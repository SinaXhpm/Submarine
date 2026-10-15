import { useCallback, useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { AlertTriangle, ExternalLink, Loader2, MonitorSmartphone, RotateCw, X } from "lucide-react";
import { AuthPrompts } from "./AuthPrompts";
import { readUseSshAgent } from "../util/sshAgent";
import { IS_ANDROID } from "../util/platform";
import { describeRdpOptions, readRdpOptions } from "../util/rdpOptions";

// An RDP server's tab (issue #3). Submarine opens the system's RDP app, going
// through the node's SSH server when it has one (a forward on a loopback port
// of this node's own, open while the tab is). The tab shows how that went,
// asks the SSH server's host-key and login questions, and opens the RDP app
// again on request. The RDP app asks for the RDP password itself.

type Opened = { app: string; address: string; via: string | null; tracked: boolean };
type LogLine = { msg: string; type: string; time: string };

export default function RdpSessionView({ session, displayName, onClose, onStatusChange, isActiveView = false, onPromptChange }: any) {
  // "idle" on Android: nothing connects there (yet).
  const [status, setStatus] = useState<"idle" | "connecting" | "connected" | "failed" | "disconnected">(IS_ANDROID ? "idle" : "connecting");
  const [logs, setLogs] = useState<LogLine[]>([]);
  const [opened, setOpened] = useState<Opened | null>(null);
  const [reason, setReason] = useState("");
  // Set when an RDP window this tab can follow closed: its last words.
  const [appClosed, setAppClosed] = useState<string[] | null>(null);
  // Set when an app that hands the file on (macOS `open`, Remmina) failed.
  const [appFailed, setAppFailed] = useState<string[] | null>(null);
  const [busy, setBusy] = useState(false);
  // The options the RDP app was last opened with (this device's, read then).
  const [usedOptions, setUsedOptions] = useState("");

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
    setOpened(null);
    setAppClosed(null);
    setAppFailed(null);
    setReason("");
    const options = readRdpOptions(session.serverUuid);
    setUsedOptions(describeRdpOptions(options));
    invoke("initiate_rdp", { sessionId: session.id, serverId: session.serverId, useSshAgent: readUseSshAgent(), options }).catch((e) => {
      const why = String(e);
      pushLog({ msg: why, type: "error" });
      setReason(why);
      setStatus("failed");
    });
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
      // connect() cleared the notes below: an app that ends at once may
      // already have reported, ahead of this.
      listen(`connection-success-${id}`, (e: any) => {
        setOpened(e.payload as Opened);
        setStatus("connected");
      }),
      listen(`connection-failed-${id}`, (e: any) => {
        setReason(String(e.payload?.reason ?? "The connection failed."));
        setStatus("failed");
      }),
      listen(`session-disconnected-${id}`, (e: any) => {
        setReason(String(e.payload?.reason ?? "The connection closed."));
        setStatus("disconnected");
      }),
      listen(`rdp-app-exited-${id}`, (e: any) => {
        setAppClosed(Array.isArray(e.payload?.said) ? e.payload.said : []);
      }),
      listen(`rdp-app-failed-${id}`, (e: any) => {
        setAppFailed(Array.isArray(e.payload?.said) ? e.payload.said : []);
      }),
    ]);
    // Android: nothing to open yet (the built-in client comes later).
    subs.then(() => { if (!cancelled && !IS_ANDROID) connect(); });
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

  const openAgain = async () => {
    setBusy(true);
    // Cleared before asking: an app that fails at once reports before the
    // answer below arrives.
    setAppClosed(null);
    setAppFailed(null);
    try {
      // Not always the app opened before (one may have been installed since).
      // With the options as they are now: they may have changed since.
      const options = readRdpOptions(session.serverUuid);
      setUsedOptions(describeRdpOptions(options));
      const again = await invoke<{ app: string; tracked: boolean }>("rdp_open_again", { sessionId: session.id, options });
      setOpened((o) => (o ? { ...o, app: again.app, tracked: again.tracked } : o));
      pushLog({ msg: `Opened ${again.app} again.`, type: "success" });
    } catch (e) {
      pushLog({ msg: String(e), type: "error" });
    } finally {
      setBusy(false);
    }
  };

  const name = displayName || session.serverName;

  if (IS_ANDROID) {
    return (
      <div className="relative flex-1 flex flex-col p-4 sm:p-8 bg-[#0a0a0c] text-white overflow-hidden">
        <div className="max-w-2xl w-full mx-auto">
          <div className="flex items-center gap-3 mb-4">
            <div className="shrink-0 w-9 h-9 rounded-xl flex items-center justify-center bg-primary/10 text-primary" aria-hidden="true">
              <MonitorSmartphone size={16} />
            </div>
            <div className="min-w-0 flex-1">
              <h2 className="text-sm font-semibold text-zinc-100 truncate" title={session.serverName}>{name}</h2>
              <p className="text-[11px] mt-0.5 text-zinc-500">RDP server</p>
            </div>
            <button
              onClick={onClose}
              title="Close session"
              aria-label="Close session"
              className="h-8 px-2.5 flex items-center gap-1.5 rounded-lg bg-white/5 text-zinc-300 hover:bg-red-500/15 hover:text-red-300 text-xs font-semibold transition-colors"
            >
              <X size={13} />
            </button>
          </div>
          <p className="text-[13px] leading-relaxed text-zinc-300">
            RDP servers open on a computer for now, in its RDP app. Submarine's own RDP client, which works on this phone too, is on its way.
          </p>
        </div>
      </div>
    );
  }

  const working = status === "connecting";
  const broken = status === "failed" || status === "disconnected";

  return (
    <div className="relative flex-1 flex flex-col p-4 sm:p-8 bg-[#0a0a0c] text-white overflow-hidden">
      <div className="max-w-2xl w-full mx-auto flex-1 flex flex-col min-h-0">
        <div className="flex items-center gap-3 mb-3 sm:mb-4">
          <div
            className={`shrink-0 w-9 h-9 rounded-xl flex items-center justify-center ${broken ? "bg-red-500/10 text-red-400" : "bg-primary/10 text-primary"}`}
            aria-hidden="true"
          >
            {working ? <Loader2 size={16} className="animate-spin" /> : broken ? <AlertTriangle size={16} /> : <MonitorSmartphone size={16} />}
          </div>
          <div className="min-w-0 flex-1">
            <h2 className="text-sm font-semibold text-zinc-100 truncate" title={session.serverName}>{name}</h2>
            <p className={`text-[11px] mt-0.5 ${broken ? "text-red-400/90" : "text-zinc-500"}`}>
              {working ? "Connecting…" : status === "failed" ? "Connection failed" : status === "disconnected" ? "Disconnected" : `RDP · opened in ${opened?.app ?? "the RDP app"}`}
            </p>
          </div>
          <div className="shrink-0 flex items-center gap-2">
            {status === "connected" && (
              <button
                onClick={openAgain}
                disabled={busy}
                title="Open the RDP app again"
                className="h-8 px-2.5 sm:px-3 flex items-center gap-1.5 rounded-lg bg-primary/10 text-primary hover:bg-primary/20 text-xs font-semibold transition-colors disabled:opacity-50"
              >
                <ExternalLink size={13} /><span className="hidden sm:inline">Open again</span>
              </button>
            )}
            {broken && (
              <button
                onClick={connect}
                title="Reconnect"
                aria-label="Reconnect"
                className="h-8 px-2.5 sm:px-3 flex items-center gap-1.5 rounded-lg bg-primary/10 text-primary hover:bg-primary/20 text-xs font-semibold transition-colors"
              >
                <RotateCw size={13} /><span className="hidden sm:inline">Reconnect</span>
              </button>
            )}
            <button
              onClick={onClose}
              title="Close session"
              aria-label="Close session"
              className="h-8 px-2.5 sm:px-3 flex items-center gap-1.5 rounded-lg bg-white/5 text-zinc-300 hover:bg-red-500/15 hover:text-red-300 text-xs font-semibold transition-colors"
            >
              <X size={13} /><span className="hidden sm:inline">Close</span>
            </button>
          </div>
        </div>

        {status === "connected" && opened && (
          <div className="mb-3 sm:mb-4 p-4 rounded-2xl border border-white/5 bg-[#121214] text-[12.5px] leading-relaxed text-zinc-300 space-y-1.5">
            <p>
              {opened.app} connects to <span className="font-mono text-zinc-100">{opened.address}</span>
              {opened.via ? <> — a port of this computer that goes through the SSH server <span className="text-zinc-100">{opened.via}</span>.</> : "."}
            </p>
            {usedOptions && <p className="text-zinc-400">{usedOptions}</p>}
            <p className="text-zinc-500">
              It asks for the RDP password itself.{opened.via ? " The connection stays open while this tab does; closing the tab ends it." : ""}
            </p>
            {appFailed && (
              <div className="pt-1 text-red-300/90">
                <p>{opened.app} couldn't open it.</p>
                {appFailed.map((l, i) => <p key={i} className="font-mono text-[11px] text-zinc-500 break-all">{l}</p>)}
              </div>
            )}
            {appClosed && (
              <div className="pt-1 text-amber-300/90">
                <p>The RDP window was closed. Open it again with “Open again”.</p>
                {appClosed.map((l, i) => <p key={i} className="font-mono text-[11px] text-zinc-500 break-all">{l}</p>)}
              </div>
            )}
          </div>
        )}
        {broken && reason && (
          <p className="mb-3 sm:mb-4 text-[12.5px] text-red-300/90 break-words">{reason}</p>
        )}

        <div
          className="flex-1 min-h-0 bg-[#121214] border border-white/5 rounded-2xl p-4 font-mono text-[12px] overflow-y-auto overscroll-contain custom-scrollbar select-text cursor-text"
          style={{ WebkitOverflowScrolling: "touch", lineHeight: 1.5 }}
        >
          {logs.map((l, i) => (
            <div key={i} className={`mb-2 ${l.type === "error" ? "text-red-400" : l.type === "success" ? "text-primary" : "text-zinc-400"}`}>
              <span className="text-zinc-600 opacity-50 mr-3">[{l.time}]</span>
              {l.msg}
            </div>
          ))}
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
    </div>
  );
}
