//! First-run setup wizard (GPU / egui path).
//!
//! Displayed before the overlay when the FM database or `indicators.json` is
//! missing.  The window is a normal (decorated, non-transparent) egui surface
//! window — it is not an overlay.  Once the user satisfies all requirements and
//! clicks "Start Overlay", `done` is set to `true` and the caller creates the
//! overlay windows.  If the user clicks "Exit", `exit_requested` is set.

use std::sync::{Arc, Mutex};
use winit::{
    event::WindowEvent,
    event_loop::ActiveEventLoop,
    window::{Window, WindowAttributes, WindowId},
};

use crate::render_gpu::GpuContext;

// ─── Background download state ────────────────────────────────────────────────

#[derive(Clone)]
enum DownloadStatus {
    Idle,
    InProgress,
    Done,
    Failed(String),
}

// ─── SetupWizardWindow ────────────────────────────────────────────────────────

pub struct SetupWizardWindow {
    pub window: Arc<Window>,
    egui_ctx: egui::Context,
    egui_state: egui_winit::State,
    renderer: egui_wgpu::Renderer,
    surface: wgpu::Surface<'static>,
    surface_config: wgpu::SurfaceConfiguration,
    /// Whether the FM database was missing and still needs to be supplied.
    needs_fm: bool,
    /// Whether `indicators.json` was missing and still needs to be created.
    needs_config: bool,
    /// Whether `indicators.json.example` is available as a seed source.
    has_example: bool,
    /// Set to true once the FM database requirement is satisfied.
    fm_done: bool,
    /// Set to true once the config requirement is satisfied.
    config_done: bool,
    /// Background download progress, written by the worker thread.
    download_status: Arc<Mutex<DownloadStatus>>,
    /// Set to `true` when setup is complete and the overlay should start.
    pub done: bool,
    /// Set to `true` when the user wants to exit without starting the overlay.
    pub exit_requested: bool,
}

impl SetupWizardWindow {
    pub fn new(
        event_loop: &ActiveEventLoop,
        instance: &wgpu::Instance,
        ctx: &GpuContext,
        needs: core_client::SetupNeeds,
    ) -> Option<Self> {
        let attrs = WindowAttributes::default()
            .with_title("War Thunder BYOH — First-run Setup")
            .with_decorations(true)
            .with_resizable(false)
            .with_inner_size(winit::dpi::LogicalSize::new(440u32, 300u32));

        let window = Arc::new(event_loop.create_window(attrs).ok()?);
        let size = window.inner_size();

        let surface = instance.create_surface(Arc::clone(&window)).ok()?;
        let caps = surface.get_capabilities(&ctx.adapter);
        let fmt = caps.formats.iter()
            .find(|f| f.is_srgb())
            .copied()
            .unwrap_or(caps.formats[0]);

        let surface_config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: fmt,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode: wgpu::CompositeAlphaMode::Opaque,
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&ctx.device, &surface_config);

        let egui_ctx = egui::Context::default();
        let egui_state = egui_winit::State::new(
            egui_ctx.clone(),
            egui::ViewportId::ROOT,
            &*window,
            None, None, None,
        );
        let renderer = egui_wgpu::Renderer::new(&ctx.device, fmt, None, 1, false);

        // If there is no example config to seed from, auto-satisfy the config
        // requirement so the user is not blocked on an unactionable step.
        let config_done = !needs.needs_config || (!needs.has_example && needs.needs_config);
        let fm_done = !needs.needs_fm;

