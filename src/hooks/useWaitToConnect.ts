import { useEffect, useState } from "react";

/** A tab opened when the profile was unlocked, without connecting (its
 *  server's "At startup: Open the tab"): `waiting` until Connect is pressed in
 *  the tab or in its menu (Connect there sends `session-reconnect-<id>`). A
 *  view starts its connection once `waiting` is false. Any other tab doesn't
 *  wait. */
export function useWaitToConnect(session: { id: string; openOnly?: boolean }): { waiting: boolean; connectNow: () => void } {
  const [waiting, setWaiting] = useState(() => !!session.openOnly);
  useEffect(() => {
    if (!waiting) return;
    const evt = `session-reconnect-${session.id}`;
    const go = () => setWaiting(false);
    window.addEventListener(evt, go);
    return () => window.removeEventListener(evt, go);
  }, [waiting, session.id]);
  return { waiting, connectNow: () => setWaiting(false) };
}
