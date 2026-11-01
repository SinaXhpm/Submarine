import { useCallback, useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { AlertTriangle, ClipboardPaste, ExternalLink, FlaskConical, FolderSymlink, Loader2, MonitorSmartphone, RotateCw, SlidersHorizontal, X } from "lucide-react";
import { AuthPrompts } from "./AuthPrompts";
import RemoteScreen, { type RemoteScreenHandle } from "./RemoteScreen";
import { FilesPanel, LinkMark, LinkPanel, RdpSettingsPanel, ToolPanel, usePauseWhileHidden, type SharedNow } from "./ScreenTabTools";
import { usePageShown } from "../hooks/usePageShown";
import { readUseSshAgent } from "../util/sshAgent";
import { IS_MOBILE, IS_WINDOWS } from "../util/platform";
import type { LinkStats } from "../util/screenLink";
import {
  RDP_BUILTIN_EXPERIMENTAL, RDP_NO_NLA_PREFIX, RDP_PRESETS, describeRdpOptions, pinRdpAddress, rdpGraphics, rdpNeedsReconnect, rdpShare, readRdpOptions, writeRdpOptions,
  type RdpOptions,
} from "../util/rdpOptions";
import { type PasteKeys, pasteKeysOf, readLocalClipboard, takeServerCopy } from "../util/rdpClipboard";
import { scancodeFor } from "../util/scancodes";

// An RDP server's tab (issue #3). By default Submarine opens the system's RDP
// app (through a forward on the node's own loopback address while the node
// has an SSH server); then the tab shows how that went and opens the app
// again on request. Remote Desktop Connection on Windows gets the saved
// password; the other apps ask for it themselves. As this device's
// option, the desktop shows in the tab itself: the experimental built-in
// client (rdp_client.rs), straight or through the node's SSH server, with the
// saved password or one asked for here. A phone has only that one, offered
// in the tab. Either way the tab asks the host-key, certificate and login
// questions.

// `password`: the app got the saved password (Remote Desktop Connection).
type Opened = { app: string; address: string; via: string | null; tracked: boolean; password?: boolean };

// How the built-in client says the server refused the user name or password
// (rdp_client.rs, login_error).
const PASSWORD_REFUSED = /didn't accept the user name or password/;
type LogLine = { msg: string; type: string; time: string };

// `shown`: the tab is on screen (alone, or in a split), not behind another view.
export default function RdpSessionView({ session, displayName, onClose, onStatusChange, isActiveView = false, shown = true, onPromptChange }: any) {
  // Read once: a tab keeps the way it opened with until it's closed (a phone
  // can switch to the built-in client from the tab).
  const [builtin, setBuiltin] = useState(() => readRdpOptions(session.serverUuid).app === "builtin");
  // "idle" on a phone until the built-in client is chosen: there's no RDP app.
  const [status, setStatus] = useState<"idle" | "connecting" | "connected" | "failed" | "disconnected">(
    IS_MOBILE && !builtin ? "idle" : "connecting",
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
  // The built-in client shares the clipboard with the server (this server's
  // option, read when the tab connects).
  const [shareClipboard, setShareClipboard] = useState(false);
  // Text the user copied on the server that didn't go on this computer's
  // clipboard by itself (the tab wasn't in front, or a webview that wants a
  // click for that): one click does.
  const [serverCopy, setServerCopy] = useState<string | null>(null);
  const inFront = useRef(false);
  // The server isn't at the address this device's sharing was chosen for: it
  // is off for this connection (`rdp::moved`).
  const [moved, setMoved] = useState<{ was: string; now: string } | null>(null);
  // The built-in client's saved password was refused: the next connect asks for
  // it (with a "Save password" tick) instead of trying it again.
  const askPassword = useRef(false);
  const passwordRetries = useRef(0);
  // Hidden while the desktop shows; open again on request.
  const [showLog, setShowLog] = useState(false);
  // The built-in client's tools: this server's options on this device as its
  // panels change them, the ones the session now open was opened with, what it
  // shares, and how its link is doing.
  const [opts, setOpts] = useState<RdpOptions>(() => readRdpOptions(session.serverUuid));
  const [usedOpts, setUsedOpts] = useState<RdpOptions | null>(null);
  const [shared, setShared] = useState<SharedNow | null>(null);
  const [link, setLink] = useState<LinkStats | null>(null);
  const [panel, setPanel] = useState<"link" | "files" | "settings" | null>(null);
  const [drives, setDrives] = useState<string[]>([]);

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
    setMoved(null);
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
      const options = readRdpOptions(session.serverUuid);
      // The panels show what this session has, and the resolution is sized for at once.
      setOpts(options);
      setUsedOpts(options);
      setShared(null);
      setLink(null);
      const { width, height } = canvas.wantedSize(options.resolution);
      invoke("rdp_builtin_open", {
        sessionId: session.id,
        serverId: session.serverId,
        useSshAgent: readUseSshAgent(),
        width,
        height,
        allowNoNla: options.allowNoNla,
        graphics: rdpGraphics(options),
        clipboard: options.clipboard,
        // A phone or tablet shares no files: its backend has no drive to give.
        share: IS_MOBILE ? null : rdpShare(options),
        sharedFor: options.sharedFor,
        askPassword: askPassword.current,
        frames: canvas.newChannel(),
      }).catch(failed);
      setShareClipboard(options.clipboard);
      setServerCopy(null);
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
        askPassword.current = false;
        passwordRetries.current = 0;
        setOpened(e.payload as Opened);
        setShared((e.payload?.shared as SharedNow | null) ?? null);
        setStatus("connected");
        pinRdpAddress(session.serverUuid, e.payload?.target);
      }),
      listen(`rdp-moved-${id}`, (e: any) => {
        setMoved({ was: String(e.payload?.was ?? ""), now: String(e.payload?.now ?? "") });
        setShareClipboard(false);
        setUsedOptions(describeRdpOptions({ ...readRdpOptions(session.serverUuid), clipboard: false, drives: false }));
      }),
      // How the built-in client's link is doing, every 2 seconds.
      listen(`rdp-link-${id}`, (e: any) => setLink(e.payload as LinkStats)),
      listen(`connection-failed-${id}`, (e: any) => {
        const why = String(e.payload?.reason ?? "The connection failed.");
        askPassword.current = false;
        setReason(why);
        setStatus("failed");
        // The server refused the password: ask for it at once, twice at most.
        if (builtin && PASSWORD_REFUSED.test(why) && passwordRetries.current < 2) {
          passwordRetries.current += 1;
          askPassword.current = true;
          setTimeout(() => connect(), 300);
        }
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
      // Text the user copied on the server, for this computer's clipboard.
      listen(`rdp-clipboard-${id}`, (e: any) => {
        takeServerCopy(String(e.payload?.text ?? ""), inFront.current, setServerCopy);
      }),
    ]);
    // A phone connects once the built-in client is chosen in the tab.
    subs.then(() => { if (!cancelled && (builtin || !IS_MOBILE)) connect(); });
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
      const again = await invoke<{ app: string; tracked: boolean; password: boolean }>("rdp_open_again", { sessionId: session.id, options });
      setOpened((o) => (o ? { ...o, app: again.app, tracked: again.tracked, password: again.password } : o));
      pushLog({ msg: `Opened ${again.app} again.`, type: "success" });
    } catch (e) {
      pushLog({ msg: String(e), type: "error" });
    } finally {
      setBusy(false);
    }
  };

  // This computer's clipboard to the server's, then the paste key (the Paste
  // button sends Ctrl+V) once the text is there. When that can't be done, a
  // held-back key press still goes on its own (its modifier is down on the
  // server already) and the server pastes what it has; the button sends nothing.
  const pasteToServer = async (keys: PasteKeys, fromKey: boolean) => {
    const keyAlone = () => {
      const code = keys === "shiftinsert" ? scancodeFor("Insert") : keys === "ctrlv" ? scancodeFor("KeyV") : null;
      if (!fromKey || code === null) return;
      const key = keys === "shiftinsert" ? "Insert" : "v";
      invoke("rdp_builtin_input", { sessionId: session.id, events: [{ t: "key", code, key, down: true }, { t: "key", code, key, down: false }] }).catch(() => {});
    };
    let text: string;
    try {
      text = await readLocalClipboard();
    } catch (e) {
      pushLog({ msg: `This computer's clipboard couldn't be read: ${e}`, type: "warning" });
      keyAlone();
      return;
    }
    try {
      await invoke("rdp_builtin_paste", { sessionId: session.id, text, keys });
    } catch (e) {
      pushLog({ msg: String(e), type: "warning" });
      keyAlone();
    }
  };
  // A paste key in the tab: held back from the screen until the text is on the
  // server (capture phase, ahead of the screen's own key handler). Its repeats
  // while held go nowhere: the screen would type them.
  const onPasteKey = (e: React.KeyboardEvent) => {
    if (!shareClipboard || status !== "connected" || e.nativeEvent.isComposing) return;
    const keys = pasteKeysOf(e);
    if (!keys) return;
    e.preventDefault();
    e.stopPropagation();
    if (e.repeat) return;
    // Cmd+V on a Mac: the server has the Windows key down. A Ctrl tap now
    // keeps it from opening the Start menu when the user lets go of Cmd
    // before the paste is done.
    if (keys === "cmdv") {
      const code = scancodeFor("ControlLeft");
      if (code !== null) {
        invoke("rdp_builtin_input", { sessionId: session.id, events: [{ t: "key", code, key: "Control", down: true }, { t: "key", code, key: "Control", down: false }] }).catch(() => {});
      }
    }
    void pasteToServer(keys, true);
  };

  // A choice made in a panel: kept for the server on this device at once,
  // over what the server form may have changed meanwhile.
  const setOpt = (patch: Partial<RdpOptions>) => {
    const next = { ...(session.serverUuid ? readRdpOptions(session.serverUuid) : opts), ...patch };
    setOpts(next);
    if (session.serverUuid) writeRdpOptions(session.serverUuid, next);
  };
  const togglePanel = (which: "link" | "files" | "settings") => {
    if (panel === which) return setPanel(null);
    if (session.serverUuid) setOpts(readRdpOptions(session.serverUuid));
    if (which === "files" && IS_WINDOWS) invoke<string[]>("rdp_share_drives").then(setDrives, () => setDrives([]));
    setPanel(which);
  };
  const lessData = () => {
    const light = RDP_PRESETS.find((p) => p.value === "light");
    if (light) setOpt(light.set);
    setPanel("settings");
  };

  // The picture pauses while the tab is hidden (behind another tab or view,
  // or the window minimised), if the server can and the user lets it: a few
  // seconds on, and it comes back as soon as the tab does.
  const pageShown = usePageShown();
  inFront.current = isActiveView && shown && pageShown;
  usePauseWhileHidden(builtin && status === "connected" && !(shown && pageShown) && opts.pauseHidden, "rdp_builtin_shown", session.id);
  // No link to show without a connection.
  useEffect(() => {
    if (status !== "connected") setPanel((p) => (p === "link" ? null : p));
  }, [status]);

  const name = displayName || session.serverName;
  const working = status === "connecting";
  const broken = status === "failed" || status === "disconnected";
  // A server without NLA was refused before anything was sent: the tab offers
  // to allow it for this server on this device, and connects again.
  const noNla = status === "failed" && reason.startsWith(RDP_NO_NLA_PREFIX) && !!session.serverUuid && !moved;
  const allowWithoutNla = () => {
    if (!session.serverUuid) return;
    writeRdpOptions(session.serverUuid, { ...readRdpOptions(session.serverUuid), allowNoNla: true });
    pushLog({ msg: "Servers without NLA are allowed for this server now.", type: "warning" });
    connect();
  };

  // The user allows what was shared for the server's new address: it goes to
  // the server from a new connection on (for the RDP app, its next window).
  const allowNewAddress = () => {
    if (!session.serverUuid || !moved) return;
    writeRdpOptions(session.serverUuid, { ...readRdpOptions(session.serverUuid), sharedFor: moved.now });
    pushLog({ msg: `What this device shares with the server is allowed for ${moved.now} now.`, type: "warning" });
    setMoved(null);
    if (!builtin && status === "connected") void openAgain();
    else connect();
  };
  const movedNote = (place: string) => moved && (
    <div role="status" className={`shrink-0 px-3 py-1.5 flex items-center gap-2 bg-amber-500/10 text-[11.5px] text-amber-200 ${place}`}>
      <AlertTriangle size={12} className="shrink-0" aria-hidden="true" />
      <span className="min-w-0 break-words">This server moved from {moved.was} to {moved.now}, so the files, clipboard and login without NLA this device allowed are off.</span>
      <button
        onClick={allowNewAddress}
        className="ml-auto shrink-0 h-6 px-2 rounded-md bg-amber-500/20 text-amber-100 hover:bg-amber-500/30 font-semibold"
      >
        Allow here
      </button>
    </div>
  );

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
      <span className="hidden [@container(min-width:560px)]:inline">Experimental</span>
    </span>
  );

  // A phone: no RDP app to hand it to, so the built-in client is offered here,
  // and saved as this device's choice for the server once taken.
  if (IS_MOBILE && !builtin) {
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
    // A change the session now open hasn't got: the panels offer to connect again.
    const reconnectForOpts = connected && usedOpts && rdpNeedsReconnect(usedOpts, opts)
      ? () => { setPanel(null); connect(); }
      : null;
    return (
      <div data-rdp-status={status} className="relative flex-1 flex flex-col bg-[#0a0a0c] text-white overflow-hidden">
        {/* The labels show by the bar's own width, not the window's: in a split
            view the tab is a part of the window. */}
        <div className="shrink-0 h-9 px-2 sm:px-3 flex items-center gap-2 border-b border-white/5 bg-[#0e0e10] [container-type:inline-size]">
          <span className={`shrink-0 ${broken ? "text-red-400" : "text-primary"}`} aria-hidden="true">
            {working ? <Loader2 size={14} className="animate-spin-brief" /> : broken ? <AlertTriangle size={14} /> : <MonitorSmartphone size={14} />}
          </span>
          <span className="min-w-0 truncate text-[12px] font-semibold text-zinc-200" title={session.serverName}>{name}</span>
          <span className={`min-w-0 truncate text-[11px] ${broken ? "text-red-400/90" : "text-zinc-500"}`}>
            {working ? "Connecting…" : status === "failed" ? "Connection failed" : status === "disconnected" ? "Disconnected" : "RDP"}
          </span>
          {experimentalBadge}
          <div className="ml-auto shrink-0 flex items-center gap-1.5">
            {connected && shareClipboard && (
              <button
                onClick={() => void pasteToServer("ctrlv", false)}
                title="Paste this device's clipboard into the server"
                aria-label="Paste this device's clipboard into the server"
                className="h-7 px-2 flex items-center gap-1.5 rounded-md bg-white/5 text-zinc-300 hover:text-zinc-100 text-[11px] font-semibold transition-colors"
              >
                <ClipboardPaste size={12} aria-hidden="true" /><span className="hidden [@container(min-width:560px)]:inline">Paste</span>
              </button>
            )}
            {connected && <LinkMark link={link} open={panel === "link"} onClick={() => togglePanel("link")} />}
            {/* A phone or tablet shares no files: its backend has no drive to give. */}
            {!IS_MOBILE && (
              <button
                onClick={() => togglePanel("files")}
                aria-expanded={panel === "files"}
                title="Shared files"
                aria-label="Shared files"
                className={`h-7 px-2 flex items-center gap-1.5 rounded-md text-[11px] font-semibold transition-colors ${panel === "files" ? "bg-primary/15 text-primary" : "bg-white/5 text-zinc-400 hover:text-zinc-200"}`}
              >
                <FolderSymlink size={12} aria-hidden="true" /><span className="hidden [@container(min-width:700px)]:inline">Files</span>
              </button>
            )}
            <button
              onClick={() => togglePanel("settings")}
              aria-expanded={panel === "settings"}
              title="Speed and quality"
              aria-label="Speed and quality"
              className={`h-7 px-2 flex items-center gap-1.5 rounded-md text-[11px] font-semibold transition-colors ${panel === "settings" ? "bg-primary/15 text-primary" : "bg-white/5 text-zinc-400 hover:text-zinc-200"}`}
            >
              <SlidersHorizontal size={12} aria-hidden="true" /><span className="hidden [@container(min-width:700px)]:inline">Settings</span>
            </button>
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
                <RotateCw size={12} /><span className="hidden [@container(min-width:560px)]:inline">Reconnect</span>
              </button>
            )}
            <button
              onClick={onClose}
              title="Close session"
              aria-label="Close session"
              className="h-7 px-2 flex items-center gap-1.5 rounded-md bg-white/5 text-zinc-300 hover:bg-red-500/15 hover:text-red-300 text-[11px] font-semibold transition-colors"
            >
              <X size={12} /><span className="hidden [@container(min-width:560px)]:inline">Close</span>
            </button>
          </div>
        </div>

        {movedNote("border-b border-white/5")}
        {connected && serverCopy !== null && (
          <div role="status" className="shrink-0 px-3 py-1.5 flex items-center gap-2 border-b border-white/5 bg-primary/10 text-[11.5px] text-zinc-200">
            <span className="min-w-0 truncate">Text was copied on the server ({serverCopy.length} characters).</span>
            <button
              onClick={() => navigator.clipboard.writeText(serverCopy).then(() => setServerCopy(null), () => {})}
              className="shrink-0 h-6 px-2 rounded-md bg-primary/20 text-primary hover:bg-primary/30 font-semibold"
            >
              Copy it here
            </button>
            <button onClick={() => setServerCopy(null)} aria-label="Dismiss" className="ml-auto shrink-0 text-zinc-500 hover:text-zinc-300">
              <X size={12} />
            </button>
          </div>
        )}
        <div className="relative flex-1 min-h-0 flex flex-col" onKeyDownCapture={onPasteKey}>
          <RemoteScreen
            ref={canvasRef}
            sessionId={session.id}
            active={isActiveView && !promptWaiting && !panel}
            connected={connected}
            inputCommand="rdp_builtin_input"
            resizeCommand="rdp_builtin_resize"
            scale={opts.resolution}
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

        {/* Outside the screen: a paste key in a panel isn't the server's. */}
        {panel === "link" && (
          <ToolPanel title="Connection" onClose={() => setPanel(null)}>
            <LinkPanel link={link} onLessData={lessData} />
          </ToolPanel>
        )}
        {panel === "files" && (
          <ToolPanel title="Shared files" onClose={() => setPanel(null)}>
            <FilesPanel shared={connected ? shared : undefined} o={opts} set={setOpt} drives={drives} reconnect={reconnectForOpts} />
          </ToolPanel>
        )}
        {panel === "settings" && (
          <ToolPanel title="Speed and quality" onClose={() => setPanel(null)}>
            <RdpSettingsPanel o={opts} set={setOpt} reconnect={reconnectForOpts} />
          </ToolPanel>
        )}

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
            {working ? <Loader2 size={16} className="animate-spin-brief" /> : broken ? <AlertTriangle size={16} /> : <MonitorSmartphone size={16} />}
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

        {movedNote("mb-3 sm:mb-4 rounded-xl")}
        {status === "connected" && opened && (
          <div className="mb-3 sm:mb-4 p-4 rounded-2xl border border-white/5 bg-[#121214] text-[12.5px] leading-relaxed text-zinc-300 space-y-1.5">
            <p>
              {opened.app} connects to <span className="font-mono text-zinc-100">{opened.address}</span>
              {opened.via ? <> — a port of this computer that goes through the SSH server <span className="text-zinc-100">{opened.via}</span>.</> : "."}
            </p>
            {usedOptions && <p className="text-zinc-400">{usedOptions}</p>}
            <p className="text-zinc-500">
              {opened.password ? "It got the saved password." : "It asks for the RDP password itself."}
              {opened.via ? " The connection stays open while this tab does; closing the tab ends it." : ""}
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
