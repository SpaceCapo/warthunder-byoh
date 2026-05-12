//! FM database auto-update.
//!
//! On startup the overlay calls [`check_and_update_fm`], which:
//!
//! 1. Reads the local `version_tag` file (e.g. `v2.55.1.88`) from the FM base
//!    directory.
//! 2. Queries the GitHub Releases API for the latest release of the FM database
//!    repository.
//! 3. If the remote tag is newer (four-part version comparison), backs up the
//!    existing `fm/` tree to `fm/<old-tag>/` and downloads + extracts the new
//!    release zip.
//! 4. Returns the current (possibly just-updated) version tag string.
//!
//! All network errors are non-fatal — on any failure the function prints a
//! diagnostic and returns whatever version tag was already on disk.

use std::io::Read;
use std::path::{Path, PathBuf};

const RELEASES_API: &str =
    "https://api.github.com/repos/SpaceCapo/warthunder-byo-fm/releases/latest";

// ── Directory resolution ──────────────────────────────────────────────────────

/// Return the FM *base* directory: `<exe_dir>/fm/` when the exe is *not*
/// inside a `target/` build directory, otherwise `data/fm/` (for development).
///
/// The directory is **not** required to already exist — `check_and_update_fm`
/// will create it on first run.
///
/// Layout inside the base dir:
/// ```text
/// <fm_base>/
///   fm/
///     fm_data_db.csv
///     fm_names_db.csv
///   version        ← game version string, e.g. "2.55.1.88"
///   version_tag    ← release tag, e.g. "v2.55.1.88"
/// ```
pub fn fm_base_dir() -> PathBuf {
    if let Ok(exe) = std::env::current_exe() {
        let exe_dir = exe.parent().unwrap_or(Path::new("."));
        // Treat any path that contains a "target" component as a dev/build
        // directory and fall through to the data/fm fallback.
        let in_target = exe.components().any(|c| {
            c.as_os_str().to_string_lossy() == "target"
        });
        if !in_target {
            return exe_dir.join("fm");
        }
    }
    PathBuf::from("data").join("fm")
}

