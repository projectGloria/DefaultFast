//! Slint glue. The UI thread owns `State` (thread-local) and never touches the registry or
//! disk: scans, Shell queries, icon extraction and every write run on background threads and
//! report back through `slint::invoke_from_event_loop`.
use crate::{
    discovery,
    model::{is_protocol, Index},
    registry::{Hive, Registry, WinRegistry},
    search, shell,
    shell::IconSource,
    window::{self, ChromeEvent},
    worker::{self, Candidate, Job, Reply},
};
use slint::{ComponentHandle, Image, Model, ModelNotify, ModelRc, ModelTracker, Rgba8Pixel, SharedPixelBuffer, Timer, TimerMode};
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet, VecDeque},
    rc::Rc,
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc, Arc,
    },
    time::{Duration, Instant},
};

slint::include_modules!();

/// Re-index when the window regains focus after this long (spec: 30 s).
const STALE_AFTER: Duration = Duration::from_secs(30);
const ICON_CACHE_MAX: usize = 400;

// ------------------------------------------------------------------------------------------
// Lazily iconed list models
// ------------------------------------------------------------------------------------------

pub trait Iconic: Clone + 'static {
    fn has_icon(&self) -> bool;
    fn set_icon(&mut self, img: Image);
}
macro_rules! iconic {
    ($($t:ty),*) => {$(
        impl Iconic for $t {
            fn has_icon(&self) -> bool { self.has_icon }
            fn set_icon(&mut self, img: Image) { self.icon = img; self.has_icon = true; }
        }
    )*};
}
iconic!(ResultRow, SupportRow, HandlerRow);

/// A Vec-backed model whose rows request their icon the first time Slint asks for the row.
/// ListView only instantiates visible rows, so only visible rows ever cost an icon load.
pub struct IconModel<T: Iconic> {
    rows: RefCell<Vec<(T, Option<IconSource>)>>,
    notify: ModelNotify,
}

impl<T: Iconic> IconModel<T> {
    fn new() -> Rc<Self> {
        Rc::new(Self { rows: RefCell::new(Vec::new()), notify: ModelNotify::default() })
    }
    fn set(&self, rows: Vec<(T, Option<IconSource>)>) {
        *self.rows.borrow_mut() = rows;
        self.notify.reset();
    }
    fn update(&self, i: usize, f: impl FnOnce(&mut T)) {
        if let Some(r) = self.rows.borrow_mut().get_mut(i) {
            f(&mut r.0);
        } else {
            return;
        }
        self.notify.row_changed(i);
    }
    fn push(&self, row: T, src: Option<IconSource>) {
        let i = {
            let mut rows = self.rows.borrow_mut();
            rows.push((row, src));
            rows.len() - 1
        };
        self.notify.row_added(i, 1);
    }
    fn get(&self, i: usize) -> Option<T> {
        self.rows.borrow().get(i).map(|r| r.0.clone())
    }
    fn len(&self) -> usize {
        self.rows.borrow().len()
    }
    fn icon_arrived(&self, src: &IconSource, img: &Image) {
        let changed: Vec<usize> = {
            let mut rows = self.rows.borrow_mut();
            rows.iter_mut()
                .enumerate()
                .filter(|(_, r)| r.1.as_ref() == Some(src) && !r.0.has_icon())
                .map(|(i, r)| {
                    r.0.set_icon(img.clone());
                    i
                })
                .collect()
        };
        for i in changed {
            self.notify.row_changed(i);
        }
    }
}

impl<T: Iconic> Model for IconModel<T> {
    type Data = T;
    fn row_count(&self) -> usize {
        self.rows.borrow().len()
    }
    fn row_data(&self, row: usize) -> Option<T> {
        let mut rows = self.rows.borrow_mut();
        let r = rows.get_mut(row)?;
        if !r.0.has_icon() {
            if let Some(src) = &r.1 {
                // Cached: fill in place (no notify needed, we return the filled row).
                if let Some(img) = icon_cached(src) {
                    r.0.set_icon(img);
                } else {
                    request_icon(src.clone());
                }
            }
        }
        Some(r.0.clone())
    }
    fn model_tracker(&self) -> &dyn ModelTracker {
        &self.notify
    }
}

struct Models {
    results: Rc<IconModel<ResultRow>>,
    supports: Rc<IconModel<SupportRow>>,
    handlers: Rc<IconModel<HandlerRow>>,
}

#[derive(Default)]
struct IconCache {
    images: HashMap<IconSource, Image>,
    order: VecDeque<IconSource>,
    requested: HashSet<IconSource>,
    tx: Option<mpsc::Sender<(IconSource, u32)>>,
    px: u32,
}

