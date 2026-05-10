# War Thunder BYOH — War Thunder Bring Your Own HUD

A lightweight, local-only in-game overlay for [War Thunder](https://warthunder.com/), built as an open-source alternative to [WTRTI](https://mesofthorny.github.io/WTRTI/).

Built on top of flight model data shared in the [warthunder-byo-fm](https://github.com/SpaceCapo/warthunder-byo-fm) GitHub Repo.

Displays real-time flight data, vehicle statistics, and session information directly on top of the game — with no process injection and no anti-cheat risk.

---

## Features

- **Real-time HUD overlay** — transparent, always-on-top window rendering configurable stat windows over the game
- **Data-driven indicators** — define any combination of fields via `indicators.json`; supports expressions, thresholds, and colour tokens
- **Two render backends** — GPU path (wgpu + egui, default) and a lightweight GDI fallback for Windows
- **~60 Hz display, ~6 Hz data** — overlay renders smoothly from a cached snapshot; no HTTP on the render hot path
- **Drag-to-reposition** — left-click drag any overlay window to move it; updated positions are written back to `indicators.json` automatically (debounced 800 ms after the last move, and on clean exit)
 - **Settings window + hot-reload** — GPU builds include a settings window with a simple Settings tab and a "Reload indicators & config" button. Reload validates the files before applying so invalid JSON or expression errors won't replace your running configuration. If reload fails you'll see an error dialog with details.
 - **Persistent config** — `config.json` (next to `indicators.json`) stores visibility preferences (eg. `always_show`, `show_when_byoh_foreground`, `only_during_mission`). The settings UI exposes the most-used toggles; the mission-only toggle is currently hidden in the UI while mission-polling is stabilised but the config key remains supported.
- **Flight model database** — CSV database of 1,537 aircraft with display names, types, and performance parameters; auto-updates from [warthunder-byo-fm](https://github.com/SpaceCapo/warthunder-byo-fm) releases on startup
- **Field catalog** — scraped catalog of all known War Thunder local API fields (`data/fields.json`)
- **Cross-platform** — builds for Windows (x86_64), Linux (glibc and musl static), and macOS via Docker cross-compilation
- **Local-only by default** — queries only the game's own `localhost` API; no data leaves your machine

---

## How It Works

War Thunder exposes a local HTTP API on `localhost:8111` while the game is running. WT BYOH reads from these endpoints (`/state`, `/indicators`, `/map_info`, etc.) at the game's native rate (~6 Hz), caches the results, and feeds the overlay renderer at ~60 fps — keeping the HUD visually smooth without hammering the API.

The overlay runs as a completely separate process. It does not inject into the game, does not hook system input, and does not read game memory. The only communication with the game is through its own documented local API.

---

## Project Layout

```
crates/
  core_client/   Rust library — War Thunder API client + display engine
  overlay/       Rust binary  — transparent HUD (warthunder-byoh / warthunder-byoh.exe)
  scraper/       Rust binary  — live API field harvester
tools/
  parse_fm.py    Python script — generates FM + names databases from datamine
data/
  fields.json         catalog of all known API fields
  fm/
    fm_data_db.csv    flight model parameters for 1,192 aircraft
    fm_names_db.csv   display names + types for 1,537 aircraft
    version           game version the database was built from
    version_tag       release tag (e.g. v2.55.1.88)
```

---

## Getting Started

### Installing
- Download the appropriate release .zip file from the releases section. 
- Unzip to your favorite folder.
- Run the `warthunder_byoh` executable for your platform (eg: `warthunder_byoh.exe` (Windows) or `warthunder_byoh` (Linux) or `War Thunder BYOH` (Mac)) 

### Configuration
- Edit the `indicators.json` file with the game closed. See `indicators.json.example` for some examples and `fields.json` for all possible field.s

### Build

#### Prerequisites

- [Docker](https://www.docker.com/) — used for all cross-compilation
- `make`
- War Thunder installed and running (for live use), or use fixture JSON files for development

#### Building

```bash
# Build the Windows binaries (both GDI and GPU backends)
make windows

# Build only the Windows GPU/egui binary
make windows-gpu

# Build for Linux (glibc)
make linux

# Build the Linux GPU/egui binary
make linux-gpu

# Build a fully static Linux binary (musl)
make linux-musl

# Build for macOS
make macos

# Build a macOS .app bundle
make mac-app
```

All targets produce binaries in `release/`.

### Deploy (Windows)

```bash
make windows-deploy
```

This builds the Windows binary, fetches the latest FM database, and copies everything to your configured deploy path. Copy `indicators.json` (from `indicators.json.example` as a starting point) into the same directory before running.

### Run

```bash
# Windows
warthunder-byoh.exe

# Linux / macOS
./warthunder-byoh
```

Start War Thunder first. The overlay will display a graceful offline state when the game is not running and automatically pick up data once it is.

---

## Configuration

### `indicators.json`

Overlay windows and indicators are defined in `indicators.json`. Copy `data/indicators.json.example` as a starting point. The file is a JSON array; each element defines one transparent overlay window that appears on screen.

> **Position persistence** — dragging any overlay window with the left mouse button updates its `x`/`y` in memory immediately. The changed values are written back to `indicators.json` automatically: 800 ms after the drag ends (debounce), and again on clean exit. Any JSON comments in the original file are removed on the first save (they become clean JSON). To keep the file human-friendly, edit it with the overlay closed or use the hot-reload button after editing.

#### Window object

| Field | Required | Default | Description |
|---|---|---|---|
| `id` | yes | — | Unique string identifier for the window |
| `x` | no | `0` | Horizontal position in logical (DPI-scaled) pixels from the top-left of the primary monitor |
| `y` | no | `0` | Vertical position in logical pixels |
| `width` | no | `240` | Window width in logical pixels |
| `height` | no | auto | Window height; if omitted, computed from the number of indicators |
| `indicators` | yes | — | Array of indicator definitions (rows in this window) |
| `style` | no | — | Style overrides for this window; see **Style object** below |

#### Indicator object

Each entry in `indicators` defines one row in the window.

| Field | Required | Default | Description |
|---|---|---|---|
| `id` | yes | — | Unique string identifier for this row |
| `label` | yes | — | Text shown in the left column |
| `formula` | yes | — | Expression to evaluate; see **Expressions** below |
| `unit` | no | `""` | Text shown in the right column after the value |
| `format` | no | `"integer"` | How the computed value is rendered; see **Formats** |
| `color` | no | — | Fixed colour token — bypasses all threshold logic |
| `show_when` | no | always show | Expression; row is hidden when this evaluates to `0` or `false` |
| `warn_above` | no | — | Colour → `warn` when `value > threshold` |
| `warn_below` | no | — | Colour → `warn` when `value < threshold` |
| `good_above` | no | — | Colour → `good` when `value > threshold` |
| `good_below` | no | — | Colour → `good` when `value < threshold` |
| `crit_above` | no | — | Colour → `crit` when `value > threshold` |
| `crit_below` | no | — | Colour → `crit` when `value < threshold` |
| `style` | no | — | Per-indicator colour overrides; only colour fields take effect (layout fields are ignored at indicator level) |

#### Style object

An optional `style` block can appear on a **window** (all fields honoured) or an **indicator** (colour fields only; layout fields silently ignored at indicator level).

| Field | Type | Description |
|---|---|---|
| `font_size` | number | Font size in logical points (window-level only) |
| `line_height` | number | Row height in logical pixels (window-level only) |
| `pad_x` | number | Horizontal padding inside the window, in logical pixels (window-level only) |
| `pad_y` | number | Vertical padding inside the window, in logical pixels (window-level only) |
| `col_gap` | number | Gap between the label and value columns, in logical pixels (window-level only) |
| `c_label` | color | Label column text colour |
| `c_unit` | color | Unit column text colour |
| `c_warn` | color | Colour used for the `warn` token |
| `c_crit` | color | Colour used for the `crit` token |
| `c_good` | color | Colour used for the `good` token |
| `c_info` | color | Colour used for the `info` token |
| `c_shadow` | color | Drop-shadow colour (GPU path only) |

Colors can be specified in any of these formats:

| Format | Example |
|---|---|
| Hex RGB | `"#FF8040"` |
| Hex RGBA | `"#FF8040A0"` |
| CSS rgb() | `"rgb(255, 128, 64)"` |
| CSS rgba() | `"rgba(255, 128, 64, 160)"` |
| JSON array RGB | `[255, 128, 64]` |
| JSON array RGBA | `[255, 128, 64, 160]` |

Example window with a style block:

```json
{
  "id": "engine",
  "x": 10, "y": 10,
  "style": {
    "font_size": 18,
    "c_warn": "#DCDC3C",
    "c_crit": "#DC3C3C"
  },
  "indicators": [
    { "id": "rpm", "label": "RPM", "formula": "rpm", "format": "integer" }
  ]
}
```

#### Formats

| Format | Example output | Notes |
|---|---|---|
| `"integer"` | `1234` | Default; rounds to nearest integer |
| `"decimal1"` | `12.3` | One decimal place |
| `"decimal2"` | `12.34` | Two decimal places |
| `"+decimal1"` | `+12.3` / `-12.3` | One decimal place, always prints sign |
| `"+decimal2"` | `+12.34` / `-12.34` | Two decimal places, always prints sign |
| `"time"` | `4:32` / `1:23:45` | Treats value as **seconds**; renders as M:SS or H:MM:SS. Negative or non-finite → `--:--` |

#### Colour tokens

| Token | Meaning |
|---|---|
| `"value"` | Normal / white (default when no threshold is met) |
| `"warn"` | Warning — yellow |
| `"good"` | Good / in-range — green |
| `"crit"` | Critical / danger — red |

**Priority**: when multiple threshold conditions are met simultaneously, the highest severity wins: `crit` > `warn` > `good`. Setting `color` directly overrides all thresholds.

#### Thresholds

Each threshold field can be either a **number literal** or a **formula string** that references the same variables available in `formula`. Formula thresholds are evaluated at render time, so they adapt per vehicle:

```json
{ "warn_above": 650 }
{ "warn_above": "fm_crit_ias" }
{ "crit_below": "5 * 60" }
```

#### Expressions

`formula`, `show_when`, and formula-style thresholds are evaluated by the [`evalexpr`](https://docs.rs/evalexpr) engine. Supported syntax:

- **Arithmetic**: `+`, `-`, `*`, `/`, `%`, `^` (power)
- **Comparison**: `==`, `!=`, `<`, `>`, `<=`, `>=`
- **Boolean**: `&&`, `||`, `!`
- **Math functions**: `math::abs(x)`, `math::floor(x)`, `math::ceil(x)`, `math::sqrt(x)`, `math::min(x,y)`, `math::max(x,y)`, `math::clamp(x,min,max)`, and others — see the [evalexpr docs](https://docs.rs/evalexpr) for the full list

If any variable referenced in a formula is absent from the current data frame (e.g. an FM field for an unrecognised aircraft), the indicator row is silently hidden — no error is shown.

#### Available variables

**Common API fields** — the full list is in `data/fields.json`:

| Variable | Description |
|---|---|
| `valid` | `1.0` when in-flight telemetry is live; `0.0` in menus/hangar. Use as a `show_when` guard on every indicator. |
| `ias_kmh` | Indicated airspeed (km/h) |
| `tas_kmh` | True airspeed (km/h) |
| `altitude_m` | Barometric altitude (m) |
| `vy_ms` | Vertical speed (m/s) |
| `mfuel_kg` | Current fuel mass (kg) |
| `aoa_deg` | Angle of attack (deg) |
| `gear_pct` | Gear extension 0–100 |
| `flaps_pct` | Flap extension 0–100 |
| `airbrake_pct` | Airbrake extension 0–100 |
| `indicators_g_meter` | Current G-load |
| `indicators_compass` | Compass heading (deg) |
| `indicators_aviahorizon_pitch` | Pitch (deg, API sign is inverted — use `0 - indicators_aviahorizon_pitch` for positive-up) |
| `fuel_consume` | Fuel flow as reported by the API (kg/h); present on some aircraft only |
| `thrust_1_kgs` … `thrust_4_kgs` | Per-engine thrust (kgf) |

**Derived fields** — computed by WT BYOH from raw API data:

| Variable | Description |
|---|---|
| `sep` | Specific Excess Power (m/s) — kinematic form: `Vz + (TAS/g) × dTAS/dt` via OLS regression over a 2-second sliding window |
| `sep_thrust` | Thrust-based SEP upper bound (m/s): `ΣThrust_kgf × TAS_ms / weight_kg`; requires FM empty mass and live fuel data |
| `fuel_flow` | Unified fuel flow (kg/h): native `fuel_consume` if the API reports it, otherwise an EMA-smoothed calculated rate. **Use this in preference to `fuel_consume`.** |
| `fuel_consume_calc` | EMA-smoothed calculated fuel flow (kg/h); only emitted when the native `fuel_consume` is absent |
| `crit_g_pos` | Structural positive G limit for current gross weight, using FM wing overload data |
| `fm_crit_flaps_current` | Speed limit (km/h) for the currently-extended flap detent (combat/takeoff/landing) |

**FM database fields** — injected when WT BYOH identifies the current aircraft. All absent and silently ignored for unknown aircraft:

| Variable | Description |
|---|---|
| `fm_crit_ias` | VNE — never-exceed IAS (km/h) |
| `fm_crit_mach` | Never-exceed Mach number |
| `fm_crit_gear_spd` | Max gear-extension speed (km/h) |
| `fm_crit_flaps_spd` | Max full-flap extension speed (km/h) |
| `fm_crit_flaps_combat_spd` | Max combat-flap extension speed (km/h) |
| `fm_crit_aoa_pos` | Critical positive AoA (deg) |
| `fm_crit_aoa_neg` | Critical negative AoA (deg) |
| `fm_crit_wing_overload_pos` | Max positive structural wing load (N) |
| `fm_crit_wing_overload_neg` | Max negative structural wing load (N) |
| `fm_max_fuel_kg` | Maximum fuel capacity (kg) |
| `fm_empty_mass_kg` | Empty / structural mass (kg) |
| `fm_rpm_normal` | Normal max RPM |
| `fm_rpm_max` | Emergency max RPM |
| `fm_num_engines` | Number of engines |

#### Example

```json
[
  {
    "id": "flight",
    "x": 550,
    "y": 40,
    "indicators": [
      {
        "id": "ias",
        "label": "IAS",
        "unit": "km/h",
        "formula": "ias_kmh",
        "format": "integer",
        "show_when": "valid",
        "warn_above": "fm_crit_ias * 0.9",
        "crit_above": "fm_crit_ias"
      },
      {
        "id": "fuel_time",
        "label": "FUEL",
        "formula": "(mfuel_kg / fuel_flow) * 3600",
        "format": "time",
        "show_when": "valid",
        "warn_below": "15 * 60",
        "crit_below": "5 * 60"
      },
      {
        "id": "sep",
        "label": "SEP",
        "unit": "m/s",
        "formula": "sep",
        "format": "+decimal1",
        "show_when": "valid"
      }
    ]
  }
]
```

See `data/indicators.json.example` for a more complete example. After editing, use the "Reload indicators & config" button in the Settings tab (GPU build) to apply changes without restarting. If the file contains invalid JSON or an expression error, a dialog shows the details and your running configuration is left unchanged.

---

### `config.json`

Persistent application settings are stored in `config.json`. A starter example is provided at `config.json.example` (repo root) — copy it next to `indicators.json` or into the executable directory as `config.json` to customize behaviour. Example:

```json
{
  "always_show": false,
  "show_when_byoh_foreground": false,
  "only_during_mission": false
}
```

| Key | Default | Description |
|---|---|---|
| `always_show` | `false` | Show overlay regardless of War Thunder focus or mission state |
| `show_when_byoh_foreground` | `false` | Also show overlay while the BYOH settings window has focus; useful for positioning indicators |
| `only_during_mission` | `false` | Hide overlay when `/mission.json` does not report `status: "running"`. This key is supported but the checkbox is currently hidden in the UI while mission polling is stabilised |

`config.json` is written automatically when you change a setting in the Settings tab, so you typically only need to create it manually to set `only_during_mission` until the UI exposes it again.

---

## Flight Model Database

The `data/fm/` directory contains two CSV databases generated from game datamine data:

| File | Contents | Rows |
|---|---|---|
| `fm_data_db.csv` | Performance parameters (max speed, climb rate, turn time, engine power, etc.) | 1,192 |
| `fm_names_db.csv` | Display names, FM file mappings, and aircraft type classifications | 1,537 |

### Automatic Updates

On startup the overlay checks the [warthunder-byo-fm](https://github.com/SpaceCapo/warthunder-byo-fm) GitHub releases for a newer FM database. If one is available it downloads and extracts it into the `fm/` directory next to the executable — no user action required. The current local version is shown in the settings window (GPU build) or printed to the console on startup.

If the check fails (no internet, GitHub down, etc.) the overlay continues normally with whatever FM data is already on disk.

To force a manual update outside the overlay:

```bash
make fetch-fm
```

### Regenerating from Datamine

To rebuild from a fresh game datamine instead:

```bash
python3 tools/parse_fm.py \
  --fm-dir    /path/to/datamine/aces.vromfs.bin_u/gamedata/flightmodels/fm \
  --datamine-dir /path/to/datamine/output \
  --out       data/fm/fm_data_db.csv \
  --names-out data/fm/fm_names_db.csv
```

Use `--verify data/fm/fm_data_db.csv` to diff output against the existing database and review changes before committing.

---

## Development

### Running Tests

```bash
cargo test
```

The test suite uses inline synchronous fixtures — no running game required. All tests must pass before merging.

### Updating the Field Catalog

Run the scraper while War Thunder is running:

```bash
make build-scraper
./release/linux/scraper
```

This hits all known local API endpoints, collects field names, and merges results into `data/fields.json`.

### Direct Docker Invocation

For one-off Cargo commands inside the build environment:

```bash
docker run --rm -v "$(pwd)":/work -w /work \
  -v cargo-registry:/cargo-reg -e CARGO_HOME=/cargo-reg \
  -v wt-target:/work/target \
  wt-builder:latest bash -lc "cargo test"
```

---

## Anti-Cheat & Legal Notes

WT BYOH is designed to be safe to use alongside War Thunder's anti-cheat system:

- **No process injection** — the overlay is an independent process with no hooks into the game
- **No memory access** — all data comes from the game's own `localhost` HTTP API
- **No kernel drivers** — no low-level system hooks of any kind
- **Local-only** — no data is sent to any external server

The War Thunder local API (`localhost:8111`) is a deliberate, documented feature provided by Gaijin Entertainment. This tool reads only from that API.

That said, always review the current [War Thunder EULA](https://warthunder.com/en/user_agreement/) before using third-party tools in online play. Use at your own risk.

---

## Contributing

1. Read the [War Thunder local API docs](https://github.com/lucasvmx/WarThunder-localhost-documentation) — these are the authoritative reference for available endpoints and field shapes.
2. Check [WTRTI](https://mesofthorny.github.io/WTRTI/) for feature reference and UX ideas.
3. Fork, branch, and open a PR. Keep changes small and well-tested.
4. Run `cargo test` before submitting — all tests must pass.

See `AGENTS.md` for architecture details, build system notes, and agent/contributor guidelines.

---

## License

Copyright (C) 2025 SpaceCapo. Licensed under the [GNU Affero General Public License v3.0](LICENSE).

This project is not affiliated with or endorsed by Gaijin Entertainment. War Thunder is a trademark of Gaijin Entertainment.
