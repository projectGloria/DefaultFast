#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
#![deny(unsafe_code)]
mod app_scan;
mod assoc;
mod cache;
mod ffi;
mod hash;
mod model;
mod registry_scan;
mod search;
mod ui;

fn main() {
    let start = std::time::Instant::now();
    if std::env::args().any(|a| a == "--benchmark") {
        ui::benchmark();
        return;
    }
    if std::env::args().any(|a| a == "--diagnose") {
        ui::diagnose();
        return;
    }
    if std::env::args().any(|a| a == "--self-test-association") {
        let result = assoc::self_test();
        let report = format!("{result:?}\n");
        let _ = cache::atomic_write("self-test.txt", report.as_bytes());
        if result.is_err() {
            std::process::exit(1);
        }
        return;
    }
    if let Err(e) = ffi::ui::run(start) {
        ffi::ui::fatal(&e);
    }
}
