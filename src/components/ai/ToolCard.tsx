import { useEffect, useMemo, useRef, useState } from "react";
import { Check, ChevronDown, Copy, FileEdit, FileText, Folder, ShieldAlert, Square, TerminalSquare, Trash2, Undo2, X, Pencil } from "lucide-react";
import { ExactSegments, ExactText, HiddenWarnings } from "./ExactText";
import { LineDiff } from "./LineDiff";
import { inspect, segments } from "../../util/hiddenChars";
import type { Answer, CallRecord, Preview } from "../../util/aiApi";

// One tool call in the chat: what it does, exactly; the buttons
// while it waits for the user; its output while it runs and after. No key
// approves a call, and no button takes focus by itself, so typing can't
// approve anything by accident.

export interface LiveCall {
  id: string;
  tool: string;
  input: Record<string, unknown>;
  needs: "none" | "approval";
  reason: string | null;
  red: string | null;
  path: string | null;
  preview: Preview | null;
  /** Set once it runs. */
  how: string | null;
  /** The command that runs, when the user changed it. */
  command: string | null;
  out: string;
  err: string;
  started: number | null;
  /** The final record, once it is answered. */
  record: CallRecord | null;
}

/** What the page keeps of a running command's output. */
export const LIVE_CAP = 1024 * 1024;

export const fromRecord = (r: CallRecord): LiveCall => ({
  id: r.id,
  tool: r.tool,
  input: r.input,
  needs: "none",
  reason: null,
  red: r.red ?? null,
  path: r.path ?? null,
  preview: r.preview ?? null,
  how: r.how ?? null,
  command: r.command ?? null,
  out: r.out ?? "",
  err: r.err ?? "",
  started: null,
  record: r,
});

const HOW: Record<string, string> = {
  read_only: "read-only",
  approved: "approved",
  edited: "edited by you",
  always: "always run",
  auto: "Auto",
  full: "Full access",
};

/** Terminal colour codes and other escapes, out of the card's plain text. */
// eslint-disable-next-line no-control-regex
const stripAnsi = (s: string) => s.replace(/\x1b\[[0-?]*[ -/]*[@-~]|\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)|\x1b[@-Z\\-_]/g, "");

const str = (v: unknown) => (typeof v === "string" ? v : "");

const size = (n: number) => (n < 1024 ? `${n} B` : n < 1024 * 1024 ? `${(n / 1024).toFixed(1)} KB` : `${(n / 1024 / 1024).toFixed(1)} MB`);

// A command's box stops growing at this height and scrolls, so that a card
// that waits fits in the view with its buttons.
const COMMAND_BOX = "40vh";

// The lines of a text as its box draws them: a line feed at the end ends the
// last line, it doesn't start another.
const linesIn = (s: string) => {
  let n = s && !s.endsWith("\n") ? 1 : 0;
  for (let i = 0; i < s.length; i++) if (s.charCodeAt(i) === 0x0a) n++;
  return n;
};

interface Props {
  call: LiveCall;
  /** False once the run is over: nothing waits any more. */
  live: boolean;
  /** "Always run this exact command on this server" is offered (at Ask, not on a red card). */
  canAlways: boolean;
  onDecide: (answer: Answer) => Promise<void>;
  onStop?: () => void;
  /** `path`: the file as the card shows it, which Undo checks against its log entry. */
  onUndo?: (actionId: number, path: string) => void;
  /** This change was undone: it can't be undone again. */
  undone?: boolean;
}

