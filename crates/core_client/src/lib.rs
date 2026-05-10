//! Core client for War Thunder local API — data-driven edition.
//!
//! ## Architecture
//!
//! ```text
//! Background threads (one per endpoint)
//!   → each loops: GET /state | GET /indicators  (500 ms timeout, keep-alive)
//!   → writes latest JsonValue into Arc<RwLock<EndpointCache>>
//!
//! fetch_display_windows()  ← called by overlay poller at 60 Hz, no HTTP
//!   → reads EndpointCache snapshot (instant, no blocking)
//!   → builds RawFrame from fields
//!   → Calculator::evaluate per window
//!   → Vec<WindowRows>
//! ```
//!
//! The background threads run at whatever rate the game server allows (~6 Hz
//! observed).  The overlay renders at 60 fps from the latest cached values,
//! so the HUD is visually smooth even though telemetry data refreshes at ~6 Hz.
//!
//! ## Test path
//! `Client::with_config` / `Client::with_windows` (used by unit tests) do NOT
//! spawn background threads.  Instead `fetch_display_windows` calls an inline
//! synchronous fetch so tests remain deterministic.
//!
//! ## indicators.json format
//!
//! ```json
//! [
//!   {
//!     "id": "flight",
//!     "x": 100, "y": 100,
//!     "indicators": [ { "id": "altitude", ... }, ... ]
//!   }
//! ]
//! ```
//!
//! ## Colour tokens
//! `DisplayRow::color` uses a small set of named tokens (`"value"`, `"warn"`,
//! `"good"`, `"info"`, `"unit"`) so the renderer can map them to actual
//! platform colours without knowing about threshold logic.

use serde::{Deserialize, Serialize};
use serde::de::{self, Deserializer, Visitor};
use serde::ser::Serializer;
use serde_json::Value as JsonValue;
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

pub mod fm_update;
pub use fm_update::{check_and_update_fm, fm_base_dir, read_fm_version_tag};

// ── Field catalog ─────────────────────────────────────────────────────────────

/// One entry in `fields.json` — describes a single raw API field.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct FieldDef {
    pub id: String,
    pub endpoint: String,
    pub api_key: String,
    pub label: String,
    pub unit: String,
    #[serde(rename = "type")]
    pub field_type: String,
}

// ── Indicator definitions ─────────────────────────────────────────────────────

// ── Color type ────────────────────────────────────────────────────────────────

/// Overlay RGBA color (8-bit per channel).
///
/// Accepted JSON formats:
/// - `"#RRGGBB"` / `"#RRGGBBAA"` hex strings
/// - `[r, g, b]` / `[r, g, b, a]` byte arrays
/// - `"rgb(r,g,b)"` / `"rgba(r,g,b,a)"` strings (a is 0–255)
///
/// Serialises as `"#RRGGBB"` (or `"#RRGGBBAA"` when alpha ≠ 255).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct OverlayColor(pub [u8; 4]); // [R, G, B, A]

impl OverlayColor {
    pub fn rgb(r: u8, g: u8, b: u8) -> Self { Self([r, g, b, 255]) }
    pub fn rgba(r: u8, g: u8, b: u8, a: u8) -> Self { Self([r, g, b, a]) }
    pub fn to_rgba(self) -> [u8; 4] { self.0 }
}

impl Serialize for OverlayColor {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let [r, g, b, a] = self.0;
        if a == 255 {
            s.serialize_str(&format!("#{r:02X}{g:02X}{b:02X}"))
        } else {
            s.serialize_str(&format!("#{r:02X}{g:02X}{b:02X}{a:02X}"))
        }
    }
}

struct OverlayColorVisitor;

impl<'de> Visitor<'de> for OverlayColorVisitor {
    type Value = OverlayColor;

    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "a color as \"#RRGGBB\", \"#RRGGBBAA\", [r,g,b], [r,g,b,a], \"rgb(r,g,b)\", or \"rgba(r,g,b,a)\"")
    }

    fn visit_str<E: de::Error>(self, v: &str) -> Result<OverlayColor, E> {
        let s = v.trim();
        if let Some(hex) = s.strip_prefix('#') {
            match hex.len() {
                6 => {
                    let r = u8::from_str_radix(&hex[0..2], 16).map_err(de::Error::custom)?;
                    let g = u8::from_str_radix(&hex[2..4], 16).map_err(de::Error::custom)?;
                    let b = u8::from_str_radix(&hex[4..6], 16).map_err(de::Error::custom)?;
                    Ok(OverlayColor([r, g, b, 255]))
                }
                8 => {
                    let r = u8::from_str_radix(&hex[0..2], 16).map_err(de::Error::custom)?;
                    let g = u8::from_str_radix(&hex[2..4], 16).map_err(de::Error::custom)?;
                    let b = u8::from_str_radix(&hex[4..6], 16).map_err(de::Error::custom)?;
                    let a = u8::from_str_radix(&hex[6..8], 16).map_err(de::Error::custom)?;
                    Ok(OverlayColor([r, g, b, a]))
                }
                n => Err(de::Error::custom(format!("hex color must be 6 or 8 hex chars, got {n}")))
            }
        } else if let Some(inner) = s.strip_prefix("rgba(").and_then(|t| t.strip_suffix(')')) {
            let parts: Vec<&str> = inner.split(',').collect();
            if parts.len() != 4 {
                return Err(de::Error::custom(format!("rgba() needs 4 components, got {}", parts.len())));
            }
            let r = parts[0].trim().parse::<u8>().map_err(de::Error::custom)?;
            let g = parts[1].trim().parse::<u8>().map_err(de::Error::custom)?;
            let b = parts[2].trim().parse::<u8>().map_err(de::Error::custom)?;
            let a = parts[3].trim().parse::<u8>().map_err(de::Error::custom)?;
            Ok(OverlayColor([r, g, b, a]))
        } else if let Some(inner) = s.strip_prefix("rgb(").and_then(|t| t.strip_suffix(')')) {
            let parts: Vec<&str> = inner.split(',').collect();
            if parts.len() != 3 {
                return Err(de::Error::custom(format!("rgb() needs 3 components, got {}", parts.len())));
            }
            let r = parts[0].trim().parse::<u8>().map_err(de::Error::custom)?;
            let g = parts[1].trim().parse::<u8>().map_err(de::Error::custom)?;
            let b = parts[2].trim().parse::<u8>().map_err(de::Error::custom)?;
            Ok(OverlayColor([r, g, b, 255]))
        } else {
            Err(de::Error::custom(format!("unknown color format: {s:?}")))
        }
    }

    fn visit_seq<A: de::SeqAccess<'de>>(self, mut seq: A) -> Result<OverlayColor, A::Error> {
        let r = seq.next_element::<u8>()?.ok_or_else(|| de::Error::custom("missing r component"))?;
        let g = seq.next_element::<u8>()?.ok_or_else(|| de::Error::custom("missing g component"))?;
        let b = seq.next_element::<u8>()?.ok_or_else(|| de::Error::custom("missing b component"))?;
        let a = seq.next_element::<u8>()?.unwrap_or(255);
        Ok(OverlayColor([r, g, b, a]))
    }
}

impl<'de> Deserialize<'de> for OverlayColor {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        d.deserialize_any(OverlayColorVisitor)
    }
}

// ── Render style ──────────────────────────────────────────────────────────────

/// Per-window or per-indicator render style overrides.
///
/// All fields are `Option`; absent fields fall back to global defaults.
/// Window-level style applies to all rows in the window.
/// Indicator-level style applies to that row only — layout fields
/// (`pad_x`, `pad_y`, `col_gap`, `font_size`, `line_height`) are silently
/// ignored at the indicator level; only colour overrides take effect.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct RenderStyle {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub font_size: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line_height: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pad_x: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pad_y: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub col_gap: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub c_label: Option<OverlayColor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub c_unit: Option<OverlayColor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub c_warn: Option<OverlayColor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub c_crit: Option<OverlayColor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub c_good: Option<OverlayColor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub c_info: Option<OverlayColor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub c_shadow: Option<OverlayColor>,
}

// ── Indicator definitions ─────────────────────────────────────────────────────

/// A threshold value for warn/good coloring.  Can be a fixed number or a
/// formula string referencing RawFrame variables (including FM fields like
/// `fm_crit_ias`, `fm_crit_gear_spd`, etc.).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Threshold {
    Fixed(f64),
    Formula(String),
}

