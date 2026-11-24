#!/usr/bin/env bash
#
# Regenerate the landing page's media in modern formats.
#
# The originals were a 195 KB GIF (the demo animation) and four 82–222 KB PNGs
# (phone screenshots) — 87% of the page's weight. This converts them to
# WebP/AVIF and MP4/WebM once, into `src/assets/` so Vite fingerprints them
# (fingerprinted names are what make the immutable cache headers in
# `public/_headers` safe).
#
# Run after replacing any source asset in `public/assets/`:
#   ./scripts/optimize-assets.sh
#
# Requires ffmpeg, cwebp, avifenc.
#
# Resolution is deliberately NOT reduced: the screenshots render at ~258×560
# CSS px, so a 480 px-wide source is the 2x asset a retina display wants.
# Only the format changes.
set -euo pipefail

cd "$(dirname "$0")/.."
SRC=public/assets
OUT=src/assets
mkdir -p "$OUT"

# ── Phone screenshots: PNG → WebP (primary) + AVIF (smallest) ──────────────
# Both themes, because the page picks one with `<picture>` +
# `prefers-color-scheme`: each visitor downloads one set, not both.
for name in mobile-ios-light mobile-ios-dark mobile-android-light mobile-android-dark; do
  cwebp -quiet -q 82 -m 6 "$SRC/$name.png" -o "$OUT/$name.webp"
  avifenc -q 62 --speed 4 -s 8 "$SRC/$name.png" "$OUT/$name.avif" >/dev/null
done

# ── Logo: 192×172 PNG used at 20–80 px ─────────────────────────────────────
# One WebP is enough; it lands under 5 KB.
cwebp -quiet -q 88 -m 6 -alpha_q 100 "$SRC/../syscity.png" -o "$OUT/syscity.webp"

# ── Demo clip ───────────────────────────────────────────────────────────────
# NOT generated here. The landing page's demo is a composite screen recording —
# the app next to the Linux desktop it drove, with captions burned in — that is
# cut by hand from a real run, not derived from a checked-in GIF. The previous
# per-theme `demo-light`/`demo-dark` pair came from `public/assets/*.gif` and no
# longer exists; `src/assets/demo-desktop.mp4|webm|-poster.webp` is authored, so
# re-running this script must not overwrite or recreate the old pair.

echo "Generated:"
ls -la "$OUT" | awk 'NF>3 && $9 !~ /^\.\.?$/ {printf "  %-32s %7.0f KB\n", $9, $5/1024}'
