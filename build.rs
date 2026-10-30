//! Build script: embeds the manifest, the VERSIONINFO resource (from Cargo.toml) and a
//! procedurally drawn app icon. The icon is generated here so the repo carries no binary assets.
use std::{env, fs, path::PathBuf};

fn main() {
    let manifest_dir = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("manifest dir"));
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("out dir"));

    // The manifest goes through the linker (not the .rc) so it never collides with a
    // default manifest; it enables ComCtl v6, PerMonitorV2 DPI and asInvoker.
    let manifest = manifest_dir.join("app.manifest");
    println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
    println!("cargo:rustc-link-arg-bins=/MANIFESTINPUT:{}", manifest.display());
    println!("cargo:rerun-if-changed=app.manifest");
    println!("cargo:rerun-if-changed=build.rs");

    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let icon = out.join("defaultfast.ico");
    fs::write(&icon, ico(&[16, 20, 24, 32, 40, 48, 64])).expect("write icon");

    let version = env::var("CARGO_PKG_VERSION").expect("version");
    let mut res = winresource::WindowsResource::new();
    res.set_icon(&icon.to_string_lossy())
        .set("ProductName", "DefaultFast")
        .set("FileDescription", "DefaultFast - set and repair default apps")
        .set("InternalName", "DefaultFast.exe")
        .set("OriginalFilename", "DefaultFast.exe")
        .set("ProductVersion", &version)
        .set("FileVersion", &version)
        .set("LegalCopyright", "MIT License");
    res.compile().expect("compile Windows resources (needs rc.exe from the Windows SDK)");
}

/// Draws the icon: a rounded accent square with a white check mark, anti-aliased by
/// signed-distance coverage. Stored as 32-bit BMP entries (PNG is unnecessary below 256 px).
fn ico(sizes: &[u32]) -> Vec<u8> {
    let images: Vec<Vec<u8>> = sizes.iter().map(|&s| bmp_entry(s)).collect();
    let mut out = Vec::new();
    out.extend_from_slice(&[0, 0, 1, 0]);
    out.extend_from_slice(&(sizes.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * sizes.len() as u32;
    for (s, img) in sizes.iter().zip(&images) {
        out.push(*s as u8);
        out.push(*s as u8);
        out.extend_from_slice(&[0, 0, 1, 0, 32, 0]);
        out.extend_from_slice(&(img.len() as u32).to_le_bytes());
        out.extend_from_slice(&offset.to_le_bytes());
        offset += img.len() as u32;
    }
    for img in images {
        out.extend_from_slice(&img);
    }
    out
}

fn bmp_entry(size: u32) -> Vec<u8> {
    let s = size as f32;
    let radius = s * 0.22;
    let half = s / 2.0;
    let stroke = (s * 0.075).max(1.1);
    // Check mark polyline in unit coordinates.
    let pts = [(0.28, 0.53), (0.44, 0.68), (0.73, 0.36)];
    let mut px = vec![0u8; (size * size * 4) as usize];
    for y in 0..size {
        for x in 0..size {
            let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
            // Rounded-rect signed distance.
            let qx = (fx - half).abs() - (half - radius - 0.5);
            let qy = (fy - half).abs() - (half - radius - 0.5);
            let outside = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt() + qx.max(qy).min(0.0);
            let shape = (radius - outside).clamp(0.0, 1.0);
            // Distance to the check mark.
            let mut d = f32::MAX;
            for w in pts.windows(2) {
                let (ax, ay, bx, by) = (w[0].0 * s, w[0].1 * s, w[1].0 * s, w[1].1 * s);
                let (vx, vy) = (bx - ax, by - ay);
                let t = (((fx - ax) * vx + (fy - ay) * vy) / (vx * vx + vy * vy)).clamp(0.0, 1.0);
                d = d.min(((fx - ax - t * vx).powi(2) + (fy - ay - t * vy).powi(2)).sqrt());
            }
            let mark = (stroke - d + 0.5).clamp(0.0, 1.0);
            // Windows 11 default accent #0067C0 blended toward white for the mark.
            let (r, g, b) = (
                0.0 + 255.0 * mark,
                103.0 + 152.0 * mark,
                192.0 + 63.0 * mark,
            );
            // BMP rows are bottom-up, BGRA, straight alpha.
            let i = (((size - 1 - y) * size + x) * 4) as usize;
            px[i] = b as u8;
            px[i + 1] = g as u8;
            px[i + 2] = r as u8;
            px[i + 3] = (shape * 255.0) as u8;
        }
    }
    let mask_row = size.div_ceil(32) * 4;
    let mut out = Vec::new();
    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&(size as i32).to_le_bytes());
    out.extend_from_slice(&((size * 2) as i32).to_le_bytes()); // XOR + AND mask height
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&32u16.to_le_bytes());
    out.extend_from_slice(&[0; 24]);
    out.extend_from_slice(&px);
    out.extend(std::iter::repeat_n(0u8, (mask_row * size) as usize));
    out
}
