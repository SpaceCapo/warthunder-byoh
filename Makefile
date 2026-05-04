SHELL := /usr/bin/env bash
PROJECT     := overlay
BIN         := warthunder-byoh
RELEASE_DIR := release
MACOS_SDK_DEFAULT=/tmp/MacOSX26.1.sdk.tar.xz

ICON_SRC   := crates/overlay/assets/icon_source.png
ICON_ICO   := crates/overlay/assets/icon.ico
ICON_ICNS  := crates/overlay/assets/app.icns
ICON_DIR   := crates/overlay/assets/icons

# Named Docker volumes for build caches.
# cargo-registry caches downloaded crate sources; wt-target caches compiled artifacts.
# Both persist across `docker run --rm` invocations so incremental builds are fast.
CARGO_REG_VOL  := cargo-registry
TARGET_VOL     := wt-target
DOCKER_CACHE   := -v $(CARGO_REG_VOL):/cargo-reg -e CARGO_HOME=/cargo-reg -v $(TARGET_VOL):/work/target

.PHONY: all linux linux-gpu linux-musl linux-pkg windows windows-deploy windows-gpu gpu-deploy macos mac-app icons build-scraper clean fetch-fm

all: linux linux-musl windows macos

# ── Icon generation ────────────────────────────────────────────────────────────
# Requires: ImageMagick (convert) and icnsutils (png2icns) on the host.
# Run once (or whenever icon_source.png changes) to regenerate platform assets.
icons:
	@echo "Generating icon assets from $(ICON_SRC)..."
	@mkdir -p $(ICON_DIR)
	@# Crop landscape source to a centred square, then resize to each size.
	@convert "$(ICON_SRC)" -gravity Center -extent 768x768 /tmp/icon_square.png
	@for SIZE in 16 32 48 128 256 512; do \
	  convert /tmp/icon_square.png -resize $${SIZE}x$${SIZE} $(ICON_DIR)/$${SIZE}.png; \
	  echo "  $(ICON_DIR)/$${SIZE}.png"; \
	done
	@# Windows .ico (multi-size)
	@convert $(ICON_DIR)/16.png $(ICON_DIR)/32.png $(ICON_DIR)/48.png $(ICON_DIR)/256.png \
	         $(ICON_ICO)
	@echo "  $(ICON_ICO)"
	@# macOS .icns
	@png2icns $(ICON_ICNS) \
	  $(ICON_DIR)/16.png $(ICON_DIR)/32.png $(ICON_DIR)/128.png \
	  $(ICON_DIR)/256.png $(ICON_DIR)/512.png
	@echo "  $(ICON_ICNS)"
	@rm -f /tmp/icon_square.png
	@echo "Done."

# ── Linux builds ──────────────────────────────────────────────────────────────
linux-gpu:
	@echo "Building Linux GPU overlay (glibc) using builder image..."
	docker build -t wt-builder:latest docker/ubuntu24 || true
	docker run --rm -v "$(PWD)":/work -w /work $(DOCKER_CACHE) wt-builder:latest bash -lc \
	  "cargo build --features render,gpu --release -p $(PROJECT) \
	   && mkdir -p $(RELEASE_DIR)/linux-gpu \
	   && cp target/release/$(BIN) $(RELEASE_DIR)/linux-gpu/$(BIN)"

linux-glibc:
	@echo "Building Linux (glibc) target using builder image..."
	docker build -t wt-builder:latest docker/ubuntu24 || true
	docker run --rm -v "$(PWD)":/work -w /work $(DOCKER_CACHE) wt-builder:latest bash -lc \
	  "cargo build --features render --release -p $(PROJECT) \
	   && mkdir -p $(RELEASE_DIR)/linux \
	   && cp target/release/$(BIN) $(RELEASE_DIR)/linux/$(BIN)-glibc"

linux-musl:	
	@echo "Building Linux (musl static) target using builder image..."
	docker build -t wt-builder:latest docker/ubuntu24 || true
	docker run --rm -v "$(PWD)":/work -w /work $(DOCKER_CACHE) wt-builder:latest bash -lc \
	  "cargo build --features render --release --target x86_64-unknown-linux-musl -p $(PROJECT) \
	   && mkdir -p $(RELEASE_DIR)/linux \
	   && cp target/x86_64-unknown-linux-musl/release/$(BIN) $(RELEASE_DIR)/linux/$(BIN)-musl"

