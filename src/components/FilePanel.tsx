import React, { useState, useEffect, useRef, useImperativeHandle, forwardRef } from "react";
import { createPortal } from "react-dom";
import { listen } from "@tauri-apps/api/event";
import { invoke } from "@tauri-apps/api/core";
import {
  Folder, FolderUp, File, ArrowUp, RefreshCw, Trash2, Edit3, Shield,
  X, ChevronUp, ChevronDown, Plus, MoreVertical, FolderSearch,
  Download, Upload, ExternalLink, Move, CheckSquare, Square, Search,
  Terminal, Link, FolderSymlink, FileSymlink, CornerDownRight, Archive, PackageOpen,
} from "lucide-react";
import { FileEntry, FileProvider, LinkInfo } from "../fs/types";
import { carryLinkState, mergePermissions, permissionOctal, safeLeafName, shellSingleQuote } from "../fs/dirContext";
import { useConfirm, useOverwritePrompt, OverwriteChoice, OverwritePromptOptions } from "../ui/confirm";
import { IS_ANDROID } from "../util/platform";
import {
  acquireTransferSlot, dropQueued, enqueueTransfers, getQueued, isQueued, queueSignal, startQueued, waitForTurn,
} from "../fs/transferQueue";
import { parentPathOf } from "../fs/localProvider";
import { pathCrumbs } from "../fs/pathCrumbs";
import { canMoveInto, rulesFor, takenNames } from "../fs/moveRules";
import {
  ARCHIVE_FORMATS, ArchiveFormat, archiveFileName, archiveKind, extractFolderName, suggestArchiveBase,
} from "../fs/archive";

// Last format picked in the archive dialog. The remote side is remembered
// per session (the same id the saved directories use): `zip` being installed
// on one server says nothing about the next one.
const archiveFormatKey = (providerId: string, sessionId?: string) =>
  providerId === "remote" ? `submarine-archive-format-remote-${sessionId ?? ""}` : "submarine-archive-format-local";

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
  overwritePrompt: (opts: OverwritePromptOptions) => Promise<OverwriteChoice>,
  // Aborted when this item is cancelled from the transfers bar: the prompt
  // closes, and a choice made for a cancelled item is never latched.
  signal?: AbortSignal,
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
    const choice = await overwritePrompt({ name, direction, batchSize, signal });
    if (signal?.aborted) return "skipped";
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

// "name (reason)" entries of one batch, cut to what fits a single notice.
const listFailures = (failures: string[]) =>
  failures.length > 3
    ? `${failures.slice(0, 3).join(", ")} +${failures.length - 3} more`
    : failures.join(", ");

type SortColumn = "name" | "size" | "modified" | "permissions";
interface SortState { column: SortColumn; asc: boolean; }

export interface ActiveDrag {
  paneId: "local" | "remote";
  entry: FileEntry;
  /** Everything being dragged: the selection, or just `entry`. */
  items: FileEntry[];
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
  /** Focused PTY for this session. Stays set while SFTP hides the terminal. */
  terminalId?: string;
  /** Compact layout: close the SFTP pane so the terminal that just received `cd` is visible. */
  onRevealTerminal?: () => void;
}

export interface FilePanelHandle {
  refresh: () => Promise<void>;
  currentDir: () => string;
  /** Shows a short notice in this panel's own stack, above its docked bars. */
  notify: (msg: string, type?: "info" | "success" | "error") => void;
  /**
   * Sends this panel's `items` to `destDir` on the other side (upload from
   * the local panel, download from the remote one) as one queued batch.
   */
  sendItems: (items: FileEntry[], destDir: string) => Promise<void>;
}

