import type { ReactNode } from "react";

// A row of a server's options: its label and control, and under them what it
// does. The server form and the RDP tab's panels use the same ones.

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
