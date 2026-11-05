//! DS file formats a mod package may contain (PLUGIN_FORMAT.md "Allowed file types"), mirroring the site's
//! `src/lib/filetypes.ts` rule for rule: known extension, magic matches the extension, `.carc` is LZ77 (type 0x10)
//! compressed NARC, `.kcl` header offsets are sane, and every file inside a NARC/CARC passes the same rules.
//! Also the archive helpers the mod workspace needs: LZ77 compression and NARC extraction / rebuilding.

pub struct DsFormat {
    pub ext: &'static str,
    pub magic: Option<&'static [u8]>,
    pub group: &'static str,
}

const fn f(ext: &'static str, magic: &'static [u8], group: &'static str) -> DsFormat {
    DsFormat {
        ext,
        magic: Some(magic),
        group,
    }
}

pub const DS_FORMATS: &[DsFormat] = &[
    f("nsbmd", b"BMD0", "Models and animation"),
    f("nsbtx", b"BTX0", "Models and animation"),
    f("nsbca", b"BCA0", "Models and animation"),
    f("nsbta", b"BTA0", "Models and animation"),
    f("nsbtp", b"BTP0", "Models and animation"),
    f("nsbma", b"BMA0", "Models and animation"),
    f("nsbva", b"BVA0", "Models and animation"),
    f("nclr", b"RLCN", "2D graphics"),
    f("ncgr", b"RGCN", "2D graphics"),
    f("nscr", b"RCSN", "2D graphics"),
    f("ncer", b"RECN", "2D graphics"),
    f("nanr", b"RNAN", "2D graphics"),
    f("narc", b"NARC", "Archives"),
    f("carc", b"NARC", "Archives"),
    f("sdat", b"SDAT", "Sound"),
    f("sseq", b"SSEQ", "Sound"),
    f("sbnk", b"SBNK", "Sound"),
    f("swar", b"SWAR", "Sound"),
    f("strm", b"STRM", "Sound"),
    f("bmg", b"MESGbmg1", "Text"),
    DsFormat {
        ext: "kcl",
        magic: None,
        group: "Course",
    },
    f("nkm", b"NKMD", "Course"),
];

pub const README_NAMES: [&str; 2] = ["readme.md", "readme.txt"];
pub const PREVIEW_EXTENSIONS: [&str; 4] = ["png", "jpg", "jpeg", "webp"];
pub const MAX_PREVIEW_BYTES: usize = 5 * 1024 * 1024;
pub const MAX_PREVIEWS: usize = 8;
pub const MAX_README_BYTES: usize = 1024 * 1024;
const MAX_DEPTH: usize = 4;

/// Lower-case extension of the last path part ("" when there is none or the name starts with the dot).
pub fn ext_of(name: &str) -> String {
    let base = name.rsplit('/').next().unwrap_or("");
    match base.rfind('.') {
        Some(dot) if dot > 0 => base[dot + 1..].to_ascii_lowercase(),
        _ => String::new(),
    }
}
pub fn format_by_ext(ext: &str) -> Option<&'static DsFormat> {
    DS_FORMATS.iter().find(|f| f.ext == ext)
}
/// The DS format whose magic `b` starts with (never `.carc`, whose magic is only visible after decompressing).
pub fn format_by_magic(b: &[u8]) -> Option<&'static DsFormat> {
    DS_FORMATS
        .iter()
        .find(|f| f.ext != "carc" && f.magic.is_some_and(|m| b.starts_with(m)))
}
fn le32(b: &[u8], o: usize) -> Option<usize> {
    Some(u32::from_le_bytes(b.get(o..o.checked_add(4)?)?.try_into().ok()?) as usize)
}
fn le16(b: &[u8], o: usize) -> Option<usize> {
    Some(u16::from_le_bytes(b.get(o..o.checked_add(2)?)?.try_into().ok()?) as usize)
}

// ---------------------------------------------------------------- LZ77 (type 0x10)

/// Decompress LZ77 type 0x10. Fails on bad data or when the declared size exceeds `max_out`.
pub fn lz77_decompress(b: &[u8], max_out: usize) -> Result<Vec<u8>, String> {
    if b.len() < 4 || b[0] != 0x10 {
        return Err("not LZ77 (type 0x10) data".into());
    }
    let mut size = b[1] as usize | (b[2] as usize) << 8 | (b[3] as usize) << 16;
    let mut p = 4;
    if size == 0 {
        size = le32(b, 4).ok_or("LZ77 header ends early")?;
        p = 8;
    }
    if size > max_out {
        return Err(format!("decompressed size {size} is over the limit"));
    }
    let mut out = Vec::with_capacity(size);
    while out.len() < size {
        let flags = *b.get(p).ok_or("LZ77 data ends early")?;
        p += 1;
        for bit in (0..8).rev() {
            if out.len() >= size {
                break;
            }
            if flags & (1 << bit) != 0 {
                let (x, y) = (
                    *b.get(p).ok_or("LZ77 data ends early")?,
                    *b.get(p + 1).ok_or("LZ77 data ends early")?,
                );
                p += 2;
                let len = (x >> 4) as usize + 3;
                let disp = (((x & 0x0f) as usize) << 8 | y as usize) + 1;
                if disp > out.len() {
                    return Err("LZ77 back-reference before the start".into());
                }
                for _ in 0..len {
                    if out.len() >= size {
                        break;
                    }
                    out.push(out[out.len() - disp]);
                }
            } else {
                out.push(*b.get(p).ok_or("LZ77 data ends early")?);
                p += 1;
            }
        }
    }
    Ok(out)
}

