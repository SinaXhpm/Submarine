// What of a paste may reach the program in the terminal. A paste is text the
// user didn't type: from a page, a chat, a document. Left whole, it can carry
// the end marker of a bracketed paste (ESC [ 2 0 1 ~) and everything after it
// is read as typed input, which runs; or any other escape sequence or control
// character the program takes as a key (Ctrl+C, Ctrl+D, Ctrl+U, an arrow
// key). Tab, line feed and carriage return are text and stay; the other
// control characters (C0, DEL and the C1 range) are left out, as terminal
// emulators generally do for a paste.
const CONTROLS = /[\u0000-\u0008\u000b\u000c\u000e-\u001f\u007f-\u009f]/g;

/** `text` without its control characters (tab, line feed and carriage return stay). */
export function withoutPasteControls(text: string): string {
  return text.replace(CONTROLS, "");
}
