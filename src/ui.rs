//! Safe catalog worker and diagnostic entry points. Native controls/rendering are in ffi::ui.
use crate::{
    app_scan, cache, ffi,
    model::{App, FileType},
    registry_scan,
};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc::{self, Receiver, Sender},
    Arc,
};

pub enum Event {
    Cached(Vec<FileType>, Vec<App>),
    Types(Vec<FileType>),
    Apps(Vec<App>),
    Complete(Vec<FileType>, Vec<App>, Option<String>),
    Handlers(String, Vec<App>),
    Result(Result<String, String>),
    Icon(String, Vec<u8>, u32, u32),
}
pub struct Workers {
    pub events: Receiver<Event>,
    pub send: Sender<Event>,
    pub refresh: Sender<()>,
    pub cancel: Arc<AtomicBool>,
    pub icons: Sender<(String, u32)>,
}
pub fn workers(wake: impl Fn() + Send + Sync + 'static) -> Workers {
    let wake = Arc::new(wake);
    let (tx, rx) = mpsc::channel();
    let (refresh, work) = mpsc::channel();
    let (icons, icon_rx) = mpsc::channel::<(String, u32)>();
    let cancel = Arc::new(AtomicBool::new(false));
    let stopped = cancel.clone();
    let sender = tx.clone();
    let notify = wake.clone();
    std::thread::spawn(move || {
        let _com = ffi::ComApartment::new();
        if let Some((types, apps)) = cache::load() {
            let _ = sender.send(Event::Cached(types, apps));
            notify();
        }
        loop {
            let mut types = Vec::new();
            let mut apps = Vec::new();
            registry_scan::scan(
                |batch| {
                    types.extend(batch.iter().cloned());
                    let _ = sender.send(Event::Types(batch));
                    notify();
                },
                || stopped.load(Ordering::Relaxed),
            );
            app_scan::scan(
                |batch| {
                    app_scan::merge(&mut apps, batch.clone());
                    let _ = sender.send(Event::Apps(batch));
                    notify();
                },
                || stopped.load(Ordering::Relaxed),
            );
            if stopped.load(Ordering::Relaxed) {
                break;
            }
            types.sort_by(|a, b| a.extension.cmp(&b.extension));
            let error = cache::save(&types, &apps)
                .err()
                .map(|e| format!("Cache could not be saved: {e}"));
            let _ = sender.send(Event::Complete(types, apps, error));
            notify();
            if work.recv().is_err() {
                break;
            }
            while work.try_recv().is_ok() {}
        }
    });
    let refresh_watch = refresh.clone();
    ffi::watch(cancel.clone(), move || {
        let _ = refresh_watch.send(());
    });
    let sender = tx.clone();
    let notify = wake;
    std::thread::spawn(move || {
        let _com = ffi::ComApartment::new();
        while let Ok((key, dpi)) = icon_rx.recv() {
            if let Some((bytes, w, h)) = ffi::ui::load_icon(&key, dpi) {
                let _ = sender.send(Event::Icon(key, bytes, w, h));
                notify();
            } else {
                let _ = sender.send(Event::Icon(key, Vec::new(), 0, 0));
                notify();
            }
        }
    });
    Workers {
        events: rx,
        send: tx,
        refresh,
        cancel,
        icons,
    }
}
pub fn async_handlers(extension: String, send: Sender<Event>, wake: impl Fn() + Send + 'static) {
    std::thread::spawn(move || {
        let _com = ffi::ComApartment::new();
        let handlers = app_scan::for_type(&extension);
        let _ = send.send(Event::Handlers(extension, handlers));
        wake();
    });
}
pub fn async_change(
    ext: String,
    app: App,
    verb: bool,
    send: Sender<Event>,
    wake: impl Fn() + Send + 'static,
) {
    std::thread::spawn(move || {
        let _com = ffi::ComApartment::new();
        let result = if verb {
            crate::assoc::set_batch_verb(&ext, &app)
        } else {
            crate::assoc::set(&ext, &app)
        };
        let _ = send.send(Event::Result(result));
        wake();
    });
}

pub fn benchmark() {
    let start = std::time::Instant::now();
    let types: Vec<_> = (0..6000)
        .map(|i| format!("file{i} windows batch file .type{i}"))
        .collect();
    let mut out = Vec::with_capacity(types.len());
    let mut samples = Vec::new();
    for query in ["bat", ".bat", "batch", "pdf", "file1", "wbf", "不存在", ""] {
        for _ in 0..100 {
            let t = std::time::Instant::now();
            crate::search::filter(
                types
                    .iter()
                    .enumerate()
                    .map(|(i, t)| (i, t.as_str(), false)),
                query,
                &mut out,
            );
            samples.push(t.elapsed().as_nanos());
        }
    }
    samples.sort_unstable();
    let (working, peak) = ffi::memory();
    let report=format!("6000 entries, 800 filters\np50_us={}\np95_us={}\np99_us={}\nworking_set_bytes={working}\npeak_working_set_bytes={peak}\nharness_ms={}\nStartup and actual paint measurements: launch normally; read startup.txt.\n",samples[400]/1000,samples[760]/1000,samples[792]/1000,start.elapsed().as_millis());
    let _ = cache::atomic_write("benchmark.txt", report.as_bytes());
    println!("{report}");
}
pub fn diagnose() {
    let _com = ffi::ComApartment::new();
    let mut n = 0;
    registry_scan::scan(|b| n += b.len(), || false);
    let sid = ffi::sid();
    let mut report = format!(
        "Registered types: {n}\nSID available: {}\nUCPD running (or conservatively protected): {}\n",
        sid.is_ok(),
        ffi::ucpd_running()
    );
    for ext in [".txt", ".pdf", ".bat", ".cmd"] {
        let path = crate::assoc::choice_path(ext);
        let prog = ffi::reg_string(ffi::Root::User, &path, "ProgId");
        let stored = ffi::reg_string(ffi::Root::User, &path, "Hash");
        let generated = match (&sid, &prog, ffi::reg_time(ffi::Root::User, &path)) {
            (Ok(s), Some(p), Some(t)) => crate::hash::generate(ext, s, p, t).ok(),
            _ => None,
        };
        report.push_str(&format!(
            "{ext}: effective={:?}, existing_legacy_hash_matches={}\n",
            ffi::effective(ext, false),
            stored.is_some() && stored == generated
        ));
    }
    let _ = cache::atomic_write("diagnostics.txt", report.as_bytes());
    println!("{report}");
}
