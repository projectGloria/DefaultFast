use crate::{ffi, model::App};
use std::collections::BTreeMap;
use std::path::Path;

fn useful(name: &str) -> bool {
    let s = name.to_lowercase();
    ![
        "uninstall",
        "unins0",
        "updater",
        "crashpad",
        "crashreport",
        "setup.exe",
        "helper.exe",
    ]
    .iter()
    .any(|x| s.contains(x))
}

pub fn merge(apps: &mut Vec<App>, incoming: impl IntoIterator<Item = App>) {
    let mut identities: BTreeMap<String, usize> = apps
        .iter()
        .enumerate()
        .map(|(i, a)| (a.identity().to_lowercase(), i))
        .collect();
    for mut app in incoming {
        if app.name.is_empty() || !useful(&app.name) {
            continue;
        }
        app.reindex();
        let key = app.identity().to_lowercase();
        if let Some(index) = identities.get(&key) {
            let old = &mut apps[*index];
            old.recommended |= app.recommended;
            if app.recommended || old.prog_id.is_empty() {
                old.prog_id = app.prog_id;
            }
            if old.publisher.is_empty() {
                old.publisher = app.publisher;
            }
            if old.name.len() < app.name.len() && !app.name.to_lowercase().ends_with(".exe") {
                old.name = app.name;
            }
            old.reindex();
        } else {
            identities.insert(key, apps.len());
            apps.push(app);
        }
    }
    apps.sort_by_cached_key(|a| (!a.recommended, a.name.to_lowercase()));
}

fn from_command(name: String, command: String, prog_id: String) -> Option<App> {
    let executable = ffi::command_executable(&command)?;
    if !useful(&executable) {
        return None;
    }
    let (description, publisher) = ffi::executable_metadata(&executable);
    let description = description.trim_end_matches(" Launcher Shim").to_owned();
    let name = if name.is_empty() {
        if description.is_empty() {
            Path::new(&executable)
                .file_stem()?
                .to_string_lossy()
                .into_owned()
        } else {
            description
        }
    } else {
        ffi::resolve_resource(&name)
    };
    Some(App {
        name,
        publisher,
        icon: executable.clone(),
        executable,
        prog_id,
        ..App::default()
    })
}

pub fn scan(mut emit: impl FnMut(Vec<App>), cancelled: impl Fn() -> bool) {
    for root in [ffi::Root::User, ffi::Root::Machine] {
        let mut batch = Vec::new();
        for exe in ffi::subkeys(root, "Software\\Classes\\Applications") {
            if cancelled() {
                return;
            }
            let key = format!("Software\\Classes\\Applications\\{exe}");
            if ffi::reg_value_exists(root, &key, "NoOpenWith") {
                continue;
            }
            if let Some(command) =
                ffi::reg_string(root, &format!("{key}\\shell\\open\\command"), "")
            {
                if let Some(app) = from_command(
                    ffi::reg_string(root, &key, "FriendlyAppName").unwrap_or_default(),
                    command,
                    format!("Applications\\{exe}"),
                ) {
                    batch.push(app);
                }
            }
        }
        for view in [
            "Software\\Microsoft\\Windows\\CurrentVersion\\App Paths",
            "Software\\WOW6432Node\\Microsoft\\Windows\\CurrentVersion\\App Paths",
        ] {
            for exe in ffi::subkeys(root, view) {
                if cancelled() {
                    return;
                }
                if let Some(path) = ffi::reg_string(root, &format!("{view}\\{exe}"), "") {
                    if let Some(app) =
                        from_command(String::new(), format!("\"{path}\""), String::new())
                    {
                        batch.push(app);
                    }
                }
            }
        }
        for (name, capabilities) in ffi::reg_values(root, "Software\\RegisteredApplications") {
            if cancelled() {
                return;
            }
            let title = ffi::reg_string(root, &capabilities, "ApplicationName").unwrap_or(name);
            let mut ids = BTreeMap::new();
            for (_, id) in ffi::reg_values(root, &format!("{capabilities}\\FileAssociations")) {
                ids.insert(id, ());
            }
            for (id, _) in ids {
                if let Some(command) = ffi::reg_string(
                    ffi::Root::Classes,
                    &format!("{id}\\shell\\open\\command"),
                    "",
                ) {
                    if let Some(app) = from_command(title.clone(), command, id) {
                        batch.push(app);
                    }
                }
            }
        }
        emit(batch);
    }
    // The Shell AppsFolder supplies installed packaged apps without parsing protected package directories.
    // AppX ProgIDs and recommended IAssocHandlers provide their file-type activation identities.
    emit(ffi::shell_apps());
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        if let Ok(entries) =
            std::fs::read_dir(std::path::PathBuf::from(local).join("Microsoft\\WindowsApps"))
        {
            let mut batch = Vec::new();
            for entry in entries.flatten() {
                if cancelled() {
                    return;
                }
                if entry
                    .path()
                    .extension()
                    .is_some_and(|e| e.eq_ignore_ascii_case("exe"))
                {
                    let path = entry.path().to_string_lossy().into_owned();
                    if let Some(app) =
                        from_command(String::new(), format!("\"{path}\""), String::new())
                    {
                        batch.push(app);
                    }
                }
            }
            emit(batch);
        }
    }
    for dir in ffi::start_menu_dirs() {
        let mut pending = vec![dir];
        while let Some(path) = pending.pop() {
            if cancelled() {
                return;
            }
            if let Ok(entries) = std::fs::read_dir(path) {
                let mut batch = Vec::new();
                for entry in entries.flatten() {
                    if let Ok(kind) = entry.file_type() {
                        if kind.is_dir() && !kind.is_symlink() {
                            pending.push(entry.path());
                        } else if entry
                            .path()
                            .extension()
                            .is_some_and(|e| e.eq_ignore_ascii_case("lnk"))
                        {
                            if let Some(app) = ffi::shortcut(&entry.path()) {
                                batch.push(app);
                            }
                        }
                    }
                }
                if !batch.is_empty() {
                    emit(batch);
                }
            }
        }
    }
}

pub fn for_type(extension: &str) -> Vec<App> {
    let mut apps = ffi::recommended_apps(extension);
    for root in [ffi::Root::Classes, ffi::Root::User] {
        let key = if matches!(root, ffi::Root::User) {
            format!("Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\FileExts\\{extension}")
        } else {
            extension.to_owned()
        };
        for (id, _) in ffi::reg_values(root, &format!("{key}\\OpenWithProgids")) {
            let name = ffi::reg_string(ffi::Root::Classes, &id, "").unwrap_or_default();
            if let Some(command) = ffi::reg_string(
                ffi::Root::Classes,
                &format!("{id}\\shell\\open\\command"),
                "",
            ) {
                if let Some(mut app) = from_command(name, command, id) {
                    app.recommended = true;
                    apps.push(app);
                }
            }
        }
        for (_, exe) in ffi::reg_values(root, &format!("{key}\\OpenWithList")) {
            if exe.to_lowercase().ends_with(".exe") {
                let id = format!("Applications\\{exe}");
                if let Some(command) = ffi::reg_string(
                    ffi::Root::Classes,
                    &format!("{id}\\shell\\open\\command"),
                    "",
                ) {
                    if let Some(mut app) = from_command(String::new(), command, id) {
                        app.recommended = true;
                        apps.push(app);
                    }
                }
            }
        }
    }
    apps
}
