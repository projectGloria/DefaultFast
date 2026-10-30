//! `--startup-probe`: measures the budgets on a real launch and exits.
//! - first_frame_ms: process creation (kernel timestamp) -> first event-loop turn after the
//!   first frame was presented (Slint draws before showing the window).
//! - idle: 2 s after the index finished building, window untouched.
//! - after_search: then types "vlc" (search + detail pane + icons) and waits 2 s, which is
//!   the realistic "idle after use" number.
use crate::{logfile, shell, ui};
use slint::{Timer, TimerMode};
use std::{cell::Cell, cell::RefCell, time::Duration};

thread_local! {
    static FIRST: Cell<(u64, usize)> = const { Cell::new((0, 0)) };
    static POLL: Timer = Timer::default();
    static TICKS: Cell<u32> = const { Cell::new(0) };
    static REPORT: RefCell<String> = const { RefCell::new(String::new()) };
}

pub fn first_frame() {
    let us = shell::micros_since_process_start();
    FIRST.with(|f| f.set((us, shell::memory().0)));
}

fn mem_line(label: &str) -> String {
    let (ws, peak, private) = shell::memory();
    format!("{label}_working_set_bytes={ws}\n{label}_private_bytes={private}\npeak_working_set_bytes={peak}\n")
}

pub fn arm() {
    POLL.with(|t| {
        t.start(TimerMode::Repeated, Duration::from_millis(250), || {
            if !ui::index_complete() {
                return;
            }
            let n = TICKS.with(|c| {
                c.set(c.get() + 1);
                c.get()
            });
            match n {
                // 2 s idle after the index completed.
                8 => {
                    let (first_us, ws_first) = FIRST.with(Cell::get);
                    let exe = std::env::current_exe().ok().and_then(|p| std::fs::metadata(p).ok()).map(|m| m.len()).unwrap_or(0);
                    let head = format!("first_frame_ms={:.1}\nfirst_frame_working_set_bytes={ws_first}\nexe_bytes={exe}\n", first_us as f64 / 1000.0);
                    REPORT.with(|r| *r.borrow_mut() = head + &mem_line("idle"));
                    if let Some(app) = ui::main_app() {
                        let t = std::time::Instant::now();
                        app.invoke_example("vlc".into());
                        REPORT.with(|r| r.borrow_mut().push_str(&format!("search_and_render_us={}\n", t.elapsed().as_micros())));
                    }
                }
                // 2 s after the search.
                16 => {
                    REPORT.with(|r| r.borrow_mut().push_str(&mem_line("after_search")));
                    let report = REPORT.with(|r| r.borrow().clone());
                    let _ = std::fs::write(logfile::data_dir().join("startup.txt"), report);
                    let _ = slint::quit_event_loop();
                }
                _ => {}
            }
        });
    });
}
