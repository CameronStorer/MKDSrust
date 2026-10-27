//! Block 5.0 oracle: replays a build open-loop from a recorded `inputs.csv` and compares logical-frame
//! recordings (`acceptance --record DIR`, see `race_check::oracle`).
//!
//! ```text
//! frame_compare compare A B [--mask FILE]
//! frame_compare replay EXE ROM INPUTS OUT [max-frames] [--mask FILE] [--dump-frames N,..] [--env K=V]..
//! frame_compare check EXE_A EXE_B ROM INPUTS OUT [max-frames] [--mask FILE] [--env-a K=V].. [--env-b K=V]..
//! frame_compare ramdiff A.bin B.bin          (raw RAM dumps from --dump-frames; keep them out of the repo)
//! ```
//! `replay` runs `EXE ROM - OUT/race MAX --record OUT/rec --replay INPUTS`: no route steering, so a divergence
//! cannot correct itself. `check` replays both builds and compares them. The first differing logical frame
//! and its regions are reported; the exit code is 0 when equal, 1 when different, 2 on errors.
//! A mask lists address ranges (`0xSTART-0xEND reason`) or whole regions (`region NAME reason`) allowed to
//! differ, each with its reason. Ranges are applied when recording (hashes cover the masked bytes as zero),
//! so pass the same mask to `replay`/`check`; `region` entries are also honoured by `compare`.
use std::path::{Path, PathBuf};
use std::process::Command;

struct Recording {
    regions: Vec<String>,
    mask: String,
    /// (logical frame, hardware frame, cycles, hashes)
    frames: Vec<(u32, u32, u64, Vec<u64>)>,
    inputs: Vec<(u32, u16)>,
    events: Vec<[u8; 12]>,
}

fn load(dir: &Path) -> Result<Recording, String> {
    let read = |n: &str| std::fs::read(dir.join(n)).map_err(|e| format!("{}: {e}", dir.join(n).display()));
    let regions: Vec<String> = String::from_utf8_lossy(&read("regions.csv")?)
        .lines()
        .skip(1)
        .filter_map(|l| l.split(',').nth(1).map(str::to_string))
        .collect();
    let bin = read("frames.bin")?;
    if bin.len() < 12 || &bin[..8] != b"MKORC001" {
        return Err(format!("{}: not an oracle recording", dir.display()));
    }
    let n = u32::from_le_bytes(bin[8..12].try_into().unwrap()) as usize;
    if n != regions.len() {
        return Err(format!("{}: frames.bin has {n} regions, regions.csv {}", dir.display(), regions.len()));
    }
    let size = 16 + 8 * n;
    let frames = bin[12..]
        .chunks_exact(size)
        .map(|r| {
            let u32_at = |o: usize| u32::from_le_bytes(r[o..o + 4].try_into().unwrap());
            let hashes = (0..n).map(|i| u64::from_le_bytes(r[16 + 8 * i..24 + 8 * i].try_into().unwrap())).collect();
            (u32_at(0), u32_at(4), u64::from_le_bytes(r[8..16].try_into().unwrap()), hashes)
        })
        .collect();
    let mut inputs = Vec::new();
    for line in String::from_utf8_lossy(&read("inputs.csv")?).lines().skip(1) {
        let mut f = line.split(',');
        if let (Some(Ok(a)), Some(Ok(b))) = (f.next().map(str::parse), f.next().map(str::parse)) {
            inputs.push((a, b));
        }
    }
    let events = read("events.bin")?.chunks_exact(12).map(|c| c.try_into().unwrap()).collect();
    let mask = String::from_utf8_lossy(&read("mask.txt").unwrap_or_default()).to_string();
    Ok(Recording { regions, mask, frames, inputs, events })
}

/// `region NAME reason` entries of a mask file.
fn mask_regions(path: Option<&String>) -> Result<Vec<(String, String)>, String> {
    let Some(p) = path else { return Ok(Vec::new()) };
    let text = std::fs::read_to_string(p).map_err(|e| format!("{p}: {e}"))?;
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let line = line.split('#').next().unwrap().trim();
        if let Some(rest) = line.strip_prefix("region ") {
            let (name, why) = rest.trim().split_once(char::is_whitespace).unwrap_or((rest.trim(), ""));
            if why.trim().is_empty() {
                return Err(format!("{p} line {}: region {name} needs a reason", i + 1));
            }
            out.push((name.to_string(), why.trim().to_string()));
        } else if !line.is_empty() && !line.split_once(char::is_whitespace).is_some_and(|(_, r)| !r.trim().is_empty()) {
            return Err(format!("{p} line {}: every mask entry needs a reason", i + 1));
        }
    }
    Ok(out)
}

