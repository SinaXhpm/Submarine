// The title a program in a terminal sets itself (OSC 0 and 2, #391), shown on
// the tab after the server's name. It comes from the server, so it is cleaned
// before it is shown, and a program that keeps changing it (a progress count,
// a clock) doesn't redraw the app each time.
import { stripHidden } from "./hiddenChars";

// Settings → Terminal → "Show program titles on tabs". On unless turned off;
// for this device.
export const SHOW_TERMINAL_TITLES_KEY = "submarine-terminal-show-titles";

export function readShowTerminalTitles(): boolean {
  try {
    return localStorage.getItem(SHOW_TERMINAL_TITLES_KEY) !== "off";
  } catch {
    return true;
  }
}

// The longest title kept, in characters. A longer one is cut, with an ellipsis.
export const TITLE_MAX = 120;

// What of a title is shown: no control, bidi or invisible characters
// (util/hiddenChars), so a title can't hide text or turn the server's name
// around on the tab; one line, with runs of spaces as one; at most TITLE_MAX
// characters, never cut inside one.
export function cleanTerminalTitle(raw: string): string {
  const text = stripHidden(raw).replace(/\s+/g, " ").trim();
  const chars = Array.from(text);
  if (chars.length <= TITLE_MAX) return text;
  return chars.slice(0, TITLE_MAX - 1).join("").trimEnd() + "…";
}

// While a title keeps changing, at most one report in this time. The last one
// always comes.
export const TITLE_GAP_MS = 250;

export interface TitleReporter {
  // A title the program set, as xterm read it ("" when it cleared it).
  set(raw: string): void;
  // Report again after Settings changed: the title, or "" while titles are off.
  refresh(): void;
  // Forget the title: what set it is gone (a new shell after a reconnect).
  reset(): void;
  // Stop; nothing is reported after this. Says whether a title was reported
  // last, which the caller then clears.
  dispose(): boolean;
}

// Reports a terminal's cleaned title when it changes. `enabled` is read at
// each report, so turning the setting off clears the title and turning it on
// brings it back.
export function titleReporter(
  report: (title: string) => void,
  enabled: () => boolean = readShowTerminalTitles,
): TitleReporter {
  let raw = "";
  let shown = "";
  let last = -Infinity;
  let timer: ReturnType<typeof setTimeout> | null = null;
  let disposed = false;
  const flush = () => {
    timer = null;
    if (disposed) return;
    const next = enabled() ? cleanTerminalTitle(raw) : "";
    if (next === shown) return;
    shown = next;
    last = Date.now();
    report(next);
  };
  const schedule = () => {
    if (disposed || timer !== null) return;
    timer = setTimeout(flush, Math.max(0, last + TITLE_GAP_MS - Date.now()));
  };
  return {
    set(next) {
      raw = next;
      schedule();
    },
    refresh: schedule,
    reset() {
      raw = "";
      schedule();
    },
    dispose() {
      disposed = true;
      if (timer !== null) clearTimeout(timer);
      timer = null;
      return shown !== "";
    },
  };
}
