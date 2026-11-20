# dmgbuild settings for the window of the GitHub download's DMG. apple.yml
# (mac-build) runs it from the repository's root, around an empty Submarine.app:
#   dmgbuild -s packaging/macos/dmg-settings.py -D app=<Submarine.app> Submarine <out.dmg>
# and mac-sign makes room in the image and puts the signed app in that
# Submarine.app's place.
#
# The window is 660 x 432 points with its title bar, so the area under the bar is
# 400 points tall on macOS 26 (a 32-point bar), 404 on macOS 11 to 15 and 410 on
# 10.15. dmg-background.png (660 x 410) and its @2x (rendered from
# dmg-background.svg; dmgbuild merges the two into one Retina TIFF) fill each of
# them, drawn for these places: the app at (170, 172) and Applications at
# (490, 172), the icons' centres, with 128-point icons. Finder draws the names
# under the icons in black, in Light and Dark alike, so the picture has light
# plates there: move an icon, move its plate.
import os.path

app = defines["app"]
files = [app]
symlinks = {"Applications": "/Applications"}
icon_locations = {os.path.basename(app.rstrip("/")): (170, 172), "Applications": (490, 172)}
background = "packaging/macos/dmg-background.png"
icon = "src-tauri/icons/icon.icns"
format = "UDZO"
window_rect = ((200, 120), (660, 432))
icon_size = 128
text_size = 14
default_view = "icon-view"
show_icon_preview = False
show_status_bar = False
show_tab_view = False
show_toolbar = False
show_pathbar = False
show_sidebar = False
