import { useCallback, useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { AlertTriangle, Loader2, MonitorSmartphone, RotateCw, ShieldAlert, X } from "lucide-react";
import { AuthPrompts } from "./AuthPrompts";
import RemoteScreen, { type RemoteScreenHandle } from "./RemoteScreen";
import { readUseSshAgent } from "../util/sshAgent";
import { readVncOptions } from "../util/vncOptions";

// A VNC server's tab: the server's screen in the tab itself (vnc_client.rs),
// straight to it or through the node's SSH server, whose host-key and login
// questions it asks, with the saved VNC password or one asked for here. Plain
// VNC isn't encrypted: the tab says so when it goes straight to the server.

type LogLine = { msg: string; type: string; time: string };
type Status = "connecting" | "connected" | "failed" | "disconnected";

// How vnc_client.rs (explain) says the server refused the password.
const PASSWORD_REFUSED = /refused the password|said: Authentication failed/i;

const NOT_ENCRYPTED = "VNC isn't encrypted: the screen and what is typed cross the network as they are. Through an SSH server, it's private up to that server.";

export default function VncSessionView({ session, displayName, onClose, onStatusChange, isActiveView = false, onPromptChange }: any) {
  const [status, setStatus] = useState<Status>("connecting");
  const [logs, setLogs] = useState<LogLine[]>([]);
  const [reason, setReason] = useState("");
  // Set once connected: the SSH server it goes through, or "" straight.
  const [via, setVia] = useState<string | null>(null);
  const screenRef = useRef<RemoteScreenHandle | null>(null);
  // The saved VNC password was refused: the next connect asks for it (with a
  // "Save password" tick) instead of trying it again.
  const askPassword = useRef(false);
  const passwordRetries = useRef(0);
  // Hidden while the screen shows; open again on request.
  const [showLog, setShowLog] = useState(false);

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
    setVia(null);
    const failed = (e: unknown) => {
      const why = String(e);
      pushLog({ msg: why, type: "error" });
      setReason(why);
      setStatus("failed");
    };
    const screen = screenRef.current;
    if (!screen) return failed("The tab isn't ready yet. Try again.");
    const { width, height } = screen.wantedSize();
    invoke("vnc_open", {
      sessionId: session.id,
      serverId: session.serverId,
      useSshAgent: readUseSshAgent(),
      width,
      height,
      options: readVncOptions(session.serverUuid),
      askPassword: askPassword.current,
      frames: screen.newChannel(),
    }).catch(failed);
  }, [session.id, session.serverId, session.serverUuid, pushLog]);

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
        setVia(e.payload?.via ?? "");
        setStatus("connected");
      }),
      listen(`connection-failed-${id}`, (e: any) => {
        const why = String(e.payload?.reason ?? "The connection failed.");
        askPassword.current = false;
        setReason(why);
        setStatus("failed");
        // The server refused the password: ask for it at once, twice at most.
        if (PASSWORD_REFUSED.test(why) && passwordRetries.current < 2) {
          passwordRetries.current += 1;
          askPassword.current = true;
          setTimeout(() => connect(), 300);
        }
      }),
      listen(`session-disconnected-${id}`, (e: any) => {
        setReason(String(e.payload?.reason ?? "The connection closed."));
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

  const name = displayName || session.serverName;
  const working = status === "connecting";
  const broken = status === "failed" || status === "disconnected";
  const connected = status === "connected";

  return (
    <div data-vnc-status={status} className="relative flex-1 flex flex-col bg-[#0a0a0c] text-white overflow-hidden">
      <div className="shrink-0 h-9 px-2 sm:px-3 flex items-center gap-2 border-b border-white/5 bg-[#0e0e10]">
        <span className={`shrink-0 ${broken ? "text-red-400" : "text-primary"}`} aria-hidden="true">
          {working ? <Loader2 size={14} className="animate-spin" /> : broken ? <AlertTriangle size={14} /> : <MonitorSmartphone size={14} />}
        </span>
        <span className="min-w-0 truncate text-[12px] font-semibold text-zinc-200" title={session.serverName}>{name}</span>
        {connected && via === "" && (
          <span
            className="shrink-0 inline-flex items-center gap-1 h-5 px-1.5 rounded border border-amber-500/30 bg-amber-500/10 text-amber-300 text-[10px] font-bold uppercase tracking-wider"
            title={NOT_ENCRYPTED}
          >
            <ShieldAlert size={10} aria-hidden="true" />
            <span className="hidden sm:inline">Not encrypted</span>
          </span>
        )}
        <span className={`min-w-0 truncate text-[11px] ${broken ? "text-red-400/90" : "text-zinc-500"}`}>
          {working ? "Connecting…" : status === "failed" ? "Connection failed" : status === "disconnected" ? "Disconnected" : via ? `VNC · through ${via}` : "VNC"}
        </span>
        <div className="ml-auto shrink-0 flex items-center gap-1.5">
          {connected && (
            <button
              onClick={() => setShowLog((v) => !v)}
              aria-pressed={showLog}
              title={showLog ? "Hide the log" : "Show the log"}
              className={`h-7 px-2 rounded-md text-[11px] font-semibold transition-colors ${showLog ? "bg-primary/15 text-primary" : "bg-white/5 text-zinc-400 hover:text-zinc-200"}`}
            >
              Log
            </button>
          )}
          {broken && (
            <button
              onClick={connect}
              title="Reconnect"
              aria-label="Reconnect"
              className="h-7 px-2 flex items-center gap-1.5 rounded-md bg-primary/10 text-primary hover:bg-primary/20 text-[11px] font-semibold transition-colors"
            >
              <RotateCw size={12} /><span className="hidden sm:inline">Reconnect</span>
            </button>
          )}
          <button
            onClick={onClose}
            title="Close session"
            aria-label="Close session"
            className="h-7 px-2 flex items-center gap-1.5 rounded-md bg-white/5 text-zinc-300 hover:bg-red-500/15 hover:text-red-300 text-[11px] font-semibold transition-colors"
          >
            <X size={12} /><span className="hidden sm:inline">Close</span>
          </button>
        </div>
      </div>

      <div className="relative flex-1 min-h-0 flex flex-col">
        <RemoteScreen
          ref={screenRef}
          sessionId={session.id}
          active={isActiveView && !promptWaiting}
          connected={connected}
          inputCommand="vnc_input"
          resizeCommand="vnc_resize"
        />
        {(!connected || showLog) && (
          <div className="absolute inset-0 z-10 flex flex-col p-4 sm:p-8 bg-[#0a0a0c]/90">
            <div className="max-w-2xl w-full mx-auto flex-1 flex flex-col min-h-0">
              {broken && reason && <p className="mb-3 sm:mb-4 text-[12.5px] text-red-300/90 break-words">{reason}</p>}
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
            </div>
          </div>
        )}
      </div>

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
