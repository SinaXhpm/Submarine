import { t } from "../i18n";
import type { ConfirmOptions } from "../ui/confirm";
import { inspect, stripHidden } from "./hiddenChars";

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

// Text for a terminal from outside it (the Library's Run and Paste): it goes
// the way a paste does, and `enter` presses Enter after it, in the same write.
// It reaches this terminal only, whatever broadcast is set to. A plain write
// would skip the brackets, and every line break in it would be an Enter.
export const TERMINAL_PASTE = "submarine-terminal-paste";
export interface TerminalPaste { terminalId: string; text: string; enter: boolean; }

export function pasteIntoTerminal(detail: TerminalPaste) {
  window.dispatchEvent(new CustomEvent<TerminalPaste>(TERMINAL_PASTE, { detail }));
}

export interface WriteToTerminal {
  terminalId: string;
  text: string;
  /** Press Enter after it, so the shell runs it; false leaves it at the prompt. */
  enter: boolean;
  /** The caller showed this line cut short (a Library card): show it whole first. */
  cut?: boolean;
  /** The caller's `useConfirm()`, which asks the user. */
  confirm: (opts: ConfirmOptions) => Promise<boolean>;
}

/**
 * Put text in a terminal as a paste, bracketed when the program there asked
 * for that: the Library's Run and Paste, and the AI's "Insert into terminal"
 * (always `enter: false`). Such text isn't typed here (a Library in a shared
 * profile can be anyone's, a reply is the model's) and the caller may show
 * only the start of it. So no control character or other hidden one goes in
 * (util/hiddenChars), and what can't be seen whole is shown whole first: more
 * than one line, a run of 16 or more blanks, or a line to be run that the
 * caller cut short (`cut`; a line only pasted can be read at the prompt).
 * A shell without bracketed paste (plain sh, BusyBox, an older bash) runs
 * each line of a paste as it lands, so for text only to be pasted the
 * terminal asks once more there (TerminalView). True when the text was sent
 * to the terminal.
 */
export async function writeToTerminal({ terminalId, text, enter, cut = false, confirm }: WriteToTerminal): Promise<boolean> {
  if (!terminalId) return false;
  // A loop, not /\n+$/: that regex retries from every line break of a long
  // run that isn't at the end, and a reply can hold 200,000 of them.
  let clean = stripHidden(text);
  let end = clean.length;
  while (end > 0 && clean.charCodeAt(end - 1) === 0x0a) end--;
  clean = clean.slice(0, end);
  if (!clean) return false;
  const lines = clean.split("\n").length;
  if (lines > 1 || cut || inspect(clean).longBlanks) {
    const ok = await confirm({
      title: enter
        ? (lines > 1 ? t("Run these {count} lines?", { count: lines }) : t("Run this line?"))
        : (lines > 1 ? t("Paste these {count} lines?", { count: lines }) : t("Paste this line?")),
      message: clean,
      okLabel: enter ? t("Run") : t("Paste"),
      safeDefault: true,
    });
    if (!ok) return false;
  }
  pasteIntoTerminal({ terminalId, text: clean, enter });
  return true;
}
