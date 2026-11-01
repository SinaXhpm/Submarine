import { useEffect, useMemo, useRef, useState, forwardRef, useImperativeHandle } from "react";
import { createPortal } from "react-dom";
import { invoke } from "@tauri-apps/api/core";
import { File as FileIcon, Download, Upload, AlertTriangle, Check, X, Ban, Folder, Rows, Columns, LayoutPanelTop, Shield, ShieldAlert, Trash2, GitCompare } from "lucide-react";
import FilePanel, { ActiveDrag, FilePanelHandle } from "./FilePanel";
import type { FileEntry } from "../fs/types";
import MirrorsPanel from "./MirrorsPanel";
import { createLocalProvider } from "../fs/localProvider";
import { createRemoteProvider } from "../fs/remoteProvider";
import { useTextPrompt } from "../ui/confirm";
import { useIsNarrow } from "../hooks/useViewport";
import { useTauriListen } from "../hooks/useTauriListen";
import { bookmarksKey } from "../util/sftpBookmarks";
import { FOLLOW_SETUP, type ShellFolderState } from "../util/shellFolder";

// Root (sudo) mode for the remote pane — see `sftp_set_elevated` in lib.rs.
type ElevationStatus = { elevated: boolean; passwordless: boolean };

const SUDO_MESSAGES: Record<string, string> = {
  "[SUDO] NEED_PASSWORD": "sudo needs a password.",
  "[SUDO] WRONG_PASSWORD": "Wrong sudo password.",
  "[SUDO] NOT_ALLOWED": "This user isn't allowed to run sftp-server with sudo.",
  "[SUDO] REQUIRETTY": "sudo on this server requires a terminal (requiretty), so it can't be used for file access.",
  "[SUDO] NO_SUDO": "sudo isn't installed on this server.",
  "[SUDO] NO_SFTP_SERVER": "Couldn't find the sftp-server program on this server.",
};

const describeSudoError = (raw: string) => {
  const code = Object.keys(SUDO_MESSAGES).find((k) => raw.includes(k));
  return code ? SUDO_MESSAGES[code] : raw.replace(/^\[SUDO\] FAILED:?\s*/, "sudo failed: ");
};

// Dual-pane SFTP workspace. Owns the two FilePanels, the cross-pane drag
// state, and the global mouseup that turns a release over the opposite pane
// into the source pane's own upload or download (FilePanelHandle.sendTo).

export type SftpView = "files" | "mirror";

interface SftpWorkspaceProps {
  sessionId: string;
  disabled?: boolean;
  // Mirror config from the parent session — both pieces are needed by the
  // nested MirrorsPanel. Passing them through here keeps Mirror inside the
  // SFTP tool instead of a separate toolbar entry, so the session toolbar
  // has one fewer item to fit on phones.
  serverId?: number;
  mirrorsConfig?: any[];
  // Files or Mirror. The switch is the Mirror button in the SFTP panel's
  // title bar (SessionView), which leaves this panel all its height.
  view?: SftpView;
  // The unlocked profile: with the server id it keys the panes' bookmarks
  // (util/sftpBookmarks). A server that isn't saved (Quick Connect) has none.
  profile?: string | null;
  // Follow (#245), the button in the SFTP panel's title bar: the folder the
  // active terminal's shell reported, where the remote pane goes. Null while
  // the shell hasn't said (the pane shows how to make it); undefined while
  // Follow is off.
  follow?: ShellFolderState | null;
}
// "split" is Local above Remote; the name predates "side" and stays because
// it is what earlier versions saved.
type FilesLayout = "tabs" | "split" | "side";
type FilesSide = "local" | "remote";

const LAYOUT_OPTIONS: { id: FilesLayout; title: string; Icon: typeof Rows }[] = [
  { id: "tabs", title: "One panel at a time", Icon: LayoutPanelTop },
  { id: "split", title: "Both panels, Local above Remote (drag-drop between them)", Icon: Rows },
  { id: "side", title: "Both panels side by side (drag-drop between them)", Icon: Columns },
];

// Cursor-following drag ghost, isolated into its own component so that the
// per-mousemove position updates re-render ONLY this tiny node — not the whole
// SftpWorkspace and, through it, both (un-memoized) FilePanels with their full
// row lists. The parent drives it imperatively via the ref instead of holding
// the position in its own state.
export interface DragGhostHandle {
  show: (drag: ActiveDrag) => void;
  hide: () => void;
}

