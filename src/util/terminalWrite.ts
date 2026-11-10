import { invoke } from "@tauri-apps/api/core";

// What is typed into a terminal, on its way to the shell. By terminal, not by
// view: a Wall tile and its tab write to the same shell, and so does broadcast.
//
// One write at a time, what comes meanwhile going out together in the next: each
// invoke is a request of its own, nothing keeps two in order, and keys a few ms
// apart could reach the shell swapped.
//
// Held while the shell is opening: the backend has nowhere to put input until the
// channel is open and would drop it, and the terminal takes keys before that. They
// go out once it's open, as a server's own terminal keeps what's typed ahead.
//
// Opening a shell starts a new writer. What the old one still has, or is stuck
// sending on a connection that hung, was typed for the old shell: a command typed
// into a stalled session must not run by itself in the new one after a reconnect.
type Writer = { chunks: number[][]; sending: boolean; held: boolean };

const writers = new Map<string, Writer>();

const pump = (terminalId: string, w: Writer) => {
  if (writers.get(terminalId) !== w || w.sending || w.held) return;
  if (w.chunks.length === 0) {
    writers.delete(terminalId);
    return;
  }
  const data = w.chunks.length === 1 ? w.chunks[0] : w.chunks.flat();
  w.chunks = [];
  w.sending = true;
  invoke("write_terminal_data", { terminalId, data })
    .catch(console.error)
    .finally(() => {
      w.sending = false;
      pump(terminalId, w);
    });
};

export const writeTerminal = (terminalId: string, bytes: number[]) => {
  let w = writers.get(terminalId);
  if (!w) {
    w = { chunks: [], sending: false, held: false };
    writers.set(terminalId, w);
  }
  w.chunks.push(bytes);
  pump(terminalId, w);
};

/**
 * The shell is about to be opened, or opened again: keep what's typed. Call what
 * this returns when opening ended. What was kept goes to the shell, or goes away if
 * there is none. Only the first call counts: a view that goes away while its shell
 * opens lets go with false, and that shell gets nothing when it opens after all.
 * It does nothing to a later opening's input.
 */
export const holdTerminalInput = (terminalId: string) => {
  const w: Writer = { chunks: [], sending: false, held: true };
  writers.set(terminalId, w);
  return (opened: boolean) => {
    if (!w.held) return;
    w.held = false;
    if (!opened) w.chunks = [];
    pump(terminalId, w);
  };
};