impl Threshold {
    fn eval(&self, frame: &RawFrame) -> Option<f64> {
        match self {
            Threshold::Fixed(v) => Some(*v),
            Threshold::Formula(expr) => {
                use evalexpr::*;
                let mut ctx = HashMapContext::new();
                for (k, v) in frame {
                    ctx.set_value(k.clone(), Value::Float(*v)).ok();
                }
                eval_float_with_context(expr, &ctx)
                    .ok()
                    .or_else(|| eval_int_with_context(expr, &ctx).ok().map(|i| i as f64))
                    .or_else(|| eval_boolean_with_context(expr, &ctx).ok().map(|b| if b { 1.0 } else { 0.0 }))
            }
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct IndicatorDef {
    pub id: String,
    pub label: String,
    #[serde(default)]
    pub unit: String,
    pub formula: String,
    #[serde(default = "default_format")]
    pub format: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warn_below: Option<Threshold>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warn_above: Option<Threshold>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub good_above: Option<Threshold>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub good_below: Option<Threshold>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub crit_above: Option<Threshold>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub crit_below: Option<Threshold>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub show_when: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub style: Option<RenderStyle>,
}

fn default_format() -> String { "integer".to_string() }

// ── Window definition ─────────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct WindowDef {
    pub id: String,
    #[serde(default)]
    pub x: i32,
    #[serde(default)]
    pub y: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    pub indicators: Vec<IndicatorDef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub style: Option<RenderStyle>,
}

impl WindowDef {
    pub fn computed_height(&self) -> u32 {
        self.height.unwrap_or_else(|| {
            let pad = 20u32;
            let row_h = 26u32;
            pad + (self.indicators.len() as u32).max(1) * row_h + pad / 2
        })
    }
    pub fn computed_width(&self) -> u32 {
        self.width.unwrap_or(240)
    }
}

// ── Raw frame ─────────────────────────────────────────────────────────────────

pub type RawFrame = HashMap<String, f64>;

// ── FM database ───────────────────────────────────────────────────────────────

/// Parsed row from fm_data_db.csv for one vehicle.
#[derive(Debug, Clone, Default)]
pub struct FmRecord {
    pub crit_ias_kmh: Option<f64>,
    pub crit_mach: Option<f64>,
    pub crit_gear_kmh: Option<f64>,
    /// Most restrictive (min) flaps speed limit (km/h) — full extension / landing.
    pub crit_flaps_min_kmh: Option<f64>,
    /// Least restrictive (max) flaps speed limit (km/h) — shallowest extension breakpoint.
    pub crit_flaps_combat_kmh: Option<f64>,
    pub max_fuel_kg: Option<f64>,
    /// Normal max RPM (second of three RPM values).
    pub rpm_normal: Option<f64>,
    /// Emergency max RPM (third of three RPM values).
    pub rpm_max: Option<f64>,
    pub crit_aoa_pos: Option<f64>,
    pub crit_aoa_neg: Option<f64>,
    pub num_engines: Option<f64>,
    /// Empty (structural) mass in kg, from FM data.
    pub empty_mass_kg: Option<f64>,
    /// Maximum positive structural wing load (N).  Wing breaks when
    /// `Ny × total_weight_N > crit_wing_overload_pos`.
    pub crit_wing_overload_pos: Option<f64>,
    /// Maximum negative structural wing load (N, stored as negative value).
    pub crit_wing_overload_neg: Option<f64>,
    // Named flap-position speed limits derived from CritFlapsSpd (positional assignment).
    /// flaps_pct/100 threshold at or below which combat flaps applies.
    pub combat_flaps_ratio: Option<f64>,
    pub combatflaps_crit_speed: Option<f64>,
    pub takeoff_flaps_ratio: Option<f64>,
    pub takeoffflaps_crit_speed: Option<f64>,
    pub landing_flaps_ratio: Option<f64>,
    pub landingflaps_crit_speed: Option<f64>,
}

/// Map from vehicle name (fm_names_db.csv `Name` column) → FmRecord.
pub type FmDb = HashMap<String, FmRecord>;

/// Load FM database from the two CSV files found relative to the exe.
/// Returns an empty map on any error (non-fatal).
pub fn load_fm_db(data_dir: Option<&Path>) -> FmDb {
    let mut db = FmDb::new();

    // Resolve directory: caller-provided, or <exe_dir>/fm/fm/, or data/fm/fm/
    let fm_dir: PathBuf = if let Some(d) = data_dir {
        d.to_path_buf()
    } else if let Ok(exe) = std::env::current_exe() {
        let candidate = exe.parent().unwrap_or(Path::new(".")).join("fm").join("fm");
        if candidate.exists() { candidate } else { PathBuf::from("data").join("fm").join("fm") }
    } else {
        PathBuf::from("data").join("fm").join("fm")
    };

    let _names_path = fm_dir.join("fm_names_db.csv");
    let data_path  = fm_dir.join("fm_data_db.csv");

    // --- fm_names_db.csv: Name;FmName;Type;English ---
    // We use this only to build the set of known vehicle names; fm_data_db
    // keys are already vehicle names we can look up by the `/indicators` `type` field.
    // We just parse fm_data_db directly using the Name column.

    let data_text = match std::fs::read_to_string(&data_path) {
        Ok(t) => t,
        Err(e) => { eprintln!("[fm_db] read {}: {e}", data_path.display()); return db; }
    };

    // Header: Name;Length;WingSpan;WingArea;EmptyMass;MaxFuelMass;CritAirSpd;CritAirSpdMach;
    //         CritGearSpd;CombatFlaps;TakeoffFlaps;CritFlapsSpd;CritWingOverload;
    //         NumEngines;RPM;MaxNitro;NitroConsum;CritAoA
    // Indices (0-based): Name=0 MaxFuelMass=5 CritAirSpd=6 CritAirSpdMach=7 CritGearSpd=8
    //                    CritFlapsSpd=11 NumEngines=13 RPM=14 CritAoA=17

    for (line_no, line) in data_text.lines().enumerate() {
        if line_no == 0 { continue; } // skip header
        let cols: Vec<&str> = line.split(';').collect();
        if cols.len() < 18 { continue; }

        let name = cols[0].trim().to_string();
        if name.is_empty() { continue; }

        let parse = |s: &str| -> Option<f64> { s.trim().parse::<f64>().ok() };

        // Col 9: CombatFlaps degree — 0 means no combat flaps position.
        // Col 10: TakeoffFlaps degree — 0 means no takeoff flaps position.
        let has_combat_flaps  = parse(cols[9]).map(|v| v != 0.0).unwrap_or(false);
        let has_takeoff_flaps = parse(cols[10]).map(|v| v != 0.0).unwrap_or(false);

        // CritFlapsSpd: "ratio,speed,ratio,speed,..." — parse pairs.
        // Filter ratio=0 entries (structural limit for retracted flaps), sort ascending.
        // Then assign positionally: combat (if has_combat_flaps && ≥2 remaining),
        // takeoff (if has_takeoff_flaps && ≥2 remaining after combat), landing = last.
        let (flaps_min, flaps_combat,
             combat_flaps_ratio, combatflaps_crit_speed,
             takeoff_flaps_ratio, takeoffflaps_crit_speed,
             landing_flaps_ratio, landingflaps_crit_speed) = {
            let s = cols[11].trim();
            let nums: Vec<f64> = s.split(',').filter_map(|x| x.trim().parse().ok()).collect();
            let mut pairs: Vec<(f64, f64)> = nums.chunks(2)
                .filter(|c| c.len() == 2)
                .map(|c| (c[0], c[1]))
                .filter(|(r, _)| *r > 0.0)   // skip ratio=0 structural entries
                .collect();
            pairs.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));

            let speeds: Vec<f64> = pairs.iter().map(|p| p.1).collect();
            let (flaps_min, flaps_combat) = if speeds.is_empty() {
                (None, None)
            } else {
                let min_spd = speeds.iter().cloned().fold(f64::INFINITY, f64::min);
                let max_spd = speeds.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
                (Some(min_spd), Some(max_spd))
            };

            // Positional assignment
            let mut remaining = pairs.as_slice();
            let mut combat_r   = None::<f64>;
            let mut combat_spd = None::<f64>;
            let mut takeoff_r   = None::<f64>;
            let mut takeoff_spd = None::<f64>;

            if has_combat_flaps && remaining.len() >= 2 {
                combat_r   = Some(remaining[0].0);
                combat_spd = Some(remaining[0].1);
                remaining = &remaining[1..];
            }
            if has_takeoff_flaps && remaining.len() >= 2 {
                takeoff_r   = Some(remaining[0].0);
                takeoff_spd = Some(remaining[0].1);
                remaining = &remaining[1..];
            }
            // Last entry is always landing
            let (landing_r, landing_spd) = remaining.last()
                .map(|&(r, s)| (Some(r), Some(s)))
                .unwrap_or((None, None));

            (flaps_min, flaps_combat,
             combat_r, combat_spd,
             takeoff_r, takeoff_spd,
             landing_r, landing_spd)
        };

        // RPM: "idle,normal_max,emergency_max"
        let (rpm_normal, rpm_max) = {
            let s = cols[14].trim();
            let vals: Vec<f64> = s.split(',').filter_map(|x| x.trim().parse().ok()).collect();
            (vals.get(1).copied(), vals.get(2).copied())
        };

        // CritAoA: "pos1,neg1,pos2,neg2" — take first pair
        let (aoa_pos, aoa_neg) = {
            let s = cols[17].trim();
            let vals: Vec<f64> = s.split(',').filter_map(|x| x.trim().parse().ok()).collect();
            (vals.first().copied(), vals.get(1).copied())
        };

        // CritWingOverload: "neg_N,pos_N" — structural wing load limits in Newtons.
        let (crit_wing_overload_neg, crit_wing_overload_pos) = {
            let s = cols[12].trim();
            let vals: Vec<f64> = s.split(',').filter_map(|x| x.trim().parse().ok()).collect();
            (vals.first().copied(), vals.get(1).copied())
        };

        db.insert(name, FmRecord {
            empty_mass_kg:       parse(cols[4]),
            crit_ias_kmh:        parse(cols[6]),
            crit_mach:           parse(cols[7]),
            crit_gear_kmh:       parse(cols[8]),
            crit_flaps_min_kmh:  flaps_min,
            crit_flaps_combat_kmh: flaps_combat,
            max_fuel_kg:         parse(cols[5]),
            rpm_normal,
            rpm_max,
            crit_aoa_pos:        aoa_pos,
            crit_aoa_neg:        aoa_neg,
            num_engines:         parse(cols[13]),
            crit_wing_overload_pos,
            crit_wing_overload_neg,
            combat_flaps_ratio,
            combatflaps_crit_speed,
            takeoff_flaps_ratio,
            takeoffflaps_crit_speed,
            landing_flaps_ratio,
            landingflaps_crit_speed,
        });
    }

    eprintln!("[fm_db] loaded {} records from {}", db.len(), data_path.display());
    db
}

/// Given current flap extension (0–100 %) and the vehicle's named flap-position
/// speed limits, return the applicable critical speed limit (km/h).
///
/// Logic:
/// - If ratio ≤ combat_flaps_ratio → use combat speed
/// - Else if ratio ≤ takeoff_flaps_ratio → use takeoff speed
/// - Else → use landing speed
/// Returns `None` when flaps_pct ≤ 0 or no landing speed is available.
pub fn compute_named_flaps_current(flaps_pct: f64, rec: &FmRecord) -> Option<f64> {
    if flaps_pct <= 0.0 { return None; }
    let ratio = flaps_pct / 100.0;
    if let (Some(cr), Some(cs)) = (rec.combat_flaps_ratio, rec.combatflaps_crit_speed) {
        if ratio <= cr { return Some(cs); }
    }
    if let (Some(tr), Some(ts)) = (rec.takeoff_flaps_ratio, rec.takeoffflaps_crit_speed) {
        if ratio <= tr { return Some(ts); }
    }
    rec.landingflaps_crit_speed
}

