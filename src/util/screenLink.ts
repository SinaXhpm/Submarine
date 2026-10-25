// How the link to the server of an RDP (built-in client) or VNC tab is doing,
// as the backend measures it (screen_link.rs, `LinkStats`, sent to the tab
// every 2 seconds while it is connected). The delay is a round trip through
// everything on the way, SSH servers and VPNs included: the server is asked to
// draw a corner of the screen again, and its answer is timed.

export interface LinkStats {
  /** The middle one of the last few round trips, in ms; null before the first answer. */
  rttMs: number | null;
  /** How long the server has said nothing at all since the question now out was asked, in ms. */
  waitingMs: number;
  /** Bytes a second from and to the server, lately. */
  inRate: number;
  outRate: number;
  /** All the bytes of the session so far. */
  bytesIn: number;
  bytesOut: number;
  /** The server takes the question the delay is measured with. */
  canMeasure: boolean;
  /** The server can stop sending the picture while the tab is hidden. */
  canPause: boolean;
  /** It has, at the tab's request. */
  paused: boolean;
}

export type LinkQuality = "good" | "fair" | "weak" | "silent" | "unknown";

/** Nothing at all from the server this long after a question: it isn't answering. */
export const SILENT_AFTER_MS = 4000;
/** Below this round trip a remote desktop feels close at hand. */
export const GOOD_BELOW_MS = 150;
/** From this one on, every click and key shows the wait. */
export const WEAK_FROM_MS = 300;

export function linkQuality(s: LinkStats | null): LinkQuality {
  if (!s) return "unknown";
  if (s.waitingMs >= SILENT_AFTER_MS) return "silent";
  if (s.rttMs === null) return "unknown";
  if (s.rttMs < GOOD_BELOW_MS) return "good";
  if (s.rttMs < WEAK_FROM_MS) return "fair";
  return "weak";
}

/** The word on the tab's connection mark. */
export const LINK_WORDS: Record<LinkQuality, string> = {
  good: "Good",
  fair: "Fair",
  weak: "Weak",
  silent: "Not answering",
  unknown: "Measuring",
};

/** "820 B", "35 KB", "1.2 MB", "2.05 GB" (decimal units, as networks count). */
export function formatBytes(n: number): string {
  if (n < 1000) return `${Math.round(n)} B`;
  if (n < 1_000_000) return `${(n / 1000).toFixed(n < 10_000 ? 1 : 0)} KB`;
  if (n < 1_000_000_000) return `${(n / 1_000_000).toFixed(1)} MB`;
  return `${(n / 1_000_000_000).toFixed(2)} GB`;
}

export const formatRate = (bytesPerSecond: number) => `${formatBytes(bytesPerSecond)}/s`;
