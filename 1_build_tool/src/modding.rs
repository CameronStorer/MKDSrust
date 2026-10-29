//! Mod workspaces: "make a mod" without hand-writing patches.
//!
//! `create` unpacks the player's own ROM files into `<root>/<id>/romfs/` (optionally with every `.carc`/`.narc`
//! extracted into a `<name>.unpacked/` folder) next to a draft `manifest.json` and a README. The author edits files
//! with any DS tool. `pack` compares `romfs/` with the ROM: changed files become bsdiff patches, new allowlisted
//! files become `add` entries, unchanged files are left out. The zip is then screened with the same rules as the
//! site (`modpkg::Package`), so what packs here uploads there. Workspaces live outside every repository; they hold
//! the player's own game files and are never published.
use crate::{dsfiles, modpkg};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Write,
    path::{Path, PathBuf},
};

type Result<T> = modpkg::Result<T>;

/// Folder suffix for an extracted archive: `romfs/a/b.carc.unpacked/`.
pub const UNPACKED: &str = ".unpacked";
/// Author-supplied package extras copied into the zip as-is (README.md, preview images).
pub const EXTRAS: &str = "package";
const STATE: &str = ".workspace.json";

/// The workspace's `manifest.json`: a package manifest whose `files` are filled in by `pack`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Draft {
    #[serde(default = "one")]
    pub format: u32,
    #[serde(rename = "type")]
    pub kind: String,
    pub id: String,
    pub name: String,
    pub version: String,
    pub author: String,
    #[serde(default = "amce")]
    pub game: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
    #[serde(default)]
    pub description: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conflicts: Vec<String>,
}
fn one() -> u32 {
    1
}
fn amce() -> String {
    "AMCE".into()
}
impl Draft {
    pub fn new(id: &str, name: &str, kind: &str, author: &str) -> Self {
        Self {
            format: 1,
            kind: kind.into(),
            id: id.into(),
            name: name.into(),
            version: "1.0.0".into(),
            author: author.into(),
            game: amce(),
            license: None,
            description: String::new(),
            depends: vec![],
            conflicts: vec![],
        }
    }
    /// The package manifest for these files (validated like any downloaded package).
    pub fn manifest(&self, files: BTreeMap<String, modpkg::Entry>) -> Result<(modpkg::Manifest, Vec<u8>)> {
        let mut v = serde_json::to_value(self)?;
        v["files"] = serde_json::to_value(&files)?;
        let bytes = serde_json::to_vec_pretty(&v)?;
        Ok((modpkg::Manifest::parse(&bytes)?, bytes))
    }
}

/// A readable id from author and name: `author.my-mod`.
pub fn suggest_id(author: &str, name: &str) -> String {
    let slug = |s: &str| {
        let mut out = String::new();
        for c in s.to_lowercase().chars() {
            if c.is_ascii_alphanumeric() {
                out.push(c);
            } else if !out.ends_with('-') && !out.is_empty() {
                out.push('-');
            }
        }
        out.trim_end_matches('-').to_string()
    };
    let (a, n) = (slug(author), slug(name));
    let id = match (a.is_empty(), n.is_empty()) {
        (false, false) => format!("{a}.{n}"),
        (true, false) => n,
        (false, true) => format!("{a}.mod"),
        _ => "my.mod".into(),
    };
    id.chars().take(64).collect::<String>().trim_end_matches(['-', '.']).to_string()
}

#[derive(Serialize, Deserialize, Default)]
struct State {
    rom: Option<PathBuf>,
}

/// One workspace folder under the modding root.
pub struct Workspace {
    pub dir: PathBuf,
    pub draft: Draft,
}
impl Workspace {
    pub fn open(dir: &Path) -> Result<Self> {
        let draft: Draft = serde_json::from_slice(&std::fs::read(dir.join("manifest.json"))?)
            .map_err(|e| format!("{}: manifest.json: {e}", dir.display()))?;
        Ok(Self { dir: dir.into(), draft })
    }
    /// The ROM this workspace was made from, if it was recorded.
    pub fn rom(&self) -> Option<PathBuf> {
        let s: State = serde_json::from_slice(&std::fs::read(self.dir.join(STATE)).ok()?).ok()?;
        s.rom
    }
}

