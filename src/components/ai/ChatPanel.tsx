import { useCallback, useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { invoke } from "@tauri-apps/api/core";
import { Send, Square, RotateCcw, Flag, ChevronDown, ChevronUp, MoreHorizontal, Pencil, Trash2, Loader2, ArrowLeft, Paperclip, SlidersHorizontal, MessageSquarePlus } from "lucide-react";
import { useConfirm, useTextPrompt } from "../../ui/confirm";
import { Markdown } from "./Markdown";
import { ToolCard, fromRecord, LIVE_CAP, type LiveCall } from "./ToolCard";
import { effortChoices, effortLabel } from "./AiSettings";
import { FullAccessBanner } from "./LevelPicker";
import {
  ai, consentNeeded, consentNote, errText, undoChangedQuestion, CONTEXT_WARN, AI_CHATS_DELETED_EVENT, AI_LEVEL_EVENT,
  type AiChatsDeletedDetail, type AiEvent, type AiLevelDetail, type Answer, type ChatItem, type ChatSettings, type ChatView, type ContextUse, type Level, type LevelInfo,
  type ProviderView, type Usage,
} from "../../util/aiApi";
import { feedOf, type AiFeed } from "../../util/aiEvents";
import { IS_MOBILE } from "../../util/platform";

// One chat: opening it continues the same history; its provider,
// model and effort can change between messages. Replies stream in as events
// on a Channel (util/aiApi.ts), batched by the backend. In a server chat a
// reply can call tools: each call shows as a card, in order, with the text
// around it.

type Seg = { kind: "text" | "thinking"; text: string } | { kind: "call"; key: string };

interface Live {
  runId: number | null;
  segs: Seg[];
  /** Each call's card, by its key: its place in `segs`. */
  calls: Record<string, LiveCall>;
  /** The card a call id's events go to. A call that repeats an id gets a
   *  card of its own, so nothing of an earlier card carries over to it. */
  cards: Record<string, string>;
  model: string;
  notices: { kind: string; text: string }[];
  usage: Usage | null;
  context: ContextUse | null;
}

const fresh = (model: string): Live => ({ runId: null, segs: [], calls: {}, cards: {}, model, notices: [], usage: null, context: null });

const fmt = (n: number) => (n >= 10000 ? `${Math.round(n / 1000)}k` : n.toLocaleString());

const REPORT_URL = "https://github.com/SinaXhpm/Submarine/issues/new";

/** Text from the user's terminal, for the composer's Attach (AI tab only). */
export interface Attach {
  selection: () => Promise<string | null>;
  tail: () => Promise<string | null>;
}

interface Props {
  chatId: number;
  providers: ProviderView[];
  onChanged: () => void;
  onDeleted: () => void;
  onBack?: () => void;
  /** Present in a session's AI tab: "Insert into terminal". */
  onInsert?: (code: string) => void;
  /** The open session of the chat's server: the assistant's tools act through it. */
  sessionId?: string | null;
  /** The server's level, for what the cards offer. */
  level?: Level | null;
  /** Every event of a run, for the AI terminal. */
  onRunEvent?: (e: AiEvent) => void;
  attach?: Attach;
  /** The AI tab's own controls: what takes the title's place (its chat
   *  picker), what ends the bar, what the options drawer adds, and the notes
   *  shown under the bar. */
  titleSlot?: ReactNode;
  barEnd?: ReactNode;
  drawerExtra?: ReactNode;
  banner?: ReactNode;
  /** Opens the chat that "Continue in a new chat" made. Without it the
   *  chat doesn't offer that. */
  onContinued?: (chatId: number) => void;
  /** On the AI page, for a server's chat: open the server's session and its
   *  AI tab on this chat. */
  onOpenServer?: (serverUuid: string, chatId: number) => void;
  /** On the AI page: the connected session of each saved server, by uuid. A
   *  server's chat acts through its server's, as in the AI tab. */
  sessions?: Record<string, string>;
  /** "Continue in a new chat" started or ended: the AI tab holds its chat. */
  onContinuing?: (on: boolean) => void;
  /** This view is answering (a reply, or a summary) or not: the AI page
   *  doesn't switch chats under it. */
  onBusy?: (busy: boolean) => void;
  /** The backend says this chat is answering, in a view that isn't this one. */
  elsewhere?: boolean;
}

/** Whether the options drawer is open, kept per device. Closed by default:
 *  the chat gets the room. */
const DRAWER_KEY = "submarine-ai-options-open";
const readDrawer = () => {
  try { return localStorage.getItem(DRAWER_KEY) === "1"; } catch { return false; }
};

/** A label above a field in the options drawer. */
const Field = ({ label, children }: { label: string; children: ReactNode }) => (
  <label className="flex flex-col gap-1 min-w-0">
    <span className="text-[10px] font-bold uppercase tracking-wider text-zinc-500">{label}</span>
    {children}
  </label>
);

/** An icon button in a chat's bar: as tall on a phone as the bar's other controls. */
export const barIcon = "h-10 w-9 sm:h-7 sm:w-7 shrink-0 rounded-lg flex items-center justify-center";

/** An on/off switch in the options drawer. */
export const Toggle = ({ on, label, onChange, disabled }: { on: boolean; label: string; onChange: (v: boolean) => void; disabled?: boolean }) => (
  <button type="button" role="switch" aria-checked={on} disabled={disabled} onClick={() => onChange(!on)}
    className="min-h-10 sm:min-h-0 flex items-center gap-2 text-[12px] text-zinc-300 disabled:opacity-40">
    <span className={`w-7 h-4 rounded-full transition-colors relative ${on ? "bg-primary/70" : "bg-white/15"}`}>
      <span className={`absolute top-0.5 h-3 w-3 rounded-full bg-white transition-all ${on ? "left-3.5" : "left-0.5"}`} />
    </span>
    {label}
  </button>
);

/** Appends to the last segment of that kind, or starts one. */
const grow = (segs: Seg[], kind: "text" | "thinking", text: string): Seg[] => {
  const last = segs[segs.length - 1];
  if (last && last.kind === kind) return [...segs.slice(0, -1), { kind, text: last.text + text }];
  return [...segs, { kind, text }];
};

/** A new card at the end, for call `id`: its events go to it from now on. */
const addCard = (l: Live, id: string, call: LiveCall): Live => {
  const key = String(l.segs.length);
  return { ...l, segs: [...l.segs, { kind: "call", key }], calls: { ...l.calls, [key]: call }, cards: { ...l.cards, [id]: key } };
};

const reduce = (l: Live, e: AiEvent): Live => {
  switch (e.type) {
    case "run_started": return { ...l, runId: e.run_id, model: e.model };
    case "model": return { ...l, model: e.model };
    case "text_delta": return { ...l, segs: grow(l.segs, "text", e.text) };
    case "thinking_delta": return { ...l, segs: grow(l.segs, "thinking", e.text) };
    case "notice": return { ...l, notices: [...l.notices, { kind: e.kind, text: e.text }] };
    case "usage": return { ...l, usage: e.usage };
    case "context": return { ...l, context: { used: e.used, limit: e.limit } };
    case "tool_call":
      return addCard(l, e.call_id, { id: e.call_id, tool: e.tool, input: e.input, needs: e.needs, reason: e.reason, red: e.red, path: e.path, preview: e.preview, how: null, command: null, out: "", err: "", started: null, record: null });
    case "tool_running": {
      const key = l.cards[e.call_id];
      const c = l.calls[key];
      return c ? { ...l, calls: { ...l.calls, [key]: { ...c, how: e.how, command: e.command, started: Date.now() } } } : l;
    }
    case "tool_output": {
      const key = l.cards[e.call_id];
      const c = l.calls[key];
      if (!c) return l;
      const stream = e.stream === "err" ? "err" : "out";
      if (c[stream].length >= LIVE_CAP) return l;
      return { ...l, calls: { ...l.calls, [key]: { ...c, [stream]: (c[stream] + e.text).slice(0, LIVE_CAP) } } };
    }
    case "tool_result": {
      const key = l.cards[e.call_id];
      const c = l.calls[key];
      // A result for a card that has one is another call's (the id was
      // repeated), and a call that never waited or ran has no card yet.
      if (!c || c.record) return addCard(l, e.call_id, fromRecord(e.record));
      return { ...l, calls: { ...l.calls, [key]: { ...c, how: e.record.how ?? c.how, out: c.out || e.record.out || "", err: c.err || e.record.err || "", record: e.record } } };
    }
    default: return l;
  }
};

export const ChatPanel = ({
  chatId, providers, onChanged, onDeleted, onBack, onInsert, sessionId: givenSession = null, level: givenLevel = null, onRunEvent, attach,
  titleSlot, barEnd, drawerExtra, banner, onContinued, onOpenServer, sessions, onContinuing, onBusy, elsewhere = false,
}: Props) => {
  const confirm = useConfirm();
  const prompt = useTextPrompt();
  const [view, setView] = useState<ChatView | null>(null);
  // The session the assistant's tools act through: the tab's, or on the AI
  // page the chat's server's when it is connected.
  const onPage = sessions !== undefined;
  const sessionId = givenSession ?? (view?.server_uuid ? sessions?.[view.server_uuid] ?? null : null);
  // On the AI page, the level that session's calls are decided at: shown as
  // the AI tab shows it, with Full access's banner.
  const [pageLevel, setPageLevel] = useState<LevelInfo | null>(null);
  const pageServer = onPage && sessionId ? view?.server_uuid ?? null : null;
  useEffect(() => {
    if (!pageServer || !sessionId) { setPageLevel(null); return; }
    let gone = false;
    const read = () => ai.levelInfo(pageServer, sessionId).then((i) => { if (!gone) setPageLevel(i); }).catch(() => {});
    void read();
    const onChange = (e: Event) => { if ((e as CustomEvent<AiLevelDetail>).detail?.serverUuid === pageServer) void read(); };
    window.addEventListener(AI_LEVEL_EVENT, onChange);
    return () => { gone = true; window.removeEventListener(AI_LEVEL_EVENT, onChange); };
  }, [pageServer, sessionId]);
  const level = givenLevel ?? pageLevel?.level ?? null;
  const alive = useRef(true);
  useEffect(() => {
    alive.current = true;
    return () => { alive.current = false; };
  }, []);
  const [live, setLive] = useState<Live | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [text, setText] = useState("");
  const [levels, setLevels] = useState<string[]>([]);
  const [menu, setMenu] = useState(false);
  const [attachMenu, setAttachMenu] = useState(false);
  const [drawer, setDrawerState] = useState(readDrawer);
  // "Continue in a new chat" is writing its summary.
  const [continuing, setContinuing] = useState(false);
  const drafted = useRef(false);
  const drawerRef = useRef<HTMLDivElement>(null);
  const drawerButton = useRef<HTMLButtonElement>(null);
  const composer = useRef<HTMLTextAreaElement>(null);
  const scroller = useRef<HTMLDivElement>(null);
  const stick = useRef(true);
  const runEvent = useRef(onRunEvent);
  runEvent.current = onRunEvent;
  // On the AI page, the feed of the session a run acts through: its events go
  // to that session's AI terminal too, as the AI tab's do. Taken when the run
  // starts, so a later session with the same id (the next profile's) never
  // gets them.
  const runFeed = useRef<AiFeed | undefined>(undefined);

  // Reloading doesn't clear an error: the reload after a failed run would wipe
  // the message it is meant to show. A new send or retry clears it.
  const load = useCallback(async () => {
    try {
      setView(await ai.loadChat(chatId));
    } catch (e) {
      setError(errText(e));
    }
  }, [chatId]);

  useEffect(() => {
    setView(null);
    setLive(null);
    setText("");
    stick.current = true;
    drafted.current = false;
    void load();
  }, [load]);

  // A deleted chat goes from every view showing it, this one included: what
  // it held doesn't stay on screen.
  const deletedRef = useRef(onDeleted);
  deletedRef.current = onDeleted;
  useEffect(() => {
    const onGone = (e: Event) => {
      const d = (e as CustomEvent<AiChatsDeletedDetail>).detail;
      if (d?.all || d?.ids?.includes(chatId)) deletedRef.current();
    };
    window.addEventListener(AI_CHATS_DELETED_EVENT, onGone);
    return () => window.removeEventListener(AI_CHATS_DELETED_EVENT, onGone);
  }, [chatId]);

  const busyHere = !!live || continuing;
  const busyRef = useRef(onBusy);
  busyRef.current = onBusy;
  useEffect(() => { busyRef.current?.(busyHere); }, [busyHere]);
  useEffect(() => () => { busyRef.current?.(false); }, []);

  // A chat that "Continue in a new chat" made opens with the summary in its
  // composer, once.
  useEffect(() => {
    if (!view?.draft || drafted.current) return;
    drafted.current = true;
    const draft = view.draft;
    setText((t) => t || draft);
  }, [view]);

  const settings = view?.settings;
  const provider = providers.find((p) => p.id === settings?.provider_id) ?? null;

  useEffect(() => {
    if (!settings?.provider_id || !settings.model) { setLevels([]); return; }
    let gone = false;
    ai.effortLevels(settings.provider_id, settings.model)
      .then((l) => { if (!gone) setLevels(l); })
      .catch(() => { if (!gone && provider) setLevels(effortChoices(provider.kind, provider.kind === "compat" ? provider.profile : null)); });
    return () => { gone = true; };
  }, [settings?.provider_id, settings?.model, provider]);

  const setDrawer = (open: boolean) => {
    setDrawerState(open);
    try { localStorage.setItem(DRAWER_KEY, open ? "1" : "0"); } catch { /* not kept */ }
  };
  useEffect(() => {
    if (!drawer) return;
    // Escape in a dialog or a view on top of the chat is theirs.
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape" && !document.querySelector('[role=dialog], [data-ai-overlay]')) setDrawer(false);
    };
    const onDown = (e: MouseEvent) => {
      const t = e.target as Node;
      if (drawerRef.current?.contains(t) || drawerButton.current?.contains(t)) return;
      // A dialog the drawer opened (turning on Auto, say) isn't outside it.
      if (t instanceof Element && t.closest("[role=dialog]")) return;
      setDrawer(false);
    };
    window.addEventListener("keydown", onKey);
    window.addEventListener("mousedown", onDown);
    return () => { window.removeEventListener("keydown", onKey); window.removeEventListener("mousedown", onDown); };
  }, [drawer]);

  // The message box grows with what is typed, up to about ten lines.
  useEffect(() => {
    const el = composer.current;
    if (!el) return;
    el.style.height = "auto";
    el.style.height = `${Math.min(el.scrollHeight, 240)}px`;
  }, [text]);

  // Follow the reply as it grows, unless the user scrolled up to read. A card
  // that waits and is taller than the view shows from its top instead: the
  // start of what the user approves stays in view, not only its end.
  useEffect(() => {
    const el = scroller.current;
    if (!el || !stick.current) return;
    el.scrollTop = el.scrollHeight;
    const card = el.querySelector<HTMLElement>("[data-waiting]");
    const above = card ? el.getBoundingClientRect().top - card.getBoundingClientRect().top : 0;
    if (above > 0) el.scrollTop -= above + 8;
  }, [view, live]);

  const onScroll = () => {
    const el = scroller.current;
    if (el) stick.current = el.scrollHeight - el.scrollTop - el.clientHeight < 60;
  };

  const onEvent = useCallback((e: AiEvent) => {
    // The AI tab's own handler hears a run to its end, even once this view is
    // gone (its chat deleted while answering): the tab is held until then.
    runEvent.current?.(e);
    // Nothing else does once this view is gone: a run winding down after Lock
    // reaches no session's feed (the next profile's have the same ids) and
    // reloads nothing.
    if (!alive.current) return;
    runFeed.current?.emit(e);
    setLive((cur) => reduce(cur ?? fresh(""), e));
    if (e.type === "run_finished") {
      // A failed run marks its message ("No answer: …"), which the reload
      // shows; the error isn't repeated in a box of its own.
      void load().then(() => setLive(null));
      onChanged();
    }
  }, [load, onChanged]);

  /** Asks once per provider before anything goes to it. The yes is for the
   *  host the dialog names: if the provider's address changed since this
   *  list was read, the backend refuses it, and the dialog asks once more
   *  with the address it has now. */
  const askConsent = async (providerId: number): Promise<boolean> => {
    let list = providers;
    for (let again = false; ; again = true) {
      const p = list.find((x) => x.id === providerId);
      if (!p) return false;
      const note = consentNote(p);
      const ok = await confirm({
        title: `Send chats to ${p.host}?`,
        message:
          `Messages you send, and everything the assistant reads on your servers (command output, file contents, folder listings), will be sent to ${p.host}. Submarine sends it nowhere else. Don't use the assistant on servers whose data must not leave them.` +
          (note ? `\n\n${note}` : ""),
        okLabel: "Continue",
        safeDefault: true,
      });
      if (!ok) return false;
      try {
        await ai.consent(providerId, p.host);
        return true;
      } catch (e) {
        if (again || !String(e).startsWith("[AI] CONSENT_HOST")) throw e;
        list = await ai.providers();
      }
    }
  };

  // What a failed send or summary shows. Consent is asked only for the chat's
  // own provider; a consent tag that names another one means this view is
  // behind (the chat's provider changed meanwhile), so the chat is read again.
  const failText = (e: unknown): string => {
    if (consentNeeded(e) === null) return errText(e);
    void load();
    return "This chat's provider changed meanwhile. Check it, then try again.";
  };

  const start = async (kind: "send" | "retry", body?: string) => {
    setError(null);
    stick.current = true;
    runFeed.current = onPage && sessionId ? feedOf(sessionId) : undefined;
    setLive(fresh(settings?.model ?? ""));
    const go = () => (kind === "send" ? ai.send(chatId, body ?? "", onEvent, sessionId) : ai.retry(chatId, onEvent, sessionId));
    try {
      await go();
      if (kind === "send") void load();
    } catch (e) {
      const pid = consentNeeded(e);
      const own = pid !== null && pid === settings?.provider_id ? pid : null;
      try {
        if (own !== null && (await askConsent(own))) {
          await go();
          if (kind === "send") void load();
          onChanged();
          return;
        }
        // A no to the dialog shows nothing; any other error does.
        if (own === null) setError(failText(e));
      } catch (e2) {
        setError(failText(e2));
      }
      setLive(null);
      if (kind === "send" && body) setText(body);
    }
  };

  const send = () => {
    const body = text.trim();
    if (!body || live || continuing) return;
    setText("");
    void start("send", body);
  };

  const stop = () => {
    if (live?.runId) void ai.stop(live.runId);
  };

  const decide = async (callId: string, answer: Answer) => {
    if (live?.runId == null) return;
    try {
      await ai.decide(live.runId, callId, answer);
    } catch (e) {
      setError(errText(e));
    }
  };

  // The card's path and this chat go with the id: a log entry that isn't
  // this card's change is refused.
  const undo = async (actionId: number, path: string) => {
    if (!sessionId) {
      setError("Open the server's session to undo a change.");
      return;
    }
    try {
      await ai.undo(actionId, sessionId, false, path, chatId);
    } catch (e) {
      if (!String(e).startsWith("[AI] UNDO_CHANGED")) { setError(errText(e)); return; }
      const ok = await confirm(undoChangedQuestion(path));
      if (!ok) return;
      try { await ai.undo(actionId, sessionId, true, path, chatId); } catch (e2) { setError(errText(e2)); return; }
    }
    await load();
  };

  const change = async (patch: Partial<ChatSettings>) => {
    if (!settings) return;
    try {
      await ai.setChatSettings(chatId, { ...settings, ...patch });
      await load();
      onChanged();
    } catch (e) {
      setError(errText(e));
    }
  };

  const pickProvider = (id: number) => {
    const p = providers.find((x) => x.id === id);
    if (!p) return;
    // A provider saved without a model keeps the chat's: it can be changed next.
    void change({ provider_id: id, model: p.model || settings?.model || "", effort: p.defaults.effort, max_tokens: p.defaults.max_tokens, show_reasoning: p.defaults.show_reasoning, fallback: p.defaults.fallback });
  };

  const rename = async () => {
    setMenu(false);
    const t = await prompt({ title: "Rename chat", initialValue: view?.title ?? "", okLabel: "Rename" });
    if (t == null) return;
    try { await ai.renameChat(chatId, t); await load(); onChanged(); } catch (e) { setError(errText(e)); }
  };

  const remove = async () => {
    setMenu(false);
    const ok = await confirm({
      title: "Delete this chat?",
      message: view?.server_uuid
        ? "It is removed from this device.\n\nThe activity log keeps the commands the assistant ran in it and the earlier content of files it changed, for up to 90 days. You can clear the log in Activity."
        : "It is removed from this device.",
      okLabel: "Delete",
      destructive: true,
      safeDefault: true,
    });
    if (!ok) return;
    // A server chat's commands and earlier file contents can go with it.
    const clear = !!view?.server_uuid && (await confirm({
      title: "Clear its activity too?",
      message: "The activity log keeps the commands this chat ran and the earlier content of files it changed, for up to 90 days. Clear them now? Then its changes can't be undone.",
      okLabel: "Clear them",
      cancelLabel: "Keep them",
      destructive: true,
      safeDefault: true,
    }));
    // The deletion's event closes the chat here too, as in every other view.
    try { await ai.deleteChat(chatId, clear); } catch (e) { setError(errText(e)); }
  };

  const setMaxTokens = async () => {
    setMenu(false);
    const t = await prompt({
      title: "Max output tokens",
      message: "Empty means the provider's default.",
      initialValue: settings?.max_tokens ? String(settings.max_tokens) : "",
      validate: (v) => (v.trim() === "" || /^\d{1,7}$/.test(v.trim()) ? null : "A whole number"),
    });
    if (t == null) return;
    void change({ max_tokens: t.trim() ? Number(t.trim()) : null });
  };

  // As a terminal's links ask. The host gets a line of its own: an address
  // can start with what looks like another host. A very long one is cut, so
  // that line and the buttons stay in view.
  const openLink = async (url: string) => {
    let host: string;
    try { host = new URL(url).host; } catch { return; }
    const shown = url.length > 300 ? `${url.slice(0, 300)}…` : url;
    const ok = await confirm({ title: "Open this link?", message: `${shown}\n\nIt opens in your browser, at ${host}.`, okLabel: "Open", safeDefault: true });
    if (ok) invoke("open_external_url", { url }).catch((e) => setError(errText(e)));
  };

  const report = async (item: ChatItem) => {
    const text = item.text?.trim() ? item.text : "";
    const ok = await confirm({
      title: "Report this reply?",
      message:
        (text ? "The reply is copied to your clipboard, and " : "This reply has no text to copy; ") +
        "Submarine's issue page opens in your browser: write or paste what you want to report there. Nothing is sent by the app. Don't include private data from your servers.",
      okLabel: "Open the issue page",
      safeDefault: true,
    });
    if (!ok) return;
    // Copied only now, once the user agreed.
    if (text) {
      try { await navigator.clipboard.writeText(text); } catch { setError("The reply couldn't be copied: select it in the chat and copy it."); }
    }
    invoke("open_external_url", { url: REPORT_URL }).catch((e) => setError(errText(e)));
  };

  const addAttachment = async (what: "selection" | "tail") => {
    setAttachMenu(false);
    if (!attach) return;
    const got = what === "selection" ? await attach.selection() : await attach.tail();
    if (!got || !got.trim()) {
      setError(what === "selection" ? "Nothing is selected in the terminal." : "The terminal has no output to attach.");
      return;
    }
    // The terminal shows what the server printed. Tag characters in it could
    // carry instructions nobody sees, and a ``` in it would end the block, so
    // the rest would read as the user's own words: they go, and the fence is
    // longer than any run of backticks inside.
    let body = (got.length > 64 * 1024 ? got.slice(-64 * 1024) : got).replace(/[\u{E0000}-\u{E007F}]/gu, "");
    let end = body.length;
    while (end > 0 && body.charCodeAt(end - 1) === 0x0a) end--;
    body = body.slice(0, end);
    let longest = 0;
    for (let i = 0, run = 0; i < body.length; i++) {
      run = body.charCodeAt(i) === 0x60 ? run + 1 : 0;
      if (run > longest) longest = run;
    }
    const fence = "`".repeat(Math.max(3, longest + 1));
    const label = what === "selection" ? "Selected terminal text" : "The last lines of my terminal";
    setText((t) => `${t}${t && !t.endsWith("\n") ? "\n" : ""}${label}:\n${fence}text\n${body}\n${fence}\n`);
  };

  /** "Continue in a new chat": the backend writes a summary of this chat and
   *  makes the new one, which opens with the summary in its composer. */
  const continueNew = async () => {
    if (!onContinued) return;
    setError(null);
    setContinuing(true);
    onContinuing?.(true);
    const go = () => ai.continueChat(chatId, sessionId);
    try {
      let r;
      try {
        r = await go();
      } catch (e) {
        // As for a send: only the chat's own provider is asked about.
        const pid = consentNeeded(e);
        if (pid === null || pid !== settings?.provider_id) throw e;
        // A no to the dialog shows nothing.
        if (!(await askConsent(pid))) return;
        r = await go();
      }
      onChanged();
      // Only from the chat that asked: one the user left has nothing to open.
      if (alive.current) onContinued(r.chat.id);
    } catch (e) {
      if (alive.current && !String(e).includes("STOPPED")) setError(failText(e));
    } finally {
      onContinuing?.(false);
      if (alive.current) setContinuing(false);
    }
  };

  const totals = useMemo(() => {
    let input = 0, output = 0, cached = 0, cost = 0, hasCost = false;
    for (const i of view?.items ?? []) {
      if (i.kind === "assistant" && i.usage) {
        input += i.usage.input; output += i.usage.output; cached += i.usage.cache_read;
        if (typeof i.usage.cost === "number") { cost += i.usage.cost; hasCost = true; }
      }
    }
    return { input, output, cached, cost: hasCost ? cost : null };
  }, [view]);

  const lastUserFailed = (() => {
    const items = view?.items ?? [];
    for (let i = items.length - 1; i >= 0; i--) {
      if (items[i].kind === "note") continue;
      return items[i].kind === "user" && !!items[i].failed;
    }
    return false;
  })();

  if (!view) {
    return (
      <div className="flex-1 flex items-center justify-center text-zinc-500 text-sm">
        {error ? <span className="text-rose-300">{error}</span> : <Loader2 size={16} className="animate-spin" />}
      </div>
    );
  }

  const kind = provider?.kind ?? "compat";
  const profile = provider && provider.kind === "compat" ? provider.profile : null;
  const running = !!live || continuing;
  // The context meter: the last request's input against the model's limit.
  const context = live?.context ?? view.context;
  const share = context?.limit ? context.used / context.limit : null;
  const canContinue = !!onContinued && view.items.some((i) => i.kind === "assistant");
  const serverChat = !!view.server_uuid;
  // Only at Ask: at Ask every command, the list's commands ask too.
  const canAlways = level === "ask";
  const lastText = live ? live.segs.map((s, i) => (s.kind === "text" ? i : -1)).reduce((a, b) => Math.max(a, b), -1) : -1;

  return (
    <div className="flex-1 flex flex-col min-h-0 min-w-0">
      {/* One bar: the title (or the tab's chat picker), Options, Stop, the menu.
          The chat's settings are in the drawer under it, closed by default. */}
      <div className="relative shrink-0 z-20">
        <div className="h-11 px-1 sm:px-2 flex items-center gap-1 sm:gap-1.5 border-b border-white/5 bg-white/[0.02]">
          {onBack && (
            <button type="button" aria-label="Back to chats" onClick={onBack} className={`${barIcon} text-zinc-400 hover:text-zinc-100 hover:bg-white/5`}>
              <ArrowLeft size={16} />
            </button>
          )}
          {titleSlot ?? <div className="flex-1 min-w-0 px-1 text-[13px] font-semibold text-zinc-100 truncate" dir="auto">{view.title}</div>}
          <button
            ref={drawerButton}
            type="button"
            aria-label="Chat options"
            aria-expanded={drawer}
            title={`Provider, model and effort: ${provider?.name ?? "none"} · ${settings?.model ?? ""}`}
            onClick={() => setDrawer(!drawer)}
            className={`h-10 sm:h-8 min-w-9 sm:min-w-0 px-2 rounded-lg flex items-center justify-center gap-1.5 text-[11.5px] border transition-colors ${drawer ? "border-primary/40 text-primary bg-primary/10" : "border-white/10 text-zinc-300 hover:bg-white/5"}`}
          >
            <SlidersHorizontal size={13} className="shrink-0" />
            {!titleSlot && <span className="hidden sm:inline truncate max-w-[12rem] font-mono" dir="ltr">{settings?.model}</span>}
          </button>
          {running && live?.runId != null && (
            <button type="button" onClick={stop} className="h-10 sm:h-8 shrink-0 px-2.5 rounded-lg text-[11px] font-bold uppercase border border-rose-500/40 text-rose-300 hover:bg-rose-500/10 flex items-center gap-1.5">
              <Square size={10} fill="currentColor" /> Stop
            </button>
          )}
          <div className="relative">
            <button type="button" aria-label="Chat menu" onClick={() => setMenu((m) => !m)} className={`${barIcon} text-zinc-400 hover:text-zinc-100 hover:bg-white/5`}>
              <MoreHorizontal size={16} />
            </button>
            {menu && (
              <div className="absolute right-0 top-11 sm:top-9 z-40 w-56 rounded-xl border border-white/10 bg-[#16161a] shadow-2xl p-1 text-[12px]">
                <button type="button" className="w-full text-left px-3 py-3 sm:py-2 rounded-lg hover:bg-white/5 text-zinc-200 flex items-center gap-2" onClick={() => void rename()}>
                  <Pencil size={12} /> Rename
                </button>
                {canContinue && (
                  <button type="button" disabled={running} className="w-full text-left px-3 py-3 sm:py-2 rounded-lg hover:bg-white/5 text-zinc-200 flex items-center gap-2 disabled:opacity-40"
                    title="A new chat that starts from a summary of this one" onClick={() => { setMenu(false); void continueNew(); }}>
                    <MessageSquarePlus size={12} /> Continue in a new chat
                  </button>
                )}
                <button type="button" className="w-full text-left px-3 py-3 sm:py-2 rounded-lg hover:bg-rose-500/10 text-rose-300 flex items-center gap-2" onClick={() => void remove()}>
                  <Trash2 size={12} /> Delete
                </button>
              </div>
            )}
          </div>
          {barEnd}
        </div>

        {drawer && (
          <div ref={drawerRef} className="absolute left-0 right-0 top-full max-h-[calc(100vh-17rem)] sm:max-h-[70vh] overflow-y-auto border-b border-white/10 bg-[#141417] shadow-2xl shadow-black/60">
            <div className="p-3 space-y-3">
              <div className="grid grid-cols-1 sm:grid-cols-[minmax(0,1fr)_minmax(0,1.3fr)] gap-2">
                <Field label="Provider">
                  <select
                    aria-label="Provider"
                    disabled={running}
                    className="h-10 sm:h-8 px-2 bg-zinc-950 border border-white/10 rounded-lg text-[12px] text-zinc-200"
                    value={settings?.provider_id ?? ""}
                    onChange={(e) => pickProvider(Number(e.target.value))}
                  >
                    {!provider && <option value="">Pick a provider</option>}
                    {providers.map((p) => <option key={p.id} value={p.id}>{p.name}</option>)}
                  </select>
                </Field>
                <Field label="Model">
                  <ModelField value={settings?.model ?? ""} disabled={running} onCommit={(m) => m !== settings?.model && void change({ model: m })} />
                </Field>
              </div>
              <div className="flex flex-wrap items-end gap-x-4 gap-y-2">
                {levels.length > 0 && (
                  <Field label="Effort">
                    <select
                      aria-label="Effort"
                      disabled={running}
                      className="h-10 sm:h-8 px-2 bg-zinc-950 border border-white/10 rounded-lg text-[12px] text-zinc-200"
                      value={settings?.effort ?? ""}
                      onChange={(e) => void change({ effort: e.target.value || null })}
                    >
                      <option value="">Default</option>
                      {levels.map((l) => <option key={l} value={l}>{effortLabel(kind, profile, l)}</option>)}
                    </select>
                  </Field>
                )}
                <Field label="Max output">
                  <button type="button" disabled={running} onClick={() => void setMaxTokens()} className="h-10 sm:h-8 px-2 rounded-lg border border-white/10 bg-zinc-950 text-[12px] text-zinc-200 text-left hover:bg-white/5">
                    {settings?.max_tokens ? `${settings.max_tokens.toLocaleString()} tokens` : "Provider's default"}
                  </button>
                </Field>
                {kind === "anthropic" && (
                  <div className="flex flex-col gap-1.5 pb-1">
                    <Toggle on={!!settings?.show_reasoning} disabled={running} label="Reasoning summaries" onChange={(v) => void change({ show_reasoning: v })} />
                    <Toggle on={!!settings?.fallback} disabled={running} label="Retry on another model when declined" onChange={(v) => void change({ fallback: v })} />
                  </div>
                )}
              </div>
              {drawerExtra}
              <div className="flex flex-wrap items-center gap-x-3 gap-y-1.5 pt-1 border-t border-white/5">
                <p className="flex-1 min-w-[12rem] text-[10.5px] text-zinc-500 leading-snug">A new model or provider re-reads the whole chat without the provider's cache, so the next message costs more.</p>
                {context && (
                  <span className="text-[11px] text-zinc-500 shrink-0 flex items-center gap-1.5" title="How much of the model's context the last request filled">
                    context {fmt(context.used)}{context.limit ? ` of ${fmt(context.limit)}` : ""}
                    {share !== null && (
                      <span className="w-12 h-1.5 rounded-full bg-white/10 overflow-hidden" aria-label={`${Math.round(share * 100)}% of the context`}>
                        <span className={`block h-full ${share >= CONTEXT_WARN ? "bg-amber-400" : "bg-primary/70"}`} style={{ width: `${Math.min(100, Math.round(share * 100))}%` }} />
                      </span>
                    )}
                  </span>
                )}
                <span className="text-[11px] text-zinc-500 shrink-0" title="Tokens used by this chat">
                  {totals.input + totals.output > 0 && <>in {fmt(totals.input)}{totals.cached > 0 && ` (${fmt(totals.cached)} cached)`} · out {fmt(totals.output)}</>}
                  {totals.cost !== null && <> · ${totals.cost.toFixed(4)}</>}
                </span>
                <button type="button" aria-label="Close options" onClick={() => setDrawer(false)} className="h-10 sm:h-7 px-2 rounded-lg text-[11px] text-zinc-400 border border-white/10 hover:bg-white/5 flex items-center gap-1 shrink-0">
                  <ChevronUp size={12} /> Close
                </button>
              </div>
            </div>
          </div>
        )}
      </div>
      {onPage && pageServer && sessionId && pageLevel?.level === "full" && (
        <FullAccessBanner serverName={pageLevel.name || view.server_label || "this server"} serverUuid={pageServer} sessionId={sessionId} info={pageLevel} onChanged={setPageLevel} />
      )}
      {onPage && pageLevel?.level === "auto" && (
        <div className="shrink-0 px-3 py-1 text-[11px] text-rose-200 border-b border-rose-500/30 bg-rose-500/10">
          Auto on {pageLevel.name || view.server_label || "this server"}: commands run without asking, except ones that look risky.
        </div>
      )}
      {elsewhere && !live && !continuing && (
        <div className="shrink-0 px-3 py-1 text-[11px] text-zinc-400 border-b border-white/5 flex items-center gap-2">
          <Loader2 size={11} className="animate-spin shrink-0" />
          <span className="flex-1">This chat is answering in another view.</span>
          <button type="button" onClick={() => void ai.stopChat(chatId).then(onChanged)} className="h-10 sm:h-7 px-2.5 rounded-lg text-[10.5px] font-bold uppercase border border-white/10 text-zinc-300 hover:bg-white/5">
            Stop
          </button>
        </div>
      )}
      {onPage && serverChat && !sessionId && (
        <div className="shrink-0 px-3 py-1 text-[11px] text-amber-300/80 border-b border-white/5 flex flex-wrap items-center gap-x-2 gap-y-1">
          <span className="flex-1 min-w-[12rem]">The assistant can't reach {view.server_label ?? "this server"} from here. Continue this chat in the server's AI tab to let it act there.</span>
          {onOpenServer && view.server_uuid && (
            <button type="button" onClick={() => onOpenServer(view.server_uuid!, chatId)} disabled={running}
              className="h-10 sm:h-7 px-2.5 rounded-lg text-[11px] font-semibold border border-amber-400/30 text-amber-200 hover:bg-amber-500/10 disabled:opacity-40">
              Open the server
            </button>
          )}
        </div>
      )}
      {banner}

      <div ref={scroller} onScroll={onScroll} className="flex-1 min-h-0 overflow-y-auto px-3 py-4 space-y-4">
        {view.facts && (
          <details className="rounded-lg border border-white/5 bg-white/[0.02] text-[11.5px]">
            <summary className="cursor-pointer select-none px-3 py-3 sm:py-1.5 text-zinc-500">
              {view.facts_sent ? "Server facts sent to the assistant" : "Server facts (found after the chat started; used by Submarine's checks, not sent)"}
            </summary>
            <pre className="px-3 pb-2 whitespace-pre-wrap text-zinc-400 font-mono text-[11px]" dir="ltr">{view.facts}</pre>
          </details>
        )}
        {view.items.length === 0 && !live && (
          <div className="text-center text-zinc-500 text-[13px] mt-10">
            {serverChat ? `Ask about ${view.server_label ?? "this server"}. The assistant acts only within the level set for it.` : "Ask anything. This chat has no access to any server."}
          </div>
        )}
        {view.items.map((item, i) => (
          <Item key={`${item.kind}-${item.seq ?? `n${i}`}`} item={item} onOpenLink={openLink} onInsert={onInsert} onReport={report} onUndo={sessionId ? undo : undefined} undone={view.undone} />
        ))}
        {live && (
          <div className="space-y-2">
            {live.segs.map((s, i) =>
              s.kind === "call" ? (
                live.calls[s.key] && (
                  <ToolCard key={s.key} call={live.calls[s.key]} live canAlways={canAlways} onDecide={(a) => decide(live.calls[s.key].id, a)} onStop={stop} onUndo={sessionId ? undo : undefined} />
                )
              ) : s.kind === "thinking" ? (
                <Thinking key={i} text={s.text} open={i === live.segs.length - 1} />
              ) : (
                <div key={i} className="text-[13.5px] text-zinc-200 leading-relaxed">
                  <Markdown text={s.text} streaming={i === lastText} onOpenLink={openLink} onInsert={onInsert} />
                </div>
              ),
            )}
            {(live.segs.length === 0 || live.segs[live.segs.length - 1].kind === "call") && !Object.values(live.calls).some((c) => !c.record) && (
              <div className="flex items-center gap-2 text-[12px] text-zinc-500"><Loader2 size={12} className="animate-spin" /> {live.model || "Waiting"}…</div>
            )}
            {live.notices.map((n, i) => (
              <div key={i} className={`text-[11.5px] rounded-lg px-3 py-1.5 border ${n.kind === "context" || n.kind === "max_sessions" || n.kind === "steps" || n.kind === "hidden" ? "text-amber-300 bg-amber-500/10 border-amber-500/20" : "text-zinc-400 bg-white/[0.03] border-white/5"}`}>{n.text}</div>
            ))}
          </div>
        )}
        {!live && canContinue && share !== null && share >= CONTEXT_WARN && (
          <div className="rounded-lg border border-amber-500/20 bg-amber-500/10 px-3 py-2 space-y-2">
            <p className="text-[12px] text-amber-200 leading-snug">
              This chat fills {Math.round(share * 100)}% of what the model can take in. Each message of a long chat costs more, and once it is full the provider refuses it or drops its start. A new chat that starts from a summary of this one avoids that.
            </p>
            <div className="flex items-center gap-2">
              <button type="button" disabled={continuing} onClick={() => void continueNew()}
                className="h-10 sm:h-8 px-3 rounded-lg text-[11px] font-bold uppercase border border-amber-400/40 text-amber-200 hover:bg-amber-500/10 flex items-center gap-1.5 disabled:opacity-70">
                {continuing ? <Loader2 size={12} className="animate-spin" /> : <MessageSquarePlus size={12} />}
                {continuing ? "Writing a summary…" : "Continue in a new chat"}
              </button>
              {continuing && (
                <button type="button" onClick={() => void ai.stopChat(chatId)} className="h-10 sm:h-8 px-2.5 rounded-lg text-[11px] font-bold uppercase border border-white/10 text-zinc-300 hover:bg-white/5">
                  Stop
                </button>
              )}
            </div>
          </div>
        )}
        {continuing && !(share !== null && share >= CONTEXT_WARN) && (
          <div className="flex items-center gap-2 text-[12px] text-zinc-400">
            <Loader2 size={12} className="animate-spin" /> Writing a summary for the new chat…
            <button type="button" onClick={() => void ai.stopChat(chatId)} className="h-10 sm:h-7 px-2 rounded-lg text-[11px] border border-white/10 text-zinc-300 hover:bg-white/5">Stop</button>
          </div>
        )}
        {error && <div className="text-[12px] text-rose-300 bg-rose-500/10 border border-rose-500/20 rounded-lg px-3 py-2" dir="auto">{error}</div>}
        {lastUserFailed && !live && (
          <button type="button" onClick={() => void start("retry")} className="h-10 sm:h-8 px-3 rounded-lg text-[11px] font-bold uppercase border border-white/10 text-zinc-300 hover:bg-white/5 flex items-center gap-1.5">
            <RotateCcw size={12} /> Try again
          </button>
        )}
      </div>

      <div className="shrink-0 border-t border-white/5 p-2 flex items-end gap-2">
        {attach && (
          <div className="relative">
            <button type="button" aria-label="Attach" title="Attach terminal output" disabled={running} onClick={() => setAttachMenu((m) => !m)}
              className="h-10 w-10 shrink-0 rounded-xl border border-white/10 text-zinc-400 flex items-center justify-center hover:text-zinc-100 hover:bg-white/5 disabled:opacity-40">
              <Paperclip size={16} />
            </button>
            {attachMenu && (
              <div className="absolute left-0 bottom-12 z-20 w-60 rounded-xl border border-white/10 bg-[#16161a] shadow-2xl p-1 text-[12px]">
                <button type="button" className="w-full text-left px-3 py-3 sm:py-2 rounded-lg hover:bg-white/5 text-zinc-200" onClick={() => void addAttachment("selection")}>Terminal selection</button>
                <button type="button" className="w-full text-left px-3 py-3 sm:py-2 rounded-lg hover:bg-white/5 text-zinc-200" onClick={() => void addAttachment("tail")}>Last 200 lines of the terminal</button>
              </div>
            )}
          </div>
        )}
        <textarea
          aria-label="Message"
          value={text}
          disabled={running}
          dir="auto"
          ref={composer}
          rows={2}
          placeholder={running ? "Answering…" : "Message the assistant"}
          onChange={(e) => setText(e.target.value)}
          onKeyDown={(e) => {
            // A phone's keyboard has no Shift+Enter: Enter makes a new line
            // there and the Send button sends (Ctrl+Enter from a real keyboard).
            if (e.key === "Enter" && !e.shiftKey && !e.nativeEvent.isComposing && (!IS_MOBILE || e.ctrlKey || e.metaKey)) {
              e.preventDefault();
              send();
            }
          }}
          className="flex-1 resize-none min-h-[3.25rem] max-h-60 bg-zinc-950 border border-white/10 rounded-xl px-3 py-2 text-[13.5px] leading-relaxed text-zinc-100 focus:outline-none focus:border-primary/50 disabled:opacity-50"
        />
        <button
          type="button"
          aria-label="Send"
          disabled={running || !text.trim()}
          onClick={send}
          className="h-10 w-10 shrink-0 rounded-xl bg-primary/15 border border-primary/30 text-primary flex items-center justify-center disabled:opacity-40 hover:bg-primary hover:text-zinc-950 transition-all"
        >
          <Send size={16} />
        </button>
      </div>
    </div>
  );
};