/// Compress to LZ77 type 0x10 (greedy, hash chains over the 4 KiB window). Output always decompresses to `data`.
pub fn lz77_compress(data: &[u8]) -> Vec<u8> {
    let n = data.len();
    let mut out = Vec::with_capacity(n / 2 + 16);
    if n > 0 && n < 1 << 24 {
        out.extend_from_slice(&[0x10, n as u8, (n >> 8) as u8, (n >> 16) as u8]);
    } else {
        out.extend_from_slice(&[0x10, 0, 0, 0]);
        out.extend_from_slice(&(n as u32).to_le_bytes());
    }
    const NONE: usize = usize::MAX;
    let mut head = vec![NONE; 1 << 16];
    let mut prev = vec![NONE; n];
    let key = |i: usize| {
        ((data[i] as usize) << 8 ^ (data[i + 1] as usize) << 4 ^ data[i + 2] as usize) & 0xffff
    };
    let insert = |i: usize, head: &mut Vec<usize>, prev: &mut Vec<usize>| {
        if i + 2 < n {
            let k = key(i);
            prev[i] = head[k];
            head[k] = i;
        }
    };
    let mut i = 0;
    while i < n {
        let flag_at = out.len();
        out.push(0);
        let mut flags = 0u8;
        for bit in (0..8).rev() {
            if i >= n {
                break;
            }
            let (mut best_len, mut best_disp) = (0, 0);
            if i + 2 < n {
                let mut cand = head[key(i)];
                let mut tries = 0;
                while cand != NONE && i - cand <= 0x1000 && tries < 256 {
                    if i - cand < 2 {
                        cand = prev[cand];
                        tries += 1;
                        continue;
                    }
                    let disp = i - cand;
                    // disp 1 is legal for this format (VRAM-safe variants avoid it; the game's decoder accepts it)
                    let max = 18.min(n - i);
                    let mut l = 0;
                    while l < max && data[cand + l] == data[i + l] {
                        l += 1;
                    }
                    if l > best_len {
                        best_len = l;
                        best_disp = disp;
                        if l == max {
                            break;
                        }
                    }
                    cand = prev[cand];
                    tries += 1;
                }
            }
            if best_len >= 3 {
                flags |= 1 << bit;
                let d = best_disp - 1;
                out.push(((best_len - 3) as u8) << 4 | (d >> 8) as u8);
                out.push(d as u8);
                for j in i..i + best_len {
                    insert(j, &mut head, &mut prev);
                }
                i += best_len;
            } else {
                out.push(data[i]);
                insert(i, &mut head, &mut prev);
                i += 1;
            }
        }
        out[flag_at] = flags;
    }
    while out.len() % 4 != 0 {
        out.push(0);
    }
    out
}

// ---------------------------------------------------------------- KCL

/// KCL has no magic; sanity-check its header offsets exactly as the site does.
pub fn kcl_header_problem(b: &[u8]) -> Option<String> {
    if b.len() < 0x40 {
        return Some("too short for a KCL header".into());
    }
    let (pos, nrm, tri, oct) = (le32(b, 0)?, le32(b, 4)?, le32(b, 8)?, le32(b, 12)?);
    if !(0x38..=0x40).contains(&pos) {
        return Some("KCL positions offset is not right after the header".into());
    }
    if [pos, nrm, tri, oct].iter().any(|x| x % 4 != 0) {
        return Some("KCL section offsets are not 4-byte aligned".into());
    }
    if !(pos <= nrm && nrm <= tri + 0x10 && tri + 0x10 <= oct && oct < b.len()) {
        return Some("KCL section offsets are out of order or past the end".into());
    }
    if (nrm - pos) % 12 != 0 {
        return Some("KCL position section is not whole vectors".into());
    }
    None
}

// ---------------------------------------------------------------- NARC

pub struct NarcEntry<'a> {
    pub name: Option<String>,
    pub data: &'a [u8],
}
struct NarcLayout {
    btaf: usize,
    count: usize,
    btnf: usize,
    btnf_size: usize,
    gmif: usize,
}
fn narc_layout(b: &[u8]) -> Result<NarcLayout, String> {
    if !b.starts_with(b"NARC") {
        return Err("NARC magic missing".into());
    }
    let header = le16(b, 12).ok_or("NARC header ends early")?;
    let btaf = if header > 0 { header } else { 0x10 };
    if b.get(btaf..btaf + 4) != Some(b"BTAF") {
        return Err("NARC file table (BTAF) missing".into());
    }
    let count = le16(b, btaf + 8).ok_or("NARC file table ends early")?;
    if btaf + 12 + count * 8 > b.len() {
        return Err("NARC file table runs past the end".into());
    }
    let btnf = btaf
        .checked_add(le32(b, btaf + 4).ok_or("bad BTAF")?)
        .ok_or("bad BTAF")?;
    if b.get(btnf..btnf + 4) != Some(b"BTNF") {
        return Err("NARC name table (BTNF) missing".into());
    }
    let btnf_size = le32(b, btnf + 4).ok_or("bad BTNF")?;
    let gmif = btnf.checked_add(btnf_size).ok_or("bad BTNF")?;
    if b.get(gmif..gmif + 4) != Some(b"GMIF") {
        return Err("NARC data block (GMIF) missing".into());
    }
    Ok(NarcLayout {
        btaf,
        count,
        btnf,
        btnf_size,
        gmif,
    })
}

/// Parse a NARC (BTAF allocation table, BTNF names, GMIF data).
pub fn parse_narc(b: &[u8]) -> Result<Vec<NarcEntry<'_>>, String> {
    let l = narc_layout(b)?;
    let data = l.gmif + 8;
    let data_end = b.len().min(l.gmif + le32(b, l.gmif + 4).ok_or("bad GMIF")?);
    let names = narc_names(b, l.btnf + 8, l.btnf + l.btnf_size, l.count);
    let mut out = Vec::with_capacity(l.count);
    for i in 0..l.count {
        let start = le32(b, l.btaf + 12 + i * 8).unwrap();
        let end = le32(b, l.btaf + 16 + i * 8).unwrap();
        if end < start || data + end > data_end {
            return Err(format!("NARC entry {i} points outside the data block"));
        }
        out.push(NarcEntry {
            name: names[i].clone(),
            data: &b[data + start..data + end],
        });
    }
    Ok(out)
}

