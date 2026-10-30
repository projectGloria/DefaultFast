//! Native UI boundary. UI state is thread-local and RefCell-guarded: synchronous Win32
//! re-entry never obtains a second mutable reference. Worker wakeups carry no raw pointers.
use super::*;
use crate::{
    app_scan, assoc, cache,
    model::{App, FileType},
    search,
    ui::{self, Event, Workers},
};
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet, VecDeque},
    sync::atomic::Ordering,
    time::Instant,
};
use windows::Win32::UI::{
    Accessibility::{HCF_HIGHCONTRASTON, HIGHCONTRASTW, HIGHCONTRASTW_FLAGS},
    Input::KeyboardAndMouse::*,
};
use windows::Win32::{
    Graphics::{
        Direct2D::{Common::*, *},
        DirectWrite::*,
        Dwm::*,
        Dxgi::Common::*,
        Gdi::*,
        Imaging::*,
    },
    System::LibraryLoader::GetModuleHandleW,
    UI::{Controls::*, HiDpi::*},
};

#[allow(non_snake_case)]
unsafe fn SendMessageW(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    windows::Win32::UI::WindowsAndMessaging::SendMessageW(hwnd, msg, Some(wp), Some(lp))
}

const WAKE: u32 = WM_APP + 1;
const ACTION: u32 = WM_APP + 2;
const SEARCH_ID: usize = 100;
const LIST_ID: usize = 101;
const APPLY: usize = 200;
const CANCEL: usize = 201;
const BROWSE: usize = 202;
const SETTINGS: usize = 203;
const UNDO: usize = 204;
thread_local! {static STATE:RefCell<Option<State>>=const{RefCell::new(None)};}

fn color(r: u8, g: u8, b: u8) -> D2D1_COLOR_F {
    D2D1_COLOR_F {
        r: r as f32 / 255.,
        g: g as f32 / 255.,
        b: b as f32 / 255.,
        a: 1.,
    }
}
fn rect(x: f32, y: f32, w: f32, h: f32) -> D2D_RECT_F {
    D2D_RECT_F {
        left: x,
        top: y,
        right: x + w,
        bottom: y + h,
    }
}
fn rgb(c: D2D1_COLOR_F) -> COLORREF {
    COLORREF((c.r * 255.) as u32 | (((c.g * 255.) as u32) << 8) | (((c.b * 255.) as u32) << 16))
}
fn blend(a: D2D1_COLOR_F, b: D2D1_COLOR_F, t: f32) -> D2D1_COLOR_F {
    let t = t.clamp(0., 1.);
    D2D1_COLOR_F {
        r: a.r + (b.r - a.r) * t,
        g: a.g + (b.g - a.g) * t,
        b: a.b + (b.b - a.b) * t,
        a: 1.,
    }
}
fn ease(since: Instant) -> f32 {
    let t = (since.elapsed().as_secs_f32() / 0.18).min(1.);
    1. - (1. - t).powi(3)
}
unsafe fn motion_enabled() -> bool {
    let mut enabled = windows::core::BOOL(1);
    let _ = SystemParametersInfoW(
        SPI_GETCLIENTAREAANIMATION,
        0,
        Some((&mut enabled as *mut windows::core::BOOL).cast()),
        SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
    );
    enabled.as_bool()
}
const ROW_TIMER: usize = 0xdf02;
#[derive(Clone, Copy)]
struct Theme {
    dark: bool,
    background: D2D1_COLOR_F,
    surface: D2D1_COLOR_F,
    text: D2D1_COLOR_F,
    muted: D2D1_COLOR_F,
    accent: D2D1_COLOR_F,
    selected: D2D1_COLOR_F,
    high_contrast: bool,
}
impl Theme {
    unsafe fn read() -> Self {
        let dark = super::reg_raw(
            Root::User,
            "Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize",
            "AppsUseLightTheme",
        )
        .is_some_and(|(_, v)| v.first() == Some(&0));
        let mut accent = 0u32;
        let mut opaque = windows::core::BOOL(0);
        let _ = DwmGetColorizationColor(&mut accent, &mut opaque);
        let mut hc = HIGHCONTRASTW {
            cbSize: size_of::<HIGHCONTRASTW>() as u32,
            ..Default::default()
        };
        let _ = SystemParametersInfoW(
            SPI_GETHIGHCONTRAST,
            hc.cbSize,
            Some((&mut hc as *mut HIGHCONTRASTW).cast()),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        );
        Self {
            dark,
            background: if dark {
                color(25, 27, 32)
            } else {
                color(246, 247, 250)
            },
            surface: if dark {
                color(32, 35, 41)
            } else {
                color(255, 255, 255)
            },
            text: if dark {
                color(243, 245, 249)
            } else {
                color(25, 31, 43)
            },
            muted: if dark {
                color(162, 172, 190)
            } else {
                color(92, 104, 124)
            },
            accent: color((accent >> 16) as u8, (accent >> 8) as u8, accent as u8),
            selected: if dark {
                color(43, 58, 79)
            } else {
                color(228, 237, 252)
            },
            high_contrast: hc.dwFlags & HCF_HIGHCONTRASTON != HIGHCONTRASTW_FLAGS(0),
        }
    }
    unsafe fn window(self, hwnd: HWND) {
        let dark = self.dark as i32;
        let round = 2i32;
        let backdrop = 2i32;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_USE_IMMERSIVE_DARK_MODE,
            (&dark as *const i32).cast(),
            4,
        );
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            (&round as *const i32).cast(),
            4,
        );
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_SYSTEMBACKDROP_TYPE,
            (&backdrop as *const i32).cast(),
            4,
        );
    }
}

struct Renderer {
    target: ID2D1HwndRenderTarget,
    write: IDWriteFactory,
    normal: IDWriteTextFormat,
    small: IDWriteTextFormat,
    heading: IDWriteTextFormat,
    icons: VecDeque<(String, ID2D1Bitmap)>,
}
impl Renderer {
    unsafe fn new(hwnd: HWND, dpi: u32) -> windows::core::Result<Self> {
        let factory: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_MULTI_THREADED, None)?;
        let write: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
        let mut r = RECT::default();
        GetClientRect(hwnd, &mut r)?;
        let target = factory.CreateHwndRenderTarget(
            &D2D1_RENDER_TARGET_PROPERTIES {
                r#type: if std::env::args().any(|a| a == "--software") {
                    D2D1_RENDER_TARGET_TYPE_SOFTWARE
                } else {
                    D2D1_RENDER_TARGET_TYPE_DEFAULT
                },
                pixelFormat: D2D1_PIXEL_FORMAT {
                    format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    alphaMode: D2D1_ALPHA_MODE_IGNORE,
                },
                dpiX: dpi as f32,
                dpiY: dpi as f32,
                ..Default::default()
            },
            &D2D1_HWND_RENDER_TARGET_PROPERTIES {
                hwnd,
                pixelSize: D2D_SIZE_U {
                    width: r.right.max(1) as u32,
                    height: r.bottom.max(1) as u32,
                },
                presentOptions: D2D1_PRESENT_OPTIONS_IMMEDIATELY,
            },
        )?;
        target.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_CLEARTYPE);
        let normal = write.CreateTextFormat(
            w!("Segoe UI"),
            None,
            DWRITE_FONT_WEIGHT_NORMAL,
            DWRITE_FONT_STYLE_NORMAL,
            DWRITE_FONT_STRETCH_NORMAL,
            14.,
            w!(""),
        )?;
        let small = write.CreateTextFormat(
            w!("Segoe UI"),
            None,
            DWRITE_FONT_WEIGHT_NORMAL,
            DWRITE_FONT_STYLE_NORMAL,
            DWRITE_FONT_STRETCH_NORMAL,
            11.5,
            w!(""),
        )?;
        let heading = write.CreateTextFormat(
            w!("Segoe UI Variable Display"),
            None,
            DWRITE_FONT_WEIGHT_SEMI_BOLD,
            DWRITE_FONT_STYLE_NORMAL,
            DWRITE_FONT_STRETCH_NORMAL,
            25.,
            w!(""),
        )?;
        for f in [&normal, &small, &heading] {
            let _ = f.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP);
        }
        Ok(Self {
            target,
            write,
            normal,
            small,
            heading,
            icons: VecDeque::new(),
        })
    }
    unsafe fn rounded(&self, r: D2D_RECT_F, c: D2D1_COLOR_F) {
        if let Ok(brush) = self.target.CreateSolidColorBrush(&c, None) {
            self.target.FillRoundedRectangle(
                &D2D1_ROUNDED_RECT {
                    rect: r,
                    radiusX: 7.,
                    radiusY: 7.,
                },
                &brush,
            );
        }
    }
    unsafe fn text(&self, text: &str, r: D2D_RECT_F, c: D2D1_COLOR_F, kind: u8, query: &str) {
        let format = match kind {
            1 => &self.small,
            2 => &self.heading,
            _ => &self.normal,
        };
        let utf16: Vec<_> = text.encode_utf16().collect();
        if let (Ok(layout), Ok(brush)) = (
            self.write.CreateTextLayout(
                &utf16,
                format,
                (r.right - r.left).max(1.),
                (r.bottom - r.top).max(1.),
            ),
            self.target.CreateSolidColorBrush(&c, None),
        ) {
            if !query.is_empty() {
                let lower = text.to_lowercase();
                let q = query.trim_start_matches('.');
                if let Some(pos) = lower.find(q) {
                    // Map expanded lowercase code points back to original UTF-16 character ranges.
                    let (mut offset, mut original) = (0usize, 0u32);
                    for ch in text.chars() {
                        let n = ch.to_lowercase().map(char::len_utf8).sum::<usize>();
                        let len = ch.len_utf16() as u32;
                        if offset < pos + q.len() && offset + n > pos {
                            let _ = layout.SetFontWeight(
                                DWRITE_FONT_WEIGHT_BOLD,
                                DWRITE_TEXT_RANGE {
                                    startPosition: original,
                                    length: len,
                                },
                            );
                        }
                        offset += n;
                        original += len;
                    }
                } else {
                    let mut chars = q.chars();
                    let mut next = chars.next();
                    let mut original = 0;
                    for ch in text.chars() {
                        let len = ch.len_utf16() as u32;
                        let mut matched = false;
                        for lower in ch.to_lowercase() {
                            if Some(lower) == next {
                                matched = true;
                                next = chars.next();
                            }
                        }
                        if matched {
                            let _ = layout.SetFontWeight(
                                DWRITE_FONT_WEIGHT_BOLD,
                                DWRITE_TEXT_RANGE {
                                    startPosition: original,
                                    length: len,
                                },
                            );
                        }
                        original += len;
                        if next.is_none() {
                            break;
                        }
                    }
                }
            }
            // Obtain the binding's Vector2 through a point member without an extra dependency.
            let mut origin = D2D1_ELLIPSE::default().point;
            origin.X = r.left;
            origin.Y = r.top;
            self.target
                .DrawTextLayout(origin, &layout, &brush, D2D1_DRAW_TEXT_OPTIONS_CLIP);
        }
    }
    unsafe fn resize(&self, hwnd: HWND, dpi: u32) {
        let mut r = RECT::default();
        let _ = GetClientRect(hwnd, &mut r);
        self.target.SetDpi(dpi as f32, dpi as f32);
        let _ = self.target.Resize(&D2D_SIZE_U {
            width: r.right.max(1) as u32,
            height: r.bottom.max(1) as u32,
        });
    }
    unsafe fn icon(
        &mut self,
        key: &str,
        pixels: &VecDeque<(String, Vec<u8>, u32, u32)>,
        x: f32,
        y: f32,
    ) {
        if let Some(i) = self.icons.iter().position(|(k, _)| k == key) {
            if let Some(entry) = self.icons.remove(i) {
                self.icons.push_front(entry);
            }
        } else if let Some((_, bytes, w, h)) = pixels.iter().find(|(k, _, _, _)| k == key) {
            if !bytes.is_empty() {
                if let Ok(bitmap) = self.target.CreateBitmap(
                    D2D_SIZE_U {
                        width: *w,
                        height: *h,
                    },
                    Some(bytes.as_ptr().cast()),
                    w * 4,
                    &D2D1_BITMAP_PROPERTIES {
                        pixelFormat: D2D1_PIXEL_FORMAT {
                            format: DXGI_FORMAT_B8G8R8A8_UNORM,
                            alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
                        },
                        dpiX: 96.,
                        dpiY: 96.,
                    },
                ) {
                    self.icons.push_front((key.into(), bitmap));
                    if self.icons.len() > 48 {
                        self.icons.pop_back();
                    }
                }
            }
        }
        if let Some((_, bitmap)) = self.icons.iter().find(|(k, _)| k == key) {
            self.target.DrawBitmap(
                bitmap,
                Some(&rect(x, y, 30., 30.)),
                1.,
                D2D1_BITMAP_INTERPOLATION_MODE_LINEAR,
                None,
            );
        } else {
            self.rounded(rect(x, y, 29., 32.), color(110, 130, 160));
            self.text(
                "•",
                rect(x + 9., y + 2., 16., 25.),
                color(240, 245, 255),
                0,
                "",
            );
        }
    }
}

