import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// @ts-expect-error process is a nodejs global
const host = process.env.TAURI_DEV_HOST;

// https://vite.dev/config/
export default defineConfig(async () => ({
  plugins: [react()],

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
    watch: {
      // 3. tell Vite to ignore watching `src-tauri`
      ignored: ["**/src-tauri/**"],
    },
  },
}));
