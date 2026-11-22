fn main() {
    if std::env::var_os("CARGO_FEATURE_TAILCAT_CAPI").is_some() {
        println!("cargo:rerun-if-env-changed=SUBMARINE_LIBTAILCAT_DIR");
        let dir = std::env::var("SUBMARINE_LIBTAILCAT_DIR")
            .expect("tailcat-capi requires SUBMARINE_LIBTAILCAT_DIR to contain libtailcat");
        println!("cargo:rerun-if-changed={dir}");
        println!("cargo:rustc-link-search=native={dir}");
        println!("cargo:rustc-link-lib=dylib=tailcat");
    }
    tauri_build::build()
}
