//! GPU-accelerated overlay renderer using wgpu + glyphon.
//!
//! # Two rendering modes
//!
//! ## Offscreen mode (Windows)
//! DXGI swap chains on Windows only support `CompositeAlphaMode::Opaque`, so
//! we cannot composite a transparent wgpu surface directly.  Instead we render
//! to an offscreen `wgpu::Texture`, copy it to a CPU staging `wgpu::Buffer`,
//! and hand the BGRA pixels to `UpdateLayeredWindow` — the same WIN32 call
//! used by the GDI path, giving true per-pixel alpha with GPU-rendered text.
//!
//! ## Surface mode (Linux / macOS)
//! On Linux (Wayland / X11 with a compositing WM) and macOS the wgpu surface
//! supports `PreMultiplied` or `PostMultiplied` alpha and we present directly.
//!
//! # Layout
//!
//! ```text
//! | PAD_X | LABEL_COL_W | VALUE_COL_W (right-align) | COL_GAP | UNIT … |
//! ```

use core_client::DisplayRow;
use glyphon::{
    Attrs, Buffer, Cache, Color, Family, FontSystem, Metrics, Resolution, Shaping, SwashCache,
    TextArea, TextAtlas, TextBounds, TextRenderer, Viewport,
};
use std::time::Instant;

// ─── ResolvedStyle ────────────────────────────────────────────────────────────

/// Fully-resolved render style (no Options) for one overlay window.
///
/// Constructed from `defaults()`, then optionally refined by a window-level
/// `RenderStyle` via `apply()`, and further refined per-row by `apply_colors()`.
#[derive(Clone)]
pub struct ResolvedStyle {
    pub font_size:   f32,
    pub line_height: f32,
    pub pad_x:       f32,
    pub pad_y:       f32,
    pub col_gap:     f32,
    pub c_label:  Color,
    pub c_value:  Color,
    pub c_unit:   Color,
    pub c_warn:   Color,
    pub c_crit:   Color,
    pub c_good:   Color,
    pub c_info:   Color,
    pub c_shadow: Color,
}

impl ResolvedStyle {
    pub fn defaults() -> Self {
        Self {
            font_size:   32.0,
            line_height: 40.0,
            pad_x:        8.0,
            pad_y:       10.0,
            col_gap:     12.0,
            c_label:  Color::rgb(255, 255, 255),
            c_value:  Color::rgb(255, 255, 255),
            c_unit:   Color::rgb(192, 192, 192),
            c_warn:   Color::rgb(220,  60,  60),
            c_crit:   Color::rgb(220,  60,  60),
            c_good:   Color::rgb( 80, 200, 120),
            c_info:   Color::rgb( 80, 160, 220),
            c_shadow: Color::rgba(0, 0, 0, 160),
        }
    }

    /// Apply all overrides from `style` (layout + colours).
    pub fn apply(&self, style: &core_client::RenderStyle) -> Self {
        let mut r = self.clone();
        if let Some(v) = style.font_size   { r.font_size   = v; }
        if let Some(v) = style.line_height { r.line_height = v; }
        if let Some(v) = style.pad_x       { r.pad_x       = v; }
        if let Some(v) = style.pad_y       { r.pad_y       = v; }
        if let Some(v) = style.col_gap     { r.col_gap      = v; }
        r.apply_colors(style)
    }

    /// Apply only colour overrides from `style` (used at per-indicator level).
    pub fn apply_colors(&self, style: &core_client::RenderStyle) -> Self {
        let mut r = self.clone();
        if let Some(c) = style.c_label  { r.c_label  = oc(c); }
        if let Some(c) = style.c_value  { r.c_value  = oc(c); }
        if let Some(c) = style.c_unit   { r.c_unit   = oc(c); }
        if let Some(c) = style.c_warn   { r.c_warn   = oc(c); }
        if let Some(c) = style.c_crit   { r.c_crit   = oc(c); }
        if let Some(c) = style.c_good   { r.c_good   = oc(c); }
        if let Some(c) = style.c_info   { r.c_info   = oc(c); }
        if let Some(c) = style.c_shadow { r.c_shadow = oc(c); }
        r
    }

    /// Map a color token to the appropriate resolved color.
    pub fn token_to_color(&self, token: &str) -> Color {
        match token {
            "value" => self.c_value,
            "warn"  => self.c_warn,
            "crit"  => self.c_crit,
            "good"  => self.c_good,
            "info"  => self.c_info,
            _       => self.c_label,
        }
    }
}

