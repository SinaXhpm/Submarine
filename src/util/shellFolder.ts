// The folder a terminal's shell is in, when the shell says so (#245), for the
// SFTP panel's "Follow". Shells say it with OSC 7, `file://<host><path>` (fish 4
// on its own; bash and zsh with a line in their start-up file, which the panel
// shows), or kitty's `kitty-shell-cwd://`, or iTerm2's OSC 1337 `CurrentDir=`.

export type ShellFolder = { path: string; host: string };

// An absolute path without control characters, of a sane length.
const plainPath = (p: string) => p.startsWith("/") && p.length <= 4096 && !/[\u0000-\u001f\u007f]/.test(p);

// OSC 7's data. The path is percent-encoded (fish), or as it is (a shell that
// prints $PWD as it comes, which the lines for bash and zsh do).
export function parseOsc7(data: string): ShellFolder | null {
  const m = /^(file|kitty-shell-cwd):\/\/([^/]*)(\/.*)$/s.exec(data);
  // A host name is at most 253 characters, without spaces or controls (the
  // pane may show it).
  if (!m || m[2].length > 255 || /[\s\u0000-\u001f\u007f]/.test(m[2])) return null;
  let path = m[3];
  if (m[1] === "file") {
    try { path = decodeURIComponent(path); } catch { /* a raw path with a stray % */ }
  }
  return plainPath(path) ? { path, host: m[2] } : null;
}

// OSC 1337's `CurrentDir=<path>` (iTerm2's shell integration): no host.
export function parseOsc1337(data: string): ShellFolder | null {
  if (!data.startsWith("CurrentDir=")) return null;
  const path = data.slice("CurrentDir=".length);
  return plainPath(path) ? { path, host: "" } : null;
}

// A terminal's folder. `home` is the host its shell reported first (its own);
// a report from another host (an ssh onward from that terminal) sets `away`
// and keeps `path`, the last folder at home.
export type ShellFolderState = { home: string; path: string; away: string | null };

const hostOf = (h: string) => {
  const host = h.toLowerCase();
  return host === "localhost" ? "" : host;
};

// The state after a report, or null when nothing changed (a shell reports at
// every prompt). A report without a host counts as home.
export function nextShellFolder(prev: ShellFolderState | undefined, path: string, host: string): ShellFolderState | null {
  const h = hostOf(host);
  if (!prev) return { home: h, path, away: null };
  if (!h || !prev.home || h === prev.home) {
    if (prev.path === path && prev.away === null && (prev.home || !h)) return null;
    return { home: prev.home || h, path, away: null };
  }
  return prev.away === h ? null : { ...prev, away: h };
}

// The line that makes a shell report its folder, for each shell.
export const FOLLOW_SETUP: { shell: string; file: string; line: string; note?: string }[] = [
  { shell: "bash", file: "~/.bashrc", line: "PROMPT_COMMAND='printf \"\\033]7;file://%s%s\\007\" \"$HOSTNAME\" \"$PWD\"'\"${PROMPT_COMMAND:+; $PROMPT_COMMAND}\"" },
  { shell: "zsh", file: "~/.zshrc", line: "autoload -Uz add-zsh-hook; _osc7_cwd() { printf '\\033]7;file://%s%s\\007' \"$HOST\" \"$PWD\"; }; add-zsh-hook precmd _osc7_cwd" },
  { shell: "fish", file: "~/.config/fish/config.fish", line: "function _osc7_cwd --on-event fish_prompt; printf '\\e]7;file://%s%s\\a' $hostname $PWD; end", note: "fish 4 and later report it on their own." },
];
