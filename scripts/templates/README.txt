HoldMyRAM {{VERSION}}
=====================

What it is
----------
HoldMyRAM is a small Windows tray app that frees RAM on demand. You pick which
apps, startup items, scheduled tasks and services it may close. Each item shows
a risk rating (Suggested / Caution / Dangerous / Unrated) with a short
explanation. Press a global hotkey to close the selected set; press it again to
restore what was closed.

Requirements
------------
- Windows 10 or 11, 64-bit.
- The Microsoft Edge WebView2 runtime. It is built into Windows 11 and most
  up-to-date Windows 10 installs. If the window does not open, install it from
  https://developer.microsoft.com/microsoft-edge/webview2/

How to run
----------
1. Unzip the folder anywhere you like.
2. Keep holdmyram.exe and holdmyram-helper.exe together in the same folder.
   The helper refuses to run unless holdmyram.exe sits next to it.
3. Double-click holdmyram.exe. There is no installer and no window at start;
   look for the HoldMyRAM icon in the system tray (it may be under the ^ arrow).
4. Click the tray icon to open the window and choose what to close.
5. The default hotkey is Ctrl+Alt+R. You can change it in Settings.
   The app you are currently using is never closed by the hotkey.

Administrator prompt (UAC)
--------------------------
Stopping Windows services and some scheduled tasks needs administrator rights.
HoldMyRAM itself always runs as your normal user. Only when you toggle a
service or task, a short-lived helper (holdmyram-helper.exe) asks Windows for
elevation. The UAC prompt will say "Unknown publisher" because the program is
not code-signed. You can decline it; app-only toggling still works.

SmartScreen warning
-------------------
Because the exe is unsigned, Windows SmartScreen may say "Windows protected
your PC". Choose "More info" and then "Run anyway". If the zip came from the
internet you can also right-click it, open Properties, and tick "Unblock"
before unzipping.

Where your data lives
---------------------
- Settings, profiles, risk overrides: %APPDATA%\HoldMyRAM
- Restore journal and state:          %LOCALAPPDATA%\HoldMyRAM
Paste either path into the Explorer address bar to open it. Settings can be
exported and imported from the app as a TOML file.

How to uninstall
----------------
1. Right-click the tray icon and choose Quit. If items are currently closed,
   restore them first (toggle off).
2. Delete the HoldMyRAM folder you unzipped.
3. Optional: delete %APPDATA%\HoldMyRAM and %LOCALAPPDATA%\HoldMyRAM.
4. Optional: if you ticked Settings > Start HoldMyRAM when I sign in (off by default), remove the "HoldMyRAM" value
   under HKEY_CURRENT_USER\Software\Microsoft\Windows\CurrentVersion\Run
   (Task Manager > Startup apps also lets you disable it).
Nothing else is installed on your system.

Limits
------
- Unsigned: expect the SmartScreen and "Unknown publisher" prompts above.
- Not an installer and has no auto-update; replace the folder to upgrade.
- Windows x64 only.
- Closing apps can lose unsaved work in them. Review the ratings first, and
  treat Dangerous items with care.

Licence
-------
All rights reserved by the author; not yet licensed for redistribution.
Third-party components are listed in THIRD-PARTY-NOTICES.txt.

Updates
-------
HoldMyRAM checks the project's GitHub Releases for newer versions and offers to
install them. It verifies the download against SHA256SUMS.txt from the same
release. To update by hand: quit from the tray, then extract the new zip over
the existing folder (keep both exes together).
The checksum check protects against a corrupted download. It does not protect
against a compromised account or release, because the checksums come from the
same place as the file.
