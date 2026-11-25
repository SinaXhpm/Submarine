import colors from "tailwindcss/colors";
import plugin from "tailwindcss/plugin";

// The interface is dark, or light when the background picked in Settings is
// light (#412; util/appTheme puts the class "light" on <html>). Every colour
// below is a CSS variable: its dark value under :root, its light twin under
// :root.light. A palette's shades trade places (zinc-900 <-> zinc-100, 500
// stays), so what reads on the dark look reads on the light one; white
// becomes the ink the light look writes with, and black becomes white.
const PALETTES = [
  "slate", "gray", "zinc", "neutral", "stone", "red", "orange", "amber", "yellow", "lime", "green",
  "emerald", "teal", "cyan", "sky", "blue", "indigo", "violet", "purple", "fuchsia", "pink", "rose",
];
const TWIN = { 50: 950, 100: 900, 200: 800, 300: 700, 400: 600, 500: 500, 600: 400, 700: 300, 800: 200, 900: 100, 950: 50 };
const SHADES = Object.keys(TWIN);
// The panels' own dark shades, written bg-shade-121214 where a component
// wants exactly #121214 on the dark look.
const PANEL_SHADES = [
  "08080a", "09090b", "0a0a0c", "0a0a0d", "0c0c0e", "0c0c0f", "0e0e10", "0f0f12", "111114", "121214",
  "121215", "121216", "141417", "141418", "15151a", "161619", "16161a", "16161b", "17171c", "1a0f10",
  "1a1a1e", "1b1b20", "1c1c21", "1c1c22", "1f1f26", "232328", "2c1117", "2c1f0b", "38141c", "e4e4e7",
];

const rgbOf = (hex) => {
  const h = hex.replace("#", "");
  const full = h.length === 3 ? h.replace(/./g, "$&$&") : h;
  return [0, 2, 4].map((i) => parseInt(full.slice(i, i + 2), 16));
};
// HSL lightness on the 0-255 scale.
const lightnessOf = (rgb) => (Math.max(...rgb) + Math.min(...rgb)) / 2;
// Where zinc's shades go on the other look, by lightness; a shade between
// them goes in proportion.
const MIRROR = [[0, 255], [255, 0], ...SHADES.map((s) => [lightnessOf(rgbOf(colors.zinc[s])), lightnessOf(rgbOf(colors.zinc[TWIN[s]]))])]
  .sort((a, b) => a[0] - b[0]);
const mirrorLightness = (l) => {
  for (let i = 1; i < MIRROR.length; i++) {
    const [x0, y0] = MIRROR[i - 1];
    const [x1, y1] = MIRROR[i];
    if (l <= x1) return x1 === x0 ? y1 : y0 + ((y1 - y0) * (l - x0)) / (x1 - x0);
  }
  return 0;
};
// A panel shade's light twin: its lightness mirrored, its hue and saturation kept.
const twinOf = (hex) => {
  const rgb = rgbOf(hex);
  const l = lightnessOf(rgb);
  const l2 = mirrorLightness(l);
  const room = (x) => 255 - Math.abs(2 * x - 255);
  const k = room(l) ? room(l2) / room(l) : 0;
  return rgb.map((c) => Math.round(Math.min(255, Math.max(0, l2 + (c - l) * k))));
};

const dark = {};
const light = {};
const add = (name, darkRgb, lightRgb) => {
  dark[`--c-${name}`] = darkRgb.join(" ");
  light[`--c-${name}`] = lightRgb.join(" ");
};
for (const p of PALETTES) for (const s of SHADES) add(`${p}-${s}`, rgbOf(colors[p][s]), rgbOf(colors[p][TWIN[s]]));
add("white", [255, 255, 255], rgbOf(colors.zinc[950]));
add("black", [0, 0, 0], [255, 255, 255]);
for (const h of PANEL_SHADES) add(`shade-${h}`, rgbOf(h), twinOf(h));

const themed = (name) => `rgb(var(--c-${name}) / <alpha-value>)`;

/** @type {import('tailwindcss').Config} */
export default {
  content: ["./index.html", "./src/**/*.{js,ts,jsx,tsx}"],
  theme: {
    colors: {
      inherit: "inherit",
      current: "currentColor",
      transparent: "transparent",
      white: themed("white"),
      black: themed("black"),
      ...Object.fromEntries(PALETTES.map((p) => [p, Object.fromEntries(SHADES.map((s) => [s, themed(`${p}-${s}`)]))])),
      shade: Object.fromEntries(PANEL_SHADES.map((h) => [h, themed(`shade-${h}`)])),
      // A switch's knob: white on either look.
      knob: "#ffffff",
    },
    extend: {
      colors: {
        background: "var(--background)",
        surface: themed("shade-09090b"),
        panel: themed("shade-121214"),
        primary: "rgb(var(--primary) / <alpha-value>)",
        border: "var(--border)",
        muted: themed("zinc-400"),
      },
      // Text in the accent colour: on the light look, the accent made dark
      // enough to read (util/appTheme). Dark text on an accent or amber fill
      // stays dark on either look, as do shadows.
      textColor: {
        primary: "rgb(var(--primary-text) / <alpha-value>)",
        black: "#000000",
        zinc: { 950: colors.zinc[950] },
      },
      boxShadowColor: {
        black: "#000000",
      },
      // The fonts the user picks in Settings (util/uiFont.ts, defaults in
      // App.css): font-sans is the interface font, font-mono the one for text
      // set in columns, which follows the terminal font.
      fontFamily: {
        sans: ["var(--ui-font)"],
        mono: ["var(--ui-mono)"],
      },
      // Tailwind's spin and pulse, but they stop: 12 turns, 5 pulses. For
      // states that can last (a tab connecting or waiting for an answer, a
      // retry, broadcast on): the browser draws an endless animation at every
      // frame, and a tab left waiting kept 13-29% of a CPU core busy for a
      // dot and a spinner that had nothing new to say.
      animation: {
        "spin-brief": "spin 1s linear 12",
        "pulse-brief": "pulse 2s cubic-bezier(0.4, 0, 0.6, 1) 5",
        // One reconnect attempt's spinner: turns for at most half a minute.
        "spin-attempt": "spin 1.2s linear 25",
      },
    },
  },
  plugins: [
    plugin(({ addBase }) => {
      addBase({
        ":root": dark,
        ":root.light": light,
        // What keeps its own dark look on the light one: a terminal, a remote
        // screen, and what is drawn over them (a find bar, a pane's close button).
        ":root.light .theme-dark": {
          ...dark,
          "--primary-text": "var(--primary)",
          "--lift-to": "255",
          "color-scheme": "dark",
          color: "rgb(var(--c-white))",
        },
      });
    }),
  ],
}
