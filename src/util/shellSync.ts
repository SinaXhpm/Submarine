// "Sync my shell config to this server" — the list of local/remote path
// pairs to push. Per-device (localStorage), like the font prefs in
// terminalFont.ts: it describes THIS machine's dotfiles, not a server, so it
// stays out of the vault even though the opt-in toggle to actually show the
// button lives on the server row (see AddNodePanel's syncShellConfig field).
//
// `remote` is always relative to the remote account's home directory (e.g.
// ".zshrc", ".oh-my-zsh/custom") rather than an absolute path — the same
// pair list has to work across servers that have different usernames.
export const SHELL_SYNC_PAIRS_KEY = "submarine-shell-sync-pairs";

export interface ShellSyncPair {
  local: string;
  remote: string;
}

export function readShellSyncPairs(): ShellSyncPair[] {
  try {
    const raw = localStorage.getItem(SHELL_SYNC_PAIRS_KEY);
    if (!raw) return [];
    const parsed = JSON.parse(raw);
    if (!Array.isArray(parsed)) return [];
    return parsed
      .filter((p): p is ShellSyncPair => p && typeof p.local === "string" && typeof p.remote === "string")
      .map((p) => ({ local: p.local, remote: p.remote }));
  } catch {
    return [];
  }
}

export function writeShellSyncPairs(pairs: ShellSyncPair[]): void {
  try {
    localStorage.setItem(SHELL_SYNC_PAIRS_KEY, JSON.stringify(pairs));
  } catch {
    // Best-effort, same as the other per-device prefs.
  }
}