pub fn load_icon(key: &str, dpi: u32) -> Option<(Vec<u8>, u32, u32)> {
    let expanded = expand(key);
    if expanded.starts_with('\\') || expanded.contains("://") {
        return None;
    }
    if expanded.as_bytes().get(1) == Some(&b':') && !local_path(&expanded) {
        return None;
    }
    unsafe {
        let size = (32 * dpi / 96).clamp(16, 96);
        let mut icon = HICON::default();
        if let Some((path, index)) = key.rsplit_once(',').and_then(|(p, i)| {
            i.trim()
                .parse::<i32>()
                .ok()
                .map(|i| (p.trim_matches('"'), i))
        }) {
            let expanded_path = expand(path);
            if Path::new(&expanded_path).is_absolute() && !local_path(&expanded_path) {
                return None;
            }
            let _ = SHDefExtractIconW(
                PCWSTR(wide(&expand(path)).as_ptr()),
                index,
                0,
                Some(&mut icon),
                None,
                size,
            );
        }
        if icon.is_invalid() {
            let mut info = SHFILEINFOW::default();
            let flags = SHGFI_ICON | SHGFI_LARGEICON;
            if key.starts_with('.') {
                SHGetFileInfoW(
                    PCWSTR(wide(key).as_ptr()),
                    FILE_ATTRIBUTE_NORMAL,
                    Some(&mut info),
                    size_of::<SHFILEINFOW>() as u32,
                    flags | SHGFI_USEFILEATTRIBUTES,
                );
            } else {
                let mut pidl = std::ptr::null_mut();
                if SHParseDisplayName(PCWSTR(wide(key).as_ptr()), None, &mut pidl, 0, None).is_ok()
                {
                    SHGetFileInfoW(
                        PCWSTR(pidl.cast()),
                        FILE_FLAGS_AND_ATTRIBUTES(0),
                        Some(&mut info),
                        size_of::<SHFILEINFOW>() as u32,
                        flags | SHGFI_PIDL,
                    );
                    CoTaskMemFree(Some(pidl.cast()));
                } else {
                    SHGetFileInfoW(
                        PCWSTR(wide(key).as_ptr()),
                        FILE_ATTRIBUTE_NORMAL,
                        Some(&mut info),
                        size_of::<SHFILEINFOW>() as u32,
                        flags,
                    );
                }
            }
            icon = info.hIcon;
        }
        if icon.is_invalid() {
            return None;
        }
        let result = (|| {
            let factory: IWICImagingFactory =
                CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER).ok()?;
            let bitmap = factory.CreateBitmapFromHICON(icon).ok()?;
            let bitmap = WICConvertBitmapSource(&GUID_WICPixelFormat32bppPBGRA, &bitmap).ok()?;
            let (mut w, mut h) = (0, 0);
            bitmap.GetSize(&mut w, &mut h).ok()?;
            if w > 256 || h > 256 || w == 0 || h == 0 {
                return None;
            }
            let mut pixels = vec![0u8; (w * h * 4) as usize];
            bitmap
                .CopyPixels(std::ptr::null(), w * 4, &mut pixels)
                .ok()?;
            Some((pixels, w, h))
        })();
        let _ = DestroyIcon(icon);
        result
    }
}

// SAFETY: constructed only from a MULTI_THREADED D2D factory. DirectWrite's shared factory
// is free threaded. Ownership moves once from initialization worker to UI; there are no
// concurrent calls, and this code never calls D3D/DXGI behind Direct2D's factory lock.
// https://learn.microsoft.com/windows/win32/direct2d/multi-threaded-direct2d-apps
struct PreparedRenderer(Renderer);
unsafe impl Send for PreparedRenderer {}
type RenderRequest = std::sync::mpsc::Sender<(usize, u32)>;
type RenderReady = std::sync::mpsc::Receiver<(usize, Result<PreparedRenderer, String>)>;
fn renderer_worker(main: HWND) -> (RenderRequest, RenderReady) {
    let (tx, rx) = std::sync::mpsc::channel::<(usize, u32)>();
    let (done, ready) = std::sync::mpsc::channel();
    let main = main.0 as usize;
    std::thread::spawn(move || {
        let _com = ComApartment::new();
        while let Ok((raw, dpi)) = rx.recv() {
            let result = unsafe { Renderer::new(HWND(raw as *mut _), dpi) }
                .map(PreparedRenderer)
                .map_err(|e| e.to_string());
            if done.send((raw, result)).is_err() {
                break;
            }
            unsafe {
                let _ = PostMessageW(Some(HWND(main as *mut _)), WAKE, WPARAM(0), LPARAM(0));
            }
        }
    });
    (tx, ready)
}

