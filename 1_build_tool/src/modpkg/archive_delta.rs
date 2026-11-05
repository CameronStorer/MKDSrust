//! Format 3 consumer, ported from Toolbox's GPL-3.0 archive_delta reference.
//! Resolves archive names in memory; no member path is used as a filesystem path.
use super::{MemberPatch, Result, bspatch::Bspatch, hash, safe_path};
use crate::dsfiles;
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{self, Write},
};

pub const MAX_MEMBER: usize = 64 * 1024 * 1024;
pub const MAX_EXPANDED: usize = 256 * 1024 * 1024;
pub const MAX_DEPTH: usize = 8;

pub fn validate(members: &BTreeMap<String, MemberPatch>) -> Result<()> {
    if members.is_empty() || members.len() >= super::MAX_FILES {
        return Err("Invalid archive member count".into());
    }
    let mut folded = BTreeSet::new();
    for (path, member) in members {
        if !safe_path(path)
            || !safe_path(&member.patch)
            || dsfiles::ext_of(&member.patch) != "bsdiff"
            || dsfiles::format_by_ext(&dsfiles::ext_of(path)).is_none()
            || !folded.insert(path.to_ascii_lowercase())
        {
            return Err(format!("{path}: unsafe or ambiguous archive member/payload path").into());
        }
        for digest in [&member.source_sha256, &member.target_sha256] {
            if digest.len() != 64
                || !digest
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            {
                return Err(format!("{path}: invalid member SHA-256").into());
            }
        }
    }
    // A target cannot also be a container for another target (including case variants).
    for path in &folded {
        for (at, _) in path.match_indices('/') {
            if folded.contains(&path[..at]) {
                return Err(format!("{path}: overlapping archive member targets").into());
            }
        }
    }
    Ok(())
}

fn plain(bytes: &[u8], budget: &mut usize) -> Result<Vec<u8>> {
    if bytes.len() > MAX_MEMBER {
        return Err("archive member exceeds 64 MiB".into());
    }
    let limit = MAX_MEMBER.min(*budget);
    let result = if bytes.first() == Some(&0x10) {
        dsfiles::lz77_decompress(bytes, limit)?
    } else {
        if bytes.len() > limit {
            return Err("archive expansion exceeds 256 MiB".into());
        }
        bytes.to_vec()
    };
    reserve(result.len() as u64, budget)?;
    Ok(result)
}

fn reserve(size: u64, budget: &mut usize) -> Result<usize> {
    if size > MAX_MEMBER as u64 || size > *budget as u64 {
        return Err("archive expansion or patch output exceeds limit".into());
    }
    let size = size as usize;
    *budget -= size;
    Ok(size)
}

fn wrap(original: &[u8], bytes: Vec<u8>) -> Result<Vec<u8>> {
    let result = if original.first() == Some(&0x10) {
        dsfiles::lz77_compress(&bytes)
    } else {
        bytes
    };
    if result.len() > MAX_MEMBER {
        return Err("archive output exceeds 64 MiB".into());
    }
    Ok(result)
}

/// A forged BSDIFF size hint must not allow writes beyond the validated output size.
struct Output {
    bytes: Vec<u8>,
    limit: usize,
}
impl Write for Output {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.limit - self.bytes.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "patch exceeds declared output size",
            ));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub fn apply(
    original: &[u8],
    members: &BTreeMap<String, MemberPatch>,
    payloads: &BTreeMap<String, Vec<u8>>,
) -> Result<Vec<u8>> {
    validate(members)?;
    let mut seen = BTreeSet::new();
    let mut budget = MAX_EXPANDED;
    let result = apply_inner(original, "", members, payloads, &mut seen, 0, &mut budget)?;
    if let Some(path) = members.keys().find(|p| !seen.contains(*p)) {
        return Err(format!("{path}: archive patch targets a missing member").into());
    }
    Ok(result)
}

