#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
mod portable;
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
    system_toolchain: bool,
    setup_done: bool,
    last_build: String,
    verbose: bool,
}
fn build_date(stored: &str) -> String {
    // Old launcher versions stored Debug(SystemTime). Preserve a readable migration.
    let seconds = stored.parse::<i64>().ok().or_else(|| stored.split("tv_sec: ").nth(1).and_then(|s| s.split(',').next()).and_then(|s| s.parse().ok()));
    seconds.and_then(mkds_builder::modpkg::format_timestamp).unwrap_or_else(|| "Unknown (rebuild to update)".into())
}
enum Event {
    ModInstalled(Result<String, String>),
    Inspected(Result<RomInfo, String>, String),
    Log(String),
    Built(Result<PathBuf, String>, String),
    Played(Result<i32, String>),
    Installed(Result<(), String>),
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
    wizard: usize,
}
fn fingerprint() -> String {
    use sha2::{Digest, Sha256};
    mkds_builder::rom::hex(&Sha256::digest(include_bytes!(concat!(
        env!("OUT_DIR"),
        "/source.tar.gz"
    ))))
}
const TEAL: Color32 = Color32::from_rgb(86, 210, 193);
fn card() -> egui::Frame {
    egui::Frame::none()
        .fill(Color32::from_rgb(39, 48, 57))
        .rounding(16.)
        .inner_margin(22.)
        .stroke(egui::Stroke::new(1., Color32::from_rgb(57, 69, 79)))
        .shadow(egui::epaint::Shadow {
            offset: egui::vec2(0., 4.),
            blur: 12.,
            spread: 0.,
            color: Color32::from_black_alpha(35),
        })
}
fn glyph(ui: &mut egui::Ui, kind: usize, color: Color32) {
    let (r, _) = ui.allocate_exact_size(egui::vec2(20., 22.), egui::Sense::hover());
    let p = ui.painter();
    let c = r.center();
    let stroke = egui::Stroke::new(1.7, color);
    if kind == 0 {
        p.add(egui::Shape::convex_polygon(
            vec![
                c + egui::vec2(-5., -7.),
                c + egui::vec2(7., 0.),
                c + egui::vec2(-5., 7.),
            ],
            color,
            egui::Stroke::NONE,
        ));
    } else {
        match kind {
            1 => {
                p.circle_stroke(c, 7., stroke);
                p.circle_stroke(c, 2.5, stroke);
                for d in [egui::vec2(0., 9.), egui::vec2(9., 0.)] {
                    p.line_segment([c - d, c + d], stroke);
                }
            }
            2 => {
                p.rect_stroke(r.shrink(2.), 5., stroke);
                p.line_segment([c + egui::vec2(-6., 0.), c], stroke);
                p.line_segment([c + egui::vec2(-3., -3.), c + egui::vec2(-3., 3.)], stroke);
                p.circle_filled(c + egui::vec2(5., 0.), 2., color);
            }
            3 => {
                p.rect_stroke(r.shrink(3.), 2., stroke);
                p.line_segment([c + egui::vec2(-5., 0.), c + egui::vec2(5., 0.)], stroke);
                p.line_segment([c + egui::vec2(0., -5.), c + egui::vec2(0., 5.)], stroke);
            }
            4 => {
                p.line_segment([c + egui::vec2(-6., -6.), c], stroke);
                p.line_segment([c, c + egui::vec2(-6., 6.)], stroke);
                p.line_segment([c + egui::vec2(2., 6.), c + egui::vec2(7., 6.)], stroke);
            }
            5 => {
                p.circle_stroke(c, 8., stroke);
                p.circle_filled(c + egui::vec2(0., -4.), 1., color);
                p.line_segment([c, c + egui::vec2(0., 5.)], stroke);
            }
            _ => {
                p.circle_stroke(c, 8., stroke);
                p.line_segment([c + egui::vec2(-4., 0.), c + egui::vec2(-1., 4.)], stroke);
                p.line_segment([c + egui::vec2(-1., 4.), c + egui::vec2(5., -4.)], stroke);
            }
        }
    }
}
impl Launcher {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        cc.egui_ctx.set_theme(egui::Theme::Dark);
        cc.egui_ctx.set_visuals(egui::Visuals::dark());
        let mut style = (*cc.egui_ctx.style()).clone();
        style.spacing.item_spacing = egui::vec2(12., 12.);
        style.spacing.button_padding = egui::vec2(18., 10.);
        style
            .text_styles
            .insert(egui::TextStyle::Small, egui::FontId::proportional(12.));
        style
            .text_styles
            .insert(egui::TextStyle::Body, egui::FontId::proportional(16.));
        style
            .text_styles
            .insert(egui::TextStyle::Button, egui::FontId::proportional(16.));
        style
            .text_styles
            .insert(egui::TextStyle::Heading, egui::FontId::proportional(28.));
        style.visuals.panel_fill = Color32::from_rgb(25, 32, 39);
        style.visuals.override_text_color = Some(Color32::from_rgb(226, 230, 233));
        style.visuals.window_fill = Color32::from_rgb(25, 32, 39);
        style.visuals.selection.bg_fill = Color32::from_rgb(206, 66, 43);
        style.visuals.widgets.hovered.bg_fill = Color32::from_rgb(240, 114, 65);
        style.visuals.widgets.inactive.rounding = egui::Rounding::same(9.);
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
            let image =
                eframe::icon_data::from_png_bytes(bytes).expect("valid embedded branding PNG");
            cc.egui_ctx.load_texture(
                name,
                egui::ColorImage::from_rgba_unmultiplied(
                    [image.width as usize, image.height as usize],
                    &image.rgba,
                ),
                egui::TextureOptions::LINEAR,
            )
        };
        let mut app = Self {
            branding: load_texture(
                "MKDSrust wordmark",
                include_bytes!("../../assets/branding/logo_dark.png"),
            ),
            icon: load_texture(
                "MKDSrust icon",
                include_bytes!("../../assets/branding/icon.png"),
            ),
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
            wizard: 0,
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
        if !app.prefs.setup_done {
            app.page = 6;
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
                Event::Installed(result) => {
                    self.busy = false;
                    match result {
                        Ok(()) => {
                            self.wizard = 3;
                            self.message = "Toolchain ready. Continue to build your game.".into();
                        }
                        Err(e) => self.message = e,
                    }
                }
                Event::Log(line) => {
                    self.message = line.chars().take(180).collect();
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
                            self.prefs.last_build = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default().as_secs().to_string();
                            self.wizard = 4;
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
                Event::ModInstalled(result) => {
                    self.busy = false;
                    self.message = result.unwrap_or_else(|e| format!("Mod installation failed: {e}"));
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
        let data = self.dir.clone();
        let system = self.prefs.system_toolchain;
        let output = build_folder(&self.prefs.output);
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
            let mut logger = |s| {
                let _ = tx.send(Event::Log(s));
            };
            let prepared = (|| {
                let source = portable::source(&data, &mut logger)?;
                let toolchain = if system {
                    None
                } else {
                    Some(portable::install(&data, &mut logger)?)
                };
                Ok::<_, String>((source, toolchain))
            })();
            let (source, toolchain) = match prepared {
                Ok(p) => p,
                Err(e) => {
                    let _ = tx.send(Event::Built(Err(e), stamp));
                    return;
                }
            };
            let opts = build::RecompileOptions {
                source,
                output,
                allow_unverified: false,
                offline: false,
                allow_incomplete: false,
                keep_source: true,
                with_acceptance: false,
                jobs: None,
                cache_dir: Some(build::default_cache_dir()),
                with_interpreter: false,
            };
            let result = std::panic::catch_unwind(|| {
                build::recompile_with_toolchain(&info, &opts, toolchain.as_ref(), &mut |s| {
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
        let verbose = self.prefs.verbose;
        let (tx, rx) = mpsc::channel();
        self.rx = Some(rx);
        std::thread::spawn(move || {
            let mut env: Vec<_> = ["MKDS_HW_FS", "MKDS_HW_ARM7", "MKDS_HW_TIME", "MKDS_HW_SND"]
                .into_iter()
                .filter_map(|k| accuracy.then_some((k, Some("exact"))))
                .collect();
            env.push(("MKDS_VERBOSE", Some(if verbose { "1" } else { "0" })));
            if accuracy { env.push(("MKDS_NO_MODS", Some("1"))); }
            let _ = tx.send(Event::Played(
                build::launch_with_env(&output, &[], &env).map_err(|e| e.to_string()),
            ));
        });
    }
    fn home(&mut self, ui: &mut egui::Ui) {
        let width = ui.available_width();
        let (hero, _) = ui.allocate_exact_size(egui::vec2(width, 220.), egui::Sense::hover());
        let mut mesh = egui::Mesh::default();
        for (pos, color) in [
            (hero.left_top(), Color32::from_rgb(105, 48, 37)),
            (hero.right_top(), Color32::from_rgb(23, 81, 81)),
            (hero.right_bottom(), Color32::from_rgb(27, 49, 59)),
            (hero.left_bottom(), Color32::from_rgb(49, 37, 40)),
        ] {
            mesh.colored_vertex(pos, color);
        }
        mesh.add_triangle(0, 1, 2);
        mesh.add_triangle(0, 2, 3);
        ui.painter().add(mesh);
        for n in 0..8 {
            let x = hero.right() - n as f32 * 48.;
            ui.painter().line_segment(
                [
                    egui::pos2(x, hero.top()),
                    egui::pos2(x - 110., hero.bottom()),
                ],
                egui::Stroke::new(1., Color32::from_white_alpha(14)),
            );
        }
        ui.allocate_new_ui(egui::UiBuilder::new().max_rect(hero.shrink(24.)), |ui| {
            ui.label(
                RichText::new("BUILT BY YOU. READY TO RACE.")
                    .size(11.)
                    .color(TEAL),
            );
            ui.add(egui::Image::new(&self.branding).fit_to_exact_size(egui::vec2(340., 85.)));
            ui.label(
                RichText::new("Your game, on your desktop.")
                    .size(26.)
                    .strong(),
            );
            ui.label("Bring your ROM. Build locally. Make it yours.");
        });
        ui.add_space(12.);
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
                "NOT BUILT - start Setup to prepare your game"
            })
            .color(Color32::from_rgb(95, 210, 205)),
        );
        let play_response = ui.add_enabled(
            !self.busy,
            egui::Button::new(RichText::new(if built { "▶  Play" } else { "▶  Set up" }).size(30.))
                .fill(Color32::from_rgb(206, 66, 43))
                .rounding(14.)
                .min_size(egui::vec2(300., 88.)),
        );
        if play_response.hovered() {
            ui.painter().rect_stroke(
                play_response.rect.expand(3.),
                16.,
                egui::Stroke::new(3., TEAL),
            );
        }
        if play_response.clicked() {
            if built {
                self.play();
            } else {
                self.page = 6;
                self.wizard = 0;
            }
        }
        ui.add_space(18.);
        let tools = portable::ready(&self.dir) || self.prefs.system_toolchain;
        let saves = mkds_builder::settings::save_folder();
        let states = [
            ("Game", if built { "Ready" } else { "Not built" }, built),
            (
                "Toolchain",
                if tools { "Ready" } else { "Needs setup" },
                tools,
            ),
            (
                "Saves",
                if saves.as_ref().is_some_and(|p| p.is_dir()) {
                    "Available"
                } else {
                    "No saves yet"
                },
                true,
            ),
            (
                "Mode",
                if self.prefs.accuracy {
                    "Accuracy"
                } else {
                    "Native"
                },
                true,
            ),
        ];
        ui.columns(4, |columns| {
            for (i, (name, value, ready)) in states.iter().enumerate() {
                card().inner_margin(14.).show(&mut columns[i], |ui| {
                    ui.set_min_width(ui.available_width());
                    ui.horizontal(|ui| {
                        glyph(ui, i, TEAL);
                        ui.label(*name);
                    });
                    ui.horizontal(|ui| {
                        let (r, _) =
                            ui.allocate_exact_size(egui::vec2(8., 8.), egui::Sense::hover());
                        ui.painter().circle_filled(
                            r.center(),
                            3.,
                            if *ready {
                                TEAL
                            } else {
                                Color32::from_rgb(242, 189, 83)
                            },
                        );
                        ui.strong(*value);
                    });
                });
            }
        });
        ui.add_space(12.);
        card().show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.label(RichText::new("RECENT ACTIVITY").size(11.).color(TEAL));
            ui.label(if self.title.is_empty() {
                "Your locally built game"
            } else {
                &self.title
            });
            ui.label(format!(
                "Last built: {}",
                if self.prefs.last_build.is_empty() {
                    "No builds yet".to_string()
                } else {
                    build_date(&self.prefs.last_build)
                }
            ));
            ui.label(self.prefs.output.display().to_string());
            if ui.button("Set up or rebuild game").clicked() {
                self.page = 6;
                self.wizard = 1;
            }
        });
        ui.horizontal_wrapped(|ui| {
            for (label, path) in [
                ("Game folder", self.prefs.output.clone()),
                ("Saves", saves.unwrap_or_else(|| self.dir.join("saves"))),
            ] {
                if ui
                    .add_enabled(!path.as_os_str().is_empty(), egui::Button::new(label))
                    .clicked()
                {
                    if let Err(e) = std::fs::create_dir_all(&path).and_then(|_| {
                        std::process::Command::new("explorer.exe")
                            .arg(path)
                            .spawn()
                            .map(|_| ())
                    }) {
                        self.message = e.to_string();
                    }
                }
            }
            ui.hyperlink_to("Documentation", "https://docs.mkdsrust.com");
            ui.hyperlink_to("GitHub", "https://github.com/CameronStorer/MKDSrust");
        });
    }
    fn rom_and_build(&mut self, ui: &mut egui::Ui) {
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
                    egui::Button::new("Build game"),
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
        egui::CollapsingHeader::new("Detailed build log").show(ui, |ui| self.log_panel(ui));
    }
    fn log_panel(&self, ui: &mut egui::Ui) {
        ui.separator();
        ui.label("Build activity | downloads and compiler output");
        egui::ScrollArea::vertical()
            .id_salt("build_log")
            .max_height(260.)
            .stick_to_bottom(true)
            .show(ui, |ui| {
                ui.add(egui::Label::new(RichText::new(&self.log).monospace()).wrap());
            });
    }
    fn settings_page(&mut self, ui: &mut egui::Ui) {
        egui::CollapsingHeader::new("Advanced").show(ui, |ui| {
            if ui.checkbox(&mut self.prefs.verbose, "Verbose game logging (show console)").changed() { self.save(); }
            if ui.button("Open logs").clicked() {
                let logs = self.prefs.output.join("logs");
                if let Err(e) = std::fs::create_dir_all(&logs).and_then(|_| std::process::Command::new("explorer.exe").arg(logs).spawn().map(|_| ())) { self.message = e.to_string(); }
            }
        });
        if ui.button("Open save folder").clicked() {
            if let Some(folder) = mkds_builder::settings::save_folder() {
                let result = std::fs::create_dir_all(&folder).and_then(|_| {
                    std::process::Command::new("explorer.exe")
                        .arg(&folder)
                        .spawn()
                        .map(|_| ())
                });
                if let Err(e) = result {
                    self.message = e.to_string();
                }
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
        ui.checkbox(&mut self.settings.fullscreen, "Start fullscreen / borderless");
        ui.checkbox(&mut self.settings.vsync, "Vsync");
        ui.checkbox(&mut self.settings.single_screen, "Single screen");
        ui.checkbox(&mut self.settings.swap_screens, "Swap screens");
        ui.add(egui::Slider::new(&mut self.settings.gap, 0..=192).text("Screen gap"));
        ui.add(egui::Slider::new(&mut self.settings.window_size, 1..=4).text("Window size (DS multiples)"));
        ui.checkbox(&mut self.settings.fps, "Show FPS counter");
        egui::ComboBox::from_label("FPS position").selected_text(["Top left", "Top right", "Bottom left", "Bottom right"][self.settings.fps_position as usize]).show_ui(ui, |ui| {
            for (n, label) in ["Top left", "Top right", "Bottom left", "Bottom right"].iter().enumerate() { ui.selectable_value(&mut self.settings.fps_position, n as u32, *label); }
        });
        ui.checkbox(&mut self.settings.frame_limiter, "Limit game speed");
        ui.add(egui::Slider::new(&mut self.settings.speed, 25..=400).text("Speed (%)"));
        ui.add(egui::Slider::new(&mut self.settings.volume, 0..=100).text("Master volume (%)"));
        ui.checkbox(&mut self.settings.mute, "Mute");
        ui.checkbox(&mut self.settings.verbose, "Verbose performance logging");
        ui.separator();
        ui.heading("Textures & customization");
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
        ui.checkbox(&mut self.settings.touch_mouse, "Left mouse button controls touch screen");
        ui.add(egui::Slider::new(&mut self.settings.deadzone, 5..=95).text("Controller deadzone (%)"));
        ui.label("Rumble: off. Controller capture wizard is available in-game with F10.");
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
        ui.separator();
        ui.strong("Keyboard & controller bindings");
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
            if let Err(e) = self.settings.save(&self.dir.join("settings.txt")) { self.message = e; return; }
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
    fn install_toolchain(&mut self) {
        if self.busy {
            return;
        }
        self.busy = true;
        self.message.clear();
        let data = self.dir.clone();
        let (tx, rx) = mpsc::channel();
        self.rx = Some(rx);
        std::thread::spawn(move || {
            let result = portable::install(&data, &mut |s| {
                let _ = tx.send(Event::Log(s));
            })
            .map(|_| ());
            let _ = tx.send(Event::Installed(result));
        });
    }
    fn setup(&mut self, ui: &mut egui::Ui) {
        ui.heading("Let's get you on the grid");
        ui.label("A one-time setup. Your ROM stays on your computer.");
        ui.add_space(20.);
        ui.horizontal_wrapped(|ui| {
            for (i, title) in ["Welcome", "Verify ROM", "Toolchain", "Build", "Ready"]
                .iter()
                .enumerate()
            {
                ui.label(RichText::new(format!("{}  {}", i + 1, title)).color(
                    if i == self.wizard {
                        Color32::from_rgb(245, 194, 74)
                    } else {
                        Color32::GRAY
                    },
                ));
            }
        });
        ui.add(
            egui::ProgressBar::new((self.wizard + 1) as f32 / 5.)
                .fill(TEAL)
                .desired_height(6.),
        );
        ui.add_space(18.);
        ui.spacing_mut().button_padding = egui::vec2(24., 16.);
        ui.visuals_mut().widgets.inactive.weak_bg_fill = Color32::from_rgb(180, 65, 44);
        ui.visuals_mut().widgets.hovered.weak_bg_fill = Color32::from_rgb(231, 108, 64);
        card().show(ui, |ui| {
            match self.wizard {
                0 => {
                    ui.add(egui::Image::new(&self.branding).fit_to_exact_size(egui::vec2(440., 110.)));
                    ui.heading("Welcome to MKDSrust");
                    ui.label("Bring your own clean USA revision 0 cartridge dump. We'll verify it, prepare a private Rust toolchain, and build your native game here.");
                    ui.label("No Visual Studio or Rust installation required. Internet is needed for the toolchain and crate dependencies. Allow several GB of disk space.");
                    if ui.button("Get started").clicked() { self.wizard = 1; }
                }
                1 => {
                    ui.heading("Choose and verify your ROM");
                    if ui.add_enabled(!self.busy, egui::Button::new("Choose .nds ROM")).clicked() {
                        if let Some(p) = rfd::FileDialog::new().add_filter("Nintendo DS ROM", &["nds"]).pick_file() { self.inspect(p); }
                    }
                    ui.label(self.prefs.rom.display().to_string());
                    if let Some(info) = &self.info {
                        ui.label(&info.compatibility);
                        ui.label(format!("{} | {} | revision {}", self.title, info.game_code, info.revision));
                        if info.reference_match && ui.button("Continue to toolchain").clicked() { self.wizard = 2; }
                    }
                }
                2 => {
                    ui.heading("Prepare your build tools");
                    ui.label(format!("Rust {} | {}", portable::VERSION, portable::TARGET));
                    ui.label("Official Rust archives are checked against published SHA-256 checksums. Failed downloads can be retried and resumed.");
                    ui.label("Download size: several hundred MB; exact transferred and total bytes appear in the log.");
                    if portable::ready(&self.dir) || self.prefs.system_toolchain {
                        if ui.button("Continue to build").clicked() { self.wizard = 3; }
                    } else if ui.add_enabled(!self.busy, egui::Button::new("Install portable toolchain / Retry")).clicked() { self.install_toolchain(); }
                }
                3 => { ui.heading("Build your native game"); ui.label("The first build downloads crates.io dependencies. Keep this window open; compilation can take a while."); self.rom_and_build(ui); }
                _ => {
                    ui.heading("You're ready to play"); ui.label("Your game was built locally from your ROM.");
                    if ui.button("Go to Home").clicked() { self.prefs.setup_done = true; self.save(); self.page = 0; }
                }
            }
        });
        if self.busy {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Working... progress and downloads appear below.");
            });
        }
        if self.wizard != 3 {
            egui::CollapsingHeader::new("Detailed activity log")
                .default_open(self.busy)
                .show(ui, |ui| self.log_panel(ui));
        }
    }
    fn install_mod(&mut self, archive: PathBuf) {
        if self.busy { return; }
        let game = self.prefs.output.clone(); let rom = self.prefs.rom.clone();
        let (tx, rx) = mpsc::channel(); self.rx = Some(rx); self.busy = true;
        self.message = "Screening mod against your ROM…".into();
        std::thread::spawn(move || {
            let result = (|| -> mkds_builder::modpkg::Result<String> {
                if !game.join("manifest.json").is_file() { return Err("Build a game first".into()); }
                let info = rom::inspect_rom(&rom)?;
                if !info.reference_match { return Err("Mod screening requires the supported AMCE rev 0 ROM".into()); }
                let retail = mkds_builder::modpkg::RetailIndex::from_rom(&std::fs::read(&rom)?)?;
                let pkg = mkds_builder::modpkg::Package::read(&archive, &retail)?;
                pkg.install(&game)?;
                Ok(format!("Installed {}. Enable it below. {}", pkg.manifest.name, pkg.warnings.join("; ")))
            })().map_err(|e| e.to_string());
            let _ = tx.send(Event::ModInstalled(result));
        });
    }
    fn mods_page(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        use mkds_builder::modpkg;
        ui.heading("Mods");
        ui.hyperlink_to("Browse mods on mkdsrust.com", "https://mkdsrust.com");
        ui.label("Install a .zip or drop it here. Later enabled mods win. Place dependencies first.");
        ui.label("Mods are disabled in exact timing and acceptance runs.");
        ui.add_enabled_ui(!self.busy, |ui| {
            if ui.button("Install from .zip").clicked() {
                if let Some(path) = rfd::FileDialog::new().add_filter("Mod package", &["zip"]).pick_file() { self.install_mod(path); }
            }
            if ui.button("Open mods folder").clicked() {
                let path = self.prefs.output.join("mods");
                if let Err(e) = std::fs::create_dir_all(&path).and_then(|_| std::process::Command::new("explorer.exe").arg(path).spawn().map(|_| ())) { self.message = e.to_string(); }
            }
        });
        if !self.busy {
            if let Some(path) = ctx.input(|i| i.raw.dropped_files.iter().filter_map(|f| f.path.clone()).next()) { self.install_mod(path); }
        }
        let installed = modpkg::installed(&self.prefs.output);
        let mut enabled = modpkg::enabled(&self.prefs.output);
        let active: Vec<_> = enabled.iter().filter_map(|id| installed.iter().find(|m| &m.id == id).cloned()).collect();
        let mut displayed = installed.clone();
        displayed.sort_by_key(|m| enabled.iter().position(|id| id == &m.id).unwrap_or(usize::MAX));
        let mut changed = false; let mut remove = None;
        ui.add_enabled_ui(!self.busy, |ui| {
            for m in &displayed {
                ui.separator();
                ui.horizontal(|ui| {
                    let mut on = enabled.contains(&m.id);
                    if ui.checkbox(&mut on, format!("{} {} ({})", m.name, m.version, m.id)).changed() {
                        enabled.retain(|id| id != &m.id); if on { enabled.push(m.id.clone()); } changed = true;
                    }
                    if let Some(i) = enabled.iter().position(|id| id == &m.id) {
                        if ui.add_enabled(i > 0, egui::Button::new("Up")).clicked() { enabled.swap(i, i-1); changed = true; }
                        if ui.add_enabled(i+1 < enabled.len(), egui::Button::new("Down")).clicked() { enabled.swap(i, i+1); changed = true; }
                    }
                    if ui.button("Remove").clicked() { remove = Some(m.id.clone()); }
                });
                if !m.description.is_empty() { ui.label(&m.description); }
                for warning in modpkg::issues(m, &active) { ui.colored_label(Color32::YELLOW, warning); }
            }
        });
        let mut owners = std::collections::BTreeMap::new();
        for m in &active {
            for target in m.files.keys() {
                if let Some(previous) = owners.insert(target, &m.id) {
                    ui.colored_label(Color32::YELLOW, format!("Load order: {} overrides {} for {}", m.id, previous, target));
                }
            }
        }
        if let Some(id) = remove {
            let path = self.prefs.output.join("mods").join(&id);
            // IDs were validated; do not follow a directory link during removal.
            match std::fs::symlink_metadata(&path) {
                Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => {
                    match std::fs::remove_dir_all(&path) {
                        Ok(()) => { enabled.retain(|x| x != &id); changed = true; }
                        Err(e) => self.message = e.to_string(),
                    }
                }
                _ => self.message = "Unsafe mod folder; remove it manually".into(),
            }
        }
        if changed { if let Err(e) = modpkg::save_enabled(&self.prefs.output, &enabled) { self.message = e.to_string(); } }
    }
    fn advanced(&mut self, ui: &mut egui::Ui) {
        ui.heading("Advanced");
        ui.label("Build tools and diagnostics");
        ui.add_space(20.);
        let mut changed = ui
            .checkbox(&mut self.prefs.accuracy, "Accuracy mode (exact hardware)")
            .changed();
        changed |= ui.checkbox(&mut self.prefs.verbose, "Verbose game logging (show console)").changed();
        changed |= ui
            .checkbox(
                &mut self.prefs.system_toolchain,
                "Use system Rust instead of the portable GNU toolchain",
            )
            .changed();
        ui.label("System Rust uses your installed default target and may require Visual Studio. Portable GNU is recommended.");
        if changed {
            self.save();
        }
        if ui
            .add_enabled(
                !self.busy,
                egui::Button::new("Install / repair portable toolchain"),
            )
            .clicked()
        {
            self.install_toolchain();
        }
        if ui.button("Rebuild game").clicked() {
            self.page = 6;
            self.wizard = 3;
        }
        for (label, path) in [
            ("Open launcher data", self.dir.clone()),
            ("Open build cache", build::default_cache_dir()),
            ("Open game folder", self.prefs.output.clone()),
            ("Open logs", self.prefs.output.join("logs")),
        ] {
            if ui.button(label).clicked() {
                if let Err(e) = std::process::Command::new("explorer.exe").arg(path).spawn() {
                    self.message = e.to_string();
                }
            }
        }
        if ui.button("Open saved build log").clicked() {
            let _ = std::process::Command::new("notepad.exe")
                .arg(self.dir.join("launcher-build.log"))
                .spawn();
        }
        self.log_panel(ui);
    }
    fn about(&self, ui: &mut egui::Ui) {
        ui.add(egui::Image::new(&self.icon).fit_to_exact_size(egui::vec2(40., 40.)));
        ui.add(egui::Image::new(&self.branding).fit_to_exact_size(egui::vec2(440., 110.)));
        ui.heading(format!("MKDSrust {}", env!("CARGO_PKG_VERSION")));
        ui.label("Licensed under GPL-3.0-or-later");
        ui.hyperlink_to(
            "Source repository",
            "https://github.com/CameronStorer/MKDSrust",
        );
        ui.hyperlink_to("Documentation", "https://docs.mkdsrust.com");
        ui.hyperlink_to("Website and mods", "https://mkdsrust.com");
        ui.add_space(24.);
        ui.heading("Licenses & acknowledgements");
        ui.hyperlink_to(
            "MKDSrust: GPL-3.0-or-later",
            "https://github.com/CameronStorer/MKDSrust/blob/main/LICENSE",
        );
        ui.label("Interface: egui / eframe (MIT or Apache-2.0). Bundled default fonts: Hack (MIT), Ubuntu (Ubuntu Font Licence), Noto Emoji (OFL). Navigation icons are original vector shapes.");
        egui::CollapsingHeader::new("Read bundled font licenses").show(ui, |ui| {
            for (name, text) in [
                (
                    "Hack",
                    include_str!("../../assets/licenses/Hack-Regular.txt"),
                ),
                ("Ubuntu", include_str!("../../assets/licenses/UFL.txt")),
                ("Noto Emoji", include_str!("../../assets/licenses/OFL.txt")),
                (
                    "Emoji icons",
                    include_str!("../../assets/licenses/emoji-icon-font-mit-license.txt"),
                ),
            ] {
                egui::CollapsingHeader::new(name).show(ui, |ui| {
                    ui.label(text);
                });
            }
        });
        ui.separator();
        ui.label("MKDSrust is an independent project and is not affiliated with or endorsed by Nintendo. Mario Kart and Nintendo DS are Nintendo trademarks. No Nintendo ROM, game assets, or translated game code are distributed. Supply your own legally obtained cartridge dump; all game builds happen locally.");
    }
    fn draw(&mut self, ctx: &egui::Context) {
        self.poll();
        ctx.request_repaint_after(Duration::from_millis(100));
        egui::SidePanel::left("navigation")
            .exact_width(175.)
            .show(ctx, |ui| {
                ui.add_space(22.);

                ui.add(egui::Image::new(&self.branding).fit_to_exact_size(egui::vec2(153., 38.)));
                ui.label(
                    RichText::new("DESKTOP LAUNCHER")
                        .size(11.)
                        .color(Color32::from_rgb(245, 194, 74)),
                );
                ui.add_space(30.);
                for (i, title) in [
                    "Home", "Settings", "Controls", "Mods", "Advanced", "About", "Setup",
                ]
                .iter()
                .enumerate()
                {
                    if ui
                        .horizontal(|ui| {
                            let active = self.page == i;
                            let (r, _) =
                                ui.allocate_exact_size(egui::vec2(3., 26.), egui::Sense::hover());
                            if active {
                                ui.painter().rect_filled(r, 2., TEAL);
                            }
                            glyph(ui, i, if active { TEAL } else { Color32::GRAY });
                            ui.selectable_value(&mut self.page, i, *title).clicked()
                        })
                        .inner
                    {
                        self.capture = None;
                    }
                }
                ui.with_layout(egui::Layout::bottom_up(egui::Align::LEFT), |ui| {
                    ui.label(
                        RichText::new(concat!("v", env!("CARGO_PKG_VERSION")))
                            .small()
                            .color(Color32::GRAY),
                    );
                });
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
            let frame = if self.page == 0 { egui::Frame::none().inner_margin(12.) } else { card() };
            frame.show(ui, |ui| { ui.set_min_width(ui.available_width()); match self.page {
                0 => self.home(ui), 1 => self.settings_page(ui), 2 => self.controls_page(ui, ctx),
                3 => self.mods_page(ui, ctx),
                4 => self.advanced(ui), 5 => self.about(ui), _ => self.setup(ui),
            } });       }); });
        if self.busy && ctx.input(|i| i.viewport().close_requested()) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.message = "Wait for the build or game to finish before closing.".into();
        }
    }
}
impl eframe::App for Launcher {
    fn update(&mut self, ctx: &egui::Context, _: &mut eframe::Frame) {
        for event in ctx.input(|i| i.events.clone()) {
            if let egui::Event::Screenshot { image, .. } = event {
                if let Some(frame) = self.smoke_frame {
                    let names = [
                        "home",
                        "settings",
                        "controls",
                        "mods",
                        "advanced",
                        "about",
                        "setup-welcome",
                        "setup-rom",
                        "setup-toolchain",
                        "setup-build",
                        "setup-done",
                    ];
                    let name = names[(frame as usize / 8).min(10)];
                    let dir = std::env::var_os("MKDS_SMOKE_SHOTS")
                        .map(PathBuf::from)
                        .unwrap_or_else(|| PathBuf::from("release_shots"));
                    let result = std::fs::create_dir_all(&dir)
                        .map_err(|e| e.to_string())
                        .and_then(|_| {
                            image::save_buffer(
                                dir.join(format!("{name}.png")),
                                &image.as_raw(),
                                image.size[0] as u32,
                                image.size[1] as u32,
                                image::ColorType::Rgba8,
                            )
                            .map_err(|e| e.to_string())
                        });
                    if let Err(e) = result {
                        self.message = e;
                    }
                }
            }
        }
        if let Some(frame) = self.smoke_frame {
            let screen = (frame as usize / 8).min(10);
            self.page = screen.min(6);
            if screen >= 6 {
                self.wizard = screen - 6;
            }
        }
        self.draw(ctx);
        if let Some(frame) = &mut self.smoke_frame {
            *frame += 1;
            if *frame % 8 == 3 {
                ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot);
            }
            if *frame >= 88 {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
    }
}
fn main() -> eframe::Result<()> {
    let smoke_size = if std::env::args().any(|a| a == "--smoke-test") {
        std::env::var("MKDS_SMOKE_SIZE").ok().and_then(|s| {
            let (w, h) = s.split_once('x')?;
            Some([w.parse::<f32>().ok()?, h.parse::<f32>().ok()?])
        })
    } else {
        None
    };
    eframe::run_native(
        "MKDSrust",
        eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default()
                .with_icon(
                    eframe::icon_data::from_png_bytes(include_bytes!(
                        "../../assets/branding/icon.png"
                    ))
                    .expect("valid embedded window icon"),
                )
                .with_inner_size(smoke_size.unwrap_or([1280., 800.]))
                .with_min_inner_size([1024., 640.]),
            ..Default::default()
        },
        Box::new(|cc| Ok(Box::new(Launcher::new(cc)))),
    )
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn verbose_persists_and_legacy_build_dates_remain_readable() {
        let prefs = Preferences { verbose: true, last_build: "0".into(), ..Preferences::default() };
        let restored: Preferences = serde_json::from_slice(&serde_json::to_vec(&prefs).unwrap()).unwrap();
        assert!(restored.verbose); assert_eq!(build_date(&restored.last_build), "1970-01-01T00:00:00Z");
        assert_eq!(build_date("SystemTime { tv_sec: 0, tv_nsec: 123 }"), "1970-01-01T00:00:00Z");
    }
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
            branding: ctx.load_texture(
                "test logo",
                egui::ColorImage::new([1, 1], Color32::WHITE),
                Default::default(),
            ),
            icon: ctx.load_texture(
                "test icon",
                egui::ColorImage::new([1, 1], Color32::WHITE),
                Default::default(),
            ),
            wizard: 0,
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
        for page in 0..7 {
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
        assert_eq!(
            std::fs::read_to_string(output).unwrap(),
            "native,,native,exact"
        );
        assert_eq!(std::env::var_os("MKDS_HW_FS"), before);
    }
}

