//! Audited operating-system boundary. Raw pointers, COM calls and window callbacks live only
//! in this module and its children. Every returned string/record is owned Rust data. Handles
//! are closed here; registry mutation wrappers only accept HKCU paths.
#![allow(unsafe_code)]
pub mod ui;
use crate::model::App;
use std::{
    mem::size_of,
    path::{Path, PathBuf},
};
use windows::{
    core::{w, Interface, PCWSTR, PWSTR},
    Win32::{
        Foundation::*,
        Security::{Authorization::*, Cryptography::*, *},
        Storage::FileSystem::*,
        System::{Com::*, ProcessStatus::*, Registry::*, Threading::*},
        UI::{Shell::*, WindowsAndMessaging::*},
    },
};

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}
fn from_wide(s: &[u16]) -> String {
    String::from_utf16_lossy(&s[..s.iter().position(|c| *c == 0).unwrap_or(s.len())])
}
fn error(context: &str, e: WIN32_ERROR) -> String {
    format!(
        "{context}: {} ({}). Use Windows Settings if access is denied.",
        windows::core::Error::from_hresult(e.to_hresult()),
        e.0
    )
}
#[derive(Clone, Copy)]
pub enum Root {
    Classes,
    User,
    Machine,
}
impl Root {
    fn key(self) -> HKEY {
        match self {
            Self::Classes => HKEY_CLASSES_ROOT,
            Self::User => HKEY_CURRENT_USER,
            Self::Machine => HKEY_LOCAL_MACHINE,
        }
    }
}
struct Key(HKEY);
impl Drop for Key {
    fn drop(&mut self) {
        unsafe {
            let _ = RegCloseKey(self.0);
        }
    }
}
fn open(root: Root, path: &str, access: REG_SAM_FLAGS) -> Option<Key> {
    unsafe {
        let mut k = HKEY::default();
        RegOpenKeyExW(
            root.key(),
            PCWSTR(wide(path).as_ptr()),
            Some(0),
            access,
            &mut k,
        )
        .ok()
        .ok()?;
        Some(Key(k))
    }
}
pub fn reg_key_exists(root: Root, path: &str) -> bool {
    open(root, path, KEY_READ).is_some()
}
pub fn reg_value_exists(root: Root, path: &str, name: &str) -> bool {
    reg_raw(root, path, name).is_some()
}
pub fn reg_raw(root: Root, path: &str, name: &str) -> Option<(u32, Vec<u8>)> {
    unsafe {
        let k = open(root, path, KEY_QUERY_VALUE)?;
        let n = wide(name);
        let mut size = 0;
        let mut kind = REG_VALUE_TYPE(0);
        RegQueryValueExW(
            k.0,
            PCWSTR(n.as_ptr()),
            None,
            Some(&mut kind),
            None,
            Some(&mut size),
        )
        .ok()
        .ok()?;
        if size > 65536 {
            return None;
        }
        let mut bytes = vec![0; size as usize];
        RegQueryValueExW(
            k.0,
            PCWSTR(n.as_ptr()),
            None,
            Some(&mut kind),
            Some(bytes.as_mut_ptr()),
            Some(&mut size),
        )
        .ok()
        .ok()?;
        bytes.truncate(size as usize);
        Some((kind.0, bytes))
    }
}
pub fn reg_string(root: Root, path: &str, name: &str) -> Option<String> {
    let (kind, b) = reg_raw(root, path, name)?;
    if kind != REG_SZ.0 && kind != REG_EXPAND_SZ.0 {
        return None;
    }
    let s = from_wide(
        &b.as_chunks::<2>()
            .0
            .iter()
            .map(|v| u16::from_le_bytes([v[0], v[1]]))
            .collect::<Vec<_>>(),
    );
    Some(if kind == REG_EXPAND_SZ.0 {
        expand(&s)
    } else {
        s
    })
}
pub fn subkeys(root: Root, path: &str) -> Vec<String> {
    unsafe {
        let Some(k) = open(root, path, KEY_ENUMERATE_SUB_KEYS) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        let mut index = 0;
        loop {
            let mut b = [0u16; 256];
            let mut n = b.len() as u32;
            let e = RegEnumKeyExW(
                k.0,
                index,
                Some(PWSTR(b.as_mut_ptr())),
                &mut n,
                None,
                None,
                None,
                None,
            );
            if e == ERROR_NO_MORE_ITEMS {
                break;
            }
            if e == ERROR_SUCCESS {
                out.push(from_wide(&b[..n as usize]));
            } else if e != ERROR_MORE_DATA {
                break;
            }
            index += 1;
        }
        out
    }
}
pub fn reg_values(root: Root, path: &str) -> Vec<(String, String)> {
    unsafe {
        let Some(k) = open(root, path, KEY_QUERY_VALUE) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        let mut index = 0;
        loop {
            let mut b = [0u16; 16384];
            let mut n = b.len() as u32;
            let e = RegEnumValueW(
                k.0,
                index,
                Some(PWSTR(b.as_mut_ptr())),
                &mut n,
                None,
                None,
                None,
                None,
            );
            if e == ERROR_NO_MORE_ITEMS {
                break;
            }
            if e == ERROR_SUCCESS {
                let name = from_wide(&b[..n as usize]);
                let value = reg_string(root, path, &name).unwrap_or_default();
                out.push((name, value));
            } else if e != ERROR_MORE_DATA {
                break;
            }
            index += 1;
        }
        out
    }
}
pub fn reg_time(root: Root, path: &str) -> Option<u64> {
    unsafe {
        let k = open(root, path, KEY_QUERY_VALUE)?;
        let mut t = FILETIME::default();
        RegQueryInfoKeyW(
            k.0,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Some(&mut t),
        )
        .ok()
        .ok()?;
        Some(((t.dwHighDateTime as u64) << 32) | t.dwLowDateTime as u64)
    }
}
pub fn reg_set(path: &str, name: &str, value: &str) -> Result<(), String> {
    let b: Vec<u8> = wide(value)
        .into_iter()
        .flat_map(|v| v.to_le_bytes())
        .collect();
    reg_write_raw(path, name, REG_SZ.0, &b)
}
fn reg_write_raw(path: &str, name: &str, kind: u32, bytes: &[u8]) -> Result<(), String> {
    unsafe {
        let mut k = HKEY::default();
        let e = RegCreateKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(wide(path).as_ptr()),
            None,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            None,
            &mut k,
            None,
        );
        if e != ERROR_SUCCESS {
            return Err(error("Create HKCU key", e));
        }
        let k = Key(k);
        let e = RegSetValueExW(
            k.0,
            PCWSTR(wide(name).as_ptr()),
            None,
            REG_VALUE_TYPE(kind),
            Some(bytes),
        );
        e.ok().map_err(|_| error("Write HKCU value", e))
    }
}
pub fn reg_restore(path: &str, name: &str, value: Option<&(u32, Vec<u8>)>) -> Result<(), String> {
    unsafe {
        if let Some((kind, b)) = value {
            reg_write_raw(path, name, *kind, b)
        } else {
            let Some(k) = open(Root::User, path, KEY_SET_VALUE) else {
                return Ok(());
            };
            let e = RegDeleteValueW(k.0, PCWSTR(wide(name).as_ptr()));
            if e == ERROR_FILE_NOT_FOUND {
                Ok(())
            } else {
                e.ok().map_err(|_| error("Restore HKCU value", e))
            }
        }
    }
}
pub fn reg_delete_tree(path: &str) -> Result<(), String> {
    unsafe {
        // RegDeleteKeyW needs DELETE on the child, unlike RegDeleteTree on a denied UserChoice.
        // UserChoice contains values only. Never remove any parent or relax an ACL.
        let e = RegDeleteKeyW(HKEY_CURRENT_USER, PCWSTR(wide(path).as_ptr()));
        if e == ERROR_FILE_NOT_FOUND || e == ERROR_PATH_NOT_FOUND {
            Ok(())
        } else {
            e.ok().map_err(|_| error("Delete HKCU UserChoice", e))
        }
    }
}
pub fn expand(s: &str) -> String {
    unsafe {
        let input = wide(s);
        let mut out = vec![0; 32768];
        let n = windows::Win32::System::Environment::ExpandEnvironmentStringsW(
            PCWSTR(input.as_ptr()),
            Some(&mut out),
        );
        if n > 0 && (n as usize) <= out.len() {
            from_wide(&out)
        } else {
            s.into()
        }
    }
}
pub fn resolve_resource(s: &str) -> String {
    if !s.starts_with('@') {
        return s.into();
    }
    unsafe {
        let mut out = vec![0; 4096];
        if SHLoadIndirectString(PCWSTR(wide(s).as_ptr()), &mut out, None).is_ok() {
            from_wide(&out)
        } else {
            s.into()
        }
    }
}
fn assoc_string(ext: &str, kind: ASSOCSTR) -> Option<String> {
    unsafe {
        let mut b = vec![0u16; 32768];
        let mut n = b.len() as u32;
        let result = AssocQueryStringW(
            ASSOCF_NONE,
            kind,
            PCWSTR(wide(ext).as_ptr()),
            PCWSTR::null(),
            Some(PWSTR(b.as_mut_ptr())),
            &mut n,
        );
        if result.is_ok() {
            Some(from_wide(&b))
        } else {
            None
        }
    }
}
pub fn effective(ext: &str, app: bool) -> Option<String> {
    assoc_string(
        ext,
        if app {
            ASSOCSTR_FRIENDLYAPPNAME
        } else {
            ASSOCSTR_PROGID
        },
    )
}
pub fn effective_command(ext: &str) -> Option<String> {
    assoc_string(ext, ASSOCSTR_COMMAND)
}
pub fn friendly_type(ext: &str) -> Option<String> {
    assoc_string(ext, ASSOCSTR_FRIENDLYDOCNAME).map(|s| resolve_resource(&s))
}
pub fn notify_associations() {
    unsafe { SHChangeNotify(SHCNE_ASSOCCHANGED, SHCNF_IDLIST, None, None) }
}
/// Test-only namespace cleanup. Refuses any real extension, application, or shared parent.
pub fn remove_self_test_key(path: &str) -> Result<(), String> {
    unsafe {
        let ext = format!(".defaultpicker-selftest-{}", std::process::id());
        let prefix = format!(
            "Software\\Classes\\DefaultPicker.SelfTest.{}.",
            std::process::id()
        );
        if path != format!("Software\\Classes\\{ext}")
            && !path.starts_with(&prefix)
            && !(path.starts_with("Software\\Classes\\DefaultPicker.")
                && path.ends_with(&ext)
                && !path
                    .trim_start_matches("Software\\Classes\\")
                    .contains('\\'))
            && path
                != format!(
                    "Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\FileExts\\{ext}"
                )
        {
            return Err("Refused cleanup outside this process's isolated test namespace".into());
        }
        let e = RegDeleteTreeW(HKEY_CURRENT_USER, PCWSTR(wide(path).as_ptr()));
        if e == ERROR_FILE_NOT_FOUND || e == ERROR_PATH_NOT_FOUND {
            Ok(())
        } else {
            e.ok().map_err(|_| error("Clean up test key", e))
        }
    }
}
pub fn command_executable(command: &str) -> Option<String> {
    let expanded = expand(command);
    let s = expanded.trim();
    let candidate = if let Some(rest) = s.strip_prefix('"') {
        rest.split('"').next()?
    } else {
        let lower = s.to_lowercase();
        if let Some(end) = lower.find(".exe") {
            s.get(..end + 4)?
        } else {
            s.split_whitespace().next()?
        }
    };
    if candidate.contains('%') || candidate.is_empty() {
        return None;
    }
    let candidate = candidate.trim_matches('"');
    if candidate.starts_with('\\') || candidate.starts_with('/') {
        return None;
    }
    if Path::new(candidate).is_absolute() {
        return if local_path(candidate) && Path::new(candidate).is_file() {
            Some(candidate.into())
        } else {
            None
        };
    }
    if candidate.contains(['\\', '/']) {
        return None;
    }
    let candidate = if Path::new(candidate).extension().is_none() {
        format!("{candidate}.exe")
    } else {
        candidate.into()
    };
    let mut paths = Vec::new();
    if let Some(system) = std::env::var_os("SystemRoot") {
        paths.push(PathBuf::from(&system).join("System32"));
        paths.push(PathBuf::from(system));
    }
    if let Some(environment) = std::env::var_os("PATH") {
        paths.extend(std::env::split_paths(&environment));
    }
    paths
        .into_iter()
        .filter(|p| local_path(&p.to_string_lossy()))
        .map(|p| p.join(&candidate))
        .find(|p| p.is_file())
        .map(|p| p.to_string_lossy().into_owned())
}
pub fn local_path(path: &str) -> bool {
    let p = path.as_bytes();
    if p.len() < 3 || p[1] != b':' || !p[0].is_ascii_alphabetic() || !b"\\/".contains(&p[2]) {
        return false;
    }
    unsafe { GetDriveTypeW(PCWSTR(wide(&format!("{}:\\", p[0] as char)).as_ptr())) != 4 }
}
pub fn executable_metadata(path: &str) -> (String, String) {
    if !local_path(path) {
        return (String::new(), String::new());
    }
    unsafe {
        let mut unused = 0;
        let path = wide(path);
        let n = GetFileVersionInfoSizeW(PCWSTR(path.as_ptr()), Some(&mut unused));
        if n == 0 || n > 1024 * 1024 {
            return (String::new(), String::new());
        }
        let mut data = vec![0u8; n as usize];
        if GetFileVersionInfoW(PCWSTR(path.as_ptr()), Some(0), n, data.as_mut_ptr().cast()).is_err()
        {
            return (String::new(), String::new());
        }
        let mut p = std::ptr::null_mut();
        let mut size = 0;
        let language = if VerQueryValueW(
            data.as_ptr().cast(),
            w!("\\VarFileInfo\\Translation"),
            &mut p,
            &mut size,
        )
        .as_bool()
            && size >= 4
        {
            let pair = std::slice::from_raw_parts(p.cast::<u16>(), 2);
            format!("{:04x}{:04x}", pair[0], pair[1])
        } else {
            "040904b0".into()
        };
        let query = |name: &str| {
            let mut p = std::ptr::null_mut();
            let mut n = 0;
            let key = wide(&format!("\\StringFileInfo\\{language}\\{name}"));
            if VerQueryValueW(data.as_ptr().cast(), PCWSTR(key.as_ptr()), &mut p, &mut n).as_bool()
                && !p.is_null()
                && n < 32768
            {
                from_wide(std::slice::from_raw_parts(p.cast::<u16>(), n as usize))
            } else {
                String::new()
            }
        };
        (query("FileDescription"), query("CompanyName"))
    }
}
pub fn lowercase_windows(text: &mut [u16]) {
    unsafe {
        CharLowerBuffW(text);
    }
}
pub fn md5(input: &[u8]) -> Result<[u8; 16], String> {
    unsafe {
        let mut alg = BCRYPT_ALG_HANDLE::default();
        BCryptOpenAlgorithmProvider(
            &mut alg,
            BCRYPT_MD5_ALGORITHM,
            PCWSTR::null(),
            BCRYPT_OPEN_ALGORITHM_PROVIDER_FLAGS(0),
        )
        .ok()
        .map_err(|e| e.to_string())?;
        let mut out = [0; 16];
        let result = BCryptHash(alg, None, input, &mut out);
        let _ = BCryptCloseAlgorithmProvider(alg, 0);
        result.ok().map_err(|e| e.to_string())?;
        Ok(out)
    }
}
pub fn sid() -> Result<String, String> {
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token)
            .map_err(|e| e.to_string())?;
        let mut size = 0;
        let _ = GetTokenInformation(token, TokenUser, None, 0, &mut size);
        // usize-aligned allocation for TOKEN_USER and SID data.
        let mut data = vec![0usize; (size as usize + size_of::<usize>() - 1) / size_of::<usize>()];
        let result = GetTokenInformation(
            token,
            TokenUser,
            Some(data.as_mut_ptr().cast()),
            size,
            &mut size,
        );
        let _ = CloseHandle(token);
        result.map_err(|e| e.to_string())?;
        let user = &*(data.as_ptr().cast::<TOKEN_USER>());
        let mut string = PWSTR::null();
        ConvertSidToStringSidW(user.User.Sid, &mut string).map_err(|e| e.to_string())?;
        let result = string.to_string().map_err(|e| e.to_string());
        let _ = LocalFree(Some(HLOCAL(string.0.cast())));
        result
    }
}
pub fn replace_file(from: &Path, to: &Path) -> Result<(), String> {
    unsafe {
        MoveFileExW(
            PCWSTR(wide(&from.to_string_lossy()).as_ptr()),
            PCWSTR(wide(&to.to_string_lossy()).as_ptr()),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
        .map_err(|e| e.to_string())
    }
}
pub fn ucpd_running() -> bool {
    use windows::Win32::System::Services::*;
    unsafe {
        let Ok(manager) = OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), SC_MANAGER_CONNECT) else {
            return reg_key_exists(Root::Machine, "SYSTEM\\CurrentControlSet\\Services\\UCPD");
        };
        let service = OpenServiceW(manager, w!("UCPD"), SERVICE_QUERY_STATUS);
        let _ = CloseServiceHandle(manager);
        let Ok(service) = service else {
            return false;
        };
        let mut state = SERVICE_STATUS_PROCESS::default();
        let mut needed = 0;
        let bytes = std::slice::from_raw_parts_mut(
            (&mut state as *mut SERVICE_STATUS_PROCESS).cast::<u8>(),
            size_of::<SERVICE_STATUS_PROCESS>(),
        );
        let result =
            QueryServiceStatusEx(service, SC_STATUS_PROCESS_INFO, Some(bytes), &mut needed);
        let _ = CloseServiceHandle(service);
        // If querying status fails, conservatively use Settings instead of probing registry writes.
        result.is_err() || state.dwCurrentState == SERVICE_RUNNING
    }
}
pub struct ComApartment(bool);
impl ComApartment {
    pub fn new() -> Self {
        Self(unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED).is_ok() })
    }
}
impl Drop for ComApartment {
    fn drop(&mut self) {
        if self.0 {
            unsafe {
                CoUninitialize();
            }
        }
    }
}
unsafe fn owned_com_string(s: PWSTR) -> String {
    let result = s.to_string().unwrap_or_default();
    CoTaskMemFree(Some(s.0.cast()));
    result
}
pub fn start_menu_dirs() -> Vec<PathBuf> {
    unsafe {
        [FOLDERID_Programs, FOLDERID_CommonPrograms]
            .iter()
            .filter_map(|id| {
                SHGetKnownFolderPath(id, KNOWN_FOLDER_FLAG(0), None)
                    .ok()
                    .map(|s| PathBuf::from(owned_com_string(s)))
            })
            .collect()
    }
}
pub fn shortcut(path: &Path) -> Option<App> {
    unsafe {
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).ok()?;
        let file: IPersistFile = link.cast().ok()?;
        file.Load(PCWSTR(wide(&path.to_string_lossy()).as_ptr()), STGM_READ)
            .ok()?;
        let mut b = vec![0u16; 32768];
        let mut data = WIN32_FIND_DATAW::default();
        link.GetPath(&mut b, &mut data, SLGP_RAWPATH.0 as u32)
            .ok()?;
        let executable = command_executable(&format!("\"{}\"", from_wide(&b)))?;
        let mut icon = vec![0u16; 32768];
        let mut index = 0;
        let _ = link.GetIconLocation(&mut icon, &mut index);
        Some(App {
            name: path.file_stem()?.to_string_lossy().into_owned(),
            icon: if icon[0] == 0 {
                executable.clone()
            } else {
                format!("{},{}", from_wide(&icon), index)
            },
            executable,
            ..App::default()
        })
    }
}
pub fn recommended_apps(ext: &str) -> Vec<App> {
    unsafe {
        let mut out = Vec::new();
        let Ok(en) = SHAssocEnumHandlers(PCWSTR(wide(ext).as_ptr()), ASSOC_FILTER_NONE) else {
            return out;
        };
        loop {
            let mut items = [None];
            let mut count = 0;
            let result = en.Next(&mut items, Some(&mut count));
            if count == 0 || result.is_err() {
                break;
            }
            let Some(handler) = items[0].take() else {
                break;
            };
            let name = handler
                .GetUIName()
                .ok()
                .map(|s| owned_com_string(s))
                .unwrap_or_default();
            let identity = handler
                .GetName()
                .ok()
                .map(|s| owned_com_string(s))
                .unwrap_or_default();
            // IAssocHandler::GetName can be an executable OR an AppX ProgID, never guess one from UI text.
            let executable = command_executable(&format!("\"{identity}\"")).unwrap_or_default();
            let prog_id = if !identity.contains(':') && reg_key_exists(Root::Classes, &identity) {
                identity
            } else {
                String::new()
            };
            let mut icon = PWSTR::null();
            let mut index = 0;
            let icon = if handler.GetIconLocation(&mut icon, &mut index).is_ok() {
                format!("{},{}", owned_com_string(icon), index)
            } else {
                executable.clone()
            };
            out.push(App {
                name,
                executable,
                prog_id,
                icon,
                recommended: true,
                ..App::default()
            });
        }
        out
    }
}
pub fn shell_apps() -> Vec<App> {
    unsafe {
        let mut out = Vec::new();
        let Ok(folder) = SHCreateItemInKnownFolder::<_, IShellItem>(
            &FOLDERID_AppsFolder,
            KNOWN_FOLDER_FLAG(0),
            PCWSTR::null(),
        ) else {
            return out;
        };
        let Ok(en) = folder.BindToHandler::<_, IEnumShellItems>(None, &BHID_EnumItems) else {
            return out;
        };
        loop {
            let mut item = [None];
            let mut n = 0;
            if en.Next(&mut item, Some(&mut n)).is_err() || n == 0 {
                break;
            }
            let Some(item) = item[0].take() else {
                continue;
            };
            let mut name = item
                .GetDisplayName(SIGDN_NORMALDISPLAY)
                .ok()
                .map(|s| owned_com_string(s))
                .unwrap_or_default();
            let identity = item
                .GetDisplayName(SIGDN_DESKTOPABSOLUTEPARSING)
                .ok()
                .map(|s| owned_com_string(s))
                .unwrap_or_default();
            if !identity.is_empty() {
                // Terminal's packaged shell identity has no file-handler ProgID. Its registered
                // App Path is the supported launcher, also used by the batch-command alternative.
                let executable = if identity
                    .to_lowercase()
                    .contains("microsoft.windowsterminal_")
                {
                    name = "Windows Terminal".into();
                    [Root::User, Root::Machine]
                        .into_iter()
                        .find_map(|root| {
                            reg_string(
                                root,
                                "Software\\Microsoft\\Windows\\CurrentVersion\\App Paths\\wt.exe",
                                "",
                            )
                        })
                        .unwrap_or_default()
                } else {
                    String::new()
                };
                let publisher = if executable.is_empty() {
                    "Installed app".into()
                } else {
                    executable_metadata(&executable).1
                };
                out.push(App {
                    name,
                    icon: identity,
                    executable,
                    publisher,
                    ..App::default()
                });
            }
        }
        out
    }
}
pub fn memory() -> (usize, usize) {
    unsafe {
        let mut m = PROCESS_MEMORY_COUNTERS {
            cb: size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
            ..Default::default()
        };
        if GetProcessMemoryInfo(
            GetCurrentProcess(),
            &mut m,
            size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        )
        .is_ok()
        {
            (m.WorkingSetSize, m.PeakWorkingSetSize)
        } else {
            (0, 0)
        }
    }
}

