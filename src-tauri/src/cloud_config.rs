//! Self-hosting: a user-editable override for the cloud API base URL.
//!
//! `cloud::CLOUD_API_BASE` stays the default; this module layers a per-device
//! override on top of it, persisted in `<app_data>/cloud_server.json`. It lives
//! in its own file (with a one-line hook in `cloud::url`) so the fork's diff
//! against upstream stays tiny and rebases cleanly.
//!
//! A bearer token is only meaningful to the server that issued it, so changing
//! the server signs this device out first (best-effort revoke on the OLD server,
//! then the local token is dropped).

use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};
use tauri::Manager as _;

use crate::cloud::{self, CloudState, CLOUD_API_BASE};

const CONFIG_FILENAME: &str = "cloud_server.json";

/// `None` = use `CLOUD_API_BASE`. Read on every request via `base()`, so a
/// change applies immediately without rebuilding the reqwest client.
static OVERRIDE: RwLock<Option<String>> = RwLock::new(None);

#[derive(Serialize, Deserialize)]
struct StoredConfig {
    url: String,
}

#[derive(Serialize)]
pub struct ServerInfo {
    /// The base URL currently in effect.
    pub url: String,
    /// The built-in default, so the UI can offer "reset".
    pub default_url: String,
    pub is_custom: bool,
}

/// The effective API base URL (no trailing slash).
pub fn base() -> String {
    OVERRIDE
        .read()
        .ok()
        .and_then(|g| g.clone())
        .unwrap_or_else(|| CLOUD_API_BASE.to_string())
}

fn info() -> ServerInfo {
    let url = base();
    ServerInfo {
        is_custom: url != CLOUD_API_BASE,
        url,
        default_url: CLOUD_API_BASE.to_string(),
    }
}

fn config_path(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    app.path()
        .app_data_dir()
        .map(|d| d.join(CONFIG_FILENAME))
        .map_err(|e| format!("[CLOUD] APP_DATA_DIR_NOT_FOUND: {}", e))
}

/// Normalise + validate a user-entered base URL. Returns `None` for "use the
/// default" (blank input, or the default itself).
///
/// HTTPS is required except for loopback: the login password and bearer token
/// travel in these requests, so plain HTTP is only tolerated for a server on
/// this same machine (local development).
fn normalize(input: &str) -> Result<Option<String>, String> {
    let trimmed = input.trim().trim_end_matches('/');
    if trimmed.is_empty() || trimmed == CLOUD_API_BASE {
        return Ok(None);
    }
    let parsed = reqwest::Url::parse(trimmed)
        .map_err(|e| format!("[CLOUD] INVALID_SERVER_URL: {}", e))?;
    let host = parsed
        .host_str()
        .ok_or("[CLOUD] INVALID_SERVER_URL: missing host")?;
    let loopback = host == "localhost"
        || host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<std::net::IpAddr>()
            .map(|ip| ip.is_loopback())
            .unwrap_or(false);
    match parsed.scheme() {
        "https" => {}
        "http" if loopback => {}
        "http" => {
            return Err("[CLOUD] INSECURE_SERVER_URL: use https:// (plain http is only allowed for localhost)".into())
        }
        s => return Err(format!("[CLOUD] INVALID_SERVER_URL: unsupported scheme '{}'", s)),
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return Err("[CLOUD] INVALID_SERVER_URL: remove the ?query / #fragment".into());
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err("[CLOUD] INVALID_SERVER_URL: credentials in the URL are not supported".into());
    }
    // Rebuild from the parsed form (lower-cased host, etc.) so equal servers
    // compare equal; `Url` always renders a path, so strip its trailing '/'.
    Ok(Some(parsed.as_str().trim_end_matches('/').to_string()))
}

/// Load the persisted override. Called once from `setup`, before any request.
/// A missing or invalid file falls back to the default rather than failing
/// startup.
pub fn init(app: &tauri::AppHandle) {
    let stored = config_path(app)
        .ok()
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|b| serde_json::from_slice::<StoredConfig>(&b).ok())
        .and_then(|c| normalize(&c.url).ok().flatten());
    if let Ok(mut g) = OVERRIDE.write() {
        *g = stored;
    }
}

#[tauri::command]
pub async fn cloud_get_server() -> Result<ServerInfo, String> {
    Ok(info())
}

/// Set (or, with a blank / default value, clear) the server override. If the
/// effective server changes, the current session is signed out first.
#[tauri::command]
pub async fn cloud_set_server(
    app: tauri::AppHandle,
    state: tauri::State<'_, Arc<CloudState>>,
    url: String,
) -> Result<ServerInfo, String> {
    let next = normalize(&url)?;
    let next_effective = next.clone().unwrap_or_else(|| CLOUD_API_BASE.to_string());
    if next_effective == base() {
        return Ok(info());
    }

    // Sign out against the CURRENT server before switching, so the revoke call
    // reaches the server that issued the token.
    cloud::cloud_logout(app.clone(), state).await?;

    let path = config_path(&app)?;
    match &next {
        Some(u) => {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("[CLOUD] CONFIG_DIR_CREATE: {}", e))?;
            }
            let bytes = serde_json::to_vec(&StoredConfig { url: u.clone() })
                .map_err(|e| format!("[CLOUD] CONFIG_SERIALIZE: {}", e))?;
            std::fs::write(&path, bytes).map_err(|e| format!("[CLOUD] CONFIG_WRITE: {}", e))?;
        }
        None => {
            let _ = std::fs::remove_file(&path);
        }
    }
    if let Ok(mut g) = OVERRIDE.write() {
        *g = next;
    }
    Ok(info())
}

#[cfg(test)]
mod tests {
    use super::normalize;
    use crate::cloud::CLOUD_API_BASE;

    #[test]
    fn blank_and_default_mean_no_override() {
        assert_eq!(normalize("").unwrap(), None);
        assert_eq!(normalize("   ").unwrap(), None);
        assert_eq!(normalize(CLOUD_API_BASE).unwrap(), None);
        assert_eq!(normalize(&format!("{}/", CLOUD_API_BASE)).unwrap(), None);
    }

    #[test]
    fn accepts_https_and_normalises() {
        assert_eq!(
            normalize(" https://Sync.Example.com/ ").unwrap().as_deref(),
            Some("https://sync.example.com")
        );
        assert_eq!(
            normalize("https://example.com:8443/submarine/").unwrap().as_deref(),
            Some("https://example.com:8443/submarine")
        );
    }

    #[test]
    fn http_only_for_loopback() {
        assert!(normalize("http://localhost:8080").unwrap().is_some());
        assert!(normalize("http://127.0.0.1:8080").unwrap().is_some());
        assert!(normalize("http://[::1]:8080").unwrap().is_some());
        assert!(normalize("http://192.168.1.10").is_err());
        assert!(normalize("http://example.com").is_err());
    }

    #[test]
    fn rejects_junk() {
        assert!(normalize("example.com").is_err());
        assert!(normalize("ftp://example.com").is_err());
        assert!(normalize("https://example.com/?x=1").is_err());
        assert!(normalize("https://user:pw@example.com").is_err());
    }
}