thread_local! {
    static MODELS: Models = Models { results: IconModel::new(), supports: IconModel::new(), handlers: IconModel::new() };
    static ICONS: RefCell<IconCache> = RefCell::new(IconCache::default());
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
    static TIMERS: Timers = Timers::default();
}

#[derive(Default)]
struct Timers {
    preview: Timer,
    toast: Timer,
}

fn icon_cached(src: &IconSource) -> Option<Image> {
    ICONS.with(|c| c.borrow().images.get(src).cloned())
}

fn request_icon(src: IconSource) {
    ICONS.with(|c| {
        let mut c = c.borrow_mut();
        if c.requested.insert(src.clone()) {
            c.order.push_back(src.clone());
            while c.order.len() > ICON_CACHE_MAX {
                if let Some(old) = c.order.pop_front() {
                    c.images.remove(&old);
                    c.requested.remove(&old);
                }
            }
            let px = c.px;
            if let Some(tx) = &c.tx {
                let _ = tx.send((src, px));
            }
        }
    });
}

fn icon_loaded(src: IconSource, w: u32, h: u32, px: Vec<u8>) {
    let buf = SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(&px, w, h);
    let img = Image::from_rgba8_premultiplied(buf);
    ICONS.with(|c| {
        let mut c = c.borrow_mut();
        c.images.insert(src.clone(), img.clone());
    });
    MODELS.with(|m| {
        m.results.icon_arrived(&src, &img);
        m.supports.icon_arrived(&src, &img);
        m.handlers.icon_arrived(&src, &img);
    });
    with_state(|s, app| {
        if s.detail_icon.as_ref() == Some(&src) {
            app.set_d_icon(img.clone());
            app.set_d_has_icon(true);
        }
    });
}

fn icon_thread(rx: mpsc::Receiver<(IconSource, u32)>) {
    std::thread::Builder::new()
        .name("icons".into())
        .spawn(move || {
            let _com = shell::Com::init();
            for (src, px) in rx {
                if let Some((w, h, pixels)) = shell::load_icon(&src, px) {
                    let _ = slint::invoke_from_event_loop(move || icon_loaded(src, w, h, pixels));
                }
            }
        })
        .expect("spawn icon thread");
}

// ------------------------------------------------------------------------------------------
// State
// ------------------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
enum RowRef {
    Header,
    Program(String),
    Target(String),
}

#[derive(Clone)]
struct SupportState {
    target: String,
    prog_id: String,
}

enum View {
    Welcome,
    Program { key: String, rows: Vec<SupportState> },
    Target { target: String, candidates: Vec<Candidate> },
    Repair,
    History,
}

pub(crate) struct State {
    app: slint::Weak<AppWindow>,
    index: Arc<Index>,
    custom_supports: HashMap<String, Vec<SupportState>>,
    results: search::Results,
    rows: Vec<RowRef>,
    view: View,
    detail_seq: Arc<AtomicU64>,
    detail_icon: Option<IconSource>,
    last_index: Instant,
    scanning: bool,
    jobs: mpsc::Sender<Job>,
}

impl State {
    fn next_seq(&self) -> u64 {
        self.detail_seq.fetch_add(1, Ordering::Relaxed) + 1
    }
}

fn with_state<R>(f: impl FnOnce(&mut State, &AppWindow) -> R) -> Option<R> {
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        let s = s.as_mut()?;
        let app = s.app.upgrade()?;
        Some(f(s, &app))
    })
}

// ------------------------------------------------------------------------------------------
// Startup
// ------------------------------------------------------------------------------------------

fn os_build(reg: &dyn Registry) -> u32 {
    reg.string(Hive::LocalMachine, "SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion", "CurrentBuildNumber")
        .and_then(|b| b.parse().ok())
        .unwrap_or(0)
}

/// Accent colors as Windows 11 uses them: AccentPalette[4] (Dark1) on light backgrounds,
/// AccentPalette[1] (Light2) on dark backgrounds.
fn set_accent_colors(app: &AppWindow, accent: Option<([u8; 3], [u8; 3])>) {
    if let Some((light, dark)) = accent {
        let theme = app.global::<Theme>();
        theme.set_accent_light(slint::Color::from_rgb_u8(light[0], light[1], light[2]).into());
        theme.set_accent_dark(slint::Color::from_rgb_u8(dark[0], dark[1], dark[2]).into());
    }
}

fn placement_path() -> std::path::PathBuf {
    crate::logfile::data_dir().join("window.txt")
}

