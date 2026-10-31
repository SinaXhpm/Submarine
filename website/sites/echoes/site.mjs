// echoes.sinaxhpm.com: the home page and the privacy policy of Echoes, the
// network toolkit (its source is github.com/SinaXhpm/Echoes), in every
// language that has strings in i18n/. The app itself is in English.
import path from "node:path";
import { fileURLToPath } from "node:url";
import { AUTHOR, ICON, LANGS, Site, esc, fill, plain, rich, stripTags } from "../../engine.mjs";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const ORIGIN = "https://echoes.sinaxhpm.com";
const REPO_URL = "https://github.com/SinaXhpm/Echoes";
const RELEASES_URL = `${REPO_URL}/releases`;
const ISSUES_URL = `${REPO_URL}/issues`;
const POLICY_URL = `${REPO_URL}/blob/master/PRIVACY.md`;
const APP_STORE = "https://apps.apple.com/app/id6824582968";
const SUBMARINE = "https://submarine.sinaxhpm.com";
// The date at the top of PRIVACY.md, which this site's policy follows word for word.
const POLICY_DATE = "2026-10-21";

// Where each download button goes. GitHub's "latest" link works for every file
// whose name has no version in it; the .rpm's has, so it goes to the release.
const LATEST = `${RELEASES_URL}/latest/download`;
const LINKS = {
  ios: APP_STORE,
  mac: `${APP_STORE}?platform=mac`,
  zip: `${LATEST}/Echoes-Windows-x64.zip`,
  arm64: `${LATEST}/Echoes-osx-arm64.dmg`,
  x64: `${LATEST}/Echoes-osx-x64.dmg`,
  appimage: `${LATEST}/Echoes-Linux-x64.AppImage`,
  deb: `${LATEST}/echoes.deb`,
  rpm: `${RELEASES_URL}/latest`,
  apk: `${LATEST}/Echoes-Android.apk`,
};

// Screenshots: img/<name>-<width>.webp, made by tools/images.sh echoes. The
// desktop ones are the app's window drawn at 2x by its own store-shot builds.
const SHOTS = {
  hero: { widths: [860, 1290, 1720], ratio: [1720, 1350], sizes: "(max-width: 900px) 92vw, 860px" },
  heroPhone: { widths: [360, 720], ratio: [720, 1564], sizes: "(max-width: 900px) 26vw, 230px", cls: "phone" },
  ping: { widths: [860, 1720], ratio: [1720, 1350], sizes: "(max-width: 900px) 100vw, 640px" },
  speed: { widths: [860, 1720], ratio: [1720, 1350], sizes: "(max-width: 900px) 100vw, 640px" },
  monitor: { widths: [860, 1720], ratio: [1720, 1350], sizes: "(max-width: 900px) 100vw, 640px" },
  share: { widths: [860, 1720], ratio: [1720, 1350], sizes: "(max-width: 900px) 100vw, 640px" },
  dns: { widths: [860, 1720], ratio: [1720, 1350], sizes: "(max-width: 900px) 100vw, 640px" },
  tls: { widths: [860, 1720], ratio: [1720, 1350], sizes: "(max-width: 900px) 100vw, 640px" },
  curl: { widths: [860, 1720], ratio: [1720, 1350], sizes: "(max-width: 900px) 100vw, 640px" },
  strings: { widths: [860, 1720], ratio: [1720, 1350], sizes: "(max-width: 900px) 100vw, 640px" },
  doctorSni: { widths: [860], ratio: [860, 653], sizes: "(max-width: 960px) 100vw, 640px" },
  shareBrowser: { widths: [390], ratio: [390, 844], sizes: "(max-width: 900px) 24vw, 170px", cls: "phone" },
  iphone: { widths: [360, 720], ratio: [720, 1564], sizes: "(max-width: 700px) 60vw, 250px", cls: "phone" },
  ipad: { widths: [520, 1040], ratio: [1040, 1387], sizes: "(max-width: 700px) 86vw, 400px", cls: "tablet" },
  android: { widths: [412], ratio: [412, 880], sizes: "(max-width: 700px) 60vw, 250px", cls: "phone" },
  scanner: { widths: [860], ratio: [860, 653], sizes: "(max-width: 900px) 100vw, 370px" },
  ssh: { widths: [860], ratio: [860, 653], sizes: "(max-width: 900px) 100vw, 370px" },
  cloudflare: { widths: [860], ratio: [860, 653], sizes: "(max-width: 900px) 100vw, 370px" },
};
const TOOL_ICON = {
  ping: "wifi", doctor: "stethoscope", dns: "globe", scanner: "search", checkhost: "radar", monitor: "activity", network: "network", speed: "gauge",
  curl: "terminal", tls: "lock", ipinfo: "pin", cloudflare: "cloud", telegram: "send",
  ssh: "code", socket: "plug", webserver: "server", proxy: "tunnel",
  strings: "sliders", notes: "note", backup: "archive",
};
const DOCTOR_ICON = { network: "wifi", dns: "globe", sni: "shield", outside: "radar", dpi: "zap", report: "note" };
const PLATFORM_ICON = { apple: "phone", windows: "windows", macos: "laptop", linux: "box", android: "phone" };

