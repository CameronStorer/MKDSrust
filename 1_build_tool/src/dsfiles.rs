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
    DsFormat { ext, magic: Some(magic), group }
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
    DsFormat { ext: "kcl", magic: None, group: "Course" },
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
    DS_FORMATS.iter().find(|f| f.ext != "carc" && f.magic.is_some_and(|m| b.starts_with(m)))
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
                let (x, y) = (*b.get(p).ok_or("LZ77 data ends early")?, *b.get(p + 1).ok_or("LZ77 data ends early")?);
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
    let key = |i: usize| ((data[i] as usize) << 8 ^ (data[i + 1] as usize) << 4 ^ data[i + 2] as usize) & 0xffff;
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
    let btnf = btaf.checked_add(le32(b, btaf + 4).ok_or("bad BTAF")?).ok_or("bad BTAF")?;
    if b.get(btnf..btnf + 4) != Some(b"BTNF") {
        return Err("NARC name table (BTNF) missing".into());
    }
    let btnf_size = le32(b, btnf + 4).ok_or("bad BTNF")?;
    let gmif = btnf.checked_add(btnf_size).ok_or("bad BTNF")?;
    if b.get(gmif..gmif + 4) != Some(b"GMIF") {
        return Err("NARC data block (GMIF) missing".into());
    }
    Ok(NarcLayout { btaf, count, btnf, btnf_size, gmif })
}

/// Parse a NARC (BTAF allocation table, BTNF names, GMIF data).
pub fn parse_narc(b: &[u8]) -> Result<Vec<NarcEntry<'_>>, String> {
    let l = narc_layout(b)?;
    let data = l.gmif + 8;
    let data_end = b.len().min(l.gmif + le32(b, l.gmif + 4).ok_or("bad GMIF")?);
    let names = narc_names(b, l.btnf + 8, (l.btnf + l.btnf_size).saturating_sub(8).max(l.btnf + 8), l.count);
    let mut out = Vec::with_capacity(l.count);
    for i in 0..l.count {
        let start = le32(b, l.btaf + 12 + i * 8).unwrap();
        let end = le32(b, l.btaf + 16 + i * 8).unwrap();
        if end < start || data + end > data_end {
            return Err(format!("NARC entry {i} points outside the data block"));
        }
        out.push(NarcEntry { name: names[i].clone(), data: &b[data + start..data + end] });
    }
    Ok(out)
}

