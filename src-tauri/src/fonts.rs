//! Fonts installed on this device, for the terminal font picker.
//!
//! A webview can't enumerate system fonts: the Local Font Access API pops a
//! permission prompt and doesn't exist on WebKitGTK or Android. So the OS font
//! folders are read here with `fontdb`, which only parses each file's name
//! table: Windows (system + per-user font folders), macOS and Linux
//! (fontconfig). Android returns an empty list on purpose: its WebView doesn't
//! reliably resolve system fonts by their file family names, so they'd show up
//! as choices that silently fall back. The bundled fonts cover Android.

use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SystemFont {
    /// Family name as CSS `font-family` resolves it.
    pub family: String,
    /// At least one face of the family is fixed-pitch — the ones whose columns
    /// line up in a terminal. Any other family still works, just unevenly.
    pub monospace: bool,
}

/// Fold per-face `(family, monospaced)` pairs into one entry per family.
/// Names are de-duplicated case-insensitively (first spelling wins), and names
/// CSS can't use are dropped: empty ones, and macOS's hidden system faces
/// (`.SF NS`, …). The result is sorted case-insensitively.
#[cfg_attr(target_os = "android", allow(dead_code))]
pub(crate) fn summarize<I: IntoIterator<Item = (String, bool)>>(faces: I) -> Vec<SystemFont> {
    let mut by_key: BTreeMap<String, SystemFont> = BTreeMap::new();
    for (family, monospace) in faces {
        let family = family.trim();
        if family.is_empty() || family.starts_with('.') {
            continue;
        }
        by_key
            .entry(family.to_lowercase())
            .and_modify(|f| f.monospace |= monospace)
            .or_insert_with(|| SystemFont { family: family.to_string(), monospace });
    }
    by_key.into_values().collect()
}

/// Every font family installed on this device (empty on Android). Parsing the
/// font folder takes a moment on a machine with many fonts, so it runs off the
/// async runtime.
#[tauri::command]
pub async fn list_system_fonts() -> Result<Vec<SystemFont>, String> {
    #[cfg(target_os = "android")]
    {
        Ok(Vec::new())
    }
    #[cfg(not(target_os = "android"))]
    {
        tauri::async_runtime::spawn_blocking(|| {
            let mut db = fontdb::Database::new();
            db.load_system_fonts();
            // fontdb lists a face's English (US) family name first when the
            // font has one — the name CSS matches on.
            summarize(
                db.faces()
                    .filter_map(|face| face.families.first().map(|(name, _)| (name.clone(), face.monospaced))),
            )
        })
        .await
        .map_err(|e| format!("[FONTS] LIST_FAILED: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(family: &str, monospace: bool) -> SystemFont {
        SystemFont { family: family.into(), monospace }
    }

    #[test]
    fn one_entry_per_family_monospace_if_any_face_is() {
        let got = summarize([
            ("Consolas".to_string(), true),
            ("Consolas".to_string(), true),
            ("Mixed".to_string(), false),
            ("Mixed".to_string(), true),
        ]);
        assert_eq!(got, vec![f("Consolas", true), f("Mixed", true)]);
    }

    #[test]
    fn dedupes_case_insensitively_and_sorts() {
        let got = summarize([
            ("zeta".to_string(), false),
            ("Arial".to_string(), false),
            ("ARIAL".to_string(), false),
            ("beta".to_string(), true),
        ]);
        assert_eq!(got, vec![f("Arial", false), f("beta", true), f("zeta", false)]);
    }

    #[test]
    fn drops_names_css_cannot_use() {
        let got = summarize([
            (String::new(), true),
            ("   ".to_string(), true),
            (".SF NS".to_string(), false),
            ("  Fira Code ".to_string(), true),
        ]);
        assert_eq!(got, vec![f("Fira Code", true)]);
    }
}
