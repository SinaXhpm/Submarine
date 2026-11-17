// Builds what the installer carries:
//   product.rs  name, version, identifier ... from src-tauri/tauri.conf.json, so the
//               installer always matches the app it installs;
//   i18n.rs     the texts, checked: every language has every English key, with the
//               same {placeholders};
//   page.html   the page with its style, script, fonts and logo inlined;
//   payload.rs  the app and the uninstaller, packed with brotli (setup only; from
//               SUBMARINE_SETUP_APP and SUBMARINE_SETUP_UNINSTALLER);
// and gives each exe its icon, manifest (asInvoker, per-monitor DPI) and version.
use std::collections::BTreeMap;
use std::env;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

// The app's languages (src/i18n): a text file for any other is an error.
const LANGUAGES: &[&str] = &[
    "en", "de", "es", "fr", "it", "pl", "pt-BR", "tr", "ru", "uk", "hi", "ja", "ko", "zh-CN", "zh-TW",
];

struct Product {
    name: String,
    version: String,
    identifier: String,
    manufacturer: String,
    publisher: String,
    homepage: String,
    copyright: String,
    main_exe: String,
}

fn main() {
    let crate_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let repo = crate_dir.join("..").join("..").join("..");
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());

    let product = product(&repo);
    write_product(&out, &product);
    texts(&crate_dir, &out);
    page(&crate_dir, &repo, &out);
    payload(&out, &product);
    resources(&crate_dir, &repo, &out, &product);

    if env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        // The exe's own imports come from System32 only, never from the folder
        // the setup was downloaded to (DLL planting). Windows 10 1607 and later
        // honour it; older ones ignore it.
        println!("cargo:rustc-link-arg-bins=/DEPENDENTLOADFLAG:0x800");
        static_vcruntime(&out);
    }
}

/// Links the Visual C++ runtime into the exe, as the app does (tauri-build),
/// so setup runs on a Windows without the VC++ redistributable; the UCRT stays
/// a DLL, part of Windows 10 and later. From ChrisDenton/static_vcruntime
/// (MIT/Apache-2.0) by way of tauri-build.
fn static_vcruntime(out: &Path) {
    // Rust asks for msvcrt.lib by name: an empty one found first takes its place.
    let machine: &[u8] = match env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
        Ok("x86_64") => &[0x64, 0x86],
        Ok("x86") => &[0x4C, 0x01],
        _ => return,
    };
    let empty_lib: &[u8] = &[
        1, 0, 94, 3, 96, 98, 60, 0, 0, 0, 1, 0, 0, 0, 0, 0, 132, 1, 46, 100, 114, 101, 99, 116, 118, 101, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 60, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 10, 16, 0, 46, 100, 114, 101, 99, 116, 118, 101, 0,
        0, 0, 0, 1, 0, 0, 0, 3, 0, 4, 0, 0, 0,
    ];
    fs::write(out.join("msvcrt.lib"), [machine, empty_lib].concat()).unwrap();
    println!("cargo:rustc-link-search=native={}", out.display());
    for lib in ["libvcruntimed.lib", "vcruntime.lib", "vcruntimed.lib", "libcmtd.lib", "msvcrt.lib", "msvcrtd.lib", "libucrt.lib", "libucrtd.lib"] {
        println!("cargo:rustc-link-arg=/NODEFAULTLIB:{lib}");
    }
    for lib in ["libcmt.lib", "libvcruntime.lib", "ucrt.lib"] {
        println!("cargo:rustc-link-arg=/DEFAULTLIB:{lib}");
    }
}

fn read(path: &Path) -> Vec<u8> {
    println!("cargo:rerun-if-changed={}", path.display());
    fs::read(path).unwrap_or_else(|e| panic!("can't read {}: {e}", path.display()))
}

fn read_str(path: &Path) -> String {
    String::from_utf8(read(path)).unwrap_or_else(|_| panic!("{} isn't UTF-8", path.display()))
}

