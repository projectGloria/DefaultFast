//! Health check and repair of broken associations.
//!
//! The motivating bug: `HKCR\.bat` / `.cmd` with no default value while `HKCR\batfile` /
//! `cmdfile` still exist, so double-click shows "Select an app to open this file". Typically
//! an empty `HKCU\Software\Classes\.bat` override (which shadows HKLM in the merged HKCR view)
//! or a wiped HKLM default.
//!
//! Known-good fix (core Windows types):
//! 1. delete HKCU\…\Explorer\FileExts\<ext> (UserChoice, UserChoiceLatest, OpenWithList…)
//! 2. delete HKCU\Software\Classes\<ext>
//! 3. set HKLM\SOFTWARE\Classes\<ext> default = ProgID (elevated helper, only if needed)
//! 4. verify the ProgID's open command exists
//! 5. SHChangeNotify
use crate::{
    backup::{self, Op},
    discovery,
    error::Result,
    registry::{Hive, Registry},
    userchoice::{self, Algorithm},
};

const HKCR: Hive = Hive::ClassesRoot;
const HKCU: Hive = Hive::CurrentUser;
const HKLM: Hive = Hive::LocalMachine;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KnownGood {
    pub ext: &'static str,
    pub prog_id: &'static str,
    /// Anything other than the Windows class here is a breakage, not a preference.
    pub critical: bool,
}

pub const TABLE: &[KnownGood] = &[
    KnownGood { ext: ".bat", prog_id: "batfile", critical: true },
    KnownGood { ext: ".cmd", prog_id: "cmdfile", critical: true },
    KnownGood { ext: ".exe", prog_id: "exefile", critical: true },
    KnownGood { ext: ".lnk", prog_id: "lnkfile", critical: true },
    KnownGood { ext: ".txt", prog_id: "txtfile", critical: false },
    KnownGood { ext: ".ps1", prog_id: "Microsoft.PowerShellScript.1", critical: false },
    KnownGood { ext: ".vbs", prog_id: "VBSFile", critical: false },
    KnownGood { ext: ".js", prog_id: "JSFile", critical: false },
    KnownGood { ext: ".msi", prog_id: "Msi.Package", critical: true },
    KnownGood { ext: ".reg", prog_id: "regfile", critical: true },
    KnownGood { ext: ".url", prog_id: "InternetShortcut", critical: false },
    KnownGood { ext: ".zip", prog_id: "CompressedFolder", critical: false },
];

