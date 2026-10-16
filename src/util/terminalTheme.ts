// Terminal colours (#15) — per-device (localStorage), like the font settings.
// A scheme is xterm's theme: background, foreground, cursor, selection and
// the 16 ANSI colours. "default" is the look terminals had before schemes
// existed. A scheme of the user's own comes in as a Windows Terminal colour
// scheme (the JSON most scheme sites offer) and is checked key by key: only
// colours in #rgb / #rrggbb / #rrggbbaa form are taken.
import type { ITheme } from "xterm";

export const THEME_KEY = "submarine-terminal-theme";
export const CUSTOM_THEME_KEY = "submarine-terminal-custom-theme";
export const BOLD_BRIGHT_KEY = "submarine-terminal-bold-bright";
export const MIN_CONTRAST_KEY = "submarine-terminal-min-contrast";

export const DEFAULT_THEME_ID = "default";
export const CUSTOM_THEME_ID = "custom";
// What "keep text readable" asks xterm for: the WCAG AA ratio for text.
export const READABLE_CONTRAST = 4.5;

export type TerminalScheme = { id: string; name: string; theme: ITheme };

// The ANSI colour names, in order, as xterm and Windows Terminal call them
// ("magenta" is "purple" in a Windows Terminal scheme).
const ANSI = [
  "black", "red", "green", "yellow", "blue", "magenta", "cyan", "white",
  "brightBlack", "brightRed", "brightGreen", "brightYellow", "brightBlue", "brightMagenta", "brightCyan", "brightWhite",
] as const;

// xterm's own ANSI colours, which the default scheme keeps (for its preview).
export const XTERM_ANSI = {
  black: "#2e3436", red: "#cc0000", green: "#4e9a06", yellow: "#c4a000",
  blue: "#3465a4", magenta: "#75507b", cyan: "#06989a", white: "#d3d7cf",
  brightBlack: "#555753", brightRed: "#ef2929", brightGreen: "#8ae234", brightYellow: "#fce94f",
  brightBlue: "#729fcf", brightMagenta: "#ad7fa8", brightCyan: "#34e2e2", brightWhite: "#eeeeec",
} as const;

const scheme = (
  id: string, name: string, background: string, foreground: string, cursor: string, selection: string, ansi: string[],
): TerminalScheme => {
  const theme: ITheme = { background, foreground, cursor, cursorAccent: background, selectionBackground: selection };
  ANSI.forEach((k, i) => { (theme as Record<string, string>)[k] = ansi[i]; });
  return { id, name, theme };
};

// Values as their authors publish them (Windows Terminal's built-in schemes,
// draculatheme.com, nordtheme.com, ethanschoonover.com/solarized, gruvbox).
export const SCHEMES: TerminalScheme[] = [
  {
    id: DEFAULT_THEME_ID,
    name: "Submarine",
    // xterm's own ANSI colours; only the rest was ever set.
    theme: { background: "#09090b", foreground: "#e4e4e7", cursor: "#60a5fa", selectionBackground: "rgba(96, 165, 250, 0.3)" },
  },
  scheme("campbell", "Campbell", "#0c0c0c", "#cccccc", "#ffffff", "rgba(255, 255, 255, 0.25)", [
    "#0c0c0c", "#c50f1f", "#13a10e", "#c19c00", "#0037da", "#881798", "#3a96dd", "#cccccc",
    "#767676", "#e74856", "#16c60c", "#f9f1a5", "#3b78ff", "#b4009e", "#61d6d6", "#f2f2f2",
  ]),
  scheme("one-half-dark", "One Half Dark", "#282c34", "#dcdfe4", "#ffffff", "rgba(255, 255, 255, 0.2)", [
    "#282c34", "#e06c75", "#98c379", "#e5c07b", "#61afef", "#c678dd", "#56b6c2", "#dcdfe4",
    "#5a6374", "#e06c75", "#98c379", "#e5c07b", "#61afef", "#c678dd", "#56b6c2", "#dcdfe4",
  ]),
  scheme("dracula", "Dracula", "#282a36", "#f8f8f2", "#f8f8f2", "#44475a", [
    "#21222c", "#ff5555", "#50fa7b", "#f1fa8c", "#bd93f9", "#ff79c6", "#8be9fd", "#f8f8f2",
    "#6272a4", "#ff6e6e", "#69ff94", "#ffffa5", "#d6acff", "#ff92df", "#a4ffff", "#ffffff",
  ]),
  scheme("solarized-dark", "Solarized Dark", "#002b36", "#839496", "#93a1a1", "#073642", [
    "#073642", "#dc322f", "#859900", "#b58900", "#268bd2", "#d33682", "#2aa198", "#eee8d5",
    "#002b36", "#cb4b16", "#586e75", "#657b83", "#839496", "#6c71c4", "#93a1a1", "#fdf6e3",
  ]),
  scheme("nord", "Nord", "#2e3440", "#d8dee9", "#d8dee9", "#434c5e", [
    "#3b4252", "#bf616a", "#a3be8c", "#ebcb8b", "#81a1c1", "#b48ead", "#88c0d0", "#e5e9f0",
    "#4c566a", "#bf616a", "#a3be8c", "#ebcb8b", "#81a1c1", "#b48ead", "#8fbcbb", "#eceff4",
  ]),
  scheme("gruvbox-dark", "Gruvbox Dark", "#282828", "#ebdbb2", "#ebdbb2", "#504945", [
    "#282828", "#cc241d", "#98971a", "#d79921", "#458588", "#b16286", "#689d6a", "#a89984",
    "#928374", "#fb4934", "#b8bb26", "#fabd2f", "#83a598", "#d3869b", "#8ec07c", "#ebdbb2",
  ]),
  scheme("one-half-light", "One Half Light", "#fafafa", "#383a42", "#4f525d", "rgba(56, 58, 66, 0.2)", [
    "#383a42", "#e45649", "#50a14f", "#c18301", "#0184bc", "#a626a4", "#0997b3", "#fafafa",
    "#4f525d", "#df6c75", "#98c379", "#e4c07a", "#61afef", "#c577dd", "#56b5c1", "#ffffff",
  ]),
];

