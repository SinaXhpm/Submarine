//! App-info + update-check + safe URL opener.
//!
//! Tiny module: the version and GitHub link on the profile screen, and a
//! check for whether a newer release exists on GitHub. The actual download
//! is intentionally NOT automated — we surface the release URL and let
//! the user pick whether to grab it (auto-update infrastructure is a
//! separate, bigger problem involving signed updates).

use serde::Serialize;

/// `<owner>/<repo>` on GitHub (canonical casing). Used to build the API URL
/// for the releases query and the user-facing repo URL.
pub const GITHUB_REPO: &str = "SinaXhpm/Submarine";

#[derive(Debug, Clone, Serialize)]
pub struct AppInfo {
    pub version: String,
    /// Whether this copy looks for new releases on GitHub. Not on iOS, not in
    /// the Mac App Store build and not as a Flatpak: the store it came from
    /// updates those (and PRIVACY.md says the iOS app doesn't ask GitHub).
    pub updates: bool,
    pub github_repo_url: String,
    pub github_releases_url: String,
}

/// Running as a Flatpak. Flathub updates it, so a newer release on GitHub is
/// nothing for its user to act on.
pub fn in_flatpak() -> bool {
    std::path::Path::new("/.flatpak-info").exists()
}

/// The version this build was released as. The release workflow stamps the
/// git tag into Cargo.toml, but a Windows build gets numbers only (0.5.0 for
/// v0.5.0-beta.6: the .msi needs that), so the workflow also sets the full tag
/// as SUBMARINE_RELEASE_VERSION while building. Without it a Windows beta
/// couldn't tell beta.6 from beta.7, or from the release.
pub fn running_version() -> &'static str {
    match option_env!("SUBMARINE_RELEASE_VERSION") {
        Some(v) if !v.trim().is_empty() => v.trim(),
        _ => env!("CARGO_PKG_VERSION"),
    }
}

/// Static info bundled with the binary: the version from running_version.
#[tauri::command]
pub fn app_info() -> AppInfo {
    AppInfo {
        version: running_version().to_string(),
        updates: updates(),
        github_repo_url: format!("https://github.com/{}", GITHUB_REPO),
        github_releases_url: format!("https://github.com/{}/releases", GITHUB_REPO),
    }
}

/// See `AppInfo::updates`: off in the App Store's and Google Play's builds
/// (both with the `app-store` feature), and in a Flatpak.
const UPDATES: bool = !cfg!(any(target_os = "ios", feature = "app-store"));
fn updates() -> bool {
    UPDATES && !in_flatpak()
}

