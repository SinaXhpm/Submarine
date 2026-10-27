// What every product site shares: the languages and their strings, safe text
// helpers, hashed assets, the page frame (head, header, footer), sitemap and
// the checks run on the finished site. Each site (sites/<id>/site.mjs) adds
// its own pages on top. No dependencies beyond Node 20+.
import crypto from "node:crypto";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { ICON } from "./shared/icons.mjs";

export { ICON };
export const HERE = path.dirname(fileURLToPath(import.meta.url));
export const SHARED = path.join(HERE, "shared");
export const AUTHOR = { "@type": "Person", name: "Sina Xhpm", url: "https://sinaxhpm.com" };

// ---- languages --------------------------------------------------------------
export const LANGS = [
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

// ---- text helpers -------------------------------------------------------------
export const esc = (s) => String(s).replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;");
export const stripTags = (s) => String(s).replace(/<[^>]+>/g, "");
const ALLOWED = /^<\/?(a|strong|em|code|br)(\s[^<>]*)?\/?>$/;
// Fills {name} from vals; every placeholder must be known.
export function fill(s, vals, where) {
  return String(s).replace(/\{(\w+)\}/g, (m, k) => {
    if (!(k in vals)) throw new Error(`unknown placeholder {${k}} in ${where}`);
    return vals[k];
  });
}
// A string that may carry the allowed inline tags (a, strong, em, code, br).
export function rich(s, vals, where) {
  const out = fill(s, vals, where);
  for (const tag of out.match(/<[^>]*>/g) || []) {
    if (!ALLOWED.test(tag)) throw new Error(`tag not allowed in ${where}: ${tag}`);
    if (/^<a\s/.test(tag) && !/^<a href="(https:\/\/[^"]+|\/[^"]*|#[^"]*)"( rel="noopener")?>$/.test(tag)) throw new Error(`link not allowed in ${where}: ${tag}`);
  }
  if ((out.match(/</g) || []).length !== (out.match(/>/g) || []).length) throw new Error(`broken tag in ${where}`);
  return out;
}
export const plain = (s, vals, where) => esc(stripTags(fill(s, vals, where)));
export const json = (o) => JSON.stringify(o).replace(/</g, "\\u003c");

// The language's strings, English where it has none; arrays are taken whole
// (a translated list replaces the English one).
function mergeStrings(en, tr, warnings, at = "") {
  if (tr === undefined) { if (at) warnings.push(`missing ${at}`); return en; }
  if (Array.isArray(en)) {
    if (!Array.isArray(tr)) { warnings.push(`not a list: ${at}`); return en; }
    // A list of things with ids (features, sections) must be the English one, in its order.
    const ids = (xs) => xs.map((x) => (x && typeof x === "object" ? x.id : undefined)).join(",");
    if (en.some((x) => x && x.id) && ids(tr) !== ids(en)) { warnings.push(`other items than English, English used: ${at}`); return en; }
    if (tr.length !== en.length) warnings.push(`list length ${tr.length} vs ${en.length}: ${at}`);
    return tr.map((v, i) => (en[i] !== undefined && typeof en[i] === "object" ? mergeStrings(en[i], v, warnings, `${at}[${i}]`) : v));
  }
  if (en && typeof en === "object") {
    const o = {};
    for (const k of Object.keys(en)) o[k] = mergeStrings(en[k], tr ? tr[k] : undefined, warnings, at ? `${at}.${k}` : k);
    return o;
  }
  return tr;
}

const hashName = (name, buf) => {
  const ext = path.extname(name);
  return `${name.slice(0, -ext.length)}.${crypto.createHash("sha256").update(buf).digest("hex").slice(0, 10)}${ext}`;
};

