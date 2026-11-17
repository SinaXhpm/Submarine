//! File helpers.
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use windows::core::HSTRING;
use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

/// Runs `f` again for a few seconds while the file is busy: an antivirus
/// scanner holds a new exe for a moment, and a closed app's process can take
/// a little while to let go of its files.
pub fn retry<T>(mut f: impl FnMut() -> io::Result<T>) -> io::Result<T> {
    let mut wait = 50;
    let mut waited = 0;
    loop {
        match f() {
            Err(e) if waited < 8000 && matches!(e.raw_os_error(), Some(5 | 32 | 33)) => {
                std::thread::sleep(Duration::from_millis(wait));
                waited += wait;
                wait = (wait * 2).min(1000);
            }
            other => return other,
        }
    }
}

pub fn remove_file(path: &Path) -> io::Result<()> {
    match retry(|| fs::remove_file(path)) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

/// Free bytes on the drive the folder is (or will be) on.
pub fn free_space(dir: &Path) -> Option<u64> {
    let existing = dir.ancestors().find(|p| p.is_dir())?;
    let mut free = 0u64;
    unsafe { GetDiskFreeSpaceExW(&HSTRING::from(existing), Some(&mut free), None, None).ok()? };
    Some(free)
}

/// Can setup write here? (Program Files can't be written to without
/// administrator rights, for example.)
pub fn check_writable(dir: &Path) -> io::Result<()> {
    let probe = dir.join(format!(".setup-{}.tmp", std::process::id()));
    fs::OpenOptions::new().write(true).create_new(true).open(&probe)?;
    fs::remove_file(&probe)
}

fn key(p: &Path) -> String {
    let s = std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf());
    s.to_string_lossy().replace('/', "\\").trim_end_matches('\\').to_lowercase()
}

/// The same file or folder, by path (Windows paths ignore case).
pub fn same_path(a: &Path, b: &Path) -> bool {
    key(a) == key(b)
}

/// The folder setup installs to when the user picks `chosen`: a folder of the
/// app's own inside it, unless it already is one.
pub fn install_dir_for(chosen: &Path, product: &str) -> PathBuf {
    let named = chosen.file_name().map(|n| n.to_string_lossy().eq_ignore_ascii_case(product)).unwrap_or(false);
    if named {
        chosen.to_path_buf()
    } else {
        chosen.join(product)
    }
}

/// A folder setup can install to: a full path, not a drive's root, not inside
/// Windows' own folder.
pub fn acceptable_dir(dir: &Path, windows_dir: &Path) -> bool {
    dir.is_absolute()
        && dir.parent().is_some()
        && dir.file_name().is_some()
        && !key(dir).starts_with(&(key(windows_dir) + "\\"))
        && !same_path(dir, windows_dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folders() {
        let p = |s: &str| PathBuf::from(s);
        assert_eq!(install_dir_for(&p(r"D:\Apps"), "Submarine"), p(r"D:\Apps\Submarine"));
        assert_eq!(install_dir_for(&p(r"D:\Apps\submarine"), "Submarine"), p(r"D:\Apps\submarine"));
        assert_eq!(install_dir_for(&p(r"D:\"), "Submarine"), p(r"D:\Submarine"));
        let win = p(r"C:\Windows");
        assert!(acceptable_dir(&p(r"C:\Users\a\AppData\Local\Submarine"), &win));
        assert!(!acceptable_dir(&p(r"C:\"), &win));
        assert!(!acceptable_dir(&p(r"C:\Windows\System32\Submarine"), &win));
        assert!(!acceptable_dir(&p(r"Submarine"), &win));
        assert!(acceptable_dir(&p(r"C:\WindowsApps2\Submarine"), &win));
        assert!(same_path(&p(r"C:\Users\A\Submarine\"), &p(r"c:/users/a/submarine")));
    }
}
