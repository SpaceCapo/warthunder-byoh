//! Settings / about window powered by egui.
//!
//! This is the "face" of the application to the OS: it has normal window
//! decorations, appears in the taskbar / dock, and is what the user sees when
//! they interact with the app outside of the overlay HUD.
//!
//! Overlay (indicator) windows use `WS_EX_TOOLWINDOW` / macOS collection
//! behaviour to stay off the taskbar and dock window list.

use std::sync::{Arc, RwLock, Mutex, atomic::{AtomicBool, Ordering}};
use winit::{
    event::WindowEvent,
    event_loop::ActiveEventLoop,
    window::{Window, WindowAttributes},
};

use crate::render_gpu::GpuContext;

/// Which tab is active in the settings window.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SettingsTab {
    Main,
    Settings,
}

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
    /// Wrapped in Arc<Mutex> so the background FM-update thread can write a
    /// fresh tag after the deferred update completes.
    fm_version_tag: Arc<Mutex<String>>,
    /// Pending FM update found by the background check thread.
    /// `Some((remote_tag, download_url))` when an update is available.
    pending_fm_update: Arc<Mutex<Option<(String, String)>>>,
    /// Install progress 0.0..=1.0 while an FM install is running, else None.
    fm_install_progress: Arc<Mutex<Option<f32>>>,
    /// Active tab.
    active_tab: SettingsTab,
    /// Shared application config (read/written by the Settings tab).
    app_config: Arc<RwLock<core_client::AppConfig>>,
    /// Set to true when the user presses the Reload button; the poller thread
    /// reads and clears this flag.
    reload_requested: Arc<AtomicBool>,
    /// Written by the poller thread when a reload fails; read and cleared here
    /// to show an error popup.
    reload_error: Arc<Mutex<Option<String>>>,
    /// Currently displayed reload-error message (shown in a modal until dismissed).
    displayed_error: Option<String>,
    /// Platform-specific directory for config files (indicators.json, config.json).
    config_dir: std::path::PathBuf,
    /// Platform-specific directory for FM database files.
    fm_dir: std::path::PathBuf,
}

