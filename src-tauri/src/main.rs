#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use aes_gcm::{aead::{Aead, KeyInit}, Aes256Gcm, Nonce};
use argon2::{Algorithm, Argon2, Params, Version};
use zeroize::{Zeroize, Zeroizing};
use rand::Rng;
use rusqlite::{ffi, Connection, DatabaseName};
use rusqlite::serialize::OwnedData;
use std::ptr::NonNull;
use std::sync::Mutex as StdMutex;
use std::path::PathBuf;
use std::fs;
use tauri::Manager;
use serde_json::json;
use ssh_key::{private::Ed25519Keypair, rand_core::OsRng, PrivateKey};
mod ssh_manager;
mod tunnel;
mod monitor;
mod cloud;
mod about;
mod mirror;
use ssh_manager::SshState;
use monitor::{MonitorMap, SharedSettings};
use mirror::MirrorMap;
use std::sync::Arc;
use tokio::io::AsyncReadExt;

// On-disk vault layout:
//   bytes 0..3   magic ("OMNV")
//   byte  4      version (1)
//   bytes 5..20  salt (16 bytes, per-profile)
//   bytes 21..32 nonce (12 bytes, per-save)
//   rest         AES-256-GCM(zstd(serialised-sqlite)) + 16-byte tag
const VAULT_MAGIC: &[u8; 4] = b"OMNV";
const VAULT_VERSION: u8 = 1;
/// zstd compression level. 3 is the library default — fast enough that
/// save latency is dominated by sqlite serialisation, with compression
/// ratios within a couple percent of the slower levels for SQL-like data.
const VAULT_COMPRESS_LEVEL: i32 = 3;
const SALT_LEN: usize = 16;
const NONCE_LEN: usize = 12;
const HEADER_LEN: usize = 4 + 1 + SALT_LEN;

pub struct DbState {
    pub conn: std::sync::Arc<StdMutex<Option<Connection>>>,
    /// `Zeroizing` wipes the 32-byte AES-256-GCM key on drop. Without
    /// this, the master key lives on in the heap allocator until the
    /// slot is reused — long enough to land in a crash dump or swap
    /// file. The mutex slot itself is overwritten with None on profile
    /// close which triggers the Zeroize Drop.
    pub master_key: StdMutex<Option<Zeroizing<[u8; 32]>>>,
    pub salt: StdMutex<Option<[u8; SALT_LEN]>>,
    pub db_path: StdMutex<Option<PathBuf>>,
    /// Name of the profile the user picked on the launch screen. Drives the
    /// path of `db_path` (under `<app_data>/profiles/<name>.submarine`) and is
    /// cleared by `close_profile` so the app returns to the picker.
    pub active_profile: StdMutex<Option<String>>,
}

// ---------------------------------------------------------------------------
// Profile path helpers
// ---------------------------------------------------------------------------

/// Where all profile files live. Created on first use. Each profile is an
/// independently encrypted `.submarine` file — no shared salt, no shared key.
pub(crate) fn profiles_dir(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    let data_dir = app.path().app_data_dir()
        .map_err(|e| format!("[SYSTEM] APP_DATA_DIR_NOT_FOUND: {}", e))?;
    Ok(data_dir.join("profiles"))
}

/// Compute the on-disk path for a named profile. Caller has already
/// validated the name with `validate_profile_name`.
pub(crate) fn profile_path(app: &tauri::AppHandle, name: &str) -> Result<PathBuf, String> {
    Ok(profiles_dir(app)?.join(format!("{}.submarine", name)))
}

/// Reject names that would let a user escape the profiles dir or collide
/// with reserved filenames on Windows. Keep the charset narrow on purpose
/// so a profile name is always a safe filename component on every OS.
pub(crate) fn validate_profile_name(name: &str) -> Result<(), String> {
    let n = name.trim();
    if n.is_empty() {
        return Err("Profile name cannot be empty".into());
    }
    if n.len() > 32 {
        return Err("Profile name too long (max 32 chars)".into());
    }
    if !n.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return Err("Profile name may only contain letters, numbers, '-' and '_'".into());
    }
    // Windows reserved device names — also weird on macOS/Linux as filename roots.
    let upper = n.to_uppercase();
    let reserved = ["CON", "PRN", "AUX", "NUL"];
    // `last_byte` is safe here because we already enforced ASCII-only at
    // the charset check above — but we still use `?`/`.map(...)` rather
    // than `.unwrap()` so a future relaxation can never silently panic.
    let last_ascii_digit = upper.as_bytes().last().is_some_and(|b| b.is_ascii_digit());
    if reserved.contains(&upper.as_str())
        || (upper.starts_with("COM") && upper.len() == 4 && last_ascii_digit)
        || (upper.starts_with("LPT") && upper.len() == 4 && last_ascii_digit)
    {
        return Err(format!("'{}' is a reserved name on Windows", n));
    }
    Ok(())
}

// Argon2id parameters for vault-key derivation:
//   m_cost   64 MiB  — memory hardness; raises cost of GPU/ASIC attacks
//   t_cost   3       — passes over the buffer
//   p_cost   4       — parallelism; up to 4 lanes if available
//   output   32 B    — AES-256-GCM key length
// Tuned higher than OWASP's interactive-login defaults because this protects
// the entire profile vault, not a single-request login. Changing these
// values invalidates every existing vault — bump only on a deliberate
// re-keying migration.
const ARGON2_M_COST: u32 = 64 * 1024;
const ARGON2_T_COST: u32 = 3;
const ARGON2_P_COST: u32 = 4;

fn derive_key(password: &str, salt_bytes: &[u8]) -> Result<[u8; 32], String> {
    let params = Params::new(ARGON2_M_COST, ARGON2_T_COST, ARGON2_P_COST, Some(32))
        .map_err(|e| format!("[CRYPTO] ARGON2_PARAMS: {}", e))?;
    let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut key = [0u8; 32];
    // Raw API: write derived bytes directly into the key buffer. Avoids
    // the PHC-string round-trip (encode then truncate b64) the previous
    // implementation used, which was fragile and made parameter changes
    // invisible to type-checking.
    argon2
        .hash_password_into(password.as_bytes(), salt_bytes, &mut key)
        .map_err(|e| format!("[CRYPTO] HASH_FAILED: {}", e))?;
    Ok(key)
}

fn encrypt_with_key(plaintext: &[u8], key: &[u8; 32]) -> Result<(Vec<u8>, [u8; NONCE_LEN]), String> {
    let cipher = Aes256Gcm::new(key.into());
    let nonce_bytes: [u8; NONCE_LEN] = rand::thread_rng().gen();
    let nonce = Nonce::from_slice(&nonce_bytes);
    let ciphertext = cipher.encrypt(nonce, plaintext)
        .map_err(|e| format!("[CRYPTO] ENCRYPT_FAILED: {}", e))?;
    Ok((ciphertext, nonce_bytes))
}

fn decrypt_with_key(ciphertext: &[u8], nonce_bytes: &[u8], key: &[u8; 32]) -> Result<Vec<u8>, String> {
    if nonce_bytes.len() != NONCE_LEN {
        return Err("[CRYPTO] NONCE_LEN_INVALID".into());
    }
    let cipher = Aes256Gcm::new(key.into());
    let nonce = Nonce::from_slice(nonce_bytes);
    cipher.decrypt(nonce, ciphertext)
        .map_err(|e| format!("[CRYPTO] DECRYPT_FAILURE: Possible wrong key or corrupted data. Details: {}", e))
}

/// Returns (salt, nonce, ciphertext) parsed out of an on-disk vault blob.
fn parse_vault_blob(data: &[u8]) -> Result<(Vec<u8>, Vec<u8>, Vec<u8>), String> {
    if data.len() < HEADER_LEN + NONCE_LEN {
        return Err("[VAULT] INVALID_FORMAT: Data too short".into());
    }
    if &data[..4] != VAULT_MAGIC {
        return Err("[VAULT] BAD_MAGIC".into());
    }
    if data[4] != VAULT_VERSION {
        return Err(format!("[VAULT] UNSUPPORTED_VERSION: {}", data[4]));
    }
    let salt = data[5..5 + SALT_LEN].to_vec();
    let nonce = data[HEADER_LEN..HEADER_LEN + NONCE_LEN].to_vec();
    let ct = data[HEADER_LEN + NONCE_LEN..].to_vec();
    Ok((salt, nonce, ct))
}

/// Copies `data` into a sqlite-allocated buffer wrapped in `OwnedData`.
/// `Connection::deserialize` requires a buffer allocated by `sqlite3_malloc`
/// because it frees it via `SQLITE_DESERIALIZE_FREEONCLOSE`.
fn to_sqlite_owned(data: &[u8]) -> Result<OwnedData, String> {
    let sz = data.len();
    let raw = unsafe { ffi::sqlite3_malloc64(sz as u64) } as *mut u8;
    let ptr = NonNull::new(raw).ok_or("[DATABASE] SQLITE_MALLOC_FAILED")?;
    unsafe {
        std::ptr::copy_nonoverlapping(data.as_ptr(), ptr.as_ptr(), sz);
        Ok(OwnedData::from_raw_nonnull(ptr, sz))
    }
}

fn write_vault_blob(salt: &[u8; SALT_LEN], nonce: &[u8; NONCE_LEN], ciphertext: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_LEN + NONCE_LEN + ciphertext.len());
    out.extend_from_slice(VAULT_MAGIC);
    out.push(VAULT_VERSION);
    out.extend_from_slice(salt);
    out.extend_from_slice(nonce);
    out.extend_from_slice(ciphertext);
    out
}

/// Compress the plaintext SQLite serialisation for vault v2 writes.
/// Errors here are surfaced as crypto-domain errors because the caller's
/// invariant ("save the DB") is what's broken, not just I/O.
fn vault_compress(plaintext: &[u8]) -> Result<Vec<u8>, String> {
    zstd::stream::encode_all(plaintext, VAULT_COMPRESS_LEVEL)
        .map_err(|e| format!("[VAULT] COMPRESS_FAILED: {}", e))
}

/// Decompress the post-decrypt body for vault v2 reads. Bounded by a
/// generous max-size guard so a corrupt or hostile file can't make us
/// allocate gigabytes — a real Submarine SQLite snapshot is well under
/// 64 MiB even with thousands of nodes.
fn vault_decompress(compressed: &[u8]) -> Result<Vec<u8>, String> {
    const MAX_DECOMPRESSED: usize = 64 * 1024 * 1024;
    let mut out = Vec::new();
    let mut decoder = zstd::stream::Decoder::new(compressed)
        .map_err(|e| format!("[VAULT] DECOMPRESS_INIT_FAILED: {}", e))?;
    use std::io::Read;
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = decoder.read(&mut buf)
            .map_err(|e| format!("[VAULT] DECOMPRESS_FAILED: {}", e))?;
        if n == 0 { break; }
        if out.len() + n > MAX_DECOMPRESSED {
            return Err("[VAULT] DECOMPRESS_TOO_LARGE: refusing to inflate past 64 MiB".into());
        }
        out.extend_from_slice(&buf[..n]);
    }
    Ok(out)
}

fn save_vault_internal(state: &DbState) -> Result<(), String> {
    let conn_guard = state.conn.lock().map_err(|_| "[STATE] MUTEX_POISON_CONN")?;
    let key_guard = state.master_key.lock().map_err(|_| "[STATE] MUTEX_POISON_KEY")?;
    let salt_guard = state.salt.lock().map_err(|_| "[STATE] MUTEX_POISON_SALT")?;
    let path_guard = state.db_path.lock().map_err(|_| "[STATE] MUTEX_POISON_PATH")?;

    if let (Some(conn), Some(key), Some(salt), Some(path)) =
        (&*conn_guard, &*key_guard, &*salt_guard, &*path_guard)
    {
        let serialized = conn.serialize(DatabaseName::Main)
            .map_err(|e| format!("[DATABASE] SERIALIZE_FAILED: {}", e))?;
        // Compress-then-encrypt. Order matters: compressing AFTER encryption
        // is useless because AES-GCM ciphertext is indistinguishable from
        // random. Doing it before keeps the on-disk file small AND keeps
        // ciphertext semantically secure.
        let compressed = Zeroizing::new(vault_compress(&*serialized)?);
        let (ciphertext, nonce) = encrypt_with_key(&compressed, key)?;
        let blob = write_vault_blob(salt, &nonce, &ciphertext);
        // Atomic write: tmp -> fsync -> rename. A crash / power loss in the
        // middle of a direct fs::write would leave the vault truncated, and
        // every saved credential would be unrecoverable on next launch.
        let tmp_path = path.with_extension("submarine.tmp");
        {
            use std::io::Write as _;
            let mut f = fs::File::create(&tmp_path)
                .map_err(|e| format!("[FILE] VAULT_TMP_CREATE_FAILED at {:?}: {}", tmp_path, e))?;
            f.write_all(&blob)
                .map_err(|e| format!("[FILE] VAULT_TMP_WRITE_FAILED at {:?}: {}", tmp_path, e))?;
            f.sync_all()
                .map_err(|e| format!("[FILE] VAULT_TMP_SYNC_FAILED at {:?}: {}", tmp_path, e))?;
        }
        fs::rename(&tmp_path, path)
            .map_err(|e| format!("[FILE] VAULT_RENAME_FAILED {:?} -> {:?}: {}", tmp_path, path, e))?;
    } else {
        return Err("[STATE] MISSING_REQUIRED_RESOURCES_FOR_SAVE".into());
    }
    Ok(())
}

/// Returns the list of available profile names (sorted, lowercased not enforced).
#[tauri::command]
async fn list_profiles(app_handle: tauri::AppHandle) -> Result<Vec<String>, String> {
    let dir = profiles_dir(&app_handle)?;
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in fs::read_dir(&dir).map_err(|e| format!("[FILE] READ_DIR_FAILED: {}", e))? {
        let entry = match entry { Ok(e) => e, Err(_) => continue };
        let path = entry.path();
        if !path.is_file() { continue; }
        if path.extension().and_then(|e| e.to_str()) != Some("submarine") { continue; }
        if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
            // Hide anything that wouldn't pass our name validator — likely
            // a manually-placed file or stray artefact. We don't surface it
            // because the user has no way to act on it from the UI.
            if validate_profile_name(stem).is_ok() {
                out.push(stem.to_string());
            }
        }
    }
    out.sort_by(|a, b| a.to_lowercase().cmp(&b.to_lowercase()));
    Ok(out)
}

/// Mark a profile as the active one. Subsequent `check_db_exists` /
/// `setup_master_db` calls operate against that profile's file. Returns
/// whether the profile's encrypted file already exists (caller uses this
/// to decide between "ask for password" and "this profile is empty / not
/// yet created" flows).
#[tauri::command]
async fn select_profile(
    app_handle: tauri::AppHandle,
    state: tauri::State<'_, DbState>,
    name: String,
) -> Result<bool, String> {
    validate_profile_name(&name)?;
    *state.active_profile.lock().map_err(|_| "[STATE] LOCK_FAILED")? = Some(name.clone());
    Ok(profile_path(&app_handle, &name)?.exists())
}

/// Drop in-memory state so the UI can return to the profile picker without
/// restarting the app. This MUST tear down every piece of per-profile
/// runtime state, not just the DB — otherwise live SSH sessions, tunnels,
/// SFTP channels, terminal PTYs, and fingerprint waiters from the
/// previous profile would survive the switch and (worse) attribute any
/// `known_hosts` writes they triggered to the NEXT profile's DB.
#[tauri::command]
async fn close_profile(
    state: tauri::State<'_, DbState>,
    ssh: tauri::State<'_, SshState>,
    monitor_map: tauri::State<'_, MonitorMap>,
) -> Result<(), String> {
    // 1. Stop monitor pollers. Flip `paused` first so the next loop iteration
    // releases the SSH handle, then drop the map so the Arc strong_count
    // falls to 1 and the poller exits.
    monitor::pause_all(monitor_map.inner().clone()).await;
    monitor_map.lock().await.clear();

    // 2. Collect every active session id, then run the standard
    // disconnect path for each one. This frees tunnel listener sockets,
    // SFTP channels, and the SSH handle in the right order.
    let session_ids: Vec<String> = ssh.connections.lock().await.keys().cloned().collect();
    for sid in &session_ids {
        tunnel::stop_all_for_session(&ssh.tunnels, sid).await;
        ssh.forwarded_targets.lock().await.remove(sid);
        ssh.sftp_sessions.lock().await.remove(sid);
        ssh.connections.lock().await.remove(sid);
        let temp = std::env::temp_dir().join(format!("submarine_sftp_{}", sid));
        if temp.exists() {
            let _ = std::fs::remove_dir_all(&temp);
        }
    }

    // 3. Close every terminal channel. The spawned PTY task watches its
    // `rx` end — dropping the senders here lets each task observe `None`
    // and call `channel.close()` cleanly. We do this AFTER connections are
    // gone so the task sees the close before trying another write.
    ssh.terminal_txs.lock().await.clear();
    ssh.resize_txs.lock().await.clear();

    // 4. Abort any pending fingerprint prompts. Sending `false` to the
    // oneshot rejects the prompt; if the rx side is already gone, the
    // send just errors out which is fine.
    let waiters: Vec<tokio::sync::oneshot::Sender<bool>> =
        ssh.fp_txs.lock().await.drain().map(|(_, v)| v).collect();
    for tx in waiters {
        let _ = tx.send(false);
    }

    // 5. Belt-and-suspenders: clear the residual maps in case anything
    // raced in between the steps above.
    ssh.tunnels.lock().await.clear();
    ssh.forwarded_targets.lock().await.clear();
    ssh.sftp_sessions.lock().await.clear();
    ssh.connections.lock().await.clear();
    ssh.fp_txs.lock().await.clear();

    // 6. Drop DB state last, so any in-flight write triggered by a
    // disconnecting handler above had a valid DB to land in.
    *state.conn.lock().map_err(|_| "[STATE] LOCK_FAILED_CONN")? = None;
    *state.master_key.lock().map_err(|_| "[STATE] LOCK_FAILED_KEY")? = None;
    *state.salt.lock().map_err(|_| "[STATE] LOCK_FAILED_SALT")? = None;
    *state.db_path.lock().map_err(|_| "[STATE] LOCK_FAILED_PATH")? = None;
    *state.active_profile.lock().map_err(|_| "[STATE] LOCK_FAILED_PROFILE")? = None;
    Ok(())
}

/// Permanently delete a profile's encrypted file. The caller must NOT be
/// "in" that profile (would orphan in-memory state pointing at a deleted
/// file). UI enforces this by only showing the delete button on the picker
/// screen.
#[tauri::command]
async fn delete_profile(app_handle: tauri::AppHandle, name: String) -> Result<(), String> {
    validate_profile_name(&name)?;
    let path = profile_path(&app_handle, &name)?;
    if path.exists() {
        fs::remove_file(&path)
            .map_err(|e| format!("[FILE] DELETE_PROFILE_FAILED at {:?}: {}", path, e))?;
    }
    Ok(())
}

/// Copy a profile's encrypted file to a user-chosen location so it can be
/// backed up or moved between machines. The file is already encrypted at
/// rest — we just copy bytes; we never decrypt or re-encrypt.
///
/// Returns `Some(path)` on success or `None` if the user cancels the
/// native save dialog. Errors bubble up as `Err`.
#[tauri::command]
async fn export_profile(
    app_handle: tauri::AppHandle,
    name: String,
) -> Result<Option<String>, String> {
    validate_profile_name(&name)?;
    let src = profile_path(&app_handle, &name)?;
    if !src.exists() {
        return Err(format!("Profile '{}' not found on disk", name));
    }

    // rfd's blocking dialog must not run on the main thread on macOS — we're
    // already off the UI thread in a tauri async command so a direct call is
    // fine. spawn_blocking would be needed if this was wrapped differently.
    let default_name = format!("{}.submarine", name);
    let chosen = rfd::FileDialog::new()
        .set_title("Export profile")
        .set_file_name(&default_name)
        .add_filter("Submarine profile", &["submarine"])
        .save_file();

    let dst = match chosen {
        Some(p) => p,
        None => return Ok(None),
    };

    fs::copy(&src, &dst)
        .map_err(|e| format!("[FILE] EXPORT_COPY_FAILED to {:?}: {}", dst, e))?;
    Ok(Some(dst.to_string_lossy().to_string()))
}

