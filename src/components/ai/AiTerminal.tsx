import { useEffect, useRef } from "react";
import { Terminal } from "xterm";
import { FitAddon } from "xterm-addon-fit";
import "xterm/css/xterm.css";
import { DEFAULT_FONT_STACK, readFontSize } from "../../util/terminalFont";
import { readScheme } from "../../util/terminalTheme";
import { ai, type AiEvent, type CallRecord } from "../../util/aiApi";
import type { AiFeed } from "../../util/aiEvents";
import { segments } from "../../util/hiddenChars";

// The session's AI terminal: every command the assistant runs in
// the chat open in this session's AI tab, live, with its output. It takes no
// input but Ctrl+C, which stops every run that acts through this session,
// the AI tab's and the AI page's, as their Stop does. Closing it stops
// nothing.

const PROMPT = "\x1b[36mai ▸\x1b[0m ";
const DIM = (s: string) => `\x1b[2m${s}\x1b[0m`;

const HOW: Record<string, string> = {
  read_only: "read-only",
  approved: "approved",
  edited: "edited by you",
  always: "always run",
  auto: "Auto",
  full: "Full access",
};

// What isn't output (a call's line and path, the command the user changed it
// to, the [result] tags) is written as the cards show it: hidden and control
// characters as escapes such as ⟨U+001B⟩, so none of it can move the cursor
// or pass for something else. Only the output is the server's to draw.
const exact = (s: string) => segments(s).map((g) => g.text).join("");
/** One-line text: a line feed in it is an escape too. */
const exactLine = (s: string) => exact(s).replace(/\n/g, "⟨U+000A⟩");
/** Text that may have more lines, each on its own. */
const exactLines = (s: string) => exact(s).replace(/\n/g, "\r\n");

/** The one line a call shows as. */
const lineFor = (tool: string, input: Record<string, unknown>, path: string | null): string => {
  const p = exactLine(path ?? (typeof input.path === "string" ? input.path : ""));
  switch (tool) {
    case "run_command": {
      const c = typeof input.command === "string" ? input.command : "";
      const lines = c.split("\n");
      return lines.length > 1 ? `${exactLine(lines[0])} … (${lines.length} lines)` : exactLine(c);
    }
    case "list_directory": return `list ${p}`;
    case "read_file": return `read ${p}`;
    case "write_file": return `write ${p}`;
    case "edit_file": return `edit ${p}`;
    default: return exactLine(tool);
  }
};

/**
 * The server's output as the terminal shows it. Colours and cursor moves
 * stay; what could act outside the view (titles, links, clipboard, mouse and
 * screen modes) goes. Line breaks become CR LF. One filter per stream: output
 * comes in pieces, and a sequence split between two of them is still caught,
 * while an unfinished one never reaches the terminal. A sequence that goes
 * leaves a CAN in its place: xterm ends any sequence on it and draws nothing,
 * so what is kept on either side can't join into a new one. A scan, not a
 * regex, so text full of unended sequences costs one pass.
 */
