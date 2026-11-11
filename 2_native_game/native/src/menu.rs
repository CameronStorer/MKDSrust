//! Host-only overlay. No guest memory or emulation state is accessed here.
use crate::{
    gpu::Gpu,
    pad::Pads,
    window::{Display, HostLink},
};
use mkds_builder::{
    controls::{self, Controls},
    settings::{self, Layout, Settings},
};
use std::{path::PathBuf, sync::atomic::Ordering, time::Instant};
use winit::{
    event::{KeyEvent, WindowEvent},
    keyboard::{KeyCode, PhysicalKey},
    window::Window,
};

pub struct Menu {
    pub open: bool,
    /// Pause menu (Esc / the Quit binding): Resume, Settings, Quit. The game waits while it is open.
    pub pause: bool,
    pause_choice: usize,
    pause_was: bool,
    quit: bool,
    pad_held: Vec<usize>,
    pub settings: Settings,
    controls: Controls,
    controls_path: Option<PathBuf>,
    ctx: egui::Context,
    state: Option<egui_winit::State>,
    renderer: egui_wgpu::Renderer,
    output: Option<egui::FullOutput>,
    tab: usize,
    capture: Option<(bool, usize)>,
    wizard: bool,
    armed: bool,
    pads: Pads,
    status: String,
    saved_settings: Settings,
    saved_controls: Controls,
    fps_start: Instant,
    frames: u32,
    fps: f32,
    /// The Widescreen mod is part of this build: show its setting.
    widescreen_mod: bool,
}
const PAUSE: [&str; 3] = ["Resume", "Settings (F10)", "Quit game"];

