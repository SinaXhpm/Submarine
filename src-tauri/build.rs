fn main() {
    tauri_build::build();

    // Windows: the `cargo test` binary loads COMCTL32 v5 (no v6 manifest —
    // tauri-build embeds the manifest in the app binary only), but the binary
    // statically imports TaskDialogIndirect (COMCTL32 v6 only), so it fails
    // to start with 0xc0000139 STATUS_ENTRYPOINT_NOT_FOUND before any test
    // runs. The import comes from tauri-runtime-wry's own dialog code, not
    // from this crate, so it cannot be removed from here. Embed the same v6
    // manifest into every binary this package links (including the lib's test
    // binary) via the general `cargo:rustc-link-arg`. Note this also applies
    // to the app binary, which already gets a manifest from tauri-build —
    // if the app build ever reports a duplicate-resource error, this is why.
    // `cargo:rustc-link-arg-tests` cannot be used instead: it only applies
    // to `[[test]]` integration targets, and this package has none.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        println!("cargo:rerun-if-changed=windows-app-manifest.rc");
        println!("cargo:rerun-if-changed=windows-app-manifest.xml");
        embed_resource::compile_for_everything("windows-app-manifest.rc", embed_resource::NONE)
            .manifest_optional()
            .expect("failed to embed the Windows app manifest into test binaries");
    }
}