#[derive(Debug, Clone, Serialize)]
pub struct UpdateInfo {
    /// Current bundled version (semver, no "v" prefix).
    pub current: String,
    /// Latest release tag on GitHub, with "v" stripped to match `current`.
    /// `None` if the API call failed or no release exists.
    pub latest: Option<String>,
    /// True when the highest published, non-prerelease release on GitHub is a
    /// strictly newer semver than the running build (per-component NUMERIC
    /// compare — see `semver_greater`). Frontend uses this to colour the
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

/// Ask GitHub which release is newest and whether it beats the running build.
///
/// We pull the RELEASES LIST rather than `/releases/latest`: GitHub's "latest"
/// flag is date-based (and manually overridable), so a hotfix published on an
/// old line could point it at a lower version than what's actually shipped.
/// Instead we scan every published, non-prerelease release and pick the highest
/// SEMVER ourselves, then compare that to `current`. With `include_beta` (the
/// "Tell me about beta versions" setting) pre-releases count too. Anonymous calls are rate-
/// limited to ~60/hour per IP — fine for an occasional check. The timeout is
/// short so a hung connection can't leave the check spinning.
#[tauri::command]
pub async fn check_for_updates(include_beta: Option<bool>) -> Result<UpdateInfo, String> {
    let current = running_version().to_string();
    let include_beta = include_beta.unwrap_or(false);
    // The App Store's builds and a Flatpak: nothing is asked, nothing is offered.
    if !updates() {
        return Ok(UpdateInfo { current, latest: None, has_update: false, release_url: None });
    }
    let url = format!("https://api.github.com/repos/{}/releases?per_page=100", GITHUB_REPO);

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
        // 404 means the repo path is wrong; anything else is a transient
        // GitHub / rate-limit error. Either way, don't nag — report "no
        // newer release known" so the UI stays quiet rather than scary.
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(UpdateInfo { current, latest: None, has_update: false, release_url: None });
        }
        return Err(format!("[UPDATE] HTTP {}", resp.status()));
    }

    // An empty repo returns `[]` (200), not 404 — handled naturally below.
    // The list of releases is a few hundred KB at most; read no more than 8 MB.
    let bytes = crate::cloud::read_capped(resp, 8 * 1024 * 1024).await.map_err(|e| format!("[UPDATE] {}", e))?;
    let releases: Vec<GhRelease> = serde_json::from_slice(&bytes).map_err(|e| format!("[UPDATE] BAD_JSON: {}", e))?;

    // Highest published semver wins, pre-releases only with include_beta.
    // Drafts are only visible with auth (we're anonymous) but we skip them
    // defensively all the same.
    let mut best: Option<(String, String)> = None; // (clean_version, html_url)
    for r in releases {
        if r.draft || (r.prerelease && !include_beta) || r.tag_name.is_empty() {
            continue;
        }
        let clean = r.tag_name.strip_prefix('v').unwrap_or(&r.tag_name).to_string();
        let wins = match &best {
            None => true,
            Some((cur_best, _)) => semver_greater(&clean, cur_best),
        };
        if wins {
            best = Some((clean, r.html_url));
        }
    }

    match best {
        None => Ok(UpdateInfo { current, latest: None, has_update: false, release_url: None }),
        Some((latest_clean, html_url)) => {
            let has_update = semver_greater(&latest_clean, &current);
            Ok(UpdateInfo {
                current,
                latest: Some(latest_clean),
                has_update,
                release_url: if html_url.is_empty() { None } else { Some(html_url) },
            })
        }
    }
}

/// True when version `a` is newer than `b`, by semver's order: the numbers
/// part by part, then a release above its own pre-releases (0.5.0 above
/// 0.5.0-beta.6), and pre-release parts as numbers when both are (beta.10
/// above beta.9). Build metadata (+...) doesn't count. Comparing the numbers
/// only, as before, made 0.5.0 and 0.5.0-beta.6 equal: someone on a beta was
/// never told about its release.
fn semver_greater(a: &str, b: &str) -> bool {
    semver_cmp(a, b) == std::cmp::Ordering::Greater
}

fn semver_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering::{Equal, Greater, Less};
    let split = |s: &str| -> (Vec<u64>, Option<Vec<String>>) {
        let s = s.trim();
        let s = s.split('+').next().unwrap_or(s);
        let (core, pre) = match s.split_once('-') {
            Some((core, pre)) => (core, Some(pre.split('.').map(str::to_string).collect())),
            None => (s, None),
        };
        (core.split('.').map(|p| p.parse::<u64>().unwrap_or(0)).collect(), pre)
    };
    let (an, ap) = split(a);
    let (bn, bp) = split(b);
    for i in 0..an.len().max(bn.len()) {
        let o = an.get(i).copied().unwrap_or(0).cmp(&bn.get(i).copied().unwrap_or(0));
        if o != Equal {
            return o;
        }
    }
    match (ap, bp) {
        (None, None) => Equal,
        (None, Some(_)) => Greater,
        (Some(_), None) => Less,
        (Some(ap), Some(bp)) => {
            for i in 0..ap.len().max(bp.len()) {
                let o = match (ap.get(i), bp.get(i)) {
                    (None, None) => Equal,
                    (None, Some(_)) => Less,
                    (Some(_), None) => Greater,
                    (Some(x), Some(y)) => match (x.parse::<u64>(), y.parse::<u64>()) {
                        (Ok(x), Ok(y)) => x.cmp(&y),
                        // A number sorts below a word (beta.1 < beta.rc).
                        (Ok(_), Err(_)) => Less,
                        (Err(_), Ok(_)) => Greater,
                        (Err(_), Err(_)) => x.cmp(y),
                    },
                };
                if o != Equal {
                    return o;
                }
            }
            Equal
        }
    }
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
    let url = checked_url(&url)?;
    // On Linux the browser starts without this app's own WebKit workarounds
    // (child_env.rs), which the plugin would hand down to it.
    #[cfg(target_os = "linux")]
    {
        let _ = app;
        crate::child_env::open_detached(&url).map_err(|e| format!("[OPEN] {}", e))
    }
    #[cfg(not(target_os = "linux"))]
    {
        use tauri_plugin_opener::OpenerExt;
        app.opener()
            .open_url(&url, None::<&str>)
            .map_err(|e| format!("[OPEN] {}", e))
    }
}

