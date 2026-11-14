import { memo, useCallback, useDeferredValue, useEffect, useId, useMemo, useRef, useState } from "react";
import {
  ChevronDown, FileEdit, FilePlus, FileText, FolderOpen, History, Loader2, MessageSquare, RefreshCw, Search, Server, Terminal, Trash2, Undo2, Wrench, X,
  type LucideIcon,
} from "lucide-react";
import { useConfirm } from "../../ui/confirm";
import { useIsCompact } from "../../hooks/useViewport";
import { segments, type Segment } from "../../util/hiddenChars";
import { ExactSegments, ExactText } from "./ExactText";
import { ai, errText, undoChangedQuestion, LEVELS, type ActionView } from "../../util/aiApi";

// What the assistant did on a server, from the log kept on this device: each
// tool call, how it was let through, and Undo for a change that can still be
// put back. For one server it also shows the always-run list, which can be
// edited here. The log outlives its chats and has its own Clear.

export interface ActivityViewProps {
  /** One server; null: every server (the AI page). */
  serverUuid: string | null;
  serverName?: string;
  /** For every server: server_uuid to name. */
  serverNames?: Record<string, string>;
  /** The server's open session: Undo acts through it. */
  sessionId?: string | null;
  /** For every server: the connected session of each, by uuid, for Undo. */
  sessions?: Record<string, string>;
  onOpenChat?: (chatId: number) => void;
  onClose: () => void;
}

const TOOLS: Record<string, { label: string; Icon: LucideIcon }> = {
  run_command: { label: "Command", Icon: Terminal },
  list_directory: { label: "List folder", Icon: FolderOpen },
  read_file: { label: "Read file", Icon: FileText },
  write_file: { label: "Write file", Icon: FilePlus },
  edit_file: { label: "Edit file", Icon: FileEdit },
  // Only an outside agent's (the agent gateway).
  server_facts: { label: "Server facts", Icon: Server },
  delete_file: { label: "Delete file", Icon: Trash2 },
};

type Tone = "plain" | "red" | "warn";

const DECISIONS: Record<string, { label: string; note: string; tone: Tone }> = {
  read_only: { label: "read-only", note: "Ran by itself: it only reads", tone: "plain" },
  approved: { label: "you approved", note: "You approved it", tone: "plain" },
  edited: { label: "you edited", note: "You changed the command, then it ran", tone: "plain" },
  always: { label: "always run", note: "On the server's always-run list", tone: "plain" },
  auto: { label: "Auto", note: "Ran by itself at Auto", tone: "red" },
  full: { label: "Full access", note: "Ran by itself at Full access", tone: "red" },
  denied: { label: "denied", note: "You said no", tone: "warn" },
  refused: { label: "refused", note: "Not allowed at this level, or the server wasn't connected", tone: "warn" },
  failed: { label: "failed", note: "It couldn't be done", tone: "warn" },
  skipped: { label: "skipped", note: "Not run: an earlier call in the same step was denied", tone: "warn" },
  stopped: { label: "stopped", note: "Stopped before it finished", tone: "warn" },
  interrupted: { label: "interrupted", note: "Cut off; it may have run", tone: "warn" },
};

const TONE: Record<Tone, string> = {
  plain: "bg-white/5 text-zinc-400",
  red: "bg-rose-500/10 text-rose-300",
  warn: "bg-amber-500/10 text-amber-300",
};

/** The backend sends at most this many entries, newest first. */
const LIMIT = 500;
/** What a row draws of the first line; the row cuts it to its width anyway. */
const PREVIEW = 300;

const SHORT_DATE = new Intl.DateTimeFormat(undefined, { month: "short", day: "numeric" });
const FULL_DATE = new Intl.DateTimeFormat(undefined, { dateStyle: "medium", timeStyle: "medium" });
const utf8 = new TextEncoder();