/// Read the `version_tag` file from `fm_base`.  Returns `None` if absent.
pub fn read_fm_version_tag(fm_base: &Path) -> Option<String> {
    std::fs::read_to_string(fm_base.join("version_tag"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

// ── Version parsing ───────────────────────────────────────────────────────────

/// Parse a tag like `v2.55.1.88` or `2.55.1.88` into a four-part tuple.
fn parse_quad(tag: &str) -> Option<(u32, u32, u32, u32)> {
    let s = tag.trim().trim_start_matches('v');
    let p: Vec<u32> = s.split('.').filter_map(|x| x.parse().ok()).collect();
    match p.as_slice() {
        [a, b, c, d] => Some((*a, *b, *c, *d)),
        [a, b, c]    => Some((*a, *b, *c,  0)),
        [a, b]       => Some((*a, *b,  0,  0)),
        _            => None,
    }
}

// ── HTTP helpers ──────────────────────────────────────────────────────────────

fn build_client() -> Option<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .user_agent(concat!("warthunder-byoh/", env!("CARGO_PKG_VERSION")))
        .build()
        .ok()
}

// ── Public entry points ───────────────────────────────────────────────────────

/// Check whether a newer FM release is available on GitHub **without** making
/// any local changes.
///
/// Returns `Some((remote_tag, download_url))` when an update is available,
/// `None` when the local database is already current or a network error occurs.
///
/// This is the lightweight half of the update flow — call it on a background
/// thread and present the result to the user before downloading anything.
pub fn check_fm_update_available(fm_base: &Path) -> Option<(String, String)> {
    let local_tag = read_fm_version_tag(fm_base).unwrap_or_default();

    let http = build_client()?;

    let resp = match http.get(RELEASES_API).send() {
        Ok(r) => r,
        Err(e) => { eprintln!("[fm_update] API request failed: {e}"); return None; }
    };

    let json: serde_json::Value = match resp.json() {
        Ok(j) => j,
        Err(e) => { eprintln!("[fm_update] API response parse error: {e}"); return None; }
    };

    let remote_tag = json["tag_name"].as_str()?.to_string();

    // Already up to date?
    if let (Some(l), Some(r)) = (parse_quad(&local_tag), parse_quad(&remote_tag)) {
        if r <= l {
            eprintln!("[fm_update] FM is up to date ({local_tag})");
            return None;
        }
    }

    let download_url = json["assets"]
        .as_array()
        .and_then(|arr| {
            arr.iter().find(|a| {
                a["name"].as_str().map(|n| n.ends_with(".zip")).unwrap_or(false)
            })
        })
        .and_then(|a| a["browser_download_url"].as_str())
        .map(|s| s.to_string())?;

    eprintln!("[fm_update] update available: {local_tag:?} → {remote_tag}");
    Some((remote_tag, download_url))
}

/// Download and extract an FM update that was previously located by
/// [`check_fm_update_available`].
///
/// `progress_cb` is called with values in `0.0..=1.0`:
/// - `0.0..0.9` — download progress
/// - `0.9..=1.0` — extract + cleanup progress
///
/// Returns the new version tag on success, or an error message on failure.
/// All network / IO errors are returned as `Err(String)` (non-fatal to caller).
pub fn install_fm_update(
    fm_base: &Path,
    remote_tag: &str,
    download_url: &str,
    progress_cb: impl Fn(f32),
) -> Result<String, String> {
    let local_tag = read_fm_version_tag(fm_base).unwrap_or_default();

    let http = build_client()
        .ok_or_else(|| "failed to build HTTP client".to_string())?;

    // ── 1. Streaming download with progress ───────────────────────────────────
    progress_cb(0.0);
    eprintln!("[fm_update] downloading {download_url}");

    let resp = http
        .get(download_url)
        .send()
        .map_err(|e| format!("download request failed: {e}"))?;

    let content_length = resp.content_length().unwrap_or(0);
    let mut zip_bytes: Vec<u8> = if content_length > 0 {
        Vec::with_capacity(content_length as usize)
    } else {
        Vec::new()
    };

    {
        let mut reader = resp;
        let mut chunk = [0u8; 65536];
        loop {
            let n = reader
                .read(&mut chunk)
                .map_err(|e| format!("download read error: {e}"))?;
            if n == 0 { break; }
            zip_bytes.extend_from_slice(&chunk[..n]);
            if content_length > 0 {
                let frac = (zip_bytes.len() as f64 / content_length as f64 * 0.9) as f32;
                progress_cb(frac.min(0.89));
            }
        }
    }
    eprintln!("[fm_update] downloaded {} bytes", zip_bytes.len());
    progress_cb(0.9);

    // ── 2. Back up existing data ──────────────────────────────────────────────
    if !local_tag.is_empty() && fm_base.join("version_tag").exists() {
        let backup_dir = fm_base.join(&local_tag);
        match std::fs::create_dir_all(&backup_dir) {
            Err(e) => eprintln!("[fm_update] backup dir create error: {e}"),
            Ok(()) => {
                for entry in &["fm", "version", "version_tag"] {
                    let src = fm_base.join(entry);
                    if src.exists() {
                        let dst = backup_dir.join(entry);
                        if let Err(e) = std::fs::rename(&src, &dst) {
                            eprintln!("[fm_update] backup move {entry}: {e}");
                        }
                    }
                }
                eprintln!("[fm_update] old data backed up to {}", backup_dir.display());
            }
        }
    }

    // ── 3. Extract zip into fm_base ───────────────────────────────────────────
    let cursor = std::io::Cursor::new(zip_bytes.as_slice());
    let mut archive = zip::ZipArchive::new(cursor)
        .map_err(|e| format!("zip open error: {e}"))?;

    let total = archive.len().max(1) as f32;
    for i in 0..archive.len() {
        let mut zf = match archive.by_index(i) {
            Ok(f) => f,
            Err(e) => { eprintln!("[fm_update] zip entry {i} error: {e}"); continue; }
        };

        let rel = zf.name().trim_start_matches("./").to_string();
        if rel.is_empty() || rel.starts_with('/') || rel.contains("..") {
            continue;
        }

        let out_path = fm_base.join(&rel);

        if zf.is_dir() {
            if let Err(e) = std::fs::create_dir_all(&out_path) {
                eprintln!("[fm_update] mkdir {}: {e}", out_path.display());
            }
            continue;
        }

        if let Some(parent) = out_path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                eprintln!("[fm_update] mkdir {}: {e}", parent.display());
                continue;
            }
        }

        let mut out_file = match std::fs::File::create(&out_path) {
            Ok(f) => f,
            Err(e) => { eprintln!("[fm_update] create {}: {e}", out_path.display()); continue; }
        };

        let mut buf = Vec::new();
        if let Err(e) = zf.read_to_end(&mut buf) {
            eprintln!("[fm_update] read zip entry {rel}: {e}");
            continue;
        }
        if let Err(e) = std::io::Write::write_all(&mut out_file, &buf) {
            eprintln!("[fm_update] write {}: {e}", out_path.display());
        }

        // 0.9 → 1.0 over extraction
        let extract_frac = 0.9 + (i as f32 + 1.0) / total * 0.1;
        progress_cb(extract_frac.min(0.99));
    }

    eprintln!("[fm_update] FM updated to {remote_tag}");

    // ── 4. Remove stale backup dirs ───────────────────────────────────────────
    if let Ok(entries) = std::fs::read_dir(fm_base) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() { continue; }
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if parse_quad(name).is_some() {
                    match std::fs::remove_dir_all(&path) {
                        Ok(()) => eprintln!("[fm_update] removed old backup {}", path.display()),
                        Err(e) => eprintln!("[fm_update] could not remove backup {}: {e}", path.display()),
                    }
                }
            }
        }
    }

    progress_cb(1.0);
    Ok(remote_tag.to_string())
}

