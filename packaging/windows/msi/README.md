# .msi pictures

The `.msi` installs with Windows Installer's own wizard (Tauri's WiX template). It shows two pictures, set in `src-tauri/tauri.conf.json` under `bundle.windows.wix`:

- `dialog.bmp` (493 x 312): the first and the last page. The wizard writes their text in black from x = 180, so the art keeps to the 164 pixels on the left.
- `banner.bmp` (493 x 58): the top of the other pages. The wizard writes the page's title and description in black over its left part, so only the right end holds the logo.

Both are 24-bit BMPs made from `dialog.svg` and `banner.svg`, which take the logo from `src-tauri/icons/128x128@2x.png`: render the SVG at its size (as for the DMG picture, see `packaging/macos/README.md`) and save the result as a 24-bit BMP.
