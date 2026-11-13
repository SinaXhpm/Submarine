# The product websites

Two static sites, built from this folder:

- [submarine.sinaxhpm.com](https://submarine.sinaxhpm.com) (`sites/submarine/`): what Submarine is, how it protects your data, the privacy policy and the account deletion page.
- [echoes.sinaxhpm.com](https://echoes.sinaxhpm.com) (`sites/echoes/`): Echoes, the network toolkit by the same author (its source is [SinaXhpm/Echoes](https://github.com/SinaXhpm/Echoes)), and its privacy policy, which follows that repository's PRIVACY.md word for word.
- [captun.sinaxhpm.com](https://captun.sinaxhpm.com) (`sites/captun/`): Captun, the Wi-Fi hotspot and internet-sharing app by the same author (its source is [SinaXhpm/captun](https://github.com/SinaXhpm/captun)). One page: it has no release and no licence yet, so the page links the source, offers no download and never calls it open source; the routing engine is marked experimental.

All three sites are in English and 9 translations (no Persian or Arabic since November 2026: their old addresses go to the English pages). Static HTML with one style sheet and one small script (plus the delete form on the Submarine site); fonts are served from the site itself; no cookies, no analytics, no third-party requests. The servers send a strict Content-Security-Policy (`script-src 'self'; style-src 'self'`), so the pages have no inline script or style.

## Layout

```
build.mjs            the command: builds one site or both into dist/<site>/
engine.mjs           what the sites share: languages, strings, safe text, hashed assets, page frame, sitemap, checks
shared/              site.css, site.js, icons.mjs, fonts/ (Inter, JetBrains Mono; OFL, licences next to them)
sites/<site>/        site.mjs (the pages), i18n/<lang>.json (the strings), img/ (pictures), og.html (the link picture)
tools/               images.sh, check-i18n.mjs, serve.mjs, shoot.mjs, delete-e2e.mjs
```

Each site has its own accent colour (`body[data-site]` in `site.css`); everything else is shared.

## Build

```sh
node website/build.mjs                       # all -> website/dist/submarine, website/dist/echoes, website/dist/captun
node website/build.mjs --site echoes
```

Node 20 or later and git; no npm packages. The Submarine security page links into the app's code at a fixed commit of `main` (`--commit <sha>`, default `origin/main`), so the links show the exact lines they talk about; the build finds those lines itself (`CODE` in `sites/submarine/site.mjs`). The numbers the pages quote (Argon2 settings, sync limits, how long logs are kept) are in `FACTS` there; change them there and nowhere else.

The build stops on an unfilled `{placeholder}`, a link to a page, file or anchor that isn't there, an image without `alt`, a duplicate id, or anything the CSP would block.

## Pictures

`tools/images.sh submarine [screenshots dir]` makes the Submarine site's `img/*.webp` and icons from the app's screenshots and icons. `tools/images.sh echoes <Echoes repo> <store shots>` does the same for Echoes, from its repository's screenshots and the window pictures its store-screenshot workflows draw at 2x. `tools/images.sh captun <Captun repo> <screenshots>` makes Captun's from its icons and pictures of every page of the desktop app drawn with made-up data. All need ImageMagick 7 with WebP. Look at every picture before it is published: nothing private may show (paths, host names, real addresses).

`sites/<site>/og.html` is the link preview picture (`img/og.png`), taken at 1200 x 630 with a headless browser, for example:

```sh
msedge --headless=new --window-size=1200,630 --screenshot=<site>/img/og.png file:///.../sites/<site>/og.html
```

## Check

```sh
node website/tools/check-i18n.mjs submarine zh es fr de ru pt-br ja tr hi         # translations against English and the app's own words
node website/tools/check-i18n.mjs echoes zh es fr de ru pt-br ja tr hi
node website/tools/check-i18n.mjs captun zh es fr de ru pt-br ja tr hi
node website/tools/serve.mjs 8790 submarine                                      # serves dist/<site> with the production headers
node website/tools/shoot.mjs http://127.0.0.1:8790 out / /de/ /security/          # pictures + console/CSP errors (Edge or Chrome)
```

## Translations

`sites/<site>/i18n/en.json` is the source. Each language is `<code>.json` with the same keys; a missing string falls back to English (the build lists them), and a list of things with ids (features, sections) that doesn't match the English one is taken from English whole. Keep `{placeholders}` and the HTML tags as they are. Submarine's app is translated (since 0.4), so a translated Submarine page names the app's buttons and settings as the app does in that language (`src/i18n/<lang>.json`; `check-i18n.mjs` checks the ones the pages name), except in picture descriptions: the screenshots show the app in English. Echoes and Captun are in English only, so their sites keep the names of their tools, buttons and settings in English (`check-i18n.mjs` knows them). Don't make any claim stronger or weaker than the English one. The privacy policies' translations say that the English text is the one that counts. A new language is a line in `LANGS` in `engine.mjs` plus its JSON files.

## Hosting

The Submarine site shares its folder with the sync API on the server (both names, `submarine.sinaxhpm.com` and `api.sinaxhpm.com`). The API keeps its paths, at the root and under `/api`; they are routed before any file is looked at, so no page can ever take one of them. Never redirect an API path: the apps send POSTs. The Echoes site has a folder of its own and no PHP at all.
