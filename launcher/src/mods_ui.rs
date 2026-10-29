//! Mods page: one load order for every package type (Mod Organizer style), plus making your own mods.
use super::*;
use mkds_builder::{modding, modpkg};
use std::collections::{BTreeMap, BTreeSet};

pub const SITE_MODS: &str = "https://mkdsrust.com/mods";

#[derive(Default)]
pub struct ModsState {
    pub filter: Option<&'static str>,
    pub create: Option<CreateForm>,
    pub report: String,
    /// A mod project installed for "Test in game"; removed when the game closes.
    pub dev: Option<String>,
}

pub struct CreateForm {
    name: String,
    author: String,
    kind: &'static str,
    id: String,
    id_edited: bool,
    extract: bool,
}
impl Default for CreateForm {
    fn default() -> Self {
        Self { name: String::new(), author: String::new(), kind: "other", id: String::new(), id_edited: false, extract: true }
    }
}

pub fn kind_label(kind: &str) -> &'static str {
    match kind {
        "character" => "Character",
        "kart" => "Kart",
        "track" => "Track",
        "battle" => "Battle",
        "music" => "Music",
        "texture" => "Texture",
        "gameplay" => "Gameplay",
        _ => "Other",
    }
}
fn kind_color(kind: &str) -> Color32 {
    match kind {
        "character" => Color32::from_rgb(245, 194, 74),
        "kart" => Color32::from_rgb(240, 128, 96),
        "track" => Color32::from_rgb(110, 190, 120),
        "battle" => Color32::from_rgb(220, 100, 140),
        "music" => Color32::from_rgb(160, 130, 240),
        "texture" => Color32::from_rgb(90, 170, 240),
        "gameplay" => TEAL,
        _ => Color32::GRAY,
    }
}
fn chip(ui: &mut egui::Ui, kind: &str) {
    let c = kind_color(kind);
    egui::Frame::none()
        .fill(c.gamma_multiply(0.18))
        .stroke(egui::Stroke::new(1., c.gamma_multiply(0.6)))
        .rounding(10.)
        .inner_margin(egui::Margin::symmetric(8., 1.))
        .show(ui, |ui| ui.label(RichText::new(kind_label(kind)).small().color(c)));
}
fn open_folder(path: &Path) -> Result<(), String> {
    std::fs::create_dir_all(path).map_err(|e| e.to_string())?;
    std::process::Command::new("explorer.exe").arg(path).spawn().map(|_| ()).map_err(|e| e.to_string())
}

/// Screen a downloaded/dropped zip against the player's ROM and install it (see [`modding::install_zip`]).
fn screen_and_install(game: &Path, rom: &Path, archive: &Path) -> modpkg::Result<String> {
    if !rom::inspect_rom(rom)?.reference_match {
        return Err("Mod screening needs the supported Mario Kart DS (USA, rev 0) ROM".into());
    }
    modding::install_zip(game, &std::fs::read(rom)?, archive)
}

impl Launcher {
    fn modding_root(&self) -> PathBuf {
        self.dir.join("modding")
    }
    fn game_ready(&self) -> bool {
        self.prefs.output.join("manifest.json").is_file()
    }
    fn start(&mut self, message: &str) -> mpsc::Sender<Event> {
        let (tx, rx) = mpsc::channel();
        self.rx = Some(rx);
        self.busy = true;
        self.message = message.into();
        tx
    }

    pub(crate) fn install_mod(&mut self, archive: PathBuf) {
        if self.busy {
            return;
        }
        let (game, rom) = (self.prefs.output.clone(), self.prefs.rom.clone());
        let tx = self.start("Checking the mod against your ROM…");
        std::thread::spawn(move || {
            let r = screen_and_install(&game, &rom, &archive).map_err(|e| format!("Mod not installed: {e}"));
            let _ = tx.send(Event::ModInstalled(r));
        });
    }

    /// A `mkdsrust://install?url=...` link from the site.
    pub(crate) fn install_link(&mut self, link: &str) {
        self.page = 3;
        let url = match protocol::install_url(link) {
            Ok(u) => u,
            Err(e) => {
                self.message = e;
                return;
            }
        };
        let (game, rom, data) = (self.prefs.output.clone(), self.prefs.rom.clone(), self.dir.clone());
        let tx = self.start("Downloading the mod from mkdsrust.com…");
        std::thread::spawn(move || {
            let r = (|| -> Result<String, String> {
                let dir = data.join("downloads");
                std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
                let file = tempfile::Builder::new().suffix(".zip").tempfile_in(&dir).map_err(|e| e.to_string())?;
                protocol::download(&url, file.path())?;
                screen_and_install(&game, &rom, file.path()).map_err(|e| e.to_string())
            })()
            .map_err(|e| format!("Mod not installed: {e}"));
            let _ = tx.send(Event::ModInstalled(r));
        });
    }

