// Builds the Submarine website (submarine.sinaxhpm.com) into website/dist/:
// one static page per language and page, from the strings in i18n/<lang>.json
// (English is the source; a missing string falls back to it, with a warning).
// No dependencies beyond Node 20+ and git.
//
//   node website/build.mjs [--out website/dist] [--commit <sha>] [--date 2026-10-21]
//
// --commit is the main-branch commit the security page's code links point to
// (default: origin/main). It must exist on GitHub.
import { execFileSync } from "node:child_process";
import crypto from "node:crypto";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const REPO = path.resolve(HERE, "..");
const arg = (name, fallback) => {
  const i = process.argv.indexOf(`--${name}`);
  return i > 0 ? process.argv[i + 1] : fallback;
};
const OUT = path.resolve(arg("out", path.join(HERE, "dist")));
const COMMIT = arg("commit", execFileSync("git", ["-C", REPO, "rev-parse", "origin/main"]).toString().trim());
const DATE = arg("date", new Date().toISOString().slice(0, 10));
const POLICY_DATE = "2026-10-21";

const SITE = "https://submarine.sinaxhpm.com";
const REPO_URL = "https://github.com/SinaXhpm/Submarine";
const RELEASES_URL = `${REPO_URL}/releases`;
const ISSUES_URL = `${REPO_URL}/issues`;

// ---- languages ----------------------------------------------------------
const LANGS = [
  { code: "en", dir: "ltr", html: "en", hreflang: "en", locale: "en", og: "en_US", name: "English" },
  { code: "fa", dir: "rtl", html: "fa", hreflang: "fa", locale: "fa", og: "fa_IR", name: "فارسی" },
  { code: "ar", dir: "rtl", html: "ar", hreflang: "ar", locale: "ar-u-nu-latn", og: "ar_AR", name: "العربية" },
  { code: "zh", dir: "ltr", html: "zh-Hans", hreflang: "zh-Hans", locale: "zh-Hans", og: "zh_CN", name: "简体中文" },
  { code: "es", dir: "ltr", html: "es", hreflang: "es", locale: "es", og: "es_ES", name: "Español" },
  { code: "fr", dir: "ltr", html: "fr", hreflang: "fr", locale: "fr", og: "fr_FR", name: "Français" },
  { code: "de", dir: "ltr", html: "de", hreflang: "de", locale: "de", og: "de_DE", name: "Deutsch" },
  { code: "ru", dir: "ltr", html: "ru", hreflang: "ru", locale: "ru", og: "ru_RU", name: "Русский" },
  { code: "pt-br", dir: "ltr", html: "pt-BR", hreflang: "pt-BR", locale: "pt-BR", og: "pt_BR", name: "Português (Brasil)" },
  { code: "ja", dir: "ltr", html: "ja", hreflang: "ja", locale: "ja", og: "ja_JP", name: "日本語" },
  { code: "tr", dir: "ltr", html: "tr", hreflang: "tr", locale: "tr", og: "tr_TR", name: "Türkçe" },
  { code: "hi", dir: "ltr", html: "hi", hreflang: "hi", locale: "hi", og: "hi_IN", name: "हिन्दी" },
];
const PAGES = ["home", "security", "privacy", "delete"];
const SLUG = { home: "", security: "security/", privacy: "privacy/", delete: "delete-account/" };
const pagePath = (lang, page) => `/${lang.code === "en" ? "" : lang.code + "/"}${SLUG[page]}`;
const abs = (p) => SITE + p;

// ---- facts (one place; the pages only refer to them) --------------------
// Checked against the code at COMMIT: Argon2 settings (src-tauri/src/lib.rs
// ARGON2_*), the hosted sync limits and retention (the server's sync.php,
// config and cron), the APK certificate (the release workflow's signing key).
const FACTS = {
  argonMemMiB: 64,
  argonPasses: 3,
  argonLanes: 4,
  limitMb: 5,
  limitItems: 20000,
  limitProfiles: 100,
  sessionDays: 90,
  auditDays: 90,
  rateDays: 7,
  unverifiedDays: 7,
  lockoutTries: 5,
  lockoutMinutes: 15,
  certSha256: "e9e350dc9f8628bee461be3424c0c2b2180f00685cc380de80c86808ae669f1c",
};

