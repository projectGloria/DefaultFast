//! Health check (Repair) and History views: UI-thread state and callbacks. The scan, fixes,
//! backup listing and restores all run on the worker (see actions.rs).
use crate::{
    actions::{self, Action},
    backup::Entry,
    repair::{FixKind, Issue},
    shell, ui,
    ui::{HistoryRow, IssueRow},
    worker::Job,
};
use slint::{ModelRc, VecModel};
use std::{cell::RefCell, rc::Rc};

thread_local! {
    static ISSUES: RefCell<Vec<(Issue, bool)>> = const { RefCell::new(Vec::new()) };
    static HISTORY: RefCell<Vec<Entry>> = const { RefCell::new(Vec::new()) };
    static ISSUE_MODEL: Rc<VecModel<IssueRow>> = Rc::new(VecModel::default());
    static HISTORY_MODEL: Rc<VecModel<HistoryRow>> = Rc::new(VecModel::default());
}

pub fn run_health() {
    if let Some(app) = ui::main_app() {
        app.set_health_state(0);
        app.set_health_text("Checking…".into());
    }
    ui::send(Job::Health);
}

pub fn health_done(issues: Vec<(Issue, bool)>) {
    let rows: Vec<IssueRow> = issues
        .iter()
        .map(|(i, admin)| IssueRow {
            title: i.title().into(),
            detail: i.detail().into(),
            action: if i.fix() == FixKind::None { "".into() } else { "Fix".into() },
            severity: i.severity(),
            admin: *admin,
        })
        .collect();
    let fixable = issues.iter().filter(|(i, _)| i.fix() != FixKind::None).count();
    ISSUE_MODEL.with(|m| m.set_vec(rows));
    ISSUES.with(|s| *s.borrow_mut() = issues);
    if let Some(app) = ui::main_app() {
        app.set_fixable_count(fixable as i32);
        app.set_health_state(if fixable == 0 { 1 } else { 2 });
        app.set_health_text(match fixable {
            0 => "All good".into(),
            1 => "1 issue · Fix".into(),
            n => format!("{n} issues · Fix").into(),
        });
    }
}

fn fix(issues: Vec<(Issue, bool)>) {
    let issues: Vec<(Issue, bool)> = issues.into_iter().filter(|(i, _)| i.fix() != FixKind::None).collect();
    if issues.is_empty() {
        return;
    }
    let admin: Vec<String> = issues.iter().filter(|(_, a)| *a).map(|(i, _)| i.target.clone()).collect();
    let action = Action::Fix(issues.into_iter().map(|(i, _)| i).collect());
    if admin.is_empty() {
        actions::submit(action, "Repairing…");
        return;
    }
    let list = admin.join(", ");
    actions::confirm(
        action,
        "Windows will ask for admin permission",
        &format!(
            "Restoring the Windows default for {list} changes one system-wide setting (HKLM\\SOFTWARE\\Classes). \
             DefaultFast runs a tiny helper with admin rights for just that step; everything else stays in your account. \
             A backup is saved first, and Undo puts it back."
        ),
        "Continue",
        true,
    );
}

pub fn history_loaded(entries: Vec<Entry>) {
    let rows: Vec<HistoryRow> = entries.iter().map(|e| HistoryRow { title: e.summary.as_str().into(), when: e.created.as_str().into(), undone: e.undone }).collect();
    HISTORY_MODEL.with(|m| m.set_vec(rows));
    HISTORY.with(|h| *h.borrow_mut() = entries);
}

/// Refreshes the History list after a change, if it is on screen.
pub fn history_changed() {
    if ui::main_app().is_some_and(|app| app.get_view() == 4) {
        ui::send(Job::Action(Action::ListHistory));
    }
}

fn restore(entry: Entry, undo: bool) {
    let action = Action::Restore { path: entry.path.clone(), undo };
    if !undo || entry.machine {
        let extra = if entry.machine { " It includes a system-wide setting, so Windows will ask for admin permission." } else { "" };
        actions::confirm(
            action,
            if undo { "Undo this change?" } else { "Restore this backup?" },
            &format!("This puts back the settings saved {}: \"{}\". The current state is backed up first.{extra}", entry.created, entry.summary),
            if undo { "Undo" } else { "Restore" },
            entry.machine,
        );
    } else {
        actions::submit(action, "Undoing…");
    }
}

pub fn wire(app: &ui::AppWindow) {
    ISSUE_MODEL.with(|m| app.set_issues(ModelRc::from(m.clone())));
    HISTORY_MODEL.with(|m| app.set_history(ModelRc::from(m.clone())));
    app.on_show_repair(|| {
        ui::set_view_repair();
        run_health();
    });
    app.on_run_health(run_health);
    app.on_fix_issue(|i| {
        if let Some(issue) = ISSUES.with(|s| s.borrow().get(i as usize).cloned()) {
            fix(vec![issue]);
        }
    });
    app.on_fix_all(|| fix(ISSUES.with(|s| s.borrow().clone())));
    app.on_show_history(|| {
        ui::set_view_history();
        ui::send(Job::Action(Action::ListHistory));
    });
    app.on_undo_last(|| {
        match HISTORY.with(|h| h.borrow().iter().find(|e| !e.undone).cloned()) {
            Some(e) => restore(e, true),
            None => ui::toast("Nothing to undo.", 2, false, false),
        }
    });
    app.on_restore(|i| {
        if let Some(e) = HISTORY.with(|h| h.borrow().get(i as usize).cloned()) {
            restore(e, false);
        }
    });
    app.on_open_backups(|| {
        let _ = shell::open_uri(&crate::backup::dir().to_string_lossy());
    });
}
