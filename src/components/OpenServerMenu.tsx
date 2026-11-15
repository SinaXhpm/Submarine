// The + after the last tab: opens a saved server from anywhere, without going
// back to the Servers page. Type to narrow the list; Enter opens the first
// match, or the one picked with the arrow keys. Quick connect is at the foot.
import { useEffect, useLayoutEffect, useMemo, useRef, useState, type KeyboardEvent } from "react";
import { createPortal } from "react-dom";
import { Search, Zap } from "lucide-react";
import { useT } from "../i18n";
import { folderTree } from "../util/folderTree";
import { PROTOCOL_ICON, TAB_DOT, avatarStyle, protocolOf } from "../util/serverLook";

export function OpenServerMenu({ servers, folders, serverTabs, anchor, onOpen, onQuickConnect, onClose }: {
  servers: any[];
  folders: any[];
  // Each saved server's open tab, for the dot on its avatar.
  serverTabs: Record<number, string>;
  // The + button: the menu opens under it.
  anchor: HTMLElement;
  onOpen: (server: any) => void;
  onQuickConnect?: () => void;
  onClose: () => void;
}) {
  const t = useT();
  const [query, setQuery] = useState("");
  const [pick, setPick] = useState(0);
  const boxRef = useRef<HTMLDivElement>(null);
  const listRef = useRef<HTMLDivElement>(null);
  const tree = useMemo(() => folderTree(folders), [folders]);
  const q = query.trim().toLowerCase();
  // The same match as the Servers page's search: the name or the address.
  const matches = useMemo(
    () => (servers || []).filter((s: any) => !q || s.name.toLowerCase().includes(q) || String(s.host || "").toLowerCase().includes(q)),
    [servers, q],
  );
  useEffect(() => { setPick(0); }, [q]);
  useEffect(() => {
    listRef.current?.querySelector<HTMLElement>(`[data-pick="${pick}"]`)?.scrollIntoView({ block: "nearest" });
  }, [pick]);

  // Under the + button, inside the window; measured before it shows, and again on a resize.
  const [pos, setPos] = useState<{ left: number; top: number } | null>(null);
  useLayoutEffect(() => {
    const place = () => {
      const r = anchor.getBoundingClientRect();
      const w = boxRef.current?.offsetWidth ?? 320;
      setPos({ left: Math.max(8, Math.min(r.left, window.innerWidth - w - 8)), top: r.bottom + 6 });
    };
    place();
    window.addEventListener("resize", place);
    return () => window.removeEventListener("resize", place);
  }, [anchor]);

  // Where the focus was before the menu opened: a terminal being typed in, or
  // the + itself when the menu was opened from the keyboard. Escape gives it
  // back there.
  const before = useRef<HTMLElement | null>(document.activeElement instanceof HTMLElement ? document.activeElement : null);
  const close = (refocus: boolean) => {
    onClose();
    if (!refocus) return;
    const back = before.current && before.current !== document.body && before.current.isConnected ? before.current : anchor;
    back.focus({ preventScroll: true });
  };
  const open = (s: any) => {
    onClose();
    onOpen(s);
  };
  const where = (s: any): string => {
    const id = tree.serverFolder(s);
    return id == null ? "" : tree.pathOf(id).map((f: any) => f.name).join(" › ");
  };
  const onKey = (e: KeyboardEvent<HTMLInputElement>) => {
    if (e.nativeEvent.isComposing) return;
    if (e.key === "ArrowDown") { e.preventDefault(); setPick((p) => Math.min(p + 1, matches.length - 1)); }
    else if (e.key === "ArrowUp") { e.preventDefault(); setPick((p) => Math.max(p - 1, 0)); }
    else if (e.key === "Enter") { e.preventDefault(); const s = matches[pick]; if (s) open(s); }
    else if (e.key === "Escape") { e.preventDefault(); e.stopPropagation(); close(true); }
  };

  return createPortal(
    <>
      <div className="fixed inset-0 z-[80]" onMouseDown={() => close(false)} />
      <div
        ref={boxRef}
        role="dialog"
        aria-label={t("Open a server")}
        data-open-server-menu=""
        className="fixed z-[81] w-80 max-w-[calc(100vw-16px)] rounded-xl border border-white/10 bg-[#15151a] shadow-2xl shadow-black/60 p-1.5 no-drag"
        // Until it is placed it is see-through, not hidden: a hidden box
        // can't take the focus its search asks for.
        style={pos ?? { opacity: 0 }}
      >
        <div className="relative mb-1">
          <Search size={14} className="absolute left-2.5 top-1/2 -translate-y-1/2 text-zinc-500 pointer-events-none" />
          <input
            autoFocus
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            onKeyDown={onKey}
            placeholder={t("Search servers…")}
            aria-label={t("Search servers…")}
            role="combobox"
            aria-expanded="true"
            aria-controls="open-server-list"
            aria-activedescendant={matches[pick] ? `open-server-${matches[pick].id}` : undefined}
            className="w-full h-9 pl-8 pr-2 rounded-lg bg-black/40 border border-white/10 focus:border-primary/50 text-[12.5px] text-zinc-100 outline-none placeholder:text-zinc-600"
          />
        </div>
        <div ref={listRef} id="open-server-list" role="listbox" aria-label={t("Open a server")} className="max-h-72 overflow-y-auto custom-scrollbar">
          {matches.length === 0 && <div className="px-3 py-3 text-[12px] text-zinc-500">{t("No matches.")}</div>}
          {matches.map((s: any, i: number) => {
            const Icon = PROTOCOL_ICON[protocolOf(s)];
            const tab = serverTabs[s.id];
            const folder = where(s);
            const picked = i === pick;
            return (
              <button
                key={s.id}
                id={`open-server-${s.id}`}
                type="button"
                tabIndex={-1}
                data-pick={i}
                role="option"
                aria-selected={picked}
                onMouseMove={() => { if (!picked) setPick(i); }}
                onClick={() => open(s)}
                className={`w-full flex items-center gap-2.5 px-2 h-10 rounded-lg text-left ${picked ? "bg-primary/10" : ""}`}
              >
                <span className="relative w-6 h-6 rounded-md border flex items-center justify-center shrink-0" style={avatarStyle(s.color)}>
                  <Icon size={12} />
                  {tab && <span aria-hidden className={`absolute -bottom-0.5 -right-0.5 w-2 h-2 rounded-full border border-[#15151a] ${TAB_DOT[tab] ?? "bg-zinc-500"}`} />}
                </span>
                <span className="min-w-0 flex-1">
                  <span className={`block truncate text-[12.5px] font-semibold ${picked ? "text-primary" : "text-zinc-100"}`}>{s.name}</span>
                  <span className="block truncate text-[10.5px] text-zinc-500">
                    {folder && <>{folder} · </>}
                    <span className="font-mono">{s.host}</span>
                  </span>
                </span>
                {picked && <kbd aria-hidden className="shrink-0 text-[10px] text-zinc-400 font-mono border border-white/10 rounded px-1">↵</kbd>}
              </button>
            );
          })}
        </div>
        {onQuickConnect && (
          <div className="mt-1 pt-1 border-t border-white/5">
            <button
              type="button"
              onClick={() => { onClose(); onQuickConnect(); }}
              className="w-full flex items-center gap-2 px-2 h-9 rounded-lg text-amber-300/90 hover:bg-amber-500/10 text-[12px] font-semibold"
            >
              <Zap size={13} /> {t("Quick connect")}
            </button>
          </div>
        )}
      </div>
    </>,
    document.body,
  );
}
