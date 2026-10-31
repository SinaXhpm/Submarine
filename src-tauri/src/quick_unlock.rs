//! Device-local unlock: silent, current-user DPAPI on Windows; biometrics on Android.
//! Existing vault encryption and password-based opening are unchanged.
use std::path::PathBuf;
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use serde::{Deserialize, Serialize};
use tauri::Manager;
use zeroize::Zeroizing;
use crate::{DbState, SALT_LEN};

pub const CANCELLED: &str = "[QUICK_UNLOCK] CANCELLED";
pub const STALE: &str = "[QUICK_UNLOCK] STALE";
pub const UNAVAILABLE: &str = "[QUICK_UNLOCK] UNAVAILABLE";
#[cfg(windows)]
const FILE_VERSION: u8 = 2;
#[cfg(not(windows))]
const FILE_VERSION: u8 = 1;
// Retain the command/UI method identifier for compatibility with the picker.
// Windows v2 files contain DPAPI data, never Windows Hello material.
#[cfg(windows)]
const METHOD: &str = "windows-hello";
#[cfg(target_os = "android")]
const METHOD: &str = "android-biometric";
#[cfg(not(any(windows, target_os = "android")))]
const METHOD: &str = "";

#[derive(Serialize, Deserialize)]
struct SealedFile {
    v: u8,
    method: String,
    id: String,
    vault_salt: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    challenge: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    nonce: Option<String>,
    sealed: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    iv: Option<String>,
}
struct Sealed {
    challenge: Option<Vec<u8>>,
    nonce: Option<Vec<u8>>,
    sealed: Vec<u8>,
    iv: Option<Vec<u8>>,
}
fn quick_unlock_dir(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    Ok(app.path().app_data_dir().map_err(|e| format!("[SYSTEM] APP_DATA_DIR_NOT_FOUND: {e}"))?.join("quick-unlock"))
}
fn file_for(app: &tauri::AppHandle, name: &str) -> Result<PathBuf, String> {
    crate::validate_profile_name(name)?;
    Ok(quick_unlock_dir(app)?.join(format!("{name}.json")))
}
fn read_file(path: &std::path::Path) -> Option<SealedFile> {
    let f: SealedFile = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    (f.v == FILE_VERSION && f.method == METHOD).then_some(f)
}
#[derive(Debug, PartialEq, Eq)]
enum VaultSalt { Salt([u8; SALT_LEN]), Gone, Unreadable }
fn salt_in(path: &std::path::Path) -> VaultSalt {
    use std::io::{ErrorKind, Read};
    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == ErrorKind::NotFound => return VaultSalt::Gone,
        Err(_) => return VaultSalt::Unreadable,
    };
    let mut head = [0u8; crate::HEADER_LEN + crate::NONCE_LEN];
    match file.read_exact(&mut head) {
        Ok(()) => (),
        Err(e) if e.kind() == ErrorKind::UnexpectedEof => return VaultSalt::Gone,
        Err(_) => return VaultSalt::Unreadable,
    }
    match crate::parse_vault_blob(&head) {
        Ok((salt, _, _)) => salt.try_into().map(VaultSalt::Salt).unwrap_or(VaultSalt::Gone),
        Err(_) => VaultSalt::Gone,
    }
}
fn vault_salt(app: &tauri::AppHandle, name: &str) -> VaultSalt {
    crate::profile_path(app, name).map(|p| salt_in(&p)).unwrap_or(VaultSalt::Gone)
}
pub fn forget(app: &tauri::AppHandle, name: &str) {
    if let Ok(path) = file_for(app, name) {
        if let Some(f) = read_file(&path) { platform::forget(app, &f.id); }
        let _ = std::fs::remove_file(path);
    }
}
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x,y)| acc | (x ^ y)) == 0
}
#[derive(Serialize)]
pub struct Support { method: Option<&'static str>, ready: bool }
#[tauri::command]
pub async fn quick_unlock_support(app: tauri::AppHandle) -> Result<Support, String> {
    if METHOD.is_empty() { return Ok(Support { method: None, ready: false }); }
    Ok(Support { method: Some(METHOD), ready: platform::ready(&app).await })
}
#[tauri::command]
pub async fn quick_unlock_status(app: tauri::AppHandle, name: String) -> Result<bool, String> {
    if METHOD.is_empty() { return Ok(false); }
    let Some(f) = read_file(&file_for(&app, &name)?) else { return Ok(false); };
    match vault_salt(&app, &name) {
        VaultSalt::Salt(salt) if hex::encode(salt) == f.vault_salt => Ok(true),
        VaultSalt::Unreadable => Ok(true),
        _ => { forget(&app, &name); Ok(false) }
    }
}
#[tauri::command]
pub async fn quick_unlock_enable(app: tauri::AppHandle, state: tauri::State<'_, DbState>, password: String) -> Result<(), String> {
    if METHOD.is_empty() { return Err(UNAVAILABLE.into()); }
    let password = Zeroizing::new(password);
    let name = state.active_profile.lock().map_err(|_| "[STATE] LOCK_FAILED_PROFILE")?.clone().ok_or("[STATE] NO_PROFILE_SELECTED")?;
    let key = state.master_key.lock().map_err(|_| "[STATE] LOCK_FAILED")?.clone().ok_or("[STATE] LOCKED")?;
    let salt = (*state.salt.lock().map_err(|_| "[STATE] LOCK_FAILED")?).ok_or("[STATE] LOCKED")?;
    let derived = Zeroizing::new(tokio::task::spawn_blocking(move || crate::derive_key(&password, &salt)).await.map_err(|e| format!("[CRYPTO] KDF_JOIN: {e}"))??);
    if !ct_eq(derived.as_ref(), key.as_ref()) { return Err("[QUICK_UNLOCK] WRONG_PASSWORD".into()); }
    if vault_salt(&app, &name) != VaultSalt::Salt(salt) { return Err("[QUICK_UNLOCK] NOT_SAVED".into()); }
    let path = file_for(&app, &name)?;
    let previous = read_file(&path);
    use rand::Rng;
    let mut raw = [0u8; 16];
    rand::rng().fill_bytes(&mut raw);
    let id = format!("submarine-quick-unlock-{}", hex::encode(raw));
    let sealed = platform::seal(&app, &id, &key).await?;
    let file = SealedFile {
        v: FILE_VERSION, method: METHOD.into(), id: id.clone(), vault_salt: hex::encode(salt),
        challenge: sealed.challenge.map(|v| B64.encode(v)), nonce: sealed.nonce.map(|v| B64.encode(v)),
        sealed: B64.encode(sealed.sealed), iv: sealed.iv.map(|v| B64.encode(v)),
    };
    let written = (|| -> Result<(), String> {
        std::fs::create_dir_all(quick_unlock_dir(&app)?).map_err(|e| format!("[FILE] DIR_CREATION_FAILED: {e}"))?;
        let tmp = path.with_extension("json.tmp");
        let bytes = serde_json::to_vec(&file).map_err(|e| e.to_string())?;
        std::fs::write(&tmp, bytes).map_err(|e| format!("[FILE] WRITE_FAILED: {e}"))?;
        #[cfg(unix)] {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)).map_err(|e| format!("[FILE] PERMISSION_FAILED: {e}"))?;
        }
        std::fs::rename(&tmp, &path).map_err(|e| format!("[FILE] WRITE_FAILED: {e}"))
    })();
    if let Err(e) = written { platform::forget(&app, &id); return Err(e); }
    if let Some(old) = previous { platform::forget(&app, &old.id); }
    Ok(())
}
#[tauri::command]
pub async fn quick_unlock_disable(app: tauri::AppHandle, name: String) -> Result<(), String> {
    let path = file_for(&app, &name)?;
    let old = read_file(&path);
    match std::fs::remove_file(&path) {
        Ok(()) => (),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
        Err(e) => return Err(format!("[FILE] DELETE_FAILED: {e}")),
    }
    if let Some(old) = old { platform::forget(&app, &old.id); }
    Ok(())
}
#[tauri::command]
pub async fn quick_unlock(app: tauri::AppHandle, state: tauri::State<'_, DbState>, name: String) -> Result<(), String> {
    if METHOD.is_empty() { return Err(UNAVAILABLE.into()); }
    let f = read_file(&file_for(&app, &name)?).ok_or(STALE)?;
    let salt = match vault_salt(&app, &name) {
        VaultSalt::Salt(salt) => salt,
        VaultSalt::Gone => return Err(STALE.into()),
        VaultSalt::Unreadable => return Err("[QUICK_UNLOCK] Profile file is temporarily unreadable. Try again or use its password.".into()),
    };
    if hex::encode(salt) != f.vault_salt { forget(&app, &name); return Err(STALE.into()); }
    let decode = |s: &Option<String>| -> Result<Option<Vec<u8>>, String> {
        s.as_ref().map(|v| B64.decode(v).map_err(|_| STALE.to_string())).transpose()
    };
    let sealed = Sealed { challenge: decode(&f.challenge)?, nonce: decode(&f.nonce)?, sealed: B64.decode(&f.sealed).map_err(|_| STALE.to_string())?, iv: decode(&f.iv)? };
    let key = match platform::open(&app, &f.id, &sealed).await {
        Ok(key) => key,
        Err(e) if e == STALE => { forget(&app, &name); return Err(e); }
        Err(e) => return Err(e),
    };
    *state.active_profile.lock().map_err(|_| "[STATE] LOCK_FAILED_PROFILE")? = Some(name.clone());
    match crate::setup_master_db_inner(app.clone(), crate::VaultSecret::Key(key), state, false).await {
        Ok(_) => Ok(()),
        Err(e) if e.contains("DECRYPT") => { forget(&app, &name); Err(STALE.into()) },
        Err(e) => Err(e),
    }
}