/// Inject time-derivative virtual fields into the frame.
///
/// Computes:
/// - `sep` — Specific Excess Power in m/s: `Vz + (V/g)·(dV/dt)`
///   dV/dt is estimated via OLS linear regression over a sliding ring buffer
///   of up to 8 TAS samples (≤ 2 s window) to suppress quantisation noise
///   from the integer km/h TAS field.
/// - `sep_thrust` — thrust-upper-bound SEP (m/s, ignores drag):
///   `(Σ thrust_N_kgs) · TAS_ms / (fm_empty_mass_kg + mfuel_kg)`.
///   Only emitted when both FM empty mass and current fuel are in the frame.
///
/// Falls back to `vy_ms` alone when the sample window is too short (<50 ms).
/// SEP is clamped to ±300 m/s to suppress transient spikes.
///
/// The `now` parameter is injectable for deterministic tests; production
/// callers pass `Instant::now()`.
fn inject_derived_fields(frame: &mut RawFrame, state: &mut DerivedState, now: Instant) {
    let (Some(&vy_ms), Some(&tas_kmh)) = (frame.get("vy_ms"), frame.get("tas_kmh")) else { return; };
    let tas_ms = tas_kmh / 3.6;

    // ── Update ring buffer ────────────────────────────────────────────────
    // Only push when TAS actually changes.  The overlay polls at ~60 Hz but
    // the WT server only delivers new data at ~6 Hz; without this guard the
    // buffer fills with identical values and the OLS slope is always zero.
    let should_push = state.last_tas_pushed.map(|prev| prev != tas_ms).unwrap_or(true);
    if should_push {
        state.tas_history.push_back((now, tas_ms));
        state.last_tas_pushed = Some(tas_ms);
        // Keep at most 8 entries.
        while state.tas_history.len() > 8 { state.tas_history.pop_front(); }
        // Prune entries older than 2 s.
        while state.tas_history.len() > 1 {
            let age = now.duration_since(state.tas_history[0].0).as_secs_f64();
            if age > 2.0 { state.tas_history.pop_front(); } else { break; }
        }
    }

    // ── OLS dV/dt ─────────────────────────────────────────────────────────
    // We need ≥2 samples and a window ≥50 ms to get a meaningful slope.
    let accel: Option<f64> = if state.tas_history.len() >= 2 {
        let t0_inst = state.tas_history[0].0;
        let span = now.duration_since(t0_inst).as_secs_f64();
        if span >= 0.05 {
            let n = state.tas_history.len() as f64;
            let (mut sum_t, mut sum_v, mut sum_tt, mut sum_tv) = (0.0_f64, 0.0_f64, 0.0_f64, 0.0_f64);
            for &(inst, v) in &state.tas_history {
                let t = inst.duration_since(t0_inst).as_secs_f64();
                sum_t  += t;
                sum_v  += v;
                sum_tt += t * t;
                sum_tv += t * v;
            }
            let denom = n * sum_tt - sum_t * sum_t;
            if denom.abs() > 1e-12 { Some((n * sum_tv - sum_t * sum_v) / denom) } else { None }
        } else { None }
    } else { None };

    // ── SEP (kinematic energy-rate form) ──────────────────────────────────
    let sep = if let Some(a) = accel {
        (vy_ms + (tas_ms / 9.81) * a).clamp(-300.0, 300.0)
    } else {
        vy_ms
    };
    frame.insert("sep".into(), sep);

    // ── Thrust-based SEP upper bound (drag not available from API) ────────
    // sep_thrust = Σ(T_kgf) · V_ms / W_kg
    //   kgf cancels: (kgf · m/s) / kg  ≡  (N · m/s) / (kg · g) = m/s (power/weight).
    let thrust_kgf: f64 = [
        frame.get("thrust_1_kgs"),
        frame.get("thrust_2_kgs"),
        frame.get("thrust_3_kgs"),
        frame.get("thrust_4_kgs"),
    ]
    .iter()
    .filter_map(|v| v.copied())
    .sum::<f64>();

    if let (Some(&em_kg), Some(&fuel_kg)) = (frame.get("fm_empty_mass_kg"), frame.get("mfuel_kg")) {
        let weight_kg = em_kg + fuel_kg;
        if weight_kg > 100.0 && thrust_kgf > 0.0 && tas_ms > 1.0 {
            let sep_thrust = (thrust_kgf * tas_ms / weight_kg).clamp(-300.0, 300.0);
            frame.insert("sep_thrust".into(), sep_thrust);
        }

        // ── Structural G limit (matches WTRTI "crit_g_pos") ─────────────────
        // Formula: crit_g_pos = 2 × CritWingOverload_pos_N / (mass_total_kg × g)
        // The FM database stores per-wing overload; doubling gives the airframe limit.
        // Verified against WTRTI State window: matches to 5 decimal places.
        if let Some(&crit_wo_pos) = frame.get("fm_crit_wing_overload_pos") {
            if weight_kg > 100.0 && crit_wo_pos > 0.0 {
                let crit_g_pos = 2.0 * crit_wo_pos / (weight_kg * 9.81);
                frame.insert("crit_g_pos".into(), crit_g_pos);
            }
        }
    }

    // ── Unified fuel flow (kg/h) ──────────────────────────────────────────────
    // `fuel_flow` is the single field to use in indicators.json regardless of
    // aircraft type.  Prefer the native `fuel_consume` reported by the API;
    // fall back to the differentiated EMA for planes that don't expose it.
    // Calculated instantaneous rate: differentiate `mfuel_kg` over time.
    // We only update when the fuel value actually changes (same ~6 Hz guard
    // used for TAS above) and only when the aircraft is burning fuel (Δfuel > 0).
    if let Some(&fuel_kg) = frame.get("mfuel_kg") {
        let changed = state.last_fuel_kg.map(|prev| prev != fuel_kg).unwrap_or(true);
        if changed {
            if let (Some(prev_kg), Some(prev_t)) = (state.last_fuel_kg, state.last_fuel_time) {
                let dt_s = now.duration_since(prev_t).as_secs_f64();
                let delta_kg = prev_kg - fuel_kg; // positive when consuming
                if dt_s >= 0.05 && delta_kg > 0.0 {
                    let rate_kgh = (delta_kg / dt_s) * 3600.0;
                    if rate_kgh < 50_000.0 {
                        const ALPHA: f64 = 0.25;
                        let smoothed = match state.fuel_consume_ema {
                            Some(prev_ema) => prev_ema + ALPHA * (rate_kgh - prev_ema),
                            None => rate_kgh,
                        };
                        state.fuel_consume_ema = Some(smoothed);
                    }
                } else if delta_kg <= 0.0 {
                    // Fuel went up (refuel / new mission) — reset the EMA.
                    state.fuel_consume_ema = None;
                }
            }
            state.last_fuel_kg = Some(fuel_kg);
            state.last_fuel_time = Some(now);
        }
    }

    // Emit `fuel_consume_calc` from the EMA when the native field is absent.
    if !frame.contains_key("fuel_consume") {
        if let Some(rate) = state.fuel_consume_ema {
            frame.insert("fuel_consume_calc".into(), rate);
        }
    }

    // Always emit `fuel_flow`: native API value takes priority, EMA as fallback.
    let flow = frame.get("fuel_consume").copied()
        .or_else(|| state.fuel_consume_ema);
    if let Some(rate) = flow {
        frame.insert("fuel_flow".into(), rate);
    }
}

fn inject_fm_fields(frame: &mut RawFrame, rec: &FmRecord) {
    macro_rules! put {
        ($key:expr, $opt:expr) => { if let Some(v) = $opt { frame.insert($key.into(), v); } }
    }
    put!("fm_crit_ias",            rec.crit_ias_kmh);
    put!("fm_crit_mach",           rec.crit_mach);
    put!("fm_crit_gear_spd",       rec.crit_gear_kmh);
    put!("fm_crit_flaps_spd",      rec.crit_flaps_min_kmh);
    put!("fm_crit_flaps_combat_spd", rec.crit_flaps_combat_kmh);
    put!("fm_max_fuel_kg",         rec.max_fuel_kg);
    put!("fm_rpm_normal",          rec.rpm_normal);
    put!("fm_rpm_max",             rec.rpm_max);
    put!("fm_crit_aoa_pos",        rec.crit_aoa_pos);
    put!("fm_crit_aoa_neg",        rec.crit_aoa_neg);
    put!("fm_num_engines",         rec.num_engines);
    put!("fm_empty_mass_kg",       rec.empty_mass_kg);
    put!("fm_crit_wing_overload_pos", rec.crit_wing_overload_pos);
    put!("fm_crit_wing_overload_neg", rec.crit_wing_overload_neg);
    put!("fm_combatflaps_crit_speed",  rec.combatflaps_crit_speed);
    put!("fm_takeoffflaps_crit_speed", rec.takeoffflaps_crit_speed);
    put!("fm_landingflaps_crit_speed", rec.landingflaps_crit_speed);
}



#[derive(Debug, Clone, PartialEq)]
pub struct DisplayRow {
    pub label: String,
    pub value_str: String,
    pub unit: String,
    pub color: String,
    /// Per-indicator render style (colours only; layout fields ignored at row level).
    pub style: Option<RenderStyle>,
}

// ── Window rows ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct WindowRows {
    pub id: String,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub rows: Vec<DisplayRow>,
    /// Window-level render style (applies to all rows unless overridden per-indicator).
    pub style: Option<RenderStyle>,
}

// ── Error ─────────────────────────────────────────────────────────────────────

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("parse error: {0}")]
    Parse(#[from] serde_json::Error),
    #[error("circuit open")]
    CircuitOpen,
    #[error("other error: {0}")]
    Other(String),
}

// ── HTTP abstraction (used only by sync/test path) ────────────────────────────

pub trait HttpClient: Send + Sync {
    fn get(&self, url: &str) -> Result<String, Error>;
}

// ── Persistent raw-TCP HTTP/1.1 connection ────────────────────────────────────
//
// One instance per background fetch thread.  The socket stays open between
// requests so every poll is just a write + read on an already-connected
// socket — no TCP handshake overhead.  This mirrors what WTRTI does with
// libhv's AsyncHttpClient + keep-alive.
//
// Protocol: HTTP/1.1 with Connection: keep-alive.  The WT server returns
// Content-Length so we read exactly that many body bytes.  If the server
// sends Connection: close we drain the response, close the socket, and
// reconnect before the next request (one reconnect per cycle rather than
// one per request).  Any I/O error triggers an immediate reconnect + retry.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;

struct PersistentConn {
    host: String,   // e.g. "127.0.0.1:8111"
    path: String,   // e.g. "/state"
    stream: Option<BufReader<TcpStream>>,
}

impl PersistentConn {
    fn new(host: &str, path: &str) -> Self {
        Self {
            host: host.to_string(),
            path: path.to_string(),
            stream: None,
        }
    }

    fn connect(&mut self) -> std::io::Result<()> {
        // TcpStream::connect accepts "host:port" and handles DNS resolution,
        // unlike connect_timeout which requires a SocketAddr (IP only).
        let t0 = std::time::Instant::now();
        let tcp = TcpStream::connect(&self.host)?;
        let _ms = t0.elapsed().as_millis();
        // eprintln!("[http] connect {} in {}ms", self.host, ms);
        tcp.set_read_timeout(Some(Duration::from_secs(5)))?;
        tcp.set_write_timeout(Some(Duration::from_secs(2)))?;
        tcp.set_nodelay(true)?;
        self.stream = Some(BufReader::new(tcp));
        Ok(())
    }

    /// Send one HTTP GET and return the response body as a String.
    /// Reconnects transparently on failure (at most once per call).
    fn get(&mut self) -> Result<String, Error> {
        self.do_get().or_else(|_| {
            // On any error reconnect and retry once.
            self.stream = None;
            self.do_get()
        })
    }

    fn do_get(&mut self) -> Result<String, Error> {
        if self.stream.is_none() {
            self.connect()?;
        }
        let stream = self.stream.as_mut().unwrap();

        // Write request.
        let req = format!(
            "GET {} HTTP/1.1\r\nHost: {}\r\nConnection: keep-alive\r\n\r\n",
            self.path, self.host
        );
        stream.get_mut().write_all(req.as_bytes())?;

        // Read status line.
        let mut status_line = String::new();
        stream.read_line(&mut status_line)?;
        if status_line.is_empty() {
            return Err(Error::Io(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "server closed connection")));
        }

        // Read headers.
        let mut content_length: Option<usize> = None;
        let mut chunked = false;
        let mut server_close = false;
        loop {
            let mut line = String::new();
            stream.read_line(&mut line)?;
            let trimmed = line.trim_end_matches(|c| c == '\r' || c == '\n');
            if trimmed.is_empty() { break; }
            let lower = trimmed.to_lowercase();
            if lower.starts_with("content-length:") {
                if let Ok(n) = trimmed[15..].trim().parse::<usize>() {
                    content_length = Some(n);
                }
            } else if lower.starts_with("transfer-encoding:") && lower.contains("chunked") {
                chunked = true;
            } else if lower.starts_with("connection:") && lower.contains("close") {
                server_close = true;
            }
        }

        // Log the encoding strategy once per path so we know what the server sends.
        {
            use std::sync::Mutex;
            static LOGGED: std::sync::OnceLock<Mutex<HashMap<String, bool>>> = std::sync::OnceLock::new();
            let map = LOGGED.get_or_init(|| Mutex::new(HashMap::new()));
            let mut guard = map.lock().unwrap();
            if !guard.contains_key(&self.path) {
                let mode = if content_length.is_some() { format!("Content-Length={}", content_length.unwrap()) }
                           else if chunked { "chunked".to_string() }
                           else { "unknown(will read-to-close)".to_string() };
                eprintln!("[http] {} body encoding: {}", self.path, mode);
                guard.insert(self.path.clone(), true);
            }
        }

