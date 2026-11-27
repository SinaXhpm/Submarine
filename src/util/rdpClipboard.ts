// The built-in RDP client's shared clipboard (rdp_client.rs, `Cmd::Paste`):
// this computer's text goes to the server only when the user pastes it there,
// with a paste key in the tab or its Paste button. The tab holds that key back,
// reads the clipboard, hands both to the backend, and the backend presses the
// key on the server once the text is on the server's clipboard.
// The VNC tab (VncSessionView) takes the same paste keys and reads the
// clipboard the same way; it sends the key itself, after the text.
// When the user copied on the server last, both send the key alone
// (`ServerCopyWatch`).

import { invoke } from "@tauri-apps/api/core";
import { IS_LINUX, IS_MAC } from "./platform";
import { copyText, readClipboardText } from "./clipboard";

/** The paste keys the backend knows; kept equal to `rdp_client::PasteKeys`. */
export type PasteKeys = "none" | "ctrlv" | "shiftinsert" | "cmdv";

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

/** Text the user copied on a server. It goes on this device's clipboard only
 *  while the server's tab is in front and the app has the focus: from behind
 *  another tab, or while the user works in another app (a phone's split
 *  screen keeps both shown), a server could replace what the user just copied
 *  there, and have its own text pasted into another server. Otherwise, or
 *  when the webview refuses (one that wants a click for it), `hold` keeps it
 *  for the tab's "Copy it here" bar. True when it went on the clipboard. */
export async function takeServerCopy(text: string, inFront: boolean, hold: (text: string | null) => void): Promise<boolean> {
  if (!inFront || !document.hasFocus()) {
    hold(text);
    return false;
  }
  try {
    await copyText(text);
  } catch {
    hold(text);
    return false;
  }
  hold(null);
  return true;
}

/** This device's clipboard text. On Windows, Linux and Android the backend
 *  reads it (see util/clipboard). */
export function readLocalClipboard(): Promise<string> {
  return readClipboardText();
}

/** This device's clipboard mark now (rdp_builtin_files_mark): it changes
 *  when anything is copied here. None where that can't be told (Linux, phones). */
export const clipboardMark = () => invoke<number | null>("rdp_builtin_files_mark").catch(() => null);

/** A short digest of clipboard text, kept in its place: enough to tell later
 *  whether the clipboard still holds the same text. */
function digest(text: string): string {
  let a = 0x811c9dc5;
  let b = 0x9747b28c;
  for (let i = 0; i < text.length; i++) {
    const c = text.charCodeAt(i);
    a = Math.imul(a ^ c, 0x01000193);
    b = Math.imul(b ^ c, 0x5bd1e995);
  }
  return `${text.length}:${(a >>> 0).toString(36)}:${(b >>> 0).toString(36)}`;
}

/** This device's clipboard as it was when the server's became the newer one:
 *  its mark, or where none can be had, its text's digest (if known). */
type Seen = { mark: number | null; text: string | null };

/** Whether a paste key needs this device's clipboard. When the user copied
 *  on the server last and nothing was copied here since, the key goes alone
 *  and the server pastes its own copy: an image, cells or files stay what
 *  they are there, where this device's text (or a copy of the files) would
 *  take their place. Only a copy made with its tab in front and the app
 *  focused counts: one a program on the server made, or the clipboard it had
 *  when the tab connected, leaves this device's clipboard the one to paste. */
export class ServerCopyWatch {
  private seen: Promise<Seen | null> = Promise.resolve(null);
  // This device's text as the tab last put it here or sent it (a digest),
  // for where neither a mark nor a quiet read tells (phones).
  private known: string | null = null;

  /** The user copied on the server: this device's clipboard as it is now. */
  serverCopied(): void {
    const known = this.known;
    this.seen = clipboardMark().then(async (mark) => {
      if (mark !== null) return { mark, text: null };
      // Linux reads its clipboard without a word; Android says so on screen
      // each time, and iOS asks first.
      if (IS_LINUX) return { mark: null, text: await readClipboardText().then(digest, () => null) };
      return { mark: null, text: known };
    });
  }

  /** The server's copy went on this device's clipboard: its text, or null for files. */
  copiedHere(text: string | null): void {
    const known = text === null ? null : digest(text);
    this.known = known;
    this.seen = clipboardMark().then((mark) => ({ mark, text: mark === null ? known : null }));
  }

  /** This device's text (null: its files) went to the server's clipboard. */
  sentFromHere(text: string | null): void {
    this.known = text === null ? null : digest(text);
    this.seen = Promise.resolve(null);
  }

  /** A new connection, or a copy on the server that doesn't count. */
  forget(): void {
    this.seen = Promise.resolve(null);
  }

  /** Nothing was copied here since, as the clipboard's mark tells. */
  async sameMark(): Promise<boolean> {
    const seen = await this.seen;
    return seen !== null && seen.mark !== null && (await clipboardMark()) === seen.mark;
  }

  /** This device's clipboard holds the same text as then, where no mark tells. */
  async sameText(text: string): Promise<boolean> {
    const seen = await this.seen;
    return seen !== null && seen.mark === null && seen.text !== null && seen.text === digest(text);
  }
}
