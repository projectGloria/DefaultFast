//! Registry access behind a trait so association logic (UserChoice, repair, backup) is unit
//! testable against `MemRegistry`. `WinRegistry` is the only implementation that touches the OS;
//! its `unsafe` is limited to the raw Reg* calls. Every write passes `check_write` (an allowlist
//! of file-association keys) and is logged.
use crate::error::{Error, Result};
use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Hive {
    CurrentUser,
    LocalMachine,
    /// The merged HKCU\Software\Classes + HKLM\Software\Classes view. Read-only by policy:
    /// writing through HKCR silently picks a hive, which is exactly the ambiguity we avoid.
    ClassesRoot,
}

impl Hive {
    pub fn reg_name(self) -> &'static str {
        match self {
            Hive::CurrentUser => "HKEY_CURRENT_USER",
            Hive::LocalMachine => "HKEY_LOCAL_MACHINE",
            Hive::ClassesRoot => "HKEY_CLASSES_ROOT",
        }
    }
    pub fn parse(s: &str) -> Option<Hive> {
        match s.to_ascii_uppercase().as_str() {
            "HKEY_CURRENT_USER" | "HKCU" => Some(Hive::CurrentUser),
            "HKEY_LOCAL_MACHINE" | "HKLM" => Some(Hive::LocalMachine),
            "HKEY_CLASSES_ROOT" | "HKCR" => Some(Hive::ClassesRoot),
            _ => None,
        }
    }
}

impl fmt::Display for Hive {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Hive::CurrentUser => "HKCU",
            Hive::LocalMachine => "HKLM",
            Hive::ClassesRoot => "HKCR",
        })
    }
}

pub const REG_NONE: u32 = 0;
pub const REG_SZ: u32 = 1;
pub const REG_EXPAND_SZ: u32 = 2;
pub const REG_BINARY: u32 = 3;
pub const REG_DWORD: u32 = 4;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Value {
    Sz(String),
    /// Kept unexpanded; callers expand when they need a path.
    ExpandSz(String),
    Dword(u32),
    /// Anything else (REG_NONE, REG_BINARY, REG_MULTI_SZ, QWORD...), preserved byte-exact so
    /// backups round-trip values we don't interpret.
    Raw(u32, Vec<u8>),
}

impl Value {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Sz(s) | Value::ExpandSz(s) => Some(s),
            _ => None,
        }
    }
    pub fn kind(&self) -> u32 {
        match self {
            Value::Sz(_) => REG_SZ,
            Value::ExpandSz(_) => REG_EXPAND_SZ,
            Value::Dword(_) => REG_DWORD,
            Value::Raw(k, _) => *k,
        }
    }
    /// Wire bytes as RegSetValueExW expects them (strings are UTF-16LE with NUL).
    pub fn to_bytes(&self) -> Vec<u8> {
        match self {
            Value::Sz(s) | Value::ExpandSz(s) => s
                .encode_utf16()
                .chain(Some(0))
                .flat_map(|u| u.to_le_bytes())
                .collect(),
            Value::Dword(d) => d.to_le_bytes().to_vec(),
            Value::Raw(_, b) => b.clone(),
        }
    }
    pub fn from_bytes(kind: u32, bytes: &[u8]) -> Value {
        match kind {
            REG_SZ | REG_EXPAND_SZ => {
                let units: Vec<u16> = bytes
                    .chunks_exact(2)
                    .map(|c| u16::from_le_bytes([c[0], c[1]]))
                    .collect();
                let end = units.iter().position(|&u| u == 0).unwrap_or(units.len());
                let s = String::from_utf16_lossy(&units[..end]);
                if kind == REG_SZ {
                    Value::Sz(s)
                } else {
                    Value::ExpandSz(s)
                }
            }
            REG_DWORD if bytes.len() == 4 => {
                Value::Dword(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
            }
            _ => Value::Raw(kind, bytes.to_vec()),
        }
    }
}

