//! Rips the user's ROM into loose files (`assets/`, format 1; spec: https://github.com/CameronStorer/MKDSrust-Docs).
//! Every FAT entry is written exactly once: named NitroFS files under `files/`, overlays and system
//! binaries under `system/`. The manifest records sizes and SHA-256 so the runtime can rebuild an
//! identical file system (same FAT ids) from the folder and detect edited or missing files.
use crate::rom::{BuildError, err, hex, list_files_with_ids};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const FORMAT_VERSION: u32 = 1;

fn u32le(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
fn u16le(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes(b[o..o + 2].try_into().unwrap())
}

/// `rom[off..off+len]`, with header-supplied ranges checked against the image.
fn range<'a>(rom: &'a [u8], off: u32, len: u32, label: &str) -> Result<&'a [u8], BuildError> {
    let (off, len) = (off as usize, len as usize);
    match off.checked_add(len) {
        Some(end) if end <= rom.len() && (len == 0 || off >= 0x200) => Ok(&rom[off..end]),
        _ => err(format!("Invalid {label} range in NDS header.")),
    }
}

/// A NitroFS name must stay inside `assets/files/` on every platform.
fn safe_name(name: &str) -> Result<(), BuildError> {
    let bad = name.is_empty()
        || name.starts_with('/')
        || name.split('/').any(|part| part.is_empty() || part == "." || part == "..")
        || name.chars().any(|c| c == '\\' || c == ':' || c.is_control());
    if bad {
        return err(format!("Refusing unsafe file name in ROM: {name:?}"));
    }
    Ok(())
}

/// Overlay ids and their FAT file ids from a y9/y7 table (32-byte records).
fn overlay_files(table: &[u8], label: &str) -> Result<Vec<(u32, usize)>, BuildError> {
    if table.len() % 32 != 0 {
        return err(format!("{label} overlay table is not a whole number of records."));
    }
    Ok(table
        .chunks_exact(32)
        .map(|r| (u32le(r, 0), u32le(r, 0x18) as usize))
        .collect())
}

fn banner_len(rom: &[u8], off: usize) -> usize {
    match rom.get(off..off + 2).map(|v| u16le(v, 0)) {
        Some(2) => 0x940,
        Some(3) => 0xA40,
        Some(0x103) => 0x23C0,
        _ => 0x840,
    }
}

struct Entry {
    id: usize,
    kind: &'static str,
    name: Option<String>,
    overlay: Option<u32>,
    path: String,
    data_range: (usize, usize),
}

/// What a rip wrote: the manifest (also saved as `assets/manifest.json`).
#[derive(Debug)]
pub struct Ripped {
    pub manifest: Value,
    pub file_count: usize,
    pub bytes: u64,
}

