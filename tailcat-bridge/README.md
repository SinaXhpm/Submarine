# Submarine Tailcat bridge

This Go Mobile package is the Android-only native transport adapter. It turns
Tailcat TCP streams into loopback TCP listeners; it never creates a TUN device
or asks Android for VPN permission. `build-aar.sh` pins both Tailcat (go.mod)
and gomobile, and emits arm64-v8a plus x86_64 JNI libraries.

The generated AAR is intentionally not versioned. Build it before Android
packaging and copy it to `src-tauri/gen/android/app/libs/tailcat-bridge.aar`.
