// Session logs (#397): a terminal's output written to a file while the session
// runs (src-tauri/src/session_log.rs). This device's choice, never synced: the
// folder is a folder on this device, and turning logs on here shouldn't start
// files on another. Settings has the default: what to log, the folder (empty is
// "Submarine logs" in Documents), the file name with PuTTY's & codes, and
// whether a log that exists is added to. A server can have its own choice of
// what to log, kept by its sync id; a quick connect goes by the default. Not on
// a phone or tablet. Read when a terminal opens, so a change applies to the
// terminals opened after it.
import { IS_MOBILE } from "./platform";

export const SESSION_LOG_MODE_KEY = "submarine-session-log-mode";
export const SESSION_LOG_FOLDER_KEY = "submarine-session-log-folder";
export const SESSION_LOG_FILE_KEY = "submarine-session-log-file";
export const SESSION_LOG_APPEND_KEY = "submarine-session-log-append";
const SERVERS_KEY = "submarine-session-log-servers";

export type LogMode = "off" | "text" | "all";
export type LogChoice = "default" | LogMode;

/** The same as session_log.rs' DEFAULT_FILE_NAME. */
export const DEFAULT_LOG_FILE_NAME = "&S &Y-&M-&D &T.log";

export type SessionLogDefaults = { mode: LogMode; folder: string; fileName: string; append: boolean };

const read = (key: string): string | null => {
  try {
    return localStorage.getItem(key);
  } catch {
    return null;
  }
};
const write = (key: string, value: string) => {
  try {
    if (value) localStorage.setItem(key, value);
    else localStorage.removeItem(key);
  } catch {
    // Best-effort, same as the other per-device prefs.
  }
};
const mode = (v: unknown): LogMode => (v === "text" || v === "all" ? v : "off");

export function readSessionLogDefaults(): SessionLogDefaults {
  return {
    mode: mode(read(SESSION_LOG_MODE_KEY)),
    folder: read(SESSION_LOG_FOLDER_KEY) || "",
    fileName: read(SESSION_LOG_FILE_KEY) || "",
    append: read(SESSION_LOG_APPEND_KEY) === "on",
  };
}

export function writeSessionLogDefaults(d: SessionLogDefaults): void {
  write(SESSION_LOG_MODE_KEY, d.mode === "off" ? "" : d.mode);
  write(SESSION_LOG_FOLDER_KEY, d.folder.trim());
  write(SESSION_LOG_FILE_KEY, d.fileName.trim() === DEFAULT_LOG_FILE_NAME ? "" : d.fileName.trim());
  write(SESSION_LOG_APPEND_KEY, d.append ? "on" : "");
}

const readAll = (): Record<string, LogChoice> => {
  try {
    const parsed = JSON.parse(read(SERVERS_KEY) || "{}");
    return parsed && typeof parsed === "object" && !Array.isArray(parsed) ? parsed : {};
  } catch {
    return {};
  }
};
const writeAll = (all: Record<string, LogChoice>) => write(SERVERS_KEY, Object.keys(all).length ? JSON.stringify(all) : "");

/** A server's own choice on this device. */
export function readServerLogChoice(uuid?: string | null): LogChoice {
  const v = uuid ? readAll()[uuid] : undefined;
  return v === "off" || v === "text" || v === "all" ? v : "default";
}

export function writeServerLogChoice(uuid: string, choice: LogChoice): void {
  const all = readAll();
  if (choice === "default") delete all[uuid];
  else all[uuid] = choice;
  writeAll(all);
}

export function forgetServerLogChoice(uuid: string): void {
  writeServerLogChoice(uuid, "default");
}

/** What a terminal's open command takes as `sessionLog`: null logs nothing. */
export type SessionLogRequest = { mode: LogMode; folder: string; fileName: string; append: boolean; serverName: string; serverId: number };

export function sessionLogRequest(server: { uuid?: string | null; id?: number; name?: string }): SessionLogRequest | null {
  if (IS_MOBILE) return null;
  const d = readSessionLogDefaults();
  const choice = readServerLogChoice(server.uuid);
  const m = choice === "default" ? d.mode : choice;
  if (m === "off") return null;
  return { mode: m, folder: d.folder, fileName: d.fileName, append: d.append, serverName: server.name || "", serverId: server.id || 0 };
}
