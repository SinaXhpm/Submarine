import React from "react";
import ReactDOM from "react-dom/client";
import App from "./App";
import ErrorBoundary from "./components/ErrorBoundary";
import "./App.css";
// Registers the bundled terminal fonts (@font-face) before any terminal mounts.
import "./util/bundledFonts";

// Last-resort global handlers so a failure that escapes React (an async reject,
// a listener throw) is at least recorded instead of vanishing silently. The
// ErrorBoundary below covers render-time throws; these cover everything else.
window.addEventListener("unhandledrejection", (e) => {
  console.error("[app] unhandled promise rejection:", e.reason);
});
window.addEventListener("error", (e) => {
  console.error("[app] uncaught error:", e.error ?? e.message);
});

// The webview's reload keys (F5, Ctrl/Cmd+R, Ctrl/Cmd+Shift+R) would drop
// every open session, so they do nothing here. Capture phase, so a handler
// that stops the event (xterm, an overlay's input) can't let one through.
// Only the default is cancelled: xterm still sends Ctrl+R to the shell, and
// Ctrl+Shift+R still reaches the terminal's history overlay. By keyCode, not
// `code`: the webview's accelerator goes by the layout's R key (the physical
// O on Dvorak), as does the Ctrl+R that xterm sends.
window.addEventListener("keydown", (e) => {
  const reloadCombo = (e.ctrlKey || e.metaKey) && !e.altKey && e.keyCode === 82;
  if (e.key === "F5" || reloadCombo) e.preventDefault();
}, true);

// ── Android soft-keyboard sizing (visualViewport → --vv-h) ──────────────────
// The Android on-screen keyboard covers our fixed-bottom UI (MobileKeyBar,
// Sidebar dock) unless the layout root shrinks with the keyboard. The
// AndroidManifest sets `windowSoftInputMode="adjustResize"` which shrinks
// the Activity window on every OS version we support — but on older
// WebViews (pre-Chromium M139, mid-2025) `100vh` occasionally lags a frame
// behind the resize. Belt-and-braces: mirror `window.visualViewport.height`
// into a `--vv-h` CSS variable that `#root` reads (see index.html). On
// desktop the value equals `innerHeight` and never changes, so there's zero
// runtime cost.
//
// Fires on:
//   - visualViewport.resize   → keyboard open/close, pinch-zoom
//   - visualViewport.scroll   → URL bar collapse / IME partial slide
//   - window.resize           → window resize (desktop)
//   - window.orientationchange → device rotation (kb usually re-computes)
function syncVvHeight() {
  const vv = window.visualViewport;
  const h = vv ? vv.height : window.innerHeight;
  document.documentElement.style.setProperty("--vv-h", `${h}px`);
}
syncVvHeight();
if (window.visualViewport) {
  window.visualViewport.addEventListener("resize", syncVvHeight);
  window.visualViewport.addEventListener("scroll", syncVvHeight);
}
window.addEventListener("resize", syncVvHeight);
window.addEventListener("orientationchange", syncVvHeight);

ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
  <ErrorBoundary>
    <App />
  </ErrorBoundary>,
);
