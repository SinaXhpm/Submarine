// Terminal fonts shipped inside the app, so they work on every device —
// including Android and machines where they aren't installed. Regular + bold
// (xterm draws bold text with real bold glyphs instead of smearing the regular
// ones). Each font is split by script (latin, cyrillic, greek, …) and the
// webview only fetches the parts a terminal actually renders.
//
// All are free to redistribute: SIL Open Font License 1.1, except Ubuntu Mono
// (Ubuntu Font Licence 1.0). Their licence texts ship with the app under
// /licenses/fonts/. Imported once from main.tsx.
import "@fontsource/jetbrains-mono/400.css";
import "@fontsource/jetbrains-mono/700.css";
import "@fontsource/fira-code/400.css";
import "@fontsource/fira-code/700.css";
import "@fontsource/cascadia-code/400.css";
import "@fontsource/cascadia-code/700.css";
import "@fontsource/source-code-pro/400.css";
import "@fontsource/source-code-pro/700.css";
import "@fontsource/ubuntu-mono/400.css";
import "@fontsource/ubuntu-mono/700.css";
import "@fontsource/ibm-plex-mono/400.css";
import "@fontsource/ibm-plex-mono/700.css";
import "@fontsource/inconsolata/400.css";
import "@fontsource/inconsolata/700.css";

/** The fonts' real family names. Their bundled @font-face copies are
 *  registered as "Submarine <name>" so they never hide an installed copy —
 *  see bundledAlias in terminalFont.ts. */
export const BUNDLED_FONTS: readonly string[] = [
  "JetBrains Mono",
  "Fira Code",
  "Cascadia Code",
  "Source Code Pro",
  "Ubuntu Mono",
  "IBM Plex Mono",
  "Inconsolata",
];
