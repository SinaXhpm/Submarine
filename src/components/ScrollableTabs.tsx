import { useRef, useEffect, useState, useCallback, ReactNode } from "react";
import { ChevronLeft, ChevronRight } from "lucide-react";

// Horizontal tab-strip wrapper that solves the "tabs overflow off-screen and
// the user has no way to reach them" problem we kept hitting on narrow
// windows + the embedded-pane layouts where many sub-sub-tabs live inside a
// shrinking column. Native scrollbars stay hidden (matches the rest of the
// app's chrome), but we add:
//
//   1. Inline left/right chevron buttons that fade in only when the strip is
//      actually scrollable in that direction. Click → smooth scroll by ~70%
//      of the visible width. Touch / trackpad horizontal swipes still work.
//   2. Mouse-wheel-translates-to-horizontal-scroll, so a desktop user with a
//      vertical wheel can reach hidden tabs without needing Shift+Wheel.
//   3. An "active tab auto-into-view" pass via data-active="true" — the
//      currently-selected tab is scrolled into view whenever it changes, so
//      switching tabs via keyboard or programmatically doesn't leave it
//      off-screen.
//
// Usage:
//   <ScrollableTabs trailing={<RefreshButton/>}>
//     <button data-active={tab === "a"} ...>A</button>
//     <button data-active={tab === "b"} ...>B</button>
//   </ScrollableTabs>
//
// `trailing` is rendered outside the scrolling region (right-pinned), so it
// stays visible even when the strip is mid-scroll.

interface ScrollableTabsProps {
  children: ReactNode;
  /** Optional non-scrolling element rendered to the right of the strip (e.g. a Refresh button). */
  trailing?: ReactNode;
  /** Extra class on the outer wrapper. */
  className?: string;
  /** Extra class on the inner scrolling row. */
  innerClassName?: string;
}

export function ScrollableTabs({ children, trailing, className = "", innerClassName = "" }: ScrollableTabsProps) {
  const ref = useRef<HTMLDivElement>(null);
  const [canLeft, setCanLeft] = useState(false);
  const [canRight, setCanRight] = useState(false);

  const update = useCallback(() => {
    const el = ref.current;
    if (!el) return;
    // 2-px slack absorbs sub-pixel rounding on HiDPI displays — without it the
    // chevrons can flicker at the exact edges.
    setCanLeft(el.scrollLeft > 2);
    setCanRight(el.scrollLeft + el.clientWidth < el.scrollWidth - 2);
  }, []);

  useEffect(() => {
    const el = ref.current;
    if (!el) return;
    update();
    // ResizeObserver fires on container AND child mutations because adding /
    // removing tabs changes scrollWidth without firing 'scroll'.
    const ro = new ResizeObserver(update);
    ro.observe(el);
    for (const child of Array.from(el.children)) ro.observe(child as Element);
    const mo = new MutationObserver(() => {
      // Re-observe new children after DOM mutates (tab insert/remove).
      ro.disconnect();
      ro.observe(el);
      for (const child of Array.from(el.children)) ro.observe(child as Element);
      update();
    });
    mo.observe(el, { childList: true });
    el.addEventListener("scroll", update, { passive: true });
    window.addEventListener("resize", update);
    return () => {
      ro.disconnect();
      mo.disconnect();
      el.removeEventListener("scroll", update);
      window.removeEventListener("resize", update);
    };
  }, [update]);

  // Keep the active tab in view whenever it changes. We watch the
  // `data-active` attribute on descendants so callers don't have to thread an
  // "active id" prop in — every tab strip in the app just marks its current
  // button with `data-active={isActive}` and we handle the scroll.
  useEffect(() => {
    const el = ref.current;
    if (!el) return;
    const scrollActiveIntoView = () => {
      const active = el.querySelector<HTMLElement>('[data-active="true"]');
      if (!active) return;
      const r = active.getBoundingClientRect();
      const er = el.getBoundingClientRect();
      if (r.left < er.left || r.right > er.right) {
        active.scrollIntoView({ inline: "nearest", block: "nearest", behavior: "smooth" });
      }
    };
    scrollActiveIntoView();
    const mo = new MutationObserver(scrollActiveIntoView);
    mo.observe(el, { attributes: true, subtree: true, attributeFilter: ["data-active"] });
    return () => mo.disconnect();
  }, []);

  const scrollBy = (dir: 1 | -1) => {
    const el = ref.current;
    if (!el) return;
    el.scrollBy({ left: dir * Math.max(120, el.clientWidth * 0.7), behavior: "smooth" });
  };

  return (
    <div className={`flex items-center min-w-0 ${className}`}>
      {/* Scroll-row + chevrons live inside their own relative box so the
          chevrons can be positioned against the strip's edges, while the
          trailing slot sits outside and stays put. */}
      <div className="relative flex-1 min-w-0">
        <div
          ref={ref}
          onWheel={(e) => {
            // Translate vertical wheel to horizontal scroll *only* when the
            // strip is actually overflowing — otherwise we'd swallow the
            // page's vertical scroll while the cursor is over the tab bar.
            const el = e.currentTarget;
            if (el.scrollWidth > el.clientWidth && Math.abs(e.deltaY) > Math.abs(e.deltaX)) {
              el.scrollLeft += e.deltaY;
            }
          }}
          className={`flex items-center gap-1.5 overflow-x-auto no-scrollbar -mx-1 px-1 snap-x ${innerClassName}`}
        >
          {children}
        </div>
        {canLeft && (
          <button
            type="button"
            onClick={() => scrollBy(-1)}
            aria-label="Scroll tabs left"
            className="absolute left-0 top-0 bottom-0 w-7 flex items-center justify-center text-zinc-300 hover:text-white z-10
                       bg-gradient-to-r from-[#121214] via-[#121214]/85 to-transparent
                       cursor-pointer"
          >
            <ChevronLeft size={14} />
          </button>
        )}
        {canRight && (
          <button
            type="button"
            onClick={() => scrollBy(1)}
            aria-label="Scroll tabs right"
            className="absolute right-0 top-0 bottom-0 w-7 flex items-center justify-center text-zinc-300 hover:text-white z-10
                       bg-gradient-to-l from-[#121214] via-[#121214]/85 to-transparent
                       cursor-pointer"
          >
            <ChevronRight size={14} />
          </button>
        )}
      </div>
      {trailing && <div className="shrink-0 ml-2">{trailing}</div>}
    </div>
  );
}
