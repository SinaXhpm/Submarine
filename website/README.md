# The product websites

Two static sites, built from this folder:

- [submarine.sinaxhpm.com](https://submarine.sinaxhpm.com) (`sites/submarine/`): what Submarine is, how it protects your data, the privacy policy and the account deletion page.
- [echoes.sinaxhpm.com](https://echoes.sinaxhpm.com) (`sites/echoes/`): Echoes, the network toolkit by the same author (its source is [SinaXhpm/Echoes](https://github.com/SinaXhpm/Echoes)), and its privacy policy, which follows that repository's PRIVACY.md word for word.

Both are in English and 11 translations. Static HTML with one style sheet and one small script (plus the delete form on the Submarine site); fonts are served from the site itself; no cookies, no analytics, no third-party requests. The servers send a strict Content-Security-Policy (`script-src 'self'; style-src 'self'`), so the pages have no inline script or style.

## Layout

```
build.mjs            the command: builds one site or both into dist/<site>/
engine.mjs           what both sites share: languages, strings, safe text, hashed assets, page frame, sitemap, checks
shared/              site.css, site.js, icons.mjs, fonts/ (Inter, Vazirmatn, JetBrains Mono; OFL, licences next to them)
sites/<site>/        site.mjs (the pages), i18n/<lang>.json (the strings), img/ (pictures), og.html (the link picture)
tools/               images.sh, check-i18n.mjs, serve.mjs, shoot.mjs, delete-e2e.mjs
```

Each site has its own accent colour (`body[data-site]` in `site.css`); everything else is shared.

## Build

```sh
node website/build.mjs                       # both -> website/dist/submarine, website/dist/echoes
node website/build.mjs --site echoes
```

Node 20 or later and git; no npm packages. The Submarine security page links into the app's code at a fixed commit of `main` (`--commit <sha>`, default `origin/main`), so the links show the exact lines they talk about; the build finds those lines itself (`CODE` in `sites/submarine/site.mjs`). The numbers the pages quote (Argon2 settings, sync limits, how long logs are kept) are in `FACTS` there; change them there and nowhere else.

The build stops on an unfilled `{placeholder}`, a link to a page, file or anchor that isn't there, an image without `alt`, a duplicate id, or anything the CSP would block.

## Pictures

`tools/images.sh submarine [screenshots dir]` makes the Submarine site's `img/*.webp` and icons from the app's screenshots and icons. `tools/images.sh echoes <Echoes repo> <store shots>` does the same for Echoes, from its repository's screenshots and the window pictures its store-screenshot workflows draw at 2x. Both need ImageMagick 7 with WebP. Look at every picture before it is published: nothing private may show (paths, host names, real addresses).

`sites/<site>/og.html` is the link preview picture (`img/og.png`), taken at 1200 x 630 with a headless browser, for example:

```sh
msedge --headless=new --window-size=1200,630 --screenshot=<site>/img/og.png file:///.../sites/<site>/og.html
```

## Check

```sh
node website/tools/check-i18n.mjs submarine fa ar zh es fr de ru pt-br ja tr hi   # translations against English
node website/tools/check-i18n.mjs echoes fa ar zh es fr de ru pt-br ja tr hi
node website/tools/serve.mjs 8790 submarine                                      # serves dist/<site> with the production headers
node website/tools/shoot.mjs http://127.0.0.1:8790 out / /fa/ /security/          # pictures + console/CSP errors (Edge or Chrome)
```

## Translations

`sites/<site>/i18n/en.json` is the source. Each language is `<code>.json` with the same keys; a missing string falls back to English (the build lists them), and a list of things with ids (features, sections) that doesn't match the English one is taken from English whole. Keep `{placeholders}` and the HTML tags as they are. Both apps are in English, so the names of their buttons, settings and tools stay in English (`check-i18n.mjs` knows them). Don't make any claim stronger or weaker than the English one. The privacy policies' translations say that the English text is the one that counts. A new language is a line in `LANGS` in `engine.mjs` plus its JSON files.

## Hosting

The Submarine site shares its folder with the sync API on the server (both names, `submarine.sinaxhpm.com` and `api.sinaxhpm.com`). The API keeps its paths, at the root and under `/api`; they are routed before any file is looked at, so no page can ever take one of them. Never redirect an API path: the apps send POSTs. The Echoes site has a folder of its own and no PHP at all.
