import { useState, useEffect, useRef, memo } from "react";
import { createPortal } from "react-dom";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow, LogicalSize } from "@tauri-apps/api/window";
import { TerminalSquare, Folder, Network, AlertTriangle, Check, X, ShieldAlert, KeyRound, Play, Library, Info, Container, Plus, SplitSquareHorizontal, Columns, Rows, RotateCw } from "lucide-react";
import TerminalView from "./TerminalView";
import SftpWorkspace from "./SftpWorkspace";
import TunnelsPanel from "./TunnelsPanel";
import InfoPanel from "./InfoPanel";
import { CmdsPanel } from "./CmdsPanel";
import { useIsCompact } from "../hooks/useViewport";

// Compact "run this tab on its own dedicated SSH connection" toggle, shown in
// the SFTP and Port-Forwarding tab headers. The status dot reflects the live
// state of the dedicated connection: green = up, amber (pulsing) = opening /
// applies-on-connect, red = couldn't be established (falling back to the main
// session). No dot when the toggle is off.
const SepToggle = ({ on, onToggle, status, title, onReconnect }: {
  on: boolean;
  onToggle: (v: boolean) => void;
  status: 'ready' | 'pending' | 'failed' | 'off';
  title: string;
  onReconnect?: () => void;
}) => (
  <span className="flex items-center gap-1.5">
    <label className="flex items-center gap-1.5 text-[10px] text-zinc-400 hover:text-zinc-200 cursor-pointer select-none" title={title}>
      <input type="checkbox" className="w-3 h-3 accent-primary" checked={on} onChange={(e) => onToggle(e.target.checked)} />
      <span className="uppercase tracking-wider font-bold">Dedicated session</span>
      {on && (
        <span
          className={`w-1.5 h-1.5 rounded-full shrink-0 ${
            status === 'ready' ? 'bg-emerald-400' : status === 'failed' ? 'bg-red-400' : 'bg-amber-400 animate-pulse'
          }`}
        />
      )}
    </label>
    {/* Reconnect just the dedicated connection — outside the <label> so a
        click can't toggle the checkbox. Shown whenever the toggle is on and
        no connect attempt is already in flight; also the retry affordance
        after a red (failed) dot. */}
    {on && onReconnect && status !== 'pending' && (
      <button
        onClick={onReconnect}
        title="Reconnect the dedicated session"
        className="p-0.5 rounded text-zinc-500 hover:text-white hover:bg-white/10 transition-colors"
      >
        <RotateCw size={10} />
      </button>
    )}
  </span>
);

