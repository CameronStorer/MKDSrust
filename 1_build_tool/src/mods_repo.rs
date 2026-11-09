//! The mod repository: `Mods/<id>/mod.json` (official) and `Mods/community/<id>/mod.json` (community) in the
//! source checkout. A mod is chosen before a build and compiled into the game; `Mods/index.json` lists them all for
//! the launcher and the website.
//!
//! A mod switches on runtime `features` (code that ships with MKDSrust, such as the widescreen renderer). Mods are
//! original code only: never game data, and never code copied from the source generated from a ROM.
use crate::rom::BuildError;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Runtime features a mod may switch on. A mod naming anything else needs a newer MKDSrust.
pub const FEATURES: &[&str] = &["widescreen"];

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModInfo {
    /// Folder name and identifier: lowercase letters, digits and `-`.
    pub id: String,
    pub name: String,
    pub version: String,
    pub authors: Vec<String>,
    /// SPDX identifier; must be GPL-3.0 compatible.
    pub license: String,
    pub category: String,
    /// One sentence.
    pub summary: String,
    /// A few paragraphs, plain text.
    pub description: String,
    pub features: Vec<String>,
    /// `official` (in `Mods/`) or `community` (in `Mods/community/`); filled in from the folder.
    #[serde(default)]
    pub tier: String,
    /// Path of the mod folder relative to the checkout, e.g. `Mods/widescreen`; filled in from the folder.
    #[serde(default)]
    pub path: String,
}

const LICENSES: &[&str] = &[
    "GPL-3.0-or-later", "GPL-3.0-only", "MIT", "Apache-2.0", "BSD-2-Clause", "BSD-3-Clause", "ISC", "0BSD",
    "Zlib", "CC0-1.0", "Unlicense", "MPL-2.0", "LGPL-3.0-or-later",
];

fn bad(what: String) -> BuildError {
    BuildError(what)
}

/// Checks one manifest's fields (the folder name must equal the id).
pub fn validate(info: &ModInfo, folder: &str) -> Result<(), BuildError> {
    let id_ok = !info.id.is_empty()
        && info.id.len() <= 40
        && info.id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if !id_ok || info.id != folder {
        return Err(bad(format!("mod `{folder}`: the id must equal the folder name and use a-z, 0-9 and -")));
    }
    if semver::Version::parse(&info.version).is_err() {
        return Err(bad(format!("mod `{folder}`: version `{}` is not x.y.z", info.version)));
    }
    if info.name.trim().is_empty() || info.summary.trim().is_empty() || info.authors.is_empty() {
        return Err(bad(format!("mod `{folder}`: name, summary and authors are required")));
    }
    if !LICENSES.contains(&info.license.as_str()) {
        return Err(bad(format!("mod `{folder}`: license `{}` is not on the GPL-compatible list", info.license)));
    }
    if let Some(f) = info.features.iter().find(|f| !FEATURES.contains(&f.as_str())) {
        return Err(bad(format!("mod `{folder}` needs runtime feature `{f}`, which this MKDSrust does not have")));
    }
    Ok(())
}

/// Every mod in the checkout, official first, each sorted by id.
pub fn list(source: &Path) -> Result<Vec<ModInfo>, BuildError> {
    let mut out = Vec::new();
    for (tier, dir) in [("official", source.join("Mods")), ("community", source.join("Mods/community"))] {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        let mut found = Vec::new();
        for entry in entries {
            let entry = entry?;
            let folder = entry.file_name().to_string_lossy().into_owned();
            let manifest = entry.path().join("mod.json");
            if !entry.file_type()?.is_dir() || !manifest.is_file() {
                continue;
            }
            let text = std::fs::read_to_string(&manifest)?;
            let mut info: ModInfo = serde_json::from_str(&text)
                .map_err(|e| bad(format!("{}: {e}", manifest.display())))?;
            validate(&info, &folder)?;
            info.tier = tier.into();
            info.path = if tier == "official" { format!("Mods/{folder}") } else { format!("Mods/community/{folder}") };
            found.push(info);
        }
        found.sort_by(|a, b| a.id.cmp(&b.id));
        out.extend(found);
    }
    let mut ids: Vec<&str> = out.iter().map(|m| m.id.as_str()).collect();
    ids.sort_unstable();
    if let Some(w) = ids.windows(2).find(|w| w[0] == w[1]) {
        return Err(bad(format!("two mods share the id `{}`", w[0])));
    }
    Ok(out)
}

/// The mods with these ids, in the order given; an unknown id is an error.
pub fn resolve(source: &Path, ids: &[String]) -> Result<Vec<ModInfo>, BuildError> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let all = list(source)?;
    ids.iter()
        .map(|id| {
            all.iter()
                .find(|m| &m.id == id)
                .cloned()
                .ok_or_else(|| bad(format!("mod `{id}` is not in {}", source.join("Mods").display())))
        })
        .collect()
}

/// `Mods/index.json`: every mod's manifest, for the launcher and the website.
pub fn index_json(source: &Path) -> Result<String, BuildError> {
    let mods = list(source)?;
    let text = serde_json::to_string_pretty(&serde_json::json!({ "format": 1, "mods": mods }))
        .map_err(|e| bad(e.to_string()))?;
    Ok(text + "\n")
}

/// Where the index lives in a checkout.
pub fn index_path(source: &Path) -> PathBuf {
    source.join("Mods/index.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, id: &str, features: &[&str]) {
        std::fs::create_dir_all(dir.join(id)).unwrap();
        let info = serde_json::json!({
            "id": id, "name": "Test", "version": "1.0.0", "authors": ["me"], "license": "MIT",
            "category": "Graphics", "summary": "s", "description": "d", "features": features,
        });
        std::fs::write(dir.join(id).join("mod.json"), info.to_string()).unwrap();
    }

    #[test]
    fn lists_resolves_and_rejects() {
        let t = tempfile::tempdir().unwrap();
        write(&t.path().join("Mods"), "widescreen", &["widescreen"]);
        write(&t.path().join("Mods/community"), "extra", &[]);
        let all = list(t.path()).unwrap();
        assert_eq!(all.iter().map(|m| (m.id.as_str(), m.tier.as_str())).collect::<Vec<_>>(), [("widescreen", "official"), ("extra", "community")]);
        assert_eq!(all[1].path, "Mods/community/extra");
        assert_eq!(resolve(t.path(), &["widescreen".into()]).unwrap()[0].features, ["widescreen"]);
        assert!(resolve(t.path(), &["nope".into()]).is_err());
        write(&t.path().join("Mods"), "future", &["teleport"]);
        assert!(list(t.path()).unwrap_err().0.contains("teleport"));
    }

    /// The real repository: every manifest is valid and `Mods/index.json` is up to date.
    #[test]
    fn repository_index_is_current() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        if !root.join("Mods").is_dir() {
            return;
        }
        let expected = index_json(root).unwrap();
        let actual = std::fs::read_to_string(index_path(root)).unwrap_or_default().replace("\r\n", "\n");
        assert!(actual == expected, "Mods/index.json is stale: run `mkds_builder --mods-index`");
    }
}
