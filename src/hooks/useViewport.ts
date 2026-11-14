import { useEffect, useState } from "react";

// Live viewport-width flags: isNarrow / isCompact.

const NARROW_BREAKPOINT = 640;   // sm — labels collapse to icons
const COMPACT_BREAKPOINT = 900;  // 2-pane layouts collapse to single column

// Only the yes or no is state: dragging the window's edge re-renders a
// component (DesktopApp: the whole app) when the answer changes, not on every
// pixel.
function useNarrowerThan(limit: number): boolean {
  // SSR-safe default (Tauri webview always has window, but the guard keeps
  // hot-reload from blowing up on the initial render in some setups).
  const [narrower, setNarrower] = useState<boolean>(() =>
    typeof window !== "undefined" && window.innerWidth < limit
  );
  useEffect(() => {
    const onResize = () => setNarrower(window.innerWidth < limit);
    window.addEventListener("resize", onResize);
    return () => window.removeEventListener("resize", onResize);
  }, [limit]);
  return narrower;
}

export function useIsNarrow(): boolean {
  return useNarrowerThan(NARROW_BREAKPOINT);
}

/// "Compact" = too tight to fit terminal + tool side-by-side comfortably.
/// SessionView uses this to switch its split into a stacked single-pane
/// view with a back-chip to swap.
export function useIsCompact(): boolean {
  return useNarrowerThan(COMPACT_BREAKPOINT);
}
