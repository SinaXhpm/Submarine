import { Fragment, useEffect, useLayoutEffect, useRef, useState } from "react";
import { createPortal } from "react-dom";

export interface MenuItem { label: string; icon: React.ReactNode; onSelect: () => void; danger?: boolean; }
// Where a menu opens: under (or, short of room, over) the span top..bottom,
// over it first when `up`; its right edge at `x` when `end`, else its left edge.
export interface MenuPlace { x: number; top: number; bottom: number; end: boolean; up?: boolean; }

// A row's actions in a small menu. Arrow keys move, Enter picks, Escape or
// Tab close it, and a click outside closes it without doing anything else.
export const RowMenu = ({ label, items, place, onClose }: {
  label: string; items: MenuItem[]; place: MenuPlace; onClose: (refocus: boolean) => void;
}) => {
  const ref = useRef<HTMLDivElement | null>(null);
  const [pos, setPos] = useState<{ left: number; top: number } | null>(null);
  const onCloseRef = useRef(onClose);
  onCloseRef.current = onClose;

  // Measured off screen first, then placed inside the window, all before the
  // first paint.
  useLayoutEffect(() => {
    const el = ref.current;
    if (!el) return;
    const { width: w, height: h } = el.getBoundingClientRect();
    const left = Math.max(8, Math.min(place.end ? place.x - w : place.x, window.innerWidth - w - 8));
    const below = place.bottom + 4;
    const above = place.top - h - 4;
    const fitsBelow = below + h <= window.innerHeight - 8;
    const up = place.up ? above >= 8 || !fitsBelow : !fitsBelow;
    setPos({ left, top: up ? Math.max(8, above) : below });
    el.querySelector<HTMLElement>("[role=menuitem]")?.focus({ preventScroll: true });
    if (typeof el.animate === "function" && !window.matchMedia?.("(prefers-reduced-motion: reduce)").matches) {
      // It slides out of the span it opens from.
      el.animate([{ opacity: 0, transform: `translateY(${up ? 4 : -4}px)` }, { opacity: 1, transform: "none" }], { duration: 120, easing: "ease-out" });
    }
  }, [place]);

  useEffect(() => {
    const close = () => onCloseRef.current(false);
    window.addEventListener("resize", close);
    window.addEventListener("blur", close);
    return () => {
      window.removeEventListener("resize", close);
      window.removeEventListener("blur", close);
    };
  }, []);

  const onKeyDown = (e: React.KeyboardEvent) => {
    const all = Array.from(ref.current?.querySelectorAll<HTMLElement>("[role=menuitem]") ?? []);
    const i = all.indexOf(document.activeElement as HTMLElement);
    const go = (n: number) => { e.preventDefault(); all[(n + all.length) % all.length]?.focus(); };
    if (e.key === "ArrowDown") go(i + 1);
    else if (e.key === "ArrowUp") go(i < 0 ? -1 : i - 1);
    else if (e.key === "Home") go(0);
    else if (e.key === "End") go(-1);
    else if (e.key === "Escape" || e.key === "Tab") { e.preventDefault(); e.stopPropagation(); onClose(true); }
  };

  return createPortal(
    <>
      <div
        className="fixed inset-0 z-[9998]"
        onClick={() => onClose(false)}
        onContextMenu={(e) => { e.preventDefault(); onClose(false); }}
      />
      <div
        ref={ref}
        role="menu"
        aria-label={label}
        onKeyDown={onKeyDown}
        style={pos ?? { left: -9999, top: -9999 }}
        className="fixed z-[9999] min-w-[220px] rounded-xl border border-white/10 bg-shade-141418 p-1 shadow-2xl shadow-black/60"
      >
        {items.map((it, i) => (
          <Fragment key={it.label}>
            {it.danger && i > 0 && !items[i - 1].danger && <div role="separator" className="mx-1 my-1 h-px bg-white/[0.07]" />}
            <button
              type="button"
              role="menuitem"
              tabIndex={-1}
              // Focus goes back where the menu came from (a row's ⋯) first,
              // so a confirm that opens next returns it there. 40 px high
              // on a phone, for a finger.
              onClick={() => { onClose(true); it.onSelect(); }}
              className={`flex h-10 sm:h-9 w-full items-center gap-2.5 rounded-lg px-2.5 text-left text-[13px] outline-none transition-colors ${
                it.danger
                  ? "text-rose-300 hover:bg-rose-500/10 focus:bg-rose-500/10"
                  : "text-zinc-200 hover:bg-white/[0.06] focus:bg-white/[0.06]"
              }`}
            >
              <span className="shrink-0 opacity-80">{it.icon}</span>
              {it.label}
            </button>
          </Fragment>
        ))}
      </div>
    </>,
    document.body,
  );
};
