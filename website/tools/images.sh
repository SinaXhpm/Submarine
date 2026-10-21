#!/bin/bash
# Makes the website's pictures in website/img/ from the app's screenshots
# (docs/screenshots, the PNGs made for the README) and its icons. Needs
# ImageMagick 7 with WebP. Run it again when the screenshots change.
#   bash website/tools/images.sh [screenshots dir]
set -euo pipefail
HERE="$(cd "$(dirname "$0")/.." && pwd)"
REPO="$(cd "$HERE/.." && pwd)"
SRC="${1:-$REPO/docs/screenshots}"
OUT="$HERE/img"
mkdir -p "$OUT"
q=80

# name, source file, crop (WxH+X+Y or "-"), widths
shot() {
  local name="$1" file="$2" crop="$3"; shift 3
  local tmp
  tmp="$(mktemp --suffix=.png)"
  if [ "$crop" = "-" ]; then cp "$SRC/$file" "$tmp"; else magick "$SRC/$file" -crop "$crop" +repage "$tmp"; fi
  for w in "$@"; do
    magick "$tmp" -strip -resize "${w}x>" -quality "$q" -define webp:method=6 "$OUT/$name-$w.webp"
  done
  rm -f "$tmp"
  echo "$name: $(magick identify -format '%wx%h' "$OUT/$name-${@: -1}.webp") largest"
}

shot hero hero.png - 960 1600 2560
shot terminal wall.png - 960 1600 2560
shot files compare.png 648x980+1912+160 400 648
shot desktops rdp.png - 960 1600
shot tunnels ports.png 640x700+0+0 320 640
shot fits add-server.png 896x1060+0+0 448 896
shot monitor monitor.png - 960 1600 2560
shot vault logins.png 1100x940+0+0 550 1100
shot phone android-terminal.png - 360 720 1080

# Icons, from the app's own.
ICONS="$REPO/src-tauri/icons"
magick "$ICONS/icon.png" -strip -resize 64x64 "$OUT/logo-64.png"
magick "$ICONS/icon.png" -strip -resize 192x192 "$OUT/icon-192.png"
magick "$ICONS/icon.png" -strip "$OUT/icon-512.png"
magick "$ICONS/icon.png" -strip -resize 150x150 -background "#0a0a0c" -gravity center -extent 180x180 "$OUT/apple-touch-icon.png"
cp "$ICONS/icon.ico" "$OUT/favicon.ico"
ls -la "$OUT"
