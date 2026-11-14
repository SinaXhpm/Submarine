import { Search, Plus, Server, Folder, FolderPlus, ChevronLeft, ChevronRight, Trash2, Edit2, Zap, Check, X, Copy, GripVertical, ShieldAlert, KeyRound, Sparkles, LayoutGrid, List } from "lucide-react";
import { useState, useEffect, useLayoutEffect, useMemo, useRef, useCallback } from "react";
import { createPortal } from "react-dom";
import { useConfirm } from "../ui/confirm";
import { HeldReview } from "./HeldReview";
import { HeldDeletions } from "./HeldDeletions";
import { folderTree } from "../util/folderTree";
import { tn, useT } from "../i18n";
import { ai, AI_LEVEL_EVENT, type Level } from "../util/aiApi";
import { LevelMenu } from "./ai/LevelPicker";
import { PROTOCOL_ICON, USUAL_PORT, TAB_DOT, avatarStyle, protocolOf } from "../util/serverLook";

// The servers as a grid of cards or as a list, kept on this device.
const VIEW_KEY = "submarine-servers-view";
// The list's columns: name, address, type, the open tab, the buttons.
const LIST_COLS = "grid grid-cols-[minmax(0,1.3fr)_minmax(0,1.6fr)_4.5rem_minmax(0,9rem)_7rem] items-center gap-3";

