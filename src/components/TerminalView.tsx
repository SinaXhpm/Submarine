import { useEffect, useRef, useState } from 'react';
import { Terminal } from 'xterm';
import { FitAddon } from 'xterm-addon-fit';
import { SearchAddon, ISearchOptions } from 'xterm-addon-search';
import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import 'xterm/css/xterm.css';
import { KeyRound } from 'lucide-react';
import { useIsNarrow } from '../hooks/useViewport';
import { MobileKeyBar, ModifiersState, ModKey, SpecialKey } from './MobileKeyBar';
import { useBroadcast } from '../ui/broadcast';
import HistorySearchOverlay from './HistorySearchOverlay';
import { DEFAULT_FONT_STACK, fontFamilyCss, readFontFamily, readFontSize } from '../util/terminalFont';
import { readScrollback } from '../util/terminalScrollback';
import { withoutPasteControls } from '../util/pasteText';
import { READABLE_CONTRAST, readBoldBright, readMinContrast, readScheme } from '../util/terminalTheme';
import { readUseLocalShell } from '../util/terminalShell';
import { readUseSshAgent } from '../util/sshAgent';
import { IS_ANDROID, IS_LINUX } from '../util/platform';
import { useConfirm } from '../ui/confirm';

// Does the recent remote OUTPUT look like a no-echo password / passphrase
// prompt? Used to skip command-history capture so typed secrets (sudo, su,
// mysql -p, ssh/gpg passphrase) aren't persisted into searchable history. At a
// no-echo prompt the output still ENDS with the prompt text because the user's
// keystrokes were never echoed back. We strip ANSI colour/OSC sequences first
// and only match when the prompt is the trailing content.
const looksLikePasswordPrompt = (out: string): boolean => {
  const clean = out
    .replace(/\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)/g, "") // OSC …BEL/ST
    .replace(/\x1b\[[0-9;?]*[ -/]*[@-~]/g, "")          // CSI …final
    .replace(/[\r\n]+/g, "\n");
  const tail = clean.slice(-160);
  return /(?:password|passphrase)\b[^:\n]*:\s*$/i.test(tail);
};

// Shared search options — highlight all matches in yellow, the active
// match in orange. `decorations` uses the marker overlay so the highlight
// survives scrollback and repaints (the addon's built-in selection-based
// mode would clobber the user's own selection every keystroke). The
// overview-ruler fields are typed as required in xterm-addon-search 0.13
// but we don't render an overview ruler (no `overviewRulerLane` on the
// terminal), so their values are cosmetic — mirror the match colors so
// they'd still read sensibly if a ruler were ever enabled.
const SEARCH_OPTIONS: ISearchOptions = {
  caseSensitive: false,
  decorations: {
    matchBackground: '#facc15',
    matchBorder: '#facc15',
    matchOverviewRuler: '#facc15',
    activeMatchBackground: '#f97316',
    activeMatchBorder: '#f97316',
    activeMatchColorOverviewRuler: '#f97316',
  },
};

// State machine for the modifier stickies on the mobile key bar.
// off → armed → locked → off (cycled by repeated taps).
const cycleMod = (s: "off" | "armed" | "locked"): "off" | "armed" | "locked" =>
  s === "off" ? "armed" : s === "armed" ? "locked" : "off";

// A terminal can be shown in two places, its session tab and a Wall tile, but
// its PTY has one size (#174). A Wall tile only exists while the Wall is on
// screen, so while one is open it sets the size; the session tab, hidden
// behind it, still refits but sends nothing. When the tile goes away the tab
// gives the PTY its own size back. ptySizes remembers what each PTY was last
// given, so the same size isn't sent twice.
const ptySizes = new Map<string, { cols: number; rows: number }>();
const wallTiles = new Map<string, number>();
const sendPtySize = (terminalId: string, cols: number, rows: number) => {
  const last = ptySizes.get(terminalId);
  if (last && last.cols === cols && last.rows === rows) return;
  ptySizes.set(terminalId, { cols, rows });
  invoke('resize_terminal', { terminalId, cols, rows }).catch(() => {});
};
// Once the PTY is (re)opened: the backend drops resizes while a shell is
// starting, so the size is sent again, the open Wall tile's if there is one.
const resendPtySize = (terminalId: string, cols: number, rows: number) => {
  const size = (wallTiles.get(terminalId) ?? 0) > 0 ? ptySizes.get(terminalId) ?? { cols, rows } : { cols, rows };
  ptySizes.delete(terminalId);
  sendPtySize(terminalId, size.cols, size.rows);
};
const WALL_TILE_CLOSED = 'submarine-wall-tile-closed';
// How long a terminal's size must hold before its PTY gets it. A layout
// change passes through sizes for a few frames: the narrow and the wide
// layout swap at 640 px (the terminal is 2 columns wide for a frame) and
// panels animate their width. The shell redraws its prompt for every size
// it is given, so each of those left the screen garbled and the cursor out
// of place.
const PTY_SIZE_SETTLE_MS = 150;

