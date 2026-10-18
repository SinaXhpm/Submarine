//! Quick unlock (#166): open a profile with Windows Hello or the phone's
//! fingerprint instead of typing its password. Off until the user turns it on
//! for a profile, on one device, with the profile's password; never synced,
//! exported or backed up.
//!
//! What's kept is the profile's vault key (what its password gives through
//! Argon2, see `derive_key`), sealed so that only this device can open it, and
//! only right after the user is verified there:
//! - Windows: a Windows Hello key (KeyCredentialManager; in the TPM, and not
//!   offered without one) signs a random challenge kept next to the sealed key. The
//!   signature (RSA PKCS#1 v1.5 with SHA-256: the same every time for the same
//!   key and challenge) goes through HKDF into the AES-GCM key that seals the
//!   vault key. Every signature needs the user's PIN, face or finger.
//! - Android: an AES key in the Android Keystore that only works right after
//!   a strong biometric check (BiometricPrompt) and is dropped when a new
//!   fingerprint is enrolled (SubmarinePlugin.kt).
//! The file also holds the vault's salt: a vault replaced since (imported or
//! restored again) doesn't match, and the file is dropped. A copy of the file
//! on another machine (a portable folder moved) can't be opened there: the
//! platform key it needs stays behind.
//!
//! The password itself is never kept. Linux and macOS aren't offered it.

use std::path::PathBuf;

use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use serde::{Deserialize, Serialize};
use tauri::Manager;
use rand::Rng;
use zeroize::Zeroizing;

use crate::{DbState, SALT_LEN};

const FILE_VERSION: u8 = 1;
/// What the UI tells apart: the user closed the prompt (just ask for the
/// password), the file no longer fits this profile (it was dropped: set it up
/// again), or the platform can't do it here.
pub const CANCELLED: &str = "[QUICK_UNLOCK] CANCELLED";
pub const STALE: &str = "[QUICK_UNLOCK] STALE";
pub const UNAVAILABLE: &str = "[QUICK_UNLOCK] UNAVAILABLE";

#[derive(Serialize, Deserialize)]
struct SealedFile {
    v: u8,
    method: String,
    /// The platform key's name: a Windows Hello credential, an Android
    /// Keystore alias. Random, so no profile name ends up in either store.
    id: String,
    /// The vault's salt when this was set up (hex).
    vault_salt: String,
    /// Windows: the challenge the Hello key signs, and the AES-GCM nonce.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    challenge: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    nonce: Option<String>,
    /// The sealed vault key (base64).
    sealed: String,
    /// Android: the GCM IV the Keystore chose.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    iv: Option<String>,
}

/// What the platform keeps of a sealed key (see `seal` / `open`).
struct Sealed {
    challenge: Option<Vec<u8>>,
    nonce: Option<Vec<u8>>,
    sealed: Vec<u8>,
    iv: Option<Vec<u8>>,
}

fn quick_unlock_dir(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    let dir = app.path().app_data_dir().map_err(|e| format!("[SYSTEM] APP_DATA_DIR_NOT_FOUND: {e}"))?;
    Ok(dir.join("quick-unlock"))
}

fn file_for(app: &tauri::AppHandle, name: &str) -> Result<PathBuf, String> {
    crate::validate_profile_name(name)?;
    Ok(quick_unlock_dir(app)?.join(format!("{name}.json")))
}

fn read_file(path: &std::path::Path) -> Option<SealedFile> {
    let text = std::fs::read_to_string(path).ok()?;
    let f: SealedFile = serde_json::from_str(&text).ok()?;
    (f.v == FILE_VERSION && f.method == METHOD).then_some(f)
}

/// The salt in a vault file's header (it never changes for that vault).
fn vault_salt(app: &tauri::AppHandle, name: &str) -> Option<[u8; SALT_LEN]> {
    let data = std::fs::read(crate::profile_path(app, name).ok()?).ok()?;
    let (salt, _, _) = crate::parse_vault_blob(&data).ok()?;
    salt.try_into().ok()
}

