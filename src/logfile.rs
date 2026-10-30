//! Tiny rotating log at %LOCALAPPDATA%\DefaultFast\log.txt. Every registry mutation is logged
//! (see registry.rs). Capped at 256 KiB; the previous file is kept as log.old.txt.
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::PathBuf,
    sync::Mutex,
};

const MAX: u64 = 256 * 1024;
static LOCK: Mutex<()> = Mutex::new(());

/// %LOCALAPPDATA%\DefaultFast, created on demand. The only folder this app writes files to.
pub fn data_dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let dir = base.join("DefaultFast");
    let _ = fs::create_dir_all(&dir);
    dir
}

pub fn write(line: &str) {
    if cfg!(test) {
        return; // unit tests use MemRegistry; keep the user's real log clean
    }
    // A poisoned lock only means another thread panicked mid-write; logging must go on.
    let _guard = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let dir = data_dir();
    let path = dir.join("log.txt");
    if fs::metadata(&path).is_ok_and(|m| m.len() > MAX) {
        let _ = fs::rename(&path, dir.join("log.old.txt"));
    }
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "{} [{}] {line}", crate::shell::timestamp(), std::process::id());
    }
}
