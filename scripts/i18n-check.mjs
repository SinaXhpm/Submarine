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
const NON_LITERAL = /\b(t|tx)\(\s*(?![\s"'`])[^)]/g;
const NON_LITERAL_N = /\btn\(\s*[^,]+?,\s*(?![\s"'`])[^)]/g;

const keys = new Map(); // text → first place
const plurals = new Map(); // tn()'s second form → its first form
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
    const one = litValue(m, 1), other = litValue(m, 4);
    for (const key of [one, other]) {
      if (key && !keys.has(key)) keys.set(key, `${rel}:${lineOf(text, m.index)}`);
    }
    if (other) plurals.set(other, one);
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

// Plural lines: "<second form>|few" and the like, for a language whose rules
// have more forms than English (tn() in src/i18n/index.ts). A language with a
// "two" or "few" form needs that line for every pair; "zero", "one" and
// "many" lines are optional (without one, the nearer English form is used).
const PLURAL_LINE = /^([\s\S]*)\|(zero|one|two|few|many)$/;
const NEEDED_FORMS = ["two", "few"];

let failed = false;
console.log(`${sorted.length} texts in src/ (src/i18n/en.json written)`);
if (dynamic.length) console.log(`${dynamic.length} calls with a non-literal text (fine for a tk() table):\n  ${dynamic.join("\n  ")}`);

const list = (items) => items.map((k) => JSON.stringify(k)).join("\n    ");

for (const name of readdirSync(i18nDir).filter((n) => n.endsWith(".json") && n !== "en.json").sort()) {
  const dict = JSON.parse(readFileSync(join(i18nDir, name), "utf8"));
  const tag = name.replace(/\.json$/, "");
  let forms;
  try { forms = new Intl.PluralRules(tag).resolvedOptions().pluralCategories; } catch { forms = ["one", "other"]; }
  // The English second form a plural line belongs to, or null.
  const pluralOf = (k) => {
    const m = PLURAL_LINE.exec(k);
    return m && plurals.has(m[1]) && forms.includes(m[2]) ? m[1] : null;
  };
  const missing = sorted.filter((k) => !(k in dict) || dict[k] === "");
  const missingForms = [];
  for (const other of plurals.keys()) {
    for (const f of NEEDED_FORMS) if (forms.includes(f) && !dict[`${other}|${f}`]) missingForms.push(`${other}|${f}`);
  }
  const unused = Object.keys(dict).filter((k) => !keys.has(k) && !pluralOf(k));
  const mismatched = Object.keys(dict).filter((k) => {
    if (dict[k] === "") return false;
    const base = keys.has(k) ? k : pluralOf(k);
    return base !== null && placeholders(base) !== placeholders(dict[k]);
  });
  const extra = Object.keys(dict).filter((k) => !keys.has(k) && pluralOf(k)).length;
  console.log(`\n${name}: ${sorted.length - missing.length}/${sorted.length} translated${extra ? `, ${extra} plural lines` : ""}`);
  if (missing.length) {
    console.log(`  ${missing.length} missing:\n    ${list(missing)}`);
    if (strict) failed = true;
  }
  if (missingForms.length) {
    console.log(`  ${missingForms.length} plural lines missing (${tag} has the forms ${forms.join(", ")}):\n    ${list(missingForms)}`);
    if (strict) failed = true;
  }
  if (unused.length) {
    console.log(`  ${unused.length} no longer used (remove them):\n    ${list(unused)}`);
    failed = true;
  }
  if (mismatched.length) {
    console.log(`  ${mismatched.length} with different placeholders:\n    ${list(mismatched)}`);
    failed = true;
  }
}

process.exit(failed ? 1 : 0);
