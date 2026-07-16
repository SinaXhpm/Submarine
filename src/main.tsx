import React from "react";
import ReactDOM from "react-dom/client";
import App from "./App";
import ErrorBoundary from "./components/ErrorBoundary";
import "./App.css";

// Last-resort global handlers so a failure that escapes React (an async reject,
// a listener throw) is at least recorded instead of vanishing silently. The
// ErrorBoundary below covers render-time throws; these cover everything else.
window.addEventListener("unhandledrejection", (e) => {
  console.error("[app] unhandled promise rejection:", e.reason);
});
window.addEventListener("error", (e) => {
  console.error("[app] uncaught error:", e.error ?? e.message);
});

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
