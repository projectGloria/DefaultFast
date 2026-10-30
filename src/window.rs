//! Custom window chrome on top of Slint's winit window, keeping a *real* native frame.
//!
//! How it works:
//! 1. A thread-local CBT hook catches `HCBT_CREATEWND` for our top-level window and subclasses
//!    it before its first `WM_NCCALCSIZE`, so the stock caption never flashes.
//! 2. `WM_NCCALCSIZE` keeps DefWindowProc's left/right/bottom frame (native resize borders,
//!    shadow, Aero Snap) but moves the client top up to the window top: our Slint top bar
//!    replaces the caption.
//! 3. `WM_NCHITTEST` maps the top bar: empty area -> HTCAPTION (drag, double-click maximize,
//!    right-click system menu), maximize button -> HTMAXBUTTON (Windows 11 Snap Layouts
//!    flyout), top edge -> HTTOP*, buttons/icon -> HTCLIENT so Slint handles them.
//!
//! Hit-test geometry must match `ui/app.slint` (CAPTION_H, BUTTON_W, ICON_W).
#![allow(unsafe_code)]
use std::cell::{Cell, RefCell};
use windows::{
    core::{BOOL, PCWSTR},
    Win32::{
        Foundation::*,
        Graphics::{Dwm::*, Gdi::*},
        System::{LibraryLoader::GetModuleHandleW, Threading::GetCurrentThreadId},
        UI::{
            Controls::MARGINS,
            HiDpi::{GetDpiForWindow, GetSystemMetricsForDpi},
            Input::KeyboardAndMouse::{TrackMouseEvent, TME_LEAVE, TME_NONCLIENT, TRACKMOUSEEVENT},
            Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass},
            WindowsAndMessaging::*,
        },
    },
};

pub const CAPTION_H: f32 = 36.0;
pub const BUTTON_W: f32 = 46.0;
pub const ICON_W: f32 = 44.0;

/// Notifications from the window procedure. Delivered through the sink, which defers them to
/// the Slint event loop so we never re-enter winit/Slint from inside a window message.
#[derive(Clone, Copy, Debug)]
pub enum ChromeEvent {
    MaxHover(bool),
    Maximized(bool),
    /// Light/dark or accent changed.
    ThemeChanged,
    /// WM_SETTINGCHANGE (any): association/environment caches may be stale.
    SettingChanged,
    Activated,
}

thread_local! {
    static MAIN: Cell<isize> = const { Cell::new(0) };
    static HOOK: Cell<isize> = const { Cell::new(0) };
    static TITLE: RefCell<Vec<u16>> = const { RefCell::new(Vec::new()) };
    static SINK: RefCell<Option<Box<dyn Fn(ChromeEvent)>>> = const { RefCell::new(None) };
    static MAX_HOVER: Cell<bool> = const { Cell::new(false) };
    static ZOOMED: Cell<bool> = const { Cell::new(false) };
    static DARK: Cell<bool> = const { Cell::new(false) };
}

fn emit(e: ChromeEvent) {
    SINK.with(|s| {
        if let Some(f) = s.borrow().as_ref() {
            f(e)
        }
    });
}

pub fn hwnd() -> isize {
    MAIN.with(Cell::get)
}

fn main_hwnd() -> Option<HWND> {
    let h = hwnd();
    (h != 0).then_some(HWND(h as *mut _))
}

/// Arms the CBT hook. Call on the UI thread before the Slint window is shown.
pub fn install(title: &str, dark: bool, sink: impl Fn(ChromeEvent) + 'static) {
    TITLE.with(|t| *t.borrow_mut() = title.encode_utf16().collect());
    SINK.with(|s| *s.borrow_mut() = Some(Box::new(sink)));
    DARK.with(|d| d.set(dark));
    // SAFETY: thread-local CBT hook (dwThreadId = this thread), removed once our window exists.
    unsafe {
        if let Ok(h) = SetWindowsHookExW(WH_CBT, Some(cbt_proc), None, GetCurrentThreadId()) {
            HOOK.with(|c| c.set(h.0 as isize));
        }
    }
}

fn unhook() {
    let h = HOOK.with(|c| c.replace(0));
    if h != 0 {
        // SAFETY: handle from SetWindowsHookExW on this thread.
        unsafe {
            let _ = UnhookWindowsHookEx(HHOOK(h as *mut _));
        }
    }
}

