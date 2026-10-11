import React, { useState, useEffect, useLayoutEffect, useMemo, useRef, useImperativeHandle, forwardRef } from "react";
import { createPortal } from "react-dom";
import { listen } from "@tauri-apps/api/event";
import { invoke } from "@tauri-apps/api/core";
import {
  Folder, File, ArrowUp, RefreshCw, Trash2, Edit3, Shield,
  X, ChevronUp, ChevronDown, Plus, MoreVertical, FolderSearch, FolderPlus,
  Download, Upload, ExternalLink, Move, CheckSquare, MinusSquare, Square, Search, Link2, Copy, Bookmark,
} from "lucide-react";
import { FileEntry, FileProvider } from "../fs/types";
import { localDir, localJoin } from "../fs/localPath";
import { useConfirm, useOverwritePrompt, OverwriteChoice } from "../ui/confirm";
import { IS_ANDROID } from "../util/platform";
import { nameFilterMatcher } from "../util/nameFilter";
import { BOOKMARKS_MAX, readBookmarks, writeBookmarks } from "../util/sftpBookmarks";

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

type SortColumn = "name" | "size" | "modified" | "permissions" | "owner";
interface SortState { column: SortColumn; asc: boolean; }

// The header's square icon buttons; a step smaller in the narrowest panes.
const HEADER_BUTTON_BASE =
  "shrink-0 flex items-center justify-center rounded bg-white/[0.04] border border-white/10 hover:bg-white/10";
const headerButton = (tight: boolean) => `${HEADER_BUTTON_BASE} ${tight ? "h-5 w-5" : "h-[22px] w-[22px]"}`;

// How the header lays itself out, from the width its buttons have (`inner`,
// in px). It goes by the pane, not the window: side by side, a pane is half
// the tool panel. The steps are sized for the remote pane, whose name group
// is the wider one, and both panes use them, so their headers are the same
// height next to each other.
//   rows   the name group shares a row with the buttons, or has its own
//   elev   what the remote pane's root/sudo control spells out: the icon
//          alone, the word, or the word and what it was reached through
//   tight  smaller gaps, for the narrowest panes
interface HeaderFit { rows: 1 | 2; elev: "full" | "word" | "icon"; tight: boolean }
const HEADER_FITS: { from: number; fit: HeaderFit }[] = [
  { from: 284, fit: { rows: 1, elev: "full", tight: false } },
  { from: 236, fit: { rows: 1, elev: "word", tight: false } },
  { from: 200, fit: { rows: 1, elev: "icon", tight: false } },
  { from: 160, fit: { rows: 2, elev: "full", tight: false } },
  { from: 124, fit: { rows: 2, elev: "word", tight: false } },
  { from: 0, fit: { rows: 2, elev: "icon", tight: true } },
];
const headerFitIndex = (inner: number) => HEADER_FITS.findIndex((s) => inner >= s.from);

// The file list's columns. The name column is as wide as the folder's longest
// name needs, from NAME_MIN to NAME_MAX (a longer name is cut short and shows
// in full on hover). The others are as wide as the widest value they can show
// and sit right after it, so in a wide pane the room left over stays empty at
// the end of the row instead of opening a gap between a name and its size.
// When a pane can't fit a column beside a NAME_MIN name, the columns give way
// from the end (owner, rights, changed, then size), and what they showed moves
// to a line under the name.
type MetaColumn = "size" | "modified" | "permissions" | "owner";
type ColumnWidths = Record<MetaColumn, number>;
const REMOTE_COLUMNS: MetaColumn[] = ["size", "modified", "permissions", "owner"];
const LOCAL_COLUMNS: MetaColumn[] = ["size", "modified"]; // local entries carry no rights or owner here
const COLUMN_LABEL: Record<MetaColumn, string> = { size: "SIZE", modified: "CHANGED", permissions: "RIGHTS", owner: "OWNER" };
// The left-aligned columns keep a little room from the column before them
// (COLUMN_LEAD, part of their width), so a size and a date don't run together.
const COLUMN_CELL: Record<MetaColumn, string> = {
  size: "text-right text-[10.5px] text-zinc-300",
  modified: "pl-1.5 text-[9.5px] text-zinc-400",
  permissions: "pl-1.5 text-[10.5px] text-zinc-300 opacity-90",
  owner: "pl-1.5 text-[10.5px] text-zinc-300",
};
const COLUMN_LEAD: ColumnWidths = { size: 0, modified: 6, permissions: 6, owner: 6 };
// A column is as wide as the widest value it can show (at its text size) or
// its heading with the sort arrow, whichever is wider, in the list's own
// font: measured once (see measureColumns), these until then. An owner
// longer than the sample is cut short and shows in full on hover.
const COLUMN_SAMPLE: Record<MetaColumn, [string, number]> = {
  size: ["1023.9 KB", 10.5],
  modified: ["0000-00-00 00:00", 9.5],
  permissions: ["drwxr-xr-x", 10.5],
  owner: ["deployuser", 10.5],
};
const COLUMN_FALLBACK: ColumnWidths = { size: 56, modified: 98, permissions: 70, owner: 72 };
const SORT_ARROW = 15;  // the heading's arrow and its margin
const NAME_MIN = 128;
const NAME_MAX = 340;
const NAME_CHROME = 26; // the name column's icon, gap and padding
const CHECK_WIDTH = 14; // the selection boxes, in selection mode
const MENU_WIDTH = 16;  // a row's options button
const COLUMN_GAP = 6;   // gap-1.5
const LIST_INSET = 30;  // the list's padding, and a row's border and padding

// How many of `columns` fit beside a NAME_MIN name in a list `width` px wide.
// Selection mode's boxes come out of the name's room, not a column's, so
// starting a selection never moves a column under the names.
const columnsThatFit = (width: number, columns: MetaColumn[], widths: ColumnWidths) => {
  let need = LIST_INSET + NAME_MIN + COLUMN_GAP + MENU_WIDTH;
  let fit = 0;
  for (const column of columns) {
    need += COLUMN_GAP + widths[column];
    if (need > width) break;
    fit++;
  }
  return fit;
};

// One canvas measures text for every pane.
let measureContext: CanvasRenderingContext2D | null | undefined;
const textContext = () => {
  if (measureContext === undefined) measureContext = document.createElement("canvas").getContext("2d");
  return measureContext;
};

// The width of the widest of `names` in `font`, in px, up to `cap`.
const widestName = (names: string[], font: string, cap: number) => {
  const ctx = textContext();
  if (!ctx) return cap;
  ctx.font = font;
  let widest = 0;
  for (const name of names) {
    widest = Math.max(widest, ctx.measureText(name).width);
    if (widest >= cap) return cap;
  }
  return widest;
};

const measureColumns = (family: string): ColumnWidths => {
  const ctx = textContext();
  const widths = { ...COLUMN_FALLBACK };
  if (!ctx) return widths;
  for (const column of Object.keys(COLUMN_SAMPLE) as MetaColumn[]) {
    const [sample, size] = COLUMN_SAMPLE[column];
    ctx.font = `400 ${size}px ${family}`;
    const value = ctx.measureText(sample).width;
    ctx.font = `700 10.5px ${family}`;
    const heading = ctx.measureText(`${COLUMN_LABEL[column]} `).width + SORT_ARROW;
    widths[column] = Math.ceil(Math.max(value, heading)) + 2 + COLUMN_LEAD[column];
  }
  return widths;
};

// Typed letters within this many ms of each other make one name to jump to.
const TYPE_AHEAD_MS = 800;

