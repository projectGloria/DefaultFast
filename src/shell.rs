//! Thin wrappers over Shell / COM / process APIs. Every `unsafe` block here is a single OS call
//! (or a short sequence on one COM object) with owned Rust data in and out; no raw pointer
//! escapes this module.
#![allow(unsafe_code)]
use crate::error::{Error, Result};
use std::path::PathBuf;
use windows::{
    core::{w, Interface, HSTRING, PCWSTR, PWSTR},
    Win32::{
        Foundation::*,
        Graphics::Gdi::*,
        Security::{Authorization::ConvertSidToStringSidW, *},
        Storage::FileSystem::*,
        System::{
            Com::*, Environment::ExpandEnvironmentStringsW, ProcessStatus::*,
            SystemInformation::GetLocalTime, Threading::*,
        },
        UI::{Shell::{Common::COMDLG_FILTERSPEC, *}, WindowsAndMessaging::*},
    },
};

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

fn from_wide(s: &[u16]) -> String {
    let end = s.iter().position(|&c| c == 0).unwrap_or(s.len());
    String::from_utf16_lossy(&s[..end])
}

/// Frees a CoTaskMem string after copying it.
unsafe fn take_co_string(p: PWSTR) -> String {
    let s = p.to_string().unwrap_or_default();
    CoTaskMemFree(Some(p.0.cast()));
    s
}

/// COM apartment for the current thread, released on drop. Every thread that touches Shell
/// objects (scan, worker, icons, dialog) owns one.
pub struct Com(bool);
impl Com {
    pub fn init() -> Self {
        // SAFETY: plain apartment init; balanced by CoUninitialize in Drop when it succeeded.
        Self(unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE).is_ok() })
    }
}
impl Drop for Com {
    fn drop(&mut self) {
        if self.0 {
            // SAFETY: matches the successful CoInitializeEx on this thread.
            unsafe { CoUninitialize() }
        }
    }
}

fn local_time() -> windows::Win32::Foundation::SYSTEMTIME {
    // SAFETY: no inputs; returns a struct by value.
    unsafe { GetLocalTime() }
}

/// "2026-10-30 20:31:05.123" (local time) for logs and history.
pub fn timestamp() -> String {
    let t = local_time();
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond, t.wMilliseconds
    )
}

/// "20261030-203105-123": sortable, filename-safe.
pub fn file_stamp() -> String {
    let t = local_time();
    format!(
        "{:04}{:02}{:02}-{:02}{:02}{:02}-{:03}",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond, t.wMilliseconds
    )
}

pub fn expand_env(s: &str) -> String {
    if !s.contains('%') {
        return s.to_owned();
    }
    let input = wide(s);
    let mut out = vec![0u16; 4096];
    // SAFETY: `out` is a valid buffer; the return value is the required length incl. NUL.
    let n = unsafe { ExpandEnvironmentStringsW(PCWSTR(input.as_ptr()), Some(&mut out)) } as usize;
    if n == 0 || n > out.len() {
        return s.to_owned();
    }
    from_wide(&out)
}

/// Resolves "@shell32.dll,-1234" / "@{Package?ms-resource://…}" strings; others pass through.
pub fn load_indirect(s: &str) -> String {
    if !s.starts_with('@') {
        return s.to_owned();
    }
    let mut out = vec![0u16; 1024];
    let input = wide(s);
    // SAFETY: valid in/out buffers; the reserved parameter is None.
    if unsafe { SHLoadIndirectString(PCWSTR(input.as_ptr()), &mut out, None) }.is_ok() {
        from_wide(&out)
    } else {
        String::new()
    }
}

#[derive(Clone, Copy, Debug)]
pub enum AssocStr {
    ProgId,
    Executable,
    FriendlyApp,
    FriendlyDoc,
    Command,
}

/// What Shell *effectively* uses for an extension (".mp4") or protocol ("https"), after
/// UserChoice, HKCU/HKLM classes and defaults are resolved. This, not a registry readback, is
/// the source of truth for "did the change work".
pub fn assoc_query(target: &str, what: AssocStr) -> Option<String> {
    let kind = match what {
        AssocStr::ProgId => ASSOCSTR_PROGID,
        AssocStr::Executable => ASSOCSTR_EXECUTABLE,
        AssocStr::FriendlyApp => ASSOCSTR_FRIENDLYAPPNAME,
        AssocStr::FriendlyDoc => ASSOCSTR_FRIENDLYDOCNAME,
        AssocStr::Command => ASSOCSTR_COMMAND,
    };
    let flags = if target.starts_with('.') { ASSOCF_NONE } else { ASSOCF_IS_PROTOCOL };
    let t = wide(target);
    let mut buf = vec![0u16; 2048];
    let mut len = buf.len() as u32;
    // SAFETY: `buf` has `len` slots; extra/verb pointers are null.
    let r = unsafe {
        AssocQueryStringW(flags | ASSOCF_NOTRUNCATE, kind, PCWSTR(t.as_ptr()), PCWSTR::null(), Some(PWSTR(buf.as_mut_ptr())), &mut len)
    };
    if r.is_err() {
        return None;
    }
    let s = from_wide(&buf);
    (!s.is_empty()).then_some(s)
}