export default function build({ out, date }) {
  const site = new Site({
    id: "echoes",
    origin: ORIGIN,
    name: "Echoes",
    src: HERE,
    out,
    date,
    langs: LANGS.map((l) => l.code),
    slugs: { home: "", privacy: "privacy/" },
    themeColor: "#04070f",
    ogAlt: "Echoes: 20 network tools in one free app",
    nav: (lang, page, t) => [
      [`${site.pagePath(lang, "home")}#doctor`, t.nav.doctor, null],
      [`${site.pagePath(lang, "home")}#tools`, t.nav.tools, null],
      [`${site.pagePath(lang, "home")}#devices`, t.nav.devices, null],
      [site.pagePath(lang, "privacy"), t.nav.privacy, "privacy"],
      [REPO_URL, t.nav.github, null, "code"],
    ],
    cta: (lang, t) => [`${site.pagePath(lang, "home")}#download`, t.nav.download],
    footer: (lang, page, t) => {
      const home = site.pagePath(lang, "home");
      const f = t.footer;
      const cols = [
        { title: f.product, links: [[`${home}#doctor`, t.nav.doctor], [`${home}#tools`, t.nav.tools], [`${home}#devices`, t.nav.devices], [`${home}#download`, t.nav.download], [`${home}#faq`, f.faq]] },
        { title: f.trust, links: [[site.pagePath(lang, "privacy"), f.privacyPolicy], [APP_STORE, f.appStore], [RELEASES_URL, f.releases]] },
        { title: f.project, links: [[REPO_URL, t.nav.github], [ISSUES_URL, f.issues], ["https://sinaxhpm.com", f.author]] },
      ];
      const sisterPath = lang.code === "en" ? "/" : `/${lang.code}/`;
      const sister = `<a class="sister" href="${SUBMARINE}${sisterPath}" hreflang="${lang.hreflang}"><img src="${site.assets["assets/submarine.png"]}" width="44" height="44" alt=""><span><b>Submarine</b><span>${esc(f.sister)}</span></span></a>`;
      return site.footerFrame(lang, page, t, cols, sister);
    },
  });

  site.putShared();
  site.asset(path.join(HERE, "img", "logo-64.png"), "logo.png");
  site.asset(path.join(HERE, "img", "submarine-88.png"), "submarine.png");
  site.asset(path.join(HERE, "img", "icon-192.png"), "icon-192.png");
  site.asset(path.join(HERE, "img", "icon-512.png"), "icon-512.png");
  site.asset(path.join(HERE, "img", "og.png"), "og.png", "og");
  site.putShots(SHOTS);
  site.copy(path.join(HERE, "img", "favicon.ico"), "favicon.ico");
  site.copy(path.join(HERE, "img", "apple-touch-icon.png"), "apple-touch-icon.png");
  site.copy(path.join(HERE, "img", "og.png"), "og.png");

  const valuesFor = (lang) => {
    const d = new Intl.DateTimeFormat(lang.locale, { dateStyle: "long", timeZone: "UTC" });
    return {
      date: d.format(new Date(`${POLICY_DATE}T00:00:00Z`)),
      repoUrl: REPO_URL,
      issuesUrl: ISSUES_URL,
      policyUrl: POLICY_URL,
      privacyUrl: site.pagePath(lang, "privacy"),
    };
  };

  const ul = (xs, v, where) => `<ul>${xs.map((x) => `<li>${rich(x, v, where)}</li>`).join("")}</ul>`;
  const ticks = (xs, v, where) => `<ul class="ticks">${xs.map((x) => `<li>${ICON.check}<span>${rich(x, v, where)}</span></li>`).join("")}</ul>`;
  const head2 = (h, v, where) => `<div class="section-head reveal">${h.kicker ? `<span class="kicker">${esc(h.kicker)}</span>` : ""}<h2>${rich(h.title, v, where)}</h2>${h.lede ? `<p>${rich(h.lede, v, where)}</p>` : ""}</div>`;

  function homePage(lang, t, v) {
    const h = t.home;
    const where = `${lang.code}:home`;
    const hero = `<section class="hero"><div class="wrap">
<div class="hero-copy">
<a class="pill" href="#download"><b>${esc(h.hero.pillTag)}</b>${esc(h.hero.pill)}</a>
<h1>${rich(h.hero.title, v, where)}</h1>
<p class="lede">${rich(h.hero.lede, v, where)}</p>
<div class="btn-row"><a class="btn primary" href="${APP_STORE}">${ICON.store}${esc(h.hero.appStore)}</a><a class="btn" href="#download">${ICON.download}${esc(h.hero.download)}</a></div>
<ul class="trust">${h.hero.trust.map((x) => `<li>${ICON.check}${esc(x)}</li>`).join("")}</ul>
<div class="platforms">${[["phone", "iPhone"], ["tablet", "iPad"], ["laptop", "Mac"], ["windows", "Windows"], ["box", "Linux"], ["phone", "Android"]].map(([i, n]) => `<span>${ICON[i]}${n}</span>`).join("")}</div>
</div>
<div class="hero-stage narrow combo">
${site.shot("hero", h.hero.imageAlt, t, { eager: true })}
${site.shot("heroPhone", h.hero.phoneAlt, t, { cls: "over" })}
<span class="float-chip c1">${ICON.stethoscope}${esc(h.hero.chips[0])}</span>
<span class="float-chip c3">${ICON.gauge}${esc(h.hero.chips[1])}</span>
<span class="float-chip c4">${ICON.share}${esc(h.hero.chips[2])}</span>
</div>
</div></section>`;

    const instead = `<section class="alt instead-band"><div class="wrap">
<p class="center muted">${esc(h.instead.title)}</p>
<div class="instead">${h.instead.items.map((x) => `<span class="old">${esc(x)}</span>`).join("")}<span class="arrow" aria-hidden="true"></span><span class="new">Echoes</span></div>
</div></section>`;

    const dr = h.doctor;
    const doctor = `<section id="doctor"><div class="wrap">
${head2(dr, v, where)}
<div class="showcase">
<div class="reveal">${site.shot("doctorSni", dr.alt, t)}</div>
<div class="points">${dr.points.map((p) => `<div class="point reveal"><div class="ico">${ICON[DOCTOR_ICON[p.id]] || ICON.sparkles}</div><div><h3>${esc(p.title)}</h3><p>${rich(p.text, v, where)}</p></div></div>`).join("")}</div>
</div>
</div></section>`;

    const tl = h.tools;
    const tools = `<section class="alt" id="tools"><div class="wrap">
${head2(tl, v, where)}
<div class="tool-groups">${tl.groups.map((g) => `<div class="tool-group reveal"><h3>${esc(g.title)}<span>${g.items.length}</span></h3><div class="tool-grid">${g.items.map((x) => `<div class="tool"><div class="ico">${ICON[TOOL_ICON[x.id]] || ICON.sparkles}</div><div><b>${esc(x.title)}</b><span>${esc(x.text)}</span></div></div>`).join("")}</div></div>`).join("\n")}</div>
</div></section>`;

    const rows = h.features.items.map((f, i) => {
      const pic = f.id === "share"
        ? `<div class="combo">${site.shot("share", f.alt, t)}${site.shot("shareBrowser", f.altPhone, t, { cls: "over" })}</div>`
        : site.shot(f.id, f.alt, t);
      return `<article class="feature reveal${i % 2 ? " flip" : ""}" id="${f.id}">
<div class="feature-text"><h3>${esc(f.title)}</h3><p>${rich(f.text, v, where)}</p>${ticks(f.points, v, where)}</div>
${pic}
</article>`;
    }).join("\n");
    const features = `<section id="tour"><div class="wrap">
${head2(h.features, v, where)}
<div class="features">
${rows}
</div>
<h3 class="center more-title reveal">${esc(h.more.title)}</h3>
<div class="gallery reveal">${h.more.items.map((x) => site.shot(x.id, x.alt, t, { caption: esc(x.caption) })).join("")}</div>
</div></section>`;

    const dv = h.devices;
    const devices = `<section class="alt" id="devices"><div class="wrap">
${head2(dv, v, where)}
<div class="devices reveal">${site.shot("iphone", dv.altIphone, t)}${site.shot("ipad", dv.altIpad, t)}${site.shot("android", dv.altAndroid, t)}</div>
<p class="note center-note">${rich(dv.note, v, where)}</p>
</div></section>`;

    const pv = h.privacy;
    const privacyIcons = [ICON.lock, ICON.ban, ICON.shieldCheck];
    const privacy = `<section id="private"><div class="wrap">
<div class="glow-card reveal"><div class="inner">
${head2(pv, v, where)}
<div class="cards">${pv.items.map((c, i) => `<div class="card"><div class="ico">${privacyIcons[i]}</div><h3>${esc(c.title)}</h3><p>${rich(c.text, v, where)}</p></div>`).join("")}</div>
<p class="more"><a href="${site.pagePath(lang, "privacy")}">${esc(pv.link)} ${ICON.arrow}</a></p>
</div></div>
</div></section>`;

    const fr = h.free;
    const free = `<section class="alt" id="free"><div class="wrap free">
<div class="reveal"><span class="kicker">${esc(fr.kicker)}</span><p class="big grad">${esc(fr.big)}</p><p>${rich(fr.text, v, where)}</p></div>
<ul class="checks reveal">${fr.items.map((x) => `<li>${ICON.check}<span>${rich(x, v, where)}</span></li>`).join("")}</ul>
</div></section>`;

    const dl = h.download;
    const download = `<section id="download"><div class="wrap">
${head2(dl, v, where)}
<div class="platform-cards five">${dl.platforms.map((p) => `<div class="platform-card reveal${p.id === "apple" ? " featured" : ""}"><div class="ico">${ICON[PLATFORM_ICON[p.id]]}</div><h3>${esc(p.name)}</h3><p>${esc(p.detail)}</p><div class="dl-links">${p.links.map((l, i) => `<a class="btn ${i === 0 ? "primary" : ""} small" href="${LINKS[l.id]}">${p.id === "apple" ? ICON.store : ICON.download}${esc(l.label)}</a>`).join("")}</div></div>`).join("")}</div>
<div class="verify-box">${ul(dl.notes, v, where)}</div>
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
          name: "Echoes",
          alternateName: ["Echoes: Network Tools & Ping", "Echoes network toolkit"],
          applicationCategory: "UtilitiesApplication",
          applicationSubCategory: "Network tools",
          operatingSystem: "iOS 15, iPadOS 15, macOS 12, Windows 10, Windows 11, Linux, Android 6.0",
          description: stripTags(fill(h.description, v, where)),
          url,
          inLanguage: "en",
          downloadUrl: [APP_STORE, RELEASES_URL],
          license: "https://opensource.org/licenses/MIT",
          isAccessibleForFree: true,
          offers: { "@type": "Offer", price: "0", priceCurrency: "USD" },
          author: AUTHOR,
          image: site.abs(site.assets["og/og.png"]),
          screenshot: ["hero", "speed", "share", "iphone"].map((s) => site.shotLargest(s)),
          featureList: h.tools.groups.flatMap((g) => g.items.map((x) => x.title)),
          sameAs: [REPO_URL, APP_STORE],
        },
        {
          "@type": "FAQPage",
          "@id": `${url}#faq`,
          inLanguage: lang.hreflang,
          mainEntity: h.faq.items.map((q) => ({ "@type": "Question", name: q.q, acceptedAnswer: { "@type": "Answer", text: stripTags(fill(q.a, v, where)) } })),
        },
        { "@type": "WebSite", "@id": `${ORIGIN}/#site`, name: "Echoes", url: `${ORIGIN}/`, inLanguage: site.built.map((l) => l.hreflang) },
      ],
    };
    return site.page({ lang, page: "home", title: plain(h.title, v, where), description: plain(h.description, v, where), ld, main: [hero, instead, doctor, tools, features, devices, privacy, free, download, faq].join("\n"), t, v });
  }

  function privacyPage(lang, t, v) {
    const p = t.privacy;
    const where = `${lang.code}:privacy`;
    const sections = p.sections.map((sec) => ({
      id: sec.id,
      toc: sec.title,
      html: `<h2>${esc(sec.title)}</h2>
${(sec.paragraphs || []).map((x) => `<p>${rich(x, v, where)}</p>`).join("\n")}
${sec.list ? ul(sec.list, v, where) : ""}`,
    }));
    const main = site.docPage({ lang, page: "privacy", t, heading: p.heading, lede: rich(p.intro, v, where), meta: `${esc(fill(t.common.updated, v, where))} · ${rich(p.sourceNote, v, where)}`, sections });
    return site.page({ lang, page: "privacy", title: plain(p.title, v, where), description: plain(p.description, v, where), ld: [site.crumbs(lang, "privacy", p.heading)], main, t, v });
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
    return `# Echoes

> ${stripTags(fill(t.home.description, v, "llms"))}

Echoes is a free, open-source (MIT) network toolkit by Sina Xhpm, on iPhone, iPad and Mac (App Store) and on Windows, Linux, Android and macOS (GitHub releases), from one C# / Avalonia codebase. No account, no ads, no analytics; nothing is sent to the developer. The app is in English.

## Tools

${t.home.tools.groups.map((g) => `### ${g.title}\n\n${g.items.map((x) => `- ${x.title}: ${x.text}`).join("\n")}`).join("\n\n")}

## Pages

- [Home](${ORIGIN}/): tools, screenshots, downloads, questions
- [Privacy policy](${ORIGIN}/privacy/): Echoes collects no data; the services some tools use
- Translations: ${site.built.filter((l) => l.code !== "en").map((l) => `[${l.name}](${site.abs(site.pagePath(l, "home"))})`).join(", ")}

## Links

- App Store (iPhone, iPad, Mac): ${APP_STORE}
- Source code: ${REPO_URL}
- Releases and downloads: ${RELEASES_URL}
- Issues: ${ISSUES_URL}
- Author: https://sinaxhpm.com
- Sister app: Submarine, an SSH, SFTP, RDP, VNC and Telnet client (${SUBMARINE}/)
`;
  }

  for (const lang of site.built) {
    const t = site.str[lang.code];
    const v = valuesFor(lang);
    const base = lang.code === "en" ? "" : `${lang.code}/`;
    site.file(`${base}index.html`, homePage(lang, t, v));
    site.file(`${base}privacy/index.html`, privacyPage(lang, t, v));
  }
  const vEn = valuesFor(LANGS[0]);
  site.file("404.html", notFoundPage(site.en, vEn));
  site.file("sitemap.xml", site.sitemap(["home", "privacy"]));
  site.file("robots.txt", site.robots());
  site.file("llms.txt", llms(vEn));
  site.file("manifest.webmanifest", site.manifest(stripTags(site.en.home.description)));
  site.file(".well-known/security.txt", site.securityTxt(`${ISSUES_URL}/new`, `${ORIGIN}/privacy/`));
  return site.check();
}
