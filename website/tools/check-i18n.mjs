// Checks a site's translation against its English strings: the same keys and
// list lengths, the same {placeholders} and HTML tags in every string, links to
// the same places, the same text inside <code>, the names that must stay as they are still there, and no
// raw invisible direction marks (they must be written as \u escapes).
//   node website/tools/check-i18n.mjs <site> <lang> [<lang>...]     (exit 1 on a problem)
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const [site, ...langs] = process.argv.slice(2);
const DIR = path.join(path.dirname(fileURLToPath(import.meta.url)), "..", "sites", site || "", "i18n");
if (!site || !fs.existsSync(DIR)) { console.log("usage: check-i18n.mjs <submarine|echoes> <lang>..."); process.exit(2); }
const en = JSON.parse(fs.readFileSync(path.join(DIR, "en.json"), "utf8"));
// Names and labels that appear in English in every language. Both apps are in
// English, so the sites name their buttons and tools as the apps show them.
const KEEP = {
  submarine: ["Submarine", "SSH", "SFTP", "RDP", "VNC", "Telnet", "GitHub", "Argon2id", "AES-256-GCM", "SHA256SUMS", "MIT",
    "Cloud sync", "Delete account…", "Delete from cloud…", "Check for updates", "Forgot password?", "X-Auth-Token", "cloud_token.json",
    "Save password", "Share the clipboard", "Open in: Submarine's tab", "Less data", "Ctrl+Shift+F", "Ctrl+Shift+R", "DPAPI", "Windows Hello", "IronRDP", "Echoes"],
  echoes: ["Echoes", "App Store", "GitHub", "MIT", "Submarine", "cURL", "Doctor", "String Lab", "TLS Inspector", "Telegram", "Cloudflare",
    "v2rayNG", "Hiddify", "Xray", "PRIVACY.md"],
}[site] || [];
// Keys whose text is a name in the app: the same in every language.
const FIXED = {
  submarine: [/\b(id|file)$/],
  echoes: [/\b(id|file)$/, /tools\.groups\[\d+\]\.items\[\d+\]\.title$/, /download\.platforms\[\d+\]\.links\[\d+\]\.label$/],
}[site];
const FIXED_EXCEPT = /links\[\d+\]\.label$/;   // "Download .zip" and the like may be translated, the file names not
const BIDI = /[‎‏‪-‮⁦-⁩]/;

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
  const codes = (s) => tokens(s, /<code[^>]*>[\s\S]*?<\/code>/g);
  if (!same(codes(a), codes(b))) bad(lang, at, "the text inside <code> changed");
  for (const k of KEEP) if (a.includes(k) && !b.includes(k)) bad(lang, at, `"${k}" must stay as it is`);
  const fixed = FIXED.some((re) => re.test(at)) && !(FIXED_EXCEPT.test(at) && /\s/.test(a));
  if (fixed && a !== b) bad(lang, at, "must not be translated");
}

for (const lang of langs) {
  const f = path.join(DIR, `${lang}.json`);
  let raw, tr;
  try { raw = fs.readFileSync(f, "utf8"); tr = JSON.parse(raw); } catch (e) { bad(lang, "file", e.message); continue; }
  const before = problems;
  if (BIDI.test(raw)) bad(lang, "file", "raw invisible direction marks: write them as \\u200e, \\u200f, \\u2066-\\u2069 escapes");
  walk(lang, en, tr, lang);
  if (problems === before) console.log(`${site} ${lang}: ok`);
}
process.exit(problems ? 1 : 0);
