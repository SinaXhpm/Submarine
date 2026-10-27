// submarine.sinaxhpm.com: the home page, how your data is protected (with
// links into the code at a fixed commit), the privacy policy and the account
// deletion page, in every language that has strings in i18n/.
import { execFileSync } from "node:child_process";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { AUTHOR, ICON, LANGS, Site, esc, fill, json, plain, rich, stripTags } from "../../engine.mjs";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const REPO = path.resolve(HERE, "..", "..", "..");
const ORIGIN = "https://submarine.sinaxhpm.com";
const REPO_URL = "https://github.com/SinaXhpm/Submarine";
const RELEASES_URL = `${REPO_URL}/releases`;
const ISSUES_URL = `${REPO_URL}/issues`;
const ECHOES = "https://echoes.sinaxhpm.com";
const POLICY_DATE = "2026-10-27";

// ---- facts (one place; the pages only refer to them) --------------------
// Checked against the code at the commit: Argon2 settings (src-tauri/src/lib.rs
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
  scrollback: 10000,
  certSha256: "e9e350dc9f8628bee461be3424c0c2b2180f00685cc380de80c86808ae669f1c",
};

// The security page's links into the code: where each piece starts, resolved
// to line ranges at the commit so the links never drift.
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
  rdpPassword: { file: "src-tauri/src/rdp.rs", from: /^pub fn protect_password\(/ },
  shareInside: { file: "src-tauri/src/rdp_share.rs", from: /^pub fn inside\(/ },
  vncPaste: { file: "src-tauri/src/vnc_client.rs", from: /^pub fn paste_refusal\(/ },
};

function resolveCode(commit) {
  const cache = new Map();
  const lines = (file) => {
    if (!cache.has(file)) cache.set(file, execFileSync("git", ["-C", REPO, "show", `${commit}:${file}`], { maxBuffer: 64 << 20 }).toString().split("\n"));
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
    if (a < 0) throw new Error(`code anchor not found: ${key} in ${c.file} at ${commit}`);
    const b0 = c.through ? ls.findIndex((l, i) => i > a && c.through.test(l)) : a;
    if (b0 < 0) throw new Error(`code anchor 'through' not found: ${key}`);
    const b = blockEnd(ls, b0);
    if (b - a > 160) throw new Error(`code range too long for ${key}: ${a + 1}-${b + 1}`);
    out[key] = { file: c.file, from: a + 1, to: b + 1, url: `${REPO_URL}/blob/${commit}/${c.file}#L${a + 1}-L${b + 1}` };
  }
  return out;
}

// Screenshots: img/<name>-<width>.webp, made by tools/images.sh.
const SHOTS = {
  hero: { widths: [960, 1600, 2560], ratio: [2560, 1600], sizes: "(max-width: 1160px) 100vw, 1120px" },
  terminal: { widths: [960, 1600, 2560], ratio: [2560, 940], sizes: "(max-width: 900px) 100vw, 680px" },
  files: { widths: [400, 648], ratio: [648, 980], sizes: "(max-width: 900px) 80vw, 380px", cls: "tall" },
  rdp: { widths: [960, 1600, 2560], ratio: [2560, 1600], sizes: "(max-width: 960px) 100vw, 680px" },
  vnc: { widths: [960, 1600, 2560], ratio: [2560, 1600], sizes: "(max-width: 960px) 90vw, 440px" },
  remoteSettings: { widths: [380, 760], ratio: [760, 1232], sizes: "(max-width: 960px) 60vw, 220px" },
  tunnels: { widths: [320, 640], ratio: [640, 700], sizes: "(max-width: 900px) 80vw, 380px", cls: "tall" },
  fits: { widths: [448, 896], ratio: [896, 1060], sizes: "(max-width: 900px) 80vw, 380px", cls: "tall" },
  monitor: { widths: [960, 1600, 2560], ratio: [2560, 680], sizes: "(max-width: 900px) 100vw, 680px" },
  vault: { widths: [550, 1100], ratio: [1100, 940], sizes: "(max-width: 900px) 100vw, 680px" },
  phone: { widths: [360, 720, 1080], ratio: [1080, 2160], sizes: "(max-width: 900px) 70vw, 300px", cls: "phone" },
};
// Which picture goes with which feature row, and the icons of the tiles and points.
const FEATURE_SHOT = { terminal: "terminal", files: "files", tunnels: "tunnels", fits: "fits", monitor: "monitor", vault: "vault", phone: "phone" };
const TILE_ICON = { terminal: "terminal", wall: "split", files: "folder", remote: "desktop", telnet: "server", tunnels: "tunnel", agent: "key", sync: "sync", sharing: "users", monitor: "activity", vault: "lock", phone: "phone", commands: "code", imports: "plug" };
const POINT_ICON = { clipboard: "clipboard", files: "folder", quality: "sliders", mark: "shieldCheck", ssh: "tunnel", keyboard: "keyboard", password: "key", split: "split" };
const PLATFORM_ICON = { windows: "windows", macos: "laptop", linux: "box", android: "phone" };

export default function build({ out, commit, date }) {
  commit = commit || execFileSync("git", ["-C", REPO, "rev-parse", "origin/main"]).toString().trim();
  const site = new Site({
    id: "submarine",
    origin: ORIGIN,
    name: "Submarine",
    src: HERE,
    out,
    date,
    langs: LANGS.map((l) => l.code),
    slugs: { home: "", security: "security/", privacy: "privacy/", delete: "delete-account/" },
    themeColor: "#04070f",
    ogAlt: "Submarine: SSH, SFTP, RDP, VNC and Telnet client",
    nav: (lang, page, t) => [
      [`${site.pagePath(lang, "home")}#features`, t.nav.features, null],
      [`${site.pagePath(lang, "home")}#remote`, t.nav.remote, null],
      [site.pagePath(lang, "security"), t.nav.security, "security"],
      [site.pagePath(lang, "privacy"), t.nav.privacy, "privacy"],
      [REPO_URL, t.nav.github, null, "code"],
    ],
    cta: (lang, t) => [`${site.pagePath(lang, "home")}#download`, t.nav.download],
    footer: (lang, page, t) => {
      const home = site.pagePath(lang, "home");
      const f = t.footer;
      const cols = [
        { title: f.product, links: [[`${home}#features`, t.nav.features], [`${home}#remote`, t.nav.remote], [`${home}#download`, t.nav.download], [`${home}#faq`, f.faq], [RELEASES_URL, f.releases]] },
        { title: f.trust, links: [[site.pagePath(lang, "security"), t.nav.security], [site.pagePath(lang, "privacy"), t.nav.privacy], [site.pagePath(lang, "delete"), f.deleteAccount], [`${home}#verify`, f.verify]] },
        { title: f.project, links: [[REPO_URL, t.nav.github], [ISSUES_URL, f.issues], ["https://sinaxhpm.com", f.author]] },
      ];
      const sisterPath = lang.code === "en" ? "/" : `/${lang.code}/`;
      const sister = `<a class="sister" href="${ECHOES}${sisterPath}" hreflang="${lang.hreflang}"><img src="${site.assets["assets/echoes.png"]}" width="44" height="44" alt=""><span><b>Echoes</b><span>${esc(f.sister)}</span></span></a>`;
      return site.footerFrame(lang, page, t, cols, sister);
    },
  });

  const code = resolveCode(commit);
  site.putShared();
  site.asset(path.join(HERE, "delete.js"), "delete.js");
  site.asset(path.join(HERE, "img", "logo-64.png"), "logo.png");
  site.asset(path.join(HERE, "img", "echoes-88.png"), "echoes.png");
  site.asset(path.join(HERE, "img", "icon-192.png"), "icon-192.png");
  site.asset(path.join(HERE, "img", "icon-512.png"), "icon-512.png");
  site.asset(path.join(HERE, "img", "og.png"), "og.png", "og");
  site.putShots(SHOTS);
  site.copy(path.join(HERE, "img", "favicon.ico"), "favicon.ico");
  site.copy(path.join(HERE, "img", "apple-touch-icon.png"), "apple-touch-icon.png");
  // The link picture under a name that stays (the README and other sites point at it).
  site.copy(path.join(HERE, "img", "og.png"), "og.png");

  const valuesFor = (lang) => {
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
      scrollback: n.format(FACTS.scrollback),
      certSha256: FACTS.certSha256,
      commit: `<code>${commit.slice(0, 7)}</code>`,
      date: d.format(new Date(`${POLICY_DATE}T00:00:00Z`)),
      repoUrl: REPO_URL,
      issuesUrl: ISSUES_URL,
      releasesUrl: RELEASES_URL,
      deleteUrl: site.pagePath(lang, "delete"),
      securityUrl: site.pagePath(lang, "security"),
      privacyUrl: site.pagePath(lang, "privacy"),
      code,
    };
  };

  const ul = (xs, v, where, cls = "") => `<ul${cls ? ` class="${cls}"` : ""}>${xs.map((x) => `<li>${rich(x, v, where)}</li>`).join("")}</ul>`;
  const ticks = (xs, v, where) => `<ul class="ticks">${xs.map((x) => `<li>${ICON.check}<span>${rich(x, v, where)}</span></li>`).join("")}</ul>`;
  const head2 = (h, v, where, id) => `<div class="section-head reveal">${h.kicker ? `<span class="kicker">${esc(h.kicker)}</span>` : ""}<h2${id ? ` id="${id}-title"` : ""}>${rich(h.title, v, where)}</h2>${h.lede ? `<p>${rich(h.lede, v, where)}</p>` : ""}</div>`;
  const badge = (t, f) => (f.new ? `<span class="badge">${esc(t.common.newIn)}</span>` : "");

  // ---- home ----
  function homePage(lang, t, v) {
    const h = t.home;
    const where = `${lang.code}:home`;
    const sec = site.pagePath(lang, "security");
    const hero = `<section class="hero"><div class="wrap">
<div class="hero-copy">
<a class="pill" href="#remote"><b>${esc(h.hero.pillTag)}</b>${esc(h.hero.pill)}</a>
<h1>${rich(h.hero.title, v, where)}</h1>
<p class="lede">${rich(h.hero.lede, v, where)}</p>
<div class="btn-row"><a class="btn primary" href="#download">${ICON.download}${esc(h.hero.download)}</a><a class="btn" href="${sec}">${ICON.shield}${esc(h.hero.security)}</a></div>
<ul class="trust">${h.hero.trust.map((x) => `<li>${ICON.check}${esc(x)}</li>`).join("")}</ul>
<div class="platforms">${h.download.platforms.map((p) => `<span>${ICON[PLATFORM_ICON[p.id]]}${esc(p.name)}</span>`).join("")}</div>
</div>
<div class="hero-stage">
${site.shot("hero", h.hero.imageAlt, t, { eager: true })}
<span class="float-chip c1">${ICON.lock}${esc(h.hero.chips[0])}</span>
<span class="float-chip c2">${ICON.desktop}${esc(h.hero.chips[1])}</span>
<span class="float-chip c3">${ICON.sync}${esc(h.hero.chips[2])}</span>
</div>
</div></section>`;

    const instead = `<section class="alt instead-band"><div class="wrap">
<p class="center muted">${esc(h.instead.title)}</p>
<div class="instead">${h.instead.items.map((x) => `<span class="old">${esc(x)}</span>`).join("")}<span class="arrow" aria-hidden="true"></span><span class="new">Submarine</span></div>
</div></section>`;

    const overview = `<section id="features"><div class="wrap">
${head2(h.overview, v, where)}
<div class="bento">${h.overview.tiles.map((x) => `<article class="tile reveal${x.wide ? " wide" : ""}"><div class="ico">${ICON[TILE_ICON[x.id]] || ICON.sparkles}</div><h3>${esc(x.title)}${badge(t, x)}</h3><p>${rich(x.text, v, where)}</p></article>`).join("\n")}</div>
</div></section>`;

    const rows = h.features.items.map((f, i) => `<article class="feature reveal${i % 2 ? " flip" : ""}" id="${f.id}">
<div class="feature-text"><h3>${esc(f.title)}${badge(t, f)}</h3><p>${rich(f.text, v, `${where}.features.${f.id}`)}</p>${f.points ? ticks(f.points, v, where) : ""}</div>
${site.shot(FEATURE_SHOT[f.id], f.alt, t)}
</article>`).join("\n");
    const features = `<section class="alt" id="tour"><div class="wrap">
${head2(h.features, v, where)}
<div class="features">
${rows}
</div>
</div></section>`;

    const r = h.remote;
    const remote = `<section id="remote"><div class="wrap">
${head2(r, v, where)}
<div class="showcase">
<div class="reveal">${site.shot("rdp", r.alt, t)}
<div class="duo">${site.shot("vnc", r.altVnc, t)}${site.shot("remoteSettings", r.altSettings, t)}</div></div>
<div class="points">${r.points.map((p) => `<div class="point reveal"><div class="ico">${ICON[POINT_ICON[p.id]] || ICON.sparkles}</div><div><h3>${esc(p.title)}</h3><p>${rich(p.text, v, where)}</p></div></div>`).join("")}</div>
</div>
${r.note ? `<p class="note reveal">${rich(r.note, v, where)}</p>` : ""}
</div></section>`;

    const pv = h.privacy;
    const privacyIcons = [ICON.lock, ICON.sync, ICON.eyeOff];
    const privacy = `<section class="alt" id="private"><div class="wrap">
<div class="glow-card reveal"><div class="inner">
${head2(pv, v, where)}
<div class="cards">${pv.items.map((c, i) => `<div class="card"><div class="ico">${privacyIcons[i] || ICON.shield}</div><h3>${esc(c.title)}</h3><p>${rich(c.text, v, where)}</p></div>`).join("")}</div>
<p class="more"><a href="${sec}">${esc(pv.link)} ${ICON.arrow}</a></p>
</div></div>
</div></section>`;

    const fr = h.free;
    const free = `<section id="free"><div class="wrap free">
<div class="reveal"><span class="kicker">${esc(fr.kicker)}</span><p class="big grad">${esc(fr.big)}</p><p>${rich(fr.text, v, where)}</p></div>
<ul class="checks reveal">${fr.items.map((x) => `<li>${ICON.check}<span>${rich(x, v, where)}</span></li>`).join("")}</ul>
</div></section>`;

    const dl = h.download;
    const download = `<section class="alt" id="download"><div class="wrap">
${head2(dl, v, where)}
<div class="platform-cards">${dl.platforms.map((p) => `<div class="platform-card reveal"><div class="ico">${ICON[PLATFORM_ICON[p.id]]}</div><h3>${esc(p.name)}</h3><p>${esc(p.detail)}</p><a class="btn primary" href="${RELEASES_URL}/latest">${ICON.download}${esc(dl.button)}</a></div>`).join("")}</div>
<p class="note">${rich(dl.betaNote, v, where)}</p>
<div class="verify-box" id="verify"><h3>${ICON.shieldCheck}${esc(dl.verifyTitle)}</h3>${ul(dl.verify, v, where)}</div>
</div></section>`;

    const faq = `<section id="faq"><div class="wrap">
${head2(h.faq, v, where)}
<div class="faq">${h.faq.items.map((q) => `<details><summary>${esc(q.q)}${ICON.plus}</summary><div><p>${rich(q.a, v, where)}</p></div></details>`).join("")}</div>
</div></section>`;

    const url = site.abs(site.pagePath(lang, "home"));
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
          author: AUTHOR,
          image: site.abs(site.assets["og/og.png"]),
          screenshot: ["hero", "terminal", "rdp", "phone"].map((s) => site.shotLargest(s)),
          featureList: h.overview.tiles.map((f) => f.title),
          sameAs: [REPO_URL],
        },
        {
          "@type": "FAQPage",
          "@id": `${url}#faq`,
          inLanguage: lang.hreflang,
          mainEntity: h.faq.items.map((q) => ({ "@type": "Question", name: q.q, acceptedAnswer: { "@type": "Answer", text: stripTags(fill(q.a, v, where)) } })),
        },
        { "@type": "WebSite", "@id": `${ORIGIN}/#site`, name: "Submarine", url: `${ORIGIN}/`, inLanguage: site.built.map((l) => l.hreflang) },
      ],
    };
    return site.page({ lang, page: "home", title: plain(h.title, v, where), description: plain(h.description, v, where), ld, main: [hero, instead, overview, features, remote, privacy, free, download, faq].join("\n"), t, v });
  }

  // ---- security ----
  function codeLinks(keys, t, v) {
    if (!keys || !keys.length) return "";
    return `<ul class="code-links">${keys.map((k) => {
      const c = v.code[k];
      return `<li><a href="${c.url}" rel="noopener">${ICON.code}${esc(t.security.codeLabels[k])}</a> <span>${esc(c.file.split("/").pop())}:${c.from}–${c.to}</span></li>`;
    }).join("")}</ul>`;
  }
  const twoCol = (o, v, where) => `<div class="two-col"><div class="cannot"><h3>${ICON.eyeOff}${esc(o.cannotTitle)}</h3>${ul(o.cannot, v, where)}</div><div class="can"><h3>${ICON.eye}${esc(o.canTitle)}</h3>${ul(o.can, v, where)}</div></div>`;

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
    const sections = [
      { id: "summary", toc: s.summaryTitle, html: `<div class="summary-box"><h2>${esc(s.summaryTitle)}</h2>${ul(s.summary, v, where)}</div>${twoCol(s.canCannot, v, where)}` },
      { id: "flow", toc: d.title, html: `<h2>${esc(d.title)}</h2>
<figure class="flow">
<div class="flow-zone"><span>${esc(d.never)}</span><div class="flow-row"><span class="chip">${esc(d.password)}</span><span class="arrow"></span><span class="chip op">${esc(d.kdf)}</span><span class="arrow"></span><span class="chip">${esc(d.key)}</span><span class="arrow"></span><span class="chip op">${esc(d.encrypt)}</span><span class="arrow"></span><span class="chip">${esc(d.items)}</span></div></div>
<div class="flow-row"><span class="arrow"></span><span class="chip op">${esc(d.https)}</span><span class="arrow"></span><span class="chip server">${esc(d.server)}</span><span class="muted">${esc(d.stores)}</span></div>
<figcaption>${esc(d.caption)}</figcaption>
</figure>` },
      ...s.sections.map((x) => ({
        id: x.id,
        toc: x.title,
        html: `<h2>${esc(x.title)}</h2>
${(x.paragraphs || []).map((p) => `<p>${rich(p, v, `${where}.${x.id}`)}</p>`).join("\n")}
${x.example ? `<p>${rich(x.example, v, where)} <span class="muted">(${esc(t.common.example)})</span></p>${example}` : ""}${x.cannot ? twoCol(x, v, where) : ""}${codeLinks(x.code, t, v)}`,
      })),
      { id: "verify", toc: s.verify.title, html: `<h2>${esc(s.verify.title)}</h2>
<p>${rich(s.verify.lede, v, where)}</p>
<ol class="steps">${s.verify.items.map((x) => `<li><h3>${esc(x.title)}</h3><p>${rich(x.text, v, where)}</p></li>`).join("")}</ol>
<h3>${esc(s.verify.paramsTitle)}</h3>
<div class="table-wrap"><table class="params"><tbody>${s.verify.params.map(([k, val]) => `<tr><td>${esc(k)}</td><td>${rich(val, v, where)}</td></tr>`).join("")}</tbody></table></div>` },
      { id: "limits", toc: s.limits.title, html: `<h2>${esc(s.limits.title)}</h2>${ul(s.limits.items, v, where)}` },
    ];
    const main = site.docPage({ lang, page: "security", t, heading: s.heading, lede: rich(s.lede, v, where), sections });
    const ld = [site.crumbs(lang, "security", s.heading), { "@context": "https://schema.org", "@type": "TechArticle", headline: s.heading, inLanguage: lang.hreflang, dateModified: date, author: AUTHOR, about: { "@id": `${site.abs(site.pagePath(lang, "home"))}#app` } }];
    return site.page({ lang, page: "security", title: plain(s.title, v, where), description: plain(s.description, v, where), ld, main, t, v });
  }

  // ---- privacy ----
  function privacyPage(lang, t, v) {
    const p = t.privacy;
    const where = `${lang.code}:privacy`;
    const block = (sec) => ({
      id: sec.id,
      toc: sec.title,
      html: `<h2>${esc(sec.title)}</h2>
${(sec.paragraphs || []).map((x) => `<p>${rich(x, v, where)}</p>`).join("\n")}
${sec.list ? ul(sec.list, v, where) : ""}
${sec.after ? `<p>${rich(sec.after, v, where)}</p>` : ""}`,
    });
    const tb = p.table;
    const sections = [
      { id: "short", toc: p.shortTitle, html: `<div class="summary-box"><h2>${esc(p.shortTitle)}</h2>${ul(p.short, v, where)}</div>` },
      ...p.sections.map(block),
      { id: tb.id, toc: tb.title, html: `<h2>${esc(tb.title)}</h2>
<p>${rich(tb.intro, v, where)}</p>
<div class="table-wrap"><table><thead><tr>${tb.head.map((x) => `<th scope="col">${esc(x)}</th>`).join("")}</tr></thead><tbody>${tb.rows.map((r) => `<tr>${r.map((c) => `<td>${rich(c, v, where)}</td>`).join("")}</tr>`).join("")}</tbody></table></div>
${tb.after.map((x) => `<p>${rich(x, v, where)}</p>`).join("\n")}` },
      ...p.after.map(block),
    ];
    const main = site.docPage({ lang, page: "privacy", t, heading: p.heading, lede: rich(p.intro, v, where), meta: esc(fill(t.common.updated, v, where)), sections });
    return site.page({ lang, page: "privacy", title: plain(p.title, v, where), description: plain(p.description, v, where), ld: [site.crumbs(lang, "privacy", p.heading)], main, t, v });
  }

  // ---- delete ----
  function deletePage(lang, t, v) {
    const d = t.delete;
    const where = `${lang.code}:delete`;
    const messages = { ...d.errors, working: d.working, done: d.done };
    const main = `<div class="doc-hero"><div class="wrap">
${site.translationNote(lang, "delete", t)}
<h1>${esc(d.heading)}</h1>
<p class="lede">${rich(d.lede, v, where)}</p>
</div></div>
<div class="wrap delete-grid">
<div class="doc-body">
<section><h2>${esc(d.whatTitle)}</h2>${ul(d.what, v, where)}
<h3>${esc(d.staysTitle)}</h3>${ul(d.stays, v, where)}</section>
<section><h2>${esc(d.inAppTitle)}</h2><p>${rich(d.inApp, v, where)}</p></section>
<section><h2>${esc(d.helpTitle)}</h2><p>${rich(d.help, v, where)}</p></section>
</div>
<div class="glow-card form-card" id="form"><div class="inner">
<h2>${esc(d.formTitle)}</h2>
<p class="muted">${rich(d.formLede, v, where)}</p>
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
<script type="application/json" id="delete-text">${json(messages)}</script>
</div></div>
</div>`;
    return site.page({ lang, page: "delete", title: plain(d.title, v, where), description: plain(d.description, v, where), ld: [site.crumbs(lang, "delete", d.heading)], main, t, v, scripts: `<script src="${site.assets["assets/delete.js"]}" defer></script>\n` });
  }

  function notFoundPage(t, v) {
    const lang = LANGS[0];
    const n = t.notFound;
    const main = `<div class="wrap nf"><div>
<p class="code grad">404</p>
<h1>${esc(n.heading)}</h1>
<p class="muted">${esc(n.text)}</p>
<p><a class="btn primary" href="/">${esc(n.home)}</a></p>
<p class="foot-langs">${site.built.map((l) => `<a href="${site.pagePath(l, "home")}" hreflang="${l.hreflang}" lang="${l.html}">${esc(l.name)}</a>`).join("")}</p>
</div></div>`;
    return site.page({ lang, page: "home", title: n.title, description: n.text, ld: null, main, t, v, index: false });
  }

  function llms(v) {
    const t = site.en;
    const list = (xs) => xs.map((x) => `- ${stripTags(fill(x, v, "llms"))}`).join("\n");
    return `# Submarine

> ${stripTags(fill(t.home.description, v, "llms"))}

Submarine is a free, open-source (MIT) remote-access client by Sina Xhpm: SSH terminal, two-pane SFTP, port forwarding, folder mirror, a built-in RDP client, VNC and Telnet, for Windows 10/11, macOS (Apple Silicon: M1 and later), Linux and Android 7.0+. Built with Rust and Tauri. No ads, no analytics, no paid plan; the optional encrypted sync service is free within limits (${FACTS.limitMb} MB of encrypted data, ${FACTS.limitItems.toLocaleString("en")} items and ${FACTS.limitProfiles} profiles per account).

## Features

${t.home.overview.tiles.map((x) => `- ${x.title}: ${stripTags(fill(x.text, v, "llms"))}`).join("\n")}

## Pages

- [Home](${ORIGIN}/): features, screenshots, downloads, questions
- [How your data is protected](${ORIGIN}/security/): what is encrypted and how, what the sync server can and cannot see, how to check it in the code
- [Privacy policy](${ORIGIN}/privacy/): what the app and the sync service keep, and for how long
- [Delete your account](${ORIGIN}/delete-account/)
- Translations: ${site.built.filter((l) => l.code !== "en").map((l) => `[${l.name}](${site.abs(site.pagePath(l, "home"))})`).join(", ")}

## Security, in short

${list(t.security.summary)}

## Links

- Source code: ${REPO_URL}
- Releases and downloads: ${RELEASES_URL}
- Issues: ${ISSUES_URL}
- Author: https://sinaxhpm.com
- Sister app: Echoes, network tools (${ECHOES}/)

When citing Submarine, please link ${ORIGIN}/ or the GitHub repository. The name "Submarine" and its logo are not covered by the MIT license; please don't ship a fork under the Submarine name.
`;
  }

  for (const lang of site.built) {
    const t = site.str[lang.code];
    const v = valuesFor(lang);
    const base = lang.code === "en" ? "" : `${lang.code}/`;
    site.file(`${base}index.html`, homePage(lang, t, v));
    site.file(`${base}security/index.html`, securityPage(lang, t, v));
    site.file(`${base}privacy/index.html`, privacyPage(lang, t, v));
    site.file(`${base}delete-account/index.html`, deletePage(lang, t, v));
  }
  const vEn = valuesFor(LANGS[0]);
  site.file("404.html", notFoundPage(site.en, vEn));
  site.file("sitemap.xml", site.sitemap(["home", "security", "privacy", "delete"]));
  site.file("robots.txt", site.robots(["/account/", "/api/"]));
  site.file("llms.txt", llms(vEn));
  site.file("manifest.webmanifest", site.manifest(stripTags(site.en.home.description)));
  site.file(".well-known/security.txt", site.securityTxt(`${ISSUES_URL}/new`, `${ORIGIN}/security/`));
  for (const [k, c] of Object.entries(code)) console.log(`code ${k}: ${c.file}:${c.from}-${c.to}`);
  console.log(`code links at ${commit.slice(0, 7)}`);
  return site.check();
}
