import { useEffect, useMemo, useRef, useState, forwardRef, useImperativeHandle } from "react";
import { createPortal } from "react-dom";
import { listen } from "@tauri-apps/api/event";
import { invoke } from "@tauri-apps/api/core";
import { File as FileIcon, Download, Upload, AlertTriangle, Check, X, Ban, Folder, FolderUp, Rows, LayoutPanelTop } from "lucide-react";
import FilePanel, { ActiveDrag, FilePanelHandle } from "./FilePanel";
import MirrorsPanel from "./MirrorsPanel";
import { createLocalProvider } from "../fs/localProvider";
import { createRemoteProvider } from "../fs/remoteProvider";
import { transferFile } from "../fs/transfer";
import { useOverwritePrompt } from "../ui/confirm";

// Dual-pane SFTP workspace. Owns the two FilePanels, the cross-pane drag
// state, and the global mouseup that turns a release over the opposite pane
// into a `transferFile` call. The panels themselves stay agnostic — they only
// know how to drive their own provider.

interface SftpWorkspaceProps {
  sessionId: string;
  disabled?: boolean;
  // Mirror config from the parent session — both pieces are needed by the
  // (now-nested) MirrorsPanel sub-tab. Passing them through here lets us
  // collapse the previously-separate Mirror toolbar entry into one SFTP
  // umbrella ("Files" vs "Mirror" sub-tabs), so the session toolbar has
  // one fewer item to fit on phones.
  serverId?: number;
  mirrorsConfig?: any[];
}

type SftpView = "files" | "mirror";
type FilesLayout = "tabs" | "split";
type FilesSide = "local" | "remote";

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
      <FileIcon size={12} className="text-indigo-300 shrink-0" />
      <span className="truncate max-w-[260px]">{drag.entry.name}</span>
    </div>,
    document.body
  );
});
DragGhost.displayName = "DragGhost";

