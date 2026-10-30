//! DefaultFast: set and repair Windows default apps.
//!
//! `unsafe` is denied crate-wide and only re-allowed inside the thin OS wrapper modules
//! (`registry::win`, `shell`, `window`).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
#![deny(unsafe_code)]

mod actions;
mod discovery;
mod error;
mod logfile;
mod model;
mod probe;
mod registry;
mod search;
mod shell;
mod ui;
mod window;
mod worker;

use std::time::Instant;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--diagnose") => diagnose(),
        Some("--startup-probe") => run_ui(true, None),
        // `DefaultFast.exe .pdf` opens with the search pre-filled.
        Some(q) if !q.starts_with("--") => run_ui(false, Some(args.join(" "))),
        _ => run_ui(false, None),
    }
}

fn run_ui(probe: bool, query: Option<String>) {
    if let Err(e) = ui::run(probe, query) {
        logfile::write(&format!("UI failed to start: {e}"));
        std::process::exit(1);
    }
}

/// Builds the index headlessly and writes timings/counts to diagnostics.txt.
fn diagnose() {
    let _com = shell::Com::init();
    let reg = registry::WinRegistry;
    let t0 = Instant::now();
    let (file_types, protocols) = discovery::scan_types(&reg, &|| false);
    let t1 = Instant::now();
    let mut programs = discovery::scan_programs(&reg, &file_types, &|| false);
    let t2 = Instant::now();
    discovery::scan_shell(&mut programs, &|| false);
    let t3 = Instant::now();
    let mut index = model::Index { programs, file_types, protocols, complete: true };
    discovery::sort(&mut index);
    let mut results = search::Results::default();
    let queries = ["v", "vl", "vlc", "notepad", "pdf", ".bat", "http", "chrome", "zzzz"];
    let t4 = Instant::now();
    for q in queries {
        search::run(&index, q, &mut results);
    }
    let per_query = t4.elapsed() / queries.len() as u32;
    let mut report = format!(
        "file_types={} protocols={} programs={} with_supports={}\n\
         scan_types_ms={} scan_programs_ms={} scan_shell_ms={} search_us_per_query={}\n",
        index.file_types.len(),
        index.protocols.len(),
        index.programs.len(),
        index.programs.iter().filter(|p| !p.supports.is_empty()).count(),
        (t1 - t0).as_millis(),
        (t2 - t1).as_millis(),
        (t3 - t2).as_millis(),
        per_query.as_micros(),
    );
    for p in &index.programs {
        report.push_str(&format!("{} | {} | {} | {} targets\n", p.name, p.exe, p.aumid, p.supports.len()));
    }
    let _ = std::fs::write(logfile::data_dir().join("diagnostics.txt"), report);
}
