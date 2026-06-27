import React, { useState, useEffect, useRef, useImperativeHandle, forwardRef } from "react";
import { createPortal } from "react-dom";
import { listen } from "@tauri-apps/api/event";
import { invoke } from "@tauri-apps/api/core";
import {
  Folder, File, ArrowUp, RefreshCw, Trash2, Edit3, Shield,
  X, ChevronUp, ChevronDown, Plus, MoreVertical, FolderSearch,
  Download, Upload, ExternalLink, Move, CheckSquare, Square, Search,
} from "lucide-react";
import { FileEntry, FileProvider } from "../fs/types";
import { useConfirm, useOverwritePrompt, OverwriteChoice } from "../ui/confirm";

// Batch overwrite state shared across items in a single download/upload run.
// Once the user picks "Overwrite all" or "Skip all" the kind is sticky and we
// stop prompting; "ask" means we prompt on each individual conflict. Mutated
// in place inside the helper so the loop sees updates immediately.
type OverwriteBatchKind = "ask" | "overwrite-all" | "skip-all";
interface OverwriteBatch { kind: OverwriteBatchKind; }

// Run a single transfer that may trip the backend's `EXISTS:<path>` sentinel.
// First attempt is always with overwrite=false unless the batch state already
// says "overwrite all". On EXISTS: consult batch state, prompt the user if
// needed, then either retry with overwrite=true or skip.
async function transferWithOverwriteCheck(
  invokeFn: (overwrite: boolean) => Promise<void>,
  name: string,
  direction: "download" | "upload",
  batchSize: number,
  batch: OverwriteBatch,
  overwritePrompt: (opts: { name: string; direction: "download" | "upload"; batchSize: number }) => Promise<OverwriteChoice>,
): Promise<"done" | "skipped" | "cancelled"> {
  if (batch.kind === "overwrite-all") {
    await invokeFn(true);
    return "done";
  }
  try {
    await invokeFn(false);
    return "done";
  } catch (err: any) {
    const msg = String(err);
    if (!msg.startsWith("EXISTS:")) throw err;
    if (batch.kind === "skip-all") return "skipped";
    const choice = await overwritePrompt({ name, direction, batchSize });
    if (choice === "cancel") return "cancelled";
    if (choice === "skip") return "skipped";
    if (choice === "skip-all") { batch.kind = "skip-all"; return "skipped"; }
    if (choice === "overwrite-all") batch.kind = "overwrite-all";
    await invokeFn(true);
    return "done";
  }
}

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
  // Last five distinct directories visited in this panel, MRU first. Lives
  // in component state (cleared per session) — persistence didn't seem
  // worth the complexity given users usually want recents from this work
  // session, not whatever they were doing last week. Updated from `fetch`
  // when navigation succeeds.
  const [recentDirs, setRecentDirs] = useState<string[]>([]);
  const [recentOpen, setRecentOpen] = useState(false);
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
  // Name filter applied AFTER sort — substring, case-insensitive. Kept
  // separate from the path bar so the user can leave a filter active while
  // typing into the path. Sticky across `cd` so a quick filter session can
  // span sibling dirs; the X button or Escape on the input clears it.
  const [nameFilter, setNameFilter] = useState("");

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

  const pushRecent = (path: string) => {
    if (!path) return;
    setRecentDirs((prev) => {
      const next = [path, ...prev.filter((p) => p !== path)];
      return next.slice(0, 5);
    });
  };

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
      pushRecent(result.currentPath);
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
        pushRecent(result.currentPath);
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

  // Forward-walking autocomplete. As the user types a path, look at the
  // segment AFTER the last separator and prefix-match it against whichever
  // directory's listing applies. Three sources, in order of preference:
  //   1. typed path's parent matches the pane's current directory → use
  //      the already-loaded `entries` (free, no SFTP roundtrip).
  //   2. typed parent matches a previously-prefetched lookahead → reuse
  //      that cached listing.
  //   3. typed parent is somewhere else → kick a debounced provider.list
  //      to load it (effect below), then case 2 lights up.
  const [lookaheadParent, setLookaheadParent] = useState<string>("");
  const [lookaheadEntries, setLookaheadEntries] = useState<FileEntry[]>([]);

  // Split the typed input into (parent_dir, leaf_prefix) using either '/'
  // or '\' as the separator so Windows local paths work too.
  const splitInputPath = (input: string): { parent: string; leaf: string } | null => {
    const sep = Math.max(input.lastIndexOf('/'), input.lastIndexOf('\\'));
    if (sep < 0) return null;
    // Keep the trailing separator on the parent so "/var/" parses as
    // parent=/var/, leaf="" — that's what makes typing the slash trigger
    // a fresh listing of the directory below.
    const parent = input.substring(0, sep + 1);
    const leaf = input.substring(sep + 1);
    return { parent, leaf };
  };

  // Prefetch listings for typed paths outside the current directory. Debounced
  // so a fast typist doesn't fire one SFTP request per keystroke; skipped
  // when the typed parent already matches currentPath (free from `entries`).
  useEffect(() => {
    if (!inputFocused) return;
    const split = splitInputPath(tempInput);
    if (!split) return;
    // Normalize: most providers report paths without the trailing '/'.
    const probe = split.parent.replace(/[\\/]+$/, "") || "/";
    if (probe === currentPath || probe === lookaheadParent) return;
    const t = setTimeout(async () => {
      try {
        const result = await provider.list(probe);
        setLookaheadEntries(result.entries);
        setLookaheadParent(result.currentPath);
      } catch { /* parent doesn't exist (yet) — suggestions just stay empty */ }
    }, 220);
    return () => clearTimeout(t);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [tempInput, inputFocused, currentPath]);

  const suggestions = (() => {
    if (!inputFocused) return [];
    const split = splitInputPath(tempInput);
    if (!split) return [];
    const probe = split.parent.replace(/[\\/]+$/, "") || "/";
    const source =
      probe === currentPath      ? entries :
      probe === lookaheadParent  ? lookaheadEntries :
                                   [];
    const leafLower = split.leaf.toLowerCase();
    return source.filter(e => e.name.toLowerCase().startsWith(leafLower));
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

  // Live-edit toast. `sftp_open_remote_file` downloads the file to a temp
  // dir, opens it in the system editor, and watches the file's mtime.
  // Every time the user saves, Rust pushes the change back to the server
  // and emits `sftp-sync-status-{id}`. Surface the success path as a
  // notify() so the user sees their save actually landed; errors get the
  // full text from the backend.
  useEffect(() => {
    if (!sessionId) return;
    let unlisten: (() => void) | null = null;
    listen<{ status: string; message?: string }>(
      `sftp-sync-status-${sessionId}`,
      (event) => {
        const { status, message } = event.payload || ({} as any);
        if (status === "success") {
          notify(message || "Changes uploaded", "success");
        } else if (status === "error") {
          notify(message || "Auto-sync failed", "error");
        }
      },
    ).then((u) => { unlisten = u; });
    return () => { if (unlisten) unlisten(); };
  }, [sessionId]);

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
        const batch: OverwriteBatch = { kind: "ask" };
        let cancelled = false;
        for (const p of paths) {
          if (cancelled) break;
          const name = p.split(/[\\/]/).pop() || "file";
          notify(`Uploading ${name}...`, "info");
          try {
            const remote = dir.endsWith("/") ? `${dir}${name}` : `${dir}/${name}`;
            const res = await transferWithOverwriteCheck(
              (ow) => invoke("sftp_upload_file", { sessionId, localPath: p, remotePath: remote, overwrite: ow }),
              name, "upload", paths.length, batch, overwritePrompt
            );
            if (res === "done") notify(`Uploaded ${name}`, "success");
            if (res === "cancelled") { cancelled = true; notify("Upload batch cancelled", "info"); }
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
        // v1 is the destination *directory* the user typed (matches the
        // bulk-move semantics so users have a single mental model). We
        // append the entry's own name so the item keeps its filename. To
        // change the filename, the user picks "Rename" instead.
        const destDir = v1.replace(/[\\/]+$/, "");
        const dest = provider.joinPath(destDir, entry.name);
        if (dest === entry.path) throw new Error("Destination is the current location — nothing to move.");
        await provider.rename(entry.path, dest);
        notify(`Moved ${entry.name} → ${destDir}`, "success");
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
  const overwritePrompt = useOverwritePrompt();

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

  // Remote → local download. Bulk-aware, folder-aware. Destination is
  // whatever the local pane is currently showing (`getOppositeDir`) — that's
  // almost always what the user wants and saves the round-trip through a
  // folder picker every single time. Only falls back to the native picker
  // if the sibling pane hasn't reported a directory yet (e.g. it's still
  // loading). Folders go through `sftp_download_dir` which walks the tree
  // and preserves structure; individual files go through the single-file
  // command. Both paths emit progress on the same `sftp-transfer-{id}`
  // channel so the user sees uniform cards.
  const downloadItems = async (items: FileEntry[]) => {
    if (!sessionId || items.length === 0) return;
    let dest = getOppositeDir?.();
    if (!dest) {
      try { dest = (await invoke<string | null>("select_local_folder")) || undefined; }
      catch (err: any) { notify(`Pick folder failed: ${err}`, "error"); return; }
      if (!dest) return;
    }
    const sep = dest.includes("\\") ? "\\" : "/";
    const trimmed = dest.replace(/[\\/]+$/, "");
    const fileCount = items.filter(e => !e.isDir).length;
    const dirCount  = items.filter(e =>  e.isDir).length;
    const summary =
      items.length === 1
        ? `Downloading ${items[0].isDir ? "folder " : ""}${items[0].name}…`
        : `Downloading ${fileCount} files${dirCount ? ` + ${dirCount} folder${dirCount === 1 ? "" : "s"}` : ""}…`;
    notify(summary, "info");
    const batch: OverwriteBatch = { kind: "ask" };
    let count = 0;
    let cancelled = false;
    for (const e of items) {
      if (cancelled) break;
      try {
        const dest = e.isDir ? trimmed : `${trimmed}${sep}${e.name}`;
        const cmd = e.isDir ? "sftp_download_dir" : "sftp_download_file";
        const res = await transferWithOverwriteCheck(
          (ow) => invoke(cmd, { sessionId, remotePath: e.path, localPath: dest, overwrite: ow }),
          e.name, "download", items.length, batch, overwritePrompt
        );
        if (res === "done") count++;
        if (res === "cancelled") { cancelled = true; }
      } catch (err: any) {
        notify(`Download failed for ${e.name}: ${err}`, "error");
      }
    }
    if (count > 0) {
      notify(items.length === 1 ? `Downloaded ${items[0].name}` : `Downloaded ${count} of ${items.length} items`, "success");
    } else if (cancelled) {
      notify("Download batch cancelled", "info");
    }
  };

  // Local → remote upload. Mirror of downloadItems — handles both files
  // (sftp_upload_file) and directories (sftp_upload_dir recursive walk).
  // Destination is whatever directory the remote pane is showing; if the
  // remote pane hasn't reported one yet (still loading), we bail with a
  // clear error rather than guessing the home dir.
  const uploadItems = async (items: FileEntry[]) => {
    if (!sessionId || items.length === 0) return;
    const dest = getOppositeDir?.();
    if (!dest) { notify("Open a directory in the remote pane first.", "error"); return; }
    const trimmed = dest.replace(/[\\/]+$/, "");
    const fileCount = items.filter((e) => !e.isDir).length;
    const dirCount = items.filter((e) => e.isDir).length;
    const summary = items.length === 1
      ? `Uploading ${items[0].isDir ? "folder " : ""}${items[0].name}…`
      : `Uploading ${fileCount} file${fileCount === 1 ? "" : "s"}${dirCount ? ` + ${dirCount} folder${dirCount === 1 ? "" : "s"}` : ""}…`;
    notify(summary, "info");
    const batch: OverwriteBatch = { kind: "ask" };
    let count = 0;
    let cancelled = false;
    for (const e of items) {
      if (cancelled) break;
      try {
        // For files we pass the remote target as a full file path; for dirs
        // we pass the remote PARENT and sftp_upload_dir hangs the source
        // basename underneath it (same convention as sftp_download_dir).
        const remotePath = e.isDir ? trimmed : `${trimmed}/${e.name}`;
        const cmd = e.isDir ? "sftp_upload_dir" : "sftp_upload_file";
        const res = await transferWithOverwriteCheck(
          (ow) => invoke(cmd, { sessionId, localPath: e.path, remotePath, overwrite: ow }),
          e.name, "upload", items.length, batch, overwritePrompt
        );
        if (res === "done") count++;
        if (res === "cancelled") { cancelled = true; }
      } catch (err: any) {
        notify(`Upload failed for ${e.name}: ${err}`, "error");
      }
    }
    if (count > 0) {
      notify(items.length === 1 ? `Uploaded ${items[0].name}` : `Uploaded ${count} of ${items.length} items`, "success");
    } else if (cancelled) {
      notify("Upload batch cancelled", "info");
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
    const sorted = [...entries].sort((a, b) => {
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
    const needle = nameFilter.trim().toLowerCase();
    if (!needle) return sorted;
    return sorted.filter(e => e.name.toLowerCase().includes(needle));
  })();
  const filteredOut = nameFilter.trim() ? entries.length - sortedEntries.length : 0;

  // ---- select-all -------------------------------------------------------------
  // Toggles the entire visible (sorted) list. Computed AFTER `sortedEntries`
  // so the const TDZ doesn't fire on first render. Cmd/Ctrl+A is the keyboard
  // counterpart; we let the browser handle Ctrl+A in text inputs by bailing
  // out when the focused element is editable.
  const selectAllVisible = () => {
    if (sortedEntries.length === 0) return;
    setSelected(new Set(sortedEntries.map(e => e.path)));
    lastSelectedPathRef.current = sortedEntries[sortedEntries.length - 1].path;
  };
  const clearSelection = () => {
    setSelected(new Set());
    lastSelectedPathRef.current = null;
  };
  const allSelected = sortedEntries.length > 0 && selected.size === sortedEntries.length;
  const toggleSelectAll = () => { allSelected ? clearSelection() : selectAllVisible(); };

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (!(e.ctrlKey || e.metaKey) || e.key.toLowerCase() !== "a") return;
      const t = e.target as HTMLElement | null;
      if (t && (t.tagName === "INPUT" || t.tagName === "TEXTAREA" || t.isContentEditable)) return;
      // Only fire when this panel owns the focus, so two side-by-side
      // FilePanel instances don't both select all on the same press.
      const root = dropTargetRef.current;
      if (root && document.activeElement && !root.contains(document.activeElement)) return;
      e.preventDefault();
      toggleSelectAll();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [sortedEntries.length, allSelected]);

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
        // Bottom-right of the pane, not top-right — top-right used to sit
        // on top of the path-bar header and obscure the first row of
        // entries on a short pane. Bottom keeps the file list visible.
        <div className={`absolute bottom-3 right-3 z-50 max-w-[80%] px-3 py-1.5 rounded-lg border text-[11px] font-mono shadow-2xl backdrop-blur-md animate-in fade-in slide-in-from-bottom-4 duration-300 ${
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
          <div className="relative shrink-0">
            <button
              onClick={() => setRecentOpen((p) => !p)}
              onBlur={() => setTimeout(() => setRecentOpen(false), 200)}
              disabled={recentDirs.filter((p) => p !== currentPath).length === 0}
              title="Recent directories"
              className="p-1 rounded bg-white/[0.04] border border-white/10 text-zinc-200 hover:bg-white/10 disabled:opacity-30 disabled:cursor-not-allowed flex items-center"
            >
              <ChevronDown size={11} />
            </button>
            {recentOpen && (
              <div className="absolute top-[28px] left-0 z-50 min-w-[220px] max-h-[220px] overflow-y-auto bg-[#0c0c0e]/95 border border-white/10 rounded-lg shadow-2xl p-1 backdrop-blur-md font-mono text-[11px] text-zinc-200 no-scrollbar">
                <div className="px-2 py-1 text-[9px] uppercase tracking-wider text-zinc-500 font-bold">Recent</div>
                {recentDirs
                  .filter((p) => p !== currentPath)
                  .map((p) => (
                    <button
                      key={p}
                      onMouseDown={(e) => { e.preventDefault(); setRecentOpen(false); fetch(p); }}
                      className="w-full flex items-center gap-2 p-1.5 rounded text-left hover:bg-white/10 hover:text-white truncate"
                      title={p}
                    >
                      <Folder size={11} className="text-indigo-300 shrink-0" />
                      <span className="truncate">{p}</span>
                    </button>
                  ))}
              </div>
            )}
          </div>
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
          <button
            onClick={toggleSelectAll}
            disabled={sortedEntries.length === 0}
            title={allSelected ? "Deselect all (Ctrl+A)" : "Select all (Ctrl+A)"}
            className={`p-1 rounded border border-white/10 hover:bg-white/10 disabled:opacity-30 ${
              allSelected
                ? "bg-indigo-500/20 text-indigo-200"
                : "bg-white/[0.04] text-zinc-300"
            }`}
          >
            {allSelected ? <CheckSquare size={11} /> : <Square size={11} />}
          </button>
        </div>
      </div>

      {/* Filter row — always visible, narrow (24 px). Lets the user trim
          the rendered list by name without touching sort or the path bar.
          Sticky across cd so they can hunt the same name across sibling
          dirs; the X clears, Esc on the input clears too. */}
      <div className="shrink-0 flex items-center gap-1 px-2 py-1 bg-[#0e0e10] border border-white/5 rounded text-[10.5px] font-mono">
        <Search size={11} className="text-zinc-500 shrink-0" />
        <input
          type="text"
          value={nameFilter}
          onChange={(e) => setNameFilter(e.target.value)}
          onKeyDown={(e) => { if (e.key === "Escape") { setNameFilter(""); (e.currentTarget as HTMLInputElement).blur(); } }}
          placeholder="Filter by name…"
          className="flex-1 min-w-0 h-5 bg-transparent text-zinc-100 placeholder:text-zinc-600 focus:outline-none"
        />
        {nameFilter && (
          <>
            <span className="text-[9.5px] text-zinc-500 shrink-0">
              {sortedEntries.length}/{entries.length}
            </span>
            <button
              onClick={() => setNameFilter("")}
              title="Clear filter"
              className="shrink-0 p-0.5 rounded text-zinc-400 hover:text-white hover:bg-white/10"
            >
              <X size={10} />
            </button>
          </>
        )}
      </div>

      {/* Bulk action bar — shows for any non-empty selection so the user
          can send / move / delete with one click without right-clicking.
          Below the threshold of 1 the bar still appears (was previously
          ≥2) because in tabs mode the user can't drag-drop between sides
          and needs a discoverable "Send to other side" target. */}
      {selected.size > 0 && (
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
          {!isRemote && (
            <button
              onClick={() => uploadItems(sortedEntries.filter(e => selected.has(e.path)))}
              title={getOppositeDir?.()
                ? `Upload to ${getOppositeDir!()}`
                : "Open a remote directory first…"}
              disabled={!getOppositeDir?.()}
              className="px-2 py-0.5 rounded bg-sky-500/15 text-sky-300 hover:bg-sky-500/25 flex items-center gap-1 disabled:opacity-40 disabled:cursor-not-allowed"
            >
              <Upload size={10} /> Upload
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
        {/* The select column is conditionally injected — only present when
            there's at least one selected row. With nothing selected the row
            content reclaims the 22px and the list reads cleanly. Discovery
            paths into multi-select that work without the column visible:
            row click (single select), Ctrl/Shift-click (extend), Ctrl+A
            (select all), right-click → context menu, and the header bar's
            select-all toggle to the right of the address bar. */}
        <div className={`min-w-full grid ${
          selected.size > 0
            ? (showPerms ? "grid-cols-[22px_minmax(180px,1fr)_65px_115px_85px]" : "grid-cols-[22px_minmax(180px,1fr)_75px_125px]")
            : (showPerms ? "grid-cols-[minmax(180px,1fr)_65px_115px_85px]"      : "grid-cols-[minmax(180px,1fr)_75px_125px]")
        } gap-1.5 px-2.5 bg-[#161619] border-b border-white/5 font-mono text-[10.5px] text-zinc-300 select-none font-bold shrink-0 sticky top-0 z-10 shadow-md`}>
          {selected.size > 0 && (
            <div
              className="bg-[#161619] flex items-center justify-center py-1.5 cursor-pointer hover:text-white"
              onClick={(e) => { e.stopPropagation(); toggleSelectAll(); }}
              title={allSelected ? "Deselect all" : "Select all"}
            >
              {allSelected ? <CheckSquare size={12} className="text-indigo-300" /> : <Square size={12} className="text-zinc-500" />}
            </div>
          )}
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
            sortedEntries.map((entry) => {
              const isSel = selected.has(entry.path);
              return (
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
                data-fs-row-path={entry.path}
                data-fs-row-isdir={entry.isDir ? "1" : "0"}
                className={`grid ${
                  selected.size > 0
                    ? (showPerms ? "grid-cols-[22px_minmax(180px,1fr)_65px_115px_85px]" : "grid-cols-[22px_minmax(180px,1fr)_75px_125px]")
                    : (showPerms ? "grid-cols-[minmax(180px,1fr)_65px_115px_85px]"      : "grid-cols-[minmax(180px,1fr)_75px_125px]")
                } gap-1.5 px-2.5 py-1 border-l-2 cursor-pointer transition-colors items-center ${
                  isSel
                    ? "bg-indigo-950/40 border-indigo-400 text-indigo-100 font-bold"
                    : "border-transparent text-zinc-200 hover:bg-white/5 hover:text-white"
                }`}
              >
                {selected.size > 0 && (
                  <div
                    className="flex items-center justify-center"
                    onMouseDown={(e) => e.stopPropagation()}
                    onClick={(e) => {
                      // Pure toggle for this row — doesn't replace the
                      // selection the way a bare row click does. Keeps
                      // existing selection intact and just flips this entry
                      // in/out.
                      e.stopPropagation();
                      const next = new Set(selected);
                      if (next.has(entry.path)) next.delete(entry.path);
                      else next.add(entry.path);
                      setSelected(next);
                      lastSelectedPathRef.current = entry.path;
                    }}
                    title={isSel ? "Deselect" : "Select"}
                  >
                    {isSel
                      ? <CheckSquare size={12} className="text-indigo-300" />
                      : <Square size={12} className="text-zinc-500 hover:text-zinc-300" />}
                  </div>
                )}
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
              );
            })
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
              // Both single and bulk move ask for a destination *directory*
              // (we auto-append the original name). Defaulting v1 to the
              // current path means the user only edits the directory part —
              // no chance to fat-finger the filename and rename by accident.
              if (multi) setModal({ type: "move-bulk", v1: currentPath });
              else setModal({ type: "move", entry: contextMenu.entry, v1: currentPath });
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
                    {modal.type === "move" || modal.type === "move-bulk"
                      ? "Destination directory"
                      : "Name"}
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
