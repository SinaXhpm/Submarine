import { useEffect, useRef, useState } from 'react';
import { Terminal } from 'xterm';
import { FitAddon } from 'xterm-addon-fit';
import { SearchAddon, ISearchOptions } from 'xterm-addon-search';
import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import 'xterm/css/xterm.css';
import { useIsNarrow } from '../hooks/useViewport';
import { MobileKeyBar, ModifiersState, ModKey } from './MobileKeyBar';
import { useBroadcast } from '../ui/broadcast';
import HistorySearchOverlay from './HistorySearchOverlay';

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

const TerminalView = ({
  sessionId,
  terminalId,
  disabled = false,
  isActive = true,
  containerExec,
  connectionEpoch = 0,
  serverId = 0,
  serverName = "",
  attachOnly = false,
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
  /// When set, this terminal runs `docker exec -it <container> <shell>`
  /// on the SSH host instead of the user's login shell. Used by the
  /// Docker tab in InfoPanel to open an interactive session inside a
  /// specific container without leaving the app.
  containerExec?: { container: string; useSudo: boolean };
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
}) => {
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
  const sendSpecialKey = (key: "esc" | "tab") => {
    if (disabledRef.current) return;
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
    const ce = containerExecRef.current;
    if (ce) {
      invoke('open_container_terminal', {
        sessionId,
        terminalId,
        container: ce.container,
        cols: term.cols || 80,
        rows: term.rows || 24,
        useSudo: ce.useSudo,
      }).catch(e => {
        term.writeln(`\x1b[31mReconnect into container failed: ${e}\x1b[0m`);
      });
    } else {
      invoke('open_terminal', {
        sessionId,
        terminalId,
        cols: term.cols || 80,
        rows: term.rows || 24,
      }).catch(e => {
        term.writeln(`\x1b[31mReconnect failed: ${e}\x1b[0m`);
      });
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [connectionEpoch]);

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
    });
    return () => cancelAnimationFrame(id);
  }, [isActive]);

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
  const [toast, setToast] = useState<{ msg: string; tone: "ok" | "err" } | null>(null);
  const toastTimerRef = useRef<number | null>(null);
  const notify = (msg: string, tone: "ok" | "err" = "ok") => {
    setToast({ msg, tone });
    if (toastTimerRef.current) window.clearTimeout(toastTimerRef.current);
    toastTimerRef.current = window.setTimeout(() => setToast(null), 1400);
  };

  useEffect(() => {
    if (!terminalRef.current) return;

    const term = new Terminal({
      cursorBlink: true,
      fontSize: parseInt(localStorage.getItem('submarine-terminal-font-size') || '14'),
      fontFamily: 'Consolas, "Courier New", monospace',
      theme: {
        background: '#09090b',
        foreground: '#e4e4e7',
        cursor: '#60a5fa',
        selectionBackground: 'rgba(96, 165, 250, 0.3)',
      },
      allowProposedApi: true
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
        if (containerExec) {
          invoke('open_container_terminal', {
            sessionId,
            terminalId,
            container: containerExec.container,
            cols: term.cols || 80,
            rows: term.rows || 24,
            useSudo: containerExec.useSudo,
          }).catch(e => {
            term.writeln(`\x1b[31mFailed to attach to container: ${e}\x1b[0m`);
          });
        } else {
          invoke('open_terminal', {
            sessionId,
            terminalId,
            cols: term.cols || 80,
            rows: term.rows || 24
          }).catch(e => {
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
    const onDataDisposable = term.onData((data) => {
      if (disabledRef.current) return;
      const mods = modifiersRef.current;
      const hasMod = mods.ctrl !== "off" || mods.alt !== "off";
      let bytes: number[];
      if (hasMod && data.length === 1) {
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
      // not parse the shell's echo, so pastes / arrow-key edits will
      // over-count or mis-count. Skip anything under 3 chars to weed out
      // "y\r" prompts and stray Enters.
      for (let i = 0; i < data.length; i++) {
        const ch = data.charCodeAt(i);
        if (ch === 0x0d /* CR */ || ch === 0x0a /* LF */) {
          const line = commandBufRef.current.trim();
          commandBufRef.current = "";
          // Do NOT persist what was typed at a no-echo password/passphrase
          // prompt — otherwise sudo/su/mysql -p/gpg secrets land verbatim in
          // searchable history. The onData handler can't see the PTY echo
          // mode, so we infer it from the remote output tail.
          if (line.length >= 3 && !looksLikePasswordPrompt(recentOutputRef.current)) {
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
    });

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
      const text = buildSelectedText();
      if (!text) return;
      const ok = await writeClipboard(text);
      if (ok) notify('Copied');
    };
    // Only copy when the release actually lands on the text grid. xterm's
    // text rows live inside .xterm-screen in both renderers; releasing on
    // padding, the viewport scrollbar, or anything outside that subtree
    // means the user didn't finish on text — drop the copy so a stray
    // click on the gutter doesn't clobber the clipboard.
    const onMouseUp = (ev: MouseEvent) => {
      const target = ev.target as HTMLElement | null;
      if (!target || !target.closest('.xterm-screen')) return;
      void copySelectionIfAny();
    };
    // Right-click → paste clipboard into the PTY. preventDefault swallows the
    // platform context menu so the user gets the terminal-style behavior they
    // asked for. Disabled sessions silently drop the paste (matches onData).
    const onContextMenu = async (ev: MouseEvent) => {
      ev.preventDefault();
      if (disabledRef.current) return;
      let text = '';
      try { text = await navigator.clipboard.readText(); }
      catch { notify('Clipboard read denied', 'err'); return; }
      if (!text) return;
      // Normalize line endings the way xterm and OpenSSH do: collapse
      // CRLF / lone LF to a single CR. A Windows-style clipboard pastes
      // `\r\n` per line — the PTY sees CR (Enter) followed by LF (Enter
      // again), and the shell runs the previous command twice and inserts
      // a blank line between every pair. Stripping LF puts paste back on
      // the standard terminal contract: one Enter per line break.
      const normalized = text.replace(/\r\n/g, '\r').replace(/\n/g, '\r');
      invoke('write_terminal_data', {
        terminalId,
        data: Array.from(new TextEncoder().encode(normalized)),
      }).catch(console.error);
      notify('Pasted');
    };
    terminalRef.current.addEventListener('mouseup', onMouseUp);
    terminalRef.current.addEventListener('contextmenu', onContextMenu);

    // Handle Resize
    const onResizeDisposable = term.onResize(({ cols, rows }) => {
      invoke('resize_terminal', { terminalId, cols, rows }).catch(console.error);
    });

    // Handle Output with proper async cleanup for StrictMode
    const unlistenPromise = listen(`terminal-output-${terminalId}`, (event: any) => {
      const data = new Uint8Array(event.payload);
      term.write(data);
      // Keep a short tail of decoded output so command-history capture can
      // recognize a no-echo password prompt and skip the typed secret.
      try {
        const txt = new TextDecoder().decode(data);
        const combined = recentOutputRef.current + txt;
        recentOutputRef.current = combined.length > 512 ? combined.slice(-512) : combined;
      } catch { /* decode hiccup on a split multibyte chunk — ignore */ }
    });

    // Coalesce resize bursts via rAF, and skip the refit on hidden tabs
    // so SIGWINCH doesn't leak to background PTYs (the become-visible
    // effect refits when the tab is shown again).
    let rafId = 0;
    const resizeObserver = new ResizeObserver(() => {
      if (!xtermRef.current) return;
      if (!isActiveRef.current) return;
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
    
    // Handle Settings Change
    const handleSettingsChange = () => {
      const newSize = parseInt(localStorage.getItem('submarine-terminal-font-size') || '14');
      if (term.options.fontSize !== newSize) {
        term.options.fontSize = newSize;
        fitAddon.fit();
      }
    };
    window.addEventListener('submarine-settings-changed', handleSettingsChange);

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
      window.removeEventListener('submarine-settings-changed', handleSettingsChange);
      resizeObserver.disconnect();
      onDataDisposable.dispose();
      onResizeDisposable.dispose();
      if (container) {
        container.removeEventListener('mouseup', onMouseUp);
        container.removeEventListener('contextmenu', onContextMenu);
      }
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
      }
    };
  }, [sessionId, terminalId]);

  return (
    // flex-col so the MobileKeyBar can pin to the bottom of the terminal
    // pane (between xterm and the system soft keyboard). On desktop the bar
    // isn't rendered at all, so the xterm child claims the full height as
    // before.
    <div className="h-full w-full bg-[#09090b] p-2 pr-2 pb-0 relative flex flex-col">
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
      {isMobile && !disabled && (
        <MobileKeyBar
          modifiers={modifiers}
          onToggleModifier={toggleModifier}
          onSpecialKey={sendSpecialKey}
        />
      )}
      {/* Clipboard toast — bottom-right of the terminal pane. Pointer-events
          off so a stray hover never blocks selection / right-click. */}
      {toast && (
        <div className="absolute bottom-3 right-3 z-20 pointer-events-none">
          <span
            className={`px-2.5 py-1 rounded text-[10.5px] font-mono uppercase tracking-wider border ${
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
          if (!execute) commandBufRef.current = command;
        }}
      />
      {/* Disabled badge: NON-blocking. xterm stays interactive for scroll +
          mouse selection so the user can copy whatever was on screen at the
          time the session dropped. Keystrokes are still swallowed at the
          source via `disabledRef` in the onData callback — no input reaches
          a dead PTY. `pointer-events-none` keeps the badge from eating the
          select-drag that starts under it. */}
      {disabled && (
        <div className="absolute top-2 left-1/2 -translate-x-1/2 z-10 pointer-events-none">
          <span className="px-2.5 py-1 rounded text-[10.5px] font-mono uppercase tracking-wider border bg-rose-500/15 border-rose-500/30 text-rose-300 shadow-lg">
            Disconnected — buffer is read-only
          </span>
        </div>
      )}
    </div>
  );
};

export default TerminalView;