/// Convenience wrapper: check for an FM update and, if one is available,
/// silently download and install it.
///
/// Used by the GDI path (no settings UI) and by `[check_setup_needs]`-driven
/// first-run downloads.  The GPU path uses [`check_fm_update_available`] +
/// [`install_fm_update`] separately so the user can confirm before downloading.
///
/// Returns the current (possibly just-updated) version tag string.
pub fn check_and_update_fm(fm_base: &Path) -> String {
    let local_tag = read_fm_version_tag(fm_base).unwrap_or_default();

    match check_fm_update_available(fm_base) {
        None => local_tag,
        Some((remote_tag, download_url)) => {
            match install_fm_update(fm_base, &remote_tag, &download_url, |_| {}) {
                Ok(tag) => tag,
                Err(e) => {
                    eprintln!("[fm_update] install failed: {e}");
                    local_tag
                }
            }
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::parse_quad;

    #[test]
    fn parse_quad_strips_v_prefix() {
        assert_eq!(parse_quad("v2.55.1.88"), Some((2, 55, 1, 88)));
        assert_eq!(parse_quad("2.55.1.88"),  Some((2, 55, 1, 88)));
    }

    #[test]
    fn parse_quad_three_part() {
        assert_eq!(parse_quad("v1.2.3"), Some((1, 2, 3, 0)));
    }

    #[test]
    fn parse_quad_comparison() {
        let old = parse_quad("v2.55.1.62").unwrap();
        let new = parse_quad("v2.55.1.88").unwrap();
        assert!(new > old);
    }

    #[test]
    fn parse_quad_equal() {
        let a = parse_quad("v2.55.1.88").unwrap();
        let b = parse_quad("v2.55.1.88").unwrap();
        assert!(b <= a);
    }

    #[test]
    fn parse_quad_bad_input() {
        assert_eq!(parse_quad("not-a-version"), None);
        assert_eq!(parse_quad(""), None);
    }
}
