fn main() {
    tauri_build::build();

    // Windows: tauri-build embeds the Common-Controls v6 manifest into the
    // app binary only (`cargo:rustc-link-arg-bins`), so `cargo test` binaries
    // load COMCTL32 v5 instead. v5 does not export TaskDialogIndirect (used
    // by rfd::MessageDialog), so the test binary fails to start with
    // 0xc0000139 STATUS_ENTRYPOINT_NOT_FOUND before any test runs. Embed the
    // same manifest into test binaries only (`cargo:rustc-link-arg-tests`) —
    // a scope tauri-build does not touch, so the app binary is not linked
    // twice. The manifest is the same one tauri-build embeds (see
    // windows-app-manifest.xml). No-op on non-Windows: embed-resource
    // returns NotWindows and `manifest_optional` tolerates it.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        println!("cargo:rerun-if-changed=windows-app-manifest.rc");
        println!("cargo:rerun-if-changed=windows-app-manifest.xml");
        embed_resource::compile_for_tests("windows-app-manifest.rc", embed_resource::NONE)
            .manifest_optional()
            .expect("failed to embed the Windows app manifest into test binaries");
    }
}