/// Format 3 needs unambiguous, valid member names rather than the legacy reader's
/// best-effort names for damaged archives.
pub fn parse_narc_strict(b: &[u8]) -> Result<Vec<NarcEntry<'_>>, String> {
    let l = narc_layout(b)?;
    let declared = le32(b, 8).ok_or("truncated NARC header")?;
    if !(16..=b.len()).contains(&declared)
        || le16(b, 12).is_none_or(|n| n < 16)
        || l.btnf_size < 8
        || l.btaf + 12 + l.count * 8 > l.btnf
        || l.gmif + 8 > declared
        || le32(b, l.gmif + 4).is_none_or(|n| n < 8 || n > declared - l.gmif)
    {
        return Err("invalid NARC block ranges".into());
    }
    let fnt = b.get(l.btnf + 8..l.gmif).ok_or("invalid NARC name table")?;
    let data = l.gmif + 8;
    let data_end = l.gmif + le32(b, l.gmif + 4).ok_or("invalid GMIF size")?;
    let mut entries = Vec::with_capacity(l.count);
    for i in 0..l.count {
        let start = le32(b, l.btaf + 12 + i * 8).ok_or("invalid NARC member range")?;
        let end = le32(b, l.btaf + 16 + i * 8).ok_or("invalid NARC member range")?;
        if end < start || end > data_end - data {
            return Err(format!("NARC entry {i} points outside the data block"));
        }
        entries.push(NarcEntry {
            name: None,
            data: &b[data + start..data + end],
        });
    }
    // SDK archives can have an unnamed root record; they have no addressable names.
    if fnt.len() == 8 && le32(fnt, 0) == Some(4) {
        for entry in &mut entries {
            entry.name = None;
        }
        return Ok(entries);
    }
    let dirs = le16(fnt, 6).ok_or("truncated NARC name table")?;
    if !(1..=4096).contains(&dirs) || dirs * 8 > fnt.len() {
        return Err("invalid NARC directory count".into());
    }
    fn walk(
        fnt: &[u8],
        dir: usize,
        dirs: usize,
        prefix: &str,
        entries: &mut [NarcEntry<'_>],
        seen: &mut std::collections::BTreeSet<usize>,
        depth: usize,
    ) -> Result<(), String> {
        if dir >= dirs || depth > 32 || !seen.insert(dir) {
            return Err("invalid or cyclic NARC directory".into());
        }
        let mut p = le32(fnt, dir * 8).ok_or("truncated NARC directory")?;
        if p < dirs * 8 {
            return Err("NARC directory points into its table".into());
        }
        let mut id = le16(fnt, dir * 8 + 4).ok_or("truncated NARC file id")?;
        loop {
            let tag = *fnt.get(p).ok_or("truncated NARC directory")?;
            p += 1;
            if tag == 0 {
                return Ok(());
            }
            let len = (tag & 127) as usize;
            let name = std::str::from_utf8(fnt.get(p..p + len).ok_or("truncated NARC name")?)
                .map_err(|_| "invalid UTF-8 NARC name")?;
            if len == 0 || name.contains(['/', '\\', ':']) || !crate::modpkg::safe_path(name) {
                return Err(format!("{prefix}{name}: unsafe NARC name"));
            }
            p += len;
            if tag & 128 != 0 {
                let child = le16(fnt, p).ok_or("truncated NARC child id")?;
                p += 2;
                if child < 0xf000 {
                    return Err("invalid NARC child id".into());
                }
                walk(
                    fnt,
                    child - 0xf000,
                    dirs,
                    &format!("{prefix}{name}/"),
                    entries,
                    seen,
                    depth + 1,
                )?;
            } else {
                let entry = entries.get_mut(id).ok_or("NARC name refers outside FAT")?;
                if entry.name.is_some() {
                    return Err("duplicate NARC file id".into());
                }
                entry.name = Some(format!("{prefix}{name}"));
                id += 1;
            }
        }
    }
    for entry in &mut entries {
        entry.name = None;
    }
    walk(
        fnt,
        0,
        dirs,
        "",
        &mut entries,
        &mut std::collections::BTreeSet::new(),
        0,
    )?;
    Ok(entries)
}

/// Names by file id from a BTNF block; `None` for unnamed archives. Same walk as the site (`end` matches its bound).
fn narc_names(b: &[u8], base: usize, end: usize, count: usize) -> Vec<Option<String>> {
    let mut names = vec![None; count];
    let Some(dirs) = le16(b, base + 6) else {
        return names;
    };
    if !(1..=4096).contains(&dirs) || base + dirs * 8 > end {
        return names;
    }
    fn walk(
        b: &[u8],
        base: usize,
        end: usize,
        dir: usize,
        prefix: &str,
        depth: usize,
        names: &mut Vec<Option<String>>,
    ) -> Option<()> {
        if depth > 16 {
            return Some(());
        }
        let e = base + dir * 8;
        let mut p = base + le32(b, e)?;
        let mut id = le16(b, e + 4)?;
        while p < end {
            let len = *b.get(p)? as usize;
            p += 1;
            if len == 0 {
                break;
            }
            let name = String::from_utf8_lossy(b.get(p..p + (len & 0x7f))?).into_owned();
            p += len & 0x7f;
            if len & 0x80 != 0 {
                let sub = le16(b, p)? & 0x0fff;
                p += 2;
                walk(
                    b,
                    base,
                    end,
                    sub,
                    &format!("{prefix}{name}/"),
                    depth + 1,
                    names,
                )?;
            } else {
                if id < names.len() {
                    names[id] = Some(format!("{prefix}{name}"));
                }
                id += 1;
            }
        }
        Some(())
    }
    if walk(b, base, end, 0, "", 0, &mut names).is_none() {
        // names are optional; a damaged name table leaves the rest unnamed
    }
    names
}

/// The same archive with each file's bytes replaced by `files[i]` (header, name table and order kept; data 4-byte
/// aligned with 0xFF padding as the SDK packer does).
pub fn rebuild_narc(original: &[u8], files: &[Vec<u8>]) -> Result<Vec<u8>, String> {
    rebuild_narc_with(original, files, 4, 0xff)
}
fn rebuild_narc_with(
    original: &[u8],
    files: &[Vec<u8>],
    align: usize,
    pad: u8,
) -> Result<Vec<u8>, String> {
    let l = narc_layout(original)?;
    if files.len() != l.count {
        return Err(format!(
            "archive has {} files, got {}",
            l.count,
            files.len()
        ));
    }
    let mut gmif_data = Vec::new();
    let mut table = Vec::with_capacity(l.count * 8);
    for f in files {
        while gmif_data.len() % align != 0 {
            gmif_data.push(pad);
        }
        table.extend_from_slice(&(gmif_data.len() as u32).to_le_bytes());
        gmif_data.extend_from_slice(f);
        table.extend_from_slice(&(gmif_data.len() as u32).to_le_bytes());
    }
    while gmif_data.len() % 4 != 0 {
        gmif_data.push(pad);
    }
    let mut out = original[..l.btaf + 12].to_vec();
    out.extend_from_slice(&table);
    let btaf_size = out.len() - l.btaf;
    out[l.btaf + 4..l.btaf + 8].copy_from_slice(&(btaf_size as u32).to_le_bytes());
    out.extend_from_slice(&original[l.btnf..l.btnf + l.btnf_size]);
    out.extend_from_slice(b"GMIF");
    out.extend_from_slice(&((gmif_data.len() + 8) as u32).to_le_bytes());
    out.extend_from_slice(&gmif_data);
    let total = out.len() as u32;
    out[8..12].copy_from_slice(&total.to_le_bytes());
    Ok(out)
}