export const NodeGrid = ({ servers, folders, credentials, serverTabs, activeFolderId: activeFolderIdProp, onActiveFolderChange, onOpenServer, onEditServer, onAddClick, onQuickConnect, onRemoveServer, onRemoveFolder, onRenameFolder, onCreateFolder, onMoveServer, onMoveFolder, onCloneServer, onReorderServers, onAllowHeld, keyChange, onAcceptKey, shareGrant, onUnlockSharing, heldDeletions, onResolveDeletions, isMobile }: any) => {
  const t = useT();
  const [search, setSearch] = useState("");
  const searchRef = useRef<HTMLInputElement>(null);
  const [view, setView] = useState<"grid" | "list">(() => {
    try { return localStorage.getItem(VIEW_KEY) === "list" ? "list" : "grid"; } catch { return "grid"; }
  });
  const chooseView = (next: "grid" | "list") => {
    setView(next);
    try { localStorage.setItem(VIEW_KEY, next); } catch { /* kept for this visit only */ }
  };
  const listView = view === "list";
  const viewRef = useRef(view);
  viewRef.current = view;
  // "/" goes to the search, from anywhere on the page but a field, a dialog,
  // or a panel open over the page.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== "/" || e.ctrlKey || e.metaKey || e.altKey || e.defaultPrevented) return;
      const el = e.target as HTMLElement | null;
      if (el && (el.isContentEditable || /^(INPUT|TEXTAREA|SELECT)$/.test(el.tagName))) return;
      const box = searchRef.current;
      if (!box || box.closest("[inert]") || document.querySelector('[aria-modal="true"]')) return;
      e.preventDefault();
      box.focus();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);
  // The login shown on a card: a saved login's user name, or the server's own.
  const loginUser = useMemo(() => new Map<number, string>((credentials || []).map((c: any) => [c.id, c.username || ""])), [credentials]);
  const loginOf = (s: any): string => {
    const saved = (s.auth_type || (s.credential_id ? "vault" : "custom_pass")) === "vault";
    return (saved ? loginUser.get(s.credential_id) : s.username) || "";
  };
  // user@host, and the port when it isn't the protocol's usual one.
  const address = (s: any) => {
    const user = loginOf(s);
    const port = Number(s.port);
    const other = port > 0 && port !== USUAL_PORT[protocolOf(s)];
    return (
      <>
        {user && <span className="text-zinc-500">{user}@</span>}
        {s.host}
        {other && <span className="text-zinc-500">:{port}</span>}
      </>
    );
  };
  const tabTitle = (st: string): string =>
    st === "waiting" ? t("Waiting for you — open this tab to answer") :
    st === "connected" ? t("Connected") :
    st === "connecting" ? t("Connecting…") :
    st === "failed" ? t("Connection failed") :
    st === "disconnected" ? t("Disconnected") : st;
  // The kind of connection, as a badge: SSH has one like the others.
  const badge = (s: any) => {
    const p = protocolOf(s);
    const [label, title, tone] =
      p === "rdp" ? ["RDP", t("RDP server"), "text-sky-300 bg-sky-500/10 border-sky-500/20"] :
      p === "telnet" ? ["TELNET", t("Telnet server (not encrypted)"), "text-amber-300 bg-amber-500/10 border-amber-500/20"] :
      p === "vnc" ? ["VNC", t("VNC server"), "text-violet-300 bg-violet-500/10 border-violet-500/20"] :
      p === "ftp" ? ["FTP", t("FTP server (not encrypted)"), "text-amber-300 bg-amber-500/10 border-amber-500/20"] :
      p === "ftps" || p === "ftps-implicit" ? ["FTPS", t("FTPS server (FTP over TLS)"), "text-teal-300 bg-teal-500/10 border-teal-500/20"] :
      ["SSH", t("SSH server"), "text-zinc-300 bg-white/[0.06] border-white/15"];
    return <span title={title} className={`shrink-0 px-1 rounded text-[9px] font-bold tracking-wider leading-[14px] border ${tone}`}>{label}</span>;
  };
  // Changes someone else made in a shared profile that this device holds
  // back until its user allows them (see HeldReview).
  const heldCount: number = (servers || []).reduce((n: number, s: any) => n + (s.held?.length || 0), 0);
  const [reviewOpen, setReviewOpen] = useState(false);
  const closeReview = useCallback(() => setReviewOpen(false), []);
  // Deletions someone else's sync would have made here, held until the user
  // decides (see HeldDeletions).
  const deletionCount: number = (heldDeletions || []).length;
  const [deletionsOpen, setDeletionsOpen] = useState(false);
  const closeDeletions = useCallback(() => setDeletionsOpen(false), []);
  // Native window.confirm() is a silent no-op inside Tauri's Android WebView
  // (same shim gap useTextPrompt was added for), so the delete buttons here
  // silently did nothing on Android after v0.2.32 made the action cluster
  // always-visible on mobile. Route through the themed useConfirm hook.
  const confirm = useConfirm();
  // A shared profile's new key, waiting for the user's OK: they compare its
  // fingerprint with the one in the owner's Profile panel first. Cancel has
  // the focus, so a stray Enter doesn't accept it. The answer goes with the
  // fingerprint shown: the waiting key can change while the dialog is open.
  const reviewKey = async () => {
    if (!keyChange || !onAcceptKey) return;
    const shown: string = keyChange;
    const ok = await confirm({
      title: t("Use the new key?"),
      message: t("This shared profile's key was replaced, usually because its owner removed someone. The new key's fingerprint is {fingerprint}. Ask the owner whether their Profile panel shows the same one. If it doesn't, the key didn't come from them: don't use it.", { fingerprint: shown }),
      okLabel: t("Use the new key"),
      cancelLabel: t("Not now"),
      safeDefault: true,
    });
    if (ok) onAcceptKey(shown);
  };
  // The AI assistant's level per server, for the red mark on Auto and Full
  // access. Only servers off the default are listed, and none when the
  // assistant was never used: an error just leaves the marks off.
  const [aiLevels, setAiLevels] = useState<Map<string, Level>>(() => new Map());
  useEffect(() => {
    let latest = 0;
    const load = () => {
      const n = ++latest;
      ai.serverLevels()
        .then((list) => { if (n === latest) setAiLevels(new Map(list.map((x) => [x.server_uuid, x.level]))); })
        .catch(() => {});
    };
    load();
    window.addEventListener(AI_LEVEL_EVENT, load);
    return () => { latest = -1; window.removeEventListener(AI_LEVEL_EVENT, load); };
  }, []);
  // The card whose "AI permission level…" menu is open, and its button.
  const [aiMenu, setAiMenu] = useState<{ uuid: string; name: string; anchor: HTMLElement } | null>(null);
  const closeAiMenu = useCallback(() => setAiMenu(null), []);
  // Folder navigation is controlled when the parent passes both props, so the
  // selection survives NodeGrid unmount/remount (the parent sticks it onto
  // its own state). Falls back to local state for older callers that don't
  // lift the value — same behaviour as before, just optional.
  const [localFolderId, setLocalFolderId] = useState<number | null>(null);
  const activeFolderId: number | null = onActiveFolderChange ? (activeFolderIdProp ?? null) : localFolderId;
  const setActiveFolderId = (next: number | null) => {
    if (onActiveFolderChange) onActiveFolderChange(next);
    else setLocalFolderId(next);
  };
  // Per-folder inline rename state. Only one folder can be in edit mode at
  // a time, so a single { id, draft } slot is enough.
  const [renaming, setRenaming] = useState<{ id: number; draft: string } | null>(null);
  const commitRename = async () => {
    if (!renaming || !onRenameFolder) return;
    const next = renaming.draft.trim();
    if (next && next !== folders?.find((f: any) => f.id === renaming.id)?.name) {
      try { await onRenameFolder(renaming.id, next); } catch { /* parent surfaces error */ }
    }
    setRenaming(null);
  };

  // Folders nest (see util/folderTree): the grid shows the folders and
  // servers directly inside the open folder (or the top level).
  const tree = useMemo(() => folderTree(folders), [folders]);
  // The open folder was deleted (here or on another device): back to the top.
  useEffect(() => {
    if (activeFolderId !== null && folders && !tree.has(activeFolderId)) setActiveFolderId(null);
  }, [activeFolderId, folders]);

  // "Add folder": the name of the folder being made inside the open folder,
  // typed into a draft card at the head of the grid. null when none is open;
  // closed when another folder is opened.
  const [newFolder, setNewFolder] = useState<string | null>(null);
  useEffect(() => { setNewFolder(null); }, [activeFolderId]);
  const commitNewFolder = async () => {
    const name = (newFolder ?? "").trim();
    if (!name || !onCreateFolder) { setNewFolder(null); return; }
    try {
      await onCreateFolder(name, activeFolderId);
      setNewFolder(null);
    } catch { /* parent logs it; the input stays open for a retry */ }
  };

  const filteredServers = servers?.filter((s: any) =>
    s.name.toLowerCase().includes(search.toLowerCase()) ||
    s.host.includes(search)
  ) || [];

  const currentFolder = folders?.find((f: any) => f.id === activeFolderId);

  // If search is active, we might want to just show all matching servers flat,
  // or still respect the folder. Let's just show them flat if searching.
  const isSearching = search.trim() !== "";

  const displayedServers = isSearching
    ? filteredServers
    : filteredServers.filter((s: any) => tree.serverFolder(s) === activeFolderId);
  const displayedFolders = isSearching ? [] : tree.childrenOf(activeFolderId);

  // Drag a server or a folder onto a folder card (into it) or onto a step of
  // the path above the grid (up to it). The target under the pointer: a
  // folder id, or "root" for the top level.
  type DropTarget = number | "root";
  const [dropTarget, setDropTarget] = useState<DropTarget | null>(null);
  const dropTargetRef = useRef<DropTarget | null>(null);
  const targetAt = (x: number, y: number): DropTarget | null => {
    const el = (document.elementFromPoint(x, y) as HTMLElement | null)?.closest<HTMLElement>("[data-drop-folder]");
    if (!el) return null;
    const v = el.dataset.dropFolder;
    return v === "root" ? "root" : Number(v);
  };
  const setTarget = (t: DropTarget | null) => {
    if (dropTargetRef.current === t) return;
    dropTargetRef.current = t;
    setDropTarget(t);
  };

  // ── Modern floating drag-to-reorder (pointer-based: mouse AND touch) ────────
  // Grabbing the grip lifts the node out of the grid: a fixed-position clone
  // follows the pointer (the "floating" tile), the vacated slot shows a dashed
  // placeholder, and the remaining cards reflow with a smooth FLIP animation as
  // the placeholder tracks the pointer's insertion point. Drag it back and the
  // order returns; on release the final order is persisted via onReorderServers
  // and held until the props catch up so there's no flash of the old order.
  // Disabled while searching (results aren't the stored order).
  const canReorder = !!onReorderServers && !isSearching;
  const [dragId, setDragId] = useState<number | null>(null);
  // Live id order during a drag (null when not dragging). Rendered from when set.
  const [order, setOrder] = useState<number[] | null>(null);
  const gridRef = useRef<HTMLDivElement | null>(null);
  const cloneRef = useRef<HTMLDivElement | null>(null);
  // Latest values read inside the window listeners without re-subscribing them.
  const orderRef = useRef<number[] | null>(null);
  orderRef.current = order;
  const initialOrderRef = useRef<number[]>([]);
  const onReorderRef = useRef<any>(onReorderServers);
  onReorderRef.current = onReorderServers;
  const onMoveServerRef = useRef<any>(onMoveServer);
  onMoveServerRef.current = onMoveServer;
  const rafRef = useRef<number | null>(null);
  // The scroll container + its scrollTop at drag start. The slot snapshot is in
  // viewport coords; if the list scrolls mid-drag the cards move but the
  // snapshot doesn't, so we subtract the scroll delta to keep them aligned.
  const scrollerRef = useRef<HTMLElement | null>(null);
  const scrollTop0Ref = useRef(0);
  // Pointer + grab geometry for the floating clone.
  const pointerRef = useRef<{ x: number; y: number }>({ x: 0, y: 0 });
  const grabOffsetRef = useRef<{ x: number; y: number }>({ x: 0, y: 0 });
  const cardSizeRef = useRef<{ w: number; h: number }>({ w: 240, h: 64 });
  // Fixed snapshot of the grid CELL centres, captured at drag start. Insertion
  // is computed against these — never the live card rects — because the live
  // cards carry in-flight FLIP transforms that would corrupt the maths (the
  // grid cells themselves don't move as the order changes; only which card
  // occupies each cell does).
  const slotsRef = useRef<{ x: number; y: number; h: number }[]>([]);
  // Per-card rects for FLIP (measured after each render, keyed by server id).
  const flipRects = useRef<Map<number, DOMRect>>(new Map());
  // True once a drag actually moves, so the click synthesized on pointerup
  // doesn't open whatever card sits under the finger. Cleared next tick.
  const draggedRef = useRef(false);

  // The lifted copy tilts a little; a list row, much wider, hardly.
  const tilt = () => (viewRef.current === "list" ? "rotate(0.3deg) scale(1.01)" : "rotate(1.5deg) scale(1.03)");
  const moveClone = (x: number, y: number) => {
    const el = cloneRef.current;
    if (el) el.style.transform = `translate(${x - grabOffsetRef.current.x}px, ${y - grabOffsetRef.current.y}px) ${tilt()}`;
  };

  useEffect(() => {
    if (dragId === null) return;
    document.body.style.userSelect = 'none';
    const move = (e: PointerEvent) => {
      draggedRef.current = true;
      pointerRef.current = { x: e.clientX, y: e.clientY };
      moveClone(e.clientX, e.clientY); // clone tracks the pointer 1:1, every frame
      // Over a folder card or a step of the path: the drop moves it there.
      setTarget(onMoveServerRef.current ? targetAt(e.clientX, e.clientY) : null);
      if (rafRef.current != null) return; // coalesce the reflow recompute
      const px = e.clientX, py = e.clientY;
      rafRef.current = requestAnimationFrame(() => {
        rafRef.current = null;
        if (dropTargetRef.current !== null) {
          // Not a new place in this list: show the list as it was.
          const initial = initialOrderRef.current;
          setOrder((prev) => (prev && prev.length === initial.length && prev.every((v, i) => v === initial[i]) ? prev : [...initial]));
          return;
        }
        // Insertion index = how many grid CELLS sit before the pointer in
        // reading order (earlier row, or same row and left of it), measured
        // against the fixed drag-start snapshot. Counting the dragged card's
        // own cell is intentional: removing dragId below shifts everything left
        // by one, so inserting at this raw count lands the card exactly under
        // the pointer. Pure function of pointer position → no flicker, and it
        // works across rows because the snapshot never moves.
        // Compensate for any list scroll since drag start: the cells moved by
        // this delta in viewport space but the snapshot didn't.
        const sd = scrollerRef.current ? scrollerRef.current.scrollTop - scrollTop0Ref.current : 0;
        // In the list the rows stack: a row is before the pointer once the
        // pointer is below it, wherever it is sideways.
        const stacked = viewRef.current === "list";
        let idx = 0;
        for (const c of slotsRef.current) {
          const cy = c.y - sd;
          const before = (cy < py - c.h / 2) || (!stacked && Math.abs(cy - py) <= c.h / 2 && c.x < px);
          if (before) idx++;
        }
        setOrder((prev) => {
          const base = (prev ?? initialOrderRef.current).filter((x) => x !== dragId);
          const clamped = Math.max(0, Math.min(idx, base.length));
          base.splice(clamped, 0, dragId);
          if (prev && prev.length === base.length && prev.every((v, i) => v === base[i])) return prev;
          return base;
        });
      });
    };
    const finish = () => {
      if (rafRef.current != null) { cancelAnimationFrame(rafRef.current); rafRef.current = null; }
      document.body.style.userSelect = '';
      // Clear any leftover FLIP transforms so cards sit cleanly after the drop.
      gridRef.current?.querySelectorAll<HTMLElement>('[data-sid]').forEach((el) => {
        el.style.transition = ''; el.style.transform = '';
      });
      const target = dropTargetRef.current;
      setTarget(null);
      if (target !== null) {
        // Into a folder (or up the path). The card leaves this list once the
        // move is stored, so the order override goes when it's done.
        Promise.resolve(onMoveServerRef.current?.(dragId, target === "root" ? null : target))
          .catch(() => {})
          .finally(() => setOrder(null));
        setDragId(null);
        setTimeout(() => { draggedRef.current = false; }, 0);
        return;
      }
      const final = orderRef.current;
      const initial = initialOrderRef.current;
      const changed = !!final && (final.length !== initial.length || final.some((v, i) => v !== initial[i]));
      if (changed) {
        // Persist and KEEP `order` applied — the catch-up effect below clears it
        // once get_servers returns the same sequence, so the grid never flashes
        // back to the old order between release and refresh. If the persist
        // FAILS, drop the override so the grid reverts to the real (unchanged)
        // order instead of showing an arrangement the DB never accepted.
        Promise.resolve(onReorderRef.current?.(final)).catch(() => setOrder(null));
      } else {
        setOrder(null); // dropped where it started: revert cleanly
      }
      setDragId(null);
      setTimeout(() => { draggedRef.current = false; }, 0);
    };
    window.addEventListener('pointermove', move);
    window.addEventListener('pointerup', finish);
    window.addEventListener('pointercancel', finish);
    return () => {
      window.removeEventListener('pointermove', move);
      window.removeEventListener('pointerup', finish);
      window.removeEventListener('pointercancel', finish);
      document.body.style.userSelect = '';
    };
  }, [dragId]);

  // FLIP: after each drag render, animate every card from its previous box to
  // its new one so the reflow around the placeholder is smooth, not a jump.
  useLayoutEffect(() => {
    const grid = gridRef.current;
    if (!grid || order === null) { flipRects.current.clear(); return; }
    grid.querySelectorAll<HTMLElement>('[data-sid]').forEach((el) => {
      const id = Number(el.dataset.sid);
      // Strip any in-flight FLIP transform first so we read the true LAYOUT box,
      // not a mid-animation position — otherwise fast successive moves compound.
      el.style.transition = 'none';
      el.style.transform = '';
      const next = el.getBoundingClientRect();
      const prev = flipRects.current.get(id);
      if (prev) {
        const dx = prev.left - next.left, dy = prev.top - next.top;
        if (dx || dy) {
          el.style.transform = `translate(${dx}px, ${dy}px)`;
          void el.getBoundingClientRect(); // force reflow so the next line animates
          el.style.transition = 'transform 180ms cubic-bezier(0.2,0,0,1)';
          el.style.transform = '';
        }
      }
      flipRects.current.set(id, next);
    });
  }, [order]);

  // Clear the local `order` override once the incoming servers prop reflects it
  // (or the drag ended with no change) — avoids a flash back to the old order.
  useEffect(() => {
    if (dragId !== null || order === null) return;
    const cur = displayedServers.map((s: any) => s.id);
    if (cur.length === order.length && cur.every((v: number, i: number) => v === order[i])) {
      setOrder(null);
    }
  }, [dragId, order, displayedServers]);

  const beginDrag = (e: any, id: number) => {
    if (!canReorder) return;
    e.preventDefault();
    e.stopPropagation();
    const card = (e.currentTarget as HTMLElement).closest('[data-sid]') as HTMLElement | null;
    const r = card?.getBoundingClientRect();
    grabOffsetRef.current = r ? { x: e.clientX - r.left, y: e.clientY - r.top } : { x: 20, y: 32 };
    cardSizeRef.current = r ? { w: r.width, h: r.height } : { w: 240, h: 64 };
    pointerRef.current = { x: e.clientX, y: e.clientY };
    // Snapshot every grid cell's centre NOW, while all cards are full and
    // untransformed, in visual (row-major) order — the stable reference the
    // move handler counts against.
    const cells = gridRef.current ? Array.from(gridRef.current.querySelectorAll<HTMLElement>('[data-sid]')) : [];
    slotsRef.current = cells.map((el) => {
      const rr = el.getBoundingClientRect();
      return { x: rr.left + rr.width / 2, y: rr.top + rr.height / 2, h: rr.height };
    });
    // Remember the scroll container + position so mid-drag scrolling can be
    // compensated in the move handler (keeps the drop index aligned).
    scrollerRef.current = (gridRef.current?.closest('.custom-scrollbar') as HTMLElement | null) ?? null;
    scrollTop0Ref.current = scrollerRef.current ? scrollerRef.current.scrollTop : 0;
    draggedRef.current = false;
    initialOrderRef.current = displayedServers.map((s: any) => s.id);
    setOrder(initialOrderRef.current);
    setDragId(id);
  };

  // The servers to render, honouring the live drag order when one is active.
  const orderedServers: any[] = (() => {
    if (!order) return displayedServers;
    const byId = new Map<number, any>(displayedServers.map((s: any) => [s.id, s]));
    const out = order.map((id) => byId.get(id)).filter(Boolean);
    // Include any servers not present in `order` (e.g. added mid-drag) at the end.
    for (const s of displayedServers) if (!order.includes(s.id)) out.push(s);
    return out;
  })();

  const draggedServer = dragId != null ? displayedServers.find((s: any) => s.id === dragId) : null;

  // Dragging a folder (by its grip) onto another folder or a step of the
  // path. Folders keep their order, so there's no reordering here; a folder
  // can't go into itself or its own subfolders, nor where it already is.
  const [folderDragId, setFolderDragId] = useState<number | null>(null);
  const folderCloneRef = useRef<HTMLDivElement | null>(null);
  const onMoveFolderRef = useRef<any>(onMoveFolder);
  onMoveFolderRef.current = onMoveFolder;
  const canMoveFolders = !!onMoveFolder && !isSearching;
  const beginFolderDrag = (e: any, id: number) => {
    if (!canMoveFolders) return;
    e.preventDefault();
    e.stopPropagation();
    const card = (e.currentTarget as HTMLElement).closest('[data-drop-folder]') as HTMLElement | null;
    const r = card?.getBoundingClientRect();
    grabOffsetRef.current = r ? { x: e.clientX - r.left, y: e.clientY - r.top } : { x: 20, y: 32 };
    cardSizeRef.current = r ? { w: r.width, h: r.height } : { w: 240, h: 64 };
    pointerRef.current = { x: e.clientX, y: e.clientY };
    draggedRef.current = false;
    setFolderDragId(id);
  };
  useEffect(() => {
    if (folderDragId === null) return;
    const inside = tree.subtreeOf(folderDragId);
    const from = tree.parentOf(folderDragId);
    const allowed = (t: DropTarget | null) =>
      t !== null && (t === "root" ? from !== null : !inside.has(t) && t !== from);
    document.body.style.userSelect = 'none';
    const move = (e: PointerEvent) => {
      draggedRef.current = true;
      pointerRef.current = { x: e.clientX, y: e.clientY };
      const el = folderCloneRef.current;
      if (el) el.style.transform = `translate(${e.clientX - grabOffsetRef.current.x}px, ${e.clientY - grabOffsetRef.current.y}px) ${tilt()}`;
      const t = targetAt(e.clientX, e.clientY);
      setTarget(allowed(t) ? t : null);
    };
    const finish = () => {
      document.body.style.userSelect = '';
      const t = dropTargetRef.current;
      setTarget(null);
      if (t !== null) {
        Promise.resolve(onMoveFolderRef.current?.(folderDragId, t === "root" ? null : t)).catch(() => {});
      }
      setFolderDragId(null);
      setTimeout(() => { draggedRef.current = false; }, 0);
    };
    window.addEventListener('pointermove', move);
    window.addEventListener('pointerup', finish);
    window.addEventListener('pointercancel', finish);
    return () => {
      window.removeEventListener('pointermove', move);
      window.removeEventListener('pointerup', finish);
      window.removeEventListener('pointercancel', finish);
      document.body.style.userSelect = '';
    };
  }, [folderDragId]);
  const draggedFolder = folderDragId != null ? folders?.find((f: any) => f.id === folderDragId) : null;
  const dragging = dragId !== null || folderDragId !== null;

  // The dot on a server's avatar while its tab is open. `edge` is the colour
  // around the avatar, so the dot stands clear of it.
  const tabDot = (tab: string | undefined, edge: string) => tab && (
    <span
      role="img"
      aria-label={tabTitle(tab)}
      title={tabTitle(tab)}
      className={`absolute -bottom-1 -right-1 w-3 h-3 rounded-full border-2 ${edge} ${TAB_DOT[tab] ?? 'bg-zinc-500'}`}
    />
  );

  // A server as a card (the grid) or a row (the list). Its avatar carries the
  // server's own colour and the kind of connection, with a dot while its tab
  // is open. Rendered as a plain keyed <div> (not a nested component) so React
  // reuses the same DOM node across renders — that's what lets the FLIP effect
  // measure and animate real positions. While it is the one being dragged it
  // renders as a dashed placeholder holding the slot; the floating clone
  // (portal, below) is what the pointer carries.
  const renderServer = (s: any) => {
    const aiLevel: Level | undefined = s.uuid ? aiLevels.get(s.uuid) : undefined;
    // The assistant works in SSH sessions. A server with a level set keeps
    // the item, so the level can still be changed back.
    const aiOffered = !!s.uuid && (!['rdp', 'telnet', 'vnc'].includes(s.protocol) || aiLevel !== undefined);
    const aiMenuOpen = !!aiMenu && aiMenu.uuid === s.uuid;
    const Icon = PROTOCOL_ICON[protocolOf(s)];
    const tab: string | undefined = serverTabs?.[s.id];
    if (dragId === s.id) {
      return (
        <div
          key={`server-${s.id}`}
          data-sid={s.id}
          aria-hidden
          className={listView
            ? `${isMobile ? 'h-14' : 'h-11'} rounded-lg border-2 border-dashed border-primary/40 bg-primary/[0.05]`
            : 'h-16 rounded-xl border-2 border-dashed border-primary/40 bg-primary/[0.05]'}
        />
      );
    }
    const open = () => { if (draggedRef.current) return; onOpenServer(s); };

    // Drag handle (grip) — grab to reorder. Over the left edge, hidden until
    // hover on desktop, always shown on touch (no hover there). touch-action:
    // none stops the gesture from scrolling the list while dragging. Pointer
    // only, so not a Tab stop: from the keyboard it would do nothing.
    const grip = canReorder && (
      <button
        tabIndex={-1}
        onPointerDown={(e) => beginDrag(e, s.id)}
        onClick={(e) => e.stopPropagation()}
        title={onMoveServer ? t("Drag to reorder, or onto a folder") : t("Drag to reorder")}
        className="absolute left-0.5 top-1/2 -translate-y-1/2 p-1 rounded-md text-zinc-600 hover:text-zinc-300 cursor-grab active:cursor-grabbing opacity-100 sm:opacity-0 sm:group-hover:opacity-100 [@media(hover:none)]:!opacity-100 transition-opacity"
        style={{ touchAction: 'none' }}
      >
        <GripVertical size={14} />
      </button>
    );

    // A button so the keyboard can open it too: its click bubbles to the
    // card's own handler.
    const name = (
      <h3 className={`min-w-0 text-zinc-100 truncate tracking-tight leading-tight ${listView && !isMobile ? 'text-[12.5px] font-semibold' : 'text-[13px] font-bold'}`}>
        <button type="button" className="max-w-full truncate text-left rounded outline-none focus-visible:ring-2 focus-visible:ring-primary/60">{s.name}</button>
      </h3>
    );

    // Auto or Full access: red, like the SFTP root badge.
    const aiMark = (aiLevel === 'auto' || aiLevel === 'full') && (
      <span
        title={aiLevel === 'full'
          ? "Full access: the AI assistant runs every command on this server without asking, until the session tab closes"
          : "Auto: the AI assistant runs commands on this server without asking, except risky ones"}
        className="shrink-0 inline-flex items-center gap-0.5 px-1 rounded text-[9px] font-black uppercase tracking-wider leading-[14px] text-rose-200 bg-rose-500/20 border border-rose-500/50"
      >
        <ShieldAlert size={9} className="shrink-0" />
        {aiLevel === 'full' ? 'AI full' : 'AI auto'}
      </span>
    );

    const buttons = (
      <>
        {aiOffered && (
          <button
            onClick={(e) => {
              e.stopPropagation();
              const anchor = e.currentTarget;
              setAiMenu(aiMenuOpen ? null : { uuid: s.uuid, name: s.name, anchor });
            }}
            title="AI permission level…"
            aria-label="AI permission level…"
            aria-haspopup="menu"
            aria-expanded={aiMenuOpen}
            className={`${isMobile ? 'h-10 px-2 flex items-center' : 'p-1.5'} rounded-lg text-zinc-500 hover:text-primary hover:bg-white/5 transition-all`}
          >
            <Sparkles size={13} />
          </button>
        )}
        {onCloneServer && (
          <button
            onClick={(e) => { e.stopPropagation(); onCloneServer(s.id); }}
            title={t("Clone")}
            className="p-1.5 rounded-lg text-zinc-500 hover:text-primary hover:bg-white/5 transition-all"
          >
            <Copy size={13} />
          </button>
        )}
        <button
          onClick={(e) => { e.stopPropagation(); onEditServer(s); }}
          title={t("Edit")}
          className="p-1.5 rounded-lg text-zinc-500 hover:text-white hover:bg-white/5 transition-all"
        >
          <Edit2 size={13} />
        </button>
        <button
          onClick={async (e) => {
            e.stopPropagation();
            if (await confirm({ title: t("Delete server?"), message: t("“{name}” will be removed from this profile.", { name: s.name }), destructive: true })) {
              onRemoveServer(s.id);
            }
          }}
          title={t("Delete")}
          className="p-1.5 rounded-lg text-zinc-500 hover:text-red-500 hover:bg-white/5 transition-all"
        >
          <Trash2 size={13} />
        </button>
      </>
    );

    // The list on a wide screen: one row, a column each.
    if (listView && !isMobile) {
      return (
        <div
          key={`server-${s.id}`}
          data-sid={s.id}
          onClick={open}
          className={`group relative ${LIST_COLS} h-11 pl-7 pr-2 border-t border-white/5 first:border-t-0 hover:bg-white/[0.03] transition-colors cursor-pointer`}
        >
          {grip}
          <div className="flex items-center gap-2.5 min-w-0">
            <div className="w-6 h-6 rounded-md border flex items-center justify-center shrink-0" style={avatarStyle(s.color)}>
              <Icon size={12} />
            </div>
            {name}
            {aiMark}
          </div>
          <span className="text-[11px] text-zinc-300 font-mono truncate">{address(s)}</span>
          <span className="flex">{badge(s)}</span>
          <span className="flex items-center gap-1.5 min-w-0 text-[11.5px] text-zinc-300">
            {tab && (
              <>
                <span aria-hidden className={`w-2 h-2 rounded-full shrink-0 ${TAB_DOT[tab] ?? 'bg-zinc-500'}`} />
                <span className="truncate" title={tabTitle(tab)}>{tabTitle(tab)}</span>
              </>
            )}
          </span>
          <div className={`flex items-center justify-end gap-0.5 opacity-0 group-hover:opacity-100 group-has-[:focus-visible]:opacity-100 [@media(hover:none)]:!opacity-100 transition-opacity ${aiMenuOpen ? '!opacity-100' : ''}`}>
            {buttons}
          </div>
        </div>
      );
    }

    // A card, or a row of the list on a phone: the avatar, the name over the
    // address. Actions: absolute + hover-revealed on desktop so the name gets
    // the full width; inline + always visible on a phone (no hover there).
    return (
      <div
        key={`server-${s.id}`}
        data-sid={s.id}
        onClick={open}
        className={listView
          ? 'group relative flex items-center gap-3 h-14 pl-7 pr-1 border-t border-white/5 first:border-t-0 active:bg-white/[0.03] cursor-pointer'
          : 'group relative flex items-center gap-3 h-16 pl-6 pr-2 rounded-xl bg-gradient-to-br from-[#17171c] to-[#121216] border border-white/5 hover:border-primary/40 hover:bg-white/[0.015] transition-colors duration-150 cursor-pointer overflow-hidden'}
      >
        {grip}
        <div className={`relative ${listView ? 'w-8 h-8' : 'w-9 h-9'} rounded-lg border flex items-center justify-center shrink-0`} style={avatarStyle(s.color)}>
          <Icon size={listView ? 14 : 16} />
          {tabDot(tab, listView ? 'border-[#121216]' : 'border-[#16161b]')}
        </div>
        <div className="min-w-0 flex-1">
          {name}
          <div className="flex items-center gap-1.5 mt-1">
            {aiMark}
            {badge(s)}
            <span className="text-[10.5px] text-zinc-300 font-mono truncate">{address(s)}</span>
          </div>
        </div>
        <div className={`flex items-center gap-0.5 shrink-0 opacity-100 sm:opacity-0 sm:group-hover:opacity-100 sm:group-has-[:focus-visible]:opacity-100 [@media(hover:none)]:!opacity-100 sm:absolute sm:right-1.5 sm:top-1/2 sm:-translate-y-1/2 sm:pl-8 sm:bg-gradient-to-l sm:from-[#121216] sm:via-[#121216] sm:to-transparent transition-all duration-200 ${aiMenuOpen ? '!opacity-100' : ''}`}>
          {buttons}
        </div>
      </div>
    );
  };

  // Deleting a folder takes everything below it too: the question says how much.
  const deleteFolder = async (f: any) => {
    const inside = tree.subtreeOf(f.id);
    const subfolders = inside.size - 1;
    const allServers = servers?.filter((s: any) => inside.has(tree.serverFolder(s) as number)).length || 0;
    const serversText = tn(allServers, "{count} server", "{count} servers");
    const what = subfolders > 0
      ? t("{folders} and {servers}", { folders: tn(subfolders, "{count} folder", "{count} folders"), servers: serversText })
      : serversText;
    const message = subfolders > 0
      ? t("“{name}” and everything inside it ({what}) will be removed.", { name: f.name, what })
      : t("“{name}” and every server inside it ({what}) will be removed.", { name: f.name, what });
    if (await confirm({ title: t("Delete folder?"), message, destructive: true })) {
      onRemoveFolder(f.id);
    }
  };

  // Folders share the servers' language so the page reads as one surface: an
  // avatar in the folder's colour, the name with what's inside, rename in
  // place. A function that returns the card, not a component: one defined in
  // here would be a new type on each render, and React would rebuild every
  // card, the rename field with them.
  const renderFolder = (f: any) => {
    const serverCount = servers?.filter((s: any) => tree.serverFolder(s) === f.id).length || 0;
    const folderCount = tree.childrenOf(f.id).length;
    const isRenaming = renaming?.id === f.id;
    const isTarget = dropTarget === f.id;
    const isDragged = folderDragId === f.id;
    const contents = [
      folderCount > 0 ? tn(folderCount, "{count} folder", "{count} folders") : null,
      tn(serverCount, "{count} server", "{count} servers"),
    ].filter(Boolean).join(' · ');
    const wideRow = listView && !isMobile;

    // Drag handle: drop the folder into another one, or up the path.
    const grip = canMoveFolders && !isRenaming && (
      <button
        tabIndex={-1}
        onPointerDown={(e) => beginFolderDrag(e, f.id)}
        onClick={(e) => e.stopPropagation()}
        title={t("Drag into another folder")}
        className="absolute left-0.5 top-1/2 -translate-y-1/2 p-1 rounded-md text-zinc-600 hover:text-zinc-300 cursor-grab active:cursor-grabbing opacity-100 sm:opacity-0 sm:group-hover:opacity-100 [@media(hover:none)]:!opacity-100 transition-opacity"
        style={{ touchAction: 'none' }}
      >
        <GripVertical size={14} />
      </button>
    );
    const avatar = (
      <div className={`${wideRow ? 'w-6 h-6 rounded-md' : listView ? 'w-8 h-8 rounded-lg' : 'w-9 h-9 rounded-lg'} border flex items-center justify-center shrink-0`} style={avatarStyle(f.color)}>
        <Folder size={wideRow ? 12 : listView ? 14 : 16} />
      </div>
    );
    const field = isRenaming && (
      <input
        autoFocus
        value={renaming!.draft}
        onChange={(e) => setRenaming({ id: f.id, draft: e.target.value })}
        onClick={(e) => e.stopPropagation()}
        onKeyDown={(e) => {
          if (e.key === "Enter") { e.preventDefault(); commitRename(); }
          if (e.key === "Escape") { e.preventDefault(); setRenaming(null); }
        }}
        className="w-full min-w-0 h-7 px-2 bg-black/40 border border-primary/40 rounded-lg text-[13px] font-bold text-white outline-none"
      />
    );
    const name = (
      <h3 className={`min-w-0 text-zinc-100 truncate tracking-tight leading-tight ${wideRow ? 'text-[12.5px] font-semibold' : 'text-[13px] font-bold'}`}>
        <button type="button" className="max-w-full truncate text-left rounded outline-none focus-visible:ring-2 focus-visible:ring-primary/60">{f.name}</button>
      </h3>
    );
    const renameButtons = (
      <div className="flex items-center gap-0.5 shrink-0">
        <button
          onClick={(e) => { e.stopPropagation(); commitRename(); }}
          title={t("Save name")}
          className="p-1.5 text-emerald-400 hover:bg-white/10 rounded-lg"
        >
          <Check size={14} />
        </button>
        <button
          onClick={(e) => { e.stopPropagation(); setRenaming(null); }}
          title={t("Cancel")}
          className="p-1.5 text-zinc-400 hover:text-rose-400 hover:bg-white/10 rounded-lg"
        >
          <X size={14} />
        </button>
      </div>
    );
    const buttons = (
      <>
        <button
          onClick={(e) => { e.stopPropagation(); setRenaming({ id: f.id, draft: f.name || "" }); }}
          title={t("Rename folder")}
          className="p-1.5 rounded-lg text-zinc-500 hover:text-primary hover:bg-white/5 transition-all"
        >
          <Edit2 size={13} />
        </button>
        <button
          onClick={(e) => { e.stopPropagation(); void deleteFolder(f); }}
          title={t("Delete folder")}
          className="p-1.5 rounded-lg text-zinc-500 hover:text-red-500 hover:bg-white/5 transition-all"
        >
          <Trash2 size={13} />
        </button>
      </>
    );
    const openFolder = () => { if (!isRenaming && !draggedRef.current) setActiveFolderId(f.id); };

    if (listView) {
      return (
        <div
          key={`folder-${f.id}`}
          data-drop-folder={f.id}
          onClick={openFolder}
          className={`group relative ${wideRow ? `${LIST_COLS} h-11 pr-2` : 'flex items-center gap-3 h-14 pr-1'} pl-7 border-t border-white/5 first:border-t-0 cursor-pointer transition-colors ${
            isTarget ? 'bg-primary/10 ring-2 ring-inset ring-primary/60' : 'hover:bg-white/[0.03]'
          } ${isDragged ? 'opacity-40' : ''}`}
        >
          {grip}
          {wideRow ? (
            <>
              <div className="flex items-center gap-2.5 min-w-0">{avatar}{field || name}</div>
              <span className="text-[11px] text-zinc-500 truncate">{contents}</span>
              <span />
              <span />
              {isRenaming ? <div className="flex justify-end">{renameButtons}</div> : (
                <div className="flex items-center justify-end gap-0.5 opacity-0 group-hover:opacity-100 group-has-[:focus-visible]:opacity-100 [@media(hover:none)]:!opacity-100 transition-opacity">{buttons}</div>
              )}
            </>
          ) : (
            <>
              {avatar}
              <div className="min-w-0 flex-1">
                {field || (
                  <>
                    {name}
                    <span className="text-[10.5px] text-zinc-500">{contents}</span>
                  </>
                )}
              </div>
              {isRenaming ? renameButtons : <div className="flex items-center gap-0.5 shrink-0">{buttons}</div>}
            </>
          )}
        </div>
      );
    }

    return (
      <div
        key={`folder-${f.id}`}
        data-drop-folder={f.id}
        onClick={openFolder}
        className={`group relative flex items-center gap-3 h-16 pl-6 pr-2 rounded-xl bg-gradient-to-br from-[#1c1c22] to-[#141418] border transition-colors duration-150 cursor-pointer overflow-hidden ${
          isTarget ? 'border-primary ring-2 ring-primary/60 bg-primary/10' : 'border-white/5 hover:border-primary/40 hover:bg-white/[0.015]'
        } ${isDragged ? 'opacity-40' : ''}`}
      >
        {grip}
        {avatar}
        <div className="min-w-0 flex-1">
          {field || (
            <>
              {name}
              <span className="text-[10px] text-zinc-500 font-bold uppercase tracking-[0.12em]">{contents}</span>
            </>
          )}
        </div>
        {/* Right cluster: count badge (idle) that swaps to actions on hover. */}
        {isRenaming ? renameButtons : (
          <div className="relative flex items-center shrink-0">
            <span className="text-[10px] bg-black/60 text-zinc-400 px-1.5 py-0.5 rounded-md font-mono group-hover:opacity-0 group-has-[:focus-visible]:opacity-0 transition-opacity">{serverCount}</span>
            <div className="absolute right-0 flex items-center gap-0.5 opacity-100 sm:opacity-0 sm:group-hover:opacity-100 sm:group-has-[:focus-visible]:opacity-100 [@media(hover:none)]:!opacity-100 transition-all bg-gradient-to-l from-[#16161b] via-[#16161b] to-transparent pl-6">
              {buttons}
            </div>
          </div>
        )}
      </div>
    );
  };

  // The folder being made with "Add folder": a card or a row to name it.
  // Plain JSX, not a nested component, so typing keeps focus.
  const newFolderDraft = !isSearching && newFolder !== null && (
    <div className={listView
      ? 'relative flex items-center gap-3 h-14 pl-7 pr-2 border-t border-white/5 first:border-t-0 bg-primary/[0.04]'
      : 'relative flex items-center gap-3 h-16 pl-6 pr-2 rounded-xl bg-gradient-to-br from-[#1c1c22] to-[#141418] border border-primary/40 overflow-hidden'}>
      <div className={`${listView ? 'w-8 h-8' : 'w-9 h-9'} rounded-lg border flex items-center justify-center shrink-0`} style={avatarStyle(null)}>
        <FolderPlus size={listView ? 14 : 16} />
      </div>
      <input
        autoFocus
        aria-label={t("New folder name")}
        placeholder={t("Folder name…")}
        value={newFolder}
        onChange={(e) => setNewFolder(e.target.value)}
        onKeyDown={(e) => {
          if (e.key === "Enter") { e.preventDefault(); commitNewFolder(); }
          if (e.key === "Escape") { e.preventDefault(); setNewFolder(null); }
        }}
        className="min-w-0 flex-1 h-7 px-2 bg-black/40 border border-primary/40 rounded-lg text-[13px] font-bold text-white outline-none placeholder:text-zinc-600 placeholder:font-normal"
      />
      <div className="flex items-center gap-0.5 shrink-0">
        <button
          onClick={commitNewFolder}
          disabled={!newFolder.trim()}
          title={t("Create folder")}
          aria-label={t("Create folder")}
          className="p-1.5 text-emerald-400 hover:bg-white/10 rounded-lg disabled:opacity-40 disabled:hover:bg-transparent"
        >
          <Check size={14} />
        </button>
        <button
          onClick={() => setNewFolder(null)}
          title={t("Cancel")}
          aria-label={t("Cancel new folder")}
          className="p-1.5 text-zinc-400 hover:text-rose-400 hover:bg-white/10 rounded-lg"
        >
          <X size={14} />
        </button>
      </div>
    </div>
  );

  // What a folder or a search holds when it holds nothing.
  const emptyNote =
    !isSearching && activeFolderId !== null && displayedServers.length === 0 && displayedFolders.length === 0 && newFolder === null
      ? t("Nothing in this folder yet. Add a server or folder here, or drag a server onto this folder from the level above.")
      : isSearching && displayedServers.length === 0
        ? t("No matches.")
        : null;

  const headerButton = "inline-flex items-center gap-1.5 h-10 sm:h-8 rounded-lg border text-[11px] font-bold uppercase tracking-wider transition-colors shrink-0";

  return (
    <div className="flex-1 flex flex-col h-full p-6 overflow-hidden bg-transparent">
      {/* One row, the same at the top level and inside folders: where you
          are, the search, and the buttons. */}
      <div className="flex flex-wrap items-center gap-2 mb-5 shrink-0">
        {activeFolderId === null ? (
          <h2 className="flex items-center gap-1.5 flex-1 min-w-[10rem] text-[13px] font-bold text-zinc-100 tracking-tight">
            <Server size={15} className="text-primary shrink-0" /> {t("All servers")}
          </h2>
        ) : (
          <div className="flex items-center gap-2 flex-1 min-w-[12rem]">
            <button aria-label={t("Back")}
              title={t("Up one level")}
              onClick={() => setActiveFolderId(tree.parentOf(activeFolderId))}
              className="w-8 h-8 bg-black rounded-xl border border-white/5 flex items-center justify-center hover:bg-white/10 hover:text-white text-zinc-500 transition-all shadow-inner shrink-0"
            >
              <ChevronLeft size={16} />
            </button>
            {/* The path down to this folder. Each step above it opens that
                level, and takes a server or folder dropped on it. */}
            <nav aria-label={t("Folder path")} className="flex items-center gap-1 min-w-0 flex-wrap text-[13px]">
              {[{ id: "root", name: t("All servers") }, ...tree.pathOf(activeFolderId)].map((step: any, i: number, all: any[]) => {
                const last = i === all.length - 1;
                const target: DropTarget = step.id === "root" ? "root" : step.id;
                const hot = dropTarget === target;
                return (
                  <span key={String(step.id)} className="flex items-center gap-1 min-w-0">
                    {i > 0 && <ChevronRight size={13} className="text-zinc-600 shrink-0" />}
                    {last ? (
                      <span className="font-bold text-primary tracking-tight flex items-center gap-1.5 truncate">
                        <Folder size={15} className="shrink-0" /> <span className="truncate">{step.name}</span>
                      </span>
                    ) : (
                      <button
                        data-drop-folder={target}
                        onClick={() => setActiveFolderId(step.id === "root" ? null : step.id)}
                        className={`px-1.5 py-0.5 rounded-md truncate max-w-[12rem] transition-colors ${
                          hot ? 'bg-primary/20 text-primary ring-2 ring-primary/60' : 'text-zinc-400 hover:text-white hover:bg-white/5'
                        } ${dragging ? 'ring-1 ring-white/10' : ''}`}
                      >
                        {step.name}
                      </button>
                    )}
                  </span>
                );
              })}
            </nav>
          </div>
        )}
        {/* Search: on a phone a row of its own, under the rest. Enter opens
            the first match; Escape empties it. */}
        <div className="relative w-full sm:w-60 order-last sm:order-none">
          <Search className="absolute left-3 top-1/2 -translate-y-1/2 text-zinc-400 pointer-events-none" size={14} />
          <input
            ref={searchRef}
            type="text"
            aria-label={t("Search servers…")}
            placeholder={t("Search servers…")}
            className="w-full h-10 sm:h-8 bg-[#1c1c21] border border-white/10 rounded-lg pl-9 pr-8 text-[13px] text-zinc-100 outline-none focus:border-primary/50 focus:bg-[#16161a] transition-all placeholder:text-zinc-600 shadow-inner"
            value={search}
            onChange={(e) => setSearch(e.target.value)}
            onKeyDown={(e) => {
              if (e.nativeEvent.isComposing) return;
              if (e.key === "Enter" && isSearching && displayedServers.length > 0) {
                e.preventDefault();
                onOpenServer(displayedServers[0]);
              } else if (e.key === "Escape" && search) {
                e.preventDefault();
                e.stopPropagation();
                setSearch("");
              }
            }}
          />
          {!search && !isMobile && (
            <kbd aria-hidden className="absolute right-2 top-1/2 -translate-y-1/2 px-1.5 rounded border border-white/10 text-[10px] leading-4 text-zinc-500 font-mono pointer-events-none">/</kbd>
          )}
        </div>
        <div className="flex items-center gap-2 flex-wrap">
          {/* Each half has its own edge, so each is as tall as the buttons beside it. */}
          <div className="inline-flex shrink-0">
            <button
              type="button"
              aria-pressed={!listView}
              aria-label={t("Show as a grid")}
              title={t("Show as a grid")}
              onClick={() => chooseView("grid")}
              className={`h-10 sm:h-8 w-10 sm:w-8 flex items-center justify-center rounded-l-lg border border-white/10 transition-colors ${!listView ? 'bg-primary/15 text-primary' : 'text-zinc-500 hover:text-zinc-200 hover:bg-white/5'}`}
            >
              <LayoutGrid size={14} />
            </button>
            <button
              type="button"
              aria-pressed={listView}
              aria-label={t("Show as a list")}
              title={t("Show as a list")}
              onClick={() => chooseView("list")}
              className={`h-10 sm:h-8 w-10 sm:w-8 -ml-px flex items-center justify-center rounded-r-lg border border-white/10 transition-colors ${listView ? 'bg-primary/15 text-primary' : 'text-zinc-500 hover:text-zinc-200 hover:bg-white/5'}`}
            >
              <List size={14} />
            </button>
          </div>
          {/* On a phone these two are icons, so the buttons share one row:
              their names stay for screen readers and as tooltips. */}
          {onCreateFolder && (
            <button
              onClick={() => setNewFolder((v) => v ?? "")}
              title={activeFolderId === null ? t("Group servers in a folder") : t("Make a folder inside this one")}
              aria-label={t("Add folder")}
              className={`${headerButton} ${isMobile ? 'w-10 justify-center' : 'px-3'} bg-primary/10 border-primary/25 text-primary hover:bg-primary/20`}
            >
              <FolderPlus size={13} /> {!isMobile && t("Add folder")}
            </button>
          )}
          {onQuickConnect && (
            <button
              onClick={onQuickConnect}
              title={t("Quick connect")}
              aria-label={t("Quick connect")}
              className={`${headerButton} ${isMobile ? 'w-10 justify-center' : 'px-3'} bg-amber-500/10 border-amber-500/25 text-amber-300/90 hover:bg-amber-500/20 hover:text-amber-200`}
            >
              <Zap size={13} /> {!isMobile && t("Quick connect")}
            </button>
          )}
          {/* From inside a folder the new server starts in that folder, so
              the user doesn't have to repeat the choice they just made. */}
          <button
            onClick={() => onAddClick(activeFolderId)}
            className={`${headerButton} px-3 bg-primary/15 border-primary/30 text-primary hover:bg-primary/25`}
          >
            <Plus size={13} /> {t("Add server")}
          </button>
        </div>
      </div>

      {heldCount > 0 && onAllowHeld && (
        <div role="status" className="w-full mb-4 shrink-0 flex items-center gap-3 rounded-lg border border-amber-400/25 bg-amber-400/[0.06] px-3 py-2 text-[12px] text-amber-100/90">
          <ShieldAlert size={14} className="text-amber-400 shrink-0" />
          <span className="flex-1 min-w-0">
            {tn(heldCount, "A change made elsewhere is waiting for your OK.", "{count} changes made elsewhere are waiting for your OK.")}
          </span>
          <button
            onClick={() => setReviewOpen(true)}
            className="shrink-0 px-2.5 h-7 rounded text-[10.5px] font-bold uppercase tracking-wider bg-amber-400/10 border border-amber-400/30 text-amber-200 hover:bg-amber-400/20"
          >
            {t("Review")}
          </button>
        </div>
      )}
      {reviewOpen && onAllowHeld && (
        <HeldReview servers={servers || []} onAllow={onAllowHeld} onClose={closeReview} />
      )}
      {deletionCount > 0 && onResolveDeletions && (
        <div role="status" className="w-full mb-4 shrink-0 flex items-center gap-3 rounded-lg border border-amber-400/25 bg-amber-400/[0.06] px-3 py-2 text-[12px] text-amber-100/90">
          <Trash2 size={14} className="text-amber-400 shrink-0" />
          <span className="flex-1 min-w-0">
            {(heldDeletions || []).some((d: any) => d.emptied)
              ? tn(deletionCount, "An item was deleted or emptied by someone you share this profile with. It stays here until you decide.", "{count} items were deleted or emptied by someone you share this profile with. They stay here until you decide.")
              : tn(deletionCount, "An item was deleted by someone you share this profile with. It stays here until you decide.", "{count} items were deleted by someone you share this profile with. They stay here until you decide.")}
          </span>
          <button
            onClick={() => setDeletionsOpen(true)}
            className="shrink-0 px-2.5 h-7 rounded text-[10.5px] font-bold uppercase tracking-wider bg-amber-400/10 border border-amber-400/30 text-amber-200 hover:bg-amber-400/20"
          >
            {t("Review")}
          </button>
        </div>
      )}
      {deletionsOpen && onResolveDeletions && (
        <HeldDeletions items={heldDeletions || []} onResolve={onResolveDeletions} onClose={closeDeletions} />
      )}
      {keyChange && onAcceptKey && (
        <div role="status" className="w-full mb-4 shrink-0 flex items-center gap-3 rounded-lg border border-amber-400/25 bg-amber-400/[0.06] px-3 py-2 text-[12px] text-amber-100/90">
          <KeyRound size={14} className="text-amber-400 shrink-0" />
          <span className="flex-1 min-w-0">
            {t("This shared profile's key has changed. Your changes aren't uploaded until you accept the new key.")}
          </span>
          <button
            onClick={reviewKey}
            className="shrink-0 px-2.5 h-7 rounded text-[10.5px] font-bold uppercase tracking-wider bg-amber-400/10 border border-amber-400/30 text-amber-200 hover:bg-amber-400/20"
          >
            {t("Review")}
          </button>
        </div>
      )}
      {/* The grant changed and this device couldn't open it (note_share_grant):
          it only pulls until that's sorted out. */}
      {shareGrant && (
        <div role="status" className="w-full mb-4 shrink-0 flex items-center gap-3 rounded-lg border border-amber-400/25 bg-amber-400/[0.06] px-3 py-2 text-[12px] text-amber-100/90">
          <KeyRound size={14} className="text-amber-400 shrink-0" />
          <span className="flex-1 min-w-0">
            {shareGrant === "locked"
              ? t("This shared profile's key may have changed. Unlock sharing to check. Until then you can't see what's new, and your changes aren't uploaded.")
              : shareGrant === "checking"
                ? t("Sharing is unlocked. The next sync checks this shared profile's key. Until then you can't see what's new, and your changes aren't uploaded.")
                : t("This shared profile has a new key your sharing key can't open. Ask its owner to invite you again. Until then your changes aren't uploaded.")}
          </span>
          {shareGrant === "locked" && onUnlockSharing && (
            <button
              onClick={onUnlockSharing}
              className="shrink-0 px-2.5 h-7 rounded text-[10.5px] font-bold uppercase tracking-wider bg-amber-400/10 border border-amber-400/30 text-amber-200 hover:bg-amber-400/20"
            >
              {t("Unlock sharing")}
            </button>
          )}
        </div>
      )}

      <div className="flex-1 overflow-y-auto custom-scrollbar pr-1 pb-10">
        {servers?.length === 0 && folders?.length === 0 ? (
          <div className="h-full flex items-center justify-center">
            <button
              onClick={() => onAddClick(null)}
              className="flex flex-col items-center gap-4 p-12 rounded-3xl border-2 border-dashed border-zinc-800 hover:border-primary/40 hover:bg-primary/5 transition-all group"
            >
              <div className="w-14 h-14 bg-primary/10 rounded-2xl flex items-center justify-center text-primary shadow-[0_0_15px_rgba(var(--primary),0.1)] transition-transform">
                <Plus size={28} />
              </div>
              <div className="text-center">
                <h3 className="text-[15px] font-bold text-white tracking-tight">{t("Add your first server")}</h3>
                <p className="text-[12px] text-zinc-500 mt-1">{t("Tap here to get started.")}</p>
              </div>
            </button>
          </div>
        ) : listView ? (
          <div className="rounded-xl border border-white/5 bg-[#121216] overflow-hidden">
            {!isMobile && (
              <div className={`${LIST_COLS} h-8 pl-7 pr-2 border-b border-white/5 bg-white/[0.02] text-[10px] font-bold uppercase tracking-[0.12em] text-zinc-500`}>
                <span>{t("Name")}</span>
                <span>{t("Address")}</span>
                <span>{t("Type")}</span>
                <span>{t("Session")}</span>
                <span />
              </div>
            )}
            <div ref={gridRef}>
              {newFolderDraft}
              {displayedFolders.map((f: any) => renderFolder(f))}
              {orderedServers.map((s: any) => renderServer(s))}
              {emptyNote && (
                <div className="px-4 py-8 text-center text-zinc-600 text-xs italic">{emptyNote}</div>
              )}
            </div>
          </div>
        ) : (
          <div ref={gridRef} className={`grid ${isMobile ? 'grid-cols-1' : 'grid-cols-2 lg:grid-cols-3 xl:grid-cols-4'} gap-3`}>
            {newFolderDraft}
            {/* The folders inside the open one (or the top level); none while searching. */}
            {displayedFolders.map((f: any) => renderFolder(f))}
            {/* The servers, in the live drag order while reordering. */}
            {orderedServers.map((s: any) => renderServer(s))}
            {emptyNote && (
              <div className="col-span-full py-10 text-center text-zinc-600 text-xs italic">{emptyNote}</div>
            )}
          </div>
        )}
      </div>

      {/* Floating drag clone — a fixed-position copy of the node that the
          pointer carries while the grid reflows underneath. pointer-events:none
          so it never intercepts the elementFromPoint / hover logic. */}
      {draggedServer && createPortal(
        <div
          ref={cloneRef}
          className="fixed left-0 top-0 z-[9999] pointer-events-none will-change-transform"
          style={{
            width: cardSizeRef.current.w,
            transform: `translate(${pointerRef.current.x - grabOffsetRef.current.x}px, ${pointerRef.current.y - grabOffsetRef.current.y}px) ${tilt()}`,
          }}
        >
          {(() => {
            const Icon = PROTOCOL_ICON[protocolOf(draggedServer)];
            return (
              <div className={`relative flex items-center gap-3 ${listView ? 'h-11 pl-7 rounded-lg bg-[#1b1b20]' : 'h-16 pl-6 rounded-xl bg-gradient-to-br from-[#1b1b20] to-[#141418]'} pr-2 border border-primary/50 ring-1 ring-primary/40 shadow-2xl shadow-black/60 overflow-hidden`}>
                <div className={`${listView ? 'w-6 h-6 rounded-md' : 'w-9 h-9 rounded-lg'} border flex items-center justify-center shrink-0`} style={avatarStyle(draggedServer.color)}>
                  <Icon size={listView ? 12 : 16} />
                </div>
                <div className={`min-w-0 flex-1 ${listView ? 'flex items-center gap-3' : ''}`}>
                  <h3 className="font-bold text-zinc-100 text-[13px] truncate tracking-tight leading-tight">{draggedServer.name}</h3>
                  <div className={`flex items-center gap-1.5 min-w-0 ${listView ? '' : 'mt-1'}`}>
                    {badge(draggedServer)}
                    <span className="text-[10.5px] text-zinc-300 font-mono truncate">{address(draggedServer)}</span>
                  </div>
                </div>
              </div>
            );
          })()}
        </div>,
        document.body
      )}

      {/* The same floating copy for a folder being dragged. */}
      {draggedFolder && createPortal(
        <div
          ref={folderCloneRef}
          className="fixed left-0 top-0 z-[9999] pointer-events-none will-change-transform"
          style={{
            width: cardSizeRef.current.w,
            transform: `translate(${pointerRef.current.x - grabOffsetRef.current.x}px, ${pointerRef.current.y - grabOffsetRef.current.y}px) ${tilt()}`,
          }}
        >
          <div className={`relative flex items-center gap-3 ${listView ? 'h-11 pl-7 rounded-lg bg-[#1f1f26]' : 'h-16 pl-6 rounded-xl bg-gradient-to-br from-[#1f1f26] to-[#16161b]'} pr-2 border border-primary/50 ring-1 ring-primary/40 shadow-2xl shadow-black/60 overflow-hidden`}>
            <div className={`${listView ? 'w-6 h-6 rounded-md' : 'w-9 h-9 rounded-lg'} border flex items-center justify-center shrink-0`} style={avatarStyle(draggedFolder.color)}>
              <Folder size={listView ? 12 : 16} />
            </div>
            <h3 className="font-bold text-zinc-100 text-[13px] truncate tracking-tight leading-tight">{draggedFolder.name}</h3>
          </div>
        </div>,
        document.body
      )}

      {/* A card's AI level menu. Out here, not in the card: a click in it
          would bubble to the card and open the server. */}
      {aiMenu && (
        <LevelMenu
          key={aiMenu.uuid}
          serverUuid={aiMenu.uuid}
          serverName={aiMenu.name}
          current={aiLevels.get(aiMenu.uuid) ?? 'ask'}
          anchor={aiMenu.anchor}
          compact={!!isMobile}
          onClose={closeAiMenu}
        />
      )}
    </div>
  );
};
