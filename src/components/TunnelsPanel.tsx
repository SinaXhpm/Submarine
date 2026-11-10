import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { Plus, X, Activity, AlertTriangle, Globe, ArrowRight, ArrowDownLeft, Square, Play, Pencil, FileText, Trash2 } from "lucide-react";
import { useConfirm } from "../ui/confirm";
import { serverRowChanged } from "../util/serverJson";

// Tunnels panel — the session's port-forwarding rules, running and stopped,
// with start / stop / rename / remove. A node's saved rules start at connect
// time (see initiate_connection), except the ones the user stopped: Stop keeps
// a rule, Remove deletes it.

// One forwarding rule, as the backend keeps it (tunnel::TunnelSpec).
interface TunnelRule {
  type: "D" | "L" | "R";
  local: string;
  remote: string;
  name?: string | null;
  stopped?: boolean;
}

interface TunnelStatus {
  id: string;
  session_id: string;
  kind: string;        // "dynamic" | "local" | "remote"
  listen_addr: string;
  target: string;
  state: string;       // "starting" | "listening" | "error" | "closed"
  error?: string;
  conns_total: number;
  conns_active: number;
  bytes_in: number;
  bytes_out: number;
  spec?: TunnelRule;   // the rule it was started from
}

interface TunnelLogEntry {
  tunnel_id: string;
  ts_ms: number;
  level: "info" | "warn" | "error";
  event: string;        // "listen" | "connect" | "close" | "fail" | "stop" | "shutdown"
  target?: string;
  peer?: string;
  message?: string;
}

// Cap the rolling log buffer so a chatty SOCKS proxy doesn't grow the array
// unbounded across a long session. 200 is enough to scroll back through the
// last few minutes of activity on a busy tunnel without burning React render
// time on a 10k-entry list.
const LOG_CAP = 200;

// The longest name a forward keeps (the backend trims to the same).
const NAME_MAX = 64;

interface TunnelsPanelProps {
  sessionId: string;
  // The node this session belongs to. When present (> 0), rule changes are
  // persisted back to the node's saved rules so they survive to the next
  // open. Absent/0 for quick-connect sessions with no saved node.
  serverId?: number;
  disabled?: boolean;
}

const KIND_LABEL: Record<TunnelRule["type"], string> = {
  D: "dynamic", L: "local", R: "remote",
};

// The same forward: kind, local side and target match (a name or a stop
// doesn't make it another one), as in the backend.
const sameForward = (a: TunnelRule, b: TunnelRule) =>
  a.type === b.type && a.local === b.local && (a.remote || "") === (b.remote || "");

const ruleKey = (r: TunnelRule) => `${r.type}|${r.local}|${r.remote || ""}`;

const isRunning = (t?: TunnelStatus) => !!t && (t.state === "listening" || t.state === "starting");