        Some(Self {
            window,
            egui_ctx,
            egui_state,
            renderer,
            surface,
            surface_config,
            needs_fm:     needs.needs_fm,
            needs_config: needs.needs_config,
            has_example:  needs.has_example,
            fm_done,
            config_done,
            download_status: Arc::new(Mutex::new(DownloadStatus::Idle)),
            done:           false,
            exit_requested: false,
        })
    }

    pub fn window_id(&self) -> WindowId { self.window.id() }

    /// Feed a winit `WindowEvent` into egui.
    pub fn on_event(&mut self, event: &WindowEvent) {
        let response = self.egui_state.on_window_event(&self.window, event);
        if response.repaint { self.window.request_redraw(); }
    }

    pub fn resize(&mut self, ctx: &GpuContext, w: u32, h: u32) {
        if w == 0 || h == 0 { return; }
        self.surface_config.width  = w;
        self.surface_config.height = h;
        self.surface.configure(&ctx.device, &self.surface_config);
    }

    pub fn render(&mut self, ctx: &GpuContext) {
        let surface_texture = match self.surface.get_current_texture() {
            Ok(t)  => t,
            Err(e) => { eprintln!("[setup] surface error: {e}"); return; }
        };
        let view = surface_texture.texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        // Poll background download status; promote fm_done when finished.
        let dl_status = self.download_status.lock()
            .map(|g| g.clone())
            .unwrap_or(DownloadStatus::Idle);
        if matches!(dl_status, DownloadStatus::Done) {
            self.fm_done = true;
        }

        // Extract state needed inside the egui closure as local copies so we
        // don't try to borrow `self` inside `egui_ctx.run()`.
        let needs_fm     = self.needs_fm;
        let needs_config = self.needs_config;
        let has_example  = self.has_example;
        let fm_done      = self.fm_done;
        let config_done  = self.config_done;
        let all_done     = fm_done && config_done;
        let dl_status_c  = dl_status;

        // Action flags set inside the closure, applied after it returns.
        let mut do_download  = false;
        let mut do_retry     = false;
        let mut do_seed      = false;
        let mut do_start     = false;
        let mut do_exit      = false;

        let raw_input = self.egui_state.take_egui_input(&self.window);
        let full_output = self.egui_ctx.run(raw_input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                ui.heading("First-run Setup");
                ui.separator();
                ui.add_space(6.0);

                // ── FM Database ───────────────────────────────────────────
                if needs_fm {
                    ui.label(egui::RichText::new("FM Database").strong());
                    if fm_done {
                        ui.label(
                            egui::RichText::new("  FM database ready.")
                                .color(egui::Color32::from_rgb(80, 200, 80)),
                        );
                    } else {
                        match &dl_status_c {
                            DownloadStatus::Idle => {
                                ui.label("  FM database not found.");
                                if ui.button("  Download from GitHub (~1 MB)").clicked() {
                                    do_download = true;
                                }
                            }
                            DownloadStatus::InProgress => {
                                ui.horizontal(|ui| {
                                    ui.spinner();
                                    ui.label("  Downloading FM database…");
                                });
                            }
                            DownloadStatus::Done => {
                                // fm_done was set above; this branch is unreachable in practice
                                ui.label(
                                    egui::RichText::new("  FM database downloaded.")
                                        .color(egui::Color32::from_rgb(80, 200, 80)),
                                );
                            }
                            DownloadStatus::Failed(msg) => {
                                ui.label(
                                    egui::RichText::new(format!("  Download failed: {msg}"))
                                        .color(egui::Color32::from_rgb(220, 80, 80)),
                                );
                                if ui.button("  Retry").clicked() { do_retry = true; }
                            }
                        }
                    }
                    ui.add_space(8.0);
                }

                // ── Configuration ─────────────────────────────────────────
                if needs_config {
                    ui.label(egui::RichText::new("Configuration").strong());
                    if config_done {
                        ui.label(
                            egui::RichText::new("  indicators.json ready.")
                                .color(egui::Color32::from_rgb(80, 200, 80)),
                        );
                    } else if has_example {
                        ui.label("  indicators.json not found.");
                        if ui.button("  Use example config").clicked() { do_seed = true; }
                    } else {
                        ui.label(
                            egui::RichText::new(
                                "  indicators.json not found (no example available).\n  \
                                 The overlay will use built-in defaults.",
                            )
                            .color(egui::Color32::from_rgb(200, 200, 80)),
                        );
                        // config_done was already set to true in new() for this case
                    }
                    ui.add_space(8.0);
                }

                ui.separator();
                ui.add_space(6.0);

                ui.horizontal(|ui| {
                    ui.add_enabled_ui(all_done, |ui| {
                        if ui.button("Start Overlay").clicked() { do_start = true; }
                    });
                    ui.add_space(8.0);
                    if ui.button("Exit").clicked() { do_exit = true; }
                });
            });
        });

        // Apply deferred actions (must happen after egui_ctx.run returns).
        if do_download || do_retry { self.start_download(); }
        if do_seed {
            match core_client::seed_config_from_example() {
                Ok(())  => { self.config_done = true; }
                Err(e)  => { eprintln!("[setup] seed config: {e}"); }
            }
        }
        if do_start { self.done           = true; }
        if do_exit  { self.exit_requested = true; }

        // ── GPU submit ────────────────────────────────────────────────────

        self.egui_state.handle_platform_output(&self.window, full_output.platform_output);

        let tris = self.egui_ctx.tessellate(full_output.shapes, full_output.pixels_per_point);

        let screen_desc = egui_wgpu::ScreenDescriptor {
            size_in_pixels: [self.surface_config.width, self.surface_config.height],
            pixels_per_point: full_output.pixels_per_point,
        };

        let mut encoder = ctx.device.create_command_encoder(
            &wgpu::CommandEncoderDescriptor { label: Some("setup_encoder") },
        );

        for (id, delta) in &full_output.textures_delta.set {
            self.renderer.update_texture(&ctx.device, &ctx.queue, *id, delta);
        }
        self.renderer.update_buffers(&ctx.device, &ctx.queue, &mut encoder, &tris, &screen_desc);

        {
            let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("setup_pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: 0.12, g: 0.12, b: 0.12, a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes:        None,
                occlusion_query_set:     None,
            }).forget_lifetime();
            self.renderer.render(&mut rp, &tris, &screen_desc);
        }

        for id in &full_output.textures_delta.free {
            self.renderer.free_texture(id);
        }

        ctx.queue.submit(std::iter::once(encoder.finish()));
        surface_texture.present();
    }

    // ─── Private helpers ──────────────────────────────────────────────────────

    fn start_download(&self) {
        // Guard: don't start a second download if one is already running.
        if let Ok(s) = self.download_status.lock() {
            if matches!(*s, DownloadStatus::InProgress) { return; }
        }
        if let Ok(mut s) = self.download_status.lock() {
            *s = DownloadStatus::InProgress;
        }
        let status  = Arc::clone(&self.download_status);
        let fm_root = core_client::fm_root_dir();
        std::thread::spawn(move || {
            let result = core_client::download_fm_data(&fm_root);
            if let Ok(mut s) = status.lock() {
                *s = match result {
                    Ok(())  => DownloadStatus::Done,
                    Err(e)  => DownloadStatus::Failed(e),
                };
            }
        });
        // Request a redraw so the spinner appears immediately.
        self.window.request_redraw();
    }
}
