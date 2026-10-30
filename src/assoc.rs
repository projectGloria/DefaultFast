use crate::{
    cache::{self, Reader},
    ffi::{self, Root},
    hash,
    model::App,
    search,
};

pub fn executable_type(ext: &str) -> bool {
    [
        ".bat", ".cmd", ".exe", ".com", ".lnk", ".scr", ".pif", ".msi", ".ps1", ".vbs", ".js",
    ]
    .contains(&ext)
}
pub fn choice_path(ext: &str) -> String {
    format!("Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\FileExts\\{ext}\\UserChoice")
}

#[derive(Clone, Debug)]
struct ValueChange {
    key: String,
    name: String,
    before: Option<(u32, Vec<u8>)>,
    after: String,
}
#[derive(Clone, Debug)]
struct Undo {
    ext: String,
    before: Option<String>,
    effective_before: String,
    after: String,
    values: Vec<ValueChange>,
    verb: bool,
}

fn save_undo(u: &Undo) -> Result<(), String> {
    let mut b = b"DPU4".to_vec();
    cache::put_string(&mut b, &u.ext);
    cache::put_string(&mut b, u.before.as_deref().unwrap_or(""));
    cache::put_string(&mut b, &u.effective_before);
    cache::put_string(&mut b, &u.after);
    cache::put_u32(&mut b, u.verb as u32);
    cache::put_u32(&mut b, u.values.len() as u32);
    for v in &u.values {
        cache::put_string(&mut b, &v.key);
        cache::put_string(&mut b, &v.name);
        cache::put_string(&mut b, &v.after);
        if let Some((kind, data)) = &v.before {
            cache::put_u32(&mut b, *kind);
            cache::put_u32(&mut b, data.len() as u32);
            b.extend_from_slice(data);
        } else {
            cache::put_u32(&mut b, u32::MAX);
        }
    }
    cache::atomic_write("undo.bin", &b)
        .map_err(|e| format!("Cannot save Undo; nothing changed: {e}"))
}
fn load_undo() -> Option<Undo> {
    let b = cache::read("undo.bin").ok()?;
    let mut r = Reader(&b);
    if r.take(4)? != b"DPU4" {
        return None;
    }
    let ext = r.string()?;
    if search::normalize_extension(&ext).as_deref() != Some(&ext) {
        return None;
    }
    let previous = r.string()?;
    let effective_before = r.string()?;
    let after = r.string()?;
    let verb = r.number()? != 0;
    let n = r.number()?;
    if n > 32 {
        return None;
    }
    let mut values = Vec::new();
    for _ in 0..n {
        let key = r.string()?;
        if !key.starts_with("Software\\Classes\\") {
            return None;
        }
        let name = r.string()?;
        let after = r.string()?;
        let kind = r.number()?;
        let before = if kind == u32::MAX {
            None
        } else {
            let n = r.number()? as usize;
            if n > 65536 {
                return None;
            }
            Some((kind, r.take(n)?.to_vec()))
        };
        values.push(ValueChange {
            key,
            name,
            before,
            after,
        });
    }
    if !r.0.is_empty() {
        return None;
    }
    Some(Undo {
        ext,
        before: if previous.is_empty() {
            None
        } else {
            Some(previous)
        },
        after,
        effective_before,
        values,
        verb,
    })
}
fn change(key: String, name: &str, after: String) -> ValueChange {
    let before = ffi::reg_raw(Root::User, &key, name);
    ValueChange {
        key,
        name: name.into(),
        before,
        after,
    }
}
fn preflight(ext: &str) -> Result<(), String> {
    if ext == ".pdf" && ffi::ucpd_running() {
        return Err("Windows protects PDF defaults. Use Open Settings to choose this app.".into());
    }
    let base = choice_path(ext);
    let parent = base.trim_end_matches("\\UserChoice");
    if ffi::reg_key_exists(Root::User, &format!("{parent}\\UserChoiceLatest")) {
        return Err(
            "This Windows version uses UserChoiceLatest. Change the default in Settings.".into(),
        );
    }
    if let Some(id) = ffi::reg_string(Root::User, &base, "ProgId") {
        let stored = ffi::reg_string(Root::User, &base, "Hash")
            .ok_or("Existing UserChoice has no readable hash. Use Settings.")?;
        let time = ffi::reg_time(Root::User, &base)
            .ok_or("Cannot inspect current UserChoice timestamp.")?;
        let expected = hash::generate(ext, &ffi::sid()?, &id, time)?;
        if stored != expected {
            return Err(
                "Windows uses an unrecognized association hash. Nothing changed; use Settings."
                    .into(),
            );
        }
    } else {
        let sid = ffi::sid()?;
        let compatible = [".txt", ".html", ".htm", ".pdf"].iter().any(|sample| {
            let path = choice_path(sample);
            match (
                ffi::reg_string(Root::User, &path, "ProgId"),
                ffi::reg_string(Root::User, &path, "Hash"),
                ffi::reg_time(Root::User, &path),
            ) {
                (Some(id), Some(stored), Some(time)) => {
                    hash::generate(sample, &sid, &id, time).is_ok_and(|computed| computed == stored)
                }
                _ => false,
            }
        });
        if !compatible {
            return Err("No existing association confirms this Windows hash format. Nothing changed; use Settings.".into());
        }
    }
    Ok(())
}
fn write_choice(ext: &str, id: Option<&str>) -> Result<(), String> {
    let key = choice_path(ext);
    ffi::reg_delete_tree(&key)?;
    let Some(id) = id else {
        ffi::notify_associations();
        return Ok(());
    };
    // No permission/ownership escalation. A denied delete or create is a Settings fallback.
    ffi::reg_set(&key, "ProgId", id)?;
    for _ in 0..3 {
        let time = ffi::reg_time(Root::User, &key).ok_or("Cannot read UserChoice write time")?;
        let hash = hash::generate(ext, &ffi::sid()?, id, time)?;
        ffi::reg_set(&key, "Hash", &hash)?;
        let after = ffi::reg_time(Root::User, &key).ok_or("Cannot verify UserChoice timestamp")?;
        if time / 600_000_000 == after / 600_000_000
            && ffi::reg_string(Root::User, &key, "ProgId").as_deref() == Some(id)
            && ffi::reg_string(Root::User, &key, "Hash").as_deref() == Some(&hash)
        {
            ffi::notify_associations();
            return Ok(());
        }
    }
    Err("The registry timestamp changed during the write. Undo is available.".into())
}
fn apply_values(values: &[ValueChange]) -> Result<(), String> {
    for v in values {
        ffi::reg_set(&v.key, &v.name, &v.after)?;
    }
    Ok(())
}
fn restore_values(values: &[ValueChange], check: bool) -> Result<(), String> {
    for v in values.iter().rev() {
        if check && ffi::reg_string(Root::User, &v.key, &v.name).as_deref() != Some(&v.after) {
            // Untouched members of a partially applied transaction are already restored.
            if ffi::reg_raw(Root::User, &v.key, &v.name) == v.before {
                continue;
            }
            return Err(format!(
                "{} was changed by another program; Undo stopped to preserve it.",
                v.key
            ));
        }
        ffi::reg_restore(&v.key, &v.name, v.before.as_ref())?;
    }
    Ok(())
}