const HEX = /^#(?:[0-9a-f]{3}|[0-9a-f]{6}|[0-9a-f]{8})$/i;

// A Windows Terminal colour scheme, pasted as JSON: { "name", "background",
// "foreground", "cursorColor", "selectionBackground", "black" … "brightWhite" }
// ("purple" / "brightPurple" for magenta). One scheme, or a list of them (the
// first is used). Throws a message that says what's wrong.
export function parseWindowsTerminalScheme(text: string): TerminalScheme {
  let data: unknown;
  try {
    data = JSON.parse(text);
  } catch {
    throw new Error("That isn't JSON. Paste a Windows Terminal colour scheme: { \"name\": …, \"background\": \"#…\", … }");
  }
  if (Array.isArray(data)) data = data[0];
  if (!data || typeof data !== "object") throw new Error("That isn't a colour scheme.");
  const src = data as Record<string, unknown>;
  const color = (key: string, required: boolean): string | undefined => {
    const v = src[key];
    if (v === undefined && !required) return undefined;
    if (typeof v !== "string" || !HEX.test(v.trim())) throw new Error(`"${key}" needs a colour like #1e1e2e.`);
    return v.trim().toLowerCase();
  };
  const background = color("background", true)!;
  const theme: ITheme = { background, foreground: color("foreground", true)!, cursorAccent: background };
  const cursor = color("cursorColor", false);
  if (cursor) theme.cursor = cursor;
  const selection = color("selectionBackground", false);
  // Windows Terminal draws its selection see-through; an opaque one here
  // would hide the text under it.
  if (selection) theme.selectionBackground = selection.length === 7 ? `${selection}55` : selection;
  for (const k of ANSI) {
    const wt = k === "magenta" ? "purple" : k === "brightMagenta" ? "brightPurple" : k;
    (theme as Record<string, string | undefined>)[k] = color(wt, true);
  }
  const rawName = typeof src.name === "string" ? src.name.replace(/[\u0000-\u001f\u007f]/g, "").trim() : "";
  return { id: CUSTOM_THEME_ID, name: rawName.slice(0, 60) || "My scheme", theme };
}

// A checked scheme back as Windows Terminal JSON, to keep: only the keys it
// was read from, whatever else the pasted text had.
export function schemeToWindowsTerminalJson(s: TerminalScheme): string {
  const t = s.theme as Record<string, string | undefined>;
  const out: Record<string, string> = { name: s.name };
  if (t.background) out.background = t.background;
  if (t.foreground) out.foreground = t.foreground;
  if (t.cursor) out.cursorColor = t.cursor;
  if (t.selectionBackground) out.selectionBackground = t.selectionBackground;
  for (const k of ANSI) {
    const wt = k === "magenta" ? "purple" : k === "brightMagenta" ? "brightPurple" : k;
    if (t[k]) out[wt] = t[k]!;
  }
  return JSON.stringify(out);
}

export function readCustomScheme(): TerminalScheme | null {
  try {
    const raw = localStorage.getItem(CUSTOM_THEME_KEY);
    return raw ? parseWindowsTerminalScheme(raw) : null;
  } catch {
    return null;
  }
}

// The scheme in use: the saved choice, or the default when it's unknown or
// the custom one is missing or broken.
export function readScheme(): TerminalScheme {
  let id = DEFAULT_THEME_ID;
  try { id = localStorage.getItem(THEME_KEY) || DEFAULT_THEME_ID; } catch { /* default */ }
  if (id === CUSTOM_THEME_ID) return readCustomScheme() ?? SCHEMES[0];
  return SCHEMES.find((s) => s.id === id) ?? SCHEMES[0];
}

export function readThemeId(): string {
  return readScheme().id;
}

// xterm draws bold text in the bright colours by default; some schemes look
// better without.
export function readBoldBright(): boolean {
  try { return localStorage.getItem(BOLD_BRIGHT_KEY) !== "off"; } catch { return true; }
}

// Off by default: colours as the scheme has them. On, xterm lightens or
// darkens text that is hard to read on its background (dark blue on black).
export function readMinContrast(): boolean {
  try { return localStorage.getItem(MIN_CONTRAST_KEY) === "on"; } catch { return false; }
}
