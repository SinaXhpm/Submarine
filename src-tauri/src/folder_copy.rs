//! A copy of a profile kept in a folder the user chooses (#319): the profile's
//! encrypted file, the same bytes Export writes, replaced after each save. Put
//! the folder in OneDrive, Google Drive or Dropbox and the copy goes with it to
//! the user's other computers, where Import opens it with the profile's
//! password.
//!
//! It is a copy, not sync: what changes in the copy elsewhere doesn't come
//! back. The setting belongs to this device, since a folder here can be missing
//! on another computer: it lives beside the profiles in `folder-copies.json`
//! (profile name -> folder), never in the profile, and never syncs.
//!
//! A save never waits for the copy, nor fails with it: the bytes just saved go
//! to a worker thread, which writes them atomically (a temp file, then a
//! rename) so the copy is always a whole profile. Saves that come faster than
//! the folder takes them replace the one still waiting. The last error is kept
//! for the profile's page until a copy works again.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

/// The settings file, beside the profiles.
const SETTINGS: &str = "folder-copies.json";

fn settings_path(vault: &Path) -> Option<PathBuf> {
    Some(vault.parent()?.join(SETTINGS))
}

/// The profile's name: its file's name without `.submarine`.
fn profile_name(vault: &Path) -> Option<String> {
    vault.file_stem()?.to_str().map(str::to_string)
}