struct State {
    main: HWND,
    edit: HWND,
    list: HWND,
    popup: Option<HWND>,
    app_edit: HWND,
    app_list: HWND,
    types: Vec<FileType>,
    type_map: HashMap<String, usize>,
    apps: Vec<App>,
    choices: Vec<App>,
    rows: Vec<(usize, i32)>,
    app_rows: Vec<(usize, i32)>,
    query: String,
    app_query: String,
    selected: Option<FileType>,
    theme: Theme,
    dpi: u32,
    font: HFONT,
    renderers: HashMap<usize, Renderer>,
    renderer_requests: RenderRequest,
    renderer_ready: RenderReady,
    renderer_pending: HashSet<usize>,
    renderer_failed: HashSet<usize>,
    workers: Workers,
    pixels: VecDeque<(String, Vec<u8>, u32, u32)>,
    pending: HashSet<String>,
    loading: bool,
    busy: bool,
    status: String,
    status_error: bool,
    start: Instant,
    first_frame: bool,
    animation: Option<(Instant, bool, RECT)>,
    filter_max_us: u128,
    type_focus: Option<String>,
    app_focus: Option<String>,
    input_started: Option<Instant>,
    paint_samples: Vec<u128>,
    paint_count: u64,
    background_brush: HBRUSH,
    first_paint_us: Option<u128>,
    first_gpu_us: Option<u128>,
    hover: HashMap<usize, (Option<usize>, Option<usize>, Instant)>,
    selection_motion: HashMap<usize, Instant>,
    motion_enabled: bool,
}
impl State {
    unsafe fn new(main: HWND, start: Instant) -> Self {
        let dpi = GetDpiForWindow(main).max(96);
        let theme = Theme::read();
        theme.window(main);
        let edit = control(
            main,
            w!("EDIT"),
            "Search file types",
            WS_TABSTOP | WINDOW_STYLE(ES_AUTOHSCROLL as u32),
            SEARCH_ID,
        );
        SendMessageW(
            edit,
            EM_SETCUEBANNER,
            WPARAM(1),
            LPARAM(w!("Search file types, e.g. .bat, .pdf").0 as isize),
        );
        let list = make_list(main, LIST_ID);
        let _ = control(main, w!("BUTTON"), "Undo", WS_TABSTOP, UNDO);
        let _ = control(main, w!("BUTTON"), "Open Settings", WS_TABSTOP, SETTINGS);
        let raw = main.0 as usize;
        let workers = ui::workers(move || {
            let _ = PostMessageW(Some(HWND(raw as *mut _)), WAKE, WPARAM(0), LPARAM(0));
        });
        let font = make_font(dpi);
        let (renderer_requests, renderer_ready) = renderer_worker(main);
        for h in [edit, list] {
            SendMessageW(h, WM_SETFONT, WPARAM(font.0 as usize), LPARAM(1));
        }
        let mut s = Self {
            main,
            edit,
            list,
            popup: None,
            app_edit: HWND::default(),
            app_list: HWND::default(),
            types: Vec::new(),
            type_map: HashMap::new(),
            apps: Vec::new(),
            choices: Vec::new(),
            rows: Vec::with_capacity(8192),
            app_rows: Vec::with_capacity(1024),
            query: String::new(),
            app_query: String::new(),
            selected: None,
            theme,
            dpi,
            font,
            renderers: HashMap::new(),
            renderer_requests,
            renderer_ready,
            renderer_pending: HashSet::new(),
            renderer_failed: HashSet::new(),
            workers,
            pixels: VecDeque::new(),
            pending: HashSet::new(),
            loading: true,
            busy: false,
            status: "Discovering registered file types…".into(),
            status_error: false,
            start,
            first_frame: true,
            animation: None,
            filter_max_us: 0,
            type_focus: None,
            app_focus: None,
            input_started: None,
            paint_samples: Vec::with_capacity(1024),
            paint_count: 0,
            background_brush: CreateSolidBrush(rgb(theme.surface)),
            first_paint_us: None,
            first_gpu_us: None,
            hover: HashMap::new(),
            selection_motion: HashMap::new(),
            motion_enabled: motion_enabled(),
        };
        s.layout(main);
        s
    }
    unsafe fn layout(&mut self, hwnd: HWND) {
        let mut r = RECT::default();
        let _ = GetClientRect(hwnd, &mut r);
        let scale = self.dpi as f32 / 96.;
        let w = r.right as f32 / scale;
        let h = r.bottom as f32 / scale;
        let fly = self.popup == Some(hwnd);
        let (edit, list) = if fly {
            (self.app_edit, self.app_list)
        } else {
            (self.edit, self.list)
        };
        place(edit, 24., if fly { 100. } else { 90. }, w - 48., 36., scale);
        place(
            list,
            16.,
            if fly { 150. } else { 143. },
            w - 32.,
            h - if fly { 224. } else { 207. },
            scale,
        );
        if fly {
            place_id(hwnd, BROWSE, 24., h - 53., 90., 32., scale);
            place_id(hwnd, SETTINGS, 122., h - 53., 110., 32., scale);
            place_id(hwnd, CANCEL, w - 260., h - 53., 90., 32., scale);
            place_id(hwnd, APPLY, w - 160., h - 53., 136., 32., scale);
        } else {
            place_id(hwnd, UNDO, w - 242., h - 46., 72., 30., scale);
            place_id(hwnd, SETTINGS, w - 158., h - 46., 134., 30., scale);
        }
        let mut col = LVCOLUMNW {
            mask: LVCF_WIDTH,
            cx: ((w - 56.) * scale) as i32,
            ..Default::default()
        };
        SendMessageW(
            list,
            LVM_SETCOLUMNW,
            WPARAM(0),
            LPARAM((&mut col as *mut LVCOLUMNW) as isize),
        );
        if let Some(renderer) = self.renderers.get(&(hwnd.0 as usize)) {
            renderer.resize(hwnd, self.dpi);
        }
        if let Some(renderer) = self.renderers.get(&(list.0 as usize)) {
            renderer.resize(list, self.dpi);
        }
        let _ = InvalidateRect(Some(hwnd), None, false);
        let _ = InvalidateRect(Some(list), None, false);
    }
    unsafe fn filter(&mut self, fly: bool) {
        let begin = Instant::now();
        if fly {
            let query = window_text(self.app_edit).to_lowercase();
            if query != self.app_query {
                self.app_focus = None;
            }
            self.app_query = query;
            search::filter(
                self.choices
                    .iter()
                    .enumerate()
                    .map(|(i, a)| (i, a.index.as_str(), a.recommended)),
                &self.app_query,
                &mut self.app_rows,
            );
            set_count(self.app_list, self.app_rows.len());
            if let Some(id) = &self.app_focus {
                if let Some(row) = self
                    .app_rows
                    .iter()
                    .position(|(i, _)| self.choices[*i].identity() == id)
                {
                    select_row(self.app_list, row);
                }
            }
        } else {
            let query = window_text(self.edit).to_lowercase();
            if query != self.query {
                self.type_focus = None;
            }
            self.query = query;
            search::filter(
                self.types
                    .iter()
                    .enumerate()
                    .map(|(i, t)| (i, t.index.as_str(), false)),
                &self.query,
                &mut self.rows,
            );
            set_count(self.list, self.rows.len());
            if let Some(ext) = &self.type_focus {
                if let Some(row) = self
                    .rows
                    .iter()
                    .position(|(i, _)| self.types[*i].extension == *ext)
                {
                    select_row(self.list, row);
                }
            }
        }
        self.remember_focus(fly);
        self.filter_max_us = self.filter_max_us.max(begin.elapsed().as_micros());
        let _ = InvalidateRect(
            Some(if fly { self.app_list } else { self.list }),
            None,
            false,
        );
    }
    unsafe fn remember_focus(&mut self, fly: bool) {
        let row = selected_index(if fly { self.app_list } else { self.list });
        if fly {
            self.app_focus = row
                .and_then(|row| self.app_rows.get(row))
                .map(|(i, _)| self.choices[*i].identity().into());
        } else {
            self.type_focus = row
                .and_then(|row| self.rows.get(row))
                .map(|(i, _)| self.types[*i].extension.clone());
        }
    }
    unsafe fn drain(&mut self) {
        while let Ok((raw, result)) = self.renderer_ready.try_recv() {
            self.renderer_pending.remove(&raw);
            if ![
                self.main,
                self.list,
                self.app_list,
                self.popup.unwrap_or_default(),
            ]
            .iter()
            .any(|h| h.0 as usize == raw)
            {
                continue;
            }
            match result {
                Ok(prepared) => {
                    prepared.0.resize(HWND(raw as *mut _), self.dpi);
                    self.renderers.insert(raw, prepared.0);
                }
                Err(e) => {
                    self.renderer_failed.insert(raw);
                    self.status = format!("Using native rendering: {e}");
                }
            }
            let _ = InvalidateRect(Some(HWND(raw as *mut _)), None, false);
        }
        let mut types_changed = false;
        let mut apps_changed = false;
        // Limit work per dispatch so a large refresh cannot starve keyboard input.
        for _ in 0..8 {
            let Ok(event) = self.workers.events.try_recv() else {
                break;
            };
            match event {
                Event::Cached(types, apps) => {
                    if self.types.is_empty() {
                        self.types = types;
                        self.type_map = self
                            .types
                            .iter()
                            .enumerate()
                            .map(|(i, t)| (t.extension.clone(), i))
                            .collect();
                        self.apps = apps;
                        types_changed = true;
                        apps_changed = true;
                        self.status = "Refreshing installed apps in the background…".into();
                    }
                }
                Event::Types(batch) => {
                    for row in batch {
                        if let Some(i) = self.type_map.get(&row.extension) {
                            if self.types[*i] != row {
                                self.types[*i] = row;
                                types_changed = true;
                            }
                        } else {
                            self.type_map
                                .insert(row.extension.clone(), self.types.len());
                            self.types.push(row);
                            types_changed = true;
                        }
                    }
                }
                Event::Apps(batch) => {
                    app_scan::merge(&mut self.apps, batch);
                    apps_changed = true;
                }
                Event::Complete(types, apps, error) => {
                    self.types = types;
                    self.type_map = self
                        .types
                        .iter()
                        .enumerate()
                        .map(|(i, t)| (t.extension.clone(), i))
                        .collect();
                    self.apps = apps;
                    self.loading = false;
                    types_changed = true;
                    apps_changed = true;
                    if !self.status_error {
                        self.status = error.unwrap_or_else(|| {
                            format!(
                                "{} file types · {} installed apps",
                                self.types.len(),
                                self.apps.len()
                            )
                        });
                    }
                }
                Event::Handlers(ext, handlers) => {
                    if self.selected.as_ref().is_some_and(|t| t.extension == ext) {
                        let mut choices = self.apps.clone();
                        app_scan::merge(&mut choices, handlers);
                        self.choices = choices;
                        self.filter(true);
                    }
                }
                Event::Result(result) => {
                    self.busy = false;
                    match result {
                        Ok(message) => {
                            self.status = message;
                            self.status_error = false;
                            self.close_popup();
                            let _ = self.workers.refresh.send(());
                        }
                        Err(message) => {
                            self.status = message.clone();
                            self.status_error = true;
                            if let Some(popup) = self.popup {
                                let _ = EnableWindow(popup, true);
                            }
                            MessageBoxW(
                                self.popup.or(Some(self.main)),
                                PCWSTR(wide(&message).as_ptr()),
                                w!("DefaultPicker — change could not be confirmed"),
                                MB_OK | MB_ICONWARNING,
                            );
                        }
                    }
                }
                Event::Icon(key, pixels, w, h) => {
                    self.pending.remove(&key);
                    if self.pixels.len() >= 48 {
                        self.pixels.pop_back();
                    }
                    self.pixels.push_front((key, pixels, w, h));
                    let _ = InvalidateRect(Some(self.list), None, false);
                    if self.popup.is_some() {
                        let _ = InvalidateRect(Some(self.app_list), None, false);
                    }
                }
            }
        }
        if types_changed {
            self.filter(false);
        }
        if apps_changed && self.popup.is_some() {
            let recommended: Vec<_> = self
                .choices
                .iter()
                .filter(|a| a.recommended)
                .cloned()
                .collect();
            self.choices = self.apps.clone();
            app_scan::merge(&mut self.choices, recommended);
            self.filter(true);
        }
        let _ = InvalidateRect(Some(self.main), None, false);
    }
    unsafe fn open_popup(&mut self) {
        if self.popup.is_some() {
            return;
        }
        let row = selected_index(self.list).unwrap_or(0);
        let Some((i, _)) = self.rows.get(row) else {
            return;
        };
        self.selected = Some(self.types[*i].clone());
        self.choices = self.apps.clone();
        self.app_query.clear();
        let mut r = RECT::default();
        let _ = GetWindowRect(self.main, &mut r);
        let scale = self.dpi as f32 / 96.;
        let (w, h) = ((660. * scale) as i32, (550. * scale) as i32);
        let x = (r.left + r.right - w) / 2;
        let y = (r.top + r.bottom - h) / 2;
        let Ok(hwnd) = CreateWindowExW(
            WS_EX_CONTROLPARENT | WS_EX_LAYERED,
            w!("DefaultPickerWindow"),
            w!("Choose a default app"),
            WS_POPUP | WS_CAPTION | WS_THICKFRAME | WS_SYSMENU | WS_CLIPCHILDREN,
            x,
            y,
            w,
            h,
            Some(self.main),
            None,
            None,
            None,
        ) else {
            return;
        };
        self.popup = Some(hwnd);
        self.theme.window(hwnd);
        self.app_edit = control(
            hwnd,
            w!("EDIT"),
            "Search apps",
            WS_TABSTOP | WINDOW_STYLE(ES_AUTOHSCROLL as u32),
            SEARCH_ID,
        );
        SendMessageW(
            self.app_edit,
            EM_SETCUEBANNER,
            WPARAM(1),
            LPARAM(w!("Search installed apps, e.g. terminal").0 as isize),
        );
        self.app_list = make_list(hwnd, LIST_ID);
        let _ = control(hwnd, w!("BUTTON"), "Browse…", WS_TABSTOP, BROWSE);
        let _ = control(hwnd, w!("BUTTON"), "Open Settings", WS_TABSTOP, SETTINGS);
        let _ = control(hwnd, w!("BUTTON"), "Cancel", WS_TABSTOP, CANCEL);
        let label = if self
            .selected
            .as_ref()
            .is_some_and(|t| assoc::executable_type(&t.extension))
        {
            "Launch via Terminal"
        } else {
            "Set as default"
        };
        let _ = control(
            hwnd,
            w!("BUTTON"),
            label,
            WS_TABSTOP | WINDOW_STYLE(BS_DEFPUSHBUTTON as u32),
            APPLY,
        );
        for child in [self.app_edit, self.app_list] {
            SendMessageW(child, WM_SETFONT, WPARAM(self.font.0 as usize), LPARAM(1));
        }
        self.layout(hwnd);
        self.filter(true);
        let _ = EnableWindow(self.main, false);
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
        let _ = SetFocus(Some(self.app_edit));
        self.animate(hwnd, false);
        if let Some(t) = &self.selected {
            let ext = t.extension.clone();
            let send = self.workers.send.clone();
            let raw = self.main.0 as usize;
            ui::async_handlers(ext, send, move || {
                let _ = PostMessageW(Some(HWND(raw as *mut _)), WAKE, WPARAM(0), LPARAM(0));
            });
        }
    }
    unsafe fn animate(&mut self, hwnd: HWND, closing: bool) {
        let mut enabled = windows::core::BOOL(1);
        let _ = SystemParametersInfoW(
            SPI_GETCLIENTAREAANIMATION,
            0,
            Some((&mut enabled as *mut windows::core::BOOL).cast()),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        );
        if !enabled.as_bool() {
            let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), 255, LWA_ALPHA);
            if closing {
                self.destroy_popup();
            }
            return;
        }
        let mut r = RECT::default();
        let _ = GetWindowRect(hwnd, &mut r);
        self.animation = Some((Instant::now(), closing, r));
        let _ = SetTimer(Some(hwnd), 1, 15, None);
        if !closing {
            let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), 0, LWA_ALPHA);
        }
    }
    unsafe fn animation_tick(&mut self, hwnd: HWND) {
        if let Some((start, closing, r)) = self.animation {
            let t = (start.elapsed().as_secs_f32() / 0.18).min(1.);
            let eased = 1. - (1. - t).powi(3);
            let opacity = if closing { 1. - eased } else { eased };
            let _ =
                SetLayeredWindowAttributes(hwnd, COLORREF(0), (255. * opacity) as u8, LWA_ALPHA);
            let offset = ((1. - opacity) * 10. * self.dpi as f32 / 96.) as i32;
            let _ = SetWindowPos(
                hwnd,
                None,
                r.left,
                r.top + offset,
                0,
                0,
                SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
            );
            if t >= 1. {
                let _ = KillTimer(Some(hwnd), 1);
                self.animation = None;
                if closing {
                    self.destroy_popup();
                }
            }
        }
    }
    unsafe fn close_popup(&mut self) {
        if self.busy {
            return;
        }
        if let Some(hwnd) = self.popup {
            self.animate(hwnd, true);
        }
    }
    unsafe fn destroy_popup(&mut self) {
        if let Some(hwnd) = self.popup.take() {
            self.renderers.remove(&(self.app_list.0 as usize));
            self.renderers.remove(&(hwnd.0 as usize));
            let _ = DestroyWindow(hwnd);
            self.app_list = HWND::default();
            self.app_edit = HWND::default();
            self.selected = None;
            self.choices.clear();
            self.app_rows.clear();
            let _ = EnableWindow(self.main, true);
            let _ = SetForegroundWindow(self.main);
            let _ = SetFocus(Some(self.edit));
        }
    }
    unsafe fn apply(&mut self) {
        if self.busy {
            return;
        }
        let Some(t) = self.selected.clone() else {
            return;
        };
        let row = selected_index(self.app_list).unwrap_or(0);
        let Some((i, _)) = self.app_rows.get(row) else {
            return;
        };
        let app = self.choices[*i].clone();
        let verb = assoc::executable_type(&t.extension);
        if verb {
            if ![".bat", ".cmd"].contains(&t.extension.as_str()) {
                self.status =
                    "Windows protects this executable class. Choose its handler in Settings."
                        .into();
                self.status_error = true;
                self.open_settings();
                return;
            }
            let explanation=format!("Windows treats {} as executable code. This changes the per-user open command for its effective ProgID to launch Windows Terminal with cmd.exe /d /c.\n\nIt affects every extension sharing that ProgID. It does not change the default-app metadata. Scripts still run with your permissions.\n\nApply this launch-command override? Ctrl+Z restores the previous command.",t.extension);
            if MessageBoxW(
                self.popup,
                PCWSTR(wide(&explanation).as_ptr()),
                w!("Change script launch command"),
                MB_YESNO | MB_ICONWARNING | MB_DEFBUTTON2,
            ) != IDYES
            {
                return;
            }
        }
        self.busy = true;
        self.status = "Applying and verifying the association…".into();
        self.status_error = false;
        if let Some(p) = self.popup {
            let _ = EnableWindow(p, false);
        }
        let raw = self.main.0 as usize;
        ui::async_change(
            t.extension,
            app,
            verb,
            self.workers.send.clone(),
            move || {
                let _ = PostMessageW(Some(HWND(raw as *mut _)), WAKE, WPARAM(0), LPARAM(0));
            },
        );
    }
    unsafe fn undo(&mut self) {
        if self.busy {
            return;
        }
        self.busy = true;
        let sender = self.workers.send.clone();
        let raw = self.main.0 as usize;
        std::thread::spawn(move || {
            let _com = ComApartment::new();
            let _ = sender.send(Event::Result(assoc::undo()));
            let _ = PostMessageW(Some(HWND(raw as *mut _)), WAKE, WPARAM(0), LPARAM(0));
        });
    }
    unsafe fn open_settings(&self) {
        let result = ShellExecuteW(
            Some(self.popup.unwrap_or(self.main)),
            w!("open"),
            w!("ms-settings:defaultapps"),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        );
        if result.0 as isize <= 32 {
            MessageBoxW(Some(self.main),w!("Windows could not open Settings. Open Settings > Apps > Default apps manually."),w!("DefaultPicker"),MB_OK|MB_ICONWARNING);
        }
    }
    unsafe fn browse(&mut self) {
        let result = (|| -> windows::core::Result<String> {
            let dialog: IFileOpenDialog =
                CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER)?;
            dialog.SetOptions(FOS_FILEMUSTEXIST | FOS_PATHMUSTEXIST | FOS_FORCEFILESYSTEM)?;
            dialog.SetFileTypes(&[windows::Win32::UI::Shell::Common::COMDLG_FILTERSPEC {
                pszName: w!("Windows applications"),
                pszSpec: w!("*.exe"),
            }])?;
            dialog.SetTitle(w!("Choose an application"))?;
            dialog.Show(self.popup)?;
            let item = dialog.GetResult()?;
            Ok(owned_com_string(item.GetDisplayName(SIGDN_FILESYSPATH)?))
        })();
        if let Ok(executable) = result {
            let name = Path::new(&executable)
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            let mut app = App {
                name,
                icon: executable.clone(),
                executable,
                ..App::default()
            };
            app.reindex();
            self.choices.insert(0, app);
            let _ = SetWindowTextW(self.app_edit, w!(""));
            self.filter(true);
            select_row(self.app_list, 0);
            let _ = SetFocus(Some(self.app_list));
        }
    }
    unsafe fn paint(&mut self, hwnd: HWND, is_list: bool) -> bool {
        if !self.renderers.contains_key(&(hwnd.0 as usize)) {
            let raw = hwnd.0 as usize;
            if !self.renderer_failed.contains(&raw) && self.renderer_pending.insert(raw) {
                let _ = self.renderer_requests.send((raw, self.dpi));
            }
            if is_list {
                return false;
            }
            // Genuine, interactive native first frame while GPU/font setup runs off-thread.
            let mut ps = PAINTSTRUCT::default();
            let dc = BeginPaint(hwnd, &mut ps);
            let _ = FillRect(dc, &ps.rcPaint, self.background_brush);
            SetBkMode(dc, TRANSPARENT);
            SetTextColor(dc, rgb(self.theme.text));
            let old = SelectObject(dc, self.font.into());
            let scale = self.dpi as f32 / 96.;
            let mut heading = wide(if self.popup == Some(hwnd) {
                "Choose a default app"
            } else {
                "DefaultPicker"
            });
            let mut area = RECT {
                left: (24. * scale) as i32,
                top: (22. * scale) as i32,
                right: (640. * scale) as i32,
                bottom: (58. * scale) as i32,
            };
            let _ = DrawTextW(dc, &mut heading, &mut area, DT_LEFT | DT_SINGLELINE);
            SelectObject(dc, old);
            let _ = EndPaint(hwnd, &ps);
            self.record_first_frame(hwnd, is_list);
            return true;
        }
        let mut ps = PAINTSTRUCT::default();
        let _ = BeginPaint(hwnd, &mut ps);
        let Some(mut render) = self.renderers.remove(&(hwnd.0 as usize)) else {
            let _ = EndPaint(hwnd, &ps);
            return true;
        };
        self.paint_count += 1;
        render.target.BeginDraw();
        let th = self.theme;
        render
            .target
            .Clear(Some(&if is_list { th.surface } else { th.background }));
        let size = render.target.GetSize();
        let w = size.width;
        let h = size.height;
        if is_list {
            let fly = hwnd == self.app_list;
            let rows = if fly { &self.app_rows } else { &self.rows };
            let top = SendMessageW(hwnd, LVM_GETTOPINDEX, WPARAM(0), LPARAM(0))
                .0
                .max(0) as usize;
            let count = SendMessageW(hwnd, LVM_GETCOUNTPERPAGE, WPARAM(0), LPARAM(0))
                .0
                .max(1) as usize
                + 2;
            if rows.is_empty() {
                if self.loading && !fly {
                    for i in 0..6 {
                        let y = 14. + i as f32 * 55.;
                        render.rounded(rect(16., y, 30., 30.), th.selected);
                        render.rounded(rect(64., y + 2., w * 0.38, 9.), th.selected);
                        render.rounded(rect(64., y + 18., w * 0.22, 7.), th.selected);
                    }
                } else {
                    render.text("No matches", rect(24., 28., w - 48., 28.), th.text, 0, "");
                    render.text(
                        if fly {
                            "Try another name, or choose Browse…"
                        } else {
                            "Try an extension or a file type name."
                        },
                        rect(24., 61., w - 48., 24.),
                        th.muted,
                        1,
                        "",
                    );
                }
            }
            for (row, row_data) in rows.iter().enumerate().take(top + count).skip(top) {
                let mut rr = RECT {
                    left: LVIR_BOUNDS as i32,
                    ..Default::default()
                };
                SendMessageW(
                    hwnd,
                    LVM_GETITEMRECT,
                    WPARAM(row),
                    LPARAM((&mut rr as *mut RECT) as isize),
                );
                let scale = self.dpi as f32 / 96.;
                let y = rr.top as f32 / scale;
                let height = (rr.bottom - rr.top) as f32 / scale;
                let selected = SendMessageW(
                    hwnd,
                    LVM_GETITEMSTATE,
                    WPARAM(row),
                    LPARAM(LVIS_SELECTED.0 as isize),
                )
                .0 != 0;
                let raw = hwnd.0 as usize;
                if let Some((hover, previous, since)) = self.hover.get(&raw) {
                    let t = if self.motion_enabled {
                        ease(*since)
                    } else {
                        1.
                    };
                    let opacity = if *hover == Some(row) {
                        t * 0.55
                    } else if *previous == Some(row) {
                        (1. - t) * 0.55
                    } else {
                        0.
                    };
                    if opacity > 0. {
                        render.rounded(
                            rect(3., y + 2., w - 6., height - 4.),
                            blend(th.surface, th.selected, opacity),
                        );
                    }
                }
                if selected {
                    let t = if self.motion_enabled {
                        self.selection_motion
                            .get(&raw)
                            .map(|s| ease(*s))
                            .unwrap_or(1.)
                    } else {
                        1.
                    };
                    render.rounded(
                        rect(3., y + 2., w - 6., height - 4.),
                        blend(th.surface, th.selected, t),
                    );
                    render.rounded(rect(3., y + 12., 3., (height - 24.).max(8.)), th.accent);
                }
                let i = row_data.0;
                let (name, detail, right, key, query) = if fly {
                    let a = &self.choices[i];
                    let detail = if a.recommended {
                        "Recommended for this file type".into()
                    } else if !a.publisher.is_empty() {
                        a.publisher.clone()
                    } else if a.executable.is_empty() {
                        "Installed app · choose an advertised handler in Settings".into()
                    } else {
                        a.executable.clone()
                    };
                    (
                        a.name.clone(),
                        detail,
                        String::new(),
                        a.icon.clone(),
                        &self.app_query,
                    )
                } else {
                    let t = &self.types[i];
                    (
                        t.name.clone(),
                        t.extension.clone(),
                        t.current_app.clone(),
                        if t.icon.is_empty() {
                            t.extension.clone()
                        } else {
                            t.icon.clone()
                        },
                        &self.query,
                    )
                };
                if !self.pending.contains(&key) && !self.pixels.iter().any(|(k, _, _, _)| k == &key)
                {
                    self.pending.insert(key.clone());
                    let _ = self.workers.icons.send((key.clone(), self.dpi));
                }
                render.icon(&key, &self.pixels, 15., y + (height - 30.) / 2.);
                let name_width = if fly {
                    w - 86.
                } else {
                    (w * 0.64 - 65.).max(80.)
                };
                render.text(&name, rect(61., y + 7., name_width, 22.), th.text, 0, query);
                render.text(
                    &detail,
                    rect(61., y + 30., w - 85., 18.),
                    th.muted,
                    1,
                    query,
                );
                if !right.is_empty() {
                    render.text(
                        &right,
                        rect(w * 0.65, y + 9., w * 0.35 - 18., 22.),
                        th.muted,
                        1,
                        "",
                    );
                }
            }
        } else if self.popup == Some(hwnd) {
            if let Some(t) = &self.selected {
                let key = if t.icon.is_empty() {
                    t.extension.clone()
                } else {
                    t.icon.clone()
                };
                if !self.pending.contains(&key) && !self.pixels.iter().any(|(k, _, _, _)| k == &key)
                {
                    self.pending.insert(key.clone());
                    let _ = self.workers.icons.send((key.clone(), self.dpi));
                }
                render.icon(&key, &self.pixels, 24., 24.);
                render.text(
                    &format!("Choose an app for {}", t.extension),
                    rect(70., 18., w - 94., 38.),
                    th.text,
                    2,
                    "",
                );
                render.text(
                    &format!(
                        "{}  ·  Current: {}",
                        t.name,
                        if t.current_app.is_empty() {
                            "Not set"
                        } else {
                            &t.current_app
                        }
                    ),
                    rect(70., 61., w - 94., 24.),
                    th.muted,
                    1,
                    "",
                );
            }
            if self.busy {
                render.text(
                    "Applying and checking with Windows…",
                    rect(24., h - 79., w - 48., 22.),
                    th.muted,
                    1,
                    "",
                );
            }
        } else {
            render.text(
                "DefaultPicker",
                rect(24., 19., w - 48., 38.),
                th.text,
                2,
                "",
            );
            render.text(
                "Choose what opens your files.",
                rect(25., 59., w - 48., 22.),
                th.muted,
                0,
                "",
            );
            let message = if self.status_error {
                "Change not confirmed. Open Settings to continue."
            } else {
                &self.status
            };
            render.text(
                message,
                rect(24., h - 40., (w - 290.).max(80.), 28.),
                if self.status_error {
                    color(210, 100, 80)
                } else {
                    th.muted
                },
                1,
                "",
            );
        }
        let result = render.target.EndDraw(None, None);
        if result.is_ok() {
            self.renderers.insert(hwnd.0 as usize, render);
        } else {
            self.renderer_failed.insert(hwnd.0 as usize);
            let _ = InvalidateRect(Some(hwnd), None, false);
        }
        let _ = EndPaint(hwnd, &ps);
        if is_list {
            if let Some(start) = self.input_started.take() {
                if self.paint_samples.len() < 1024 {
                    self.paint_samples.push(start.elapsed().as_micros());
                }
            }
        }
        if !is_list && hwnd == self.main && self.first_gpu_us.is_none() {
            self.first_gpu_us = Some(self.start.elapsed().as_micros());
            if std::env::args().any(|a| a == "--startup-probe") {
                let _ = PostMessageW(Some(self.main), WM_CLOSE, WPARAM(0), LPARAM(0));
            }
        }
        self.record_first_frame(hwnd, is_list);
        true
    }
    fn record_first_frame(&mut self, hwnd: HWND, is_list: bool) {
        if self.first_frame && !is_list && hwnd == self.main {
            self.first_frame = false;
            let elapsed = self.start.elapsed();
            self.first_paint_us = Some(elapsed.as_micros());
            let sender = self.workers.send.clone();
            std::thread::spawn(move || {
                let (ws, peak) = memory();
                let report=format!("entry_to_first_paint_us={}\nworking_set_at_first_paint_bytes={ws}\npeak_bytes={peak}\nThis excludes process creation and is not cold-start-to-input latency.\n",elapsed.as_micros());
                let _ = cache::atomic_write("startup.txt", report.as_bytes());
                drop(sender);
            });
        }
    }
}

