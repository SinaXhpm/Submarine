#!/usr/bin/env node
// The Android version code for a release version, so every upload to Google
// Play has a new, higher number, and so do the APKs on GitHub, which share it
// (a Play copy and a GitHub copy can then update each other).
//
//   node scripts/android-version-code.mjs 0.5.0-beta.3   -> 50003
//   node scripts/android-version-code.mjs 0.5.0          -> 50099
//   node scripts/android-version-code.mjs 0.5.0 --stamp src-tauri/tauri.conf.json
//
// major * 1,000,000 + minor * 10,000 + patch * 100 + n, where n is the number
// at the end of a pre-release ("beta.3" is 3; one with no number is 1) and 99
// for a final release, so 0.5.0-beta.3 < 0.5.0-rc.4 < 0.5.0 < 0.5.1-beta.1.
// Minor and patch go up to 99, the pre-release number up to 98.
//
// Tauri's own number (major * 1,000,000 + minor * 1,000 + patch) gives a beta
// and its final release the same code, which Play refuses; 0.5.0-beta.2, the
// last APK made that way, has 5000, below every code made here.
//
// --stamp writes it into a Tauri config as bundle.android.versionCode.
import { readFileSync, writeFileSync } from "node:fs";
import { pathToFileURL } from "node:url";

export function versionCode(version) {
  const m = /^v?(\d+)\.(\d+)\.(\d+)(?:-([0-9A-Za-z.-]+))?(?:\+[0-9A-Za-z.-]+)?$/.exec(version);
  if (!m) throw new Error(`not a version: ${version}`);
  const [major, minor, patch] = [m[1], m[2], m[3]].map(Number);
  const pre = m[4];
  if (minor > 99 || patch > 99) throw new Error(`minor and patch must be at most 99: ${version}`);
  let n = 99;
  if (pre !== undefined) {
    const tail = /(\d+)$/.exec(pre);
    n = tail ? Number(tail[1]) : 1;
    if (n < 1 || n > 98) throw new Error(`the pre-release number must be 1 to 98: ${version}`);
  }
  const code = major * 1_000_000 + minor * 10_000 + patch * 100 + n;
  // Google Play's ceiling.
  if (code > 2_100_000_000) throw new Error(`version code too large: ${code}`);
  return code;
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  const [version, flag, file] = process.argv.slice(2);
  if (!version || (flag && (flag !== "--stamp" || !file))) {
    console.error("usage: android-version-code.mjs <version> [--stamp <tauri config>]");
    process.exit(2);
  }
  let code;
  try {
    code = versionCode(version);
  } catch (e) {
    console.error(`android-version-code: ${e.message}`);
    process.exit(1);
  }
  if (flag === "--stamp") {
    const conf = JSON.parse(readFileSync(file, "utf8"));
    conf.bundle ??= {};
    conf.bundle.android ??= {};
    conf.bundle.android.versionCode = code;
    writeFileSync(file, JSON.stringify(conf, null, 2) + "\n");
  }
  console.log(code);
}
