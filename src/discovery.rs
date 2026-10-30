//! Builds the search index: file types, protocols and programs (with the targets each program
//! declares). Runs on a background thread in two passes: registry sources first (fast, gives
//! complete association data), then Start Menu shortcuts and packaged apps (COM, slower).
use crate::{
    model::{FileType, Index, Program, Protocol},
    registry::{Hive, Registry},
    shell,
};
use std::collections::HashMap;
use std::path::Path;

const HKCR: Hive = Hive::ClassesRoot;

/// Extracts the executable from a shell command line (already env-expanded):
/// `"C:\x\vlc.exe" --started-from-file "%1"` -> `C:\x\vlc.exe`. Pure; no file system access.
pub fn parse_command_exe(command: &str) -> Option<String> {
    let s = command.trim();
    let exe = if let Some(rest) = s.strip_prefix('"') {
        rest.split('"').next()?
    } else {
        // Unquoted paths with spaces are common ("C:\Program Files\x\y.exe %1"): cut after
        // the first ".exe" rather than at the first space.
        let lower = s.to_ascii_lowercase();
        match lower.find(".exe") {
            Some(i) => &s[..i + 4],
            None => s.split_whitespace().next()?,
        }
    };
    let exe = exe.trim();
    if exe.is_empty() || exe.contains('%') {
        return None;
    }
    Some(exe.to_owned())
}

/// Expands and resolves a command's executable to an existing absolute path. Bare names
/// ("notepad.exe", "rundll32") are looked up in System32, Windows and PATH.
pub fn resolve_command_exe(command: &str) -> Option<String> {
    let exe = parse_command_exe(&shell::expand_env(command))?;
    let p = Path::new(&exe);
    if p.is_absolute() {
        // UNC/network executables are never registered or launched from here.
        return (!exe.starts_with("\\\\") && p.is_file()).then_some(exe);
    }
    if exe.contains(['\\', '/']) {
        return None;
    }
    let name = if p.extension().is_none() { format!("{exe}.exe") } else { exe };
    let mut dirs = Vec::new();
    if let Some(root) = std::env::var_os("SystemRoot") {
        dirs.push(Path::new(&root).join("System32"));
        dirs.push(Path::new(&root).to_path_buf());
    }
    if let Some(path) = std::env::var_os("PATH") {
        dirs.extend(std::env::split_paths(&path));
    }
    dirs.into_iter()
        .map(|d| d.join(&name))
        .find(|c| c.is_file())
        .map(|c| c.to_string_lossy().into_owned())
}

/// Installers, updaters and crash reporters are never sensible default handlers.
fn junk(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    ["uninstall", "unins0", "setup.exe", "updater", "update.exe", "crashpad", "crashreport", "crashhandler", "helper.exe", "elevation_service", "notification_helper"]
        .iter()
        .any(|j| n.contains(j))
}

/// Normalizes ".MP4" / "mp4" / " .mp4 " to ".mp4". Rejects anything that can't be a key name.
pub fn normalize_extension(input: &str) -> Option<String> {
    let s = input.trim().trim_start_matches('.');
    if s.is_empty() || s.len() > 64 || s.chars().any(|c| c.is_control() || "\\/:*?\"<>| ".contains(c)) {
        return None;
    }
    Some(format!(".{}", s.to_lowercase()))
}

/// The command line registered for a ProgID: `shell\open`, else the default verb, else the
/// first verb.
pub fn prog_command(reg: &dyn Registry, prog_id: &str) -> Option<String> {
    let shell_key = format!("{prog_id}\\shell");
    let mut verbs = vec!["open".to_owned()];
    if let Some(def) = reg.string(HKCR, &shell_key, "").filter(|d| !d.is_empty()) {
        verbs.insert(0, def);
    }
    verbs.extend(reg.subkeys(HKCR, &shell_key).into_iter().take(3));
    verbs.iter().find_map(|v| reg.string(HKCR, &format!("{shell_key}\\{v}\\command"), "").filter(|c| !c.is_empty()))
}

/// Caches ProgID -> executable lookups; thousands of extensions share a handful of ProgIDs.
struct ProgCache<'a> {
    reg: &'a dyn Registry,
    exe: HashMap<String, Option<String>>,
}

impl ProgCache<'_> {
    fn exe(&mut self, prog_id: &str) -> Option<String> {
        let key = prog_id.to_lowercase();
        if let Some(v) = self.exe.get(&key) {
            return v.clone();
        }
        let v = prog_command(self.reg, prog_id).and_then(|c| resolve_command_exe(&c));
        self.exe.insert(key, v.clone());
        v
    }
}