/// All workspaces under `root`, by name.
pub fn list(root: &Path) -> Vec<Workspace> {
    let mut out: Vec<_> = std::fs::read_dir(root)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| Workspace::open(&e.path()).ok())
        .collect();
    out.sort_by(|a, b| a.draft.name.to_lowercase().cmp(&b.draft.name.to_lowercase()));
    out
}

fn write(path: &Path, bytes: &[u8]) -> Result<()> {
    std::fs::create_dir_all(path.parent().unwrap())?;
    std::fs::write(path, bytes)?;
    Ok(())
}

/// Name of entry `i` inside an extracted archive: its own name, or `0007.nsbmd` style when unnamed.
fn entry_name(e: &dsfiles::NarcEntry, i: usize) -> String {
    match &e.name {
        Some(n) if modpkg::safe_path(n) => n.clone(),
        _ => match dsfiles::format_by_magic(e.data) {
            Some(f) => format!("{i:04}.{}", f.ext),
            None => format!("{i:04}.bin"),
        },
    }
}
/// Archive bytes as a NARC (`.carc` decompressed), or None when `path` is not a readable archive.
fn narc_of(path: &str, bytes: &[u8]) -> Option<Vec<u8>> {
    match dsfiles::ext_of(path).as_str() {
        "carc" => dsfiles::lz77_decompress(bytes, modpkg::MAX_UNPACKED as usize).ok().filter(|b| b.starts_with(b"NARC")),
        "narc" => Some(bytes.to_vec()),
        _ => None,
    }
    .filter(|n| dsfiles::parse_narc(n).is_ok())
}

/// Make a workspace `<root>/<draft.id>/` from the player's ROM.
pub fn create(root: &Path, draft: &Draft, rom_path: &Path, rom: &[u8], extract_archives: bool) -> Result<PathBuf> {
    // the same rules as a real package (one placeholder file, since files are only known when packing)
    let placeholder = BTreeMap::from([("x.bmg".to_string(), modpkg::Entry::Add(modpkg::Add { add: "x.bmg".into() }))]);
    draft.manifest(placeholder)?;
    let dir = root.join(&draft.id);
    if dir.exists() {
        return Err(format!("A mod project named {} already exists", draft.id).into());
    }
    let fs = crate::rom::NitroFs::parse(rom)?;
    std::fs::create_dir_all(root)?;
    let staging = tempfile::tempdir_in(root)?;
    let romfs = staging.path().join("romfs");
    let mut archives = 0;
    for path in fs.paths() {
        let bytes = fs.file(path).unwrap();
        if extract_archives {
            if let Some(narc) = narc_of(path, bytes) {
                let folder = romfs.join(format!("{path}{UNPACKED}"));
                for (i, e) in dsfiles::parse_narc(&narc).unwrap().iter().enumerate() {
                    write(&folder.join(entry_name(e, i)), e.data)?;
                }
                std::fs::create_dir_all(&folder)?;
                archives += 1;
                continue;
            }
        }
        write(&romfs.join(path), bytes)?;
    }
    std::fs::create_dir_all(staging.path().join(EXTRAS))?;
    write(&staging.path().join("manifest.json"), &serde_json::to_vec_pretty(draft)?)?;
    write(&staging.path().join("README.md"), readme(draft, archives).as_bytes())?;
    write(&staging.path().join(STATE), &serde_json::to_vec_pretty(&State { rom: Some(rom_path.into()) })?)?;
    std::fs::rename(staging.keep(), &dir)?;
    Ok(dir)
}