// ---------------------------------------------------------------- minimal-change recompression

#[derive(Clone, Copy, PartialEq, Debug)]
struct Tok {
    len: usize,
    /// 0 = literal
    disp: usize,
}

/// A parsed LZ77 type 0x10 stream: tokens with their output positions, the byte offset of each 8-token group's
/// flag byte, the header length and the offset just past the last token.
struct Lz77Stream {
    toks: Vec<(usize, Tok)>,
    groups: Vec<usize>,
    header: usize,
    end: usize,
}

fn lz77_stream(b: &[u8]) -> Option<Lz77Stream> {
    if b.len() < 4 || b[0] != 0x10 {
        return None;
    }
    let mut size = b[1] as usize | (b[2] as usize) << 8 | (b[3] as usize) << 16;
    let mut p = 4;
    if size == 0 {
        size = le32(b, 4)?;
        p = 8;
    }
    let header = p;
    let mut out = 0;
    let (mut toks, mut groups) = (Vec::new(), Vec::new());
    while out < size {
        groups.push(p);
        let flags = *b.get(p)?;
        p += 1;
        for bit in (0..8).rev() {
            if out >= size {
                break;
            }
            if flags & (1 << bit) != 0 {
                let (x, y) = (*b.get(p)?, *b.get(p + 1)?);
                p += 2;
                let t = Tok {
                    len: (x >> 4) as usize + 3,
                    disp: (((x & 0x0f) as usize) << 8 | y as usize) + 1,
                };
                if t.disp > out {
                    return None;
                }
                toks.push((out, t));
                out += t.len;
            } else {
                p += 1;
                toks.push((out, Tok { len: 1, disp: 0 }));
                out += 1;
            }
        }
    }
    // a final match may run past the declared size (the decoder stops there); such streams are not spliced
    (out == size).then_some(Lz77Stream {
        toks,
        groups,
        header,
        end: p,
    })
}

fn lz77_header(n: usize, header: usize) -> Vec<u8> {
    if header == 4 && n > 0 && n < 1 << 24 {
        vec![0x10, n as u8, (n >> 8) as u8, (n >> 16) as u8]
    } else {
        let mut h = vec![0x10, 0, 0, 0];
        h.extend_from_slice(&(n as u32).to_le_bytes());
        h
    }
}

/// Append `toks` (the first one at `data[pos]`) as flag groups of up to 8 tokens.
fn emit_groups(data: &[u8], toks: &[Tok], mut pos: usize, out: &mut Vec<u8>) {
    for group in toks.chunks(8) {
        let flag_at = out.len();
        out.push(0);
        let mut flags = 0u8;
        for (k, t) in group.iter().enumerate() {
            if t.disp == 0 {
                out.push(data[pos]);
            } else {
                flags |= 0x80 >> k;
                let d = t.disp - 1;
                out.push(((t.len - 3) as u8) << 4 | (d >> 8) as u8);
                out.push(d as u8);
            }
            pos += t.len;
        }
        out[flag_at] = flags;
    }
}

/// Does match `t` at position `p` reproduce `data`?
fn valid(data: &[u8], p: usize, t: Tok) -> bool {
    p + t.len <= data.len()
        && t.disp != 0
        && t.disp <= p
        && (0..t.len).all(|i| data[p + i] == data[p + i - t.disp])
}

/// Longest match at `p` (distance 2..=4096, length 3..=`cap`), or a literal.
fn greedy_at(data: &[u8], p: usize, cap: usize) -> Tok {
    let max = 18.min(cap).min(data.len() - p);
    let mut best = Tok { len: 1, disp: 0 };
    if max >= 3 {
        for d in 2..=p.min(0x1000) {
            let mut l = 0;
            while l < max && data[p + l] == data[p + l - d] {
                l += 1;
            }
            if l >= 3 && l > best.len.max(2) {
                best = Tok { len: l, disp: d };
                if l == max {
                    break;
                }
            }
        }
    }
    best
}

/// Above this many changed bytes an edit is a rewrite: [`lz77_compress`] is used instead of splicing.
const SPLICE_LIMIT: usize = 256 * 1024;

/// Compress `new` to LZ77 type 0x10 so that it differs from `original` (an LZ77 stream of similar data, e.g. the
/// retail file) as little as possible, so a binary diff against the original stays small.
///
/// The original stream is a sequence of flag groups (a flag byte + 8 tokens). Every group whose tokens all still
/// decode to the same bytes of `new` is copied byte for byte. Each run of groups in between is re-encoded from `new`,
/// reusing the original tokens that are still valid and greedily encoding the rest, then padded to a whole number of
/// groups by splitting matches (into a shorter match plus literals), so the next kept group starts on a group
/// boundary again. A run that cannot be padded absorbs the following kept group and tries again.
///
/// Falls back to [`lz77_compress`] when `original` is not a well-formed LZ77 stream or the edit is a rewrite. The
/// result always decompresses to exactly `new` (checked here; on a mismatch the fallback is used).
pub fn lz77_recompress(original: &[u8], new: &[u8]) -> Vec<u8> {
    match lz77_splice(original, new) {
        Some(b) if lz77_decompress(&b, new.len()).ok().as_deref() == Some(new) => b,
        _ => lz77_compress(new),
    }
}