pub fn set(ext: &str, app: &App) -> Result<String, String> {
    let ext = search::normalize_extension(ext).ok_or("Invalid file extension")?;
    if executable_type(&ext) {
        return Err("This is an executable class. Choose the explicitly labeled launch-command alternative, or use Settings.".into());
    }
    preflight(&ext)?;
    let mut id = app.prog_id.clone();
    let mut values = Vec::new();
    // A recommended handler's ProgID is specific to the selected extension. A globally scanned
    // capability ProgID may be for another type, so only reuse it when the shell recommends it.
    if !app.recommended || id.is_empty() {
        if app.executable.is_empty() {
            return Err("This packaged app does not advertise a handler for this type. Choose an advertised app in Settings.".into());
        }
        if !ffi::local_path(&app.executable) || !std::path::Path::new(&app.executable).is_file() {
            return Err("The selected executable no longer exists.".into());
        }
        let identity = app
            .executable
            .to_lowercase()
            .bytes()
            .fold(2166136261u32, |h, c| (h ^ c as u32).wrapping_mul(16777619));
        id = format!("DefaultPicker.{identity:08x}{}", ext);
        let base = format!("Software\\Classes\\{id}");
        values.push(change(
            base.clone(),
            "",
            format!("{} {} file", app.name, ext),
        ));
        values.push(change(
            format!("{base}\\shell\\open\\command"),
            "",
            format!("\"{}\" \"%1\" %*", app.executable),
        ));
        values.push(change(
            format!("Software\\Classes\\{ext}\\OpenWithProgids"),
            &id,
            String::new(),
        ));
    }
    let u = Undo {
        before: ffi::reg_string(Root::User, &choice_path(&ext), "ProgId"),
        effective_before: ffi::effective(&ext, false).unwrap_or_default(),
        ext: ext.clone(),
        after: id.clone(),
        values,
        verb: false,
    };
    let previous_journal = cache::read("undo.bin").ok();
    save_undo(&u)?;
    let result = apply_values(&u.values)
        .and_then(|_| write_choice(&ext, Some(&id)))
        .and_then(|_| {
            let actual = ffi::effective(&ext, false).unwrap_or_default();
            if actual.eq_ignore_ascii_case(&id) {
                Ok(())
            } else {
                Err(format!(
                    "Windows kept the effective handler '{actual}'. Use Settings."
                ))
            }
        });
    if let Err(e) = result {
        let rollback = rollback(&u, false);
        if rollback.is_ok() {
            if let Some(bytes) = previous_journal {
                cache::atomic_write("undo.bin",&bytes).map_err(|restore|format!("{e}; previous default restored, but the earlier Undo log could not be restored: {restore}"))?;
            }
        }
        return Err(format!(
            "{e} {}",
            match rollback {
                Ok(_) => "Previous values restored.".into(),
                Err(r) => format!("Rollback needs attention: {r}. The Undo log is retained."),
            }
        ));
    }
    Ok(format!(
        "{} now opens with {}. Ctrl+Z to undo.",
        ext, app.name
    ))
}