/** A model name the user can type; committed on Enter or when the field loses focus. */
const ModelField = ({ value, disabled, onCommit }: { value: string; disabled: boolean; onCommit: (m: string) => void }) => {
  const [v, setV] = useState(value);
  useEffect(() => setV(value), [value]);
  const commit = () => {
    const m = v.trim();
    if (m) onCommit(m);
    else setV(value);
  };
  return (
    <input
      aria-label="Model"
      disabled={disabled}
      value={v}
      dir="ltr"
      spellCheck={false}
      onChange={(e) => setV(e.target.value)}
      onBlur={commit}
      onKeyDown={(e) => { if (e.key === "Enter") { e.preventDefault(); commit(); } }}
      className="h-10 sm:h-8 w-full min-w-0 px-2 bg-zinc-950 border border-white/10 rounded-lg text-[12px] text-zinc-200 font-mono"
    />
  );
};

const Thinking = ({ text, open }: { text: string; open?: boolean }) => (
  <details open={open} className="group rounded-lg border border-white/5 bg-white/[0.02]">
    <summary className="cursor-pointer select-none list-none px-3 py-3 sm:py-1.5 text-[11.5px] text-zinc-500 flex items-center gap-1">
      <ChevronDown size={12} className="transition-transform group-open:rotate-0 -rotate-90" /> Thinking
    </summary>
    <div className="px-3 pb-2 text-[12px] text-zinc-500 whitespace-pre-wrap leading-relaxed" dir="auto">{text}</div>
  </details>
);

