#!/usr/bin/env node
// Lists every text the app translates and checks the dictionaries against it.
//
//   node scripts/i18n-check.mjs            report; writes src/i18n/en.json
//   node scripts/i18n-check.mjs --strict   also fail on an untranslated text
//
// The texts are the string literals passed to t(), tk(), tx() and tn() in
// src/ (see src/i18n/index.ts). en.json lists them all, English to English,
// as the template for a new language. For every other src/i18n/<id>.json it
// reports texts that are missing, lines that no code uses any more, and
// placeholders ({name}) that differ between the English and the translation.
// A t() whose first argument isn't a literal is reported too: such a text
// can't be listed, so it should come from a table of tk() keys instead.
import { readdirSync, readFileSync, statSync, writeFileSync } from "node:fs";
import { join, relative } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(fileURLToPath(import.meta.url), "..", "..");
const srcDir = join(root, "src");
const i18nDir = join(srcDir, "i18n");
const strict = process.argv.includes("--strict");

function* walk(dir) {
  for (const name of readdirSync(dir)) {
    const p = join(dir, name);
    if (statSync(p).isDirectory()) { if (p !== i18nDir) yield* walk(p); }
    else if (/\.(ts|tsx)$/.test(name) && !/\.test\.tsx?$/.test(name)) yield p;
  }
}

// One string literal: "…", '…' or `…` without ${}.
const LIT = String.raw`(?:"((?:\\.|[^"\\])*)"|'((?:\\.|[^'\\])*)'|` + "`((?:\\\\.|[^`\\\\$])*)`)";
const unescape = (s) => s.replace(/\\(.)/g, (_, c) => (c === "n" ? "\n" : c));
const litValue = (m, i) => unescape(m[i] ?? m[i + 1] ?? m[i + 2] ?? "");

// t("…"), tk("…"), tx("…")  and  tn(expr, "…", "…")
const CALL = new RegExp(String.raw`\b(t|tk|tx)\(\s*` + LIT, "g");
const CALL_N = new RegExp(String.raw`\btn\(\s*[^,]+?,\s*` + LIT + String.raw`\s*,\s*` + LIT, "g");
// A t()/tx()/tn() whose first text argument isn't a literal.
const NON_LITERAL = /\b(t|tx)\(\s*(?!["'`])[^)]/g;
const NON_LITERAL_N = /\btn\(\s*[^,]+?,\s*(?!["'`])[^)]/g;

const keys = new Map(); // text → first place
const dynamic = [];
const lineOf = (text, index) => text.slice(0, index).split("\n").length;

for (const file of walk(srcDir)) {
  const text = readFileSync(file, "utf8");
  const rel = relative(root, file).replace(/\\/g, "/");
  for (const m of text.matchAll(CALL)) {
    const key = litValue(m, 2);
    if (key && !keys.has(key)) keys.set(key, `${rel}:${lineOf(text, m.index)}`);
  }
  for (const m of text.matchAll(CALL_N)) {
    for (const i of [2, 5]) {
      const key = litValue(m, i);
      if (key && !keys.has(key)) keys.set(key, `${rel}:${lineOf(text, m.index)}`);
    }
  }
  for (const re of [NON_LITERAL, NON_LITERAL_N]) {
    for (const m of text.matchAll(re)) {
      // `t(key)` where key comes from a tk() table is the one allowed shape;
      // it can't be told from a mistake here, so it is listed, not failed.
      dynamic.push(`${rel}:${lineOf(text, m.index)}`);
    }
  }
}

const sorted = [...keys.keys()].sort((a, b) => a.localeCompare(b, "en"));
const en = Object.fromEntries(sorted.map((k) => [k, k.replace(/\|[a-z][a-z ]*$/, "")]));
writeFileSync(join(i18nDir, "en.json"), JSON.stringify(en, null, 2) + "\n");

const placeholders = (s) => [...s.matchAll(/\{(\w+)\}/g)].map((m) => m[1]).sort().join(",");

let failed = false;
console.log(`${sorted.length} texts in src/ (src/i18n/en.json written)`);
if (dynamic.length) console.log(`${dynamic.length} calls with a non-literal text (fine for a tk() table):\n  ${dynamic.join("\n  ")}`);

for (const name of readdirSync(i18nDir).filter((n) => n.endsWith(".json") && n !== "en.json").sort()) {
  const dict = JSON.parse(readFileSync(join(i18nDir, name), "utf8"));
  const missing = sorted.filter((k) => !(k in dict) || dict[k] === "");
  const unused = Object.keys(dict).filter((k) => !keys.has(k));
  const mismatched = sorted.filter((k) => k in dict && dict[k] !== "" && placeholders(k) !== placeholders(dict[k]));
  const done = sorted.length - missing.length;
  console.log(`\n${name}: ${done}/${sorted.length} translated`);
  if (missing.length) {
    console.log(`  ${missing.length} missing:\n    ${missing.map((k) => JSON.stringify(k)).join("\n    ")}`);
    if (strict) failed = true;
  }
  if (unused.length) {
    console.log(`  ${unused.length} no longer used (remove them):\n    ${unused.map((k) => JSON.stringify(k)).join("\n    ")}`);
    failed = true;
  }
  if (mismatched.length) {
    console.log(`  ${mismatched.length} with different placeholders:\n    ${mismatched.map((k) => JSON.stringify(k)).join("\n    ")}`);
    failed = true;
  }
}

process.exit(failed ? 1 : 0);
