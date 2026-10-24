// The built-in RDP client's shared clipboard (rdp_client.rs, `Cmd::Paste`):
// this computer's text goes to the server only when the user pastes it there,
// with a paste key in the tab or its Paste button. The tab holds that key back,
// reads the clipboard, hands both to the backend, and the backend presses the
// key on the server once the text is on the server's clipboard.
// The VNC tab (VncSessionView) takes the same paste keys and reads the
// clipboard the same way; it sends the key itself, after the text.

import { invoke } from "@tauri-apps/api/core";
import { IS_ANDROID, IS_LINUX } from "./platform";

/** The paste keys the backend knows; kept equal to `rdp_client::PasteKeys`. */
export type PasteKeys = "none" | "ctrlv" | "shiftinsert" | "cmdv";

export const IS_MAC: boolean =
  typeof navigator !== "undefined" && /Mac/i.test(navigator.platform || navigator.userAgent) && !IS_ANDROID;

/** The paste a key press asks for, if it is one: Ctrl+V, Shift+Insert, and
 *  on a Mac Cmd+V (which the server would take for Win+V). */
export function pasteKeysOf(
  e: { code: string; ctrlKey: boolean; shiftKey: boolean; altKey: boolean; metaKey: boolean },
  mac: boolean = IS_MAC,
): PasteKeys | null {
  const only = (ctrl: boolean, shift: boolean, meta: boolean) => e.ctrlKey === ctrl && e.shiftKey === shift && e.metaKey === meta && !e.altKey;
  if (e.code === "KeyV" && only(true, false, false)) return "ctrlv";
  if (e.code === "Insert" && only(false, true, false)) return "shiftinsert";
  if (mac && e.code === "KeyV" && only(false, false, true)) return "cmdv";
  return null;
}

/** This computer's clipboard text. On Linux the backend reads it (the
 *  webview crashes on an empty clipboard there, see TerminalView). */
export function readLocalClipboard(): Promise<string> {
  return IS_LINUX ? invoke<string>("clipboard_read_text") : navigator.clipboard.readText();
}
