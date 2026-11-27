//! Folders the user picked, kept usable after a restart in the Mac App Store
//! build. That build runs in Apple's sandbox, which lets the app use a folder
//! picked in the open panel only until it quits: after a restart, a mirror's
//! folder, the RDP shared folder, the session-log folder, the local pane's
//! folder and the profile's folder copy were refused until picked again. A
//! security-scoped bookmark, saved with each pick, opens the folder again at
//! the next launch (entitlements/appstore.entitlements, bookmarks.app-scope).
//! The bookmarks are this device's own, in the app's data folder, never in a
//! profile. Other builds need none of this: `remember` and `restore` do
//! nothing there.

#[cfg(desktop)]
use std::path::Path;

/// After the user picked `folder` in the open panel: keeps it usable after a
/// restart. When that fails, the folder must be picked again then; the log
/// says why.
#[cfg(desktop)]
pub fn remember(app: &tauri::AppHandle, folder: &Path) {
    #[cfg(all(target_os = "macos", feature = "app-store"))]
    if let Err(e) = mac::file(app).and_then(|file| mac::remember(&file, folder)) {
        eprintln!("[FOLDER] {}: not kept for the next launch: {e}", folder.display());
    }
    #[cfg(not(all(target_os = "macos", feature = "app-store")))]
    let _ = (app, folder);
}

/// At launch: opens the folders picked before, for as long as the app runs.
/// On a thread of its own: a network disk that went away can keep the system
/// busy for a long time, and the window doesn't wait for it. Everything that
/// uses these folders comes after a profile is unlocked.
pub fn restore(app: &tauri::AppHandle) {
    #[cfg(all(target_os = "macos", feature = "app-store"))]
    match mac::file(app) {
        Ok(file) => {
            std::thread::spawn(move || mac::restore(&file));
        }
        Err(e) => eprintln!("[FOLDER] the folders picked before were not opened: {e}"),
    }
    #[cfg(not(all(target_os = "macos", feature = "app-store")))]
    let _ = app;
}

/// The list of kept folders, a JSON file.
#[cfg(any(test, all(target_os = "macos", feature = "app-store")))]
mod store {
    use std::path::Path;
    use std::sync::Mutex;

    /// Folders kept, the newest first; past this many, the oldest go.
    pub const MAX: usize = 64;

    /// Held from reading the list to writing it back: a folder picked while
    /// the ones picked before are being opened isn't lost.
    static FILE: Mutex<()> = Mutex::new(());

    #[derive(serde::Serialize, serde::Deserialize, Clone, Debug, PartialEq)]
    pub struct Entry {
        pub path: String,
        /// The bookmark, in base64.
        pub bookmark: String,
    }

    /// `entry` first: the same folder picked again replaces its old bookmark.
    pub fn put_first(entries: &mut Vec<Entry>, entry: Entry) {
        entries.retain(|e| e.path != entry.path);
        entries.insert(0, entry);
        entries.truncate(MAX);
    }

    /// What opening the kept folders found to change in the list.
    #[derive(Default)]
    pub struct Found {
        /// Bookmarks that can't be read: dropped.
        pub broken: Vec<Entry>,
        /// Bookmarks the system asked to renew, with their new ones.
        pub renewed: Vec<(Entry, String)>,
    }

    impl Found {
        pub fn is_empty(&self) -> bool {
            self.broken.is_empty() && self.renewed.is_empty()
        }

        /// Made to the list as it is now: a folder picked again in the
        /// meantime keeps the bookmark of that pick.
        pub fn apply(&self, entries: &mut Vec<Entry>) {
            entries.retain(|e| !self.broken.contains(e));
            for (old, new) in &self.renewed {
                if let Some(e) = entries.iter_mut().find(|e| **e == *old) {
                    e.bookmark = new.clone();
                }
            }
        }
    }

    /// The kept folders: none when the file isn't there or can't be read.
    pub fn read(file: &Path) -> Vec<Entry> {
        let text = match std::fs::read_to_string(file) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
            Err(e) => {
                eprintln!("[FOLDER] {}: can't be read: {e}", file.display());
                return Vec::new();
            }
        };
        serde_json::from_str(&text).unwrap_or_else(|e| {
            eprintln!("[FOLDER] {}: unreadable, started again: {e}", file.display());
            Vec::new()
        })
    }

    /// Changes the list in the file, with no other change in between.
    pub fn update(file: &Path, change: impl FnOnce(&mut Vec<Entry>)) -> Result<(), String> {
        let _held = FILE.lock().unwrap_or_else(|e| e.into_inner());
        let mut entries = read(file);
        change(&mut entries);
        if let Some(dir) = file.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let text = serde_json::to_string(&entries).map_err(|e| e.to_string())?;
        crate::write_atomic(file, &file.with_extension("json.tmp"), text.as_bytes())
    }
}

#[cfg(all(target_os = "macos", feature = "app-store"))]
mod mac {
    use super::store::{put_first, read, update, Entry, Found};
    use base64::Engine;
    use objc2::rc::{autoreleasepool, Retained};
    use objc2::runtime::Bool;
    use objc2_foundation::{NSData, NSURLBookmarkCreationOptions, NSURLBookmarkResolutionOptions, NSURL};
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;
    use tauri::Manager;

    const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

    /// The folders opened at launch. Held, and never stopped, until the app
    /// quits: the sandbox lets the app use them while they are open.
    static OPEN: Mutex<Vec<Retained<NSURL>>> = Mutex::new(Vec::new());

    pub fn file(app: &tauri::AppHandle) -> Result<PathBuf, String> {
        Ok(app.path().app_data_dir().map_err(|e| e.to_string())?.join("folder-access.json"))
    }

