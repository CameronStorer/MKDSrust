//! All fixtures are generated from authored strings and structural headers.
#![allow(dead_code)]
use mkds_builder::{dsfiles, modpkg};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

fn put32(b: &mut [u8], at: usize, n: usize) {
    b[at..at + 4].copy_from_slice(&(n as u32).to_le_bytes());
}

pub fn rom(files: &[(&str, Vec<u8>)]) -> Vec<u8> {
    let mut fnt = vec![8, 0, 0, 0, 0, 0, 1, 0];
    for (name, _) in files {
        fnt.push(name.len() as u8);
        fnt.extend_from_slice(name.as_bytes());
    }
    fnt.push(0);
    let mut out = vec![0; 0x10000];
    out[0x200..0x200 + fnt.len()].copy_from_slice(&fnt);
    put32(&mut out, 0x40, 0x200);
    put32(&mut out, 0x44, fnt.len());
    put32(&mut out, 0x48, 0x1000);
    put32(&mut out, 0x4c, files.len() * 8);
    for (i, (_, bytes)) in files.iter().enumerate() {
        let start = out.len();
        out.extend_from_slice(bytes);
        let end = out.len();
        put32(&mut out, 0x1000 + i * 8, start);
        put32(&mut out, 0x1004 + i * 8, end);
    }
    out
}

pub fn narc(files: &[(&str, Vec<u8>)]) -> Vec<u8> {
    let mut dirs = BTreeSet::from([String::new()]);
    for (name, _) in files {
        for (at, _) in name.match_indices('/') {
            dirs.insert(name[..at].into());
        }
    }
    let dirs: Vec<_> = dirs.into_iter().collect();
    let mut fnt = vec![0; dirs.len() * 8];
    let mut payload = Vec::new();
    let mut ranges = Vec::new();
    for (i, dir) in dirs.iter().enumerate() {
        let at = fnt.len();
        put32(&mut fnt, i * 8, at);
        fnt[i * 8 + 4..i * 8 + 6].copy_from_slice(&(ranges.len() as u16).to_le_bytes());
        let parent = dir.rsplit_once('/').map_or("", |(p, _)| p);
        let parent = if i == 0 {
            dirs.len() as u16
        } else {
            0xf000 + dirs.iter().position(|d| d == parent).unwrap() as u16
        };
        fnt[i * 8 + 6..i * 8 + 8].copy_from_slice(&parent.to_le_bytes());
        for (name, bytes) in files {
            let (parent, name) = name.rsplit_once('/').unwrap_or(("", name));
            if parent != dir {
                continue;
            }
            fnt.push(name.len() as u8);
            fnt.extend_from_slice(name.as_bytes());
            while !payload.len().is_multiple_of(4) {
                payload.push(0xff);
            }
            let start = payload.len();
            payload.extend_from_slice(bytes);
            ranges.push((start, payload.len()));
        }
        for (j, child) in dirs.iter().enumerate().skip(1) {
            let (parent, name) = child.rsplit_once('/').unwrap_or(("", child));
            if parent == dir {
                fnt.push(0x80 | name.len() as u8);
                fnt.extend_from_slice(name.as_bytes());
                fnt.extend_from_slice(&(0xf000 + j as u16).to_le_bytes());
            }
        }
        fnt.push(0);
    }
    while !fnt.len().is_multiple_of(4) {
        fnt.push(0xff);
    }
    while !payload.len().is_multiple_of(4) {
        payload.push(0xff);
    }
    let mut out = b"NARC\xfe\xff\x00\x01\0\0\0\0\x10\0\x03\0".to_vec();
    out.extend_from_slice(b"BTAF");
    out.extend_from_slice(&((12 + ranges.len() * 8) as u32).to_le_bytes());
    out.extend_from_slice(&(ranges.len() as u16).to_le_bytes());
    out.extend_from_slice(&[0, 0]);
    for (a, b) in ranges {
        out.extend_from_slice(&(a as u32).to_le_bytes());
        out.extend_from_slice(&(b as u32).to_le_bytes());
    }
    for (tag, bytes) in [(b"BTNF", fnt), (b"GMIF", payload)] {
        out.extend_from_slice(tag);
        out.extend_from_slice(&((8 + bytes.len()) as u32).to_le_bytes());
        out.extend(bytes);
    }
    let len = out.len();
    put32(&mut out, 8, len);
    out
}

pub fn plain(bytes: &[u8]) -> Vec<u8> {
    if bytes.first() == Some(&0x10) {
        dsfiles::lz77_decompress(bytes, 1 << 26).unwrap()
    } else {
        bytes.to_vec()
    }
}
pub fn member(bytes: &[u8], path: &str) -> Vec<u8> {
    let archive = plain(bytes);
    dsfiles::parse_narc(&archive)
        .unwrap()
        .into_iter()
        .find(|e| e.name.as_deref() == Some(path))
        .unwrap()
        .data
        .to_vec()
}
pub fn patch(source: &[u8], target: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    modpkg::qbsdiff::Bsdiff::new(source, target)
        .compare(&mut out)
        .unwrap();
    out
}
pub fn package(id: &str, root: &str, edits: &[(&str, &[u8], &[u8])]) -> modpkg::overlay::Files {
    let mut members = serde_json::Map::new();
    let mut contents = BTreeMap::new();
    for (i, (path, source, target)) in edits.iter().enumerate() {
        let name = format!("patches/{i}.bsdiff");
        contents.insert(name.clone(), patch(source, target));
        members.insert((*path).into(), serde_json::json!({"patch": name, "source_sha256":modpkg::hash(source), "target_sha256":modpkg::hash(target)}));
    }
    let m = serde_json::json!({"format":3,"type":"other","id":id,"name":"Synthetic","version":"1.0.0","author":"Tests","game":"AMCE","files":{root:{"archive":members}}});
    contents.insert("manifest.json".into(), serde_json::to_vec(&m).unwrap());
    contents
}
pub fn manifest(contents: &modpkg::overlay::Files) -> modpkg::Manifest {
    modpkg::Manifest::parse(&contents["manifest.json"]).unwrap()
}
pub fn edit_manifest(
    contents: &mut modpkg::overlay::Files,
    edit: impl FnOnce(&mut serde_json::Value),
) {
    let mut value = serde_json::from_slice(&contents["manifest.json"]).unwrap();
    edit(&mut value);
    contents.insert("manifest.json".into(), serde_json::to_vec(&value).unwrap());
}
pub fn zip(contents: &modpkg::overlay::Files, path: &Path) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("package");
    for (name, bytes) in contents {
        let file = root.join(name);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, bytes).unwrap();
    }
    std::fs::rename(modpkg::pack(&root).unwrap(), path).unwrap();
}