/// Pass 1a: every extension and URL protocol under HKCR.
pub fn scan_types(reg: &dyn Registry, cancel: &dyn Fn() -> bool) -> (Vec<FileType>, Vec<Protocol>) {
    let mut types = Vec::new();
    let mut protocols = Vec::new();
    for key in reg.subkeys(HKCR, "") {
        if cancel() {
            break;
        }
        if key.starts_with('.') {
            let Some(ext) = normalize_extension(&key) else { continue };
            let prog_id = reg.string(HKCR, &key, "").unwrap_or_default();
            let friendly = if prog_id.is_empty() {
                String::new()
            } else {
                reg.string(HKCR, &prog_id, "").unwrap_or_default()
            };
            let friendly = if friendly.is_empty() || friendly.starts_with('@') {
                format!("{} file", ext[1..].to_uppercase())
            } else {
                friendly
            };
            let mut t = FileType { search_name: ext[1..].to_owned(), search: String::new(), ext, friendly };
            t.search = format!("{} {}", t.search_name, t.friendly.to_lowercase());
            types.push(t);
        } else if !key.contains(['{', ' ']) && reg.get(HKCR, &key, "URL Protocol").is_some() {
            let name = key.to_lowercase();
            let friendly = reg.string(HKCR, &key, "").unwrap_or_default();
            let friendly = friendly.trim_start_matches("URL:").trim().to_owned();
            let search = format!("{name} {}", friendly.to_lowercase());
            protocols.push(Protocol { name, friendly, search });
        }
    }
    types.sort_by(|a, b| a.ext.cmp(&b.ext));
    types.dedup_by(|a, b| a.ext == b.ext);
    protocols.sort_by(|a, b| a.name.cmp(&b.name));
    protocols.dedup_by(|a, b| a.name == b.name);
    (types, protocols)
}

/// Merges programs by identity (exe path or AUMID), keeping the best name and union of
/// supported targets.
pub fn merge(into: &mut Vec<Program>, incoming: Vec<Program>) {
    let mut by_key: HashMap<String, usize> = into.iter().enumerate().map(|(i, p)| (p.key(), i)).collect();
    for mut p in incoming {
        if p.name.trim().is_empty() || junk(&p.name) || junk(p.exe_name()) {
            continue;
        }
        let key = p.key();
        if key.is_empty() {
            continue;
        }
        if let Some(&i) = by_key.get(&key) {
            let old = &mut into[i];
            if p.name_rank > old.name_rank {
                old.name = std::mem::take(&mut p.name);
                old.name_rank = p.name_rank;
            }
            if old.registered_name.is_empty() && !p.registered_name.is_empty() {
                old.registered_name = p.registered_name;
                old.registered_machine = p.registered_machine;
            }
            if old.aumid.is_empty() {
                old.aumid = p.aumid;
            }
            for s in p.supports {
                old.add_support(&s.target, &s.prog_id);
            }
            old.reindex();
        } else {
            p.reindex();
            by_key.insert(key, into.len());
            into.push(p);
        }
    }
}

fn program(name: String, rank: u8, exe: String) -> Program {
    Program { name, name_rank: rank, exe, ..Program::default() }
}

fn stem(exe: &str) -> String {
    Path::new(exe).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
}

