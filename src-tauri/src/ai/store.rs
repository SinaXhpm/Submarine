//! The AI store: providers and their keys, chats, levels.
//!
//! A file of its own beside the profile's vault, `<name>.submarine-ai`, never
//! inside it: `export_profile` copies the vault byte for byte, kept copies and
//! cloud restores are vault copies too, and a foreign import keeps device-local
//! tables as the file brought them. Keys and chats must stay on this device.
//!
//! The format follows the vault's: magic `OMAI`, version 1, a 12-byte nonce,
//! then AES-256-GCM over the zstd image of an in-memory SQLite database. The
//! key comes from the vault's master key through HKDF, so unlocking the
//! profile (password or quick unlock) opens this file too, with no prompt.

use super::act::Undo;
use super::canon::{Role, Turn};
use super::exec::Facts;
use super::files::Stamp;
use super::gate::Level;
use super::net::{self, Route};
use super::provider::{profile::Profile, Kind, Settings};
use rusqlite::{params, Connection, OptionalExtension, MAIN_DB};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use zeroize::Zeroizing;

pub const MAGIC: &[u8; 4] = b"OMAI";
pub const VERSION: u8 = 1;
const NONCE_LEN: usize = 12;
/// The largest store image, before compression; as the vault's.
pub const MAX_IMAGE: usize = 64 * 1024 * 1024;
/// Past this the page is told, once per unlock, that the store is filling up.
const NEAR_FULL: usize = 48 * 1024 * 1024;

/// The `meta` keys kept per chat, each followed by the chat's id.
const CHAT_META: [&str; 5] = ["level_told:", "strip_thinking:", "context:", "draft:", "note_tag:"];

/// How long the activity log keeps a row, in seconds: 90 days.
const ACTIONS_KEPT: i64 = 90 * 86_400;

pub fn file_for(vault: &Path) -> PathBuf {
    vault.with_extension("submarine-ai")
}

/// Its own temp name: `with_extension("submarine.tmp")` would give the
/// vault's, and two saves at once would move one file over the other.
pub fn tmp_for(vault: &Path) -> PathBuf {
    vault.with_extension("submarine-ai.tmp")
}

pub fn derive_key(master: &[u8; 32], salt: &[u8]) -> Zeroizing<[u8; 32]> {
    let hk = hkdf::Hkdf::<sha2::Sha256>::new(Some(salt), master);
    let mut okm = Zeroizing::new([0u8; 32]);
    // 32 bytes is always a valid HKDF-SHA256 output length.
    hk.expand(b"submarine ai store v1", &mut *okm).expect("32-byte HKDF output");
    okm
}

fn seal(image: &[u8], key: &[u8; 32]) -> Result<Vec<u8>, String> {
    let compressed = Zeroizing::new(crate::vault_compress(image)?);
    let (ct, nonce) = crate::encrypt_with_key(&compressed, key)?;
    let mut out = Vec::with_capacity(5 + NONCE_LEN + ct.len());
    out.extend_from_slice(MAGIC);
    out.push(VERSION);
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ct);
    Ok(out)
}

/// A file this key opens but this version can't load: it stays as it is.
const UNREADABLE: &str = "[AI] STORE_UNREADABLE: this version of Submarine can't read the assistant's data on this device; update the app to open it";

/// The image in a store file, or `None` when the file isn't a store this key
/// opens. One it opens that doesn't decompress is an error.
fn unseal(blob: &[u8], key: &[u8; 32]) -> Result<Option<Zeroizing<Vec<u8>>>, String> {
    if blob.len() < 5 + NONCE_LEN + 16 || &blob[..4] != MAGIC || blob[4] != VERSION {
        return Ok(None);
    }
    let Ok(plain) = crate::decrypt_with_key(&blob[5 + NONCE_LEN..], &blob[5..5 + NONCE_LEN], key) else { return Ok(None) };
    let plain = Zeroizing::new(plain);
    let image = crate::vault_decompress(&plain).map_err(|_| UNREADABLE.to_string())?;
    Ok(Some(Zeroizing::new(image)))
}