fn apply_inner(
    original: &[u8],
    prefix: &str,
    members: &BTreeMap<String, MemberPatch>,
    payloads: &BTreeMap<String, Vec<u8>>,
    seen: &mut BTreeSet<String>,
    depth: usize,
    budget: &mut usize,
) -> Result<Vec<u8>> {
    // Every failure, including malformed containers/patches and limits, identifies the requested member.
    let requested = members
        .keys()
        .find(|p| p.starts_with(prefix))
        .expect("targeted container");
    let result = (|| -> Result<Vec<u8>> {
        if depth > MAX_DEPTH {
            return Err("archive nesting exceeds 8".into());
        }
        let source = plain(original, budget)?;
        let entries = dsfiles::parse_narc_strict(&source)?;
        // Overlapping FAT ranges must not multiply a small container into unbounded
        // owned copies while staging unchanged members.
        let copied = entries
            .iter()
            .try_fold(0usize, |n, e| n.checked_add(e.data.len()))
            .ok_or("archive member size overflow")?;
        if copied > MAX_MEMBER {
            return Err("archive members exceed 64 MiB output limit".into());
        }
        let mut names = BTreeSet::new();
        let mut files = Vec::with_capacity(entries.len());
        for entry in &entries {
            let Some(name) = &entry.name else {
                files.push(entry.data.to_vec());
                continue;
            };
            if !safe_path(name) || !names.insert(name.to_ascii_lowercase()) {
                return Err(
                    format!("{prefix}{name}: unsafe or ambiguous archive member name").into(),
                );
            }
            let path = format!("{prefix}{name}");
            let bytes = if let Some(member) = members.get(&path) {
                let result = (|| -> Result<Vec<u8>> {
                    let source = plain(entry.data, budget)?;
                    if hash(&source) != member.source_sha256 {
                        return Err(
                            "source member hash mismatch (conflicting mod or wrong ROM)".into()
                        );
                    }
                    let patch = payloads
                        .get(&member.patch)
                        .ok_or("missing member patch payload")?;
                    if patch.len() as u64 > super::MAX_ZIP {
                        return Err("member patch exceeds package limit".into());
                    }
                    let patcher = Bspatch::new(patch)?;
                    let size = patcher.hint_target_size();
                    reserve(size, budget)?;
                    let mut output = Output {
                        bytes: Vec::new(),
                        limit: size as usize,
                    };
                    patcher.apply(&source, &mut output)?;
                    if output.bytes.len() != size as usize {
                        return Err("patch output size mismatch".into());
                    }
                    if hash(&output.bytes) != member.target_sha256 {
                        return Err("patched member hash mismatch".into());
                    }
                    wrap(entry.data, output.bytes)
                })()
                .map_err(|e| format!("{path}: {e}"))?;
                seen.insert(path);
                result
            } else if members.keys().any(|p| p.starts_with(&format!("{path}/"))) {
                apply_inner(
                    entry.data,
                    &format!("{path}/"),
                    members,
                    payloads,
                    seen,
                    depth + 1,
                    budget,
                )?
            } else {
                entry.data.to_vec()
            };
            files.push(bytes);
        }
        let rebuilt = dsfiles::update_narc_bounded(&source, &files, MAX_MEMBER)?;
        wrap(original, rebuilt)
    })();
    result.map_err(|e| format!("{requested}: {e}").into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expansion_and_patch_outputs_share_one_cumulative_budget() {
        let mut budget = MAX_EXPANDED;
        for _ in 0..4 {
            reserve(MAX_MEMBER as u64, &mut budget).unwrap();
        }
        assert_eq!(budget, 0);
        assert!(reserve(1, &mut budget).is_err());
        assert_eq!(budget, 0);
        let mut budget = MAX_EXPANDED;
        assert!(reserve(MAX_MEMBER as u64 + 1, &mut budget).is_err());
        assert_eq!(budget, MAX_EXPANDED);
        let mut budget = 7;
        plain(b"abc", &mut budget).unwrap();
        reserve(3, &mut budget).unwrap();
        let compressed = dsfiles::lz77_compress(b"abc");
        assert!(plain(&compressed, &mut budget).is_err());
        assert_eq!(budget, 1);
    }
}
