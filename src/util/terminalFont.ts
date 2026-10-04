// Terminal font preferences — per-device (localStorage), shared by the
// Settings panel, every TerminalView and the connect-log box so they can
// never disagree about the size or face.

export const FONT_SIZE_KEY = "submarine-terminal-font-size";
export const FONT_FAMILY_KEY = "submarine-terminal-font-family";

export const MIN_FONT_SIZE = 1;
export const MAX_FONT_SIZE = 99;
export const DEFAULT_FONT_SIZE = 14;

// Fallback stack appended after whatever the user picks, so a font that isn't
// installed on this device degrades to a real monospace face. Consolas stays
// first so Windows looks exactly as before; ui-monospace (SF Mono) / Menlo
// cover macOS, DejaVu/Ubuntu/Liberation/Noto cover Linux and Droid/Noto Sans
// Mono cover Android — instead of everything but Windows landing on Courier New.
export const DEFAULT_FONT_STACK =
  'Consolas, ui-monospace, Menlo, Monaco, "Cascadia Mono", "DejaVu Sans Mono", ' +
  '"Ubuntu Mono", "Liberation Mono", "Noto Sans Mono", "Droid Sans Mono", "Courier New", monospace';

// Suggestions for the font picker. Any installed font name works; these are
// just the common terminal faces (incl. Nerd Font builds for starship/p10k).
export const FONT_PRESETS = [
  "Cascadia Code",
  "Cascadia Mono",
  "Consolas",
  "JetBrains Mono",
  "Fira Code",
  "Hack",
  "Source Code Pro",
  "MesloLGS NF",
  "MesloLGS Nerd Font",
  "FiraCode Nerd Font",
  "JetBrainsMono Nerd Font",
  "SF Mono",
  "Menlo",
  "Monaco",
  "Ubuntu Mono",
  "DejaVu Sans Mono",
  "Noto Sans Mono",
  "Courier New",
];

export function clampFontSize(v: unknown): number {
  const n = typeof v === "number" ? v : parseInt(String(v ?? ""), 10);
  if (!Number.isFinite(n)) return DEFAULT_FONT_SIZE;
  return Math.min(MAX_FONT_SIZE, Math.max(MIN_FONT_SIZE, Math.round(n)));
}

// Keep only what a CSS font-family list can contain. The value only ever
// lands in a font-family property, but stripping `;{}()<>\` etc. keeps a
// pasted oddity from producing an invalid declaration.
export function sanitizeFontFamily(raw: string): string {
  return (raw || "")
    .replace(/[^\p{L}\p{N} _.,'"-]/gu, "")
    .replace(/\s+/g, " ")
    .trim()
    .slice(0, 200);
}

// User value → CSS font-family. A bare name gets quoted ("Fira Code"); a
// value that already looks like a list is used as typed. The default stack
// always follows as the fallback.
export function fontFamilyCss(custom: string): string {
  const v = sanitizeFontFamily(custom);
  if (!v) return DEFAULT_FONT_STACK;
  const head = v.includes(",") || /^["']/.test(v) ? v : `"${v.replace(/["']/g, "")}"`;
  return `${head}, ${DEFAULT_FONT_STACK}`;
}

export function readFontSize(): number {
  try {
    return clampFontSize(localStorage.getItem(FONT_SIZE_KEY) ?? DEFAULT_FONT_SIZE);
  } catch {
    return DEFAULT_FONT_SIZE;
  }
}

export function readFontFamily(): string {
  try {
    return sanitizeFontFamily(localStorage.getItem(FONT_FAMILY_KEY) || "");
  } catch {
    return "";
  }
}

// First family in the user's value, unquoted — what "is it installed?" checks.
export function primaryFontName(custom: string): string {
  return sanitizeFontFamily(custom).split(",")[0].trim().replace(/^["']|["']$/g, "");
}

// Best-effort "is this font installed here?" probe: render a sample in
// "<name>, <generic>" and compare with the bare generic. If the width never
// changes against three different generics, the name fell back every time.
// Web APIs can't list system fonts, so this is the standard trick.
export function isFontAvailable(name: string): boolean {
  const n = primaryFontName(name);
  if (!n) return true;
  if (["monospace", "serif", "sans-serif", "ui-monospace", "system-ui"].includes(n.toLowerCase())) return true;
  try {
    const ctx = document.createElement("canvas").getContext("2d");
    if (!ctx) return true;
    const sample = "mmmmmmmmmmlli1|WW@#0Oo";
    const width = (family: string) => {
      ctx.font = `72px ${family}`;
      return ctx.measureText(sample).width;
    };
    return ["monospace", "serif", "sans-serif"].some(
      (generic) => width(`"${n}", ${generic}`) !== width(generic),
    );
  } catch {
    return true;
  }
}
