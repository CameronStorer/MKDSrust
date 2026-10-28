//! Block 4B: raised (T1) functions must behave exactly like their translation (T0 with the same
//! exact passes): same exit pc, CPU state, budget, memory writes and calls, from many random
//! starting states and budgets (small budgets end the function at a slice end). Compiles the
//! generated code with rustc, like `synthetic_lift.rs`.
#![cfg(feature = "gen")]
use recomp::lift::{discover, emit_full, Func, Image};
use recomp::raise_emit::{emit_t1, emit_t1_full, Notes};
use std::{path::PathBuf, process::Command};

#[path = "../build_support.rs"]
#[allow(dead_code)]
mod build_support;

/// Generates T0 + T1 for each function and a `main` comparing them; returns how many were raised.
fn build_checks(funcs: &[(String, u32, Func)], image: &[u8], base: u32, seeds: u32) -> (String, usize) {
    build_checks_cost(funcs, image, base, seeds, None)
}

type Cost<'a> = Option<&'a dyn Fn(&recomp::arm::Decoded, bool) -> u32>;

/// Emission settings are process-wide (`--timing native`): tests that emit hold this lock.
static EMIT: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn emit_lock() -> std::sync::MutexGuard<'static, ()> {
    EMIT.lock().unwrap_or_else(|e| e.into_inner())
}

