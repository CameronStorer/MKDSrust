//! NDS header/filesystem validation (port of 5_tools/rom_builder.py `inspect_rom`).
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

pub const REFERENCE_SHA256: &str =
    "0ea8a91a0ae3e9db26337c25a6a8568f6737fa36c1e0761988c0fa0ecbf210ed";

#[derive(Debug)]
pub struct BuildError(pub String);
impl std::fmt::Display for BuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for BuildError {}
impl From<std::io::Error> for BuildError {
    fn from(e: std::io::Error) -> Self {
        BuildError(e.to_string())
    }
}
pub fn err<T>(msg: impl Into<String>) -> Result<T, BuildError> {
    Err(BuildError(msg.into()))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RomInfo {
    pub path: String,
    pub size: u64,
    pub game_code: String,
    pub revision: u8,
    pub sha256: String,
    pub arm9_sha256: String,
    pub compatibility: String,
    pub reference_match: bool,
    pub file_count: usize,
    pub courses: Vec<String>,
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// SHA-1 names flat saves by the original cartridge image (not the sparse virtual card).
pub fn sha1(bytes: &[u8]) -> String {
    let mut h = [0x67452301u32, 0xefcdab89, 0x98badcfe, 0x10325476, 0xc3d2e1f0];
    let mut padded = bytes.to_vec();
    padded.push(0x80);
    while padded.len() % 64 != 56 { padded.push(0); }
    padded.extend_from_slice(&((bytes.len() as u64) * 8).to_be_bytes());
    for block in padded.chunks_exact(64) {
        let mut w = [0u32; 80];
        for (i, b) in block.chunks_exact(4).enumerate() { w[i] = u32::from_be_bytes(b.try_into().unwrap()); }
        for i in 16..80 { w[i] = (w[i-3] ^ w[i-8] ^ w[i-14] ^ w[i-16]).rotate_left(1); }
        let [mut a, mut b, mut c, mut d, mut e] = h;
        for (i, word) in w.into_iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | (!b & d), 0x5a827999),
                20..=39 => (b ^ c ^ d, 0x6ed9eba1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8f1bbcdc),
                _ => (b ^ c ^ d, 0xca62c1d6),
            };
            let t = a.rotate_left(5).wrapping_add(f).wrapping_add(e).wrapping_add(k).wrapping_add(word);
            e = d; d = c; c = b.rotate_left(30); b = a; a = t;
        }
        for (s, v) in h.iter_mut().zip([a,b,c,d,e]) { *s = s.wrapping_add(v); }
    }
    h.iter().map(|v| format!("{v:08x}")).collect()
}

#[cfg(test)]
mod sha1_tests {
    #[test]
    fn known_vectors() {
        assert_eq!(super::sha1(b""), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        assert_eq!(super::sha1(b"abc"), "a9993e364706816aba3e25717850c26c9cd0d89d");
    }
}

pub fn sha256_file(path: &Path) -> Result<String, BuildError> {
    let mut f = File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex(&h.finalize()))
}

fn u32le(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
fn u16le(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes(b[o..o + 2].try_into().unwrap())
}

fn region(f: &mut File, size: u64, off: u64, len: u64, label: &str) -> Result<Vec<u8>, BuildError> {
    if off < 0x200 || len == 0 || off + len > size {
        return err(format!("Invalid {label} range in NDS header."));
    }
    f.seek(SeekFrom::Start(off))?;
    let mut v = vec![0u8; len as usize];
    f.read_exact(&mut v)
        .map_err(|_| BuildError(format!("Truncated {label}.")))?;
    Ok(v)
}

struct Walk<'a> {
    fnt: &'a [u8],
    count: usize,
    fat: usize,
    visited: HashSet<u32>,
    names: Vec<String>,
    file_ids: Vec<(String, usize)>,
}