/// Names by file id from a BTNF block; `None` for unnamed archives. Same walk as the site (`end` matches its bound).
fn narc_names(b: &[u8], base: usize, end: usize, count: usize) -> Vec<Option<String>> {
    let mut names = vec![None; count];
    let Some(dirs) = le16(b, base + 6) else { return names };
    if !(1..=4096).contains(&dirs) || base + dirs * 8 > end {
        return names;
    }
    fn walk(b: &[u8], base: usize, end: usize, dir: usize, prefix: &str, depth: usize, names: &mut Vec<Option<String>>) -> Option<()> {
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
                walk(b, base, end, sub, &format!("{prefix}{name}/"), depth + 1, names)?;
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
    let l = narc_layout(original)?;
    if files.len() != l.count {
        return Err(format!("archive has {} files, got {}", l.count, files.len()));
    }
    let mut gmif_data = Vec::new();
    let mut table = Vec::with_capacity(l.count * 8);
    for f in files {
        while gmif_data.len() % 4 != 0 {
            gmif_data.push(0xff);
        }
        table.extend_from_slice(&(gmif_data.len() as u32).to_le_bytes());
        gmif_data.extend_from_slice(f);
        table.extend_from_slice(&(gmif_data.len() as u32).to_le_bytes());
    }
    while gmif_data.len() % 4 != 0 {
        gmif_data.push(0xff);
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

// ---------------------------------------------------------------- the checks

/// Problems with one game file against the allowlist (empty = allowed). `budget` limits decompressed bytes.
pub fn check_game_file(b: &[u8], name: &str, budget: &mut usize, depth: usize) -> Vec<String> {
    let ext = ext_of(name);
    let by_magic = format_by_magic(b);
    let mut problems = Vec::new();
    let Some(fmt) = format_by_ext(&ext) else {
        let shown = if ext.is_empty() { "no extension".to_string() } else { format!(".{ext}") };
        let looks = by_magic.map(|m| format!(" (content looks like .{})", m.ext)).unwrap_or_default();
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
                    let what = format_by_magic(&out).map(|m| format!(" (.{})", m.ext)).unwrap_or_default();
                    problems.push(format!(".carc decompresses to something that is not a NARC{what}"));
                }
            }
            Err(e) => problems.push(format!(".carc is not LZ77-compressed NARC data ({e})")),
        }
    } else if fmt.ext == "kcl" {
        if let Some(m) = by_magic {
            problems.push(format!(".kcl file actually starts with {} (a .{})", String::from_utf8_lossy(m.magic.unwrap()), m.ext));
        } else if let Some(k) = kcl_header_problem(b) {
            problems.push(k);
        }
    } else if !b.starts_with(fmt.magic.unwrap()) {
        let magic = String::from_utf8_lossy(fmt.magic.unwrap());
        let other = by_magic.map(|m| format!(", but this is a .{} ({})", m.ext, String::from_utf8_lossy(m.magic.unwrap()))).unwrap_or_default();
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
                        let inner = e.name.clone().unwrap_or_else(|| match format_by_magic(e.data) {
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
        let kind = if ext == "jpg" || ext == "jpeg" { "JPEG".into() } else { ext.to_uppercase() };
        return Some(format!(".{ext} preview does not contain {kind} data"));
    }
    (b.len() > MAX_PREVIEW_BYTES).then(|| "preview image is larger than 5 MB".into())
}
/// An archive or compressed container other than NARC/CARC (by magic or extension), as the site's `detectArchive`.
pub fn detect_archive(b: &[u8], name: &str) -> Option<String> {
    let be32 = |o: usize| b.get(o..o + 4).map(|x| u32::from_be_bytes(x.try_into().unwrap()));
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
        let archive_ext = "zip 7z rar gz tgz bz2 xz lz lz4 lzma zst tar cab lzh arj".split(' ').any(|x| x == ext);
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
        for data in [b"".to_vec(), b"abcabcabcabcabcabcabcabc".to_vec(), (0..70000u32).map(|i| (i * 7 / 13) as u8).collect(), vec![0x42; 9000]] {
            let c = lz77_compress(&data);
            assert_eq!(lz77_decompress(&c, 1 << 20).unwrap(), data);
        }
        assert!(lz77_decompress(b"\x11\x04\0\0abcd", 100).is_err());
        assert!(lz77_decompress(b"\x10\x10\0\0\x80\x00\x05", 100).is_err());
        assert!(lz77_decompress(&lz77_compress(&[1; 100]), 50).is_err());
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
        assert_eq!(rebuild_narc(&a, &[b"MESGbmg1 hello".to_vec(), b"RLCNxx".to_vec()]).unwrap(), a);
    }

    #[test]
    fn allowlist_matches_site_rules() {
        let mut budget = 1 << 24;
        assert!(check_game_file(b"MESGbmg1....", "x/t.bmg", &mut budget, 0).is_empty());
        assert!(check_game_file(b"RLCN", "a.NCLR", &mut budget, 0).is_empty());
        assert!(check_game_file(b"MESGbmg1", "a.txt", &mut budget, 0)[0].contains("not an allowed file type (content looks like .bmg)"));
        assert!(check_game_file(b"RLCN", "a.bmg", &mut budget, 0)[0].contains("but this is a .nclr"));
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
        assert!(is_readme("README.md") && is_readme("readme.TXT") && !is_readme("docs/README.md") && !is_readme("notes.md"));
        assert_eq!(preview_problem(b"\x89PNG\r\n\x1a\nxxxx", "shots/a.png"), None);
        assert!(preview_problem(b"GIF89a", "a.png").is_some());
        assert!(preview_problem(&[0xff, 0xd8, 0xff, 0], "a.jpeg").is_none());
        assert!(detect_archive(b"PK\x03\x04", "README.md").is_some());
        assert!(detect_archive(b"hello", "x.7z").is_some());
        assert!(detect_archive(b"hello", "README.md").is_none());
    }
}