/// Tells Explorer to reload associations and icons; no explorer.exe restart needed.
pub fn notify_assoc_changed() {
    // SAFETY: SHCNE_ASSOCCHANGED takes no item pointers.
    unsafe { SHChangeNotify(SHCNE_ASSOCCHANGED, SHCNF_IDLIST, None, None) }
}

/// The current user's SID string ("S-1-5-21-…"), an input of the UserChoice hash.
pub fn user_sid() -> Result<String> {
    // SAFETY: token handle is closed below; the TOKEN_USER buffer is usize-aligned and sized
    // by the probe call; the SID string is LocalFree'd after copying.
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token)?;
        let mut size = 0u32;
        let _ = GetTokenInformation(token, TokenUser, None, 0, &mut size);
        let mut buf = vec![0usize; (size as usize).div_ceil(size_of::<usize>())];
        let r = GetTokenInformation(token, TokenUser, Some(buf.as_mut_ptr().cast()), size, &mut size);
        let _ = CloseHandle(token);
        r?;
        let user = &*(buf.as_ptr().cast::<TOKEN_USER>());
        let mut s = PWSTR::null();
        ConvertSidToStringSidW(user.User.Sid, &mut s)?;
        let out = s.to_string().unwrap_or_default();
        let _ = LocalFree(Some(HLOCAL(s.0.cast())));
        Ok(out)
    }
}

pub fn is_elevated() -> bool {
    // SAFETY: token handle closed below; TOKEN_ELEVATION is a plain struct out param.
    unsafe {
        let mut token = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return false;
        }
        let mut e = TOKEN_ELEVATION::default();
        let mut size = 0;
        let ok = GetTokenInformation(token, TokenElevation, Some((&mut e as *mut TOKEN_ELEVATION).cast()), size_of::<TOKEN_ELEVATION>() as u32, &mut size).is_ok();
        let _ = CloseHandle(token);
        ok && e.TokenIsElevated != 0
    }
}

/// (FileDescription, CompanyName) from an executable's version resource.
pub fn version_info(path: &str) -> (String, String) {
    let p = wide(path);
    // SAFETY: the version block is sized by GetFileVersionInfoSizeW; VerQueryValueW returns
    // pointers into that block, which is alive for the whole closure.
    unsafe {
        let n = GetFileVersionInfoSizeW(PCWSTR(p.as_ptr()), None);
        if n == 0 || n > 4 << 20 {
            return Default::default();
        }
        let mut data = vec![0u8; n as usize];
        if GetFileVersionInfoW(PCWSTR(p.as_ptr()), None, n, data.as_mut_ptr().cast()).is_err() {
            return Default::default();
        }
        let mut ptr = std::ptr::null_mut();
        let mut len = 0u32;
        let lang = if VerQueryValueW(data.as_ptr().cast(), w!("\\VarFileInfo\\Translation"), &mut ptr, &mut len).as_bool() && len >= 4 {
            let pair = std::slice::from_raw_parts(ptr.cast::<u16>(), 2);
            format!("{:04x}{:04x}", pair[0], pair[1])
        } else {
            "040904b0".into()
        };
        let get = |name: &str| {
            let key = wide(&format!("\\StringFileInfo\\{lang}\\{name}"));
            let mut ptr = std::ptr::null_mut();
            let mut len = 0u32;
            if VerQueryValueW(data.as_ptr().cast(), PCWSTR(key.as_ptr()), &mut ptr, &mut len).as_bool() && !ptr.is_null() && len > 0 && len < 4096 {
                from_wide(std::slice::from_raw_parts(ptr.cast::<u16>(), len as usize)).trim().to_owned()
            } else {
                String::new()
            }
        };
        (get("FileDescription"), get("CompanyName"))
    }
}

