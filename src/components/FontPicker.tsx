import { useEffect, useMemo, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { ChevronDown } from "lucide-react";
import { BUNDLED_FONTS } from "../util/terminalFont";

type SystemFont = { family: string; monospace: boolean };

// The installed-font list is read from the OS font folders once per run and
// shared by every picker (it takes a moment on machines with many fonts).
let installedFontsPromise: Promise<SystemFont[]> | null = null;
const loadInstalledFonts = () => {
  if (!installedFontsPromise) {
    installedFontsPromise = invoke<SystemFont[]>("list_system_fonts").catch((e) => {
      installedFontsPromise = null; // let a later open retry
      throw e;
    });
  }
  return installedFontsPromise;
};

type Row = { key: string; family: string; label: string; monospace?: boolean };

const SAMPLE = "Aa 0O 1lI {}";

// Terminal font picker: "Default", then the fonts shipped with the app, a
// divider, then every font installed on this device. Typing filters both
// lists; Enter on typed text uses it as-is (any family, or a CSS list).
export default function FontPicker({
  value,
  onChange,
  onCommit,
  placeholder,
}: {
  value: string;
  onChange: (v: string) => void;
  onCommit: (v: string) => void;
  placeholder?: string;
}) {
  const [open, setOpen] = useState(false);
  // Filter only by what the user typed after opening, not by the current
  // value — opening the list should show everything.
  const [query, setQuery] = useState<string | null>(null);
  const [installed, setInstalled] = useState<SystemFont[] | null>(null);
  const [loadError, setLoadError] = useState(false);
  const [active, setActive] = useState(-1);
  const rootRef = useRef<HTMLDivElement>(null);
  const listRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!open || installed) return;
    let alive = true;
    loadInstalledFonts().then(
      (fonts) => { if (alive) setInstalled(fonts); },
      () => { if (alive) setLoadError(true); },
    );
    return () => { alive = false; };
  }, [open, installed]);

  // Close (keeping what was typed) on a click anywhere outside the picker.
  useEffect(() => {
    if (!open) return;
    const onDown = (e: MouseEvent) => {
      if (rootRef.current && !rootRef.current.contains(e.target as Node)) close(true);
    };
    document.addEventListener("mousedown", onDown);
    return () => document.removeEventListener("mousedown", onDown);
  });

  const q = (query ?? "").trim().toLowerCase();
  const match = (name: string) => !q || name.toLowerCase().includes(q);
  const builtInRows: Row[] = useMemo(
    () => BUNDLED_FONTS.filter(match).map((f) => ({ key: `b:${f}`, family: f, label: f, monospace: true })),
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [q],
  );
  const installedRows: Row[] = useMemo(
    () => (installed ?? []).filter((f) => match(f.family)).map((f) => ({ key: `i:${f.family}`, family: f.family, label: f.family, monospace: f.monospace })),
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [installed, q],
  );
  const defaultRow: Row | null = !q ? { key: "default", family: "", label: "Default (system monospace)" } : null;
  const rows: Row[] = [...(defaultRow ? [defaultRow] : []), ...builtInRows, ...installedRows];

  const close = (keepTyped: boolean) => {
    setOpen(false);
    setQuery(null);
    setActive(-1);
    if (keepTyped) onCommit(value);
  };
  const choose = (family: string) => {
    onChange(family);
    onCommit(family);
    setOpen(false);
    setQuery(null);
    setActive(-1);
  };

  // Keep the keyboard-highlighted row in view.
  useEffect(() => {
    if (active < 0 || !listRef.current) return;
    listRef.current.querySelector(`[data-row="${active}"]`)?.scrollIntoView({ block: "nearest" });
  }, [active]);

  const onKeyDown = (e: React.KeyboardEvent<HTMLInputElement>) => {
    if (e.key === "ArrowDown") {
      e.preventDefault();
      if (!open) { setOpen(true); return; }
      setActive((i) => Math.min(rows.length - 1, i + 1));
    } else if (e.key === "ArrowUp") {
      e.preventDefault();
      setActive((i) => Math.max(0, i - 1));
    } else if (e.key === "Enter") {
      e.preventDefault();
      if (open && active >= 0 && rows[active]) choose(rows[active].family);
      else { onCommit(value); setOpen(false); setQuery(null); }
    } else if (e.key === "Escape") {
      if (open) { e.preventDefault(); close(true); }
    }
  };

  const renderRow = (row: Row, index: number) => {
    const selected = (row.family || "").toLowerCase() === value.trim().toLowerCase();
    return (
      <button
        key={row.key}
        type="button"
        role="option"
        aria-selected={selected}
        data-row={index}
        onMouseDown={(e) => e.preventDefault()} // keep focus in the input
        onClick={() => choose(row.family)}
        onMouseEnter={() => setActive(index)}
        style={{ contentVisibility: "auto", containIntrinsicSize: "auto 30px" } as React.CSSProperties}
        className={`w-full flex items-center gap-3 px-3 h-[30px] text-left text-[12.5px] transition-colors ${
          index === active ? "bg-white/[0.07]" : ""
        } ${selected ? "text-primary" : "text-zinc-200"}`}
      >
        <span className="truncate flex-1">{row.label}</span>
        {row.family && row.monospace === false && (
          <span className="shrink-0 text-[10px] text-zinc-500" title="Not monospace — columns may not line up in a terminal">proportional</span>
        )}
        {row.family && (
          <span
            className="shrink-0 text-[12px] text-zinc-400 whitespace-pre"
            style={{ fontFamily: `"${row.family.replace(/["\\]/g, "")}", monospace` }}
          >
            {SAMPLE}
          </span>
        )}
      </button>
    );
  };

  let index = 0;
  return (
    <div ref={rootRef} className="space-y-1.5">
      <div className="relative">
        <input
          id="term-font-family"
          role="combobox"
          aria-expanded={open}
          aria-controls="term-font-list"
          aria-autocomplete="list"
          value={value}
          onFocus={() => setOpen(true)}
          onClick={() => setOpen(true)}
          onChange={(e) => {
            onChange(e.target.value);
            setQuery(e.target.value);
            setActive(-1);
            setOpen(true);
          }}
          onKeyDown={onKeyDown}
          placeholder={placeholder}
          spellCheck={false}
          autoCapitalize="off"
          autoCorrect="off"
          className="w-full h-9 bg-black border border-white/10 rounded-lg pl-3 pr-9 text-[12.5px] text-white placeholder:text-zinc-600 focus:border-primary/50 outline-none"
        />
        <button
          type="button"
          tabIndex={-1}
          aria-label={open ? "Close font list" : "Open font list"}
          onMouseDown={(e) => e.preventDefault()}
          onClick={() => (open ? close(true) : setOpen(true))}
          className="absolute right-1 top-1 w-7 h-7 flex items-center justify-center text-zinc-500 hover:text-white"
        >
          <ChevronDown size={14} className={`transition-transform ${open ? "rotate-180" : ""}`} />
        </button>
      </div>

      {open && (
        <div
          id="term-font-list"
          ref={listRef}
          role="listbox"
          className="max-h-72 overflow-y-auto custom-scrollbar rounded-lg border border-white/10 bg-[#0c0c0f] py-1"
        >
          {defaultRow && renderRow(defaultRow, index++)}

          <div className="px-3 pt-2 pb-1 text-[10px] font-black text-zinc-500 uppercase tracking-wider">Built-in</div>
          {builtInRows.length ? builtInRows.map((r) => renderRow(r, index++)) : (
            <div className="px-3 py-1.5 text-[11.5px] text-zinc-600">No match</div>
          )}

          <div className="my-1.5 border-t border-white/10" role="separator" />

          <div className="px-3 pt-1 pb-1 text-[10px] font-black text-zinc-500 uppercase tracking-wider">
            Installed on this device{installed && installed.length ? ` · ${installed.length}` : ""}
          </div>
          {installed === null && !loadError && (
            <div className="px-3 py-1.5 text-[11.5px] text-zinc-500">Reading installed fonts…</div>
          )}
          {loadError && (
            <div className="px-3 py-1.5 text-[11.5px] text-amber-400/90">Couldn't read the installed fonts.</div>
          )}
          {installed && installed.length === 0 && (
            <div className="px-3 py-1.5 text-[11.5px] text-zinc-500">
              Installed fonts can't be listed on this device — the built-in fonts above work everywhere.
            </div>
          )}
          {installed && installed.length > 0 && (installedRows.length ? installedRows.map((r) => renderRow(r, index++)) : (
            <div className="px-3 py-1.5 text-[11.5px] text-zinc-600">No match</div>
          ))}
        </div>
      )}
    </div>
  );
}