unsafe fn make_font(dpi: u32) -> HFONT {
    CreateFontW(
        -((14 * dpi / 96) as i32),
        0,
        0,
        0,
        400,
        0,
        0,
        0,
        DEFAULT_CHARSET,
        OUT_DEFAULT_PRECIS,
        CLIP_DEFAULT_PRECIS,
        CLEARTYPE_QUALITY,
        0,
        w!("Segoe UI"),
    )
}
unsafe fn control(parent: HWND, class: PCWSTR, name: &str, style: WINDOW_STYLE, id: usize) -> HWND {
    let is_edit = class.to_string().is_ok_and(|s| s == "EDIT");
    let is_button = class.to_string().is_ok_and(|s| s == "BUTTON");
    let result = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        class,
        PCWSTR(wide(name).as_ptr()),
        WS_CHILD
            | WS_VISIBLE
            | style
            | if is_button {
                WINDOW_STYLE(BS_OWNERDRAW as u32)
            } else {
                WINDOW_STYLE(0)
            },
        0,
        0,
        1,
        1,
        Some(parent),
        Some(HMENU(id as *mut _)),
        None,
        None,
    );
    if let Ok(hwnd) = result {
        let _ = SetWindowTheme(hwnd, w!("Explorer"), PCWSTR::null());
        if is_edit {
            let _ = SetWindowTextW(hwnd, w!(""));
            let _ = SetWindowSubclass(hwnd, Some(child_proc), 1, 0);
            SendMessageW(hwnd, EM_SETMARGINS, WPARAM(3), LPARAM(12 | (12 << 16)));
        }
        hwnd
    } else {
        HWND::default()
    }
}
unsafe fn make_list(parent: HWND, id: usize) -> HWND {
    let list = control(
        parent,
        WC_LISTVIEWW,
        "Results",
        WS_TABSTOP
            | WS_VSCROLL
            | WINDOW_STYLE(
                LVS_REPORT | LVS_OWNERDATA | LVS_SINGLESEL | LVS_SHOWSELALWAYS | LVS_NOCOLUMNHEADER,
            ),
        id,
    );
    SendMessageW(
        list,
        LVM_SETEXTENDEDLISTVIEWSTYLE,
        WPARAM(0),
        LPARAM((LVS_EX_FULLROWSELECT | LVS_EX_DOUBLEBUFFER) as isize),
    );
    let mut col = LVCOLUMNW {
        mask: LVCF_WIDTH,
        cx: 640,
        ..Default::default()
    };
    SendMessageW(
        list,
        LVM_INSERTCOLUMNW,
        WPARAM(0),
        LPARAM((&mut col as *mut LVCOLUMNW) as isize),
    );
    // A single empty image defines fixed row height, while rows remain virtual and accessible.
    let dpi = GetDpiForWindow(parent).max(96);
    let images = ImageList_Create(1, (56 * dpi / 96) as i32, ILC_COLOR32, 1, 1);
    if !images.is_invalid() {
        SendMessageW(
            list,
            LVM_SETIMAGELIST,
            WPARAM(LVSIL_SMALL as usize),
            LPARAM(images.0 as isize),
        );
    }
    let _ = SetWindowSubclass(list, Some(list_proc), 1, 0);
    list
}
unsafe fn place(hwnd: HWND, x: f32, y: f32, w: f32, h: f32, scale: f32) {
    let _ = SetWindowPos(
        hwnd,
        None,
        (x * scale) as i32,
        (y * scale) as i32,
        (w.max(1.) * scale) as i32,
        (h.max(1.) * scale) as i32,
        SWP_NOZORDER | SWP_NOACTIVATE,
    );
    if GetDlgCtrlID(hwnd) == SEARCH_ID as i32 {
        let region = CreateRoundRectRgn(
            0,
            0,
            (w * scale) as i32,
            (h * scale) as i32,
            (12. * scale) as i32,
            (12. * scale) as i32,
        );
        if !region.is_invalid() && SetWindowRgn(hwnd, Some(region), true) == 0 {
            let _ = DeleteObject(region.into());
        }
    }
}
unsafe fn place_id(parent: HWND, id: usize, x: f32, y: f32, w: f32, h: f32, scale: f32) {
    if let Ok(hwnd) = GetDlgItem(Some(parent), id as i32) {
        place(hwnd, x, y, w, h, scale);
    }
}
unsafe fn window_text(hwnd: HWND) -> String {
    let n = GetWindowTextLengthW(hwnd).clamp(0, 1024) as usize;
    let mut b = vec![0u16; n + 1];
    GetWindowTextW(hwnd, &mut b);
    from_wide(&b)
}
unsafe fn set_count(list: HWND, n: usize) {
    SendMessageW(
        list,
        LVM_SETITEMCOUNT,
        WPARAM(n),
        LPARAM(LVSICF_NOINVALIDATEALL as isize),
    );
    if n > 0 {
        select_row(list, 0);
    }
}
unsafe fn selected_index(list: HWND) -> Option<usize> {
    let n = SendMessageW(
        list,
        LVM_GETNEXTITEM,
        WPARAM(usize::MAX),
        LPARAM(LVNI_SELECTED as isize),
    )
    .0;
    if n < 0 {
        None
    } else {
        Some(n as usize)
    }
}
unsafe fn select_row(list: HWND, index: usize) {
    let mut item = LVITEMW {
        stateMask: LIST_VIEW_ITEM_STATE_FLAGS(LVIS_SELECTED.0 | LVIS_FOCUSED.0),
        state: LIST_VIEW_ITEM_STATE_FLAGS(LVIS_SELECTED.0 | LVIS_FOCUSED.0),
        ..Default::default()
    };
    SendMessageW(
        list,
        LVM_SETITEMSTATE,
        WPARAM(usize::MAX),
        LPARAM(
            (&mut LVITEMW {
                stateMask: LIST_VIEW_ITEM_STATE_FLAGS(LVIS_SELECTED.0 | LVIS_FOCUSED.0),
                ..Default::default()
            } as *mut LVITEMW) as isize,
        ),
    );
    SendMessageW(
        list,
        LVM_SETITEMSTATE,
        WPARAM(index),
        LPARAM((&mut item as *mut LVITEMW) as isize),
    );
    SendMessageW(list, LVM_ENSUREVISIBLE, WPARAM(index), LPARAM(0));
}