const DragGhost = forwardRef<DragGhostHandle>((_props, ref) => {
  const [drag, setDrag] = useState<ActiveDrag | null>(null);
  useImperativeHandle(ref, () => ({
    show: (d) => setDrag(d),
    hide: () => setDrag(null),
  }), []);
  if (!drag) return null;
  // Rendered through a portal so any ancestor's `transform` / `backdrop-filter`
  // doesn't re-anchor the `position: fixed` element to a containing block.
  return createPortal(
    <div
      style={{
        position: "fixed",
        top: drag.y + 8,
        left: drag.x + 12,
        pointerEvents: "none",
        zIndex: 10000,
      }}
      className="bg-[#0c0c0e]/95 border border-indigo-500/40 rounded-lg px-3 py-1.5 text-[11px] font-mono text-zinc-100 shadow-2xl backdrop-blur-md flex items-center gap-2"
    >
      {drag.entries.length === 1 && drag.entries[0].isDir
        ? <Folder size={12} className="text-indigo-300 shrink-0" />
        : <FileIcon size={12} className="text-indigo-300 shrink-0" />}
      <span className="truncate max-w-[260px]">
        {drag.entries.length === 1 ? drag.entries[0].name : `${drag.entries.length} items`}
      </span>
    </div>,
    document.body
  );
});
DragGhost.displayName = "DragGhost";

// Live transfer progress, keyed by the backend-assigned id. The Rust
// commands stream events at ~10Hz; we replace the entry on each update so
// a single growing progress bar shows per transfer. A folder delete that
// takes a while comes through here too (kind "delete"): for it `bytes` is
// the number of entries removed so far, and there is no total.
interface Transfer {
  id: string;
  name: string;
  kind: "upload" | "download" | "delete";
  bytes: number;
  total: number;
  status: "progress" | "done" | "error" | "cancelled";
  error?: string;
}

const formatBytes = (n: number) => {
  if (!n) return "0 B";
  const k = 1024, units = ["B", "KB", "MB", "GB"];
  const i = Math.min(units.length - 1, Math.floor(Math.log(n) / Math.log(k)));
  return `${(n / Math.pow(k, i)).toFixed(i === 0 ? 0 : 1)} ${units[i]}`;
};

