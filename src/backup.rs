//! Backups in regedit's own `.reg` format (UTF-16LE, "Windows Registry Editor Version 5.00"),
//! written to %LOCALAPPDATA%\DefaultFast\backups before any registry write.
//!
//! A backup is a list of `Op`s that recreate the earlier state exactly:
//! - `DeleteTree` + `Key`s: a whole subtree (`[-KEY]` then `[KEY]` blocks). A key that didn't
//!   exist is just the `[-KEY]` line, so restoring removes what we created.
//! - `SetValue`: a single value (used for HKLM, where we only ever touch one value), marked
//!   with a `; value-only` comment so restore doesn't wipe the rest of that key.
//!
//! Restoring a UserChoice key never replays the old Hash (it depends on the key's write time):
//! the ProgId is rewritten through `userchoice::write`, which computes a fresh hash.
use crate::{
    error::{Error, Result},
    registry::{Hive, Registry, Value, REG_BINARY, REG_DWORD, REG_EXPAND_SZ, REG_SZ},
    userchoice,
};
use std::path::{Path, PathBuf};

pub const KEEP: usize = 50;
const HEADER: &str = "Windows Registry Editor Version 5.00";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Op {
    DeleteTree(Hive, String),
    Key(Hive, String, Vec<(String, Value)>),
    SetValue(Hive, String, String, Option<Value>),
}

impl Op {
    fn hive(&self) -> Hive {
        match self {
            Op::DeleteTree(h, _) | Op::Key(h, _, _) | Op::SetValue(h, _, _, _) => *h,
        }
    }
}

/// Snapshot of a whole subtree (or its absence).
pub fn snapshot_tree(reg: &dyn Registry, hive: Hive, path: &str) -> Result<Vec<Op>> {
    let mut ops = vec![Op::DeleteTree(hive, path.to_owned())];
    fn walk(reg: &dyn Registry, hive: Hive, path: &str, ops: &mut Vec<Op>, depth: u32) -> Result<()> {
        if depth > 128 { return Err(Error::Invalid(format!("Registry subtree too deep to back up: {path}"))); }
        let Some(key) = reg.snapshot_key(hive, path)? else {
            if depth != 0 { return Err(Error::Invalid("Registry changed while backing up".into())); }
            return Ok(());
        };
        ops.push(Op::Key(hive, path.to_owned(), key.values));
        for child in key.children {
            walk(reg, hive, &format!("{path}\\{child}"), ops, depth + 1)?;
        }
        Ok(())
    }
    walk(reg, hive, path, &mut ops, 0)?;
    Ok(ops)
}

pub fn snapshot_value(reg: &dyn Registry, hive: Hive, path: &str, name: &str) -> Result<Op> {
    Ok(Op::SetValue(hive, path.to_owned(), name.to_owned(), reg.snapshot_value(hive, path, name)?))
}

fn esc(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(",")
}

fn value_line(name: &str, v: Option<&Value>) -> String {
    let n = if name.is_empty() { "@".to_owned() } else { format!("\"{}\"", esc(name)) };
    match v {
        None => format!("{n}=-"),
        Some(Value::Sz(s)) => format!("{n}=\"{}\"", esc(s)),
        Some(Value::Dword(d)) => format!("{n}=dword:{d:08x}"),
        Some(Value::Raw(REG_BINARY, b)) => format!("{n}=hex:{}", hex(b)),
        Some(v) => format!("{n}=hex({:x}):{}", v.kind(), hex(&v.to_bytes())),
    }
}

pub fn to_reg(summary: &str, created: &str, ops: &[Op]) -> String {
    let mut out = format!("{HEADER}\r\n\r\n; DefaultFast backup\r\n; Summary: {summary}\r\n; Created: {created}\r\n");
    for op in ops {
        out.push_str("\r\n");
        match op {
            Op::DeleteTree(h, p) => out.push_str(&format!("[-{}\\{p}]\r\n", h.reg_name())),
            Op::Key(h, p, values) => {
                out.push_str(&format!("[{}\\{p}]\r\n", h.reg_name()));
                for (n, v) in values {
                    out.push_str(&value_line(n, Some(v)));
                    out.push_str("\r\n");
                }
            }
            Op::SetValue(h, p, n, v) => {
                out.push_str(&format!("; value-only\r\n[{}\\{p}]\r\n", h.reg_name()));
                out.push_str(&value_line(n, v.as_ref()));
                out.push_str("\r\n");
            }
        }
    }
    out
}

