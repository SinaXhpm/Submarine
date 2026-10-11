// Whether a new terminal should ask the remote host to run THIS desktop's
// own default shell (as a login shell) instead of the account's configured
// one. Per-device, like the font prefs in util/terminalFont — read once when
// a PTY is opened (src-tauri/src/lib.rs::open_terminal), not live-reactive,
// so toggling it mid-session doesn't reopen already-running terminals.
export const USE_LOCAL_SHELL_KEY = "submarine-terminal-use-local-shell";

export function readUseLocalShell(): boolean {
  try {
    return localStorage.getItem(USE_LOCAL_SHELL_KEY) === "on";
  } catch {
    return false;
  }
}
