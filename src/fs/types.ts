// Abstraction over a filesystem (local or remote SFTP). The same `FilePanel`
// component is mounted twice — once with a LocalProvider and once with a
// RemoteProvider — and dispatches all I/O through this interface.
//
// Cross-pane transfer is handled outside the provider (see `transfer.ts`) so
// each backend can keep its own fast path (e.g. `sftp_download_file` writes
// directly to disk instead of round-tripping through a JS `Uint8Array`).

export interface FileEntry {
  name: string;
  path: string;
  /** For a symlink this describes the target, so links to folders open like folders. */
  isDir: boolean;
  size: number;
  permissions?: number; // unix mode bits
  uid?: number;
  gid?: number;
  modified?: number; // unix timestamp (seconds)
  /** The entry is a symlink (or Windows junction). Delete/rename act on the link itself. */
  isSymlink?: boolean;
  /** Symlink whose target is missing or not accessible. */
  brokenLink?: boolean;
}

export interface ListResult {
  currentPath: string;
  entries: FileEntry[];
}

export interface FileProvider {
  /** Identity tag used by transfer.ts to pick the right backend command. */
  readonly id: "local" | "remote";
  /** Short label shown in the panel header. */
  readonly label: string;
  /** Native path separator for this provider. */
  readonly pathSep: "/" | "\\";

  // ---- navigation ----------------------------------------------------------
  homePath(): Promise<string>;
  list(path: string): Promise<ListResult>;
  /** Joins a directory and an entry name into a full path. */
  joinPath(dir: string, name: string): string;
  /** Returns the parent directory of the given path. */
  parentPath(path: string): string;

  // ---- mutations -----------------------------------------------------------
  mkdir(path: string): Promise<void>;
  /** `isSymlink` makes the remote side unlink the link instead of rmdir-ing it. */
  remove(path: string, isDir: boolean, isSymlink?: boolean): Promise<void>;
  rename(from: string, to: string): Promise<void>;

  // ---- optional unix-only operations --------------------------------------
  chmod?: (path: string, mode: number) => Promise<void>;
  chown?: (path: string, uid: number, gid: number) => Promise<void>;
}

/** Remote provider carries the SSH session id so transfer.ts can target it. */
export interface RemoteFileProvider extends FileProvider {
  readonly id: "remote";
  readonly sessionId: string;
}

export interface LocalFileProvider extends FileProvider {
  readonly id: "local";
}