const ago = (at: number, now: number) => {
  const s = Math.max(0, now / 1000 - at);
  if (s < 60) return "now";
  if (s < 3600) return `${Math.floor(s / 60)} min`;
  if (s < 86_400) return `${Math.floor(s / 3600)} h`;
  if (s < 7 * 86_400) return `${Math.floor(s / 86_400)} d`;
  const d = new Date(at * 1000);
  return Number.isFinite(d.getTime()) ? SHORT_DATE.format(d) : "";
};

// The log keeps the first 2,000 bytes of a command or path, without a mark
// where it was cut.
const maybeCut = (s: string) => s.length > 600 && utf8.encode(s).length > 1996;

/** An entry with what its row shows, worked out once per load. */
interface Row {
  a: ActionView;
  /** The summary in lower case, for the search. */
  lower: string;
  /** The first line, hidden characters as escapes. */
  first: Segment[];
  /** The first line goes on past PREVIEW. */
  cut: boolean;
  /** How many lines follow it. */
  more: number;
  full: string;
  iso: string;
}

const prep = (a: ActionView): Row => {
  // Line breaks at the start don't make the row show an empty line.
  const text = a.summary.replace(/^\n+|\n+$/g, "");
  const nl = text.indexOf("\n");
  let more = 0;
  for (let i = nl; i >= 0; i = text.indexOf("\n", i + 1)) more++;
  let first = nl < 0 ? text : text.slice(0, nl);
  const cut = first.length > PREVIEW;
  if (cut) {
    const c = first.charCodeAt(PREVIEW - 1);
    first = first.slice(0, c >= 0xd800 && c <= 0xdbff ? PREVIEW - 1 : PREVIEW);
  }
  const at = new Date(a.at * 1000);
  const valid = Number.isFinite(at.getTime());
  return {
    a,
    lower: a.summary.toLowerCase(),
    first: segments(first),
    cut,
    more,
    full: valid ? FULL_DATE.format(at) : "",
    iso: valid ? at.toISOString() : "",
  };
};

