# DefaultFast

A small, fast, portable Windows 10 (1809+) / 11 utility to **set and repair default apps**.
Type a program, a file type (`.pdf`) or a protocol (`https`), pick, done. It also finds and
fixes broken associations — including the "`.bat` / `.cmd` has no handler" bug where
double-clicking a script shows *Select an app to open this file*.

Single exe, no installer, no runtime dependencies, no network access, no telemetry. Runs as a
normal user; only system-wide repairs ask for admin (UAC), one step at a time.

## Build

Prerequisites: Rust 1.88+ (stable, `x86_64-pc-windows-msvc`) and Visual Studio Build Tools with
*Desktop development with C++* (MSVC linker + Windows SDK, which provides `rc.exe`).

```powershell
cargo build --release          # -> target\release\DefaultFast.exe
cargo test                     # 31 unit tests (search, hash, repair, backup, registry policy)
.\scripts\measure.ps1 -Runs 5  # budgets on this machine
```

No CI is configured on purpose; everything builds locally.

## Use

| Action | How |
| --- | --- |
| Search | Just type. `Ctrl+K` or `/` refocuses the search box. |
| Navigate results | `↑` / `↓`, `Enter` opens the detail pane and moves focus into it |
| Program detail | Check file types/protocols (`Space`), then **Set as default for selected** (`Ctrl+Enter`) or **Make default for everything** |
| File type / protocol detail | **Set default** next to any app (`Enter` / `Ctrl+Enter` on the selected one), **Browse for another program…** for any `.exe` |
| Back / clear | `Esc` (detail → search box; search box → clear; empty → home) |
| Undo | The toast's **Undo**, or **History → Undo last change / Restore** |
| Repair | Footer status ("All good" / "3 issues · Fix") → **Health check** |

Command line: `DefaultFast.exe .pdf` (opens with that search), `--repair` (opens the health
check), `--self-test`, `--diagnose`, `--startup-probe` (see *Diagnostics*).

Programs that declare no file types (portable apps, many editors) get an **Add type** box in
their detail pane: type `.txt`, and DefaultFast registers the program properly first.

## How setting a default works (and why it can break)

Windows stores per-user defaults in
`HKCU\…\Explorer\FileExts\<ext>\UserChoice` (protocols: `…\Shell\Associations\UrlAssociations\<proto>\UserChoice`)
as `ProgId` + `Hash`. A `ProgId` without the right `Hash` is ignored or reset, and
`IApplicationAssociationRegistration::SetAppAsDefault` is blocked for non-system callers.