// ---- a site -------------------------------------------------------------------
// cfg: { id, origin, name, src (folder with i18n/ and img/), out, slugs {page: "path/"},
//        langs [codes], themeColor, ogAlt, nav(lang, page, t) -> [[href, label, pageId]],
//        cta(lang, t) -> [href, label], footer(lang, page, t, v) -> html }
export class Site {
  constructor(cfg) {
    Object.assign(this, cfg);
    this.assets = {};
    this.shotUrls = {};
    this.warnings = [];
    const read = (code) => JSON.parse(fs.readFileSync(path.join(this.src, "i18n", `${code}.json`), "utf8"));
    this.en = read("en");
    this.str = { en: this.en };
    for (const code of this.langs.filter((c) => c !== "en")) {
      if (!fs.existsSync(path.join(this.src, "i18n", `${code}.json`))) continue;
      const w = [];
      this.str[code] = mergeStrings(this.en, read(code), w);
      this.warnings.push(...w.map((x) => `${code}: ${x}`));
    }
    this.built = LANGS.filter((l) => this.str[l.code]);
    fs.rmSync(this.out, { recursive: true, force: true });
    fs.mkdirSync(this.out, { recursive: true });
  }

  pagePath(lang, page) { return `/${lang.code === "en" ? "" : lang.code + "/"}${this.slugs[page]}`; }
  abs(p) { return this.origin + p; }

  // A file under a name with its hash in it, so it can be cached for good.
  asset(src, name, subdir = "assets", buf = fs.readFileSync(src)) {
    const out = `${subdir}/${hashName(name, buf)}`;
    fs.mkdirSync(path.join(this.out, subdir), { recursive: true });
    fs.writeFileSync(path.join(this.out, out), buf);
    this.assets[`${subdir}/${name}`] = "/" + out;
    return "/" + out;
  }
  file(rel, content) {
    const f = path.join(this.out, rel);
    fs.mkdirSync(path.dirname(f), { recursive: true });
    fs.writeFileSync(f, content);
  }
  copy(src, rel) {
    fs.mkdirSync(path.dirname(path.join(this.out, rel)), { recursive: true });
    fs.copyFileSync(src, path.join(this.out, rel));
  }