/// Open a file picker and verify the chosen file looks like a Submarine
/// vault (right header bytes). We do NOT decrypt — that requires the
/// profile password, which the user enters after import via the regular
/// unlock flow.
///
/// Returns `(source_path, suggested_name)` so the UI can confirm or rename
/// before committing the copy.
#[tauri::command]
async fn import_profile_pick() -> Result<Option<(String, String)>, String> {
    let picked = rfd::FileDialog::new()
        .set_title("Import profile")
        .add_filter("Submarine profile", &["submarine"])
        .pick_file();

    let path = match picked {
        Some(p) => p,
        None => return Ok(None),
    };

    // Cheap header check (no decryption). If the file isn't a vault we want
    // to fail before the user picks a name and gets a confusing error later.
    let mut header = [0u8; 5];
    let mut f = fs::File::open(&path).map_err(|e| format!("[FILE] IMPORT_OPEN_FAILED: {}", e))?;
    use std::io::Read;
    let n = f.read(&mut header).map_err(|e| format!("[FILE] IMPORT_READ_FAILED: {}", e))?;
    if n < 5 || &header[..4] != VAULT_MAGIC {
        return Err("Selected file is not a Submarine profile (bad header).".into());
    }
    if header[4] != VAULT_VERSION {
        return Err(format!(
            "Profile uses an unsupported vault version ({}). Update Submarine first.",
            header[4]
        ));
    }

    // Suggest a name from the file stem, sanitized to our profile-name rules
    // so the user can hit Enter without re-typing in the common case.
    let suggested = path
        .file_stem()
        .and_then(|s| s.to_str())
        .map(|s| {
            s.chars()
                .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
                .take(32)
                .collect::<String>()
        })
        .unwrap_or_else(|| "imported".to_string());

    Ok(Some((path.to_string_lossy().to_string(), suggested)))
}

/// Commit a picked vault file into the profiles dir under `name`. Refuses
/// to overwrite an existing profile — the UI must prompt the user to pick
/// a different name (or delete the existing one) in that case.
#[tauri::command]
async fn import_profile_save(
    app_handle: tauri::AppHandle,
    source_path: String,
    name: String,
) -> Result<(), String> {
    validate_profile_name(&name)?;
    let src = PathBuf::from(&source_path);
    if !src.exists() {
        return Err("Source file no longer exists.".into());
    }

    let dir = profiles_dir(&app_handle)?;
    fs::create_dir_all(&dir).map_err(|e| format!("[FILE] MKDIR_FAILED: {}", e))?;
    let dst = profile_path(&app_handle, &name)?;
    if dst.exists() {
        return Err(format!("Profile '{}' already exists", name));
    }

    // Single-read import: load the file into memory ONCE, validate the
    // header on the in-memory bytes, then write to the destination. The
    // previous "read 5 bytes to validate, then fs::copy" was TOCTOU —
    // an attacker (or a script running in parallel) could swap the file
    // between the header read and the copy and we'd import garbage.
    let bytes = fs::read(&src).map_err(|e| format!("[FILE] IMPORT_READ_FAILED: {}", e))?;
    if bytes.len() < 5 || &bytes[..4] != VAULT_MAGIC || bytes[4] != VAULT_VERSION {
        return Err("Source file is no longer a valid Submarine profile.".into());
    }
    if bytes.len() < HEADER_LEN + NONCE_LEN + 16 {
        return Err("Source file is truncated — header is valid but the body is too small.".into());
    }

    fs::write(&dst, &bytes)
        .map_err(|e| format!("[FILE] IMPORT_WRITE_FAILED to {:?}: {}", dst, e))?;
    Ok(())
}

/// Whether the *currently selected* profile's encrypted file exists on
/// disk. Returns false if no profile is selected — that signals the UI to
/// stay on the picker instead of jumping to the password prompt.
#[tauri::command]
async fn check_db_exists(
    app_handle: tauri::AppHandle,
    state: tauri::State<'_, DbState>,
) -> Result<bool, String> {
    let name = state.active_profile.lock()
        .map_err(|_| "[STATE] LOCK_FAILED_PROFILE")?
        .clone();
    let Some(name) = name else { return Ok(false) };
    Ok(profile_path(&app_handle, &name)?.exists())
}

#[tauri::command]
async fn setup_master_db(app_handle: tauri::AppHandle, mut password: String, state: tauri::State<'_, DbState>) -> Result<(), String> {
    // The active profile must be picked before this command — the UI does
    // it from the picker screen. Refuse early instead of silently writing
    // to a default path.
    let profile_name = state.active_profile.lock()
        .map_err(|_| "[STATE] LOCK_FAILED_PROFILE")?
        .clone()
        .ok_or("[STATE] NO_PROFILE_SELECTED")?;

    let dir = profiles_dir(&app_handle)?;
    if !dir.exists() {
        fs::create_dir_all(&dir).map_err(|e| format!("[FILE] DIR_CREATION_FAILED: {}", e))?;
    }

    let path = profile_path(&app_handle, &profile_name)?;
    let mut conn;
    let salt_bytes: [u8; SALT_LEN];
    // Wrap the derived AES key so it's wiped on every early-return path
    // and at the natural end of this function. Once it lands in DbState
    // the StdMutex<Option<Zeroizing<...>>> takes over the same guarantee.
    let key: Zeroizing<[u8; 32]>;
    let needs_resave;

    if path.exists() {
        let encrypted_data = fs::read(&path)
            .map_err(|e| format!("[FILE] VAULT_READ_FAILED: {}", e))?;
        let (parsed_salt, nonce, ciphertext) = parse_vault_blob(&encrypted_data)?;
        // Normalise the Vec<u8> salt into a fixed-size array up front so we
        // can copy it into both the spawn_blocking closure (move-by-Copy) and
        // the salt_bytes slot later, without juggling clones or lifetimes.
        let mut salt_fixed = [0u8; SALT_LEN];
        salt_fixed.copy_from_slice(&parsed_salt);
        // Argon2id with m=64MiB is CPU-heavy (≈0.5–2s depending on hardware).
        // Running it directly on the async runtime thread blocks every other
        // Tauri command for that duration — UI freezes, IPC backs up. Hand
        // it off to the blocking pool so the runtime stays responsive. The
        // closure also zeroizes the password buffer once the derivation is
        // done, preserving the secret-hygiene the original sync path had.
        let mut password_owned = std::mem::take(&mut password);
        let derived = tokio::task::spawn_blocking(move || {
            let res = derive_key(&password_owned, &salt_fixed);
            password_owned.zeroize();
            res
        })
            .await
            .map_err(|e| format!("[CRYPTO] KDF_JOIN: {}", e))??;
        key = Zeroizing::new(derived);
        let raw = Zeroizing::new(decrypt_with_key(&ciphertext, &nonce, &key)?);
        let decrypted_data = Zeroizing::new(vault_decompress(&raw)?);

        salt_bytes = salt_fixed;
        needs_resave = false;

        conn = Connection::open_in_memory()
            .map_err(|e| format!("[DATABASE] MEM_INIT_FAILED: {}", e))?;
        let owned = to_sqlite_owned(&decrypted_data)?;
        conn.deserialize(DatabaseName::Main, owned, false)
            .map_err(|e| format!("[DATABASE] DESERIALIZE_FAILED: {}", e))?;
        // Schema migration for vaults created before the Notes feature shipped.
        // Existing tables are untouched; only the new ones get materialised.
        // Idempotent — running it on a fresh vault that already has `notes`
        // (from the schema batch below) is a no-op.
        conn.execute(
            "CREATE TABLE IF NOT EXISTS notes (id INTEGER PRIMARY KEY AUTOINCREMENT, title TEXT, body TEXT)",
            [],
        ).map_err(|e| format!("[DATABASE] NOTES_MIGRATION_FAILED: {}", e))?;
        // Schema migration for the autostart-on-launch flag added later. We
        // can't use `IF NOT EXISTS` on ALTER, so swallow the "duplicate
        // column" error specifically — anything else propagates.
        if let Err(e) = conn.execute(
            "ALTER TABLE servers ADD COLUMN autostart INTEGER NOT NULL DEFAULT 0",
            [],
        ) {
            let s = e.to_string();
            if !s.contains("duplicate column name") {
                return Err(format!("[DATABASE] AUTOSTART_MIGRATION_FAILED: {}", s));
            }
        }
        // Schema migration for the mirror-config column.
        if let Err(e) = conn.execute(
            "ALTER TABLE servers ADD COLUMN mirrors TEXT NOT NULL DEFAULT '[]'",
            [],
        ) {
            let s = e.to_string();
            if !s.contains("duplicate column name") {
                return Err(format!("[DATABASE] MIRRORS_MIGRATION_FAILED: {}", s));
            }
        }
    } else {
        let mut fresh = [0u8; SALT_LEN];
        rand::thread_rng().fill(&mut fresh);
        salt_bytes = fresh;
        // Same reasoning as the unlock path above — keep the async runtime
        // unblocked during the Argon2 derivation on fresh-profile creation.
        let mut password_owned = std::mem::take(&mut password);
        let derived = tokio::task::spawn_blocking(move || {
            let res = derive_key(&password_owned, &salt_bytes);
            password_owned.zeroize();
            res
        })
            .await
            .map_err(|e| format!("[CRYPTO] KDF_JOIN: {}", e))??;
        key = Zeroizing::new(derived);
        needs_resave = true;

        conn = Connection::open_in_memory()
            .map_err(|e| format!("[DATABASE] MEM_INIT_FAILED: {}", e))?;
        conn.execute_batch(
            "CREATE TABLE folders (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT, parent_id INTEGER);
             CREATE TABLE ssh_keys (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT, public_key TEXT, private_key TEXT, passphrase TEXT);
             CREATE TABLE credentials (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT, auth_type TEXT, username TEXT, password TEXT, key_id INTEGER, FOREIGN KEY(key_id) REFERENCES ssh_keys(id));
             CREATE TABLE servers (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT, host TEXT, port INTEGER, username TEXT, password TEXT, credential_id INTEGER, folder_id INTEGER, proxy_type TEXT DEFAULT 'none', proxy_host TEXT, proxy_port INTEGER, tunnels TEXT, auth_type TEXT DEFAULT 'vault', key_id INTEGER, autostart INTEGER NOT NULL DEFAULT 0, mirrors TEXT NOT NULL DEFAULT '[]', FOREIGN KEY(folder_id) REFERENCES folders(id));
             CREATE TABLE commands (id INTEGER PRIMARY KEY AUTOINCREMENT, title TEXT, content TEXT);
             CREATE TABLE notes (id INTEGER PRIMARY KEY AUTOINCREMENT, title TEXT, body TEXT);
             CREATE TABLE known_hosts (id INTEGER PRIMARY KEY AUTOINCREMENT, host TEXT, port INTEGER, fingerprint TEXT);
             CREATE TABLE monitor_configs (node_id INTEGER PRIMARY KEY, enabled_metrics TEXT NOT NULL DEFAULT '[\"cpu\",\"mem\",\"disk\",\"load\"]', custom_metrics TEXT NOT NULL DEFAULT '[]', paused INTEGER NOT NULL DEFAULT 1, FOREIGN KEY(node_id) REFERENCES servers(id) ON DELETE CASCADE);
             CREATE TABLE monitor_settings (id INTEGER PRIMARY KEY, json TEXT NOT NULL);"
        ).map_err(|e| format!("[DATABASE] SCHEMA_CREATION_FAILED: {}", e))?;
    }

    conn.execute("PRAGMA foreign_keys = ON", []).map_err(|e| format!("[DATABASE] PRAGMA_FAILED: {}", e))?;

    // Reset every monitor to paused on profile open. Pollers don't survive
    // app restart, so a row with `paused=0` left over from the previous
    // session would advertise itself as "running" in the UI while no actual
    // backend task is spinning. Forcing pause makes the displayed state
    // truthful and matches the user's preference for explicit start.
    let _ = conn.execute("UPDATE monitor_configs SET paused = 1", []);

    // Acquire all four slot locks FIRST, then populate them in one go.
    // The previous "lock-populate, lock-populate, ..." pattern could
    // leave DbState half-initialised on a poisoned-mutex error from any
    // step but the first — later commands would see e.g. db_path set
    // but no master_key and fail in save_vault_internal with a
    // confusing MISSING_REQUIRED_RESOURCES error.
    let mut conn_guard = state.conn.lock().map_err(|_| "[STATE] LOCK_FAILED_CONN")?;
    let mut key_guard = state.master_key.lock().map_err(|_| "[STATE] LOCK_FAILED_KEY")?;
    let mut salt_guard = state.salt.lock().map_err(|_| "[STATE] LOCK_FAILED_SALT")?;
    let mut path_guard = state.db_path.lock().map_err(|_| "[STATE] LOCK_FAILED_PATH")?;
    *conn_guard = Some(conn);
    *key_guard = Some(key);
    *salt_guard = Some(salt_bytes);
    *path_guard = Some(path);
    drop(path_guard);
    drop(salt_guard);
    drop(key_guard);
    drop(conn_guard);

    if needs_resave {
        save_vault_internal(&state)?;
    }
    Ok(())
}

/// Flush the in-memory vault to disk. Used by the frontend after a successful
/// SSH connection so any `known_hosts` row that `check_server_key` inserted
/// during the handshake survives an app restart — otherwise the user would
/// see the same fingerprint prompt every time they reconnect.
#[tauri::command]
async fn persist_vault(state: tauri::State<'_, DbState>) -> Result<(), String> {
    save_vault_internal(&state)
}

/// Create a brand-new profile, encrypted with `password`, and select it as
/// the active profile so the app can proceed directly into the main view
/// without bouncing back through `select_profile + setup_master_db`.
/// Rejected if a profile with the same name already exists — the picker
/// surfaces existing names so a clobber would be the user's mistake to
/// recover from, not something we should silently do.
#[tauri::command]
async fn create_profile(
    app_handle: tauri::AppHandle,
    state: tauri::State<'_, DbState>,
    name: String,
    password: String,
) -> Result<(), String> {
    validate_profile_name(&name)?;
    if password.is_empty() {
        return Err("Password cannot be empty".into());
    }
    let dir = profiles_dir(&app_handle)?;
    fs::create_dir_all(&dir).map_err(|e| format!("[FILE] MKDIR_FAILED: {}", e))?;
    let path = profile_path(&app_handle, &name)?;
    if path.exists() {
        return Err(format!("Profile '{}' already exists", name));
    }
    *state.active_profile.lock().map_err(|_| "[STATE] LOCK_FAILED")? = Some(name);
    // Reuse setup_master_db's fresh-schema branch by deferring to it. Empty
    // profile starts with the same migrations the legacy path would do.
    setup_master_db(app_handle, password, state).await
}

#[tauri::command]
async fn generate_ssh_key(state: tauri::State<'_, DbState>, name: String) -> Result<(), String> {
    let keypair = Ed25519Keypair::random(&mut OsRng);
    let priv_key = PrivateKey::from(keypair);
    let pub_ssh = priv_key.public_key().to_openssh()
        .map_err(|e| format!("[SSH] PUB_EXPORT_FAILED: {}", e))?;
    let priv_ssh = priv_key.to_openssh(ssh_key::LineEnding::LF)
        .map_err(|e| format!("[SSH] PRIV_EXPORT_FAILED: {}", e))?.to_string();

    let conn_guard = state.conn.lock().map_err(|_| "[STATE] LOCK_FAILED")?;
    let conn = conn_guard.as_ref().ok_or("[STATE] DATABASE_NOT_INITIALIZED")?;
    
    conn.execute("INSERT INTO ssh_keys (name, public_key, private_key) VALUES (?1, ?2, ?3)", rusqlite::params![name, pub_ssh, priv_ssh])
        .map_err(|e| format!("[DATABASE] KEY_INSERT_FAILED: {}", e))?;
    
    drop(conn_guard);
    save_vault_internal(&state)?;
    Ok(())
}

/// Reject key formats the SSH client cannot use, so failures surface when the
/// user enters the key rather than when they try to connect.
fn validate_ssh_private_key(private_key: &str) -> Result<(), String> {
    let trimmed = private_key.trim();

    // PKCS#1 / PKCS#8 / SEC1 PEM headers. Whether we accept these depends on
    // whether the build has russh's OpenSSL backend wired in — see the
    // matching gate in the connect path.
    if trimmed.starts_with("-----BEGIN RSA PRIVATE KEY-----") {
        #[cfg(feature = "full-ssh-algos")]
        {
            return Ok(());
        }
        #[cfg(not(feature = "full-ssh-algos"))]
        {
            return Err("[SSH] UNSUPPORTED_KEY_FORMAT_DEV: RSA keys aren't available in this debug build. Either build with --features full-ssh-algos (requires Perl) or install a release build from GitHub — both support RSA. Ed25519 keys work everywhere.".into());
        }
    }
    if trimmed.starts_with("-----BEGIN DSA PRIVATE KEY-----")
        || trimmed.starts_with("-----BEGIN EC PRIVATE KEY-----")
    {
        // DSA is dead; ECDSA in PEM (SEC1) needs an extra conversion russh
        // doesn't do for us. Tell the user to convert and retry rather than
        // pretending the format is fine.
        return Err("[SSH] UNSUPPORTED_KEY_FORMAT: DSA / SEC1 ECDSA PEM keys aren't supported. Convert to OpenSSH format with `ssh-keygen -p -m PEM -f <file>` and re-import, or generate a fresh Ed25519 key.".into());
    }

    if trimmed.starts_with("-----BEGIN OPENSSH PRIVATE KEY-----") {
        // Inspect the algorithm without requiring the passphrase — the algorithm
        // header is unencrypted even when the body is encrypted.
        if let Ok(parsed) = ssh_key::PrivateKey::from_openssh(trimmed) {
            match parsed.algorithm() {
                ssh_key::Algorithm::Ed25519 => {}
                ssh_key::Algorithm::Rsa { .. } | ssh_key::Algorithm::Ecdsa { .. } => {
                    #[cfg(not(feature = "full-ssh-algos"))]
                    {
                        return Err(format!(
                            "[SSH] UNSUPPORTED_KEY_TYPE_DEV: {} keys need a release build (or local build with --features full-ssh-algos). Ed25519 works everywhere.",
                            parsed.algorithm().as_str()
                        ));
                    }
                }
                other => {
                    return Err(format!(
                        "[SSH] UNSUPPORTED_KEY_TYPE: {} keys are not supported.",
                        other.as_str()
                    ));
                }
            }
        }
        // If parsing fails entirely (unexpected header layout), let it through;
        // the connect path will surface a clearer error.
        return Ok(());
    }

    Err("[SSH] UNRECOGNIZED_KEY_FORMAT: Expected an OpenSSH-format private key (begins with -----BEGIN OPENSSH PRIVATE KEY-----) or RSA PEM.".into())
}

