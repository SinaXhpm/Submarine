// The AI tab's run events, shared with the session's AI terminal. The tab
// feeds every event of its runs in; the terminal reads them live, and when it
// opens mid-run it first gets what the current run has said so far.

import type { AiEvent } from "./aiApi";

/** Sent on `window` when a session's view should show its AI tab. */
export const AI_FOCUS_EVENT = "submarine-ai-focus";

// Requests for a session whose view isn't there yet: it takes its own when it
// mounts, if that is soon. By session id: the chat to open (null for the one
// open last), and when it was asked.
const focusWaiting = new Map<string, { chatId: number | null; at: number }>();
const FOCUS_WAIT_MS = 60_000;

/** Asks a session's view to show its AI tab, on `chatId` when given. */
export function requestAiFocus(sessionId: string, chatId: number | null) {
  focusWaiting.set(sessionId, { chatId, at: Date.now() });
  window.dispatchEvent(new CustomEvent(AI_FOCUS_EVENT, { detail: { sessionId } }));
}

/** The request waiting for this session, which it clears: `undefined` when there is none. */
export function takeAiFocus(sessionId: string): number | null | undefined {
  const req = focusWaiting.get(sessionId);
  if (!req) return undefined;
  focusWaiting.delete(sessionId);
  return Date.now() - req.at < FOCUS_WAIT_MS ? req.chatId : undefined;
}

/** Drops every request: the profile is closing, and ids mean other things in the next. */
export function clearAiFocus() {
  focusWaiting.clear();
}

// Each session's feed, so a run of that server's chat started on the AI page
// shows in the session's AI terminal too.
const feeds = new Map<string, AiFeed>();

/** Makes a session's feed known; the function it returns takes it back. */
export function registerFeed(sessionId: string, feed: AiFeed): () => void {
  feeds.set(sessionId, feed);
  return () => {
    if (feeds.get(sessionId) === feed) feeds.delete(sessionId);
  };
}

export const feedOf = (sessionId: string): AiFeed | undefined => feeds.get(sessionId);

/** Forgets every session's feed: the profile is closing, and the next one's
 *  sessions have the same ids. */
export function clearFeeds() {
  feeds.clear();
}

export interface AiFeed {
  emit: (e: AiEvent) => void;
  subscribe: (fn: (e: AiEvent) => void) => () => void;
  /** The current run's events so far (empty between runs). */
  current: () => AiEvent[];
  /** The chat whose runs this is, for the terminal's replay of earlier ones. */
  chatId: () => number | null;
  setChat: (id: number | null) => void;
  /** Called when the chat changes: the terminal starts over. */
  onChat: (fn: (id: number | null) => void) => () => void;
}

/** How many events of one run are kept for a terminal that opens late. */
const KEEP = 20000;

export const createFeed = (): AiFeed => {
  const subs = new Set<(e: AiEvent) => void>();
  const chatSubs = new Set<(id: number | null) => void>();
  let run: AiEvent[] = [];
  let chat: number | null = null;
  return {
    emit: (e) => {
      if (e.type === "run_started") run = [];
      if (run.length < KEEP) run.push(e);
      subs.forEach((f) => f(e));
      if (e.type === "run_finished") run = [];
    },
    subscribe: (fn) => {
      subs.add(fn);
      return () => { subs.delete(fn); };
    },
    current: () => run,
    chatId: () => chat,
    setChat: (id) => {
      if (id === chat) return;
      chat = id;
      run = [];
      chatSubs.forEach((f) => f(id));
    },
    onChat: (fn) => {
      chatSubs.add(fn);
      return () => { chatSubs.delete(fn); };
    },
  };
};