/// Restores the saved outer position + client size (physical px), compensating for the
/// caption height winit adds and our WM_NCCALCSIZE removes. Returns the monitor DPI scale.
fn restore_placement(app: &AppWindow) -> f32 {
    let saved = std::fs::read_to_string(placement_path()).unwrap_or_default();
    let v: Vec<i32> = saved.split_whitespace().filter_map(|x| x.parse().ok()).collect();
    if let [x, y, w, h, max] = v[..] {
        if w >= 400 && h >= 300 && window::rect_visible(x, y, w, h) {
            app.window().set_position(slint::PhysicalPosition::new(x, y));
            let extra = window::top_extra_px(x, y);
            app.window().set_size(slint::PhysicalSize::new(w as u32, (h - extra).max(200) as u32));
            if max != 0 {
                app.window().set_maximized(true);
            }
            return window::scale_at(x, y);
        }
    }
    // First run: 760x560 logical client area, centered by Windows.
    let scale = window::scale_at(0, 0);
    let extra = window::top_extra_px(0, 0) as f32 / scale;
    app.window().set_size(slint::LogicalSize::new(760.0, 560.0 - extra));
    scale
}

fn save_placement(app: &AppWindow) {
    let w = app.window();
    let max = w.is_maximized();
    let (p, s) = (w.position(), w.size());
    std::thread::spawn(move || {
        let path = placement_path();
        let old = std::fs::read_to_string(&path).unwrap_or_default();
        let line = if max {
            // Keep the last normal rectangle; only flip the maximized flag.
            let v: Vec<&str> = old.split_whitespace().collect();
            if v.len() == 5 {
                format!("{} {} {} {} 1", v[0], v[1], v[2], v[3])
            } else {
                let scale = window::scale_at(p.x, p.y);
                let (nw, nh) = ((760.0 * scale) as i32, (560.0 * scale) as i32);
                format!("{} {} {nw} {nh} 1", p.x.max(0), p.y.max(0))
            }
        } else {
            format!("{} {} {} {} 0", p.x, p.y, s.width, s.height)
        };
        let _ = std::fs::write(path, line);
    });
}

pub fn run(probe: bool, query: Option<String>) -> Result<(), slint::PlatformError> {
    slint::BackendSelector::new().backend_name("winit".into()).renderer_name("software".into()).select()?;
    let app = AppWindow::new()?;
    let reg = Arc::new(WinRegistry);
    let dark = worker::system_dark(reg.as_ref());

    app.set_version(env!("CARGO_PKG_VERSION").into());
    app.global::<Theme>().set_icon_font(if os_build(reg.as_ref()) >= 22000 { "Segoe Fluent Icons" } else { "Segoe MDL2 Assets" }.into());
    set_accent_colors(&app, worker::accent_colors(reg.as_ref()));
    app.set_results_message("Start typing. Try .pdf, vlc or https.".into());
    MODELS.with(|m| {
        app.set_results(ModelRc::from(m.results.clone()));
        app.set_supports(ModelRc::from(m.supports.clone()));
        app.set_handlers(ModelRc::from(m.handlers.clone()));
    });

    // Window chrome: subclass on creation; window events are deferred into the event loop.
    window::install("DefaultFast", dark, |e| {
        let _ = slint::invoke_from_event_loop(move || on_chrome(e));
    });
    let scale = restore_placement(&app);

    // Threads: icons, worker (Shell queries + all registry writes, serialized).
    let (icon_tx, icon_rx) = mpsc::channel();
    icon_thread(icon_rx);
    ICONS.with(|c| {
        let mut c = c.borrow_mut();
        c.tx = Some(icon_tx);
        c.px = if scale > 1.4 { 48 } else if scale > 1.1 { 32 } else { 24 };
    });
    let detail_seq = Arc::new(AtomicU64::new(0));
    let jobs = worker::spawn(reg, detail_seq.clone(), |reply| {
        let _ = slint::invoke_from_event_loop(move || on_reply(reply));
    });

    STATE.with(|s| {
        *s.borrow_mut() = Some(State {
            app: app.as_weak(),
            index: Arc::new(Index::default()),
            custom_supports: HashMap::new(),
            results: search::Results::default(),
            rows: Vec::new(),
            view: View::Welcome,
            detail_seq,
            detail_icon: None,
            last_index: Instant::now(),
            scanning: false,
            jobs,
        })
    });

    wire_callbacks(&app);
    app.window().on_close_requested({
        let weak = app.as_weak();
        move || {
            if let Some(app) = weak.upgrade() {
                save_placement(&app);
            }
            slint::CloseRequestResponse::HideWindow
        }
    });

    start_scan();
    send(Job::Health);

    // The window exists once the event loop has run: drop the CBT hook, report startup.
    let weak = app.as_weak();
    Timer::single_shot(Duration::ZERO, move || {
        window::ready();
        if let Some(app) = weak.upgrade() {
            app.set_is_max(window::is_maximized());
            app.invoke_focus_search();
            match query.as_deref() {
                Some("--repair") => app.invoke_show_repair(),
                Some(q) => app.invoke_example(q.into()),
                None => {}
            }
        }
        if probe {
            crate::probe::first_frame();
        }
    });
    if probe {
        crate::probe::arm();
    }
    app.run()
}