const SftpWorkspace = ({ sessionId, disabled = false, serverId = 0, mirrorsConfig = [] }: SftpWorkspaceProps) => {
  // Active sub-tab. Files is the default (the common workflow); Mirror is
  // for the per-server one-way replication setup.
  const [view, setView] = useState<SftpView>("files");
  // Files layout: "tabs" (one side full height) is the default because the
  // side panel is narrow on most desktop setups and "split" squeezed each
  // FilePanel into 5-6 rows. "split" stays available for users who want
  // simultaneous Local+Remote visibility (drag-drop still works there).
  // Persisted globally (not per-session) — pure layout preference.
  const [layout, setLayout] = useState<FilesLayout>(() => {
    try {
      const v = localStorage.getItem("submarine-sftp-layout");
      return v === "split" ? "split" : "tabs";
    } catch { return "tabs"; }
  });
  const setLayoutPersisted = (l: FilesLayout) => {
    setLayout(l);
    try { localStorage.setItem("submarine-sftp-layout", l); } catch { /* quota — ignore */ }
  };
  // Active side in tabs mode. We persist it per (session) so the user
  // returns to the side they were last using, not always Local.
  const sideStorageKey = `submarine-sftp-side-${sessionId}`;
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

  // Persist the last directory each panel was in per (session, side) so the
  // user lands on the same path next time they open this server. If the
  // saved path no longer exists, FilePanel falls back to provider.homePath().
  const storageKey = `submarine-server-dirs-${sessionId}`;
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

  const handleDragMove = (drag: ActiveDrag | null) => {
    dragRef.current = drag;
    if (drag) ghostRef.current?.show(drag);
    else ghostRef.current?.hide();
  };

  // Live transfer progress, keyed by the backend-assigned id. The Rust
  // commands stream events at ~10Hz; we replace the entry on each update so
  // a single growing progress bar shows per transfer.
  interface Transfer {
    id: string;
    name: string;
    kind: "upload" | "download";
    bytes: number;
    total: number;
    status: "progress" | "done" | "error" | "cancelled";
    error?: string;
  }
  const [transfers, setTransfers] = useState<Record<string, Transfer>>({});

  useEffect(() => {
    let unlisten: (() => void) | null = null;
    listen<Transfer>(`sftp-transfer-${sessionId}`, (event) => {
      const t = event.payload;
      if (!t || !t.id) return;
      setTransfers((prev) => ({ ...prev, [t.id]: t }));
      if (t.status === "done" || t.status === "error" || t.status === "cancelled") {
        // Leave the final state visible briefly before clearing the card so
        // the user sees the success tick / failure colour / cancel notice.
        const linger = t.status === "error" ? 6000 : t.status === "cancelled" ? 3000 : 1800;
        setTimeout(() => {
          setTransfers((prev) => {
            const { [t.id]: _, ...rest } = prev;
            return rest;
          });
        }, linger);
      }
    }).then((fn) => { unlisten = fn; });
    return () => { if (unlisten) unlisten(); };
  }, [sessionId]);

  const cancelTransfer = (id: string) => {
    // Fire-and-forget: the backend emits its own "cancelled" event when the
    // loop notices the flag, which is what wipes the card.
    invoke("sftp_cancel_transfer", { transferId: id }).catch(() => {});
  };

  const formatBytes = (n: number) => {
    if (!n) return "0 B";
    const k = 1024, units = ["B", "KB", "MB", "GB"];
    const i = Math.min(units.length - 1, Math.floor(Math.log(n) / Math.log(k)));
    return `${(n / Math.pow(k, i)).toFixed(i === 0 ? 0 : 1)} ${units[i]}`;
  };

  const overwritePrompt = useOverwritePrompt();

  // Cross-pane drop dispatch: when the user releases the mouse anywhere, look
  // up which pane is under the cursor; if it differs from the source pane,
  // run the transfer and refresh both panels.
  useEffect(() => {
    const onMouseUp = (e: MouseEvent) => {
      const active = dragRef.current;
      if (!active) return;
      // Clear immediately so a second mouseup (e.g. the FilePanel's own
      // listener clearing the ghost) doesn't re-enter this branch.
      dragRef.current = null;

      const hit = document.elementFromPoint(e.clientX, e.clientY);
      if (!hit) return;
      const pane = (hit as HTMLElement).closest("[data-fs-pane]") as HTMLElement | null;
      if (!pane) return;
      const targetPaneId = pane.getAttribute("data-fs-pane");
      if (!targetPaneId || targetPaneId === active.paneId) return;

      // Row-aware drop: if the cursor is on a folder row inside the
      // destination pane, drop INTO that folder (its path) instead of
      // the pane's current directory. Falling on a file row, an empty
      // area, or the header still falls back to the pane's currentPath.
      // This makes the natural "drag onto folder" gesture work, matching
      // how users expect Finder/Explorer drag-drop to behave.
      const row = (hit as HTMLElement).closest("[data-fs-row-isdir]") as HTMLElement | null;
      const rowIsDir = row?.getAttribute("data-fs-row-isdir") === "1";
      const rowPath = rowIsDir ? row?.getAttribute("data-fs-row-path") : null;
      const targetDir = rowPath || pane.getAttribute("data-fs-current-path") || "";
      if (!targetDir) return;

      const srcProv = active.paneId === "local" ? localProvider : remoteProvider;
      const destProv = targetPaneId === "local" ? localProvider : remoteProvider;
      const isCrossSide = (active.paneId === "local") !== (targetPaneId === "local");

      const action = active.paneId === "local" && targetPaneId === "remote"
        ? "Uploading"
        : active.paneId === "remote" && targetPaneId === "local"
          ? "Downloading"
          : "Moving";
      notify(`${action} ${active.entry.name}…`, "info");

      const runTransfer = async () => {
        const srcInfo = { provider: srcProv, path: active.entry.path, name: active.entry.name, isDir: active.entry.isDir };
        const dstInfo = { provider: destProv, dir: targetDir };
        try {
          await transferFile(srcInfo, dstInfo, false);
        } catch (err: any) {
          const msg = String(err?.message ?? err);
          // The backend emits `EXISTS:<path>` only for cross-side SFTP
          // transfers; same-side rename surfaces its own per-provider
          // errors that won't match this sentinel. Both paths therefore
          // do the right thing.
          if (!msg.startsWith("EXISTS:") || !isCrossSide || active.entry.isDir) throw err;
          const direction = action === "Uploading" ? "upload" : "download";
          const choice = await overwritePrompt({ name: active.entry.name, direction, batchSize: 1 });
          if (choice === "cancel" || choice === "skip" || choice === "skip-all") {
            notify(`${active.entry.name} skipped`, "info");
            return;
          }
          await transferFile(srcInfo, dstInfo, true);
        }
        notify(`${active.entry.name} ✓`, "success");
        // Refresh both sides — source may have lost the file (move semantics
        // for same-side transfers), target gains it.
        localRef.current?.refresh();
        remoteRef.current?.refresh();
      };

      runTransfer().catch((err) => {
        notify(`Transfer failed: ${err}`, "error");
        console.error("Cross-pane transfer failed:", err);
      });
    };
    window.addEventListener("mouseup", onMouseUp);
    return () => window.removeEventListener("mouseup", onMouseUp);
  }, [localProvider, remoteProvider, overwritePrompt]);

  return (
    <div className="flex-1 flex flex-col h-full overflow-hidden bg-[#0a0a0c] relative">
      {/* Sub-tab strip — Files vs Mirror, replacing the standalone Mirror
          toolbar button that used to live next to SFTP / Ports / Library. The
          Mirror panel keeps state across tab switches via CSS hidden (same
          mounted-but-invisible pattern the parent SessionView used before)
          so the live worker's counters and rolling log survive a switch back
          to Files. */}
      <div className="shrink-0 grid grid-cols-2 border-b border-white/5 bg-black/20">
        <button
          onClick={() => setView("files")}
          className={`h-9 flex items-center justify-center gap-1.5 text-[11px] font-bold uppercase tracking-wider transition-all ${
            view === "files"
              ? "text-primary bg-primary/5 border-b border-primary"
              : "text-zinc-500 hover:text-zinc-200 hover:bg-white/[0.03] border-b border-transparent"
          }`}
        >
          <Folder size={12} /> Files
        </button>
        <button
          onClick={() => setView("mirror")}
          disabled={!serverId}
          title={!serverId ? "Mirror needs a saved server" : undefined}
          className={`h-9 flex items-center justify-center gap-1.5 text-[11px] font-bold uppercase tracking-wider transition-all disabled:opacity-40 disabled:cursor-not-allowed ${
            view === "mirror"
              ? "text-primary bg-primary/5 border-b border-primary"
              : "text-zinc-500 hover:text-zinc-200 hover:bg-white/[0.03] border-b border-transparent"
          }`}
        >
          <FolderUp size={12} /> Mirror
        </button>
      </div>

      {/* Files view — dual-pane browser. Stays mounted when Mirror is on top
          so directory state and selection don't reset across tab toggles.
          Both FilePanels are ALWAYS mounted (one is just CSS-hidden in
          tabs mode) so cd state, scroll position, and selection survive a
          tab toggle. */}
      <div className={`${view === "files" ? "flex-1 flex flex-col min-h-0" : "hidden"}`}>
        {/* Layout toolbar: Local|Remote pills in tabs mode (or a static
            label in split mode), plus the global layout toggle on the
            right. The toggle's label is the DESTINATION mode so the
            user can predict what clicking will do. */}
        <div className="shrink-0 h-10 sm:h-8 flex items-stretch border-b border-white/5 bg-black/20">
          {layout === "tabs" ? (
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
          <button
            onClick={() => setLayoutPersisted(layout === "tabs" ? "split" : "tabs")}
            title={layout === "tabs" ? "Show both panels stacked (drag-drop between them)" : "Switch to tabbed view (one panel at full height)"}
            className="px-3 border-l border-white/5 text-[10px] font-bold uppercase tracking-wider text-zinc-400 hover:bg-white/5 hover:text-white flex items-center gap-1.5 transition-all shrink-0"
          >
            {layout === "tabs"
              ? <><Rows size={11} /> Split</>
              : <><LayoutPanelTop size={11} /> Tabs</>
            }
          </button>
        </div>

        {/* Local panel — visible in split mode (top), or in tabs mode when
            Local is the active side. Hidden via CSS (not unmounted) when
            on the inactive tab so its directory and provider state
            survive a side-swap. */}
        <div className={
          layout === "split"
            ? "flex-1 min-h-0 border-b border-white/10"
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
          />
        </div>
        <div className={
          layout === "split"
            ? "flex-1 min-h-0"
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
          />
        </div>
      </div>

      {/* Mirror view — kept MOUNTED (CSS hidden) so the live worker's logs
          and progress counters survive when the user pops back to Files. */}
      {!!serverId && (
        <div className={`${view === "mirror" ? "flex-1 flex flex-col min-h-0" : "hidden"}`}>
          <MirrorsPanel
            sessionId={sessionId}
            serverId={serverId}
            configuredMirrors={mirrorsConfig}
            disabled={disabled}
          />
        </div>
      )}

      {notification && (
        <div className={`absolute bottom-3 left-1/2 -translate-x-1/2 z-50 px-3 py-1.5 rounded-lg border text-[11px] font-mono shadow-2xl backdrop-blur-md animate-in fade-in slide-in-from-bottom-4 duration-300 ${
          notification.type === "success" ? "bg-emerald-950/90 border-emerald-500/30 text-emerald-400" :
          notification.type === "error"   ? "bg-rose-950/90 border-rose-500/30 text-rose-400" :
                                            "bg-indigo-950/90 border-indigo-500/30 text-indigo-400"
        }`}>{notification.msg}</div>
      )}

      {/* Live transfer cards — one growing progress bar per active SFTP
          upload/download. Stacked bottom-right, fade out shortly after the
          transfer completes. */}
      {Object.values(transfers).length > 0 && (
        <div className="absolute bottom-3 right-3 z-50 flex flex-col gap-1.5 max-w-[280px]">
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
                                         Download;
            const statusLabel =
              t.status === "error"     ? "failed"
              : t.status === "cancelled" ? "cancelled"
              : t.total > 0              ? `${pct}%`
                                         : formatBytes(t.bytes);
            return (
              <div key={t.id} className={`px-2.5 py-1.5 rounded border ${tone} font-mono text-[10.5px] shadow-2xl backdrop-blur-md animate-in fade-in slide-in-from-right-4`}>
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
                    <div
                      className={`h-full ${barTone} transition-[width] duration-150`}
                      style={{ width: t.total > 0 ? `${pct}%` : "100%" }}
                    />
                  </div>
                )}
                {t.status === "error" && t.error && (
                  <div className="text-[9.5px] opacity-80 truncate" title={t.error}>{t.error}</div>
                )}
              </div>
            );
          })}
        </div>
      )}

      {/* Cursor-following drag ghost. Self-contained so its per-mousemove
          position updates don't re-render this workspace or the FilePanels. */}
      <DragGhost ref={ghostRef} />
    </div>
  );
};

export default SftpWorkspace;