fn product(repo: &Path) -> Product {
    let conf: serde_json::Value =
        serde_json::from_str(&read_str(&repo.join("src-tauri").join("tauri.conf.json"))).expect("tauri.conf.json");
    let s = |v: &serde_json::Value| v.as_str().unwrap_or_default().to_string();
    let mut identifier = s(&conf["identifier"]);
    let mut name = s(&conf["productName"]);
    let mut version = s(&conf["version"]);
    // The registry key the NSIS installer kept the install folder under:
    // Software\<the identifier's second part>\<product> ("submarine"). It stays
    // that key, so installs made with that one keep updating in place.
    let mut manufacturer = identifier.split('.').nth(1).unwrap_or(&identifier).to_string();
    // The publisher Settings > Apps shows, and the exe's company name.
    let publisher = match conf["bundle"]["publisher"].as_str() {
        Some(p) if !p.is_empty() => p.to_string(),
        _ => "Sina Xhpm".to_string(),
    };
    // The exe's name: mainBinaryName, else the Cargo package's name.
    let main_exe = match conf["mainBinaryName"].as_str() {
        Some(n) if !n.is_empty() => format!("{n}.exe"),
        _ => {
            let cargo = read_str(&repo.join("src-tauri").join("Cargo.toml"));
            let mut in_package = false;
            let mut found = None;
            for line in cargo.lines() {
                let line = line.trim();
                if line.starts_with('[') {
                    in_package = line == "[package]";
                } else if in_package && line.starts_with("name") {
                    found = line.split('"').nth(1).map(|n| format!("{n}.exe"));
                    break;
                }
            }
            found.expect("src-tauri/Cargo.toml has no package name")
        }
    };

    // A test build installs under its own name, folder and registry keys, so it
    // never touches a real installation on the machine it is tried on.
    println!("cargo:rerun-if-env-changed=SUBMARINE_SETUP_TEST");
    if env::var_os("SUBMARINE_SETUP_TEST").is_some() {
        name = "Submarine Setup Test".into();
        identifier = "com.submarine.setuptest".into();
        manufacturer = "submarine-setuptest".into();
    }
    println!("cargo:rerun-if-env-changed=SUBMARINE_SETUP_VERSION");
    if let Ok(v) = env::var("SUBMARINE_SETUP_VERSION") {
        version = v;
    }

    for (what, v) in [("productName", &name), ("version", &version), ("identifier", &identifier)] {
        assert!(!v.is_empty(), "tauri.conf.json has no {what}");
    }
    assert!(
        identifier.contains('.') && !identifier.contains(['\\', '/', ':']),
        "unexpected identifier {identifier:?}: the uninstaller deletes data folders named after it"
    );
    Product {
        name,
        version,
        identifier,
        manufacturer,
        publisher,
        homepage: s(&conf["bundle"]["homepage"]),
        copyright: s(&conf["bundle"]["copyright"]),
        main_exe,
    }
}

fn write_product(out: &Path, p: &Product) {
    let mut s = String::new();
    for (k, v) in [
        ("PRODUCT_NAME", &p.name),
        ("VERSION", &p.version),
        ("IDENTIFIER", &p.identifier),
        ("MANUFACTURER", &p.manufacturer),
        ("PUBLISHER", &p.publisher),
        ("HOMEPAGE", &p.homepage),
        ("COPYRIGHT", &p.copyright),
        ("MAIN_EXE", &p.main_exe),
    ] {
        writeln!(s, "pub const {k}: &str = {v:?};").unwrap();
    }
    fs::write(out.join("product.rs"), s).unwrap();
}

fn placeholders(s: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut rest = s;
    while let Some(start) = rest.find('{') {
        let after = &rest[start + 1..];
        match after.find('}') {
            Some(end) if after[..end].chars().all(|c| c.is_ascii_alphanumeric() || c == '_') && end > 0 => {
                found.push(after[..end].to_string());
                rest = &after[end + 1..];
            }
            _ => rest = after,
        }
    }
    found.sort();
    found
}

