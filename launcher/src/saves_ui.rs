//! Home's "Saves" card: what the save holds, importing a save from an emulator or flashcart, exporting it, and an
//! "everything unlocked" test save that never touches the real one.
use super::*;
use mkds_builder::mkds_save::{self, SaveKeys, Summary};

#[derive(Default)]
pub struct SavesState {
    /// Keys read from the ROM (and the ROM's SHA-1, which names the save), for the ROM path they came from.
    keys: Option<(PathBuf, Result<(SaveKeys, String), String>)>,
    /// The summary shown, for the save file and modification time it was made from.
    shown: Option<(PathBuf, Option<SystemTime>, Result<Summary, String>)>,
    /// A checked save waiting for "Use this save".
    pending: Option<(PathBuf, Vec<u8>, Summary)>,
}

/// The SHA-1 the built game names its save by (from the bundle's asset manifest).
pub fn bundle_save_id(output: &Path) -> Option<String> {
    let bytes = std::fs::read(output.join("assets/manifest.json")).ok()?;
    let manifest: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    manifest["rom"]["sha1"].as_str().map(str::to_ascii_lowercase)
}

/// The test save to play with, when the player switched to it and it exists.
pub fn active_test_save(test_save: bool, output: &Path) -> Option<PathBuf> {
    if !test_save {
        return None;
    }
    let path = mkds_save::test_save_path(&mkds_builder::settings::save_folder()?, &bundle_save_id(output)?);
    path.is_file().then_some(path)
}

fn lines(ui: &mut egui::Ui, summary: &Summary) {
    for line in summary.lines() {
        ui.label(RichText::new(line).size(14.));
    }
}

impl Launcher {
    fn save_keys(&mut self) -> Option<Result<&(SaveKeys, String), String>> {
        let rom = self.prefs.rom.clone();
        if !self.info.as_ref().is_some_and(|i| i.reference_match) {
            return None;
        }
        if self.saves.keys.as_ref().is_none_or(|(p, _)| *p != rom) {
            let loaded = std::fs::read(&rom)
                .map_err(|e| format!("Cannot read your ROM: {e}"))
                .and_then(|bytes| Ok((SaveKeys::from_rom(&bytes)?, mkds_builder::rom::sha1(&bytes))));
            self.saves.keys = Some((rom, loaded));
        }
        self.saves.keys.as_ref().map(|(_, k)| k.as_ref().map_err(Clone::clone))
    }