  // The shared style sheet with its fonts, and the shared script.
  putShared() {
    let css = fs.readFileSync(path.join(SHARED, "site.css"), "utf8");
    css = css.replace(/url\("fonts\/([^"]+)"\)/g, (m, f) => {
      const url = this.asset(path.join(SHARED, "fonts", f), f, "assets/fonts");
      return `url("${url}")`;
    });
    this.asset(null, "site.css", "assets", Buffer.from(css));
    this.asset(path.join(SHARED, "site.js"), "site.js");
    for (const f of fs.readdirSync(path.join(SHARED, "fonts")).filter((x) => x.startsWith("LICENSE"))) this.copy(path.join(SHARED, "fonts", f), `assets/fonts/${f}`);
  }

  // Screenshots: <src>/img/<name>-<width>.webp. defs: { name: { widths, ratio, sizes, cls } }
  putShots(defs) {
    this.shotDefs = defs;
    for (const [name, s] of Object.entries(defs)) {
      this.shotUrls[name] = s.widths.map((w) => ({ w, url: this.asset(path.join(this.src, "img", `${name}-${w}.webp`), `${name}-${w}.webp`, "assets/img") }));
    }
  }
  shotLargest(name) { const s = this.shotUrls[name]; return this.abs(s[s.length - 1].url); }
  shot(name, alt, t, { eager = false, cls = "", caption = "" } = {}) {
    const s = this.shotDefs[name];
    const set = this.shotUrls[name];
    const mid = set[Math.min(1, set.length - 1)];
    const big = set[set.length - 1];
    const h = Math.round((mid.w * s.ratio[1]) / s.ratio[0]);
    const c = [s.cls, cls].filter(Boolean).join(" ");
    return `<figure class="shot${c ? " " + c : ""}"><a href="${big.url}" aria-label="${esc(t.common.openFull)}"><img src="${mid.url}" srcset="${set.map((x) => `${x.url} ${x.w}w`).join(", ")}" sizes="${s.sizes}" width="${mid.w}" height="${h}" alt="${esc(alt)}"${eager ? ' fetchpriority="high"' : ' loading="lazy"'} decoding="async"></a>${caption ? `<figcaption>${caption}</figcaption>` : ""}</figure>`;
  }

  langLinks(lang, page) {
    return this.built.map((l) => `<a href="${this.pagePath(l, page)}" hreflang="${l.hreflang}" lang="${l.html}"${l === lang ? ' aria-current="true"' : ""}>${esc(l.name)}</a>`).join("");
  }

  head(lang, page, title, description, ld, { index = true } = {}) {
    const p = this.pagePath(lang, page);
    const og = this.abs(this.assets["og/og.png"]);
    const fontPre = lang.dir === "rtl" ? "assets/fonts/vazirmatn-arabic-wght-normal.woff2" : "assets/fonts/inter-latin-wght-normal.woff2";
    const alternates = index ? this.built.map((l) => `<link rel="alternate" hreflang="${l.hreflang}" href="${this.abs(this.pagePath(l, page))}">`).join("\n")
      + `\n<link rel="alternate" hreflang="x-default" href="${this.abs(this.pagePath(LANGS[0], page))}">` : '<meta name="robots" content="noindex">';
    return `<!doctype html>
<html lang="${lang.html}" dir="${lang.dir}" data-close="${esc(this.str[lang.code].common.close)}">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>${esc(title)}</title>
<meta name="description" content="${esc(description)}">
${index ? `<link rel="canonical" href="${this.abs(p)}">\n` : ""}${alternates}
<meta name="theme-color" content="${this.themeColor}">
<meta name="color-scheme" content="dark">
<link rel="preload" href="${this.assets[fontPre]}" as="font" type="font/woff2" crossorigin>
<link rel="stylesheet" href="${this.assets["assets/site.css"]}">
<link rel="icon" href="/favicon.ico" sizes="any">
<link rel="icon" href="${this.assets["assets/icon-192.png"]}" type="image/png" sizes="192x192">
<link rel="apple-touch-icon" href="/apple-touch-icon.png">
<link rel="manifest" href="/manifest.webmanifest">
<meta property="og:type" content="website">
<meta property="og:site_name" content="${esc(this.name)}">
<meta property="og:title" content="${esc(title)}">
<meta property="og:description" content="${esc(description)}">
<meta property="og:url" content="${this.abs(p)}">
<meta property="og:image" content="${og}">
<meta property="og:image:width" content="1200">
<meta property="og:image:height" content="630">
<meta property="og:image:alt" content="${esc(this.ogAlt)}">
<meta property="og:locale" content="${lang.og}">
${this.built.filter((l) => l !== lang).map((l) => `<meta property="og:locale:alternate" content="${l.og}">`).join("\n")}
<meta name="twitter:card" content="summary_large_image">
<meta name="twitter:title" content="${esc(title)}">
<meta name="twitter:description" content="${esc(description)}">
<meta name="twitter:image" content="${og}">
${ld ? `<script type="application/ld+json">${json(ld)}</script>\n` : ""}<script src="${this.assets["assets/site.js"]}" defer></script>
</head>`;
  }

  header(lang, page, t) {
    const home = this.pagePath(lang, "home");
    const links = this.nav(lang, page, t);
    const [ctaHref, ctaLabel] = this.cta(lang, t);
    const navLinks = links.map(([href, label, p, icon]) => icon
      ? `<a class="icon-link" href="${href}" aria-label="${esc(label)}" title="${esc(label)}">${ICON[icon]}</a>`
      : `<a href="${href}"${p === page ? ' aria-current="page"' : ""}>${esc(label)}</a>`).join("");
    return `<a class="skip" href="#main">${esc(t.nav.skip)}</a>
<header class="site-head"><div class="wrap">
<a class="brand" href="${home}" aria-label="${esc(t.nav.home)}"><img src="${this.assets["assets/logo.png"]}" width="32" height="32" alt="">${esc(this.name)}</a>
<nav class="nav" aria-label="${esc(t.nav.menu)}">${navLinks}
<details class="pop"><summary aria-label="${esc(t.nav.language)}">${ICON.globe}<span>${esc(lang.name)}</span></summary><div class="pop-panel lang-list">${this.langLinks(lang, page)}</div></details>
<a class="btn primary small cta" href="${ctaHref}">${esc(ctaLabel)}</a>
</nav>
<details class="pop menu"><summary aria-label="${esc(t.nav.menu)}">${ICON.menu}</summary><div class="pop-panel">${links.map(([href, label]) => `<a href="${href}">${esc(label)}</a>`).join("")}<a href="${ctaHref}">${esc(ctaLabel)}</a><hr><div class="lang-list">${this.langLinks(lang, page)}</div></div></details>
</div></header>`;
  }

  footerFrame(lang, page, t, cols, sister) {
    return `<footer class="site-foot"><div class="wrap">
<div class="foot-grid">
<div class="foot-about"><a class="brand" href="${this.pagePath(lang, "home")}"><img src="${this.assets["assets/logo.png"]}" width="32" height="32" alt="">${esc(this.name)}</a>
<p>${esc(t.common.tagline)}</p><p>${esc(t.footer.noTracking)}</p></div>
${cols.map((c) => `<div><h4>${esc(c.title)}</h4><ul>${c.links.map(([href, label]) => `<li><a href="${href}">${esc(label)}</a></li>`).join("")}</ul></div>`).join("\n")}
<div class="foot-sister">${sister}</div>
</div>
<nav class="foot-langs" aria-label="${esc(t.nav.language)}">${this.langLinks(lang, page)}</nav>
<div class="foot-bottom"><span>© ${new Date(`${this.date}T00:00:00Z`).getUTCFullYear()} Sina Xhpm · ${esc(t.footer.license)}</span><span>${esc(t.footer.madeWith)}</span></div>
</div></footer>`;
  }

  page({ lang, page, title, description, ld, main, t, v, scripts = "", index = true, bodyClass = "" }) {
    return `${this.head(lang, page, title, description, ld, { index })}
<body data-site="${this.id}"${bodyClass ? ` class="${bodyClass}"` : ""}>
${this.header(lang, page, t)}
<main id="main">
${main}
</main>
${this.footer(lang, page, t, v)}
${scripts}</body>
</html>
`;
  }

  translationNote(lang, page, t) {
    return lang.code === "en" ? "" : `<p class="translation-note" role="note">${esc(t.common.translationNote)} <a href="${this.pagePath(LANGS[0], page)}" hreflang="en">${esc(t.common.englishVersion)}</a></p>`;
  }

  crumbs(lang, page, name) {
    return {
      "@context": "https://schema.org",
      "@type": "BreadcrumbList",
      itemListElement: [
        { "@type": "ListItem", position: 1, name: this.name, item: this.abs(this.pagePath(lang, "home")) },
        { "@type": "ListItem", position: 2, name, item: this.abs(this.pagePath(lang, page)) },
      ],
    };
  }

  // A document page: a heading band, then the text with a list of its sections.
  docPage({ lang, page, t, heading, lede, meta = "", sections, aside = "" }) {
    const toc = sections.filter((s) => s.id && s.toc);
    return `<div class="doc-hero"><div class="wrap">
${this.translationNote(lang, page, t)}
<h1>${esc(heading)}</h1>
${lede ? `<p class="lede">${lede}</p>` : ""}${meta ? `\n<p class="meta">${meta}</p>` : ""}
</div></div>
<div class="wrap doc">
<nav class="toc" aria-label="${esc(t.common.onThisPage)}"><b>${esc(t.common.onThisPage)}</b><ol>${toc.map((s) => `<li><a href="#${s.id}">${esc(s.toc)}</a></li>`).join("")}</ol>${aside}</nav>
<div class="doc-body">
${sections.map((s) => `<section${s.id ? ` id="${s.id}"` : ""}>${s.html}</section>`).join("\n")}
</div>
</div>`;
  }

  // ---- site files ----
  sitemap(pages) {
    const urls = [];
    for (const pg of pages) {
      for (const l of this.built) {
        const alts = this.built.map((a) => `    <xhtml:link rel="alternate" hreflang="${a.hreflang}" href="${this.abs(this.pagePath(a, pg))}"/>`).join("\n");
        urls.push(`  <url>
    <loc>${this.abs(this.pagePath(l, pg))}</loc>
    <lastmod>${this.date}</lastmod>
${alts}
    <xhtml:link rel="alternate" hreflang="x-default" href="${this.abs(this.pagePath(LANGS[0], pg))}"/>
  </url>`);
      }
    }
    return `<?xml version="1.0" encoding="UTF-8"?>
<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9" xmlns:xhtml="http://www.w3.org/1999/xhtml">
${urls.join("\n")}
</urlset>
`;
  }

  robots(disallow = []) {
    return `User-agent: *
Allow: /
${disallow.map((d) => `Disallow: ${d}\n`).join("")}
Sitemap: ${this.origin}/sitemap.xml
`;
  }

  securityTxt(contact, policy) {
    const expires = new Date(Date.parse(`${this.date}T00:00:00Z`) + 365 * 864e5).toISOString();
    return `# Found a security problem in ${this.name}? Please open an issue that only says
# you found one (no details), and a private channel will be set up with you.
Contact: ${contact}
Expires: ${expires}
Preferred-Languages: en, fa
Canonical: ${this.origin}/.well-known/security.txt
Policy: ${policy}
`;
  }

  manifest(description) {
    return JSON.stringify({
      name: this.name,
      short_name: this.name,
      description,
      start_url: "/",
      display: "browser",
      background_color: this.themeColor,
      theme_color: this.themeColor,
      icons: [
        { src: this.assets["assets/icon-192.png"], sizes: "192x192", type: "image/png" },
        { src: this.assets["assets/icon-512.png"], sizes: "512x512", type: "image/png" },
      ],
    }, null, 2) + "\n";
  }

  // Every page's links to this site resolve to a built file; nothing left unfilled.
  check(extra = []) {
    const problems = [];
    const files = [];
    const walk = (d) => { for (const e of fs.readdirSync(d, { withFileTypes: true })) { const f = path.join(d, e.name); if (e.isDirectory()) walk(f); else files.push(f); } };
    walk(this.out);
    const exists = (u) => {
      const p = decodeURI(u.split(/[?#]/)[0]);
      if (p === "/") return true;
      const f = path.join(this.out, p);
      return fs.existsSync(f) && (fs.statSync(f).isFile() || fs.existsSync(path.join(f, "index.html")));
    };
    for (const f of files.filter((x) => x.endsWith(".html"))) {
      const html = fs.readFileSync(f, "utf8");
      const rel = path.relative(this.out, f);
      for (const m of html.matchAll(/(?:href|src)="(\/[^"]*)"/g)) if (!exists(m[1])) problems.push(`${rel}: broken link ${m[1]}`);
      for (const m of html.matchAll(/srcset="([^"]+)"/g)) for (const part of m[1].split(",")) { const u = part.trim().split(" ")[0]; if (!exists(u)) problems.push(`${rel}: broken srcset ${u}`); }
      for (const m of html.matchAll(/href="#([^"]+)"/g)) if (!html.includes(`id="${m[1]}"`)) problems.push(`${rel}: no anchor #${m[1]}`);
      const text = html.replace(/<script[\s\S]*?<\/script>/g, "").replace(/<[^>]+>/g, " ");
      const left = text.match(/\{[A-Za-z]\w*\}/g);
      if (left) problems.push(`${rel}: unfilled ${[...new Set(left)].join(" ")}`);
      if (/<img(?![^>]*\balt=)/.test(html)) problems.push(`${rel}: an image without alt`);
      if (/\sstyle="/.test(html) || /<style/.test(html)) problems.push(`${rel}: an inline style (blocked by the CSP)`);
      if (/<script(?![^>]*\b(src=|type="application\/(ld\+)?json"))/.test(html)) problems.push(`${rel}: an inline script (blocked by the CSP)`);
      if (/\son[a-z]+="/.test(html)) problems.push(`${rel}: an inline event handler (blocked by the CSP)`);
      const ids = [...html.matchAll(/\sid="([^"]+)"/g)].map((m) => m[1]);
      const dup = ids.filter((x, i) => ids.indexOf(x) !== i);
      if (dup.length) problems.push(`${rel}: duplicate id ${[...new Set(dup)].join(" ")}`);
    }
    problems.push(...extra);
    if (this.warnings.length) console.log(`${this.id}: ${this.warnings.length} translation warnings:\n  ${this.warnings.slice(0, 40).join("\n  ")}${this.warnings.length > 40 ? "\n  ..." : ""}`);
    if (problems.length) { console.error(problems.join("\n")); process.exitCode = 1; return false; }
    console.log(`${this.id}: built ${files.length} files for ${this.built.length} languages into ${this.out}`);
    return true;
  }
}
