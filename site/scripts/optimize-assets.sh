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

# ── Demo animation: GIF → MP4 (H.264) + WebM (VP9) ────────────────────────
# A GIF cannot express inter-frame compression for screen recordings the way a
# video codec can; these land around a quarter of the GIF's bytes. The GIF has
# no audio track, so the videos carry none either (which is also what lets the
# page autoplay them muted).
for name in demo-light demo-dark; do
  # The source is 1200×567 and H.264's yuv420p needs even dimensions, so the
  # filter rounds both down (losing at most one row/column of the last line).
  SCALE="scale=trunc(iw/2)*2:trunc(ih/2)*2"
  ffmpeg -loglevel error -y -i "$SRC/$name.gif" -vf "$SCALE" \
    -c:v libx264 -preset veryslow -crf 30 -pix_fmt yuv420p \
    -movflags +faststart -an "$OUT/$name.mp4"
  # The GIF carries an alpha channel, which decodes to `gbrap` — a pixel
  # format VP9 refuses. `yuva420p` is the alpha-capable format it wants, and
  # VP9 only honours alpha with alternate reference frames off.
  ffmpeg -loglevel error -y -i "$SRC/$name.gif" -vf "$SCALE" \
    -c:v libvpx-vp9 -crf 36 -b:v 0 -row-mt 1 -pix_fmt yuva420p -auto-alt-ref 0 \
    -an "$OUT/$name.webm"
done

# ── Poster frames ─────────────────────────────────────────────────────────
# A `<video>` needs a poster so the layout is filled before playback starts
# (and for the first paint, which is what keeps CLS at zero).
# ffmpeg's build here has no WebP encoder, so the frame is extracted as PNG
# and encoded by cwebp (the same tool the screenshots use).
for name in demo-light demo-dark; do
  ffmpeg -loglevel error -y -i "$SRC/$name.gif" -frames:v 1 "$OUT/$name-poster.png"
  cwebp -quiet -q 80 -m 6 "$OUT/$name-poster.png" -o "$OUT/$name-poster.webp"
  rm -f "$OUT/$name-poster.png"
done

echo "Generated:"
ls -la "$OUT" | awk 'NF>3 && $9 !~ /^\.\.?$/ {printf "  %-32s %7.0f KB\n", $9, $5/1024}'
