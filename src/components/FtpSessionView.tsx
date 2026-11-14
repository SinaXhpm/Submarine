import { useCallback, useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { AlertTriangle, FolderOpen, FolderUp, Loader2, Lock, RotateCw, ScrollText, ShieldAlert, Sparkles, X } from "lucide-react";
import { AuthPrompts } from "./AuthPrompts";
import { AiPanel } from "./ai/AiPanel";
import { AI_FOCUS_EVENT, takeAiFocus } from "../util/aiEvents";
import { IS_MOBILE } from "../util/platform";
import SftpWorkspace, { type SftpView } from "./SftpWorkspace";
import { parseObjectList } from "../util/serverJson";
import { readUseSshAgent } from "../util/sshAgent";
import { tk, useT } from "../i18n";

// An FTP server's tab: FTPS (TLS after connecting, or from the start) or
// plain FTP. It connects — straight to the server, or through the node's SSH
// server, asking that server's questions — logs in (the saved password, or
// one asked for here), and shows the server's files in the same file browser
// an SSH server's tab has. Under FTPS a certificate that no public authority
// vouches for is shown to check once, like an SSH host key. Plain FTP isn't
// encrypted, and the tab says so. The backend is ftp_connect in lib.rs.
//
// The AI assistant works here too, with its file tools only (FTP runs no
// commands): its panel sits beside the files, or over them on a phone.

type LogLine = { msg: string; type: string; time: string };
type Status = "connecting" | "connected" | "failed" | "disconnected";
type Opened = { address?: string; via?: string | null; security?: string; tls?: string | null; home?: string };

const NOT_ENCRYPTED = tk("Plain FTP isn't encrypted: the password and the files cross the network as they are.");
// What the backend says when the server refused the login (lib.rs ftp_connect).
const LOGIN_REFUSED = /^The server refused the login/;

export default function FtpSessionView({ session, profile, displayName, onClose, onStatusChange, isActiveView = false, onPromptChange }: any) {
  const t = useT();
  const [status, setStatus] = useState<Status>("connecting");
  const [logs, setLogs] = useState<LogLine[]>([]);
  const [reason, setReason] = useState("");
  const [opened, setOpened] = useState<Opened | null>(null);
  // The files come with the first connection and stay (greyed) after a drop.
  const [everConnected, setEverConnected] = useState(false);
  const [showLog, setShowLog] = useState(false);
  // The files, or the folder mirror (a saved server's, as in an SSH tab).
  const [view, setView] = useState<SftpView>("files");
  // The AI assistant: shown or not; mounted the first time it shows, then kept
  // (a run goes on while it is hidden). The AI page's "Open the server" opens
  // it on the chat it asks for.
  const [aiOpen, setAiOpen] = useState(false);
  const [aiMounted, setAiMounted] = useState(false);
  const [aiOpenChat, setAiOpenChat] = useState<{ id: number | null; n: number } | null>(null);
  const [aiPending, setAiPending] = useState(0);
  useEffect(() => { if (aiOpen) setAiMounted(true); }, [aiOpen]);
  useEffect(() => {
    const take = () => {
      const chat = takeAiFocus(session.id);
      if (chat === undefined) return;
      setAiOpen(true);
      setAiOpenChat((prev) => ({ id: chat, n: (prev?.n ?? 0) + 1 }));
    };
    take();
    const onFocus = (e: Event) => {
      if ((e as CustomEvent<{ sessionId: string }>).detail?.sessionId === session.id) take();
    };
    window.addEventListener(AI_FOCUS_EVENT, onFocus);
    return () => window.removeEventListener(AI_FOCUS_EVENT, onFocus);
  }, [session.id]);
  // As wide as the SSH tab's AI panel (the same setting).
  const [aiWidth] = useState<number>(() => {
    try {
      const saved = parseInt(localStorage.getItem("submarine-ai-panel-width") || "", 10);
      if (Number.isFinite(saved) && saved >= 320) return saved;
    } catch { /* the default below */ }
    return Math.max(460, Math.min(900, Math.round((window.innerWidth || 1440) * 0.45)));
  });
  // Asked again after the saved password was refused: twice at most.
  const askPassword = useRef(false);
  const passwordRetries = useRef(0);

  const [fingerprintPrompt, setFingerprintPrompt] = useState<any>(null);
  const [kbiPrompt, setKbiPrompt] = useState<any>(null);
  const [kbiValues, setKbiValues] = useState<string[]>([]);
  const fpNonceRef = useRef<string | null>(null);
  const kbiNonceRef = useRef<string | null>(null);
  const kbiFirstInputRef = useRef<HTMLInputElement | null>(null);
  const promptWaiting = !!(fingerprintPrompt || kbiPrompt);

  useEffect(() => { onStatusChange?.(session.id, status); }, [onStatusChange, session.id, status]);
  useEffect(() => { onPromptChange?.(session.id, promptWaiting); }, [onPromptChange, session.id, promptWaiting]);
  useEffect(() => () => onPromptChange?.(session.id, false), [onPromptChange, session.id]);
  useEffect(() => {
    if (kbiPrompt && isActiveView) kbiFirstInputRef.current?.focus();
  }, [kbiPrompt, isActiveView]);

  const pushLog = useCallback((p: any) => {
    const time = new Date().toLocaleTimeString();
    setLogs((l) => [...l.slice(-499), { msg: String(p?.msg ?? ""), type: String(p?.type ?? "info"), time }]);
  }, []);

  const connect = useCallback(() => {
    setStatus("connecting");
    setReason("");
    invoke("ftp_connect", {
      sessionId: session.id,
      serverId: session.serverId,
      useSshAgent: readUseSshAgent(),
      askPassword: askPassword.current,
    }).catch((e) => {
      const why = String(e);
      pushLog({ msg: why, type: "error" });
      setReason(why);
      setStatus("failed");
    });
  }, [session.id, session.serverId, pushLog]);

  // Listen first, then connect: a result that comes back at once must find
  // its listener (see #167).
  useEffect(() => {
    let cancelled = false;
    const id = session.id;
    const subs = Promise.all([
      listen(`session-log-${id}`, (e: any) => pushLog(e.payload)),
      listen(`fingerprint-prompt-${id}`, (e: any) => {
        fpNonceRef.current = e.payload?.nonce ?? null;
        setFingerprintPrompt(e.payload);
      }),
      listen(`fingerprint-prompt-dismiss-${id}`, (e: any) => {
        const nonce = e.payload?.nonce;
        if (nonce && fpNonceRef.current && nonce !== fpNonceRef.current) return;
        fpNonceRef.current = null;
        setFingerprintPrompt(null);
      }),
      listen(`kbi-prompt-${id}`, (e: any) => {
        const p = e.payload;
        kbiNonceRef.current = p?.nonce ?? null;
        setKbiPrompt(p);
        setKbiValues(Array.isArray(p?.prompts) ? p.prompts.map(() => "") : []);
      }),
      listen(`kbi-prompt-dismiss-${id}`, (e: any) => {
        const nonce = e.payload?.nonce;
        if (nonce && kbiNonceRef.current && nonce !== kbiNonceRef.current) return;
        kbiNonceRef.current = null;
        setKbiPrompt(null);
        setKbiValues([]);
      }),
      listen(`connection-success-${id}`, (e: any) => {
        askPassword.current = false;
        passwordRetries.current = 0;
        setOpened((e.payload ?? {}) as Opened);
        setStatus("connected");
        setEverConnected(true);
      }),
      listen(`connection-failed-${id}`, (e: any) => {
        const why = String(e.payload?.reason ?? t("The connection failed."));
        askPassword.current = false;
        setReason(why);
        setStatus("failed");
        // The server refused the login: ask for the password at once.
        if (e.payload?.auth && LOGIN_REFUSED.test(why) && passwordRetries.current < 2) {
          passwordRetries.current += 1;
          askPassword.current = true;
          setTimeout(() => connect(), 300);
        }
      }),
      // The server can't be reached any more (with why), or a Disconnect.
      listen(`session-disconnected-${id}`, (e: any) => {
        const why = String(e.payload?.reason ?? t("The connection closed."));
        pushLog({ msg: why, type: "error" });
        setReason(why);
        setStatus("disconnected");
      }),
    ]);
    subs.then(() => { if (!cancelled) connect(); });
    // The tab menu's Reconnect.
    const onReconnect = () => connect();
    window.addEventListener(`session-reconnect-${id}`, onReconnect);
    return () => {
      cancelled = true;
      window.removeEventListener(`session-reconnect-${id}`, onReconnect);
      subs.then((fns) => fns.forEach((f) => f()));
      invoke("disconnect_session", { sessionId: id }).catch(console.error);
    };
  }, [session.id, connect, pushLog]);

  const handleFingerprintResponse = async (accepted: boolean) => {
    const nonce = fingerprintPrompt?.nonce;
    fpNonceRef.current = null;
    setFingerprintPrompt(null);
    if (nonce) await invoke("verify_fingerprint_response", { nonce, accepted }).catch(console.error);
  };
  const answerKbi = async (responses: string[] | null) => {
    const nonce = kbiPrompt?.nonce;
    kbiNonceRef.current = null;
    setKbiPrompt(null);
    setKbiValues([]);
    if (nonce) await invoke("submit_kbi_response", { nonce, responses }).catch(console.error);
  };
  const reconnect = () => {
    passwordRetries.current = 0;
    connect();
  };

  const name = displayName || session.serverName;
  const working = status === "connecting";
  const broken = status === "failed" || status === "disconnected";
  const plain = session.protocol === "ftp";
  const kind = plain ? "FTP" : "FTPS";
  const statusText = working
    ? t("Connecting…")
    : status === "failed"
      ? t("Connection failed")
      : status === "disconnected"
        ? t("Disconnected")
        : [opened?.tls ? `${kind} · ${opened.tls}` : kind, opened?.via ? t("through {via}", { via: opened.via }) : null]
            .filter(Boolean)
            .join(" · ");

  const logList = (
    <div
      className="flex-1 min-h-0 bg-[#121214] border border-white/5 rounded-2xl p-4 font-mono text-[12px] overflow-y-auto overscroll-contain custom-scrollbar select-text cursor-text"
      style={{ WebkitOverflowScrolling: "touch", lineHeight: 1.5 }}
    >
      {logs.map((l, i) => (
        <div
          key={i}
          className={`mb-2 ${l.type === "error" ? "text-red-400" : l.type === "success" ? "text-primary" : l.type === "warning" || l.type === "warn" ? "text-amber-300/90" : "text-zinc-400"}`}
        >
          <span className="text-zinc-600 opacity-50 mr-3">[{l.time}]</span>
          {l.msg}
        </div>
      ))}
    </div>
  );

  return (
    <div className="relative flex-1 flex flex-col min-h-0 bg-[#0a0a0c] text-white overflow-hidden" data-ftp-status={status}>
      <div className="h-10 shrink-0 flex items-center gap-2 px-3 border-b border-white/5 bg-[#141417]">
        <span className={`shrink-0 ${broken ? "text-red-400" : "text-primary"}`} aria-hidden="true">
          {working ? <Loader2 size={14} className="animate-spin-brief" /> : broken ? <AlertTriangle size={14} /> : <FolderOpen size={14} />}
        </span>
        <span className="text-[12px] font-semibold text-zinc-100 truncate" title={session.serverName}>{name}</span>
        {plain ? (
          <span
            className="shrink-0 inline-flex items-center gap-1 h-5 px-1.5 rounded border border-amber-500/30 bg-amber-500/10 text-amber-300 text-[10px] font-bold uppercase tracking-wider"
            title={t(NOT_ENCRYPTED)}
          >
            <ShieldAlert size={10} aria-hidden="true" />
            <span className="hidden sm:inline">{t("Not encrypted")}</span>
          </span>
        ) : (
          <span
            className="shrink-0 inline-flex items-center gap-1 h-5 px-1.5 rounded border border-teal-500/30 bg-teal-500/10 text-teal-300 text-[10px] font-bold uppercase tracking-wider"
            title={t("FTP over TLS: the login and the files are encrypted.")}
          >
            <Lock size={10} aria-hidden="true" />
            <span className="hidden sm:inline">TLS</span>
          </span>
        )}
        <span className={`min-w-0 flex-1 truncate text-[11px] ${broken ? "text-red-400/90" : "text-zinc-500"}`}>{statusText}</span>
        {everConnected && session.serverUuid && (
          <button
            onClick={() => setAiOpen((v) => !v)}
            aria-pressed={aiOpen}
            aria-label={t("AI")}
            title={t("AI — the assistant for this server")}
            className={`relative h-10 min-w-10 justify-center sm:h-7 sm:min-w-0 px-2 flex items-center gap-1.5 rounded-lg text-xs font-semibold transition-colors ${aiOpen ? "bg-primary/15 text-primary" : "bg-white/5 text-zinc-300 hover:bg-white/10"}`}
          >
            <Sparkles size={13} /><span className="hidden sm:inline">{t("AI")}</span>
            {aiPending > 0 && !aiOpen && <span className="absolute -top-1 -right-1 min-w-[16px] h-4 px-1 rounded-full bg-amber-400 text-black text-[10px] font-bold leading-4 text-center">{aiPending}</span>}
          </button>
        )}
        {everConnected && (
          <button
            onClick={() => setView((v) => (v === "mirror" ? "files" : "mirror"))}
            disabled={!session.serverId}
            aria-pressed={view === "mirror"}
            aria-label={t("Mirror")}
            title={!session.serverId
              ? t("Mirror needs a saved server")
              : view === "mirror" ? t("Back to the files") : t("Mirror: keep a local folder copied to this server")}
            className={`h-10 min-w-10 justify-center sm:h-7 sm:min-w-0 px-2 flex items-center gap-1.5 rounded-lg text-xs font-semibold transition-colors disabled:opacity-40 ${view === "mirror" ? "bg-primary/15 text-primary" : "bg-white/5 text-zinc-300 hover:bg-white/10"}`}
          >
            <FolderUp size={13} /><span className="hidden sm:inline">{t("Mirror")}</span>
          </button>
        )}
        {everConnected && (
          <button
            onClick={() => setShowLog((v) => !v)}
            aria-pressed={showLog}
            title={t("The connection log")}
            className={`h-10 min-w-10 justify-center sm:h-7 sm:min-w-0 px-2 flex items-center gap-1.5 rounded-lg text-xs font-semibold transition-colors ${showLog ? "bg-primary/15 text-primary" : "bg-white/5 text-zinc-300 hover:bg-white/10"}`}
          >
            <ScrollText size={13} /><span className="hidden sm:inline">{t("Log")}</span>
          </button>
        )}
        {broken && (
          <button
            onClick={reconnect}
            title={t("Reconnect")}
            aria-label={t("Reconnect")}
            className="h-10 min-w-10 justify-center sm:h-7 sm:min-w-0 px-2 flex items-center gap-1.5 rounded-lg bg-primary/10 text-primary hover:bg-primary/20 text-xs font-semibold transition-colors"
          >
            <RotateCw size={13} /><span className="hidden sm:inline">{t("Reconnect")}</span>
          </button>
        )}
        <button
          onClick={onClose}
          title={t("Close session")}
          aria-label={t("Close session")}
          className="h-10 min-w-10 justify-center sm:h-7 sm:min-w-0 px-2 flex items-center gap-1.5 rounded-lg bg-white/5 text-zinc-300 hover:bg-red-500/15 hover:text-red-300 text-xs font-semibold transition-colors"
        >
          <X size={13} /><span className="hidden sm:inline">{t("Close")}</span>
        </button>
      </div>

      {everConnected ? (
        <div className="flex-1 min-h-0 relative flex">
          <div className={`flex-1 min-w-0 min-h-0 relative flex flex-col ${aiOpen && IS_MOBILE ? "hidden" : ""}`}>
            <SftpWorkspace
              sessionId={session.id}
              disabled={status !== "connected"}
              serverId={session.serverId}
              profile={profile}
              mirrorsConfig={parseObjectList(session.mirrors)}
              view={view}
              ftp
            />
          </div>
          {aiMounted && session.serverUuid && (
            <div
              data-ftp-ai=""
              className={aiOpen ? (IS_MOBILE ? "flex-1 min-w-0 min-h-0 flex flex-col" : "shrink-0 min-h-0 flex flex-col border-l border-white/10") : "hidden"}
              style={aiOpen && !IS_MOBILE ? { width: `${aiWidth}px`, maxWidth: "60%" } : undefined}
            >
              <AiPanel
                sessionId={session.id}
                serverUuid={session.serverUuid}
                serverName={session.serverName}
                connected={status === "connected"}
                active={aiOpen && isActiveView}
                onPending={setAiPending}
                onClose={() => setAiOpen(false)}
                openChat={aiOpenChat}
              />
            </div>
          )}
          {broken && reason && (
            <div className="absolute left-3 right-3 top-3 z-20 px-3 py-2 rounded-lg border border-red-500/20 bg-[#1a0f10]/95 text-[12px] text-red-300/90 break-words">
              {reason}
            </div>
          )}
          {showLog && (
            <div className="absolute inset-0 z-30 flex flex-col p-3 sm:p-4 bg-[#0a0a0c]/95">
              {logList}
            </div>
          )}
        </div>
      ) : (
        <div className="flex-1 min-h-0 flex flex-col p-4 sm:p-8">
          <div className="max-w-2xl w-full mx-auto flex-1 flex flex-col min-h-0">
            {broken && reason && <p className="mb-3 sm:mb-4 text-[12.5px] text-red-300/90 break-words">{reason}</p>}
            {logList}
          </div>
        </div>
      )}

      {promptWaiting && (
        <div
          role="dialog"
          aria-label={t("Connection prompt for {name}", { name: session.serverName })}
          className="absolute inset-0 z-40 flex items-start justify-center bg-black/70 backdrop-blur-sm p-4 sm:p-8 overflow-y-auto"
        >
          <div className="max-w-2xl w-full mt-6 sm:mt-12">
            <AuthPrompts
              fingerprintPrompt={fingerprintPrompt}
              kbiPrompt={kbiPrompt}
              kbiValues={kbiValues}
              setKbiValues={setKbiValues}
              kbiFirstInputRef={kbiFirstInputRef}
              handleFingerprintResponse={handleFingerprintResponse}
              handleKbiSubmit={() => answerKbi(kbiValues)}
              handleKbiCancel={() => answerKbi(null)}
              serverName={session.serverName}
              displayName={displayName}
            />
          </div>
        </div>
      )}
    </div>
  );
}
