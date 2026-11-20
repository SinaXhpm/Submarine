//! A short log in the temp folder (%TEMP%\submarine-setup.log), for when
//! something fails: the error page names it. Steps, folders and errors only.
use std::fmt::Display;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

use windows::Win32::System::SystemInformation::GetLocalTime;

use crate::product::{PRODUCT_NAME, VERSION};

static FILE: Mutex<Option<File>> = Mutex::new(None);

pub fn path() -> PathBuf {
    std::env::temp_dir().join(format!("{}-setup.log", PRODUCT_NAME.to_lowercase().replace(' ', "-")))
}

fn now() -> String {
    let t = unsafe { GetLocalTime() };
    format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02}", t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond)
}

/// Opens the log; one that has grown past 1 MB starts over.
pub fn start(role: &str) {
    let path = path();
    let too_big = std::fs::metadata(&path).map(|m| m.len() > 1 << 20).unwrap_or(false);
    let file = OpenOptions::new().create(true).append(!too_big).write(true).truncate(too_big).open(&path).ok();
    *FILE.lock().unwrap_or_else(|e| e.into_inner()) = file;
    line(format_args!("---- {role} {PRODUCT_NAME} {VERSION}"));
}

pub fn line(msg: impl Display) {
    if let Some(f) = FILE.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        let _ = writeln!(f, "{} {msg}", now());
    }
}

#[macro_export]
macro_rules! log {
    ($($t:tt)*) => { $crate::log::line(format_args!($($t)*)) };
}
