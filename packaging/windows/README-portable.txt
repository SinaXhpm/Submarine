Submarine - portable
====================

Run submarine.exe. Because the submarine-data folder sits next to it,
Submarine keeps everything it saves in that folder instead of your Windows
user profile: profiles, the cloud sign-in, the window position and the UI
preferences. Keep the two together in a folder you can write to - a USB
stick is fine. Remove submarine-data and Submarine goes back to the usual
per-user folders, exactly like the installed build.

Bringing data over from an installed copy: close Submarine, then copy two
things into submarine-data:

  1. The contents of %APPDATA%\com.submarine.app - all except
     sync_device.json, which identifies each install to cloud sync (the
     portable copy creates its own).
  2. The folder %LOCALAPPDATA%\com.submarine.app\EBWebView. It holds the
     app's settings, including Auto-sync. Without it Auto-sync starts out
     on and the first profile you open is uploaded to your cloud - so if
     you skip it, turn Auto-sync off in Settings before opening a profile
     you keep only on this computer.

submarine-data then holds your encrypted profiles and your cloud sign-in
token, so keep it somewhere only you can read.

Needs the Microsoft Edge WebView2 runtime, a Windows system component that
is preinstalled on Windows 11 and current Windows 10.

More detail: https://github.com/sinaxhpm/submarine#portable-mode-windows
