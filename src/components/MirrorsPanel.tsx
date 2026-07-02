import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import {
  Plus, X, RefreshCw, AlertTriangle, FolderUp, ArrowRight, Square,
  FolderOpen, FileText, Trash2, Pause, Play, Check
} from "lucide-react";
import { IS_ANDROID } from "../util/platform";

// One-way local → remote folder mirror panel. Lives next to SFTP / Tunnels /
// CMDS inside a session. The configured mirror pairs are stored on the node
// row (server.mirrors JSON), so they roam with the encrypted profile. The
// panel lets the user (1) configure pairs, (2) preview what an initial
// sync would push, (3) start / stop the live watcher, and (4) read a
// rolling log of upload / delete / error events as the watcher fires.

type ConflictMode = "local" | "remote" | "newer";

interface MirrorSpec {
  local: string;
  remote: string;
  soft_delete: boolean;
  excludes: string[];
  // Initial-sync collision rule. "local" matches rsync's source-wins default
  // and is what most users expect for a tool literally called "mirror".
  conflict_resolution: ConflictMode;
}

interface MirrorStatus {
  id: string;
  session_id: string;
  local: string;
  remote: string;
  state: string;       // "starting" | "scanning" | "initial-sync" | "watching" | "error" | "stopped"
  queue_depth: number;
  uploaded: number;
  downloaded: number;
  deleted: number;
  scanned: number;
  transfer_total: number;
  transfer_done: number;
  last_event_ms: number;
  error?: string;
}

interface MirrorLogEntry {
  mirror_id: string;
  ts_ms: number;
  level: "info" | "warn" | "error";
  event: string;       // "start" | "upload" | "soft-delete" | "delete" | "upload-fail" | ...
  path?: string;
  message?: string;
}

interface DryRunReport {
  entries: { path: string; size: number; action: string }[];
  total_bytes: number;
}

interface MirrorsPanelProps {
  sessionId: string;
  /**
   * Saved server row id. >0 means this session is backed by a saved node
   * and ad-hoc mirrors we create here can be persisted into that node's
   * mirrors column. 0 (or undefined) is a quick-connect session — no DB
   * row to attach to, so the "save into profile" call is skipped.
   */
  serverId?: number;
  configuredMirrors: MirrorSpec[];  // from server.mirrors
  disabled?: boolean;
}

const LOG_CAP = 200;

// Shared status row used by every mirror card (saved + ad-hoc). Renders the
// state badge, transfer counters, and an inline progress bar that lights up
// during the initial-sync transfer phase.
function MirrorLiveStatus({ live }: { live: MirrorStatus }) {
  const stateBadge =
    live.state === "watching"     ? "bg-emerald-500/15 text-emerald-300" :
    live.state === "initial-sync" ? "bg-indigo-500/15 text-indigo-300"   :
    live.state === "scanning"     ? "bg-sky-500/15 text-sky-300"         :
    live.state === "error"        ? "bg-rose-500/15 text-rose-300"       :
                                    "bg-white/5 text-zinc-400";
  const pct = live.transfer_total > 0
    ? Math.round((live.transfer_done * 100) / live.transfer_total)
    : 0;
  return (
    <div className="mt-1 space-y-1">
      <div className="flex items-center gap-3 text-[10px] text-zinc-500 flex-wrap">
        <span className={`px-1.5 rounded ${stateBadge}`}>{live.state}</span>
        {live.state === "scanning" && (
          <span className="text-sky-300">scanned {live.scanned}</span>
        )}
        {live.state === "initial-sync" && live.transfer_total > 0 && (
          <span className="text-indigo-300">{live.transfer_done}/{live.transfer_total} ({pct}%)</span>
        )}
        <span>↑ {live.uploaded}</span>
        <span>↓ {live.downloaded}</span>
        <span>✗ {live.deleted}</span>
        {live.queue_depth > 0 && <span className="text-amber-300">queue {live.queue_depth}</span>}
        {live.error && <span className="text-rose-400 truncate" title={live.error}>· {live.error}</span>}
      </div>
      {live.state === "initial-sync" && live.transfer_total > 0 && (
        <div className="h-1 bg-white/5 rounded overflow-hidden">
          <div className="h-full bg-indigo-500 transition-[width] duration-150"
               style={{ width: `${pct}%` }} />
        </div>
      )}
    </div>
  );
}

