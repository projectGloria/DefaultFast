//! User actions: what each button does, run on the worker thread, and how the outcome is
//! shown. (Milestone 2: navigation + Settings; writes arrive with the UserChoice writer.)
use crate::{registry::Registry, shell, ui};

pub enum Action {
    Health,
}

pub enum Outcome {
    Health,
}

pub fn run(_reg: &dyn Registry, action: Action) -> Outcome {
    match action {
        Action::Health => Outcome::Health,
    }
}

pub fn on_reply(reply: crate::worker::Reply) {
    if let crate::worker::Reply::Action(Outcome::Health) = reply {
        if let Some(app) = ui::main_app() {
            app.set_health_state(1);
            app.set_health_text("Health check".into());
        }
    }
}

fn not_yet() {
    ui::toast("This action arrives in the next milestone build.", 2, false, false);
}

pub fn wire(app: &ui::AppWindow) {
    app.on_open_settings(|| {
        let _ = shell::open_uri("ms-settings:defaultapps");
    });
    app.on_apply_selected(not_yet);
    app.on_apply_all(not_yet);
    app.on_set_handler(|_| not_yet());
    app.on_browse(not_yet);
    app.on_show_repair(ui::set_view_repair);
    app.on_run_health(not_yet);
    app.on_fix_issue(|_| not_yet());
    app.on_fix_all(not_yet);
    app.on_show_history(ui::set_view_history);
    app.on_undo_last(not_yet);
    app.on_restore(|_| not_yet());
    app.on_open_backups(not_yet);
    app.on_toast_undo_clicked(not_yet);
    app.on_toast_settings_clicked(|| {
        let _ = shell::open_uri("ms-settings:defaultapps");
    });
    app.on_confirm_accept(|| {});
    app.on_confirm_cancel(|| {
        if let Some(app) = ui::main_app() {
            app.set_confirm_visible(false);
        }
    });
}