// The letter of a Ctrl/⌘ shortcut: the key's own, or on a layout without
// Latin letters (Persian, Russian…) the one printed beside it.
const shortcutLetter = (e: React.KeyboardEvent) =>
  /^[a-z]$/i.test(e.key) ? e.key.toLowerCase()
  : /^Key[A-Z]$/.test(e.code) ? e.code.slice(3).toLowerCase()
  : "";

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
  /**
   * Extra controls rendered next to the pane label (remote: the root/sudo
   * badge). The pane root carries `group/pane` and `data-elev` (full | word |
   * icon), which says how much such a control should spell out at this width.
   */
  headerExtra?: React.ReactNode;
  /** A line of its own under the header's buttons (remote: why sudo failed). */
  headerNote?: React.ReactNode;
  /** File operations in this pane run as root — tint the header so it's unmistakable. */
  rootMode?: boolean;
  /** Where this pane keeps its bookmarked folders (util/sftpBookmarks). None: no bookmarks (a server that isn't saved). */
  bookmarksKey?: string;
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
  headerExtra,
  headerNote,
  rootMode = false,
  bookmarksKey,
}, ref) => {
  const [currentPath, setCurrentPath] = useState("");
  // Last five distinct directories visited in this panel, MRU first. Lives
  // in component state (cleared per session) — persistence didn't seem
  // worth the complexity given users usually want recents from this work
  // session, not whatever they were doing last week. Updated from `fetch`
  // when navigation succeeds.
  const [recentDirs, setRecentDirs] = useState<string[]>([]);
  const [recentOpen, setRecentOpen] = useState(false);
  // Bookmarked folders: the star at the end of the path box, and their own
  // part of the Recent list. Kept on this device (util/sftpBookmarks).
  const [bookmarks, setBookmarks] = useState<string[]>(() => (bookmarksKey ? readBookmarks(bookmarksKey) : []));
  useEffect(() => { setBookmarks(bookmarksKey ? readBookmarks(bookmarksKey) : []); }, [bookmarksKey]);
  const [entries, setEntries] = useState<FileEntry[]>([]);
  const [loading, setLoading] = useState(false);
  // Multi-selection lives as a Set of paths (see onRowClick for the clicks).
  // lastSelectedPathRef is the anchor a Shift-click or Shift-arrow ranges
  // from, and rangeBaseRef what was selected before a run of those began (see
  // selectRange). Both change through setAnchor.
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const lastSelectedPathRef = useRef<string | null>(null);
  const rangeBaseRef = useRef<Set<string> | null>(null);
  const setAnchor = (path: string | null) => {
    lastSelectedPathRef.current = path;
    rangeBaseRef.current = null;
  };
  // Selection mode: the rows show selection boxes, and a plain click adds or
  // removes a row instead of selecting just that one. Entered by Ctrl/⌘-click,
  // a Shift range, Ctrl+A, Space, "Select" in a row's menu (on a touch screen
  // a long press opens it) or "Select more" on the selection bar. Left once
  // nothing is selected (Esc, a click on empty space, the bar's Clear).
  const [selectMode, setSelectMode] = useState(false);
  useEffect(() => { if (selected.size === 0) setSelectMode(false); }, [selected]);
  const inMode = selectMode && selected.size > 0;
  // The row the keyboard is on, and whether the keyboard moved it last (its
  // outline only shows then: after a click the selection says where it is).
  const [cursor, setCursor] = useState<string | null>(null);
  const [keyNav, setKeyNav] = useState(false);
  const [sort, setSort] = useState<SortState>({ column: "name", asc: true });

  const [tempInput, setTempInput] = useState("");
  const [inputFocused, setInputFocused] = useState(false);
  const [activeSuggestion, setActiveSuggestion] = useState(-1);
  // Name filter applied AFTER sort — case-insensitive substring, or a `*` /
  // `?` wildcard pattern for the whole name (util/nameFilter). Kept separate
  // from the path bar so the user can leave a filter active while
  // typing into the path. Sticky across `cd` so a quick filter session can
  // span sibling dirs; the X button clears it.
  const [nameFilter, setNameFilter] = useState("");
  // The filter's row opens from the magnifier beside the path (or Ctrl+F in
  // the list) and is closed at first. Closing it clears the filter, so no
  // rows are ever left out by a filter that isn't on screen.
  const [filterOpen, setFilterOpen] = useState(false);
  const filterInputRef = useRef<HTMLInputElement | null>(null);

  const [contextMenu, setContextMenu] = useState<{ x: number; y: number; entry: FileEntry } | null>(null);
  // `orig`: the mode and owner the Properties dialog opened with, so Save
  // only sends what the user changed.
  const [modal, setModal] = useState<{ type: "rename" | "mkdir" | "properties" | "move" | "move-bulk"; entry?: FileEntry; v1?: string; v2?: string; orig?: { mode: number; uid?: number } } | null>(null);
  const [notification, setNotification] = useState<{ msg: string; type: "info" | "success" | "error" } | null>(null);

  const [dragOver, setDragOver] = useState(false);
  const paneRef = useRef<HTMLDivElement | null>(null);
  const dropTargetRef = useRef<HTMLDivElement | null>(null);
  const menuRef = useRef<HTMLDivElement | null>(null);
  const currentPathRef = useRef(currentPath);
  useEffect(() => { currentPathRef.current = currentPath; }, [currentPath]);

  // Android quick-picker state. There is no OS-level folder picker that
  // returns a real filesystem path on Android (SAF returns content URIs
  // that Rust's std::fs can't open), so we replace the desktop rfd call
  // with a small popover of writable paths returned by the backend. Loaded
  // lazily the first time the user opens the popover so we don't burn a
  // touch-probe per directory on every panel mount.
  const [androidPickerOpen, setAndroidPickerOpen] = useState(false);
  const [androidQuickDirs, setAndroidQuickDirs] = useState<{ label: string; path: string }[] | null>(null);

  // The header follows its own width (see HEADER_FITS). Only the step is
  // state, so dragging the panel's divider re-renders the pane when a step
  // is crossed, not on every pixel.
  const headerRowRef = useRef<HTMLDivElement | null>(null);
  const [fitIndex, setFitIndex] = useState(0);
  const fit = HEADER_FITS[fitIndex].fit;
  const headerBtn = headerButton(fit.tight);
  // Before the first paint, so a narrow pane never flashes the wide layout.
  useLayoutEffect(() => {
    const el = headerRowRef.current;
    if (!el || typeof ResizeObserver === "undefined") return;
    const measure = () => {
      // 0 while the pane is hidden (the other tab): keep the last step.
      const inner = el.clientWidth;
      if (inner > 0) setFitIndex(headerFitIndex(inner));
    };
    const observer = new ResizeObserver(measure);
    observer.observe(el);
    measure();
    return () => observer.disconnect();
  }, []);

  // The list's columns follow the list's width the same way (see
  // columnsThatFit): only how many fit is state. The rows' font is read
  // once, for measuring names and columns.
  const listRef = useRef<HTMLDivElement | null>(null);
  const [listFont, setListFont] = useState("");
  const [columnWidths, setColumnWidths] = useState(COLUMN_FALLBACK);
  const [columnsFit, setColumnsFit] = useState(3);
  const metaColumns = provider.id === "remote" ? REMOTE_COLUMNS : LOCAL_COLUMNS;
  useLayoutEffect(() => {
    const scroller = dropTargetRef.current;
    const rows = listRef.current;
    if (!scroller || !rows) return;
    const style = getComputedStyle(rows);
    setListFont(`${style.fontWeight} ${style.fontSize} ${style.fontFamily}`);
    const widths = measureColumns(style.fontFamily);
    setColumnWidths(widths);
    if (typeof ResizeObserver === "undefined") return;
    const measure = () => {
      // 0 while the pane is hidden (the other tab): keep the last fit.
      const width = scroller.clientWidth;
      if (width <= 0) return;
      setColumnsFit(columnsThatFit(width, metaColumns, widths));
    };
    const observer = new ResizeObserver(measure);
    observer.observe(scroller);
    measure();
    return () => observer.disconnect();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // ---- helpers ----------------------------------------------------------------

  const notify = (msg: string, type: "info" | "success" | "error" = "info") => {
    setNotification({ msg, type });
    setTimeout(() => setNotification(null), 4000);
  };

  const formatRights = (isDir: boolean, perm?: number, isSymlink?: boolean) => {
    const kind = isSymlink ? "l" : isDir ? "d" : "-";
    if (perm === undefined) return kind + "---------";
    const r = (v: number) => (v & 4 ? "r" : "-");
    const w = (v: number) => (v & 2 ? "w" : "-");
    const x = (v: number) => (v & 1 ? "x" : "-");
    const u = (perm >> 6) & 7, g = (perm >> 3) & 7, o = perm & 7;
    return kind + r(u) + w(u) + x(u) + r(g) + w(g) + x(g) + r(o) + w(o) + x(o);
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

  const toggleBookmark = (path: string) => {
    if (!bookmarksKey || !path) return;
    const marked = bookmarks.includes(path);
    if (!marked && bookmarks.length >= BOOKMARKS_MAX) {
      notify(`Up to ${BOOKMARKS_MAX} bookmarks: remove one to add another`, "info");
      return;
    }
    const next = marked ? bookmarks.filter((p) => p !== path) : [...bookmarks, path];
    setBookmarks(next);
    writeBookmarks(bookmarksKey, next);
  };

  // A row to bring into view once the next listing is on screen (see the
  // layout effect after the sorting).
  const revealRef = useRef<{ path: string; block: ScrollLogicalPosition } | null>(null);

  // Every listing asked for gets the next number, and only the newest one's
  // answer is used. On a slow link answers come back out of order: open a
  // big folder, then another before the first has listed, and the older
  // answer would land last and put the pane back in the folder just left.
  const listSeqRef = useRef(0);

  // `reveal`: a row to select in the new folder if it is there (going up
  // selects the folder just left).
  const fetch = async (path: string, reveal?: string) => {
    const seq = ++listSeqRef.current;
    setLoading(true);
    try {
      const result = await provider.list(path);
      if (seq !== listSeqRef.current) return;
      const present = new Set(result.entries.map((e) => e.path));
      setEntries(result.entries);
      setCurrentPath(result.currentPath);
      setTempInput(result.currentPath);
      if (result.currentPath === currentPathRef.current) {
        // A refresh keeps the selection and the keyboard's row, less what
        // is gone. It ends a run of Shift ranges, whose base may be gone.
        setSelected((prev) => {
          const kept = [...prev].filter((p) => present.has(p));
          return kept.length === prev.size ? prev : new Set(kept);
        });
        setCursor((c) => (c && present.has(c) ? c : null));
        const anchor = lastSelectedPathRef.current;
        setAnchor(anchor && present.has(anchor) ? anchor : null);
      } else if (reveal && present.has(reveal)) {
        selectOnly(reveal);
        setCursor(reveal);
        revealRef.current = { path: reveal, block: "center" };
      } else {
        setSelected(new Set());
        setAnchor(null);
        setCursor(null);
      }
      onPathChange?.(result.currentPath);
      pushRecent(result.currentPath);
    } catch (err: any) {
      if (seq === listSeqRef.current) notify(`List failed: ${err}`, "error");
    } finally {
      if (seq === listSeqRef.current) setLoading(false);
    }
  };

  // Initial load: try the caller-supplied `initialPath` first (the
  // per-server-saved directory), and fall back to the provider's home if it
  // no longer exists — that way a saved path being removed doesn't strand
  // the user with an error screen.
  useEffect(() => {
    const seq = ++listSeqRef.current;
    (async () => {
      const tryFetch = async (path: string) => {
        const result = await provider.list(path);
        if (seq !== listSeqRef.current) return; // the user went elsewhere meanwhile
        setEntries(result.entries);
        setCurrentPath(result.currentPath);
        setTempInput(result.currentPath);
        setSelected(new Set());
        setAnchor(null);
        setCursor(null);
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
        if (seq === listSeqRef.current) notify(`Failed to load: ${err}`, "error");
      } finally {
        if (seq === listSeqRef.current) setLoading(false);
      }
    })();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [provider]);

  useImperativeHandle(ref, () => ({
    refresh: () => fetch(currentPathRef.current),
    currentDir: () => currentPathRef.current,
  }), []);

  const goUp = () => fetch(provider.parentPath(currentPath), currentPath);

  // ---- selection -------------------------------------------------------------

  // The clicks every desktop file manager has:
  //   - plain click  → just this row (in selection mode: add or remove it)
  //   - Ctrl/⌘+click → add or remove this row
  //   - Shift+click  → the rows from the anchor to this one, added to the
  //                    selection with Ctrl/⌘ or in selection mode
  // Ranges use `ordered` (the rendered order), not `entries`, so they match
  // what is on screen.
  const selectOnly = (path: string) => {
    setSelected(new Set([path]));
    setAnchor(path);
  };
  const toggleRow = (path: string) => {
    const next = new Set(selected);
    if (next.has(path)) next.delete(path);
    else next.add(path);
    setSelected(next);
    setAnchor(path);
    setSelectMode(true);
  };
  // The rows from the anchor to `to`. A run of Shift-clicks or Shift-arrows
  // from one anchor grows and shrinks one range on top of what was selected
  // before the run began: nothing, unless the run adds (`add` at its start).
  // False when the anchor isn't on screen to range from.
  const selectRange = (to: string, ordered: FileEntry[], add: boolean) => {
    const a = ordered.findIndex(x => x.path === lastSelectedPathRef.current);
    const b = ordered.findIndex(x => x.path === to);
    if (a < 0 || b < 0) return false;
    const [lo, hi] = a <= b ? [a, b] : [b, a];
    if (!rangeBaseRef.current) rangeBaseRef.current = add ? new Set(selected) : new Set();
    const next = new Set(rangeBaseRef.current);
    for (const x of ordered.slice(lo, hi + 1)) next.add(x.path);
    setSelected(next);
    if (next.size > 1) setSelectMode(true);
    return true; // the anchor stays where the range started
  };
  const onRowClick = (e: React.MouseEvent, entry: FileEntry, ordered: FileEntry[]) => {
    setCursor(entry.path);
    setKeyNav(false);
    const add = e.ctrlKey || e.metaKey || inMode;
    if (e.shiftKey && selectRange(entry.path, ordered, add)) return;
    if (add) toggleRow(entry.path);
    else selectOnly(entry.path);
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

  // A typed folder the way the provider names one: no separator at the end,
  // except on a root.
  const typedFolder = (typed: string) =>
    provider.id === "local" ? localDir(typed) : typed.replace(/[\\/]+$/, "") || "/";

  // Prefetch listings for typed paths outside the current directory. Debounced
  // so a fast typist doesn't fire one SFTP request per keystroke; skipped
  // when the typed parent already matches currentPath (free from `entries`).
  useEffect(() => {
    if (!inputFocused) return;
    const split = splitInputPath(tempInput);
    if (!split) return;
    const probe = typedFolder(split.parent);
    if (probe === currentPath || probe === lookaheadParent) return;
    const t = setTimeout(async () => {
      try {
        const result = await provider.list(probe);
        setLookaheadEntries(result.entries);
        // Remembered as typed, not as the provider resolved it (another
        // letter case, a link followed): it is compared with what is typed.
        setLookaheadParent(probe);
      } catch { /* parent doesn't exist (yet) — suggestions just stay empty */ }
    }, 220);
    return () => clearTimeout(t);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [tempInput, inputFocused, currentPath]);

  const suggestions = (() => {
    if (!inputFocused) return [];
    const split = splitInputPath(tempInput);
    if (!split) return [];
    const probe = typedFolder(split.parent);
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
    // Right-click on a row that isn't already part of the selection should
    // switch focus to it (Explorer/Finder behaviour), out of selection mode
    // too, so the menu acts on that row alone. Right-click on a row that IS
    // selected keeps the multi-selection so bulk actions apply.
    if (!selected.has(entry.path)) {
      selectOnly(entry.path);
      setSelectMode(false);
    }
    setCursor(entry.path);
    setKeyNav(false);
    setContextMenu({ x: e.clientX, y: e.clientY, entry });
  };
  // The menu opens at the pointer and moves up or left as far as it has to
  // to stay in the window, by its own size.
  useLayoutEffect(() => {
    const el = menuRef.current;
    if (!contextMenu || !el) return;
    const { width, height } = el.getBoundingClientRect();
    const x = Math.max(4, Math.min(contextMenu.x, window.innerWidth - width - 4));
    const y = Math.max(4, Math.min(contextMenu.y, window.innerHeight - height - 4));
    if (x !== contextMenu.x || y !== contextMenu.y) setContextMenu({ ...contextMenu, x, y });
  }, [contextMenu]);

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
  // dispatch only when the cursor landed inside our root: anywhere in the
  // pane, the selection bar over the list's end included.
  //
  // Only the remote pane takes OS drops (they're uploads). The local pane is
  // given a sessionId too, for its Upload button, so checking that alone made
  // a drop on the local pane upload the files to `<local folder>/<name>` on
  // the server.

  useEffect(() => {
    if (!sessionId || provider.id !== "remote") return;
    let unlisten: (() => void) | null = null;
    listen<{ paths: string[]; position: { x: number; y: number } }>(
      "tauri://drag-drop",
      async (event) => {
        const { paths, position } = event.payload || ({} as any);
        if (!paths?.length || !paneRef.current) return;
        const hit = document.elementFromPoint(position.x, position.y);
        if (!hit || !paneRef.current.contains(hit)) return;
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
  }, [sessionId, provider.id]);

  // ---- modals -----------------------------------------------------------------

  // Properties shows the path's current mode and owner, read fresh and
  // following symlinks: chmod/chown on a link change its target, and the
  // listing only has the link's own mode (777). The setuid/setgid/sticky
  // bits are kept so Save can't drop them.
  const openProperties = async (entry: FileEntry) => {
    setContextMenu(null);
    let permissions = entry.permissions;
    let uid = entry.uid;
    if (provider.stat) {
      try {
        ({ permissions, uid } = await provider.stat(entry.path));
      } catch (err: any) {
        notify(`Properties: ${err}`, "error");
        return;
      }
    }
    const mode = permissions !== undefined ? permissions & 0o7777 : 0o755;
    setModal({ type: "properties", entry, v1: mode.toString(8).padStart(3, "0"), v2: uid?.toString(), orig: { mode, uid } });
  };

  const submitModal = async () => {
    if (!modal) return;
    const { type, entry, v1, v2 } = modal;
    try {
      if (type === "rename" && entry && v1) {
        const destDir = provider.parentPath(entry.path);
        const dest = provider.joinPath(destDir, v1);
        await provider.rename(entry.path, dest);
        // The row keeps its place in the selection and the keyboard's place
        // under its new name (the refresh below keeps both).
        setSelected((prev) => {
          if (!prev.has(entry.path)) return prev;
          const next = new Set(prev);
          next.delete(entry.path);
          return next.add(dest);
        });
        if (lastSelectedPathRef.current === entry.path) setAnchor(dest);
        setCursor(dest);
        revealRef.current = { path: dest, block: "nearest" };
        notify(`Renamed to ${v1}`, "success");
      } else if (type === "move" && entry && v1) {
        // v1 is the destination *directory* the user typed (matches the
        // bulk-move semantics so users have a single mental model). We
        // append the entry's own name so the item keeps its filename. To
        // change the filename, the user picks "Rename" instead.
        const destDir = typedFolder(v1);
        const dest = provider.joinPath(destDir, entry.name);
        if (dest === entry.path) throw new Error("Destination is the current location — nothing to move.");
        await provider.rename(entry.path, dest);
        notify(`Moved ${entry.name} → ${destDir}`, "success");
      } else if (type === "move-bulk" && v1) {
        // v1 is the destination *directory*; each selected item keeps its
        // own name under it.
        const dest = typedFolder(v1);
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
          setAnchor(null);
          notify(items.length === 1 ? `Moved ${items[0].name} → ${dest}` : `Moved ${count} of ${items.length} items → ${dest}`, "success");
        }
      } else if (type === "mkdir" && v1) {
        const made = provider.joinPath(currentPath, v1);
        await provider.mkdir(made);
        selectOnly(made);
        setCursor(made);
        revealRef.current = { path: made, block: "nearest" };
        notify(`Created ${v1}`, "success");
      } else if (type === "properties" && entry && v1 && provider.chmod) {
        const mode = parseInt(v1, 8);
        if (isNaN(mode) || mode < 0 || mode > 0o7777) throw new Error("Invalid octal mode");
        const orig = modal.orig;
        // Only what changed. Owner first: Linux clears setuid/setgid on a
        // chown, so the mode goes on after it (again, if it has those bits).
        let chowned = false;
        if (v2 && provider.chown) {
          const uid = parseInt(v2);
          if (!isNaN(uid) && uid !== orig?.uid) {
            await provider.chown(entry.path, uid, null); // null: keep the group
            chowned = true;
          }
        }
        const chmodded = mode !== orig?.mode || (chowned && (mode & 0o6000) !== 0);
        if (chmodded) await provider.chmod(entry.path, mode);
        notify(chowned || chmodded ? "Properties updated" : "Nothing changed", chowned || chmodded ? "success" : "info");
      }
      await fetch(currentPath);
    } catch (err: any) {
      notify(`Failed: ${err}`, "error");
    } finally {
      setModal(null);
    }
  };

  // Once a dialog closes the list has the keyboard again (F2, a new name,
  // Enter, and the arrows go on from the renamed row).
  const hadModalRef = useRef(false);
  useEffect(() => {
    const active = document.activeElement;
    if (hadModalRef.current && !modal && (!active || active === document.body)) {
      dropTargetRef.current?.focus({ preventScroll: true });
    }
    hadModalRef.current = !!modal;
  }, [modal]);

  // ---- removal ----------------------------------------------------------------

  const confirmDialog = useConfirm();
  const overwritePrompt = useOverwritePrompt();

  // Bulk-aware delete. Pops a single confirm dialog regardless of count, then
  // applies provider.remove to each item in turn — best-effort: an item that
  // fails doesn't halt the rest, and one toast says how the batch went.
  // Selection is cleared on success so the user isn't left with stale paths
  // highlighted.
  const removeItems = async (items: FileEntry[]) => {
    if (items.length === 0) return;
    const ok = await confirmDialog({
      title: items.length === 1 ? "Delete item" : `Delete ${items.length} items`,
      message: items.length === 1
        ? (items[0].isSymlink
            ? `Delete the link “${items[0].name}”? Only the link is removed; what it points to is not touched.`
            : items[0].isDir
            ? `Permanently delete folder “${items[0].name}” and everything inside?`
            : `Permanently delete “${items[0].name}”?`)
        : `Permanently delete ${items.length} items? Folders include their contents; links are removed without touching what they point to.`,
      okLabel: "Delete",
      destructive: true,
    });
    if (!ok) return;
    let count = 0;
    // The first item that could not be deleted, and why. One toast has to
    // say how the whole batch went, so only the first is kept.
    let failure: { name: string; reason: string } | null = null;
    let failed = 0;
    let stopped = false;
    for (const it of items) {
      try { await provider.remove(it.path, it.isDir, it.isSymlink); count++; }
      catch (err: any) {
        // A remote folder's delete was stopped from its card: the rest of
        // the batch stops with it.
        if (String(err) === "cancelled") { stopped = true; break; }
        failed++;
        failure ??= { name: it.name, reason: String(err) };
      }
    }
    if (count > 0) {
      setSelected(new Set());
      setAnchor(null);
    }
    if (stopped) {
      notify(count > 0 ? `Delete stopped after ${count} of ${items.length} items` : "Delete stopped. What was already removed is gone.", "info");
    } else if (failure) {
      notify(
        items.length === 1
          ? `Delete failed for ${failure.name}: ${failure.reason}`
          : `Deleted ${count} of ${items.length} items. ${failed === 1 ? "" : `${failed} failed; the first: `}${failure.name}: ${failure.reason}`,
        "error",
      );
    } else {
      notify(items.length === 1 ? `Deleted ${items[0].name}` : `Deleted ${count} of ${items.length} items`, "success");
    }
    // A folder that could not be removed completely, or whose delete was
    // stopped, may have lost part of its contents: show what is there now.
    await fetch(currentPath);
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
    const destDir = localDir(dest);
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
        // A folder is given the folder it goes into (the backend adds its
        // name), a file the full path it is saved as.
        const dest = e.isDir ? destDir : localJoin(destDir, e.name);
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

  // Copy name / Copy path in a row's menu.
  const copyText = async (text: string, what: string) => {
    try {
      await navigator.clipboard.writeText(text);
      notify(`Copied ${what}`, "success");
    } catch (err: any) {
      notify(`Copy failed: ${err}`, "error");
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

  // A double-click or Enter on a row.
  const openEntry = (entry: FileEntry) => {
    if (entry.isDir) { fetch(entry.path); return; }
    if (entry.brokenLink) { notify(`“${entry.name}” is a broken link: its target is missing or not accessible.`, "error"); return; }
    // For files: remote → live-edit (download + open editor + auto-upload on
    // save); local → open in the OS default app. Both are desktop-only — on
    // Android opening a file is a no-op (there's no OS default editor to hand
    // off to).
    if (IS_ANDROID) return;
    if (provider.id === "remote") liveEditEntry(entry);
    else openLocalEntry(entry);
  };

  // ---- owners -----------------------------------------------------------------
  // The remote OWNER column: an SFTP listing has only numeric uids, so the
  // names are looked up on the server (sftp_resolve_owners) for uids not
  // asked about yet, and kept for this pane. A server that can't do it (no
  // POSIX sh), or two failed lookups, stop the asking: SFTP-only accounts
  // aren't sent an exec on every folder change. Without a name the uid shows.
  const [owners, setOwners] = useState<Map<number, string>>(() => new Map());
  const ownersAskedRef = useRef(new Set<number>());
  const ownerFailuresRef = useRef(0);
  useEffect(() => {
    if (provider.id !== "remote" || !sessionId || ownerFailuresRef.current >= 2) return;
    const ask = [...new Set(entries.map((e) => e.uid).filter((u): u is number => typeof u === "number"))]
      .filter((u) => !ownersAskedRef.current.has(u));
    if (ask.length === 0) return;
    ask.forEach((u) => ownersAskedRef.current.add(u));
    invoke<{ owners: { uid: number; name: string }[]; status: string }>("sftp_resolve_owners", { sessionId, uids: ask })
      .then((found) => {
        if (found.status !== "complete") ownerFailuresRef.current = 2;
        if (found.owners.length === 0) return;
        setOwners((prev) => {
          const next = new Map(prev);
          for (const o of found.owners) next.set(o.uid, o.name);
          return next;
        });
      })
      .catch(() => {
        ownerFailuresRef.current += 1;
        ask.forEach((u) => ownersAskedRef.current.delete(u));
      });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [entries]);
  const ownerName = (entry: FileEntry) =>
    entry.uid === undefined ? "" : owners.get(entry.uid) ?? String(entry.uid);

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
        case "owner": va = ownerName(a).toLowerCase(); vb = ownerName(b).toLowerCase(); break;
      }
      if (va < vb) return sort.asc ? -1 : 1;
      if (va > vb) return sort.asc ? 1 : -1;
      return 0;
    });
    const matches = nameFilterMatcher(nameFilter);
    return matches ? sorted.filter(e => matches(e.name)) : sorted;
  })();
  const filteredOut = nameFilter.trim() ? entries.length - sortedEntries.length : 0;

  // As wide as the folder's longest name needs (see NAME_MIN). Measured on
  // the whole listing, so the columns hold still while a filter is typed.
  const nameWidth = useMemo(() => {
    if (!listFont) return NAME_MIN;
    const text = widestName(entries.map((e) => e.name), listFont, NAME_MAX - NAME_CHROME);
    return Math.min(NAME_MAX, Math.max(NAME_MIN, Math.ceil(text) + NAME_CHROME));
  }, [entries, listFont]);

  // A new folder starts at the top; then the row `fetch` (or a dialog) asked
  // for comes into view.
  useLayoutEffect(() => {
    if (dropTargetRef.current) dropTargetRef.current.scrollTop = 0;
  }, [currentPath]);
  useLayoutEffect(() => {
    const reveal = revealRef.current;
    if (!reveal) return;
    revealRef.current = null;
    const index = sortedEntries.findIndex((e) => e.path === reveal.path);
    (listRef.current?.children[index] as HTMLElement | undefined)?.scrollIntoView({ block: reveal.block });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [entries]);

  // ---- select-all -------------------------------------------------------------
  // Over the rows on screen: a filter leaves the others out, and typing one
  // drops the rows it hides from the selection, so what is selected is always
  // what the selection bar and the menu act on.
  const selectAllVisible = () => {
    if (sortedEntries.length === 0) return;
    setSelected(new Set(sortedEntries.map(e => e.path)));
    setAnchor(sortedEntries[sortedEntries.length - 1].path);
    setSelectMode(true);
  };
  const clearSelection = () => {
    setSelected(new Set());
    setAnchor(null);
  };
  const selectedEntries = sortedEntries.filter(e => selected.has(e.path));
  const allSelected = sortedEntries.length > 0 && selectedEntries.length === sortedEntries.length;
  useEffect(() => {
    if (selected.size === 0) return;
    const shown = new Set(sortedEntries.map((e) => e.path));
    const kept = [...selected].filter((p) => shown.has(p));
    if (kept.length === selected.size) return;
    setSelected(new Set(kept));
    const anchor = lastSelectedPathRef.current;
    setAnchor(anchor && shown.has(anchor) ? anchor : null);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [nameFilter]);

  // ---- keyboard ---------------------------------------------------------------
  // The list takes the focus when it is clicked or tabbed to. Then:
  //   ↑ ↓ Home End PgUp PgDn  move; outside selection mode the selection
  //                           moves along. Shift extends it, Ctrl/⌘ moves alone
  //   Space                   start selection mode / add or remove the row
  //   Enter                   open             Backspace, Alt+↑  up a folder
  //   Delete (⌘⌫)             delete           F2                rename
  //   Ctrl/⌘+A                select all       Esc               clear it
  //   Ctrl/⌘+F                the name filter
  //   letters                 the next name that starts with what was typed
  const typeAheadRef = useRef({ text: "", at: 0 });

  const openFilter = () => {
    setFilterOpen(true);
    // Already open: back into it, with what is typed selected.
    filterInputRef.current?.focus();
    filterInputRef.current?.select();
  };
  const closeFilter = () => {
    setFilterOpen(false);
    setNameFilter("");
  };

  const scrollToRow = (index: number) =>
    (listRef.current?.children[index] as HTMLElement | undefined)?.scrollIntoView({ block: "nearest" });

  const moveCursor = (index: number, mods: { shiftKey: boolean; ctrlKey: boolean; metaKey: boolean }) => {
    if (sortedEntries.length === 0) return;
    const at = Math.max(0, Math.min(sortedEntries.length - 1, index));
    const to = sortedEntries[at].path;
    setCursor(to);
    setKeyNav(true);
    if (mods.shiftKey) {
      if (!lastSelectedPathRef.current) setAnchor(cursor ?? to);
      if (!selectRange(to, sortedEntries, inMode)) selectOnly(to);
    } else if (!(mods.ctrlKey || mods.metaKey) && !inMode) {
      selectOnly(to);
    }
    scrollToRow(at);
  };

  // The same letter again goes on to the next name that starts with it.
  const typeAhead = (key: string, from: number) => {
    const typed = typeAheadRef.current;
    const now = Date.now();
    typed.text = now - typed.at > TYPE_AHEAD_MS ? key : typed.text + key;
    typed.at = now;
    const text = typed.text.toLowerCase();
    const repeat = [...text].every((c) => c === text[0]);
    const prefix = repeat ? text[0] : text;
    const start = repeat ? from + 1 : Math.max(from, 0);
    for (let i = 0; i < sortedEntries.length; i++) {
      const at = (start + i) % sortedEntries.length;
      if (sortedEntries[at].name.toLowerCase().startsWith(prefix)) {
        moveCursor(at, { shiftKey: false, ctrlKey: false, metaKey: false });
        return;
      }
    }
  };

  const onListKeyDown = (e: React.KeyboardEvent<HTMLDivElement>) => {
    if (disabled) return;
    // A row's options button keeps Enter and Space for itself.
    if (e.target !== e.currentTarget && (e.key === "Enter" || e.key === " ")) return;
    // A key closes the row menu; Esc does only that.
    if (contextMenu) {
      setContextMenu(null);
      if (e.key === "Escape") { e.preventDefault(); return; }
    }
    const at = sortedEntries.findIndex((x) => x.path === (cursor ?? lastSelectedPathRef.current));
    const current = at >= 0 ? sortedEntries[at] : undefined;
    const ctrl = e.ctrlKey || e.metaKey;
    const page = () => {
      const row = listRef.current?.firstElementChild as HTMLElement | null;
      return Math.max(1, Math.floor((dropTargetRef.current?.clientHeight ?? 0) / (row?.offsetHeight || 24)) - 2);
    };
    const typing = Date.now() - typeAheadRef.current.at <= TYPE_AHEAD_MS;
    switch (e.key) {
      case "ArrowDown": moveCursor(at < 0 ? 0 : at + 1, e); break;
      case "ArrowUp":
        if (e.altKey) goUp();
        else moveCursor(at < 0 ? 0 : at - 1, e);
        break;
      case "Home": moveCursor(0, e); break;
      case "End": moveCursor(sortedEntries.length - 1, e); break;
      case "PageDown": moveCursor(at < 0 ? 0 : at + page(), e); break;
      case "PageUp": moveCursor(at < 0 ? 0 : at - page(), e); break;
      case "Enter": if (current) openEntry(current); break;
      case "Backspace":
        if (e.metaKey) removeItems(selectedEntries);
        else goUp();
        break;
      case "Delete": removeItems(selectedEntries); break;
      case "F2": if (current) setModal({ type: "rename", entry: current, v1: current.name }); break;
      case "Escape":
        if (selected.size === 0) return;
        clearSelection();
        break;
      case " ":
        if (typing && !ctrl) { typeAhead(" ", at); break; }
        if (!current) break;
        setCursor(current.path);
        setKeyNav(true);
        if (ctrl || inMode) {
          toggleRow(current.path);
        } else {
          setSelected(new Set([...selected, current.path]));
          setAnchor(current.path);
          setSelectMode(true);
        }
        break;
      default:
        if (ctrl && !e.altKey && shortcutLetter(e) === "a") selectAllVisible();
        else if (ctrl && !e.altKey && shortcutLetter(e) === "f") openFilter();
        else if (e.key.length === 1 && !ctrl && !e.altKey) typeAhead(e.key, at);
        else return;
    }
    e.preventDefault();
    e.stopPropagation();
  };

  const toggleSort = (col: SortColumn) =>
    setSort((p) => ({ column: col, asc: p.column === col ? !p.asc : true }));

  const sortIcon = (col: SortColumn) => {
    if (sort.column !== col) return null;
    return sort.asc
      ? <ChevronUp size={11} className="inline ml-1 text-indigo-400" />
      : <ChevronDown size={11} className="inline ml-1 text-indigo-400" />;
  };

  // ---- HTML5 dragover for visual feedback during OS-level drop ----------------
  // Only the remote pane takes the drop (see the tauri://drag-drop listener);
  // over the local pane the cursor says so instead of a highlight.

  const onDragOver = (e: React.DragEvent) => {
    e.preventDefault();
    if (provider.id === "remote") setDragOver(true);
    else e.dataTransfer.dropEffect = "none";
  };
  const onDragLeave = (e: React.DragEvent) => { e.preventDefault(); setDragOver(false); };
  const onDrop = (e: React.DragEvent) => { e.preventDefault(); setDragOver(false); };

  const isRemote = provider.id === "remote";

  // The Recent list, and a row of it: open the folder, or (the bookmark at
  // its end) add or remove its bookmark. Mouse-down keeps the focus on the
  // list's button, whose blur closes the list.
  const recentShown = recentDirs.filter((p) => p !== currentPath);
  const currentBookmarked = bookmarks.includes(currentPath);
  const dirRow = (path: string, part: "recent" | "bookmark") => {
    const marked = bookmarks.includes(path);
    return (
      <div key={`${part}:${path}`} className="flex items-center rounded hover:bg-white/10">
        <button
          onMouseDown={(e) => { e.preventDefault(); setRecentOpen(false); fetch(path); }}
          className="flex-1 min-w-0 flex items-center gap-2 p-1.5 text-left hover:text-white"
          title={path}
        >
          <Folder size={11} className="text-indigo-300 shrink-0" />
          <span className="truncate">{path}</span>
        </button>
        {bookmarksKey && (
          <button
            onMouseDown={(e) => e.preventDefault()}
            onClick={() => toggleBookmark(path)}
            title={marked ? "Remove the bookmark" : "Bookmark this folder"}
            aria-label={marked ? `Remove the bookmark for ${path}` : `Bookmark ${path}`}
            aria-pressed={marked}
            className={`p-1.5 shrink-0 ${marked ? "text-amber-300 hover:text-amber-200" : "text-zinc-500 hover:text-zinc-200"}`}
          >
            <Bookmark size={11} fill={marked ? "currentColor" : "none"} />
          </button>
        )}
      </div>
    );
  };

  // The list's columns at this width (see columnsThatFit), shared by the
  // header and the rows; the ones that gave way show under each name.
  const shownColumns = metaColumns.slice(0, columnsFit);
  const movedColumns = metaColumns.slice(shownColumns.length);
  const gridTemplateColumns = [
    ...(inMode ? [`${CHECK_WIDTH}px`] : []),
    `minmax(0, ${nameWidth}px)`,
    ...shownColumns.map((c) => `${columnWidths[c]}px`),
    `${MENU_WIDTH}px`,
  ].join(" ");
  const columnText = (column: MetaColumn, entry: FileEntry) =>
    column === "size" ? (entry.isDir ? "" : formatSize(entry.size))
    : column === "modified" ? formatTime(entry.modified)
    : column === "owner" ? ownerName(entry)
    : formatRights(entry.isDir, entry.permissions, entry.isSymlink);

  return (
    <div
      ref={paneRef}
      data-fs-pane={provider.id}
      data-fs-current-path={currentPath}
      data-elev={fit.elev}
      className="group/pane flex-1 flex flex-col h-full bg-[#09090b] p-1.5 gap-1.5 overflow-hidden relative select-none"
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
        <div className={`absolute bottom-3 right-3 z-50 max-w-[calc(100%-1.5rem)] px-3 py-1.5 rounded-lg border text-[11px] font-mono [overflow-wrap:anywhere] shadow-2xl backdrop-blur-md animate-in fade-in slide-in-from-bottom-4 duration-300 ${
          notification.type === "success" ? "bg-emerald-950/90 border-emerald-500/30 text-emerald-400" :
          notification.type === "error"   ? "bg-rose-950/90 border-rose-500/30 text-rose-400" :
                                            "bg-indigo-950/90 border-indigo-500/30 text-indigo-400"
        }`}>{notification.msg}</div>
      )}

      {/* Header: the pane's name and buttons on top, the path on a row of
          its own so a long path stays readable even in a narrow pane. */}
      <div className={`w-full flex flex-col gap-1.5 p-1.5 border rounded-lg shrink-0 shadow-lg ${
        rootMode ? "bg-rose-950/30 border-rose-500/40" : "bg-[#121214] border-white/5"
      }`}>
        {/* Three groups: who (the name, plus the pane's own control: Browse
            here, root/sudo on the remote side), where to go, what to make.
            One row when the pane has the width; otherwise the name group
            takes a row to itself. Both panes switch at the same widths
            (HEADER_FITS), so side by side their lists start level. */}
        <div ref={headerRowRef} className={`relative flex flex-wrap items-center gap-y-1.5 ${fit.tight ? "gap-x-1" : "gap-x-2"}`}>
          <div className={`flex items-center gap-1.5 min-w-0 min-h-[22px] ${fit.rows === 2 ? "basis-full justify-between" : ""}`}>
            <span className="text-[10px] font-black uppercase tracking-widest text-zinc-300 pl-1 shrink-0">
              {provider.label}
            </span>
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
                aria-label="Browse for folder"
                className={`${headerBtn} text-emerald-300`}
              >
                <FolderSearch size={11} />
              </button>
            )}
            {headerExtra}
          </div>
          <div className={`flex items-center shrink-0 ${fit.tight ? "gap-0.5" : "gap-1"}`}>
            <button onClick={goUp} title="Up" aria-label="Up" className={`${headerBtn} text-zinc-200`}>
              <ArrowUp size={11} />
            </button>
            <button
              onClick={() => setRecentOpen((p) => !p)}
              onBlur={() => setTimeout(() => setRecentOpen(false), 200)}
              disabled={recentShown.length === 0 && bookmarks.length === 0}
              title="Recent and bookmarked folders"
              aria-label="Recent directories"
              className={`${headerBtn} text-zinc-200 disabled:opacity-30 disabled:cursor-not-allowed`}
            >
              <ChevronDown size={11} />
            </button>
            <button onClick={() => fetch(currentPath)} title="Refresh" aria-label="Refresh"
              className={`${headerBtn} text-zinc-200`}>
              <RefreshCw size={11} className={loading ? "animate-spin" : ""} />
            </button>
          </div>
          <button onClick={() => setModal({ type: "mkdir", v1: "" })} title="New Folder" aria-label="New Folder"
            className={`${headerBtn} text-indigo-300 ml-auto`}>
            <FolderPlus size={11} />
          </button>
          {/* Both lists open under the buttons across the header's width, so
              neither is cut off by a narrow pane. */}
          {recentOpen && (recentShown.length > 0 || bookmarks.length > 0) && (
            <div className="absolute top-full mt-1 inset-x-0 z-50 max-h-[320px] overflow-y-auto bg-[#0c0c0e]/95 border border-white/10 rounded-lg shadow-2xl p-1 backdrop-blur-md font-mono text-[11px] text-zinc-200 no-scrollbar">
              {recentShown.length > 0 && (
                <>
                  <div className="px-2 py-1 text-[9px] uppercase tracking-wider text-zinc-500 font-bold">Recent</div>
                  {recentShown.map((p) => dirRow(p, "recent"))}
                </>
              )}
              {recentShown.length > 0 && bookmarks.length > 0 && <div className="my-1 h-px bg-white/10" />}
              {bookmarks.length > 0 && (
                <>
                  <div className="px-2 py-1 text-[9px] uppercase tracking-wider text-zinc-500 font-bold">Bookmarks</div>
                  {bookmarks.map((p) => dirRow(p, "bookmark"))}
                </>
              )}
            </div>
          )}
          {IS_ANDROID && androidPickerOpen && androidQuickDirs && (
            <div className="absolute top-full mt-1 inset-x-0 z-50 bg-[#0c0c0e]/95 border border-white/10 rounded-lg shadow-2xl p-1 backdrop-blur-md font-mono text-[11px]">
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
        </div>
        {headerNote}
        <div className="relative flex items-center gap-1">
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
            className="flex-1 min-w-0 h-6 px-2 bg-white/[0.04] border border-white/10 rounded text-[11px] text-zinc-100 font-mono focus:outline-none focus:border-indigo-400/50 focus:bg-white/10"
          />
          {bookmarksKey && (
            <button
              onClick={() => toggleBookmark(currentPath)}
              disabled={!currentPath}
              title={currentBookmarked ? "Remove this folder's bookmark" : "Bookmark this folder"}
              aria-label="Bookmark this folder"
              aria-pressed={currentBookmarked}
              className={`${HEADER_BUTTON_BASE} h-6 w-6 disabled:opacity-30 ${currentBookmarked ? "text-amber-300" : "text-zinc-200"}`}
            >
              <Bookmark size={11} fill={currentBookmarked ? "currentColor" : "none"} />
            </button>
          )}
          <button
            onClick={() => (filterOpen ? closeFilter() : openFilter())}
            title={filterOpen ? "Close the name filter" : "Filter by name (Ctrl+F)"}
            aria-label="Filter by name"
            aria-pressed={filterOpen}
            className={`${HEADER_BUTTON_BASE} h-6 w-6 ${filterOpen ? "text-indigo-200 bg-indigo-500/20 border-indigo-400/40" : "text-zinc-200"}`}
          >
            <Search size={11} />
          </button>
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
      </div>

      {/* Filter row, opened from the magnifier beside the path. Lets the
          user trim the rendered list by name without touching sort or the
          path bar. Sticky across cd so they can hunt the same name across
          sibling dirs; the X clears it, and Esc closes the row. Enter or ↓
          goes on to the rows that are left. */}
      {filterOpen && (
        <div className="shrink-0 flex items-center gap-1 px-2 py-1 bg-[#0e0e10] border border-white/5 rounded text-[10.5px] font-mono">
          <Search size={11} className="text-zinc-500 shrink-0" />
          <input
            ref={filterInputRef}
            type="text"
            autoFocus
            value={nameFilter}
            onChange={(e) => setNameFilter(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Escape") {
                e.preventDefault();
                closeFilter();
                dropTargetRef.current?.focus({ preventScroll: true });
              } else if ((e.key === "Enter" || e.key === "ArrowDown") && sortedEntries.length > 0) {
                e.preventDefault();
                dropTargetRef.current?.focus({ preventScroll: true });
                moveCursor(0, { shiftKey: false, ctrlKey: false, metaKey: false });
              }
            }}
            placeholder="Filter by name…"
            title="Filter by name. Wildcards match the whole name: * any characters, ? one character (e.g. *.sh)"
            className="flex-1 min-w-0 h-5 bg-transparent text-zinc-100 placeholder:text-zinc-600 focus:outline-none"
          />
          {nameFilter && (
            <>
              <span className="text-[9.5px] text-zinc-500 shrink-0">
                {sortedEntries.length}/{entries.length}
              </span>
              <button
                onClick={() => { setNameFilter(""); filterInputRef.current?.focus(); }}
                title="Clear filter"
                className="shrink-0 p-0.5 rounded text-zinc-400 hover:text-white hover:bg-white/10"
              >
                <X size={10} />
              </button>
            </>
          )}
        </div>
      )}

      {/* Bulk action bar — shows for any non-empty selection so the user can
          send / move / delete with one click without right-clicking. It is a
          FLOATING overlay (absolute, pinned bottom-center) rather than a flex
          sibling, so its appearance never pushes the file list down — the rows
          stay exactly where they are when a selection starts (the list gets
          room at its end instead, so its last rows can scroll clear of it).
          Centred by its margins, so it may grow to the pane's width before it
          wraps (a left: 50% would leave it half of that). */}
      {selectedEntries.length > 0 && (
        <div className="absolute bottom-3 inset-x-2 mx-auto w-fit z-20 flex flex-wrap items-center justify-center gap-1.5 px-2.5 py-1.5 bg-indigo-950/95 border border-indigo-500/40 rounded-lg shadow-2xl shadow-black/50 backdrop-blur-md text-[10.5px] text-indigo-100 font-mono">
          <span className="font-bold uppercase tracking-wider text-[9.5px] text-indigo-300 shrink-0">
            {selectedEntries.length} selected
          </span>
          {!inMode && (
            <button
              onClick={() => setSelectMode(true)}
              title="Add rows with a click (Ctrl+click, Shift+click and Space do it too)"
              className="px-2 py-0.5 rounded bg-white/5 text-indigo-200 hover:bg-white/10 flex items-center gap-1"
            >
              <CheckSquare size={10} /> Select more
            </button>
          )}
          <div className="flex-1" />
          {isRemote && (
            <button
              onClick={() => downloadItems(selectedEntries)}
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
              onClick={() => uploadItems(selectedEntries)}
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
            onClick={() => removeItems(selectedEntries)}
            className="px-2 py-0.5 rounded bg-rose-500/15 text-rose-300 hover:bg-rose-500/25 flex items-center gap-1"
          >
            <Trash2 size={10} /> Delete
          </button>
          <button
            onClick={clearSelection}
            title="Clear the selection (Esc)"
            className="px-2 py-0.5 rounded bg-white/5 text-zinc-400 hover:bg-white/10 flex items-center gap-1"
          >
            <X size={10} /> Clear
          </button>
        </div>
      )}

      {/* List. It takes the keyboard focus when clicked or tabbed to (see
          onListKeyDown), so keys go to the pane they are meant for. */}
      <div
        ref={dropTargetRef}
        tabIndex={0}
        aria-label={`${provider.label} files`}
        onKeyDown={onListKeyDown}
        onBlur={(e) => { if (!e.currentTarget.contains(e.relatedTarget as Node | null)) setKeyNav(false); }}
        onDragOver={onDragOver}
        onDragLeave={onDragLeave}
        onDrop={onDrop}
        className={`flex-1 border rounded-lg bg-[#121214] flex flex-col overflow-y-auto overflow-x-hidden transition-all duration-200 border-indigo-500/30 shadow-2xl shadow-indigo-950/10 outline-none focus-visible:border-indigo-400/70 ${dragOver ? "border-indigo-400 bg-indigo-950/10" : ""}`}
      >
        {/* The rows' columns, lined up with them: its padding matches the
            list's and a row's own. In selection mode the box in front selects
            all, or clears the selection once everything is selected. */}
        <div
          style={{ gridTemplateColumns }}
          className="grid gap-1.5 pl-4 pr-3.5 bg-[#161619] border-b border-white/5 font-mono text-[10.5px] text-zinc-300 font-bold shrink-0 sticky top-0 z-10 shadow-md"
        >
          {inMode && (
            <button
              type="button"
              tabIndex={-1}
              role="checkbox"
              aria-checked={allSelected ? "true" : "mixed"}
              onClick={() => (allSelected ? clearSelection() : selectAllVisible())}
              title={allSelected ? "Clear the selection (Esc)" : "Select all (Ctrl+A)"}
              aria-label={allSelected ? "Clear the selection" : "Select all"}
              className="flex items-center justify-center py-1.5 text-indigo-300 hover:text-indigo-100"
            >
              {allSelected ? <CheckSquare size={12} /> : <MinusSquare size={12} />}
            </button>
          )}
          <div className="cursor-pointer hover:text-white py-1.5 truncate" onClick={() => toggleSort("name")}>
            NAME {sortIcon("name")}
          </div>
          {shownColumns.map((c) => (
            <div
              key={c}
              className={`cursor-pointer hover:text-white py-1.5 truncate ${c === "size" ? "text-right" : "pl-1.5"}`}
              onClick={() => toggleSort(c)}
            >
              {COLUMN_LABEL[c]} {sortIcon(c)}
            </div>
          ))}
        </div>

        {/* The rows. A click on the empty space around them clears the
            selection. While the selection bar is up, there is room for it at
            the end, so the last rows can scroll clear of it. */}
        <div
          ref={listRef}
          className={`flex-1 p-1 font-mono text-[11px] ${selectedEntries.length > 0 ? "pb-16" : ""}`}
          onClick={(e) => { if (e.target === e.currentTarget) clearSelection(); }}
        >
          {loading && sortedEntries.length === 0 ? (
            <div className="text-center py-14 text-zinc-400">Loading…</div>
          ) : sortedEntries.length === 0 ? (
            <div className="text-center py-14 px-3 text-zinc-500 break-words">
              {filteredOut ? `No names match “${nameFilter.trim()}”` : "Empty"}
            </div>
          ) : (
            sortedEntries.map((entry) => {
              const isSel = selected.has(entry.path);
              const isCursor = entry.path === cursor;
              const moved = movedColumns.map((c) => columnText(c, entry)).filter(Boolean).join(" · ");
              return (
              <div
                key={entry.path}
                onMouseDown={(e) => handleRowMouseDown(e, entry)}
                onContextMenu={(e) => openMenu(e, entry)}
                onClick={(e) => onRowClick(e, entry, sortedEntries)}
                onDoubleClick={() => openEntry(entry)}
                data-fs-row-path={entry.path}
                data-fs-row-isdir={entry.isDir ? "1" : "0"}
                data-fs-row-selected={isSel ? "1" : "0"}
                style={{ gridTemplateColumns }}
                className={`group grid gap-x-1.5 px-2.5 py-1 border-l-2 cursor-pointer transition-colors items-center scroll-mt-8 ${
                  selectedEntries.length > 0 ? "scroll-mb-16" : ""
                } ${
                  isSel
                    ? "bg-indigo-500/15 border-indigo-400 text-indigo-100"
                    : "border-transparent text-zinc-200 hover:bg-white/5 hover:text-white"
                } ${keyNav && isCursor ? "ring-1 ring-inset ring-indigo-400/60" : ""}`}
              >
                {/* Selection mode only. A click anywhere on the row does what
                    a click on its box would. */}
                {inMode && (
                  <span className="flex items-center justify-center">
                    {isSel
                      ? <CheckSquare size={12} className="text-indigo-300" />
                      : <Square size={12} className="text-zinc-500 group-hover:text-zinc-300" />}
                  </span>
                )}
                <div className="flex items-center gap-2 min-w-0 pr-1">
                  <span
                    className="relative inline-flex shrink-0"
                    title={entry.isSymlink ? (entry.brokenLink ? "Broken link: target is missing or not accessible" : "Symbolic link") : undefined}
                  >
                    {entry.isDir
                      ? <Folder size={12} className="text-indigo-300" />
                      : <File size={12} className={entry.brokenLink ? "text-rose-400/70" : "text-zinc-500"} />}
                    {entry.isSymlink && (
                      <Link2
                        size={8}
                        className={`absolute -bottom-1 -right-1.5 rounded-sm bg-zinc-950 ${entry.brokenLink ? "text-rose-400" : "text-sky-300"}`}
                      />
                    )}
                  </span>
                  <div title={entry.name} className={`min-w-0 flex-1 truncate text-[11px] ${entry.brokenLink ? "text-zinc-400 line-through decoration-rose-400/50" : "text-zinc-100"}`}>{entry.name}</div>
                </div>
                {shownColumns.map((c) => (
                  <div key={c} className={`truncate ${COLUMN_CELL[c]}`} title={c === "owner" ? columnText(c, entry) : undefined}>{columnText(c, entry)}</div>
                ))}
                {/* The row's menu, for a pointer without a right button. Shown
                    on hover, on the selected rows and on the keyboard's row
                    (always on Android, which has no hover). */}
                <button
                  type="button"
                  tabIndex={-1}
                  onMouseDown={(e) => e.stopPropagation()}
                  onClick={(e) => openMenu(e, entry)}
                  title="Options"
                  aria-label="Options"
                  className={`flex items-center justify-center rounded text-zinc-400 hover:text-white hover:bg-white/10 hover:!opacity-100 ${
                    IS_ANDROID || isSel || isCursor ? "opacity-60" : "opacity-0 group-hover:opacity-60"
                  }`}
                >
                  <MoreVertical size={11} />
                </button>
                {/* What the columns that gave way show: a line of its own
                    under the name, as wide as the row. */}
                {moved && (
                  <div
                    style={{ gridColumn: `${inMode ? 2 : 1} / -1` }}
                    className="row-start-2 pl-5 truncate text-[10px] text-zinc-500"
                  >
                    {moved}
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
        const acting = selectedEntries.length > 0 ? selectedEntries : [contextMenu.entry];
        const multi = acting.length > 1;
        const uploadDir = isRemote ? undefined : getOppositeDir?.();
        return (
        <div ref={menuRef}
          style={{ top: contextMenu.y, left: contextMenu.x }}
          className="fixed z-[9999] min-w-[180px] bg-[#0c0c0e] border border-white/10 rounded-lg shadow-2xl p-1 backdrop-blur-md font-mono text-[11.5px] text-zinc-200">

          {/* Primary actions: open a folder or a local file, send what is
              selected to the other pane, live-edit a remote file. */}
          {!multi && contextMenu.entry.isDir && (
            <button onClick={() => { setContextMenu(null); fetch(contextMenu.entry.path); }}
              className="w-full flex items-center gap-2 p-1.5 rounded hover:bg-white/10 text-left hover:text-white">
              <Folder size={11} className="text-indigo-400" /><span>Open</span>
            </button>
          )}
          {!multi && !contextMenu.entry.isDir && !isRemote && !IS_ANDROID && (
            <button onClick={() => { setContextMenu(null); openLocalEntry(contextMenu.entry); }}
              className="w-full flex items-center gap-2 p-1.5 rounded hover:bg-white/10 text-left hover:text-white">
              <ExternalLink size={11} className="text-indigo-400" /><span>Open</span>
            </button>
          )}
          {isRemote ? (
            <button onClick={() => { setContextMenu(null); downloadItems(acting); }}
              className="w-full flex items-center gap-2 p-1.5 rounded hover:bg-white/10 text-left hover:text-white">
              <Download size={11} className="text-emerald-400" />
              <span>Download{multi ? ` (${acting.length})` : "…"}</span>
            </button>
          ) : (
            <button onClick={() => { setContextMenu(null); uploadItems(acting); }}
              disabled={!uploadDir}
              title={uploadDir ? `Upload to ${uploadDir}` : "Open a remote directory first…"}
              className="w-full flex items-center gap-2 p-1.5 rounded hover:bg-white/10 text-left hover:text-white disabled:opacity-40 disabled:hover:bg-transparent disabled:cursor-not-allowed">
              <Upload size={11} className="text-sky-400" />
              <span>Upload{multi ? ` (${acting.length})` : ""}</span>
            </button>
          )}
          {/* Live-edit needs a local temp file + a native OS editor + a
              filesystem watcher. Android has none of those in a form we
              can bridge, so the backend command returns an error there
              — hide the menu item entirely rather than surface it. */}
          {isRemote && !multi && !contextMenu.entry.isDir && !IS_ANDROID && (
            <button onClick={() => { setContextMenu(null); liveEditEntry(contextMenu.entry); }}
              className="w-full flex items-center gap-2 p-1.5 rounded hover:bg-white/10 text-left hover:text-white">
              <ExternalLink size={11} className="text-indigo-400" /><span>Edit (auto-upload)</span>
            </button>
          )}

          {!isRemote && !multi && !IS_ANDROID && (
            <button onClick={() => { setContextMenu(null); revealLocalEntry(contextMenu.entry); }}
              className="w-full flex items-center gap-2 p-1.5 rounded hover:bg-white/10 text-left hover:text-white">
              <FolderSearch size={11} className="text-emerald-300" /><span>Reveal in Explorer</span>
            </button>
          )}

          {/* Into selection mode, where a click adds or removes a row: the
              way to pick several rows without a keyboard. */}
          {!inMode ? (
            <button onClick={() => { setContextMenu(null); setSelectMode(true); }}
              className="w-full flex items-center gap-2 p-1.5 rounded hover:bg-white/10 text-left hover:text-white">
              <CheckSquare size={11} className="text-indigo-300" /><span>Select</span>
            </button>
          ) : !allSelected && (
            <button onClick={() => { setContextMenu(null); selectAllVisible(); }}
              className="w-full flex items-center gap-2 p-1.5 rounded hover:bg-white/10 text-left hover:text-white">
              <CheckSquare size={11} className="text-indigo-300" /><span>Select all</span>
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
          {/* A row's name or full path; for several rows, their paths, one
              per line. */}
          {!multi ? (
            <>
              <button onClick={() => { setContextMenu(null); copyText(contextMenu.entry.name, "name"); }}
                className="w-full flex items-center gap-2 p-1.5 rounded hover:bg-white/10 text-left hover:text-white">
                <Copy size={11} /><span>Copy name</span>
              </button>
              <button onClick={() => { setContextMenu(null); copyText(contextMenu.entry.path, "path"); }}
                className="w-full flex items-center gap-2 p-1.5 rounded hover:bg-white/10 text-left hover:text-white">
                <Copy size={11} /><span>Copy path</span>
              </button>
            </>
          ) : (
            <button onClick={() => { setContextMenu(null); copyText(acting.map((e) => e.path).join("\n"), `${acting.length} paths`); }}
              className="w-full flex items-center gap-2 p-1.5 rounded hover:bg-white/10 text-left hover:text-white">
              <Copy size={11} /><span>Copy {acting.length} paths</span>
            </button>
          )}
          {!multi && provider.chmod && (
            <button onClick={() => openProperties(contextMenu.entry)}
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
            onKeyDown={(e) => { if (e.key === "Escape") { e.preventDefault(); setModal(null); } }}
            className="w-full max-w-[320px] bg-[#121214] border border-white/5 rounded-xl shadow-2xl p-4 font-mono text-[11px]">
            <div className="flex items-center justify-between mb-3">
              <span className="font-black uppercase tracking-wider text-zinc-300">
                {modal.type === "rename" ? "Rename" :
                 modal.type === "mkdir" ? "New Directory" :
                 modal.type === "move" ? "Move to…" :
                 modal.type === "move-bulk" ? `Move ${selectedEntries.length} items to…` : "Properties"}
              </span>
              <button onClick={() => setModal(null)} className="text-zinc-500 hover:text-white"><X size={12} /></button>
            </div>
            <div className="space-y-3">
              {modal.type === "properties" ? (
                <>
                  <div>Name: <span className="text-zinc-100 font-bold">{modal.entry?.name}</span></div>
                  <div>Path: <span className="text-zinc-400 text-[10px] block truncate">{modal.entry?.path}</span></div>
                  {modal.entry?.isSymlink && (
                    <div className="text-[10px] text-amber-300/80">
                      This is a link: these are the settings of the file it points to, and changes apply to that file.
                    </div>
                  )}

                  {/* RWX matrix — owner/group/other × read/write/execute. The
                      checkbox grid is the source of truth; the octal input
                      below mirrors it and accepts manual edits both ways.
                      setuid/setgid/sticky (the 4th octal digit) aren't on the
                      grid but ride along untouched. */}
                  {(() => {
                    const parsed = parseInt(modal.v1 || "0", 8);
                    const full = isNaN(parsed) ? 0 : parsed & 0o7777;
                    const mode = full & 0o777;
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
                      const next = full ^ (bit << shift);
                      setModal({ ...modal, v1: next.toString(8).padStart(3, "0") });
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
                          // Only accept up to 4 digits (setuid/setgid/sticky
                          // first), each 0–7 — anything else is ignored so
                          // the checkbox grid never sees garbage.
                          const v = e.target.value.replace(/[^0-7]/g, "").slice(0, 4);
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
