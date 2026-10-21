// Checks a translation against the English strings: the same keys and list
// lengths, the same {placeholders} and HTML tags in every string, links to the
// same places, and the names that must stay as they are still there.
//   node website/tools/check-i18n.mjs <lang> [<lang>...]     (exit 1 on a problem)
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const DIR = path.join(path.dirname(fileURLToPath(import.meta.url)), "..", "i18n");
const en = JSON.parse(fs.readFileSync(path.join(DIR, "en.json"), "utf8"));
// Names and labels that appear in English in every language (the app's own
// buttons are English, so the site names them as the app shows them).
const KEEP = ["Submarine", "SSH", "SFTP", "RDP", "VNC", "Telnet", "GitHub", "Argon2id", "AES-256-GCM", "SHA256SUMS", "MIT",
  "Cloud sync", "Delete account…", "Delete from cloud…", "Check for updates", "Forgot password?", "X-Auth-Token", "cloud_token.json"];

let problems = 0;
const bad = (lang, where, msg) => { problems++; console.log(`${lang} ${where}: ${msg}`); };
const tokens = (s, re) => (String(s).match(re) || []).sort();
const same = (a, b) => JSON.stringify(a) === JSON.stringify(b);

function walk(lang, a, b, at) {
  if (Array.isArray(a)) {
    if (!Array.isArray(b)) return bad(lang, at, "should be a list");
    if (a.length !== b.length) bad(lang, at, `list of ${b.length}, English has ${a.length}`);
    a.forEach((x, i) => walk(lang, x, b[i], `${at}[${i}]`));
    return;
  }
  if (a && typeof a === "object") {
    if (!b || typeof b !== "object") return bad(lang, at, "should be an object");
    for (const k of Object.keys(a)) {
      if (!(k in b)) bad(lang, `${at}.${k}`, "missing");
      else walk(lang, a[k], b[k], `${at}.${k}`);
    }
    for (const k of Object.keys(b)) if (!(k in a)) bad(lang, `${at}.${k}`, "not in English");
    return;
  }
  if (typeof a === "boolean" || typeof a === "number") { if (a !== b) bad(lang, at, `should stay ${a}`); return; }
  if (typeof b !== "string") return bad(lang, at, "should be text");
  if (!same(tokens(a, /\{\w+\}/g), tokens(b, /\{\w+\}/g))) bad(lang, at, `placeholders ${tokens(b, /\{\w+\}/g).join(" ")} vs English ${tokens(a, /\{\w+\}/g).join(" ")}`);
  const tagNames = (s) => tokens(s, /<\/?[a-z]+/g);
  if (!same(tagNames(a), tagNames(b))) bad(lang, at, `HTML tags ${tagNames(b).join(" ")} vs English ${tagNames(a).join(" ")}`);
  if (!same(tokens(a, /href="[^"]*"/g), tokens(b, /href="[^"]*"/g))) bad(lang, at, "a link changed");
  if (!same(tokens(a, /class="[^"]*"/g), tokens(b, /class="[^"]*"/g))) bad(lang, at, "a class changed");
  for (const k of KEEP) if (a.includes(k) && !b.includes(k)) bad(lang, at, `"${k}" must stay as it is`);
  if (/\b(id|file)$/.test(at)) { if (a !== b) bad(lang, at, "must not be translated"); }
}

for (const lang of process.argv.slice(2)) {
  const f = path.join(DIR, `${lang}.json`);
  let tr;
  try { tr = JSON.parse(fs.readFileSync(f, "utf8")); } catch (e) { bad(lang, "file", e.message); continue; }
  const before = problems;
  walk(lang, en, tr, lang);
  if (problems === before) console.log(`${lang}: ok`);
}
process.exit(problems ? 1 : 0);