unsafe extern "system" fn cbt_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HCBT_CREATEWND as i32 && hwnd() == 0 {
        let cbt = &*(lparam.0 as *const CBT_CREATEWNDW);
        let cs = &*cbt.lpcs;
        let top_level = cs.hwndParent.is_invalid() && (cs.style as u32 & WS_CAPTION.0) == WS_CAPTION.0;
        let name = if cs.lpszName.is_null() { &[][..] } else { cs.lpszName.as_wide() };
        if top_level && TITLE.with(|t| t.borrow().as_slice() == name) {
            let h = HWND(wparam.0 as *mut _);
            MAIN.with(|m| m.set(h.0 as isize));
            let _ = SetWindowSubclass(h, Some(subclass_proc), 1, 0);
        }
    }
    CallNextHookEx(None, code, wparam, lparam)
}

fn dpi_scale(h: HWND) -> f32 {
    // SAFETY: valid window handle.
    (unsafe { GetDpiForWindow(h) }.max(96)) as f32 / 96.0
}

/// Top resize band height in physical pixels (what the stripped top frame used to provide).
fn frame_y(h: HWND) -> i32 {
    // SAFETY: plain metric queries.
    unsafe {
        let dpi = GetDpiForWindow(h).max(96);
        GetSystemMetricsForDpi(SM_CYFRAME, dpi) + GetSystemMetricsForDpi(SM_CXPADDEDBORDER, dpi)
    }
}

fn zoomed(h: HWND) -> bool {
    // SAFETY: valid window handle.
    unsafe { IsZoomed(h).as_bool() }
}

unsafe fn apply_dwm(h: HWND) {
    let dark = BOOL::from(DARK.with(Cell::get));
    let _ = DwmSetWindowAttribute(h, DWMWA_USE_IMMERSIVE_DARK_MODE, (&dark as *const BOOL).cast(), size_of::<BOOL>() as u32);
    // Windows 11 only; both calls fail harmlessly on Windows 10.
    let round = DWMWCP_ROUND;
    let _ = DwmSetWindowAttribute(h, DWMWA_WINDOW_CORNER_PREFERENCE, (&round as *const DWM_WINDOW_CORNER_PREFERENCE).cast(), size_of::<DWM_WINDOW_CORNER_PREFERENCE>() as u32);
    let backdrop = DWMSBT_MAINWINDOW;
    let _ = DwmSetWindowAttribute(h, DWMWA_SYSTEMBACKDROP_TYPE, (&backdrop as *const DWM_SYSTEMBACKDROP_TYPE).cast(), size_of::<DWM_SYSTEMBACKDROP_TYPE>() as u32);
    // 1 px of extended frame at the top restores the DWM top border/shadow on Windows 10.
    let m = MARGINS { cxLeftWidth: 0, cxRightWidth: 0, cyTopHeight: 1, cyBottomHeight: 0 };
    let _ = DwmExtendFrameIntoClientArea(h, &m);
}

unsafe fn set_icon(h: HWND) {
    // winresource stores the app icon as resource id 1.
    let Ok(module) = GetModuleHandleW(None) else { return };
    for (which, metric) in [(ICON_BIG, SM_CXICON), (ICON_SMALL, SM_CXSMICON)] {
        let size = GetSystemMetricsForDpi(metric, GetDpiForWindow(h).max(96));
        if let Ok(icon) = LoadImageW(Some(module.into()), PCWSTR(1 as *const u16), IMAGE_ICON, size, size, LR_DEFAULTCOLOR) {
            SendMessageW(h, WM_SETICON, Some(WPARAM(which as usize)), Some(LPARAM(icon.0 as isize)));
        }
    }
}

