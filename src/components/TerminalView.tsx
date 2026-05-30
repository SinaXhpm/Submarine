import { useEffect, useRef, useState } from 'react';
import { Terminal } from 'xterm';
import { FitAddon } from 'xterm-addon-fit';
import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import 'xterm/css/xterm.css';

const TerminalView = ({
  sessionId,
  terminalId,
  disabled = false,
  isActive = true,
}: {
  sessionId: string;
  terminalId: string;
  disabled?: boolean;
  /// Tells us when this terminal is the visible one in its parent. Used to
  /// trigger a refit + refresh whenever we become visible — without this,
  /// xterm's internal canvas can be left holding stale glyphs from before
  /// the parent's display/opacity change and the prompt appears garbled
  /// until the user types something.
  isActive?: boolean;
}) => {
  const terminalRef = useRef<HTMLDivElement>(null);
  const xtermRef = useRef<Terminal | null>(null);
  const fitAddonRef = useRef<FitAddon | null>(null);
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
    term.open(terminalRef.current);
    
    setTimeout(() => {
      fitAddon.fit();
      
      // Start PTY Session with correct dimensions only ONCE
      if (!openedRef.current) {
        openedRef.current = true;
        invoke('open_terminal', { 
          sessionId, 
          terminalId,
          cols: term.cols || 80,
          rows: term.rows || 24
        }).catch(e => {
          term.writeln(`\x1b[31mFailed to open terminal: ${e}\x1b[0m`);
        });
      }
    }, 50);

    xtermRef.current = term;

    // Handle Input — swallow keystrokes once the session is disconnected so
    // they don't pile up against a dead backend channel.
    const onDataDisposable = term.onData((data) => {
      if (disabledRef.current) return;
      invoke('write_terminal_data', {
        terminalId,
        data: Array.from(new TextEncoder().encode(data))
      }).catch(console.error);
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
      invoke('write_terminal_data', {
        terminalId,
        data: Array.from(new TextEncoder().encode(text)),
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
    });

    // Resize Observer for container resizing. Coalesce via rAF so a
    // drag burst (60Hz worth of layout-change events) collapses to one
    // fit+refresh per frame — without this the IPC channel to the
    // backend would be flooded with resize_terminal calls during any
    // window drag.
    //
    // Skip the refit entirely when this terminal isn't the visible tab.
    // Hidden terminals stay mounted with non-zero `absolute inset-0`
    // dimensions, so any session layout change (toggling the tool pane,
    // dragging the divider, resizing the window) fires this observer on
    // every tab at once. If we let it run, fit() reports a new cols/rows,
    // term.onResize → invoke('resize_terminal') → SIGWINCH lands on the
    // hidden tab's PTY, bash's checkwinsize redraws its prompt, and the
    // user comes back to a stack of duplicated `[user@host]#` lines.
    // The become-visible effect refits and refreshes when the tab is
    // shown again, so nothing is lost by gating here.
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

    const container = terminalRef.current;
    return () => {
      window.removeEventListener('submarine-settings-changed', handleSettingsChange);
      resizeObserver.disconnect();
      onDataDisposable.dispose();
      onResizeDisposable.dispose();
      if (container) {
        container.removeEventListener('mouseup', onMouseUp);
        container.removeEventListener('contextmenu', onContextMenu);
      }
      if (toastTimerRef.current) window.clearTimeout(toastTimerRef.current);
      unlistenPromise.then(unlisten => unlisten());
      term.dispose();
      invoke('close_terminal', { terminalId }).catch(console.error);
    };
  }, [sessionId, terminalId]);

  return (
    <div className="h-full w-full bg-[#09090b] p-2 pr-2 pb-0 relative">
      <div
        ref={terminalRef}
        className="h-full w-full overflow-hidden select-text"
      />
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
      {/* Disabled overlay: blocks pointer events on top of xterm and dims the
          output. Keyboard input is gated separately via `disabledRef` because
          xterm grabs key events at a lower level. */}
      {disabled && (
        <div className="absolute inset-0 bg-black/55 backdrop-blur-[1px] flex items-center justify-center text-zinc-300 text-xs font-mono tracking-wider uppercase select-none pointer-events-auto z-10">
          <span className="px-3 py-1.5 bg-red-500/15 border border-red-500/30 rounded text-red-300">
            Session disconnected — reconnect to resume
          </span>
        </div>
      )}
    </div>
  );
};

export default TerminalView;