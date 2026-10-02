import type { KeyboardEvent } from "react";

/// Arrow keys for a tab or radio row (WAI-ARIA): Left / Right step through
/// the enabled items, wrapping, and Home / End jump to the ends. The item
/// reached is focused and, unless `select` is false, selected (clicked).
/// Pass `select: false` where selecting is heavy (it swaps a whole pane):
/// arrows then only move focus and Enter / Space selects, so a held key
/// cannot churn through every item. Items pair this with a roving tabIndex,
/// 0 on the selected one and -1 on the rest, so Tab enters the row once.
export function onRovingKeyDown(e: KeyboardEvent<HTMLElement>, select = true) {
  const items = Array.from(
    e.currentTarget.querySelectorAll<HTMLElement>('[role="tab"]:not(:disabled), [role="radio"]:not(:disabled)')
  );
  const n = items.length;
  const i = items.indexOf(document.activeElement as HTMLElement);
  if (i < 0) return;
  const next =
    e.key === "ArrowRight" ? (i + 1) % n :
    e.key === "ArrowLeft" ? (i - 1 + n) % n :
    e.key === "Home" ? 0 :
    e.key === "End" ? n - 1 :
    -1;
  if (next < 0 || next === i) return;
  e.preventDefault();
  items[next].focus();
  if (select) items[next].click();
}
