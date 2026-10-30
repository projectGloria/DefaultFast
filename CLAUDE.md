# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

DefaultPicker: a portable, single-exe Rust/Win32 file-association picker for Windows 10/11 x64. `windows` (0.62) is the **only** direct dependency — keep it that way. No installer, service, networking, or telemetry. Static CRT via `.cargo/config.toml` (`+crt-static`), manifest embedded by `build.rs` from `app.manifest` (PerMonitorV2 DPI, asInvoker, ComCtl v6).

## Commands

Requires stable Rust 1.88+ (`x86_64-pc-windows-msvc`) and VS Build Tools with the Windows SDK.

```powershell
.\build.ps1 -Test                 # cargo test + release build, copies to dist\DefaultPicker.exe
.\build.ps1 -Profile small        # opt-level="z" variant
cargo test --locked               # all unit tests
cargo test <name>                 # single test, e.g. cargo test hash::tests
cargo clippy --all-targets -- -D warnings
```

Release builds use the GUI subsystem (no console); debug builds keep the console. Diagnostic modes must be awaited from PowerShell and write reports to `%LOCALAPPDATA%\DefaultPicker`:

```powershell
Start-Process .\dist\DefaultPicker.exe -ArgumentList '--diagnose' -Wait               # diagnostics.txt
Start-Process .\dist\DefaultPicker.exe -ArgumentList '--self-test-association' -Wait  # SelfTests\...\self-test.txt
.\scripts\measure.ps1 -Runs 5    # runs --startup-probe N times + --benchmark
```

Other flags: `--benchmark` (scoring over 6,000 synthetic entries), `--startup-probe` (exits after first GPU frame), `--software` (software Direct2D target). `--self-test-association` uses an artificial, process-specific extension/ProgIDs under HKCU and cleans up; it never touches real file types.

## Architecture

**Unsafe boundary.** `main.rs` has `#![deny(unsafe_code)]`; only `src/ffi/` re-allows it. All raw pointers, COM, registry, window procs, and Direct2D/DirectWrite live in `ffi/mod.rs` (OS wrappers, RAII handles, `ComApartment`) and `ffi/ui.rs` (window, native controls, rendering). Everything else is safe Rust consuming owned data returned by `ffi`. New OS calls go into `ffi`, not into the domain modules. Registry *mutation* wrappers in `ffi` only accept HKCU paths — preserve that invariant.

**Threading / event flow.** `ui::workers()` spawns background threads (each with its own `ComApartment`): a catalog worker (cache load → `registry_scan` → `app_scan`, streaming batches) and an icon worker. They send `ui::Event` values over an `mpsc` channel and call a `wake` closure that `PostMessageW`s `WM_APP+1` (`WAKE`) to the main window; `ffi/ui.rs` drains the channel on that message. Association changes/undo (`assoc::set`, `assoc::undo`) also run off-thread and report back as `Event::Result`. Refresh is triggered by registry change notifications, not polling. Never block the UI thread with scans or `AssocQueryString` lookups.

**Rendering.** First frame uses plain native controls; Direct2D/DirectWrite init happens on a worker and the UI swaps to GPU rendering afterwards. Native Edit and owner-data ListView are kept for keyboard behavior and accessibility. Icons are requested only for visible rows; CPU pixel and GPU bitmap caches are each 48-entry LRUs.

**Association writes (`assoc` + `hash`).** Flow: compatibility preflight (rejects `UserChoiceLatest`, UCPD-protected types, existing incompatible hashes, denied ACLs) → write undo journal to `undo.bin` *before* any registry write → HKCU registration → UserChoice write with legacy v1 hash → readback → effective verification via `AssocQueryString`. A registry write alone never counts as success; failures fall back to "Open Settings". Undo regenerates the hash rather than replaying the old one, and checks for intervening external changes. Never seize ACLs/ownership or automate Settings. `.bat`/`.cmd` use a separate path (`set_batch_verb`) that overrides the ProgID open command for Windows Terminal, behind explicit user confirmation.

The UserChoice hash (`hash.rs`) is undocumented: lowercase ext + user SID + ProgID + key FILETIME rounded down to the minute + fixed UX string, UTF-16LE incl. NUL, MD5 (CNG via `ffi`) seeding two wrapping-u32 streams, base64. Minute-boundary writes are retried. Fixed test vectors in `hash.rs` were generated with PS-SFTA — don't change them to make a refactor pass.

**Cache (`cache.rs`).** Versioned binary format (`MAGIC = b"DFPK\x02..."`, 16 MiB cap, checksum, atomic replace). Bump the version byte in `MAGIC` when changing the `FileType`/`App` encoding. Files: `catalog.bin`, `window.bin`, `undo.bin`.

**Search (`search.rs`).** Must stay allocation-free on the hot path (exact/prefix/word-boundary/substring/bounded fuzzy scoring into reused storage); `--benchmark` measures it.

## Gotchas

- `.gitignore` contains a bare `config.toml` pattern, which also ignores `.cargo/config.toml` — the file that enables the static CRT. Commit it explicitly (`git add -f`) or tighten the pattern.
- README links `docs/verification.md`, which does not exist in the repo yet. README states the memory budgets from the original brief are **not met**.
- `dist/` and `target/` are build output (ignored).
- Discovery excludes UNC/mapped-network executables; keep that exclusion in both discovery and registration.