const FilePanel = forwardRef<FilePanelHandle, FilePanelProps>(({
  provider,
  disabled = false,
  sessionId,
  onDragMove,
  initialPath,
  onPathChange,
  getOppositeDir,
  terminalId,
  onRevealTerminal,
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
  // One list() at a time. A call that arrives mid-flight stores its path in
  // pendingPathRef and waits; the in-flight call runs that path next, so a
  // refresh after rename/delete/upload is not dropped. The overlay blocks
  // clicks until the chain finishes.
  const listingRef = useRef(false);
  const pendingPathRef = useRef<string | null>(null);
  const waitersRef = useRef<Array<() => void>>([]);
  const lastOkRef = useRef(false);
  // Row to select when a listing of exactly `dir` lands. Set by "Go to
  // target" on a file symlink; any other listing drops it unused.
  const selectAfterListRef = useRef<{ dir: string; name: string } | null>(null);
  const scrollToPathRef = useRef<string | null>(null);
  // Bumped on every listing. Background link resolution captures it and
  // stops once the pane shows something newer.
  const listGenRef = useRef(0);
  // Set while a directory link is being opened: `from` is where the link
  // lives. The listing of `linkPath` turns it into `linkBack`.
  const linkEntryRef = useRef<{ linkPath: string; from: string } | null>(null);
  // The listing canonicalizes a link path to its real directory, whose
  // parent is not where the user came from. While the pane sits in `dir`,
  // ".." returns to `back` instead.
  const [linkBack, setLinkBack] = useState<{ dir: string; back: string } | null>(null);
  // Multi-selection lives as a Set of paths. Single-click replaces, Ctrl/⌘-
  // click toggles a row in/out, Shift-click extends from the last-clicked
  // anchor. lastSelectedPathRef remembers that anchor across renders.
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const lastSelectedPathRef = useRef<string | null>(null);
  // The selection as of the latest render, for async work that outlives the
  // render it started in.
  const selectedRef = useRef(selected);
  selectedRef.current = selected;
  // Remote "open in editor" downloads the file first. While that runs, the
  // row's highlight fills left-to-right. One slot per path, so opening a
  // second file does not steal the first file's bar.
  // Bumped on every open attempt. A completion timer from the previous
  // attempt must not clear the bar for a newer open of the same path.
  const openGenRef = useRef(0);
  const openGenByPathRef = useRef(new Map<string, number>());
  // Remote paths whose open-in-editor download is still inside `invoke`.
  // A second request for one of these is ignored until that call returns.
  const openingPathsRef = useRef<Set<string>>(new Set());
  const [openProgress, setOpenProgress] = useState<Record<string, { bytes: number; total: number }>>({});
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
  const [dirMenu, setDirMenu] = useState<{ x: number; y: number } | null>(null);
  // "archive": v1 is the name without extension, v2 the format, `items` what
  // was selected when the dialog opened.
  const [modal, setModal] = useState<{ type: "rename" | "mkdir" | "newfile" | "symlink" | "properties" | "move" | "move-bulk" | "archive"; entry?: FileEntry; items?: FileEntry[]; v1?: string; v2?: string } | null>(null);
  const [notification, setNotification] = useState<{ msg: string; type: "info" | "success" | "error" } | null>(null);
  const notifyTimerRef = useRef<number | null>(null);

  const [dragOver, setDragOver] = useState(false);
  // Folder (row, ".." or path-bar segment) an in-pane drag would drop into.
  const [dropHover, setDropHover] = useState<string | null>(null);
  const dropHoverRef = useRef<string | null>(null);
  // Set for the tick after a drag ends, so the `click` that the same
  // mouseup produces doesn't replace or clear the selection.
  const dragJustEndedRef = useRef(false);
  // An in-pane move is running (or waiting for the transfer slot). A second
  // one would work from rows whose paths the first is about to change, so
  // it is refused with a notice.
  const movingRef = useRef(false);
  // The move in progress, shown as its own row in the notice stack (with a
  // cancel button) for as long as it runs: while it waits for the transfer
  // slot and while it renames.
  const [pendingMove, setPendingMove] = useState<{ label: string; targetDir: string; waiting: boolean } | null>(null);
  const cancelPendingMoveRef = useRef<() => void>(() => {});
  // Archive / extract runs in progress, each its own row in the notice stack
  // so a later notify() does not hide that one is still working.
  const [busyJobs, setBusyJobs] = useState<{ id: number; label: string }[]>([]);
  const busyJobSeqRef = useRef(0);
  // The handle is built once; this always points at the current render's
  // upload / download so a cross-pane drop doesn't run a stale closure.
  const sendItemsRef = useRef<(items: FileEntry[], destDir: string) => Promise<void>>(async () => {});
  const dropTargetRef = useRef<HTMLDivElement | null>(null);
  const menuRef = useRef<HTMLDivElement | null>(null);
  const currentPathRef = useRef(currentPath);
  useEffect(() => { currentPathRef.current = currentPath; }, [currentPath]);
  const entriesRef = useRef(entries);
  useEffect(() => { entriesRef.current = entries; }, [entries]);
  useEffect(() => {
    const path = scrollToPathRef.current;
    if (!path) return;
    scrollToPathRef.current = null;
    dropTargetRef.current?.querySelectorAll<HTMLElement>("[data-fs-row-path]").forEach((row) => {
      if (row.getAttribute("data-fs-row-path") === path) row.scrollIntoView({ block: "center" });
    });
  }, [entries]);

  // Android quick-picker state. There is no OS-level folder picker that
  // returns a real filesystem path on Android (SAF returns content URIs
  // that Rust's std::fs can't open), so we replace the desktop rfd call
  // with a small popover of writable paths returned by the backend. Loaded
  // lazily the first time the user opens the popover so we don't burn a
  // touch-probe per directory on every panel mount.
  const [androidPickerOpen, setAndroidPickerOpen] = useState(false);
  const [androidQuickDirs, setAndroidQuickDirs] = useState<{ label: string; path: string }[] | null>(null);

  // ---- helpers ----------------------------------------------------------------

  const notify = (msg: string, type: "info" | "success" | "error" = "info") => {
    if (notifyTimerRef.current != null) {
      window.clearTimeout(notifyTimerRef.current);
      notifyTimerRef.current = null;
    }
    setNotification({ msg, type });
    notifyTimerRef.current = window.setTimeout(() => {
      notifyTimerRef.current = null;
      setNotification(null);
    }, 4000);
  };

  const formatRights = (isDir: boolean, perm?: number) => {
    if (perm === undefined) return isDir ? "d---------" : "----------";
    const r = (v: number) => (v & 4 ? "r" : "-");
    const w = (v: number) => (v & 2 ? "w" : "-");
    const x = (v: number) => (v & 1 ? "x" : "-");
    const u = (perm >> 6) & 7, g = (perm >> 3) & 7, o = perm & 7;
    return (isDir ? "d" : "-") + r(u) + w(u) + x(u) + r(g) + w(g) + x(g) + r(o) + w(o) + x(o);
  };

  // Remote listings show seconds too, so a fresh upload or save is visibly
  // newer than the copy that was there a moment ago.
  const formatTime = (ts?: number, withSeconds = false) => {
    if (!ts) return "-";
    const d = new Date(ts * 1000);
    const pad = (n: number) => n.toString().padStart(2, "0");
    const hm = `${pad(d.getHours())}:${pad(d.getMinutes())}`;
    return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${withSeconds ? `${hm}:${pad(d.getSeconds())}` : hm}`;
  };

  const formatSize = (bytes: number) => {
    if (!bytes) return "0 B";
    const k = 1024;
    const sizes = ["B", "KB", "MB", "GB", "TB"];
    const i = Math.floor(Math.log(bytes) / Math.log(k));
    return parseFloat((bytes / Math.pow(k, i)).toFixed(1)) + " " + sizes[i];
  };

  // Paths compare and split without a trailing separator; a bare root keeps
  // its one separator.
  const stripTrailingSep = (p: string) => p.replace(/[\\/]+$/, "") || p;

  // ---- listing / navigation ---------------------------------------------------

  const pushRecent = (path: string) => {
    if (!path) return;
    setRecentDirs((prev) => {
      const next = [path, ...prev.filter((p) => p !== path)];
      return next.slice(0, 5);
    });
  };

  // A fresh answer about a link supersedes whatever was carried over.
  const applyLinkInfo = (infos: LinkInfo[]) => {
    const byPath = new Map(infos.map((i) => [i.path, i.patch]));
    setEntries((prev) => prev.map((e) => {
      const patch = e.isSymlink ? byPath.get(e.path) : undefined;
      return patch ? { ...e, ...patch, linkStale: undefined } : e;
    }));
  };

  // The whole `resolveLinks` call failed, so nothing is known about these
  // links any more: drop the folder they may have been, record the error.
  const linkCallFailed = (paths: string[], err: unknown): LinkInfo[] => {
    const patch: Partial<FileEntry> = { linkState: "error", linkError: String(err), isDir: false };
    return paths.map((path) => ({ path, patch }));
  };

  // Not yet followed, or followed once and not confirmed since.
  const needsResolve = (e: FileEntry) =>
    !!e.isSymlink && (e.linkStale || (e.linkState !== "ok" && e.linkState !== "broken"));

  // Follows the symlinks of a listing that is already on screen, in chunks,
  // so a directory full of links does not delay its own first paint. Links
  // carried over as "ok" by `carryLinkState` are skipped here and re-checked
  // lazily by `ensureResolvedAll`; broken and failed ones are tried again.
  // A failed chunk marks its links so the failure is visible; opening one
  // retries it.
  const resolveListedLinks = async (gen: number, list: FileEntry[]) => {
    if (!provider.resolveLinks) return;
    const paths = list.filter((e) => e.isSymlink && e.linkState !== "ok").map((e) => e.path);
    const CHUNK = 64;
    for (let i = 0; i < paths.length; i += CHUNK) {
      if (listGenRef.current !== gen) return;
      const chunk = paths.slice(i, i + CHUNK);
      try {
        const infos = await provider.resolveLinks(chunk);
        if (listGenRef.current !== gen) return;
        applyLinkInfo(infos);
      } catch (err) {
        if (listGenRef.current !== gen) return;
        applyLinkInfo(linkCallFailed(chunk, err));
        return;
      }
    }
  };

  // `silent` skips the error toast. Used for the saved initial path, which
  // falls through to home when the directory is gone.
  const fetch = async (path: string, silent = false): Promise<boolean> => {
    if (listingRef.current) {
      pendingPathRef.current = path;
      await new Promise<void>((resolve) => {
        waitersRef.current.push(resolve);
      });
      return lastOkRef.current;
    }

    listingRef.current = true;
    setContextMenu(null);
    setDirMenu(null);
    setRecentOpen(false);
    setLoading(true);
    let target: string | null = path;
    let ok = false;
    try {
      while (target !== null) {
        const requested = target;
        pendingPathRef.current = null;
        try {
          const result = await provider.list(requested);
          const newer = pendingPathRef.current;
          if (newer !== null) {
            target = newer;
            pendingPathRef.current = null;
            continue;
          }
          const listed = result.currentPath === currentPathRef.current
            ? carryLinkState(result.entries, entriesRef.current)
            : result.entries;
          const gen = ++listGenRef.current;
          setEntries(listed);
          setCurrentPath(result.currentPath);
          setTempInput(result.currentPath);
          const via = linkEntryRef.current;
          linkEntryRef.current = null;
          if (via && via.linkPath === requested && via.from !== result.currentPath) {
            setLinkBack({ dir: result.currentPath, back: via.from });
          } else {
            setLinkBack((prev) => (prev && prev.dir === result.currentPath ? prev : null));
          }
          const sel = selectAfterListRef.current;
          selectAfterListRef.current = null;
          const focus = sel && stripTrailingSep(sel.dir) === stripTrailingSep(result.currentPath)
            ? listed.find((e) => e.name === sel.name)
            : undefined;
          scrollToPathRef.current = focus ? focus.path : null;
          setSelected(new Set(focus ? [focus.path] : []));
          lastSelectedPathRef.current = focus ? focus.path : null;
          void resolveListedLinks(gen, listed);
          onPathChange?.(result.currentPath);
          pushRecent(result.currentPath);
          ok = true;
        } catch (err: any) {
          const newer = pendingPathRef.current;
          if (newer !== null) {
            target = newer;
            pendingPathRef.current = null;
            continue;
          }
          selectAfterListRef.current = null;
          linkEntryRef.current = null;
          if (!silent || requested !== path) notify(`List failed: ${err}`, "error");
          ok = false;
        }
        target = pendingPathRef.current;
        pendingPathRef.current = null;
      }
    } finally {
      listingRef.current = false;
      lastOkRef.current = ok;
      setLoading(false);
      const waiters = waitersRef.current;
      waitersRef.current = [];
      for (const resolve of waiters) resolve();
    }
    return ok;
  };

  // Initial load: try the caller-supplied `initialPath` first (the
  // per-server-saved directory), and fall back to the provider's home if it
  // no longer exists — that way a saved path being removed doesn't strand
  // the user with an error screen.
  useEffect(() => {
    (async () => {
      if (initialPath) {
        const ok = await fetch(initialPath, true);
        if (ok) return;
      }
      try {
        const home = await provider.homePath();
        await fetch(home);
      } catch (err: any) {
        notify(`Failed to load: ${err}`, "error");
      }
    })();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [provider]);

  useImperativeHandle(ref, () => ({
    refresh: async () => { await fetch(currentPathRef.current); },
    currentDir: () => currentPathRef.current,
    notify: (msg, type) => notify(msg, type),
    sendItems: (items, destDir) => sendItemsRef.current(items, destDir),
  }), []);

  const cameViaLink = linkBack !== null && linkBack.dir === currentPath;
  const goUp = () => fetch(cameViaLink ? linkBack!.back : provider.parentPath(currentPath));

  // Latest known state of each entry, following the links the background
  // pass has not reached, failed on, or only carried over from an earlier
  // listing. Needed before any action that depends on what a link points
  // at: until resolved, a link looks like a file.
  const ensureResolvedAll = async (items: FileEntry[]): Promise<FileEntry[]> => {
    const live = items.map((entry) => entriesRef.current.find((e) => e.path === entry.path) ?? entry);
    const unresolved = live.filter(needsResolve);
    if (unresolved.length === 0 || !provider.resolveLinks) return live;
    const paths = unresolved.map((e) => e.path);
    let infos: LinkInfo[];
    try {
      infos = await provider.resolveLinks(paths);
    } catch (err) {
      infos = linkCallFailed(paths, err);
    }
    applyLinkInfo(infos);
    const byPath = new Map(infos.map((i) => [i.path, i.patch]));
    return live.map((e) => {
      const patch = byPath.get(e.path);
      return patch ? { ...e, ...patch, linkStale: undefined } : e;
    });
  };
  const ensureResolved = async (entry: FileEntry): Promise<FileEntry> =>
    (await ensureResolvedAll([entry]))[0];

  // True (after telling the user why) when the link cannot be followed.
  const linkUnusable = (entry: FileEntry): boolean => {
    if (entry.linkState === "broken") {
      notify(`Broken link: ${entry.name} → ${entry.linkTarget ?? "?"}`, "error");
      return true;
    }
    if (entry.linkState !== "ok") {
      notify(`Cannot read the target of ${entry.name}: ${entry.linkError ?? "not resolved"}`, "error");
      return true;
    }
    return false;
  };

  // Enter a directory through a link to it. The listing canonicalizes the
  // path; `linkEntryRef` makes ".." return to the directory holding the link,
  // which is the current one for a row and may be elsewhere for a suggestion.
  const enterDirLink = (entry: FileEntry) => {
    linkEntryRef.current = { linkPath: entry.path, from: provider.parentPath(entry.path) };
    fetch(entry.path);
  };

  // Double-click / Open / Edit. A directory link is entered through the
  // link, so ".." comes back here; a file link opens like the file it points
  // at. Everything past the resolve step works on the fresh entry: the row's
  // own `isDir` may describe what the link pointed at before a refresh.
  const openEntry = async (row: FileEntry) => {
    let entry = row;
    if (row.isSymlink) {
      const from = currentPath;
      entry = await ensureResolved(row);
      if (currentPathRef.current !== from || linkUnusable(entry)) return;
      if (entry.isDir) { enterDirLink(entry); return; }
    } else if (entry.isDir) {
      fetch(entry.path);
      return;
    }
    // For files: remote → live-edit (download + open editor + auto-upload
    // on save); local → open in the OS default app. Both are desktop-only —
    // on Android a double-tap on a file is a no-op (there's no OS default
    // editor to hand off to).
    if (IS_ANDROID) return;
    if (isRemote) liveEditEntry(entry);
    else openLocalEntry(entry);
  };

  // Properties edits the target's mode and owner (chmod/chown follow the
  // link), so a link is followed first and the dialog shows fresh values.
  const openProperties = async (row: FileEntry) => {
    const entry = row.isSymlink ? await ensureResolved(row) : row;
    if (entry.isSymlink && linkUnusable(entry)) return;
    setModal({ type: "properties", entry, v1: permissionOctal(entry.permissions), v2: entry.uid?.toString() });
  };

  // Jump to where the link really points, leaving the link behind: a
  // directory target is opened by its real path (".." is then its real
  // parent); a file target is selected inside its own directory.
  const goToLinkTarget = async (entry: FileEntry) => {
    if (!provider.realPath) return;
    const from = currentPath;
    const resolved = await ensureResolved(entry);
    if (currentPathRef.current !== from || linkUnusable(resolved)) return;
    let real: string;
    try {
      real = await provider.realPath(entry.path);
    } catch (err: any) {
      notify(`Cannot resolve ${entry.name}: ${err}`, "error");
      return;
    }
    if (currentPathRef.current !== from) return;
    if (resolved.isDir) { fetch(real); return; }
    const target = stripTrailingSep(real);
    const dir = provider.parentPath(target);
    selectAfterListRef.current = { dir, name: target.split(provider.pathSep).pop() ?? "" };
    fetch(dir);
  };

  // WinSCP-style ".." row. Hidden at a filesystem root, where parentPath is
  // the same place (remote "/") or a drive root ("C:\"). Not a real entry:
  // it never joins `entries`, so select-all, delete, drag, and transfer
  // cannot target it.
  const atFilesystemRoot = (path: string): boolean => {
    if (!path || path === "/") return true;
    const trimmed = path.replace(/[\\/]+$/, "");
    return trimmed === "" || /^[a-zA-Z]:$/.test(trimmed);
  };
  const showParent = currentPath.length > 0 && (cameViaLink || !atFilesystemRoot(currentPath));

  // ---- selection -------------------------------------------------------------

  // Three click modes match every desktop file manager people already know:
  //   - plain click  → replace selection with this row
  //   - Ctrl/⌘+click → toggle this row in / out of the existing set
  //   - Shift+click  → extend the range from the last anchor to this row
  // Shift-extend uses `sortedEntries` (the rendered order), not `entries`,
  // so the visual range matches what the user just dragged across.
  const onRowClick = (e: React.MouseEvent, entry: FileEntry, ordered: FileEntry[]) => {
    if (dragJustEndedRef.current) return;
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

  // A link in the suggestions was never followed (only the pane's own
  // listing gets the background pass), so ask before deciding to enter it.
  const pickSuggestion = async (e: FileEntry) => {
    setTempInput(e.path);
    setInputFocused(false);
    const resolved = e.isSymlink ? await ensureResolved(e) : e;
    if (!resolved.isDir) return;
    if (e.isSymlink) enterDirLink(e);
    else fetch(e.path);
  };

  // ---- path bar segments ------------------------------------------------------

  const crumbs = pathCrumbs(currentPath, provider.pathSep);

  const crumbsRef = useRef<HTMLDivElement | null>(null);
  const blurTimerRef = useRef<number | null>(null);
  const clearBlurTimer = () => {
    if (blurTimerRef.current != null) {
      window.clearTimeout(blurTimerRef.current);
      blurTimerRef.current = null;
    }
  };
  useEffect(() => clearBlurTimer, []);

  // A long path scrolls inside the bar; keep its end (the current folder)
  // in view.
  useEffect(() => {
    const el = crumbsRef.current;
    if (el) el.scrollLeft = el.scrollWidth;
  }, [currentPath, inputFocused]);

  const editPath = () => {
    // A pending blur from the previous edit must not close this one.
    clearBlurTimer();
    setTempInput(currentPath);
    setActiveSuggestion(-1);
    setInputFocused(true);
  };

  // ---- context menu auto-close ------------------------------------------------

  useEffect(() => {
    if (!contextMenu && !dirMenu) return;
    const onWindowMouseDown = (ev: MouseEvent) => {
      const target = ev.target as Node | null;
      if (target && menuRef.current?.contains(target)) return;
      setContextMenu(null);
      setDirMenu(null);
    };
    const timer = setTimeout(() => window.addEventListener("mousedown", onWindowMouseDown), 50);
    return () => {
      clearTimeout(timer);
      window.removeEventListener("mousedown", onWindowMouseDown);
    };
  }, [contextMenu, dirMenu]);

  const openMenu = (e: React.MouseEvent, entry: FileEntry) => {
    e.preventDefault();
    e.stopPropagation();
    const MENU_W = 200, MENU_H = 380;
    const x = Math.min(e.clientX, window.innerWidth - MENU_W - 4);
    const y = Math.min(e.clientY, window.innerHeight - MENU_H - 4);
    // Right-click on a row that isn't already part of the selection should
    // switch focus to it (Explorer/Finder behaviour). Right-click on a row
    // that IS selected keeps the multi-selection so bulk actions apply.
    if (!selected.has(entry.path)) {
      setSelected(new Set([entry.path]));
      lastSelectedPathRef.current = entry.path;
    }
    setDirMenu(null);
    setContextMenu({ x: Math.max(4, x), y: Math.max(4, y), entry });
  };

  // ---- drag-source -------------------------------------------------------------

  const DRAG_START_THRESHOLD = 6;

  // ---- in-pane move (drop on a folder of this same pane) ------------------------

  const isRemoteProvider = provider.id === "remote";
  const moveRules = () => rulesFor(provider.id, provider.pathSep);

  // Folder of THIS pane under the cursor that `items` can be moved into.
  // Drop targets carry `data-fs-drop-path`: folder rows, the ".." row and the
  // path-bar segments.
  const dropTargetAt = (x: number, y: number, items: FileEntry[]): string | null => {
    const hit = document.elementFromPoint(x, y) as HTMLElement | null;
    if (!hit || hit.closest("[data-fs-pane]")?.getAttribute("data-fs-pane") !== provider.id) return null;
    const target = hit.closest("[data-fs-drop-path]")?.getAttribute("data-fs-drop-path");
    const rules = moveRules();
    if (!target || !items.some((it) => canMoveInto(it, target, rules))) return null;
    return target;
  };

  const moveInto = async (items: FileEntry[], targetDir: string) => {
    const rules = moveRules();
    const movable = items.filter((it) => canMoveInto(it, targetDir, rules));
    if (movable.length === 0) return;
    if (movingRef.current) {
      notify("Another move is still in progress. Wait for it, or cancel it.", "info");
      return;
    }
    movingRef.current = true;
    // Cancel works in both phases. While waiting for the slot nothing has
    // been touched, so the panel is handed back at once (`ownsPanel` goes
    // false and this call just returns when its turn comes). While renaming
    // it stops the loop before the next item.
    let cancelled = false;
    let waiting = true;
    let ownsPanel = true;
    cancelPendingMoveRef.current = () => {
      cancelled = true;
      if (!waiting) return;
      ownsPanel = false;
      movingRef.current = false;
      setPendingMove(null);
    };
    const label = movable.length === 1 ? movable[0].name : `${movable.length} items`;
    const showRow = () => { if (ownsPanel) setPendingMove({ label, targetDir, waiting }); };
    // Something is already queued: the wait will be visible, say so now.
    // Otherwise only a move that takes a moment gets the row, so a plain
    // quick one doesn't flash it. (The slot can also be held by a move in
    // the other panel, which is not in the queue; the timer covers that.)
    let rowTimer: number | null = null;
    if (sessionId && getQueued(sessionId).length > 0) showRow();
    else rowTimer = window.setTimeout(showRow, 250);
    // Moves take their turn in the session's transfer slot, so a rename
    // never pulls a file out from under an upload / download that was
    // queued before it (and a later batch starts from the new paths).
    let release: (() => void) | null = null;
    try {
      if (sessionId) release = await acquireTransferSlot(sessionId);
      if (cancelled) return;
      waiting = false;
      setPendingMove((cur) => (cur ? { ...cur, waiting: false } : cur));
      // A rename onto an existing name replaces it on the local side and
      // fails with a bare "Failure" over SFTP, so look first and skip those.
      let taken: Set<string>;
      try {
        const existing = (await provider.list(targetDir)).entries.map((e) => e.name);
        taken = takenNames(movable.map((it) => it.name), existing, rules);
      } catch (err: any) {
        notify(`Move failed: ${err}`, "error");
        return;
      }
      const moved: string[] = [];
      const failures: string[] = [];
      for (const it of movable) {
        if (cancelled) break;
        if (taken.has(it.name)) {
          failures.push(`${it.name} (already exists there)`);
          continue;
        }
        try {
          await provider.rename(it.path, provider.joinPath(targetDir, it.name));
          moved.push(it.path);
        } catch (err: any) {
          failures.push(`${it.name} (${err})`);
        }
      }
      // One notice for the whole drop: per-item ones would replace each
      // other and leave only the last on screen.
      const stopped = movable.length - moved.length - failures.length;
      const notMoved = failures.length > 0 ? ` Not moved: ${listFailures(failures)}` : "";
      const stoppedNote = stopped > 0 ? ` (${stopped} cancelled)` : "";
      if (failures.length === 0 && stopped === 0) {
        notify(movable.length === 1 ? `Moved ${movable[0].name} → ${targetDir}` : `Moved ${moved.length} items → ${targetDir}`, "success");
      } else if (moved.length > 0) {
        notify(`Moved ${moved.length} of ${movable.length} → ${targetDir}${stoppedNote}.${notMoved}`, failures.length > 0 ? "error" : "info");
      } else if (failures.length > 0) {
        notify(`Nothing moved${stoppedNote}.${notMoved}`, "error");
      } else {
        notify("Move cancelled", "info");
      }
      if (moved.length > 0) {
        // Only what actually left: rows that failed, and selected rows the
        // name filter hides (never part of the drag), stay selected.
        // The refresh itself clears the selection, so put the rest back.
        const gone = new Set(moved);
        // Taken from the selection as it is NOW: rows clicked while the
        // move ran must not be rolled back to the drag-start snapshot.
        const keep = new Set([...selectedRef.current].filter((path) => !gone.has(path)));
        const anchor = lastSelectedPathRef.current;
        await fetch(currentPathRef.current);
        setSelected(keep);
        lastSelectedPathRef.current = anchor && keep.has(anchor) ? anchor : null;
      }
    } finally {
      release?.();
      if (rowTimer != null) window.clearTimeout(rowTimer);
      if (ownsPanel) {
        ownsPanel = false;
        setPendingMove(null);
        movingRef.current = false;
      }
    }
  };

  const handleRowMouseDown = (e: React.MouseEvent, entry: FileEntry) => {
    if (disabled || e.button !== 0) return;
    // An unresolved link may turn out to be a folder or gone. Re-check now
    // so the next attempt can go ahead. A resolved folder is draggable.
    if (needsResolve(entry)) { void ensureResolved(entry); return; }

    const startX = e.clientX;
    const startY = e.clientY;
    let dragStarted = false;
    // Dragging a selected row carries the whole (visible) selection; any
    // other row is dragged on its own.
    const items = selected.has(entry.path)
      ? sortedEntries.filter((en) => selected.has(en.path))
      : [entry];

    const setHover = (path: string | null) => {
      if (dropHoverRef.current === path) return;
      dropHoverRef.current = path;
      setDropHover(path);
    };

    const onMove = (ev: MouseEvent) => {
      if (!dragStarted) {
        if (Math.abs(ev.clientX - startX) < DRAG_START_THRESHOLD &&
            Math.abs(ev.clientY - startY) < DRAG_START_THRESHOLD) return;
        dragStarted = true;
      }
      onDragMove({
        paneId: provider.id as "local" | "remote",
        entry,
        items,
        x: ev.clientX,
        y: ev.clientY,
      });
      setHover(dropTargetAt(ev.clientX, ev.clientY, items));
    };

    const onUp = (ev: MouseEvent) => {
      window.removeEventListener("mousemove", onMove);
      window.removeEventListener("mouseup", onUp);
      // Workspace listens for its own `mouseup` and uses the active drag (which
      // we keep up to date via `onDragMove`) to dispatch the cross-pane
      // transfer. We just clear our local indicator here.
      onDragMove(null);
      setHover(null);
      if (!dragStarted) return;
      dragJustEndedRef.current = true;
      window.setTimeout(() => { dragJustEndedRef.current = false; }, 0);
      const target = dropTargetAt(ev.clientX, ev.clientY, items);
      if (target) moveInto(items, target);
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
    let alive = true;
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
    ).then((u) => {
      // Unmounted before listen() resolved: a kept listener would show a
      // second auto-sync notice after the panel is opened again.
      if (!alive) {
        u();
        return;
      }
      unlisten = u;
    });
    return () => {
      alive = false;
      if (unlisten) unlisten();
    };
  }, [sessionId]);

  // Open-in-editor download progress. Rust streams `sftp-open-{id}` while
  // `sftp_open_remote_file` copies the remote file into the temp dir.
  useEffect(() => {
    if (!sessionId || provider.id !== "remote") return;
    let alive = true;
    let unlisten: (() => void) | null = null;
    listen<{ path: string; bytes: number; total: number; status: string }>(
      `sftp-open-${sessionId}`,
      (event) => {
        const payload = event.payload;
        if (!payload?.path || payload.status === "error") return;
        setOpenProgress((prev) => {
          const cur = prev[payload.path];
          if (!cur) return prev;
          return {
            ...prev,
            [payload.path]: {
              bytes: payload.bytes ?? 0,
              // Keep the listing size when the server omits the stat.
              total: payload.total > 0 ? payload.total : cur.total,
            },
          };
        });
      },
    ).then((fn) => {
      if (!alive) {
        fn();
        return;
      }
      unlisten = fn;
    });
    return () => {
      alive = false;
      if (unlisten) unlisten();
    };
  }, [sessionId, provider.id]);

  // OS drops carry bare paths. Each is looked up in its parent's listing to
  // tell folders from files (and to get file sizes for the queue). A path
  // whose parent cannot be listed is sent as a file, as before.
  const describeDroppedPaths = async (paths: string[]) => {
    const leaf = (p: string) => p.replace(/[\\/]+$/, "").split(/[\\/]/).pop() || "file";
    const byParent = new Map<string, string[]>();
    for (const p of paths) {
      const parent = parentPathOf(p, p.includes("\\") ? "\\" : "/");
      byParent.set(parent, [...(byParent.get(parent) ?? []), p]);
    }
    const found = new Map<string, { isDir: boolean; size: number }>();
    await Promise.all([...byParent].map(async ([parent, group]) => {
      try {
        const raw = await invoke<{ name: string; is_dir: boolean; size: number }[]>("local_list_dir", { path: parent });
        const byName = new Map(raw.map((r) => [r.name, r]));
        for (const p of group) {
          const r = byName.get(leaf(p));
          if (r) found.set(p, { isDir: r.is_dir, size: r.size });
        }
      } catch { /* unknown kind: sent as a file */ }
    }));
    return paths.map((p) => {
      const info = found.get(p);
      return { path: p, name: leaf(p), isDir: info?.isDir ?? false, size: info && !info.isDir ? info.size : undefined };
    });
  };

  // ---- OS-level drag-drop into this pane --------------------------------------
  // Tauri 2 routes OS file drops through `tauri://drag-drop`; HTML5 drop events
  // fire too but their `File.path` is empty inside Tauri. We listen globally and
  // dispatch only when the cursor landed inside our root.

  useEffect(() => {
    // Only the remote pane accepts OS drops: the local pane has a session
    // id too (for its Upload button), but its current dir is a local path.
    if (!sessionId || provider.id !== "remote") return;
    let alive = true;
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
        const trimmed = dir.replace(/\/+$/, "");
        // Same batch path as the Upload button, so the whole drop shows in
        // the transfers bar with the files still waiting as "Queued".
        const items = await describeDroppedPaths(paths as string[]);
        const dirCount = items.filter((it) => it.isDir).length;
        const fileCount = items.length - dirCount;
        notify(items.length === 1
          ? `Uploading ${items[0].isDir ? "folder " : ""}${items[0].name}…`
          : `Uploading ${fileCount} file${fileCount === 1 ? "" : "s"}${dirCount ? ` + ${dirCount} folder${dirCount === 1 ? "" : "s"}` : ""}…`, "info");
        const res = await runTransferBatch("upload", items, (it, transferId, overwrite) => {
          // Folders go to the remote PARENT (sftp_upload_dir adds the name),
          // files to their full remote path — same as uploadItems.
          const remotePath = it.isDir ? trimmed || "/" : `${trimmed}/${it.name}`;
          const cmd = it.isDir ? "sftp_upload_dir" : "sftp_upload_file";
          return invoke(cmd, { sessionId, localPath: it.path, remotePath, overwrite, transferId });
        });
        notifyBatchResult("upload", items, res);
        await fetch(dir);
      }
    ).then((fn) => {
      // Unmounted (or session changed) before listen() resolved.
      if (!alive) {
        fn();
        return;
      }
      unlisten = fn;
    });
    return () => {
      alive = false;
      if (unlisten) unlisten();
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [sessionId]);

  // ---- modals -----------------------------------------------------------------

  const submitModal = async () => {
    if (!modal) return;
    const { type, entry, v1, v2 } = modal;
    if (type === "archive") {
      // Stays open when the name is rejected; the packing itself runs on
      // after the dialog closes.
      if (await startArchive(modal.items ?? [], v1 || "", (v2 || "zip") as ArchiveFormat)) setModal(null);
      return;
    }
    if (type === "mkdir" || type === "newfile" || type === "symlink") {
      const label = type === "mkdir" ? "folder" : type === "newfile" ? "file" : "link";
      if (!safeLeafName(v1 || "")) {
        notify(`Failed: Invalid ${label} name`, "error");
        return;
      }
      if (type === "symlink" && !(v2 || "").trim()) {
        notify("Failed: Link target is required", "error");
        return;
      }
    }
    if (type === "properties") {
      if (!v1 || Number.isNaN(parseInt(v1, 8))) {
        notify("Failed: Invalid octal mode", "error");
        return;
      }
      const uidText = (v2 || "").trim();
      if (uidText && entry && provider.chown) {
        const uid = parseInt(uidText, 10);
        if (Number.isNaN(uid)) {
          notify("Failed: Invalid owner UID", "error");
          return;
        }
        if (uid !== entry.uid && entry.gid == null) {
          notify("Failed: Group is unavailable", "error");
          return;
        }
      }
    }
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
        const name = safeLeafName(v1)!;
        await provider.mkdir(provider.joinPath(currentPath, name));
        notify(`Created ${name}`, "success");
      } else if (type === "newfile" && v1 && sessionId) {
        const name = safeLeafName(v1)!;
        await invoke("sftp_create_file", { sessionId, path: provider.joinPath(currentPath, name) });
        notify(`Created ${name}`, "success");
      } else if (type === "symlink" && v1 && sessionId) {
        const name = safeLeafName(v1)!;
        await invoke("sftp_create_symlink", {
          sessionId,
          path: provider.joinPath(currentPath, name),
          target: (v2 || "").trim(),
        });
        notify(`Created link ${name}`, "success");
      } else if (type === "properties" && entry && v1 && provider.chmod) {
        const edited = parseInt(v1, 8);
        await provider.chmod(entry.path, mergePermissions(entry.permissions, edited));
        const uidText = (v2 || "").trim();
        if (uidText && provider.chown && entry.gid != null) {
          const uid = parseInt(uidText, 10);
          if (uid !== entry.uid) await provider.chown(entry.path, uid, entry.gid);
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
        ? (items[0].isSymlink
            ? `Delete link “${items[0].name}”? Its target is not touched.`
            : items[0].isDir
            ? `Permanently delete folder “${items[0].name}” and everything inside?`
            : `Permanently delete “${items[0].name}”?`)
        : `Permanently delete ${items.length} items?${
            items.some((e) => e.isDir && !e.isSymlink) ? " Folders include their contents." : ""
          }${
            items.some((e) => e.isSymlink) ? " Links are removed without touching their targets." : ""
          }`,
      okLabel: "Delete",
      destructive: true,
    });
    if (!ok) return;
    let count = 0;
    for (const it of items) {
      // A link is unlinked as a file even when it points at a directory.
      try { await provider.remove(it.path, it.isDir && !it.isSymlink); count++; }
      catch (err: any) { notify(`Delete failed for ${it.name}: ${err}`, "error"); }
    }
    if (count > 0) {
      setSelected(new Set());
      lastSelectedPathRef.current = null;
      notify(items.length === 1 ? `Deleted ${items[0].name}` : `Deleted ${count} of ${items.length} items`, "success");
      await fetch(currentPath);
    }
  };

  // ---- archive / extract --------------------------------------------------------

  const runBusyJob = async (label: string, work: () => Promise<void>) => {
    const id = ++busyJobSeqRef.current;
    setBusyJobs((cur) => [...cur, { id, label }]);
    try {
      await work();
    } finally {
      setBusyJobs((cur) => cur.filter((job) => job.id !== id));
    }
  };

  // Shows what a finished job produced, unless the user has moved on to
  // another folder. The refresh clears the selection, so it is put back.
  const refreshAfterJob = async (dir: string) => {
    if (currentPathRef.current !== dir) return;
    const keep = new Set(selectedRef.current);
    const anchor = lastSelectedPathRef.current;
    await fetch(dir);
    setSelected(keep);
    lastSelectedPathRef.current = anchor;
  };

  const openArchiveDialog = (items: FileEntry[]) => {
    if (items.length === 0) return;
    const dirName = atFilesystemRoot(currentPath)
      ? ""
      : currentPath.replace(/[\\/]+$/, "").split(/[\\/]/).pop() || "";
    let format: ArchiveFormat = isRemoteProvider ? "tar.gz" : "zip";
    try {
      const saved = localStorage.getItem(archiveFormatKey(provider.id, sessionId));
      const known = ARCHIVE_FORMATS.find((f) => f.format === saved);
      if (known) format = known.format;
    } catch { /* storage unavailable: keep the default */ }
    setModal({
      type: "archive",
      items,
      v1: suggestArchiveBase(items, dirName, entries.map((e) => e.name), moveRules().foldCase),
      v2: format,
    });
  };

  // False when the name was refused (the dialog stays open). True once the
  // packing has started; it reports its own result.
  const startArchive = async (items: FileEntry[], base: string, format: ArchiveFormat): Promise<boolean> => {
    const name = base.trim() ? safeLeafName(archiveFileName(base, format)) : null;
    if (!name || items.length === 0) {
      notify("Failed: Invalid archive name", "error");
      return false;
    }
    const rules = moveRules();
    if (takenNames([name], items.map((it) => it.name), rules).size > 0) {
      notify(`Failed: ${name} is one of the items being archived`, "error");
      return false;
    }
    const dir = currentPath;
    const dest = provider.joinPath(dir, name);
    const confirmReplace = () => confirmDialog({
      title: "Replace archive",
      message: `“${name}” already exists in this folder. Replace it?`,
      okLabel: "Replace",
      destructive: true,
    });
    let overwrite = false;
    const clash = entries.filter((e) => takenNames([name], [e.name], rules).size > 0);
    // A folder is never replaced, so it must not be offered.
    if (clash.some((e) => e.isDir)) {
      notify(`Failed: a folder named ${name} already exists here`, "error");
      return false;
    }
    if (clash.length > 0) {
      if (!(await confirmReplace())) return false;
      overwrite = true;
    }
    try { localStorage.setItem(archiveFormatKey(provider.id, sessionId), format); } catch { /* not remembered */ }
    const names = items.map((it) => it.name);
    const label = items.length === 1 ? items[0].name : `${items.length} items`;
    void runBusyJob(`Archiving ${label} → ${name}…`, async () => {
      try {
        try {
          await provider.archive(dir, names, dest, format, overwrite);
        } catch (err: any) {
          // The listing was stale: the name is taken after all.
          if (overwrite || !String(err).startsWith("EXISTS:")) throw err;
          if (!(await confirmReplace())) return;
          await provider.archive(dir, names, dest, format, true);
        }
        notify(`Created ${name}`, "success");
        await refreshAfterJob(dir);
      } catch (err: any) {
        notify(`Archive failed: ${err}`, "error");
      }
    });
    return true;
  };

  // `intoFolder` unpacks into a folder named after the archive; otherwise
  // into the current directory. Unpacking replaces same-named files without
  // asking, so anything but a brand-new folder is confirmed first.
  const extractEntry = async (entry: FileEntry, intoFolder: boolean) => {
    const dir = currentPath;
    const folder = extractFolderName(entry.name);
    if (intoFolder && !folder) {
      notify(`Failed: ${entry.name} has no usable folder name`, "error");
      return;
    }
    const destDir = intoFolder ? provider.joinPath(dir, folder!) : dir;
    const newFolder = intoFolder && takenNames([folder!], entries.map((e) => e.name), moveRules()).size === 0;
    if (!newFolder) {
      const ok = await confirmDialog({
        title: "Extract archive",
        message: `Extract “${entry.name}” into ${destDir}? Files with the same names will be replaced.`,
        okLabel: "Extract",
      });
      if (!ok) return;
    }
    await runBusyJob(`Extracting ${entry.name}…`, async () => {
      try {
        await provider.extract(dir, entry.name, intoFolder ? folder : null);
        notify(`Extracted ${entry.name} → ${destDir}`, "success");
        await refreshAfterJob(dir);
      } catch (err: any) {
        notify(`Extract failed: ${err}`, "error");
      }
    });
  };

  // ---- contextual actions -----------------------------------------------------

  // Runs a download/upload batch one item at a time. Every item is queued
  // first, so the transfers bar lists what is still waiting, and each item
  // reserves its turn in the session's transfer slot right then, so this
  // batch and any other one (or a cross-pane drag) never write at the same
  // time and run in the order the bar lists them. The
  // slot is held through the overwrite prompt: the EXISTS check and the
  // write must not be split by another transfer to the same path. Each gets its
  // transfer id up front and `run` passes it to the backend, so the bar
  // links the item to its progress events by id and can cancel it before
  // the first event. An item removed from the bar is skipped, also when that
  // happens during the overwrite prompt: the prompt closes (queue signal)
  // without latching an "all" choice, and the retry checks the queue again.
  type BatchResult = { count: number; skipped: number; failures: string[] };
  const runTransferBatch = async <T extends { name: string; isDir: boolean; size?: number }>(
    kind: "download" | "upload",
    items: T[],
    run: (item: T, transferId: string, overwrite: boolean) => Promise<unknown>,
  ): Promise<BatchResult> => {
    if (!sessionId) return { count: 0, skipped: 0, failures: [] };
    const batch: OverwriteBatch = { kind: "ask" };
    const ids = enqueueTransfers(sessionId, items.map((it) => ({
      name: it.name, kind, size: it.isDir ? undefined : it.size, isDir: it.isDir,
    })));
    let count = 0;
    let skipped = 0;
    let cancelled = false;
    const failures: string[] = [];
    try {
      for (let i = 0; i < items.length && !cancelled; i++) {
        const item = items[i];
        const id = ids[i];
        // The turn was reserved at enqueue time; null means the item was
        // removed from the bar before its turn (the slot has moved on).
        const release = await waitForTurn(id);
        if (!release) continue;
        // Removed from the bar while it waited for its turn.
        if (!startQueued(sessionId, id)) { release(); continue; }
        try {
          const res = await transferWithOverwriteCheck(
            async (overwrite) => {
              if (!isQueued(sessionId, id)) throw "cancelled";
              await run(item, id, overwrite);
            },
            item.name, kind, items.length, batch, overwritePrompt, queueSignal(id),
          );
          if (res === "done") count++;
          if (res === "skipped") skipped++;
          if (res === "cancelled") cancelled = true;
        } catch (err: any) {
          // Cancelled from the transfers bar: its row already says so.
          if (!String(err).endsWith("cancelled")) failures.push(`${item.name} (${err})`);
        } finally {
          dropQueued(sessionId, [id]);
          release();
        }
      }
    } finally {
      dropQueued(sessionId, ids);
    }
    return { count, skipped, failures };
  };

  // One notice for the whole batch. Per-item ones replace each other in the
  // single toast slot, so a failure would vanish behind the final count.
  const notifyBatchResult = (kind: "download" | "upload", items: { name: string }[], res: BatchResult) => {
    const done = kind === "download" ? "Downloaded" : "Uploaded";
    const label = kind === "download" ? "Download" : "Upload";
    const total = items.length;
    // Whatever is neither done, skipped nor failed was cancelled: from the
    // transfers bar, or with the rest of the batch at the overwrite prompt.
    const stopped = total - res.count - res.skipped - res.failures.length;
    const notes = [
      res.skipped > 0 ? `${res.skipped} skipped` : "",
      stopped > 0 ? `${stopped} cancelled` : "",
    ].filter(Boolean).join(", ");
    const tail = notes ? ` (${notes})` : "";
    if (res.failures.length > 0) {
      notify(res.count > 0 || notes
        ? `${done} ${res.count} of ${total}${tail}. Failed: ${listFailures(res.failures)}`
        : `${label} failed: ${listFailures(res.failures)}`, "error");
    } else if (res.count === total) {
      notify(total === 1 ? `${done} ${items[0].name}` : `${done} ${res.count} of ${total} items`, "success");
    } else if (total === 1) {
      notify(res.skipped > 0 ? `${items[0].name} skipped` : `${label} of ${items[0].name} cancelled`, "info");
    } else {
      // Green is for a batch that fully arrived; a cut-short one is a note.
      notify(`${done} ${res.count} of ${total} items${tail}`, "info");
    }
  };

  // Remote → local download. Bulk-aware, folder-aware. Destination is
  // whatever the local pane is currently showing (`getOppositeDir`) — that's
  // almost always what the user wants and saves the round-trip through a
  // folder picker every single time. Only falls back to the native picker
  // if the sibling pane hasn't reported a directory yet (e.g. it's still
  // loading). Folders go through `sftp_download_dir` which walks the tree
  // and preserves structure; individual files go through the single-file
  // command. Both paths emit progress on the same `sftp-transfer-{id}`
  // channel so the user sees uniform cards.
  const downloadItems = async (selection: FileEntry[], destDir?: string) => {
    // A link to a folder is left out: `sftp_download_dir` would walk the
    // target's tree, which is not what selecting the link row asks for.
    // Links are followed first, since an unresolved one looks like a file.
    const resolved = await ensureResolvedAll(selection);
    const items = resolved.filter((e) => !(e.isSymlink && e.isDir));
    const skippedLinks = selection.length - items.length;
    if (skippedLinks > 0) {
      notify(
        `Skipped ${skippedLinks} folder link${skippedLinks === 1 ? "" : "s"} — open a link to download its contents`,
        "info",
      );
    }
    if (!sessionId || items.length === 0) return;
    let dest = destDir || getOppositeDir?.();
    if (!dest) {
      // Fallback destination when the sibling pane hasn't reported a dir
      // yet. Desktop opens the rfd folder picker; Android has no picker
      // that returns a real filesystem path we can hand to Rust, so we
      // land in the app-visible default (Downloads or, if scoped storage
      // blocks it, app-scoped external files). Users can navigate from
      // there if they want a different subfolder.
      try {
        dest = IS_ANDROID
          ? ((await invoke<string>("android_default_local_dir")) || undefined)
          : ((await invoke<string | null>("select_local_folder")) || undefined);
      } catch (err: any) { notify(`Pick folder failed: ${err}`, "error"); return; }
      if (!dest) return;
    }
    const sep = dest.includes("\\") ? "\\" : "/";
    const trimmed = dest.replace(/[\\/]+$/, "");
    // A folder lands under this parent; a root keeps its separator ("C:\\").
    const dirParent = trimmed === "" || /^[a-zA-Z]:$/.test(trimmed) ? dest : trimmed;
    const fileCount = items.filter(e => !e.isDir).length;
    const dirCount  = items.filter(e =>  e.isDir).length;
    const summary =
      items.length === 1
        ? `Downloading ${items[0].isDir ? "folder " : ""}${items[0].name}…`
        : `Downloading ${fileCount} files${dirCount ? ` + ${dirCount} folder${dirCount === 1 ? "" : "s"}` : ""}…`;
    notify(summary, "info");
    const res = await runTransferBatch("download", items, (e, transferId, overwrite) => {
      const dest = e.isDir ? dirParent : `${trimmed}${sep}${e.name}`;
      const cmd = e.isDir ? "sftp_download_dir" : "sftp_download_file";
      return invoke(cmd, { sessionId, remotePath: e.path, localPath: dest, overwrite, transferId });
    });
    notifyBatchResult("download", items, res);
  };

  // Local → remote upload. Mirror of downloadItems — handles both files
  // (sftp_upload_file) and directories (sftp_upload_dir recursive walk).
  // Destination is whatever directory the remote pane is showing; if the
  // remote pane hasn't reported one yet (still loading), we bail with a
  // clear error rather than guessing the home dir.
  const uploadItems = async (items: FileEntry[], destDir?: string) => {
    if (!sessionId || items.length === 0) return;
    const dest = destDir || getOppositeDir?.();
    if (!dest) { notify("Open a directory in the remote pane first.", "error"); return; }
    const trimmed = dest.replace(/[\\/]+$/, "");
    const fileCount = items.filter((e) => !e.isDir).length;
    const dirCount = items.filter((e) => e.isDir).length;
    const summary = items.length === 1
      ? `Uploading ${items[0].isDir ? "folder " : ""}${items[0].name}…`
      : `Uploading ${fileCount} file${fileCount === 1 ? "" : "s"}${dirCount ? ` + ${dirCount} folder${dirCount === 1 ? "" : "s"}` : ""}…`;
    notify(summary, "info");
    const res = await runTransferBatch("upload", items, (e, transferId, overwrite) => {
      // For files we pass the remote target as a full file path; for dirs
      // we pass the remote PARENT and sftp_upload_dir hangs the source
      // basename underneath it (same convention as sftp_download_dir).
      const remotePath = e.isDir ? trimmed || "/" : `${trimmed}/${e.name}`;
      const cmd = e.isDir ? "sftp_upload_dir" : "sftp_upload_file";
      return invoke(cmd, { sessionId, localPath: e.path, remotePath, overwrite, transferId });
    });
    notifyBatchResult("upload", items, res);
  };

  sendItemsRef.current = isRemoteProvider ? downloadItems : uploadItems;

  // Remote: open in OS default editor with a save-watcher that re-uploads on
  // every change. Backed by the existing `sftp_open_remote_file` command.
  const liveEditEntry = async (entry: FileEntry) => {
    if (!sessionId || entry.isDir) return;
    if (openingPathsRef.current.has(entry.path)) {
      notify(`${entry.name} is already downloading`, "info");
      return;
    }
    openingPathsRef.current.add(entry.path);
    const transferId = `${Date.now()}-${Math.random().toString(36).slice(2, 8)}`;
    const gen = ++openGenRef.current;
    openGenByPathRef.current.set(entry.path, gen);
    setSelected(new Set([entry.path]));
    lastSelectedPathRef.current = entry.path;
    setOpenProgress((prev) => ({
      ...prev,
      [entry.path]: { bytes: 0, total: entry.size || 0 },
    }));
    const clearOpen = () => {
      if (openGenByPathRef.current.get(entry.path) !== gen) return;
      openGenByPathRef.current.delete(entry.path);
      setOpenProgress((prev) => {
        if (!(entry.path in prev)) return prev;
        const next = { ...prev };
        delete next[entry.path];
        return next;
      });
    };
    try {
      await invoke("sftp_open_remote_file", { sessionId, remotePath: entry.path, transferId });
    } catch (err: any) {
      clearOpen();
      const message = String(err);
      if (message === "cancelled" || message.endsWith("cancelled")) notify("Open cancelled", "info");
      else if (message === "already downloading" || message.endsWith("already downloading")) {
        notify(`${entry.name} is already downloading`, "info");
      } else notify(`Open failed: ${err}`, "error");
      return;
    } finally {
      openingPathsRef.current.delete(entry.path);
    }
    setOpenProgress((prev) => {
      const cur = prev[entry.path];
      if (!cur || openGenByPathRef.current.get(entry.path) !== gen) return prev;
      const total = cur.total || entry.size || 1;
      return { ...prev, [entry.path]: { bytes: total, total } };
    });
    window.setTimeout(clearOpen, 280);
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

  // Current-directory totals for the status bar. Size sums regular files
  // only: a directory row's `size` is its own inode, not the files inside
  // it, and we do not walk children. The ".." row is not in `entries`.
  const dirStats = (() => {
    let folderCount = 0;
    let fileCount = 0;
    let totalBytes = 0;
    let selectedBytes = 0;
    let selectedCount = 0;
    for (const entry of sortedEntries) {
      const isSelected = selected.has(entry.path);
      if (isSelected) selectedCount++;
      if (entry.isDir) {
        folderCount++;
        continue;
      }
      fileCount++;
      // A link's size is its target's once resolved; that file lives
      // elsewhere and is not part of this directory's total.
      if (entry.isSymlink) continue;
      totalBytes += entry.size || 0;
      if (isSelected) selectedBytes += entry.size || 0;
    }
    return {
      folderCount,
      fileCount,
      totalBytes,
      totalCount: sortedEntries.length,
      selectedBytes,
      selectedCount,
    };
  })();

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
  // Every selected row is hidden by the name filter: the bar stays (so the
  // selection can be cleared) but there is nothing to act on.
  const noVisibleSelection = dirStats.selectedCount === 0;
  // Selection-bar button. Taller on a phone so it is an easy tap target.
  const actionBtn ="h-8 sm:h-7 min-w-8 px-2 sm:px-2.5 rounded-md border flex items-center justify-center gap-1.5 transition-colors disabled:opacity-40 disabled:cursor-not-allowed";

  const openDirProperties = async () => {
    if (!sessionId || !currentPath) return;
    const meta = await invoke<{ permissions?: number; uid?: number; gid?: number }>("sftp_stat", {
      sessionId,
      path: currentPath,
    });
    const trimmed = currentPath.replace(/[\\/]+$/, "");
    const name = trimmed.split(/[\\/]/).pop() || currentPath;
    setModal({
      type: "properties",
      entry: {
        name,
        path: currentPath,
        isDir: true,
        size: 0,
        permissions: meta.permissions,
        uid: meta.uid,
        gid: meta.gid,
      },
      v1: permissionOctal(meta.permissions),
      v2: meta.uid?.toString(),
    });
  };

  const openDirInTerminal = async () => {
    if (!currentPath) return;
    if (!terminalId) {
      notify("No terminal is open for this session", "error");
      return;
    }
    const line = `cd ${shellSingleQuote(currentPath)}\r`;
    const data = Array.from(new TextEncoder().encode(line));
    await invoke("write_terminal_data", { terminalId, data });
    onRevealTerminal?.();
  };

  const onListBackgroundMenu = (e: React.MouseEvent) => {
    if (disabled || loading) return;
    if (selected.size > 0) {
      const anchor = entries.find((en) => selected.has(en.path));
      if (anchor) openMenu(e, anchor);
      else e.preventDefault();
      return;
    }
    if (!isRemote) return;
    e.preventDefault();
    e.stopPropagation();
    const menuW = 220;
    const menuH = 200;
    const x = Math.min(e.clientX, window.innerWidth - menuW - 4);
    const y = Math.min(e.clientY, window.innerHeight - menuH - 4);
    setContextMenu(null);
    setDirMenu({ x: Math.max(4, x), y: Math.max(4, y) });
  };

  return (
    <div
      data-fs-pane={provider.id}
      data-fs-current-path={currentPath}
      aria-busy={loading}
      className="flex-1 flex flex-col h-full bg-[#09090b] p-1.5 gap-1.5 overflow-hidden relative select-none"
    >
      {disabled && (
        <div className="absolute inset-0 z-[80] bg-black/55 backdrop-blur-[1px] flex items-center justify-center text-zinc-300 text-xs font-mono uppercase">
          <span className="px-3 py-1.5 bg-red-500/15 border border-red-500/30 rounded text-red-300">
            Session disconnected
          </span>
        </div>
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
          {/* Path bar. Stretches to the row height, which the icon buttons
              set, so both are the same height. Shows clickable segments;
              a click on the free space switches to the text input. Both
              views fill this box absolutely, so the switch cannot resize
              the header. */}
          <div className="flex-1 min-w-0 relative self-stretch">
            {!inputFocused ? (
              <div
                ref={crumbsRef}
                onClick={editPath}
                // A vertical wheel does not scroll a horizontal overflow on
                // its own; without this the parents of a long path are
                // out of reach.
                onWheel={(e) => { if (!e.deltaX) e.currentTarget.scrollLeft += e.deltaY; }}
                className="absolute inset-0 flex items-center px-2 bg-white/[0.04] border border-white/10 rounded text-[11px] font-mono overflow-x-auto no-scrollbar cursor-text hover:border-white/20"
              >
                {crumbs.length === 0 && <span className="shrink-0 text-zinc-500">Path…</span>}
                {crumbs.map((c, i) => {
                  const current = i === crumbs.length - 1;
                  const navigable = !current && c.navigable;
                  return (
                    <React.Fragment key={c.path}>
                      {i > 0 && !/^[\\/]$/.test(crumbs[i - 1].label) && (
                        <span className="shrink-0 text-zinc-600">{provider.pathSep}</span>
                      )}
                      {navigable ? (
                        <button
                          type="button"
                          onClick={(e) => { e.stopPropagation(); fetch(c.path); }}
                          title={c.path}
                          data-fs-drop-path={c.path}
                          className={`shrink-0 whitespace-nowrap cursor-pointer transition-colors hover:text-indigo-300 ${
                            dropHover === c.path ? "text-indigo-200 bg-indigo-500/30 rounded ring-1 ring-indigo-400" : "text-zinc-300"
                          }`}
                        >
                          {c.label}
                        </button>
                      ) : (
                        <span className={`shrink-0 whitespace-nowrap ${current ? "text-zinc-100" : "text-zinc-500"}`}>{c.label}</span>
                      )}
                    </React.Fragment>
                  );
                })}
                <button type="button" title="Edit path" aria-label="Edit path" className="flex-1 min-w-4 self-stretch cursor-text focus:outline-none" />
              </div>
            ) : (
            <input
              type="text"
              autoFocus
              value={tempInput}
              onChange={(e) => { setTempInput(e.target.value); setActiveSuggestion(-1); }}
              onFocus={(e) => {
                clearBlurTimer();
                setInputFocused(true);
                const end = e.currentTarget.value.length;
                e.currentTarget.setSelectionRange(end, end);
              }}
              onBlur={() => { blurTimerRef.current = window.setTimeout(() => setInputFocused(false), 250); }}
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
              className="absolute inset-0 w-full h-full px-2 bg-white/[0.04] border border-white/10 rounded text-[11px] text-zinc-100 font-mono focus:outline-none focus:border-indigo-400/50 focus:bg-white/10"
            />
            )}
            {inputFocused && suggestions.length > 0 && (
              <div className="absolute top-full mt-1 left-0 right-0 max-h-[220px] overflow-y-auto z-50 bg-[#0c0c0e]/95 border border-white/10 rounded-lg shadow-2xl p-1 backdrop-blur-md font-mono text-[11px] text-zinc-200 no-scrollbar">
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
        <div className="flex items-center gap-1 shrink-0 relative">
          {provider.id === "local" && (
            <button
              onClick={async () => {
                if (IS_ANDROID) {
                  // First open pulls the quick-dir list; subsequent opens
                  // reuse the cached list so scoped-storage probes don't
                  // rerun on every click.
                  if (!androidQuickDirs) {
                    try {
                      const dirs = await invoke<{ label: string; path: string }[]>("android_quick_dirs");
                      setAndroidQuickDirs(dirs);
                    } catch (err: any) {
                      notify(`Browse failed: ${err}`, "error");
                      return;
                    }
                  }
                  setAndroidPickerOpen((v) => !v);
                  return;
                }
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
          {IS_ANDROID && androidPickerOpen && androidQuickDirs && (
            <div className="absolute top-[24px] right-0 z-50 min-w-[180px] bg-[#0c0c0e]/95 border border-white/10 rounded-lg shadow-2xl p-1 backdrop-blur-md font-mono text-[11px]">
              {androidQuickDirs.length === 0 ? (
                <div className="p-2 text-zinc-400 text-[10.5px]">
                  No writable locations found. On Android 11+ shared storage
                  requires SAF — the app can only read/write its own scoped
                  storage directly.
                </div>
              ) : androidQuickDirs.map((d) => (
                <button
                  key={d.path}
                  onClick={() => { setAndroidPickerOpen(false); fetch(d.path); }}
                  className="w-full flex items-center gap-2 p-1.5 rounded text-left text-zinc-200 hover:bg-white/10 hover:text-white"
                >
                  <Folder size={11} className="text-indigo-300 shrink-0" />
                  <div className="flex-1 min-w-0">
                    <div className="truncate">{d.label}</div>
                    <div className="truncate text-[9.5px] text-zinc-500">{d.path}</div>
                  </div>
                </button>
              ))}
            </div>
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

      {/* File card: the scrolling list, then the docked selection bar. The
          bar is a flex sibling of the list, so it shortens the list instead
          of covering the last rows. (The transfers bar lives on the
          SftpWorkspace root, so it stays visible on the Mirror tab too.) */}
      <div className={`flex-1 min-h-0 flex flex-col border rounded-lg bg-[#121214] overflow-hidden transition-colors duration-200 shadow-2xl shadow-indigo-950/10 ${dragOver ? "border-indigo-400" : "border-indigo-500/30"}`}>
      <div className="relative flex-1 min-h-0 flex flex-col">
      {/* List */}
      <div
        ref={dropTargetRef}
        onDragOver={onDragOver}
        onDragLeave={onDragLeave}
        onDrop={onDrop}
        className={`flex-1 min-h-0 flex flex-col overflow-auto transition-colors duration-200 ${dragOver ? "bg-indigo-950/10" : ""}`}
      >
        {/* The select column is conditionally injected — only present when
            there's at least one selected row. With nothing selected the row
            content reclaims the 22px and the list reads cleanly. Discovery
            paths into multi-select that work without the column visible:
            row click (single select), Ctrl/Shift-click (extend), Ctrl+A
            (select all), right-click → context menu, and the header bar's
            select-all toggle to the right of the address bar. */}
        <div className={`group min-w-full grid ${
          showPerms
            ? "grid-cols-[22px_1fr] sm:grid-cols-[22px_minmax(180px,1fr)_65px_125px_85px]"
            : "grid-cols-[22px_1fr] sm:grid-cols-[22px_minmax(180px,1fr)_75px_125px]"
        } gap-1.5 px-2.5 bg-[#161619] border-b border-white/5 font-mono text-[10.5px] text-zinc-300 select-none font-bold shrink-0 sticky top-0 z-10 shadow-md`}>
          {/* Select-all — the 22px column is ALWAYS reserved (both here and in
              every row) so starting a selection never shifts the columns
              sideways. The control is revealed on header hover, or whenever a
              selection is already active. */}
          <div
            className={`bg-[#161619] flex items-center justify-center py-1.5 cursor-pointer hover:text-white transition-opacity ${selected.size > 0 ? "opacity-100" : "opacity-0 group-hover:opacity-100"}`}
            onClick={(e) => { e.stopPropagation(); toggleSelectAll(); }}
            title={allSelected ? "Deselect all" : "Select all"}
          >
            {allSelected ? <CheckSquare size={12} className="text-indigo-300" /> : <Square size={12} className="text-zinc-500" />}
          </div>
          <div className="bg-[#161619] cursor-pointer hover:text-white py-1.5" onClick={() => toggleSort("name")}>
            NAME {sortIcon("name")}
          </div>
          <div className="hidden sm:block bg-[#161619] cursor-pointer hover:text-white text-right py-1.5" onClick={() => toggleSort("size")}>
            SIZE {sortIcon("size")}
          </div>
          <div className="hidden sm:block bg-[#161619] cursor-pointer hover:text-white text-right py-1.5" onClick={() => toggleSort("modified")}>
            CHANGED {sortIcon("modified")}
          </div>
          {showPerms && (
            <div className="hidden sm:block bg-[#161619] cursor-pointer hover:text-white text-right py-1.5" onClick={() => toggleSort("permissions")}>
              RIGHTS {sortIcon("permissions")}
            </div>
          )}
        </div>

        <div className="min-w-full flex-1 p-1 font-mono text-[11px]"
             onContextMenu={onListBackgroundMenu}
             onClick={(e) => {
               // Click landed on the bare list background (not on a row, since
               // rows stopPropagation via their own onClick chain implicitly
               // by being the click target). Clear selection so users can
               // escape a multi-selection without a keyboard shortcut.
               if (e.target === e.currentTarget && !dragJustEndedRef.current) {
                 setSelected(new Set());
                 lastSelectedPathRef.current = null;
               }
             }}>
          {loading && sortedEntries.length === 0 ? (
            <div className="text-center py-14 text-zinc-400">Loading…</div>
          ) : (
            <>
          {showParent && (
            <div
              onDoubleClick={(e) => { e.stopPropagation(); goUp(); }}
              title={cameViaLink ? `Back to ${linkBack!.back}` : "Parent directory"}
              data-fs-drop-path={provider.parentPath(currentPath)}
              className={`grid ${
                showPerms
                  ? "grid-cols-[22px_1fr] sm:grid-cols-[22px_minmax(180px,1fr)_65px_125px_85px]"
                  : "grid-cols-[22px_1fr] sm:grid-cols-[22px_minmax(180px,1fr)_75px_125px]"
              } gap-1.5 px-2.5 py-1 border-l-2 border-transparent cursor-pointer transition-colors items-center text-zinc-200 hover:bg-white/5 hover:text-white ${
                dropHover !== null && dropHover === provider.parentPath(currentPath) ? "bg-indigo-500/20 ring-1 ring-inset ring-indigo-400" : ""
              }`}
            >
              <div />
              <div className="flex items-center gap-2 min-w-0 pr-1">
                <FolderUp size={12} className="text-indigo-300 shrink-0" />
                <div className="truncate text-zinc-100 text-[11px]">..</div>
              </div>
              <div className="hidden sm:block" />
              <div className="hidden sm:block" />
              {showPerms && <div className="hidden sm:block" />}
            </div>
          )}
          {sortedEntries.length === 0 ? (
            showParent ? null : <div className="text-center py-14 text-zinc-500">Empty</div>
          ) : (
            sortedEntries.map((entry) => {
              const isSel = selected.has(entry.path);
              const opening = openProgress[entry.path];
              const openPct = opening && opening.total > 0
                ? Math.min(100, (opening.bytes / opening.total) * 100)
                : null;
              return (
              <div
                key={entry.path}
                onMouseDown={(e) => handleRowMouseDown(e, entry)}
                onContextMenu={(e) => openMenu(e, entry)}
                onClick={(e) => onRowClick(e, entry, sortedEntries)}
                onDoubleClick={() => { openEntry(entry); }}
                data-fs-row-path={entry.path}
                data-fs-row-isdir={entry.isDir ? "1" : "0"}
                data-fs-drop-path={entry.isDir ? entry.path : undefined}
                className={`group isolate relative overflow-hidden grid ${
                  showPerms
                    ? "grid-cols-[22px_1fr] sm:grid-cols-[22px_minmax(180px,1fr)_65px_125px_85px]"
                    : "grid-cols-[22px_1fr] sm:grid-cols-[22px_minmax(180px,1fr)_75px_125px]"
                } gap-1.5 px-2.5 py-1 border-l-2 cursor-pointer transition-colors items-center ${
                  dropHover === entry.path
                    ? "bg-indigo-500/20 ring-1 ring-inset ring-indigo-400 border-indigo-400 text-white"
                    : opening
                    ? "border-indigo-400 text-indigo-100 font-bold"
                    : isSel
                    ? "bg-indigo-950/40 border-indigo-400 text-indigo-100 font-bold"
                    : "border-transparent text-zinc-200 hover:bg-white/5 hover:text-white"
                }`}
              >
                {opening && (
                  <>
                    <div className="absolute inset-0 -z-10 bg-indigo-950/25 pointer-events-none" />
                    {openPct === null ? (
                      <div className="absolute inset-y-0 left-0 -z-10 w-1/3 bg-indigo-400/50 pointer-events-none open-progress-indeterminate" />
                    ) : (
                      <div
                        className="absolute inset-y-0 left-0 -z-10 bg-indigo-400/45 pointer-events-none transition-[width] duration-150 ease-linear"
                        style={{ width: `${openPct === 0 ? 0 : Math.max(openPct, 1.5)}%` }}
                      />
                    )}
                  </>
                )}
                {/* Per-row checkbox. Its 22px column is always reserved, so the
                    box appearing never shifts the row's content sideways. Hidden
                    at rest, revealed on row hover or whenever a selection is
                    already active (so any row can be toggled). */}
                <div
                  className={`flex items-center justify-center transition-opacity ${isSel || selected.size > 0 ? "opacity-100" : "opacity-0 group-hover:opacity-100"}`}
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
                <div className="flex items-center gap-2 min-w-0 pr-1">
                  {/* A link's kind is unknown until STAT answers: a bare
                      chain until then, file or folder link afterwards. */}
                  {entry.isSymlink
                    ? (entry.linkState === "pending"
                        ? <Link size={12} className="text-zinc-500 shrink-0" />
                        : entry.isDir
                        ? <FolderSymlink size={12} className="text-cyan-300 shrink-0" />
                        : <FileSymlink size={12} className={`${
                            entry.linkState === "broken" ? "text-rose-400"
                              : entry.linkState === "error" ? "text-amber-400"
                              : "text-cyan-300"
                          } shrink-0`} />)
                    : entry.isDir
                    ? <Folder size={12} className="text-indigo-300 shrink-0" />
                    : <File size={12} className="text-zinc-500 shrink-0" />}
                  <div className="min-w-0 flex-1">
                    <div
                      className="truncate text-zinc-100 text-[11px]"
                      title={entry.isSymlink
                        ? `${entry.name} → ${entry.linkTarget ?? "…"}${
                            entry.linkState === "broken" ? " (broken)"
                              : entry.linkState === "error" ? ` (${entry.linkError ?? "target unreadable"})`
                              : ""
                          }`
                        : undefined}
                    >
                      {entry.name}
                      {entry.isSymlink && entry.linkTarget && (
                        <span className={`font-normal ${
                          entry.linkState === "broken" ? "text-rose-400/80"
                            : entry.linkState === "error" ? "text-amber-400/80"
                            : "text-zinc-500"
                        }`}> → {entry.linkTarget}</span>
                      )}
                    </div>
                    {/* Narrow-viewport subline: on < sm the SIZE / CHANGED /
                        RIGHTS cells are display:none (so they don't force
                        horizontal scroll), and their info collapses into
                        this muted second line under the filename. */}
                    <div className="sm:hidden truncate text-[10px] text-zinc-500 font-mono">
                      {[
                        entry.isDir ? null : formatSize(entry.size),
                        formatTime(entry.modified, isRemote),
                        showPerms ? formatRights(entry.isDir, entry.permissions) : null,
                      ].filter(Boolean).join(" · ")}
                    </div>
                  </div>
                </div>
                <div className="hidden sm:block text-right text-[10.5px] text-zinc-300 font-sans">
                  {entry.isDir ? "" : formatSize(entry.size)}
                </div>
                <div className="hidden sm:block text-right text-[9.5px] text-zinc-400 truncate">
                  {formatTime(entry.modified, isRemote)}
                </div>
                {showPerms && (
                  <div className="hidden sm:flex text-right text-[10.5px] text-zinc-300 font-mono opacity-90 items-center justify-end gap-1">
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
            </>
          )}
        </div>
      </div>

      {(notification || pendingMove || busyJobs.length > 0) && (
        // Bottom-right of the list area: never over the path bar, and never
        // over the docked selection bar below the list. The
        // move row is its own element so a later notify() cannot take
        // the cancel button with it.
        <div className="absolute bottom-2 right-2 left-2 z-30 flex flex-col items-end gap-1.5 pointer-events-none">
          {notification && (
            <div className={`pointer-events-auto max-w-full px-3 py-1.5 rounded-lg border text-[11px] font-mono shadow-2xl backdrop-blur-md animate-in fade-in slide-in-from-bottom-4 duration-300 ${
              notification.type === "success" ? "bg-emerald-950/90 border-emerald-500/30 text-emerald-400" :
              notification.type === "error"   ? "bg-rose-950/90 border-rose-500/30 text-rose-400" :
                                                "bg-indigo-950/90 border-indigo-500/30 text-indigo-400"
            }`}>{notification.msg}</div>
          )}
          {pendingMove && (
            <div className="pointer-events-auto max-w-full px-3 py-1.5 rounded-lg border text-[11px] font-mono shadow-2xl backdrop-blur-md bg-indigo-950/90 border-indigo-500/30 text-indigo-400 flex items-center gap-2">
              <span className="min-w-0">
                {pendingMove.waiting
                  ? `Waiting to move ${pendingMove.label} → ${pendingMove.targetDir}…`
                  : `Moving ${pendingMove.label} → ${pendingMove.targetDir}…`}
              </span>
              <button
                type="button"
                title="Cancel move"
                onClick={() => cancelPendingMoveRef.current()}
                className="shrink-0 p-0.5 rounded hover:bg-white/15 text-zinc-300 hover:text-rose-300"
              >
                <X size={12} />
              </button>
            </div>
          )}
          {busyJobs.map((job) => (
            <div key={job.id} className="pointer-events-auto max-w-full px-3 py-1.5 rounded-lg border text-[11px] font-mono shadow-2xl backdrop-blur-md bg-indigo-950/90 border-indigo-500/30 text-indigo-400 flex items-center gap-2">
              <RefreshCw size={11} className="shrink-0 animate-spin" />
              <span className="min-w-0">{job.label}</span>
            </div>
          ))}
        </div>
      )}
      </div>

      {/* Selection bar — docked at the bottom of the card for any non-empty
          selection, so send / move / delete is one click without a
          right-click. The bar is a size container: when the PANEL (not the
          window) is narrow the buttons drop their labels (see
          `.selection-bar` in App.css), and they wrap rather than clip on the
          narrowest side panel; the title still names the action. The count
          and size are of the visible selected rows, which is what the
          actions act on; rows hidden by the name filter are only noted. */}
      {selected.size > 0 && (
        <div className="selection-bar shrink-0 flex flex-wrap items-center gap-x-3 gap-y-1.5 px-3 py-1.5 border-t border-indigo-500/25 bg-indigo-950/25 font-mono text-[10.5px]">
          <div className="flex flex-wrap items-center gap-x-2 gap-y-0.5 min-w-0">
            <span className="font-bold text-zinc-100 whitespace-nowrap">{dirStats.selectedCount} selected</span>
            {dirStats.selectedBytes > 0 && (
              <>
                <span className="w-px h-3 bg-white/15 shrink-0" />
                <span className="text-zinc-400 whitespace-nowrap">{formatSize(dirStats.selectedBytes)}</span>
              </>
            )}
            {selected.size > dirStats.selectedCount && (
              <span className="text-zinc-500 whitespace-nowrap" title="Selected rows the name filter hides; actions skip them">
                +{selected.size - dirStats.selectedCount} hidden by filter
              </span>
            )}
          </div>
          <div className="ml-auto flex flex-wrap justify-end items-center gap-1.5">
            {isRemote ? (
              <button
                onClick={() => downloadItems(sortedEntries.filter(e => selected.has(e.path)))}
                title={getOppositeDir?.()
                  ? `Download to ${getOppositeDir!()}`
                  : "Pick a destination folder…"}
                disabled={noVisibleSelection}
                className={`${actionBtn} border-emerald-500/30 bg-emerald-500/10 text-emerald-300 hover:bg-emerald-500/20`}
              >
                <Download size={12} /> <span className="selection-bar-label">Download</span>
              </button>
            ) : (
              <button
                onClick={() => uploadItems(sortedEntries.filter(e => selected.has(e.path)))}
                title={getOppositeDir?.()
                  ? `Upload to ${getOppositeDir!()}`
                  : "Open a remote directory first…"}
                disabled={noVisibleSelection || !getOppositeDir?.()}
                className={`${actionBtn} border-sky-500/30 bg-sky-500/10 text-sky-300 hover:bg-sky-500/20`}
              >
                <Upload size={12} /> <span className="selection-bar-label">Upload</span>
              </button>
            )}
            <button
              onClick={() => setModal({ type: "move-bulk", v1: currentPath })}
              title="Move selected items…"
              disabled={noVisibleSelection}
              className={`${actionBtn} border-white/10 bg-white/[0.04] text-zinc-200 hover:bg-white/10`}
            >
              <Move size={12} /> <span className="selection-bar-label">Move…</span>
            </button>
            <button
              onClick={() => removeItems(sortedEntries.filter(e => selected.has(e.path)))}
              title="Delete selected items"
              disabled={noVisibleSelection}
              className={`${actionBtn} border-rose-500/30 bg-rose-500/10 text-rose-300 hover:bg-rose-500/20`}
            >
              <Trash2 size={12} /> <span className="selection-bar-label">Delete</span>
            </button>
            <button
              onClick={clearSelection}
              title="Clear selection"
              className={`${actionBtn} border-white/10 bg-white/[0.04] text-zinc-400 hover:bg-white/10 hover:text-zinc-200`}
            >
              <X size={12} /> <span className="selection-bar-label">Clear</span>
            </button>
          </div>
        </div>
      )}

      </div>

      <div
        className="shrink-0 h-7 px-2.5 flex items-center justify-between gap-3 rounded-lg border border-white/5 bg-[#121214] font-mono text-[10.5px] text-zinc-500"
        title="Size counts files in this directory only, not files inside subfolders"
      >
        <div className="flex items-center gap-2 min-w-0 truncate">
          <span className="inline-flex items-center gap-1 shrink-0">
            <Folder size={11} className="text-indigo-300" />
            <span className="text-zinc-200">{dirStats.folderCount}</span>
            {dirStats.folderCount === 1 ? "folder" : "folders"}
          </span>
          <span className="inline-flex items-center gap-1 shrink-0">
            <File size={11} className="text-zinc-500" />
            <span className="text-zinc-200">{dirStats.fileCount}</span>
            {dirStats.fileCount === 1 ? "file" : "files"}
          </span>
          <span className="w-px h-3 bg-white/10 shrink-0" />
          <span className="truncate">
            <span className="text-zinc-200">{dirStats.totalCount}</span>
            {dirStats.totalCount === 1 ? " item" : " items"}
          </span>
        </div>
        <div className="flex items-center gap-2 shrink-0">
          {/* On a phone the selection bar already shows this. */}
          <span className="hidden sm:inline">
            Selected{" "}
            <span className="text-zinc-100">{dirStats.selectedCount} · {formatSize(dirStats.selectedBytes)}</span>
          </span>
          <span className="hidden sm:block w-px h-3 bg-white/10" />
          <span>
            Total size{" "}
            <span className="text-zinc-100">{formatSize(dirStats.totalBytes)}</span>
          </span>
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
        // The menu holds the entry as it was when opened; a link may have
        // been resolved since. Every per-item action below uses the live
        // one: the snapshot of a link carries lstat data (mode 777, not a
        // directory) that must not drive chmod or the file/folder choice.
        const menuEntry = entries.find(e => e.path === contextMenu.entry.path) ?? contextMenu.entry;
        // Not confirmed to point at a file (pending, broken, unreadable), so
        // file actions do not apply. Only Open, which resolves first and
        // explains a failure, makes sense.
        const unresolvedLink = menuEntry.isSymlink && menuEntry.linkState !== "ok";
        return (
        <div ref={menuRef}
          style={{ top: contextMenu.y, left: contextMenu.x }}
          className="fixed z-[9999] min-w-[180px] bg-[#0c0c0e] border border-white/10 rounded-lg shadow-2xl p-1 backdrop-blur-md font-mono text-[11.5px] text-zinc-200">

          {/* Primary action: open folder, or transfer/edit file. The exact
              set depends on which side this panel is on. */}
          {(menuEntry.isDir || unresolvedLink) && !multi ? (
            <button onClick={() => { setContextMenu(null); openEntry(menuEntry); }}
              className="w-full flex items-center gap-2 p-1.5 rounded hover:bg-white/10 text-left hover:text-white">
              {menuEntry.isDir
                ? <Folder size={11} className="text-indigo-400" />
                : <Link size={11} className="text-cyan-300" />}
              <span>Open</span>
            </button>
          ) : isRemote ? (
            <>
              <button onClick={() => { setContextMenu(null); downloadItems(acting); }}
                className="w-full flex items-center gap-2 p-1.5 rounded hover:bg-white/10 text-left hover:text-white">
                <Download size={11} className="text-emerald-400" />
                <span>Download{multi ? ` (${selectedFileCount})` : "…"}</span>
              </button>
              {/* Live-edit needs a local temp file + a native OS editor + a
                  filesystem watcher. Android has none of those in a form we
                  can bridge, so the backend command returns an error there
                  — hide the menu item entirely rather than surface it. */}
              {!multi && !menuEntry.isDir && !IS_ANDROID && (
                <button onClick={() => { setContextMenu(null); openEntry(menuEntry); }}
                  className="w-full flex items-center gap-2 p-1.5 rounded hover:bg-white/10 text-left hover:text-white">
                  <ExternalLink size={11} className="text-indigo-400" /><span>Edit (auto-upload)</span>
                </button>
              )}
            </>
          ) : (
            !multi && !menuEntry.isDir && !IS_ANDROID && (
              <button onClick={() => { setContextMenu(null); openLocalEntry(menuEntry); }}
                className="w-full flex items-center gap-2 p-1.5 rounded hover:bg-white/10 text-left hover:text-white">
                <ExternalLink size={11} className="text-indigo-400" /><span>Open</span>
              </button>
            )
          )}

          {!multi && menuEntry.isSymlink && menuEntry.linkState !== "broken" && provider.realPath && (
            <button onClick={() => { setContextMenu(null); goToLinkTarget(menuEntry); }}
              title={menuEntry.linkTarget}
              className="w-full flex items-center gap-2 p-1.5 rounded hover:bg-white/10 text-left hover:text-white">
              <CornerDownRight size={11} className="text-cyan-300" /><span>Go to target</span>
            </button>
          )}

          {!isRemote && !multi && !IS_ANDROID && (
            <button onClick={() => { setContextMenu(null); revealLocalEntry(menuEntry); }}
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
          {/* A broken link has no target to chmod; anything else is
              followed on click. */}
          {!multi && provider.chmod && menuEntry.linkState !== "broken" && (
            <button onClick={() => { setContextMenu(null); openProperties(menuEntry); }}
              className="w-full flex items-center gap-2 p-1.5 rounded hover:bg-white/10 text-left hover:text-white">
              <Shield size={11} /><span>Properties</span>
            </button>
          )}

          <div className="h-px bg-white/5 my-1" />
          <button onClick={() => { setContextMenu(null); openArchiveDialog(acting); }}
            className="w-full flex items-center gap-2 p-1.5 rounded hover:bg-white/10 text-left hover:text-white">
            <Archive size={11} className="text-amber-300" />
            <span>{multi ? `Add to archive (${acting.length})…` : "Add to archive…"}</span>
          </button>
          {!multi && !contextMenu.entry.isDir && archiveKind(contextMenu.entry.name) && (
            <>
              <button onClick={() => { setContextMenu(null); extractEntry(contextMenu.entry, false); }}
                className="w-full flex items-center gap-2 p-1.5 rounded hover:bg-white/10 text-left hover:text-white">
                <PackageOpen size={11} className="text-amber-300" /><span>Extract here</span>
              </button>
              {extractFolderName(contextMenu.entry.name) && (
                <button onClick={() => { setContextMenu(null); extractEntry(contextMenu.entry, true); }}
                  className="w-full flex items-center gap-2 p-1.5 rounded hover:bg-white/10 text-left hover:text-white">
                  <PackageOpen size={11} className="text-amber-300 shrink-0" />
                  <span className="truncate max-w-[260px]">Extract to “{extractFolderName(contextMenu.entry.name)}{provider.pathSep}”</span>
                </button>
              )}
            </>
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

      {dirMenu && createPortal(
        <div
          ref={menuRef}
          className="fixed z-[9999] min-w-[210px] bg-[#161619] border border-white/10 rounded-lg shadow-2xl p-1 font-mono text-[11px] text-zinc-200"
          style={{ left: dirMenu.x, top: dirMenu.y }}
          onMouseDown={(e) => e.stopPropagation()}
        >
          <button
            onClick={() => { setDirMenu(null); setModal({ type: "newfile", v1: "" }); }}
            className="w-full flex items-center gap-2 p-1.5 rounded hover:bg-white/10 text-left hover:text-white"
          >
            <File size={11} className="text-zinc-300" /><span>New File</span>
          </button>
          <button
            onClick={() => { setDirMenu(null); setModal({ type: "mkdir", v1: "" }); }}
            className="w-full flex items-center gap-2 p-1.5 rounded hover:bg-white/10 text-left hover:text-white"
          >
            <Plus size={11} className="text-indigo-300" /><span>New Folder</span>
          </button>
          <button
            onClick={() => { setDirMenu(null); setModal({ type: "symlink", v1: "", v2: "" }); }}
            className="w-full flex items-center gap-2 p-1.5 rounded hover:bg-white/10 text-left hover:text-white"
          >
            <Link size={11} className="text-sky-300" /><span>New Symbolic Link</span>
          </button>
          <div className="h-px bg-white/5 my-1" />
          <button
            onClick={() => {
              setDirMenu(null);
              void openDirInTerminal().catch((err) => notify(String(err), "error"));
            }}
            className="w-full flex items-center gap-2 p-1.5 rounded hover:bg-white/10 text-left hover:text-white"
          >
            <Terminal size={11} className="text-emerald-300" /><span>Open in Terminal</span>
          </button>
          <button
            onClick={() => {
              setDirMenu(null);
              void openDirProperties().catch((err) => notify(String(err), "error"));
            }}
            className="w-full flex items-center gap-2 p-1.5 rounded hover:bg-white/10 text-left hover:text-white"
          >
            <Shield size={11} /><span>Folder Properties</span>
          </button>
        </div>,
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
                 modal.type === "mkdir" ? "New Folder" :
                 modal.type === "newfile" ? "New File" :
                 modal.type === "symlink" ? "New Symbolic Link" :
                 modal.type === "move" ? "Move to…" :
                 modal.type === "archive" ? ((modal.items?.length ?? 0) > 1 ? `Archive ${modal.items!.length} items` : "Add to archive") :
                 modal.type === "move-bulk" ? `Move ${dirStats.selectedCount} items to…` : "Properties"}
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
              ) : modal.type === "symlink" ? (
                <>
                  <div>
                    <label className="text-[10px] text-zinc-400 block mb-1">Link name</label>
                    <input type="text" autoFocus value={modal.v1 || ""}
                      onChange={(e) => setModal({ ...modal, v1: e.target.value })}
                      onKeyDown={(e) => e.key === "Enter" && submitModal()}
                      className="w-full h-7 px-2 bg-white/5 border border-white/5 rounded text-zinc-200 focus:outline-none focus:border-indigo-400/40 text-[11.5px] font-mono" />
                  </div>
                  <div>
                    <label className="text-[10px] text-zinc-400 block mb-1">Target</label>
                    <input type="text" value={modal.v2 || ""}
                      onChange={(e) => setModal({ ...modal, v2: e.target.value })}
                      onKeyDown={(e) => e.key === "Enter" && submitModal()}
                      className="w-full h-7 px-2 bg-white/5 border border-white/5 rounded text-zinc-200 focus:outline-none focus:border-indigo-400/40 text-[11.5px] font-mono" />
                  </div>
                </>
              ) : modal.type === "archive" ? (
                <>
                  <div>
                    <label className="text-[10px] text-zinc-400 block mb-1">Archive name</label>
                    <input type="text" autoFocus value={modal.v1 || ""}
                      onFocus={(e) => e.currentTarget.select()}
                      onChange={(e) => setModal({ ...modal, v1: e.target.value })}
                      onKeyDown={(e) => e.key === "Enter" && submitModal()}
                      className="w-full h-7 px-2 bg-white/5 border border-white/5 rounded text-zinc-200 focus:outline-none focus:border-indigo-400/40 text-[11.5px] font-mono" />
                  </div>
                  <div>
                    <label className="text-[10px] text-zinc-400 block mb-1">Format</label>
                    <div className="grid grid-cols-3 gap-1">
                      {ARCHIVE_FORMATS.map((f) => (
                        <button
                          key={f.format}
                          type="button"
                          aria-pressed={modal.v2 === f.format}
                          onClick={() => setModal({ ...modal, v2: f.format })}
                          className={`h-7 rounded border transition-colors ${
                            modal.v2 === f.format
                              ? "bg-indigo-500/20 border-indigo-400/40 text-indigo-200"
                              : "bg-white/[0.04] border-white/10 text-zinc-400 hover:bg-white/[0.08]"
                          }`}
                        >
                          {f.ext}
                        </button>
                      ))}
                    </div>
                  </div>
                  <div className="text-[10px] text-zinc-500 truncate" title={provider.joinPath(currentPath, archiveFileName(modal.v1 || "", (modal.v2 || "zip") as ArchiveFormat))}>
                    Saved here as{" "}
                    <span className="text-zinc-300">{archiveFileName(modal.v1 || "", (modal.v2 || "zip") as ArchiveFormat)}</span>
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
                <button onClick={submitModal} className="px-3 h-7 rounded bg-indigo-500 text-white font-bold hover:bg-indigo-600">{modal.type === "archive" ? "Create" : "Apply"}</button>
              </div>
            </div>
          </div>
        </div>
      )}

      {loading && !disabled && (
        <div className="absolute inset-0 z-[60] bg-black/50 backdrop-blur-[1px] flex items-center justify-center cursor-wait">
          <div className="flex items-center gap-2 px-3 py-1.5 rounded-lg bg-[#121214] border border-white/10 text-zinc-200 text-[11px] font-mono shadow-2xl">
            <RefreshCw size={13} className="animate-spin text-indigo-300" />
            Loading…
          </div>
        </div>
      )}
    </div>
  );
});

FilePanel.displayName = "FilePanel";

export default FilePanel;
