# DefaultPicker

A portable Rust / Win32 file-association picker for Windows 10/11 x64. Only `windows` is a direct Cargo dependency. The CRT is linked statically; the app uses Windows' own system DLLs. No installer, service, tray process, telemetry, or networking client.

This is a working implementation with explicit Windows fallbacks. **The strict memory budgets in the brief are not met.** See [verification and remaining limitations](docs/verification.md) for measured results and the requirement checklist.

## Build

Prerequisites: stable Rust 1.88+ with the `x86_64-pc-windows-msvc` target, and Visual Studio Build Tools with **Desktop development with C++** and a Windows 10/11 SDK. Run in a PowerShell terminal:

```powershell
.\build.ps1 -Test
```

Output: `dist\DefaultPicker.exe`. Copy this single file anywhere and launch it without elevation. Cargo resolves build dependencies on the first build; the executable requires no Cargo/Rust installation. `cargo build --release --locked` is also sufficient if Cargo is on PATH.

`-Profile small` builds the `opt-level="z"` variant. The default uses `opt-level=3`; both use fat LTO, one codegen unit, stripped symbols, abort-on-panic, and the static CRT.

## Use

Search for `.bat`, `bat`, `batch`, or another extension or friendly name. Click a file type, or select with arrows and Enter. Search the app chooser; select an app and press Enter or **Set as default**. **Browse…** accepts a local executable. Escape closes the chooser; Tab navigates native controls. **Undo** and Ctrl+Z restore the last recorded change, including after restarting the app.

Settings opens through **Open Settings**. Selecting an app there is a user action: this app does not drive Settings, simulate keys, or bypass Windows protection.

Executable classes receive different treatment. `.bat`/`.cmd` offer **Launch via Terminal**, with an explanation and explicit confirmation. That overrides the current user's ProgID open command and can affect other extensions sharing the ProgID. It does not change default-app metadata. It only supports an installed Windows Terminal launcher; unknown ProgIDs and other executable classes use Settings. No scripts are executed during verification.

## Architecture

| Module | Responsibility |
| --- | --- |
| `registry_scan` | HKCR merged extension discovery, normalized names, effective ProgIDs, friendly names and icon locations |
| `search` | Allocation-free exact / prefix / word-boundary / substring / bounded fuzzy scoring; reused result storage |
| `app_scan` | Applications, App Paths, capabilities, OpenWith lists, Shell AppsFolder packaged identities, execution aliases, and resolved Start Menu shortcuts |
| `assoc` | Compatibility preflight, journal-before-write, HKCU registration, UserChoice set/readback/effective verification, rollback and persistent Undo |
| `hash` | Legacy UserChoice v1 formatting and mixer; CNG MD5 and Windows lowercase at the FFI boundary |
| `cache` | Bounded, versioned binary catalog with checksum, atomic file replacement, settings and Undo storage |
| `ui` | Safe background scanning, progressive events, icon worker, diagnostics and benchmark |
| `ffi` | Documented Windows/COM/registry boundary, RAII resources, native controls and Direct2D/DirectWrite rendering |

The first frame uses functioning native controls while GPU and font initialization runs on a worker. The UI then switches to Direct2D/DirectWrite rendering. Native Edit and owner-data ListView controls retain their keyboard behavior and Windows accessibility proxies. Only visible rows request icons; CPU icon pixels and per-target GPU bitmaps each have a 48-entry LRU bound. No full icon catalog is decoded at startup.

Scans and effective-association lookups run on workers. Cache data appears before refresh results; changed rows stream in, and the completed catalog removes stale records. Registry notifications re-arm before refresh. There is no polling loop. Animation timers exist only for active 180 ms popup or row transitions; reduced-motion settings suppress them.

This implementation uses the Win32 compositor for popup fade/slide instead of an explicit DirectComposition visual tree, preserving the native child controls without snapshotting them. It requests DWM rounded corners, dark title bars and the Windows 11 system backdrop; opaque client rendering currently limits the visible Mica effect. These deviations are listed in the checklist.

