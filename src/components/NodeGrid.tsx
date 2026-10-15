import { Search, Plus, Server, Globe, Folder, FolderPlus, ChevronLeft, ChevronRight, Trash2, Edit2, Zap, Check, X, Copy, GripVertical, ShieldAlert, KeyRound } from "lucide-react";
import { useState, useEffect, useLayoutEffect, useRef, useCallback } from "react";
import { createPortal } from "react-dom";
import { useConfirm } from "../ui/confirm";
import { HeldReview } from "./HeldReview";
import { HeldDeletions } from "./HeldDeletions";
import { folderTree } from "../util/folderTree";

export const NodeGrid = ({ servers, folders, activeFolderId: activeFolderIdProp, onActiveFolderChange, onOpenServer, onEditServer, onAddClick, onQuickConnect, onRemoveServer, onRemoveFolder, onRenameFolder, onCreateFolder, onMoveServer, onMoveFolder, onCloneServer, onReorderServers, onAllowHeld, keyChange, onAcceptKey, heldDeletions, onResolveDeletions, isMobile }: any) => {
  const [search, setSearch] = useState("");
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
  // the focus, so a stray Enter doesn't accept it.
  const reviewKey = async () => {
    if (!keyChange || !onAcceptKey) return;
    const ok = await confirm({
      title: "Use the new key?",
      message:
        `This shared profile's key was replaced, usually because its owner removed someone. The new key's fingerprint is ${keyChange}. ` +
        "Ask the owner whether their Profile panel shows the same one. If it doesn't, the key didn't come from them: don't use it.",
      okLabel: "Use the new key",
      cancelLabel: "Not now",
      safeDefault: true,
    });
    if (ok) onAcceptKey();
  };
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
  const tree = folderTree(folders);
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

  const moveClone = (x: number, y: number) => {
    const el = cloneRef.current;
    if (el) el.style.transform = `translate(${x - grabOffsetRef.current.x}px, ${y - grabOffsetRef.current.y}px) rotate(1.5deg) scale(1.03)`;
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
        let idx = 0;
        for (const c of slotsRef.current) {
          const cy = c.y - sd;
          const before = (cy < py - c.h / 2) || (Math.abs(cy - py) <= c.h / 2 && c.x < px);
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
      if (el) el.style.transform = `translate(${e.clientX - grabOffsetRef.current.x}px, ${e.clientY - grabOffsetRef.current.y}px) rotate(1.5deg) scale(1.03)`;
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

  // Compact horizontal node card: a monogram avatar carries identity, the
  // server projects its own colour as a left accent bar. Kept short (h-16) so
  // name + host read clearly. Rendered as a plain keyed <div> (not a nested
  // component) so React reuses the same DOM node across renders — that's what
  // lets the FLIP effect measure and animate real positions. While this card is
  // the one being dragged it renders as a dashed placeholder holding the slot;
  // the floating clone (portal, below) is what the pointer carries.
  const renderServer = (s: any) => {
    const accent = s.color || 'rgb(var(--primary))';
    const monogram = (s.name || '').trim().charAt(0).toUpperCase();
    if (dragId === s.id) {
      return (
        <div
          key={`server-${s.id}`}
          data-sid={s.id}
          aria-hidden
          className="h-16 rounded-xl border-2 border-dashed border-primary/40 bg-primary/[0.05]"
        />
      );
    }
    return (
      <div
        key={`server-${s.id}`}
        data-sid={s.id}
        onClick={() => { if (draggedRef.current) return; onOpenServer(s); }}
        className="group relative flex items-center gap-3 h-16 pl-3.5 pr-2 rounded-xl bg-gradient-to-br from-[#17171c] to-[#121216] border border-white/5 hover:border-primary/40 hover:bg-white/[0.015] transition-colors duration-150 cursor-pointer overflow-hidden"
      >
        {/* Per-server colour accent: a crisp left bar. */}
        <span aria-hidden className="absolute left-0 top-0 bottom-0 w-[3px]" style={{ background: accent }} />

        {/* Drag handle (grip) — grab to reorder. Hidden until hover on desktop,
            always shown on touch (no hover there). touch-action:none stops the
            gesture from scrolling the list while dragging. Pointer only, so
            not a Tab stop: from the keyboard it would do nothing. */}
        {canReorder && (
          <button
            tabIndex={-1}
            onPointerDown={(e) => beginDrag(e, s.id)}
            onClick={(e) => e.stopPropagation()}
            title={onMoveServer ? "Drag to reorder, or onto a folder" : "Drag to reorder"}
            className="shrink-0 -ml-1.5 -mr-1 p-1 rounded-md text-zinc-600 hover:text-zinc-300 cursor-grab active:cursor-grabbing opacity-100 sm:opacity-0 sm:group-hover:opacity-100 [@media(hover:none)]:!opacity-100 transition-opacity"
            style={{ touchAction: 'none' }}
          >
            <GripVertical size={14} />
          </button>
        )}

        {/* Monogram avatar. */}
        <div className="w-9 h-9 rounded-lg bg-gradient-to-br from-primary/25 to-primary/[0.03] border border-primary/20 flex items-center justify-center text-primary shadow-inner shrink-0">
          {monogram
            ? <span className="font-black text-[14px] leading-none">{monogram}</span>
            : <Server size={14} />}
        </div>

        {/* Name + host — flex-1 + min-w-0 so long values truncate cleanly. */}
        <div className="min-w-0 flex-1">
          {/* A button so the keyboard can open it too: its click bubbles to
              the card's own handler. */}
          <h3 className="font-bold text-zinc-100 text-[13px] truncate tracking-tight leading-tight">
            <button type="button" className="max-w-full truncate text-left rounded outline-none focus-visible:ring-2 focus-visible:ring-primary/60">{s.name}</button>
          </h3>
          <div className="flex items-center gap-1 mt-0.5">
            {s.protocol === 'rdp'
              ? <span title="RDP server" className="shrink-0 px-1 rounded text-[9px] font-bold tracking-wider leading-[14px] text-sky-300 bg-sky-500/10 border border-sky-500/20">RDP</span>
              : <Globe size={10} className="text-zinc-400 shrink-0" />}
            <span className="text-[10.5px] text-zinc-300 font-mono truncate">{s.host}</span>
          </div>
        </div>

        {/* Actions: absolute + hover-revealed on desktop so the name gets the
            full width; inline + always-visible on mobile (no hover there). */}
        <div className="flex items-center gap-0.5 shrink-0 opacity-100 sm:opacity-0 sm:group-hover:opacity-100 sm:group-has-[:focus-visible]:opacity-100 [@media(hover:none)]:!opacity-100 sm:absolute sm:right-1.5 sm:top-1/2 sm:-translate-y-1/2 sm:pl-8 sm:bg-gradient-to-l sm:from-[#121216] sm:via-[#121216] sm:to-transparent transition-all duration-200">
          {onCloneServer && (
            <button
              onClick={(e) => { e.stopPropagation(); onCloneServer(s.id); }}
              title="Clone"
              className="p-1.5 rounded-lg text-zinc-500 hover:text-primary hover:bg-white/5 transition-all"
            >
              <Copy size={13} />
            </button>
          )}
          <button
            onClick={(e) => { e.stopPropagation(); onEditServer(s); }}
            title="Edit"
            className="p-1.5 rounded-lg text-zinc-500 hover:text-white hover:bg-white/5 transition-all"
          >
            <Edit2 size={13} />
          </button>
          <button
            onClick={async (e) => {
              e.stopPropagation();
              if (await confirm({ title: 'Delete server?', message: `“${s.name}” will be removed from this profile.`, destructive: true })) {
                onRemoveServer(s.id);
              }
            }}
            title="Delete"
            className="p-1.5 rounded-lg text-zinc-500 hover:text-red-500 hover:bg-white/5 transition-all"
          >
            <Trash2 size={13} />
          </button>
        </div>
      </div>
    );
  };

  // Folders share the compact horizontal language so the grid reads as one
  // surface: a tinted folder glyph, name + server count, inline rename in
  // place, and the same hover lift as server cards.
  const FolderCard = ({ f }: { f: any }) => {
    const serverCount = servers?.filter((s: any) => tree.serverFolder(s) === f.id).length || 0;
    const folderCount = tree.childrenOf(f.id).length;
    const isRenaming = renaming?.id === f.id;
    const accent = f.color || 'rgb(var(--primary))';
    const isTarget = dropTarget === f.id;
    const isDragged = folderDragId === f.id;
    const contents = [
      folderCount > 0 ? (folderCount === 1 ? '1 folder' : `${folderCount} folders`) : null,
      serverCount === 1 ? '1 server' : `${serverCount} servers`,
    ].filter(Boolean).join(' · ');
    return (
      <div
        data-drop-folder={f.id}
        onClick={() => { if (!isRenaming && !draggedRef.current) setActiveFolderId(f.id); }}
        className={`group relative flex items-center gap-3 h-16 pl-3.5 pr-2 rounded-xl bg-gradient-to-br from-[#1c1c22] to-[#141418] border transition-colors duration-150 cursor-pointer overflow-hidden ${
          isTarget ? 'border-primary ring-2 ring-primary/60 bg-primary/10' : 'border-white/5 hover:border-primary/40 hover:bg-white/[0.015]'
        } ${isDragged ? 'opacity-40' : ''}`}
      >
        <span aria-hidden className="absolute left-0 top-0 bottom-0 w-[3px]" style={{ background: accent }} />

        {/* Drag handle: drop the folder into another one, or up the path. */}
        {canMoveFolders && !isRenaming && (
          <button
            tabIndex={-1}
            onPointerDown={(e) => beginFolderDrag(e, f.id)}
            onClick={(e) => e.stopPropagation()}
            title="Drag into another folder"
            className="shrink-0 -ml-1.5 -mr-1 p-1 rounded-md text-zinc-600 hover:text-zinc-300 cursor-grab active:cursor-grabbing opacity-100 sm:opacity-0 sm:group-hover:opacity-100 [@media(hover:none)]:!opacity-100 transition-opacity"
            style={{ touchAction: 'none' }}
          >
            <GripVertical size={14} />
          </button>
        )}

        {/* Folder glyph avatar. */}
        <div
          className="w-9 h-9 rounded-lg bg-black/40 border border-white/10 flex items-center justify-center shadow-inner shrink-0"
          style={{ color: f.color || undefined }}
        >
          <Folder size={16} className={f.color ? '' : 'text-primary'} />
        </div>

        {/* Name (inline-editable) + count. */}
        <div className="min-w-0 flex-1">
          {isRenaming ? (
            <input
              autoFocus
              value={renaming!.draft}
              onChange={(e) => setRenaming({ id: f.id, draft: e.target.value })}
              onClick={(e) => e.stopPropagation()}
              onKeyDown={(e) => {
                if (e.key === "Enter") { e.preventDefault(); commitRename(); }
                if (e.key === "Escape") { e.preventDefault(); setRenaming(null); }
              }}
              className="w-full h-7 px-2 bg-black/40 border border-primary/40 rounded-lg text-[13px] font-bold text-white outline-none"
            />
          ) : (
            <>
              <h3 className="font-bold text-zinc-100 text-[13px] truncate tracking-tight leading-tight">
                <button type="button" className="max-w-full truncate text-left rounded outline-none focus-visible:ring-2 focus-visible:ring-primary/60">{f.name}</button>
              </h3>
              <span className="text-[10px] text-zinc-500 font-bold uppercase tracking-[0.12em]">
                {contents}
              </span>
            </>
          )}
        </div>

        {/* Right cluster: count badge (idle) that swaps to actions on hover. */}
        {isRenaming ? (
          <div className="flex items-center gap-0.5 shrink-0">
            <button
              onClick={(e) => { e.stopPropagation(); commitRename(); }}
              title="Save name"
              className="p-1.5 text-emerald-400 hover:bg-white/10 rounded-lg"
            >
              <Check size={14} />
            </button>
            <button
              onClick={(e) => { e.stopPropagation(); setRenaming(null); }}
              title="Cancel"
              className="p-1.5 text-zinc-400 hover:text-rose-400 hover:bg-white/10 rounded-lg"
            >
              <X size={14} />
            </button>
          </div>
        ) : (
          <div className="relative flex items-center shrink-0">
            <span className="text-[10px] bg-black/60 text-zinc-400 px-1.5 py-0.5 rounded-md font-mono group-hover:opacity-0 group-has-[:focus-visible]:opacity-0 transition-opacity">{serverCount}</span>
            <div className="absolute right-0 flex items-center gap-0.5 opacity-100 sm:opacity-0 sm:group-hover:opacity-100 sm:group-has-[:focus-visible]:opacity-100 [@media(hover:none)]:!opacity-100 transition-all bg-gradient-to-l from-[#16161b] via-[#16161b] to-transparent pl-6">
              <button
                onClick={(e) => { e.stopPropagation(); setRenaming({ id: f.id, draft: f.name || "" }); }}
                title="Rename folder"
                className="p-1.5 rounded-lg text-zinc-500 hover:text-primary hover:bg-white/5 transition-all"
              >
                <Edit2 size={13} />
              </button>
              <button
                onClick={async (e) => {
                  e.stopPropagation();
                  // Everything below goes too: say how much.
                  const inside = tree.subtreeOf(f.id);
                  const subfolders = inside.size - 1;
                  const allServers = servers?.filter((s: any) => inside.has(tree.serverFolder(s) as number)).length || 0;
                  const what = [
                    subfolders > 0 ? (subfolders === 1 ? '1 folder' : `${subfolders} folders`) : null,
                    allServers === 1 ? '1 server' : `${allServers} servers`,
                  ].filter(Boolean).join(' and ');
                  const message = subfolders > 0
                    ? `“${f.name}” and everything inside it (${what}) will be removed.`
                    : `“${f.name}” and every server inside it (${what}) will be removed.`;
                  if (await confirm({ title: 'Delete folder?', message, destructive: true })) {
                    onRemoveFolder(f.id);
                  }
                }}
                title="Delete folder"
                className="p-1.5 rounded-lg text-zinc-500 hover:text-red-500 hover:bg-white/5 transition-all"
              >
                <Trash2 size={13} />
              </button>
            </div>
          </div>
        )}
      </div>
    );
  };

  return (
    <div className="flex-1 flex flex-col h-full p-6 overflow-hidden bg-transparent">
      <div className="relative max-w-sm w-full mx-auto mb-6 shrink-0">
        <Search className="absolute left-3 top-1/2 -translate-y-1/2 text-zinc-400" size={14} />
        <input
          type="text"
          placeholder="Search servers…"
          className="w-full h-9 bg-[#1c1c21] border border-white/10 rounded-lg pl-9 pr-4 text-[13px] text-zinc-100 outline-none focus:border-primary/50 focus:bg-[#16161a] transition-all placeholder:text-zinc-600 shadow-inner"
          value={search}
          onChange={(e) => setSearch(e.target.value)}
        />
      </div>

      {heldCount > 0 && onAllowHeld && (
        <div role="status" className="max-w-xl w-full mx-auto -mt-3 mb-5 shrink-0 flex items-center gap-3 rounded-lg border border-amber-400/25 bg-amber-400/[0.06] px-3 py-2 text-[12px] text-amber-100/90">
          <ShieldAlert size={14} className="text-amber-400 shrink-0" />
          <span className="flex-1 min-w-0">
            {heldCount === 1
              ? "A change from someone you share this profile with is waiting for your OK."
              : `${heldCount} changes from people you share this profile with are waiting for your OK.`}
          </span>
          <button
            onClick={() => setReviewOpen(true)}
            className="shrink-0 px-2.5 h-7 rounded text-[10.5px] font-bold uppercase tracking-wider bg-amber-400/10 border border-amber-400/30 text-amber-200 hover:bg-amber-400/20"
          >
            Review
          </button>
        </div>
      )}
      {reviewOpen && onAllowHeld && (
        <HeldReview servers={servers || []} onAllow={onAllowHeld} onClose={closeReview} />
      )}
      {deletionCount > 0 && onResolveDeletions && (
        <div role="status" className="max-w-xl w-full mx-auto -mt-3 mb-5 shrink-0 flex items-center gap-3 rounded-lg border border-amber-400/25 bg-amber-400/[0.06] px-3 py-2 text-[12px] text-amber-100/90">
          <Trash2 size={14} className="text-amber-400 shrink-0" />
          <span className="flex-1 min-w-0">
            {deletionCount === 1 ? "An item was" : `${deletionCount} items were`} deleted by someone you share this
            profile with. {deletionCount === 1 ? "It stays" : "They stay"} here until you decide.
          </span>
          <button
            onClick={() => setDeletionsOpen(true)}
            className="shrink-0 px-2.5 h-7 rounded text-[10.5px] font-bold uppercase tracking-wider bg-amber-400/10 border border-amber-400/30 text-amber-200 hover:bg-amber-400/20"
          >
            Review
          </button>
        </div>
      )}
      {deletionsOpen && onResolveDeletions && (
        <HeldDeletions items={heldDeletions || []} onResolve={onResolveDeletions} onClose={closeDeletions} />
      )}
      {keyChange && onAcceptKey && (
        <div role="status" className="max-w-xl w-full mx-auto -mt-3 mb-5 shrink-0 flex items-center gap-3 rounded-lg border border-amber-400/25 bg-amber-400/[0.06] px-3 py-2 text-[12px] text-amber-100/90">
          <KeyRound size={14} className="text-amber-400 shrink-0" />
          <span className="flex-1 min-w-0">
            This shared profile's key has changed. Your changes aren't uploaded until you accept the new key.
          </span>
          <button
            onClick={reviewKey}
            className="shrink-0 px-2.5 h-7 rounded text-[10.5px] font-bold uppercase tracking-wider bg-amber-400/10 border border-amber-400/30 text-amber-200 hover:bg-amber-400/20"
          >
            Review
          </button>
        </div>
      )}

      <div className="flex-1 overflow-y-auto custom-scrollbar pr-1 pb-10">

        {/* Header navigation if in folder */}
        {!isSearching && activeFolderId !== null && (
          <div className="flex items-center gap-3 mb-6 flex-wrap">
            <button aria-label="Back"
              title="Up one level"
              onClick={() => setActiveFolderId(tree.parentOf(activeFolderId))}
              className="w-8 h-8 bg-black rounded-xl border border-white/5 flex items-center justify-center hover:bg-white/10 hover:text-white text-zinc-500 transition-all shadow-inner shrink-0"
            >
              <ChevronLeft size={16} />
            </button>
            {/* The path down to this folder. Each step above it opens that
                level, and takes a server or folder dropped on it. */}
            {/* At least 12rem wide: on a narrow window the buttons wrap below
                it rather than squeezing the path to nothing. */}
            <nav aria-label="Folder path" className="flex items-center gap-1 flex-1 min-w-[12rem] flex-wrap text-[13px]">
              {[{ id: "root", name: "All servers" }, ...tree.pathOf(activeFolderId)].map((step: any, i: number, all: any[]) => {
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
            {onCreateFolder && (
              <button
                onClick={() => setNewFolder((v) => v ?? "")}
                title="Make a folder inside this one"
                className="h-8 px-3 bg-primary/10 text-primary border border-primary/25 hover:bg-primary/20 rounded-lg flex items-center gap-1.5 text-[11px] font-bold uppercase tracking-wider shrink-0"
              >
                <FolderPlus size={13} /> Add folder
              </button>
            )}
            {/* Adding a server from inside a folder pre-selects this folder
                in the new-node panel, so the user doesn't have to repeat
                the choice they just clicked into. */}
            <button
              onClick={() => onAddClick(activeFolderId)}
              className="h-8 px-3 bg-primary/15 text-primary border border-primary/30 hover:bg-primary/25 rounded-lg flex items-center gap-1.5 text-[11px] font-bold uppercase tracking-wider shrink-0"
            >
              <Plus size={13} /> Add server
            </button>
          </div>
        )}

        {servers?.length === 0 && folders?.length === 0 ? (
          <div className="h-full flex items-center justify-center">
            <button
              onClick={onAddClick}
              className="flex flex-col items-center gap-4 p-12 rounded-3xl border-2 border-dashed border-zinc-800 hover:border-primary/40 hover:bg-primary/5 transition-all group"
            >
              <div className="w-14 h-14 bg-primary/10 rounded-2xl flex items-center justify-center text-primary shadow-[0_0_15px_rgba(var(--primary),0.1)] transition-transform">
                <Plus size={28} />
              </div>
              <div className="text-center">
                <h3 className="text-[15px] font-bold text-white tracking-tight">Add your first server</h3>
                <p className="text-[12px] text-zinc-500 mt-1">Tap here to get started.</p>
              </div>
            </button>
          </div>
        ) : (
          <>
            {/* Compact action toolbar (root only) — sized to its own content
                rather than filling a full grid cell like the cards. */}
            {(!isSearching && activeFolderId === null) && (
              <div className="flex items-center gap-2 mb-4 flex-wrap">
                <button
                  onClick={onAddClick}
                  className="inline-flex items-center gap-1.5 h-8 px-3 rounded-lg bg-primary/10 border border-primary/25 text-primary text-[11px] font-bold uppercase tracking-wider hover:bg-primary/20 transition-colors"
                >
                  <Plus size={13} /> Add server
                </button>
                {onCreateFolder && (
                  <button
                    onClick={() => setNewFolder((v) => v ?? "")}
                    title="Group servers in a folder"
                    className="inline-flex items-center gap-1.5 h-8 px-3 rounded-lg bg-primary/10 border border-primary/25 text-primary text-[11px] font-bold uppercase tracking-wider hover:bg-primary/20 transition-colors"
                  >
                    <FolderPlus size={13} /> Add folder
                  </button>
                )}
                {onQuickConnect && (
                  <button
                    onClick={onQuickConnect}
                    className="inline-flex items-center gap-1.5 h-8 px-3 rounded-lg bg-amber-500/10 border border-amber-500/25 text-amber-300/90 text-[11px] font-bold uppercase tracking-wider hover:bg-amber-500/20 hover:text-amber-200 transition-colors"
                  >
                    <Zap size={13} /> Quick connect
                  </button>
                )}
              </div>
            )}

          <div ref={gridRef} className={`grid ${isMobile ? 'grid-cols-1' : 'grid-cols-2 lg:grid-cols-3 xl:grid-cols-4'} gap-3`}>

            {/* The folder being made with "Add folder": a card to name it.
                Plain JSX, not a nested component, so typing keeps focus. */}
            {!isSearching && newFolder !== null && (
              <div className="relative flex items-center gap-3 h-16 pl-3.5 pr-2 rounded-xl bg-gradient-to-br from-[#1c1c22] to-[#141418] border border-primary/40 overflow-hidden">
                <span aria-hidden className="absolute left-0 top-0 bottom-0 w-[3px] bg-primary" />
                <div className="w-9 h-9 rounded-lg bg-black/40 border border-white/10 flex items-center justify-center shadow-inner shrink-0">
                  <FolderPlus size={16} className="text-primary" />
                </div>
                <input
                  autoFocus
                  aria-label="New folder name"
                  placeholder="Folder name…"
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
                    title="Create folder"
                    aria-label="Create folder"
                    className="p-1.5 text-emerald-400 hover:bg-white/10 rounded-lg disabled:opacity-40 disabled:hover:bg-transparent"
                  >
                    <Check size={14} />
                  </button>
                  <button
                    onClick={() => setNewFolder(null)}
                    title="Cancel"
                    aria-label="Cancel new folder"
                    className="p-1.5 text-zinc-400 hover:text-rose-400 hover:bg-white/10 rounded-lg"
                  >
                    <X size={14} />
                  </button>
                </div>
              </div>
            )}

            {/* The folders inside the open one (or the top level); none while searching. */}
            {displayedFolders.map((f: any) => (
              <FolderCard key={`folder-${f.id}`} f={f} />
            ))}

            {/* Render Servers — honours the live drag order while reordering. */}
            {orderedServers.map((s: any) => renderServer(s))}

            {/* Empty Folder State */}
            {!isSearching && activeFolderId !== null && displayedServers.length === 0 && displayedFolders.length === 0 && newFolder === null && (
              <div className="col-span-full py-10 text-center text-zinc-600 text-xs italic">
                Nothing in this folder yet. Add a server or folder here, or drag a server onto this folder from the level above.
              </div>
            )}

            {/* Empty Search State */}
            {isSearching && displayedServers.length === 0 && (
              <div className="col-span-full py-10 text-center text-zinc-600 text-xs italic">
                No matches.
              </div>
            )}

          </div>
          </>
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
            transform: `translate(${pointerRef.current.x - grabOffsetRef.current.x}px, ${pointerRef.current.y - grabOffsetRef.current.y}px) rotate(1.5deg) scale(1.03)`,
          }}
        >
          <div className="relative flex items-center gap-3 h-16 pl-3.5 pr-2 rounded-xl bg-gradient-to-br from-[#1b1b20] to-[#141418] border border-primary/50 ring-1 ring-primary/40 shadow-2xl shadow-black/60 overflow-hidden">
            <span aria-hidden className="absolute left-0 top-0 bottom-0 w-[3px]" style={{ background: draggedServer.color || 'rgb(var(--primary))' }} />
            <div className="w-9 h-9 rounded-lg bg-gradient-to-br from-primary/25 to-primary/[0.03] border border-primary/20 flex items-center justify-center text-primary shadow-inner shrink-0">
              {(draggedServer.name || '').trim().charAt(0)
                ? <span className="font-black text-[14px] leading-none">{(draggedServer.name || '').trim().charAt(0).toUpperCase()}</span>
                : <Server size={14} />}
            </div>
            <div className="min-w-0 flex-1">
              <h3 className="font-bold text-zinc-100 text-[13px] truncate tracking-tight leading-tight">{draggedServer.name}</h3>
              <div className="flex items-center gap-1 mt-0.5">
                <Globe size={10} className="text-zinc-400 shrink-0" />
                <span className="text-[10.5px] text-zinc-300 font-mono truncate">{draggedServer.host}</span>
              </div>
            </div>
          </div>
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
            transform: `translate(${pointerRef.current.x - grabOffsetRef.current.x}px, ${pointerRef.current.y - grabOffsetRef.current.y}px) rotate(1.5deg) scale(1.03)`,
          }}
        >
          <div className="relative flex items-center gap-3 h-16 pl-3.5 pr-2 rounded-xl bg-gradient-to-br from-[#1f1f26] to-[#16161b] border border-primary/50 ring-1 ring-primary/40 shadow-2xl shadow-black/60 overflow-hidden">
            <span aria-hidden className="absolute left-0 top-0 bottom-0 w-[3px]" style={{ background: draggedFolder.color || 'rgb(var(--primary))' }} />
            <div className="w-9 h-9 rounded-lg bg-black/40 border border-white/10 flex items-center justify-center shadow-inner shrink-0" style={{ color: draggedFolder.color || undefined }}>
              <Folder size={16} className={draggedFolder.color ? '' : 'text-primary'} />
            </div>
            <h3 className="font-bold text-zinc-100 text-[13px] truncate tracking-tight leading-tight">{draggedFolder.name}</h3>
          </div>
        </div>,
        document.body
      )}
    </div>
  );
};
