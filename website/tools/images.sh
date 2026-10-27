#!/bin/bash
# Makes a site's pictures in website/sites/<site>/img/ from the app's
# screenshots and icons. Needs ImageMagick 7 with WebP. Run it again when the
# screenshots change, and look at every picture before it is published.
#   bash website/tools/images.sh submarine [screenshots dir]
#        (default: docs/screenshots of this repo)
#   bash website/tools/images.sh echoes <Echoes repo> <store shots dir>
#        (store shots: the Echoes-mac-store-screenshots and Echoes-store-screenshots
#         artifacts of the Echoes repo's macos-appstore.yml and ios-screenshots.yml,
#         unpacked into mac/ and ios/)
set -euo pipefail
HERE="$(cd "$(dirname "$0")/.." && pwd)"
REPO="$(cd "$HERE/.." && pwd)"
SITE="${1:?submarine or echoes}"
OUT="$HERE/sites/$SITE/img"
mkdir -p "$OUT"
q=80

# name, source file, crop (WxH+X+Y or "-"), widths
shot() {
  local name="$1" file="$2" crop="$3"; shift 3
  local tmp
  tmp="$(mktemp --suffix=.png)"
  if [ "$crop" = "-" ]; then cp "$file" "$tmp"; else magick "$file" -crop "$crop" +repage "$tmp"; fi
  for w in "$@"; do
    magick "$tmp" -strip -resize "${w}x>" -quality "$q" -define webp:method=6 "$OUT/$name-$w.webp"
  done
  rm -f "$tmp"
  echo "$name: $(magick identify -format '%wx%h' "$OUT/$name-${@: -1}.webp") largest"
}

# A square icon from a logo that may not be square: centred, transparent around it.
square() { magick "$1" -strip -background none -gravity center -resize "${2}x${2}" -extent "${2}x${2}" "$3"; }

case "$SITE" in
submarine)
  SRC="${2:-$REPO/docs/screenshots}"
  shot hero "$SRC/hero.png" - 960 1600 2560
  shot terminal "$SRC/wall.png" - 960 1600 2560
  shot files "$SRC/compare.png" 648x980+1912+160 400 648
  shot rdp "$SRC/rdp.png" - 960 1600 2560
  shot vnc "$SRC/vnc.png" - 960 1600 2560
  shot tunnels "$SRC/ports.png" 640x700+0+0 320 640
  shot fits "$SRC/add-server.png" 896x1060+0+0 448 896
  shot monitor "$SRC/monitor.png" - 960 1600 2560
  shot vault "$SRC/logins.png" 1100x940+0+0 550 1100
  shot phone "$SRC/android-terminal.png" - 360 720 1080
  # the Speed and quality panel, cut from a picture of a remote desktop tab with it open
  shot remoteSettings "$SRC/remote-settings.png" 760x1232+1784+168 380 760
  ICONS="$REPO/src-tauri/icons"
  magick "$ICONS/icon.png" -strip -resize 64x64 "$OUT/logo-64.png"
  magick "$ICONS/icon.png" -strip -resize 192x192 "$OUT/icon-192.png"
  magick "$ICONS/icon.png" -strip "$OUT/icon-512.png"
  magick "$ICONS/icon.png" -strip -resize 150x150 -background "#04070f" -gravity center -extent 180x180 "$OUT/apple-touch-icon.png"
  cp "$ICONS/icon.ico" "$OUT/favicon.ico"
  ;;
echoes)
  ER="${2:?the Echoes repo}"
  ART="${3:?the folder with the store shots}"
  DOCS="$ER/docs/screenshots"
  shot hero "$ART/mac/window-01-doctor.png" - 860 1290 1720
  shot heroPhone "$ART/ios/iphone/01-doctor.png" - 360 720
  shot ping "$ART/mac/window-02-ping.png" - 860 1720
  shot speed "$ART/mac/window-03-speed.png" - 860 1720
  shot monitor "$ART/mac/window-04-monitor.png" - 860 1720
  shot share "$ART/mac/window-05-share.png" - 860 1720
  shot dns "$ART/mac/window-06-dns.png" - 860 1720
  shot tls "$ART/mac/window-07-tls.png" - 860 1720
  shot curl "$ART/mac/window-08-curl.png" - 860 1720
  shot strings "$ART/mac/window-09-password.png" - 860 1720
  shot iphone "$ART/ios/iphone/02-menu.png" - 360 720
  shot ipad "$ART/ios/ipad/01-doctor.png" - 520 1040
  shot doctorSni "$DOCS/doctor-sni-filtering.png" - 860
  shot shareBrowser "$DOCS/share-browser-phone.png" - 390
  shot android "$DOCS/android-speed-test.png" - 412
  shot scanner "$DOCS/port-scanner.png" - 860
  shot ssh "$DOCS/ssh-server-health.png" - 860
  shot cloudflare "$DOCS/cloudflare-dns-records.png" - 860
  square "$ER/Assets/logo.png" 64 "$OUT/logo-64.png"
  square "$ER/Assets/logo.png" 192 "$OUT/icon-192.png"
  square "$ER/Assets/logo.png" 512 "$OUT/icon-512.png"
  magick "$ER/Echoes.iOS/Assets.xcassets/AppIcon.appiconset/AppIcon-1024.png" -strip -resize 180x180 "$OUT/apple-touch-icon.png"
  magick "$ER/Assets/logo.png" -strip -background none -gravity center -resize 48x48 -extent 48x48 -define icon:auto-resize=48,32,16 "$OUT/favicon.ico"
  # each site's footer shows the other one
  square "$ER/Assets/logo.png" 88 "$HERE/sites/submarine/img/echoes-88.png"
  magick "$REPO/src-tauri/icons/icon.png" -strip -resize 88x88 "$OUT/submarine-88.png"
  ;;
*) echo "unknown site $SITE"; exit 1 ;;
esac
ls -la "$OUT"
