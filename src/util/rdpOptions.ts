// RDP options (issue #3): how an RDP server's window looks and what it shares
// with the server. This device's own settings (localStorage), kept by the
// server's sync id like shellSync.ts: a window size belongs to this screen,
// and in a shared profile nobody else may switch on sharing this computer's
// files. The RDP tab hands them to the backend (src-tauri/src/rdp.rs).

export interface RdpOptions {
  /** Where it opens: in this computer's RDP app, or in its Submarine tab (the
   *  built-in client: experimental, built from an IronRDP commit rather than
   *  a release, so only when chosen). The options below are the RDP app's. */
  app: "builtin" | "system";
  fullScreen: boolean;
  /** The window's size when not full screen. */
  width: number;
  height: number;
  clipboard: boolean;
  /** This computer's drives (with Remmina: the home folder). */
  drives: boolean;
  /** The built-in client may log in to this server without NLA. Off: a server
   *  that doesn't do NLA is refused before anything is sent (audit 2, #4). */
  allowNoNla: boolean;
}

export const RDP_OPTIONS_KEY = "submarine-rdp-options";

// What Remote Desktop Connection does on its own, files kept private.
export const DEFAULT_RDP_OPTIONS: RdpOptions = { app: "system", fullScreen: true, width: 1600, height: 900, clipboard: true, drives: false, allowNoNla: false };

/** How the built-in client's refusal of a server without NLA starts: the tab
 *  offers the one-click opt-in for it. Kept equal to `rdp_client::NO_NLA`
 *  (a Rust test checks it). */
export const RDP_NO_NLA_PREFIX = "This server doesn't use NLA";

/** What the tab and the form say about the built-in client. */
export const RDP_BUILTIN_EXPERIMENTAL =
  "Experimental: the built-in RDP client is built from an IronRDP commit, not an official IronRDP release, and may be unstable.";

export const RDP_WINDOW_SIZES: [number, number][] = [
  [1280, 720],
  [1366, 768],
  [1600, 900],
  [1920, 1080],
  [2560, 1440],
];

const readAll = (): Record<string, unknown> => {
  try {
    const raw = localStorage.getItem(RDP_OPTIONS_KEY);
    const parsed = raw ? JSON.parse(raw) : null;
    return parsed && typeof parsed === "object" && !Array.isArray(parsed) ? parsed : {};
  } catch {
    return {};
  }
};

/** A server's options on this device; the defaults where none are kept. */
export function readRdpOptions(uuid?: string | null): RdpOptions {
  const kept = uuid ? (readAll()[uuid] as Partial<RdpOptions> | undefined) : undefined;
  const o = { ...DEFAULT_RDP_OPTIONS };
  if (kept && typeof kept === "object") {
    if (kept.app === "builtin" || kept.app === "system") o.app = kept.app;
    if (typeof kept.fullScreen === "boolean") o.fullScreen = kept.fullScreen;
    if (Number.isFinite(kept.width)) o.width = Number(kept.width);
    if (Number.isFinite(kept.height)) o.height = Number(kept.height);
    if (typeof kept.clipboard === "boolean") o.clipboard = kept.clipboard;
    if (typeof kept.drives === "boolean") o.drives = kept.drives;
    if (typeof kept.allowNoNla === "boolean") o.allowNoNla = kept.allowNoNla;
  }
  return o;
}

export function writeRdpOptions(uuid: string, options: RdpOptions): void {
  const all = readAll();
  all[uuid] = options;
  try {
    localStorage.setItem(RDP_OPTIONS_KEY, JSON.stringify(all));
  } catch {
    // Best-effort, same as the other per-device prefs.
  }
}

/** One line for the RDP tab: "Full screen · clipboard shared · files not shared". */
export function describeRdpOptions(o: RdpOptions): string {
  return [
    o.fullScreen ? "Full screen" : `Window ${o.width}×${o.height}`,
    o.clipboard ? "clipboard shared" : "clipboard not shared",
    o.drives ? "files shared" : "files not shared",
  ].join(" · ");
}