fn unesc(s: &str) -> String {
    let mut out = String::new();
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c == '\\' {
            if let Some(n) = it.next() {
                out.push(n);
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Splits `"name"=data` respecting escaped quotes; `@=data` is the default value.
fn split_value(line: &str) -> Option<(String, &str)> {
    if let Some(rest) = line.strip_prefix("@=") {
        return Some((String::new(), rest));
    }
    let body = line.strip_prefix('"')?;
    let mut escaped = false;
    for (i, c) in body.char_indices() {
        match c {
            '\\' if !escaped => escaped = true,
            '"' if !escaped => {
                let rest = body[i + 1..].strip_prefix('=')?;
                return Some((unesc(&body[..i]), rest));
            }
            _ => escaped = false,
        }
    }
    None
}

fn parse_data(data: &str) -> Option<Option<Value>> {
    if data == "-" {
        return Some(None);
    }
    if let Some(s) = data.strip_prefix('"').and_then(|d| d.strip_suffix('"')) {
        return Some(Some(Value::Sz(unesc(s))));
    }
    if let Some(d) = data.strip_prefix("dword:") {
        return Some(Some(Value::Dword(u32::from_str_radix(d, 16).ok()?)));
    }
    let (kind, hexdata) = if let Some(h) = data.strip_prefix("hex:") {
        (REG_BINARY, h)
    } else {
        let rest = data.strip_prefix("hex(")?;
        let (k, h) = rest.split_once("):")?;
        (u32::from_str_radix(k, 16).ok()?, h)
    };
    let bytes: Vec<u8> = hexdata.split(',').map(str::trim).filter(|s| !s.is_empty()).map(|b| u8::from_str_radix(b, 16)).collect::<std::result::Result<_, _>>().ok()?;
    Some(Some(match kind {
        REG_SZ | REG_EXPAND_SZ | REG_DWORD => {
            let value = Value::from_bytes(kind, &bytes);
            if value.to_bytes() == bytes { value } else { Value::Raw(kind, bytes) }
        },
        k => Value::Raw(k, bytes),
    }))
}

fn parse_key(s: &str) -> Option<(Hive, String)> {
    let (h, p) = s.split_once('\\')?;
    Some((Hive::parse(h)?, p.to_owned()))
}

/// Parses a backup written by `to_reg` (also tolerates regedit's `\` line continuations).
pub fn parse_reg(text: &str) -> Result<(String, String, Vec<Op>)> {
    let bad = |what: &str| Error::Invalid(format!("Backup file is damaged: {what}"));
    // Join continuation lines first.
    let mut lines: Vec<String> = Vec::new();
    let mut pending = String::new();
    for raw in text.trim_start_matches('\u{feff}').lines() {
        let l = raw.trim_end_matches('\r');
        let can_continue = if pending.is_empty() {
            !l.trim_start().starts_with(['[', ';']) && l.contains('=')
        } else {
            !l.trim_start().starts_with(';')
        };
        if let Some(cont) = l.strip_suffix('\\').filter(|_| can_continue) {
            if pending.is_empty() {
                pending.push_str(cont);
            } else {
                pending.push_str(cont.trim_start());
            }
            continue;
        }
        if !pending.is_empty() {
            pending.push_str(l.trim_start());
            lines.push(std::mem::take(&mut pending));
        } else {
            lines.push(l.to_owned());
        }
    }
    if !pending.is_empty() {
        lines.push(pending);
    }
    if lines.first().map(|l| l.trim()) != Some(HEADER) {
        return Err(bad("missing header"));
    }
    let (mut summary, mut created) = (String::new(), String::new());
    let mut ops = Vec::new();
    let mut value_only = false;
    for l in lines.iter().skip(1) {
        let l = l.trim();
        if let Some(c) = l.strip_prefix(';') {
            let c = c.trim();
            if let Some(s) = c.strip_prefix("Summary:") {
                summary = s.trim().to_owned();
            } else if let Some(s) = c.strip_prefix("Created:") {
                created = s.trim().to_owned();
            } else if c == "value-only" {
                value_only = true;
            }
            continue;
        }
        if l.is_empty() {
            continue;
        }
        if let Some(k) = l.strip_prefix("[-").and_then(|k| k.strip_suffix(']')) {
            let (h, p) = parse_key(k).ok_or_else(|| bad(l))?;
            ops.push(Op::DeleteTree(h, p));
        } else if let Some(k) = l.strip_prefix('[').and_then(|k| k.strip_suffix(']')) {
            let (h, p) = parse_key(k).ok_or_else(|| bad(l))?;
            ops.push(if value_only { Op::SetValue(h, p, String::new(), None) } else { Op::Key(h, p, Vec::new()) });
            value_only = false;
        } else {
            let (name, data) = split_value(l).ok_or_else(|| bad(l))?;
            let v = parse_data(data).ok_or_else(|| bad(l))?;
            match ops.last_mut() {
                Some(Op::Key(_, _, values)) => values.push((name, v.ok_or_else(|| bad(l))?)),
                Some(Op::SetValue(_, _, n, val)) => {
                    *n = name;
                    *val = v;
                }
                _ => return Err(bad(l)),
            }
        }
    }
    Ok((summary, created, ops))
}

pub fn dir() -> PathBuf {
    let d = crate::logfile::data_dir().join("backups");
    let _ = std::fs::create_dir_all(&d);
    d
}

/// Writes the backup (UTF-16LE with BOM, like regedit) and prunes to the newest `KEEP`.
pub fn save(dir: &Path, stamp: &str, created: &str, summary: &str, ops: &[Op]) -> Result<PathBuf> {
    let path = dir.join(format!("{stamp}.reg"));
    let text = to_reg(summary, created, ops);
    let bytes: Vec<u8> = [0xFF, 0xFE].into_iter().chain(text.encode_utf16().flat_map(u16::to_le_bytes)).collect();
    // Write-then-rename: a crash never leaves a half-written backup behind.
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, &path)?;
    prune(dir, KEEP);
    Ok(path)
}

fn read_reg_text(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path)?;
    Ok(if bytes.starts_with(&[0xFF, 0xFE]) {
        let units: Vec<u16> = bytes[2..].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        String::from_utf16_lossy(&units)
    } else {
        String::from_utf8_lossy(&bytes).into_owned()
    })
}

pub fn load(path: &Path) -> Result<(String, String, Vec<Op>)> {
    parse_reg(&read_reg_text(path)?)
}

/// Fast header-only scan for History: avoids parsing every hex/value line of all 50 backups.
fn load_entry_meta(path: &Path) -> Option<(String, String, bool)> {
    let text = read_reg_text(path).ok()?;
    let mut lines = text.trim_start_matches('\u{feff}').lines().map(|l| l.trim());
    if lines.next() != Some(HEADER) {
        return None;
    }
    let (mut summary, mut created, mut machine) = (String::new(), String::new(), false);
    for l in lines {
        if let Some(c) = l.strip_prefix(';') {
            let c = c.trim();
            if let Some(s) = c.strip_prefix("Summary:") {
                summary = s.trim().to_owned();
            } else if let Some(s) = c.strip_prefix("Created:") {
                created = s.trim().to_owned();
            }
        } else if let Some(k) = l.strip_prefix("[-").or_else(|| l.strip_prefix('[')).and_then(|k| k.strip_suffix(']')) {
            if parse_key(k).is_some_and(|(h, _)| h == Hive::LocalMachine) {
                machine = true;
            }
        }
    }
    Some((summary, created, machine))
}

#[derive(Clone, Debug)]
pub struct Entry {
    pub path: PathBuf,
    pub summary: String,
    pub created: String,
    pub undone: bool,
    /// Touches HKLM, so restoring it needs the elevated helper (UI warns first).
    pub machine: bool,
}

/// Newest first. Undone backups are renamed `*.undone.reg`.
pub fn list(dir: &Path) -> Vec<Entry> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("reg")))
        .collect();
    files.sort();
    files.reverse();
    files
        .into_iter()
        .filter_map(|path| {
            let (summary, created, machine) = load_entry_meta(&path)?;
            let undone = path.to_string_lossy().ends_with(".undone.reg");
            Some(Entry { path, summary, created, undone, machine })
        })
        .collect()
}