linux-all-pkg: linux-pkg-dir linux-glibc linux-musl linux-gpu
# Package the Linux binaries with data files into a self-contained directory.
# The binary resolves data files relative to its own location, so the layout is:
#   release/linux-gpu/
#     warthunder-byoh
#	  warthunder-byoh-glibc (optional, if linux-glibc was built)
#     warthunder-byoh-musl (optional, if linux-musl was built)
#     indicators.json
#     indicators.json.example
#     fields.json
#     fm/  (FM database CSVs)
	@echo "Packaging Linux GPU bundle..."
	@PKG="$(RELEASE_DIR)/linux"; \
	mkdir -p "$$PKG/fm"; \
	cp "$(RELEASE_DIR)/linux-gpu/$(BIN)" "$$PKG/$(BIN)"; \
	cp "$(RELEASE_DIR)/linux/$(BIN)-glibc" "$$PKG/$(BIN)-glibc"; \
	cp "$(RELEASE_DIR)/linux/$(BIN)-musl" "$$PKG/$(BIN)-musl"; \
	cp "data/fields.json"     "$$PKG/fields.json"; \
	cp "data/indicators.json.example" "$$PKG/indicators.json.example"; \
	cp -r "data/fm/."         "$$PKG/fm/"; \
	echo "  $$PKG"
	@echo "Linux GPU bundle ready."

linux-pkg-dir:
	mkdir -p $(RELEASE_DIR)/linux

# ── Windows builds ────────────────────────────────────────────────────────────
# Builds both executables in a single Docker run:
#   warthunder-byoh.exe     — GPU overlay (wgpu + glyphon, DirectX/wgpu backend)
#   warthunder-byoh-gdi.exe — GDI overlay (Win32 GDI layered window, no GPU required)
windows-gdi:
	@echo "Building Windows binaries (x86_64-pc-windows-gnu) using wt-builder image..."
	docker build -t wt-builder:latest docker/ubuntu24 || true
	docker run --rm -v "$(PWD)":/work -w /work $(DOCKER_CACHE) wt-builder:latest bash -lc \
	  "mkdir -p $(RELEASE_DIR)/windows-gdi \
	   && cargo build --features render,windows-glue --release --target x86_64-pc-windows-gnu -p $(PROJECT) \
	   && cp target/x86_64-pc-windows-gnu/release/$(BIN).exe $(RELEASE_DIR)/windows-gdi/$(BIN)-gdi.exe"

windows-gpu:
	@echo "Building Windows GPU overlay (x86_64-pc-windows-gnu) using wt-builder image..."
	docker build -t wt-builder:latest docker/ubuntu24 || true
	docker run --rm -v "$(PWD)":/work -w /work $(DOCKER_CACHE) wt-builder:latest bash -lc \
	  "cargo build --features render,gpu,windows-glue --release --target x86_64-pc-windows-gnu -p $(PROJECT) \
	   && mkdir -p $(RELEASE_DIR)/windows-gpu \
	   && cp target/x86_64-pc-windows-gnu/release/$(BIN).exe $(RELEASE_DIR)/windows-gpu/$(BIN).exe"

windows-pkg: windows-pkg-dir windows-gdi windows-gpu
	@echo "Packaging Windows bundle..."
	@PKG="$(RELEASE_DIR)/windows"; \
	mkdir -p "$$PKG/fm"; \
	cp "$(RELEASE_DIR)/windows-gpu/$(BIN).exe" "$$PKG/$(BIN).exe"; \
	cp "$(RELEASE_DIR)/windows-gdi/$(BIN)-gdi.exe" "$$PKG/$(BIN)-gdi.exe"; \
	cp "data/fields.json" "$$PKG/fields.json"; \
	cp "data/indicators.json.example" "$$PKG/indicators.json.example"; \
	cp -r "data/fm/." "$$PKG/fm/"; \
	echo "  $$PKG"
	@echo "Windows bundle ready."

windows-pkg-dir:
	mkdir -p $(RELEASE_DIR)/windows

# ── FM data ───────────────────────────────────────────────────────────────────
FM_DIR := data/fm
FM_URL := https://github.com/SpaceCapo/warthunder-byo-fm/releases/latest/download/warthunder-byo-fm.zip

fetch-fm:
	@echo "Fetching FM database files into $(FM_DIR)..."
	@mkdir -p $(FM_DIR)
	curl -fsSL "$(FM_URL)" -o /tmp/fm.zip
	@echo "Extracting FM database files from downloaded archive into $(FM_DIR)..."
	unzip -o /tmp/fm.zip -d $(FM_DIR) > /dev/null
	@echo "FM data fetched."

# ── Windows deploy ────────────────────────────────────────────────────────────
# Deploy directory: use WT_BYOH_DEPLOY_DIR env var if set/non-empty, otherwise default to ./deploy
DEPLOY_DIR := $(if $(strip $(WT_BYOH_DEPLOY_DIR)),$(strip $(WT_BYOH_DEPLOY_DIR)),./deploy)