pub fn set_batch_verb(ext: &str, app: &App) -> Result<String, String> {
    if ![".bat", ".cmd"].contains(&ext) {
        return Err("Launch-command changes are only supported for .bat and .cmd; Windows requires Settings for this executable class.".into());
    }
    let exe = app.executable.clone();
    if !ffi::local_path(&exe) || !std::path::Path::new(&exe).is_file() {
        return Err("Choose a local Windows Terminal executable with Browse.".into());
    }
    let name = std::path::Path::new(&exe)
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_lowercase();
    if name != "wt.exe" && name != "windowsterminal.exe" {
        return Err("The launch-command alternative supports Windows Terminal only; arbitrary command templates are intentionally not guessed.".into());
    }
    let id = ffi::effective(ext, false).ok_or("Windows did not return an effective ProgID")?;
    if id.contains('\\') || id.is_empty() || id.eq_ignore_ascii_case("Unknown") {
        return Err("Unsupported executable ProgID".into());
    }
    // Warn in the UI before this function is called: overriding batfile/cmdfile affects every
    // extension sharing that ProgID and does not change Windows' default-app metadata.
    let key = format!("Software\\Classes\\{id}\\shell\\open\\command");
    let command = format!("\"{exe}\" cmd.exe /d /c \"\"%1\" %*\"");
    let u = Undo {
        ext: ext.into(),
        before: None,
        effective_before: ffi::effective_command(ext).unwrap_or_default(),
        after: id,
        values: vec![change(key, "", command.clone())],
        verb: true,
    };
    save_undo(&u)?;
    apply_values(&u.values)?;
    ffi::notify_associations();
    let actual = ffi::effective_command(ext).unwrap_or_default();
    if actual != command {
        restore_values(&u.values, true)?;
        ffi::notify_associations();
        return Err("Windows did not honor the launch-command override. Previous value restored; use Settings.".into());
    }
    Ok(format!(
        "{ext} launch command now uses Windows Terminal. Ctrl+Z to restore."
    ))
}
fn rollback(u: &Undo, check: bool) -> Result<String, String> {
    if !u.verb {
        let current = ffi::reg_string(Root::User, &choice_path(&u.ext), "ProgId");
        if check && current.as_deref() != Some(&u.after) && current != u.before {
            return Err(
                "Another app changed this default. Undo will not overwrite its choice.".into(),
            );
        }
        if current != u.before {
            write_choice(&u.ext, u.before.as_deref())?;
        }
    }
    restore_values(&u.values, true)?;
    ffi::notify_associations();
    if !u.effective_before.is_empty() {
        let actual = if u.verb {
            ffi::effective_command(&u.ext)
        } else {
            ffi::effective(&u.ext, false)
        };
        if actual.is_none_or(|value| !value.eq_ignore_ascii_case(&u.effective_before)) {
            return Err("Prior values were written, but Windows did not confirm the previous effective association. Use Settings.".into());
        }
    }
    if let Ok(dir) = cache::directory() {
        let _ = std::fs::remove_file(dir.join("undo.bin"));
    }
    Ok("Previous association restored.".into())
}
pub fn undo() -> Result<String, String> {
    let u = load_undo().ok_or("No readable Undo record")?;
    if !u.verb {
        preflight(&u.ext)?;
    }
    rollback(&u, true)
}