pub fn prune(dir: &Path, keep: usize) {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("reg")))
        .collect();
    files.sort(); // names start with a sortable timestamp
    let excess = files.len().saturating_sub(keep);
    for f in files.into_iter().take(excess) {
        let _ = std::fs::remove_file(f);
    }
}

pub fn mark_undone(path: &Path) -> PathBuf {
    if path.to_string_lossy().ends_with(".undone.reg") {
        return path.to_owned();
    }
    let new = path.with_extension("undone.reg");
    if std::fs::rename(path, &new).is_ok() {
        new
    } else {
        path.to_owned()
    }
}

/// The subtree roots / values a backup covers, used to snapshot the *current* state before
/// restoring (so a restore is itself undoable). Deduplicates subtrees so child `Key` ops inside
/// a `DeleteTree` root are not snapshotted a second time.
pub fn coverage(reg: &dyn Registry, ops: &[Op]) -> Result<Vec<Op>> {
    let mut roots: Vec<(Hive, String, String)> = Vec::new();
    for op in ops {
        if let Op::DeleteTree(h, p) | Op::Key(h, p, _) = op {
            let low = p.to_lowercase();
            let covered = roots.iter().any(|(rh, _, rl)| *rh == *h && (low == *rl || low.strip_prefix(rl.as_str()).is_some_and(|r| r.starts_with('\\'))));
            if !covered {
                roots.retain(|(rh, _, rl)| !(*rh == *h && rl.strip_prefix(low.as_str()).is_some_and(|r| r.starts_with('\\'))));
                roots.push((*h, p.clone(), low));
            }
        }
    }
    let mut out = Vec::new();
    for (h, p, _) in &roots {
        out.extend(snapshot_tree(reg, *h, p)?);
    }
    let mut seen_vals: Vec<(Hive, String, String)> = Vec::new();
    for op in ops {
        if let Op::SetValue(h, p, n, _) = op {
            let low_p = p.to_lowercase();
            let low_n = n.to_lowercase();
            let in_root = roots.iter().any(|(rh, _, rl)| *rh == *h && (low_p == *rl || low_p.strip_prefix(rl.as_str()).is_some_and(|r| r.starts_with('\\'))));
            if !in_root && !seen_vals.iter().any(|(sh, sp, sn)| *sh == *h && *sp == low_p && *sn == low_n) {
                seen_vals.push((*h, low_p, low_n));
                out.push(snapshot_value(reg, *h, p, n)?);
            }
        }
    }
    Ok(out)
}