    pub(crate) fn saves_card(&mut self, ui: &mut egui::Ui) {
        ui.label(RichText::new("Saves").size(16.).strong());
        let Some(folder) = mkds_builder::settings::save_folder() else {
            ui.label("No save folder is available.");
            return;
        };
        let (keys, sha1) = match self.save_keys() {
            None => {
                let text = if self.info.is_some() {
                    "Save tools need your verified Mario Kart DS (USA) ROM."
                } else if self.busy && !self.prefs.rom.as_os_str().is_empty() {
                    "Checking your ROM…"
                } else {
                    "Choose your ROM first: the save tools read it to understand saves."
                };
                ui.label(RichText::new(text).color(Color32::GRAY));
                return;
            }
            Some(Err(e)) => {
                ui.label(RichText::new(e).color(Color32::from_rgb(242, 120, 83)));
                return;
            }
            Some(Ok((k, s))) => (k.clone(), s.clone()),
        };
        let real = mkds_save::save_path(&folder, &sha1);
        let test = mkds_save::test_save_path(&folder, &sha1);
        if self.prefs.test_save && !test.is_file() {
            self.prefs.test_save = false;
            self.save();
        }
        let idle = !self.busy;

        if let Some((file, save, summary)) = self.saves.pending.take() {
            ui.label(RichText::new(format!("Import {}?", file.file_name().unwrap_or_default().to_string_lossy())).strong());
            lines(ui, &summary);
            ui.label(RichText::new("Your current save is copied to saves/backup first.").color(Color32::GRAY));
            let mut keep = true;
            ui.horizontal(|ui| {
                if ui.add_enabled(idle, egui::Button::new("Use this save")).clicked() {
                    keep = false;
                    self.message = match mkds_save::install(&folder, &sha1, &save) {
                        Ok(Some(b)) => format!("Save imported. Your previous save is kept as {}.", b.display()),
                        Ok(None) => "Save imported.".into(),
                        Err(e) => e,
                    };
                    self.prefs.test_save = false;
                    self.save();
                }
                if ui.button("Cancel").clicked() {
                    keep = false;
                }
            });
            if keep {
                self.saves.pending = Some((file, save, summary));
            }
            return;
        }

        ui.horizontal(|ui| {
            ui.label("Play with");
            let mut test_save = self.prefs.test_save;
            ui.selectable_value(&mut test_save, false, "My save");
            ui.add_enabled_ui(test.is_file(), |ui| {
                ui.selectable_value(&mut test_save, true, "Test save (everything unlocked)")
                    .on_disabled_hover_text("Use \"Unlock all (for testing)\" to make one.");
            });
            if test_save != self.prefs.test_save && idle {
                self.prefs.test_save = test_save;
                self.save();
            }
        });
        let active = if self.prefs.test_save { test.clone() } else { real.clone() };
        let modified = std::fs::metadata(&active).and_then(|m| m.modified()).ok();
        if self.saves.shown.as_ref().is_none_or(|(p, m, _)| *p != active || *m != modified) {
            let summary = std::fs::read(&active).map_err(|e| e.to_string()).and_then(|b| keys.summary(&b));
            self.saves.shown = Some((active.clone(), modified, summary));
        }
        match &self.saves.shown.as_ref().unwrap().2 {
            _ if !active.is_file() => {
                ui.label("No save yet: the game creates one the first time you play. You can also import one.");
            }
            Ok(summary) => lines(ui, &summary.clone()),
            Err(e) => {
                ui.label(RichText::new(format!("This save cannot be read: {e}")).color(Color32::from_rgb(242, 120, 83)));
            }
        }
        if self.prefs.test_save {
            ui.label(RichText::new("Playing the test save. Your own save is untouched; switch back with \"My save\".").color(TEAL));
        }

        ui.horizontal_wrapped(|ui| {
            if ui.add_enabled(idle, egui::Button::new("Import save…")).on_hover_text("From melonDS or DeSmuME (.sav, .dsv), a flashcart, or an earlier MKDSrust install.").clicked() {
                let mut dialog = rfd::FileDialog::new()
                    .add_filter("DS save", &["sav", "dsv", "bin"])
                    .add_filter("All files", &["*"]);
                // Saves of the per-user MKDSrust install are not copied into portable installs any more: start there.
                if let Some(dir) = portable::legacy_dir().map(|d| d.join("saves")).filter(|d| d.is_dir() && *d != folder) {
                    dialog = dialog.set_directory(dir);
                }
                if let Some(file) = dialog.pick_file() {
                    match mkds_save::read_import(&keys, &file) {
                        Ok((save, summary)) => self.saves.pending = Some((file, save, summary)),
                        Err(e) => self.message = e,
                    }
                }
            }
            if ui.add_enabled(idle && active.is_file(), egui::Button::new("Export save…")).on_hover_text("A raw .sav that emulators and flashcarts read.").clicked() {
                let name = if self.prefs.test_save { "Mario Kart DS (unlocked).sav" } else { "Mario Kart DS.sav" };
                if let Some(target) = rfd::FileDialog::new().set_file_name(name).add_filter("DS save", &["sav"]).save_file() {
                    self.message = match std::fs::copy(&active, &target) {
                        Ok(_) => format!("Save exported to {}.", target.display()),
                        Err(e) => format!("Could not export the save: {e}"),
                    };
                }
            }
            let unlock = ui
                .add_enabled(idle && real.is_file(), egui::Button::new("Unlock all (for testing)"))
                .on_hover_text("Makes a separate test save from yours with every cup, class, character and kart unlocked. Your records stay as they are, and your own save is not changed.")
                .on_disabled_hover_text("Play once first, so the game creates your save.");
            if unlock.clicked() {
                let made = std::fs::read(&real)
                    .map_err(|e| e.to_string())
                    .and_then(|b| keys.unlock_all(&b))
                    .and_then(|u| mkds_save::write_atomic(&test, &u));
                match made {
                    Ok(()) => {
                        self.prefs.test_save = true;
                        self.save();
                        self.message = "Test save ready: the game now starts with everything unlocked. Choose \"My save\" to go back.".into();
                    }
                    Err(e) => self.message = format!("Could not make the test save: {e}"),
                }
            }
            if ui.button("Open save folder").clicked() {
                let result = std::fs::create_dir_all(&folder).and_then(|_| std::process::Command::new("explorer.exe").arg(&folder).spawn().map(|_| ()));
                if let Err(e) = result {
                    self.message = e.to_string();
                }
            }
        });
        if !idle && self.playing {
            ui.small("Close the game to change saves (it writes its save while running).");
        }
    }
}
