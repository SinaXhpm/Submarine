//! The app's own Android plugin (gen/android/.../SubmarinePlugin.kt): profile
//! export through the system's "Save to" picker, and the "Block screenshots"
//! switch. Only Rust reaches it. The plugin has no permissions in the
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
