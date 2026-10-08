// Runtime platform detection. Tauri v2 has no synchronous OS query on the JS
// side, so we sniff the user agent — the WebView UA on Android always contains
// "Android" and on desktop Tauri never does. Kept as a module constant so
// components read it once at import time and don't burn cycles on every render.
//
// Why not `useIsNarrow`? A small desktop window is still desktop — folder
// pickers, "Open with default app", and ~/.ssh/config all work on a 400px-wide
// Windows build. Only Android truly lacks these capabilities.
export const IS_ANDROID: boolean =
  typeof navigator !== "undefined" && /Android/i.test(navigator.userAgent);

// Desktop Linux, where the webview is WebKitGTK. (Android's user agent says
// "Linux" too, so it is ruled out.) Clipboard reads go through the backend
// there — see TerminalView.
export const IS_LINUX: boolean =
  typeof navigator !== "undefined" && /Linux/i.test(navigator.userAgent) && !IS_ANDROID;

// True when we're inside a Tauri WebView (desktop or Android). Kept for future
// use — some UI may want to differentiate a browser preview from packaged app.
export const IS_TAURI: boolean =
  typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
