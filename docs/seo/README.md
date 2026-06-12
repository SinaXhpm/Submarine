# SEO / AEO assets for the Submarine website

These structured-data templates belong on whatever public page advertises
Submarine — currently `sinaxhpm.com/submarine`. They are **not** loaded by
the desktop or mobile app: the Tauri WebView CSP (`script-src 'self'`)
blocks inline `<script>` tags, and AI crawlers don't reach the shell.

## Files

- **software-application.jsonld** — `SoftwareApplication` schema. Tells
  Google, Bing, ChatGPT search, and Perplexity what the app is, who made
  it, where to download it, and what it costs. Highest-impact AEO asset.
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

## What to update on every release

The `softwareVersion` field is intentionally NOT pinned in
`software-application.jsonld` — Google complains if it goes stale, so
either templatize it from your site's build (preferred) or update it
manually each release. The current shipping version lives in
`package.json` at the repo root.