fn build_checks_cost(funcs: &[(String, u32, Func)], image: &[u8], base: u32, seeds: u32, cost: Cost) -> (String, usize) {
    let _lock = emit_lock();
    let img = Image::new(base, image);
    let mut src = String::from("#![allow(non_upper_case_globals, unused)]\nstatic IMAGE: &[u8] = include_bytes!(\"image.bin\");\n");
    let mut checks = String::new();
    let mut raised = 0;
    for (name, entry, f) in funcs {
        let none = |_: u32| None;
        let notes = Notes { call: &none, ..Notes::NONE };
        let Ok(t1) = emit_t1_full(f, name, Some(&img), &notes, cost) else { continue };
        raised += 1;
        src.push_str(&emit_full(f, name, Some(&img), cost, true));
        src.push_str(&t1);
        // flags live at each block start: at a slice end only these must match (dead flags may differ)
        let (_, live_in) = recomp::raise::flag_liveness_blocks(f, Some(&img));
        let rows: Vec<String> = live_in.iter().map(|(b, l)| format!("({b:#X}, {l})")).collect();
        src.push_str(&format!("static LIVE_{name}: &[(u32, u8)] = &[{}];
", rows.join(", ")));
        checks.push_str(&format!(
            r#"
    for seed in 0..{seeds}u64 {{
        let mut a = recomp::rt::Cpu::default();
        a.budget = [7u32, 23, 61, 150, 400, 5000][(seed % 6) as usize];
        for i in 0..13 {{ a.r[i] = (seed as u32).wrapping_mul(0x9E37_79B1).rotate_left(i as u32 * 5) ^ (i as u32 * 0x0101_0101); }}
        if seed % 3 == 0 {{ a.r[0] = 0; a.r[1] = 1; }}
        a.r[13] = 0x027E_3000; a.r[14] = 0x0200_0401;
        a.n = seed & 1 != 0; a.z = seed & 2 != 0; a.c = seed & 4 != 0; a.v = seed & 8 != 0;
        let mut b = a.clone();
        let mut x = recomp::mem::Ram::new(IMAGE, {base:#X}, seed);
        let mut y = recomp::mem::Ram::new(IMAGE, {base:#X}, seed);
        let p = f_{name}(&mut a, &mut x, {entry:#X});
        let q = f_{name}_t1(&mut b, &mut y, {entry:#X});
        assert_eq!(p, q, "{name} exit, seed {{seed}}");
        if p == recomp::rt::BUDGET_EXHAUSTED {{
            // dead flags may differ at a slice end: compare only the live ones
            let live = LIVE_{name}.iter().find(|e| e.0 == a.r[15]).map_or(15, |e| e.1);
            for (bit, (x, y)) in [(a.n, b.n), (a.z, b.z), (a.c, b.c), (a.v, b.v)].into_iter().enumerate() {{
                if live >> bit & 1 != 0 {{ assert_eq!(x, y, "{name} live flag {{bit}}, seed {{seed}}"); }}
            }}
            a.n = b.n; a.z = b.z; a.c = b.c; a.v = b.v;
        }}
        assert_eq!(a.state(), b.state(), "{name} state, seed {{seed}}");
        assert_eq!(a.budget, b.budget, "{name} budget, seed {{seed}}");
        assert_eq!(x.writes, y.writes, "{name} writes, seed {{seed}}");
        assert_eq!(x.events, y.events, "{name} calls, seed {{seed}}");
    }}"#
        ));
    }
    src.push_str(&format!("fn main() {{{checks}\n    println!(\"ok\");\n}}\n"));
    (src, raised)
}

/// Compiles `src` against the built recomp library and runs it.
fn compile_and_run(src: &str, image: &[u8], tag: &str) {
    let deps = std::env::current_exe().unwrap().parent().unwrap().to_path_buf();
    let library = std::fs::read_dir(&deps)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().starts_with("librecomp-") && e.path().extension().is_some_and(|x| x == "rlib"))
        .max_by_key(|e| e.metadata().unwrap().modified().unwrap())
        .expect("compiled recomp library missing")
        .path();
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let dir = std::env::temp_dir().join(format!("recomp_t1_{tag}_{}_{stamp}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("image.bin"), image).unwrap();
    let source = dir.join("checks.rs");
    let exe = dir.join(format!("checks{}", std::env::consts::EXE_SUFFIX));
    std::fs::write(&source, src).unwrap();
    let mut native = Vec::new();
    if let Ok(builds) = std::fs::read_dir(deps.parent().unwrap().join("build")) {
        for b in builds.filter_map(Result::ok) {
            if let Ok(out) = std::fs::read_to_string(b.path().join("output")) {
                for l in out.lines() {
                    if let Some(p) = l.strip_prefix("cargo:rustc-link-search=native=").or_else(|| l.strip_prefix("cargo::rustc-link-search=native=")) {
                        native.push(format!("native={p}"));
                    }
                }
            }
        }
    }
    let compile = Command::new("rustc")
        .args(["--edition", "2024", "-O"])
        .arg(&source)
        .arg("--extern")
        .arg(format!("recomp={}", library.display()))
        .arg("-L")
        .arg(format!("dependency={}", deps.display()))
        .args(native.iter().flat_map(|n| ["-L", n.as_str()]))
        .arg("-o")
        .arg(&exe)
        .output()
        .unwrap();
    assert!(compile.status.success(), "generated source {} failed to compile:\n{}", source.display(), String::from_utf8_lossy(&compile.stderr));
    let run = Command::new(&exe).output().unwrap();
    assert!(run.status.success(), "T1 differs from T0 ({}):\n{}", source.display(), String::from_utf8_lossy(&run.stderr));
}

#[test]
fn raised_synthetic_functions_match_their_translation() {
    let base = 0x0200_0000u32;
    // Thumb: push {r4, lr}; movs r4, #3; loop: subs r4, #1; bne loop; bl +0; pop {r4, pc}
    let t: [u16; 7] = [0xB510, 0x2403, 0x3C01, 0xD1FD, 0xF000, 0xF800, 0xBD10];
    // ARM: cmp r0, #0; beq skip; add r1, r1, r2; skip: str r1, [r3]; ldr r0, [r3, #4]; bx lr
    let a: [u32; 6] = [0xE350_0000, 0x0A00_0000, 0xE081_1002, 0xE583_1000, 0xE593_0004, 0xE12F_FF1E];
    let mut image: Vec<u8> = t.iter().flat_map(|h| h.to_le_bytes()).collect();
    image.resize(0x20, 0);
    image.extend(a.iter().flat_map(|w| w.to_le_bytes()));
    let img = Image::new(base, &image);
    let funcs = vec![
        ("thumb_loop".to_string(), base + 1, discover(&img, base + 1, Some((base, base + 14)), 100).unwrap()),
        ("arm_if".to_string(), base + 0x20, discover(&img, base + 0x20, Some((base + 0x20, base + 0x38)), 100).unwrap()),
    ];
    let (src, raised) = build_checks(&funcs, &image, base, 200);
    assert_eq!(raised, 2, "both functions are raised");
    compile_and_run(&src, &image, "synthetic");
}

/// Real ROM (`MKDS_ROM`): a seeded sample of ARM9 main functions (`MKDS_T1_SAMPLE`, default 300).
#[test]
fn raised_rom_functions_match_their_translation() {
    let Ok(path) = std::env::var("MKDS_ROM") else {
        eprintln!("MKDS_ROM not set: skipped");
        return;
    };
    let rom = std::fs::read(path).unwrap();
    let image = mkds_builder::extract::arm9_main(&rom).unwrap();
    let base = u32::from_le_bytes(rom[0x28..0x2C].try_into().unwrap());
    let csv = std::fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../4_docs/FUNCTION_BOUNDARIES.csv")).unwrap();
    let img = Image::new(base, &image);
    let want: usize = std::env::var("MKDS_T1_SAMPLE").ok().and_then(|v| v.parse().ok()).unwrap_or(300);
    let mut all: Vec<_> = build_support::boundaries(&csv)
        .unwrap()
        .into_iter()
        .filter(|b| b.segment == "arm9_main" && b.entry >= base && ((b.entry - base) as usize) < image.len())
        .collect();
    // deterministic shuffle
    let mut s = 0x2545_F491_4F6C_DD1Du64;
    for i in (1..all.len()).rev() {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        all.swap(i, (s % (i as u64 + 1)) as usize);
    }
    let mut funcs = Vec::new();
    let mut rejected = std::collections::BTreeMap::<&str, usize>::new();
    for b in all {
        if funcs.len() >= want {
            break;
        }
        let entry = b.entry | b.thumb as u32;
        let Ok(f) = discover(&img, entry, Some((b.entry, b.entry + b.size)), 20_000) else { continue };
        match emit_t1(&f, "x", Some(&img)) {
            Ok(_) => funcs.push((format!("f{:08X}", b.entry), entry, f)),
            Err(r) => *rejected.entry(r).or_default() += 1,
        }
    }
    eprintln!("{} ROM functions raised; stayed T0: {rejected:?}", funcs.len());
    let (src, raised) = build_checks(&funcs, &image, base, 48);
    assert_eq!(raised, funcs.len());
    compile_and_run(&src, &image, "rom");
}

/// Real ROM (`MKDS_ROM`): the ARM7 program (found by discovery; its budget is charged in cycles).
#[test]
fn raised_arm7_functions_match_their_translation() {
    let Ok(path) = std::env::var("MKDS_ROM") else {
        eprintln!("MKDS_ROM not set: skipped");
        return;
    };
    let rom = std::fs::read(path).unwrap();
    let (entry, secs) = build_support::arm7_sections(&rom).unwrap();
    // discover over all sections, then test the functions of the section holding the most of them
    // (the test bus serves one image)
    let full = Image { segs: secs.iter().map(|s| (s.ram_address, s.data.as_slice())).collect() };
    let ranges: Vec<(u32, u32)> = secs.iter().map(|s| (s.ram_address, s.ram_address + s.data.len() as u32)).collect();
    let is_code = |a: u32| ranges.iter().any(|&(l, h)| (l..h).contains(&(a & !1)));
    let all = recomp::lift::discover_program(&full, &[entry], &is_code, 20_000);
    let sec = secs.iter().max_by_key(|s| all.keys().filter(|&&e| (s.ram_address..s.ram_address + s.data.len() as u32).contains(&(e & !1))).count()).unwrap();
    let (lo, hi) = (sec.ram_address, sec.ram_address + sec.data.len() as u32);
    let image = sec.data.clone();
    let funcs: Vec<(String, u32, Func)> = all
        .into_iter()
        .filter(|(_, f)| f.insts.keys().all(|&a| (lo..hi).contains(&(a & !1))))
        .take(400)
        .map(|(e, f)| (format!("a{:08X}", e & !1), e, f))
        .collect();
    let cost = |d: &recomp::arm::Decoded, jumped: bool| recomp::rt::arm7_cycles(d, true, jumped);
    let (src, raised) = build_checks_cost(&funcs, &image, lo, 32, Some(&cost));
    eprintln!("{raised} of {} ARM7 functions raised", funcs.len());
    compile_and_run(&src, &image, "arm7");
}

/// Block 5.5 (no-spill emission, `--timing native`): turns native timing on while alive.
struct NativeTiming;
impl NativeTiming {
    fn on() -> Self {
        recomp::lift::set_timing_native(true, &[]);
        NativeTiming
    }
}
impl Drop for NativeTiming {
    fn drop(&mut self) {
        recomp::lift::set_timing_native(false, &[]);
        recomp::raise_emit::set_direct(Default::default());
        recomp::raise_emit::set_current("", None);
    }
}

/// No-spill checks: T1 emitted without spills (block transfers on the locals, `msr` syncing only the banked
/// registers, no budget lines) and, for closed functions that call nothing, the direct variant (registers as
/// arguments and results) behind its T1 wrapper, against the translation with a budget so large no slice
/// ends: same exit pc, CPU state, memory writes and calls. Returns (source, raised, direct).
fn build_nospill_checks(funcs: &[(String, u32, Func)], image: &[u8], base: u32, seeds: u32) -> (String, usize, usize) {
    use recomp::raise_emit::{set_current, set_direct, take_direct, take_facts, DirectSig};
    let _lock = emit_lock();
    let _native = NativeTiming::on();
    assert!(recomp::raise_emit::nospill_enabled(), "MKDS_SPILL=keep is set");
    let img = Image::new(base, image);
    let none = |_: u32| None;
    let notes = Notes { call: &none, ..Notes::NONE };
    // closed leaves get a direct variant
    set_direct(Default::default());
    let mut sigs = std::collections::HashMap::new();
    for (name, entry, f) in funcs {
        set_current("arm9_main", None);
        if emit_t1_full(f, name, Some(&img), &notes, None).is_ok() {
            if let Some(x) = take_facts() {
                if x.escape.is_none() && x.calls.is_empty() {
                    sigs.insert(*entry, DirectSig { params: x.live_in, rets: x.written, name: format!("d_{name}") });
                }
            }
        }
    }
    set_direct(sigs.iter().map(|(&e, d)| (e, vec![("arm9_main".to_string(), d.clone())])).collect());
    let mut src = String::from("#![allow(non_upper_case_globals, unused)]\nstatic IMAGE: &[u8] = include_bytes!(\"image.bin\");\nmod direct { pub use super::*; }\n");
    let mut checks = String::new();
    let (mut raised, mut direct) = (0, 0);
    for (name, entry, f) in funcs {
        set_current("arm9_main", sigs.get(entry).cloned());
        let Ok(t1) = emit_t1_full(f, name, Some(&img), &notes, None) else { continue };
        assert!(!t1.contains("budget!(0x"), "{name}: no budget lines without a budget");
        if let Some(d) = take_direct() {
            src.push_str(&d);
            direct += 1;
        }
        let _ = take_facts();
        raised += 1;
        src.push_str(&emit_full(f, name, Some(&img), None, true));
        src.push_str(&t1);
        checks.push_str(&format!(
            r#"
    for seed in 0..{seeds}u64 {{
        let mut a = recomp::rt::Cpu::default();
        a.budget = 1 << 22;
        for i in 0..13 {{ a.r[i] = (seed as u32).wrapping_mul(0x9E37_79B1).rotate_left(i as u32 * 5) ^ (i as u32 * 0x0101_0101); }}
        if seed % 3 == 0 {{ a.r[0] = 0; a.r[1] = 1; }}
        a.r[13] = 0x027E_3000; a.r[14] = 0x0200_0401;
        a.n = seed & 1 != 0; a.z = seed & 2 != 0; a.c = seed & 4 != 0; a.v = seed & 8 != 0;
        let mut b = a.clone();
        let mut x = recomp::mem::Ram::new(IMAGE, {base:#X}, seed);
        let mut y = recomp::mem::Ram::new(IMAGE, {base:#X}, seed);
        let p = f_{name}(&mut a, &mut x, {entry:#X});
        let q = f_{name}_t1(&mut b, &mut y, {entry:#X});
        // a long loop can still run out (polls and budgets charge differently): no common end to compare
        if p == recomp::rt::BUDGET_EXHAUSTED || q == recomp::rt::BUDGET_EXHAUSTED {{ continue; }}
        assert_eq!(p, q, "{name} exit, seed {{seed}}");
        a.budget = 0; b.budget = 0;
        assert_eq!(a.state(), b.state(), "{name} state, seed {{seed}}");
        assert_eq!(x.writes, y.writes, "{name} writes, seed {{seed}}");
        assert_eq!(x.events, y.events, "{name} calls, seed {{seed}}");
    }}"#
        ));
    }
    src.push_str(&format!("fn main() {{{checks}\n    println!(\"ok\");\n}}\n"));
    (src, raised, direct)
}

#[test]
fn nospill_synthetic_functions_match_their_translation() {
    let base = 0x0200_0000u32;
    // the functions of `raised_synthetic_functions_match_their_translation`, plus an ARM leaf with block
    // transfers and mode switches: push {r4, r5}; mrs r4, cpsr; orr r5, r4, #0x80; msr cpsr_c, r5;
    // msr cpsr_c, r4; add r0, r0, r4; pop {r4, r5}; bx lr
    let t: [u16; 7] = [0xB510, 0x2403, 0x3C01, 0xD1FD, 0xF000, 0xF800, 0xBD10];
    let a: [u32; 6] = [0xE350_0000, 0x0A00_0000, 0xE081_1002, 0xE583_1000, 0xE593_0004, 0xE12F_FF1E];
    let l: [u32; 8] = [0xE92D_0030, 0xE10F_4000, 0xE384_5080, 0xE121_F005, 0xE121_F004, 0xE080_0004, 0xE8BD_0030, 0xE12F_FF1E];
    let mut image: Vec<u8> = t.iter().flat_map(|h| h.to_le_bytes()).collect();
    image.resize(0x20, 0);
    image.extend(a.iter().flat_map(|w| w.to_le_bytes()));
    image.resize(0x40, 0);
    image.extend(l.iter().flat_map(|w| w.to_le_bytes()));
    let img = Image::new(base, &image);
    let funcs = vec![
        ("thumb_loop".to_string(), base + 1, discover(&img, base + 1, Some((base, base + 14)), 100).unwrap()),
        ("arm_if".to_string(), base + 0x20, discover(&img, base + 0x20, Some((base + 0x20, base + 0x38)), 100).unwrap()),
        ("arm_leaf".to_string(), base + 0x40, discover(&img, base + 0x40, Some((base + 0x40, base + 0x60)), 100).unwrap()),
    ];
    let (src, raised, direct) = build_nospill_checks(&funcs, &image, base, 200);
    assert_eq!(raised, 3, "all three are raised");
    assert_eq!(direct, 2, "the two leaves get a direct variant (the loop polls)");
    compile_and_run(&src, &image, "nospill_synthetic");
}

/// Real ROM (`MKDS_ROM`): the no-spill emission of a seeded sample of ARM9 main functions.
#[test]
fn nospill_rom_functions_match_their_translation() {
    let Ok(path) = std::env::var("MKDS_ROM") else {
        eprintln!("MKDS_ROM not set: skipped");
        return;
    };
    let rom = std::fs::read(path).unwrap();
    let image = mkds_builder::extract::arm9_main(&rom).unwrap();
    let base = u32::from_le_bytes(rom[0x28..0x2C].try_into().unwrap());
    let csv = std::fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../4_docs/FUNCTION_BOUNDARIES.csv")).unwrap();
    let img = Image::new(base, &image);
    let want: usize = std::env::var("MKDS_T1_SAMPLE").ok().and_then(|v| v.parse().ok()).unwrap_or(300);
    let mut all: Vec<_> = build_support::boundaries(&csv)
        .unwrap()
        .into_iter()
        .filter(|b| b.segment == "arm9_main" && b.entry >= base && ((b.entry - base) as usize) < image.len())
        .collect();
    let mut s = 0x9E37_79B9_7F4A_7C15u64;
    for i in (1..all.len()).rev() {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        all.swap(i, (s % (i as u64 + 1)) as usize);
    }
    let mut funcs = Vec::new();
    {
        let _lock = emit_lock();
        for b in all {
            if funcs.len() >= want {
                break;
            }
            let entry = b.entry | b.thumb as u32;
            let Ok(f) = discover(&img, entry, Some((b.entry, b.entry + b.size)), 20_000) else { continue };
            if emit_t1(&f, "x", Some(&img)).is_ok() {
                funcs.push((format!("f{:08X}", b.entry), entry, f));
            }
        }
    }
    let (src, raised, direct) = build_nospill_checks(&funcs, &image, base, 32);
    eprintln!("{raised} ROM functions raised without spills, {direct} of them closed leaves called through their direct variant");
    assert_eq!(raised, funcs.len());
    compile_and_run(&src, &image, "nospill_rom");
}