fn lz77_splice(original: &[u8], new: &[u8]) -> Option<Vec<u8>> {
    let st = lz77_stream(original)?;
    let old = lz77_decompress(original, usize::MAX).ok()?;
    if new.len() as u64 >= 1 << 32 {
        return None;
    }
    // unchanged prefix `a` and suffix `s`; tokens in between are reused only when the size did not change
    let a = old.iter().zip(new).take_while(|(x, y)| x == y).count();
    let s = old[a..]
        .iter()
        .rev()
        .zip(new[a..].iter().rev())
        .take_while(|(x, y)| x == y)
        .count();
    let delta = new.len() as isize - old.len() as isize;
    if new.len() - a - s > SPLICE_LIMIT {
        return None;
    }
    // position in `new` of each original token that still decodes to the same bytes there
    let keep: Vec<Option<usize>> = st
        .toks
        .iter()
        .map(|&(q, t)| {
            let p = if q + t.len <= a || delta == 0 {
                q
            } else if q >= old.len() - s {
                (q as isize + delta) as usize
            } else {
                return None;
            };
            let ok = if t.disp == 0 {
                new.get(p) == Some(&old[q])
            } else {
                valid(new, p, t)
            };
            ok.then_some(p)
        })
        .collect();
    let ngroups = st.groups.len();
    let span = |g: usize| 8 * g..(8 * g + 8).min(st.toks.len());
    // start (in `new`) of each group that can be copied verbatim: all its tokens kept, still tiling `new`
    let clean: Vec<Option<usize>> = (0..ngroups)
        .map(|g| {
            let r = span(g);
            let start = keep[r.start]?;
            let mut p = start;
            for k in r {
                if keep[k] != Some(p) {
                    return None;
                }
                p += st.toks[k].1.len;
            }
            // the final, partial group can only stay final: more tokens after it would need its unused flag bits
            (span(g).len() == 8 || p == new.len()).then_some(start)
        })
        .collect();
    let group_bytes = |g: usize| st.groups[g]..st.groups.get(g + 1).copied().unwrap_or(st.end);
    let group_end = |g: usize| clean[g].unwrap() + span(g).map(|k| st.toks[k].1.len).sum::<usize>();
    let next_clean =
        |from: usize, p: usize| (from..ngroups).find(|&h| clean[h].is_some_and(|x| x >= p));

    let mut out = lz77_header(new.len(), st.header);
    let (mut p, mut g) = (0, 0);
    while g < ngroups && p < new.len() {
        if clean[g] == Some(p) {
            out.extend_from_slice(&original[group_bytes(g)]);
            p = group_end(g);
            g += 1;
            continue;
        }
        // re-encode new[p..y) up to the next kept group (or the end), padded to whole groups
        let mut stop = next_clean(g, p);
        loop {
            let y = stop.map_or(new.len(), |h| clean[h].unwrap());
            let reusable = (8 * g..stop.map_or(st.toks.len(), |h| 8 * h))
                .filter_map(|k| Some((keep[k]?, st.toks[k].1)));
            let mut run = encode_run(new, p, y, reusable);
            if stop.is_none() || pad(&mut run) {
                emit_groups(
                    new,
                    &run.iter().map(|r| r.1).collect::<Vec<_>>(),
                    p,
                    &mut out,
                );
                p = y;
                g = stop.unwrap_or(ngroups);
                break;
            }
            stop = next_clean(stop.unwrap() + 1, p);
        }
    }
    if p < new.len() {
        // the original stream ran out before the new data did: encode the rest
        let run = encode_run(new, p, new.len(), std::iter::empty());
        emit_groups(
            new,
            &run.iter().map(|r| r.1).collect::<Vec<_>>(),
            p,
            &mut out,
        );
    }
    out.extend_from_slice(&original[st.end..]); // the original's trailing padding
    Some(out)
}

/// Tokens covering exactly `data[p..y)`: the given still-valid original tokens (sorted by position) where they fit,
/// greedy matches elsewhere.
fn encode_run(
    data: &[u8],
    mut p: usize,
    y: usize,
    kept: impl Iterator<Item = (usize, Tok)>,
) -> Vec<(usize, Tok)> {
    let kept: Vec<(usize, Tok)> = kept.filter(|&(q, t)| q >= p && q + t.len <= y).collect();
    let mut run = Vec::new();
    let mut m = 0;
    while p < y {
        while m < kept.len() && kept[m].0 < p {
            m += 1;
        }
        let t = match kept.get(m) {
            Some(&(q, t)) if q == p => t,
            next => greedy_at(data, p, next.map_or(y, |n| n.0) - p),
        };
        run.push((p, t));
        p += t.len;
    }
    run
}

/// Pad `run` to a whole number of 8-token groups by splitting matches: match(L) -> match(L-k) + k literals, or L
/// literals. False when it has too few matches to split.
fn pad(run: &mut Vec<(usize, Tok)>) -> bool {
    let mut need = (8 - run.len() % 8) % 8;
    let lit = Tok { len: 1, disp: 0 };
    let mut i = 0;
    while need > 0 && i < run.len() {
        let (p, t) = run[i];
        let k = if t.disp == 0 {
            0
        } else if need >= t.len - 1 {
            t.len - 1
        } else {
            need.min(t.len - 3)
        };
        if k > 0 {
            let head = if k == t.len - 1 {
                lit
            } else {
                Tok {
                    len: t.len - k,
                    disp: t.disp,
                }
            };
            let tail = (p + t.len - k..p + t.len).map(|q| (q, lit));
            run.splice(i..=i, std::iter::once((p, head)).chain(tail));
            need -= k;
        }
        i += k + 1;
    }
    need == 0
}

/// The same archive with each file replaced by `files[i]`, changing as few bytes as possible: when every file keeps
/// its size the data is spliced in place; otherwise the archive is rebuilt with the original's alignment padding.
pub fn update_narc(original: &[u8], files: &[Vec<u8>]) -> Result<Vec<u8>, String> {
    update_narc_bounded(original, files, usize::MAX)
}

/// Check the rebuilt size before allocating an archive's output.
pub fn update_narc_bounded(
    original: &[u8],
    files: &[Vec<u8>],
    max: usize,
) -> Result<Vec<u8>, String> {
    let entries = parse_narc(original)?;
    if files.len() != entries.len() {
        return Err(format!(
            "archive has {} files, got {}",
            entries.len(),
            files.len()
        ));
    }
    if entries
        .iter()
        .zip(files)
        .all(|(e, f)| e.data.len() == f.len())
    {
        if original.len() > max {
            return Err("rebuilt archive exceeds output limit".into());
        }
        let mut out = original.to_vec();
        for (e, f) in entries.iter().zip(files) {
            let at = e.data.as_ptr() as usize - original.as_ptr() as usize;
            out[at..at + f.len()].copy_from_slice(f);
        }
        return Ok(out);
    }
    let l = narc_layout(original)?;
    let data = l.gmif + 8;
    let pad = entries
        .iter()
        .zip(entries.iter().skip(1))
        .find_map(|(x, y)| {
            let end = x.data.as_ptr() as usize + x.data.len() - original.as_ptr() as usize;
            let next = y.data.as_ptr() as usize - original.as_ptr() as usize;
            (next > end).then(|| original[end])
        })
        .unwrap_or(0xff);
    let aligned4 = entries
        .iter()
        .all(|e| (e.data.as_ptr() as usize - original.as_ptr() as usize - data) % 4 == 0);
    let align = if aligned4 { 4 } else { 1 };
    let mut size = 0usize;
    for file in files {
        size = size
            .checked_add(align - 1)
            .map(|n| n / align * align)
            .and_then(|n| n.checked_add(file.len()))
            .ok_or("archive size overflow")?;
    }
    size = size
        .checked_add(3)
        .map(|n| n / 4 * 4)
        .and_then(|n| n.checked_add(l.gmif + 8))
        .ok_or("archive size overflow")?;
    if size > max {
        return Err("rebuilt archive exceeds output limit".into());
    }
    rebuild_narc_with(original, files, if aligned4 { 4 } else { 1 }, pad)
}

