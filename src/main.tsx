import React from "react";
import ReactDOM from "react-dom/client";
import App from "./App";
import ErrorBoundary from "./components/ErrorBoundary";
import { IS_IOS, IS_MOBILE } from "./util/platform";
import { initI18n } from "./i18n";
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

// The webview's own right-click menu has a Reload too, so it stays shut, in
// the capture phase as above. Only the default is cancelled: the app's own
// menus and the terminal's paste get the event as before. Two places keep it,
// and neither has a Reload there: a text field (paste, spell check) and
// selected text (Copy). A phone's webview has no such menu: a long press
// selects text there, and cancelling this could cost it its Copy bar.
const NOT_TEXT_INPUT = /^(checkbox|radio|range|color|file|button|submit|reset|image)$/;
const inTextField = (target: EventTarget | null) => {
  const field = target instanceof Element ? target.closest("input, textarea, [contenteditable]") : null;
  return !!field && !(field instanceof HTMLInputElement && NOT_TEXT_INPUT.test(field.type));
};
const onSelectedText = (e: MouseEvent) => {
  const selection = window.getSelection();
  if (!selection || selection.isCollapsed) return false;
  return Array.from(selection.getRangeAt(0).getClientRects()).some((r) =>
    e.clientX >= r.left && e.clientX <= r.right && e.clientY >= r.top && e.clientY <= r.bottom);
};
if (!IS_MOBILE) {
  window.addEventListener("contextmenu", (e) => {
    if (!inTextField(e.target) && !onSelectedText(e)) e.preventDefault();
  }, true);
}

// ── iOS: the page stays inside the safe area ────────────────────────────────
// index.html asks for `viewport-fit=cover`, for Android's insets (the gesture
// pill under MobileKeyBar and the Sidebar dock). On iPhone and iPad that lays
// the page out under the status bar and the Dynamic Island too, and only the
// bottom bars pad for it: the session bar, panels and menus at the top went
// under the clock. Without it, WKWebView keeps the whole page inside the safe
// area (fixed panels and menus included) and the insets read 0.
if (IS_IOS) {
  const viewport = document.querySelector('meta[name="viewport"]');
  const content = viewport?.getAttribute("content");
  if (viewport && content) viewport.setAttribute("content", content.replace(/,\s*viewport-fit=cover/, ""));
}

// ── Android soft-keyboard sizing (visualViewport → #root's height) ──────────
// The Android on-screen keyboard covers our fixed-bottom UI (MobileKeyBar,
// Sidebar dock) unless the layout root shrinks with the keyboard. The
// AndroidManifest sets `windowSoftInputMode="adjustResize"` which shrinks
// the Activity window on every OS version we support — but on older
// WebViews (pre-Chromium M139, mid-2025) `100vh` occasionally lags a frame
// behind the resize. Belt-and-braces: mirror `window.visualViewport.height`
// into `#root`'s height (index.html has the first value). On desktop the
// value equals `innerHeight` and never changes, so there's zero runtime
// cost. Set on `#root` itself: an inherited CSS variable on <html> made the
// browser look at the style of every element in the document each time.
//
// Fires on:
//   - visualViewport.resize   → keyboard open/close, pinch-zoom
//   - visualViewport.scroll   → URL bar collapse / IME partial slide
//   - window.resize           → window resize (desktop)
//   - window.orientationchange → device rotation (kb usually re-computes)
const root = document.getElementById("root") as HTMLElement;
function syncVvHeight() {
  const vv = window.visualViewport;
  const h = vv ? vv.height : window.innerHeight;
  root.style.height = `${h}px`;
}
syncVvHeight();
if (window.visualViewport) {
  window.visualViewport.addEventListener("resize", syncVvHeight);
  window.visualViewport.addEventListener("scroll", syncVvHeight);
}
window.addEventListener("resize", syncVvHeight);
window.addEventListener("orientationchange", syncVvHeight);

// The device's language is loaded before the first screen, so the app never
// shows English first (src/i18n). It renders even if that fails.
void initI18n().finally(() => {
  ReactDOM.createRoot(root).render(
    <ErrorBoundary>
      <App />
    </ErrorBoundary>,
  );
});