pub(crate) fn send(job: Job) {
    with_state(|s, _| {
        let _ = s.jobs.send(job);
    });
}

pub(crate) fn index_complete() -> bool {
    with_state(|s, _| s.index.complete && !s.scanning).unwrap_or(false)
}

fn on_chrome(e: ChromeEvent) {
    with_state(|s, app| match e {
        ChromeEvent::MaxHover(h) => app.set_max_hover(h),
        ChromeEvent::Maximized(m) => app.set_is_max(m),
        ChromeEvent::ThemeChanged => {
            let _ = s.jobs.send(Job::Theme);
        }
        ChromeEvent::SettingChanged => {
            s.last_index = Instant::now() - STALE_AFTER;
        }
        ChromeEvent::Activated => {}
    });
    // Spec: invalidate on WM_SETTINGCHANGE or on focus after >= 30 s.
    if matches!(e, ChromeEvent::SettingChanged | ChromeEvent::Activated) {
        let stale = with_state(|s, _| s.last_index.elapsed() >= STALE_AFTER && !s.scanning).unwrap_or(false);
        if stale {
            start_scan();
        }
    }
}

// ------------------------------------------------------------------------------------------
// Indexing and search
// ------------------------------------------------------------------------------------------

fn start_scan() {
    let go = with_state(|s, _| !std::mem::replace(&mut s.scanning, true)).unwrap_or(false);
    if !go {
        return;
    }
    std::thread::Builder::new()
        .name("scan".into())
        .spawn(|| {
            let _com = shell::Com::init();
            let reg = WinRegistry;
            let never = || false;
            let (file_types, protocols) = discovery::scan_types(&reg, &never);
            let programs = discovery::scan_programs(&reg, &file_types, &never);
            let mut index = Index { programs, file_types, protocols, complete: false };
            discovery::sort(&mut index);
            let first = index.clone();
            let _ = slint::invoke_from_event_loop(move || on_index(first));
            discovery::scan_shell(&mut index.programs, &never);
            discovery::sort(&mut index);
            index.complete = true;
            let _ = slint::invoke_from_event_loop(move || on_index(index));
        })
        .expect("spawn scan thread");
}

fn on_index(mut index: Index) {
    let complete = index.complete;
    with_state(|s, app| {
        for (key, extras) in &s.custom_supports {
            if let Some(p) = index.programs.iter_mut().find(|p| p.key() == *key) {
                for ex in extras {
                    p.add_support(&ex.target, &ex.prog_id);
                }
            }
        }
        s.index = Arc::new(index);
        if complete {
            s.scanning = false;
            // The one-off Shell scan pulled in DLL pages the idle UI never touches again.
            shell::trim_working_set();
            s.last_index = Instant::now();
            app.set_status_text(format!("{} programs · {} file types · {} protocols", s.index.programs.len(), s.index.file_types.len(), s.index.protocols.len()).into());
        } else {
            app.set_status_text("Finding programs…".into());
        }
        let keep = app.get_selected();
        let keep = usize::try_from(keep).ok().and_then(|i| s.rows.get(i).cloned());
        refresh_results(s, app, keep);
        // A query typed (or passed on the command line) before the index existed: preview
        // its top hit now that there are results.
        if matches!(s.view, View::Welcome) && !app.get_query().is_empty() {
            open_row(s, app, app.get_selected());
        }
    });
}

fn group_title(k: search::Kind) -> &'static str {
    match k {
        search::Kind::Program => "Programs",
        search::Kind::FileType => "File types",
        search::Kind::Protocol => "Protocols",
    }
}

const HEADER_H: f32 = 30.0;
const ITEM_H: f32 = 46.0;

fn row_y(rows: &[RowRef], i: usize) -> f32 {
    rows.get(..i).unwrap_or(rows).iter().map(|r| if *r == RowRef::Header { HEADER_H } else { ITEM_H }).sum()
}

