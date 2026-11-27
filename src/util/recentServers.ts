// The servers this device connected to last, newest first: the Recent row at
// the bottom of the Servers page. Kept on this device (localStorage), per
// profile, by the server's sync uuid (its id differs from device to device).
// Only uuids and times: no names or addresses outside the profile's vault.

export type RecentServer = { uuid: string; at: number };

export const RECENT_MAX = 10;

const PREFIX = "submarine-recent-servers:";

// Profile names hold only [A-Za-z0-9_-], so ":" keeps the parts apart.
const keyOf = (profile: string) => `${PREFIX}${profile}`;

// What is kept for `profile`: well-formed entries only, each server once, in
// the stored order (newest first), at most RECENT_MAX.
export function readRecent(profile: string): RecentServer[] {
  try {
    const saved = JSON.parse(localStorage.getItem(keyOf(profile)) || "[]");
    if (!Array.isArray(saved)) return [];
    const seen = new Set<string>();
    const list: RecentServer[] = [];
    for (const e of saved) {
      if (!e || typeof e.uuid !== "string" || !e.uuid || !Number.isFinite(e.at) || seen.has(e.uuid)) continue;
      seen.add(e.uuid);
      list.push({ uuid: e.uuid, at: e.at });
    }
    return list.slice(0, RECENT_MAX);
  } catch {
    return [];
  }
}

export function writeRecent(profile: string, list: RecentServer[]) {
  try {
    if (list.length > 0) localStorage.setItem(keyOf(profile), JSON.stringify(list));
    else localStorage.removeItem(keyOf(profile));
  } catch { /* storage full or off: the row lasts until the app closes */ }
}

// `list` with `uuid` connected at `at`: first, once, at most RECENT_MAX. The
// servers no longer in `known` (deleted here or by a sync) give up their
// place, but only when `known` holds `uuid` too: a list still loading, or
// one from before this server was saved, would drop the rest by mistake.
export function withRecent(list: RecentServer[], uuid: string, at: number, known?: ReadonlySet<string>): RecentServer[] {
  const prune = known?.has(uuid) ? known : null;
  const rest = list.filter((e) => e.uuid !== uuid && (!prune || prune.has(e.uuid)));
  return [{ uuid, at }, ...rest].slice(0, RECENT_MAX);
}

// A deleted profile's: a new profile of the same name starts with none.
export const forgetProfileRecent = (profile: string) => writeRecent(profile, []);

// Settings → Connections → "Show recent servers". On unless turned off; for
// this device. Off: no row, and no list kept for any profile here.
export const SHOW_RECENT_KEY = "submarine-show-recent-servers";

export function readShowRecent(): boolean {
  try {
    return localStorage.getItem(SHOW_RECENT_KEY) !== "off";
  } catch {
    return true;
  }
}

// Every profile's list on this device: the switch was turned off.
export function forgetAllRecent() {
  try {
    const keys: string[] = [];
    for (let i = 0; i < localStorage.length; i++) {
      const key = localStorage.key(i);
      if (key?.startsWith(PREFIX)) keys.push(key);
    }
    keys.forEach((key) => localStorage.removeItem(key));
  } catch { /* storage off: nothing was kept */ }
}