/// Convert an `OverlayColor` to a glyphon `Color`.
fn oc(c: core_client::OverlayColor) -> Color {
    let [r, g, b, a] = c.to_rgba();
    Color::rgba(r, g, b, a)
}

/// Returns `true` during the "on" half of a 2 Hz blink cycle.
pub fn blink_on() -> bool {
    use std::sync::OnceLock;
    static START: OnceLock<Instant> = OnceLock::new();
    let start = START.get_or_init(Instant::now);
    (start.elapsed().as_millis() / 250) % 2 == 0
}

// ─── Text buffer builder ──────────────────────────────────────────────────────

/// Layout descriptor for one piece of text.
struct TextEntry {
    buf: Buffer,
    x: f32,
    y: f32,
    color: Color,
}

/// Cached output of `make_text_entries`.
///
/// Keyed on the logical content of the rows + blink state + styles.  As long as
/// the key matches the previous frame we skip all font shaping and reuse the
/// existing `Buffer` objects directly.
pub struct TextCache {
    /// Window-level style key.
    win_style_key: Option<core_client::RenderStyle>,
    /// Snapshot key: `(label, value_str, unit, color, row_style)` per row.
    row_key:   Vec<(String, String, String, String, Option<core_client::RenderStyle>)>,
    blink_key: bool,
    entries:   Vec<TextEntry>,
    pub cached_w: u32,
    pub cached_h: u32,
}

impl TextCache {
    pub fn new() -> Self {
        Self {
            win_style_key: None,
            row_key: Vec::new(),
            blink_key: false,
            entries: Vec::new(),
            cached_w: 1,
            cached_h: 1,
        }
    }

    /// Return `true` if the cache is still valid for `rows` + `blink` + `win_style`.
    fn is_valid(
        &self,
        rows: &[DisplayRow],
        blink: bool,
        win_style: Option<&core_client::RenderStyle>,
    ) -> bool {
        blink == self.blink_key
            && win_style == self.win_style_key.as_ref()
            && rows.len() == self.row_key.len()
            && rows.iter().zip(&self.row_key).all(|(r, k)| {
                r.label == k.0
                    && r.value_str == k.1
                    && r.unit == k.2
                    && r.color == k.3
                    && r.style.as_ref() == k.4.as_ref()
            })
    }

    /// Rebuild the cache from `rows`.  Called only when `is_valid` returns false.
    pub fn rebuild(
        &mut self,
        font_system: &mut FontSystem,
        rows: &[DisplayRow],
        blink: bool,
        win_style: Option<&core_client::RenderStyle>,
    ) {
        self.win_style_key = win_style.cloned();
        self.row_key = rows.iter()
            .map(|r| (
                r.label.clone(),
                r.value_str.clone(),
                r.unit.clone(),
                r.color.clone(),
                r.style.clone(),
            ))
            .collect();
        self.blink_key = blink;

        let (entries, w, h) = make_text_entries(font_system, rows, blink, win_style);
        self.entries  = entries;
        self.cached_w = w;
        self.cached_h = h;
    }

    /// Return the cached entries (and their dimensions), rebuilding if necessary.
    pub fn get_or_rebuild<'a>(
        &'a mut self,
        font_system: &mut FontSystem,
        rows: &[DisplayRow],
        blink: bool,
        win_style: Option<&core_client::RenderStyle>,
    ) -> (&'a [TextEntry], u32, u32) {
        if !self.is_valid(rows, blink, win_style) {
            self.rebuild(font_system, rows, blink, win_style);
        }
        (&self.entries, self.cached_w, self.cached_h)
    }
}

