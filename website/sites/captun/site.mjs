// captun.sinaxhpm.com: the home page of Captun, the Wi-Fi hotspot and internet-sharing app (its source is
// github.com/SinaXhpm/captun), in every language that has strings in i18n/. The app itself is in English.
// Captun has no release yet and no licence file: the page links the source, offers no download, and never calls it
// open source. What it says follows the Captun session's list of what is safe to claim (2026-11-13): the routing
// engine is experimental, macOS has not run yet, and HTTPS inspection, the sign-in page, accounts and a background
// service are not claimed at all.
import path from "node:path";
import { fileURLToPath } from "node:url";
import { AUTHOR, ICON, LANGS, Site, esc, fill, plain, rich, stripTags } from "../../engine.mjs";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const ORIGIN = "https://captun.sinaxhpm.com";
// The repository itself, never a commit or a file at a fixed commit (its history may be rewritten).
const REPO_URL = "https://github.com/SinaXhpm/captun";
const ISSUES_URL = `${REPO_URL}/issues`;
const BUILD_URL = `${REPO_URL}#build-from-source`;
const SUBMARINE = "https://submarine.sinaxhpm.com";
const ECHOES = "https://echoes.sinaxhpm.com";

// Screenshots: img/<name>-<width>.webp, made by tools/images.sh captun from the app's own pages drawn with
// made-up data at 960 x 620 (1.5x).
const SHOTS = {
  home: { widths: [960, 1440], ratio: [1440, 930], sizes: "(max-width: 900px) 92vw, 860px" },
  ready: { widths: [720, 1440], ratio: [1440, 790], sizes: "(max-width: 960px) 100vw, 640px" },
  devices: { widths: [720, 1440], ratio: [1440, 480], sizes: "(max-width: 900px) 100vw, 640px" },
  outbound: { widths: [720, 1440], ratio: [1440, 620], sizes: "(max-width: 900px) 100vw, 640px" },
  rules: { widths: [720, 1440], ratio: [1440, 540], sizes: "(max-width: 900px) 100vw, 640px" },
  dns: { widths: [720, 1440], ratio: [1440, 690], sizes: "(max-width: 900px) 100vw, 640px" },
  usage: { widths: [720, 1440], ratio: [1440, 930], sizes: "(max-width: 900px) 100vw, 640px" },
  // close-ups of what the next version adds, all 8:5
  ...Object.fromEntries(["nextQr", "nextKinds", "nextPause", "nextBackup", "nextCsv", "nextAlerts"].map((n) => [n, { widths: [480, 960], ratio: [960, 600], sizes: "(max-width: 700px) 92vw, (max-width: 1020px) 46vw, 360px" }])),
};
const USE_ICON = { vpn: "lock", usb: "plug", rules: "shield", watch: "users" };
const SHARE_ICON = { wifi: "wifi", cable: "network", usb: "plug", reach: "ban" };
const DEVICE_ICON = { phone: "phone", tv: "desktop", laptop: "laptop", tablet: "tablet" };
const OUT_ICON = { direct: "globe", vpn: "lock", proxy: "server" };
const NEXT_SHOT = { qr: "nextQr", kinds: "nextKinds", pause: "nextPause", backup: "nextBackup", csv: "nextCsv", alerts: "nextAlerts" };
const PRIVACY_ICON = [ICON.eyeOff, ICON.lock, ICON.tunnel];
const PLATFORM_ICON = { windows: "windows", linux: "box", macos: "laptop" };