/// Per-user and all-users Start Menu "Programs" folders.
pub fn start_menu_dirs() -> Vec<PathBuf> {
    [FOLDERID_Programs, FOLDERID_CommonPrograms]
        .iter()
        // SAFETY: known-folder GUIDs are static; the returned string is CoTaskMemFree'd.
        .filter_map(|id| unsafe { SHGetKnownFolderPath(id, KNOWN_FOLDER_FLAG(0), None).ok().map(|p| PathBuf::from(take_co_string(p))) })
        .collect()
}

/// Resolves a .lnk to its target path without touching the network (SLGP_RAWPATH, no Resolve).
pub fn resolve_shortcut(path: &std::path::Path) -> Option<String> {
    // SAFETY: COM objects are reference counted by the bindings; buffers are local.
    unsafe {
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).ok()?;
        let file: IPersistFile = link.cast().ok()?;
        file.Load(&HSTRING::from(path.as_os_str()), STGM_READ).ok()?;
        let mut buf = vec![0u16; 1024];
        link.GetPath(&mut buf, std::ptr::null_mut(), SLGP_RAWPATH.0 as u32).ok()?;
        let s = expand_env(&from_wide(&buf));
        (!s.is_empty()).then_some(s)
    }
}

/// Packaged (UWP/MSIX) apps from the Shell AppsFolder: (display name, AppUserModelID).
/// Only parsing names of the form "PackageFamily!AppId" are packaged apps.
pub fn packaged_apps() -> Vec<(String, String)> {
    let mut out = Vec::new();
    // SAFETY: COM enumeration; every returned string is copied and freed.
    unsafe {
        let Ok(folder) = SHCreateItemInKnownFolder::<_, IShellItem>(&FOLDERID_AppsFolder, KNOWN_FOLDER_FLAG(0), PCWSTR::null()) else {
            return out;
        };
        let Ok(items) = folder.BindToHandler::<_, IEnumShellItems>(None, &BHID_EnumItems) else {
            return out;
        };
        loop {
            let mut item = [None];
            let mut fetched = 0;
            if items.Next(&mut item, Some(&mut fetched)).is_err() || fetched == 0 {
                break;
            }
            let Some(item) = item[0].take() else { continue };
            let id = item.GetDisplayName(SIGDN_PARENTRELATIVEPARSING).map(|p| take_co_string(p)).unwrap_or_default();
            if !id.contains('!') || id.contains('\\') {
                continue;
            }
            let name = item.GetDisplayName(SIGDN_NORMALDISPLAY).map(|p| take_co_string(p)).unwrap_or_default();
            if !name.is_empty() {
                out.push((name, id));
            }
        }
    }
    out
}

#[derive(Clone, Debug, Default)]
pub struct Handler {
    pub name: String,
    /// Executable path for desktop apps, or a ProgID/AUMID for packaged apps.
    pub identity: String,
    pub recommended: bool,
}

