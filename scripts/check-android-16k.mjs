#!/usr/bin/env node
// Checks that the 64-bit native libraries in an APK or AAB work on devices
// with 16 KB memory pages: every loadable segment aligned to 16 KB or more.
// Google Play refuses apps targeting Android 15+ without it, and such a
// device can't load the library at all. (32-bit libraries aren't checked:
// 16 KB pages exist only on 64-bit devices.)
//
//   node scripts/check-android-16k.mjs <file.apk|file.aab> ...
//
// Uses `unzip` to read the libraries; exits 1 if one fails or none is found.
import { execFileSync } from "node:child_process";

const PAGE = 0x4000;
const SIXTY_FOUR = /^(?:base\/)?lib\/(arm64-v8a|x86_64)\/[^/]+\.so$/;

/** The alignments of an ELF file's PT_LOAD segments. */
export function loadAlignments(elf) {
  if (elf.length < 64 || elf.readUInt32BE(0) !== 0x7f454c46) throw new Error("not an ELF file");
  const is64 = elf[4] === 2;
  const little = elf[5] === 1;
  const u16 = (o) => (little ? elf.readUInt16LE(o) : elf.readUInt16BE(o));
  const u32 = (o) => (little ? elf.readUInt32LE(o) : elf.readUInt32BE(o));
  const u64 = (o) => Number(little ? elf.readBigUInt64LE(o) : elf.readBigUInt64BE(o));
  const phoff = is64 ? u64(32) : u32(28);
  const phentsize = u16(is64 ? 54 : 42);
  const phnum = u16(is64 ? 56 : 44);
  const out = [];
  for (let i = 0; i < phnum; i++) {
    const o = phoff + i * phentsize;
    if (u32(o) !== 1) continue; // PT_LOAD
    out.push(is64 ? u64(o + 48) : u32(o + 28));
  }
  return out;
}

const files = process.argv.slice(2);
if (files.length === 0) {
  console.error("usage: check-android-16k.mjs <file.apk|file.aab> ...");
  process.exit(2);
}
let bad = 0;
for (const file of files) {
  const names = execFileSync("unzip", ["-Z1", file], { encoding: "utf8", maxBuffer: 64 << 20 })
    .split(/\r?\n/)
    .filter((n) => SIXTY_FOUR.test(n));
  if (names.length === 0) {
    console.error(`${file}: no 64-bit native library found`);
    bad++;
    continue;
  }
  for (const name of names) {
    const elf = execFileSync("unzip", ["-p", file, name], { maxBuffer: 1 << 30 });
    const aligns = loadAlignments(elf);
    const ok = aligns.length > 0 && aligns.every((a) => a >= PAGE);
    console.log(`${ok ? "ok  " : "FAIL"} ${name}: LOAD align ${aligns.map((a) => "0x" + a.toString(16)).join(", ")}`);
    if (!ok) bad++;
  }
}
if (bad) {
  console.error(`${bad} problem(s): 64-bit libraries must be aligned to 16 KB (NDK r28 or later does it by default).`);
  process.exit(1);
}
