// Mobile-only key bar that sits between xterm and the system on-screen
// keyboard, exposing the keys Android (and the iOS soft keyboard) don't
// surface but every terminal user needs: Ctrl, Alt, Shift, Esc, Tab.
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

interface MobileKeyBarProps {
  modifiers: ModifiersState;
  onToggleModifier: (m: ModKey) => void;
  /** Esc / Tab buttons forward through here so the parent can apply
   *  Shift+Tab → back-tab and decide whether to consume the modifier. */
  onSpecialKey: (key: "esc" | "tab") => void;
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

  const KeyBtn = ({ label, onClick, title }: { label: string; onClick: () => void; title?: string }) => (
    <button
      type="button"
      onClick={onClick}
      onMouseDown={(e) => e.preventDefault()}
      onTouchStart={(e) => e.preventDefault()}
      title={title}
      className="shrink-0 h-9 min-w-[48px] px-2.5 rounded-md text-[10.5px] font-bold uppercase tracking-wider border bg-white/[0.05] text-zinc-300 border-white/10 hover:bg-white/10 active:bg-white/15 transition-colors"
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
      className="shrink-0 flex items-center gap-1.5 px-2 py-1.5 bg-[#0c0c0e] border-t border-white/10 overflow-x-auto no-scrollbar"
    >
      <ModBtn m="ctrl"  label="Ctrl"  />
      <ModBtn m="alt"   label="Alt"   />
      <ModBtn m="shift" label="Shift" />
      <span className="shrink-0 w-px h-6 bg-white/10 mx-1" />
      <KeyBtn label="Esc" onClick={() => onSpecialKey("esc")} title="Send Escape (0x1b)" />
      <KeyBtn label="Tab" onClick={() => onSpecialKey("tab")} title="Send Tab (or back-tab when Shift armed)" />
    </div>
  );
}