    fn create_project(&mut self, form: CreateForm) {
        let (rom, root) = (self.prefs.rom.clone(), self.modding_root());
        let tx = self.start("Creating your mod project (copying your ROM's files)…");
        std::thread::spawn(move || {
            let r = (|| -> modpkg::Result<PathBuf> {
                if !rom::inspect_rom(&rom)?.reference_match {
                    return Err("Mod projects need the supported Mario Kart DS (USA, rev 0) ROM (choose it in Setup)".into());
                }
                let draft = modding::Draft::new(form.id.trim(), form.name.trim(), form.kind, form.author.trim());
                modding::create(&root, &draft, &rom, &std::fs::read(&rom)?, form.extract)
            })()
            .map_err(|e| e.to_string());
            let _ = tx.send(Event::ModCreated(r));
        });
    }

    fn pack_project(&mut self, dir: PathBuf) {
        let rom = self.prefs.rom.clone();
        let tx = self.start("Packing: comparing your files with the ROM and screening the zip…");
        std::thread::spawn(move || {
            let r = (|| -> modpkg::Result<String> {
                let ws = modding::Workspace::open(&dir)?;
                Ok(modding::pack(&ws, &std::fs::read(&rom)?, None)?.summary())
            })()
            .map_err(|e| e.to_string());
            let _ = tx.send(Event::ModPacked(r));
        });
    }

    fn test_project(&mut self, dir: PathBuf) {
        let (rom, game) = (self.prefs.rom.clone(), self.prefs.output.clone());
        let tx = self.start("Preparing a test copy of your mod…");
        std::thread::spawn(move || {
            let r = (|| -> modpkg::Result<(String, String)> {
                let ws = modding::Workspace::open(&dir)?;
                let bytes = std::fs::read(&rom)?;
                let retail = modpkg::RetailIndex::from_rom(&bytes)?;
                let (id, report) = modding::test_install(&ws, &bytes, &retail, &game)?;
                Ok((id, report.summary()))
            })()
            .map_err(|e| e.to_string());
            let _ = tx.send(Event::ModTest(r));
        });
    }

    /// Mod-related events from [`Launcher::poll`]. Returns false when `event` is not one of them.
    pub(crate) fn mod_event(&mut self, event: &Event) -> bool {
        match event {
            Event::ModInstalled(r) => {
                self.busy = false;
                self.message = r.clone().unwrap_or_else(|e| e.clone());
            }
            Event::ModCreated(r) => {
                self.busy = false;
                match r {
                    Ok(dir) => {
                        self.message = format!("Created {}. Edit files under romfs/, then Test in game or Pack.", dir.display());
                        let _ = open_folder(dir);
                    }
                    Err(e) => self.message = format!("Could not create the mod project: {e}"),
                }
            }
            Event::ModPacked(r) => {
                self.busy = false;
                match r {
                    Ok(s) => {
                        self.message = "Packed and screened: ready to upload.".into();
                        self.mods.report = s.clone();
                    }
                    Err(e) => {
                        self.message = "Packing failed (see the report on the Mods page).".into();
                        self.mods.report = e.clone();
                    }
                }
            }
            Event::ModTest(r) => {
                self.busy = false;
                match r {
                    Ok((id, s)) => {
                        self.mods.dev = Some(id.clone());
                        self.mods.report = s.clone();
                        self.play();
                        self.message = format!("Testing {id}: it is installed last (top priority) until the game closes.");
                    }
                    Err(e) => {
                        self.message = "Test failed (see the report on the Mods page).".into();
                        self.mods.report = e.clone();
                    }
                }
            }
            _ => return false,
        }
        true
    }

    /// After the game closes: remove a "Test in game" copy.
    pub(crate) fn finish_test(&mut self) {
        if let Some(id) = self.mods.dev.take() {
            if let Err(e) = modding::remove_installed(&self.prefs.output, &id) {
                self.message = format!("Could not remove the test copy {id}: {e}");
            }
        }
    }

