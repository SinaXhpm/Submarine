import { ReactNode } from "react";

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
// Usage:
//   <ScrollableTabs trailing={<RefreshButton/>}>
//     <button>A</button>
//     <button>B</button>
//   </ScrollableTabs>
//
// `trailing` stays pinned to the right edge of the FIRST row regardless of
// wrap — useful for a Refresh-this-tab button that should always be one
// thumb away even when there are eight wrapped tabs.

interface ScrollableTabsProps {
  children: ReactNode;
  /** Optional non-wrapping element rendered to the right of the strip
   *  (typically a Refresh button). Stays pinned on the first row. */
  trailing?: ReactNode;
  className?: string;
  innerClassName?: string;
}

export function ScrollableTabs({ children, trailing, className = "", innerClassName = "" }: ScrollableTabsProps) {
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
