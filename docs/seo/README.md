# SEO / AEO assets for the Submarine website

These structured-data templates belong on whatever public page advertises
Submarine — now `submarine.sinaxhpm.com` (the website's own pages carry their own copies). They are **not** loaded by
the desktop or mobile app: the Tauri WebView CSP (`script-src 'self'`)
blocks inline `<script>` tags, and AI crawlers don't reach the shell.

## Files

- **software-application.jsonld** — `SoftwareApplication` schema. Tells
  Google, Bing, ChatGPT search, and Perplexity what the app is, who made
  it, where to download it, what it costs, and what it looks like (the
  `screenshot` URLs point at `docs/screenshots/` on `main`). Highest-impact
  AEO asset.
- **faq.jsonld** — `FAQPage` schema mirroring the README FAQ. Answer
  engines quote the `acceptedAnswer` text verbatim for matching queries.

## Where to drop them on the website

Inside the `<head>` of the landing page, one `<script>` block per file:

```html
<script type="application/ld+json">
  <!-- contents of software-application.jsonld -->
</script>
<script type="application/ld+json">
  <!-- contents of faq.jsonld -->
</script>
```

Validate with <https://validator.schema.org/> after deploying.

## Keep the claims true

Answer engines quote these files word for word, so a claim that the app
doesn't meet gets repeated. When a feature or a privacy property changes,
change these together: the README (Security & privacy, FAQ), `llms.txt`,
`faq.jsonld`, and the `featureList` here. The points most likely to drift:

- what cloud sync encrypts, and what the server still sees (metadata, and
  a copy of the vault key sealed with the master password);
- the one anonymous GitHub releases request when the profile page opens;
- Quick connect saves the host (and what was typed) to the root folder;
- which protocols are labelled unencrypted (Telnet, plain VNC) and which
  features are experimental (the built-in RDP client).

## What to update on every release

The `softwareVersion` field is intentionally NOT pinned in
`software-application.jsonld` — Google complains if it goes stale, so
either templatize it from your site's build (preferred) or update it
manually each release. The current shipping version lives in
`package.json` at the repo root.

## Repository metadata (set on GitHub, not in the repo)

GitHub's About box and topics are search signals too, and live in the
repository settings:

- **Description**: `Free, open-source SSH & SFTP client with RDP, VNC and Telnet — terminal, dual-pane SFTP, port forwarding, folder mirror, encrypted profiles. Windows · macOS · Linux · Android. Rust + Tauri.`
- **Website**: `https://sinaxhpm.com`
- **Topics** (GitHub allows at most 20): `ssh`, `ssh-client`, `sftp`, `sftp-client`, `telnet`, `vnc`, `rdp`, `remote-desktop`, `terminal`, `port-forwarding`, `ssh-tunnel`, `socks5`, `folder-sync`, `putty-alternative`, `winscp-alternative`, `tauri`, `rust`, `react`, `android`, `cross-platform`
- **Social preview** (Settings → General): a 1280×640 image; `docs/screenshots/hero.png` cropped to that ratio works.