// ---------------------------------------------------------------- the checks

/// Problems with one game file against the allowlist (empty = allowed). `budget` limits decompressed bytes.
pub fn check_game_file(b: &[u8], name: &str, budget: &mut usize, depth: usize) -> Vec<String> {
    let ext = ext_of(name);
    let by_magic = format_by_magic(b);
    let mut problems = Vec::new();
    let Some(fmt) = format_by_ext(&ext) else {
        let shown = if ext.is_empty() {
            "no extension".to_string()
        } else {
            format!(".{ext}")
        };
        let looks = by_magic
            .map(|m| format!(" (content looks like .{})", m.ext))
            .unwrap_or_default();
        return vec![format!("{shown} is not an allowed file type{looks}")];
    };
    let mut narc: Option<std::borrow::Cow<[u8]>> = None;
    if fmt.ext == "carc" {
        match lz77_decompress(b, *budget) {
            Ok(out) => {
                *budget -= out.len();
                if out.starts_with(b"NARC") {
                    narc = Some(out.into());
                } else {
                    let what = format_by_magic(&out)
                        .map(|m| format!(" (.{})", m.ext))
                        .unwrap_or_default();
                    problems.push(format!(
                        ".carc decompresses to something that is not a NARC{what}"
                    ));
                }
            }
            Err(e) => problems.push(format!(".carc is not LZ77-compressed NARC data ({e})")),
        }
    } else if fmt.ext == "kcl" {
        if let Some(m) = by_magic {
            problems.push(format!(
                ".kcl file actually starts with {} (a .{})",
                String::from_utf8_lossy(m.magic.unwrap()),
                m.ext
            ));
        } else if let Some(k) = kcl_header_problem(b) {
            problems.push(k);
        }
    } else if !b.starts_with(fmt.magic.unwrap()) {
        let magic = String::from_utf8_lossy(fmt.magic.unwrap());
        let other = by_magic
            .map(|m| {
                format!(
                    ", but this is a .{} ({})",
                    m.ext,
                    String::from_utf8_lossy(m.magic.unwrap())
                )
            })
            .unwrap_or_default();
        problems.push(format!(".{} must start with \"{magic}\"{other}", fmt.ext));
    } else if fmt.ext == "narc" {
        narc = Some(b.into());
    }
    if let Some(bytes) = narc {
        if depth >= MAX_DEPTH {
            problems.push("archives nested too deeply".into());
        } else {
            match parse_narc(&bytes) {
                Ok(entries) => {
                    for (i, e) in entries.iter().enumerate() {
                        let inner =
                            e.name
                                .clone()
                                .unwrap_or_else(|| match format_by_magic(e.data) {
                                    Some(m) => format!("file{i}.{}", m.ext),
                                    None => format!("file{i}"),
                                });
                        let shown = e.name.clone().unwrap_or_else(|| format!("#{i}"));
                        for p in check_game_file(e.data, &inner, budget, depth + 1) {
                            problems.push(format!("inside: {shown}: {p}"));
                        }
                    }
                }
                Err(e) => problems.push(format!("archive structure is damaged ({e})")),
            }
        }
    }
    problems
}