impl SettingsWindow {
    pub fn new(
        event_loop: &ActiveEventLoop,
        instance: &wgpu::Instance,
        ctx: &GpuContext,
        fm_version_tag: Arc<Mutex<String>>,
        pending_fm_update: Arc<Mutex<Option<(String, String)>>>,
        fm_install_progress: Arc<Mutex<Option<f32>>>,
        app_config: Arc<RwLock<core_client::AppConfig>>,
        reload_requested: Arc<AtomicBool>,
        reload_error: Arc<Mutex<Option<String>>>,
        config_dir: std::path::PathBuf,
        fm_dir: std::path::PathBuf,
        // Optional position hint so the window opens on the same monitor as the
        // overlay windows (and therefore appears in the correct per-monitor taskbar).
        hint_pos: Option<winit::dpi::LogicalPosition<i32>>,
    ) -> Option<Self> {
        let mut attrs = WindowAttributes::default()
            .with_title("War Thunder BYOH")
            .with_decorations(true)
            .with_resizable(false)
            // Start hidden so callers can apply platform window-style changes
            // (WS_EX_APPWINDOW, owner clearing) before the Shell first sees the
            // window.  The shell registers a taskbar entry on SW_SHOW, so any
            // style fix applied while hidden takes effect cleanly on first show.
            .with_visible(false)
            .with_inner_size(winit::dpi::LogicalSize::new(420u32, 290u32));
        if let Some(pos) = hint_pos {
            attrs = attrs.with_position(pos);
        }

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
            pending_fm_update,
            fm_install_progress,
            active_tab: SettingsTab::Main,
            app_config,
            reload_requested,
            reload_error,
            displayed_error: None,
            config_dir,
            fm_dir,
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
        let mut active_tab = self.active_tab;

        // Check the reload-error channel; latch any new message into local state.
        if let Ok(mut slot) = self.reload_error.try_lock() {
            if slot.is_some() {
                self.displayed_error = slot.take();
            }
        }
        let mut displayed_error = self.displayed_error.clone();

        // Snapshot the config so the closure can read/mutate a local copy.
        let mut config = self.app_config
            .read()
            .map(|g| g.clone())
            .unwrap_or_default();
        let mut config_changed = false;

        let reload_requested = self.reload_requested.clone();
        let config_dir = self.config_dir.clone();
        let fm_dir = self.fm_dir.clone();

        let fm_version_tag_snap = self.fm_version_tag
            .lock()
            .map(|g| g.clone())
            .unwrap_or_default();

        // Snapshot FM update state.
        let pending_update_snap = self.pending_fm_update
            .lock()
            .map(|g| g.clone())
            .unwrap_or(None);
        let install_progress_snap = self.fm_install_progress
            .lock()
            .map(|g| *g)
            .unwrap_or(None);

        let mut install_triggered = false;

        let full_output = self.egui_ctx.run(raw_input, |ui_ctx| {
            build_ui(
                ui_ctx,
                &mut show_about,
                &mut exit_requested,
                &fm_version_tag_snap,
                pending_update_snap.as_ref().map(|(t, _)| t.as_str()),
                install_progress_snap,
                &mut install_triggered,
                &mut active_tab,
                &mut config,
                &mut config_changed,
                &reload_requested,
                &mut displayed_error,
                &config_dir,
                &fm_dir,
            );
        });

        // Spawn the install thread when the user clicked Install.
        if install_triggered {
            if let Some((remote_tag, download_url)) = pending_update_snap {
                let progress_arc = self.fm_install_progress.clone();
                let tag_arc = self.fm_version_tag.clone();
                let pending_arc = self.pending_fm_update.clone();
                let fm_base = core_client::fm_base_dir();
                std::thread::Builder::new()
                    .name("fm-install".to_string())
                    .spawn(move || {
                        let result = core_client::install_fm_update(
                            &fm_base,
                            &remote_tag,
                            &download_url,
                            |p| {
                                if let Ok(mut lock) = progress_arc.lock() {
                                    *lock = Some(p);
                                }
                            },
                        );
                        match result {
                            Ok(new_tag) => {
                                if let Ok(mut lock) = tag_arc.lock() { *lock = new_tag; }
                                if let Ok(mut lock) = pending_arc.lock() { *lock = None; }
                            }
                            Err(e) => eprintln!("[fm_update] install error: {e}"),
                        }
                        if let Ok(mut lock) = progress_arc.lock() { *lock = None; }
                    })
                    .ok();
            }
        }

        // Write config changes back and persist to disk.
        if config_changed {
            if let Ok(mut lock) = self.app_config.write() {
                *lock = config.clone();
            }
            config.save();
        }

        self.show_about = show_about;
        self.exit_requested = exit_requested;
        self.active_tab = active_tab;
        self.displayed_error = displayed_error;

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
#[allow(clippy::too_many_arguments)]
fn build_ui(
    ctx: &egui::Context,
    show_about: &mut bool,
    exit_requested: &mut bool,
    fm_version_tag: &str,
    // Remote tag when an FM update is available, `None` otherwise.
    pending_update_tag: Option<&str>,
    // Install progress 0.0..=1.0 while installing, else `None`.
    install_progress: Option<f32>,
    // Set to `true` when the user clicks the Install button.
    install_triggered: &mut bool,
    active_tab: &mut SettingsTab,
    config: &mut core_client::AppConfig,
    config_changed: &mut bool,
    reload_requested: &Arc<AtomicBool>,
    displayed_error: &mut Option<String>,
    config_dir: &std::path::Path,
    fm_dir: &std::path::Path,
) {
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
        // Tab bar
        ui.horizontal(|ui| {
            if ui.selectable_label(*active_tab == SettingsTab::Main, "Main").clicked() {
                *active_tab = SettingsTab::Main;
            }
            if ui.selectable_label(*active_tab == SettingsTab::Settings, "Settings").clicked() {
                *active_tab = SettingsTab::Settings;
            }
        });
        ui.separator();

        match active_tab {
            SettingsTab::Main => {
                ui.vertical_centered(|ui| {
                    ui.add_space(12.0);
                    ui.heading("War Thunder BYOH (Bring Your Own HUD)");
                    ui.add_space(8.0);
                    ui.label(
                        egui::RichText::new(concat!("Build: ", env!("BYOH_BUILD_VERSION")))
                            .color(egui::Color32::from_rgb(200, 200, 200))
                    );
                    if !fm_version_tag.is_empty() {
                        ui.add_space(4.0);
                        ui.label(format!("FM database: {fm_version_tag}"));
                    } else {
                        ui.add_space(4.0);
                        ui.label(
                            egui::RichText::new("FM database: Unknown")
                                .color(egui::Color32::from_rgb(160, 160, 160)),
                        );
                    }

                    // FM update prompt / progress bar
                    if let Some(p) = install_progress {
                        ui.add_space(6.0);
                        let pct = (p * 100.0) as u32;
                        ui.add(
                            egui::ProgressBar::new(p)
                                .text(format!("Installing FM update... {pct}%"))
                                .animate(true),
                        );
                    } else if let Some(remote_tag) = pending_update_tag {
                        ui.add_space(6.0);
                        let arrow = if fm_version_tag.is_empty() {
                            format!("FM update available: {remote_tag}")
                        } else {
                            format!("FM update available: {fm_version_tag} → {remote_tag}")
                        };
                        ui.label(
                            egui::RichText::new(arrow)
                                .color(egui::Color32::from_rgb(255, 220, 80)),
                        );
                        ui.add_space(4.0);
                        if ui.button("  Install FM update  ").clicked() {
                            *install_triggered = true;
                        }
                    }

                    ui.add_space(8.0);
                    ui.separator();
                    ui.add_space(6.0);
                    ui.label(
                        egui::RichText::new(format!("Config dir:  {}", config_dir.display()))
                            .small()
                            .color(egui::Color32::from_rgb(200, 200, 200)),
                    );
                    ui.add_space(2.0);
                    ui.label(
                        egui::RichText::new(format!("FM data dir: {}", fm_dir.display()))
                            .small()
                            .color(egui::Color32::from_rgb(200, 200, 200)),
                    );
                });
            }

            SettingsTab::Settings => {
                ui.add_space(8.0);

                ui.label("Overlay visibility:");
                ui.add_space(4.0);

                if ui.checkbox(
                    &mut config.always_show,
                    "Always show overlay",
                ).on_hover_text("Show indicator windows regardless of War Thunder focus or mission state.")
                .changed() {
                    *config_changed = true;
                }

                if ui.checkbox(
                    &mut config.show_when_byoh_foreground,
                    "Show overlay when BYOH is focused",
                ).on_hover_text("Also show indicator windows when this settings window has focus.\nUseful for positioning or testing indicators.")
                .changed() {
                    *config_changed = true;
                }

                // TODO: mission polling is not yet reliable — checkbox hidden until fixed.
                // if ui.checkbox(
                //     &mut config.only_during_mission,
                //     "Only show overlay during active mission",
                // ).on_hover_text("Hide indicator windows when no mission is running.\nIgnored when \"Always show\" is enabled.")
                // .changed() {
                //     *config_changed = true;
                // }

                ui.add_space(16.0);
                ui.separator();
                ui.add_space(8.0);

                if ui.button("Reload indicators & config").clicked() {
                    reload_requested.store(true, Ordering::Relaxed);
                }
                ui.label(
                    egui::RichText::new(
                        "Reloads indicators.json and config.json from disk.\n\
                         Window layout changes (positions, new windows) require a restart."
                    )
                    .small()
                    .color(egui::Color32::from_rgb(200, 200, 200)),
                );

                ui.add_space(12.0);
                ui.separator();
                ui.add_space(8.0);

                ui.horizontal(|ui| {
                    if ui.button("Open config folder").clicked() {
                        open_folder(config_dir);
                    }
                    if ui.button("Open FM folder").clicked() {
                        open_folder(fm_dir);
                    }
                });
            }
        }
    });

