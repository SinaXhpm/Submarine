import { invoke } from "@tauri-apps/api/core";
import { FileProvider, RemoteFileProvider } from "./types";

// Cross-pane transfer dispatch. Instead of streaming the file through JS, we
// pick the most direct backend command for each (source, destination) pair so
// large files don't have to materialise in the renderer's memory.
//
//   remote → local : sftp_download_file  (streams to disk)
//   local  → remote: sftp_upload_file    (streams from disk)
//   remote → remote: sftp_rename         (server-side move)
//   local  → local : local_rename        (in-process move)

export interface TransferSource {
  provider: FileProvider;
  /** Full path of the file being moved. */
  path: string;
  /** Base name — used to derive the destination path. */
  name: string;
  isDir: boolean;
}

export interface TransferTarget {
  provider: FileProvider;
  /** Directory the file should land in. */
  dir: string;
}

// `overwrite=false` (default) makes the backend reject pre-existing
// destinations with the `EXISTS:<path>` sentinel — callers should catch
// that, surface the overwrite prompt, and re-call with `overwrite=true`.
// Drag-drop used to clobber silently; that's no longer the default
// because users couldn't tell when they'd just overwritten work.
export async function transferFile(
  src: TransferSource,
  dest: TransferTarget,
  overwrite: boolean = false,
): Promise<void> {
  if (src.isDir) {
    // Recursive transfer between providers is non-trivial — we surface a clear
    // error rather than silently doing the wrong thing. A future iteration can
    // walk the tree and reuse the same dispatch for each leaf.
    throw new Error("Folder transfer between panes is not supported yet");
  }

  const destPath = dest.provider.joinPath(dest.dir, src.name);

  // remote → local : SFTP download streams straight to disk.
  if (src.provider.id === "remote" && dest.provider.id === "local") {
    await invoke("sftp_download_file", {
      sessionId: (src.provider as RemoteFileProvider).sessionId,
      remotePath: src.path,
      localPath: destPath,
      overwrite,
    });
    return;
  }

  // local → remote: SFTP upload reads straight from disk.
  if (src.provider.id === "local" && dest.provider.id === "remote") {
    await invoke("sftp_upload_file", {
      sessionId: (dest.provider as RemoteFileProvider).sessionId,
      localPath: src.path,
      remotePath: destPath,
      overwrite,
    });
    return;
  }

  // Same-side moves can just go through the provider's rename.
  await src.provider.rename(src.path, destPath);
}
