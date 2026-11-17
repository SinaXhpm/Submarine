#![windows_subsystem = "windows"]
//! Submarine's uninstaller: setup puts it in the install folder.

fn main() {
    std::process::exit(setup_core::uninstall::main());
}
