<div align="center">
  <img src="src/assets/logo.png" alt="Submarine" width="120" />
  <h1>Submarine</h1>
  <p><strong>A modern, secure SSH/SFTP desktop client.</strong></p>
  <p>
    <a href="https://github.com/sinaxhpm/submarine/releases"><img alt="release" src="https://img.shields.io/github/v/release/sinaxhpm/submarine?include_prereleases" /></a>
    <a href="https://github.com/sinaxhpm/submarine/actions"><img alt="ci" src="https://img.shields.io/github/actions/workflow/status/sinaxhpm/submarine/release.yml" /></a>
    <img alt="platforms" src="https://img.shields.io/badge/platform-windows%20%7C%20macos%20%7C%20linux-blue" />
  </p>
</div>

---

## What it is

Submarine is a fast, local-first SSH/SFTP client with optional end-to-end encrypted profile sync across your machines. Built with Rust (Tauri 2) and React — small binary, native feel, no Electron.

## Features

- **SSH terminal** — tabbed sessions, full xterm.js, ANSI 256 + truecolour, mouse support
- **SFTP browser** — drag-and-drop uploads, in-place rename, chmod/chown, dual-pane workspace
- **Port forwarding** — local (`-L`), remote (`-R`), and dynamic SOCKS/HTTP (`-D`) tunnels — SOCKS4/4a, SOCKS5/5h, HTTP CONNECT, plain HTTP proxy
- **Resource monitor** — live CPU / RAM / disk / network sparklines per host
- **Quick commands + notes** — per-profile editable snippets, surfaced inside any open session via the in-pane Library
- **Saved profiles** — passwords and private keys stored in a local AES-256-GCM vault
- **End-to-end encrypted sync** — your profiles roam across machines; we only host the opaque ciphertext, never the keys
- **Broad SSH compatibility** — Ed25519, ECDSA P-256, RSA (SHA-2/SHA-1) host keys + matching client keys; legacy KEX (DH-G14/G1) and MAC (HMAC-SHA1) for older servers
- **Autostart** — flag any server and it opens + connects automatically when the app launches
- **TOFU host keys** — first-use prompt with fingerprint, pinned thereafter
- **Quick connect** — one-off connections without saving

## Security

Everything is encrypted on your machine before it leaves it. Neither Submarine nor the sync server ever sees your passwords, private keys, or any profile content.

**How it works, end to end:**

1. **Master password → vault key** (on your device).
   When you unlock, your password runs through **Argon2id** (m=64 MiB, t=3, p=4) to derive a vault key. The password is wiped from memory the moment derivation finishes; it never goes anywhere else.
2. **Profiles → encrypted vault** (on your device).
   Every saved profile — credentials, private keys, tunnels, notes, mirrors — is serialised, compressed with zstd, then sealed with **AES-256-GCM** using that vault key. What lands on disk is an opaque ciphertext.
3. **Vault → cloud sync** (end-to-end).
   When you turn sync on, the bytes we upload are that exact same encrypted blob. The server stores ciphertext + a random nonce + a version stamp. That's it.

**Why the sync backend can't read your profiles:**

- The vault key is derived **only on your device**, from a password we never receive — no register-with-password, no recovery email, no "forgot password" link, nothing the server could derive a key from.
- The blob is encrypted **before** the upload call. By the time the bytes hit the network they're already AES-256-GCM ciphertext with no key material attached.
- Argon2id's cost (64 MiB / 3 passes / 4 lanes) makes brute-forcing the master password from a leaked ciphertext infeasible.
- Worst case — full server compromise — an attacker gets the same opaque blob you'd see if you uploaded a random file to S3. Same threat model.

**Other defences:**

- Master key lives in `zeroize`-wrapped memory and is wiped on lock.
- TOFU host keys, pinned per-host with a per-connection nonce binding (prevents prompt-races).
- Strict CSP, minimal Tauri capability ACL, no shell/fs plugins exposed to the UI.

## Install

Grab a binary from the [latest release](https://github.com/sinaxhpm/submarine/releases):

| OS | File |
|---|---|
| Windows | `Submarine_x.y.z_x64-setup.exe` or `.msi` |
| macOS (Apple Silicon) | `Submarine-vx.y.z-macos-arm64.app.zip` or `.dmg` |
| Debian / Ubuntu / Mint | `submarine_x.y.z_amd64.deb` |
| Fedora / RHEL / openSUSE | `submarine-x.y.z-1.x86_64.rpm` |
| Arch / Manjaro / EndeavourOS | `submarine-x.y.z-1-x86_64.pkg.tar.zst` (`sudo pacman -U`) |
| Any Linux | `submarine_x.y.z_amd64.AppImage` (chmod +x and run) |

> Builds are currently **unsigned**. Windows SmartScreen will prompt — click "More info → Run anyway". macOS users may need `xattr -d com.apple.quarantine /Applications/Submarine.app`.

## Build from source

Requirements: Node 20+, Rust stable, plus [Tauri prerequisites](https://v2.tauri.app/start/prerequisites/) for your OS. Windows additionally needs **Strawberry Perl** for the vendored OpenSSL build (`winget install StrawberryPerl.StrawberryPerl`); macOS and Linux already ship with Perl.

```bash
git clone https://github.com/sinaxhpm/submarine
cd submarine
npm install
npm run tauri dev          # development
npm run tauri build        # release bundle
```

## Releasing

Push a tag matching `v*` and the GitHub Actions workflow builds installers for all platforms and drafts a release:

```bash
git tag v0.1.0
git push origin v0.1.0
```

## Tech stack

**Frontend** — React 18, TypeScript, Tailwind, xterm.js
**Backend** — Rust, Tauri 2, russh, rusqlite, aes-gcm, argon2, zstd

## Credits

Designed and built by [Sina](https://sinaxhpm.com) in close collaboration with [Claude](https://claude.com/claude-code) — every line of code in this repo was written together with Anthropic's Claude.

## License

MIT