    pub(crate) fn mods_page(&mut self, ui: &mut egui::Ui) {
        let game = self.prefs.output.clone();
        let ready = self.game_ready();
        ui.horizontal(|ui| {
            ui.heading("Mods");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let browse = egui::Button::new(RichText::new("Browse mods on mkdsrust.com  ↗").strong().color(Color32::BLACK))
                    .fill(TEAL)
                    .min_size(egui::vec2(0., 40.));
                if ui.add(browse).on_hover_text(SITE_MODS).clicked() {
                    ui.ctx().open_url(egui::OpenUrl::new_tab(SITE_MODS));
                }
            });
        });
        ui.label("All your mods in one load order, whatever their type. Top to bottom: when two mods change the same file, the lower one wins. Drop a mod .zip anywhere in this window to install it.");
        let busy = self.busy;
        ui.add_enabled_ui(!busy, |ui| {
            ui.horizontal_wrapped(|ui| {
                if ui.add_enabled(ready, egui::Button::new("Install .zip…")).clicked() {
                    if let Some(path) = rfd::FileDialog::new().add_filter("Mod package", &["zip"]).pick_file() {
                        self.install_mod(path);
                    }
                }
                if ui.button("Create mod…").clicked() {
                    self.mods.create = Some(CreateForm::default());
                }
                if ui.add_enabled(ready, egui::Button::new("Open mods folder")).clicked() {
                    if let Err(e) = open_folder(&game.join("mods")) {
                        self.message = e;
                    }
                }
            });
        });
        if !ready {
            ui.colored_label(Color32::YELLOW, "Build your game first (Setup): mods are installed into the game folder.");
        }

        let installed = modpkg::installed(&game);
        let by_id: BTreeMap<&str, &modpkg::Manifest> = installed.iter().map(|m| (m.id.as_str(), m)).collect();
        let mut order = modpkg::load_order(&game);
        let mut on: BTreeSet<String> = modpkg::enabled(&game).into_iter().collect();
        ui.add_space(4.);
        ui.horizontal_wrapped(|ui| {
            if ui.selectable_label(self.mods.filter.is_none(), format!("All  {}", installed.len())).clicked() {
                self.mods.filter = None;
            }
            for kind in modpkg::TYPES {
                let n = installed.iter().filter(|m| m.kind == kind).count();
                let selected = self.mods.filter == Some(kind);
                let text = RichText::new(format!("{}  {n}", kind_label(kind)));
                if ui.selectable_label(selected, if n == 0 { text.color(Color32::DARK_GRAY) } else { text }).clicked() {
                    self.mods.filter = if selected { None } else { Some(kind) };
                }
            }
        });
        ui.add_space(4.);

        // which enabled mods touch each file, in load order (the last one wins)
        let mut touch: BTreeMap<String, Vec<&str>> = BTreeMap::new();
        for id in order.iter().filter(|id| on.contains(*id)) {
            if let Some(m) = by_id.get(id.as_str()) {
                for t in m.files.keys() {
                    touch.entry(t.to_ascii_lowercase()).or_default().push(&m.id);
                }
            }
        }
        let active: Vec<modpkg::Manifest> =
            order.iter().filter(|id| on.contains(*id)).filter_map(|id| by_id.get(id.as_str()).map(|m| (*m).clone())).collect();

        if installed.is_empty() {
            ui.label(RichText::new("No mods installed yet. Browse mkdsrust.com, drop a .zip here, or make your own with Create mod.").color(Color32::GRAY));
        }
        let (mut changed, mut remove, mut moved) = (false, None::<String>, None::<(usize, usize)>);
        ui.add_enabled_ui(!busy, |ui| {
            for (i, id) in order.iter().enumerate() {
                let Some(m) = by_id.get(id.as_str()) else { continue };
                if self.mods.filter.is_some_and(|f| f != m.kind) {
                    continue;
                }
                let enabled = on.contains(id);
                let row = egui::Frame::none()
                    .fill(if enabled { Color32::from_rgb(30, 41, 49) } else { Color32::from_rgb(24, 29, 34) })
                    .rounding(8.)
                    .inner_margin(egui::Margin::symmetric(10., 6.))
                    .show(ui, |ui| {
                        ui.set_min_width(ui.available_width());
                        ui.horizontal(|ui| {
                            ui.dnd_drag_source(egui::Id::new(("mod-order", id.as_str())), i, |ui| {
                                ui.label(RichText::new("≡").size(20.).color(Color32::GRAY));
                            })
                            .response
                            .on_hover_text("Drag to change the load order");
                            ui.label(RichText::new(format!("{:>2}", i + 1)).monospace().color(Color32::GRAY));
                            let mut e = enabled;
                            if ui.checkbox(&mut e, "").on_hover_text(if enabled { "Enabled" } else { "Disabled" }).changed() {
                                if e { on.insert(id.clone()) } else { on.remove(id) };
                                changed = true;
                            }
                            ui.vertical(|ui| {
                                ui.horizontal(|ui| {
                                    let name = RichText::new(&m.name).strong();
                                    ui.label(if enabled { name } else { name.color(Color32::GRAY) });
                                    ui.label(RichText::new(format!("v{}", m.version)).small().color(Color32::GRAY));
                                    chip(ui, &m.kind);
                                    ui.label(RichText::new(format!("by {}", m.author)).small().color(Color32::GRAY));
                                });
                                if enabled {
                                    let (mut wins, mut loses) = (Vec::new(), Vec::new());
                                    for t in m.files.keys() {
                                        let ids = &touch[&t.to_ascii_lowercase()];
                                        if ids.len() > 1 {
                                            if ids.last() == Some(&m.id.as_str()) {
                                                let others: Vec<_> = ids[..ids.len() - 1].iter().copied().collect();
                                                wins.push(format!("{t}: wins over {}", others.join(", ")));
                                            } else {
                                                loses.push(format!("{t}: overridden by {}", ids.last().unwrap()));
                                            }
                                        }
                                    }
                                    if !wins.is_empty() || !loses.is_empty() {
                                        let text = format!(
                                            "⚠ {} file(s) overlap with other mods: wins {}, overridden {}",
                                            wins.len() + loses.len(),
                                            wins.len(),
                                            loses.len()
                                        );
                                        let color = if loses.is_empty() { Color32::from_rgb(245, 194, 74) } else { Color32::from_rgb(240, 128, 96) };
                                        ui.label(RichText::new(text).small().color(color))
                                            .on_hover_text([wins, loses].concat().join("\n"));
                                    }
                                    for w in modpkg::issues(m, &active) {
                                        ui.label(RichText::new(format!("⚠ {w}")).small().color(Color32::YELLOW));
                                    }
                                }
                            });
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if ui.small_button("Remove").clicked() {
                                    remove = Some(id.clone());
                                }
                                if ui.add_enabled(i + 1 < order.len(), egui::Button::new("▼").small()).on_hover_text("Later (higher priority)").clicked() {
                                    moved = Some((i, i + 1));
                                }
                                if ui.add_enabled(i > 0, egui::Button::new("▲").small()).on_hover_text("Earlier (lower priority)").clicked() {
                                    moved = Some((i, i - 1));
                                }
                            });
                        });
                    })
                    .response;
                if let Some(from) = row.dnd_hover_payload::<usize>() {
                    if *from != i {
                        let y = if *from < i { row.rect.bottom() } else { row.rect.top() };
                        ui.painter().hline(row.rect.x_range(), y, egui::Stroke::new(2., TEAL));
                    }
                }
                if let Some(from) = row.dnd_release_payload::<usize>() {
                    moved = Some((*from, i));
                }
            }
        });
        if let Some((from, to)) = moved {
            if from != to && from < order.len() && to < order.len() {
                let id = order.remove(from);
                order.insert(to, id);
                changed = true;
            }
        }
        if let Some(id) = remove {
            match modding::remove_installed(&game, &id) {
                Ok(()) => {
                    order.retain(|x| x != &id);
                    on.remove(&id);
                    self.message = format!("Removed {id}.");
                }
                Err(e) => self.message = e.to_string(),
            }
        }
        if changed {
            if let Err(e) = modpkg::save_load_order(&game, &order, &on) {
                self.message = e.to_string();
            }
        }

        ui.add_space(14.);
        ui.separator();
        let root = self.modding_root();
        ui.horizontal(|ui| {
            ui.heading("Your mod projects");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Open packages folder").clicked() {
                    if let Err(e) = open_folder(&root.join("packages")) {
                        self.message = e;
                    }
                }
                ui.hyperlink_to("How to make a mod", "https://docs.mkdsrust.com/guides--first-mod.html");
            });
        });
        let projects = modding::list(&root);
        if projects.is_empty() {
            ui.label(RichText::new("Create mod copies your ROM's files into a project folder you edit with any DS tool. Test in game runs it right away; Pack makes a ready-to-upload zip.").color(Color32::GRAY));
        }
        let mut action = None;
        ui.add_enabled_ui(!busy, |ui| {
            for ws in &projects {
                egui::Frame::none().fill(Color32::from_rgb(24, 29, 34)).rounding(8.).inner_margin(egui::Margin::symmetric(10., 6.)).show(ui, |ui| {
                    ui.set_min_width(ui.available_width());
                    ui.horizontal(|ui| {
                        ui.vertical(|ui| {
                            ui.horizontal(|ui| {
                                ui.label(RichText::new(&ws.draft.name).strong());
                                ui.label(RichText::new(format!("v{}", ws.draft.version)).small().color(Color32::GRAY));
                                chip(ui, &ws.draft.kind);
                            });
                            ui.label(RichText::new(ws.dir.display().to_string()).small().color(Color32::GRAY));
                        });
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.button("Pack").on_hover_text("Write a ready-to-upload zip with only your changes").clicked() {
                                action = Some((0, ws.dir.clone()));
                            }
                            if ui.add_enabled(ready, egui::Button::new("Test in game")).on_hover_text("Install as a temporary top-priority mod and play").clicked() {
                                action = Some((1, ws.dir.clone()));
                            }
                            if ui.button("Open folder").clicked() {
                                action = Some((2, ws.dir.clone()));
                            }
                        });
                    });
                });
            }
        });
        match action {
            Some((0, dir)) => self.pack_project(dir),
            Some((1, dir)) => self.test_project(dir),
            Some((_, dir)) => {
                if let Err(e) = open_folder(&dir) {
                    self.message = e;
                }
            }
            None => {}
        }
        if !self.mods.report.is_empty() {
            egui::CollapsingHeader::new("Last pack / test report").default_open(true).show(ui, |ui| {
                ui.add(egui::Label::new(RichText::new(&self.mods.report).monospace().size(12.)).wrap());
            });
        }
        self.create_window(ui.ctx());
    }

    fn create_window(&mut self, ctx: &egui::Context) {
        let busy = self.busy;
        let have_rom = !self.prefs.rom.as_os_str().is_empty();
        let root = self.modding_root();
        let Some(form) = &mut self.mods.create else { return };
        let (mut open, mut submit) = (true, false);
        egui::Window::new("Create a mod")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0., 0.))
            .open(&mut open)
            .show(ctx, |ui| {
                egui::Grid::new("create-mod").num_columns(2).spacing([14., 10.]).show(ui, |ui| {
                    ui.label("Name");
                    ui.add(egui::TextEdit::singleline(&mut form.name).hint_text("Faster Karts"));
                    ui.end_row();
                    ui.label("Author");
                    ui.add(egui::TextEdit::singleline(&mut form.author).hint_text("your name"));
                    ui.end_row();
                    ui.label("Type");
                    egui::ComboBox::from_id_salt("create-mod-type").selected_text(kind_label(form.kind)).show_ui(ui, |ui| {
                        for kind in modpkg::TYPES {
                            ui.selectable_value(&mut form.kind, kind, kind_label(kind));
                        }
                    });
                    ui.end_row();
                    ui.label("Mod id");
                    if ui.text_edit_singleline(&mut form.id).changed() {
                        form.id_edited = true;
                    }
                    ui.end_row();
                });
                if !form.id_edited {
                    form.id = modding::suggest_id(&form.author, &form.name);
                }
                ui.checkbox(&mut form.extract, "Unpack .carc/.narc archives into folders (easiest to edit)");
                ui.label(
                    RichText::new(format!(
                        "Creates {} with a copy of your ROM's files, a manifest and a README. It stays on this PC.",
                        root.join(&form.id).display()
                    ))
                    .small()
                    .color(Color32::GRAY),
                );
                let valid = modpkg::valid_id(&form.id) && !form.name.trim().is_empty() && !form.author.trim().is_empty();
                if !have_rom {
                    ui.colored_label(Color32::YELLOW, "Choose your ROM in Setup first.");
                } else if !valid {
                    ui.label(RichText::new("Name, author and an id of 3-64 characters (a-z 0-9 . _ -) are needed.").small().color(Color32::GRAY));
                }
                if ui.add_enabled(valid && have_rom && !busy, egui::Button::new("Create project")).clicked() {
                    submit = true;
                }
            });
        if submit {
            let form = self.mods.create.take().unwrap();
            self.create_project(form);
        } else if !open {
            self.mods.create = None;
        }
    }
}
