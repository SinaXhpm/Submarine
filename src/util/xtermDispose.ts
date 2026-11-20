import type { Terminal } from "xterm";

// xterm 5.3's viewport syncs its scroll area on the next frame (and on a
// timer after the terminal opens), and doesn't cancel that when the terminal
// is disposed. A terminal closed within that frame, as on Lock while it draws
// a lot of output, then read its renderer after it was gone ("Cannot read
// properties of undefined (reading 'dimensions')"). After dispose those
// callbacks find nothing to do. The names are xterm's own, kept in its build.
type ViewportInternals = { syncScrollArea?: unknown; _innerRefresh?: unknown };

export function disposeTerminal(term: Terminal): void {
  const viewport = (term as unknown as { _core?: { viewport?: ViewportInternals } })._core?.viewport;
  term.dispose();
  if (viewport) {
    viewport.syncScrollArea = () => {};
    viewport._innerRefresh = () => {};
  }
}
