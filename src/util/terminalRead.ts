// Reading a terminal from outside it, as the AI's composer does to attach
// terminal text: its selection, or the last lines of its buffer with the
// scrollback. A request and its reply are window events, as a paste from
// outside is (util/pasteText); the TerminalView that owns the terminal's PTY
// answers, at once. A terminal in a hidden tab still takes in its output, so
// its last lines are current.
export const TERMINAL_READ = "submarine-terminal-read";
export const TERMINAL_READ_REPLY = "submarine-terminal-read-reply";

export type TerminalReadWhat = "selection" | "tail";

export interface TerminalRead {
  requestId: number;
  terminalId: string;
  what: TerminalReadWhat;
  /** For "tail": how many lines, a wrapped line counting as one. */
  lines: number;
}

export interface TerminalReadReply {
  requestId: number;
  terminalId: string;
  text: string;
}

let nextRequest = 1;

/**
 * The text of terminal `terminalId`: its selection, or its last `lines` lines
 * without the blank ones at the end. Null when no terminal by that id answers
 * within `timeoutMs`.
 */
export function readTerminal(terminalId: string, what: TerminalReadWhat, lines = 200, timeoutMs = 1000): Promise<string | null> {
  const requestId = nextRequest++;
  return new Promise((resolve) => {
    const onReply = (e: Event) => {
      const reply = (e as CustomEvent<TerminalReadReply>).detail;
      if (reply?.requestId === requestId && reply.terminalId === terminalId) done(reply.text);
    };
    const done = (text: string | null) => {
      window.removeEventListener(TERMINAL_READ_REPLY, onReply);
      window.clearTimeout(timer);
      resolve(text);
    };
    window.addEventListener(TERMINAL_READ_REPLY, onReply);
    const timer = window.setTimeout(() => done(null), timeoutMs);
    window.dispatchEvent(new CustomEvent<TerminalRead>(TERMINAL_READ, { detail: { requestId, terminalId, what, lines } }));
  });
}
