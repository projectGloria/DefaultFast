//! `--self-test`: end-to-end check of the real write path on this machine without touching
//! any real file type. Uses an invented extension (.dfselftest<pid>) and invented
//! `Applications\…` handlers that point at this exe (never launched), runs set -> verify ->
//! undo -> verify, then deletes every key it created. Report: %LOCALAPPDATA%\DefaultFast\self-test.txt
use crate::{
    backup,
    defaults::{self, Request, Status},
    registry::{Hive, Registry, Value, WinRegistry},
    shell::{self, AssocStr},
    userchoice,
};

pub fn run() -> i32 {
    let _com = shell::Com::init();
    let reg = WinRegistry;
    let pid = std::process::id();
    let ext = format!(".dfselftest{pid}");
    let exe = std::env::current_exe().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
    let app_a = format!("dfselftest{pid}a.exe");
    let app_b = format!("dfselftest{pid}b.exe");
    let keys = [
        format!("Software\\Classes\\{ext}"),
        format!("Software\\Classes\\Applications\\{app_a}"),
        format!("Software\\Classes\\Applications\\{app_b}"),
        userchoice::base_key(&ext),
    ];
    let dir = std::env::temp_dir().join(format!("dfselftest{pid}"));
    let _ = std::fs::create_dir_all(&dir);
    let mut log = Vec::new();

    let result = (|| -> Result<(), String> {
        let sid = shell::user_sid().map_err(|e| e.to_string())?;
        let (algo, valid, invalid) = userchoice::algorithm_status(&reg, &sid);
        log.push(format!("hash algorithm vs this Windows: {algo:?} ({valid} valid, {invalid} invalid)"));
        // Baseline: the extension's class default points at handler A.
        reg.set(Hive::CurrentUser, &keys[0], "", &Value::Sz(format!("Applications\\{app_a}"))).map_err(|e| e.to_string())?;
        reg.set(Hive::CurrentUser, &format!("{}\\shell\\open\\command", keys[1]), "", &Value::Sz(format!("\"{exe}\" \"%1\""))).map_err(|e| e.to_string())?;
        crate::shell_notify();
        let before = shell::assoc_query(&ext, AssocStr::ProgId).unwrap_or_default();
        log.push(format!("baseline effective ProgID: {before}"));

        // Set handler B through the real transaction (registration + UserChoice + verify).
        let verify = |t: &str, p: &str, _: &str| shell::assoc_query(t, AssocStr::ProgId).is_some_and(|e| e.eq_ignore_ascii_case(p));
        let fake_b = format!("{}\\{app_b}", exe.rsplit_once('\\').map(|x| x.0).unwrap_or(""));
        let req = Request { app_name: "Self-test B".into(), exe: fake_b, changes: vec![(ext.clone(), format!("Applications\\{app_b}"))] };
        let rep = defaults::apply(&reg, &verify, &sid, &req, &shell::file_stamp(), &shell::timestamp(), &dir);
        log.push(format!("apply: {:?}", rep.results));
        if rep.results[0].1 != Status::Done {
            return Err("UserChoice write was not honored by Shell".into());
        }
        let after = shell::assoc_query(&ext, AssocStr::ProgId).unwrap_or_default();
        log.push(format!("effective after set: {after}; hash valid: {:?}", userchoice::validate(&reg, &ext, &sid)));

        // Undo from the .reg backup and confirm Shell is back to A.
        let path = rep.backup.ok_or("no backup written")?;
        let (_, _, ops) = backup::load(&path).map_err(|e| e.to_string())?;
        backup::restore(&reg, &ops, &sid).map_err(|e| e.to_string())?;
        crate::shell_notify();
        let undone = shell::assoc_query(&ext, AssocStr::ProgId).unwrap_or_default();
        log.push(format!("effective after undo: {undone}"));
        if !undone.eq_ignore_ascii_case(&before) {
            return Err(format!("undo left {undone}, expected {before}"));
        }
        if reg.exists(Hive::CurrentUser, &keys[2]) {
            return Err("undo left the registration of handler B".into());
        }
        Ok(())
    })();

    for k in keys.iter().rev() {
        if let Err(e) = reg.delete_tree(Hive::CurrentUser, k) {
            log.push(format!("cleanup failed for {k}: {e}"));
        }
    }
    crate::shell_notify();
    let _ = std::fs::remove_dir_all(&dir);
    let ok = result.is_ok();
    log.push(match result {
        Ok(()) => "PASS".into(),
        Err(e) => format!("FAIL: {e}"),
    });
    let _ = std::fs::write(crate::logfile::data_dir().join("self-test.txt"), log.join("\r\n"));
    if ok {
        0
    } else {
        1
    }
}
