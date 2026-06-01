import { useEffect, useMemo, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { listen } from "@tauri-apps/api/event";
import { invoke } from "@tauri-apps/api/core";
import { File as FileIcon, Download, Upload, AlertTriangle, Check, X, Ban } from "lucide-react";
import FilePanel, { ActiveDrag, FilePanelHandle } from "./FilePanel";
import { createLocalProvider } from "../fs/localProvider";
import { createRemoteProvider } from "../fs/remoteProvider";
import { transferFile } from "../fs/transfer";

// Dual-pane SFTP workspace. Owns the two FilePanels, the cross-pane drag
// state, and the global mouseup that turns a release over the opposite pane
// into a `transferFile` call. The panels themselves stay agnostic — they only
// know how to drive their own provider.

interface SftpWorkspaceProps {
  sessionId: string;
  disabled?: boolean;
}

const SftpWorkspace = ({ sessionId, disabled = false }: SftpWorkspaceProps) => {
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
  // Separate state purely for the ghost element render.
  const [ghostDrag, setGhostDrag] = useState<ActiveDrag | null>(null);
  const [notification, setNotification] = useState<{ msg: string; type: "info" | "success" | "error" } | null>(null);

  const notify = (msg: string, type: "info" | "success" | "error" = "info") => {
    setNotification({ msg, type });
    setTimeout(() => setNotification(null), 4000);
  };

  const handleDragMove = (drag: ActiveDrag | null) => {
    dragRef.current = drag;
    setGhostDrag(drag);
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
      const targetDir = pane.getAttribute("data-fs-current-path") || "";
      if (!targetDir) return;

      const srcProv = active.paneId === "local" ? localProvider : remoteProvider;
      const destProv = targetPaneId === "local" ? localProvider : remoteProvider;

      const action = active.paneId === "local" && targetPaneId === "remote"
        ? "Uploading"
        : active.paneId === "remote" && targetPaneId === "local"
          ? "Downloading"
          : "Moving";
      notify(`${action} ${active.entry.name}…`, "info");

      transferFile(
        { provider: srcProv, path: active.entry.path, name: active.entry.name, isDir: active.entry.isDir },
        { provider: destProv, dir: targetDir }
      ).then(() => {
        notify(`${active.entry.name} ✓`, "success");
        // Refresh both sides — source may have lost the file (move semantics
        // for same-side transfers), target gains it.
        localRef.current?.refresh();
        remoteRef.current?.refresh();
      }).catch((err) => {
        notify(`Transfer failed: ${err}`, "error");
        console.error("Cross-pane transfer failed:", err);
      });
    };
    window.addEventListener("mouseup", onMouseUp);
    return () => window.removeEventListener("mouseup", onMouseUp);
  }, [localProvider, remoteProvider]);

  return (
    <div className="flex-1 flex flex-col h-full overflow-hidden bg-[#0a0a0c] relative">
      <div className="flex-1 min-h-0 border-b border-white/10">
        <FilePanel
          ref={localRef}
          provider={localProvider}
          disabled={disabled}
          onDragMove={handleDragMove}
          initialPath={savedDirsRef.current.local}
          onPathChange={(p) => saveDir("local", p)}
          getOppositeDir={() => remoteRef.current?.currentDir()}
        />
      </div>
      <div className="flex-1 min-h-0">
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

      {/* Ghost element that follows the cursor while dragging. Rendered through
          a portal so any ancestor's `transform` / `backdrop-filter` doesn't
          re-anchor the `position: fixed` element to a containing block. */}
      {ghostDrag && createPortal(
        <div
          style={{
            position: "fixed",
            top: ghostDrag.y + 8,
            left: ghostDrag.x + 12,
            pointerEvents: "none",
            zIndex: 10000,
          }}
          className="bg-[#0c0c0e]/95 border border-indigo-500/40 rounded-lg px-3 py-1.5 text-[11px] font-mono text-zinc-100 shadow-2xl backdrop-blur-md flex items-center gap-2"
        >
          <FileIcon size={12} className="text-indigo-300 shrink-0" />
          <span className="truncate max-w-[260px]">{ghostDrag.entry.name}</span>
        </div>,
        document.body
      )}
    </div>
  );
};

export default SftpWorkspace;