fn readme(d: &Draft, archives: usize) -> String {
    format!(
        "# {name}\n\n\
This folder is your MKDSrust mod project. It holds a copy of the files from **your own** ROM, so it is for you\n\
only: share the packed .zip, never this folder.\n\n\
## Edit\n\n\
Change anything under `romfs/` with your favourite DS tools, for example:\n\n\
- text (`.bmg`): a BMG editor such as Kuriimu2 or the MKDS Course Modifier's message editor\n\
- 2D graphics (`.nclr` `.ncgr` `.nscr` `.ncer` `.nanr`): NitroPaint or Tinke\n\
- models and textures (`.nsbmd` `.nsbtx` and animations): Blender with a NSBMD exporter, or MKDS Course Modifier\n\
- courses (`.nkm` `.kcl`): MKDS Course Modifier / KMP-style course editors\n\
- sound (`.sdat` `.sseq` `.swar` `.strm`): VGMTrans, Nitro Studio 2 or SDAT tools\n\n\
{archives_note}\
You can also add **new** files (allowed DS formats only) anywhere under `romfs/`; they are packed as `add` entries.\n\
Deleting a file does nothing (the original stays).\n\n\
Put an optional `README.md` and up to 8 preview images (`.png` `.jpg` `.webp`) for the mod page in `{extras}/`.\n\n\
## Test and pack\n\n\
In the launcher (Mods page, *Your mod projects*): **Test in game** runs your changes right away; **Pack** writes a\n\
ready-to-upload zip with only the differences (patches against the player's own ROM, never your copy of the game\n\
files), checked with the same rules as mkdsrust.com. From a terminal: `mkds_builder mod pack \"<this folder>\"`.\n\n\
Edit `manifest.json` to change the name, version (`{version}` now), type, author, license or description.\n\
Mod id: `{id}` (type `{kind}`).\n",
        name = d.name,
        id = d.id,
        kind = d.kind,
        version = d.version,
        extras = EXTRAS,
        archives_note = if archives > 0 {
            format!(
                "{archives} archives (`.carc`/`.narc`) were extracted into `<name>{UNPACKED}/` folders. Edit the files inside;\n\
                 they are packed back into the archive automatically. Keep their names (new files inside an archive are ignored).\n\n"
            )
        } else {
            "Archives (`.carc` = compressed `.narc`) are kept as single files; open them with an archive-aware tool.\n\n".into()
        },
    )
}

/// What `pack` / `stage` found.
#[derive(Default, Debug)]
pub struct Report {
    pub patched: Vec<(String, u64, u64)>,
    pub added: Vec<(String, u64)>,
    pub extras: Vec<String>,
    pub skipped: Vec<String>,
    pub warnings: Vec<String>,
    pub zip: Option<PathBuf>,
}
impl Report {
    pub fn summary(&self) -> String {
        let mut s = String::new();
        if let Some(z) = &self.zip {
            s += &format!("Packed {}\n", z.display());
        }
        s += &format!(
            "{} changed file(s) as patches, {} new file(s), {} package extra(s). Screening: passed.\n",
            self.patched.len(),
            self.added.len(),
            self.extras.len()
        );
        for (p, size, patch) in &self.patched {
            s += &format!("  patch {p} ({patch} bytes for a {size}-byte file)\n");
        }
        for (p, size) in &self.added {
            s += &format!("  add   {p} ({size} bytes)\n");
        }
        for p in &self.extras {
            s += &format!("  extra {p}\n");
        }
        for w in self.skipped.iter().chain(&self.warnings) {
            s += &format!("  note: {w}\n");
        }
        s
    }
}

/// Every file under `dir` as a relative `/` path.
fn walk(dir: &Path) -> Vec<(String, PathBuf)> {
    fn go(base: &Path, d: &Path, out: &mut Vec<(String, PathBuf)>) {
        for e in std::fs::read_dir(d).into_iter().flatten().flatten() {
            let p = e.path();
            let Ok(t) = e.file_type() else { continue };
            if t.is_symlink() {
                continue;
            } else if t.is_dir() {
                go(base, &p, out);
            } else if let Ok(rel) = p.strip_prefix(base) {
                out.push((rel.to_string_lossy().replace('\\', "/"), p));
            }
        }
    }
    let mut out = Vec::new();
    go(dir, dir, &mut out);
    out.sort();
    out
}

