use crate::model::{App, FileType};
use std::{
    io::{self, Write},
    path::PathBuf,
};
const MAGIC: &[u8; 8] = b"DFPK\x02\0\0\0";
const MAX: usize = 16 * 1024 * 1024;

pub fn directory() -> io::Result<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")
        .ok_or_else(|| io::Error::other("LOCALAPPDATA is not defined"))?;
    let directory = PathBuf::from(base).join("DefaultPicker");
    if std::env::args().any(|a| a == "--self-test-association") {
        Ok(directory
            .join("SelfTests")
            .join(std::process::id().to_string()))
    } else {
        Ok(directory)
    }
}
pub fn atomic_write(name: &str, bytes: &[u8]) -> io::Result<()> {
    let dir = directory()?;
    std::fs::create_dir_all(&dir)?;
    let temp = dir.join(format!("{name}.{}.tmp", std::process::id()));
    let mut file = std::fs::File::create(&temp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    crate::ffi::replace_file(&temp, &dir.join(name)).map_err(io::Error::other)
}
pub fn read(name: &str) -> io::Result<Vec<u8>> {
    let path = directory()?.join(name);
    if std::fs::metadata(&path)?.len() > MAX as u64 {
        return Err(io::Error::other("File exceeds size limit"));
    }
    std::fs::read(path)
}
pub fn put_u32(out: &mut Vec<u8>, n: u32) {
    out.extend_from_slice(&n.to_le_bytes());
}
pub fn put_string(out: &mut Vec<u8>, s: &str) {
    put_u32(out, s.len() as u32);
    out.extend_from_slice(s.as_bytes());
}
pub struct Reader<'a>(pub &'a [u8]);
impl<'a> Reader<'a> {
    pub fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let s = self.0.get(..n)?;
        self.0 = &self.0[n..];
        Some(s)
    }
    pub fn number(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    pub fn string(&mut self) -> Option<String> {
        let n = self.number()? as usize;
        if n > 65536 {
            return None;
        }
        Some(std::str::from_utf8(self.take(n)?).ok()?.to_owned())
    }
}
fn checksum(b: &[u8]) -> u32 {
    b.iter()
        .fold(2166136261u32, |h, c| (h ^ *c as u32).wrapping_mul(16777619))
}
pub fn encode(types: &[FileType], apps: &[App]) -> Vec<u8> {
    let mut b = MAGIC.to_vec();
    put_u32(&mut b, types.len() as u32);
    put_u32(&mut b, apps.len() as u32);
    for t in types {
        for s in [&t.extension, &t.name, &t.prog_id, &t.current_app, &t.icon] {
            put_string(&mut b, s);
        }
    }
    for a in apps {
        for s in [&a.name, &a.executable, &a.prog_id, &a.publisher, &a.icon] {
            put_string(&mut b, s);
        }
    }
    let c = checksum(&b);
    put_u32(&mut b, c);
    b
}
pub fn decode(b: &[u8]) -> Option<(Vec<FileType>, Vec<App>)> {
    if b.len() < 20 || b.len() > MAX {
        return None;
    }
    let end = b.len() - 4;
    if checksum(&b[..end]) != u32::from_le_bytes(b[end..].try_into().ok()?) {
        return None;
    }
    let mut r = Reader(&b[..end]);
    if r.take(8)? != MAGIC {
        return None;
    }
    let nt = r.number()? as usize;
    let na = r.number()? as usize;
    if nt + na > 100_000 {
        return None;
    }
    let mut types = Vec::with_capacity(nt);
    let mut apps = Vec::with_capacity(na);
    for _ in 0..nt {
        let mut t = FileType {
            extension: r.string()?,
            name: r.string()?,
            prog_id: r.string()?,
            current_app: r.string()?,
            icon: r.string()?,
            index: String::new(),
        };
        if crate::search::normalize_extension(&t.extension).as_deref() != Some(t.extension.as_str())
        {
            return None;
        }
        t.reindex();
        types.push(t);
    }
    for _ in 0..na {
        let mut a = App {
            name: r.string()?,
            executable: r.string()?,
            prog_id: r.string()?,
            publisher: r.string()?,
            icon: r.string()?,
            index: String::new(),
            recommended: false,
        };
        a.reindex();
        apps.push(a);
    }
    if !r.0.is_empty() {
        return None;
    }
    Some((types, apps))
}
pub fn load() -> Option<(Vec<FileType>, Vec<App>)> {
    decode(&read("catalog.bin").ok()?)
}
pub fn save(types: &[FileType], apps: &[App]) -> io::Result<()> {
    atomic_write("catalog.bin", &encode(types, apps))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn round_trip_and_corruption() {
        let mut t = FileType {
            extension: ".文件".into(),
            name: "Unicode\0test".into(),
            ..FileType::default()
        };
        t.reindex();
        let mut a = App {
            name: "Terminal".into(),
            ..App::default()
        };
        a.reindex();
        let b = encode(&[t.clone()], &[a.clone()]);
        assert_eq!(decode(&b), Some((vec![t], vec![a])));
        for n in 0..b.len() {
            assert!(decode(&b[..n]).is_none());
        }
        let mut bad = b;
        bad[18] ^= 0xff;
        assert!(decode(&bad).is_none());
    }
}