/// Removes the file and its platform key. Best effort: nothing to remove is
/// fine. Also called when the profile is deleted.
pub fn forget(app: &tauri::AppHandle, name: &str) {
    let Ok(path) = file_for(app, name) else { return };
    if let Some(f) = read_file(&path) {
        platform::forget(app, &f.id);
    }
    let _ = std::fs::remove_file(&path);
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// AES-GCM key from a Windows Hello signature (see the module doc).
#[cfg_attr(not(windows), allow(dead_code))]
fn wrapping_key(signature: &[u8], challenge: &[u8]) -> Result<Zeroizing<[u8; 32]>, String> {
    let hk = hkdf::Hkdf::<sha2::Sha256>::new(Some(challenge), signature);
    let mut out = Zeroizing::new([0u8; 32]);
    hk.expand(b"submarine quick unlock v1", out.as_mut()).map_err(|_| "[CRYPTO] HKDF".to_string())?;
    Ok(out)
}

#[derive(Serialize)]
pub struct Support {
    /// "windows-hello" / "android-biometric"; none where it isn't offered.
    method: Option<&'static str>,
    /// The device can do it now: Windows Hello is set up, a fingerprint is
    /// enrolled. Else the UI says what to set up first.
    ready: bool,
}

#[tauri::command]
pub async fn quick_unlock_support(app: tauri::AppHandle) -> Result<Support, String> {
    if METHOD.is_empty() {
        return Ok(Support { method: None, ready: false });
    }
    let ready = platform::ready(&app).await;
    Ok(Support { method: Some(METHOD), ready })
}

/// Whether it's on for this profile on this device. A file that no longer
/// fits the profile's vault is dropped here.
#[tauri::command]
pub async fn quick_unlock_status(app: tauri::AppHandle, name: String) -> Result<bool, String> {
    if METHOD.is_empty() {
        return Ok(false);
    }
    let path = file_for(&app, &name)?;
    let Some(f) = read_file(&path) else { return Ok(false) };
    match vault_salt(&app, &name) {
        Some(salt) if hex::encode(salt) == f.vault_salt => Ok(true),
        _ => {
            forget(&app, &name);
            Ok(false)
        }
    }
}

/// Turns it on for the open profile. Takes its password again, so a profile
/// left open can't be set to open with somebody else's finger.
#[tauri::command]
pub async fn quick_unlock_enable(app: tauri::AppHandle, state: tauri::State<'_, DbState>, password: String) -> Result<(), String> {
    if METHOD.is_empty() {
        return Err(UNAVAILABLE.into());
    }
    let password = Zeroizing::new(password);
    let name = state.active_profile.lock().map_err(|_| "[STATE] LOCK_FAILED_PROFILE")?.clone().ok_or("[STATE] NO_PROFILE_SELECTED")?;
    let key = state.master_key.lock().map_err(|_| "[STATE] LOCK_FAILED")?.clone().ok_or("[STATE] LOCKED")?;
    let salt: [u8; SALT_LEN] = (*state.salt.lock().map_err(|_| "[STATE] LOCK_FAILED")?).ok_or("[STATE] LOCKED")?;
    let derived = {
        let salt = salt;
        tokio::task::spawn_blocking(move || crate::derive_key(&password, &salt))
            .await
            .map_err(|e| format!("[CRYPTO] KDF_JOIN: {e}"))??
    };
    let derived = Zeroizing::new(derived);
    if !ct_eq(derived.as_ref(), key.as_ref()) {
        return Err("[QUICK_UNLOCK] WRONG_PASSWORD".into());
    }
    if vault_salt(&app, &name) != Some(salt) {
        // The open vault isn't the one on disk (not saved yet): nothing to seal against.
        return Err("[QUICK_UNLOCK] NOT_SAVED".into());
    }

    forget(&app, &name);
    let mut raw = [0u8; 16];
    rand::rng().fill_bytes(&mut raw);
    let id = format!("submarine-quick-unlock-{}", hex::encode(raw));
    let sealed = platform::seal(&app, &id, &key).await?;
    let file = SealedFile {
        v: FILE_VERSION,
        method: METHOD.into(),
        id,
        vault_salt: hex::encode(salt),
        challenge: sealed.challenge.map(|c| B64.encode(c)),
        nonce: sealed.nonce.map(|n| B64.encode(n)),
        sealed: B64.encode(&sealed.sealed),
        iv: sealed.iv.map(|v| B64.encode(v)),
    };
    let dir = quick_unlock_dir(&app)?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("[FILE] DIR_CREATION_FAILED: {e}"))?;
    let path = file_for(&app, &name)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec(&file).map_err(|e| e.to_string())?).map_err(|e| format!("[FILE] WRITE_FAILED: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&tmp, &path).map_err(|e| format!("[FILE] WRITE_FAILED: {e}"))?;
    Ok(())
}

#[tauri::command]
pub async fn quick_unlock_disable(app: tauri::AppHandle, name: String) -> Result<(), String> {
    file_for(&app, &name)?;
    forget(&app, &name);
    Ok(())
}

/// Opens the profile `name` with Windows Hello or the fingerprint. Never
/// creates one: there has to be a vault the sealed key fits.
#[tauri::command]
pub async fn quick_unlock(app: tauri::AppHandle, state: tauri::State<'_, DbState>, name: String) -> Result<(), String> {
    if METHOD.is_empty() {
        return Err(UNAVAILABLE.into());
    }
    let path = file_for(&app, &name)?;
    let f = read_file(&path).ok_or(STALE)?;
    let salt = vault_salt(&app, &name).ok_or(STALE)?;
    if hex::encode(salt) != f.vault_salt {
        forget(&app, &name);
        return Err(STALE.into());
    }
    let b64 = |s: &Option<String>| -> Result<Option<Vec<u8>>, String> {
        s.as_ref().map(|v| B64.decode(v).map_err(|_| STALE.to_string())).transpose()
    };
    let sealed = Sealed {
        challenge: b64(&f.challenge)?,
        nonce: b64(&f.nonce)?,
        sealed: B64.decode(&f.sealed).map_err(|_| STALE.to_string())?,
        iv: b64(&f.iv)?,
    };
    let key = match platform::open(&app, &f.id, &sealed).await {
        Ok(k) => k,
        Err(e) if e == STALE => {
            forget(&app, &name);
            return Err(e);
        }
        Err(e) => return Err(e),
    };
    *state.active_profile.lock().map_err(|_| "[STATE] LOCK_FAILED_PROFILE")? = Some(name.clone());
    match crate::setup_master_db_inner(app.clone(), crate::VaultSecret::Key(key), state, false).await {
        Ok(_) => Ok(()),
        Err(e) if e.contains("DECRYPT") => {
            // It opened, but doesn't open this vault: drop it.
            forget(&app, &name);
            Err(STALE.into())
        }
        Err(e) => Err(e),
    }
}

// ---------------------------------------------------------------------------
// Windows: Windows Hello.
// ---------------------------------------------------------------------------
#[cfg(windows)]
const METHOD: &str = "windows-hello";

#[cfg(windows)]
mod platform {
    use super::{wrapping_key, Sealed, CANCELLED, STALE, UNAVAILABLE};
    use windows::core::{Array, HSTRING};
    use windows::Security::Credentials::{KeyCredentialCreationOption, KeyCredentialManager, KeyCredentialStatus};
    use rand::Rng;
    use windows::Security::Cryptography::CryptographicBuffer;
    use zeroize::Zeroizing;

    // Opening a key: NotFound means it's gone (the file is stale).
    fn status_err(s: KeyCredentialStatus) -> String {
        if s == KeyCredentialStatus::UserCanceled || s == KeyCredentialStatus::UserPrefersPassword {
            CANCELLED.into()
        } else if s == KeyCredentialStatus::NotFound {
            STALE.into()
        } else {
            format!("{UNAVAILABLE}: Windows Hello said {}", s.0)
        }
    }

    // Making a key: NotFound there means Windows Hello isn't set up.
    fn create_err(s: KeyCredentialStatus) -> String {
        if s == KeyCredentialStatus::UserCanceled || s == KeyCredentialStatus::UserPrefersPassword {
            CANCELLED.into()
        } else if s == KeyCredentialStatus::NotFound {
            format!("{UNAVAILABLE}: Windows Hello isn't set up on this computer")
        } else {
            format!("{UNAVAILABLE}: Windows Hello said {}", s.0)
        }
    }

    fn win(e: windows::core::Error) -> String {
        format!("{UNAVAILABLE}: {}", e.message())
    }

    // The Windows Hello prompt of a desktop app can open behind the app's
    // window: bring it forward once it shows (KeePassXC does the same).
    fn bring_prompt_forward() {
        std::thread::spawn(|| {
            use windows::core::w;
            use windows::Win32::UI::WindowsAndMessaging::{FindWindowW, SetForegroundWindow};
            for _ in 0..40 {
                std::thread::sleep(std::time::Duration::from_millis(75));
                if let Ok(hwnd) = unsafe { FindWindowW(w!("Credential Dialog Xaml Host"), None) } {
                    if !hwnd.is_invalid() {
                        let _ = unsafe { SetForegroundWindow(hwnd) };
                        return;
                    }
                }
            }
        });
    }

    pub async fn ready(_app: &tauri::AppHandle) -> bool {
        tokio::task::spawn_blocking(|| KeyCredentialManager::IsSupportedAsync().and_then(|op| op.join()).unwrap_or(false))
            .await
            .unwrap_or(false)
    }

    fn sign(id: &str, challenge: &[u8]) -> Result<Zeroizing<Vec<u8>>, String> {
        let opened = KeyCredentialManager::OpenAsync(&HSTRING::from(id)).map_err(win)?.join().map_err(win)?;
        let st = opened.Status().map_err(win)?;
        if st != KeyCredentialStatus::Success {
            return Err(status_err(st));
        }
        let cred = opened.Credential().map_err(win)?;
        let data = CryptographicBuffer::CreateFromByteArray(challenge).map_err(win)?;
        let op = cred.RequestSignAsync(&data).map_err(win)?;
        bring_prompt_forward();
        let signed = op.join().map_err(win)?;
        let st = signed.Status().map_err(win)?;
        if st != KeyCredentialStatus::Success {
            return Err(status_err(st));
        }
        let mut out = Array::<u8>::new();
        CryptographicBuffer::CopyToByteArray(&signed.Result().map_err(win)?, &mut out).map_err(win)?;
        Ok(Zeroizing::new(out.to_vec()))
    }

    // Without a TPM, Windows Hello keeps its keys in software, where a program
    // running as the user could copy them: not offered then. With one, Windows
    // keeps them in it (its Passport key provider marks them hardware). Asking
    // Windows to attest the key doesn't tell: a home PC's Windows Hello keys get
    // NotSupported even in a TPM 2.0 that is ready for attestation.
    fn tpm_present() -> bool {
        use windows::Win32::System::TpmBaseServices::{Tbsi_GetDeviceInfo, TBS_SUCCESS, TPM_DEVICE_INFO};
        let mut info = TPM_DEVICE_INFO::default();
        let rc = unsafe { Tbsi_GetDeviceInfo(std::mem::size_of::<TPM_DEVICE_INFO>() as u32, &mut info as *mut _ as *mut core::ffi::c_void) };
        rc == TBS_SUCCESS && info.tpmVersion != 0
    }

    pub async fn seal(_app: &tauri::AppHandle, id: &str, key: &[u8; 32]) -> Result<Sealed, String> {
        let id = id.to_string();
        let key = Zeroizing::new(*key);
        tokio::task::spawn_blocking(move || {
            // Before Windows asks for the PIN: nothing to make without a TPM.
            if !tpm_present() {
                return Err(format!("{UNAVAILABLE}: this computer has no TPM security chip, so Windows Hello would keep the key in software"));
            }
            let op = KeyCredentialManager::RequestCreateAsync(&HSTRING::from(id.as_str()), KeyCredentialCreationOption::ReplaceExisting).map_err(win)?;
            bring_prompt_forward();
            let created = op.join().map_err(win)?;
            let st = created.Status().map_err(win)?;
            if st != KeyCredentialStatus::Success {
                return Err(create_err(st));
            }
            let mut challenge = vec![0u8; 32];
            rand::rng().fill_bytes(&mut challenge);
            let sig = match sign(&id, &challenge) {
                Ok(s) => s,
                Err(e) => {
                    forget_now(&id);
                    return Err(e);
                }
            };
            let wrap = wrapping_key(&sig, &challenge)?;
            let (sealed, nonce) = crate::encrypt_with_key(key.as_ref(), &wrap)?;
            Ok(Sealed { challenge: Some(challenge), nonce: Some(nonce.to_vec()), sealed, iv: None })
        })
        .await
        .map_err(|e| format!("[QUICK_UNLOCK] JOIN: {e}"))?
    }

    pub async fn open(_app: &tauri::AppHandle, id: &str, s: &Sealed) -> Result<Zeroizing<[u8; 32]>, String> {
        let id = id.to_string();
        let challenge = s.challenge.clone().ok_or(STALE)?;
        let nonce = s.nonce.clone().ok_or(STALE)?;
        let sealed = s.sealed.clone();
        tokio::task::spawn_blocking(move || {
            let sig = sign(&id, &challenge)?;
            let wrap = wrapping_key(&sig, &challenge)?;
            let raw = Zeroizing::new(crate::decrypt_with_key(&sealed, &nonce, &wrap).map_err(|_| STALE.to_string())?);
            let key: [u8; 32] = raw.as_slice().try_into().map_err(|_| STALE.to_string())?;
            Ok(Zeroizing::new(key))
        })
        .await
        .map_err(|e| format!("[QUICK_UNLOCK] JOIN: {e}"))?
    }

    fn forget_now(id: &str) {
        let _ = KeyCredentialManager::DeleteAsync(&HSTRING::from(id)).and_then(|op| op.join());
    }

    pub fn forget(_app: &tauri::AppHandle, id: &str) {
        forget_now(id);
    }
}

// ---------------------------------------------------------------------------
// Android: a Keystore key behind BiometricPrompt (SubmarinePlugin.kt).
// ---------------------------------------------------------------------------
#[cfg(target_os = "android")]
const METHOD: &str = "android-biometric";

#[cfg(target_os = "android")]
mod platform {
    use super::{Sealed, B64, CANCELLED, STALE, UNAVAILABLE};
    use base64::Engine as _;
    use serde::Deserialize;
    use zeroize::Zeroizing;

    fn plugin_err(e: String) -> String {
        if e.contains("CANCELLED") {
            CANCELLED.into()
        } else if e.contains("KEY_GONE") {
            STALE.into()
        } else {
            format!("{UNAVAILABLE}: {e}")
        }
    }

    #[derive(Deserialize)]
    struct Ready { ready: bool }
    #[derive(Deserialize)]
    struct SealOut { sealed: String, iv: String }
    #[derive(Deserialize)]
    struct OpenOut { data: String }

    pub async fn ready(app: &tauri::AppHandle) -> bool {
        crate::android_bridge::call::<Ready>(app, "quickUnlockReady", serde_json::json!({})).await.map(|r| r.ready).unwrap_or(false)
    }

    pub async fn seal(app: &tauri::AppHandle, id: &str, key: &[u8; 32]) -> Result<Sealed, String> {
        let data = Zeroizing::new(B64.encode(key));
        let out: SealOut = crate::android_bridge::call(app, "quickUnlockSeal", serde_json::json!({ "alias": id, "data": data.as_str() }))
            .await
            .map_err(plugin_err)?;
        Ok(Sealed {
            challenge: None,
            nonce: None,
            sealed: B64.decode(out.sealed).map_err(|_| UNAVAILABLE.to_string())?,
            iv: Some(B64.decode(out.iv).map_err(|_| UNAVAILABLE.to_string())?),
        })
    }

    pub async fn open(app: &tauri::AppHandle, id: &str, s: &Sealed) -> Result<Zeroizing<[u8; 32]>, String> {
        let iv = s.iv.as_ref().ok_or(STALE)?;
        let out: OpenOut = crate::android_bridge::call(
            app,
            "quickUnlockOpen",
            serde_json::json!({ "alias": id, "sealed": B64.encode(&s.sealed), "iv": B64.encode(iv) }),
        )
        .await
        .map_err(plugin_err)?;
        let raw = Zeroizing::new(B64.decode(out.data).map_err(|_| STALE.to_string())?);
        let key: [u8; 32] = raw.as_slice().try_into().map_err(|_| STALE.to_string())?;
        Ok(Zeroizing::new(key))
    }

    pub fn forget(app: &tauri::AppHandle, id: &str) {
        let app = app.clone();
        let id = id.to_string();
        tauri::async_runtime::spawn(async move {
            let _ = crate::android_bridge::call::<serde_json::Value>(&app, "quickUnlockForget", serde_json::json!({ "alias": id })).await;
        });
    }
}

// ---------------------------------------------------------------------------
// Elsewhere (Linux, macOS): not offered.
// ---------------------------------------------------------------------------
#[cfg(not(any(windows, target_os = "android")))]
const METHOD: &str = "";

#[cfg(not(any(windows, target_os = "android")))]
mod platform {
    use super::{Sealed, UNAVAILABLE};
    use zeroize::Zeroizing;
    pub async fn ready(_app: &tauri::AppHandle) -> bool { false }
    pub async fn seal(_app: &tauri::AppHandle, _id: &str, _key: &[u8; 32]) -> Result<Sealed, String> { Err(UNAVAILABLE.into()) }
    pub async fn open(_app: &tauri::AppHandle, _id: &str, _s: &Sealed) -> Result<Zeroizing<[u8; 32]>, String> { Err(UNAVAILABLE.into()) }
    pub fn forget(_app: &tauri::AppHandle, _id: &str) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_wrapping_key_depends_on_signature_and_challenge() {
        let a = wrapping_key(b"signature-1", b"challenge-1").unwrap();
        assert_eq!(*a, *wrapping_key(b"signature-1", b"challenge-1").unwrap(), "same inputs, same key");
        assert_ne!(*a, *wrapping_key(b"signature-2", b"challenge-1").unwrap());
        assert_ne!(*a, *wrapping_key(b"signature-1", b"challenge-2").unwrap());
    }

    #[test]
    fn a_sealed_key_opens_only_with_its_wrapping_key() {
        let vault_key = [7u8; 32];
        let wrap = wrapping_key(b"sig", b"chal").unwrap();
        let (ct, nonce) = crate::encrypt_with_key(&vault_key, &wrap).unwrap();
        assert_eq!(crate::decrypt_with_key(&ct, &nonce, &wrap).unwrap(), vault_key.to_vec());
        let other = wrapping_key(b"other sig", b"chal").unwrap();
        assert!(crate::decrypt_with_key(&ct, &nonce, &other).is_err());
    }

    #[test]
    fn constant_time_compare() {
        assert!(ct_eq(b"abc", b"abc"));
        assert!(!ct_eq(b"abc", b"abd"));
        assert!(!ct_eq(b"abc", b"ab"));
    }

    #[test]
    fn a_file_of_another_kind_or_version_is_not_read() {
        let dir = std::env::temp_dir().join(format!("qu-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("x.json");
        let good = SealedFile { v: FILE_VERSION, method: METHOD.into(), id: "i".into(), vault_salt: "00".into(), challenge: None, nonce: None, sealed: "AA==".into(), iv: None };
        std::fs::write(&p, serde_json::to_vec(&good).unwrap()).unwrap();
        assert_eq!(read_file(&p).is_some(), true);
        let other = SealedFile { v: FILE_VERSION + 1, ..good };
        std::fs::write(&p, serde_json::to_vec(&other).unwrap()).unwrap();
        assert!(read_file(&p).is_none());
        std::fs::write(&p, b"not json").unwrap();
        assert!(read_file(&p).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
