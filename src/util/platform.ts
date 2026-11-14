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

// iPhone and iPad. An iPad's WebView says "Macintosh" like a Mac's does (it
// asks for desktop pages), so it is told apart by its touch screen: a Mac has
// none.
export const IS_IOS: boolean =
  typeof navigator !== "undefined" &&
  (/iPhone|iPad|iPod/i.test(navigator.userAgent) ||
    (/Macintosh/i.test(navigator.userAgent) && navigator.maxTouchPoints > 1));

// A phone or tablet: no file dialogs, no "open with", no window to move or
// resize, no ssh-agent or ~/.ssh/config. (The layout itself follows the
// window's width, not this.)
export const IS_MOBILE: boolean = IS_ANDROID || IS_IOS;

// Desktop Linux, where the webview is WebKitGTK. (Android's user agent says
// "Linux" too, so it is ruled out.) Clipboard reads go through the backend
// there — see TerminalView.
export const IS_LINUX: boolean =
  typeof navigator !== "undefined" && /Linux/i.test(navigator.userAgent) && !IS_ANDROID;

// Windows, where drives have letters (an RDP server may be given a whole one).
export const IS_WINDOWS: boolean =
  typeof navigator !== "undefined" && /Windows/i.test(navigator.userAgent);

// True when we're inside a Tauri WebView (desktop or Android). Kept for future
// use — some UI may want to differentiate a browser preview from packaged app.
export const IS_TAURI: boolean =
  typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
