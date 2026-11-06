//! Everything about the per-user `UserChoice` key and its `Hash`, isolated here.
//!
//! Windows ignores (and may reset) a UserChoice whose `Hash` doesn't match. The legacy
//! algorithm, reverse-engineered by SetUserFTA / PS-SFTA (and used by Mozilla), is:
//!
//! 1. input = lowercase(target + user SID + ProgId + hex(FILETIME of the key's last write,
//!    truncated to the minute, as two %08x halves) + EXPERIENCE), UTF-16LE including the NUL;
//! 2. MD5(input) supplies two odd 32-bit seeds;
//! 3. two multiply/rotate checksum streams (`mix`) run over the input in 8-byte blocks;
//! 4. the 8 output bytes are base64-encoded.
//!
//! It is undocumented: a Windows update can change it. Newer Windows 11 builds add
//! `UserChoiceLatest` with a different, unpublished hash — we never fake that one; such
//! targets go to the Settings fallback. Every write is verified through Shell afterwards.
use crate::{
    error::{Error, Result},
    registry::{Hive, Registry, Value},
};

pub const EXPERIENCE: &str = "User Choice set via Windows User Experience {D18B6DD5-6124-4341-9318-804003BAFA0B}";
const HKCU: Hive = Hive::CurrentUser;
/// FILETIME ticks per minute (100 ns units).
const MINUTE: u64 = 600_000_000;

/// HKCU parent key holding UserChoice for an extension (".mp4") or protocol ("https").
pub fn base_key(target: &str) -> String {
    if target.starts_with('.') {
        format!("Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\FileExts\\{target}")
    } else {
        format!("Software\\Microsoft\\Windows\\Shell\\Associations\\UrlAssociations\\{target}")
    }
}
pub fn choice_key(target: &str) -> String {
    format!("{}\\UserChoice", base_key(target))
}
pub fn latest_key(target: &str) -> String {
    format!("{}\\UserChoiceLatest", base_key(target))
}

/// Simple per-char lowercase matching CharLowerBuffW for the inputs we hash (extensions,
/// SIDs, ProgIDs): multi-char expansions (e.g. 'İ') are left unchanged, as Windows does.
fn lowercase(s: &str) -> String {
    s.chars()
        .map(|c| {
            let mut l = c.to_lowercase();
            match (l.next(), l.next()) {
                (Some(x), None) => x,
                _ => c,
            }
        })
        .collect()
}

pub fn hash(target: &str, sid: &str, prog_id: &str, filetime: u64) -> String {
    let minute = filetime / MINUTE * MINUTE;
    let input = lowercase(&format!("{target}{sid}{prog_id}{:08x}{:08x}{EXPERIENCE}", minute >> 32, minute as u32));
    let bytes: Vec<u8> = input.encode_utf16().chain(Some(0)).flat_map(u16::to_le_bytes).collect();
    base64(&mix(&bytes, &md5(&bytes)))
}

/// The two checksum streams (constants from the reverse-engineered algorithm).
fn mix(bytes: &[u8], digest: &[u8; 16]) -> [u8; 8] {
    let word = |b: &[u8]| u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
    let seeds = [word(&digest[0..4]) | 1, word(&digest[4..8]) | 1];
    const A: [[u32; 4]; 2] = [[0xcf98b111, 0x87085b9f, 0x12ceb96d, 0x257e1d83], [0xa27416f5, 0xd38396ff, 0x7c932b89, 0xbfa49f69]];
    const B: [[u32; 4]; 2] = [[0xef0569fb, 0x689b6b9f, 0x79f8a395, 0xc3efea97], [0xc31713db, 0xddcd1f0f, 0x59c3af2d, 0x35bd1ec9]];
    let (mut x, mut y, mut sx, mut sy) = (0u32, 0u32, 0u32, 0u32);
    // Only whole 8-byte blocks count; a trailing partial block is ignored by design.
    for block in bytes.chunks_exact(8) {
        for j in 0..2 {
            let n = word(&block[j * 4..j * 4 + 4]);
            x = x.wrapping_add(n).wrapping_mul(seeds[j]);
            for m in A[j] {
                x = x.rotate_left(16).wrapping_mul(m);
            }
            y = y.wrapping_add(n);
            y = y.rotate_left(16).wrapping_mul(B[j][0]).wrapping_add(y.wrapping_mul(seeds[j]));
            y = (y >> 16).wrapping_mul(B[j][1]).wrapping_add(y.wrapping_mul(B[j][2]));
            y = y.rotate_left(16).wrapping_mul(B[j][3]).wrapping_add(y);
            sx = sx.wrapping_add(x);
            sy = sy.wrapping_add(y);
        }
    }
    let mut out = [0u8; 8];
    out[..4].copy_from_slice(&(x ^ y).to_le_bytes());
    out[4..].copy_from_slice(&(sx ^ sy).to_le_bytes());
    out
}

