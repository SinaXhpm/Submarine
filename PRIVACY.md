# Privacy policy

*Last updated: 14 November 2026.*

This policy covers the Submarine app (Windows, macOS, Linux, Android and iOS), the optional Submarine sync service that the app can use, and the website [submarine.sinaxhpm.com](https://submarine.sinaxhpm.com). It is also published, with translations, at [submarine.sinaxhpm.com/privacy/](https://submarine.sinaxhpm.com/privacy/).

**In short**

- Your servers, passwords, SSH keys and notes stay on your device, encrypted with a key made from your profile password. The app has no analytics, no advertising, no tracking and no crash reporting.
- A message you send with *Send feedback* reaches the developer with only what you typed in it.
- Cloud sync is optional. If you turn it on, the sync server keeps your account's email address and your profiles in encrypted form. It cannot read what is inside them.
- You can delete your account, and everything the server holds for it, from inside the app or on the website.
- The AI assistant is optional and does nothing until you add an AI provider with your own API key. Its chats go from your device to that provider, not to us. With a model on your own machine or network, they stay there.

## Who is responsible

Submarine is made by Sina Xhpm ([sinaxhpm.com](https://sinaxhpm.com)), who also runs the sync service at `submarine.sinaxhpm.com` (older versions of the app use `api.sinaxhpm.com`). The source code of the app is public at [github.com/SinaXhpm/Submarine](https://github.com/SinaXhpm/Submarine) (MIT license), so what this policy says about the app can be checked in the code.

## What stays on your device

- **Your profiles** (servers, logins, SSH keys, tunnels, notes, commands, known host keys, and passwords you choose to save from a login prompt) are stored in the app's data folder. They are encrypted with AES-256-GCM, using a key made from your profile password with Argon2id. The profile password is never stored and never sent anywhere. There is no way to recover it: if you forget it, the profile cannot be opened.
- **Quick unlock** (optional, off by default): if you turn on Windows Hello or an Android fingerprint, the profile's key is sealed to a key held by the system (the TPM or the Android Keystore). The system checks your fingerprint or PIN; the app never sees it. On Windows you can instead choose to open a profile automatically (also off by default): its key is then protected by Windows for your account, with no check at all, so anyone or any program using that Windows account can open it.
- **The AI assistant's data** (optional, see below): your AI providers and their API keys, your chats, the permission levels and always-run commands you set, and the assistant's activity log. They are kept in a file of their own beside the profile, encrypted with AES-256-GCM using a key made from the profile's key, so they open when the profile opens. This file is never synced, never part of a shared profile and never in an exported profile.
- **Settings** such as the window position, the last-used profile name and the colour theme, and choices made for this device only (a remote desktop's speed and quality, whether it shares the clipboard, the folder or drive shared with an RDP server, and which servers always get a terminal with its own login) are stored in the app's own settings, not encrypted, and never synced.
- **The clipboard** is read only when you paste. What you paste goes to the server you paste it into: a terminal, or a remote desktop's tab. With *Share the clipboard* on (the default for RDP and VNC servers), text copied on the remote desktop is put on this device's clipboard. Nothing from the clipboard is ever sent to us.
- **Remote Desktop Connection (Windows)** gets a server's saved password in a short-lived .rdp file. The password in it is encrypted with Windows DPAPI for your Windows user, and the file is deleted once it has been read. If you let the RDP app remember a login itself, Windows keeps it, not Submarine.
- **The activity log** in the app lives in memory and goes away when the app closes. The AI assistant keeps a log of its own (see below).

## What the app connects to

- **Your own servers.** The app connects only to the hosts you add or open, using SSH, SFTP, FTP or FTPS, Telnet, VNC or RDP, through any proxy or jump host you set up. What you type and transfer goes to that server, not to us. For a remote desktop, that includes the clipboard text you paste into it, and a folder or drive of this computer if you choose to share one with the built-in RDP client. Plain FTP, plain Telnet and plain VNC are not encrypted, and the app says so.
- **The sync service**, only if you sign in to cloud sync (see below). You can point the app at a server of your own instead; then that server's operator handles the data.
- **The AI providers you add**, only if you use the AI assistant (see below). Requests go from your device straight to the provider, or through a proxy or one of your servers, as set for that provider. Submarine has no AI service of its own, and the sync service is not involved.
- **GitHub**, to look for a newer version (not in the iOS app, the Mac App Store version or the Flatpak, which their stores keep up to date). When the profile list opens, and when you press "Check for updates", the app asks GitHub's public list of releases (`api.github.com`) whether a newer version exists. The request carries the app's name and version (`submarine-app/x.y.z`) and nothing else; GitHub sees your IP address, as with any web request.
- **The developer's server, when you send feedback** (see below), and only then.
- **Links** you click open in your system browser.

Nothing else leaves your device. The app contains no third-party analytics, advertising or tracking code.

## Cloud sync (optional): what the sync server keeps

If you create an account, the server stores:

| What | Why | Can the server read it? |
|---|---|---|
| Your email address | to sign you in, and to send sign-in and reset codes | Yes |
| Your account password | to sign you in | It receives the password over HTTPS when you sign in and stores only a salted bcrypt hash of it |
| Sign-in sessions: a hashed token, the IP address it was issued to, when it was last used, and an expiry (90 days) | to keep you signed in | The token is hashed; the rest is readable |
| Your profiles' records | to sync them between your devices | **No.** Each record is encrypted on your device with the profile's key before it is uploaded. The server sees an ID for each record, its type (server, login, key, note…), timestamps (which include a random ID of the device that made the change), a deleted flag and the size, plus the profile's name and how many records it has |
| A copy of each profile's key, sealed with your profile password | to bring a profile onto a new device | Not without your profile password. Someone who got the server's data could try to guess a weak password offline, so use a long one |
| Sharing: your public key, your private key wrapped with your sharing passphrase, the names of shared profiles, the emails and roles of the people you share with, and the keys sealed to them | to share a profile with other accounts | The names and emails, yes; the keys, no |
| A log of account events: what happened (sign-up, sign-in, failed sign-in, sign-out, lock-out, password reset, a sync, a share invitation or change, and similar), the email address when it is known, the IP address, the time and Cloudflare's request ID. Never what was synced | to protect accounts and stop abuse | Yes. Kept for 90 days, then deleted |
| Rate-limit counters, keyed by IP address and email | to slow down guessing and floods | Yes. Kept for 7 days |

Sign-ups that are never verified are deleted after 7 days.

The service sits behind Cloudflare, which handles the HTTPS connection for it and passes requests on to the server. So Cloudflare processes IP addresses, request headers and request contents, including your sign-in details and your already-encrypted records, on the service's behalf. The server sends verification and reset emails through its hosting provider's mail system.

We do not sell your data, share it with advertisers, use it for advertising, analytics or profiling, or combine it with anything else.

## Feedback you send from the app

*Send feedback* (in Settings, on the profile list, and *Suggest a fix* under Language) sends a message to the developer. It is sent only when you press *Send*, and it contains only what you typed in the form:

- your message;
- a name, if you give one;
- an email address, if you give one, used only to answer that message.

Nothing else is added: no logs, no list of your servers, no app version or system details, no account and no device ID. The message goes over HTTPS to the same server as cloud sync (`submarine.sinaxhpm.com`), also when you sync with a server of your own, and through Cloudflare like every request to it. The server uses your IP address against floods and does not keep it with the message: it goes into the rate-limit counters (kept for 7 days), and, only if more messages than allowed come from it, into the log of account events (kept for 90 days).

The developer reads the messages. They are kept for 180 days and then deleted automatically; one can be deleted sooner on request. After sending, the app shows the message's report number: quote it to have that message deleted (see Contact). Messages are not shared with anyone and are not used for anything but reading and answering them.

## AI assistant (optional)

The assistant does nothing until you add an AI provider in *Settings → AI assistant*: Anthropic, OpenAI, another OpenAI-compatible service (such as OpenRouter, DeepSeek, Groq or Google Gemini), or a model on your own machine or network (such as Ollama or LM Studio). You use your own account and API key with that provider, if it needs them. Submarine has no AI service of its own, and we never see your chats or your keys.

- **Where requests go.** From your device straight to the provider, or through the system's proxy or one you set for it, or out through one of your servers over its open SSH session. Through a server, the provider sees that server's address. Requests use HTTPS, which stays encrypted from your device to the provider, so a proxy or server in between sees only which host is contacted and how much goes through. Plain http (not encrypted) is allowed only to an address on your own machine or network, or, through one of your servers, on that server's own machine or network. Over plain http, anyone on that network can read the chat and any API key sent with it, and on another network the same address can belong to someone else.
- **What the provider receives.** The chat so far, with each request: your messages (with any terminal text you attach) and the assistant's replies. In a chat about a server, also what the assistant reads and does there (command output, file contents, folder listings, the results of its changes) and your answers when it asks for approval. The request carries your API key and the app's name and version, and, as with any web request, the provider sees the address it comes from. If you switch a chat to another provider, the whole chat goes to that one with your next message. *Test* and the requests that ask a provider about its models carry your key and nothing from your chats or servers.
- **Server facts.** When a chat about a server starts, the app collects a few facts about the server and sends them with the chat: the system's short name and version number (as in `debian 12`), the kernel's name and version, the processor type, the login user's name and whether it is in an admin group (sudo, wheel or admin), the home folder, the name of the login shell, and which of bash, systemctl, docker, apt, dnf, yum, apk and pacman are installed. The chat shows them under *Server facts sent to the assistant*. They don't include the server's host name or address, or the name you gave it, though the assistant may come across these in what it reads.
- **Private keys.** A private key block in command output (`-----BEGIN … PRIVATE KEY-----`, and the PGP, SSH2 and PuTTY forms) is replaced with `[private key removed by Submarine]` before it is shown or sent, and so is one in a file the assistant reads. This catches a key printed or read as a whole block; a command that prints only part of a key, or a key in another form, is not caught, so take it as a guard against accidents and not as a promise that no key can ever be sent. Other file contents, and terminal text you attach, are sent as they are, without hidden Unicode Tag characters: those can't be seen on screen and can carry instructions for a model, so they are taken out of everything the assistant is sent, your own messages included (the chat tells you when a message of yours held some). The assistant never gets the passwords and keys saved in your profile: it works only on the server, through the open session.
- **Your consent.** What a provider receives is under that provider's own terms and privacy policy. Before your first message goes to a provider, the app asks you, naming the provider's address, and it asks again if you change that address to another host, change the provider's type, or allow providers that may keep data. An API key saved for one host is never sent to another: when you change the host, the app asks for the key again. For some providers it adds a note: Gemini (on Google's free tier, Google may use the data to improve its products, and people may read it), DeepSeek (it stores the data in China) and OpenRouter (it passes requests on to other providers; when it is added with the OpenRouter preset, Submarine asks it to use only ones that don't keep or train on data, unless you allow them in the provider's settings). To withdraw your consent, choose *withdraw* next to the provider in *Settings → AI assistant*: a chat that is answering stops before its next request, and the app asks again before the next message. What a provider already received stays under its own rules.
- **A local model** (Ollama, LM Studio, or another server on your machine or network) keeps the data on that machine or network.
- **What is kept on your device**: your providers and their API keys, your chats with everything in them, the permission level of each server and the commands you chose to always run there, and the assistant's activity log. The log lists each command the assistant ran or asked to run and each folder or file it opened or changed, with when it happened and whether and how it was allowed, and keeps the earlier content of each changed file so the change can be undone. Log entries older than 90 days are removed when the assistant's file is opened and each time a new entry is added. All of it is in the assistant's own encrypted file (see *What stays on your device*).
- **Report this reply** (the flag under each reply) copies the reply to your clipboard and, once you confirm, opens the project's issue page on GitHub in your browser. The app sends nothing itself. What you post there is public, so leave out private data from your servers.

## AI agents on this computer (optional)

On a computer, *Settings → AI agents* lets an AI agent that runs on the same computer (such as Claude Code) work on your servers through Submarine. It is off until you turn it on.

- **Who can connect.** Only programs running as your user on this computer, through a channel that other users and the network can't open. Submarine shows you which program asks, and what started it.
- **Nothing without your approval.** An agent asks for access, and you choose in Submarine's window the server, the operations, the folders, how long (8 hours at most) and how its operations run. Access ends when that time is up, when the agent disconnects, when you revoke it, lock the profile or close the app.
- **What an agent receives.** The results of the operations you allow: command output, and folder listings and file contents from the folders you chose. Never your passwords, keys or other saved secrets. The address Submarine connects to is replaced in what the agent sees; other addresses a server prints can still appear.
- **Where it goes from there.** What the agent does with what it receives, for example sending it to its own AI provider, is under that agent's terms and privacy policy.
- **What is kept on your device.** Each operation an agent asks for is listed in the assistant's activity log, with the agent's name, on this device.

## This website

The website uses no cookies, no analytics, no advertising and no third-party scripts or fonts. Like any website, Cloudflare and the hosting provider process your IP address, the page you ask for and your browser's details in order to deliver the page, and the hosting provider keeps standard server logs for a limited time.

If you delete your account on the website, what you type is sent only to the sync service, over HTTPS, and is not stored by the page.

## Deleting your data

- **Delete your account** in the app (version 0.4 and later): *Cloud sync → (signed in) → Delete account…*, or on the website's [delete account page](https://submarine.sinaxhpm.com/delete-account/). You confirm with your account password. This removes, at once: the cloud copies of all your profiles, your sharing key, every profile you shared (the people you shared it with lose access), your membership in other people's shares, your sessions, and the log entries of your account. Records you wrote into someone else's shared profile stay with that profile, with no writer named. Profiles on your devices are not touched.
- **Log lines written after that** (the deletion itself, which carries no email, and any later sign-in attempt for that email) stay for up to 90 days. Backups taken around updates of the server software are kept only briefly.
- **Remove one profile's cloud copy** in the app: the profile's menu on the profile list, *Delete from cloud…*.
- **Delete a feedback message** before its 180 days: contact us with its report number.
- **Delete the AI assistant's chats**: one chat from its menu (*Delete*), or all of them with *Delete all* under the chat list on the AI page. The assistant's activity log outlives its chats unless you choose *Clear them* when you delete a chat; clear it any time with *Clear the log* in its *Activity* view, for one server or for every server. To delete a provider's API key, choose *Remove* next to the key in the provider's settings and save, or delete the provider, in *Settings → AI assistant*. Deleting a profile deletes the assistant's file with it. For what a provider received, its own rules apply: ask the provider.
- **Delete what is on a device** by removing the app (for the Windows portable version, the folder next to the program).
- If you use an older version, or can't open the app any more, contact us (below) and we will delete the account when you can show that it is yours.

## Security

All traffic to the sync service uses HTTPS. Your profile password and your sharing passphrase never leave your device, and the keys that open your profiles are never stored on the server in a form it can open. The server limits sign-in attempts, pauses password sign-in for a while after repeated wrong passwords, and needs your password again for the most sensitive actions (replacing your sharing key, deleting your account). The [security page](https://submarine.sinaxhpm.com/security/) explains all of this in detail.

## Children

Submarine is a tool for system administrators and developers. It is not directed at children, and we do not knowingly collect data from them.

## Changes to this policy

When this policy changes, the date at the top changes too. The full history of changes is in the repository.

## Contact

Questions or requests about your data: open an issue at [github.com/SinaXhpm/Submarine/issues](https://github.com/SinaXhpm/Submarine/issues) (please don't post private information there), or write through [sinaxhpm.com](https://sinaxhpm.com).
