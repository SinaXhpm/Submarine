// Desktop binary entry-point. All the real wiring (Tauri Builder, command
// registry, state, plugin setup) lives in `lib.rs::run()` — which is also
// what Tauri's Android entry-point macro invokes. Keeping main.rs to a
// thin shim is what lets the SAME Rust code ship as a desktop bin AND an
// Android cdylib without duplicating the Builder section.
//
// The `windows_subsystem = "windows"` attribute below stays here (it only
// applies to the bin target) so release builds on Windows don't spawn a
// background console window.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    submarine_lib::run();
}
