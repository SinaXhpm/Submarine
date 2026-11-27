#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
if ! command -v go >/dev/null; then
  export PATH="$HOME/.local/toolchains/go/bin:$PATH"
fi
command -v go >/dev/null || { echo "Go 1.27.4 is required" >&2; exit 1; }
ndk="${ANDROID_NDK_HOME:-${ANDROID_NDK_ROOT:-${ANDROID_HOME:-$HOME/Android/Sdk}/ndk/27.1.12297006}}"
host_tag="linux-x86_64"
[[ "$(uname -s)" == Darwin ]] && host_tag="darwin-x86_64"
toolchain="$ndk/toolchains/llvm/prebuilt/$host_tag/bin"
[[ -x "$toolchain/clang" ]] || { echo "Android NDK r27b not found at $ndk" >&2; exit 1; }
command -v git >/dev/null && command -v npm >/dev/null || { echo "git and npm are required" >&2; exit 1; }

tailcat_revision="b9491c13c63925af4119aca70bd21b1691ff3d5b"
tailscale_version="v1.105.0-pre.0.20261122230636-678b3be66bd7"
native_build_id="$tailcat_revision-$tailscale_version"
cache_root="${XDG_CACHE_HOME:-$HOME/.cache}/submarine/libtailcat/$tailcat_revision"
source_dir="$cache_root/source"
libs_dir="$repo_root/src-tauri/gen/android/app/libs/libtailcat/$native_build_id"
jni_dir="$repo_root/src-tauri/gen/android/app/src/main/jniLibs"
mkdir -p "$cache_root" "$libs_dir" "$jni_dir"

if [[ ! -d "$source_dir/.git" ]]; then
  git clone --filter=blob:none https://github.com/tailscale/tailcat.git "$source_dir"
fi
git -C "$source_dir" fetch --depth 1 origin "$tailcat_revision"
git -C "$source_dir" checkout --detach "$tailcat_revision"
git -C "$source_dir" reset --hard "$tailcat_revision"

export GOTOOLCHAIN=go1.27.4
# The checked-in Cargo.lock currently resolves dependencies requiring 1.94+.
export RUSTUP_TOOLCHAIN="${RUSTUP_TOOLCHAIN:-1.98.1}"
export ANDROID_HOME="${ANDROID_HOME:-$HOME/Android/Sdk}"
export ANDROID_NDK_HOME="$ndk"
export ANDROID_NDK_ROOT="$ndk"
# Use the standard local Android debug key for a sideloadable test APK when no
# project or CI release-signing configuration was supplied.
if [[ -z "${SUBMARINE_KEYSTORE_PATH:-}" && ! -f "$repo_root/src-tauri/gen/android/keystore.properties" ]]; then
  export SUBMARINE_KEYSTORE_PATH="$HOME/.android/debug.keystore"
  export SUBMARINE_KEYSTORE_PASSWORD=android
  export SUBMARINE_KEY_ALIAS=androiddebugkey
  export SUBMARINE_KEY_PASSWORD=android
fi
if [[ ! -f "$repo_root/src-tauri/gen/android/keystore.properties" && ( -z "${SUBMARINE_KEYSTORE_PATH:-}" || ! -f "$SUBMARINE_KEYSTORE_PATH" ) ]]; then
  echo "No Android signing key is configured; install the standard debug keystore or configure release signing." >&2
  exit 1
fi
go -C "$source_dir" mod edit -require="tailscale.com@$tailscale_version"
go -C "$source_dir" mod tidy
tags="$(cat "$source_dir/build-tags.txt")"

build_one() {
  local abi="$1" goarch="$2" target="$3" cc="$4"
  local out="$libs_dir/$target"
  mkdir -p "$out" "$jni_dir/$abi"
  if [[ ! -s "$out/libtailcat.so" ]]; then
    env CGO_ENABLED=1 GOOS=android GOARCH="$goarch" GOARM="${5:-}" CC="$toolchain/$cc" \
      go -C "$source_dir" build -tags "$tags" -buildmode=c-shared -trimpath \
        -ldflags='-s -w -extldflags=-Wl,-z,max-page-size=16384' \
        -o "$out/libtailcat.so" ./cmd/libtailcat
  fi
  install -m 0644 "$out/libtailcat.so" "$jni_dir/$abi/libtailcat.so"
}

build_one arm64-v8a arm64 aarch64-linux-android aarch64-linux-android24-clang
build_one armeabi-v7a arm armv7-linux-androideabi armv7a-linux-androideabi24-clang 7

export SUBMARINE_LIBTAILCAT_DIR="$libs_dir"
cd "$repo_root"
npm run tauri -- android build --apk --split-per-abi --target aarch64 armv7 --features tailcat-capi
