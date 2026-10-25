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
  /** The desktop's size as a part of the tab's, where the server can change
   *  it (TigerVNC can; a server showing a physical screen can't): fewer
   *  pixels to send, drawn larger and softer. At once while connected. */
  resolution: VncResolution;
  /** Ask for no updates while the tab is hidden (a few seconds on); what
   *  changed meanwhile comes in the first one after. At once while connected. */
  pauseHidden: boolean;
}

export type VncEncoding = "auto" | "tight" | "zrle" | "raw";
export type VncColors = 32 | 16 | 8;
export type VncMaxFps = 60 | 30 | 15 | 5;
export type VncResolution = 1 | 0.75 | 0.5;

export const VNC_OPTIONS_KEY = "submarine-vnc-options";

// What the VNC tab always asked for; it pauses while hidden, as the RDP tab does.
export const DEFAULT_VNC_OPTIONS: VncOptions = { encoding: "auto", colors: 32, maxFps: 60, clipboard: true, resolution: 1, pauseHidden: true };

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
export const VNC_RESOLUTION_CHOICES: { value: VncResolution; label: string }[] = [
  { value: 1, label: "Full (sharpest)" },
  { value: 0.75, label: "Three quarters" },
  { value: 0.5, label: "Half (least to send)" },
];

/** Speed and quality in one go (the clipboard and the pause are left as they
 *  are). "Standard" is what the tab asks for unless told otherwise. */
export type VncPreset = "standard" | "light" | "least";
type PresetChoices = Pick<VncOptions, "encoding" | "colors" | "maxFps" | "resolution">;
export const VNC_PRESETS: { value: VncPreset; label: string; set: PresetChoices }[] = [
  { value: "standard", label: "Standard", set: { encoding: "auto", colors: 32, maxFps: 60, resolution: 1 } },
  { value: "light", label: "Less data", set: { encoding: "auto", colors: 16, maxFps: 15, resolution: 0.75 } },
  { value: "least", label: "Least data", set: { encoding: "auto", colors: 8, maxFps: 5, resolution: 0.5 } },
];

/** The preset the choices are, if they are one. */
export function vncPresetOf(o: VncOptions): VncPreset | null {
  const keys = Object.keys(VNC_PRESETS[0].set) as (keyof PresetChoices)[];
  return VNC_PRESETS.find((p) => keys.every((k) => p.set[k] === o[k]))?.value ?? null;
}

/** Whether a session opened with `used` needs to connect again for `now`:
 *  what the server is told when the session starts (the resolution and the
 *  pause go to a running session at once). */
export function vncNeedsReconnect(used: VncOptions, now: VncOptions): boolean {
  const g = (o: VncOptions) => JSON.stringify([o.encoding, o.colors, o.maxFps, o.clipboard]);
  return g(used) !== g(now);
}

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
    if (VNC_RESOLUTION_CHOICES.some((c) => c.value === kept.resolution)) o.resolution = kept.resolution as VncResolution;
    if (typeof kept.pauseHidden === "boolean") o.pauseHidden = kept.pauseHidden;
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
