// Terminal font preferences — per-device (localStorage), shared by the
// Settings panel, every TerminalView and the connect-log box so they can
// never disagree about the size or face.
import { BUNDLED_FONTS } from "./bundledFonts";

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

// Fonts shipped with the app (always available, on every device). The picker
// lists these first, then every font installed on this device.
export { BUNDLED_FONTS };

const isBundled = (name: string) =>
  BUNDLED_FONTS.some((f) => f.toLowerCase() === name.toLowerCase());

export function clampFontSize(v: unknown): number {
  const n = typeof v === "number" ? v : parseInt(String(v ?? ""), 10);
  if (!Number.isFinite(n)) return DEFAULT_FONT_SIZE;
  return Math.min(MAX_FONT_SIZE, Math.max(MIN_FONT_SIZE, Math.round(n)));
}

// Keep only what a CSS font-family list can contain. The value only ever
// lands in a font-family property, but stripping `;{}<>\` etc. keeps a pasted
// oddity from producing an invalid declaration. `+ & ( ) !` stay: real family
// names use them ("M+ 1mn"), and fontFamilyCss quotes every name.
export function sanitizeFontFamily(raw: string): string {
  return (raw || "")
    .replace(/[^\p{L}\p{N} _.,'"+&()!-]/gu, "")
    .replace(/\s+/g, " ")
    .trim()
    .slice(0, 200);
}

// The private name a bundled font's own copy is registered under (see the
// @fontsource transform in vite.config.ts), or null for any other font.
export function bundledAlias(name: string): string | null {
  const hit = BUNDLED_FONTS.find((f) => f.toLowerCase() === name.trim().toLowerCase());
  return hit ? `Submarine ${hit}` : null;
}

const GENERIC_FAMILIES = new Set([
  "monospace", "serif", "sans-serif", "ui-monospace", "system-ui", "cursive", "fantasy",
]);

// User value → CSS font-family: every name quoted ("Fira Code"), generic
// families left bare, and a bundled font followed by its bundled copy — so an
// installed copy of it wins (often the fuller one, with box drawing and
// Powerline glyphs) and the bundled one fills in when it isn't installed. The
// default stack always follows as the fallback.
export function fontFamilyCss(custom: string): string {
  const v = sanitizeFontFamily(custom);
  const parts: string[] = [];
  for (const token of v.split(",")) {
    const name = token.replace(/["']/g, "").trim();
    if (!name) continue;
    if (GENERIC_FAMILIES.has(name.toLowerCase())) {
      parts.push(name.toLowerCase());
      continue;
    }
    parts.push(`"${name}"`);
    const alias = bundledAlias(name);
    if (alias) parts.push(`"${alias}"`);
  }
  return parts.length ? `${parts.join(", ")}, ${DEFAULT_FONT_STACK}` : DEFAULT_FONT_STACK;
}

export function readFontSize(): number {
  try {
    return clampFontSize(localStorage.getItem(FONT_SIZE_KEY) ?? DEFAULT_FONT_SIZE);
  } catch {
    return DEFAULT_FONT_SIZE;
  }
}

// A pinch on a terminal and the key bar's text-size keys (#372) change the
// same saved size as Settings. They ask DesktopApp, which keeps the settings,
// so Settings shows the new size and its next save can't put the old one back.
export const FONT_SIZE_REQUEST = "submarine-font-size-request";
export type FontSizeRequest = { size: number } | { step: number };
export function requestFontSize(request: FontSizeRequest): void {
  window.dispatchEvent(new CustomEvent<FontSizeRequest>(FONT_SIZE_REQUEST, { detail: request }));
}

// The size a request asks for, from the size now. Anything else keeps it.
export function requestedFontSize(now: number, request: unknown): number {
  const r = request as { size?: unknown; step?: unknown } | null;
  if (typeof r?.size === "number" && Number.isFinite(r.size)) return clampFontSize(r.size);
  if (typeof r?.step === "number" && Number.isFinite(r.step)) return clampFontSize(now + r.step);
  return now;
}

// A pinch scales the size with the distance between the fingers, from 6 to
// 40: beyond those a slip of the fingers leaves text too small to read or a
// terminal a few columns wide. A size set in Settings beyond them is the
// limit on that side instead, so a pinch never jumps it into the range.
export const PINCH_MIN_FONT_SIZE = 6;
export const PINCH_MAX_FONT_SIZE = 40;
export function pinchFontSize(start: number, scale: number): number {
  if (!Number.isFinite(scale) || scale <= 0) return start;
  return keepInPinchRange(start, start * scale);
}
// A size a pinch that began at `start` can reach, in whole steps.
export function keepInPinchRange(start: number, size: number): number {
  if (!Number.isFinite(size)) return start;
  const lo = Math.min(start, PINCH_MIN_FONT_SIZE);
  const hi = Math.max(start, PINCH_MAX_FONT_SIZE);
  return Math.min(hi, Math.max(lo, Math.round(size)));
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
  // A bundled font is always available — and the canvas probe below would
  // wrongly say "missing" until the webview has fetched it the first time.
  if (isBundled(n)) return true;
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