/// The workspace's current bytes for ROM file `path`, or None when unchanged / missing.
fn current(romfs: &Path, path: &str, original: &[u8], report: &mut Report) -> Result<Option<Vec<u8>>> {
    let folder = romfs.join(format!("{path}{UNPACKED}"));
    if folder.is_dir() {
        let narc = narc_of(path, original).ok_or_else(|| format!("{path}{UNPACKED}: the original is not an archive"))?;
        let entries = dsfiles::parse_narc(&narc)?;
        let mut files = Vec::with_capacity(entries.len());
        let mut changed = false;
        let mut known = BTreeSet::new();
        for (i, e) in entries.iter().enumerate() {
            let name = entry_name(e, i);
            known.insert(name.clone());
            match std::fs::read(folder.join(&name)) {
                Ok(b) => {
                    changed |= b != e.data;
                    files.push(b);
                }
                Err(_) => {
                    report.skipped.push(format!("{path}{UNPACKED}/{name} is missing; the original is kept"));
                    files.push(e.data.to_vec());
                }
            }
        }
        for (rel, _) in walk(&folder) {
            if !known.contains(&rel) {
                report.skipped.push(format!("{path}{UNPACKED}/{rel}: new files inside an archive are not supported (ignored)"));
            }
        }
        if romfs.join(path).is_file() {
            report.skipped.push(format!("{path}: both the archive and its {UNPACKED} folder exist; the folder is used"));
        }
        if !changed {
            return Ok(None);
        }
        // splice into the original archive and reuse the original compression around the edit: small patches
        let rebuilt = dsfiles::update_narc(&narc, &files)?;
        return Ok(Some(if dsfiles::ext_of(path) == "carc" { dsfiles::lz77_recompress(original, &rebuilt) } else { rebuilt }));
    }
    match std::fs::read(romfs.join(path)) {
        Ok(b) if b != original => Ok(Some(b)),
        Ok(_) => Ok(None),
        Err(_) => Ok(None),
    }
}

/// Package contents (manifest + payloads) for a workspace, as `pack` would zip them. `id` overrides the mod id
/// (used by "Test in game"). Screened against `retail` before returning.
pub fn stage(ws: &Workspace, rom: &[u8], retail: &modpkg::RetailIndex, id: Option<&str>) -> Result<(modpkg::Package, Report)> {
    let fs = crate::rom::NitroFs::parse(rom)?;
    let romfs = ws.dir.join("romfs");
    if !romfs.is_dir() {
        return Err(format!("{} has no romfs folder", ws.dir.display()).into());
    }
    let mut report = Report::default();
    let mut files = BTreeMap::new();
    let mut contents = BTreeMap::new();
    let mut paths: Vec<&str> = fs.paths().collect();
    paths.sort();
    let lower: BTreeSet<String> = paths.iter().map(|p| p.to_ascii_lowercase()).collect();
    for path in &paths {
        let original = fs.file(path).unwrap();
        if let Some(new) = current(&romfs, path, original, &mut report)? {
            let mut patch = Vec::new();
            modpkg::qbsdiff::Bsdiff::new(original, &new).compare(&mut patch)?;
            let name = format!("patches/{path}.bsdiff");
            report.patched.push((path.to_string(), new.len() as u64, patch.len() as u64));
            files.insert(path.to_string(), modpkg::Entry::Patch(modpkg::Patch { patch: name.clone() }));
            contents.insert(name, patch);
        }
    }
    // new files: anything under romfs that is not a ROM path and not inside an extracted archive
    for (rel, full) in walk(&romfs) {
        if lower.contains(&rel.to_ascii_lowercase()) {
            continue;
        }
        if let Some(i) = rel.find(&format!("{UNPACKED}/")) {
            if lower.contains(&rel[..i].to_ascii_lowercase()) {
                continue;
            }
        }
        let bytes = std::fs::read(&full)?;
        let mut budget = modpkg::MAX_UNPACKED as usize;
        let problems = dsfiles::check_game_file(&bytes, &rel, &mut budget, 0);
        if !problems.is_empty() || !modpkg::safe_path(&rel) {
            report.skipped.push(format!("{rel}: not packed ({})", problems.join("; ")));
            continue;
        }
        let name = format!("files/{rel}");
        report.added.push((rel.clone(), bytes.len() as u64));
        files.insert(rel, modpkg::Entry::Add(modpkg::Add { add: name.clone() }));
        contents.insert(name, bytes);
    }
    if files.is_empty() {
        return Err("Nothing to pack yet: no file under romfs/ differs from your ROM.".into());
    }
    for (rel, full) in walk(&ws.dir.join(EXTRAS)) {
        if dsfiles::is_readme(&rel) || dsfiles::is_preview_path(&rel) {
            let name = if dsfiles::is_readme(&rel) { rel.clone() } else { format!("previews/{}", rel.rsplit('/').next().unwrap()) };
            contents.insert(name.clone(), std::fs::read(full)?);
            report.extras.push(name);
        } else {
            report.skipped.push(format!("{EXTRAS}/{rel}: only README.md/README.txt and preview images go in {EXTRAS}/"));
        }
    }
    let mut draft = ws.draft.clone();
    if let Some(id) = id {
        draft.id = id.into();
    }
    let (_, manifest) = draft.manifest(files)?;
    contents.insert("manifest.json".into(), manifest);
    let pkg = modpkg::Package::validate(contents, retail).map_err(|e| format!("Screening failed:\n{e}"))?;
    report.warnings.extend(pkg.warnings.iter().cloned());
    Ok((pkg, report))
}

