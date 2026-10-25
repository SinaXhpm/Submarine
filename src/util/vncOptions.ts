// VNC options: how a VNC server's screen is asked for (speed and quality). This
// device's own settings (localStorage), kept by the server's sync id like
// rdpOptions.ts: a phone on a mobile network wants other choices than a
// desktop on the same LAN as the server. The VNC tab hands them to the backend
// (src-tauri/src/vnc_client.rs, `Options`).

export interface VncOptions {
  /** Which way the server packs the screen. Automatic suits most links. */
  encoding: VncEncoding;
  /** Bits a pixel on the wire: less is less to send on a slow link. */
  colors: VncColors;
  /** The most updates asked for in a second (60: no limit). */
  maxFps: VncMaxFps;
  /** Share the clipboard: text copied on the server comes to this device, and
   *  this device's goes to the server when pasted there (plain ASCII only:
   *  `vnc_client::paste_refusal`). Not part of the speed and quality. */
  clipboard: boolean;
}

export type VncEncoding = "auto" | "tight" | "zrle" | "raw";
export type VncColors = 32 | 16 | 8;
export type VncMaxFps = 60 | 30 | 15 | 5;

export const VNC_OPTIONS_KEY = "submarine-vnc-options";

// What the VNC tab always asked for.
export const DEFAULT_VNC_OPTIONS: VncOptions = { encoding: "auto", colors: 32, maxFps: 60, clipboard: true };

/** The choices as the form lists them. */
export const VNC_ENCODING_CHOICES: { value: VncEncoding; label: string }[] = [
  { value: "auto", label: "Automatic" },
  { value: "tight", label: "Tight (full colour only)" },
  { value: "zrle", label: "ZRLE (any colour depth)" },
  { value: "raw", label: "Raw (fast network only)" },
];
export const VNC_COLOR_CHOICES: { value: VncColors; label: string }[] = [
  { value: 32, label: "Full colour (32-bit)" },
  { value: 16, label: "65,536 colours (16-bit)" },
  { value: 8, label: "256 colours (8-bit)" },
];
export const VNC_FPS_CHOICES: { value: VncMaxFps; label: string }[] = [
  { value: 60, label: "No limit" },
  { value: 30, label: "30 a second" },
  { value: 15, label: "15 a second" },
  { value: 5, label: "5 a second" },
];

const readAll = (): Record<string, unknown> => {
  try {
    const raw = localStorage.getItem(VNC_OPTIONS_KEY);
    const parsed = raw ? JSON.parse(raw) : null;
    return parsed && typeof parsed === "object" && !Array.isArray(parsed) ? parsed : {};
  } catch {
    return {};
  }
};

/** What the options mean together: Tight reads only full colour, so below it the
 *  choice goes back to Automatic (which then starts with ZRLE). */
export function normalizeVncOptions(o: VncOptions): VncOptions {
  return o.encoding === "tight" && o.colors !== 32 ? { ...o, encoding: "auto" } : o;
}

/** A server's options on this device; the defaults where none are kept. */
export function readVncOptions(uuid?: string | null): VncOptions {
  const kept = uuid ? (readAll()[uuid] as Partial<VncOptions> | undefined) : undefined;
  const o = { ...DEFAULT_VNC_OPTIONS };
  if (kept && typeof kept === "object") {
    if (VNC_ENCODING_CHOICES.some((c) => c.value === kept.encoding)) o.encoding = kept.encoding as VncEncoding;
    if (VNC_COLOR_CHOICES.some((c) => c.value === kept.colors)) o.colors = kept.colors as VncColors;
    if (VNC_FPS_CHOICES.some((c) => c.value === kept.maxFps)) o.maxFps = kept.maxFps as VncMaxFps;
    if (typeof kept.clipboard === "boolean") o.clipboard = kept.clipboard;
  }
  return normalizeVncOptions(o);
}

export function writeVncOptions(uuid: string, options: VncOptions): void {
  const all = readAll();
  all[uuid] = normalizeVncOptions(options);
  try {
    localStorage.setItem(VNC_OPTIONS_KEY, JSON.stringify(all));
  } catch {
    // Best-effort, same as the other per-device prefs.
  }
}