/// Build one `TextEntry` per visible text piece for the given display rows.
/// Returns `(entries, needed_w, needed_h)` — the content-driven window size.
fn make_text_entries(
    font_system: &mut FontSystem,
    rows: &[DisplayRow],
    blink: bool,
    win_style: Option<&core_client::RenderStyle>,
) -> (Vec<TextEntry>, u32, u32) {
    // Resolve window-level style; per-row styles only override colours.
    let win_rs = match win_style {
        Some(s) => ResolvedStyle::defaults().apply(s),
        None    => ResolvedStyle::defaults(),
    };

    let metrics = Metrics::new(win_rs.font_size, win_rs.line_height);

    // Helper: measure rendered width of a string.
    let measure = |font_system: &mut FontSystem, text: &str| -> f32 {
        let mut buf = Buffer::new(font_system, metrics);
        buf.set_size(font_system, None, Some(win_rs.line_height));
        buf.set_text(
            font_system, text,
            Attrs::new().family(Family::Monospace),
            Shaping::Basic,
        );
        buf.shape_until_scroll(font_system, false);
        buf.layout_runs().next().map(|r| r.line_w).unwrap_or(0.0)
    };

    // Measure max label, value, and unit widths to set column positions dynamically.
    let label_col_w = rows.iter()
        .filter(|r| !r.label.is_empty())
        .map(|r| measure(font_system, &r.label))
        .fold(0.0_f32, f32::max);

    let value_col_w = rows.iter()
        .filter(|r| !r.value_str.is_empty())
        .map(|r| measure(font_system, &r.value_str))
        .fold(0.0_f32, f32::max);

    let unit_col_w = rows.iter()
        .filter(|r| !r.unit.is_empty())
        .map(|r| measure(font_system, &r.unit))
        .fold(0.0_f32, f32::max);
    let has_units = rows.iter().any(|r| !r.unit.is_empty());

    let mut entries: Vec<TextEntry> = Vec::new();

    macro_rules! push {
        ($text:expr, $x:expr, $y:expr, $color:expr) => {{
            let mut buf = Buffer::new(font_system, metrics);
            buf.set_size(font_system, None, Some(win_rs.line_height));
            buf.set_text(
                font_system,
                $text,
                Attrs::new().family(Family::Monospace).color($color),
                Shaping::Basic,
            );
            buf.shape_until_scroll(font_system, false);
            entries.push(TextEntry { buf, x: $x, y: $y, color: $color });
        }};
    }

    for (i, row) in rows.iter().enumerate() {
        // Resolve per-row style: inherit window style, then apply per-indicator colour overrides.
        let row_rs = match &row.style {
            Some(s) => win_rs.apply_colors(s),
            None    => win_rs.clone(),
        };

        let y = win_rs.pad_y + i as f32 * win_rs.line_height;
        let vc = row_rs.token_to_color(&row.color);
        let show = row.color != "crit" || blink;

        if !row.label.is_empty() {
            let lc = if row.color == "value" || row.color.is_empty() { row_rs.c_label } else { vc };
            push!(&row.label, win_rs.pad_x + 2.0, y + 2.0, row_rs.c_shadow);
            push!(&row.label, win_rs.pad_x, y, lc);
        }
        if show && !row.value_str.is_empty() {
            // Right-align value within value column.
            let this_w = measure(font_system, &row.value_str);
            let vx = win_rs.pad_x + label_col_w + win_rs.col_gap + (value_col_w - this_w).max(0.0);
            push!(&row.value_str, vx + 2.0, y + 2.0, row_rs.c_shadow);
            push!(&row.value_str, vx, y, vc);
        }
        if show && !row.unit.is_empty() {
            let ux = win_rs.pad_x + label_col_w + win_rs.col_gap + value_col_w + win_rs.col_gap;
            push!(&row.unit, ux + 2.0, y + 2.0, row_rs.c_shadow);
            push!(&row.unit, ux, y, row_rs.c_unit);
        }
    }

    // Compute required window dimensions from actual content.
    let needed_w = (win_rs.pad_x
        + label_col_w
        + win_rs.col_gap
        + value_col_w
        + if has_units { win_rs.col_gap + unit_col_w } else { 0.0 }
        + win_rs.pad_x)
        .ceil() as u32;
    let needed_h = (win_rs.pad_y + rows.len().max(1) as f32 * win_rs.line_height + win_rs.pad_y)
        .ceil() as u32;

    (entries, needed_w.max(1), needed_h.max(1))
}

// ─── GpuContext ───────────────────────────────────────────────────────────────

/// Shared GPU resources (one per process).
pub struct GpuContext {
    pub adapter: wgpu::Adapter,
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    pub font_system: FontSystem,
    pub swash_cache: SwashCache,
    pub glyph_cache: Cache,
}

impl GpuContext {
    pub fn new(
        instance: &wgpu::Instance,
        compat_surface: Option<&wgpu::Surface<'_>>,
    ) -> Self {
        let adapter = pollster::block_on(instance.request_adapter(
            &wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::None,
                compatible_surface: compat_surface,
                force_fallback_adapter: false,
            },
        ))
        .expect("[gpu] no suitable adapter");

        let (device, queue) = pollster::block_on(
            adapter.request_device(&wgpu::DeviceDescriptor::default(), None),
        )
        .expect("[gpu] device init failed");

        let glyph_cache = Cache::new(&device);
        Self {
            adapter, device, queue,
            font_system: FontSystem::new(),
            swash_cache: SwashCache::new(),
            glyph_cache,
        }
    }
}

// ─── GpuOffscreenState (Windows) ─────────────────────────────────────────────

/// wgpu staging-buffer row alignment (bytes).
const ROW_ALIGN: u32 = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;

