import { ReactNode, useLayoutEffect, useRef, useState } from "react";

// Tab-strip wrapper that simply wraps overflowing tabs onto the next row.
// We tried the chevron-arrow-with-gradient pattern first (v0.2.29-v0.2.30)
// and it felt heavy on phones — gradients fighting the tabs for attention,
// arrows that the user wasn't sure they could even tap. Wrapping is the
// principled answer: every tab is visible at once, no hidden state, no
// special interaction model, the user just reads top-to-bottom-left-to-right.
//
// Cost: one or two extra rows of height on narrow widths. That's much
// cheaper than the bug it replaces, where users couldn't reach hidden tabs.
//
// With `select`, tabs that don't fit on one row become a dropdown of the
// same choices instead of wrapping, so the strip keeps one row's height in a
// narrow panel. The tabs stay mounted (invisible) so the strip can tell when
// they fit again.
//
// Usage:
//   <ScrollableTabs trailing={<RefreshButton/>}>
//     <button>A</button>
//     <button>B</button>
//   </ScrollableTabs>
//
// `trailing` stays pinned to the right edge of the FIRST row regardless of
// wrap — useful for a Refresh-this-tab button that should always be one
// thumb away even when there are eight wrapped tabs.

export interface TabSelect<T extends string> {
  value: T;
  options: { value: T; label: string }[];
  onChange: (value: T) => void;
  /** Accessible name of the dropdown. */
  label: string;
}

interface ScrollableTabsProps<T extends string> {
  children: ReactNode;
  /** Optional non-wrapping element rendered to the right of the strip
   *  (typically a Refresh button). Stays pinned on the first row. */
  trailing?: ReactNode;
  className?: string;
  innerClassName?: string;
  /** Show this dropdown instead of wrapping when the tabs don't fit on
   *  one row. */
  select?: TabSelect<T>;
}

export function ScrollableTabs<T extends string>({ children, trailing, className = "", innerClassName = "", select }: ScrollableTabsProps<T>) {
  const boxRef = useRef<HTMLDivElement | null>(null);
  const rowRef = useRef<HTMLDivElement | null>(null);
  const [collapsed, setCollapsed] = useState(false);
  const collapsible = !!select;

  // The tab row is max-content wide whether it is shown or not, so its width
  // is what the tabs need on one row; compare it with the room there is.
  // A layout effect, so a strip that doesn't fit never paints wrapped.
  useLayoutEffect(() => {
    if (!collapsible) return;
    const box = boxRef.current, row = rowRef.current;
    if (!box || !row) return;
    const check = () => setCollapsed(row.offsetWidth > box.clientWidth);
    check();
    const ro = new ResizeObserver(check);
    ro.observe(box);
    ro.observe(row);
    return () => ro.disconnect();
  }, [collapsible]);

  if (!select) {
    return (
      // Outer flex container keeps the trailing slot pinned to the right of
      // the first row. The wrapping happens inside `flex-1 flex flex-wrap`.
      // `items-start` on the outer flex so the trailing button doesn't jump
      // down when tabs wrap to a second line.
      <div className={`flex items-start gap-2 ${className}`}>
        <div className={`flex-1 min-w-0 flex flex-wrap items-center gap-1.5 ${innerClassName}`}>
          {children}
        </div>
        {trailing && <div className="shrink-0">{trailing}</div>}
      </div>
    );
  }

  return (
    <div className={`flex items-start gap-2 ${className}`}>
      <div ref={boxRef} className="flex-1 min-w-0 relative overflow-hidden">
        <div
          ref={rowRef}
          aria-hidden={collapsed || undefined}
          className={`w-max flex items-center gap-1.5 ${collapsed ? "invisible absolute left-0 top-0" : ""} ${innerClassName}`}
        >
          {children}
        </div>
        {collapsed && (
          <select
            value={select.value}
            onChange={(e) => select.onChange(e.target.value as T)}
            aria-label={select.label}
            title={select.label}
            className="w-full h-8 sm:h-7 px-2 rounded-lg bg-primary/10 text-primary border border-primary/20 text-[11px] font-semibold tracking-tight focus:outline-none focus:border-primary/50 cursor-pointer"
          >
            {select.options.map((o) => (
              <option key={o.value} value={o.value} className="bg-[#1a1a1e] text-zinc-200">{o.label}</option>
            ))}
          </select>
        )}
      </div>
      {trailing && <div className="shrink-0">{trailing}</div>}
    </div>
  );
}
