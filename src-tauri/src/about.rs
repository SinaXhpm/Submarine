//! App-info + update-check + safe URL opener.
//!
//! Tiny module: just enough to render the About panel and let the user
//! see whether a newer release exists on GitHub. The actual download
//! is intentionally NOT automated — we surface the release URL and let
//! the user pick whether to grab it (auto-update infrastructure is a
//! separate, bigger problem involving signed updates).

use serde::Serialize;

/// `<owner>/<repo>` on GitHub. Used to build the API URL for the
/// "latest release" query and the user-facing repo URL.
pub const GITHUB_REPO: &str = "sinaxhpm/submarine";

/// Marketing site / project page. Surfaced in the About panel.
pub const WEBSITE_URL: &str = "https://sinaxhpm.com";

#[derive(Debug, Clone, Serialize)]
pub struct AppInfo {
    pub version: String,
    pub github_repo_url: String,
    pub github_releases_url: String,
    pub website_url: String,
}

/// Static info bundled with the binary. Version comes from CARGO_PKG_VERSION
/// which Cargo + tauri-action set from the git tag during release builds.
#[tauri::command]
pub fn app_info() -> AppInfo {
    AppInfo {
        version: env!("CARGO_PKG_VERSION").to_string(),
        github_repo_url: format!("https://github.com/{}", GITHUB_REPO),
        github_releases_url: format!("https://github.com/{}/releases", GITHUB_REPO),
        website_url: WEBSITE_URL.to_string(),
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct UpdateInfo {
    /// Current bundled version (semver, no "v" prefix).
    pub current: String,
    /// Latest release tag on GitHub, with "v" stripped to match `current`.
    /// `None` if the API call failed or no release exists.
    pub latest: Option<String>,
    /// True when `latest > current` under naive lexicographic compare on
    /// dot-split numeric components. Frontend uses this to colour the
    /// banner (green = up-to-date, amber = update available).
    pub has_update: bool,
    /// Direct link to the latest release page. Frontend shows an "Open
    /// release notes" button when has_update is true.
    pub release_url: Option<String>,
}

/// Subset of GitHub's `/releases/latest` JSON we actually care about.
/// Marked non_exhaustive-friendly via #[serde(default)] so a future
/// addition / rename on GitHub's side doesn't break us.
#[derive(Debug, serde::Deserialize)]
struct GhRelease {
    #[serde(default)]
    tag_name: String,
    #[serde(default)]
    html_url: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
}

/// Hit GitHub's "latest release" endpoint. Anonymous calls are rate-
/// limited to ~60/hour per IP — fine for an occasional click. The
/// timeout is short so a hung connection can't freeze the About modal.
#[tauri::command]
pub async fn check_for_updates() -> Result<UpdateInfo, String> {
    let current = env!("CARGO_PKG_VERSION").to_string();
    let url = format!("https://api.github.com/repos/{}/releases/latest", GITHUB_REPO);

    // Build a fresh client per call rather than holding state — this is
    // a one-shot probe, not a hot path. User-Agent is required by the
    // GitHub API; an empty/missing UA gets a 403.
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(6))
        .user_agent(concat!("submarine-app/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| format!("[UPDATE] CLIENT: {}", e))?;

    let resp = client
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| format!("[UPDATE] NETWORK: {}", e))?;

    if !resp.status().is_success() {
        // 404 means no releases yet — return current with latest=None
        // so the UI can show "no releases on GitHub yet" rather than
        // a scary error.
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(UpdateInfo { current, latest: None, has_update: false, release_url: None });
        }
        return Err(format!("[UPDATE] HTTP {}", resp.status()));
    }

    let release: GhRelease = resp
        .json()
        .await
        .map_err(|e| format!("[UPDATE] BAD_JSON: {}", e))?;

    // `latest` only counts published, non-prerelease tags. Drafts shouldn't
    // appear via /releases/latest anyway but we double-check defensively.
    if release.draft || release.prerelease || release.tag_name.is_empty() {
        return Ok(UpdateInfo { current, latest: None, has_update: false, release_url: None });
    }

    let latest_clean = release.tag_name.strip_prefix('v').unwrap_or(&release.tag_name).to_string();
    let has_update = semver_greater(&latest_clean, &current);

    Ok(UpdateInfo {
        current,
        latest: Some(latest_clean),
        has_update,
        release_url: if release.html_url.is_empty() { None } else { Some(release.html_url) },
    })
}

/// True when `a > b` under dotted-number compare (ignores any non-numeric
/// suffix like "-beta"). Robust enough for our semver-tagged releases:
/// "0.2.0" > "0.1.5", "1.0.0" > "0.9.9". A pre-release suffix on `a`
/// makes it sort EQUAL to the same base — preferring stable over pre.
fn semver_greater(a: &str, b: &str) -> bool {
    let strip = |s: &str| s.split(['-', '+']).next().unwrap_or(s).to_string();
    let parse = |s: String| -> Vec<u64> {
        s.split('.').map(|p| p.parse::<u64>().unwrap_or(0)).collect()
    };
    let aa = parse(strip(a));
    let bb = parse(strip(b));
    for i in 0..aa.len().max(bb.len()) {
        let av = aa.get(i).copied().unwrap_or(0);
        let bv = bb.get(i).copied().unwrap_or(0);
        if av != bv {
            return av > bv;
        }
    }
    false
}

/// Open a URL in the user's default browser. URL must be http(s) — we
/// refuse file://, javascript:, and anything else to avoid being a
/// privileged drop-tool for the webview. tauri-plugin-opener dispatches
/// to ACTION_VIEW on Android and to xdg-open / open / start on desktop,
/// so the same call works on every platform we ship (the `open` crate
/// has no Android backend, which is why we used to return "not wired on
/// Android yet" — now fixed).
#[tauri::command]
pub fn open_external_url(app: tauri::AppHandle, url: String) -> Result<(), String> {
    use tauri_plugin_opener::OpenerExt;
    let lowered = url.to_ascii_lowercase();
    if !(lowered.starts_with("https://") || lowered.starts_with("http://")) {
        return Err("[OPEN] only http(s) urls are allowed".into());
    }
    app.opener()
        .open_url(&url, None::<&str>)
        .map_err(|e| format!("[OPEN] {}", e))
}