    if *show_about {
        egui::Window::new("About")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.vertical_centered(|ui| {
                    ui.heading("War Thunder BYOH");
                    ui.label(concat!("Build: ", env!("BYOH_BUILD_VERSION")));
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

    if let Some(msg) = displayed_error.clone() {
        egui::Window::new("Reload Error")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label("Failed to reload indicators.json:");
                ui.add_space(4.0);
                egui::ScrollArea::vertical().max_height(200.0).show(ui, |ui| {
                    ui.add(
                        egui::TextEdit::multiline(&mut msg.as_str())
                            .font(egui::TextStyle::Monospace)
                            .desired_width(f32::INFINITY),
                    );
                    });
                ui.add_space(8.0);
                ui.vertical_centered(|ui| {
                    if ui.button("  OK  ").clicked() {
                        *displayed_error = None;
                    }
                });
            });
    }
}

/// Open `path` in the system file manager (best-effort; failures are silently
/// ignored because this is a convenience shortcut, not a critical operation).
fn open_folder(path: &std::path::Path) {
    #[cfg(target_os = "windows")]
    {
        // `explorer <path>` is unreliable from a Windows-subsystem process.
        // `cmd /c start "" <path>` works consistently; CREATE_NO_WINDOW
        // suppresses the brief console flash.
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let _ = std::process::Command::new("cmd")
            .args(["/c", "start", "", &path.display().to_string()])
            .creation_flags(CREATE_NO_WINDOW)
            .spawn();
    }

    #[cfg(target_os = "macos")]
    { let _ = std::process::Command::new("open").arg(path).spawn(); }

    #[cfg(target_os = "linux")]
    { let _ = std::process::Command::new("xdg-open").arg(path).spawn(); }

    // Silently no-op on unknown platforms.
    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    { let _ = path; }
}
