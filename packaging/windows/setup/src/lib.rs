//! Submarine's Windows installer (setup.exe) and uninstaller (uninstall.exe).
//!
//! It installs for the current user, with no administrator rights, into the
//! same folder and under the same registry keys and shortcuts as the NSIS
//! installer it replaces: a copy installed by that one updates in place and
//! uninstalls cleanly. The window is a web page (ui/) in WebView2, which the
//! app needs anyway; without a window (/S) nothing of it is used.
#![cfg(windows)]

pub mod args;
pub mod dialog;
pub mod failure;
pub mod fsx;
pub mod i18n;
pub mod install;
pub mod log;
pub mod native;
pub mod product;
pub mod registry;
pub mod running;
pub mod shell;
pub mod ui;
pub mod uninstall;
pub mod version;
pub mod webview2;
