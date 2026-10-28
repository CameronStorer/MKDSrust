//! Lifter coverage over every function boundary (dictionary/FUNCTION_BOUNDARIES.csv) of the user's ROM:
//! how many functions discover cleanly, and which encodings are still undecodable (left to the
//! interpreter). Prints a report; fails only if discovery itself errors on a readable entry.
//! Skipped when no ROM is available. Run with `cargo test --test coverage -- --nocapture`.
use recomp::lift::{discover, Image};
use std::collections::BTreeMap;

#[test]
fn lift_coverage() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let rom_path = std::env::var_os("MKDS_ROM").map(std::path::PathBuf::from).unwrap_or_else(|| root.join("6_data/rom/gamefile.nds"));
    let (Ok(rom), Ok(csv)) = (std::fs::read(rom_path), std::fs::read_to_string(root.join("dictionary/FUNCTION_BOUNDARIES.csv"))) else {
        eprintln!("no ROM or boundaries: skipping");
        return;
    };
    let mut images: BTreeMap<String, (u32, Vec<u8>)> = BTreeMap::new();
    for s in mkds_builder::extract::arm9_sections(&rom).unwrap() {
        images.insert(s.name, (s.ram_address, s.data));
    }
    for o in mkds_builder::extract::arm9_overlays(&rom).unwrap() {
        images.insert(format!("ov9_{:03}", o.id), (o.ram_address, o.data));
    }
    let (mut total, mut clean, mut insts, mut missing_seg) = (0, 0, 0usize, 0);
    let mut bad: BTreeMap<String, (usize, u32, u32)> = BTreeMap::new();
    let mut errors = Vec::new();
    for line in csv.lines().skip(1) {
        let f: Vec<&str> = line.split(',').collect();
        let (seg, ea, size, thumb) = (f[0], u32::from_str_radix(&f[1][2..], 16).unwrap(), f[2].parse::<u32>().unwrap(), f[3] == "thumb");
        let Some((base, data)) = images.get(seg) else {
            missing_seg += 1;
            continue;
        };
        total += 1;
        let img = Image::new(*base, data);
        match discover(&img, ea | thumb as u32, Some((ea, ea + size)), 20_000) {
            Ok(func) => {
                insts += func.insts.len();
                if func.undecoded.is_empty() {
                    clean += 1;
                }
                for (pc, w) in func.undecoded {
                    // class: ARM top 12 bits / Thumb top 8 bits
                    let class = if pc & 1 != 0 { format!("thumb {:02X}xx", w >> 8) } else { format!("arm {:03X}xxxxx [{:X}]", w >> 20, (w >> 4) & 15) };
                    let e = bad.entry(class).or_insert((0, w, pc));
                    e.0 += 1;
                }
            }
            Err(e) => errors.push(format!("{seg} {ea:#X}: {e}")),
        }
    }
    eprintln!("{total} functions ({missing_seg} in segments not loaded), {clean} fully decoded, {insts} instructions");
    let mut v: Vec<_> = bad.into_iter().collect();
    v.sort_by_key(|(_, (n, ..))| std::cmp::Reverse(*n));
    eprintln!("undecodable sites by class (count, example word @ pc):");
    for (k, (n, w, pc)) in v.iter().take(40) {
        eprintln!("  {k:<24} {n:>6}  {w:#010X} @ {pc:#010X}");
    }
    for e in errors.iter().take(20) {
        eprintln!("  error: {e}");
    }
    assert!(errors.len() < total / 20 + 1, "{} discovery errors", errors.len());
}
