import { useCallback, useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { AlertTriangle, ExternalLink, FlaskConical, Loader2, MonitorSmartphone, RotateCw, X } from "lucide-react";
import { AuthPrompts } from "./AuthPrompts";
import RemoteScreen, { type RemoteScreenHandle } from "./RemoteScreen";
import { readUseSshAgent } from "../util/sshAgent";
import { IS_ANDROID } from "../util/platform";
import { RDP_BUILTIN_EXPERIMENTAL, RDP_NO_NLA_PREFIX, describeRdpOptions, readRdpOptions, writeRdpOptions } from "../util/rdpOptions";

// An RDP server's tab (issue #3). By default Submarine opens the system's RDP
// app (through a forward on a loopback port while the node has an SSH
// server); then the tab shows how that went and opens the app again on
// request, and the app asks for the password itself. As this device's
// option, the desktop shows in the tab itself: the experimental built-in
// client (rdp_client.rs), straight or through the node's SSH server, with the
// saved password or one asked for here. A phone has only that one, offered
// in the tab. Either way the tab asks the host-key, certificate and login
// questions.

type Opened = { app: string; address: string; via: string | null; tracked: boolean };
type LogLine = { msg: string; type: string; time: string };

export default function RdpSessionView({ session, displayName, onClose, onStatusChange, isActiveView = false, onPromptChange }: any) {
  // Read once: a tab keeps the way it opened with until it's closed (a phone
  // can switch to the built-in client from the tab).
  const [builtin, setBuiltin] = useState(() => readRdpOptions(session.serverUuid).app === "builtin");
  // "idle" on a phone until the built-in client is chosen: there's no RDP app.
  const [status, setStatus] = useState<"idle" | "connecting" | "connected" | "failed" | "disconnected">(
    IS_ANDROID && !builtin ? "idle" : "connecting",
  );
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

  const canvasRef = useRef<RemoteScreenHandle | null>(null);
  // Hidden while the desktop shows; open again on request.
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
    setOpened(null);
    setAppClosed(null);
    setAppFailed(null);
    setReason("");
    const failed = (e: unknown) => {
      const why = String(e);
      pushLog({ msg: why, type: "error" });
      setReason(why);
      setStatus("failed");
    };
    if (builtin) {
      const canvas = canvasRef.current;
      if (!canvas) return failed("The tab isn't ready yet. Try again.");
      pushLog({ msg: RDP_BUILTIN_EXPERIMENTAL, type: "warning" });
      const { width, height } = canvas.wantedSize();
      invoke("rdp_builtin_open", {
        sessionId: session.id,
        serverId: session.serverId,
        useSshAgent: readUseSshAgent(),
        width,
        height,
        allowNoNla: readRdpOptions(session.serverUuid).allowNoNla,
        frames: canvas.newChannel(),
      }).catch(failed);
      return;
    }
    const options = readRdpOptions(session.serverUuid);
    setUsedOptions(describeRdpOptions(options));
    invoke("initiate_rdp", { sessionId: session.id, serverId: session.serverId, useSshAgent: readUseSshAgent(), options }).catch(failed);
  }, [builtin, session.id, session.serverId, session.serverUuid, pushLog]);

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
    // A phone connects once the built-in client is chosen in the tab.
    subs.then(() => { if (!cancelled && (builtin || !IS_ANDROID)) connect(); });
    // The tab menu's Reconnect.
    const onReconnect = () => connect();
    window.addEventListener(`session-reconnect-${id}`, onReconnect);
    return () => {
      cancelled = true;
      window.removeEventListener(`session-reconnect-${id}`, onReconnect);
      subs.then((fns) => fns.forEach((f) => f()));
      if (builtin) invoke("rdp_builtin_close", { sessionId: id }).catch(console.error);
      invoke("disconnect_session", { sessionId: id }).catch(console.error);
    };
  }, [builtin, session.id, connect, pushLog]);

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
  const working = status === "connecting";
  const broken = status === "failed" || status === "disconnected";
  // A server without NLA was refused before anything was sent: the tab offers
  // to allow it for this server on this device, and connects again.
  const noNla = status === "failed" && reason.startsWith(RDP_NO_NLA_PREFIX) && !!session.serverUuid;
  const allowWithoutNla = () => {
    if (!session.serverUuid) return;
    writeRdpOptions(session.serverUuid, { ...readRdpOptions(session.serverUuid), allowNoNla: true });
    pushLog({ msg: "Servers without NLA are allowed for this server now.", type: "warning" });
    connect();
  };

  const prompts = promptWaiting && (
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
  );

  const logPanel = (
    <div
      className="flex-1 min-h-0 bg-[#121214] border border-white/5 rounded-2xl p-4 font-mono text-[12px] overflow-y-auto overscroll-contain custom-scrollbar select-text cursor-text"
      style={{ WebkitOverflowScrolling: "touch", lineHeight: 1.5 }}
    >
      {logs.map((l, i) => (
        <div key={i} className={`mb-2 ${l.type === "error" ? "text-red-400" : l.type === "success" ? "text-primary" : l.type === "warning" ? "text-amber-300" : "text-zinc-400"}`}>
          <span className="text-zinc-600 opacity-50 mr-3">[{l.time}]</span>
          {l.msg}
        </div>
      ))}
    </div>
  );

  // The experimental client's mark, on its tab whatever its state.
  const experimentalBadge = (
    <span
      role="note"
      title={RDP_BUILTIN_EXPERIMENTAL}
      aria-label={RDP_BUILTIN_EXPERIMENTAL}
      className="shrink-0 inline-flex items-center gap-1 h-5 px-1.5 rounded border border-amber-500/30 bg-amber-500/10 text-amber-300 text-[10px] font-bold uppercase tracking-wider"
    >
      <FlaskConical size={10} aria-hidden="true" />
      <span className="hidden sm:inline">Experimental</span>
    </span>
  );

  // A phone: no RDP app to hand it to, so the built-in client is offered here,
  // and saved as this device's choice for the server once taken.
  if (IS_ANDROID && !builtin) {
    const useBuiltin = () => {
      if (session.serverUuid) writeRdpOptions(session.serverUuid, { ...readRdpOptions(session.serverUuid), app: "builtin" });
      setStatus("connecting");
      setBuiltin(true);
    };
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
          <div role="note" className="flex gap-2.5 p-3 rounded-xl border border-amber-500/25 bg-amber-500/5 text-[12.5px] leading-relaxed text-amber-200/90">
            <FlaskConical size={15} className="shrink-0 mt-0.5 text-amber-300" aria-hidden="true" />
            <span>
              On a phone, RDP servers open with Submarine's built-in RDP client. {RDP_BUILTIN_EXPERIMENTAL} Your choice is kept for this server on this phone.
            </span>
          </div>
          <button
            onClick={useBuiltin}
            className="mt-4 h-10 px-4 flex items-center gap-2 rounded-xl bg-primary/15 text-primary hover:bg-primary/25 text-[13px] font-semibold transition-colors"
          >
            <FlaskConical size={14} aria-hidden="true" /> Open with the built-in client
          </button>
        </div>
      </div>
    );
  }

  if (builtin) {
    const connected = status === "connected";
    return (
      <div data-rdp-status={status} className="relative flex-1 flex flex-col bg-[#0a0a0c] text-white overflow-hidden">
        <div className="shrink-0 h-9 px-2 sm:px-3 flex items-center gap-2 border-b border-white/5 bg-[#0e0e10]">
          <span className={`shrink-0 ${broken ? "text-red-400" : "text-primary"}`} aria-hidden="true">
            {working ? <Loader2 size={14} className="animate-spin" /> : broken ? <AlertTriangle size={14} /> : <MonitorSmartphone size={14} />}
          </span>
          <span className="min-w-0 truncate text-[12px] font-semibold text-zinc-200" title={session.serverName}>{name}</span>
          <span className={`min-w-0 truncate text-[11px] ${broken ? "text-red-400/90" : "text-zinc-500"}`}>
            {working ? "Connecting…" : status === "failed" ? "Connection failed" : status === "disconnected" ? "Disconnected" : "RDP"}
          </span>
          {experimentalBadge}
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
            {noNla && (
              <button
                onClick={allowWithoutNla}
                title="Log in to this server without NLA: the password then goes inside TLS only"
                className="h-7 px-2 flex items-center gap-1.5 rounded-md bg-amber-500/15 text-amber-200 hover:bg-amber-500/25 text-[11px] font-semibold transition-colors"
              >
                <AlertTriangle size={12} aria-hidden="true" />Allow without NLA
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
            ref={canvasRef}
            sessionId={session.id}
            active={isActiveView && !promptWaiting}
            connected={connected}
            inputCommand="rdp_builtin_input"
            resizeCommand="rdp_builtin_resize"
          />
          {(!connected || showLog) && (
            <div className="absolute inset-0 z-10 flex flex-col p-4 sm:p-8 bg-[#0a0a0c]/90">
              <div className="max-w-2xl w-full mx-auto flex-1 flex flex-col min-h-0">
                {broken && reason && <p className="mb-3 sm:mb-4 text-[12.5px] text-red-300/90 break-words">{reason}</p>}
                {logPanel}
              </div>
            </div>
          )}
        </div>

        {prompts}
      </div>
    );
  }

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

        {logPanel}

        {prompts}
      </div>
    </div>
  );
}
