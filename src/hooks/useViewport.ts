import { type RefObject, useEffect, useLayoutEffect, useState } from "react";

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

/// Live width of one element. For controls that sit in a resizable pane,
/// where the viewport width says nothing about how much room they actually
/// have. The first measure runs in a layout effect, so the first paint
/// already uses the real width instead of a placeholder.
export function useElementWidth(ref: RefObject<HTMLElement | null>): number {
  const [w, setW] = useState(0);
  useLayoutEffect(() => {
    const el = ref.current;
    if (!el) return;
    // Border box both times: the observer's contentRect leaves the padding out.
    const measure = () => setW(Math.round(el.getBoundingClientRect().width));
    measure();
    const ro = new ResizeObserver(measure);
    ro.observe(el);
    return () => ro.disconnect();
  }, [ref]);
  return w;
}