export default function build({ out, date }) {
  const site = new Site({
    id: "captun",
    origin: ORIGIN,
    name: "Captun",
    src: HERE,
    out,
    date,
    langs: LANGS.map((l) => l.code),
    slugs: { home: "" },
    themeColor: "#07111b",
    ogAlt: "Captun: share your computer's internet, VPN or proxy with every device",
    nav: (lang, page, t) => [
      [`${site.pagePath(lang, "home")}#tour`, t.nav.features, null],
      [`${site.pagePath(lang, "home")}#how`, t.nav.how, null],
      [`${site.pagePath(lang, "home")}#platforms`, t.nav.platforms, null],
      [`${site.pagePath(lang, "home")}#faq`, t.nav.faq, null],
      [REPO_URL, t.nav.github, null, "code"],
    ],
    cta: (lang, t) => [`${site.pagePath(lang, "home")}#source`, t.nav.get],
    footer: (lang, page, t) => {
      const home = site.pagePath(lang, "home");
      const f = t.footer;
      const sisterPath = lang.code === "en" ? "/" : `/${lang.code}/`;
      const cols = [
        { title: f.product, links: [[`${home}#tour`, t.nav.features], [`${home}#how`, t.nav.how], [`${home}#platforms`, t.nav.platforms], [`${home}#source`, t.nav.get], [`${home}#faq`, t.nav.faq]] },
        { title: f.project, links: [[REPO_URL, t.nav.github], [ISSUES_URL, f.issues], ["https://sinaxhpm.com", f.author]] },
        { title: f.more, links: [[`${SUBMARINE}${sisterPath}`, "Submarine"], [`${ECHOES}${sisterPath}`, "Echoes"]] },
      ];
      const sister = `<a class="sister" href="${ECHOES}${sisterPath}" hreflang="${lang.hreflang}"><img src="${site.assets["assets/echoes.png"]}" width="44" height="44" alt=""><span><b>Echoes</b><span>${esc(f.sister)}</span></span></a>`;
      return site.footerFrame(lang, page, t, cols, sister);
    },
  });

  site.putShared();
  site.asset(path.join(HERE, "img", "logo-64.png"), "logo.png");
  site.asset(path.join(HERE, "img", "echoes-88.png"), "echoes.png");
  site.asset(path.join(HERE, "img", "icon-192.png"), "icon-192.png");
  site.asset(path.join(HERE, "img", "icon-512.png"), "icon-512.png");
  site.asset(path.join(HERE, "img", "og.png"), "og.png", "og");
  site.putShots(SHOTS);
  site.copy(path.join(HERE, "img", "favicon.ico"), "favicon.ico");
  site.copy(path.join(HERE, "img", "apple-touch-icon.png"), "apple-touch-icon.png");
  site.copy(path.join(HERE, "img", "og.png"), "og.png");

  const valuesFor = () => ({ repoUrl: REPO_URL, issuesUrl: ISSUES_URL, buildUrl: BUILD_URL });

  const ul = (xs, v, where) => `<ul>${xs.map((x) => `<li>${rich(x, v, where)}</li>`).join("")}</ul>`;
  const ticks = (xs, v, where) => `<ul class="ticks">${xs.map((x) => `<li>${ICON.check}<span>${rich(x, v, where)}</span></li>`).join("")}</ul>`;
  const head2 = (h, v, where) => `<div class="section-head reveal">${h.kicker ? `<span class="kicker">${esc(h.kicker)}</span>` : ""}<h2>${rich(h.title, v, where)}</h2>${h.lede ? `<p>${rich(h.lede, v, where)}</p>` : ""}</div>`;

  function homePage(lang, t, v) {
    const h = t.home;
    const where = `${lang.code}:home`;
    const hero = `<section class="hero"><div class="wrap">
<div class="hero-copy">
<a class="pill" href="#source"><b>${esc(h.hero.pillTag)}</b>${esc(h.hero.pill)}</a>
<h1>${rich(h.hero.title, v, where)}</h1>
<p class="lede">${rich(h.hero.lede, v, where)}</p>
<div class="btn-row"><a class="btn primary" href="${REPO_URL}">${ICON.code}${esc(h.hero.source)}</a><a class="btn" href="#how">${ICON.route}${esc(h.hero.how)}</a></div>
<ul class="trust">${h.hero.trust.map((x) => `<li>${ICON.check}${esc(x)}</li>`).join("")}</ul>
<div class="platforms">${[["windows", "Windows"], ["box", "Linux"], ["laptop", h.hero.macos]].map(([i, n]) => `<span>${ICON[i]}${esc(n)}</span>`).join("")}</div>
</div>
<div class="hero-stage narrow">
${site.shot("home", h.hero.imageAlt, t, { eager: true })}
<span class="float-chip c1">${ICON.lock}${esc(h.hero.chips[0])}</span>
<span class="float-chip c2">${ICON.plug}${esc(h.hero.chips[1])}</span>
<span class="float-chip c3">${ICON.shield}${esc(h.hero.chips[2])}</span>
</div>
</div></section>`;

    const instead = `<section class="alt instead-band"><div class="wrap">
<p class="center muted">${esc(h.instead.title)}</p>
<div class="instead">${h.instead.items.map((x) => `<span class="old">${esc(x)}</span>`).join("")}<span class="arrow" aria-hidden="true"></span><span class="new">Captun</span></div>
</div></section>`;

    const us = h.uses;
    const uses = `<section id="uses"><div class="wrap">
${head2(us, v, where)}
<div class="cards">${us.items.map((c) => `<div class="card reveal"><div class="ico">${ICON[USE_ICON[c.id]]}</div><h3>${esc(c.title)}</h3><p>${rich(c.text, v, where)}</p></div>`).join("")}</div>
</div></section>`;

    const hw = h.how;
    const chips = (xs, icons) => xs.map((x) => `<span class="chip">${ICON[icons[x.id]] || ""}<span>${esc(x.name)}${x.detail ? `<small>${esc(x.detail)}</small>` : ""}</span></span>`).join("");
    const how = `<section class="alt" id="how"><div class="wrap">
${head2(hw, v, where)}
<figure class="route reveal">
<div class="route-col"><b>${esc(hw.devicesTitle)}</b>${chips(hw.devices, DEVICE_ICON)}</div>
<div class="route-link"><span>${esc(hw.links)}</span></div>
<div class="route-hub"><img src="${site.assets["assets/icon-192.png"]}" width="72" height="72" alt=""><h3>Captun</h3><p>${esc(hw.hub)}</p></div>
<div class="route-link"><span>${esc(hw.choose)}</span></div>
<div class="route-col"><b>${esc(hw.outTitle)}</b>${chips(hw.outbounds, OUT_ICON)}</div>
<figcaption class="visually-hidden">${esc(hw.caption)}</figcaption>
</figure>
<ol class="steps route-steps">${hw.steps.map((s) => `<li class="reveal"><h3>${esc(s.title)}</h3><p>${rich(s.text, v, where)}</p></li>`).join("")}</ol>
<p class="note center-note">${rich(hw.note, v, where)}</p>
</div></section>`;

    const sh = h.share;
    const share = `<section id="share"><div class="wrap">
${head2(sh, v, where)}
<div class="showcase">
<div class="reveal">${site.shot("ready", sh.alt, t)}</div>
<div class="points">${sh.points.map((p) => `<div class="point reveal"><div class="ico">${ICON[SHARE_ICON[p.id]]}</div><div><h3>${esc(p.title)}</h3><p>${rich(p.text, v, where)}</p></div></div>`).join("")}</div>
</div>
</div></section>`;

    const rows = h.features.items.map((f, i) => `<article class="feature reveal${i % 2 ? " flip" : ""}" id="${f.id}">
<div class="feature-text"><h3>${esc(f.title)}${f.badge ? `<span class="badge">${esc(f.badge)}</span>` : ""}</h3><p>${rich(f.text, v, where)}</p>${ticks(f.points, v, where)}</div>
${site.shot(f.id, f.alt, t)}
</article>`).join("\n");
    const features = `<section class="alt" id="tour"><div class="wrap">
${head2(h.features, v, where)}
<div class="features">
${rows}
</div>
<p class="note center-note">${rich(h.features.note, v, where)}</p>
</div></section>`;

    // Built and tested, not released: the next version (the Captun session, 2026-11-14). Each says whether it needs
    // the experimental routing engine.
    const nx = h.next;
    const next = `<section id="next"><div class="wrap">
${head2(nx, v, where)}
<div class="next-grid">${nx.items.map((x) => `<article class="next-card reveal">${site.shot(NEXT_SHOT[x.id], x.alt, t)}<h3>${esc(x.title)}</h3><p>${rich(x.text, v, where)}</p>${x.routing ? `<span class="badge">${esc(nx.routing)}</span>` : ""}</article>`).join("")}</div>
<p class="note center-note">${rich(nx.note, v, where)}</p>
</div></section>`;

    const pv = h.privacy;
    const privacy = `<section id="private"><div class="wrap">
<div class="glow-card reveal"><div class="inner">
${head2(pv, v, where)}
<div class="cards">${pv.items.map((c, i) => `<div class="card"><div class="ico">${PRIVACY_ICON[i]}</div><h3>${esc(c.title)}</h3><p>${rich(c.text, v, where)}</p></div>`).join("")}</div>
</div></div>
</div></section>`;

    const pl = h.platforms;
    const platforms = `<section class="alt" id="platforms"><div class="wrap">
${head2(pl, v, where)}
<div class="platform-cards three">${pl.items.map((p) => `<div class="platform-card reveal"><div class="ico">${ICON[PLATFORM_ICON[p.id]]}</div><h3>${esc(p.name)}${p.badge ? `<span class="badge">${esc(p.badge)}</span>` : ""}</h3><p>${rich(p.text, v, where)}</p></div>`).join("")}</div>
<p class="note center-note">${rich(pl.note, v, where)}</p>
</div></section>`;

    const sc = h.source;
    const source = `<section id="source"><div class="wrap">
${head2(sc, v, where)}
<div class="btn-row center-row"><a class="btn primary" href="${REPO_URL}">${ICON.code}${esc(sc.repo)}</a><a class="btn" href="${BUILD_URL}">${ICON.book}${esc(sc.build)}</a></div>
<ol class="steps route-steps">${sc.steps.map((s) => `<li class="reveal"><h3>${esc(s.title)}</h3><p>${rich(s.text, v, where)}</p></li>`).join("")}</ol>
<div class="verify-box">${ul(sc.notes, v, where)}</div>
</div></section>`;

    const faq = `<section class="alt" id="faq"><div class="wrap">
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
          name: "Captun",
          applicationCategory: "UtilitiesApplication",
          applicationSubCategory: "Wi-Fi hotspot and internet sharing",
          operatingSystem: "Windows, Linux",
          description: stripTags(fill(h.description, v, where)),
          url,
          inLanguage: "en",
          author: AUTHOR,
          image: site.abs(site.assets["og/og.png"]),
          screenshot: ["home", "devices", "outbound", "usage"].map((s) => site.shotLargest(s)),
          featureList: [...h.uses.items.map((x) => x.title), ...h.features.items.map((x) => x.title)],
          sameAs: [REPO_URL],
        },
        {
          "@type": "FAQPage",
          "@id": `${url}#faq`,
          inLanguage: lang.hreflang,
          mainEntity: h.faq.items.map((q) => ({ "@type": "Question", name: q.q, acceptedAnswer: { "@type": "Answer", text: stripTags(fill(q.a, v, where)) } })),
        },
        { "@type": "WebSite", "@id": `${ORIGIN}/#site`, name: "Captun", url: `${ORIGIN}/`, inLanguage: site.built.map((l) => l.hreflang) },
      ],
    };
    return site.page({ lang, page: "home", title: plain(h.title, v, where), description: plain(h.description, v, where), ld, main: [hero, instead, uses, how, share, features, next, privacy, platforms, source, faq].join("\n"), t, v });
  }

  function notFoundPage(t) {
    const lang = LANGS[0];
    const n = t.notFound;
    const main = `<div class="wrap nf"><div>
<p class="code grad">404</p>
<h1>${esc(n.heading)}</h1>
<p class="muted">${esc(n.text)}</p>
<p><a class="btn primary" href="/">${esc(n.home)}</a></p>
<p class="foot-langs">${site.built.map((l) => `<a href="${site.pagePath(l, "home")}" hreflang="${l.hreflang}" lang="${l.html}">${esc(l.name)}</a>`).join("")}</p>
</div></div>`;
    return site.page({ lang, page: "home", title: n.title, description: n.text, ld: null, main, t, v: valuesFor(), index: false });
  }

  function llms(v) {
    const t = site.en;
    const h = t.home;
    return `# Captun

> ${stripTags(fill(h.description, v, "llms"))}

Captun (captain + TUN) is a Wi-Fi hotspot and internet-sharing app for Windows and Linux by Sina Xhpm, written in Rust with a Tauri desktop app; macOS is in progress (the code builds but has not run on a Mac yet). There is no release yet: it is built from source (${REPO_URL}). The source is public; it has no licence file. The app is in English. There is no Android or iOS app: phones and other devices connect to Captun.

## What it does

${h.uses.items.map((x) => `- ${x.title}: ${stripTags(fill(x.text, v, "llms"))}`).join("\n")}

## Features

${h.features.items.map((x) => `- ${x.title}${x.badge ? ` (${x.badge.toLowerCase()})` : ""}: ${stripTags(fill(x.text, v, "llms"))}`).join("\n")}

## Coming in the next version (built and tested, not released yet)

${h.next.items.map((x) => `- ${x.title}${x.routing ? " (needs the routing engine)" : ""}: ${stripTags(fill(x.text, v, "llms"))}`).join("\n")}

Per-device outbounds, rules, speed limits, data allowances, the usage history and live speeds need "Route devices through Captun", its own routing engine, which is experimental. Captun routes traffic; it doesn't encrypt or anonymise it (the VPN or proxy a user adds does).

## Pages

- [Home](${ORIGIN}/): what Captun does, how it works, features, platforms, building it, questions
- Translations: ${site.built.filter((l) => l.code !== "en").map((l) => `[${l.name}](${site.abs(site.pagePath(l, "home"))})`).join(", ")}

## Links

- Source code: ${REPO_URL}
- Issues: ${ISSUES_URL}
- Author: https://sinaxhpm.com
- Also by the author: Submarine, an SSH, SFTP, RDP, VNC and Telnet client (${SUBMARINE}/), and Echoes, 20 network tools in one app (${ECHOES}/)
`;
  }

  for (const lang of site.built) {
    const t = site.str[lang.code];
    const base = lang.code === "en" ? "" : `${lang.code}/`;
    site.file(`${base}index.html`, homePage(lang, t, valuesFor()));
  }
  site.file("404.html", notFoundPage(site.en));
  site.file("sitemap.xml", site.sitemap(["home"]));
  site.file("robots.txt", site.robots());
  site.file("llms.txt", llms(valuesFor()));
  site.file("manifest.webmanifest", site.manifest(stripTags(site.en.home.description)));
  site.file(".well-known/security.txt", site.securityTxt(`${ISSUES_URL}/new`));
  return site.check();
}
