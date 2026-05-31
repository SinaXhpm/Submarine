<div align="center">
  <img src="src/assets/logo.png" alt="Submarine logo" width="120" />
  <h1>Submarine — Modern SSH &amp; SFTP Client</h1>
  <p><strong>A fast, secure, cross-platform SSH and SFTP desktop client for Windows, macOS, and Linux.</strong></p>
  <p>Manage servers, edit remote files, forward ports, and sync folders — all from one native window. Built with Rust and Tauri.</p>
  <p>
    <a href="https://github.com/sinaxhpm/submarine/releases"><img alt="release" src="https://img.shields.io/github/v/release/sinaxhpm/submarine?include_prereleases" /></a>
    <a href="https://github.com/sinaxhpm/submarine/actions"><img alt="ci" src="https://img.shields.io/github/actions/workflow/status/sinaxhpm/submarine/release.yml" /></a>
    <img alt="platforms" src="https://img.shields.io/badge/platform-windows%20%7C%20macos%20%7C%20linux-blue" />
    <img alt="license" src="https://img.shields.io/badge/license-MIT-green" />
  </p>
</div>

---

## Why Submarine

Most SSH clients feel like they were built a decade ago and never updated. Submarine is a clean, modern alternative for developers and sysadmins who connect to remote servers every day.

- **Native and fast.** Built with Rust + Tauri — boots in under a second, uses around 80 MB of RAM with several sessions open. No Electron, no battery drain.
- **One window, everything you need.** Terminal, SFTP file browser, port tunnels, folder sync, and saved commands — all live as tabs around each server. No more juggling PuTTY plus WinSCP plus a tunnel app.
- **Secure by default.** Saved servers are encrypted on disk. Optional cloud sync only ever sees encrypted blobs — your passwords and keys can't be read by anyone but you.
- **Truly cross-platform.** Same app on Windows, macOS (Apple Silicon + Intel via Rosetta), and every major Linux distro — `.deb`, `.rpm`, Arch package, and AppImage.

## Key Features

### Terminal

- Tabbed sessions per server and multiple shells per session
- Full xterm.js — true colour, ANSI 256, mouse support
- **Select to copy** — release the mouse over your selection and it's in the clipboard
- Right-click to paste
- Wrapped lines copy as a single line (no phantom blank rows)

### SFTP File Browser

- Dual-pane (local on top, remote on bottom) with drag-and-drop between them
- **Multi-select with Ctrl/Shift-click** for bulk download, move, or delete
- **Live edit** — double-click any remote file to open it in your default editor; saves auto-upload back to the server
- **Direct download** to the local folder you already have open — no folder picker every time
- Permissions editor with a checkbox grid (chmod / chown)
- Drag files in from your OS file manager to upload

### Folder Mirror

Pick a local folder, pick a remote folder, and Submarine keeps them in sync.

- **Two-way initial sync.** Walks both sides, compares by size and content hash (not just timestamp), and merges them into the same state.
- **Live watcher.** Every save, create, rename, or delete on your local folder pushes to the server in real time.
- **Conflict resolution per mirror.** Pick what wins when a file differs on both sides: **Local wins** (default, like rsync), **Remote wins**, or **Newer wins**.
- **Soft delete.** Removed files move to `.submarine-trash/` on the server — a fat-fingered `rm` won't wipe out remote work.

### Port Forwarding

- Local (`-L`), Remote (`-R`), and Dynamic (`-D`) tunnels
- Dynamic supports **SOCKS4, SOCKS4a, SOCKS5, SOCKS5h, HTTP CONNECT, and plain HTTP proxy**
- Each tunnel starts and stops independently, saved with the server profile

### End-to-End Encrypted Profile Sync

