import { useEffect, useState } from "react";

// Live viewport-width hook with derived isNarrow / isCompact flags.

const NARROW_BREAKPOINT = 640;   // sm — labels collapse to icons
const COMPACT_BREAKPOINT = 900;  // 2-pane layouts collapse to single column

export function useViewportWidth(): number {
  // SSR-safe default (Tauri webview always has window, but the guard keeps
  // hot-reload from blowing up on the initial render in some setups).
  const [w, setW] = useState<number>(() =>
    typeof window === "undefined" ? COMPACT_BREAKPOINT : window.innerWidth
  );
  useEffect(() => {
    const onResize = () => setW(window.innerWidth);
    window.addEventListener("resize", onResize);
    return () => window.removeEventListener("resize", onResize);
  }, []);
  return w;
}

export function useIsNarrow(): boolean {
  return useViewportWidth() < NARROW_BREAKPOINT;
}

/// "Compact" = too tight to fit terminal + tool side-by-side comfortably.
/// SessionView uses this to switch its split into a stacked single-pane
/// view with a back-chip to swap.
export function useIsCompact(): boolean {
  return useViewportWidth() < COMPACT_BREAKPOINT;
}