/// Read + write registry surface used by all association logic. `Send + Sync` so one instance
/// can be shared by the scan, worker and icon threads.
pub trait Registry: Send + Sync {
    fn get(&self, hive: Hive, path: &str, name: &str) -> Option<Value>;
    fn values(&self, hive: Hive, path: &str) -> Vec<(String, Value)>;
    fn subkeys(&self, hive: Hive, path: &str) -> Vec<String>;
    fn exists(&self, hive: Hive, path: &str) -> bool;
    /// Key last-write time as a FILETIME (100 ns ticks since 1601), which the UserChoice
    /// hash depends on.
    fn last_write(&self, hive: Hive, path: &str) -> Option<u64>;
    fn create_key(&self, hive: Hive, path: &str) -> Result<()>;
    fn set(&self, hive: Hive, path: &str, name: &str, value: &Value) -> Result<()>;
    fn delete_value(&self, hive: Hive, path: &str, name: &str) -> Result<()>;
    /// Deletes the key and every subkey. Missing keys are not an error.
    fn delete_tree(&self, hive: Hive, path: &str) -> Result<()>;

    fn string(&self, hive: Hive, path: &str, name: &str) -> Option<String> {
        self.get(hive, path, name)
            .and_then(|v| v.as_str().map(str::to_owned))
    }
}

/// Write allowlist. DefaultFast may only change file-association keys:
/// - HKCU\Software\Classes\.<ext>\…, HKCU\Software\Classes\Applications\<exe>\…
/// - HKCU\…\Explorer\FileExts\…, HKCU\…\Shell\Associations\UrlAssociations\…
/// - HKLM\Software\Classes\.<ext> itself (the default value, via the elevated helper)
pub fn check_write(hive: Hive, path: &str) -> Result<()> {
    let p = path.to_ascii_lowercase();
    let after = |prefix: &str| p.strip_prefix(prefix).filter(|rest| !rest.is_empty());
    let ok = match hive {
        Hive::CurrentUser => {
            after("software\\classes\\.").is_some()
                || after("software\\classes\\applications\\").is_some()
                || after("software\\microsoft\\windows\\currentversion\\explorer\\fileexts\\")
                    .is_some()
                || after("software\\microsoft\\windows\\shell\\associations\\urlassociations\\")
                    .is_some()
        }
        Hive::LocalMachine => after("software\\classes\\.").is_some_and(|ext| !ext.contains('\\')),
        Hive::ClassesRoot => false,
    };
    if ok && !p.contains("\\\\") && !p.ends_with('\\') {
        Ok(())
    } else {
        Err(Error::Forbidden(format!("{hive}\\{path}")))
    }
}

// ---------------------------------------------------------------------------------------------
// Win32 implementation
// ---------------------------------------------------------------------------------------------

pub use win::WinRegistry;

