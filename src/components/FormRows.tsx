import type { ReactNode } from "react";

// A row of a server's options: its label and control, and under them what it
// does. The server form and the RDP and VNC tabs' panels use the same ones.

// Speed and quality in one go: the sets of choices side by side, the one the
// choices are now marked (none: the user's own mix), and what it does under them.
export function QuickChoiceRow<T extends string>({ choices, current, onPick, hint }: {
  choices: { value: T; label: string }[];
  current: T | null;
  onPick: (value: T) => void;
  hint: string;
}) {
  return (
    <div className="space-y-1">
      <div className="flex justify-between items-center gap-3">
        <span className="text-[11px] font-bold text-zinc-400 shrink-0">Quick choice</span>
        <div role="radiogroup" aria-label="Quick choice" className="flex-1 min-w-0 grid grid-cols-3 gap-1 p-0.5 rounded-lg bg-[#1a1a1e] border border-white/10">
          {choices.map((p) => (
            <button
              key={p.value}
              type="button"
              role="radio"
              aria-checked={current === p.value}
              onClick={() => onPick(p.value)}
              className={`h-8 px-1 rounded-md text-[11px] font-semibold truncate transition-colors ${current === p.value ? "bg-primary/20 text-primary" : "text-zinc-400 hover:text-zinc-200"}`}
            >
              {p.label}
            </button>
          ))}
        </div>
      </div>
      <p className="text-[10.5px] text-zinc-500 leading-snug ml-1">{hint}</p>
    </div>
  );
}

// A choice from a list.
export function ChoiceRow({ label, hint, value, onChange, choices }: {
  label: string;
  hint?: string;
  value: string | number;
  onChange: (value: string) => void;
  choices: { value: string | number; label: string; disabled?: boolean }[];
}) {
  return (
    <div className="space-y-1">
      <div className="flex justify-between items-center gap-3">
        <label className="text-[11px] font-bold text-zinc-400 shrink-0">{label}</label>
        <select
          aria-label={label}
          className="flex-1 min-w-0 h-9 bg-[#1a1a1e] rounded-lg px-3 text-[12px] text-zinc-300 border border-white/10 outline-none focus:border-primary/50 focus:bg-[#232328] transition-all shadow-inner"
          value={value}
          onChange={(e) => onChange(e.target.value)}
        >
          {choices.map((c) => (
            <option key={c.value} value={c.value} disabled={c.disabled} className="bg-[#1a1a1e]">{c.label}</option>
          ))}
        </select>
      </div>
      {hint && <p className="text-[10.5px] text-zinc-500 leading-snug ml-1">{hint}</p>}
    </div>
  );
}

// An on/off switch.
export function SwitchRow({ label, hint, checked, onChange }: {
  label: string;
  hint?: ReactNode;
  checked: boolean;
  onChange: (checked: boolean) => void;
}) {
  return (
    <div className="flex items-center justify-between gap-3">
      <div className="min-w-0">
        <div className="text-[11px] font-bold text-zinc-300">{label}</div>
        {hint && <div className="text-[10.5px] text-zinc-500 leading-snug">{hint}</div>}
      </div>
      <button
        type="button"
        onClick={() => onChange(!checked)}
        role="switch"
        aria-label={label}
        aria-checked={checked}
        className={`relative shrink-0 w-11 h-6 rounded-full transition-colors ${checked ? "bg-primary" : "bg-white/10"}`}
      >
        <span className={`absolute top-0.5 left-0.5 w-5 h-5 bg-zinc-100 rounded-full shadow transition-transform ${checked ? "translate-x-5" : "translate-x-0"}`} />
      </button>
    </div>
  );
}
