//! Build-time pipeline: ROM -> decompress ARM9 -> lift the functions listed in the function
//! table -> `$OUT_DIR/lifted.rs`. With no ROM present it writes an empty table.
#![allow(dead_code)]
#[path = "src/arm.rs"]
mod arm;
mod build_support;
#[path = "src/ir.rs"]
#[allow(dead_code)]
mod ir;
#[path = "src/lift.rs"]
mod lift;
#[path = "src/raise.rs"]
#[allow(dead_code)]
mod raise;
#[path = "src/rt.rs"]
mod rt;
#[path = "src/thumb.rs"]
mod thumb;

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-env-changed=MKDS_ROM");
    println!("cargo:rerun-if-env-changed=MKDS_FUNCTIONS");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/lift.rs");
    println!("cargo:rerun-if-changed=src/arm.rs");
    println!("cargo:rerun-if-changed=src/thumb.rs");
    println!("cargo:rerun-if-changed=src/rt.rs");
    println!("cargo:rerun-if-env-changed=MKDS_LIFT_CAP");
    println!("cargo:rerun-if-env-changed=MKDS_LIFT_OFFSET");
    // how many functions to lift (default 400 while the full-game build is brought up)
    let cap: usize = std::env::var("MKDS_LIFT_CAP")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(400);
    println!("cargo:rerun-if-changed=build_support.rs");
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let out = out_dir.join("lifted.rs");
    let explicit_rom = std::env::var_os("MKDS_ROM");
    let rom_path = explicit_rom
        .clone()
        .map(PathBuf::from)
        // No implicit repository ROM: only an explicit MKDS_ROM is lifted here (capped development
        // path for `mkds_lift`). Production generation is `mkds_gen`, so compiling the `recomp`
        // library for a generated game never depends on a stray gamefile.nds.
        .unwrap_or_else(|| manifest.join("no-rom-supplied.nds"));
    let metadata = std::env::var_os("MKDS_FUNCTIONS")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            // address-only boundaries with modes (5_tools/export_boundaries.py), else the private table
            let b = manifest.join("../4_docs/FUNCTION_BOUNDARIES.csv");
            if b.exists() {
                b
            } else {
                manifest.join("../4_docs/FUNCTION_TABLE.csv")
            }
        });
    // Watch the ROM only when one was supplied: a watched path that does not exist makes Cargo rerun
    // this script (and rebuild recomp plus every generated crate) on every build.
    if explicit_rom.is_some() {
        println!("cargo:rerun-if-changed={}", rom_path.display());
    }
    println!("cargo:rerun-if-changed={}", metadata.display());
    let mut code = String::from(lift::PRELUDE);
    let mut table = String::from("pub static LIFTED: &[recomp::mem::Lifted] = &[\n");
    let mut lifted = 0;
    let mut report = String::from("status,segment,entry,reason\n");
    let mut rejected = 0;
    match std::fs::read(&rom_path) {
        Err(e) if explicit_rom.is_none() && e.kind() == std::io::ErrorKind::NotFound => {
            println!(
                "cargo:warning=No ROM supplied: library-only build, zero lifted game functions."
            );
            report.push_str("no_rom,,,library-only build\n");
        }
        Err(e) => panic!("Cannot read supplied ROM {}: {e}", rom_path.display()),
        Ok(rom) => {
            assert!(rom.len() >= 0x200, "ROM header is truncated");
            assert!(
                &rom[12..16] == b"AMCE" && rom[0x1E] == 0,
                "Current lift spike supports AMCE revision0 only; other ROMs need matched metadata/loader support"
            );
            let sections = mkds_builder::extract::arm9_sections(&rom)
                .expect("Cannot split supplied ROM ARM9 sections");
            let overlays = mkds_builder::extract::arm9_overlays(&rom)
                .expect("Cannot load supplied ROM ARM9 overlays");
            // Metadata segment identifiers select a concrete guest image. Overlay ids
            // are the NitroFS/metadata ov9_### indices, not guest addresses.
            let mut images: HashMap<String, (u32, Vec<u8>)> = sections
                .into_iter()
                .map(|image| (image.name, (image.ram_address, image.data)))
                .collect();
            for overlay in overlays {
                images.insert(
                    format!("ov9_{:03}", overlay.id),
                    (overlay.ram_address, overlay.data),
                );
            }
            let csv = std::fs::read_to_string(&metadata).unwrap_or_else(|e| {
                panic!("Cannot read function metadata {}: {e}", metadata.display())
            });
            let parsed = build_support::boundaries(&csv).expect("Invalid function metadata");
            if csv.trim_start_matches('\u{feff}').starts_with("name,") {
                println!(
                    "cargo:warning=Using private legacy inventory (ARM assumed); distribution builds should set MKDS_FUNCTIONS to address-only metadata with explicit modes."
                );
            }
            // The temporary cap is global. Interleave per-segment boundaries so a sorted
            // address manifest does not spend the entire budget on ARM9 main before overlays.
            let mut by_segment: BTreeMap<String, VecDeque<_>> = BTreeMap::new();
            for boundary in parsed {
                by_segment
                    .entry(boundary.segment.clone())
                    .or_default()
                    .push_back(boundary);
            }
            let mut entries = Vec::new();
            loop {
                let mut any = false;
                for queue in by_segment.values_mut() {
                    if let Some(boundary) = queue.pop_front() {
                        entries.push(boundary);
                        any = true;
                    }
                }
                if !any {
                    break;
                }
            }
            // development batching: skip the first MKDS_LIFT_OFFSET interleaved entries, so
            // successive capped runs (differential tests) can cover the whole table in slices
            let offset: usize = std::env::var("MKDS_LIFT_OFFSET").ok().and_then(|v| v.parse().ok()).unwrap_or(0);
            entries.drain(..offset.min(entries.len()));
            for boundary in entries {
                let ea = boundary.entry;
                let Some((base, image)) = images.get(&boundary.segment) else {
                    report.push_str(&format!(
                        "skipped,{},{ea:#010X},segment absent from ROM image tables\n",
                        boundary.segment
                    ));
                    continue;
                };
                let refuse = if lifted >= cap {
                    Some("MKDS_LIFT_CAP reached")
                } else {
                    None
                };
                if let Some(reason) = refuse {
                    report.push_str(&format!(
                        "skipped,{},{ea:#010X},{reason}\n",
                        boundary.segment
                    ));
                    continue;
                }
                // Refuse boundaries beyond the loaded image before decoding.
                if ea
                    .checked_sub(*base)
                    .and_then(|o| (o as usize).checked_add(boundary.size as usize))
                    .is_none_or(|end| end > image.len())
                {
                    report.push_str(&format!(
                        "rejected,{},{ea:#010X},outside selected image\n",
                        boundary.segment
                    ));
                    rejected += 1;
                    continue;
                }
                let img = lift::Image::new(*base, image);
                let entry = ea | boundary.thumb as u32;
                match lift::discover(&img, entry, Some((ea, ea + boundary.size)), 20_000) {
                    Ok(func) => {
                        // overlays share addresses: names carry the segment
                        let id = format!("{}_{entry:08X}", boundary.segment);
                        code.push_str(&lift::emit_with(&func, &id, Some(&img)));
                        table.push_str(&format!(
                        "    recomp::mem::Lifted {{ segment: {:?}, entry: {entry:#X}, blocks: BLOCKS_{id}, insts: INSTS_{id}, f: f_{id} }},\n",
                        boundary.segment
                    ));
                        lifted += 1;
                        let note = if func.undecoded.is_empty() {
                            String::new()
                        } else {
                            format!(
                                " ({} undecodable sites left to the interpreter)",
                                func.undecoded.len()
                            )
                        };
                        report.push_str(&format!(
                            "lifted,{},{ea:#010X},{} {} instructions in {} blocks{note}\n",
                            boundary.segment,
                            if boundary.thumb { "thumb" } else { "arm" },
                            func.insts.len(),
                            func.blocks.len()
                        ));
                    }
                    Err(reason) => {
                        rejected += 1;
                        let reason = reason.replace([',', '\n', '\r'], " ");
                        report.push_str(&format!(
                            "rejected,{},{ea:#010X},{reason}\n",
                            boundary.segment
                        ));
                    }
                }
            }
            std::fs::write(out_dir.join("lift_report.csv"), &report).unwrap();
            assert!(
                lifted > 0,
                "Supplied ROM/metadata produced no supported lifted functions; see lift_report.csv (not a completed game build)"
            );
            println!(
                "cargo:warning=Lift: {lifted} functions (ARM and Thumb) generated; {rejected} rejected; whole-game boot and DS services are not implemented yet. Report: {}",
                out_dir.join("lift_report.csv").display()
            );
        }
    }
    std::fs::write(out_dir.join("lift_report.csv"), report).unwrap();
    table.push_str("];\n");
    std::fs::write(out, code + &table).unwrap();
}
