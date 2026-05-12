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

// ── Public entry point ────────────────────────────────────────────────────────

/// Check for a newer FM release on GitHub and update the local database if one
/// is available.
///
/// Returns the version tag string that should be displayed in the overlay
/// (e.g. `"v2.55.1.88"`).  Returns an empty string if no version info is
/// available.
pub fn check_and_update_fm(fm_base: &Path) -> String {
    let local_tag = read_fm_version_tag(fm_base).unwrap_or_default();

    // ── 1. Query GitHub Releases API ─────────────────────────────────────────
    let http = match build_client() {
        Some(c) => c,
        None => {
            eprintln!("[fm_update] failed to build HTTP client");
            return local_tag;
        }
    };

    let resp = match http.get(RELEASES_API).send() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("[fm_update] API request failed: {e}");
            return local_tag;
        }
    };

    let json: serde_json::Value = match resp.json() {
        Ok(j) => j,
        Err(e) => {
            eprintln!("[fm_update] API response parse error: {e}");
            return local_tag;
        }
    };

    let remote_tag = match json["tag_name"].as_str() {
        Some(t) => t.to_string(),
        None => {
            eprintln!("[fm_update] no tag_name in GitHub response");
            return local_tag;
        }
    };

    // ── 2. Version comparison ─────────────────────────────────────────────────
    match (parse_quad(&local_tag), parse_quad(&remote_tag)) {
        (Some(l), Some(r)) if r <= l => {
            eprintln!("[fm_update] FM is up to date ({local_tag})");
            return local_tag;
        }
        (None, _) if local_tag.is_empty() => {
            eprintln!("[fm_update] no local FM version; downloading {remote_tag}");
        }
        _ => {
            eprintln!("[fm_update] FM update available: {local_tag} → {remote_tag}");
        }
    }

    // ── 3. Find the zip asset ─────────────────────────────────────────────────
    let download_url = match json["assets"]
        .as_array()
        .and_then(|arr| {
            arr.iter().find(|a| {
                a["name"].as_str().map(|n| n.ends_with(".zip")).unwrap_or(false)
            })
        })
        .and_then(|a| a["browser_download_url"].as_str())
    {
        Some(u) => u.to_string(),
        None => {
            eprintln!("[fm_update] no zip asset in release {remote_tag}");
            return local_tag;
        }
    };

    // ── 4. Download ───────────────────────────────────────────────────────────
    eprintln!("[fm_update] downloading {download_url}");
    let zip_bytes = match http.get(&download_url).send().and_then(|r| r.bytes()) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("[fm_update] download failed: {e}");
            return local_tag;
        }
    };
    eprintln!("[fm_update] downloaded {} bytes", zip_bytes.len());

    // ── 5. Back up existing data ──────────────────────────────────────────────
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

    // ── 6. Extract zip into fm_base ───────────────────────────────────────────
    let cursor = std::io::Cursor::new(zip_bytes.as_ref());
    let mut archive = match zip::ZipArchive::new(cursor) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("[fm_update] zip open error: {e}");
            return local_tag;
        }
    };

    for i in 0..archive.len() {
        let mut zf = match archive.by_index(i) {
            Ok(f) => f,
            Err(e) => { eprintln!("[fm_update] zip entry {i} error: {e}"); continue; }
        };

        // Sanitise: strip any leading `./` or absolute prefix.
        let rel = zf.name().trim_start_matches("./").to_string();
        if rel.is_empty() || rel.starts_with('/') || rel.contains("..") {
            continue; // skip suspicious paths
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
    }

    eprintln!("[fm_update] FM updated to {remote_tag}");

    // ── 7. Remove the backup dir now that the update succeeded ────────────────
    // Also prune any older version-tagged backup dirs left by previous updates.
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

    remote_tag
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
