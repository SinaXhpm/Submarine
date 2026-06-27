import { useState, useEffect, useRef, memo } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow, LogicalSize } from "@tauri-apps/api/window";
import { TerminalSquare, Folder, Network, AlertTriangle, Check, X, ShieldAlert, Play, Library, Info, Container } from "lucide-react";
import TerminalView from "./TerminalView";
import SftpWorkspace from "./SftpWorkspace";
import TunnelsPanel from "./TunnelsPanel";
import InfoPanel from "./InfoPanel";
import { CmdsPanel } from "./CmdsPanel";
import { useIsCompact } from "../hooks/useViewport";

const SessionViewImpl = ({ session, onClose, addLog, onStatusChange }: any) => {
  const [status, setStatus] = useState<'connecting' | 'connected' | 'failed' | 'disconnected'>('connecting');

  // Bubble every status change up to the parent so the session-tab strip
  // can draw a coloured dot (green / amber / red) next to the name without
  // having to mount its own listeners or duplicate the connection-event
  // wiring we already do here.
  useEffect(() => {
    if (typeof onStatusChange === "function") {
      onStatusChange(session?.id, status);
    }
  }, [status, session?.id, onStatusChange]);
  const [disconnectReason, setDisconnectReason] = useState<string>("");
  const [logs, setLogs] = useState<{ msg: string, type: string }[]>([]);
  // Cap to keep a chatty session (monitors, mirrors, MOTD spam) from
  // ballooning React state — each render copies the whole array, so an
  // unbounded log makes the UI quadratically slower over time.
  const LOG_CAP = 500;
  const pushLog = (entry: { msg: string, type: string }) =>
    setLogs(prev => prev.length >= LOG_CAP
      ? [...prev.slice(prev.length - LOG_CAP + 1), entry]
      : [...prev, entry]);
  const [fingerprintPrompt, setFingerprintPrompt] = useState<any>(null);
  const [isAuthError, setIsAuthError] = useState(false);
  const [customPassword, setCustomPassword] = useState("");
  // The connection effect runs once and captures `customPassword=""` in its
  // closure. Auto-reconnect attempts and the disconnect listener that
  // schedules them must read the LATEST password the user typed into the
  // auth-error retry input — so we mirror it into a ref.
  const customPasswordRef = useRef("");
  useEffect(() => { customPasswordRef.current = customPassword; }, [customPassword]);

  // Terminal IDs MUST be unique across every open session, not just within
  // this one — the backend dispatches `terminal-output-<id>` events globally
  // and any collision means two tabs end up reading from the same PTY.
  // Scoping the id by `session.id` ensures Server A's "term-0" never clashes
  // with Server B's "term-0".
  // A terminal slot is either a regular login shell (the default) or a
  // docker-exec session into a specific container (spawned from the Info
  // panel). Container slots stay otherwise identical — same xterm, same
  // events — they just point the backend at `open_container_terminal`.
  const [terminals, setTerminals] = useState<{id: string, title: string, container?: { name: string; useSudo: boolean }}[]>(() => {
    return [{ id: `${session.id}-term-0`, title: '1' }];
  });
  // Bumped on every successful reconnect. TerminalView watches this prop
  // and re-opens its PTY on change WITHOUT disposing its xterm instance,
  // so the user keeps the scroll-back from before the drop. Without this
  // we used to mint a brand-new terminal_id and remount TerminalView,
  // which dropped the entire visual buffer of their previous session.
  const [connectionEpoch, setConnectionEpoch] = useState(0);
  const openContainerTerminal = (containerName: string, useSudo: boolean) => {
    const newId = `${session.id}-term-${Date.now()}`;
    setTerminals(prev => [...prev, {
      id: newId,
      title: containerName.length > 10 ? `${containerName.slice(0, 9)}…` : containerName,
      container: { name: containerName, useSudo },
    }]);
    setActiveTab(newId);
    setActiveTool(null);
  };

  const [activeTab, setActiveTab] = useState<string>(`${session.id}-term-0`);
  const [activeTool, setActiveTool] = useState<'sftp' | 'tunnels' | 'mirrors' | 'cmds' | 'info' | null>(null);
  // On narrow viewports the side-by-side terminal+tool layout doesn't fit.
  // We collapse to a stacked single-pane view: when a tool is open, the
  // tool takes full width and the terminal is hidden behind a back-chip.
  const isCompact = useIsCompact();
  // Width of the right-side tool pane in pixels. The divider drag updates this
  // and we sync it to localStorage so subsequent sessions remember the split.
  // First-time default is a quarter of the current window width — looks right
  // across both narrow and wide monitors without us picking a magic pixel.
  const [toolPanelWidth, setToolPanelWidth] = useState<number>(() => {
    const saved = parseInt(localStorage.getItem('submarine-tool-panel-width') || '', 10);
    if (Number.isFinite(saved) && saved >= 240) return saved;
    const quarter = Math.round((window.innerWidth || 1440) / 4);
    return Math.max(240, Math.min(900, quarter));
  });

  const initiatedRef = useRef(false);

  // ---- Auto-reconnect with exponential backoff -----------------------------
  // After a previously-good session drops, try to reconnect on a 1.5s → 3 → 6
  // → 12 → 24s → 30s cadence (capped at 30s). Retries are UNBOUNDED — the
  // user said the SOCKS use-case in particular needs the tunnel to come back
  // by itself after a long blip (laptop sleep, mobile-hotspot dropout) rather
  // than silently giving up after 5 attempts and leaving them with no SOCKS
  // until they notice. Only an auth failure or an explicit Cancel stops the
  // cycle; the terminal/SFTP overlay stays in place the whole time so the
  // user can see what's happening without losing context.
  const RECONNECT_BASE_MS = 1500;
  const RECONNECT_MAX_MS = 30000;
  const [reconnectAttempt, setReconnectAttempt] = useState(0);
  const [nextReconnectAt, setNextReconnectAt] = useState<number | null>(null);
  const reconnectAttemptRef = useRef(0);
  const reconnectTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);

  const updateAttempt = (n: number) => {
    reconnectAttemptRef.current = n;
    setReconnectAttempt(n);
  };

  const cancelReconnect = () => {
    if (reconnectTimerRef.current) {
      clearTimeout(reconnectTimerRef.current);
      reconnectTimerRef.current = null;
    }
    updateAttempt(0);
    setNextReconnectAt(null);
  };

  useEffect(() => {
    if (initiatedRef.current) return;
    initiatedRef.current = true;

    // Start connection. We read from the ref so a password the user types
    // into the auth-retry input is picked up by later attempts inside this
    // same effect closure (the effect itself only runs once).
    invoke("initiate_connection", { sessionId: session.id, serverId: session.serverId, customPassword: customPasswordRef.current || null, quickAuth: session.quickAuth || null })
      .catch(e => {
        pushLog({ msg: `Failed to initiate: ${e}`, type: 'error' });
        setStatus('failed');
      });

    // Setup listeners
    const unlistenLog = listen(`session-log-${session.id}`, (event: any) => {
      pushLog(event.payload);
    });

    const unlistenPrompt = listen(`fingerprint-prompt-${session.id}`, (event: any) => {
      setFingerprintPrompt(event.payload);
    });

    const unlistenPromptDismiss = listen(`fingerprint-prompt-dismiss-${session.id}`, () => {
      setFingerprintPrompt(null);
    });

    const unlistenSuccess = listen(`connection-success-${session.id}`, () => {
      const wasReconnect = reconnectAttemptRef.current > 0 || status === 'disconnected' || status === 'failed';
      setStatus('connected');
      cancelReconnect();
      // On a successful RECONNECT we bump connectionEpoch instead of
      // replacing the terminals array. The terminal_id stays the same
      // (so the existing event listener keeps catching output), the
      // xterm instance is preserved (so the user's scroll-back survives
      // the drop), and TerminalView's per-epoch effect re-invokes
      // open_terminal — backend's terminal_txs HashMap.insert replaces
      // the dead entry transparently so the user's next keystroke
      // routes into the fresh PTY.
      if (wasReconnect) {
        setConnectionEpoch(e => e + 1);
      }
      // The handshake may have inserted a row into `known_hosts` (user just
      // accepted a new fingerprint). Flush the encrypted vault so the entry
      // survives an app restart — otherwise the prompt would reappear every
      // session, which was especially painful for SOCKS-proxied connections.
      invoke("persist_vault").catch(console.error);
    });

    const unlistenFailed = listen(`connection-failed-${session.id}`, (event: any) => {
      pushLog({ msg: `Connection failed: ${event.payload?.reason}`, type: 'error' });
      const isAuth = !!event.payload?.is_auth_error;
      setStatus('failed');
      setIsAuthError(isAuth);
      addLog(`SSH_CONNECTION_FAILED [${session.serverName}]: ${event.payload?.reason}`, "error");
      // If this failure happened during an auto-reconnect attempt, queue the
      // next try unless we've exhausted them or the credentials are wrong
      // (retrying auth-rejected attempts just wastes time).
      if (reconnectAttemptRef.current > 0 && !isAuth) {
        scheduleReconnect(reconnectAttemptRef.current + 1);
      } else if (isAuth) {
        cancelReconnect();
      }
    });

    // Backend fires `session-disconnected-{id}` from the keepalive watcher when
    // a previously-good session is detected as closed (network drop, server
    // kill, idle timeout). Flip into a frozen state — terminals + SFTP are
    // disabled until the user reconnects or closes the tab. Kick off
    // auto-reconnect immediately; the banner shows the countdown.
    const unlistenDisconnected = listen(`session-disconnected-${session.id}`, (event: any) => {
      const reason = event.payload?.reason || "Connection lost";
      const userInitiated = !!event.payload?.user_initiated;
      setStatus('disconnected');
      setDisconnectReason(reason);
      addLog(`SSH_DISCONNECTED [${session.serverName}]: ${reason}`, "error");
      // User-initiated disconnect (right-click → Disconnect) should NOT
      // bounce straight into auto-reconnect — the user explicitly asked
      // for the session to be dead. The "Reconnect" banner button is
      // still available if they change their mind.
      if (!userInitiated) {
        scheduleReconnect(1);
      } else {
        cancelReconnect();
      }
    });

    return () => {
      unlistenLog.then(f => f());
      unlistenPrompt.then(f => f());
      unlistenPromptDismiss.then(f => f());
      unlistenSuccess.then(f => f());
      unlistenFailed.then(f => f());
      unlistenDisconnected.then(f => f());

      if (reconnectTimerRef.current) clearTimeout(reconnectTimerRef.current);
      invoke("disconnect_session", { sessionId: session.id }).catch(console.error);
    };
  }, [session.id, session.serverId]);

  // Force a re-render once per second so the countdown text in the banner
  // stays current without re-allocating timer state on every render.
  const [, forceTick] = useState(0);
  useEffect(() => {
    if (!nextReconnectAt) return;
    const t = setInterval(() => forceTick((n) => n + 1), 500);
    return () => clearInterval(t);
  }, [nextReconnectAt]);

  const scheduleReconnect = (attempt: number) => {
    if (reconnectTimerRef.current) {
      clearTimeout(reconnectTimerRef.current);
      reconnectTimerRef.current = null;
    }
    const delay = Math.min(RECONNECT_MAX_MS, RECONNECT_BASE_MS * Math.pow(2, attempt - 1));
    updateAttempt(attempt);
    setNextReconnectAt(Date.now() + delay);
    reconnectTimerRef.current = setTimeout(() => {
      setNextReconnectAt(null);
      setStatus('connecting');
      invoke("initiate_connection", {
        sessionId: session.id,
        serverId: session.serverId,
        customPassword: customPasswordRef.current || null,
        quickAuth: session.quickAuth || null,
      }).catch(console.error);
    }, delay);
  };

  const reconnect = () => {
    cancelReconnect();
    setStatus('connecting');
    setLogs([]);
    setIsAuthError(false);
    setDisconnectReason("");
    invoke("initiate_connection", {
      sessionId: session.id,
      serverId: session.serverId,
      customPassword: customPassword || null,
      quickAuth: session.quickAuth || null,
    }).catch(console.error);
  };

  // Tab-strip right-click "Reconnect" fires a DOM CustomEvent rather than
  // round-tripping through React props — keeps DesktopApp's tab UI decoupled
  // from SessionView's reconnect lifecycle.
  useEffect(() => {
    const onReconnect = () => reconnect();
    const evt = `session-reconnect-${session.id}`;
    window.addEventListener(evt, onReconnect);
    return () => window.removeEventListener(evt, onReconnect);
  }, [session.id, customPassword]);

  // ---- Tool pane sizing + window growth ------------------------------------
  // Philosophy: the terminal column is sacred. Opening a tool pane or
  // dragging the divider grows or shrinks the OS window in lockstep so the
  // terminal's pixel width never changes underneath the user.

  const appWindow = getCurrentWindow();
  const toolWidthRef = useRef(toolPanelWidth);
  useEffect(() => { toolWidthRef.current = toolPanelWidth; }, [toolPanelWidth]);

  const adjustWindowWidth = async (deltaPx: number) => {
    try {
      const size = await appWindow.outerSize();
      const scale = await appWindow.scaleFactor();
      const logical = size.toLogical(scale);
      const next = Math.max(640, Math.round(logical.width + deltaPx));
      await appWindow.setSize(new LogicalSize(next, Math.round(logical.height)));
    } catch (e) {
      console.error("window resize failed", e);
    }
  };

  // Grow the window when the tool pane is opened, shrink when it's closed.
  // The +4 accounts for the resize divider itself.
  const prevActiveToolRef = useRef<typeof activeTool>(null);
  useEffect(() => {
    const prev = prevActiveToolRef.current;
    prevActiveToolRef.current = activeTool;
    if (!prev && activeTool) {
      adjustWindowWidth(toolWidthRef.current + 4);
    } else if (prev && !activeTool) {
      adjustWindowWidth(-(toolWidthRef.current + 4));
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [activeTool]);

  // If the user closes the session tab while a tool is open, give the
  // window space back rather than leaving it stretched.
  useEffect(() => {
    return () => {
      if (prevActiveToolRef.current) {
        adjustWindowWidth(-(toolWidthRef.current + 4));
      }
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const startToolResize = (e: React.MouseEvent) => {
    e.preventDefault();
    const startX = e.clientX;
    const startWidth = toolWidthRef.current;
    let lastCommittedWidth = startWidth;
    let pendingWidth = startWidth;
    let frameRequested = false;

    const onMove = (ev: MouseEvent) => {
      // Dragging LEFT (cursor moves left) widens the tool pane.
      const next = Math.max(240, Math.min(900, startWidth + (startX - ev.clientX)));
      pendingWidth = next;
      setToolPanelWidth(next);
      // Throttle window resizes to one per animation frame. setSize crosses an
      // IPC boundary and dispatching it on every mousemove makes the drag feel
      // laggy. We batch by recomputing the delta against last-committed width
      // inside the frame, so no mouse movement is dropped.
      if (!frameRequested) {
        frameRequested = true;
        requestAnimationFrame(() => {
          frameRequested = false;
          const delta = pendingWidth - lastCommittedWidth;
          if (delta !== 0) {
            lastCommittedWidth = pendingWidth;
            adjustWindowWidth(delta);
          }
        });
      }
    };

    const onUp = () => {
      window.removeEventListener("mousemove", onMove);
      window.removeEventListener("mouseup", onUp);
      try { localStorage.setItem("submarine-tool-panel-width", String(toolWidthRef.current)); }
      catch { /* ignore */ }
    };

    window.addEventListener("mousemove", onMove);
    window.addEventListener("mouseup", onUp);
  };

  const handleFingerprintResponse = async (accepted: boolean) => {
    // The `nonce` came from the prompt event and binds this response 1:1
    // to the connect attempt that emitted it. The backend ignores any
    // response whose nonce isn't in its in-flight map, so stale clicks
    // (or a hostile script that knows only the session id) can't accept
    // a fingerprint on the user's behalf.
    const nonce = fingerprintPrompt?.nonce;
    setFingerprintPrompt(null);
    if (!nonce) return;
    try {
      await invoke("verify_fingerprint_response", { nonce, accepted });
    } catch (e) {
      console.error(e);
    }
  };

  // Only render the full-screen log view for the FIRST connection — once an
  // auto-reconnect cycle is running, the user's terminal output and SFTP
  // state stay visible behind a slim banner.
  if (reconnectAttempt === 0 && (status === 'connecting' || status === 'failed')) {
    return (
      <div className="flex-1 flex flex-col p-4 sm:p-8 bg-[#0a0a0c] text-white overflow-hidden">
        <div className="max-w-2xl w-full mx-auto flex-1 flex flex-col">
          {/* Header: on desktop, title row keeps title + actions side-by-side.
              On phone, wide letter-spacing on the title wraps "0-1 AMIR" onto
              three lines and the Reconnect / Close buttons get pushed past
              the viewport. We stack vertically and trim the typography so
              the whole header fits in two compact rows at any width. */}
          <div className="flex flex-col sm:flex-row sm:items-center sm:justify-between gap-3 mb-4 sm:mb-6">
            <div>
              <h2 className="text-base sm:text-xl font-black uppercase tracking-wider sm:tracking-[0.2em] break-words">
                {session.serverName}
              </h2>
              <p className="text-[10px] text-zinc-500 font-bold uppercase tracking-widest mt-1">
                {status === 'connecting' ? 'Establishing Connection...' : 'Connection Failed'}
              </p>
            </div>
            {status === 'failed' && (
              <div className="flex flex-wrap gap-2 items-center">
                {isAuthError && (
                  <input
                    type="password"
                    placeholder="Password..."
                    className="h-8 flex-1 min-w-0 sm:flex-none bg-[#1a1a1e] rounded-lg px-3 text-xs text-white border border-white/10 outline-none focus:border-primary/50"
                    value={customPassword}
                    onChange={e => setCustomPassword(e.target.value)}
                    onKeyDown={e => {
                      if(e.key === 'Enter') reconnect();
                    }}
                  />
                )}
                <button onClick={reconnect} className="flex-1 sm:flex-none px-4 py-2 bg-primary/10 text-primary hover:bg-primary/20 rounded-lg text-xs font-bold uppercase tracking-wider transition-colors">
                  Reconnect
                </button>
                <button onClick={onClose} className="flex-1 sm:flex-none px-4 py-2 bg-red-500/10 text-red-500 hover:bg-red-500/20 rounded-lg text-xs font-bold uppercase tracking-wider transition-colors">
                  Close Session
                </button>
              </div>
            )}
          </div>

          {/* Log Window */}
          <div className="flex-1 bg-[#121214] border border-white/5 rounded-2xl p-4 font-mono text-[11px] overflow-y-auto custom-scrollbar shadow-inner relative select-text cursor-text">
            {logs.map((l, i) => (
              <div key={i} className={`mb-2 ${l.type === 'error' ? 'text-red-400' : l.type === 'success' ? 'text-primary' : 'text-zinc-400'}`}>
                <span className="text-zinc-600 opacity-50 mr-3">[{new Date().toLocaleTimeString()}]</span>
                {l.msg}
              </div>
            ))}

            {/* Fingerprint Prompt — two flavors:
                  • mismatch=false → first time seeing this host, light warning
                  • mismatch=true  → host key CHANGED, looks like a MITM,
                                     red treatment + explicit copy that lists
                                     the old fingerprints we used to trust */}
            {fingerprintPrompt && (() => {
              const isMismatch = !!fingerprintPrompt.mismatch;
              const tone = isMismatch
                ? "border-red-500/40 bg-red-500/10"
                : "border-amber-500/30 bg-amber-500/5";
              const accent = isMismatch ? "text-red-400" : "text-amber-500";
              const acceptBtn = isMismatch
                ? "bg-red-500 text-white hover:bg-red-400"
                : "bg-amber-500 text-black hover:bg-amber-400";
              return (
              <div className={`mt-6 p-4 border ${tone} rounded-xl animate-in fade-in slide-in-from-bottom-4`}>
                <div className="flex items-start gap-3">
                  <ShieldAlert className={`${accent} mt-1`} size={20} />
                  <div>
                    <h3 className={`text-sm font-bold ${accent} uppercase tracking-widest`}>
                      {isMismatch ? "Host key has CHANGED" : "Unknown host fingerprint"}
                    </h3>
                    {isMismatch ? (
                      <p className="text-zinc-300 mt-2 mb-4 leading-relaxed">
                        The host '{fingerprintPrompt.host}' is presenting a different key than the one you trusted before. This is what a man-in-the-middle attack looks like — but it can also mean the server admin rotated the key.<br/><br/>
                        New {fingerprintPrompt.keyType} fingerprint: <span className="text-white font-bold break-all">{fingerprintPrompt.fingerprint}</span><br/>
                        {Array.isArray(fingerprintPrompt.priorFingerprints) && fingerprintPrompt.priorFingerprints.length > 0 && (
                          <span className="block mt-1 text-zinc-500 text-[11px]">
                            Previously trusted: <span className="font-mono break-all">{fingerprintPrompt.priorFingerprints.join(", ")}</span>
                          </span>
                        )}
                        <span className="block mt-3 text-red-300 text-[12px]">Verify the new fingerprint out-of-band (call the admin, check the server console) before accepting.</span>
                      </p>
                    ) : (
                      <p className="text-zinc-400 mt-2 mb-4 leading-relaxed">
                        The authenticity of host '{fingerprintPrompt.host}' can't be established.<br/>
                        {fingerprintPrompt.keyType} key fingerprint is <span className="text-white font-bold break-all">{fingerprintPrompt.fingerprint}</span>.<br/>
                        Are you sure you want to continue connecting?
                      </p>
                    )}
                    <div className="flex gap-3">
                      <button
                        onClick={() => handleFingerprintResponse(true)}
                        className={`px-6 py-2 ${acceptBtn} font-bold text-xs uppercase tracking-wider rounded-lg transition-colors flex items-center gap-2`}
                      >
                        <Check size={14} /> {isMismatch ? "Accept new key" : "Accept & save"}
                      </button>
                      <button
                        onClick={() => handleFingerprintResponse(false)}
                        className="px-6 py-2 bg-white/5 text-zinc-300 font-bold text-xs uppercase tracking-wider rounded-lg hover:bg-white/10 transition-colors flex items-center gap-2"
                      >
                        <X size={14} /> {isMismatch ? "Abort" : "Reject"}
                      </button>
                    </div>
                  </div>
                </div>
              </div>
              );
            })()}
          </div>
        </div>
      </div>
    );
  }

  // Connected State with Nested Tabs
  return (
    <div className="flex-1 flex flex-col bg-background overflow-hidden animate-in fade-in">
      {/* Nested Tab Bar */}
      <div className="h-12 border-b border-white/5 bg-[#121214]/50 flex items-center px-2 sm:px-4 shrink-0 justify-between gap-1">
        <div
          className="flex items-center gap-1 overflow-x-auto no-scrollbar flex-1 mr-1 sm:mr-4 mask-fade-right"
          onWheel={(e) => { e.currentTarget.scrollLeft += e.deltaY; }}
        >
        {terminals.map(t => (
          <div key={t.id} className="group relative flex items-center">
            <button
              onClick={() => setActiveTab(t.id)}
              title={t.container ? `Container: ${t.container.name}` : undefined}
              className={`h-8 px-3 sm:px-4 ${terminals.length > 1 ? 'pr-6' : ''} rounded-lg flex items-center gap-2 text-[11px] font-bold uppercase tracking-wider transition-all ${activeTab === t.id ? 'bg-primary/10 text-primary border border-primary/20 shadow-inner' : 'text-zinc-300 bg-white/[0.04] border border-white/10 hover:bg-white/[0.08] hover:border-white/20 hover:text-white'} ${t.container ? 'ring-1 ring-sky-400/30' : ''}`}
            >
              {t.container
                ? <Container size={14} className="text-sky-300" />
                : <TerminalSquare size={14} />}
              {t.title}
            </button>
            {terminals.length > 1 && (
              <button
                onClick={(e) => {
                  e.stopPropagation();
                  setTerminals(prev => prev.filter(x => x.id !== t.id));
                  if (activeTab === t.id) setActiveTab(terminals[0].id);
                }}
                className="absolute right-1 w-5 h-5 flex items-center justify-center opacity-100 sm:opacity-0 sm:group-hover:opacity-100 hover:text-red-500 text-zinc-500 transition-opacity"
              >
                <X size={12} />
              </button>
            )}
          </div>
        ))}

          <button
            onClick={() => {
              // Scope to this session's id (see note on the initial terminal
              // above) so even if the user mashes "+" on two sessions in the
              // same millisecond, the ids can never collide.
              const newId = `${session.id}-term-${Date.now()}`;
              setTerminals(prev => [...prev, { id: newId, title: `${prev.length + 1}` }]);
              setActiveTab(newId);
            }}
            className="h-8 w-8 ml-1 shrink-0 rounded-lg flex items-center justify-center text-zinc-500 hover:bg-white/10 hover:text-white transition-all border border-dashed border-white/10"
            title="New Terminal"
          >
            <div className="text-[14px] font-bold">+</div>
          </button>
        </div>

        {/* Tool toggles: on desktop each shows a label next to the icon; on
            phone we drop the label and the left separator so all four still
            fit comfortably in the same h-12 row as the terminal tabs. Each
            button keeps its 32-px hit target (h-8 w-8) which is well above
            the 24-dp recommended touch minimum. */}
        <div className="flex items-center gap-1 shrink-0 sm:border-l border-white/5 sm:pl-4 pl-1">
          <button
            onClick={() => setActiveTool(activeTool === 'sftp' ? null : 'sftp')}
            title="SFTP"
            className={`h-8 w-8 sm:w-auto sm:px-4 rounded-lg flex items-center justify-center sm:gap-2 text-[11px] font-bold uppercase tracking-wider transition-all ${activeTool === 'sftp' ? 'bg-primary/10 text-primary border border-primary/20 shadow-inner' : 'text-zinc-300 bg-white/[0.04] border border-white/10 hover:bg-white/[0.08] hover:border-white/20 hover:text-white'}`}
          >
            <Folder size={14} /> <span className="hidden sm:inline">SFTP</span>
          </button>
          <button
            onClick={() => setActiveTool(activeTool === 'tunnels' ? null : 'tunnels')}
            title="Ports"
            className={`h-8 w-8 sm:w-auto sm:px-4 rounded-lg flex items-center justify-center sm:gap-2 text-[11px] font-bold uppercase tracking-wider transition-all ${activeTool === 'tunnels' ? 'bg-primary/10 text-primary border border-primary/20 shadow-inner' : 'text-zinc-300 bg-white/[0.04] border border-white/10 hover:bg-white/[0.08] hover:border-white/20 hover:text-white'}`}
          >
            <Network size={14} /> <span className="hidden sm:inline">Ports</span>
          </button>
          <button
            onClick={() => setActiveTool(activeTool === 'cmds' ? null : 'cmds')}
            title="Library — commands, notes, this node's notes"
            className={`h-8 w-8 sm:w-auto sm:px-4 rounded-lg flex items-center justify-center sm:gap-2 text-[11px] font-bold uppercase tracking-wider transition-all ${activeTool === 'cmds' ? 'bg-primary/10 text-primary border border-primary/20 shadow-inner' : 'text-zinc-300 bg-white/[0.04] border border-white/10 hover:bg-white/[0.08] hover:border-white/20 hover:text-white'}`}
          >
            <Library size={14} /> <span className="hidden sm:inline">Library</span>
          </button>
          <button
            onClick={() => setActiveTool(activeTool === 'info' ? null : 'info')}
            title="Info — server overview, network, services, docker"
            className={`h-8 w-8 sm:w-auto sm:px-4 rounded-lg flex items-center justify-center sm:gap-2 text-[11px] font-bold uppercase tracking-wider transition-all ${activeTool === 'info' ? 'bg-primary/10 text-primary border border-primary/20 shadow-inner' : 'text-zinc-300 bg-white/[0.04] border border-white/10 hover:bg-white/[0.08] hover:border-white/20 hover:text-white'}`}
          >
            <Info size={14} /> <span className="hidden sm:inline">Info</span>
          </button>
        </div>
      </div>

      {/* Disconnection / auto-reconnect banner. Pinned to the top so it's
          visible whether the terminal or SFTP is in focus. The disabled
          overlay inside TerminalView / SftpWorkspace / TunnelsPanel does the
          heavy lifting of locking input out. */}
      {(status === 'disconnected' || reconnectAttempt > 0) && (
        (() => {
          const isAttempting = reconnectAttempt > 0 && status === 'connecting';
          const isWaiting = reconnectAttempt > 0 && nextReconnectAt !== null;
          const countdown = nextReconnectAt
            ? Math.max(0, Math.ceil((nextReconnectAt - Date.now()) / 1000))
            : 0;
          const tone =
            isAttempting ? "bg-indigo-500/10 border-indigo-500/30 text-indigo-300" :
            isWaiting    ? "bg-amber-500/10 border-amber-500/30 text-amber-300" :
                           "bg-red-500/10 border-red-500/30 text-red-400";
          const heading =
            isAttempting ? `Reconnecting · attempt ${reconnectAttempt}` :
            isWaiting    ? `Auto-reconnect in ${countdown}s · attempt ${reconnectAttempt}` :
                           "Session disconnected";
          return (
            <div className={`shrink-0 px-4 py-2 border-b flex items-center justify-between gap-3 animate-in fade-in ${tone}`}>
              <div className="flex items-center gap-2 text-[11px] font-bold uppercase tracking-wider">
                <AlertTriangle size={14} />
                <span>{heading}</span>
                {disconnectReason && (
                  <span className="opacity-70 normal-case font-normal tracking-normal">— {disconnectReason}</span>
                )}
              </div>
              <div className="flex items-center gap-2">
                {isWaiting && (
                  <button onClick={cancelReconnect} className="px-3 py-1 bg-white/5 text-zinc-300 hover:bg-white/10 hover:text-white rounded-md text-[10px] font-bold uppercase tracking-wider transition-colors">
                    Cancel
                  </button>
                )}
                <button onClick={reconnect} className="px-3 py-1 bg-primary/10 text-primary hover:bg-primary/20 rounded-md text-[10px] font-bold uppercase tracking-wider transition-colors flex items-center gap-1.5">
                  <Play size={12} /> {isWaiting ? "Now" : "Reconnect"}
                </button>
                <button onClick={onClose} className="px-3 py-1 bg-white/5 text-zinc-400 hover:bg-white/10 hover:text-white rounded-md text-[10px] font-bold uppercase tracking-wider transition-colors">
                  Close
                </button>
              </div>
            </div>
          );
        })()
      )}

      {/* Tab Content Split Pane. Side-by-side on roomy desktop; on narrow
          (`isCompact`) viewports the tool pane takes over the full width
          and the terminal is hidden — the tool tabs themselves act as the
          "back to terminal" affordance (clicking the active tool toggles
          it off). This avoids squeezing a usable terminal + tool into a
          mobile-sized window. */}
      <div className="flex-1 flex overflow-hidden relative bg-[#09090b]">
        {/* Left Panel: Active Terminals.
            On compact + activeTool, hide entirely so the tool fills the
            screen. Terminals stay mounted (no PTY teardown) — just CSS
            hidden so swapping back keeps the same shell session. */}
        <div className={`h-full relative ${activeTool && isCompact ? 'hidden' : 'flex-1 min-w-0'}`}>
          {terminals.map(t => (
            <div key={t.id} className={`absolute inset-0 ${activeTab === t.id ? 'opacity-100 z-10' : 'opacity-0 z-0 pointer-events-none'}`}>
              <TerminalView
                sessionId={session.id}
                terminalId={t.id}
                disabled={status !== 'connected'}
                isActive={activeTab === t.id && !(activeTool && isCompact)}
                containerExec={t.container ? { container: t.container.name, useSudo: t.container.useSudo } : undefined}
                connectionEpoch={connectionEpoch}
              />
            </div>
          ))}
        </div>

        {/* Resizable divider — only useful when both panes are visible. */}
        {activeTool && !isCompact && (
          <div
            onMouseDown={startToolResize}
            className="w-1 shrink-0 cursor-col-resize bg-white/5 hover:bg-primary/40 transition-colors"
            title="Drag to resize"
          />
        )}

        {/* Tool side panel. MirrorsPanel stays mounted even when the user
            pops over to SFTP / Ports / CMDS so its live status, log, and
            event subscriptions don't reset every tool switch — a mirror in
            initial-sync used to "look like it restarted" because the React
            tree was torn down. Other tools still conditional-render: they
            don't carry live state worth preserving across switches. */}
        <div
          style={activeTool && !isCompact ? { width: `${toolPanelWidth}px` } : undefined}
          className={`${activeTool ? (isCompact ? 'flex-1 min-w-0' : 'shrink-0') : 'hidden'} bg-[#121214]/95 flex flex-col h-full overflow-hidden ${activeTool ? 'animate-in slide-in-from-right duration-300' : ''}`}
        >
          {activeTool === 'sftp' && (
            <div className="flex-1 flex flex-col overflow-hidden">
              <div className="h-10 px-4 flex items-center justify-between border-b border-white/5 bg-white/5">
                <span className="text-[10px] font-bold uppercase tracking-wider text-zinc-400">SFTP File Browser</span>
                <button onClick={() => setActiveTool(null)} className="text-zinc-500 hover:text-white transition-colors">
                  <X size={14} />
                </button>
              </div>
              <div className="flex-1 overflow-hidden relative">
                <SftpWorkspace
                  sessionId={session.id}
                  disabled={status !== 'connected'}
                  serverId={session.serverId}
                  mirrorsConfig={(() => {
                    try { return JSON.parse(session.mirrors || "[]"); } catch { return []; }
                  })()}
                />
              </div>
            </div>
          )}

          {activeTool === 'tunnels' && (
            <div className="flex-1 flex flex-col overflow-hidden">
              <div className="h-10 px-4 flex items-center justify-between border-b border-white/5 bg-white/5">
                <span className="text-[10px] font-bold uppercase tracking-wider text-zinc-400">Port Forwarding</span>
                <button onClick={() => setActiveTool(null)} className="text-zinc-500 hover:text-white transition-colors">
                  <X size={14} />
                </button>
              </div>
              <div className="flex-1 overflow-hidden relative">
                <TunnelsPanel sessionId={session.id} disabled={status !== 'connected'} />
              </div>
            </div>
          )}

          {activeTool === 'cmds' && (
            <CmdsPanel
              activeTab={activeTab}
              onClose={() => setActiveTool(null)}
              serverId={session.serverId}
              serverName={session.serverName}
            />
          )}

          {/* InfoPanel stays mounted across tool switches so its cached
              probe result survives a SFTP-and-back trip — the user
              explicitly asked for "cache while session is alive", and a
              fresh remount would re-fire the probe every tab swap. */}
          <div className={`flex-1 flex flex-col overflow-hidden ${activeTool === 'info' ? '' : 'hidden'}`}>
            <div className="h-10 px-4 flex items-center justify-between border-b border-white/5 bg-white/5">
              <span className="text-[10px] font-bold uppercase tracking-wider text-zinc-400">Server Info</span>
              <button onClick={() => setActiveTool(null)} className="text-zinc-500 hover:text-white transition-colors">
                <X size={14} />
              </button>
            </div>
            <InfoPanel
              sessionId={session.id}
              disabled={status !== 'connected'}
              onOpenContainerTerminal={openContainerTerminal}
            />
          </div>
        </div>
      </div>
    </div>
  );

};

/// Memoised export so a re-render in DesktopApp (sessionStatuses changing
/// when ANY session connects/reconnects/drops) doesn't cascade through
/// every other session subtree. The default shallow comparator is fine
/// because the parent now hands stable closures (`addLog`, `onClose`,
/// `onStatusChange`) and `session` only changes identity when the user
/// adds or edits a row.
export const SessionView = memo(SessionViewImpl);