fn read_settings(settings: &Path) -> HashMap<String, String> {
    fs::read_to_string(settings)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

fn write_settings(settings: &Path, map: &HashMap<String, String>) -> Result<(), String> {
    let text = serde_json::to_string_pretty(map).map_err(|e| e.to_string())?;
    crate::write_atomic(settings, &settings.with_extension("json.tmp"), text.as_bytes())
}

/// The folder this device copies the profile at `vault` into, if any.
pub fn folder(vault: &Path) -> Option<PathBuf> {
    let map = read_settings(&settings_path(vault)?);
    map.get(&profile_name(vault)?).map(PathBuf::from)
}

/// Sets the folder for the profile at `vault`, or with `None` stops the copies.
/// The copy already in a folder stays there: it is the user's.
pub fn set(vault: &Path, folder: Option<&Path>) -> Result<(), String> {
    let settings = settings_path(vault).ok_or("This profile has no folder of its own.")?;
    let name = profile_name(vault).ok_or("This profile has no name.")?;
    if let Some(dir) = folder {
        refuse_own_folder(vault, dir)?;
    }
    let mut map = read_settings(&settings);
    match folder {
        Some(dir) => map.insert(name, dir.to_string_lossy().into_owned()),
        None => map.remove(&name),
    };
    write_settings(&settings, &map)?;
    errors().remove(&key(vault));
    Ok(())
}

/// A profile deleted from this device: its setting goes (the copy stays).
pub fn forget(vault: &Path) {
    let _ = set(vault, None);
}

/// Not the profiles' own folder: the copy would be the profile's own file.
fn refuse_own_folder(vault: &Path, dir: &Path) -> Result<(), String> {
    if let (Some(own), Ok(chosen)) = (vault.parent().and_then(|p| fs::canonicalize(p).ok()), fs::canonicalize(dir)) {
        if own == chosen {
            return Err("That is the folder the profiles themselves are in. Choose another one for the copy.".into());
        }
    }
    Ok(())
}

/// Where the copy goes in `dir`: under the profile's own file name.
fn copy_path(dir: &Path, vault: &Path) -> Option<PathBuf> {
    Some(dir.join(vault.file_name()?))
}

// The last error of each profile's copies, by the profile file's path.
static ERRORS: Mutex<Option<HashMap<String, String>>> = Mutex::new(None);
// One copy written at a time: two at once would meet on the temp file.
static WRITING: Mutex<()> = Mutex::new(());

fn key(vault: &Path) -> String {
    vault.to_string_lossy().into_owned()
}

struct Errors(std::sync::MutexGuard<'static, Option<HashMap<String, String>>>);
impl Errors {
    fn remove(&mut self, k: &str) {
        if let Some(map) = self.0.as_mut() {
            map.remove(k);
        }
    }
    fn insert(&mut self, k: String, v: String) {
        self.0.get_or_insert_with(HashMap::new).insert(k, v);
    }
    fn get(&self, k: &str) -> Option<String> {
        self.0.as_ref().and_then(|map| map.get(k).cloned())
    }
}
fn errors() -> Errors {
    Errors(ERRORS.lock().unwrap_or_else(|e| e.into_inner()))
}

/// Writes `blob` as the copy of the profile at `vault` in `dir`.
fn write(vault: &Path, dir: &Path, blob: &[u8]) -> Result<(), String> {
    if !dir.is_dir() {
        return Err(format!("The folder isn't there: {}", dir.display()));
    }
    refuse_own_folder(vault, dir)?;
    let dst = copy_path(dir, vault).ok_or("This profile has no file name.")?;
    let _one = WRITING.lock().unwrap_or_else(|e| e.into_inner());
    crate::write_atomic(&dst, &dst.with_extension("submarine.tmp"), blob)
        .map_err(|e| format!("The copy couldn't be written in {}: {}", dir.display(), e))
}

fn note(vault: &Path, result: &Result<(), String>) {
    let mut errs = errors();
    match result {
        Ok(()) => errs.remove(&key(vault)),
        Err(e) => errs.insert(key(vault), e.clone()),
    }
}

// The copies waiting for the worker: the newest bytes of each profile.
struct Job {
    vault: PathBuf,
    dir: PathBuf,
    blob: Vec<u8>,
}
static QUEUE: Mutex<Vec<Job>> = Mutex::new(Vec::new());
static RUNNING: AtomicBool = AtomicBool::new(false);

/// After a save of the profile at `vault`: the bytes just saved go to its
/// folder too, when it has one. Returns at once.
pub fn after_save(vault: &Path, blob: &[u8]) {
    let Some(dir) = folder(vault) else { return };
    let start = {
        let mut queue = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
        queue.retain(|job| job.vault != vault);
        queue.push(Job { vault: vault.to_path_buf(), dir, blob: blob.to_vec() });
        !RUNNING.swap(true, Ordering::SeqCst)
    };
    if start {
        std::thread::spawn(work);
    }
}

fn work() {
    loop {
        let job = {
            let mut queue = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
            if queue.is_empty() {
                RUNNING.store(false, Ordering::SeqCst);
                return;
            }
            queue.remove(0)
        };
        let result = write(&job.vault, &job.dir, &job.blob);
        note(&job.vault, &result);
    }
}

/// A copy now, of the profile's file as it is on disk.
pub fn copy_now(vault: &Path) -> Result<(), String> {
    let dir = folder(vault).ok_or("No folder is set for a copy of this profile.")?;
    let blob = crate::read_vault_file(vault)?;
    let result = write(vault, &dir, &blob);
    note(vault, &result);
    result
}

/// What the profile's page shows.
#[derive(serde::Serialize, Debug, PartialEq)]
pub struct Status {
    /// The folder copies go to; none when they're off.
    pub folder: Option<String>,
    /// When the copy there was last written, in Unix seconds.
    pub copied_at: Option<u64>,
    /// Why the last copy failed, until one works.
    pub error: Option<String>,
}

pub fn status(vault: &Path) -> Status {
    let dir = folder(vault);
    let copied_at = dir
        .as_deref()
        .and_then(|d| copy_path(d, vault))
        .and_then(|p| fs::metadata(p).ok())
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs());
    Status {
        folder: dir.map(|d| d.to_string_lossy().into_owned()),
        copied_at,
        error: errors().get(&key(vault)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("submarine-folder-copy-{}-{}", name, std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    // A profiles folder with one profile, and a folder for its copy.
    fn setup(name: &str) -> (PathBuf, PathBuf) {
        let root = temp_dir(name);
        let profiles = root.join("profiles");
        let dest = root.join("OneDrive");
        fs::create_dir_all(&profiles).unwrap();
        fs::create_dir_all(&dest).unwrap();
        let vault = profiles.join("Work.submarine");
        fs::write(&vault, b"the profile, encrypted").unwrap();
        (vault, dest)
    }

    fn wait_for_worker() {
        for _ in 0..200 {
            if !RUNNING.load(Ordering::SeqCst) && QUEUE.lock().unwrap().is_empty() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("the worker didn't finish");
    }

    #[test]
    fn the_setting_is_kept_beside_the_profiles_and_can_be_stopped() {
        let (vault, dest) = setup("setting");
        assert_eq!(folder(&vault), None);
        set(&vault, Some(&dest)).unwrap();
        assert_eq!(folder(&vault), Some(dest.clone()));
        assert!(vault.parent().unwrap().join(SETTINGS).exists());
        // Another profile's setting is its own.
        assert_eq!(folder(&vault.with_file_name("Home.submarine")), None);
        set(&vault, None).unwrap();
        assert_eq!(folder(&vault), None);
    }

    #[test]
    fn a_save_writes_the_same_bytes_to_the_folder() {
        let (vault, dest) = setup("save");
        set(&vault, Some(&dest)).unwrap();
        after_save(&vault, b"saved once");
        after_save(&vault, b"saved twice");
        wait_for_worker();
        assert_eq!(fs::read(dest.join("Work.submarine")).unwrap(), b"saved twice");
        assert!(!dest.join("Work.submarine.tmp").exists());
        let st = status(&vault);
        assert_eq!(st.folder.as_deref(), Some(dest.to_string_lossy().as_ref()));
        assert!(st.copied_at.is_some());
        assert_eq!(st.error, None);
    }

    #[test]
    fn without_a_folder_a_save_writes_nothing() {
        let (vault, dest) = setup("off");
        after_save(&vault, b"saved");
        wait_for_worker();
        assert!(!dest.join("Work.submarine").exists());
        assert_eq!(status(&vault), Status { folder: None, copied_at: None, error: None });
    }

    #[test]
    fn a_missing_folder_is_reported_until_a_copy_works() {
        let (vault, dest) = setup("missing");
        set(&vault, Some(&dest)).unwrap();
        fs::remove_dir_all(&dest).unwrap();
        after_save(&vault, b"saved");
        wait_for_worker();
        let err = status(&vault).error.expect("an error");
        assert!(err.contains("isn't there"), "{err}");
        fs::create_dir_all(&dest).unwrap();
        copy_now(&vault).unwrap();
        assert_eq!(status(&vault).error, None);
        assert_eq!(fs::read(dest.join("Work.submarine")).unwrap(), b"the profile, encrypted");
    }

    #[test]
    fn the_profiles_own_folder_is_refused() {
        let (vault, _) = setup("own");
        let own = vault.parent().unwrap().to_path_buf();
        assert!(set(&vault, Some(&own)).is_err());
        assert_eq!(folder(&vault), None);
        assert_eq!(fs::read(&vault).unwrap(), b"the profile, encrypted");
    }

    #[test]
    fn forgetting_a_profile_keeps_its_copy() {
        let (vault, dest) = setup("forget");
        set(&vault, Some(&dest)).unwrap();
        copy_now(&vault).unwrap();
        forget(&vault);
        assert_eq!(folder(&vault), None);
        assert!(dest.join("Work.submarine").exists());
    }
}
