//! Transactional overlay on the verified in-memory card. Acceptance never calls this module.
use mkds_builder::{
    modpkg::{self, Entry, Manifest},
    rom,
};
use modpkg::qbsdiff;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};
type Result<T> = modpkg::Result<T>;

pub fn allowed(headless: bool) -> bool {
    allowed_for(
        headless,
        std::env::var("MKDS_NO_MODS").ok().as_deref(),
        std::env::var("MKDS_TIMING").ok().as_deref(),
        std::env::var("MKDS_HW_FS").ok().as_deref(),
        std::env::var("MKDS_HW_TIME").ok().as_deref(),
    )
}
fn allowed_for(
    headless: bool,
    no_mods: Option<&str>,
    timing: Option<&str>,
    fs: Option<&str>,
    time: Option<&str>,
) -> bool {
    !headless
        && no_mods != Some("1")
        && timing == Some("native")
        && fs == Some("native")
        && time != Some("exact")
}
pub fn apply(card: &mut Vec<u8>, game: &Path, log: &mut dyn FnMut(String)) -> Result<()> {
    let fs = rom::NitroFs::parse(card)?;
    let originals: BTreeMap<String, Vec<u8>> = fs
        .paths()
        .map(|p| (p.into(), fs.file(p).unwrap().to_vec()))
        .collect();
    let mut current = originals.clone();
    let all = modpkg::installed(game);
    let enabled = modpkg::enabled(game);
    let requested: Vec<_> = all
        .iter()
        .filter(|m| enabled.contains(&m.id))
        .cloned()
        .collect();
    let mut applied: Vec<Manifest> = Vec::new();
    let mut seen = BTreeSet::new();
    for id in enabled {
        if !seen.insert(id.clone()) {
            continue;
        }
        let Some(m) = requested.iter().find(|m| m.id == id) else {
            log(format!("[mods] skipped {id}: invalid or missing manifest"));
            continue;
        };
        let mut issues = modpkg::issues(m, &requested);
        // Earlier mods must have successfully supplied dependencies, not merely been requested.
        issues.extend(
            modpkg::issues(m, &applied)
                .into_iter()
                .filter(|s| s.contains("missing dependency")),
        );
        if !issues.is_empty() {
            log(format!("[mods] skipped {id}: {}", issues.join("; ")));
            continue;
        }
        let result = (|| -> Result<BTreeMap<String, Vec<u8>>> {
            let mods = game.join("mods").canonicalize()?;
            let root = mods.join(&id).canonicalize()?;
            if !root.starts_with(&mods) {
                return Err("Mod escapes mods directory".into());
            }
            let mut changes = BTreeMap::new();
            let mut total = 0u64;
            for (target, e) in &m.files {
                let file = root.join(e.source()).canonicalize()?;
                if !file.starts_with(&root) {
                    return Err("Payload escapes mod directory".into());
                }
                if std::fs::metadata(&file)?.len() > modpkg::MAX_ZIP {
                    return Err("Payload too large".into());
                }
                let payload = std::fs::read(file)?;
                let bytes = match e {
                    Entry::Add(_) => {
                        if originals.contains_key(target) {
                            return Err("add replaces retail path".into());
                        }
                        payload
                    }
                    Entry::Patch(_) => {
                        if !originals.contains_key(target) {
                            return Err("patch target absent from ROM".into());
                        }
                        let source = current.get(target).ok_or("Patch target missing")?;
                        // Input key includes current bytes, so changing load order invalidates the cache.
                        let key = modpkg::hash(
                            format!("v1:{}:{}", modpkg::hash(source), modpkg::hash(&payload))
                                .as_bytes(),
                        );
                        let cache = game.join("mods/.cache");
                        std::fs::create_dir_all(&cache)?;
                        let cached = std::fs::read_to_string(cache.join(format!("{key}.index")))
                            .ok()
                            .and_then(|digest| {
                                if digest.len() != 64
                                    || !digest.bytes().all(|b| b.is_ascii_hexdigit())
                                {
                                    return None;
                                }
                                let file = cache.join(&digest);
                                if std::fs::metadata(&file).ok()?.len() > modpkg::MAX_UNPACKED {
                                    return None;
                                }
                                std::fs::read(file)
                                    .ok()
                                    .filter(|b| modpkg::hash(b) == digest)
                            });
                        if let Some(bytes) = cached {
                            bytes
                        } else {
                            let patcher = qbsdiff::Bspatch::new(&payload)?;
                            if patcher.hint_target_size() > modpkg::MAX_UNPACKED {
                                return Err("Patch result too large".into());
                            }
                            let mut bytes = Vec::new();
                            patcher.apply(source, &mut bytes)?;
                            let digest = modpkg::hash(&bytes);
                            std::fs::write(cache.join(&digest), &bytes)?;
                            std::fs::write(cache.join(format!("{key}.index")), digest)?;
                            bytes
                        }
                    }
                };
                total += bytes.len() as u64;
                if total > modpkg::MAX_UNPACKED {
                    return Err("Mod result exceeds limit".into());
                }
                changes.insert(target.clone(), bytes);
            }
            Ok(changes)
        })();
        match result {
            Ok(changes) => {
                let mut next = current.clone();
                next.extend(changes);
                match rebuild(card, &originals, &next) {
                    Ok(_) => {
                        current = next;
                        for target in m.files.keys() {
                            log(format!("[mods] {id}: applied {target}"));
                        }
                        applied.push(m.clone());
                    }
                    Err(e) => log(format!("[mods] skipped {id}: {e}")),
                }
            }
            Err(e) => log(format!("[mods] skipped {id}: {e}")),
        }
    }
    if applied.is_empty() {
        return Ok(());
    }
    *card = rebuild(card, &originals, &current)?;
    Ok(())
}

