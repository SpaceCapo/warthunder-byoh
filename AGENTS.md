AGENTS GUIDE

**Standing Rules for Agents**

- **Do not change the poller interval** (currently 16 ms) without explicit user approval. Ask first.
- Make atomic changes; run `cargo test` after each change — all tests must pass before continuing.
- Prefer small, well-tested patches over large sweeping rewrites.

**Project Summary**

We are building a replacement for the third-party tool "WTRTI" for War Thunder. The tool's primary user experience is an in-game overlay that displays player statistics, session data, and contextual tooling on top of the running game. The overlay should be lightweight, local-only by default, and safe with respect to game anti-cheat/EULA constraints.

**External References**

- War Thunder local API documentation (authoritative source for endpoints): https://github.com/lucasvmx/WarThunder-localhost-documentation?tab=readme-ov-file
- The original WTRTI tool (feature reference / UI ideas): https://mesofthorny.github.io/WTRTI/

Read those two links first. The lucasvmx repo is the authoritative doc for what the game exposes locally; the WTRTI website is useful for feature coverage and UX reference.

---

**What Has Been Built**

The project is a Rust workspace (`Cargo.toml`) with three crates plus Python tooling:

```
crates/
  core_client/   <- Rust library: War Thunder local API client + data-driven display engine
  overlay/       <- Rust binary: transparent always-on-top HUD (warthunder-byoh.exe / warthunder-byoh)
  scraper/       <- Rust binary: live API field harvester (writes data/fields.json)
tools/
  parse_fm.py    <- Python script: parses game FM data -> fm_data_db.csv + fm_names_db.csv
data/
  fields.json              <- scraped catalog of all known API fields
  fm/
    fm_data_db.csv         <- flight model data (1,192 aircraft)
    fm_names_db.csv        <- aircraft display names + types (1,537 aircraft)
    version                <- game version the data was built from
    version_tag            <- release tag (e.g. v2.55.1.88)
```

**core_client** (`crates/core_client/`)

- Spawns one background thread per HTTP endpoint (`/state`, `/indicators`, etc.).
- Each thread polls the game's local API at whatever rate the server allows (~6 Hz observed) and writes the latest JSON into a shared `Arc<RwLock<EndpointCache>>`.
- `fetch_display_windows()` — called by the overlay at 60 Hz — reads the cache (no HTTP on the hot path), evaluates user-configured indicator expressions via `evalexpr`, and returns a `Vec<WindowRows>` for rendering.
- The test path (`Client::with_config` / `Client::with_windows`) skips background threads and does an inline synchronous fetch so unit tests are fully deterministic.
- Colour tokens (`"value"`, `"warn"`, `"good"`, `"info"`, `"unit"`) decouple threshold logic from platform rendering.

**overlay** (`crates/overlay/`)

- Binary: `warthunder-byoh` (Windows: `warthunder-byoh.exe`).
- Cross-platform transparent always-on-top window built on `winit`.
- Two render backends, selected at compile time via Cargo features:
  - `gpu` — wgpu + egui + glyphon (full GPU path; default for release builds).
  - GDI fallback — Win32 GDI text rendering (lightweight, no GPU required).
- Reads `indicators.json` at startup to know which windows and fields to display.
- **Position persistence** — `WindowEvent::Moved` updates `window_defs[idx].x/y` in memory; positions are written back to `indicators.json` debounced (800 ms after drag ends) and on every clean exit path. Physical pixel coordinates from winit are converted to logical (DPI-scaled) before saving.
- Poller runs at 16 ms (≈60 Hz) reading from the core_client cache.
- Supports configurable position, opacity, refresh interval, and a lock/toggle hotkey.
- Runs as a separate process — no game process injection.

**scraper** (`crates/scraper/`)

- Hits live War Thunder local API endpoints, collects every JSON key seen, and writes/merges `data/fields.json`.
- Used to keep the field catalog up to date when Gaijin changes the API.

**parse_fm.py** (`tools/parse_fm.py`)

Generates two reference CSVs from a datamine of the game files:

- `fm_data_db.csv` — flight model parameters (max speed, climb rate, turn time, etc.) parsed from FM JSON files.
- `fm_names_db.csv` — aircraft display names, FM file mappings, and type classifications (`fighter` / `bomber` / `strike` / `helicopter`).

Key flags:
```
--fm-dir PATH         Directory of FM JSON files (game datamine)
--datamine-dir PATH   Datamine output root (enables fm_names_db.csv generation)
--out PATH            Output path for fm_data_db.csv
--names-out PATH      Output path for fm_names_db.csv
--verify CSV          Diff output against an existing CSV and print summary
--aircraft NAME       Process a single aircraft (debugging)
```

Datamine sources (all under the datamine output root):
```
aces.vromfs.bin_u/gamedata/flightmodels/*.json    <- unit files (Name, FmName)
aces.vromfs.bin_u/gamedata/flightmodels/fm/*.json <- FM parameter files
char.vromfs.bin_u/config/unittags.json            <- type/country tags
lang.vromfs.bin_u/lang/units.csv                  <- English display names
```

---

**Build & Deploy**