/// Pack a workspace into `out` (default `<workspace>/../packages/<id>-<version>.zip`), screen the written zip the
/// way an upload is screened, and report what went in.
pub fn pack(ws: &Workspace, rom: &[u8], out: Option<&Path>) -> Result<Report> {
    let retail = modpkg::RetailIndex::from_rom(rom)?;
    let (pkg, mut report) = stage(ws, rom, &retail, None)?;
    let out = out.map(PathBuf::from).unwrap_or_else(|| {
        ws.dir.parent().unwrap_or(&ws.dir).join("packages").join(format!("{}-{}.zip", ws.draft.id, ws.draft.version))
    });
    std::fs::create_dir_all(out.parent().unwrap())?;
    let mut staged = tempfile::NamedTempFile::new_in(out.parent().unwrap())?;
    {
        let mut zip = zip::ZipWriter::new(staged.as_file_mut());
        let opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        for (name, data) in &pkg.contents {
            zip.start_file(name.as_str(), opts)?;
            zip.write_all(data)?;
        }
        zip.finish()?;
    }
    if staged.as_file().metadata()?.len() > modpkg::MAX_ZIP {
        return Err("The package is larger than 100 MB".into());
    }
    // the zip itself, read back exactly as the launcher and site read uploads
    modpkg::Package::read(staged.path(), &retail).map_err(|e| format!("The written zip failed screening:\n{e}"))?;
    staged.persist(&out)?;
    report.zip = Some(out);
    Ok(report)
}

/// The id a workspace is tested under: `<id>.dev`, so it never replaces an installed release of the same mod.
pub fn dev_id(id: &str) -> String {
    let base: String = id.chars().take(60).collect();
    format!("{}.dev", base.trim_end_matches(['.', '-']))
}

/// Install a workspace into `game` as `<id>.dev`, enabled and last in the load order (highest priority).
pub fn test_install(ws: &Workspace, rom: &[u8], retail: &modpkg::RetailIndex, game: &Path) -> Result<(String, Report)> {
    let id = dev_id(&ws.draft.id);
    let (pkg, report) = stage(ws, rom, retail, Some(&id))?;
    remove_installed(game, &id)?;
    pkg.install(game)?;
    let mut order = modpkg::load_order(game);
    order.retain(|x| x != &id);
    order.push(id.clone());
    let mut enabled: BTreeSet<String> = modpkg::enabled(game).into_iter().collect();
    enabled.insert(id.clone());
    modpkg::save_load_order(game, &order, &enabled)?;
    Ok((id, report))
}