unsafe extern "system" fn child_proc(
    hwnd: HWND,
    msg: u32,
    wp: WPARAM,
    lp: LPARAM,
    _id: usize,
    _data: usize,
) -> LRESULT {
    if msg == WM_KEYDOWN {
        let key = wp.0 as u32;
        if key == VK_RETURN_CODE
            || key == VK_ESCAPE_CODE
            || key == VK_UP_CODE
            || key == VK_DOWN_CODE
        {
            if let Ok(parent) = GetParent(hwnd) {
                let _ = PostMessageW(Some(parent), ACTION, wp, LPARAM(0));
            }
            return LRESULT(0);
        }
    }
    DefSubclassProc(hwnd, msg, wp, lp)
}
const VK_RETURN_CODE: u32 = 13;
const VK_ESCAPE_CODE: u32 = 27;
const VK_UP_CODE: u32 = 38;
const VK_DOWN_CODE: u32 = 40;
unsafe extern "system" fn list_proc(
    hwnd: HWND,
    msg: u32,
    wp: WPARAM,
    lp: LPARAM,
    _id: usize,
    _data: usize,
) -> LRESULT {
    if msg == WM_MOUSEMOVE || msg == WM_MOUSELEAVE {
        STATE.with(|cell| {
            if let Ok(mut borrow) = cell.try_borrow_mut() {
                if let Some(s) = borrow.as_mut() {
                    let mut hit = LVHITTESTINFO {
                        pt: POINT {
                            x: (lp.0 as u16 as i16) as i32,
                            y: ((lp.0 >> 16) as u16 as i16) as i32,
                        },
                        ..Default::default()
                    };
                    let row = if msg == WM_MOUSELEAVE {
                        None
                    } else {
                        let index = SendMessageW(
                            hwnd,
                            LVM_HITTEST,
                            WPARAM(0),
                            LPARAM((&mut hit as *mut LVHITTESTINFO) as isize),
                        )
                        .0;
                        if index < 0 {
                            None
                        } else {
                            Some(index as usize)
                        }
                    };
                    let old = s.hover.get(&(hwnd.0 as usize)).and_then(|v| v.0);
                    if old != row {
                        s.hover.insert(hwnd.0 as usize, (row, old, Instant::now()));
                        if s.motion_enabled {
                            let _ = SetTimer(Some(hwnd), ROW_TIMER, 15, None);
                        }
                        let _ = InvalidateRect(Some(hwnd), None, false);
                    }
                    let mut tracking = TRACKMOUSEEVENT {
                        cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                        dwFlags: TME_LEAVE,
                        hwndTrack: hwnd,
                        ..Default::default()
                    };
                    let _ = TrackMouseEvent(&mut tracking);
                }
            }
        });
    }
    if msg == WM_TIMER && wp.0 == ROW_TIMER {
        STATE.with(|cell| {
            if let Ok(borrow) = cell.try_borrow() {
                if let Some(s) = borrow.as_ref() {
                    let raw = hwnd.0 as usize;
                    let active = s.motion_enabled
                        && (s
                            .hover
                            .get(&raw)
                            .is_some_and(|v| v.2.elapsed().as_millis() < 180)
                            || s.selection_motion
                                .get(&raw)
                                .is_some_and(|v| v.elapsed().as_millis() < 180));
                    if !active {
                        let _ = KillTimer(Some(hwnd), ROW_TIMER);
                    }
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
            }
        });
        return LRESULT(0);
    }
    if msg == WM_PAINT {
        let drawn = STATE.with(|cell| {
            let Ok(mut borrow) = cell.try_borrow_mut() else {
                return false;
            };
            let Some(s) = borrow.as_mut() else {
                return false;
            };
            if s.theme.high_contrast {
                return false;
            }
            s.paint(hwnd, true)
        });
        if drawn {
            return LRESULT(0);
        }
    }
    if msg == WM_ERASEBKGND {
        return LRESULT(1);
    }
    if msg == WM_NCDESTROY {
        let images = SendMessageW(
            hwnd,
            LVM_GETIMAGELIST,
            WPARAM(LVSIL_SMALL as usize),
            LPARAM(0),
        );
        if images.0 != 0 {
            let _ = ImageList_Destroy(Some(HIMAGELIST(images.0)));
        }
    }
    let result = DefSubclassProc(hwnd, msg, wp, lp);
    if [
        WM_VSCROLL,
        WM_MOUSEWHEEL,
        WM_KEYDOWN,
        WM_LBUTTONDOWN,
        WM_LBUTTONUP,
        WM_SIZE,
    ]
    .contains(&msg)
    {
        let _ = InvalidateRect(Some(hwnd), None, false);
    }
    result
}

unsafe extern "system" fn window_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let handled=STATE.with(|cell|{
        let Ok(mut guard)=cell.try_borrow_mut()else{return None;};let s=guard.as_mut()?;
        match msg{
            WM_ERASEBKGND=>Some(LRESULT(1)),
            WM_SETFOCUS=>{let _=SetFocus(Some(if s.popup==Some(hwnd){s.app_edit}else{s.edit}));Some(LRESULT(0))},
            WM_PAINT=>{s.paint(hwnd,false);Some(LRESULT(0))},
            WAKE=>{s.drain();Some(LRESULT(0))},
            WM_SIZE=>{s.layout(hwnd);Some(LRESULT(0))},
            WM_GETMINMAXINFO=>{let info=&mut *(lp.0 as *mut MINMAXINFO);info.ptMinTrackSize=POINT{x:(560*s.dpi/96) as i32,y:(420*s.dpi/96) as i32};Some(LRESULT(0))},
            WM_DPICHANGED=>{
                s.dpi=(wp.0&0xffff) as u32;let suggested=&*(lp.0 as *const RECT);let _=SetWindowPos(hwnd,None,suggested.left,suggested.top,suggested.right-suggested.left,suggested.bottom-suggested.top,SWP_NOZORDER|SWP_NOACTIVATE);
                let old=s.font;s.font=make_font(s.dpi);for h in [s.edit,s.list,s.app_edit,s.app_list]{if !h.is_invalid(){SendMessageW(h,WM_SETFONT,WPARAM(s.font.0 as usize),LPARAM(1));}}let _=DeleteObject(old.into());s.renderers.clear();s.pixels.clear();s.pending.clear();s.layout(hwnd);Some(LRESULT(0))
            },
            WM_SETTINGCHANGE|WM_THEMECHANGED|WM_DWMCOLORIZATIONCOLORCHANGED=>{s.theme=Theme::read();s.motion_enabled=motion_enabled();let _=DeleteObject(s.background_brush.into());s.background_brush=CreateSolidBrush(rgb(s.theme.surface));s.theme.window(s.main);if let Some(p)=s.popup{s.theme.window(p);let _=InvalidateRect(Some(p),None,false);}let _=InvalidateRect(Some(s.main),None,true);let _=InvalidateRect(Some(s.list),None,false);if s.popup.is_some(){let _=InvalidateRect(Some(s.app_list),None,false);}Some(LRESULT(0))},
            WM_CTLCOLOREDIT=>{let dc=HDC(wp.0 as *mut _);SetTextColor(dc,rgb(s.theme.text));SetBkColor(dc,rgb(s.theme.surface));Some(LRESULT(s.background_brush.0 as isize))},
            WM_DRAWITEM=>{
                let item=&*(lp.0 as *const DRAWITEMSTRUCT);
                let accent=item.CtlID==APPLY as u32;
                let pressed=item.itemState.0&ODS_SELECTED.0!=0;
                let c=if accent{s.theme.accent}else if pressed{s.theme.selected}else{s.theme.surface};
                let brush=CreateSolidBrush(rgb(c));let old_brush=SelectObject(item.hDC,brush.into());let old_pen=SelectObject(item.hDC,GetStockObject(NULL_PEN));
                let _=FillRect(item.hDC,&item.rcItem,s.background_brush);let _=RoundRect(item.hDC,item.rcItem.left,item.rcItem.top,item.rcItem.right,item.rcItem.bottom,10,10);
                SetBkMode(item.hDC,TRANSPARENT);let text_color=if accent{if c.r*0.299+c.g*0.587+c.b*0.114>0.62{color(20,24,30)}else{color(255,255,255)}}else{s.theme.text};SetTextColor(item.hDC,rgb(text_color));
                let old_font=SelectObject(item.hDC,s.font.into());let mut text=wide(&window_text(item.hwndItem));let mut r=item.rcItem;let _=DrawTextW(item.hDC,&mut text,&mut r,DT_CENTER|DT_VCENTER|DT_SINGLELINE|DT_END_ELLIPSIS);
                if item.itemState.0&ODS_FOCUS.0!=0{let _=DrawFocusRect(item.hDC,&item.rcItem);}
                SelectObject(item.hDC,old_font);SelectObject(item.hDC,old_brush);SelectObject(item.hDC,old_pen);let _=DeleteObject(brush.into());Some(LRESULT(1))
            },
            WM_TIMER=>{s.animation_tick(hwnd);Some(LRESULT(0))},
            WM_COMMAND=>{
                let id=wp.0&0xffff;let code=(wp.0>>16)&0xffff;
                if id==SEARCH_ID&&code==EN_CHANGE as usize{s.input_started=Some(Instant::now());s.filter(s.popup==Some(hwnd));}
                else{match id{APPLY=>s.apply(),CANCEL=>s.close_popup(),BROWSE=>s.browse(),SETTINGS=>s.open_settings(),UNDO=>s.undo(),_=>{}}}
                Some(LRESULT(0))
            },
            ACTION=>{
                let fly=s.popup==Some(hwnd);match wp.0 as u32{
                    VK_RETURN_CODE=>{if fly{s.apply();}else{s.open_popup();}},VK_ESCAPE_CODE=>{if fly{s.close_popup();}else{let _=SetWindowTextW(s.edit,w!(""));s.filter(false);}},
                    VK_UP_CODE|VK_DOWN_CODE=>{let list=if fly{s.app_list}else{s.list};let len=if fly{s.app_rows.len()}else{s.rows.len()};if len>0{let row=selected_index(list).unwrap_or(0);let next=if wp.0 as u32==VK_UP_CODE{row.saturating_sub(1)}else{(row+1).min(len-1)};select_row(list,next);s.remember_focus(fly);}},_=>{}
                }Some(LRESULT(0))
            },
            WM_NOTIFY=>{
                let header=&*(lp.0 as *const NMHDR);let fly=header.hwndFrom==s.app_list;
                if header.code==LVN_ITEMCHANGED{
                    let info=&*(lp.0 as *const NMLISTVIEW);
                    if info.iItem>=0&&info.uNewState&LVIS_SELECTED.0!=0{
                        s.selection_motion.insert(header.hwndFrom.0 as usize,Instant::now());if s.motion_enabled{let _=SetTimer(Some(header.hwndFrom),ROW_TIMER,15,None);}
                        let row=info.iItem as usize;
                        if fly{if let Some((i,_))=s.app_rows.get(row){s.app_focus=Some(s.choices[*i].identity().into());}}
                        else{if let Some((i,_))=s.rows.get(row){s.type_focus=Some(s.types[*i].extension.clone());}}
                    }
                }else if header.code==LVN_GETDISPINFOW{
                    let info=&mut *(lp.0 as *mut NMLVDISPINFOW);let row=info.item.iItem.max(0) as usize;
                    let rows=if fly{&s.app_rows}else{&s.rows};
                    if let Some((i,_))=rows.get(row){if info.item.mask&LVIF_TEXT!=LIST_VIEW_ITEM_FLAGS(0)&&!info.item.pszText.is_null()&&info.item.cchTextMax>0{
                        let label=if fly{let a=&s.choices[*i];format!("{} {} {}",a.name,a.publisher,if a.recommended{"recommended"}else{""})}else{let t=&s.types[*i];format!("{} {} {}",t.name,t.extension,t.current_app)};
                        let text=wide(&label);let n=text.len().min(info.item.cchTextMax.max(1) as usize)-1;std::ptr::copy_nonoverlapping(text.as_ptr(),info.item.pszText.0,n);*info.item.pszText.0.add(n)=0;
                    }}
                }else if header.code==NM_RETURN||header.code==NM_DBLCLK||(!fly&&header.code==NM_CLICK){let _=PostMessageW(Some(hwnd),ACTION,WPARAM(13),LPARAM(0));}
                Some(LRESULT(0))
            },
            WM_CLOSE=>{if hwnd==s.main{if s.busy{MessageBoxW(Some(hwnd),w!("An association change is being verified. Close the app after it completes."),w!("DefaultPicker"),MB_OK);return Some(LRESULT(0));}save_window(hwnd);s.workers.cancel.store(true,Ordering::Relaxed);let _=DestroyWindow(hwnd);PostQuitMessage(0);}else{s.close_popup();}Some(LRESULT(0))},
            WM_DESTROY=>{if hwnd==s.main{PostQuitMessage(0);}Some(LRESULT(0))},
            _=>None,
        }
    });
    handled.unwrap_or_else(|| DefWindowProcW(hwnd, msg, wp, lp))
}