fn set_selected(s: &State, app: &AppWindow, i: i32) {
    app.set_selected(i);
    if i >= 0 {
        app.set_selected_y(row_y(&s.rows, i as usize));
        app.set_selected_h(ITEM_H);
        app.set_scroll_results(app.get_scroll_results().wrapping_add(1));
    }
}

/// Re-runs the current query against the current index. `keep` preserves the selection
/// across index refreshes.
fn refresh_results(s: &mut State, app: &AppWindow, keep: Option<RowRef>) {
    let query = app.get_query().to_string();
    search::run(&s.index, &query, &mut s.results);
    let mut rows: Vec<(ResultRow, Option<IconSource>)> = Vec::new();
    let mut refs = Vec::new();
    for kind in search::group_order(&s.results) {
        let hits = match kind {
            search::Kind::Program => &s.results.programs,
            search::Kind::FileType => &s.results.file_types,
            search::Kind::Protocol => &s.results.protocols,
        };
        if hits.is_empty() {
            continue;
        }
        rows.push((ResultRow { kind: 0, title: group_title(kind).into(), ..Default::default() }, None));
        refs.push(RowRef::Header);
        for &(i, _) in hits {
            match kind {
                search::Kind::Program => {
                    let p = &s.index.programs[i];
                    let subtitle = if !p.supports.is_empty() {
                        format!("Can open {} type{}", p.supports.len(), if p.supports.len() == 1 { "" } else { "s" })
                    } else if p.exe.is_empty() {
                        "Microsoft Store app".to_owned()
                    } else {
                        p.exe.clone()
                    };
                    let src = if !p.exe.is_empty() { IconSource::File(p.exe.clone()) } else { IconSource::Aumid(p.aumid.clone()) };
                    rows.push((ResultRow { kind: 1, title: p.name.as_str().into(), subtitle: subtitle.into(), ..Default::default() }, Some(src)));
                    refs.push(RowRef::Program(p.key()));
                }
                search::Kind::FileType => {
                    let t = &s.index.file_types[i];
                    rows.push((ResultRow { kind: 2, title: t.ext.as_str().into(), subtitle: t.friendly.as_str().into(), ..Default::default() }, Some(IconSource::Extension(t.ext.clone()))));
                    refs.push(RowRef::Target(t.ext.clone()));
                }
                search::Kind::Protocol => {
                    let p = &s.index.protocols[i];
                    rows.push((ResultRow { kind: 3, title: p.name.as_str().into(), subtitle: p.friendly.as_str().into(), ..Default::default() }, None));
                    refs.push(RowRef::Target(p.name.clone()));
                }
            }
        }
    }
    let message = if query.trim().is_empty() {
        "Start typing. Try .pdf, vlc or https.".to_owned()
    } else if !s.index.complete && rows.is_empty() {
        format!("Still looking… nothing named '{}' yet.", query.trim())
    } else {
        format!("Nothing named '{}'. Try an extension like .pdf, or a protocol like https.", query.trim())
    };
    app.set_results_message(message.into());
    let sel = keep
        .and_then(|k| refs.iter().position(|r| *r == k))
        .or_else(|| refs.iter().position(|r| *r != RowRef::Header));
    s.rows = refs;
    MODELS.with(|m| m.results.set(rows));
    set_selected(s, app, sel.map(|i| i as i32).unwrap_or(-1));
}

/// Shows the selected row in the detail pane after a short pause in typing, so fast typing
/// doesn't queue Shell queries for every intermediate result.
fn schedule_preview() {
    TIMERS.with(|t| {
        t.preview.start(TimerMode::SingleShot, Duration::from_millis(120), || {
            with_state(|s, app| open_row(s, app, app.get_selected()));
        })
    });
}

// ------------------------------------------------------------------------------------------
// Detail views
// ------------------------------------------------------------------------------------------

fn open_row(s: &mut State, app: &AppWindow, i: i32) {
    let Some(r) = usize::try_from(i).ok().and_then(|i| s.rows.get(i).cloned()) else { return };
    match r {
        RowRef::Header => {}
        RowRef::Program(key) => open_program(s, app, &key),
        RowRef::Target(t) => open_target(s, app, &t),
    }
}

fn set_detail_icon(s: &mut State, app: &AppWindow, src: Option<IconSource>) {
    s.detail_icon = src.clone();
    match src.as_ref().and_then(icon_cached) {
        Some(img) => {
            app.set_d_icon(img);
            app.set_d_has_icon(true);
        }
        None => {
            app.set_d_has_icon(false);
            if let Some(src) = src {
                request_icon(src);
            }
        }
    }
}

