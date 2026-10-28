//! Production generator: user's ROM + address-only function metadata -> a Cargo workspace of
//! lifted Rust (`part_NN` crates + a `lifted` crate holding `lifted::LIFTED`), with a complete
//! account of every metadata entry. Contains no game data; everything comes from the supplied ROM.
//!
//! ```text
//! mkds_gen --rom <game.nds> --metadata dictionary/FUNCTION_BOUNDARIES.csv --out <dir>
//!          [--recomp <path to recomp crate>] [--lockfile <Cargo.lock>] [--part-bytes N]
//!          [--no-game] [--allow-incomplete]
//! ```
//!
//! Exit status: 0 = every entry was lifted (undecodable sites left to the interpreter are
//! counted in the report); 2 = output written but some entries were rejected or absent (a
//! blocker unless `--allow-incomplete`); 1 = bad arguments/input, nothing written.
//! Results: `<out>/generation.json` (machine readable) and `<out>/coverage.csv` (one row per
//! metadata entry). `generation.json` is written last, so its presence marks a finished output.
#![allow(dead_code)]
#[path = "../arm.rs"]
mod arm;
#[path = "../../build_support.rs"]
mod build_support;
#[path = "../ir.rs"]
mod ir;
#[path = "../lift.rs"]
mod lift;
#[path = "../raise.rs"]
mod raise;
#[path = "../raise_emit.rs"]
mod raise_emit;
#[path = "../rt.rs"]
mod rt;
#[path = "../signature.rs"]
mod signature;
#[path = "../thumb.rs"]
mod thumb;

use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const MAX_FUNCTION_INSTRUCTIONS: usize = 20_000;
const DEFAULT_PART_BYTES: usize = 3_000_000;
const MARKER: &str = "generation.json";
const OWNED: &str = ".mkds_gen_output";

struct Args {
    rom: PathBuf,
    metadata: PathBuf,
    out: PathBuf,
    recomp: PathBuf,
    lockfile: Option<PathBuf>,
    part_bytes: usize,
    game: bool,
    allow_incomplete: bool,
    /// Optional function names/meanings (the project's own analysis: the function dictionary
    /// `dictionary/*.csv`, or the older `name,segment,ea,...,meaning` table), written as comments above
    /// generated functions; ids and code are unchanged. Repeatable: later tables override earlier ones.
    names: Vec<PathBuf>,
    /// Write the argument / return analysis (`recomp::signature`) of every ARM9 function here (CSV), for
    /// the function dictionary.
    signatures: Option<PathBuf>,
}

fn parse_args() -> Result<Args, String> {
    let (mut rom, mut metadata, mut out, mut lockfile, mut names, mut signatures) = (None, None, None, None, Vec::new(), None);
    let mut recomp = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let (mut part_bytes, mut game, mut allow_incomplete) = (DEFAULT_PART_BYTES, true, false);
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        let mut value = |name: &str| it.next().ok_or(format!("{name} needs a value"));
        match a.as_str() {
            "--rom" => rom = Some(PathBuf::from(value("--rom")?)),
            "--metadata" => metadata = Some(PathBuf::from(value("--metadata")?)),
            "--out" => out = Some(PathBuf::from(value("--out")?)),
            "--recomp" => recomp = PathBuf::from(value("--recomp")?),
            "--lockfile" => lockfile = Some(PathBuf::from(value("--lockfile")?)),
            "--names" => names.push(PathBuf::from(value("--names")?)),
            "--signatures" => signatures = Some(PathBuf::from(value("--signatures")?)),
            "--part-bytes" => {
                part_bytes = value("--part-bytes")?.parse().map_err(|_| "--part-bytes must be a number")?;
                if part_bytes < 10_000 {
                    return Err("--part-bytes is too small".into());
                }
            }
            "--no-game" => game = false,
            "--allow-incomplete" => allow_incomplete = true,
            // Block 5.4: `native` emits ARM9 code without the per-block budget (see `recomp::lift::timing_native`)
            "--timing" => match value("--timing")?.as_str() {
                "native" => lift::set_timing_native(true, recomp::os_hle::SAFE_LOOPS),
                "exact" => lift::set_timing_native(false, &[]),
                t => return Err(format!("--timing must be exact or native, not {t}")),
            },
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(Args {
        rom: rom.ok_or("--rom is required (no implicit ROM is used)")?,
        metadata: metadata.ok_or("--metadata is required (no implicit metadata is used)")?,
        out: out.ok_or("--out is required")?,
        recomp,
        lockfile,
        part_bytes,
        game,
        allow_incomplete,
        names,
        signatures,
    })
}

/// Records of a CSV text (quoted fields may hold commas, doubled quotes and newlines).
fn csv_records(text: &str) -> Vec<Vec<String>> {
    let (mut rows, mut row, mut field) = (Vec::new(), Vec::new(), String::new());
    let mut quoted = false;
    let mut chars = text.trim_start_matches('\u{feff}').chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if quoted && chars.peek() == Some(&'"') => {
                field.push('"');
                chars.next();
            }
            '"' => quoted = !quoted,
            ',' if !quoted => row.push(std::mem::take(&mut field)),
            '\n' if !quoted => {
                row.push(std::mem::take(&mut field));
                rows.push(std::mem::take(&mut row));
            }
            '\r' if !quoted => {}
            c => field.push(c),
        }
    }
    if !field.is_empty() || !row.is_empty() {
        row.push(field);
        rows.push(row);
    }
    rows
}

/// One function's dictionary facts, merged over the names tables.
#[derive(Default)]
struct Entry {
    name: String,
    args: String,
    returns: String,
    notes: Vec<String>,
}

/// Merges the names table `text` into `out` by `(segment, entry without the Thumb bit)`. Columns: `name`,
/// `segment`, `ea` or `address`, and (optional) `meaning` or `notes`, `args`, `returns` (the function
/// dictionary's format). A later table overrides only what it fills in: a name or signature replaces the
/// earlier one, its notes are added after the earlier ones.
fn merge_names(text: &str, out: &mut HashMap<(String, u32), Entry>) -> Result<(), String> {
    let rows = csv_records(text);
    let head = rows.first().ok_or("names table is empty")?;
    let col = |n: &str| head.iter().position(|h| h.trim() == n);
    let (Some(ci), Some(cs), Some(ce)) = (col("name"), col("segment"), col("ea").or(col("address"))) else {
        return Err("names table needs the columns name, segment, ea (or address)".into());
    };
    let cm = col("meaning").or(col("notes"));
    let (ca, cr) = (col("args"), col("returns"));
    for r in rows.iter().skip(1) {
        let get = |i: Option<usize>| i.and_then(|i| r.get(i)).map(|v| v.trim()).unwrap_or("");
        let Ok(ea) = u32::from_str_radix(get(Some(ce)).trim_start_matches("0x").trim_start_matches("0X"), 16) else { continue };
        let e = out.entry((get(Some(cs)).to_string(), ea & !1)).or_default();
        let name = get(Some(ci));
        if !name.is_empty() && !name.eq_ignore_ascii_case(&format!("sub_{:X}", ea & !1)) {
            e.name = name.to_string();
        }
        if !get(ca).is_empty() || !get(cr).is_empty() {
            (e.args, e.returns) = (get(ca).to_string(), get(cr).to_string());
        }
        let notes: String = get(cm).split_whitespace().collect::<Vec<_>>().join(" ");
        if !notes.is_empty() && !e.notes.contains(&notes) {
            e.notes.push(notes);
        }
    }
    Ok(())
}

/// The comment line for one function, or `None` when nothing is known beyond its address.
fn comment_line(ea: u32, e: &Entry) -> Option<String> {
    if e.name.is_empty() && e.notes.is_empty() && e.args.is_empty() && e.returns.is_empty() {
        return None;
    }
    let mut name = if e.name.is_empty() { format!("sub_{ea:X}") } else { e.name.clone() };
    if !e.args.is_empty() || !e.returns.is_empty() {
        name = format!("{name}({})", e.args);
        if !e.returns.is_empty() && e.returns != "void" {
            name = format!("{name} -> {}", e.returns);
        }
    }
    let mut line = if e.notes.is_empty() { name } else { format!("{name}: {}", e.notes.join(" | ")) };
    if line.chars().count() > 300 {
        line = line.chars().take(297).collect::<String>() + "...";
    }
    Some(line)
}

/// `(segment, entry without the Thumb bit)` -> one comment line, from names tables applied in order.
fn function_names<'a>(texts: impl IntoIterator<Item = &'a str>) -> Result<HashMap<(String, u32), String>, String> {
    let mut merged = HashMap::new();
    for t in texts {
        merge_names(t, &mut merged)?;
    }
    Ok(merged.iter().filter_map(|(k, e)| Some((k.clone(), comment_line(k.1, e)?))).collect())
}