fn texts(crate_dir: &Path, out: &Path) {
    let dir = crate_dir.join("i18n");
    println!("cargo:rerun-if-changed={}", dir.display());
    let load = |lang: &str| -> BTreeMap<String, String> {
        let v: serde_json::Value = serde_json::from_str(&read_str(&dir.join(format!("{lang}.json"))))
            .unwrap_or_else(|e| panic!("i18n/{lang}.json: {e}"));
        let obj = v.as_object().unwrap_or_else(|| panic!("i18n/{lang}.json isn't an object"));
        obj.iter()
            .map(|(k, v)| (k.clone(), v.as_str().unwrap_or_else(|| panic!("i18n/{lang}.json: {k} isn't text")).to_string()))
            .collect()
    };
    for entry in fs::read_dir(&dir).unwrap() {
        let name = entry.unwrap().file_name().to_string_lossy().into_owned();
        let lang = name.strip_suffix(".json").unwrap_or_else(|| panic!("i18n/{name}: not a .json file"));
        assert!(LANGUAGES.contains(&lang), "i18n/{name}: not one of the app's languages");
    }
    let en = load("en");
    let mut gen = String::from("pub static LANGS: &[(&str, &str)] = &[\n");
    let mut problems = Vec::new();
    for lang in LANGUAGES {
        let texts = load(lang);
        for key in en.keys() {
            match texts.get(key) {
                None => problems.push(format!("{lang}: {key} is missing")),
                Some(t) if t.trim().is_empty() => problems.push(format!("{lang}: {key} is empty")),
                Some(t) if placeholders(t) != placeholders(&en[key]) => {
                    problems.push(format!("{lang}: {key} has other placeholders than English"))
                }
                _ => {}
            }
        }
        for key in texts.keys().filter(|k| !en.contains_key(*k)) {
            problems.push(format!("{lang}: {key} isn't an English key"));
        }
        let json = serde_json::to_string(&texts).unwrap();
        writeln!(gen, "    ({lang:?}, {json:?}),").unwrap();
    }
    assert!(problems.is_empty(), "texts:\n  {}", problems.join("\n  "));
    gen.push_str("];\n");
    fs::write(out.join("i18n.rs"), gen).unwrap();
}

fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        s.push(T[(n >> 18) as usize & 63] as char);
        s.push(T[(n >> 12) as usize & 63] as char);
        s.push(if c.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        s.push(if c.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    s
}

fn data_uri(path: &Path, mime: &str) -> String {
    format!("data:{mime};base64,{}", base64(&read(path)))
}

fn page(crate_dir: &Path, repo: &Path, out: &Path) {
    let ui = crate_dir.join("ui");
    let fonts = repo.join("website").join("shared").join("fonts");
    let license = read_str(&fonts.join("LICENSE-Inter.txt")).replace("*/", "* /");
    let css = read_str(&ui.join("style.css"))
        .replace("__INTER_LICENSE__", &license)
        .replace("__FONT_LATIN__", &data_uri(&fonts.join("inter-latin-wght-normal.woff2"), "font/woff2"))
        .replace("__FONT_LATIN_EXT__", &data_uri(&fonts.join("inter-latin-ext-wght-normal.woff2"), "font/woff2"))
        .replace("__FONT_CYRILLIC__", &data_uri(&fonts.join("inter-cyrillic-wght-normal.woff2"), "font/woff2"));
    let js = read_str(&ui.join("app.js"));
    let logo = data_uri(&repo.join("src-tauri").join("icons").join("128x128@2x.png"), "image/png");
    let html = read_str(&ui.join("page.html"));
    for marker in ["/*__STYLE__*/", "/*__SCRIPT__*/", "__LOGO__"] {
        assert!(html.contains(marker), "ui/page.html lost its {marker} marker");
    }
    let html = html
        .replace("/*__STYLE__*/", &css)
        .replace("/*__SCRIPT__*/", &js)
        .replace("__LOGO__", &logo);
    fs::write(out.join("page.html"), html).unwrap();
}

fn payload(out: &Path, product: &Product) {
    println!("cargo:rerun-if-env-changed=SUBMARINE_SETUP_APP");
    println!("cargo:rerun-if-env-changed=SUBMARINE_SETUP_UNINSTALLER");
    println!("cargo:rerun-if-env-changed=SUBMARINE_SETUP_QUALITY");
    let (Some(app), Some(uninstaller)) = (env::var_os("SUBMARINE_SETUP_APP"), env::var_os("SUBMARINE_SETUP_UNINSTALLER"))
    else {
        println!("cargo:warning=the setup has nothing to install: set SUBMARINE_SETUP_APP and SUBMARINE_SETUP_UNINSTALLER");
        fs::write(out.join("payload.rs"), "pub static PAYLOAD: &[setup_core::install::PayloadFile] = &[];\n").unwrap();
        return;
    };
    // Brotli at its best (quality 11, a 16 MB window) packs the app as small as
    // NSIS's LZMA did; a quick local build can ask for less.
    let quality: i32 = env::var("SUBMARINE_SETUP_QUALITY").ok().and_then(|q| q.parse().ok()).unwrap_or(11);
    let dir = out.join("payload");
    fs::create_dir_all(&dir).unwrap();
    let mut gen = String::from("pub static PAYLOAD: &[setup_core::install::PayloadFile] = &[\n");
    for (name, src) in [(product.main_exe.as_str(), PathBuf::from(app)), ("uninstall.exe", PathBuf::from(uninstaller))] {
        let data = read(&src);
        let sha: [u8; 32] = Sha256::digest(&data).into();
        let hex: String = sha.iter().map(|b| format!("{b:02x}")).collect();
        let packed = dir.join(format!("{name}.br"));
        let stamp = dir.join(format!("{name}.stamp"));
        let want = format!("{hex} q{quality}");
        if fs::read_to_string(&stamp).ok().as_deref() != Some(want.as_str()) || !packed.exists() {
            let params = brotli::enc::BrotliEncoderParams {
                quality,
                lgwin: 24,
                size_hint: data.len(),
                ..Default::default()
            };
            let mut packed_data = Vec::new();
            brotli::BrotliCompress(&mut &data[..], &mut packed_data, &params).expect("brotli");
            fs::write(&packed, &packed_data).unwrap();
            fs::write(&stamp, &want).unwrap();
        }
        writeln!(
            gen,
            "    setup_core::install::PayloadFile {{ name: {name:?}, size: {}, sha256: {sha:?}, data: include_bytes!({:?}) }},",
            data.len(),
            packed.display().to_string()
        )
        .unwrap();
    }
    gen.push_str("];\n");
    fs::write(out.join("payload.rs"), gen).unwrap();
}

fn rc_string(s: &str) -> String {
    s.replace('"', "\"\"")
}

fn rc_path(p: &Path) -> String {
    p.display().to_string().replace('\\', "\\\\")
}

fn resources(crate_dir: &Path, repo: &Path, out: &Path, p: &Product) {
    let icon = repo.join("src-tauri").join("icons").join("icon.ico");
    let manifest = crate_dir.join("resources").join("setup.manifest");
    read(&icon);
    read(&manifest);
    // FILEVERSION takes four numbers: "0.5.0-beta.6" -> 0,5,0,0.
    let mut nums: Vec<u16> = p.version.split(['-', '+']).next().unwrap().split('.').map(|n| n.parse().unwrap_or(0)).collect();
    nums.resize(4, 0);
    let numeric = nums.iter().map(|n| n.to_string()).collect::<Vec<_>>().join(",");
    for (bin, description) in [("setup", format!("{} Setup", p.name)), ("uninstall", format!("{} Uninstaller", p.name))] {
        let rc = format!(
            r#"#pragma code_page(65001)
1 ICON "{icon}"
1 24 "{manifest}"
1 VERSIONINFO
FILEVERSION {numeric}
PRODUCTVERSION {numeric}
FILEFLAGSMASK 0x3F
FILEFLAGS 0x0
FILEOS 0x40004
FILETYPE 0x1
FILESUBTYPE 0x0
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904B0"
    BEGIN
      VALUE "CompanyName", "{publisher}"
      VALUE "FileDescription", "{description}"
      VALUE "FileVersion", "{version}"
      VALUE "InternalName", "{bin}"
      VALUE "LegalCopyright", "{copyright}"
      VALUE "OriginalFilename", "{bin}.exe"
      VALUE "ProductName", "{name}"
      VALUE "ProductVersion", "{version}"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x409, 1200
  END
END
"#,
            icon = rc_path(&icon),
            manifest = rc_path(&manifest),
            description = rc_string(&description),
            publisher = rc_string(&p.publisher),
            version = rc_string(&p.version),
            copyright = rc_string(&p.copyright),
            name = rc_string(&p.name),
        );
        let path = out.join(format!("{bin}.rc"));
        fs::write(&path, rc).unwrap();
        embed_resource::compile_for(&path, [bin], embed_resource::NONE)
            .manifest_required()
            .unwrap_or_else(|e| panic!("resources for {bin}: {e:?}"));
    }
}