windows-deploy: windows-pkg fetch-fm
	@echo "Deploying Windows build to $(DEPLOY_DIR)..."
	@mkdir -p $(DEPLOY_DIR)/fm/fm
	cp -v  $(RELEASE_DIR)/windows/$(BIN).exe     $(DEPLOY_DIR)/$(BIN).exe
	cp -v  $(RELEASE_DIR)/windows/$(BIN)-gdi.exe $(DEPLOY_DIR)/$(BIN)-gdi.exe
	cp -v  ./data/fields.json 					 $(DEPLOY_DIR)/
	cp -v  ./data/indicators.json.example		 $(DEPLOY_DIR)/
	cp -vr $(FM_DIR)                     		 $(DEPLOY_DIR)/
# 	cp -v $(FM_DIR)/fm/fm_names_db.csv  $(DEPLOY_DIR)/fm/fm/
# 	cp -v $(FM_DIR)/fm/fm_data_db.csv   $(DEPLOY_DIR)/fm/fm/
# 	cp -v $(FM_DIR)/fm/fm_version       $(DEPLOY_DIR)/fm/fm/
# 	cp -v data/fm/current_version    $(DEPLOY_DIR)/fm/

gpu-deploy: windows-deploy

# ── macOS build + .app bundle ─────────────────────────────────────────────────
MACOS_TARGET_VOL := wt-macos-target

# macOS cross-build uses a dedicated builder image that layers the osxcross
# toolchain (from crazymax/osxcross:latest-ubuntu) on top of Ubuntu + Rust.
# No external SDK tarball is required — it is baked into the crazymax image.
# After the Docker build the release dir is chowned back to the calling user
# so subsequent host-side operations (mac-app, cp) work without sudo.
macos:
	@echo "Building macOS builder image..."
	docker build -t wt-macos-builder:latest docker/macos
	@echo "Building macOS (x86_64-apple-darwin) using wt-macos-builder image..."
	docker run --rm -v "$(PWD)":/work -w /work \
	  -v $(CARGO_REG_VOL):/cargo-reg -e CARGO_HOME=/cargo-reg \
	  -v $(MACOS_TARGET_VOL):/work/target \
	  wt-macos-builder:latest bash -lc \
	  "cargo build --features render,gpu --release --target x86_64-apple-darwin -p $(PROJECT) \
	   && mkdir -p $(RELEASE_DIR)/macos \
	   && cp target/x86_64-apple-darwin/release/$(BIN) $(RELEASE_DIR)/macos/$(BIN) \
	   && chown -R $$(id -u):$$(id -g) $(RELEASE_DIR)/macos 2>/dev/null || true"
	@# Fall back to host chown if the in-container chown didn't cover it.
	@chown -R "$(shell id -u):$(shell id -g)" "$(RELEASE_DIR)/macos" 2>/dev/null || true

# Bundle the macOS binary into a .app with icon and Info.plist.
# Run after `make macos` (or `make mac-app` which calls macos first).
mac-app: mac-pkg-dir macos
	@echo "Packaging macOS .app bundle..."
	@APP="$(RELEASE_DIR)/macos/War Thunder BYOH.app"; \
	mkdir -p "$$APP/Contents/MacOS/fm" "$$APP/Contents/Resources"; \
	cp "$(RELEASE_DIR)/macos/$(BIN)" "$$APP/Contents/MacOS/$(BIN)"; \
	cp "$(ICON_ICNS)" "$$APP/Contents/Resources/app.icns"; \
	cp "packaging/Info.plist" "$$APP/Contents/Info.plist"; \
	cp "data/fields.json" "$$APP/Contents/MacOS/fields.json"; \
	cp "data/indicators.json.example" "$$APP/Contents/MacOS/indicators.json.example"; \
	cp -r "data/fm/." "$$APP/Contents/MacOS/fm/"; \
	echo "  $$APP"
	@echo "macOS .app bundle ready. Removing temporary binary from $(RELEASE_DIR)/macos..."
	rm "$(RELEASE_DIR)/macos/$(BIN)"

mac-pkg-dir:
	mkdir -p $(RELEASE_DIR)/macos

# ── Scraper ───────────────────────────────────────────────────────────────────
build-scraper:
	@echo "Building scraper (Linux) using builder image..."
	docker build -t wt-builder:latest docker/ubuntu24 || true
	docker run --rm -v "$(PWD)":/work -w /work $(DOCKER_CACHE) wt-builder:latest bash -lc \
	  "cargo build --release -p scraper \
	   && mkdir -p $(RELEASE_DIR)/linux \
	   && cp target/release/scraper $(RELEASE_DIR)/linux/scraper"

# ── Clean ─────────────────────────────────────────────────────────────────────
clean:
	rm -rf $(RELEASE_DIR)
	rm -f dockcross

package-all-dirs: linux-pkg-dir windows-pkg-dir mac-pkg-dir
	@echo "Creating empty package directories for all platforms..."
	
# Package All
package: package-all-dirs linux-all-pkg windows-pkg mac-app
	@echo "All platforms packaged."
