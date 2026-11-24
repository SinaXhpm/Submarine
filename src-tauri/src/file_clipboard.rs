//! This device's clipboard as a list of files, for the built-in RDP client's
//! file copy (rdp_files.rs): the files the user copied here, to paste on the
//! server, and the server's files once they are here, for the user to paste.
//! Windows (CF_HDROP), macOS (file URLs) and Linux (GTK: `text/uri-list` and
//! GNOME's copied files). A phone has none.

use std::path::PathBuf;

/// The files on this device's clipboard; none when it holds something else.
pub async fn read(app: &tauri::AppHandle) -> Result<Vec<PathBuf>, String> {
    #[cfg(windows)]
    {
        let _ = app;
        tokio::task::spawn_blocking(win::read).await.map_err(|e| e.to_string())?
    }
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        on_main_thread(app, platform::read).await
    }
    #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
    {
        let _ = app;
        Ok(Vec::new())
    }
}

/// Puts `paths` on this device's clipboard, to be pasted as a copy.
pub async fn write(app: &tauri::AppHandle, paths: Vec<PathBuf>) -> Result<(), String> {
    #[cfg(windows)]
    {
        let _ = app;
        tokio::task::spawn_blocking(move || win::write(&paths)).await.map_err(|e| e.to_string())?
    }
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        on_main_thread(app, platform::write(&paths)).await
    }
    #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
    {
        let _ = (app, paths);
        Err("Files can't be put on the clipboard here.".into())
    }
}

/// A mark that changes whenever anything goes on this device's clipboard,
/// from any app; none where that can't be told (Linux).
pub async fn change_mark(app: &tauri::AppHandle) -> Option<u64> {
    #[cfg(windows)]
    {
        let _ = app;
        // SAFETY: reads a counter.
        Some(u64::from(unsafe { windows::Win32::System::DataExchange::GetClipboardSequenceNumber() }))
    }
    #[cfg(target_os = "macos")]
    {
        let count = on_main_thread(app, |tx| {
            let _ = tx.send(Ok(objc2_app_kit::NSPasteboard::generalPasteboard().changeCount()));
        });
        count.await.ok().map(|count| count as u64)
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = app;
        None
    }
}