/// Handlers Shell would offer in "Open with" for an extension or protocol.
pub fn assoc_handlers(target: &str) -> Vec<Handler> {
    let mut out = Vec::new();
    let t = wide(target);
    for (filter, recommended) in [(ASSOC_FILTER_RECOMMENDED, true), (ASSOC_FILTER_NONE, false)] {
        // SAFETY: COM enumerator; strings copied and freed.
        unsafe {
            let en = if target.starts_with('.') {
                SHAssocEnumHandlers(PCWSTR(t.as_ptr()), filter)
            } else if recommended {
                SHAssocEnumHandlersForProtocolByApplication(PCWSTR(t.as_ptr()))
            } else {
                break;
            };
            let Ok(en) = en else { continue };
            loop {
                let mut item = [None];
                let mut fetched = 0;
                if en.Next(&mut item, Some(&mut fetched)).is_err() || fetched == 0 {
                    break;
                }
                let Some(h) = item[0].take() else { break };
                let identity = h.GetName().map(|p| take_co_string(p)).unwrap_or_default();
                if identity.is_empty() || out.iter().any(|o: &Handler| o.identity.eq_ignore_ascii_case(&identity)) {
                    continue;
                }
                let name = h.GetUIName().map(|p| take_co_string(p)).unwrap_or_default();
                out.push(Handler { name, identity, recommended });
            }
        }
    }
    out
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum IconSource {
    /// Executable or any file path.
    File(String),
    /// Generic icon for an extension (".pdf").
    Extension(String),
    /// Packaged app, via shell:AppsFolder\<AUMID>.
    Aumid(String),
}

/// Loads an icon as premultiplied RGBA. Runs on the icon thread (needs COM).
pub fn load_icon(src: &IconSource, size: u32) -> Option<(u32, u32, Vec<u8>)> {
    match src {
        IconSource::File(p) => image_factory(p, size).or_else(|| file_info_icon(p, false, size)),
        IconSource::Aumid(id) => image_factory(&format!("shell:AppsFolder\\{id}"), size),
        IconSource::Extension(e) => file_info_icon(e, true, size),
    }
}

fn image_factory(parsing: &str, size: u32) -> Option<(u32, u32, Vec<u8>)> {
    // SAFETY: COM object from a parsing name; the HBITMAP is deleted after copying pixels.
    unsafe {
        let item: IShellItemImageFactory = SHCreateItemFromParsingName(&HSTRING::from(parsing), None).ok()?;
        let bmp = item.GetImage(SIZE { cx: size as i32, cy: size as i32 }, SIIGBF_ICONONLY | SIIGBF_BIGGERSIZEOK).ok()?;
        let px = bitmap_pixels(bmp, true);
        let _ = DeleteObject(bmp.into());
        px
    }
}

fn file_info_icon(path: &str, by_extension: bool, size: u32) -> Option<(u32, u32, Vec<u8>)> {
    let p = wide(path);
    let mut info = SHFILEINFOW::default();
    let mut flags = SHGFI_ICON | if size > 16 { SHGFI_LARGEICON } else { SHGFI_SMALLICON };
    if by_extension {
        flags |= SHGFI_USEFILEATTRIBUTES;
    }
    // SAFETY: `info` is a valid out struct; the HICON is destroyed after copying pixels.
    unsafe {
        let r = SHGetFileInfoW(PCWSTR(p.as_ptr()), FILE_ATTRIBUTE_NORMAL, Some(&mut info), size_of::<SHFILEINFOW>() as u32, flags);
        if r == 0 || info.hIcon.is_invalid() {
            return None;
        }
        let px = icon_pixels(info.hIcon);
        let _ = DestroyIcon(info.hIcon);
        px
    }
}

unsafe fn icon_pixels(icon: HICON) -> Option<(u32, u32, Vec<u8>)> {
    let mut ii = ICONINFO::default();
    GetIconInfo(icon, &mut ii).ok()?;
    let px = bitmap_pixels(ii.hbmColor, false);
    let _ = DeleteObject(ii.hbmColor.into());
    let _ = DeleteObject(ii.hbmMask.into());
    px
}

/// Copies a 32-bpp DIB into top-down premultiplied RGBA.
unsafe fn bitmap_pixels(bmp: HBITMAP, premultiplied: bool) -> Option<(u32, u32, Vec<u8>)> {
    let mut bm = BITMAP::default();
    if GetObjectW(bmp.into(), size_of::<BITMAP>() as i32, Some((&mut bm as *mut BITMAP).cast())) == 0 {
        return None;
    }
    let (w, h) = (bm.bmWidth.unsigned_abs(), bm.bmHeight.unsigned_abs());
    if w == 0 || h == 0 || w > 512 || h > 512 {
        return None;
    }
    let mut info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w as i32,
            biHeight: -(h as i32), // negative = top-down rows
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut px = vec![0u8; (w * h * 4) as usize];
    let dc = GetDC(None);
    let lines = GetDIBits(dc, bmp, 0, h, Some(px.as_mut_ptr().cast()), &mut info, DIB_RGB_COLORS);
    ReleaseDC(None, dc);
    if lines == 0 {
        return None;
    }
    // Legacy 24-bit icons come back with alpha 0 everywhere; treat them as opaque.
    let has_alpha = px.chunks_exact(4).any(|p| p[3] != 0);
    for p in px.chunks_exact_mut(4) {
        p.swap(0, 2); // BGRA -> RGBA
        if !has_alpha {
            p[3] = 255;
        } else if !premultiplied {
            let a = p[3] as u16;
            for c in &mut p[..3] {
                *c = ((*c as u16 * a + 127) / 255) as u8;
            }
        }
    }
    Some((w, h, px))
}

/// Opens a URI (ms-settings:…) with the default handler.
pub fn open_uri(uri: &str) -> Result<()> {
    let u = wide(uri);
    // SAFETY: plain ShellExecuteW; values > 32 mean success.
    let r = unsafe { ShellExecuteW(None, w!("open"), PCWSTR(u.as_ptr()), PCWSTR::null(), PCWSTR::null(), SW_SHOWNORMAL) };
    if r.0 as isize > 32 {
        Ok(())
    } else {
        Err(Error::Invalid(format!("Could not open {uri}")))
    }
}

/// Relaunches this exe elevated ("runas") with `args` and waits for its exit code. Blocks:
/// call from a worker thread, never the UI thread.
pub fn run_elevated(args: &str) -> Result<u32> {
    let exe = std::env::current_exe()?;
    let file = HSTRING::from(exe.as_os_str());
    let params = HSTRING::from(args);
    let mut info = SHELLEXECUTEINFOW {
        cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI,
        lpVerb: w!("runas"),
        lpFile: PCWSTR(file.as_ptr()),
        lpParameters: PCWSTR(params.as_ptr()),
        nShow: SW_HIDE.0,
        ..Default::default()
    };
    // SAFETY: `info` and the HSTRINGs it points into live across the call; the process handle
    // is waited on and closed.
    unsafe {
        if let Err(e) = ShellExecuteExW(&mut info) {
            return Err(if e.code() == ERROR_CANCELLED.to_hresult() { Error::ElevationCancelled } else { e.into() });
        }
        if info.hProcess.is_invalid() {
            return Err(Error::Invalid("The elevated helper did not start".into()));
        }
        WaitForSingleObject(info.hProcess, 120_000);
        let mut code = 1u32;
        let _ = GetExitCodeProcess(info.hProcess, &mut code);
        let _ = CloseHandle(info.hProcess);
        Ok(code)
    }
}

/// Modal "pick an .exe" dialog. Runs its own message loop, so call it from a dedicated STA
/// thread with the main window as owner.
pub fn browse_exe(owner: isize) -> Option<String> {
    let _com = Com::init();
    // SAFETY: COM dialog; the result string is copied and freed.
    unsafe {
        let dialog: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
        let filters = [
            COMDLG_FILTERSPEC { pszName: w!("Programs"), pszSpec: w!("*.exe") },
        ];
        dialog.SetFileTypes(&filters).ok()?;
        dialog.SetTitle(w!("Choose a program")).ok()?;
        let opts = dialog.GetOptions().ok()?;
        dialog.SetOptions(opts | FOS_FILEMUSTEXIST | FOS_FORCEFILESYSTEM | FOS_NOCHANGEDIR).ok()?;
        dialog.Show(Some(HWND(owner as *mut _))).ok()?;
        let item = dialog.GetResult().ok()?;
        Some(take_co_string(item.GetDisplayName(SIGDN_FILESYSPATH).ok()?))
    }
}

/// Whether the UCPD driver (User Choice Protection Driver) is running. It blocks third-party
/// writes to http/https/.pdf UserChoice keys.
pub fn ucpd_running() -> bool {
    use windows::Win32::System::Services::*;
    // SAFETY: SCM handles are closed; status struct is passed as a sized byte slice.
    unsafe {
        let Ok(scm) = OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), SC_MANAGER_CONNECT) else {
            return true; // can't tell: assume protected and let verification decide
        };
        let svc = OpenServiceW(scm, w!("UCPD"), SERVICE_QUERY_STATUS);
        let _ = CloseServiceHandle(scm);
        let Ok(svc) = svc else { return false };
        let mut st = SERVICE_STATUS_PROCESS::default();
        let mut needed = 0;
        let bytes = std::slice::from_raw_parts_mut((&mut st as *mut SERVICE_STATUS_PROCESS).cast::<u8>(), size_of::<SERVICE_STATUS_PROCESS>());
        let r = QueryServiceStatusEx(svc, SC_STATUS_PROCESS_INFO, Some(bytes), &mut needed);
        let _ = CloseServiceHandle(svc);
        r.is_err() || st.dwCurrentState == SERVICE_RUNNING
    }
}

/// (working set, peak working set) in bytes.
pub fn memory() -> (usize, usize) {
    let mut m = PROCESS_MEMORY_COUNTERS { cb: size_of::<PROCESS_MEMORY_COUNTERS>() as u32, ..Default::default() };
    // SAFETY: `m` is a correctly sized out struct for the current process pseudo-handle.
    if unsafe { GetProcessMemoryInfo(GetCurrentProcess(), &mut m, m.cb) }.is_ok() {
        (m.WorkingSetSize, m.PeakWorkingSetSize)
    } else {
        (0, 0)
    }
}

/// Microseconds since this process was created (kernel timestamp), for startup measurement.
pub fn micros_since_process_start() -> u64 {
    let (mut c, mut e, mut k, mut u) = Default::default();
    // SAFETY: four FILETIME out params for the current process.
    unsafe {
        if GetProcessTimes(GetCurrentProcess(), &mut c, &mut e, &mut k, &mut u).is_err() {
            return 0;
        }
        let now = windows::Win32::System::SystemInformation::GetSystemTimeAsFileTime();
        let t = |f: FILETIME| ((f.dwHighDateTime as u64) << 32) | f.dwLowDateTime as u64;
        t(now).saturating_sub(t(c)) / 10
    }
}