/// Pass 1b: programs from RegisteredApplications, HKCR\Applications, App Paths and every
/// extension's OpenWithProgids / OpenWithList.
pub fn scan_programs(reg: &dyn Registry, types: &[FileType], cancel: &dyn Fn() -> bool) -> Vec<Program> {
    let mut cache = ProgCache { reg, exe: HashMap::new() };
    let mut out: Vec<Program> = Vec::new();

    // RegisteredApplications -> Capabilities (the authoritative "this app supports X" list).
    for (hive, machine) in [(Hive::CurrentUser, false), (Hive::LocalMachine, true)] {
        for (reg_name, caps) in reg.values(hive, "Software\\RegisteredApplications") {
            if cancel() {
                return out;
            }
            let Some(caps) = caps.as_str().map(str::to_owned) else { continue };
            let mut p = Program { registered_name: reg_name.clone(), registered_machine: machine, ..Program::default() };
            let name = reg.string(hive, &caps, "ApplicationName").map(|n| shell::load_indirect(&n)).unwrap_or_default();
            p.name = if name.is_empty() { reg_name.clone() } else { name };
            p.name_rank = 3;
            for sub in ["FileAssociations", "URLAssociations"] {
                for (target, prog) in reg.values(hive, &format!("{caps}\\{sub}")) {
                    let Some(prog) = prog.as_str() else { continue };
                    let target = if sub == "FileAssociations" {
                        match normalize_extension(&target) {
                            Some(t) => t,
                            None => continue,
                        }
                    } else {
                        target.to_lowercase()
                    };
                    p.add_support(&target, prog);
                    if p.exe.is_empty() && p.aumid.is_empty() {
                        // Desktop apps (Chrome, Edge) also set ProgID\Application\AppUserModelID,
                        // so a resolvable command wins; only command-less ProgIDs are packaged.
                        if let Some(exe) = cache.exe(prog) {
                            p.exe = exe;
                        } else if let Some(aumid) = reg.string(HKCR, &format!("{prog}\\Application"), "AppUserModelID") {
                            p.aumid = aumid;
                        }
                    }
                }
            }
            if p.exe.is_empty() && p.aumid.is_empty() {
                // Browsers: ...\StartMenuInternet\<name>\Capabilities -> ...\<name>\shell\open\command
                let parent = caps.rsplit_once('\\').map(|x| x.0).unwrap_or_default();
                if let Some(cmd) = reg.string(hive, &format!("{parent}\\shell\\open\\command"), "") {
                    p.exe = resolve_command_exe(&cmd).unwrap_or_default();
                }
            }
            if !p.supports.is_empty() {
                merge(&mut out, vec![p]);
            }
        }
    }

    // HKCR\Applications\<exe> with SupportedTypes.
    for exe_name in reg.subkeys(HKCR, "Applications") {
        if cancel() {
            return out;
        }
        let key = format!("Applications\\{exe_name}");
        if reg.get(HKCR, &key, "NoOpenWith").is_some() {
            continue;
        }
        let Some(exe) = cache.exe(&key) else { continue };
        let friendly = reg.string(HKCR, &key, "FriendlyAppName").map(|n| shell::load_indirect(&n)).unwrap_or_default();
        let mut p = if friendly.is_empty() { program(stem(&exe), 0, exe) } else { program(friendly, 2, exe) };
        for (ext, _) in reg.values(HKCR, &format!("{key}\\SupportedTypes")) {
            if let Some(ext) = normalize_extension(&ext) {
                p.add_support(&ext, &key);
            }
        }
        merge(&mut out, vec![p]);
    }

    // App Paths: installed programs that may declare nothing but should be findable.
    for hive in [Hive::CurrentUser, Hive::LocalMachine] {
        let base = "Software\\Microsoft\\Windows\\CurrentVersion\\App Paths";
        for exe_name in reg.subkeys(hive, base) {
            if let Some(path) = reg.string(hive, &format!("{base}\\{exe_name}"), "") {
                if let Some(exe) = resolve_command_exe(&format!("\"{}\"", path.trim_matches('"'))) {
                    merge(&mut out, vec![program(stem(&exe), 0, exe)]);
                }
            }
        }
    }

    // Per-extension OpenWithProgids / OpenWithList and the class default: attaches the
    // reverse mapping (exe -> targets) that most installers only record here.
    let mut found: Vec<Program> = Vec::new();
    for t in types {
        if cancel() {
            return out;
        }
        let mut progs: Vec<String> = reg.values(HKCR, &format!("{}\\OpenWithProgids", t.ext)).into_iter().map(|v| v.0).collect();
        if let Some(def) = reg.string(HKCR, &t.ext, "").filter(|d| !d.is_empty()) {
            progs.push(def);
        }
        for app in reg.subkeys(HKCR, &format!("{}\\OpenWithList", t.ext)) {
            progs.push(format!("Applications\\{app}"));
        }
        for prog in progs {
            if let Some(exe) = cache.exe(&prog) {
                let mut p = program(stem(&exe), 0, exe);
                p.add_support(&t.ext, &prog);
                found.push(p);
            } else if let Some(aumid) = reg.string(HKCR, &format!("{prog}\\Application"), "AppUserModelID") {
                let name = reg.string(HKCR, &format!("{prog}\\Application"), "ApplicationName").map(|n| shell::load_indirect(&n)).unwrap_or_default();
                // Unnamed for now; the packaged-app pass supplies the display name (rank 3).
                let name = if name.is_empty() { aumid.clone() } else { name };
                let mut p = Program { name, aumid, ..Program::default() };
                p.add_support(&t.ext, &prog);
                found.push(p);
            }
        }
    }
    merge(&mut out, found);
    out
}