/// The dictionary's argument / return analysis over all ARM9 code: listed functions and the gap functions
/// discovery found (they make calls too). A call resolves to the caller's own overlay first, then the fixed
/// image. CSV `address,segment,args,returns,callers` (address without the Thumb bit).
fn write_signatures(
    path: &std::path::Path,
    pending: &[Pending],
    gap_funcs: &BTreeMap<u32, lift::Func>,
    ov_gap_funcs: &BTreeMap<String, BTreeMap<u32, lift::Func>>,
    fixed_seg: &dyn Fn(u32) -> Option<String>,
) -> Result<(), String> {
    let mut funcs: BTreeMap<(String, u32), &lift::Func> = BTreeMap::new();
    for p in pending {
        funcs.insert((p.segment.clone(), p.func.entry & !1), &p.func);
    }
    for (&e, f) in gap_funcs {
        if let Some(seg) = fixed_seg(e) {
            funcs.entry((seg, e & !1)).or_insert(f);
        }
    }
    for (ov, fs) in ov_gap_funcs {
        for (&e, f) in fs {
            funcs.entry((ov.clone(), e & !1)).or_insert(f);
        }
    }
    let resolve = |caller: &(String, u32), t: u32| -> Option<(String, u32)> {
        let own = (caller.0.clone(), t & !1);
        if caller.0.starts_with("ov9_") && funcs.contains_key(&own) {
            return Some(own);
        }
        let fixed = (fixed_seg(t)?, t & !1);
        funcs.contains_key(&fixed).then_some(fixed)
    };
    let sig = signature::analyse(&funcs, &resolve);
    let mut csv = String::from("address,segment,args,returns,callers
");
    for ((seg, entry), s) in &sig {
        let (a, r) = signature::describe(s);
        let _ = writeln!(csv, "{entry:#010X},{seg},\"{a}\",{r},{}", s.callers);
    }
    std::fs::write(path, csv).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    eprintln!("mkds_gen: argument/return analysis of {} functions written to {}", sig.len(), path.display());
    Ok(())
}

/// Block 4B passes that keep exact behaviour (proven by a bit-identical race): on by default;
/// `MKDS_EXACT=1` generates the plain translation.
fn raise() -> bool {
    std::env::var_os("MKDS_EXACT").is_none()
}

fn sha_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

/// Identity of the generator itself: a different decoder/lifter/runtime-interface source gives a
/// different id, so output from an older generator is recognisable.
fn generator_id() -> String {
    let mut h = Sha256::new();
    for src in [
        include_bytes!("../lift.rs").as_slice(),
        include_bytes!("../ir.rs"),
        include_bytes!("../raise.rs"),
        include_bytes!("../raise_emit.rs"),
        include_bytes!("../arm.rs"),
        include_bytes!("../thumb.rs"),
        include_bytes!("../rt.rs"),
        include_bytes!("../../build_support.rs"),
    ] {
        h.update((src.len() as u64).to_le_bytes());
        h.update(src);
    }
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// Writes `text` unless the file already holds it (keeps Cargo's timestamps: no needless rebuilds).
fn put(path: impl AsRef<Path>, text: &str) -> Result<(), String> {
    let path = path.as_ref();
    if std::fs::read_to_string(path).ok().as_deref() != Some(text) {
        std::fs::write(path, text).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    }
    Ok(())
}

fn toml_path(p: &Path) -> String {
    let s = p.to_string_lossy();
    format!("{:?}", s.strip_prefix(r"\\?\").unwrap_or(&s))
}

struct Row {
    status: &'static str,
    segment: String,
    entry: u32,
    thumb: bool,
    size: u32,
    insts: usize,
    blocks: usize,
    undecoded: Vec<(u32, u32)>,
    reason: String,
}

/// A listed ARM9 function between discovery and emission.
struct Pending {
    row: usize,
    segment: String,
    lo: u32,
    hi: u32,
    func: lift::Func,
    /// Block starts other code enters it at (cross-function discovery).
    extra: BTreeSet<u32>,
}

struct Lifted {
    /// ARM7 code (goes to `LIFTED7`) rather than ARM9 (`LIFTED`).
    arm7: bool,
    segment: String,
    entry: u32,
    id: String,
    code: String,
    /// The raised (T1) version, installed at the entry (block 4B), or why there is none.
    t1: Result<String, &'static str>,
    /// The raised version's Rust name (the dictionary name where there is one).
    t1_name: String,
    /// The field accessors the raised version calls (defined in the `layout` crate).
    fields: Vec<(ir::FieldRef, ir::Access)>,
    /// Block 5.5: a closed function's direct variant (goes to the `direct` crate).
    direct: Option<String>,
}

const RUST_KEYWORDS: &[&str] = &[
    "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum", "extern", "false", "fn", "for",
    "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub", "ref", "return", "self", "static", "struct",
    "super", "trait", "true", "type", "unsafe", "use", "where", "while", "abstract", "become", "box", "do", "final", "gen",
    "macro", "override", "priv", "try", "typeof", "unsized", "virtual", "yield",
];

/// Gives every raised function its dictionary name (else `sub_<segment>_<address>`), unique across the
/// generated code: a name used more than once gets the segment and address appended.
fn name_t1(funcs: &mut [Lifted]) {
    let ident = |f: &Lifted| -> Option<String> {
        let n = CALL_NAMES.get()?.get(&(f.segment.clone(), f.entry & !1))?;
        let n: String = n.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' }).collect();
        let ok = n.chars().next().is_some_and(|c| c.is_ascii_alphabetic()) && !RUST_KEYWORDS.contains(&n.as_str()) && !n.starts_with("f_") && !n.starts_with("sub_");
        ok.then_some(n)
    };
    let mut count: HashMap<String, usize> = HashMap::new();
    for f in funcs.iter().filter(|f| f.t1.is_ok()) {
        if let Some(n) = ident(f) {
            *count.entry(n).or_default() += 1;
        }
    }
    for f in funcs.iter_mut() {
        let Ok(code) = &f.t1 else { continue };
        let name = match ident(f) {
            Some(n) if count[&n] == 1 => n,
            Some(n) => format!("{n}_{}", f.id.to_lowercase()),
            None => format!("sub_{}", f.id.to_lowercase()),
        };
        f.t1 = Ok(code.replacen(&format!("pub fn f_{}_t1(", f.id), &format!("pub fn {name}("), 1));
        f.t1_name = name;
    }
}

/// Dictionary names by (segment, address without the Thumb bit), for call comments in T1 code.
static CALL_NAMES: std::sync::OnceLock<HashMap<(String, u32), String>> = std::sync::OnceLock::new();

/// The dictionary name of the function a call from `segment` reaches at `target`: the caller's own
/// segment first (overlays share addresses), then any other segment with a function there.
fn call_name(segment: &str, target: u32) -> Option<String> {
    let names = CALL_NAMES.get()?;
    let a = target & !1;
    names.get(&(segment.to_string(), a)).or_else(|| names.iter().find(|((s, e), _)| *e == a && !s.starts_with("ov")).map(|(_, n)| n)).cloned()
}

/// Research layout for comments in raised code: field names by (struct, offset), the fields each
/// function uses (by entry, without the Thumb bit) and global names by address.
#[derive(Default)]
struct Layout {
    /// (struct, offset) -> (field name, type).
    fields: HashMap<(String, u32), (String, String)>,
    /// Struct -> its Rust module name.
    modules: BTreeMap<String, String>,
    uses: HashMap<u32, Vec<(String, u32)>>,
    globals: HashMap<u32, String>,
    /// Struct -> the struct it extends (its fields come first).
    parent: HashMap<String, String>,
    /// Global address -> the struct the pointer stored there points to.
    points_global: HashMap<u32, String>,
    /// Global address -> the struct stored there itself (`Foo` or the first of `Foo[8]`).
    is_global: HashMap<u32, String>,
    /// (struct, offset) -> the struct the pointer in that field points to.
    points_field: HashMap<(String, u32), String>,
    /// Offset -> the structs with a field there.
    by_offset: HashMap<u32, Vec<String>>,
    /// Struct -> share of its word slots (up to its last field) holding a known field.
    density: HashMap<String, f64>,
}
static LAYOUT: std::sync::OnceLock<Layout> = std::sync::OnceLock::new();

fn hex(s: &str) -> Option<u32> {
    u32::from_str_radix(s.trim().trim_start_matches("0x").trim_start_matches("0X"), 16).ok()
}

/// Reads `fields.csv`, `field_uses.csv` and `globals.csv` from `dir` (any may be missing).
fn load_layout(dir: &Path) -> Layout {
    let mut l = Layout::default();
    let rows = |name: &str| -> Vec<Vec<String>> {
        std::fs::read_to_string(dir.join(name)).map(|t| csv_records(&t).into_iter().skip(1).collect()).unwrap_or_default()
    };
    // structs.csv: aliases (one struct under several research names), parents, global roots
    // (`*0x0217ACF8`) and pointer fields (`*(kart+0x514)`)
    let st_rows = rows("structs.csv");
    let mut canon: HashMap<String, String> = st_rows.iter().filter_map(|r| r.first()).map(|n| (n.clone(), n.clone())).collect();
    for r in &st_rows {
        let (Some(n), Some(aliases)) = (r.first(), r.get(1)) else { continue };
        for a in aliases.split(';').map(str::trim).filter(|a| !a.is_empty()) {
            canon.entry(a.to_string()).or_insert_with(|| n.clone());
        }
    }
    let canon = |s: &str| canon.get(s.trim()).cloned().unwrap_or_else(|| s.trim().to_string());
    let pointee = |ty: &str| -> Option<String> {
        let t = ty.trim().trim_matches('"').trim();
        let inner = t.strip_prefix("ptr<")?.strip_suffix('>')?.trim();
        Some(canon(inner))
    };
    let mut fields_at: Vec<(String, u32, String)> = Vec::new();
    for r in &st_rows {
        let Some(n) = r.first() else { continue };
        if let Some(p) = r.get(2).filter(|p| !p.trim().is_empty()) {
            l.parent.insert(n.clone(), canon(p));
        }
        if let Some(a) = r.get(3).and_then(|root| hex(root.trim().strip_prefix('*')?)) {
            l.points_global.insert(a, n.clone());
        }
        // `*(kart+0x514)`: the struct a field of another struct points to
        if let Some(rec) = r.get(5) {
            if let Some(inner) = rec.trim().strip_prefix("*(").and_then(|x| x.split(')').next()) {
                if let Some((base, off)) = inner.split_once('+') {
                    if let Some(off) = hex(off) {
                        fields_at.push((canon(base), off, n.clone()));
                    }
                }
            }
        }
    }
    let mut taken: HashSet<(String, String)> = HashSet::new();
    for r in rows("fields.csv") {
        if let (Some(st), Some(off), Some(n)) = (r.first(), r.get(1).and_then(|o| hex(o)), r.get(2)) {
            let st = canon(st);
            if l.fields.contains_key(&(st.clone(), off)) {
                continue; // an alias names the same field: the first name stays
            }
            // field names unique per struct, usable as Rust names
            let mut name = rust_ident(&snake_case(n));
            if !taken.insert((st.clone(), name.clone())) {
                name = format!("{name}_{off:x}");
                taken.insert((st.clone(), name.clone()));
            }
            let ty = r.get(3).cloned().unwrap_or_default();
            if let Some(p) = pointee(&ty) {
                l.points_field.insert((st.clone(), off), p);
            }
            l.fields.insert((st, off), (name, ty));
        }
    }
    for (base, off, st) in fields_at {
        l.points_field.entry((base, off)).or_insert(st);
    }
    // pointer seeds only to structs with known fields
    let known: HashSet<String> = l.fields.keys().map(|(st, _)| st.clone()).collect();
    l.points_global.retain(|_, st| known.contains(st));
    l.points_field.retain(|_, st| known.contains(st));
    for (st, off) in l.fields.keys() {
        l.by_offset.entry(*off).or_default().push(st.clone());
    }
    for v in l.by_offset.values_mut() {
        v.sort();
    }
    let mut slots: HashMap<String, (BTreeSet<u32>, u32)> = HashMap::new();
    for (st, off) in l.fields.keys() {
        let e = slots.entry(st.clone()).or_default();
        e.0.insert(off / 4);
        e.1 = e.1.max(off / 4 + 1);
    }
    l.density = slots.into_iter().map(|(st, (used, n))| (st, used.len() as f64 / n.max(1) as f64)).collect();
    let mut used: HashSet<String> = ["std", "core", "alloc", "recomp", "layout", "r", "iw", "m", "c"].iter().map(|s| s.to_string()).collect();
    let structs: BTreeSet<String> = l.fields.keys().map(|(st, _)| st.clone()).collect();
    for st in structs {
        let base = rust_ident(&snake_case(&st));
        let mut m = base.clone();
        let mut k = 2;
        while !used.insert(m.clone()) {
            m = format!("{base}_{k}");
            k += 1;
        }
        l.modules.insert(st, m);
    }
    for r in rows("field_uses.csv") {
        if let (Some(f), Some(st), Some(off)) = (r.first().and_then(|f| hex(f)), r.get(1), r.get(2).and_then(|o| hex(o))) {
            l.uses.entry(f & !1).or_default().push((canon(st), off));
        }
    }
    for r in rows("globals.csv") {
        if let (Some(a), Some(n)) = (r.first().and_then(|a| hex(a)), r.get(1)) {
            if let Some(p) = r.get(2).and_then(|t| pointee(t)) {
                l.points_global.entry(a).or_insert(p);
            } else if let Some(t) = r.get(2) {
                // a struct instance: `Foo` or an array `Foo[8]` (its first element)
                let t = t.trim().trim_matches('"').trim();
                let base = canon(t.split('[').next().unwrap_or(""));
                if !base.is_empty() && known.contains(&base) {
                    l.is_global.insert(a, base);
                }
            }
            l.globals.insert(a, n.clone());
        }
    }
    l
}

/// The field if the function at `entry` is known to access exactly one named field at `off`.
fn field_name(entry: u32, off: u32) -> Option<ir::FieldRef> {
    let l = LAYOUT.get()?;
    let mut hits = l.uses.get(&(entry & !1))?.iter().filter(|(_, o)| *o == off);
    let (st, o) = hits.next()?;
    if hits.next().is_some() {
        return None;
    }
    let (name, ty) = l.fields.get(&(st.clone(), *o))?;
    Some(ir::FieldRef { strukt: st.clone(), module: l.modules.get(st)?.clone(), name: name.clone(), off: *o, primary: primary_access(ty) })
}

/// Fields without a scalar type take the access the code uses most as their plain accessor
/// (`kart::bounce(m, p)` instead of `kart::bounce_u32(m, p)`): the raised text and the recorded
/// uses are renamed to match.
fn untyped_field_widths(funcs: &mut [Lifted]) {
    let mut count: HashMap<(String, u32), BTreeMap<ir::Access, usize>> = HashMap::new();
    for f in funcs.iter() {
        for (r, a) in &f.fields {
            if r.primary.is_none() && !a.is_store() {
                *count.entry((r.strukt.clone(), r.off)).or_default().entry(*a).or_default() += 1;
            }
        }
    }
    // most used load; ties go to the first kind
    let pick: HashMap<(String, u32), ir::Access> = count.into_iter().map(|(k, m)| {
        let best = m.iter().max_by(|x, y| x.1.cmp(y.1).then(y.0.cmp(x.0))).map(|(a, _)| *a).unwrap();
        (k, best)
    }).collect();
    for f in funcs.iter_mut() {
        if f.fields.is_empty() {
            continue;
        }
        let mut renames: BTreeMap<String, String> = BTreeMap::new();
        for (r, a) in f.fields.iter_mut() {
            if r.primary.is_some() {
                continue;
            }
            if let Some(&p) = pick.get(&(r.strukt.clone(), r.off)) {
                let old = format!("{}::{}(", r.module, r.accessor(*a));
                r.primary = Some(p);
                renames.insert(old, format!("{}::{}(", r.module, r.accessor(*a)));
            }
        }
        // longest first, so `x_u32(` is not hit by a shorter name's pattern
        let mut v: Vec<_> = renames.into_iter().filter(|(a, b)| a != b).collect();
        v.sort_by_key(|(a, _)| std::cmp::Reverse(a.len()));
        if let Ok(t) = &mut f.t1 {
            for (a, b) in &v {
                *t = replace_word(t, a, b);
            }
        }
        if let Some(t) = &mut f.direct {
            for (a, b) in &v {
                *t = replace_word(t, a, b);
            }
        }
    }
}

/// `text` with `from` replaced where it starts a word (not preceded by an identifier character).
fn replace_word(text: &str, from: &str, to: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find(from) {
        let prev = if i > 0 { rest[..i].chars().last() } else { out.chars().last() };
        out.push_str(&rest[..i]);
        if prev.is_some_and(|c| c.is_ascii_alphanumeric() || c == '_') {
            out.push_str(from);
        } else {
            out.push_str(to);
        }
        rest = &rest[i + from.len()..];
    }
    out.push_str(rest);
    out
}

/// The structs the function at `entry` is known to use a field of at `off`.
fn structs_at(entry: u32, off: u32) -> Vec<String> {
    let Some(l) = LAYOUT.get() else { return Vec::new() };
    let mut v: Vec<String> = l.uses.get(&(entry & !1)).into_iter().flatten().filter(|(_, o)| *o == off).map(|(st, _)| st.clone()).collect();
    v.sort();
    v.dedup();
    v
}

/// Struct `st`'s field at `off` (or the field of the struct it extends).
fn field_in(st: &str, off: u32) -> Option<ir::FieldRef> {
    let l = LAYOUT.get()?;
    let mut st = st.to_string();
    for _ in 0..8 {
        if let Some((name, ty)) = l.fields.get(&(st.clone(), off)) {
            return Some(ir::FieldRef { strukt: st.clone(), module: l.modules.get(&st)?.clone(), name: name.clone(), off, primary: primary_access(ty) });
        }
        st = l.parent.get(&st)?.clone();
    }
    None
}

/// The access a field type implies: scalars of 1, 2 or 4 bytes (`fx12`/`fx32` are 4-byte fixed
/// point, `fx16` 2-byte); none for vectors, arrays and unknown types.
fn primary_access(ty: &str) -> Option<ir::Access> {
    use ir::Access::*;
    let t = ty.trim().trim_matches('"').trim().to_ascii_lowercase();
    match t.as_str() {
        "u8" | "bool" => Some(U8),
        "s8" | "i8" => Some(I8),
        "u16" => Some(U16),
        "s16" | "i16" | "fx16" => Some(I16),
        "u32" | "s32" | "i32" | "fx12" | "fx32" | "fnptr" => Some(U32),
        _ if t.starts_with("ptr") => Some(U32),
        _ => None,
    }
}

/// `CamelCase` / `mixed_Case` -> `snake_case`.
fn snake_case(s: &str) -> String {
    let mut out = String::new();
    let mut prev_lower = false;
    for ch in s.chars() {
        if ch.is_ascii_uppercase() {
            if prev_lower {
                out.push('_');
            }
            out.push(ch.to_ascii_lowercase());
            prev_lower = false;
        } else if ch.is_ascii_alphanumeric() {
            out.push(ch);
            prev_lower = ch.is_ascii_lowercase() || ch.is_ascii_digit();
        } else {
            if !out.ends_with('_') {
                out.push('_');
            }
            prev_lower = false;
        }
    }
    out.trim_matches('_').to_string()
}

/// `s` as a usable Rust identifier: never empty, never starting with a digit, never a keyword.
fn rust_ident(s: &str) -> String {
    const KW: &[&str] = &["as", "break", "const", "continue", "crate", "else", "enum", "extern", "false", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub", "ref", "return", "self", "static", "struct", "super", "trait", "true", "type", "unsafe", "use", "where", "while", "async", "await", "dyn", "abstract", "become", "box", "do", "final", "macro", "override", "priv", "typeof", "unsized", "virtual", "yield", "try", "gen", "union"];
    let s = if s.is_empty() { "field".to_string() } else { s.to_string() };
    let s = if s.starts_with(|c: char| c.is_ascii_digit()) { format!("f_{s}") } else { s };
    if KW.contains(&s.as_str()) { format!("{s}_") } else { s }
}

/// The `layout` crate: every research struct as a module with its field offsets (`pub const`) and
/// the accessors the raised code calls, each the exact helper call it replaces.
fn layout_crate(used: &BTreeSet<(ir::FieldRef, ir::Access)>) -> String {
    let mut s = String::from(
        "//! Struct layouts from our research: field offsets, and the accessors the raised code uses\n\
         //! (`kart::racer_index(m, p)` reads the field of the struct at `p`). Each accessor is exactly\n\
         //! the memory access it names. Generated; the names come from the dictionary.\n\
         #![allow(non_snake_case, dead_code, clippy::all)]\n",
    );
    let Some(l) = LAYOUT.get() else { return s };
    let mut by_struct: BTreeMap<&str, Vec<(u32, &str, &str)>> = BTreeMap::new();
    for ((st, off), (n, ty)) in &l.fields {
        by_struct.entry(st).or_default().push((*off, n, ty));
    }
    for (st, mut fields) in by_struct {
        fields.sort();
        let module = &l.modules[st];
        let _ = writeln!(s, "\n/// `{st}`.\npub mod {module} {{\n    #[allow(unused_imports)] use recomp::rt as R;\n    #[allow(unused_imports)] use recomp::rt::Mem;");
        for (off, n, ty) in &fields {
            let ty = if ty.is_empty() { String::new() } else { format!(": `{}`", ty.replace(['\n', '\r', '`'], " ")) };
            let _ = writeln!(s, "    /// `{n}`{ty}\n    pub const {}: u32 = {off:#X};", n.to_ascii_uppercase());
        }
        for (f, a) in used.iter().filter(|(f, _)| f.strukt == st) {
            let off = f.off;
            let at = if off == 0 { "p".to_string() } else { format!("p.wrapping_add({off:#X})") };
            if a.is_store() {
                let _ = writeln!(s, "    #[inline(always)] pub fn {}<M: Mem + ?Sized>(m: &mut M, p: u32, v: u32) {{ R::{}(m, {at}, v) }}", f.accessor(*a), a.helper());
            } else {
                let _ = writeln!(s, "    #[inline(always)] pub fn {}<M: Mem + ?Sized>(m: &mut M, p: u32) -> u32 {{ R::{}(m, {at}) }}", f.accessor(*a), a.helper());
            }
        }
        s.push_str("}\n");
    }
    s
}

fn global_name(a: u32) -> Option<String> {
    LAYOUT.get()?.globals.get(&a).cloned()
}

/// Rounds of whole-program type propagation (each passes argument types one call level further).
const TYPE_ROUNDS: usize = 3;

/// Argument types from the callers: (function entry without the Thumb bit, register) -> struct.
static ARG_HINTS: std::sync::RwLock<Option<HashMap<(u32, u8), String>>> = std::sync::RwLock::new(None);

fn arg_hint(entry: u32, r: u8) -> Option<String> {
    ARG_HINTS.read().unwrap().as_ref()?.get(&(entry & !1, r)).cloned()
}

/// Runs `k` with the comment and type sources for the raised version of the ARM9 function at `entry`.
fn with_notes9<T>(segment: &str, entry: u32, k: impl FnOnce(&raise_emit::Notes) -> T) -> T {
    k(&raise_emit::Notes {
        call: &|t| call_name(segment, t),
        field: &|off| field_name(entry, off),
        fields_of: &|off| structs_at(entry, off),
        field_in: &field_in,
        module_of: &|st| LAYOUT.get()?.modules.get(st).cloned(),
        arg_hint: &|r| arg_hint(entry, r),
        global_points: &|a| LAYOUT.get()?.points_global.get(&a).cloned(),
        field_points: &|st, off| LAYOUT.get()?.points_field.get(&(st.to_string(), off)).cloned(),
        structs_with: &|off| LAYOUT.get().and_then(|l| l.by_offset.get(&off).cloned()).unwrap_or_default(),
        density: &|st| LAYOUT.get().and_then(|l| l.density.get(st).copied()).unwrap_or(1.0),
        global: &global_name,
        global_is: &|a| LAYOUT.get()?.is_global.get(&a).cloned(),
    })
}

/// Layout facts from struct inference (`MKDS_TYPE_DUMP=<file>`), gathered over the final pass.
static TYPE_DUMP: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// Block 5.5: entries a direct call must not bypass the dispatcher for (they stay `m.call`): natives
/// (installed by the dispatcher), the OS hooks at dispatch (IRQ safe entries, the tick pin's
/// `OS_GetTick`), and the thread-context code, which must see the true registers in `c.r` (AMCE revision
/// 0: `OS_Rescheduler` 0x0200DE50, `OS_SaveContext` 0x0200E6EC, `OS_LoadContext` 0x0200E738).
fn direct_excluded() -> HashSet<u32> {
    let mut x: HashSet<u32> = recomp::native::NATIVE.iter().chain(recomp::native::fs::NATIVES).map(|n| n.entry & !1).collect();
    x.extend(recomp::os_hle::SAFE_ENTRIES.iter().map(|e| e.0 & !1));
    x.extend([recomp::os_hle::OS_GET_TICK, 0x0200_DE50, 0x0200_E6EC, 0x0200_E738]);
    x
}

/// An ARM9 function: its translation and, when the exact passes are on, its raised version.
fn lifted9(segment: String, f: &lift::Func, id: String, img: &lift::Image) -> Lifted {
    let code = lift::emit_full(f, &id, Some(img), None, raise());
    let t1 = if !raise() {
        Err("exact generation (MKDS_EXACT)")
    } else if std::env::var_os("MKDS_NO_T1").is_some() {
        Err("T1 off (MKDS_NO_T1)")
    } else {
        let _ = raise_emit::take_dump();
        let t = with_notes9(&segment, f.entry, |n| raise_emit::emit_t1_annotated(f, &id, Some(img), n));
        TYPE_DUMP.lock().unwrap().extend(raise_emit::take_dump());
        t
    };
    let fields = if t1.is_ok() { ir::take_field_uses() } else { Vec::new() };
    let direct = raise_emit::take_direct();
    let _ = raise_emit::take_facts();
    let t1_name = format!("f_{id}_t1");
    Lifted { arm7: false, segment, entry: f.entry, code, t1, t1_name, id, fields, direct }
}

fn run() -> Result<ExitCode, String> {
    let args = parse_args()?;
    let info = mkds_builder::rom::inspect_rom(&args.rom).map_err(|e| e.0)?;
    let rom = std::fs::read(&args.rom).map_err(|e| format!("cannot read ROM: {e}"))?;
    let metadata_bytes = std::fs::read(&args.metadata)
        .map_err(|e| format!("cannot read metadata {}: {e}", args.metadata.display()))?;
    let csv = String::from_utf8(metadata_bytes.clone()).map_err(|_| "metadata is not UTF-8")?;
    if csv.trim_start_matches('\u{feff}').starts_with("name,") {
        return Err("private legacy inventory is not accepted; supply address-only metadata (segment,entry,size,mode)".into());
    }
    let entries = build_support::boundaries(&csv).map_err(|e| format!("invalid metadata: {e}"))?;
    if args.recomp.join("Cargo.toml").is_file() {
    } else {
        return Err(format!("--recomp {} is not the recomp crate", args.recomp.display()));
    }
    let recomp_path = args.recomp.canonicalize().map_err(|e| e.to_string())?;

    // exact segment identities: ARM9 main, autoload sections, ov9_NNN overlays
    let mut images: HashMap<String, (u32, Vec<u8>)> = HashMap::new();
    for s in mkds_builder::extract::arm9_sections(&rom).map_err(|e| e.0)? {
        images.insert(s.name, (s.ram_address, s.data));
    }
    for o in mkds_builder::extract::arm9_overlays(&rom).map_err(|e| e.0)? {
        images.insert(format!("ov9_{:03}", o.id), (o.ram_address, o.data));
    }

    // lift every entry; each gets exactly one row
    let mut rows = Vec::new();
    // dictionary names for call comments in raised code (the comment lines come later)
    {
        let mut merged = HashMap::new();
        for p in &args.names {
            let text = std::fs::read_to_string(p).map_err(|e| format!("cannot read names table {}: {e}", p.display()))?;
            merge_names(&text, &mut merged)?;
        }
        let _ = CALL_NAMES.set(merged.into_iter().filter(|(_, e)| !e.name.is_empty()).map(|(k, e)| (k, e.name)).collect());
        // layout tables live next to the name tables (the dictionary folder)
        if let Some(dir) = args.names.first().and_then(|p| p.parent()) {
            let _ = LAYOUT.set(load_layout(dir));
        }
    }
    let mut funcs = Vec::new();
    let mut spans: BTreeMap<String, Vec<(u32, u32)>> = BTreeMap::new();
    let mut pending: Vec<Pending> = Vec::new();
    for b in &entries {
        let mut row = Row {
            status: "rejected",
            segment: b.segment.clone(),
            entry: b.entry,
            thumb: b.thumb,
            size: b.size,
            insts: 0,
            blocks: 0,
            undecoded: Vec::new(),
            reason: String::new(),
        };
        let Some((base, data)) = images.get(&b.segment) else {
            row.status = "segment_absent";
            row.reason = "segment not present in this ROM".into();
            rows.push(row);
            continue;
        };
        if b.entry.checked_sub(*base).and_then(|o| (o as usize).checked_add(b.size as usize)).is_none_or(|end| end > data.len()) {
            row.reason = "boundary outside the selected segment image".into();
            rows.push(row);
            continue;
        }
        let img = lift::Image::new(*base, data);
        let entry = b.entry | b.thumb as u32;
        match lift::discover(&img, entry, Some((b.entry, b.entry + b.size)), MAX_FUNCTION_INSTRUCTIONS) {
            Ok(func) => {
                spans.entry(b.segment.clone()).or_default().push((b.entry, b.entry + b.size));
                row.status = if func.undecoded.is_empty() { "lifted" } else { "lifted_with_fallback" };
                pending.push(Pending { row: rows.len(), segment: b.segment.clone(), lo: b.entry, hi: b.entry + b.size, func, extra: BTreeSet::new() });
            }
            Err(e) => row.reason = e,
        }
        rows.push(row);
    }
    assert_eq!(rows.len(), entries.len(), "every metadata entry must be accounted for");

    // ARM9 code the metadata misses. (1) Cross-function entries: a call, tail call, code pointer or
    // computed-jump target landing inside another listed function at a pc that is not one of its
    // block starts becomes an extra block start there (metadata boundaries can split one routine;
    // without this the dispatcher meets an unknown pc and interprets). (2) Gaps: code in the main
    // image / autoload sections outside every listed function, found by whole-program discovery from
    // the references into gaps plus every data word pointing into a gap. Repeated to a fixpoint.
    let fixed_names: Vec<String> = images.keys().filter(|k| !k.starts_with("ov9_")).cloned().collect();
    let img9 = lift::Image { segs: fixed_names.iter().map(|k| (images[k].0, images[k].1.as_slice())).collect() };
    let fixed_seg = |a: u32| fixed_names.iter().find(|k| { let (b, d) = &images[*k]; (a & !1) >= *b && ((a & !1) - b) < d.len() as u32 }).cloned();
    let mut by_seg: HashMap<String, Vec<(u32, u32, usize)>> = HashMap::new();
    for (i, p) in pending.iter().enumerate() {
        by_seg.entry(p.segment.clone()).or_default().push((p.lo, p.hi, i));
    }
    for v in by_seg.values_mut() {
        v.sort();
    }
    let listed = |seg: &str, a: u32| -> Option<usize> {
        let v = by_seg.get(seg)?;
        let a = a & !1;
        let i = v.partition_point(|r| r.0 <= a);
        (i > 0 && a < v[i - 1].1).then(|| v[i - 1].2)
    };
    let in_gap = |a: u32| fixed_seg(a).is_some_and(|s| listed(&s, a).is_none());
    // overlays: the same discovery inside each overlay's own image (its gaps hold code only while
    // it is resident, so its discovered functions belong to it and dispatch only with it)
    let mut ov_names: Vec<String> = images.keys().filter(|k| k.starts_with("ov9_")).cloned().collect();
    ov_names.sort();
    let in_ov_gap = |ov: &str, a: u32| {
        let (b, d) = &images[ov];
        let x = a & !1;
        x >= *b && x - b < d.len() as u32 && listed(ov, a).is_none()
    };
    let ov_data_ptrs: HashMap<String, Vec<u32>> = ov_names
        .iter()
        .map(|ov| {
            let w: Vec<u32> = images[ov].1.chunks_exact(4).map(|w| u32::from_le_bytes(w.try_into().unwrap()))
                .filter(|&w| (if w & 1 != 0 { true } else { w & 3 == 0 }) && in_ov_gap(ov, w))
                .collect();
            (ov.clone(), w)
        })
        .collect();
    let mut ov_gap_funcs: BTreeMap<String, BTreeMap<u32, lift::Func>> = BTreeMap::new();
    let data_ptrs: Vec<u32> = fixed_names
        .iter()
        .flat_map(|k| images[k].1.chunks_exact(4).map(|w| u32::from_le_bytes(w.try_into().unwrap())))
        .filter(|&w| (if w & 1 != 0 { true } else { w & 3 == 0 }) && in_gap(w))
        .collect();
    let mut gap_funcs: BTreeMap<u32, lift::Func> = BTreeMap::new();
    for round in 0..8 {
        // (gap seeds, strong gap pointer candidates, weak ones)
        let mut gaps: (BTreeSet<u32>, BTreeSet<u32>, BTreeSet<u32>) = Default::default();
        let mut changed: BTreeSet<usize> = BTreeSet::new();
        let mut ov_gaps: BTreeMap<String, BTreeSet<u32>> = BTreeMap::new();
        // `trusted`: the referrer is listed code (its non-pointer references are real); code found by
        // discovery may be data that lifted cleanly, so its references only count at known instructions
        let note = |p_seg: Option<&str>, trusted: bool, fall: bool, refs: Vec<(u32, bool)>, pending: &mut Vec<Pending>, changed: &mut BTreeSet<usize>, gaps: &mut (BTreeSet<u32>, BTreeSet<u32>, BTreeSet<u32>), ov_gaps: &mut BTreeMap<String, BTreeSet<u32>>| {
            for (t, ptr) in refs {
                if let Some(ov) = p_seg.filter(|s| s.starts_with("ov9_")) {
                    if in_ov_gap(ov, t) {
                        ov_gaps.entry(ov.to_string()).or_default().insert(t);
                        continue;
                    }
                }
                // an overlay refers to its own overlay or to the fixed image; fixed code only to fixed code
                let hit = p_seg.filter(|s| s.starts_with("ov9_")).and_then(|s| listed(s, t)).or_else(|| fixed_seg(t).and_then(|s| listed(&s, t)));
                match hit {
                    Some(j) => {
                        let q = &mut pending[j];
                        let known = q.func.blocks.contains(&t) || q.extra.contains(&t) || t == q.func.entry;
                        // a pointer must hit a decoded instruction boundary (else it may be data)
                        // a trusted pointer to code the function's own discovery never reached is kept
                        // if that code lifts cleanly on its own (else it is data inside the range)
                        let clean = |q: &Pending| {
                            let (base, data) = &images[&q.segment];
                            let (lo, hi) = (q.lo, q.hi);
                            lift::discover(&lift::Image::new(*base, data), t, Some((lo, hi)), MAX_FUNCTION_INSTRUCTIONS)
                                .is_ok_and(|f| !f.insts.is_empty() && f.undecoded.is_empty())
                        };
                        // an entry the function's discovery did not reach must lift cleanly on its own,
                        // whoever refers to it (a mis-bounded table can point into an instruction)
                        let ok = q.func.insts.contains_key(&t) || (trusted && clean(q));
                        let _ = ptr;
                        if !known && ok && q.func.entry & 1 == t & 1 {
                            q.extra.insert(t);
                            changed.insert(j);
                        }
                    }
                    None if in_gap(t) => {
                        // every gap candidate must lift with no undecodable site: literals often point at
                        // read-only data in gaps, fall-throughs can be literal pools, and a few listed
                        // "functions" are data themselves (e.g. in the crt0 header area)
                        let _ = (ptr, trusted, fall);
                        gaps.2.insert(t);
                    }
                    None => {}
                }
            }
        };
        for i in 0..pending.len() {
            let (seg, refs, falls) = {
                let p = &pending[i];
                let (base, data) = &images[&p.segment];
                (p.segment.clone(), lift::references(&p.func, &lift::Image::new(*base, data)), lift::fall_exits(&p.func))
            };
            note(Some(&seg), true, false, refs, &mut pending, &mut changed, &mut gaps, &mut ov_gaps);
            note(Some(&seg), true, true, falls.into_iter().map(|t| (t, true)).collect(), &mut pending, &mut changed, &mut gaps, &mut ov_gaps);
        }
        // where a function (and its literal pool) ends, the next function usually starts: try each
        // gap start as a pointer candidate in the mode of the code before it
        let after = |f: &lift::Func, hi: u32| -> u32 {
            let end = f.insts.iter().last().map(|(&a, d)| (a & !1) + d.size).unwrap_or(hi).max(hi);
            let lits: BTreeSet<u32> = lift::literal_addresses(f).into_iter().collect();
            let mut q = (end + 3) & !3;
            while lits.contains(&q) {
                q += 4;
            }
            q | (f.entry & 1)
        };
        for p in pending.iter() {
            let q = after(&p.func, p.hi);
            if p.segment.starts_with("ov9_") {
                if in_ov_gap(&p.segment, q) {
                    ov_gaps.entry(p.segment.clone()).or_default().insert(q);
                }
            } else if in_gap(q) {
                gaps.2.insert(q);
            }
        }
        for (ov, fs) in &ov_gap_funcs {
            for f in fs.values() {
                let q = after(f, 0);
                if in_ov_gap(ov, q) {
                    ov_gaps.entry(ov.clone()).or_default().insert(q);
                }
            }
        }
        for f in gap_funcs.values() {
            let q = after(f, 0);
            if in_gap(q) {
                gaps.2.insert(q);
            }
        }
        let gap_count = gap_funcs.len();
        let weak: Vec<u32> = gaps.2.iter().copied().chain(data_ptrs.iter().copied()).collect();
        let strong: Vec<u32> = gaps.1.iter().copied().collect();
        let seeds: Vec<u32> = gaps.0.iter().copied().collect();
        gap_funcs = lift::discover_program_with(&img9, &seeds, &strong, &weak, &in_gap, MAX_FUNCTION_INSTRUCTIONS);
        for f in gap_funcs.values() {
            note(None, false, false, lift::references(f, &img9), &mut pending, &mut changed, &mut gaps, &mut ov_gaps);
            note(None, false, true, lift::fall_exits(f).into_iter().map(|t| (t, true)).collect(), &mut pending, &mut changed, &mut gaps, &mut ov_gaps);
        }
        let ov_count: usize = ov_gap_funcs.values().map(|m| m.len()).sum();
        for ov in &ov_names {
            let (base, data) = &images[ov];
            let img = lift::Image::new(*base, data);
            let weak: Vec<u32> = ov_gaps.get(ov).into_iter().flatten().copied().chain(ov_data_ptrs[ov].iter().copied()).collect();
            let found = lift::discover_program_with(&img, &[], &[], &weak, &|a| in_ov_gap(ov, a), MAX_FUNCTION_INSTRUCTIONS);
            for f in found.values() {
                note(Some(ov), false, false, lift::references(f, &img), &mut pending, &mut changed, &mut gaps, &mut ov_gaps);
                note(Some(ov), false, true, lift::fall_exits(f).into_iter().map(|t| (t, true)).collect(), &mut pending, &mut changed, &mut gaps, &mut ov_gaps);
            }
            ov_gap_funcs.insert(ov.clone(), found);
        }
        let ov_now: usize = ov_gap_funcs.values().map(|m| m.len()).sum();
        if changed.is_empty() && gap_funcs.len() == gap_count && ov_now == ov_count {
            break;
        }
        for &j in &changed {
            let p = &mut pending[j];
            let (base, data) = &images[&p.segment];
            let img = lift::Image::new(*base, data);
            let extra: Vec<u32> = p.extra.iter().copied().collect();
            let (lo, hi) = (p.lo, p.hi);
            if let Ok(f) = lift::discover_seeded(&img, p.func.entry, &extra, &|a| (a & !1) >= lo && (a & !1) < hi, MAX_FUNCTION_INSTRUCTIONS) {
                p.func = f;
            }
        }
        eprintln!("mkds_gen: ARM9 discovery round {round}: {} functions gained entry points, {} gap functions, {ov_now} overlay gap functions", changed.len(), gap_funcs.len());
    }
    // ARM9 functions to raise: (segment, function, id, image segment; None = the fixed sections)
    let mut jobs: Vec<(String, &lift::Func, String, Option<String>)> = Vec::new();
    for p in &pending {
        let id = format!("{}_{:08X}", p.segment, p.func.entry);
        let row = &mut rows[p.row];
        row.insts = p.func.insts.len();
        row.blocks = p.func.blocks.len();
        row.undecoded = p.func.undecoded.clone();
        if !p.extra.is_empty() {
            row.reason = format!("{} entry points added by cross-function discovery", p.extra.len());
        }
        jobs.push((p.segment.clone(), &p.func, id, Some(p.segment.clone())));
    }

    if let Some(path) = &args.signatures {
        write_signatures(path, &pending, &gap_funcs, &ov_gap_funcs, &|a| fixed_seg(a))?;
    }

    // code with no metadata, found by the generator: our own IRQ vector stubs (rt.rs) and the
    // whole ARM7 program (lift::discover_program over its run-time sections)
    let mut extra: Vec<Row> = Vec::new();
    let stub9: Vec<u8> = rt::ARM9_IRQ_STUB.iter().flat_map(|w| w.to_le_bytes()).collect();
    let stub7: Vec<u8> = rt::ARM7_IRQ_STUB.iter().flat_map(|w| w.to_le_bytes()).collect();
    let stub9_img = lift::Image::new(0xFFFF_0018, &stub9);
    let stub9_f = lift::discover(&stub9_img, 0xFFFF_0018, Some((0xFFFF_0018, 0xFFFF_0018 + stub9.len() as u32)), 100)?;
    extra.push(discovered_row("bios9", &stub9_f));
    jobs.push(("bios9".into(), &stub9_f, "bios9_FFFF0018".into(), Some("bios9".into())));
    for (&entry, f) in &gap_funcs {
        let seg = fixed_seg(entry).expect("gap function inside a fixed segment");
        let id = format!("{seg}_{entry:08X}");
        extra.push(discovered_row(&seg, f));
        jobs.push((seg, f, id, None));
    }
    for (ov, fs) in &ov_gap_funcs {
        for (&entry, f) in fs {
            let id = format!("{ov}_{entry:08X}");
            extra.push(discovered_row(ov, f));
            jobs.push((ov.clone(), f, id, Some(ov.clone())));
        }
    }
    // whole-program struct types: rounds of inference pass argument types from callers to
    // callees; the last round's hints go into the generated code
    let image = |seg: &Option<String>| -> lift::Image {
        match seg.as_deref() {
            None => lift::Image { segs: img9.segs.clone() },
            Some("bios9") => lift::Image::new(0xFFFF_0018, &stub9),
            Some(k) => lift::Image::new(images[k].0, &images[k].1),
        }
    };
    // whole-program flag facts: calls and returns keep only the flags the other side reads
    // (least fixed point: start from "nothing read" and grow until stable)
    if raise() && std::env::var_os("MKDS_CALL_FLAGS_OFF").is_none() {
        let entries: HashSet<u32> = jobs.iter().map(|j| j.1.entry & !1).collect();
        // the functions each code address belongs to (jumps into the middle of a function)
        let mut owner: HashMap<u32, Vec<u32>> = HashMap::new();
        for j in &jobs {
            for &b in &j.1.blocks {
                owner.entry(b & !1).or_default().push(j.1.entry & !1);
            }
        }
        // address-taken functions: an aligned word anywhere in the ARM9 program's images, or a
        // constant in its code, equal to the entry (Thumb bit either way)
        let mut taken: HashSet<u32> = HashSet::new();
        // the values as stored (with the Thumb bit): which instruction set a pointer selects
        let mut taken_exact: HashSet<u32> = HashSet::new();
        let mut words = |data: &[u8]| {
            for w in data.chunks_exact(4) {
                let v = u32::from_le_bytes([w[0], w[1], w[2], w[3]]);
                if entries.contains(&(v & !1)) {
                    taken.insert(v & !1);
                    taken_exact.insert(v);
                }
            }
        };
        for (_, data) in images.values() {
            words(data);
        }
        for (_, f, _, src) in &jobs {
            let img = image(src);
            for v in raise::code_constants(f, Some(&img)) {
                if entries.contains(&(v & !1)) {
                    taken.insert(v & !1);
                    taken_exact.insert(v);
                }
            }
        }
        let zero = || raise::FlagContext {
            entry: jobs.iter().flat_map(|j| j.1.blocks.iter().copied()).map(|b| (b, 0)).collect(),
            ret: entries.iter().map(|&e| (e, 0)).collect(),
            taken: taken.clone(),
            any_entry: 0,
            any_ret: 0,
            restored: 0,
            kills: HashMap::new(),
            entered: HashSet::new(),
        };
        let mut ctx = zero();
        // code other functions call or jump into below a function entry
        raise::set_flag_context(Some(ctx.clone()));
        for (_, f, _, src) in &jobs {
            let img = image(src);
            let (_, calls, tails, _) = raise::flag_facts(f, Some(&img));
            for t in calls.into_iter().filter_map(|c| c.0).chain(tails) {
                let t = t & !1;
                if !entries.contains(&t) && owner.contains_key(&t) {
                    ctx.entered.insert(t);
                }
            }
        }
        // what each function writes before every return: greatest fixed point from "all four"
        ctx.kills = entries.iter().map(|&e| (e, raise::NZCV)).collect();
        for _ in 0..32 {
            raise::set_flag_context(Some(ctx.clone()));
            let mut next = ctx.kills.clone();
            for (_, f, _, src) in &jobs {
                let img = image(src);
                let k = raise::flag_kills(f, Some(&img));
                let e = next.get_mut(&(f.entry & !1)).unwrap();
                *e &= k;
            }
            if next == ctx.kills {
                break;
            }
            ctx.kills = next;
        }
        let kills = ctx.kills.clone();
        eprintln!("mkds_gen: {} of {} functions write all four flags before returning", kills.values().filter(|&&k| k == raise::NZCV).count(), kills.len());
        // address-taken "functions" whose first instruction is conditional are data or code
        // fragments read as functions (zero words decode as `andeq`): compiled code never starts a
        // function by testing its caller's flags, and no computed call reaches them
        let fragments: HashSet<u32> = jobs
            .iter()
            .filter(|(_, f, _, src)| {
                // the ARM9 can't fetch instructions from DTCM
                if (0x027E_0000..0x0280_0000).contains(&(f.entry & !1)) {
                    return true;
                }
                let img = image(src);
                // zero words (padding) decode as `andeq r0, r0, r0` / `movs r0, r0`
                let a = f.entry & !1;
                let zero = if f.entry & 1 != 0 { img.read16(a) == Some(0) } else { img.read32(a) == Some(0) };
                zero || ir::lower_block(f, f.entry, Some(&img)).first().is_some_and(|i| i.cond != 14)
            })
            .map(|(_, f, _, _)| f.entry)
            .collect();
        // an entry a computed call can reach: address-taken, not a fragment, and in the
        // instruction set a stored pointer selects when both decodes of the address are entries
        let exact_entries: HashSet<u32> = jobs.iter().map(|j| j.1.entry).collect();
        let callable = |e: u32| {
            exact_entries.contains(&e) && taken.contains(&(e & !1)) && !fragments.contains(&e) && (!exact_entries.contains(&(e ^ 1)) || taken_exact.contains(&e))
        };
        eprintln!("mkds_gen: {} functions are data, padding or fragments (start with a conditional instruction or a zero word): not call targets", fragments.len());
        let mut rounds = 0;
        loop {
            raise::set_flag_context(Some(ctx.clone()));
            let mut next = zero();
            next.kills = kills.clone();
            next.entered = ctx.entered.clone();
            for (_, f, _, src) in &jobs {
                let img = image(src);
                let (starts, calls, tails, restored) = raise::flag_facts(f, Some(&img));
                next.restored |= restored;
                let me = f.entry & !1;
                // keyed by the exact address: a stray decode of the same bytes in the other
                // instruction set is another block
                for (b, v) in starts {
                    *next.entry.entry(b).or_insert(0) |= v;
                }
                // returns of the code a call or jump reaches must keep `live`
                // (a fixed address outside all known code can't return through our functions)
                let needs = |t: Option<u32>, live: u8, next: &mut raise::FlagContext| match t {
                    Some(t) => owner.get(&(t & !1)).into_iter().flatten().for_each(|g| *next.ret.get_mut(g).unwrap() |= live),
                    None => next.any_ret |= live,
                };
                for (t, live) in calls {
                    if let Some(tt) = t {
                        if live != 0 && std::env::var_os("MKDS_RET_DEBUG").is_some() && ctx.any_ret == 0 && ctx.ret.values().all(|v| *v == 0) {
                            eprintln!("ret_seed: round {rounds} {:#X} calls {tt:#X} leaving {live:#X} live", f.entry);
                        }
                    }
                    if t.is_none() && live != 0 && ctx.any_ret == 0 && std::env::var_os("MKDS_ANYRET_DEBUG").is_some() {
                        eprintln!("any_ret: round {rounds} {:#X} computed call leaves {live:#X} live (ret {:#X} restored {:#X} any_entry {:#X})", f.entry, ctx.ret[&me], ctx.restored, ctx.any_entry);
                    }
                    needs(t, live, &mut next);
                }
                // a tail jump returns to this function's callers
                let mine = ctx.ret[&me] | if ctx.taken.contains(&me) { ctx.any_ret } else { 0 };
                for t in tails {
                    needs(Some(t), mine, &mut next);
                }
            }
            next.any_entry = next.entry.iter().filter(|(e, _)| callable(**e)).fold(ctx.any_entry, |a, (_, b)| a | b);
            next.any_ret |= ctx.any_ret;
            next.restored |= ctx.restored;
            for (k, v) in next.entry.iter_mut() {
                *v |= ctx.entry.get(k).copied().unwrap_or(0);
            }
            for (k, v) in next.ret.iter_mut() {
                *v |= ctx.ret[k];
            }
            if std::env::var_os("MKDS_ANYRET_DEBUG").is_some() {
                for (e, v) in &next.entry {
                    if *v != 0 && callable(*e) && ctx.entry.get(e).copied().unwrap_or(0) == 0 {
                        eprintln!("entry_reads: round {rounds} {e:#X} {v:#X}");
                    }
                }
            }
            rounds += 1;
            if next == ctx {
                break;
            }
            ctx = next;
        }
        raise::set_flag_context(Some(ctx.clone()));
        let free = ctx.ret.iter().filter(|(e, v)| (**v | if taken.contains(*e) { ctx.any_ret } else { 0 }) == 0).count();
        eprintln!("mkds_gen: flag facts in {rounds} rounds: {free}/{} functions return without live flags ({} address-taken; after computed calls {:#X}, after CPSR restores {:#X})", ctx.ret.len(), taken.len(), ctx.any_ret, ctx.restored);
        if let Ok(a) = std::env::var("MKDS_FN_DEBUG") {
            let a = u32::from_str_radix(a.trim_start_matches("0x"), 16).unwrap();
            raise::set_flag_context(Some(ctx.clone()));
            for &b in jobs.iter().filter(|j| j.1.entry & !1 == a).flat_map(|j| j.1.blocks.iter()) { eprintln!("entry fact {b:#X} = {:#X}", ctx.entry.get(&b).copied().unwrap_or(99)); }
            for (_, f, _, src) in jobs.iter().filter(|j| j.1.entry & !1 == a) {
                let img = image(src);
                let sf = raise::flag_effects(f, Some(&img));
                for &b in &f.blocks {
                    for ins in ir::lower_block(f, b, Some(&img)) {
                        eprintln!("{:#X} cond {}", ins.addr, ins.cond);
                        for s in &ins.stmts {
                            eprintln!("    {:?} -> {:?}", s, sf(s, ins.addr));
                            if let ir::S::Call { target: ir::E::Const(t), .. } = s { eprintln!("      callee entry fact {:#X}", ctx.entry.get(t).copied().unwrap_or(99)); }
                        }
                    }
                }
            }
        }
    }
    if raise() && std::env::var_os("MKDS_NO_T1").is_none() && LAYOUT.get().is_some() {
        for round in 0..TYPE_ROUNDS {
            let mut seen: BTreeMap<(u32, u8), BTreeSet<String>> = BTreeMap::new();
            for (seg, f, id, src) in &jobs {
                let img = image(src);
                let _ = with_notes9(seg, f.entry, |n| raise_emit::emit_t1_annotated(f, id, Some(&img), n));
                let _ = ir::take_field_uses();
                let (args, calls) = raise_emit::take_types();
                for (r, st) in args {
                    seen.entry((f.entry & !1, r)).or_default().insert(st);
                }
                for (t, r, st) in calls {
                    seen.entry((t & !1, r)).or_default().insert(st);
                }
            }
            // a hint only where every caller (and the function itself) agrees
            let hints: HashMap<(u32, u8), String> = seen.into_iter().filter(|(_, v)| v.len() == 1).map(|(k, v)| (k, v.into_iter().next().unwrap())).collect();
            eprintln!("mkds_gen: type round {round}: {} typed arguments", hints.len());
            *ARG_HINTS.write().unwrap() = Some(hints);
        }
    }
    // Block 5.5 (`--timing native`): closed functions and their direct signatures
    let mut own_sig: HashMap<(String, u32), raise_emit::DirectSig> = HashMap::new();
    if raise() && std::env::var_os("MKDS_NO_T1").is_none() && raise_emit::nospill_enabled() {
        let t = std::time::Instant::now();
        let emit_facts = |seg: &str, f: &lift::Func, id: &str, src: &Option<String>| -> Option<raise_emit::Facts> {
            let img = image(src);
            raise_emit::set_current(seg, None);
            let r = with_notes9(seg, f.entry, |n| raise_emit::emit_t1_annotated(f, id, Some(&img), n));
            let _ = ir::take_field_uses();
            let _ = raise_emit::take_types();
            let _ = raise_emit::take_dump();
            let facts = raise_emit::take_facts();
            r.ok().and(facts)
        };
        raise_emit::set_direct(HashMap::new());
        let facts: Vec<Option<raise_emit::Facts>> = jobs.iter().map(|(seg, f, id, src)| emit_facts(seg, f, id, src)).collect();
        let excluded = direct_excluded();
        let mut closed: Vec<bool> = jobs.iter().zip(&facts).map(|((_, f, _, _), x)| x.as_ref().is_some_and(|x| x.escape.is_none()) && !excluded.contains(&(f.entry & !1))).collect();
        let mut rows: HashMap<u32, Vec<usize>> = HashMap::new();
        for (i, (_, f, _, _)) in jobs.iter().enumerate() {
            rows.entry(f.entry).or_default().push(i);
        }
        // a call from `seg` reaches a closed row: the caller's own segment, or one always resident
        let reaches = |closed: &[bool], seg: &str, t: u32| -> Option<usize> {
            let r = rows.get(&t)?;
            r.iter().copied().find(|&j| closed[j] && jobs[j].0 == seg).or_else(|| r.iter().copied().find(|&j| closed[j] && !jobs[j].0.starts_with("ov9_")))
        };
        // closed functions that fail the return check once the callee signatures are known
        let mut dropped: HashSet<usize> = HashSet::new();
        let (sigs, rounds) = 'outer: loop {
        loop {
            let mut changed = false;
            for i in 0..jobs.len() {
                if closed[i] && facts[i].as_ref().unwrap().calls.iter().any(|&t| reaches(&closed, &jobs[i].0, t).is_none()) {
                    closed[i] = false;
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        let mut why: BTreeMap<&str, usize> = BTreeMap::new();
        for (i, x) in facts.iter().enumerate() {
            let w = match x {
                None => "not raised",
                Some(_) if closed[i] => "closed",
                Some(_) if dropped.contains(&i) => "returns elsewhere (with the callee signatures)",
                Some(x) => x.escape.unwrap_or(if excluded.contains(&(jobs[i].1.entry & !1)) { "native, hook or thread context" } else { "calls a function that is not closed" }),
            };
            *why.entry(w).or_default() += 1;
        }
        eprintln!("mkds_gen: no-spill: functions by reason {why:?}");
        // readable unique names in the `direct` crate
        let mut count: HashMap<String, usize> = HashMap::new();
        let ident = |i: usize| -> Option<String> {
            let n = CALL_NAMES.get()?.get(&(jobs[i].0.clone(), jobs[i].1.entry & !1))?;
            let n: String = n.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' }).collect();
            let ok = n.chars().next().is_some_and(|c| c.is_ascii_alphabetic()) && !RUST_KEYWORDS.contains(&n.as_str()) && !n.starts_with("f_") && !n.starts_with("sub_");
            ok.then_some(n)
        };
        for i in (0..jobs.len()).filter(|&i| closed[i]) {
            if let Some(n) = ident(i) {
                *count.entry(n).or_default() += 1;
            }
        }
        let mut sigs: BTreeMap<usize, raise_emit::DirectSig> = BTreeMap::new();
        for i in (0..jobs.len()).filter(|&i| closed[i]) {
            let name = match ident(i) {
                Some(n) if count[&n] == 1 => n,
                Some(n) => format!("{n}_{}", jobs[i].2.to_lowercase()),
                None => format!("sub_{}", jobs[i].2.to_lowercase()),
            };
            sigs.insert(i, raise_emit::DirectSig { params: 0, rets: 0, name });
        }
        let table = |sigs: &BTreeMap<usize, raise_emit::DirectSig>| -> HashMap<u32, Vec<(String, raise_emit::DirectSig)>> {
            let mut m: HashMap<u32, Vec<(String, raise_emit::DirectSig)>> = HashMap::new();
            for (&i, d) in sigs {
                m.entry(jobs[i].1.entry).or_default().push((jobs[i].0.clone(), d.clone()));
            }
            m
        };
        // arguments and results: least fixed point (registers only get added)
        let mut rounds = 0;
        loop {
            rounds += 1;
            raise_emit::set_direct(table(&sigs));
            let mut changed = false;
            let keys: Vec<usize> = sigs.keys().copied().collect();
            for i in keys {
                let (seg, f, id, src) = &jobs[i];
                let x = emit_facts(seg, f, id, src).expect("a closed function emits");
                let d = sigs.get_mut(&i).unwrap();
                let (p, r) = (d.params | x.live_in, d.rets | x.written);
                if (p, r) != (d.params, d.rets) {
                    d.params = p;
                    d.rets = r;
                    changed = true;
                }
            }
            if !changed || rounds == 64 {
                break;
            }
        }
        if rounds == 64 {
            return Err("no-spill: direct signatures did not converge".into());
        }
        raise_emit::set_direct(table(&sigs));
        // the final check with the real callee signatures (the first pass assumed the calling convention)
        let mut bad = Vec::new();
        for &i in sigs.keys() {
            let (seg, f, id, src) = &jobs[i];
            if emit_facts(seg, f, id, src).is_none_or(|x| x.escape.is_some()) {
                bad.push(i);
            }
        }
        if bad.is_empty() {
            break 'outer (sigs, rounds);
        }
        for i in bad {
            closed[i] = false;
            dropped.insert(i);
        }
        };
        let sites: usize = (0..jobs.len())
            .filter_map(|i| facts[i].as_ref().map(|x| (i, x)))
            .map(|(i, x)| x.calls.iter().filter(|&&t| reaches(&closed, &jobs[i].0, t).is_some()).count())
            .sum();
        eprintln!("mkds_gen: no-spill: {} closed functions, signatures in {rounds} rounds, {sites} call sites call them directly ({:.1}s)", sigs.len(), t.elapsed().as_secs_f64());
        for (i, d) in sigs {
            own_sig.insert((jobs[i].0.clone(), jobs[i].1.entry), d);
        }
    }
    for (seg, f, id, src) in &jobs {
        let img = image(src);
        raise_emit::set_current(seg, own_sig.get(&(seg.clone(), f.entry)).cloned());
        funcs.push(lifted9(seg.clone(), f, id.clone(), &img));
    }
    raise_emit::set_current("", None);
    if let Some(p) = std::env::var_os("MKDS_TYPE_DUMP") {
        let mut t = TYPE_DUMP.lock().unwrap().join("\n");
        t.push('\n');
        std::fs::write(&p, t).map_err(|e| format!("cannot write {}: {e}", Path::new(&p).display()))?;
    }
    if std::env::var_os("MKDS_FOLD_STATS").is_some() {
        eprintln!("mkds_gen: compare folding [no run, guarded, live after, live beyond cond, no form, folded] = {:?}", raise_emit::fold_stats());
        eprintln!("mkds_gen: flags as locals [compares, made locals, folded away] = {:?}", raise_emit::flag_stats());
    }
    // the ARM7 program keeps the per-function rule
    raise::set_flag_context(None);
    let (arm7_entry, arm7_secs) = build_support::arm7_sections(&rom)?;
    let mut arm7_ranges: Vec<(String, u32, u32)> = vec![("bios7".into(), 0x18, 0x18 + stub7.len() as u32)];
    arm7_ranges.extend(arm7_secs.iter().map(|s| (s.name.clone(), s.ram_address, s.ram_address + s.data.len() as u32)));
    let img7 = lift::Image {
        segs: std::iter::once((0x18u32, stub7.as_slice())).chain(arm7_secs.iter().map(|s| (s.ram_address, s.data.as_slice()))).collect(),
    };
    let seg7 = |a: u32| arm7_ranges.iter().find(|r| (r.1..r.2).contains(&(a & !1))).map(|r| r.0.clone());
    // every aligned word of the ARM7 image that points into its code is a pointer candidate
    // (handler tables live in data); discovery keeps only the ones that lift cleanly
    let words7: Vec<u32> = arm7_secs
        .iter()
        .flat_map(|s| s.data.chunks_exact(4).map(|w| u32::from_le_bytes(w.try_into().unwrap())))
        .filter(|&w| seg7(w).is_some())
        .collect();
    let funcs7 = lift::discover_program_with(&img7, &[arm7_entry, 0x18], &[], &words7, &|a| seg7(a).is_some(), MAX_FUNCTION_INSTRUCTIONS);
    let mut spans7: BTreeMap<String, Vec<(u32, u32)>> = BTreeMap::new();
    for (&entry, f) in &funcs7 {
        let seg = seg7(entry).expect("discovered inside a section");
        let id = format!("{seg}_{entry:08X}");
        let lo = f.insts.keys().next().copied().unwrap_or(entry) & !1;
        let hi = f.insts.iter().last().map(|(&a, d)| (a & !1) + d.size).unwrap_or(lo);
        spans7.entry(seg.clone()).or_default().push((lo, hi));
        extra.push(discovered_row(&seg, f));
        funcs.push(Lifted { arm7: true, segment: seg, entry, code: lift::emit_full(f, &id, Some(&img7), Some(&|d, jumped| rt::arm7_cycles(d, true, jumped)), raise()), t1: if raise() && std::env::var_os("MKDS_NO_T1").is_none() {
            let notes = raise_emit::Notes::NONE;
            raise_emit::emit_t1_full(f, &id, Some(&img7), &notes, Some(&|d, jumped| rt::arm7_cycles(d, true, jumped)))
        } else {
            Err("exact generation")
        }, t1_name: format!("f_{id}_t1"), id, fields: Vec::new(), direct: None });
    }

    // output directory: only create/replace our own previous output
    if args.out.exists() {
        let nonempty = std::fs::read_dir(&args.out).map_err(|e| e.to_string())?.next().is_some();
        if nonempty && !(args.out.join(MARKER).is_file() || args.out.join(OWNED).is_file()) {
            return Err(format!("{} is not empty and is not a previous generator output; refusing to use it", args.out.display()));
        }
    }
    std::fs::create_dir_all(&args.out).map_err(|e| e.to_string())?;
    std::fs::write(args.out.join(OWNED), "created by mkds_gen\n").map_err(|e| e.to_string())?;
    // an unfinished/replaced output must not look complete
    let _ = std::fs::remove_file(args.out.join(MARKER));
    if let Ok(dir) = std::fs::read_dir(&args.out) {
        for e in dir.flatten() {
            let n = e.file_name().to_string_lossy().into_owned();
            if n.starts_with("part_") || n == "lifted" || n == "game" || n == "layout" || n == "direct" {
                let _ = std::fs::remove_dir_all(e.path());
            }
        }
    }

    // readable names as comments (ids and code unchanged)
    let mut named = 0usize;
    if !args.names.is_empty() {
        let mut texts = Vec::new();
        for p in &args.names {
            texts.push(std::fs::read_to_string(p).map_err(|e| format!("cannot read names table {}: {e}", p.display()))?);
        }
        let names = function_names(texts.iter().map(|t| t.as_str()))?;
        for f in funcs.iter_mut().filter(|f| !f.arm7) {
            if let Some(line) = names.get(&(f.segment.clone(), f.entry & !1)) {
                f.code = format!("// {line}\n{}", f.code);
                if let Ok(t1) = &f.t1 {
                    f.t1 = Ok(format!("/// {line}\n{t1}"));
                }
                if let Some(d) = &f.direct {
                    f.direct = Some(format!("/// {line}\n{d}"));
                }
                named += 1;
            }
        }
        let tables: Vec<String> = args.names.iter().map(|p| p.display().to_string()).collect();
        eprintln!("mkds_gen: {named} functions named from {}", tables.join(", "));
    }

    name_t1(&mut funcs);
    untyped_field_widths(&mut funcs);

    // balanced parts by emitted source size
    let recomp_dep = format!("recomp = {{ path = {} }}", toml_path(&recomp_path));
    let mut parts: Vec<Vec<&Lifted>> = vec![Vec::new()];
    let mut cur = 0usize;
    for f in &funcs {
        let len = f.code.len() + f.t1.as_ref().map_or(0, |t| t.len());
        if cur > 0 && cur + len > args.part_bytes {
            parts.push(Vec::new());
            cur = 0;
        }
        cur += len;
        parts.last_mut().unwrap().push(f);
    }
    let gen_id = generator_id();
    let rom_sha = info.sha256.clone();
    let meta_sha = sha_hex(&metadata_bytes);
    let mut table = format!(
        "//! Generated from the user's ROM: the lifted function table and its provenance.\n\
         pub const ROM_SHA256: &str = {rom_sha:?};\npub const METADATA_SHA256: &str = {meta_sha:?};\n\
         pub const GENERATOR_ID: &str = {gen_id:?};\npub const FUNCTION_COUNT: usize = {};\n\
         pub static LIFTED: &[recomp::mem::Lifted] = &[\n",
        funcs.iter().filter(|f| !f.arm7).count()
    );
    let mut table7 = String::from("/// The ARM7 program, found by whole-program discovery (no metadata), plus our ARM7 IRQ stub at 0x18.\npub static LIFTED7: &[recomp::mem::Lifted] = &[\n");
    let mut members = Vec::new();
    // Block 5.5: the direct variants of closed functions, in one crate (they only call each other)
    let direct_src: Vec<&str> = funcs.iter().filter_map(|f| f.direct.as_deref()).collect();
    let direct_dep = if direct_src.is_empty() { String::new() } else { "direct = { path = \"../direct\" }\n".to_string() };
    if !direct_src.is_empty() {
        let dir = args.out.join("direct");
        std::fs::create_dir_all(dir.join("src")).map_err(|e| e.to_string())?;
        put(dir.join("Cargo.toml"), &format!("[package]\nname = \"direct\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\n{recomp_dep}\nlayout = {{ path = \"../layout\" }}\n"))?;
        let mut src = String::from("//! Generated from the user's ROM: raised functions that reach no safe point, called directly with\n//! their registers as Rust arguments and results (Block 5.5, `--timing native`).\n#![allow(non_snake_case, non_upper_case_globals, clippy::all)]\n#[allow(unused_imports)]\nuse layout::*;\n");
        for d in &direct_src {
            src.push_str(d);
        }
        put(dir.join("src/lib.rs"), &src)?;
        members.push("direct".to_string());
        eprintln!("mkds_gen: {} direct functions in crate direct", direct_src.len());
    }
    let mut part_of: HashMap<String, String> = HashMap::new();
    let mut deps = String::new();
    let mut bytes = 0usize;
    for (n, chunk) in parts.iter().enumerate().filter(|(_, c)| !c.is_empty()) {
        let name = format!("part_{n:02}");
        let dir = args.out.join(&name);
        std::fs::create_dir_all(dir.join("src")).map_err(|e| e.to_string())?;
        put(dir.join("Cargo.toml"), &format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\n{recomp_dep}\nlayout = {{ path = \"../layout\" }}\n{direct_dep}"))?;
        let mut src = String::from("//! Generated from the user's ROM (lifted machine code).\n#![allow(non_snake_case, non_upper_case_globals, clippy::all)]\n#[allow(unused_imports)]\nuse layout::*;\n");
        for f in chunk {
            part_of.insert(f.id.clone(), name.clone());
            src.push_str(&f.code);
            let t = if f.arm7 { &mut table7 } else { &mut table };
            if let Ok(t1) = &f.t1 {
                // listed first: the dispatcher maps a block to the first row that has it, so the
                // raised version runs at the entry and the translation everywhere else
                src.push_str(t1);
                let _ = writeln!(t, "    recomp::mem::Lifted {{ segment: {:?}, entry: {:#X}, blocks: &[{:#X}], insts: {name}::INSTS_{id}, f: {name}::{t1} }},", f.segment, f.entry, f.entry, id = f.id, t1 = f.t1_name);
            }
            let _ = writeln!(t, "    recomp::mem::Lifted {{ segment: {:?}, entry: {:#X}, blocks: {name}::BLOCKS_{id}, insts: {name}::INSTS_{id}, f: {name}::f_{id} }},", f.segment, f.entry, id = f.id);
        }
        bytes += src.len();
        put(dir.join("src/lib.rs"), &src)?;
        let _ = writeln!(deps, "{name} = {{ path = \"../{name}\" }}");
        members.push(name);
    }
    table.push_str("];\n");
    table7.push_str("];\n");
    table.push_str(&table7);
    let _ = writeln!(table, "pub const ARM7_FUNCTION_COUNT: usize = {};\npub const ARM7_ENTRY: u32 = {arm7_entry:#X};", funcs7.len());
    let names7: Vec<String> = arm7_ranges.iter().map(|r| format!("{:?}", r.0)).collect();
    let _ = writeln!(table, "/// Segments of `LIFTED7` (none is an overlay: dispatch them all).\npub const ARM7_SEGMENTS: &[&str] = &[{}];", names7.join(", "));
    // ARM7 code signatures: the bytes of every lifted ARM7 function (literal pools excluded), so a
    // host can check once after boot that the ARM7 code in memory is what was lifted
    table.push_str("/// ARM7 code signatures (same format as `SEGMENT_SIGS`); `bios7` is the runtime's own stub.\npub static ARM7_SIGS: &[recomp::residency::SegmentSig] = &[\n");
    for (seg, list) in spans7.iter_mut().filter(|(s, _)| s.as_str() != "bios7") {
        let (base, data) = arm7_secs.iter().find(|s| &s.name == seg).map(|s| (s.ram_address, &s.data)).unwrap();
        let _ = writeln!(table, "    recomp::residency::SegmentSig {{ segment: {seg:?}, ranges: &[");
        for (lo, hi) in merge(list) {
            let _ = writeln!(table, "        recomp::residency::CodeRange {{ start: {lo:#X}, len: {}, hash: {:#X} }},", hi - lo, fnv(&data[(lo - base) as usize..(hi - base) as usize]));
        }
        table.push_str("    ] },\n");
    }
    table.push_str("];\n");
    // overlay residency signatures: hash of every lifted function's bytes, merged into runs
    table.push_str("/// Overlay code signatures: an overlay is resident only if guest RAM matches all ranges.\npub static SEGMENT_SIGS: &[recomp::residency::SegmentSig] = &[\n");
    let mut sig_ranges = 0usize;
    for (seg, list) in spans.iter_mut().filter(|(seg, _)| seg.starts_with("ov9_")) {
        let (base, data) = &images[seg];
        let _ = writeln!(table, "    recomp::residency::SegmentSig {{ segment: {seg:?}, ranges: &[");
        for (lo, hi) in merge(list) {
            let hash = fnv(&data[(lo - base) as usize..(hi - base) as usize]);
            let _ = writeln!(table, "        recomp::residency::CodeRange {{ start: {lo:#X}, len: {}, hash: {hash:#X} }},", hi - lo);
            sig_ranges += 1;
        }
        table.push_str("    ] },\n");
    }
    table.push_str("];\n");
    let ldir = args.out.join("lifted");
    std::fs::create_dir_all(ldir.join("src")).map_err(|e| e.to_string())?;
    put(ldir.join("Cargo.toml"), &format!("[package]\nname = \"lifted\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\n{recomp_dep}\n{deps}"))?;
    put(ldir.join("src/lib.rs"), &table)?;
    members.push("lifted".into());
    // struct layouts and the field accessors the raised code calls
    let used: BTreeSet<(ir::FieldRef, ir::Access)> = funcs.iter().flat_map(|f| f.fields.iter().cloned()).collect();
    let ydir = args.out.join("layout");
    std::fs::create_dir_all(ydir.join("src")).map_err(|e| e.to_string())?;
    put(ydir.join("Cargo.toml"), &format!("[package]
name = \"layout\"
version = \"0.1.0\"
edition = \"2024\"

[dependencies]
{recomp_dep}
"))?;
    put(ydir.join("src/lib.rs"), &layout_crate(&used))?;
    members.push("layout".into());
    eprintln!("mkds_gen: {} field accessors in layout", used.len());
    if args.game {
        let builder = recomp_path.parent().unwrap().join("1_build_tool");
        let gdir = args.out.join("game");
        std::fs::create_dir_all(gdir.join("src")).map_err(|e| e.to_string())?;
        put(gdir.join("Cargo.toml"), &format!("[package]\nname = \"game\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\n{recomp_dep}\nlifted = {{ path = \"../lifted\" }}\nmkds_builder = {{ path = {} }}\n", toml_path(&builder)))?;
        put(gdir.join("src/main.rs"), concat!(
            "//! Generated boot probe: runs the user's ROM on the recompiled game code.\n",
            "//! `game <rom.nds> [budget_millions]` - refuses a ROM other than the one this was generated from.\n",
            "fn main() {\n",
            "    let mut a = std::env::args().skip(1);\n",
            "    let rom_path = a.next().unwrap_or_else(|| { eprintln!(\"usage: game <rom.nds> [budget_millions]\"); std::process::exit(1) });\n",
            "    let budget: u64 = a.next().and_then(|v| v.parse().ok()).unwrap_or(100);\n",
            "    let info = mkds_builder::rom::inspect_rom(std::path::Path::new(&rom_path)).unwrap_or_else(|e| { eprintln!(\"{}\", e.0); std::process::exit(1) });\n",
            "    if info.sha256 != lifted::ROM_SHA256 {\n",
            "        eprintln!(\"This build was generated from a different ROM (sha256 {}); regenerate for {}.\", lifted::ROM_SHA256, info.sha256);\n",
            "        std::process::exit(2);\n",
            "    }\n",
            "    let rom = std::fs::read(&rom_path).expect(\"cannot read the ROM\");\n",
            "    let image = mkds_builder::extract::arm9_main(&rom).expect(\"ARM9 binary\");\n",
            "    recomp::boot::probe(&rom, image, lifted::LIFTED, budget);\n",
            "}\n",
        ))?;
        members.push("game".into());
    }
    if let Some(lock) = &args.lockfile {
        let text = std::fs::read(lock).map_err(|e| format!("cannot read lockfile: {e}"))?;
        std::fs::write(args.out.join("Cargo.lock"), text).map_err(|e| e.to_string())?;
    }
    let list: Vec<String> = members.iter().map(|m| format!("\"{m}\"")).collect();
    put(args.out.join("Cargo.toml"), &format!("[workspace]\nmembers = [{}]\nresolver = \"3\"\n\n[profile.release]\ncodegen-units = 16\ndebug = false\n", list.join(", ")))?;
    if args.lockfile.is_some() {
        // Add the generated member crates to the copied lock without changing any pinned version,
        // so later `cargo build --locked [--offline]` runs reproduce the same dependency set.
        let status = std::process::Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
            .args(["update", "--workspace", "--offline", "--manifest-path"])
            .arg(args.out.join("Cargo.toml"))
            .status()
            .map_err(|e| format!("cannot run cargo to settle the lockfile: {e}"))?;
        if !status.success() {
            return Err("cargo could not settle the copied lockfile offline (are the dependencies cached?)".into());
        }
    }

    // coverage account
    let mut csv_out = String::from("status,segment,entry,mode,size,instructions,blocks,undecoded_sites,reason\n");
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    let mut per_segment: BTreeMap<String, BTreeMap<&str, usize>> = BTreeMap::new();
    let mut fallback_sites = 0usize;
    let mut fallback = Vec::new();
    for r in &rows {
        *counts.entry(r.status).or_default() += 1;
        *per_segment.entry(r.segment.clone()).or_default().entry(r.status).or_default() += 1;
        fallback_sites += r.undecoded.len();
        let _ = writeln!(csv_out, "{},{},{:#010X},{},{},{},{},{},{}", r.status, r.segment, r.entry, if r.thumb { "thumb" } else { "arm" }, r.size, r.insts, r.blocks, r.undecoded.len(), r.reason.replace([',', '\n', '\r'], " "));
        for (pc, word) in &r.undecoded {
            fallback.push(json!({"segment": r.segment, "function": format!("{:#010X}", r.entry), "pc": format!("{pc:#010X}"), "word": format!("{word:#010X}"), "category": "undecoded_instruction"}));
        }
    }
    let mut extra_counts: BTreeMap<String, (usize, usize, usize)> = BTreeMap::new();
    let mut fallback_sites7 = 0usize;
    for r in &extra {
        let e = extra_counts.entry(r.segment.clone()).or_default();
        *e = (e.0 + 1, e.1 + r.insts, e.2 + r.undecoded.len());
        fallback_sites7 += r.undecoded.len();
        let _ = writeln!(csv_out, "{},{},{:#010X},{},{},{},{},{},{}", r.status, r.segment, r.entry, if r.thumb { "thumb" } else { "arm" }, r.size, r.insts, r.blocks, r.undecoded.len(), r.reason);
        for (pc, word) in &r.undecoded {
            fallback.push(json!({"segment": r.segment, "function": format!("{:#010X}", r.entry), "pc": format!("{pc:#010X}"), "word": format!("{word:#010X}"), "category": "undecoded_instruction"}));
        }
    }
    let fallback_sites = fallback_sites + fallback_sites7;
    put(args.out.join("coverage.csv"), &csv_out)?;
    // block 4B tiers: which functions run raised (T1) and why the others stay exact translations
    let mut tiers = String::from("segment,entry,tier,name,part,reason
");
    let mut tier_counts: BTreeMap<&str, usize> = BTreeMap::new();
    for f in &funcs {
        let (tier, reason) = match &f.t1 { Ok(_) => ("T1", ""), Err(r) => ("T0", *r) };
        *tier_counts.entry(if f.t1.is_ok() { "T1" } else { reason }).or_default() += 1;
        let part = part_of.get(&f.id).map(|s| s.as_str()).unwrap_or("");
        let name = if f.t1.is_ok() { f.t1_name.as_str() } else { "" };
        let _ = writeln!(tiers, "{},{:#010X},{tier},{name},{part},{reason}", f.segment, f.entry);
    }
    put(args.out.join("tiers.csv"), &tiers)?;
    eprintln!("mkds_gen: tiers {tier_counts:?}");
    let lifted_n = counts.get("lifted").unwrap_or(&0) + counts.get("lifted_with_fallback").unwrap_or(&0);
    let unaccounted = rows.len() - lifted_n;
    let complete = unaccounted == 0;
    let status = if !complete { "incomplete" } else if fallback_sites > 0 { "complete_with_interpreter_fallback" } else { "complete" };
    let mut report = json!({
        "status": status,
        "rom": {"path": info.path, "sha256": rom_sha, "game_code": info.game_code, "revision": info.revision, "arm9_sha256": info.arm9_sha256},
        "metadata": {"path": args.metadata.to_string_lossy(), "sha256": meta_sha, "entries": rows.len()},
        "generator_id": gen_id,
        "recomp_crate": recomp_path.to_string_lossy(),
        "lockfile_copied": args.lockfile.is_some(),
        "lift_cap": null,
        "counts": counts,
        "per_segment": per_segment,
        "lifted_functions": lifted_n,
        "named_functions": named,
        "rejected_or_absent_entries": unaccounted,
        "tiers": tier_counts,
        "undecoded_sites_left_to_interpreter": fallback_sites,
        "fallback_sites": fallback,
        "generated_source_bytes": bytes,
        "residency_signature_ranges": sig_ranges,
        "workspace_members": members,
        "rejected": rows.iter().filter(|r| !r.status.starts_with("lifted")).map(|r| json!({"status": r.status, "segment": r.segment, "entry": format!("{:#010X}", r.entry), "reason": r.reason})).collect::<Vec<_>>(),
        "service_replacements": "none declared by the generator; BIOS/SDK services are runtime concerns",
        "discovered": {
            "rule": "lift::discover_program: entry + calls + tail calls + literal/adr code pointers that lift with no undecodable site; IRQ stubs from rt.rs",
            "arm7_entry": format!("{arm7_entry:#010X}"),
            "arm7_sections": arm7_secs.iter().map(|s| json!({"name": s.name, "address": format!("{:#010X}", s.ram_address), "bytes": s.data.len()})).collect::<Vec<_>>(),
            "arm7_functions": funcs7.len(),
            "per_segment": extra_counts.iter().map(|(k, v)| (k.clone(), json!({"functions": v.0, "instructions": v.1, "undecoded_sites": v.2}))).collect::<BTreeMap<_, _>>(),
        },
    });
    if lift::timing_native() {
        report["timing"] = json!("native"); // Block 5.4: ARM9 code without the instruction budget
    }
    put(args.out.join(MARKER), &serde_json::to_string_pretty(&report).unwrap())?;
    println!(
        "{status}: {lifted_n}/{} entries lifted in {} part crates ({:.1} MB), {fallback_sites} undecoded sites left to the interpreter -> {}",
        rows.len(),
        members.len() - 2 - args.game as usize - members.iter().any(|m| m == "direct") as usize,
        bytes as f64 / 1e6,
        args.out.display()
    );
    Ok(if complete || args.allow_incomplete { ExitCode::SUCCESS } else { ExitCode::from(2) })
}

/// Coverage row for a function found by the generator (no metadata entry): size = code span.
fn discovered_row(segment: &str, f: &lift::Func) -> Row {
    let lo = f.insts.keys().next().copied().unwrap_or(f.entry) & !1;
    let hi = f.insts.iter().last().map(|(&a, d)| (a & !1) + d.size).unwrap_or(lo);
    Row {
        status: "discovered",
        segment: segment.into(),
        entry: f.entry & !1,
        thumb: f.entry & 1 != 0,
        size: hi - lo,
        insts: f.insts.len(),
        blocks: f.blocks.len(),
        undecoded: f.undecoded.clone(),
        reason: String::new(),
    }
}

/// Sorted, overlapping/adjacent byte ranges merged.
fn merge(list: &mut [(u32, u32)]) -> Vec<(u32, u32)> {
    list.sort();
    let mut merged: Vec<(u32, u32)> = Vec::new();
    for &(lo, hi) in list.iter() {
        match merged.last_mut() {
            Some(last) if lo <= last.1 => last.1 = last.1.max(hi),
            _ => merged.push((lo, hi)),
        }
    }
    merged
}

/// FNV-1a 64, same as `recomp::residency::fnv1a64`.
fn fnv(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for &b in bytes {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("mkds_gen: {e}");
            ExitCode::from(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_table_gives_one_comment_line_per_known_function() {
        let csv = "name,segment,ea,size,meaning\n\
                   sub_2000000,arm9_main,0x02000000,4,\"Copies a block, \"\"fast\"\"\nacross lines\"\n\
                   sub_2000010,arm9_main,0x02000010,4,\n\
                   OS_Init,arm9_main,0x02000021,8,\n";
        let names = function_names([csv]).unwrap();
        assert_eq!(names[&("arm9_main".to_string(), 0x0200_0000)], "sub_2000000: Copies a block, \"fast\" across lines");
        assert!(!names.contains_key(&("arm9_main".to_string(), 0x0200_0010)), "generic name, no meaning");
        assert_eq!(names[&("arm9_main".to_string(), 0x0200_0020)], "OS_Init", "Thumb bit ignored");
        assert!(function_names(["a,b\n1,2\n"]).is_err());
    }

    #[test]
    fn later_tables_override_only_what_they_fill_in() {
        let base = "address,segment,name,args,returns,notes
0x02000000,arm9_main,,a0:u32,void,copies words
";
        let refined = "address,segment,name,args,returns,notes
0x02000000,arm9_main,MI_CpuCopy32,,,
";
        let names = function_names([base, refined]).unwrap();
        assert_eq!(names[&("arm9_main".to_string(), 0x0200_0000)], "MI_CpuCopy32(a0:u32): copies words");
    }

    #[test]
    fn dictionary_rows_give_signature_and_notes() {
        let csv = "address,segment,size,name,kind,area,args,returns,status,confidence,notes\n\
                   0x02000000,arm9_main,4,,game,,,,described,guess,clears a flag\n\
                   0x02000010,arm9_main,4,FX_Div,sdk,,\"a:fx32, b:fx32\",fx32,native,likely,\n\
                   0x02000020,arm9_main,4,,game,,,,unknown,,\n";
        let names = function_names([csv]).unwrap();
        assert_eq!(names[&("arm9_main".to_string(), 0x0200_0000)], "sub_2000000: clears a flag");
        assert_eq!(names[&("arm9_main".to_string(), 0x0200_0010)], "FX_Div(a:fx32, b:fx32) -> fx32");
        assert!(!names.contains_key(&("arm9_main".to_string(), 0x0200_0020)));
    }
}
