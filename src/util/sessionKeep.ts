// What keeps an SSH session going: the keepalives this app sends, and logging back
// in by itself after a drop (#299). On by default. This device's choice, never
// synced, as a phone may want them where a desktop doesn't. A server can have its
// own choice, kept by its sync id; a quick connect has no id and goes by the default.
// `sendKeepalives` is read at connect and passed to `initiate_connection`; whether
// to reconnect is read when a session drops, and again before each try.
export const SEND_KEEPALIVES_KEY = "submarine-send-keepalives";
export const AUTO_RECONNECT_KEY = "submarine-auto-reconnect";
const SERVERS_KEY = "submarine-session-keep";

export type KeepChoice = "default" | "on" | "off";
export type SessionKeep = { keepalives: KeepChoice; reconnect: KeepChoice };
export const DEFAULT_SESSION_KEEP: SessionKeep = { keepalives: "default", reconnect: "default" };

const readSwitch = (key: string): boolean => {
  try {
    return localStorage.getItem(key) !== "off";
  } catch {
    return true;
  }
};
export const readSendKeepalivesDefault = () => readSwitch(SEND_KEEPALIVES_KEY);
export const readAutoReconnectDefault = () => readSwitch(AUTO_RECONNECT_KEY);

const readAll = (): Record<string, Partial<SessionKeep>> => {
  try {
    const parsed = JSON.parse(localStorage.getItem(SERVERS_KEY) || "{}");
    return parsed && typeof parsed === "object" && !Array.isArray(parsed) ? parsed : {};
  } catch {
    return {};
  }
};
const writeAll = (all: Record<string, Partial<SessionKeep>>) => {
  try {
    localStorage.setItem(SERVERS_KEY, JSON.stringify(all));
  } catch {
    // Best-effort, same as the other per-device prefs.
  }
};
const choice = (v: unknown): KeepChoice => (v === "on" || v === "off" ? v : "default");

/** A server's own choice on this device. */
export function readSessionKeep(uuid?: string | null): SessionKeep {
  const kept = uuid ? readAll()[uuid] : undefined;
  return { keepalives: choice(kept?.keepalives), reconnect: choice(kept?.reconnect) };
}

export function writeSessionKeep(uuid: string, keep: SessionKeep): void {
  const all = readAll();
  if (keep.keepalives === "default" && keep.reconnect === "default") delete all[uuid];
  else all[uuid] = keep;
  writeAll(all);
}

export function forgetSessionKeep(uuid: string): void {
  const all = readAll();
  delete all[uuid];
  writeAll(all);
}

export function readSendKeepalives(uuid?: string | null): boolean {
  const c = readSessionKeep(uuid).keepalives;
  return c === "default" ? readSendKeepalivesDefault() : c === "on";
}

export function readAutoReconnect(uuid?: string | null): boolean {
  const c = readSessionKeep(uuid).reconnect;
  return c === "default" ? readAutoReconnectDefault() : c === "on";
}
