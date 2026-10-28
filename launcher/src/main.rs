#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
use eframe::egui::{self, Color32, RichText};
use mkds_builder::{
    build,
    controls::{self, Controls},
    rom::{self, RomInfo},
    settings::{Layout, Renderer, Settings},
};
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver},
    time::{Duration, SystemTime},
};

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct Preferences {
    rom: PathBuf,
    output: PathBuf,
    accuracy: bool,
    fingerprint: String,
}
enum Event {
    Inspected(Result<RomInfo, String>, String),
    Log(String),
    Built(Result<PathBuf, String>, String),
    Played(Result<i32, String>),
}
struct Launcher {
    branding: egui::TextureHandle,
    icon: egui::TextureHandle,
    prefs: Preferences,
    dir: PathBuf,
    settings: Settings,
    controls: Controls,
    page: usize,
    info: Option<RomInfo>,
    title: String,
    message: String,
    rx: Option<Receiver<Event>>,
    busy: bool,
    log: String,
    capture: Option<usize>,
    source_stamp: String,
    smoke_frame: Option<u8>,
}
fn source() -> Option<PathBuf> {
    std::env::current_dir()
        .ok()
        .and_then(|p| build::find_source(&p))
        .or_else(|| {
            std::env::current_exe()
                .ok()
                .and_then(|p| build::find_source(&p))
        })
        .or_else(|| build::find_source(Path::new(env!("CARGO_MANIFEST_DIR"))))
}
// Only source metadata/timestamps are inspected; no game assets are read for the UI.
fn fingerprint() -> String {
    fn walk(p: &Path, latest: &mut u128) {
        if let Ok(entries) = std::fs::read_dir(p) {
            for e in entries.flatten() {
                let p = e.path();
                if p.is_dir() {
                    if !matches!(e.file_name().to_str(), Some("target" | ".git" | "assets")) {
                        walk(&p, latest);
                    }
                } else if matches!(
                    p.extension().and_then(|x| x.to_str()),
                    Some("rs" | "toml" | "lock" | "csv")
                ) {
                    if let Ok(t) = e.metadata().and_then(|m| m.modified()) {
                        *latest = (*latest).max(
                            t.duration_since(SystemTime::UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_nanos(),
                        );
                    }
                }
            }
        }
    }
    let mut latest = 0;
    if let Some(s) = source() {
        for name in ["1_build_tool/src", "2_native_game", "dictionary"] {
            walk(&s.join(name), &mut latest);
        }
    }
    latest.to_string()
}
impl Launcher {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        cc.egui_ctx.set_visuals(egui::Visuals::dark());
        let mut style = (*cc.egui_ctx.style()).clone();
        style.spacing.item_spacing = egui::vec2(12., 12.);
        style.visuals.selection.bg_fill = Color32::from_rgb(25, 125, 145);
        cc.egui_ctx.set_style(style);
        let dir = controls::default_path()
            .and_then(|p| p.parent().map(Path::to_path_buf))
            .unwrap_or_else(|| PathBuf::from("."));
        let mut errors = Vec::new();
        let prefs = match std::fs::read(dir.join("launcher.json")) {
            Ok(b) => serde_json::from_slice(&b).unwrap_or_else(|e| {
                errors.push(format!("Launcher preferences: {e}"));
                Preferences::default()
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Preferences::default(),
            Err(e) => {
                errors.push(e.to_string());
                Preferences::default()
            }
        };
        let settings = Settings::load_or_create(&dir.join("settings.txt")).unwrap_or_else(|e| {
            errors.push(e);
            Settings::default()
        });
        let controls = Controls::load_or_create(&dir.join("controls.txt")).unwrap_or_else(|e| {
            errors.push(e);
            Controls::default()
        });
        let log = std::fs::read_to_string(dir.join("launcher-build.log")).unwrap_or_default();
        let load_texture = |name, bytes: &[u8]| {
            let image = eframe::icon_data::from_png_bytes(bytes).expect("valid embedded branding PNG");
            cc.egui_ctx.load_texture(
                name,
                egui::ColorImage::from_rgba_unmultiplied(
                    [image.width as usize, image.height as usize], &image.rgba),
                egui::TextureOptions::LINEAR,
            )
        };
        let mut app = Self {
            branding: load_texture("MKDSrust wordmark", include_bytes!("../../assets/branding/logo_dark.png")),
            icon: load_texture("MKDSrust icon", include_bytes!("../../assets/branding/icon.png")),
            prefs,
            dir,
            settings,
            controls,
            page: 0,
            info: None,
            title: String::new(),
            message: errors.join("\n"),
            rx: None,
            busy: false,
            log,
            capture: None,
            source_stamp: fingerprint(),
            smoke_frame: std::env::args().any(|a| a == "--smoke-test").then_some(0),
        };
        if let Some(bundle) = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(Path::to_path_buf))
            .filter(|p| p.join("manifest.json").is_file())
        {
            app.prefs.output = bundle;
        }
        if !app.prefs.rom.as_os_str().is_empty() {
            app.inspect(app.prefs.rom.clone());
        }
        app
    }
    fn save(&mut self) {
        let result = (|| -> Result<(), String> {
            std::fs::create_dir_all(&self.dir).map_err(|e| e.to_string())?;
            std::fs::write(
                self.dir.join("launcher.json"),
                serde_json::to_vec_pretty(&self.prefs).map_err(|e| e.to_string())?,
            )
            .map_err(|e| e.to_string())
        })();
        if let Err(e) = result {
            self.message = e;
        }
    }
    fn inspect(&mut self, path: PathBuf) {
        self.info = None;
        self.title.clear();
        self.busy = true;
        let (tx, rx) = mpsc::channel();
        self.rx = Some(rx);
        std::thread::spawn(move || {
            let result = rom::inspect_rom(&path).map_err(|e| e.to_string());
            // Header title is displayed only in memory, never bundled or committed.
            let title = std::fs::File::open(&path)
                .and_then(|mut f| {
                    use std::io::Read;
                    let mut b = [0; 12];
                    f.read_exact(&mut b)?;
                    Ok(String::from_utf8_lossy(&b)
                        .trim_end_matches('\0')
                        .to_owned())
                })
                .unwrap_or_default();
            let _ = tx.send(Event::Inspected(result, title));
        });
    }
    fn poll(&mut self) {
        let events: Vec<_> = self
            .rx
            .as_ref()
            .map(|r| r.try_iter().collect())
            .unwrap_or_default();
        for event in events {
            match event {
                Event::Log(line) => {
                    self.log.push_str(&line);
                    self.log.push('\n');
                }
                Event::Inspected(result, title) => {
                    self.busy = false;
                    match result {
                        Ok(info) => {
                            self.prefs.rom = PathBuf::from(&info.path);
                            if self.prefs.output.as_os_str().is_empty() {
                                self.prefs.output = build::default_output(&info);
                            }
                            self.title = title;
                            self.info = Some(info);
                            self.save();
                        }
                        Err(e) => self.message = e,
                    }
                }
                Event::Built(result, stamp) => {
                    self.busy = false;
                    match result {
                        Ok(p) => {
                            self.prefs.output = p;
                            self.prefs.fingerprint = stamp;
                            self.message = "Build complete. Ready to play.".into();
                            self.save();
                        }
                        Err(e) => {
                            self.log.push_str(&format!("\nBuild failed: {e}\n"));
                            self.message = e;
                        }
                    }
                    if let Err(e) = std::fs::write(self.dir.join("launcher-build.log"), &self.log) {
                        self.message = format!("Could not save build log: {e}");
                    }
                }
                Event::Played(result) => {
                    self.busy = false;
                    self.message = match result {
                        Ok(0) => "Game closed.".into(),
                        Ok(n) => format!("Game exited with code {n}."),
                        Err(e) => e,
                    };
                }
            }
        }
    }
    fn start_build(&mut self) {
        let Some(info) = self.info.clone() else {
            return;
        };
        let Some(source) = source() else {
            self.message =
                "Source checkout not found. Run the launcher from the project checkout.".into();
            return;
        };
        let mut output = self.prefs.output.clone();
        if output.exists() && std::fs::read_dir(&output).map_or(true, |mut i| i.next().is_some()) {
            let suffix = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            output = output.with_file_name(format!(
                "{}-{suffix}",
                output.file_name().unwrap_or_default().to_string_lossy()
            ));
        }
        let stamp = fingerprint();
        self.source_stamp = stamp.clone();
        self.log = format!(
            "Building into {}\nPrevious builds are preserved.\n",
            output.display()
        );
        self.busy = true;
        self.message.clear();
        let (tx, rx) = mpsc::channel();
        self.rx = Some(rx);
        std::thread::spawn(move || {
            let opts = build::RecompileOptions {
                source,
                output,
                allow_unverified: false,
                offline: false,
                allow_incomplete: false,
                keep_source: false,
                with_acceptance: false,
                jobs: None,
                cache_dir: Some(build::default_cache_dir()),
                with_interpreter: false,
            };
            let result = std::panic::catch_unwind(|| {
                build::recompile(&info, &opts, &mut |s| {
                    let _ = tx.send(Event::Log(s));
                })
            })
            .map_err(|_| "Build worker panicked.".to_string())
            .and_then(|r| r.map_err(|e| e.to_string()));
            let _ = tx.send(Event::Built(result, stamp));
        });
    }
    fn play(&mut self) {
        self.busy = true;
        let output = self.prefs.output.clone();
        let accuracy = self.prefs.accuracy;
        let (tx, rx) = mpsc::channel();
        self.rx = Some(rx);
        std::thread::spawn(move || {
            let env: Vec<_> = ["MKDS_HW_FS", "MKDS_HW_ARM7", "MKDS_HW_TIME", "MKDS_HW_SND"]
                .into_iter()
                .filter_map(|k| accuracy.then_some((k, Some("exact"))))
                .collect();
            let _ = tx.send(Event::Played(
                build::launch_with_env(&output, &[], &env).map_err(|e| e.to_string()),
            ));
        });
    }
    fn home(&mut self, ui: &mut egui::Ui) {
        let width = ui.available_width().min(480.);
        ui.add(egui::Image::new(&self.branding).fit_to_exact_size(egui::vec2(width, width / 4.)));
        ui.heading("Your game, on your desktop");
        ui.label("Choose your own ROM, build locally, then play.");
        ui.separator();
        let built = std::fs::read(self.prefs.output.join("manifest.json"))
            .ok()
            .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
            .is_some_and(|m| {
                m["executable"]
                    .as_str()
                    .is_some_and(|e| self.prefs.output.join(e).is_file())
            });
        let outdated = built
            && (self.prefs.fingerprint != self.source_stamp
                || self.info.as_ref().is_some_and(|i| {
                    std::fs::read(self.prefs.output.join("manifest.json"))
                        .ok()
                        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
                        .is_none_or(|m| m["rom"]["sha256"] != i.sha256)
                }));
        ui.label(
            RichText::new(if outdated {
                "OUTDATED - rebuild recommended"
            } else if built {
                "BUILT - ready to play"
            } else {
                "NOT BUILT - complete setup below"
            })
            .color(Color32::from_rgb(95, 210, 205)),
        );
        if ui
            .add_enabled(
                built && !self.busy,
                egui::Button::new(RichText::new("Play").size(30.)).min_size(egui::vec2(240., 78.)),
            )
            .clicked()
        {
            self.play();
        }
        ui.add_enabled_ui(!self.busy, |ui| {
            if ui.button("1. Choose .nds ROM").clicked() {
                if let Some(p) = rfd::FileDialog::new()
                    .add_filter("ROM", &["nds"])
                    .pick_file()
                {
                    self.inspect(p);
                }
            }
            if ui.button("Refresh status").clicked() {
                self.source_stamp = fingerprint();
                if !self.prefs.rom.as_os_str().is_empty() {
                    self.inspect(self.prefs.rom.clone());
                }
            }
            ui.label(self.prefs.rom.display().to_string());
            if let Some(info) = &self.info {
                ui.label(format!(
                    "Title: {}  |  Code: {}  |  Revision: {}",
                    self.title, info.game_code, info.revision
                ));
                let region = match info.game_code.chars().last() {
                    Some('E') => "North America",
                    Some('P') => "Europe",
                    Some('J') => "Japan",
                    _ => "Other / unknown",
                };
                ui.label(format!(
                    "Region: {region}  |  Supported dump: {}",
                    if info.reference_match { "yes" } else { "no" }
                ));
                ui.label(&info.compatibility);
            }
            if ui.button("2. Choose output folder").clicked() {
                if let Some(p) = rfd::FileDialog::new().pick_folder() {
                    self.prefs.output = p;
                    self.save();
                }
            }
            ui.label(self.prefs.output.display().to_string());
            ui.label("Rebuilds use a new sibling folder when the output is occupied.");
            if ui
                .add_enabled(
                    self.info.as_ref().is_some_and(|i| i.reference_match),
                    egui::Button::new(if built { "Rebuild" } else { "3. Build game" }),
                )
                .clicked()
            {
                self.start_build();
            }
            if ui
                .add_enabled(
                    self.prefs.output.is_dir(),
                    egui::Button::new("Open game folder"),
                )
                .clicked()
            {
                if let Err(e) = std::process::Command::new("explorer.exe")
                    .arg(&self.prefs.output)
                    .spawn()
                {
                    self.message = e.to_string();
                }
            }
        });
        self.log_panel(ui);
    }
    fn log_panel(&self, ui: &mut egui::Ui) {
        ui.separator();
        ui.label("Last build log");
        egui::ScrollArea::vertical()
            .id_salt("build_log")
            .max_height(260.)
            .stick_to_bottom(true)
            .show(ui, |ui| {
                ui.add(egui::Label::new(RichText::new(&self.log).monospace()).wrap());
            });
    }
    fn settings_page(&mut self, ui: &mut egui::Ui) {
        if ui.button("Open save folder").clicked() {
            if let Some(folder) = mkds_builder::settings::save_folder() {
                let result = std::fs::create_dir_all(&folder).and_then(|_| std::process::Command::new("explorer.exe").arg(&folder).spawn().map(|_| ()));
                if let Err(e) = result { self.message = e.to_string(); }
            }
        }
        ui.heading("Display settings");
        ui.label("Shared with the terminal launcher and game. Save before launching.");
        ui.horizontal(|ui| {
            ui.label("Renderer");
            ui.selectable_value(&mut self.settings.renderer, Renderer::Gpu, "GPU");
            ui.selectable_value(&mut self.settings.renderer, Renderer::Software, "Software");
        });
        egui::ComboBox::from_label("Internal scale")
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
        ui.horizontal(|ui| {
            ui.label("Layout");
            ui.selectable_value(&mut self.settings.layout, Layout::Stacked, "Stacked");
            ui.selectable_value(
                &mut self.settings.layout,
                Layout::SideBySide,
                "Side by side",
            );
        });
        ui.checkbox(&mut self.settings.smooth, "Smooth filtering");
        ui.checkbox(&mut self.settings.fullscreen, "Start fullscreen");
        folder(ui, "Texture pack", &mut self.settings.texture_pack);
        folder(ui, "Dump textures", &mut self.settings.dump_textures);
        if ui.button("Save display settings").clicked() {
            self.message = self
                .settings
                .save(&self.dir.join("settings.txt"))
                .map_or_else(|e| e, |_| "Display settings saved.".into());
        }
    }
    fn controls_page(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.heading("Controls");
        ui.label("Capture replaces a keyboard binding. Use the menus to add multiple keys or controller bindings.");
        if let Some(action) = self.capture {
            ui.label(format!(
                "Press a key for {} (or click Cancel).",
                controls::ACTIONS[action]
            ));
            if ui.button("Cancel capture").clicked() {
                self.capture = None;
            }
            for event in ctx.input(|i| i.events.clone()) {
                if let egui::Event::Key {
                    key,
                    physical_key,
                    pressed: true,
                    repeat: false,
                    ..
                } = event
                {
                    if let Some(index) = map_key(physical_key.unwrap_or(key)) {
                        self.controls.keys[action] = vec![index];
                        self.capture = None;
                    } else {
                        self.message = "Use the key menu for this key.".into();
                    }
                }
            }
        }
        egui::Grid::new("bindings").striped(true).show(ui, |ui| {
            ui.strong("Action");
            ui.strong("Keyboard");
            ui.strong("Controller");
            ui.end_row();
            for a in 0..controls::ACTIONS.len() {
                ui.label(format!("{}  {}", controls::ACTIONS[a], controls::ROLES[a]));
                ui.horizontal(|ui| {
                    if ui.button(self.controls.describe(a)).clicked() {
                        self.capture = Some(a);
                    }
                    ui.menu_button("Edit", |ui| {
                        egui::ScrollArea::vertical()
                            .max_height(300.)
                            .show(ui, |ui| {
                                for (k, name) in controls::KEYS.iter().enumerate() {
                                    toggle_binding(ui, name, k, &mut self.controls.keys[a]);
                                }
                            });
                    });
                });
                ui.menu_button(self.controls.describe_pad(a), |ui| {
                    for (k, name) in controls::PAD.iter().enumerate() {
                        toggle_binding(ui, name, k, &mut self.controls.pad[a]);
                    }
                });
                ui.end_row();
            }
        });
        if ui.button("Save controls").clicked() {
            self.message = self
                .controls
                .save(&self.dir.join("controls.txt"))
                .map_or_else(|e| e, |_| "Controls saved.".into());
        }
        if ui
            .button("Reset controls to defaults (save to apply)")
            .clicked()
        {
            self.controls = Controls::default();
        }
    }
}
fn toggle_binding(ui: &mut egui::Ui, name: &str, k: usize, values: &mut Vec<usize>) {
    let mut on = values.contains(&k);
    if ui.checkbox(&mut on, name).changed() {
        if on {
            values.push(k);
        } else {
            values.retain(|&v| v != k);
        }
    }
}
fn map_key(key: egui::Key) -> Option<usize> {
    let name = format!("{key:?}");
    let mapped = match name.as_str() {
        "ArrowDown" => "Down",
        "ArrowUp" => "Up",
        "ArrowLeft" => "Left",
        "ArrowRight" => "Right",
        "Num0" => "Key0",
        "Num1" => "Key1",
        "Num2" => "Key2",
        "Num3" => "Key3",
        "Num4" => "Key4",
        "Num5" => "Key5",
        "Num6" => "Key6",
        "Num7" => "Key7",
        "Num8" => "Key8",
        "Num9" => "Key9",
        "Equals" => "Equal",
        other => other,
    };
    controls::key_index(mapped)
}
fn folder(ui: &mut egui::Ui, label: &str, value: &mut Option<PathBuf>) {
    ui.horizontal(|ui| {
        ui.label(label);
        ui.label(
            value
                .as_ref()
                .map_or("Disabled".into(), |p| p.display().to_string()),
        );
        if ui.button("Browse").clicked() {
            if let Some(p) = rfd::FileDialog::new().pick_folder() {
                *value = Some(p);
            }
        }
        if ui.button("Clear").clicked() {
            *value = None;
        }
    });
}
impl Launcher {
    fn draw(&mut self, ctx: &egui::Context) {
        self.poll();
        ctx.request_repaint_after(Duration::from_millis(100));
        egui::SidePanel::left("navigation")
            .exact_width(175.)
            .show(ctx, |ui| {
                ui.add_space(22.);
                ui.add(egui::Image::new(&self.icon).fit_to_exact_size(egui::vec2(64., 64.)));
                ui.heading("MKDSrust");
                ui.label("MKDSrust desktop launcher");
                ui.add_space(30.);
                for (i, title) in ["Home", "Settings", "Controls", "Advanced"]
                    .iter()
                    .enumerate()
                {
                    if ui.selectable_value(&mut self.page, i, *title).clicked() {
                        self.capture = None;
                    }
                }
            });
        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            ui.horizontal(|ui| {
                if self.busy {
                    ui.spinner();
                    ui.label("Working - keep this window open.");
                }
                ui.label(&self.message);
            });
        });
        egui::CentralPanel::default().show(ctx, |ui| { egui::ScrollArea::vertical().show(ui, |ui| {
            ui.add_space(18.);
            match self.page { 0 => self.home(ui), 1 => self.settings_page(ui), 2 => self.controls_page(ui, ctx), _ => {
                ui.heading("Advanced"); let changed = ui.checkbox(&mut self.prefs.accuracy, "Accuracy mode (exact hardware)").changed(); if changed { self.save(); } self.log_panel(ui);
            }}
        }); });
        if self.busy && ctx.input(|i| i.viewport().close_requested()) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.message = "Wait for the build or game to finish before closing.".into();
        }
    }
}
impl eframe::App for Launcher {
    fn update(&mut self, ctx: &egui::Context, _: &mut eframe::Frame) {
        self.draw(ctx);
        if let Some(frame) = &mut self.smoke_frame {
            *frame += 1;
            self.page = (*frame as usize / 2).min(3);
            if *frame >= 8 {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
    }
}
fn main() -> eframe::Result<()> {
    eframe::run_native(
        "MKDSrust",
        eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default()
                .with_icon(eframe::icon_data::from_png_bytes(include_bytes!("../../assets/branding/icon.png"))
                    .expect("valid embedded window icon"))
                .with_inner_size([1020., 820.])
                .with_min_inner_size([800., 650.]),
            ..Default::default()
        },
        Box::new(|cc| Ok(Box::new(Launcher::new(cc)))),
    )
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn physical_key_names_match_builder() {
        assert_eq!(map_key(egui::Key::ArrowLeft), controls::key_index("Left"));
        assert_eq!(map_key(egui::Key::Num5), controls::key_index("Key5"));
        assert_eq!(map_key(egui::Key::Z), controls::key_index("Z"));
    }
    #[test]
    fn accuracy_defaults_off() {
        let p: Preferences = serde_json::from_str("{}").unwrap();
        assert!(!p.accuracy);
    }

    #[test]
    fn all_pages_render_and_capture_saves_shared_controls() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = egui::Context::default();
        let mut app = Launcher {
            prefs: Preferences::default(),
            dir: dir.path().into(),
            settings: Settings::default(),
            controls: Controls::default(),
            page: 0,
            info: None,
            title: String::new(),
            message: String::new(),
            rx: None,
            busy: false,
            log: "Build output".into(),
            capture: None,
            source_stamp: String::new(),
            smoke_frame: None,
        };
        for page in 0..4 {
            app.page = page;
            let output = ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1020., 820.),
                    )),
                    ..Default::default()
                },
                |ctx| app.draw(ctx),
            );
            assert!(!output.shapes.is_empty());
        }
        app.page = 2;
        app.capture = Some(0);
        let _ = ctx.run(
            egui::RawInput {
                events: vec![egui::Event::Key {
                    key: egui::Key::Q,
                    physical_key: Some(egui::Key::Q),
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::NONE,
                }],
                ..Default::default()
            },
            |ctx| app.draw(ctx),
        );
        assert_eq!(app.controls.describe(0), "Q");
        app.controls.save(&dir.path().join("controls.txt")).unwrap();
        assert_eq!(
            Controls::load_or_create(&dir.path().join("controls.txt"))
                .unwrap()
                .describe(0),
            "Q"
        );
    }

    #[test]
    fn child_environment_probe() {
        if let Some(path) = std::env::var_os("LAUNCHER_TEST_PROBE") {
            let values: Vec<_> = ["MKDS_HW_FS", "MKDS_HW_ARM7", "MKDS_HW_TIME", "MKDS_HW_SND"]
                .map(|k| std::env::var(k).unwrap_or_default())
                .into();
            std::fs::write(path, values.join(",")).unwrap();
        }
    }

    #[test]
    fn launch_overrides_only_child_environment() {
        let dir = tempfile::tempdir().unwrap();
        let exe = if cfg!(windows) { "probe.exe" } else { "probe" };
        std::fs::copy(std::env::current_exe().unwrap(), dir.path().join(exe)).unwrap();
        let manifest = serde_json::json!({"rom": {"path": "unused"}, "executable": exe, "launch_args": ["--exact", "tests::child_environment_probe", "--nocapture"]});
        std::fs::write(dir.path().join("manifest.json"), manifest.to_string()).unwrap();
        let output = dir.path().join("result.txt");
        let output = output.to_str().unwrap();
        let before = std::env::var_os("MKDS_HW_FS");
        assert_eq!(
            build::launch_with_env(
                dir.path(),
                &[],
                &[
                    ("LAUNCHER_TEST_PROBE", Some(output)),
                    ("MKDS_HW_FS", Some("native")),
                    ("MKDS_HW_ARM7", None),
                    ("MKDS_HW_TIME", Some("native")),
                    ("MKDS_HW_SND", Some("exact")),
                ]
            )
            .unwrap(),
            0
        );
        assert_eq!(std::fs::read_to_string(output).unwrap(), "native,,native,exact");
        assert_eq!(std::env::var_os("MKDS_HW_FS"), before);
    }
}