#[allow(unsafe_code)]
mod win {
    use super::*;
    use crate::logfile;
    use windows::{
        core::{PCWSTR, PWSTR},
        Win32::{Foundation::*, System::Registry::*},
    };

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(Some(0)).collect()
    }

    fn root(h: Hive) -> HKEY {
        match h {
            Hive::CurrentUser => HKEY_CURRENT_USER,
            Hive::LocalMachine => HKEY_LOCAL_MACHINE,
            Hive::ClassesRoot => HKEY_CLASSES_ROOT,
        }
    }

    /// RAII HKEY: closed on drop so early returns can't leak handles.
    struct Key(HKEY);
    impl Drop for Key {
        fn drop(&mut self) {
            // SAFETY: self.0 came from a successful RegOpen/CreateKeyEx and is closed once.
            unsafe {
                let _ = RegCloseKey(self.0);
            }
        }
    }

    fn open(h: Hive, path: &str, access: REG_SAM_FLAGS) -> std::result::Result<Key, WIN32_ERROR> {
        let mut k = HKEY::default();
        let p = wide(path);
        // SAFETY: `p` is NUL-terminated and outlives the call; `k` is a valid out pointer.
        let e = unsafe { RegOpenKeyExW(root(h), PCWSTR(p.as_ptr()), Some(0), access, &mut k) };
        if e == ERROR_SUCCESS {
            Ok(Key(k))
        } else {
            Err(e)
        }
    }

    fn fail(op: &'static str, h: Hive, path: &str, e: WIN32_ERROR) -> Error {
        Error::Registry {
            op,
            path: format!("{h}\\{path}"),
            source: windows::core::Error::from_hresult(e.to_hresult()),
        }
    }

    fn missing(e: WIN32_ERROR) -> bool {
        e == ERROR_FILE_NOT_FOUND || e == ERROR_PATH_NOT_FOUND
    }

    /// Logs every mutation with its outcome; the log is the audit trail next to the backups.
    fn logged(what: String, r: Result<()>) -> Result<()> {
        match &r {
            Ok(()) => logfile::write(&format!("OK   {what}")),
            Err(e) => logfile::write(&format!("FAIL {what}: {e}")),
        }
        r
    }

    #[derive(Default)]
    pub struct WinRegistry;

    impl WinRegistry {
        fn query(k: &Key, name: &str) -> Option<Value> {
            let n = wide(name);
            let mut kind = REG_VALUE_TYPE(0);
            let mut size = 0u32;
            // SAFETY: size-probe call; all pointers are valid locals.
            let e = unsafe {
                RegQueryValueExW(k.0, PCWSTR(n.as_ptr()), None, Some(&mut kind), None, Some(&mut size))
            };
            if e != ERROR_SUCCESS || size > 1 << 20 {
                return None;
            }
            let mut buf = vec![0u8; size as usize];
            // SAFETY: `buf` holds exactly `size` bytes, as reported by the probe.
            let e = unsafe {
                RegQueryValueExW(
                    k.0,
                    PCWSTR(n.as_ptr()),
                    None,
                    Some(&mut kind),
                    Some(buf.as_mut_ptr()),
                    Some(&mut size),
                )
            };
            if e != ERROR_SUCCESS {
                return None;
            }
            buf.truncate(size as usize);
            Some(Value::from_bytes(kind.0, &buf))
        }

        fn delete_tree_inner(h: Hive, path: &str) -> std::result::Result<(), WIN32_ERROR> {
            // Manual recursion with RegDeleteKeyExW instead of RegDeleteTreeW: UserChoice keys
            // carry a "deny SetValue" ACE, and RegDeleteTree's value-enumeration access can
            // trip over it. RegDeleteKeyEx only needs DELETE on each child.
            for child in WinRegistry.subkeys(h, path) {
                Self::delete_tree_inner(h, &format!("{path}\\{child}"))?;
            }
            let p = wide(path);
            // SAFETY: NUL-terminated path; KEY_WOW64_64KEY targets the native view.
            let e = unsafe { RegDeleteKeyExW(root(h), PCWSTR(p.as_ptr()), KEY_WOW64_64KEY.0, None) };
            if e == ERROR_SUCCESS || missing(e) {
                Ok(())
            } else {
                Err(e)
            }
        }
    }

    impl Registry for WinRegistry {
        fn get(&self, hive: Hive, path: &str, name: &str) -> Option<Value> {
            let k = open(hive, path, KEY_QUERY_VALUE).ok()?;
            Self::query(&k, name)
        }

        fn values(&self, hive: Hive, path: &str) -> Vec<(String, Value)> {
            let Ok(k) = open(hive, path, KEY_QUERY_VALUE) else {
                return Vec::new();
            };
            let mut out = Vec::new();
            let mut name = vec![0u16; 16384];
            for index in 0.. {
                let mut len = name.len() as u32;
                // SAFETY: `name` has `len` u16 slots; other out params are unused.
                let e = unsafe {
                    RegEnumValueW(k.0, index, Some(PWSTR(name.as_mut_ptr())), &mut len, None, None, None, None)
                };
                if e == ERROR_NO_MORE_ITEMS {
                    break;
                }
                if e != ERROR_SUCCESS {
                    continue;
                }
                let n = String::from_utf16_lossy(&name[..len as usize]);
                if let Some(v) = Self::query(&k, &n) {
                    out.push((n, v));
                }
                if index > 100_000 {
                    break;
                }
            }
            out
        }

        fn subkeys(&self, hive: Hive, path: &str) -> Vec<String> {
            let Ok(k) = open(hive, path, KEY_ENUMERATE_SUB_KEYS) else {
                return Vec::new();
            };
            let mut out = Vec::new();
            let mut name = [0u16; 256]; // registry key names are capped at 255 chars
            for index in 0.. {
                let mut len = name.len() as u32;
                // SAFETY: `name` has `len` u16 slots; class/time outputs are unused.
                let e = unsafe {
                    RegEnumKeyExW(k.0, index, Some(PWSTR(name.as_mut_ptr())), &mut len, None, None, None, None)
                };
                if e == ERROR_NO_MORE_ITEMS {
                    break;
                }
                if e == ERROR_SUCCESS {
                    out.push(String::from_utf16_lossy(&name[..len as usize]));
                }
                if index > 1_000_000 {
                    break;
                }
            }
            out
        }

        fn exists(&self, hive: Hive, path: &str) -> bool {
            open(hive, path, KEY_QUERY_VALUE).is_ok()
        }

        fn last_write(&self, hive: Hive, path: &str) -> Option<u64> {
            let k = open(hive, path, KEY_QUERY_VALUE).ok()?;
            let mut t = FILETIME::default();
            // SAFETY: only the last-write-time out parameter is requested.
            let e = unsafe {
                RegQueryInfoKeyW(k.0, None, None, None, None, None, None, None, None, None, None, Some(&mut t))
            };
            (e == ERROR_SUCCESS).then(|| ((t.dwHighDateTime as u64) << 32) | t.dwLowDateTime as u64)
        }

        fn create_key(&self, hive: Hive, path: &str) -> Result<()> {
            let r = check_write(hive, path).and_then(|_| {
                let mut k = HKEY::default();
                let p = wide(path);
                // SAFETY: NUL-terminated path, valid out pointer; key is closed via `Key`.
                let e = unsafe {
                    RegCreateKeyExW(root(hive), PCWSTR(p.as_ptr()), None, PCWSTR::null(),
                        REG_OPTION_NON_VOLATILE, KEY_SET_VALUE, None, &mut k, None)
                };
                if e != ERROR_SUCCESS {
                    return Err(fail("Create key", hive, path, e));
                }
                drop(Key(k));
                Ok(())
            });
            logged(format!("CREATE {hive}\\{path}"), r)
        }

        fn set(&self, hive: Hive, path: &str, name: &str, value: &Value) -> Result<()> {
            let r = check_write(hive, path).and_then(|_| {
                let mut k = HKEY::default();
                let p = wide(path);
                // SAFETY: as in create_key.
                let e = unsafe {
                    RegCreateKeyExW(root(hive), PCWSTR(p.as_ptr()), None, PCWSTR::null(),
                        REG_OPTION_NON_VOLATILE, KEY_SET_VALUE, None, &mut k, None)
                };
                if e != ERROR_SUCCESS {
                    return Err(fail("Open for write", hive, path, e));
                }
                let k = Key(k);
                let n = wide(name);
                let bytes = value.to_bytes();
                // SAFETY: `bytes` is the complete value payload for the declared type.
                let e = unsafe {
                    RegSetValueExW(k.0, PCWSTR(n.as_ptr()), None, REG_VALUE_TYPE(value.kind()), Some(&bytes))
                };
                if e == ERROR_SUCCESS { Ok(()) } else { Err(fail("Write value", hive, path, e)) }
            });
            logged(format!("SET {hive}\\{path} [{}] = {value:?}", display_name(name)), r)
        }

        fn delete_value(&self, hive: Hive, path: &str, name: &str) -> Result<()> {
            let r = check_write(hive, path).and_then(|_| {
                let k = match open(hive, path, KEY_SET_VALUE) {
                    Ok(k) => k,
                    Err(e) if missing(e) => return Ok(()),
                    Err(e) => return Err(fail("Open for delete", hive, path, e)),
                };
                let n = wide(name);
                // SAFETY: NUL-terminated value name on an open key.
                let e = unsafe { RegDeleteValueW(k.0, PCWSTR(n.as_ptr())) };
                if e == ERROR_SUCCESS || missing(e) { Ok(()) } else { Err(fail("Delete value", hive, path, e)) }
            });
            logged(format!("DELVAL {hive}\\{path} [{}]", display_name(name)), r)
        }

        fn delete_tree(&self, hive: Hive, path: &str) -> Result<()> {
            let r = check_write(hive, path).and_then(|_| {
                Self::delete_tree_inner(hive, path).map_err(|e| fail("Delete key", hive, path, e))
            });
            logged(format!("DELKEY {hive}\\{path}"), r)
        }
    }

    fn display_name(name: &str) -> &str {
        if name.is_empty() { "(Default)" } else { name }
    }
}