/// A blocking registry-notification thread: no polling and no periodic wakeups.
pub fn watch(
    cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    mut changed: impl FnMut() + Send + 'static,
) {
    std::thread::spawn(move || unsafe {
        let paths = [
            (Root::User, "Software\\Classes"),
            (Root::Machine, "Software\\Classes"),
            (
                Root::User,
                "Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\FileExts",
            ),
            (Root::User, "Software\\RegisteredApplications"),
            (Root::Machine, "Software\\RegisteredApplications"),
            (
                Root::User,
                "Software\\Microsoft\\Windows\\CurrentVersion\\App Paths",
            ),
            (
                Root::Machine,
                "Software\\Microsoft\\Windows\\CurrentVersion\\App Paths",
            ),
        ];
        let mut keys = Vec::new();
        let mut events = Vec::new();
        for (root, path) in paths {
            if let Some(k) = open(root, path, KEY_NOTIFY) {
                if let Ok(e) = CreateEventW(None, false, false, None) {
                    if RegNotifyChangeKeyValue(
                        k.0,
                        true,
                        REG_NOTIFY_CHANGE_NAME | REG_NOTIFY_CHANGE_LAST_SET,
                        Some(e),
                        true,
                    ) == ERROR_SUCCESS
                    {
                        keys.push(k);
                        events.push(e);
                    } else {
                        let _ = CloseHandle(e);
                    }
                }
            }
        }
        while !events.is_empty() && !cancel.load(std::sync::atomic::Ordering::Relaxed) {
            let result = WaitForMultipleObjects(&events, false, INFINITE);
            let i = result.0.wrapping_sub(WAIT_OBJECT_0.0) as usize;
            if i >= events.len() {
                break;
            }
            // Re-arm before refreshing so edits during a scan trigger another refresh.
            let _ = RegNotifyChangeKeyValue(
                keys[i].0,
                true,
                REG_NOTIFY_CHANGE_NAME | REG_NOTIFY_CHANGE_LAST_SET,
                Some(events[i]),
                true,
            );
            changed();
        }
        for e in events {
            let _ = CloseHandle(e);
        }
    });
}