App discovery covers installed registrations and shortcuts across the computer. It does not recursively crawl every directory for unregistered portable executables; use Browse for those. Some packaged apps cannot handle an arbitrary extension. Their registered AppX handlers are usable when advertised; otherwise the app gives a Settings fallback. Publisher text comes from executable version resources when available. UNC and mapped network executable paths are excluded from discovery and registration.

## Association correctness and Windows limits

Legacy v1 hashes use the lowercase extension, current user SID, ProgID, the key's UTC FILETIME rounded down to a minute, and the fixed User Experience string. The UTF-16LE input includes its NUL terminator. MD5 supplies mixer seeds; the two wrapping-32-bit checksum streams produce eight bytes, encoded as base64. Minute-boundary writes are retried and read back. The effective ProgID is then checked through `AssocQueryString`; a registry write alone never counts as success.

The hash is undocumented and is **not a universal Windows 11 API**. Existing incompatible hashes, `UserChoiceLatest`, running UCPD protection for PDF, denied registry permissions, and failed effective verification produce an explanation and a Settings fallback. ACLs and ownership are never seized. Read [Mozilla's Windows restrictions](https://firefox-source-docs.mozilla.org/widget/windows/shell/set-default.html) and Microsoft's [default-association API limitation](https://learn.microsoft.com/en-us/windows/win32/api/shobjidl_core/nf-shobjidl_core-iapplicationassociationregistration-setappasdefault).

All registry mutations are under HKCU. Undo stores prior registry values and the prior effective association, regenerating a hash instead of replaying an obsolete hash. It checks for another application's intervening changes and retains the log if recovery cannot be confirmed. Undo is one level. Empty registration keys may remain after values are restored; no unrelated key tree is removed. Concurrent changes by other processes cannot be made atomic with a file journal.

Algorithm references: [Mozilla's implementation](https://github.com/mozilla-firefox/firefox/blob/main/browser/components/shell/WindowsUserChoice.cpp), [PS-SFTA reference](https://github.com/DanysysTeam/PS-SFTA). Fixed test vectors were calculated independently with PS-SFTA's hash-only function; no downloaded association-setting script is invoked by this application.

## Data, diagnostics and measurement

Local data lives in `%LOCALAPPDATA%\DefaultPicker`: `catalog.bin`, `window.bin`, `undo.bin`, and measurement reports. Deleting `catalog.bin` causes a rescan. Window bounds are restored only when plausibly visible. Keep `undo.bin` if you want to restore the last change.

The release executable uses the Windows GUI subsystem. PowerShell should wait explicitly for diagnostic modes:

```powershell
Start-Process .\dist\DefaultPicker.exe -ArgumentList '--diagnose' -Wait
Get-Content "$env:LOCALAPPDATA\DefaultPicker\diagnostics.txt"

.\scripts\measure.ps1 -Runs 5
```

`--benchmark` measures scoring and sorting over 6,000 entries, using 800 samples. The UI separately records edit-change-to-list-EndDraw times in `session.txt` on exit. These are different measurements. `--startup-probe` closes itself after the first GPU frame. Reports distinguish first native frame, first GPU frame, process lifetime, and working set; they exclude neither measurement overhead nor shared system DLL pages from working set. Repeated launches do not establish an OS cold-cache result.

```powershell
Start-Process .\dist\DefaultPicker.exe -ArgumentList '--self-test-association' -Wait
Get-ChildItem "$env:LOCALAPPDATA\DefaultPicker\SelfTests" -Filter self-test.txt -Recurse
```

The integration mode creates a process-specific artificial extension and ProgIDs, verifies set and Undo, tests unknown-format rejection and external-choice conflict, then removes its isolated registry keys. Its Undo directory is separate. It never changes a real file type or launches a handler.

`--software` requests a software Direct2D target for troubleshooting. It does not remove the Windows Shell or DirectWrite memory costs.

Eight unit tests cover scoring, extension normalization, binary cache round-trip/corruption/truncation, RFC MD5, base64, minute rounding, and three fixed UserChoice vectors. Run `cargo clippy --all-targets -- -D warnings` for static checks. See the verification document for the final measurements.