#[tauri::command]
async fn add_ssh_key(state: tauri::State<'_, DbState>, name: String, public_key: String, private_key: String, passphrase: Option<String>) -> Result<(), String> {
    validate_ssh_private_key(&private_key)?;

    let conn_guard = state.conn.lock().map_err(|_| "[STATE] LOCK_FAILED")?;
    let conn = conn_guard.as_ref().ok_or("[STATE] DATABASE_NOT_INITIALIZED")?;

    conn.execute("INSERT INTO ssh_keys (name, public_key, private_key, passphrase) VALUES (?1, ?2, ?3, ?4)", rusqlite::params![name, public_key, private_key, passphrase])
        .map_err(|e| format!("[DATABASE] KEY_INSERT_FAILED: {}", e))?;

    drop(conn_guard);
    save_vault_internal(&state)?;
    Ok(())
}

#[tauri::command]
async fn edit_ssh_key(state: tauri::State<'_, DbState>, id: i32, name: String, public_key: String, private_key: String, passphrase: Option<String>) -> Result<(), String> {
    validate_ssh_private_key(&private_key)?;

    let conn_guard = state.conn.lock().map_err(|_| "[STATE] LOCK_FAILED")?;
    let conn = conn_guard.as_ref().ok_or("[STATE] DATABASE_NOT_INITIALIZED")?;

    conn.execute("UPDATE ssh_keys SET name=?1, public_key=?2, private_key=?3, passphrase=?4 WHERE id=?5", rusqlite::params![name, public_key, private_key, passphrase, id])
        .map_err(|e| format!("[DATABASE] KEY_UPDATE_FAILED: {}", e))?;

    drop(conn_guard);
    save_vault_internal(&state)?;
    Ok(())
}

#[tauri::command]
async fn delete_ssh_key(state: tauri::State<'_, DbState>, id: i32) -> Result<(), String> {
    let conn_guard = state.conn.lock().map_err(|_| "[STATE] LOCK_FAILED")?;
    let conn = conn_guard.as_ref().ok_or("[STATE] DATABASE_NOT_INITIALIZED")?;
    
    conn.execute("DELETE FROM ssh_keys WHERE id=?1", rusqlite::params![id])
        .map_err(|e| format!("[DATABASE] KEY_DELETE_FAILED: {}", e))?;
    
    drop(conn_guard);
    save_vault_internal(&state)?;
    Ok(())
}

/// Canonicalize a node row's identity fields based on the chosen auth_type.
/// The principle: a node either authenticates via a vault credential OR via
/// inline node-level fields — never a hybrid. Storing only the relevant
/// fields makes the connection-time resolution unambiguous and keeps the DB
/// honest about which mode a node really uses.
fn normalize_server_identity(
    auth_type: &str,
    username: Option<String>,
    password: Option<String>,
    key_id: Option<i32>,
    credential_id: Option<i32>,
) -> (Option<String>, Option<String>, Option<i32>, Option<i32>) {
    // Treat empty / whitespace-only usernames as absent so the DB stays clean.
    let username = username.and_then(|s| {
        let t = s.trim();
        if t.is_empty() { None } else { Some(t.to_string()) }
    });
    match auth_type {
        "vault" => {
            // Credential carries everything; the node row stores only the link.
            (None, None, None, credential_id)
        }
        "custom_key" => {
            // No password, no credential link — node owns username + key.
            (username, None, key_id, None)
        }
        // "custom_pass" and any unknown fallback: node owns username + password.
        _ => (username, password, None, None),
    }
}

#[tauri::command]
async fn add_server(
    state: tauri::State<'_, DbState>,
    name: String,
    host: String,
    port: i32,
    username: Option<String>,
    password: Option<String>,
    credential_id: Option<i32>,
    folder_id: Option<i32>,
    proxy_type: String,
    proxy_host: String,
    proxy_port: i32,
    tunnels: Vec<serde_json::Value>,
    auth_type: String,
    key_id: Option<i32>,
    autostart: Option<bool>,
    mirrors: Option<Vec<serde_json::Value>>,
) -> Result<(), String> {
    let conn_guard = state.conn.lock().map_err(|_| "[STATE] LOCK_FAILED")?;
    let conn = conn_guard.as_ref().ok_or("[STATE] DATABASE_NOT_INITIALIZED")?;

    let tunnels_json = serde_json::to_string(&tunnels).unwrap_or_else(|_| "[]".to_string());
    let mirrors_json = mirrors.as_ref()
        .map(|m| serde_json::to_string(m).unwrap_or_else(|_| "[]".into()))
        .unwrap_or_else(|| "[]".into());

    // Enforce the "one source of truth" rule at write time: in vault mode the
    // node row carries no identity at all; in custom_* mode the credential
    // link is dropped. This keeps the DB self-consistent even if a future
    // caller forgets to nil out the fields.
    let (db_username, db_password, db_key_id, db_credential_id) = normalize_server_identity(
        &auth_type, username, password, key_id, credential_id,
    );

    let autostart_i: i32 = if autostart.unwrap_or(false) { 1 } else { 0 };
    let res = conn.execute(
        "INSERT INTO servers (name, host, port, username, password, credential_id, folder_id, proxy_type, proxy_host, proxy_port, tunnels, auth_type, key_id, autostart, mirrors) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
        rusqlite::params![name, host, port, db_username, db_password, db_credential_id, folder_id, proxy_type, proxy_host, proxy_port, tunnels_json, auth_type, db_key_id, autostart_i, mirrors_json],
    ).map_err(|e| format!("[DATABASE] SERVER_INSERT_FAILED: SQL_ERROR={}", e))?;

    if res == 0 {
        return Err("[DATABASE] SERVER_INSERT_FAILED: No rows affected".into());
    }

    drop(conn_guard);
    save_vault_internal(&state)?;
    Ok(())
}

#[tauri::command]
async fn edit_server(
    state: tauri::State<'_, DbState>,
    id: i32,
    name: String,
    host: String,
    port: i32,
    username: Option<String>,
    password: Option<String>,
    credential_id: Option<i32>,
    folder_id: Option<i32>,
    proxy_type: String,
    proxy_host: String,
    proxy_port: i32,
    tunnels: Vec<serde_json::Value>,
    auth_type: String,
    key_id: Option<i32>,
    autostart: Option<bool>,
    mirrors: Option<Vec<serde_json::Value>>,
) -> Result<(), String> {
    let conn_guard = state.conn.lock().map_err(|_| "[STATE] LOCK_FAILED")?;
    let conn = conn_guard.as_ref().ok_or("[STATE] DATABASE_NOT_INITIALIZED")?;

    let tunnels_json = serde_json::to_string(&tunnels).unwrap_or_else(|_| "[]".to_string());
    let mirrors_json = mirrors.as_ref()
        .map(|m| serde_json::to_string(m).unwrap_or_else(|_| "[]".into()))
        .unwrap_or_else(|| "[]".into());

    let (db_username, db_password, db_key_id, db_credential_id) = normalize_server_identity(
        &auth_type, username, password, key_id, credential_id,
    );

    let autostart_i: i32 = if autostart.unwrap_or(false) { 1 } else { 0 };
    conn.execute(
        "UPDATE servers SET name=?1, host=?2, port=?3, username=?4, password=?5, credential_id=?6, folder_id=?7, proxy_type=?8, proxy_host=?9, proxy_port=?10, tunnels=?11, auth_type=?12, key_id=?13, autostart=?14, mirrors=?15 WHERE id=?16",
        rusqlite::params![name, host, port, db_username, db_password, db_credential_id, folder_id, proxy_type, proxy_host, proxy_port, tunnels_json, auth_type, db_key_id, autostart_i, mirrors_json, id],
    ).map_err(|e| format!("[DATABASE] SERVER_UPDATE_FAILED: SQL_ERROR={}", e))?;

    drop(conn_guard);
    save_vault_internal(&state)?;
    Ok(())
}

/// Append a single mirror spec to a saved server's mirrors column.
///
/// Exists so the live MirrorsPanel's "new mirror" flow can persist the
/// mirror it just started without round-tripping the whole edit_server
/// payload (which would force the panel to know about every other
/// node field — username, tunnels, auth_type, etc.). Pairs with
/// re-encrypting the vault on the way out, the same as the full edit
/// path does. Duplicate (local, remote) entries are coalesced so
/// hitting "Start mirror" twice on the same pair doesn't bloat the row.
#[tauri::command]
async fn add_mirror_to_server(
    state: tauri::State<'_, DbState>,
    server_id: i32,
    mirror: serde_json::Value,
) -> Result<(), String> {
    let conn_guard = state.conn.lock().map_err(|_| "[STATE] LOCK_FAILED")?;
    let conn = conn_guard.as_ref().ok_or("[STATE] DATABASE_NOT_INITIALIZED")?;

    let existing: String = conn.query_row(
        "SELECT mirrors FROM servers WHERE id = ?1",
        rusqlite::params![server_id],
        |row| row.get::<_, Option<String>>(0).map(|v| v.unwrap_or_else(|| "[]".into())),
    ).map_err(|e| format!("[DATABASE] SERVER_LOOKUP_FAILED: {}", e))?;

    let mut list: Vec<serde_json::Value> = serde_json::from_str(&existing).unwrap_or_default();
    let local  = mirror.get("local").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let remote = mirror.get("remote").and_then(|v| v.as_str()).unwrap_or("").to_string();
    if local.is_empty() || remote.is_empty() {
        return Err("mirror needs both local and remote paths".into());
    }
    // Replace any prior entry for the same (local, remote) pair so the
    // latest spec (e.g. updated excludes / conflict mode) wins instead of
    // piling a duplicate row beside it.
    list.retain(|m| {
        m.get("local").and_then(|v| v.as_str()) != Some(&local)
            || m.get("remote").and_then(|v| v.as_str()) != Some(&remote)
    });
    list.push(mirror);

    let next = serde_json::to_string(&list).map_err(|e| format!("[DATABASE] SERIALIZE_FAILED: {}", e))?;
    conn.execute(
        "UPDATE servers SET mirrors = ?1 WHERE id = ?2",
        rusqlite::params![next, server_id],
    ).map_err(|e| format!("[DATABASE] MIRROR_SAVE_FAILED: {}", e))?;

    drop(conn_guard);
    save_vault_internal(&state)?;
    Ok(())
}

#[tauri::command]
async fn delete_server(state: tauri::State<'_, DbState>, id: i32) -> Result<(), String> {
    let conn_guard = state.conn.lock().map_err(|_| "[STATE] LOCK_FAILED")?;
    let conn = conn_guard.as_ref().ok_or("[STATE] DATABASE_NOT_INITIALIZED")?;
    
    conn.execute("DELETE FROM servers WHERE id=?1", rusqlite::params![id])
        .map_err(|e| format!("[DATABASE] SERVER_DELETE_FAILED: {}", e))?;
    
    drop(conn_guard);
    save_vault_internal(&state)?;
    Ok(())
}

#[tauri::command]
async fn get_servers(state: tauri::State<'_, DbState>) -> Result<Vec<serde_json::Value>, String> {
    let conn_guard = state.conn.lock().map_err(|_| "[STATE] LOCK_FAILED")?;
    let conn = conn_guard.as_ref().ok_or("[STATE] DATABASE_NOT_INITIALIZED")?;
    let mut stmt = conn.prepare("SELECT id, name, host, port, username, password, credential_id, folder_id, proxy_type, proxy_host, proxy_port, tunnels, auth_type, key_id, autostart, mirrors FROM servers")
        .map_err(|e| format!("[DATABASE] PREPARE_FAILED: {}", e))?;

    let rows = stmt.query_map([], |row| {
        Ok(json!({
            "id": row.get::<_, i32>(0)?,
            "name": row.get::<_, String>(1)?,
            "host": row.get::<_, String>(2)?,
            "port": row.get::<_, i32>(3)?,
            "username": row.get::<_, Option<String>>(4)?.unwrap_or_default(),
            "password": row.get::<_, Option<String>>(5)?,
            "credential_id": row.get::<_, Option<i32>>(6)?,
            "folder_id": row.get::<_, Option<i32>>(7)?,
            "proxy_type": row.get::<_, Option<String>>(8)?.unwrap_or_else(|| "none".to_string()),
            "proxy_host": row.get::<_, Option<String>>(9)?.unwrap_or_default(),
            "proxy_port": row.get::<_, Option<i32>>(10)?.unwrap_or(1080),
            "tunnels": row.get::<_, Option<String>>(11)?.unwrap_or_else(|| "[]".to_string()),
            "auth_type": row.get::<_, Option<String>>(12)?.unwrap_or_else(|| "vault".to_string()),
            "key_id": row.get::<_, Option<i32>>(13)?,
            "autostart": row.get::<_, i32>(14).unwrap_or(0) != 0,
            "mirrors": row.get::<_, Option<String>>(15)?.unwrap_or_else(|| "[]".to_string()),
        }))
    }).map_err(|e| format!("[DATABASE] QUERY_MAPPING_FAILED: {}", e))?;

    let mut list = Vec::new();
    for r in rows {
        list.push(r.map_err(|e| format!("[DATABASE] ROW_FETCH_FAILED: {}", e))?);
    }
    Ok(list)
}

#[tauri::command]
async fn get_ssh_keys(state: tauri::State<'_, DbState>) -> Result<Vec<serde_json::Value>, String> {
    let conn_guard = state.conn.lock().map_err(|_| "[STATE] LOCK_FAILED")?;
    let conn = conn_guard.as_ref().ok_or("[STATE] DATABASE_NOT_INITIALIZED")?;
    let mut stmt = conn.prepare("SELECT id, name, public_key, private_key, passphrase FROM ssh_keys").map_err(|e| e.to_string())?;
    let rows = stmt.query_map([], |row| {
        Ok(json!({
            "id": row.get::<_, i32>(0)?, 
            "name": row.get::<_, String>(1)?, 
            "public_key": row.get::<_, String>(2)?,
            "private_key": row.get::<_, String>(3)?,
            "passphrase": row.get::<_, Option<String>>(4)?
        }))
    }).map_err(|e| e.to_string())?;
    
    let mut list = Vec::new();
    for r in rows {
        list.push(r.map_err(|e| format!("[DATABASE] ROW_FETCH_FAILED: {}", e))?);
    }
    Ok(list)
}

#[tauri::command]
async fn get_credentials(state: tauri::State<'_, DbState>) -> Result<Vec<serde_json::Value>, String> {
    let conn_guard = state.conn.lock().map_err(|_| "[STATE] LOCK_FAILED")?;
    let conn = conn_guard.as_ref().ok_or("[STATE] DATABASE_NOT_INITIALIZED")?;
    let mut stmt = conn.prepare("SELECT id, name, auth_type, username, password, key_id FROM credentials").map_err(|e| e.to_string())?;
    let rows = stmt.query_map([], |row| {
        Ok(json!({
            "id": row.get::<_, i32>(0)?, 
            "name": row.get::<_, String>(1)?, 
            "auth_type": row.get::<_, String>(2)?,
            "username": row.get::<_, String>(3)?,
            "password": row.get::<_, Option<String>>(4)?,
            "key_id": row.get::<_, Option<i32>>(5)?
        }))
    }).map_err(|e| e.to_string())?;
    
    let mut list = Vec::new();
    for r in rows {
        list.push(r.map_err(|e| format!("[DATABASE] ROW_FETCH_FAILED: {}", e))?);
    }
    Ok(list)
}

#[tauri::command]
async fn add_credential(state: tauri::State<'_, DbState>, name: String, auth_type: String, username: String, password: Option<String>, key_id: Option<i32>) -> Result<(), String> {
    let conn_guard = state.conn.lock().map_err(|_| "[STATE] LOCK_FAILED")?;
    let conn = conn_guard.as_ref().ok_or("[STATE] DATABASE_NOT_INITIALIZED")?;
    
    conn.execute("INSERT INTO credentials (name, auth_type, username, password, key_id) VALUES (?1, ?2, ?3, ?4, ?5)", rusqlite::params![name, auth_type, username, password, key_id])
        .map_err(|e| format!("[DATABASE] CREDENTIAL_INSERT_FAILED: {}", e))?;
    
    drop(conn_guard);
    save_vault_internal(&state)?;
    Ok(())
}

#[tauri::command]
async fn edit_credential(state: tauri::State<'_, DbState>, id: i32, name: String, auth_type: String, username: String, password: Option<String>, key_id: Option<i32>) -> Result<(), String> {
    let conn_guard = state.conn.lock().map_err(|_| "[STATE] LOCK_FAILED")?;
    let conn = conn_guard.as_ref().ok_or("[STATE] DATABASE_NOT_INITIALIZED")?;
    
    conn.execute("UPDATE credentials SET name=?1, auth_type=?2, username=?3, password=?4, key_id=?5 WHERE id=?6", rusqlite::params![name, auth_type, username, password, key_id, id])
        .map_err(|e| format!("[DATABASE] CREDENTIAL_UPDATE_FAILED: {}", e))?;
    
    drop(conn_guard);
    save_vault_internal(&state)?;
    Ok(())
}

#[tauri::command]
async fn delete_credential(state: tauri::State<'_, DbState>, id: i32) -> Result<(), String> {
    let conn_guard = state.conn.lock().map_err(|_| "[STATE] LOCK_FAILED")?;
    let conn = conn_guard.as_ref().ok_or("[STATE] DATABASE_NOT_INITIALIZED")?;
    
    conn.execute("DELETE FROM credentials WHERE id=?1", rusqlite::params![id])
        .map_err(|e| format!("[DATABASE] CREDENTIAL_DELETE_FAILED: {}", e))?;
    
    drop(conn_guard);
    save_vault_internal(&state)?;
    Ok(())
}

#[tauri::command]
async fn add_folder(state: tauri::State<'_, DbState>, name: String, parent_id: Option<i32>) -> Result<(), String> {
    let conn_guard = state.conn.lock().map_err(|_| "[STATE] LOCK_FAILED")?;
    let conn = conn_guard.as_ref().ok_or("[STATE] DATABASE_NOT_INITIALIZED")?;
    
    conn.execute(
        "INSERT INTO folders (name, parent_id) VALUES (?1, ?2)",
        rusqlite::params![name, parent_id],
    ).map_err(|e| format!("[DATABASE] FOLDER_INSERT_FAILED: {}", e))?;
    
    drop(conn_guard);
    save_vault_internal(&state)?;
    Ok(())
}

#[tauri::command]
async fn rename_folder(state: tauri::State<'_, DbState>, id: i32, name: String) -> Result<(), String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err("[VALIDATION] FOLDER_NAME_EMPTY".into());
    }
    let conn_guard = state.conn.lock().map_err(|_| "[STATE] LOCK_FAILED")?;
    let conn = conn_guard.as_ref().ok_or("[STATE] DATABASE_NOT_INITIALIZED")?;
    let affected = conn.execute(
        "UPDATE folders SET name=?1 WHERE id=?2",
        rusqlite::params![trimmed, id],
    ).map_err(|e| format!("[DATABASE] FOLDER_RENAME_FAILED: {}", e))?;
    if affected == 0 {
        return Err(format!("[DATABASE] FOLDER_RENAME_FAILED: no folder with id {}", id));
    }
    drop(conn_guard);
    save_vault_internal(&state)?;
    Ok(())
}

#[tauri::command]
async fn delete_folder(state: tauri::State<'_, DbState>, id: i32) -> Result<(), String> {
    let conn_guard = state.conn.lock().map_err(|_| "[STATE] LOCK_FAILED")?;
    let conn = conn_guard.as_ref().ok_or("[STATE] DATABASE_NOT_INITIALIZED")?;
    
    // First, delete all servers in this folder
    conn.execute("DELETE FROM servers WHERE folder_id=?1", rusqlite::params![id])
        .map_err(|e| format!("[DATABASE] FOLDER_SERVERS_DELETE_FAILED: {}", e))?;
        
    // Then delete the folder
    conn.execute("DELETE FROM folders WHERE id=?1", rusqlite::params![id])
        .map_err(|e| format!("[DATABASE] FOLDER_DELETE_FAILED: {}", e))?;
    
    drop(conn_guard);
    save_vault_internal(&state)?;
    Ok(())
}