/// The database in a store file, or `None` when the content isn't a store
/// this key opens: not one at all, sealed under another key, or damaged. A
/// newer Submarine's file is an error instead, and so is one this key opens
/// whose content doesn't load: it is this profile's own, saved in a way this
/// version can't read, and stays for the version that can.
fn load(blob: &[u8], key: &[u8; 32]) -> Result<Option<Connection>, String> {
    if blob.len() > 4 && &blob[..4] == MAGIC && blob[4] > VERSION {
        return Err("[AI] STORE_NEWER: a newer version of Submarine saved the assistant's data on this device; update the app to open it".into());
    }
    let Some(image) = unseal(blob, key)? else { return Ok(None) };
    let owned = crate::to_sqlite_owned(&image)?;
    let mut conn = Connection::open_in_memory().map_err(db_err)?;
    // A database at all: it loads, and its schema reads.
    let loaded = conn.deserialize(MAIN_DB, owned, false).is_ok();
    if !loaded || conn.query_row("SELECT COUNT(*) FROM sqlite_master", [], |r| r.get::<_, i64>(0)).is_err() {
        return Err(UNREADABLE.into());
    }
    Ok(Some(conn))
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS providers (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL,
  kind TEXT NOT NULL CHECK (kind IN ('anthropic','openai','compat')), profile TEXT NOT NULL,
  base_url TEXT NOT NULL, api_key TEXT, model TEXT NOT NULL,
  defaults TEXT NOT NULL DEFAULT '{}',
  context_tokens INTEGER, allow_data_collection INTEGER NOT NULL DEFAULT 0,
  proxy TEXT NOT NULL DEFAULT '{"mode":"system"}', consent_at INTEGER, is_default INTEGER NOT NULL DEFAULT 0,
  created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS server_levels (server_uuid TEXT PRIMARY KEY,
  level TEXT NOT NULL CHECK (level IN ('suggest','ask_all','ask','auto')), updated_at INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS always_run (server_uuid TEXT NOT NULL, command TEXT NOT NULL, added_at INTEGER NOT NULL,
  PRIMARY KEY (server_uuid, command));
CREATE TABLE IF NOT EXISTS chats (id INTEGER PRIMARY KEY AUTOINCREMENT, title TEXT NOT NULL, server_uuid TEXT, server_label TEXT,
  system_prompt TEXT NOT NULL, tools_json TEXT NOT NULL DEFAULT 'null', facts TEXT,
  settings TEXT NOT NULL, last_activity INTEGER NOT NULL, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS turns (chat_id INTEGER NOT NULL REFERENCES chats(id) ON DELETE CASCADE, seq INTEGER NOT NULL,
  role TEXT NOT NULL, canon TEXT NOT NULL, origin TEXT, model TEXT,
  renders TEXT NOT NULL DEFAULT '{}', ui TEXT NOT NULL DEFAULT '{}', created_at INTEGER NOT NULL,
  PRIMARY KEY (chat_id, seq));
CREATE TABLE IF NOT EXISTS notes (id INTEGER PRIMARY KEY, chat_id INTEGER NOT NULL REFERENCES chats(id) ON DELETE CASCADE,
  before_seq INTEGER NOT NULL, kind TEXT NOT NULL, text TEXT NOT NULL, at INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS file_state (chat_id INTEGER NOT NULL REFERENCES chats(id) ON DELETE CASCADE, path TEXT NOT NULL,
  size INTEGER, mtime INTEGER, sha256 TEXT, written INTEGER NOT NULL DEFAULT 0, PRIMARY KEY (chat_id, path));
CREATE TABLE IF NOT EXISTS actions (id INTEGER PRIMARY KEY AUTOINCREMENT, chat_id INTEGER, server_uuid TEXT, at INTEGER NOT NULL,
  tool TEXT NOT NULL, summary TEXT NOT NULL, decision TEXT NOT NULL, level TEXT NOT NULL,
  exit_code INTEGER, path TEXT, undo BLOB, created INTEGER NOT NULL DEFAULT 0, after_sha TEXT,
  undone INTEGER NOT NULL DEFAULT 0);
CREATE TABLE IF NOT EXISTS agent_events (id INTEGER PRIMARY KEY AUTOINCREMENT, at INTEGER NOT NULL, kind TEXT NOT NULL,
  agent TEXT NOT NULL, connection INTEGER NOT NULL, grant_id TEXT, server_uuid TEXT, detail TEXT NOT NULL DEFAULT '');
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT);
INSERT OR IGNORE INTO meta(key, value) VALUES ('schema', '1');
"#;

/// Adds a column a later version brought, when the table doesn't have it.
fn add_column(conn: &Connection, table: &str, column: &str, kind: &str) -> Result<(), String> {
    let has = conn
        .query_row(&format!("SELECT COUNT(*) FROM pragma_table_info('{table}') WHERE name = ?1"), [column], |r| r.get::<_, i64>(0))
        .map_err(db_err)?
        > 0;
    if !has {
        conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {kind};")).map_err(db_err)?;
    }
    Ok(())
}

pub fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

fn db_err(e: rusqlite::Error) -> String {
    format!("[AI] STORE: {e}")
}

pub struct Store {
    conn: Connection,
    key: Zeroizing<[u8; 32]>,
    /// The vault of the profile, beside which the file is.
    vault: PathBuf,
    path: PathBuf,
    tmp: PathBuf,
    /// The DbState generation of the profile this store belongs to.
    pub generation: u64,
    /// What a save has for the page to show, until it is taken.
    notice: Option<String>,
    /// The page was told this store is filling up.
    told_full: bool,
}

impl Store {
    /// Opens the store beside `vault`, or starts a new one. A file whose
    /// content this key doesn't open (a new profile reusing an old name,
    /// damage) is set aside as `.unreadable-<unix>` and a new store starts;
    /// the text returned says so. A file that can't be read, is too big, was
    /// saved by a newer Submarine, or opens with this key but doesn't load
    /// stays as it is, and opening fails.
    pub fn open(vault: &Path, master: &[u8; 32], salt: &[u8], generation: u64) -> Result<(Store, Option<String>), String> {
        let path = file_for(vault);
        let key = derive_key(master, salt);
        let mut notice = None;
        let conn = match fs::metadata(&path) {
            Ok(meta) => {
                if meta.len() > (MAX_IMAGE + 1024 * 1024) as u64 {
                    return Err("[AI] STORE_TOO_LARGE: the assistant's file on this device is bigger than this version of Submarine opens".into());
                }
                let blob = fs::read(&path).map_err(|e| format!("[AI] STORE_READ: {e}"))?;
                match load(&blob, &key)? {
                    Some(conn) => conn,
                    None => {
                        let aside = path.with_extension(format!("submarine-ai.unreadable-{}", now()));
                        fs::rename(&path, &aside).map_err(|e| format!("[AI] STORE_SET_ASIDE: {e}"))?;
                        notice = Some("The assistant's saved data on this device couldn't be opened, so it was set aside and a new store started. API keys and chats need adding again.".to_string());
                        Connection::open_in_memory().map_err(db_err)?
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Connection::open_in_memory().map_err(db_err)?,
            Err(e) => return Err(format!("[AI] STORE_READ: {e}")),
        };
        // secure_delete zeroes what a delete frees: the file is the serialized
        // database, free pages included, so a deleted chat or key would still
        // be in it otherwise. Temporary data (VACUUM, big sorts) stays in
        // memory: on disk it would be outside the encryption.
        conn.execute_batch("PRAGMA foreign_keys = ON; PRAGMA secure_delete = ON; PRAGMA temp_store = MEMORY;").map_err(db_err)?;
        conn.execute_batch(SCHEMA).map_err(db_err)?;
        // The agent gateway's: which agent a call came from, under which grant.
        add_column(&conn, "actions", "agent", "TEXT")?;
        add_column(&conn, "actions", "grant_id", "TEXT")?;
        let pruned = conn.execute("DELETE FROM actions WHERE at < ?1", [now() - ACTIONS_KEPT]).map_err(db_err)?
            + conn.execute("DELETE FROM agent_events WHERE at < ?1", [now() - ACTIONS_KEPT]).map_err(db_err)?;
        // Rebuilt without what the prune removed. A store from before that may
        // hold freed pages with old content: rebuilt once, without them. A
        // store that can't be rebuilt still works.
        let scrubbed: bool = conn.query_row("SELECT 1 FROM meta WHERE key = 'free_pages_scrubbed'", [], |_| Ok(true)).unwrap_or(false);
        if (pruned > 0 || !scrubbed) && conn.execute_batch("VACUUM;").is_ok() && !scrubbed {
            let _ = conn.execute("INSERT OR REPLACE INTO meta (key, value) VALUES ('free_pages_scrubbed', '1')", []);
        }
        Ok((Store { conn, key, vault: vault.to_path_buf(), path, tmp: tmp_for(vault), generation, notice: None, told_full: false }, notice))
    }

    #[cfg(test)]
    pub fn for_tests() -> Store {
        let dir = std::env::temp_dir().join(format!("submarine-ai-test-{}-{}", std::process::id(), rand_suffix()));
        fs::create_dir_all(&dir).unwrap();
        Store::open(&dir.join("p.submarine"), &[7u8; 32], &[1u8; 16], 1).unwrap().0
    }

    /// Writes the store to its file: sealed, then tmp, fsync, rename.
    pub fn save(&mut self) -> Result<(), String> {
        self.save_capped(MAX_IMAGE, NEAR_FULL)
    }

    /// `save`, with the cap and the size the page is told about as arguments.
    fn save_capped(&mut self, max: usize, near: usize) -> Result<(), String> {
        let mut image = self.conn.serialize(MAIN_DB).map_err(db_err)?;
        if image.len() > max {
            // The image holds freed pages too: rebuilt from the live rows, it
            // may fit. The view of the old pages goes first.
            drop(image);
            self.compact();
            image = self.conn.serialize(MAIN_DB).map_err(db_err)?;
            if image.len() > max {
                return Err("[AI] STORE_TOO_LARGE: the assistant's data passed 64 MiB; delete old chats or clear the activity log".into());
            }
        }
        if image.len() > near && !self.told_full {
            self.told_full = true;
            self.notice = Some(format!(
                "The assistant's data on this device is {} MiB, and it can't be saved past 64 MiB. Delete old chats, or clear the activity log, to make room.",
                image.len() / (1024 * 1024)
            ));
        }
        let blob = seal(&image, &self.key)?;
        crate::write_atomic(&self.path, &self.tmp, &blob)
    }

    /// What the saves have for the page to show, once.
    pub fn take_notice(&mut self) -> Option<String> {
        self.notice.take()
    }

    /// The vault this store belongs to.
    pub fn vault(&self) -> &Path {
        &self.vault
    }

    /// Rebuilds the database from its live rows, as deletes do before their
    /// save: the image shrinks, and keeps no piece of what was deleted
    /// (secure_delete zeroes what a delete frees, but a page rebuilt earlier
    /// can still hold an old copy of a row in its unused space). A store that
    /// can't be rebuilt still works.
    fn compact(&self) {
        let _ = self.conn.execute_batch("VACUUM;");
    }

    // --- providers --------------------------------------------------------

    pub fn providers(&self) -> Result<Vec<ProviderView>, String> {
        let mut st = self.conn.prepare(&format!("SELECT {PROVIDER_COLS} FROM providers ORDER BY is_default DESC, name COLLATE NOCASE")).map_err(db_err)?;
        let rows = st.query_map([], provider_row).map_err(db_err)?;
        rows.map(|r| r.map(|p| p.view).map_err(db_err)).collect()
    }

    pub fn provider(&self, id: i64) -> Result<ProviderRow, String> {
        self.conn
            .query_row(&format!("SELECT {PROVIDER_COLS} FROM providers WHERE id = ?1"), [id], provider_row)
            .optional()
            .map_err(db_err)?
            .ok_or_else(|| "[AI] NO_PROVIDER: that provider no longer exists".to_string())
    }

    pub fn default_provider(&self) -> Result<Option<i64>, String> {
        self.conn.query_row("SELECT id FROM providers ORDER BY is_default DESC, id LIMIT 1", [], |r| r.get(0)).optional().map_err(db_err)
    }

    pub fn save_provider(&mut self, input: &ProviderInput) -> Result<i64, String> {
        let name = input.name.trim();
        if name.is_empty() || name.chars().count() > 80 {
            return Err("[AI] BAD_PROVIDER: give the provider a name (up to 80 characters)".into());
        }
        // The model may wait: "Load models" saves the provider first, to ask it
        // which models it has. A chat needs one.
        let model = input.model.trim();
        if model.len() > 200 || model.chars().any(char::is_control) {
            return Err("[AI] BAD_PROVIDER: that model name isn't valid".into());
        }
        let base = net::normalize_base(&input.base_url)?;
        let stored = match input.id {
            Some(id) => self
                .conn
                .query_row("SELECT proxy FROM providers WHERE id = ?1", [id], |r| r.get::<_, String>(0))
                .optional()
                .map_err(db_err)?
                .and_then(|p| serde_json::from_str::<Route>(&p).ok()),
            None => None,
        };
        let route = net::normalize_route(&route_from_page(&input.route, stored.as_ref())?)?;
        net::check_route(&base, &route)?;
        let profile = match input.kind {
            Kind::Compat => input.profile.unwrap_or(Profile::Custom),
            _ => Profile::Custom,
        };
        let defaults = serde_json::to_string(&input.defaults).map_err(|e| e.to_string())?;
        let proxy = serde_json::to_string(&route).map_err(|e| e.to_string())?;
        let t = now();
        let tx = self.conn.transaction().map_err(db_err)?;
        let id = match input.id {
            Some(id) => {
                let old = tx
                    .query_row(
                        "SELECT base_url, kind, profile, allow_data_collection, proxy, COALESCE(api_key, '') != '' FROM providers WHERE id = ?1",
                        [id],
                        |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, i64>(3)? != 0, r.get::<_, String>(4)?, r.get::<_, i64>(5)? != 0)),
                    )
                    .optional()
                    .map_err(db_err)?;
                let (old_base, old_kind, old_profile, old_allow, old_proxy, has_key) = old.ok_or("[AI] NO_PROVIDER: that provider no longer exists")?;
                let host_changed = net::host_of(&old_base) != net::host_of(&base);
                let old_route: Route = serde_json::from_str(&old_proxy).unwrap_or_default();
                // A key was entered for one address and the way to it. It
                // isn't sent to another host, in clear where it went
                // encrypted, or through another route over plain http, until
                // it is entered again.
                let plain_moved = net::plain_http(&base) && (!net::plain_http(&old_base) || old_route != route);
                if (host_changed || plain_moved) && has_key && matches!(input.key, KeyChange::Keep) {
                    return Err("[AI] KEY_HOST: enter the API key again for the new address or route, or remove the key".into());
                }
                // The consent named the host and said what is asked of the
                // provider and how the data gets there: a change to any of
                // that is asked about again.
                let ask_again = host_changed
                    || old_kind != input.kind.as_str()
                    || old_profile != profile.as_str()
                    || (input.allow_data_collection && !old_allow)
                    || net::plain_http(&old_base) != net::plain_http(&base)
                    || (net::plain_http(&base) && old_route != route);
                tx.execute(
                    "UPDATE providers SET name=?2, kind=?3, profile=?4, base_url=?5, model=?6, defaults=?7, context_tokens=?8,
                       allow_data_collection=?9, proxy=?10, updated_at=?11, consent_at = CASE WHEN ?12 THEN NULL ELSE consent_at END
                     WHERE id=?1",
                    params![id, name, input.kind.as_str(), profile.as_str(), base, model, defaults, input.context_tokens, input.allow_data_collection, proxy, t, ask_again],
                )
                .map_err(db_err)?;
                id
            }
            None => {
                tx.execute(
                    "INSERT INTO providers (name, kind, profile, base_url, model, defaults, context_tokens, allow_data_collection, proxy, created_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?10)",
                    params![name, input.kind.as_str(), profile.as_str(), base, model, defaults, input.context_tokens, input.allow_data_collection, proxy, t],
                )
                .map_err(db_err)?;
                tx.last_insert_rowid()
            }
        };
        match &input.key {
            KeyChange::Keep => {}
            KeyChange::Remove => {
                tx.execute("UPDATE providers SET api_key = NULL WHERE id = ?1", [id]).map_err(db_err)?;
            }
            KeyChange::Set { value } => {
                let v = value.trim();
                if v.is_empty() || v.len() > 4096 || v.chars().any(char::is_control) {
                    return Err("[AI] BAD_KEY: the API key is empty, too long, or has a line break".into());
                }
                tx.execute("UPDATE providers SET api_key = ?2 WHERE id = ?1", params![id, v]).map_err(db_err)?;
            }
        }
        let count: i64 = tx.query_row("SELECT COUNT(*) FROM providers WHERE is_default = 1", [], |r| r.get(0)).map_err(db_err)?;
        if input.is_default || count == 0 {
            tx.execute("UPDATE providers SET is_default = (id = ?1)", [id]).map_err(db_err)?;
        }
        tx.commit().map_err(db_err)?;
        Ok(id)
    }

    /// The chats that used the provider are left without one: they ask for a
    /// provider again, never take whichever one is added next.
    pub fn delete_provider(&mut self, id: i64) -> Result<(), String> {
        let tx = self.conn.transaction().map_err(db_err)?;
        tx.execute("DELETE FROM providers WHERE id = ?1", [id]).map_err(db_err)?;
        tx.execute("UPDATE chats SET settings = json_set(settings, '$.provider_id', NULL) WHERE json_extract(settings, '$.provider_id') = ?1", [id])
            .map_err(db_err)?;
        let has_default: i64 = tx.query_row("SELECT COUNT(*) FROM providers WHERE is_default = 1", [], |r| r.get(0)).map_err(db_err)?;
        if has_default == 0 {
            tx.execute("UPDATE providers SET is_default = 1 WHERE id = (SELECT MIN(id) FROM providers)", []).map_err(db_err)?;
        }
        tx.commit().map_err(db_err)?;
        self.compact();
        Ok(())
    }

    pub fn set_consent(&mut self, id: i64, at: Option<i64>) -> Result<(), String> {
        let n = self.conn.execute("UPDATE providers SET consent_at = ?2 WHERE id = ?1", params![id, at]).map_err(db_err)?;
        if n == 0 {
            return Err("[AI] NO_PROVIDER: that provider no longer exists".into());
        }
        Ok(())
    }

    // --- chats ------------------------------------------------------------

    pub fn create_chat(&mut self, title: &str, server: Option<(&str, &str)>, system_prompt: &str, tools: &Value, settings: &ChatSettings) -> Result<i64, String> {
        let t = now();
        self.conn
            .execute(
                "INSERT INTO chats (title, server_uuid, server_label, system_prompt, tools_json, settings, last_activity, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7, ?7)",
                params![title, server.map(|s| s.0), server.map(|s| s.1), system_prompt, tools.to_string(), json_string(settings)?, t],
            )
            .map_err(db_err)?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn chat(&self, id: i64) -> Result<ChatRow, String> {
        self.conn
            .query_row(
                "SELECT id, title, server_uuid, server_label, system_prompt, tools_json, settings FROM chats WHERE id = ?1",
                [id],
                |r| {
                    Ok(ChatRow {
                        id: r.get(0)?,
                        title: r.get(1)?,
                        server_uuid: r.get(2)?,
                        server_label: r.get(3)?,
                        system_prompt: r.get(4)?,
                        tools: serde_json::from_str(&r.get::<_, String>(5)?).unwrap_or(Value::Null),
                        settings: serde_json::from_str(&r.get::<_, String>(6)?).unwrap_or_default(),
                    })
                },
            )
            .optional()
            .map_err(db_err)?
            .ok_or_else(|| "[AI] NO_CHAT: that chat no longer exists".to_string())
    }

    pub fn chats(&self, filter: &ChatFilter, search: &str) -> Result<Vec<ChatSummary>, String> {
        let names: std::collections::HashMap<i64, String> = self
            .providers()?
            .into_iter()
            .map(|p| (p.id, p.name))
            .collect();
        let (clause, arg): (&str, Option<&str>) = match filter {
            ChatFilter::All => ("1=1", None),
            ChatFilter::Plain => ("c.server_uuid IS NULL", None),
            ChatFilter::Server { server_uuid } => ("c.server_uuid = ?1", Some(server_uuid.as_str())),
        };
        let sql = format!(
            "SELECT c.id, c.title, c.server_uuid, c.server_label, c.settings, c.last_activity,
                    (SELECT COUNT(*) FROM turns t WHERE t.chat_id = c.id),
                    (SELECT t.role || '|' || t.ui FROM turns t WHERE t.chat_id = c.id ORDER BY t.seq DESC LIMIT 1)
             FROM chats c WHERE {clause} ORDER BY c.last_activity DESC, c.id DESC"
        );
        let mut st = self.conn.prepare(&sql).map_err(db_err)?;
        let map = |r: &rusqlite::Row| -> rusqlite::Result<(i64, String, Option<String>, Option<String>, String, i64, i64, Option<String>)> {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?))
        };
        let rows: Vec<_> = match arg {
            Some(a) => st.query_map([a], map).map_err(db_err)?.collect::<Result<_, _>>().map_err(db_err)?,
            None => st.query_map([], map).map_err(db_err)?.collect::<Result<_, _>>().map_err(db_err)?,
        };
        let needle = search.trim().to_lowercase();
        Ok(rows
            .into_iter()
            .filter(|r| needle.is_empty() || r.1.to_lowercase().contains(&needle))
            .map(|(id, title, server_uuid, server_label, settings, last_activity, turns, last)| {
                let settings: ChatSettings = serde_json::from_str(&settings).unwrap_or_default();
                let failed = last.as_deref().is_some_and(|l| l.starts_with("user|") && l.contains("\"failed\""));
                ChatSummary {
                    id,
                    title,
                    server_uuid,
                    server_label,
                    provider_name: settings.provider_id.and_then(|p| names.get(&p).cloned()),
                    model: settings.settings.model,
                    last_activity,
                    turns,
                    state: if failed { "failed" } else { "ok" }.into(),
                }
            })
            .collect())
    }

    pub fn summary(&self, id: i64) -> Result<ChatSummary, String> {
        self.chats(&ChatFilter::All, "")?.into_iter().find(|c| c.id == id).ok_or_else(|| "[AI] NO_CHAT: that chat no longer exists".to_string())
    }

    pub fn set_chat_settings(&mut self, id: i64, settings: &ChatSettings) -> Result<(), String> {
        let n = self.conn.execute("UPDATE chats SET settings = ?2, updated_at = ?3 WHERE id = ?1", params![id, json_string(settings)?, now()]).map_err(db_err)?;
        if n == 0 {
            return Err("[AI] NO_CHAT: that chat no longer exists".into());
        }
        Ok(())
    }

    pub fn rename_chat(&mut self, id: i64, title: &str) -> Result<(), String> {
        let title = clean_title(title);
        self.conn.execute("UPDATE chats SET title = ?2, updated_at = ?3 WHERE id = ?1", params![id, title, now()]).map_err(db_err)?;
        Ok(())
    }

    /// A chat goes with what is kept about it in `meta`: SQLite gives a new
    /// chat the id of the last one deleted, which mustn't find the old one's
    /// level note, context or draft.
    /// The activity log keeps the chat's rows, without the chat: they would
    /// point at the next chat given its id.
    /// The chat goes; what it did stays in the activity log (without its
    /// chat) unless `clear_activity`.
    pub fn delete_chat(&mut self, id: i64, clear_activity: bool) -> Result<(), String> {
        self.conn.execute("DELETE FROM chats WHERE id = ?1", [id]).map_err(db_err)?;
        let actions = if clear_activity { "DELETE FROM actions WHERE chat_id = ?1" } else { "UPDATE actions SET chat_id = NULL WHERE chat_id = ?1" };
        self.conn.execute(actions, [id]).map_err(db_err)?;
        for prefix in CHAT_META {
            self.conn.execute("DELETE FROM meta WHERE key = ?1", [format!("{prefix}{id}")]).map_err(db_err)?;
        }
        self.compact();
        Ok(())
    }

    pub fn delete_all_chats(&mut self, clear_activity: bool) -> Result<(), String> {
        let actions = if clear_activity { "DELETE FROM actions WHERE chat_id IS NOT NULL;" } else { "UPDATE actions SET chat_id = NULL;" };
        self.conn.execute_batch(&format!("DELETE FROM chats; {actions}")).map_err(db_err)?;
        for prefix in CHAT_META.iter().chain(["last_chat:"].iter()) {
            self.conn.execute("DELETE FROM meta WHERE substr(key, 1, length(?1)) = ?1", [prefix]).map_err(db_err)?;
        }
        self.compact();
        Ok(())
    }

    pub fn turns(&self, chat_id: i64) -> Result<Vec<TurnRow>, String> {
        let mut st = self.conn.prepare("SELECT seq, canon, renders FROM turns WHERE chat_id = ?1 ORDER BY seq").map_err(db_err)?;
        let rows = st
            .query_map([chat_id], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)))
            .map_err(db_err)?;
        let mut out = Vec::new();
        for row in rows {
            let (seq, canon, renders) = row.map_err(db_err)?;
            let turn: Turn = serde_json::from_str(&canon).map_err(|e| format!("[AI] STORE: a damaged turn ({e})"))?;
            let renders = match serde_json::from_str::<Value>(&renders) {
                Ok(Value::Object(m)) => m,
                _ => Map::new(),
            };
            out.push(TurnRow { seq, turn, renders });
        }
        Ok(out)
    }

    pub fn set_render(&mut self, chat_id: i64, seq: i64, target: &str, entries: &Value) -> Result<(), String> {
        let cur: String = self.conn.query_row("SELECT renders FROM turns WHERE chat_id = ?1 AND seq = ?2", params![chat_id, seq], |r| r.get(0)).map_err(db_err)?;
        let mut map = match serde_json::from_str::<Value>(&cur) {
            Ok(Value::Object(m)) => m,
            _ => Map::new(),
        };
        // Rendered once: a stored rendering is never replaced.
        if map.contains_key(target) {
            return Ok(());
        }
        map.insert(target.into(), entries.clone());
        self.conn
            .execute("UPDATE turns SET renders = ?3 WHERE chat_id = ?1 AND seq = ?2", params![chat_id, seq, Value::Object(map).to_string()])
            .map_err(db_err)?;
        Ok(())
    }

    /// Appends a turn. `wire`, when given, is the exact copy from the provider
    /// that produced the turn (`origin`).
    pub fn append_turn(&mut self, chat_id: i64, turn: &Turn, wire: Option<(&str, &Value)>, model: Option<&str>, ui: &Value) -> Result<i64, String> {
        let seq: i64 = self.conn.query_row("SELECT COALESCE(MAX(seq), 0) + 1 FROM turns WHERE chat_id = ?1", [chat_id], |r| r.get(0)).map_err(db_err)?;
        let role = match turn.role {
            Role::User => "user",
            Role::Assistant => "assistant",
        };
        let mut renders = Map::new();
        if let Some((target, entries)) = wire {
            renders.insert(target.into(), entries.clone());
        }
        let t = now();
        self.conn
            .execute(
                "INSERT INTO turns (chat_id, seq, role, canon, origin, model, renders, ui, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![chat_id, seq, role, json_string(turn)?, wire.map(|w| w.0), model, Value::Object(renders).to_string(), ui.to_string(), t],
            )
            .map_err(db_err)?;
        self.conn.execute("UPDATE chats SET last_activity = ?2, updated_at = ?2 WHERE id = ?1", params![chat_id, t]).map_err(db_err)?;
        Ok(seq)
    }

    pub fn set_turn_ui(&mut self, chat_id: i64, seq: i64, ui: &Value) -> Result<(), String> {
        self.conn.execute("UPDATE turns SET ui = ?3 WHERE chat_id = ?1 AND seq = ?2", params![chat_id, seq, ui.to_string()]).map_err(db_err)?;
        Ok(())
    }

    pub fn turn_ui(&self, chat_id: i64, seq: i64) -> Result<Value, String> {
        let ui: String = self.conn.query_row("SELECT ui FROM turns WHERE chat_id = ?1 AND seq = ?2", params![chat_id, seq], |r| r.get(0)).map_err(db_err)?;
        Ok(serde_json::from_str(&ui).unwrap_or_else(|_| json!({})))
    }

    /// A line in the chat that isn't part of the history the model gets
    /// ("Model changed to …"), shown before the next turn.
    pub fn add_note(&mut self, chat_id: i64, kind: &str, text: &str) -> Result<(), String> {
        let next: i64 = self.conn.query_row("SELECT COALESCE(MAX(seq), 0) + 1 FROM turns WHERE chat_id = ?1", [chat_id], |r| r.get(0)).map_err(db_err)?;
        self.conn
            .execute("INSERT INTO notes (chat_id, before_seq, kind, text, at) VALUES (?1, ?2, ?3, ?4, ?5)", params![chat_id, next, kind, text, now()])
            .map_err(db_err)?;
        Ok(())
    }

    /// What the page shows of a chat: its turns' UI copies and its notes, in order.
    pub fn chat_view(&self, id: i64) -> Result<ChatView, String> {
        let chat = self.chat(id)?;
        let mut items: Vec<(i64, i64, Value)> = Vec::new();
        let mut st = self.conn.prepare("SELECT seq, role, ui FROM turns WHERE chat_id = ?1 ORDER BY seq").map_err(db_err)?;
        for row in st.query_map([id], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))).map_err(db_err)? {
            let (seq, role, ui) = row.map_err(db_err)?;
            let mut v: Value = serde_json::from_str(&ui).unwrap_or_else(|_| json!({}));
            if let Some(o) = v.as_object_mut() {
                o.insert("kind".into(), json!(role));
                o.insert("seq".into(), json!(seq));
            }
            items.push((seq, 1, v));
        }
        let mut st = self.conn.prepare("SELECT before_seq, kind, text FROM notes WHERE chat_id = ?1 ORDER BY id").map_err(db_err)?;
        for row in st.query_map([id], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))).map_err(db_err)? {
            let (before, kind, text) = row.map_err(db_err)?;
            items.push((before, 0, json!({"kind": "note", "note": kind, "text": text})));
        }
        items.sort_by_key(|(seq, order, _)| (*seq, *order));
        let facts = self.chat_facts(id)?.map(|f| f.text);
        // Facts found when the chat started are in its system prompt; ones a
        // later tool call found only steer Submarine's own checks.
        let facts_sent = facts.as_deref().is_some_and(|f| chat.system_prompt.contains(f));
        let context = self.meta(&format!("context:{id}"))?.and_then(|v| serde_json::from_str(&v).ok());
        let draft = self.meta(&format!("draft:{id}"))?;
        let mut st = self.conn.prepare("SELECT id FROM actions WHERE chat_id = ?1 AND undone = 1").map_err(db_err)?;
        let undone = st.query_map([id], |r| r.get(0)).map_err(db_err)?.collect::<Result<Vec<i64>, _>>().map_err(db_err)?;
        Ok(ChatView {
            id: chat.id,
            title: chat.title,
            server_uuid: chat.server_uuid,
            server_label: chat.server_label,
            settings: chat.settings,
            facts,
            facts_sent,
            items: items.into_iter().map(|i| i.2).collect(),
            context,
            draft,
            undone,
        })
    }

    pub fn meta(&self, key: &str) -> Result<Option<String>, String> {
        self.conn.query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0)).optional().map_err(db_err)
    }

    pub fn set_meta(&mut self, key: &str, value: &str) -> Result<(), String> {
        self.conn.execute("INSERT INTO meta (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value", params![key, value]).map_err(db_err)?;
        Ok(())
    }

    pub fn del_meta(&mut self, key: &str) -> Result<(), String> {
        self.conn.execute("DELETE FROM meta WHERE key = ?1", [key]).map_err(db_err)?;
        Ok(())
    }

    // --- servers: levels and always-run ------------------------------------

    /// The level saved for a server; `None` means the default (Ask).
    pub fn level(&self, server_uuid: &str) -> Result<Option<Level>, String> {
        let v: Option<String> = self.conn.query_row("SELECT level FROM server_levels WHERE server_uuid = ?1", [server_uuid], |r| r.get(0)).optional().map_err(db_err)?;
        Ok(v.as_deref().and_then(Level::parse))
    }

    /// Every saved level, by server.
    pub fn levels(&self) -> Result<Vec<(String, Level)>, String> {
        let mut st = self.conn.prepare("SELECT server_uuid, level FROM server_levels").map_err(db_err)?;
        let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))).map_err(db_err)?;
        let mut out = Vec::new();
        for row in rows {
            let (uuid, level) = row.map_err(db_err)?;
            if let Some(l) = Level::parse(&level) {
                out.push((uuid, l));
            }
        }
        Ok(out)
    }

    /// The facts of the newest chat about this server that has them, as saved.
    pub fn latest_facts(&self, server_uuid: &str) -> Result<Option<String>, String> {
        self.conn
            .query_row(
                "SELECT facts FROM chats WHERE server_uuid = ?1 AND facts IS NOT NULL ORDER BY created_at DESC, id DESC LIMIT 1",
                [server_uuid],
                |r| r.get(0),
            )
            .optional()
            .map_err(db_err)
    }

    /// Saves a server's level. Full access is never saved.
    pub fn set_level(&mut self, server_uuid: &str, level: Level) -> Result<(), String> {
        if level == Level::Full {
            return Err("[AI] BAD_LEVEL: Full access is never saved".into());
        }
        self.conn
            .execute(
                "INSERT INTO server_levels (server_uuid, level, updated_at) VALUES (?1, ?2, ?3)
                 ON CONFLICT(server_uuid) DO UPDATE SET level = excluded.level, updated_at = excluded.updated_at",
                params![server_uuid, level.as_str(), now()],
            )
            .map_err(db_err)?;
        Ok(())
    }

    pub fn always(&self, server_uuid: &str) -> Result<Vec<String>, String> {
        let mut st = self.conn.prepare("SELECT command FROM always_run WHERE server_uuid = ?1 ORDER BY added_at, command").map_err(db_err)?;
        let rows = st.query_map([server_uuid], |r| r.get::<_, String>(0)).map_err(db_err)?;
        rows.collect::<Result<_, _>>().map_err(db_err)
    }

    pub fn add_always(&mut self, server_uuid: &str, command: &str) -> Result<(), String> {
        self.conn
            .execute("INSERT OR IGNORE INTO always_run (server_uuid, command, added_at) VALUES (?1, ?2, ?3)", params![server_uuid, command, now()])
            .map_err(db_err)?;
        Ok(())
    }

    pub fn remove_always(&mut self, server_uuid: &str, command: &str) -> Result<(), String> {
        self.conn.execute("DELETE FROM always_run WHERE server_uuid = ?1 AND command = ?2", params![server_uuid, command]).map_err(db_err)?;
        Ok(())
    }

    // --- a server chat's facts and files -----------------------------------

    pub fn chat_facts(&self, chat_id: i64) -> Result<Option<Facts>, String> {
        let v: Option<String> = self.conn.query_row("SELECT facts FROM chats WHERE id = ?1", [chat_id], |r| r.get(0)).optional().map_err(db_err)?.flatten();
        Ok(v.and_then(|f| serde_json::from_str(&f).ok()))
    }

    pub fn set_chat_facts(&mut self, chat_id: i64, facts_json: &str) -> Result<(), String> {
        self.conn.execute("UPDATE chats SET facts = ?2 WHERE id = ?1", params![chat_id, facts_json]).map_err(db_err)?;
        Ok(())
    }

    /// What the chat saw of each file it read or wrote, and the files it wrote.
    pub fn file_states(&self, chat_id: i64) -> Result<(HashMap<String, Stamp>, Vec<String>), String> {
        let mut st = self.conn.prepare("SELECT path, size, mtime, sha256, written FROM file_state WHERE chat_id = ?1").map_err(db_err)?;
        let rows = st
            .query_map([chat_id], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, Option<i64>>(1)?, r.get::<_, Option<i64>>(2)?, r.get::<_, Option<String>>(3)?, r.get::<_, i64>(4)?))
            })
            .map_err(db_err)?;
        let (mut stamps, mut written) = (HashMap::new(), Vec::new());
        for row in rows {
            let (path, size, mtime, sha256, w) = row.map_err(db_err)?;
            if w != 0 {
                written.push(path.clone());
            }
            stamps.insert(path, Stamp { size: size.unwrap_or(0) as u64, mtime: mtime.map(|m| m as u32), sha256 });
        }
        Ok((stamps, written))
    }

    /// Gives every file of the chat a stamp no file matches, as after a write
    /// that failed: each is read again before the next change. The files it
    /// wrote stay marked.
    pub fn forget_stamps(&mut self, chat_id: i64) -> Result<(), String> {
        self.conn
            .execute("UPDATE file_state SET size = ?2, mtime = NULL, sha256 = NULL WHERE chat_id = ?1", params![chat_id, i64::MAX])
            .map_err(db_err)?;
        Ok(())
    }

    /// The files chat `from` wrote become files chat `to` wrote (a chat
    /// continued from it), with a stamp no file matches: running one there
    /// asks, and a change there reads it first. What `from` only read stays
    /// with it.
    pub fn copy_written(&mut self, from: i64, to: i64) -> Result<(), String> {
        self.conn
            .execute(
                "INSERT INTO file_state (chat_id, path, size, mtime, sha256, written)
                 SELECT ?2, path, ?3, NULL, NULL, 1 FROM file_state WHERE chat_id = ?1 AND written != 0
                 ON CONFLICT(chat_id, path) DO UPDATE SET size = excluded.size, mtime = NULL, sha256 = NULL, written = 1",
                params![from, to, i64::MAX],
            )
            .map_err(db_err)?;
        Ok(())
    }

    /// Records a file the chat read or wrote. Once written, it stays marked.
    pub fn set_file_state(&mut self, chat_id: i64, path: &str, stamp: &Stamp, written: bool) -> Result<(), String> {
        self.conn
            .execute(
                "INSERT INTO file_state (chat_id, path, size, mtime, sha256, written) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(chat_id, path) DO UPDATE SET size = excluded.size, mtime = excluded.mtime, sha256 = excluded.sha256,
                   written = MAX(written, excluded.written)",
                params![chat_id, path, stamp.size as i64, stamp.mtime.map(|m| m as i64), stamp.sha256, written as i64],
            )
            .map_err(db_err)?;
        Ok(())
    }

    // --- the activity log ---------------------------------------------------

    /// Logs a call; a write's row also keeps what Undo needs. Rows older than
    /// `ACTIONS_KEPT` go, here and when the store opens.
    pub fn log_action(&mut self, e: &ActionEntry, undo: Option<&Undo>) -> Result<i64, String> {
        let t = now();
        self.conn.execute("DELETE FROM actions WHERE at < ?1", [t - ACTIONS_KEPT]).map_err(db_err)?;
        self.conn
            .execute(
                // A chat deleted while its run wound down leaves the row without one.
                "INSERT INTO actions (chat_id, server_uuid, at, tool, summary, decision, level, exit_code, path, undo, created, after_sha)
                 VALUES ((SELECT id FROM chats WHERE id = ?1), ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                params![
                    e.chat_id,
                    e.server_uuid,
                    t,
                    e.tool,
                    e.summary,
                    e.decision,
                    e.level,
                    e.exit_code.map(|c| c as i64),
                    undo.map(|u| u.path.as_str()),
                    undo.and_then(|u| u.before.as_deref()),
                    undo.is_some_and(|u| u.before.is_none()) as i64,
                    undo.map(|u| u.after_sha.as_str()),
                ],
            )
            .map_err(db_err)?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Logs a call an outside agent made through the agent gateway: the same
    /// row as the assistant's, without a chat, with who asked and under which
    /// grant, and what Undo needs for a change, as for the assistant's.
    pub fn log_agent_action(&mut self, e: &ActionEntry, agent: &str, grant: &str, undo: Option<&Undo>) -> Result<i64, String> {
        let t = now();
        self.conn.execute("DELETE FROM actions WHERE at < ?1", [t - ACTIONS_KEPT]).map_err(db_err)?;
        self.conn
            .execute(
                "INSERT INTO actions (chat_id, server_uuid, at, tool, summary, decision, level, exit_code, agent, grant_id, path, undo, created, after_sha)
                 VALUES (NULL, ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                params![
                    e.server_uuid,
                    t,
                    e.tool,
                    e.summary,
                    e.decision,
                    e.level,
                    e.exit_code.map(|c| c as i64),
                    agent,
                    grant,
                    undo.map(|u| u.path.as_str()),
                    undo.and_then(|u| u.before.as_deref()),
                    undo.is_some_and(|u| u.before.is_none()) as i64,
                    undo.map(|u| u.after_sha.as_str()),
                ],
            )
            .map_err(db_err)?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn log_agent_event(&mut self, e: &AgentEvent) -> Result<(), String> {
        let t = now();
        self.conn.execute("DELETE FROM agent_events WHERE at < ?1", [t - ACTIONS_KEPT]).map_err(db_err)?;
        self.conn
            .execute(
                "INSERT INTO agent_events (at, kind, agent, connection, grant_id, server_uuid, detail) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![t, e.kind, e.agent, e.connection as i64, e.grant, e.server_uuid, e.detail],
            )
            .map_err(db_err)?;
        Ok(())
    }

    /// The newest agent events.
    pub fn agent_events(&self, limit: i64) -> Result<Vec<AgentEventView>, String> {
        let mut st = self
            .conn
            .prepare("SELECT id, at, kind, agent, connection, grant_id, server_uuid, detail FROM agent_events ORDER BY id DESC LIMIT ?1")
            .map_err(db_err)?;
        let rows = st
            .query_map([limit], |r| {
                Ok(AgentEventView { id: r.get(0)?, at: r.get(1)?, kind: r.get(2)?, agent: r.get(3)?, connection: r.get(4)?, grant: r.get(5)?, server_uuid: r.get(6)?, detail: r.get(7)? })
            })
            .map_err(db_err)?;
        rows.collect::<Result<_, _>>().map_err(db_err)
    }

    /// A write's row, for Undo.
    pub fn undo_entry(&self, id: i64) -> Result<UndoEntry, String> {
        self.conn
            .query_row("SELECT chat_id, server_uuid, path, undo, created, after_sha, undone FROM actions WHERE id = ?1 AND path IS NOT NULL", [id], |r| {
                Ok(UndoEntry {
                    chat_id: r.get(0)?,
                    server_uuid: r.get(1)?,
                    path: r.get(2)?,
                    before: r.get(3)?,
                    created: r.get::<_, i64>(4)? != 0,
                    after_sha: r.get::<_, Option<String>>(5)?.unwrap_or_default(),
                    undone: r.get::<_, i64>(6)? != 0,
                })
            })
            .optional()
            .map_err(db_err)?
            .ok_or_else(|| "[AI] NO_ACTION: that change is no longer in the activity log".to_string())
    }

    pub fn mark_undone(&mut self, id: i64) -> Result<(), String> {
        self.conn.execute("UPDATE actions SET undone = 1, undo = NULL WHERE id = ?1", [id]).map_err(db_err)?;
        Ok(())
    }

    /// The newest log rows, for one server or all.
    /// Clears the activity log, of one server or of all.
    pub fn clear_actions(&mut self, server_uuid: Option<&str>) -> Result<(), String> {
        self.conn.execute("DELETE FROM actions WHERE ?1 IS NULL OR server_uuid = ?1", params![server_uuid]).map_err(db_err)?;
        self.conn.execute("DELETE FROM agent_events WHERE ?1 IS NULL OR server_uuid = ?1", params![server_uuid]).map_err(db_err)?;
        self.compact();
        Ok(())
    }

    pub fn actions(&self, server_uuid: Option<&str>, limit: i64) -> Result<Vec<ActionView>, String> {
        let mut st = self
            .conn
            .prepare(
                "SELECT id, chat_id, server_uuid, at, tool, summary, decision, level, exit_code, path IS NOT NULL AND undone = 0, undone, path, agent
                 FROM actions WHERE ?1 IS NULL OR server_uuid = ?1 ORDER BY id DESC LIMIT ?2",
            )
            .map_err(db_err)?;
        let rows = st
            .query_map(params![server_uuid, limit], |r| {
                Ok(ActionView {
                    id: r.get(0)?,
                    chat_id: r.get(1)?,
                    server_uuid: r.get(2)?,
                    at: r.get(3)?,
                    tool: r.get(4)?,
                    summary: r.get(5)?,
                    decision: r.get(6)?,
                    level: r.get(7)?,
                    exit_code: r.get(8)?,
                    can_undo: r.get::<_, i64>(9)? != 0,
                    undone: r.get::<_, i64>(10)? != 0,
                    path: r.get(11)?,
                    agent: r.get(12)?,
                })
            })
            .map_err(db_err)?;
        rows.collect::<Result<_, _>>().map_err(db_err)
    }
}

/// One call for the activity log.
pub struct ActionEntry {
    pub chat_id: i64,
    pub server_uuid: String,
    pub tool: String,
    pub summary: String,
    pub decision: String,
    pub level: String,
    pub exit_code: Option<u32>,
}

pub struct UndoEntry {
    pub chat_id: Option<i64>,
    pub server_uuid: Option<String>,
    pub path: String,
    pub before: Option<Vec<u8>>,
    pub created: bool,
    pub after_sha: String,
    pub undone: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct ActionView {
    pub id: i64,
    pub chat_id: Option<i64>,
    pub server_uuid: Option<String>,
    pub at: i64,
    pub tool: String,
    pub summary: String,
    pub decision: String,
    pub level: String,
    pub exit_code: Option<i64>,
    pub can_undo: bool,
    /// The change was undone.
    pub undone: bool,
    /// The file a write or an edit changed: Undo is asked for it by name.
    pub path: Option<String>,
    /// The outside agent the call came from, through the agent gateway.
    /// `None`: the assistant's own.
    pub agent: Option<String>,
}

/// Something that happened between an outside agent and the user: a
/// connection, a request, an answer, the end of a grant.
pub struct AgentEvent {
    /// connected, disconnected, asked, approved, denied, ended.
    pub kind: String,
    /// Who: the name the agent gave, and what the system says it is.
    pub agent: String,
    pub connection: u64,
    pub grant: Option<String>,
    pub server_uuid: Option<String>,
    /// What exactly: a grant's operations and time, a denial's note, why a
    /// grant ended.
    pub detail: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct AgentEventView {
    pub id: i64,
    pub at: i64,
    pub kind: String,
    pub agent: String,
    pub connection: i64,
    pub grant: Option<String>,
    pub server_uuid: Option<String>,
    pub detail: String,
}

#[cfg(test)]
fn rand_suffix() -> String {
    use rand::Rng;
    let mut b = [0u8; 6];
    rand::rng().fill_bytes(&mut b);
    hex::encode(b)
}

/// What the page sees in place of a manual proxy's password, and sends back
/// to keep it, as it sees only `has_key` for the API key.
pub const PROXY_PASSWORD_SHOWN: &str = "********";

/// A provider as the page sees it: a manual proxy's password hidden.
pub fn for_page(mut view: ProviderView) -> ProviderView {
    view.route = shown_route(&view.route);
    view
}

fn shown_route(route: &Route) -> Route {
    if let Route::Manual { url } = route {
        if let Ok(mut u) = reqwest::Url::parse(url) {
            if u.password().is_some() && u.set_password(Some(PROXY_PASSWORD_SHOWN)).is_ok() {
                return Route::Manual { url: u.as_str().trim_end_matches('/').to_string() };
            }
        }
    }
    route.clone()
}

/// A route from the page with its stand-in password replaced by the stored
/// one: only for the same proxy (scheme, user, host and port), so a password
/// given for one proxy never goes to another.
fn route_from_page(route: &Route, stored: Option<&Route>) -> Result<Route, String> {
    let Route::Manual { url } = route else { return Ok(route.clone()) };
    let Ok(u) = reqwest::Url::parse(url) else { return Ok(route.clone()) };
    if u.password() != Some(PROXY_PASSWORD_SHOWN) {
        return Ok(route.clone());
    }
    let again = || "[AI] PROXY_PASSWORD: enter the proxy's password again for this address".to_string();
    let Some(Route::Manual { url: old }) = stored else { return Err(again()) };
    let o = reqwest::Url::parse(old).map_err(|_| again())?;
    let same = (o.scheme(), o.username(), o.host_str(), o.port_or_known_default()) == (u.scheme(), u.username(), u.host_str(), u.port_or_known_default());
    if !same || o.password().is_none() {
        return Err(again());
    }
    Ok(Route::Manual { url: old.clone() })
}

fn json_string<T: Serialize>(v: &T) -> Result<String, String> {
    serde_json::to_string(v).map_err(|e| format!("[AI] STORE: {e}"))
}

pub fn clean_title(t: &str) -> String {
    let one_line: String = t.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    let trimmed = one_line.split_whitespace().collect::<Vec<_>>().join(" ");
    let short: String = trimmed.chars().take(60).collect();
    if short.is_empty() {
        "New chat".into()
    } else {
        short
    }
}

const PROVIDER_COLS: &str = "id, name, kind, profile, base_url, api_key, model, defaults, context_tokens, allow_data_collection, proxy, consent_at, is_default";

fn provider_row(r: &rusqlite::Row) -> rusqlite::Result<ProviderRow> {
    let base: String = r.get(4)?;
    let key: Option<String> = r.get(5)?;
    let profile = Profile::parse(&r.get::<_, String>(3)?).unwrap_or(Profile::Custom);
    let kind = Kind::parse(&r.get::<_, String>(2)?).unwrap_or(Kind::Compat);
    let view = ProviderView {
        id: r.get(0)?,
        name: r.get(1)?,
        kind,
        profile,
        has_key: key.as_deref().is_some_and(|k| !k.is_empty()),
        model: r.get(6)?,
        defaults: serde_json::from_str(&r.get::<_, String>(7)?).unwrap_or_default(),
        context_tokens: r.get(8)?,
        allow_data_collection: r.get::<_, i64>(9)? != 0,
        route: serde_json::from_str(&r.get::<_, String>(10)?).unwrap_or_default(),
        consent_at: r.get(11)?,
        is_default: r.get::<_, i64>(12)? != 0,
        host: net::host_of(&base),
        plain_http: net::plain_http(&base),
        local: kind == Kind::Compat && profile.local(),
        base_url: base,
    };
    Ok(ProviderRow { view, key: key.map(Zeroizing::new) })
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProviderDefaults {
    #[serde(default)]
    pub effort: Option<String>,
    #[serde(default)]
    pub max_tokens: Option<u32>,
    #[serde(default = "yes")]
    pub show_reasoning: bool,
    #[serde(default = "yes")]
    pub fallback: bool,
}

impl Default for ProviderDefaults {
    fn default() -> Self {
        ProviderDefaults { effort: None, max_tokens: None, show_reasoning: true, fallback: true }
    }
}

fn yes() -> bool {
    true
}

/// A provider as the page sees it: never the key, only whether one is set.
#[derive(Clone, Debug, Serialize)]
pub struct ProviderView {
    pub id: i64,
    pub name: String,
    pub kind: Kind,
    pub profile: Profile,
    pub base_url: String,
    pub has_key: bool,
    pub model: String,
    pub defaults: ProviderDefaults,
    pub context_tokens: Option<u32>,
    pub allow_data_collection: bool,
    pub route: Route,
    pub consent_at: Option<i64>,
    pub is_default: bool,
    pub host: String,
    pub plain_http: bool,
    pub local: bool,
}

pub struct ProviderRow {
    pub view: ProviderView,
    pub key: Option<Zeroizing<String>>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum KeyChange {
    Keep,
    Set { value: String },
    Remove,
}

#[derive(Debug, Deserialize)]
pub struct ProviderInput {
    pub id: Option<i64>,
    pub name: String,
    pub kind: Kind,
    #[serde(default)]
    pub profile: Option<Profile>,
    pub base_url: String,
    pub key: KeyChange,
    pub model: String,
    #[serde(default)]
    pub defaults: ProviderDefaults,
    #[serde(default)]
    pub context_tokens: Option<u32>,
    #[serde(default)]
    pub allow_data_collection: bool,
    #[serde(default)]
    pub route: Route,
    #[serde(default)]
    pub is_default: bool,
}

/// A chat's current settings: its provider, and what goes with each request.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChatSettings {
    pub provider_id: Option<i64>,
    #[serde(flatten)]
    pub settings: Settings,
}

impl Default for ChatSettings {
    fn default() -> Self {
        ChatSettings { provider_id: None, settings: Settings { model: String::new(), effort: None, max_tokens: None, show_reasoning: true, fallback: true } }
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ChatFilter {
    All,
    Plain,
    Server { server_uuid: String },
}

#[derive(Clone, Debug, Serialize)]
pub struct ChatSummary {
    pub id: i64,
    pub title: String,
    pub server_uuid: Option<String>,
    pub server_label: Option<String>,
    pub provider_name: Option<String>,
    pub model: String,
    pub last_activity: i64,
    pub turns: i64,
    /// "ok", or "failed" when the last request didn't get an answer.
    pub state: String,
}

pub struct ChatRow {
    pub id: i64,
    pub title: String,
    pub server_uuid: Option<String>,
    pub server_label: Option<String>,
    pub system_prompt: String,
    pub tools: Value,
    pub settings: ChatSettings,
}

#[derive(Clone, Debug, Serialize)]
pub struct ChatView {
    pub id: i64,
    pub title: String,
    pub server_uuid: Option<String>,
    pub server_label: Option<String>,
    pub settings: ChatSettings,
    /// What the facts probe found out about the server.
    pub facts: Option<String>,
    /// Whether the facts went to the model, in the chat's system prompt.
    pub facts_sent: bool,
    pub items: Vec<Value>,
    /// How full the context was at the last request: `{used, limit}`.
    pub context: Option<Value>,
    /// Text for the composer, from "Continue in a new chat".
    pub draft: Option<String>,
    /// The activity entries of this chat whose change was undone.
    pub undone: Vec<i64>,
}

pub struct TurnRow {
    pub seq: i64,
    /// The neutral copy, with its role.
    pub turn: Turn,
    /// Target key -> the turn's entries for that target; the producing
    /// provider's own copy among them.
    pub renders: Map<String, Value>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_proxy_password_never_reaches_the_page_and_stays_with_its_proxy() {
        let stored = Route::Manual { url: "socks5h://alice:s3cret@proxy.lan:1080".into() };
        let shown = shown_route(&stored);
        assert_eq!(shown, Route::Manual { url: "socks5h://alice:********@proxy.lan:1080".into() });
        assert_eq!(route_from_page(&shown, Some(&stored)).unwrap(), stored, "kept as it was");
        let moved = Route::Manual { url: "socks5h://alice:********@other.lan:1080".into() };
        assert!(route_from_page(&moved, Some(&stored)).unwrap_err().starts_with("[AI] PROXY_PASSWORD"));
        assert!(route_from_page(&shown, None).is_err());
        let typed = Route::Manual { url: "socks5h://alice:new@other.lan:1080".into() };
        assert_eq!(route_from_page(&typed, Some(&stored)).unwrap(), typed);
        let plain = Route::Manual { url: "http://proxy.lan:3128".into() };
        assert_eq!(shown_route(&plain), plain);
    }

    fn provider(store: &mut Store, name: &str, key: KeyChange) -> i64 {
        store
            .save_provider(&ProviderInput {
                id: None,
                name: name.into(),
                kind: Kind::Anthropic,
                profile: None,
                base_url: "https://api.anthropic.com/".into(),
                key,
                model: "claude-opus-5-5".into(),
                defaults: ProviderDefaults::default(),
                context_tokens: None,
                allow_data_collection: false,
                route: Route::System,
                is_default: false,
            })
            .unwrap()
    }

    #[test]
    fn seal_and_open_round_trip_and_a_wrong_key_sets_the_file_aside() {
        let dir = std::env::temp_dir().join(format!("submarine-ai-rt-{}-{}", std::process::id(), rand_suffix()));
        fs::create_dir_all(&dir).unwrap();
        let vault = dir.join("work.submarine");
        let (mut s, notice) = Store::open(&vault, &[9u8; 32], &[2u8; 16], 5).unwrap();
        assert!(notice.is_none());
        let id = provider(&mut s, "Claude", KeyChange::Set { value: "sk-ant-secret".into() });
        s.save().unwrap();
        assert!(dir.join("work.submarine-ai").exists());
        assert!(!dir.join("work.submarine.tmp").exists(), "never the vault's temp name");
        let blob = fs::read(dir.join("work.submarine-ai")).unwrap();
        assert_eq!(&blob[..4], MAGIC);
        assert!(!blob.windows(13).any(|w| w == b"sk-ant-secret"), "the key is encrypted");

        let (s2, _) = Store::open(&vault, &[9u8; 32], &[2u8; 16], 6).unwrap();
        let p = s2.provider(id).unwrap();
        assert_eq!(p.key.as_deref().map(|k| k.as_str()), Some("sk-ant-secret"));
        assert!(p.view.has_key && p.view.is_default);

        let (s3, notice) = Store::open(&vault, &[8u8; 32], &[2u8; 16], 7).unwrap();
        assert!(notice.is_some());
        assert!(s3.providers().unwrap().is_empty());
        assert!(fs::read_dir(&dir).unwrap().flatten().any(|e| e.file_name().to_string_lossy().contains("unreadable")));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn provider_views_never_carry_the_key_and_a_new_host_asks_again() {
        let mut s = Store::for_tests();
        let id = provider(&mut s, "A", KeyChange::Set { value: "k1".into() });
        s.set_consent(id, Some(now())).unwrap();
        let json = serde_json::to_string(&s.providers().unwrap()).unwrap();
        assert!(!json.contains("k1"));
        let mut input = ProviderInput {
            id: Some(id),
            name: "A".into(),
            kind: Kind::Compat,
            profile: Some(Profile::Ollama),
            base_url: "http://localhost:11434/v1".into(),
            key: KeyChange::Keep,
            model: "qwen3".into(),
            defaults: ProviderDefaults::default(),
            context_tokens: Some(4096),
            allow_data_collection: false,
            route: Route::System,
            is_default: false,
        };
        let err = s.save_provider(&input).unwrap_err();
        assert!(err.starts_with("[AI] KEY_HOST"), "the saved key doesn't follow the address: {err}");
        assert_eq!(s.provider(id).unwrap().view.base_url, "https://api.anthropic.com", "nothing changed");
        input.key = KeyChange::Remove;
        s.save_provider(&input).unwrap();
        let v = &s.providers().unwrap()[0];
        assert_eq!(v.consent_at, None, "another host: asked again");
        assert!(!v.has_key && v.plain_http && v.local);
        // Without a key there is nothing to follow it.
        input.key = KeyChange::Keep;
        input.base_url = "http://127.0.0.1:11434/v1".into();
        s.save_provider(&input).unwrap();
        input.key = KeyChange::Set { value: "k2".into() };
        input.base_url = "http://localhost:11434/v1".into();
        s.save_provider(&input).unwrap();
        assert!(s.provider(id).unwrap().view.has_key, "a key entered with the new address");
        input.base_url = "http://8.8.8.8/v1".into();
        assert!(s.save_provider(&input).is_err());
        input.base_url = "http://localhost:11434/v1".into();
        input.route = Route::Manual { url: "http://proxy:3128".into() };
        assert!(s.save_provider(&input).is_err(), "plain http never through a proxy");
    }

    #[test]
    fn chats_turns_renders_and_notes() {
        let mut s = Store::for_tests();
        let p = provider(&mut s, "A", KeyChange::Set { value: "k".into() });
        let settings = ChatSettings { provider_id: Some(p), settings: Settings { model: "claude-opus-5-5".into(), effort: None, max_tokens: None, show_reasoning: true, fallback: true } };
        let c = s.create_chat("New chat", None, "sys", &Value::Null, &settings).unwrap();
        let u = Turn::user_text("hello");
        let seq = s.append_turn(c, &u, None, None, &json!({"text": "hello"})).unwrap();
        s.set_render(c, seq, "anthropic", &json!([{"role": "user", "content": [{"type": "text", "text": "hello"}]}])).unwrap();
        s.set_render(c, seq, "anthropic", &json!(["something else"])).unwrap();
        s.add_note(c, "settings", "Model changed to claude-sonnet-5-5").unwrap();
        let rows = s.turns(c).unwrap();
        assert_eq!(rows[0].renders["anthropic"][0]["role"], "user", "a stored rendering is never replaced");
        let view = s.chat_view(c).unwrap();
        assert_eq!(view.items[0]["kind"], "user");
        assert_eq!(view.items[1]["kind"], "note");
        let list = s.chats(&ChatFilter::Plain, "").unwrap();
        assert_eq!((list.len(), list[0].provider_name.as_deref()), (1, Some("A")));
        assert!(s.chats(&ChatFilter::Server { server_uuid: "x".into() }, "").unwrap().is_empty());
        assert!(s.chats(&ChatFilter::All, "zzz").unwrap().is_empty());
        s.delete_chat(c, false).unwrap();
        assert!(s.turns(c).unwrap().is_empty(), "turns go with their chat");
    }

    #[test]
    fn titles_are_one_short_line() {
        assert_eq!(clean_title("  how do I\nfix nginx?\t "), "how do I fix nginx?");
        assert_eq!(clean_title(""), "New chat");
        assert_eq!(clean_title(&"x".repeat(100)).chars().count(), 60);
    }

    #[test]
    fn a_file_that_cant_be_read_is_newer_or_doesnt_load_stays_as_it_is() {
        let dir = std::env::temp_dir().join(format!("submarine-ai-keep-{}-{}", std::process::id(), rand_suffix()));
        fs::create_dir_all(&dir).unwrap();
        let vault = dir.join("work.submarine");
        let file = file_for(&vault);
        let open = || Store::open(&vault, &[9u8; 32], &[2u8; 16], 1);
        let set_aside = || fs::read_dir(&dir).unwrap().flatten().any(|e| e.file_name().to_string_lossy().contains("unreadable"));

        // A read that fails says nothing about the content: a folder where the file goes.
        fs::create_dir_all(&file).unwrap();
        let err = open().err().expect("a read that fails is an error");
        assert!(err.starts_with("[AI] STORE_READ"), "{err}");
        assert!(file.is_dir() && !set_aside());
        fs::remove_dir(&file).unwrap();

        let mut newer = MAGIC.to_vec();
        newer.push(VERSION + 1);
        newer.extend_from_slice(&[0u8; 64]);
        fs::write(&file, &newer).unwrap();
        let err = open().err().expect("a newer file is an error");
        assert!(err.starts_with("[AI] STORE_NEWER"), "{err}");
        assert_eq!(fs::read(&file).unwrap(), newer, "left as it was");
        assert!(!set_aside());

        // This key opens it, so it is this profile's, but what is inside
        // doesn't decompress, or isn't a database.
        let key = derive_key(&[9u8; 32], &[2u8; 16]);
        let (ct, nonce) = crate::encrypt_with_key(b"not zstd", &key).unwrap();
        let mut not_zstd = MAGIC.to_vec();
        not_zstd.push(VERSION);
        not_zstd.extend_from_slice(&nonce);
        not_zstd.extend_from_slice(&ct);
        for blob in [not_zstd, seal(b"not a database", &key).unwrap()] {
            fs::write(&file, &blob).unwrap();
            let err = open().err().expect("a file this key opens but can't load is an error");
            assert!(err.starts_with("[AI] STORE_UNREADABLE"), "{err}");
            assert_eq!(fs::read(&file).unwrap(), blob, "left as it was");
            assert!(!set_aside());
        }

        // Content that isn't a store at all goes aside.
        fs::write(&file, b"not a store").unwrap();
        let (s, notice) = open().unwrap();
        assert!(notice.is_some() && s.providers().unwrap().is_empty());
        assert!(!file.exists() && set_aside());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_chat_knows_which_of_its_changes_were_undone() {
        let mut s = Store::for_tests();
        let c = s.create_chat("c", None, "sys", &Value::Null, &ChatSettings::default()).unwrap();
        let e = ActionEntry {
            chat_id: c,
            server_uuid: "u".into(),
            tool: "write_file".into(),
            summary: "/etc/a".into(),
            decision: "approved".into(),
            level: "ask".into(),
            exit_code: None,
        };
        let undo = Undo { path: "/etc/a".into(), before: Some(b"old".to_vec()), after_sha: "x".into() };
        let first = s.log_action(&e, Some(&undo)).unwrap();
        s.log_action(&e, Some(&undo)).unwrap();
        assert!(s.chat_view(c).unwrap().undone.is_empty());
        s.mark_undone(first).unwrap();
        assert_eq!(s.chat_view(c).unwrap().undone, [first]);
    }

    #[test]
    fn provider_and_log_ids_are_never_given_out_again() {
        let mut s = Store::for_tests();
        provider(&mut s, "A", KeyChange::Keep);
        let b = provider(&mut s, "B", KeyChange::Keep);
        s.delete_provider(b).unwrap();
        assert!(provider(&mut s, "C", KeyChange::Keep) > b, "a new provider never gets a deleted one's id");

        let entry = ActionEntry {
            chat_id: 0,
            server_uuid: "u".into(),
            tool: "write_file".into(),
            summary: "/etc/a".into(),
            decision: "approved".into(),
            level: "ask".into(),
            exit_code: None,
        };
        let undo = Undo { path: "/etc/a".into(), before: Some(b"old".to_vec()), after_sha: "x".into() };
        let first = s.log_action(&entry, Some(&undo)).unwrap();
        s.clear_actions(None).unwrap();
        let second = s.log_action(&entry, Some(&undo)).unwrap();
        assert!(second > first, "a cleared log never gives its ids out again");
        assert!(s.undo_entry(first).is_err());
        assert_eq!(s.actions(None, 10).unwrap()[0].path.as_deref(), Some("/etc/a"));

        // A deleted chat's activity stays, without its chat, unless the user
        // clears it too.
        let in_chat = |s: &mut Store| {
            let c = s.create_chat("c", None, "sys", &Value::Null, &ChatSettings::default()).unwrap();
            let e = ActionEntry {
                chat_id: c,
                server_uuid: "u".into(),
                tool: "write_file".into(),
                summary: "/etc/a".into(),
                decision: "approved".into(),
                level: "ask".into(),
                exit_code: None,
            };
            s.log_action(&e, Some(&undo)).unwrap();
            c
        };
        let kept = in_chat(&mut s);
        let count = |s: &Store| s.actions(None, 100).unwrap().len();
        let n = count(&s);
        s.delete_chat(kept, false).unwrap();
        assert_eq!(count(&s), n, "kept, without its chat");
        let cleared = in_chat(&mut s);
        s.delete_chat(cleared, true).unwrap();
        assert_eq!(count(&s), n, "the second chat's row went with it");
        in_chat(&mut s);
        s.delete_all_chats(true).unwrap();
        assert_eq!(count(&s), n, "delete all can clear what the chats did too");
    }

    #[test]
    fn deleting_a_provider_leaves_its_chats_without_one() {
        let mut s = Store::for_tests();
        let kept = provider(&mut s, "A", KeyChange::Keep);
        let gone = provider(&mut s, "B", KeyChange::Set { value: "k".into() });
        let on = |p: i64| ChatSettings { provider_id: Some(p), ..ChatSettings::default() };
        let x = s.create_chat("x", None, "sys", &Value::Null, &on(gone)).unwrap();
        let y = s.create_chat("y", None, "sys", &Value::Null, &on(kept)).unwrap();
        s.delete_provider(gone).unwrap();
        assert_eq!(s.chat(x).unwrap().settings, ChatSettings { provider_id: None, ..on(gone) }, "it asks for a provider again");
        assert_eq!(s.chat(y).unwrap().settings.provider_id, Some(kept));
        assert!(s.providers().unwrap()[0].is_default);
    }

    #[test]
    fn consent_is_asked_again_when_what_it_named_changes() {
        let mut s = Store::for_tests();
        let mut input = ProviderInput {
            id: None,
            name: "OR".into(),
            kind: Kind::Compat,
            profile: Some(Profile::OpenRouter),
            base_url: "https://openrouter.ai/api/v1".into(),
            key: KeyChange::Set { value: "k".into() },
            model: "m".into(),
            defaults: ProviderDefaults::default(),
            context_tokens: None,
            allow_data_collection: false,
            route: Route::System,
            is_default: false,
        };
        let id = s.save_provider(&input).unwrap();
        input.id = Some(id);
        input.key = KeyChange::Keep;
        let asked_again = |s: &mut Store, input: &ProviderInput| {
            s.set_consent(id, Some(1)).unwrap();
            s.save_provider(input).unwrap();
            s.provider(id).unwrap().view.consent_at.is_none()
        };
        input.name = "OpenRouter".into();
        assert!(!asked_again(&mut s, &input), "a new name: kept");
        input.allow_data_collection = true;
        assert!(asked_again(&mut s, &input), "data collection allowed");
        input.allow_data_collection = false;
        assert!(!asked_again(&mut s, &input), "turned off again: kept");
        input.route = Route::Direct;
        assert!(!asked_again(&mut s, &input), "an https provider's route: kept");
        input.profile = Some(Profile::Custom);
        assert!(asked_again(&mut s, &input), "another profile");
        input.kind = Kind::OpenAi;
        assert!(asked_again(&mut s, &input), "another kind");

        input.kind = Kind::Compat;
        input.profile = Some(Profile::Ollama);
        input.key = KeyChange::Remove;
        input.base_url = "http://localhost:11434/v1".into();
        assert!(asked_again(&mut s, &input), "another host");
        input.key = KeyChange::Keep;
        input.route = Route::ChatServer;
        assert!(asked_again(&mut s, &input), "a plain-http provider's route");
    }

    #[test]
    fn a_saved_key_doesnt_follow_a_move_to_plain_http_or_another_route() {
        let mut s = Store::for_tests();
        let gateway = |base: &str| ProviderInput {
            id: None,
            name: "Gateway".into(),
            kind: Kind::Compat,
            profile: Some(Profile::Custom),
            base_url: base.into(),
            key: KeyChange::Set { value: "k".into() },
            model: "m".into(),
            defaults: ProviderDefaults::default(),
            context_tokens: None,
            allow_data_collection: false,
            route: Route::Direct,
            is_default: false,
        };
        let refused = |s: &mut Store, input: &ProviderInput| s.save_provider(input).is_err_and(|e| e.starts_with("[AI] KEY_HOST"));

        // The same host to host_of, now in clear: with its port, and on the
        // default ports, where it is another listener too.
        for (from, to) in [("https://gpu.lan:4000/v1", "http://gpu.lan:4000/v1"), ("https://ai.lan/v1", "http://ai.lan/v1")] {
            let mut input = gateway(from);
            let id = s.save_provider(&input).unwrap();
            input.id = Some(id);
            input.key = KeyChange::Keep;
            input.base_url = to.into();
            assert!(refused(&mut s, &input), "{from} to {to}");
            assert_eq!(s.provider(id).unwrap().view.base_url, from, "nothing changed");
        }

        // A key saved with a plain-http address stays for that address and
        // route.
        let mut input = gateway("http://localhost:8000/v1");
        let id = s.save_provider(&input).unwrap();
        input.id = Some(id);
        input.key = KeyChange::Keep;
        input.name = "Local".into();
        s.save_provider(&input).unwrap();
        input.route = Route::ChatServer;
        assert!(refused(&mut s, &input), "a plain-http provider's route");
        assert_eq!(s.provider(id).unwrap().view.route, Route::Direct, "nothing changed");
        input.key = KeyChange::Set { value: "k2".into() };
        s.save_provider(&input).unwrap();
        // Back to the same host over https: the key goes encrypted, and stays.
        input.key = KeyChange::Keep;
        input.route = Route::Direct;
        input.base_url = "https://localhost:8000/v1".into();
        s.save_provider(&input).unwrap();
        assert!(s.provider(id).unwrap().view.has_key);
    }

    #[test]
    fn forgotten_stamps_match_no_file_and_keep_the_written_mark() {
        let mut s = Store::for_tests();
        let a = s.create_chat("a", Some(("u", "web")), "sys", &Value::Null, &ChatSettings::default()).unwrap();
        let b = s.create_chat("b", Some(("u", "web")), "sys", &Value::Null, &ChatSettings::default()).unwrap();
        let seen = Stamp { size: 3, mtime: Some(10), sha256: Some("h".into()) };
        s.set_file_state(a, "/etc/a", &seen, true).unwrap();
        s.set_file_state(a, "/etc/b", &seen, false).unwrap();
        s.set_file_state(b, "/etc/a", &seen, false).unwrap();
        s.forget_stamps(a).unwrap();
        let (stamps, written) = s.file_states(a).unwrap();
        let unknown = Stamp { size: i64::MAX as u64, mtime: None, sha256: None };
        assert_eq!((stamps.len(), stamps.values().all(|st| *st == unknown)), (2, true));
        assert_eq!(written, vec!["/etc/a".to_string()]);
        assert_eq!(s.file_states(b).unwrap().0["/etc/a"], seen, "another chat's stay");
    }

    #[test]
    fn a_continued_chat_keeps_the_files_the_old_one_wrote() {
        let mut s = Store::for_tests();
        let old = s.create_chat("a", Some(("u", "web")), "sys", &Value::Null, &ChatSettings::default()).unwrap();
        let new = s.create_chat("a (continued)", Some(("u", "web")), "sys", &Value::Null, &ChatSettings::default()).unwrap();
        let seen = Stamp { size: 3, mtime: Some(10), sha256: Some("h".into()) };
        s.set_file_state(old, "/home/u/deploy.sh", &seen, true).unwrap();
        s.set_file_state(old, "/etc/hosts", &seen, false).unwrap();
        s.set_file_state(new, "/srv/x", &seen, false).unwrap();
        s.copy_written(old, new).unwrap();
        let (stamps, mut written) = s.file_states(new).unwrap();
        written.sort();
        assert_eq!(written, ["/home/u/deploy.sh"], "written, not only read");
        let unknown = Stamp { size: i64::MAX as u64, mtime: None, sha256: None };
        assert_eq!(stamps["/home/u/deploy.sh"], unknown, "read again before a change");
        assert_eq!((stamps.len(), &stamps["/srv/x"]), (2, &seen), "the new chat's own stay");
        assert_eq!(s.file_states(old).unwrap().0["/home/u/deploy.sh"], seen, "the old chat's stay");
        // Done twice, or from a chat that is gone: nothing changes, no error.
        s.copy_written(old, new).unwrap();
        s.copy_written(999, new).unwrap();
        assert_eq!(s.file_states(new).unwrap().0.len(), 2);
    }

    #[test]
    fn a_save_past_the_cap_rebuilds_and_deletes_shrink_the_image() {
        let mut s = Store::for_tests();
        let text = "secret chat text ".repeat(20_000);
        let big_chat = |s: &mut Store| {
            let c = s.create_chat("big", None, "sys", &Value::Null, &ChatSettings::default()).unwrap();
            s.append_turn(c, &Turn::user_text(&text), None, None, &json!({ "text": text })).unwrap();
            c
        };
        let size = |s: &Store| s.conn.serialize(MAIN_DB).unwrap().len();
        let c = big_chat(&mut s);
        let full = size(&s);
        // Freed pages stay in the image until it is rebuilt.
        s.conn.execute("DELETE FROM chats WHERE id = ?1", [c]).unwrap();
        assert_eq!(size(&s), full);
        s.save_capped(full / 2, usize::MAX).unwrap();
        assert!(size(&s) < full / 2, "rebuilt, it fits");
        big_chat(&mut s);
        let err = s.save_capped(full / 2, usize::MAX).unwrap_err();
        assert!(err.starts_with("[AI] STORE_TOO_LARGE") && err.contains("activity log"), "{err}");

        let c = big_chat(&mut s);
        let before = size(&s);
        s.delete_chat(c, false).unwrap();
        assert!(size(&s) < before, "a deleted chat leaves the image");

        s.save_capped(usize::MAX, 1).unwrap();
        assert!(s.take_notice().is_some_and(|n| n.contains("activity log")));
        s.save_capped(usize::MAX, 1).unwrap();
        assert!(s.take_notice().is_none(), "told once per open store");
    }
}
