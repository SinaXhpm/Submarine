// Mobile-only key bar that sits between xterm and the system on-screen
// keyboard, exposing the keys Android (and the iOS soft keyboard) don't
// surface but every terminal user needs. One row holds the ones used most:
// Ctrl, Esc, Tab and the arrows. A second row, opened by the chevron, adds
// Alt, Shift and the symbols a shell line is full of but a phone keyboard
// hides on another page: | ~ / -. Every key fits on screen, with no scrolling.
//
// Modifier semantics match Termux / iSH:
//   - tap once   → "armed"  (highlight in primary color, fires on next typed char)
//   - tap twice  → "locked" (amber highlight, stays on until tapped again)
//   - tap when locked → off
//
// Whether to actually transform the next typed character lives in
// TerminalView (which owns the xterm `onData` callback). This component
// only renders state + forwards events.

import { useCallback, useEffect, useRef, useSyncExternalStore } from "react";
import { ChevronUp } from "lucide-react";
import { useT } from "../i18n";

// A held arrow repeats as a held key on a keyboard does (#324): once when
// pressed, then every HOLD_EVERY ms from HOLD_DELAY on, until the finger lifts.
const HOLD_DELAY = 400;
const HOLD_EVERY = 60;

export type ModKey = "ctrl" | "alt" | "shift";
export type ModState = "off" | "armed" | "locked";
export interface ModifiersState { ctrl: ModState; alt: ModState; shift: ModState; }
export type SpecialKey = "esc" | "tab" | "up" | "down" | "left" | "right" | "|" | "~" | "/" | "-";

// Whether the second row is open is one choice for the device: it stays as it
// was left, and every session's bar follows it.
const SECOND_ROW_KEY = "submarine-keybar-second-row";
const readSecondRow = () => {
  try { return localStorage.getItem(SECOND_ROW_KEY) === "on"; } catch { return false; }
};
let secondRowOpen = readSecondRow();
const secondRowListeners = new Set<() => void>();
const setSecondRowOpen = (open: boolean) => {
  secondRowOpen = open;
  try { localStorage.setItem(SECOND_ROW_KEY, open ? "on" : "off"); } catch { /* lasts this run */ }
  secondRowListeners.forEach((l) => l());
};
const subscribeSecondRow = (listener: () => void) => {
  secondRowListeners.add(listener);
  return () => { secondRowListeners.delete(listener); };
};

interface MobileKeyBarProps {
  modifiers: ModifiersState;
  onToggleModifier: (m: ModKey) => void;
  /** Every key button forwards through here so the parent can encode it
   *  (Shift+Tab → back-tab, arrows with modifiers) and consume the
   *  armed modifiers. */
  onSpecialKey: (key: SpecialKey) => void;
}

