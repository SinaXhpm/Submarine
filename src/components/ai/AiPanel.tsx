import { useCallback, useEffect, useRef, useState, type ReactNode } from "react";
import { ChevronDown, History, LayoutList, Loader2, Plus, Sparkles, TerminalSquare, X } from "lucide-react";
import { ChatPanel, Toggle, barIcon } from "./ChatPanel";
import { LevelPicker, FullAccessBanner } from "./LevelPicker";
import { ActivityView } from "./ActivityView";
import {
  ai, errText, AI_LEVEL_EVENT, AI_OPEN_EVENT,
  type AiEvent, type AiLevelDetail, type AiOpenDetail, type ChatSummary, type Level, type LevelInfo, type ProviderView,
} from "../../util/aiApi";
import type { AiFeed } from "../../util/aiEvents";

// The session's AI tab: chats about this server, at the level set for it.
// The assistant's tools act through this session only. Every event of its
// runs also goes to the session's AI terminal. One thin bar on top; the
// chat's settings are in a drawer, so the chat has the room.

/** Whether the AI terminal opens by itself on a run's first command. */
const OPEN_TERMINAL_KEY = "submarine-ai-open-terminal";
const readOpenTerminal = () => {
  try { return localStorage.getItem(OPEN_TERMINAL_KEY) !== "0"; } catch { return true; }
};

interface Props {
  sessionId: string;
  serverUuid: string | null;
  serverName: string;
  connected: boolean;
  /** The tab is on screen. */
  active: boolean;
  // The tab's terminal side: the AI terminal's feed and how to show it, Insert
  // into the terminal, attaching its text. An FTP server's tab has no terminal
  // and gives none of them (its assistant has file tools only).
  feed?: AiFeed;
  onShowTerminal?: () => void;
  onInsert?: (code: string) => void;
  readTerminal?: (what: "selection" | "tail") => Promise<string | null>;
  /** How many calls wait for the user's answer. */
  onPending: (n: number) => void;
  onClose: () => void;
  headerActions?: ReactNode;
  /** A chat to show, asked for from outside ("Open the server" on the AI
   *  page): `id` null means the one open last. `n` changes with each ask. */
  openChat?: { id: number | null; n: number } | null;
  /** The level calls are decided at, as it changes. */
  onLevel?: (level: Level) => void;
}