DefaultFast computes the hash with the algorithm reverse-engineered by
[SetUserFTA](https://kolbi.cz/blog/2017/10/25/setuserfta-userchoice-hash-defeated-set-file-type-associations-per-user/) /
[PS-SFTA](https://github.com/DanysysTeam/PS-SFTA) (also used by Firefox):

1. `input = lowercase(ext_or_protocol + user SID + ProgId + hex(key last-write FILETIME, truncated to the minute) + "User Choice set via Windows User Experience {D18B6DD5-6124-4341-9318-804003BAFA0B}")`, UTF-16LE with the terminating NUL;
2. MD5 of that gives two seeds for two multiply/rotate checksum passes over the input;
3. the 8 resulting bytes are base64-encoded.

All of it lives in `src/userchoice.rs` (pure Rust, unit-tested against PS-SFTA vectors). Before
writing anything, DefaultFast checks its hash against the UserChoice keys **Windows itself
wrote on this machine** (on the development machine: 73 of 73 matched). A change is only
reported as done when Shell (`AssocQueryStringW`, ProgID/executable) confirms the new default;
otherwise that target is rolled back.

**Why it may need updates:** the algorithm is undocumented. Microsoft can change it in any
feature update. Recent Windows 11 builds add `UserChoiceLatest` with a different, unpublished
hash, and the UCPD driver blocks third-party writes for `http`, `https` and `.pdf`. DefaultFast
never fakes those: if the hash check fails, `UserChoiceLatest` is present, a write is denied or
Shell doesn't confirm the change, you get a plain-language toast with **Open Settings**
(deep-linked to the app's page when it is registered), and nothing is left half-changed.

To make a choice valid and persistent for apps that don't register a ProgID, DefaultFast first
creates `HKCU\Software\Classes\Applications\<exe>\shell\open\command`, `SupportedTypes` and the
extension's `OpenWithProgids` entry — exactly what Windows' own "Choose another app" does.

## Repair

The startup health scan (background thread) and **Health check** find:

- known Windows types whose merged `HKCR\<ext>` default is empty (the `.bat`/`.cmd` bug — an
  empty `HKCU\Software\Classes\.bat` shadowing HKLM, or a wiped HKLM default),
- critical types (`.exe`, `.lnk`, `.bat`, `.cmd`, `.msi`, `.reg`) mapped or user-overridden to
  something else,
- per-user defaults naming an uninstalled ProgID,
- UserChoice entries pointing to a missing ProgID or missing executable, or with an invalid hash.

Known-good table: `.bat→batfile`, `.cmd→cmdfile`, `.exe→exefile`, `.lnk→lnkfile`,
`.txt→txtfile`, `.ps1→Microsoft.PowerShellScript.1`, `.vbs→VBSFile`, `.js→JSFile`,
`.msi→Msi.Package`, `.reg→regfile`, `.url→InternetShortcut`, `.zip→CompressedFolder`.
Fix: (1) delete `HKCU\…\FileExts\<ext>` (UserChoice, UserChoiceLatest, OpenWithList…),
(2) delete `HKCU\Software\Classes\<ext>`, (3) set `HKLM\SOFTWARE\Classes\<ext>` to the ProgID —
**only if it is wrong**, via the elevated helper — (4) verify the ProgID can open files,
(5) `SHChangeNotify(SHCNE_ASSOCCHANGED)`. Explorer is never restarted.

## Elevation

The app never runs elevated. When a fix needs HKLM, a dialog explains why first, then
`DefaultFast.exe --elevated-fix .bat .cmd` is started with ShellExecute `runas` (one UAC
prompt for all of them). The helper accepts only extensions from the known-good table and the
registry layer still refuses any HKLM write other than `HKLM\SOFTWARE\Classes\.<ext>`.

## Backups, Undo and logs

Everything lives in `%LOCALAPPDATA%\DefaultFast`:

- `backups\*.reg` — before **any** registry write, the affected keys are exported in regedit's
  own format (you can double-click them too). Undo/Restore replay them, regenerating UserChoice
  hashes instead of replaying stale ones, and back up the current state first. The newest 50
  are kept.
- `log.txt` — every registry write and its result (256 KB, one rotation to `log.old.txt`).
- `window.txt` — last size/position.

DefaultFast writes nothing outside the file-association keys and this folder; the allowlist is
enforced in `src/registry.rs` and unit-tested.

## Measured budgets

Release build, Windows 11 (build 26100), warm launches, `scripts\measure.ps1 -Runs 5`:

| Budget | Target | Measured | |
| --- | --- | --- | --- |
| Cold start to interactive window | < 150 ms | **92–95 ms** to first presented frame | ✅ |
| Idle RAM (working set) | < 30 MB | **21.7 MB** after a search (≈11 MB private); 5.5 MB idle before any use | ✅ |
| Peak during the first index build | — | 39 MB (transient, then trimmed) | ℹ️ |
| Per-keystroke search + re-render | < 16 ms | **1.7–3.2 ms** (search alone ≈0.45 ms, 905 types + 160 apps + 174 protocols) | ✅ |
| Binary size | < 6 MB (ideally < 4) | **8.28 MB** | ❌ |

Index build (background, UI already interactive): file types/protocols ≈0.27 s, registry
programs ≈0.45 s, Start Menu + packaged apps ≈0.5 s.

## Trade-offs and deviations (priority: correctness > speed > size > polish)

- **Binary size over budget.** Measured before writing the UI: Slint 1.18 with the software
  renderer is 7.6 MB for an empty window (ICU + parley text layout can't be disabled). Slint
  1.12/1.14 are 2.8–3.7 MB but load every system font (36–38 MB RAM, over the RAM budget). Stock
  Slint 1.18 was chosen; a raw Win32 UI would meet the size budget at the cost of much more
  hand-written UI code and weaker accessibility.
- **Mica.** DWM backdrop, rounded corners and dark title-bar attributes are applied, but
  Slint's software renderer presents opaque pixels (softbuffer has no alpha), so the backdrop
  can't show through. The client uses a Mica-like solid tint instead. Skia/OpenGL could show it
  but costs several MB and GPU-driver RAM.
- **Window chrome.** Implemented the preferred way: a real native window (resize borders, Aero
  Snap, shadow, rounded corners) whose caption is replaced via `WM_NCCALCSIZE`/`WM_NCHITTEST`; the
  maximize button returns `HTMAXBUTTON` so the Windows 11 Snap Layouts flyout works. The window
  is subclassed from a CBT hook at creation, so the stock caption never flashes.
- **Packaged apps** come from the Shell AppsFolder (names, AUMIDs, icons) plus their registered
  Capabilities, instead of `PackageManager`: same data without parsing manifests or pulling in
  WinRT projections.
- **UserChoiceLatest** targets are never written (unpublished hash); they go to Settings.
- **Idle working set** is trimmed once after the one-off index build (Shell DLL pages used only
  by that scan). The table reports the number after real use, not just the trimmed one.
- **Protocol rows** use a globe glyph rather than an extracted icon.

## Diagnostics

- `--diagnose` writes `diagnostics.txt`: index counts/timings, the hash check against this
  machine's UserChoice keys, the health-scan result and the program list. Read-only.
- `--self-test` runs set → verify → undo → verify on an invented extension with invented
  `Applications\…` handlers, then deletes every key it made. Writes `self-test.txt`.
- `--startup-probe` measures the budgets and exits (used by `scripts\measure.ps1`).

## Architecture

| Module | Role |
| --- | --- |
| `registry` | `Registry` trait, Win32 impl (the only raw Reg* calls), in-memory mock, write allowlist, write log |
| `discovery` | Two-pass index: HKCR types/protocols, RegisteredApplications, `Applications\*`, App Paths, OpenWithProgids/List; then Start Menu + AppsFolder |
| `search` | Allocation-free fuzzy/prefix scoring, grouped results |
| `userchoice` | Hash, UserChoice read/validate/write, algorithm check |
| `defaults` | Set-default transaction: preflight, backup, registration, write, Shell verify, rollback |
| `repair` | Health scan, known-good table, fix plan |
| `backup` | `.reg` export/parse/restore, history, pruning |
| `elevated` | `runas` helper modes |
| `window` | CBT hook + subclass for the custom title bar, DWM attributes |
| `shell` | Thin COM/Shell wrappers (AssocQueryString, icons, shortcuts, dialogs, SID) |
| `ui`, `actions`, `repair_ui`, `worker` | Slint glue, button actions, background worker (all writes are serialized there) |

The UI thread never touches the registry or disk: scans, Shell queries, icon extraction and
writes run on background threads and post results back with `slint::invoke_from_event_loop`.

See [docs/TESTPLAN.md](docs/TESTPLAN.md) for the manual test plan and
[CHANGELOG.md](CHANGELOG.md) for versions.
