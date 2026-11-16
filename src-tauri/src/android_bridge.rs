//! The app's own Android plugin (gen/android/.../SubmarinePlugin.kt): profile
//! export through the system's "Save to" picker, the "Block screenshots"
//! switch, quick unlock, and the keep-alive service while a connection is
//! open. Only Rust reaches it. The plugin has no permissions in the
//! capabilities, and Tauri refuses a plugin command without one, so the page
//! can't call it directly.

use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};
use tauri::plugin::{Builder, PluginHandle, TauriPlugin};
use tauri::{AppHandle, Manager, Wry};

struct Bridge(PluginHandle<Wry>);

pub fn init() -> TauriPlugin<Wry> {
    Builder::new("submarine-android")
        .setup(|app, api| {
            let handle = api.register_android_plugin("com.submarine.app", "SubmarinePlugin")?;
            app.manage(Bridge(handle));
            Ok(())
        })
        .build()
}

fn bridge(app: &AppHandle) -> Result<&PluginHandle<Wry>, String> {
    app.try_state::<Bridge>()
        .map(|b| &b.inner().0)
        .ok_or_else(|| "The Android bridge isn't ready.".to_string())
}

#[derive(Serialize)]
struct SaveFileArgs<'a> {
    path: &'a str,
    name: &'a str,
}

#[derive(Deserialize)]
struct SaveFileResult {
    saved: bool,
    #[serde(default)]
    name: Option<String>,
}

/// Export `src` (a vault, still encrypted) as `file_name` wherever the user
/// picks. The copy is staged in cache/export/, the only folder the plugin
/// reads from, and removed again whatever happens. Returns the saved
/// document's name, or None if the user cancelled.
pub async fn save_export(app: &AppHandle, src: &Path, file_name: &str) -> Result<Option<String>, String> {
    let dir = app
        .path()
        .app_cache_dir()
        .map_err(|e| format!("No cache folder for the export: {}", e))?
        .join("export");
    // Anything left from an export cut short (the app killed mid-way) goes:
    // one export runs at a time.
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).map_err(|e| format!("[FILE] EXPORT_STAGE_FAILED: {}", e))?;
    let staged = dir.join(file_name);
    let result = async {
        fs::copy(src, &staged).map_err(|e| format!("[FILE] EXPORT_STAGE_FAILED: {}", e))?;
        let path = staged.to_string_lossy();
        bridge(app)?
            .run_mobile_plugin_async::<SaveFileResult>("saveFile", SaveFileArgs { path: &path, name: file_name })
            .await
            .map_err(|e| format!("Couldn't export: {}", e))
    }
    .await;
    let _ = fs::remove_dir_all(&dir);
    let res = result?;
    Ok(res.saved.then(|| res.name.filter(|n| !n.is_empty()).unwrap_or_else(|| file_name.to_string())))
}

#[derive(Serialize)]
struct SecureScreenArgs {
    enabled: bool,
}

#[derive(Deserialize)]
struct SecureScreenResult {
    enabled: bool,
}

/// "Block screenshots" (FLAG_SECURE): read it, or set it when `enabled` is
/// given. Returns the state now in effect.
pub async fn secure_screen(app: &AppHandle, enabled: Option<bool>) -> Result<bool, String> {
    let handle = bridge(app)?;
    let res = match enabled {
        Some(enabled) => handle.run_mobile_plugin_async::<SecureScreenResult>("setSecureScreen", SecureScreenArgs { enabled }).await,
        None => handle.run_mobile_plugin_async::<SecureScreenResult>("getSecureScreen", ()).await,
    };
    res.map(|r| r.enabled).map_err(|e| format!("Couldn't change the screenshot setting: {}", e))
}

#[derive(Serialize)]
struct KeepAliveArgs {
    open: bool,
}

/// Android's keep-alive service (ConnectionKeeperService) runs only while a
/// connection is open: Google Play allows a foreground service for work the
/// user started and still has going, not for an app that is merely open.
/// Checked every 2 s. When the last connection goes, the service stays for
/// `STOP_AFTER` first, so a reconnect (the old connection gone, the new one
/// not in yet) doesn't stop it: Android won't let it start again while the
/// app is in the background.
pub fn watch_connections(app: AppHandle) {
    const EVERY: std::time::Duration = std::time::Duration::from_secs(2);
    const STOP_AFTER: std::time::Duration = std::time::Duration::from_secs(60);
    tauri::async_runtime::spawn(async move {
        // What the plugin was last told, once it took it.
        let mut told: Option<bool> = None;
        let mut none_since: Option<std::time::Instant> = None;
        loop {
            tokio::time::sleep(EVERY).await;
            let want = if crate::any_connection_open(&app).await {
                none_since = None;
                true
            } else {
                let since = *none_since.get_or_insert_with(std::time::Instant::now);
                told == Some(true) && since.elapsed() < STOP_AFTER
            };
            if told == Some(want) {
                continue;
            }
            let Ok(handle) = bridge(&app) else { continue };
            // Not taken (the plugin not ready yet): asked again next round.
            if handle.run_mobile_plugin_async::<serde_json::Value>("setKeepAlive", KeepAliveArgs { open: want }).await.is_ok() {
                told = Some(want);
            }
        }
    });
}

#[derive(Serialize)]
struct ClipboardWriteArgs<'a> {
    text: &'a str,
}

#[derive(Deserialize)]
struct ClipboardText {
    text: String,
}

/// The system clipboard's text. Android's WebView refuses the page's own
/// clipboard API, so the page reads it through here (lib.rs
/// `clipboard_read_text`). Empty when the clipboard holds no text.
pub async fn clipboard_read(app: &AppHandle) -> Result<String, String> {
    bridge(app)?
        .run_mobile_plugin_async::<ClipboardText>("clipboardRead", ())
        .await
        .map(|r| r.text)
        .map_err(|e| format!("Couldn't read the clipboard: {}", e))
}

/// Puts `text` on the system clipboard (lib.rs `clipboard_write_text`).
pub async fn clipboard_write(app: &AppHandle, text: &str) -> Result<(), String> {
    bridge(app)?
        .run_mobile_plugin_async::<serde_json::Value>("clipboardWrite", ClipboardWriteArgs { text })
        .await
        .map(|_| ())
        .map_err(|e| format!("Couldn't copy: {}", e))
}

/// A call for quick unlock (quick_unlock.rs): the Keystore key behind
/// BiometricPrompt. The error is the plugin's own ("CANCELLED", "KEY_GONE",
/// …), which quick_unlock reads.
pub async fn call<T: serde::de::DeserializeOwned>(app: &AppHandle, command: &str, args: serde_json::Value) -> Result<T, String> {
    bridge(app)?.run_mobile_plugin_async::<T>(command, args).await.map_err(|e| e.to_string())
}
