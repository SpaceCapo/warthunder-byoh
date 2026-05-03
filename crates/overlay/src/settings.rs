//! Settings / about window powered by egui.
//!
//! This is the "face" of the application to the OS: it has normal window
//! decorations, appears in the taskbar / dock, and is what the user sees when
//! they interact with the app outside of the overlay HUD.
//!
//! Overlay (indicator) windows use `WS_EX_TOOLWINDOW` / macOS collection
//! behaviour to stay off the taskbar and dock window list.

use std::sync::Arc;
use winit::{
    event::WindowEvent,
    event_loop::ActiveEventLoop,
    window::{Window, WindowAttributes},
};

use crate::render_gpu::GpuContext;

pub struct SettingsWindow {
    pub window: Arc<Window>,
    egui_ctx: egui::Context,
    egui_state: egui_winit::State,
    renderer: egui_wgpu::Renderer,
    surface: wgpu::Surface<'static>,
    surface_config: wgpu::SurfaceConfiguration,
    /// Whether the About dialog is open.
    show_about: bool,
    /// Set to true when the user clicks File → Exit.
    pub exit_requested: bool,
    /// FM database version tag (e.g. "v2.55.1.88"), empty if unavailable.
    fm_version_tag: String,
}

impl SettingsWindow {
    pub fn new(
        event_loop: &ActiveEventLoop,
        instance: &wgpu::Instance,
        ctx: &GpuContext,
        fm_version_tag: String,
    ) -> Option<Self> {
        let attrs = WindowAttributes::default()
            .with_title("War Thunder BYOH")
            .with_decorations(true)
            .with_resizable(false)
            .with_inner_size(winit::dpi::LogicalSize::new(420u32, 180u32));

        let window = Arc::new(event_loop.create_window(attrs).ok()?);
        let size = window.inner_size();

        let surface = instance.create_surface(Arc::clone(&window)).ok()?;
        let caps = surface.get_capabilities(&ctx.adapter);

        // Prefer an sRGB format; fall back to whatever the surface offers.
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
            &window,
            None, // native pixels_per_point (use window DPI)
            None, // max texture side (use default)
            None, // theme
        );

        let renderer = egui_wgpu::Renderer::new(&ctx.device, fmt, None, 1, false);

        Some(Self {
            window,
            egui_ctx,
            egui_state,
            renderer,
            surface,
            surface_config,
            show_about: false,
            exit_requested: false,
            fm_version_tag,
        })
    }

    pub fn window_id(&self) -> winit::window::WindowId {
        self.window.id()
    }

    /// Feed a winit `WindowEvent` to egui.  Returns `true` if egui consumed it.
    pub fn on_event(&mut self, event: &WindowEvent) -> bool {
        let response = self.egui_state.on_window_event(&self.window, event);
        if response.repaint {
            self.window.request_redraw();
        }
        response.consumed
    }

    pub fn resize(&mut self, ctx: &GpuContext, w: u32, h: u32) {
        if w == 0 || h == 0 { return; }
        self.surface_config.width = w;
        self.surface_config.height = h;
        self.surface.configure(&ctx.device, &self.surface_config);
    }

    pub fn render(&mut self, ctx: &GpuContext) {
        let surface_texture = match self.surface.get_current_texture() {
            Ok(t) => t,
            Err(e) => { eprintln!("[settings] surface error: {e}"); return; }
        };
        let view = surface_texture.texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        let raw_input = self.egui_state.take_egui_input(&self.window);

        // Pull mutable flags out so we can pass them into the UI closure
        // without borrowing `self` inside the `run` callback.
        let mut show_about = self.show_about;
        let mut exit_requested = self.exit_requested;

        let full_output = self.egui_ctx.run(raw_input, |ui_ctx| {
            build_ui(ui_ctx, &mut show_about, &mut exit_requested, &self.fm_version_tag);
        });

        self.show_about = show_about;
        self.exit_requested = exit_requested;

        self.egui_state.handle_platform_output(&self.window, full_output.platform_output);

        let tris = self.egui_ctx.tessellate(full_output.shapes, full_output.pixels_per_point);

        for (id, delta) in &full_output.textures_delta.set {
            self.renderer.update_texture(&ctx.device, &ctx.queue, *id, delta);
        }

        let screen_desc = egui_wgpu::ScreenDescriptor {
            size_in_pixels: [self.surface_config.width, self.surface_config.height],
            pixels_per_point: full_output.pixels_per_point,
        };

        let mut encoder = ctx.device.create_command_encoder(
            &wgpu::CommandEncoderDescriptor { label: Some("settings_encoder") }
        );
        self.renderer.update_buffers(
            &ctx.device, &ctx.queue, &mut encoder, &tris, &screen_desc,
        );

        {
            let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("settings_pass"),
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
                timestamp_writes: None,
                occlusion_query_set: None,
            }).forget_lifetime();
            self.renderer.render(&mut rp, &tris, &screen_desc);
        }

        for id in &full_output.textures_delta.free {
            self.renderer.free_texture(id);
        }

        ctx.queue.submit(std::iter::once(encoder.finish()));
        surface_texture.present();
    }
}

/// Build the egui UI for the settings window.
fn build_ui(ctx: &egui::Context, show_about: &mut bool, exit_requested: &mut bool, fm_version_tag: &str) {
    egui::TopBottomPanel::top("menu_bar").show(ctx, |ui| {
        egui::menu::bar(ui, |ui| {
            ui.menu_button("File", |ui| {
                if ui.button("Exit").clicked() {
                    *exit_requested = true;
                    ui.close_menu();
                }
            });
            ui.menu_button("Help", |ui| {
                if ui.button("About").clicked() {
                    *show_about = true;
                    ui.close_menu();
                }
            });
        });
    });

    egui::CentralPanel::default().show(ctx, |ui| {
        ui.vertical_centered(|ui| {
            ui.add_space(16.0);
            ui.heading("War Thunder BYOH (Bring Your Own HUD)");
            ui.add_space(8.0);
            ui.label("Thanks for using!  Settings and configuration coming soon.");
            if !fm_version_tag.is_empty() {
                ui.add_space(8.0);
                ui.label(format!("FM database: {fm_version_tag}"));
            }
        });
    });

    if *show_about {
        egui::Window::new("About")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.vertical_centered(|ui| {
                    ui.heading("War Thunder BYOH");
                    ui.label(concat!("Version ", env!("CARGO_PKG_VERSION")));
                    ui.add_space(8.0);
                    ui.label("A local War Thunder overlay and telemetry tool.");
                    ui.label("Data sourced exclusively from the game's local HTTP API.");
                    ui.add_space(8.0);
                    if ui.button("  Close  ").clicked() {
                        *show_about = false;
                    }
                });
            });
    }
}
