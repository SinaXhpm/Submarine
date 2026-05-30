import React, { useState, useEffect, useRef, useImperativeHandle, forwardRef } from "react";
import { createPortal } from "react-dom";
import { listen } from "@tauri-apps/api/event";
import { invoke } from "@tauri-apps/api/core";
import {
  Folder, File, ArrowUp, RefreshCw, Trash2, Edit3, Shield,
  X, ChevronUp, ChevronDown, Plus, MoreVertical, FolderSearch,
  Download, ExternalLink, Move
} from "lucide-react";
import { FileEntry, FileProvider } from "../fs/types";
import { useConfirm } from "../ui/confirm";

// Generic two-mode file panel. Drives all I/O through a `FileProvider` so
// the same component renders either the local filesystem or the remote SFTP
// tree. Drag-out and drop integration are handled by the parent workspace —
// FilePanel just emits lifecycle callbacks.

type SortColumn = "name" | "size" | "modified" | "permissions";
interface SortState { column: SortColumn; asc: boolean; }

export interface ActiveDrag {
  paneId: "local" | "remote";
  entry: FileEntry;
  x: number;
  y: number;
}

export interface FilePanelProps {
  provider: FileProvider;
  /** True when the underlying session is disconnected — UI is dimmed and ops are blocked. */
  disabled?: boolean;
  /** Optional session id to scope OS drag-drop events (Tauri fires them globally). */
  sessionId?: string;
  /** Notifies the parent of an active cross-pane drag. */
  onDragMove: (drag: ActiveDrag | null) => void;
  /**
   * Optional starting directory. Overrides the provider's home. If listing it
   * fails (e.g. the saved dir was removed since last session), the panel falls
   * back to the provider's home without surfacing the error.
   */
  initialPath?: string;
  /** Fires after every successful navigation — workspace uses it to persist. */
  onPathChange?: (path: string) => void;
  /**
   * Live read of whatever directory the *other* pane is currently in. Wired
   * from SftpWorkspace via the sibling's FilePanelHandle. Used by the remote
   * pane's download action: if the local pane has a directory open, the
   * download lands there directly instead of popping a folder picker.
   */
  getOppositeDir?: () => string | undefined;
}

export interface FilePanelHandle {
  refresh: () => Promise<void>;
  currentDir: () => string;
}