/// Applies the HKCU part of a backup. HKLM `SetValue`s are returned for the elevated helper.
pub fn restore(reg: &dyn Registry, ops: &[Op], sid: &str) -> Result<Vec<Op>> {
    let mut machine = Vec::new();
    for op in ops {
        if op.hive() == Hive::LocalMachine {
            machine.push(op.clone());
            continue;
        }
        match op {
            Op::DeleteTree(h, p) => reg.delete_tree(*h, p)?,
            Op::Key(h, p, values) => {
                let lower = p.to_ascii_lowercase();
                if lower.ends_with("\\userchoicelatest") {
                    // Unpublished hash: can't be recreated validly; Windows rebuilds it.
                    crate::logfile::write(&format!("SKIP restore of {h}\\{p} (UserChoiceLatest)"));
                    continue;
                }
                if lower.ends_with("\\userchoice") {
                    let target = p.rsplit('\\').nth(1).unwrap_or_default().to_lowercase();
                    if let Some(prog) = values.iter().find(|(n, _)| n.eq_ignore_ascii_case("ProgId")).and_then(|(_, v)| v.as_str()) {
                        userchoice::write(reg, &target, prog, sid)?;
                    }
                    continue;
                }
                reg.create_key(*h, p)?;
                for (n, v) in values {
                    reg.set(*h, p, n, v)?;
                }
            }
            Op::SetValue(h, p, n, v) => match v {
                Some(v) => reg.set(*h, p, n, v)?,
                None => reg.delete_value(*h, p, n)?,
            },
        }
    }
    Ok(machine)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{MemRegistry, REG_NONE};

    const SID: &str = "S-1-5-21-7";
    fn fe(ext: &str) -> String {
        format!("Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\FileExts\\{ext}")
    }

    fn seeded() -> MemRegistry {
        let r = MemRegistry::new();
        let k = format!("{}\\OpenWithList", fe(".mp4"));
        r.seed(Hive::CurrentUser, &k, "a", Value::Sz("vlc.exe".into()));
        r.seed(Hive::CurrentUser, &k, "MRUList", Value::Sz("a".into()));
        r.seed(Hive::CurrentUser, &format!("{}\\OpenWithProgids", fe(".mp4")), "VLC.mp4", Value::Raw(REG_NONE, vec![]));
        r.seed(Hive::CurrentUser, &fe(".mp4"), "Weird \"quoted\" \\ name", Value::ExpandSz("%SystemRoot%\\x.exe".into()));
        r.seed(Hive::CurrentUser, &fe(".mp4"), "", Value::Dword(42));
        userchoice::write(&r, ".mp4", "WMP11.AssocFile.MP4", SID).unwrap();
        r
    }

    #[test]
    fn snapshots_fail_on_unreadable_keys_and_values() {
        let r = seeded();
        let root = fe(".mp4");
        r.read_deny.lock().unwrap().push(format!("{}\\openwithlist", root.to_lowercase()));
        assert!(snapshot_tree(&r, Hive::CurrentUser, &root).is_err());
        r.read_deny.lock().unwrap().push("software\\classes\\.bat".into());
        assert!(snapshot_value(&r, Hive::LocalMachine, "SOFTWARE\\Classes\\.bat", "").is_err());
        assert!(coverage(&r, &[Op::DeleteTree(Hive::CurrentUser, root)]).is_err());
    }

    #[test]
    fn snapshots_include_deep_children_or_fail_instead_of_truncating() {
        let r = MemRegistry::new();
        let root = "Software\\Classes\\Applications\\deep.exe";
        let deep = format!("{root}{}", "\\child".repeat(20));
        r.seed(Hive::CurrentUser, &deep, "data", Value::Dword(7));
        let ops = snapshot_tree(&r, Hive::CurrentUser, root).unwrap();
        r.delete_tree(Hive::CurrentUser, root).unwrap();
        restore(&r, &ops, SID).unwrap();
        assert_eq!(r.get(Hive::CurrentUser, &deep, "data"), Some(Value::Dword(7)));
        r.seed_key(Hive::CurrentUser, &format!("{root}{}", "\\child".repeat(130)));
        assert!(snapshot_tree(&r, Hive::CurrentUser, root).is_err());
    }

    #[test]
    fn unusual_string_payloads_survive_backup_round_trip() {
        let ops = vec![Op::SetValue(Hive::CurrentUser, "Software\\Classes\\.x".into(), String::new(), Some(Value::Raw(REG_SZ, vec![1, 2, 3])))];
        assert_eq!(parse_reg(&to_reg("x", "now", &ops)).unwrap().2, ops);
    }

    #[test]
    fn reg_text_round_trip() {
        let r = seeded();
        let mut ops = snapshot_tree(&r, Hive::CurrentUser, &fe(".mp4")).unwrap();
        ops.push(snapshot_value(&r, Hive::LocalMachine, "SOFTWARE\\Classes\\.bat", "").unwrap());
        ops.push(Op::SetValue(Hive::LocalMachine, "SOFTWARE\\Classes\\.cmd".into(), String::new(), Some(Value::Sz("cmdfile".into()))));
        let text = to_reg("Set VLC as default for .mp4", "2026-10-30 20:00:00", &ops);
        assert!(text.starts_with(HEADER));
        let (summary, created, parsed) = parse_reg(&text).unwrap();
        assert_eq!(summary, "Set VLC as default for .mp4");
        assert_eq!(created, "2026-10-30 20:00:00");
        assert_eq!(parsed, ops);
    }

    #[test]
    fn restore_round_trip_regenerates_userchoice_hash() {
        let r = seeded();
        let before = snapshot_tree(&r, Hive::CurrentUser, &fe(".mp4")).unwrap();
        let (_, _, ops) = parse_reg(&to_reg("x", "y", &before)).unwrap();

        // Change everything: new choice, extra key, removed list.
        r.advance(10 * 600_000_000);
        userchoice::write(&r, ".mp4", "VLC.mp4", SID).unwrap();
        r.delete_tree(Hive::CurrentUser, &format!("{}\\OpenWithList", fe(".mp4"))).unwrap();
        r.set(Hive::CurrentUser, &format!("{}\\Junk", fe(".mp4")), "x", &Value::Sz("y".into())).unwrap();

        r.advance(10 * 600_000_000);
        let machine = restore(&r, &ops, SID).unwrap();
        assert!(machine.is_empty());
        assert_eq!(userchoice::read(&r, ".mp4").unwrap().prog_id, "WMP11.AssocFile.MP4");
        // The restored choice validates against its *new* write time.
        assert_eq!(userchoice::validate(&r, ".mp4", SID), Some(true));
        assert!(!r.exists(Hive::CurrentUser, &format!("{}\\Junk", fe(".mp4"))));
        assert_eq!(r.string(Hive::CurrentUser, &format!("{}\\OpenWithList", fe(".mp4")), "a").as_deref(), Some("vlc.exe"));
        assert_eq!(r.get(Hive::CurrentUser, &fe(".mp4"), ""), Some(Value::Dword(42)));
    }

    #[test]
    fn restoring_a_missing_key_removes_it() {
        let r = MemRegistry::new();
        let ops = snapshot_tree(&r, Hive::CurrentUser, "Software\\Classes\\Applications\\new.exe").unwrap();
        assert_eq!(ops.len(), 1);
        r.set(Hive::CurrentUser, "Software\\Classes\\Applications\\new.exe\\shell\\open\\command", "", &Value::Sz("x".into())).unwrap();
        restore(&r, &ops, SID).unwrap();
        assert!(!r.exists(Hive::CurrentUser, "Software\\Classes\\Applications\\new.exe"));
    }

    #[test]
    fn machine_ops_are_returned_not_applied() {
        let r = MemRegistry::new();
        let ops = vec![Op::SetValue(Hive::LocalMachine, "SOFTWARE\\Classes\\.bat".into(), String::new(), Some(Value::Sz("batfile".into())))];
        assert_eq!(restore(&r, &ops, SID).unwrap(), ops);
    }

    #[test]
    fn files_save_list_prune() {
        let d = std::env::temp_dir().join(format!("defaultfast-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        for i in 0..(KEEP + 3) {
            save(&d, &format!("20261030-0000{i:02}-000"), "now", &format!("change {i}"), &[Op::DeleteTree(Hive::CurrentUser, "Software\\Classes\\.x".into())]).unwrap();
        }
        let entries = list(&d);
        assert_eq!(entries.len(), KEEP);
        assert_eq!(entries[0].summary, format!("change {}", KEEP + 2), "newest first");
        let undone = mark_undone(&entries[0].path);
        assert!(list(&d)[0].undone);
        assert_eq!(load(&undone).unwrap().2.len(), 1);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn multiline_continuation_values_parse() {
        let text = concat!(
            "Windows Registry Editor Version 5.00\r\n\r\n",
            "; Summary: Multiline\r\n; Created: now\r\n\r\n",
            "[HKEY_CURRENT_USER\\Software\\Classes\\.x]\r\n",
            "\"Bin\"=hex:01,02,\\\r\n  03,04,\\\r\n  05,06\r\n",
        );
        let (_, _, ops) = parse_reg(text).unwrap();
        assert_eq!(ops, vec![Op::Key(Hive::CurrentUser, "Software\\Classes\\.x".into(), vec![("Bin".into(), Value::Raw(REG_BINARY, vec![1, 2, 3, 4, 5, 6]))])]);
    }

    #[test]
    fn coverage_does_not_duplicate_subtrees() {
        let r = seeded();
        let snap = snapshot_tree(&r, Hive::CurrentUser, &fe(".mp4")).unwrap();
        let cov = coverage(&r, &snap).unwrap();
        assert_eq!(cov, snap);
    }
}
