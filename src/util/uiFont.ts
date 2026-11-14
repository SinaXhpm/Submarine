// The interface font — per device (localStorage), like the terminal font. The
// two set the CSS variables the app's own text uses (defaults in App.css):
//   --ui-font  menus, panels and forms (body, and Tailwind's font-sans)
//   --ui-mono  text set in columns, such as paths and a server's figures
//              (Tailwind's font-mono): the terminal font, so it follows the
//              face picked for the terminal, then the interface font for the
//              characters a monospace font has none of (Chinese, Japanese,
//              Korean in a translated label), then the system's monospace.
import { bundledAlias, fontFamilyCss, sanitizeFontFamily } from "./terminalFont";

export const UI_FONT_FAMILY_KEY = "submarine-ui-font-family";

/** The app's own interface font: Inter where it is installed, else the system's. */
export const DEFAULT_UI_STACK = "'Inter', system-ui, -apple-system, sans-serif";

const GENERIC_FAMILIES = new Set([
  "serif", "sans-serif", "monospace", "cursive", "fantasy", "system-ui", "ui-sans-serif", "ui-serif", "ui-monospace", "ui-rounded",
]);

/** The user's value as CSS: names quoted, a font the app ships followed by its own copy, generic families bare. */
function parts(custom: string, withGeneric: boolean): string[] {
  const out: string[] = [];
  for (const token of sanitizeFontFamily(custom).split(",")) {
    const name = token.replace(/["']/g, "").trim();
    if (!name) continue;
    if (GENERIC_FAMILIES.has(name.toLowerCase())) {
      if (withGeneric) out.push(name.toLowerCase());
      continue;
    }
    out.push(`"${name}"`);
    const alias = bundledAlias(name);
    if (alias) out.push(`"${alias}"`);
  }
  return out;
}

/** The interface font as a CSS font-family: the user's choice, then the app's own. */
export function uiFontCss(custom: string): string {
  const own = parts(custom, true);
  return own.length ? `${own.join(", ")}, ${DEFAULT_UI_STACK}` : DEFAULT_UI_STACK;
}

/**
 * Text set in columns: the terminal font, then the interface font (named
 * faces only: a generic family there would take every character before the
 * monospace one), then the system's monospace. With no interface font chosen,
 * the characters the terminal font lacks come from the system, as before.
 */
export function uiMonoCss(terminal: string, ui: string): string {
  const mono = fontFamilyCss(terminal).replace(/,\s*monospace\s*$/, "");
  const own = parts(ui, false);
  return `${mono}${own.length ? `, ${own.join(", ")}` : ""}, monospace`;
}

export function readUiFontFamily(): string {
  try {
    return sanitizeFontFamily(localStorage.getItem(UI_FONT_FAMILY_KEY) || "");
  } catch {
    return "";
  }
}

/** Sets both variables on the page: once at start (main.tsx), then on every change in Settings. */
export function applyFonts(terminal: string, ui: string): void {
  const root = document.documentElement.style;
  root.setProperty("--ui-font", uiFontCss(ui));
  root.setProperty("--ui-mono", uiMonoCss(terminal, ui));
}
