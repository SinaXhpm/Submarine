//! The texts, in the app's languages (i18n/*.json, checked by build.rs). The
//! language is picked the way the app picks it: the first of Windows'
//! preferred display languages it has, else English.
use serde_json::{Map, Value};
use windows::core::PWSTR;
use windows::Win32::Globalization::{GetUserPreferredUILanguages, MUI_LANGUAGE_NAME};

include!(concat!(env!("OUT_DIR"), "/i18n.rs"));

/// A system language tag as one of ours. Same rules as the app's
/// systemLanguage(): Portuguese gets the Brazilian text, Chinese goes by script.
pub fn match_tag(tag: &str) -> Option<&'static str> {
    let lower = tag.to_ascii_lowercase().replace('_', "-");
    let base = lower.split('-').next().unwrap_or("");
    if base == "zh" {
        let traditional = lower.contains("hant") || lower.split('-').any(|p| matches!(p, "tw" | "hk" | "mo"));
        return Some(if traditional { "zh-TW" } else { "zh-CN" });
    }
    Some(match base {
        "en" => "en",
        "de" => "de",
        "es" => "es",
        "fr" => "fr",
        "it" => "it",
        "pl" => "pl",
        "pt" => "pt-BR",
        "tr" => "tr",
        "ru" => "ru",
        "uk" => "uk",
        "hi" => "hi",
        "ja" => "ja",
        "ko" => "ko",
        _ => return None,
    })
}

fn preferred_tags() -> Vec<String> {
    let (mut count, mut len) = (0u32, 0u32);
    unsafe {
        if GetUserPreferredUILanguages(MUI_LANGUAGE_NAME, &mut count, None, &mut len).is_err() || len == 0 {
            return Vec::new();
        }
        let mut buf = vec![0u16; len as usize];
        if GetUserPreferredUILanguages(MUI_LANGUAGE_NAME, &mut count, Some(PWSTR(buf.as_mut_ptr())), &mut len).is_err() {
            return Vec::new();
        }
        buf.split(|c| *c == 0).filter(|s| !s.is_empty()).map(String::from_utf16_lossy).collect()
    }
}

pub fn pick() -> &'static str {
    #[cfg(feature = "lab")]
    if let Ok(lang) = std::env::var("SUBMARINE_SETUP_LANG") {
        if let Some((id, _)) = LANGS.iter().find(|(id, _)| *id == lang) {
            return id;
        }
    }
    preferred_tags().iter().find_map(|t| match_tag(t)).unwrap_or("en")
}

pub struct Strings {
    pub lang: &'static str,
    texts: Map<String, Value>,
}

fn load_map(lang: &str) -> Map<String, Value> {
    LANGS
        .iter()
        .find(|(id, _)| *id == lang)
        .and_then(|(_, json)| serde_json::from_str::<Value>(json).ok())
        .and_then(|v| match v {
            Value::Object(m) => Some(m),
            _ => None,
        })
        .unwrap_or_default()
}

impl Strings {
    pub fn load(lang: &'static str) -> Strings {
        // English under the chosen language: a key a language lacks still reads.
        let mut texts = load_map("en");
        texts.extend(load_map(lang));
        Strings { lang, texts }
    }

    pub fn t(&self, key: &str) -> String {
        self.texts.get(key).and_then(Value::as_str).unwrap_or(key).to_string()
    }

    /// A text with its {placeholders} filled in.
    pub fn f(&self, key: &str, vars: &[(&str, &str)]) -> String {
        let mut s = self.t(key);
        for (name, value) in vars {
            s = s.replace(&format!("{{{name}}}"), value);
        }
        s
    }

    /// All texts, for the page.
    pub fn all(&self) -> Value {
        Value::Object(self.texts.clone())
    }
}

/// "54 MB": what the page shows, for messages built here.
pub fn megabytes(bytes: u64) -> String {
    format!("{} MB", (bytes + (1 << 20) - 1) >> 20)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags() {
        assert_eq!(match_tag("de-DE"), Some("de"));
        assert_eq!(match_tag("pt-PT"), Some("pt-BR"));
        assert_eq!(match_tag("zh-Hant-HK"), Some("zh-TW"));
        assert_eq!(match_tag("zh-HK"), Some("zh-TW"));
        assert_eq!(match_tag("zh-SG"), Some("zh-CN"));
        assert_eq!(match_tag("zh-Hans-CN"), Some("zh-CN"));
        assert_eq!(match_tag("en-GB"), Some("en"));
        assert_eq!(match_tag("nl-NL"), None);
    }

    #[test]
    fn every_language_loads() {
        for (lang, _) in LANGS {
            let s = Strings::load(lang);
            assert_ne!(s.t("install"), "install", "{lang}");
        }
        let de = Strings::load("de");
        assert_eq!(de.f("status.new", &[("version", "1.2.3")]), "Version 1.2.3");
    }
}
