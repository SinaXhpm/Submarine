# Windows setup

`setup.exe` (released as `Submarine_<version>_x64-setup.exe`) and the `uninstall.exe` it puts next to the app. The window is a page (`ui/`) in WebView2, which the app needs anyway, in the app's own look and in the app's 15 languages (`i18n/`).

It installs for the current user, without administrator rights, into the folder and under the registry keys Tauri's NSIS installer used, so a copy installed with that one updates in place:

| What | Where |
|---|---|
| App and uninstaller | `%LOCALAPPDATA%\Submarine\` (or the folder picked under Options) |
| Apps entry | `HKCU\Software\Microsoft\Windows\CurrentVersion\Uninstall\Submarine` |
| Remembered folder | `HKCU\Software\submarine\Submarine` |
| Shortcuts | Start menu always, desktop when ticked; both carry the app's ID (`com.submarine.app`) |

The uninstaller removes the app, its shortcuts (only ones that point to it), the jump list and the Apps entry. With "Also delete my profiles and settings" ticked it also deletes `%APPDATA%\com.submarine.app` and `%LOCALAPPDATA%\com.submarine.app`, and the remembered folder. It deletes nothing else: no folder is ever deleted with what is in it, except those two.

A running app is closed the way Windows closes it at sign-out (it saves and quits), after the user agrees; only what is still running after that is ended. A copy installed for all users from the `.msi` is removed first (Windows asks for administrator rights), except in a silent install. Without WebView2, setup offers to install it from Microsoft and checks that the download is signed by Microsoft.

## Command line

The NSIS installer's switches, so scripts and package managers keep working:

| | |
|---|---|
| `/S` | silent: no window |
| `/P` | passive: progress only, no questions |
| `/NS` | no shortcuts |
| `/R` | start the app after a silent or passive install |
| `/UPDATE` | an update: shortcuts stay as they are |
| `/D=<folder>` | install folder; last on the line, unquoted |
| `_?=<folder>` | (uninstaller) the install folder; last on the line |

Exit codes: 0 done, 1 cancelled, 2 failed. The log is `%TEMP%\submarine-setup.log`.

## Building

CI (`.github/workflows/release.yml`) builds it after the app: the uninstaller first, then setup with both packed in.

```powershell
cd packaging/windows/setup
cargo build --release --target x86_64-pc-windows-msvc --bin uninstall
$env:SUBMARINE_SETUP_APP = "<path>\submarine.exe"           # the app, built
$env:SUBMARINE_SETUP_UNINSTALLER = "<copy of>\uninstall.exe" # from the line above
cargo build --release --target x86_64-pc-windows-msvc --bin setup
```

`build.rs` takes the name, version and identifier from `src-tauri/tauri.conf.json`, checks that every language has every English text with the same `{placeholders}`, inlines the page, and packs the app with brotli. `SUBMARINE_SETUP_QUALITY=5` packs faster for a quick try (the release uses 11).

## Trying it without touching an installed Submarine

`SUBMARINE_SETUP_TEST=1` builds it as "Submarine Setup Test": its own folder, Apps entry, shortcuts and data folders. `SUBMARINE_SETUP_VERSION` sets another version (to try an update). The `lab` feature, for test builds only, adds a DevTools port, a window position and a language from `SUBMARINE_SETUP_CDP`, `SUBMARINE_SETUP_POS` and `SUBMARINE_SETUP_LANG`.

To change the look, open `ui/page.html` with `ui/style.css` and `ui/app.js` filled in; `window.__SETUP__` holds what setup passes to the page (see `install.rs` and `uninstall.rs`).
