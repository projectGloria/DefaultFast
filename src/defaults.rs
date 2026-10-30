//! "Make X the default for these targets": the whole transaction around UserChoice.
//!
//! preflight -> backup (.reg) -> register the app if needed -> write UserChoice per target ->
//! verify through Shell (AssocQueryString) -> roll back any target Windows didn't honor.
//! A registry write alone never counts as success.
use crate::{
    backup::{self, Op},
    error::Error,
    registry::{Hive, Registry, Value, REG_NONE},
    userchoice::{self, Algorithm},
};

const HKCU: Hive = Hive::CurrentUser;

/// Types Windows itself depends on. Re-pointing them breaks launching programs/shortcuts;
/// they are only ever restored to the Windows default by the Repair view.
pub const SYSTEM_TYPES: &[&str] = &[".exe", ".com", ".scr", ".pif", ".lnk", ".cpl", ".msc", ".dll", ".sys"];
/// Script types: allowed, but the UI confirms because double-click stops *running* them.
pub const SCRIPT_TYPES: &[&str] = &[".bat", ".cmd", ".ps1", ".vbs", ".vbe", ".js", ".jse", ".wsf", ".reg", ".msi", ".hta"];

#[derive(Clone, Debug)]
pub struct Request {
    pub app_name: String,
    /// Executable for `Applications\<exe>` registration; empty for packaged apps.
    pub exe: String,
    /// (target, ProgID) pairs.
    pub changes: Vec<(String, String)>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    Done,
    /// Windows protects it (UCPD, UserChoiceLatest, unknown hash): use Settings.
    Protected(String),
    Failed(String),
}

#[derive(Clone, Debug)]
pub struct Report {
    pub results: Vec<(String, Status)>,
    pub backup: Option<std::path::PathBuf>,
}

#[cfg(test)]
impl Report {
    pub fn done(&self) -> usize {
        self.results.iter().filter(|r| r.1 == Status::Done).count()
    }
    pub fn protected(&self) -> usize {
        self.results.iter().filter(|r| matches!(r.1, Status::Protected(_))).count()
    }
}

/// Verifies with Shell that `target` now resolves to `prog_id` (or `exe`).
pub type Verify<'a> = &'a dyn Fn(&str, &str, &str) -> bool;

fn exe_name(exe: &str) -> &str {
    exe.rsplit(['\\', '/']).next().unwrap_or(exe)
}

/// Registry ops that make `Applications\<exe>` a valid, persistent choice for `target`.
fn registration(reg: &dyn Registry, exe: &str, target: &str, prog_id: &str) -> Vec<(Hive, String, String, Value)> {
    let mut ops = Vec::new();
    if !prog_id.starts_with("Applications\\") || exe.is_empty() {
        return ops;
    }
    let app_key = format!("Software\\Classes\\{prog_id}");
    let cmd_key = format!("{prog_id}\\shell\\open\\command");
    let wanted = format!("\"{exe}\" \"%1\"");
    let existing = reg.string(Hive::ClassesRoot, &cmd_key, "");
    // Keep an existing command that already launches this exe (it may carry better args).
    let points_here = existing.as_deref().and_then(crate::discovery::parse_command_exe).is_some_and(|e| e.eq_ignore_ascii_case(exe));
    if !points_here {
        ops.push((HKCU, format!("{app_key}\\shell\\open\\command"), String::new(), Value::Sz(wanted)));
    }
    if target.starts_with('.') {
        ops.push((HKCU, format!("{app_key}\\SupportedTypes"), target.to_owned(), Value::Sz(String::new())));
        ops.push((HKCU, format!("Software\\Classes\\{target}\\OpenWithProgids"), prog_id.to_owned(), Value::Raw(REG_NONE, Vec::new())));
    }
    ops
}