    fn bookmark(url: &NSURL) -> Result<String, String> {
        let data = url
            .bookmarkDataWithOptions_includingResourceValuesForKeys_relativeToURL_error(
                NSURLBookmarkCreationOptions::WithSecurityScope,
                None,
                None,
            )
            .map_err(|e| e.localizedDescription().to_string())?;
        Ok(B64.encode(data.to_vec()))
    }

    pub fn remember(file: &Path, folder: &Path) -> Result<(), String> {
        let bookmark = autoreleasepool(|_| -> Result<String, String> {
            let url = NSURL::from_directory_path(folder).ok_or("not a folder's path")?;
            bookmark(&url)
        })?;
        let entry = Entry { path: folder.to_string_lossy().into_owned(), bookmark };
        update(file, |entries| put_first(entries, entry))
    }

    pub fn restore(file: &Path) {
        let mut found = Found::default();
        let mut open = OPEN.lock().unwrap_or_else(|e| e.into_inner());
        autoreleasepool(|_| {
            for entry in read(file) {
                let Ok(bytes) = B64.decode(&entry.bookmark) else {
                    eprintln!("[FOLDER] {}: a broken bookmark, dropped", entry.path);
                    found.broken.push(entry);
                    continue;
                };
                let mut stale = Bool::NO;
                // SAFETY: `stale` outlives the call.
                let resolved = unsafe {
                    NSURL::URLByResolvingBookmarkData_options_relativeToURL_bookmarkDataIsStale_error(
                        &NSData::with_bytes(&bytes),
                        // No mounting: a network disk that isn't connected
                        // stays so, and the folder opens at a launch when it is.
                        NSURLBookmarkResolutionOptions::WithSecurityScope
                            | NSURLBookmarkResolutionOptions::WithoutUI
                            | NSURLBookmarkResolutionOptions::WithoutMounting,
                        None,
                        &mut stale,
                    )
                };
                // Kept when it doesn't open: the folder may be on a disk that
                // isn't there now.
                let url = match resolved {
                    Ok(url) => url,
                    Err(e) => {
                        eprintln!("[FOLDER] {}: not opened: {}", entry.path, e.localizedDescription());
                        continue;
                    }
                };
                // SAFETY: a URL resolved from a security-scoped bookmark.
                if !unsafe { url.startAccessingSecurityScopedResource() } {
                    eprintln!("[FOLDER] {}: the sandbox didn't open it", entry.path);
                    continue;
                }
                // The system asks for a new bookmark (the folder moved, or
                // macOS was updated): made now, while the folder is open.
                if stale.as_bool() {
                    match bookmark(&url) {
                        Ok(new) => found.renewed.push((entry, new)),
                        Err(e) => eprintln!("[FOLDER] {}: bookmark not renewed: {e}", entry.path),
                    }
                }
                open.push(url);
            }
        });
        if !found.is_empty() {
            if let Err(e) = update(file, |entries| found.apply(entries)) {
                eprintln!("[FOLDER] {}: not saved: {e}", file.display());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::store::{put_first, read, update, Entry, Found, MAX};

    fn entry(path: &str, bookmark: &str) -> Entry {
        Entry { path: path.into(), bookmark: bookmark.into() }
    }

    #[test]
    fn a_folder_picked_again_moves_first_with_its_new_bookmark() {
        let mut list = vec![entry("/a", "1"), entry("/b", "2")];
        put_first(&mut list, entry("/b", "3"));
        assert_eq!(list, vec![entry("/b", "3"), entry("/a", "1")]);
    }

    #[test]
    fn the_oldest_go_past_the_limit() {
        let mut list = Vec::new();
        for i in 0..MAX + 5 {
            put_first(&mut list, entry(&format!("/f{i}"), "b"));
        }
        assert_eq!(list.len(), MAX);
        assert_eq!(list[0].path, format!("/f{}", MAX + 4));
        assert_eq!(list[MAX - 1].path, "/f5");
    }

    #[test]
    fn what_the_launch_found_goes_into_the_list_as_it_is_now() {
        // Nothing found: the file isn't written.
        assert!(Found::default().is_empty());
        let found = Found {
            broken: vec![entry("/a", "1")],
            renewed: vec![(entry("/b", "2"), "2b".into()), (entry("/c", "3"), "3b".into())],
        };
        assert!(!found.is_empty());
        let mut list = vec![entry("/a", "1"), entry("/b", "2"), entry("/c", "3")];
        // Picked again while the launch was opening it: that pick's bookmark stays.
        put_first(&mut list, entry("/c", "9"));
        found.apply(&mut list);
        assert_eq!(list, vec![entry("/c", "9"), entry("/b", "2b")]);
    }

    #[test]
    fn the_list_is_kept_in_a_file() {
        let dir = std::env::temp_dir().join(format!("submarine-folder-access-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let file = dir.join("data").join("folder-access.json");
        // None yet: an empty list, and the folder is made on the first write.
        assert!(read(&file).is_empty());
        update(&file, |list| put_first(list, entry("/Users/me/Projects", "Ym9va21hcms="))).unwrap();
        update(&file, |list| put_first(list, entry("/Volumes/Disk/Logs", "bG9ncw=="))).unwrap();
        assert_eq!(read(&file), vec![entry("/Volumes/Disk/Logs", "bG9ncw=="), entry("/Users/me/Projects", "Ym9va21hcms=")]);
        // A damaged file reads as an empty list, and the next pick starts it again.
        std::fs::write(&file, "{not json").unwrap();
        assert!(read(&file).is_empty());
        update(&file, |list| put_first(list, entry("/Users/me/Projects", "bmV3"))).unwrap();
        assert_eq!(read(&file), vec![entry("/Users/me/Projects", "bmV3")]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
