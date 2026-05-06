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

Overlay windows and indicators are defined in `indicators.json`. Copy `indicators.json.example` as a starting point:

```json
[
  {
    "id": "flight",
    "x": 100,
    "y": 100,
    "indicators": [
      { "id": "altitude", "label": "ALT", "expr": "altitude", "unit": "m" },
      { "id": "speed",    "label": "IAS", "expr": "IAS",      "unit": "km/h",
        "warn": 650, "good": 0 }
    ]
  }
]
```

Available fields are listed in `data/fields.json`. Use `expr` to combine or transform values with standard arithmetic expressions.

Persistent application settings are stored in `config.json`. A starter example is provided at `config.json.example` (repo root) — copy it next to `indicators.json` or into the executable directory as `config.json` to customize behaviour. Example:

```json
{
  "always_show": false,
  "show_when_byoh_foreground": false,
  "only_during_mission": false
}
```

Meaning:
- `always_show`: overlay remains visible regardless of War Thunder focus/mission state.
- `show_when_byoh_foreground`: also show overlay while the BYOH settings window has focus (useful for positioning).
- `only_during_mission`: hide the overlay when `/mission.json` does not report `status: "running"` (the key is supported but the checkbox is currently hidden in the UI while mission polling is stabilised).

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