export function MobileKeyBar({ modifiers, onToggleModifier, onSpecialKey }: MobileKeyBarProps) {
  const t = useT();
  const wantsSecondRow = useSyncExternalStore(subscribeSecondRow, () => secondRowOpen);
  // A modifier that's on stays in sight, so closing the row can't hide it.
  const showSecondRow = wantsSecondRow || modifiers.alt !== "off" || modifiers.shift !== "off";

  // The key being held, repeating. The lift is heard on the window, wherever
  // the finger is by then; leaving the app or the page ends it too.
  const sendKey = useRef(onSpecialKey);
  sendKey.current = onSpecialKey;
  const holdTimer = useRef<number | null>(null);
  const stopHold = useCallback(() => {
    if (holdTimer.current !== null) {
      window.clearTimeout(holdTimer.current);
      holdTimer.current = null;
    }
  }, []);
  useEffect(() => {
    window.addEventListener("pointerup", stopHold);
    window.addEventListener("pointercancel", stopHold);
    window.addEventListener("blur", stopHold);
    document.addEventListener("visibilitychange", stopHold);
    return () => {
      stopHold();
      window.removeEventListener("pointerup", stopHold);
      window.removeEventListener("pointercancel", stopHold);
      window.removeEventListener("blur", stopHold);
      document.removeEventListener("visibilitychange", stopHold);
    };
  }, [stopHold]);
  const startHold = (key: SpecialKey) => {
    stopHold();
    sendKey.current(key);
    const again = () => {
      sendKey.current(key);
      holdTimer.current = window.setTimeout(again, HOLD_EVERY);
    };
    holdTimer.current = window.setTimeout(again, HOLD_DELAY);
  };

  // Render functions, not components: a component made anew on each render
  // would be a new kind of element each time, and React would replace the
  // key under the finger while it's held.
  const modBtn = (m: ModKey, label: string) => {
    const s = modifiers[m];
    const tone =
      s === "locked" ? "bg-amber-500/25 text-amber-200 border-amber-500/50 ring-1 ring-amber-300/40 shadow-inner shadow-amber-500/20"
      : s === "armed" ? "bg-primary/20 text-primary border-primary/50 shadow-inner shadow-primary/20"
      : "bg-white/[0.05] text-zinc-300 border-transparent hover:bg-white/10";
    return (
      <button
        type="button"
        onClick={() => onToggleModifier(m)}
        // Pointer/touch should NOT steal focus from the terminal — otherwise
        // the soft keyboard would dismiss every time the user reaches for a
        // modifier. preventDefault on mousedown is the standard trick.
        onMouseDown={(e) => e.preventDefault()}
        onTouchStart={(e) => e.preventDefault()}
        aria-pressed={s !== "off"}
        title={s === "locked" ? t("{label} locked — tap to release", { label }) : s === "armed" ? t("{label} armed — next key combines, or tap to lock", { label }) : t("Tap to arm {label}", { label })}
        className={`flex-1 min-w-0 h-9 rounded-md text-[clamp(9px,2.9vw,10.5px)] font-bold uppercase border transition-colors ${tone}`}
      >
        {label}
      </button>
    );
  };

  // `symbol`: a one-glyph key (arrow or symbol), with a larger glyph.
  // `held`: an arrow, which repeats while held. It goes when pressed, not
  // when the finger lifts as a click does, so the click after a press sends
  // nothing more; one from the keyboard (detail 0) still sends. A long press
  // brings up no menu and selects nothing.
  const keyBtn = ({ label, onClick, title, symbol, held }: { label: string; onClick: () => void; title?: string; symbol?: boolean; held?: SpecialKey }) => (
    <button
      type="button"
      onClick={held ? (e) => { if (e.detail === 0) onClick(); } : onClick}
      onPointerDown={held ? (e) => { if (e.button === 0) startHold(held); } : undefined}
      onContextMenu={held ? (e) => e.preventDefault() : undefined}
      onMouseDown={(e) => e.preventDefault()}
      onTouchStart={(e) => e.preventDefault()}
      title={title}
      aria-label={title}
      className={`flex-1 min-w-0 h-9 rounded-md font-bold border border-transparent bg-white/[0.05] text-zinc-300 hover:bg-white/10 active:bg-white/15 transition-colors ${
        symbol ? "text-[14px]" : "text-[clamp(9px,2.9vw,10.5px)] uppercase"
      }${held ? " touch-none select-none" : ""}`}
    >
      {label}
    </button>
  );

  return (
    <div
      // Pointer events: see preventDefault notes on each button — together
      // they ensure tapping the bar doesn't blur the terminal input target
      // (which on Android would hide the soft keyboard).
      onMouseDown={(e) => e.preventDefault()}
      onTouchStart={(e) => e.preventDefault()}
      // pb-[env(safe-area-inset-bottom)] clears the Android gesture-nav pill
      // (and iPhone home indicator) so the bottom row of Ctrl/Alt/Shift chips
      // doesn't sit under the system UI. Resolves to 0 on devices with no
      // safe area (older Android, desktop) and on browsers that don't
      // support env() — no visible effect there.
      style={{ paddingBottom: 'max(env(safe-area-inset-bottom), 6px)' }}
      className="shrink-0 flex flex-col gap-1 px-1.5 pt-1.5 bg-[#0c0c0e] border-t border-white/10"
    >
      {/* Above the main row, so that row doesn't move under the thumb. */}
      {showSecondRow && (
        <div className="flex gap-1">
          {modBtn("alt", "Alt")}
          {modBtn("shift", "Shift")}
          {keyBtn({ symbol: true, label: "|", onClick: () => onSpecialKey("|"), title: t("Pipe (|)") })}
          {keyBtn({ symbol: true, label: "~", onClick: () => onSpecialKey("~"), title: t("Tilde (~)") })}
          {keyBtn({ symbol: true, label: "/", onClick: () => onSpecialKey("/"), title: t("Slash (/)") })}
          {keyBtn({ symbol: true, label: "-", onClick: () => onSpecialKey("-"), title: t("Dash (-)") })}
        </div>
      )}
      <div className="flex gap-1">
        {modBtn("ctrl", "Ctrl")}
        {keyBtn({ label: "Esc", onClick: () => onSpecialKey("esc"), title: t("Send Escape (0x1b)") })}
        {keyBtn({ label: "Tab", onClick: () => onSpecialKey("tab"), title: t("Send Tab (or back-tab when Shift armed)") })}
        {keyBtn({ symbol: true, label: "←", onClick: () => onSpecialKey("left"), title: t("Left arrow"), held: "left" })}
        {keyBtn({ symbol: true, label: "↑", onClick: () => onSpecialKey("up"), title: t("Up arrow"), held: "up" })}
        {keyBtn({ symbol: true, label: "↓", onClick: () => onSpecialKey("down"), title: t("Down arrow"), held: "down" })}
        {keyBtn({ symbol: true, label: "→", onClick: () => onSpecialKey("right"), title: t("Right arrow"), held: "right" })}
        <button
          type="button"
          onClick={() => setSecondRowOpen(!wantsSecondRow)}
          onMouseDown={(e) => e.preventDefault()}
          onTouchStart={(e) => e.preventDefault()}
          aria-expanded={showSecondRow}
          title={showSecondRow ? t("Fewer keys") : t("More keys")}
          aria-label={showSecondRow ? t("Fewer keys") : t("More keys")}
          className="flex-1 min-w-0 h-9 rounded-md flex items-center justify-center text-zinc-500 active:bg-white/10"
        >
          <ChevronUp size={18} className={`transition-transform ${showSecondRow ? "rotate-180" : ""}`} />
        </button>
      </div>
    </div>
  );
}
