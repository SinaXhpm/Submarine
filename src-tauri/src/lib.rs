// windows_subsystem is bin-only; the matching attribute lives in main.rs.

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
mod docker;
mod hlc;
mod identity;
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
    /// This profile's Hybrid Logical Clock, shared into the SQLite `hlc_now()`
    /// custom function so every row mutation auto-stamps `updated_at`. `Some`
    /// while a profile is open; `None` on the picker. Held behind an Arc so the
    /// same clock instance backs both the SQL function and any Rust-side sync
    /// code (the merge engine's `observe`).
    pub hlc: StdMutex<Option<std::sync::Arc<hlc::Hlc>>>,
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
        save_vault_blocking(conn, key, salt, path)
    } else {
        Err("[STATE] MISSING_REQUIRED_RESOURCES_FOR_SAVE".into())
    }
}

/// Pure-sync vault serialise + encrypt + atomic write. Pulled out of
/// `save_vault_internal` so the async wrapper below can hand it to
/// `spawn_blocking` with owned snapshots — keeps the SQLite serialise,
/// zstd compression, AES-GCM encrypt, and fsync off the tokio worker
/// pool during hot paths like the post-connect `persist_vault` call.
fn save_vault_blocking(
    conn: &Connection,
    key: &Zeroizing<[u8; 32]>,
    salt: &[u8; SALT_LEN],
    path: &std::path::Path,
) -> Result<(), String> {
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
    Ok(())
}

// ---------------------------------------------------------------------------
// Per-entity sync foundation (uuid + HLC + tombstone)
// ---------------------------------------------------------------------------

/// The vault tables that participate in per-entity Last-Write-Wins sync. Each
/// carries the `uuid` (portable identity) / `updated_at` (HLC) / `deleted`
/// (tombstone) columns. `known_hosts`, `cmd_history`, `monitor_settings`, and
/// `schema_meta` are intentionally device-local and NOT synced.
const SYNCED_TABLES: &[&str] = &[
    "folders", "ssh_keys", "credentials", "servers", "commands", "notes", "monitor_configs",
];

/// Opaque 128-bit hex id for a synced row. Not RFC-4122 formatted — we only
/// need global uniqueness, and this matches the app's existing random-id idiom
/// (see `app_temp_root`).
fn new_entity_uuid() -> String {
    let mut bytes = [0u8; 16];
    rand::thread_rng().fill(&mut bytes);
    hex::encode(bytes)
}

/// This profile's Data Encryption Key — the 256-bit key that every per-entity
/// sync blob is encrypted under. Stored in `sync_meta` inside the vault (itself
/// encrypted at rest with the vault password), so a solo profile needs no
/// passphrase to sync. When the profile is shared, THIS key is what gets sealed
/// to each member's public key — decoupling "who can read the synced data" from
/// "who knows the vault password". Generated once and reused; rotating it (on a
/// member revoke) is a deliberate, separate action. Returns `(dek, created)`.
fn get_or_create_dek(conn: &Connection) -> Result<([u8; 32], bool), String> {
    let existing: Option<String> = conn
        .query_row("SELECT value FROM sync_meta WHERE key='dek'", [], |r| r.get(0))
        .ok();
    if let Some(hex_s) = existing {
        let raw = hex::decode(&hex_s).map_err(|e| format!("[SHARE] DEK_HEX: {e}"))?;
        if raw.len() == 32 {
            let mut d = [0u8; 32];
            d.copy_from_slice(&raw);
            return Ok((d, false));
        }
        // Malformed row (shouldn't happen) — fall through and mint a fresh one.
    }
    let mut d = [0u8; 32];
    rand::thread_rng().fill(&mut d);
    conn.execute(
        "INSERT INTO sync_meta(key,value) VALUES('dek',?1)
         ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        [hex::encode(d)],
    )
    .map_err(|e| format!("[SHARE] DEK_STORE: {e}"))?;
    Ok((d, true))
}

/// Load — or generate once — this device's stable sync node id. Stored in a
/// device-local sidecar next to the cloud token, deliberately OUTSIDE the vault
/// so it never travels when a vault is copied/restored to another device (two
/// devices sharing a node id would make HLC tie-breaks collide and corrupt the
/// causal order).
fn sync_device_node_id(app: &tauri::AppHandle) -> String {
    use tauri::Manager as _;
    let fresh = || {
        let mut b = [0u8; 8];
        rand::thread_rng().fill(&mut b);
        hex::encode(b)
    };
    let Ok(dir) = app.path().app_data_dir() else { return fresh() };
    let path = dir.join("sync_device.json");
    if let Ok(bytes) = std::fs::read(&path) {
        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&bytes) {
            if let Some(id) = v.get("node_id").and_then(|x| x.as_str()) {
                if !id.is_empty() {
                    return id.to_string();
                }
            }
        }
    }
    let id = fresh();
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(&path, serde_json::json!({ "node_id": id }).to_string());
    id
}

/// One-time backfill: give a uuid + HLC stamp to every existing synced row that
/// predates the sync columns (i.e. `uuid IS NULL`). Idempotent. Returns whether
/// anything changed, so the caller can force a resave to persist the ids.
fn backfill_sync_columns(conn: &Connection, hlc: &hlc::Hlc) -> Result<bool, String> {
    let mut changed = false;
    for table in SYNCED_TABLES {
        let pk = if *table == "monitor_configs" { "node_id" } else { "id" };
        // Snapshot the PKs first — can't hold the SELECT statement open across
        // the per-row UPDATE on the same connection.
        let ids: Vec<i64> = {
            let mut stmt = conn
                .prepare(&format!("SELECT {pk} FROM {table} WHERE uuid IS NULL"))
                .map_err(|e| format!("[SYNC] BACKFILL_SELECT {table}: {e}"))?;
            let rows = stmt
                .query_map([], |r| r.get::<_, i64>(0))
                .map_err(|e| format!("[SYNC] BACKFILL_QUERY {table}: {e}"))?;
            rows.filter_map(|r| r.ok()).collect()
        };
        for id in ids {
            conn.execute(
                &format!("UPDATE {table} SET uuid=?1, updated_at=?2 WHERE {pk}=?3"),
                rusqlite::params![new_entity_uuid(), hlc.tick(), id],
            )
            .map_err(|e| format!("[SYNC] BACKFILL_UPDATE {table}: {e}"))?;
            changed = true;
        }
    }
    Ok(changed)
}

/// Register the SQLite custom functions the auto-stamp triggers call. Must run
/// on every freshly-opened connection — custom functions are per-connection
/// state, not stored in the serialized DB, whereas the triggers (which call
/// them) ARE serialized and survive across opens. `hlc_now()` returns this
/// profile's next monotonic stamp; `sync_new_uuid()` mints a fresh row id.
fn register_sync_functions(conn: &Connection, hlc: &std::sync::Arc<hlc::Hlc>) -> Result<(), String> {
    use rusqlite::functions::FunctionFlags;
    let h = hlc.clone();
    conn.create_scalar_function("hlc_now", 0, FunctionFlags::empty(), move |_| Ok(h.tick()))
        .map_err(|e| format!("[SYNC] FN_HLC: {e}"))?;
    conn.create_scalar_function("sync_new_uuid", 0, FunctionFlags::empty(), move |_| Ok(new_entity_uuid()))
        .map_err(|e| format!("[SYNC] FN_UUID: {e}"))?;
    Ok(())
}

/// Create the tombstone side-table + the per-table auto-stamp triggers
/// (idempotent). This is the whole per-entity instrumentation, centralised:
///   - AFTER INSERT  → assign a uuid + HLC stamp (skipped when the row already
///     carries a uuid — i.e. the merge engine applying a remote insert).
///   - AFTER UPDATE  → bump the HLC, UNLESS the caller set `updated_at` itself
///     (again: the merge engine applying a remote change keeps the remote stamp).
///   - AFTER DELETE  → record a tombstone so the deletion propagates. Fires for
///     FK-cascade deletes too (monitor_configs), which the map flagged as the
///     one path that bypassed Rust. Rows are still HARD-deleted, so every
///     existing `SELECT` is untouched — the tombstone lives only in the side
///     table the sync layer reads.
/// `monitor_configs` gets a column-aware UPDATE guard so the per-open `paused`
/// reset (device-local housekeeping) never churns the stamp.
fn create_sync_triggers(conn: &Connection) -> Result<(), String> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS sync_tombstones (uuid TEXT PRIMARY KEY, entity_type TEXT NOT NULL, updated_at TEXT NOT NULL)",
        [],
    )
    .map_err(|e| format!("[SYNC] TOMBSTONE_TABLE: {e}"))?;
    // Device-local operational flags. `merge` is set to 1 while the sync engine
    // applies remote records, which the auto-stamp triggers check so they don't
    // overwrite the incoming HLC stamps with fresh local ones.
    conn.execute(
        "CREATE TABLE IF NOT EXISTS sync_flags (key TEXT PRIMARY KEY, val INTEGER NOT NULL)",
        [],
    )
    .map_err(|e| format!("[SYNC] FLAGS_TABLE: {e}"))?;
    // Ensure sync_meta exists before the triggers (which read editor_label from
    // it) are created — the convergence path also creates it, but tests call
    // create_sync_triggers directly.
    conn.execute(
        "CREATE TABLE IF NOT EXISTS sync_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)",
        [],
    )
    .map_err(|e| format!("[SYNC] SYNC_META_TABLE: {e}"))?;
    // Guard shared by the stamp/tombstone triggers: suppress them while merging.
    const GUARD: &str = "COALESCE((SELECT val FROM sync_flags WHERE key='merge'),0)=0";
    // The editor label stamped onto edited_by at mutation time (email when known,
    // else NULL). Kept as a subquery so it always reflects the latest value.
    const EDITOR: &str = "(SELECT value FROM sync_meta WHERE key='editor_label')";
    for t in SYNCED_TABLES {
        let au_extra = if *t == "monitor_configs" {
            " AND (NEW.enabled_metrics IS NOT OLD.enabled_metrics OR NEW.custom_metrics IS NOT OLD.custom_metrics OR NEW.deleted IS NOT OLD.deleted)"
        } else {
            ""
        };
        // Drop-then-create so the definition is always current across app
        // versions (CREATE IF NOT EXISTS would keep a stale earlier trigger).
        let ddl = format!(
            "DROP TRIGGER IF EXISTS {t}_sync_ai;
             DROP TRIGGER IF EXISTS {t}_sync_au;
             DROP TRIGGER IF EXISTS {t}_sync_ad;
             CREATE TRIGGER {t}_sync_ai AFTER INSERT ON {t} FOR EACH ROW WHEN NEW.uuid IS NULL AND {GUARD}
               BEGIN UPDATE {t} SET uuid = sync_new_uuid(), updated_at = hlc_now(), edited_by = {EDITOR} WHERE rowid = NEW.rowid; END;
             CREATE TRIGGER {t}_sync_au AFTER UPDATE ON {t} FOR EACH ROW
               WHEN NEW.updated_at IS OLD.updated_at{au_extra} AND {GUARD}
               BEGIN UPDATE {t} SET updated_at = hlc_now(), edited_by = {EDITOR} WHERE rowid = NEW.rowid; END;
             CREATE TRIGGER {t}_sync_ad AFTER DELETE ON {t} FOR EACH ROW WHEN OLD.uuid IS NOT NULL AND {GUARD}
               BEGIN INSERT INTO sync_tombstones(uuid, entity_type, updated_at) VALUES (OLD.uuid, '{t}', hlc_now())
                     ON CONFLICT(uuid) DO UPDATE SET updated_at = excluded.updated_at, entity_type = excluded.entity_type; END;"
        );
        conn.execute_batch(&ddl)
            .map_err(|e| format!("[SYNC] TRIGGER {t}: {e}"))?;
        // Unique index on uuid enables ON CONFLICT(uuid) upserts in the merge
        // engine. NOT partial — SQLite treats NULLs as distinct, so pre-backfill
        // NULL uuids coexist fine, and a plain index (unlike a partial one) is a
        // valid ON CONFLICT target.
        conn.execute(
            &format!("CREATE UNIQUE INDEX IF NOT EXISTS ux_{t}_uuid ON {t}(uuid)"),
            [],
        )
        .map_err(|e| format!("[SYNC] UUID_INDEX {t}: {e}"))?;
    }
    Ok(())
}

#[cfg(test)]
mod sync_trigger_tests {
    use super::*;
    use rusqlite::Connection;

    fn setup() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        // Minimal stand-ins for the synced tables — only the columns the
        // triggers touch. monitor_configs keeps enabled_metrics/custom_metrics
        // so the column-aware UPDATE guard can be exercised.
        conn.execute_batch(
            "CREATE TABLE folders(id INTEGER PRIMARY KEY, name TEXT, uuid TEXT, updated_at TEXT, deleted INTEGER NOT NULL DEFAULT 0, edited_by TEXT);
             CREATE TABLE ssh_keys(id INTEGER PRIMARY KEY, name TEXT, uuid TEXT, updated_at TEXT, deleted INTEGER NOT NULL DEFAULT 0, edited_by TEXT);
             CREATE TABLE credentials(id INTEGER PRIMARY KEY, name TEXT, uuid TEXT, updated_at TEXT, deleted INTEGER NOT NULL DEFAULT 0, edited_by TEXT);
             CREATE TABLE servers(id INTEGER PRIMARY KEY, name TEXT, uuid TEXT, updated_at TEXT, deleted INTEGER NOT NULL DEFAULT 0, edited_by TEXT);
             CREATE TABLE commands(id INTEGER PRIMARY KEY, title TEXT, uuid TEXT, updated_at TEXT, deleted INTEGER NOT NULL DEFAULT 0, edited_by TEXT);
             CREATE TABLE notes(id INTEGER PRIMARY KEY, title TEXT, uuid TEXT, updated_at TEXT, deleted INTEGER NOT NULL DEFAULT 0, edited_by TEXT);
             CREATE TABLE monitor_configs(node_id INTEGER PRIMARY KEY, enabled_metrics TEXT, custom_metrics TEXT, paused INTEGER NOT NULL DEFAULT 1, uuid TEXT, updated_at TEXT, deleted INTEGER NOT NULL DEFAULT 0, edited_by TEXT);",
        ).unwrap();
        let hlc = std::sync::Arc::new(hlc::Hlc::new("testnode".into(), 0));
        register_sync_functions(&conn, &hlc).unwrap();
        create_sync_triggers(&conn).unwrap();
        conn
    }
    fn ua(conn: &Connection, name: &str) -> String {
        conn.query_row("SELECT updated_at FROM servers WHERE name=?1", [name], |r| r.get(0)).unwrap()
    }

    #[test]
    fn insert_auto_stamps_uuid_and_updated_at() {
        let conn = setup();
        conn.execute("INSERT INTO servers(name) VALUES('a')", []).unwrap();
        let (uuid, at): (Option<String>, Option<String>) = conn
            .query_row("SELECT uuid, updated_at FROM servers WHERE name='a'", [], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap();
        assert!(uuid.as_deref().map(|s| !s.is_empty()).unwrap_or(false), "uuid must be stamped");
        assert!(at.as_deref().map(|s| !s.is_empty()).unwrap_or(false), "updated_at must be stamped");
    }

    #[test]
    fn edit_bumps_stamp_but_merge_apply_is_preserved() {
        let conn = setup();
        conn.execute("INSERT INTO servers(name) VALUES('a')", []).unwrap();
        let t1 = ua(&conn, "a");
        conn.execute("UPDATE servers SET name='b' WHERE name='a'", []).unwrap();
        let t2 = ua(&conn, "b");
        assert!(t2 > t1, "a normal edit must bump the stamp");
        // The merge engine applies a remote change by setting updated_at itself;
        // the trigger must NOT overwrite it with a fresh local stamp.
        conn.execute(
            "UPDATE servers SET name='c', updated_at='999999999999999:00000:remote' WHERE name='b'",
            [],
        ).unwrap();
        assert_eq!(ua(&conn, "c"), "999999999999999:00000:remote", "merge-applied stamp must survive");
    }

    #[test]
    fn delete_leaves_a_tombstone_and_hard_deletes() {
        let conn = setup();
        conn.execute("INSERT INTO servers(name) VALUES('a')", []).unwrap();
        let uuid: String = conn.query_row("SELECT uuid FROM servers WHERE name='a'", [], |r| r.get(0)).unwrap();
        conn.execute("DELETE FROM servers WHERE name='a'", []).unwrap();
        let tombs: i64 = conn
            .query_row("SELECT COUNT(*) FROM sync_tombstones WHERE uuid=?1 AND entity_type='servers'", [&uuid], |r| r.get(0))
            .unwrap();
        assert_eq!(tombs, 1, "delete must record a tombstone");
        let rows: i64 = conn.query_row("SELECT COUNT(*) FROM servers WHERE name='a'", [], |r| r.get(0)).unwrap();
        assert_eq!(rows, 0, "row must be hard-deleted so existing SELECTs are unaffected");
    }

    #[test]
    fn monitor_paused_reset_does_not_churn_but_real_change_stamps() {
        let conn = setup();
        conn.execute("INSERT INTO monitor_configs(node_id, enabled_metrics, custom_metrics) VALUES(1,'[]','[]')", []).unwrap();
        let t1: String = conn.query_row("SELECT updated_at FROM monitor_configs WHERE node_id=1", [], |r| r.get(0)).unwrap();
        conn.execute("UPDATE monitor_configs SET paused=1", []).unwrap();
        let t2: String = conn.query_row("SELECT updated_at FROM monitor_configs WHERE node_id=1", [], |r| r.get(0)).unwrap();
        assert_eq!(t1, t2, "device-local paused reset must not churn the sync stamp");
        conn.execute("UPDATE monitor_configs SET enabled_metrics='[\"cpu\"]' WHERE node_id=1", []).unwrap();
        let t3: String = conn.query_row("SELECT updated_at FROM monitor_configs WHERE node_id=1", [], |r| r.get(0)).unwrap();
        assert!(t3 > t2, "a real config change must stamp");
    }
}

// ---------------------------------------------------------------------------
// Sync engine: per-entity serialize (FK int -> uuid) + LWW merge apply
// ---------------------------------------------------------------------------

/// One record as it travels to/from the server: metadata in the clear (so the
/// server can LWW-order without decrypting) + an encrypted payload blob. A
/// tombstone is `deleted: true` with no blob.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
pub struct SyncRecord {
    pub uuid: String,
    pub entity_type: String,
    pub updated_at: String,
    pub deleted: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blob: Option<String>,
}

struct Fk {
    key: &'static str,       // payload key holding the referenced row's uuid
    col: &'static str,       // local FK int column
    ref_table: &'static str, // table the FK points at
}
struct EntitySpec {
    table: &'static str,
    cols: &'static [&'static str], // content columns synced verbatim
    fks: &'static [Fk],
}

/// Applied in this order so most FK referents already exist on merge; the two
/// self-referential FKs (folders.parent_id, servers.jump_host_id) are resolved
/// in a deferred fixup pass afterwards.
// `edited_by` rides along as a normal column on every spec: the auto-stamp
// triggers set it to the current editor label at mutation time, so it flows
// through collect + apply for free and records WHO last changed each entity
// (zero-knowledge — it's inside the encrypted blob, never seen by the server).
const ENTITIES: &[EntitySpec] = &[
    EntitySpec { table: "ssh_keys", cols: &["name", "public_key", "private_key", "passphrase", "edited_by"], fks: &[] },
    EntitySpec { table: "folders", cols: &["name", "color", "edited_by"], fks: &[Fk { key: "parent_uuid", col: "parent_id", ref_table: "folders" }] },
    EntitySpec { table: "credentials", cols: &["name", "auth_type", "username", "password", "edited_by"], fks: &[Fk { key: "key_uuid", col: "key_id", ref_table: "ssh_keys" }] },
    EntitySpec {
        table: "servers",
        cols: &["name", "host", "port", "username", "password", "proxy_type", "proxy_host", "proxy_port", "tunnels", "auth_type", "autostart", "mirrors", "color", "notes", "run_on_connect", "edited_by"],
        fks: &[
            Fk { key: "credential_uuid", col: "credential_id", ref_table: "credentials" },
            Fk { key: "folder_uuid", col: "folder_id", ref_table: "folders" },
            Fk { key: "key_uuid", col: "key_id", ref_table: "ssh_keys" },
            Fk { key: "jump_uuid", col: "jump_host_id", ref_table: "servers" },
        ],
    },
    EntitySpec { table: "commands", cols: &["title", "content", "edited_by"], fks: &[] },
    EntitySpec { table: "notes", cols: &["title", "body", "edited_by"], fks: &[] },
];

/// AES-256-GCM a per-entity payload with the profile key; frame is `nonce || ct`
/// hex-encoded. Same key that protects the whole vault, so the server stays
/// zero-knowledge.
fn encrypt_entity(plaintext: &[u8], key: &[u8; 32]) -> Result<String, String> {
    let (ct, nonce) = encrypt_with_key(plaintext, key)?;
    let mut out = Vec::with_capacity(NONCE_LEN + ct.len());
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ct);
    Ok(hex::encode(out))
}
fn decrypt_entity(blob_hex: &str, key: &[u8; 32]) -> Result<Vec<u8>, String> {
    let raw = hex::decode(blob_hex).map_err(|e| format!("[SYNC] BLOB_HEX: {e}"))?;
    if raw.len() < NONCE_LEN + 16 {
        return Err("[SYNC] BLOB_TOO_SHORT".into());
    }
    decrypt_with_key(&raw[NONCE_LEN..], &raw[..NONCE_LEN], key)
}

fn json_to_sql(v: Option<&serde_json::Value>) -> Box<dyn rusqlite::types::ToSql> {
    use serde_json::Value;
    match v {
        None | Some(Value::Null) => Box::new(rusqlite::types::Null),
        Some(Value::String(s)) => Box::new(s.clone()),
        Some(Value::Bool(b)) => Box::new(*b as i64),
        Some(Value::Number(n)) => {
            if let Some(i) = n.as_i64() {
                Box::new(i)
            } else {
                Box::new(n.as_f64().unwrap_or(0.0))
            }
        }
        // arrays/objects (shouldn't occur — tunnels/mirrors are TEXT) → JSON text
        Some(other) => Box::new(other.to_string()),
    }
}

/// Serialize every synced entity + tombstone changed since `since` (empty = all)
/// into encrypted records. FK ints are translated to the referenced row's uuid
/// via SQL subqueries so the payload is portable across devices.
fn collect_local_records(conn: &Connection, key: &[u8; 32], since: &str) -> Result<Vec<SyncRecord>, String> {
    let mut out = Vec::new();
    for spec in ENTITIES {
        let mut pairs: Vec<String> = spec.cols.iter().map(|c| format!("'{c}', t.{c}")).collect();
        for fk in spec.fks {
            pairs.push(format!("'{}', (SELECT uuid FROM {} WHERE id = t.{})", fk.key, fk.ref_table, fk.col));
        }
        let sql = format!(
            "SELECT t.uuid, t.updated_at, json_object({}) FROM {} t WHERE t.uuid IS NOT NULL AND t.updated_at > ?1",
            pairs.join(", "),
            spec.table
        );
        let mut stmt = conn.prepare(&sql).map_err(|e| format!("[SYNC] SER_PREP {}: {e}", spec.table))?;
        let rows = stmt
            .query_map([since], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)))
            .map_err(|e| format!("[SYNC] SER_QUERY {}: {e}", spec.table))?;
        for row in rows {
            let (uuid, updated_at, payload) = row.map_err(|e| format!("[SYNC] SER_ROW {}: {e}", spec.table))?;
            out.push(SyncRecord {
                uuid,
                entity_type: spec.table.to_string(),
                updated_at,
                deleted: false,
                blob: Some(encrypt_entity(payload.as_bytes(), key)?),
            });
        }
    }
    let mut stmt = conn
        .prepare("SELECT uuid, entity_type, updated_at FROM sync_tombstones WHERE updated_at > ?1")
        .map_err(|e| format!("[SYNC] SER_TOMB_PREP: {e}"))?;
    let rows = stmt
        .query_map([since], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)))
        .map_err(|e| format!("[SYNC] SER_TOMB_QUERY: {e}"))?;
    for row in rows {
        let (uuid, entity_type, updated_at) = row.map_err(|e| format!("[SYNC] SER_TOMB_ROW: {e}"))?;
        out.push(SyncRecord { uuid, entity_type, updated_at, deleted: true, blob: None });
    }
    Ok(out)
}

/// Merge incoming records into the local vault, Last-Write-Wins per row. Runs
/// with the auto-stamp triggers suppressed so incoming HLC stamps are written
/// verbatim, and advances the local clock past everything seen.
fn apply_remote_records(conn: &Connection, key: &[u8; 32], records: &[SyncRecord], hlc: &hlc::Hlc) -> Result<(), String> {
    conn.execute("INSERT INTO sync_flags(key,val) VALUES('merge',1) ON CONFLICT(key) DO UPDATE SET val=1", [])
        .map_err(|e| format!("[SYNC] MERGE_ON: {e}"))?;
    let r = apply_remote_inner(conn, key, records, hlc);
    let _ = conn.execute("UPDATE sync_flags SET val=0 WHERE key='merge'", []);
    r
}

fn apply_remote_inner(conn: &Connection, key: &[u8; 32], records: &[SyncRecord], hlc: &hlc::Hlc) -> Result<(), String> {
    for rec in records {
        hlc.observe(hlc::Hlc::phys_of(&rec.updated_at));
    }
    // (table, uuid, fk_col, ref_uuid) — self-ref / not-yet-present FKs to fix up.
    let mut fk_fixups: Vec<(String, String, String, String)> = Vec::new();
    for spec in ENTITIES {
        for rec in records.iter().filter(|r| !r.deleted && r.entity_type == spec.table) {
            apply_entity(conn, key, spec, rec, &mut fk_fixups)?;
        }
    }
    for rec in records.iter().filter(|r| r.deleted) {
        apply_tombstone(conn, rec)?;
    }
    for (table, uuid, fk_col, ref_uuid) in fk_fixups {
        let id: Option<i64> = conn
            .query_row(&format!("SELECT id FROM {table} WHERE uuid=?1"), [&ref_uuid], |r| r.get(0))
            .ok();
        conn.execute(&format!("UPDATE {table} SET {fk_col}=?1 WHERE uuid=?2"), rusqlite::params![id, uuid])
            .map_err(|e| format!("[SYNC] FK_FIXUP {table}: {e}"))?;
    }
    Ok(())
}

fn apply_entity(conn: &Connection, key: &[u8; 32], spec: &EntitySpec, rec: &SyncRecord, fk_fixups: &mut Vec<(String, String, String, String)>) -> Result<(), String> {
    // LWW: keep local if it's newer-or-equal. A deleted row is hard-deleted, so
    // its tombstone carries its only surviving stamp — it has to count as the
    // local side of the comparison. Without it, a batch collected before a
    // delete (sync drops the lock for the network leg, so the user can delete
    // mid-flight) comes back still carrying the row as live, finds no live row
    // to compare against, and re-inserts it — secrets and all.
    let local_ua: Option<String> = conn
        .query_row(&format!("SELECT updated_at FROM {} WHERE uuid=?1", spec.table), [&rec.uuid], |r| r.get(0))
        .ok();
    let tomb_ua: Option<String> = conn
        .query_row(
            "SELECT updated_at FROM sync_tombstones WHERE uuid=?1 AND entity_type=?2",
            rusqlite::params![&rec.uuid, spec.table],
            |r| r.get(0),
        )
        .ok();
    if [local_ua.as_deref(), tomb_ua.as_deref()]
        .into_iter()
        .flatten()
        .any(|l| l >= rec.updated_at.as_str())
    {
        return Ok(());
    }
    let blob = rec.blob.as_deref().ok_or("[SYNC] ENTITY_NO_BLOB")?;
    let plain = decrypt_entity(blob, key)?;
    let payload: serde_json::Value = serde_json::from_slice(&plain).map_err(|e| format!("[SYNC] PAYLOAD_JSON: {e}"))?;
    let obj = payload.as_object().ok_or("[SYNC] PAYLOAD_NOT_OBJ")?;

    let mut columns: Vec<String> = vec!["uuid".into(), "updated_at".into(), "deleted".into()];
    let mut params: Vec<Box<dyn rusqlite::types::ToSql>> =
        vec![Box::new(rec.uuid.clone()), Box::new(rec.updated_at.clone()), Box::new(rec.deleted as i64)];
    for c in spec.cols {
        columns.push((*c).into());
        params.push(json_to_sql(obj.get(*c)));
    }
    for fk in spec.fks {
        let ref_uuid = obj.get(fk.key).and_then(|v| v.as_str());
        let id: Option<i64> = match ref_uuid {
            None => None,
            Some(ru) => {
                let found: Option<i64> = conn
                    .query_row(&format!("SELECT id FROM {} WHERE uuid=?1", fk.ref_table), [ru], |r| r.get(0))
                    .ok();
                if found.is_none() {
                    fk_fixups.push((spec.table.to_string(), rec.uuid.clone(), fk.col.to_string(), ru.to_string()));
                }
                found
            }
        };
        columns.push(fk.col.into());
        params.push(Box::new(id));
    }

    let placeholders = std::iter::repeat("?").take(columns.len()).collect::<Vec<_>>().join(",");
    let update_set: Vec<String> = columns.iter().filter(|c| c.as_str() != "uuid").map(|c| format!("{c}=excluded.{c}")).collect();
    let sql = format!(
        "INSERT INTO {} ({}) VALUES ({}) ON CONFLICT(uuid) DO UPDATE SET {}",
        spec.table,
        columns.join(","),
        placeholders,
        update_set.join(",")
    );
    let refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|b| b.as_ref()).collect();
    conn.execute(&sql, refs.as_slice()).map_err(|e| format!("[SYNC] UPSERT {}: {e}", spec.table))?;
    Ok(())
}

fn apply_tombstone(conn: &Connection, rec: &SyncRecord) -> Result<(), String> {
    // Only tombstones for known entity types are meaningful here.
    if !ENTITIES.iter().any(|s| s.table == rec.entity_type) {
        return Ok(());
    }
    conn.execute(
        "INSERT INTO sync_tombstones(uuid, entity_type, updated_at) VALUES (?1,?2,?3)
         ON CONFLICT(uuid) DO UPDATE SET updated_at=excluded.updated_at, entity_type=excluded.entity_type
         WHERE excluded.updated_at > sync_tombstones.updated_at",
        rusqlite::params![rec.uuid, rec.entity_type, rec.updated_at],
    )
    .map_err(|e| format!("[SYNC] TOMB_UPSERT: {e}"))?;
    let local_ua: Option<String> = conn
        .query_row(&format!("SELECT updated_at FROM {} WHERE uuid=?1", rec.entity_type), [&rec.uuid], |r| r.get(0))
        .ok();
    if let Some(l) = local_ua {
        if l.as_str() < rec.updated_at.as_str() {
            // Merge guard is on → the AFTER DELETE trigger won't create a
            // competing tombstone; the explicit upsert above stands.
            conn.execute(&format!("DELETE FROM {} WHERE uuid=?1", rec.entity_type), [&rec.uuid])
                .map_err(|e| format!("[SYNC] TOMB_DELETE: {e}"))?;
        }
    }
    Ok(())
}

#[derive(serde::Serialize)]
struct SyncReport {
    pushed: usize,
    pulled: usize,
}

// A personal profile's DEK, sealed under the profile's OWN password, rides the
// normal /sync stream as one reserved record. That's what lets a fresh device
// bootstrap a personal profile with nothing but the profile password: sign in,
// pull, derive the key from the password + the salt carried in this record, and
// unseal the DEK — no whole-vault blob download, no separate recovery secret,
// no server changes (the /sync table accepts any entity_type ≤24 chars). The
// server stores it as an opaque blob like every other record, so zero-knowledge
// still holds — cracking it costs the same Argon2id work as the vault itself.
// It never matches an ENTITIES spec, so apply naturally ignores it; only the
// restore path reads it. The updated_at is a fixed low sentinel so it sorts
// first and never churns the LWW upsert.
//
// Blob layout (hex): version(1) ‖ salt(SALT_LEN) ‖ nonce(NONCE_LEN) ‖
// AES-256-GCM(master_key, dek). `master_key` is Argon2id(password, salt) — the
// exact same derivation as the on-disk vault — so a device that knows the
// password reproduces it from the embedded salt and decrypts the DEK.
const ESCROW_ETYPE: &str = "dek_escrow";
const ESCROW_UUID: &str = "0000000000000000000000000000dead";
const ESCROW_UAT: &str = "000000000000000:00000:0";
// Version byte fronting the escrow blob. `2` = password-sealed (the format
// below). `1` was the retired identity-sealed sealed-box; no such records were
// ever published to any live account, so there is nothing to migrate — but the
// byte lets the reader reject an unknown/legacy shape cleanly instead of
// mis-parsing it.
const PW_ESCROW_VERSION: u8 = 2;

/// Build the reserved DEK-escrow record: the profile DEK sealed under the
/// profile's master key (Argon2id of its password). `salt` is the vault's own
/// KDF salt, embedded so a fresh device can re-derive `master_key` from just the
/// password.
fn build_pw_escrow_record(
    master_key: &[u8; 32],
    salt: &[u8; SALT_LEN],
    dek: &[u8; 32],
) -> Result<SyncRecord, String> {
    let (ct, nonce) = encrypt_with_key(dek, master_key)?;
    let mut blob = Vec::with_capacity(1 + SALT_LEN + NONCE_LEN + ct.len());
    blob.push(PW_ESCROW_VERSION);
    blob.extend_from_slice(salt);
    blob.extend_from_slice(&nonce);
    blob.extend_from_slice(&ct);
    Ok(SyncRecord {
        uuid: ESCROW_UUID.to_string(),
        entity_type: ESCROW_ETYPE.to_string(),
        updated_at: ESCROW_UAT.to_string(),
        deleted: false,
        blob: Some(hex::encode(blob)),
    })
}

/// Recover a profile's DEK from a password-sealed escrow blob. Runs Argon2id, so
/// callers hand it to the blocking pool. Returns `DECRYPT_FAILURE` (via
/// `decrypt_with_key`) when the password is wrong — the GCM tag won't verify.
fn open_pw_escrow(blob_hex: &str, password: &str) -> Result<[u8; 32], String> {
    let raw = hex::decode(blob_hex).map_err(|_| "[SYNC] ESCROW_BAD_HEX")?;
    let head = 1 + SALT_LEN + NONCE_LEN;
    if raw.len() < head + 16 {
        return Err("[SYNC] ESCROW_TOO_SHORT".into());
    }
    if raw[0] != PW_ESCROW_VERSION {
        return Err(format!("[SYNC] ESCROW_BAD_VERSION: {}", raw[0]));
    }
    let salt = &raw[1..1 + SALT_LEN];
    let nonce = &raw[1 + SALT_LEN..head];
    let ct = &raw[head..];
    let mk = Zeroizing::new(derive_key(password, salt)?);
    let dek_vec = Zeroizing::new(decrypt_with_key(ct, nonce, &mk)?);
    if dek_vec.len() != 32 {
        return Err("[SYNC] ESCROW_BAD_DEK_LEN".into());
    }
    let mut dek = [0u8; 32];
    dek.copy_from_slice(&dek_vec);
    Ok(dek)
}

