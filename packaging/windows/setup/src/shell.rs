//! Shortcuts, pins, jump lists and the folder picker (COM).
use std::ffi::c_void;
use std::path::{Path, PathBuf};

use windows::core::{Interface, HRESULT, HSTRING};
use windows::Win32::Foundation::HWND;
use windows::Win32::Storage::EnhancedStorage::PKEY_AppUserModel_ID;
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, IPersistFile, CLSCTX_INPROC_SERVER,
    COINIT_APARTMENTTHREADED, STGM_READ,
};
use windows::Win32::UI::Shell::PropertiesSystem::IPropertyStore;
use windows::Win32::UI::Shell::{
    ApplicationDestinations, DestinationList, FileOpenDialog, IApplicationDestinations, ICustomDestinationList,
    IFileOpenDialog, IShellItem, IShellLinkW, IStartMenuPinnedList, SHCreateItemFromParsingName, ShellLink, StartMenuPin,
    FOS_FORCEFILESYSTEM, FOS_NOCHANGEDIR, FOS_PATHMUSTEXIST, FOS_PICKFOLDERS, SIGDN_FILESYSPATH, SLGP_RAWPATH,
};

/// COM for this thread while it lives.
pub struct Com(bool);

pub fn com() -> Com {
    let hr: HRESULT = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
    Com(hr.is_ok())
}

impl Drop for Com {
    fn drop(&mut self) {
        if self.0 {
            unsafe { CoUninitialize() };
        }
    }
}

/// A shortcut to `target`, carrying the app's ID so taskbar pins and the jump
/// list group with the running app (the NSIS installer set the same ID).
pub fn create_shortcut(lnk: &Path, target: &Path, app_id: &str) -> windows::core::Result<()> {
    unsafe {
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
        link.SetPath(&HSTRING::from(target))?;
        if let Some(dir) = target.parent() {
            link.SetWorkingDirectory(&HSTRING::from(dir))?;
        }
        link.SetIconLocation(&HSTRING::from(target), 0)?;
        let store: IPropertyStore = link.cast()?;
        store.SetValue(&PKEY_AppUserModel_ID, &PROPVARIANT::from(app_id))?;
        store.Commit()?;
        link.cast::<IPersistFile>()?.Save(&HSTRING::from(lnk), true)?;
    }
    Ok(())
}

/// Where a shortcut points.
pub fn shortcut_target(lnk: &Path) -> Option<PathBuf> {
    if !lnk.is_file() {
        return None;
    }
    unsafe {
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).ok()?;
        link.cast::<IPersistFile>().ok()?.Load(&HSTRING::from(lnk), STGM_READ).ok()?;
        let mut buf = vec![0u16; 32768];
        link.GetPath(&mut buf, std::ptr::null_mut(), SLGP_RAWPATH.0 as u32).ok()?;
        let len = buf.iter().position(|c| *c == 0).unwrap_or(buf.len());
        (len > 0).then(|| PathBuf::from(String::from_utf16_lossy(&buf[..len])))
    }
}

/// Takes a shortcut off the taskbar and Start before it is deleted.
pub fn unpin(lnk: &Path) {
    unsafe {
        let Ok(list) = CoCreateInstance::<_, IStartMenuPinnedList>(&StartMenuPin, None, CLSCTX_INPROC_SERVER) else {
            return;
        };
        if let Ok(item) = SHCreateItemFromParsingName::<_, _, IShellItem>(&HSTRING::from(lnk), None) {
            let _ = list.RemoveFromList(&item);
        }
    }
}

/// Clears the app's jump list (recent and pinned items).
pub fn clear_jump_list(app_id: &str) {
    let id = HSTRING::from(app_id);
    unsafe {
        if let Ok(list) = CoCreateInstance::<_, ICustomDestinationList>(&DestinationList, None, CLSCTX_INPROC_SERVER) {
            let _ = list.DeleteList(&id);
        }
        if let Ok(dest) = CoCreateInstance::<_, IApplicationDestinations>(&ApplicationDestinations, None, CLSCTX_INPROC_SERVER) {
            if dest.SetAppID(&id).is_ok() {
                let _ = dest.RemoveAllDestinations();
            }
        }
    }
}

/// Windows' folder picker. None: cancelled.
pub fn pick_folder(owner: HWND, title: &str, start: &Path) -> Option<PathBuf> {
    unsafe {
        let dialog: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
        let options = dialog.GetOptions().ok()?;
        dialog.SetOptions(options | FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM | FOS_PATHMUSTEXIST | FOS_NOCHANGEDIR).ok()?;
        let _ = dialog.SetTitle(&HSTRING::from(title));
        if let Some(dir) = start.ancestors().find(|p| p.is_dir()) {
            if let Ok(item) = SHCreateItemFromParsingName::<_, _, IShellItem>(&HSTRING::from(dir), None) {
                let _ = dialog.SetFolder(&item);
            }
        }
        dialog.Show(Some(owner)).ok()?;
        let name = dialog.GetResult().ok()?.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
        let path = name.to_string().ok();
        CoTaskMemFree(Some(name.0 as *const c_void));
        path.map(PathBuf::from)
    }
}
