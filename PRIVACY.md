# Privacy policy

*Last updated: 21 October 2026.*

This policy covers the Submarine app (Windows, macOS, Linux, Android and iOS) and the optional Submarine sync service that the app can use.

**In short**

- Your servers, passwords, SSH keys and notes stay on your device, encrypted with a key made from your master password. The app has no analytics, no advertising, no tracking and no crash reporting.
- Cloud sync is optional. If you turn it on, the sync server keeps your account's email address and your profiles in encrypted form. It cannot read what is inside them.
- You can delete your account, and everything the server holds for it, from inside the app.

## Who is responsible

Submarine is made by Sina Xhpm ([sinaxhpm.com](https://sinaxhpm.com)), who also runs the sync service at `api.sinaxhpm.com`. The source code is public at [github.com/SinaXhpm/Submarine](https://github.com/SinaXhpm/Submarine) (MIT license), so what this policy says can be checked in the code.

## What stays on your device

- **Your profiles** (servers, logins, SSH keys, tunnels, notes, commands, known host keys) are stored in the app's data folder. They are encrypted with AES-256-GCM, using a key derived from your master password with Argon2id. The master password is never stored and never sent anywhere. There is no way to recover it: if you forget it, the profile cannot be opened.
- **Quick unlock** (optional, off by default): if you turn on Windows Hello or an Android fingerprint, the profile's key is sealed to a key held by the system (the TPM or the Android Keystore). The system checks your fingerprint or PIN; the app never sees it.
- **Settings** such as the window position, the last-used profile name and the colour theme are stored in the app's own settings, not encrypted.
- **The clipboard** is read only when you paste into the app, and never sent anywhere.
- **The activity log** in the app lives in memory and goes away when the app closes.

## What the app connects to

- **Your own servers.** The app connects only to the hosts you add or open, using SSH, SFTP, Telnet, VNC or RDP, through any proxy or jump host you set up. What you type and transfer goes to that server, not to us. Plain Telnet and plain VNC are not encrypted, and the app says so.
- **The sync service**, only if you sign in to cloud sync (see below). You can point the app at a server of your own instead; then that server's operator handles the data.
- **GitHub**, to look for a newer version (desktop and Android apps only: the iOS app is updated by the App Store and does not do this). When the profile list opens, and when you press "Check for updates", the app asks GitHub's public releases list (`api.github.com`) whether a newer version exists. The request carries the app's name and version (`submarine-app/x.y.z`) and nothing else; GitHub sees your IP address, as with any web request.
- **Links** you click open in your system browser.

Nothing else leaves your device. The app contains no third-party analytics, advertising or tracking code.

## Cloud sync (optional): what the sync server keeps

If you create an account, the server stores:

| What | Why | Can the server read it? |
|---|---|---|
| Your email address | to sign you in and to send sign-in and reset codes | Yes |
| Your account password | to sign you in | No: it keeps only a salted bcrypt hash |
| Sign-in sessions: a hashed token, the IP address it was issued to, an expiry (90 days) | to keep you signed in | The token is hashed; the rest is readable |
| Your profiles' records | to sync them between your devices | **No.** Each record is encrypted on your device with the profile's key before it is uploaded. The server sees an ID per record, its type (server, login, key, note…), timestamps, a deleted flag and the size, plus the profile's display name and how many records it has |
| A copy of each profile's key, sealed with your master password | to bring a profile onto a new device | Not without your master password. Someone who got the server's data could try to guess a weak master password offline, so use a long one |
| Sharing: your public key, your private key wrapped with your sharing passphrase, the names of shared profiles, the emails and roles of the people you share with, and the keys sealed to them | to share a profile with other accounts | The names and emails, yes; the keys, no |
| A log of account events: what happened (sign-up, sign-in, failed sign-in, sign-out, lock-out, password reset, a sync, a share invitation or change, and similar), the email address when it is known, the IP address, the time and Cloudflare's request ID. Never what was synced | to protect accounts and stop abuse | Yes. Kept for 90 days, then deleted |
| Rate-limit counters, keyed by IP address and email | to slow down guessing and floods | Yes. Kept for 7 days |

Sign-ups that are never verified are deleted after 7 days.

The service sits behind Cloudflare, which passes requests on to the server and so handles IP addresses and request headers on the service's behalf. The server sends verification and reset emails through its hosting provider's mail system.

We do not sell your data, share it with advertisers, use it for advertising, analytics or profiling, or combine it with anything else.

## Deleting your data

- **Delete your account**, in the app (version 0.4.0 and later): *Cloud sync → (signed in) → Delete account…*. You confirm with your account password. This removes, at once: the cloud copies of all your profiles, your sharing key, every profile you shared (the people you shared it with lose access), your membership in other people's shares, your sessions, and the log entries of your account. Records you wrote into someone else's shared profile stay with that profile, with no writer named. Profiles on your devices are not touched.
- **Log lines written after that** (the deletion itself, which carries no email, and any later sign-in attempt for that email) stay for up to 90 days. Backups taken around updates of the server software are kept only briefly.
- **Remove one profile's cloud copy**, in the app: the profile's menu on the profile picker, *Delete from cloud…*.
- **Delete what is on a device** by removing the app (on the Windows portable version, the folder next to the program).
- If you use an older version, or can't open the app any more, contact us (below) and we will delete the account when you can show it is yours.

## Security

All traffic to the sync service uses HTTPS. Your master password and your sharing passphrase never leave your device, and the keys that open your profiles are never stored on the server in a form it can open. The server limits sign-in attempts, locks an account for a while after repeated wrong passwords, and needs your password again for the most sensitive actions (replacing your sharing key, deleting your account).

## Children

Submarine is a tool for system administrators and developers. It is not directed at children, and we do not knowingly collect data from them.

## Changes to this policy

When this policy changes, the date at the top changes too. The full history of changes is in the repository.

## Contact

Questions or requests about your data: open an issue at [github.com/SinaXhpm/Submarine/issues](https://github.com/SinaXhpm/Submarine/issues) (please don't post private information there) or write through [sinaxhpm.com](https://sinaxhpm.com).
