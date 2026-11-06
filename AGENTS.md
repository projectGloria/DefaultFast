# Repository Guidelines

## Project Structure & Module Organization
DefaultFast is a portable Windows 10 1809+/11 utility written in Rust 2021 and Slint. `src/main.rs` is the entry point; `defaults.rs`, `repair.rs`, `backup.rs`, and `userchoice.rs` implement association logic. `registry.rs` provides Windows access and an in-memory test implementation. UI state and background work live in `ui.rs`, `actions.rs`, and `worker.rs`; layouts live in `ui/app.slint`. `build.rs` compiles Slint and generates the icon and Windows resources; no binary assets are tracked. Unit tests sit beside implementation code. See `docs/TESTPLAN.md` for manual checks and `scripts/measure.ps1` for performance measurements.

## Build, Test, and Development Commands
Use Windows, Rust 1.88+, and Visual Studio Build Tools with Desktop development with C++ and the Windows SDK. Target `x86_64-pc-windows-msvc`.

- `cargo run` — launch the development UI.
- `cargo build --release` — build `target/release/DefaultFast.exe`; keep builds warning-free.
- `cargo test` — run unit tests.
- `cargo test userchoice::` — run focused hash tests.
- `powershell -File scripts/measure.ps1 -Runs 5` — measure startup and memory budgets.
- `./target/release/DefaultFast.exe --diagnose` — run read-only diagnostics.
- `./target/release/DefaultFast.exe --self-test` — exercise real registry writes on an invented extension, then clean up.

## Coding Style & Naming Conventions
Use four-space indentation, `snake_case` Rust modules/functions, `PascalCase` types, and `SCREAMING_SNAKE_CASE` constants. Follow surrounding formatting; no custom formatter or lint configuration is present. Slint properties use hyphenated names. Keep title-bar geometry synchronized between `ui/app.slint` and `src/window.rs`.

## Testing Guidelines
Use Rust's built-in `#[test]` framework in `#[cfg(test)]` modules, with descriptive snake_case names. Test registry logic through `MemRegistry`, including denied writes and rollback. Preserve existing hash test vectors. No numeric coverage threshold is configured. Run relevant manual scenarios from `docs/TESTPLAN.md`; perform intentionally destructive setups only in a test VM.

## Commit & Pull Request Guidelines
Follow existing milestone subjects: `v0.1.0-beta.5: describe the change`. Per `CLAUDE.md`, bump `Cargo.toml` for every change, synchronize `Cargo.lock`, and add a dated, severity-labelled `CHANGELOG.md` entry. PRs should describe behavior, reference relevant issues, report validation, and include screenshots for UI changes. No CI is configured; validate locally. Never commit build outputs or executables.

## Architecture & Safety
Route registry mutations through `Registry` and its allowlist; preserve backup, Shell verification, and rollback. Confine unsafe code to existing OS wrappers. Keep registry/disk I/O off the UI thread and return results through Slint's event loop.
