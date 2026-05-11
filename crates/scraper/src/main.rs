//! War Thunder API field scraper.
//!
//! Hits live WT local API endpoints, collects every JSON key, and writes/merges
//! `data/fields.json` so the overlay can have a complete field catalog.
//!
//! Usage:
//!   cargo run -p scraper -- --out data/fields.json
//!
//! If the game is not running the tool will still write any fields it can reach,
//! and skip endpoints that are offline with a warning.

use clap::Parser;
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use std::collections::HashMap;
use std::path::PathBuf;

// ── CLI ───────────────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(about = "Scrape WT local API endpoints and write/merge data/fields.json")]
struct Args {
    /// Base URL for the WT local API.
    #[arg(long, default_value = "http://localhost:8111")]
    base: String,

    /// Output path for the field catalog JSON.
    #[arg(long, default_value = "data/fields.json")]
    out: PathBuf,

    /// Override the FM root directory (where version/fm/ live).
    #[arg(long)]
    fm_dir: Option<PathBuf>,

    /// Override the config directory (where indicators.json lives).
    #[arg(long)]
    config_dir: Option<PathBuf>,
}

// ── Field catalog entry ───────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct FieldEntry {
    /// Stable snake_case identifier used in formulas.
    pub id: String,
    /// Which endpoint this field comes from.
    pub endpoint: String,
    /// The raw JSON key as returned by the WT API (may contain spaces and commas).
    pub api_key: String,
    /// Human-readable short label (editable by user).
    pub label: String,
    /// Unit string (editable by user).
    pub unit: String,
    /// JSON value type: "f64", "bool", or "string".
    #[serde(rename = "type")]
    pub field_type: String,
}

// ── Endpoints to scrape ───────────────────────────────────────────────────────

const ENDPOINTS: &[&str] = &["/state", "/indicators", "/mission.json"];

// ── Key → id normalisation ────────────────────────────────────────────────────

/// Convert a raw API key like `"throttle 1, %"` into a stable snake_case id
/// like `"throttle_1_pct"`.
fn key_to_id(endpoint: &str, key: &str) -> String {
    // Strip leading slash and extension for prefix
    let prefix = endpoint
        .trim_start_matches('/')
        .trim_end_matches(".json")
        .replace('/', "_");

    let s = key
        .to_lowercase()
        // common unit suffixes → short names
        .replace(", %", "_pct")
        .replace(", km/h", "_kmh")
        .replace(", m/s", "_ms")
        .replace(", m", "_m")
        .replace(", kg", "_kg")
        .replace(", hp", "_hp")
        .replace(", rpm", "_rpm")
        .replace(", c", "_c")
        .replace(", g", "_g")
        .replace(", deg", "_deg")
        // remaining punctuation → underscores
        .replace([' ', ',', '/', '\\', '-', '(', ')', '.'], "_");

    // Collapse multiple underscores
    let mut id = String::new();
    let mut prev_us = false;
    for ch in s.chars() {
        if ch == '_' {
            if !prev_us { id.push('_'); }
            prev_us = true;
        } else {
            id.push(ch);
            prev_us = false;
        }
    }
    let id = id.trim_matches('_').to_string();

    // Prepend endpoint prefix only when needed to disambiguate (mission.json keys)
    if prefix == "mission" || prefix == "indicators" {
        format!("{}_{}", prefix, id)
    } else {
        id
    }
}

/// Infer a short unit string from the raw API key.
fn infer_unit(key: &str) -> String {
    let k = key.to_lowercase();
    if k.contains(", %") { return "%".into(); }
    if k.contains(", km/h") { return "km/h".into(); }
    if k.contains(", m/s") { return "m/s".into(); }
    if k.contains(", m") && !k.contains("rpm") { return "m".into(); }
    if k.contains(", kg") { return "kg".into(); }
    if k.contains(", hp") { return "hp".into(); }
    if k.contains(", rpm") { return "rpm".into(); }
    if k.contains(", c") { return "°C".into(); }
    if k.contains(", g") { return "g".into(); }
    if k.contains(", deg") { return "°".into(); }
    String::new()
}

/// Derive a short label from an id, e.g. "altitude_m" → "ALT".
fn default_label(id: &str) -> String {
    // Just use the first token uppercased for a default — user can edit fields.json
    let first = id.split('_').next().unwrap_or(id);
    first.to_uppercase()
}

// ── Scrape ────────────────────────────────────────────────────────────────────