impl Menu {
    pub fn new(window: &Window, gpu: &Gpu, format: wgpu::TextureFormat, display: &Display) -> Self {
        let ctx = egui::Context::default();
        ctx.set_visuals(egui::Visuals::dark());
        let state = egui_winit::State::new(
            ctx.clone(),
            egui::ViewportId::ROOT,
            window,
            Some(window.scale_factor() as f32),
            None,
            None,
        );
        let controls = display
            .controls_path
            .as_ref()
            .and_then(|p| Controls::load_or_create(p).ok())
            .unwrap_or_default();
        let mut settings = display.settings.clone();
        settings.scale = display.scale;
        settings.smooth = display.smooth;
        settings.fullscreen = display.fullscreen;
        Self {
            widescreen_mod: display.widescreen_mod,
            open: false,
            pause: false,
            pause_choice: 0,
            pause_was: false,
            quit: false,
            pad_held: Vec::new(),
            saved_settings: settings.clone(),
            settings,
            saved_controls: controls.clone(),
            controls,
            controls_path: display.controls_path.clone(),
            ctx,
            state: Some(state),
            renderer: egui_wgpu::Renderer::new(&gpu.device, format, egui_wgpu::RendererOptions::default()),
            output: None,
            tab: 0,
            capture: None,
            wizard: false,
            armed: false,
            pads: Pads::new(),
            status: String::new(),
            fps_start: Instant::now(),
            frames: 0,
            fps: 0.0,
        }
    }
    pub fn event(&mut self, window: &Window, event: &WindowEvent) {
        // A captured Enter/Space must not also activate the currently focused UI button.
        if self.capture.is_some() && matches!(event, WindowEvent::KeyboardInput { .. }) {
            return;
        }
        let _ = self.state.as_mut().unwrap().on_window_event(window, event);
        if matches!(event, WindowEvent::Focused(false)) {
            self.cancel_capture();
        }
    }
    pub fn cancel_capture(&mut self) {
        self.capture = None;
        self.wizard = false;
    }
    pub fn key(&mut self, event: &KeyEvent) {
        if event.physical_key == PhysicalKey::Code(KeyCode::Escape) {
            if self.capture.is_some() {
                self.cancel_capture();
            } else if self.open {
                self.open = false;
            } else {
                self.pause = false;
            }
            return;
        }
        if self.pause && !self.open {
            match event.physical_key {
                PhysicalKey::Code(KeyCode::ArrowUp) => self.pause_choice = (self.pause_choice + PAUSE.len() - 1) % PAUSE.len(),
                PhysicalKey::Code(KeyCode::ArrowDown | KeyCode::Tab) => self.pause_choice = (self.pause_choice + 1) % PAUSE.len(),
                PhysicalKey::Code(KeyCode::Enter | KeyCode::NumpadEnter | KeyCode::Space) => self.choose(self.pause_choice),
                _ => {}
            }
            return;
        }
        if let Some((false, action)) = self.capture {
            if let PhysicalKey::Code(code) = event.physical_key {
                if code == KeyCode::F11 || (code == KeyCode::F9 && self.widescreen_mod) {
                    self.status = "F9, F10 and F11 are reserved (map overlay, menu, fullscreen).".into();
                    return;
                }
                if let Some(key) = crate::window::key_index(code, &event.logical_key) {
                    self.controls.keys[action] = vec![key];
                    self.next(false, action);
                } else {
                    self.status = "That key is not supported; choose another key.".into();
                }
            }
        }
    }
    /// The pause menu over the dimmed game.
    fn pause_window(&mut self, ctx: &egui::Context) {
        // Dim the game, then a small centred menu.
        let screen = ctx.content_rect();
        ctx.layer_painter(egui::LayerId::new(egui::Order::Background, egui::Id::new("pause dim")))
            .rect_filled(screen, 0.0, egui::Color32::from_black_alpha(150));
        egui::Window::new("MKDSrust • Paused").anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .resizable(false).collapsible(false).show(ctx, |ui| {
            ui.vertical_centered(|ui| {
                for (i, label) in PAUSE.iter().enumerate() {
                    let button = egui::Button::new(egui::RichText::new(*label).size(18.0)).min_size(egui::vec2(220.0, 36.0)).selected(i == self.pause_choice);
                    let r = ui.add(button);
                    if r.hovered() { self.pause_choice = i; }
                    if r.clicked() { self.choose(i); }
                }
                ui.add_space(4.0);
                ui.small("Esc / B: resume   Arrows + Enter / A: choose");
            });
        });
    }
    /// A pause menu entry: 0 Resume, 1 Settings (the F10 menu), 2 Quit.
    fn choose(&mut self, choice: usize) {
        match choice {
            0 => self.pause = false,
            1 => { self.open = true; self.cancel_capture(); }
            _ => self.quit = true,
        }
    }
    /// Controller in the pause menu: up/down choose, A (South) confirms, B (East) or Start resumes.
    fn pause_pad(&mut self) {
        let pressed = self.pads.pressed(self.settings.deadzone as f32 / 100.0);
        let new: Vec<usize> = pressed.iter().copied().filter(|b| !self.pad_held.contains(b)).collect();
        self.pad_held = pressed;
        if !self.pause || self.open {
            return;
        }
        let is = |name: &str| new.iter().any(|&b| controls::PAD[b] == name);
        if is("DPadUp") || is("StickUp") { self.pause_choice = (self.pause_choice + PAUSE.len() - 1) % PAUSE.len(); }
        if is("DPadDown") || is("StickDown") { self.pause_choice = (self.pause_choice + 1) % PAUSE.len(); }
        if is("South") { self.choose(self.pause_choice); } else if is("East") || is("Start") { self.pause = false; }
    }
    fn next(&mut self, pad: bool, action: usize) {
        self.capture = if self.wizard && action < 11 {
            Some((pad, action + 1))
        } else {
            None
        };
        self.armed = false;
    }
    pub fn prepare(&mut self, window: &Window, link: &HostLink, frame: bool) {
        self.frames += u32::from(frame);
        if self.fps_start.elapsed().as_secs_f32() >= 1.0 {
            self.fps = self.frames as f32 / self.fps_start.elapsed().as_secs_f32();
            self.frames = 0;
            self.fps_start = Instant::now();
        }
        if link.pause_request.swap(false, Ordering::AcqRel) && !self.pause && !self.open {
            self.pause = true;
            self.pause_choice = 0;
        }
        if self.open || self.pause || self.pause_was {
            self.pads.poll();
            self.pause_pad();
        }
        if self.open {
            if let Some((true, action)) = self.capture {
                let pressed = self.pads.pressed(self.settings.deadzone as f32 / 100.0);
                if pressed.is_empty() {
                    self.armed = true;
                } else if self.armed {
                    self.controls.pad[action] = vec![pressed[0]];
                    self.next(true, action);
                }
            }
        }
        let ctx = self.ctx.clone();
        let input = self.state.as_mut().unwrap().take_egui_input(window);
        let output = ctx.run_pass(input, |ctx| {
            if self.settings.fps {
                let anchors = [egui::Align2::LEFT_TOP, egui::Align2::RIGHT_TOP, egui::Align2::LEFT_BOTTOM, egui::Align2::RIGHT_BOTTOM];
                egui::Area::new(egui::Id::unique("fps")).anchor(anchors[self.settings.fps_position as usize], egui::Vec2::ZERO).interactable(false).show(ctx, |ui| {
                    egui::Frame::dark_canvas(ui.style()).show(ui, |ui| { ui.label(format!("{:.1} FPS", self.fps)); });
                });
            }
            if self.pause && !self.open {
                self.pause_window(ctx);
            }
            if self.open {
                egui::Window::new("MKDSrust • Settings").default_width(610.0).default_height(490.0).resizable(true).collapsible(false).show(ctx, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        for (i, name) in ["Graphics", "Performance", "Controls", "Controller", "Audio", "System"].iter().enumerate() {
                            if ui.selectable_label(self.tab == i, *name).clicked() { self.tab = i; self.cancel_capture(); }
                        }
                    });
                    ui.separator();
                    egui::ScrollArea::vertical().max_height(540.0).show(ui, |ui| self.page(ui, link));
                    ui.separator();
                    ui.label("Changes apply immediately and save automatically. The game keeps running.");
                    if !self.status.is_empty() { ui.colored_label(egui::Color32::YELLOW, &self.status); }
                    if ui.button("Return to game (F10)").clicked() { self.open = false; self.cancel_capture(); }
                });
            }
        });
        // A surface can be unavailable for a frame. Preserve font/texture deltas until painted.
        if let Some(pending) = &mut self.output {
            pending.append(output);
        } else {
            self.output = Some(output);
        }
        if self.settings != self.saved_settings {
            match settings::default_path()
                .ok_or("No settings folder available".to_string())
                .and_then(|p| self.settings.save(&p))
            {
                Ok(()) => {
                    self.saved_settings = self.settings.clone();
                    self.status = "Settings saved.".into();
                }
                Err(e) => self.status = e,
            }
        }
        if self.controls != self.saved_controls {
            match self
                .controls_path
                .as_ref()
                .ok_or("No controls folder available".to_string())
                .and_then(|p| self.controls.save(p))
            {
                Ok(()) => {
                    self.saved_controls = self.controls.clone();
                    self.status = "Controls saved.".into();
                }
                Err(e) => self.status = e,
            }
        }
        *link.preferences.lock().unwrap() = Some((self.settings.clone(), self.controls.clone()));
        if self.quit {
            link.close();
        }
        if self.pause_was && !self.pause {
            // Whatever closed the menu must not reach the game, and must not reopen it.
            link.input_guard.store(true, Ordering::Release);
            link.quit_held.store(true, Ordering::Release);
        }
        self.pause_was = self.pause;
        link.paused.store(self.pause, Ordering::Release);
        link.menu_open.store(self.open || self.pause, Ordering::Release);
    }
    fn page(&mut self, ui: &mut egui::Ui, link: &HostLink) {
        match self.tab {
            0 => {
                ui.heading("Graphics");
                egui::ComboBox::from_label("Internal resolution")
                    .selected_text(
                        self.settings
                            .scale
                            .map_or("Auto".into(), |n| format!("{n}x")),
                    )
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut self.settings.scale, None, "Auto");
                        for n in 1..=8 {
                            ui.selectable_value(&mut self.settings.scale, Some(n), format!("{n}x"));
                        }
                    });
                ui.checkbox(
                    &mut self.settings.smooth,
                    "Linear filtering (off = nearest)",
                );
                ui.checkbox(&mut self.settings.vsync, "Vsync");
                ui.checkbox(&mut self.settings.fullscreen, "Fullscreen / borderless");
                ui.horizontal(|ui| {
                    ui.label("Window size");
                    for n in 1..=4 {
                        ui.selectable_value(&mut self.settings.window_size, n, format!("{n}x"));
                    }
                });
                ui.horizontal(|ui| {
                    ui.label("Screen layout");
                    ui.selectable_value(&mut self.settings.layout, Layout::Stacked, "Stacked");
                    ui.selectable_value(
                        &mut self.settings.layout,
                        Layout::SideBySide,
                        "Side by side",
                    );
                    if self.widescreen_mod {
                        ui.selectable_value(&mut self.settings.layout, Layout::Combined, "Combined (mod)");
                    }
                });
                if self.settings.layout == Layout::Combined {
                    if !self.widescreen_mod {
                        ui.small("Combined needs the Widescreen mod; showing Stacked.");
                    } else if self.settings.renderer != settings::Renderer::Gpu {
                        ui.small("Combined needs the GPU renderer; showing Stacked.");
                    } else {
                        ui.small("One widescreen picture from both screens: menus side by side, races full width with the map (F9: Picture or Track). Single screen, swap and gap do not apply.");
                    }
                }
                if self.widescreen_mod {
                    ui.horizontal(|ui| {
                        ui.label("Widescreen (mod)");
                        for w in settings::Widescreen::ALL {
                            ui.selectable_value(&mut self.settings.widescreen, w, w.name());
                        }
                    });
                    if self.settings.widescreen != settings::Widescreen::Off
                        && self.settings.renderer != settings::Renderer::Gpu
                    {
                        ui.small("Widescreen needs the GPU renderer.");
                    }
                    ui.checkbox(&mut self.settings.map_overlay, "Map in the corner of the 3D screen (F9)");
                    if self.settings.map_overlay {
                        ui.horizontal(|ui| {
                            ui.label("Show");
                            ui.selectable_value(&mut self.settings.map_overlay_track, false, "Whole touch screen");
                            ui.selectable_value(&mut self.settings.map_overlay_track, true, "Track and heads only");
                        });
                        if self.settings.map_overlay_track {
                            ui.small("Click the map once to switch the game to its whole-course view.");
                        }
                        ui.add(egui::Slider::new(&mut self.settings.map_overlay_size, 25..=75).text("Map size %"));
                        ui.add(egui::Slider::new(&mut self.settings.map_overlay_opacity, 30..=100).text("Map opacity %"));
                    }
                }
                ui.checkbox(&mut self.settings.single_screen, "Single screen");
                ui.checkbox(
                    &mut self.settings.swap_screens,
                    "Swap screens (single: show touch screen)",
                );
                ui.add(egui::Slider::new(&mut self.settings.gap, 0..=192).text("Screen gap"));
            }
            1 => {
                ui.heading("Performance");
                ui.checkbox(&mut self.settings.fps, "Show FPS counter");
                egui::ComboBox::from_label("FPS position")
                    .selected_text(
                        ["Top left", "Top right", "Bottom left", "Bottom right"]
                            [self.settings.fps_position as usize],
                    )
                    .show_ui(ui, |ui| {
                        for (n, label) in ["Top left", "Top right", "Bottom left", "Bottom right"]
                            .iter()
                            .enumerate()
                        {
                            ui.selectable_value(&mut self.settings.fps_position, n as u32, *label);
                        }
                    });
                ui.checkbox(&mut self.settings.frame_limiter, "Limit game speed");
                ui.add(egui::Slider::new(&mut self.settings.speed, 25..=400).text("Speed (%)"));
            }
            2 | 3 => {
                let pad = self.tab == 3;
                ui.heading(if pad {
                    "Controller"
                } else {
                    "Keyboard & touch"
                });
                if pad {
                    let names = self.pads.names();
                    if names.is_empty() {
                        ui.label("No controller connected. Connect one to begin mapping.");
                    }
                    for name in names {
                        ui.label(name);
                    }
                    ui.add(
                        egui::Slider::new(&mut self.settings.deadzone, 5..=95)
                            .text("Analog deadzone (%)"),
                    );
                    ui.label("Rumble: off");
                } else {
                    ui.checkbox(
                        &mut self.settings.touch_mouse,
                        "Left mouse button controls touch screen",
                    );
                }
                if let Some((_, a)) = self.capture {
                    ui.colored_label(
                        egui::Color32::YELLOW,
                        format!(
                            "{} for {}. Escape cancels.",
                            if pad {
                                "Release all buttons, then press a button / stick direction"
                            } else {
                                "Press a key"
                            },
                            controls::ACTIONS[a]
                        ),
                    );
                    if ui.button("Cancel capture").clicked() {
                        self.cancel_capture();
                    }
                }
                if ui.button("Map all DS buttons…").clicked() {
                    self.capture = Some((pad, 0));
                    self.wizard = true;
                    self.armed = false;
                }
                egui::Grid::new("bindings").striped(true).show(ui, |ui| {
                    for a in 0..controls::ACTIONS.len() {
                        ui.label(controls::ACTIONS[a]);
                        if ui
                            .button(if pad {
                                self.controls.describe_pad(a)
                            } else {
                                self.controls.describe(a)
                            })
                            .clicked()
                        {
                            self.capture = Some((pad, a));
                            self.wizard = false;
                            self.armed = false;
                        }
                        if ui.small_button("Clear").clicked() {
                            if pad {
                                self.controls.pad[a].clear();
                            } else {
                                self.controls.keys[a].clear();
                            }
                        }
                        ui.end_row();
                    }
                });
                if ui.button("Reset to defaults").clicked() {
                    if pad {
                        self.controls.pad = Controls::default().pad;
                        self.settings.deadzone = 50;
                    } else {
                        self.controls.keys = Controls::default().keys;
                        self.settings.touch_mouse = true;
                    }
                    self.cancel_capture();
                }
            }
            4 => {
                ui.heading("Audio");
                ui.add(
                    egui::Slider::new(&mut self.settings.volume, 0..=100).text("Master volume (%)"),
                );
                ui.checkbox(&mut self.settings.mute, "Mute");
                ui.label("If started with --mute, restart without that option to enable audio.");
            }
            _ => {
                ui.heading("System");
                ui.checkbox(&mut self.settings.verbose, "Verbose performance logging");
                if ui.button("Open saves folder").clicked() {
                    self.status = settings::save_folder()
                        .ok_or("No saves folder available".to_string())
                        .and_then(|p| {
                            std::fs::create_dir_all(&p).map_err(|e| e.to_string())?;
                            std::process::Command::new("explorer.exe")
                                .arg(p)
                                .spawn()
                                .map(|_| "Opened saves folder.".into())
                                .map_err(|e| e.to_string())
                        })
                        .unwrap_or_else(|e| e);
                }
                if ui.button("Quit game").clicked() {
                    link.close();
                }
            }
        }
    }
    pub fn paint(
        &mut self,
        gpu: &Gpu,
        window: &Window,
        view: &wgpu::TextureView,
        encoder: &mut wgpu::CommandEncoder,
    ) {
        let Some(output) = self.output.take() else {
            return;
        };
        self.state
            .as_mut()
            .unwrap()
            .handle_platform_output(window, output.platform_output);
        for (id, deltas) in &output.textures_delta.set {
            for delta in deltas {
                self.renderer.update_texture(&gpu.device, &gpu.queue, *id, delta);
            }
        }
        let jobs = self.ctx.tessellate(output.shapes, output.pixels_per_point);
        let size = window.inner_size();
        let screen = egui_wgpu::ScreenDescriptor {
            size_in_pixels: [size.width.max(1), size.height.max(1)],
            pixels_per_point: output.pixels_per_point,
        };
        let buffers =
            self.renderer
                .update_buffers(&gpu.device, &gpu.queue, encoder, &jobs, &screen);
        // This UI has no custom paint callbacks, so it never returns auxiliary command buffers.
        debug_assert!(buffers.is_empty());
        {
            let pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("F10 overlay"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
                    resolve_target: None,
                    depth_slice: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            });
            self.renderer
                .render(&mut pass.forget_lifetime(), &jobs, &screen);
        }
        for id in &output.textures_delta.free {
            self.renderer.free_texture(id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    /// Opt-in visual proof using the real overlay UI, egui tessellation and wgpu renderer, with no ROM.
    #[test]
    #[ignore = "writes menu screenshots; set MKDS_F10_SHOTS and run explicitly"]
    fn render_menu_tabs() {
        let gpu = Gpu::headless().unwrap();
        let dir = PathBuf::from(std::env::var_os("MKDS_F10_SHOTS").expect("MKDS_F10_SHOTS"));
        std::fs::create_dir_all(&dir).unwrap();
        let ctx = egui::Context::default();
        ctx.set_visuals(egui::Visuals::dark());
        let settings = Settings::default();
        let controls = Controls::default();
        let mut menu = Menu {
            widescreen_mod: false,
            open: true,
            pause: false,
            pause_choice: 0,
            pause_was: false,
            quit: false,
            pad_held: Vec::new(),
            settings: settings.clone(),
            saved_settings: settings,
            controls: controls.clone(),
            saved_controls: controls,
            controls_path: None,
            ctx: ctx.clone(),
            state: None,
            renderer: egui_wgpu::Renderer::new(
                &gpu.device,
                wgpu::TextureFormat::Rgba8Unorm,
                egui_wgpu::RendererOptions::default(),
            ),
            output: None,
            tab: 0,
            capture: None,
            wizard: false,
            armed: false,
            pads: Pads::none(),
            status: String::new(),
            fps_start: Instant::now(),
            frames: 0,
            fps: 0.0,
        };
        let link = HostLink::for_tests();
        {
            // The pause menu (Esc), over a plain background.
            menu.pause = true;
            menu.open = false;
            let mut output = None;
            for i in 0..30 {
                // Enough passes for the window's fade-in to finish.
                let raw = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(800.0, 700.0))), time: Some(i as f64 * 0.05), ..Default::default() };
                let current = ctx.run_pass(raw, |ctx| menu.pause_window(ctx));
                for (id, deltas) in &current.textures_delta.set {
                    for delta in deltas {
                        menu.renderer.update_texture(&gpu.device, &gpu.queue, *id, delta);
                    }
                }
                output = Some(current);
            }
            shot(&gpu, &mut menu.renderer, &ctx, output.unwrap(), &dir.join("pause.png"));
            menu.pause = false;
            menu.open = true;
        }
        for (tab, name) in [
            "graphics",
            "performance",
            "controls",
            "controller",
            "audio",
            "system",
        ]
        .iter()
        .enumerate()
        {
            menu.tab = tab;
            let mut output = None;
            for _ in 0..2 {
                let current = ctx.run_ui(egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(800.0,700.0))), ..Default::default() }, |ui| {
                    egui::CentralPanel::default().show(ui, |ui| {
                        ui.heading("MKDSrust • F10 settings");
                        ui.horizontal(|ui| { for (i,label) in ["Graphics","Performance","Controls","Controller","Audio","System"].iter().enumerate() { let _ = ui.selectable_label(tab==i,*label); } });
                        ui.separator(); menu.page(ui, &link);
                        ui.separator(); ui.label("Changes apply immediately and save automatically. F10 returns to game.");
                    });
                });
                // First layout can be discarded, but texture updates must always be uploaded.
                for (id, deltas) in &current.textures_delta.set {
                    for delta in deltas {
                        menu.renderer.update_texture(&gpu.device, &gpu.queue, *id, delta);
                    }
                }
                output = Some(current);
            }
            let output = output.unwrap();
            let jobs = ctx.tessellate(output.shapes, output.pixels_per_point);
            let texture = gpu.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("ROM-free F10 screenshot"),
                size: wgpu::Extent3d {
                    width: 800,
                    height: 700,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            });
            let mut encoder = gpu.device.create_command_encoder(&Default::default());
            let screen = egui_wgpu::ScreenDescriptor {
                size_in_pixels: [800, 700],
                pixels_per_point: output.pixels_per_point,
            };
            assert!(
                menu.renderer
                    .update_buffers(&gpu.device, &gpu.queue, &mut encoder, &jobs, &screen)
                    .is_empty()
            );
            {
                let view = texture.create_view(&Default::default());
                let pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &view,
                        resolve_target: None,
                        depth_slice: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    ..Default::default()
                });
                menu.renderer
                    .render(&mut pass.forget_lifetime(), &jobs, &screen);
            }
            gpu.queue.submit([encoder.finish()]);
            let pixels = gpu.read_rgba(&texture);
            assert!(
                pixels.windows(2).any(|w| w[0] != w[1]),
                "menu must contain rendered content"
            );
            let bytes: Vec<u8> = pixels.into_iter().flat_map(u32::to_be_bytes).collect();
            let file = std::fs::File::create(dir.join(format!("f10_{name}.png"))).unwrap();
            let mut png = png::Encoder::new(file, 800, 700);
            png.set_color(png::ColorType::Rgba);
            png.set_depth(png::BitDepth::Eight);
            png.write_header()
                .unwrap()
                .write_image_data(&bytes)
                .unwrap();
        }
    }

    /// Renders one egui output at 800x700 into a PNG (tests only).
    fn shot(gpu: &Gpu, renderer: &mut egui_wgpu::Renderer, ctx: &egui::Context, output: egui::FullOutput, path: &std::path::Path) {
        let jobs = ctx.tessellate(output.shapes, output.pixels_per_point);
        let texture = gpu.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("ROM-free overlay screenshot"),
            size: wgpu::Extent3d { width: 800, height: 700, depth_or_array_layers: 1 },
            mip_level_count: 1, sample_count: 1, dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let mut encoder = gpu.device.create_command_encoder(&Default::default());
        let screen = egui_wgpu::ScreenDescriptor { size_in_pixels: [800, 700], pixels_per_point: output.pixels_per_point };
        assert!(renderer.update_buffers(&gpu.device, &gpu.queue, &mut encoder, &jobs, &screen).is_empty());
        {
            let view = texture.create_view(&Default::default());
            let pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view, resolve_target: None, depth_slice: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color { r: 0.2, g: 0.35, b: 0.6, a: 1.0 }), store: wgpu::StoreOp::Store },
                })],
                ..Default::default()
            });
            renderer.render(&mut pass.forget_lifetime(), &jobs, &screen);
        }
        gpu.queue.submit([encoder.finish()]);
        let bytes: Vec<u8> = gpu.read_rgba(&texture).into_iter().flat_map(u32::to_be_bytes).collect();
        let mut png = png::Encoder::new(std::fs::File::create(path).unwrap(), 800, 700);
        png.set_color(png::ColorType::Rgba);
        png.set_depth(png::BitDepth::Eight);
        png.write_header().unwrap().write_image_data(&bytes).unwrap();
    }
}
