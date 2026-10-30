# Manual test plan

Run on a standard (non-admin) user account unless noted. Before starting, note the current
defaults you'll touch; every change below must be undoable from the toast or **History**.
Automated coverage: `cargo test` (logic) and `DefaultFast.exe --self-test` (real write path on
an invented extension).

## 1. Set VLC as default for .mp4
1. Install VLC. Launch DefaultFast, type `vlc`, press `Enter`.
2. Detail pane lists VLC's file types; `.mp4` shows "Now: <current app>".
3. Check `.mp4` (`Space`), press `Ctrl+Enter`.
4. Expect a green toast "VLC media player now opens .mp4." with **Undo**; the row shows
   "Already the default".
5. In Explorer, double-click an `.mp4`: VLC opens. Right-click → Properties shows VLC.
6. Press **Undo**: the previous app is back (verify in Explorer).

## 2. Set a browser for https
1. Type `https`, `Enter`. Current default is shown; browsers are listed.
2. Click **Set default** on another browser.
3. Expected on most Windows 11 builds: UCPD protects `https`, so the toast says Windows kept the
   old app and offers **Open Settings**, which opens that browser's page in Settings. Nothing
   is left half-changed (`--diagnose` still shows a valid hash for `https`).
4. On builds without UCPD: green toast; clicking a link opens the new browser. Undo restores.

## 3. Repair .bat and .cmd from the "no handler" state
Setup (admin terminal, on a test VM — this breaks .bat on purpose):
```
reg add HKLM\SOFTWARE\Classes\.bat /ve /d "" /f
reg add HKLM\SOFTWARE\Classes\.cmd /ve /d "" /f
```
(or the per-user variant: `reg add HKCU\Software\Classes\.bat /f` with no default value).
1. Double-clicking a `.bat` now shows *Select an app to open this file*.
2. Launch DefaultFast: footer shows "2 issues · Fix" (startup scan).
3. Open it: ".bat files have no app" / ".cmd files have no app"; the HKLM variant shows a shield.
4. **Fix all** → explanation dialog → **Continue** → one UAC prompt (HKLM variant only).
5. Expect "Repaired .bat and .cmd."; footer "All good"; double-clicking a `.bat` runs it.
6. **History** shows "Repair .bat and .cmd"; **Undo** (with UAC) restores the broken state —
   proving the backup round-trips; fix again afterwards.

## 4. Undo a change
1. Make any change (test 1). Close DefaultFast, reopen it.
2. **History** → newest entry → **Undo last change**. Expect "Undone: …", entry marked "undone".
3. **Restore** on an older entry asks for confirmation and backs up the current state first
   (a new "Before restoring: …" entry appears).
4. `%LOCALAPPDATA%\DefaultFast\backups` contains `.reg` files that regedit can import.

## 5. Custom title bar
- Drag the empty top-bar area: window moves. Double-click it: maximize/restore toggles.
- Right-click the empty top bar, or click the app icon, or press `Alt+Space`: system menu.
- Hover the app icon: tooltip "DefaultFast <version>".
- Resize from all four edges and all four corners (top edge included).
- `Win+←/→/↑/↓`: snaps/maximizes/restores/minimizes.
- Hover the maximize button (Windows 11): Snap Layouts flyout appears; pick a layout.
- Close turns red on hover; `Alt+F4` closes; minimize works.
- Switch Windows between light/dark mode and change the accent color: the app follows live.
- Close and reopen: same size and position (and maximized state).

## 6. Standard user / elevation
1. Run from a standard account. Tests 1, 2 and 4 must never show UAC.
2. Only HKLM repairs (test 3, HKLM variant) show the explanation dialog, then UAC.
3. Cancel UAC: toast "The admin prompt was cancelled…"; per-user parts are done and undoable.
4. `log.txt` lists every registry write with OK/FAIL.

## 7. Keyboard and screen reader
- Everything above is doable without a mouse: `Ctrl+K`, arrows, `Enter`, `Space`,
  `Ctrl+Enter`, `Esc`, `Tab`.
- With Narrator: the search box, result items ("…, PDF file"), buttons, check rows ("…, currently
  …") and toasts are announced; icon glyphs are silent.