unsafe fn save_window(hwnd: HWND) {
    let mut r = RECT::default();
    if GetWindowRect(hwnd, &mut r).is_ok() {
        let bytes: Vec<_> = [r.left, r.top, r.right, r.bottom]
            .into_iter()
            .flat_map(i32::to_le_bytes)
            .collect();
        let _ = cache::atomic_write("window.bin", &bytes);
    }
}
fn saved_window() -> Option<[i32; 4]> {
    let b = cache::read("window.bin").ok()?;
    if b.len() != 16 {
        return None;
    }
    let mut r = [0; 4];
    for (i, v) in r.iter_mut().enumerate() {
        *v = i32::from_le_bytes(b[i * 4..i * 4 + 4].try_into().ok()?);
    }
    if r[2] - r[0] < 300 || r[3] - r[1] < 300 || r[2] - r[0] > 5000 || r[3] - r[1] > 4000 {
        return None;
    }
    Some(r)
}
pub fn fatal(message: &str) {
    unsafe {
        MessageBoxW(
            None,
            PCWSTR(wide(message).as_ptr()),
            w!("DefaultPicker"),
            MB_OK | MB_ICONERROR,
        );
    }
}
pub fn run(start: Instant) -> Result<(), String> {
    unsafe {
        let _com = ComApartment::new();
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let _ = InitCommonControlsEx(&INITCOMMONCONTROLSEX {
            dwSize: size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_LISTVIEW_CLASSES,
        });
        let instance = GetModuleHandleW(None).map_err(|e| e.to_string())?;
        let class = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(window_proc),
            hInstance: instance.into(),
            hCursor: LoadCursorW(None, IDC_ARROW).map_err(|e| e.to_string())?,
            lpszClassName: w!("DefaultPickerWindow"),
            ..Default::default()
        };
        if RegisterClassW(&class) == 0 {
            return Err("Cannot register window class".into());
        }
        let dpi = GetDpiForSystem().max(96);
        let mut work = RECT::default();
        let _ = SystemParametersInfoW(
            SPI_GETWORKAREA,
            0,
            Some((&mut work as *mut RECT).cast()),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        );
        let (width, height) = ((760 * dpi / 96) as i32, (580 * dpi / 96) as i32);
        let mut r = saved_window().unwrap_or([
            (work.right - width) / 2,
            (work.bottom - height) / 2,
            (work.right + width) / 2,
            (work.bottom + height) / 2,
        ]);
        if r[2] < work.left + 100
            || r[0] > work.right - 100
            || r[3] < work.top + 100
            || r[1] > work.bottom - 100
        {
            r = [
                work.left + 40,
                work.top + 40,
                work.left + 40 + width,
                work.top + 40 + height,
            ];
        }
        let hwnd = CreateWindowExW(
            WS_EX_CONTROLPARENT,
            w!("DefaultPickerWindow"),
            w!("DefaultPicker"),
            WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN,
            r[0],
            r[1],
            r[2] - r[0],
            r[3] - r[1],
            None,
            None,
            Some(instance.into()),
            None,
        )
        .map_err(|e| e.to_string())?;
        STATE.with(|state| *state.borrow_mut() = Some(State::new(hwnd, start)));
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
        let _ = UpdateWindow(hwnd);
        STATE.with(|state| {
            if let Some(s) = state.borrow().as_ref() {
                let _ = SetFocus(Some(s.edit));
            }
        });
        let mut msg = MSG::default();
        loop {
            let status = GetMessageW(&mut msg, None, 0, 0).0;
            if status <= 0 {
                break;
            }
            let (dialog, busy) = STATE.with(|cell| {
                let borrow = cell.borrow();
                let s = borrow.as_ref();
                (
                    s.map(|s| s.popup.unwrap_or(s.main)),
                    s.is_some_and(|s| s.busy),
                )
            });
            if msg.message == WM_KEYDOWN
                && msg.wParam.0 == b'Z' as usize
                && (GetKeyState(0x11) as u16 & 0x8000) != 0
                && !busy
            {
                STATE.with(|s| {
                    if let Some(s) = s.borrow_mut().as_mut() {
                        s.undo();
                    }
                });
                continue;
            }
            if msg.message == WM_KEYDOWN && msg.wParam.0 == 27 {
                if let Some(target) = dialog {
                    let _ = PostMessageW(Some(target), ACTION, WPARAM(27), LPARAM(0));
                }
                continue;
            }
            if msg.message == WM_KEYDOWN && msg.wParam.0 == 13 {
                let focus = GetFocus();
                let id = GetDlgCtrlID(focus);
                if let Some(target) = dialog {
                    if [APPLY, CANCEL, BROWSE, SETTINGS, UNDO].contains(&(id as usize)) {
                        let _ =
                            PostMessageW(Some(target), WM_COMMAND, WPARAM(id as usize), LPARAM(0));
                    } else {
                        let _ = PostMessageW(Some(target), ACTION, WPARAM(13), LPARAM(0));
                    }
                }
                continue;
            }
            if let Some(dialog) = dialog {
                if IsDialogMessageW(dialog, &msg).as_bool() {
                    continue;
                }
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        STATE.with(|state| {
            if let Some(mut s) = state.borrow_mut().take() {
                s.workers.cancel.store(true, Ordering::Relaxed);
                let report = format!(
                    "max_filter_and_native_update_us={}\nworking_set_at_exit_bytes={}\npaint_count={}\ninput_to_list_enddraw_us={:?}\n",
                    s.filter_max_us,
                    memory().0,
                    s.paint_count,
                    s.paint_samples,
                );
                let _ = cache::atomic_write("session.txt", report.as_bytes());
                let startup=format!("entry_to_first_native_paint_us={}\nentry_to_first_gpu_paint_us={}\nworking_set_at_exit_bytes={}\npeak_working_set_bytes={}\nExcludes OS process creation; first native frame is interactive while GPU initializes on worker.\n",s.first_paint_us.unwrap_or(0),s.first_gpu_us.unwrap_or(0),memory().0,memory().1);
                let _=cache::atomic_write("startup.txt",startup.as_bytes());
                s.renderers.clear();
                let _ = DeleteObject(s.font.into());
                let _ = DeleteObject(s.background_brush.into());
            }
        });
        Ok(())
    }
}
