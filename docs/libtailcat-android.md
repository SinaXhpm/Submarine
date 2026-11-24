# Native Tailcat on Android

Run `npm run android:build:native-tailcat` on Linux or macOS to build and package the native Tailcat transport. The script pins Tailcat v0.8.0 and the Tailscale revision containing the Android cgo fallback fix, builds arm64-v8a and armeabi-v7a `libtailcat.so` libraries with 16 KB ELF page alignment, then creates signed split APKs.

Required tools are Go 1.27.4 (the Go toolchain auto-downloads it if needed), Rust 1.98.1 or later, JDK 17, Android SDK platform 36, and Android NDK r27b. Set `ANDROID_NDK_HOME` if the NDK is not under `$ANDROID_HOME/ndk/27.1.12297006`.

Generated Tailcat sources are cached under `~/.cache/submarine/libtailcat/`; native libraries and APKs remain under `src-tauri/gen/android/`. They are build outputs and are not committed. The script uses the configured release keystore when present; otherwise it uses the standard local Android debug keystore for sideloadable test APKs. The arm64 APK is written to `src-tauri/gen/android/app/build/outputs/apk/arm64/release/app-arm64-release.apk`.

Go does not support Android x86_64 for `-buildmode=c-shared`, so the native Tailcat transport is not packaged for x86_64 emulators.