/// One full per-entity sync of the OPEN profile: collect every local record,
/// exchange with the server (which LWW-merges + returns its view), merge the
/// server's records back, and persist. Full-set each call (the dataset is
/// small — a handful of servers/credentials); a `since` watermark is a later
/// optimisation, not needed for correctness.
#[tauri::command]
async fn sync_now(
    app: tauri::AppHandle,
    db_state: tauri::State<'_, DbState>,
    cloud: tauri::State<'_, std::sync::Arc<cloud::CloudState>>,
) -> Result<SyncReport, String> {
    let profile = db_state
        .active_profile
        .lock()
        .map_err(|_| "[STATE] LOCK_PROFILE")?
        .clone()
        .ok_or("[SYNC] NO_PROFILE_OPEN")?;

    // Snapshot the profile DEK + clock and serialise local records, then DROP
    // all locks before the network round-trip (never hold a std Mutex across
    // .await). Per-entity blobs are encrypted with the DEK — NOT the vault
    // master key — so the same records can be shared with other members, who
    // hold the DEK via a sealed grant without ever knowing this vault's
    // password. The DEK is created + persisted at profile-open, so it already
    // exists here; get_or_create is a defensive fallback only.
    let (records, dek, hlc, share, cloud_profile, master_key, salt) = {
        let conn_g = db_state.conn.lock().map_err(|_| "[STATE] LOCK_CONN")?;
        let conn = conn_g.as_ref().ok_or("[STATE] DB_NOT_OPEN")?;
        let hlc_g = db_state.hlc.lock().map_err(|_| "[STATE] LOCK_HLC")?;
        let hlc = hlc_g.as_ref().ok_or("[SYNC] NO_HLC")?.clone();
        // The session master key + salt (retained in DbState for the vault
        // re-save) let us seal the DEK under the profile's own password with no
        // extra KDF work — copied out here so no guard is held across the await.
        let master_key = db_state
            .master_key
            .lock()
            .map_err(|_| "[STATE] LOCK_KEY")?
            .as_ref()
            .map(|k| **k);
        let salt = *db_state.salt.lock().map_err(|_| "[STATE] LOCK_SALT")?;
        let (dek, _created) = get_or_create_dek(conn)?;
        let records = collect_local_records(conn, &dek, "")?;
        // If this profile is shared, sync_meta carries its share_id + my role;
        // that routes the exchange to /shares/sync instead of personal /sync.
        let share_id: Option<String> = conn
            .query_row("SELECT value FROM sync_meta WHERE key='share_id'", [], |r| r.get(0))
            .ok();
        let share_role: Option<String> = conn
            .query_row("SELECT value FROM sync_meta WHERE key='share_role'", [], |r| r.get(0))
            .ok();
        // Personal /sync is keyed by profile name. A restored profile may carry
        // a different LOCAL name than its cloud key, so it records the cloud
        // name in sync_meta; fall back to the local name for everything else.
        let cloud_profile: String = conn
            .query_row("SELECT value FROM sync_meta WHERE key='cloud_profile'", [], |r| r.get(0))
            .unwrap_or_else(|_| profile.clone());
        (records, dek, hlc, share_id.map(|s| (s, share_role.unwrap_or_default())), cloud_profile, master_key, salt)
    };

    // Shared profile → /shares/sync (role-gated). A viewer ('user') pushes
    // nothing — it can only pull, so sending records would just 403.
    let (remote, pushed) = if let Some((share_id, role)) = &share {
        let to_push: &[SyncRecord] = if role == "user" { &[] } else { &records };
        let remote = cloud::shared_sync_exchange(&app, &cloud, share_id, "", to_push).await?;
        (remote, to_push.len())
    } else {
        // Personal profile: publish (or refresh) the DEK escrow alongside the
        // data so any device that knows this profile's password can bring it
        // down. Sealed under the profile's OWN master key — always-on, no
        // sharing identity or separate recovery secret required.
        let mut to_push = records.clone();
        if let (Some(mk), Some(salt)) = (master_key, salt) {
            to_push.push(build_pw_escrow_record(&mk, &salt, &dek)?);
        }
        // Send the local display name so the server can label this partition —
        // essential once `cloud_profile` is an opaque UUID for new profiles. For
        // legacy `main` the name equals the partition, so it's a harmless echo.
        let remote =
            cloud::sync_exchange(&app, &cloud, &cloud_profile, "", &to_push, Some(profile.as_str())).await?;
        (remote, to_push.len())
    };
    let pulled = remote.len();

    {
        let conn_g = db_state.conn.lock().map_err(|_| "[STATE] LOCK_CONN")?;
        let conn = conn_g.as_ref().ok_or("[STATE] DB_NOT_OPEN")?;
        apply_remote_records(conn, &dek, &remote, &hlc)?;
    }
    // Persist the merged vault so the applied changes survive a restart.
    save_vault_async(&db_state).await?;
    Ok(SyncReport { pushed, pulled })
}

#[derive(serde::Serialize)]
struct RecentEdit {
    name: String,
    edited_by: String,
    updated_at: String,
}

#[derive(serde::Serialize)]
struct SyncDiff {
    in_sync: usize,
    needs_push: usize,
    needs_pull: usize,
    /// Names of local nodes whose local copy differs from the cloud (either an
    /// unpushed local edit, or a cloud copy at a different stamp). Capped.
    out_of_sync_nodes: Vec<String>,
}

#[derive(serde::Serialize)]
struct ProfileSyncStats {
    /// Total synced records held locally (live rows + tombstones).
    total_records: usize,
    /// On-disk size of this profile's encrypted vault, for heaviness awareness.
    vault_bytes: u64,
    /// Last few edited nodes with who touched them — read straight from the
    /// local vault, no network. Always present.
    recent_edits: Vec<RecentEdit>,
    /// Live comparison against the cloud. `None` when offline / not signed in /
    /// never synced — the panel then shows recent edits only.
    diff: Option<SyncDiff>,
}

/// Read-only sync + activity snapshot for the Profile panel. The recent-edits
/// list is local and instant; the cloud diff is a best-effort dry run (an empty
/// push that mutates nothing, then a local comparison) and is omitted on any
/// network/auth failure rather than erroring the whole call.
#[tauri::command]
async fn profile_sync_stats(
    app: tauri::AppHandle,
    db_state: tauri::State<'_, DbState>,
    cloud: tauri::State<'_, std::sync::Arc<cloud::CloudState>>,
) -> Result<ProfileSyncStats, String> {
    let profile = db_state
        .active_profile
        .lock()
        .map_err(|_| "[STATE] LOCK_PROFILE")?
        .clone()
        .ok_or("[SYNC] NO_PROFILE_OPEN")?;

    // Local snapshot under the lock: records, DEK, routing, recent edits, and a
    // uuid→name map for servers so the diff can name what's out of sync.
    let (local, dek, share, cloud_profile, recent_edits, server_names) = {
        let conn_g = db_state.conn.lock().map_err(|_| "[STATE] LOCK_CONN")?;
        let conn = conn_g.as_ref().ok_or("[STATE] DB_NOT_OPEN")?;
        let (dek, _created) = get_or_create_dek(conn)?;
        let local = collect_local_records(conn, &dek, "")?;
        let share_id: Option<String> = conn
            .query_row("SELECT value FROM sync_meta WHERE key='share_id'", [], |r| r.get(0))
            .ok();
        let share_role: Option<String> = conn
            .query_row("SELECT value FROM sync_meta WHERE key='share_role'", [], |r| r.get(0))
            .ok();
        let cloud_profile: String = conn
            .query_row("SELECT value FROM sync_meta WHERE key='cloud_profile'", [], |r| r.get(0))
            .unwrap_or_else(|_| profile.clone());

        let mut recent_edits = Vec::new();
        {
            let mut stmt = conn
                .prepare(
                    "SELECT name, COALESCE(edited_by,''), COALESCE(updated_at,'') FROM servers \
                     WHERE deleted=0 AND updated_at IS NOT NULL ORDER BY updated_at DESC LIMIT 5",
                )
                .map_err(|e| format!("[SYNC] RECENT_PREP: {e}"))?;
            let rows = stmt
                .query_map([], |r| {
                    Ok(RecentEdit {
                        name: r.get(0)?,
                        edited_by: r.get(1)?,
                        updated_at: r.get(2)?,
                    })
                })
                .map_err(|e| format!("[SYNC] RECENT_QUERY: {e}"))?;
            for row in rows {
                recent_edits.push(row.map_err(|e| format!("[SYNC] RECENT_ROW: {e}"))?);
            }
        }

        let mut server_names: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        {
            let mut stmt = conn
                .prepare("SELECT uuid, name FROM servers WHERE uuid IS NOT NULL AND deleted=0")
                .map_err(|e| format!("[SYNC] NAMES_PREP: {e}"))?;
            let rows = stmt
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
                .map_err(|e| format!("[SYNC] NAMES_QUERY: {e}"))?;
            for row in rows {
                let (u, n) = row.map_err(|e| format!("[SYNC] NAMES_ROW: {e}"))?;
                server_names.insert(u, n);
            }
        }

        (
            local,
            dek,
            share_id.map(|s| (s, share_role.unwrap_or_default())),
            cloud_profile,
            recent_edits,
            server_names,
        )
    };

    let total_records = local.len();
    let vault_bytes = profile_path(&app, &profile)
        .ok()
        .and_then(|p| std::fs::metadata(&p).ok())
        .map(|m| m.len())
        .unwrap_or(0);
    let _ = &dek; // reserved for future decrypt of incoming node names

    // Best-effort dry run: pure pull (empty push mutates nothing on the server).
    let diff = {
        let pulled = if let Some((share_id, _role)) = &share {
            cloud::shared_sync_exchange(&app, &cloud, share_id, "", &[]).await
        } else {
            cloud::sync_exchange(&app, &cloud, &cloud_profile, "", &[], None).await
        };
        match pulled {
            Ok(server) => {
                use std::collections::HashMap;
                let local_map: HashMap<&str, (&str, &str)> = local
                    .iter()
                    .map(|r| (r.uuid.as_str(), (r.updated_at.as_str(), r.entity_type.as_str())))
                    .collect();
                let server_map: HashMap<&str, (&str, &str)> = server
                    .iter()
                    // The reserved escrow record is bookkeeping, not user data.
                    .filter(|r| r.entity_type != ESCROW_ETYPE)
                    .map(|r| (r.uuid.as_str(), (r.updated_at.as_str(), r.entity_type.as_str())))
                    .collect();

                let mut in_sync = 0usize;
                let mut needs_push = 0usize;
                let mut needs_pull = 0usize;
                let mut out: Vec<String> = Vec::new();

                let mut uuids: std::collections::HashSet<&str> = std::collections::HashSet::new();
                uuids.extend(local_map.keys());
                uuids.extend(server_map.keys());
                for u in uuids {
                    let l = local_map.get(u).map(|(ua, _)| *ua).unwrap_or("");
                    let s = server_map.get(u).map(|(ua, _)| *ua).unwrap_or("");
                    let is_server = local_map.get(u).map(|(_, et)| *et == "servers").unwrap_or(false)
                        || server_map.get(u).map(|(_, et)| *et == "servers").unwrap_or(false);
                    if l == s {
                        in_sync += 1;
                    } else {
                        if l > s {
                            needs_push += 1;
                        } else {
                            needs_pull += 1;
                        }
                        if is_server && out.len() < 20 {
                            if let Some(name) = server_names.get(u) {
                                out.push(name.clone());
                            }
                        }
                    }
                }
                Some(SyncDiff { in_sync, needs_push, needs_pull, out_of_sync_nodes: out })
            }
            Err(_) => None,
        }
    };

    Ok(ProfileSyncStats { total_records, vault_bytes, recent_edits, diff })
}

// ===========================================================================
// Profile sharing commands (E2E). Identity lives in the cloud session; the
// per-profile DEK is what actually gets shared, sealed to each member.
// ===========================================================================

fn hex_to_32(s: &str) -> Result<[u8; 32], String> {
    let raw = hex::decode(s).map_err(|_| "[SHARE] BAD_HEX".to_string())?;
    if raw.len() != 32 {
        return Err("[SHARE] BAD_KEY_LEN".into());
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&raw);
    Ok(out)
}

#[derive(serde::Serialize)]
struct IdentityStatus {
    exists_on_server: bool,
    unlocked: bool,
    public_key: Option<String>,
}

/// Is a sharing identity set up (server) and/or unlocked (this session)?
#[tauri::command]
async fn identity_status(
    app: tauri::AppHandle,
    cloud: tauri::State<'_, std::sync::Arc<cloud::CloudState>>,
) -> Result<IdentityStatus, String> {
    let unlocked = cloud.identity().await;
    if let Some((pubk, _)) = unlocked {
        return Ok(IdentityStatus { exists_on_server: true, unlocked: true, public_key: Some(hex::encode(pubk)) });
    }
    // Not unlocked — ask the server whether a key exists (requires being signed in).
    if cloud.token().await.is_none() {
        return Ok(IdentityStatus { exists_on_server: false, unlocked: false, public_key: None });
    }
    let keys = cloud::fetch_my_keys(&app, &cloud).await?;
    Ok(IdentityStatus { exists_on_server: keys.exists, unlocked: false, public_key: keys.public_key })
}

/// Set up (first time) or unlock (already exists) the sharing identity from the
/// user's encryption passphrase. Never overwrites an existing key — a wrong
/// passphrase errors instead, so a real key with live grants is never orphaned
/// by a typo. Use `reset_identity` to deliberately regenerate.
#[tauri::command]
async fn setup_identity(
    app: tauri::AppHandle,
    cloud: tauri::State<'_, std::sync::Arc<cloud::CloudState>>,
    enc_passphrase: String,
) -> Result<IdentityStatus, String> {
    if enc_passphrase.chars().count() < 8 {
        return Err("[SHARE] WEAK_PASSPHRASE: use at least 8 characters".into());
    }
    let existing = cloud::fetch_my_keys(&app, &cloud).await?;
    if existing.exists {
        let salt_hex = existing.enc_salt.ok_or("[SHARE] MISSING_SALT")?;
        let wrapped = existing.wrapped_privkey.ok_or("[SHARE] MISSING_WRAPPED")?;
        let pub_hex = existing.public_key.ok_or("[SHARE] MISSING_PUB")?;
        let salt = hex::decode(&salt_hex).map_err(|_| "[SHARE] BAD_SALT")?;
        let secret = identity::unwrap_secret(&enc_passphrase, &salt, &wrapped).map_err(|_| {
            "[SHARE] WRONG_PASSPHRASE: that passphrase doesn't match your sharing identity. If you forgot it, reset your identity (regenerates keys — you'll need to re-share)."
        })?;
        if hex::encode(identity::public_of(&secret)) != pub_hex {
            return Err("[SHARE] KEY_MISMATCH: stored identity is inconsistent — reset your identity to regenerate.".into());
        }
        cloud.set_identity(identity::public_of(&secret), secret).await;
        return Ok(IdentityStatus { exists_on_server: true, unlocked: true, public_key: Some(pub_hex) });
    }
    // First-time setup — generate, wrap, publish.
    let kp = identity::generate_keypair();
    let mut salt = [0u8; 16];
    rand::thread_rng().fill(&mut salt);
    let wrapped = identity::wrap_secret(&enc_passphrase, &salt, &kp.secret)?;
    cloud::publish_identity(&app, &cloud, &hex::encode(kp.public), &wrapped, &hex::encode(salt)).await?;
    cloud.set_identity(kp.public, kp.secret).await;
    Ok(IdentityStatus { exists_on_server: true, unlocked: true, public_key: Some(hex::encode(kp.public)) })
}

/// Deliberately regenerate the sharing identity (e.g. forgotten passphrase).
/// Rotates the published key; grants sealed to the OLD key stop working, so the
/// caller must re-share afterwards. Overwrites unconditionally.
#[tauri::command]
async fn reset_identity(
    app: tauri::AppHandle,
    cloud: tauri::State<'_, std::sync::Arc<cloud::CloudState>>,
    enc_passphrase: String,
) -> Result<IdentityStatus, String> {
    if enc_passphrase.chars().count() < 8 {
        return Err("[SHARE] WEAK_PASSPHRASE: use at least 8 characters".into());
    }
    let kp = identity::generate_keypair();
    let mut salt = [0u8; 16];
    rand::thread_rng().fill(&mut salt);
    let wrapped = identity::wrap_secret(&enc_passphrase, &salt, &kp.secret)?;
    cloud::publish_identity(&app, &cloud, &hex::encode(kp.public), &wrapped, &hex::encode(salt)).await?;
    cloud.set_identity(kp.public, kp.secret).await;
    Ok(IdentityStatus { exists_on_server: true, unlocked: true, public_key: Some(hex::encode(kp.public)) })
}

#[derive(serde::Serialize)]
struct ShareResult {
    share_id: String,
}

/// Owner: turn the OPEN profile into a shared profile. Seals the profile DEK to
/// my own public key and records the share_id locally.
#[tauri::command]
async fn share_current_profile(
    app: tauri::AppHandle,
    db_state: tauri::State<'_, DbState>,
    cloud: tauri::State<'_, std::sync::Arc<cloud::CloudState>>,
    name: String,
) -> Result<ShareResult, String> {
    let name = name.trim().to_string();
    if name.is_empty() || name.chars().count() > 64 {
        return Err("[SHARE] BAD_NAME: 1-64 characters".into());
    }
    let (my_pub, _) = cloud.identity().await.ok_or("[SHARE] IDENTITY_LOCKED: set up your sharing identity first")?;
    let dek = {
        let conn_g = db_state.conn.lock().map_err(|_| "[STATE] LOCK_CONN")?;
        let conn = conn_g.as_ref().ok_or("[STATE] DB_NOT_OPEN")?;
        get_or_create_dek(conn)?.0
    };
    let sealed_self = hex::encode(identity::seal_to(&my_pub, &dek)?);
    let share_id = new_entity_uuid(); // 32 hex — within the server's {16,64}
    cloud::create_share(&app, &cloud, &share_id, &name, &sealed_self).await?;
    {
        let conn_g = db_state.conn.lock().map_err(|_| "[STATE] LOCK_CONN")?;
        let conn = conn_g.as_ref().ok_or("[STATE] DB_NOT_OPEN")?;
        conn.execute(
            "INSERT INTO sync_meta(key,value) VALUES('share_id',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            [&share_id],
        )
        .map_err(|e| format!("[SHARE] STORE_SHARE_ID: {e}"))?;
        conn.execute(
            "INSERT INTO sync_meta(key,value) VALUES('share_role','owner') ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            [],
        )
        .map_err(|e| format!("[SHARE] STORE_ROLE: {e}"))?;
    }
    save_vault_async(&db_state).await?;
    Ok(ShareResult { share_id })
}

#[derive(serde::Serialize)]
struct InviteResult {
    email: String,
}

/// Owner: invite `email` to a share at `role` (editor|user). Obtains the DEK by
/// unsealing my own grant, then re-seals it to the invitee's public key.
#[tauri::command]
async fn invite_to_share(
    app: tauri::AppHandle,
    cloud: tauri::State<'_, std::sync::Arc<cloud::CloudState>>,
    share_id: String,
    email: String,
    role: String,
) -> Result<InviteResult, String> {
    if role != "editor" && role != "user" {
        return Err("[SHARE] BAD_ROLE: role must be 'editor' or 'user'".into());
    }
    let (_, my_secret) = cloud.identity().await.ok_or("[SHARE] IDENTITY_LOCKED")?;
    // Get the DEK by unsealing my own grant for this share.
    let mine = cloud::share_dek(&app, &cloud, &share_id).await?;
    let dek_bytes = identity::unseal(&my_secret, &hex::decode(&mine.sealed_dek).map_err(|_| "[SHARE] BAD_GRANT")?)
        .map_err(|_| "[SHARE] UNSEAL_SELF: cannot open your own grant — your identity may have changed")?;
    let dek = {
        if dek_bytes.len() != 32 {
            return Err("[SHARE] BAD_DEK_LEN".into());
        }
        let mut d = [0u8; 32];
        d.copy_from_slice(&dek_bytes);
        d
    };
    let info = cloud::lookup_pubkey(&app, &cloud, &email)
        .await?
        .ok_or("[SHARE] NO_ACCOUNT: that email hasn't set up sharing yet")?;
    let invitee_pub = hex_to_32(&info.public_key)?;
    let sealed = hex::encode(identity::seal_to(&invitee_pub, &dek)?);
    cloud::invite_member(&app, &cloud, &share_id, &email, &role, &sealed).await?;
    Ok(InviteResult { email: info.email })
}

/// Every share I'm a member of.
#[tauri::command]
async fn list_shares(
    app: tauri::AppHandle,
    cloud: tauri::State<'_, std::sync::Arc<cloud::CloudState>>,
) -> Result<Vec<cloud::ShareInfo>, String> {
    cloud::list_shares(&app, &cloud).await
}

/// The member roster of a share I belong to.
#[tauri::command]
async fn share_member_list(
    app: tauri::AppHandle,
    cloud: tauri::State<'_, std::sync::Arc<cloud::CloudState>>,
    share_id: String,
) -> Result<Vec<cloud::MemberInfo>, String> {
    cloud::share_members(&app, &cloud, &share_id).await
}

#[derive(serde::Serialize)]
struct AcceptResult {
    role: String,
}

/// Accept a pending invite. Verifies I can unseal the DEK with my identity, then
/// records the share locally. (Materialising it as a syncable local profile is
/// wired in S3c.)
#[tauri::command]
async fn accept_share(
    app: tauri::AppHandle,
    cloud: tauri::State<'_, std::sync::Arc<cloud::CloudState>>,
    share_id: String,
) -> Result<AcceptResult, String> {
    let (_, my_secret) = cloud.identity().await.ok_or("[SHARE] IDENTITY_LOCKED: unlock your sharing identity first")?;
    let resp = cloud::accept_share(&app, &cloud, &share_id).await?;
    let dek = identity::unseal(&my_secret, &hex::decode(&resp.sealed_dek).map_err(|_| "[SHARE] BAD_GRANT")?)
        .map_err(|_| "[SHARE] UNSEAL_FAILED: this invite wasn't sealed to your current identity")?;
    if dek.len() != 32 {
        return Err("[SHARE] BAD_DEK_LEN".into());
    }
    Ok(AcceptResult { role: resp.role })
}

/// Invitee: materialise an accepted share as a local profile and pull its data.
/// Creates a fresh local vault (encrypted at rest with `vault_password`), sets
/// its DEK to the shared data-key unsealed from my grant, records the share,
/// then syncs to populate it. The vault password is local-only and unrelated to
/// the DEK, so each member protects their on-disk copy with their own password.
#[tauri::command]
async fn import_shared_profile(
    app: tauri::AppHandle,
    db_state: tauri::State<'_, DbState>,
    cloud: tauri::State<'_, std::sync::Arc<cloud::CloudState>>,
    share_id: String,
    local_name: String,
    vault_password: String,
) -> Result<SyncReport, String> {
    let (_, my_secret) = cloud.identity().await.ok_or("[SHARE] IDENTITY_LOCKED: unlock your sharing identity first")?;
    let mine = cloud::share_dek(&app, &cloud, &share_id).await?;
    let dek_vec = identity::unseal(&my_secret, &hex::decode(&mine.sealed_dek).map_err(|_| "[SHARE] BAD_GRANT")?)
        .map_err(|_| "[SHARE] UNSEAL_FAILED: this share wasn't sealed to your identity")?;
    if dek_vec.len() != 32 {
        return Err("[SHARE] BAD_DEK_LEN".into());
    }
    let dek_hex = hex::encode(&dek_vec);

    // Create the local vault (fresh profile) — same steps as create_profile.
    validate_profile_name(&local_name)?;
    if vault_password.is_empty() {
        return Err("Password cannot be empty".into());
    }
    let dir = profiles_dir(&app)?;
    fs::create_dir_all(&dir).map_err(|e| format!("[FILE] MKDIR_FAILED: {}", e))?;
    if profile_path(&app, &local_name)?.exists() {
        return Err(format!("Profile '{}' already exists", local_name));
    }
    *db_state.active_profile.lock().map_err(|_| "[STATE] LOCK_FAILED")? = Some(local_name.clone());
    // setup_master_db consumes a State handle; hand it a fresh one from `app` so
    // our own `db_state` stays usable for the sync_meta writes + sync below.
    {
        use tauri::Manager as _;
        setup_master_db(app.clone(), vault_password, app.state::<DbState>()).await?;
    }

    // Replace the freshly-minted DEK with the SHARED one + record share meta, so
    // sync_now routes to /shares/sync and decrypts the shared blobs correctly.
    {
        let conn_g = db_state.conn.lock().map_err(|_| "[STATE] LOCK_CONN")?;
        let conn = conn_g.as_ref().ok_or("[STATE] DB_NOT_OPEN")?;
        conn.execute(
            "INSERT INTO sync_meta(key,value) VALUES('dek',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            [&dek_hex],
        )
        .map_err(|e| format!("[SHARE] SET_DEK: {e}"))?;
        conn.execute(
            "INSERT INTO sync_meta(key,value) VALUES('share_id',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            [&share_id],
        )
        .map_err(|e| format!("[SHARE] SET_SHARE_ID: {e}"))?;
        conn.execute(
            "INSERT INTO sync_meta(key,value) VALUES('share_role',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            [&mine.role],
        )
        .map_err(|e| format!("[SHARE] SET_ROLE: {e}"))?;
    }
    // Pull the shared data into the new profile.
    sync_now(app, db_state, cloud).await
}

/// New device: restore one of YOUR OWN personal cloud profiles using nothing but
/// its password. Pulls the profile's /sync stream, unseals its DEK from the
/// reserved escrow record by re-deriving the master key from the password + the
/// salt carried in that record, materialises a fresh local vault under that DEK
/// (protected by the same password), and populates it. This is the
/// personal-profile counterpart to `import_shared_profile` — no whole-vault blob
/// download, no sharing identity, no separate recovery secret. The profile must
/// have been synced at least once since it became password-restorable (so the
/// escrow record exists on the server).
#[tauri::command]
async fn restore_personal_profile(
    app: tauri::AppHandle,
    db_state: tauri::State<'_, DbState>,
    cloud: tauri::State<'_, std::sync::Arc<cloud::CloudState>>,
    cloud_profile: String,
    local_name: String,
    vault_password: String,
) -> Result<SyncReport, String> {
    // Validate up front so a bad name/empty password fails before any network.
    let cloud_profile = cloud_profile.trim().to_string();
    if cloud_profile.is_empty() {
        return Err("[SYNC] NO_PROFILE_NAME".into());
    }
    validate_profile_name(&local_name)?;
    if vault_password.is_empty() {
        return Err("Password cannot be empty".into());
    }

    // Peek at the cloud stream (empty push) and lift the escrow record out.
    let peek = cloud::sync_exchange(&app, &cloud, &cloud_profile, "", &[], None).await?;
    let sealed = peek
        .iter()
        .find(|r| r.entity_type == ESCROW_ETYPE && r.uuid == ESCROW_UUID)
        .and_then(|r| r.blob.clone())
        .ok_or("[SYNC] NO_ESCROW: no restorable copy of that profile in your cloud. Open it on the original device and sync once, then try again.")?;

    // Re-derive the master key from the password (Argon2id → blocking pool) and
    // unseal the DEK. A wrong password fails the GCM tag → surfaced as a wrong
    // password below.
    let pw_for_escrow = vault_password.clone();
    let dek = tokio::task::spawn_blocking(move || open_pw_escrow(&sealed, &pw_for_escrow))
        .await
        .map_err(|e| format!("[SYNC] ESCROW_JOIN: {e}"))?
        .map_err(|_| "[SYNC] ESCROW_UNSEAL_FAILED: wrong password for this profile")?;
    let dek_hex = hex::encode(dek);

    // Create the local vault (fresh profile) — same steps as create_profile.
    let dir = profiles_dir(&app)?;
    fs::create_dir_all(&dir).map_err(|e| format!("[FILE] MKDIR_FAILED: {}", e))?;
    if profile_path(&app, &local_name)?.exists() {
        return Err(format!("Profile '{}' already exists", local_name));
    }
    *db_state.active_profile.lock().map_err(|_| "[STATE] LOCK_FAILED")? = Some(local_name.clone());
    {
        use tauri::Manager as _;
        setup_master_db(app.clone(), vault_password, app.state::<DbState>()).await?;
    }

    // Swap the freshly-minted random DEK for the escrowed one, and remember the
    // cloud key in case the local name differs, so sync_now targets the right
    // /sync stream and decrypts the pulled blobs.
    {
        let conn_g = db_state.conn.lock().map_err(|_| "[STATE] LOCK_CONN")?;
        let conn = conn_g.as_ref().ok_or("[STATE] DB_NOT_OPEN")?;
        conn.execute(
            "INSERT INTO sync_meta(key,value) VALUES('dek',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            [&dek_hex],
        )
        .map_err(|e| format!("[SYNC] SET_DEK: {e}"))?;
        conn.execute(
            "INSERT INTO sync_meta(key,value) VALUES('cloud_profile',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            [&cloud_profile],
        )
        .map_err(|e| format!("[SYNC] SET_CLOUD_PROFILE: {e}"))?;
    }
    sync_now(app, db_state, cloud).await
}

/// Owner: change a member's role.
#[tauri::command]
async fn share_set_role(
    app: tauri::AppHandle,
    cloud: tauri::State<'_, std::sync::Arc<cloud::CloudState>>,
    share_id: String,
    member_user_id: i64,
    role: String,
) -> Result<(), String> {
    if role != "editor" && role != "user" {
        return Err("[SHARE] BAD_ROLE".into());
    }
    cloud::set_member_role(&app, &cloud, &share_id, member_user_id, &role).await
}

/// Owner: remove a member.
#[tauri::command]
async fn share_revoke(
    app: tauri::AppHandle,
    cloud: tauri::State<'_, std::sync::Arc<cloud::CloudState>>,
    share_id: String,
    member_user_id: i64,
) -> Result<(), String> {
    cloud::revoke_member(&app, &cloud, &share_id, member_user_id).await
}

/// Non-owner: leave a share.
#[tauri::command]
async fn share_leave(
    app: tauri::AppHandle,
    cloud: tauri::State<'_, std::sync::Arc<cloud::CloudState>>,
    share_id: String,
) -> Result<(), String> {
    cloud::leave_share(&app, &cloud, &share_id).await
}

/// Owner: delete the whole shared profile.
#[tauri::command]
async fn share_delete(
    app: tauri::AppHandle,
    cloud: tauri::State<'_, std::sync::Arc<cloud::CloudState>>,
    share_id: String,
) -> Result<(), String> {
    cloud::delete_share(&app, &cloud, &share_id).await
}

#[derive(serde::Serialize)]
struct ProfileShareStatus {
    profile: Option<String>,
    share_id: Option<String>,
    role: Option<String>,
    identity_unlocked: bool,
    signed_in: bool,
    email: Option<String>,
}

/// Everything the in-app Profile panel needs about the OPEN profile, read in one
/// shot with no network call so the panel paints instantly: which profile is
/// open, whether it's shared and at what role, and whether the cloud account and
/// sharing identity are ready. The member roster is fetched separately.
#[tauri::command]
async fn profile_share_status(
    db_state: tauri::State<'_, DbState>,
    cloud: tauri::State<'_, std::sync::Arc<cloud::CloudState>>,
) -> Result<ProfileShareStatus, String> {
    // Scoped so both std MutexGuards drop before the awaits below.
    let (profile, share_id, role) = {
        let profile = db_state
            .active_profile
            .lock()
            .map_err(|_| "[STATE] LOCK_FAILED")?
            .clone();
        let conn_g = db_state.conn.lock().map_err(|_| "[STATE] LOCK_CONN")?;
        match conn_g.as_ref() {
            Some(conn) => {
                let sid: Option<String> = conn
                    .query_row("SELECT value FROM sync_meta WHERE key='share_id'", [], |r| r.get(0))
                    .ok();
                let role: Option<String> = conn
                    .query_row("SELECT value FROM sync_meta WHERE key='share_role'", [], |r| r.get(0))
                    .ok();
                (profile, sid, role)
            }
            None => (profile, None, None),
        }
    };
    let st = cloud.status().await;
    Ok(ProfileShareStatus {
        profile,
        share_id,
        role,
        identity_unlocked: cloud.identity().await.is_some(),
        signed_in: st.signed_in,
        email: st.email,
    })
}

/// Stop sharing the OPEN profile. Owner → tears the share down for everyone;
/// member → leaves it. Either way this device keeps its local copy: only the
/// share bookkeeping is cleared, so the profile falls back to syncing on the
/// personal path under the DEK it already holds.
#[tauri::command]
async fn stop_sharing(
    app: tauri::AppHandle,
    db_state: tauri::State<'_, DbState>,
    cloud: tauri::State<'_, std::sync::Arc<cloud::CloudState>>,
) -> Result<(), String> {
    let (share_id, role) = {
        let conn_g = db_state.conn.lock().map_err(|_| "[STATE] LOCK_CONN")?;
        let conn = conn_g.as_ref().ok_or("[STATE] DB_NOT_OPEN")?;
        let sid: Option<String> = conn
            .query_row("SELECT value FROM sync_meta WHERE key='share_id'", [], |r| r.get(0))
            .ok();
        let role: Option<String> = conn
            .query_row("SELECT value FROM sync_meta WHERE key='share_role'", [], |r| r.get(0))
            .ok();
        (
            sid.ok_or("[SHARE] NOT_SHARED: this profile isn't shared")?,
            role.unwrap_or_default(),
        )
    };
    if role == "owner" {
        cloud::delete_share(&app, &cloud, &share_id).await?;
    } else {
        cloud::leave_share(&app, &cloud, &share_id).await?;
    }
    {
        let conn_g = db_state.conn.lock().map_err(|_| "[STATE] LOCK_CONN")?;
        let conn = conn_g.as_ref().ok_or("[STATE] DB_NOT_OPEN")?;
        conn.execute("DELETE FROM sync_meta WHERE key IN ('share_id','share_role')", [])
            .map_err(|e| format!("[SHARE] CLEAR_META: {e}"))?;
    }
    save_vault_async(&db_state).await
}

/// Set the label stamped onto future local edits (the `edited_by` column). The
/// frontend calls this on unlock with the signed-in cloud email so changes are
/// attributed to a person across shared/multi-device use. An empty label clears
/// it (edits stay unattributed). Local-only — sync_meta never leaves the vault.
#[tauri::command]
async fn set_editor_label(db_state: tauri::State<'_, DbState>, label: String) -> Result<(), String> {
    let label = label.trim().to_string();
    {
        let conn_g = db_state.conn.lock().map_err(|_| "[STATE] LOCK_CONN")?;
        let conn = conn_g.as_ref().ok_or("[STATE] DB_NOT_OPEN")?;
        if label.is_empty() {
            conn.execute("DELETE FROM sync_meta WHERE key='editor_label'", [])
                .map_err(|e| format!("[SYNC] EDITOR_CLEAR: {e}"))?;
        } else {
            conn.execute(
                "INSERT INTO sync_meta(key,value) VALUES('editor_label',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                [&label],
            )
            .map_err(|e| format!("[SYNC] EDITOR_SET: {e}"))?;
        }
    }
    save_vault_async(&db_state).await
}

#[cfg(test)]
mod sync_engine_tests {
    use super::*;
    use rusqlite::Connection;

    const KEY: [u8; 32] = [7u8; 32];

