// Abstraction over a filesystem (local or remote SFTP). The same `FilePanel`
// component is mounted twice — once with a LocalProvider and once with a
// RemoteProvider — and dispatches all I/O through this interface.
//
// Cross-pane transfer is handled outside the provider (the panels' upload / download batches) so
// each backend can keep its own fast path (e.g. `sftp_download_file` writes
// directly to disk instead of round-tripping through a JS `Uint8Array`).

export interface FileEntry {
  name: string;
  path: string;
  isDir: boolean;
  size: number;
  permissions?: number; // unix mode bits
  uid?: number;
  gid?: number;
  modified?: number; // unix timestamp (seconds)
  /**
   * The entry itself is a symbolic link. Anything that acts on the entry
   * (delete, drag, download) must check this first: `isDir`
   * describes the link's TARGET once resolved, and is only meant for the
   * icon, the sort order, and opening.
   */
  isSymlink?: boolean;
  /**
   * "pending": listed, target not followed yet (attributes are the link's
   * own). "ok": attributes and `isDir` are the target's. "broken": target
   * missing or a loop. "error": the target could not be read (`linkError`).
   */
  linkState?: LinkState;
  /** Link text as stored on disk, possibly relative to the link's directory. */
  linkTarget?: string;
  linkError?: string;
  /**
   * The link's own lstat mtime. Unlike `modified`, never replaced by the
   * target's; it changes when the link is retargeted, which is when a
   * resolved link has to be followed again.
   */
  linkModified?: number;
  /**
   * State carried over from an earlier listing of the same directory and
   * not confirmed since. Shown as is, but followed again before any action
   * relies on it: the target may have gone away meanwhile.
   */
  linkStale?: boolean;
}

export type LinkState = "pending" | "ok" | "broken" | "error";

/** Result of following one symlink; merged over its `FileEntry`. */
export interface LinkInfo {
  path: string;
  patch: Partial<FileEntry>;
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
  remove(path: string, isDir: boolean): Promise<void>;
  rename(from: string, to: string): Promise<void>;

  // ---- optional unix-only operations --------------------------------------
  chmod?: (path: string, mode: number) => Promise<void>;
  chown?: (path: string, uid: number, gid: number) => Promise<void>;

  // ---- optional symlink support -------------------------------------------
  /** Follows the given links. Absent when the provider does not report links. */
  resolveLinks?: (paths: string[]) => Promise<LinkInfo[]>;
  /** Absolute path with every symlink component resolved. */
  realPath?: (path: string) => Promise<string>;
}

/** Remote provider carries the SSH session id so transfer.ts can target it. */
export interface RemoteFileProvider extends FileProvider {
  readonly id: "remote";
  readonly sessionId: string;
}

export interface LocalFileProvider extends FileProvider {
  readonly id: "local";
}