        // Read body.
        let body = if let Some(len) = content_length {
            let mut buf = vec![0u8; len];
            stream.read_exact(&mut buf)?;
            String::from_utf8(buf).map_err(|e| Error::Other(e.to_string()))?
        } else if chunked {
            // Decode HTTP/1.1 chunked transfer encoding.
            let mut body = String::new();
            loop {
                let mut size_line = String::new();
                stream.read_line(&mut size_line)?;
                let chunk_size = usize::from_str_radix(size_line.trim(), 16)
                    .map_err(|e| Error::Other(format!("bad chunk size {:?}: {e}", size_line.trim())))?;
                if chunk_size == 0 {
                    // Consume trailing CRLF after last chunk.
                    let mut trailer = String::new();
                    stream.read_line(&mut trailer)?;
                    break;
                }
                let mut chunk = vec![0u8; chunk_size];
                stream.read_exact(&mut chunk)?;
                body.push_str(&String::from_utf8(chunk).map_err(|e| Error::Other(e.to_string()))?);
                // Consume CRLF after chunk data.
                let mut crlf = String::new();
                stream.read_line(&mut crlf)?;
            }
            body
        } else {
            // No Content-Length and not chunked — server will close to signal end.
            // Drop the connection after reading so we don't block forever.
            server_close = true;
            let mut buf = String::new();
            stream.read_to_string(&mut buf)?;
            buf
        };

        if server_close {
            self.stream = None;
        } else {
            // The WT server closes the TCP connection after every response without
            // sending a "Connection: close" header.  Detect this by attempting a
            // zero-byte peek: if the socket is at EOF we drop it now so the next
            // call starts with a fresh connect rather than paying a failed-write
            // round-trip (which can block ~700 ms on Windows before reporting RST).
            if let Some(ref s) = self.stream {
                let tcp: &TcpStream = s.get_ref();
                // Temporarily switch to non-blocking to peek without blocking.
                if tcp.set_nonblocking(true).is_ok() {
                    let mut buf = [0u8; 1];
                    let eof = match tcp.peek(&mut buf) {
                        Ok(0) => true,                          // EOF
                        Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => false, // still open
                        Err(_) => true,                         // any other error = treat as closed
                        Ok(_) => false,                         // data already waiting (shouldn't happen)
                    };
                    let _ = tcp.set_nonblocking(false);
                    if eof {
                        self.stream = None;
                    }
                }
            }
        }

        Ok(body)
    }
}

pub struct ReqwestHttpClient {
    client: reqwest::blocking::Client,
}

impl ReqwestHttpClient {
    pub fn new(timeout: Duration) -> Result<Self, reqwest::Error> {
        Ok(Self {
            client: reqwest::blocking::Client::builder()
                .timeout(timeout)
                .pool_idle_timeout(None)
                .pool_max_idle_per_host(2)
                .tcp_keepalive(Duration::from_secs(1))
                .build()?,
        })
    }
}

impl HttpClient for ReqwestHttpClient {
    fn get(&self, url: &str) -> Result<String, Error> {
        let resp = self.client.get(url)
            .header("Connection", "keep-alive")
            .send()?;
        Ok(resp.text()?)
    }
}

#[cfg(test)]
pub struct FixtureHttpClient {
    fixtures: HashMap<String, String>,
    pub calls: Arc<Mutex<HashMap<String, usize>>>,
}

#[cfg(test)]
impl FixtureHttpClient {
    pub fn new(fixtures: HashMap<String, String>) -> Self {
        Self { fixtures, calls: Arc::new(Mutex::new(HashMap::new())) }
    }
}

#[cfg(test)]
impl HttpClient for FixtureHttpClient {
    fn get(&self, url: &str) -> Result<String, Error> {
        let key = match url.find("//") {
            Some(pos) => match url[pos + 2..].find('/') {
                Some(p2) => &url[pos + 2 + p2..],
                None => "/",
            },
            None => url,
        }.to_string();
        if let Ok(mut calls) = self.calls.lock() {
            *calls.entry(key.clone()).or_insert(0) += 1;
        }
        self.fixtures.get(&key).or_else(|| self.fixtures.get(url))
            .cloned()
            .ok_or_else(|| Error::Other(format!("fixture not found: {}", url)))
    }
}

// ── Endpoint cache ────────────────────────────────────────────────────────────

/// Latest JSON responses from each endpoint, updated by background threads.
/// `None` means we haven't received a successful response yet.
type EndpointCache = Arc<RwLock<HashMap<String, JsonValue>>>;

// ── Config loading ────────────────────────────────────────────────────────────

pub fn find_config(name: &str) -> Option<PathBuf> {
    if let Ok(exe) = std::env::current_exe() {
        let exe_dir = exe.parent().unwrap_or(std::path::Path::new("."));
        let p = exe_dir.join("data").join(name);
        if p.exists() { return Some(p); }
        let p = exe_dir.join(name);
        if p.exists() { return Some(p); }
    }
    let p = PathBuf::from("data").join(name);
    if p.exists() { return Some(p); }
    None
}

pub fn load_fields(path: Option<&Path>) -> Vec<FieldDef> {
    let p = path.map(|p| p.to_path_buf()).or_else(|| find_config("fields.json"));
    let Some(p) = p else { return Vec::new() };
    let text = std::fs::read_to_string(p).unwrap_or_default();
    let stripped = json_comments::StripComments::new(text.as_bytes());
    serde_json::from_reader(stripped).unwrap_or_default()
}

pub fn load_window_defs(path: Option<&Path>) -> Vec<WindowDef> {
    match try_load_window_defs(path) {
        Ok(v) => v,
        Err(e) => { eprintln!("[core_client] {e}"); Vec::new() }
    }
}

/// Like `load_window_defs` but returns a `Result` with a human-readable error
/// message so callers can surface it to the user.
pub fn try_load_window_defs(path: Option<&Path>) -> Result<Vec<WindowDef>, String> {
    // If indicators.json doesn't exist yet, seed it from indicators.json.example
    // so the user gets a working default without overwriting any existing config.
    if path.is_none() && find_config("indicators.json").is_none() {
        if let Some(example) = find_config("indicators.json.example") {
            let dest = example.with_file_name("indicators.json");
            match std::fs::copy(&example, &dest) {
                Ok(_) => eprintln!("[core_client] created indicators.json from indicators.json.example"),
                Err(e) => eprintln!("[core_client] could not seed indicators.json: {e}"),
            }
        }
    }
    let p = path.map(|p| p.to_path_buf()).or_else(|| find_config("indicators.json"))
        .ok_or_else(|| "indicators.json not found".to_string())?;
    let text = std::fs::read_to_string(&p)
        .map_err(|e| format!("could not read {}: {e}", p.display()))?;
    let stripped = json_comments::StripComments::new(text.as_bytes());
    serde_json::from_reader::<_, Vec<WindowDef>>(stripped)
        .map_err(|e| format!("parse error in {}: {e}", p.display()))
}

/// Write `defs` back to `path` as pretty-printed JSON.
///
/// Called by the overlay after a window drag (debounced) and on exit so that
/// user-adjusted positions persist across restarts.  Any JSON comments that
/// were in the original file are lost — the file becomes clean JSON — but all
/// indicator configuration is preserved verbatim.
pub fn save_window_defs(path: &Path, defs: &[WindowDef]) -> Result<(), String> {
    let text = serde_json::to_string_pretty(defs)
        .map_err(|e| format!("could not serialise window defs: {e}"))?;
    std::fs::write(path, text)
        .map_err(|e| format!("could not write {}: {e}", path.display()))
}

// ── App configuration ─────────────────────────────────────────────────────────

/// Persistent application settings stored in `config.json`.
///
/// Every field carries `#[serde(default)]` so that older config files missing
/// new keys are silently filled with sane defaults on load.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    /// Show indicator overlays regardless of whether War Thunder is in the
    /// foreground.  Overrides all other visibility conditions.
    #[serde(default)]
    pub always_show: bool,

    /// Also show indicator overlays when the BYOH settings window has keyboard
    /// focus.  Useful for positioning indicators or debugging formulas.
    #[serde(default)]
    pub show_when_byoh_foreground: bool,

    /// Hide indicator overlays unless `/mission.json` reports `status: "running"`.
    /// Ignored when `always_show` is `true`.
    #[serde(default)]
    pub only_during_mission: bool,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            always_show: false,
            show_when_byoh_foreground: false,
            only_during_mission: false,
        }
    }
}

impl AppConfig {
    /// Load from the default `config.json` location.  Returns `Default` if the
    /// file does not exist or cannot be parsed.
    pub fn load() -> Self {
        let Some(p) = find_config("config.json") else { return Self::default() };
        let text = match std::fs::read_to_string(&p) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("[config] read {}: {e}", p.display());
                return Self::default();
            }
        };
        match serde_json::from_str(&text) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("[config] parse config.json: {e}");
                Self::default()
            }
        }
    }

    /// Save to `config.json` in the same directory as `indicators.json`,
    /// falling back to the executable directory.
    pub fn save(&self) {
        let path = if let Some(p) = find_config("config.json") {
            p
        } else if let Some(p) = find_config("indicators.json") {
            p.with_file_name("config.json")
        } else if let Ok(exe) = std::env::current_exe() {
            exe.with_file_name("config.json")
        } else {
            PathBuf::from("config.json")
        };
        match serde_json::to_string_pretty(self) {
            Ok(text) => {
                if let Err(e) = std::fs::write(&path, text) {
                    eprintln!("[config] write {}: {e}", path.display());
                }
            }
            Err(e) => eprintln!("[config] serialize config: {e}"),
        }
    }
}

/// Poll `/mission.json` and return `true` when a mission is actively running.
///
/// Uses a blocking HTTP GET with a 500 ms timeout; intended to be called from
/// a background thread at a low rate (~1 Hz is plenty).  Returns `false` on
/// any network error (game offline, timeout, etc.).
pub fn poll_mission_running(base_url: &str) -> bool {
    let url = format!("{}/mission.json", base_url.trim_end_matches('/'));
    let http = reqwest::blocking::ClientBuilder::new()
        .timeout(std::time::Duration::from_millis(500))
        .build()
        .unwrap_or_default();
    http.get(&url)
        .send()
        .ok()
        .and_then(|r| r.json::<JsonValue>().ok())
        .and_then(|j| {
            j.get("status")
                .and_then(|s| s.as_str())
                .map(|s| s == "running")
        })
        .unwrap_or(false)
}

// ── Calculator ────────────────────────────────────────────────────────────────

pub struct Calculator {
    indicators: Vec<IndicatorDef>,
}

impl Calculator {
    pub fn new(indicators: Vec<IndicatorDef>) -> Self { Self { indicators } }

    pub fn evaluate(&self, frame: &RawFrame) -> Vec<DisplayRow> {
        let mut rows = Vec::new();
        for ind in &self.indicators {
            if let Some(sw) = &ind.show_when {
                match self.eval_formula(sw, frame) {
                    Some(v) if v != 0.0 => {}
                    _ => continue,
                }
            }
            let value = match self.eval_formula(&ind.formula, frame) {
                Some(v) => v,
                None => continue,
            };
            rows.push(DisplayRow {
                label: ind.label.clone(),
                value_str: format_value(value, &ind.format),
                unit: ind.unit.clone(),
                color: resolve_color(value, ind, frame),
                style: ind.style.clone(),
            });
        }
        rows
    }

    fn eval_formula(&self, formula: &str, frame: &RawFrame) -> Option<f64> {
        use evalexpr::*;
        let mut ctx = HashMapContext::new();
        for (k, v) in frame {
            ctx.set_value(k.clone(), Value::Float(*v)).ok();
        }
        // Try float, then int, then bool (bool → 1.0 / 0.0).
        eval_float_with_context(formula, &ctx)
            .ok()
            .or_else(|| eval_int_with_context(formula, &ctx).ok().map(|i| i as f64))
            .or_else(|| eval_boolean_with_context(formula, &ctx).ok().map(|b| if b { 1.0 } else { 0.0 }))
    }
}

