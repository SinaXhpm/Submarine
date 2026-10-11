// Folders bookmarked in the SFTP panes. Kept on this device (localStorage),
// per profile (server ids restart in every profile's database), per saved
// server and per pane. Profile names hold only [A-Za-z0-9_-], so ":" keeps
// the parts of a key apart.

export type BookmarkSide = "local" | "remote";

// Enough for any pane; a list can't grow without end.
export const BOOKMARKS_MAX = 50;

const PREFIX = "submarine-sftp-bookmarks:";

const profilePrefix = (profile: string) => `${PREFIX}${profile}:`;

export const bookmarksKey = (profile: string, serverId: number, side: BookmarkSide) =>
  `${profilePrefix(profile)}${serverId}:${side}`;

// What is stored under `key`: strings only, each once, at most BOOKMARKS_MAX.
export function readBookmarks(key: string): string[] {
  try {
    const saved = JSON.parse(localStorage.getItem(key) || "[]");
    if (!Array.isArray(saved)) return [];
    const paths = saved.filter((p): p is string => typeof p === "string" && p.length > 0);
    return [...new Set(paths)].slice(0, BOOKMARKS_MAX);
  } catch {
    return [];
  }
}

export function writeBookmarks(key: string, paths: string[]) {
  try {
    if (paths.length > 0) localStorage.setItem(key, JSON.stringify(paths));
    else localStorage.removeItem(key);
  } catch { /* storage full or off: the bookmark lasts until the pane closes */ }
}

// Drops every key that starts with `prefix`.
function forget(prefix: string) {
  try {
    for (const key of Object.keys(localStorage)) {
      if (key.startsWith(prefix)) localStorage.removeItem(key);
    }
  } catch { /* storage off: nothing was kept */ }
}

// A deleted profile's: a new profile of the same name starts its server ids
// at 1 again, and must not find these.
export const forgetProfileBookmarks = (profile: string) => forget(profilePrefix(profile));

// A deleted server's, in both panes.
export const forgetServerBookmarks = (profile: string, serverId: number) =>
  forget(`${profilePrefix(profile)}${serverId}:`);
