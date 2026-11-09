//! Mods page: the curated mod repository (`Mods/` in the MKDSrust repository). Ticked mods are compiled into the
//! next build. The list comes from the launcher's own copy of the project, refreshed from GitHub when online, so new
//! mods appear without a launcher update.
use super::*;
use mkds_builder::mods_repo::ModInfo;

pub const INDEX_URL: &str = "https://raw.githubusercontent.com/CameronStorer/MKDSrust/main/Mods/index.json";
const REPO_TREE: &str = "https://github.com/CameronStorer/MKDSrust/tree/main/";
const SUBMIT: &str = "https://github.com/CameronStorer/MKDSrust/blob/main/Mods/README.md";

#[derive(Default)]
pub struct Catalog {
    pub mods: Vec<ModInfo>,
    /// The mods included with this launcher (id, version): a newer version on GitHub needs a newer launcher,
    /// because a mod's runtime code ships with MKDSrust.
    embedded: Vec<(String, String)>,
    pub status: String,
    rx: Option<Receiver<Result<Vec<ModInfo>, String>>>,
    loaded: bool,
}

/// `Mods/index.json` -> its valid mods (a mod this launcher cannot build, e.g. one needing a newer runtime feature,
/// is left out).
pub fn parse_index(bytes: &[u8]) -> Result<Vec<ModInfo>, String> {
    #[derive(Deserialize)]
    struct Index {
        format: u32,
        mods: Vec<ModInfo>,
    }
    let index: Index = serde_json::from_slice(bytes).map_err(|e| format!("mod list: {e}"))?;
    if index.format != 1 {
        return Err("the online mod list needs a newer launcher".into());
    }
    Ok(index
        .mods
        .into_iter()
        .filter(|m| {
            let folder = m.path.rsplit('/').next().unwrap_or_default().to_string();
            let tier_ok = (m.tier == "official" && m.path == format!("Mods/{}", m.id))
                || (m.tier == "community" && m.path == format!("Mods/community/{}", m.id));
            tier_ok && mkds_builder::mods_repo::validate(m, &folder).is_ok()
        })
        .collect())
}

impl Launcher {
    /// Loads the embedded list once, then asks GitHub for the current one.
    fn load_catalog(&mut self) {
        if self.catalog.loaded {
            return;
        }
        self.catalog.loaded = true;
        if let Ok(dir) = portable::source(&self.dir, &mut |_| {}) {
            if let Ok(mods) = mkds_builder::mods_repo::list(&dir) {
                self.catalog.embedded = mods.iter().map(|m| (m.id.clone(), m.version.clone())).collect();
                self.catalog.mods = mods;
            }
        }
        self.refresh_catalog();
    }

    pub fn refresh_catalog(&mut self) {
        let (tx, rx) = mpsc::channel();
        self.catalog.rx = Some(rx);
        self.catalog.status = "Checking GitHub for new mods…".into();
        std::thread::spawn(move || {
            let result = std::process::Command::new(if cfg!(windows) { "curl.exe" } else { "curl" })
                .args(["-fsSL", "--proto", "=https", "--max-time", "20", INDEX_URL])
                .output()
                .map_err(|e| format!("could not start curl: {e}"))
                .and_then(|o| {
                    if o.status.success() {
                        parse_index(&o.stdout)
                    } else {
                        Err("offline or GitHub unreachable".into())
                    }
                });
            let _ = tx.send(result);
        });
    }

    pub fn poll_catalog(&mut self) {
        let Some(rx) = &self.catalog.rx else { return };
        if let Ok(result) = rx.try_recv() {
            self.catalog.rx = None;
            match result {
                Ok(online) => {
                    // Online entries win; keep embedded ones GitHub no longer lists only if they are selected.
                    let mut merged = online;
                    for m in std::mem::take(&mut self.catalog.mods) {
                        if !merged.iter().any(|o| o.id == m.id) && self.prefs.mods.contains(&m.id) {
                            merged.push(m);
                        }
                    }
                    self.catalog.mods = merged;
                    self.catalog.status = "Up to date with GitHub.".into();
                }
                Err(e) => self.catalog.status = format!("Showing the mods included with this launcher ({e})."),
            }
        }
    }

    pub fn catalog_busy(&self) -> bool {
        self.catalog.rx.is_some()
    }

    /// The ids built into the current game (from its manifest).
    pub fn built_mods(&self) -> Option<Vec<String>> {
        let bytes = std::fs::read(self.prefs.output.join("manifest.json")).ok()?;
        let m: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
        let mut ids: Vec<String> = m["mods"]
            .as_array()
            .map(|a| a.iter().filter_map(|x| x["id"].as_str().map(String::from)).collect())
            .unwrap_or_default();
        ids.sort();
        Some(ids)
    }

    /// The selected mods' manifests (the build writes any the embedded project lacks into its copy).
    pub fn selected_mod_infos(&self) -> Vec<ModInfo> {
        self.catalog.mods.iter().filter(|m| self.prefs.mods.contains(&m.id)).cloned().collect()
    }