const TunnelsPanel = ({ sessionId, serverId, disabled = false }: TunnelsPanelProps) => {
  const [tunnels, setTunnels] = useState<Record<string, TunnelStatus>>({});
  const [rules, setRules] = useState<TunnelRule[]>([]);
  const rulesRef = useRef<TunnelRule[]>([]);
  const [adding, setAdding] = useState(false);
  const [draft, setDraft] = useState<{ type: "D" | "L" | "R"; local: string; remote: string; name: string }>({
    type: "D", local: "1080", remote: "", name: "",
  });
  const [busy, setBusy] = useState(false);
  // Read by handlers that can run twice before a render (a double click).
  const busyRef = useRef(false);
  const [error, setError] = useState<string | null>(null);
  // The rule whose name is being edited, and the text typed so far. The ref
  // is the live copy: Enter and the blur that follows must save once, and
  // Esc must not save at all.
  const [renaming, setRenamingState] = useState<{ key: string; value: string } | null>(null);
  const renamingRef = useRef<{ key: string; value: string } | null>(null);
  const setRenaming = (next: { key: string; value: string } | null) => {
    renamingRef.current = next;
    setRenamingState(next);
  };
  const [logs, setLogs] = useState<TunnelLogEntry[]>([]);
  const [logsOpen, setLogsOpen] = useState(true);
  const [logFilter, setLogFilter] = useState<string | null>(null); // tunnel id, null = all
  const logScrollRef = useRef<HTMLDivElement>(null);
  const autoScrollRef = useRef(true);
  const confirmDialog = useConfirm();

  // The backend takes the node's id to start from its saved rules when the
  // session has no list of its own yet; none for a quick-connect session.
  const nodeId = serverId && serverId > 0 ? serverId : null;

  const reloadRules = async () => {
    try {
      const list: TunnelRule[] = await invoke("list_tunnel_rules", { sessionId, serverId: nodeId });
      rulesRef.current = list;
      setRules(list);
    } catch (e: any) {
      setError(String(e));
    }
  };

  // Initial snapshot
  const reload = async () => {
    try {
      const list: TunnelStatus[] = await invoke("list_tunnels", { sessionId });
      const next: Record<string, TunnelStatus> = {};
      for (const t of list) next[t.id] = t;
      setTunnels(next);
    } catch (e: any) {
      setError(String(e));
    }
    await reloadRules();
  };

  useEffect(() => {
    reload();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [sessionId, nodeId]);

  // Live updates: every status transition (starting → listening, new
  // connection counted, error, closed) fires a per-session event.
  useEffect(() => {
    let unlisten: (() => void) | null = null;
    listen<TunnelStatus>(`tunnel-update-${sessionId}`, (event) => {
      const t = event.payload;
      if (!t || !t.id) return;
      setTunnels((prev) => {
        if (t.state === "closed") {
          const { [t.id]: _, ...rest } = prev;
          return rest;
        }
        const next = { ...prev, [t.id]: t };
        // A forward started again replaces what was left of its last run.
        if (t.spec && isRunning(t)) {
          for (const [id, other] of Object.entries(next)) {
            if (id !== t.id && other.spec && sameForward(other.spec, t.spec) && !isRunning(other)) {
              delete next[id];
            }
          }
        }
        return next;
      });
      // A forward with no rule here: the rules changed somewhere else (the
      // node was edited, say), so fetch them again.
      if (t.spec && !rulesRef.current.some((r) => sameForward(r, t.spec!))) reloadRules();
    }).then((fn) => { unlisten = fn; });
    return () => { if (unlisten) unlisten(); };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [sessionId, nodeId]);

  // Connection log: per-session stream of accepted / refused / closed
  // connections. Kept in a rolling buffer so the panel scrolls back through
  // recent activity without unbounded memory growth.
  useEffect(() => {
    let unlisten: (() => void) | null = null;
    listen<TunnelLogEntry>(`tunnel-log-${sessionId}`, (event) => {
      const entry = event.payload;
      if (!entry || !entry.tunnel_id) return;
      setLogs((prev) => {
        const next = prev.length >= LOG_CAP ? prev.slice(prev.length - LOG_CAP + 1) : prev.slice();
        next.push(entry);
        return next;
      });
    }).then((fn) => { unlisten = fn; });
    return () => { if (unlisten) unlisten(); };
  }, [sessionId]);

  // Auto-scroll to bottom when new logs come in — but only if the user
  // hasn't scrolled up to read older entries. Detect "user at bottom" by
  // checking the scroll position against the scroll height before pushing
  // the autoscroll.
  useEffect(() => {
    const el = logScrollRef.current;
    if (!el || !autoScrollRef.current) return;
    el.scrollTop = el.scrollHeight;
  }, [logs]);

  const onLogScroll = () => {
    const el = logScrollRef.current;
    if (!el) return;
    const dist = el.scrollHeight - el.scrollTop - el.clientHeight;
    autoScrollRef.current = dist < 32;
  };

  // Save the session's rules back to the node so changes survive to the next
  // open. No-op for quick-connect (no node); best-effort (a save failure must
  // never block the action the user just took).
  const persistTunnels = () => {
    if (!nodeId) return;
    invoke("persist_session_tunnels", { sessionId, serverId: nodeId }).then(serverRowChanged).catch(() => {});
  };

  // Run one change to the rules: no two at once, the error (if any) shown,
  // and the rules saved and listed again afterwards either way.
  const change = async (action: () => Promise<unknown>) => {
    if (busyRef.current) return false;
    busyRef.current = true;
    setBusy(true); setError(null);
    let ok = true;
    try {
      await action();
    } catch (e: any) {
      setError(String(e));
      ok = false;
    } finally {
      persistTunnels();
      await reloadRules();
      busyRef.current = false;
      setBusy(false);
    }
    return ok;
  };

  const addTunnel = async () => {
    // Catch obvious port mistakes (0, 65536+, non-numeric tail) in the UI so
    // we don't roundtrip to Rust just to bind() and fail. Accepts bare port
    // ("8080") or host:port ("0.0.0.0:8080") for L/R; D always wants a bare
    // port. Targets are intentionally NOT pre-resolved — DNS is the backend's
    // problem.
    const localStr = draft.local.trim();
    const portPart = localStr.includes(":") ? localStr.slice(localStr.lastIndexOf(":") + 1) : localStr;
    const portNum = Number(portPart);
    if (!Number.isInteger(portNum) || portNum < 1 || portNum > 65535) {
      setError(`Listen port must be between 1 and 65535 (got "${portPart}")`);
      return;
    }
    if (draft.type !== "D") {
      const target = draft.remote.trim();
      const colonIdx = target.lastIndexOf(":");
      const targetPort = colonIdx > 0 ? Number(target.slice(colonIdx + 1)) : NaN;
      if (!target || colonIdx <= 0 || !Number.isInteger(targetPort) || targetPort < 1 || targetPort > 65535) {
        setError(`Target must be host:port with a valid port (got "${target}")`);
        return;
      }
    }
    const ok = await change(() => invoke("start_tunnel", {
      sessionId,
      serverId: nodeId,
      spec: { type: draft.type, local: draft.local, remote: draft.remote, name: draft.name.trim() || null },
    }));
    if (ok) {
      setAdding(false);
      setDraft({ type: "D", local: "1080", remote: "", name: "" });
    }
  };

  const startRule = (rule: TunnelRule) =>
    change(() => invoke("start_tunnel", { sessionId, serverId: nodeId, spec: { ...rule, stopped: false } }));

  const stopTunnel = (t: TunnelStatus) =>
    change(() => invoke("stop_tunnel", { tunnelId: t.id, serverId: nodeId }));

  const removeRule = async (rule: TunnelRule) => {
    const what = rule.name ? `“${rule.name}”` : `${KIND_LABEL[rule.type]} ${rule.local}`;
    const ok = await confirmDialog({
      title: "Remove forward",
      message: nodeId
        ? `Remove ${what}? It is deleted from this server's saved forwards.`
        : `Remove ${what}?`,
      okLabel: "Remove",
      destructive: true,
    });
    if (!ok) return;
    await change(() => invoke("remove_tunnel_rule", { sessionId, serverId: nodeId, spec: rule }));
  };

  const finishRename = async (rule: TunnelRule) => {
    const edit = renamingRef.current;
    setRenaming(null);
    if (!edit || edit.key !== ruleKey(rule) || edit.value.trim() === (rule.name || "")) return;
    await change(() => invoke("rename_tunnel_rule", { sessionId, serverId: nodeId, spec: rule, name: edit.value }));
  };

  // A row per rule, with the tunnel running for it (or what is left of its
  // last run: an error). Tunnels with no rule here (the rules changed
  // somewhere else) get a row of their own, so they can still be stopped.
  const liveFor = (rule: TunnelRule): TunnelStatus | undefined => {
    const runs = Object.values(tunnels).filter((t) => t.spec && sameForward(t.spec, rule));
    return runs.find(isRunning) ?? runs[0];
  };
  const rows = rules.map((rule) => ({ rule, live: liveFor(rule) }));
  const strays = Object.values(tunnels).filter((t) => !t.spec || !rules.some((r) => sameForward(r, t.spec!)));
  const runningCount = rows.filter((r) => isRunning(r.live)).length + strays.filter(isRunning).length;
  const total = rows.length + strays.length;

  const renderRow = (rule: TunnelRule | null, live: TunnelStatus | undefined) => {
    const running = isRunning(live);
    const state = live && (running || live.state === "error") ? live.state : rule?.stopped ? "stopped" : "not running";
    const stateTone =
      state === "listening" ? "border-emerald-500/30 bg-emerald-950/30 text-emerald-300" :
      state === "starting"  ? "border-indigo-500/30 bg-indigo-950/30 text-indigo-300" :
      state === "error"     ? "border-rose-500/30 bg-rose-950/30 text-rose-300" :
                              "border-white/10 bg-white/[0.03] text-zinc-400";
    const type = rule?.type ?? (live?.kind === "dynamic" ? "D" : live?.kind === "remote" ? "R" : "L");
    const KindIcon = type === "D" ? Globe : type === "R" ? ArrowDownLeft : ArrowRight;
    const key = rule ? ruleKey(rule) : `live-${live!.id}`;
    const isRenaming = !!rule && renaming?.key === key;
    // Where it listens and where to: as resolved while it runs, as typed otherwise.
    const listenText = live && running ? live.listen_addr : rule?.local ?? live?.listen_addr ?? "";
    const targetText = live && running ? live.target : rule?.remote ?? live?.target ?? "";
    const iconButton = "p-0.5 rounded hover:bg-white/10 text-zinc-400 shrink-0 disabled:opacity-40";
    return (
      <div key={key} className={`rounded border ${stateTone} p-2 font-mono text-[11px]`}>
        <div className="flex items-center gap-2 min-w-0">
          <KindIcon size={12} className="shrink-0" />
          {isRenaming ? (
            <input
              autoFocus
              value={renaming!.value}
              maxLength={NAME_MAX}
              placeholder="Name"
              aria-label="Forward name"
              onChange={(e) => setRenaming({ key, value: e.target.value })}
              onKeyDown={(e) => {
                if (e.key === "Enter") finishRename(rule!);
                else if (e.key === "Escape") setRenaming(null);
              }}
              onBlur={() => finishRename(rule!)}
              className="flex-1 min-w-0 h-5 px-1.5 bg-white/[0.06] border border-white/15 rounded text-[11px] text-zinc-100 focus:outline-none focus:border-primary/50"
            />
          ) : rule?.name ? (
            <span className="flex-1 min-w-0 truncate font-bold text-zinc-100" title={rule.name}>{rule.name}</span>
          ) : (
            <span className="flex-1 min-w-0 truncate font-bold uppercase tracking-wider text-[10px] opacity-80">{KIND_LABEL[type]}</span>
          )}
          {live && (
            <button
              onClick={() => setLogFilter(logFilter === live.id ? null : live.id)}
              title={logFilter === live.id ? "Show all tunnels in log" : "Filter log to this tunnel"}
              aria-label="Filter log"
              className={`p-0.5 rounded shrink-0 ${
                logFilter === live.id
                  ? "bg-primary/20 text-primary"
                  : "text-zinc-400 hover:text-primary hover:bg-white/10"
              }`}
            >
              <FileText size={11} />
            </button>
          )}
          {running ? (
            <button onClick={() => stopTunnel(live!)} disabled={busy} title="Stop (the forward stays in the list)" aria-label="Stop" className={`${iconButton} hover:text-amber-300`}>
              <Square size={11} />
            </button>
          ) : rule ? (
            <button onClick={() => startRule(rule)} disabled={busy} title="Start" aria-label="Start" className={`${iconButton} hover:text-emerald-300`}>
              <Play size={11} fill="currentColor" />
            </button>
          ) : null}
          {rule && (
            <button onClick={() => setRenaming({ key, value: rule.name || "" })} disabled={busy} title="Name this forward" aria-label="Rename" className={`${iconButton} hover:text-zinc-100`}>
              <Pencil size={11} />
            </button>
          )}
          {rule && (
            <button onClick={() => removeRule(rule)} disabled={busy} title="Remove" aria-label="Remove" className={`${iconButton} hover:text-rose-400`}>
              <Trash2 size={11} />
            </button>
          )}
        </div>
        <div className="mt-1 flex items-center gap-2 min-w-0 text-zinc-300">
          <span className={`truncate ${type === "D" ? "" : "flex-1"}`} title={listenText}>{listenText}</span>
          {type !== "D" ? (
            <>
              <ArrowRight size={11} className="text-zinc-500 shrink-0" />
              <span className="text-zinc-400 truncate flex-1" title={targetText}>{targetText}</span>
            </>
          ) : (
            <span className="text-zinc-500 text-[10px]">SOCKS</span>
          )}
        </div>
        <div className="mt-1 flex items-center gap-3 text-[10px] text-zinc-500 min-w-0">
          <span className={`px-1.5 rounded shrink-0 ${
            state === "listening" ? "bg-emerald-500/15 text-emerald-300" :
            state === "starting"  ? "bg-indigo-500/15 text-indigo-300" :
            state === "error"     ? "bg-rose-500/15 text-rose-300" :
            state === "not running" ? "bg-amber-500/15 text-amber-300" :
                                      "bg-white/5 text-zinc-400"
          }`}
            title={
              state === "stopped" ? "Kept, but not started on connect until you start it" :
              state === "not running" ? "It starts with the connection; Start tries now" : undefined
            }
          >
            {state}
          </span>
          {live && running && (
            <span title="connections currently bridged / total accepted since start">
              <span className={live.conns_active > 0 ? "text-emerald-300" : "text-zinc-500"}>
                {live.conns_active}
              </span>
              <span className="opacity-50"> active</span>
              <span className="opacity-30"> · </span>
              <span>{live.conns_total} total</span>
            </span>
          )}
          {live?.error && state === "error" && (
            <span className="text-rose-400 truncate" title={live.error}>{live.error}</span>
          )}
        </div>
      </div>
    );
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

      {/* Header: counts + add button */}
      <div className="shrink-0 px-3 py-2 flex items-center justify-between gap-2 border-b border-white/5 bg-[#121214]">
        <div className="flex items-center gap-2 min-w-0 text-[10px] font-bold uppercase tracking-widest text-zinc-400">
          <Activity size={12} className="shrink-0" />
          <span className="whitespace-nowrap">{total} {total === 1 ? "forward" : "forwards"}</span>
          {total > 0 && <span className="text-zinc-600 font-normal whitespace-nowrap truncate">· {runningCount} running</span>}
        </div>
        <button
          onClick={() => { setAdding((a) => !a); setError(null); }}
          className="h-6 px-2.5 rounded text-[10px] font-bold uppercase tracking-wider bg-primary/15 text-primary border border-primary/30 hover:bg-primary/25 flex items-center gap-1.5 shrink-0"
        >
          {adding ? <X size={11} /> : <Plus size={11} />}
          <span>{adding ? "Cancel" : "Add"}</span>
        </button>
      </div>

      {/* Add form — fields stacked top-to-bottom so the 1/3-width tool pane
          doesn't squeeze both inputs into illegible slivers. */}
      {adding && (
        <div className="shrink-0 p-3 border-b border-white/5 bg-[#0f0f12] space-y-2">
          <input
            value={draft.name}
            onChange={(e) => setDraft({ ...draft, name: e.target.value })}
            maxLength={NAME_MAX}
            placeholder="Name (optional)"
            aria-label="Forward name"
            className="w-full h-7 px-2 bg-white/[0.04] border border-white/10 rounded text-[11px] text-zinc-200"
          />
          <div className="flex items-center gap-2">
            <select
              value={draft.type}
              onChange={(e) => setDraft({ ...draft, type: e.target.value as "D" | "L" | "R" })}
              className="h-7 px-1.5 bg-white/[0.04] border border-white/10 rounded text-[11px] text-primary font-bold uppercase shrink-0"
            >
              <option value="D" className="bg-[#1a1a1e]">Dynamic</option>
              <option value="L" className="bg-[#1a1a1e]">Local</option>
              <option value="R" className="bg-[#1a1a1e]">Remote</option>
            </select>
            <input
              value={draft.local}
              onChange={(e) => setDraft({ ...draft, local: e.target.value })}
              placeholder={
                draft.type === "D" ? "Listen port (1080)" :
                draft.type === "L" ? "Local addr (8080 or 0.0.0.0:8080)" :
                                      "Server bind (8080 or 0.0.0.0:8080)"
              }
              className="flex-1 min-w-0 h-7 px-2 bg-white/[0.04] border border-white/10 rounded text-[11px] font-mono text-zinc-200"
            />
          </div>
          {draft.type !== "D" && (
            <div className="flex items-center gap-2 pl-1">
              <ArrowRight size={12} className="text-zinc-500 shrink-0" />
              <input
                value={draft.remote}
                onChange={(e) => setDraft({ ...draft, remote: e.target.value })}
                placeholder={draft.type === "L" ? "Target host:port (eg intranet:80)" : "Local target host:port"}
                className="flex-1 min-w-0 h-7 px-2 bg-white/[0.04] border border-white/10 rounded text-[11px] font-mono text-zinc-200"
              />
            </div>
          )}
          <div className="flex items-start justify-between gap-2">
            <span className="text-[10px] text-zinc-500 font-mono flex-1 leading-snug">
              {draft.type === "D" && "SOCKS5 proxy on this machine; traffic exits via the server."}
              {draft.type === "L" && "Connections to your local addr are tunneled to the target through the server."}
              {draft.type === "R" && "Server listens on bind addr (defaults to 0.0.0.0); incoming connections are forwarded back here. Needs GatewayPorts in sshd_config to listen externally."}
            </span>
            <button
              onClick={addTunnel}
              disabled={busy}
              className="h-7 px-3 rounded text-[10px] font-bold uppercase tracking-wider bg-primary/15 text-primary border border-primary/30 hover:bg-primary/25 disabled:opacity-50 shrink-0"
            >
              {busy ? "…" : "Start"}
            </button>
          </div>
        </div>
      )}

      {error && (
        <div className="shrink-0 px-3 py-1.5 border-b border-white/5 bg-rose-950/20 text-[10.5px] text-rose-400 font-mono flex items-start gap-1.5" role="alert">
          <AlertTriangle size={11} className="mt-0.5 shrink-0" />
          <span className="break-words flex-1 min-w-0">{error}</span>
          <button onClick={() => setError(null)} aria-label="Dismiss" className="shrink-0 text-rose-300/70 hover:text-rose-200">
            <X size={11} />
          </button>
        </div>
      )}

      {/* List */}
      <div className="flex-1 overflow-auto p-2 space-y-1.5">
        {total === 0 ? (
          <div className="text-center py-12 text-zinc-500 text-[11px] font-mono">
            No port forwards.<br />
            <span className="text-zinc-600 text-[10px]">Add one above, or define rules on the node.</span>
          </div>
        ) : (
          <>
            {rows.map(({ rule, live }) => renderRow(rule, live))}
            {strays.map((t) => renderRow(null, t))}
          </>
        )}
      </div>

      {/* Activity log — rolling feed of connect / close / fail events across
          all tunnels in this session, or filtered to one via the row icon.
          Auto-scrolls to bottom unless the user has scrolled up to read
          older entries. */}
      <div className="shrink-0 border-t border-white/5 bg-[#0c0c0e]">
        <div className="px-3 py-1.5 flex items-center justify-between border-b border-white/5">
          <button
            onClick={() => setLogsOpen((o) => !o)}
            className="text-[10px] font-bold uppercase tracking-widest text-zinc-400 hover:text-zinc-200 flex items-center gap-1.5"
          >
            <FileText size={11} />
            <span>Log</span>
            <span className="text-zinc-600 font-normal">
              ({logFilter ? logs.filter((l) => l.tunnel_id === logFilter).length : logs.length})
            </span>
            {logFilter && (
              <span className="ml-1 px-1.5 rounded bg-primary/15 text-primary text-[9px]">filtered</span>
            )}
          </button>
          <div className="flex items-center gap-1">
            {logFilter && (
              <button
                onClick={() => setLogFilter(null)}
                title="Show all tunnels"
                className="px-1.5 text-[9px] uppercase tracking-wider text-zinc-500 hover:text-zinc-200"
              >
                Clear filter
              </button>
            )}
            <button
              onClick={() => setLogs([])}
              title="Clear log"
              className="p-1 text-zinc-500 hover:text-rose-400 rounded hover:bg-white/5"
            >
              <Trash2 size={11} />
            </button>
            <button
              onClick={() => setLogsOpen((o) => !o)}
              title={logsOpen ? "Collapse" : "Expand"}
              className="p-1 text-zinc-500 hover:text-zinc-200 rounded hover:bg-white/5"
            >
              {logsOpen ? <ArrowDownLeft size={11} className="rotate-90" /> : <ArrowDownLeft size={11} className="-rotate-90" />}
            </button>
          </div>
        </div>
        {logsOpen && (
          <div
            ref={logScrollRef}
            onScroll={onLogScroll}
            className="h-40 overflow-auto px-2 py-1 font-mono text-[10.5px] leading-relaxed space-y-0.5"
          >
            {(() => {
              const visible = logFilter ? logs.filter((l) => l.tunnel_id === logFilter) : logs;
              if (visible.length === 0) {
                return (
                  <div className="text-zinc-600 text-center py-4">
                    No log entries {logFilter ? "for this tunnel" : "yet"}.
                  </div>
                );
              }
              return visible.map((l, i) => {
                const ts = new Date(l.ts_ms);
                const hh = String(ts.getHours()).padStart(2, "0");
                const mm = String(ts.getMinutes()).padStart(2, "0");
                const ss = String(ts.getSeconds()).padStart(2, "0");
                const tone =
                  l.level === "error" ? "text-rose-300" :
                  l.level === "warn"  ? "text-amber-300" :
                                        "text-zinc-400";
                const eventTone =
                  l.event === "fail"    ? "bg-rose-500/15 text-rose-300" :
                  l.event === "connect" ? "bg-emerald-500/15 text-emerald-300" :
                  l.event === "close"   ? "bg-white/5 text-zinc-500" :
                  l.event === "listen"  ? "bg-indigo-500/15 text-indigo-300" :
                  l.event === "stop" || l.event === "shutdown"
                                        ? "bg-amber-500/15 text-amber-300" :
                                          "bg-white/5 text-zinc-500";
                return (
                  <div key={i} className="flex items-start gap-1.5">
                    <span className="text-zinc-600 shrink-0">{hh}:{mm}:{ss}</span>
                    <span className={`px-1 rounded text-[9px] uppercase font-bold shrink-0 ${eventTone}`}>
                      {l.event}
                    </span>
                    <span className={`flex-1 break-all ${tone}`}>
                      {l.target && <span className="text-zinc-200">{l.target}</span>}
                      {l.peer && <span className="text-zinc-600"> ← {l.peer}</span>}
                      {l.message && (
                        <span className={l.target || l.peer ? "text-zinc-500 ml-1" : ""}>
                          {l.target || l.peer ? "· " : ""}{l.message}
                        </span>
                      )}
                    </span>
                  </div>
                );
              });
            })()}
          </div>
        )}
      </div>
    </div>
  );
};

export default TunnelsPanel;
