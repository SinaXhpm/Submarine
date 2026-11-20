#![windows_subsystem = "windows"]
//! Submarine's setup: installs the app packed into this exe (see build.rs).

include!(concat!(env!("OUT_DIR"), "/payload.rs"));

fn main() {
    std::process::exit(setup_core::install::main(PAYLOAD));
}