fn format_value(v: f64, fmt: &str) -> String {
    match fmt {
        "decimal1"  => format!("{:.1}", v),
        "decimal2"  => format!("{:.2}", v),
        "+decimal1" => format!("{:+.1}", v),
        "+decimal2" => format!("{:+.2}", v),
        "time" => {
            // Render seconds as H:MM:SS (or M:SS when < 1 hour).
            // Negative / NaN / Inf treated as "--:--".
            if !v.is_finite() || v < 0.0 {
                return "--:--".into();
            }
            let total = v as u64;
            let h = total / 3600;
            let m = (total % 3600) / 60;
            let s = total % 60;
            if h > 0 {
                format!("{}:{:02}:{:02}", h, m, s)
            } else {
                format!("{}:{:02}", m, s)
            }
        }
        _           => format!("{:.0}", v),
    }
}

fn resolve_color(v: f64, ind: &IndicatorDef, frame: &RawFrame) -> String {
    if let Some(c) = &ind.color { return c.clone(); }
    // crit checked first — takes priority over warn
    if let Some(t) = &ind.crit_above  { if let Some(tval) = t.eval(frame) { if v > tval { return "crit".into(); } } }
    if let Some(t) = &ind.crit_below  { if let Some(tval) = t.eval(frame) { if v < tval { return "crit".into(); } } }
    if let Some(t) = &ind.warn_below  { if let Some(tval) = t.eval(frame) { if v < tval { return "warn".into(); } } }
    if let Some(t) = &ind.warn_above  { if let Some(tval) = t.eval(frame) { if v > tval { return "warn".into(); } } }
    if let Some(t) = &ind.good_above  { if let Some(tval) = t.eval(frame) { if v > tval { return "good".into(); } } }
    if let Some(t) = &ind.good_below  { if let Some(tval) = t.eval(frame) { if v < tval { return "good".into(); } } }
    "value".into()
}

// ── Client ────────────────────────────────────────────────────────────────────

/// Mode of operation for internal fetch path.
enum FetchMode {
    /// Background threads keep `endpoint_cache` warm; fetch_raw reads cache.
    Background { endpoint_cache: EndpointCache },
    /// Synchronous fetch on each call (used by tests / with_config).
    Sync {
        http: Arc<dyn HttpClient>,
        base_url: String,
        cache: Mutex<HashMap<String, (Instant, JsonValue)>>,
        last_request: Mutex<HashMap<String, Instant>>,
        min_interval: Duration,
        consecutive_failures: Mutex<usize>,
        failed_until: Mutex<Option<Instant>>,
        failure_threshold: usize,
        failure_backoff: Duration,
        retry_limit: usize,
    },
}

/// Running state for computing time-derivative fields (e.g. SEP).
///
/// We keep a ring buffer of (Instant, TAS m/s) samples and derive dV/dt via
/// ordinary least-squares linear regression over the window.  This is far
/// more noise-resistant than a single-step finite difference on a quantised
/// integer TAS field.
struct DerivedState {
    /// Newest-last ring buffer of (sample_time, TAS m/s).  Max 8 entries,
    /// pruned to a 2-second sliding window.
    tas_history: VecDeque<(Instant, f64)>,
    /// Last TAS value pushed into the ring buffer.  We only push a new sample
    /// when the value changes so that repeated identical readings from the
    /// cache (injected at ~60 Hz while the server only updates at ~6 Hz) do
    /// not flood the buffer with zero-slope duplicates.
    last_tas_pushed: Option<f64>,

    /// Last fuel sample used to compute instantaneous consumption.
    last_fuel_kg: Option<f64>,
    last_fuel_time: Option<Instant>,
    /// EMA-smoothed fuel consumption rate (kg/h).  α = 0.25 so the display
    /// settles within a few API ticks (~1 s) without being too jittery.
    fuel_consume_ema: Option<f64>,
}

impl DerivedState {
    fn new() -> Self {
        Self {
            tas_history: VecDeque::with_capacity(8),
            last_tas_pushed: None,
            last_fuel_kg: None,
            last_fuel_time: None,
            fuel_consume_ema: None,
        }
    }
}

pub struct Client {
    fields: Vec<FieldDef>,
    windows: Mutex<Vec<(WindowDef, Calculator)>>,
    mode: FetchMode,
    fm_db: Arc<FmDb>,
    derived: Mutex<DerivedState>,
    /// FM database release tag currently loaded, e.g. `"v2.55.1.88"`.
    /// Empty string if no FM data is present.
    pub fm_version_tag: String,
}

impl Client {
    // ── Public constructors ──────────────────────────────────────────────────

    /// Production constructor: spawns background fetch threads per endpoint.
    pub fn new(base_url: Option<&str>) -> Result<Self, Error> {
        let window_defs = load_window_defs(None);
        Self::new_with_windows(window_defs, base_url)
    }

    /// Production constructor with pre-loaded window defs.
    pub fn new_with_windows(
        window_defs: Vec<WindowDef>,
        base_url: Option<&str>,
    ) -> Result<Self, Error> {
        let fields = load_fields(None);
        // Parse host:port from the base URL for raw-TCP connections.
        // Always resolve to 127.0.0.1 rather than "localhost" — on Windows,
        // "localhost" resolves to ::1 (IPv6) first; if WT only listens on
        // IPv4 the failed IPv6 connect attempt adds ~500ms per reconnect.
        let base = base_url.unwrap_or("http://localhost:8111").to_string();
        let host_port = {
            let s = base
                .trim_start_matches("http://")
                .trim_start_matches("https://")
                .trim_end_matches('/');
            // Replace hostnames that are known aliases for loopback with the
            // literal IPv4 address so TcpStream::connect never does DNS.
            if s.starts_with("localhost:") {
                s.replacen("localhost", "127.0.0.1", 1)
            } else {
                s.to_string()
            }
        };

        let endpoint_cache: EndpointCache = Arc::new(RwLock::new(HashMap::new()));

        // Collect unique endpoints
        let endpoints: std::collections::HashSet<String> =
            fields.iter()
                .map(|f| f.endpoint.clone())
                .filter(|e| e != "__virtual__")
                .collect();

        // Spawn one background fetch thread per endpoint.
        // Each thread owns a PersistentConn — a raw TcpStream that stays
        // open across requests, exactly like WTRTI's libhv AsyncHttpClient.
        for endpoint in endpoints {
            let cache = endpoint_cache.clone();
            let host = host_port.clone();
            std::thread::Builder::new()
                .name(format!("wt-fetch-{endpoint}"))
                .spawn(move || {
                    let path = if endpoint.starts_with('/') {
                        endpoint.clone()
                    } else {
                        format!("/{endpoint}")
                    };
                    let mut conn = PersistentConn::new(&host, &path);
                    let mut total_ms: u64 = 0;
                    let mut cycles: u64 = 0;

                    loop {
                        let t0 = Instant::now();
                        match conn.get() {
                            Ok(body) => {
                                if let Ok(v) = serde_json::from_str::<JsonValue>(&body) {
                                    if let Ok(mut c) = cache.write() {
                                        c.insert(endpoint.clone(), v);
                                    }
                                }
                            }
                            Err(e) => {
                                eprintln!("[fetch {endpoint}] error: {e}");
                                std::thread::sleep(Duration::from_millis(500));
                            }
                        }

                        let elapsed = t0.elapsed();
                        let elapsed_ms = elapsed.as_millis() as u64;
                        total_ms += elapsed_ms;
                        cycles += 1;

                        if cycles % 100 == 0 {
                            eprintln!("[fetch {endpoint}] avg over 100: {}ms", total_ms / 100);
                            total_ms = 0;
                        }

                        // If the fetch returned nearly instantly (game not running),
                        // back off so we don't spin-poll.
                        if elapsed < Duration::from_millis(50) {
                            std::thread::sleep(Duration::from_millis(200));
                        }
                    }
                })
                .expect("spawn fetch thread");
        }

        let windows = window_defs.into_iter()
            .map(|wd| { let calc = Calculator::new(wd.indicators.clone()); (wd, calc) })
            .collect();

        let fm_base = fm_base_dir();
        let fm_version_tag = check_and_update_fm(&fm_base);
        let fm_db = Arc::new(load_fm_db(None));

        Ok(Self {
            fields,
            windows: Mutex::new(windows),
            mode: FetchMode::Background { endpoint_cache },
            fm_db,
            derived: Mutex::new(DerivedState::new()),
            fm_version_tag,
        })
    }

    /// Test/compat constructor: synchronous HTTP, no background threads.
    pub fn with_config(
        http: Arc<dyn HttpClient>,
        base_url: String,
        fields: Vec<FieldDef>,
        indicators: Vec<IndicatorDef>,
    ) -> Self {
        let window = WindowDef {
            id: "default".to_string(),
            x: 100, y: 100,
            width: None, height: None,
            indicators,
            style: None,
        };
        Self::with_windows(http, base_url, fields, vec![window])
    }

    /// Test/compat constructor with explicit windows.
    pub fn with_windows(
        http: Arc<dyn HttpClient>,
        base_url: String,
        fields: Vec<FieldDef>,
        window_defs: Vec<WindowDef>,
    ) -> Self {
        let windows = window_defs.into_iter()
            .map(|wd| { let calc = Calculator::new(wd.indicators.clone()); (wd, calc) })
            .collect();
        Self {
            fields,
            windows: Mutex::new(windows),
            mode: FetchMode::Sync {
                http,
                base_url,
                cache: Mutex::new(HashMap::new()),
                last_request: Mutex::new(HashMap::new()),
                min_interval: Duration::from_millis(16),
                consecutive_failures: Mutex::new(0),
                failed_until: Mutex::new(None),
                failure_threshold: 10,
                failure_backoff: Duration::from_secs(5),
                retry_limit: 0,
            },
            fm_db: Arc::new(FmDb::new()),
            derived: Mutex::new(DerivedState::new()),
            fm_version_tag: String::new(),
        }
    }

    // ── Internal fetch ───────────────────────────────────────────────────────

    /// Fetch JSON for an endpoint — sync mode only (tests).
    pub fn fetch_json(&self, endpoint: &str) -> Result<JsonValue, Error> {
        let FetchMode::Sync {
            http, base_url, cache, last_request, min_interval,
            consecutive_failures, failed_until, failure_threshold,
            failure_backoff, retry_limit,
        } = &self.mode else {
            // In background mode, return cached value if available
            if let FetchMode::Background { endpoint_cache } = &self.mode {
                if let Ok(c) = endpoint_cache.read() {
                    if let Some(v) = c.get(endpoint) { return Ok(v.clone()); }
                }
            }
            return Err(Error::Other("background mode: no cached value".into()));
        };

        // Circuit breaker
        if let Ok(fu) = failed_until.lock() {
            if let Some(until) = *fu {
                if Instant::now() < until { return Err(Error::CircuitOpen); }
            }
        }
        // Rate limit
        let now = Instant::now();
        if let Ok(lr) = last_request.lock() {
            if let Some(last) = lr.get(endpoint) {
                if now.duration_since(*last) < *min_interval {
                    if let Ok(c) = cache.lock() {
                        if let Some((_, v)) = c.get(endpoint) { return Ok(v.clone()); }
                    }
                }
            }
        }

        let url = format!(
            "{}{}",
            base_url.trim_end_matches('/'),
            if endpoint.starts_with('/') { endpoint.to_string() } else { format!("/{endpoint}") }
        );

        let mut last_err: Option<Error> = None;
        for _ in 0..=*retry_limit {
            match http.get(&url) {
                Ok(body) => match serde_json::from_str::<JsonValue>(&body) {
                    Ok(v) => {
                        if let Ok(mut c) = cache.lock() { c.insert(endpoint.to_string(), (Instant::now(), v.clone())); }
                        if let Ok(mut lr) = last_request.lock() { lr.insert(endpoint.to_string(), Instant::now()); }
                        if let Ok(mut cf) = consecutive_failures.lock() { *cf = 0; }
                        if let Ok(mut fu) = failed_until.lock() { *fu = None; }
                        return Ok(v);
                    }
                    Err(e) => last_err = Some(Error::Parse(e)),
                },
                Err(e) => last_err = Some(e),
            }
        }

        if let Ok(mut cf) = consecutive_failures.lock() {
            *cf += 1;
            if *cf >= *failure_threshold {
                if let Ok(mut fu) = failed_until.lock() {
                    *fu = Some(Instant::now() + *failure_backoff);
                }
            }
        }
        Err(last_err.unwrap_or_else(|| Error::Other("unknown".into())))
    }

