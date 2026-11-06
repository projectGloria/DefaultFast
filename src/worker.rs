//! The single background thread that runs Shell queries and *all* registry writes, so writes
//! are serialized and the UI thread never blocks. Results go back as `Reply` values.
use crate::{
    model::Index,
    registry::{Hive, Registry, WinRegistry},
    shell::{self, AssocStr},
};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    mpsc, Arc,
};

#[derive(Clone, Debug, Default)]
pub struct Candidate {
    pub name: String,
    pub exe: String,
    pub aumid: String,
    pub prog_id: String,
    pub recommended: bool,
    pub is_current: bool,
}

pub enum Job {
    /// Current default for each (target, prog_id) of a program.
    ProgramDefaults { seq: u64, key: String, targets: Vec<(String, String)>, exe: String },
    /// Current default + ranked candidate apps for a file type / protocol.
    TargetInfo { seq: u64, target: String, index: Arc<Index> },
    Theme,
    Health,
    Action(crate::actions::Action),
}

pub enum Reply {
    ProgramDefaults { key: String, rows: Vec<(String, String, bool)> },
    TargetInfo { target: String, current: String, candidates: Vec<Candidate> },
    Theme { dark: bool, accent: Option<([u8; 3], [u8; 3])> },
    Action(crate::actions::Outcome),
}

pub fn system_dark(reg: &dyn Registry) -> bool {
    matches!(reg.get(Hive::CurrentUser, "Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize", "AppsUseLightTheme"), Some(crate::registry::Value::Dword(0)))
}

pub fn accent_colors(reg: &dyn Registry) -> Option<([u8; 3], [u8; 3])> {
    if let Some(crate::registry::Value::Raw(_, b)) =
        reg.get(Hive::CurrentUser, "Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\Accent", "AccentPalette")
    {
        if b.len() >= 32 {
            return Some(([b[16], b[17], b[18]], [b[4], b[5], b[6]]));
        }
    }
    None
}

pub fn spawn(reg: Arc<WinRegistry>, latest_seq: Arc<AtomicU64>, sink: impl Fn(Reply) + Send + 'static) -> mpsc::Sender<Job> {
    let (tx, rx) = mpsc::channel::<Job>();
    std::thread::Builder::new()
        .name("worker".into())
        .spawn(move || {
            let _com = shell::Com::init();
            for job in rx {
                let reply = match job {
                    Job::ProgramDefaults { seq, key, targets, exe } => {
                        if seq != latest_seq.load(Ordering::Relaxed) {
                            continue;
                        }
                        let mut rows = Vec::with_capacity(targets.len());
                        let mut aborted = false;
                        for (t, prog) in targets {
                            if seq != latest_seq.load(Ordering::Relaxed) {
                                aborted = true;
                                break;
                            }
                            let (name, current) = current_default(&t, &prog, &exe);
                            rows.push((t, name, current));
                        }
                        if aborted {
                            continue;
                        }
                        Reply::ProgramDefaults { key, rows }
                    }
                    Job::TargetInfo { seq, target, index } => {
                        if seq != latest_seq.load(Ordering::Relaxed) {
                            continue;
                        }
                        let current = shell::assoc_query(&target, AssocStr::FriendlyApp)
                            .unwrap_or_else(|| "None. Windows asks which app to use.".into());
                        let candidates = candidates(reg.as_ref(), &target, &index);
                        Reply::TargetInfo { target, current, candidates }
                    }
                    Job::Theme => Reply::Theme { dark: system_dark(reg.as_ref()), accent: accent_colors(reg.as_ref()) },
                    Job::Health => Reply::Action(crate::actions::run(reg.as_ref(), crate::actions::Action::Health)),
                    Job::Action(a) => Reply::Action(crate::actions::run(reg.as_ref(), a)),
                };
                sink(reply);
            }
        })
        .expect("spawn worker");
    tx
}

fn same_path(a: &str, b: &str) -> bool {
    !a.is_empty() && a.eq_ignore_ascii_case(b)
}

/// (display name of the effective handler, whether it already is `prog_id` / `exe`).
pub fn current_default(target: &str, prog_id: &str, exe: &str) -> (String, bool) {
    let name = shell::assoc_query(target, AssocStr::FriendlyApp).unwrap_or_else(|| "nothing (Windows asks)".into());
    let eff_prog = shell::assoc_query(target, AssocStr::ProgId).unwrap_or_default();
    let current = (!prog_id.is_empty() && eff_prog.eq_ignore_ascii_case(prog_id))
        || (!exe.is_empty() && shell::assoc_query(target, AssocStr::Executable).is_some_and(|e| same_path(exe, &e)));
    (name, current)
}

/// Ranked handlers: Shell's recommended list, then everything Shell offers, then programs
/// that declare the target in their capabilities. Each gets the ProgID that will be written.
fn candidates(reg: &dyn Registry, target: &str, index: &Index) -> Vec<Candidate> {
    let eff_prog = shell::assoc_query(target, AssocStr::ProgId).unwrap_or_default();
    let eff_exe = shell::assoc_query(target, AssocStr::Executable).unwrap_or_default();
    let mut out: Vec<Candidate> = Vec::new();
    let push = |c: Candidate, out: &mut Vec<Candidate>| {
        let dup = out.iter().any(|o| (same_path(&o.exe, &c.exe)) || (!o.aumid.is_empty() && o.aumid == c.aumid) || (!c.prog_id.is_empty() && o.prog_id.eq_ignore_ascii_case(&c.prog_id)));
        if !dup {
            out.push(c);
        }
    };
    for h in shell::assoc_handlers(target) {
        let mut c = Candidate { name: h.name.clone(), recommended: h.recommended, ..Default::default() };
        if h.identity.contains(":\\") {
            c.exe = h.identity.clone();
            // Prefer the app's own ProgID for this target; else the generic Applications\ entry.
            c.prog_id = index
                .programs
                .iter()
                .find(|p| same_path(&p.exe, &c.exe))
                .and_then(|p| p.supports.iter().find(|s| s.target == target))
                .map(|s| s.prog_id.clone())
                .unwrap_or_else(|| format!("Applications\\{}", h.identity.rsplit('\\').next().unwrap_or_default()));
        } else if reg.exists(Hive::ClassesRoot, &h.identity) {
            c.prog_id = h.identity.clone();
            c.aumid = reg.string(Hive::ClassesRoot, &format!("{}\\Application", h.identity), "AppUserModelID").unwrap_or_default();
        } else {
            continue;
        }
        push(c, &mut out);
    }
    for (p, s) in index.programs_for(target) {
        push(Candidate { name: p.name.clone(), exe: p.exe.clone(), aumid: p.aumid.clone(), prog_id: s.prog_id.clone(), recommended: false, is_current: false }, &mut out);
    }
    for c in &mut out {
        c.is_current = (!c.prog_id.is_empty() && c.prog_id.eq_ignore_ascii_case(&eff_prog)) || same_path(&c.exe, &eff_exe);
    }
    // Current default first, then recommended, keeping Shell's order otherwise.
    out.sort_by_key(|c| (!c.is_current, !c.recommended));
    out
}