    pub(crate) fn catalog_page(&mut self, ui: &mut egui::Ui) {
        self.load_catalog();
        ui.heading("Mods");
        ui.label("Tick the mods you want, then rebuild: they are compiled into your game. Settings for a mod are in the game's F10 menu.");
        ui.add_space(6.);
        ui.horizontal(|ui| {
            if ui.add_enabled(self.catalog.rx.is_none(), egui::Button::new("Refresh list")).clicked() {
                self.refresh_catalog();
            }
            if self.catalog.rx.is_some() {
                ui.spinner();
            }
            ui.small(&self.catalog.status);
        });
        ui.add_space(10.);
        let mut changed = false;
        for (tier, title, note) in [
            ("official", "Official", "Made and tested with each MKDSrust release."),
            ("community", "Community", "Made by players, reviewed by a moderator before it was added."),
        ] {
            let mods: Vec<ModInfo> = self.catalog.mods.iter().filter(|m| m.tier == tier).cloned().collect();
            ui.label(RichText::new(title).size(16.).strong());
            ui.small(note);
            if mods.is_empty() {
                ui.label(RichText::new("None yet.").color(Color32::GRAY));
            }
            for m in mods {
                card().show(ui, |ui| {
                    ui.set_min_width(ui.available_width());
                    ui.horizontal(|ui| {
                        let mut on = self.prefs.mods.contains(&m.id);
                        if ui.checkbox(&mut on, RichText::new(&m.name).size(15.).strong()).changed() {
                            if on {
                                self.prefs.mods.push(m.id.clone());
                            } else {
                                self.prefs.mods.retain(|id| id != &m.id);
                            }
                            changed = true;
                        }
                        ui.label(RichText::new(format!("{}  ·  {}  ·  by {}", m.version, m.category, m.authors.join(", "))).color(Color32::GRAY));
                    });
                    ui.label(&m.summary);
                    let here = self.catalog.embedded.iter().find(|(id, _)| id == &m.id).map(|(_, v)| v.clone());
                    let newer = here.as_ref().is_some_and(|v| {
                        matches!((semver::Version::parse(&m.version), semver::Version::parse(v)), (Ok(a), Ok(b)) if a > b)
                    });
                    if newer {
                        ui.horizontal_wrapped(|ui| {
                            ui.label(RichText::new(format!(
                                "Version {} is out; this launcher builds {}.",
                                m.version,
                                here.unwrap_or_default()
                            )).color(Color32::from_rgb(245, 194, 74)));
                            ui.hyperlink_to("Get the latest launcher", "https://github.com/CameronStorer/MKDSrust/releases/latest");
                        });
                    }
                    egui::CollapsingHeader::new("Details").id_salt(&m.id).show(ui, |ui| {
                        for paragraph in m.description.split("\n\n") {
                            ui.label(paragraph);
                        }
                        ui.hyperlink_to("Source and README on GitHub", format!("{REPO_TREE}{}", m.path));
                    });
                });
                ui.add_space(6.);
            }
            ui.add_space(10.);
        }
        if changed {
            self.prefs.mods.sort();
            self.prefs.mods.dedup();
            self.save();
        }
        let built = self.built_mods();
        card().show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            match &built {
                None => {
                    ui.label("Build your game on Home; the ticked mods are included.");
                }
                Some(b) if *b == self.prefs.mods => {
                    ui.label(RichText::new("Your game is built with exactly these mods.").color(TEAL));
                }
                Some(_) => {
                    ui.label(RichText::new("Your selection changed: rebuild to apply it.").color(Color32::from_rgb(245, 194, 74)));
                    let can = !self.busy && self.info.as_ref().is_some_and(|i| i.reference_match);
                    if ui.add_enabled(can, egui::Button::new("Rebuild now")).clicked() {
                        self.page = 0;
                        self.start_build();
                    }
                }
            }
        });
        ui.add_space(8.);
        ui.horizontal_wrapped(|ui| {
            ui.label("Made a mod?");
            ui.hyperlink_to("How to submit it", SUBMIT);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_keeps_valid_mods_only() {
        let good = serde_json::json!({
            "id": "widescreen", "name": "Widescreen", "version": "1.0.0", "authors": ["a"], "license": "MIT",
            "category": "Graphics", "summary": "s", "description": "d", "features": ["widescreen"],
            "tier": "official", "path": "Mods/widescreen",
        });
        let mut future = good.clone();
        future["id"] = "teleport".into();
        future["path"] = "Mods/teleport".into();
        future["features"] = serde_json::json!(["teleport"]);
        let mut escape = good.clone();
        escape["id"] = "x".into();
        escape["path"] = "../x".into();
        let index = serde_json::json!({ "format": 1, "mods": [good, future, escape] });
        let mods = parse_index(index.to_string().as_bytes()).unwrap();
        assert_eq!(mods.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), ["widescreen"]);
        assert!(parse_index(br#"{"format":2,"mods":[]}"#).is_err());
    }

    /// The repository's own index parses and keeps every mod.
    #[test]
    fn repository_index_parses() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let Ok(bytes) = std::fs::read(root.join("Mods/index.json")) else { return };
        let all = mkds_builder::mods_repo::list(root).unwrap();
        assert_eq!(parse_index(&bytes).unwrap().len(), all.len());
    }
}
