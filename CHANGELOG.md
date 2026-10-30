# Changelog

Versions follow the rule in the project brief: minor change -> bump the pre-release counter,
medium change -> beta, major change -> next version number. The version lives only in
`Cargo.toml`.

## 0.1.0-beta.3 — 2026-10-30 — medium
Repair view (known-good table, no-handler / wrong-class / dangling / missing-exe / bad-hash
checks, startup scan), on-demand elevated helper (--elevated-fix / --elevated-set) with UAC
explanation, History view with Undo last change and Restore from backup, --repair launch option.

## 0.1.0-beta.2 — 2026-10-30 — medium
UserChoice writer (legacy hash, pure-Rust MD5, PS-SFTA vectors, live check against this
machine's own keys), .reg backups before every write, Shell verification with per-target
rollback, Settings fallback for UCPD / UserChoiceLatest, Undo toast, Browse, --self-test.

## 0.1.0-beta.1 — 2026-10-30 — medium
Slint UI with custom Windows 11 top bar on a real native frame (Snap Layouts, resize borders,
shadow, rounded corners), grouped fuzzy search with lazy icons, program and file-type/protocol
detail panes, live theme/accent, remembered window placement, startup probe.

## 0.1.0-alpha.1 — 2026-10-30 — major
Start of DefaultFast: registry layer (trait + Win32 + in-memory mock, write allowlist, write
logging), Shell wrappers, two-pass program/file-type/protocol discovery and fuzzy search.
