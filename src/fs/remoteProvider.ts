import { invoke } from "@tauri-apps/api/core";
import { FileEntry, LinkInfo, ListResult, RemoteFileProvider } from "./types";
import { linkInfoFromRaw, RawSftpLink } from "./dirContext";

// SFTP provider. Wraps the existing `sftp_*` Tauri commands behind the
// `FileProvider` interface so the same `FilePanel` UI can drive either side.
// Remote paths are POSIX (always `/`).

type RawSftpEntry = {
  name: string;
  path: string;
  is_dir: boolean;
  size: number;
  permissions?: number;
  uid?: number;
  gid?: number;
  modified?: number;
  is_symlink?: boolean;
};

type RawSftpList = {
  current_path: string;
  entries: RawSftpEntry[];
};

export function createRemoteProvider(sessionId: string): RemoteFileProvider {
  return {
    id: "remote",
    sessionId,
    label: "Remote",
    pathSep: "/",

    async homePath() {
      // Empty path triggers `sftp_list_dir` to canonicalize "." against the
      // remote home — see the Rust command. We then just lift the result.
      const result = await invoke<RawSftpList>("sftp_list_dir", { sessionId, path: "" });
      return result.current_path;
    },

    async list(path: string): Promise<ListResult> {
      const raw = await invoke<RawSftpList>("sftp_list_dir", { sessionId, path });
      const entries: FileEntry[] = raw.entries.map((r) => ({
        name: r.name,
        path: r.path,
        isDir: r.is_dir,
        size: r.size,
        permissions: r.permissions,
        uid: r.uid,
        gid: r.gid,
        modified: r.modified,
        isSymlink: !!r.is_symlink,
        linkState: r.is_symlink ? "pending" : undefined,
        linkModified: r.is_symlink ? r.modified : undefined,
      }));
      return { currentPath: raw.current_path, entries };
    },

    joinPath(dir: string, name: string) {
      return dir.endsWith("/") ? `${dir}${name}` : `${dir}/${name}`;
    },

    parentPath(path: string) {
      const trimmed = path.replace(/\/+$/, "");
      const idx = trimmed.lastIndexOf("/");
      if (idx <= 0) return "/";
      return trimmed.slice(0, idx);
    },

    async mkdir(path: string) {
      await invoke("sftp_create_dir", { sessionId, path });
    },

    async remove(path: string, isDir: boolean) {
      if (isDir) await invoke("sftp_remove_dir", { sessionId, path });
      else await invoke("sftp_remove_file", { sessionId, path });
    },

    async rename(from: string, to: string) {
      await invoke("sftp_rename", { sessionId, oldpath: from, newpath: to });
    },

    // Both run `tar` / `zip` / `unzip` on the server, so nothing is
    // transferred; see `archive.rs`.
    async archive(dir, names, dest, format, overwrite) {
      await invoke("sftp_archive", { sessionId, dir, names, dest, format, overwrite });
    },

    async extract(dir: string, name: string, folder: string | null) {
      await invoke("sftp_extract", { sessionId, dir, name, folder });
    },

    async chmod(path: string, mode: number) {
      await invoke("sftp_set_permissions", { sessionId, path, permissions: mode });
    },

    async chown(path: string, uid: number, gid: number) {
      await invoke("sftp_set_owner", { sessionId, path, uid, gid });
    },

    async resolveLinks(paths: string[]): Promise<LinkInfo[]> {
      const raw = await invoke<RawSftpLink[]>("sftp_resolve_links", { sessionId, paths });
      return raw.map(linkInfoFromRaw);
    },

    async realPath(path: string) {
      return invoke<string>("sftp_realpath", { sessionId, path });
    },
  };
}
