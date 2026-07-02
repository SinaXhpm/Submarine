import { useState, useEffect, useRef, memo } from "react";
import { createPortal } from "react-dom";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow, LogicalSize } from "@tauri-apps/api/window";
import { TerminalSquare, Folder, Network, AlertTriangle, Check, X, ShieldAlert, Play, Library, Info, Container, Plus, SplitSquareHorizontal, Columns, Rows, LayoutGrid } from "lucide-react";
import TerminalView from "./TerminalView";
import SftpWorkspace from "./SftpWorkspace";
import TunnelsPanel from "./TunnelsPanel";
import InfoPanel from "./InfoPanel";
import { CmdsPanel } from "./CmdsPanel";
import { useIsCompact } from "../hooks/useViewport";

const SessionViewImpl = ({ session, onClose, addLog, onStatusChange, onOpenCompare }: any) => {
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
  const [logs, setLogs] = useState<{ msg: string, type: string, time?: string }[]>([]);
  // Cap to keep a chatty session (monitors, mirrors, MOTD spam) from
  // ballooning React state — each render copies the whole array, so an
  // unbounded log makes the UI quadratically slower over time.
  const LOG_CAP = 500;
  const pushLog = (entry: { msg: string, type: string }) => {
    const stamped = { ...entry, time: new Date().toLocaleTimeString() };
    setLogs(prev => prev.length >= LOG_CAP
      ? [...prev.slice(prev.length - LOG_CAP + 1), stamped]
      : [...prev, stamped]);
  };
  const [fingerprintPrompt, setFingerprintPrompt] = useState<any>(null);
  const [isAuthError, setIsAuthError] = useState(false);
  const [customPassword, setCustomPassword] = useState("");
  // The connection effect runs once and captures `customPassword=""` in its
  // closure. Auto-reconnect attempts and the disconnect listener that
  // schedules them must read the LATEST password the user typed into the
  // auth-error retry input — so we mirror it into a ref.
  const customPasswordRef = useRef("");
  useEffect(() => { customPasswordRef.current = customPassword; }, [customPassword]);

  // Mirror `status` into a ref so the one-shot effect below (which registers
  // the `connection-success` listener from its INITIAL closure) can see the
  // latest value. Without this ref, `wasReconnect` was computed against a
  // status snapshot captured at effect-run time (always `'connecting'`), so
  // a manual reconnect via the banner — which zeroes reconnectAttemptRef —
  // failed to bump `connectionEpoch`. TerminalView's per-epoch effect then
  // never re-called open_terminal against the new SSH handle, and the
  // reconnected terminal appeared frozen while SFTP/tunnels worked fine.
  const statusRef = useRef(status);
  useEffect(() => { statusRef.current = status; }, [status]);

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
  // Split-pane state — an ORDERED array of terminal IDs that currently
  // share the main pane. `[]` or a single-id array means "no split, use
  // the usual absolute-overlap layout"; length ≥ 2 means the panes tile
  // side-by-side (or stacked). `activeTab` still tracks which of the split
  // members owns keyboard focus; clicking any pane re-focuses it.
  //
  // We kept this an array instead of a recursive tree — tmux-style
  // arbitrary geometry is neat but 95% of users want "N shells sharing
  // one screen" and a flat layout is trivially resizable, mountable, and
  // reorderable. Add-to-split appends to the end; close-from-split
  // filters that id out.
  const [splitTerminals, setSplitTerminals] = useState<string[]>([]);
  // Horizontal (row) vs vertical (column). The trigger button next to
  // "+" hosts the orientation swap; the same right-click on any divider
  // also flips it.
  const [splitOrientation, setSplitOrientation] = useState<"h" | "v">("h");
  // Per-pane flex ratio, length always equal to `splitTerminals.length`.
  // Sum is arbitrary (we hand it directly to flex-grow so relative
  // proportions are what matters). Add-to-split appends `1` so the new
  // pane comes in equal-sized with its neighbours; drag between two
  // panes redistributes those two panes' share and leaves the rest
  // alone. Reset (double-click a divider) flattens back to all-equal.
  const [splitRatios, setSplitRatios] = useState<number[]>([]);
  // "+" button's secondary menu — surfaced via right-click / long-press
  // so the primary click stays a fast "new tab" without stealing the
  // advanced flows (split view, compare across servers). Coordinates are
  // window-space, matching the pattern used by the session tab menu at
  // App level.
  const [plusMenu, setPlusMenu] = useState<{ x: number; y: number } | null>(null);
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
      // statusRef.current is authoritative here — the plain `status` we
      // could reach through closure is frozen at effect-run time.
      const prevStatus = statusRef.current;
      const wasReconnect = reconnectAttemptRef.current > 0 || prevStatus === 'disconnected' || prevStatus === 'failed';
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
                <span className="text-zinc-600 opacity-50 mr-3">[{l.time}]</span>
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
        {terminals.map(t => {
          // Highlight every terminal currently participating in the split
          // — the keyboard-focused one gets the primary tint, the others
          // a softer accent, so the pairing is obvious at a glance.
          const isFocusedHalf = activeTab === t.id;
          const isSplitPartner = !!(splitTerminals.includes(t.id) && !isFocusedHalf);
          return (
          <div key={t.id} className="group relative flex items-center">
            <button
              onClick={() => {
                // Clicking a tab NOT participating in the current split
                // exits split mode entirely — split is scoped to the
                // panes it was built with; jumping elsewhere means the
                // user wants a plain full-pane view.
                if (splitTerminals.length >= 2 && !splitTerminals.includes(t.id)) {
                  setSplitTerminals([]);
                  setSplitRatios([]);
                }
                setActiveTab(t.id);
              }}
              title={t.container ? `Container: ${t.container.name}` : undefined}
              className={`h-8 px-3 sm:px-4 ${terminals.length > 1 ? 'pr-6' : ''} rounded-lg flex items-center gap-2 text-[11px] font-bold uppercase tracking-wider transition-all ${
                isFocusedHalf
                  ? 'bg-primary/10 text-primary border border-primary/20 shadow-inner'
                  : isSplitPartner
                    ? 'bg-primary/[0.04] text-primary/70 border border-primary/10'
                    : 'text-zinc-300 bg-white/[0.04] border border-white/10 hover:bg-white/[0.08] hover:border-white/20 hover:text-white'
              } ${t.container ? 'ring-1 ring-sky-400/30' : ''}`}
            >
              {t.container
                ? <Container size={14} className="text-sky-300" />
                : <TerminalSquare size={14} />}
              {t.title}
              {isSplitPartner && (
                <SplitSquareHorizontal size={10} className="text-primary/60" />
              )}
            </button>
            {terminals.length > 1 && (
              <button
                onClick={(e) => {
                  e.stopPropagation();
                  setTerminals(prev => prev.filter(x => x.id !== t.id));
                  if (activeTab === t.id) setActiveTab(terminals[0].id);
                  // Closing a terminal that's participating in the split
                  // has to drop it from splitTerminals AND splitRatios or
                  // the flex layout renders a torn-down partner.
                  if (splitTerminals.includes(t.id)) {
                    const idx = splitTerminals.indexOf(t.id);
                    const nextTerms = splitTerminals.filter(id => id !== t.id);
                    const nextRatios = splitRatios.filter((_, i) => i !== idx);
                    // Falling below 2 members collapses back to the plain
                    // absolute-overlap layout.
                    if (nextTerms.length < 2) {
                      setSplitTerminals([]);
                      setSplitRatios([]);
                    } else {
                      setSplitTerminals(nextTerms);
                      setSplitRatios(nextRatios);
                    }
                  }
                }}
                aria-label="Close terminal"
                className="absolute right-1 top-1/2 -translate-y-1/2 w-5 h-5 flex items-center justify-center rounded opacity-100 sm:opacity-0 sm:group-hover:opacity-100 hover:bg-white/5 hover:text-red-400 text-zinc-500 transition-all"
              >
                <X size={12} />
              </button>
            )}
          </div>
          );
        })}

          {/* Single "+" affordance for all terminal-creation flavours.
              Click adds a plain tab (the 90% case). The right-click menu
              — also reachable via long-press on touch — surfaces the
              advanced modes: split-with-new-pane and un-split. That
              keeps the tab strip visually calm (one small button, no
              badges, no orientation glyphs stealing attention) while
              still exposing the power features via a discoverable
              secondary gesture. */}
          <button
            onClick={() => {
              const newId = `${session.id}-term-${Date.now()}`;
              setTerminals(prev => [...prev, { id: newId, title: `${prev.length + 1}` }]);
              setActiveTab(newId);
            }}
            onContextMenu={(e) => {
              e.preventDefault();
              const rect = e.currentTarget.getBoundingClientRect();
              setPlusMenu({ x: rect.left, y: rect.bottom + 4 });
            }}
            className="h-10 w-10 sm:h-8 sm:w-8 ml-1 shrink-0 rounded-lg flex items-center justify-center text-zinc-500 hover:bg-white/10 hover:text-white transition-all border border-dashed border-white/10 relative"
            title="New terminal · right-click for split view"
          >
            <Plus size={14} />
            {splitTerminals.length >= 2 && (
              <span
                className="absolute -top-1 -right-1 min-w-[14px] h-[14px] px-1 rounded-full bg-primary text-[9px] font-bold text-black flex items-center justify-center leading-none"
                aria-hidden="true"
                title={`Split active (${splitTerminals.length} panes)`}
              >
                {splitTerminals.length}
              </span>
            )}
          </button>
        </div>

        {/* Tool rail: icon-only across every viewport so the terminal
            tab strip owns the horizontal real estate. Labels moved to
            tooltips (native title + aria-label). This clawed back ~200
            px on desktop that was previously spent on repeated bold-
            uppercase "SFTP / PORTS / LIBRARY / INFO" text — the icons
            plus the always-visible active-state tint already convey
            which tool is open. */}
        <div className="flex items-center gap-1 shrink-0 sm:border-l border-white/5 sm:pl-3 pl-1">
          {([
            { id: 'sftp',    icon: Folder,  label: 'SFTP — file browser' },
            { id: 'tunnels', icon: Network, label: 'Ports — port forwarding' },
            { id: 'cmds',    icon: Library, label: 'Library — commands & notes' },
            { id: 'info',    icon: Info,    label: 'Info — server overview' },
          ] as const).map(({ id, icon: Icon, label }) => {
            const on = activeTool === id;
            return (
              <button
                key={id}
                onClick={() => setActiveTool(on ? null : id)}
                title={label}
                aria-label={label}
                aria-pressed={on}
                className={`h-10 w-10 sm:h-8 sm:w-8 rounded-lg flex items-center justify-center transition-all ${
                  on
                    ? 'bg-primary/10 text-primary border border-primary/20 shadow-inner'
                    : 'text-zinc-300 bg-white/[0.04] border border-white/10 hover:bg-white/[0.08] hover:border-white/20 hover:text-white'
                }`}
              >
                <Icon size={14} />
              </button>
            );
          })}
        </div>
      </div>

      {/* "+" secondary menu — right-click / long-press on the plus
          button. Kept intentionally short: three items covering "same
          server + tile" (split), the h/v orientation flip, and "cross-
          server tile" (opens the Compare workspace pre-selected with
          this session). Everything else is one primary click on "+". */}
      {plusMenu && createPortal(
        <>
          <div
            className="fixed inset-0 z-[9998]"
            onClick={() => setPlusMenu(null)}
            onContextMenu={(e) => { e.preventDefault(); setPlusMenu(null); }}
          />
          <div
            style={{ left: Math.min(plusMenu.x, window.innerWidth - 260), top: Math.min(plusMenu.y, window.innerHeight - 220) }}
            className="fixed z-[9999] w-[240px] bg-[#15151a] border border-white/10 rounded-lg shadow-2xl py-1 text-[11.5px]"
          >
            {!isCompact && (
              <>
                <button
                  onClick={() => {
                    setPlusMenu(null);
                    const newId = `${session.id}-term-${Date.now()}`;
                    setTerminals(prev => [...prev, { id: newId, title: `${prev.length + 1}` }]);
                    if (splitTerminals.length >= 2) {
                      setSplitTerminals(prev => [...prev, newId]);
                      setSplitRatios(prev => [...prev, 1]);
                    } else {
                      setSplitTerminals([activeTab, newId]);
                      setSplitRatios([1, 1]);
                    }
                    setActiveTab(newId);
                  }}
                  className="w-full flex items-center gap-2.5 px-3 py-1.5 hover:bg-white/[0.06] text-zinc-200 hover:text-white text-left"
                >
                  {splitOrientation === "h" ? <Columns size={13} className="text-primary/70" /> : <Rows size={13} className="text-primary/70" />}
                  <span className="flex-1">{splitTerminals.length >= 2 ? `Add pane (${splitTerminals.length + 1}-way split)` : "Split with new pane"}</span>
                </button>
                <button
                  onClick={() => {
                    setSplitOrientation(o => o === "h" ? "v" : "h");
                    setPlusMenu(null);
                  }}
                  className="w-full flex items-center gap-2.5 px-3 py-1.5 hover:bg-white/[0.06] text-zinc-300 hover:text-white text-left"
                >
                  {splitOrientation === "h" ? <Rows size={13} className="text-zinc-500" /> : <Columns size={13} className="text-zinc-500" />}
                  <span className="flex-1">Flip orientation ({splitOrientation === "h" ? "→ vertical" : "→ horizontal"})</span>
                </button>
                {splitTerminals.length >= 2 && (
                  <button
                    onClick={() => {
                      setSplitTerminals([]);
                      setSplitRatios([]);
                      setPlusMenu(null);
                    }}
                    className="w-full flex items-center gap-2.5 px-3 py-1.5 hover:bg-white/[0.06] text-zinc-300 hover:text-white text-left"
                  >
                    <X size={13} className="text-zinc-500" />
                    <span className="flex-1">Exit split (keep terminals)</span>
                  </button>
                )}
                <div className="h-px bg-white/5 my-1" />
              </>
            )}
            {onOpenCompare && (
              <button
                onClick={() => { onOpenCompare(session.id); setPlusMenu(null); }}
                className="w-full flex items-center gap-2.5 px-3 py-1.5 hover:bg-white/[0.06] text-zinc-300 hover:text-white text-left"
              >
                <LayoutGrid size={13} className="text-primary/70" />
                <span className="flex-1">Compare with other servers…</span>
              </button>
            )}
          </div>
        </>,
        document.body
      )}

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
                  <button onClick={cancelReconnect} className="px-3 py-1.5 sm:py-1 min-h-8 sm:min-h-6 bg-white/5 text-zinc-300 hover:bg-white/10 hover:text-white rounded-md text-[10px] font-bold uppercase tracking-wider transition-colors">
                    Cancel
                  </button>
                )}
                <button onClick={reconnect} className="px-3 py-1.5 sm:py-1 min-h-8 sm:min-h-6 bg-primary/10 text-primary hover:bg-primary/20 rounded-md text-[10px] font-bold uppercase tracking-wider transition-colors flex items-center gap-1.5">
                  <Play size={12} /> {isWaiting ? "Now" : "Reconnect"}
                </button>
                <button onClick={onClose} className="px-3 py-1.5 sm:py-1 min-h-8 sm:min-h-6 bg-white/5 text-zinc-400 hover:bg-white/10 hover:text-white rounded-md text-[10px] font-bold uppercase tracking-wider transition-colors">
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
            hidden so swapping back keeps the same shell session.

            Split mode: when `splitTerminals` holds 2+ ids and the
            viewport is roomy enough, we tile those terminals side-by-side
            (or stacked, per `splitOrientation`) with draggable dividers
            between adjacent panes. All OTHER terminals stay mounted but
            hidden — so switching a tab that isn't in the split preserves
            its scrollback. The tiles fill the whole left panel; the tool
            side panel continues to work exactly as before. */}
        <div className={`h-full relative ${activeTool && isCompact ? 'hidden' : 'flex-1 min-w-0'}`}>
          {splitTerminals.length >= 2 && !isCompact ? (
            <div className={`absolute inset-0 flex ${splitOrientation === "h" ? "flex-row" : "flex-col"}`}>
              {splitTerminals.map((termId, slotIdx) => {
                const t = terminals.find(x => x.id === termId);
                if (!t) return null;
                const isFocused = activeTab === t.id;
                const grow = splitRatios[slotIdx] ?? 1;
                const notLast = slotIdx < splitTerminals.length - 1;
                return (
                  <div
                    key={t.id}
                    style={{ flexGrow: grow, flexBasis: 0 }}
                    onMouseDown={() => setActiveTab(t.id)}
                    onTouchStart={() => setActiveTab(t.id)}
                    className={`relative min-w-0 min-h-0 border ${
                      isFocused
                        ? "border-primary/40 shadow-[inset_0_0_0_1px_rgba(var(--primary),0.15)]"
                        : "border-white/[0.03]"
                    }`}
                  >
                    <TerminalView
                      sessionId={session.id}
                      terminalId={t.id}
                      disabled={status !== 'connected'}
                      isActive={isFocused && !(activeTool && isCompact)}
                      containerExec={t.container ? { container: t.container.name, useSudo: t.container.useSudo } : undefined}
                      connectionEpoch={connectionEpoch}
                      serverId={session.serverId}
                      serverName={session.serverName}
                    />
                    {/* Per-pane detach affordance — pulls the pane out of
                        the split without killing the underlying terminal.
                        Positioned top-right so it doesn't fight the
                        divider (which lives on the pane's right/bottom
                        edge). Hidden until pane hover, tiny hit area. */}
                    <button
                      onClick={(e) => {
                        e.stopPropagation();
                        setSplitTerminals(prev => prev.filter(id => id !== t.id));
                        setSplitRatios(prev => prev.filter((_, i) => i !== slotIdx));
                        // If the focused pane left the split, hand focus
                        // to whichever pane is left (or fall back to the
                        // orphan terminal itself).
                        if (isFocused) {
                          const remaining = splitTerminals.filter(id => id !== t.id);
                          setActiveTab(remaining[0] ?? t.id);
                        }
                      }}
                      title="Remove this pane from the split (keeps the terminal)"
                      className="absolute top-1 right-1 z-30 h-5 w-5 rounded flex items-center justify-center bg-black/40 border border-white/10 text-zinc-400 hover:text-white hover:bg-black/60 opacity-0 hover:opacity-100 group-hover/pane:opacity-100 transition-opacity"
                    >
                      <X size={11} />
                    </button>
                    {/* Divider between this pane and the next. Rendered
                        only when there IS a next pane — otherwise a
                        trailing divider would float in dead space. */}
                    {notLast && (
                      <div
                        onMouseDown={(e) => {
                          e.preventDefault();
                          e.stopPropagation();
                          const container = e.currentTarget.parentElement?.parentElement;
                          if (!container) return;
                          const rect = container.getBoundingClientRect();
                          const total = splitOrientation === "h" ? rect.width : rect.height;
                          const startClient = splitOrientation === "h" ? e.clientX : e.clientY;
                          const sumGrow = splitRatios.reduce((a, b) => a + (b || 0), 0) || splitTerminals.length;
                          const pxPerUnit = total / sumGrow;
                          const startA = splitRatios[slotIdx] ?? 1;
                          const startB = splitRatios[slotIdx + 1] ?? 1;
                          const move = (ev: MouseEvent) => {
                            const client = splitOrientation === "h" ? ev.clientX : ev.clientY;
                            const deltaUnits = (client - startClient) / pxPerUnit;
                            // Redistribute between the two adjacent panes
                            // only — the rest of the row stays untouched.
                            // Clamp each so neither collapses below 0.3
                            // units (≈15% of the pair's original span).
                            const minPair = 0.3;
                            const combined = startA + startB;
                            const nextA = Math.max(minPair, Math.min(combined - minPair, startA + deltaUnits));
                            const nextB = combined - nextA;
                            setSplitRatios(prev => {
                              const copy = prev.slice();
                              copy[slotIdx] = nextA;
                              copy[slotIdx + 1] = nextB;
                              return copy;
                            });
                          };
                          const up = () => {
                            window.removeEventListener("mousemove", move);
                            window.removeEventListener("mouseup", up);
                          };
                          window.addEventListener("mousemove", move);
                          window.addEventListener("mouseup", up);
                        }}
                        onDoubleClick={(e) => {
                          e.stopPropagation();
                          // Reset all panes to equal share, not just this
                          // divider's pair — matches the "reset layout"
                          // idiom users expect from tmux/tiling WMs.
                          setSplitRatios(splitTerminals.map(() => 1));
                        }}
                        onContextMenu={(e) => {
                          e.preventDefault();
                          e.stopPropagation();
                          setSplitOrientation(o => o === "h" ? "v" : "h");
                        }}
                        title="Drag to resize · double-click resets · right-click flips orientation"
                        className={`absolute z-20 bg-white/5 hover:bg-primary/40 transition-colors ${
                          splitOrientation === "h"
                            ? "top-0 bottom-0 right-0 w-1 cursor-col-resize"
                            : "left-0 right-0 bottom-0 h-1 cursor-row-resize"
                        }`}
                      />
                    )}
                  </div>
                );
              })}
              {/* Off-screen host for terminals that are NOT in the split.
                  Keeps them mounted so their PTY buffer / scrollback
                  survives — critical for the "compare" workflow where the
                  user swaps a partner in and out of the tile. */}
              <div className="hidden">
                {terminals.filter(t => !splitTerminals.includes(t.id)).map(t => (
                  <TerminalView
                    key={t.id}
                    sessionId={session.id}
                    terminalId={t.id}
                    disabled={status !== 'connected'}
                    isActive={false}
                    containerExec={t.container ? { container: t.container.name, useSudo: t.container.useSudo } : undefined}
                    connectionEpoch={connectionEpoch}
                    serverId={session.serverId}
                    serverName={session.serverName}
                  />
                ))}
              </div>
            </div>
          ) : (
            terminals.map(t => (
              <div key={t.id} className={`absolute inset-0 ${activeTab === t.id ? 'opacity-100 z-10' : 'opacity-0 z-0 pointer-events-none'}`}>
                <TerminalView
                  sessionId={session.id}
                  terminalId={t.id}
                  disabled={status !== 'connected'}
                  isActive={activeTab === t.id && !(activeTool && isCompact)}
                  containerExec={t.container ? { container: t.container.name, useSudo: t.container.useSudo } : undefined}
                  connectionEpoch={connectionEpoch}
                  serverId={session.serverId}
                  serverName={session.serverName}
                />
              </div>
            ))
          )}
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