    /// Build a `RawFrame` from the current endpoint data (cache or sync fetch).
    pub fn fetch_raw(&self) -> RawFrame {
        let mut frame = RawFrame::new();

        match &self.mode {
            FetchMode::Background { endpoint_cache } => {
                // Instant read — no HTTP
                let cache = match endpoint_cache.read() {
                    Ok(c) => c,
                    Err(_) => return frame,
                };
                let mut by_endpoint: HashMap<&str, Vec<&FieldDef>> = HashMap::new();
                for f in &self.fields {
                    if f.endpoint != "__virtual__" {
                        by_endpoint.entry(&f.endpoint).or_default().push(f);
                    }
                }
                for (endpoint, fields) in &by_endpoint {
                    if let Some(json) = cache.get(*endpoint) {
                        Self::extract_fields(json, fields, &mut frame);
                    }
                }
                // Inject FM fields from the current vehicle name (from /indicators "type")
                if let Some(indicators_json) = cache.get("indicators").or_else(|| cache.get("/indicators")) {
                    if let Some(vehicle) = indicators_json.get("type").and_then(|v| v.as_str()) {
                        if let Some(rec) = self.fm_db.get(vehicle) {
                            inject_fm_fields(&mut frame, rec);
                            // Compute dynamic flaps speed limit based on current extension
                            if let Some(flaps_pct) = frame.get("flaps_pct").copied() {
                                if let Some(spd) = compute_named_flaps_current(flaps_pct, rec) {
                                    frame.insert("fm_crit_flaps_current".into(), spd);
                                }
                            }
                        }
                    }
                }
                // Inject derived / time-derivative virtual fields (e.g. SEP).
                if let Ok(mut ds) = self.derived.lock() {
                    inject_derived_fields(&mut frame, &mut ds, Instant::now());
                }
            }
            FetchMode::Sync { .. } => {
                // Group and fetch sequentially (tests use deterministic fixtures)
                let mut by_endpoint: HashMap<&str, Vec<&FieldDef>> = HashMap::new();
                for f in &self.fields { by_endpoint.entry(&f.endpoint).or_default().push(f); }
                for (endpoint, fields) in &by_endpoint {
                    if *endpoint == "__virtual__" { continue; }
                    if let Ok(json) = self.fetch_json(endpoint) {
                        Self::extract_fields(&json, fields, &mut frame);
                    }
                }
                // Inject FM fields from /indicators "type" in sync mode too
                if let Ok(indicators_json) = self.fetch_json("indicators").or_else(|_| self.fetch_json("/indicators")) {
                    if let Some(vehicle) = indicators_json.get("type").and_then(|v| v.as_str()) {
                        if let Some(rec) = self.fm_db.get(vehicle) {
                            inject_fm_fields(&mut frame, rec);
                            if let Some(flaps_pct) = frame.get("flaps_pct").copied() {
                                if let Some(spd) = compute_named_flaps_current(flaps_pct, rec) {
                                    frame.insert("fm_crit_flaps_current".into(), spd);
                                }
                            }
                        }
                    }
                }
                // Inject derived / time-derivative virtual fields (e.g. SEP).
                if let Ok(mut ds) = self.derived.lock() {
                    inject_derived_fields(&mut frame, &mut ds, Instant::now());
                }
            }
        }
        frame
    }

    fn extract_fields(json: &JsonValue, fields: &[&FieldDef], frame: &mut RawFrame) {
        for field in fields {
            let val = json.get(&field.api_key);
            match field.field_type.as_str() {
                "f64" => {
                    if let Some(n) = val.and_then(|v| v.as_f64()) {
                        frame.insert(field.id.clone(), n);
                    }
                }
                "bool" => {
                    if let Some(b) = val.and_then(|v| v.as_bool()) {
                        frame.insert(field.id.clone(), if b { 1.0 } else { 0.0 });
                    }
                }
                _ => {}
            }
        }
    }

    // ── Public API ───────────────────────────────────────────────────────────

    pub fn fetch_display_windows(&self) -> Vec<WindowRows> {
        let frame = self.fetch_raw();
        let offline = frame.is_empty();

        let windows = self.windows.lock().unwrap_or_else(|e| e.into_inner());
        windows.iter().map(|(wd, calc)| {
            let rows = if offline {
                vec![DisplayRow {
                    label: "WT".into(), value_str: "offline".into(),
                    unit: String::new(), color: "unit".into(), style: None,
                }]
            } else {
                let mut rows = calc.evaluate(&frame);
                if rows.is_empty() {
                    rows.push(DisplayRow {
                        label: "WT".into(), value_str: "waiting".into(),
                        unit: String::new(), color: "unit".into(), style: None,
                    });
                }
                rows
            };
            WindowRows {
                id: wd.id.clone(), x: wd.x, y: wd.y,
                width: wd.computed_width(), height: wd.computed_height(),
                rows,
                style: wd.style.clone(),
            }
        }).collect()
    }

    /// Convenience: flatten all windows into one row list (used by CLI).
    pub fn fetch_display_rows(&self) -> Vec<DisplayRow> {
        self.fetch_display_windows().into_iter().flat_map(|w| w.rows).collect()
    }

    /// Reload `indicators.json` from disk and replace the calculator set
    /// in-place.  The existing background fetch threads continue running
    /// (their cached data remains valid); only the display formula/threshold
    /// logic is updated.  Window layout changes (new windows, repositioning)
    /// require an application restart.
    /// Reload `indicators.json` from disk and hot-swap the calculator set.
    ///
    /// Returns `Ok(())` on success.  On any parse or I/O error the existing
    /// windows are left unchanged and a human-readable message is returned as
    /// `Err(String)` so the caller can surface it to the user.
    pub fn reload_window_defs(&self) -> Result<Vec<WindowDef>, String> {
        let new_defs = try_load_window_defs(None)?;
        let new_windows: Vec<(WindowDef, Calculator)> = new_defs.iter()
            .map(|wd| (wd.clone(), Calculator::new(wd.indicators.clone())))
            .collect();
        if let Ok(mut lock) = self.windows.lock() {
            *lock = new_windows;
        }
        Ok(new_defs)
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_fields() -> Vec<FieldDef> {
        vec![
            FieldDef { id: "altitude_m".into(), endpoint: "/state".into(), api_key: "H, m".into(), label: "ALT".into(), unit: "m".into(), field_type: "f64".into() },
            FieldDef { id: "tas_kmh".into(), endpoint: "/state".into(), api_key: "TAS, km/h".into(), label: "TAS".into(), unit: "km/h".into(), field_type: "f64".into() },
            FieldDef { id: "fuel_kg".into(), endpoint: "/state".into(), api_key: "Mfuel, kg".into(), label: "FUEL".into(), unit: "kg".into(), field_type: "f64".into() },
            FieldDef { id: "valid".into(), endpoint: "/state".into(), api_key: "valid".into(), label: "VALID".into(), unit: String::new(), field_type: "bool".into() },
        ]
    }

    fn make_indicators() -> Vec<IndicatorDef> {
        vec![
            IndicatorDef { id: "alt".into(), label: "ALT".into(), unit: "m".into(), formula: "altitude_m".into(), format: "integer".into(), color: None, warn_below: None, warn_above: None, good_above: None, good_below: None, crit_above: None, crit_below: None, show_when: Some("valid".into()), style: None },
            IndicatorDef { id: "fuel".into(), label: "FUEL".into(), unit: "kg".into(), formula: "fuel_kg".into(), format: "integer".into(), color: None, warn_below: Some(Threshold::Fixed(100.0)), warn_above: None, good_above: None, good_below: None, crit_above: None, crit_below: None, show_when: Some("valid".into()), style: None },
        ]
    }

    fn state_fixture() -> &'static str {
        r#"{"valid": true, "H, m": 4936, "TAS, km/h": 237, "Mfuel, kg": 750}"#
    }

    fn make_client(fixture_body: &str) -> Client {
        let mut fixtures = HashMap::new();
        fixtures.insert("/state".to_string(), fixture_body.to_string());
        let http = FixtureHttpClient::new(fixtures);
        Client::with_config(Arc::new(http), "http://localhost:8111".to_string(), make_fields(), make_indicators())
    }

    #[test]
    fn test_fetch_raw_from_fixtures() {
        let client = make_client(state_fixture());
        let frame = client.fetch_raw();
        assert_eq!(frame.get("altitude_m"), Some(&4936.0));
        assert_eq!(frame.get("tas_kmh"), Some(&237.0));
        assert_eq!(frame.get("valid"), Some(&1.0));
    }

