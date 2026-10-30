# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

DefaultFast: portable Rust/Slint Windows utility to set and repair default apps (file types and
protocols). Windows 10 1809+ / 11 only, x86_64-pc-windows-msvc. The README is the product spec
summary (budgets, trade-offs); `docs/TESTPLAN.md` is the manual test plan.

## Commands

`cargo` lives in `~/.cargo/bin` (may not be on PATH in Git Bash).

```bash
cargo build --release                         # target/release/DefaultFast.exe — must be warning-free
cargo test                                    # all unit tests
cargo test userchoice::                       # one module; e.g. repair::tests::bat_cmd_no_handler_from_empty_user_override
powershell -File scripts/measure.ps1 -Runs 5  # budgets via --startup-probe
./target/release/DefaultFast.exe --diagnose   # read-only: index stats, hash check vs real keys, health scan
./target/release/DefaultFast.exe --self-test  # real write path on an invented extension, cleans up
```
Reports land in `%LOCALAPPDATA%\DefaultFast` (diagnostics.txt, self-test.txt, startup.txt, log.txt).
Release builds are GUI-subsystem: no console output. `DefaultFast.exe <query>` / `--repair` open
straight into a search / the health check — use these (plus PrintWindow screenshots) for UI
checks; never send synthetic keystrokes to the desktop.

## Mandatory process rules (from the project brief)

- **Bump `version` in Cargo.toml after every change** and add a CHANGELOG.md entry (version,
  date, one line, minor/medium/major). Minor → increment the current pre-release counter;
  medium → beta (or next beta); major → next version. Version is read via
  `env!("CARGO_PKG_VERSION")` and embedded by `build.rs` (winresource) — never hardcode it.
- Commit per milestone with the version in the message (`v0.1.0-beta.3: …`). No CI/workflows.
  Never commit binaries (`.gitignore`: target/, *.exe).
- Priority on conflicts: correctness/safety of registry changes > speed > size > polish.

## Architecture (the parts that span files)

- **unsafe boundary**: `main.rs` has `#![deny(unsafe_code)]`; only `registry::win`, `shell` and
  `window` allow it, as thin wrappers. New OS calls go there.
- **Registry writes**: everything goes through the `Registry` trait (`registry.rs`).
  `check_write` is an allowlist (HKCU `Classes\.<ext>`, `Classes\Applications\<exe>`,
  `Explorer\FileExts\…`, `UrlAssociations\…`; HKLM only `SOFTWARE\Classes\.<ext>` itself; HKCR
  never). `WinRegistry` logs every mutation. Logic modules are tested with `MemRegistry`
  (`#[cfg(test)]`, case-insensitive, key-level HKCR merge, fake clock, `deny` list for
  ACCESS_DENIED).
- **Setting a default** (`defaults::apply`): preflight (SYSTEM_TYPES refused, UserChoiceLatest →
  Protected, `userchoice::algorithm_status` Mismatch → Protected) → `.reg` backup
  (`backup::save`) → `Applications\<exe>` registration → `userchoice::write` → Shell verify
  (`AssocQueryString`, injected as a closure) → per-target rollback from the snapshot. Success
  is never inferred from a registry readback. `Status::Protected` drives the Settings fallback.
- **UserChoice hash** is isolated in `userchoice.rs` (pure Rust MD5; PS-SFTA test vectors — never
  edit vectors to make a refactor pass). Restoring a UserChoice key always re-hashes
  (`backup::restore`), never replays the old Hash.
- **Repair** (`repair.rs`): pure scan over the trait with injected `exe_exists`/`expand`;
  `fix_user` does the HKCU steps and returns `KnownGood`s needing HKLM, which
  `elevated::fix_machine` handles via a `runas` relaunch (`--elevated-fix` / `--elevated-set`,
  args validated against the known-good table).
- **Threads**: UI thread owns thread-local `State` (`ui.rs`) and never does registry/disk I/O.
  `scan` thread builds the `Index` (two passes, `on_index` twice; trims working set when done),
  `icons` thread loads icons requested lazily by `IconModel::row_data` (only visible rows),
  `worker` thread runs Shell queries and *all* writes (`Job`/`Reply`, `actions::run`). Results
  return via `slint::invoke_from_event_loop`. Don't call `ui::toast`/`main_app` inside
  `with_state` (RefCell re-borrow panic).
- **Window chrome** (`window.rs` ↔ `ui/app.slint`): CBT hook subclasses the winit window at
  creation; `WM_NCCALCSIZE` strips only the caption; `WM_NCHITTEST` geometry constants
  (CAPTION_H 36, BUTTON_W 46, ICON_W 44) must match the Slint top bar. Maximize is
  `HTMAXBUTTON` (Snap Layouts), so its hover is mirrored from `WM_NCMOUSEMOVE` into `max-hover`.
  Chrome events are deferred into the event loop.
- **Slint gotchas**: `row`, `maximized`, `close` are reserved/builtin names; no `x`/`y` on items
  inside layouts; elements inside `if` can't be referenced by id from outside (use a property +
  `changed` handler, see `detail-focus`).
