import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { History, Plus, Search, Settings, Sparkles, Trash2, Loader2, Server, X } from "lucide-react";
import { useConfirm } from "../../ui/confirm";
import { useIsCompact } from "../../hooks/useViewport";
import { ChatPanel } from "./ChatPanel";
import { ActivityView } from "./ActivityView";
import { AiSettings } from "./AiSettings";
import { ai, errText, AI_OPEN_EVENT, AI_OPEN_SERVER_EVENT, type AiOpenDetail, type AiOpenServerDetail, type ChatSummary, type ProviderView } from "../../util/aiApi";

// The AI page: every chat, saved on this device, in one list. Kept mounted
// while the app runs (as Monitor is), so a reply going on in the background
// keeps its place. A server's chat acts through that server's session when it
// is connected, and offers to open it when it isn't.

const ago = (unix: number) => {
  const s = Math.max(0, Date.now() / 1000 - unix);
  if (s < 60) return "now";
  if (s < 3600) return `${Math.floor(s / 60)} min`;
  if (s < 86400) return `${Math.floor(s / 3600)} h`;
  return new Date(unix * 1000).toLocaleDateString();
};

interface Props {
  active: boolean;
  /** The connected SSH session of each saved server, by uuid. */
  openSessions?: Record<string, string>;
  /** Every saved server's name, by uuid, for the activity of all of them. */
  serverNames?: Record<string, string>;
}