unsafe extern "system" fn subclass_proc(h: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM, _id: usize, _data: usize) -> LRESULT {
    match msg {
        WM_CREATE => {
            let r = DefSubclassProc(h, msg, wparam, lparam);
            apply_dwm(h);
            set_icon(h);
            return r;
        }
        WM_NCCALCSIZE if wparam.0 != 0 => {
            let params = &mut *(lparam.0 as *mut NCCALCSIZE_PARAMS);
            let top = params.rgrc[0].top;
            let r = DefSubclassProc(h, msg, wparam, lparam);
            if r.0 != 0 {
                return r;
            }
            // Maximized windows overhang the monitor by the frame; keep the client on-screen.
            params.rgrc[0].top = top + if zoomed(h) { frame_y(h) } else { 0 };
            return LRESULT(0);
        }
        WM_NCHITTEST => {
            let r = DefSubclassProc(h, msg, wparam, lparam);
            if r.0 as u32 != HTCLIENT {
                return r; // left/right/bottom borders from the real frame
            }
            let mut pt = POINT { x: (lparam.0 & 0xffff) as i16 as i32, y: ((lparam.0 >> 16) & 0xffff) as i16 as i32 };
            let _ = ScreenToClient(h, &mut pt);
            let mut rc = RECT::default();
            let _ = GetClientRect(h, &mut rc);
            let s = dpi_scale(h);
            let w = rc.right;
            if !zoomed(h) && pt.y < frame_y(h) {
                let corner = (BUTTON_W * s) as i32 / 2;
                return LRESULT(if pt.x < corner { HTTOPLEFT } else if pt.x >= w - corner { HTTOPRIGHT } else { HTTOP } as isize);
            }
            if (pt.y as f32) < CAPTION_H * s {
                let bw = BUTTON_W * s;
                let x = pt.x as f32;
                let w = w as f32;
                let ht = if x >= w - 2.0 * bw && x < w - bw {
                    HTMAXBUTTON
                } else if x >= w - 3.0 * bw || x < ICON_W * s {
                    HTCLIENT // close, minimize and app icon are Slint buttons
                } else {
                    HTCAPTION
                };
                return LRESULT(ht as isize);
            }
            return LRESULT(HTCLIENT as isize);
        }
        // The maximize button is non-client (for Snap Layouts), so Slint never sees the mouse
        // there: track hover/press here and mirror it into the UI.
        WM_NCMOUSEMOVE => {
            let over = wparam.0 as u32 == HTMAXBUTTON;
            if over != MAX_HOVER.with(|c| c.replace(over)) {
                emit(ChromeEvent::MaxHover(over));
            }
            if over {
                let mut t = TRACKMOUSEEVENT { cbSize: size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE | TME_NONCLIENT, hwndTrack: h, dwHoverTime: 0 };
                let _ = TrackMouseEvent(&mut t);
                return LRESULT(0);
            }
        }
        WM_NCMOUSELEAVE | WM_MOUSEMOVE => {
            if MAX_HOVER.with(|c| c.replace(false)) {
                emit(ChromeEvent::MaxHover(false));
            }
        }
        // Swallow presses on HTMAXBUTTON: DefWindowProc would paint a Windows 7 style button.
        WM_NCLBUTTONDOWN | WM_NCLBUTTONDBLCLK if wparam.0 as u32 == HTMAXBUTTON => return LRESULT(0),
        WM_NCLBUTTONUP if wparam.0 as u32 == HTMAXBUTTON => {
            toggle_maximize();
            return LRESULT(0);
        }
        // Alt+Space: show the system menu even if the UI layer swallows SYSCHAR.
        WM_SYSCHAR if wparam.0 == ' ' as usize => {
            return DefWindowProcW(h, WM_SYSCOMMAND, WPARAM(SC_KEYMENU as usize), LPARAM(' ' as isize));
        }
        WM_SIZE => {
            let z = wparam.0 as u32 == SIZE_MAXIMIZED;
            if wparam.0 as u32 != SIZE_MINIMIZED && z != ZOOMED.with(|c| c.replace(z)) {
                emit(ChromeEvent::Maximized(z));
            }
        }
        WM_SETTINGCHANGE => {
            let what = if lparam.0 != 0 { PCWSTR(lparam.0 as *const u16).to_string().unwrap_or_default() } else { String::new() };
            if what == "ImmersiveColorSet" {
                emit(ChromeEvent::ThemeChanged);
            }
            emit(ChromeEvent::SettingChanged);
        }
        WM_DWMCOLORIZATIONCOLORCHANGED => emit(ChromeEvent::ThemeChanged),
        WM_ACTIVATE if (wparam.0 & 0xffff) as u32 != WA_INACTIVE => emit(ChromeEvent::Activated),
        WM_NCDESTROY => {
            let _ = RemoveWindowSubclass(h, Some(subclass_proc), 1);
            MAIN.with(|m| m.set(0));
        }
        _ => {}
    }
    DefSubclassProc(h, msg, wparam, lparam)
}

/// Called once the window exists: removes the creation hook.
pub fn ready() -> bool {
    if hwnd() != 0 {
        unhook();
        true
    } else {
        false
    }
}

pub fn set_dark(dark: bool) {
    DARK.with(|d| d.set(dark));
    if let Some(h) = main_hwnd() {
        // SAFETY: attribute updates on our live window.
        unsafe { apply_dwm(h) }
    }
}

pub fn minimize() {
    if let Some(h) = main_hwnd() {
        // SAFETY: valid window handle.
        unsafe {
            let _ = ShowWindow(h, SW_MINIMIZE);
        }
    }
}