/// Rips `rom` into `<out>/assets/`. `out` must be empty or absent; the files are staged next to it
/// and moved into place only when everything succeeded, so a failure never leaves a partial rip.
pub fn rip(rom: &[u8], rom_sha256: &str, out: &Path, log: &mut dyn FnMut(String)) -> Result<Ripped, BuildError> {
    if rom.len() < 0x200 {
        return err("Truncated NDS header.");
    }
    if out.exists() && std::fs::read_dir(out)?.next().is_some() {
        return err(format!("Output must be empty; choose a new directory: {}", out.display()));
    }
    let h = &rom[..0x200];
    let fnt = range(rom, u32le(h, 0x40), u32le(h, 0x44), "FNT")?;
    let fat = range(rom, u32le(h, 0x48), u32le(h, 0x4C), "FAT")?;
    if fat.len() % 8 != 0 || fnt.len() < 8 {
        return err("Malformed NDS filesystem tables.");
    }
    let fat_entries = fat.len() / 8;
    let arm9 = range(rom, u32le(h, 0x20), u32le(h, 0x2C), "ARM9")?;
    let arm7 = range(rom, u32le(h, 0x30), u32le(h, 0x3C), "ARM7")?;
    let y9 = range(rom, u32le(h, 0x50), u32le(h, 0x54), "ARM9 overlay table")?;
    let y7 = range(rom, u32le(h, 0x58), u32le(h, 0x5C), "ARM7 overlay table")?;
    let banner_off = u32le(h, 0x68) as usize;
    let banner = if banner_off == 0 {
        &[][..]
    } else {
        range(rom, banner_off as u32, banner_len(rom, banner_off) as u32, "banner")?
    };

    // one entry per FAT id: named files first, then overlays, then anything left unnamed
    let mut by_id: BTreeMap<usize, Entry> = BTreeMap::new();
    let fat_range = |id: usize| -> Result<(usize, usize), BuildError> {
        let (s, e) = (u32le(fat, id * 8) as usize, u32le(fat, id * 8 + 4) as usize);
        if s > e || e > rom.len() || (s < 0x200 && e > s) {
            return err(format!("Invalid FAT range for file id {id}."));
        }
        Ok((s, e))
    };
    for (name, id) in list_files_with_ids(fnt, fat_entries)? {
        safe_name(&name)?;
        let path = format!("files/{name}");
        by_id.insert(id, Entry { id, kind: "file", name: Some(name), overlay: None, path, data_range: fat_range(id)? });
    }
    for (kind, prefix, table) in [("arm9_overlay", "arm9", y9), ("arm7_overlay", "arm7", y7)] {
        for (ov, id) in overlay_files(table, prefix)? {
            if id >= fat_entries {
                return err(format!("{prefix} overlay {ov} points past the FAT."));
            }
            if by_id.contains_key(&id) {
                return err(format!("{prefix} overlay {ov} shares FAT id {id} with another file."));
            }
            let path = format!("system/overlays/{prefix}_ov_{ov:03}.bin");
            by_id.insert(id, Entry { id, kind, name: None, overlay: Some(ov), path, data_range: fat_range(id)? });
        }
    }
    for id in 0..fat_entries {
        if !by_id.contains_key(&id) {
            let path = format!("system/unnamed/fat_{id:05}.bin");
            by_id.insert(id, Entry { id, kind: "unnamed", name: None, overlay: None, path, data_range: fat_range(id)? });
        }
    }
    let system: Vec<(&str, &[u8])> = vec![
        ("header.bin", h),
        ("arm9.bin", arm9),
        ("arm7.bin", arm7),
        ("arm9_overlay_table.bin", y9),
        ("arm7_overlay_table.bin", y7),
        ("fnt.bin", fnt),
        ("fat.bin", fat),
        ("banner.bin", banner),
    ];

    let parent = match out.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    };
    std::fs::create_dir_all(&parent)?;
    let stage = tempfile::Builder::new().prefix("mkds-rip-").tempdir_in(&parent)?;
    let root = stage.path().join("out");
    let assets = root.join("assets");
    let write = |rel: &str, data: &[u8]| -> Result<String, BuildError> {
        let p = assets.join(rel);
        std::fs::create_dir_all(p.parent().unwrap())?;
        std::fs::write(&p, data)?;
        Ok(hex(&Sha256::digest(data)))
    };

    let mut sum = Sha256::new();
    let mut files = Vec::with_capacity(by_id.len());
    let mut bytes = 0u64;
    for (n, e) in by_id.values().enumerate() {
        let data = &rom[e.data_range.0..e.data_range.1];
        let sha = write(&e.path, data)?;
        sum.update(format!("{} {sha}\n", e.id).as_bytes());
        bytes += data.len() as u64;
        let mut j = json!({"id": e.id, "kind": e.kind, "path": e.path, "size": data.len(), "sha256": sha});
        if let Some(name) = &e.name {
            j["name"] = json!(name);
        }
        if let Some(ov) = e.overlay {
            j["overlay"] = json!(ov);
        }
        files.push(j);
        if n % 100 == 99 {
            log(format!("Ripped {} of {} files", n + 1, by_id.len()));
        }
    }
    let mut system_json = serde_json::Map::new();
    let mut names: Vec<&(&str, &[u8])> = system.iter().collect();
    names.sort_by_key(|(n, _)| *n);
    for (name, data) in names {
        let sha = write(&format!("system/{name}"), data)?;
        sum.update(format!("{name} {sha}\n").as_bytes());
        bytes += data.len() as u64;
        system_json.insert((*name).into(), json!({"size": data.len(), "sha256": sha}));
    }
    std::fs::write(
        assets.join("README.txt"),
        "These files were generated on your machine from your own ROM by mkds_builder --rip.\n\
         They are the game's copyrighted data: keep them for your own use and do not distribute them.\n\
         Layout and manifest format: https://github.com/CameronStorer/MKDSrust-Docs in the project.\n",
    )?;
    let manifest = json!({
        "format": "mkds-assets",
        "format_version": FORMAT_VERSION,
        "rom": {
            "sha1": crate::rom::sha1(rom),
            "sha256": rom_sha256,
            "game_code": String::from_utf8_lossy(&h[12..16]),
            "revision": h[0x1E],
            "size": rom.len(),
        },
        "fat_entries": fat_entries,
        "fnt_directories": u16le(fnt, 6),
        "system": system_json,
        "files": files,
        "assets_sha256": hex(&sum.finalize()),
    });
    std::fs::write(
        assets.join("manifest.json"),
        serde_json::to_string_pretty(&manifest).unwrap() + "\n",
    )?;
    if out.exists() {
        std::fs::remove_dir(out)?; // only an empty directory is ever removed
    }
    std::fs::rename(&root, out)?;
    log(format!("Ripped {} files ({:.1} MB) into {}", by_id.len(), bytes as f64 / 1e6, out.join("assets").display()));
    Ok(Ripped { manifest, file_count: by_id.len(), bytes })
}