Docker-based cross-compilation; `make` targets:

| Target | Description |
|---|---|
| `make windows` | Windows x86_64 binaries (both GDI and GPU backends) |
| `make windows-gpu` | Windows x86_64 binary (GPU/egui backend only) |
| `make linux` | Linux glibc binary |
| `make linux-gpu` | Linux GPU/egui binary |
| `make linux-musl` | Linux static musl binary |
| `make macos` | macOS binary |
| `make mac-app` | macOS .app bundle |
| `make windows-deploy` | Build + copy to `./deploy` — overridden by `WT_BYOH_DEPLOY_DIR` environment variable |
| `make fetch-fm` | Pull latest FM database into `data/fm/` |

Docker image: `wt-builder:latest` (built from `docker/ubuntu24/`).

Direct Docker invocation pattern (when running a single command):
```bash
docker run --rm -v "$(pwd)":/work -w /work \
  -v cargo-registry:/cargo-reg -e CARGO_HOME=/cargo-reg \
  -v wt-target:/work/target \
  wt-builder:latest bash -lc "<cargo command>"
```

**Deployment notes:**
- Copy `indicators.json` (not `indicators.json.example`) into the deploy directory.
- FM data files (`data/fm/`) are copied automatically by `windows-deploy`.

---

**Goals**

- Recreate the core useful features of WTRTI with better maintainability and a clean architecture, optimized for an in-game overlay experience.
- Prioritise security and privacy: by default data should remain local and never be sent to remote servers without explicit consent.
- Provide a modular codebase so pieces (core API client, CLI, and overlay UI) can be developed independently and composed.
- Ship a small, test-covered core that can be extended later.

**Non-Goals (initially)**

- Building a public cloud service that collects user data.
- Implementing every WTRTI feature at once — focus on a minimal viable replacement and iterative additions.

**Optional / Later Features**

- Advanced analytics and visualizations (graphs of progression, detailed match analytics).
- Import/export integrations with third-party services (only after opt-in and clear privacy controls).
- Additional overlay-related features: compact HUD modes, per-vehicle quick-stats, and contextual action shortcuts.

---

**Architecture Guidance**

- Keep the system componentized and overlay-aware:
  - Core API client module that calls the local War Thunder endpoints and returns typed/normalized data.
  - CLI tooling for scripted workflows and export that uses the core API client.
  - An overlay UI component responsible for rendering an in-game HUD. The overlay should be a separate process (not injected into the game) and call the core API client directly to query the local War Thunder API.
  - A settings UI (for configuration and debugging) should be embedded in the overlay or provided as a native settings window inside the same application. Do not expose a network server for configuration.
- The War Thunder local API is documented in the lucasvmx repo above. The design should rely only on these HTTP endpoints rather than memory hacking or other fragile approaches.
- Be defensive: local endpoints may change or be unavailable if the game is not running. The overlay should detect offline conditions and either show cached data or a graceful offline indicator.
- Respect the game's rate limits and avoid aggressive polling. Provide configurable refresh intervals and client-side caching.
- Rendering should be GPU-friendly and performant. Minimize work on the main UI thread and keep frame-rate independent updates for the HUD.

**Security & Privacy**

- Default to local-only operation. Any feature that transmits data off the user's machine must be opt-in and explicit.
- Store any persisted data minimally and encrypted when it contains sensitive tokens or session identifiers.
- The tool must query the game's local HTTP API directly and should not start its own long-running network server.
- Avoid any in-process injection or memory reads/writes into the game process.
- Avoid using kernel drivers or system hooks that could trigger anti-cheat heuristics.
- Provide an option to run the overlay on a second monitor or in a detached window to reduce risk for users concerned about anti-cheat.

**Legal & Compliance**

- Prefer using only the documented local API to reduce legal risk.
- Avoid any behavior that modifies the game process or hooks input.
- Prefer rendering overlays as independent windows with transparency and click-through controls.

---

**Development Workflow**

- Branching: short-lived feature branches, open PRs for non-trivial changes.
- Tests: require unit tests for the API client and regression tests for parsing logic.
- CI: run lint, tests, and a small integration test suite that uses recorded API responses.

**How to Contribute / Onboarding**

1. Read the two external links (API docs and WTRTI site) to understand expected endpoints and features.
2. Start a local dev environment: run the game (or a recorded fixture) and confirm you can reach the local API endpoints documented in the lucasvmx repo.
3. Run `cargo test` to verify the existing test suite passes.
4. To iterate on the overlay: `make windows-deploy` builds and deploys to the Windows path.
5. To update FM data: run `parse_fm.py` with `--fm-dir` pointing to the real game datamine.

**Notes for Agents**

- When you begin a task, update this file with your assumptions and what you tested.
- Use the lucasvmx repo as the source of truth for endpoint names and request/response shapes. If you find discrepancies, document them and capture sample responses as fixtures.
- Prioritize small, well-tested patches. Avoid large sweeping changes without tests or documented rollout steps.
- When making changes to `parse_fm.py`, verify with `--verify data/fm/fm_data_db.csv` and check `--datamine-dir` output against `data/fm/fm_names_db.csv`.