fn base64(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for c in bytes.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        for k in 0..4 {
            out.push(if k > c.len() { '=' } else { T[(n >> (18 - 6 * k) & 63) as usize] as char });
        }
    }
    out
}

/// RFC 1321 MD5. Pure Rust so the hash module is testable without the OS (and MD5 here is a
/// format requirement, not a security primitive).
fn md5(input: &[u8]) -> [u8; 16] {
    const S: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20,
        4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
    ];
    const K: [u32; 64] = [
        0xd76aa478, 0xe8c7b756, 0x242070db, 0xc1bdceee, 0xf57c0faf, 0x4787c62a, 0xa8304613, 0xfd469501,
        0x698098d8, 0x8b44f7af, 0xffff5bb1, 0x895cd7be, 0x6b901122, 0xfd987193, 0xa679438e, 0x49b40821,
        0xf61e2562, 0xc040b340, 0x265e5a51, 0xe9b6c7aa, 0xd62f105d, 0x02441453, 0xd8a1e681, 0xe7d3fbc8,
        0x21e1cde6, 0xc33707d6, 0xf4d50d87, 0x455a14ed, 0xa9e3e905, 0xfcefa3f8, 0x676f02d9, 0x8d2a4c8a,
        0xfffa3942, 0x8771f681, 0x6d9d6122, 0xfde5380c, 0xa4beea44, 0x4bdecfa9, 0xf6bb4b60, 0xbebfbc70,
        0x289b7ec6, 0xeaa127fa, 0xd4ef3085, 0x04881d05, 0xd9d4d039, 0xe6db99e5, 0x1fa27cf8, 0xc4ac5665,
        0xf4292244, 0x432aff97, 0xab9423a7, 0xfc93a039, 0x655b59c3, 0x8f0ccc92, 0xffeff47d, 0x85845dd1,
        0x6fa87e4f, 0xfe2ce6e0, 0xa3014314, 0x4e0811a1, 0xf7537e82, 0xbd3af235, 0x2ad7d2bb, 0xeb86d391,
    ];
    let mut msg = Vec::with_capacity((input.len() + 9).div_ceil(64) * 64);
    msg.extend_from_slice(input);
    let bit_len = (input.len() as u64).wrapping_mul(8);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_le_bytes());
    let mut h = [0x67452301u32, 0xefcdab89, 0x98badcfe, 0x10325476];
    for chunk in msg.chunks_exact(64) {
        let mut m = [0u32; 16];
        for (dst, w) in m.iter_mut().zip(chunk.chunks_exact(4)) {
            *dst = u32::from_le_bytes([w[0], w[1], w[2], w[3]]);
        }
        let [mut a, mut b, mut c, mut d] = h;
        for i in 0..64 {
            let (f, g) = match i / 16 {
                0 => ((b & c) | (!b & d), i),
                1 => ((d & b) | (!d & c), (5 * i + 1) % 16),
                2 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | !d), (7 * i) % 16),
            };
            let f = f.wrapping_add(a).wrapping_add(K[i]).wrapping_add(m[g]);
            a = d;
            d = c;
            c = b;
            b = b.wrapping_add(f.rotate_left(S[i]));
        }
        for (x, y) in h.iter_mut().zip([a, b, c, d]) {
            *x = x.wrapping_add(y);
        }
    }
    let mut out = [0u8; 16];
    for (i, v) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
    }
    out
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Choice {
    pub prog_id: String,
    pub hash: String,
    pub time: u64,
}

pub fn read(reg: &dyn Registry, target: &str) -> Option<Choice> {
    let key = choice_key(target);
    Some(Choice {
        prog_id: reg.string(HKCU, &key, "ProgId")?,
        hash: reg.string(HKCU, &key, "Hash").unwrap_or_default(),
        time: reg.last_write(HKCU, &key)?,
    })
}

/// Some(true/false) when a legacy UserChoice exists and its hash does / doesn't validate.
pub fn validate(reg: &dyn Registry, target: &str, sid: &str) -> Option<bool> {
    let c = read(reg, target)?;
    Some(hash(target, sid, &c.prog_id, c.time) == c.hash)
}

/// Whether Windows uses the newer, unpublished UserChoiceLatest format for this target.
pub fn uses_latest(reg: &dyn Registry, target: &str) -> bool {
    reg.exists(HKCU, &latest_key(target))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Algorithm {
    /// At least one existing UserChoice validates: our hash matches this Windows build.
    Confirmed,
    /// No UserChoice keys to compare against (fresh profile); rely on post-write verification.
    Unconfirmed,
    /// UserChoice keys exist but none validates: the algorithm changed. Don't write.
    Mismatch,
}

/// Compares our hash with what Windows itself wrote on this machine (sampled).
pub fn algorithm_status(reg: &dyn Registry, sid: &str) -> (Algorithm, usize, usize) {
    let (mut valid, mut invalid) = (0usize, 0usize);
    let parents = [base_key(".x"), base_key("x")].map(|k| k.rsplit_once('\\').map(|p| p.0.to_owned()).unwrap_or_default());
    'outer: for parent in parents {
        for t in reg.subkeys(HKCU, &parent) {
            if valid + invalid >= 200 {
                break 'outer;
            }
            let target = t.to_lowercase();
            match validate(reg, &target, sid) {
                Some(true) => valid += 1,
                Some(false) => invalid += 1,
                None => {}
            }
        }
    }
    let status = if valid > 0 {
        Algorithm::Confirmed
    } else if invalid > 0 {
        Algorithm::Mismatch
    } else {
        Algorithm::Unconfirmed
    };
    (status, valid, invalid)
}

