// Mobile-only key bar that sits between xterm and the system on-screen
// keyboard, exposing the keys Android (and the iOS soft keyboard) don't
// surface but every terminal user needs: Ctrl, Alt, Shift, Esc, Tab, the
// arrows, | and ~.
//
// Modifier semantics match Termux / iSH:
//   - tap once   → "armed"  (highlight in primary color, fires on next typed char)
//   - tap twice  → "locked" (amber highlight, stays on until tapped again)
//   - tap when locked → off
//
// Whether to actually transform the next typed character lives in
// TerminalView (which owns the xterm `onData` callback). This component
// only renders state + forwards events.

export type ModKey = "ctrl" | "alt" | "shift";
export type ModState = "off" | "armed" | "locked";
export interface ModifiersState { ctrl: ModState; alt: ModState; shift: ModState; }
export type SpecialKey = "esc" | "tab" | "up" | "down" | "left" | "right" | "|" | "~";

interface MobileKeyBarProps {
  modifiers: ModifiersState;
  onToggleModifier: (m: ModKey) => void;
  /** Every key button forwards through here so the parent can encode it
   *  (Shift+Tab → back-tab, arrows with modifiers) and consume the
   *  armed modifiers. */
  onSpecialKey: (key: SpecialKey) => void;
}

export function MobileKeyBar({ modifiers, onToggleModifier, onSpecialKey }: MobileKeyBarProps) {
  const ModBtn = ({ m, label }: { m: ModKey; label: string }) => {
    const s = modifiers[m];
    const tone =
      s === "locked" ? "bg-amber-500/25 text-amber-200 border-amber-500/50 ring-1 ring-amber-300/40 shadow-inner shadow-amber-500/20"
      : s === "armed" ? "bg-primary/20 text-primary border-primary/50 shadow-inner shadow-primary/20"
      : "bg-white/[0.05] text-zinc-300 border-white/10 hover:bg-white/10";
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
        title={s === "locked" ? `${label} locked — tap to release` : s === "armed" ? `${label} armed — next key combines, or tap to lock` : `Tap to arm ${label}`}
        className={`shrink-0 h-9 min-w-[48px] px-2.5 rounded-md text-[10.5px] font-bold uppercase tracking-wider border transition-colors ${tone}`}
      >
        {label}
      </button>
    );
  };

  // `symbol`: a one-glyph key (arrow, | or ~) — narrower, with a larger glyph.
  const KeyBtn = ({ label, onClick, title, symbol }: { label: string; onClick: () => void; title?: string; symbol?: boolean }) => (
    <button
      type="button"
      onClick={onClick}
      onMouseDown={(e) => e.preventDefault()}
      onTouchStart={(e) => e.preventDefault()}
      title={title}
      aria-label={title}
      className={`shrink-0 h-9 rounded-md font-bold border bg-white/[0.05] text-zinc-300 border-white/10 hover:bg-white/10 active:bg-white/15 transition-colors ${
        symbol ? "min-w-[38px] px-2 text-[14px]" : "min-w-[48px] px-2.5 text-[10.5px] uppercase tracking-wider"
      }`}
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
      // On a phone the keys don't all fit; the row scrolls sideways with no
      // scrollbar, so its right edge fades out to show there's more. The
      // extra right padding lets the last key scroll clear of the fade.
      className="shrink-0 flex items-center gap-1.5 pl-2 pr-6 pt-1.5 bg-[#0c0c0e] border-t border-white/10 overflow-x-auto no-scrollbar [mask-image:linear-gradient(to_right,black_calc(100%_-_24px),transparent)]"
    >
      <ModBtn m="ctrl"  label="Ctrl"  />
      <ModBtn m="alt"   label="Alt"   />
      <ModBtn m="shift" label="Shift" />
      <span className="shrink-0 w-px h-6 bg-white/10 mx-1" />
      <KeyBtn label="Esc" onClick={() => onSpecialKey("esc")} title="Send Escape (0x1b)" />
      <KeyBtn label="Tab" onClick={() => onSpecialKey("tab")} title="Send Tab (or back-tab when Shift armed)" />
      <span className="shrink-0 w-px h-6 bg-white/10 mx-1" />
      <KeyBtn symbol label="←" onClick={() => onSpecialKey("left")} title="Left arrow" />
      <KeyBtn symbol label="↑" onClick={() => onSpecialKey("up")} title="Up arrow" />
      <KeyBtn symbol label="↓" onClick={() => onSpecialKey("down")} title="Down arrow" />
      <KeyBtn symbol label="→" onClick={() => onSpecialKey("right")} title="Right arrow" />
      <span className="shrink-0 w-px h-6 bg-white/10 mx-1" />
      <KeyBtn symbol label="|" onClick={() => onSpecialKey("|")} title="Pipe (|)" />
      <KeyBtn symbol label="~" onClick={() => onSpecialKey("~")} title="Tilde (~)" />
    </div>
  );
}
