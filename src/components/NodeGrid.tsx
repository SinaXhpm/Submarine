import { Search, Plus, Server, Globe, Folder, ChevronLeft, Trash2, Edit2, Zap, Check, X, Copy, GripVertical } from "lucide-react";
import { useState, useEffect, useLayoutEffect, useRef } from "react";
import { createPortal } from "react-dom";
import { useConfirm } from "../ui/confirm";

export const NodeGrid = ({ servers, folders, activeFolderId: activeFolderIdProp, onActiveFolderChange, onOpenServer, onEditServer, onAddClick, onQuickConnect, onRemoveServer, onRemoveFolder, onRenameFolder, onCloneServer, onReorderServers, isMobile }: any) => {
  const [search, setSearch] = useState("");
  // Native window.confirm() is a silent no-op inside Tauri's Android WebView
  // (same shim gap useTextPrompt was added for), so the delete buttons here
  // silently did nothing on Android after v0.2.32 made the action cluster
  // always-visible on mobile. Route through the themed useConfirm hook.
  const confirm = useConfirm();
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
    : filteredServers.filter((s: any) => s.folder_id === activeFolderId);

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
      if (rafRef.current != null) return; // coalesce the reflow recompute
      const px = e.clientX, py = e.clientY;
      rafRef.current = requestAnimationFrame(() => {
        rafRef.current = null;
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
            gesture from scrolling the list while dragging. */}
        {canReorder && (
          <button
            onPointerDown={(e) => beginDrag(e, s.id)}
            onClick={(e) => e.stopPropagation()}
            title="Drag to reorder"
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
          <h3 className="font-bold text-zinc-100 text-[13px] truncate tracking-tight leading-tight">{s.name}</h3>
          <div className="flex items-center gap-1 mt-0.5">
            <Globe size={10} className="text-zinc-400 shrink-0" />
            <span className="text-[10.5px] text-zinc-300 font-mono truncate">{s.host}</span>
          </div>
        </div>

        {/* Actions: absolute + hover-revealed on desktop so the name gets the
            full width; inline + always-visible on mobile (no hover there). */}
        <div className="flex items-center gap-0.5 shrink-0 opacity-100 sm:opacity-0 sm:group-hover:opacity-100 [@media(hover:none)]:!opacity-100 sm:absolute sm:right-1.5 sm:top-1/2 sm:-translate-y-1/2 sm:pl-8 sm:bg-gradient-to-l sm:from-[#121216] sm:via-[#121216] sm:to-transparent transition-all duration-200">
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
    const serverCount = servers?.filter((s: any) => s.folder_id === f.id).length || 0;
    const isRenaming = renaming?.id === f.id;
    const accent = f.color || 'rgb(var(--primary))';
    return (
      <div
        onClick={() => { if (!isRenaming) setActiveFolderId(f.id); }}
        className="group relative flex items-center gap-3 h-16 pl-3.5 pr-2 rounded-xl bg-gradient-to-br from-[#1c1c22] to-[#141418] border border-white/5 hover:border-primary/40 hover:bg-white/[0.015] transition-colors duration-150 cursor-pointer overflow-hidden"
      >
        <span aria-hidden className="absolute left-0 top-0 bottom-0 w-[3px]" style={{ background: accent }} />

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
              <h3 className="font-bold text-zinc-100 text-[13px] truncate tracking-tight leading-tight">{f.name}</h3>
              <span className="text-[10px] text-zinc-500 font-bold uppercase tracking-[0.12em]">
                {serverCount === 1 ? '1 server' : `${serverCount} servers`}
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
            <span className="text-[10px] bg-black/60 text-zinc-400 px-1.5 py-0.5 rounded-md font-mono group-hover:opacity-0 transition-opacity">{serverCount}</span>
            <div className="absolute right-0 flex items-center gap-0.5 opacity-100 sm:opacity-0 sm:group-hover:opacity-100 [@media(hover:none)]:!opacity-100 transition-all bg-gradient-to-l from-[#16161b] via-[#16161b] to-transparent pl-6">
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
                  if (await confirm({ title: 'Delete folder?', message: `“${f.name}” and every server inside it will be removed.`, destructive: true })) {
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

      <div className="flex-1 overflow-y-auto custom-scrollbar pr-1 pb-10">

        {/* Header navigation if in folder */}
        {!isSearching && activeFolderId !== null && (
          <div className="flex items-center gap-3 mb-6">
            <button
              onClick={() => setActiveFolderId(null)}
              className="w-8 h-8 bg-black rounded-xl border border-white/5 flex items-center justify-center hover:bg-white/10 hover:text-white text-zinc-500 transition-all shadow-inner"
            >
              <ChevronLeft size={16} />
            </button>
            <h2 className="text-[15px] font-bold text-primary tracking-tight flex items-center gap-2 flex-1">
              <Folder size={16} /> {currentFolder?.name}
            </h2>
            {/* Adding a server from inside a folder pre-selects this folder
                in the new-node panel, so the user doesn't have to repeat
                the choice they just clicked into. */}
            <button
              onClick={() => onAddClick(activeFolderId)}
              className="h-8 px-3 bg-primary/15 text-primary border border-primary/30 hover:bg-primary/25 rounded-lg flex items-center gap-1.5 text-[11px] font-bold uppercase tracking-wider"
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

            {/* Render Folders (only in root, and when not searching) */}
            {!isSearching && activeFolderId === null && folders?.map((f: any) => (
              <FolderCard key={`folder-${f.id}`} f={f} />
            ))}

            {/* Render Servers — honours the live drag order while reordering. */}
            {orderedServers.map((s: any) => renderServer(s))}

            {/* Empty Folder State */}
            {!isSearching && activeFolderId !== null && displayedServers.length === 0 && (
              <div className="col-span-full py-10 text-center text-zinc-600 text-xs italic">
                Nothing in this folder yet.
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
    </div>
  );
};