    // The DEK-escrow record round-trips through the sync stream: sealed under
    // the profile's own password on push, recovered on a fresh device from just
    // that password — and it's inert to the merge engine (never a phantom row).
    #[test]
    fn dek_escrow_round_trips_and_apply_ignores_it() {
        let dek: [u8; 32] = KEY;
        let salt = [3u8; SALT_LEN];
        let password = "correct horse battery staple";
        let master_key = derive_key(password, &salt).unwrap();
        let rec = build_pw_escrow_record(&master_key, &salt, &dek).unwrap();
        assert_eq!(rec.entity_type, ESCROW_ETYPE);
        assert_eq!(rec.uuid, ESCROW_UUID);

        // A fresh device recovers the exact DEK from just the password...
        let recovered = open_pw_escrow(rec.blob.as_ref().unwrap(), password).unwrap();
        assert_eq!(recovered, dek, "escrow must recover the exact DEK");
        // ...and the wrong password must not (GCM tag fails).
        assert!(
            open_pw_escrow(rec.blob.as_ref().unwrap(), "wrong password").is_err(),
            "wrong password must not recover the DEK"
        );

        // Applying an escrow record must not create a phantom row anywhere.
        let (b, hb) = device("B");
        apply_remote_records(&b, &KEY, std::slice::from_ref(&rec), &hb).unwrap();
        for spec in ENTITIES {
            let n: i64 = b
                .query_row(&format!("SELECT COUNT(*) FROM {}", spec.table), [], |r| r.get(0))
                .unwrap();
            assert_eq!(n, 0, "escrow record must not land in table {}", spec.table);
        }
    }

    #[test]
    fn edited_by_is_stamped_and_syncs() {
        let (a, _ha) = device("A");
        a.execute("INSERT INTO sync_meta(key,value) VALUES('editor_label','alice@x.com')", []).unwrap();
        a.execute("INSERT INTO servers(name,host,port) VALUES('web','h',22)", []).unwrap();
        let recs = collect_local_records(&a, &KEY, "").unwrap();
        let srv = recs.iter().find(|r| r.entity_type == "servers").unwrap();
        let plain = decrypt_entity(srv.blob.as_ref().unwrap(), &KEY).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&plain).unwrap();
        assert_eq!(v["edited_by"], "alice@x.com", "trigger must stamp the editor label");

        // Cross-device apply preserves who edited it (not overwritten locally).
        let (b, hb) = device("B");
        apply_remote_records(&b, &KEY, &recs, &hb).unwrap();
        let eb: String = b.query_row("SELECT edited_by FROM servers WHERE name='web'", [], |r| r.get(0)).unwrap();
        assert_eq!(eb, "alice@x.com");
    }

    #[test]
    fn dek_is_stable_and_encrypts_blobs() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute("CREATE TABLE sync_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)", [])
            .unwrap();
        let (dek1, created1) = get_or_create_dek(&conn).unwrap();
        assert!(created1, "first call must mint the DEK");
        let (dek2, created2) = get_or_create_dek(&conn).unwrap();
        assert!(!created2, "second call must reuse it");
        assert_eq!(dek1, dek2, "DEK must be stable across opens");
        // A per-entity blob encrypted under the DEK round-trips with the reloaded DEK.
        let blob = encrypt_entity(b"{\"host\":\"h\"}", &dek1).unwrap();
        assert_eq!(decrypt_entity(&blob, &dek2).unwrap(), b"{\"host\":\"h\"}");
    }

    fn device(node: &str) -> (Connection, std::sync::Arc<hlc::Hlc>) {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE folders(id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT, parent_id INTEGER, color TEXT, uuid TEXT, updated_at TEXT, deleted INTEGER NOT NULL DEFAULT 0, edited_by TEXT);
             CREATE TABLE ssh_keys(id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT, public_key TEXT, private_key TEXT, passphrase TEXT, uuid TEXT, updated_at TEXT, deleted INTEGER NOT NULL DEFAULT 0, edited_by TEXT);
             CREATE TABLE credentials(id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT, auth_type TEXT, username TEXT, password TEXT, key_id INTEGER, uuid TEXT, updated_at TEXT, deleted INTEGER NOT NULL DEFAULT 0, edited_by TEXT);
             CREATE TABLE servers(id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT, host TEXT, port INTEGER, username TEXT, password TEXT, credential_id INTEGER, folder_id INTEGER, proxy_type TEXT, proxy_host TEXT, proxy_port INTEGER, tunnels TEXT, auth_type TEXT, key_id INTEGER, autostart INTEGER, mirrors TEXT, color TEXT, notes TEXT, run_on_connect TEXT, jump_host_id INTEGER, uuid TEXT, updated_at TEXT, deleted INTEGER NOT NULL DEFAULT 0, edited_by TEXT);
             CREATE TABLE commands(id INTEGER PRIMARY KEY AUTOINCREMENT, title TEXT, content TEXT, uuid TEXT, updated_at TEXT, deleted INTEGER NOT NULL DEFAULT 0, edited_by TEXT);
             CREATE TABLE notes(id INTEGER PRIMARY KEY AUTOINCREMENT, title TEXT, body TEXT, uuid TEXT, updated_at TEXT, deleted INTEGER NOT NULL DEFAULT 0, edited_by TEXT);
             CREATE TABLE monitor_configs(node_id INTEGER PRIMARY KEY, enabled_metrics TEXT, custom_metrics TEXT, paused INTEGER, uuid TEXT, updated_at TEXT, deleted INTEGER NOT NULL DEFAULT 0, edited_by TEXT);",
        ).unwrap();
        let hlc = std::sync::Arc::new(hlc::Hlc::new(node.into(), 0));
        register_sync_functions(&conn, &hlc).unwrap();
        create_sync_triggers(&conn).unwrap();
        (conn, hlc)
    }

    #[test]
    fn round_trip_resolves_fks_across_devices() {
        let (a, _ha) = device("A");
        a.execute("INSERT INTO ssh_keys(name, public_key, private_key) VALUES('k','pub','priv')", []).unwrap();
        a.execute("INSERT INTO credentials(name, auth_type, username, key_id) VALUES('c','key','root',(SELECT id FROM ssh_keys WHERE name='k'))", []).unwrap();
        a.execute("INSERT INTO folders(name) VALUES('f')", []).unwrap();
        a.execute("INSERT INTO servers(name, host, port, credential_id, folder_id) VALUES('s1','h',22,(SELECT id FROM credentials WHERE name='c'),(SELECT id FROM folders WHERE name='f'))", []).unwrap();
        let recs = collect_local_records(&a, &KEY, "").unwrap();

        let (b, hb) = device("B");
        // Unrelated pre-existing row so B's autoincrement ids differ from A's,
        // proving the merge resolves references by uuid, not by raw id.
        b.execute("INSERT INTO folders(name) VALUES('other')", []).unwrap();
        apply_remote_records(&b, &KEY, &recs, &hb).unwrap();

        let (cred, folder): (String, String) = b.query_row(
            "SELECT (SELECT name FROM credentials c WHERE c.id=s.credential_id), (SELECT name FROM folders f WHERE f.id=s.folder_id) FROM servers s WHERE s.name='s1'",
            [], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
        assert_eq!(cred, "c", "server.credential_id must resolve to the right credential on B");
        assert_eq!(folder, "f", "server.folder_id must resolve to the right folder on B");
        let key_name: String = b.query_row("SELECT (SELECT name FROM ssh_keys k WHERE k.id=c.key_id) FROM credentials c WHERE c.name='c'", [], |r| r.get(0)).unwrap();
        assert_eq!(key_name, "k", "credential.key_id must resolve to the right ssh_key on B");
    }

    #[test]
    fn lww_older_remote_does_not_clobber_newer_local() {
        let (a, _ha) = device("A");
        a.execute("INSERT INTO servers(name, host) VALUES('s','h1')", []).unwrap();
        let old = collect_local_records(&a, &KEY, "").unwrap();
        let (b, hb) = device("B");
        apply_remote_records(&b, &KEY, &old, &hb).unwrap();
        b.execute("UPDATE servers SET host='h2-newer' WHERE name='s'", []).unwrap();
        apply_remote_records(&b, &KEY, &old, &hb).unwrap(); // re-apply the STALE version
        let host: String = b.query_row("SELECT host FROM servers WHERE name='s'", [], |r| r.get(0)).unwrap();
        assert_eq!(host, "h2-newer", "an older remote record must not overwrite a newer local edit");
    }

    // `sync_now` snapshots records under the DB lock, then DROPS the lock for
    // the network leg — so the user can delete a node while their own batch is
    // still in flight. The reply is the server's view computed from the
    // PRE-delete push (and `since` is always "", so it echoes everything back),
    // meaning it still carries the deleted node as live. The tombstone is the
    // only thing standing between that reply and a resurrected password.
    #[test]
    fn stale_inflight_batch_does_not_resurrect_a_deleted_row() {
        let (a, ha) = device("A");
        a.execute("INSERT INTO servers(name, host, password) VALUES('prod','h','SUPERSECRET')", []).unwrap();

        // T=0 — sync collects the batch and lets go of the lock.
        let inflight = collect_local_records(&a, &KEY, "").unwrap();

        // T=2s — user deletes the node while the POST is still in flight.
        a.execute("DELETE FROM servers WHERE name='prod'", []).unwrap();

        // T=5s — reply lands, still carrying 'prod' as live at its old stamp.
        apply_remote_records(&a, &KEY, &inflight, &ha).unwrap();

        let live: i64 = a
            .query_row("SELECT COUNT(*) FROM servers WHERE name='prod'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(live, 0, "a stale in-flight batch must not resurrect a deleted row");
    }

    // The mirror case: a delete must not shadow a genuine re-creation. If some
    // other device creates a row again AFTER our delete, its stamp is newer than
    // our tombstone and it has to land.
    #[test]
    fn tombstone_does_not_block_a_newer_recreate() {
        let (a, ha) = device("A");
        a.execute("INSERT INTO servers(name, host) VALUES('s','h1')", []).unwrap();
        let uuid: String = a.query_row("SELECT uuid FROM servers WHERE name='s'", [], |r| r.get(0)).unwrap();
        a.execute("DELETE FROM servers WHERE name='s'", []).unwrap();
        assert_eq!(
            a.query_row("SELECT COUNT(*) FROM sync_tombstones WHERE uuid=?1", [&uuid], |r| r.get::<_, i64>(0)).unwrap(),
            1,
        );

        // Device B re-creates the same uuid later (newer HLC than our tombstone).
        let (b, hb) = device("B");
        b.execute("INSERT INTO servers(name, host) VALUES('s','h2-recreated')", []).unwrap();
        b.execute("UPDATE servers SET uuid=?1, updated_at=hlc_now() WHERE name='s'", [&uuid]).unwrap();
        let recreate = collect_local_records(&b, &KEY, "").unwrap();
        let _ = hb;

        apply_remote_records(&a, &KEY, &recreate, &ha).unwrap();
        let host: Option<String> = a.query_row("SELECT host FROM servers WHERE uuid=?1", [&uuid], |r| r.get(0)).ok();
        assert_eq!(
            host.as_deref(),
            Some("h2-recreated"),
            "a re-create newer than the tombstone must still apply",
        );
    }

    #[test]
    fn delete_propagates_via_tombstone() {
        let (a, _ha) = device("A");
        a.execute("INSERT INTO servers(name, host) VALUES('s','h')", []).unwrap();
        let (b, hb) = device("B");
        apply_remote_records(&b, &KEY, &collect_local_records(&a, &KEY, "").unwrap(), &hb).unwrap();
        assert_eq!(b.query_row("SELECT COUNT(*) FROM servers WHERE name='s'", [], |r| r.get::<_, i64>(0)).unwrap(), 1);
        a.execute("DELETE FROM servers WHERE name='s'", []).unwrap();
        let recs = collect_local_records(&a, &KEY, "").unwrap();
        assert!(recs.iter().any(|r| r.deleted), "delete must produce a tombstone record");
        apply_remote_records(&b, &KEY, &recs, &hb).unwrap();
        assert_eq!(b.query_row("SELECT COUNT(*) FROM servers WHERE name='s'", [], |r| r.get::<_, i64>(0)).unwrap(), 0, "delete must propagate to B");
    }
}

/// Async-friendly vault save. Snapshots the key/salt/path under the sync
/// mutexes, clones the Arc'd connection slot, then hands the whole thing
/// to `spawn_blocking`. The SQLite serialise + zstd + AES-GCM + fsync
/// chain runs on a blocking-pool thread so concurrent terminal output
/// and keystrokes don't stall on the tokio worker pool.
async fn save_vault_async(state: &DbState) -> Result<(), String> {
    let conn_arc = std::sync::Arc::clone(&state.conn);
    let (key, salt, path) = {
        let kg = state.master_key.lock().map_err(|_| "[STATE] MUTEX_POISON_KEY")?;
        let sg = state.salt.lock().map_err(|_| "[STATE] MUTEX_POISON_SALT")?;
        let pg = state.db_path.lock().map_err(|_| "[STATE] MUTEX_POISON_PATH")?;
        match (kg.as_ref(), sg.as_ref(), pg.as_ref()) {
            (Some(k), Some(s), Some(p)) => (k.clone(), *s, p.clone()),
            _ => return Err("[STATE] MISSING_REQUIRED_RESOURCES_FOR_SAVE".into()),
        }
    };
    tokio::task::spawn_blocking(move || {
        let cg = conn_arc.lock().map_err(|_| "[STATE] MUTEX_POISON_CONN")?;
        let conn = cg.as_ref().ok_or("[STATE] DATABASE_NOT_INITIALIZED")?;
        save_vault_blocking(conn, &key, &salt, &path)
    })
    .await
    .map_err(|e| format!("[CRYPTO] VAULT_JOIN: {}", e))?
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

/// List the caller's CLOUD profiles (from the per-entity sync store) so the
/// picker can surface profiles that exist in the account but not yet on this
/// device — the "sign in and see all your profiles" path. Read-only; requires
/// a signed-in cloud session. The UI merges these with the local
/// `list_profiles`, matching by name: a profile present locally is opened with
/// its password; a cloud-only one is restored (name pre-filled) then opened.
#[tauri::command]
async fn cloud_list_sync_profiles(
    app: tauri::AppHandle,
    cloud: tauri::State<'_, std::sync::Arc<cloud::CloudState>>,
) -> Result<Vec<cloud::SyncProfileInfo>, String> {
    cloud::list_sync_profiles(&app, &cloud).await
}

/// Delete one of the caller's OWN personal profiles from the cloud: wipes every
/// synced record (data, tombstones, and the escrow key) for that partition on
/// the server. Owner-scoped by construction — the /sync store is per-user, so a
/// caller can only ever delete their own partition. The local vault (if any) is
/// left alone; the UI offers "remove from this device" as a separate action.
/// Returns the number of records the server removed.
#[tauri::command]
async fn cloud_delete_profile(
    app: tauri::AppHandle,
    cloud: tauri::State<'_, std::sync::Arc<cloud::CloudState>>,
    profile: String,
) -> Result<i64, String> {
    let profile = profile.trim().to_string();
    if profile.is_empty() {
        return Err("[SYNC] NO_PROFILE_NAME".into());
    }
    cloud::delete_sync_profile(&app, &cloud, &profile).await
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
    // 0. Persist any accumulated in-memory changes (chief among them:
    // cmd_history rows written by TerminalView's Enter-key handler, which
    // deliberately skip a per-keystroke fsync). Best-effort: if the vault
    // isn't in a saveable state (partial init, crypto error) just log via
    // the returned Err and continue teardown — losing recent history is
    // preferable to leaking session state.
    let _ = save_vault_async(&state).await;

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
        let temp = session_sftp_dir(sid);
        if temp.exists() {
            let _ = std::fs::remove_dir_all(&temp);
        }
        let drag = session_drag_dir(sid);
        if drag.exists() {
            let _ = std::fs::remove_dir_all(&drag);
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
    // Same for any pending keyboard-interactive (2FA) prompt — sending `None`
    // signals cancel so the connect worker aborts the interactive attempt
    // instead of hanging on its 120s timeout after the profile is gone.
    let kbi_waiters: Vec<tokio::sync::oneshot::Sender<Option<Vec<String>>>> =
        ssh.kbi_txs.lock().await.drain().map(|(_, v)| v).collect();
    for tx in kbi_waiters {
        let _ = tx.send(None);
    }

    // 5. Belt-and-suspenders: clear the residual maps in case anything
    // raced in between the steps above.
    ssh.tunnels.lock().await.clear();
    ssh.forwarded_targets.lock().await.clear();
    ssh.sftp_sessions.lock().await.clear();
    ssh.connections.lock().await.clear();
    ssh.jump_connections.lock().await.clear();
    ssh.fp_txs.lock().await.clear();
    ssh.kbi_txs.lock().await.clear();

    // 6. Drop DB state last, so any in-flight write triggered by a
    // disconnecting handler above had a valid DB to land in.
    *state.conn.lock().map_err(|_| "[STATE] LOCK_FAILED_CONN")? = None;
    *state.master_key.lock().map_err(|_| "[STATE] LOCK_FAILED_KEY")? = None;
    *state.salt.lock().map_err(|_| "[STATE] LOCK_FAILED_SALT")? = None;
    *state.db_path.lock().map_err(|_| "[STATE] LOCK_FAILED_PATH")? = None;
    *state.active_profile.lock().map_err(|_| "[STATE] LOCK_FAILED_PROFILE")? = None;
    *state.hlc.lock().map_err(|_| "[STATE] LOCK_FAILED_HLC")? = None;
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

    // Native file dialogs are desktop-only. On Android we'd hit the SAF
    // intent system via a Tauri plugin instead, but profile export from
    // the mobile UI isn't a wired feature yet, so the command just refuses.
    #[cfg(target_os = "android")]
    {
        let _ = src;
        return Err("Profile export is not available on Android.".into());
    }
    #[cfg(not(target_os = "android"))]
    {
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
    #[cfg(target_os = "android")]
    {
        return Err("Profile import is not available on Android.".into());
    }
    #[cfg(not(target_os = "android"))]
    {
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
    let mut needs_resave;

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
        // Schema version metadata. A single-row `schema_meta` table records
        // the highest column-migration the running binary knows about. If a
        // user opens an older binary against a newer vault, we surface a
        // clear warning instead of silently swallowing "duplicate column"
        // errors and risking write-side schema drift. Bump SCHEMA_VERSION
        // here every time a new ALTER lands in this block.
        conn.execute(
            "CREATE TABLE IF NOT EXISTS schema_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)",
            [],
        ).map_err(|e| format!("[DATABASE] META_TABLE_FAILED: {}", e))?;
        // v5 — command history table for the Ctrl+R overlay. Best-effort
        // captured per-Enter by TerminalView; unencrypted-within-vault since
        // it's not a secret (the vault itself is encrypted at rest).
        conn.execute(
            "CREATE TABLE IF NOT EXISTS cmd_history (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                server_id INTEGER,
                server_name TEXT,
                command TEXT NOT NULL,
                ts INTEGER NOT NULL,
                exit_code INTEGER
            )",
            [],
        ).map_err(|e| format!("[DATABASE] CMD_HISTORY_TABLE_FAILED: {}", e))?;
        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_cmd_history_ts ON cmd_history(ts DESC)",
            [],
        ).map_err(|e| format!("[DATABASE] CMD_HISTORY_INDEX_FAILED: {}", e))?;
        const SCHEMA_VERSION: i64 = 6;
        let stored: i64 = conn.query_row(
            "SELECT CAST(value AS INTEGER) FROM schema_meta WHERE key = 'schema_version'",
            [],
            |row| row.get(0),
        ).unwrap_or(0);
        if stored > SCHEMA_VERSION {
            return Err(format!(
                "[DATABASE] SCHEMA_AHEAD_OF_BINARY: vault was written by a newer build (schema v{}), this binary only understands v{}. Upgrade the app before opening this profile.",
                stored, SCHEMA_VERSION,
            ));
        }
        conn.execute(
            "INSERT INTO schema_meta (key, value) VALUES ('schema_version', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            rusqlite::params![SCHEMA_VERSION.to_string()],
        ).map_err(|e| format!("[DATABASE] META_WRITE_FAILED: {}", e))?;

        // Schema migrations for the per-node and per-folder colour bar. NULL
        // means "use the default ring" — the UI treats absence as the same
        // visual as before this column existed.
        for stmt in [
            "ALTER TABLE servers ADD COLUMN color TEXT",
            "ALTER TABLE folders ADD COLUMN color TEXT",
            // v4: per-node free-form description / runbook. Defaults to empty
            // so existing rows don't need backfill. NOT NULL keeps the read
            // path branchless.
            "ALTER TABLE servers ADD COLUMN notes TEXT NOT NULL DEFAULT ''",
            // Commands auto-typed into the FIRST terminal on the INITIAL
            // connect (never on reconnect / extra shells). Empty = nothing.
            "ALTER TABLE servers ADD COLUMN run_on_connect TEXT NOT NULL DEFAULT ''",
            // ProxyJump: optional id of another server to bounce through. NULL
            // = connect directly. Nullable + additive so old binaries ignore
            // it (no SCHEMA_VERSION bump, matching run_on_connect above).
            "ALTER TABLE servers ADD COLUMN jump_host_id INTEGER",
            // Per-algorithm host-key tracking. Legacy rows keep key_type NULL
            // (treated conservatively as "same type" so a real key rotation is
            // never downgraded to a benign first-time prompt); rows recorded
            // after this migration store the host-key algorithm so a server
            // ADDING a new algorithm no longer looks like a MITM key change.
            "ALTER TABLE known_hosts ADD COLUMN key_type TEXT",
            // Per-entity sync columns (schema v6). Nullable uuid/updated_at get
            // backfilled row-by-row just below; deleted is a plain constant
            // default so it's a safe single-statement ADD.
            "ALTER TABLE folders ADD COLUMN uuid TEXT",
            "ALTER TABLE folders ADD COLUMN updated_at TEXT",
            "ALTER TABLE folders ADD COLUMN deleted INTEGER NOT NULL DEFAULT 0",
            "ALTER TABLE ssh_keys ADD COLUMN uuid TEXT",
            "ALTER TABLE ssh_keys ADD COLUMN updated_at TEXT",
            "ALTER TABLE ssh_keys ADD COLUMN deleted INTEGER NOT NULL DEFAULT 0",
            "ALTER TABLE credentials ADD COLUMN uuid TEXT",
            "ALTER TABLE credentials ADD COLUMN updated_at TEXT",
            "ALTER TABLE credentials ADD COLUMN deleted INTEGER NOT NULL DEFAULT 0",
            "ALTER TABLE servers ADD COLUMN uuid TEXT",
            "ALTER TABLE servers ADD COLUMN updated_at TEXT",
            "ALTER TABLE servers ADD COLUMN deleted INTEGER NOT NULL DEFAULT 0",
            "ALTER TABLE commands ADD COLUMN uuid TEXT",
            "ALTER TABLE commands ADD COLUMN updated_at TEXT",
            "ALTER TABLE commands ADD COLUMN deleted INTEGER NOT NULL DEFAULT 0",
            "ALTER TABLE notes ADD COLUMN uuid TEXT",
            "ALTER TABLE notes ADD COLUMN updated_at TEXT",
            "ALTER TABLE notes ADD COLUMN deleted INTEGER NOT NULL DEFAULT 0",
            "ALTER TABLE monitor_configs ADD COLUMN uuid TEXT",
            "ALTER TABLE monitor_configs ADD COLUMN updated_at TEXT",
            "ALTER TABLE monitor_configs ADD COLUMN deleted INTEGER NOT NULL DEFAULT 0",
            // Per-person attribution (schema v6+). The auto-stamp triggers set
            // this to the editor label at mutation time; NULL on legacy rows.
            "ALTER TABLE folders ADD COLUMN edited_by TEXT",
            "ALTER TABLE ssh_keys ADD COLUMN edited_by TEXT",
            "ALTER TABLE credentials ADD COLUMN edited_by TEXT",
            "ALTER TABLE servers ADD COLUMN edited_by TEXT",
            "ALTER TABLE commands ADD COLUMN edited_by TEXT",
            "ALTER TABLE notes ADD COLUMN edited_by TEXT",
            "ALTER TABLE monitor_configs ADD COLUMN edited_by TEXT",
        ] {
            if let Err(e) = conn.execute(stmt, []) {
                let s = e.to_string();
                if !s.contains("duplicate column name") {
                    return Err(format!("[DATABASE] COLUMN_MIGRATION_FAILED: {}", s));
                }
            }
        }
    } else {
        // Master-password strength floor — enforced ONLY at vault CREATION, not
        // on unlock (an existing vault with a short password must still open).
        // This password is the single cryptographic root protecting every
        // stored credential and private key, so a trivial one is a real risk.
        // Count Unicode scalar values, not bytes, so non-Latin passwords aren't
        // over-counted. 8 is a floor, not a ceiling — the UI should also nudge.
        if password.chars().count() < 8 {
            password.zeroize();
            return Err("[CRYPTO] WEAK_MASTER_PASSWORD: choose at least 8 characters — this password protects every saved credential.".into());
        }
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
            "CREATE TABLE folders (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT, parent_id INTEGER, color TEXT, uuid TEXT, updated_at TEXT, deleted INTEGER NOT NULL DEFAULT 0, edited_by TEXT);
             CREATE TABLE ssh_keys (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT, public_key TEXT, private_key TEXT, passphrase TEXT, uuid TEXT, updated_at TEXT, deleted INTEGER NOT NULL DEFAULT 0, edited_by TEXT);
             CREATE TABLE credentials (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT, auth_type TEXT, username TEXT, password TEXT, key_id INTEGER, uuid TEXT, updated_at TEXT, deleted INTEGER NOT NULL DEFAULT 0, edited_by TEXT, FOREIGN KEY(key_id) REFERENCES ssh_keys(id));
             CREATE TABLE servers (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT, host TEXT, port INTEGER, username TEXT, password TEXT, credential_id INTEGER, folder_id INTEGER, proxy_type TEXT DEFAULT 'none', proxy_host TEXT, proxy_port INTEGER, tunnels TEXT, auth_type TEXT DEFAULT 'vault', key_id INTEGER, autostart INTEGER NOT NULL DEFAULT 0, mirrors TEXT NOT NULL DEFAULT '[]', color TEXT, notes TEXT NOT NULL DEFAULT '', run_on_connect TEXT NOT NULL DEFAULT '', jump_host_id INTEGER, uuid TEXT, updated_at TEXT, deleted INTEGER NOT NULL DEFAULT 0, edited_by TEXT, FOREIGN KEY(folder_id) REFERENCES folders(id));
             CREATE TABLE commands (id INTEGER PRIMARY KEY AUTOINCREMENT, title TEXT, content TEXT, uuid TEXT, updated_at TEXT, deleted INTEGER NOT NULL DEFAULT 0, edited_by TEXT);
             CREATE TABLE notes (id INTEGER PRIMARY KEY AUTOINCREMENT, title TEXT, body TEXT, uuid TEXT, updated_at TEXT, deleted INTEGER NOT NULL DEFAULT 0, edited_by TEXT);
             CREATE TABLE known_hosts (id INTEGER PRIMARY KEY AUTOINCREMENT, host TEXT, port INTEGER, fingerprint TEXT, key_type TEXT);
             CREATE TABLE monitor_configs (node_id INTEGER PRIMARY KEY, enabled_metrics TEXT NOT NULL DEFAULT '[\"cpu\",\"mem\",\"disk\",\"load\"]', custom_metrics TEXT NOT NULL DEFAULT '[]', paused INTEGER NOT NULL DEFAULT 1, uuid TEXT, updated_at TEXT, deleted INTEGER NOT NULL DEFAULT 0, edited_by TEXT, FOREIGN KEY(node_id) REFERENCES servers(id) ON DELETE CASCADE);
             CREATE TABLE monitor_settings (id INTEGER PRIMARY KEY, json TEXT NOT NULL);
             CREATE TABLE schema_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE cmd_history (id INTEGER PRIMARY KEY AUTOINCREMENT, server_id INTEGER, server_name TEXT, command TEXT NOT NULL, ts INTEGER NOT NULL, exit_code INTEGER);
             CREATE INDEX idx_cmd_history_ts ON cmd_history(ts DESC);
             INSERT INTO schema_meta (key, value) VALUES ('schema_version', '6');"
        ).map_err(|e| format!("[DATABASE] SCHEMA_CREATION_FAILED: {}", e))?;
    }

    conn.execute("PRAGMA foreign_keys = ON", []).map_err(|e| format!("[DATABASE] PRAGMA_FAILED: {}", e))?;

    // ---- Per-entity sync instrumentation (schema v6) ----
    // A per-profile Hybrid Logical Clock backs the `hlc_now()` SQL function so
    // every row mutation auto-stamps `updated_at`. Seed the clock past the
    // newest stamp already in the vault so a freshly-started process never
    // issues one that sorts before data it already holds.
    let sync_node_id = sync_device_node_id(&app_handle);
    let seed_ms: u64 = conn
        .query_row(
            "SELECT COALESCE(MAX(updated_at),'') FROM (
               SELECT updated_at FROM servers UNION ALL SELECT updated_at FROM credentials
               UNION ALL SELECT updated_at FROM ssh_keys UNION ALL SELECT updated_at FROM folders
               UNION ALL SELECT updated_at FROM commands UNION ALL SELECT updated_at FROM notes
               UNION ALL SELECT updated_at FROM monitor_configs)",
            [],
            |r| r.get::<_, String>(0),
        )
        .ok()
        .map(|s| hlc::Hlc::phys_of(&s))
        .unwrap_or(0);
    let hlc_arc = std::sync::Arc::new(hlc::Hlc::new(sync_node_id, seed_ms));
    register_sync_functions(&conn, &hlc_arc)?;
    // Backfill rows that predate the sync columns (no-op on a fresh vault, and
    // a no-op on every open after the first). Run it BEFORE creating triggers so
    // the backfill UPDATEs can't fire them.
    if backfill_sync_columns(&conn, &hlc_arc)? {
        needs_resave = true;
    }
    create_sync_triggers(&conn)?;

    // Per-profile sync metadata (the DEK, and later the identity/grant state).
    // Created here so an existing vault gains it on first open under a v6 build.
    conn.execute(
        "CREATE TABLE IF NOT EXISTS sync_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)",
        [],
    )
    .map_err(|e| format!("[SHARE] SYNC_META_TABLE: {e}"))?;
    // Mint the profile's Data Encryption Key eagerly and persist it, so it can
    // never drift from blobs already pushed (which would happen if a sync
    // created it in-memory but crashed before saving). No-op after the first.
    if get_or_create_dek(&conn)?.1 {
        needs_resave = true;
    }

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
    let mut hlc_guard = state.hlc.lock().map_err(|_| "[STATE] LOCK_FAILED_HLC")?;
    *conn_guard = Some(conn);
    *key_guard = Some(key);
    *salt_guard = Some(salt_bytes);
    *path_guard = Some(path);
    *hlc_guard = Some(hlc_arc);
    drop(hlc_guard);
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
    save_vault_async(&state).await
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
    setup_master_db(app_handle.clone(), password, state).await?;

    // New profile: mint a stable UUID and partition its cloud sync by that UUID
    // rather than its name. Two vaults that happen to share a display name then
    // land in SEPARATE server partitions with separate keys — no cross-decrypt
    // failures, no silently merged records. Legacy `main` predates this and
    // keeps its name partition (untouched); only profiles born here get a UUID.
    // The human-readable name still reaches the server via sync_now's `name`.
    let db_state = app_handle.state::<DbState>();
    {
        let conn_g = db_state.conn.lock().map_err(|_| "[STATE] LOCK_CONN")?;
        let conn = conn_g.as_ref().ok_or("[STATE] DB_NOT_OPEN")?;
        let mut pid = [0u8; 16];
        rand::thread_rng().fill(&mut pid);
        let pid_hex = hex::encode(pid); // 32 hex chars — fits the server's 32-char partition column
        // DO NOTHING (never overwrite): a fresh vault has neither key, but this
        // must never repartition a profile if it somehow re-runs.
        conn.execute(
            "INSERT INTO sync_meta(key,value) VALUES('profile_id',?1) ON CONFLICT(key) DO NOTHING",
            [&pid_hex],
        )
        .map_err(|e| format!("[STATE] PROFILE_ID_STORE: {e}"))?;
        conn.execute(
            "INSERT INTO sync_meta(key,value) VALUES('cloud_profile',?1) ON CONFLICT(key) DO NOTHING",
            [&pid_hex],
        )
        .map_err(|e| format!("[STATE] CLOUD_PROFILE_STORE: {e}"))?;
    }
    save_vault_internal(&db_state)?;
    Ok(())
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
    color: Option<String>,
) -> Result<i64, String> {
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
        "INSERT INTO servers (name, host, port, username, password, credential_id, folder_id, proxy_type, proxy_host, proxy_port, tunnels, auth_type, key_id, autostart, mirrors, color) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
        rusqlite::params![name, host, port, db_username, db_password, db_credential_id, folder_id, proxy_type, proxy_host, proxy_port, tunnels_json, auth_type, db_key_id, autostart_i, mirrors_json, color],
    ).map_err(|e| format!("[DATABASE] SERVER_INSERT_FAILED: SQL_ERROR={}", e))?;

    if res == 0 {
        return Err("[DATABASE] SERVER_INSERT_FAILED: No rows affected".into());
    }

    let new_id = conn.last_insert_rowid();
    drop(conn_guard);
    save_vault_internal(&state)?;
    Ok(new_id)
}

