# Changelog

Versions follow the rule in the project brief: minor change -> bump the pre-release counter,
medium change -> beta, major change -> next version number. The version lives only in
`Cargo.toml`.

## 0.1.0-beta.1 — 2026-10-30 — medium
Slint UI with custom Windows 11 top bar on a real native frame (Snap Layouts, resize borders,
shadow, rounded corners), grouped fuzzy search with lazy icons, program and file-type/protocol
detail panes, live theme/accent, remembered window placement, startup probe.

## 0.1.0-alpha.1 — 2026-10-30 — major
Start of DefaultFast: registry layer (trait + Win32 + in-memory mock, write allowlist, write
logging), Shell wrappers, two-pass program/file-type/protocol discovery and fuzzy search.