/// Screen a mod zip against the player's ROM and install it into `game` (the launcher's Install and the
/// `mkds_builder mod install` command). A newer copy of an installed mod replaces it in place, keeping its load-order
/// slot (the old copy is restored if the new one fails); a new mod is enabled and goes last (highest priority).
/// Returns a one-line summary.
pub fn install_zip(game: &Path, rom: &[u8], archive: &Path) -> Result<String> {
    if !game.join("manifest.json").is_file() {
        return Err("Build your game first (Setup), then install mods.".into());
    }
    let retail = modpkg::RetailIndex::from_rom(rom)?;
    let pkg = modpkg::Package::read(archive, &retail)?;
    let id = pkg.manifest.id.clone();
    let dest = game.join("mods").join(&id);
    let fresh = !dest.exists();
    let backup = game.join("mods").join(format!(".replaced-{id}"));
    if !fresh {
        let meta = std::fs::symlink_metadata(&dest)?;
        if !meta.is_dir() || meta.file_type().is_symlink() {
            return Err("Unsafe mod folder; remove it manually".into());
        }
        let _ = std::fs::remove_dir_all(&backup);
        std::fs::rename(&dest, &backup)?;
    }
    if let Err(e) = pkg.install(game) {
        if !fresh {
            let _ = std::fs::rename(&backup, &dest);
        }
        return Err(e);
    }
    let _ = std::fs::remove_dir_all(&backup);
    let mut order = modpkg::load_order(game);
    let mut on: BTreeSet<String> = modpkg::enabled(game).into_iter().collect();
    if fresh {
        order.retain(|x| x != &id);
        order.push(id.clone());
        on.insert(id.clone());
    }
    modpkg::save_load_order(game, &order, &on)?;
    let mut msg = if fresh {
        format!("Installed {} {} and enabled it (last in the load order).", pkg.manifest.name, pkg.manifest.version)
    } else {
        format!("Updated {} to {} in place.", pkg.manifest.name, pkg.manifest.version)
    };
    if !pkg.warnings.is_empty() {
        msg += &format!(" Notes: {}", pkg.warnings.join("; "));
    }
    Ok(msg)
}

