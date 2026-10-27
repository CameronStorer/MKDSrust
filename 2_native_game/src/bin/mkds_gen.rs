//! Production generator: user's ROM + address-only function metadata -> a Cargo workspace of
//! lifted Rust (`part_NN` crates + a `lifted` crate holding `lifted::LIFTED`), with a complete
//! account of every metadata entry. Contains no game data; everything comes from the supplied ROM.
//!
//! ```text
//! mkds_gen --rom <game.nds> --metadata <boundaries.csv> --out <dir>
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
#[path = "../rt.rs"]
mod rt;
#[path = "../signature.rs"]
mod signature;
#[path = "../thumb.rs"]
mod thumb;

use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap};
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
    for p in &pending {
        let (base, data) = &images[&p.segment];
        let img = lift::Image::new(*base, data);
        let id = format!("{}_{:08X}", p.segment, p.func.entry);
        let row = &mut rows[p.row];
        row.insts = p.func.insts.len();
        row.blocks = p.func.blocks.len();
        row.undecoded = p.func.undecoded.clone();
        if !p.extra.is_empty() {
            row.reason = format!("{} entry points added by cross-function discovery", p.extra.len());
        }
        funcs.push(Lifted { arm7: false, segment: p.segment.clone(), entry: p.func.entry, code: lift::emit_full(&p.func, &id, Some(&img), None, raise()), id });
    }

    if let Some(path) = &args.signatures {
        write_signatures(path, &pending, &gap_funcs, &ov_gap_funcs, &|a| fixed_seg(a))?;
    }

    // code with no metadata, found by the generator: our own IRQ vector stubs (rt.rs) and the
    // whole ARM7 program (lift::discover_program over its run-time sections)
    let mut extra: Vec<Row> = Vec::new();
    let stub9: Vec<u8> = rt::ARM9_IRQ_STUB.iter().flat_map(|w| w.to_le_bytes()).collect();
    let stub7: Vec<u8> = rt::ARM7_IRQ_STUB.iter().flat_map(|w| w.to_le_bytes()).collect();
    {
        let img = lift::Image::new(0xFFFF_0018, &stub9);
        let f = lift::discover(&img, 0xFFFF_0018, Some((0xFFFF_0018, 0xFFFF_0018 + stub9.len() as u32)), 100)?;
        extra.push(discovered_row("bios9", &f));
        funcs.push(Lifted { arm7: false, segment: "bios9".into(), entry: f.entry, code: lift::emit_full(&f, "bios9_FFFF0018", Some(&img), None, raise()), id: "bios9_FFFF0018".into() });
    }
    for (&entry, f) in &gap_funcs {
        let seg = fixed_seg(entry).expect("gap function inside a fixed segment");
        let id = format!("{seg}_{entry:08X}");
        extra.push(discovered_row(&seg, f));
        funcs.push(Lifted { arm7: false, segment: seg, entry, code: lift::emit_full(f, &id, Some(&img9), None, raise()), id });
    }
    for (ov, fs) in &ov_gap_funcs {
        let (base, data) = &images[ov];
        let img = lift::Image::new(*base, data);
        for (&entry, f) in fs {
            let id = format!("{ov}_{entry:08X}");
            extra.push(discovered_row(ov, f));
            funcs.push(Lifted { arm7: false, segment: ov.clone(), entry, code: lift::emit_full(f, &id, Some(&img), None, raise()), id });
        }
    }
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
        funcs.push(Lifted { arm7: true, segment: seg, entry, code: lift::emit_full(f, &id, Some(&img7), Some(&|d, jumped| rt::arm7_cycles(d, true, jumped)), raise()), id });
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
            if n.starts_with("part_") || n == "lifted" || n == "game" {
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
                named += 1;
            }
        }
        let tables: Vec<String> = args.names.iter().map(|p| p.display().to_string()).collect();
        eprintln!("mkds_gen: {named} functions named from {}", tables.join(", "));
    }

    // balanced parts by emitted source size
    let recomp_dep = format!("recomp = {{ path = {} }}", toml_path(&recomp_path));
    let mut parts: Vec<Vec<&Lifted>> = vec![Vec::new()];
    let mut cur = 0usize;
    for f in &funcs {
        if cur > 0 && cur + f.code.len() > args.part_bytes {
            parts.push(Vec::new());
            cur = 0;
        }
        cur += f.code.len();
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
    let mut deps = String::new();
    let mut bytes = 0usize;
    for (n, chunk) in parts.iter().enumerate().filter(|(_, c)| !c.is_empty()) {
        let name = format!("part_{n:02}");
        let dir = args.out.join(&name);
        std::fs::create_dir_all(dir.join("src")).map_err(|e| e.to_string())?;
        put(dir.join("Cargo.toml"), &format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\n{recomp_dep}\n"))?;
        let mut src = String::from("//! Generated from the user's ROM (lifted machine code).\n#![allow(non_snake_case, non_upper_case_globals, clippy::all)]\n");
        for f in chunk {
            src.push_str(&f.code);
            let t = if f.arm7 { &mut table7 } else { &mut table };
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
    let lifted_n = counts.get("lifted").unwrap_or(&0) + counts.get("lifted_with_fallback").unwrap_or(&0);
    let unaccounted = rows.len() - lifted_n;
    let complete = unaccounted == 0;
    let status = if !complete { "incomplete" } else if fallback_sites > 0 { "complete_with_interpreter_fallback" } else { "complete" };
    let report = json!({
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
    put(args.out.join(MARKER), &serde_json::to_string_pretty(&report).unwrap())?;
    println!(
        "{status}: {lifted_n}/{} entries lifted in {} part crates ({:.1} MB), {fallback_sites} undecoded sites left to the interpreter -> {}",
        rows.len(),
        members.len() - 1 - args.game as usize,
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