/// Re-hashes every file listed in `<assets>/manifest.json` and returns the recomputed
/// `assets_sha256`; any missing or edited file is an error naming it.
pub fn verify(assets: &Path) -> Result<String, BuildError> {
    let m: Value = serde_json::from_slice(&std::fs::read(assets.join("manifest.json")).map_err(|e| {
        BuildError(format!("Missing assets manifest in {}: {e}", assets.display()))
    })?)
    .map_err(|e| BuildError(format!("Unreadable assets manifest: {e}")))?;
    let mut sum = Sha256::new();
    let check = |rel: &str, want: &Value| -> Result<String, BuildError> {
        if safe_name(rel).is_err() {
            return err(format!("Unsafe path in assets manifest: {rel}"));
        }
        let data = std::fs::read(assets.join(rel))
            .map_err(|_| BuildError(format!("Asset missing: {rel}. Rebuild from your ROM.")))?;
        let sha = hex(&Sha256::digest(&data));
        if want.as_str() != Some(sha.as_str()) {
            return err(format!("Asset changed: {rel}. Rebuild from your ROM."));
        }
        Ok(sha)
    };
    for f in m["files"].as_array().ok_or_else(|| BuildError("Assets manifest lacks files.".into()))? {
        let sha = check(f["path"].as_str().unwrap_or(""), &f["sha256"])?;
        sum.update(format!("{} {sha}
", f["id"]).as_bytes());
    }
    let system = m["system"].as_object().ok_or_else(|| BuildError("Assets manifest lacks system files.".into()))?;
    let mut names: Vec<&String> = system.keys().collect();
    names.sort();
    for name in names {
        let sha = check(&format!("system/{name}"), &system[name]["sha256"])?;
        sum.update(format!("{name} {sha}
").as_bytes());
    }
    let got = hex(&sum.finalize());
    if m["assets_sha256"].as_str() != Some(got.as_str()) {
        return err("Assets manifest does not match its files. Rebuild from your ROM.");
    }
    Ok(got)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Header + ARM9/ARM7 + FNT (file "a", dir "d" with file "b") + FAT (3 ids) + one ARM9 overlay.
    fn sample_rom() -> Vec<u8> {
        let mut fnt = Vec::new();
        fnt.extend_from_slice(&16u32.to_le_bytes());
        fnt.extend_from_slice(&1u16.to_le_bytes()); // first file id of root = 1 (id 0 is the overlay)
        fnt.extend_from_slice(&2u16.to_le_bytes());
        fnt.extend_from_slice(&(16u32 + 7).to_le_bytes());
        fnt.extend_from_slice(&2u16.to_le_bytes());
        fnt.extend_from_slice(&0xF000u16.to_le_bytes());
        fnt.extend_from_slice(&[1, b'a', 0x81, b'd', 0x01, 0xF0, 0]);
        fnt.extend_from_slice(&[1, b'b', 0]);
        let mut rom = vec![0u8; 0x200];
        let put = |rom: &mut Vec<u8>, data: &[u8]| -> (u32, u32) {
            let off = rom.len() as u32;
            rom.extend_from_slice(data);
            (off, data.len() as u32)
        };
        rom[12..16].copy_from_slice(b"TEST");
        let arm9 = put(&mut rom, b"ARM9CODE");
        let arm7 = put(&mut rom, b"ARM7");
        let fnt_r = put(&mut rom, &fnt);
        let ov = put(&mut rom, b"overlay!");
        let a = put(&mut rom, b"xy");
        let b = put(&mut rom, b"abc");
        let mut fat = Vec::new();
        for (o, l) in [ov, a, b] {
            fat.extend_from_slice(&o.to_le_bytes());
            fat.extend_from_slice(&(o + l).to_le_bytes());
        }
        let fat_r = put(&mut rom, &fat);
        let mut y9 = vec![0u8; 32];
        y9[0x18..0x1C].copy_from_slice(&0u32.to_le_bytes()); // overlay 0 -> FAT id 0
        let y9_r = put(&mut rom, &y9);
        for (field, (o, l)) in [(0x20usize, arm9), (0x30, arm7), (0x40, fnt_r), (0x48, fat_r), (0x50, y9_r)] {
            rom[field..field + 4].copy_from_slice(&o.to_le_bytes());
            let len_field = if field == 0x20 { 0x2C } else if field == 0x30 { 0x3C } else { field + 4 };
            rom[len_field..len_field + 4].copy_from_slice(&l.to_le_bytes());
        }
        rom
    }

    #[test]
    fn rips_every_fat_entry_once_with_ids_and_hashes() {
        let d = tempfile::tempdir().unwrap();
        let out = d.path().join("rip one");
        let r = rip(&sample_rom(), "00", &out, &mut |_| {}).unwrap();
        assert_eq!(r.file_count, 3);
        let a = out.join("assets");
        assert_eq!(std::fs::read(a.join("files/a")).unwrap(), b"xy");
        assert_eq!(std::fs::read(a.join("files/d/b")).unwrap(), b"abc");
        assert_eq!(std::fs::read(a.join("system/overlays/arm9_ov_000.bin")).unwrap(), b"overlay!");
        assert_eq!(std::fs::read(a.join("system/arm9.bin")).unwrap(), b"ARM9CODE");
        let m: Value = serde_json::from_slice(&std::fs::read(a.join("manifest.json")).unwrap()).unwrap();
        let files = m["files"].as_array().unwrap();
        assert_eq!(files.iter().map(|f| f["id"].as_u64().unwrap()).collect::<Vec<_>>(), vec![0, 1, 2]);
        assert_eq!(files[0]["kind"], "arm9_overlay");
        assert_eq!(files[2]["name"], "d/b");
        assert_eq!(files[2]["sha256"], hex(&Sha256::digest(b"abc")));
    }

    #[test]
    fn rip_is_deterministic_and_refuses_a_nonempty_output() {
        let d = tempfile::tempdir().unwrap();
        let (one, two) = (d.path().join("one"), d.path().join("two"));
        rip(&sample_rom(), "00", &one, &mut |_| {}).unwrap();
        rip(&sample_rom(), "00", &two, &mut |_| {}).unwrap();
        let read = |p: &Path| std::fs::read(p.join("assets/manifest.json")).unwrap();
        assert_eq!(read(&one), read(&two));
        let e = rip(&sample_rom(), "00", &one, &mut |_| {}).unwrap_err();
        assert!(e.0.contains("Output must be empty"));
        assert!(!d.path().read_dir().unwrap().any(|e| e.unwrap().file_name().to_string_lossy().starts_with("mkds-rip-")));
    }

    #[test]
    fn hostile_names_are_refused() {
        for name in ["../x", "/abs", "a\\b", "c:x", "a//b", ""] {
            assert!(safe_name(name).is_err(), "{name:?}");
        }
        assert!(safe_name("data/Course/cross_course.carc").is_ok());
    }

    #[test]
    fn rips_the_reference_rom_when_available() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let Ok(rom) = std::fs::read(root.join("6_data/rom/gamefile.nds")) else {
            return;
        };
        let d = tempfile::tempdir().unwrap();
        let r = rip(&rom, "test", &d.path().join("r"), &mut |_| {}).unwrap();
        let fat_entries = r.manifest["fat_entries"].as_u64().unwrap() as usize;
        assert_eq!(r.file_count, fat_entries, "one file per FAT id");
        let a = d.path().join("r/assets");
        assert!(a.join("files/data/Course/cross_course.carc").is_file());
        let names = r.manifest["files"].as_array().unwrap().iter().filter(|f| f["kind"] == "file").count();
        let overlays = r.manifest["files"].as_array().unwrap().iter().filter(|f| f["kind"] == "arm9_overlay").count();
        assert!(names > 500 && overlays > 0, "{names} named files, {overlays} overlays");
        assert_eq!(r.manifest["files"].as_array().unwrap().iter().filter(|f| f["kind"] == "unnamed").count(), 0);
    }

    #[test]
    fn verify_detects_missing_and_edited_assets() {
        let d = tempfile::tempdir().unwrap();
        let out = d.path().join("v");
        let r = rip(&sample_rom(), "00", &out, &mut |_| {}).unwrap();
        let a = out.join("assets");
        assert_eq!(verify(&a).unwrap(), r.manifest["assets_sha256"].as_str().unwrap());
        std::fs::write(a.join("files/d/b"), b"ABC").unwrap();
        assert!(verify(&a).unwrap_err().0.contains("Asset changed: files/d/b"));
        std::fs::remove_file(a.join("files/d/b")).unwrap();
        assert!(verify(&a).unwrap_err().0.contains("Asset missing: files/d/b"));
    }
}
