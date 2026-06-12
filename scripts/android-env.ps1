# Source this file before running Android builds:
#     . .\scripts\android-env.ps1
#     npm run android:init    # one-time scaffold
#     npm run android:dev     # build + install on connected phone
#
# All paths are hard-coded to this machine; edit if you move the SDK/NDK.

$SDK = 'G:\0098\phack\Main\atich\behzadpax-mix\sdk'

# Auto-pick the NEWEST NDK directory under $SDK\ndk. NDK 25/26/27 are all
# supported by Tauri 2.1 + cargo-ndk; we just want the latest one you have
# so cross-compile uses the freshest Clang. Override by setting NDK_VERSION
# before sourcing this script if you need to pin (e.g. CI reproducibility).
if (-not $NDK_VERSION) {
    $latest = Get-ChildItem (Join-Path $SDK 'ndk') -Directory -ErrorAction SilentlyContinue |
              Sort-Object { [version]($_.Name -replace '^(\d+)\.(\d+)\.(\d+).*','$1.$2.$3') } -Descending |
              Select-Object -First 1
    if ($latest) { $NDK_VERSION = $latest.Name }
}

# JAVA — Tauri 2 + Android Gradle Plugin 8 expects JDK 17. Prefer an
# explicit JDK 17 install over Android Studio's bundled JDK 11.
$jdk17 = 'C:\Program Files\Java\jdk-17'
if (Test-Path $jdk17) {
    $env:JAVA_HOME = $jdk17
} elseif (-not $env:JAVA_HOME -or -not (Test-Path $env:JAVA_HOME)) {
    $env:JAVA_HOME = 'C:\Program Files\Android\Android Studio\jre'
}

$env:ANDROID_HOME      = $SDK
$env:ANDROID_SDK_ROOT  = $SDK
$env:NDK_HOME          = Join-Path $SDK "ndk\$NDK_VERSION"
$env:ANDROID_NDK_HOME  = $env:NDK_HOME
$env:ANDROID_NDK_ROOT  = $env:NDK_HOME

# Make adb / java / cargo-ndk reachable from the shell.
$prepend = @(
    (Join-Path $SDK 'platform-tools'),
    (Join-Path $env:JAVA_HOME 'bin')
) -join ';'
if (-not ($env:PATH -like "*$prepend*")) {
    $env:PATH = "$prepend;$env:PATH"
}

Write-Output "Android env loaded for this PowerShell session:"
Write-Output "  JAVA_HOME      = $env:JAVA_HOME"
Write-Output "  ANDROID_HOME   = $env:ANDROID_HOME"
Write-Output "  NDK_HOME       = $env:NDK_HOME"
Write-Output ""
Write-Output "  java -version  -> $(& "$env:JAVA_HOME\bin\java.exe" -version 2>&1 | Select-Object -First 1)"
Write-Output ""
Write-Output "Connected devices:"
& "$SDK\platform-tools\adb.exe" devices
