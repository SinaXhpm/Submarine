//! A copy of a profile as it was, kept before a sync that deletes rows from it.
//!
//! A tombstone from the sync server is `{uuid, type, stamp, deleted}`: nothing
//! in it proves who sent it (the real fix is to authenticate them, which the
//! sync protocol doesn't do yet). A compromised server, or anyone holding the
//! account's token, can therefore delete every server, login and key of a
//! personal profile at the next sync, and the merge is then saved over the only
//! copy. Until deletions are authenticated, the sync that is about to delete
//! rows first keeps the profile file as it stood: `<name>.submarine.bak-<unix
//! time>`, next to it. The name doesn't end in `.submarine`, so the profile
//! list doesn't show it; renaming it back (or importing it) gets the profile
//! as it was.
//!
//! Best effort by design: a backup that can't be made never stops a sync.

use std::fs;
use std::path::{Path, PathBuf};

/// How many backups of one profile are kept: the newest ones.
pub const KEEP: usize = 5;
/// A new backup is made only when the newest is older than this. Without it,
/// a stream of one-row deletions would push the good copy out of the five;
/// with it, the first copy stays while the others are made at this pace.
pub const MIN_GAP_SECS: u64 = 10 * 60;

fn stem(vault: &Path) -> Option<String> {
    Some(format!("{}.bak-", vault.file_name()?.to_str()?))
}

/// The backups of `vault` that exist, oldest first, with the time in their name.
fn existing(vault: &Path) -> Vec<(u64, PathBuf)> {
    let (Some(dir), Some(stem)) = (vault.parent(), stem(vault)) else {
        return Vec::new();
    };
    let mut found: Vec<(u64, PathBuf)> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_str()?.to_string();
            let when = name.strip_prefix(&stem)?.parse::<u64>().ok()?;
            Some((when, e.path()))
        })
        .collect();
    found.sort();
    found
}

/// Copies `vault` to a new backup, unless there is no file yet or the newest
/// backup is younger than `MIN_GAP_SECS`; then removes all but the newest
/// `KEEP`. Returns the new backup's file name when one was made.
pub fn keep(vault: &Path, now_unix: u64) -> Result<Option<String>, String> {
    if !vault.is_file() {
        return Ok(None);
    }
    let have = existing(vault);
    if have.last().is_some_and(|(when, _)| now_unix.saturating_sub(*when) < MIN_GAP_SECS) {
        return Ok(None);
    }
    let name = format!("{}{now_unix:010}", stem(vault).ok_or("[BACKUP] NAME")?);
    let dst = vault.with_file_name(&name);
    // Whole or not at all: a half copy is never taken for a backup.
    let tmp = vault.with_file_name(format!("{name}.tmp"));
    if let Err(e) = fs::copy(vault, &tmp).and_then(|_| fs::rename(&tmp, &dst)) {
        let _ = fs::remove_file(&tmp);
        return Err(format!("[BACKUP] COPY: {e}"));
    }
    let mut all = existing(vault);
    while all.len() > KEEP {
        let (_, oldest) = all.remove(0);
        let _ = fs::remove_file(oldest);
    }
    Ok(Some(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("submarine-vault-backup-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_profile_as_it_was_is_kept_next_to_it() {
        let dir = temp_dir("kept");
        let vault = dir.join("work.submarine");
        fs::write(&vault, b"the vault before the sync").unwrap();
        let name = keep(&vault, 1_000_000_000).unwrap().expect("a backup is made");
        assert_eq!(name, "work.submarine.bak-1000000000");
        assert_eq!(fs::read(dir.join(&name)).unwrap(), b"the vault before the sync");
        // The profile list takes files that end in .submarine only.
        assert!(!name.ends_with(".submarine"));
        // No half-copy is left lying around.
        assert!(fs::read_dir(&dir).unwrap().flatten().all(|e| !e.file_name().to_string_lossy().ends_with(".tmp")));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn nothing_to_copy_is_no_error() {
        let dir = temp_dir("none");
        assert_eq!(keep(&dir.join("gone.submarine"), 1_000_000_000).unwrap(), None);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_stream_of_deleting_syncs_doesnt_push_the_first_copy_out() {
        let dir = temp_dir("gap");
        let vault = dir.join("p.submarine");
        fs::write(&vault, b"all there").unwrap();
        let t0 = 2_000_000_000u64;
        assert!(keep(&vault, t0).unwrap().is_some());
        fs::write(&vault, b"one gone").unwrap();
        // A sync a minute, each deleting a row: no new copy until the first is old enough.
        for minute in 1..MIN_GAP_SECS / 60 {
            assert_eq!(keep(&vault, t0 + minute * 60).unwrap(), None, "minute {minute}");
        }
        assert!(keep(&vault, t0 + MIN_GAP_SECS).unwrap().is_some());
        let have = existing(&vault);
        assert_eq!(have.len(), 2);
        assert_eq!(fs::read(&have[0].1).unwrap(), b"all there", "the first copy is still the oldest");
        assert_eq!(fs::read(&have[1].1).unwrap(), b"one gone");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_the_newest_few_are_kept_and_other_profiles_are_left_alone() {
        let dir = temp_dir("rotate");
        let vault = dir.join("p.submarine");
        let other = dir.join("q.submarine");
        fs::write(&vault, b"p").unwrap();
        fs::write(&other, b"q").unwrap();
        keep(&other, 3_000_000_000).unwrap();
        for i in 0..(KEEP as u64 + 3) {
            fs::write(&vault, format!("version {i}")).unwrap();
            assert!(keep(&vault, 3_000_000_000 + i * (MIN_GAP_SECS + 1)).unwrap().is_some());
        }
        let have = existing(&vault);
        assert_eq!(have.len(), KEEP);
        // The newest ones: versions 3 to 7.
        assert_eq!(fs::read(&have[0].1).unwrap(), b"version 3");
        assert_eq!(fs::read(&have[KEEP - 1].1).unwrap(), b"version 7");
        assert_eq!(existing(&other).len(), 1, "another profile's backup is not touched");
        let _ = fs::remove_dir_all(&dir);
    }
}
