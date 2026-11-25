// The interface's look, per device (localStorage): the accent colour and the
// background picked in Settings. On a light background the whole interface
// turns light (#412): Tailwind's colours are CSS variables with a light twin
// each (tailwind.config.js), and the class "light" on <html> switches to them.
// Applied before the first screen (main.tsx), so a light look doesn't start
// dark, and whenever Settings changes the colours.
import { hexRgb } from "./serverLook";

export const PRIMARY_COLOR_KEY = "submarine-primary-color";
export const BG_COLOR_KEY = "submarine-bg-color";
export const DEFAULT_PRIMARY = "#60a5fa";
export const DEFAULT_BACKGROUND = "#0a0a0c";

export const readPrimaryColor = (): string => localStorage.getItem(PRIMARY_COLOR_KEY) || DEFAULT_PRIMARY;
export const readBackgroundColor = (): string => localStorage.getItem(BG_COLOR_KEY) || DEFAULT_BACKGROUND;

type Rgb = [number, number, number];

// WCAG's relative luminance and contrast ratio.
const linear = (v: number) => {
  const s = v / 255;
  return s <= 0.04045 ? s / 12.92 : ((s + 0.055) / 1.055) ** 2.4;
};
const luminance = ([r, g, b]: Rgb) => 0.2126 * linear(r) + 0.7152 * linear(g) + 0.0722 * linear(b);
const contrast = (a: Rgb, b: Rgb) => {
  const [x, y] = [luminance(a), luminance(b)];
  return (Math.max(x, y) + 0.05) / (Math.min(x, y) + 0.05);
};

// Light when dark text reads better on it than white text: above this
// luminance black has the higher contrast. A colour that isn't #rgb or
// #rrggbb (still being typed in Settings) keeps the dark look.
const LIGHT_ABOVE = 0.179;

// The accent as text on this background: as it is when it reads well enough
// (4.5:1, WCAG AA for text), else darkened step by step until it does.
const readableOn = (accent: Rgb, background: Rgb): Rgb => {
  for (let k = 1; k > 0; k -= 0.05) {
    const c = accent.map((v) => Math.round(v * k)) as Rgb;
    if (contrast(c, background) >= 4.5) return c;
  }
  return [0, 0, 0];
};

export function applyTheme(primary: string, background: string): void {
  const root = document.documentElement;
  const accent = hexRgb(primary) ?? (hexRgb(DEFAULT_PRIMARY) as Rgb);
  const bg = hexRgb(background);
  const light = !!bg && luminance(bg) > LIGHT_ABOVE;
  root.classList.toggle("light", light);
  root.style.colorScheme = light ? "light" : "dark";
  root.style.setProperty("--primary", accent.join(" "));
  root.style.setProperty("--primary-hex", primary);
  root.style.setProperty("--primary-text", (light && bg ? readableOn(accent, bg) : accent).join(" "));
  root.style.setProperty("--background", background);
}