pub fn toggle_maximize() {
    if let Some(h) = main_hwnd() {
        // SAFETY: valid window handle.
        unsafe {
            let _ = ShowWindow(h, if zoomed(h) { SW_RESTORE } else { SW_MAXIMIZE });
        }
    }
}

pub fn is_maximized() -> bool {
    main_hwnd().is_some_and(zoomed)
}

pub fn close() {
    if let Some(h) = main_hwnd() {
        // SAFETY: posting WM_CLOSE lets winit/Slint run their normal close path.
        unsafe {
            let _ = PostMessageW(Some(h), WM_CLOSE, WPARAM(0), LPARAM(0));
        }
    }
}

/// System menu under the app icon (click on the icon, like stock windows).
pub fn show_system_menu() {
    let Some(h) = main_hwnd() else { return };
    // SAFETY: menu handle belongs to the window; the chosen command is posted back to it.
    unsafe {
        let menu = GetSystemMenu(h, false);
        if menu.is_invalid() {
            return;
        }
        let s = dpi_scale(h);
        let mut pt = POINT { x: (8.0 * s) as i32, y: (CAPTION_H * s) as i32 };
        let _ = ClientToScreen(h, &mut pt);
        let z = zoomed(h);
        let state = |on: bool| if on { MF_ENABLED } else { MF_GRAYED };
        let _ = EnableMenuItem(menu, SC_RESTORE, MF_BYCOMMAND | state(z));
        let _ = EnableMenuItem(menu, SC_MAXIMIZE, MF_BYCOMMAND | state(!z));
        let _ = EnableMenuItem(menu, SC_MOVE, MF_BYCOMMAND | state(!z));
        let _ = EnableMenuItem(menu, SC_SIZE, MF_BYCOMMAND | state(!z));
        let cmd = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_LEFTALIGN | TPM_TOPALIGN, pt.x, pt.y, None, h, None);
        if cmd.0 != 0 {
            let _ = PostMessageW(Some(h), WM_SYSCOMMAND, WPARAM(cmd.0 as usize), LPARAM(0));
        }
    }
}

/// Physical pixels that winit adds above the requested inner size (caption + top frame via
/// AdjustWindowRectEx) and that our WM_NCCALCSIZE hands back to the client. Subtracting it
/// from a requested client height makes the restored window exactly the saved size.
pub fn top_extra_px(x: i32, y: i32) -> i32 {
    use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
    // SAFETY: pure queries; the monitor handle is never null with DEFAULTTONEAREST.
    unsafe {
        let mon = MonitorFromPoint(POINT { x, y }, MONITOR_DEFAULTTONEAREST);
        let (mut dx, mut dy) = (96u32, 96u32);
        let _ = GetDpiForMonitor(mon, MDT_EFFECTIVE_DPI, &mut dx, &mut dy);
        GetSystemMetricsForDpi(SM_CYCAPTION, dy) + GetSystemMetricsForDpi(SM_CYFRAME, dy) + GetSystemMetricsForDpi(SM_CXPADDEDBORDER, dy)
    }
}

/// DPI scale of the monitor nearest to a point (before our window exists).
pub fn scale_at(x: i32, y: i32) -> f32 {
    use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
    // SAFETY: as above.
    unsafe {
        let mon = MonitorFromPoint(POINT { x, y }, MONITOR_DEFAULTTONEAREST);
        let (mut dx, mut dy) = (96u32, 96u32);
        let _ = GetDpiForMonitor(mon, MDT_EFFECTIVE_DPI, &mut dx, &mut dy);
        dy.max(96) as f32 / 96.0
    }
}

/// True when the rectangle (physical px) is mostly on some monitor; guards restored placement
/// after a monitor was unplugged.
pub fn rect_visible(x: i32, y: i32, w: i32, h: i32) -> bool {
    let r = RECT { left: x, top: y, right: x + w, bottom: y + h };
    // SAFETY: pure query.
    let mon = unsafe { MonitorFromRect(&r, MONITOR_DEFAULTTONULL) };
    if mon.is_invalid() {
        return false;
    }
    let mut info = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
    // SAFETY: valid monitor handle and sized struct.
    if !unsafe { GetMonitorInfoW(mon, &mut info) }.as_bool() {
        return false;
    }
    let wa = info.rcWork;
    let ix = (r.right.min(wa.right) - r.left.max(wa.left)).max(0);
    let iy = (r.bottom.min(wa.bottom) - r.top.max(wa.top)).max(0);
    ix * iy * 2 >= w * h
}