/// Compares two recordings; prints the report. `Ok(true)` when equal.
fn compare(a: &Path, b: &Path, mask: Option<&String>) -> Result<bool, String> {
    let (ra, rb) = (load(a)?, load(b)?);
    if ra.regions != rb.regions {
        return Err("the recordings have different region lists (different recorder versions)".into());
    }
    if ra.mask != rb.mask {
        println!("warning: the recordings were made with different masks:\n  A: {:?}\n  B: {:?}", ra.mask, rb.mask);
    }
    let skip_names = mask_regions(mask)?;
    for (n, _) in &skip_names {
        if !ra.regions.contains(n) {
            return Err(format!("mask: no region named {n}"));
        }
    }
    let skip: Vec<bool> = ra.regions.iter().map(|n| skip_names.iter().any(|(m, _)| m == n)).collect();
    let mut equal = true;
    // frames present in both, in order; the first missing or differing one is reported
    let (mut i, mut j, mut same) = (0, 0, 0u64);
    let mut first: Option<String> = None;
    while i < ra.frames.len() && j < rb.frames.len() {
        let (fa, fb) = (&ra.frames[i], &rb.frames[j]);
        if fa.0 != fb.0 {
            if first.is_none() {
                let (lo, side) = if fa.0 < fb.0 { (fa.0, "B") } else { (fb.0, "A") };
                first = Some(format!("logical frame {lo}: no sample in {side} (lag frame or a different V-blank wait)"));
            }
            equal = false;
            if fa.0 < fb.0 { i += 1 } else { j += 1 }
            continue;
        }
        let diff: Vec<&str> = (0..ra.regions.len())
            .filter(|&k| !skip[k] && fa.3[k] != fb.3[k])
            .map(|k| ra.regions[k].as_str())
            .collect();
        if diff.is_empty() {
            same += 1;
        } else {
            equal = false;
            if first.is_none() {
                first = Some(format!(
                    "logical frame {} (hardware frame A {} / B {}): {} region(s) differ: {}",
                    fa.0,
                    fa.1,
                    fb.1,
                    diff.len(),
                    diff.join(" ")
                ));
            }
        }
        i += 1;
        j += 1;
    }
    let tail = (ra.frames.len() - i, rb.frames.len() - j);
    println!(
        "A: {} logical frames ({:?}..{:?}); B: {} ({:?}..{:?}); equal in both: {same}",
        ra.frames.len(),
        ra.frames.first().map(|f| f.0),
        ra.frames.last().map(|f| f.0),
        rb.frames.len(),
        rb.frames.first().map(|f| f.0),
        rb.frames.last().map(|f| f.0)
    );
    if tail != (0, 0) {
        println!("unmatched samples at the end: A {} / B {} (a run stopped earlier)", tail.0, tail.1);
    }
    for (n, why) in &skip_names {
        println!("masked region {n}: {why}");
    }
    // inputs
    if ra.inputs != rb.inputs {
        let k = ra.inputs.iter().zip(&rb.inputs).position(|(x, y)| x != y).unwrap_or(ra.inputs.len().min(rb.inputs.len()));
        println!("inputs differ at change row {k}: A {:?} / B {:?}", ra.inputs.get(k), rb.inputs.get(k));
        equal = false;
    }
    // PXI / SPU event streams (informative: the per-frame hashes `pxi` / `spu` already decide equality)
    if ra.events != rb.events {
        let k = ra.events.iter().zip(&rb.events).position(|(x, y)| x != y).unwrap_or(ra.events.len().min(rb.events.len()));
        let show = |e: Option<&[u8; 12]>| {
            e.map_or("<end>".to_string(), |e| {
                format!(
                    "frame {} kind {} off {:#x} value {:#010x}",
                    u32::from_le_bytes(e[..4].try_into().unwrap()),
                    e[4],
                    u16::from_le_bytes(e[6..8].try_into().unwrap()),
                    u32::from_le_bytes(e[8..].try_into().unwrap())
                )
            })
        };
        println!("event streams differ at event {k}: A {} / B {}", show(ra.events.get(k)), show(rb.events.get(k)));
    } else {
        println!("event streams equal ({} PXI/SPU events)", ra.events.len());
    }
    match &first {
        Some(f) => println!("FIRST DIFFERENCE: {f}"),
        None if equal => println!("EQUAL: every common logical frame matches in every region"),
        None => {}
    }
    println!("{}", if equal { "EQUAL" } else { "DIFFERENT" });
    Ok(equal)
}