// ---------------------------------------------------------------------------------------------
// In-memory implementation for tests
// ---------------------------------------------------------------------------------------------

#[cfg(test)]
pub use mem::MemRegistry;

#[cfg(test)]
mod mem {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    #[derive(Default, Clone)]
    struct Node {
        values: BTreeMap<String, (String, Value)>,
        time: u64,
    }

    /// Case-insensitive in-memory registry. HKCR reads merge HKCU\Software\Classes over
    /// HKLM\Software\Classes at key level, like Windows does for the merged view.
    #[derive(Default)]
    pub struct MemRegistry {
        keys: Mutex<BTreeMap<(Hive, String), (String, Node)>>,
        clock: Mutex<u64>,
        /// Paths (lowercase prefixes) whose writes fail with ACCESS_DENIED, to test rollback.
        pub deny: Mutex<Vec<String>>,
    }

    impl MemRegistry {
        pub fn new() -> Self {
            let r = Self::default();
            *r.clock.lock().unwrap() = 133_000_000_000_000_000;
            r
        }
        /// Test setup helper that bypasses the write policy (e.g. to seed HKLM state).
        pub fn seed(&self, hive: Hive, path: &str, name: &str, value: Value) {
            self.put(hive, path, name, value);
        }
        pub fn seed_key(&self, hive: Hive, path: &str) {
            self.ensure(hive, path);
        }
        /// Advances the fake clock (FILETIME ticks) used for last-write times.
        pub fn advance(&self, ticks: u64) {
            *self.clock.lock().unwrap() += ticks;
        }
        fn now(&self) -> u64 {
            let mut c = self.clock.lock().unwrap();
            *c += 10_000; // 1 ms per write
            *c
        }
        fn ensure(&self, hive: Hive, path: &str) {
            let now = self.now();
            let mut keys = self.keys.lock().unwrap();
            let mut acc = String::new();
            for part in path.split('\\') {
                if !acc.is_empty() {
                    acc.push('\\');
                }
                acc.push_str(part);
                keys.entry((hive, acc.to_lowercase()))
                    .or_insert_with(|| (acc.clone(), Node { time: now, ..Node::default() }));
            }
        }
        fn put(&self, hive: Hive, path: &str, name: &str, value: Value) {
            self.ensure(hive, path);
            let now = self.now();
            let mut keys = self.keys.lock().unwrap();
            let node = &mut keys.get_mut(&(hive, path.to_lowercase())).unwrap().1;
            node.values.insert(name.to_lowercase(), (name.to_owned(), value));
            node.time = now;
        }
        fn denied(&self, hive: Hive, path: &str) -> Result<()> {
            let p = path.to_lowercase();
            if self.deny.lock().unwrap().iter().any(|d| p.starts_with(d)) {
                return Err(Error::Registry {
                    op: "Write value",
                    path: format!("{hive}\\{path}"),
                    source: windows::core::Error::from_hresult(windows::core::HRESULT(0x8007_0005u32 as i32)),
                });
            }
            Ok(())
        }
        /// Resolves an HKCR path to the concrete hive that backs it.
        fn resolve(&self, hive: Hive, path: &str) -> (Hive, String) {
            if hive != Hive::ClassesRoot {
                return (hive, path.to_owned());
            }
            let user = format!("Software\\Classes\\{path}");
            let keys = self.keys.lock().unwrap();
            if keys.contains_key(&(Hive::CurrentUser, user.to_lowercase())) {
                (Hive::CurrentUser, user)
            } else {
                (Hive::LocalMachine, format!("Software\\Classes\\{path}"))
            }
        }
        fn node(&self, hive: Hive, path: &str) -> Option<Node> {
            let (h, p) = self.resolve(hive, path);
            self.keys.lock().unwrap().get(&(h, p.to_lowercase())).map(|n| n.1.clone())
        }
        fn children(&self, hive: Hive, path: &str) -> Vec<String> {
            let prefix = if path.is_empty() { String::new() } else { format!("{}\\", path.to_lowercase()) };
            let keys = self.keys.lock().unwrap();
            keys.iter()
                .filter(|((h, k), _)| *h == hive && k.starts_with(&prefix) && !k[prefix.len()..].contains('\\') && k.len() > prefix.len())
                .map(|(_, (orig, _))| orig[prefix.len()..].to_owned())
                .collect()
        }
    }