/// Pass 2: Start Menu shortcuts (good display names) and packaged apps. Also fills in names
/// from version resources for programs that only had a file stem.
pub fn scan_shell(programs: &mut Vec<Program>, cancel: &dyn Fn() -> bool) {
    let mut found = Vec::new();
    for dir in shell::start_menu_dirs() {
        let mut stack = vec![dir];
        while let Some(d) = stack.pop() {
            if cancel() {
                return;
            }
            let Ok(entries) = std::fs::read_dir(&d) else { continue };
            for e in entries.flatten() {
                let path = e.path();
                let Ok(kind) = e.file_type() else { continue };
                if kind.is_dir() && !kind.is_symlink() {
                    stack.push(path);
                } else if path.extension().is_some_and(|x| x.eq_ignore_ascii_case("lnk")) {
                    if let Some(target) = shell::resolve_shortcut(&path) {
                        if target.to_ascii_lowercase().ends_with(".exe") && !target.starts_with("\\\\") && Path::new(&target).is_file() {
                            let name = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                            found.push(program(name, 2, target));
                        }
                    }
                }
            }
        }
    }
    merge(programs, found);

    let packaged = shell::packaged_apps()
        .into_iter()
        .map(|(name, aumid)| Program { name, name_rank: 3, aumid, ..Program::default() })
        .collect();
    merge(programs, packaged);

    for p in programs.iter_mut() {
        if cancel() {
            return;
        }
        if p.name_rank == 0 && !p.exe.is_empty() {
            let (desc, _) = shell::version_info(&p.exe);
            let desc = desc.trim_end_matches(" Launcher Shim").trim();
            if !desc.is_empty() && desc.len() < 64 {
                p.name = desc.to_owned();
                p.name_rank = 1;
                p.reindex();
            }
        }
    }
}

pub fn sort(index: &mut Index) {
    index.programs.sort_by_cached_key(|p| p.search_name.clone());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{MemRegistry, Value};

    #[test]
    fn command_parsing() {
        assert_eq!(parse_command_exe("\"C:\\Program Files\\VLC\\vlc.exe\" --started-from-file \"%1\"").as_deref(), Some("C:\\Program Files\\VLC\\vlc.exe"));
        assert_eq!(parse_command_exe("C:\\Program Files\\App\\app.exe %1").as_deref(), Some("C:\\Program Files\\App\\app.exe"));
        assert_eq!(parse_command_exe("rundll32 shell32.dll,OpenAs %1").as_deref(), Some("rundll32"));
        assert_eq!(parse_command_exe("\"%1\" %*"), None);
        assert_eq!(parse_command_exe(""), None);
    }

    #[test]
    fn extensions_normalize() {
        assert_eq!(normalize_extension(" BAT ").as_deref(), Some(".bat"));
        assert_eq!(normalize_extension(".PDF").as_deref(), Some(".pdf"));
        for bad in ["", ".", "a\\b", "a b", "a/b", "a\0b"] {
            assert_eq!(normalize_extension(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn merge_keeps_best_name_and_union_of_targets() {
        let mut a = program("vlc".into(), 0, "C:\\VLC\\vlc.exe".into());
        a.add_support(".mp4", "Applications\\vlc.exe");
        let mut b = program("VLC media player".into(), 3, "c:\\vlc\\VLC.EXE".into());
        b.add_support(".mp4", "VLC.mp4");
        b.add_support(".mkv", "VLC.mkv");
        let mut out = Vec::new();
        merge(&mut out, vec![a]);
        merge(&mut out, vec![b, program("Uninstall VLC".into(), 3, "C:\\VLC\\uninstall.exe".into())]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "VLC media player");
        // Capabilities ProgID replaces the generic Applications\ registration.
        assert_eq!(out[0].supports.iter().find(|s| s.target == ".mp4").unwrap().prog_id, "VLC.mp4");
        assert_eq!(out[0].supports.len(), 2);
    }

    #[test]
    fn scan_types_finds_extensions_and_protocols() {
        let r = MemRegistry::new();
        r.seed(Hive::LocalMachine, "Software\\Classes\\.txt", "", Value::Sz("txtfile".into()));
        r.seed(Hive::LocalMachine, "Software\\Classes\\txtfile", "", Value::Sz("Text Document".into()));
        r.seed(Hive::LocalMachine, "Software\\Classes\\https", "URL Protocol", Value::Sz(String::new()));
        r.seed(Hive::LocalMachine, "Software\\Classes\\https", "", Value::Sz("URL:HyperText Transfer Protocol with privacy".into()));
        r.seed_key(Hive::LocalMachine, "Software\\Classes\\CLSID");
        let (types, protos) = scan_types(&r, &|| false);
        assert_eq!(types.len(), 1);
        assert_eq!(types[0].friendly, "Text Document");
        assert_eq!(protos.len(), 1);
        assert_eq!(protos[0].friendly, "HyperText Transfer Protocol with privacy");
    }
}