pub fn apply(reg: &dyn Registry, verify: Verify, sid: &str, req: &Request, stamp: &str, created: &str, dir: &std::path::Path) -> Report {
    let mut results: Vec<(String, Status)> = Vec::new();
    let (algo, _, _) = userchoice::algorithm_status(reg, sid);
    let mut todo = Vec::new();
    for (target, prog_id) in &req.changes {
        let status = if SYSTEM_TYPES.contains(&target.as_str()) {
            Some(Status::Failed(format!("{target} must stay with Windows. Use Health check to repair it.")))
        } else if algo == Algorithm::Mismatch {
            Some(Status::Protected("This Windows version uses a default-app format DefaultFast can't verify.".into()))
        } else if userchoice::uses_latest(reg, target) {
            Some(Status::Protected(format!("Windows protects {target} with a newer format (UserChoiceLatest).")))
        } else if prog_id.is_empty() {
            Some(Status::Failed("No handler registration found for this app.".into()))
        } else {
            None
        };
        match status {
            Some(s) => results.push((target.clone(), s)),
            None => todo.push((target.clone(), prog_id.clone())),
        }
    }
    if todo.is_empty() {
        return Report { results, backup: None };
    }

    // Backup everything we may touch, before the first write.
    let reg_ops: Vec<_> = todo.iter().flat_map(|(t, p)| registration(reg, &req.exe, t, p)).collect();
    let mut snap: Vec<Op> = Vec::new();
    for (t, _) in &todo {
        snap.extend(backup::snapshot_tree(reg, HKCU, &userchoice::choice_key(t)));
    }
    let mut roots: Vec<String> = reg_ops.iter().map(|(_, k, _, _)| registration_root(k)).collect();
    roots.sort();
    roots.dedup();
    for r in &roots {
        snap.extend(backup::snapshot_tree(reg, HKCU, r));
    }
    let targets: Vec<&str> = todo.iter().map(|t| t.0.as_str()).collect();
    let summary = format!("Set {} as default for {}", req.app_name, targets.join(", "));
    let backup_path = match backup::save(dir, stamp, created, &summary, &snap) {
        Ok(p) => p,
        Err(e) => {
            for (t, _) in todo {
                results.push((t, Status::Failed(format!("Could not save a backup, so nothing was changed: {e}"))));
            }
            return Report { results, backup: None };
        }
    };

    for (hive, key, name, value) in &reg_ops {
        if let Err(e) = reg.set(*hive, key, name, value) {
            // Registration failed: undo it and stop; no UserChoice was written yet.
            let _ = backup::restore(reg, &snap, sid);
            let _ = std::fs::remove_file(&backup_path);
            for (t, _) in &todo {
                results.push((t.clone(), Status::Failed(format!("Could not register {}: {e}", req.app_name))));
            }
            return Report { results, backup: None };
        }
    }

    for (target, prog_id) in &todo {
        let r = userchoice::write(reg, target, prog_id, sid);
        crate::shell_notify();
        let status = match r {
            Ok(()) if verify(target, prog_id, &req.exe) => Status::Done,
            Ok(()) => Status::Protected(format!("Windows kept its previous choice for {target}.")),
            Err(e) if e.wants_settings() => Status::Protected(format!("Windows blocked the change for {target}.")),
            Err(e) => Status::Failed(e.to_string()),
        };
        if status != Status::Done {
            // Put this target's UserChoice back exactly as it was.
            let key = userchoice::choice_key(target);
            let mine: Vec<Op> = snap.iter().filter(|op| op_path(op).eq_ignore_ascii_case(&key)).cloned().collect();
            let _ = backup::restore(reg, &mine, sid);
        }
        results.push((target.clone(), status));
    }
    crate::shell_notify();

    let any_done = results.iter().any(|r| r.1 == Status::Done);
    if !any_done {
        // Nothing stuck: remove the registration too and drop the now-pointless backup.
        let _ = backup::restore(reg, &snap, sid);
        let _ = std::fs::remove_file(&backup_path);
        return Report { results, backup: None };
    }
    Report { results, backup: Some(backup_path) }
}

/// Backup root for a registration key: the whole Applications\<exe> or <ext>\OpenWithProgids.
fn registration_root(key: &str) -> String {
    let lower = key.to_ascii_lowercase();
    if lower.starts_with("software\\classes\\applications\\") {
        key.split('\\').take(4).collect::<Vec<_>>().join("\\")
    } else {
        key.to_owned()
    }
}

fn op_path(op: &Op) -> &str {
    match op {
        Op::DeleteTree(_, p) | Op::Key(_, p, _) | Op::SetValue(_, p, _, _) => p,
    }
}

