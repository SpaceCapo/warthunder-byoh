// build.rs — embed application icon into the Windows PE at compile time,
// and generate a compile-time RGBA byte array for winit's set_window_icon().
//
// Uses windres directly (x86_64-w64-mingw32-windres) instead of the winres
// crate, which silently fails during Linux→Windows cross-compilation.

fn main() {
    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR not set");
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set");

    // ── Generate icon_rgba.rs (all platforms) ────────────────────────────────
    // Decode the 32×32 PNG at build time into a flat RGBA array so the runtime
    // binary can create a winit::window::Icon without a PNG decoder dependency.
    let png_path = format!("{}/assets/icons/32.png", manifest_dir);
    let img = image::open(&png_path)
        .unwrap_or_else(|e| panic!("failed to open {png_path}: {e}"))
        .into_rgba8();
    let (w, h) = img.dimensions();
    let rgba: Vec<u8> = img.into_raw();

    // Write as a Rust source file included at compile time.
    let gen_path = format!("{}/icon_rgba.rs", out_dir);
    let pixel_literals: String = rgba.iter()
        .map(|b| format!("{b}u8"))
        .collect::<Vec<_>>()
        .join(",");
    std::fs::write(
        &gen_path,
        format!(
            "pub const ICON_RGBA: &[u8] = &[{pixel_literals}];\n\
             pub const ICON_WIDTH: u32 = {w};\n\
             pub const ICON_HEIGHT: u32 = {h};\n"
        ),
    )
    .unwrap_or_else(|e| panic!("failed to write {gen_path}: {e}"));

    println!("cargo:rerun-if-changed=assets/icons/32.png");

    // ── Windows PE resource (icon in Explorer / taskbar) ─────────────────────
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let rc_path = format!("{}/assets/app.rc", manifest_dir);
        let obj_path = format!("{}/app_icon.o", out_dir);

        // The Docker builder ships this as x86_64-w64-mingw32-windres.
        // Allow override via env var for other toolchains.
        let windres = std::env::var("WINDRES")
            .unwrap_or_else(|_| "x86_64-w64-mingw32-windres".to_string());

        let status = std::process::Command::new(&windres)
            .arg(&rc_path)
            .arg("-o")
            .arg(&obj_path)
            .arg(format!("--include-dir={}/assets", manifest_dir))
            .status()
            .unwrap_or_else(|e| panic!("failed to run {windres}: {e}"));

        assert!(status.success(), "{windres} exited with {status}");

        println!("cargo:rustc-link-arg={}", obj_path);
        println!("cargo:rerun-if-changed=assets/app.rc");
        println!("cargo:rerun-if-changed=assets/icon.ico");
    }
}