// The live transfer cards: one growing progress bar per active upload or
// download; a card fades out shortly after its transfer ends. A component of
// its own for the same reason as DragGhost: an event re-renders only this, not
// the whole SftpWorkspace and both FilePanels, ten times a second per transfer,
// on the thread the terminal beside them draws and takes keys on.
function TransferCards({ sessionId }: { sessionId: string }) {
  const [transfers, setTransfers] = useState<Record<string, Transfer>>({});

  useTauriListen<Transfer>(`sftp-transfer-${sessionId}`, (event) => {
    const t = event.payload;
    if (!t || !t.id) return;
    setTransfers((prev) => ({ ...prev, [t.id]: t }));
    if (t.status === "done" || t.status === "error" || t.status === "cancelled") {
      // Leave the final state visible briefly before clearing the card so
      // the user sees the success tick / failure colour / cancel notice.
      const linger = t.status === "error" || t.error ? 6000 : t.status === "cancelled" ? 3000 : 1800;
      setTimeout(() => {
        setTransfers((prev) => {
          const { [t.id]: _, ...rest } = prev;
          return rest;
        });
      }, linger);
    }
  }, [sessionId]);

  const cancelTransfer = (id: string) => {
    // Fire-and-forget: the backend emits its own "cancelled" event when the
    // loop notices the flag, which is what wipes the card.
    invoke("sftp_cancel_transfer", { transferId: id }).catch(() => {});
  };

  return (
    <>
      {Object.values(transfers).map((t) => {
        const pct = t.total > 0 ? Math.min(100, Math.round((t.bytes * 100) / t.total)) : 0;
        const tone =
          t.status === "error"     ? "border-rose-500/40 bg-rose-950/85 text-rose-200" :
          t.status === "done"      ? "border-emerald-500/40 bg-emerald-950/85 text-emerald-200" :
          t.status === "cancelled" ? "border-amber-500/40 bg-amber-950/85 text-amber-200" :
                                     "border-indigo-500/40 bg-indigo-950/85 text-indigo-100";
        const barTone =
          t.status === "error"     ? "bg-rose-500" :
          t.status === "done"      ? "bg-emerald-500" :
          t.status === "cancelled" ? "bg-amber-500" :
                                     "bg-indigo-500";
        const Icon =
          t.status === "error"     ? AlertTriangle :
          t.status === "done"      ? Check :
          t.status === "cancelled" ? Ban :
          t.kind   === "upload"    ? Upload :
          t.kind   === "delete"    ? Trash2 :
                                     Download;
        const statusLabel =
          t.status === "error"     ? "failed"
          : t.status === "cancelled" ? "cancelled"
          : t.kind === "delete"      ? `${t.bytes.toLocaleString()} removed`
          : t.total > 0              ? `${pct}%`
                                     : formatBytes(t.bytes);
        return (
          <div key={t.id} className={`w-full max-w-[280px] pointer-events-auto px-2.5 py-1.5 rounded border ${tone} font-mono text-[10.5px] shadow-2xl backdrop-blur-md animate-in fade-in slide-in-from-right-4`}>
            <div className="flex items-center gap-1.5 mb-1">
              <Icon size={11} className="shrink-0" />
              <span className="truncate flex-1" title={t.name}>{t.name}</span>
              <span className="text-[9.5px] opacity-80 shrink-0">{statusLabel}</span>
              {/* Stop button — only while the transfer is still running.
                  Finished / failed / cancelled cards fade out on their
                  own timer; no button needed. */}
              {t.status === "progress" && (
                <button
                  onClick={() => cancelTransfer(t.id)}
                  title="Cancel"
                  className="shrink-0 p-0.5 rounded hover:bg-white/15 text-zinc-300 hover:text-rose-300"
                >
                  <X size={11} />
                </button>
              )}
            </div>
            {t.status !== "error" && (
              <div className="h-1 bg-white/10 rounded overflow-hidden">
                {/* No transition: with a step every 100 ms, an animated width
                    is a layout and a paint at every frame for as long as the
                    transfer runs, a whole core beside a long file list. */}
                <div
                  className={`h-full ${barTone} ${t.kind === "delete" && t.status === "progress" ? "animate-pulse" : ""}`}
                  style={{ width: t.total > 0 ? `${pct}%` : "100%" }}
                />
              </div>
            )}
            {/* Why it failed: up to three lines, so the reason can be
                read without hovering; the tooltip has all of it. */}
            {t.status === "error" && t.error && (
              <div className="text-[9.5px] opacity-80 line-clamp-3 [overflow-wrap:anywhere]" title={t.error}>{t.error}</div>
            )}
            {/* A finished folder download can carry a note, e.g. names
                this computer can't store were skipped; a cancelled
                upload says what it left on the server. */}
            {(t.status === "done" || t.status === "cancelled") && t.error && (
              <div className="text-[9.5px] text-amber-300/90 line-clamp-3 [overflow-wrap:anywhere]" title={t.error}>{t.error}</div>
            )}
          </div>
        );
      })}
    </>
  );
}

// Follow is on but the terminal's shell hasn't said which folder it is in:
// the one line that makes it, for the shell picked.
function FollowSetup({ onCopy }: { onCopy: (line: string) => void }) {
  const [picked, setPicked] = useState(0);
  const setup = FOLLOW_SETUP[picked];
  return (
    <div className="px-1 flex flex-col gap-1 text-[10px] leading-snug text-zinc-400">
      <span>
        The terminal hasn't said which folder it is in. Add this line to {setup.file}, then open a new terminal.
        {setup.note ? ` ${setup.note}` : ""}
      </span>
      <div className="flex items-center gap-1">
        {FOLLOW_SETUP.map((s, i) => (
          <button
            key={s.shell}
            type="button"
            aria-pressed={i === picked}
            onClick={() => setPicked(i)}
            className={`h-5 px-1.5 rounded border ${i === picked ? "text-primary bg-primary/10 border-primary/30" : "text-zinc-400 border-white/10 hover:text-white"}`}
          >
            {s.shell}
          </button>
        ))}
        <button
          type="button"
          onClick={() => onCopy(setup.line)}
          className="ml-auto h-5 px-1.5 rounded border border-white/10 text-zinc-300 hover:text-white hover:bg-white/5"
        >
          Copy
        </button>
      </div>
      <code className="block px-1.5 py-1 rounded bg-black/30 font-mono text-zinc-300 break-all select-text">{setup.line}</code>
    </div>
  );
}