export const ActivityView = ({ serverUuid, serverName, serverNames, sessionId = null, sessions, onOpenChat, onClose }: ActivityViewProps) => {
  const confirm = useConfirm();
  const compact = useIsCompact();
  const uid = useId();
  const [items, setItems] = useState<ActionView[] | null>(null);
  const [always, setAlways] = useState<string[] | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [query, setQuery] = useState("");
  const [expanded, setExpanded] = useState<ReadonlySet<number>>(() => new Set());
  const [undoing, setUndoing] = useState<number | null>(null);
  const [removing, setRemoving] = useState<string | null>(null);
  const [clearing, setClearing] = useState(false);
  const [now, setNow] = useState(() => Date.now());
  const rootRef = useRef<HTMLDivElement>(null);
  const searchRef = useRef<HTMLInputElement>(null);
  const opener = useRef<HTMLElement | null>(null);
  const noticeTimer = useRef<number | undefined>(undefined);
  const seq = useRef({ load: 0, activity: 0, always: 0 });
  const undoBusy = useRef(false);
  // The view can close while a call or a dialog is under way. After that
  // nothing is shown, no dialog opens, and a dialog's answer does nothing.
  const alive = useRef(true);
  const latest = useRef({ onClose, onOpenChat, query });
  latest.current = { onClose, onOpenChat, query };

  const name = serverName || "this server";

  useEffect(() => {
    alive.current = true;
    return () => {
      alive.current = false;
      window.clearTimeout(noticeTimer.current);
    };
  }, []);

  // Each load drops the answer of one that started before it: the server
  // can change while a load is on its way.
  const loadActivity = useCallback(async () => {
    const n = ++seq.current.activity;
    try {
      const list = await ai.activity(serverUuid);
      if (alive.current && n === seq.current.activity) { setItems(list); setNow(Date.now()); }
    } catch (e) {
      if (alive.current && n === seq.current.activity) setError(errText(e));
    }
  }, [serverUuid]);

  const loadAlways = useCallback(async () => {
    if (!serverUuid) return;
    const n = ++seq.current.always;
    try {
      const list = await ai.alwaysList(serverUuid);
      if (alive.current && n === seq.current.always) setAlways(list);
    } catch (e) {
      if (alive.current && n === seq.current.always) setError(errText(e));
    }
  }, [serverUuid]);

  const load = useCallback(async () => {
    const n = ++seq.current.load;
    setError(null);
    setLoading(true);
    await Promise.all([loadActivity(), loadAlways()]);
    if (alive.current && n === seq.current.load) setLoading(false);
  }, [loadActivity, loadAlways]);

  useEffect(() => {
    setItems(null);
    setAlways(null);
    setExpanded(new Set());
    void load();
  }, [load]);

  // The times in the rows ("5 min") move on while the view is open.
  useEffect(() => {
    const t = window.setInterval(() => setNow(Date.now()), 30_000);
    return () => window.clearInterval(t);
  }, []);

  const flash = useCallback((text: string) => {
    if (!alive.current) return;
    window.clearTimeout(noticeTimer.current);
    setNotice(text);
    noticeTimer.current = window.setTimeout(() => setNotice(null), 5000);
  }, []);

  // Focus moves into the view, so Escape and Tab start here, unless the user
  // is typing somewhere (the terminal). Escape and Close give it back.
  useEffect(() => {
    const root = rootRef.current;
    const a = document.activeElement;
    if (a instanceof HTMLElement && a !== document.body && !root?.contains(a)) opener.current = a;
    const typing = a instanceof HTMLElement && (a.isContentEditable || /^(INPUT|TEXTAREA|SELECT)$/.test(a.tagName));
    if (!typing) root?.focus({ preventScroll: true });
  }, []);

  const closeBack = useCallback(() => {
    const back = opener.current;
    latest.current.onClose();
    if (back) window.setTimeout(() => { if (back.isConnected) back.focus({ preventScroll: true }); }, 0);
  }, []);

  // A button that goes away with what it acted on (Undo, Remove, Clear)
  // would take the focus out of the view with it. `from` is that button; if
  // the user has moved on (to the search), the focus stays where it is.
  const holdFocus = useCallback((from: Element | null) => {
    const root = rootRef.current;
    if (root && from && from !== root && document.activeElement === from && root.contains(from)) root.focus({ preventScroll: true });
  }, []);

  // Escape and a click outside close the view. Not while it is hidden (its
  // tab stays mounted: not drawn, or out of view and inert), behind a panel
  // that made the app inert, or while a dialog is up: those keys and clicks
  // are theirs. An Escape typed elsewhere, such as in the terminal, isn't
  // for this view, and a click on the button that opened it is left to it.
  useEffect(() => {
    const shown = () => {
      const root = rootRef.current;
      if (!root || root.getClientRects().length === 0 || root.closest("[inert]")) return null;
      return document.querySelector('[aria-modal="true"]') ? null : root;
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== "Escape" || e.repeat || e.defaultPrevented || e.isComposing) return;
      const root = shown();
      const t = e.target;
      if (!root || !(t === document.body || (t instanceof Node && root.contains(t)))) return;
      e.preventDefault();
      if (t === searchRef.current && latest.current.query) setQuery("");
      else closeBack();
    };
    const onDown = (e: MouseEvent) => {
      const root = shown();
      const t = e.target as Node;
      if (!root || root.contains(t) || opener.current?.contains(t)) return;
      latest.current.onClose();
    };
    window.addEventListener("keydown", onKey);
    window.addEventListener("mousedown", onDown);
    return () => {
      window.removeEventListener("keydown", onKey);
      window.removeEventListener("mousedown", onDown);
    };
  }, [closeBack]);

  const toggle = useCallback((id: number) => {
    setExpanded((s) => {
      const n = new Set(s);
      if (!n.delete(id)) n.add(id);
      return n;
    });
  }, []);

  const openChat = useCallback((chatId: number) => {
    const { onOpenChat: open, onClose: close } = latest.current;
    open?.(chatId);
    close();
  }, []);

  /** The session a row's Undo acts through: the one given, or its server's. */
  const sessionFor = useCallback(
    (rowServer: string | null) => sessionId ?? (rowServer ? sessions?.[rowServer] ?? null : null),
    [sessionId, sessions],
  );

  // As in the chat: when the file changed since the assistant wrote it, ask
  // before putting the earlier content back. The entry's path goes with the
  // id, and an entry for another file is refused.
  const undo = useCallback(async (id: number) => {
    const row = items?.find((a) => a.id === id);
    const sid = sessionFor(row?.server_uuid ?? null);
    if (!row || !sid || undoBusy.current) return;
    const path = row.path ?? row.summary;
    const from = document.activeElement;
    undoBusy.current = true;
    setUndoing(id);
    setError(null);
    try {
      let done: string;
      try {
        done = await ai.undo(id, sid, false, path, null);
      } catch (e) {
        if (!String(e).startsWith("[AI] UNDO_CHANGED")) throw e;
        if (!alive.current) return;
        const ok = await confirm(undoChangedQuestion(path, !!row.agent));
        if (!ok || !alive.current) return;
        done = await ai.undo(id, sid, true, path, null);
      }
      if (!alive.current) return;
      flash(done);
      holdFocus(from);
      await loadActivity();
    } catch (e) {
      if (!alive.current) return;
      setError(errText(e));
      holdFocus(from);
      void loadActivity();
    } finally {
      undoBusy.current = false;
      if (alive.current) setUndoing(null);
    }
  }, [sessionFor, items, confirm, flash, holdFocus, loadActivity]);

  const removeAlways = async (command: string) => {
    if (!serverUuid || removing !== null) return;
    const from = document.activeElement;
    const ok = await confirm({
      title: "Remove from the always-run list?",
      message: "The assistant will ask again before running it.",
      okLabel: "Remove",
      safeDefault: true,
    });
    if (!ok || !alive.current) return;
    setRemoving(command);
    setError(null);
    try { await ai.alwaysRemove(serverUuid, command); } catch (e) { if (alive.current) setError(errText(e)); }
    if (!alive.current) return;
    setRemoving(null);
    holdFocus(from);
    await loadAlways();
  };

  const clear = async () => {
    if (clearing) return;
    const from = document.activeElement;
    const ok = await confirm({
      title: serverUuid ? `Clear the activity log for ${name}?` : "Clear the activity log of every server?",
      message: "Undo is no longer possible for the changes in it.",
      okLabel: "Clear",
      destructive: true,
      safeDefault: true,
    });
    if (!ok || !alive.current) return;
    setClearing(true);
    setError(null);
    try {
      await ai.clearActivity(serverUuid);
      if (alive.current) setExpanded(new Set());
    } catch (e) {
      if (alive.current) setError(errText(e));
    }
    if (!alive.current) return;
    setClearing(false);
    holdFocus(from);
    await loadActivity();
  };

  const rows = useMemo(() => (items ?? []).map(prep), [items]);
  const needle = useDeferredValue(query).trim().toLowerCase();
  const shownRows = useMemo(() => (needle ? rows.filter((r) => r.lower.includes(needle)) : rows), [rows, needle]);
  const when = useMemo(() => {
    const m = new Map<number, string>();
    for (const r of rows) m.set(r.a.id, ago(r.a.at, now));
    return m;
  }, [rows, now]);

  const btn = compact ? "h-10" : "h-8";
  const iconBtn = `${compact ? "h-10 w-10" : "h-8 w-8"} shrink-0 rounded-lg flex items-center justify-center text-zinc-400 hover:text-zinc-100 hover:bg-white/5`;
  const hasItems = !!items && items.length > 0;
  const spinner = <div className="flex justify-center py-6 text-zinc-500"><Loader2 size={16} className="animate-spin" /></div>;

  return (
    <div ref={rootRef} tabIndex={-1} role="region" aria-labelledby={`${uid}-title`} className="flex-1 min-h-0 min-w-0 flex flex-col bg-[#121214] outline-none">
      <div className="h-11 px-2 flex items-center gap-1.5 border-b border-white/5 bg-white/[0.02] shrink-0">
        <History size={14} aria-hidden="true" className="text-primary shrink-0 ml-1" />
        <h2 id={`${uid}-title`} className="flex-1 min-w-0 px-1 text-[13px] font-semibold text-zinc-100 truncate">
          {serverUuid ? <>Activity on <bdi>{name}</bdi></> : "Activity"}
        </h2>
        <button type="button" aria-label="Refresh" title="Refresh" aria-disabled={loading} onClick={() => { if (!loading) void load(); }} className={iconBtn}>
          <RefreshCw size={15} className={loading ? "animate-spin" : ""} />
        </button>
        <button type="button" aria-label="Close activity" title="Close" onClick={closeBack} className={iconBtn}>
          <X size={15} />
        </button>
      </div>

      <div aria-live="polite" className="shrink-0">
        {error && <div className="mx-3 mt-2 text-[12px] text-rose-300 bg-rose-500/10 border border-rose-500/20 rounded-lg px-3 py-2 break-words" dir="auto">{error}</div>}
        {notice && <div className="mx-3 mt-2 text-[12px] text-emerald-300 bg-emerald-500/10 border border-emerald-500/20 rounded-lg px-3 py-2 break-words" dir="auto">{notice}</div>}
      </div>

      <div className="flex-1 min-h-0 overflow-y-auto custom-scrollbar">
        {serverUuid && (
          <section aria-labelledby={`${uid}-always`} className="px-3 py-3 space-y-2 border-b border-white/5">
            <h3 id={`${uid}-always`} className="text-[10px] font-bold uppercase tracking-wider text-zinc-500">Always run on this server</h3>
            {always === null ? (
              !error && spinner
            ) : always.length === 0 ? (
              <p className="text-[12px] text-zinc-500">Nothing yet. An approval card's "Always run" adds a command here.</p>
            ) : (
              <ul className="space-y-2">
                {always.map((c) => (
                  <li key={c} className="flex items-start gap-2">
                    <ExactText text={c} maxHeight="8rem" className="flex-1 min-w-0" />
                    <button
                      type="button"
                      disabled={removing !== null && removing !== c}
                      aria-disabled={removing === c}
                      onClick={() => void removeAlways(c)}
                      className={`${btn} px-2.5 shrink-0 rounded-lg text-[10.5px] font-bold uppercase border border-white/10 text-zinc-300 hover:bg-white/5 flex items-center gap-1.5 disabled:opacity-40`}
                    >
                      {removing === c ? <Loader2 size={11} className="animate-spin" /> : <X size={11} />} Remove
                    </button>
                  </li>
                ))}
              </ul>
            )}
          </section>
        )}

        <section aria-labelledby={`${uid}-log`}>
          {/* Stays in view over the rows, with the search. */}
          <div className="sticky top-0 z-10 bg-[#121214] px-3 pt-3 pb-2 space-y-2">
            <div className="flex items-baseline gap-2">
              <h3 id={`${uid}-log`} className="text-[10px] font-bold uppercase tracking-wider text-zinc-500">What the assistant did</h3>
              {hasItems && <span className="text-[10.5px] text-zinc-600">{needle ? `${shownRows.length} of ${items.length}` : items.length}</span>}
            </div>
            {hasItems && (
              <div className="relative">
                <Search size={13} aria-hidden="true" className="absolute left-2.5 top-1/2 -translate-y-1/2 text-zinc-500" />
                <input
                  ref={searchRef}
                  value={query}
                  onChange={(e) => setQuery(e.target.value)}
                  aria-label="Search the activity"
                  placeholder="Search commands and paths"
                  dir="auto"
                  spellCheck={false}
                  className={`w-full ${btn} pl-8 pr-9 bg-zinc-950 border border-white/10 rounded-lg text-[12px] text-zinc-200 focus:outline-none focus:border-primary/50`}
                />
                {query && (
                  <button type="button" aria-label="Clear search" onClick={() => { setQuery(""); searchRef.current?.focus(); }}
                    className={`absolute right-1 top-1/2 -translate-y-1/2 ${compact ? "h-8 w-8" : "h-7 w-7"} rounded-md flex items-center justify-center text-zinc-500 hover:text-zinc-200 hover:bg-white/5`}>
                    <X size={13} />
                  </button>
                )}
              </div>
            )}
          </div>

          <div className="px-3 pb-3">
            {items === null ? (
              !error && spinner
            ) : items.length === 0 ? (
              <p className="text-[12px] text-zinc-500">
                Nothing yet. Every command the assistant runs on {serverUuid ? "this server" : "a server"}, and every folder or file it opens or changes, shows here for 90 days.
              </p>
            ) : shownRows.length === 0 ? (
              <p className="text-[12px] text-zinc-500">Nothing matches the search.</p>
            ) : (
              <ul className="space-y-1.5">
                {shownRows.map((r) => (
                  <Entry
                    key={r.a.id}
                    row={r}
                    when={when.get(r.a.id) ?? ""}
                    open={expanded.has(r.a.id)}
                    server={serverUuid || !serverNames ? null : (r.a.server_uuid && serverNames[r.a.server_uuid]) || "Unknown server"}
                    undoing={undoing === r.a.id}
                    undoBusy={r.a.can_undo && undoing !== null}
                    canUndo={!!sessionFor(r.a.server_uuid)}
                    canOpen={!!onOpenChat}
                    compact={compact}
                    idBase={uid}
                    onToggle={toggle}
                    onUndo={undo}
                    onOpenChat={openChat}
                  />
                ))}
              </ul>
            )}
            {items && items.length >= LIMIT && <p className="mt-2 text-[11px] text-zinc-600">The newest {LIMIT} entries are shown.</p>}
          </div>
        </section>
      </div>

      {hasItems && (
        <div className="shrink-0 p-2 border-t border-white/5">
          <button
            type="button"
            aria-disabled={clearing}
            onClick={() => void clear()}
            className={`w-full ${btn} rounded-lg text-[11px] font-bold uppercase text-zinc-500 hover:text-rose-300 hover:bg-rose-500/10 flex items-center justify-center gap-1.5`}
          >
            <Trash2 size={12} /> Clear the log
          </button>
        </div>
      )}
    </div>
  );
};