- Save servers once, access them on every machine you own
- The sync server hosts an opaque encrypted blob — we have no way to read it (see [Security](#security))
- No registration. No password recovery email. Your master password never leaves your device.

### Productivity

- **Quick commands & notes**: per-server snippet library, surfaced inside any open session
- **Autostart**: flag a server and it opens and connects when the app launches
- **Resource monitor**: live CPU, RAM, disk, and network sparklines per host
- **Quick connect**: one-off sessions without saving
- **TOFU host keys**: first-use prompt with fingerprint, pinned afterwards

## Install

Pick a binary from the [latest release](https://github.com/sinaxhpm/submarine/releases) and you're done — no extra dependencies.

| OS | File |
|---|---|
| Windows 10 / 11 | `.exe` installer or `.msi` |
| macOS (Apple Silicon, or Intel via Rosetta) | `.dmg` or `.app.zip` |
| Debian / Ubuntu / Mint | `.deb` — `sudo apt install ./submarine_*.deb` |
| Fedora / RHEL / openSUSE | `.rpm` — `sudo dnf install ./submarine-*.rpm` |
| Arch / Manjaro / EndeavourOS | `.pkg.tar.zst` — `sudo pacman -U submarine-*.pkg.tar.zst` |
| Any Linux | `.AppImage` — `chmod +x` and double-click |

> Builds are currently **unsigned**. Windows SmartScreen will prompt — click "More info → Run anyway". On macOS you may need `xattr -d com.apple.quarantine /Applications/Submarine.app`.

## Security

Your data is encrypted on your machine before anything leaves it. Submarine and the sync server never see your passwords, private keys, or profile content.

**How it works:**

1. **Your master password becomes a vault key — on your device.** When you unlock, your password is run through Argon2id to derive a key. The password is wiped from memory the moment derivation finishes. It never goes anywhere.
2. **Profiles are sealed with AES-256-GCM — on your device.** Every saved server (credentials, keys, tunnels, notes, mirrors) is compressed and encrypted with that key. What hits disk is opaque ciphertext.
3. **Sync uploads that same ciphertext — byte for byte.** When you turn cloud sync on, the bytes uploaded are exactly the encrypted blob from your disk. The server stores ciphertext plus a random nonce. Nothing else.

**Why the sync server can't read your profiles:**

- No registration with a server-side password. No recovery email. No "forgot password" flow. **There is nothing on the server that could be used to derive your key.**
- Encryption happens before the upload call. By the time bytes hit the network they are already ciphertext with no key material attached.
- Even with a full server breach, an attacker gets the same opaque blob they'd see if you uploaded a random file to S3. Same threat model.

**Other defences:**

- Master key kept in zeroized memory, wiped on lock
- TOFU host keys with per-connection nonce binding (resists prompt-race attacks)
- Strict Content Security Policy, minimal Tauri permission ACL, no shell or filesystem plugins exposed to the UI

## FAQ

### Is Submarine free?

Yes. MIT-licensed. Use it personally, use it at work, fork it, redistribute it.

### Does it work offline?

Yes. Cloud sync is opt-in. All profiles, mirrors, and tunnels live locally and are encrypted at rest.

### How is it different from PuTTY, WinSCP, or MobaXterm?

PuTTY and WinSCP are two separate apps you alt-tab between. MobaXterm bundles them but feels dated and is Windows-only. Submarine puts terminal, SFTP, tunnels, and folder mirror in one modern tabbed window per server — and runs natively on Windows, macOS, and Linux.

### Does it support keys with passphrases?

Yes. File-based keys and pasted PEM/OpenSSH keys, with or without passphrase.

### Can I sync profiles between Windows and macOS?

Yes. The encrypted vault is platform-portable. Enable sync on one machine, unlock on another with the same master password.

### Why Rust and Tauri instead of Electron?

Smaller installer (around 10 MB vs ~100 MB for an Electron equivalent), lower RAM, faster startup, native window controls, and no Chromium per app.

### Where are my profiles stored?

In a single encrypted file under your OS app-data directory. Nothing in plaintext. Nothing in a global Keychain or registry hive.

## Build from Source

Requirements: Node 20+, Rust stable, [Tauri prerequisites](https://v2.tauri.app/start/prerequisites/) for your OS. Windows additionally needs **Strawberry Perl** for the vendored OpenSSL build (`winget install StrawberryPerl.StrawberryPerl`).

```bash
git clone https://github.com/sinaxhpm/submarine
cd submarine
npm install
npm run tauri dev          # run in development
npm run tauri build        # build release bundle
```

## Tech Stack

**Frontend** — React 18 · TypeScript · Tailwind CSS · xterm.js
**Backend** — Rust · Tauri 2 · russh · rusqlite · aes-gcm · argon2 · zstd

## Credits

Designed and built by [Sina](https://sinaxhpm.com) in collaboration with [Claude](https://claude.com/claude-code). Every line of code in this repo was written together with Anthropic's Claude.

## License

MIT — see [LICENSE](LICENSE).

---

<sub><i>Bitvise, forever in my heart.</i></sub>
