import { useEffect, useRef } from "react";

// Focus for a slide-over panel that stays mounted while closed, so that it
// can slide out. Closed, its backdrop is inert; open, the app behind it is.
// Opening moves focus into the panel (the element that had it is about to go
// inert); closing gives focus back to that element, unless it has been put
// somewhere else in the meantime. Put the returned ref on the panel, with
// tabIndex={-1} so it can hold focus without being a Tab stop.
export const useSlideOverFocus = (open: boolean) => {
  const panelRef = useRef<HTMLDivElement | null>(null);
  useEffect(() => {
    if (!open) return;
    const panel = panelRef.current;
    if (!panel) return;
    const had = document.activeElement;
    const opener = had instanceof HTMLElement && had !== document.body && !panel.contains(had) ? had : null;
    if (!panel.contains(had)) panel.focus({ preventScroll: true });
    return () => {
      // Still in the panel, or dropped to the page as the panel went inert.
      const now = document.activeElement;
      if (opener?.isConnected && (!now || now === document.body || panel.contains(now))) {
        opener.focus({ preventScroll: true });
      }
    };
  }, [open]);
  return panelRef;
};