fn word(card: &[u8], off: usize) -> usize {
    u32::from_le_bytes(card[off..off + 4].try_into().unwrap()) as usize
}
fn set(card: &mut [u8], off: usize, n: usize) {
    card[off..off + 4].copy_from_slice(&(n as u32).to_le_bytes());
}
fn append(card: &mut Vec<u8>, bytes: &[u8]) -> Result<usize> {
    let off = (card.len() + 511) & !511;
    if off
        .checked_add(bytes.len())
        .is_none_or(|n| n > 512 * 1024 * 1024)
    {
        return Err("Virtual card exceeds 512 MB".into());
    }
    card.resize(off, 0xff);
    card.extend_from_slice(bytes);
    Ok(off)
}
fn rebuild(
    original_card: &[u8],
    originals: &BTreeMap<String, Vec<u8>>,
    files: &BTreeMap<String, Vec<u8>>,
) -> Result<Vec<u8>> {
    let mut card = original_card.to_vec();
    let fat_off = word(&card, 0x48);
    let fat_len = word(&card, 0x4c);
    let mut fat = card[fat_off..fat_off + fat_len].to_vec();
    let fnt_off = word(&card, 0x40);
    let fnt_len = word(&card, 0x44);
    let ids = rom::list_files_with_ids(&card[fnt_off..fnt_off + fnt_len], fat_len / 8)?;
    for (path, id) in &ids {
        if files[path] != originals[path] {
            let bytes = &files[path];
            let off = append(&mut card, bytes)?;
            set(&mut fat, id * 8, off);
            set(&mut fat, id * 8 + 4, off + bytes.len());
        }
    }
    if files.len() != originals.len() {
        // Keep original FAT IDs (overlay tables and direct-ID opens). New path lookup aliases
        // use appended contiguous IDs per directory, so insertions cannot renumber old IDs.
        let mut dirs = BTreeSet::from([String::new()]);
        for path in files.keys() {
            let mut dir = path.as_str();
            while let Some((parent, _)) = dir.rsplit_once('/') {
                dirs.insert(parent.to_string());
                dir = parent;
            }
        }
        let dirs: Vec<_> = dirs.into_iter().collect();
        if dirs.len() > 4096 {
            return Err("Too many NitroFS directories".into());
        }
        let mut fnt = vec![0u8; dirs.len() * 8];
        for (i, dir) in dirs.iter().enumerate() {
            let start = fnt.len();
            let first = fat.len() / 8;
            if first > u16::MAX as usize {
                return Err("Too many NitroFS files".into());
            }
            for (path, bytes) in files {
                let (parent, name) = path.rsplit_once('/').unwrap_or(("", path));
                if parent != dir {
                    continue;
                }
                if name.len() > 127 {
                    return Err("NitroFS name too long".into());
                }
                fnt.push(name.len() as u8);
                fnt.extend_from_slice(name.as_bytes());
                let off = append(&mut card, bytes)?;
                fat.extend_from_slice(&(off as u32).to_le_bytes());
                fat.extend_from_slice(&((off + bytes.len()) as u32).to_le_bytes());
            }
            for (child_id, child) in dirs.iter().enumerate().skip(1) {
                let (parent, name) = child.rsplit_once('/').unwrap_or(("", child));
                if parent != dir {
                    continue;
                }
                if name.len() > 127 {
                    return Err("NitroFS directory name too long".into());
                }
                fnt.push(0x80 | name.len() as u8);
                fnt.extend_from_slice(name.as_bytes());
                fnt.extend_from_slice(&(0xf000u16 + child_id as u16).to_le_bytes());
            }
            fnt.push(0);
            let parent = if i == 0 {
                dirs.len() as u16
            } else {
                let parent = dir.rsplit_once('/').map(|p| p.0).unwrap_or("");
                0xf000 + dirs.iter().position(|d| d == parent).unwrap() as u16
            };
            set(&mut fnt, i * 8, start);
            fnt[i * 8 + 4..i * 8 + 6].copy_from_slice(&(first as u16).to_le_bytes());
            fnt[i * 8 + 6..i * 8 + 8].copy_from_slice(&parent.to_le_bytes());
        }
        let off = append(&mut card, &fnt)?;
        set(&mut card, 0x40, off);
        set(&mut card, 0x44, fnt.len());
    }
    let off = append(&mut card, &fat)?;
    set(&mut card, 0x48, off);
    set(&mut card, 0x4c, fat.len());
    Ok(card)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> Vec<u8> {
        let mut card = vec![0; 0x10000];
        let fnt = [8, 0, 0, 0, 0, 0, 1, 0, 1, b'a', 0];
        set(&mut card, 0x40, 0x200);
        set(&mut card, 0x44, fnt.len());
        card[0x200..0x200 + fnt.len()].copy_from_slice(&fnt);
        set(&mut card, 0x48, 0x220);
        set(&mut card, 0x4c, 8);
        set(&mut card, 0x220, 0x8000);
        set(&mut card, 0x224, 0x8003);
        card[0x8000..0x8003].copy_from_slice(b"old");
        card
    }
    #[test]
    fn patches_stack_failed_mods_are_atomic_and_cache_corruption_recovers() {
        let game = tempfile::tempdir().unwrap();
        for (id, from, to, bad) in [
            ("test.one", b"old".as_slice(), b"first".as_slice(), false),
            ("test.bad", b"first".as_slice(), b"wrong".as_slice(), true),
            ("test.two", b"first".as_slice(), b"second".as_slice(), false),
        ] {
            let root = game.path().join("mods").join(id);
            std::fs::create_dir_all(&root).unwrap();
            let mut patch = Vec::new();
            qbsdiff::Bsdiff::new(from, to).compare(&mut patch).unwrap();
            std::fs::write(root.join("patch.bsdiff"), patch).unwrap();
            let mut files = serde_json::json!({"a":{"patch":"patch.bsdiff"}});
            if bad {
                files["z"] = serde_json::json!({"patch":"patch.bsdiff"});
            }
            let manifest = serde_json::json!({"format":1,"type":"other","id":id,"name":"Synthetic","author":"Test","version":"1.0.0","game":"AMCE","files":files});
            std::fs::write(
                root.join("manifest.json"),
                serde_json::to_vec(&manifest).unwrap(),
            )
            .unwrap();
        }
        modpkg::save_enabled(
            game.path(),
            &["test.one".into(), "test.bad".into(), "test.two".into()],
        )
        .unwrap();
        for pass in 0..2 {
            let mut card = fixture();
            let mut lines = Vec::new();
            apply(&mut card, game.path(), &mut |l| lines.push(l)).unwrap();
            assert_eq!(
                rom::NitroFs::parse(&card).unwrap().file("a"),
                Some(b"second".as_slice())
            );
            assert!(lines.iter().any(|l| l.contains("skipped test.bad")));
            if pass == 0 {
                for entry in std::fs::read_dir(game.path().join("mods/.cache"))
                    .unwrap()
                    .flatten()
                {
                    if entry.path().extension().is_none() {
                        std::fs::write(entry.path(), b"corrupt").unwrap();
                    }
                }
            }
        }
    }
    #[test]
    fn exact_diagnostics_and_explicit_disable_never_apply_mods() {
        assert!(allowed_for(
            false,
            None,
            Some("native"),
            Some("native"),
            None
        ));
        assert!(!allowed_for(
            false,
            Some("1"),
            Some("native"),
            Some("native"),
            None
        ));
        assert!(!allowed_for(
            false,
            None,
            Some("exact"),
            Some("native"),
            None
        ));
        assert!(!allowed_for(false, None, None, Some("native"), None));
        assert!(!allowed_for(
            true,
            None,
            Some("native"),
            Some("native"),
            None
        ));
        assert!(!allowed_for(
            false,
            None,
            Some("native"),
            Some("native"),
            Some("exact")
        ));
    }
    #[test]
    fn resized_and_added_files_preserve_old_fat_ids() {
        let mut card = vec![0; 0x10000];
        let fnt = [8, 0, 0, 0, 0, 0, 1, 0, 1, b'a', 0];
        set(&mut card, 0x40, 0x200);
        set(&mut card, 0x44, fnt.len());
        card[0x200..0x200 + fnt.len()].copy_from_slice(&fnt);
        set(&mut card, 0x48, 0x220);
        set(&mut card, 0x4c, 8);
        set(&mut card, 0x220, 0x8000);
        set(&mut card, 0x224, 0x8003);
        card[0x8000..0x8003].copy_from_slice(b"old");
        let originals = BTreeMap::from([("a".into(), b"old".to_vec())]);
        let files = BTreeMap::from([
            ("a".into(), b"longer".to_vec()),
            ("new/b".into(), b"added".to_vec()),
        ]);
        let card = rebuild(&card, &originals, &files).unwrap();
        let fs = rom::NitroFs::parse(&card).unwrap();
        assert_eq!(fs.file("a"), Some(b"longer".as_slice()));
        assert_eq!(fs.file("new/b"), Some(b"added".as_slice()));
        let old = word(&card, word(&card, 0x48));
        assert_eq!(&card[old..old + 6], b"longer");
    }
}
