//! Legacy UserChoice v1. Undocumented and deliberately gated by a live-format check.
//! Algorithm reference: Mozilla WindowsUserChoice.cpp; constants describe the Windows format.
use crate::ffi;
const EXPERIENCE: &str =
    "User Choice set via Windows User Experience {D18B6DD5-6124-4341-9318-804003BAFA0B}";

pub fn generate(
    extension: &str,
    sid: &str,
    prog_id: &str,
    filetime: u64,
) -> Result<String, String> {
    let minute = filetime / 600_000_000 * 600_000_000;
    let input = format!(
        "{extension}{sid}{prog_id}{:08x}{:08x}{EXPERIENCE}",
        minute >> 32,
        minute as u32
    );
    let mut utf16: Vec<u16> = input.encode_utf16().chain(Some(0)).collect();
    ffi::lowercase_windows(&mut utf16);
    let bytes: Vec<u8> = utf16.iter().flat_map(|v| v.to_le_bytes()).collect();
    let digest = ffi::md5(&bytes)?;
    Ok(mix(&bytes, &digest))
}

fn mix(bytes: &[u8], digest: &[u8; 16]) -> String {
    let word = |b: &[u8]| u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
    let seeds = [word(&digest[0..4]) | 1, word(&digest[4..8]) | 1];
    let a = [
        [0xcf98b111, 0x87085b9f, 0x12ceb96d, 0x257e1d83],
        [0xa27416f5, 0xd38396ff, 0x7c932b89, 0xbfa49f69],
    ];
    let b = [
        [0xef0569fb, 0x689b6b9f, 0x79f8a395, 0xc3efea97],
        [0xc31713db, 0xddcd1f0f, 0x59c3af2d, 0x35bd1ec9],
    ];
    let (mut x, mut y, mut sx, mut sy) = (0u32, 0u32, 0u32, 0u32);
    for block in bytes.as_chunks::<8>().0 {
        for j in 0..2 {
            let n = word(&block[j * 4..j * 4 + 4]);
            x = x.wrapping_add(n).wrapping_mul(seeds[j]);
            for multiplier in a[j] {
                x = x.rotate_left(16).wrapping_mul(multiplier);
            }
            y = y.wrapping_add(n);
            y = y
                .rotate_left(16)
                .wrapping_mul(b[j][0])
                .wrapping_add(y.wrapping_mul(seeds[j]));
            y = (y >> 16)
                .wrapping_mul(b[j][1])
                .wrapping_add(y.wrapping_mul(b[j][2]));
            y = y.rotate_left(16).wrapping_mul(b[j][3]).wrapping_add(y);
            sx = sx.wrapping_add(x);
            sy = sy.wrapping_add(y);
        }
    }
    let output: Vec<u8> = (x ^ y)
        .to_le_bytes()
        .into_iter()
        .chain((sx ^ sy).to_le_bytes())
        .collect();
    base64(&output)
}

fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for c in bytes.chunks(3) {
        let bits = ((c[0] as u32) << 16)
            | ((c.get(1).copied().unwrap_or(0) as u32) << 8)
            | c.get(2).copied().unwrap_or(0) as u32;
        for k in 0..4 {
            out.push(if k > c.len() {
                '='
            } else {
                TABLE[((bits >> (18 - 6 * k)) & 63) as usize] as char
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn base64_vectors() {
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }
    #[test]
    fn md5_rfc1321() {
        assert_eq!(
            ffi::md5(b"abc").expect("CNG MD5"),
            [
                0x90, 0x01, 0x50, 0x98, 0x3c, 0xd2, 0x4f, 0xb0, 0xd6, 0x96, 0x3f, 0x7d, 0x28, 0xe1,
                0x7f, 0x72
            ]
        );
    }
    #[test]
    fn minute_rounding() {
        let a = generate(".txt", "S-1-5-21-1", "txtfile", 133_000_000_000_000_000).expect("hash");
        let minute = 133_000_000_000_000_000u64 / 600_000_000 * 600_000_000;
        assert_eq!(
            a,
            generate(".TXT", "s-1-5-21-1", "TXTFILE", minute + 599_999_999).expect("hash")
        );
        assert_ne!(
            a,
            generate(".txt", "S-1-5-21-1", "txtfile", minute + 600_000_000).expect("hash")
        );
    }

    #[test]
    fn legacy_vectors_from_independent_ps_sfta_reference() {
        // Generated using only PS-SFTA's Get-Hash function (2026-10-30), with fixed inputs.
        // Timestamp is 01d882cb8348fc00, UTF-16LE + NUL, lowercased canonical v1 input.
        for (extension, expected) in [
            (".txt", "ieFZH30Ku3k="),
            (".bat", "HNKIYxnTkO0="),
            (".test", "RvRrc5Z89sA="),
        ] {
            assert_eq!(
                generate(extension, "S-1-5-21-1", "txtfile", 132_999_999_600_000_000)
                    .expect("CNG hash"),
                expected
            );
        }
    }
}
