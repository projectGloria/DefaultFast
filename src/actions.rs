//! What each button does. Writes run on the worker thread (`run`), results come back to the UI
//! thread (`on_reply`) as toasts with Undo / Open Settings.
use crate::{
    backup,
    defaults::{self, Report, Request, Status},
    logfile,
    registry::Registry,
    shell::{self, AssocStr},
    ui,
    worker::Job,
};
use std::{cell::RefCell, path::PathBuf};

pub enum Action {
    Health,
    Apply { req: Request, settings_uri: String },
    Restore { path: PathBuf, undo: bool },
}

pub enum Outcome {
    Health,
    Applied { report: Report, app_name: String, settings_uri: String },
    Restored(Result<String, String>),
}

thread_local! {
    /// Backup created by the most recent change: what the toast's Undo restores.
    static LAST_BACKUP: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
    static LAST_SETTINGS: RefCell<String> = const { RefCell::new(String::new()) };
    /// Action waiting for the confirm dialog.
    static PENDING: RefCell<Option<Action>> = const { RefCell::new(None) };
}

// ------------------------------------------------------------------------------------------
// Worker side
// ------------------------------------------------------------------------------------------

/// Shell's view of the effective handler matches what we wrote.
fn verify(target: &str, prog_id: &str, exe: &str) -> bool {
    let eff_prog = shell::assoc_query(target, AssocStr::ProgId).unwrap_or_default();
    let eff_exe = shell::assoc_query(target, AssocStr::Executable).unwrap_or_default();
    eff_prog.eq_ignore_ascii_case(prog_id) || (!exe.is_empty() && eff_exe.eq_ignore_ascii_case(exe))
}

pub fn run(reg: &dyn Registry, action: Action) -> Outcome {
    match action {
        Action::Health => Outcome::Health,
        Action::Apply { req, settings_uri } => {
            let report = match shell::user_sid() {
                Ok(sid) => defaults::apply(reg, &verify, &sid, &req, &shell::file_stamp(), &shell::timestamp(), &backup::dir()),
                Err(e) => Report { results: req.changes.iter().map(|(t, _)| (t.clone(), Status::Failed(format!("Can't read your account SID: {e}")))).collect(), backup: None },
            };
            logfile::write(&format!("APPLY {} -> {:?}", req.app_name, report.results));
            Outcome::Applied { report, app_name: req.app_name, settings_uri }
        }
        Action::Restore { path, undo } => Outcome::Restored(restore(reg, &path, undo)),
    }
}

/// Restores a backup after snapshotting the current state (so the restore is undoable too).
fn restore(reg: &dyn Registry, path: &std::path::Path, undo: bool) -> Result<String, String> {
    let (summary, _, ops) = backup::load(path).map_err(|e| e.to_string())?;
    let sid = shell::user_sid().map_err(|e| e.to_string())?;
    let current = backup::coverage(reg, &ops);
    backup::save(&backup::dir(), &shell::file_stamp(), &shell::timestamp(), &format!("Before restoring: {summary}"), &current).map_err(|e| format!("Could not back up the current state, so nothing was restored: {e}"))?;
    let machine = backup::restore(reg, &ops, &sid).map_err(|e| e.to_string())?;
    crate::shell_notify();
    if !machine.is_empty() {
        crate::elevated::restore_machine(&machine).map_err(|e| e.to_string())?;
        crate::shell_notify();
    }
    if undo {
        backup::mark_undone(path);
    }
    Ok(if undo { format!("Undone: {summary}") } else { format!("Restored: {summary}") })
}

// ------------------------------------------------------------------------------------------
// UI side
// ------------------------------------------------------------------------------------------

fn join_targets(t: &[String]) -> String {
    match t.len() {
        0 => String::new(),
        1 => t[0].clone(),
        2 => format!("{} and {}", t[0], t[1]),
        n if n <= 4 => format!("{} and {}", t[..n - 1].join(", "), t[n - 1]),
        n => format!("{}, {} and {} more", t[0], t[1], n - 2),
    }
}

pub fn on_reply(reply: crate::worker::Reply) {
    let crate::worker::Reply::Action(outcome) = reply else { return };
    if let Some(app) = ui::main_app() {
        app.set_busy(false);
    }
    match outcome {
        Outcome::Health => crate::repair_ui::health_done(),
        Outcome::Applied { report, app_name, settings_uri } => {
            let pick = |f: fn(&Status) -> bool| report.results.iter().filter(|r| f(&r.1)).map(|r| r.0.clone()).collect::<Vec<_>>();
            let done = pick(|s| *s == Status::Done);
            let protected = pick(|s| matches!(s, Status::Protected(_)));
            let failed: Vec<String> = report.results.iter().filter_map(|r| if let Status::Failed(e) = &r.1 { Some(e.clone()) } else { None }).collect();
            LAST_BACKUP.with(|b| *b.borrow_mut() = report.backup.clone());
            LAST_SETTINGS.with(|s| *s.borrow_mut() = settings_uri);
            let mut text = String::new();
            if !done.is_empty() {
                text.push_str(&format!("{app_name} now opens {}.", join_targets(&done)));
            }
            if !protected.is_empty() {
                text.push_str(&format!(
                    "{}Windows protects {} and kept the old app. Open Settings to choose {app_name} there.",
                    if text.is_empty() { "" } else { " " },
                    join_targets(&protected)
                ));
            }
            if !failed.is_empty() {
                text.push_str(&format!("{}{}", if text.is_empty() { "" } else { " " }, failed[0]));
            }
            let kind = if !done.is_empty() && protected.is_empty() && failed.is_empty() { 0 } else if done.is_empty() && protected.is_empty() { 1 } else { 2 };
            ui::toast(&text, kind, report.backup.is_some(), !protected.is_empty());
            ui::refresh_detail();
            crate::repair_ui::history_changed();
        }
        Outcome::Restored(r) => {
            match r {
                Ok(msg) => ui::toast(&msg, 0, false, false),
                Err(e) => ui::toast(&format!("Restore stopped: {e}"), 1, false, false),
            }
            LAST_BACKUP.with(|b| *b.borrow_mut() = None);
            ui::refresh_detail();
            crate::repair_ui::history_changed();
        }
    }
}