export const AiPage = ({ active, openSessions, serverNames }: Props) => {
  const confirm = useConfirm();
  const compact = useIsCompact();
  const [providers, setProviders] = useState<ProviderView[] | null>(null);
  const [chats, setChats] = useState<ChatSummary[]>([]);
  const [open, setOpen] = useState<number | null>(null);
  const [search, setSearch] = useState("");
  const [filter, setFilter] = useState<string>("all");
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [activity, setActivity] = useState(false);
  // The providers and their keys, over the page: the same as Settings → AI
  // assistant, without leaving it.
  const [settingsOpen, setSettingsOpen] = useState(false);
  // The open chat is answering: its cards wait on this view, so it stays.
  const [pageBusy, setPageBusy] = useState(false);
  const [held, setHeld] = useState(false);
  const busyRef = useRef(false);
  busyRef.current = pageBusy;
  const openRef = useRef(open);
  openRef.current = open;
  useEffect(() => { if (!pageBusy) setHeld(false); }, [pageBusy]);

  /** Opens a chat, unless the open one is answering. */
  const switchTo = useCallback((id: number | null) => {
    if (busyRef.current && id !== openRef.current) {
      setHeld(true);
      return false;
    }
    setHeld(false);
    setOpen(id);
    return true;
  }, []);

  const reloadChats = useCallback(async () => {
    try {
      const f = filter === "all" ? { kind: "all" as const } : filter === "plain" ? { kind: "plain" as const } : { kind: "server" as const, server_uuid: filter };
      setChats(await ai.chats(f, search));
      setError(null);
    } catch (e) {
      setError(errText(e));
    }
  }, [filter, search]);

  const reloadProviders = useCallback(async () => {
    try { setProviders(await ai.providers()); } catch (e) { setError(errText(e)); setProviders([]); }
  }, []);

  useEffect(() => {
    if (!active) return;
    void reloadProviders();
    void reloadChats();
    ai.notice().then((n) => n && setNotice(n)).catch(() => {});
  }, [active, reloadProviders, reloadChats]);

  // "All chats" in a session's AI tab: the page shows, on that chat.
  useEffect(() => {
    const onOpen = (e: Event) => {
      const id = (e as CustomEvent<AiOpenDetail>).detail?.chatId;
      setActivity(false);
      if (typeof id === "number" && switchTo(id)) {
        setFilter("all");
        setSearch("");
      }
    };
    window.addEventListener(AI_OPEN_EVENT, onOpen);
    return () => window.removeEventListener(AI_OPEN_EVENT, onOpen);
  }, [switchTo]);

  // The list refreshes while a chat answers in the background.
  useEffect(() => {
    if (!active || !chats.some((c) => c.state === "running")) return;
    const t = window.setInterval(() => void reloadChats(), 3000);
    return () => window.clearInterval(t);
  }, [active, chats, reloadChats]);

  const servers = useMemo(() => {
    const m = new Map<string, string>();
    for (const c of chats) if (c.server_uuid) m.set(c.server_uuid, c.server_label ?? "Server");
    return [...m.entries()];
  }, [chats]);

  const newChat = async () => {
    if (busyRef.current) {
      setHeld(true);
      return;
    }
    try {
      const c = await ai.createChat(null, null);
      switchTo(c.id);
      await reloadChats();
    } catch (e) {
      setError(errText(e));
    }
  };

  const openServer = (serverUuid: string, chatId: number) => {
    if (serverNames && !serverNames[serverUuid]) {
      setError("This chat's server no longer exists.");
      return;
    }
    window.dispatchEvent(new CustomEvent<AiOpenServerDetail>(AI_OPEN_SERVER_EVENT, { detail: { serverUuid, chatId } }));
  };

  const deleteAll = async () => {
    const ok = await confirm({
      title: "Delete every chat?",
      message: "All chats are removed from this device. Providers and keys stay.\n\nThe activity log keeps the commands the assistant ran and the earlier content of files it changed, for up to 90 days. You can clear the log in Activity.",
      okLabel: "Delete all",
      destructive: true,
      safeDefault: true,
    });
    if (!ok) return;
    const clear = await confirm({
      title: "Clear their activity too?",
      message: "The activity log keeps the commands these chats ran and the earlier content of files they changed, for up to 90 days. Clear them now? Then their changes can't be undone.",
      okLabel: "Clear them",
      cancelLabel: "Keep them",
      destructive: true,
      safeDefault: true,
    });
    try { await ai.deleteAllChats(clear); setOpen(null); await reloadChats(); } catch (e) { setError(errText(e)); }
  };

  if (providers === null) {
    return <div className="flex-1 flex items-center justify-center text-zinc-500"><Loader2 size={16} className="animate-spin" /></div>;
  }

  // The first provider is added right here: the page goes on to its chats as
  // soon as it is saved.
  if (providers.length === 0) {
    return (
      <div data-ai-setup="" className="flex-1 overflow-y-auto custom-scrollbar p-4 sm:p-6">
        <div className="max-w-xl mx-auto space-y-4">
          <div className="flex items-center gap-2 text-zinc-100 font-semibold"><Sparkles size={18} className="text-primary" /> AI assistant</div>
          <p className="text-[13px] text-zinc-400">Add a provider first: Anthropic, OpenAI, another OpenAI-compatible service, or a model on your own network, with your own API key.</p>
          <AiSettings embedded onChanged={setProviders} />
        </div>
      </div>
    );
  }

  const showList = !compact || open === null;
  const showChat = !compact || open !== null;

  return (
    <div className="relative flex-1 flex min-h-0 min-w-0">
      {settingsOpen && (
        <div data-ai-overlay="" data-ai-settings="" className="absolute inset-0 z-30 flex flex-col bg-[#121214]">
          <div className="h-11 px-3 flex items-center gap-2 border-b border-white/5 bg-white/[0.02] shrink-0">
            <Settings size={14} className="text-zinc-400 shrink-0" />
            <div className="flex-1 min-w-0 truncate text-[11px] font-black text-zinc-400 uppercase tracking-wider">AI assistant settings</div>
            <button type="button" onClick={() => setSettingsOpen(false)} aria-label="Close AI settings"
              className={`${compact ? "h-10 w-10" : "h-8 w-8"} rounded-lg text-zinc-500 hover:text-white hover:bg-white/5 flex items-center justify-center`}>
              <X size={15} />
            </button>
          </div>
          <div className="flex-1 overflow-y-auto custom-scrollbar p-4 sm:p-6">
            <div className="max-w-xl mx-auto"><AiSettings embedded onChanged={setProviders} /></div>
          </div>
        </div>
      )}
      {activity && (
        <div data-ai-overlay="" className="absolute inset-0 z-30 flex flex-col bg-[#121214]">
          <ActivityView
            serverUuid={null}
            serverNames={{ ...Object.fromEntries(servers), ...serverNames }}
            sessions={openSessions}
            onOpenChat={(id) => { if (switchTo(id)) setActivity(false); }}
            onClose={() => setActivity(false)}
          />
        </div>
      )}
      {/* Behind the activity view nothing takes the focus or a key. */}
      <div className="flex-1 flex min-h-0 min-w-0" inert={activity || settingsOpen ? "" : undefined}>
      {showList && (
        <div className={`${compact ? "flex-1 min-w-0" : "w-72 shrink-0 border-r border-white/5"} flex flex-col min-h-0`}>
          <div className="p-3 space-y-2 border-b border-white/5">
            <div className="flex items-center gap-2">
              <div className="flex-1 text-[11px] font-black text-zinc-500 uppercase tracking-wider flex items-center gap-1.5"><Sparkles size={13} /> Chats</div>
              <button type="button" aria-label="AI settings" title="AI providers and keys" onClick={() => setSettingsOpen(true)}
                className={`${compact ? "h-10 w-10" : "h-8 w-8"} rounded-lg border border-white/10 text-zinc-400 hover:text-zinc-100 hover:bg-white/5 flex items-center justify-center`}>
                <Settings size={14} />
              </button>
              <button type="button" aria-label="Activity" title="What the assistant did on your servers" onClick={() => setActivity(true)}
                className={`${compact ? "h-10 w-10" : "h-8 w-8"} rounded-lg border border-white/10 text-zinc-400 hover:text-zinc-100 hover:bg-white/5 flex items-center justify-center`}>
                <History size={14} />
              </button>
              <button type="button" onClick={() => void newChat()} className={`${compact ? "h-10" : "h-8"} px-3 rounded-lg text-[11px] font-bold uppercase bg-primary/10 border border-primary/30 text-primary hover:bg-primary hover:text-zinc-950 transition-all flex items-center gap-1.5`}>
                <Plus size={12} /> New chat
              </button>
            </div>
            <div className="relative">
              <Search size={13} className="absolute left-2.5 top-1/2 -translate-y-1/2 text-zinc-500" />
              <input
                value={search}
                onChange={(e) => setSearch(e.target.value)}
                placeholder="Search titles"
                dir="auto"
                className={`w-full ${compact ? "h-10" : "h-8"} pl-8 pr-2 bg-zinc-950 border border-white/10 rounded-lg text-[12px] text-zinc-200 focus:outline-none focus:border-primary/50`}
              />
            </div>
            {servers.length > 0 && (
              <select value={filter} onChange={(e) => setFilter(e.target.value)} className={`w-full ${compact ? "h-10" : "h-8"} px-2 bg-zinc-950 border border-white/10 rounded-lg text-[12px] text-zinc-300`}>
                <option value="all">All chats</option>
                <option value="plain">Plain chats</option>
                {servers.map(([uuid, name]) => <option key={uuid} value={uuid}>{name}</option>)}
              </select>
            )}
          </div>
          {notice && <div className="m-2 text-[11.5px] text-amber-300 bg-amber-500/10 border border-amber-500/20 rounded-lg px-3 py-2">{notice}</div>}
          {held && <div className="m-2 text-[11.5px] text-amber-300 bg-amber-500/10 border border-amber-500/20 rounded-lg px-3 py-2">The open chat is answering. Wait for it, or stop it, to open another.</div>}
          {error && <div className="m-2 text-[11.5px] text-rose-300 bg-rose-500/10 border border-rose-500/20 rounded-lg px-3 py-2">{error}</div>}
          <div className="flex-1 min-h-0 overflow-y-auto p-2 space-y-1">
            {chats.length === 0 && <p className="text-[12px] text-zinc-500 text-center mt-6">No chats yet.</p>}
            {chats.map((c) => (
              <button
                type="button"
                key={c.id}
                onClick={() => switchTo(c.id)}
                className={`w-full text-left px-3 py-2 rounded-xl border transition-all ${open === c.id ? "bg-primary/10 border-primary/20" : "border-transparent hover:bg-white/[0.04]"}`}
              >
                <div className="flex items-center gap-1.5">
                  {c.state === "running" && <Loader2 size={11} className="animate-spin text-primary shrink-0" />}
                  {c.state === "failed" && <span className="h-1.5 w-1.5 rounded-full bg-rose-400 shrink-0" title="No answer" />}
                  <span className="text-[12.5px] text-zinc-100 truncate" dir="auto">{c.title}</span>
                </div>
                <div className="text-[10.5px] text-zinc-500 truncate flex items-center gap-1">
                  {c.server_label ? <><Server size={10} /> {c.server_label} ·</> : null}
                  {c.provider_name ?? "no provider"} · {c.model} · {ago(c.last_activity)}
                </div>
              </button>
            ))}
          </div>
          {chats.length > 0 && (
            <div className="p-2 border-t border-white/5">
              <button type="button" onClick={() => void deleteAll()} className={`w-full ${compact ? "h-10" : "h-8"} rounded-lg text-[11px] font-bold uppercase text-zinc-500 hover:text-rose-300 hover:bg-rose-500/10 flex items-center justify-center gap-1.5`}>
                <Trash2 size={12} /> Delete all
              </button>
            </div>
          )}
        </div>
      )}
      {showChat && (
        open !== null ? (
          <ChatPanel
            key={open}
            chatId={open}
            providers={providers}
            onChanged={() => void reloadChats()}
            onDeleted={() => { setOpen(null); void reloadChats(); }}
            onBack={compact ? () => void switchTo(null) : undefined}
            sessions={openSessions ?? {}}
            onContinued={(id) => { setOpen(id); void reloadChats(); }}
            onOpenServer={openServer}
            onBusy={setPageBusy}
            elsewhere={!pageBusy && chats.find((c) => c.id === open)?.state === "running"}
          />
        ) : (
          <div className="flex-1 flex items-center justify-center text-[13px] text-zinc-500">Pick a chat, or start a new one.</div>
        )
      )}
      </div>
    </div>
  );
};