const MirrorsPanel = ({ sessionId, serverId = 0, configuredMirrors, disabled = false }: MirrorsPanelProps) => {
  const [statuses, setStatuses] = useState<Record<string, MirrorStatus>>({});
  const [logs, setLogs] = useState<MirrorLogEntry[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [adding, setAdding] = useState(false);
  const [draft, setDraft] = useState<MirrorSpec>({ local: "", remote: "", soft_delete: true, excludes: [], conflict_resolution: "local" });
  const [excludesText, setExcludesText] = useState("");
  // Mirrors saved into the profile *during this session* — kept here so the
  // "Saved on this node" group lights up immediately without waiting for the
  // user to close + reopen the session for the props-level configuredMirrors
  // to refresh from DesktopApp's servers state.
  const [extraSaved, setExtraSaved] = useState<MirrorSpec[]>([]);
  // Merged view of the two saved sources, dedup-ed by (local, remote) —
  // matches the backend's coalescing rule so the UI never double-lists a
  // mirror that the user just re-saved with updated excludes / conflict mode.
  const allSavedMirrors = (() => {
    const k = (m: MirrorSpec) => `${m.local}|${m.remote}`;
    const out: MirrorSpec[] = [...configuredMirrors];
    for (const m of extraSaved) {
      const idx = out.findIndex(x => k(x) === k(m));
      if (idx >= 0) out[idx] = m; else out.push(m);
    }
    return out;
  })();
  // Map from configured-mirror index (by `local|remote` key) to the live
  // mirror id while it's running. Lets us show Start vs Stop on each row
  // without scanning statuses every render.
  const [runningByPair, setRunningByPair] = useState<Record<string, string>>({});
  // Active dry-run preview waiting for the user to confirm or cancel.
  const [pending, setPending] = useState<{ spec: MirrorSpec; report: DryRunReport } | null>(null);
  const [working, setWorking] = useState(false);
  const logScrollRef = useRef<HTMLDivElement>(null);
  const autoScrollRef = useRef(true);

  const reload = async () => {
    try {
      const list: MirrorStatus[] = await invoke("list_mirrors", { sessionId });
      const next: Record<string, MirrorStatus> = {};
      const running: Record<string, string> = {};
      for (const m of list) {
        next[m.id] = m;
        running[`${m.local}|${m.remote}`] = m.id;
      }
      setStatuses(next);
      setRunningByPair(running);
    } catch (e: any) {
      setError(String(e));
    }
  };

  useEffect(() => { reload(); /* eslint-disable-next-line */ }, [sessionId]);

  useEffect(() => {
    let unlisten: (() => void) | null = null;
    listen<MirrorStatus>(`mirror-update-${sessionId}`, (event) => {
      const s = event.payload;
      if (!s || !s.id) return;
      setStatuses((prev) => {
        if (s.state === "stopped") {
          const { [s.id]: _, ...rest } = prev;
          return rest;
        }
        return { ...prev, [s.id]: s };
      });
      if (s.state === "stopped") {
        setRunningByPair((prev) => {
          const key = `${s.local}|${s.remote}`;
          if (prev[key] !== s.id) return prev;
          const { [key]: _, ...rest } = prev;
          return rest;
        });
      } else {
        setRunningByPair((prev) => ({ ...prev, [`${s.local}|${s.remote}`]: s.id }));
      }
    }).then((fn) => { unlisten = fn; });
    return () => { if (unlisten) unlisten(); };
  }, [sessionId]);

  useEffect(() => {
    let unlisten: (() => void) | null = null;
    listen<MirrorLogEntry>(`mirror-log-${sessionId}`, (event) => {
      const entry = event.payload;
      if (!entry) return;
      setLogs((prev) => {
        const next = prev.length >= LOG_CAP ? prev.slice(prev.length - LOG_CAP + 1) : prev.slice();
        next.push(entry);
        return next;
      });
    }).then((fn) => { unlisten = fn; });
    return () => { if (unlisten) unlisten(); };
  }, [sessionId]);

  useEffect(() => {
    const el = logScrollRef.current;
    if (!el || !autoScrollRef.current) return;
    el.scrollTop = el.scrollHeight;
  }, [logs]);

  const onLogScroll = () => {
    const el = logScrollRef.current;
    if (!el) return;
    autoScrollRef.current = el.scrollHeight - el.scrollTop - el.clientHeight < 32;
  };

  const pickLocal = async () => {
    try {
      const picked: string | null = await invoke("pick_local_directory");
      if (picked) setDraft({ ...draft, local: picked });
    } catch (e: any) { setError(String(e)); }
  };

  const askPreviewThenStart = async (spec: MirrorSpec) => {
    if (working) return;
    setWorking(true); setError(null);
    try {
      // Saved mirrors written before the conflict_resolution field existed
      // come back from the vault without it. Backfill the default here so
      // the preview/start round-trip carries a value the UI can also display.
      const norm: MirrorSpec = { ...spec, conflict_resolution: spec.conflict_resolution || "local" };
      const report: DryRunReport = await invoke("mirror_dry_run", { sessionId, spec: norm });
      setPending({ spec: norm, report });
    } catch (e: any) {
      setError(String(e));
    } finally {
      setWorking(false);
    }
  };

  const confirmStart = async () => {
    if (!pending || working) return;
    setWorking(true); setError(null);
    try {
      await invoke("start_mirror", { sessionId, spec: pending.spec });
      // Persist into the saved-node row (serverId > 0 means this isn't a
      // quick-connect; backend coalesces by local|remote so re-saving an
      // existing pair just updates its excludes/conflict mode in place).
      // Persistence failure is non-fatal — the mirror is already running;
      // we just surface the error so the user knows it didn't reach the
      // profile.
      if (serverId > 0) {
        try {
          await invoke("add_mirror_to_server", { serverId, mirror: pending.spec });
          setExtraSaved(prev => {
            const k = (m: MirrorSpec) => `${m.local}|${m.remote}`;
            const without = prev.filter(m => k(m) !== k(pending.spec));
            return [...without, pending.spec];
          });
        } catch (e: any) {
          setError(`Mirror started but not saved to profile: ${e}`);
        }
      }
      // Mirror is now spawned — collapse the preview, close the ad-hoc
      // form, and reset the draft inputs. The newly-started mirror takes
      // over as a live status row in the list below, which is what the
      // user wants to see while initial sync runs.
      setPending(null);
      setAdding(false);
      setDraft({ local: "", remote: "", soft_delete: true, excludes: [], conflict_resolution: "local" });
      setExcludesText("");
    } catch (e: any) {
      setError(String(e));
    } finally {
      setWorking(false);
    }
  };

  const stopMirror = async (id: string) => {
    try { await invoke("stop_mirror", { mirrorId: id }); }
    catch (e: any) { setError(String(e)); }
  };

  const fmtBytes = (n: number) => {
    if (n < 1024) return `${n} B`;
    if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
    if (n < 1024 * 1024 * 1024) return `${(n / 1024 / 1024).toFixed(1)} MB`;
    return `${(n / 1024 / 1024 / 1024).toFixed(2)} GB`;
  };

  const submitDraft = () => {
    const local = draft.local.trim();
    const remote = draft.remote.trim();
    if (!local || !remote) { setError("Local and remote paths are required."); return; }
    const excludes = excludesText
      .split(/\r?\n/).map((s) => s.trim()).filter(Boolean);
    askPreviewThenStart({ ...draft, local, remote, excludes });
  };

  return (
    <div className="flex-1 flex flex-col h-full bg-[#09090b] overflow-hidden relative">
      {disabled && (
        <div className="absolute inset-0 z-30 bg-black/55 backdrop-blur-[1px] flex items-center justify-center text-zinc-300 text-xs font-mono uppercase">
          <span className="px-3 py-1.5 bg-red-500/15 border border-red-500/30 rounded text-red-300">
            Session disconnected
          </span>
        </div>
      )}

      {/* Header */}
      <div className="shrink-0 px-3 py-2 flex items-center justify-between border-b border-white/5 bg-[#121214]">
        <div className="flex items-center gap-2 text-[10px] font-bold uppercase tracking-widest text-zinc-400">
          <FolderUp size={12} />
          <span>{Object.keys(statuses).length} running · {allSavedMirrors.length} saved</span>
        </div>
        <button
          onClick={() => { setAdding((a) => !a); setError(null); }}
          className="h-6 px-2.5 rounded text-[10px] font-bold uppercase tracking-wider bg-primary/15 text-primary border border-primary/30 hover:bg-primary/25 flex items-center gap-1.5"
        >
          {adding ? <X size={11} /> : <Plus size={11} />}
          <span>{adding ? "Cancel" : "New mirror"}</span>
        </button>
      </div>

      {/* Ad-hoc new mirror form */}
      {adding && !pending && (
        <div className="shrink-0 p-3 border-b border-white/5 bg-[#0f0f12] space-y-2">
          <div className="flex items-center gap-2">
            {/* Native folder picker is desktop-only (rfd). On Android the
                backend command errors, so we hide the button and lean on
                the text input; the placeholder guides users to a sensible
                writable location on external storage. Platform detection
                lives in util/platform to keep one source of truth. */}
            {!IS_ANDROID && (
              <button
                onClick={pickLocal}
                className="h-7 px-2 rounded bg-white/[0.04] border border-white/10 hover:bg-white/10 text-zinc-300 text-[11px] flex items-center gap-1.5 shrink-0"
                title="Pick a local directory"
              >
                <FolderOpen size={12} /> Browse
              </button>
            )}
            <input
              value={draft.local}
              onChange={(e) => setDraft({ ...draft, local: e.target.value })}
              placeholder={IS_ANDROID
                ? "/storage/emulated/0/… (type path)"
                : "/local/path (or browse)"}
              className="flex-1 min-w-0 h-7 px-2 bg-white/[0.04] border border-white/10 rounded text-[11px] font-mono text-zinc-200"
            />
          </div>
          <div className="flex items-center gap-2">
            <ArrowRight size={12} className="text-zinc-500 shrink-0 ml-[58px]" />
            <input
              value={draft.remote}
              onChange={(e) => setDraft({ ...draft, remote: e.target.value })}
              placeholder="/remote/path on server"
              className="flex-1 min-w-0 h-7 px-2 bg-white/[0.04] border border-white/10 rounded text-[11px] font-mono text-zinc-200"
            />
          </div>
          <textarea
            value={excludesText}
            onChange={(e) => setExcludesText(e.target.value)}
            placeholder="Excludes (one per line) — substring or *.ext"
            rows={2}
            className="w-full p-2 bg-white/[0.04] border border-white/10 rounded text-[10.5px] font-mono text-zinc-300 placeholder-zinc-600 resize-none focus:outline-none focus:border-primary/40"
          />
          <div className="flex items-center gap-2 text-[10.5px] text-zinc-400">
            <span className="shrink-0 uppercase tracking-wider font-bold text-zinc-500 text-[9.5px]">On conflict</span>
            <select
              value={draft.conflict_resolution}
              onChange={(e) => setDraft({ ...draft, conflict_resolution: e.target.value as ConflictMode })}
              className="h-7 px-2 bg-white/[0.04] border border-white/10 rounded text-[11px] text-zinc-200 outline-none focus:border-primary/40"
              title="Decides which side wins when a file exists on both sides with different content. Missing-on-one-side files always copy across regardless."
            >
              <option value="local">Local wins (overwrite remote)</option>
              <option value="remote">Remote wins (overwrite local)</option>
              <option value="newer">Newer mtime wins</option>
            </select>
          </div>
          <div className="flex items-center justify-between gap-2">
            <label className="flex items-center gap-2 text-[10.5px] text-zinc-400 select-none">
              <input
                type="checkbox"
                checked={draft.soft_delete}
                onChange={(e) => setDraft({ ...draft, soft_delete: e.target.checked })}
                className="accent-primary"
              />
              <span>Soft delete (move to <span className="font-mono text-zinc-300">.submarine-trash</span> instead of removing)</span>
            </label>
            <button
              onClick={submitDraft}
              disabled={working}
              className="h-7 px-3 rounded text-[10px] font-bold uppercase tracking-wider bg-primary/15 text-primary border border-primary/30 hover:bg-primary/25 disabled:opacity-50"
            >
              {working ? "…" : "Continue"}
            </button>
          </div>
          {error && (
            <div className="text-[10.5px] text-rose-400 font-mono flex items-start gap-1.5">
              <AlertTriangle size={11} className="mt-0.5 shrink-0" />
              <span className="break-words">{error}</span>
            </div>
          )}
        </div>
      )}

      {/* Dry-run confirmation */}
      {pending && (() => {
        // Count download-modified actions — those are the risky ones. A
        // "download-new" only writes a fresh path locally so it can't clobber
        // existing work; "download-modified" overwrites a local file whose
        // content the user might be in the middle of editing. We use this
        // count to decide whether to escalate the warning copy.
        const downloadModified = pending.report.entries.filter(
          e => e.action === "download-modified"
        ).length;
        const conflictMode = pending.spec.conflict_resolution || "local";
        return (
        <div className="shrink-0 p-3 border-b border-white/5 bg-[#0f0f12] space-y-2">
          <div className="flex items-start gap-2 text-[11px] text-zinc-300">
            <FolderUp size={13} className="text-primary mt-0.5 shrink-0" />
            <div className="min-w-0 flex-1">
              <div className="font-bold">Initial sync preview</div>
              <div className="text-zinc-500 text-[10.5px] font-mono truncate">
                {pending.spec.local} → {pending.spec.remote}
              </div>
            </div>
          </div>

          {/* Overwrite-risk advisory. Always shown when there's any work to
              do, escalated to amber when the plan includes download-modified
              entries — those overwrite a local file the user might still be
              editing, and the engine has no re-compare guard between scan and
              transfer. The user explicitly opted for a warning-only approach
              over re-engineering the reconciliation flow, so this is the
              single point of consent for the data-loss window. */}
          {pending.report.entries.length > 0 && (
            <div className={`flex items-start gap-2 text-[10.5px] px-2.5 py-1.5 rounded border ${
              downloadModified > 0
                ? "bg-amber-500/10 border-amber-500/30 text-amber-200"
                : "bg-sky-500/5 border-sky-500/20 text-sky-200"
            }`}>
              <AlertTriangle size={12} className="mt-0.5 shrink-0" />
              <div className="min-w-0 leading-relaxed">
                <span className="font-bold uppercase tracking-wider mr-1">Heads up</span>
                {downloadModified > 0 ? (
                  <>
                    {downloadModified} file{downloadModified === 1 ? "" : "s"} marked <span className="font-mono">↓ mod</span> will be <b>overwritten locally</b> with the remote copy.
                    {" "}If you edit any of these during the sync, your changes can be lost silently — there's no re-compare between scan and transfer. Save / close editors first, or change conflict mode (currently <span className="font-mono">{conflictMode}</span>).
                  </>
                ) : (
                  <>
                    Initial sync is one decision per file based on this scan. If you edit a file locally <b>before the transfer phase finishes</b>, the change may be silently overwritten by the remote copy. Best to pause edits until the live watcher kicks in.
                  </>
                )}
              </div>
            </div>
          )}

          <div className="text-[11px] text-zinc-300 bg-black/40 border border-white/5 rounded p-2 max-h-40 overflow-auto custom-scrollbar font-mono">
            {pending.report.entries.length === 0 ? (
              <div className="text-zinc-500 italic">Already in sync — nothing to move in either direction.</div>
            ) : (
              pending.report.entries.slice(0, 200).map((e, i) => {
                const isDown = e.action.startsWith("download-");
                const isNew = e.action.endsWith("-new");
                const tone = isDown
                  ? (isNew ? "bg-sky-500/15 text-sky-300"      : "bg-violet-500/15 text-violet-300")
                  : (isNew ? "bg-emerald-500/15 text-emerald-300" : "bg-amber-500/15 text-amber-300");
                const label = `${isDown ? "↓" : "↑"} ${isNew ? "new" : "mod"}`;
                return (
                  <div key={i} className="flex items-center gap-2 text-[10.5px]">
                    <span className={`px-1 rounded text-[9px] font-bold uppercase shrink-0 ${tone}`}>{label}</span>
                    <span className="truncate flex-1 text-zinc-300">{e.path}</span>
                    <span className="text-zinc-500 shrink-0">{fmtBytes(e.size)}</span>
                  </div>
                );
              })
            )}
            {pending.report.entries.length > 200 && (
              <div className="text-zinc-500 text-[10px] italic mt-1">… {pending.report.entries.length - 200} more</div>
            )}
          </div>
          <div className="flex items-center justify-between text-[11px]">
            <span className="text-zinc-400">
              {pending.report.entries.length} files · {fmtBytes(pending.report.total_bytes)}
            </span>
            <div className="flex gap-2">
              <button
                onClick={() => setPending(null)}
                className="h-7 px-3 rounded text-[10px] font-bold uppercase tracking-wider bg-white/[0.04] border border-white/10 text-zinc-300 hover:bg-white/10"
              >
                Cancel
              </button>
              <button
                onClick={confirmStart}
                disabled={working}
                className="h-7 px-3 rounded text-[10px] font-bold uppercase tracking-wider bg-primary text-black border border-primary disabled:opacity-50 flex items-center gap-1.5"
              >
                <Check size={11} /> Start mirror
              </button>
            </div>
          </div>
        </div>
        );
      })()}

      {/* Configured + running list */}
      <div className="flex-1 overflow-auto p-2 space-y-1.5">
        {/* Configured mirrors stored on the node — Start button picks up
            saved config and confirms via the dry-run preview. */}
        {allSavedMirrors.length > 0 && (
          <div className="text-[10px] font-bold uppercase tracking-widest text-zinc-500 px-1 pt-1">Saved on this node</div>
        )}
        {allSavedMirrors.map((m, i) => {
          const key = `${m.local}|${m.remote}`;
          const liveId = runningByPair[key];
          const live = liveId ? statuses[liveId] : null;
          return (
            <div key={`saved-${i}`} className="rounded border border-white/5 bg-white/[0.02] p-2 font-mono text-[11px]">
              <div className="flex items-center gap-2">
                <FolderUp size={12} className="text-primary shrink-0" />
                <span className="text-zinc-300 truncate flex-1">{m.local}</span>
                <ArrowRight size={11} className="text-zinc-500 shrink-0" />
                <span className="text-zinc-400 truncate flex-1">{m.remote}</span>
                {live ? (
                  <button
                    onClick={() => stopMirror(live.id)}
                    title="Stop mirror"
                    className="p-0.5 rounded hover:bg-white/10 text-zinc-400 hover:text-rose-400 shrink-0"
                  >
                    <Square size={11} />
                  </button>
                ) : (
                  <button
                    onClick={() => askPreviewThenStart(m)}
                    title="Start mirror"
                    className="p-0.5 rounded hover:bg-white/10 text-zinc-400 hover:text-emerald-400 shrink-0"
                  >
                    <Play size={11} fill="currentColor" />
                  </button>
                )}
              </div>
              {live && <MirrorLiveStatus live={live} />}
            </div>
          );
        })}

        {/* Ad-hoc / live mirrors that aren't in the saved list. */}
        {Object.values(statuses).filter((s) => !allSavedMirrors.some((m) => m.local === s.local && m.remote === s.remote)).length > 0 && (
          <div className="text-[10px] font-bold uppercase tracking-widest text-zinc-500 px-1 pt-2">Ad-hoc this session</div>
        )}
        {Object.values(statuses)
          .filter((s) => !allSavedMirrors.some((m) => m.local === s.local && m.remote === s.remote))
          .map((live) => (
            <div key={live.id} className="rounded border border-white/5 bg-white/[0.02] p-2 font-mono text-[11px]">
              <div className="flex items-center gap-2">
                <FolderUp size={12} className="text-primary shrink-0" />
                <span className="text-zinc-300 truncate flex-1">{live.local}</span>
                <ArrowRight size={11} className="text-zinc-500 shrink-0" />
                <span className="text-zinc-400 truncate flex-1">{live.remote}</span>
                <button
                  onClick={() => stopMirror(live.id)}
                  className="p-0.5 rounded hover:bg-white/10 text-zinc-400 hover:text-rose-400 shrink-0"
                >
                  <Square size={11} />
                </button>
              </div>
              <MirrorLiveStatus live={live} />
            </div>
          ))}

        {allSavedMirrors.length === 0 && Object.keys(statuses).length === 0 && (
          <div className="text-center py-12 text-zinc-500 text-[11px] font-mono">
            No mirrors yet.<br />
            <span className="text-zinc-600 text-[10px]">Add one with “New mirror”, or define them on the node form.</span>
          </div>
        )}
      </div>

      {/* Log feed */}
      <div className="shrink-0 border-t border-white/5 bg-[#0c0c0e]">
        <div className="px-3 py-1.5 flex items-center justify-between border-b border-white/5">
          <span className="text-[10px] font-bold uppercase tracking-widest text-zinc-400 flex items-center gap-1.5">
            <FileText size={11} /> Log <span className="text-zinc-600 font-normal">({logs.length})</span>
          </span>
          <button
            onClick={() => setLogs([])}
            title="Clear log"
            className="p-1 text-zinc-500 hover:text-rose-400 rounded hover:bg-white/5"
          >
            <Trash2 size={11} />
          </button>
        </div>
        <div
          ref={logScrollRef}
          onScroll={onLogScroll}
          className="h-36 overflow-auto px-2 py-1 font-mono text-[10.5px] leading-relaxed space-y-0.5 select-text cursor-text"
        >
          {logs.length === 0 ? (
            <div className="text-zinc-600 text-center py-4">No log entries yet.</div>
          ) : (
            logs.map((l, i) => {
              const ts = new Date(l.ts_ms);
              const hh = String(ts.getHours()).padStart(2, "0");
              const mm = String(ts.getMinutes()).padStart(2, "0");
              const ss = String(ts.getSeconds()).padStart(2, "0");
              const tone =
                l.level === "error" ? "text-rose-300" :
                l.level === "warn"  ? "text-amber-300" :
                                      "text-zinc-400";
              const eventTone =
                l.event === "upload"      ? "bg-emerald-500/15 text-emerald-300" :
                l.event === "download"    ? "bg-sky-500/15 text-sky-300" :
                l.event === "soft-delete" || l.event === "delete"
                                          ? "bg-amber-500/15 text-amber-300" :
                l.event.endsWith("-fail") ? "bg-rose-500/15 text-rose-300" :
                                            "bg-white/5 text-zinc-400";
              return (
                <div key={i} className="flex items-start gap-1.5">
                  <span className="text-zinc-600 shrink-0">{hh}:{mm}:{ss}</span>
                  <span className={`px-1 rounded text-[9px] uppercase font-bold shrink-0 ${eventTone}`}>{l.event}</span>
                  <span className={`flex-1 break-all ${tone}`}>
                    {l.path && <span className="text-zinc-200">{l.path}</span>}
                    {l.message && (
                      <span className={l.path ? "text-zinc-500 ml-1" : ""}>
                        {l.path ? "· " : ""}{l.message}
                      </span>
                    )}
                  </span>
                </div>
              );
            })
          )}
        </div>
      </div>
    </div>
  );
};

export default MirrorsPanel;
