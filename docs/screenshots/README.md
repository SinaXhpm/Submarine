# Screenshots for the project README

The README's gallery uses the files below. Replace a file in place and the
README picks it up; no code changes needed.

| File | What it shows |
|---|---|
| `hero.png` | A server session with the terminal on the left and the dual-pane SFTP browser on the right. The first image in the README. |
| `compare.png` | SFTP compare view: the local and remote folder, files marked only here / other size / newer / older. |
| `vnc.png` | A VNC desktop in its tab, with the "not encrypted" label. |
| `rdp.png` | An RDP desktop in its tab (the experimental built-in client), with the "Experimental" label. |
| `wall.png` | The Wall: three terminals from different servers side by side (cropped to the tiles). |
| `monitor.png` | Monitoring: CPU, memory, disk and load graphs for three nodes (cropped). |
| `servers-home-lab.png` | A folder of servers with colour tags and RDP / VNC / Telnet badges (cropped). |
| `servers.png` | The server list at its top level (cropped). |
| `ports.png` | Port forwarding panel: a local forward, a SOCKS5 proxy, a remote forward (panel only). |
| `mirror.png` | Folder Mirror panel with one saved mirror (panel only). |
| `library-panel.png` | The per-server command library in a session (panel only). |
| `library.png` | The Library page: saved commands. |
| `logins.png` | Saved passwords and SSH keys. |
| `add-server.png` | The server form: type, folder, tag colour, host, login (panel only). |
| `profiles.png` | The profile picker (cropped to the card). |
| `settings.png` | Settings: accent colour, background, ssh-agent and ssh config switches, terminal font. |
| `terminal-colours.png` | Settings: terminal colour schemes with a live preview (cropped). |
| `terminal.png` | A full-width terminal session with coloured output. |
| `android-terminal.png` | Android: an SSH session with the key bar and the bottom navigation (the app's own screen, no status bar). |
| `android-server-form.png` | Android: the server form; the tag colours wrap into rows on a narrow screen. |

## How they are made

All of them show a throwaway demo profile ("Demo") with invented servers
(`example.com` names and documentation addresses; the live ones are a
local test server with a made-up user), never a real profile. The window is
rendered at 1280×800 at 2× device scale (2560×1600 PNG), then cropped where
a whole window would be mostly empty.

## Before adding or replacing one

- Nothing of a real machine or person may be visible: no user name, no
  computer name, no home or data-folder path, no real host, address,
  e-mail, key or token. Settings → Maintenance shows the data folder, and
  Info shows the computer name — avoid or crop those.
- Look at every image at full size before committing it; also check the
  title bar and any terminal text.
- Keep each file under about 400 KB and the pictures current with the
  interface (the version label should match the release).
