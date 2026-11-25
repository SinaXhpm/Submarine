import { invoke } from "@tauri-apps/api/core";
import { IS_ANDROID, IS_LINUX, IS_WINDOWS } from "./platform";

// Copy and paste for the whole app. The page's own clipboard API
// (navigator.clipboard) isn't usable everywhere:
// - Android's WebView refuses it: a paste said "Clipboard read denied" and the
//   copy buttons did nothing. There the app's Android plugin uses the system
//   clipboard (lib.rs clipboard_read_text / clipboard_write_text).
// - WebView2 on Windows asks the user to allow the page's read, and after a
//   Block every paste said "Clipboard read denied": reads go through the
//   backend.
// - WebKitGTK on Linux refuses the page's read, and the webview's own read can
//   crash the app on an empty clipboard (#89): reads go through the backend.
// Elsewhere the page's API works.

/** Puts `text` on the clipboard. Rejects when it couldn't. */
export function copyText(text: string): Promise<void> {
  if (IS_ANDROID) return invoke<void>("clipboard_write_text", { text });
  if (!navigator.clipboard) return Promise.reject(new Error("No clipboard here"));
  return navigator.clipboard.writeText(text);
}

/** The clipboard's text ("" when it holds none). Rejects when it can't be read. */
export function readClipboardText(): Promise<string> {
  if (IS_ANDROID || IS_LINUX || IS_WINDOWS) return invoke<string>("clipboard_read_text");
  if (!navigator.clipboard) return Promise.reject(new Error("No clipboard here"));
  return navigator.clipboard.readText();
}
