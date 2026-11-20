//! What is installed, from src-tauri/tauri.conf.json (see build.rs):
//! PRODUCT_NAME, VERSION, IDENTIFIER, MANUFACTURER, HOMEPAGE, COPYRIGHT, MAIN_EXE.
include!(concat!(env!("OUT_DIR"), "/product.rs"));

pub const UNINSTALL_EXE: &str = "uninstall.exe";

/// The Apps entry, per user: the key the NSIS installer used.
pub fn uninstall_key() -> String {
    format!(r"Software\Microsoft\Windows\CurrentVersion\Uninstall\{PRODUCT_NAME}")
}

/// Where the install folder is remembered (Tauri's NSIS installer kept it here).
pub fn folder_key() -> String {
    format!(r"Software\{MANUFACTURER}\{PRODUCT_NAME}")
}

pub fn folder_key_parent() -> String {
    format!(r"Software\{MANUFACTURER}")
}

/// "Submarine.lnk", in the Start menu and on the desktop.
pub fn shortcut_name() -> String {
    format!("{PRODUCT_NAME}.lnk")
}

/// The files setup puts in the install folder and the uninstaller removes.
pub fn installed_files() -> [&'static str; 2] {
    [MAIN_EXE, UNINSTALL_EXE]
}
