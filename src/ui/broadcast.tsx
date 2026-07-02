import { createContext, useCallback, useContext, useEffect, useMemo, useRef, useState } from "react";

// ─────────────────────────────────────────────────────────────────────────────
// Multi-exec broadcast (Termius / MobaXterm style)
//
// A global context tracks:
//   • whether broadcast is armed at all,
//   • which session ids are selected as targets,
//   • the currently-active terminal id per session (so fan-out knows *which*
//     of the session's tabs is "the shell to write to" — a session with
//     multiple terminal tabs picks the visible one).
//
// TerminalView reads from `useBroadcast()` inside its onData handler. When
// broadcast is on and there are 2+ targets, each keystroke is written to its
// own PTY (as always) AND fanned out to every target session's active
// terminal id EXCEPT the source (avoids the user's typing showing up twice
// locally). Backend errors are swallowed — a dead target should silently
// skip, not blow the whole write pipeline.
// ─────────────────────────────────────────────────────────────────────────────

interface BroadcastState {
  enabled: boolean;
  targetSessionIds: Set<string>;
  /** session_id → currently-active terminal_id for that session. Updated by
   *  each TerminalView on mount + whenever it becomes the active tab. */
  sessionTerminalMap: Record<string, string>;
}

interface BroadcastActions {
  enable: () => void;
  disable: () => void;
  toggleEnabled: () => void;
  toggleTarget: (sessionId: string) => void;
  setTargets: (ids: string[]) => void;
  selectAll: (allSessionIds: string[]) => void;
  selectNone: () => void;
  /** Called by TerminalView when it mounts (and it's the active tab) or when
   *  the parent's active-tab flag flips to it. Idempotent. */
  registerActiveTerminal: (sessionId: string, terminalId: string) => void;
  /** Called by TerminalView on unmount, if it was the active terminal for
   *  its session. Skips the update if some other TerminalView already
   *  claimed the slot (avoids racing during tab-swap teardown). */
  unregisterActiveTerminal: (sessionId: string, terminalId: string) => void;
  /** Also called on session close from the tab strip so a dead session id
   *  doesn't linger in targets / sessionTerminalMap and eat a broadcast
   *  slot with nowhere to go. */
  removeSession: (sessionId: string) => void;
  /** Ref-based accessor so onData's long-lived closure inside xterm always
   *  reads the latest state without re-binding every keystroke. */
  stateRef: React.MutableRefObject<BroadcastState>;
}

type BroadcastContextValue = BroadcastState & BroadcastActions;

const BroadcastContext = createContext<BroadcastContextValue | null>(null);

export const BroadcastProvider = ({ children }: { children: React.ReactNode }) => {
  const [enabled, setEnabled] = useState(false);
  const [targetSessionIds, setTargetSessionIds] = useState<Set<string>>(() => new Set());
  const [sessionTerminalMap, setSessionTerminalMap] = useState<Record<string, string>>({});

  // Mirror state into a ref so TerminalView's xterm.onData closure — which is
  // bound once at mount and captures its context snapshot — can always see
  // the latest broadcast targets without re-binding. This mirrors how
  // `disabledRef` / `modifiersRef` work inside TerminalView itself.
  const stateRef = useRef<BroadcastState>({ enabled, targetSessionIds, sessionTerminalMap });
  useEffect(() => {
    stateRef.current = { enabled, targetSessionIds, sessionTerminalMap };
  }, [enabled, targetSessionIds, sessionTerminalMap]);

  const enable = useCallback(() => setEnabled(true), []);
  const disable = useCallback(() => setEnabled(false), []);
  const toggleEnabled = useCallback(() => setEnabled(v => !v), []);

  const toggleTarget = useCallback((sessionId: string) => {
    setTargetSessionIds(prev => {
      const next = new Set(prev);
      if (next.has(sessionId)) next.delete(sessionId);
      else next.add(sessionId);
      return next;
    });
  }, []);

  const setTargets = useCallback((ids: string[]) => {
    setTargetSessionIds(new Set(ids));
  }, []);

  const selectAll = useCallback((allSessionIds: string[]) => {
    setTargetSessionIds(new Set(allSessionIds));
  }, []);

  const selectNone = useCallback(() => {
    setTargetSessionIds(new Set());
  }, []);

  const registerActiveTerminal = useCallback((sessionId: string, terminalId: string) => {
    setSessionTerminalMap(prev => {
      if (prev[sessionId] === terminalId) return prev;
      return { ...prev, [sessionId]: terminalId };
    });
  }, []);

  const unregisterActiveTerminal = useCallback((sessionId: string, terminalId: string) => {
    setSessionTerminalMap(prev => {
      // Only clear if we're still the one registered — a new TerminalView
      // may have already claimed the slot during a tab swap.
      if (prev[sessionId] !== terminalId) return prev;
      const { [sessionId]: _drop, ...rest } = prev;
      return rest;
    });
  }, []);

  const removeSession = useCallback((sessionId: string) => {
    setSessionTerminalMap(prev => {
      if (!(sessionId in prev)) return prev;
      const { [sessionId]: _drop, ...rest } = prev;
      return rest;
    });
    setTargetSessionIds(prev => {
      if (!prev.has(sessionId)) return prev;
      const next = new Set(prev);
      next.delete(sessionId);
      return next;
    });
  }, []);

  const value = useMemo<BroadcastContextValue>(() => ({
    enabled,
    targetSessionIds,
    sessionTerminalMap,
    enable,
    disable,
    toggleEnabled,
    toggleTarget,
    setTargets,
    selectAll,
    selectNone,
    registerActiveTerminal,
    unregisterActiveTerminal,
    removeSession,
    stateRef,
  }), [
    enabled, targetSessionIds, sessionTerminalMap,
    enable, disable, toggleEnabled, toggleTarget, setTargets,
    selectAll, selectNone, registerActiveTerminal, unregisterActiveTerminal, removeSession,
  ]);

  return (
    <BroadcastContext.Provider value={value}>
      {children}
    </BroadcastContext.Provider>
  );
};

export const useBroadcast = (): BroadcastContextValue => {
  const ctx = useContext(BroadcastContext);
  if (!ctx) throw new Error("useBroadcast must be used within <BroadcastProvider>");
  return ctx;
};