const Failed = ({ failed }: { failed: NonNullable<ChatItem["failed"]> }) => (
  <div className="max-w-[85%] space-y-1">
    {failed.partial && <div className="text-[12.5px] text-zinc-400 border-l-2 border-white/10 pl-2 whitespace-pre-wrap" dir="auto">{failed.partial}</div>}
    <div className={`text-[11px] ${failed.stopped ? "text-zinc-500" : "text-rose-300"}`} dir="auto">
      {failed.stopped ? "Stopped." : `No answer: ${errText(failed.error)}`}
    </div>
  </div>
);

const Item = ({ item, onOpenLink, onInsert, onReport, onUndo, undone }: {
  item: ChatItem;
  onOpenLink: (url: string) => void;
  onInsert?: (code: string) => void;
  onReport: (item: ChatItem) => void;
  onUndo?: (actionId: number, path: string) => void;
  undone: number[];
}) => {
  if (item.kind === "note") {
    const warn = item.note === "refused" || item.note === "cut_off" || item.note === "hidden";
    return <div className={`text-center text-[11px] ${warn ? "text-amber-400/90" : "text-zinc-500"}`} dir="auto">{item.text}</div>;
  }
  if (item.kind === "user") {
    // A turn of tool results isn't a message: only what happened after it shows.
    if (item.results) return item.failed ? <div className="flex flex-col items-end"><Failed failed={item.failed} /></div> : null;
    return (
      <div className="flex flex-col items-end gap-1">
        <div className="max-w-[85%] rounded-2xl rounded-br-md bg-primary/10 border border-primary/20 px-3 py-2 text-[13.5px] text-zinc-100 whitespace-pre-wrap break-words" dir="auto">{item.text}</div>
        {item.failed && <Failed failed={item.failed} />}
      </div>
    );
  }
  return (
    <div className="space-y-1.5">
      {item.thinking && <Thinking text={item.thinking} />}
      {item.text && (
        <div className="text-[13.5px] text-zinc-200 leading-relaxed">
          <Markdown text={item.text} onOpenLink={onOpenLink} onInsert={onInsert} />
        </div>
      )}
      {/* Keyed by place: two calls can have the same id. */}
      {item.calls?.map((c, j) => (
        <ToolCard key={j} call={fromRecord(c)} live={false} canAlways={false} onDecide={async () => {}} onUndo={onUndo} undone={c.action_id != null && undone.includes(c.action_id)} />
      ))}
      <div className="flex items-center gap-2 text-[10.5px] text-zinc-600">
        {item.model && <span>{item.model}</span>}
        {item.answered_by && <span className="text-amber-400/80">· answered by {item.answered_by}</span>}
        {item.usage && <span>· {fmt(item.usage.input)} in · {fmt(item.usage.output)} out</span>}
        <button type="button" aria-label="Report this reply" title="Report this reply" onClick={() => onReport(item)} className="ml-auto h-10 w-10 -my-3 -mr-2 sm:h-auto sm:w-auto sm:my-0 sm:mr-0 sm:p-1 flex items-center justify-center rounded hover:bg-white/5 hover:text-zinc-400">
          <Flag size={11} />
        </button>
      </div>
    </div>
  );
};