/// The address the browser gets: parsed, not only checked at its start, so
/// a space, a quote or a control character can't reach a handler that might
/// read it differently. An http(s) scheme and a host are required.
fn checked_url(url: &str) -> Result<String, String> {
    if url.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err("[OPEN] that address has spaces or control characters".into());
    }
    let parsed = tauri::Url::parse(url).map_err(|_| "[OPEN] that isn't a web address".to_string())?;
    if !matches!(parsed.scheme(), "https" | "http") {
        return Err("[OPEN] only http(s) urls are allowed".into());
    }
    if parsed.host_str().is_none_or(str::is_empty) {
        return Err("[OPEN] that address has no host".into());
    }
    Ok(parsed.as_str().to_string())
}

#[cfg(test)]
mod tests {
    use super::{checked_url, semver_greater};

    #[test]
    fn versions_compare_as_semver_says() {
        // Numbers part by part.
        assert!(semver_greater("0.5.1", "0.5.0"));
        assert!(semver_greater("0.10.0", "0.9.9"));
        assert!(semver_greater("1.0.0", "0.99.99"));
        assert!(!semver_greater("0.5.0", "0.5.0"));
        assert!(!semver_greater("0.3.8", "0.5.0"));
        // A release is newer than its own betas, and a beta of the next one is
        // newer than the release before it.
        assert!(semver_greater("0.5.0", "0.5.0-beta.6"));
        assert!(!semver_greater("0.5.0-beta.6", "0.5.0"));
        assert!(semver_greater("0.5.1-beta.1", "0.5.0"));
        assert!(!semver_greater("0.5.0-beta.6", "0.5.1"));
        // Beta numbers as numbers.
        assert!(semver_greater("0.5.0-beta.10", "0.5.0-beta.9"));
        assert!(semver_greater("0.5.0-beta.7", "0.5.0-beta.6"));
        assert!(!semver_greater("0.5.0-beta.6", "0.5.0-beta.6"));
        // Words in order, a number below a word, a longer list above its start.
        assert!(semver_greater("0.5.0-rc.1", "0.5.0-beta.9"));
        assert!(semver_greater("0.5.0-beta.rc", "0.5.0-beta.1"));
        assert!(semver_greater("0.5.0-beta.6.1", "0.5.0-beta.6"));
        // Build metadata doesn't count.
        assert!(!semver_greater("0.5.0+build.7", "0.5.0"));
        assert!(semver_greater("0.5.0+x", "0.5.0-beta.1"));
    }

    #[test]
    fn only_a_parsed_web_address_opens() {
        assert_eq!(checked_url("https://example.com/a?b=1").unwrap(), "https://example.com/a?b=1");
        assert_eq!(checked_url("HTTP://Example.COM").unwrap(), "http://example.com/");
        for bad in [
            "file:///etc/passwd",
            "javascript:alert(1)",
            "ms-settings:",
            "https://exa mple.com",
            "https://example.com/\u{7}",
            "https://example.com/\n--flag",
            "https://",
            "",
        ] {
            assert!(checked_url(bad).is_err(), "{bad:?}");
        }
    }
}