interface EntryProps {
  row: Row;
  when: string;
  open: boolean;
  /** The server's name, in the view of every server. */
  server: string | null;
  undoing: boolean;
  /** An Undo is on its way: one at a time. */
  undoBusy: boolean;
  /** There is a session to undo through. */
  canUndo: boolean;
  canOpen: boolean;
  compact: boolean;
  idBase: string;
  onToggle: (id: number) => void;
  onUndo: (id: number) => void;
  onOpenChat: (chatId: number) => void;
}

// One row. Kept apart and memoised: with 500 rows, typing in the search or
// opening a row redraws only the rows that change.
const Entry = memo(function Entry({ row, when, open, server, undoing, undoBusy, canUndo, canOpen, compact, idBase, onToggle, onUndo, onOpenChat }: EntryProps) {
  const { a } = row;
  const tool = TOOLS[a.tool];
  const Icon = tool?.Icon ?? Wrench;
  const toolLabel = tool?.label ?? a.tool;
  const d = DECISIONS[a.decision];
  const level = LEVELS.find((l) => l.id === a.level)?.label ?? a.level;
  const isCommand = a.tool === "run_command";
  const chatId = a.chat_id;
  const details = `${idBase}-${a.id}`;
  const btn = compact ? "h-10 px-3" : "h-8 px-2.5";

  return (
    <li className="rounded-lg border border-white/5 bg-white/[0.02]">
      {/* The summary takes the line; the rest goes under it when the row is narrow. */}
      <div className="flex flex-wrap items-center gap-x-2 gap-y-1 px-2.5 py-1">
        <button
          type="button"
          aria-expanded={open}
          aria-controls={open ? details : undefined}
          onClick={() => onToggle(a.id)}
          className={`grow-[999] basis-60 min-w-0 ${compact ? "min-h-10" : "min-h-8"} flex items-center gap-2 text-left text-zinc-300 hover:text-zinc-100`}
        >
          <span title={toolLabel} className="shrink-0 text-zinc-500"><Icon size={13} aria-hidden="true" /></span>
          <span className="sr-only">{toolLabel}: </span>
          <span className="min-w-0 overflow-hidden text-ellipsis whitespace-pre font-mono text-[11.5px]" dir="ltr">
            {row.first.length > 0 ? <ExactSegments segs={row.first} /> : <span className="text-zinc-600">(empty)</span>}
            {row.cut && <span className="text-zinc-600">…</span>}
          </span>
          {row.more > 0 && <span className="shrink-0 text-[10.5px] text-zinc-500">+{row.more} {row.more === 1 ? "line" : "lines"}</span>}
          <ChevronDown size={12} aria-hidden="true" className={`ml-auto shrink-0 text-zinc-600 transition-transform ${open ? "" : "-rotate-90"}`} />
        </button>

        <div className="grow flex flex-wrap items-center gap-1.5 text-[10.5px]">
          <span title={d?.note} className={`px-1.5 py-0.5 rounded ${TONE[d?.tone ?? "plain"]}`}>{d?.label ?? a.decision}</span>
          {a.agent && <span title={`An outside agent, through Submarine: ${a.agent}`} className="px-1.5 py-0.5 rounded bg-sky-500/10 text-sky-300">agent</span>}
          {isCommand && a.exit_code !== null && (
            <span className={`px-1.5 py-0.5 rounded font-mono ${a.exit_code === 0 ? "bg-emerald-500/10 text-emerald-300" : "bg-rose-500/10 text-rose-300"}`}>exit {a.exit_code}</span>
          )}
          {server !== null && (
            <span title={server} className="min-w-0 max-w-[8rem] flex items-center gap-1 text-zinc-500">
              <Server size={10} aria-hidden="true" className="shrink-0" />
              <span className="truncate" dir="auto">{server}</span>
            </span>
          )}
          <time dateTime={row.iso || undefined} title={row.full} className="text-zinc-500 whitespace-nowrap">{when}</time>
          {(a.can_undo || (canOpen && chatId !== null)) && (
            <span className="ml-auto flex items-center gap-1.5">
              {a.can_undo && (
                <button
                  type="button"
                  disabled={!canUndo || (undoBusy && !undoing)}
                  aria-disabled={undoing}
                  title={canUndo ? "Put back what was there before" : "Open the server's session to undo"}
                  onClick={() => onUndo(a.id)}
                  className={`${btn} rounded-lg text-[10.5px] font-bold uppercase border border-white/10 text-zinc-300 hover:bg-white/5 flex items-center gap-1.5 disabled:opacity-40 disabled:hover:bg-transparent`}
                >
                  {undoing ? <Loader2 size={11} className="animate-spin-brief" /> : <Undo2 size={11} />} Undo
                </button>
              )}
              {canOpen && chatId !== null && (
                <button
                  type="button"
                  aria-label="Open chat"
                  title="Open the chat"
                  onClick={() => onOpenChat(chatId)}
                  className={`${compact ? "h-10 w-10" : "h-8 w-8"} rounded-lg border border-white/10 text-zinc-400 hover:text-zinc-100 hover:bg-white/5 flex items-center justify-center`}
                >
                  <MessageSquare size={12} />
                </button>
              )}
            </span>
          )}
        </div>
      </div>

      {open && (
        <div id={details} className="px-2.5 pb-2.5 space-y-1.5">
          <ExactText text={a.summary} what={isCommand ? "command" : "text"} maxHeight="16rem" />
          <p className="text-[11px] text-zinc-500">{[toolLabel, level && `Level: ${level}`, d?.note, row.full].filter(Boolean).join(" · ")}</p>
          {a.agent && <p className="text-[11px] text-zinc-500 break-words" dir="auto">An outside agent, through Submarine: {a.agent}</p>}
          {maybeCut(a.summary) && <p className="text-[11px] text-zinc-500">The log keeps the first 2,000 bytes, so the end may be missing.</p>}
        </div>
      )}
    </li>
  );
});