impl Walk<'_> {
    fn dir(&mut self, dir: u32, prefix: &str, depth: u32) -> Result<(), BuildError> {
        let idx = dir.wrapping_sub(0xF000) as usize;
        if depth > 32 || idx >= self.count || !self.visited.insert(dir) {
            return err("Invalid or cyclic FNT directory tree.");
        }
        let mut cur = u32le(self.fnt, idx * 8) as usize;
        let mut file_id = u16le(self.fnt, idx * 8 + 4) as usize;
        loop {
            let Some(&tag) = self.fnt.get(cur) else {
                return err("Truncated FNT directory.");
            };
            cur += 1;
            if tag == 0 {
                return Ok(());
            }
            let len = (tag & 127) as usize;
            if len == 0 || cur + len > self.fnt.len() {
                return err("Invalid FNT filename.");
            }
            let name = String::from_utf8_lossy(&self.fnt[cur..cur + len]).into_owned();
            cur += len;
            if tag & 128 != 0 {
                if cur + 2 > self.fnt.len() {
                    return err("Truncated FNT child directory.");
                }
                let child = u16le(self.fnt, cur) as u32;
                cur += 2;
                self.dir(child, &format!("{prefix}{name}/"), depth + 1)?;
            } else {
                if file_id >= self.fat {
                    return err("FNT file id exceeds FAT.");
                }
                let path = format!("{prefix}{name}");
                self.names.push(path.clone());
                self.file_ids.push((path, file_id));
                file_id += 1;
            }
        }
    }
}

fn walk_fnt(fnt: &[u8], fat_entries: usize) -> Result<Walk<'_>, BuildError> {
    if fnt.len() < 8 {
        return err("Malformed NDS filesystem tables.");
    }
    let count = u16le(fnt, 6) as usize;
    if !(1..=4096).contains(&count) || count * 8 > fnt.len() {
        return err("Invalid FNT directory count.");
    }
    let mut w = Walk {
        fnt,
        count,
        fat: fat_entries,
        visited: HashSet::new(),
        names: Vec::new(),
        file_ids: Vec::new(),
    };
    w.dir(0xF000, "", 0)?;
    Ok(w)
}

/// Parses the FNT and returns every file path (directory-qualified).
pub fn list_files(fnt: &[u8], fat_entries: usize) -> Result<Vec<String>, BuildError> {
    Ok(walk_fnt(fnt, fat_entries)?.names)
}

/// Parses the FNT and returns paths paired with their FAT file ids.
pub fn list_files_with_ids(
    fnt: &[u8],
    fat_entries: usize,
) -> Result<Vec<(String, usize)>, BuildError> {
    Ok(walk_fnt(fnt, fat_entries)?.file_ids)
}

/// A zero-copy NitroFS view over a user-supplied NDS image.
pub struct NitroFs<'a> {
    rom: &'a [u8],
    files: HashMap<String, (usize, usize)>,
}

impl<'a> NitroFs<'a> {
    /// Parses FNT/FAT paths and ranges from the NDS header without extracting files to disk.
    pub fn parse(rom: &'a [u8]) -> Result<Self, BuildError> {
        if rom.len() < 0x200 {
            return err("Truncated NDS header.");
        }
        let fnt_off = u32le(rom, 0x40) as usize;
        let fnt_len = u32le(rom, 0x44) as usize;
        let fat_off = u32le(rom, 0x48) as usize;
        let fat_len = u32le(rom, 0x4C) as usize;
        let fnt_end = fnt_off
            .checked_add(fnt_len)
            .ok_or_else(|| BuildError("Invalid FNT range.".into()))?;
        let fat_end = fat_off
            .checked_add(fat_len)
            .ok_or_else(|| BuildError("Invalid FAT range.".into()))?;
        if fnt_off < 0x200 || fnt_len == 0 || fnt_end > rom.len() {
            return err("Invalid FNT range in NDS header.");
        }
        if fat_off < 0x200 || fat_len % 8 != 0 || fat_end > rom.len() {
            return err("Invalid FAT range in NDS header.");
        }
        let fnt = &rom[fnt_off..fnt_end];
        let fat = &rom[fat_off..fat_end];
        let file_ids = list_files_with_ids(fnt, fat_len / 8)?;
        let mut files = HashMap::with_capacity(file_ids.len());
        for (path, file_id) in file_ids {
            let record_start = file_id
                .checked_mul(8)
                .ok_or_else(|| BuildError("FAT file id overflow.".into()))?;
            let record_end = record_start
                .checked_add(8)
                .ok_or_else(|| BuildError("FAT file id overflow.".into()))?;
            let record = fat
                .get(record_start..record_end)
                .ok_or_else(|| BuildError("FNT file id exceeds FAT.".into()))?;
            let start = u32le(record, 0) as usize;
            let end = u32le(record, 4) as usize;
            if start < 0x200 || start > end || end > rom.len() {
                return err(format!("Invalid FAT range for {path}."));
            }
            files.insert(path, (start, end));
        }
        Ok(Self { rom, files })
    }