fn open_program(s: &mut State, app: &AppWindow, key: &str) {
    if matches!(&s.view, View::Program { key: k, .. } if k == key) {
        return;
    }
    let Some(p) = s.index.program_by_key(key).cloned() else { return };
    app.set_d_title(p.name.as_str().into());
    app.set_d_subtitle(if p.exe.is_empty() { "Microsoft Store app".into() } else { p.exe.as_str().into() });
    set_detail_icon(s, app, Some(if p.exe.is_empty() { IconSource::Aumid(p.aumid.clone()) } else { IconSource::File(p.exe.clone()) }));
    let mut supports = p.supports.clone();
    supports.sort_by(|a, b| (is_protocol(&a.target), &a.target).cmp(&(is_protocol(&b.target), &b.target)));
    let rows: Vec<(SupportRow, Option<IconSource>)> = supports
        .iter()
        .map(|sp| {
            let src = (!is_protocol(&sp.target)).then(|| IconSource::Extension(sp.target.clone()));
            (SupportRow { target: sp.target.as_str().into(), current: "…".into(), ..Default::default() }, src)
        })
        .collect();
    MODELS.with(|m| m.supports.set(rows));
    app.set_support_selected(0);
    app.set_checked_count(0);
    app.set_program_note(if supports.is_empty() {
        if p.exe.is_empty() {
            format!("{} doesn't declare any file types, and Store apps can only open types they declare. Use Windows Settings for it.", p.name)
        } else {
            format!("{} doesn't declare any file types. Add one below (for example .txt), or search for a file type and choose Browse.", p.name)
        }
    } else {
        String::new()
    }.into());
    let targets: Vec<(String, String)> = supports.iter().map(|x| (x.target.clone(), x.prog_id.clone())).collect();
    let seq = s.next_seq();
    s.view = View::Program { key: key.to_owned(), rows: supports.iter().map(|x| SupportState { target: x.target.clone(), prog_id: x.prog_id.clone() }).collect() };
    app.set_view(1);
    let _ = s.jobs.send(Job::ProgramDefaults { seq, key: key.to_owned(), targets, exe: p.exe.clone() });
}

fn open_target(s: &mut State, app: &AppWindow, target: &str) {
    if matches!(&s.view, View::Target { target: t, .. } if t == target) {
        return;
    }
    let proto = is_protocol(target);
    let friendly = if proto {
        s.index.protocols.iter().find(|p| p.name == target).map(|p| p.friendly.clone())
    } else {
        s.index.file_types.iter().find(|t| t.ext == target).map(|t| t.friendly.clone())
    }
    .unwrap_or_default();
    app.set_d_title(target.into());
    app.set_d_subtitle(friendly.into());
    app.set_d_glyph(if proto { "\u{E774}" } else { "\u{E8A5}" }.into());
    set_detail_icon(s, app, (!proto).then(|| IconSource::Extension(target.to_owned())));
    app.set_current_default("Checking…".into());
    app.set_can_browse(true);
    MODELS.with(|m| m.handlers.set(Vec::new()));
    app.set_handler_selected(0);
    let seq = s.next_seq();
    s.view = View::Target { target: target.to_owned(), candidates: Vec::new() };
    app.set_view(2);
    let _ = s.jobs.send(Job::TargetInfo { seq, target: target.to_owned(), index: s.index.clone() });
}

fn on_reply(reply: Reply) {
    match reply {
        Reply::ProgramDefaults { key, rows } => {
            with_state(|s, app| {
                let View::Program { key: k, rows: states } = &s.view else { return };
                if *k != key {
                    return;
                }
                MODELS.with(|m| {
                    for (target, current, is_current) in rows {
                        if let Some(i) = states.iter().position(|r| r.target == target) {
                            m.supports.update(i, |r| {
                                r.current = current.into();
                                r.is_current = is_current;
                            });
                        }
                    }
                });
                update_checked(app);
            });
        }
        Reply::TargetInfo { target, current, candidates } => {
            with_state(|s, app| {
                let View::Target { target: t, candidates: c } = &mut s.view else { return };
                if *t != target {
                    return;
                }
                app.set_current_default(current.into());
                let rows = candidates
                    .iter()
                    .map(|c| {
                        let src = if !c.exe.is_empty() { Some(IconSource::File(c.exe.clone())) } else if !c.aumid.is_empty() { Some(IconSource::Aumid(c.aumid.clone())) } else { None };
                        let detail = if c.exe.is_empty() { "Microsoft Store app".to_owned() } else { c.exe.clone() };
                        (HandlerRow { name: c.name.as_str().into(), detail: detail.into(), is_current: c.is_current, recommended: c.recommended, ..Default::default() }, src)
                    })
                    .collect();
                *c = candidates;
                MODELS.with(|m| m.handlers.set(rows));
                // Start keyboard selection on the first app that isn't already the default.
                let first = c.iter().position(|x| !x.is_current).unwrap_or(0);
                app.set_handler_selected(first as i32);
            });
        }
        Reply::Theme { dark, accent } => {
            window::set_dark(dark);
            if let Some(app) = main_app() {
                set_accent_colors(&app, accent);
            }
        }
        other => crate::actions::on_reply(other),
    }
}