const FilePanel = forwardRef<FilePanelHandle, FilePanelProps>(({
  provider,
  disabled = false,
  sessionId,
  onDragMove,
  initialPath,
  onPathChange,
  getOppositeDir,
}, ref) => {
  const [currentPath, setCurrentPath] = useState("");
  const [entries, setEntries] = useState<FileEntry[]>([]);
  const [loading, setLoading] = useState(false);
  // Multi-selection lives as a Set of paths. Single-click replaces, Ctrl/⌘-
  // click toggles a row in/out, Shift-click extends from the last-clicked
  // anchor. lastSelectedPathRef remembers that anchor across renders.
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const lastSelectedPathRef = useRef<string | null>(null);
  const [sort, setSort] = useState<SortState>({ column: "name", asc: true });

  const [tempInput, setTempInput] = useState("");
  const [inputFocused, setInputFocused] = useState(false);
  const [activeSuggestion, setActiveSuggestion] = useState(-1);

  const [contextMenu, setContextMenu] = useState<{ x: number; y: number; entry: FileEntry } | null>(null);
  const [modal, setModal] = useState<{ type: "rename" | "mkdir" | "properties" | "move" | "move-bulk"; entry?: FileEntry; v1?: string; v2?: string } | null>(null);
  const [notification, setNotification] = useState<{ msg: string; type: "info" | "success" | "error" } | null>(null);

  const [dragOver, setDragOver] = useState(false);
  const dropTargetRef = useRef<HTMLDivElement | null>(null);
  const menuRef = useRef<HTMLDivElement | null>(null);
  const currentPathRef = useRef(currentPath);
  useEffect(() => { currentPathRef.current = currentPath; }, [currentPath]);

  // ---- helpers ----------------------------------------------------------------

  const notify = (msg: string, type: "info" | "success" | "error" = "info") => {
    setNotification({ msg, type });
    setTimeout(() => setNotification(null), 4000);
  };

  const formatRights = (isDir: boolean, perm?: number) => {
    if (perm === undefined) return isDir ? "d---------" : "----------";
    const r = (v: number) => (v & 4 ? "r" : "-");
    const w = (v: number) => (v & 2 ? "w" : "-");
    const x = (v: number) => (v & 1 ? "x" : "-");
    const u = (perm >> 6) & 7, g = (perm >> 3) & 7, o = perm & 7;
    return (isDir ? "d" : "-") + r(u) + w(u) + x(u) + r(g) + w(g) + x(g) + r(o) + w(o) + x(o);
  };

  const formatTime = (ts?: number) => {
    if (!ts) return "-";
    const d = new Date(ts * 1000);
    const pad = (n: number) => n.toString().padStart(2, "0");
    return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}`;
  };

  const formatSize = (bytes: number) => {
    if (!bytes) return "0 B";
    const k = 1024;
    const sizes = ["B", "KB", "MB", "GB", "TB"];
    const i = Math.floor(Math.log(bytes) / Math.log(k));
    return parseFloat((bytes / Math.pow(k, i)).toFixed(1)) + " " + sizes[i];
  };

  // ---- listing / navigation ---------------------------------------------------

  const fetch = async (path: string) => {
    setLoading(true);
    try {
      const result = await provider.list(path);
      setEntries(result.entries);
      setCurrentPath(result.currentPath);
      setTempInput(result.currentPath);
      setSelected(new Set());
      lastSelectedPathRef.current = null;
      onPathChange?.(result.currentPath);
    } catch (err: any) {
      notify(`List failed: ${err}`, "error");
    } finally {
      setLoading(false);
    }
  };

  // Initial load: try the caller-supplied `initialPath` first (the
  // per-server-saved directory), and fall back to the provider's home if it
  // no longer exists — that way a saved path being removed doesn't strand
  // the user with an error screen.
  useEffect(() => {
    (async () => {
      const tryFetch = async (path: string) => {
        const result = await provider.list(path);
        setEntries(result.entries);
        setCurrentPath(result.currentPath);
        setTempInput(result.currentPath);
        setSelected(new Set());
      lastSelectedPathRef.current = null;
        onPathChange?.(result.currentPath);
      };
      setLoading(true);
      try {
        if (initialPath) {
          try { await tryFetch(initialPath); return; }
          catch { /* fall through to home */ }
        }
        const home = await provider.homePath();
        await tryFetch(home);
      } catch (err: any) {
        notify(`Failed to load: ${err}`, "error");
      } finally {
        setLoading(false);
      }
    })();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [provider]);

  useImperativeHandle(ref, () => ({
    refresh: () => fetch(currentPathRef.current),
    currentDir: () => currentPathRef.current,
  }), []);

  const goUp = () => fetch(provider.parentPath(currentPath));

  // ---- selection -------------------------------------------------------------

  // Three click modes match every desktop file manager people already know:
  //   - plain click  → replace selection with this row
  //   - Ctrl/⌘+click → toggle this row in / out of the existing set
  //   - Shift+click  → extend the range from the last anchor to this row
  // Shift-extend uses `sortedEntries` (the rendered order), not `entries`,
  // so the visual range matches what the user just dragged across.
  const onRowClick = (e: React.MouseEvent, entry: FileEntry, ordered: FileEntry[]) => {
    if (e.shiftKey && lastSelectedPathRef.current) {
      const a = ordered.findIndex(x => x.path === lastSelectedPathRef.current);
      const b = ordered.findIndex(x => x.path === entry.path);
      if (a >= 0 && b >= 0) {
        const [lo, hi] = a <= b ? [a, b] : [b, a];
        const range = ordered.slice(lo, hi + 1).map(x => x.path);
        setSelected(new Set([...selected, ...range]));
        return;
      }
    }
    if (e.ctrlKey || e.metaKey) {
      const next = new Set(selected);
      if (next.has(entry.path)) next.delete(entry.path);
      else next.add(entry.path);
      setSelected(next);
      lastSelectedPathRef.current = entry.path;
      return;
    }
    setSelected(new Set([entry.path]));
    lastSelectedPathRef.current = entry.path;
  };

  // Autocomplete suggestions for the path input — only when typing inside the
  // current directory, otherwise the dropdown becomes noise.
  const suggestions = (() => {
    if (!inputFocused) return [];
    if (tempInput.startsWith(currentPath)) {
      const suffix = tempInput.substring(currentPath.length).replace(/^[\\/]/, "");
      if (!suffix.includes("/") && !suffix.includes("\\")) {
        return entries.filter(e => e.name.toLowerCase().startsWith(suffix.toLowerCase()));
      }
    }
    return [];
  })();

  const pickSuggestion = (e: FileEntry) => {
    setTempInput(e.path);
    if (e.isDir) fetch(e.path);
    setInputFocused(false);
  };

  // ---- context menu auto-close ------------------------------------------------

  useEffect(() => {
    if (!contextMenu) return;
    const onWindowMouseDown = (ev: MouseEvent) => {
      const target = ev.target as Node | null;
      if (target && menuRef.current?.contains(target)) return;
      setContextMenu(null);
    };
    const timer = setTimeout(() => window.addEventListener("mousedown", onWindowMouseDown), 50);
    return () => {
      clearTimeout(timer);
      window.removeEventListener("mousedown", onWindowMouseDown);
    };
  }, [contextMenu]);

  const openMenu = (e: React.MouseEvent, entry: FileEntry) => {
    e.preventDefault();
    e.stopPropagation();
    const MENU_W = 200, MENU_H = 320;
    const x = Math.min(e.clientX, window.innerWidth - MENU_W - 4);
    const y = Math.min(e.clientY, window.innerHeight - MENU_H - 4);
    // Right-click on a row that isn't already part of the selection should
    // switch focus to it (Explorer/Finder behaviour). Right-click on a row
    // that IS selected keeps the multi-selection so bulk actions apply.
    if (!selected.has(entry.path)) {
      setSelected(new Set([entry.path]));
      lastSelectedPathRef.current = entry.path;
    }
    setContextMenu({ x: Math.max(4, x), y: Math.max(4, y), entry });
  };

  // ---- drag-source -------------------------------------------------------------

  const DRAG_START_THRESHOLD = 6;

  const handleRowMouseDown = (e: React.MouseEvent, entry: FileEntry) => {
    if (disabled || e.button !== 0) return;
    if (entry.isDir) return; // folder drag handled later

    const startX = e.clientX;
    const startY = e.clientY;
    let dragStarted = false;

    const onMove = (ev: MouseEvent) => {
      if (!dragStarted) {
        if (Math.abs(ev.clientX - startX) < DRAG_START_THRESHOLD &&
            Math.abs(ev.clientY - startY) < DRAG_START_THRESHOLD) return;
        dragStarted = true;
      }
      onDragMove({
        paneId: provider.id as "local" | "remote",
        entry,
        x: ev.clientX,
        y: ev.clientY,
      });
    };

    const onUp = () => {
      window.removeEventListener("mousemove", onMove);
      window.removeEventListener("mouseup", onUp);
      // Workspace listens for its own `mouseup` and uses the active drag (which
      // we keep up to date via `onDragMove`) to dispatch the cross-pane
      // transfer. We just clear our local indicator here.
      onDragMove(null);
    };

    window.addEventListener("mousemove", onMove);
    window.addEventListener("mouseup", onUp);
  };

  // (Removed dead listener for `sftp-sync-status-{id}` — the Rust side
  // never emits that event today, so the listener was just a leak. If
  // live-edit toasts get re-added, restore both ends together.)

  // ---- OS-level drag-drop into this pane --------------------------------------
  // Tauri 2 routes OS file drops through `tauri://drag-drop`; HTML5 drop events
  // fire too but their `File.path` is empty inside Tauri. We listen globally and
  // dispatch only when the cursor landed inside our root.

  useEffect(() => {
    if (!sessionId) return; // local pane doesn't need this
    let unlisten: (() => void) | null = null;
    listen<{ paths: string[]; position: { x: number; y: number } }>(
      "tauri://drag-drop",
      async (event) => {
        const { paths, position } = event.payload || ({} as any);
        if (!paths?.length || !dropTargetRef.current) return;
        const hit = document.elementFromPoint(position.x, position.y);
        if (!hit || !dropTargetRef.current.contains(hit)) return;
        setDragOver(false);
        const dir = currentPathRef.current;
        for (const p of paths) {
          const name = p.split(/[\\/]/).pop() || "file";
          notify(`Uploading ${name}...`, "info");
          try {
            await invoke("sftp_upload_file", {
              sessionId,
              localPath: p,
              remotePath: dir.endsWith("/") ? `${dir}${name}` : `${dir}/${name}`,
            });
            notify(`Uploaded ${name}`, "success");
          } catch (err: any) {
            notify(`Upload failed: ${err}`, "error");
          }
        }
        await fetch(dir);
      }
    ).then((fn) => { unlisten = fn; });
    return () => { if (unlisten) unlisten(); };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [sessionId]);

  // ---- modals -----------------------------------------------------------------

  const submitModal = async () => {
    if (!modal) return;
    const { type, entry, v1, v2 } = modal;
    try {
      if (type === "rename" && entry && v1) {
        const destDir = provider.parentPath(entry.path);
        const dest = provider.joinPath(destDir, v1);
        await provider.rename(entry.path, dest);
        notify(`Renamed to ${v1}`, "success");
      } else if (type === "move" && entry && v1) {
        // v1 is the absolute destination path the user typed.
        await provider.rename(entry.path, v1);
        notify(`Moved to ${v1}`, "success");
      } else if (type === "move-bulk" && v1) {
        // v1 is the destination *directory*; each selected item keeps its
        // own name under it.
        const dest = v1.replace(/[\\/]+$/, "");
        const items = sortedEntries.filter(e => selected.has(e.path));
        let count = 0;
        for (const it of items) {
          try {
            await provider.rename(it.path, provider.joinPath(dest, it.name));
            count++;
          } catch (err: any) {
            notify(`Move failed for ${it.name}: ${err}`, "error");
          }
        }
        if (count > 0) {
          setSelected(new Set());
          lastSelectedPathRef.current = null;
          notify(items.length === 1 ? `Moved ${items[0].name} → ${dest}` : `Moved ${count} of ${items.length} items → ${dest}`, "success");
        }
      } else if (type === "mkdir" && v1) {
        await provider.mkdir(provider.joinPath(currentPath, v1));
        notify(`Created ${v1}`, "success");
      } else if (type === "properties" && entry && v1 && provider.chmod) {
        const mode = parseInt(v1, 8);
        if (isNaN(mode)) throw new Error("Invalid octal mode");
        await provider.chmod(entry.path, mode);
        if (v2 && provider.chown) {
          const uid = parseInt(v2);
          if (!isNaN(uid)) await provider.chown(entry.path, uid, entry.gid ?? 0);
        }
        notify("Properties updated", "success");
      }
      await fetch(currentPath);
    } catch (err: any) {
      notify(`Failed: ${err}`, "error");
    } finally {
      setModal(null);
    }
  };

  // ---- removal ----------------------------------------------------------------

  const confirmDialog = useConfirm();

  // Bulk-aware delete. Pops a single confirm dialog regardless of count, then
  // applies provider.remove to each item in turn — best-effort: per-item
  // failures notify but don't halt the rest. Selection is cleared on success
  // so the user isn't left with stale paths highlighted.
  const removeItems = async (items: FileEntry[]) => {
    if (items.length === 0) return;
    const ok = await confirmDialog({
      title: items.length === 1 ? "Delete item" : `Delete ${items.length} items`,
      message: items.length === 1
        ? (items[0].isDir
            ? `Permanently delete folder “${items[0].name}” and everything inside?`
            : `Permanently delete “${items[0].name}”?`)
        : `Permanently delete ${items.length} items? Folders include their contents.`,
      okLabel: "Delete",
      destructive: true,
    });
    if (!ok) return;
    let count = 0;
    for (const it of items) {
      try { await provider.remove(it.path, it.isDir); count++; }
      catch (err: any) { notify(`Delete failed for ${it.name}: ${err}`, "error"); }
    }
    if (count > 0) {
      setSelected(new Set());
      lastSelectedPathRef.current = null;
      notify(items.length === 1 ? `Deleted ${items[0].name}` : `Deleted ${count} of ${items.length} items`, "success");
      await fetch(currentPath);
    }
  };

  // ---- contextual actions -----------------------------------------------------

  // Remote → local download. Bulk-aware. Destination is whatever the local
  // pane is currently showing (`getOppositeDir`) — that's almost always what
  // the user wants and saves the round-trip through a folder picker every
  // single time. Only falls back to the native picker if the sibling pane
  // hasn't reported a directory yet (e.g. it's still loading).
  const downloadItems = async (items: FileEntry[]) => {
    if (!sessionId) return;
    const files = items.filter(e => !e.isDir);
    if (files.length === 0) { notify("Folder download not supported", "error"); return; }
    let dest = getOppositeDir?.();
    if (!dest) {
      try { dest = (await invoke<string | null>("select_local_folder")) || undefined; }
      catch (err: any) { notify(`Pick folder failed: ${err}`, "error"); return; }
      if (!dest) return;
    }
    const sep = dest.includes("\\") ? "\\" : "/";
    const trimmed = dest.replace(/[\\/]+$/, "");
    notify(files.length === 1 ? `Downloading ${files[0].name}…` : `Downloading ${files.length} files…`, "info");
    let count = 0;
    for (const f of files) {
      try {
        await invoke("sftp_download_file", {
          sessionId, remotePath: f.path, localPath: `${trimmed}${sep}${f.name}`,
        });
        count++;
      } catch (err: any) {
        notify(`Download failed for ${f.name}: ${err}`, "error");
      }
    }
    if (count > 0) {
      notify(files.length === 1 ? `Downloaded ${files[0].name}` : `Downloaded ${count} of ${files.length} files`, "success");
    }
  };

  // Remote: open in OS default editor with a save-watcher that re-uploads on
  // every change. Backed by the existing `sftp_open_remote_file` command.
  const liveEditEntry = async (entry: FileEntry) => {
    if (!sessionId || entry.isDir) return;
    try {
      notify(`Opening ${entry.name} in default editor…`, "info");
      await invoke("sftp_open_remote_file", { sessionId, remotePath: entry.path });
    } catch (err: any) {
      notify(`Open failed: ${err}`, "error");
    }
  };

  // Local: open file in default OS application.
  const openLocalEntry = async (entry: FileEntry) => {
    if (entry.isDir) { fetch(entry.path); return; }
    try {
      await invoke("local_open_file", { localPath: entry.path });
    } catch (err: any) {
      notify(`Open failed: ${err}`, "error");
    }
  };

  // Local: reveal in OS file manager.
  const revealLocalEntry = async (entry: FileEntry) => {
    try {
      await invoke("local_open_in_explorer", { localPath: entry.path });
    } catch (err: any) {
      notify(`Reveal failed: ${err}`, "error");
    }
  };

  // ---- sorting ----------------------------------------------------------------

  const sortedEntries = (() => {
    return [...entries].sort((a, b) => {
      if (a.isDir !== b.isDir) return b.isDir ? 1 : -1;
      let va: any, vb: any;
      switch (sort.column) {
        case "name": va = a.name.toLowerCase(); vb = b.name.toLowerCase(); break;
        case "size": va = a.isDir ? -1 : a.size; vb = b.isDir ? -1 : b.size; break;
        case "modified": va = a.modified || 0; vb = b.modified || 0; break;
        case "permissions": va = a.permissions || 0; vb = b.permissions || 0; break;
      }
      if (va < vb) return sort.asc ? -1 : 1;
      if (va > vb) return sort.asc ? 1 : -1;
      return 0;
    });
  })();

  const toggleSort = (col: SortColumn) =>
    setSort((p) => ({ column: col, asc: p.column === col ? !p.asc : true }));

  const sortIcon = (col: SortColumn) => {
    if (sort.column !== col) return null;
    return sort.asc
      ? <ChevronUp size={11} className="inline ml-1 text-indigo-400" />
      : <ChevronDown size={11} className="inline ml-1 text-indigo-400" />;
  };

  // ---- HTML5 dragover for visual feedback during OS-level drop ----------------

  const onDragOver = (e: React.DragEvent) => { e.preventDefault(); setDragOver(true); };
  const onDragLeave = (e: React.DragEvent) => { e.preventDefault(); setDragOver(false); };
  const onDrop = (e: React.DragEvent) => { e.preventDefault(); setDragOver(false); };

  const isRemote = provider.id === "remote";
  const showPerms = isRemote; // local entries don't carry perms here

  return (
    <div
      data-fs-pane={provider.id}
      data-fs-current-path={currentPath}
      className="flex-1 flex flex-col h-full bg-[#09090b] p-1.5 gap-1.5 overflow-hidden relative select-none"
    >
      {disabled && (
        <div className="absolute inset-0 z-30 bg-black/55 backdrop-blur-[1px] flex items-center justify-center text-zinc-300 text-xs font-mono uppercase">
          <span className="px-3 py-1.5 bg-red-500/15 border border-red-500/30 rounded text-red-300">
            Session disconnected
          </span>
        </div>
      )}

      {notification && (
        <div className={`absolute top-3 right-3 z-50 px-3 py-1.5 rounded-lg border text-[11px] font-mono shadow-2xl backdrop-blur-md animate-in fade-in slide-in-from-top-4 duration-300 ${
          notification.type === "success" ? "bg-emerald-950/90 border-emerald-500/30 text-emerald-400" :
          notification.type === "error"   ? "bg-rose-950/90 border-rose-500/30 text-rose-400" :
                                            "bg-indigo-950/90 border-indigo-500/30 text-indigo-400"
        }`}>{notification.msg}</div>
      )}

      {/* Header */}
      <div className="w-full flex items-center justify-between gap-1.5 p-1.5 bg-[#121214] border border-white/5 rounded-lg shrink-0 shadow-lg">
        <span className="text-[10px] font-black uppercase tracking-widest text-zinc-300 px-1.5 shrink-0">
          {provider.label}
        </span>
        <div className="h-5 w-px bg-white/10 shrink-0" />
        <div className="flex-1 flex items-center gap-1.5 min-w-0 relative">
          <button onClick={goUp} title="Up" className="p-1 rounded bg-white/[0.04] border border-white/10 text-zinc-200 hover:bg-white/10 shrink-0">
            <ArrowUp size={11} />
          </button>
          <div className="flex-1 relative">
            <input
              type="text"
              value={tempInput}
              onChange={(e) => { setTempInput(e.target.value); setActiveSuggestion(-1); }}
              onFocus={() => setInputFocused(true)}
              onBlur={() => setTimeout(() => setInputFocused(false), 250)}
              onKeyDown={(e) => {
                if (e.key === "Enter") {
                  e.preventDefault();
                  if (activeSuggestion >= 0 && activeSuggestion < suggestions.length) {
                    pickSuggestion(suggestions[activeSuggestion]);
                  } else {
                    const target = tempInput.trim();
                    if (target) {
                      // Drop the autocomplete dropdown so the user clearly
                      // sees navigation kick off, and blur the input so
                      // browser default form-like behaviour doesn't kick in.
                      setInputFocused(false);
                      (e.currentTarget as HTMLInputElement).blur();
                      fetch(target);
                    }
                  }
                } else if (e.key === "ArrowDown" && suggestions.length > 0) {
                  e.preventDefault(); setActiveSuggestion((p) => (p + 1) % suggestions.length);
                } else if (e.key === "ArrowUp" && suggestions.length > 0) {
                  e.preventDefault(); setActiveSuggestion((p) => (p - 1 + suggestions.length) % suggestions.length);
                } else if (e.key === "Escape") setInputFocused(false);
              }}
              placeholder="Path…"
              className="w-full h-6 px-2 bg-white/[0.04] border border-white/10 rounded text-[11px] text-zinc-100 font-mono focus:outline-none focus:border-indigo-400/50 focus:bg-white/10"
            />
            {inputFocused && suggestions.length > 0 && (
              <div className="absolute top-[28px] left-0 right-0 max-h-[220px] overflow-y-auto z-50 bg-[#0c0c0e]/95 border border-white/10 rounded-lg shadow-2xl p-1 backdrop-blur-md font-mono text-[11px] text-zinc-200 no-scrollbar">
                {suggestions.map((s, idx) => (
                  <button key={s.path} onClick={() => pickSuggestion(s)}
                    className={`w-full flex items-center justify-between p-1.5 rounded text-left transition-colors ${
                      idx === activeSuggestion ? "bg-indigo-500/30 text-white font-bold" : "hover:bg-white/5 hover:text-white"
                    }`}>
                    <div className="flex items-center gap-2 truncate">
                      {s.isDir ? <Folder size={11} className="text-indigo-300 shrink-0" /> : <File size={11} className="text-zinc-500 shrink-0" />}
                      <span className="truncate">{s.name}</span>
                    </div>
                    {s.isDir && <span className="text-[9px] bg-indigo-500/20 text-indigo-300 px-1 rounded">dir</span>}
                  </button>
                ))}
              </div>
            )}
          </div>
          <button onClick={() => fetch(currentPath)} title="Refresh"
            className={`p-1 rounded bg-white/[0.04] border border-white/10 text-zinc-200 hover:bg-white/10 shrink-0 ${loading ? "animate-spin" : ""}`}>
            <RefreshCw size={11} />
          </button>
        </div>
        <div className="h-5 w-px bg-white/10 shrink-0" />
        <div className="flex items-center gap-1 shrink-0">
          {provider.id === "local" && (
            <button
              onClick={async () => {
                try {
                  const picked = await invoke<string | null>("select_local_folder");
                  if (picked) await fetch(picked);
                } catch (err: any) {
                  notify(`Browse failed: ${err}`, "error");
                }
              }}
              title="Browse for folder"
              className="p-1 rounded bg-white/[0.04] border border-white/10 text-emerald-300 hover:bg-white/10"
            >
              <FolderSearch size={11} />
            </button>
          )}
          <button onClick={() => setModal({ type: "mkdir", v1: "" })} title="New Folder"
            className="p-1 rounded bg-white/[0.04] border border-white/10 text-indigo-300 hover:bg-white/10">
            <Folder size={11} />
          </button>
        </div>
      </div>

      {/* Bulk action bar — only when ≥2 rows are selected. Single-selection
          users get the same actions via right-click; the bar exists so
          multi-select doesn't force a right-click round-trip. */}
      {selected.size > 1 && (
        <div className="shrink-0 flex items-center gap-1.5 px-2 py-1 bg-indigo-950/40 border border-indigo-500/30 rounded text-[10.5px] text-indigo-100 font-mono">
          <span className="font-bold uppercase tracking-wider text-[9.5px] text-indigo-300 shrink-0">
            {selected.size} selected
          </span>
          <div className="flex-1" />
          {isRemote && (
            <button
              onClick={() => downloadItems(sortedEntries.filter(e => selected.has(e.path)))}
              title={getOppositeDir?.()
                ? `Download to ${getOppositeDir!()}`
                : "Pick a destination folder…"}
              className="px-2 py-0.5 rounded bg-emerald-500/15 text-emerald-300 hover:bg-emerald-500/25 flex items-center gap-1"
            >
              <Download size={10} /> Download
            </button>
          )}
          <button
            onClick={() => setModal({ type: "move-bulk", v1: currentPath })}
            className="px-2 py-0.5 rounded bg-white/5 text-zinc-200 hover:bg-white/10 flex items-center gap-1"
          >
            <Move size={10} /> Move…
          </button>
          <button
            onClick={() => removeItems(sortedEntries.filter(e => selected.has(e.path)))}
            className="px-2 py-0.5 rounded bg-rose-500/15 text-rose-300 hover:bg-rose-500/25 flex items-center gap-1"
          >
            <Trash2 size={10} /> Delete
          </button>
          <button
            onClick={() => { setSelected(new Set()); lastSelectedPathRef.current = null; }}
            className="px-2 py-0.5 rounded bg-white/5 text-zinc-400 hover:bg-white/10 flex items-center gap-1"
          >
            <X size={10} /> Clear
          </button>
        </div>
      )}

      {/* List */}
      <div
        ref={dropTargetRef}
        onDragOver={onDragOver}
        onDragLeave={onDragLeave}
        onDrop={onDrop}
        className={`flex-1 border rounded-lg bg-[#121214] flex flex-col overflow-auto transition-all duration-200 border-indigo-500/30 shadow-2xl shadow-indigo-950/10 ${dragOver ? "border-indigo-400 bg-indigo-950/10" : ""}`}
      >
        <div className={`min-w-full grid ${showPerms ? "grid-cols-[minmax(180px,1fr)_65px_115px_85px]" : "grid-cols-[minmax(180px,1fr)_75px_125px]"} gap-1.5 px-2.5 bg-[#161619] border-b border-white/5 font-mono text-[10.5px] text-zinc-300 select-none font-bold shrink-0 sticky top-0 z-10 shadow-md`}>
          <div className="bg-[#161619] cursor-pointer hover:text-white py-1.5" onClick={() => toggleSort("name")}>
            NAME {sortIcon("name")}
          </div>
          <div className="bg-[#161619] cursor-pointer hover:text-white text-right py-1.5" onClick={() => toggleSort("size")}>
            SIZE {sortIcon("size")}
          </div>
          <div className="bg-[#161619] cursor-pointer hover:text-white text-right py-1.5" onClick={() => toggleSort("modified")}>
            CHANGED {sortIcon("modified")}
          </div>
          {showPerms && (
            <div className="bg-[#161619] cursor-pointer hover:text-white text-right py-1.5" onClick={() => toggleSort("permissions")}>
              RIGHTS {sortIcon("permissions")}
            </div>
          )}
        </div>

        <div className="min-w-full p-1 font-mono text-[11px]"
             onClick={(e) => {
               // Click landed on the bare list background (not on a row, since
               // rows stopPropagation via their own onClick chain implicitly
               // by being the click target). Clear selection so users can
               // escape a multi-selection without a keyboard shortcut.
               if (e.target === e.currentTarget) {
                 setSelected(new Set());
                 lastSelectedPathRef.current = null;
               }
             }}>
          {loading && sortedEntries.length === 0 ? (
            <div className="text-center py-14 text-zinc-400">Loading…</div>
          ) : sortedEntries.length === 0 ? (
            <div className="text-center py-14 text-zinc-500">Empty</div>
          ) : (
            sortedEntries.map((entry) => (
              <div
                key={entry.path}
                onMouseDown={(e) => handleRowMouseDown(e, entry)}
                onContextMenu={(e) => openMenu(e, entry)}
                onClick={(e) => onRowClick(e, entry, sortedEntries)}
                onDoubleClick={() => {
                  if (entry.isDir) { fetch(entry.path); return; }
                  // For files: remote → live-edit (download + open editor +
                  // auto-upload on save); local → open in the OS default app.
                  if (isRemote) liveEditEntry(entry);
                  else openLocalEntry(entry);
                }}
                className={`grid ${showPerms ? "grid-cols-[minmax(180px,1fr)_65px_115px_85px]" : "grid-cols-[minmax(180px,1fr)_75px_125px]"} gap-1.5 px-2.5 py-1 border-l-2 cursor-pointer transition-colors items-center ${
                  selected.has(entry.path)
                    ? "bg-indigo-950/40 border-indigo-400 text-indigo-100 font-bold"
                    : "border-transparent text-zinc-200 hover:bg-white/5 hover:text-white"
                }`}
              >
                <div className="flex items-center gap-2 truncate pr-1">
                  {entry.isDir
                    ? <Folder size={12} className="text-indigo-300 shrink-0" />
                    : <File size={12} className="text-zinc-500 shrink-0" />}
                  <span className="truncate text-zinc-100 text-[11px]">{entry.name}</span>
                </div>
                <div className="text-right text-[10.5px] text-zinc-300 font-sans">
                  {entry.isDir ? "" : formatSize(entry.size)}
                </div>
                <div className="text-right text-[9.5px] text-zinc-400 truncate">
                  {formatTime(entry.modified)}
                </div>
                {showPerms && (
                  <div className="text-right text-[10.5px] text-zinc-300 font-mono opacity-90 flex items-center justify-end gap-1">
                    <span className="truncate">{formatRights(entry.isDir, entry.permissions)}</span>
                    <button onClick={(e) => openMenu(e, entry)} title="Options"
                      className="opacity-60 hover:opacity-100 p-0.5 rounded hover:bg-white/10 text-zinc-400 hover:text-white shrink-0">
                      <MoreVertical size={11} />
                    </button>
                  </div>
                )}
              </div>
            ))
          )}
        </div>
      </div>

      {/* Context menu (portal) — bulk-aware. When the right-click anchor is
          part of a multi-selection, actions like Download / Move / Delete
          apply to the whole set; per-item actions (Rename, Properties,
          Edit) only show when exactly one row is selected. */}
      {contextMenu && createPortal((() => {
        const selectedEntries = sortedEntries.filter(e => selected.has(e.path));
        const acting = selectedEntries.length > 0 ? selectedEntries : [contextMenu.entry];
        const multi = acting.length > 1;
        const selectedFileCount = acting.filter(e => !e.isDir).length;
        return (
        <div ref={menuRef}
          style={{ top: contextMenu.y, left: contextMenu.x }}
          className="fixed z-[9999] min-w-[180px] bg-[#0c0c0e] border border-white/10 rounded-lg shadow-2xl p-1 backdrop-blur-md font-mono text-[11.5px] text-zinc-200">

          {/* Primary action: open folder, or transfer/edit file. The exact
              set depends on which side this panel is on. */}
          {contextMenu.entry.isDir && !multi ? (
            <button onClick={() => { setContextMenu(null); fetch(contextMenu.entry.path); }}
              className="w-full flex items-center gap-2 p-1.5 rounded hover:bg-white/10 text-left hover:text-white">
              <Folder size={11} className="text-indigo-400" /><span>Open</span>
            </button>
          ) : isRemote ? (
            <>
              <button onClick={() => { setContextMenu(null); downloadItems(acting); }}
                className="w-full flex items-center gap-2 p-1.5 rounded hover:bg-white/10 text-left hover:text-white">
                <Download size={11} className="text-emerald-400" />
                <span>Download{multi ? ` (${selectedFileCount})` : "…"}</span>
              </button>
              {!multi && !contextMenu.entry.isDir && (
                <button onClick={() => { setContextMenu(null); liveEditEntry(contextMenu.entry); }}
                  className="w-full flex items-center gap-2 p-1.5 rounded hover:bg-white/10 text-left hover:text-white">
                  <ExternalLink size={11} className="text-indigo-400" /><span>Edit (auto-upload)</span>
                </button>
              )}
            </>
          ) : (
            !multi && !contextMenu.entry.isDir && (
              <button onClick={() => { setContextMenu(null); openLocalEntry(contextMenu.entry); }}
                className="w-full flex items-center gap-2 p-1.5 rounded hover:bg-white/10 text-left hover:text-white">
                <ExternalLink size={11} className="text-indigo-400" /><span>Open</span>
              </button>
            )
          )}

          {!isRemote && !multi && (
            <button onClick={() => { setContextMenu(null); revealLocalEntry(contextMenu.entry); }}
              className="w-full flex items-center gap-2 p-1.5 rounded hover:bg-white/10 text-left hover:text-white">
              <FolderSearch size={11} className="text-emerald-300" /><span>Reveal in Explorer</span>
            </button>
          )}

          <div className="h-px bg-white/5 my-1" />

          <button onClick={() => { setContextMenu(null); setModal({ type: "mkdir", v1: "" }); }}
            className="w-full flex items-center gap-2 p-1.5 rounded hover:bg-white/10 text-left hover:text-white">
            <Plus size={11} className="text-indigo-300" /><span>New Folder</span>
          </button>
          {!multi && (
            <button onClick={() => { setContextMenu(null); setModal({ type: "rename", entry: contextMenu.entry, v1: contextMenu.entry.name }); }}
              className="w-full flex items-center gap-2 p-1.5 rounded hover:bg-white/10 text-left hover:text-white">
              <Edit3 size={11} /><span>Rename</span>
            </button>
          )}
          <button onClick={() => {
              setContextMenu(null);
              if (multi) setModal({ type: "move-bulk", v1: currentPath });
              else setModal({ type: "move", entry: contextMenu.entry, v1: contextMenu.entry.path });
            }}
            className="w-full flex items-center gap-2 p-1.5 rounded hover:bg-white/10 text-left hover:text-white">
            <Move size={11} /><span>{multi ? `Move (${acting.length}) to…` : "Move to…"}</span>
          </button>
          {!multi && provider.chmod && (
            <button onClick={() => { setContextMenu(null); setModal({ type: "properties", entry: contextMenu.entry, v1: (contextMenu.entry.permissions ? (contextMenu.entry.permissions & 0o777).toString(8) : "755"), v2: contextMenu.entry.uid?.toString() }); }}
              className="w-full flex items-center gap-2 p-1.5 rounded hover:bg-white/10 text-left hover:text-white">
              <Shield size={11} /><span>Properties</span>
            </button>
          )}

          <div className="h-px bg-white/5 my-1" />
          <button onClick={() => { setContextMenu(null); removeItems(acting); }}
            className="w-full flex items-center gap-2 p-1.5 rounded hover:bg-rose-950/20 text-left text-rose-400">
            <Trash2 size={11} /><span>Delete{multi ? ` (${acting.length})` : ""}</span>
          </button>
        </div>
        );
      })(),
        document.body
      )}

      {/* Modal */}
      {modal && (
        <div className="fixed inset-0 z-[9998] bg-black/60 backdrop-blur-sm flex items-center justify-center p-4" onClick={() => setModal(null)}>
          <div onClick={(e) => e.stopPropagation()}
            className="w-full max-w-[320px] bg-[#121214] border border-white/5 rounded-xl shadow-2xl p-4 font-mono text-[11px]">
            <div className="flex items-center justify-between mb-3">
              <span className="font-black uppercase tracking-wider text-zinc-300">
                {modal.type === "rename" ? "Rename" :
                 modal.type === "mkdir" ? "New Directory" :
                 modal.type === "move" ? "Move to…" :
                 modal.type === "move-bulk" ? `Move ${selected.size} items to…` : "Properties"}
              </span>
              <button onClick={() => setModal(null)} className="text-zinc-500 hover:text-white"><X size={12} /></button>
            </div>
            <div className="space-y-3">
              {modal.type === "properties" ? (
                <>
                  <div>Name: <span className="text-zinc-100 font-bold">{modal.entry?.name}</span></div>
                  <div>Path: <span className="text-zinc-400 text-[10px] block truncate">{modal.entry?.path}</span></div>

                  {/* RWX matrix — owner/group/other × read/write/execute. The
                      checkbox grid is the source of truth; the octal input
                      below mirrors it and accepts manual edits both ways. */}
                  {(() => {
                    const parsed = parseInt(modal.v1 || "0", 8);
                    const mode = isNaN(parsed) ? 0 : parsed & 0o777;
                    const roles: { key: "owner" | "group" | "other"; label: string; shift: number }[] = [
                      { key: "owner", label: "Owner", shift: 6 },
                      { key: "group", label: "Group", shift: 3 },
                      { key: "other", label: "Other", shift: 0 },
                    ];
                    const bits: { key: "r" | "w" | "x"; label: string; bit: number }[] = [
                      { key: "r", label: "R", bit: 4 },
                      { key: "w", label: "W", bit: 2 },
                      { key: "x", label: "X", bit: 1 },
                    ];
                    const toggle = (shift: number, bit: number) => {
                      const next = mode ^ (bit << shift);
                      setModal({ ...modal, v1: (next & 0o777).toString(8).padStart(3, "0") });
                    };
                    return (
                      <div className="pt-1">
                        <label className="text-[10px] text-zinc-400 block mb-1.5 uppercase tracking-wider">Permissions</label>
                        <div className="grid grid-cols-[60px_repeat(3,1fr)] gap-1 text-center text-[10px] text-zinc-400 font-mono">
                          <div />
                          {bits.map((b) => <div key={b.key} className="font-bold">{b.label}</div>)}
                          {roles.map((role) => (
                            <React.Fragment key={role.key}>
                              <div className="text-left text-zinc-300 self-center">{role.label}</div>
                              {bits.map((b) => {
                                const on = ((mode >> role.shift) & b.bit) !== 0;
                                return (
                                  <button
                                    key={b.key}
                                    type="button"
                                    onClick={() => toggle(role.shift, b.bit)}
                                    className={`h-6 rounded border transition-colors ${
                                      on
                                        ? "bg-indigo-500/20 border-indigo-400/40 text-indigo-200"
                                        : "bg-white/[0.04] border-white/10 text-zinc-500 hover:bg-white/[0.08]"
                                    }`}
                                  >
                                    {on ? "✓" : ""}
                                  </button>
                                );
                              })}
                            </React.Fragment>
                          ))}
                        </div>
                      </div>
                    );
                  })()}

                  <div className="flex gap-2 pt-1">
                    <div className="flex-1">
                      <label className="text-[10px] text-zinc-400 block mb-1">Octal</label>
                      <input type="text" value={modal.v1 || ""}
                        onChange={(e) => {
                          // Only accept 0–3 digits, each 0–7 — anything else is
                          // ignored so the checkbox grid never sees garbage.
                          const v = e.target.value.replace(/[^0-7]/g, "").slice(0, 3);
                          setModal({ ...modal, v1: v });
                        }}
                        className="w-full h-7 px-2 bg-white/5 border border-white/5 rounded text-zinc-200 focus:outline-none focus:border-indigo-400/40 text-[11.5px] font-mono" />
                    </div>
                    <div className="flex-1">
                      <label className="text-[10px] text-zinc-400 block mb-1">Owner UID</label>
                      <input type="text" value={modal.v2 || ""} onChange={(e) => setModal({ ...modal, v2: e.target.value })}
                        className="w-full h-7 px-2 bg-white/5 border border-white/5 rounded text-zinc-200 focus:outline-none focus:border-indigo-400/40 text-[11.5px]" />
                    </div>
                  </div>
                </>
              ) : (
                <div>
                  <label className="text-[10px] text-zinc-400 block mb-1">
                    {modal.type === "move" ? "Destination path" :
                     modal.type === "move-bulk" ? "Destination directory" : "Name"}
                  </label>
                  <input type="text" autoFocus value={modal.v1 || ""}
                    onChange={(e) => setModal({ ...modal, v1: e.target.value })}
                    onKeyDown={(e) => e.key === "Enter" && submitModal()}
                    className="w-full h-7 px-2 bg-white/5 border border-white/5 rounded text-zinc-200 focus:outline-none focus:border-indigo-400/40 text-[11.5px] font-mono" />
                </div>
              )}
              <div className="flex gap-2 justify-end pt-2">
                <button onClick={() => setModal(null)} className="px-3 h-7 rounded border border-white/5 text-zinc-400 hover:text-white">Cancel</button>
                <button onClick={submitModal} className="px-3 h-7 rounded bg-indigo-500 text-white font-bold hover:bg-indigo-600">Apply</button>
              </div>
            </div>
          </div>
        </div>
      )}
    </div>
  );
});

FilePanel.displayName = "FilePanel";

export default FilePanel;