fn aligned_bpr(width: u32) -> u32 {
    (width * 4 + ROW_ALIGN - 1) & !(ROW_ALIGN - 1)
}

/// Per-window GPU state for the offscreen path (Windows).
pub struct GpuOffscreenState {
    pub render_texture: wgpu::Texture,
    pub staging_buffer: wgpu::Buffer,
    pub text_atlas: TextAtlas,
    pub text_renderer: TextRenderer,
    pub viewport: Viewport,
    pub width: u32,
    pub height: u32,
    /// Padded bytes per row in the staging buffer (multiple of `ROW_ALIGN`).
    pub bytes_per_row: u32,
}

impl GpuOffscreenState {
    pub fn new(ctx: &GpuContext, width: u32, height: u32) -> Self {
        let w = width.max(1);
        let h = height.max(1);
        let bpr = aligned_bpr(w);

        // Bgra8Unorm: no sRGB gamma since UpdateLayeredWindow expects raw BGRA.
        let format = wgpu::TextureFormat::Bgra8Unorm;

        let render_texture = ctx.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("overlay-rt"),
            size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });

        let staging_buffer = ctx.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("overlay-staging"),
            size: (bpr * h) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });

        let mut text_atlas = TextAtlas::new(
            &ctx.device, &ctx.queue, &ctx.glyph_cache, format,
        );
        let text_renderer = TextRenderer::new(
            &mut text_atlas, &ctx.device, wgpu::MultisampleState::default(), None,
        );
        let mut viewport = Viewport::new(&ctx.device, &ctx.glyph_cache);
        viewport.update(&ctx.queue, Resolution { width: w, height: h });

        Self { render_texture, staging_buffer, text_atlas, text_renderer, viewport,
               width: w, height: h, bytes_per_row: bpr }
    }

    pub fn resize(&mut self, ctx: &GpuContext, width: u32, height: u32) {
        *self = Self::new(ctx, width, height);
    }
}

/// Render one frame to the offscreen texture and return tightly-packed BGRA
/// pixel data (no row padding) suitable for `UpdateLayeredWindow`.
pub fn render_offscreen(
    ctx: &mut GpuContext,
    state: &mut GpuOffscreenState,
    rows: &[DisplayRow],
    blink: bool,
    win_style: Option<&core_client::RenderStyle>,
) -> Vec<u8> {
    let (entries, win_w, win_h) =
        make_text_entries(&mut ctx.font_system, rows, blink, win_style);

    // Build TextAreas borrowing from entries — both live for this frame.
    let text_areas: Vec<TextArea<'_>> = entries
        .iter()
        .map(|e| TextArea {
            buffer: &e.buf,
            left: e.x,
            top: e.y,
            scale: 1.0,
            bounds: TextBounds {
                left: 0, top: 0,
                right: win_w as i32,
                bottom: win_h as i32,
            },
            default_color: e.color,
            custom_glyphs: &[],
        })
        .collect();

    state.text_renderer
        .prepare(
            &ctx.device, &ctx.queue, &mut ctx.font_system,
            &mut state.text_atlas, &state.viewport,
            text_areas, &mut ctx.swash_cache,
        )
        .expect("[gpu] offscreen prepare");

    let view = state.render_texture
        .create_view(&wgpu::TextureViewDescriptor::default());

    let mut encoder = ctx.device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("overlay-offscreen"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &view,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
        });
        state.text_renderer
            .render(&state.text_atlas, &state.viewport, &mut pass)
            .expect("[gpu] offscreen render pass");
    }

    // Copy rendered texture → staging buffer.
    encoder.copy_texture_to_buffer(
        wgpu::ImageCopyTexture {
            texture: &state.render_texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::ImageCopyBuffer {
            buffer: &state.staging_buffer,
            layout: wgpu::ImageDataLayout {
                offset: 0,
                bytes_per_row: Some(state.bytes_per_row),
                rows_per_image: Some(state.height),
            },
        },
        wgpu::Extent3d { width: state.width, height: state.height, depth_or_array_layers: 1 },
    );

    ctx.queue.submit(std::iter::once(encoder.finish()));

    // Map and wait.
    let slice = state.staging_buffer.slice(..);
    let (tx, rx) = std::sync::mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |r| { let _ = tx.send(r); });
    ctx.device.poll(wgpu::Maintain::Wait);
    rx.recv().expect("[gpu] staging map failed").expect("[gpu] staging map error");

    let row_bytes = (state.width * 4) as usize;
    let padded    = state.bytes_per_row as usize;
    let mapped    = slice.get_mapped_range();
    let mut pixels = vec![0u8; state.width as usize * state.height as usize * 4];
    for row in 0..state.height as usize {
        pixels[row * row_bytes..(row + 1) * row_bytes]
            .copy_from_slice(&mapped[row * padded..row * padded + row_bytes]);
    }
    drop(mapped);
    state.staging_buffer.unmap();

    pixels
}

