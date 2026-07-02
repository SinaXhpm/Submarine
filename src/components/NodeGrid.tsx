import { Search, Plus, Server, Globe, Folder, ChevronLeft, Trash2, Edit2, Zap, Check, X, Copy } from "lucide-react";
import { useState } from "react";
import { useConfirm } from "../ui/confirm";

export const NodeGrid = ({ servers, folders, activeFolderId: activeFolderIdProp, onActiveFolderChange, onOpenServer, onEditServer, onAddClick, onQuickConnect, onRemoveServer, onRemoveFolder, onRenameFolder, onCloneServer, isMobile }: any) => {
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
  const ServerCard = ({ s }: { s: any }) => (
    <div
      key={s.id}
      onClick={() => onOpenServer(s)}
      className="relative p-3 h-14 bg-[#16161a] border border-white/5 hover:border-primary/50 hover:shadow-[0_8px_20px_rgba(var(--primary),0.1)] rounded-xl transition-all cursor-pointer group flex items-center justify-between overflow-hidden"
    >
      {s.color && (
        <span
          className="absolute left-0 top-0 bottom-0 w-1 rounded-l-xl"
          style={{ backgroundColor: s.color }}
        />
      )}
      {/* Left cluster gets flex-1 + min-w-0 so long server names use
          every pixel not spent on the icon or the (mobile-only) action
          buttons. On desktop the actions overlay the tile from the
          right edge — they're absolute + hover-revealed — so the name
          gets the FULL row width to truncate against. */}
      <div className="flex items-center gap-3 min-w-0 flex-1">
        <div className="w-8 h-8 bg-[#0a0a0c] rounded-lg flex items-center justify-center text-zinc-500 group-hover:text-primary transition-colors shadow-inner border border-white/5 group-hover:border-primary/20 shrink-0">
          <Server size={14} />
        </div>
        <div className="min-w-0 flex-1">
          <h3 className="font-bold text-zinc-100 text-[14px] truncate tracking-tight">{s.name}</h3>
          <div className="flex items-center gap-1 mt-0.5">
            <Globe size={10} className="text-zinc-500 shrink-0" />
            <span className="text-[10.5px] text-zinc-500 font-mono truncate group-hover:text-zinc-400 transition-colors">{s.host}</span>
          </div>
        </div>
      </div>
      {/* Action cluster: absolute-positioned on desktop (so it doesn't
          claim layout width even when hidden) with a soft gradient
          behind on hover so hover-revealed buttons don't visually
          clash with whatever text they overlap. On mobile the buttons
          stay inline as a fallback since there's no hover to reveal. */}
      <div className="flex items-center gap-1 opacity-100 sm:opacity-0 sm:group-hover:opacity-100 sm:absolute sm:right-2 sm:top-1/2 sm:-translate-y-1/2 sm:pl-8 sm:bg-gradient-to-l sm:from-[#16161a] sm:via-[#16161a] sm:to-transparent transition-all">
        {onCloneServer && (
          <button
            onClick={(e) => { e.stopPropagation(); onCloneServer(s.id); }}
            title="Clone"
            className="p-2 text-zinc-500 hover:text-primary transition-all"
          >
            <Copy size={14} />
          </button>
        )}
        <button
          onClick={(e) => { e.stopPropagation(); onEditServer(s); }}
          className="p-2 text-zinc-500 hover:text-white transition-all"
        >
          <Edit2 size={14} />
        </button>
        <button
          onClick={async (e) => {
            e.stopPropagation();
            if (await confirm({ title: 'Delete server?', message: `“${s.name}” will be removed from this profile.`, destructive: true })) {
              onRemoveServer(s.id);
            }
          }}
          className="p-2 text-zinc-500 hover:text-red-500 transition-all"
        >
          <Trash2 size={14} />
        </button>
      </div>
    </div>
  );

  const FolderCard = ({ f }: { f: any }) => {
    const serverCount = servers?.filter((s: any) => s.folder_id === f.id).length || 0;
    const isRenaming = renaming?.id === f.id;
    return (
      <div
        key={f.id}
        onClick={() => { if (!isRenaming) setActiveFolderId(f.id); }}
        className="relative p-3 h-14 bg-[#1c1c21] rounded-xl border border-white/5 hover:border-primary/50 hover:bg-white/5 transition-all cursor-pointer group flex items-center justify-between shadow-inner overflow-hidden"
      >
        {f.color && (
          <span
            className="absolute left-0 top-0 bottom-0 w-1 rounded-l-xl"
            style={{ backgroundColor: f.color }}
          />
        )}
        <div className="flex items-center gap-3 min-w-0 flex-1">
          <Folder size={18} className="shrink-0" style={{ color: f.color || undefined }} />
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
              className="flex-1 h-7 px-2 bg-black/40 border border-primary/40 rounded text-[13px] font-bold text-white outline-none"
            />
          ) : (
            <h3 className="font-bold text-zinc-200 text-[13px] truncate tracking-tight">{f.name}</h3>
          )}
        </div>
        <div className="flex items-center gap-1 relative shrink-0">
          {isRenaming ? (
            <>
              <button
                onClick={(e) => { e.stopPropagation(); commitRename(); }}
                title="Save name"
                className="p-1.5 text-emerald-400 hover:bg-white/10 rounded"
              >
                <Check size={14} />
              </button>
              <button
                onClick={(e) => { e.stopPropagation(); setRenaming(null); }}
                title="Cancel"
                className="p-1.5 text-zinc-400 hover:text-rose-400 hover:bg-white/10 rounded"
              >
                <X size={14} />
              </button>
            </>
          ) : (
            <>
              <span className="text-[10px] bg-black text-zinc-500 px-1.5 py-0.5 rounded-md font-mono group-hover:opacity-0 transition-opacity">{serverCount}</span>
              <div className="absolute right-0 flex items-center gap-1 opacity-100 sm:opacity-0 sm:group-hover:opacity-100 transition-all bg-[#1c1c21]/95 backdrop-blur-sm rounded">
                <button
                  onClick={(e) => { e.stopPropagation(); setRenaming({ id: f.id, draft: f.name || "" }); }}
                  title="Rename folder"
                  className="p-1.5 text-zinc-500 hover:text-primary transition-all"
                >
                  <Edit2 size={14} />
                </button>
                <button
                  onClick={async (e) => {
                    e.stopPropagation();
                    if (await confirm({ title: 'Delete folder?', message: `“${f.name}” and every server inside it will be removed.`, destructive: true })) {
                      onRemoveFolder(f.id);
                    }
                  }}
                  title="Delete folder"
                  className="p-1.5 text-zinc-500 hover:text-red-500 transition-all"
                >
                  <Trash2 size={14} />
                </button>
              </div>
            </>
          )}
        </div>
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
          <div className={`grid ${isMobile ? 'grid-cols-1' : 'grid-cols-2 md:grid-cols-3 lg:grid-cols-4 xl:grid-cols-5'} gap-3`}>

            {/* Show Add Node + Quick Connect only in Root or when searching */}
            {(!isSearching && activeFolderId === null) && (
              <>
                <button
                  onClick={onAddClick}
                  className="flex items-center gap-3 p-3 h-14 bg-[#16161a] border border-dashed border-white/10 rounded-xl hover:bg-primary/5 hover:border-primary/40 transition-all group shadow-inner"
                >
                  <div className="w-8 h-8 bg-primary/5 rounded-lg flex items-center justify-center text-zinc-500 group-hover:text-primary transition-colors shrink-0">
                    <Plus size={16} />
                  </div>
                  <span className="text-[12px] font-bold text-zinc-400 group-hover:text-primary tracking-tight transition-colors">Add server</span>
                </button>
                {onQuickConnect && (
                  <button
                    onClick={onQuickConnect}
                    className="flex items-center gap-3 p-3 h-14 bg-[#16161a] border border-dashed border-amber-500/25 rounded-xl hover:bg-amber-500/5 hover:border-amber-500/50 transition-all group shadow-inner"
                  >
                    <div className="w-8 h-8 bg-amber-500/10 rounded-lg flex items-center justify-center text-amber-300/80 group-hover:text-amber-200 transition-colors shrink-0">
                      <Zap size={15} />
                    </div>
                    <span className="text-[12px] font-bold text-amber-300/90 group-hover:text-amber-100 tracking-tight transition-colors">Quick connect</span>
                  </button>
                )}
              </>
            )}

            {/* Render Folders (only in root, and when not searching) */}
            {!isSearching && activeFolderId === null && folders?.map((f: any) => (
              <FolderCard key={`folder-${f.id}`} f={f} />
            ))}

            {/* Render Servers */}
            {displayedServers.map((s: any) => <ServerCard key={`server-${s.id}`} s={s} />)}

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
        )}
      </div>
    </div>
  );
};
