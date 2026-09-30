# Live edit

Opening a remote file in the system editor and sending saves back. Code: `sftp_open_remote_file` and its helpers in `src-tauri/src/lib.rs` (`download_to_part`, `watch_editor_copy`, `sync_editor_copy`, `upload_editor_copy`). The frontend only listens to the `sftp-open-<session>` (download progress), `sftp-sync-status-<session>` (upload result) and `sftp-transfer-<session>` (upload progress, `source: "sync"`) events.

## The flow

1. **Open.** The remote file is downloaded to `<session temp dir>/<hash of remote path>/<name>.part`, it must be at least as long as the size the server reported (a file whose size the server does not report is refused; a log that grew meanwhile is fine), and the `.part` file is renamed over the editor copy `<name>`. The copy is then opened with `open::that`. The directory name depends only on the remote path, so the same remote file always maps to the same editor copy.
2. **Open again.** The same steps run again: download, replace the editor copy, open. **The server's version always wins.** Local changes that were not sent yet are overwritten on purpose. There is no upload before the download and no merge. If the download fails or is cancelled, the existing copy is left as it was.
3. **Save in the editor.** A watcher on the copy's directory (750 ms debounce, matched by file name) reads the file. If its hash differs from `synced`, the whole file is written over the remote file and `synced` is updated. The file is streamed in chunks, not read into memory. From the truncating open on, progress goes out on `sftp-transfer-<session>` like a manual upload (`kind: "upload"`, `source: "sync"`), so the save shows in the transfers bar. It has no cancel flag: stopping after the truncate would only leave the remote file cut short until the retry. A failed upload is retried every 10 s until an upload has finished.
4. **The server is not watched.** Nothing polls it. A change made on the server only reaches the editor by opening the file again.

## State

`live_edits()` maps each editor copy to `LiveEdit { synced, interrupted, watching }` behind a tokio mutex.

- `synced` is the hash of the bytes the server is known to have: set after a download and after an upload. A watcher event whose file hash equals `synced` uploads nothing. This is why the replace in step 2 does not send the download back.
- The mutex is held while a copy is uploaded and while it is replaced, so an upload never runs across a replace. A reopen waits for an upload that is already running, then downloads.
- `interrupted` is set right after the truncating open of the remote file succeeds and cleared when that upload has been closed. While it is set, the copy is sent again even if it equals `synced` (the user undid the edit), and a momentarily missing file counts as a failed attempt, because the remote file is already truncated. It is not set when the upload fails before the truncate (local file missing, SFTP session not open): the server is intact then. "Hash equals `synced`" or "file missing" alone never ends a retry.
- `LiveEdit::server_has(hash)` is the only place that sets `synced`. It is called after a finished upload and after a download that was read completely and installed, and it clears `interrupted` in both cases. A reopen therefore never makes the watcher send the fresh download back, even after a failed upload.
- Only one watcher exists per copy (`watching`). A reopen reuses it.
- The upload and the open's download also take the per-(session, remote file) lock (`lock_remote_file*`) that manual uploads and downloads and the mirror (`mirror.rs`) take, so a save never truncates and writes a file a batch or the mirror is writing, and an open never reads a file one of them is rewriting. Transfers and the mirror also lock the local file (`lock_local_file*`). The locks follow the tree: a path is locked with shared locks on every parent folder and an exclusive lock on itself, so the mirror's soft delete of a folder waits for a save of a file inside it. Lock order everywhere: `LiveEdit` mutex, then the remote chain, then the local chain, each root first; the transfer commands and the mirror never take the `LiveEdit` mutex, so the order cannot deadlock. The frontend's transfer slot does not cover these: they start in Rust.

## Cleanup and limits

- The watcher runs until the session is gone (a missing file does not stop it: a replace-style save removes the name for a moment); there is no time limit, so editing for hours keeps syncing. The copy is deleted when the watcher stops and when the session is disconnected or the profile is closed (the whole session directory is removed).
- A stopping watcher keeps a copy that differs from `synced`, or whose upload did not finish, and reports where it is, instead of deleting a save the server does not have. Disconnecting still removes the directory.
- `synced` lives in memory only. After an app restart nothing is recovered.
- The upload truncates the remote file and writes into it. This keeps the server's owner and mode and needs no write access to the directory. A connection lost in the middle can leave the remote file truncated. The retry (every 10 s) fixes it only while the session is alive; if the session is disconnected or the profile is closed first, the local copy is removed too. The user has already seen the "Auto-sync failed" message by then. Accepted trade-off against the alternatives listed under "Do not".
- On Windows the replace fails if the editor holds the file locked. The error is shown and the old copy stays.
- `open::that` runs on every open. Whether a second editor window appears depends on the editor.

## Do not

- Do not upload the local copy before a reopen, or compare it with the download. That contradicts step 2.
- Do not upload through a temporary remote file and `rename`. It needs write access to the directory, and plain SFTP rename does not replace an existing file.
- Do not add a journal, sidecar files, or crash recovery for the local copy. It is a disposable copy of the server file.
- Do not watch the file itself instead of its directory: a replace-style save or our own rename changes the inode.

## Reviewed and rejected

Do not raise these again without new evidence.

**"An upload holds the shared SFTP session until `write_all` ends, so other operations on that session (directory listing) wait."** This is not how the session works. Checked in `russh-sftp` 3.0.0:

- `client/rawsession.rs`: requests are kept in a shared map keyed by request id (`requests: Arc<SharedRequests>`). There is no session-wide lock around a request, so any number of requests from different callers are in flight at once and each reply is matched to its caller by id.
- `client/fs/file.rs`: a `File` writes in pipelined chunks (`write_acks`, `max_concurrent_writes`) and `shutdown` waits for all acknowledgements and the close. A directory listing issued during a long write is sent between chunks and answered normally. It shares bandwidth with the upload, it does not queue behind it.
- `get_sftp_session` returns a cached `Arc<SftpSession>` on purpose: opening a new SFTP channel per save leaks server-side channels and is what the original code did. The old comment about a freeze concerned the SSH session mutex, which `upload_editor_copy` does not hold.

So a large upload slows other transfers on the same connection like any transfer would, and nothing more. The file is streamed in 256 KiB chunks, so memory does not grow with the file size either. A separate SFTP channel for uploads is not needed; if measurements ever show a real stall, add it then and record the measurement here.