fn replay(exe: &str, rom: &str, inputs: &str, out: &Path, frames: &str, extra: &[String], env: &[(String, String)]) -> Result<PathBuf, String> {
    std::fs::create_dir_all(out).map_err(|e| e.to_string())?;
    let rec = out.join("rec");
    let race = out.join("race");
    let log = std::fs::File::create(out.join("replay.log")).map_err(|e| e.to_string())?;
    let mut cmd = Command::new(exe);
    cmd.arg(rom).arg("-").arg(&race).arg(frames).arg("--record").arg(&rec).arg("--replay").arg(inputs).args(extra);
    for (k, v) in env {
        cmd.env(k, v);
    }
    eprintln!("replay: {} -> {}", exe, rec.display());
    let status = cmd.stderr(log.try_clone().map_err(|e| e.to_string())?).stdout(log).status().map_err(|e| format!("{exe}: {e}"))?;
    if !status.success() {
        return Err(format!("{exe} failed ({status}); see {}", out.join("replay.log").display()));
    }
    Ok(rec)
}

fn ramdiff(a: &str, b: &str) -> Result<bool, String> {
    let (x, y) = (std::fs::read(a).map_err(|e| e.to_string())?, std::fs::read(b).map_err(|e| e.to_string())?);
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for k in 0..x.len().min(y.len()) {
        if x[k] != y[k] {
            match ranges.last_mut() {
                Some(r) if k - r.1 <= 16 => r.1 = k + 1,
                _ => ranges.push((k, k + 1)),
            }
        }
    }
    for (lo, hi) in ranges.iter().take(200) {
        println!("{:#010x}-{:#010x} ({} bytes)", 0x0200_0000 + lo, 0x0200_0000 + hi, hi - lo);
    }
    println!("{} differing ranges", ranges.len());
    Ok(ranges.is_empty())
}

fn run() -> Result<bool, String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut pos = Vec::new();
    let (mut mask, mut extra) = (None, Vec::new());
    let (mut env_a, mut env_b) = (Vec::new(), Vec::new());
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut value = || it.next().cloned().ok_or(format!("{a} needs a value"));
        let kv = |s: String| s.split_once('=').map(|(k, v)| (k.to_string(), v.to_string())).ok_or(format!("expected K=V, got {s}"));
        match a.as_str() {
            "--mask" => {
                let v = value()?;
                extra.extend(["--mask".to_string(), v.clone()]);
                mask = Some(v);
            }
            "--dump-frames" => extra.extend(["--dump-frames".to_string(), value()?]),
            "--env" | "--env-a" => env_a.push(kv(value()?)?),
            "--env-b" => env_b.push(kv(value()?)?),
            _ => pos.push(a.clone()),
        }
    }
    let usage = "usage: frame_compare compare A B [--mask F] | replay EXE ROM INPUTS OUT [frames] [--mask F] [--env K=V] \
                 | check EXE_A EXE_B ROM INPUTS OUT [frames] [--mask F] [--env-a K=V] [--env-b K=V] | ramdiff A.bin B.bin";
    match pos.first().map(String::as_str) {
        Some("compare") if pos.len() == 3 => compare(Path::new(&pos[1]), Path::new(&pos[2]), mask.as_ref()),
        Some("ramdiff") if pos.len() == 3 => ramdiff(&pos[1], &pos[2]),
        Some("replay") if (5..=6).contains(&pos.len()) => {
            let frames = pos.get(5).map_or("15000", String::as_str);
            replay(&pos[1], &pos[2], &pos[3], Path::new(&pos[4]), frames, &extra, &env_a).map(|_| true)
        }
        Some("check") if (6..=7).contains(&pos.len()) => {
            let out = Path::new(&pos[5]);
            let frames = pos.get(6).map_or("15000", String::as_str);
            let a = replay(&pos[1], &pos[3], &pos[4], &out.join("a"), frames, &extra, &env_a)?;
            let b = replay(&pos[2], &pos[3], &pos[4], &out.join("b"), frames, &extra, &env_b)?;
            compare(&a, &b, mask.as_ref())
        }
        _ => Err(usage.into()),
    }
}

fn main() {
    match run() {
        Ok(true) => {}
        Ok(false) => std::process::exit(1),
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    }
}
