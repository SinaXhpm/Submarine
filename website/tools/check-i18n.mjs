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
if (!site || !fs.existsSync(DIR)) { console.log("usage: check-i18n.mjs <submarine|echoes|captun> <lang>..."); process.exit(2); }
const en = JSON.parse(fs.readFileSync(path.join(DIR, "en.json"), "utf8"));
// Names that appear as they are in every language. Echoes is in English only,
// so its site names its tools and buttons as the app shows them.
const KEEP = {
  submarine: ["Submarine", "SSH", "SFTP", "RDP", "VNC", "Telnet", "GitHub", "Argon2id", "AES-256-GCM", "SHA256SUMS", "MIT",
    "X-Auth-Token", "cloud_token.json", "Ctrl+Shift+F", "Ctrl+Shift+R", "DPAPI", "Windows Hello", "IronRDP", "Echoes", "TestFlight"],
  echoes: ["Echoes", "App Store", "GitHub", "MIT", "Submarine", "cURL", "Doctor", "String Lab", "TLS Inspector", "Telegram", "Cloudflare",
    "v2rayNG", "Hiddify", "Xray", "PRIVACY.md", "Port Scanner", "Check Host", "Web Server", "Start sharing"],
  // Captun is in English only too: its button and setting names stay as the app shows them.
  captun: ["Captun", "GitHub", "Echoes", "Submarine", "VLESS", "VMess", "Trojan", "Shadowsocks", "Hysteria2", "REALITY", "Xray-core",
    "SOCKS5", "WinDivert", "TUN", "libusb", "adb", "Rust", "Tauri", "Node.js", "README", "NetworkManager", "hostapd", "DHCP",
    "Update lists", "Route devices through Captun", "Android", "iOS", "Windows", "Linux", "macOS"],
}[site] || [];
// Keys whose text is a name in the app: the same in every language.
const FIXED = {
  submarine: [/\b(id|file)$/],
  captun: [/\b(id)$/],
  echoes: [/\b(id|file)$/, /tools\.groups\[\d+\]\.items\[\d+\]\.title$/, /download\.platforms\[\d+\]\.links\[\d+\]\.label$/],
}[site];
const FIXED_EXCEPT = /links\[\d+\]\.label$/;   // "Download .zip" and the like may be translated, the file names not

// Submarine's app is translated (src/i18n/<lang>.json, English text as the key), so where the English page names
// one of its buttons or settings, the translated page names it as the app does in that language. Picture
// descriptions are left out: the screenshots show the app in English. `only`: the one string the label is checked in.
const REPO = path.join(path.dirname(fileURLToPath(import.meta.url)), "..", "..");
const APP_LANG = { zh: "zh-CN", "pt-br": "pt-BR" };
const APP_LABELS = {
  submarine: [
    ["Cloud sync", (d) => d["Cloud Sync"]],
    ...["Delete account…", "Delete from cloud…", "Check for updates", "Forgot password?", "Save password", "Share the clipboard",
      "Less data", "Send feedback", "Suggest a fix"].map((k) => [k, (d) => d[k]]),
    ["<em>Feedback</em>", (d) => `<em>${d.Feedback}</em>`],
    ["Settings → Language", (d) => `${d.Settings} → ${d.Language}`],
    ["Open in: Submarine's tab", (d) => d["Open in"]],
    ["Open in: Submarine's tab", (d) => d["Submarine's tab (experimental)"].replace(/\s*[（(][^（()）]*[)）]\s*$/, "")],
    ...["Good", "Fair", "Weak", "Not answering"].map((k) => [k, (d) => d[k], /^home\.remote\.points\[0\]\.text$/]),
  ],
}[site] || [];
const NOT_LABELS = /(^|\.)(alt|altVnc|altSettings|imageAlt)$/;
const appDict = (lang) => {
  const f = path.join(REPO, "src", "i18n", `${APP_LANG[lang] || lang}.json`);
  return fs.existsSync(f) ? JSON.parse(fs.readFileSync(f, "utf8")) : null;
};
const BIDI = /[‎‏‪-‮⁦-⁩]/;

let problems = 0;
let dict = null;   // the app's translation for the language being checked
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
  const key = at.slice(lang.length + 1);
  if (dict && !NOT_LABELS.test(key)) {
    for (const [label, inApp, only] of APP_LABELS) {
      if (!a.includes(label) || (only && !only.test(key))) continue;
      const want = inApp(dict);
      if (!want) bad(lang, at, `the app has no translation of "${label}"`);
      else if (!b.includes(want)) bad(lang, at, `"${label}" is "${want}" in the app`);
    }
  }
  const fixed = FIXED.some((re) => re.test(at)) && !(FIXED_EXCEPT.test(at) && /\s/.test(a));
  if (fixed && a !== b) bad(lang, at, "must not be translated");
}

for (const lang of langs) {
  const f = path.join(DIR, `${lang}.json`);
  let raw, tr;
  try { raw = fs.readFileSync(f, "utf8"); tr = JSON.parse(raw); } catch (e) { bad(lang, "file", e.message); continue; }
  const before = problems;
  dict = APP_LABELS.length ? appDict(lang) : null;
  if (APP_LABELS.length && !dict) bad(lang, "file", `no app translation src/i18n/${APP_LANG[lang] || lang}.json to take the button names from`);
  if (BIDI.test(raw)) bad(lang, "file", "raw invisible direction marks: write them as \\u200e, \\u200f, \\u2066-\\u2069 escapes");
  walk(lang, en, tr, lang);
  if (problems === before) console.log(`${site} ${lang}: ok`);
}
process.exit(problems ? 1 : 0);