fn scrape_endpoint(base: &str, endpoint: &str) -> Option<JsonValue> {
    let url = format!("{}{}", base.trim_end_matches('/'), endpoint);
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(3))
        .build()
        .ok()?;
    let resp = client.get(&url).send().ok()?;
    let text = resp.text().ok()?;
    serde_json::from_str(&text).ok()
}

fn collect_fields(endpoint: &str, value: &JsonValue, out: &mut Vec<FieldEntry>) {
    let obj = match value.as_object() {
        Some(o) => o,
        None => return,
    };
    for (key, val) in obj {
        let field_type = match val {
            JsonValue::Number(_) => "f64",
            JsonValue::Bool(_) => "bool",
            JsonValue::String(_) => "string",
            _ => continue, // skip arrays/objects/null
        };
        let id = key_to_id(endpoint, key);
        let unit = infer_unit(key);
        let label = default_label(&id);
        out.push(FieldEntry {
            id,
            endpoint: endpoint.to_string(),
            api_key: key.clone(),
            label,
            unit,
            field_type: field_type.to_string(),
        });
    }
}

// ── Merge with existing ───────────────────────────────────────────────────────

/// Load existing fields.json if present, keyed by id.
fn load_existing(path: &PathBuf) -> HashMap<String, FieldEntry> {
    if !path.exists() { return HashMap::new(); }
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let entries: Vec<FieldEntry> = serde_json::from_str(&text).unwrap_or_default();
    entries.into_iter().map(|e| (e.id.clone(), e)).collect()
}

fn main() {
    let args = Args::parse();

    if let Some(p) = args.fm_dir {
        core_client::set_fm_root(p);
    }
    if let Some(p) = args.config_dir {
        core_client::set_config_dir(p);
    }

    let mut existing = load_existing(&args.out);
    let mut scraped: Vec<FieldEntry> = Vec::new();

    for endpoint in ENDPOINTS {
        print!("Scraping {} ... ", endpoint);
        match scrape_endpoint(&args.base, endpoint) {
            Some(v) => {
                collect_fields(endpoint, &v, &mut scraped);
                println!("OK ({} keys)", v.as_object().map(|o| o.len()).unwrap_or(0));
            }
            None => println!("OFFLINE (skipped)"),
        }
    }

    // Merge: new fields get defaults; existing fields keep hand-edited label/unit.
    for entry in &scraped {
        existing.entry(entry.id.clone()).or_insert_with(|| entry.clone());
        // Update endpoint/api_key/type in case they changed, preserve label/unit.
        if let Some(e) = existing.get_mut(&entry.id) {
            e.endpoint = entry.endpoint.clone();
            e.api_key = entry.api_key.clone();
            e.field_type = entry.field_type.clone();
        }
    }

    let mut entries: Vec<FieldEntry> = existing.into_values().collect();
    // Sort by endpoint then api_key for stable output.
    entries.sort_by(|a, b| a.endpoint.cmp(&b.endpoint).then(a.api_key.cmp(&b.api_key)));

    if let Some(parent) = args.out.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).ok();
        }
    }
    let json = serde_json::to_string_pretty(&entries).expect("serialize");
    std::fs::write(&args.out, json).expect("write fields.json");
    println!("Wrote {} fields to {}", entries.len(), args.out.display());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_key_to_id_simple() {
        assert_eq!(key_to_id("/state", "H, m"), "h_m");
        assert_eq!(key_to_id("/state", "TAS, km/h"), "tas_kmh");
        assert_eq!(key_to_id("/state", "throttle 1, %"), "throttle_1_pct");
        assert_eq!(key_to_id("/state", "Mfuel, kg"), "mfuel_kg");
        assert_eq!(key_to_id("/state", "RPM 1"), "rpm_1");
    }

    #[test]
    fn test_infer_unit() {
        assert_eq!(infer_unit("H, m"), "m");
        assert_eq!(infer_unit("TAS, km/h"), "km/h");
        assert_eq!(infer_unit("throttle 1, %"), "%");
        assert_eq!(infer_unit("Mfuel, kg"), "kg");
    }

    #[test]
    fn test_collect_fields() {
        let v: JsonValue = serde_json::from_str(r#"{"H, m": 100.0, "valid": true, "name": "test"}"#).unwrap();
        let mut fields = Vec::new();
        collect_fields("/state", &v, &mut fields);
        // Should collect f64, bool, and string
        assert_eq!(fields.len(), 3);
        let ids: Vec<_> = fields.iter().map(|f| f.id.as_str()).collect();
        assert!(ids.contains(&"h_m"));
        assert!(ids.contains(&"valid"));
        assert!(ids.contains(&"name"));
    }
}
