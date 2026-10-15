// How many lines each terminal keeps above the screen — per-device
// (localStorage), like the font settings. xterm's own default is 1,000, which
// a long build log or `journalctl` outruns. Every line costs memory in every
// open terminal (about 12 bytes per column), so the range stops at 50,000:
// at 200 columns that is about 120 MB for one terminal.
export const SCROLLBACK_KEY = "submarine-terminal-scrollback";

export const MIN_SCROLLBACK = 1000;
export const MAX_SCROLLBACK = 50000;
export const DEFAULT_SCROLLBACK = 10000;

export function clampScrollback(v: unknown): number {
  const n = typeof v === "number" ? v : parseInt(String(v ?? ""), 10);
  if (!Number.isFinite(n)) return DEFAULT_SCROLLBACK;
  return Math.min(MAX_SCROLLBACK, Math.max(MIN_SCROLLBACK, Math.round(n)));
}

export function readScrollback(): number {
  try {
    return clampScrollback(localStorage.getItem(SCROLLBACK_KEY) ?? DEFAULT_SCROLLBACK);
  } catch {
    return DEFAULT_SCROLLBACK;
  }
}
