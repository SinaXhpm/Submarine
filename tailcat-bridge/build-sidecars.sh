#!/usr/bin/env bash
# Build the bundled desktop Tailcat helpers. Run this before `tauri build` for
# each release target. They are application sidecars, never a user dependency.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT="$ROOT/src-tauri/binaries"
GO_BIN="${GO_BIN:-go}"

mkdir -p "$OUT"
cd "$ROOT/tailcat-bridge"

build() {
  local goos="$1" goarch="$2" triple="$3" suffix="${4:-}"
  echo "Building Tailcat sidecar for $triple"
  CGO_ENABLED=0 GOOS="$goos" GOARCH="$goarch" "$GO_BIN" build -trimpath -buildvcs=false \
    -o "$OUT/tailcat-bridge-$triple$suffix" ./cmd/tailcat-bridge-sidecar
}

case "${1:-all}" in
  all)
    build linux amd64 x86_64-unknown-linux-gnu
    build linux arm64 aarch64-unknown-linux-gnu
    build darwin amd64 x86_64-apple-darwin
    build darwin arm64 aarch64-apple-darwin
    build windows amd64 x86_64-pc-windows-msvc .exe
    build windows arm64 aarch64-pc-windows-msvc .exe
    ;;
  x86_64-unknown-linux-gnu) build linux amd64 "$1" ;;
  aarch64-unknown-linux-gnu) build linux arm64 "$1" ;;
  x86_64-apple-darwin) build darwin amd64 "$1" ;;
  aarch64-apple-darwin) build darwin arm64 "$1" ;;
  x86_64-pc-windows-msvc) build windows amd64 "$1" .exe ;;
  aarch64-pc-windows-msvc) build windows arm64 "$1" .exe ;;
  *) echo "unsupported Tauri target: $1" >&2; exit 2 ;;
esac