/// Program-level request helper: the ProgID to use for `target` if the program has none.
pub fn applications_prog_id(exe: &str) -> String {
    format!("Applications\\{}", exe_name(exe))
}

impl From<Error> for Status {
    fn from(e: Error) -> Self {
        if e.wants_settings() {
            Status::Protected(e.to_string())
        } else {
            Status::Failed(e.to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::MemRegistry;

    const SID: &str = "S-1-5-21-5";

    fn tmp() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("defaultfast-defaults-{}-{:?}", std::process::id(), std::thread::current().id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Verifier backed by the mock: "effective" = UserChoice ProgId when its hash validates.
    fn verifier(r: &MemRegistry) -> impl Fn(&str, &str, &str) -> bool + '_ {
        move |t: &str, p: &str, _: &str| userchoice::validate(r, t, SID) == Some(true) && userchoice::read(r, t).is_some_and(|c| c.prog_id == p)
    }

    #[test]
    fn sets_registers_and_backs_up() {
        let r = MemRegistry::new();
        let d = tmp();
        let req = Request { app_name: "VLC".into(), exe: "C:\\VLC\\vlc.exe".into(), changes: vec![(".mp4".into(), "Applications\\vlc.exe".into()), (".mkv".into(), "VLC.mkv".into())] };
        let v = verifier(&r);
        let rep = apply(&r, &v, SID, &req, "s1", "now", &d);
        assert_eq!(rep.done(), 2, "{:?}", rep.results);
        assert_eq!(r.string(Hive::CurrentUser, "Software\\Classes\\Applications\\vlc.exe\\shell\\open\\command", "").as_deref(), Some("\"C:\\VLC\\vlc.exe\" \"%1\""));
        assert!(r.get(Hive::CurrentUser, "Software\\Classes\\.mp4\\OpenWithProgids", "Applications\\vlc.exe").is_some());
        // Undo = restore the backup: everything we created disappears again.
        let (_, _, ops) = backup::load(rep.backup.as_ref().unwrap()).unwrap();
        backup::restore(&r, &ops, SID).unwrap();
        assert!(userchoice::read(&r, ".mp4").is_none());
        assert!(!r.exists(Hive::CurrentUser, "Software\\Classes\\Applications\\vlc.exe"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn refuses_system_types_and_latest() {
        let r = MemRegistry::new();
        r.seed(Hive::CurrentUser, &userchoice::latest_key(".pdf"), "ProgId", Value::Sz("x".into()));
        let d = tmp();
        let req = Request { app_name: "X".into(), exe: String::new(), changes: vec![(".exe".into(), "x".into()), (".pdf".into(), "Acrobat.pdf".into())] };
        let v = verifier(&r);
        let rep = apply(&r, &v, SID, &req, "s2", "now", &d);
        assert!(matches!(rep.results[0].1, Status::Failed(_)));
        assert!(matches!(rep.results[1].1, Status::Protected(_)));
        assert!(rep.backup.is_none());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn unverified_target_is_rolled_back() {
        let r = MemRegistry::new();
        userchoice::write(&r, "https", "MSEdgeHTM", SID).unwrap();
        let d = tmp();
        let req = Request { app_name: "Chrome".into(), exe: String::new(), changes: vec![("https".into(), "ChromeHTML".into())] };
        let never = |_: &str, _: &str, _: &str| false; // Windows "kept its previous choice"
        let rep = apply(&r, &never, SID, &req, "s3", "now", &d);
        assert!(matches!(rep.results[0].1, Status::Protected(_)));
        assert_eq!(userchoice::read(&r, "https").unwrap().prog_id, "MSEdgeHTM");
        assert!(rep.backup.is_none());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn ucpd_denial_maps_to_settings_fallback() {
        let r = MemRegistry::new();
        r.deny.lock().unwrap().push(userchoice::choice_key("http").to_lowercase());
        let d = tmp();
        let req = Request { app_name: "Firefox".into(), exe: String::new(), changes: vec![("http".into(), "FirefoxURL".into())] };
        let v = verifier(&r);
        let rep = apply(&r, &v, SID, &req, "s4", "now", &d);
        assert_eq!(rep.protected(), 1);
        let _ = std::fs::remove_dir_all(&d);
    }
}
