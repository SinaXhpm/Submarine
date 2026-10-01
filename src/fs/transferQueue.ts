import { useSyncExternalStore } from "react";

// Items of a download/upload batch that have no live transfer row yet.
// Transfers run one at a time per session (see `acquireTransferSlot`), so
// every item that has not got the slot yet, from this batch or another one,
// waits here and the transfers bar shows it as "Queued". Each item gets
// its transfer id up front and FilePanel passes it to the backend command, so
// the bar links the item to its `sftp-transfer-*` events by id and can cancel
// it while it is still starting. The item being run stays here as "starting"
// until the invoke settles; the bar hides it once events for its id arrive.

export interface QueuedTransfer {
  /** Transfer id the backend command is (or will be) run with. */
  id: string;
  name: string;
  kind: "upload" | "download";
  /** Bytes, when known. Folders and OS-dropped paths have none. */
  size?: number;
  isDir: boolean;
  state: "queued" | "starting";
}

type QueueInput = Omit<QueuedTransfer, "id" | "state">;

const EMPTY: QueuedTransfer[] = [];
const queues = new Map<string, QueuedTransfer[]>();
const listeners = new Set<() => void>();
// One controller per item, aborted when the item leaves the queue for any
// reason (removed from the bar, or its batch step finished). An overwrite
// prompt still open for a removed item closes on it.
const controllers = new Map<string, AbortController>();
let seq = 0;

const newTransferId = () => `q${Date.now()}-${++seq}-${Math.random().toString(36).slice(2, 8)}`;

const update = (sessionId: string, fn: (cur: QueuedTransfer[]) => QueuedTransfer[]) => {
  const cur = queues.get(sessionId) ?? EMPTY;
  const next = fn(cur);
  if (next === cur) return;
  if (next.length === 0) queues.delete(sessionId);
  else queues.set(sessionId, next);
  listeners.forEach((l) => l());
};

/** Adds a batch as "queued" and returns the transfer id of each item, in order. */
export function enqueueTransfers(sessionId: string, items: QueueInput[]): string[] {
  const added = items.map((it) => ({ ...it, id: newTransferId(), state: "queued" as const }));
  for (const it of added) {
    controllers.set(it.id, new AbortController());
    turns.set(it.id, acquireTransferSlot(sessionId));
  }
  update(sessionId, (cur) => [...cur, ...added]);
  return added.map((it) => it.id);
}

/** Aborts once the item has left the queue (already aborted if it has). */
export function queueSignal(id: string): AbortSignal {
  return controllers.get(id)?.signal ?? AbortSignal.abort();
}

export function getQueued(sessionId: string): QueuedTransfer[] {
  return queues.get(sessionId) ?? EMPTY;
}

export function isQueued(sessionId: string, id: string): boolean {
  return getQueued(sessionId).some((it) => it.id === id);
}

/**
 * Marks a queued item as starting. Returns false when the item is gone,
 * which means the user removed it from the transfers bar while it waited.
 */
export function startQueued(sessionId: string, id: string): boolean {
  if (!isQueued(sessionId, id)) return false;
  update(sessionId, (list) => list.map((it) => (it.id === id ? { ...it, state: "starting" } : it)));
  return true;
}

export function dropQueued(sessionId: string, ids: string[]) {
  if (ids.length === 0) return;
  const drop = new Set(ids);
  for (const id of drop) {
    controllers.get(id)?.abort();
    controllers.delete(id);
    // Its turn was never taken: pass the slot on as soon as the turn comes.
    const turn = turns.get(id);
    if (turn) {
      turns.delete(id);
      turn.then((release) => release());
    }
  }
  update(sessionId, (cur) => {
    const next = cur.filter((it) => !drop.has(it.id));
    return next.length === cur.length ? cur : next;
  });
}

// One transfer slot per session. Every upload/download started from the UI
// (batches from any panel, OS drops, cross-pane drags) takes the slot for
// one item and releases it when that item settles, so two transfers never
// run at once. Without it, two batches aimed at the same path both pass the
// backend's EXISTS check and then write into the same file together.
// Waiters are served in the order they asked (FIFO). Editor saves and opens
// start in Rust and are kept apart from these by the backend's per-file
// lock instead.
const slotTails = new Map<string, Promise<void>>();

// Each item's turn in the slot, reserved when it is queued (not when its
// batch gets to it), so the rows in the bar run in the order they are
// listed: a batch queued later never overtakes the tail of an earlier one.
const turns = new Map<string, Promise<() => void>>();

/**
 * Resolves with the slot's release function when it is this item's turn,
 * or with null when the item has left the queue before its turn was taken
 * (its turn is then passed on automatically).
 */
export function waitForTurn(id: string): Promise<(() => void) | null> {
  const turn = turns.get(id);
  if (!turn) return Promise.resolve(null);
  turns.delete(id);
  return turn;
}

/** Resolves with a release function once the session's slot is free. */
export function acquireTransferSlot(sessionId: string): Promise<() => void> {
  const prev = slotTails.get(sessionId) ?? Promise.resolve();
  let release!: () => void;
  const held = new Promise<void>((resolve) => { release = resolve; });
  const tail = prev.then(() => held);
  slotTails.set(sessionId, tail);
  // Forget the chain once it is idle, so the map does not grow per session.
  tail.then(() => { if (slotTails.get(sessionId) === tail) slotTails.delete(sessionId); });
  let released = false;
  return prev.then(() => () => {
    if (released) return;
    released = true;
    release();
  });
}

export function useQueuedTransfers(sessionId: string): QueuedTransfer[] {
  return useSyncExternalStore(
    (cb) => {
      listeners.add(cb);
      return () => { listeners.delete(cb); };
    },
    () => getQueued(sessionId),
  );
}
