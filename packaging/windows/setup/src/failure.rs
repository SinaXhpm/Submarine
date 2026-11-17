//! Why setup or the uninstaller stopped, as the user reads it and as the log
//! keeps it.
use std::path::PathBuf;

use serde_json::{json, Value};

use crate::i18n::{megabytes, Strings};

pub enum Failure {
    /// The user said no (to closing the app).
    Cancelled,
    /// The folder can't be used: not writable, Windows' own, a drive's root.
    Folder,
    /// Bytes needed.
    Space(u64),
    /// The packed app doesn't unpack to what was packed.
    Damaged,
    /// The app is still running.
    Close,
    /// The .msi copy couldn't be removed.
    Msi(String),
    Write(PathBuf, String),
    Registry(String),
    /// Things the uninstaller couldn't remove.
    Remove(Vec<String>),
    Other(String),
}

impl Failure {
    pub fn message(&self, s: &Strings) -> String {
        match self {
            Failure::Cancelled => String::new(),
            Failure::Folder => s.t("error.folder"),
            Failure::Space(n) => s.f("error.space", &[("size", &megabytes(*n))]),
            Failure::Damaged => s.t("error.damaged"),
            Failure::Close => s.t("error.close"),
            Failure::Msi(_) => s.t("error.msi"),
            Failure::Write(p, e) => s.f("error.write", &[("file", &p.display().to_string()), ("error", e)]),
            Failure::Registry(e) => s.f("error.registry", &[("error", e)]),
            Failure::Remove(items) => items.join("\n"),
            Failure::Other(e) => s.f("error.unexpected", &[("error", e)]),
        }
    }

    /// For the log, in English.
    pub fn detail(&self) -> String {
        match self {
            Failure::Cancelled => "cancelled".into(),
            Failure::Folder => "folder not usable".into(),
            Failure::Space(n) => format!("not enough space, {n} bytes needed"),
            Failure::Damaged => "payload damaged".into(),
            Failure::Close => "the app didn't close".into(),
            Failure::Msi(e) => format!("msi copy not removed: {e}"),
            Failure::Write(p, e) => format!("write {}: {e}", p.display()),
            Failure::Registry(e) => format!("registry: {e}"),
            Failure::Remove(items) => format!("not removed: {}", items.join("; ")),
            Failure::Other(e) => e.clone(),
        }
    }

    /// The event for the page's error screen. A size is left for the page to
    /// write in the user's language ("85 МБ", "85 Mo").
    pub fn page_event(&self, s: &Strings, log: &str) -> Value {
        let (message, size) = match self {
            Failure::Space(n) => (s.t("error.space"), Some(*n)),
            _ => (self.message(s), None),
        };
        json!({
            "type": "error",
            "message": message,
            "size": size,
            "retry": !matches!(self, Failure::Folder | Failure::Damaged | Failure::Cancelled),
            "options": matches!(self, Failure::Folder | Failure::Space(_)),
            "msi": matches!(self, Failure::Msi(_)),
            "log": log,
        })
    }
}