// ─── GpuSurfaceState (Linux / macOS) ─────────────────────────────────────────

/// Per-window GPU state for the surface path (Linux / macOS).
pub struct GpuSurfaceState {
    pub surface: wgpu::Surface<'static>,
    pub surface_config: wgpu::SurfaceConfiguration,
    pub text_atlas: TextAtlas,
    pub text_renderer: TextRenderer,
    pub viewport: Viewport,
    pub width: u32,
    pub height: u32,
    pub text_cache: TextCache,
}

impl GpuSurfaceState {
    pub fn new(ctx: &GpuContext, surface: wgpu::Surface<'static>, width: u32, height: u32) -> Self {
        let w = width.max(1);
        let h = height.max(1);
        let caps = surface.get_capabilities(&ctx.adapter);

        let format = caps.formats.iter().find(|f| f.is_srgb()).copied()
            .unwrap_or(caps.formats[0]);

        let alpha_mode = [
            wgpu::CompositeAlphaMode::PreMultiplied,
            wgpu::CompositeAlphaMode::PostMultiplied,
            wgpu::CompositeAlphaMode::Inherit,
        ]
        .iter()
        .find(|m| caps.alpha_modes.contains(m))
        .copied()
        .unwrap_or(wgpu::CompositeAlphaMode::Auto);

        eprintln!(
            "[gpu] surface format={format:?}  alpha_mode={alpha_mode:?}  \
             available={:?}", caps.alpha_modes
        );

        let cfg = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format, width: w, height: h,
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode, view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&ctx.device, &cfg);

        let mut text_atlas = TextAtlas::new(
            &ctx.device, &ctx.queue, &ctx.glyph_cache, format,
        );
        let text_renderer = TextRenderer::new(
            &mut text_atlas, &ctx.device, wgpu::MultisampleState::default(), None,
        );
        let mut viewport = Viewport::new(&ctx.device, &ctx.glyph_cache);
        viewport.update(&ctx.queue, Resolution { width: w, height: h });

        Self { surface, surface_config: cfg, text_atlas, text_renderer, viewport,
               width: w, height: h, text_cache: TextCache::new() }
    }

    pub fn resize(&mut self, ctx: &GpuContext, width: u32, height: u32) {
        if width == 0 || height == 0 { return; }
        self.width = width;
        self.height = height;
        self.surface_config.width = width;
        self.surface_config.height = height;
        self.surface.configure(&ctx.device, &self.surface_config);
        self.viewport.update(&ctx.queue, Resolution { width, height });
    }
}

/// Render one frame to the surface and present.
///
/// Returns `Ok((needed_w, needed_h))` — the content-driven dimensions the
/// window should be resized to if they differ from the current surface size.
pub fn render_surface(
    ctx: &mut GpuContext,
    state: &mut GpuSurfaceState,
    rows: &[DisplayRow],
    blink: bool,
    win_style: Option<&core_client::RenderStyle>,
) -> Result<(u32, u32), wgpu::SurfaceError> {
    let (entries, win_w, win_h) =
        state.text_cache.get_or_rebuild(&mut ctx.font_system, rows, blink, win_style);

    let text_areas: Vec<TextArea<'_>> = entries
        .iter()
        .map(|e| TextArea {
            buffer: &e.buf,
            left: e.x, top: e.y, scale: 1.0,
            bounds: TextBounds {
                left: 0, top: 0,
                right: win_w as i32, bottom: win_h as i32,
            },
            default_color: e.color,
            custom_glyphs: &[],
        })
        .collect();

    state.text_renderer
        .prepare(
            &ctx.device, &ctx.queue, &mut ctx.font_system,
            &mut state.text_atlas, &state.viewport,
            text_areas, &mut ctx.swash_cache,
        )
        .expect("[gpu] surface prepare");

    let output = state.surface.get_current_texture()?;
    let view = output.texture.create_view(&wgpu::TextureViewDescriptor::default());

    let mut encoder = ctx.device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("overlay-surface"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &view,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
        });
        state.text_renderer
            .render(&state.text_atlas, &state.viewport, &mut pass)
            .expect("[gpu] surface render pass");
    }

    ctx.queue.submit(std::iter::once(encoder.finish()));
    output.present();
    Ok((win_w, win_h))
}