export const ToolCard = ({ call, live, canAlways, onDecide, onStop, onUndo, undone }: Props) => {
  const waiting = live && call.needs === "approval" && !call.how && !call.record;
  const running = live && !!call.how && !call.record;
  const r = call.record;
  const isCommand = call.tool === "run_command";
  const isChange = call.tool === "write_file" || call.tool === "edit_file";
  const command = str(call.input.command);
  // The path as the server resolved it, once known; else the one asked for.
  const path = call.path || str(call.input.path);
  const [mode, setMode] = useState<"buttons" | "edit" | "deny" | "again">("buttons");
  const [edited, setEdited] = useState(command);
  const [note, setNote] = useState("");
  const [busy, setBusy] = useState(false);
  const [open, setOpen] = useState(false);
  const [full, setFull] = useState(false);
  const [now, setNow] = useState(Date.now());
  const box = useRef<HTMLPreElement>(null);
  const lineCount = useMemo(() => linesIn(command), [command]);

  useEffect(() => {
    if (!running) return;
    const t = setInterval(() => setNow(Date.now()), 500);
    return () => clearInterval(t);
  }, [running]);

  // A card that starts waiting shows its command from the top: the start of
  // what the user approves is what they read first.
  useEffect(() => {
    if (waiting && box.current) box.current.scrollTop = 0;
  }, [waiting]);

  const decide = async (a: Answer) => {
    setBusy(true);
    try { await onDecide(a); } finally { setBusy(false); }
  };

  // delete_file is an agent's (the gateway), never the assistant's.
  const isDelete = call.tool === "delete_file";
  const Icon = isCommand ? TerminalSquare : call.tool === "list_directory" ? Folder : isChange ? FileEdit : isDelete ? Trash2 : FileText;
  const label = isCommand ? "Command" : call.tool === "list_directory" ? "List folder" : call.tool === "read_file" ? "Read file" : call.tool === "write_file" ? "Write file" : call.tool === "edit_file" ? "Edit file" : isDelete ? "Delete file" : call.tool;
  const red = !!call.red && (waiting || mode === "again");
  const border = red ? "border-rose-500/50" : waiting ? "border-amber-500/40" : "border-white/10";
  const output = stripAnsi(call.out + (call.err ? (call.out && !call.out.endsWith("\n") ? "\n" : "") + call.err : ""));
  const lines = output.split("\n");
  const shown = full ? output : lines.slice(0, 20).join("\n");
  const state = r?.state;
  const stateText = !r ? null : state === "done" ? null : state === "denied" ? "denied" : state === "refused" ? "refused" : state === "skipped" ? "skipped" : state === "stopped" ? "stopped" : state === "interrupted" ? "interrupted" : "failed";

  // A read or a listing that ran by itself is one line; it opens to show
  // exactly what was sent to the assistant.
  if (!isCommand && !isChange && !waiting && r) {
    return (
      <div className="rounded-lg border border-white/5 bg-white/[0.02] text-[12px]">
        <button type="button" onClick={() => setOpen((o) => !o)} className="w-full flex items-center gap-2 px-2.5 py-3 sm:py-1.5 text-left text-zinc-400 hover:text-zinc-200">
          <Icon size={13} className="shrink-0" />
          <span className="truncate font-mono text-[11.5px]" dir="ltr"><ExactSegments segs={segments(r.summary || path)} /></span>
          {stateText && <span className="text-rose-300 shrink-0">· {stateText}</span>}
          <ChevronDown size={12} className={`ml-auto shrink-0 transition-transform ${open ? "" : "-rotate-90"}`} />
        </button>
        {open && <pre className="max-h-72 overflow-auto px-2.5 pb-2 text-[11.5px] text-zinc-400 whitespace-pre-wrap break-all" dir="ltr">{r.result}</pre>}
      </div>
    );
  }

  // How many lines a command has, by its buttons: a tall one scrolls in its box.
  const linesNote = isCommand && lineCount > 1 ? <span className="self-center text-[11px] text-amber-300/90">{lineCount} lines</span> : null;

  return (
    <div data-waiting={waiting || undefined} className={`rounded-xl border ${border} bg-white/[0.02] text-[12.5px] overflow-hidden`}>
      <div className="flex items-center gap-2 px-3 py-2 border-b border-white/5">
        <Icon size={14} className={red ? "text-rose-300" : "text-zinc-400"} />
        <span className="font-semibold text-zinc-200">{label}</span>
        {call.how && <span className="text-[10.5px] px-1.5 py-0.5 rounded bg-white/5 text-zinc-400">{HOW[call.how] ?? call.how}</span>}
        {stateText && <span className="text-[10.5px] px-1.5 py-0.5 rounded bg-rose-500/10 text-rose-300">{stateText}</span>}
        {isCommand && <span className="ml-auto text-[10.5px] text-zinc-500">timeout {Number(call.input.timeout_seconds ?? 60)} s</span>}
      </div>

      <div className="px-3 py-2 space-y-2">
        {isCommand && <ExactText text={command} maxHeight={COMMAND_BOX} boxRef={box} />}
        {call.command && call.command !== command && (
          <div className="space-y-1">
            <div className="text-[11px] text-zinc-500">Changed by you to:</div>
            <ExactText text={call.command} maxHeight={COMMAND_BOX} />
          </div>
        )}
        {!isCommand && path && (
          <div>
            <div className="font-mono text-[11.5px] text-zinc-300 whitespace-pre-wrap break-all" dir="ltr"><ExactSegments segs={segments(path)} /></div>
            <HiddenWarnings found={inspect(path)} what="path" />
          </div>
        )}
        {isChange && call.preview && (
          <div className="space-y-1">
            <div className="text-[11px] text-zinc-500">
              {call.preview.old === null ? `New file · ${size(call.preview.new.length)}` : `${size(call.preview.old.length)} → ${size(call.preview.new.length)}`}
            </div>
            <LineDiff oldText={call.preview.old} newText={call.preview.new} />
          </div>
        )}

        {waiting && (
          <div className="space-y-2">
            {call.red ? (
              <div className="flex items-center gap-1.5 text-[11.5px] text-rose-300"><ShieldAlert size={13} /> Always asks: {call.red}</div>
            ) : (
              call.reason && <div className="text-[11.5px] text-amber-300/90">{call.reason}</div>
            )}
            {mode === "buttons" && (
              <div className="flex flex-wrap gap-2">
                <button type="button" disabled={busy} onClick={() => (call.red ? setMode("again") : void decide({ decision: "allow", always: false }))}
                  className={`h-10 sm:h-8 px-3 rounded-lg text-[11px] font-bold uppercase border flex items-center gap-1.5 ${call.red ? "border-rose-500/40 text-rose-200 hover:bg-rose-500/10" : "border-primary/40 text-primary hover:bg-primary/10"}`}>
                  <Check size={12} /> {isChange ? "Apply" : isCommand ? "Run" : isDelete ? "Delete" : "Allow"}
                </button>
                {isCommand && (
                  <button type="button" disabled={busy} onClick={() => { setEdited(command); setMode("edit"); }} className="h-10 sm:h-8 px-3 rounded-lg text-[11px] font-bold uppercase border border-white/10 text-zinc-300 hover:bg-white/5 flex items-center gap-1.5">
                    <Pencil size={12} /> Edit
                  </button>
                )}
                <button type="button" disabled={busy} onClick={() => setMode("deny")} className="h-10 sm:h-8 px-3 rounded-lg text-[11px] font-bold uppercase border border-white/10 text-zinc-300 hover:bg-white/5 flex items-center gap-1.5">
                  <X size={12} /> Deny
                </button>
                {isCommand && canAlways && !call.red && (
                  <button type="button" disabled={busy} onClick={() => void decide({ decision: "allow", always: true })} className="h-10 sm:h-8 px-3 rounded-lg text-[11px] border border-white/10 text-zinc-400 hover:bg-white/5">
                    Always run this exact command here
                  </button>
                )}
                {linesNote}
              </div>
            )}
            {mode === "again" && (
              <div className="flex flex-wrap items-center gap-2">
                <span className="text-[11.5px] text-rose-200">Run it anyway?</span>
                <button type="button" disabled={busy} onClick={() => void decide({ decision: "allow", always: false })} className="h-10 sm:h-8 px-3 rounded-lg text-[11px] font-bold uppercase border border-rose-500/50 text-rose-200 hover:bg-rose-500/15">
                  Run anyway
                </button>
                <button type="button" onClick={() => setMode("buttons")} className="h-10 sm:h-8 px-3 rounded-lg text-[11px] font-bold uppercase border border-white/10 text-zinc-300 hover:bg-white/5">Back</button>
                {linesNote}
              </div>
            )}
            {mode === "edit" && (
              <div className="space-y-2">
                <textarea aria-label="Command to run" value={edited} onChange={(e) => setEdited(e.target.value)} dir="ltr" spellCheck={false}
                  rows={Math.min(8, Math.max(2, edited.split("\n").length))}
                  className="w-full bg-zinc-950 border border-white/10 rounded-lg px-2 py-1.5 font-mono text-[12px] text-zinc-100 focus:outline-none focus:border-primary/50" />
                {edited !== command && edited.trim() && <ExactText text={edited} maxHeight={COMMAND_BOX} />}
                <div className="flex gap-2">
                  <button type="button" disabled={busy || !edited.trim()} onClick={() => void decide({ decision: "edit", command: edited })} className="h-10 sm:h-8 px-3 rounded-lg text-[11px] font-bold uppercase border border-primary/40 text-primary hover:bg-primary/10">
                    Run this
                  </button>
                  <button type="button" onClick={() => setMode("buttons")} className="h-10 sm:h-8 px-3 rounded-lg text-[11px] font-bold uppercase border border-white/10 text-zinc-300 hover:bg-white/5">Back</button>
                </div>
              </div>
            )}
            {mode === "deny" && (
              <div className="space-y-2">
                <input aria-label="Note to the assistant" value={note} onChange={(e) => setNote(e.target.value)} dir="auto" placeholder="Note to the assistant (optional)"
                  className="w-full h-10 sm:h-9 bg-zinc-950 border border-white/10 rounded-lg px-2 text-[12.5px] text-zinc-100 focus:outline-none focus:border-primary/50" />
                <div className="flex gap-2">
                  <button type="button" disabled={busy} onClick={() => void decide({ decision: "deny", note: note.trim() || null })} className="h-10 sm:h-8 px-3 rounded-lg text-[11px] font-bold uppercase border border-rose-500/40 text-rose-300 hover:bg-rose-500/10">
                    Deny
                  </button>
                  <button type="button" onClick={() => setMode("buttons")} className="h-10 sm:h-8 px-3 rounded-lg text-[11px] font-bold uppercase border border-white/10 text-zinc-300 hover:bg-white/5">Back</button>
                </div>
              </div>
            )}
          </div>
        )}

        {running && (
          <div className="flex items-center gap-2 text-[11.5px] text-zinc-400">
            <span>Running · {call.started ? Math.floor((now - call.started) / 1000) : 0} s</span>
            {onStop && (
              <button type="button" onClick={onStop} className="ml-auto h-10 sm:h-8 px-2.5 rounded-lg text-[10.5px] font-bold uppercase border border-rose-500/40 text-rose-300 hover:bg-rose-500/10 flex items-center gap-1">
                <Square size={10} fill="currentColor" /> Stop
              </button>
            )}
          </div>
        )}

        {(output || (isCommand && r)) && (
          <div className="space-y-1">
            {r && <div className={`text-[11.5px] ${r.state === "done" && (r.exit ?? 0) === 0 ? "text-zinc-400" : "text-amber-300/90"}`} dir="ltr">{r.summary}</div>}
            {output && (
              <div className="relative">
                <pre className="max-h-80 overflow-auto rounded-lg bg-black/30 border border-white/5 px-2 py-1.5 font-mono text-[11.5px] text-zinc-300 whitespace-pre-wrap break-all" dir="ltr">{shown}</pre>
                <div className="flex items-center gap-2 mt-1">
                  {lines.length > 20 && (
                    <button type="button" onClick={() => setFull((f) => !f)} className="min-h-10 sm:min-h-0 text-[11px] text-zinc-500 hover:text-zinc-300">
                      {full ? "Show less" : `Show all ${lines.length} lines`}
                    </button>
                  )}
                  <button type="button" onClick={() => void navigator.clipboard.writeText(output).catch(() => {})} className="ml-auto min-h-10 sm:min-h-0 text-[11px] text-zinc-500 hover:text-zinc-300 flex items-center gap-1">
                    <Copy size={11} /> Copy
                  </button>
                </div>
              </div>
            )}
          </div>
        )}

        {r && !isCommand && (r.state !== "done" || isChange) && (
          <div className={`text-[11.5px] ${r.state === "done" ? "text-zinc-400" : "text-rose-300"}`} dir="ltr"><ExactSegments segs={segments(r.summary)} /></div>
        )}
        {/* A write that failed part-way may have changed the file too: its log entry keeps what was there before. */}
        {r && isChange && r.action_id != null && onUndo && (undone ? (
          <div className="text-[11px] text-zinc-500 flex items-center gap-1.5"><Undo2 size={11} /> Undone</div>
        ) : (
          <button type="button" onClick={() => onUndo(r.action_id as number, path)} className="h-10 sm:h-8 px-2.5 rounded-lg text-[10.5px] font-bold uppercase border border-white/10 text-zinc-300 hover:bg-white/5 flex items-center gap-1.5">
            <Undo2 size={11} /> Undo
          </button>
        ))}
      </div>
    </div>
  );
};
