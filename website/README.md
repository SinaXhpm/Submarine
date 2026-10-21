# The Submarine website

The pages at [submarine.sinaxhpm.com](https://submarine.sinaxhpm.com): what Submarine is, how it protects your data, the privacy policy and the account deletion page, in English and translations. Static HTML with one style sheet and one small script (the delete form); no cookies, no analytics, no third-party requests. The server sends a strict Content-Security-Policy (`script-src 'self'; style-src 'self'`), so the pages have no inline script or style.

## Build

```sh
node website/build.mjs            # -> website/dist/
```

Node 20 or later and git; no npm packages. The security page links into the app's code at a fixed commit of `main` (`--commit <sha>`, default `origin/main`), so the links show the exact lines they talk about; the build finds those lines itself (`CODE` in `build.mjs`). The numbers the pages quote (Argon2 settings, sync limits, how long logs are kept) are in `FACTS`; change them there and nowhere else.

Pictures: `tools/images.sh [screenshots dir]` makes `img/*.webp` and the icons from the app's screenshots (`docs/screenshots`) and icons (needs ImageMagick 7 with WebP). `tools/og.html` is the link preview picture (`img/og.png`), taken at 1200 x 630 with a headless browser.

## Check

```sh
node website/tools/check-i18n.mjs fa ar zh es fr de ru pt-br ja tr hi   # translations against English
node website/tools/serve.mjs 8790                                     # serves dist/ with the production headers
node website/tools/shoot.mjs http://127.0.0.1:8790 out / /fa/ /security/   # pictures + console/CSP errors (Edge or Chrome)
```

The build itself stops on an unfilled `{placeholder}`, a link to a page or file that isn't there, an image without `alt`, or anything the CSP would block.

## Translations

`i18n/en.json` is the source. Each language is `i18n/<code>.json` with the same keys; a missing string falls back to English (the build lists them). Keep `{placeholders}` and the HTML tags as they are, leave product names and the app's own (English) button names in English, and don't make any claim stronger or weaker than the English one. The privacy policy's translations say that the English text is the one that counts. A new language is a line in `LANGS` in `build.mjs` plus its JSON file.

## Hosting

The site shares its folder with the sync API on the server (both names, `submarine.sinaxhpm.com` and `api.sinaxhpm.com`). The API keeps its paths, at the root and under `/api`; they are routed before any file is looked at, so no page can ever take one of them. Never redirect an API path: the apps send POSTs.
