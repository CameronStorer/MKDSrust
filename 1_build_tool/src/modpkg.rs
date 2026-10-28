//! Package contract shared by the builder, installer and native overlay.
//! All screening happens before extraction; no archive paths are trusted.
pub use qbsdiff;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{Read, Write},
    path::Path,
};

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
pub const MAX_ZIP: u64 = 100 * 1024 * 1024;
pub const MAX_UNPACKED: u64 = 400 * 1024 * 1024;
pub const MAX_FILES: usize = 5000;

pub fn hash(bytes: &[u8]) -> String {
    crate::rom::hex(&Sha256::digest(bytes))
}
pub fn format_timestamp(seconds: i64) -> Option<String> {
    if !(0..=253402300799).contains(&seconds) {
        return None;
    }
    let mut days = seconds / 86400;
    let mut year = 1970;
    let leap = |y| y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
    loop {
        let length = if leap(year) { 366 } else { 365 };
        if days < length {
            break;
        }
        days -= length;
        year += 1;
    }
    let lengths = [
        31,
        if leap(year) { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut month = 1;
    for length in lengths {
        if days < length {
            break;
        }
        days -= length;
        month += 1;
    }
    let time = seconds % 86400;
    Some(format!(
        "{year:04}-{month:02}-{:02}T{:02}:{:02}:{:02}Z",
        days + 1,
        time / 3600,
        time / 60 % 60,
        time % 60
    ))
}
pub fn safe_path(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= 200
        && !path
            .chars()
            .any(|c| c.is_control() || "\\:<>\"|?*".contains(c))
        && path.split('/').all(|s| {
            let stem = s.split('.').next().unwrap_or("").to_ascii_lowercase();
            !s.is_empty()
                && s != "."
                && s != ".."
                && !s.ends_with([' ', '.'])
                && ![
                    "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6",
                    "com7", "com8", "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7",
                    "lpt8", "lpt9",
                ]
                .contains(&stem.as_str())
        })
}
pub fn valid_id(id: &str) -> bool {
    (3..=64).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b))
        && id != ".."
        && id != ".cache"
        && id != "enabled.txt"
        && safe_path(id)
        && !id.contains('/')
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub format: u32,
    #[serde(rename = "type")]
    pub kind: String,
    pub id: String,
    pub name: String,
    pub version: String,
    pub author: String,
    pub game: String,
    pub license: Option<String>,
    #[serde(default)]
    pub description: String,
    pub files: BTreeMap<String, Entry>,
    #[serde(default)]
    pub depends: Vec<String>,
    #[serde(default)]
    pub conflicts: Vec<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Entry {
    Patch(Patch),
    Add(Add),
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Patch {
    pub patch: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Add {
    pub add: String,
}
impl Entry {
    pub fn source(&self) -> &str {
        match self {
            Self::Patch(p) => &p.patch,
            Self::Add(a) => &a.add,
        }
    }
}
impl Manifest {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > 1024 * 1024 {
            return Err("Manifest too large".into());
        }
        let m: Self = serde_json::from_slice(bytes)?;
        if m.format != 1
            || m.game != "AMCE"
            || !valid_id(&m.id)
            || ![
                "character",
                "track",
                "kart",
                "battle",
                "music",
                "texture",
                "other",
            ]
            .contains(&m.kind.as_str())
            || m.name.trim().is_empty()
            || m.author.trim().is_empty()
            || m.description.chars().count() > 2000
            || m.files.is_empty()
            || m.files.len() > MAX_FILES
        {
            return Err("Invalid mod manifest fields".into());
        }
        semver::Version::parse(&m.version)?;
        let mut targets = BTreeSet::new();
        for (target, entry) in &m.files {
            if !safe_path(target)
                || !safe_path(entry.source())
                || !targets.insert(target.to_ascii_lowercase())
            {
                return Err("Unsafe or duplicate mod path".into());
            }
        }
        for dep in &m.depends {
            let (id, req) = dep
                .split_once('@')
                .ok_or("Dependency needs id@version requirement")?;
            if !valid_id(id) || id == m.id {
                return Err("Invalid dependency".into());
            }
            semver::VersionReq::parse(req)?;
        }
        if m.conflicts.iter().any(|id| !valid_id(id) || id == &m.id) {
            return Err("Invalid conflict ID".into());
        }
        Ok(m)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RetailFile {
    pub path: String,
    pub size: u64,
    pub sha256: String,
    pub blocks: Vec<String>,
    #[serde(skip)]
    rolling: Vec<u64>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RetailIndex {
    pub format: u32,
    pub game: String,
    pub files: Vec<RetailFile>,
    /// Additional hashes for non-path-addressable FAT/system sections; never patch targets.
    #[serde(default)]
    pub system_files: Vec<RetailFile>,
}
impl RetailIndex {
    pub fn from_rom(rom: &[u8]) -> Result<Self> {
        let fs = crate::rom::NitroFs::parse(rom)?;
        let mut paths: Vec<_> = fs.paths().collect();
        paths.sort();
        let word = |off: usize| u32::from_le_bytes(rom[off..off + 4].try_into().unwrap()) as usize;
        let record = |path: String, data: &[u8]| RetailFile {
            path,
            size: data.len() as u64,
            sha256: hash(data),
            blocks: blocks(data),
            rolling: data.chunks_exact(4096).map(rolling_hash).collect(),
        };
        let mut system_files = vec![record("system/header.bin".into(), &rom[..0x200])];
        for (name, off, length) in [
            ("arm9.bin", 0x20, 0x2c),
            ("arm7.bin", 0x30, 0x3c),
            ("fnt.bin", 0x40, 0x44),
            ("fat.bin", 0x48, 0x4c),
            ("arm9_overlay_table.bin", 0x50, 0x54),
            ("arm7_overlay_table.bin", 0x58, 0x5c),
        ] {
            let start = word(off);
            let len = word(length);
            if len > 0 {
                let end = start.checked_add(len).ok_or("System range overflow")?;
                let data = rom.get(start..end).ok_or("Invalid system range")?;
                system_files.push(record(format!("system/{name}"), data));
            }
        }
        let banner = word(0x68);
        if banner != 0 {
            let version = rom.get(banner..banner + 2).ok_or("Invalid banner")?;
            let len = match u16::from_le_bytes(version.try_into().unwrap()) {
                2 => 0x940,
                3 => 0xa40,
                0x103 => 0x23c0,
                _ => 0x840,
            };
            system_files.push(record(
                "system/banner.bin".into(),
                rom.get(banner..banner + len).ok_or("Truncated banner")?,
            ));
        }
        let fnt = &rom[word(0x40)..word(0x40) + word(0x44)];
        let named: BTreeSet<_> = crate::rom::list_files_with_ids(fnt, word(0x4c) / 8)?
            .into_iter()
            .map(|(_, id)| id)
            .collect();
        for id in 0..word(0x4c) / 8 {
            if !named.contains(&id) {
                let off = word(0x48) + id * 8;
                let data = rom
                    .get(word(off)..word(off + 4))
                    .ok_or("Invalid unnamed FAT range")?;
                system_files.push(record(format!("system/fat_{id:05}.bin"), data));
            }
        }
        Ok(Self {
            format: 1,
            game: "AMCE".into(),
            system_files,
            files: paths
                .into_iter()
                .map(|path| {
                    let data = fs.file(path).unwrap();
                    RetailFile {
                        path: path.into(),
                        size: data.len() as u64,
                        sha256: hash(data),
                        blocks: blocks(data),
                        rolling: data.chunks_exact(4096).map(rolling_hash).collect(),
                    }
                })
                .collect(),
        })
    }
}
fn blocks(data: &[u8]) -> Vec<String> {
    data.chunks(4096).map(|b| hash(b)[..16].into()).collect()
}
fn rolling_hash(data: &[u8]) -> u64 {
    data.iter()
        .fold(0u64, |h, b| h.wrapping_mul(257).wrapping_add(*b as u64))
}

fn ds_header(bytes: &[u8]) -> bool {
    // Nintendo logo prefix or a structurally plausible NDS header, including homebrew.
    if bytes.len() < 0x160 {
        return false;
    }
    if bytes[0xc0..0xc8] == [0x24, 0xff, 0xae, 0x51, 0x69, 0x9a, 0xa2, 0x21] {
        return true;
    }
    let word = |off| u32::from_le_bytes(bytes[off..off + 4].try_into().unwrap()) as usize;
    let section = |off, len| {
        word(off) >= 0x200
            && word(len) > 0
            && word(off)
                .checked_add(word(len))
                .is_some_and(|end| end <= bytes.len())
    };
    bytes[12..16].iter().all(|b| b.is_ascii_alphanumeric())
        && section(0x20, 0x2c)
        && section(0x30, 0x3c)
}
fn near_retail(data: &[u8], f: &RetailFile) -> bool {
    let size = data.len() as u64;
    if f.size == 0 || size * 100 < f.size * 95 || size * 100 > f.size * 105 {
        return false;
    }
    let theirs: BTreeSet<_> = f.blocks.iter().collect();
    let ours = blocks(data);
    let hits = ours.iter().filter(|h| theirs.contains(h)).count();
    if !ours.is_empty() && hits * 2 >= ours.len() {
        return true;
    }
    // Search shifted 4KiB windows, not only aligned blocks (inserted prefixes).
    let mut matched = BTreeSet::new();
    let fingerprints: BTreeSet<_> = f.rolling.iter().copied().collect();
    let mut rolling = data.get(..4096).map(rolling_hash).unwrap_or(0);
    let power = (0..4095).fold(1u64, |p, _| p.wrapping_mul(257));
    for (i, chunk) in data.windows(4096).enumerate() {
        if fingerprints.is_empty() || fingerprints.contains(&rolling) {
            let h = hash(chunk);
            if theirs.iter().any(|t| t.as_str() == &h[..16]) {
                matched.insert(h[..16].to_string());
            }
            if f.blocks.iter().filter(|h| matched.contains(*h)).count() * 2 >= f.blocks.len() {
                return true;
            }
        }
        if let Some(next) = data.get(i + 4096) {
            rolling = rolling
                .wrapping_sub((data[i] as u64).wrapping_mul(power))
                .wrapping_mul(257)
                .wrapping_add(*next as u64);
        }
    }
    false
}

pub struct Package {
    pub manifest: Manifest,
    pub contents: BTreeMap<String, Vec<u8>>,
    pub warnings: Vec<String>,
}
impl Package {
    pub fn read(path: &Path, retail: &RetailIndex) -> Result<Self> {
        if std::fs::metadata(path)?.len() > MAX_ZIP {
            return Err("Zip exceeds 100 MB".into());
        }
        let mut zip = zip::ZipArchive::new(std::fs::File::open(path)?)?;
        if zip.len() > MAX_FILES {
            return Err("Too many archive entries".into());
        }
        let mut contents = BTreeMap::new();
        let mut seen = BTreeSet::new();
        let mut total = 0u64;
        for i in 0..zip.len() {
            let mut file = zip.by_index(i)?;
            let name = file.name().to_string();
            let rel = if file.is_dir() {
                name.trim_end_matches('/')
            } else {
                &name
            };
            if !safe_path(rel)
                || !seen.insert(rel.to_ascii_lowercase())
                || file.unix_mode().is_some_and(|m| {
                    m & 0o170000 != 0 && m & 0o170000 != 0o100000 && m & 0o170000 != 0o040000
                })
            {
                return Err("Unsafe, duplicate or linked zip entry".into());
            }
            if file.is_dir() {
                continue;
            }
            total = total
                .checked_add(file.size())
                .ok_or("Archive size overflow")?;
            if total > MAX_UNPACKED {
                return Err("Zip exceeds 400 MB unpacked".into());
            }
            let mut bytes = Vec::new();
            (&mut file).take(MAX_UNPACKED + 1).read_to_end(&mut bytes)?;
            if bytes.len() as u64 != file.size() {
                return Err("Archive size mismatch".into());
            }
            contents.insert(name, bytes);
        }
        Self::validate(contents, retail)
    }
    pub fn validate(contents: BTreeMap<String, Vec<u8>>, retail: &RetailIndex) -> Result<Self> {
        if retail.format != 1 || retail.game != "AMCE" {
            return Err("Unsupported retail index".into());
        }
        let manifest = Manifest::parse(
            contents
                .get("manifest.json")
                .ok_or("Missing root manifest.json")?,
        )?;
        if contents.len() > MAX_FILES
            || contents.values().map(|b| b.len() as u64).sum::<u64>() > MAX_UNPACKED
        {
            return Err("Package exceeds limits".into());
        }
        let mut warnings = Vec::new();
        for (name, bytes) in &contents {
            let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
            if !safe_path(name) || "nds srl dsi sav dsv gba 3ds cia nsp xci exe dll bat cmd com scr msi ps1 vbs sh jar app apk".split(' ').any(|x| x == ext) || ds_header(bytes) {
                return Err(format!("Forbidden package file: {name}").into());
            }
            let sha = hash(bytes);
            if retail
                .files
                .iter()
                .chain(&retail.system_files)
                .any(|f| f.sha256 == sha)
            {
                return Err(format!("Retail bytes in package: {name}").into());
            }
            if retail
                .files
                .iter()
                .chain(&retail.system_files)
                .any(|f| near_retail(bytes, f))
            {
                return Err(format!("Near-retail bytes in package: {name}").into());
            }
        }
        for (target, entry) in &manifest.files {
            if retail
                .files
                .iter()
                .map(|f| &f.path)
                .chain(manifest.files.keys())
                .any(|p| {
                    p != target
                        && (p.starts_with(&(target.to_string() + "/"))
                            || target.starts_with(&(p.to_string() + "/")))
                })
            {
                return Err("File/directory path collision".into());
            }
            let bytes = contents.get(entry.source()).ok_or("Missing payload")?;
            let original = retail.files.iter().find(|f| &f.path == target);
            match entry {
                Entry::Patch(_) => {
                    let f = original.ok_or("Patch target is absent from ROM")?;
                    let patch = qbsdiff::Bspatch::new(bytes)?;
                    if patch.hint_target_size() > MAX_UNPACKED {
                        return Err("Patch output too large".into());
                    }
                    if f.size >= 65536 && bytes.len() as u64 * 100 >= f.size * 60 {
                        // Local installer cannot provide the site's moderator review.
                        return Err("Patch >=60% of target: manual review required".into());
                    }
                    if f.size < 65536 {
                        warnings.push(format!("Small patch target: {target}"));
                    }
                }
                Entry::Add(_) => {
                    if original.is_some() {
                        return Err("add may not replace a ROM file".into());
                    }
                }
            }
        }
        Ok(Self {
            manifest,
            contents,
            warnings,
        })
    }
    pub fn install(&self, game: &Path) -> Result<()> {
        if !valid_id(&self.manifest.id) || self.contents.keys().any(|p| !safe_path(p)) {
            return Err("Unsafe installation paths".into());
        }
        let mods = game.join("mods");
        std::fs::create_dir_all(&mods)?;
        let dest = mods.join(&self.manifest.id);
        if dest.exists() {
            return Err("Mod already installed; remove it first".into());
        }
        let staging = tempfile::tempdir_in(&mods)?;
        for (name, bytes) in &self.contents {
            let path = staging.path().join(name);
            std::fs::create_dir_all(path.parent().unwrap())?;
            std::fs::write(path, bytes)?;
        }
        std::fs::rename(staging.path(), dest)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn retail(data: &[u8]) -> RetailIndex {
        RetailIndex {
            format: 1,
            game: "AMCE".into(),
            system_files: vec![],
            files: vec![RetailFile {
                path: "original.bin".into(),
                size: data.len() as u64,
                sha256: hash(data),
                blocks: blocks(data),
                rolling: data.chunks_exact(4096).map(rolling_hash).collect(),
            }],
        }
    }
    fn package(payload: &[u8]) -> BTreeMap<String, Vec<u8>> {
        let manifest = serde_json::json!({"format":1,"type":"other","id":"test.synthetic","name":"Synthetic","version":"1.0.0","author":"Test","game":"AMCE","files":{"new.bin":{"add":"files/new.bin"}}});
        BTreeMap::from([
            (
                "manifest.json".into(),
                serde_json::to_vec(&manifest).unwrap(),
            ),
            ("files/new.bin".into(), payload.to_vec()),
        ])
    }
    #[test]
    fn core_safe_paths_and_ids() {
        for p in [
            "../escape",
            "/root",
            "C:/drive",
            "a\\b",
            "a/../b",
            "a//b",
            "a:stream",
            "con.txt",
            "a. ",
            "nul",
            "x\0y",
        ] {
            assert!(!safe_path(p), "{p:?}");
        }
        assert!(safe_path("files/subdir/custom.bin"));
        assert!(valid_id("author.a-mod"));
        assert!(!valid_id(".cache"));
        assert!(!valid_id("enabled.txt"));
    }
    #[test]
    fn core_size_file_count_and_description_limits() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("oversize.zip");
        std::fs::File::create(&path)
            .unwrap()
            .set_len(MAX_ZIP + 1)
            .unwrap();
        assert!(
            Package::read(&path, &retail(b"fixture"))
                .err()
                .unwrap()
                .to_string()
                .contains("100 MB")
        );
        let mut contents = package(b"original");
        for i in 0..MAX_FILES {
            contents.insert(format!("extra/{i}.txt"), vec![]);
        }
        assert!(
            Package::validate(contents, &retail(b"fixture"))
                .err()
                .unwrap()
                .to_string()
                .contains("limits")
        );
        let mut contents = package(b"original");
        let mut m: serde_json::Value = serde_json::from_slice(&contents["manifest.json"]).unwrap();
        m["description"] = "x".repeat(2001).into();
        contents.insert("manifest.json".into(), serde_json::to_vec(&m).unwrap());
        assert!(Package::validate(contents, &retail(b"fixture")).is_err());
    }
    #[test]
    fn core_manifest_rejects_unknown_and_ambiguous_entries() {
        let p = package(b"custom creation");
        let mut json: serde_json::Value = serde_json::from_slice(&p["manifest.json"]).unwrap();
        for bad in [
            serde_json::json!({"patch":"p", "add":"a"}),
            serde_json::json!({"replace":"x"}),
            serde_json::json!({"add":"../x"}),
        ] {
            json["files"]["new.bin"] = bad;
            assert!(Manifest::parse(&serde_json::to_vec(&json).unwrap()).is_err());
        }
    }
    #[test]
    fn core_retail_and_shifted_similarity_screening() {
        let data: Vec<_> = (0..16384)
            .map(|i| ((i * i / 11 + i * 17) % 251) as u8)
            .collect();
        let index = retail(&data);
        assert!(Package::validate(package(&data), &index).is_err());
        let mut near = vec![35; 73];
        near.extend_from_slice(&data);
        near.truncate(data.len());
        assert!(near_retail(&near, &index.files[0]));
        assert!(Package::validate(package(&near), &index).is_err());
        assert!(Package::validate(package(b"entirely original content"), &index).is_ok());
    }
    #[test]
    fn core_detects_rom_headers_and_blocked_extensions_in_unreferenced_files() {
        let index = retail(b"retail fixture");
        let mut rom = vec![0; 0x200];
        rom[0xc0..0xc8].copy_from_slice(&[0x24, 0xff, 0xae, 0x51, 0x69, 0x9a, 0xa2, 0x21]);
        assert!(Package::validate(package(&rom), &index).is_err());
        for extension in "nds srl dsi sav dsv gba 3ds cia nsp xci exe dll bat cmd com scr msi ps1 vbs sh jar app apk".split(' ') {
            let mut p = package(b"original"); p.insert(format!("extra.{extension}"), b"forbidden".to_vec());
            assert!(Package::validate(p, &index).is_err());
        }
    }
    #[test]
    fn core_add_cannot_replace_a_rom_path_and_installs_atomically() {
        let index = retail(b"retail fixture");
        let p = package(b"original creation");
        let mut json: serde_json::Value = serde_json::from_slice(&p["manifest.json"]).unwrap();
        json["files"] = serde_json::json!({"original.bin":{"add":"files/new.bin"}});
        let mut bad = p.clone();
        bad.insert("manifest.json".into(), serde_json::to_vec(&json).unwrap());
        assert!(Package::validate(bad, &index).is_err());
        let pkg = Package::validate(p, &index).unwrap();
        let game = tempfile::tempdir().unwrap();
        pkg.install(game.path()).unwrap();
        assert!(pkg.install(game.path()).is_err());
        assert_eq!(
            std::fs::read(game.path().join("mods/test.synthetic/files/new.bin")).unwrap(),
            b"original creation"
        );
        save_enabled(game.path(), &[pkg.manifest.id.clone()]).unwrap();
        assert_eq!(enabled(game.path()), ["test.synthetic"]);
    }
    #[test]
    fn core_dependencies_conflicts_and_dates() {
        let p = package(b"original");
        let mut m = Manifest::parse(&p["manifest.json"]).unwrap();
        m.depends = vec!["other.mod@>=1.0.0".into()];
        assert_eq!(issues(&m, &[]).len(), 1);
        let mut other = m.clone();
        other.id = "other.mod".into();
        other.depends.clear();
        assert!(issues(&m, &[other.clone()]).is_empty());
        other.conflicts.push(m.id.clone());
        assert_eq!(issues(&m, &[other]).len(), 1);
        assert_eq!(format_timestamp(0).unwrap(), "1970-01-01T00:00:00Z");
        assert_eq!(format_timestamp(951782400).unwrap(), "2000-02-29T00:00:00Z");
        assert_eq!(
            format_timestamp(253402300799).unwrap(),
            "9999-12-31T23:59:59Z"
        );
    }
    #[test]
    fn patch_helpers_round_trip_and_zip_installs() {
        let dir = tempfile::tempdir().unwrap();
        let original = vec![17u8; 8192];
        let mut modified = original.clone();
        modified[123] = 31;
        let source = dir.path().join("source.bin");
        let target = dir.path().join("target.bin");
        let patch = dir.path().join("delta.bsdiff");
        std::fs::write(&source, &original).unwrap();
        std::fs::write(&target, &modified).unwrap();
        make_patch(&source, &target, &patch).unwrap();
        let bytes = std::fs::read(&patch).unwrap();
        let mut actual = Vec::new();
        qbsdiff::Bspatch::new(&bytes)
            .unwrap()
            .apply(&original, &mut actual)
            .unwrap();
        assert_eq!(actual, modified);
        let moddir = dir.path().join("mod");
        std::fs::create_dir(&moddir).unwrap();
        std::fs::write(moddir.join("delta.bsdiff"), bytes).unwrap();
        let manifest = serde_json::json!({"format":1,"type":"other","id":"test.patch","name":"Patch","version":"1.0.0","author":"Test","game":"AMCE","files":{"original.bin":{"patch":"delta.bsdiff"}}});
        std::fs::write(
            moddir.join("manifest.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        let archive = pack(&moddir).unwrap();
        let pkg = Package::read(&archive, &retail(&original)).unwrap();
        let game = dir.path().join("game");
        pkg.install(&game).unwrap();
        assert!(game.join("mods/test.patch/manifest.json").is_file());
    }
    #[test]
    fn hostile_zip_paths_symlinks_and_case_collisions() {
        let dir = tempfile::tempdir().unwrap();
        for (i, names) in [
            vec!["../escape"],
            vec!["/absolute"],
            vec!["C:/drive"],
            vec!["A", "a"],
        ]
        .into_iter()
        .enumerate()
        {
            let path = dir.path().join(format!("{i}.zip"));
            let mut zip = zip::ZipWriter::new(std::fs::File::create(&path).unwrap());
            for name in names {
                zip.start_file(name, zip::write::SimpleFileOptions::default())
                    .unwrap();
                zip.write_all(b"test").unwrap();
            }
            zip.finish().unwrap();
            assert!(Package::read(&path, &retail(b"retail")).is_err());
        }
        let path = dir.path().join("link.zip");
        let mut zip = zip::ZipWriter::new(std::fs::File::create(&path).unwrap());
        zip.add_symlink("link", "outside", zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.finish().unwrap();
        assert!(Package::read(&path, &retail(b"retail")).is_err());
        assert!(!dir.path().join("escape").exists());
    }
}

pub fn enabled(game: &Path) -> Vec<String> {
    std::fs::read_to_string(game.join("mods/enabled.txt"))
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|id| valid_id(id))
        .map(str::to_string)
        .collect()
}
pub fn save_enabled(game: &Path, ids: &[String]) -> Result<()> {
    if ids.iter().any(|s| !valid_id(s)) {
        return Err("Invalid enabled ID".into());
    }
    std::fs::create_dir_all(game.join("mods"))?;
    let mut staged = tempfile::NamedTempFile::new_in(game.join("mods"))?;
    staged.write_all((ids.join("\n") + "\n").as_bytes())?;
    staged.as_file().sync_all()?;
    staged.persist(game.join("mods/enabled.txt"))?;
    Ok(())
}
pub fn installed(game: &Path) -> Vec<Manifest> {
    let mut mods = Vec::new();
    if let Ok(entries) = std::fs::read_dir(game.join("mods")) {
        for entry in entries.flatten() {
            if entry
                .file_type()
                .is_ok_and(|t| t.is_dir() && !t.is_symlink())
            {
                if let Ok(bytes) = std::fs::read(entry.path().join("manifest.json")) {
                    if let Ok(m) = Manifest::parse(&bytes) {
                        if entry.file_name().to_str() == Some(&m.id) {
                            mods.push(m);
                        }
                    }
                }
            }
        }
    }
    mods.sort_by(|a, b| a.id.cmp(&b.id));
    mods
}
pub fn issues(m: &Manifest, active: &[Manifest]) -> Vec<String> {
    let mut warnings = Vec::new();
    for dep in &m.depends {
        let (id, req) = dep.split_once('@').unwrap();
        let req = semver::VersionReq::parse(req).unwrap();
        if !active.iter().any(|a| {
            a.id == id && semver::Version::parse(&a.version).is_ok_and(|v| req.matches(&v))
        }) {
            warnings.push(format!("{}: missing dependency {dep}", m.id));
        }
    }
    for other in active {
        if other.id != m.id && (m.conflicts.contains(&other.id) || other.conflicts.contains(&m.id))
        {
            warnings.push(format!("{} conflicts with {}", m.id, other.id));
        }
    }
    warnings
}
pub fn make_patch(original: &Path, modified: &Path, out: &Path) -> Result<()> {
    let source = std::fs::read(original)?;
    let target = std::fs::read(modified)?;
    if source.len() as u64 > MAX_UNPACKED || target.len() as u64 > MAX_UNPACKED {
        return Err("Patch inputs too large".into());
    }
    let mut patch = Vec::new();
    qbsdiff::Bsdiff::new(&source, &target).compare(&mut patch)?;
    std::fs::write(out, patch)?;
    Ok(())
}
pub fn pack(dir: &Path) -> Result<std::path::PathBuf> {
    let manifest_bytes = std::fs::read(dir.join("manifest.json"))?;
    let m = Manifest::parse(&manifest_bytes)?;
    if m.files.values().any(|e| matches!(e, Entry::Add(_))) {
        return Err("--pack-mod creates patch-only packages".into());
    }
    let mut contents = BTreeMap::from([("manifest.json".to_string(), manifest_bytes)]);
    let root = dir.canonicalize()?;
    for e in m.files.values() {
        let path = root.join(e.source()).canonicalize()?;
        if !path.starts_with(&root) {
            return Err("Payload escapes mod directory".into());
        }
        let data = std::fs::read(path)?;
        let ext = e
            .source()
            .rsplit('.')
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        if ext != "bsdiff" || ds_header(&data) {
            return Err("Patch payload must be a .bsdiff file".into());
        }
        if data.len() as u64 > MAX_ZIP {
            return Err("Patch too large".into());
        }
        qbsdiff::Bspatch::new(&data)?;
        contents.insert(e.source().into(), data);
    }
    if contents.len() > MAX_FILES
        || contents.values().map(|b| b.len() as u64).sum::<u64>() > MAX_UNPACKED
    {
        return Err("Package exceeds limits".into());
    }
    let mut out = dir.as_os_str().to_os_string();
    out.push(".zip");
    let out = std::path::PathBuf::from(out);
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&out)?;
    let mut zip = zip::ZipWriter::new(file);
    for (name, data) in contents {
        zip.start_file(name, zip::write::SimpleFileOptions::default())?;
        zip.write_all(&data)?;
    }
    let file = zip.finish()?;
    if file.metadata()?.len() > MAX_ZIP {
        drop(file);
        std::fs::remove_file(&out)?;
        return Err("Zip exceeds 100 MB".into());
    }
    Ok(out)
}