pub(crate) fn submit(action: Action, status: &str) {
    if let Some(app) = ui::main_app() {
        app.set_busy(true);
        app.set_status_text(status.into());
    }
    ui::send(Job::Action(action));
}

/// Asks first when a change makes double-click stop *running* scripts/installers.
fn apply_with_confirm(req: Request, settings_uri: String) {
    let scripts: Vec<String> = req.changes.iter().map(|c| c.0.clone()).filter(|t| defaults::SCRIPT_TYPES.contains(&t.as_str())).collect();
    let name = req.app_name.clone();
    let action = Action::Apply { req, settings_uri };
    if scripts.is_empty() {
        submit(action, "Applying…");
        return;
    }
    confirm(
        action,
        &format!("Open {} with {name}?", join_targets(&scripts)),
        &format!("Double-clicking {} files will open them in {name} instead of running them. You can undo this from History.", join_targets(&scripts)),
        "Change it",
        false,
    );
}

pub(crate) fn confirm(action: Action, title: &str, text: &str, ok: &str, shield: bool) {
    PENDING.with(|p| *p.borrow_mut() = Some(action));
    if let Some(app) = ui::main_app() {
        app.set_confirm_title(title.into());
        app.set_confirm_text(text.into());
        app.set_confirm_ok(ok.into());
        app.set_confirm_shield(shield);
        app.set_confirm_visible(true);
    }
}

/// Deep link to the app's page in Settings when it is registered; generic page otherwise.
pub fn settings_uri(registered: Option<(String, bool)>) -> String {
    match registered {
        Some((name, machine)) if !name.is_empty() => {
            let enc: String = name.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect();
            format!("ms-settings:defaultapps?registeredApp{}={enc}", if machine { "Machine" } else { "User" })
        }
        _ => "ms-settings:defaultapps".into(),
    }
}

fn apply_program(all: bool) {
    let Some((key, pairs)) = ui::checked_supports(all) else { return };
    let Some(p) = ui::program(&key) else { return };
    if pairs.is_empty() {
        return;
    }
    let req = Request { app_name: p.name.clone(), exe: p.exe.clone(), changes: pairs };
    apply_with_confirm(req, settings_uri(Some((p.registered_name.clone(), p.registered_machine))));
}

fn set_handler(i: i32) {
    let Some((target, cands)) = ui::current_target() else { return };
    let Some(c) = usize::try_from(i).ok().and_then(|i| cands.get(i)).cloned() else { return };
    if c.is_current {
        return;
    }
    let registered = ui::registered_for(&c.exe, &c.aumid);
    let req = Request { app_name: c.name.clone(), exe: c.exe.clone(), changes: vec![(target, c.prog_id.clone())] };
    apply_with_confirm(req, settings_uri(registered));
}

fn browse() {
    let Some((target, _)) = ui::current_target() else { return };
    let owner = crate::window::hwnd();
    // The file dialog runs its own modal loop: give it its own STA thread.
    std::thread::spawn(move || {
        let Some(exe) = shell::browse_exe(owner) else { return };
        let _ = slint::invoke_from_event_loop(move || {
            if exe.starts_with("\\\\") {
                ui::toast("Programs on network shares can't be registered. Copy it to this PC first.", 1, false, false);
                return;
            }
            let (desc, _) = shell::version_info(&exe);
            let name = if desc.is_empty() { std::path::Path::new(&exe).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default() } else { desc };
            let req = Request { app_name: name, exe: exe.clone(), changes: vec![(target, defaults::applications_prog_id(&exe))] };
            apply_with_confirm(req, settings_uri(None));
        });
    });
}

pub fn wire(app: &ui::AppWindow) {
    app.on_apply_selected(|| apply_program(false));
    app.on_apply_all(|| apply_program(true));
    app.on_set_handler(set_handler);
    app.on_browse(browse);
    app.on_open_settings(|| {
        let _ = shell::open_uri("ms-settings:defaultapps");
    });
    app.on_toast_undo_clicked(|| {
        if let Some(path) = LAST_BACKUP.with(|b| b.borrow_mut().take()) {
            if let Some(app) = ui::main_app() {
                app.set_toast_visible(false);
            }
            submit(Action::Restore { path, undo: true }, "Undoing…");
        }
    });
    app.on_toast_settings_clicked(|| {
        let uri = LAST_SETTINGS.with(|s| s.borrow().clone());
        let _ = shell::open_uri(if uri.is_empty() { "ms-settings:defaultapps" } else { &uri });
    });
    app.on_confirm_accept(|| {
        if let Some(app) = ui::main_app() {
            app.set_confirm_visible(false);
        }
        if let Some(action) = PENDING.with(|p| p.borrow_mut().take()) {
            let status = match &action {
                Action::Restore { .. } => "Restoring…",
                _ => "Applying…",
            };
            submit(action, status);
        }
    });
    app.on_confirm_cancel(|| {
        PENDING.with(|p| p.borrow_mut().take());
        if let Some(app) = ui::main_app() {
            app.set_confirm_visible(false);
        }
    });
    crate::repair_ui::wire(app);
}