const SftpWorkspace = ({ sessionId, disabled = false, serverId = 0, mirrorsConfig = [], view = "files", profile, follow }: SftpWorkspaceProps) => {
  // Mirror needs a saved server; without one the files always show.
  const shownView: SftpView = view === "mirror" && serverId ? "mirror" : "files";
  // Files layout: "tabs" (one side full height) is the default because the
  // side panel is narrow on most desktop setups and "split" squeezed each
  // FilePanel into 5-6 rows. "split" and "side" stay available for users who
  // want simultaneous Local+Remote visibility (drag-drop still works there).
  // Persisted globally (not per-session) — pure layout preference.
  const [layout, setLayout] = useState<FilesLayout>(() => {
    try {
      const v = localStorage.getItem("submarine-sftp-layout");
      return v === "split" || v === "side" ? v : "tabs";
    } catch { return "tabs"; }
  });
  // Two lists side by side don't fit a phone-width window: there "side" shows
  // as "split", and the saved choice comes back once the window is wider.
  const isNarrow = useIsNarrow();
  const shownLayout: FilesLayout = layout === "side" && isNarrow ? "split" : layout;
  const setLayoutPersisted = (l: FilesLayout) => {
    setLayout(l);
    try { localStorage.setItem("submarine-sftp-layout", l); } catch { /* quota — ignore */ }
  };
  // Active side in tabs mode. We persist it per (profile, session) so the user
  // returns to the side they were last using, not always Local. With the
  // profile: session ids come from each profile's server ids, so another
  // profile reuses them.
  const sideStorageKey = `submarine-sftp-side:${profile}:${sessionId}`;
  const [activeSide, setActiveSide] = useState<FilesSide>(() => {
    try {
      const v = localStorage.getItem(sideStorageKey);
      return v === "remote" ? "remote" : "local";
    } catch { return "local"; }
  });
  const setActiveSidePersisted = (s: FilesSide) => {
    setActiveSide(s);
    try { localStorage.setItem(sideStorageKey, s); } catch { /* ignore */ }
  };
  // Providers are created once per session so the panels' provider identity
  // is stable across renders (the FilePanel's load-on-mount effect keys off it).
  const localProvider = useMemo(() => createLocalProvider(), []);
  const remoteProvider = useMemo(() => createRemoteProvider(sessionId), [sessionId]);

  const localRef = useRef<FilePanelHandle | null>(null);
  const remoteRef = useRef<FilePanelHandle | null>(null);

  // Compare (#18): each pane's listing, for the other to mark its rows by.
  const [compareOn, setCompareOn] = useState(false);
  const [hideSame, setHideSame] = useState(false);
  const [localEntries, setLocalEntries] = useState<FileEntry[]>([]);
  const [remoteEntries, setRemoteEntries] = useState<FileEntry[]>([]);

  // ---- Root (sudo) mode for the remote pane ---------------------------------
  // Elevated = SFTP runs as root through sudo (passwordless or with a password
  // kept in backend memory for this tab only). A direct root login is flagged
  // with the same badge so it's always obvious when file ops have full power.
  const textPrompt = useTextPrompt();
  const [elevation, setElevation] = useState<ElevationStatus>({ elevated: false, passwordless: false });
  const [loginUser, setLoginUser] = useState<string | null>(null);
  const [elevBusy, setElevBusy] = useState(false);
  const [elevError, setElevError] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    invoke<ElevationStatus>("sftp_elevation_status", { sessionId })
      .then((s) => { if (!cancelled) setElevation(s); })
      .catch(() => {});
    return () => { cancelled = true; };
  }, [sessionId]);

  useEffect(() => {
    if (disabled) return;
    let cancelled = false;
    invoke<string>("sftp_login_user", { sessionId })
      .then((u) => { if (!cancelled) setLoginUser(u || null); })
      .catch(() => {});
    return () => { cancelled = true; };
  }, [sessionId, disabled]);

  const flashElevError = (msg: string) => {
    setElevError(msg);
    window.setTimeout(() => setElevError(null), 6000);
  };

  const toggleElevation = async () => {
    if (elevBusy || disabled) return;
    setElevBusy(true);
    setElevError(null);
    try {
      if (elevation.elevated) {
        setElevation(await invoke<ElevationStatus>("sftp_set_elevated", { sessionId, enabled: false, password: null }));
      } else {
        // Passwordless sudo first; only ask for a password when sudo wants one.
        let password: string | null = null;
        for (let wrongTries = 0; ; ) {
          try {
            setElevation(await invoke<ElevationStatus>("sftp_set_elevated", { sessionId, enabled: true, password }));
            break;
          } catch (e: any) {
            const raw = String(e);
            const wrong = raw.includes("[SUDO] WRONG_PASSWORD");
            if (!wrong && !raw.includes("[SUDO] NEED_PASSWORD")) throw e;
            if (wrong && ++wrongTries >= 3) throw e;
            password = await textPrompt({
              title: "Run file operations as root",
              message: wrong
                ? "Wrong sudo password, try again."
                : "Enter your sudo password. It's kept in memory for this tab only and never saved.",
              password: true,
              keepWhitespace: true,
              okLabel: "Use sudo",
              validate: (v) => (v ? null : "Enter the password"),
            });
            if (password === null) return;
          }
        }
      }
      await remoteRef.current?.refresh();
    } catch (e: any) {
      flashElevError(describeSudoError(String(e)));
    } finally {
      setElevBusy(false);
    }
  };

  const rootByLogin = !elevation.elevated && loginUser === "root";
  // The remote pane's header says how much room this control has (FilePanel
  // sets `data-elev` on the pane): the icon alone, the word, or the word
  // and how root was reached.
  const elevWord = "group-data-[elev=icon]/pane:hidden";
  const elevMore = "hidden group-data-[elev=full]/pane:inline";
  const elevBox = "h-[22px] px-1.5 shrink-0 flex items-center gap-1 rounded-md text-[10px] uppercase tracking-wider border";
  const rootBadgeClass = `${elevBox} font-black bg-rose-500/20 text-rose-200 border-rose-500/50`;
  const elevationControl = elevation.elevated ? (
    <button
      onClick={toggleElevation}
      disabled={elevBusy}
      title={`File operations run as root via sudo${elevation.passwordless ? " (passwordless)" : ""}. Click to go back to ${loginUser || "your login user"}.`}
      aria-label="root (sudo)"
      className={`${rootBadgeClass} hover:bg-rose-500/30 disabled:opacity-50`}
    >
      <ShieldAlert size={11} className="shrink-0" />
      <span className={elevWord}>root<span className={elevMore}> · sudo</span></span>
    </button>
  ) : rootByLogin ? (
    <span title="Logged in as root: file operations run with full privileges." className={rootBadgeClass}>
      <ShieldAlert size={11} className="shrink-0" />
      <span className={elevWord}>root</span>
    </span>
  ) : (
    <button
      onClick={toggleElevation}
      disabled={elevBusy || disabled}
      title="Run file operations as root (sudo)"
      aria-label="sudo"
      className={`${elevBox} font-bold bg-white/[0.04] text-zinc-300 border-white/10 hover:bg-white/10 hover:text-white disabled:opacity-40`}
    >
      <Shield size={11} className={`shrink-0 ${elevBusy ? "animate-pulse" : ""}`} />
      <span className={elevWord}>{elevBusy ? "…" : "sudo"}</span>
    </button>
  );
  // Why sudo failed: a line of its own under the header's buttons, so the
  // whole sentence shows and the buttons keep their places.
  const elevationNote = elevError ? (
    <div role="alert" className="px-1 text-[10px] leading-snug text-rose-300 break-words">{elevError}</div>
  ) : null;

  // Persist the last directory each panel was in per (session, side) so the
  // user lands on the same path next time they open this server. If the
  // saved path no longer exists, FilePanel falls back to provider.homePath().
  const storageKey = `submarine-server-dirs:${profile}:${sessionId}`;
  const savedDirsRef = useRef<{ local?: string; remote?: string }>(
    (() => {
      try {
        const raw = localStorage.getItem(storageKey);
        return raw ? JSON.parse(raw) : {};
      } catch { return {}; }
    })()
  );
  const saveDir = (side: "local" | "remote", path: string) => {
    savedDirsRef.current[side] = path;
    try { localStorage.setItem(storageKey, JSON.stringify(savedDirsRef.current)); }
    catch { /* quota or private-mode storage — ignore */ }
  };
  // Where each pane keeps its bookmarks; none for a server that isn't saved.
  const bookmarkKeyFor = (side: "local" | "remote") =>
    profile && serverId > 0 ? bookmarksKey(profile, serverId, side) : undefined;

  // Source of truth for the active drag. Updated SYNCHRONOUSLY from the
  // panel's onMove via the ref so the window mouseup handler (which runs in
  // the same tick as the panel's own mouseup that clears it) can still see
  // the source pane and entry. Going through React state introduces a race
  // because `setDrag → render → useEffect` doesn't always settle before the
  // mouseup propagates.
  const dragRef = useRef<ActiveDrag | null>(null);
  // The ghost owns its own position state; we poke it imperatively so a
  // mousemove never re-renders this workspace (and its FilePanels).
  const ghostRef = useRef<DragGhostHandle>(null);
  const [notification, setNotification] = useState<{ msg: string; type: "info" | "success" | "error" } | null>(null);

  const notify = (msg: string, type: "info" | "success" | "error" = "info") => {
    setNotification({ msg, type });
    setTimeout(() => setNotification(null), 4000);
  };

  // Follow (#245): the remote pane goes where the active terminal's shell says
  // it is, whenever that changes (and when Follow is turned on). A folder it
  // can't list (no rights, gone) leaves it where it is, and so does a terminal
  // that is on another host now (an ssh onward); the note under the pane's
  // header says which.
  const [followMiss, setFollowMiss] = useState<string | null>(null);
  const followPath = follow && !follow.away ? follow.path : null;
  useEffect(() => {
    setFollowMiss(null);
    const pane = remoteRef.current;
    if (!followPath || disabled || !pane || pane.currentDir() === followPath) return;
    let current = true;
    pane.goTo(followPath).then((ok) => { if (current && !ok) setFollowMiss(followPath); });
    return () => { current = false; };
  }, [followPath, disabled]);
  const copyLine = async (line: string) => {
    try {
      await navigator.clipboard.writeText(line);
      notify("Copied", "success");
    } catch {
      notify("Couldn't copy it: select the line and copy it from there.", "error");
    }
  };
  const followNote = follow === undefined ? null
    : follow === null ? <FollowSetup onCopy={copyLine} />
    : follow.away ? (
      <div className="px-1 text-[10px] leading-snug text-amber-300 break-words">
        The terminal is on {follow.away} now; the files stay on this server.
      </div>
    ) : followMiss ? (
      <div role="alert" className="px-1 text-[10px] leading-snug text-amber-300 break-words">
        Can't open {followMiss}, the terminal's folder.
      </div>
    ) : null;

  const handleDragMove = (drag: ActiveDrag | null) => {
    dragRef.current = drag;
    if (drag) ghostRef.current?.show(drag);
    else ghostRef.current?.hide();
  };

  // The workspace's own element: a drop counts only on its panes.
  const rootRef = useRef<HTMLDivElement | null>(null);

  // Cross-pane drop: on a release over this workspace's other pane, the pane
  // the drag came from sends what was dragged there with its own upload or
  // download (folders, several items, the overwrite prompts), into the folder
  // row under the pointer or else the folder that pane shows. The other pane
  // then lists again (onTransferred). A release over a pane of another
  // session's workspace (tabs side by side) is not this one's to handle.
  useEffect(() => {
    const onMouseUp = (e: MouseEvent) => {
      const active = dragRef.current;
      if (!active) return;
      // Clear immediately so a second mouseup (e.g. the FilePanel's own
      // listener clearing the ghost) doesn't re-enter this branch.
      dragRef.current = null;

      const hit = document.elementFromPoint(e.clientX, e.clientY) as HTMLElement | null;
      const pane = hit?.closest("[data-fs-pane]") as HTMLElement | null;
      if (!pane || !rootRef.current?.contains(pane)) return;
      const targetPaneId = pane.getAttribute("data-fs-pane");
      if (!targetPaneId || targetPaneId === active.paneId) return;

      // Dropped on a folder row: into that folder, as in Finder or Explorer.
      const row = hit?.closest("[data-fs-row-isdir]") as HTMLElement | null;
      const rowPath = row?.getAttribute("data-fs-row-isdir") === "1" ? row.getAttribute("data-fs-row-path") : null;
      const targetDir = rowPath || pane.getAttribute("data-fs-current-path") || "";
      if (!targetDir) return;

      const source = active.paneId === "local" ? localRef.current : remoteRef.current;
      source?.sendTo(active.entries, targetDir).catch((err) => notify(`Transfer failed: ${err}`, "error"));
    };
    window.addEventListener("mouseup", onMouseUp);
    return () => window.removeEventListener("mouseup", onMouseUp);
  }, []);

  return (
    <div ref={rootRef} className="flex-1 flex flex-col h-full overflow-hidden bg-[#0a0a0c] relative">
      {/* Files view — dual-pane browser. Stays mounted when Mirror is on top
          so directory state and selection don't reset across tab toggles.
          Both FilePanels are ALWAYS mounted (one is just CSS-hidden in
          tabs mode) so cd state, scroll position, and selection survive a
          tab toggle. */}
      <div className={`${shownView === "files" ? "flex-1 flex flex-col min-h-0" : "hidden"}`}>
        {/* Layout toolbar: Local|Remote pills in tabs mode (or a static
            label when both panels show), plus the global layout picker on
            the right. */}
        <div className="shrink-0 h-10 sm:h-8 flex items-stretch border-b border-white/5 bg-black/20">
          {shownLayout === "tabs" ? (
            <div className="flex-1 grid grid-cols-2">
              <button
                onClick={() => setActiveSidePersisted("local")}
                className={`h-full flex items-center justify-center gap-1.5 text-[10px] font-bold uppercase tracking-wider transition-all ${
                  activeSide === "local"
                    ? "text-emerald-300 bg-emerald-500/5 border-b border-emerald-400"
                    : "text-zinc-500 hover:text-zinc-200 hover:bg-white/[0.03] border-b border-transparent"
                }`}
              >
                <Folder size={11} /> Local
              </button>
              <button
                onClick={() => setActiveSidePersisted("remote")}
                className={`h-full flex items-center justify-center gap-1.5 text-[10px] font-bold uppercase tracking-wider transition-all ${
                  activeSide === "remote"
                    ? "text-sky-300 bg-sky-500/5 border-b border-sky-400"
                    : "text-zinc-500 hover:text-zinc-200 hover:bg-white/[0.03] border-b border-transparent"
                }`}
              >
                <Folder size={11} /> Remote
              </button>
            </div>
          ) : (
            <div className="flex-1 flex items-center px-3 text-[9.5px] font-bold uppercase tracking-widest text-zinc-500">
              Local + Remote
            </div>
          )}
          {/* Compare (#18): marks in both panes what differs from the other. */}
          <button
            type="button"
            onClick={() => setCompareOn((v) => !v)}
            aria-pressed={compareOn}
            title={compareOn ? "Stop comparing" : "Compare: mark what differs between the two folders"}
            className={`shrink-0 flex items-center gap-1.5 px-2.5 border-l border-white/5 text-[10px] font-bold uppercase tracking-wider transition-all ${
              compareOn ? "text-primary bg-primary/10" : "text-zinc-500 hover:bg-white/5 hover:text-white"
            }`}
          >
            <GitCompare size={12} />
            <span className="hidden sm:inline">Compare</span>
          </button>
          <div role="group" aria-label="Files layout" className="flex items-stretch border-l border-white/5 shrink-0">
            {LAYOUT_OPTIONS.filter((o) => o.id !== "side" || !isNarrow).map(({ id, title, Icon }) => (
              <button
                key={id}
                onClick={() => setLayoutPersisted(id)}
                title={title}
                aria-pressed={shownLayout === id}
                className={`w-9 sm:w-7 flex items-center justify-center transition-all ${
                  shownLayout === id
                    ? "text-primary bg-primary/10"
                    : "text-zinc-500 hover:bg-white/5 hover:text-white"
                }`}
              >
                <Icon size={12} />
              </button>
            ))}
          </div>
        </div>

        {compareOn && (
          <div className="shrink-0 flex flex-wrap items-center gap-x-3 gap-y-1 px-3 py-1.5 border-b border-white/5 bg-black/20 text-[10.5px] text-zinc-400">
            <span className="text-zinc-500">Each folder against the other, by name, size and time:</span>
            <span className="flex items-center gap-1"><span className="w-1 h-3 rounded-sm bg-amber-400" /> only here</span>
            <span className="flex items-center gap-1"><span className="w-1 h-3 rounded-sm bg-rose-400" /> another size or kind</span>
            <span className="flex items-center gap-1"><span className="w-1 h-3 rounded-sm bg-emerald-400" /> newer</span>
            <span className="flex items-center gap-1"><span className="w-1 h-3 rounded-sm bg-zinc-500" /> older</span>
            <label className="ml-auto flex items-center gap-1.5 cursor-pointer select-none">
              <input
                type="checkbox"
                checked={hideSame}
                onChange={(e) => setHideSame(e.target.checked)}
                className="accent-[var(--primary-hex)]"
              />
              Hide files that are the same
            </label>
          </div>
        )}
        {/* Both panels share one box: a column for tabs and split, a row
            for side by side. */}
        <div className={`flex-1 min-h-0 flex ${shownLayout === "side" ? "flex-row" : "flex-col"}`}>
          {/* Local panel — visible in split mode (top), side mode (left), or
              in tabs mode when Local is the active side. Hidden via CSS (not
              unmounted) when on the inactive tab so its directory and
              provider state survive a side-swap. */}
          <div className={
            shownLayout === "split"
              ? "flex-1 min-h-0 border-b border-white/10"
              : shownLayout === "side"
                ? "flex-1 min-w-0 min-h-0 border-r border-white/10"
                : activeSide === "local" ? "flex-1 min-h-0" : "hidden"
          }>
            <FilePanel
              ref={localRef}
              provider={localProvider}
              // The local pane also needs sessionId so its bulk-upload button can
              // invoke sftp_upload_file / sftp_upload_dir on the right session.
              // Without this the Upload button silently no-ops on the first
              // guard (`if (!sessionId) return`).
              sessionId={sessionId}
              disabled={disabled}
              onDragMove={handleDragMove}
              initialPath={savedDirsRef.current.local}
              onPathChange={(p) => saveDir("local", p)}
              getOppositeDir={() => remoteRef.current?.currentDir()}
              onTransferred={() => remoteRef.current?.refresh()}
              bookmarksKey={bookmarkKeyFor("local")}
              onEntriesChange={setLocalEntries}
              compareWith={compareOn ? remoteEntries : null}
              hideSame={hideSame}
            />
          </div>
          <div className={
            shownLayout === "split"
              ? "flex-1 min-h-0"
              : shownLayout === "side"
                ? "flex-1 min-w-0 min-h-0"
                : activeSide === "remote" ? "flex-1 min-h-0" : "hidden"
          }>
            <FilePanel
              ref={remoteRef}
              provider={remoteProvider}
              sessionId={sessionId}
              disabled={disabled}
              onDragMove={handleDragMove}
              initialPath={savedDirsRef.current.remote}
              onPathChange={(p) => saveDir("remote", p)}
              getOppositeDir={() => localRef.current?.currentDir()}
              onTransferred={() => localRef.current?.refresh()}
              bookmarksKey={bookmarkKeyFor("remote")}
              onEntriesChange={setRemoteEntries}
              compareWith={compareOn ? localEntries : null}
              hideSame={hideSame}
              headerExtra={elevationControl}
              headerNote={<>{elevationNote}{followNote}</>}
              rootMode={elevation.elevated || rootByLogin}
              archiveOff={elevation.elevated ? "Archive and extract aren't available in root mode: they run as the login user, not through sudo" : undefined}
            />
          </div>
        </div>
      </div>

      {/* Mirror view — kept MOUNTED (CSS hidden) so the live worker's logs
          and progress counters survive when the user pops back to Files. */}
      {!!serverId && (
        <div className={`${shownView === "mirror" ? "flex-1 flex flex-col min-h-0" : "hidden"}`}>
          <MirrorsPanel
            sessionId={sessionId}
            serverId={serverId}
            configuredMirrors={mirrorsConfig}
            disabled={disabled}
          />
        </div>
      )}

      {/* Notices along the bottom of the workspace: the toast, then the live
          transfer cards. The stack spans the workspace, so nothing in it can
          be wider than a narrow panel and end up cut off at the panel's edge.
          Clicks go through it everywhere but on a card. */}
      <div className="absolute bottom-3 inset-x-3 z-50 flex flex-col items-end gap-1.5 pointer-events-none">
        {notification && (
          <div className={`self-center max-w-full px-3 py-1.5 rounded-lg border text-[11px] font-mono [overflow-wrap:anywhere] shadow-2xl backdrop-blur-md animate-in fade-in slide-in-from-bottom-4 duration-300 ${
            notification.type === "success" ? "bg-emerald-950/90 border-emerald-500/30 text-emerald-400" :
            notification.type === "error"   ? "bg-rose-950/90 border-rose-500/30 text-rose-400" :
                                              "bg-indigo-950/90 border-indigo-500/30 text-indigo-400"
          }`}>{notification.msg}</div>
        )}
        <TransferCards sessionId={sessionId} />
      </div>

      {/* Cursor-following drag ghost. Self-contained so its per-mousemove
          position updates don't re-render this workspace or the FilePanels. */}
      <DragGhost ref={ghostRef} />
    </div>
  );
};

export default SftpWorkspace;
