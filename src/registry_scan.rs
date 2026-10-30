use crate::{ffi, model::FileType};
use std::collections::BTreeSet;

pub fn scan(mut emit: impl FnMut(Vec<FileType>), cancelled: impl Fn() -> bool) {
    let mut seen = BTreeSet::new();
    let mut batch = Vec::with_capacity(64);
    for raw_extension in ffi::subkeys(ffi::Root::Classes, "") {
        if cancelled() {
            return;
        }
        if !raw_extension.starts_with('.') {
            continue;
        }
        let Some(extension) = crate::search::normalize_extension(&raw_extension) else {
            continue;
        };
        if !seen.insert(extension.clone()) {
            continue;
        }
        let prog_id = ffi::effective(&extension, false).unwrap_or_else(|| {
            ffi::reg_string(ffi::Root::Classes, &extension, "").unwrap_or_default()
        });
        let name = ffi::friendly_type(&extension)
            .or_else(|| ffi::reg_string(ffi::Root::Classes, &prog_id, ""))
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| {
                format!("{} file", extension.trim_start_matches('.').to_uppercase())
            });
        let mut row = FileType {
            icon: ffi::reg_string(ffi::Root::Classes, &format!("{prog_id}\\DefaultIcon"), "")
                .unwrap_or_default(),
            current_app: ffi::effective(&extension, true).unwrap_or_default(),
            extension,
            name,
            prog_id,
            index: String::new(),
        };
        if row.extension == ".bat" {
            row.name = "Windows Batch File".into();
        }
        if row.extension == ".cmd" {
            row.name = "Windows Command Script".into();
        }
        row.reindex();
        batch.push(row);
        if batch.len() == 64 {
            emit(std::mem::replace(&mut batch, Vec::with_capacity(64)));
        }
    }
    if !batch.is_empty() {
        emit(batch);
    }
}
