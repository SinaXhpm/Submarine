import type { FileEntry, LinkInfo, LinkState } from "./types";

/** One row of `sftp_resolve_links`. */
export type RawSftpLink = {
  path: string;
  state: LinkState;
  target?: string | null;
  error?: string | null;
  is_dir: boolean;
  size?: number | null;
  permissions?: number | null;
  uid?: number | null;
  gid?: number | null;
  modified?: number | null;
};

// The backend answers with the target's attributes when the link resolves
// and with the link's own otherwise. Both replace whatever the row had, so
// a link that broke since the last pass stops looking like a folder.
export function linkInfoFromRaw(r: RawSftpLink): LinkInfo {
  return {
    path: r.path,
    patch: {
      linkState: r.state,
      linkTarget: r.target ?? undefined,
      linkError: r.error ?? undefined,
      isDir: r.is_dir,
      size: r.size ?? 0,
      permissions: r.permissions ?? undefined,
      uid: r.uid ?? undefined,
      gid: r.gid ?? undefined,
      modified: r.modified ?? undefined,
    },
  };
}

// Fresh listing of a directory already on screen: carry what the previous
// pass learned about each link, so a refresh does not blink every link
// back to "unresolved" and does not follow every link again. A link whose
// own mtime moved was retargeted: it stays "pending" and is followed anew.
// Carried state is marked stale: the target itself may have changed
// without touching the link, so actions re-check it first.
export function carryLinkState(fresh: FileEntry[], known: FileEntry[]): FileEntry[] {
  const resolved = new Map(
    known
      .filter((e) => e.isSymlink && e.linkState !== undefined && e.linkState !== "pending")
      .map((e) => [e.path, e]),
  );
  return fresh.map((e) => {
    const k = e.isSymlink ? resolved.get(e.path) : undefined;
    if (!k || k.linkModified !== e.linkModified) return e;
    return {
      ...e,
      linkState: k.linkState,
      linkTarget: k.linkTarget,
      linkError: k.linkError,
      isDir: k.isDir,
      size: k.size,
      permissions: k.permissions,
      uid: k.uid,
      gid: k.gid,
      modified: k.modified,
      linkStale: true,
    };
  });
}

// A new file or link name is one path component. Reject separators so the
// dialog cannot escape the directory that was right-clicked.
export function safeLeafName(raw: string): string | null {
  const name = raw.trim();
  if (!name || name === "." || name === "..") return null;
  if (/[\\/\0]/.test(name)) return null;
  return name;
}

export function shellSingleQuote(value: string): string {
  return `'${value.replace(/'/g, `'"'"'`)}'`;
}

// The properties dialog edits only the lower 9 mode bits. Bits above that
// (setuid, setgid, sticky) stay as the server reported them.
export function mergePermissions(known: number | undefined, edited: number): number {
  const low = edited & 0o777;
  if (known == null) return low;
  return (known & ~0o777) | low;
}

export function permissionOctal(mode: number | undefined): string {
  if (mode == null) return "";
  return (mode & 0o777).toString(8).padStart(3, "0");
}