const TerminalView = ({
  sessionId,
  terminalId,
  disabled = false,
  isActive = true,
  isActiveView = false,
  focusSignal = 0,
  containerExec,
  connectionEpoch = 0,
  serverId = 0,
  serverName = "",
  attachOnly = false,
  telnet = false,
  ownLogin = false,
  quickAuth = null,
}: {
  sessionId: string;
  terminalId: string;
  disabled?: boolean;
  /// Attach-only mode: the PTY behind this terminal is owned by
  /// another TerminalView instance (typically the one inside the
  /// session tab). This instance ONLY listens to output events and
  /// forwards input — it doesn't call open_terminal on mount,
  /// close_terminal on unmount, or fight over resize_terminal. Used
  /// by the Wall pinboard so pinning a terminal doesn't duplicate the
  /// PTY or tear it down when Wall is closed.
  attachOnly?: boolean;
  /// Tells us when this terminal is the visible one in its parent. Used to
  /// trigger a refit + refresh whenever we become visible — without this,
  /// xterm's internal canvas can be left holding stale glyphs from before
  /// the parent's display/opacity change and the prompt appears garbled
  /// until the user types something.
  isActive?: boolean;
  /// True only when this terminal's SessionView is the frontmost server
  /// tab (DesktopApp's active view). Auto-focus is gated on this so a
  /// background session — or an attach-only Wall mirror — never steals the
  /// keyboard while the user is typing in the visible one.
  isActiveView?: boolean;
  /// Monotonic counter the parent SessionView bumps to pull keyboard focus
  /// into the active terminal for selections that DON'T change which
  /// terminal is active (re-clicking the current tab, re-selecting this
  /// server tab, or clicking a tab while focus sat in a tool pane). A
  /// change — not the value — triggers the focus; only the active terminal
  /// reacts.
  focusSignal?: number;
  /// When set, this terminal runs `docker exec -it <container> <shell>`
  /// on the SSH host instead of the user's login shell. Used by the
  /// Docker tab in InfoPanel to open an interactive session inside a
  /// specific container without leaving the app.
  containerExec?: { container: string; useSudo: boolean };
  /// A telnet server's terminal (#179): it opens the connection its tab
  /// made (`telnet_open_terminal`) instead of a shell over SSH. The rest —
  /// keys, sizes, the saved-password button — works the same.
  telnet?: boolean;
  /// Bumped by the parent SessionView on every successful reconnect.
  /// When the value changes we re-open a fresh PTY against the new SSH
  /// handle WITHOUT disposing xterm — the user keeps their scroll-back
  /// from before the drop and can copy/paste anything they did pre-drop.
  /// Initial mount is opened by the main effect, so we only act on
  /// value CHANGES after that.
  connectionEpoch?: number;
  /// Server id + name are used to stamp command-history rows so the
  /// Ctrl+R overlay can show which server the command was run against.
  /// Quick-connect sessions have serverId=0 and just record the display
  /// name — that's fine for search, since the row is orphaned from any
  /// saved node anyway.
  serverId?: number;
  serverName?: string;
  /// This terminal logs in on a connection of its own, as a new connection
  /// to the server would, instead of opening a channel on its tab's (issue
  /// #230): for servers that tie what a user may do to the login, such as
  /// CA Access Control's `sesu`. When that login fails it uses the tab's
  /// connection and says so.
  ownLogin?: boolean;
  /// A quick connect's login (it has no saved server), for `ownLogin`.
  quickAuth?: unknown;
}) => {
  // The connection this terminal's shell runs on: its tab's, or its own
  // (`ownLogin`), `<tab>::term-<terminal>`, which the backend takes as the
  // tab's for what was typed there (a password asked for, a host key).
  const ownConn = `${sessionId}::term-${terminalId}`;
  const shellConnRef = useRef(sessionId);
  // Ends the wait of this terminal's own login while it is in progress.
  const cancelOwnRef = useRef<(() => void) | null>(null);
  // Logs in on this terminal's own connection; resolves to the session to
  // open the shell on: its own, or the tab's when that login failed. Null when
  // it was cancelled (a newer login started, or the terminal or its tab went
  // away): then there is nothing to open.
  const connectOwn = async (term: Terminal): Promise<string | null> => {
    // One login at a time: a newer one (a reconnect) replaces one still in
    // progress, and the backend drops that one's connection when it starts.
    cancelOwnRef.current?.();
    let cancelled = false;
    let settle: (r: true | string | null) => void = () => {};
    const done = new Promise<true | string | null>((resolve) => { settle = resolve; });
    const cancel = () => { cancelled = true; settle(null); };
    cancelOwnRef.current = cancel;
    const offs = await Promise.all([
      listen(`connection-success-${ownConn}`, () => settle(true)),
      listen(`connection-failed-${ownConn}`, (e: any) => settle(String(e.payload?.reason ?? 'the login failed'))),
    ]);
    const timer = setTimeout(() => settle('no answer within 60 seconds'), 60000);
    if (!cancelled) {
      invoke('initiate_connection', {
        sessionId: ownConn,
        serverId,
        customPassword: null,
        quickAuth: quickAuth ?? null,
        sessionRole: 'terminal',
        separateSessions: false,
        useSshAgent: readUseSshAgent(),
      }).catch((e) => settle(String(e)));
    }
    const result = await done;
    clearTimeout(timer);
    offs.forEach((off) => off());
    if (cancelOwnRef.current === cancel) cancelOwnRef.current = null;
    if (result === null) return null;
    if (result === true) return ownConn;
    term.writeln(`\x1b[33mThis terminal couldn't log in on its own (${result}); it uses the tab's connection.\x1b[0m`);
    invoke('disconnect_session', { sessionId: ownConn }).catch(() => {});
    return sessionId;
  };
  // Ends this terminal's own login: one in progress is cancelled (the backend
  // drops its connection if it still comes up), one that's up is closed.
  const closeOwn = () => {
    const pending = cancelOwnRef.current;
    cancelOwnRef.current = null;
    pending?.();
    if (!pending && shellConnRef.current === sessionId) return;
    invoke('disconnect_session', { sessionId: ownConn }).catch(() => {});
    shellConnRef.current = sessionId;
  };
  const terminalRef = useRef<HTMLDivElement>(null);
  const xtermRef = useRef<Terminal | null>(null);
  const fitAddonRef = useRef<FitAddon | null>(null);
  const searchAddonRef = useRef<SearchAddon | null>(null);
  const searchInputRef = useRef<HTMLInputElement | null>(null);
  const openedRef = useRef(false);
  // The xterm `onData` callback is bound once and persists for the life of the
  // component. We read this ref inside the callback so the latest `disabled`
  // state is observed without having to tear down and rebuild xterm.
  const disabledRef = useRef(disabled);
  useEffect(() => {
    disabledRef.current = disabled;
  }, [disabled]);
  // Mirror of `isActive` readable inside the ResizeObserver callback. The
  // observer is bound once at mount and runs for the lifetime of the
  // terminal, so a stale closure over the initial `isActive` would block
  // the gate from ever flipping. A ref is the cheapest way to expose live
  // state to that callback.
  const isActiveRef = useRef(isActive);
  useEffect(() => {
    isActiveRef.current = isActive;
  }, [isActive]);
  // Mirror of isActiveView, read inside focusTerminal's rAF / signal
  // callbacks (which would otherwise close over a stale value).
  const isActiveViewRef = useRef(isActiveView);
  useEffect(() => { isActiveViewRef.current = isActiveView; }, [isActiveView]);
  // Mirror of containerExec for the reconnect effect — same reason as
  // isActiveRef. The reconnect effect is keyed on `connectionEpoch`
  // alone (the parent re-creates the containerExec object every render
  // so including it in deps would re-fire every paint).
  const containerExecRef = useRef(containerExec);
  useEffect(() => { containerExecRef.current = containerExec; }, [containerExec]);
  // Last connectionEpoch we acted on. Initial mount's PTY is opened by
  // the main effect, so we should NOT re-open on the first run of the
  // reconnect effect — only on subsequent value changes.
  const lastEpochRef = useRef(connectionEpoch);

  // Mobile-only sticky modifiers (Ctrl / Alt / Shift) for the on-screen
  // key bar. State doubles as render input (highlighting) and is mirrored
  // into a ref so the long-lived `term.onData` callback can read it without
  // re-binding every state change. Armed modifiers consume themselves on
  // the next typed char; locked modifiers persist until the user taps the
  // chip a third time.
  const isMobile = useIsNarrow();
  const [modifiers, setModifiers] = useState<ModifiersState>({ ctrl: "off", alt: "off", shift: "off" });
  const modifiersRef = useRef<ModifiersState>(modifiers);
  useEffect(() => { modifiersRef.current = modifiers; }, [modifiers]);

  // In-terminal search (Ctrl+F / Cmd+F). Query state is component-scoped so
  // reopening the chip pre-fills the last search — matches VS Code / Chrome
  // muscle memory. showSearch drives the floating chip's mount so xterm
  // isn't burdened with a hidden overlay when the user isn't searching.
  const [showSearch, setShowSearch] = useState(false);
  const [query, setQuery] = useState('');

  // Command-history overlay (Ctrl+R / Cmd+R). Same document-level keydown
  // gate as search: only pop it when the user is focused inside this
  // terminal's DOM subtree, so multi-tab windows don't fire N overlays.
  const [showHistory, setShowHistory] = useState(false);

  // Broadcast context. `stateRef` gives us live access from inside the
  // xterm.onData closure without re-binding on every state change.
  // Callback identities on `broadcast` are stable (useCallback) so the
  // register effect below can depend on those directly without churn.
  const broadcast = useBroadcast();
  const broadcastRef = broadcast.stateRef;
  const registerActiveTerminal = broadcast.registerActiveTerminal;
  const unregisterActiveTerminal = broadcast.unregisterActiveTerminal;

  // Register this terminal as the "active" one for its session whenever
  // isActive flips true. Broadcast fan-out reads from sessionTerminalMap
  // to know which of a session's tabs is currently visible / receiving
  // input; a stale mapping to a background terminal would silently swallow
  // the mirrored keystrokes. Unregister only if we still hold the slot —
  // avoids racing with the newly-active terminal during a tab swap.
  useEffect(() => {
    if (!isActive) return;
    registerActiveTerminal(sessionId, terminalId);
    return () => {
      unregisterActiveTerminal(sessionId, terminalId);
    };
  }, [isActive, sessionId, terminalId, registerActiveTerminal, unregisterActiveTerminal]);

  // Buffer keystrokes locally so we can log "the command line the user
  // typed" on each Enter press. Best-effort: pastes, arrow-key edits,
  // multi-line entries will produce noisy or wrong rows — we accept that
  // rather than reimplement a shell parser in the browser. `commandBufRef`
  // is mutated inside the long-lived onData closure so a ref (not state)
  // is the right container.
  const commandBufRef = useRef<string>("");
  // Pasted text isn't recorded as history (see onData). `pastingRef` is set
  // while our right-click paste runs — term.paste() fires onData
  // synchronously — and `commandBufPastedRef` marks a command line that still
  // holds pasted text, so its Enter is skipped too.
  const pastingRef = useRef(false);
  const commandBufPastedRef = useRef(false);
  // The app's confirm dialog, for the terminal's long-lived handlers (links,
  // multi-line paste), which would otherwise see a stale function.
  const confirm = useConfirm();
  const confirmRef = useRef(confirm);
  confirmRef.current = confirm;
  // The terminal's input handler (what xterm's onData runs), for keys that
  // don't come from xterm itself — the mobile key bar's arrows and symbols.
  const inputRef = useRef<((data: string) => void) | null>(null);
  // Small tail of recent remote OUTPUT, kept only to detect no-echo password
  // prompts (see looksLikePasswordPrompt) — never persisted or displayed.
  const recentOutputRef = useRef<string>("");

  const toggleModifier = (m: ModKey) => {
    setModifiers(prev => ({ ...prev, [m]: cycleMod(prev[m]) }));
  };
  // Called after a transformed char is sent so armed modifiers reset back
  // to "off" while locked ones survive (matches Termux's behavior). Skipped
  // when nothing was actually armed — avoids a useless setState ping.
  const consumeArmedModifiers = () => {
    setModifiers(prev => {
      if (prev.ctrl !== "armed" && prev.alt !== "armed" && prev.shift !== "armed") return prev;
      return {
        ctrl:  prev.ctrl  === "armed" ? "off" : prev.ctrl,
        alt:   prev.alt   === "armed" ? "off" : prev.alt,
        shift: prev.shift === "armed" ? "off" : prev.shift,
      };
    });
  };

  // Esc / Tab from the bar bypass the onData modifier pipeline (those keys
  // produce escape sequences directly, not printable chars), but Shift+Tab
  // still has a meaningful encoding so we honor it here.
  //
  // Arrows and symbols go through the terminal's input handler like typed
  // keys. An arrow sends what xterm sends for the real key: `ESC [ A` (or
  // `ESC O A` once the program asked for application cursor keys, as vim and
  // less do), and with modifiers armed `ESC [ 1 ; <m> A`, where m is 1 plus
  // Shift 1, Alt 2, Ctrl 4. The symbols (| ~ / -) are typed characters, so the
  // handler applies the armed modifiers to them itself.
  const sendSpecialKey = (key: SpecialKey) => {
    if (disabledRef.current) return;
    if (key === "|" || key === "~" || key === "/" || key === "-") {
      inputRef.current?.(key);
      return;
    }
    if (key !== "esc" && key !== "tab") {
      const final = { up: "A", down: "B", right: "C", left: "D" }[key];
      const m = modifiersRef.current;
      const mod = 1 + (m.shift !== "off" ? 1 : 0) + (m.alt !== "off" ? 2 : 0) + (m.ctrl !== "off" ? 4 : 0);
      const appCursor = xtermRef.current?.modes.applicationCursorKeysMode ?? false;
      inputRef.current?.(mod > 1 ? `\x1b[1;${mod}${final}` : `\x1b${appCursor ? "O" : "["}${final}`);
      if (mod > 1) consumeArmedModifiers();
      return;
    }
    let bytes: number[];
    let consumes = false;
    if (key === "esc") {
      bytes = [0x1b];
    } else {
      if (modifiersRef.current.shift !== "off") {
        // ESC [ Z = CSI Z = back-tab (the standard Shift+Tab sequence).
        bytes = [0x1b, 0x5b, 0x5a];
        consumes = true;
      } else {
        bytes = [0x09];
      }
    }
    invoke('write_terminal_data', { terminalId, data: bytes }).catch(console.error);
    if (consumes) consumeArmedModifiers();
  };

  // Reconnect handler: when the parent bumps connectionEpoch we re-open
  // the PTY against the new SSH handle and write a divider line into the
  // existing xterm so the user can scroll up to see everything they did
  // before the disconnect. The backend's `terminal_txs` HashMap.insert
  // overwrites the dead entry transparently, so write_terminal_data
  // routes to the new task without changing the terminal_id.
  useEffect(() => {
    if (connectionEpoch === lastEpochRef.current) return;
    lastEpochRef.current = connectionEpoch;
    const term = xtermRef.current;
    if (!term) return;
    // Visible separator. \x1b[33m = yellow, \x1b[0m = reset.
    term.write('\r\n\x1b[33m── reconnected ──\x1b[0m\r\n');
    // Attach-only viewers piggyback on someone else's PTY, so they
    // must not re-open the PTY on reconnect either — the owner
    // already did it and this instance just needs to keep reading
    // events off the same terminal_id.
    if (attachOnly) return;
    // As on the first open: the backend only takes resizes once the shell is
    // up, so a resize that landed while it was starting again (the window
    // was being dragged, a panel opened) is sent once more when it is. Without
    // this the shell kept the size it was reopened with, and wrapped its lines
    // at the wrong column until the next resize.
    const syncSize = () => resendPtySize(terminalId, term.cols, term.rows);
    const ce = containerExecRef.current;
    if (telnet) {
      invoke('telnet_open_terminal', {
        sessionId,
        terminalId,
        cols: term.cols || 80,
        rows: term.rows || 24,
      }).then(syncSize, e => {
        term.writeln(`\x1b[31mReconnect failed: ${e}\x1b[0m`);
      });
    } else if (ce) {
      invoke('open_container_terminal', {
        sessionId,
        terminalId,
        container: ce.container,
        cols: term.cols || 80,
        rows: term.rows || 24,
        useSudo: ce.useSudo,
      }).then(syncSize, e => {
        term.writeln(`\x1b[31mReconnect into container failed: ${e}\x1b[0m`);
      });
    } else {
      // A terminal with a login of its own logs in again first.
      (ownLogin ? connectOwn(term) : Promise.resolve(sessionId))
        .then((sid) => {
          if (sid === null) return false;
          shellConnRef.current = sid;
          return invoke('open_terminal', {
            sessionId: sid,
            terminalId,
            cols: term.cols || 80,
            rows: term.rows || 24,
            useLocalShell: readUseLocalShell(),
          }).then(() => true);
        })
        .then((opened) => { if (opened) syncSize(); }, e => {
          term.writeln(`\x1b[31mReconnect failed: ${e}\x1b[0m`);
        });
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [connectionEpoch]);

  // A terminal with a login of its own: the tab disconnected, so its own
  // connection goes too (a reconnect logs it in again); and that connection
  // dropping on its own is said in the terminal.
  useEffect(() => {
    if (disabled) closeOwn();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [disabled]);
  useEffect(() => {
    if (!ownLogin) return;
    const off = listen(`session-disconnected-${ownConn}`, (e: any) => {
      // This side closed it (the terminal, its tab, a failed login): no news.
      if (e.payload?.user_initiated) return;
      shellConnRef.current = sessionId;
      xtermRef.current?.write("\r\n\x1b[33m── this terminal's own login closed; reconnect the tab to log in again ──\x1b[0m\r\n");
    });
    return () => { off.then((f) => f()); };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [ownConn, ownLogin]);

  // Move keyboard focus into this terminal's xterm. Shared by the
  // become-active effect and the parent-driven focus signal below, so the
  // user can type the moment a terminal tab (or its server tab) is selected
  // — issue #51. Bails in every situation where grabbing focus is wrong:
  //   • touch devices — focusing xterm pops the soft keyboard unasked;
  //   • a terminal whose SessionView isn't the frontmost server tab, or an
  //     attach-only Wall mirror (isActiveView=false) — never steal from the
  //     session the user is actually looking at;
  //   • a disconnected / not-yet-connected terminal (dead PTY; the reconnect
  //     banner or the connect-log view owns the screen). This also covers
  //     the fingerprint / 2FA prompts, which only appear while disabled;
  //   • the user is typing in a real input (SFTP path box, settings, the
  //     auth-retry password, the in-terminal Find box). xterm's own hidden
  //     <textarea> lives inside `.xterm`, so moving focus BETWEEN terminals
  //     is still allowed;
  //   • a modal dialog is open — confirm / overwrite / text-prompt all carry
  //     aria-modal.
  const focusTerminal = () => {
    const coarsePointer =
      typeof window !== "undefined" && !!window.matchMedia?.("(pointer: coarse)")?.matches;
    if (IS_ANDROID || coarsePointer) return;
    if (!isActiveViewRef.current) return;
    if (disabledRef.current) return;
    const term = xtermRef.current;
    if (!term) return;
    const active = document.activeElement as HTMLElement | null;
    if (active) {
      const tag = active.tagName;
      const isField =
        tag === "INPUT" || tag === "TEXTAREA" || tag === "SELECT" || active.isContentEditable;
      if (isField && !active.closest(".xterm")) return;
    }
    if (document.querySelector('[aria-modal="true"]')) return;
    term.focus();
  };

  // Repaint when this terminal becomes the active one. The parent uses
  // opacity (within a session) or display:none (across sessions/tabs) to
  // swap visible terminals — neither triggers xterm's internal redraw, so
  // the canvas can end up showing a stale row of glyphs from before the
  // switch. Calling `fit()` recomputes cols/rows and `refresh()` forces
  // every visible row to repaint. rAF defers until the layout has settled
  // — without it `fit()` would measure 0×0 in the display:none case.
  useEffect(() => {
    if (!isActive) return;
    const id = requestAnimationFrame(() => {
      try {
        fitAddonRef.current?.fit();
        const t = xtermRef.current;
        if (t) t.refresh(0, Math.max(0, t.rows - 1));
      } catch { /* terminal not ready yet — next tick will catch it */ }
      // Layout has settled and this is now the visible terminal: land the
      // caret in it so the user can type without a second click (#51).
      focusTerminal();
    });
    return () => cancelAnimationFrame(id);
  }, [isActive]);

  // Parent-driven re-focus. The effect above only fires when the ACTIVE
  // terminal CHANGES (isActive false→true); this handles the selections
  // that don't — re-clicking the current tab, re-selecting this server tab,
  // or clicking a tab while focus was in a tool pane — by reacting to a
  // bumped `focusSignal`. The signal is shared by all of a session's
  // terminals, so gate on isActiveRef: only the active one should grab it.
  const focusSignalRef = useRef(focusSignal);
  useEffect(() => {
    if (focusSignal === focusSignalRef.current) return; // mount / no real change
    focusSignalRef.current = focusSignal;
    const id = requestAnimationFrame(() => {
      if (isActiveRef.current) focusTerminal();
    });
    return () => cancelAnimationFrame(id);
  }, [focusSignal]);

  // Ctrl+F / Cmd+F opens the search chip — but only when the user is
  // focused inside this terminal's DOM subtree. Document-level listener is
  // needed because xterm's textarea steals keyboard focus, so the wrapper
  // never sees the keydown via bubbling within React's synthetic tree.
  // Gate on `document.activeElement` so multiple terminals in tabs / split
  // panes don't all pop their search chip on a single Ctrl+F.
  //
  // The same gate is reused for Ctrl+R / Cmd+R which opens the command
  // history overlay. Browsers normally reload the page on Ctrl+R; inside
  // the Tauri webview a document-level preventDefault stops that dead.
  useEffect(() => {
    const onKeyDown = (e: KeyboardEvent) => {
      if (!(e.ctrlKey || e.metaKey)) return;
      const wrapper = terminalRef.current;
      if (!wrapper) return;
      const active = document.activeElement;
      if (!(active instanceof Node) || !wrapper.contains(active)) return;
      if (e.key === 'f' || e.key === 'F') {
        e.preventDefault();
        setShowSearch(true);
        // If already open, refocus the input so a second Ctrl+F selects the
        // existing query for a quick overwrite.
        requestAnimationFrame(() => {
          searchInputRef.current?.focus();
          searchInputRef.current?.select();
        });
      } else if (e.key === 'r' || e.key === 'R') {
        e.preventDefault();
        setShowHistory(true);
      }
    };
    document.addEventListener('keydown', onKeyDown);
    return () => document.removeEventListener('keydown', onKeyDown);
  }, []);

  // Close + clear search decorations. Refocuses xterm so the user's next
  // keystroke goes to the shell, not to nowhere.
  const closeSearch = () => {
    const addon = searchAddonRef.current;
    // clearDecorations arrived in xterm-addon-search 0.13; guard for older
    // typings even though our lockfile pins 0.13.0.
    addon?.clearDecorations?.();
    setShowSearch(false);
    // Return focus to xterm so keystrokes resume typing at the prompt.
    xtermRef.current?.focus();
  };

  const runFind = (direction: 'next' | 'prev') => {
    const addon = searchAddonRef.current;
    if (!addon || !query) return;
    if (direction === 'next') addon.findNext(query, SEARCH_OPTIONS);
    else addon.findPrevious(query, SEARCH_OPTIONS);
  };

  // Tiny inline toast for clipboard feedback (copy / paste / errors). Pattern
  // matches the per-component notify() used in SftpWorkspace / FilePanel —
  // keeps the component self-contained without a global toast provider.
  // The colour scheme's background also fills the padding around xterm.
  const [termBg, setTermBg] = useState(() => readScheme().theme.background ?? '#09090b');
  const [toast, setToast] = useState<{ msg: string; tone: "ok" | "err" } | null>(null);
  const toastTimerRef = useRef<number | null>(null);
  const notify = (msg: string, tone: "ok" | "err" = "ok") => {
    setToast({ msg, tone });
    if (toastTimerRef.current) window.clearTimeout(toastTimerRef.current);
    toastTimerRef.current = window.setTimeout(() => setToast(null), 1400);
  };

  // The output ends in a password prompt (sudo, su, …): a button offers to
  // type a saved password there (#28). The next key typed or more output
  // takes it away. The backend types the password: it never comes here.
  const [atPasswordPrompt, setAtPasswordPrompt] = useState(false);
  const atPasswordPromptRef = useRef(false);
  const showPasswordButton = (on: boolean) => {
    if (atPasswordPromptRef.current === on) return;
    atPasswordPromptRef.current = on;
    setAtPasswordPrompt(on);
    if (!on) setPasswordMenu(null);
  };
  type SavedPassword = { label: string; detail: string; serverId?: number; credentialId?: number };
  const [passwordMenu, setPasswordMenu] = useState<SavedPassword[] | null>(null);
  const openPasswordMenu = async () => {
    try {
      const [servers, logins] = await Promise.all([
        invoke<any[]>('get_servers'),
        invoke<any[]>('get_credentials'),
      ]);
      const server = serverId > 0 ? servers.find((s) => s.id === serverId) : undefined;
      const items: SavedPassword[] = [];
      if (server?.has_password) {
        items.push({ label: "This server's password", detail: server.username || server.name, serverId: server.id });
      }
      // Its own saved login first, then the others.
      const withPassword = logins.filter((c) => c.has_password);
      withPassword.sort((a, b) => Number(b.id === server?.credential_id) - Number(a.id === server?.credential_id));
      for (const c of withPassword) {
        items.push({
          label: c.name,
          detail: c.id === server?.credential_id ? `${c.username} · this server's login` : c.username,
          credentialId: c.id,
        });
      }
      setPasswordMenu(items);
    } catch (e) {
      notify(`Couldn't read the saved passwords: ${String(e)}`, 'err');
    }
  };
  const typeSavedPassword = async (p: SavedPassword) => {
    setPasswordMenu(null);
    try {
      await invoke('type_saved_password', {
        terminalId,
        serverId: p.serverId ?? null,
        credentialId: p.credentialId ?? null,
      });
      showPasswordButton(false);
    } catch (e) {
      notify(String(e), 'err');
    }
    xtermRef.current?.focus();
  };

  useEffect(() => {
    if (!terminalRef.current) return;

    const term = new Terminal({
      cursorBlink: true,
      fontSize: readFontSize(),
      scrollback: readScrollback(),
      // Start on the always-installed fallback stack; the chosen face is
      // switched in below once it has loaded (handleSettingsChange). A bundled
      // font isn't loaded yet at this point, and xterm measures its cell size
      // from whatever face renders first — measuring the fallback and then
      // drawing the real font would leave gaps or overlapping glyphs.
      fontFamily: DEFAULT_FONT_STACK,
      // Colours: Settings → Terminal (util/terminalTheme).
      theme: readScheme().theme,
      drawBoldTextInBrightColors: readBoldBright(),
      minimumContrastRatio: readMinContrast() ? READABLE_CONTRAST : 1,
      allowProposedApi: true,
      // OSC 8 hyperlinks a program on the server prints. What they show can
      // differ from where they go, so the real target is shown first, and
      // only an http(s) link opens, in the system browser (open_external_url
      // refuses anything else) rather than inside the app. xterm already
      // ignores other schemes while allowNonHttpProtocols is off.
      linkHandler: {
        allowNonHttpProtocols: false,
        activate: (_event, uri) => {
          void (async () => {
            if (!/^https?:\/\//i.test(uri)) return;
            let host = uri;
            try { host = new URL(uri).host; } catch { return; }
            // A very long target is cut so the dialog's buttons stay on
            // screen; the host line still says where it goes.
            const shown = uri.length > 300 ? `${uri.slice(0, 300)}…` : uri;
            const ok = await confirmRef.current({
              title: 'Open this link?',
              message: `A program in this terminal links to:\n\n${shown}\n\nIt opens in your browser, at ${host}.`,
              okLabel: 'Open',
              safeDefault: true,
            });
            if (!ok) return;
            try {
              await invoke('open_external_url', { url: uri });
            } catch (e) {
              notify(`Couldn't open the link: ${String(e)}`, 'err');
            }
          })();
        },
      },
    });

    const fitAddon = new FitAddon();
    fitAddonRef.current = fitAddon;
    term.loadAddon(fitAddon);
    const searchAddon = new SearchAddon();
    searchAddonRef.current = searchAddon;
    term.loadAddon(searchAddon);
    term.open(terminalRef.current);
    
    setTimeout(() => {
      fitAddon.fit();
      // A Wall tile sets the size while it's open, also when fit() kept
      // xterm's default size and so fired no resize.
      if (attachOnly && (wallTiles.get(terminalId) ?? 0) > 0) sendPtySize(terminalId, term.cols, term.rows);

      // Start PTY Session with correct dimensions only ONCE. Attach-only
      // instances skip this entirely — they hitch onto an existing PTY
      // opened by the primary instance and would collide with it if they
      // tried to open the same terminal_id a second time.
      if (!openedRef.current && !attachOnly) {
        openedRef.current = true;
        // Branch: a `containerExec` prop means we want a docker-exec
        // session into a specific container, not the regular login
        // shell. Same xterm wiring, different backend command — the
        // PTY/data/resize event topology is identical so xterm doesn't
        // notice the difference.
        // The backend only takes resizes once the shell is up, so a refit
        // that landed while it was starting (e.g. the chosen font finished
        // loading and changed the cell size) is sent again once it's open.
        const syncSize = () => resendPtySize(terminalId, term.cols, term.rows);
        if (telnet) {
          invoke('telnet_open_terminal', {
            sessionId,
            terminalId,
            cols: term.cols || 80,
            rows: term.rows || 24,
          }).then(syncSize, e => {
            term.writeln(`\x1b[31mFailed to open terminal: ${e}\x1b[0m`);
          });
        } else if (containerExec) {
          invoke('open_container_terminal', {
            sessionId,
            terminalId,
            container: containerExec.container,
            cols: term.cols || 80,
            rows: term.rows || 24,
            useSudo: containerExec.useSudo,
          }).then(syncSize, e => {
            term.writeln(`\x1b[31mFailed to attach to container: ${e}\x1b[0m`);
          });
        } else {
          // A terminal with a login of its own logs in first.
          (ownLogin ? connectOwn(term) : Promise.resolve(sessionId)).then((sid) => {
            if (sid === null) return false;
            shellConnRef.current = sid;
            return invoke('open_terminal', {
              sessionId: sid,
              terminalId,
              cols: term.cols || 80,
              rows: term.rows || 24,
              useLocalShell: readUseLocalShell(),
            }).then(() => true);
          }).then((opened) => {
            if (!opened) return;
            syncSize();
            // Its shell takes input from now on: SessionView types a server's
            // on-connect commands once this arrives for its first terminal.
            window.dispatchEvent(new CustomEvent('submarine-terminal-opened', { detail: terminalId }));
          }, e => {
            term.writeln(`\x1b[31mFailed to open terminal: ${e}\x1b[0m`);
          });
        }
      }
    }, 50);

    xtermRef.current = term;

    // Handle Input — swallow keystrokes once the session is disconnected so
    // they don't pile up against a dead backend channel. When a mobile-key-bar
    // modifier is armed/locked we transform single printable characters into
    // the matching control sequence before sending (Ctrl+letter → 0x01-0x1a,
    // Alt+char → ESC-prefix). Multi-char inputs (paste, IME composition) are
    // forwarded verbatim because we can't sensibly "Ctrl" a phrase.
    const handleData = (data: string) => {
      if (disabledRef.current) return;
      showPasswordButton(false);
      // A paste: our right-click path, or xterm's own keyboard paste, which
      // starts with the bracketed-paste marker when the remote program asked
      // for one. Never run through the armed modifiers or into history.
      const isPaste = pastingRef.current || data.startsWith("\x1b[200~");
      const mods = modifiersRef.current;
      const hasMod = mods.ctrl !== "off" || mods.alt !== "off";
      let bytes: number[];
      if (hasMod && data.length === 1 && !isPaste) {
        const code = data.charCodeAt(0);
        let ch = code;
        if (mods.ctrl !== "off") {
          if ((code >= 0x41 && code <= 0x5a) || (code >= 0x61 && code <= 0x7a)) {
            // Letters → C0 control (Ctrl+A = 0x01 … Ctrl+Z = 0x1a). `code & 0x1f`
            // does the right thing for both upper- and lower-case letters.
            ch = code & 0x1f;
          }
          // Non-letters under Ctrl are left as-is — Android's soft keyboard
          // rarely lets the user type the ones with defined C0 mappings
          // (Ctrl+@, Ctrl+[, Ctrl+\, Ctrl+] etc.) anyway, and silently
          // mangling normal punctuation while a Ctrl chip is lit would
          // confuse more than help.
        }
        bytes = [];
        if (mods.alt !== "off") bytes.push(0x1b); // ESC prefix → Meta
        bytes.push(ch);
        consumeArmedModifiers();
      } else {
        bytes = Array.from(new TextEncoder().encode(data));
      }
      invoke('write_terminal_data', {
        terminalId,
        data: bytes,
      }).catch(console.error);

      // ── Broadcast fan-out ────────────────────────────────────────────────
      // Broadcast writes AFTER the local write so if the fan-out throws
      // synchronously it doesn't drop the source shell's own byte. We only
      // fan out when there are 2+ selected targets — a single target would
      // let the user broadcast to a single foreign session, which is
      // technically fine but almost always a footgun ("wait, why did that
      // command run there instead of here?"). 2+ makes the intent explicit.
      const bcast = broadcastRef.current;
      // Fan out ONLY when the source session is itself one of the armed
      // targets. Without this guard, typing into an unrelated session (not in
      // the target set) still wrote to every target — so a command meant only
      // for the focused server would silently execute on the broadcast group.
      // Requiring the source to be a member makes "this pane broadcasts to the
      // group" the explicit, safe model.
      if (bcast.enabled && bcast.targetSessionIds.size >= 2 && bcast.targetSessionIds.has(sessionId)) {
        for (const targetSid of bcast.targetSessionIds) {
          if (targetSid === sessionId) continue; // skip source; already written
          const targetTid = bcast.sessionTerminalMap[targetSid];
          if (!targetTid) continue; // target has no live terminal — silently skip
          invoke('write_terminal_data', {
            terminalId: targetTid,
            data: bytes,
          }).catch(() => {
            // Dead target — do NOT surface. The user asked for fire-and-forget
            // fan-out; failures here would spam their console every keystroke.
          });
        }
      }

      // ── Command history capture (best-effort) ────────────────────────────
      // Bufffer up bytes until we see a CR (Enter), then log the accumulated
      // line if it looks like a real command. Purely local heuristic — we do
      // not parse the shell's echo, so arrow-key edits will over-count or
      // mis-count. Skip anything under 3 chars to weed out "y\r" prompts and
      // stray Enters.
      //
      // Pasted text isn't recorded: a multi-line paste into an editor would
      // log every line (and, past the 1,000-entry cap, push out the real
      // history), and a pasted token or key isn't something to keep. A paste
      // that doesn't end in a line break leaves its last line on the prompt,
      // so that line's Enter is skipped too.
      if (isPaste) {
        const body = data.replace(/^\x1b\[200~/, "").replace(/\x1b\[201~$/, "");
        commandBufRef.current = "";
        commandBufPastedRef.current = !/[\r\n]$/.test(body);
        return;
      }
      for (let i = 0; i < data.length; i++) {
        const ch = data.charCodeAt(i);
        if (ch === 0x0d /* CR */ || ch === 0x0a /* LF */) {
          const line = commandBufRef.current.trim();
          const pasted = commandBufPastedRef.current;
          commandBufRef.current = "";
          commandBufPastedRef.current = false;
          // Do NOT persist what was typed at a no-echo password/passphrase
          // prompt — otherwise sudo/su/mysql -p/gpg secrets land verbatim in
          // searchable history. The onData handler can't see the PTY echo
          // mode, so we infer it from the remote output tail.
          if (!pasted && line.length >= 3 && !looksLikePasswordPrompt(recentOutputRef.current)) {
            invoke('cmd_history_add', {
              serverId: serverId > 0 ? serverId : null,
              serverName: serverName || "",
              command: line,
            }).catch(() => { /* silent — history logging is best-effort */ });
          }
        } else if (ch === 0x7f /* DEL */ || ch === 0x08 /* BS */) {
          commandBufRef.current = commandBufRef.current.slice(0, -1);
        } else if (ch === 0x03 /* Ctrl+C */ || ch === 0x15 /* Ctrl+U */) {
          // Reset the buffer — the shell just cleared the line.
          commandBufRef.current = "";
          commandBufPastedRef.current = false;
        } else if (ch === 0x1b /* ESC — arrow keys etc. */) {
          // Skip the whole ESC sequence rather than treating the following
          // bytes as literal characters. Simple heuristic:
          //   ESC [ ... <final>   → CSI, final byte is 0x40-0x7e
          //   ESC O <char>         → SS3 (function keys); consume one byte
          //   ESC <char>           → any other simple sequence; consume one
          const nxt = i + 1 < data.length ? data.charCodeAt(i + 1) : -1;
          if (nxt === 0x5b /* [ */) {
            i += 1; // consume '['
            while (i + 1 < data.length) {
              const fin = data.charCodeAt(i + 1);
              i++;
              if (fin >= 0x40 && fin <= 0x7e) break;
            }
          } else if (nxt !== -1) {
            i += 1; // consume the byte after ESC
          }
        } else if (ch >= 0x20 && ch <= 0x7e) {
          commandBufRef.current += data[i];
          if (commandBufRef.current.length > 4096) {
            // Cap to keep pathological pastes from ballooning memory.
            commandBufRef.current = commandBufRef.current.slice(-4096);
          }
        }
        // Other control chars are ignored on the assumption they don't
        // contribute to the user's visible command line.
      }
    };
    // The mobile key bar's arrow and symbol keys come through here too, so
    // they're broadcast and recorded exactly like typed input.
    inputRef.current = handleData;
    const onDataDisposable = term.onData(handleData);

    // ---- Copy on select / paste on right-click --------------------------------
    // Selection-change fires per mouse move during a drag — that's noisy AND
    // it would clobber the clipboard mid-drag. We instead wait for the user
    // to release the mouse, then copy whatever is currently selected.
    const writeClipboard = async (text: string) => {
      try {
        await navigator.clipboard.writeText(text);
        return true;
      } catch {
        return false;
      }
    };
    // term.getSelection() emits one '\n' per buffer row, including rows that
    // are mid-wrap continuations of the previous logical line — paste that
    // into anything and you get a phantom blank between every wrapped
    // segment. Walk the buffer ourselves using `isWrapped` so a wrapped
    // line round-trips as a single line. Plain (non-wrapped) row breaks
    // stay '\n'.
    const buildSelectedText = (): string => {
      const range = term.getSelectionPosition();
      if (!range) return '';
      const buf = term.buffer.active;
      const lines: string[] = [];
      for (let y = range.start.y; y <= range.end.y; y++) {
        const line = buf.getLine(y);
        if (!line) continue;
        const startX = y === range.start.y ? range.start.x : 0;
        const endX   = y === range.end.y   ? range.end.x   : undefined;
        const text = line.translateToString(true, startX, endX);
        if (y > range.start.y && line.isWrapped && lines.length > 0) {
          lines[lines.length - 1] += text;
        } else {
          lines.push(text);
        }
      }
      return lines.join('\n');
    };
    const copySelectionIfAny = async () => {
      // A selection that runs past the last line ends in empty rows: Ctrl+Shift+A
      // takes the whole buffer, the blank rows under the prompt included.
      const text = buildSelectedText().replace(/\n+$/, '');
      if (!text) return;
      const ok = await writeClipboard(text);
      if (ok) notify('Copied');
    };
    // Copy-on-select must fire wherever the mouse is RELEASED — not only inside
    // the terminal. A user who drags a selection and lets go past the terminal's
    // edge (a very natural motion) still expects the copy, but the old handler
    // listened for `mouseup` on the terminal element itself, so a release
    // outside it never fired and only left the text selected.
    //
    // Instead: ARM on a left-button mousedown that starts inside this terminal,
    // then catch `mouseup` on the DOCUMENT so the release is seen anywhere.
    // Requiring the drag to have started here keeps an unrelated release
    // elsewhere from touching the clipboard, and copySelectionIfAny() no-ops
    // when there's no real selection — so a plain click (which clears the
    // selection) still can't clobber it. The selection end is set by the last
    // mousemove, so it's final by the time we read it on mouseup.
    let selectionArmed = false;
    const onMouseDown = (ev: MouseEvent) => {
      // Assign (not just set-true) so a non-left mousedown re-disarms a flag
      // that got stuck from an earlier drag whose mouseup we never saw.
      selectionArmed = ev.button === 0;
    };
    const onDocMouseUp = (ev: MouseEvent) => {
      if (ev.button !== 0 || !selectionArmed) return;
      selectionArmed = false;
      void copySelectionIfAny();
    };
    // If the window loses focus mid-drag (Alt-Tab, a focus-stealing dialog),
    // the button is released over another window and the matching `mouseup`
    // never reaches `document`, leaving selectionArmed stuck true. A later
    // unrelated left-click would then copy this terminal's stale selection and
    // clobber the clipboard. Disarm on blur so only a release we actually see
    // can copy.
    const onWindowBlur = () => { selectionArmed = false; };
    // The clipboard's text for a paste. On Linux it comes from the backend:
    // WebKitGTK refuses the page's own clipboard read (#89), and where the
    // webview reads the clipboard itself it crashes the whole app when the
    // clipboard is empty (seen with WebKitGTK 2.54), so it is kept out of it
    // there.
    const readClipboardText = (): Promise<string> =>
      IS_LINUX ? invoke<string>('clipboard_read_text') : navigator.clipboard.readText();
    // Paste the clipboard into the PTY. Disabled sessions silently drop the
    // paste (matches onData).
    const pasteClipboard = async (toast: boolean) => {
      if (disabledRef.current) return;
      let text = '';
      try { text = await readClipboardText(); }
      catch { notify('Clipboard read denied', 'err'); return; }
      if (!text) return;
      // No escape sequence or other control character gets through (see pasteText.ts).
      const clean = withoutPasteControls(text);
      if (clean !== text) { text = clean; notify('Control characters left out of the paste'); }
      if (!text) return;
      // More than one line into a program that hasn't turned bracketed-paste
      // mode on (an older bash, a plain sh): each line would run the moment it
      // lands, so ask first and show what would run. With the mode on, the
      // program takes the paste as one block and nothing runs by itself; a
      // single line with a trailing newline runs as one command, as anywhere.
      const lines = text.replace(/(\r\n|\r|\n)$/, '').split(/\r\n|\r|\n/);
      if (lines.length > 1 && !term.modes.bracketedPasteMode) {
        const preview = lines.slice(0, 4).map((l) => (l.length > 70 ? `${l.slice(0, 70)}…` : l)).join('\n');
        const ok = await confirmRef.current({
          title: `Paste ${lines.length} lines?`,
          message: `The program here doesn't take a paste as one block, so each line runs as soon as it lands:\n\n${preview}${lines.length > 4 ? `\n… and ${lines.length - 4} more` : ''}`,
          okLabel: 'Paste',
          safeDefault: true,
        });
        if (!ok) return;
      }
      // Hand the text to xterm's own paste path instead of writing it raw to
      // the PTY. xterm collapses CRLF / lone LF to a single CR (one Enter per
      // line break — a Windows clipboard's `\r\n` would otherwise be two) and,
      // when the remote program has turned on bracketed-paste mode (vim,
      // bash ≥ 5.1, zsh, …), wraps the text in ESC[200~ … ESC[201~ so the
      // program knows it was pasted. The raw write skipped those brackets, so
      // vim treated every pasted line as typed input and auto-indented each
      // one on top of the previous (#56), and a shell ran each pasted line the
      // moment it arrived. Going through xterm also routes the paste through
      // onData like every other input, so broadcast sees it like a keyboard
      // paste; `pastingRef` keeps it out of history and the armed modifiers.
      pastingRef.current = true;
      try {
        term.paste(text);
      } finally {
        pastingRef.current = false;
      }
      if (toast) notify('Pasted');
    };
    // Ctrl+V, Ctrl+Shift+V, Shift+Insert and a phone's paste menu reach xterm's
    // own paste. When there is something to take out of the text, it is pasted
    // without it instead (the capture phase runs before xterm's handler); text
    // with nothing to take out goes the way it always did.
    const onPasteEvent = (ev: ClipboardEvent) => {
      const raw = ev.clipboardData?.getData('text/plain') ?? '';
      const clean = withoutPasteControls(raw);
      if (clean === raw) return;
      ev.preventDefault();
      ev.stopImmediatePropagation();
      if (disabledRef.current || !clean) return;
      pastingRef.current = true;
      try {
        term.paste(clean);
      } finally {
        pastingRef.current = false;
      }
      notify('Control characters left out of the paste');
    };
    // Right-click → paste. preventDefault swallows the platform context menu
    // so the user gets the terminal-style behavior they asked for.
    const onContextMenu = (ev: MouseEvent) => {
      ev.preventDefault();
      void pasteClipboard(true);
    };
    // Ctrl+Shift+A / Ctrl+Shift+C / Ctrl+Shift+V — Warp/GNOME-Terminal-style
    // select-all / copy / paste, deliberately on the SHIFTED combo so plain
    // Ctrl+A (start of line) and Ctrl+C (SIGINT) keep going straight to the
    // remote shell untouched, exactly as every readline/zle user expects.
    // xterm.js only allows one attachCustomKeyEventHandler at a time, so all
    // three live in this single handler rather than three separate calls.
    term.attachCustomKeyEventHandler((ev) => {
      const noOtherMods = !ev.altKey && !ev.metaKey && ev.ctrlKey && ev.shiftKey;
      if (noOtherMods && ev.code === 'KeyA') {
        if (ev.type === 'keydown' && !ev.repeat) term.selectAll();
        ev.preventDefault();
        return false;
      }
      if (noOtherMods && ev.code === 'KeyC') {
        if (ev.type === 'keydown' && !ev.repeat) void copySelectionIfAny();
        ev.preventDefault();
        return false;
      }
      // Linux only: paste. Elsewhere the webview's own Ctrl+Shift+V /
      // Shift+Insert already reaches xterm's built-in paste handling, but
      // WebKitGTK's own clipboard read is the one that crashes on an empty
      // clipboard (#89), so it's taken over here and routed through the
      // same path as a right-click instead.
      if (IS_LINUX) {
        const isPasteKey = !ev.altKey && !ev.metaKey && (
          (ev.ctrlKey && ev.shiftKey && ev.code === 'KeyV') ||
          (ev.shiftKey && !ev.ctrlKey && ev.key === 'Insert')
        );
        if (isPasteKey) {
          ev.preventDefault();
          if (ev.type === 'keydown' && !ev.repeat) void pasteClipboard(false);
          return false;
        }
      }
      return true;
    });
    terminalRef.current.addEventListener('mousedown', onMouseDown);
    document.addEventListener('mouseup', onDocMouseUp);
    window.addEventListener('blur', onWindowBlur);
    terminalRef.current.addEventListener('contextmenu', onContextMenu);
    terminalRef.current.addEventListener('paste', onPasteEvent, true);

    // Handle Resize, once the size has settled (PTY_SIZE_SETTLE_MS).
    let sizeTimer = 0;
    const onResizeDisposable = term.onResize(() => {
      window.clearTimeout(sizeTimer);
      sizeTimer = window.setTimeout(() => {
        if (!attachOnly && (wallTiles.get(terminalId) ?? 0) > 0) return;
        sendPtySize(terminalId, term.cols, term.rows);
      }, PTY_SIZE_SETTLE_MS);
    });
    // The session tab's view: the Wall tile that set the size has closed.
    const onWallTileClosed = (e: Event) => {
      if ((e as CustomEvent<string>).detail !== terminalId) return;
      if ((wallTiles.get(terminalId) ?? 0) > 0) return;
      sendPtySize(terminalId, term.cols, term.rows);
    };
    if (attachOnly) wallTiles.set(terminalId, (wallTiles.get(terminalId) ?? 0) + 1);
    else window.addEventListener(WALL_TILE_CLOSED, onWallTileClosed);

    // Handle Output with proper async cleanup for StrictMode
    const unlistenPromise = listen(`terminal-output-${terminalId}`, (event: any) => {
      // The backend now coalesces output and sends it base64-encoded (was a raw
      // JSON byte array — ~4x larger and one event per SSH packet, which flooded
      // this main-thread handler and froze the tab on big output). Decode back
      // to bytes; xterm buffers any multibyte sequence split across batches.
      const bin = atob(event.payload as string);
      const data = new Uint8Array(bin.length);
      for (let i = 0; i < bin.length; i++) data[i] = bin.charCodeAt(i);
      term.write(data);
      // Keep a short tail of decoded output so command-history capture can
      // recognize a no-echo password prompt and skip the typed secret.
      try {
        const txt = new TextDecoder().decode(data);
        const combined = recentOutputRef.current + txt;
        recentOutputRef.current = combined.length > 512 ? combined.slice(-512) : combined;
        showPasswordButton(looksLikePasswordPrompt(recentOutputRef.current));
      } catch { /* decode hiccup on a split multibyte chunk — ignore */ }
    });

    // Coalesce resize bursts via rAF, and skip the refit on hidden tabs
    // so SIGWINCH doesn't leak to background PTYs (the become-visible
    // effect refits when the tab is shown again).
    let rafId = 0;
    const resizeObserver = new ResizeObserver(() => {
      if (!xtermRef.current) return;
      if (!isActiveRef.current) return;
      // A terminal at its bottom stays there. On a phone, the box changing
      // while output comes in (the key bar back after a reconnect, the
      // keyboard) is at times taken by xterm for the user scrolling up, and
      // the view then stops following what comes in: a reconnected session
      // kept showing the old one's last lines, its new prompt out of sight
      // below. Put it back at the bottom once the change has settled.
      const buf = xtermRef.current.buffer.active;
      if (buf.viewportY === buf.baseY) {
        requestAnimationFrame(() => requestAnimationFrame(() => xtermRef.current?.scrollToBottom()));
      }
      if (rafId) return; // a rAF is already queued; coalesce
      rafId = requestAnimationFrame(() => {
        rafId = 0;
        if (!xtermRef.current) return;
        if (!isActiveRef.current) return;
        try {
          fitAddon.fit();
          const t = xtermRef.current;
          t.refresh(0, Math.max(0, t.rows - 1));
        } catch { /* swallow — xterm tolerates transient 0×0 sizes */ }
      });
    });
    resizeObserver.observe(terminalRef.current);
    
    // Handle Settings Change — font size and face apply live to open terminals.
    let fontDisposed = false;
    const handleSettingsChange = () => {
      // So does the scrollback; fewer lines than the terminal holds drops the
      // oldest ones.
      const scrollback = readScrollback();
      if (term.options.scrollback !== scrollback) term.options.scrollback = scrollback;
      // And the colours.
      const theme = readScheme().theme;
      if (JSON.stringify(term.options.theme) !== JSON.stringify(theme)) {
        term.options.theme = theme;
        setTermBg(theme.background ?? '#09090b');
      }
      const boldBright = readBoldBright();
      if (term.options.drawBoldTextInBrightColors !== boldBright) term.options.drawBoldTextInBrightColors = boldBright;
      const contrast = readMinContrast() ? READABLE_CONTRAST : 1;
      if (term.options.minimumContrastRatio !== contrast) term.options.minimumContrastRatio = contrast;
      const size = readFontSize();
      const family = fontFamilyCss(readFontFamily());
      if (term.options.fontSize === size && term.options.fontFamily === family) return;
      const apply = () => {
        if (fontDisposed) return;
        // A newer change may have loaded first — never let this older one
        // overwrite it.
        if (readFontSize() !== size || fontFamilyCss(readFontFamily()) !== family) return;
        term.options.fontSize = size;
        term.options.fontFamily = family;
        try {
          fitAddon.fit();
          term.refresh(0, Math.max(0, term.rows - 1));
        } catch { /* hidden tab (0×0) — the ResizeObserver refits on show */ }
      };
      // Wait until the face is ready so xterm measures the real glyph width,
      // not the fallback's (a mis-measured cell leaves gaps or overlaps).
      const fonts = (document as any).fonts;
      if (fonts?.load) fonts.load(`${size}px ${family}`).then(apply, apply);
      else apply();
    };
    window.addEventListener('submarine-settings-changed', handleSettingsChange);
    // Apply the saved face now (no-op when it's the default stack).
    handleSettingsChange();

    // ── Mobile QoL ───────────────────────────────────────────────────────────
    // Capture the container ref here so the listener add/remove calls and the
    // cleanup closure all reference the same element — the live ref can flip
    // to null between mount and cleanup if React tears the subtree down out
    // of order, and we'd leak event handlers in that case.
    const container = terminalRef.current;
    // On soft-keyboard open (`visualViewport.resize` shrinks) refit so xterm's
    // row count matches the now-shorter visible area and scroll to the
    // prompt so the cursor row sits just above the kb.
    //
    // We deliberately do NOT hook mousedown / touchstart here. Those fire at
    // the START of a selection drag, and a scrollToBottom() there yanks the
    // viewport out from under the user's finger and destroys the selection
    // anchor (v0.2.30-v0.2.31 regression). xterm already scrolls on user
    // keystrokes via `scrollOnUserInput` (default true) and on PTY writes,
    // so tapping to focus + typing already reveals the prompt without our
    // help. This handler is what actually implements "keyboard opens →
    // prompt in view" and is the only trigger we want.
    //
    // No-op on desktop: visualViewport.resize doesn't fire from window resizes,
    // only virtual-keyboard / pinch-zoom.
    const scrollPromptIntoView = () => {
      requestAnimationFrame(() => {
        try { fitAddon.fit(); } catch { /* terminal not ready */ }
        term.scrollToBottom();
      });
    };

    let lastVvHeight = window.visualViewport?.height ?? window.innerHeight;
    const onVvResize = () => {
      const h = window.visualViewport?.height ?? window.innerHeight;
      if (h < lastVvHeight - 80) {
        // Drop ≥80px almost always means the soft keyboard opened (not a
        // small browser-UI reflow). 80px is comfortably above the URL-bar
        // collapse delta on Chrome/Android.
        scrollPromptIntoView();
      }
      lastVvHeight = h;
    };
    window.visualViewport?.addEventListener('resize', onVvResize);

    return () => {
      fontDisposed = true;
      window.removeEventListener('submarine-settings-changed', handleSettingsChange);
      resizeObserver.disconnect();
      onDataDisposable.dispose();
      if (inputRef.current === handleData) inputRef.current = null;
      onResizeDisposable.dispose();
      window.clearTimeout(sizeTimer);
      if (container) {
        container.removeEventListener('mousedown', onMouseDown);
        container.removeEventListener('contextmenu', onContextMenu);
        container.removeEventListener('paste', onPasteEvent, true);
      }
      document.removeEventListener('mouseup', onDocMouseUp);
      window.removeEventListener('blur', onWindowBlur);
      window.visualViewport?.removeEventListener('resize', onVvResize);
      if (toastTimerRef.current) window.clearTimeout(toastTimerRef.current);
      unlistenPromise.then(unlisten => unlisten());
      searchAddon.dispose();
      searchAddonRef.current = null;
      term.dispose();
      // Only the PTY owner tears the backend PTY down on unmount.
      // Attach-only viewers unmount without closing so the primary
      // xterm (in the session tab) keeps working.
      if (!attachOnly) {
        invoke('close_terminal', { terminalId }).catch(console.error);
        closeOwn();
        window.removeEventListener(WALL_TILE_CLOSED, onWallTileClosed);
        ptySizes.delete(terminalId);
      } else {
        const left = (wallTiles.get(terminalId) ?? 1) - 1;
        if (left > 0) wallTiles.set(terminalId, left);
        else wallTiles.delete(terminalId);
        window.dispatchEvent(new CustomEvent(WALL_TILE_CLOSED, { detail: terminalId }));
      }
    };
  }, [sessionId, terminalId]);

  return (
    // flex-col so the MobileKeyBar can pin to the bottom of the terminal
    // pane (between xterm and the system soft keyboard). On desktop the bar
    // isn't rendered at all, so the xterm child claims the full height as
    // before.
    <div className="h-full w-full p-2 pr-2 pb-0 relative flex flex-col" style={{ background: termBg }}>
      <div
        ref={terminalRef}
        className="flex-1 min-h-0 w-full overflow-hidden select-text"
      />
      {/* Search chip — floats above xterm; positioned inside the outer
          flex column so it never displaces the terminal grid. Absolute
          top-2 right-2 keeps it clear of the disconnected badge (which
          centers) and the clipboard toast (bottom-right). */}
      {showSearch && (
        <div className="absolute top-2 right-2 z-20 flex items-center gap-1 rounded-md bg-zinc-800 border border-white/10 px-1.5 py-1 shadow-lg">
          <input
            ref={searchInputRef}
            autoFocus
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            onKeyDown={(e) => {
              // Stop keys from leaking to the document listener (Ctrl+F
              // reopen loop) and to xterm's textarea (which would type
              // into the shell).
              if (e.key === 'Escape') {
                e.preventDefault();
                setQuery('');
                closeSearch();
              } else if (e.key === 'Enter') {
                e.preventDefault();
                runFind(e.shiftKey ? 'prev' : 'next');
              } else if ((e.ctrlKey || e.metaKey) && (e.key === 'f' || e.key === 'F')) {
                // Swallow so the outer document handler doesn't re-fire
                // (which would just refocus us — harmless but noisy).
                e.preventDefault();
                (e.currentTarget as HTMLInputElement).select();
              }
              e.stopPropagation();
            }}
            placeholder="Find"
            className="bg-transparent outline-none text-[12px] text-zinc-100 placeholder:text-zinc-500 w-40 px-1"
          />
          <button
            type="button"
            onClick={() => runFind('prev')}
            title="Previous match (Shift+Enter)"
            className="px-1.5 py-0.5 text-[11px] text-zinc-300 hover:text-white hover:bg-white/5 rounded"
          >
            {'↑'}
          </button>
          <button
            type="button"
            onClick={() => runFind('next')}
            title="Next match (Enter)"
            className="px-1.5 py-0.5 text-[11px] text-zinc-300 hover:text-white hover:bg-white/5 rounded"
          >
            {'↓'}
          </button>
          <button
            type="button"
            onClick={() => { setQuery(''); closeSearch(); }}
            title="Close (Esc)"
            className="px-1.5 py-0.5 text-[11px] text-zinc-400 hover:text-white hover:bg-white/5 rounded"
          >
            {'✕'}
          </button>
        </div>
      )}
      {atPasswordPrompt && !disabled && !showSearch && (
        <div className="absolute top-2 right-2 z-20 flex flex-col items-end gap-1 max-w-[calc(100%-1rem)]">
          <button
            type="button"
            onClick={() => (passwordMenu ? setPasswordMenu(null) : void openPasswordMenu())}
            aria-haspopup="menu"
            aria-expanded={!!passwordMenu}
            title="Type a saved password at this prompt (no Enter)"
            className="flex items-center gap-1.5 rounded-md bg-zinc-800 border border-white/10 px-2 py-1 shadow-lg text-[11px] text-zinc-200 hover:text-white hover:border-primary/40"
          >
            <KeyRound size={12} className="text-primary shrink-0" />
            <span className="whitespace-nowrap">Saved password</span>
          </button>
          {passwordMenu && (
            <div
              role="menu"
              aria-label="Saved passwords"
              onKeyDown={(e) => { if (e.key === 'Escape') { e.stopPropagation(); setPasswordMenu(null); xtermRef.current?.focus(); } }}
              className="w-64 max-w-full max-h-60 overflow-y-auto rounded-md bg-zinc-900 border border-white/10 shadow-xl p-1"
            >
              {passwordMenu.length === 0 ? (
                <div className="px-2 py-1.5 text-[11px] text-zinc-400">
                  No saved passwords. Save one with the server or under Logins.
                </div>
              ) : passwordMenu.map((p) => (
                <button
                  key={p.serverId != null ? `s${p.serverId}` : `c${p.credentialId}`}
                  type="button"
                  role="menuitem"
                  onClick={() => void typeSavedPassword(p)}
                  className="w-full text-left px-2 py-1.5 rounded hover:bg-white/[0.06] focus:bg-white/[0.06] focus:outline-none"
                >
                  <div className="text-[11.5px] text-zinc-100 truncate">{p.label}</div>
                  {p.detail && <div className="text-[10px] text-zinc-500 truncate">{p.detail}</div>}
                </button>
              ))}
            </div>
          )}
        </div>
      )}
      {isMobile && !disabled && (
        <MobileKeyBar
          modifiers={modifiers}
          onToggleModifier={toggleModifier}
          onSpecialKey={sendSpecialKey}
        />
      )}
      {/* Clipboard toast — bottom-right of the terminal pane, never wider
          than the pane (a split or a wide tool panel can leave it narrow).
          Pointer-events off so a stray hover never blocks selection /
          right-click. */}
      {toast && (
        <div className="absolute bottom-3 right-3 z-20 max-w-[calc(100%-1.5rem)] pointer-events-none">
          <span
            className={`block truncate px-2.5 py-1 rounded text-[10.5px] font-mono uppercase tracking-wider border ${
              toast.tone === 'err'
                ? 'bg-rose-500/15 border-rose-500/30 text-rose-300'
                : 'bg-emerald-500/15 border-emerald-500/30 text-emerald-300'
            }`}
          >
            {toast.msg}
          </span>
        </div>
      )}
      <HistorySearchOverlay
        isOpen={showHistory}
        onClose={() => {
          setShowHistory(false);
          // Return focus to xterm so the user can keep typing at the prompt
          // instead of losing keystrokes into the void.
          xtermRef.current?.focus();
        }}
        filterServerId={serverId > 0 ? serverId : null}
        onInsert={(command, execute) => {
          if (disabledRef.current) return;
          const payload = execute ? command + "\r" : command;
          const bytes = Array.from(new TextEncoder().encode(payload));
          invoke('write_terminal_data', { terminalId, data: bytes }).catch(console.error);
          // Mirror the inserted command into the local buffer so the next
          // Enter (if the user typed one manually) still logs correctly.
          if (!execute) {
            commandBufRef.current = command;
            commandBufPastedRef.current = false;
          }
        }}
      />
      {/* Disabled badge: NON-blocking. xterm stays interactive for scroll +
          mouse selection so the user can copy whatever was on screen at the
          time the session dropped. Keystrokes are still swallowed at the
          source via `disabledRef` in the onData callback — no input reaches
          a dead PTY. `pointer-events-none` keeps the badge from eating the
          select-drag that starts under it. */}
      {/* One line: centred like this it only gets half the width, and wrapped
          it covered two lines of the terminal on a phone, where the banner
          above already says the session is down. */}
      {disabled && (
        <div className="absolute top-2 left-1/2 -translate-x-1/2 z-10 pointer-events-none">
          <span className="whitespace-nowrap px-2.5 py-1 rounded text-[10.5px] font-mono uppercase tracking-wider border bg-rose-500/15 border-rose-500/30 text-rose-300 shadow-lg">
            <span className="sm:hidden">Read-only</span>
            <span className="hidden sm:inline">Disconnected — buffer is read-only</span>
          </span>
        </div>
      )}
    </div>
  );
};

export default TerminalView;