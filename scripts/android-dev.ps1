# Robust `tauri android dev` for USB-connected phones, regardless of
# whether Wi-Fi is on or off.
#
# Why this script exists:
#   `tauri android dev` runs Vite on the host and points the device's
#   WebView at it. Two ways the WebView can reach Vite:
#
#     (A) LAN IP — phone and dev machine on the same Wi-Fi, phone hits
#                  http://<host-LAN-IP>:1420 directly. Breaks the moment
#                  Wi-Fi flaps, picks a different SSID, or the host's
#                  firewall blocks inbound 1420.
#
#     (B) USB loopback — `adb reverse tcp:1420 tcp:1420` forwards the
#                        phone's 127.0.0.1:1420 to the host's localhost
#                        binding, plus `--host 127.0.0.1` forces Tauri to
#                        bake 127.0.0.1 into the WebView's devUrl. This
#                        path works whether Wi-Fi is on, off, or you're
#                        on cellular — the only requirement is a working
#                        USB debug session.
#
# We use path (B). It's the most reliable for actual development.
# `adb reverse` is per-session (re-plug, reboot, adb kill-server all
# clear it), so we re-apply it here on every launch instead of asking
# you to remember.

$ErrorActionPreference = 'Stop'

# Bail loudly if adb isn't on PATH. Most commonly the user forgot to
# `. .\scripts\android-env.ps1` in this shell.
$adb = Get-Command adb -ErrorAction SilentlyContinue
if (-not $adb) {
    Write-Error 'adb not found on PATH — run `. .\scripts\android-env.ps1` first.'
    exit 1
}

# Surface the connected device(s) so it's obvious when the phone isn't
# actually attached (a frequent silent failure mode).
$devices = & adb devices | Select-String "device$" | ForEach-Object { ($_ -split "\s+")[0] }
if (-not $devices -or $devices.Count -eq 0) {
    Write-Error 'No device shown by `adb devices`. Re-plug USB, accept the debug prompt on the phone, and retry.'
    exit 1
}
Write-Output "Connected device(s): $($devices -join ', ')"

# 1420 = Vite dev server, 1421 = Vite HMR WebSocket. Forward both so
# hot-module reload also works through the loopback path.
Write-Output 'Setting up adb reverse 1420 + 1421...'
& adb reverse tcp:1420 tcp:1420 | Out-Null
& adb reverse tcp:1421 tcp:1421 | Out-Null
& adb reverse --list

Write-Output 'Starting tauri android dev (--host 127.0.0.1)...'
# Force Tauri to use 127.0.0.1 in the WebView's devUrl, matching what we
# just made reachable via adb reverse. Without this flag, Tauri picks the
# host's first LAN IP (or `0.0.0.0`), which usually fails on the device.
& npx tauri android dev --host 127.0.0.1 $args
