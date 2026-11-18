# macOS DMG window

The GitHub download's DMG opens on a window in the app's look: the dark ground and the waves of the Windows setup, the app with a sonar ring, and an arrow to Applications.

- `dmg-settings.py`: the window for [dmgbuild](https://pypi.org/project/dmgbuild/): its size, where the two icons go, 128-point icons, the volume icon.
- `dmg-requirements.txt`: dmgbuild and the two libraries it uses, pinned by hash.
- `dmg-background.svg`: the picture. `dmg-background.png` (660 x 410) and `dmg-background@2x.png` (1320 x 820) are made from it; dmgbuild puts the two in one Retina TIFF. The window shows 400 points of its height on macOS 26 and up to 410 on older versions, whose title bars are lower, so the waves end at 400 and their colour goes on to the bottom.

Finder draws the icons and their names over the picture, and it draws the names in black in Light and Dark alike, whatever is under them. So the picture has light plates where the names go: move an icon in `dmg-settings.py`, and move its plate in the SVG with it.

## In the release

`apple.yml` lays the window out in `mac-build`, the job without secrets, around an empty `Submarine.app`, because dmgbuild comes from PyPI. `mac-sign` checks that the window holds nothing else, puts the signed app in the empty one's place with `hdiutil` and `ditto`, and signs and notarizes the DMG as before. A test run (no upload) also takes pictures of the window in Light and Dark, in the artifact `mac-dmg-pictures`.

## Making the PNGs

Any SVG renderer that keeps the size will do, for example Edge or Chrome:

    msedge --headless --hide-scrollbars --window-size=660,410 --force-device-scale-factor=1 --screenshot=dmg-background.png dmg-background.svg
    msedge --headless --hide-scrollbars --window-size=660,410 --force-device-scale-factor=2 --screenshot=dmg-background@2x.png dmg-background.svg
