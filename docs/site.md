# Marketing site (`site/`)

The landing page at <https://www.syscity.net/> is a Vite + React single page,
prerendered to static HTML at build time and deployed to GitHub Pages by
`.github/workflows/site-pages.yml`.

`syscity.net` and `www.syscity.net` are served by GitHub Pages as the origin,
with DNS proxied through Cloudflare. That split matters for everything below:
**GitHub Pages cannot send custom response headers**, so anything of that kind
(`_headers`, `_redirects`, HSTS, redirects) has to live in Cloudflare, not in
this repo.

## What is in the repo

| Path | Purpose |
|------|---------|
| `site/src/` | React source (components, i18n) |
| `site/public/` | Copied verbatim into `dist/` — favicons, `og-image.png`, `robots.txt`, `sitemap.xml`, `site.webmanifest`, the legal and 404 pages |
| `site/scripts/postbuild.mjs` | Prerenders the app into `dist/index.html`, inlines the single stylesheet, injects the analytics beacon |
| `site/scripts/optimize-assets.sh` | Regenerates the WebP/AVIF screenshots and MP4/WebM demo from sources |
| `.github/workflows/site-pages.yml` | Build + deploy; also bakes `scripts/install.sh` and `install.ps1` into the artifact |

## Build and check locally

```bash
cd site
pnpm install
pnpm build          # tsc -b && vite build && postbuild
pnpm preview        # serve dist/
```

Two things the build must not regress, both caught by Lighthouse on
`dist/index.html`: `color-contrast` (the palette in `src/index.css` is tuned to
clear 4.5:1 at the text sizes actually used) and `image-size-responsive`
(screenshots declare their true intrinsic dimensions — the iOS and Android
captures differ in height).

## Analytics

Cloudflare Web Analytics, which is cookieless and sets nothing in the browser —
that is why the site needs no cookie banner. The beacon is injected by
`postbuild.mjs` from the `CFA_ANALYTICS_TOKEN` environment variable; when it is
unset the beacon is omitted entirely. CI passes it from the repository
**variable** of the same name (Settings → Secrets and variables → Actions →
Variables). Get the token from Cloudflare dashboard → Web Analytics.

Do not switch to a cookie-setting analytics product without adding a consent
banner at the same time.

## Files that Cloudflare can override

`public/robots.txt` is uploaded with the artifact, but Cloudflare's
**managed robots.txt** takes precedence when enabled for the zone. If
`https://www.syscity.net/robots.txt` does not serve the committed file
(check for a `Sitemap:` line), the managed one is winning.

## One-time live configuration (Cloudflare)

These are zone-level settings, outside the repo. Apply with the `cf` CLI or the
dashboard.

### 1. Force HTTPS with HSTS

`http://` already 301s to `https://`, so no visitor is served plaintext, but
nothing tells the browser to skip the first plaintext request next time.
Add HSTS: **SSL/TLS → Edge Certificates → HTTP Strict Transport Security**.

```bash
cf api /zones/<zone-id>/settings/security_header \
  --method PATCH \
  --body '{"value":{"strict_transport_security":{"enabled":true,"max_age":15552000,"include_subdomains":true,"preload":false,"nosniff":true}}}'
```

Start at `max_age` 15552000 (6 months), `preload: false`. Preload is a one-way
door: removal takes months and every subdomain must be HTTPS forever. Raise the
age only after a month with no mixed-content reports.

Verify:

```bash
curl -sSI --http1.1 https://www.syscity.net/ | grep -i strict-transport
```

### 2. Let the repo own robots.txt

If the served file is not the committed one, turn off the managed version:
**Security → Bots → Managed robots.txt** (or the zone's *AI Crawl Control*
settings), then confirm the served file contains `Sitemap: https://www.syscity.net/sitemap.xml`.

### 3. Cache policy

Already configured as one live Cache Rule (see the zone's
`http_request_cache_settings` entrypoint ruleset): `/assets/*` except
`/assets/platforms/*` gets a one-year Edge and Browser TTL. The exclusion is
load-bearing — `public/assets/platforms/*` is copied verbatim with no content
hash, so it must stay revalidatable.

## Adding a page

Because the site has no client-side router, a second page is a static file in
`site/public/` (like `privacy.html`). Reference it with a *root-absolute* path
(`/privacy.html`) from anything that can be served at a nested URL — in
particular `404.html`, which GitHub Pages serves for unknown paths at any
depth. Add it to `public/sitemap.xml` too.

The legal pages are plain HTML with the shared shell inlined; if the shell
changes, change it in both `privacy.html` and `terms.html`. The contact
addresses are `privacy@lightconsen.com` and `legal@lightconsen.com`, and the
same domain carries `security@` (SECURITY.md) and `syscity-conduct@`
(CODE_OF_CONDUCT.md). `lightconsen.com` publishes Cloudflare MX records, so
these receive mail — but a mailbox that is never read is worse than none:
confirm the four addresses are routed somewhere a person checks before the
launch posts go out.

## Things that are deliberately absent

- **Cookie banner** — nothing to consent to; see Analytics.
- **Contact / newsletter forms** — no form means no form validation and no
  spam handling to add.
- **iOS download link** — the app has no App Store listing yet. The card in
  `Download.tsx` is removed rather than left pointing at
  `apps.apple.com/app/syscity`, which resolves to an unrelated app's store
  page. Restore it with a real `.../id<digits>` URL and verify with
  `https://itunes.apple.com/lookup?bundleId=net.syscity`.