const STRING_START = "]P_^X"; // after ESC: OSC, DCS, APC, PM, SOS
const C1_STRING = new Set([0x90, 0x98, 0x9d, 0x9e, 0x9f]);
const C1_CSI = 0x9b; // ESC [ as one character
const CAN = "\x18";
const HELD_MAX = 64;
const isParam = (c: number) => c >= 0x30 && c <= 0x3b; // digits, ':' and ';'
const isFinal = (c: number) => c >= 0x40 && c <= 0x7e;
/** From `j`, past what xterm takes inside a sequence without ending it: C0 controls but CAN, SUB and ESC, and DEL. */
const skipInside = (s: string, j: number) => {
  for (; j < s.length; j++) {
    const c = s.charCodeAt(j);
    if (!((c < 0x20 && c !== 0x18 && c !== 0x1a && c !== 0x1b) || c === 0x7f)) break;
  }
  return j;
};
const outputFilter = () => {
  let held = ""; // the start of a sequence the last piece ended in
  let dropping = false; // inside a string sequence, up to its end
  return (piece: string): string => {
    const s = held + piece;
    held = "";
    const kept: string[] = [];
    let start = 0;
    let i = 0;
    const keep = (to: number) => { if (to > start) kept.push(s.slice(start, to)); };
    // The sequence from i up to `to` goes, a CAN in its place.
    const drop = (to: number) => { keep(i); kept.push(CAN); start = i = to; };
    // The sequence from i is unfinished: it waits for the next piece, or goes
    // when it is too long to be one.
    const hold = () => {
      keep(i);
      if (s.length - i <= HELD_MAX) held = s.slice(i);
      else kept.push(CAN);
      start = i = s.length;
    };
    while (i < s.length) {
      const c = s.charCodeAt(i);
      if (dropping) {
        if (c === 0x07 || c === 0x9c) {
          dropping = false;
          start = ++i;
        } else if (c === 0x1b && i + 1 === s.length) {
          held = "\x1b";
          start = i = s.length;
        } else if (c === 0x1b) {
          // ESC \ ends the string; any other ESC ends it and starts something new.
          dropping = false;
          if (s[i + 1] === "\\") i += 2;
          start = i;
        } else {
          start = ++i;
        }
        continue;
      }
      if (C1_STRING.has(c)) {
        drop(i + 1);
        dropping = true;
        continue;
      }
      if (c !== 0x1b && c !== C1_CSI) {
        i++;
        continue;
      }
      let j = skipInside(s, i + 1);
      if (c === 0x1b) {
        if (j >= s.length) { hold(); continue; }
        if (STRING_START.includes(s[j])) {
          drop(j + 1);
          dropping = true;
          continue;
        }
        if (s[j] !== "[") { i++; continue; }
        j = skipInside(s, j + 1);
      }
      // A CSI. Of these only the private modes go: ESC [ ? … h, l, s or r.
      if (j >= s.length) { hold(); continue; }
      if (s[j] !== "?") { i++; continue; }
      j++;
      while (j < s.length && isParam(s.charCodeAt(j))) j++;
      if (j >= s.length) hold();
      else if ("hlsr".includes(s[j])) drop(j + 1);
      // Cut off before its final byte, by an ESC or a control say: the start
      // goes, and what cut it off is looked at.
      else if (!isFinal(s.charCodeAt(j))) drop(j);
      else i = j + 1;
    }
    keep(s.length);
    return kept.join("").replace(/\r?\n/g, "\r\n");
  };
};
/** Output that is complete, such as a stored record's. */
const forTerminal = (s: string) => outputFilter()(s);

interface Props {
  feed: AiFeed;
  active: boolean;
  onStop: () => void;
}

