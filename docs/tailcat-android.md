# Native Tailcat transport

Submarine's Tailcat transport is deliberately application-scoped on every
platform. Rust connects an ephemeral loopback stream to its existing
`russh::client::connect_stream` implementation. No user needs to install the
Tailcat CLI, Tailscale, a VPN, or a system TUN interface.

- Android packages the Go Mobile bridge in the APK.
- Linux, macOS and Windows package the same Go bridge as a Tauri sidecar. It
  is spawned privately by Submarine and exits when Submarine closes.

## Build the native dependency

Install Go `1.27.1`, Android SDK/NDK, and set `ANDROID_NDK_HOME`, then run:

```bash
cd tailcat-bridge
./build-aar.sh
cd ../src-tauri/gen/android
./gradlew :app:assembleDebug
```

The bridge pins Tailcat to `b4dc28e8aa8936f0a90a41ad8293a64e3d6b645f`
(`v0.0.0-20260929145319-b4dc28e8aa89`) and builds both `arm64-v8a` and
`x86_64`. The generated AAR is intentionally ignored; release CI must generate
it before Gradle packaging. Verify its ABI payload with:

```bash
unzip -l src-tauri/gen/android/app/libs/tailcat-bridge.aar | grep libgojni.so
```

## Security

Tailcat addresses are stored in the encrypted profile vault as the node's host
field when transport is Tailcat. They are masked in backend logs and never
become the SSH known-hosts key: Submarine uses `tailcat:` plus a SHA-256 prefix
instead. SSH host-key verification and normal key/password/keyboard-interactive
authentication remain enabled.

## Desktop sidecars

Before a desktop bundle, build the helper matching its Tauri target:

```bash
GO_BIN=/path/to/go tailcat-bridge/build-sidecars.sh x86_64-unknown-linux-gnu
```

Use `all` (the default) to prepare the currently supported Linux, macOS and
Windows architectures. The generated files under `src-tauri/binaries/` are
ignored build artifacts. Tauri's `bundle.externalBin` copies the matching
helper into each installer/app bundle, so it is never a separate end-user
installation.
