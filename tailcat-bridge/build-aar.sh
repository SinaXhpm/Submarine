#!/usr/bin/env bash
set -euo pipefail

# Requires Go 1.27.1, Android NDK and ANDROID_NDK_HOME. The output
# is deliberately ignored: CI/release builds it, verifies the JNI ABIs, then
# places it in app/libs. Keeping it out of git avoids opaque binary churn.
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
export GOTOOLCHAIN=go1.27.1
if [[ -z "${JAVA_HOME:-}" || ! -x "$JAVA_HOME/bin/javac" ]]; then
  echo "JAVA_HOME must point to a JDK (not a JRE)" >&2
  exit 1
fi
export PATH="$JAVA_HOME/bin:$PATH"
OUT="$ROOT/src-tauri/gen/android/app/libs/tailcat-bridge.aar"
mkdir -p "$(dirname "$OUT")"
go install golang.org/x/mobile/cmd/gobind
export PATH="$(go env GOPATH)/bin:$PATH"
go run golang.org/x/mobile/cmd/gomobile init
go run golang.org/x/mobile/cmd/gomobile bind \
  -target=android/arm64,android/amd64 -androidapi=24 \
  -javapkg=com.submarine.tailcatbridge -o "$OUT" .
unzip -l "$OUT" | grep -E 'jni/(arm64-v8a|x86_64)/libgojni.so' >/dev/null