const SessionViewImpl = ({ session, onClose, addLog, onStatusChange, chromeless = false, onTerminalsChange }: any) => {
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
  // Keyboard-interactive (2FA / verification-code) prompt. `kbiPrompt` holds
  // the backend payload ({ nonce, name, instructions, prompts:[{prompt,echo}] });
  // `kbiValues` mirrors one editable answer per prompt.
  const [kbiPrompt, setKbiPrompt] = useState<any>(null);
  const [kbiValues, setKbiValues] = useState<string[]>([]);
  const [isAuthError, setIsAuthError] = useState(false);
  const [customPassword, setCustomPassword] = useState("");
  // The connect-time log box mirrors the terminal font-size setting, so the
  // user's chosen size applies to the startup logs too — not just the shell.
  // `submarine-settings-changed` (dispatched by Settings on save) keeps it live.
  const readLogFontSize = () =>
    Math.max(1, parseInt(localStorage.getItem('submarine-terminal-font-size') || '14') || 14);
  const [logFontSize, setLogFontSize] = useState(readLogFontSize);
  useEffect(() => {
    const sync = () => setLogFontSize(readLogFontSize());
    window.addEventListener('submarine-settings-changed', sync);
    return () => window.removeEventListener('submarine-settings-changed', sync);
  }, []);
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

  // ---- Dedicated SSH connections for SFTP / port-forwarding -----------------
  // Two INDEPENDENT, per-tab toggles (the controls live in the SFTP and
  // Port-Forwarding tab headers, persisted globally in localStorage under
  // `submarine-separate-sftp` / `submarine-separate-fwd`, default OFF). When a
  // toggle is on we open a dedicated secondary SSH connection — keyed
  // `${session.id}::sftp` / `${session.id}::fwd` — as a pure TRANSPORT: the
  // backend routes SFTP subsystems and tunnels over it when it exists, while
  // everything stays keyed/tagged under the plain session id. The panels never
  // re-key, tunnels migrate live in both directions, and if a dedicated
  // connection can't be established (2FA server, network blip) everything just
  // rides the primary. Unexpected drops auto-retry with backoff, and each tab
  // header has a manual reconnect button for the dedicated connection.
  type SecondaryStatus = 'off' | 'connecting' | 'ready' | 'failed';
  const [separateSftp, setSeparateSftp] = useState(localStorage.getItem('submarine-separate-sftp') === 'true');
  const [separateFwd, setSeparateFwd] = useState(localStorage.getItem('submarine-separate-fwd') === 'true');
  const separateSftpRef = useRef(separateSftp);
  const separateFwdRef = useRef(separateFwd);
  useEffect(() => { separateSftpRef.current = separateSftp; }, [separateSftp]);
  useEffect(() => { separateFwdRef.current = separateFwd; }, [separateFwd]);

  // Live status of the dedicated connections — drives the tab-header dots and
  // the retry logic only; nothing routes on it (the backend picks transports).
  const [sftpConnStatus, setSftpConnStatus] = useState<SecondaryStatus>('off');
  const [fwdConnStatus, setFwdConnStatus] = useState<SecondaryStatus>('off');
  const sftpConnStatusRef = useRef<SecondaryStatus>('off');
  const fwdConnStatusRef = useRef<SecondaryStatus>('off');
  useEffect(() => { sftpConnStatusRef.current = sftpConnStatus; }, [sftpConnStatus]);
  useEffect(() => { fwdConnStatusRef.current = fwdConnStatus; }, [fwdConnStatus]);

  // Bounded auto-retry for unexpected secondary drops: 2s → 5s → 10s, then
  // give up (red dot; manual reconnect button still works). Counters reset on
  // success and on any manual action.
  const SECONDARY_RETRY_DELAYS = [2000, 5000, 10000];
  const sftpRetryRef = useRef(0);
  const fwdRetryRef = useRef(0);
  const sftpRetryTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const fwdRetryTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const clearSecondaryRetryTimers = () => {
    if (sftpRetryTimerRef.current) { clearTimeout(sftpRetryTimerRef.current); sftpRetryTimerRef.current = null; }
    if (fwdRetryTimerRef.current) { clearTimeout(fwdRetryTimerRef.current); fwdRetryTimerRef.current = null; }
  };

  const openSftpConn = () => {
    setSftpConnStatus('connecting');
    invoke("initiate_connection", {
      sessionId: `${session.id}::sftp`,
      serverId: session.serverId,
      customPassword: customPasswordRef.current || null,
      quickAuth: session.quickAuth || null,
      sessionRole: "sftp",
      separateSessions: false,
    }).catch(console.error);
  };
  const openFwdConn = () => {
    setFwdConnStatus('connecting');
    invoke("initiate_connection", {
      sessionId: `${session.id}::fwd`,
      serverId: session.serverId,
      customPassword: customPasswordRef.current || null,
      quickAuth: session.quickAuth || null,
      sessionRole: "forward",
      separateSessions: false,
    }).catch(console.error);
  };

  // Restore forwarding on the best available transport (primary, unless a
  // dedicated connection is up). Called when the dedicated path is abandoned —
  // toggle-off, or retries exhausted — so tunnels always land somewhere.
  const restoreTunnelsOnPrimary = () => {
    invoke("restart_session_tunnels", { sessionId: session.id, serverId: session.serverId }).catch(console.error);
  };

  // Whether the in-flight PRIMARY connect deferred its saved tunnels for a
  // `::fwd` connection (i.e. separateFwd was on when we sent it). If the user
  // toggles fwd OFF during the primary handshake, the primary won't have
  // started the tunnels AND openSecondaryConnections won't open `::fwd`, so
  // they'd start nowhere — the success handler reconciles using this.
  const primarySeparateRef = useRef(false);

  // Schedule an auto-retry for a dropped secondary. Fires only if the toggle
  // is still on and the primary is still connected at fire time.
  const scheduleSecondaryRetry = (which: 'sftp' | 'fwd') => {
    const retryRef = which === 'sftp' ? sftpRetryRef : fwdRetryRef;
    const timerRef = which === 'sftp' ? sftpRetryTimerRef : fwdRetryTimerRef;
    const enabledRef = which === 'sftp' ? separateSftpRef : separateFwdRef;
    const setStatus = which === 'sftp' ? setSftpConnStatus : setFwdConnStatus;
    if (retryRef.current >= SECONDARY_RETRY_DELAYS.length) {
      // Out of retries — mark failed; operations ride the primary. For
      // forwarding, explicitly restore the tunnels there.
      setStatus('failed');
      if (which === 'fwd') restoreTunnelsOnPrimary();
      return;
    }
    const delay = SECONDARY_RETRY_DELAYS[retryRef.current];
    retryRef.current += 1;
    setStatus('connecting');
    if (timerRef.current) clearTimeout(timerRef.current);
    timerRef.current = setTimeout(() => {
      timerRef.current = null;
      if (!enabledRef.current || statusRef.current !== 'connected') return;
      if (which === 'sftp') openSftpConn(); else openFwdConn();
    }, delay);
  };

  // Open whichever dedicated connections are enabled — called from the primary
  // `connection-success` handler (first-connect AND reconnect; the backend
  // sweeps stale secondaries at the top of the primary's connect, so this is
  // clean). On `::fwd` success the backend migrates the session's tunnels onto
  // the fresh transport.
  const openSecondaryConnections = () => {
    if (separateSftpRef.current) openSftpConn();
    if (separateFwdRef.current) openFwdConn();
  };

  // Reset secondary tracking when the primary drops / reconnects. The backend
  // has already torn the secondaries down (primary connect-teardown, or the
  // primary disconnect sweep); this just resets the indicators + retry state.
  const resetSecondaryStatus = () => {
    clearSecondaryRetryTimers();
    sftpRetryRef.current = 0;
    fwdRetryRef.current = 0;
    setSftpConnStatus('off');
    setFwdConnStatus('off');
  };

  // Manual reconnect buttons in the tab headers. initiate_connection sweeps
  // its own key's stale state, so a plain re-open is a full clean reconnect
  // of just the dedicated connection — never the whole server session.
  const reconnectSftpConn = () => {
    if (statusRef.current !== 'connected') return;
    sftpRetryRef.current = 0;
    openSftpConn();
  };
  const reconnectFwdConn = () => {
    if (statusRef.current !== 'connected') return;
    fwdRetryRef.current = 0;
    openFwdConn();
  };

  // Tab-header toggles. Persist immediately; apply live if the session is
  // already connected, otherwise they take effect on the next connect.
  const toggleSeparateSftp = (on: boolean) => {
    setSeparateSftp(on);
    // Sync ref update: the disconnect below emits session-disconnected-…::sftp,
    // and its listener consults this ref — the useEffect mirror only lands
    // after the next render, which can be AFTER the event. Without this, a
    // toggle-off could read a stale "on" and auto-retry the connection the
    // user just closed.
    separateSftpRef.current = on;
    localStorage.setItem('submarine-separate-sftp', String(on));
    sftpRetryRef.current = 0;
    if (statusRef.current !== 'connected') return;
    if (on) {
      if (sftpConnStatusRef.current !== 'ready' && sftpConnStatusRef.current !== 'connecting') openSftpConn();
    } else {
      setSftpConnStatus('off');
      // Backend drops the base SFTP cache with the transport, so the next
      // file operation transparently re-opens on the primary.
      invoke("disconnect_session", { sessionId: `${session.id}::sftp` }).catch(console.error);
    }
  };
  const toggleSeparateFwd = (on: boolean) => {
    setSeparateFwd(on);
    // Sync ref update — same stale-ref race as toggleSeparateSftp.
    separateFwdRef.current = on;
    localStorage.setItem('submarine-separate-fwd', String(on));
    fwdRetryRef.current = 0;
    if (statusRef.current !== 'connected') return;
    if (on) {
      if (fwdConnStatusRef.current !== 'ready' && fwdConnStatusRef.current !== 'connecting') openFwdConn();
    } else {
      setFwdConnStatus('off');
      // Order matters: the disconnect stops tunnels riding the dedicated
      // transport AND removes it from the connections map; only then does
      // the restart re-resolve the transport — now the primary — and bring
      // the same tunnels back up there.
      invoke("disconnect_session", { sessionId: `${session.id}::fwd` })
        .catch(console.error)
        .finally(() => { restoreTunnelsOnPrimary(); });
    }
  };

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
  // Bubble our terminals + active-tab up to the parent (App) whenever
  // they change so the Wall pinboard and any other App-level consumers
  // don't have to duplicate the per-session terminal book-keeping.
  // Attach-only Wall tiles use these to know which (sessionId, terminalId)
  // pairs exist and which one is currently the session's own primary.
  useEffect(() => {
    if (typeof onTerminalsChange === "function") {
      onTerminalsChange(session.id, terminals, activeTab);
    }
  }, [session?.id, terminals, activeTab, onTerminalsChange]);
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
  // Guards the per-node "run on connect" commands to fire exactly once for
  // this session view — first successful connect only, never on reconnect.
  const ranOnConnectRef = useRef(false);

  // ---- Auto-reconnect: fixed 5s cadence, retried FOREVER -------------------
  // After a previously-good session drops we retry every 5 seconds, forever —
  // no backoff ramp, no give-up. The user wants the session/tunnel to keep
  // trying relentlessly on any drop (laptop sleep, mobile-hotspot dropout, a
  // flaky link) so a forward always comes back by itself once the link
  // returns. Only a genuine AUTH failure (wrong credentials) or an explicit
  // Cancel stops the cycle — retrying auth-rejected attempts every 5s would
  // just spam the server and risk locking the account. The terminal/SFTP
  // overlay stays in place the whole time so the user keeps their context.
  const RECONNECT_INTERVAL_MS = 5000;
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
    primarySeparateRef.current = separateFwdRef.current;
    invoke("initiate_connection", { sessionId: session.id, serverId: session.serverId, customPassword: customPasswordRef.current || null, quickAuth: session.quickAuth || null, sessionRole: "primary", separateSessions: separateFwdRef.current })
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

    const unlistenKbi = listen(`kbi-prompt-${session.id}`, (event: any) => {
      const p = event.payload;
      setKbiPrompt(p);
      setKbiValues(Array.isArray(p?.prompts) ? p.prompts.map(() => "") : []);
    });

    const unlistenKbiDismiss = listen(`kbi-prompt-dismiss-${session.id}`, () => {
      setKbiPrompt(null);
      setKbiValues([]);
    });

    // ---- Dedicated secondary connections (separate-sessions mode) ----------
    // These fire only when the setting is on and we opened `::sftp` / `::fwd`.
    // They drive the status dots + auto-retry; routing itself lives in the
    // backend (transport preference), so no keys flip here. The primary
    // session's own KBI / fingerprint prompts stay wired above.
    const unlistenSftpOk = listen(`connection-success-${session.id}::sftp`, () => {
      sftpRetryRef.current = 0;
      setSftpConnStatus('ready');
    });
    const unlistenSftpFail = listen(`connection-failed-${session.id}::sftp`, (event: any) => {
      // Auth-style failures (2FA-only server, bad credentials) won't get
      // better on retry — go straight to failed; SFTP rides the primary.
      if (event.payload?.is_auth_error) setSftpConnStatus('failed');
      else if (separateSftpRef.current && statusRef.current === 'connected') scheduleSecondaryRetry('sftp');
      else setSftpConnStatus('off');
    });
    const unlistenSftpDown = listen(`session-disconnected-${session.id}::sftp`, (event: any) => {
      // Unexpected transport death (the backend already dropped the base SFTP
      // cache, so file ops already fall back to the primary) — try to bring
      // the dedicated connection back. A user-initiated disconnect (toggle
      // off) must never retry.
      const userInitiated = !!event.payload?.user_initiated;
      if (!userInitiated && separateSftpRef.current && statusRef.current === 'connected') scheduleSecondaryRetry('sftp');
      else setSftpConnStatus('off');
    });

    const unlistenFwdOk = listen(`connection-success-${session.id}::fwd`, () => {
      fwdRetryRef.current = 0;
      setFwdConnStatus('ready');
      // Tunnel migration onto the fresh transport happens backend-side.
    });
    const unlistenFwdFail = listen(`connection-failed-${session.id}::fwd`, (event: any) => {
      if (event.payload?.is_auth_error) {
        // Won't get better on retry — run the tunnels on the primary instead.
        setFwdConnStatus('failed');
        invoke("restart_session_tunnels", { sessionId: session.id, serverId: session.serverId }).catch(console.error);
      } else if (separateFwdRef.current && statusRef.current === 'connected') {
        // scheduleSecondaryRetry falls back to the primary (and restores the
        // tunnels there) once the retry budget is exhausted.
        scheduleSecondaryRetry('fwd');
      } else {
        setFwdConnStatus('off');
      }
    });
    const unlistenFwdDown = listen(`session-disconnected-${session.id}::fwd`, (event: any) => {
      // The watcher already stopped the tunnels that rode this transport.
      // Retry brings the transport back (backend migration restarts them on
      // it); exhausted retries restore them on the primary. A user-initiated
      // disconnect (toggle off — which restores tunnels itself) never retries.
      const userInitiated = !!event.payload?.user_initiated;
      if (!userInitiated && separateFwdRef.current && statusRef.current === 'connected') scheduleSecondaryRetry('fwd');
      else setFwdConnStatus('off');
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
      } else if (session.runOnConnect && session.runOnConnect.trim() && !ranOnConnectRef.current) {
        // Per-node on-connect commands: auto-typed ONCE into the first
        // terminal on the INITIAL connect only (the `else` gates out
        // reconnects; ranOnConnectRef gates out any duplicate success
        // event). Delayed so the PTY is open and the shell has printed its
        // prompt — the commands then land after it instead of racing the
        // shell's startup and getting swallowed.
        ranOnConnectRef.current = true;
        const cmds = session.runOnConnect
          .split(/\r?\n/)
          .map((l: string) => l.trimEnd())
          .filter((l: string) => l.trim().length > 0)
          .join('\n') + '\n';
        const bytes = Array.from(new TextEncoder().encode(cmds));
        setTimeout(() => {
          invoke('write_terminal_data', { terminalId: `${session.id}-term-0`, data: bytes }).catch(() => {});
        }, 700);
      }
      // The handshake may have inserted a row into `known_hosts` (user just
      // accepted a new fingerprint). Flush the encrypted vault so the entry
      // survives an app restart — otherwise the prompt would reappear every
      // session, which was especially painful for SOCKS-proxied connections.
      invoke("persist_vault").catch(console.error);
      // Separate-sessions: now that the primary is up (and the host key is
      // saved, so no double fingerprint prompt), spin up the dedicated SFTP /
      // forwarding connections. No-op when the setting is off. Fires on
      // reconnect too — the backend already reaped the stale secondaries.
      resetSecondaryStatus();
      openSecondaryConnections();
      // Reconcile the toggled-off-during-handshake case: the primary deferred
      // its saved tunnels for a `::fwd` connection we are NOT going to open, so
      // nobody would start them — restore on the primary.
      if (primarySeparateRef.current && !separateFwdRef.current) {
        restoreTunnelsOnPrimary();
      }
    });

    const unlistenFailed = listen(`connection-failed-${session.id}`, (event: any) => {
      pushLog({ msg: `Connection failed: ${event.payload?.reason}`, type: 'error' });
      const isAuth = !!event.payload?.is_auth_error;
      setStatus('failed');
      setIsAuthError(isAuth);
      resetSecondaryStatus();
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
      resetSecondaryStatus();
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
      unlistenKbi.then(f => f());
      unlistenKbiDismiss.then(f => f());
      unlistenSftpOk.then(f => f());
      unlistenSftpFail.then(f => f());
      unlistenSftpDown.then(f => f());
      unlistenFwdOk.then(f => f());
      unlistenFwdFail.then(f => f());
      unlistenFwdDown.then(f => f());
      unlistenSuccess.then(f => f());
      unlistenFailed.then(f => f());
      unlistenDisconnected.then(f => f());

      if (reconnectTimerRef.current) clearTimeout(reconnectTimerRef.current);
      clearSecondaryRetryTimers();
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
    const delay = RECONNECT_INTERVAL_MS;
    updateAttempt(attempt);
    setNextReconnectAt(Date.now() + delay);
    reconnectTimerRef.current = setTimeout(() => {
      setNextReconnectAt(null);
      setStatus('connecting');
      primarySeparateRef.current = separateFwdRef.current;
      invoke("initiate_connection", {
        sessionId: session.id,
        serverId: session.serverId,
        customPassword: customPasswordRef.current || null,
        quickAuth: session.quickAuth || null,
        sessionRole: "primary",
        separateSessions: separateFwdRef.current,
      }).catch(console.error);
    }, delay);
  };

  const reconnect = () => {
    cancelReconnect();
    setStatus('connecting');
    setLogs([]);
    setIsAuthError(false);
    setDisconnectReason("");
    resetSecondaryStatus();
    primarySeparateRef.current = separateFwdRef.current;
    invoke("initiate_connection", {
      sessionId: session.id,
      serverId: session.serverId,
      customPassword: customPassword || null,
      quickAuth: session.quickAuth || null,
      sessionRole: "primary",
      separateSessions: separateFwdRef.current,
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

  // Keyboard-interactive (2FA) submit / cancel. Like the fingerprint flow the
  // nonce binds the answer to this exact connect attempt; cancel sends `null`
  // so the backend aborts the interactive auth instead of waiting out its
  // 120s timeout.
  const handleKbiSubmit = async () => {
    const nonce = kbiPrompt?.nonce;
    const responses = kbiValues;
    setKbiPrompt(null);
    setKbiValues([]);
    if (!nonce) return;
    try {
      await invoke("submit_kbi_response", { nonce, responses });
    } catch (e) {
      console.error(e);
    }
  };

  const handleKbiCancel = async () => {
    const nonce = kbiPrompt?.nonce;
    setKbiPrompt(null);
    setKbiValues([]);
    if (!nonce) return;
    try {
      await invoke("submit_kbi_response", { nonce, responses: null });
    } catch (e) {
      console.error(e);
    }
  };

  // Host-key fingerprint + keyboard-interactive (2FA) prompts, held in ONE
  // place so they render both in the first-connect full-screen view AND — via
  // a portal overlay (see the connected-state return) — during an auto-
  // reconnect. Previously these only lived inside the reconnectAttempt===0
  // early-return, so a changed-host-key or 2FA prompt fired mid-reconnect was
  // never drawn (the slim banner has no prompt UI), leaving the user unable to
  // respond and the reconnect cycle spinning.
  const authPrompts = (
    <>
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

      {/* Keyboard-Interactive (2FA / verification-code) prompt. The
          server drives the wording via `name` / `instructions` / each
          prompt's label; we render one input per prompt (masked unless
          the server set echo=true, e.g. a plain username). Enter on the
          last field submits. */}
      {kbiPrompt && (
        <div className="mt-6 p-4 border border-primary/30 bg-primary/5 rounded-xl animate-in fade-in slide-in-from-bottom-4">
          <div className="flex items-start gap-3">
            <KeyRound className="text-primary mt-1" size={20} />
            <div className="flex-1 min-w-0">
              <h3 className="text-sm font-bold text-primary uppercase tracking-widest">
                {kbiPrompt.name && String(kbiPrompt.name).trim() ? kbiPrompt.name : "Verification required"}
              </h3>
              {kbiPrompt.instructions && String(kbiPrompt.instructions).trim() && (
                <p className="text-zinc-400 mt-2 leading-relaxed whitespace-pre-wrap break-words">
                  {kbiPrompt.instructions}
                </p>
              )}
              <div className="mt-3 space-y-3">
                {(kbiPrompt.prompts || []).map((p: any, i: number) => (
                  <div key={i} className="space-y-1.5">
                    <label className="block text-[12px] text-zinc-300 break-words">
                      {p?.prompt || "Response"}
                    </label>
                    <input
                      type={p?.echo ? "text" : "password"}
                      autoFocus={i === 0}
                      className="w-full h-9 bg-[#1a1a1e] rounded-lg px-3 text-sm text-white border border-white/10 outline-none focus:border-primary/50 focus:bg-[#232328] transition-all"
                      value={kbiValues[i] ?? ""}
                      onChange={e => setKbiValues(vals => {
                        const next = [...vals];
                        next[i] = e.target.value;
                        return next;
                      })}
                      onKeyDown={e => {
                        if (e.key === "Enter" && i === (kbiPrompt.prompts?.length ?? 1) - 1) handleKbiSubmit();
                      }}
                    />
                  </div>
                ))}
              </div>
              <div className="flex gap-3 mt-4">
                <button
                  onClick={handleKbiSubmit}
                  className="px-6 py-2 bg-primary text-black font-bold text-xs uppercase tracking-wider rounded-lg hover:bg-primary/80 transition-colors flex items-center gap-2"
                >
                  <Check size={14} /> Submit
                </button>
                <button
                  onClick={handleKbiCancel}
                  className="px-6 py-2 bg-white/5 text-zinc-300 font-bold text-xs uppercase tracking-wider rounded-lg hover:bg-white/10 transition-colors flex items-center gap-2"
                >
                  <X size={14} /> Cancel
                </button>
              </div>
            </div>
          </div>
        </div>
      )}
    </>
  );

  // Only render the full-screen log view for the FIRST connection — once an
  // auto-reconnect cycle is running, the user's terminal output and SFTP
  // state stay visible behind a slim banner.
  if (reconnectAttempt === 0 && (status === 'connecting' || status === 'failed')) {
    return (
      <div className="flex-1 flex flex-col p-4 sm:p-8 bg-[#0a0a0c] text-white overflow-hidden">
        <div className="max-w-2xl w-full mx-auto flex-1 flex flex-col min-h-0">
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

          {/* Log Window. `min-h-0` is what makes it actually scroll: a flex
              child defaults to min-height:auto (its content height), so without
              this the box grows to fit every line and the parent's
              overflow-hidden clips it instead of the inner overflow-y-auto
              taking over — invisible on desktop with a tall window, but on a
              short Android viewport the log gets cut off with no way to scroll.
              overscroll-contain keeps a flick from scrolling the page behind it;
              WebkitOverflowScrolling gives older Android WebViews momentum. */}
          <div
            className="flex-1 min-h-0 bg-[#121214] border border-white/5 rounded-2xl p-4 font-mono overflow-y-auto overscroll-contain custom-scrollbar shadow-inner relative select-text cursor-text"
            style={{ WebkitOverflowScrolling: 'touch', fontSize: logFontSize, lineHeight: 1.5 }}
          >
            {logs.map((l, i) => (
              <div key={i} className={`mb-2 ${l.type === 'error' ? 'text-red-400' : l.type === 'success' ? 'text-primary' : 'text-zinc-400'}`}>
                <span className="text-zinc-600 opacity-50 mr-3">[{l.time}]</span>
                {l.msg}
              </div>
            ))}

            {authPrompts}
          </div>
        </div>
      </div>
    );
  }

  // Connected State with Nested Tabs
  return (
    <div className="flex-1 flex flex-col bg-background overflow-hidden animate-in fade-in">
      {/* Auth prompts during an AUTO-RECONNECT. The full-screen view above only
          renders on the first connect (reconnectAttempt===0); once a reconnect
          cycle is running we show the terminal behind a slim banner, so a
          fingerprint / 2FA prompt fired mid-reconnect would otherwise be
          invisible and unanswerable. Float it over everything via a portal. */}
      {(fingerprintPrompt || kbiPrompt) && createPortal(
        <div className="fixed inset-0 z-[200] flex items-start justify-center bg-black/70 backdrop-blur-sm p-4 sm:p-8 overflow-y-auto">
          <div className="max-w-2xl w-full mt-6 sm:mt-12">
            {authPrompts}
          </div>
        </div>,
        document.body
      )}
      {/* Nested Tab Bar — hidden in `chromeless` mode. Chromeless is
          used by the App-level Split-view tiling: merged (non-focused)
          panes show only the active terminal, no per-session tab strip
          + tool rail. Otherwise every merged tile would carry its own
          full chrome and 2-3 sessions side-by-side would fight for
          vertical space with duplicate toolbars. */}
      {!chromeless && (
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
              className={`h-8 px-3 sm:px-4 ${terminals.length > 1 ? 'pr-8' : ''} rounded-lg flex items-center gap-2 text-[11px] font-bold uppercase tracking-wider transition-all ${
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

        {/* Tool rail: icon + text label. The label collapses to icon-only on
            narrow viewports (below md) so the terminal tab strip keeps its
            horizontal real estate when space is tight; the full description
            stays in the tooltip either way. */}
        <div className="flex items-center gap-1 shrink-0 sm:border-l border-white/5 sm:pl-3 pl-1">
          {([
            { id: 'sftp',    icon: Folder,  label: 'SFTP',    hint: 'SFTP — file browser' },
            { id: 'tunnels', icon: Network, label: 'Ports',   hint: 'Ports — port forwarding' },
            { id: 'cmds',    icon: Library, label: 'Library', hint: 'Library — commands & notes' },
            { id: 'info',    icon: Info,    label: 'Info',    hint: 'Info — server overview' },
          ] as const).map(({ id, icon: Icon, label, hint }) => {
            const on = activeTool === id;
            return (
              <button
                key={id}
                onClick={() => setActiveTool(on ? null : id)}
                title={hint}
                aria-label={hint}
                aria-pressed={on}
                className={`h-10 sm:h-8 px-2.5 rounded-lg flex items-center gap-1.5 transition-all ${
                  on
                    ? 'bg-primary/10 text-primary border border-primary/20 shadow-inner'
                    : 'text-zinc-300 bg-white/[0.04] border border-white/10 hover:bg-white/[0.08] hover:border-white/20 hover:text-white'
                }`}
              >
                <Icon size={14} className="shrink-0" />
                <span className="hidden md:inline text-[11px] font-semibold tracking-tight">{label}</span>
              </button>
            );
          })}
        </div>
      </div>
      )}

      {/* "+" secondary menu — right-click / long-press on the plus
          button. Kept intentionally short: same-server "split with new
          pane", the h/v orientation flip, and un-split. Cross-server
          split lives on the session tab strip (right-click any tab
          → the picker); one entry point per concept keeps the menu
          scannable. Everything else is one primary click on "+". */}
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
              </>
            )}
            {/* Splitting THIS session's view with another server's tab
                lives on the session tab strip (right-click the tab).
                That way the picker is anchored to the tab it operates
                on and users don't have to know two entry points for the
                same feature. */}
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
              <div className="h-10 px-4 flex items-center justify-between gap-3 border-b border-white/5 bg-white/5">
                <div className="flex items-center gap-3 min-w-0">
                  <span className="text-[10px] font-bold uppercase tracking-wider text-zinc-400 shrink-0">SFTP File Browser</span>
                  <SepToggle
                    on={separateSftp}
                    onToggle={toggleSeparateSftp}
                    status={!separateSftp ? 'off' : sftpConnStatus === 'ready' ? 'ready' : sftpConnStatus === 'failed' ? 'failed' : 'pending'}
                    title="Run SFTP over its own dedicated SSH connection instead of sharing the terminal's session"
                    onReconnect={reconnectSftpConn}
                  />
                </div>
                <button onClick={() => setActiveTool(null)} className="text-zinc-500 hover:text-white transition-colors shrink-0">
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
              <div className="h-10 px-4 flex items-center justify-between gap-3 border-b border-white/5 bg-white/5">
                <div className="flex items-center gap-3 min-w-0">
                  <span className="text-[10px] font-bold uppercase tracking-wider text-zinc-400 shrink-0">Port Forwarding</span>
                  <SepToggle
                    on={separateFwd}
                    onToggle={toggleSeparateFwd}
                    status={!separateFwd ? 'off' : fwdConnStatus === 'ready' ? 'ready' : fwdConnStatus === 'failed' ? 'failed' : 'pending'}
                    title="Run port-forwarding over its own dedicated SSH connection instead of sharing the terminal's session"
                    onReconnect={reconnectFwdConn}
                  />
                </div>
                <button onClick={() => setActiveTool(null)} className="text-zinc-500 hover:text-white transition-colors shrink-0">
                  <X size={14} />
                </button>
              </div>
              <div className="flex-1 overflow-hidden relative">
                <TunnelsPanel sessionId={session.id} serverId={session.serverId} disabled={status !== 'connected'} />
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
              visible={activeTool === 'info'}
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