pub fn known(ext: &str) -> Option<KnownGood> {
    TABLE.iter().copied().find(|k| k.ext.eq_ignore_ascii_case(ext))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Known type whose merged HKCR default is empty: the ".bat has no handler" bug.
    NoHandler,
    /// Critical type whose class points somewhere else.
    WrongClass { found: String },
    /// Critical type with a per-user choice (e.g. .exe -> Notepad).
    UserOverride { prog_id: String },
    /// Per-user class default naming a ProgID that doesn't exist.
    DanglingUserDefault { prog_id: String },
    ChoiceMissingProgId { prog_id: String },
    ChoiceMissingExe { prog_id: String, exe: String },
    ChoiceBadHash { prog_id: String },
    /// Informational: none of the existing UserChoice hashes validate on this build.
    HashUnverifiable,
    /// Informational: the Windows class itself lacks an open command (needs sfc /scannow).
    ClassDamaged { prog_id: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Issue {
    pub target: String,
    pub kind: Kind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FixKind {
    /// Steps 1-5 with the known-good table.
    RestoreKnown,
    /// Delete the UserChoice key; Windows falls back to the registered default.
    ClearChoice,
    /// Delete the dangling HKCU class default value.
    ClearUserDefault,
    None,
}

impl Issue {
    pub fn fix(&self) -> FixKind {
        match self.kind {
            Kind::NoHandler | Kind::WrongClass { .. } | Kind::UserOverride { .. } => FixKind::RestoreKnown,
            Kind::ChoiceMissingProgId { .. } | Kind::ChoiceMissingExe { .. } | Kind::ChoiceBadHash { .. } => FixKind::ClearChoice,
            Kind::DanglingUserDefault { .. } => FixKind::ClearUserDefault,
            Kind::HashUnverifiable | Kind::ClassDamaged { .. } => FixKind::None,
        }
    }
    pub fn severity(&self) -> i32 {
        match self.kind {
            Kind::NoHandler | Kind::WrongClass { .. } | Kind::UserOverride { .. } => 2,
            Kind::HashUnverifiable => 0,
            _ => 1,
        }
    }
    pub fn title(&self) -> String {
        let t = &self.target;
        match &self.kind {
            Kind::NoHandler => format!("{t} files have no app"),
            Kind::WrongClass { .. } => format!("{t} files don't open the Windows way"),
            Kind::UserOverride { .. } => format!("{t} files are redirected to another app"),
            Kind::DanglingUserDefault { .. } => format!("{t} points to a removed app"),
            Kind::ChoiceMissingProgId { .. } => format!("Default for {t} no longer exists"),
            Kind::ChoiceMissingExe { .. } => format!("Default app for {t} is missing"),
            Kind::ChoiceBadHash { .. } => format!("Default for {t} is ignored by Windows"),
            Kind::HashUnverifiable => "Can't check default-app signatures".into(),
            Kind::ClassDamaged { prog_id } => format!("Windows' {prog_id} handler is damaged"),
        }
    }
    pub fn detail(&self) -> String {
        let t = &self.target;
        match &self.kind {
            Kind::NoHandler => {
                let p = known(t).map(|k| k.prog_id).unwrap_or("?");
                format!("Double-clicking a {t} file asks which app to use, although Windows' handler ({p}) is still installed. Fix restores it.")
            }
            Kind::WrongClass { found } => format!("{t} is mapped to \"{found}\" instead of the Windows handler. Fix restores the Windows default."),
            Kind::UserOverride { prog_id } => format!("Your account opens {t} with \"{prog_id}\". For this type that usually breaks things. Fix restores the Windows default."),
            Kind::DanglingUserDefault { prog_id } => format!("Your account maps {t} to \"{prog_id}\", which isn't installed. Fix removes that leftover."),
            Kind::ChoiceMissingProgId { prog_id } => format!("{t} is set to \"{prog_id}\", which was uninstalled. Fix clears it so Windows uses the next available app."),
            Kind::ChoiceMissingExe { exe, .. } => format!("{t} is set to {exe}, which no longer exists. Fix clears it so Windows uses the next available app."),
            Kind::ChoiceBadHash { prog_id } => format!("{t} is set to \"{prog_id}\", but its signature doesn't match, so Windows ignores it. Fix clears it."),
            Kind::HashUnverifiable => "This Windows version signs default-app choices in a way DefaultFast can't verify. Changes will be checked after writing, or use Windows Settings.".into(),
            Kind::ClassDamaged { prog_id } => format!("HKCR\\{prog_id} has no open command. Run \"sfc /scannow\" from an admin terminal to repair Windows files."),
        }
    }
}

/// Machine part of a known-good fix: needed only when HKLM's default is wrong.
pub fn needs_machine(reg: &dyn Registry, issue: &Issue) -> Option<KnownGood> {
    if issue.fix() != FixKind::RestoreKnown {
        return None;
    }
    let k = known(&issue.target)?;
    let current = reg.string(HKLM, &format!("SOFTWARE\\Classes\\{}", k.ext), "");
    (!current.as_deref().is_some_and(|c| c.eq_ignore_ascii_case(k.prog_id))).then_some(k)
}

/// Step 4: the class has something to run.
pub fn class_ok(reg: &dyn Registry, prog_id: &str) -> bool {
    match prog_id {
        // Shortcuts resolve through IsShortcut + shell extensions, not an open command.
        "lnkfile" => reg.get(HKCR, "lnkfile", "IsShortcut").is_some(),
        _ => reg.exists(HKCR, &format!("{prog_id}\\shell\\open\\command")) || discovery::prog_command(reg, prog_id).is_some(),
    }
}

fn choice_parents() -> [String; 2] {
    [userchoice::base_key(".x"), userchoice::base_key("x")].map(|k| k.rsplit_once('\\').map(|p| p.0.to_owned()).unwrap_or_default())
}

/// `exe_exists` and `expand` are injected so the scan is testable without the file system.
pub fn scan(reg: &dyn Registry, sid: &str, exe_exists: &dyn Fn(&str) -> bool, expand: &dyn Fn(&str) -> String) -> Vec<Issue> {
    let mut out = Vec::new();
    for k in TABLE {
        if !reg.exists(HKCR, k.prog_id) {
            continue; // optional component not installed (e.g. VBScript removed)
        }
        let target = k.ext.to_owned();
        let class = reg.string(HKCR, k.ext, "").unwrap_or_default();
        if class.is_empty() {
            out.push(Issue { target: target.clone(), kind: Kind::NoHandler });
        } else if !class.eq_ignore_ascii_case(k.prog_id) && k.critical {
            out.push(Issue { target: target.clone(), kind: Kind::WrongClass { found: class } });
        } else if !class.eq_ignore_ascii_case(k.prog_id) && !reg.exists(HKCR, &class) {
            out.push(Issue { target: target.clone(), kind: Kind::WrongClass { found: class } });
        }
        if k.critical {
            if let Some(c) = userchoice::read(reg, k.ext).filter(|c| !c.prog_id.eq_ignore_ascii_case(k.prog_id)) {
                if !out.iter().any(|i| i.target == target) {
                    out.push(Issue { target: target.clone(), kind: Kind::UserOverride { prog_id: c.prog_id } });
                }
            }
        }
        if !class_ok(reg, k.prog_id) {
            out.push(Issue { target, kind: Kind::ClassDamaged { prog_id: k.prog_id.to_owned() } });
        }
    }

    // Leftover per-user class defaults pointing at uninstalled ProgIDs.
    for key in reg.subkeys(HKCU, "Software\\Classes") {
        if !key.starts_with('.') {
            continue;
        }
        let Some(ext) = discovery::normalize_extension(&key) else { continue };
        if let Some(p) = reg.string(HKCU, &format!("Software\\Classes\\{key}"), "").filter(|p| !p.is_empty()) {
            if !reg.exists(HKCR, &p) && !out.iter().any(|i| i.target == ext) {
                out.push(Issue { target: ext, kind: Kind::DanglingUserDefault { prog_id: p } });
            }
        }
    }

    // Every legacy UserChoice: missing ProgID, missing executable, invalid hash.
    let (algo, _, _) = userchoice::algorithm_status(reg, sid);
    if algo == Algorithm::Mismatch {
        out.push(Issue { target: String::new(), kind: Kind::HashUnverifiable });
    }
    for parent in choice_parents() {
        for t in reg.subkeys(HKCU, &parent) {
            let target = t.to_lowercase();
            if out.iter().any(|i| i.target == target) || userchoice::uses_latest(reg, &target) {
                continue; // Windows uses UserChoiceLatest there; the legacy key is inert
            }
            let Some(c) = userchoice::read(reg, &target) else { continue };
            if c.prog_id.is_empty() {
                continue;
            }
            let kind = if !reg.exists(HKCR, &c.prog_id) {
                Some(Kind::ChoiceMissingProgId { prog_id: c.prog_id.clone() })
            } else if let Some(exe) = discovery::prog_command(reg, &c.prog_id)
                .and_then(|cmd| discovery::parse_command_exe(&expand(&cmd)))
                .filter(|e| e.contains(":\\") && !exe_exists(e))
            {
                Some(Kind::ChoiceMissingExe { prog_id: c.prog_id.clone(), exe })
            } else if algo == Algorithm::Confirmed && userchoice::hash(&target, sid, &c.prog_id, c.time) != c.hash {
                Some(Kind::ChoiceBadHash { prog_id: c.prog_id.clone() })
            } else {
                None
            };
            if let Some(kind) = kind {
                out.push(Issue { target, kind });
            }
        }
    }
    out.sort_by_key(|i| (-i.severity(), i.target.clone()));
    out
}

/// Everything a set of fixes will touch, for the pre-write backup.
pub fn backup_ops(reg: &dyn Registry, issues: &[Issue]) -> Result<Vec<Op>> {
    let mut ops = Vec::new();
    for i in issues {
        match i.fix() {
            FixKind::RestoreKnown => {
                ops.extend(backup::snapshot_tree(reg, HKCU, &userchoice::base_key(&i.target))?);
                ops.extend(backup::snapshot_tree(reg, HKCU, &format!("Software\\Classes\\{}", i.target))?);
                if needs_machine(reg, i).is_some() {
                    ops.push(backup::snapshot_value(reg, HKLM, &format!("SOFTWARE\\Classes\\{}", i.target), "")?);
                }
            }
            FixKind::ClearChoice => ops.extend(backup::snapshot_tree(reg, HKCU, &userchoice::choice_key(&i.target))?),
            FixKind::ClearUserDefault => ops.push(backup::snapshot_value(reg, HKCU, &format!("Software\\Classes\\{}", i.target), "")?),
            FixKind::None => {}
        }
    }
    Ok(ops)
}

/// Applies the per-user part of each fix (steps 1-2 / clear choice / clear value).
/// Returns the known-good entries whose HKLM default still needs the elevated helper.
pub fn fix_user(reg: &dyn Registry, issues: &[Issue]) -> Result<Vec<KnownGood>> {
    let mut machine = Vec::new();
    for i in issues {
        match i.fix() {
            FixKind::RestoreKnown => {
                reg.delete_tree(HKCU, &userchoice::base_key(&i.target))?;
                reg.delete_tree(HKCU, &format!("Software\\Classes\\{}", i.target))?;
                if let Some(k) = needs_machine(reg, i) {
                    machine.push(k);
                }
            }
            FixKind::ClearChoice => userchoice::clear(reg, &i.target)?,
            FixKind::ClearUserDefault => reg.delete_value(HKCU, &format!("Software\\Classes\\{}", i.target), "")?,
            FixKind::None => {}
        }
    }
    Ok(machine)
}

/// Step 3, run by the elevated helper (or directly when already elevated).
pub fn fix_machine(reg: &dyn Registry, k: KnownGood) -> Result<()> {
    reg.set(HKLM, &format!("SOFTWARE\\Classes\\{}", k.ext), "", &crate::registry::Value::Sz(k.prog_id.to_owned()))
}

/// Steps 3-4 check after fixing: merged default is right and the class can open files.
pub fn verify_known(reg: &dyn Registry, k: KnownGood) -> bool {
    reg.string(HKCR, k.ext, "").is_some_and(|c| c.eq_ignore_ascii_case(k.prog_id)) && class_ok(reg, k.prog_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{MemRegistry, Value};

    const SID: &str = "S-1-5-21-3";

    fn windows_like() -> MemRegistry {
        let r = MemRegistry::new();
        for k in TABLE {
            r.seed(HKLM, &format!("SOFTWARE\\Classes\\{}", k.ext), "", Value::Sz(k.prog_id.into()));
            r.seed(HKLM, &format!("SOFTWARE\\Classes\\{}\\shell\\open\\command", k.prog_id), "", Value::Sz("x.exe \"%1\"".into()));
        }
        r.seed(HKLM, "SOFTWARE\\Classes\\lnkfile", "IsShortcut", Value::Sz(String::new()));
        r
    }
    fn no_fs(_: &str) -> bool {
        true
    }
    fn id(s: &str) -> String {
        s.to_owned()
    }

    #[test]
    fn healthy_system_has_no_issues() {
        let r = windows_like();
        assert_eq!(scan(&r, SID, &no_fs, &id), vec![]);
    }

    #[test]
    fn bat_cmd_no_handler_from_empty_user_override() {
        let r = windows_like();
        // The real-world bug: empty HKCU class keys shadow HKLM.
        r.seed_key(HKCU, "Software\\Classes\\.bat");
        r.seed_key(HKCU, "Software\\Classes\\.cmd");
        let issues = scan(&r, SID, &no_fs, &id);
        assert_eq!(issues.iter().map(|i| (i.target.as_str(), i.kind.clone())).collect::<Vec<_>>(), vec![(".bat", Kind::NoHandler), (".cmd", Kind::NoHandler)]);
        assert!(issues.iter().all(|i| needs_machine(&r, i).is_none()), "HKLM is fine: no UAC needed");
        let machine = fix_user(&r, &issues).unwrap();
        assert!(machine.is_empty());
        assert!(verify_known(&r, known(".bat").unwrap()) && verify_known(&r, known(".cmd").unwrap()));
        assert_eq!(scan(&r, SID, &no_fs, &id), vec![]);
    }

    #[test]
    fn bat_no_handler_from_wiped_machine_default_needs_admin() {
        let r = windows_like();
        r.seed(HKLM, "SOFTWARE\\Classes\\.bat", "", Value::Sz(String::new()));
        userchoice::write(&r, ".bat", "Applications\\notepad.exe", SID).unwrap();
        let issues = scan(&r, SID, &no_fs, &id);
        assert_eq!(issues[0].kind, Kind::NoHandler);
        let ops = backup_ops(&r, &issues).unwrap();
        assert!(ops.iter().any(|o| matches!(o, Op::SetValue(Hive::LocalMachine, _, _, _))), "HKLM value backed up");
        let machine = fix_user(&r, &issues).unwrap();
        assert_eq!(machine, vec![known(".bat").unwrap()]);
        assert!(userchoice::read(&r, ".bat").is_none(), "step 1 removed the user choice");
        fix_machine(&r, machine[0]).unwrap(); // what the elevated helper does
        assert!(verify_known(&r, machine[0]));
    }

    #[test]
    fn critical_override_and_wrong_class() {
        let r = windows_like();
        r.seed(HKLM, "SOFTWARE\\Classes\\.reg", "", Value::Sz("Notepad++_file".into()));
        r.seed_key(HKLM, "SOFTWARE\\Classes\\Notepad++_file");
        userchoice::write(&r, ".exe", "Applications\\notepad.exe", SID).unwrap();
        r.seed(HKLM, "SOFTWARE\\Classes\\Applications\\notepad.exe\\shell\\open\\command", "", Value::Sz("notepad.exe %1".into()));
        // A non-critical type with a user preference is NOT an issue.
        userchoice::write(&r, ".txt", "Applications\\notepad.exe", SID).unwrap();
        let kinds: Vec<_> = scan(&r, SID, &no_fs, &id).into_iter().map(|i| { let f = i.fix(); (i.target, f) }).collect();
        assert_eq!(kinds, vec![(".exe".into(), FixKind::RestoreKnown), (".reg".into(), FixKind::RestoreKnown)]);
    }

    #[test]
    fn broken_user_choices() {
        let r = windows_like();
        r.seed(HKLM, "SOFTWARE\\Classes\\Gone.App\\shell\\open\\command", "", Value::Sz("\"C:\\Gone\\gone.exe\" \"%1\"".into()));
        userchoice::write(&r, ".mkv", "Removed.mkv", SID).unwrap();
        userchoice::write(&r, ".flac", "Gone.App", SID).unwrap();
        userchoice::write(&r, ".ogg", "txtfile", SID).unwrap();
        r.seed(HKCU, &userchoice::choice_key(".ogg"), "Hash", Value::Sz("AAAAAAAAAAA=".into()));
        let exists = |p: &str| !p.starts_with("C:\\Gone");
        let issues = scan(&r, SID, &exists, &id);
        let kinds: Vec<_> = issues.iter().map(|i| (i.target.as_str(), std::mem::discriminant(&i.kind))).collect();
        assert_eq!(kinds.len(), 3, "{issues:?}");
        assert!(issues.iter().any(|i| i.target == ".mkv" && matches!(i.kind, Kind::ChoiceMissingProgId { .. })));
        assert!(issues.iter().any(|i| i.target == ".flac" && matches!(i.kind, Kind::ChoiceMissingExe { .. })));
        assert!(issues.iter().any(|i| i.target == ".ogg" && matches!(i.kind, Kind::ChoiceBadHash { .. })));
        fix_user(&r, &issues).unwrap();
        assert_eq!(scan(&r, SID, &exists, &id), vec![]);
    }

    #[test]
    fn fix_is_undoable_from_backup() {
        let r = windows_like();
        r.seed(HKCU, "Software\\Classes\\.bat\\OpenWithProgids", "x", Value::Sz(String::new()));
        let issues = scan(&r, SID, &no_fs, &id);
        let ops = backup_ops(&r, &issues).unwrap();
        fix_user(&r, &issues).unwrap();
        assert!(!r.exists(HKCU, "Software\\Classes\\.bat"));
        backup::restore(&r, &ops, SID).unwrap();
        assert!(r.exists(HKCU, "Software\\Classes\\.bat\\OpenWithProgids"));
    }
}