export const AiTerminal = ({ feed, active, onStop }: Props) => {
  const box = useRef<HTMLDivElement>(null);
  const termRef = useRef<Terminal | null>(null);
  const fitRef = useRef<FitAddon | null>(null);
  const stopRef = useRef(onStop);
  stopRef.current = onStop;

  useEffect(() => {
    if (!box.current) return;
    const term = new Terminal({
      fontSize: readFontSize(),
      fontFamily: DEFAULT_FONT_STACK,
      theme: readScheme().theme,
      scrollback: 10000,
      cursorBlink: false,
      convertEol: false,
      // Links in the output stay text: nothing in this view opens anything.
      linkHandler: { allowNonHttpProtocols: false, activate: () => {} },
    });
    const fit = new FitAddon();
    term.loadAddon(fit);
    term.open(box.current);
    termRef.current = term;
    fitRef.current = fit;
    try { fit.fit(); } catch { /* not laid out yet */ }
    // Only Ctrl+C means anything here.
    const input = term.onData((d) => { if (d === "\x03") stopRef.current(); });

    // What each call has printed so far, so a call shows its line once.
    const shown = new Map<string, { line: string; waiting: boolean }>();
    // The output filter of each call's stream while it runs.
    const filters = new Map<string, (piece: string) => string>();
    const filterFor = (callId: string, stream: string) => {
      const key = `${callId} ${stream}`;
      let f = filters.get(key);
      if (!f) filters.set(key, (f = outputFilter()));
      return f;
    };
    const writeRecord = (r: CallRecord) => {
      const line = lineFor(r.tool, r.input, r.path ?? null);
      const how = r.how ? `  ${DIM(`[${HOW[r.how] ?? r.how}]`)}` : "";
      term.write(`${PROMPT}${line}${how}\r\n`);
      if (r.command) term.write(`${DIM("changed by you to:")} ${exactLines(r.command)}\r\n`);
      if (r.out) term.write(forTerminal(r.out));
      if (r.err) term.write(`\x1b[31m${forTerminal(r.err)}\x1b[0m`);
      if ((r.out || r.err) && !(r.err || r.out || "").endsWith("\n")) term.write("\r\n");
      term.write(`${DIM(`[${exactLine(r.summary || r.state)}]`)}\r\n`);
    };
    const onEvent = (e: AiEvent) => {
      switch (e.type) {
        case "tool_call": {
          const line = lineFor(e.tool, e.input, e.path);
          shown.set(e.call_id, { line, waiting: e.needs === "approval" });
          if (e.needs === "approval") term.write(`${PROMPT}${line}  \x1b[33m[waiting for your approval in the AI tab]\x1b[0m\r\n`);
          break;
        }
        case "tool_running": {
          const s = shown.get(e.call_id);
          const how = DIM(`[${HOW[e.how] ?? e.how}]`);
          if (s?.waiting) term.write(`${DIM("↳")} ${how}\r\n`);
          else term.write(`${PROMPT}${s?.line ?? ""}  ${how}\r\n`);
          if (e.command) term.write(`${DIM("changed by you to:")} ${exactLines(e.command)}\r\n`);
          shown.set(e.call_id, { line: s?.line ?? "", waiting: false });
          break;
        }
        case "tool_output": {
          const t = filterFor(e.call_id, e.stream)(e.text);
          term.write(e.stream === "err" ? `\x1b[31m${t}\x1b[0m` : t);
          break;
        }
        case "tool_result": {
          filters.delete(`${e.call_id} out`);
          filters.delete(`${e.call_id} err`);
          const r = e.record;
          const s = shown.get(e.call_id);
          if (!r.how && !s?.waiting) term.write(`${PROMPT}${s?.line ?? lineFor(r.tool, r.input, r.path ?? null)}\r\n`);
          const bad = r.state !== "done";
          term.write(`${bad ? "\x1b[31m" : "\x1b[2m"}[${exactLine(r.summary || r.state)}]\x1b[0m\r\n`);
          // A later call with the same id shows its own line.
          shown.delete(e.call_id);
          break;
        }
        case "run_finished":
          shown.clear();
          filters.clear();
          if (e.stop === "stopped") term.write(`${DIM("[stopped]")}\r\n`);
          break;
        default:
          break;
      }
    };

    // Earlier commands of the chat first, then the current run so far, then
    // live. Calls of the current run are left out of the replay: the run's
    // own events carry them.
    let gone = false;
    let unsubscribe = () => {};
    const startOver = async (chatId: number | null) => {
      term.reset();
      shown.clear();
      filters.clear();
      if (chatId == null) return;
      let records: CallRecord[] = [];
      try { records = await ai.terminalReplay(chatId); } catch { /* the chat may be gone */ }
      if (gone || feed.chatId() !== chatId) return;
      const current = feed.current();
      const inRun = new Set(current.flatMap((e) => (e.type === "tool_call" || e.type === "tool_result" ? [e.call_id] : [])));
      const earlier = records.filter((r) => !inRun.has(r.id));
      if (earlier.length) {
        term.write(`${DIM("── earlier in this chat ──")}\r\n`);
        earlier.forEach(writeRecord);
        term.write(`${DIM("──")}\r\n`);
      }
      current.forEach(onEvent);
    };
    void startOver(feed.chatId()).then(() => {
      if (!gone) unsubscribe = feed.subscribe(onEvent);
    });
    const offChat = feed.onChat((id) => { void startOver(id); });

    const ro = new ResizeObserver(() => { try { fit.fit(); } catch { /* hidden */ } });
    ro.observe(box.current);
    return () => {
      gone = true;
      unsubscribe();
      offChat();
      ro.disconnect();
      input.dispose();
      term.dispose();
      termRef.current = null;
    };
  }, [feed]);

  useEffect(() => {
    if (!active) return;
    const t = setTimeout(() => { try { fitRef.current?.fit(); } catch { /* hidden */ } }, 50);
    return () => clearTimeout(t);
  }, [active]);

  return (
    <div className="h-full w-full flex flex-col bg-zinc-950">
      <div className="shrink-0 px-3 py-1 text-[10.5px] text-zinc-500 border-b border-white/5">
        The assistant's commands and their output. Read-only: Ctrl+C stops the assistant in this session.
      </div>
      <div ref={box} className="flex-1 min-h-0 px-1 py-1" />
    </div>
  );
};