#[cfg(windows)]
mod platform {
    use super::{Sealed, STALE, UNAVAILABLE};
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{HLOCAL, LocalFree};
    use windows::Win32::Security::Cryptography::{CryptProtectData, CryptUnprotectData, CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN};
    use zeroize::{Zeroize, Zeroizing};

    struct OwnedBlob(CRYPT_INTEGER_BLOB);
    impl Drop for OwnedBlob {
        fn drop(&mut self) {
            if !self.0.pbData.is_null() {
                unsafe {
                    std::slice::from_raw_parts_mut(self.0.pbData, self.0.cbData as usize).zeroize();
                    let _ = LocalFree(Some(HLOCAL(self.0.pbData.cast())));
                }
            }
        }
    }
    fn dpapi(input: &[u8], protect: bool) -> Result<Zeroizing<Vec<u8>>, String> {
        let n = u32::try_from(input.len()).map_err(|_| UNAVAILABLE.to_string())?;
        let source = CRYPT_INTEGER_BLOB { cbData: n, pbData: input.as_ptr() as *mut u8 };
        let mut output = OwnedBlob(CRYPT_INTEGER_BLOB::default());
        let result = unsafe {
            if protect {
                CryptProtectData(&source, PCWSTR::null(), None, None, None, CRYPTPROTECT_UI_FORBIDDEN, &mut output.0)
            } else {
                CryptUnprotectData(&source, None, None, None, None, CRYPTPROTECT_UI_FORBIDDEN, &mut output.0)
            }
        };
        result.map_err(|e| format!("{UNAVAILABLE}: Windows credential protection failed: {e}"))?;
        if output.0.pbData.is_null() || output.0.cbData == 0 { return Err(STALE.into()); }
        Ok(Zeroizing::new(unsafe { std::slice::from_raw_parts(output.0.pbData, output.0.cbData as usize) }.to_vec()))
    }
    #[cfg(test)]
    pub(super) fn dpapi_for_test(input: &[u8], protect: bool) -> Result<Zeroizing<Vec<u8>>, String> { dpapi(input, protect) }
    pub async fn ready(_app: &tauri::AppHandle) -> bool { true }
    pub async fn seal(_app: &tauri::AppHandle, _id: &str, key: &[u8; 32]) -> Result<Sealed, String> {
        let key = Zeroizing::new(*key);
        tokio::task::spawn_blocking(move || {
            let wrapped = dpapi(key.as_ref(), true)?;
            Ok(Sealed { challenge: None, nonce: None, sealed: wrapped.to_vec(), iv: None })
        }).await.map_err(|e| format!("[QUICK_UNLOCK] JOIN: {e}"))?
    }
    pub async fn open(_app: &tauri::AppHandle, _id: &str, sealed: &Sealed) -> Result<Zeroizing<[u8; 32]>, String> {
        let blob = sealed.sealed.clone();
        tokio::task::spawn_blocking(move || {
            let raw = dpapi(&blob, false)?;
            let key: [u8; 32] = raw.as_slice().try_into().map_err(|_| STALE.to_string())?;
            Ok(Zeroizing::new(key))
        }).await.map_err(|e| format!("[QUICK_UNLOCK] JOIN: {e}"))?
    }
    pub fn forget(_app: &tauri::AppHandle, _id: &str) {}
}
#[cfg(target_os = "android")]
mod platform {
    use super::{Sealed, B64, CANCELLED, STALE, UNAVAILABLE};
    use base64::Engine as _;
    use serde::Deserialize;
    use zeroize::Zeroizing;
    fn plugin_err(e: String) -> String {
        if e.contains("CANCELLED") { CANCELLED.into() }
        else if e.contains("KEY_GONE") { STALE.into() }
        else { format!("{UNAVAILABLE}: {e}") }
    }
    #[derive(Deserialize)] struct Ready { ready: bool }
    #[derive(Deserialize)] struct SealOut { sealed: String, iv: String }
    #[derive(Deserialize)] struct OpenOut { data: String }
    pub async fn ready(app: &tauri::AppHandle) -> bool {
        crate::android_bridge::call::<Ready>(app, "quickUnlockReady", serde_json::json!({})).await.map(|r| r.ready).unwrap_or(false)
    }
    pub async fn seal(app: &tauri::AppHandle, id: &str, key: &[u8; 32]) -> Result<Sealed, String> {
        let data = Zeroizing::new(B64.encode(key));
        let out: SealOut = crate::android_bridge::call(app, "quickUnlockSeal", serde_json::json!({ "alias": id, "data": data.as_str() })).await.map_err(plugin_err)?;
        Ok(Sealed { challenge: None, nonce: None, sealed: B64.decode(out.sealed).map_err(|_| UNAVAILABLE.to_string())?, iv: Some(B64.decode(out.iv).map_err(|_| UNAVAILABLE.to_string())?) })
    }
    pub async fn open(app: &tauri::AppHandle, id: &str, s: &Sealed) -> Result<Zeroizing<[u8; 32]>, String> {
        let iv = s.iv.as_ref().ok_or(STALE)?;
        let out: OpenOut = crate::android_bridge::call(app, "quickUnlockOpen", serde_json::json!({ "alias": id, "sealed": B64.encode(&s.sealed), "iv": B64.encode(iv) })).await.map_err(plugin_err)?;
        let raw = Zeroizing::new(B64.decode(out.data).map_err(|_| STALE.to_string())?);
        Ok(Zeroizing::new(raw.as_slice().try_into().map_err(|_| STALE.to_string())?))
    }
    pub fn forget(app: &tauri::AppHandle, id: &str) {
        let app = app.clone(); let id = id.to_string();
        tauri::async_runtime::spawn(async move {
            let _ = crate::android_bridge::call::<serde_json::Value>(&app, "quickUnlockForget", serde_json::json!({ "alias": id })).await;
        });
    }
}
#[cfg(not(any(windows, target_os = "android")))]
mod platform {
    use super::{Sealed, UNAVAILABLE};
    use zeroize::Zeroizing;
    pub async fn ready(_app: &tauri::AppHandle) -> bool { false }
    pub async fn seal(_app: &tauri::AppHandle, _id: &str, _key: &[u8;32]) -> Result<Sealed,String> { Err(UNAVAILABLE.into()) }
    pub async fn open(_app: &tauri::AppHandle, _id: &str, _s: &Sealed) -> Result<Zeroizing<[u8;32]>,String> { Err(UNAVAILABLE.into()) }
    pub fn forget(_app: &tauri::AppHandle, _id: &str) {}
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn constant_time_comparison() {
        assert!(ct_eq(b"same", b"same"));
        assert!(!ct_eq(b"same", b"diff"));
        assert!(!ct_eq(b"same", b"same!"));
    }
    #[cfg(windows)]
    #[tokio::test]
    async fn dpapi_round_trip_without_ui() {
        let key = [42u8; 32];
        let wrapped = platform::dpapi_for_test(&key, true).unwrap();
        assert_ne!(&wrapped[..], &key[..]);
        let opened = platform::dpapi_for_test(&wrapped, false).unwrap();
        assert_eq!(&opened[..], &key[..]);
        let mut corrupt = wrapped.to_vec();
        let last = corrupt.len() - 1;
        corrupt[last] ^= 1;
        assert!(platform::dpapi_for_test(&corrupt, false).is_err());
    }
}
