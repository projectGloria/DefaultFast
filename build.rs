fn main() {
    let manifest = std::path::PathBuf::from(
        std::env::var_os("CARGO_MANIFEST_DIR").expect("Cargo manifest directory"),
    )
    .join("app.manifest");
    println!("cargo:rustc-link-arg-bin=defaultpicker=/MANIFEST:EMBED");
    println!(
        "cargo:rustc-link-arg-bin=defaultpicker=/MANIFESTINPUT:{}",
        manifest.display()
    );
    println!("cargo:rerun-if-changed=app.manifest");
}