fn update_checked(app: &AppWindow) {
    let n = MODELS.with(|m| (0..m.supports.len()).filter(|&i| m.supports.get(i).is_some_and(|r| r.checked)).count());
    app.set_checked_count(n as i32);
}

/// Checked (target, prog_id) pairs of the open program view.
pub(crate) fn checked_supports(all: bool) -> Option<(String, Vec<(String, String)>)> {
    with_state(|s, _| {
        let View::Program { key, rows } = &s.view else { return None };
        let picked = MODELS.with(|m| {
            rows.iter()
                .enumerate()
                .filter(|(i, _)| all || m.supports.get(*i).is_some_and(|r| r.checked))
                .map(|(_, r)| (r.target.clone(), r.prog_id.clone()))
                .collect::<Vec<_>>()
        });
        Some((key.clone(), picked))
    })
    .flatten()
}

pub(crate) fn program(key: &str) -> Option<crate::model::Program> {
    with_state(|s, _| s.index.program_by_key(key).cloned()).flatten()
}

pub(crate) fn current_target() -> Option<(String, Vec<Candidate>)> {
    with_state(|s, _| match &s.view {
        View::Target { target, candidates } => Some((target.clone(), candidates.clone())),
        _ => None,
    })
    .flatten()
}

/// Re-queries whatever detail view is open (after a change).
pub(crate) fn refresh_detail() {
    with_state(|s, app| match &s.view {
        View::Program { key, .. } => {
            let key = key.clone();
            s.view = View::Welcome;
            open_program(s, app, &key);
        }
        View::Target { target, .. } => {
            let t = target.clone();
            s.view = View::Welcome;
            open_target(s, app, &t);
        }
        _ => {}
    });
}

pub(crate) fn set_view_repair() {
    with_state(|s, app| {
        s.next_seq();
        s.view = View::Repair;
        app.set_view(3);
    });
}

pub(crate) fn set_view_history() {
    with_state(|s, app| {
        s.next_seq();
        s.view = View::History;
        app.set_view(4);
    });
}

pub(crate) fn main_app() -> Option<AppWindow> {
    STATE.with(|s| s.borrow().as_ref().and_then(|s| s.app.upgrade()))
}

/// Toast with optional Undo (restores `undo_backup`) and Settings (`settings_uri`) buttons.
pub(crate) fn toast(text: &str, kind: i32, undo: bool, settings: bool) {
    let Some(app) = main_app() else { return };
    app.set_toast_text(text.into());
    app.set_toast_kind(kind);
    app.set_toast_undo(undo);
    app.set_toast_settings(settings);
    app.set_toast_visible(true);
    let weak = app.as_weak();
    // Errors and Settings hand-offs stay until dismissed; successes fade after 8 s.
    TIMERS.with(|t| {
        if kind == 0 && !settings {
            t.toast.start(TimerMode::SingleShot, Duration::from_secs(8), move || {
                if let Some(a) = weak.upgrade() {
                    a.set_toast_visible(false);
                }
            });
        } else {
            t.toast.stop();
        }
    });
}

// ------------------------------------------------------------------------------------------
// Callbacks
// ------------------------------------------------------------------------------------------