#[tauri::command]
async fn edit_server(
    state: tauri::State<'_, DbState>,
    ssh_state: tauri::State<'_, SshState>,
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
    color: Option<String>,
    // Frontend opt-out for password persistence. When the user opens the
    // edit sheet, we call `reveal_server_password` to populate the form;
    // if that call ever fails (transient DB error / migration mid-flight)
    // the form would have a blank password and a naive save would wipe
    // the stored secret. The frontend sends `preserve_password=true`
    // whenever the password field wasn't touched, and the SQL below uses
    // COALESCE(?, password) so the existing column survives.
    preserve_password: Option<bool>,
) -> Result<(), String> {
    let tunnels_json = serde_json::to_string(&tunnels).unwrap_or_else(|_| "[]".to_string());
    let mirrors_json = mirrors.as_ref()
        .map(|m| serde_json::to_string(m).unwrap_or_else(|_| "[]".into()))
        .unwrap_or_else(|| "[]".into());

    let (db_username, db_password, db_key_id, db_credential_id) = normalize_server_identity(
        &auth_type, username, password, key_id, credential_id,
    );
    let autostart_i: i32 = if autostart.unwrap_or(false) { 1 } else { 0 };

    // All SQLite work is scoped so the non-Send connection guard is fully
    // dropped before the async cache-invalidation await below — Tauri requires
    // the command future to be Send. Returns whether the forwarding rules
    // changed: if they did, the node's saved rules become the new source of
    // truth and the live session's cached replay list must be dropped (below).
    // Otherwise a session that already seeded its specs, or had them stripped
    // to an empty list by a failed bind, keeps using the stale set and never
    // re-reads the edited rules — the "changed the port but the forward never
    // comes up" bug.
    let tunnels_changed = {
        let conn_guard = state.conn.lock().map_err(|_| "[STATE] LOCK_FAILED")?;
        let conn = conn_guard.as_ref().ok_or("[STATE] DATABASE_NOT_INITIALIZED")?;

        let old_tunnels_json: String = conn
            .query_row("SELECT COALESCE(tunnels,'[]') FROM servers WHERE id=?1", [id], |r| r.get(0))
            .unwrap_or_else(|_| "[]".to_string());

        // Two-flavour UPDATE: with `preserve_password`, the password column is
        // wrapped in COALESCE(?, password) so a NULL bind keeps the existing
        // value. Without it, the password column is overwritten the usual way.
        // The behaviour difference matters only when the auth path is custom_pass
        // because normalize_server_identity zeros password for the other modes.
        if preserve_password.unwrap_or(false) && auth_type == "custom_pass" {
            conn.execute(
                "UPDATE servers SET name=?1, host=?2, port=?3, username=?4, password=COALESCE(?5, password), credential_id=?6, folder_id=?7, proxy_type=?8, proxy_host=?9, proxy_port=?10, tunnels=?11, auth_type=?12, key_id=?13, autostart=?14, mirrors=?15, color=?16 WHERE id=?17",
                rusqlite::params![name, host, port, db_username, db_password, db_credential_id, folder_id, proxy_type, proxy_host, proxy_port, tunnels_json, auth_type, db_key_id, autostart_i, mirrors_json, color, id],
            ).map_err(|e| format!("[DATABASE] SERVER_UPDATE_FAILED: SQL_ERROR={}", e))?;
        } else {
            conn.execute(
                "UPDATE servers SET name=?1, host=?2, port=?3, username=?4, password=?5, credential_id=?6, folder_id=?7, proxy_type=?8, proxy_host=?9, proxy_port=?10, tunnels=?11, auth_type=?12, key_id=?13, autostart=?14, mirrors=?15, color=?16 WHERE id=?17",
                rusqlite::params![name, host, port, db_username, db_password, db_credential_id, folder_id, proxy_type, proxy_host, proxy_port, tunnels_json, auth_type, db_key_id, autostart_i, mirrors_json, color, id],
            ).map_err(|e| format!("[DATABASE] SERVER_UPDATE_FAILED: SQL_ERROR={}", e))?;
        }

        old_tunnels_json != tunnels_json
    };

    save_vault_internal(&state)?;

    // Forwarding rules changed → drop this server's live tunnel replay cache so
    // every (re)connect / restore path re-seeds from the freshly-saved DB rules
    // instead of clinging to a stale (or failure-emptied) in-memory list. All
    // those paths seed from the node's `tunnels` JSON precisely when the key is
    // ABSENT, so removing it is what lets the edit take effect. Session ids are
    // `session-{server_id}` (see openServer in the frontend).
    if tunnels_changed {
        ssh_state
            .session_tunnel_specs
            .lock()
            .await
            .remove(&format!("session-{}", id));
    }
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

/// Reveal the plaintext password for a single server row. Used by the edit
/// panel — only the row the user is editing has its secret crossing IPC.
/// Returns `Ok(None)` if the server uses a vault credential (no inline
/// password) or the password column is empty.
#[tauri::command]
async fn reveal_server_password(state: tauri::State<'_, DbState>, id: i32) -> Result<Option<String>, String> {
    let conn_guard = state.conn.lock().map_err(|_| "[STATE] LOCK_FAILED")?;
    let conn = conn_guard.as_ref().ok_or("[STATE] DATABASE_NOT_INITIALIZED")?;
    conn.query_row(
        "SELECT password FROM servers WHERE id = ?1",
        rusqlite::params![id],
        |row| row.get::<_, Option<String>>(0),
    ).map_err(|e| format!("[DATABASE] REVEAL_FAILED: {}", e))
}

/// Reveal the plaintext password for a single saved credential. Same shape
/// and rationale as `reveal_server_password`.
#[tauri::command]
async fn reveal_credential_password(state: tauri::State<'_, DbState>, id: i32) -> Result<Option<String>, String> {
    let conn_guard = state.conn.lock().map_err(|_| "[STATE] LOCK_FAILED")?;
    let conn = conn_guard.as_ref().ok_or("[STATE] DATABASE_NOT_INITIALIZED")?;
    conn.query_row(
        "SELECT password FROM credentials WHERE id = ?1",
        rusqlite::params![id],
        |row| row.get::<_, Option<String>>(0),
    ).map_err(|e| format!("[DATABASE] REVEAL_FAILED: {}", e))
}

/// Reveal the stored private key + passphrase for an SSH key entry. Used
/// by the key editor and by any future "show key" affordance. The list
/// view never sees these fields.
#[tauri::command]
async fn reveal_ssh_key(state: tauri::State<'_, DbState>, id: i32) -> Result<serde_json::Value, String> {
    let conn_guard = state.conn.lock().map_err(|_| "[STATE] LOCK_FAILED")?;
    let conn = conn_guard.as_ref().ok_or("[STATE] DATABASE_NOT_INITIALIZED")?;
    conn.query_row(
        "SELECT private_key, passphrase FROM ssh_keys WHERE id = ?1",
        rusqlite::params![id],
        |row| {
            Ok(json!({
                "private_key": row.get::<_, Option<String>>(0)?,
                "passphrase": row.get::<_, Option<String>>(1)?,
            }))
        },
    ).map_err(|e| format!("[DATABASE] REVEAL_FAILED: {}", e))
}

/// Light-weight colour write — used by the NodeGrid swatch picker so the
/// caller doesn't have to round-trip the whole edit_server payload just to
/// change one tag. Pass `None` to clear back to the default ring.
#[tauri::command]
async fn set_server_color(state: tauri::State<'_, DbState>, id: i32, color: Option<String>) -> Result<(), String> {
    let conn_guard = state.conn.lock().map_err(|_| "[STATE] LOCK_FAILED")?;
    let conn = conn_guard.as_ref().ok_or("[STATE] DATABASE_NOT_INITIALIZED")?;
    conn.execute(
        "UPDATE servers SET color=?1 WHERE id=?2",
        rusqlite::params![color, id],
    ).map_err(|e| format!("[DATABASE] SERVER_COLOR_FAILED: {}", e))?;
    drop(conn_guard);
    save_vault_internal(&state)?;
    Ok(())
}

#[tauri::command]
async fn set_folder_color(state: tauri::State<'_, DbState>, id: i32, color: Option<String>) -> Result<(), String> {
    let conn_guard = state.conn.lock().map_err(|_| "[STATE] LOCK_FAILED")?;
    let conn = conn_guard.as_ref().ok_or("[STATE] DATABASE_NOT_INITIALIZED")?;
    conn.execute(
        "UPDATE folders SET color=?1 WHERE id=?2",
        rusqlite::params![color, id],
    ).map_err(|e| format!("[DATABASE] FOLDER_COLOR_FAILED: {}", e))?;
    drop(conn_guard);
    save_vault_internal(&state)?;
    Ok(())
}

/// Update the free-form notes / description blob attached to a node. Stored
/// as plain text in the `notes` column; the UI renders it monospace and lets
/// the user dump runbook snippets, contact info, alerts to remember, etc.
#[tauri::command]
async fn set_server_notes(state: tauri::State<'_, DbState>, id: i32, notes: String) -> Result<(), String> {
    let conn_guard = state.conn.lock().map_err(|_| "[STATE] LOCK_FAILED")?;
    let conn = conn_guard.as_ref().ok_or("[STATE] DATABASE_NOT_INITIALIZED")?;
    conn.execute(
        "UPDATE servers SET notes=?1 WHERE id=?2",
        rusqlite::params![notes, id],
    ).map_err(|e| format!("[DATABASE] SERVER_NOTES_FAILED: {}", e))?;
    drop(conn_guard);
    save_vault_internal(&state)?;
    Ok(())
}

/// Per-server commands auto-typed into the first terminal on initial connect.
/// Stored as a single blob (newline-separated lines); the frontend sends them
/// only for `-term-0` and only on the first open, never on reconnect. Written
/// through its own command (like set_server_notes) so it doesn't have to be
/// threaded through add_server / edit_server's positional parameter lists.
#[tauri::command]
async fn set_server_run_on_connect(state: tauri::State<'_, DbState>, id: i32, value: String) -> Result<(), String> {
    let conn_guard = state.conn.lock().map_err(|_| "[STATE] LOCK_FAILED")?;
    let conn = conn_guard.as_ref().ok_or("[STATE] DATABASE_NOT_INITIALIZED")?;
    conn.execute(
        "UPDATE servers SET run_on_connect=?1 WHERE id=?2",
        rusqlite::params![value, id],
    ).map_err(|e| format!("[DATABASE] SERVER_RUN_ON_CONNECT_FAILED: {}", e))?;
    drop(conn_guard);
    save_vault_internal(&state)?;
    Ok(())
}

/// ProxyJump target for a node: `Some(other_server_id)` to bounce through that
/// server, or `None` to connect directly. Written through its own command
/// (like set_server_notes / set_server_run_on_connect) so it stays out of
/// add_server / edit_server's positional parameter lists.
#[tauri::command]
async fn set_server_jump_host(state: tauri::State<'_, DbState>, id: i32, value: Option<i32>) -> Result<(), String> {
    let conn_guard = state.conn.lock().map_err(|_| "[STATE] LOCK_FAILED")?;
    let conn = conn_guard.as_ref().ok_or("[STATE] DATABASE_NOT_INITIALIZED")?;
    conn.execute(
        "UPDATE servers SET jump_host_id=?1 WHERE id=?2",
        rusqlite::params![value, id],
    ).map_err(|e| format!("[DATABASE] SERVER_JUMP_HOST_FAILED: {}", e))?;
    drop(conn_guard);
    save_vault_internal(&state)?;
    Ok(())
}

/// Duplicate a server row verbatim — including credentials linkage, tunnels,
/// mirrors, proxy config, colour. The clone gets a "{name} (copy)" suffix so
/// it shows up beside the original in the grid; everything else is identical
/// so the user can connect to the same target with a different label or
/// tweak one field without retyping the rest.
#[tauri::command]
async fn clone_server(state: tauri::State<'_, DbState>, id: i32) -> Result<i64, String> {
    let conn_guard = state.conn.lock().map_err(|_| "[STATE] LOCK_FAILED")?;
    let conn = conn_guard.as_ref().ok_or("[STATE] DATABASE_NOT_INITIALIZED")?;
    let affected = conn.execute(
        "INSERT INTO servers (name, host, port, username, password, credential_id, folder_id, proxy_type, proxy_host, proxy_port, tunnels, auth_type, key_id, autostart, mirrors, color, notes, run_on_connect, jump_host_id)
         SELECT name || ' (copy)', host, port, username, password, credential_id, folder_id, proxy_type, proxy_host, proxy_port, tunnels, auth_type, key_id, 0, mirrors, color, notes, run_on_connect, jump_host_id
         FROM servers WHERE id = ?1",
        rusqlite::params![id],
    ).map_err(|e| format!("[DATABASE] SERVER_CLONE_FAILED: {}", e))?;
    if affected == 0 {
        return Err(format!("[DATABASE] SERVER_CLONE_FAILED: no row with id {}", id));
    }
    let new_id = conn.last_insert_rowid();
    drop(conn_guard);
    save_vault_internal(&state)?;
    Ok(new_id)
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
    // Secrets stay server-side: this listing returns `has_password` instead
    // of the plaintext column, so the password never crosses the IPC bridge
    // unless someone explicitly invokes `reveal_server_password`. The edit
    // panel calls reveal on open, but the cards / sidebar / quick-connect
    // grid never see the plaintext.
    let mut stmt = conn.prepare("SELECT id, name, host, port, username, password, credential_id, folder_id, proxy_type, proxy_host, proxy_port, tunnels, auth_type, key_id, autostart, mirrors, color, notes, run_on_connect, jump_host_id, updated_at, edited_by FROM servers")
        .map_err(|e| format!("[DATABASE] PREPARE_FAILED: {}", e))?;

    let rows = stmt.query_map([], |row| {
        let pw: Option<String> = row.get::<_, Option<String>>(5)?;
        Ok(json!({
            "id": row.get::<_, i32>(0)?,
            "name": row.get::<_, String>(1)?,
            "host": row.get::<_, String>(2)?,
            "port": row.get::<_, i32>(3)?,
            "username": row.get::<_, Option<String>>(4)?.unwrap_or_default(),
            "has_password": pw.as_deref().map(|s| !s.is_empty()).unwrap_or(false),
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
            "color": row.get::<_, Option<String>>(16)?,
            "notes": row.get::<_, Option<String>>(17)?.unwrap_or_default(),
            "run_on_connect": row.get::<_, Option<String>>(18)?.unwrap_or_default(),
            "jump_host_id": row.get::<_, Option<i32>>(19)?,
            // Attribution: HLC stamp (encodes last-edit time) + who last edited.
            "updated_at": row.get::<_, Option<String>>(20)?,
            "edited_by": row.get::<_, Option<String>>(21)?,
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
    // Private key + passphrase do NOT cross IPC in the listing — only the
    // public key (which is safe by definition) and presence flags. The edit
    // sheet calls `reveal_ssh_key` when it needs the secrets to display.
    let mut stmt = conn.prepare("SELECT id, name, public_key, private_key, passphrase FROM ssh_keys").map_err(|e| e.to_string())?;
    let rows = stmt.query_map([], |row| {
        let priv_present: Option<String> = row.get::<_, Option<String>>(3)?;
        let pp_present: Option<String> = row.get::<_, Option<String>>(4)?;
        Ok(json!({
            "id": row.get::<_, i32>(0)?,
            "name": row.get::<_, String>(1)?,
            "public_key": row.get::<_, String>(2)?,
            "has_private_key": priv_present.as_deref().map(|s| !s.is_empty()).unwrap_or(false),
            "has_passphrase": pp_present.as_deref().map(|s| !s.is_empty()).unwrap_or(false),
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
    // Same pattern as get_servers / get_ssh_keys: only `has_password` is
    // listed. The credential edit panel reveals on open via the dedicated
    // command.
    let mut stmt = conn.prepare("SELECT id, name, auth_type, username, password, key_id FROM credentials").map_err(|e| e.to_string())?;
    let rows = stmt.query_map([], |row| {
        let pw: Option<String> = row.get::<_, Option<String>>(4)?;
        Ok(json!({
            "id": row.get::<_, i32>(0)?,
            "name": row.get::<_, String>(1)?,
            "auth_type": row.get::<_, String>(2)?,
            "username": row.get::<_, String>(3)?,
            "has_password": pw.as_deref().map(|s| !s.is_empty()).unwrap_or(false),
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
    let mut stmt = conn.prepare("SELECT id, name, parent_id, color FROM folders").map_err(|e| e.to_string())?;

    let rows = stmt.query_map([], |row| {
        Ok(json!({
            "id": row.get::<_, i32>(0)?,
            "name": row.get::<_, String>(1)?,
            "parent_id": row.get::<_, Option<i32>>(2)?,
            "color": row.get::<_, Option<String>>(3)?,
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

// ───────────────────────── Command History ─────────────────────────
// Ctrl+R-style rolling log of commands the user typed into any terminal.
// Populated best-effort by TerminalView (buffered keystrokes → Enter →
// insert). Read by HistorySearchOverlay through the three commands below.
// Capped at 1000 rows — dropping the oldest — so a chatty user can't blow
// the vault size up over months of use.

const CMD_HISTORY_CAP: i64 = 1000;

#[tauri::command]
async fn cmd_history_add(
    state: tauri::State<'_, DbState>,
    server_id: Option<i32>,
    server_name: String,
    command: String,
) -> Result<i64, String> {
    let trimmed = command.trim().to_string();
    if trimmed.is_empty() {
        return Err("[HISTORY] EMPTY_COMMAND".into());
    }
    let ts: i64 = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);

    let conn_guard = state.conn.lock().map_err(|_| "[STATE] LOCK_FAILED")?;
    let conn = conn_guard.as_ref().ok_or("[STATE] DATABASE_NOT_INITIALIZED")?;

    // De-dupe against the immediately-previous row for the same server so a
    // user re-running `ls` five times in a row doesn't fill five slots. Match
    // by (server_id, command); leave older reruns alone so search still finds
    // "ran this yesterday morning too".
    let last: Option<(i64, String, Option<i32>)> = conn.query_row(
        "SELECT id, command, server_id FROM cmd_history ORDER BY ts DESC LIMIT 1",
        [],
        |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?, row.get::<_, Option<i32>>(2)?)),
    ).ok();
    if let Some((id, prev_cmd, prev_sid)) = last {
        if prev_cmd == trimmed && prev_sid == server_id {
            // Refresh the timestamp on the existing row instead of writing a
            // duplicate — keeps the "most recent first" ordering honest.
            let _ = conn.execute(
                "UPDATE cmd_history SET ts = ?1 WHERE id = ?2",
                rusqlite::params![ts, id],
            );
            return Ok(id);
        }
    }

    conn.execute(
        "INSERT INTO cmd_history (server_id, server_name, command, ts) VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![server_id, server_name, trimmed, ts],
    ).map_err(|e| format!("[DATABASE] CMD_HISTORY_INSERT_FAILED: {}", e))?;

    let new_id = conn.last_insert_rowid();

    // Prune the tail so the table stays bounded. LIMIT/OFFSET in a subquery
    // gives us "keep the newest N, drop everything older".
    let _ = conn.execute(
        "DELETE FROM cmd_history WHERE id IN (
            SELECT id FROM cmd_history ORDER BY ts DESC LIMIT -1 OFFSET ?1
        )",
        rusqlite::params![CMD_HISTORY_CAP],
    );

    // History isn't secret but the encrypted vault is the only place it can
    // land; we DO NOT call save_vault_internal on every keystroke — that'd
    // fsync per command and thrash disk. The next save_vault_* call (any
    // node edit, key add, etc., or profile-close teardown) picks it up.
    Ok(new_id)
}

#[tauri::command]
async fn cmd_history_list(
    state: tauri::State<'_, DbState>,
    query: Option<String>,
    limit: Option<u32>,
) -> Result<Vec<serde_json::Value>, String> {
    let conn_guard = state.conn.lock().map_err(|_| "[STATE] LOCK_FAILED")?;
    let conn = conn_guard.as_ref().ok_or("[STATE] DATABASE_NOT_INITIALIZED")?;

    let lim = limit.unwrap_or(200).min(1000) as i64;
    let q = query.unwrap_or_default();
    let q_trimmed = q.trim();

    let rows: Vec<serde_json::Value> = if q_trimmed.is_empty() {
        let mut stmt = conn.prepare(
            "SELECT id, server_id, server_name, command, ts, exit_code
             FROM cmd_history
             ORDER BY ts DESC LIMIT ?1"
        ).map_err(|e| format!("[DATABASE] CMD_HISTORY_PREP_FAILED: {}", e))?;
        let mapped = stmt.query_map(rusqlite::params![lim], |row| {
            Ok(json!({
                "id": row.get::<_, i64>(0)?,
                "server_id": row.get::<_, Option<i32>>(1)?,
                "server_name": row.get::<_, Option<String>>(2)?,
                "command": row.get::<_, String>(3)?,
                "ts": row.get::<_, i64>(4)?,
                "exit_code": row.get::<_, Option<i32>>(5)?,
            }))
        }).map_err(|e| format!("[DATABASE] CMD_HISTORY_QUERY_FAILED: {}", e))?;
        let mut out = Vec::new();
        for r in mapped {
            out.push(r.map_err(|e| format!("[DATABASE] CMD_HISTORY_ROW_FAILED: {}", e))?);
        }
        out
    } else {
        // Escape LIKE metacharacters (%, _, backslash) so the user's raw
        // input matches literally. Uses `\` as the escape character.
        let escaped = q_trimmed
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        let like = format!("%{}%", escaped);
        let mut stmt = conn.prepare(
            "SELECT id, server_id, server_name, command, ts, exit_code
             FROM cmd_history
             WHERE command LIKE ?1 ESCAPE '\\'
             ORDER BY ts DESC LIMIT ?2"
        ).map_err(|e| format!("[DATABASE] CMD_HISTORY_PREP_FAILED: {}", e))?;
        let mapped = stmt.query_map(rusqlite::params![like, lim], |row| {
            Ok(json!({
                "id": row.get::<_, i64>(0)?,
                "server_id": row.get::<_, Option<i32>>(1)?,
                "server_name": row.get::<_, Option<String>>(2)?,
                "command": row.get::<_, String>(3)?,
                "ts": row.get::<_, i64>(4)?,
                "exit_code": row.get::<_, Option<i32>>(5)?,
            }))
        }).map_err(|e| format!("[DATABASE] CMD_HISTORY_QUERY_FAILED: {}", e))?;
        let mut out = Vec::new();
        for r in mapped {
            out.push(r.map_err(|e| format!("[DATABASE] CMD_HISTORY_ROW_FAILED: {}", e))?);
        }
        out
    };
    Ok(rows)
}

#[tauri::command]
async fn cmd_history_clear(state: tauri::State<'_, DbState>) -> Result<(), String> {
    let conn_guard = state.conn.lock().map_err(|_| "[STATE] LOCK_FAILED")?;
    let conn = conn_guard.as_ref().ok_or("[STATE] DATABASE_NOT_INITIALIZED")?;
    conn.execute("DELETE FROM cmd_history", [])
        .map_err(|e| format!("[DATABASE] CMD_HISTORY_CLEAR_FAILED: {}", e))?;
    drop(conn_guard);
    save_vault_internal(&state)?;
    Ok(())
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

/// Drive keyboard-interactive (RFC 4256) authentication — the method behind
/// most SSH "verification code" / 2FA / OTP setups. The server sends a
/// sequence of `InfoRequest`s (each a set of prompts like "Verification
/// code:"); we relay every prompt to the UI via `kbi-prompt-{session_id}`,
/// wait for the user's answers on a per-nonce oneshot, and send them back.
///
/// Return contract:
///   * `Some(Ok(true))`  — authenticated.
///   * `Some(Ok(false))` — the user answered but the server rejected, OR the
///                         user cancelled / timed out.
///   * `Some(Err(e))`    — transport error talking to the server.
///   * `None`            — the server never actually prompted us (it doesn't
///                         offer keyboard-interactive). The caller keeps its
///                         original, more meaningful auth result instead of
///                         masking it with a generic interactive failure.
///
/// Generic over the handler so both the primary connection (`ClientHandler`)
/// and a ProxyJump intermediate hop can reuse it.
async fn run_keyboard_interactive<H: russh::client::Handler>(
    session: &mut russh::client::Handle<H>,
    user: &str,
    app: &tauri::AppHandle,
    session_id: &str,
    nonce: &str,
    kbi_txs: &std::sync::Arc<
        tokio::sync::Mutex<
            std::collections::HashMap<String, tokio::sync::oneshot::Sender<Option<Vec<String>>>>,
        >,
    >,
) -> Option<Result<bool, russh::Error>> {
    use russh::client::KeyboardInteractiveAuthResponse;
    use tauri::Emitter;

    let log = |msg: &str, ty: &str| {
        println!("[LOG-{}] {}", session_id, msg);
        let _ = app.emit(
            &format!("session-log-{}", session_id),
            serde_json::json!({"msg": msg, "type": ty}),
        );
    };

    log("Attempting Keyboard-Interactive (verification code) authentication...", "info");

    let mut resp = match session
        .authenticate_keyboard_interactive_start(user.to_string(), None)
        .await
    {
        Ok(r) => r,
        Err(e) => return Some(Err(e)),
    };

    // Guards against masking the primary error: only report a definitive
    // result once the server has actually asked us something.
    let mut asked_anything = false;

    loop {
        match resp {
            KeyboardInteractiveAuthResponse::Success => return Some(Ok(true)),
            KeyboardInteractiveAuthResponse::Failure => {
                if !asked_anything {
                    // Server declined the method outright — not really offered.
                    return None;
                }
                log("Verification code rejected by server.", "error");
                return Some(Ok(false));
            }
            KeyboardInteractiveAuthResponse::InfoRequest {
                name,
                instructions,
                prompts,
            } => {
                // A banner-only InfoRequest (zero prompts) needs an empty
                // response set to advance — nothing to ask the user.
                if prompts.is_empty() {
                    resp = match session
                        .authenticate_keyboard_interactive_respond(Vec::new())
                        .await
                    {
                        Ok(r) => r,
                        Err(e) => return Some(Err(e)),
                    };
                    continue;
                }
                asked_anything = true;

                // Fresh oneshot each round — a server may issue several
                // sequential InfoRequests within one auth exchange.
                let (tx, rx) = tokio::sync::oneshot::channel::<Option<Vec<String>>>();
                kbi_txs.lock().await.insert(nonce.to_string(), tx);

                let prompt_payload: Vec<serde_json::Value> = prompts
                    .iter()
                    .map(|p| serde_json::json!({ "prompt": p.prompt, "echo": p.echo }))
                    .collect();
                let _ = app.emit(
                    &format!("kbi-prompt-{}", session_id),
                    serde_json::json!({
                        "nonce": nonce,
                        "name": name,
                        "instructions": instructions,
                        "prompts": prompt_payload,
                    }),
                );

                // 120s: 2FA codes often mean reaching for a phone / authenticator.
                let answers = match tokio::time::timeout(
                    std::time::Duration::from_secs(120),
                    rx,
                )
                .await
                {
                    Ok(Ok(Some(a))) => a,
                    _ => {
                        // Timed out, cancelled, or the channel was dropped.
                        kbi_txs.lock().await.remove(nonce);
                        let _ = app.emit(
                            &format!("kbi-prompt-dismiss-{}", session_id),
                            serde_json::json!({}),
                        );
                        log("Verification prompt cancelled or timed out.", "error");
                        return Some(Ok(false));
                    }
                };
                let _ = app.emit(
                    &format!("kbi-prompt-dismiss-{}", session_id),
                    serde_json::json!({}),
                );

                // The protocol requires exactly one response per prompt.
                // Pad/truncate so a UI/prompt-count mismatch can never desync
                // the SSH auth state machine.
                let mut answers = answers;
                answers.resize(prompts.len(), String::new());

                resp = match session
                    .authenticate_keyboard_interactive_respond(answers)
                    .await
                {
                    Ok(r) => r,
                    Err(e) => return Some(Err(e)),
                };
            }
        }
    }
}

/// OS-level TCP keepalive on an SSH transport socket. Complements the SSH
/// protocol keepalive (`keepalive_interval` below): russh 0.40 has no
/// `keepalive_max`, so an unanswered protocol keepalive never tears the
/// connection down — but with TCP keepalive the kernel itself probes an idle
/// peer (30s idle, then every 10s) and errors the socket when the peer is
/// truly gone, which russh's run loop surfaces as `is_closed()`. That gives
/// dead-link detection during long-idle periods without any SSH traffic, and
/// keeps NAT/firewall mappings warm on flaky consumer networks. Best-effort:
/// failure to set it is never a reason to abort a connect. (`with_retries`
/// is deliberately not used — socket2 doesn't support it on Windows.)
fn apply_tcp_keepalive(stream: &tokio::net::TcpStream) {
    use socket2::{SockRef, TcpKeepalive};
    let ka = TcpKeepalive::new()
        .with_time(std::time::Duration::from_secs(30))
        .with_interval(std::time::Duration::from_secs(10));
    let _ = SockRef::from(stream).set_tcp_keepalive(&ka);
}

/// The russh client config shared by the primary connection and any ProxyJump
/// hop, so both negotiate an identical algorithm set. Extracted verbatim from
/// the inline block `initiate_connection` used to carry.
fn build_ssh_client_config() -> russh::client::Config {
    use tokio::time::Duration;
    let mut config = russh::client::Config::default();
    // SSH keepalive every 20s. The shorter interval matters because most
    // consumer routers drop idle NAT mappings around the 2-minute mark, and
    // many corporate firewalls are stricter still. russh 0.40 has no
    // `keepalive_max` knob, so the watcher loop relies on `is_closed()` to
    // surface drops within a couple of seconds.
    config.keepalive_interval = Some(Duration::from_secs(20));
    // Bigger receive window + max-allowed packet size: lets SFTP/tunnel
    // streams keep the BDP full on high-latency links.
    config.window_size = 8 * 1024 * 1024;
    config.maximum_packet_size = 65535;
    // Widen the negotiation set to match OpenSSH — but only in builds that
    // include the OpenSSL backend (release CI via `full-ssh-algos`). See the
    // long rationale that used to sit inline: legacy DH groups, the full RSA
    // host-key family, and HMAC-SHA1 MAC variants for older/embedded servers.
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
            compression: &["zlib@openssh.com", "zlib", "none"],
        };
    }
    config
}

/// Minimal auth/identity resolver for a ProxyJump bastion. Mirrors the
/// vault-vs-custom / credential-join / key-loading rules of the main connect
/// path but returns only what a jump hop needs (host, port, user, password,
/// optional key). Deliberately kept separate from `initiate_connection`'s
/// inline resolver so the primary connection path is byte-for-byte untouched.
/// It never reads `jump_host_id`, which makes ProxyJump strictly single-hop —
/// there is no way for it to recurse.
///
/// Runs fully synchronously (no `.await`) so the non-`Send` rusqlite
/// `Connection` never has to be held across a suspension point.
fn resolve_jump_auth(
    conn: &rusqlite::Connection,
    server_id: i32,
) -> Result<
    Option<(String, i32, String, Option<String>, Option<(String, Option<String>)>)>,
    String,
> {
    let mut stmt = conn
        .prepare(
            "
            SELECT s.host, s.port,
                   s.username as s_user, c.username as c_user,
                   s.password as s_pass, c.password as c_pass,
                   s.key_id   as s_key,  c.key_id   as c_key,
                   s.auth_type, c.auth_type as cred_auth_type
            FROM servers s
            LEFT JOIN credentials c ON s.credential_id = c.id
            WHERE s.id=?1
        ",
        )
        .map_err(|e| e.to_string())?;
    let mut rows = stmt.query([server_id]).map_err(|e| e.to_string())?;
    let row = match rows.next().map_err(|e| e.to_string())? {
        Some(r) => r,
        None => return Ok(None),
    };
    let host: String = row.get::<_, String>(0).map_err(|e| format!("[DB] jump host: {}", e))?;
    let port: i32 = row.get::<_, i32>(1).map_err(|e| format!("[DB] jump port: {}", e))?;
    let s_user: Option<String> = row.get::<_, Option<String>>(2).unwrap_or_default();
    let c_user: Option<String> = row.get::<_, Option<String>>(3).unwrap_or_default();
    let s_pass: Option<String> = row.get::<_, Option<String>>(4).unwrap_or_default();
    let c_pass: Option<String> = row.get::<_, Option<String>>(5).unwrap_or_default();
    let s_key: Option<i32> = row.get::<_, Option<i32>>(6).unwrap_or_default();
    let c_key: Option<i32> = row.get::<_, Option<i32>>(7).unwrap_or_default();
    let server_auth_type: String = row
        .get::<_, Option<String>>(8)
        .unwrap_or_default()
        .unwrap_or_else(|| "vault".to_string());
    let cred_auth_type: Option<String> = row.get::<_, Option<String>>(9).unwrap_or_default();

    let (username, password, key_id) = if server_auth_type == "vault" {
        (c_user.unwrap_or_default(), c_pass, c_key)
    } else {
        (s_user.unwrap_or_default(), s_pass, s_key)
    };
    let effective_key_id = if server_auth_type == "vault" {
        if cred_auth_type.as_deref() == Some("key") { key_id } else { None }
    } else if server_auth_type == "custom_key" {
        key_id
    } else {
        None
    };
    let key_data = if let Some(kid) = effective_key_id {
        let mut key_stmt = conn
            .prepare("SELECT private_key, passphrase FROM ssh_keys WHERE id = ?1")
            .map_err(|e| e.to_string())?;
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

    Ok(Some((host, port, username, password, key_data)))
}

/// Establish + authenticate an SSH connection to a ProxyJump intermediate
/// ("bastion") host and return its live `Handle`. The caller opens a
/// `direct-tcpip` channel over this handle to reach the real target, then keeps
/// the handle alive for the target session's lifetime (see
/// `SshState::jump_connections`).
///
/// The bastion is reached by a DIRECT TCP connection — its own proxy config, if
/// any, is not honored (bastions are normally directly reachable). Its host key
/// is verified through the SAME frontend prompt as the target (events emitted
/// under the target's `session_id`, keyed by a distinct nonce), and it supports
/// the full key → password → keyboard-interactive auth ladder.
async fn connect_jump_host(
    app: &tauri::AppHandle,
    db: &std::sync::Arc<std::sync::Mutex<Option<rusqlite::Connection>>>,
    fp_txs: &std::sync::Arc<
        tokio::sync::Mutex<std::collections::HashMap<String, tokio::sync::oneshot::Sender<bool>>>,
    >,
    kbi_txs: &std::sync::Arc<
        tokio::sync::Mutex<
            std::collections::HashMap<String, tokio::sync::oneshot::Sender<Option<Vec<String>>>>,
        >,
    >,
    session_id: &str,
    jump_server_id: i32,
) -> Result<russh::client::Handle<ssh_manager::ClientHandler>, String> {
    use tokio::time::Duration;
    use tauri::Emitter;

    let log = |msg: &str, ty: &str| {
        println!("[LOG-{}] [jump] {}", session_id, msg);
        let _ = app.emit(
            &format!("session-log-{}", session_id),
            serde_json::json!({"msg": msg, "type": ty}),
        );
    };

    // 1. Resolve the bastion's config synchronously and drop the DB guard
    //    before any await (rusqlite Connection is not Send).
    let (host, port, user, password, key_data) = {
        let guard = db.lock().map_err(|_| "[STATE] LOCK_FAILED".to_string())?;
        let conn = guard
            .as_ref()
            .ok_or_else(|| "[STATE] DATABASE_NOT_INITIALIZED".to_string())?;
        match resolve_jump_auth(conn, jump_server_id)? {
            Some(v) => v,
            None => return Err("jump host not found".into()),
        }
    };
    let effective_user = if user.trim().is_empty() {
        "root".to_string()
    } else {
        user.trim().to_string()
    };

    // 2. Direct TCP to the bastion.
    log(&format!("Connecting to jump host {}:{}...", host, port), "info");
    let tcp = match tokio::time::timeout(
        Duration::from_secs(10),
        tokio::net::TcpStream::connect((host.as_str(), port as u16)),
    )
    .await
    {
        Ok(Ok(s)) => {
            let _ = s.set_nodelay(true);
            apply_tcp_keepalive(&s);
            s
        }
        Ok(Err(e)) => return Err(humanize_network_err(&e.to_string(), &host, port, "Jump host connection")),
        Err(_) => return Err(format!("jump host {}:{} did not respond within 10 seconds", host, port)),
    };

    // 3. Host-key round-trip: own nonce, shared fp_txs map + frontend session.
    let (fp_tx, fp_rx) = tokio::sync::oneshot::channel();
    let jump_nonce: String = {
        let mut bytes = [0u8; 16];
        rand::thread_rng().fill(&mut bytes);
        hex::encode(bytes)
    };
    fp_txs.lock().await.insert(jump_nonce.clone(), fp_tx);
    let fp_outcome = std::sync::Arc::new(std::sync::atomic::AtomicI8::new(-1));

    let handler = ssh_manager::ClientHandler {
        app: app.clone(),
        session_id: session_id.to_string(),
        connect_nonce: jump_nonce.clone(),
        server_host: host.clone(),
        server_port: port as u16,
        db: std::sync::Arc::clone(db),
        fp_rx: Some(fp_rx),
        forwarded_targets: std::sync::Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
        fp_outcome,
    };

    // 4. Handshake.
    let config = std::sync::Arc::new(build_ssh_client_config());
    log("Jump host: SSH handshake...", "info");
    let connect_res = tokio::time::timeout(
        Duration::from_secs(15),
        russh::client::connect_stream(config, tcp, handler),
    )
    .await;
    // The host-key prompt (if any) is resolved by now — drop the sender.
    fp_txs.lock().await.remove(&jump_nonce);

    let mut session = match connect_res {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => return Err(format!("jump host handshake failed: {}", e)),
        Err(_) => return Err("jump host handshake timed out".into()),
    };

    // 5. Auth ladder: key → password → keyboard-interactive.
    let mut auth_res = if let Some((private_key, passphrase)) = key_data {
        log("Jump host: private key authentication...", "info");
        let normalized_key = private_key.replace("\r\n", "\n");
        match russh_keys::decode_secret_key(&normalized_key, passphrase.as_deref()) {
            Ok(keypair) => session.authenticate_publickey(&effective_user, std::sync::Arc::new(keypair)).await,
            Err(e) => Err(russh::Error::from(std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))),
        }
    } else if let Some(pass) = password {
        log("Jump host: password authentication...", "info");
        session.authenticate_password(&effective_user, pass).await
    } else {
        Ok(false)
    };

    if !matches!(auth_res, Ok(true)) {
        if let Some(kbi_res) =
            run_keyboard_interactive(&mut session, &effective_user, app, session_id, &jump_nonce, kbi_txs).await
        {
            auth_res = kbi_res;
        }
    }
    // Belt-and-suspenders: no dangling interactive sender for this hop.
    kbi_txs.lock().await.remove(&jump_nonce);

    match auth_res {
        Ok(true) => {
            log("Jump host authenticated.", "success");
            Ok(session)
        }
        Ok(false) => Err("jump host authentication failed".into()),
        Err(e) => Err(format!("jump host auth error: {}", e)),
    }
}

/// Fully tear down ONE connection map-entry by its exact key. Used to reap the
/// dedicated `::sftp` / `::fwd` secondary connections the separate-sessions
/// feature spins up, so disconnecting or reconnecting the PRIMARY never orphans
/// them (a leaked secondary would hold a live TCP + SSH session forever). Safe
/// to call on a key that doesn't exist — every step is then a no-op. Bumps the
/// key's generation so the secondary's own health watcher observes the change on
/// its next tick and bows out WITHOUT emitting a `session-disconnected-{key}`.
async fn teardown_connection_key(state: &SshState, mirrors: &MirrorMap, key: &str) {
    // Bump the generation FIRST — before removing the connection — so a connect
    // worker for this key that's still handshaking observes the newer value at
    // its "still wanted?" registration guard and bails instead of registering a
    // zombie. (Worker holds the generation lock across its check+insert, and we
    // bump-then-remove here, so the two orderings can't leave a stray entry.)
    {
        let mut g = state.session_generation.lock().await;
        let next = g.get(key).copied().unwrap_or(0).wrapping_add(1);
        g.insert(key.to_string(), next);
    }
    // Stop forwarders first so their listener sockets release before the SSH
    // handle drops — same ordering rationale as disconnect_session.
    tunnel::stop_all_for_session(&state.tunnels, key).await;
    state.forwarded_targets.lock().await.remove(key);
    state.session_tunnel_specs.lock().await.remove(key);
    mirror::stop_all_for_session(mirrors, key).await;
    state.sftp_sessions.lock().await.remove(key);
    // The base session's cached SFTP subsystem may be riding this dedicated
    // `::sftp` transport — drop it so the next file op re-opens on whatever
    // transport remains instead of erroring on a closed channel.
    if let Some(base) = key.strip_suffix("::sftp") {
        state.sftp_sessions.lock().await.remove(base);
    }
    state.connections.lock().await.remove(key);
    state.jump_connections.lock().await.remove(key);
    // Wipe temp files the secondary's SFTP downloads / drags left behind.
    let td = session_sftp_dir(key);
    if td.exists() {
        let _ = std::fs::remove_dir_all(&td);
    }
    let dd = session_drag_dir(key);
    if dd.exists() {
        let _ = std::fs::remove_dir_all(&dd);
    }
}

#[tauri::command]
async fn initiate_connection(
    app: tauri::AppHandle,
    state: tauri::State<'_, SshState>,
    db_state: tauri::State<'_, DbState>,
    mirrors: tauri::State<'_, MirrorMap>,
    session_id: String,
    server_id: i32,
    custom_password: Option<String>,
    quick_auth: Option<QuickAuth>,
    // Separate-sessions feature (default OFF). `session_role` marks whether this
    // is the primary terminal connection ("primary", the default) or a dedicated
    // secondary connection carved off for SFTP ("sftp") or port-forwarding
    // ("forward"). `separate_sessions` is only meaningful on the PRIMARY call: it
    // tells the primary to NOT auto-start the saved tunnels itself, because the
    // dedicated "forward" connection (opened by the frontend right after) will.
    // See the connection topology notes near the auth block below.
    session_role: Option<String>,
    separate_sessions: Option<bool>,
) -> Result<(), String> {
    use tauri::Emitter;
    use tokio::time::Duration;
    use russh::client;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    // Connection role + topology flags. Kept as plain locals so the whole
    // function (worker included) can branch on them without re-parsing.
    let role: String = session_role
        .as_deref()
        .map(str::to_string)
        .unwrap_or_else(|| "primary".to_string());
    let is_secondary = role != "primary";
    let separate = separate_sessions.unwrap_or(false);
    // Only the PRIMARY drives interactive keyboard-interactive (2FA) auth. A
    // secondary connection that hit a 2FA challenge would emit a `kbi-prompt`
    // the UI isn't wired to answer for the suffixed session id, so it would just
    // hang. Instead we let the secondary fail its auth and the frontend falls
    // back to routing SFTP / forwarding over the primary connection.
    let allow_kbi = !is_secondary;
    // Which connection auto-starts this server's saved tunnels inline:
    //   - primary + shared mode    → yes (unchanged legacy behaviour)
    //   - primary + separate mode  → no  (the "forward" connection takes over)
    //   - "forward" secondary      → never inline; it runs the MIGRATION block
    //     instead (stop anything running under the base tag, then restart the
    //     full replay list on itself, still tagged under the base session id)
    //   - "sftp" secondary         → no
    let start_tunnels_inline = role == "primary" && !separate;

    println!("[BACKEND] initiate_connection invoked for session_id: {} (role: {}, separate: {}), server_id: {}", session_id, role, separate, server_id);

    // Reconnect path: if a previous session under this id is still registered,
    // tear it down before starting the fresh handshake. Without this, the
    // duplicate-detection early-return below would silently drop every
    // reconnect attempt and the UI would just sit on "connecting…" forever.
    // Tunnels + forwarded_targets get the same treatment further down via the
    // stop_all_for_session call, so we don't duplicate that here.
    {
        let mut conns = state.connections.lock().await;
        if conns.remove(&session_id).is_some() {
            println!("[BACKEND] Tearing down stale session for reconnect: {}", session_id);
        }
    }
    // Drop any ProxyJump bastion handle from a prior attempt — dropping it
    // closes the old jump connection so the reconnect opens a fresh one.
    state.jump_connections.lock().await.remove(&session_id);
    state.sftp_sessions.lock().await.remove(&session_id);
    // Terminal IDs are `${session_id}-term-N` (see SessionView.tsx), so we
    // sweep every PTY task tied to the old handle. Match by the exact
    // `${session_id}-term-` prefix so id `session-1` doesn't sweep the
    // terminals of `session-10`, `session-11`, ... — a `contains`-based
    // match here silently tore down unrelated live sessions once server
    // IDs (which are SQLite autoincrement ints) crossed 10.
    let term_prefix = format!("{}-term-", session_id);
    state.terminal_txs.lock().await.retain(|k, _| !k.starts_with(&term_prefix));
    state.resize_txs.lock().await.retain(|k, _| !k.starts_with(&term_prefix));
    // Bump the generation so any watcher task still alive from the prior
    // attempt sees a newer value next tick and bails silently instead of
    // racing the new connect to emit `session-disconnected-{id}`.
    let connect_generation: u64 = {
        let mut g = state.session_generation.lock().await;
        let next = g.get(&session_id).copied().unwrap_or(0).wrapping_add(1);
        g.insert(session_id.clone(), next);
        next
    };

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
    // NB: the fp_txs insert is deliberately deferred until AFTER the DB
    // resolution below. The resolution block has several `?` early-returns; if
    // we inserted here, any of them would return before the worker (which owns
    // the FpCleanupGuard) is spawned, orphaning the sender in the map forever.

    let session_id_clone = session_id.clone();
    let state_connections = Arc::clone(&state.connections);
    let state_sftp_sessions = Arc::clone(&state.sftp_sessions);
    let state_tunnels = Arc::clone(&state.tunnels);
    let state_session_tunnel_specs = Arc::clone(&state.session_tunnel_specs);
    let state_session_generation = Arc::clone(&state.session_generation);
    let fp_txs_clone = Arc::clone(&state.fp_txs);
    let kbi_txs_clone = Arc::clone(&state.kbi_txs);
    let state_jump_connections = Arc::clone(&state.jump_connections);
    // Second Arc into the DB for the ProxyJump hop — the handler below moves
    // the primary `db_conn_shared`, and the spawned worker needs its own owned
    // handle to resolve the bastion's credentials.
    let db_for_jump = Arc::clone(&db_state.conn);
    let db_conn_shared = Arc::clone(&db_state.conn);

    // Reconnect path: tear down any stale listeners + R-tunnel registrations
    // bound to this session_id. Without this, the old TCP listener stays bound
    // to the local port, the new start_tunnel below hits a bind conflict, and
    // every existing tunnel silently routes traffic into a dead SSH handle.
    // First-connect is a no-op (no entries to remove).
    tunnel::stop_all_for_session(&state.tunnels, &session_id).await;
    state.forwarded_targets.lock().await.remove(&session_id);
    // Same story for mirrors — a reconnect must not inherit a mirror worker
    // that still holds an Arc<SftpSession> pointing at the DEAD handle from
    // the previous attempt. Without this the reconnected session appears
    // fine but the mirror keeps writing upload-fail logs against the old
    // socket forever, and manual Stop is the only way to clear it.
    mirror::stop_all_for_session(&mirrors, &session_id).await;

    // Separate-sessions: a PRIMARY (re)connect must reap any stale dedicated
    // `::sftp` / `::fwd` connections from a previous attempt, so the frontend can
    // re-open fresh ones after this success. Gated to the primary so a secondary
    // connect doesn't try to sweep its own (non-existent) grandchildren. Skipped
    // entirely for keys that already carry a `::` suffix (i.e. this IS a
    // secondary), and a plain no-op on first connect / shared mode.
    if !is_secondary {
        teardown_connection_key(state.inner(), mirrors.inner(), &format!("{}::sftp", session_id)).await;
        teardown_connection_key(state.inner(), mirrors.inner(), &format!("{}::fwd", session_id)).await;
    } else {
        // Dedicated-transport reconnect (manual button / auto-retry) OVER a
        // still-live old handle: the base-tagged tunnels ride this transport and
        // the base SFTP cache points at it, so release them before the new
        // handshake. Otherwise, if the fresh attempt fails, the old tunnels keep
        // running on a connection nobody watches (zombie) and file ops hit a
        // dead subsystem. Mirrors disconnect_session's dedicated-transport hooks.
        if let Some(base) = session_id.strip_suffix("::fwd") {
            tunnel::stop_all_for_session(&state.tunnels, base).await;
        }
        if let Some(base) = session_id.strip_suffix("::sftp") {
            state.sftp_sessions.lock().await.remove(base);
        }
    }

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
            None,                       // jump_host_id — quick connect never bounces through a bastion
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
                   s.auth_type, c.auth_type as cred_auth_type, s.tunnels,
                   s.jump_host_id
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
            let jump_host_id: Option<i32> = row.get::<_, Option<i32>>(14).unwrap_or_default();

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

            Some((host, port, username, password, key_data, proxy_type, proxy_host, proxy_port, server_auth_type, cred_auth_type, key_id, effective_key_id, tunnels_json, jump_host_id))
        } else {
            None
        }
    }
    };

    let (host, port, user, password, key_data, proxy_type, proxy_host, proxy_port, server_auth_type, cred_auth_type, db_key_id, effective_key_id, tunnels_json, jump_host_id) = match db_res {
        Some(val) => val,
        None => {
            return Err("Server not found".into());
        }
    };

    // DB resolution succeeded — now register the fingerprint sender. From here
    // on the worker is always spawned, so its FpCleanupGuard guarantees this
    // entry is removed even on the failure paths.
    state.fp_txs.lock().await.insert(connect_nonce.clone(), fp_tx);

    // Shared between the handler and the connect driver so we can tell host-
    // key timeouts apart from real auth errors on the failure path. See
    // ClientHandler::fp_outcome for the meaning of the values.
    let fp_outcome = std::sync::Arc::new(std::sync::atomic::AtomicI8::new(-1));
    let fp_outcome_for_driver = std::sync::Arc::clone(&fp_outcome);

    let handler = ssh_manager::ClientHandler {
        app: app.clone(),
        session_id: session_id.clone(),
        connect_nonce: connect_nonce.clone(),
        server_host: host.clone(),
        server_port: port as u16,
        db: db_conn_shared,
        fp_rx: Some(fp_rx),
        forwarded_targets: Arc::clone(&session_forwarded_targets),
        fp_outcome: std::sync::Arc::clone(&fp_outcome),
    };

    let cleanup_nonce = connect_nonce.clone();
    // Own an Arc into MirrorMap so the outer spawn (which requires 'static)
    // doesn't try to borrow the caller's `mirrors: State<'_, MirrorMap>`.
    // The Arc is 'static; the State reference is not.
    let mirrors_owned: MirrorMap = mirrors.inner().clone();

    tauri::async_runtime::spawn(async move {
        println!("[BACKEND WORKER] Started connection worker thread for session: {}", session_id_clone);

        struct FpCleanupGuard {
            fp_txs: Arc<Mutex<std::collections::HashMap<String, tokio::sync::oneshot::Sender<bool>>>>,
            kbi_txs: Arc<Mutex<std::collections::HashMap<String, tokio::sync::oneshot::Sender<Option<Vec<String>>>>>>,
            nonce: String,
        }
        impl Drop for FpCleanupGuard {
            fn drop(&mut self) {
                let fp_txs = Arc::clone(&self.fp_txs);
                let kbi_txs = Arc::clone(&self.kbi_txs);
                let nonce = self.nonce.clone();
                tauri::async_runtime::spawn(async move {
                    fp_txs.lock().await.remove(&nonce);
                    // Worker died mid keyboard-interactive prompt — drop any
                    // dangling sender so a late frontend response is a no-op.
                    kbi_txs.lock().await.remove(&nonce);
                });
            }
        }
        let _guard = FpCleanupGuard {
            fp_txs: Arc::clone(&fp_txs_clone),
            kbi_txs: Arc::clone(&kbi_txs_clone),
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

        // ProxyJump takes precedence over a direct / proxied transport. When a
        // bastion is configured we connect+auth to it first, then open a
        // `direct-tcpip` channel to the REAL target over that connection and
        // use the channel as the transport for the primary handshake — exactly
        // what `ssh -J bastion target` does. The bastion's own Handle is parked
        // in `jump_handle_holder` so it lives through the target handshake; on
        // success it moves into `state_jump_connections` for the session's life.
        let mut jump_handle_holder: Option<russh::client::Handle<ssh_manager::ClientHandler>> = None;
        let stream_res: Result<Box<dyn AsyncStream>, String> = if let Some(jid) = jump_host_id {
            emit_log(&format!("ProxyJump: routing through jump host (server id {})...", jid), "info");
            match connect_jump_host(&app, &db_for_jump, &fp_txs_clone, &kbi_txs_clone, &session_id_clone, jid).await {
                Ok(jump_handle) => {
                    // Originator address is cosmetic (logged by the bastion); the
                    // pair below is what OpenSSH sends for a -J hop.
                    match jump_handle
                        .channel_open_direct_tcpip(host.clone(), port as u32, "127.0.0.1", 0)
                        .await
                    {
                        Ok(channel) => {
                            emit_log(&format!("ProxyJump: opened tunnel to {}:{} through the jump host.", host, port), "success");
                            let boxed: Box<dyn AsyncStream> = Box::new(channel.into_stream());
                            jump_handle_holder = Some(jump_handle);
                            Ok(boxed)
                        }
                        Err(e) => Err(format!(
                            "ProxyJump: could not open a channel to {}:{} through the jump host: {}",
                            host, port, e
                        )),
                    }
                }
                Err(e) => Err(format!("ProxyJump: {}", e)),
            }
        } else {
        match proxy_type.as_str() {
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
                        // Socks5Stream derefs to the inner TcpStream, so the
                        // kernel keepalive applies to the real proxy socket.
                        apply_tcp_keepalive(&stream);
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
                        apply_tcp_keepalive(&tcp_stream);
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
                        apply_tcp_keepalive(&stream);
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
        // Config (keepalive, windows, algorithm negotiation) is shared with the
        // ProxyJump hop — see build_ssh_client_config for the full rationale.
        let config = Arc::new(build_ssh_client_config());

        let connect_future = client::connect_stream(config, StreamWrapper(stream), handler);

        match tokio::time::timeout(Duration::from_secs(15), connect_future).await {
            Ok(Ok(mut session)) => {
                emit_log("SSH Handshake complete. Authenticating user...", "info");
                
                let final_pass = custom_password.or(password);
                
                let mut auth_res = if let Some((private_key, passphrase)) = key_data {
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

                // Keyboard-interactive (2FA / verification-code) fallback. Many
                // hardened servers gate login behind an interactive one-time
                // code AFTER — or INSTEAD of — a password. If the primary
                // attempt above didn't already authenticate us and the server
                // offers keyboard-interactive, drive it: relay each prompt to
                // the UI, collect the user's answers, and send them back. If
                // the server doesn't offer it, `run_keyboard_interactive`
                // returns None and we keep the original auth result untouched.
                if allow_kbi && !matches!(auth_res, Ok(true)) {
                    if let Some(kbi_res) = run_keyboard_interactive(
                        &mut session,
                        &effective_user,
                        &app,
                        &session_id_clone,
                        &connect_nonce,
                        &kbi_txs_clone,
                    )
                    .await
                    {
                        auth_res = kbi_res;
                    }
                }

                match auth_res {
                    Ok(true) => {
                        let session_arc = Arc::new(Mutex::new(session));
                        // "Still wanted?" registration guard. The handshake can
                        // take up to ~15s (longer via proxy/jump). If, meanwhile,
                        // a reconnect for this key bumped the generation or a
                        // disconnect/teardown swept it (both of which bump the
                        // generation BEFORE removing the connection), registering
                        // now would create a ZOMBIE: a live SSH connection with a
                        // watcher that immediately exits on the generation
                        // mismatch, plus — for ::fwd — tunnels migrated onto a
                        // connection nobody tracks. Do the check and the insert
                        // TOGETHER under the generation lock so a concurrent
                        // teardown can't interleave: we either observe the newer
                        // generation and bail, or insert first and the teardown's
                        // later connection-remove reaps us.
                        let registered = {
                            let g = state_session_generation.lock().await;
                            if g.get(&session_id_clone).copied().unwrap_or(0) == connect_generation {
                                state_connections
                                    .lock()
                                    .await
                                    .insert(session_id_clone.clone(), Arc::clone(&session_arc));
                                true
                            } else {
                                false
                            }
                        };
                        if !registered {
                            emit_log("Connection superseded before it was ready — dropping it.", "info");
                            // `session_arc` drops here, closing the transport.
                            // The FpCleanupGuard's Drop still clears the nonce.
                            return;
                        }
                        emit_log("Authentication successful. Session ready.", "success");
                        // A dedicated `::sftp` transport just came up — drop the
                        // base session's cached SFTP subsystem (it rides the
                        // primary). The next file operation re-opens on this
                        // dedicated connection via get_sftp_session's
                        // transport preference. Live migration, no re-keying.
                        if let Some(base) = session_id_clone.strip_suffix("::sftp") {
                            state_sftp_sessions.lock().await.remove(base);
                        }
                        // ProxyJump: now that the target session is live, park
                        // the bastion's Handle so it stays alive for the whole
                        // session (its direct-tcpip channel carries our
                        // transport). Dropped on reconnect / disconnect / close.
                        if let Some(jh) = jump_handle_holder.take() {
                            state_jump_connections.lock().await.insert(session_id_clone.clone(), jh);
                        }
                        let _ = app.emit(
                            &format!("connection-success-{}", session_id_clone),
                            serde_json::json!({}),
                        );

                        // Separate-sessions topology: in shared mode the primary
                        // owns the saved tunnels; in separate mode the dedicated
                        // `::fwd` connection owns them (it re-enters this same
                        // block under its own suffixed session id, so the tunnels
                        // are tagged/keyed against `::fwd` and ride that handle).
                        // `start_tunnels_inline` is false for the `::sftp`
                        // connection and for the primary when `separate` is on.
                        if start_tunnels_inline {
                        // Auto-start every tunnel attached to this session.
                        // Source of truth is the in-memory `session_tunnel_specs`
                        // map, which carries both the DB-saved rules AND any
                        // ad-hoc tunnels the user opened during the previous
                        // session lifetime. On first connect it's empty, so
                        // we seed it from the DB row's `tunnels` JSON.
                        let specs_to_start: Vec<tunnel::TunnelSpec> = {
                            let mut map = state_session_tunnel_specs.lock().await;
                            match map.get(&session_id_clone).cloned() {
                                Some(existing) if !existing.is_empty() => existing,
                                _ => match serde_json::from_str::<Vec<tunnel::TunnelSpec>>(&tunnels_json) {
                                    Ok(db_specs) => {
                                        map.insert(session_id_clone.clone(), db_specs.clone());
                                        db_specs
                                    }
                                    Err(e) => {
                                        emit_log(&format!("Tunnels JSON parse error: {}", e), "error");
                                        Vec::new()
                                    }
                                }
                            }
                        };
                        for spec in &specs_to_start {
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
                                Err(e) => {
                                    emit_log(
                                        &format!("Tunnel start failed ({} {}): {}", spec.kind, spec.local, e),
                                        "error",
                                    );
                                    // A bind conflict or refused tcpip-forward
                                    // means this spec is poison for the
                                    // current session — strip it from the
                                    // replay list so we don't re-trigger the
                                    // same error on every reconnect.
                                    let mut map = state_session_tunnel_specs.lock().await;
                                    if let Some(list) = map.get_mut(&session_id_clone) {
                                        list.retain(|s| s != spec);
                                    }
                                }
                            }
                        }
                        } // end if start_tunnels_inline

                        // Dedicated `::fwd` transport: MIGRATE the session's
                        // tunnels onto this fresh connection. Stop whatever is
                        // running under the base tag (riding the primary, or a
                        // previous dead `::fwd`) so re-binding the same local
                        // ports can't conflict, then start the full replay
                        // list — seeded from the node's saved tunnels on first
                        // engagement — on this handle, still tagged under the
                        // BASE session id so the Tunnels panel / list / events
                        // never re-key. This runs on fresh connects, live
                        // toggle-on, the reconnect button, and auto-retry.
                        if role == "forward" {
                            let base = session_id_clone
                                .strip_suffix("::fwd")
                                .unwrap_or(&session_id_clone)
                                .to_string();
                            tunnel::stop_all_for_session(&state_tunnels, &base).await;
                            let specs: Vec<tunnel::TunnelSpec> = {
                                let mut map = state_session_tunnel_specs.lock().await;
                                // A PRESENT replay list — even an empty one — is
                                // authoritative: the session is already engaged
                                // and an empty list means the user stopped every
                                // tunnel. Only seed from the node's saved tunnels
                                // when the key is ABSENT (genuine first
                                // engagement) — otherwise stopped tunnels would
                                // resurrect on every ::fwd reconnect / toggle.
                                match map.get(&base).cloned() {
                                    Some(existing) => existing,
                                    None => match serde_json::from_str::<Vec<tunnel::TunnelSpec>>(&tunnels_json) {
                                        Ok(db_specs) => {
                                            if !db_specs.is_empty() {
                                                map.insert(base.clone(), db_specs.clone());
                                            }
                                            db_specs
                                        }
                                        Err(e) => {
                                            emit_log(&format!("Tunnels JSON parse error: {}", e), "error");
                                            Vec::new()
                                        }
                                    },
                                }
                            };
                            if !specs.is_empty() {
                                emit_log("Dedicated forwarding connection ready — moving tunnels onto it.", "info");
                                start_tunnel_specs_on(
                                    &app,
                                    &base,
                                    &session_arc,
                                    &session_forwarded_targets,
                                    &state_tunnels,
                                    &state_session_tunnel_specs,
                                    specs,
                                )
                                .await;
                            }
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
                        let state_gen_w = Arc::clone(&state_session_generation);
                        let state_tunnels_w = Arc::clone(&state_tunnels);
                        let state_mirrors_w: MirrorMap = mirrors_owned.clone();
                        let my_gen = connect_generation;
                        tauri::async_runtime::spawn(async move {
                            // Two-tier liveness check:
                            //   - Every 2s: cheap is_closed() poll. Catches TCP
                            //     RST / FIN, russh's own teardown, AND — now that
                            //     the transport socket carries OS TCP keepalive
                            //     (see apply_tcp_keepalive) — kernel-detected
                            //     dead peers during long idle.
                            //   - Slow active probe: open a tiny SSH channel to
                            //     force a real round-trip, catching black-holes
                            //     the kernel hasn't flagged yet. Primary every
                            //     ~30s, dedicated `::sftp`/`::fwd` secondaries
                            //     every ~60s (they matter less urgently and the
                            //     probes multiply per-session overhead).
                            //     TWO consecutive probe failures are required
                            //     before declaring death: on poor networks a
                            //     single 10s latency spike is common, and the
                            //     old one-strike/5s-timeout probe tore down
                            //     perfectly recoverable sessions — the exact
                            //     opposite of what a flaky link needs.
                            let probe_every: u32 = if sid_w.contains("::") { 30 } else { 15 };
                            let mut tick: u32 = 0;
                            let mut probe_strikes: u8 = 0;
                            loop {
                                tokio::time::sleep(Duration::from_secs(2)).await;
                                tick = tick.wrapping_add(1);

                                // Generation guard: a reconnect under the same
                                // session_id bumps the counter. If we observe a
                                // newer value here, a fresh watcher has already
                                // taken over — bow out silently so we don't
                                // double-emit `session-disconnected-{id}`.
                                {
                                    let g = state_gen_w.lock().await;
                                    if g.get(&sid_w).copied().unwrap_or(0) != my_gen {
                                        break;
                                    }
                                }

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

                                let mut dead = is_closed;

                                if !dead && tick % probe_every == 0 {
                                    // Active probe: hold the handle lock long
                                    // enough to start AND finish the round
                                    // trip — concurrent commands wait, but
                                    // that's fine, they would block on the
                                    // same lock to open their own channel
                                    // anyway. 10s timeout: generous enough
                                    // that a congested-but-alive link doesn't
                                    // strike out spuriously.
                                    let probe = {
                                        let h = handle_arc.lock().await;
                                        tokio::time::timeout(
                                            Duration::from_secs(10),
                                            h.channel_open_session(),
                                        ).await
                                    };
                                    match probe {
                                        Ok(Ok(ch)) => {
                                            probe_strikes = 0;
                                            // Close cleanly so the server
                                            // doesn't log a stuck session.
                                            let _ = ch.close().await;
                                        }
                                        _ => {
                                            probe_strikes += 1;
                                            if probe_strikes >= 2 {
                                                dead = true;
                                            } else {
                                                // One strike: re-probe on the
                                                // next 2s tick instead of a
                                                // full interval away, so a
                                                // real death still surfaces
                                                // promptly.
                                                tick = probe_every.wrapping_sub(1);
                                            }
                                        }
                                    }
                                }

                                if dead {
                                    // Re-check generation + verify OUR handle is
                                    // still the registered one before tearing
                                    // anything down. A slow probe holds the handle
                                    // lock up to 10s; during that window a manual
                                    // reconnect / auto-retry can bump the
                                    // generation and register a fresh, healthy
                                    // connection. Without this guard we'd remove
                                    // that NEW connection and kill its just-
                                    // migrated tunnels. Bail silently on mismatch.
                                    {
                                        let g = state_gen_w.lock().await;
                                        if g.get(&sid_w).copied().unwrap_or(0) != my_gen {
                                            break;
                                        }
                                    }
                                    // Compare-and-remove: only drop the map entry
                                    // if it's still OUR handle (Arc identity).
                                    {
                                        let mut conns = state_w.lock().await;
                                        match conns.get(&sid_w) {
                                            Some(cur) if Arc::ptr_eq(cur, &handle_arc) => { conns.remove(&sid_w); }
                                            _ => break, // superseded by a fresh connection — leave it be
                                        }
                                    }
                                    state_sftp_w.lock().await.remove(&sid_w);
                                    // Tear down all tunnels bound to this
                                    // session so their listeners release the
                                    // local ports + bridge tasks exit. Without
                                    // this, the listeners stayed up holding
                                    // the now-dead Arc<Handle>, and stop_tunnel
                                    // calls during reconnect raced with the
                                    // replay logic. This is the single
                                    // authoritative SSH-death tunnel-teardown
                                    // path; the listener tasks themselves no
                                    // longer probe the handle.
                                    tunnel::stop_all_for_session(&state_tunnels_w, &sid_w).await;
                                    // Dedicated-transport death: tunnels are
                                    // tagged under the BASE session id but ride
                                    // this `::fwd` connection — stop them so
                                    // their listeners release; the frontend's
                                    // auto-retry either brings the transport
                                    // back (migration restarts them on it) or
                                    // falls back to the primary. Same for the
                                    // base SFTP cache riding a dead `::sftp`.
                                    if let Some(base) = sid_w.strip_suffix("::fwd") {
                                        tunnel::stop_all_for_session(&state_tunnels_w, base).await;
                                    }
                                    if let Some(base) = sid_w.strip_suffix("::sftp") {
                                        state_sftp_w.lock().await.remove(base);
                                    }
                                    // Stop any mirrors bound to this session too — otherwise the
                                    // mirror worker keeps its own Arc<SftpSession> pointing at
                                    // this dead handle and hammers upload-fail on every future
                                    // FS event until the app is quit.
                                    mirror::stop_all_for_session(&state_mirrors_w, &sid_w).await;
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
                        // Server reached the auth phase and explicitly told
                        // us "no". This is the only branch that maps to a
                        // real auth failure — everything else gets routed
                        // through `classify_russh_error` so a network drop
                        // or host-key timeout never gets relabelled as one.
                        emit_log("Authentication rejected by server.", "error");
                        let _ = app.emit(
                            &format!("connection-failed-{}", session_id_clone),
                            serde_json::json!({
                                "reason": "Authentication rejected by server (wrong password, missing key, or account locked).",
                                "is_auth_error": true,
                            }),
                        );
                    },
                    Err(e) => {
                        let kind = classify_russh_error(&e);
                        let target = format!("{}:{}", host, port);
                        let reason = describe_error_kind(kind, &target);
                        emit_log(&format!("{} (raw: {})", reason, e), "error");
                        let _ = app.emit(
                            &format!("connection-failed-{}", session_id_clone),
                            serde_json::json!({
                                "reason": reason,
                                "is_auth_error": kind.is_auth(),
                            }),
                        );
                    }
                }
            },
            Ok(Err(e)) => {
                // Connect-stream failed AFTER the TCP socket opened — most
                // commonly this is the host-key flow ending in either a
                // declined fingerprint or a timed-out prompt. Read the
                // outcome the handler stashed and surface a precise reason
                // instead of letting it ride the generic transport bucket.
                use std::sync::atomic::Ordering;
                let fp = fp_outcome_for_driver.load(Ordering::SeqCst);
                let (reason, kind) = match fp {
                    2 => (
                        "Host key prompt timed out — Reconnect and approve the fingerprint within 90 seconds.".to_string(),
                        ConnectErrorKind::HostKey,
                    ),
                    0 => (
                        "Host key was not approved. Reconnect to see the fingerprint prompt again.".to_string(),
                        ConnectErrorKind::HostKey,
                    ),
                    _ => {
                        let kind = classify_russh_error(&e);
                        let target = format!("{}:{}", host, port);
                        (describe_error_kind(kind, &target), kind)
                    }
                };
                emit_log(&format!("{} (raw: {})", reason, e), "error");
                let _ = app.emit(
                    &format!("connection-failed-{}", session_id_clone),
                    serde_json::json!({
                        "reason": reason,
                        "is_auth_error": kind.is_auth(),
                    }),
                );
            },
            Err(_) => {
                // 15s wall-clock on connect_stream — the TCP socket is up
                // but the SSH handshake never completed. Distinct enough
                // from the auth path to deserve its own message.
                let msg = format!(
                    "{}:{} did not finish SSH handshake within 15 seconds — host may be filtering SSH or running a non-SSH service on this port.",
                    host, port
                );
                emit_log(&msg, "error");
                let _ = app.emit(
                    &format!("connection-failed-{}", session_id_clone),
                    serde_json::json!({
                        "reason": msg,
                        "is_auth_error": false,
                    }),
                );
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

/// Frontend callback for a keyboard-interactive (2FA / verification-code)
/// prompt. `responses` is `Some(answers)` when the user submits, or `None`
/// when they cancel. Mirrors `verify_fingerprint_response`: the nonce binds
/// the answer 1:1 to the connect attempt that emitted the prompt, and a nonce
/// with no in-flight entry is silently a no-op (stale / forged responses can't
/// satisfy a fresh prompt).
#[tauri::command]
async fn submit_kbi_response(
    state: tauri::State<'_, SshState>,
    nonce: String,
    responses: Option<Vec<String>>,
) -> Result<(), String> {
    if let Some(tx) = state.kbi_txs.lock().await.remove(&nonce) {
        let _ = tx.send(responses);
    }
    Ok(())
}

// ---- Port forwarding (tunnels) ---------------------------------------------

/// Resolve the SSH transport a session's tunnels should ride: the dedicated
/// `::fwd` connection when the per-tab toggle has one up, else the primary.
/// The forwarded-targets map MUST belong to the same connection as the handle
/// — for R tunnels the server pushes `forwarded-tcpip` channels back on the
/// connection that sent `tcpip_forward`, and its ClientHandler consults only
/// its own map (ssh_manager.rs::server_channel_open_forwarded_tcpip).
async fn resolve_tunnel_transport(
    state: &SshState,
    session_id: &str,
) -> Result<(Arc<tokio::sync::Mutex<russh::client::Handle<ssh_manager::ClientHandler>>>, tunnel::ForwardedTargets), String> {
    let fwd_key = format!("{}::fwd", session_id);
    let (primary, dedicated) = {
        let conns = state.connections.lock().await;
        (conns.get(session_id).cloned(), if session_id.contains("::") { None } else { conns.get(&fwd_key).cloned() })
    };
    let targets_map = state.forwarded_targets.lock().await;
    if let Some(h) = dedicated {
        // The `::fwd` entry's forwarded-targets map is created by its own
        // initiate_connection; if it's somehow missing, fall through to the
        // primary rather than starting an R tunnel whose inbound channels
        // would never resolve.
        if let Some(t) = targets_map.get(&fwd_key) {
            return Ok((h, Arc::clone(t)));
        }
    }
    let h = primary.ok_or_else(|| "Session not connected".to_string())?;
    let t = targets_map
        .get(session_id)
        .cloned()
        .ok_or_else(|| "Session forwarded-targets map missing — reconnect first".to_string())?;
    Ok((h, t))
}

/// Start every spec in `specs` that isn't already running under `sid`'s tag,
/// on the given transport. Successes are recorded into the session's replay
/// list; failures are logged to the session log and the poison spec is
/// stripped from the replay list so auto-retry cycles don't error-loop on it.
/// Used by the `::fwd` migration (connect success), the fallback/restore
/// command, and shares its semantics with the primary's auto-start.
async fn start_tunnel_specs_on(
    app: &tauri::AppHandle,
    sid: &str,
    handle: &Arc<tokio::sync::Mutex<russh::client::Handle<ssh_manager::ClientHandler>>>,
    targets: &tunnel::ForwardedTargets,
    tunnels: &Arc<tokio::sync::Mutex<std::collections::HashMap<String, tunnel::ActiveTunnel>>>,
    specs_map: &Arc<tokio::sync::Mutex<std::collections::HashMap<String, Vec<tunnel::TunnelSpec>>>>,
    specs: Vec<tunnel::TunnelSpec>,
) {
    use tauri::Emitter;
    let emit_log = |msg: &str, log_type: &str| {
        let _ = app.emit(
            &format!("session-log-{}", sid),
            serde_json::json!({ "msg": msg, "type": log_type }),
        );
    };
    // Snapshot running specs for this tag. Two-phase (ids under the map lock,
    // status after releasing it) to respect the same lock-ordering rule as
    // tunnel::stop_all_for_session.
    let candidates: Vec<(tunnel::TunnelSpec, Arc<tokio::sync::Mutex<tunnel::TunnelStatus>>)> = {
        let map = tunnels.lock().await;
        map.values().map(|t| (t.spec.clone(), Arc::clone(&t.status))).collect()
    };
    let mut running: Vec<tunnel::TunnelSpec> = Vec::new();
    for (spec, status) in candidates {
        if status.lock().await.session_id == sid {
            running.push(spec);
        }
    }
    for spec in specs {
        if running.contains(&spec) {
            continue;
        }
        match tunnel::start_tunnel(
            app.clone(),
            sid.to_string(),
            Arc::clone(handle),
            Arc::clone(tunnels),
            Arc::clone(targets),
            spec.clone(),
        )
        .await
        {
            Ok(id) => {
                emit_log(&format!("Tunnel started [{}]: {} {}", id, spec.kind, spec.local), "info");
                let mut map = specs_map.lock().await;
                let entry = map.entry(sid.to_string()).or_insert_with(Vec::new);
                if !entry.contains(&spec) {
                    entry.push(spec);
                }
            }
            Err(e) => {
                emit_log(&format!("Tunnel start failed ({} {}): {}", spec.kind, spec.local, e), "error");
                let mut map = specs_map.lock().await;
                if let Some(list) = map.get_mut(sid) {
                    list.retain(|s| s != &spec);
                }
            }
        }
    }
}

#[tauri::command]
async fn start_tunnel(
    app: tauri::AppHandle,
    state: tauri::State<'_, SshState>,
    session_id: String,
    spec: tunnel::TunnelSpec,
) -> Result<String, String> {
    // Dedicated-transport aware: rides the `::fwd` connection when one is up,
    // the primary otherwise. The tunnel is TAGGED under the plain session id
    // either way, so the Tunnels panel / list / teardown never re-key.
    let (handle, forwarded) = resolve_tunnel_transport(state.inner(), &session_id).await?;
    let id = tunnel::start_tunnel(app, session_id.clone(), handle, Arc::clone(&state.tunnels), forwarded, spec.clone()).await?;
    // Record this ad-hoc spec against the session so a future reconnect can
    // re-open it. Dedup against (kind, local, remote) so toggling the same
    // tunnel off-and-on doesn't accumulate duplicates.
    {
        let mut map = state.session_tunnel_specs.lock().await;
        let entry = map.entry(session_id).or_insert_with(Vec::new);
        if !entry.contains(&spec) {
            entry.push(spec);
        }
    }
    Ok(id)
}

#[tauri::command]
async fn stop_tunnel(
    state: tauri::State<'_, SshState>,
    tunnel_id: String,
) -> Result<(), String> {
    // Snapshot spec + session BEFORE stop — the listener task removes itself
    // from the tunnels map on exit, racing this lookup. With the snapshot in
    // hand we can drop the matching entry from the session's replay list so
    // a user who explicitly stopped a tunnel doesn't see it return on the
    // next reconnect.
    let snapshot = {
        let map = state.tunnels.lock().await;
        match map.get(&tunnel_id) {
            Some(t) => {
                let status = t.status.lock().await;
                Some((t.spec.clone(), status.session_id.clone()))
            }
            None => None,
        }
    };
    tunnel::stop_tunnel(&state.tunnels, &tunnel_id).await?;
    if let Some((spec, sid)) = snapshot {
        let mut map = state.session_tunnel_specs.lock().await;
        if let Some(list) = map.get_mut(&sid) {
            list.retain(|s| s != &spec);
        }
    }
    Ok(())
}

#[tauri::command]
async fn list_tunnels(
    state: tauri::State<'_, SshState>,
    session_id: Option<String>,
) -> Result<Vec<tunnel::TunnelStatus>, String> {
    Ok(tunnel::list_tunnels(&state.tunnels, session_id.as_deref()).await)
}

/// (Re)start every tunnel the session should have — the in-memory replay list
/// when it's non-empty, else the node's saved tunnels JSON — on the best
/// available transport (`::fwd` when the dedicated connection is up, primary
/// otherwise). Idempotent: specs already running under the session's tag are
/// skipped. The frontend calls this to restore forwarding after the dedicated
/// connection fails or is toggled off, so tunnels always land somewhere.
#[tauri::command]
async fn restart_session_tunnels(
    app: tauri::AppHandle,
    state: tauri::State<'_, SshState>,
    db_state: tauri::State<'_, DbState>,
    session_id: String,
    server_id: i32,
) -> Result<(), String> {
    // In-memory replay list first — it carries ad-hoc tunnels too. A PRESENT
    // list (even empty) is authoritative (user stopped everything); only seed
    // from the node's saved tunnels when the key is ABSENT (first engagement),
    // so explicitly-stopped tunnels don't resurrect on a restore.
    let existing: Option<Vec<tunnel::TunnelSpec>> =
        state.session_tunnel_specs.lock().await.get(&session_id).cloned();
    let mut specs: Vec<tunnel::TunnelSpec> = existing.clone().unwrap_or_default();
    if existing.is_none() && server_id > 0 {
        // Nested block so the non-Send rusqlite guard drops before any await.
        let tunnels_json: String = {
            let conn_guard = db_state.conn.lock().map_err(|_| "[STATE] LOCK_FAILED")?;
            let conn = conn_guard.as_ref().ok_or("[STATE] DATABASE_NOT_INITIALIZED")?;
            let mut stmt = conn
                .prepare("SELECT tunnels FROM servers WHERE id=?1")
                .map_err(|e| e.to_string())?;
            let mut rows = stmt.query([server_id]).map_err(|e| e.to_string())?;
            match rows.next().map_err(|e| e.to_string())? {
                Some(row) => row
                    .get::<_, Option<String>>(0)
                    .unwrap_or_default()
                    .unwrap_or_else(|| "[]".to_string()),
                None => "[]".to_string(),
            }
        };
        specs = serde_json::from_str(&tunnels_json).unwrap_or_default();
        if !specs.is_empty() {
            state
                .session_tunnel_specs
                .lock()
                .await
                .insert(session_id.clone(), specs.clone());
        }
    }
    if specs.is_empty() {
        return Ok(());
    }

    let (handle, targets) = resolve_tunnel_transport(state.inner(), &session_id).await?;
    start_tunnel_specs_on(
        &app,
        &session_id,
        &handle,
        &targets,
        &state.tunnels,
        &state.session_tunnel_specs,
        specs,
    )
    .await;
    Ok(())
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
    #[cfg(target_os = "android")]
    {
        return Err("Folder picker not available on Android.".into());
    }
    #[cfg(not(target_os = "android"))]
    {
        let path = rfd::AsyncFileDialog::new().pick_folder().await;
        Ok(path.map(|p| p.path().to_string_lossy().into_owned()))
    }
}

#[tauri::command]
async fn disconnect_session(
    app: tauri::AppHandle,
    state: tauri::State<'_, SshState>,
    mirrors: tauri::State<'_, MirrorMap>,
    session_id: String,
) -> Result<(), String> {
    // Invalidate any connect worker still handshaking for THIS key: bump the
    // generation before removing anything, so a late auth-success hits its
    // "still wanted?" guard and drops instead of re-registering a transport the
    // user just turned off (which would then steal / strand tunnels). Done for
    // every key, including secondaries — this is the toggle-off / reconnect-
    // button invalidation path.
    {
        let mut g = state.session_generation.lock().await;
        let next = g.get(&session_id).copied().unwrap_or(0).wrapping_add(1);
        g.insert(session_id.clone(), next);
    }
    // Disconnecting a dedicated transport directly (toggle-off / reconnect
    // button): tunnels are tagged under the BASE session id but ride this
    // `::fwd` connection. Stop them ONLY when the dedicated connection actually
    // exists — i.e. the tunnels really are riding it. If it's absent (a failed
    // `::fwd` that never registered, so the tunnels fell back to the primary),
    // stopping base-tagged tunnels here would needlessly bounce every live
    // primary tunnel. The caller restarts them on the best transport via
    // restart_session_tunnels afterwards.
    if let Some(base) = session_id.strip_suffix("::fwd") {
        if state.connections.lock().await.contains_key(&session_id) {
            tunnel::stop_all_for_session(&state.tunnels, base).await;
        }
    }
    // `::sftp`: remove the dedicated transport FIRST, then purge the base SFTP
    // cache — so an SFTP op racing this teardown can't re-cache a subsystem on
    // the dying transport after the purge (get_sftp_session's still-current
    // check would then see the transport gone and refuse to cache).
    if session_id.ends_with("::sftp") {
        state.connections.lock().await.remove(&session_id);
        if let Some(base) = session_id.strip_suffix("::sftp") {
            state.sftp_sessions.lock().await.remove(base);
        }
    }
    // Stop all forwarders so their listener sockets are released before the
    // SSH handle is dropped (otherwise newly-incoming connections would just
    // bounce off a dead channel).
    tunnel::stop_all_for_session(&state.tunnels, &session_id).await;
    state.forwarded_targets.lock().await.remove(&session_id);
    // Explicit user disconnect — drop the replay list too. (Auto-reconnect
    // calls `initiate_connection` directly and never hits this path, so
    // those tunnels survive the cycle.)
    state.session_tunnel_specs.lock().await.remove(&session_id);
    // Same idea for any mirror that's still running — its watcher would
    // try to push uploads through a dead SSH handle otherwise.
    mirror::stop_all_for_session(&mirrors, &session_id).await;
    // Drop SFTP first so the channel it holds is freed before we tear down the
    // underlying SSH handle.
    state.sftp_sessions.lock().await.remove(&session_id);
    state.connections.lock().await.remove(&session_id);
    // Drop any ProxyJump bastion handle for this session — closes the jump
    // connection once the target it was carrying is gone.
    state.jump_connections.lock().await.remove(&session_id);
    // Drop terminal tx/resize entries so a subsequent reconnect doesn't try
    // to write into a dead PTY task. Match by the exact `${session_id}-term-`
    // prefix — a `contains` here silently tears down session-10's terminals
    // when the user disconnects session-1.
    let term_prefix = format!("{}-term-", session_id);
    state.terminal_txs.lock().await.retain(|k, _| !k.starts_with(&term_prefix));
    state.resize_txs.lock().await.retain(|k, _| !k.starts_with(&term_prefix));
    // Wipe any temp files this session left behind (live-edit downloads + drag
    // staging). Best-effort: failures are usually because an editor still holds
    // a lock on a file, in which case the file persists until the OS cleans temp.
    let session_temp_dir = session_sftp_dir(&session_id);
    if session_temp_dir.exists() {
        let _ = std::fs::remove_dir_all(&session_temp_dir);
    }
    let session_drag = session_drag_dir(&session_id);
    if session_drag.exists() {
        let _ = std::fs::remove_dir_all(&session_drag);
    }
    // Separate-sessions: reap the dedicated `::sftp` / `::fwd` secondaries so an
    // explicit primary disconnect doesn't leak their SSH sessions. Only when
    // this IS a primary id (no `::` suffix) — a direct disconnect of a secondary
    // must not recurse into non-existent grandchildren.
    if !session_id.contains("::") {
        teardown_connection_key(state.inner(), mirrors.inner(), &format!("{}::sftp", session_id)).await;
        teardown_connection_key(state.inner(), mirrors.inner(), &format!("{}::fwd", session_id)).await;
    }
    // Tell the UI so the tab status dot flips to red. `user_initiated` keeps
    // SessionView from kicking off an auto-reconnect cycle for an intentional
    // disconnect — distinct from the watcher path which has no such flag.
    use tauri::Emitter;
    let _ = app.emit(
        &format!("session-disconnected-{}", session_id),
        serde_json::json!({
            "reason": "User disconnected",
            "user_initiated": true,
        }),
    );
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
    // Clone the Sender OUT of the map, then release the map lock before we
    // await on send(). If we held the guard across the send, a saturated
    // 32-slot mpsc on a slow SSH link would block every other terminal's
    // write path — one slow terminal freezes the entire app-wide typing
    // experience because the shared map guard is a global gate.
    // mpsc::Sender is cheap to clone.
    let tx = state.terminal_txs.lock().await.get(&terminal_id).cloned();
    if let Some(tx) = tx {
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

// Read-only Info-panel probes. Each tab fetches its own section on first
// click — the user explicitly wanted lazy per-tab fetch instead of one
// upfront mega-probe. Splitting the scripts keeps each round-trip small
// (≤200 ms typical) and lets a failing section never block the others.
const INFO_SCRIPT_OVERVIEW: &str = r#"echo __SUB_INFO_OV_SEP__
hostname 2>/dev/null
echo __SUB_INFO_OV_SEP__
( . /etc/os-release 2>/dev/null && printf "%s" "$PRETTY_NAME" ) || cat /etc/issue 2>/dev/null
printf "\n"
echo __SUB_INFO_OV_SEP__
uname -srm 2>/dev/null
echo __SUB_INFO_OV_SEP__
uptime 2>/dev/null
echo __SUB_INFO_OV_SEP__
# Memory. procps `free -b` gives bytes + an `available` column; busybox `free`
# ignores `-b` (prints KiB) and has no `available`, so fall back to the
# universal /proc/meminfo (kB) with a format tag the frontend switches on.
if F=$(free -b 2>/dev/null) && printf '%s' "$F" | grep -q '^Mem:'; then
  printf 'MEMFMT:free-b\n'; printf '%s\n' "$F"
else
  printf 'MEMFMT:meminfo\n'; cat /proc/meminfo 2>/dev/null
fi
echo __SUB_INFO_OV_SEP__
# Disks. `-T` (fs-type column) + `-k` (1 KiB blocks) is GNU coreutils; busybox
# df lacks `-T`, so fall back to the no-type layout. `-k` pins the block size
# so the frontend's *1024 is always correct.
if D=$(df -PTk 2>/dev/null) && [ -n "$D" ]; then
  printf 'DFFMT:pt\n'; printf '%s\n' "$D"
else
  printf 'DFFMT:p\n'; df -Pk 2>/dev/null
fi
echo __SUB_INFO_OV_SEP__
# CPU count: coreutils nproc -> POSIX getconf -> /proc/cpuinfo (universal floor).
nproc 2>/dev/null || getconf _NPROCESSORS_ONLN 2>/dev/null || grep -c '^processor' /proc/cpuinfo 2>/dev/null
echo __SUB_INFO_OV_SEP__
cat /proc/loadavg 2>/dev/null
echo __SUB_INFO_OV_SEP__
"#;

// Network probe layout (sections delimited by SEP):
//   [0] empty (printed before first SEP)
//   [1] NIC list   — first line `ADDRFMT:<ip-json|ip-oneline|ifconfig|none>`
//   [2] route table — first line `ROUTEFMT:<ip-json|ip-oneline|route|netstat|none>`
//   [3] firewall summary:
//         line 1: 'FW:<engine>' where engine is one of
//                 iptables | iptables-sudo | iptables-denied |
//                 nft      | nft-sudo      | nft-denied      | none
//         lines 2..N: per-chain summary rows, not the full ruleset —
//                 iptables: 'table|chain|policy|count'
//                 nft:      'family|table|chain|type|count'
//         The full ruleset for a specific chain is fetched lazily on
//         expand via ssh_iptables_chain / ssh_nft_chain — the wire cost
//         of first paint drops from ~50-500 KB to ~1-2 KB, which is the
//         single biggest bandwidth win for low-bandwidth SSH users.
//   [4] DNS resolvers — first line `DNSFMT:list`, then raw lines from
//         resolvectl / systemd-resolve / resolv.conf; the frontend extracts
//         IPs, dedupes, and flags the systemd-resolved 127.0.0.53 stub.
// The firewall block tries the user's own credentials first, falls back to
// non-interactive sudo (`sudo -n`) so we never hang waiting for a password,
// and prefers iptables over nft because iptables-nft on modern Debian-family
// systems still reports both — picking iptables gives a consistent first
// hit even on hybrid hosts.
const INFO_SCRIPT_NETWORK: &str = r#"echo __SUB_INFO_NET_SEP__
# NIC addresses. First line is a format tag the frontend switches on. `ip -j`
# (JSON) needs iproute2 >= 4.13, so RHEL/CentOS 7 (4.11) falls back to the
# text `ip -o` layout, then net-tools ifconfig for hosts without `ip` at all.
if J=$(ip -j addr 2>/dev/null) && [ -n "$J" ]; then
  printf 'ADDRFMT:ip-json\n'; printf '%s\n' "$J"
elif command -v ip >/dev/null 2>&1; then
  printf 'ADDRFMT:ip-oneline\n'
  ip -o link show 2>/dev/null
  echo __SUB_INFO_NET_SUB__
  ip -o addr show 2>/dev/null
elif command -v ifconfig >/dev/null 2>&1; then
  printf 'ADDRFMT:ifconfig\n'; ifconfig -a 2>/dev/null
else
  printf 'ADDRFMT:none\n'
fi
echo __SUB_INFO_NET_SEP__
# Routes, same tag+fallback idea.
if J=$(ip -j route 2>/dev/null) && [ -n "$J" ]; then
  printf 'ROUTEFMT:ip-json\n'; printf '%s\n' "$J"
elif command -v ip >/dev/null 2>&1; then
  printf 'ROUTEFMT:ip-oneline\n'; ip -o route show 2>/dev/null
elif command -v route >/dev/null 2>&1; then
  printf 'ROUTEFMT:route\n'; route -n 2>/dev/null
elif command -v netstat >/dev/null 2>&1; then
  printf 'ROUTEFMT:netstat\n'; netstat -rn 2>/dev/null
else
  printf 'ROUTEFMT:none\n'
fi
echo __SUB_INFO_NET_SEP__
if command -v iptables >/dev/null 2>&1; then
  IPT=""
  if iptables -t filter -n -L >/dev/null 2>&1; then
    IPT="iptables"; printf 'FW:iptables\n'
  elif sudo -n iptables -t filter -n -L >/dev/null 2>&1; then
    IPT="sudo -n iptables"; printf 'FW:iptables-sudo\n'
  else
    printf 'FW:iptables-denied\n'
  fi
  if [ -n "$IPT" ]; then
    for T in filter nat mangle raw; do
      $IPT -t $T -n -L --line-numbers 2>/dev/null | awk -v t="$T" '
        /^Chain / {
          if (chain != "") print t "|" chain "|" policy "|" count
          chain=$2; count=0; policy="-"
          if ($3 == "(policy") policy=$4
          next
        }
        /^num/ { next }
        /^$/ {
          if (chain != "") { print t "|" chain "|" policy "|" count; chain="" }
          next
        }
        chain != "" && NF > 0 { count++ }
        END { if (chain != "") print t "|" chain "|" policy "|" count }
      '
    done
  fi
elif command -v nft >/dev/null 2>&1; then
  NFT=""
  if nft list ruleset >/dev/null 2>&1; then
    NFT="nft"; printf 'FW:nft\n'
  elif sudo -n nft list ruleset >/dev/null 2>&1; then
    NFT="sudo -n nft"; printf 'FW:nft-sudo\n'
  else
    printf 'FW:nft-denied\n'
  fi
  if [ -n "$NFT" ]; then
    $NFT list ruleset 2>/dev/null | awk '
      /^table / { fam=$2; tbl=$3; chain=""; next }
      /^\tchain / { chain=$2; type=""; count=0; next }
      /^\tset / || /^\tmap / || /^\tflowtable / || /^\tct / {
        if (chain != "") { print fam "|" tbl "|" chain "|" type "|" count; chain="" }
        chain=""; next
      }
      /^\t}/ {
        if (chain != "") { print fam "|" tbl "|" chain "|" type "|" count; chain="" }
        next
      }
      chain == "" { next }
      $1 == "type" { type=$2; next }
      $1 == "hook" || $1 == "policy" || $1 == "priority" || $1 == "flags" || $1 == "device" { next }
      NF > 0 { count++ }
    '
  fi
else
  printf 'FW:none\n'
fi
echo __SUB_INFO_NET_SEP__
# DNS resolvers. systemd-resolved stubs /etc/resolv.conf at 127.0.0.53 and hides
# the real upstreams behind resolvectl, so we gather from BOTH sources and let
# the frontend extract IPs, dedupe, and flag the local stub. `systemd-resolve`
# is the pre-239 name for resolvectl. Always emits the DNSFMT tag so the
# frontend can tell "no resolvers found" from "section missing".
printf 'DNSFMT:list\n'
if command -v resolvectl >/dev/null 2>&1; then
  resolvectl dns 2>/dev/null
elif command -v systemd-resolve >/dev/null 2>&1; then
  systemd-resolve --status 2>/dev/null | grep -iE 'DNS Servers|Current DNS Server'
fi
if [ -r /etc/resolv.conf ]; then
  grep -E '^[[:space:]]*nameserver[[:space:]]' /etc/resolv.conf 2>/dev/null
fi
"#;

// Ports: ss is preferred (parseable, modern). Falls back to netstat. The -p
// flag returns process info for sockets the user owns; non-root users see
// blank process columns for foreign sockets — that's a permission limit, not
// an error. We surface it gracefully on the UI.
const INFO_SCRIPT_PORTS: &str = r#"if command -v ss >/dev/null 2>&1; then
  printf 'ENGINE:ss\n'
  ss -tuln -p 2>/dev/null
elif command -v netstat >/dev/null 2>&1; then
  printf 'ENGINE:netstat\n'
  netstat -tulnp 2>/dev/null
else
  printf 'ENGINE:none\n'
fi
"#;

// Process list for the Processes tab. First line is a format tag the frontend
// switches on. Prefer the procps `-eo` custom format (Linux); fall back to
// BSD-style `ps aux` for busybox / minimal userlands. Sorting + capping happen
// client-side so the probe stays a single cheap snapshot suitable for a ~1s
// auto-refresh.
const INFO_SCRIPT_PROCESSES: &str = r#"if ps -eo pid,user,pcpu,pmem,rss,comm >/dev/null 2>&1; then
  printf 'PSFMT:full\n'
  ps -eo pid,user,pcpu,pmem,rss,comm 2>/dev/null
elif ps aux >/dev/null 2>&1; then
  printf 'PSFMT:aux\n'
  ps aux 2>/dev/null
else
  printf 'PSFMT:none\n'
  ps 2>/dev/null
fi
"#;

const INFO_SCRIPT_SERVICES: &str = r#"if command -v systemctl >/dev/null 2>&1; then
  printf 'ENGINE:systemd\n'
  systemctl list-units --type=service --no-legend --plain --no-pager --state=running,failed,activating 2>/dev/null
else
  printf 'ENGINE:none\n'
fi
"#;

const INFO_SCRIPT_DOCKER: &str = r#"if ! command -v docker >/dev/null 2>&1; then
  printf 'DOCKER:missing\n'
  exit 0
fi
if ! docker info >/dev/null 2>&1; then
  printf 'DOCKER:denied\n'
  exit 0
fi
printf 'DOCKER:ok\n'
echo __SUB_INFO_DOCK_SEP__
docker ps -a --format '{{json .}}' 2>/dev/null
echo __SUB_INFO_DOCK_SEP__
docker volume ls --format '{{json .}}' 2>/dev/null
echo __SUB_INFO_DOCK_SEP__
docker images --format '{{json .}}' 2>/dev/null
echo __SUB_INFO_DOCK_SEP__
docker system df 2>/dev/null
"#;

#[derive(serde::Serialize, Default)]
struct InfoSectionResult {
    data: String,
    truncated: bool,
    exec_ms: u64,
}

// Run a script through a fresh exec channel on the live SSH session and
// return everything it printed (up to a 1 MB cap). Stays read-only — the
// callers in this module only invoke shell built-ins and inspection tools.
async fn run_info_script(
    state: &SshState,
    session_id: &str,
    script: &str,
    timeout_secs: u64,
) -> Result<InfoSectionResult, String> {
    use tokio::io::AsyncReadExt;
    use std::sync::Arc;

    let session_arc = {
        let connections = state.connections.lock().await;
        connections.get(session_id).map(Arc::clone)
            .ok_or_else(|| "Session not connected".to_string())?
    };

    let start = std::time::Instant::now();
    let channel = {
        let session = session_arc.lock().await;
        session.channel_open_session().await.map_err(|e| e.to_string())?
    };
    channel.exec(true, script.as_bytes()).await.map_err(|e| e.to_string())?;
    let mut stream = channel.into_stream();

    // 1 MB cap — defends the UI against pathological output (e.g. a server
    // with thousands of veth interfaces or hundreds of stopped containers).
    const MAX_BYTES: usize = 1024 * 1024;
    let mut buf: Vec<u8> = Vec::with_capacity(64 * 1024);
    let mut truncated = false;
    let read_fut = async {
        let mut tmp = [0u8; 8192];
        loop {
            match stream.read(&mut tmp).await {
                Ok(0) => break,
                Ok(n) => {
                    if buf.len() + n > MAX_BYTES {
                        let remaining = MAX_BYTES.saturating_sub(buf.len());
                        if remaining > 0 {
                            buf.extend_from_slice(&tmp[..remaining]);
                        }
                        truncated = true;
                        let mut sink = [0u8; 8192];
                        while stream.read(&mut sink).await.unwrap_or(0) > 0 {}
                        break;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                }
                Err(_) => break,
            }
        }
    };
    tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), read_fut)
        .await
        .map_err(|_| format!("probe timed out after {}s", timeout_secs))?;

    Ok(InfoSectionResult {
        data: String::from_utf8_lossy(&buf).into_owned(),
        truncated,
        exec_ms: start.elapsed().as_millis() as u64,
    })
}

#[tauri::command]
async fn ssh_info_probe_section(
    state: tauri::State<'_, SshState>,
    session_id: String,
    section: String,
) -> Result<InfoSectionResult, String> {
    let (script, timeout) = match section.as_str() {
        "overview" => (INFO_SCRIPT_OVERVIEW, 10u64),
        "network"  => (INFO_SCRIPT_NETWORK, 10u64),
        "ports"    => (INFO_SCRIPT_PORTS, 10u64),
        "processes" => (INFO_SCRIPT_PROCESSES, 10u64),
        "services" => (INFO_SCRIPT_SERVICES, 15u64),
        "docker"   => (INFO_SCRIPT_DOCKER, 20u64),
        other => return Err(format!("unknown info section: {}", other)),
    };
    run_info_script(&state, &session_id, script, timeout).await
}

// Validate an iptables/nft table/chain identifier — strict allow-list.
// Chain names on real hosts are alphanumeric plus `-` `_` `.` and occasional
// `@`; anything else would risk shell injection when we splice into the
// command string. This function is the *only* thing keeping the below
// commands from executing arbitrary shell.
fn is_safe_fw_ident(s: &str) -> bool {
    if s.is_empty() || s.len() > 128 { return false; }
    s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '@'))
}

// Two-stage firewall probe (lazy detail fetch). The network section returns
// a per-chain summary only; this command pulls the raw rules for one chain
// on user expand. Wire cost stays proportional to what the user actually
// looks at, not the size of the whole ruleset.
#[tauri::command]
async fn ssh_iptables_chain(
    state: tauri::State<'_, SshState>,
    session_id: String,
    table: String,
    chain: String,
) -> Result<String, String> {
    // Locked table allow-list; anything not here is rejected. Matches what
    // the probe script iterates over, plus `security` which some hardened
    // distros expose.
    if !matches!(table.as_str(), "filter" | "nat" | "mangle" | "raw" | "security") {
        return Err("invalid table".to_string());
    }
    if !is_safe_fw_ident(&chain) {
        return Err("invalid chain name".to_string());
    }
    // Retry with `sudo -n` when the direct call comes back empty — mirrors
    // the semantics of the summary probe so the expand behaves consistently
    // on hosts where only root can read the ruleset.
    let cmd = format!(
        "OUT=$(iptables -t {t} -n -L {c} --line-numbers 2>/dev/null); \
if [ -z \"$OUT\" ]; then OUT=$(sudo -n iptables -t {t} -n -L {c} --line-numbers 2>/dev/null); fi; \
printf '%s' \"$OUT\"",
        t = table, c = chain
    );
    run_exec_capture(&state, &session_id, &cmd, 15).await
}

#[tauri::command]
async fn ssh_nft_chain(
    state: tauri::State<'_, SshState>,
    session_id: String,
    family: String,
    table: String,
    chain: String,
) -> Result<String, String> {
    if !matches!(family.as_str(), "ip" | "ip6" | "inet" | "arp" | "bridge" | "netdev") {
        return Err("invalid family".to_string());
    }
    if !is_safe_fw_ident(&table) || !is_safe_fw_ident(&chain) {
        return Err("invalid table or chain name".to_string());
    }
    let cmd = format!(
        "OUT=$(nft list chain {f} {t} {c} 2>/dev/null); \
if [ -z \"$OUT\" ]; then OUT=$(sudo -n nft list chain {f} {t} {c} 2>/dev/null); fi; \
printf '%s' \"$OUT\"",
        f = family, t = table, c = chain
    );
    run_exec_capture(&state, &session_id, &cmd, 15).await
}

#[derive(serde::Serialize)]
struct SystemctlActionResult {
    success: bool,
    exit_code: i32,
    stdout: String,
    stderr: String,
    used_sudo: bool,
}

async fn run_exec_capture(
    state: &SshState,
    session_id: &str,
    cmd: &str,
    timeout_secs: u64,
) -> Result<String, String> {
    use tokio::io::AsyncReadExt;
    use std::sync::Arc;

    let session_arc = {
        let connections = state.connections.lock().await;
        connections.get(session_id).map(Arc::clone)
            .ok_or_else(|| "Session not connected".to_string())?
    };
    let channel = {
        let session = session_arc.lock().await;
        session.channel_open_session().await.map_err(|e| e.to_string())?
    };
    channel.exec(true, cmd.as_bytes()).await.map_err(|e| e.to_string())?;
    let mut stream = channel.into_stream();
    let mut buf: Vec<u8> = Vec::with_capacity(4096);
    let read_fut = async {
        let mut tmp = [0u8; 4096];
        loop {
            match stream.read(&mut tmp).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if buf.len() + n > 64 * 1024 { break; }
                    buf.extend_from_slice(&tmp[..n]);
                }
            }
        }
    };
    tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), read_fut)
        .await
        .map_err(|_| format!("exec timed out after {}s", timeout_secs))?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

fn parse_exit_marker(raw: &str) -> (i32, String) {
    if let Some(idx) = raw.rfind("__SUB_EXITCODE:") {
        let after = &raw[idx + "__SUB_EXITCODE:".len()..];
        let code = after.trim().split_whitespace().next().unwrap_or("1").parse().unwrap_or(1);
        let out = raw[..idx].trim_end_matches('\n').to_string();
        (code, out)
    } else {
        (1, raw.trim_end_matches('\n').to_string())
    }
}

#[tauri::command]
async fn ssh_systemctl_action(
    state: tauri::State<'_, SshState>,
    session_id: String,
    unit: String,
    action: String,
) -> Result<SystemctlActionResult, String> {
    // Locked allow-list. Anything not in this list is rejected outright
    // — we never want to dispatch arbitrary subcommands from the UI.
    let valid_action = matches!(action.as_str(), "start" | "stop" | "restart" | "reload" | "status");
    if !valid_action {
        return Err(format!("invalid action: {}", action));
    }
    // Reject hostile unit names. systemd unit names are restricted to
    // `[A-Za-z0-9:_.\\@-]+\.<suffix>` — adding any shell metachar here would
    // otherwise let the caller smuggle in a command.
    if unit.is_empty()
        || unit.len() > 256
        || !unit.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '@' | ':' | '\\'))
    {
        return Err("invalid unit name".to_string());
    }

    // Try plain systemctl first (works if user is root or has polkit rules).
    // If that fails with permission-denied semantics, retry with `sudo -n` —
    // works on hosts that gave this user NOPASSWD sudo. The `-n` ensures we
    // never block on a tty password prompt that nobody can answer.
    let plain_cmd = format!("systemctl {} {} 2>&1; echo __SUB_EXITCODE:$?", action, unit);
    let raw1 = run_exec_capture(&state, &session_id, &plain_cmd, 30).await?;
    let (code1, out1) = parse_exit_marker(&raw1);
    let needs_sudo = code1 != 0 && (
        out1.contains("Interactive authentication required")
        || out1.contains("not authorized")
        || out1.contains("polkit")
        || out1.to_lowercase().contains("permission denied")
        || out1.to_lowercase().contains("access denied")
    );

    if code1 == 0 || !needs_sudo {
        return Ok(SystemctlActionResult {
            success: code1 == 0,
            exit_code: code1,
            stdout: out1,
            stderr: String::new(),
            used_sudo: false,
        });
    }

    let sudo_cmd = format!("sudo -n systemctl {} {} 2>&1; echo __SUB_EXITCODE:$?", action, unit);
    let raw2 = run_exec_capture(&state, &session_id, &sudo_cmd, 30).await?;
    let (code2, out2) = parse_exit_marker(&raw2);
    Ok(SystemctlActionResult {
        success: code2 == 0,
        exit_code: code2,
        stdout: out2,
        stderr: String::new(),
        used_sudo: true,
    })
}

/// Signal a process by PID (Ports / Processes tab "kill" action). Tries the
/// user's own `kill` first, then `sudo -n kill` on a permission error — same
/// non-interactive sudo pattern as ssh_systemctl_action. Both the PID (a
/// validated i32) and the signal (an allow-listed name) are interpolated into
/// the shell command, so neither is attacker-controlled free text.
#[tauri::command]
async fn ssh_kill_process(
    state: tauri::State<'_, SshState>,
    session_id: String,
    pid: i32,
    signal: Option<String>,
) -> Result<SystemctlActionResult, String> {
    // Never signal PID <= 1 — that's init / the kernel and would be catastrophic.
    if pid <= 1 {
        return Err("refusing to signal PID <= 1".to_string());
    }
    let sig = signal.unwrap_or_else(|| "TERM".to_string());
    if !matches!(sig.as_str(), "TERM" | "KILL" | "HUP" | "INT") {
        return Err(format!("invalid signal: {}", sig));
    }

    let plain = format!("kill -{} {} 2>&1; echo __SUB_EXITCODE:$?", sig, pid);
    let raw1 = run_exec_capture(&state, &session_id, &plain, 15).await?;
    let (code1, out1) = parse_exit_marker(&raw1);
    let low = out1.to_lowercase();
    let needs_sudo = code1 != 0
        && (low.contains("not permitted")
            || low.contains("permission denied")
            || low.contains("operation not permitted"));

    if code1 == 0 || !needs_sudo {
        return Ok(SystemctlActionResult {
            success: code1 == 0,
            exit_code: code1,
            stdout: out1,
            stderr: String::new(),
            used_sudo: false,
        });
    }

    let sudo_cmd = format!("sudo -n kill -{} {} 2>&1; echo __SUB_EXITCODE:$?", sig, pid);
    let raw2 = run_exec_capture(&state, &session_id, &sudo_cmd, 15).await?;
    let (code2, out2) = parse_exit_marker(&raw2);
    Ok(SystemctlActionResult {
        success: code2 == 0,
        exit_code: code2,
        stdout: out2,
        stderr: String::new(),
        used_sudo: true,
    })
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

    // Transport preference: when a dedicated `::sftp` connection exists for
    // this session (the per-tab "Dedicated session" toggle), the SFTP
    // subsystem rides IT — but the cache stays keyed by the plain session id,
    // so the frontend never has to re-key anything. The dedicated connection
    // is a pure transport: it appearing/disappearing just invalidates this
    // cache (see the `::sftp` lifecycle hooks) and the next file operation
    // re-opens the subsystem on whatever transport is available.
    let (transport_key, session_arc) = {
        let connections = state.connections.lock().await;
        let dedicated_key = format!("{}::sftp", session_id);
        if !session_id.contains("::") && connections.contains_key(&dedicated_key) {
            (dedicated_key.clone(), Arc::clone(connections.get(&dedicated_key).unwrap()))
        } else if let Some(sess) = connections.get(session_id) {
            (session_id.to_string(), Arc::clone(sess))
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
    // Guard against two races before caching: (a) a reconnect replaced/removed
    // the transport handle while we were opening the subsystem (stale handle),
    // and (b) a dedicated `::sftp` transport APPEARED meanwhile — which changes
    // the preferred transport, so caching this primary-backed subsystem would
    // permanently bypass the dedicated connection. Require both the recomputed
    // preference AND the handle identity to still match the key we opened on.
    let still_current = {
        let connections = state.connections.lock().await;
        let dedicated_key = format!("{}::sftp", session_id);
        let preferred: &str = if !session_id.contains("::") && connections.contains_key(&dedicated_key) {
            dedicated_key.as_str()
        } else {
            session_id
        };
        transport_key.as_str() == preferred
            && connections.get(&transport_key)
                .map(|c| Arc::ptr_eq(c, &session_arc))
                .unwrap_or(false)
    };
    if !still_current {
        return Err("Session reconnected while opening SFTP — try again".into());
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
    overwrite: Option<bool>,
) -> Result<(), String> {
    use tauri::Emitter;
    use tokio::io::AsyncReadExt;
    use std::sync::atomic::{AtomicBool, Ordering};

    // Overwrite protection: when the caller has NOT explicitly opted in
    // (overwrite==Some(true)), refuse to clobber an existing local file.
    // The sentinel error string `EXISTS:<path>` lets the frontend tell
    // the difference between "real failure" and "you would have replaced
    // something" and surface the apply-to-all confirmation modal.
    if overwrite != Some(true) && std::path::Path::new(&local_path).exists() {
        return Err(format!("EXISTS:{}", local_path));
    }

    // Validate the destination BEFORE touching the network. A compromised
    // renderer (or a malicious SFTP server name in the UI) could otherwise
    // request a download into a system directory like `/etc` or
    // `C:\Windows\System32\…`. allow_nonexistent=true because the
    // destination file is being created right now.
    let _guarded_local = guard_local_path(&local_path, true)?;

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

    // Register a cancel flag the user can flip via `sftp_cancel_transfer`.
    // RAII-removed at the end so the map doesn't pile up across many
    // sequential transfers.
    let cancel = Arc::new(AtomicBool::new(false));
    let cancels_map = Arc::clone(&state.transfer_cancels);
    cancels_map.lock().await.insert(id.clone(), Arc::clone(&cancel));
    struct CancelGuard {
        map: Arc<tokio::sync::Mutex<std::collections::HashMap<String, Arc<AtomicBool>>>>,
        id: String,
    }
    impl Drop for CancelGuard {
        fn drop(&mut self) {
            // Best-effort cleanup; if the lock is contended we'd rather leak
            // a slot than block the drop, but in practice this never blocks.
            if let Ok(mut g) = self.map.try_lock() {
                g.remove(&self.id);
            }
        }
    }
    let _guard = CancelGuard { map: Arc::clone(&cancels_map), id: id.clone() };

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
    // progress meter (and the cancel poll).
    let mut buf = vec![0u8; 256 * 1024];
    let mut transferred: u64 = 0;
    let mut last_report = std::time::Instant::now();
    use tokio::io::AsyncWriteExt;
    loop {
        if cancel.load(Ordering::Relaxed) {
            // Drop the partial local file so we don't leave a half-baked
            // download behind; ignore errors (e.g. on Windows file-locking).
            drop(local_file);
            let _ = tokio::fs::remove_file(&local_path).await;
            emit_progress(transferred, "cancelled", None);
            return Err("cancelled".into());
        }
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

/// Recursive remote directory download. Walks the remote tree under
/// `remote_path`, mirrors its structure into `local_path/{basename}`, and
/// streams every file across with progress events aggregated under a SINGLE
/// transfer id — so the UI shows one "folder of N files" card instead of one
/// card per file. Cancellation uses the same `transfer_cancels` map as
/// single-file downloads, so the user's Cancel button works identically.
#[tauri::command]
async fn sftp_download_dir(
    app: tauri::AppHandle,
    state: tauri::State<'_, SshState>,
    session_id: String,
    remote_path: String,
    local_path: String,
    overwrite: Option<bool>,
) -> Result<(), String> {
    use tauri::Emitter;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use std::sync::atomic::{AtomicBool, Ordering};

    // Destination is the PARENT directory. We'll create remote_path's
    // basename underneath it so the user gets `local/{folder}/...`,
    // matching scp -r and rsync semantics.
    let _guarded_local = guard_local_path(&local_path, true)?;

    // Overwrite protection for the destination folder: if the target
    // `local_path/{folder}` already exists, refuse unless explicitly
    // allowed. Per-file confirmation inside the tree would be unworkable
    // for the bulk path — the user picks once at the directory level.
    {
        let folder_name = remote_path
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or("folder");
        let dst_root = std::path::PathBuf::from(&local_path).join(folder_name);
        if overwrite != Some(true) && dst_root.exists() {
            return Err(format!("EXISTS:{}", dst_root.to_string_lossy()));
        }
    }

    let sftp = get_sftp_session(&state, &session_id).await?;

    // Compute the folder name from the remote path. Strip trailing slashes
    // first so `/home/user/data/` still yields `data`, not "".
    let folder_name = remote_path
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or("folder")
        .to_string();
    let folder_name = if folder_name.is_empty() { "folder".to_string() } else { folder_name };

    let id = transfer_id();
    let event_name = format!("sftp-transfer-{}", session_id);

    // Register cancel flag early so the user can abort even during the
    // enumeration phase (which can be slow on a deep tree with many dirs).
    let cancel = Arc::new(AtomicBool::new(false));
    let cancels_map = Arc::clone(&state.transfer_cancels);
    cancels_map.lock().await.insert(id.clone(), Arc::clone(&cancel));
    struct CancelGuard {
        map: Arc<tokio::sync::Mutex<std::collections::HashMap<String, Arc<AtomicBool>>>>,
        id: String,
    }
    impl Drop for CancelGuard {
        fn drop(&mut self) {
            if let Ok(mut g) = self.map.try_lock() {
                g.remove(&self.id);
            }
        }
    }
    let _guard = CancelGuard { map: Arc::clone(&cancels_map), id: id.clone() };

    let id_for_emit = id.clone();
    let name_for_emit = folder_name.clone();
    let app_for_emit = app.clone();
    let event_for_emit = event_name.clone();
    let emit_progress = move |bytes: u64, total: u64, status: &str, error: Option<String>| {
        let _ = app_for_emit.emit(
            &event_for_emit,
            serde_json::json!({
                "id": id_for_emit, "name": name_for_emit, "kind": "download",
                "bytes": bytes, "total": total,
                "status": status, "error": error,
            }),
        );
    };

    emit_progress(0, 0, "progress", None);

    // Phase 1: enumerate. Collect every file under remote_path along with
    // its relative path (so we can preserve the tree on the local side) and
    // its size (so the progress bar has a meaningful total). Tracked
    // iteratively with an explicit stack so we don't blow the async-recursion
    // budget on pathological trees.
    let remote_root = remote_path.trim_end_matches('/').to_string();
    let mut files: Vec<(String, String, u64)> = Vec::new(); // (remote, rel, size)
    let mut total_bytes: u64 = 0;
    let mut stack: Vec<String> = vec![remote_root.clone()];

    while let Some(dir) = stack.pop() {
        if cancel.load(Ordering::Relaxed) {
            emit_progress(0, total_bytes, "cancelled", None);
            return Err("cancelled".into());
        }
        let read = match sftp.read_dir(&dir).await {
            Ok(r) => r,
            Err(e) => {
                emit_progress(0, total_bytes, "error", Some(format!("read_dir {}: {}", dir, e)));
                return Err(format!("read_dir {}: {}", dir, e));
            }
        };
        for entry in read {
            let name = entry.file_name();
            // Skip any entry whose name isn't a single plain component. A
            // hostile SFTP server can return `../../x` or `..\x` here; joining
            // that onto local_root below would escape the chosen folder
            // (zip-slip → arbitrary local write). See is_safe_dir_entry_name.
            if !is_safe_dir_entry_name(&name) { continue; }
            let full = format!("{}/{}", dir.trim_end_matches('/'), name);
            if entry.file_type().is_dir() {
                stack.push(full);
            } else if entry.file_type().is_file() {
                let size = entry.metadata().size.unwrap_or(0);
                let rel = full.strip_prefix(&remote_root)
                    .map(|s| s.trim_start_matches('/').to_string())
                    .unwrap_or_else(|| name.clone());
                total_bytes = total_bytes.saturating_add(size);
                files.push((full, rel, size));
            }
            // Symlinks and other types skipped — same conservative policy
            // as the mirror module.
        }
    }

    if files.is_empty() {
        // Still create the (empty) destination folder so the UI sees the
        // shape — otherwise the user sees "done" with nothing to show for it.
        let local_root = std::path::PathBuf::from(&local_path).join(&folder_name);
        let _ = tokio::fs::create_dir_all(&local_root).await;
        emit_progress(0, 0, "done", None);
        return Ok(());
    }

    // Phase 2: download. The local destination tree is rooted at
    // {local_path}/{folder_name}/... so multi-level files preserve their
    // structure. Create each parent directory lazily right before we open
    // the file.
    let local_root = std::path::PathBuf::from(&local_path).join(&folder_name);
    if let Err(e) = tokio::fs::create_dir_all(&local_root).await {
        emit_progress(0, total_bytes, "error", Some(format!("create {}: {}", local_root.display(), e)));
        return Err(format!("create root: {}", e));
    }

    let mut transferred: u64 = 0;
    let mut last_report = std::time::Instant::now();
    let mut buf = vec![0u8; 256 * 1024];

    for (remote_file_path, rel, _size) in &files {
        if cancel.load(Ordering::Relaxed) {
            emit_progress(transferred, total_bytes, "cancelled", None);
            return Err("cancelled".into());
        }

        // Normalise the relative path's separators for the local OS. On
        // Unix this is a no-op; on Windows we replace `/` so create_dir_all
        // produces real nested directories instead of one literal name
        // containing slashes.
        let rel_local = if cfg!(windows) { rel.replace('/', "\\") } else { rel.clone() };
        let dest = local_root.join(&rel_local);
        if let Some(parent) = dest.parent() {
            if let Err(e) = tokio::fs::create_dir_all(parent).await {
                emit_progress(transferred, total_bytes, "error",
                    Some(format!("mkdir {}: {}", parent.display(), e)));
                return Err(format!("mkdir {}: {}", parent.display(), e));
            }
        }

        let mut remote_file = match sftp.open(remote_file_path).await {
            Ok(f) => f,
            Err(e) => {
                emit_progress(transferred, total_bytes, "error",
                    Some(format!("open {}: {}", remote_file_path, e)));
                return Err(format!("open {}: {}", remote_file_path, e));
            }
        };
        let mut local_file = match tokio::fs::File::create(&dest).await {
            Ok(f) => f,
            Err(e) => {
                emit_progress(transferred, total_bytes, "error",
                    Some(format!("create {}: {}", dest.display(), e)));
                return Err(format!("create {}: {}", dest.display(), e));
            }
        };

        loop {
            if cancel.load(Ordering::Relaxed) {
                drop(local_file);
                let _ = tokio::fs::remove_file(&dest).await;
                emit_progress(transferred, total_bytes, "cancelled", None);
                return Err("cancelled".into());
            }
            let n = remote_file.read(&mut buf).await
                .map_err(|e| {
                    emit_progress(transferred, total_bytes, "error",
                        Some(format!("read {}: {}", remote_file_path, e)));
                    format!("read {}: {}", remote_file_path, e)
                })?;
            if n == 0 { break; }
            local_file.write_all(&buf[..n]).await
                .map_err(|e| {
                    emit_progress(transferred, total_bytes, "error",
                        Some(format!("write {}: {}", dest.display(), e)));
                    format!("write {}: {}", dest.display(), e)
                })?;
            transferred = transferred.saturating_add(n as u64);
            if last_report.elapsed() >= std::time::Duration::from_millis(100) {
                emit_progress(transferred, total_bytes, "progress", None);
                last_report = std::time::Instant::now();
            }
        }
        local_file.flush().await.map_err(|e| format!("flush {}: {}", dest.display(), e))?;
    }

    emit_progress(transferred, total_bytes, "done", None);
    Ok(())
}

#[tauri::command]
async fn sftp_upload_file(
    app: tauri::AppHandle,
    state: tauri::State<'_, SshState>,
    session_id: String,
    local_path: String,
    remote_path: String,
    overwrite: Option<bool>,
) -> Result<(), String> {
    use russh_sftp::protocol::OpenFlags;
    use tauri::Emitter;
    use tokio::io::AsyncWriteExt;
    use std::sync::atomic::{AtomicBool, Ordering};

    // Overwrite protection: stat the remote target first; refuse if it
    // exists and the caller hasn't explicitly opted in. Same `EXISTS:<path>`
    // sentinel as the download path.
    let sftp = get_sftp_session(&state, &session_id).await?;
    if overwrite != Some(true) {
        if sftp.metadata(&remote_path).await.is_ok() {
            return Err(format!("EXISTS:{}", remote_path));
        }
    }

    // Source must exist and live in a path the user could plausibly own —
    // refuses an SFTP push that would exfiltrate /etc/shadow or the SAM
    // hive if the renderer ever gets coerced.
    let _guarded_local = guard_local_path(&local_path, false)?;

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

    // Symmetric to sftp_download_file: register a cancel flag and clean it
    // up via RAII so a long-running upload can be stopped from the UI.
    let cancel = Arc::new(AtomicBool::new(false));
    let cancels_map = Arc::clone(&state.transfer_cancels);
    cancels_map.lock().await.insert(id.clone(), Arc::clone(&cancel));
    struct CancelGuard {
        map: Arc<tokio::sync::Mutex<std::collections::HashMap<String, Arc<AtomicBool>>>>,
        id: String,
    }
    impl Drop for CancelGuard {
        fn drop(&mut self) {
            if let Ok(mut g) = self.map.try_lock() {
                g.remove(&self.id);
            }
        }
    }
    let _guard = CancelGuard { map: Arc::clone(&cancels_map), id: id.clone() };

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
        if cancel.load(Ordering::Relaxed) {
            // Close the remote handle so the server doesn't keep an
            // open-write descriptor for a file we'll never finish.
            let _ = remote_file.shutdown().await;
            emit_progress(transferred, "cancelled", None);
            return Err("cancelled".into());
        }
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

/// Recursive directory upload — mirror of sftp_download_dir. Walks the local
/// tree, mkdirs each subdirectory on the remote, then streams every file
/// through the same flags+chunk logic as sftp_upload_file. Cancel flag and
/// EXISTS sentinel match the download path so the UI can reuse its prompt /
/// progress / abort hooks unchanged.
#[tauri::command]
async fn sftp_upload_dir(
    app: tauri::AppHandle,
    state: tauri::State<'_, SshState>,
    session_id: String,
    local_path: String,
    remote_path: String,
    overwrite: Option<bool>,
) -> Result<(), String> {
    use russh_sftp::protocol::OpenFlags;
    use tauri::Emitter;
    use tokio::io::AsyncWriteExt;
    use std::sync::atomic::{AtomicBool, Ordering};

    // remote_path is the PARENT directory; we hang the basename of
    // local_path underneath it. Matches scp -r / sftp_download_dir.
    let _guarded_local = guard_local_path(&local_path, false)?;

    let local_root = std::path::PathBuf::from(&local_path);
    let folder_name = local_root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("folder")
        .to_string();
    let folder_name = if folder_name.is_empty() { "folder".to_string() } else { folder_name };

    let sftp = get_sftp_session(&state, &session_id).await?;

    // Overwrite gate on the destination folder. Refuse unless the caller
    // explicitly opted in — symmetric with sftp_download_dir.
    let remote_root = format!("{}/{}",
        remote_path.trim_end_matches('/'),
        folder_name);
    if overwrite != Some(true) {
        if sftp.metadata(&remote_root).await.is_ok() {
            return Err(format!("EXISTS:{}", remote_root));
        }
    }

    let id = transfer_id();
    let event_name = format!("sftp-transfer-{}", session_id);

    // Cancel flag registered before the slow enumeration so the user can
    // abort even while we're walking a deep tree.
    let cancel = Arc::new(AtomicBool::new(false));
    let cancels_map = Arc::clone(&state.transfer_cancels);
    cancels_map.lock().await.insert(id.clone(), Arc::clone(&cancel));
    struct CancelGuard {
        map: Arc<tokio::sync::Mutex<std::collections::HashMap<String, Arc<AtomicBool>>>>,
        id: String,
    }
    impl Drop for CancelGuard {
        fn drop(&mut self) {
            if let Ok(mut g) = self.map.try_lock() {
                g.remove(&self.id);
            }
        }
    }
    let _guard = CancelGuard { map: Arc::clone(&cancels_map), id: id.clone() };

    let id_for_emit = id.clone();
    let name_for_emit = folder_name.clone();
    let app_for_emit = app.clone();
    let event_for_emit = event_name.clone();
    let emit_progress = move |bytes: u64, total: u64, status: &str, error: Option<String>| {
        let _ = app_for_emit.emit(
            &event_for_emit,
            serde_json::json!({
                "id": id_for_emit, "name": name_for_emit, "kind": "upload",
                "bytes": bytes, "total": total,
                "status": status, "error": error,
            }),
        );
    };

    emit_progress(0, 0, "progress", None);

    // Phase 1: enumerate. Collect every local file under local_root along
    // with its relative path (POSIX-style for the remote side) and size.
    // Iterative walk with an explicit stack so we never overflow async
    // recursion on pathological trees.
    let mut files: Vec<(std::path::PathBuf, String, u64)> = Vec::new();
    let mut dirs: Vec<String> = Vec::new(); // relative dir paths (POSIX) to mkdir on remote
    let mut total_bytes: u64 = 0;
    let mut stack: Vec<std::path::PathBuf> = vec![local_root.clone()];

    while let Some(dir) = stack.pop() {
        if cancel.load(Ordering::Relaxed) {
            emit_progress(0, total_bytes, "cancelled", None);
            return Err("cancelled".into());
        }
        let read = match std::fs::read_dir(&dir) {
            Ok(r) => r,
            Err(e) => {
                emit_progress(0, total_bytes, "error", Some(format!("read_dir {:?}: {}", dir, e)));
                return Err(format!("read_dir {:?}: {}", dir, e));
            }
        };
        for entry in read {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };
            let path = entry.path();
            let ft = match entry.file_type() {
                Ok(ft) => ft,
                Err(_) => continue,
            };
            let rel = path
                .strip_prefix(&local_root)
                .map(|p| p.to_string_lossy().replace('\\', "/"))
                .unwrap_or_default();
            if ft.is_dir() {
                dirs.push(rel);
                stack.push(path);
            } else if ft.is_file() {
                let size = path.metadata().map(|m| m.len()).unwrap_or(0);
                total_bytes = total_bytes.saturating_add(size);
                files.push((path, rel, size));
            }
            // Symlinks and other types skipped — same as sftp_download_dir.
        }
    }

    // Phase 2: mkdir the destination folder, then each enumerated subdir.
    // SFTP's mkdir is per-level; we already collected them in pre-order from
    // the stack walk but order isn't guaranteed shallow-first, so re-sort
    // by path depth to make sure parents are created before children.
    let _ = sftp.create_dir(&remote_root).await;
    dirs.sort_by_key(|d| d.matches('/').count());
    for rel in &dirs {
        if cancel.load(Ordering::Relaxed) {
            emit_progress(0, total_bytes, "cancelled", None);
            return Err("cancelled".into());
        }
        let full = format!("{}/{}", remote_root.trim_end_matches('/'), rel);
        // Tolerate already-exists — a parallel mkdir or an earlier partial
        // run shouldn't abort the whole upload.
        if sftp.metadata(&full).await.is_err() {
            if let Err(e) = sftp.create_dir(&full).await {
                emit_progress(0, total_bytes, "error", Some(format!("mkdir {}: {}", full, e)));
                return Err(format!("mkdir {}: {}", full, e));
            }
        }
    }

    if files.is_empty() {
        emit_progress(0, 0, "done", None);
        return Ok(());
    }

    // Phase 3: stream each file up. Same chunked loop as sftp_upload_file,
    // looped over the file list with a shared progress counter.
    let mut transferred: u64 = 0;
    let mut last_report = std::time::Instant::now();
    let mut buf = vec![0u8; 256 * 1024];
    use tokio::io::AsyncReadExt;

    for (local_file_path, rel, _size) in &files {
        if cancel.load(Ordering::Relaxed) {
            emit_progress(transferred, total_bytes, "cancelled", None);
            return Err("cancelled".into());
        }
        let remote_full = format!("{}/{}", remote_root.trim_end_matches('/'), rel);

        let mut local_file = match tokio::fs::File::open(local_file_path).await {
            Ok(f) => f,
            Err(e) => {
                emit_progress(transferred, total_bytes, "error",
                    Some(format!("open {:?}: {}", local_file_path, e)));
                return Err(format!("open {:?}: {}", local_file_path, e));
            }
        };
        let mut remote_file = match sftp.open_with_flags(
            remote_full.clone(),
            OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::TRUNCATE,
        ).await {
            Ok(f) => f,
            Err(e) => {
                emit_progress(transferred, total_bytes, "error",
                    Some(format!("open remote {}: {}", remote_full, e)));
                return Err(format!("open remote {}: {}", remote_full, e));
            }
        };

        loop {
            if cancel.load(Ordering::Relaxed) {
                let _ = remote_file.shutdown().await;
                emit_progress(transferred, total_bytes, "cancelled", None);
                return Err("cancelled".into());
            }
            let n = local_file.read(&mut buf).await
                .map_err(|e| {
                    emit_progress(transferred, total_bytes, "error",
                        Some(format!("read {:?}: {}", local_file_path, e)));
                    format!("read {:?}: {}", local_file_path, e)
                })?;
            if n == 0 { break; }
            remote_file.write_all(&buf[..n]).await
                .map_err(|e| {
                    emit_progress(transferred, total_bytes, "error",
                        Some(format!("write {}: {}", remote_full, e)));
                    format!("write {}: {}", remote_full, e)
                })?;
            transferred = transferred.saturating_add(n as u64);
            if last_report.elapsed() >= std::time::Duration::from_millis(100) {
                emit_progress(transferred, total_bytes, "progress", None);
                last_report = std::time::Instant::now();
            }
        }
        remote_file.shutdown().await
            .map_err(|e| format!("shutdown {}: {}", remote_full, e))?;
    }

    emit_progress(transferred, total_bytes, "done", None);
    Ok(())
}

/// Flip the cancel flag for an in-flight SFTP transfer. The download / upload
/// loop polls the flag every chunk (every ~256 KiB) and exits with a
/// "cancelled" status event as soon as it sees true. Unknown ids are a no-op
/// — by the time the UI's stop button click reaches us the transfer may have
/// already finished on its own.
#[tauri::command]
async fn sftp_cancel_transfer(
    state: tauri::State<'_, SshState>,
    transfer_id: String,
) -> Result<(), String> {
    if let Some(flag) = state.transfer_cancels.lock().await.get(&transfer_id) {
        flag.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    Ok(())
}

/// Bucket the failure modes that can come out of an SSH connect / auth round.
/// Drives both the UI's `is_auth_error` flag (so auto-reconnect only stops
/// on real credential rejection) and the wording of the error message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConnectErrorKind {
    /// Server refused the credentials we presented.
    Auth,
    /// Couldn't agree on KEX / cipher / MAC / host-key algorithm with the
    /// peer. Server is reachable, our crypto preference set doesn't overlap.
    Algorithm,
    /// Host key issue — mismatched, declined by the user, or the prompt
    /// timed out. Distinct from auth: credentials never even got tried.
    HostKey,
    /// Transport-level drop: TCP reset, EOF, peer disconnect, IO error.
    Transport,
    /// Server didn't respond inside our window (handshake / auth / probe).
    Timeout,
    /// We couldn't even reach the box: DNS lookup failed, route missing,
    /// connection refused.
    Unreachable,
    /// Anything russh surfaced that we don't have a categorical bucket for.
    /// Treated as non-auth so the UI keeps trying — a real credential
    /// rejection has a specific variant for it.
    Unknown,
}

impl ConnectErrorKind {
    fn is_auth(self) -> bool {
        matches!(self, Self::Auth)
    }
}

/// Map a russh error onto a `ConnectErrorKind`. Uses enum variants when the
/// information is available (russh 0.40 exposes them all), falling back to
/// a string sniff only for the catch-all `_ =>` branch — so a russh upgrade
/// that adds new variants degrades gracefully instead of misreporting them
/// as auth failures.
fn classify_russh_error(e: &russh::Error) -> ConnectErrorKind {
    use russh::Error::*;
    match e {
        // Real credential rejection — the only path that should set
        // is_auth_error so the UI stops auto-retrying.
        NotAuthenticated | NoAuthMethod => ConnectErrorKind::Auth,

        // Algorithm negotiation — server's reachable, we just don't share
        // the cipher / KEX / etc. it asked for.
        NoCommonCipher | NoCommonKexAlgo | NoCommonKeyAlgo
        | NoCommonCompression | NoCommonMac
        | UnknownAlgo | UnknownKey => ConnectErrorKind::Algorithm,

        // Host-key flow: the server's signature didn't verify. KeyChanged
        // carries data so it falls through to the catch-all branch which
        // sniffs the string.
        WrongServerSig => ConnectErrorKind::HostKey,

        // Transport-level: connection died mid-protocol.
        IO(_) | HUP | Disconnect | SendError => ConnectErrorKind::Transport,

        // Explicit timeouts from russh.
        ConnectionTimeout | Elapsed(_) => ConnectErrorKind::Timeout,

        // Protocol disagreements that aren't algorithm- or auth-shaped:
        // version skew, packet integrity, decryption — surface as transport
        // so the user is told "connection broke" not "password wrong".
        // `StrictKeyExchangeViolation` / `ChannelOpenFailure` carry data;
        // the `_ =>` fallthrough handles those via the string sniff below.
        Version | Kex | PacketAuth | Inconsistent | IndexOutOfBounds
        | DecryptionError | KexInit
        | WrongChannel | Pending => ConnectErrorKind::Transport,

        // Key-file problems (local cert can't be parsed). Tag as Auth-shaped
        // so the UI doesn't auto-retry a key that will keep failing.
        CouldNotReadKey | Keys(_) => ConnectErrorKind::Auth,

        // Last-resort string sniff for anything russh adds in future
        // versions or for io::Error subtypes the explicit arms above
        // missed. Default to Unknown which the driver treats as non-auth.
        _ => {
            let lc = e.to_string().to_lowercase();
            if lc.contains("connection refused")
                || lc.contains("no route")
                || lc.contains("network is unreachable")
                || lc.contains("dns")
            {
                ConnectErrorKind::Unreachable
            } else if lc.contains("timed out") || lc.contains("timeout") {
                ConnectErrorKind::Timeout
            } else if lc.contains("io error") || lc.contains("eof")
                || lc.contains("disconnect") || lc.contains("reset")
                || lc.contains("broken pipe") || lc.contains("aborted")
            {
                ConnectErrorKind::Transport
            } else if lc.contains("not authenticated") || lc.contains("auth method") {
                ConnectErrorKind::Auth
            } else {
                ConnectErrorKind::Unknown
            }
        }
    }
}

/// Human-readable phrase for an error bucket. Combined with `target` to
/// build the reason string the UI shows next to a failed connection.
fn describe_error_kind(kind: ConnectErrorKind, target: &str) -> String {
    match kind {
        ConnectErrorKind::Auth =>
            "Authentication rejected by server (wrong password, missing key, or account locked).".into(),
        ConnectErrorKind::Algorithm =>
            format!("Negotiation with {} failed: no SSH algorithm in common (this build might be missing legacy ciphers — try the release build).", target),
        ConnectErrorKind::HostKey =>
            "Host key was not approved (wrong key, declined, or the fingerprint prompt timed out).".into(),
        ConnectErrorKind::Transport =>
            format!("Connection to {} dropped mid-handshake.", target),
        ConnectErrorKind::Timeout =>
            format!("{} did not respond in time.", target),
        ConnectErrorKind::Unreachable =>
            format!("Could not reach {} (DNS lookup, route, or port refusal).", target),
        ConnectErrorKind::Unknown =>
            format!("Connection to {} failed for an unrecognised reason.", target),
    }
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

/// Process-global, UNPREDICTABLE temp root for SFTP live-edit / drag staging,
/// created once with a random name (and 0700 on Unix). The old code used a
/// fully predictable `submarine_sftp_<session_id>` directory directly in the
/// world-writable, sticky-bit /tmp — on a shared host a local attacker could
/// pre-create it (owning it) before the victim opened a remote file, capturing
/// the downloaded (possibly sensitive) contents or redirecting the write via a
/// planted symlink. A random, non-guessable root the attacker cannot pre-create
/// closes that; per-session subdirs live under it and are removed by name.
fn app_temp_root() -> &'static std::path::PathBuf {
    static ROOT: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    ROOT.get_or_init(|| {
        let mut bytes = [0u8; 12];
        rand::thread_rng().fill(&mut bytes);
        let root = std::env::temp_dir().join(format!("submarine-{}", hex::encode(bytes)));
        let _ = std::fs::create_dir_all(&root);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700));
        }
        root
    })
}

/// Per-session live-edit staging dir, under the unpredictable root.
fn session_sftp_dir(session_id: &str) -> std::path::PathBuf {
    app_temp_root().join(format!("sftp_{}", session_id))
}

/// Per-session drag staging dir, under the unpredictable root.
fn session_drag_dir(session_id: &str) -> std::path::PathBuf {
    app_temp_root().join(format!("drag_{}", session_id))
}

/// Reduce a server-controlled remote path to a safe LOCAL leaf filename for
/// temp staging. Rejects empty / "." / ".." and any name containing a path
/// separator, a drive marker (`:`), or NUL — the exact set that would let a
/// hostile/compromised SFTP server escape the per-session temp dir. On Windows
/// a leaf like "C:evil.txt" is drive-relative: `PathBuf::join` discards the
/// base and resolves it against the process CWD, writing (and auto-opening)
/// attacker bytes outside the sandbox. One gate for both live-edit and drag.
fn safe_temp_leaf_name(remote_path: &str) -> Result<String, String> {
    let raw = std::path::Path::new(remote_path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    let bad = |c: char| matches!(c, '/' | '\\' | ':' | '\0');
    if raw.is_empty() || raw == "." || raw == ".." || raw.contains(bad) {
        return Err(format!("refusing file with unsafe name: {:?}", raw));
    }
    Ok(raw.to_string())
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
    let filename = safe_temp_leaf_name(&remote_path)?;

    // Read file data
    let data = sftp.read(&remote_path).await.map_err(|e| format!("Failed to read remote file: {}", e))?;

    // Per-session subdirectory so we can sweep everything cleanly on
    // disconnect rather than leaving loose `submarine_sftp_*` files in the global
    // temp dir. The directory is also a smaller blast radius for any path-
    // related shenanigans (each editor sees only files from one session).
    let session_temp_dir = session_sftp_dir(&session_id);
    std::fs::create_dir_all(&session_temp_dir)
        .map_err(|e| format!("Failed to create temp dir: {}", e))?;
    let temp_file_path = session_temp_dir.join(&filename);
    std::fs::write(&temp_file_path, &data).map_err(|e| format!("Failed to write temporary file: {}", e))?;

    // Open local temp file in system default application. The whole
    // live-edit-in-default-editor feature is desktop-only — Android's
    // intent-based "open with" model would need a Tauri plugin instead,
    // and saving back through SAF doesn't fit the temp-file pattern we
    // depend on. Refuse cleanly so the UI can surface a polite message.
    #[cfg(target_os = "android")]
    {
        let _ = &temp_file_path;
        return Err("Live edit in system editor is not available on Android.".into());
    }
    #[cfg(not(target_os = "android"))]
    {
        let open_res = open::that(&temp_file_path);
        if let Err(e) = open_res {
            return Err(format!("Failed to open file: {}", e));
        }
    }

    // Spawn modification watcher task in background
    let connections_clone = Arc::clone(&state.connections);
    let app_handle_clone = app_handle.clone();
    let session_id_clone = session_id.clone();
    let remote_path_clone = remote_path.clone();
    let filename_clone = filename.clone();
    let temp_file_path_clone = temp_file_path.clone();

    tokio::spawn(async move {
        // Notify-driven save detection instead of the old 1.5s poll. Wires
        // the same `notify-debouncer-mini` crate the mirror module uses:
        //   - std::mpsc::Sender feeds the debouncer (a blocking pool task
        //     forwards into a tokio mpsc so this async loop can await it)
        //   - 750ms debounce coalesces an editor's swap-then-rename
        //     save pattern (vim, vscode) into one upload instead of
        //     several. The previous polling burned a syscall every
        //     1.5s for up to 4800 iterations per open file.
        use notify_debouncer_mini::{new_debouncer, notify::RecursiveMode, DebouncedEventKind};
        let (raw_tx, raw_rx) = std::sync::mpsc::channel();
        let (tok_tx, mut tok_rx) = tokio::sync::mpsc::channel::<()>(8);
        let mut debouncer = match new_debouncer(std::time::Duration::from_millis(750), raw_tx) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("[sftp-live-edit] debouncer init failed: {} — falling back to no autosync", e);
                let _ = std::fs::remove_file(&temp_file_path_clone);
                return;
            }
        };
        if let Err(e) = debouncer.watcher().watch(&temp_file_path_clone, RecursiveMode::NonRecursive) {
            eprintln!("[sftp-live-edit] watch failed: {} — falling back to no autosync", e);
            let _ = std::fs::remove_file(&temp_file_path_clone);
            return;
        }
        // Forward bridge: std::mpsc::recv blocks, so it has to live on the
        // blocking pool.
        tokio::task::spawn_blocking(move || {
            while let Ok(res) = raw_rx.recv() {
                if let Ok(events) = res {
                    if events.iter().any(|ev| matches!(ev.kind, DebouncedEventKind::Any | DebouncedEventKind::AnyContinuous)) {
                        if tok_tx.blocking_send(()).is_err() { break; }
                    }
                }
            }
        });

        // Overall 2-hour ceiling so an editor left open forever doesn't
        // keep the watcher alive past any reasonable session.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2 * 60 * 60);
        loop {
            let wait = tokio::time::sleep_until(deadline);
            tokio::select! {
                _ = wait => break,
                maybe = tok_rx.recv() => {
                    if maybe.is_none() { break; }
                    if !temp_file_path_clone.exists() { break; }
                    // Cheap pre-check: if the session is gone we exit the
                    // watcher entirely instead of looping and spamming
                    // "Auto-sync failed" toasts on every subsequent save.
                    {
                        let connections = connections_clone.lock().await;
                        if !connections.contains_key(&session_id_clone) {
                            let _ = app_handle_clone.emit(
                                &format!("sftp-sync-status-{}", session_id_clone),
                                serde_json::json!({
                                    "status": "error",
                                    "message": format!("Auto-sync stopped — session for {} is gone", filename_clone),
                                }),
                            );
                            break;
                        }
                    }
                    let upload_res = async {
                    let session_arc = {
                        let connections = connections_clone.lock().await;
                        connections.get(&session_id_clone).map(|sess| Arc::clone(sess))
                    };

                    let session_arc = match session_arc {
                        Some(sess) => sess,
                        None => return Err("SSH session disconnected".to_string()),
                    };

                    // Open the SFTP subsystem, then IMMEDIATELY drop the
                    // session mutex. Holding it across the whole write serialises
                    // every open_terminal / cold-cache SFTP-bootstrap request on
                    // the same session behind this one save — visible as a UI
                    // freeze whenever the user Ctrl-S's a large remote file.
                    let sftp = {
                        let session = session_arc.lock().await;
                        let channel = session.channel_open_session().await.map_err(|e| e.to_string())?;
                        channel.request_subsystem(true, "sftp").await.map_err(|e| e.to_string())?;
                        let s = russh_sftp::client::SftpSession::new(channel.into_stream()).await.map_err(|e| e.to_string())?;
                        drop(session);
                        s
                    };

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
        }
        // The watcher exited (timeout, file disappeared, or session gone).
        // Wipe the temp file so the remote contents aren't left lying around
        // in OS temp once editing is done. Errors are intentionally ignored
        // — on Windows the editor may still hold a lock on the file, and the
        // worst case is the file persists until the OS cleans temp.
        drop(debouncer);
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
    // temp dir. safe_temp_leaf_name restricts to a safe leaf (no separators,
    // no `..`, no drive markers) and we scope to a per-session subdir so
    // parallel drags don't clobber each other.
    let raw_name = safe_temp_leaf_name(&remote_path)?;
    let session_dir = session_drag_dir(&session_id);
    std::fs::create_dir_all(&session_dir)
        .map_err(|e| format!("Failed to create drag staging dir: {}", e))?;
    let temp_file_path = session_dir.join(&raw_name);
    std::fs::write(&temp_file_path, &data).map_err(|e| format!("Failed to write temporary file: {}", e))?;

    Ok(temp_file_path.to_string_lossy().to_string())
}

#[tauri::command]
async fn local_open_file(local_path: String) -> Result<(), String> {
    #[cfg(target_os = "android")]
    {
        let _ = local_path;
        return Err("Open-with is not available on Android.".into());
    }
    #[cfg(not(target_os = "android"))]
    {
        let safe = guard_local_path(&local_path, false)?;
        open::that(&safe).map_err(|e| format!("Failed to open local file: {}", e))?;
        Ok(())
    }
}

#[tauri::command]
async fn local_open_in_explorer(local_path: String) -> Result<(), String> {
    #[cfg(target_os = "android")]
    {
        let _ = local_path;
        return Err("Open-in-Explorer is not available on Android.".into());
    }
    #[cfg(not(target_os = "android"))]
    {
        let safe = guard_local_path(&local_path, false)?;
        if safe.is_dir() {
            open::that(&safe).map_err(|e| format!("Failed to open folder: {}", e))?;
        } else if let Some(parent) = safe.parent() {
            open::that(parent).map_err(|e| format!("Failed to open folder: {}", e))?;
        }
        Ok(())
    }
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
async fn local_home_dir(app: tauri::AppHandle) -> Result<String, String> {
    #[cfg(target_os = "android")]
    {
        // Android has no "home directory" in the desktop sense — the
        // `directories` crate returns nothing meaningful here. Fall back to
        // whatever the quick-dirs probe picked (Downloads if writable, else
        // app-scoped external storage) so the callers that want *some*
        // starting point still get one instead of an error.
        let dir = android_default_local_dir(app).await?;
        if !dir.is_empty() { return Ok(dir); }
        return Err("Could not resolve home directory".into());
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = app;
        directories::UserDirs::new()
            .and_then(|d| d.home_dir().to_str().map(String::from))
            .ok_or_else(|| "Could not resolve home directory".into())
    }
}

#[tauri::command]
async fn local_desktop_dir(app: tauri::AppHandle) -> Result<String, String> {
    #[cfg(target_os = "android")]
    {
        // No Desktop concept on Android — reuse the writable-probe default
        // instead of erroring, so `FilePanel.homePath()` lands somewhere
        // useful on first load.
        let dir = android_default_local_dir(app).await?;
        if !dir.is_empty() { return Ok(dir); }
        return Err("Could not resolve default directory".into());
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = app;
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
}

/// Test whether a directory is actually writable by the current process.
/// On Android, scoped storage means many paths appear readable via
/// `Path::exists()` but writes fail with EACCES — so we probe with a real
/// touch + delete. The probe filename is a random UUID-style token so
/// concurrent runs don't collide, and we tolerate `AlreadyExists` because
/// that still proves the parent is writable. Only called from the Android
/// quick-dirs picker today; the `#[allow]` keeps the desktop build quiet.
#[allow(dead_code)]
fn is_dir_writable(p: &std::path::Path) -> bool {
    if !p.is_dir() { return false; }
    let seq = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let probe = p.join(format!(".submarine-probe-{}", seq));
    match std::fs::write(&probe, b"") {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

/// Labeled quick-pick entries for the local file picker on Android.
/// Only writable paths are returned — the frontend uses this to populate a
/// small popover instead of the native rfd picker (which doesn't exist on
/// Android). Order is by preference: user-visible storage first (Downloads,
/// Documents), then app-scoped fallbacks that always work under scoped
/// storage. All returned paths are already canonical, so they pass
/// `guard_local_path` without further massaging.
#[derive(serde::Serialize)]
pub struct AndroidQuickDir {
    pub label: String,
    pub path: String,
}

#[tauri::command]
async fn android_quick_dirs(app: tauri::AppHandle) -> Result<Vec<AndroidQuickDir>, String> {
    #[cfg(not(target_os = "android"))]
    {
        let _ = app;
        return Ok(Vec::new());
    }
    #[cfg(target_os = "android")]
    {
        // Build the candidate list up front (label + path pairs), then run
        // the writability probe + dedupe in a plain loop. The earlier
        // closure-based version couldn't coexist with the later
        // `out.is_empty()` re-borrow because the closure held a mutable
        // borrow of `out` for the whole function scope.
        let mut candidates: Vec<(String, std::path::PathBuf)> = vec![
            ("Downloads".into(),  std::path::PathBuf::from("/storage/emulated/0/Download")),
            ("Documents".into(),  std::path::PathBuf::from("/storage/emulated/0/Documents")),
            ("DCIM".into(),       std::path::PathBuf::from("/storage/emulated/0/DCIM")),
            ("Pictures".into(),   std::path::PathBuf::from("/storage/emulated/0/Pictures")),
            ("Movies".into(),     std::path::PathBuf::from("/storage/emulated/0/Movies")),
            ("Music".into(),      std::path::PathBuf::from("/storage/emulated/0/Music")),
            ("SD card".into(),    std::path::PathBuf::from("/storage/emulated/0")),
        ];
        // App-scoped external files dir — always writable, survives reboots,
        // and visible to the user through any file manager under
        // Android/data/com.submarine.app/files. This is the fallback default
        // when everything shared is locked down.
        if let Ok(dir) = app.path().app_local_data_dir() {
            candidates.push(("App storage".into(), dir));
        }
        let mut out: Vec<AndroidQuickDir> = Vec::new();
        let mut seen = std::collections::HashSet::<String>::new();
        for (label, path) in candidates {
            if !is_dir_writable(&path) { continue; }
            let s = path.to_string_lossy().into_owned();
            if seen.insert(s.clone()) {
                out.push(AndroidQuickDir { label, path: s });
            }
        }
        // Internal cache — last-resort, still writable but hidden from the
        // user in most stock file managers. We only add it when nothing else
        // survived the writability probe above.
        if out.is_empty() {
            if let Ok(dir) = app.path().app_cache_dir() {
                if is_dir_writable(&dir) {
                    let s = dir.to_string_lossy().into_owned();
                    out.push(AndroidQuickDir { label: "App cache".into(), path: s });
                }
            }
        }
        Ok(out)
    }
}

/// Preferred default working directory on Android — first writable entry
/// from `android_quick_dirs`. FilePanel calls this on mount so the local
/// pane opens somewhere useful instead of `/` (which lists nothing under
/// scoped storage). Empty string means "leave the current path alone" and
/// is treated as a no-op by the frontend.
#[tauri::command]
async fn android_default_local_dir(app: tauri::AppHandle) -> Result<String, String> {
    let dirs = android_quick_dirs(app).await?;
    Ok(dirs.into_iter().next().map(|d| d.path).unwrap_or_default())
}

/// One resolved entry from an OpenSSH client config `Host` block. The
/// frontend picks a subset of these and turns each into a fresh server row
/// via the existing `add_server` command — password/key are left blank so
/// the user configures those after import.
#[derive(serde::Serialize)]
struct ImportedHost {
    /// The alias the user actually types (`ssh <alias>`) — becomes the
    /// server's display name after import.
    host_alias: String,
    /// Resolved `HostName`, or the alias itself if the block didn't set one
    /// (matches OpenSSH's own behaviour when HostName is omitted).
    hostname: String,
    /// Resolved `Port`, defaulting to the standard SSH port.
    port: u16,
    /// Resolved `User`. Empty string when unset — the frontend can fall
    /// back to whatever it uses elsewhere.
    user: String,
    /// Resolved `IdentityFile`. Purely informational for now — the import
    /// flow doesn't auto-attach keys because we'd need to also read and
    /// register them in the vault, which is a separate feature.
    identity_file: Option<String>,
    /// Resolved `ProxyJump`. Informational only; live proxy config still
    /// happens in the Server details panel.
    proxy_jump: Option<String>,
}

/// Read the user's OpenSSH client config (default `~/.ssh/config`) and
/// return one `ImportedHost` per non-wildcard alias so the UI can offer a
/// checkbox picker. We deliberately keep the parser dumb: no macro
/// expansion, no `Include` recursion, no token substitution (`%h`/`%u`),
/// no `Match` blocks. Anything we don't understand is skipped silently
/// rather than surfaced as an error — a user's config often has many
/// directives (ForwardAgent, LogLevel, etc.) that are irrelevant to
/// picking a host to import.
///
/// Desktop-only. Android has no meaningful `~/.ssh/config` path and its
/// import story goes through profile export/import instead — we return a
/// clear message rather than pretending to look somewhere.
#[tauri::command]
fn parse_ssh_config(path: Option<String>) -> Result<Vec<ImportedHost>, String> {
    #[cfg(target_os = "android")]
    {
        let _ = path;
        Err("Not available on Android — use export/import instead".into())
    }
    #[cfg(not(target_os = "android"))]
    {
        // One in-progress Host block. We accumulate resolved fields as we
        // walk the file and emit one ImportedHost per alias when the block
        // ends (either the next `Host` line or EOF).
        struct Block {
            aliases: Vec<String>,
            hostname: Option<String>,
            port: Option<u16>,
            user: Option<String>,
            identity_file: Option<String>,
            proxy_jump: Option<String>,
        }

        // Split `Directive value...` into (directive, rest). OpenSSH treats
        // `=` and whitespace as equivalent separators between the directive
        // name and its argument, so both `Port 2222` and `Port=2222` parse
        // the same way.
        fn split_directive(line: &str) -> (&str, &str) {
            match line.find(|c: char| c.is_whitespace() || c == '=') {
                Some(i) => {
                    let key = &line[..i];
                    let rest = line[i..]
                        .trim_start_matches(|c: char| c.is_whitespace() || c == '=');
                    (key, rest)
                }
                None => (line, ""),
            }
        }

        // Emit one ImportedHost per non-wildcard alias in the block. Wildcard
        // (`*` / `?`) and negation (`!prefix`) aliases are OpenSSH's template
        // mechanism — they don't correspond to a single server the user
        // would want as a row, so we skip them silently.
        fn flush_block(b: &Block, out: &mut Vec<ImportedHost>) {
            for alias in &b.aliases {
                if alias.contains('*') || alias.contains('?') || alias.starts_with('!') {
                    continue;
                }
                out.push(ImportedHost {
                    host_alias: alias.clone(),
                    hostname: b.hostname.clone().unwrap_or_else(|| alias.clone()),
                    port: b.port.unwrap_or(22),
                    user: b.user.clone().unwrap_or_default(),
                    identity_file: b.identity_file.clone(),
                    proxy_jump: b.proxy_jump.clone(),
                });
            }
        }

        let config_path: PathBuf = match path {
            Some(p) if !p.trim().is_empty() => PathBuf::from(p),
            _ => directories::UserDirs::new()
                .map(|u| u.home_dir().join(".ssh").join("config"))
                .ok_or_else(|| "Could not resolve home directory".to_string())?,
        };

        if !config_path.exists() {
            return Err(format!(
                "SSH config file not found at {}",
                config_path.display()
            ));
        }

        let contents = fs::read_to_string(&config_path).map_err(|e| {
            format!("Failed to read {}: {}", config_path.display(), e)
        })?;

        let mut out: Vec<ImportedHost> = Vec::new();
        let mut cur: Option<Block> = None;

        for raw_line in contents.lines() {
            let line = raw_line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (key, rest) = split_directive(line);
            let key_lc = key.to_ascii_lowercase();
            match key_lc.as_str() {
                "host" => {
                    // Close out the previous block before starting a new one.
                    if let Some(prev) = cur.take() {
                        flush_block(&prev, &mut out);
                    }
                    let mut b = Block {
                        aliases: Vec::new(),
                        hostname: None,
                        port: None,
                        user: None,
                        identity_file: None,
                        proxy_jump: None,
                    };
                    for a in rest.split_whitespace() {
                        b.aliases.push(a.trim_matches('"').to_string());
                    }
                    cur = Some(b);
                }
                "hostname" | "port" | "user" | "identityfile" | "proxyjump" => {
                    if let Some(b) = cur.as_mut() {
                        let val = rest.trim().trim_matches('"');
                        if val.is_empty() {
                            continue;
                        }
                        match key_lc.as_str() {
                            "hostname" => b.hostname = Some(val.to_string()),
                            "port" => {
                                if let Ok(p) = val.parse::<u16>() {
                                    b.port = Some(p);
                                }
                            }
                            "user" => b.user = Some(val.to_string()),
                            "identityfile" => b.identity_file = Some(val.to_string()),
                            "proxyjump" => b.proxy_jump = Some(val.to_string()),
                            _ => {}
                        }
                    }
                }
                // Include chains, Match blocks, and every other directive
                // are v1-out-of-scope. Ignoring them keeps the parser
                // predictable for the "flat list of hosts" use case.
                _ => {}
            }
        }
        if let Some(last) = cur.take() {
            flush_block(&last, &mut out);
        }

        Ok(out)
    }
}

/// Parse a blob of text pasted from another SSH client's export and return
/// the same `ImportedHost` shape as `parse_ssh_config`. Auto-detects the
/// format so the frontend only needs one "Paste your export" text area
/// instead of a picker-per-format:
///
///   • Windows Regedit `.reg` export of PuTTY sessions
///     (`HKEY_CURRENT_USER\Software\SimonTatham\PuTTY\Sessions\...`) —
///     detected by the file's `Windows Registry Editor` header. Each
///     `[HKEY_...\Sessions\<name>]` block becomes one host; HostName,
///     PortNumber, and UserName are decoded from the `"key"=dword:` /
///     `"key"="value"` entries. Percent-decoded so aliases with spaces
///     ("My Prod Box") round-trip.
///
///   • JSON array — either
///     `[{"name":"foo","host":"1.2.3.4","port":22,"user":"root"}, …]`
///     or the Termius-style
///     `[{"label":"foo","address":"1.2.3.4","port":22,"username":"root"}, …]`.
///     Any missing field defaults to the OpenSSH convention.
///
///   • MobaXterm `.mxtsessions` INI (partial) — sessions live under
///     `[Bookmarks_<n>]` with `SessionName=…` and comma-separated fields
///     `HostName,Port,UserName,…`. Best-effort — MobaXterm's schema has
///     drifted across releases so we only trust the first four fields.
///
/// Anything the parser can't recognise is a soft-fail: the returned
/// `Vec` is what we DID find, the message describes what got skipped.
/// The command is desktop+Android — no filesystem access, just text.
#[tauri::command]
fn parse_client_import(text: String) -> Result<Vec<ImportedHost>, String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err("Paste an exported session block first.".into());
    }

    // ── JSON array — the most permissive path, so try it first. Two
    //    supported field-name variants (see doc comment). We accept a
    //    generic `serde_json::Value` array rather than a strict struct
    //    so a stray extra field doesn't kill the whole import.
    if trimmed.starts_with('[') {
        let arr: Vec<serde_json::Value> = serde_json::from_str(trimmed)
            .map_err(|e| format!("JSON parse failed: {}", e))?;
        let mut out = Vec::with_capacity(arr.len());
        for (i, v) in arr.iter().enumerate() {
            let obj = v.as_object().ok_or_else(|| {
                format!("Entry #{} is not an object", i + 1)
            })?;
            let get_str = |keys: &[&str]| -> Option<String> {
                for k in keys {
                    if let Some(s) = obj.get(*k).and_then(|x| x.as_str()) {
                        if !s.is_empty() { return Some(s.to_string()); }
                    }
                }
                None
            };
            let get_u16 = |keys: &[&str]| -> Option<u16> {
                for k in keys {
                    if let Some(n) = obj.get(*k).and_then(|x| x.as_u64()) {
                        return u16::try_from(n).ok();
                    }
                    if let Some(s) = obj.get(*k).and_then(|x| x.as_str()) {
                        if let Ok(p) = s.parse::<u16>() { return Some(p); }
                    }
                }
                None
            };
            let alias = get_str(&["name", "label", "title", "alias", "host_alias"])
                .or_else(|| get_str(&["host", "hostname", "address"]))
                .unwrap_or_else(|| format!("imported-{}", i + 1));
            let hostname = get_str(&["host", "hostname", "address"]).unwrap_or_else(|| alias.clone());
            let port = get_u16(&["port"]).unwrap_or(22);
            let user = get_str(&["user", "username"]).unwrap_or_default();
            let identity_file = get_str(&["identity_file", "identityFile", "key", "privateKey"]);
            let proxy_jump = get_str(&["proxy_jump", "proxyJump", "jump"]);
            out.push(ImportedHost {
                host_alias: alias,
                hostname,
                port,
                user,
                identity_file,
                proxy_jump,
            });
        }
        if out.is_empty() {
            return Err("JSON array was valid but contained no entries.".into());
        }
        return Ok(out);
    }

    // ── PuTTY .reg export
    if trimmed.to_ascii_lowercase().contains("windows registry editor")
        || trimmed.contains("[HKEY_CURRENT_USER\\Software\\SimonTatham\\PuTTY\\Sessions")
        || trimmed.contains("[HKEY_USERS\\") && trimmed.contains("SimonTatham\\PuTTY\\Sessions")
    {
        return parse_putty_reg(trimmed);
    }

    // ── MobaXterm .mxtsessions
    if trimmed.contains("[Bookmarks") || trimmed.contains(";SessionName") {
        return parse_mobaxterm_sessions(trimmed);
    }

    Err("Unrecognised format — paste a JSON array, a PuTTY .reg export, or a MobaXterm .mxtsessions block.".into())
}

/// Parse `regedit /e` output of PuTTY's session key. The format is
/// deterministic (one `[...]` header line per session, then `"key"=type:val`
/// lines) so a line-oriented walk covers it. We only pull the three fields
/// that translate to a Submarine row: HostName, PortNumber, UserName.
/// Session-name percent-escapes (%20 for space, etc.) are undone so the
/// alias reads naturally.
fn parse_putty_reg(text: &str) -> Result<Vec<ImportedHost>, String> {
    let mut out: Vec<ImportedHost> = Vec::new();
    let mut current: Option<(String, String, u16, String)> = None; // (alias, host, port, user)
    let close = |cur: &mut Option<(String, String, u16, String)>, out: &mut Vec<ImportedHost>| {
        if let Some((alias, host, port, user)) = cur.take() {
            if !host.is_empty() || !alias.is_empty() {
                out.push(ImportedHost {
                    host_alias: alias.clone(),
                    hostname: if host.is_empty() { alias } else { host },
                    port: if port == 0 { 22 } else { port },
                    user,
                    identity_file: None,
                    proxy_jump: None,
                });
            }
        }
    };
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() { continue; }
        if line.starts_with('[') && line.ends_with(']') {
            close(&mut current, &mut out);
            // Only sessions — skip other PuTTY keys (SshHostKeys, Jumplist).
            let inner = &line[1..line.len() - 1];
            let Some(idx) = inner.rfind("\\Sessions\\") else { continue; };
            let name_raw = &inner[idx + "\\Sessions\\".len()..];
            let name = putty_unescape(name_raw);
            if name.is_empty() { continue; }
            current = Some((name, String::new(), 0, String::new()));
            continue;
        }
        let Some((key, val_raw)) = line.split_once('=') else { continue; };
        let Some(cur) = current.as_mut() else { continue; };
        let key = key.trim().trim_matches('"');
        let val = val_raw.trim();
        match key {
            "HostName" => {
                if let Some(v) = strip_reg_string(val) { cur.1 = v; }
            }
            "PortNumber" => {
                if let Some(n) = strip_reg_dword(val) {
                    if let Ok(p) = u16::try_from(n) { cur.2 = p; }
                }
            }
            "UserName" => {
                if let Some(v) = strip_reg_string(val) { cur.3 = v; }
            }
            _ => {}
        }
    }
    close(&mut current, &mut out);
    if out.is_empty() {
        return Err("No PuTTY sessions found in the pasted text.".into());
    }
    Ok(out)
}

/// PuTTY registry keys with special characters (space, backslash, non-
/// ASCII) are percent-escaped in the exported name. `%20` → space,
/// `%25` → `%`. Anything else is passed through as-is. Not a full URL
/// decoder — PuTTY's escape table is narrower.
fn putty_unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // Only treat `%HH` as an escape when both following bytes are ASCII
        // hex digits. The is_ascii_hexdigit guard also keeps the `&s[..]`
        // slice on char boundaries — without it, a `%` before a multi-byte
        // UTF-8 char (e.g. `%€`) would slice mid-codepoint and panic.
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && bytes[i + 1].is_ascii_hexdigit()
            && bytes[i + 2].is_ascii_hexdigit()
        {
            let hex = &s[i + 1..i + 3];
            if let Ok(n) = u8::from_str_radix(hex, 16) {
                out.push(n as char);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

/// `.reg` string entries look like `"HostName"="1.2.3.4"`. Strip the
/// surrounding quotes and unescape the two sequences .reg uses (`\\` and
/// `\"`). Returns None if the value isn't a quoted string.
fn strip_reg_string(val: &str) -> Option<String> {
    let s = val.trim();
    if !s.starts_with('"') || !s.ends_with('"') || s.len() < 2 { return None; }
    let inner = &s[1..s.len() - 1];
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(next) = chars.next() { out.push(next); }
        } else {
            out.push(c);
        }
    }
    Some(out)
}

/// `.reg` DWORD entries look like `"PortNumber"=dword:00000016`. The
/// `dword:` prefix is followed by exactly 8 hex chars. Anything else
/// (missing prefix, non-hex, wrong length) returns None so the caller
/// can leave the field at its default rather than propagating an error.
fn strip_reg_dword(val: &str) -> Option<u32> {
    let s = val.trim();
    let stripped = s.strip_prefix("dword:")?;
    u32::from_str_radix(stripped, 16).ok()
}

/// Parse a MobaXterm `.mxtsessions` INI-ish blob. MobaXterm stores each
/// session as one line under a `[Bookmarks_N]` group, formatted roughly
/// `<Title>=#109#0%<hostname>%<port>%<username>%…` with a variable trail
/// of feature flags. We only decode the first three fields — anything
/// past that is version-specific and not worth the complexity for an
/// import flow that leaves password/key blank anyway.
fn parse_mobaxterm_sessions(text: &str) -> Result<Vec<ImportedHost>, String> {
    let mut out: Vec<ImportedHost> = Vec::new();
    let mut in_bookmarks = false;
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with(';') { continue; }
        if line.starts_with('[') {
            in_bookmarks = line.starts_with("[Bookmarks");
            continue;
        }
        if !in_bookmarks { continue; }
        let Some((title, val)) = line.split_once('=') else { continue; };
        // MobaXterm SSH sessions start with `#109#`. Non-SSH bookmark
        // types (telnet, RDP, sftp-only) use different numbers — we
        // don't want to blindly import those as SSH rows.
        if !val.starts_with("#109#") { continue; }
        // Skip the `#109#<N>%` framing and split the payload on `%`.
        let payload = val.split_once('%').map(|(_, rest)| rest).unwrap_or(val);
        let parts: Vec<&str> = payload.split('%').collect();
        if parts.len() < 3 { continue; }
        let hostname = parts[0].trim();
        let port = parts[1].trim().parse::<u16>().unwrap_or(22);
        let user = parts[2].trim().to_string();
        if hostname.is_empty() { continue; }
        out.push(ImportedHost {
            host_alias: title.trim().to_string(),
            hostname: hostname.to_string(),
            port,
            user,
            identity_file: None,
            proxy_jump: None,
        });
    }
    if out.is_empty() {
        return Err("No MobaXterm sessions found in the pasted text.".into());
    }
    Ok(out)
}

/// Defense-in-depth guard for the local-FS commands the frontend can invoke.
/// We can't lock everything down to a sandbox (the local file browser
/// legitimately needs to roam the user's disk to pick uploads), but we CAN
/// refuse the obviously destructive cases: the filesystem root, OS system
/// directories, and unresolvable paths. If the renderer is ever compromised
/// (XSS via terminal output, a future feature, etc.) this stops
/// `local_remove("C:\\")` cold.
/// True when a single SFTP directory-entry name is a plain, safe filename —
/// i.e. one that can be joined onto a local root without escaping it.
///
/// `read_dir` entry names come straight off the SFTP wire, so a malicious or
/// compromised server can return a name that contains path separators or `..`
/// (e.g. `../../../.config/autostart/x.desktop`, or `..\..\Startup\x.bat`).
/// Joining such a name onto the download root resolves OUTSIDE it — a zip-slip
/// arbitrary-write primitive. A genuine filesystem entry name is always a
/// single component: it never contains `/` (the POSIX/SFTP separator), `\` (a
/// separator once the rel path is split for a Windows client), or a NUL, and is
/// never `.`/`..`. Rejecting anything else costs nothing on a well-behaved
/// server and stops the traversal at the point the untrusted name first enters
/// our local-path building. Callers skip (or abort on) a rejected entry.
pub(crate) fn is_safe_dir_entry_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains('/')
        && !name.contains('\\')
        && !name.contains('\0')
}

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

    let mut canon_norm = canonical.to_string_lossy().to_lowercase().replace('\\', "/");
    // On Windows, std::fs::canonicalize returns a `\\?\`-prefixed *verbatim*
    // path, e.g. `\\?\C:\Windows\System32`. After the `\` -> `/` normalization
    // above that becomes `//?/c:/windows/system32`, which never matches the
    // blocklist entries `c:/windows`, `c:/program files`, etc. — silently
    // defeating this guard for every Windows system directory. Strip the
    // verbatim prefix (and its `\\?\UNC\` variant for UNC paths) so the
    // subsequent prefix match sees a normal drive-letter path. Also strip the
    // NT-device `\\.\` prefix on the off chance a caller hands us one.
    if let Some(rest) = canon_norm.strip_prefix("//?/unc/") {
        canon_norm = format!("//{}", rest);
    } else if let Some(rest) = canon_norm.strip_prefix("//?/") {
        canon_norm = rest.to_string();
    } else if let Some(rest) = canon_norm.strip_prefix("//./") {
        canon_norm = rest.to_string();
    }
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

    // Auto-run / login-persistence locations. Unlike the fixed system-dir
    // prefixes above, these sit inside the per-user profile
    // (C:\Users\<name>\AppData\..., ~/Library/..., ~/.config/...) so no static
    // prefix catches them — match on a path tail instead. Blocking them stops
    // a download destination (or any local op) from dropping an executable
    // into a folder the OS auto-runs at next login. The markers are
    // platform-specific strings that simply never match on the wrong OS, so a
    // single unconditional loop is fine.
    const PERSIST_MARKERS: &[&str] = &[
        "/start menu/programs/startup",        // Windows Startup (per-user & all-users)
        "/appdata/roaming/microsoft/windows",  // Windows autorun / machine-managed
        "/appdata/local/microsoft/windows",
        "/library/launchagents",               // macOS per-user launch agents
        "/library/launchdaemons",              // macOS launch daemons
        "/.config/autostart",                  // Linux XDG autostart
    ];
    for marker in PERSIST_MARKERS {
        if canon_norm.contains(marker) {
            return Err(format!("Refusing operation on auto-run / persistence path: {}", canonical.display()));
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
    #[cfg(target_os = "android")]
    {
        return Err("Folder picker not available on Android.".into());
    }
    #[cfg(not(target_os = "android"))]
    {
        let folder = rfd::FileDialog::new()
            .set_title("Choose Local Directory")
            .pick_folder();
        Ok(folder.map(|p| p.to_string_lossy().to_string()))
    }
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

/// Library entrypoint shared by both the desktop `bin/main.rs` shim and
/// Tauri's Android entry-point macro. Everything that builds the
/// `tauri::Builder`, registers plugins/state/commands, and finally calls
/// `.run(generate_context!())` lives here so the same binary contents
/// ship on both platforms — only the wrapping is different.
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
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

    let builder = tauri::Builder::default();
    // Save/restore the main window's last size + position to a JSON file
    // in app_data_dir. Keeps the user's preferred geometry across launches
    // without us having to wire setSize/setPosition by hand. Plain plugin —
    // it intercepts window events; no JS-facing commands to lock down.
    // Mobile builds skip this entirely: Android decides window geometry,
    // not us, and the plugin's crate isn't compiled into the Android target.
    #[cfg(not(target_os = "android"))]
    let builder = builder.plugin(tauri_plugin_window_state::Builder::default().build());
    // Cross-platform URL opener — Android (Intent.ACTION_VIEW), Windows
    // (start), macOS (open), Linux (xdg-open). about.rs's open_external_url
    // dispatches through this so the same code path works in the desktop
    // installer AND the Android APK. Capability is granted in default.json.
    let builder = builder.plugin(tauri_plugin_opener::init());
    builder
        .manage(DbState { conn: std::sync::Arc::new(StdMutex::new(None)), master_key: StdMutex::new(None), salt: StdMutex::new(None), db_path: StdMutex::new(None), active_profile: StdMutex::new(None), hlc: StdMutex::new(None) })
        .manage(SshState::new())
        // Docker live-log stream registry — keyed by frontend-issued stream id,
        // values are tokio AbortHandles so the user can stop tailing on demand.
        .manage(docker::DockerStreams::new())
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
            list_profiles, cloud_list_sync_profiles, cloud_delete_profile, select_profile, create_profile, delete_profile, close_profile,
            export_profile, import_profile_pick, import_profile_save,
            cloud::cloud_status, cloud::cloud_signup, cloud::cloud_consume_verify_link,
            cloud::cloud_set_password, cloud::cloud_login, cloud::cloud_logout,
            cloud::cloud_request_password_reset, cloud::cloud_reset_password,
            cloud::cloud_request_login_link, cloud::cloud_login_with_link,
            sync_now,
            identity_status, setup_identity, reset_identity,
            share_current_profile, invite_to_share, list_shares, share_member_list,
            accept_share, import_shared_profile, restore_personal_profile, share_set_role, share_revoke, share_leave, share_delete,
            profile_share_status, stop_sharing, profile_sync_stats,
            set_editor_label,
            add_server, edit_server, delete_server, add_mirror_to_server, get_servers, get_ssh_keys, set_server_color, set_folder_color, set_server_notes, set_server_run_on_connect, set_server_jump_host, clone_server, reveal_server_password, reveal_credential_password, reveal_ssh_key,
            get_credentials, generate_ssh_key,
            add_folder, rename_folder, delete_folder, get_folders,
            add_command, edit_command, delete_command, get_commands,
            cmd_history_add, cmd_history_list, cmd_history_clear,
            add_note, edit_note, delete_note, get_notes,
            mirror_dry_run, start_mirror, stop_mirror, list_mirrors, pick_local_directory,
            add_credential, edit_credential, delete_credential,
            add_ssh_key, edit_ssh_key, delete_ssh_key,
            initiate_connection, verify_fingerprint_response, submit_kbi_response, disconnect_session,
            start_tunnel, stop_tunnel, list_tunnels, restart_session_tunnels,
            open_terminal, write_terminal_data, resize_terminal, close_terminal,
            ssh_info_probe_section, ssh_systemctl_action, ssh_kill_process,
            ssh_iptables_chain, ssh_nft_chain,
            docker::ssh_docker_container_action,
            docker::ssh_docker_inspect,
            docker::ssh_docker_stats,
            docker::ssh_docker_logs,
            docker::ssh_docker_logs_start,
            docker::ssh_docker_logs_stop,
            docker::ssh_docker_networks,
            docker::ssh_docker_containers,
            docker::ssh_docker_images_list,
            docker::ssh_docker_volumes_list,
            docker::ssh_docker_compose_list,
            docker::ssh_docker_compose_per_container,
            docker::ssh_docker_compose_services,
            docker::ssh_docker_compose_action,
            docker::ssh_docker_compose_view,
            docker::ssh_docker_prune,
            docker::open_container_terminal,
            select_local_folder, local_list_dir,
            local_home_dir, local_desktop_dir, local_create_dir, local_remove, local_rename,
            android_quick_dirs, android_default_local_dir,
            parse_ssh_config,
            parse_client_import,
            sftp_list_dir, sftp_create_dir, sftp_remove_file, sftp_remove_dir,
            sftp_rename, sftp_set_permissions, sftp_set_owner,
            sftp_download_file, sftp_download_dir, sftp_upload_file, sftp_upload_dir, sftp_cancel_transfer, sftp_open_remote_file,
            local_open_file, local_open_in_explorer, sftp_prepare_drag,
            monitor_list, monitor_add, monitor_remove, monitor_set_metrics, monitor_set_custom_metrics,
            monitor_resume, monitor_pause, monitor_resume_all, monitor_pause_all,
            monitor_get_settings, monitor_set_settings,
            about::app_info, about::check_for_updates, about::open_external_url
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dir_entry_name_rejects_traversal_and_separators() {
        // Attack vectors a hostile SFTP server can put in a readdir name.
        for bad in [
            "",
            ".",
            "..",
            "../../etc/passwd",
            "..\\..\\Startup\\x.bat",
            "a/b",
            "a\\b",
            "/etc/passwd",
            "with\0nul",
        ] {
            assert!(!is_safe_dir_entry_name(bad), "should reject {:?}", bad);
        }
    }

    #[test]
    fn dir_entry_name_accepts_plain_filenames() {
        // Legitimate names must still pass — including ones that merely
        // start with dots or contain colons/spaces (all legal on POSIX).
        for ok in [
            "file.txt",
            "notes 2024.md",
            ".bashrc",
            "..foo",
            "2024:01:01.log",
            "release-v0.2.37",
            "Ω_unicode_名前",
        ] {
            assert!(is_safe_dir_entry_name(ok), "should accept {:?}", ok);
        }
    }
}