    impl Registry for MemRegistry {
        fn get(&self, hive: Hive, path: &str, name: &str) -> Option<Value> {
            self.node(hive, path)?.values.get(&name.to_lowercase()).map(|v| v.1.clone())
        }
        fn values(&self, hive: Hive, path: &str) -> Vec<(String, Value)> {
            self.node(hive, path)
                .map(|n| n.values.into_values().collect())
                .unwrap_or_default()
        }
        fn subkeys(&self, hive: Hive, path: &str) -> Vec<String> {
            if hive == Hive::ClassesRoot {
                let mut out = self.children(Hive::LocalMachine, &format!("Software\\Classes\\{path}").trim_end_matches('\\').to_owned());
                for k in self.children(Hive::CurrentUser, &format!("Software\\Classes\\{path}").trim_end_matches('\\').to_owned()) {
                    if !out.iter().any(|o| o.eq_ignore_ascii_case(&k)) {
                        out.push(k);
                    }
                }
                return out;
            }
            self.children(hive, path)
        }
        fn exists(&self, hive: Hive, path: &str) -> bool {
            self.node(hive, path).is_some()
        }
        fn last_write(&self, hive: Hive, path: &str) -> Option<u64> {
            self.node(hive, path).map(|n| n.time)
        }
        fn create_key(&self, hive: Hive, path: &str) -> Result<()> {
            check_write(hive, path)?;
            self.denied(hive, path)?;
            self.ensure(hive, path);
            Ok(())
        }
        fn set(&self, hive: Hive, path: &str, name: &str, value: &Value) -> Result<()> {
            check_write(hive, path)?;
            self.denied(hive, path)?;
            self.put(hive, path, name, value.clone());
            Ok(())
        }
        fn delete_value(&self, hive: Hive, path: &str, name: &str) -> Result<()> {
            check_write(hive, path)?;
            self.denied(hive, path)?;
            let now = self.now();
            if let Some(n) = self.keys.lock().unwrap().get_mut(&(hive, path.to_lowercase())) {
                n.1.values.remove(&name.to_lowercase());
                n.1.time = now;
            }
            Ok(())
        }
        fn delete_tree(&self, hive: Hive, path: &str) -> Result<()> {
            check_write(hive, path)?;
            self.denied(hive, path)?;
            let p = path.to_lowercase();
            let prefix = format!("{p}\\");
            self.keys
                .lock()
                .unwrap()
                .retain(|(h, k), _| !(*h == hive && (*k == p || k.starts_with(&prefix))));
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_policy() {
        for ok in [
            (Hive::CurrentUser, "Software\\Classes\\.mp4\\OpenWithProgids"),
            (Hive::CurrentUser, "Software\\Classes\\Applications\\vlc.exe\\shell\\open\\command"),
            (Hive::CurrentUser, "Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\FileExts\\.mp4\\UserChoice"),
            (Hive::CurrentUser, "Software\\Microsoft\\Windows\\Shell\\Associations\\UrlAssociations\\https\\UserChoice"),
            (Hive::LocalMachine, "SOFTWARE\\Classes\\.bat"),
        ] {
            assert!(check_write(ok.0, ok.1).is_ok(), "{ok:?}");
        }
        for bad in [
            (Hive::CurrentUser, "Software\\Classes\\batfile\\shell\\open\\command"),
            (Hive::CurrentUser, "Software\\Microsoft\\Windows\\CurrentVersion\\Run"),
            (Hive::CurrentUser, "Software\\Classes\\."),
            (Hive::LocalMachine, "SOFTWARE\\Classes\\.bat\\ShellNew"),
            (Hive::LocalMachine, "SOFTWARE\\Classes\\batfile"),
            (Hive::ClassesRoot, ".bat"),
        ] {
            assert!(check_write(bad.0, bad.1).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn mem_registry_merges_classes_root() {
        let r = MemRegistry::new();
        r.seed(Hive::LocalMachine, "Software\\Classes\\.bat", "", Value::Sz("batfile".into()));
        assert_eq!(r.string(Hive::ClassesRoot, ".bat", "").as_deref(), Some("batfile"));
        // An HKCU override key without a default value shadows HKLM: the real .bat bug.
        r.seed_key(Hive::CurrentUser, "Software\\Classes\\.bat");
        assert_eq!(r.string(Hive::ClassesRoot, ".bat", ""), None);
        r.delete_tree(Hive::CurrentUser, "Software\\Classes\\.bat").unwrap();
        assert_eq!(r.string(Hive::ClassesRoot, ".bat", "").as_deref(), Some("batfile"));
    }

    #[test]
    fn value_bytes_round_trip() {
        for v in [
            Value::Sz("héllo".into()),
            Value::ExpandSz("%SystemRoot%\\x.exe".into()),
            Value::Dword(7),
            Value::Raw(REG_NONE, vec![]),
            Value::Raw(REG_BINARY, vec![1, 2, 3]),
        ] {
            assert_eq!(Value::from_bytes(v.kind(), &v.to_bytes()), v);
        }
    }
}
