//! Shared, transactional file staging for the game and launcher install validation.
use super::{Entry, MAX_UNPACKED, MAX_ZIP, Manifest, Result, archive_delta, bspatch, hash};
use std::{collections::BTreeMap, path::Path};

pub type Files = BTreeMap<String, Vec<u8>>;

pub fn read_payloads(root: &Path, manifest: &Manifest) -> Result<Files> {
    let root = root.canonicalize()?;
    let mut payloads = BTreeMap::new();
    for (target, entry) in &manifest.files {
        for source in entry.sources() {
            let label = entry.payload_target(target, source);
            let bytes = (|| -> Result<Vec<u8>> {
                let file = root.join(source).canonicalize()?;
                if !file.starts_with(&root) {
                    return Err("Payload escapes mod directory".into());
                }
                if std::fs::metadata(&file)?.len() > MAX_ZIP {
                    return Err("Payload too large".into());
                }
                Ok(std::fs::read(file)?)
            })()
            .map_err(|e| format!("{label}: {source}: {e}"))?;
            payloads.insert(source.into(), bytes);
        }
    }
    Ok(payloads)
}

/// No caller-visible changes are made unless every entry succeeds. Cache keys include
/// the current input, full entry metadata and every ordered payload hash.
pub fn stage(
    manifest: &Manifest,
    payloads: &Files,
    originals: &Files,
    current: &Files,
    cache: Option<&Path>,
) -> Result<Files> {
    let mut changes = BTreeMap::new();
    let mut total = 0u64;
    let manifest_hash = hash(&serde_json::to_vec(manifest)?);
    for (target, entry) in &manifest.files {
        let bytes = (|| -> Result<Vec<u8>> {
            if let Entry::Add(a) = entry {
                if originals.contains_key(target) {
                    return Err("add replaces retail path".into());
                }
                return payloads
                    .get(&a.add)
                    .cloned()
                    .ok_or_else(|| "missing add payload".into());
            }
            if !originals.contains_key(target) {
                return Err("patch target absent from ROM".into());
            }
            let source = current.get(target).ok_or("Patch target missing")?;
            let mut key = format!(
                "overlay-3-1:{manifest_hash}:{}:{}",
                hash(source),
                serde_json::to_string(entry)?
            );
            for path in entry.sources() {
                let label = entry.payload_target(target, path);
                let payload = payloads
                    .get(path)
                    .ok_or_else(|| format!("{label}: {path}: missing patch payload"))?;
                if payload.len() as u64 > MAX_ZIP {
                    return Err(format!("{label}: {path}: payload too large").into());
                }
                key.push_str(&format!(":{}", hash(payload)));
            }
            let key = hash(key.as_bytes());
            let limit = if matches!(entry, Entry::Archive(_)) {
                archive_delta::MAX_MEMBER as u64
            } else {
                MAX_UNPACKED
            };
            if let Some(bytes) = cache.and_then(|cache| cached(cache, &key, limit)) {
                return Ok(bytes);
            }
            let bytes = match entry {
                Entry::Patch(p) => {
                    let patcher = bspatch::Bspatch::new(&payloads[&p.patch])?;
                    if patcher.hint_target_size() > MAX_UNPACKED {
                        return Err("Patch result too large".into());
                    }
                    let mut bytes = Vec::new();
                    patcher.apply(source, &mut bytes)?;
                    bytes
                }
                Entry::Archive(a) => archive_delta::apply(source, &a.archive, payloads)?,
                Entry::Add(_) => unreachable!(),
            };
            if let Some(cache) = cache {
                std::fs::create_dir_all(cache)?;
                let digest = hash(&bytes);
                std::fs::write(cache.join(&digest), &bytes)?;
                std::fs::write(cache.join(format!("{key}.index")), digest)?;
            }
            Ok(bytes)
        })()
        .map_err(|e| {
            let label = match entry {
                Entry::Archive(a) => a
                    .archive
                    .keys()
                    .next()
                    .map_or(target.as_str(), String::as_str),
                _ => target,
            };
            format!("{target}/{label}: {e}")
        })?;
        total += bytes.len() as u64;
        if total > MAX_UNPACKED {
            return Err("Mod result exceeds limit".into());
        }
        changes.insert(target.clone(), bytes);
    }
    Ok(changes)
}

fn cached(cache: &Path, key: &str, limit: u64) -> Option<Vec<u8>> {
    let digest = std::fs::read_to_string(cache.join(format!("{key}.index"))).ok()?;
    if digest.len() != 64 || !digest.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let file = cache.join(&digest);
    if std::fs::metadata(&file).ok()?.len() > limit {
        return None;
    }
    std::fs::read(file).ok().filter(|b| hash(b) == digest)
}