    /// Returns a borrowed NitroFS file by its slash-separated path.
    pub fn file(&self, path: &str) -> Option<&'a [u8]> {
        let &(start, end) = self.files.get(path)?;
        self.rom.get(start..end)
    }

    /// Returns the directory-qualified paths in this ROM.
    pub fn paths(&self) -> impl Iterator<Item = &str> {
        self.files.keys().map(String::as_str)
    }
}

pub fn inspect_rom(path: &Path) -> Result<RomInfo, BuildError> {
    let path = path
        .canonicalize()
        .map_err(|e| BuildError(format!("Cannot open ROM: {e}")))?;
    let size = std::fs::metadata(&path)?.len();
    let mut f = File::open(&path)?;
    let mut header = [0u8; 0x200];
    f.read_exact(&mut header)
        .map_err(|_| BuildError("Truncated NDS header (at least 512 bytes required).".into()))?;
    let code = String::from_utf8_lossy(&header[12..16]).into_owned();
    let revision = header[0x1E];
    let arm9_len = u32le(&header, 0x2C) as u64;
    if arm9_len > 16 << 20 {
        return err("ARM9 section exceeds supported limits.");
    }
    let arm9 = region(&mut f, size, u32le(&header, 0x20) as u64, arm9_len, "ARM9")?;
    let (fnt_off, fnt_len) = (u32le(&header, 0x40) as u64, u32le(&header, 0x44) as u64);
    let (fat_off, fat_len) = (u32le(&header, 0x48) as u64, u32le(&header, 0x4C) as u64);
    if fnt_len > 16 << 20 || fat_len > 8 * 65536 {
        return err("Filesystem metadata exceeds supported limits.");
    }
    let fnt = region(&mut f, size, fnt_off, fnt_len, "FNT")?;
    let fat = region(&mut f, size, fat_off, fat_len, "FAT")?;
    if fnt.len() < 8 || fat.len() % 8 != 0 {
        return err("Malformed NDS filesystem tables.");
    }
    for e in fat.chunks_exact(8) {
        let (s, t) = (u32le(e, 0) as u64, u32le(e, 4) as u64);
        if s > t || t > size {
            return err("A filesystem entry points outside the ROM.");
        }
    }
    let names = list_files(&fnt, fat.len() / 8)?;
    let digest = sha256_file(&path)?;
    let known = digest == REFERENCE_SHA256;
    if !(code == "AMCE" && revision == 0) {
        return err(format!(
            "Unsupported ROM: game code {code:?}, revision {revision}; expected AMCE revision 0."
        ));
    }
    let missing: Vec<&str> = ["data/Course/cross_course.carc", "data/KartModelMain.carc"]
        .into_iter()
        .filter(|r| !names.iter().any(|n| n == r))
        .collect();
    if !missing.is_empty() {
        return err(format!(
            "Required game assets missing: {}",
            missing.join(", ")
        ));
    }
    let mut courses: Vec<String> = names
        .iter()
        .filter_map(|n| {
            n.strip_prefix("data/Course/")?
                .strip_suffix(".carc")
                .map(String::from)
        })
        .collect();
    courses.sort();
    let shown = path.to_string_lossy();
    Ok(RomInfo {
        path: shown.strip_prefix("\\\\?\\").unwrap_or(&shown).to_string(),
        size,
        game_code: code,
        revision,
        sha256: digest,
        arm9_sha256: hex(&Sha256::digest(&arm9)),
        compatibility: if known {
            "reference ROM".into()
        } else {
            "unverified AMCE revision 0 (possibly modified)".into()
        },
        reference_match: known,
        file_count: names.len(),
        courses,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Root dir (id 0xF000) holding file "a" and subdir "d" (0xF001) holding file "b".
    fn sample_fnt() -> Vec<u8> {
        let mut t = Vec::new();
        t.extend_from_slice(&16u32.to_le_bytes()); // root entries at 16
        t.extend_from_slice(&0u16.to_le_bytes());
        t.extend_from_slice(&2u16.to_le_bytes()); // dir count
        t.extend_from_slice(&(16u32 + 7).to_le_bytes()); // child entries
        t.extend_from_slice(&1u16.to_le_bytes());
        t.extend_from_slice(&0xF000u16.to_le_bytes());
        t.extend_from_slice(&[1, b'a', 0x81, b'd', 0x01, 0xF0, 0]); // 7 bytes
        t.extend_from_slice(&[1, b'b', 0]);
        t
    }

    #[test]
    fn walks_nested_directories() {
        assert_eq!(list_files(&sample_fnt(), 2).unwrap(), vec!["a", "d/b"]);
        assert_eq!(
            list_files_with_ids(&sample_fnt(), 2).unwrap(),
            vec![("a".into(), 0), ("d/b".into(), 1)]
        );
    }

    #[test]
    fn reads_nitrofs_files_by_path_without_extracting() {
        let fnt = sample_fnt();
        let (fnt_off, fat_off, file0, file1) = (0x200usize, 0x220usize, 0x300usize, 0x302usize);
        let mut rom = vec![0u8; 0x305];
        rom[0x40..0x44].copy_from_slice(&(fnt_off as u32).to_le_bytes());
        rom[0x44..0x48].copy_from_slice(&(fnt.len() as u32).to_le_bytes());
        rom[0x48..0x4C].copy_from_slice(&(fat_off as u32).to_le_bytes());
        rom[0x4C..0x50].copy_from_slice(&16u32.to_le_bytes());
        rom[fnt_off..fnt_off + fnt.len()].copy_from_slice(&fnt);
        rom[fat_off..fat_off + 4].copy_from_slice(&(file0 as u32).to_le_bytes());
        rom[fat_off + 4..fat_off + 8].copy_from_slice(&(file1 as u32).to_le_bytes());
        rom[fat_off + 8..fat_off + 12].copy_from_slice(&(file1 as u32).to_le_bytes());
        rom[fat_off + 12..fat_off + 16].copy_from_slice(&0x305u32.to_le_bytes());
        rom[file0..file1].copy_from_slice(b"xy");
        rom[file1..0x305].copy_from_slice(b"abc");

        let fs = NitroFs::parse(&rom).unwrap();
        assert_eq!(fs.file("a"), Some(&b"xy"[..]));
        assert_eq!(fs.file("d/b"), Some(&b"abc"[..]));
        assert_eq!(fs.file("missing"), None);
    }

    #[test]
    fn rejects_fat_overflow_and_cycles() {
        assert!(list_files(&sample_fnt(), 1).is_err());
        let mut t = sample_fnt();
        let n = t.len();
        t[n - 3 - 3 - 0] = t[n - 3 - 3]; // keep layout
        t[20] = 0x00; // child dir id 0xF000 -> cycle
        t[21] = 0xF0;
        let _ = n;
        assert!(list_files(&t, 4).is_err());
    }

    #[test]
    fn rejects_truncated_rom() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x.nds");
        std::fs::write(&p, [0u8; 100]).unwrap();
        assert!(inspect_rom(&p).is_err());
    }
}