#[tauri::command]
async fn get_folders(state: tauri::State<'_, DbState>) -> Result<Vec<serde_json::Value>, String> {
    let conn_guard = state.conn.lock().map_err(|_| "[STATE] LOCK_FAILED")?;
    let conn = conn_guard.as_ref().ok_or("[STATE] DATABASE_NOT_INITIALIZED")?;
    let mut stmt = conn.prepare("SELECT id, name, parent_id FROM folders").map_err(|e| e.to_string())?;
    
    let rows = stmt.query_map([], |row| {
        Ok(json!({
            "id": row.get::<_, i32>(0)?, 
            "name": row.get::<_, String>(1)?, 
            "parent_id": row.get::<_, Option<i32>>(2)?
        }))
    }).map_err(|e| e.to_string())?;
    
    let mut list = Vec::new();
    for r in rows {
        list.push(r.map_err(|e| format!("[DATABASE] ROW_FETCH_FAILED: {}", e))?);
    }
    Ok(list)
}

#[tauri::command]
async fn add_command(state: tauri::State<'_, DbState>, title: String, content: String) -> Result<(), String> {
    let conn_guard = state.conn.lock().map_err(|_| "[STATE] LOCK_FAILED")?;
    let conn = conn_guard.as_ref().ok_or("[STATE] DATABASE_NOT_INITIALIZED")?;
    
    conn.execute(
        "INSERT INTO commands (title, content) VALUES (?1, ?2)",
        rusqlite::params![title, content],
    ).map_err(|e| format!("[DATABASE] COMMAND_INSERT_FAILED: {}", e))?;
    
    drop(conn_guard);
    save_vault_internal(&state)?;
    Ok(())
}

#[tauri::command]
async fn edit_command(state: tauri::State<'_, DbState>, id: i32, title: String, content: String) -> Result<(), String> {
    let conn_guard = state.conn.lock().map_err(|_| "[STATE] LOCK_FAILED")?;
    let conn = conn_guard.as_ref().ok_or("[STATE] DATABASE_NOT_INITIALIZED")?;
    
    conn.execute(
        "UPDATE commands SET title=?1, content=?2 WHERE id=?3",
        rusqlite::params![title, content, id],
    ).map_err(|e| format!("[DATABASE] COMMAND_UPDATE_FAILED: {}", e))?;
    
    drop(conn_guard);
    save_vault_internal(&state)?;
    Ok(())
}

#[tauri::command]
async fn delete_command(state: tauri::State<'_, DbState>, id: i32) -> Result<(), String> {
    let conn_guard = state.conn.lock().map_err(|_| "[STATE] LOCK_FAILED")?;
    let conn = conn_guard.as_ref().ok_or("[STATE] DATABASE_NOT_INITIALIZED")?;
    
    conn.execute(
        "DELETE FROM commands WHERE id=?1",
        rusqlite::params![id],
    ).map_err(|e| format!("[DATABASE] COMMAND_DELETE_FAILED: {}", e))?;
    
    drop(conn_guard);
    save_vault_internal(&state)?;
    Ok(())
}

#[tauri::command]
async fn get_commands(state: tauri::State<'_, DbState>) -> Result<Vec<serde_json::Value>, String> {
    let conn_guard = state.conn.lock().map_err(|_| "[STATE] LOCK_FAILED")?;
    let conn = conn_guard.as_ref().ok_or("[STATE] DATABASE_NOT_INITIALIZED")?;
    let mut stmt = conn.prepare("SELECT id, title, content FROM commands").map_err(|e| e.to_string())?;
    
    let rows = stmt.query_map([], |row| {
        Ok(json!({
            "id": row.get::<_, i32>(0)?, 
            "title": row.get::<_, String>(1)?, 
            "content": row.get::<_, String>(2)?
        }))
    }).map_err(|e| e.to_string())?;
    
    let mut list = Vec::new();
    for r in rows {
        list.push(r.map_err(|e| format!("[DATABASE] ROW_FETCH_FAILED: {}", e))?);
    }
    Ok(list)
}

// ───────────────────────── Notes ─────────────────────────
// Free-form text notes stored alongside the rest of the profile. Mirrors the
// commands CRUD shape exactly — title + body, no FK, no timestamps. Search
// is done client-side over the returned list so the user can match against
// title and body in one go without us pushing a LIKE query through SQLite.

#[tauri::command]
async fn add_note(state: tauri::State<'_, DbState>, title: String, body: String) -> Result<(), String> {
    let conn_guard = state.conn.lock().map_err(|_| "[STATE] LOCK_FAILED")?;
    let conn = conn_guard.as_ref().ok_or("[STATE] DATABASE_NOT_INITIALIZED")?;

    conn.execute(
        "INSERT INTO notes (title, body) VALUES (?1, ?2)",
        rusqlite::params![title, body],
    ).map_err(|e| format!("[DATABASE] NOTE_INSERT_FAILED: {}", e))?;

    drop(conn_guard);
    save_vault_internal(&state)?;
    Ok(())
}

#[tauri::command]
async fn edit_note(state: tauri::State<'_, DbState>, id: i32, title: String, body: String) -> Result<(), String> {
    let conn_guard = state.conn.lock().map_err(|_| "[STATE] LOCK_FAILED")?;
    let conn = conn_guard.as_ref().ok_or("[STATE] DATABASE_NOT_INITIALIZED")?;

    conn.execute(
        "UPDATE notes SET title=?1, body=?2 WHERE id=?3",
        rusqlite::params![title, body, id],
    ).map_err(|e| format!("[DATABASE] NOTE_UPDATE_FAILED: {}", e))?;

    drop(conn_guard);
    save_vault_internal(&state)?;
    Ok(())
}

#[tauri::command]
async fn delete_note(state: tauri::State<'_, DbState>, id: i32) -> Result<(), String> {
    let conn_guard = state.conn.lock().map_err(|_| "[STATE] LOCK_FAILED")?;
    let conn = conn_guard.as_ref().ok_or("[STATE] DATABASE_NOT_INITIALIZED")?;

    conn.execute(
        "DELETE FROM notes WHERE id=?1",
        rusqlite::params![id],
    ).map_err(|e| format!("[DATABASE] NOTE_DELETE_FAILED: {}", e))?;

    drop(conn_guard);
    save_vault_internal(&state)?;
    Ok(())
}

#[tauri::command]
async fn get_notes(state: tauri::State<'_, DbState>) -> Result<Vec<serde_json::Value>, String> {
    let conn_guard = state.conn.lock().map_err(|_| "[STATE] LOCK_FAILED")?;
    let conn = conn_guard.as_ref().ok_or("[STATE] DATABASE_NOT_INITIALIZED")?;
    // Newest first — matches how users think about notes (last-touched up top).
    let mut stmt = conn.prepare("SELECT id, title, body FROM notes ORDER BY id DESC").map_err(|e| e.to_string())?;

    let rows = stmt.query_map([], |row| {
        Ok(json!({
            "id": row.get::<_, i32>(0)?,
            "title": row.get::<_, String>(1)?,
            "body": row.get::<_, String>(2)?
        }))
    }).map_err(|e| e.to_string())?;

    let mut list = Vec::new();
    for r in rows {
        list.push(r.map_err(|e| format!("[DATABASE] ROW_FETCH_FAILED: {}", e))?);
    }
    Ok(list)
}

/// One-shot inline auth bundle for "quick connect" — connecting to a host
/// without saving anything in the vault. Mirrors the subset of node fields
/// that the connection flow actually needs: address, identity, and either
/// a password or a PEM key body. Proxy + tunnel auto-start are deliberately
/// omitted; the user can save a real node if they need those.
#[derive(Debug, Clone, serde::Deserialize)]
struct QuickAuth {
    host: String,
    port: i32,
    username: String,
    #[serde(default)]
    password: Option<String>,
    #[serde(default)]
    private_key: Option<String>,
    #[serde(default)]
    passphrase: Option<String>,
}