/// Remove an installed mod folder and its load-order entries (no-op when absent).
pub fn remove_installed(game: &Path, id: &str) -> Result<()> {
    if !modpkg::valid_id(id) {
        return Err("Invalid mod id".into());
    }
    let path = game.join("mods").join(id);
    match std::fs::symlink_metadata(&path) {
        Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => std::fs::remove_dir_all(&path)?,
        Ok(_) => return Err("Unsafe mod folder; remove it manually".into()),
        Err(_) => {}
    }
    let order: Vec<String> = modpkg::load_order(game).into_iter().filter(|x| x != id).collect();
    let enabled: BTreeSet<String> = modpkg::enabled(game).into_iter().filter(|x| x != id).collect();
    modpkg::save_load_order(game, &order, &enabled)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tiny synthetic NDS image with a NitroFS holding the given files (root directory only).
    pub(crate) fn rom(files: &[(&str, Vec<u8>)]) -> Vec<u8> {
        let mut fnt = Vec::new();
        fnt.extend_from_slice(&8u32.to_le_bytes());
        fnt.extend_from_slice(&0u16.to_le_bytes());
        fnt.extend_from_slice(&1u16.to_le_bytes());
        for (name, _) in files {
            fnt.push(name.len() as u8);
            fnt.extend_from_slice(name.as_bytes());
        }
        fnt.push(0);
        let mut rom = vec![0u8; 0x200];
        let fnt_off = rom.len();
        rom.extend_from_slice(&fnt);
        while rom.len() % 0x200 != 0 {
            rom.push(0);
        }
        let fat_off = rom.len();
        rom.extend(std::iter::repeat(0).take(files.len() * 8));
        let mut fat = Vec::new();
        for (_, data) in files {
            while rom.len() % 0x200 != 0 {
                rom.push(0xff);
            }
            fat.push((rom.len() as u32, (rom.len() + data.len()) as u32));
            rom.extend_from_slice(data);
        }
        for (i, (a, b)) in fat.iter().enumerate() {
            rom[fat_off + i * 8..fat_off + i * 8 + 4].copy_from_slice(&a.to_le_bytes());
            rom[fat_off + i * 8 + 4..fat_off + i * 8 + 8].copy_from_slice(&b.to_le_bytes());
        }
        for (off, v) in [(0x40, fnt_off), (0x44, fnt.len()), (0x48, fat_off), (0x4c, files.len() * 8)] {
            rom[off..off + 4].copy_from_slice(&(v as u32).to_le_bytes());
        }
        rom
    }
    fn narc(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut btnf = Vec::new();
        btnf.extend_from_slice(&8u32.to_le_bytes());
        btnf.extend_from_slice(&0u16.to_le_bytes());
        btnf.extend_from_slice(&1u16.to_le_bytes());
        for (name, _) in files {
            btnf.push(name.len() as u8);
            btnf.extend_from_slice(name.as_bytes());
        }
        btnf.push(0);
        while btnf.len() % 4 != 0 {
            btnf.push(0xff);
        }
        let mut out = b"NARC\xfe\xff\x00\x01\0\0\0\0\x10\x00\x03\x00".to_vec();
        out.extend_from_slice(b"BTAF");
        out.extend_from_slice(&((12 + files.len() * 8) as u32).to_le_bytes());
        out.extend_from_slice(&(files.len() as u16).to_le_bytes());
        out.extend_from_slice(&[0, 0]);
        out.extend(std::iter::repeat(0).take(files.len() * 8));
        out.extend_from_slice(b"BTNF");
        out.extend_from_slice(&((btnf.len() + 8) as u32).to_le_bytes());
        out.extend_from_slice(&btnf);
        out.extend_from_slice(b"GMIF");
        out.extend_from_slice(&8u32.to_le_bytes());
        dsfiles::rebuild_narc(&out, &files.iter().map(|(_, d)| d.to_vec()).collect::<Vec<_>>()).unwrap()
    }
    fn text(n: usize, seed: u8) -> Vec<u8> {
        let mut v = b"MESGbmg1".to_vec();
        v.extend((0..n).map(|i| (i as u8).wrapping_mul(seed).wrapping_add(i as u8 >> 3)));
        v
    }

    #[test]
    fn ids_are_suggested_from_author_and_name() {
        assert_eq!(suggest_id("Cam S", "My First Mod!"), "cam-s.my-first-mod");
        assert_eq!(suggest_id("", "  "), "my.mod");
        assert!(modpkg::valid_id(&suggest_id("x", &"long name ".repeat(20))));
        assert_eq!(dev_id("a.b"), "a.b.dev");
        assert!(modpkg::valid_id(&dev_id(&"a".repeat(64))));
    }

    #[test]
    fn create_edit_pack_install_round_trip() {
        let inner = narc(&[("msg.bmg", &text(3000, 3)), ("pal.nclr", b"RLCN0123")]);
        let image = rom(&[("a.bmg", text(9000, 7)), ("b.carc", dsfiles::lz77_compress(&inner)), ("c.nclr", b"RLCN4567".to_vec())]);
        let root = tempfile::tempdir().unwrap();
        let draft = Draft::new("me.test", "Test", "gameplay", "me");
        let dir = create(root.path(), &draft, Path::new("rom.nds"), &image, true).unwrap();
        assert!(create(root.path(), &draft, Path::new("rom.nds"), &image, true).is_err());
        let ws = Workspace::open(&dir).unwrap();
        assert_eq!(ws.rom().unwrap(), Path::new("rom.nds"));
        assert!(dir.join("romfs/b.carc.unpacked/msg.bmg").is_file() && !dir.join("romfs/b.carc").exists());
        assert!(stage(&ws, &image, &modpkg::RetailIndex::from_rom(&image).unwrap(), None).err().unwrap().to_string().contains("Nothing to pack"));

        // edit one text file, one file inside the compressed archive, add a new file and extras
        let mut a = text(9000, 7);
        a[100] ^= 0x55;
        std::fs::write(dir.join("romfs/a.bmg"), &a).unwrap();
        let mut m = text(3000, 3);
        m[50] = b'!';
        std::fs::write(dir.join("romfs/b.carc.unpacked/msg.bmg"), &m).unwrap();
        std::fs::create_dir_all(dir.join("romfs/new")).unwrap();
        std::fs::write(dir.join("romfs/new/extra.bmg"), text(10, 5)).unwrap();
        std::fs::write(dir.join("romfs/notes.txt"), b"scratch").unwrap();
        std::fs::write(dir.join("package/README.md"), b"# Test mod").unwrap();
        std::fs::write(dir.join("package/shot.png"), b"\x89PNG\r\n\x1a\nxx").unwrap();

        let report = pack(&ws, &image, None).unwrap();
        let zip = report.zip.clone().unwrap();
        assert_eq!(zip, root.path().join("packages/me.test-1.0.0.zip"));
        let names: Vec<_> = report.patched.iter().map(|p| p.0.as_str()).collect();
        assert_eq!(names, ["a.bmg", "b.carc"]);
        assert_eq!(report.added, [("new/extra.bmg".to_string(), 18)]);
        assert!(report.skipped.iter().any(|s| s.starts_with("notes.txt")));
        assert_eq!(report.extras, ["README.md", "previews/shot.png"]);
        assert!(report.summary().contains("2 changed file(s)"));

        // the zip installs and the game-side overlay reproduces the edits from the original ROM
        let retail = modpkg::RetailIndex::from_rom(&image).unwrap();
        let pkg = modpkg::Package::read(&zip, &retail).unwrap();
        assert_eq!(pkg.manifest.kind, "gameplay");
        let patch = &pkg.contents["patches/b.carc.bsdiff"];
        let mut out = Vec::new();
        modpkg::qbsdiff::Bspatch::new(patch).unwrap().apply(crate::rom::NitroFs::parse(&image).unwrap().file("b.carc").unwrap(), &mut out).unwrap();
        let narc = dsfiles::lz77_decompress(&out, 1 << 20).unwrap();
        let entries = dsfiles::parse_narc(&narc).unwrap();
        assert_eq!(entries[0].data, &m[..]);
        assert_eq!(entries[1].data, b"RLCN0123");

        // test install goes last, enabled, under the dev id, and is removable
        let game = tempfile::tempdir().unwrap();
        let (id, _) = test_install(&ws, &image, &retail, game.path()).unwrap();
        assert_eq!(id, "me.test.dev");
        assert_eq!(modpkg::enabled(game.path()), ["me.test.dev"]);
        test_install(&ws, &image, &retail, game.path()).unwrap();
        remove_installed(game.path(), &id).unwrap();
        assert!(modpkg::enabled(game.path()).is_empty() && !game.path().join("mods/me.test.dev").exists());

        // installing the packed zip: needs a built game, enables it last, updates in place keeping its slot
        assert!(install_zip(game.path(), &image, &zip).is_err());
        std::fs::write(game.path().join("manifest.json"), b"{}").unwrap();
        assert!(install_zip(game.path(), &image, &zip).unwrap().starts_with("Installed Test 1.0.0"));
        test_install(&ws, &image, &retail, game.path()).unwrap();
        assert_eq!(modpkg::load_order(game.path()), ["me.test", "me.test.dev"]);
        assert!(install_zip(game.path(), &image, &zip).unwrap().starts_with("Updated Test"));
        assert_eq!(modpkg::load_order(game.path()), ["me.test", "me.test.dev"]);
        assert!(game.path().join("mods/me.test/patches/a.bmg.bsdiff").is_file());
    }

    #[test]
    fn unextracted_workspaces_patch_archives_directly() {
        let image = rom(&[("a.bmg", text(500, 7))]);
        let root = tempfile::tempdir().unwrap();
        let dir = create(root.path(), &Draft::new("me.raw", "Raw", "other", "me"), Path::new("r"), &image, false).unwrap();
        assert!(dir.join("README.md").is_file());
        let mut a = text(500, 7);
        a[9] = 0;
        std::fs::write(dir.join("romfs/a.bmg"), &a).unwrap();
        let ws = Workspace::open(&dir).unwrap();
        let report = pack(&ws, &image, Some(&root.path().join("out.zip"))).unwrap();
        assert_eq!(report.patched.len(), 1);
        assert!(root.path().join("out.zip").is_file());
    }
}