/// Writes UserChoice = `prog_id` with a matching Hash. The key is deleted and recreated
/// (existing UserChoice keys carry a deny-SetValue ACE), and the write is retried if the
/// minute rolls over between writing ProgId and Hash.
pub fn write(reg: &dyn Registry, target: &str, prog_id: &str, sid: &str) -> Result<()> {
    let key = choice_key(target);
    for _ in 0..3 {
        reg.delete_tree(HKCU, &key)?;
        reg.set(HKCU, &key, "ProgId", &Value::Sz(prog_id.to_owned()))?;
        let t1 = reg.last_write(HKCU, &key).ok_or_else(|| Error::Invalid("Cannot read the UserChoice key time".into()))?;
        let h = hash(target, sid, prog_id, t1);
        reg.set(HKCU, &key, "Hash", &Value::Sz(h.clone()))?;
        let t2 = reg.last_write(HKCU, &key).unwrap_or(0);
        if t1 / MINUTE == t2 / MINUTE && reg.string(HKCU, &key, "ProgId").as_deref() == Some(prog_id) && reg.string(HKCU, &key, "Hash").as_deref() == Some(h.as_str()) {
            return Ok(());
        }
    }
    Err(Error::Invalid("The registry clock crossed a minute three times in a row; try again".into()))
}

/// Removes the user's choice so Windows falls back to the registered default.
pub fn clear(reg: &dyn Registry, target: &str) -> Result<()> {
    reg.delete_tree(HKCU, &choice_key(target))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::MemRegistry;

    #[test]
    fn md5_rfc1321_vectors() {
        let hex = |d: [u8; 16]| d.iter().map(|b| format!("{b:02x}")).collect::<String>();
        assert_eq!(hex(md5(b"")), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(hex(md5(b"abc")), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(hex(md5(b"12345678901234567890123456789012345678901234567890123456789012345678901234567890")), "57edf4a22be3c955ac49da2e2107b67a");
    }

    #[test]
    fn base64_vectors() {
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    /// Fixed vectors computed independently with PS-SFTA's Get-Hash (timestamp
    /// 0x01d882cb8348fc00, SID S-1-5-21-1, ProgId txtfile).
    #[test]
    fn ps_sfta_vectors() {
        for (ext, expected) in [(".txt", "ieFZH30Ku3k="), (".bat", "HNKIYxnTkO0="), (".test", "RvRrc5Z89sA=")] {
            assert_eq!(hash(ext, "S-1-5-21-1", "txtfile", 132_999_999_600_000_000), expected, "{ext}");
        }
    }

    #[test]
    fn minute_truncation_and_case() {
        let base = 133_000_000_000_000_000u64 / MINUTE * MINUTE;
        let a = hash(".txt", "S-1-5-21-1", "txtfile", base);
        assert_eq!(a, hash(".TXT", "s-1-5-21-1", "TXTFILE", base + MINUTE - 1));
        assert_ne!(a, hash(".txt", "S-1-5-21-1", "txtfile", base + MINUTE));
    }

    #[test]
    fn write_validates_and_detects_tampering() {
        let r = MemRegistry::new();
        write(&r, ".mp4", "VLC.mp4", "S-1-5-21-9").unwrap();
        assert_eq!(validate(&r, ".mp4", "S-1-5-21-9"), Some(true));
        assert_eq!(read(&r, ".mp4").unwrap().prog_id, "VLC.mp4");
        // A ProgId edited without a new hash is what Windows rejects.
        r.seed(HKCU, &choice_key(".mp4"), "ProgId", Value::Sz("Evil.mp4".into()));
        assert_eq!(validate(&r, ".mp4", "S-1-5-21-9"), Some(false));
        assert_eq!(algorithm_status(&r, "S-1-5-21-9").0, Algorithm::Mismatch);
        write(&r, "https", "ChromeHTML", "S-1-5-21-9").unwrap();
        assert_eq!(algorithm_status(&r, "S-1-5-21-9").0, Algorithm::Confirmed);
    }

    #[test]
    fn denied_write_surfaces_access_denied() {
        let r = MemRegistry::new();
        r.deny.lock().unwrap().push(choice_key("https").to_lowercase());
        let e = write(&r, "https", "ChromeHTML", "S-1").unwrap_err();
        assert!(e.is_access_denied() && e.wants_settings());
    }
}