/// README.md / README.txt at the package root.
pub fn is_readme(path: &str) -> bool {
    !path.contains('/') && README_NAMES.contains(&path.to_ascii_lowercase().as_str())
}
pub fn is_preview_path(path: &str) -> bool {
    PREVIEW_EXTENSIONS.contains(&ext_of(path).as_str())
}
/// Preview images: extension and magic agree, at most 5 MB.
pub fn preview_problem(b: &[u8], path: &str) -> Option<String> {
    let ext = ext_of(path);
    let ok = match ext.as_str() {
        "png" => b.starts_with(b"\x89PNG\r\n\x1a\n"),
        "webp" => b.starts_with(b"RIFF") && b.get(8..12) == Some(b"WEBP"),
        "jpg" | "jpeg" => b.starts_with(&[0xff, 0xd8, 0xff]),
        _ => return None,
    };
    if !ok {
        let kind = if ext == "jpg" || ext == "jpeg" {
            "JPEG".into()
        } else {
            ext.to_uppercase()
        };
        return Some(format!(".{ext} preview does not contain {kind} data"));
    }
    (b.len() > MAX_PREVIEW_BYTES).then(|| "preview image is larger than 5 MB".into())
}
/// An archive or compressed container other than NARC/CARC (by magic or extension), as the site's `detectArchive`.
pub fn detect_archive(b: &[u8], name: &str) -> Option<String> {
    let be32 = |o: usize| {
        b.get(o..o + 4)
            .map(|x| u32::from_be_bytes(x.try_into().unwrap()))
    };
    let at = |o: usize, s: &[u8]| b.get(o..o + s.len()) == Some(s);
    let found = if at(0, b"PK\x03\x04") || at(0, b"PK\x05\x06") || at(0, b"PK\x07\x08") {
        "ZIP archive"
    } else if at(0, b"7z\xbc\xaf\x27\x1c") {
        "7-Zip archive"
    } else if at(0, b"Rar!\x1a\x07") {
        "RAR archive"
    } else if be32(0) == Some(0x04224d18) || be32(0) == Some(0x02214c18) {
        "LZ4 archive"
    } else if at(0, &[0x1f, 0x8b]) {
        "gzip archive"
    } else if at(0, b"BZh") && b.get(3).is_some_and(|c| (0x31..=0x39).contains(c)) {
        "bzip2 archive"
    } else if at(0, b"\xfd7zXZ\x00") {
        "xz archive"
    } else if be32(0) == Some(0x28b52ffd) {
        "Zstandard archive"
    } else if at(257, b"ustar") {
        "tar archive"
    } else if at(0, b"MSCF") {
        "CAB archive"
    } else if at(2, b"-lh") {
        "LZH archive"
    } else if at(0, &[0x60, 0xea]) {
        "ARJ archive"
    } else {
        let ext = ext_of(name);
        let archive_ext = "zip 7z rar gz tgz bz2 xz lz lz4 lzma zst tar cab lzh arj"
            .split(' ')
            .any(|x| x == ext);
        return (name.contains('.') && archive_ext).then(|| format!(".{ext} archive"));
    };
    Some(found.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn narc(files: &[(&str, &[u8])]) -> Vec<u8> {
        // minimal named NARC: one root directory
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
        let files: Vec<Vec<u8>> = files.iter().map(|(_, d)| d.to_vec()).collect();
        rebuild_narc(&out, &files).unwrap()
    }

    #[test]
    fn lz77_round_trips_and_rejects_bad_data() {
        for data in [
            b"".to_vec(),
            b"abcabcabcabcabcabcabcabc".to_vec(),
            (0..70000u32).map(|i| (i * 7 / 13) as u8).collect(),
            vec![0x42; 9000],
        ] {
            let c = lz77_compress(&data);
            assert_eq!(lz77_decompress(&c, 1 << 20).unwrap(), data);
        }
        assert!(lz77_decompress(b"\x11\x04\0\0abcd", 100).is_err());
        assert!(lz77_decompress(b"\x10\x10\0\0\x80\x00\x05", 100).is_err());
        assert!(lz77_decompress(&lz77_compress(&[1; 100]), 50).is_err());
    }

    /// Compressible synthetic data (no game content).
    fn sample(entries: u32) -> Vec<u8> {
        let mut data = Vec::new();
        for i in 0..entries {
            data.extend_from_slice(
                format!("entry {} value {}; ", i % 300, i * 7 % 1000).as_bytes(),
            );
        }
        data
    }
    fn noise(n: usize, seed: u32) -> Vec<u8> {
        let mut x = seed.wrapping_mul(2654435761).wrapping_add(12345);
        (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                x as u8
            })
            .collect()
    }
    /// An LZ77 stream from a different encoder than ours (token boundaries differ from [`lz77_compress`]), standing
    /// in for a retail file.
    fn foreign_lz77(data: &[u8]) -> Vec<u8> {
        let (mut toks, mut p) = (Vec::new(), 0);
        while p < data.len() {
            let t = greedy_at(data, p, 4 + p % 13);
            toks.push(t);
            p += t.len;
        }
        let mut out = lz77_header(data.len(), 4);
        emit_groups(data, &toks, 0, &mut out);
        out.extend_from_slice(&[0, 0]); // trailing padding, kept by recompression
        assert_eq!(lz77_decompress(&out, usize::MAX).unwrap(), data);
        out
    }
    #[cfg(feature = "diff")]
    fn patch_len(a: &[u8], b: &[u8]) -> usize {
        let mut patch = Vec::new();
        crate::modpkg::qbsdiff::Bsdiff::new(a, b)
            .compare(&mut patch)
            .unwrap();
        patch.len()
    }

    #[test]
    #[cfg(feature = "diff")]
    fn recompression_keeps_the_original_stream_outside_the_edit() {
        let data = sample(20000);
        for original in [lz77_compress(&data), foreign_lz77(&data)] {
            assert_eq!(lz77_recompress(&original, &data), original);
            for (at, insert) in [
                (5000usize, 0usize),
                (100000, 0),
                (60000, 5),
                (data.len() - 3, 0),
            ] {
                let mut edited = data.clone();
                edited[at] ^= 0x20;
                for _ in 0..insert {
                    edited.insert(at, b'#');
                }
                let new = lz77_recompress(&original, &edited);
                assert_eq!(lz77_decompress(&new, 1 << 24).unwrap(), edited);
                let patch = patch_len(&original, &new);
                assert!(
                    patch < 1000,
                    "edit at {at} (+{insert}): patch {patch} bytes for {}",
                    original.len()
                );
            }
        }
        let original = lz77_compress(&data);
        let other: Vec<u8> = (0..5000u32).map(|i| (i * i % 251) as u8).collect();
        assert_eq!(
            lz77_decompress(&lz77_recompress(&original, &other), 1 << 20).unwrap(),
            other
        );
        assert_eq!(
            lz77_decompress(&lz77_recompress(b"not lz", &other), 1 << 20).unwrap(),
            other
        );
        assert_eq!(
            lz77_decompress(&lz77_recompress(&original, b""), 16).unwrap(),
            b""
        );
    }

    #[test]
    #[cfg(feature = "diff")]
    fn recompression_splices_incompressible_edits_at_start_middle_and_end() {
        // a 1 KiB block of noise (like a recoloured palette) re-encodes to literals only, which cannot be split to
        // realign the flag groups: the run has to absorb following groups instead of shifting the rest of the stream
        let data = sample(4000);
        let original = foreign_lz77(&data);
        for (at, n) in [
            (0usize, 1024usize),
            (0, 1),
            (data.len() / 2, 1024),
            (data.len() / 2 + 3, 37),
            (data.len() - 1024, 1024),
            (data.len() - 1, 1),
        ] {
            let mut edited = data.clone();
            edited[at..at + n].copy_from_slice(&noise(n, at as u32 + 1));
            let new = lz77_recompress(&original, &edited);
            assert_eq!(
                lz77_decompress(&new, 1 << 24).unwrap(),
                edited,
                "edit at {at}+{n}"
            );
            let ours = patch_len(&original, &new);
            let full = patch_len(&original, &lz77_compress(&edited));
            // the noise itself is incompressible: about 2 bytes of patch per new byte, plus bsdiff overhead
            assert!(ours < 2 * n + 600, "edit at {at}+{n}: patch {ours} bytes");
            assert!(
                ours * 4 < full,
                "edit at {at}+{n}: patch {ours} bytes vs {full} for full recompression"
            );
        }
    }

    #[test]
    #[cfg(feature = "diff")]
    fn recompression_handles_size_changes_and_scattered_edits() {
        let data = sample(4000);
        let original = foreign_lz77(&data);
        let full = |e: &[u8]| patch_len(&original, &lz77_compress(e));
        // grow, shrink, and replace with different-length content
        let mid = data.len() / 2;
        let mut cases: Vec<Vec<u8>> = Vec::new();
        cases.push([&data[..mid], &noise(300, 9)[..], &data[mid..]].concat());
        cases.push([&data[..mid], &data[mid + 777..]].concat());
        cases.push([&data[..10], &noise(50, 3)[..], &data[200..]].concat());
        cases.push([&data[..], b"appended tail"].concat());
        cases.push(data[..data.len() - 500].to_vec());
        for edited in &cases {
            let new = lz77_recompress(&original, edited);
            assert_eq!(lz77_decompress(&new, 1 << 24).unwrap(), *edited);
            let ours = patch_len(&original, &new);
            assert!(
                ours < 1500 && ours * 5 < full(edited),
                "size {} -> {}: patch {ours} bytes",
                data.len(),
                edited.len()
            );
        }
        // many small same-size edits scattered through the file (tokens between them are reused)
        let mut edited = data.clone();
        for (i, b) in noise(40, 7).into_iter().enumerate() {
            edited[i * 2311 % data.len()] = b;
        }
        let new = lz77_recompress(&original, &edited);
        assert_eq!(lz77_decompress(&new, 1 << 24).unwrap(), edited);
        assert!(patch_len(&original, &new) * 4 < full(&edited));
    }

    #[test]
    fn recompression_round_trips_random_edits() {
        let data = sample(1500);
        for (i, original) in [
            lz77_compress(&data),
            foreign_lz77(&data),
            foreign_lz77(&noise(3000, 1)),
        ]
        .into_iter()
        .enumerate()
        {
            let base = lz77_decompress(&original, usize::MAX).unwrap();
            for seed in 0..40u32 {
                let r = noise(4, seed * 31 + i as u32);
                let at = (r[0] as usize * 256 + r[1] as usize) % base.len();
                let cut = (r[2] as usize % 64).min(base.len() - at);
                let ins = noise(r[3] as usize % 64, seed);
                let edited = [&base[..at], &ins[..], &base[at + cut..]].concat();
                let new = lz77_recompress(&original, &edited);
                assert_eq!(
                    lz77_decompress(&new, 1 << 24).unwrap(),
                    edited,
                    "stream {i} seed {seed}"
                );
            }
        }
    }

    #[test]
    fn narc_updates_splice_same_size_files() {
        let a = narc(&[("one.bmg", b"MESGbmg1 hello"), ("two.nclr", b"RLCNxx")]);
        let same = update_narc(&a, &[b"MESGbmg1 HELLO".to_vec(), b"RLCNxx".to_vec()]).unwrap();
        assert_eq!(same.len(), a.len());
        assert_eq!(same.iter().zip(&a).filter(|(x, y)| x != y).count(), 5);
        let grown =
            update_narc(&a, &[b"MESGbmg1 hello world".to_vec(), b"RLCNxx".to_vec()]).unwrap();
        assert_eq!(parse_narc(&grown).unwrap()[0].data, b"MESGbmg1 hello world");
        assert_eq!(parse_narc(&grown).unwrap()[1].data, b"RLCNxx");
    }

    #[test]
    fn narc_parse_and_rebuild_keep_names_and_order() {
        let a = narc(&[("one.bmg", b"MESGbmg1 hello"), ("two.nclr", b"RLCNxx")]);
        let entries = parse_narc(&a).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name.as_deref(), Some("one.bmg"));
        assert_eq!(entries[1].data, b"RLCNxx");
        let b = rebuild_narc(&a, &[b"MESGbmg1 changed!".to_vec(), b"RLCNxx".to_vec()]).unwrap();
        let entries = parse_narc(&b).unwrap();
        assert_eq!(entries[0].data, b"MESGbmg1 changed!");
        assert_eq!(entries[1].name.as_deref(), Some("two.nclr"));
        assert_eq!(
            rebuild_narc(&a, &[b"MESGbmg1 hello".to_vec(), b"RLCNxx".to_vec()]).unwrap(),
            a
        );
    }

    #[test]
    fn allowlist_matches_site_rules() {
        let mut budget = 1 << 24;
        assert!(check_game_file(b"MESGbmg1....", "x/t.bmg", &mut budget, 0).is_empty());
        assert!(check_game_file(b"RLCN", "a.NCLR", &mut budget, 0).is_empty());
        assert!(
            check_game_file(b"MESGbmg1", "a.txt", &mut budget, 0)[0]
                .contains("not an allowed file type (content looks like .bmg)")
        );
        assert!(
            check_game_file(b"RLCN", "a.bmg", &mut budget, 0)[0].contains("but this is a .nclr")
        );
        assert!(!check_game_file(b"\0\0\0\0", "noext", &mut budget, 0).is_empty());
        let inner = narc(&[("ok.bmg", b"MESGbmg1"), ("bad.exe", b"MZ")]);
        let p = check_game_file(&inner, "a.narc", &mut budget, 0);
        assert_eq!(p.len(), 1);
        assert!(p[0].starts_with("inside: bad.exe:"));
        let good = narc(&[("ok.bmg", b"MESGbmg1")]);
        assert!(check_game_file(&lz77_compress(&good), "a.carc", &mut budget, 0).is_empty());
        assert!(!check_game_file(&good, "a.carc", &mut budget, 0).is_empty());
        assert!(!check_game_file(&lz77_compress(b"MESGbmg1"), "a.carc", &mut budget, 0).is_empty());
        let mut kcl = vec![0u8; 0x80];
        for (i, v) in [0x3cu32, 0x48, 0x50, 0x70].iter().enumerate() {
            kcl[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
        }
        assert!(check_game_file(&kcl, "c.kcl", &mut budget, 0).is_empty());
        kcl[0] = 0x10;
        assert!(!check_game_file(&kcl, "c.kcl", &mut budget, 0).is_empty());
    }

    #[test]
    fn metadata_rules() {
        assert!(
            is_readme("README.md")
                && is_readme("readme.TXT")
                && !is_readme("docs/README.md")
                && !is_readme("notes.md")
        );
        assert_eq!(
            preview_problem(b"\x89PNG\r\n\x1a\nxxxx", "shots/a.png"),
            None
        );
        assert!(preview_problem(b"GIF89a", "a.png").is_some());
        assert!(preview_problem(&[0xff, 0xd8, 0xff, 0], "a.jpeg").is_none());
        assert!(detect_archive(b"PK\x03\x04", "README.md").is_some());
        assert!(detect_archive(b"hello", "x.7z").is_some());
        assert!(detect_archive(b"hello", "README.md").is_none());
    }
}