#[tauri::command]
async fn initiate_connection(
    app: tauri::AppHandle,
    state: tauri::State<'_, SshState>,
    db_state: tauri::State<'_, DbState>,
    session_id: String,
    server_id: i32,
    custom_password: Option<String>,
    quick_auth: Option<QuickAuth>,
) -> Result<(), String> {
    use tauri::Emitter;
    use tokio::time::Duration;
    use russh::client;
    use std::sync::Arc;
    use tokio::sync::Mutex;
    
    println!("[BACKEND] initiate_connection invoked for session_id: {}, server_id: {}", session_id, server_id);

    // 1. Prevent duplicate connections
    {
        let connections = state.connections.lock().await;
        if connections.contains_key(&session_id) {
            println!("[BACKEND] Connection already active for session: {}", session_id);
            return Ok(());
        }
        
    }

    println!("[BACKEND] No duplicates found. Registering oneshot channel and spawning connection worker...");
    let (fp_tx, fp_rx) = tokio::sync::oneshot::channel();
    // Random per-connect nonce — keys the fp_txs map so a stale `accept`
    // for one attempt (frontend bug, malicious IPC call, retry race)
    // cannot satisfy the prompt of a fresh connection. Hex over 16 bytes
    // = 128 bits of entropy, plenty for a single-use guard.
    let connect_nonce: String = {
        let mut bytes = [0u8; 16];
        rand::thread_rng().fill(&mut bytes);
        hex::encode(bytes)
    };
    state.fp_txs.lock().await.insert(connect_nonce.clone(), fp_tx);

    let session_id_clone = session_id.clone();
    let state_connections = Arc::clone(&state.connections);
    let state_sftp_sessions = Arc::clone(&state.sftp_sessions);
    let state_tunnels = Arc::clone(&state.tunnels);
    let fp_txs_clone = Arc::clone(&state.fp_txs);
    let db_conn_shared = Arc::clone(&db_state.conn);

    // Per-session map for R-tunnel target lookups. Created here so the same
    // Arc can be handed to both the ClientHandler (consulted on incoming
    // forwarded-tcpip channels) and to tunnel::start_tunnel (which writes the
    // mapping when a remote tunnel is set up).
    let session_forwarded_targets: tunnel::ForwardedTargets =
        Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));
    state.forwarded_targets.lock().await.insert(
        session_id.clone(),
        Arc::clone(&session_forwarded_targets),
    );

    // Quick connect bypasses the DB lookup entirely — we fabricate the
    // same tuple shape from the inline values so all downstream code
    // (auth, tunnel-start, handler setup) doesn't need to branch.
    let db_res = if let Some(q) = quick_auth.as_ref() {
        let key_data = q.private_key
            .clone()
            .filter(|s| !s.trim().is_empty())
            .map(|pk| (pk, q.passphrase.clone()));
        let auth_type = if key_data.is_some() { "custom_key" } else { "custom_pass" };
        Some((
            q.host.clone(),
            q.port,
            q.username.clone(),
            q.password.clone(),
            key_data,
            "none".to_string(),         // proxy_type — no proxy in quick mode
            None,                       // proxy_host
            None,                       // proxy_port
            auth_type.to_string(),      // server_auth_type
            None,                       // cred_auth_type (unused for custom_*)
            None,                       // db_key_id (debug-log only)
            None,                       // effective_key_id (auth code branches on key_data, not this)
            "[]".to_string(),           // tunnels_json — no auto-start tunnels
        ))
    } else {
    // Fetch DB record inside a nested block to drop non-Send Rows/Statement before any await
    {
        let conn_guard = db_state.conn.lock().map_err(|_| "[STATE] LOCK_FAILED")?;
        let conn = conn_guard.as_ref().ok_or("[STATE] DATABASE_NOT_INITIALIZED")?;
        
        // Pull both server-side and credential-side identity fields separately
        // and resolve in Rust. The previous COALESCE was order-of-precedence
        // magic that hid the actual rule from anyone reading the SQL — now the
        // rule is "vault mode → credential fields; custom_* mode → node fields"
        // and nothing else can mix the two.
        let mut stmt = conn.prepare("
            SELECT s.host, s.port,
                   s.username as s_user, c.username as c_user,
                   s.password as s_pass, c.password as c_pass,
                   s.key_id   as s_key,  c.key_id   as c_key,
                   s.proxy_type, s.proxy_host, s.proxy_port,
                   s.auth_type, c.auth_type as cred_auth_type, s.tunnels
            FROM servers s
            LEFT JOIN credentials c ON s.credential_id = c.id
            WHERE s.id=?1
        ").map_err(|e| e.to_string())?;

        let mut rows = stmt.query([server_id]).map_err(|e| e.to_string())?;
        if let Some(row) = rows.next().map_err(|e| e.to_string())? {
            // host/port are NOT NULL in the schema, but propagating
            // errors instead of `.unwrap()` means a manual DB edit or a
            // future schema relaxation can never silently panic the
            // spawned connection worker — the user gets a clean error.
            let host: String = row.get::<_, String>(0).map_err(|e| format!("[DB] host: {}", e))?;
            let port: i32 = row.get::<_, i32>(1).map_err(|e| format!("[DB] port: {}", e))?;
            let s_user: Option<String> = row.get::<_, Option<String>>(2).unwrap_or_default();
            let c_user: Option<String> = row.get::<_, Option<String>>(3).unwrap_or_default();
            let s_pass: Option<String> = row.get::<_, Option<String>>(4).unwrap_or_default();
            let c_pass: Option<String> = row.get::<_, Option<String>>(5).unwrap_or_default();
            let s_key:  Option<i32>    = row.get::<_, Option<i32>>(6).unwrap_or_default();
            let c_key:  Option<i32>    = row.get::<_, Option<i32>>(7).unwrap_or_default();
            let proxy_type: String = row.get::<_, Option<String>>(8).unwrap_or_default().unwrap_or_else(|| "none".to_string());
            let proxy_host: Option<String> = row.get::<_, Option<String>>(9).unwrap_or_default();
            let proxy_port: Option<i32> = row.get::<_, Option<i32>>(10).unwrap_or_default();
            let server_auth_type: String = row.get::<_, Option<String>>(11).unwrap_or_default().unwrap_or_else(|| "vault".to_string());
            let cred_auth_type: Option<String> = row.get::<_, Option<String>>(12).unwrap_or_default();
            let tunnels_json: String = row.get::<_, Option<String>>(13).unwrap_or_default().unwrap_or_else(|| "[]".to_string());

            // Single source of truth per auth_type — no field mixing.
            // - vault: identity comes ENTIRELY from the credential row. Any
            //   stale username/password/key on the node row is ignored.
            // - custom_pass / custom_key: identity comes ENTIRELY from the
            //   node row.
            let (username, password, key_id) = if server_auth_type == "vault" {
                (c_user.unwrap_or_default(), c_pass, c_key)
            } else {
                (s_user.unwrap_or_default(), s_pass, s_key)
            };

            // Whether to actually load a key file:
            // - vault: only if the chosen credential is itself key-typed
            // - custom_key: yes, use the node's selected key
            // - custom_pass: no
            let effective_key_id = if server_auth_type == "vault" {
                if cred_auth_type.as_deref() == Some("key") { key_id } else { None }
            } else if server_auth_type == "custom_key" {
                key_id
            } else {
                None
            };

            // Fetch key details if a key is needed
            let key_data = if let Some(kid) = effective_key_id {
                let mut key_stmt = conn.prepare("SELECT private_key, passphrase FROM ssh_keys WHERE id = ?1").map_err(|e| e.to_string())?;
                let mut key_rows = key_stmt.query([kid]).map_err(|e| e.to_string())?;
                if let Some(key_row) = key_rows.next().map_err(|e| e.to_string())? {
                    let private_key: String = key_row.get::<_, String>(0).map_err(|e| e.to_string())?;
                    let passphrase: Option<String> = key_row.get::<_, Option<String>>(1).map_err(|e| e.to_string())?;
                    Some((private_key, passphrase))
                } else {
                    None
                }
            } else {
                None
            };

            Some((host, port, username, password, key_data, proxy_type, proxy_host, proxy_port, server_auth_type, cred_auth_type, key_id, effective_key_id, tunnels_json))
        } else {
            None
        }
    }
    };

    let (host, port, user, password, key_data, proxy_type, proxy_host, proxy_port, server_auth_type, cred_auth_type, db_key_id, effective_key_id, tunnels_json) = match db_res {
        Some(val) => val,
        None => {
            state.fp_txs.lock().await.remove(&connect_nonce);
            return Err("Server not found".into());
        }
    };

    let handler = ssh_manager::ClientHandler {
        app: app.clone(),
        session_id: session_id.clone(),
        connect_nonce: connect_nonce.clone(),
        server_host: host.clone(),
        server_port: port as u16,
        db: db_conn_shared,
        fp_rx: Some(fp_rx),
        forwarded_targets: Arc::clone(&session_forwarded_targets),
    };

    let cleanup_nonce = connect_nonce.clone();

    tauri::async_runtime::spawn(async move {
        println!("[BACKEND WORKER] Started connection worker thread for session: {}", session_id_clone);

        struct FpCleanupGuard {
            fp_txs: Arc<Mutex<std::collections::HashMap<String, tokio::sync::oneshot::Sender<bool>>>>,
            nonce: String,
        }
        impl Drop for FpCleanupGuard {
            fn drop(&mut self) {
                let fp_txs = Arc::clone(&self.fp_txs);
                let nonce = self.nonce.clone();
                tauri::async_runtime::spawn(async move {
                    fp_txs.lock().await.remove(&nonce);
                });
            }
        }
        let _guard = FpCleanupGuard {
            fp_txs: Arc::clone(&fp_txs_clone),
            nonce: cleanup_nonce.clone(),
        };

        let emit_log = |msg: &str, log_type: &str| {
            println!("[LOG-{}] {}", session_id_clone, msg);
            let _ = app.emit(&format!("session-log-{}", session_id_clone), serde_json::json!({"msg": msg, "type": log_type}));
        };

        let cleanup = || async {
            // Already handled by Drop Guard, but keeping for immediate eviction if needed
            fp_txs_clone.lock().await.remove(&cleanup_nonce);
        };

        emit_log("Initializing SSH connection process...", "info");
        let effective_user = if user.trim().is_empty() {
            emit_log("Username is empty. Defaulting to 'root'.", "info");
            "root".to_string()
        } else {
            user.trim().to_string()
        };
        emit_log(&format!("Server Details -> Host: {}, Port: {}, User: {}", host, port, effective_user), "info");
        emit_log(&format!("[DEBUG] Server Auth Method: {}", server_auth_type), "info");
        if server_auth_type == "vault" {
            emit_log(&format!("[DEBUG] Vault Identity Auth Type: {:?}", cred_auth_type), "info");
        }
        emit_log(&format!("[DEBUG] SQLite DB key_id: {:?}", db_key_id), "info");
        emit_log(&format!("[DEBUG] effective_key_id determined: {:?}", effective_key_id), "info");
        if let Some((ref priv_key, ref passphrase)) = key_data {
            emit_log(&format!("[DEBUG] SSH Key loaded from DB. Private Key length: {} chars, Has Passphrase: {}", priv_key.len(), passphrase.is_some()), "info");
            if priv_key.trim().is_empty() {
                emit_log("[DEBUG] WARNING: SSH Key content is EMPTY!", "error");
            } else {
                let first_line = priv_key.lines().next().unwrap_or("");
                emit_log(&format!("[DEBUG] SSH Key Header: {}", first_line), "info");
            }
        } else {
            emit_log("[DEBUG] No SSH Key was loaded from database for this session.", "info");
        }

        // Set up generic stream based on proxy configuration
        trait AsyncStream: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static {}
        impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static> AsyncStream for T {}

        struct StreamWrapper(Box<dyn AsyncStream>);
        impl tokio::io::AsyncRead for StreamWrapper {
            fn poll_read(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>, buf: &mut tokio::io::ReadBuf<'_>) -> std::task::Poll<std::io::Result<()>> {
                std::pin::Pin::new(&mut *self.0).poll_read(cx, buf)
            }
        }
        impl tokio::io::AsyncWrite for StreamWrapper {
            fn poll_write(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>, buf: &[u8]) -> std::task::Poll<std::io::Result<usize>> {
                std::pin::Pin::new(&mut *self.0).poll_write(cx, buf)
            }
            fn poll_flush(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<std::io::Result<()>> {
                std::pin::Pin::new(&mut *self.0).poll_flush(cx)
            }
            fn poll_shutdown(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<std::io::Result<()>> {
                std::pin::Pin::new(&mut *self.0).poll_shutdown(cx)
            }
        }

        let stream_res: Result<Box<dyn AsyncStream>, String> = match proxy_type.as_str() {
            "socks5" => {
                let p_host = match proxy_host.as_ref().filter(|h| !h.is_empty()) {
                    Some(h) => h,
                    None => {
                        let err_msg = "SOCKS5 Proxy Host is empty";
                        emit_log(&format!("Error: {}", err_msg), "error");
                        cleanup().await;
                        let _ = app.emit(&format!("connection-failed-{}", session_id_clone), serde_json::json!({"reason": err_msg}));
                        return;
                    }
                };
                let p_port = proxy_port.unwrap_or(1080) as u16;
                emit_log(&format!("Connecting via SOCKS5 Proxy {}:{}...", p_host, p_port), "info");
                
                let proxy_addr = format!("{}:{}", p_host, p_port);
                match tokio::time::timeout(
                    Duration::from_secs(10),
                    tokio_socks::tcp::Socks5Stream::connect(proxy_addr.as_str(), (host.as_str(), port as u16))
                ).await {
                    Ok(Ok(stream)) => {
                        emit_log("SOCKS5 Proxy tunnel established successfully.", "success");
                        Ok(Box::new(stream))
                    }
                    Ok(Err(e)) => {
                        Err(humanize_network_err(&e.to_string(), &host, port, "SOCKS5 proxy"))
                    }
                    Err(_) => {
                        Err(format!("SOCKS5 proxy {}:{} did not respond in time", p_host, p_port))
                    }
                }
            }
            "http" => {
                let p_host = match proxy_host.as_ref().filter(|h| !h.is_empty()) {
                    Some(h) => h,
                    None => {
                        let err_msg = "HTTP Proxy Host is empty";
                        emit_log(&format!("Error: {}", err_msg), "error");
                        cleanup().await;
                        let _ = app.emit(&format!("connection-failed-{}", session_id_clone), serde_json::json!({"reason": err_msg}));
                        return;
                    }
                };
                let p_port = proxy_port.unwrap_or(8080) as u16;
                emit_log(&format!("Connecting via HTTP Proxy {}:{}...", p_host, p_port), "info");

                let proxy_addr = format!("{}:{}", p_host, p_port);
                match tokio::time::timeout(
                    Duration::from_secs(10),
                    tokio::net::TcpStream::connect(proxy_addr)
                ).await {
                    Ok(Ok(mut tcp_stream)) => {
                        // Disable Nagle: SSH is heavily interactive (keystrokes,
                        // small control packets) and batching adds noticeable
                        // round-trip latency. Ignore failures — set_nodelay is
                        // best-effort; some platforms / virtual NICs reject it.
                        let _ = tcp_stream.set_nodelay(true);
                        emit_log(&format!("Requesting HTTP CONNECT tunnel to {}:{}...", host, port), "info");
                        match tokio::time::timeout(
                            Duration::from_secs(10),
                            async_http_proxy::http_connect_tokio(&mut tcp_stream, &host, port as u16)
                        ).await {
                            Ok(Ok(_)) => {
                                emit_log("HTTP Proxy tunnel established successfully.", "success");
                                Ok(Box::new(tcp_stream))
                            }
                            Ok(Err(e)) => {
                                Err(humanize_network_err(&e.to_string(), &host, port, "HTTP CONNECT tunnel"))
                            }
                            Err(_) => {
                                Err(format!("HTTP CONNECT tunnel to {}:{} timed out", host, port))
                            }
                        }
                    }
                    Ok(Err(e)) => {
                        Err(humanize_network_err(&e.to_string(), p_host, p_port as i32, "HTTP proxy"))
                    }
                    Err(_) => {
                        Err(format!("HTTP proxy {}:{} did not respond in time", p_host, p_port))
                    }
                }
            }
            _ => {
                emit_log(&format!("Connecting directly to {}:{}...", host, port), "info");
                match tokio::time::timeout(
                    Duration::from_secs(10),
                    tokio::net::TcpStream::connect((host.as_str(), port as u16))
                ).await {
                    Ok(Ok(stream)) => {
                        // See HTTP-proxy branch — SSH wants every packet on the
                        // wire immediately, no Nagle batching.
                        let _ = stream.set_nodelay(true);
                        emit_log("Direct TCP Connection established successfully.", "success");
                        Ok(Box::new(stream))
                    }
                    Ok(Err(e)) => {
                        Err(humanize_network_err(&e.to_string(), &host, port, "Connection"))
                    }
                    Err(_) => {
                        Err(format!("{}:{} did not respond within 10 seconds", host, port))
                    }
                }
            }
        };

        let stream = match stream_res {
            Ok(s) => s,
            Err(e) => {
                emit_log(&e, "error");
                cleanup().await;
                let _ = app.emit(&format!("connection-failed-{}", session_id_clone), serde_json::json!({"reason": e}));
                return;
            }
        };

        emit_log("Starting SSH Handshake and establishing secure session...", "info");
        let mut config = client::Config::default();
        // russh sends a keepalive request every 30s once the connection is up.
        // Without this, idle servers behind NAT/firewall happily drop the TCP
        // session after a few minutes of silence. Pair it with the watcher
        // task below so we surface lost connections to the UI promptly.
        config.keepalive_interval = Some(Duration::from_secs(30));
        // Bigger receive window + max-allowed packet size: lets SFTP/tunnel
        // streams keep the BDP full on high-latency links. Default 2 MiB
        // window caps a single channel at ~16 Mbps over 1s RTT; 8 MiB lifts
        // that ceiling well above typical home/office links. 65535 is the
        // protocol max for maximum_packet_size (russh enforces this).
        config.window_size = 8 * 1024 * 1024;
        config.maximum_packet_size = 65535;
        // Widen the negotiation set to match what OpenSSH ships — but only
        // in builds that include the OpenSSL backend (release CI via the
        // `full-ssh-algos` feature). russh's default `Preferred` only lists
        // curve25519 + DH-G14-SHA256 for KEX and Ed25519 + ECDSA-P256 for
        // host keys, which fails "No common key/kex algorithm" against
        // older servers, embedded SSH stacks, and any shared host still
        // using RSA host keys. The full list below adds legacy DH groups,
        // the entire RSA host-key family, and the HMAC-SHA1 MAC variants.
        // Order is strongest-first; russh negotiates by picking the first
        // entry from our list that the server also offers. Default (local
        // debug) builds use russh's defaults, which is fine for connecting
        // to any modern server.
        #[cfg(feature = "full-ssh-algos")]
        {
            config.preferred = russh::Preferred {
                kex: &[
                    russh::kex::CURVE25519,
                    russh::kex::CURVE25519_PRE_RFC_8731,
                    russh::kex::DH_G14_SHA256,
                    russh::kex::DH_G14_SHA1,
                    russh::kex::DH_G1_SHA1,
                    russh::kex::EXTENSION_SUPPORT_AS_CLIENT,
                    russh::kex::EXTENSION_OPENSSH_STRICT_KEX_AS_CLIENT,
                ],
                key: &[
                    russh_keys::key::ED25519,
                    russh_keys::key::ECDSA_SHA2_NISTP256,
                    russh_keys::key::RSA_SHA2_512,
                    russh_keys::key::RSA_SHA2_256,
                    russh_keys::key::SSH_RSA,
                ],
                cipher: &[
                    russh::cipher::CHACHA20_POLY1305,
                    russh::cipher::AES_256_GCM,
                    russh::cipher::AES_256_CTR,
                    russh::cipher::AES_192_CTR,
                    russh::cipher::AES_128_CTR,
                ],
                mac: &[
                    russh::mac::HMAC_SHA512_ETM,
                    russh::mac::HMAC_SHA256_ETM,
                    russh::mac::HMAC_SHA512,
                    russh::mac::HMAC_SHA256,
                    russh::mac::HMAC_SHA1_ETM,
                    russh::mac::HMAC_SHA1,
                ],
                compression: &["none", "zlib", "zlib@openssh.com"],
            };
        }
        let config = Arc::new(config);
        
        let connect_future = client::connect_stream(config, StreamWrapper(stream), handler);

        match tokio::time::timeout(Duration::from_secs(15), connect_future).await {
            Ok(Ok(mut session)) => {
                emit_log("SSH Handshake complete. Authenticating user...", "info");
                
                let final_pass = custom_password.or(password);
                
                let auth_res = if let Some((private_key, passphrase)) = key_data {
                    emit_log("Attempting Private Key Authentication...", "info");
                    let normalized_key = private_key.replace("\r\n", "\n");
                    match russh_keys::decode_secret_key(&normalized_key, passphrase.as_deref()) {
                        Ok(keypair) => {
                            let key_arc = std::sync::Arc::new(keypair);
                            session.authenticate_publickey(&effective_user, key_arc).await
                        }
                        Err(e) => {
                            // RSA private keys only work in builds that include the
                            // OpenSSL backend (release CI). Local debug builds skip
                            // OpenSSL to stay Perl-free, so surface a targeted hint
                            // instead of the raw "Unsupported key type rsa" string.
                            #[cfg(not(feature = "full-ssh-algos"))]
                            {
                                let err_str = e.to_string();
                                if err_str.contains("Unsupported key type rsa")
                                    || err_str.contains("rsa")
                                    || private_key.contains("RSA PRIVATE KEY")
                                {
                                    emit_log("RSA private keys aren't supported in this debug build — use Ed25519 for local testing, or grab a release build from GitHub for full RSA support.", "error");
                                }
                            }
                            emit_log(&format!("Failed to parse private key: {}", e), "error");
                            Err(russh::Error::from(std::io::Error::new(
                                std::io::ErrorKind::InvalidData,
                                e.to_string(),
                            )))
                        }
                    }
                } else if let Some(pass) = final_pass {
                    emit_log("Attempting Password Authentication...", "info");
                    session.authenticate_password(&effective_user, pass).await
                } else {
                    emit_log("Neither private key nor password auth credentials provided.", "error");
                    Ok(false)
                };

                match auth_res {
                    Ok(true) => {
                        emit_log("Authentication successful. Session ready.", "success");
                        let session_arc = Arc::new(Mutex::new(session));
                        state_connections
                            .lock()
                            .await
                            .insert(session_id_clone.clone(), Arc::clone(&session_arc));
                        let _ = app.emit(
                            &format!("connection-success-{}", session_id_clone),
                            serde_json::json!({}),
                        );

                        // Auto-start any tunnel rules the user attached to this
                        // server. Failures are surfaced in-app via the tunnel
                        // status event — they don't fail the SSH connect.
                        match serde_json::from_str::<Vec<tunnel::TunnelSpec>>(&tunnels_json) {
                            Ok(specs) => {
                                for spec in specs {
                                    let started = tunnel::start_tunnel(
                                        app.clone(),
                                        session_id_clone.clone(),
                                        Arc::clone(&session_arc),
                                        Arc::clone(&state_tunnels),
                                        Arc::clone(&session_forwarded_targets),
                                        spec.clone(),
                                    ).await;
                                    match started {
                                        Ok(id) => emit_log(
                                            &format!("Tunnel started [{}]: {} {}", id, spec.kind, spec.local),
                                            "info",
                                        ),
                                        Err(e) => emit_log(
                                            &format!("Tunnel start failed ({} {}): {}", spec.kind, spec.local, e),
                                            "error",
                                        ),
                                    }
                                }
                            }
                            Err(e) => emit_log(&format!("Tunnels JSON parse error: {}", e), "error"),
                        }

                        // Health watcher: polls the SSH handle every 5s. If
                        // `is_closed()` flips to true while the session is
                        // still registered (i.e. the user did NOT call
                        // disconnect_session explicitly), we fire a
                        // `session-disconnected-{id}` event so the UI can lock
                        // down the terminal/SFTP and show a reconnect prompt.
                        // An explicit disconnect removes the map entry, which
                        // the watcher detects and exits silently — no event.
                        let app_w = app.clone();
                        let sid_w = session_id_clone.clone();
                        let state_w = Arc::clone(&state_connections);
                        let state_sftp_w = Arc::clone(&state_sftp_sessions);
                        tauri::async_runtime::spawn(async move {
                            loop {
                                tokio::time::sleep(Duration::from_secs(5)).await;

                                let handle_opt = {
                                    let conns = state_w.lock().await;
                                    conns.get(&sid_w).cloned()
                                };
                                let handle_arc = match handle_opt {
                                    Some(h) => h,
                                    None => break, // explicit disconnect — quiet
                                };

                                let is_closed = {
                                    let h = handle_arc.lock().await;
                                    h.is_closed()
                                };

                                if is_closed {
                                    state_sftp_w.lock().await.remove(&sid_w);
                                    state_w.lock().await.remove(&sid_w);
                                    let _ = app_w.emit(
                                        &format!("session-disconnected-{}", sid_w),
                                        serde_json::json!({
                                            "reason": "Connection lost"
                                        }),
                                    );
                                    break;
                                }
                            }
                        });
                    },
                    Ok(false) => {
                        emit_log("Authentication failed (Access Denied).", "error");
                        let _ = app.emit(&format!("connection-failed-{}", session_id_clone), serde_json::json!({"reason": "Access Denied", "is_auth_error": true}));
                    },
                    Err(e) => {
                        emit_log(&format!("Auth error: {}", e), "error");
                        let _ = app.emit(&format!("connection-failed-{}", session_id_clone), serde_json::json!({"reason": e.to_string(), "is_auth_error": true}));
                    }
                }
            },
            Ok(Err(e)) => {
                let pretty = humanize_network_err(&e.to_string(), &host, port, "SSH handshake");
                emit_log(&pretty, "error");
                let _ = app.emit(&format!("connection-failed-{}", session_id_clone), serde_json::json!({"reason": pretty}));
            },
            Err(_) => {
                let msg = format!("SSH handshake with {}:{} timed out after 15s — host may be filtering SSH or running a non-SSH service on this port", host, port);
                emit_log(&msg, "error");
                let _ = app.emit(&format!("connection-failed-{}", session_id_clone), serde_json::json!({"reason": msg}));
            }
        }

        cleanup().await;
    });
    
    Ok(())
}

/// Frontend acknowledgement of the SSH host-key prompt. The `nonce` must
/// match the value the matching `fingerprint-prompt-{session_id}` event
/// carried — without that match the response is dropped on the floor. Any
/// stale "accept" from a previous attempt cannot satisfy a fresh prompt.
#[tauri::command]
async fn verify_fingerprint_response(
    state: tauri::State<'_, SshState>,
    nonce: String,
    accepted: bool,
) -> Result<(), String> {
    if let Some(tx) = state.fp_txs.lock().await.remove(&nonce) {
        let _ = tx.send(accepted);
    }
    Ok(())
}

// ---- Port forwarding (tunnels) ---------------------------------------------

#[tauri::command]
async fn start_tunnel(
    app: tauri::AppHandle,
    state: tauri::State<'_, SshState>,
    session_id: String,
    spec: tunnel::TunnelSpec,
) -> Result<String, String> {
    let handle = {
        let conns = state.connections.lock().await;
        conns.get(&session_id).cloned()
            .ok_or_else(|| "Session not connected".to_string())?
    };
    let forwarded = {
        let map = state.forwarded_targets.lock().await;
        map.get(&session_id).cloned()
            .ok_or_else(|| "Session forwarded-targets map missing — reconnect first".to_string())?
    };
    tunnel::start_tunnel(app, session_id, handle, Arc::clone(&state.tunnels), forwarded, spec).await
}

#[tauri::command]
async fn stop_tunnel(
    state: tauri::State<'_, SshState>,
    tunnel_id: String,
) -> Result<(), String> {
    tunnel::stop_tunnel(&state.tunnels, &tunnel_id).await
}

#[tauri::command]
async fn list_tunnels(
    state: tauri::State<'_, SshState>,
    session_id: Option<String>,
) -> Result<Vec<tunnel::TunnelStatus>, String> {
    Ok(tunnel::list_tunnels(&state.tunnels, session_id.as_deref()).await)
}

// ----- Mirror commands -----------------------------------------------------

#[tauri::command]
async fn mirror_dry_run(
    ssh: tauri::State<'_, SshState>,
    session_id: String,
    spec: mirror::MirrorSpec,
) -> Result<mirror::DryRunReport, String> {
    let handle = {
        let conns = ssh.connections.lock().await;
        conns.get(&session_id).cloned()
            .ok_or_else(|| "Session not connected".to_string())?
    };
    mirror::dry_run(handle, spec).await
}

#[tauri::command]
async fn start_mirror(
    app: tauri::AppHandle,
    ssh: tauri::State<'_, SshState>,
    mirrors: tauri::State<'_, MirrorMap>,
    session_id: String,
    spec: mirror::MirrorSpec,
) -> Result<String, String> {
    let handle = {
        let conns = ssh.connections.lock().await;
        conns.get(&session_id).cloned()
            .ok_or_else(|| "Session not connected".to_string())?
    };
    mirror::start(app, session_id, handle, Arc::clone(&mirrors), spec).await
}

#[tauri::command]
async fn stop_mirror(
    mirrors: tauri::State<'_, MirrorMap>,
    mirror_id: String,
) -> Result<(), String> {
    mirror::stop(&mirrors, &mirror_id).await
}

#[tauri::command]
async fn list_mirrors(
    mirrors: tauri::State<'_, MirrorMap>,
    session_id: Option<String>,
) -> Result<Vec<mirror::MirrorStatus>, String> {
    Ok(mirror::list(&mirrors, session_id.as_deref()).await)
}

/// Native OS folder picker. Used by MirrorsPanel to fill in `local`.
#[tauri::command]
async fn pick_local_directory() -> Result<Option<String>, String> {
    let path = rfd::AsyncFileDialog::new().pick_folder().await;
    Ok(path.map(|p| p.path().to_string_lossy().into_owned()))
}

#[tauri::command]
async fn disconnect_session(
    state: tauri::State<'_, SshState>,
    mirrors: tauri::State<'_, MirrorMap>,
    session_id: String,
) -> Result<(), String> {
    // Stop all forwarders so their listener sockets are released before the
    // SSH handle is dropped (otherwise newly-incoming connections would just
    // bounce off a dead channel).
    tunnel::stop_all_for_session(&state.tunnels, &session_id).await;
    state.forwarded_targets.lock().await.remove(&session_id);
    // Same idea for any mirror that's still running — its watcher would
    // try to push uploads through a dead SSH handle otherwise.
    mirror::stop_all_for_session(&mirrors, &session_id).await;
    // Drop SFTP first so the channel it holds is freed before we tear down the
    // underlying SSH handle.
    state.sftp_sessions.lock().await.remove(&session_id);
    state.connections.lock().await.remove(&session_id);
    // Wipe any temp files this session left behind (live-edit downloads).
    // Best-effort: failures are usually because an editor still holds a lock
    // on a file, in which case the file persists until the OS cleans temp.
    let session_temp_dir = std::env::temp_dir().join(format!("submarine_sftp_{}", session_id));
    if session_temp_dir.exists() {
        let _ = std::fs::remove_dir_all(&session_temp_dir);
    }
    Ok(())
}

#[tauri::command]
async fn open_terminal(app: tauri::AppHandle, state: tauri::State<'_, SshState>, session_id: String, terminal_id: String, cols: u32, rows: u32) -> Result<(), String> {
    use russh::ChannelMsg;
    use tauri::Emitter;
    use std::sync::Arc;
    use crate::ssh_manager::TerminalCommand;

    let session_arc = {
        let mut connections = state.connections.lock().await;
        if let Some(sess) = connections.get_mut(&session_id) {
            Arc::clone(sess)
        } else {
            return Err("Session not connected".into());
        }
    };

    let session = session_arc.lock().await;
    let mut channel = session.channel_open_session().await.map_err(|e| e.to_string())?;
    
    // Request PTY
    channel.request_pty(false, "xterm-256color", cols, rows, 0, 0, &[]).await.map_err(|e| e.to_string())?;
    channel.request_shell(true).await.map_err(|e| e.to_string())?;

    let (tx, mut rx) = tokio::sync::mpsc::channel::<TerminalCommand>(32);
    // Last-wins watch channel for PTY resizes. The PTY task selects on
    // changes; bursty resize events (e.g. window drag) collapse to the
    // final value rather than competing with keystrokes on the data
    // mpsc. Seed with the initial size so the watch is always populated.
    let (resize_tx, mut resize_rx) = tokio::sync::watch::channel(
        crate::ssh_manager::PtySize { cols, rows },
    );
    state.terminal_txs.lock().await.insert(terminal_id.clone(), tx);
    state.resize_txs.lock().await.insert(terminal_id.clone(), resize_tx);

    let terminal_id_clone = terminal_id.clone();
    let app_clone = app.clone();

    tauri::async_runtime::spawn(async move {
        loop {
            tokio::select! {
                msg_opt = channel.wait() => {
                    match msg_opt {
                        Some(ChannelMsg::Data { ref data }) => {
                            let _ = app_clone.emit(&format!("terminal-output-{}", terminal_id_clone), data.to_vec());
                        },
                        Some(ChannelMsg::ExtendedData { ref data, ext: _ }) => {
                            let _ = app_clone.emit(&format!("terminal-output-{}", terminal_id_clone), data.to_vec());
                        },
                        Some(ChannelMsg::Eof) => break,
                        Some(ChannelMsg::Close) => break,
                        Some(_) => {},
                        None => break, // channel closed (e.g. after disconnect_session)
                    }
                },
                opt_cmd = rx.recv() => {
                    match opt_cmd {
                        Some(cmd) => match cmd {
                            TerminalCommand::Data(data) => {
                                if channel.data(&data[..]).await.is_err() {
                                    break;
                                }
                            }
                        },
                        None => {
                            let _ = channel.close().await;
                            break;
                        }
                    }
                },
                // `changed().await` resolves on every Sender::send(). We
                // then read the LATEST value with .borrow() so coalesced
                // bursts collapse to one window_change call.
                changed = resize_rx.changed() => {
                    if changed.is_err() { break; } // all senders dropped
                    let size = *resize_rx.borrow();
                    let _ = channel.window_change(size.cols, size.rows, 0, 0).await;
                }
            }
        }
        let _ = app_clone.emit(&format!("terminal-closed-{}", terminal_id_clone), serde_json::json!({}));
    });

    Ok(())
}

#[tauri::command]
async fn write_terminal_data(state: tauri::State<'_, SshState>, terminal_id: String, data: Vec<u8>) -> Result<(), String> {
    use crate::ssh_manager::TerminalCommand;
    if let Some(tx) = state.terminal_txs.lock().await.get(&terminal_id) {
        let _ = tx.send(TerminalCommand::Data(data)).await;
    }
    Ok(())
}

#[tauri::command]
async fn resize_terminal(state: tauri::State<'_, SshState>, terminal_id: String, cols: u32, rows: u32) -> Result<(), String> {
    if let Some(tx) = state.resize_txs.lock().await.get(&terminal_id) {
        // send_replace overwrites the current value unconditionally —
        // perfect for a coalescing last-wins channel.
        let _ = tx.send(crate::ssh_manager::PtySize { cols, rows });
    }
    Ok(())
}

#[tauri::command]
async fn close_terminal(state: tauri::State<'_, SshState>, terminal_id: String) -> Result<(), String> {
    state.terminal_txs.lock().await.remove(&terminal_id);
    state.resize_txs.lock().await.remove(&terminal_id);
    Ok(())
}

#[derive(serde::Serialize)]
struct SftpFileEntry {
    name: String,
    path: String,
    is_dir: bool,
    size: u64,
    permissions: Option<u32>,
    uid: Option<u32>,
    gid: Option<u32>,
    modified: Option<u64>,
}

#[derive(serde::Serialize)]
struct SftpListResult {
    current_path: String,
    entries: Vec<SftpFileEntry>,
}

pub async fn get_sftp_session(
    state: &SshState,
    session_id: &str,
) -> Result<Arc<russh_sftp::client::SftpSession>, String> {
    // Reuse one SFTP subsystem per SSH session to avoid leaking server-side
    // channels.
    if let Some(s) = state.sftp_sessions.lock().await.get(session_id) {
        return Ok(Arc::clone(s));
    }

    let session_arc = {
        let connections = state.connections.lock().await;
        if let Some(sess) = connections.get(session_id) {
            Arc::clone(sess)
        } else {
            return Err("Session not connected".into());
        }
    };

    let session = session_arc.lock().await;
    let channel = session.channel_open_session().await.map_err(|e| e.to_string())?;
    channel.request_subsystem(true, "sftp").await.map_err(|e| e.to_string())?;
    let sftp = russh_sftp::client::SftpSession::new(channel.into_stream()).await.map_err(|e| e.to_string())?;
    let arc = Arc::new(sftp);

    let mut cache = state.sftp_sessions.lock().await;
    if let Some(existing) = cache.get(session_id) {
        // Another caller raced us; keep the existing one and drop ours.
        return Ok(Arc::clone(existing));
    }
    cache.insert(session_id.to_string(), Arc::clone(&arc));
    Ok(arc)
}

#[tauri::command]
async fn sftp_list_dir(
    state: tauri::State<'_, SshState>,
    session_id: String,
    path: String,
) -> Result<SftpListResult, String> {
    let sftp = get_sftp_session(&state, &session_id).await?;
    let target_path = if path.is_empty() { ".".to_string() } else { path.clone() };
    let canonical_path = sftp.canonicalize(&target_path).await.map_err(|e| e.to_string())?;
    
    let mut read_dir = sftp.read_dir(&canonical_path).await.map_err(|e| e.to_string())?;
    let mut entries = Vec::new();
    while let Some(entry) = read_dir.next() {
        let name = entry.file_name();
        if name == "." || name == ".." {
            continue;
        }
        let is_dir = entry.file_type().is_dir();
        let metadata = entry.metadata();
        let size = metadata.size.unwrap_or(0);
        let permissions = metadata.permissions;
        let uid = metadata.uid;
        let gid = metadata.gid;
        let modified = metadata.mtime.map(|t| t as u64);
        
        let entry_path = if canonical_path.ends_with('/') {
            format!("{}{}", canonical_path, name)
        } else {
            format!("{}/{}", canonical_path, name)
        };
        
        entries.push(SftpFileEntry {
            name,
            path: entry_path,
            is_dir,
            size,
            permissions,
            uid,
            gid,
            modified,
        });
    }
    
    // Sort: directories first, then alphabetically
    entries.sort_by(|a, b| {
        if a.is_dir != b.is_dir {
            b.is_dir.cmp(&a.is_dir)
        } else {
            a.name.to_lowercase().cmp(&b.name.to_lowercase())
        }
    });
    
    Ok(SftpListResult {
        current_path: canonical_path,
        entries,
    })
}

#[tauri::command]
async fn sftp_create_dir(
    state: tauri::State<'_, SshState>,
    session_id: String,
    path: String,
) -> Result<(), String> {
    let sftp = get_sftp_session(&state, &session_id).await?;
    sftp.create_dir(path).await.map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
async fn sftp_remove_file(
    state: tauri::State<'_, SshState>,
    session_id: String,
    path: String,
) -> Result<(), String> {
    let sftp = get_sftp_session(&state, &session_id).await?;
    sftp.remove_file(path).await.map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
async fn sftp_remove_dir(
    state: tauri::State<'_, SshState>,
    session_id: String,
    path: String,
) -> Result<(), String> {
    let sftp = get_sftp_session(&state, &session_id).await?;
    sftp.remove_dir(path).await.map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
async fn sftp_rename(
    state: tauri::State<'_, SshState>,
    session_id: String,
    oldpath: String,
    newpath: String,
) -> Result<(), String> {
    let sftp = get_sftp_session(&state, &session_id).await?;

    // First try the rename as-is — the common case is the target's parent
    // already exists.
    if let Err(first_err) = sftp.rename(&oldpath, &newpath).await {
        // SSH_FX_FAILURE on rename is most commonly "destination parent
        // directory doesn't exist" — user types a path into a subfolder
        // they haven't created yet. Pre-check the parent explicitly so
        // we don't paper over real errors (permission denied, destination
        // already exists, cross-filesystem) with a silent retry that
        // would just swap one confusing message for another.
        let parent_str = std::path::Path::new(&newpath)
            .parent()
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .filter(|s| !s.is_empty() && s != "/");
        let needs_mkdir = match &parent_str {
            Some(p) => sftp.metadata(p.as_str()).await.is_err(),
            None => false,
        };
        if !needs_mkdir {
            return Err(format!("rename '{}' -> '{}': {}", oldpath, newpath, first_err));
        }
        // mkdir -p the missing chain. AlreadyExists is treated as success
        // so a parallel rename racing into the same tree doesn't break us.
        let p = parent_str.unwrap();
        let parts: Vec<&str> = p.trim_start_matches('/').split('/').filter(|s| !s.is_empty()).collect();
        let mut cur = String::from("/");
        for part in parts {
            if cur != "/" { cur.push('/'); }
            cur.push_str(part);
            if sftp.metadata(cur.as_str()).await.is_ok() { continue; }
            if let Err(e) = sftp.create_dir(&cur).await {
                let msg = e.to_string().to_lowercase();
                if msg.contains("exist") { continue; }
                return Err(format!("create destination parent '{}': {}", cur, e));
            }
        }
        sftp.rename(&oldpath, &newpath).await
            .map_err(|e| format!("rename '{}' -> '{}' (after creating parent): {}", oldpath, newpath, e))?;
    }
    Ok(())
}

#[tauri::command]
async fn sftp_set_permissions(
    state: tauri::State<'_, SshState>,
    session_id: String,
    path: String,
    permissions: u32,
) -> Result<(), String> {
    let sftp = get_sftp_session(&state, &session_id).await?;
    // Propagate metadata fetch errors instead of falling back to a zeroed
    // FileAttributes. Without this, a transient network blip or a perms
    // failure during read would have us send `set_metadata` with uid=gid
    // =size=0 — silently clobbering ownership and other attributes.
    let mut metadata = sftp.metadata(&path).await
        .map_err(|e| format!("[SFTP] METADATA_READ_FAILED: {}", e))?;
    metadata.permissions = Some(permissions);
    sftp.set_metadata(path, metadata).await.map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
async fn sftp_set_owner(
    state: tauri::State<'_, SshState>,
    session_id: String,
    path: String,
    uid: Option<u32>,
    gid: Option<u32>,
) -> Result<(), String> {
    let sftp = get_sftp_session(&state, &session_id).await?;
    let mut metadata = sftp.metadata(&path).await
        .map_err(|e| format!("[SFTP] METADATA_READ_FAILED: {}", e))?;
    metadata.uid = uid;
    metadata.gid = gid;
    sftp.set_metadata(path, metadata).await.map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
async fn sftp_download_file(
    app: tauri::AppHandle,
    state: tauri::State<'_, SshState>,
    session_id: String,
    remote_path: String,
    local_path: String,
) -> Result<(), String> {
    use tauri::Emitter;
    use tokio::io::AsyncReadExt;

    let sftp = get_sftp_session(&state, &session_id).await?;

    // Stat first so we can report a progress percentage. If the server doesn't
    // know the size (some edge SFTP servers omit it), we still report bytes
    // transferred so the UI can show throughput at least.
    let total = match sftp.metadata(&remote_path).await {
        Ok(m) => m.size.unwrap_or(0),
        Err(_) => 0,
    };
    let name = std::path::Path::new(&remote_path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("file")
        .to_string();
    let id = transfer_id();
    let event_name = format!("sftp-transfer-{}", session_id);
    let emit_progress = |bytes: u64, status: &str, error: Option<String>| {
        let _ = app.emit(
            &event_name,
            serde_json::json!({
                "id": id, "name": name, "kind": "download",
                "bytes": bytes, "total": total,
                "status": status, "error": error,
            }),
        );
    };
    emit_progress(0, "progress", None);

    let mut remote_file = sftp
        .open(&remote_path)
        .await
        .map_err(|e| { emit_progress(0, "error", Some(e.to_string())); format!("Failed to open remote file: {}", e) })?;
    let mut local_file = tokio::fs::File::create(&local_path)
        .await
        .map_err(|e| { emit_progress(0, "error", Some(e.to_string())); format!("Failed to create local file: {}", e) })?;

    // 256 KiB chunks: large enough to keep the SSH window pipelined on
    // high-latency links (with window_size=8 MiB we want ~16+ chunks in
    // flight), small enough to keep per-iteration latency low for the
    // progress meter.
    let mut buf = vec![0u8; 256 * 1024];
    let mut transferred: u64 = 0;
    let mut last_report = std::time::Instant::now();
    use tokio::io::AsyncWriteExt;
    loop {
        let n = remote_file
            .read(&mut buf)
            .await
            .map_err(|e| { emit_progress(transferred, "error", Some(e.to_string())); format!("read: {}", e) })?;
        if n == 0 { break; }
        local_file
            .write_all(&buf[..n])
            .await
            .map_err(|e| { emit_progress(transferred, "error", Some(e.to_string())); format!("write: {}", e) })?;
        transferred += n as u64;
        // Throttle progress events: each one crosses the IPC boundary, and
        // 10 Hz is more than enough for a smooth progress bar.
        if last_report.elapsed() >= std::time::Duration::from_millis(100) {
            emit_progress(transferred, "progress", None);
            last_report = std::time::Instant::now();
        }
    }
    local_file.flush().await.map_err(|e| format!("flush: {}", e))?;
    emit_progress(transferred, "done", None);
    Ok(())
}

#[tauri::command]
async fn sftp_upload_file(
    app: tauri::AppHandle,
    state: tauri::State<'_, SshState>,
    session_id: String,
    local_path: String,
    remote_path: String,
) -> Result<(), String> {
    use russh_sftp::protocol::OpenFlags;
    use tauri::Emitter;
    use tokio::io::AsyncWriteExt;

    let sftp = get_sftp_session(&state, &session_id).await?;

    let total = std::fs::metadata(&local_path)
        .map(|m| m.len())
        .unwrap_or(0);
    let name = std::path::Path::new(&local_path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("file")
        .to_string();
    let id = transfer_id();
    let event_name = format!("sftp-transfer-{}", session_id);
    let emit_progress = |bytes: u64, status: &str, error: Option<String>| {
        let _ = app.emit(
            &event_name,
            serde_json::json!({
                "id": id, "name": name, "kind": "upload",
                "bytes": bytes, "total": total,
                "status": status, "error": error,
            }),
        );
    };
    emit_progress(0, "progress", None);

    // Stream the file from disk in chunks rather than slurping the whole thing
    // into a Vec — keeps memory bounded for multi-GB transfers and lets us
    // emit progress along the way.
    let mut local_file = tokio::fs::File::open(&local_path)
        .await
        .map_err(|e| { emit_progress(0, "error", Some(e.to_string())); format!("Failed to read local file: {}", e) })?;
    let mut remote_file = sftp
        .open_with_flags(
            remote_path,
            OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::TRUNCATE,
        )
        .await
        .map_err(|e| { emit_progress(0, "error", Some(e.to_string())); format!("Failed to open remote file: {}", e) })?;

    // See sftp_download_file — 256 KiB matches the bigger SSH window.
    let mut buf = vec![0u8; 256 * 1024];
    let mut transferred: u64 = 0;
    let mut last_report = std::time::Instant::now();
    use tokio::io::AsyncReadExt;
    loop {
        let n = local_file
            .read(&mut buf)
            .await
            .map_err(|e| { emit_progress(transferred, "error", Some(e.to_string())); format!("read: {}", e) })?;
        if n == 0 { break; }
        remote_file
            .write_all(&buf[..n])
            .await
            .map_err(|e| { emit_progress(transferred, "error", Some(e.to_string())); format!("write: {}", e) })?;
        transferred += n as u64;
        if last_report.elapsed() >= std::time::Duration::from_millis(100) {
            emit_progress(transferred, "progress", None);
            last_report = std::time::Instant::now();
        }
    }
    remote_file
        .shutdown()
        .await
        .map_err(|e| format!("Failed to close remote file: {}", e))?;
    emit_progress(transferred, "done", None);
    Ok(())
}

/// Translate raw socket / SSH error messages into something a human can act
/// on. Most russh / tokio errors come out as "tcp: io error: ..." with the
/// useful detail buried — this lifts the common cases up to a clear sentence
/// while still falling back to the original text for anything unfamiliar.
fn humanize_network_err(raw: &str, host: &str, port: i32, label: &str) -> String {
    let lower = raw.to_lowercase();
    let target = if port > 0 { format!("{}:{}", host, port) } else { host.to_string() };

    if lower.contains("connection refused") {
        return format!("{}: {} refused the connection (is the SSH server running on this port?)", label, target);
    }
    if lower.contains("network is unreachable") || lower.contains("network unreachable") {
        return format!("{}: network is unreachable — check VPN / firewall / internet", label);
    }
    if lower.contains("no route to host") {
        return format!("{}: no route to {} — host is down or blocked", label, target);
    }
    if lower.contains("name or service not known")
        || lower.contains("nodename nor servname")
        || lower.contains("failed to lookup address")
        || lower.contains("no such host")
        || lower.contains("dns")
    {
        return format!("{}: could not resolve hostname {}", label, host);
    }
    if lower.contains("timed out") || lower.contains("timeout") {
        return format!("{}: {} did not respond in time", label, target);
    }
    if lower.contains("connection reset") || lower.contains("broken pipe") {
        return format!("{}: {} closed the connection", label, target);
    }
    if lower.contains("permission denied") {
        return format!("{}: permission denied (check key file readability)", label);
    }
    // Fallback — keep the raw detail so power users can still see it.
    format!("{}: {}", label, raw)
}

// Monotonically-increasing per-transfer id. The frontend uses it to group
// progress events into one updatable card per transfer.
fn transfer_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("{}-{}", ts, seq)
}

#[tauri::command]
async fn sftp_open_remote_file(
    app_handle: tauri::AppHandle,
    state: tauri::State<'_, SshState>,
    session_id: String,
    remote_path: String,
) -> Result<(), String> {
    use tauri::Emitter;
    
    let sftp = get_sftp_session(&state, &session_id).await?;
    let filename = std::path::Path::new(&remote_path)
        .file_name()
        .ok_or("Invalid remote path")?
        .to_string_lossy()
        .to_string();

    // Read file data
    let data = sftp.read(&remote_path).await.map_err(|e| format!("Failed to read remote file: {}", e))?;

    // Per-session subdirectory so we can sweep everything cleanly on
    // disconnect rather than leaving loose `submarine_sftp_*` files in the global
    // temp dir. The directory is also a smaller blast radius for any path-
    // related shenanigans (each editor sees only files from one session).
    let session_temp_dir = std::env::temp_dir().join(format!("submarine_sftp_{}", session_id));
    std::fs::create_dir_all(&session_temp_dir)
        .map_err(|e| format!("Failed to create temp dir: {}", e))?;
    let temp_file_path = session_temp_dir.join(&filename);
    std::fs::write(&temp_file_path, &data).map_err(|e| format!("Failed to write temporary file: {}", e))?;

    // Open local temp file in system default application
    let open_res = open::that(&temp_file_path);
    if let Err(e) = open_res {
        return Err(format!("Failed to open file: {}", e));
    }

    // Spawn modification watcher task in background
    let connections_clone = Arc::clone(&state.connections);
    let app_handle_clone = app_handle.clone();
    let session_id_clone = session_id.clone();
    let remote_path_clone = remote_path.clone();
    let filename_clone = filename.clone();
    let temp_file_path_clone = temp_file_path.clone();

    tokio::spawn(async move {
        let mut last_modified = std::fs::metadata(&temp_file_path_clone)
            .ok()
            .and_then(|m| m.modified().ok());

        // Watch for 2 hours, polling every 1.5 seconds
        for _ in 0..4800 {
            tokio::time::sleep(tokio::time::Duration::from_millis(1500)).await;

            if !temp_file_path_clone.exists() {
                break;
            }

            let current_modified = std::fs::metadata(&temp_file_path_clone)
                .ok()
                .and_then(|m| m.modified().ok());

            if current_modified != last_modified {
                last_modified = current_modified;

                // Sync the change back
                let upload_res = async {
                    let session_arc = {
                        let connections = connections_clone.lock().await;
                        connections.get(&session_id_clone).map(|sess| Arc::clone(sess))
                    };

                    let session_arc = match session_arc {
                        Some(sess) => sess,
                        None => return Err("SSH session disconnected".to_string()),
                    };

                    let session = session_arc.lock().await;
                    let channel = session.channel_open_session().await.map_err(|e| e.to_string())?;
                    channel.request_subsystem(true, "sftp").await.map_err(|e| e.to_string())?;
                    let sftp = russh_sftp::client::SftpSession::new(channel.into_stream()).await.map_err(|e| e.to_string())?;

                    use russh_sftp::protocol::OpenFlags;
                    use tokio::io::AsyncWriteExt;
                    let content = std::fs::read(&temp_file_path_clone).map_err(|e| format!("Failed to read file: {}", e))?;
                    // Truncate so shortening the file doesn't leave the old
                    // tail behind on the server.
                    let mut remote_file = sftp
                        .open_with_flags(
                            &remote_path_clone,
                            OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::TRUNCATE,
                        )
                        .await
                        .map_err(|e| format!("Failed to open remote file: {}", e))?;
                    remote_file
                        .write_all(&content)
                        .await
                        .map_err(|e| format!("Failed to write to remote: {}", e))?;
                    remote_file
                        .shutdown()
                        .await
                        .map_err(|e| format!("Failed to close remote file: {}", e))?;
                    Ok::<(), String>(())
                }.await;

                if let Err(e) = upload_res {
                    let _ = app_handle_clone.emit(
                        &format!("sftp-sync-status-{}", session_id_clone),
                        serde_json::json!({ "status": "error", "message": format!("Auto-sync failed: {}", e) })
                    );
                } else {
                    let _ = app_handle_clone.emit(
                        &format!("sftp-sync-status-{}", session_id_clone),
                        serde_json::json!({ "status": "success", "message": format!("Auto-synced {}", filename_clone) })
                    );
                }
            }
        }
        // The watcher exited (timeout, file disappeared, or session gone).
        // Wipe the temp file so the remote contents aren't left lying around
        // in OS temp once editing is done. Errors are intentionally ignored
        // — on Windows the editor may still hold a lock on the file, and the
        // worst case is the file persists until the OS cleans temp.
        let _ = std::fs::remove_file(&temp_file_path_clone);
    });

    Ok(())
}

#[tauri::command]
async fn sftp_prepare_drag(
    state: tauri::State<'_, SshState>,
    session_id: String,
    remote_path: String,
) -> Result<String, String> {
    let sftp = get_sftp_session(&state, &session_id).await?;
    let mut file = sftp.open(&remote_path).await.map_err(|e| e.to_string())?;
    
    // Read remote file data
    let mut data = Vec::new();
    let mut buffer = vec![0u8; 32768];
    loop {
        let n = file.read(&mut buffer).await.map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        data.extend_from_slice(&buffer[..n]);
    }
    
    // Sanitize the filename: a hostile (or compromised) SFTP server picks
    // remote_path, and we used to drop the basename straight into the OS
    // temp dir — letting the server pick any filename it wanted at top of
    // temp. Restrict to a safe leaf name (no path separators, no `..`,
    // no drive markers) and scope to a per-session subdir so parallel
    // drags don't clobber each other.
    let raw_name = std::path::Path::new(&remote_path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("file");
    let bad = |c: char| matches!(c, '/' | '\\' | ':' | '\0');
    if raw_name.is_empty() || raw_name == "." || raw_name == ".." || raw_name.contains(bad) {
        return Err(format!("refusing to drag file with unsafe name: {:?}", raw_name));
    }
    let session_dir = std::env::temp_dir().join(format!("submarine_drag_{}", session_id));
    std::fs::create_dir_all(&session_dir)
        .map_err(|e| format!("Failed to create drag staging dir: {}", e))?;
    let temp_file_path = session_dir.join(raw_name);
    std::fs::write(&temp_file_path, &data).map_err(|e| format!("Failed to write temporary file: {}", e))?;

    Ok(temp_file_path.to_string_lossy().to_string())
}

#[tauri::command]
async fn local_open_file(local_path: String) -> Result<(), String> {
    let safe = guard_local_path(&local_path, false)?;
    open::that(&safe).map_err(|e| format!("Failed to open local file: {}", e))?;
    Ok(())
}

#[tauri::command]
async fn local_open_in_explorer(local_path: String) -> Result<(), String> {
    let safe = guard_local_path(&local_path, false)?;
    if safe.is_dir() {
        open::that(&safe).map_err(|e| format!("Failed to open folder: {}", e))?;
    } else if let Some(parent) = safe.parent() {
        open::that(parent).map_err(|e| format!("Failed to open folder: {}", e))?;
    }
    Ok(())
}

#[derive(serde::Serialize)]
struct LocalFileEntry {
    name: String,
    path: String,
    is_dir: bool,
    size: u64,
    modified: Option<u64>,
}

#[tauri::command]
async fn local_home_dir() -> Result<String, String> {
    directories::UserDirs::new()
        .and_then(|d| d.home_dir().to_str().map(String::from))
        .ok_or_else(|| "Could not resolve home directory".into())
}

#[tauri::command]
async fn local_desktop_dir() -> Result<String, String> {
    // Falls back to the home directory if a Desktop folder isn't configured
    // for the user (rare on desktop OSes but possible on Linux without XDG).
    if let Some(dirs) = directories::UserDirs::new() {
        if let Some(d) = dirs.desktop_dir().and_then(|p| p.to_str()) {
            return Ok(d.to_string());
        }
        if let Some(h) = dirs.home_dir().to_str() {
            return Ok(h.to_string());
        }
    }
    Err("Could not resolve Desktop directory".into())
}

/// Defense-in-depth guard for the local-FS commands the frontend can invoke.
/// We can't lock everything down to a sandbox (the local file browser
/// legitimately needs to roam the user's disk to pick uploads), but we CAN
/// refuse the obviously destructive cases: the filesystem root, OS system
/// directories, and unresolvable paths. If the renderer is ever compromised
/// (XSS via terminal output, a future feature, etc.) this stops
/// `local_remove("C:\\")` cold.
fn guard_local_path(path: &str, allow_nonexistent: bool) -> Result<std::path::PathBuf, String> {
    let p = std::path::Path::new(path);
    let canonical = match p.canonicalize() {
        Ok(c) => c,
        Err(e) => {
            if allow_nonexistent {
                let parent = p.parent()
                    .ok_or_else(|| format!("Invalid path: {}", path))?;
                let canon_parent = parent.canonicalize()
                    .map_err(|e| format!("Invalid parent directory: {}", e))?;
                let file = p.file_name()
                    .ok_or_else(|| format!("Invalid path: {}", path))?;
                canon_parent.join(file)
            } else {
                return Err(format!("Invalid path: {}", e));
            }
        }
    };

    // Refuse the filesystem root itself (`/`, `C:\`, etc.).
    if canonical.parent().is_none() {
        return Err(format!("Refusing to operate on filesystem root: {}", canonical.display()));
    }

    let canon_norm = canonical.to_string_lossy().to_lowercase().replace('\\', "/");
    // Trim trailing slash for clean prefix matches.
    let canon_norm = canon_norm.trim_end_matches('/').to_string();

    let blocked: &[&str] = if cfg!(windows) {
        &[
            "c:/windows", "c:/program files", "c:/program files (x86)",
            "c:/programdata", "c:/system volume information", "c:/$recycle.bin",
        ]
    } else {
        &[
            "/etc", "/usr", "/bin", "/sbin", "/lib", "/lib64", "/boot",
            "/sys", "/proc", "/dev", "/var/log", "/var/run", "/root",
        ]
    };
    for prefix in blocked {
        let pfx = prefix.to_lowercase();
        if canon_norm == pfx || canon_norm.starts_with(&format!("{}/", pfx)) {
            return Err(format!("Refusing operation on system path: {}", canonical.display()));
        }
    }

    Ok(canonical)
}

#[tauri::command]
async fn local_create_dir(path: String) -> Result<(), String> {
    let safe = guard_local_path(&path, true)?;
    std::fs::create_dir_all(&safe).map_err(|e| format!("Failed to create directory: {}", e))
}

#[tauri::command]
async fn local_remove(path: String, is_dir: bool) -> Result<(), String> {
    let safe = guard_local_path(&path, false)?;
    if is_dir {
        std::fs::remove_dir_all(&safe).map_err(|e| format!("Failed to remove directory: {}", e))
    } else {
        std::fs::remove_file(&safe).map_err(|e| format!("Failed to remove file: {}", e))
    }
}

#[tauri::command]
async fn local_rename(from: String, to: String) -> Result<(), String> {
    let safe_from = guard_local_path(&from, false)?;
    let safe_to = guard_local_path(&to, true)?;
    // Same auto-mkdir-parent UX as sftp_rename: moving a file into a
    // subfolder that doesn't exist yet would otherwise fail with a
    // confusing "system cannot find the path specified" / ENOENT.
    if let Some(parent) = safe_to.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("create destination parent {:?}: {}", parent, e))?;
        }
    }
    std::fs::rename(&safe_from, &safe_to)
        .map_err(|e| format!("rename {:?} -> {:?}: {}", safe_from, safe_to, e))
}

#[tauri::command]
async fn select_local_folder() -> Result<Option<String>, String> {
    let folder = rfd::FileDialog::new()
        .set_title("Choose Local Directory")
        .pick_folder();
    Ok(folder.map(|p| p.to_string_lossy().to_string()))
}

#[tauri::command]
async fn local_list_dir(path: String) -> Result<Vec<LocalFileEntry>, String> {
    let safe = guard_local_path(&path, false)?;
    if !safe.is_dir() {
        return Err("Path is not a directory".into());
    }

    let mut entries = Vec::new();
    let read_dir = std::fs::read_dir(&safe).map_err(|e| format!("Failed to read directory: {}", e))?;

    for entry in read_dir {
        if let Ok(entry) = entry {
            let metadata = entry.metadata().ok();
            let is_dir = metadata.as_ref().map(|m| m.is_dir()).unwrap_or(false);
            let size = metadata.as_ref().map(|m| m.len()).unwrap_or(0);
            let name = entry.file_name().to_string_lossy().to_string();
            let full_path = entry.path().to_string_lossy().to_string();
            
            let modified = metadata.as_ref()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::SystemTime::UNIX_EPOCH).ok())
                .map(|d| d.as_secs());

            entries.push(LocalFileEntry {
                name,
                path: full_path,
                is_dir,
                size,
                modified,
            });
        }
    }

    // Sort: directories first, then alphabetically
    entries.sort_by(|a, b| {
        if a.is_dir != b.is_dir {
            b.is_dir.cmp(&a.is_dir)
        } else {
            a.name.to_lowercase().cmp(&b.name.to_lowercase())
        }
    });

    Ok(entries)
}

// ---------------------------------------------------------------------------
// Monitoring commands
// ---------------------------------------------------------------------------

/// Pulls the auth bundle needed to open a monitor session for a node. Returns
/// the resolved (username, password, key_pem, passphrase) following the same
/// "vault vs custom_*" rule the interactive connect path uses — so monitor
/// auth never silently diverges from what the user sees in the form.
fn resolve_node_auth_for_monitor(
    conn: &rusqlite::Connection,
    node_id: i32,
) -> Result<monitor::NodeAuth, String> {
    let mut stmt = conn.prepare("
        SELECT s.host, s.port,
               s.username, c.username,
               s.password, c.password,
               s.key_id,   c.key_id,
               s.auth_type, c.auth_type as cred_auth_type,
               s.proxy_type, s.proxy_host, s.proxy_port
        FROM servers s
        LEFT JOIN credentials c ON s.credential_id = c.id
        WHERE s.id = ?1
    ").map_err(|e| e.to_string())?;
    let mut rows = stmt.query([node_id]).map_err(|e| e.to_string())?;
    let row = rows.next().map_err(|e| e.to_string())?.ok_or("node not found")?;

    let host: String = row.get::<_, String>(0).map_err(|e| e.to_string())?;
    let port: i32 = row.get::<_, i32>(1).map_err(|e| e.to_string())?;
    let s_user: Option<String> = row.get(2).ok().flatten();
    let c_user: Option<String> = row.get(3).ok().flatten();
    let s_pass: Option<String> = row.get(4).ok().flatten();
    let c_pass: Option<String> = row.get(5).ok().flatten();
    let s_key:  Option<i32>    = row.get(6).ok().flatten();
    let c_key:  Option<i32>    = row.get(7).ok().flatten();
    let auth_type: String = row.get::<_, Option<String>>(8).ok().flatten().unwrap_or_else(|| "vault".into());
    let cred_auth_type: Option<String> = row.get(9).ok().flatten();
    let proxy_type: String = row.get::<_, Option<String>>(10).ok().flatten().unwrap_or_else(|| "none".into());
    let proxy_host: Option<String> = row.get(11).ok().flatten();
    let proxy_port: Option<i32> = row.get(12).ok().flatten();

    let (username, password, key_id) = if auth_type == "vault" {
        (c_user.unwrap_or_default(), c_pass, c_key)
    } else {
        (s_user.unwrap_or_default(), s_pass, s_key)
    };
    let effective_key_id = if auth_type == "vault" {
        if cred_auth_type.as_deref() == Some("key") { key_id } else { None }
    } else if auth_type == "custom_key" {
        key_id
    } else {
        None
    };

    let (private_key, passphrase) = if let Some(kid) = effective_key_id {
        let mut key_stmt = conn.prepare("SELECT private_key, passphrase FROM ssh_keys WHERE id = ?1")
            .map_err(|e| e.to_string())?;
        let mut krows = key_stmt.query([kid]).map_err(|e| e.to_string())?;
        if let Some(r) = krows.next().map_err(|e| e.to_string())? {
            let pk: String = r.get(0).map_err(|e| e.to_string())?;
            let pp: Option<String> = r.get(1).ok().flatten();
            (Some(pk), pp)
        } else {
            (None, None)
        }
    } else {
        (None, None)
    };

    Ok(monitor::NodeAuth {
        host,
        port: port as u16,
        username: if username.trim().is_empty() { "root".into() } else { username },
        password,
        private_key,
        passphrase,
        proxy_type,
        proxy_host: proxy_host.filter(|s| !s.is_empty()),
        proxy_port: proxy_port.map(|p| p as u16),
    })
}

fn default_metrics() -> Vec<String> {
    vec!["cpu".into(), "mem".into(), "disk".into(), "load".into()]
}

/// Look up just the display name for a node. Used by the monitor's
/// outage/recovered event payloads so the frontend can show a meaningful
/// toast ("web-01 is offline") without doing another round-trip.
fn fetch_node_name(conn: &rusqlite::Connection, node_id: i32) -> String {
    conn.query_row("SELECT name FROM servers WHERE id = ?1", [node_id], |r| r.get::<_, String>(0))
        .unwrap_or_else(|_| format!("node-{}", node_id))
}

fn load_monitor_config(
    conn: &rusqlite::Connection,
    node_id: i32,
) -> Option<(Vec<String>, Vec<monitor::CustomMetric>, bool)> {
    let mut stmt = conn.prepare("SELECT enabled_metrics, custom_metrics, paused FROM monitor_configs WHERE node_id = ?1").ok()?;
    let mut rows = stmt.query([node_id]).ok()?;
    let row = rows.next().ok()??;
    let json_metrics: String = row.get(0).ok()?;
    let json_customs: String = row.get(1).ok().unwrap_or_else(|| "[]".into());
    let paused: i32 = row.get(2).ok()?;
    let metrics: Vec<String> = serde_json::from_str(&json_metrics).unwrap_or_else(|_| default_metrics());
    let customs: Vec<monitor::CustomMetric> = serde_json::from_str(&json_customs).unwrap_or_default();
    Some((metrics, customs, paused != 0))
}

fn upsert_monitor_config(
    conn: &rusqlite::Connection,
    node_id: i32,
    metrics: &[String],
    customs: &[monitor::CustomMetric],
    paused: bool,
) -> Result<(), String> {
    let json_metrics = serde_json::to_string(metrics).map_err(|e| e.to_string())?;
    let json_customs = serde_json::to_string(customs).map_err(|e| e.to_string())?;
    conn.execute(
        "INSERT INTO monitor_configs (node_id, enabled_metrics, custom_metrics, paused) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(node_id) DO UPDATE SET enabled_metrics=excluded.enabled_metrics, custom_metrics=excluded.custom_metrics, paused=excluded.paused",
        rusqlite::params![node_id, json_metrics, json_customs, if paused { 1 } else { 0 }],
    ).map_err(|e| e.to_string())?;
    Ok(())
}

/// Frontend shape: monitor_list returns this for each known monitor (whether
/// it's been started in the live MonitorMap yet or not). UI uses it to
/// render the sidebar even before the first sample arrives.
#[derive(serde::Serialize)]
struct MonitorRow {
    node_id: i32,
    name: String,
    host: String,
    port: i32,
    enabled_metrics: Vec<String>,
    custom_metrics: Vec<monitor::CustomMetric>,
    paused: bool,
    connected: bool,
    last_error: Option<String>,
    last_sample_ts: Option<u64>,
}

fn load_settings_from_db(conn: &rusqlite::Connection) -> monitor::MonitorSettings {
    let mut stmt = match conn.prepare("SELECT json FROM monitor_settings WHERE id = 1") {
        Ok(s) => s,
        Err(_) => return monitor::MonitorSettings::default(),
    };
    let mut rows = match stmt.query([]) {
        Ok(r) => r,
        Err(_) => return monitor::MonitorSettings::default(),
    };
    if let Ok(Some(row)) = rows.next() {
        if let Ok(json) = row.get::<_, String>(0) {
            if let Ok(s) = serde_json::from_str::<monitor::MonitorSettings>(&json) {
                return s.sanitized();
            }
        }
    }
    monitor::MonitorSettings::default()
}

#[tauri::command]
async fn monitor_get_settings(
    db_state: tauri::State<'_, DbState>,
    settings: tauri::State<'_, SharedSettings>,
) -> Result<monitor::MonitorSettings, String> {
    // First-call lazy-load: if the in-memory copy is still at defaults but
    // the DB has saved values, hydrate the in-memory copy so all pollers
    // pick them up immediately. We can't tell "default vs default-saved"
    // perfectly but the worst case is idempotent.
    // Compute the DB-saved value first, then drop the std::sync::Mutex
    // guard *before* awaiting on the tokio Mutex — otherwise the future
    // captures a non-Send guard and won't compile.
    let from_db_opt: Option<monitor::MonitorSettings> = {
        let guard = db_state.conn.lock().map_err(|_| "lock")?;
        guard.as_ref().map(|conn| load_settings_from_db(conn))
    };
    if let Some(from_db) = from_db_opt {
        let mut cur = settings.lock().await;
        if *cur == monitor::MonitorSettings::default() {
            *cur = from_db;
        }
    }
    Ok(settings.lock().await.clone())
}

#[tauri::command]
async fn monitor_set_settings(
    db_state: tauri::State<'_, DbState>,
    settings: tauri::State<'_, SharedSettings>,
    new_settings: monitor::MonitorSettings,
) -> Result<monitor::MonitorSettings, String> {
    let sane = new_settings.sanitized();
    {
        let guard = db_state.conn.lock().map_err(|_| "lock")?;
        let conn = guard.as_ref().ok_or("db not ready")?;
        let json = serde_json::to_string(&sane).map_err(|e| e.to_string())?;
        conn.execute(
            "INSERT INTO monitor_settings (id, json) VALUES (1, ?1)
             ON CONFLICT(id) DO UPDATE SET json = excluded.json",
            rusqlite::params![json],
        ).map_err(|e| e.to_string())?;
    }
    save_vault_internal(&db_state)?;
    *settings.lock().await = sane.clone();
    Ok(sane)
}

#[tauri::command]
async fn monitor_list(
    db_state: tauri::State<'_, DbState>,
    map: tauri::State<'_, MonitorMap>,
) -> Result<Vec<MonitorRow>, String> {
    // Pull DB rows first so we always include configured-but-paused entries
    // even if they have no live MonitorEntry yet.
    let configs: Vec<(i32, String, String, i32, Vec<String>, Vec<monitor::CustomMetric>, bool)> = {
        let guard = db_state.conn.lock().map_err(|_| "lock")?;
        let conn = guard.as_ref().ok_or("db not ready")?;
        let mut stmt = conn.prepare("
            SELECT mc.node_id, s.name, s.host, s.port, mc.enabled_metrics, mc.custom_metrics, mc.paused
            FROM monitor_configs mc
            JOIN servers s ON s.id = mc.node_id
            ORDER BY s.name COLLATE NOCASE
        ").map_err(|e| e.to_string())?;
        let rows = stmt.query_map([], |r| {
            let metrics_json: String = r.get(4)?;
            let metrics: Vec<String> = serde_json::from_str(&metrics_json).unwrap_or_else(|_| default_metrics());
            let customs_json: String = r.get(5).unwrap_or_else(|_| "[]".into());
            let customs: Vec<monitor::CustomMetric> = serde_json::from_str(&customs_json).unwrap_or_default();
            let paused: i32 = r.get(6)?;
            Ok((r.get::<_, i32>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, i32>(3)?, metrics, customs, paused != 0))
        }).map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        for row in rows { if let Ok(v) = row { out.push(v); } }
        out
    };

    // Merge live state from MonitorMap on top.
    let live = monitor::list(map.inner().clone()).await;
    let live_by_id: std::collections::HashMap<i32, monitor::MonitorInfo> =
        live.into_iter().map(|m| (m.node_id, m)).collect();

    Ok(configs.into_iter().map(|(node_id, name, host, port, metrics, customs, paused)| {
        let live = live_by_id.get(&node_id);
        MonitorRow {
            node_id,
            name, host, port,
            enabled_metrics: metrics,
            custom_metrics: customs,
            paused,
            connected: live.map(|l| l.connected).unwrap_or(false),
            last_error: live.and_then(|l| l.last_error.clone()),
            last_sample_ts: live.and_then(|l| l.last_sample_ts),
        }
    }).collect())
}

#[tauri::command]
async fn monitor_add(
    db_state: tauri::State<'_, DbState>,
    node_id: i32,
) -> Result<(), String> {
    // Persist the config row only; the poller doesn't spawn until the user
    // explicitly clicks Resume (per the "no auto-start" rule). We still
    // resolve the auth bundle once here so adding a node with broken auth
    // fails fast instead of silently sitting in a paused state forever.
    {
        let guard = db_state.conn.lock().map_err(|_| "lock")?;
        let conn = guard.as_ref().ok_or("db not ready")?;
        let _ = resolve_node_auth_for_monitor(conn, node_id)?;
        upsert_monitor_config(conn, node_id, &default_metrics(), &[], true)?;
    }
    save_vault_internal(&db_state)?;
    Ok(())
}

#[tauri::command]
async fn monitor_remove(
    db_state: tauri::State<'_, DbState>,
    map: tauri::State<'_, MonitorMap>,
    node_id: i32,
) -> Result<(), String> {
    monitor::stop_monitor(map.inner().clone(), node_id).await;
    {
        let guard = db_state.conn.lock().map_err(|_| "lock")?;
        let conn = guard.as_ref().ok_or("db not ready")?;
        conn.execute("DELETE FROM monitor_configs WHERE node_id=?1", [node_id])
            .map_err(|e| e.to_string())?;
    }
    save_vault_internal(&db_state)?;
    Ok(())
}

#[tauri::command]
async fn monitor_set_metrics(
    db_state: tauri::State<'_, DbState>,
    map: tauri::State<'_, MonitorMap>,
    node_id: i32,
    metrics: Vec<String>,
) -> Result<(), String> {
    {
        let guard = db_state.conn.lock().map_err(|_| "lock")?;
        let conn = guard.as_ref().ok_or("db not ready")?;
        // Preserve current pause state + custom list from row.
        let (_, customs, paused) = load_monitor_config(conn, node_id)
            .unwrap_or((default_metrics(), vec![], true));
        upsert_monitor_config(conn, node_id, &metrics, &customs, paused)?;
    }
    save_vault_internal(&db_state)?;
    // If a live poller exists, hot-update it; otherwise it'll pick up on resume.
    let _ = monitor::set_enabled_metrics(map.inner().clone(), node_id, metrics).await;
    Ok(())
}

#[tauri::command]
async fn monitor_set_custom_metrics(
    db_state: tauri::State<'_, DbState>,
    map: tauri::State<'_, MonitorMap>,
    node_id: i32,
    customs: Vec<monitor::CustomMetric>,
) -> Result<(), String> {
    {
        let guard = db_state.conn.lock().map_err(|_| "lock")?;
        let conn = guard.as_ref().ok_or("db not ready")?;
        let (metrics, _, paused) = load_monitor_config(conn, node_id)
            .unwrap_or((default_metrics(), vec![], true));
        upsert_monitor_config(conn, node_id, &metrics, &customs, paused)?;
    }
    save_vault_internal(&db_state)?;
    let _ = monitor::set_custom_metrics(map.inner().clone(), node_id, customs).await;
    Ok(())
}

#[tauri::command]
async fn monitor_resume(
    app: tauri::AppHandle,
    db_state: tauri::State<'_, DbState>,
    map: tauri::State<'_, MonitorMap>,
    settings: tauri::State<'_, SharedSettings>,
    node_id: i32,
) -> Result<(), String> {
    let (auth, metrics, customs, name) = {
        let guard = db_state.conn.lock().map_err(|_| "lock")?;
        let conn = guard.as_ref().ok_or("db not ready")?;
        let (metrics, customs, _) = load_monitor_config(conn, node_id)
            .ok_or_else(|| format!("Node {} is not in the monitor list", node_id))?;
        upsert_monitor_config(conn, node_id, &metrics, &customs, false)?;
        let name = fetch_node_name(conn, node_id);
        (resolve_node_auth_for_monitor(conn, node_id)?, metrics, customs, name)
    };
    save_vault_internal(&db_state)?;

    // If a poller already exists, just hot-flip paused; otherwise spawn one.
    if monitor::set_paused(map.inner().clone(), node_id, false).await.is_err() {
        let db_arc = std::sync::Arc::clone(&db_state.conn);
        let settings_arc: SharedSettings = (*settings.inner()).clone();
        monitor::start_monitor(
            app,
            map.inner().clone(),
            db_arc,
            settings_arc,
            node_id,
            name,
            auth,
            metrics,
            customs,
            false,
        ).await;
    }
    Ok(())
}

#[tauri::command]
async fn monitor_pause(
    db_state: tauri::State<'_, DbState>,
    map: tauri::State<'_, MonitorMap>,
    node_id: i32,
) -> Result<(), String> {
    {
        let guard = db_state.conn.lock().map_err(|_| "lock")?;
        let conn = guard.as_ref().ok_or("db not ready")?;
        if let Some((metrics, customs, _)) = load_monitor_config(conn, node_id) {
            upsert_monitor_config(conn, node_id, &metrics, &customs, true)?;
        }
    }
    save_vault_internal(&db_state)?;
    let _ = monitor::set_paused(map.inner().clone(), node_id, true).await;
    Ok(())
}

#[tauri::command]
async fn monitor_pause_all(
    db_state: tauri::State<'_, DbState>,
    map: tauri::State<'_, MonitorMap>,
) -> Result<(), String> {
    {
        let guard = db_state.conn.lock().map_err(|_| "lock")?;
        let conn = guard.as_ref().ok_or("db not ready")?;
        conn.execute("UPDATE monitor_configs SET paused = 1", [])
            .map_err(|e| e.to_string())?;
    }
    save_vault_internal(&db_state)?;
    monitor::pause_all(map.inner().clone()).await;
    Ok(())
}

#[tauri::command]
async fn monitor_resume_all(
    app: tauri::AppHandle,
    db_state: tauri::State<'_, DbState>,
    map: tauri::State<'_, MonitorMap>,
    settings: tauri::State<'_, SharedSettings>,
) -> Result<(), String> {
    // Persist all to unpaused first.
    let node_ids: Vec<i32> = {
        let guard = db_state.conn.lock().map_err(|_| "lock")?;
        let conn = guard.as_ref().ok_or("db not ready")?;
        conn.execute("UPDATE monitor_configs SET paused = 0", [])
            .map_err(|e| e.to_string())?;
        let mut stmt = conn.prepare("SELECT node_id FROM monitor_configs").map_err(|e| e.to_string())?;
        let rows = stmt.query_map([], |r| r.get::<_, i32>(0)).map_err(|e| e.to_string())?;
        rows.filter_map(|r| r.ok()).collect()
    };
    save_vault_internal(&db_state)?;

    // Start (or hot-resume) each one.
    for node_id in node_ids {
        if monitor::set_paused(map.inner().clone(), node_id, false).await.is_err() {
            let (auth, metrics, customs, name) = {
                let guard = db_state.conn.lock().map_err(|_| "lock")?;
                let conn = guard.as_ref().ok_or("db not ready")?;
                let (metrics, customs, _) = load_monitor_config(conn, node_id)
                    .unwrap_or((default_metrics(), vec![], false));
                let name = fetch_node_name(conn, node_id);
                (resolve_node_auth_for_monitor(conn, node_id)?, metrics, customs, name)
            };
            let db_arc = std::sync::Arc::clone(&db_state.conn);
            let settings_arc: SharedSettings = (*settings.inner()).clone();
            monitor::start_monitor(
                app.clone(),
                map.inner().clone(),
                db_arc,
                settings_arc,
                node_id,
                name,
                auth,
                metrics,
                customs,
                false,
            ).await;
        }
    }
    Ok(())
}

fn main() {
    // Tauri 2 on Linux embeds webkit2gtk-4.1. Two failure modes have to be
    // headed off before the WebKit child process spawns, because once it's
    // up the env it inherited is the only one it sees:
    //
    //   1. The DMA-BUF + EGL renderer that WebKit defaults to crashes with
    //      "EGL_BAD_PARAMETER … Aborting" / SIGABRT on Mesa ≥ 24 across
    //      most GPUs. Flipping it off forces the older GLES path.
    //      (WebKit #258834, tauri-apps/tauri#9304)
    //
    //   2. On Wayland sessions with Mesa ≥ 24 (Fedora 40+, KDE Plasma 6,
    //      GNOME 46+) the bundled webkit2gtk-4.1's eglGetDisplay() against
    //      a wl_display still aborts even with the DMA-BUF renderer off,
    //      because the bundled libwayland-egl ABI predates the host Mesa.
    //      Routing GTK through XWayland avoids the mismatched Wayland-EGL
    //      handshake entirely and keeps the app usable on every desktop.
    //      The trade-off is XWayland's slightly fuzzier HiDPI scaling,
    //      which is acceptable in exchange for "the app actually opens".
    //
    // Every override is gated on `var_os(...).is_none()` so power users
    // (or distros that ship a patched WebKit) can opt back in by exporting
    // the variable themselves before launching.
    #[cfg(target_os = "linux")]
    {
        // The single most effective override for the EGL_BAD_PARAMETER abort
        // seen on Fedora 40+ / Mesa 24+: tell WebKit not to attempt hardware
        // accelerated rendering AT ALL. The flag short-circuits the WebKit
        // codepath that calls eglGetDisplay(), which is the exact line that
        // SIGABRTs when the bundled libwayland-egl can't negotiate with the
        // host Mesa. Set before any of the more granular flags so it wins
        // on builds of WebKit that ignore the renderer-specific switches.
        if std::env::var_os("WEBKIT_DISABLE_HARDWARE_ACCELERATION").is_none() {
            std::env::set_var("WEBKIT_DISABLE_HARDWARE_ACCELERATION", "1");
        }
        // bwrap sandbox strips inherited env from WebKitWebProcess (Fedora's
        // SELinux-confined bwrap is the canonical offender) — the flags we
        // set in this block need to reach the render-process child or none
        // of the rendering overrides will fire.
        //
        // WebKit 2.42 renamed `WEBKIT_FORCE_SANDBOX=0` to
        // `WEBKIT_DISABLE_SANDBOX_THIS_IS_DANGEROUS=1` and the old name now
        // emits a warning ("no longer allows disabling the sandbox") instead
        // of actually disabling anything. We set BOTH so the override works
        // across the full WebKit version range we'll meet in the wild —
        // older WebKit picks up the legacy name, 2.42+ picks up the loud
        // one. Losing the sandbox boundary for the webview is acceptable
        // for a desktop app that already runs with the user's full
        // filesystem access; the security boundary that matters
        // (Tauri capabilities + strict CSP) is unaffected.
        if std::env::var_os("WEBKIT_DISABLE_SANDBOX_THIS_IS_DANGEROUS").is_none() {
            std::env::set_var("WEBKIT_DISABLE_SANDBOX_THIS_IS_DANGEROUS", "1");
        }
        if std::env::var_os("WEBKIT_FORCE_SANDBOX").is_none() {
            std::env::set_var("WEBKIT_FORCE_SANDBOX", "0");
        }
        // Older-renderer + DMA-BUF flags stay as belt-and-braces — they
        // cost nothing on builds where WEBKIT_DISABLE_HARDWARE_ACCELERATION
        // already wins, and they cover the corner cases where a downstream
        // patched WebKit honours one but not the other.
        if std::env::var_os("WEBKIT_DISABLE_DMABUF_RENDERER").is_none() {
            std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
        }
        if std::env::var_os("WEBKIT_DISABLE_COMPOSITING_MODE").is_none() {
            std::env::set_var("WEBKIT_DISABLE_COMPOSITING_MODE", "1");
        }
        if std::env::var_os("GDK_BACKEND").is_none() {
            std::env::set_var("GDK_BACKEND", "x11");
        }
        // Mesa software rasteriser flag kept as the final fallback in case
        // WebKit's "no hardware" path still calls into Mesa somewhere.
        if std::env::var_os("LIBGL_ALWAYS_SOFTWARE").is_none() {
            std::env::set_var("LIBGL_ALWAYS_SOFTWARE", "1");
        }
    }

    tauri::Builder::default()
        // Save/restore the main window's last size + position to a JSON file
        // in app_data_dir. Keeps the user's preferred geometry across launches
        // without us having to wire setSize/setPosition by hand. Plain plugin —
        // it intercepts window events; no JS-facing commands to lock down.
        .plugin(tauri_plugin_window_state::Builder::default().build())
        .manage(DbState { conn: std::sync::Arc::new(StdMutex::new(None)), master_key: StdMutex::new(None), salt: StdMutex::new(None), db_path: StdMutex::new(None), active_profile: StdMutex::new(None) })
        .manage(SshState::new())
        // Monitoring state is its own root-level Tauri-managed value, separate
        // from SshState — monitors and interactive sessions own different SSH
        // handles per node and don't share lifecycle.
        .manage::<MonitorMap>(std::sync::Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())))
        // Mirror tasks live under their own root-managed map — keyed by mirror
        // id, populated by `start_mirror`, drained by the spawned worker on
        // exit. Session teardown calls `mirror::stop_all_for_session` to make
        // sure no orphan watcher is left running after the SSH handle dies.
        .manage::<MirrorMap>(std::sync::Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())))
        // Global monitor settings live in one shared Arc<Mutex>. Pollers
        // read it at the start of every cycle so interval/threshold changes
        // are hot-applied without restarting any monitor.
        .manage::<SharedSettings>(std::sync::Arc::new(tokio::sync::Mutex::new(monitor::MonitorSettings::default())))
        // CloudState needs the AppHandle to find app_data_dir on construction
        // (to load any persisted bearer token). Setup is the earliest hook we
        // get an AppHandle, so initialise it there and `manage` it for commands.
        .setup(|app| {
            let cloud_state = cloud::CloudState::new(&app.handle());
            app.manage(cloud_state);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            check_db_exists, setup_master_db, persist_vault,
            list_profiles, select_profile, create_profile, delete_profile, close_profile,
            export_profile, import_profile_pick, import_profile_save,
            cloud::cloud_status, cloud::cloud_signup, cloud::cloud_consume_verify_link,
            cloud::cloud_set_password, cloud::cloud_login, cloud::cloud_logout,
            cloud::cloud_list_remote, cloud::cloud_upload_profile,
            cloud::cloud_force_upload_profile, cloud::cloud_download_profile,
            cloud::cloud_delete_remote_profile,
            cloud::cloud_sync_overview, cloud::cloud_sync_all,
            add_server, edit_server, delete_server, add_mirror_to_server, get_servers, get_ssh_keys, 
            get_credentials, generate_ssh_key,
            add_folder, rename_folder, delete_folder, get_folders,
            add_command, edit_command, delete_command, get_commands,
            add_note, edit_note, delete_note, get_notes,
            mirror_dry_run, start_mirror, stop_mirror, list_mirrors, pick_local_directory,
            add_credential, edit_credential, delete_credential,
            add_ssh_key, edit_ssh_key, delete_ssh_key,
            initiate_connection, verify_fingerprint_response, disconnect_session,
            start_tunnel, stop_tunnel, list_tunnels,
            open_terminal, write_terminal_data, resize_terminal, close_terminal,
            select_local_folder, local_list_dir,
            local_home_dir, local_desktop_dir, local_create_dir, local_remove, local_rename,
            sftp_list_dir, sftp_create_dir, sftp_remove_file, sftp_remove_dir,
            sftp_rename, sftp_set_permissions, sftp_set_owner,
            sftp_download_file, sftp_upload_file, sftp_open_remote_file,
            local_open_file, local_open_in_explorer, sftp_prepare_drag,
            monitor_list, monitor_add, monitor_remove, monitor_set_metrics, monitor_set_custom_metrics,
            monitor_resume, monitor_pause, monitor_resume_all, monitor_pause_all,
            monitor_get_settings, monitor_set_settings,
            about::app_info, about::check_for_updates, about::open_external_url
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}