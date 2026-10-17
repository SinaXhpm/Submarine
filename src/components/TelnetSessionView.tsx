import { useCallback, useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { AlertTriangle, Loader2, RotateCw, ScrollText, ShieldAlert, TerminalSquare, X } from "lucide-react";
import { AuthPrompts } from "./AuthPrompts";
import TerminalView from "./TerminalView";
import { readUseSshAgent } from "../util/sshAgent";

// A telnet server's tab (#179). It connects — straight to the server, or
// through the node's SSH server, asking that server's host-key and login
// questions — and then shows a terminal over the connection. Telnet isn't
// encrypted, and the tab says so. The server asks for the login itself, in
// the terminal; a saved password can be typed there with the terminal's
// password button, as at a sudo prompt.

type LogLine = { msg: string; type: string; time: string };
type Status = "connecting" | "connected" | "failed" | "disconnected";

const NOT_ENCRYPTED = "Telnet isn't encrypted: what is typed here, passwords too, crosses the network as plain text.";

export default function TelnetSessionView({ session, displayName, onClose, onStatusChange, isActiveView = false, onPromptChange }: any) {
  const [status, setStatus] = useState<Status>("connecting");
  const [logs, setLogs] = useState<LogLine[]>([]);
  const [reason, setReason] = useState("");
  const [via, setVia] = useState<string | null>(null);
  // The terminal comes with the first connection and stays after a drop,
  // with what it showed; each reconnect opens it again (connectionEpoch).
  const [epoch, setEpoch] = useState(-1);
  const [showLog, setShowLog] = useState(false);
  const [focusTick, setFocusTick] = useState(0);
  // Its id starts with the session's, so a Disconnect ends it with the rest.
  const terminalId = `${session.id}-term-telnet`;

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
  // The terminal takes the keys when the tab comes to the front.
  useEffect(() => { if (isActiveView) setFocusTick((t) => t + 1); }, [isActiveView]);

  const pushLog = useCallback((p: any) => {
    const time = new Date().toLocaleTimeString();
    setLogs((l) => [...l.slice(-499), { msg: String(p?.msg ?? ""), type: String(p?.type ?? "info"), time }]);
  }, []);

  const connect = useCallback(() => {
    setStatus("connecting");
    setReason("");
    pushLog({ msg: NOT_ENCRYPTED, type: "warning" });
    invoke("telnet_connect", { sessionId: session.id, serverId: session.serverId, useSshAgent: readUseSshAgent() }).catch((e) => {
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
        setVia(e.payload?.via ?? null);
        setStatus("connected");
        setEpoch((n) => n + 1);
        setFocusTick((t) => t + 1);
      }),
      listen(`connection-failed-${id}`, (e: any) => {
        setReason(String(e.payload?.reason ?? "The connection failed."));
        setStatus("failed");
      }),
      // The server or the route ended it (with why), or a Disconnect.
      listen(`session-disconnected-${id}`, (e: any) => {
        const why = String(e.payload?.reason ?? "The connection closed.");
        pushLog({ msg: why, type: "error" });
        setReason(why);
        setStatus("disconnected");
      }),
      // The terminal's connection ended; what the server said, if it did,
      // comes as session-disconnected right after.
      listen(`terminal-closed-${terminalId}`, () => {
        setStatus((s) => (s === "connected" ? "disconnected" : s));
        setReason((r) => r || "Disconnected.");
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
  }, [session.id, terminalId, connect, pushLog]);

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

  const name = displayName || session.serverName;
  const working = status === "connecting";
  const broken = status === "failed" || status === "disconnected";
  const statusText = working
    ? "Connecting…"
    : status === "failed"
      ? "Connection failed"
      : status === "disconnected"
        ? "Disconnected"
        : via
          ? `Telnet · through ${via}`
          : "Telnet";

  const logList = (
    <div
      className="flex-1 min-h-0 bg-[#121214] border border-white/5 rounded-2xl p-4 font-mono text-[12px] overflow-y-auto overscroll-contain custom-scrollbar select-text cursor-text"
      style={{ WebkitOverflowScrolling: "touch", lineHeight: 1.5 }}
    >
      {logs.map((l, i) => (
        <div
          key={i}
          className={`mb-2 ${l.type === "error" ? "text-red-400" : l.type === "success" ? "text-primary" : l.type === "warning" ? "text-amber-300/90" : "text-zinc-400"}`}
        >
          <span className="text-zinc-600 opacity-50 mr-3">[{l.time}]</span>
          {l.msg}
        </div>
      ))}
    </div>
  );

  return (
    <div className="relative flex-1 flex flex-col min-h-0 bg-[#0a0a0c] text-white overflow-hidden" data-telnet-status={status}>
      <div className="h-10 shrink-0 flex items-center gap-2 px-3 border-b border-white/5 bg-[#111114]">
        <span className={`shrink-0 ${broken ? "text-red-400" : "text-primary"}`} aria-hidden="true">
          {working ? <Loader2 size={14} className="animate-spin" /> : broken ? <AlertTriangle size={14} /> : <TerminalSquare size={14} />}
        </span>
        <span className="text-[12px] font-semibold text-zinc-100 truncate" title={session.serverName}>{name}</span>
        <span
          className="shrink-0 inline-flex items-center gap-1 h-5 px-1.5 rounded border border-amber-500/30 bg-amber-500/10 text-amber-300 text-[10px] font-bold uppercase tracking-wider"
          title={NOT_ENCRYPTED}
        >
          <ShieldAlert size={10} aria-hidden="true" />
          <span className="hidden sm:inline">Not encrypted</span>
        </span>
        <span className={`min-w-0 flex-1 truncate text-[11px] ${broken ? "text-red-400/90" : "text-zinc-500"}`}>{statusText}</span>
        {epoch >= 0 && (
          <button
            onClick={() => setShowLog((v) => !v)}
            aria-pressed={showLog}
            title="The connection log"
            className={`h-7 px-2 flex items-center gap-1.5 rounded-lg text-xs font-semibold transition-colors ${showLog ? "bg-primary/15 text-primary" : "bg-white/5 text-zinc-300 hover:bg-white/10"}`}
          >
            <ScrollText size={13} /><span className="hidden sm:inline">Log</span>
          </button>
        )}
        {broken && (
          <button
            onClick={connect}
            title="Reconnect"
            aria-label="Reconnect"
            className="h-7 px-2 flex items-center gap-1.5 rounded-lg bg-primary/10 text-primary hover:bg-primary/20 text-xs font-semibold transition-colors"
          >
            <RotateCw size={13} /><span className="hidden sm:inline">Reconnect</span>
          </button>
        )}
        <button
          onClick={onClose}
          title="Close session"
          aria-label="Close session"
          className="h-7 px-2 flex items-center gap-1.5 rounded-lg bg-white/5 text-zinc-300 hover:bg-red-500/15 hover:text-red-300 text-xs font-semibold transition-colors"
        >
          <X size={13} /><span className="hidden sm:inline">Close</span>
        </button>
      </div>

      {epoch >= 0 ? (
        <div className="flex-1 min-h-0 relative">
          <TerminalView
            sessionId={session.id}
            terminalId={terminalId}
            telnet
            disabled={status !== "connected"}
            isActive
            isActiveView={isActiveView}
            focusSignal={focusTick}
            connectionEpoch={epoch}
            serverId={session.serverId}
            serverName={session.serverName}
          />
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
          aria-label={`Connection prompt for ${session.serverName}`}
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