/// Where a build goes. An empty or new folder is used as is. A folder that holds other files (e.g. the Desktop)
/// gets an `MKDSrust` subfolder. Earlier builds are kept: a taken name gets " (2)", " (3)", ...
fn build_folder(chosen: &Path) -> PathBuf {
    let free = |p: &Path| std::fs::read_dir(p).map_or(true, |mut i| i.next().is_none());
    let is_build = |p: &Path| p.join("manifest.json").is_file();
    let base = if free(chosen) || is_build(chosen) {
        chosen.to_path_buf()
    } else {
        chosen.join("MKDSrust")
    };
    if free(&base) {
        return base;
    }
    let name = base.file_name().unwrap_or_default().to_string_lossy().into_owned();
    (2..)
        .map(|n| base.with_file_name(format!("{name} ({n})")))
        .find(|p| free(p))
        .unwrap()
}

#[cfg(test)]
mod build_folder_tests {
    use super::build_folder;

    #[test]
    fn picks_readable_folders() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        assert_eq!(build_folder(&root.join("new")), root.join("new"));
        std::fs::write(root.join("note.txt"), "x").unwrap();
        assert_eq!(build_folder(root), root.join("MKDSrust"));
        std::fs::create_dir(root.join("MKDSrust")).unwrap();
        std::fs::write(root.join("MKDSrust/manifest.json"), "{}").unwrap();
        assert_eq!(build_folder(root), root.join("MKDSrust (2)"));
        assert_eq!(build_folder(&root.join("MKDSrust")), root.join("MKDSrust (2)"));
    }
}
