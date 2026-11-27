fn main() {
    if std::env::var_os("CARGO_FEATURE_TAILCAT_CAPI").is_some() {
        println!("cargo:rerun-if-env-changed=SUBMARINE_LIBTAILCAT_DIR");
        let dir = std::env::var("SUBMARINE_LIBTAILCAT_DIR")
            .expect("tailcat-capi requires SUBMARINE_LIBTAILCAT_DIR to contain libtailcat");
        let root = std::path::Path::new(&dir);
        let target_dir = root.join(std::env::var("TARGET").unwrap());
        let target_dir = if target_dir.join("libtailcat.so").exists() {
            target_dir
        } else {
            root.to_path_buf()
        };
        println!("cargo:rerun-if-changed={}", target_dir.display());
        println!("cargo:rustc-link-search=native={}", target_dir.display());
        println!("cargo:rustc-link-lib=dylib=tailcat");
    }
    tauri_build::build()
}
