import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// @ts-expect-error process is a nodejs global
const host = process.env.TAURI_DEV_HOST;

// https://vite.dev/config/
// The bundled terminal fonts (@fontsource/*) list a `.woff` fallback after each
// `.woff2` source. Every webview we run in (WebView2, WebKitGTK, WKWebView,
// Android WebView) reads woff2, so the fallback would only double the size of
// the fonts shipped in the app. Drop it before Vite resolves the url()s, and
// give each bundled family its private name (see below).
const fontsourceWoff2Only = {
  name: "fontsource-woff2-only",
  enforce: "pre" as const,
  transform(code: string, id: string) {
    if (!/[\\/]@fontsource[\\/].+\.css$/.test(id)) return null;
    return code
      .replace(/,\s*url\([^)]*\.woff\)\s*format\(['"]woff['"]\)/g, "")
      // Register each bundled face under a private name ("Submarine Fira
      // Code"): an @font-face with the real family name would hide an
      // installed copy, which is often the fuller one (box drawing,
      // Powerline). src/util/terminalFont.ts lists the real name first, so
      // an installed copy wins and the bundled one fills in when it's absent.
      .replace(/font-family:\s*(['"])([^'"]+)\1/g, "font-family: 'Submarine $2'");
  },
};

export default defineConfig(async () => ({
  plugins: [react(), fontsourceWoff2Only],

  // Vite options tailored for Tauri development and only applied in `tauri dev` or `tauri build`
  //
  // 1. prevent Vite from obscuring rust errors
  clearScreen: false,
  // 2. tauri expects a fixed port, fail if that port is not available
  server: {
    port: 1420,
    strictPort: true,
    // Bind to ALL interfaces so:
    //   - desktop dev still works on 127.0.0.1
    //   - Tauri Android dev can reach Vite over the LAN IP Tauri picks
    //     (it injects TAURI_DEV_HOST and rewrites the WebView's devUrl)
    //   - `adb reverse tcp:1420 tcp:1420` (USB-tethered phones with no
    //     Wi-Fi reachability) lands on Vite's localhost binding too
    // When TAURI_DEV_HOST is set explicitly, honour it — that's what Tauri
    // uses to keep the WebView and Vite on the same host string.
    host: host || "0.0.0.0",
    hmr: host
      ? {
          protocol: "ws",
          host,
          port: 1421,
        }
      : undefined,
    // The dev server binds to all interfaces (above) so Tauri Android dev and
    // adb-reverse can reach it, which also makes it reachable by anyone on the
    // same LAN while `tauri dev` runs. By default Vite serves any file under
    // the project root, so serve only what the app is built from; everything
    // else that lives in or beside the repo (local config, tool folders, logs,
    // build output, signing material) answers 403.
    fs: {
      strict: true,
      allow: ["index.html", "src", "public", "node_modules"],
      // A second net, should the allow list ever widen. This replaces Vite's
      // default deny list, so its `.env` / `*.{crt,pem}` entries are repeated.
      deny: [
        ".env",
        ".env.*",
        "*.{crt,pem}",
        "**/.git/**",
        "**/mock-api/**",
        "**/keystore.properties",
        "**/*.{jks,keystore}",
        "**/src-tauri/gen/**",
      ],
    },
    watch: {
      // 3. tell Vite to ignore watching `src-tauri`
      ignored: ["**/src-tauri/**"],
    },
  },
}));