    #[test]
    fn test_display_rows_from_fixtures() {
        let client = make_client(state_fixture());
        let rows = client.fetch_display_rows();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].label, "ALT");
        assert_eq!(rows[0].value_str, "4936");
        assert_eq!(rows[0].unit, "m");
        assert_eq!(rows[0].color, "value");
        assert_eq!(rows[1].label, "FUEL");
        assert_eq!(rows[1].value_str, "750");
        assert_eq!(rows[1].color, "value");
    }

    #[test]
    fn test_warn_color_when_fuel_low() {
        let client = make_client(r#"{"valid": true, "H, m": 100, "TAS, km/h": 200, "Mfuel, kg": 50}"#);
        let rows = client.fetch_display_rows();
        let fuel = rows.iter().find(|r| r.label == "FUEL").unwrap();
        assert_eq!(fuel.color, "warn");
    }

    #[test]
    fn test_offline_returns_fallback_row() {
        let http = FixtureHttpClient::new(HashMap::new());
        let client = Client::with_config(Arc::new(http), "http://localhost:8111".to_string(), make_fields(), make_indicators());
        let rows = client.fetch_display_rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].value_str, "offline");
    }

    #[test]
    fn test_show_when_hides_row() {
        let client = make_client(r#"{"valid": false, "H, m": 0, "TAS, km/h": 0, "Mfuel, kg": 0}"#);
        let rows = client.fetch_display_rows();
        assert!(rows.iter().all(|r| r.label != "ALT" && r.label != "FUEL"));
    }

    #[test]
    fn test_show_when_expression() {
        // show_when with a boolean expression: show only when both conditions hold
        let calc = Calculator::new(vec![IndicatorDef {
            id: "gspd".into(), label: "GSPD".into(), unit: "%".into(),
            formula: "ias / crit * 100".into(), format: "integer".into(),
            color: None, warn_below: None, warn_above: None, good_above: None, good_below: None,
            crit_above: None, crit_below: None,
            show_when: Some("valid != 0.0 && gear_pct > 0.0".into()),
            style: None,
        }]);

        let mut frame_gear_up = RawFrame::new();
        frame_gear_up.insert("valid".into(), 1.0);
        frame_gear_up.insert("gear_pct".into(), 0.0);
        frame_gear_up.insert("ias".into(), 150.0);
        frame_gear_up.insert("crit".into(), 300.0);
        assert!(calc.evaluate(&frame_gear_up).is_empty(), "should be hidden when gear up");

        let mut frame_gear_down = RawFrame::new();
        frame_gear_down.insert("valid".into(), 1.0);
        frame_gear_down.insert("gear_pct".into(), 100.0);
        frame_gear_down.insert("ias".into(), 150.0);
        frame_gear_down.insert("crit".into(), 300.0);
        let rows = calc.evaluate(&frame_gear_down);
        assert_eq!(rows.len(), 1, "should be visible when gear down");
        assert_eq!(rows[0].value_str, "50");
    }

    #[test]
    fn test_calculator_arithmetic_formula() {
        let calc = Calculator::new(vec![IndicatorDef {
            id: "fuel_pct".into(), label: "FUEL".into(), unit: "%".into(),
            formula: "fuel_kg / fuel_kg0 * 100".into(), format: "decimal1".into(),
            color: None, warn_below: Some(Threshold::Fixed(20.0)), warn_above: None, good_above: None, good_below: None, crit_above: None, crit_below: None, show_when: None, style: None,
        }]);
        let mut frame = RawFrame::new();
        frame.insert("fuel_kg".into(), 150.0);
        frame.insert("fuel_kg0".into(), 500.0);
        let rows = calc.evaluate(&frame);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].value_str, "30.0");
        assert_eq!(rows[0].color, "value");
    }

    #[test]
    fn test_rate_limit_uses_cache() {
        let mut fixtures = HashMap::new();
        fixtures.insert("/state".to_string(), state_fixture().to_string());
        let fixture = FixtureHttpClient::new(fixtures);
        let calls = fixture.calls.clone();
        let client = Client::with_config(Arc::new(fixture), "http://localhost:8111".to_string(), make_fields(), make_indicators());
        let _ = client.fetch_json("/state").unwrap();
        let _ = client.fetch_json("/state").unwrap();
        let count = calls.lock().unwrap().get("/state").copied().unwrap_or(0);
        assert_eq!(count, 1, "expected only 1 HTTP call due to rate-limit cache");
    }

    #[test]
    fn test_multi_window_fetch() {
        let mut fixtures = HashMap::new();
        fixtures.insert("/state".to_string(), state_fixture().to_string());
        let http = FixtureHttpClient::new(fixtures);
        let window_defs = vec![
            WindowDef { id: "flight".to_string(), x: 100, y: 100, width: None, height: None, indicators: vec![make_indicators()[0].clone()], style: None },
            WindowDef { id: "engine".to_string(), x: 400, y: 100, width: None, height: None, indicators: vec![make_indicators()[1].clone()], style: None },
        ];
        let client = Client::with_windows(Arc::new(http), "http://localhost:8111".to_string(), make_fields(), window_defs);
        let windows = client.fetch_display_windows();
        assert_eq!(windows.len(), 2);
        assert_eq!(windows[0].id, "flight");
        assert_eq!(windows[0].rows[0].label, "ALT");
        assert_eq!(windows[1].id, "engine");
        assert_eq!(windows[1].rows[0].label, "FUEL");
    }

    #[test]
    fn test_format_time() {
        assert_eq!(format_value(0.0,    "time"), "0:00");
        assert_eq!(format_value(59.0,   "time"), "0:59");
        assert_eq!(format_value(60.0,   "time"), "1:00");
        assert_eq!(format_value(90.0,   "time"), "1:30");
        assert_eq!(format_value(3599.0, "time"), "59:59");
        assert_eq!(format_value(3600.0, "time"), "1:00:00");
        assert_eq!(format_value(3661.0, "time"), "1:01:01");
        assert_eq!(format_value(-1.0,   "time"), "--:--");
        assert_eq!(format_value(f64::INFINITY, "time"), "--:--");
    }

    #[test]
    fn test_named_flaps_a2d() {
        // a2d: CombatFlaps=20 (≠0), TakeoffFlaps=33 (≠0), raw pairs after filtering ratio=0:
        // (0.21,420), (0.33,351), (1.0,301)
        // → combat=(0.21,420), takeoff=(0.33,351), landing=(1.0,301)
        let rec = FmRecord {
            combat_flaps_ratio:      Some(0.21), combatflaps_crit_speed:  Some(420.0),
            takeoff_flaps_ratio:     Some(0.33), takeoffflaps_crit_speed: Some(351.0),
            landing_flaps_ratio:     Some(1.0),  landingflaps_crit_speed: Some(301.0),
            ..Default::default()
        };
        assert_eq!(compute_named_flaps_current(0.0,  &rec), None);          // flaps up
        assert_eq!(compute_named_flaps_current(10.0, &rec), Some(420.0));   // ratio=0.1 ≤ 0.21 → combat
        assert_eq!(compute_named_flaps_current(21.0, &rec), Some(420.0));   // ratio=0.21 exactly → combat
        assert_eq!(compute_named_flaps_current(25.0, &rec), Some(351.0));   // ratio=0.25 > 0.21, ≤ 0.33 → takeoff
        assert_eq!(compute_named_flaps_current(33.0, &rec), Some(351.0));   // ratio=0.33 exactly → takeoff
        assert_eq!(compute_named_flaps_current(50.0, &rec), Some(301.0));   // ratio=0.5 > 0.33 → landing
        assert_eq!(compute_named_flaps_current(100.0,&rec), Some(301.0));   // full flaps → landing
    }

    #[test]
    fn test_named_flaps_a10a_no_combat() {
        // a-10a: CombatFlaps=0 (no combat), TakeoffFlaps=24 (≠0), 2 pairs after filter:
        // (0.33,740), (1.0,370.4)
        // → no combat, takeoff=(0.33,740), landing=(1.0,370.4)
        let rec = FmRecord {
            combat_flaps_ratio:      None,        combatflaps_crit_speed:  None,
            takeoff_flaps_ratio:     Some(0.33),  takeoffflaps_crit_speed: Some(740.0),
            landing_flaps_ratio:     Some(1.0),   landingflaps_crit_speed: Some(370.4),
            ..Default::default()
        };
        assert_eq!(compute_named_flaps_current(0.0,  &rec), None);
        assert_eq!(compute_named_flaps_current(10.0, &rec), Some(740.0));   // ratio=0.1 ≤ 0.33 → takeoff
        assert_eq!(compute_named_flaps_current(33.0, &rec), Some(740.0));   // exactly at takeoff
        assert_eq!(compute_named_flaps_current(50.0, &rec), Some(370.4));   // past takeoff → landing
        assert_eq!(compute_named_flaps_current(100.0,&rec), Some(370.4));
    }

    #[test]
    fn test_named_flaps_a20g_no_takeoff() {
        // a-20g: CombatFlaps=20 (≠0), TakeoffFlaps=33 (≠0), but only 2 pairs after filter:
        // (0.1,445), (1.0,296)
        // → combat=(0.1,445), only 1 pair left for landing so no takeoff, landing=(1.0,296)
        let rec = FmRecord {
            combat_flaps_ratio:      Some(0.1),  combatflaps_crit_speed:  Some(445.0),
            takeoff_flaps_ratio:     None,        takeoffflaps_crit_speed: None,
            landing_flaps_ratio:     Some(1.0),  landingflaps_crit_speed: Some(296.0),
            ..Default::default()
        };
        assert_eq!(compute_named_flaps_current(0.0,  &rec), None);
        assert_eq!(compute_named_flaps_current(5.0,  &rec), Some(445.0));   // ≤ 0.1 → combat
        assert_eq!(compute_named_flaps_current(10.0, &rec), Some(445.0));   // exactly at combat
        assert_eq!(compute_named_flaps_current(20.0, &rec), Some(296.0));   // past combat, no takeoff → landing
        assert_eq!(compute_named_flaps_current(100.0,&rec), Some(296.0));
    }

    #[test]
    fn test_named_flaps_no_data() {
        let rec = FmRecord::default();
        assert_eq!(compute_named_flaps_current(50.0, &rec), None);
        assert_eq!(compute_named_flaps_current(0.0,  &rec), None);
    }

    #[test]
    fn test_strip_jsonc_line_comment() {
        let src = r#"{ "a": 1 // comment
, "b": 2 }"#;
        let v: serde_json::Value = serde_json::from_reader(
            json_comments::StripComments::new(src.as_bytes())
        ).expect("parse");
        assert_eq!(v["a"], 1);
        assert_eq!(v["b"], 2);
    }

    #[test]
    fn test_strip_jsonc_block_comment() {
        let src = r#"{ "a": /* ignore this */ 42 }"#;
        let v: serde_json::Value = serde_json::from_reader(
            json_comments::StripComments::new(src.as_bytes())
        ).expect("parse");
        assert_eq!(v["a"], 42);
    }


    // ── SEP / inject_derived_fields tests ────────────────────────────────────

    #[test]
    fn test_sep_first_call_equals_vy() {
        // On the very first call there is only one sample — no regression
        // possible, so SEP must fall back to vy_ms.
        let mut state = DerivedState::new();
        let mut frame = RawFrame::new();
        frame.insert("vy_ms".into(), 7.0);
        frame.insert("tas_kmh".into(), 360.0); // 100 m/s
        inject_derived_fields(&mut frame, &mut state, Instant::now());
        assert_eq!(frame.get("sep"), Some(&7.0));
    }

    #[test]
    fn test_sep_zero_accel_equals_vy() {
        // Constant TAS → regression slope ≈ 0 → SEP ≈ vy_ms.
        let mut state = DerivedState::new();
        let t0 = Instant::now();
        for i in 0..4u64 {
            let mut frame = RawFrame::new();
            frame.insert("vy_ms".into(), 5.0);
            frame.insert("tas_kmh".into(), 360.0); // 100 m/s constant
            inject_derived_fields(&mut frame, &mut state, t0 + Duration::from_millis(170 * i));
        }
        // After ≥2 samples with zero slope the last SEP must be ≈ vy_ms = 5.
        let mut frame_last = RawFrame::new();
        frame_last.insert("vy_ms".into(), 5.0);
        frame_last.insert("tas_kmh".into(), 360.0);
        inject_derived_fields(&mut frame_last, &mut state, t0 + Duration::from_millis(680));
        let sep = *frame_last.get("sep").unwrap();
        assert!((sep - 5.0).abs() < 0.5, "expected sep≈5.0, got {sep}");
    }

    #[test]
    fn test_sep_positive_accel() {
        // TAS increasing at exactly 10 m/s² from 200 m/s.
        // SEP = vy_ms + (V/g)*a = 0 + (200/9.81)*10 ≈ 203.9 m/s.
        let mut state = DerivedState::new();
        let t0 = Instant::now();
        let accel_ms2 = 10.0_f64;
        let v0_ms = 200.0_f64;
        for i in 0..5u64 {
            let dt_s = 0.17 * i as f64;
            let tas_ms = v0_ms + accel_ms2 * dt_s;
            let tas_kmh = tas_ms * 3.6;
            let mut frame = RawFrame::new();
            frame.insert("vy_ms".into(), 0.0);
            frame.insert("tas_kmh".into(), tas_kmh);
            inject_derived_fields(&mut frame, &mut state, t0 + Duration::from_millis((170 * i) as u64));
        }
        // Final sample
        let dt_s = 0.17 * 5.0_f64;
        let tas_ms_final = v0_ms + accel_ms2 * dt_s;
        let mut frame_final = RawFrame::new();
        frame_final.insert("vy_ms".into(), 0.0);
        frame_final.insert("tas_kmh".into(), tas_ms_final * 3.6);
        inject_derived_fields(&mut frame_final, &mut state, t0 + Duration::from_millis(850));
        let sep = *frame_final.get("sep").unwrap();
        let expected = tas_ms_final / 9.81 * accel_ms2;
        assert!(
            (sep - expected).abs() < 5.0,
            "expected sep≈{expected:.1}, got {sep:.1}"
        );
    }

    #[test]
    fn test_sep_clamp() {
        // An absurd TAS spike should be clamped to ±300.
        let mut state = DerivedState::new();
        let t0 = Instant::now();
        // Seed one sane sample first.
        let mut f0 = RawFrame::new();
        f0.insert("vy_ms".into(), 0.0);
        f0.insert("tas_kmh".into(), 360.0);
        inject_derived_fields(&mut f0, &mut state, t0);
        // Now send a wildly different TAS 200 ms later.
        let mut f1 = RawFrame::new();
        f1.insert("vy_ms".into(), 0.0);
        f1.insert("tas_kmh".into(), 36000.0); // +9900 m/s in 200 ms
        inject_derived_fields(&mut f1, &mut state, t0 + Duration::from_millis(200));
        let sep = *f1.get("sep").unwrap();
        assert!(sep <= 300.0 && sep >= -300.0, "sep out of clamp range: {sep}");
    }

    #[test]
    fn test_sep_thrust_computed() {
        // With known thrust, empty mass, fuel, and TAS, verify sep_thrust formula.
        // sep_thrust = thrust_kgf * tas_ms / (em + fuel)
        //            = 5000.0 * 200.0 / 10000.0 = 100.0 m/s
        let mut state = DerivedState::new();
        let mut frame = RawFrame::new();
        frame.insert("vy_ms".into(), 0.0);
        frame.insert("tas_kmh".into(), 720.0);     // 200 m/s
        frame.insert("thrust_1_kgs".into(), 5000.0); // kgf
        frame.insert("mfuel_kg".into(), 2000.0);
        frame.insert("fm_empty_mass_kg".into(), 8000.0);
        inject_derived_fields(&mut frame, &mut state, Instant::now());
        let sep_thrust = *frame.get("sep_thrust").unwrap();
        let expected = 5000.0_f64 * 200.0 / 10000.0; // 100.0
        assert!((sep_thrust - expected).abs() < 0.01, "expected {expected}, got {sep_thrust}");
    }

    #[test]
    fn test_sep_thrust_absent_without_mass() {
        // sep_thrust must not appear when fm_empty_mass_kg is missing.
        let mut state = DerivedState::new();
        let mut frame = RawFrame::new();
        frame.insert("vy_ms".into(), 0.0);
        frame.insert("tas_kmh".into(), 720.0);
        frame.insert("thrust_1_kgs".into(), 5000.0);
        frame.insert("mfuel_kg".into(), 2000.0);
        // fm_empty_mass_kg intentionally absent
        inject_derived_fields(&mut frame, &mut state, Instant::now());
        assert!(frame.get("sep_thrust").is_none(), "sep_thrust should be absent without fm_empty_mass_kg");
    }

    #[test]
    fn test_crit_g_pos_matches_wtrti() {
        // Verified against WTRTI State window (F-4C simultaneous snapshot):
        //   fm_crit_wing_overload_pos = 1.105e6 N (per-wing, from FM CSV)
        //   mass_total = empty(13190) + fuel(5768.27) = 18958.27 kg
        //   WTRTI crit_g_pos = 11.790010 (uses slightly different mass; we get ~11.88)
        // At minimum, verify the formula is: 2 * overload / (mass * 9.81)
        let mut state = DerivedState::new();
        let mut frame = RawFrame::new();
        frame.insert("vy_ms".into(), 0.0);
        frame.insert("tas_kmh".into(), 720.0);
        frame.insert("fm_empty_mass_kg".into(), 13190.0);
        frame.insert("mfuel_kg".into(), 5768.271973);
        frame.insert("fm_crit_wing_overload_pos".into(), 1.105e6);
        inject_derived_fields(&mut frame, &mut state, Instant::now());
        let crit_g = *frame.get("crit_g_pos").expect("crit_g_pos should be present");
        let expected = 2.0 * 1.105e6_f64 / ((13190.0 + 5768.271973) * 9.81);
        assert!((crit_g - expected).abs() < 0.001, "expected {expected:.4}, got {crit_g:.4}");
        // Should be approximately 11.88 (close to WTRTI's 11.79 — small mass discrepancy from crew weight)
        assert!(crit_g > 11.0 && crit_g < 13.0, "crit_g_pos out of expected range: {crit_g}");
    }

    #[test]
    fn test_crit_g_pos_absent_without_overload() {
        // crit_g_pos must not appear when fm_crit_wing_overload_pos is missing.
        let mut state = DerivedState::new();
        let mut frame = RawFrame::new();
        frame.insert("vy_ms".into(), 0.0);
        frame.insert("tas_kmh".into(), 720.0);
        frame.insert("fm_empty_mass_kg".into(), 13190.0);
        frame.insert("mfuel_kg".into(), 5768.0);
        // fm_crit_wing_overload_pos intentionally absent
        inject_derived_fields(&mut frame, &mut state, Instant::now());
        assert!(frame.get("crit_g_pos").is_none(), "crit_g_pos should be absent without fm_crit_wing_overload_pos");
    }

    #[test]
    fn test_fuel_consume_calc_basic() {
        // Two frames 1 second apart, burning 1 kg → 3600 kg/h raw rate.
        // First call primes the state; second call emits the field.
        use std::time::Duration;
        let t0 = Instant::now();
        let mut state = DerivedState::new();

        let mut f0 = RawFrame::new();
        f0.insert("vy_ms".into(), 0.0);
        f0.insert("tas_kmh".into(), 400.0);
        f0.insert("mfuel_kg".into(), 500.0);
        inject_derived_fields(&mut f0, &mut state, t0);
        // First call just primes; no output yet.
        assert!(f0.get("fuel_consume_calc").is_none());

        let mut f1 = RawFrame::new();
        f1.insert("vy_ms".into(), 0.0);
        f1.insert("tas_kmh".into(), 400.0);
        f1.insert("mfuel_kg".into(), 499.0); // burned 1 kg in 1 s = 3600 kg/h
        inject_derived_fields(&mut f1, &mut state, t0 + Duration::from_secs(1));
        let rate = *f1.get("fuel_consume_calc").expect("fuel_consume_calc should be present");
        // EMA first sample = raw rate = 3600 kg/h
        assert!((rate - 3600.0).abs() < 1.0, "expected ~3600 kg/h, got {rate}");
    }

    #[test]
    fn test_fuel_consume_calc_absent_when_native_present() {
        // When `fuel_consume` is already in the frame (native API field),
        // `fuel_consume_calc` must NOT be emitted.
        use std::time::Duration;
        let t0 = Instant::now();
        let mut state = DerivedState::new();

        let mut f0 = RawFrame::new();
        f0.insert("vy_ms".into(), 0.0);
        f0.insert("tas_kmh".into(), 400.0);
        f0.insert("mfuel_kg".into(), 500.0);
        f0.insert("fuel_consume".into(), 250.0);
        inject_derived_fields(&mut f0, &mut state, t0);

        let mut f1 = RawFrame::new();
        f1.insert("vy_ms".into(), 0.0);
        f1.insert("tas_kmh".into(), 400.0);
        f1.insert("mfuel_kg".into(), 499.0);
        f1.insert("fuel_consume".into(), 250.0);
        inject_derived_fields(&mut f1, &mut state, t0 + Duration::from_secs(1));
        assert!(f1.get("fuel_consume_calc").is_none(), "should not emit calc when native present");
    }

    #[test]
    fn test_fuel_consume_calc_resets_on_refuel() {
        // EMA should reset when fuel goes up (refuel / new mission).
        use std::time::Duration;
        let t0 = Instant::now();
        let mut state = DerivedState::new();

        // Prime with a normal burn
        let mut f0 = RawFrame::new();
        f0.insert("vy_ms".into(), 0.0);
        f0.insert("tas_kmh".into(), 400.0);
        f0.insert("mfuel_kg".into(), 500.0);
        inject_derived_fields(&mut f0, &mut state, t0);
        let mut f1 = RawFrame::new();
        f1.insert("vy_ms".into(), 0.0);
        f1.insert("tas_kmh".into(), 400.0);
        f1.insert("mfuel_kg".into(), 499.0);
        inject_derived_fields(&mut f1, &mut state, t0 + Duration::from_secs(1));
        assert!(f1.get("fuel_consume_calc").is_some());

        // Now fuel goes up (refuel)
        let mut f2 = RawFrame::new();
        f2.insert("vy_ms".into(), 0.0);
        f2.insert("tas_kmh".into(), 400.0);
        f2.insert("mfuel_kg".into(), 600.0);
        inject_derived_fields(&mut f2, &mut state, t0 + Duration::from_secs(2));
        // EMA was reset; no output this tick
        assert!(f2.get("fuel_consume_calc").is_none(), "should reset EMA on refuel");
    }

    #[test]
    fn test_strip_jsonc_preserves_url_in_string() {
        // "//" inside a string must NOT be treated as a comment.
        let src = r#"{ "url": "http://exmple.com" }"#;
        let v: serde_json::Value = serde_json::from_reader(
            json_comments::StripComments::new(src.as_bytes())
        ).expect("parse");
        assert_eq!(v["url"], "http://exmple.com");
    }

    #[test]
    fn test_overlay_color_serde_hex_rgb() {
        let c: OverlayColor = serde_json::from_str(r##""#FF8040""##).unwrap();
        assert_eq!(c, OverlayColor([0xFF, 0x80, 0x40, 0xFF]));
        let s = serde_json::to_string(&c).unwrap();
        assert_eq!(s, r##""#FF8040""##);
    }

    #[test]
    fn test_overlay_color_serde_hex_rgba() {
        let c: OverlayColor = serde_json::from_str(r##""#FF8040A0""##).unwrap();
        assert_eq!(c, OverlayColor([0xFF, 0x80, 0x40, 0xA0]));
        let s = serde_json::to_string(&c).unwrap();
        assert_eq!(s, r##""#FF8040A0""##);
    }

    #[test]
    fn test_overlay_color_serde_array_rgb() {
        let c: OverlayColor = serde_json::from_str("[255, 128, 64]").unwrap();
        assert_eq!(c, OverlayColor([255, 128, 64, 255]));
    }

    #[test]
    fn test_overlay_color_serde_array_rgba() {
        let c: OverlayColor = serde_json::from_str("[255, 128, 64, 160]").unwrap();
        assert_eq!(c, OverlayColor([255, 128, 64, 160]));
    }

    #[test]
    fn test_overlay_color_serde_rgb_fn() {
        let c: OverlayColor = serde_json::from_str(r#""rgb(255,128,64)""#).unwrap();
        assert_eq!(c, OverlayColor([255, 128, 64, 255]));
    }

    #[test]
    fn test_overlay_color_serde_rgba_fn() {
        let c: OverlayColor = serde_json::from_str(r#""rgba(255, 128, 64, 160)""#).unwrap();
        assert_eq!(c, OverlayColor([255, 128, 64, 160]));
    }

    #[test]
    fn test_render_style_roundtrip() {
        let style = RenderStyle {
            font_size: Some(24.0),
            c_warn: Some(OverlayColor([255, 200, 0, 255])),
            ..Default::default()
        };
        let json = serde_json::to_string(&style).unwrap();
        let parsed: RenderStyle = serde_json::from_str(&json).unwrap();
        assert_eq!(style, parsed);
        // Absent fields should not appear in the JSON
        assert!(!json.contains("line_height"));
        assert!(!json.contains("c_label"));
    }
}