// The security page's links into the code: where each piece starts, resolved
// to line ranges at COMMIT so the links never drift.
const CODE = {
  deriveKey: { file: "src-tauri/src/lib.rs", from: /^const ARGON2_M_COST\b/, through: /^fn derive_key\(/ },
  encrypt: { file: "src-tauri/src/lib.rs", from: /^fn encrypt_with_key\(/ },
  saveVault: { file: "src-tauri/src/lib.rs", from: /^fn save_vault_blocking\(/ },
  encryptEntity: { file: "src-tauri/src/lib.rs", from: /^fn encrypt_entity\(/ },
  collectRecords: { file: "src-tauri/src/lib.rs", from: /^fn collect_local_records\(/ },
  escrowBuild: { file: "src-tauri/src/lib.rs", from: /^fn build_pw_escrow_record\(/ },
  escrowOpen: { file: "src-tauri/src/lib.rs", from: /^fn open_pw_escrow\(/ },
  loginRequest: { file: "src-tauri/src/cloud.rs", from: /^pub struct LoginRequest\b/ },
  sealTo: { file: "src-tauri/src/identity.rs", from: /^pub fn seal_to\(/ },
  invite: { file: "src-tauri/src/lib.rs", from: /^async fn invite_to_share\(/ },
  updateCheck: { file: "src-tauri/src/about.rs", from: /^pub async fn check_for_updates\(/ },
};

function resolveCode() {
  const cache = new Map();
  const lines = (file) => {
    if (!cache.has(file)) cache.set(file, execFileSync("git", ["-C", REPO, "show", `${COMMIT}:${file}`], { maxBuffer: 64 << 20 }).toString().split("\n"));
    return cache.get(file);
  };
  // From the start line to where its block (the first `{` on, or the `through`
  // item's block) closes, counting braces outside strings well enough for this code.
  const blockEnd = (ls, start) => {
    let depth = 0, seen = false;
    for (let i = start; i < ls.length; i++) {
      const l = ls[i].replace(/"(?:\\.|[^"\\])*"/g, '""').replace(/'(?:\\.|[^'\\])'/g, "''").replace(/\/\/.*$/, "");
      for (const ch of l) {
        if (ch === "{") { depth++; seen = true; }
        else if (ch === "}") depth--;
      }
      if (seen && depth === 0) return i;
      if (!seen && /;\s*$/.test(l) && i > start + 40) return i;
    }
    throw new Error("no block end");
  };
  const out = {};
  for (const [key, c] of Object.entries(CODE)) {
    const ls = lines(c.file);
    const a = ls.findIndex((l) => c.from.test(l));
    if (a < 0) throw new Error(`code anchor not found: ${key} in ${c.file} at ${COMMIT}`);
    const b0 = c.through ? ls.findIndex((l, i) => i > a && c.through.test(l)) : a;
    if (b0 < 0) throw new Error(`code anchor 'through' not found: ${key}`);
    const b = blockEnd(ls, b0);
    if (b - a > 160) throw new Error(`code range too long for ${key}: ${a + 1}-${b + 1}`);
    out[key] = { file: c.file, from: a + 1, to: b + 1, url: `${REPO_URL}/blob/${COMMIT}/${c.file}#L${a + 1}-L${b + 1}` };
  }
  return out;
}

// ---- strings -------------------------------------------------------------
const readJson = (f) => JSON.parse(fs.readFileSync(f, "utf8"));
const EN = readJson(path.join(HERE, "i18n", "en.json"));
const warnings = [];
// The language's strings, English where it has none; arrays are taken whole
// (a translated list replaces the English one).
function mergeStrings(en, tr, at = "") {
  if (tr === undefined) { if (at) warnings.push(`missing ${at}`); return en; }
  if (Array.isArray(en)) {
    if (!Array.isArray(tr)) { warnings.push(`not a list: ${at}`); return en; }
    if (tr.length !== en.length) warnings.push(`list length ${tr.length} vs ${en.length}: ${at}`);
    return tr.map((v, i) => (en[i] !== undefined && typeof en[i] === "object" ? mergeStrings(en[i], v, `${at}[${i}]`) : v));
  }
  if (en && typeof en === "object") {
    const o = {};
    for (const k of Object.keys(en)) o[k] = mergeStrings(en[k], tr ? tr[k] : undefined, at ? `${at}.${k}` : k);
    return o;
  }
  return tr;
}
const STR = {};
for (const lang of LANGS) {
  const f = path.join(HERE, "i18n", `${lang.code}.json`);
  if (lang.code === "en") STR.en = EN;
  else if (fs.existsSync(f)) {
    const before = warnings.length;
    STR[lang.code] = mergeStrings(EN, readJson(f));
    for (let i = before; i < warnings.length; i++) warnings[i] = `${lang.code}: ${warnings[i]}`;
  }
}
const BUILT = LANGS.filter((l) => STR[l.code]);

// ---- text helpers ---------------------------------------------------------
const esc = (s) => String(s).replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;");
const ALLOWED = /^<\/?(a|strong|em|code|br)(\s[^<>]*)?\/?>$/;
const stripTags = (s) => String(s).replace(/<[^>]+>/g, "");
// Fills {name} from vals; every placeholder must be known.
function fill(s, vals, where) {
  return String(s).replace(/\{(\w+)\}/g, (m, k) => {
    if (!(k in vals)) throw new Error(`unknown placeholder {${k}} in ${where}`);
    return vals[k];
  });
}
// A string that may carry the allowed inline tags (a, strong, em, code, br).
function rich(s, vals, where) {
  const out = fill(s, vals, where);
  for (const tag of out.match(/<[^>]*>/g) || []) {
    if (!ALLOWED.test(tag)) throw new Error(`tag not allowed in ${where}: ${tag}`);
    if (/^<a\s/.test(tag) && !/^<a href="(https:\/\/[^"]+|\/[^"]*|#[^"]*)"( rel="noopener")?>$/.test(tag)) throw new Error(`link not allowed in ${where}: ${tag}`);
  }
  if ((out.match(/</g) || []).length !== (out.match(/>/g) || []).length) throw new Error(`broken tag in ${where}`);
  return out;
}
const plain = (s, vals, where) => esc(stripTags(fill(s, vals, where)));

// ---- assets ----------------------------------------------------------------
const assetMap = {};
const hashName = (name, buf) => {
  const ext = path.extname(name);
  return `${name.slice(0, -ext.length)}.${crypto.createHash("sha256").update(buf).digest("hex").slice(0, 10)}${ext}`;
};
function putAsset(src, name, subdir = "assets") {
  const buf = fs.readFileSync(src);
  const out = `${subdir}/${hashName(name, buf)}`;
  fs.mkdirSync(path.join(OUT, subdir), { recursive: true });
  fs.writeFileSync(path.join(OUT, out), buf);
  assetMap[`${subdir}/${name}`] = "/" + out;
  return "/" + out;
}
function putFile(rel, content) {
  const f = path.join(OUT, rel);
  fs.mkdirSync(path.dirname(f), { recursive: true });
  fs.writeFileSync(f, content);
}

// Screenshots: website/img/<name>-<width>.webp, made by tools/images.sh.
// Each entry: the widths that exist, and the box it is shown in.
const SHOTS = {
  hero: { widths: [960, 1600, 2560], ratio: [2560, 1600], sizes: "(max-width: 1120px) 100vw, 1080px", cls: "" },
  terminal: { widths: [960, 1600, 2560], ratio: [2560, 940], sizes: "(max-width: 860px) 100vw, 640px", cls: "" },
  files: { widths: [400, 648], ratio: [648, 980], sizes: "(max-width: 860px) 80vw, 380px", cls: "tall" },
  desktops: { widths: [960, 1600], ratio: [2560, 1600], sizes: "(max-width: 860px) 100vw, 640px", cls: "" },
  tunnels: { widths: [320, 640], ratio: [640, 700], sizes: "(max-width: 860px) 80vw, 380px", cls: "tall" },
  fits: { widths: [448, 896], ratio: [896, 1060], sizes: "(max-width: 860px) 80vw, 380px", cls: "tall" },
  monitor: { widths: [960, 1600, 2560], ratio: [2560, 680], sizes: "(max-width: 860px) 100vw, 640px", cls: "" },
  vault: { widths: [550, 1100], ratio: [1100, 940], sizes: "(max-width: 860px) 100vw, 640px", cls: "" },
  phone: { widths: [360, 720, 1080], ratio: [1080, 2160], sizes: "(max-width: 860px) 70vw, 300px", cls: "phone" },
};
const shotUrls = {};
function putShots() {
  for (const [name, s] of Object.entries(SHOTS)) {
    shotUrls[name] = s.widths.map((w) => ({ w, url: putAsset(path.join(HERE, "img", `${name}-${w}.webp`), `${name}-${w}.webp`, "assets/img") }));
  }
}
function shotHtml(name, alt, t, { eager = false } = {}) {
  const s = SHOTS[name];
  const set = shotUrls[name];
  const mid = set[Math.min(1, set.length - 1)];
  const big = set[set.length - 1];
  const h = Math.round((mid.w * s.ratio[1]) / s.ratio[0]);
  return `<figure class="shot${s.cls ? " " + s.cls : ""}"><a href="${big.url}" aria-label="${esc(t.openFull)}"><img src="${mid.url}" srcset="${set.map((x) => `${x.url} ${x.w}w`).join(", ")}" sizes="${s.sizes}" width="${mid.w}" height="${h}" alt="${esc(alt)}"${eager ? ' fetchpriority="high"' : ' loading="lazy"'} decoding="async"></a></figure>`;
}

// ---- icons (inline SVG, no external requests) ------------------------------
const svg = (d, extra = "") => `<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"${extra}>${d}</svg>`;
const ICON = {
  globe: svg('<circle cx="12" cy="12" r="9"/><path d="M3 12h18M12 3a14 14 0 0 1 0 18M12 3a14 14 0 0 0 0 18"/>'),
  menu: svg('<path d="M4 7h16M4 12h16M4 17h16"/>'),
  download: svg('<path d="M12 4v11M7 10l5 5 5-5M5 20h14"/>'),
  shield: svg('<path d="M12 3l8 3v6c0 4.5-3.4 8.3-8 9-4.6-.7-8-4.5-8-9V6z"/>'),
  lock: svg('<rect x="5" y="11" width="14" height="10" rx="2"/><path d="M8 11V8a4 4 0 0 1 8 0v3"/>'),
  sync: svg('<path d="M20 12a8 8 0 0 1-14.3 4.9M4 12a8 8 0 0 1 14.3-4.9M18 3v4.5h-4.5M6 21v-4.5h4.5"/>'),
  eyeOff: svg('<path d="M3 3l18 18M10.6 6.1A9.8 9.8 0 0 1 12 6c5.5 0 9 6 9 6a15.6 15.6 0 0 1-3.2 3.9M6.6 7.6C4.3 9.2 3 12 3 12s3.5 6 9 6a8.6 8.6 0 0 0 3.4-.7M9.9 9.9a3 3 0 0 0 4.2 4.2"/>'),
};

// ---- layout ----------------------------------------------------------------
function head(lang, page, title, description, ldjson) {
  const p = pagePath(lang, page);
  const alternates = BUILT.map((l) => `<link rel="alternate" hreflang="${l.hreflang}" href="${abs(pagePath(l, page))}">`).join("\n")
    + `\n<link rel="alternate" hreflang="x-default" href="${abs(pagePath(LANGS[0], page))}">`;
  const og = `${SITE}${assetMap["og/og.png"] || "/og/og.png"}`;
  return `<!doctype html>
<html lang="${lang.html}" dir="${lang.dir}">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>${esc(title)}</title>
<meta name="description" content="${esc(description)}">
<link rel="canonical" href="${abs(p)}">
${alternates}
<meta name="theme-color" content="#0a0a0c">
<meta name="color-scheme" content="dark">
<link rel="icon" href="/favicon.ico" sizes="any">
<link rel="icon" href="${assetMap["assets/icon-192.png"]}" type="image/png" sizes="192x192">
<link rel="apple-touch-icon" href="/apple-touch-icon.png">
<link rel="manifest" href="/manifest.webmanifest">
<link rel="stylesheet" href="${assetMap["assets/site.css"]}">
<meta property="og:type" content="website">
<meta property="og:site_name" content="Submarine">
<meta property="og:title" content="${esc(title)}">
<meta property="og:description" content="${esc(description)}">
<meta property="og:url" content="${abs(p)}">
<meta property="og:image" content="${og}">
<meta property="og:image:width" content="1200">
<meta property="og:image:height" content="630">
<meta property="og:image:alt" content="Submarine: SSH, SFTP, RDP, VNC and Telnet client">
<meta property="og:locale" content="${lang.og}">
${BUILT.filter((l) => l !== lang).map((l) => `<meta property="og:locale:alternate" content="${l.og}">`).join("\n")}
<meta name="twitter:card" content="summary_large_image">
<meta name="twitter:title" content="${esc(title)}">
<meta name="twitter:description" content="${esc(description)}">
<meta name="twitter:image" content="${og}">
<script type="application/ld+json">${JSON.stringify(ldjson).replace(/</g, "\\u003c")}</script>
</head>`;
}

function langLinks(lang, page, cls = "") {
  return BUILT.map((l) => `<a href="${pagePath(l, page)}" hreflang="${l.hreflang}" lang="${l.html}"${l === lang ? ' aria-current="true"' : ""}${cls}>${esc(l.name)}</a>`).join("");
}

function header(lang, page, t) {
  const home = pagePath(lang, "home");
  const links = [
    [`${home}#features`, t.nav.features, null],
    [pagePath(lang, "security"), t.nav.security, "security"],
    [pagePath(lang, "privacy"), t.nav.privacy, "privacy"],
    [REPO_URL, t.nav.github, null],
  ];
  const navLinks = links.map(([href, label, p]) => `<a href="${href}"${p === page ? ' aria-current="page"' : ""}>${esc(label)}</a>`).join("");
  return `<a class="skip" href="#main">${esc(t.nav.skip)}</a>
<header class="site-head"><div class="wrap">
<a class="brand" href="${home}" aria-label="${esc(t.nav.home)}"><img src="${assetMap["assets/logo.png"]}" width="30" height="30" alt="">Submarine</a>
<nav class="nav" aria-label="${esc(t.nav.menu)}">${navLinks}
<details class="pop"><summary aria-label="${esc(t.nav.language)}">${ICON.globe}<span>${esc(lang.name)}</span></summary><div class="pop-panel lang-list">${langLinks(lang, page)}</div></details>
<a class="cta" href="${home}#download">${esc(t.nav.download)}</a>
</nav>
<details class="pop menu"><summary aria-label="${esc(t.nav.menu)}">${ICON.menu}</summary><div class="pop-panel">${links.map(([href, label]) => `<a href="${href}">${esc(label)}</a>`).join("")}<a href="${home}#download">${esc(t.nav.download)}</a><hr class="visually-hidden"><div class="lang-list">${langLinks(lang, page)}</div></div></details>
</div></header>`;
}

function footer(lang, page, t, v) {
  return `<footer class="site-foot"><div class="wrap">
<div class="foot-grid">
<div><p><strong>Submarine</strong> · ${esc(t.common.tagline)}</p>
<p>${rich(t.footer.madeBy, { ...v, author: '<a href="https://sinaxhpm.com">Sina Xhpm</a>' }, "footer.madeBy")} ${esc(t.footer.license)}</p>
<p>${esc(t.footer.noTracking)}</p></div>
<ul class="foot-links">
<li><a href="${pagePath(lang, "security")}">${esc(t.nav.security)}</a></li>
<li><a href="${pagePath(lang, "privacy")}">${esc(t.nav.privacy)}</a></li>
<li><a href="${pagePath(lang, "delete")}">${esc(t.footer.deleteAccount)}</a></li>
<li><a href="${REPO_URL}">${esc(t.nav.github)}</a></li>
<li><a href="${RELEASES_URL}">${esc(t.footer.releases)}</a></li>
<li><a href="${ISSUES_URL}">${esc(t.footer.issues)}</a></li>
<li><a href="https://sinaxhpm.com">${esc(t.footer.author)}</a></li>
</ul>
</div>
<nav class="foot-langs" aria-label="${esc(t.nav.language)}">${langLinks(lang, page)}</nav>
</div></footer>`;
}

function page(lang, pageName, title, description, ld, main, t, v, scripts = "") {
  return `${head(lang, pageName, title, description, ld)}
<body>
${header(lang, pageName, t)}
<main id="main">
${main}
</main>
${footer(lang, pageName, t, v)}
${scripts}</body>
</html>
`;
}

// ---- values for the placeholders -----------------------------------------
function valuesFor(lang, code) {
  const n = new Intl.NumberFormat(lang.locale);
  const d = new Intl.DateTimeFormat(lang.locale, { dateStyle: "long", timeZone: "UTC" });
  return {
    argonMem: `${n.format(FACTS.argonMemMiB)} MiB`,
    argonPasses: n.format(FACTS.argonPasses),
    argonLanes: n.format(FACTS.argonLanes),
    limitMb: n.format(FACTS.limitMb),
    limitItems: n.format(FACTS.limitItems),
    limitProfiles: n.format(FACTS.limitProfiles),
    sessionDays: n.format(FACTS.sessionDays),
    auditDays: n.format(FACTS.auditDays),
    rateDays: n.format(FACTS.rateDays),
    unverifiedDays: n.format(FACTS.unverifiedDays),
    lockoutTries: n.format(FACTS.lockoutTries),
    lockoutMinutes: n.format(FACTS.lockoutMinutes),
    certSha256: FACTS.certSha256,
    commit: `<code>${COMMIT.slice(0, 7)}</code>`,
    date: d.format(new Date(`${POLICY_DATE}T00:00:00Z`)),
    repoUrl: REPO_URL,
    issuesUrl: ISSUES_URL,
    releasesUrl: RELEASES_URL,
    deleteUrl: pagePath(lang, "delete"),
    securityUrl: pagePath(lang, "security"),
    privacyUrl: pagePath(lang, "privacy"),
    code,
  };
}

const translationNote = (lang, page, t) =>
  lang.code === "en" ? "" : `<p class="translation-note" role="note">${esc(t.common.translationNote)} <a href="${pagePath(LANGS[0], page)}" hreflang="en">${esc(t.common.englishVersion)}</a></p>`;

// ---- pages -------------------------------------------------------------------
const FEATURE_SHOT = { terminal: "terminal", files: "files", desktops: "desktops", tunnels: "tunnels", fits: "fits", monitor: "monitor", vault: "vault", phone: "phone" };

function homePage(lang, t, v) {
  const h = t.home;
  const where = `${lang.code}:home`;
  const features = h.features.items.map((f, i) => `<article class="feature${i % 2 ? " flip" : ""}" id="${f.id}">
<div class="feature-text"><h3>${esc(f.title)}${f.new ? `<span class="badge">${esc(t.common.newIn)}</span>` : ""}</h3><p>${rich(f.text, v, `${where}.features.${f.id}`)}</p></div>
${shotHtml(FEATURE_SHOT[f.id], f.alt, t.common)}
</article>`).join("\n");
  const privacyIcons = [ICON.lock, ICON.sync, ICON.eyeOff];
  const main = `<section class="hero"><div class="wrap">
<span class="eyebrow">${esc(h.hero.eyebrow)}</span>
<h1>${esc(h.hero.title)}</h1>
<p class="lede">${rich(h.hero.lede, v, where)}</p>
<div class="btn-row center"><a class="btn primary" href="#download">${ICON.download}${esc(h.hero.download)}</a><a class="btn" href="${pagePath(lang, "security")}">${ICON.shield}${esc(h.hero.security)}</a></div>
<ul class="points">${h.hero.points.map((p) => `<li>${esc(p)}</li>`).join("")}</ul>
${shotHtml("hero", h.hero.imageAlt, t.common, { eager: true })}
</div></section>
<section id="features"><div class="wrap">
<div class="section-head"><h2>${esc(h.features.title)}</h2><p>${rich(h.features.lede, v, where)}</p></div>
<div class="features">
${features}
</div>
</div></section>
<section id="private"><div class="wrap">
<div class="section-head"><h2>${esc(h.privacy.title)}</h2><p>${rich(h.privacy.lede, v, where)}</p></div>
<div class="cards">${h.privacy.items.map((c, i) => `<div class="card"><div class="icon">${privacyIcons[i] || ICON.shield}</div><h3>${esc(c.title)}</h3><p>${rich(c.text, v, where)}</p></div>`).join("")}</div>
<p class="more"><a href="${pagePath(lang, "security")}">${esc(h.privacy.link)} <span class="arrow"></span></a></p>
</div></section>
<section id="free"><div class="wrap">
<div class="section-head"><h2>${esc(h.free.title)}</h2></div>
<ul class="checks">${h.free.items.map((x) => `<li><span>${rich(x, v, where)}</span></li>`).join("")}</ul>
</div></section>
<section id="download"><div class="wrap">
<div class="section-head"><h2>${esc(h.download.title)}</h2><p>${rich(h.download.lede, v, where)}</p></div>
<div class="platforms">${h.download.platforms.map((p) => `<div class="card platform"><h3>${esc(p.name)}</h3><p>${esc(p.detail)}</p><a class="btn primary" href="${RELEASES_URL}/latest">${ICON.download}${esc(h.download.button)}</a></div>`).join("")}</div>
<p class="note">${rich(h.download.betaNote, v, where)}</p>
<div class="verify-box"><h3>${esc(h.download.verifyTitle)}</h3><ul>${h.download.verify.map((x) => `<li>${rich(x, v, where)}</li>`).join("")}</ul></div>
</div></section>
<section id="faq"><div class="wrap">
<div class="section-head"><h2>${esc(h.faq.title)}</h2></div>
<div class="faq">${h.faq.items.map((q) => `<details><summary>${esc(q.q)}</summary><div><p>${rich(q.a, v, where)}</p></div></details>`).join("")}</div>
</div></section>`;
  const url = abs(pagePath(lang, "home"));
  const ld = {
    "@context": "https://schema.org",
    "@graph": [
      {
        "@type": "SoftwareApplication",
        "@id": `${url}#app`,
        name: "Submarine",
        alternateName: ["Submarine SSH Client", "Submarine SSH"],
        applicationCategory: "DeveloperApplication",
        applicationSubCategory: "SSH client",
        operatingSystem: "Windows 10, Windows 11, macOS, Linux, Android",
        description: stripTags(fill(h.description, v, where)),
        url,
        inLanguage: lang.hreflang,
        downloadUrl: RELEASES_URL,
        license: "https://opensource.org/licenses/MIT",
        isAccessibleForFree: true,
        offers: { "@type": "Offer", price: "0", priceCurrency: "USD" },
        author: { "@type": "Person", name: "Sina Xhpm", url: "https://sinaxhpm.com" },
        image: `${SITE}${assetMap["og/og.png"]}`,
        screenshot: ["hero", "terminal", "desktops", "phone"].map((s) => `${SITE}${shotUrls[s][shotUrls[s].length - 1].url}`),
        featureList: h.features.items.map((f) => f.title),
        sameAs: [REPO_URL],
      },
      {
        "@type": "FAQPage",
        "@id": `${url}#faq`,
        inLanguage: lang.hreflang,
        mainEntity: h.faq.items.map((q) => ({ "@type": "Question", name: q.q, acceptedAnswer: { "@type": "Answer", text: stripTags(fill(q.a, v, where)) } })),
      },
      { "@type": "WebSite", "@id": `${SITE}/#site`, name: "Submarine", url: `${SITE}/`, inLanguage: BUILT.map((l) => l.hreflang) },
    ],
  };
  return page(lang, "home", fill(h.title, v, where), fill(h.description, v, where), ld, main, t, v);
}

function crumbs(lang, pageName, name) {
  return {
    "@context": "https://schema.org",
    "@type": "BreadcrumbList",
    itemListElement: [
      { "@type": "ListItem", position: 1, name: "Submarine", item: abs(pagePath(lang, "home")) },
      { "@type": "ListItem", position: 2, name, item: abs(pagePath(lang, pageName)) },
    ],
  };
}

function codeLinks(keys, t, v) {
  if (!keys || !keys.length) return "";
  return `<ul class="code-links">${keys.map((k) => {
    const c = v.code[k];
    return `<li><a href="${c.url}" rel="noopener">${esc(t.security.codeLabels[k])}</a> <span>${esc(c.file.split("/").pop())}:${c.from}–${c.to}</span></li>`;
  }).join("")}</ul>`;
}

function securityPage(lang, t, v) {
  const s = t.security;
  const where = `${lang.code}:security`;
  const d = s.diagram;
  const example = `<pre><code>{
  <span class="k">"uuid"</span>: <span class="s">"6f1c2e0a9b7d4c3e8a5f0d1b2c3e4f50"</span>,
  <span class="k">"entity_type"</span>: <span class="s">"servers"</span>,
  <span class="k">"updated_at"</span>: <span class="s">"001761043562118:00000:9f3a61c2d4e5b7a8"</span>,
  <span class="k">"deleted"</span>: false,
  <span class="k">"blob"</span>: <span class="s">"4e8a1f0c2b9d3e7a5c6f8b1d0a2e4c6b8d0f1a3c5e7b9d1f…"</span>
}</code></pre>`;
  const sections = s.sections.map((sec) => {
    const body = (sec.paragraphs || []).map((p) => `<p>${rich(p, v, `${where}.${sec.id}`)}</p>`).join("\n");
    const ex = sec.example ? `<p>${rich(sec.example, v, where)} <span class="muted">(${esc(t.common.example)})</span></p>${example}` : "";
    const lists = sec.cannot ? `<div class="two-col"><div class="cannot"><h3>${esc(sec.cannotTitle)}</h3><ul>${sec.cannot.map((x) => `<li>${rich(x, v, where)}</li>`).join("")}</ul></div><div class="can"><h3>${esc(sec.canTitle)}</h3><ul>${sec.can.map((x) => `<li>${rich(x, v, where)}</li>`).join("")}</ul></div></div>` : "";
    return `<section id="${sec.id}"><h2>${esc(sec.title)}</h2>
${body}
${ex}${lists}${codeLinks(sec.code, t, v)}
</section>`;
  }).join("\n");
  const main = `<div class="wrap narrow">
<header class="page-head">
${translationNote(lang, "security", t)}
<h1>${esc(s.heading)}</h1>
<p class="lede">${rich(s.lede, v, where)}</p>
</header>
<div class="article">
<section id="summary">
<div class="summary-box"><h2>${esc(s.summaryTitle)}</h2><ul>${s.summary.map((x) => `<li>${rich(x, v, where)}</li>`).join("")}</ul></div>
<div class="two-col"><div class="cannot"><h3>${esc(s.canCannot.cannotTitle)}</h3><ul>${s.canCannot.cannot.map((x) => `<li>${rich(x, v, where)}</li>`).join("")}</ul></div><div class="can"><h3>${esc(s.canCannot.canTitle)}</h3><ul>${s.canCannot.can.map((x) => `<li>${rich(x, v, where)}</li>`).join("")}</ul></div></div>
</section>
<section id="flow"><h2>${esc(d.title)}</h2>
<figure class="flow">
<div class="flow-zone"><span>${esc(d.never)}</span><div class="flow-row"><span class="chip">${esc(d.password)}</span><span class="arrow"></span><span class="chip op">${esc(d.kdf)}</span><span class="arrow"></span><span class="chip">${esc(d.key)}</span><span class="arrow"></span><span class="chip op">${esc(d.encrypt)}</span><span class="arrow"></span><span class="chip">${esc(d.items)}</span></div></div>
<div class="flow-row"><span class="arrow"></span><span class="chip op">${esc(d.https)}</span><span class="arrow"></span><span class="chip server">${esc(d.server)}</span><span class="muted">${esc(d.stores)}</span></div>
<figcaption>${esc(d.caption)}</figcaption>
</figure>
</section>
${sections}
<section id="verify"><h2>${esc(s.verify.title)}</h2>
<p>${rich(s.verify.lede, v, where)}</p>
<ol class="steps">${s.verify.items.map((x) => `<li><h3>${esc(x.title)}</h3><p>${rich(x.text, v, where)}</p></li>`).join("")}</ol>
<h3>${esc(s.verify.paramsTitle)}</h3>
<div class="table-wrap"><table class="params"><tbody>${s.verify.params.map(([k, val]) => `<tr><td>${esc(k)}</td><td>${rich(val, v, where)}</td></tr>`).join("")}</tbody></table></div>
</section>
<section id="limits"><h2>${esc(s.limits.title)}</h2>
<ul>${s.limits.items.map((x) => `<li>${rich(x, v, where)}</li>`).join("")}</ul>
</section>
</div>
</div>`;
  const ld = [crumbs(lang, "security", s.heading), { "@context": "https://schema.org", "@type": "TechArticle", headline: s.heading, inLanguage: lang.hreflang, dateModified: DATE, author: { "@type": "Person", name: "Sina Xhpm", url: "https://sinaxhpm.com" }, about: { "@id": `${abs(pagePath(lang, "home"))}#app` } }];
  return page(lang, "security", fill(s.title, v, where), fill(s.description, v, where), ld, main, t, v);
}

function privacyPage(lang, t, v) {
  const p = t.privacy;
  const where = `${lang.code}:privacy`;
  const block = (sec) => `<section><h2>${esc(sec.title)}</h2>
${(sec.paragraphs || []).map((x) => `<p>${rich(x, v, where)}</p>`).join("\n")}
${sec.list ? `<ul>${sec.list.map((x) => `<li>${rich(x, v, where)}</li>`).join("")}</ul>` : ""}
${sec.after ? `<p>${rich(sec.after, v, where)}</p>` : ""}
</section>`;
  const tb = p.table;
  const main = `<div class="wrap narrow">
<header class="page-head">
${translationNote(lang, "privacy", t)}
<h1>${esc(p.heading)}</h1>
<p class="muted">${esc(fill(t.common.updated, v, where))}</p>
<p class="lede">${rich(p.intro, v, where)}</p>
</header>
<div class="article">
<section><div class="summary-box"><h2>${esc(p.shortTitle)}</h2><ul>${p.short.map((x) => `<li>${rich(x, v, where)}</li>`).join("")}</ul></div></section>
${p.sections.map(block).join("\n")}
<section id="sync"><h2>${esc(tb.title)}</h2>
<p>${rich(tb.intro, v, where)}</p>
<div class="table-wrap"><table><thead><tr>${tb.head.map((x) => `<th scope="col">${esc(x)}</th>`).join("")}</tr></thead><tbody>${tb.rows.map((r) => `<tr>${r.map((c) => `<td>${rich(c, v, where)}</td>`).join("")}</tr>`).join("")}</tbody></table></div>
${tb.after.map((x) => `<p>${rich(x, v, where)}</p>`).join("\n")}
</section>
${block(p.website)}
${block({ ...p.deleting, id: "deleting" })}
${block(p.security)}
${block(p.children)}
${block(p.changes)}
${block(p.contact)}
</div>
</div>`;
  const ld = [crumbs(lang, "privacy", p.heading)];
  return page(lang, "privacy", fill(p.title, v, where), fill(p.description, v, where), ld, main, t, v);
}

function deletePage(lang, t, v) {
  const d = t.delete;
  const where = `${lang.code}:delete`;
  const messages = { ...d.errors, working: d.working, done: d.done };
  const main = `<div class="wrap narrow">
<header class="page-head">
${translationNote(lang, "delete", t)}
<h1>${esc(d.heading)}</h1>
<p class="lede">${rich(d.lede, v, where)}</p>
</header>
<div class="article">
<section><h2>${esc(d.whatTitle)}</h2><ul>${d.what.map((x) => `<li>${rich(x, v, where)}</li>`).join("")}</ul>
<h2>${esc(d.staysTitle)}</h2><ul>${d.stays.map((x) => `<li>${rich(x, v, where)}</li>`).join("")}</ul></section>
<section><h2>${esc(d.inAppTitle)}</h2><p>${rich(d.inApp, v, where)}</p></section>
<section id="form"><h2>${esc(d.formTitle)}</h2>
<p>${rich(d.formLede, v, where)}</p>
<p class="no-js-note">${esc(d.errors.noScript)}</p>
<div class="js-only" hidden>
<form id="delete-form" class="form" autocomplete="on">
<label>${esc(d.email)}<input type="email" name="email" autocomplete="username" required maxlength="254"></label>
<label>${esc(d.password)}<input type="password" name="password" autocomplete="current-password" required maxlength="1024"><span class="hint">${esc(d.passwordHint)}</span></label>
<label class="check"><input type="checkbox" name="confirm"><span>${esc(d.confirm)}</span></label>
<button class="btn danger" type="submit">${esc(d.submit)}</button>
</form>
<div id="delete-status" class="form-status" role="status" aria-live="polite" tabindex="-1"></div>
</div>
<script type="application/json" id="delete-text">${JSON.stringify(messages).replace(/</g, "\\u003c")}</script>
</section>
<section><h2>${esc(d.helpTitle)}</h2><p>${rich(d.help, v, where)}</p></section>
</div>
</div>`;
  const ld = [crumbs(lang, "delete", d.heading)];
  return page(lang, "delete", fill(d.title, v, where), fill(d.description, v, where), ld, main, t, v, `<script src="${assetMap["assets/delete.js"]}" defer></script>\n`);
}

function notFoundPage(t, v) {
  const lang = LANGS[0];
  const n = t.notFound;
  const main = `<div class="wrap nf"><div>
<h1>${esc(n.heading)}</h1>
<p class="muted">${esc(n.text)}</p>
<p><a class="btn primary" href="/">${esc(n.home)}</a></p>
<p class="foot-langs">${BUILT.map((l) => `<a href="${pagePath(l, "home")}" hreflang="${l.hreflang}" lang="${l.html}">${esc(l.name)}</a>`).join("")}</p>
</div></div>`;
  // Not a page to index: no canonical or alternates.
  return `<!doctype html>
<html lang="en" dir="ltr">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>${esc(n.title)}</title>
<meta name="robots" content="noindex">
<meta name="color-scheme" content="dark">
<link rel="icon" href="/favicon.ico" sizes="any">
<link rel="stylesheet" href="${assetMap["assets/site.css"]}">
</head>
<body>
${header(lang, "home", t)}
<main id="main">${main}</main>
${footer(lang, "home", t, v)}
</body>
</html>
`;
}

// ---- site files --------------------------------------------------------------
function sitemap() {
  const urls = [];
  for (const pg of PAGES) {
    for (const l of BUILT) {
      const alts = BUILT.map((a) => `    <xhtml:link rel="alternate" hreflang="${a.hreflang}" href="${abs(pagePath(a, pg))}"/>`).join("\n");
      urls.push(`  <url>
    <loc>${abs(pagePath(l, pg))}</loc>
    <lastmod>${DATE}</lastmod>
${alts}
    <xhtml:link rel="alternate" hreflang="x-default" href="${abs(pagePath(LANGS[0], pg))}"/>
  </url>`);
    }
  }
  return `<?xml version="1.0" encoding="UTF-8"?>
<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9" xmlns:xhtml="http://www.w3.org/1999/xhtml">
${urls.join("\n")}
</urlset>
`;
}

const robots = () => `User-agent: *
Allow: /
Disallow: /account/
Disallow: /api/

Sitemap: ${SITE}/sitemap.xml
`;

const securityTxt = () => {
  const expires = new Date(Date.parse(`${DATE}T00:00:00Z`) + 365 * 864e5).toISOString();
  return `# Found a security problem in Submarine? Please open an issue that only says
# you found one (no details), and a private channel will be set up with you.
Contact: ${ISSUES_URL}/new
Expires: ${expires}
Preferred-Languages: en, fa
Canonical: ${SITE}/.well-known/security.txt
Policy: ${SITE}/security/
`;
};

const manifest = () => JSON.stringify({
  name: "Submarine",
  short_name: "Submarine",
  description: stripTags(EN.home.description),
  start_url: "/",
  display: "browser",
  background_color: "#0a0a0c",
  theme_color: "#0a0a0c",
  icons: [
    { src: assetMap["assets/icon-192.png"], sizes: "192x192", type: "image/png" },
    { src: assetMap["assets/icon-512.png"], sizes: "512x512", type: "image/png" },
  ],
}, null, 2) + "\n";

function llms(v) {
  const t = STR.en;
  const list = (xs) => xs.map((x) => `- ${stripTags(fill(x, v, "llms"))}`).join("\n");
  return `# Submarine

> ${stripTags(fill(t.home.description, v, "llms"))}

Submarine is a free, open-source (MIT) remote-access client by Sina Xhpm: SSH terminal, two-pane SFTP, port forwarding, folder mirror, RDP, VNC and Telnet, for Windows 10/11, macOS (Apple Silicon), Linux and Android 7.0+. Built with Rust and Tauri. No ads, no analytics, no paid plan; the optional encrypted sync service is free within limits (${FACTS.limitMb} MB of encrypted data, ${FACTS.limitItems.toLocaleString("en")} items and ${FACTS.limitProfiles} profiles per account).

## Pages

- [Home](${SITE}/): features, screenshots, downloads, questions
- [How your data is protected](${SITE}/security/): what is encrypted and how, what the sync server can and cannot see, how to check it in the code
- [Privacy policy](${SITE}/privacy/): what the app and the sync service keep, and for how long
- [Delete your account](${SITE}/delete-account/)
- Translations: ${BUILT.filter((l) => l.code !== "en").map((l) => `[${l.name}](${abs(pagePath(l, "home"))})`).join(", ")}

## Security, in short

${list(t.security.summary)}

## Links

- Source code: ${REPO_URL}
- Releases and downloads: ${RELEASES_URL}
- Issues: ${ISSUES_URL}
- Author: https://sinaxhpm.com

When citing Submarine, please link ${SITE}/ or the GitHub repository. The name "Submarine" and its logo are not covered by the MIT license; please don't ship a fork under the Submarine name.
`;
}

// ---- build ---------------------------------------------------------------------
function build() {
  fs.rmSync(OUT, { recursive: true, force: true });
  fs.mkdirSync(OUT, { recursive: true });
  const code = resolveCode();

  putAsset(path.join(HERE, "src", "site.css"), "site.css");
  putAsset(path.join(HERE, "src", "delete.js"), "delete.js");
  putAsset(path.join(HERE, "img", "logo-64.png"), "logo.png");
  putAsset(path.join(HERE, "img", "icon-192.png"), "icon-192.png");
  putAsset(path.join(HERE, "img", "icon-512.png"), "icon-512.png");
  putAsset(path.join(HERE, "img", "og.png"), "og.png", "og");
  putShots();
  fs.copyFileSync(path.join(HERE, "img", "favicon.ico"), path.join(OUT, "favicon.ico"));
  fs.copyFileSync(path.join(HERE, "img", "apple-touch-icon.png"), path.join(OUT, "apple-touch-icon.png"));
  // The link picture under a name that stays (the README and other sites point at it).
  fs.copyFileSync(path.join(HERE, "img", "og.png"), path.join(OUT, "og.png"));

  for (const lang of BUILT) {
    const t = STR[lang.code];
    const v = valuesFor(lang, code);
    const base = lang.code === "en" ? "" : `${lang.code}/`;
    putFile(`${base}index.html`, homePage(lang, t, v));
    putFile(`${base}security/index.html`, securityPage(lang, t, v));
    putFile(`${base}privacy/index.html`, privacyPage(lang, t, v));
    putFile(`${base}delete-account/index.html`, deletePage(lang, t, v));
  }
  const vEn = valuesFor(LANGS[0], code);
  putFile("404.html", notFoundPage(STR.en, vEn));
  putFile("sitemap.xml", sitemap());
  putFile("robots.txt", robots());
  putFile("llms.txt", llms(vEn));
  putFile("manifest.webmanifest", manifest());
  putFile(".well-known/security.txt", securityTxt());
  check(code);
}

// Every page's links to this site resolve to a built file; nothing left unfilled.
function check(code) {
  const problems = [];
  const files = [];
  const walk = (d) => { for (const e of fs.readdirSync(d, { withFileTypes: true })) { const f = path.join(d, e.name); if (e.isDirectory()) walk(f); else files.push(f); } };
  walk(OUT);
  const exists = (u) => {
    const p = decodeURI(u.split(/[?#]/)[0]);
    if (p === "/") return true;
    const f = path.join(OUT, p);
    return fs.existsSync(f) && (fs.statSync(f).isFile() || fs.existsSync(path.join(f, "index.html")));
  };
  for (const f of files.filter((x) => x.endsWith(".html"))) {
    const html = fs.readFileSync(f, "utf8");
    const rel = path.relative(OUT, f);
    for (const m of html.matchAll(/(?:href|src)="(\/[^"]*)"/g)) if (!exists(m[1])) problems.push(`${rel}: broken link ${m[1]}`);
    for (const m of html.matchAll(/srcset="([^"]+)"/g)) for (const part of m[1].split(",")) { const u = part.trim().split(" ")[0]; if (!exists(u)) problems.push(`${rel}: broken srcset ${u}`); }
    const text = html.replace(/<script[\s\S]*?<\/script>/g, "").replace(/<[^>]+>/g, " ");
    const left = text.match(/\{[A-Za-z]\w*\}/g);
    if (left) problems.push(`${rel}: unfilled ${[...new Set(left)].join(" ")}`);
    if (/<img(?![^>]*\balt=)/.test(html)) problems.push(`${rel}: an image without alt`);
    if (/\sstyle="/.test(html)) problems.push(`${rel}: an inline style (blocked by the CSP)`);
    if (/<script(?![^>]*\b(src=|type="application\/(ld\+)?json"))/.test(html)) problems.push(`${rel}: an inline script (blocked by the CSP)`);
  }
  for (const [k, c] of Object.entries(code)) console.log(`code ${k}: ${c.file}:${c.from}-${c.to}`);
  if (warnings.length) console.log(`${warnings.length} translation warnings:\n  ${warnings.slice(0, 60).join("\n  ")}${warnings.length > 60 ? "\n  ..." : ""}`);
  if (problems.length) { console.error(problems.join("\n")); process.exit(1); }
  console.log(`built ${files.length} files for ${BUILT.length} languages into ${OUT} (code links at ${COMMIT.slice(0, 7)})`);
}

build();