/// `work` on the main thread, where GTK's clipboard lives (and AppKit's, to
/// be safe), and its answer.
#[cfg(any(target_os = "macos", target_os = "linux"))]
async fn on_main_thread<T: Send + 'static>(
    app: &tauri::AppHandle,
    work: impl FnOnce(tokio::sync::oneshot::Sender<Result<T, String>>) + Send + 'static,
) -> Result<T, String> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.run_on_main_thread(move || work(tx)).map_err(|e| e.to_string())?;
    match tokio::time::timeout(std::time::Duration::from_secs(5), rx).await {
        Ok(Ok(answer)) => answer,
        _ => Err("The clipboard did not answer.".to_string()),
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use super::PathBuf;
    use tokio::sync::oneshot::Sender;

    /// What Nautilus and the other GNOME file managers paste from (`copy`
    /// first: not a cut), and what the others take.
    const GNOME_FILES: &str = "x-special/gnome-copied-files";
    const URI_LIST: &str = "text/uri-list";

    pub fn read(tx: Sender<Result<Vec<PathBuf>, String>>) {
        gtk::Clipboard::get(&gdk::SELECTION_CLIPBOARD).request_uris(move |_, uris| {
            // Files on this computer only: a URI naming another host is passed over.
            let paths = uris
                .iter()
                .filter_map(|uri| gtk::glib::filename_from_uri(uri).ok())
                .filter(|(_, host)| host.as_deref().is_none_or(|h| h == "localhost"))
                .map(|(path, _)| path)
                .collect();
            let _ = tx.send(Ok(paths));
        });
    }

    pub fn write(paths: &[PathBuf]) -> impl FnOnce(Sender<Result<(), String>>) + Send + 'static {
        let uris: Result<Vec<String>, String> =
            paths.iter().map(|p| gtk::glib::filename_to_uri(p, None).map(|u| u.to_string()).map_err(|e| e.to_string())).collect();
        move |tx| {
            let uris = match uris {
                Ok(uris) => uris,
                Err(e) => {
                    let _ = tx.send(Err(e));
                    return;
                }
            };
            let gnome = format!("copy\n{}", uris.join("\n"));
            let list: String = uris.iter().map(|u| format!("{u}\r\n")).collect();
            let targets = [
                gtk::TargetEntry::new(GNOME_FILES, gtk::TargetFlags::empty(), 0),
                gtk::TargetEntry::new(URI_LIST, gtk::TargetFlags::empty(), 1),
            ];
            let set = gtk::Clipboard::get(&gdk::SELECTION_CLIPBOARD).set_with_data(&targets, move |_, data, info| {
                let text = if info == 0 { &gnome } else { &list };
                data.set(&data.target(), 8, text.as_bytes());
            });
            let _ = tx.send(if set { Ok(()) } else { Err("The clipboard didn't take the files.".into()) });
        }
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use super::PathBuf;
    use objc2::rc::Retained;
    use objc2::runtime::ProtocolObject;
    use objc2::ClassType;
    use objc2_app_kit::{NSPasteboard, NSPasteboardWriting};
    use objc2_foundation::{NSArray, NSString, NSURL};
    use tokio::sync::oneshot::Sender;

    pub fn read(tx: Sender<Result<Vec<PathBuf>, String>>) {
        let board = NSPasteboard::generalPasteboard();
        let classes = NSArray::from_slice(&[NSURL::class()]);
        // SAFETY: NSURL reads itself from a pasteboard; no options.
        let items = unsafe { board.readObjectsForClasses_options(&classes, None) };
        // Files on this Mac only: a web address copied is passed over.
        let paths = items
            .map(|items| {
                items
                    .to_vec()
                    .into_iter()
                    .filter_map(|item| item.downcast::<NSURL>().ok())
                    .filter(|url| url.isFileURL())
                    .filter_map(|url| url.path())
                    .map(|path| PathBuf::from(path.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        let _ = tx.send(Ok(paths));
    }

    pub fn write(paths: &[PathBuf]) -> impl FnOnce(Sender<Result<(), String>>) + Send + 'static {
        let paths: Vec<String> = paths.iter().map(|p| p.to_string_lossy().into_owned()).collect();
        move |tx| {
            let urls: Vec<Retained<NSURL>> = paths.iter().map(|p| NSURL::fileURLWithPath(&NSString::from_str(p))).collect();
            let objects: Vec<&ProtocolObject<dyn NSPasteboardWriting>> = urls.iter().map(|url| ProtocolObject::from_ref(&**url)).collect();
            let board = NSPasteboard::generalPasteboard();
            board.clearContents();
            let written = board.writeObjects(&NSArray::from_slice(&objects));
            let _ = tx.send(if written { Ok(()) } else { Err("The clipboard didn't take the files.".into()) });
        }
    }
}

#[cfg(windows)]
mod win {
    use super::PathBuf;
    use windows::core::w;
    use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL};
    use windows::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard, RegisterClipboardFormatW, SetClipboardData};
    use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE};

    /// CF_HDROP: a list of files, as Explorer copies them.
    const CF_HDROP: u32 = 15;
    /// DROPEFFECT_COPY, for "Preferred DropEffect": the files are a copy, not a cut.
    const DROPEFFECT_COPY: u32 = 1;

    /// The clipboard, open for as long as this lives. Another app may have it
    /// open for a moment, so it is tried for half a second.
    struct Open;

    impl Open {
        fn new() -> Result<Self, String> {
            for _ in 0..25 {
                // SAFETY: no window: the clipboard is opened for this thread.
                if unsafe { OpenClipboard(None) }.is_ok() {
                    return Ok(Open);
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err("Another app is using the clipboard.".into())
        }
    }

    impl Drop for Open {
        fn drop(&mut self) {
            // SAFETY: opened by this thread in `new`.
            let _ = unsafe { CloseClipboard() };
        }
    }

    pub fn read() -> Result<Vec<PathBuf>, String> {
        // SAFETY: plain calls; the memory is only read while it is locked.
        unsafe {
            if IsClipboardFormatAvailable(CF_HDROP).is_err() {
                return Ok(Vec::new());
            }
            let _open = Open::new()?;
            let Ok(handle) = GetClipboardData(CF_HDROP) else { return Ok(Vec::new()) };
            let global = HGLOBAL(handle.0);
            let ptr = GlobalLock(global) as *const u8;
            if ptr.is_null() {
                return Ok(Vec::new());
            }
            let bytes = std::slice::from_raw_parts(ptr, GlobalSize(global)).to_vec();
            let _ = GlobalUnlock(global);
            Ok(super::parse_drop_files(&bytes))
        }
    }

    pub fn write(paths: &[PathBuf]) -> Result<(), String> {
        let list = super::drop_files(paths);
        let _open = Open::new()?;
        // SAFETY: the clipboard is open (`_open`).
        unsafe { EmptyClipboard() }.map_err(|e| format!("The clipboard couldn't be emptied: {e}"))?;
        set(CF_HDROP, &list)?;
        // SAFETY: a constant name.
        let preferred = unsafe { RegisterClipboardFormatW(w!("Preferred DropEffect")) };
        if preferred != 0 {
            set(preferred, &DROPEFFECT_COPY.to_le_bytes())?;
        }
        Ok(())
    }

    /// `data` onto the open clipboard as `format`.
    fn set(format: u32, data: &[u8]) -> Result<(), String> {
        let failed = |e: windows::core::Error| format!("The clipboard didn't take the files: {e}");
        // SAFETY: memory of its own, written while locked; the clipboard owns
        // it once it took it, and it is freed here when it didn't.
        unsafe {
            let global = GlobalAlloc(GMEM_MOVEABLE, data.len()).map_err(failed)?;
            let ptr = GlobalLock(global) as *mut u8;
            if ptr.is_null() {
                let _ = GlobalFree(Some(global));
                return Err("The clipboard didn't take the files: out of memory.".into());
            }
            std::ptr::copy_nonoverlapping(data.as_ptr(), ptr, data.len());
            let _ = GlobalUnlock(global);
            if let Err(e) = SetClipboardData(format, Some(HANDLE(global.0))) {
                let _ = GlobalFree(Some(global));
                return Err(failed(e));
            }
        }
        Ok(())
    }
}

/// DROPFILES' size: where the list starts, a point, and two flags, 4 bytes each.
#[cfg(any(windows, test))]
const DROPFILES_LEN: usize = 20;

/// CF_HDROP's data for `paths`: DROPFILES, then each path in UTF-16 with a
/// NUL after it, and one more NUL at the end.
#[cfg(any(windows, test))]
fn drop_files(paths: &[PathBuf]) -> Vec<u8> {
    let mut out = Vec::with_capacity(DROPFILES_LEN + paths.len() * 128);
    out.extend_from_slice(&(DROPFILES_LEN as u32).to_le_bytes());
    out.extend_from_slice(&[0; 12]); // pt.x, pt.y, fNC
    out.extend_from_slice(&1u32.to_le_bytes()); // fWide: UTF-16
    for path in paths {
        for unit in path.as_os_str().to_string_lossy().encode_utf16() {
            out.extend_from_slice(&unit.to_le_bytes());
        }
        out.extend_from_slice(&[0, 0]);
    }
    out.extend_from_slice(&[0, 0]);
    out
}

/// The paths in CF_HDROP's data. A list in the old 8-bit form (no app this
/// side of Windows 95 makes one) reads as none.
#[cfg(any(windows, test))]
fn parse_drop_files(data: &[u8]) -> Vec<PathBuf> {
    let word = |at: usize| data.get(at..at + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
    let (Some(start), Some(wide)) = (word(0), word(16)) else { return Vec::new() };
    if wide == 0 {
        return Vec::new();
    }
    let Some(list) = data.get(start as usize..) else { return Vec::new() };
    let units: Vec<u16> = list.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)).collect();
    units
        .split(|u| *u == 0)
        .take_while(|name| !name.is_empty())
        .map(|name| PathBuf::from(String::from_utf16_lossy(name)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_list_reads_back_as_it_was_written() {
        let paths = vec![PathBuf::from("C:\\Temp\\a.txt"), PathBuf::from("C:\\Temp\\Folder ü 名前"), PathBuf::from("D:\\x")];
        let data = drop_files(&paths);
        assert_eq!(&data[..4], &20u32.to_le_bytes(), "the list starts after DROPFILES");
        assert_eq!(&data[16..20], &1u32.to_le_bytes(), "UTF-16");
        assert!(data.ends_with(&[0, 0, 0, 0]), "a NUL after the last path, and one more");
        assert_eq!(parse_drop_files(&data), paths);
        assert!(parse_drop_files(&drop_files(&[])).is_empty());
    }

    #[test]
    fn a_broken_or_old_file_list_reads_as_none() {
        assert!(parse_drop_files(&[]).is_empty());
        assert!(parse_drop_files(&[20, 0, 0, 0]).is_empty(), "too short for DROPFILES");
        let mut narrow = drop_files(&[PathBuf::from("C:\\a")]);
        narrow[16] = 0;
        assert!(parse_drop_files(&narrow).is_empty(), "8-bit names");
        let mut far = drop_files(&[PathBuf::from("C:\\a")]);
        far[..4].copy_from_slice(&999u32.to_le_bytes());
        assert!(parse_drop_files(&far).is_empty(), "a start past the end");
        // No closing NUL: the names that are there still count.
        let mut open = drop_files(&[PathBuf::from("C:\\a")]);
        open.truncate(open.len() - 4);
        assert_eq!(parse_drop_files(&open), [PathBuf::from("C:\\a")]);
    }
}