export const AiPanel = ({ sessionId, serverUuid, serverName, connected, active, feed, onShowTerminal, onInsert, readTerminal, onPending, onClose, headerActions, openChat, onLevel }: Props) => {
  const [providers, setProviders] = useState<ProviderView[] | null>(null);
  const [info, setInfo] = useState<LevelInfo | null>(null);
  const [chats, setChats] = useState<ChatSummary[]>([]);
  const [chatId, setChatId] = useState<number | null>(null);
  const [picked, setPicked] = useState(false);
  const [chatMenu, setChatMenu] = useState(false);
  const [activity, setActivity] = useState(false);
  const [openTerminal, setOpenTerminal] = useState(readOpenTerminal);
  const [busy, setBusy] = useState(false);
  // A chat that is answering stays open here: its cards wait on this view.
  const [running, setRunning] = useState(false);
  // "Continue in a new chat" is writing its summary: the tab stays too.
  const [continuing, setContinuing] = useState(false);
  const busyTab = running || continuing;
  const [error, setError] = useState<string | null>(null);
  // What the store has to say (set aside, nearly full). It is told once, to
  // this tab or the AI page, whichever asks first.
  const [notice, setNotice] = useState<string | null>(null);
  const pending = useRef(new Set<string>());
  const opened = useRef(false);
  const menuRef = useRef<HTMLDivElement>(null);
  const level = info?.level ?? null;

  const refreshChats = useCallback(async () => {
    if (!serverUuid) return;
    try { setChats(await ai.chats({ kind: "server", server_uuid: serverUuid }, "")); } catch { /* shown on use */ }
  }, [serverUuid]);

  const refreshLevel = useCallback(async () => {
    if (!serverUuid) return;
    try { setInfo(await ai.levelInfo(serverUuid, sessionId)); } catch { setInfo({ level: "ask", saved: "ask", posix: null, name: null }); }
  }, [serverUuid, sessionId]);

  const readNotice = useCallback(() => {
    ai.notice().then((n) => n && setNotice(n)).catch(() => {});
  }, []);

  // Providers can change in Settings while the tab is away: read them again
  // each time it comes back.
  useEffect(() => {
    if (!active) return;
    ai.providers().then(setProviders).catch((e) => setError(errText(e)));
    void refreshLevel();
    void refreshChats();
    readNotice();
  }, [active, refreshLevel, refreshChats, readNotice]);

  // A disconnect ends Full access in the backend: the tab shows it.
  useEffect(() => { void refreshLevel(); }, [connected, refreshLevel]);

  // A level changed elsewhere (the server card, the AI page): follow it.
  useEffect(() => {
    const onChange = (e: Event) => {
      if ((e as CustomEvent<AiLevelDetail>).detail?.serverUuid === serverUuid) void refreshLevel();
    };
    window.addEventListener(AI_LEVEL_EVENT, onChange);
    return () => window.removeEventListener(AI_LEVEL_EVENT, onChange);
  }, [serverUuid, refreshLevel]);

  useEffect(() => { if (level) onLevel?.(level); }, [level, onLevel]);

  // The chat open last for this server. A chat asked for meanwhile wins: the
  // answer that comes after it is dropped.
  useEffect(() => {
    if (!serverUuid || picked) return;
    let gone = false;
    ai.lastChat(serverUuid)
      .then((id) => { if (!gone) { setChatId(id); setPicked(true); } })
      .catch(() => { if (!gone) setPicked(true); });
    return () => { gone = true; };
  }, [serverUuid, picked]);

  // A chat asked for from outside. A chat that is answering keeps the tab.
  useEffect(() => {
    if (!openChat || busyTab) return;
    setActivity(false);
    if (openChat.id != null) {
      setChatId(openChat.id);
      setPicked(true);
    } else {
      setPicked(false);
    }
    void refreshChats();
    // Only a new ask counts, not a change of `busyTab`.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [openChat?.n]);

  useEffect(() => { feed?.setChat(chatId); }, [feed, chatId]);

  // The chat list closes on a click elsewhere, and on Escape.
  useEffect(() => {
    if (!chatMenu) return;
    const onDown = (e: MouseEvent) => { if (!menuRef.current?.contains(e.target as Node)) setChatMenu(false); };
    const onKey = (e: KeyboardEvent) => { if (e.key === "Escape") setChatMenu(false); };
    window.addEventListener("mousedown", onDown);
    window.addEventListener("keydown", onKey);
    return () => { window.removeEventListener("mousedown", onDown); window.removeEventListener("keydown", onKey); };
  }, [chatMenu]);

  const newChat = async () => {
    if (!serverUuid) return;
    setChatMenu(false);
    setBusy(true);
    setError(null);
    try {
      const c = await ai.createChat(null, null, { serverUuid, sessionId });
      setChatId(c.id);
      void refreshChats();
    } catch (e) {
      setError(errText(e));
    } finally {
      setBusy(false);
    }
  };

  const showAllChats = () => {
    setChatMenu(false);
    window.dispatchEvent(new CustomEvent<AiOpenDetail>(AI_OPEN_EVENT, { detail: chatId != null ? { chatId } : {} }));
  };

  const onRunEvent = useCallback((e: AiEvent) => {
    feed?.emit(e);
    const p = pending.current;
    if (e.type === "run_started") { p.clear(); opened.current = false; setRunning(true); }
    if (e.type === "tool_call" && e.needs === "approval") p.add(e.call_id);
    if (e.type === "tool_running" || e.type === "tool_result") p.delete(e.call_id);
    // A run's saves may have left a notice (the store nearly full).
    if (e.type === "run_finished") { p.clear(); setRunning(false); readNotice(); }
    onPending(p.size);
    // The terminal opens on the run's first command, so the user sees it run.
    if (e.type === "tool_call" && e.tool === "run_command" && !opened.current && onShowTerminal && readOpenTerminal()) {
      opened.current = true;
      onShowTerminal();
    }
  }, [feed, onPending, onShowTerminal, readNotice]);

  const attach = readTerminal ? { selection: () => readTerminal("selection"), tail: () => readTerminal("tail") } : undefined;
  const current = chats.find((c) => c.id === chatId);
  // The name as it is now (the tab keeps the one it opened with): turning on
  // Full access asks for it to be typed.
  const currentName = info?.name || serverName;
  // The level picker turns on Full access through this session, when it is up.
  const liveSession = connected ? sessionId : null;

  // The chat picker: the open chat's title, opening this server's chats.
  const picker = (
    <div ref={menuRef} className="relative flex-1 min-w-0 flex items-center gap-1.5">
      <Sparkles size={14} className="hidden sm:block text-primary shrink-0 ml-1" />
      <button
        type="button"
        disabled={busyTab}
        aria-expanded={chatMenu}
        title={busyTab ? "Wait for the answer, or stop it, to switch chats" : "This server's chats"}
        onClick={() => setChatMenu((m) => !m)}
        className="h-10 sm:h-8 min-w-0 flex-1 px-2 rounded-lg hover:bg-white/5 text-[13px] font-semibold text-zinc-100 flex items-center gap-1.5 disabled:opacity-60"
      >
        <span className="truncate" dir="auto">{current?.title ?? (chatId ? "Chat" : "AI assistant")}</span>
        <ChevronDown size={13} className="shrink-0 text-zinc-500" />
      </button>
      {chatMenu && (
        <div className="absolute left-0 top-10 z-40 w-[min(22rem,90vw)] max-h-96 overflow-y-auto rounded-xl border border-white/10 bg-[#16161a] shadow-2xl p-1 text-[12px]">
          <button type="button" disabled={!connected || busy || !providers?.length} onClick={() => void newChat()} className="w-full text-left px-3 py-3 sm:py-2 rounded-lg hover:bg-white/5 text-primary flex items-center gap-2 disabled:opacity-40">
            <Plus size={12} /> New chat
          </button>
          {chats.length === 0 && <div className="px-3 py-2 text-zinc-500">No chats about {serverName} yet.</div>}
          {chats.map((c) => (
            <button key={c.id} type="button" onClick={() => { setChatId(c.id); setChatMenu(false); }} className={`w-full text-left px-3 py-3 sm:py-2 rounded-lg hover:bg-white/5 ${c.id === chatId ? "text-primary" : "text-zinc-200"}`}>
              <div className="truncate" dir="auto">{c.title}</div>
              <div className="text-[10.5px] text-zinc-500 truncate">{c.provider_name ?? "No provider"} · {c.model}</div>
            </button>
          ))}
          <div className="mt-1 pt-1 border-t border-white/5">
            <button type="button" onClick={() => { setChatMenu(false); setActivity(true); }} className="w-full text-left px-3 py-3 sm:py-2 rounded-lg hover:bg-white/5 text-zinc-300 flex items-center gap-2">
              <History size={12} /> Activity on {serverName}
            </button>
            <button type="button" onClick={showAllChats} className="w-full text-left px-3 py-3 sm:py-2 rounded-lg hover:bg-white/5 text-zinc-300 flex items-center gap-2">
              <LayoutList size={12} /> All chats
            </button>
          </div>
        </div>
      )}
    </div>
  );

  // The end of the bar: the level, the AI terminal, full size, close.
  const barEnd = (
    <>
      {serverUuid && (
        <LevelPicker variant="chip" serverUuid={serverUuid} serverName={currentName} sessionId={liveSession} info={info} onChanged={setInfo} />
      )}
      {onShowTerminal && (
        <button type="button" onClick={onShowTerminal} aria-label="Show in a terminal" title="Show the assistant's commands in a terminal" className={`${barIcon} max-[339px]:hidden text-zinc-400 hover:text-zinc-100 hover:bg-white/5`}>
          <TerminalSquare size={15} />
        </button>
      )}
      {headerActions}
      <button type="button" onClick={onClose} aria-label="Close" className={`${barIcon} text-zinc-500 hover:text-white hover:bg-white/5`}>
        <X size={15} />
      </button>
    </>
  );

  // What the options drawer adds in the tab: the level, with what each one
  // means, the activity, and the AI terminal.
  const drawerExtra = serverUuid && (
    <div className="space-y-3 pt-1 border-t border-white/5">
      <div className="space-y-1.5">
        <div className="text-[10px] font-bold uppercase tracking-wider text-zinc-500">Permission level on {serverName}</div>
        <LevelPicker variant="list" serverUuid={serverUuid} serverName={currentName} sessionId={liveSession} info={info} onChanged={setInfo} />
      </div>
      <div className="flex flex-wrap items-center gap-3">
        {onShowTerminal && (
          <>
            <button type="button" onClick={onShowTerminal} className="h-10 sm:h-8 px-3 rounded-lg text-[11.5px] border border-white/10 text-zinc-300 hover:bg-white/5 flex items-center gap-1.5">
              <TerminalSquare size={13} /> Show in a terminal
            </button>
            <Toggle on={openTerminal} label="Open it for each run" onChange={(v) => {
              setOpenTerminal(v);
              try { localStorage.setItem(OPEN_TERMINAL_KEY, v ? "1" : "0"); } catch { /* not kept */ }
            }} />
          </>
        )}
        <button type="button" onClick={() => setActivity(true)} className="h-10 sm:h-8 px-3 rounded-lg text-[11.5px] border border-white/10 text-zinc-300 hover:bg-white/5 flex items-center gap-1.5">
          <History size={13} /> Activity and always-run commands
        </button>
      </div>
    </div>
  );

  // Shown in every state of the tab: it is told only once.
  const noticeBox = notice && (
    <div className="shrink-0 mx-3 mt-2 text-[11.5px] text-amber-300 bg-amber-500/10 border border-amber-500/20 rounded-lg px-3 py-2">{notice}</div>
  );

  const banner = (
    <>
      {serverUuid && info?.level === "full" && (
        <FullAccessBanner serverName={currentName} serverUuid={serverUuid} sessionId={sessionId} info={info} onChanged={setInfo} />
      )}
      {level === "suggest" && <div className="shrink-0 px-3 py-1 text-[11px] text-zinc-500 border-b border-white/5">Suggest only: the assistant can't reach {serverName}; it writes commands for you.</div>}
      {!connected && <div className="shrink-0 px-3 py-1 text-[11px] text-amber-300/80 border-b border-white/5">Not connected: the assistant can answer, but can't act on {serverName} until the session is back.</div>}
      {noticeBox}
      {error && <div className="shrink-0 mx-3 mt-2 text-[12px] text-rose-300 bg-rose-500/10 border border-rose-500/20 rounded-lg px-3 py-2" dir="auto">{error}</div>}
    </>
  );

  // The bar on its own, for the states without a chat.
  const bar = (
    <div className="h-11 px-1 sm:px-2 flex items-center gap-1 sm:gap-1.5 border-b border-white/5 bg-white/[0.02] shrink-0">
      {picker}
      {barEnd}
    </div>
  );

  const activityView = activity && serverUuid && (
    <div data-ai-overlay="" className="absolute inset-0 z-30 flex flex-col bg-[#121214]">
      <ActivityView
        serverUuid={serverUuid}
        serverName={serverName}
        sessionId={liveSession}
        onOpenChat={busyTab ? undefined : (id) => { setChatId(id); setActivity(false); }}
        onClose={() => setActivity(false)}
      />
    </div>
  );

  let body: ReactNode;
  if (!serverUuid) {
    body = (
      <>
        {bar}
        {noticeBox}
        <div className="p-6 text-center text-[12.5px] text-zinc-400">Save this server to use the assistant here. A quick connection has nothing to keep its chats and level with.</div>
      </>
    );
  } else if (providers && providers.length === 0) {
    body = (
      <>
        {bar}
        {noticeBox}
        <div className="p-6 text-center text-[12.5px] text-zinc-400">Add a provider in Settings → AI assistant to use the assistant.</div>
      </>
    );
  } else if (providers === null || !picked) {
    body = (
      <>
        {bar}
        {noticeBox}
        <div className="flex-1 flex items-center justify-center text-zinc-500"><Loader2 size={16} className="animate-spin" /></div>
      </>
    );
  } else if (chatId == null) {
    body = (
      <>
        {bar}
        {banner}
        <div className="flex-1 flex flex-col items-center justify-center gap-3 p-6 text-center">
          <div className="text-[12.5px] text-zinc-400 max-w-sm">Ask the assistant about {serverName}. It works over this session, within the level set for it.</div>
          <button type="button" disabled={!connected || busy} onClick={() => void newChat()} className="h-10 sm:h-9 px-4 rounded-lg text-[11px] font-bold uppercase border border-primary/40 text-primary hover:bg-primary/10 flex items-center gap-1.5 disabled:opacity-40">
            {busy ? <Loader2 size={12} className="animate-spin" /> : <Plus size={12} />} New chat
          </button>
        </div>
      </>
    );
  } else {
    body = (
      <ChatPanel
        key={chatId}
        chatId={chatId}
        providers={providers}
        onChanged={() => void refreshChats()}
        onDeleted={() => { setChatId(null); void refreshChats(); }}
        onInsert={onInsert}
        sessionId={liveSession}
        level={level}
        onRunEvent={onRunEvent}
        attach={attach}
        titleSlot={picker}
        barEnd={barEnd}
        drawerExtra={drawerExtra}
        banner={banner}
        onContinued={(id) => { setChatId(id); void refreshChats(); }}
        onContinuing={setContinuing}
        elsewhere={!running && current?.state === "running"}
      />
    );
  }

  return (
    <div className="relative flex-1 flex flex-col min-h-0">
      {/* Behind the activity view nothing takes the focus or a key. */}
      <div className="flex-1 flex flex-col min-h-0" inert={activity ? "" : undefined}>
        {body}
      </div>
      {activityView}
    </div>
  );
};