/// Integration test uses fresh, artificial registry keys and an isolated Undo directory.
/// Never opens a file, launches a handler, or changes a real file type.
pub fn self_test() -> Result<String, String> {
    let _com = ffi::ComApartment::new();
    let ext = format!(".defaultpicker-selftest-{}", std::process::id());
    let before = format!("DefaultPicker.SelfTest.{}.Before", std::process::id());
    let after = format!("DefaultPicker.SelfTest.{}.After", std::process::id());
    let other = format!("DefaultPicker.SelfTest.{}.Other", std::process::id());
    let mut keys = vec![
        format!("Software\\Classes\\{ext}"),
        format!("Software\\Classes\\{before}"),
        format!("Software\\Classes\\{after}"),
        format!("Software\\Classes\\{other}"),
        choice_path(&ext).trim_end_matches("\\UserChoice").into(),
    ];
    if keys.iter().any(|key| ffi::reg_key_exists(Root::User, key)) {
        return Err("Test namespace already exists; nothing changed".into());
    }
    let result = (|| {
        let exe = std::env::current_exe()
            .map_err(|e| e.to_string())?
            .to_string_lossy()
            .into_owned();
        ffi::reg_set(&keys[0], "", &before)?;
        for id in [&before, &after, &other] {
            ffi::reg_set(
                &format!("Software\\Classes\\{id}\\shell\\open\\command"),
                "",
                &format!("\"{exe}\" \"%1\""),
            )?;
        }
        write_choice(&ext, Some(&before))?;
        if ffi::effective(&ext, false).as_deref() != Some(&before) {
            return Err("Windows did not accept the initial legacy test association; write path unsupported on this build".into());
        }
        let app = App {
            name: "Isolated test app".into(),
            prog_id: after.clone(),
            recommended: true,
            ..App::default()
        };
        let latest = format!(
            "{}\\UserChoiceLatest",
            choice_path(&ext).trim_end_matches("\\UserChoice")
        );
        ffi::reg_set(&latest, "Test", "unsupported format")?;
        if set(&ext, &app).is_ok() || ffi::effective(&ext, false).as_deref() != Some(&before) {
            return Err("Unsupported-format preflight failed".into());
        }
        ffi::reg_delete_tree(&latest)?;
        set(&ext, &app)?;
        if ffi::effective(&ext, false).as_deref() != Some(&after) {
            return Err("Set verification failed".into());
        }
        write_choice(&ext, Some(&other))?;
        if undo().is_ok() || ffi::effective(&ext, false).as_deref() != Some(&other) {
            return Err("Undo overwrote an unrelated external choice".into());
        }
        write_choice(&ext, Some(&after))?;
        undo()?;
        if ffi::effective(&ext, false).as_deref() != Some(&before) {
            return Err("Undo verification failed".into());
        }
        let app = App {
            name: "Unregistered test application".into(),
            executable: exe,
            ..App::default()
        };
        set(&ext, &app)?;
        let generated = ffi::effective(&ext, false).ok_or("Generated handler not effective")?;
        if !generated.starts_with("DefaultPicker.") || !generated.ends_with(&ext) {
            return Err("Unexpected generated ProgID".into());
        }
        keys.push(format!("Software\\Classes\\{generated}"));
        undo()?;
        if ffi::reg_string(
            Root::User,
            &format!("Software\\Classes\\{generated}\\shell\\open\\command"),
            "",
        )
        .is_some()
        {
            return Err("Undo left the generated command in place".into());
        }
        Ok("PASS: legacy set/readback/effective verification, persistent Undo, external-choice conflict, unknown-format rejection, generated ProgID registration and restoration".into())
    })();
    let mut cleanup_errors = Vec::new();
    for key in keys.iter().rev() {
        if let Err(e) = ffi::remove_self_test_key(key) {
            cleanup_errors.push(e);
        }
    }
    ffi::notify_associations();
    if !cleanup_errors.is_empty() {
        return Err(format!(
            "{result:?}; cleanup: {}",
            cleanup_errors.join("; ")
        ));
    }
    result
}
