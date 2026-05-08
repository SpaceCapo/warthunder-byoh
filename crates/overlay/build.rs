// build.rs — embed application icon into the Windows PE at compile time,
// generate a compile-time RGBA byte array for winit's set_window_icon(),
// and embed a git-derived build version string via BYOH_BUILD_VERSION.
//
// Uses windres directly (x86_64-w64-mingw32-windres) instead of the winres
// crate, which silently fails during Linux→Windows cross-compilation.

fn main() {
    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR not set");
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set");

    // ── Git-derived build version ────────────────────────────────────────────
    // Logic mirrors the shell one-liner:
    //   exact tag → branch-sha+dirty (non-main branch) → sha+dirty → YYYYmmdd-HHmmss
    let version = git_build_version();
    println!("cargo:rustc-env=BYOH_BUILD_VERSION={version}");

    // Re-run when the git state changes: checkout, commit, stage, new tag.
    // Also re-run when the Makefile injects a new GIT_VERSION (Docker builds).
    println!("cargo:rerun-if-env-changed=GIT_VERSION");
    let crate_root = std::path::Path::new(&manifest_dir);
    let ws_root = crate_root.parent().and_then(|p| p.parent()).unwrap_or(crate_root);
    for rel in &[".git/HEAD", ".git/index", ".git/packed-refs"] {
        println!("cargo:rerun-if-changed={}", ws_root.join(rel).display());
    }
    // The refs/heads directory — catch branch pointer updates.
    println!("cargo:rerun-if-changed={}", ws_root.join(".git/refs/heads").display());

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

// ── Git version helpers ───────────────────────────────────────────────────────

/// Compute a version string from the current git state, replicating the logic:
///   exact tag → branch-sha[+dirty] (non-main branch) → sha[+dirty] → timestamp
///
/// When a `GIT_VERSION` environment variable is set (injected by the Makefile
/// before invoking Docker), it is used directly.  This bypasses the
/// `safe.directory` ownership check that git performs when the working tree is
/// mounted into a container owned by a different UID.
fn git_build_version() -> String {
    // Makefile / CI can pre-compute and inject this when git isn't accessible
    // from inside the build container.
    if let Ok(v) = std::env::var("GIT_VERSION") {
        let v = v.trim().to_string();
        if !v.is_empty() { return v; }
    }

    let run = |args: &[&str]| -> Option<String> {
        std::process::Command::new("git")
            .args(args)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    };

    // 1. Exact tag on HEAD?
    if let Some(tag) = run(&["describe", "--tags", "--exact-match"]) {
        if !tag.is_empty() { return tag; }
    }

    // 2. Inside a git repo at all?
    if run(&["rev-parse", "--git-dir"]).is_none() {
        return build_timestamp();
    }

    // 3. Short SHA — if absent (empty repo / no commits) fall back to timestamp.
    let sha = match run(&["rev-parse", "--short", "HEAD"]) {
        Some(s) if !s.is_empty() => s,
        _ => return build_timestamp(),
    };

    // 4. Any staged or unstaged modifications (not untracked)?
    let dirty = run(&["status", "--porcelain", "--untracked-files=no"])
        .map(|s| !s.is_empty())
        .unwrap_or(false);
    let suffix = if dirty { "+dirty" } else { "" };

    // 5. Branch name — omit when on main or detached HEAD.
    let branch = run(&["rev-parse", "--abbrev-ref", "HEAD"]).unwrap_or_default();
    if branch != "main" && branch != "HEAD" && !branch.is_empty() {
        format!("{branch}-{sha}{suffix}")
    } else {
        format!("{sha}{suffix}")
    }
}

/// Fallback version: current UTC time as YYYYmmdd-HHmmss.
/// Implemented in pure Rust so it works without shell utilities.
fn build_timestamp() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let tod = (secs % 86400) as u32;
    let hh  = tod / 3600;
    let mm  = (tod % 3600) / 60;
    let ss  = tod % 60;

    // Howard Hinnant's algorithm: days since Unix epoch → (y, m, d).
    let z   = (secs / 86400) as i64 + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y0  = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp  = (5 * doy + 2) / 153;
    let d   = doy - (153 * mp + 2) / 5 + 1;
    let mo  = if mp < 10 { mp + 3 } else { mp - 9 };
    let y   = if mo <= 2 { y0 + 1 } else { y0 };

    format!("{y:04}{mo:02}{d:02}-{hh:02}{mm:02}{ss:02}")
}