fn wire_callbacks(app: &AppWindow) {
    app.on_query_edited(|_| {
        with_state(|s, app| refresh_results(s, app, None));
        schedule_preview();
    });
    app.on_example(|q| {
        with_state(|s, app| {
            app.set_query(q);
            refresh_results(s, app, None);
            open_row(s, app, app.get_selected());
            app.invoke_focus_search();
        });
    });
    app.on_move_selection(|delta| {
        with_state(|s, app| {
            if s.rows.is_empty() {
                return;
            }
            let n = s.rows.len() as i32;
            let mut i = app.get_selected();
            loop {
                i += delta;
                if i < 0 || i >= n {
                    return; // stay on the first/last item
                }
                if s.rows[i as usize] != RowRef::Header {
                    break;
                }
            }
            set_selected(s, app, i);
        });
        schedule_preview();
    });
    app.on_activate(|i| {
        with_state(|s, app| {
            set_selected(s, app, i);
            open_row(s, app, i);
        });
    });
    app.on_open_selected(|| {
        with_state(|s, app| open_row(s, app, app.get_selected()));
        // Next turn, once the detail view exists: move focus into its list (Space/arrows).
        Timer::single_shot(Duration::ZERO, || {
            if let Some(app) = main_app() {
                app.set_detail_focus(app.get_detail_focus() + 1);
            }
        });
    });
    app.on_escape_pressed(|| {
        with_state(|s, app| {
            if app.get_toast_visible() {
                app.set_toast_visible(false);
            } else if !app.get_query().is_empty() {
                app.set_query("".into());
                refresh_results(s, app, None);
                s.next_seq();
                s.view = View::Welcome;
                app.set_view(0);
            } else {
                s.next_seq();
                s.view = View::Welcome;
                app.set_view(0);
            }
        });
    });
    app.on_toggle_support(|i| {
        let Ok(i) = usize::try_from(i) else { return };
        let Some(app) = main_app() else { return };
        MODELS.with(|m| m.supports.update(i, |r| r.checked = !r.checked));
        update_checked(&app);
    });
    app.on_select_all(|| {
        let Some(app) = main_app() else { return };
        MODELS.with(|m| (0..m.supports.len()).for_each(|i| m.supports.update(i, |r| r.checked = !r.is_current)));
        update_checked(&app);
    });
    app.on_select_none(|| {
        let Some(app) = main_app() else { return };
        MODELS.with(|m| (0..m.supports.len()).for_each(|i| m.supports.update(i, |r| r.checked = false)));
        update_checked(&app);
    });
    app.on_add_target(|text| add_target(&text));
    app.on_minimize(window::minimize);
    app.on_toggle_maximize(window::toggle_maximize);
    app.on_close_window(window::close);
    app.on_system_menu(window::show_system_menu);
    app.on_toast_dismiss(|| {
        if let Some(app) = main_app() {
            app.set_toast_visible(false);
        }
    });
    crate::actions::wire(app);
}

fn add_target(text: &str) {
    // `toast` re-borrows STATE, so decide inside the borrow and toast after it.
    let msg = with_state(|s, app| -> Option<&'static str> {
        let View::Program { key, rows } = &mut s.view else { return None };
        let Some(p) = s.index.program_by_key(key).cloned() else { return None };
        let raw = text.trim().to_lowercase();
        if raw.is_empty() {
            return None;
        }
        let target = if raw.starts_with('.') || !s.index.protocols.iter().any(|x| x.name == raw) {
            match discovery::normalize_extension(&raw) {
                Some(t) => t,
                None => return Some("Enter a valid file extension (for example .txt)."),
            }
        } else {
            raw
        };
        if p.exe.is_empty() {
            return Some("Store apps can only open the file types they declare. Use Windows Settings for this app.");
        }
        if let Some(i) = rows.iter().position(|r| r.target == target) {
            MODELS.with(|m| m.supports.update(i, |r| r.checked = true));
            app.set_support_selected(i as i32);
            update_checked(app);
            return None;
        }
        let prog_id = format!("Applications\\{}", p.exe_name());
        let state = SupportState { target: target.clone(), prog_id: prog_id.clone() };
        rows.push(state.clone());
        s.custom_supports.entry(key.clone()).or_default().push(state);
        if let Some(prog) = Arc::make_mut(&mut s.index).programs.iter_mut().find(|x| x.key() == *key) {
            prog.add_support(&target, &prog_id);
        }
        let src = (!is_protocol(&target)).then(|| IconSource::Extension(target.clone()));
        MODELS.with(|m| {
            m.supports.push(SupportRow { target: target.as_str().into(), current: "…".into(), checked: true, ..Default::default() }, src);
            app.set_support_selected(m.supports.len() as i32 - 1);
        });
        app.set_program_note("".into());
        update_checked(app);
        let seq = s.detail_seq.load(Ordering::Relaxed);
        let _ = s.jobs.send(Job::ProgramDefaults { seq, key: key.clone(), targets: vec![(target, prog_id)], exe: p.exe });
        None
    })
    .flatten();
    if let Some(m) = msg {
        toast(m, 1, false, false);
    }
}

/// RegisteredApplications name for the Settings deep link of a candidate app.
pub(crate) fn registered_for(exe: &str, aumid: &str) -> Option<(String, bool)> {
    with_state(|s, _| {
        s.index
            .programs
            .iter()
            .find(|p| (!exe.is_empty() && p.exe.eq_ignore_ascii_case(exe)) || (!aumid.is_empty() && p.aumid == aumid))
            .map(|p| (p.registered_name.clone(), p.registered_machine))
    })
    .flatten()
}
