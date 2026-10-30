//! Health check and History views (filled in by the repair milestone).
use crate::ui;

pub fn health_done() {
    if let Some(app) = ui::main_app() {
        app.set_health_state(1);
        app.set_health_text("Health check".into());
    }
}

pub fn history_changed() {}

pub fn wire(app: &ui::AppWindow) {
    app.on_show_repair(ui::set_view_repair);
    app.on_show_history(ui::set_view_history);
}
