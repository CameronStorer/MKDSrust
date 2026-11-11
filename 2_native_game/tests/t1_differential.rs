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

/// The lock, and the fixtures' poll mode: direct functions with loop polls are on by default in native builds,
/// where `mkds_gen` adds what they need (the IRQ proof, the flat copies). These single-function fixtures were
/// written without it, so they run with `MKDS_POLL_DIRECT=0` unless the variable is set; the poll tests set it
/// themselves.
struct EmitLock(#[allow(dead_code)] std::sync::MutexGuard<'static, ()>, Option<std::ffi::OsString>);

impl Drop for EmitLock {
    fn drop(&mut self) {
        unsafe { match &self.1 { Some(v) => std::env::set_var("MKDS_POLL_DIRECT", v), None => std::env::remove_var("MKDS_POLL_DIRECT") } }
    }
}

fn emit_lock() -> EmitLock {
    let guard = EMIT.lock().unwrap_or_else(|e| e.into_inner());
    let old = std::env::var_os("MKDS_POLL_DIRECT");
    // an empty value counts as unset (scripts often clear variables with `VAR=`)
    if old.as_ref().map_or(true, |v| v.is_empty()) { unsafe { std::env::set_var("MKDS_POLL_DIRECT", "0"); } }
    EmitLock(guard, old)
}

fn build_checks_cost(funcs: &[(String, u32, Func)], image: &[u8], base: u32, seeds: u32, cost: Cost) -> (String, usize) {
    let _lock = emit_lock();
    let img = Image::new(base, image);
    let mut src = String::from("#![allow(non_upper_case_globals, unused)]\nstatic IMAGE: &[u8] = include_bytes!(\"image.bin\");\n");
    let mut checks = String::new();
    let mut check_calls = String::new();
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
#[inline(never)]
fn check_{name}() {{
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
    }}
}}"#
        ));
        check_calls.push_str(&format!("check_{name}();\n"));
    }
    src.push_str(&checks);
    src.push_str(&format!("fn main() {{{check_calls}\n    println!(\"ok\");\n}}\n"));
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
    let csv = std::fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../dictionary/FUNCTION_BOUNDARIES.csv")).unwrap();
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
        recomp::raise_emit::set_poll_irq_preserved(false);
        recomp::lift::set_timing_native(false, &[]);
        recomp::raise_emit::set_direct(Default::default());
        recomp::raise_emit::set_boundary(Default::default());
        recomp::raise_emit::set_current("", None);
    }
}

/// Direct poll coverage includes both the conservative context path and the
/// proven IRQ path, and executes an interrupt while the loop's locals are live.
#[test]
fn direct_poll_loops_resume_with_live_registers_and_flags() {
    use recomp::raise_emit::{set_current, set_direct, set_poll_irq_preserved, take_direct, take_facts, DirectSig};
    let _lock = emit_lock();
    let _native = NativeTiming::on();
    struct Modes(Vec<(&'static str, Option<std::ffi::OsString>)>);
    impl Drop for Modes {
        fn drop(&mut self) {
            for (k, v) in &self.0 { unsafe { match v {
                Some(v) => std::env::set_var(k, v), None => std::env::remove_var(k),
            } } }
        }
    }
    let _modes = Modes(["MKDS_POLL_DIRECT", "MKDS_FLAG_LOCALS", "MKDS_BOUNDARY"].into_iter()
        .map(|k| (k, std::env::var_os(k))).collect());
    unsafe { std::env::set_var("MKDS_POLL_DIRECT", "1"); }
    let base = 0x0200_0000;
    let mut image: Vec<u8> = [0xE92D_4010u32, 0xE3A0_4003, 0xE080_0004,
        0xE254_4001, 0x1AFF_FFFC, 0xE8BD_4010, 0xE12F_FF1E].iter().flat_map(|w| w.to_le_bytes()).collect();
    image.resize(0x40, 0);
    image.extend([0xB510u16, 0x2403, 0x1900, 0x3C01, 0xD1FC, 0xBC10, 0xBC08, 0x4718].iter().flat_map(|h| h.to_le_bytes()));
    image.resize(0x80, 0);
    image.extend([0xE92D_4010u32, 0xE3A0_4011, 0xEBFF_FFDC,
        0xE080_0004, 0xE8BD_4010, 0xE12F_FF1E].iter().flat_map(|w| w.to_le_bytes()));
    let img = Image::new(base, &image);
    for narrow in [false, true] {
    for flags in ["0", "1"] {
    for boundary in ["0", "1"] {
        unsafe { std::env::set_var("MKDS_FLAG_LOCALS", flags); std::env::set_var("MKDS_BOUNDARY", boundary); }
        set_poll_irq_preserved(narrow);
        set_direct(Default::default());
        let mut direct_sigs = std::collections::HashMap::new();
        let mut src = String::from("#![allow(unused)]\nstatic IMAGE: &[u8] = include_bytes!(\"image.bin\");\nmod direct { pub use super::*; }\n");
        src.push_str(&format!("const NARROW: bool = {narrow};\n"));
        for (name, entry, end) in [("arm_poll", base, base + 28), ("thumb_poll", base + 0x41, base + 0x50)] {
            let f = discover(&img, entry, Some((entry & !1, end)), 100).unwrap();
            set_current("arm9_main", None);
            emit_t1(&f, name, Some(&img)).unwrap();
            let facts = take_facts().unwrap();
            assert_eq!(facts.escape, None);
            assert!(facts.polls);
            if !narrow { assert_eq!(facts.live_in, 0x7FFF); assert_eq!(facts.written, 0x7FFF); }
            let sig = DirectSig { cpu: true, params: facts.live_in, rets: facts.written, name: format!("d_{name}") };
            direct_sigs.insert(entry, vec![("arm9_main".into(), sig.clone())]);
            set_current("arm9_main", Some(sig));
            let t1 = emit_t1(&f, name, Some(&img)).unwrap();
            let d = take_direct().unwrap();
            assert!(take_facts().unwrap().cpu, "poll macros must retain the CPU argument");
            assert!(d.contains(if narrow { "checked IRQ preservation" } else { "full IRQ/context spill" }));
            if narrow { assert!(!d.contains("r4 = c.r[4]")); }
            src.push_str(&emit_full(&f, name, Some(&img), None, true));
            src.push_str(&d); src.push_str(&t1);
        }
        set_direct(direct_sigs);
        let parent = discover(&img, base + 0x80, Some((base + 0x80, base + 0x98)), 100).unwrap();
        set_current("arm9_main", None);
        emit_t1(&parent, "parent_poll", Some(&img)).unwrap();
        let x = take_facts().unwrap();
        assert_eq!(x.escape, None);
        set_current("arm9_main", Some(DirectSig { cpu: true, params: x.live_in, rets: x.written, name: "d_parent_poll".into() }));
        let t1 = emit_t1(&parent, "parent_poll", Some(&img)).unwrap();
        src.push_str(&emit_full(&parent, "parent_poll", Some(&img), None, true));
        src.push_str(&take_direct().unwrap()); src.push_str(&t1);
        src.push_str(r#"
use recomp::rt::{Bus as _, Mem as _};
struct PollBus { ram: recomp::mem::Ram<'static>, polls: usize, suspends: bool }
impl recomp::rt::Mem for PollBus {
    fn r8(&mut self,a:u32)->u8 { self.ram.r8(a) }
    fn r16(&mut self,a:u32)->u16 { self.ram.r16(a) }
    fn r32(&mut self,a:u32)->u32 { self.ram.r32(a) }
    fn w8(&mut self,a:u32,v:u8) { self.ram.w8(a,v) }
    fn w16(&mut self,a:u32,v:u16) { self.ram.w16(a,v) }
    fn w32(&mut self,a:u32,v:u32) { self.ram.w32(a,v) }
}
impl recomp::rt::Bus for PollBus {
    fn can_suspend(&self)->bool { self.suspends }
    fn call(&mut self,c:&mut recomp::rt::Cpu,t:u32,_:u32)->u32 {
        assert_eq!(t,0x02000000); f_arm_poll(c,self,t)
    }
    fn swi(&mut self,_:&mut recomp::rt::Cpu,_:u32) { unreachable!() }
    fn mrc(&mut self,_:&mut recomp::rt::Cpu,_:u8,_:u8,_:u8,_:u8,_:u8)->u32 { unreachable!() }
    fn mcr(&mut self,_:&mut recomp::rt::Cpu,_:u8,_:u8,_:u8,_:u8,_:u8,_:u32) { unreachable!() }
    fn slice_end(&mut self,c:&mut recomp::rt::Cpu,pc:u32,cost:u32)->bool {
        self.polls += 1;
        if !self.suspends { return false; }
        // The BIOS stub saves volatile registers; exception mode banks sp/lr
        // and preserves the interrupted flags. The handler uses its own stack.
        let volatile = [c.r[0],c.r[1],c.r[2],c.r[3],c.r[12]];
        let sp = c.r[13]; let lr = c.r[14];
        recomp::rt::take_irq(c,pc,0xFFFF0000);
        c.r[0] = 0xDEAD; c.r[1] = 0xBEEF; c.r[2] = 0xABCD;
        c.r[3] = 0x1234; c.r[12] = 0x5678;
        c.n = true; c.z = false; c.c = false; c.v = true;
        // Model an unproven handler/context path that observes an incoming
        // saved register and changes another. Only the full form permits this.
        if !NARROW { assert_eq!(c.r[4],4-self.polls as u32); c.r[5] ^= c.r[6]; }
        [c.r[0],c.r[1],c.r[2],c.r[3],c.r[12]] = volatile;
        c.restore_spsr();
        assert_eq!((c.r[13],c.r[14]),(sp,lr));
        // Pay exactly this block, forcing another IRQ on the next iteration.
        c.budget = cost;
        assert!(recomp::rt::spend(c,cost));
        true
    }
}
fn main() {
    for (entry,t0,t1) in [(0x02000000,f_arm_poll as recomp::mem::LiftedFn,f_arm_poll_t1 as recomp::mem::LiftedFn),
        (0x02000041,f_thumb_poll as recomp::mem::LiftedFn,f_thumb_poll_t1 as recomp::mem::LiftedFn),
        (0x02000080,f_parent_poll as recomp::mem::LiftedFn,f_parent_poll_t1 as recomp::mem::LiftedFn)] {
    for seed in 0..128u64 {
    for suspends in [false,true] {
        let mut a = recomp::rt::Cpu::default();
        for i in 0..15 { a.r[i] = (seed as u32).wrapping_mul(0x9E3779B1).rotate_left(i as u32); }
        a.r[13]=0x027E3000; a.r[14]=0x02000401; a.budget=0;
        a.banked[2][0]=0x027E1000;
        a.n=seed&1!=0; a.z=seed&2!=0; a.c=seed&4!=0; a.v=seed&8!=0;
        let mut b=a.clone();
        let mut x=PollBus { ram:recomp::mem::Ram::new(IMAGE,0x02000000,seed),polls:0,suspends };
        let mut y=PollBus { ram:recomp::mem::Ram::new(IMAGE,0x02000000,seed),polls:0,suspends };
        assert_eq!(t0(&mut a,&mut x,entry),t1(&mut b,&mut y,entry));
        let expected = if suspends { 3 } else { 1 };
        assert_eq!(x.polls,expected); assert_eq!(y.polls,expected);
        a.budget=0; b.budget=0;
        assert_eq!(a.state(),b.state(),"entry {entry:#X} seed {seed}");
        assert_eq!(x.ram.writes,y.ram.writes);
    }}}
}
"#);
        compile_and_run(&src, &image, &format!("poll_{narrow}_{flags}_{boundary}"));
    }}}
}

struct BoundaryMode(Option<std::ffi::OsString>);
impl BoundaryMode {
    fn on() -> Self {
        let old = std::env::var_os("MKDS_BOUNDARY");
        unsafe { std::env::set_var("MKDS_BOUNDARY", "1"); }
        Self(old)
    }
}
impl Drop for BoundaryMode {
    fn drop(&mut self) {
        unsafe {
            match &self.0 {
                Some(v) => std::env::set_var("MKDS_BOUNDARY", v),
                None => std::env::remove_var("MKDS_BOUNDARY"),
            }
        }
        recomp::raise_emit::set_boundary(Default::default());
    }
}

#[test]
fn direct_poll_proof_rejects_unsaved_handlers_and_exact_output_is_unchanged() {
    use recomp::raise_emit::{set_current, take_facts};
    let _lock = emit_lock();
    let _native = NativeTiming::on();
    let _boundary = BoundaryMode::on();
    struct PollMode(Option<std::ffi::OsString>);
    impl Drop for PollMode { fn drop(&mut self) { unsafe { match &self.0 {
        Some(v) => std::env::set_var("MKDS_POLL_DIRECT", v),
        None => std::env::remove_var("MKDS_POLL_DIRECT"),
    } } } }
    let _poll = PollMode(std::env::var_os("MKDS_POLL_DIRECT"));
    let base = 0x0200_0000;
    // A saved r4 is preserved across every loop iteration. The otherwise
    // identical handler without the push/pop must not certify a narrow poll.
    let safe = [0xE92D_4010u32, 0xE3A0_4003, 0xE254_4001,
        0x1AFF_FFFD, 0xE8BD_4010, 0xE12F_FF1E];
    let unsafe_handler = [0xE3A0_4003u32, 0xE254_4001, 0x1AFF_FFFD, 0xE12F_FF1E];
    unsafe { std::env::set_var("MKDS_POLL_DIRECT", "1"); }
    for (words, preserved) in [(&safe[..], true), (&unsafe_handler[..], false)] {
        let bytes: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
        let img = Image::new(base, &bytes);
        let f = discover(&img, base, Some((base, base + bytes.len() as u32)), 100).unwrap();
        set_current("arm9_main", None);
        emit_t1(&f, "handler", Some(&img)).unwrap();
        let x = take_facts().unwrap();
        assert_eq!(x.irq_preserved & 0x2FF0 == 0x2FF0, preserved);
        assert_eq!(x.preserved, 0, "unproven IRQ paths cannot be narrow boundary callees");
        assert!(x.polls);
    }
    let bytes: Vec<u8> = safe.iter().flat_map(|w| w.to_le_bytes()).collect();
    let img = Image::new(base, &bytes);
    let f = discover(&img, base, Some((base, base + bytes.len() as u32)), 100).unwrap();
    unsafe { std::env::set_var("MKDS_POLL_DIRECT", "0"); }
    emit_t1(&f, "off", Some(&img)).unwrap();
    assert_eq!(take_facts().unwrap().escape, Some("loop (poll point)"));
    recomp::lift::set_timing_native(false, &[]);
    let t0 = emit_full(&f, "exact", Some(&img), None, true);
    let t1 = emit_t1(&f, "exact", Some(&img)).unwrap();
    unsafe { std::env::set_var("MKDS_POLL_DIRECT", "1"); }
    assert!(!recomp::raise_emit::poll_direct_enabled());
    assert_eq!(t0, emit_full(&f, "exact", Some(&img), None, true));
    assert_eq!(t1, emit_t1(&f, "exact", Some(&img)).unwrap());
}

#[test]
fn boundary_direct_calls_match_translation_on_random_states() {
    use recomp::raise_emit::{set_boundary, set_current, take_direct, take_facts, BoundarySig, DirectSig};
    let _lock = emit_lock();
    let _native = NativeTiming::on();
    let _boundary = BoundaryMode::on();
    let base = 0x0200_0000;
    // Caller keeps r4 in a Rust local, and overwrites r2 even though the callee never reads it.
    let caller: [u32; 7] = [0xE92D_4010, 0xE3A0_4007, 0xE3A0_2063,
        0xEB00_003B, 0xE080_0004, 0xE8BD_4010, 0xE12F_FF1E];
    let mut image: Vec<u8> = caller.iter().flat_map(|w| w.to_le_bytes()).collect();
    image.resize(0x40, 0);
    // Tail call through the same boundary.
    image.extend([0xE3A0_2063u32, 0xEA00_002D].iter().flat_map(|w| w.to_le_bytes()));
    image.resize(0x100, 0);
    // Consume incoming carry before the loop overwrites it: tail boundaries
    // must load their flag inputs too. r12 and r1 pass through unchanged.
    image.extend([0xE0A0_0001u32, 0xE3A0_2003, 0xE252_2001, 0x1AFF_FFFD, 0xE080_0001,
        0xE283_3001, 0xE12F_FF1E].iter().flat_map(|w| w.to_le_bytes()));
    let img = Image::new(base, &image);
    let open = discover(&img, base + 0x100, Some((base + 0x100, base + 0x11C)), 100).unwrap();
    set_current("arm9_main", None);
    emit_t1(&open, "open", Some(&img)).unwrap();
    let facts = take_facts().unwrap();
    assert_eq!(facts.escape, Some("loop (poll point)"));
    assert_eq!(facts.preserved, 0x2FF0);
    assert_eq!(facts.boundary_params, 0x100B);
    set_boundary([(base + 0x100, vec![("arm9_main".into(), BoundarySig {
        params: facts.boundary_params, preserved: facts.preserved,
    })])].into_iter().collect());
    let mut src = String::from("#![allow(unused)]\nmod direct { pub use super::*; }\n");
    src.push_str(&emit_full(&open, "open", Some(&img), None, true));
    for (name, entry, end) in [("boundary", base, base + 28), ("tail", base + 0x40, base + 0x48)] {
        let f = discover(&img, entry, Some((entry, end)), 100).unwrap();
        set_current("arm9_main", Some(DirectSig { params: 0x7FFF, rets: 0x5FFF, name: format!("d_{name}"), cpu: true }));
        let t1 = emit_t1(&f, name, Some(&img)).unwrap();
        assert_eq!(take_facts().unwrap().escape, None, "caller {name}");
        let d = take_direct().unwrap();
        assert!(d.contains("checked dispatcher boundary"));
        assert!(!d.contains("c.r[4] ="));
        assert!(!d.contains("c.r[2] ="), "callee overwrites r2 before reading it");
        assert!(!d.contains("spill!();"));
        src.push_str(&emit_full(&f, name, Some(&img), None, true));
        src.push_str(&t1);
        src.push_str(&d);
    }
    src.push_str(r#"
use recomp::rt::{Bus as _, Mem as _};
struct CheckBus(recomp::mem::Ram<'static>);
impl recomp::rt::Mem for CheckBus {
    fn r8(&mut self,a:u32)->u8 { self.0.r8(a) }
    fn r16(&mut self,a:u32)->u16 { self.0.r16(a) }
    fn r32(&mut self,a:u32)->u32 { self.0.r32(a) }
    fn w8(&mut self,a:u32,v:u8) { self.0.w8(a,v) }
    fn w16(&mut self,a:u32,v:u16) { self.0.w16(a,v) }
    fn w32(&mut self,a:u32,v:u32) { self.0.w32(a,v) }
}
impl recomp::rt::Bus for CheckBus {
    fn call(&mut self,c:&mut recomp::rt::Cpu,t:u32,ret:u32)->u32 {
        assert_eq!(t,0x02000100);
        self.0.events.push((t,ret));
        f_open(c,self,t)
    }
    fn swi(&mut self,c:&mut recomp::rt::Cpu,n:u32) { self.0.swi(c,n) }
    fn mrc(&mut self,c:&mut recomp::rt::Cpu,cp:u8,o:u8,n:u8,m:u8,p:u8)->u32 { self.0.mrc(c,cp,o,n,m,p) }
    fn mcr(&mut self,c:&mut recomp::rt::Cpu,cp:u8,o:u8,n:u8,m:u8,p:u8,v:u32) { self.0.mcr(c,cp,o,n,m,p,v) }
}
fn main() {
    for seed in 0..256u64 {
        for (entry, t0, t1) in [
            (0x02000000, f_boundary as recomp::mem::LiftedFn, f_boundary_t1 as recomp::mem::LiftedFn),
            (0x02000040, f_tail as recomp::mem::LiftedFn, f_tail_t1 as recomp::mem::LiftedFn),
        ] {
            let mut a = recomp::rt::Cpu::default();
            for r in 0..15 { a.r[r] = (seed as u32).wrapping_mul(0x9e3779b9).rotate_left(r as u32 * 2); }
            a.r[13] = 0x027e3000; a.r[14] = 0x02000401; a.budget = 1 << 22;
            a.n = seed & 1 != 0; a.z = seed & 2 != 0; a.c = seed & 4 != 0; a.v = seed & 8 != 0;
            let mut b = a.clone();
            let mut x = CheckBus(recomp::mem::Ram::new(&[],0x02000000,seed));
            let mut y = CheckBus(recomp::mem::Ram::new(&[],0x02000000,seed));
            let mut p = t0(&mut a,&mut x,entry);
            // T0 tail exits are dispatched by its caller rather than nested through Bus::call.
            if p == 0x02000100 { p = x.call(&mut a,p,0x02000401); }
            assert_eq!(p, t1(&mut b,&mut y,entry), "exit seed {seed}");
            a.budget = 0; b.budget = 0;
            assert_eq!(a.state(),b.state(), "state seed {seed}");
            assert_eq!(x.0.writes,y.0.writes, "writes seed {seed}");
            assert_eq!(x.0.events,y.0.events, "calls seed {seed}");
        }
    }
}
"#);
    compile_and_run(&src, &image, "boundary");
}

#[test]
fn boundary_rejects_bad_conventions_and_other_overlays_and_exact_timing() {
    use recomp::raise_emit::{set_boundary, set_current, take_facts, BoundarySig};
    let _lock = emit_lock();
    let _native = NativeTiming::on();
    let _boundary = BoundaryMode::on();
    let base = 0x0200_0000;
    // A callee that destroys r4; another preserves r4 but uses it as an implicit argument.
    for words in [
        &[0xE3A0_4007u32, 0xE12F_FF1E][..],
        &[0xE084_0001, 0xE12F_FF1E][..],
        // Passing an incoming saved value through a frame slot is still an implicit argument.
        &[0xE52D_4004, 0xE49D_0004, 0xE12F_FF1E][..],
    ] {
        let image: Vec<_> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
        let img = Image::new(base, &image);
        let f = discover(&img, base, Some((base, base + image.len() as u32)), 100).unwrap();
        set_current("arm9_main", None);
        emit_t1(&f, "bad", Some(&img)).unwrap();
        assert_ne!(take_facts().unwrap().preserved & 0x2FF0, 0x2FF0);
    }
    let words = [0xE92D_4000u32, 0xEB00_003D, 0xE8BD_8000];
    let image: Vec<_> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
    let img = Image::new(base, &image);
    let f = discover(&img, base, Some((base, base + 12)), 100).unwrap();
    for (seg, kept) in [("arm9_main", 0x2FE0), ("ov9_other", 0x2FF0)] {
        set_boundary([(base + 0x100, vec![(seg.into(), BoundarySig { params: 0x100F, preserved: kept })])].into_iter().collect());
        let code = emit_t1(&f, "fallback", Some(&img)).unwrap();
        assert!(code.contains("spill!(); let x = m.call"));
        assert!(!code.contains("checked dispatcher boundary"));
    }
    set_boundary([(base + 0x100, vec![("arm9_main".into(), BoundarySig { params: 0x100F, preserved: 0x2FF0 })])].into_iter().collect());
    recomp::lift::set_timing_native(false, &[]);
    let code = emit_t1(&f, "exact", Some(&img)).unwrap();
    assert!(code.contains("spill!(); let x = m.call"));
    assert!(!code.contains("checked dispatcher boundary"));
    unsafe { std::env::set_var("MKDS_BOUNDARY", "0"); }
    assert_eq!(code, emit_t1(&f, "exact", Some(&img)).unwrap(), "exact emission ignores the switch");
    recomp::lift::set_timing_native(true, &[]);
    assert!(!emit_t1(&f, "native_off", Some(&img)).unwrap().contains("checked dispatcher boundary"));
}

/// Every listed ARM9 function eligible for a direct variant made entirely of boundary calls.
/// The dispatcher stub consumes exactly the certified input mask and adversarially overwrites
/// all volatile registers. Unlike a constant-output stub, this detects missing argument spills.
#[test]
fn boundary_rom_direct_variants_match_their_translation() {
    use recomp::raise_emit::{set_boundary, set_current, take_direct, take_facts, BoundarySig, DirectSig};
    use std::collections::{BTreeMap, HashMap, HashSet};
    let Ok(path) = std::env::var("MKDS_ROM") else { eprintln!("MKDS_ROM not set: skipped"); return; };
    let _lock = emit_lock();
    let _native = NativeTiming::on();
    let _boundary = BoundaryMode::on();
    let rom = std::fs::read(path).unwrap();
    let mut images: BTreeMap<String, (u32, Vec<u8>)> = mkds_builder::extract::arm9_sections(&rom).unwrap()
        .into_iter().map(|s| (s.name, (s.ram_address, s.data))).collect();
    for s in mkds_builder::extract::arm9_overlays(&rom).unwrap() {
        images.insert(format!("ov9_{:03}", s.id), (s.ram_address, s.data));
    }
    let img_for = |seg: &str| -> Image<'_> {
        Image { segs: images.iter().filter(|(s, _)| if seg.starts_with("ov9_") { s.as_str() == seg } else { !s.starts_with("ov9_") })
            .map(|(_, (base, data))| (*base, data.as_slice())).collect() }
    };
    let csv = std::fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../dictionary/FUNCTION_BOUNDARIES.csv")).unwrap();
    let mut jobs = Vec::new();
    for b in build_support::boundaries(&csv).unwrap() {
        if !images.contains_key(&b.segment) { continue; }
        if let Ok(f) = discover(&img_for(&b.segment), b.entry | b.thumb as u32, Some((b.entry, b.entry + b.size)), 20_000) {
            jobs.push((b.segment, f));
        }
    }
    let mut excluded: HashSet<u32> = recomp::native::NATIVE.iter().chain(recomp::native::fs::NATIVES).map(|n| n.entry & !1).collect();
    excluded.extend(recomp::os_hle::SAFE_ENTRIES.iter().map(|e| e.0 & !1));
    excluded.extend(recomp::native::EVENT_ENTRIES.iter().map(|e| e & !1));
    excluded.extend([recomp::os_hle::OS_GET_TICK, 0x0200_DE50, 0x0200_E6EC, 0x0200_E738]);
    let mut conventions: HashMap<u32, Vec<(String, BoundarySig)>> = HashMap::new();
    for (seg, f) in &jobs {
        conventions.entry(f.entry).or_default().push((seg.clone(), BoundarySig {
            params: 0, preserved: if excluded.contains(&(f.entry & !1)) { 0 } else { 0x2FF0 },
        }));
    }
    let mut converged = false;
    for _ in 0..64 {
        set_boundary(conventions.clone());
        let mut next = conventions.clone();
        for (seg, f) in &jobs {
            set_current(seg, None);
            let facts = emit_t1(f, "proof", Some(&img_for(seg))).ok().and_then(|_| take_facts());
            let d = &mut next.get_mut(&f.entry).unwrap().iter_mut().find(|r| r.0 == *seg).unwrap().1;
            d.preserved &= facts.as_ref().map_or(0, |x| x.preserved);
            d.params |= facts.as_ref().map_or(0x100F, |x| x.boundary_params);
        }
        if next == conventions { converged = true; break; }
        conventions = next;
    }
    assert!(converged, "boundary conventions converge");
    set_boundary(conventions.clone());
    let resolved = |seg: &str, t: u32| -> Option<&BoundarySig> {
        let rows = conventions.get(&t)?;
        rows.iter().find(|(s, _)| s == seg).or_else(|| rows.iter().find(|(s, _)| !s.starts_with("ov9_")))
            .map(|r| &r.1).filter(|d| d.preserved & 0x2FF0 == 0x2FF0)
    };
    let mut checked = 0;
    for (seg, (base, image)) in &images {
        let candidates: Vec<_> = jobs.iter().filter(|(s, _)| s == seg).filter(|(_, f)| {
            if excluded.contains(&(f.entry & !1)) { return false; }
            set_current(seg, None);
            if emit_t1(f, "candidate", Some(&img_for(seg))).is_err() { return false; }
            let x = take_facts().unwrap();
            x.escape.is_none() && !x.calls.is_empty() && x.calls.iter().all(|&t| resolved(seg, t).is_some())
        }).collect();
        let params: String = conventions.keys().filter_map(|&t| resolved(seg, t).map(|d| format!("({t:#X}, {}),", d.params))).collect();
        for (batch, funcs) in candidates.chunks(100).enumerate() {
            let mut src = format!("#![allow(unused, non_snake_case)]\nmod direct {{ pub use super::*; }}\nstatic IMAGE: &[u8] = include_bytes!(\"image.bin\");\nstatic PARAMS: &[(u32,u16)] = &[{params}];\n");
            src.push_str(r#"
use recomp::rt::{Bus as _, Mem as _};
struct CheckBus(recomp::mem::Ram<'static>);
impl recomp::rt::Mem for CheckBus {
    fn r8(&mut self,a:u32)->u8 { self.0.r8(a) }
    fn r16(&mut self,a:u32)->u16 { self.0.r16(a) }
    fn r32(&mut self,a:u32)->u32 { self.0.r32(a) }
    fn w8(&mut self,a:u32,v:u8) { self.0.w8(a,v) }
    fn w16(&mut self,a:u32,v:u16) { self.0.w16(a,v) }
    fn w32(&mut self,a:u32,v:u32) { self.0.w32(a,v) }
}
impl recomp::rt::Bus for CheckBus {
    fn call(&mut self,c:&mut recomp::rt::Cpu,t:u32,ret:u32)->u32 {
        let params = PARAMS.iter().find(|r| r.0 == t).expect("certified boundary").1;
        let mut h = c.r[13].wrapping_add(c.r[14]);
        for r in [0,1,2,3,12] { if params >> r & 1 != 0 { h = h.rotate_left(7) ^ c.r[r]; } }
        let p = self.0.call(c,t,ret);
        for r in [0,1,2,3,12] { c.r[r] ^= h.rotate_left(r as u32); }
        p
    }
    fn swi(&mut self,c:&mut recomp::rt::Cpu,n:u32) { self.0.swi(c,n) }
    fn mrc(&mut self,c:&mut recomp::rt::Cpu,cp:u8,o:u8,n:u8,m:u8,p:u8)->u32 { self.0.mrc(c,cp,o,n,m,p) }
    fn mcr(&mut self,c:&mut recomp::rt::Cpu,cp:u8,o:u8,n:u8,m:u8,p:u8,v:u32) { self.0.mcr(c,cp,o,n,m,p,v) }
}
"#);
            let mut calls = String::new();
            for (_, f) in funcs {
                let name = format!("boundary_{:08x}", f.entry);
                // results as the generator gives them (sp is not one unless the function leaves it moved)
                let mut t1 = Err("");
                for rets in [0x5FFF, 0x7FFF] {
                    set_current(seg, Some(DirectSig { params: 0x7FFF, rets, name: format!("d_{name}"), cpu: true }));
                    t1 = emit_t1(f, &name, Some(&img_for(seg)));
                    if t1.is_ok() { break; }
                }
                let t1 = t1.unwrap();
                let direct = take_direct().unwrap();
                assert!(direct.contains("checked dispatcher boundary"));
                src.push_str(&emit_full(f, &name, Some(&img_for(seg)), None, true));
                src.push_str(&t1);
                src.push_str(&direct);
                src.push_str(&format!(r#"
fn check_{name}() {{
    for seed in 0..32u64 {{
        let mut a = recomp::rt::Cpu::default();
        for r in 0..15 {{ a.r[r] = (seed as u32).wrapping_mul(0x9e3779b9).rotate_left(r as u32 * 2) ^ r as u32; }}
        if seed % 3 == 0 {{ a.r[0] = 0; a.r[1] = 1; }}
        a.r[13] = 0x027e3000; a.r[14] = 0x0badf001; a.budget = 1 << 22;
        a.n = seed & 1 != 0; a.z = seed & 2 != 0; a.c = seed & 4 != 0; a.v = seed & 8 != 0;
        let mut b = a.clone();
        let mut x = CheckBus(recomp::mem::Ram::new(IMAGE,{base:#X},seed));
        let mut y = CheckBus(recomp::mem::Ram::new(IMAGE,{base:#X},seed));
        let mut p = f_{name}(&mut a,&mut x,{entry:#X});
        if PARAMS.iter().any(|r| r.0 == p) {{ let ret = a.r[14]; p = x.call(&mut a,p,ret); }}
        let q = f_{name}_t1(&mut b,&mut y,{entry:#X});
        assert_eq!(p,q,"{name} exit seed {{seed}}");
        a.budget = 0; b.budget = 0;
        assert_eq!(a.state(),b.state(),"{name} state seed {{seed}}");
        assert_eq!(x.0.writes,y.0.writes,"{name} writes seed {{seed}}");
        assert_eq!(x.0.events,y.0.events,"{name} calls seed {{seed}}");
    }}
}}
"#, entry=f.entry));
                calls.push_str(&format!("check_{name}();\n"));
                checked += 1;
            }
            src.push_str(&format!("fn main() {{ {calls} }}"));
            compile_and_run(&src, image, &format!("boundary_{seg}_{batch}"));
        }
    }
    assert!(checked > 0);
    eprintln!("boundary differential: {checked} ROM direct variants, 32 states each (all eligible listed ARM9 images)");
}

#[test]
fn constant_veneer_chain_calls_directly_and_matches_translation() {
    use recomp::raise_emit::{set_current, set_direct, take_direct, take_facts, DirectSig};
    let _lock = emit_lock();
    let _native = NativeTiming::on();
    let base = 0x0200_0000;
    // ldr r3, [pc]; bx r3; .word veneer
    // ldr ip, [pc]; bx ip; .word leaf
    // Thumb: add r0, #1; bx lr (also checks interworking and the Thumb bit).
    let words: [u32; 8] = [0xE59F_3000, 0xE12F_FF13, base + 12,
        0xE59F_C000, 0xE12F_FF1C, base + 25, 0x4770_3001, 0];
    let image: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
    let img = Image::new(base, &image);
    let entries = [base, base + 12, base + 25];
    let funcs: Vec<_> = entries.iter().map(|&e| discover(&img, e, Some((e & !1, (e & !1) + if e & 1 != 0 { 4 } else { 8 })), 100).unwrap()).collect();
    // Deliberately return every register: this also checks the scratch register
    // loaded by each veneer survives the tail call.
    let sigs: Vec<_> = (0..3).map(|i| DirectSig { cpu: true, params: 0x7fff, rets: 0x7fff, name: format!("d_v{i}") }).collect();
    set_direct(entries.iter().zip(&sigs).map(|(&e, d)| (e, vec![("arm9_main".into(), d.clone())])).collect());
    let mut src = String::from("#![allow(unused)]\nmod direct { pub use super::*; }\n");
    for i in 0..3 {
        set_current("arm9_main", Some(sigs[i].clone()));
        let t1 = emit_t1(&funcs[i], &format!("v{i}"), Some(&img)).unwrap();
        let facts = take_facts().unwrap();
        assert_eq!(facts.escape, None);
        assert_eq!(facts.calls, if i < 2 { vec![entries[i + 1]] } else { vec![] });
        let d = take_direct().unwrap();
        assert!(!d.contains("spill!();"));
        if i < 2 { assert!(d.contains(&format!("d_v{}(c, m", i + 1))); }
        src.push_str(&emit_full(&funcs[i], &format!("v{i}"), Some(&img), None, true));
        src.push_str(&t1);
        src.push_str(&d);
    }
    src.push_str(r#"
fn main() {
    for seed in 0..200u64 {
        let mut a = recomp::rt::Cpu::default();
        for r in 0..15 { a.r[r] = (seed as u32).wrapping_mul(0x9e3779b9).rotate_left(r as u32); }
        a.r[14] = 0x02000401; a.budget = 1 << 22;
        let mut b = a.clone();
        let mut x = recomp::mem::Ram::new(&[], 0x02000000, seed);
        let mut y = recomp::mem::Ram::new(&[], 0x02000000, seed);
        let p = f_v0(&mut a, &mut x, 0x02000000);
        assert_eq!(p, 0x0200000c);
        let p = f_v1(&mut a, &mut x, p);
        assert_eq!(p, 0x02000019);
        let p = f_v2(&mut a, &mut x, p);
        let q = f_v0_t1(&mut b, &mut y, 0x02000000);
        assert_eq!(p, q);
        a.budget = 0; b.budget = 0;
        assert_eq!(a.state(), b.state());
        assert_eq!(x.writes, y.writes);
        assert_eq!(x.events, y.events);
    }
}
"#);
    compile_and_run(&src, &image, "veneer_chain");
}

#[test]
fn veneer_proof_keeps_unproven_and_overlay_targets_on_dispatcher() {
    use recomp::raise_emit::{set_current, set_direct, take_facts, DirectSig};
    let _lock = emit_lock();
    let _native = NativeTiming::on();
    let base = 0x0200_0000;
    // A conditional overwrite must invalidate the literal even on the other path.
    let words: [u32; 4] = [0xE59F_3004, 0x11A0_3000, 0xE12F_FF13, base + 0x101];
    let image: Vec<_> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
    let img = Image::new(base, &image);
    let f = discover(&img, base, Some((base, base + 12)), 100).unwrap();
    set_current("arm9_main", None);
    let code = emit_t1(&f, "conditional_veneer", Some(&img)).unwrap();
    let facts = take_facts().unwrap();
    assert!(facts.escape.is_some());
    assert!(facts.calls.is_empty());
    assert!(code.contains("exit!(iw(r3))"));

    let words: [u32; 3] = [0xE59F_C000, 0xE12F_FF1C, base + 0x101];
    let image: Vec<_> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
    let img = Image::new(base, &image);
    let f = discover(&img, base, Some((base, base + 8)), 100).unwrap();
    set_direct([(base + 0x101, vec![("ov9_other".into(), DirectSig {
        cpu: true, params: 1 << 14, rets: 1, name: "unavailable".into(),
    })])].into_iter().collect());
    let code = emit_t1(&f, "overlay_veneer", Some(&img)).unwrap();
    let facts = take_facts().unwrap();
    assert_eq!(facts.calls, vec![base + 0x101]);
    assert!(code.contains("exit!(iw(r12))"));
    assert!(!code.contains("direct::unavailable"));
}

/// No-spill checks: T1 emitted without spills (block transfers on the locals, `msr` syncing only the banked
/// registers, no budget lines) and, for closed functions that call nothing, the direct variant (registers as
/// arguments and results) behind its T1 wrapper, against the translation with a budget so large no slice
/// ends: same exit pc, CPU state, memory writes and calls. Returns (source, raised, direct).
fn build_nospill_checks(funcs: &[(String, u32, Func)], image: &[u8], base: u32, seeds: u32) -> (String, usize, usize) {
    build_nospill_checks_flags(funcs, image, base, seeds, None)
}

fn build_nospill_checks_flags(funcs: &[(String, u32, Func)], image: &[u8], base: u32, seeds: u32, returns: Option<u8>) -> (String, usize, usize) {
    build_nospill_checks_context(funcs, image, base, seeds, returns, None)
}

fn build_nospill_checks_context(funcs: &[(String, u32, Func)], image: &[u8], base: u32, seeds: u32, returns: Option<u8>, context: Option<recomp::raise::FlagContext>) -> (String, usize, usize) {
    use recomp::raise_emit::{set_current, set_direct, take_direct, take_facts, DirectSig};
    let _lock = emit_lock();
    let _native = NativeTiming::on();
    // Every emitter test holds EMIT; restore both globals before releasing it.
    struct FlagMode(Option<std::ffi::OsString>);
    impl Drop for FlagMode {
        fn drop(&mut self) {
            recomp::raise::set_flag_context(None);
            unsafe { match &self.0 { Some(v) => std::env::set_var("MKDS_FLAG_LOCALS", v), None => std::env::remove_var("MKDS_FLAG_LOCALS") } }
        }
    }
    let _mode = returns.map(|mask| {
        let saved = FlagMode(std::env::var_os("MKDS_FLAG_LOCALS"));
        unsafe { std::env::set_var("MKDS_FLAG_LOCALS", "1"); }
        recomp::raise::set_flag_context(Some(context.clone().unwrap_or_else(|| recomp::raise::FlagContext {
            ret: funcs.iter().map(|(_, e, _)| (e & !1, mask)).collect(),
            ..Default::default()
        })));
        saved
    });
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
                if x.escape.is_none() && (x.calls.is_empty() || returns.is_some() && x.calls.iter().all(|t| funcs.iter().any(|(_, e, _)| e == t))) {
                    sigs.insert(*entry, DirectSig { cpu: true, params: x.live_in, rets: x.written, name: format!("d_{name}") });
                }
            }
        }
    }
    set_direct(sigs.iter().map(|(&e, d)| (e, vec![("arm9_main".to_string(), d.clone())])).collect());
    if recomp::raise_emit::flag_locals_enabled() {
        for (name, entry, f) in funcs {
            let Some(d) = sigs.get_mut(entry) else { continue };
            set_current("arm9_main", Some(d.clone()));
            emit_t1_full(f, name, Some(&img), &notes, None).unwrap();
            d.cpu = take_facts().unwrap().cpu;
            let _ = take_direct();
        }
        set_direct(sigs.iter().map(|(&e, d)| (e, vec![("arm9_main".to_string(), d.clone())])).collect());
    }
    let mut src = String::from("#![allow(non_upper_case_globals, unused)]\nstatic IMAGE: &[u8] = include_bytes!(\"image.bin\");\nmod direct { pub use super::*; }\n");
    // A direct call executes the guest leaf, whereas Ram::call deliberately
    // simulates an unknown callee with random clobbers. Give T0 the same real
    // leaves; otherwise larger samples fail when they contain both a caller
    // and its direct callee. External calls still use the adversarial stub.
    let leaf_cases: String = funcs.iter().filter(|(_, e, _)| sigs.contains_key(e))
        .map(|(name, e, _)| format!("{e:#X} => return f_{name}(c, self, target),\n")).collect();
    src.push_str(&format!(r#"
use recomp::rt::{{Bus as _, Mem as _}};
struct CheckBus(recomp::mem::Ram<'static>);
impl recomp::rt::Mem for CheckBus {{
    fn r8(&mut self, a:u32)->u8 {{ self.0.r8(a) }}
    fn r16(&mut self, a:u32)->u16 {{ self.0.r16(a) }}
    fn r32(&mut self, a:u32)->u32 {{ self.0.r32(a) }}
    fn w8(&mut self, a:u32,v:u8) {{ self.0.w8(a,v) }}
    fn w16(&mut self, a:u32,v:u16) {{ self.0.w16(a,v) }}
    fn w32(&mut self, a:u32,v:u32) {{ self.0.w32(a,v) }}
}}
impl recomp::rt::Bus for CheckBus {{
    fn call(&mut self,c:&mut recomp::rt::Cpu,target:u32,ret:u32)->u32 {{
        match target {{ {leaf_cases} _ => self.0.call(c,target,ret) }}
    }}
    fn swi(&mut self,c:&mut recomp::rt::Cpu,n:u32) {{ self.0.swi(c,n) }}
    fn mrc(&mut self,c:&mut recomp::rt::Cpu,cp:u8,op1:u8,crn:u8,crm:u8,op2:u8)->u32 {{ self.0.mrc(c,cp,op1,crn,crm,op2) }}
    fn mcr(&mut self,c:&mut recomp::rt::Cpu,cp:u8,op1:u8,crn:u8,crm:u8,op2:u8,v:u32) {{ self.0.mcr(c,cp,op1,crn,crm,op2,v) }}
}}
"#));
    let mut checks = String::new();
    let mut check_calls = String::new();
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
        let return_mask = context.as_ref().map_or(returns.unwrap_or(15), |ctx|
            ctx.ret.get(&(entry & !1)).copied().unwrap_or(15)
                | if ctx.taken.contains(&(entry & !1)) { ctx.any_ret } else { 0 });
        checks.push_str(&format!(
            r#"
#[inline(never)]
fn check_{name}() {{
    for seed in 0..{seeds}u64 {{
        let mut a = recomp::rt::Cpu::default();
        a.budget = 1 << 22;
        for i in 0..13 {{ a.r[i] = (seed as u32).wrapping_mul(0x9E37_79B1).rotate_left(i as u32 * 5) ^ (i as u32 * 0x0101_0101); }}
        if seed % 3 == 0 {{ a.r[0] = 0; a.r[1] = 1; }}
        a.r[13] = 0x027E_3000; a.r[14] = 0x0200_0401;
        a.n = seed & 1 != 0; a.z = seed & 2 != 0; a.c = seed & 4 != 0; a.v = seed & 8 != 0;
        let mut b = a.clone();
        let mut x = CheckBus(recomp::mem::Ram::new(IMAGE, {base:#X}, seed));
        let mut y = CheckBus(recomp::mem::Ram::new(IMAGE, {base:#X}, seed));
        let p = f_{name}(&mut a, &mut x, {entry:#X});
        let q = f_{name}_t1(&mut b, &mut y, {entry:#X});
        // a long loop can still run out (polls and budgets charge differently): no common end to compare
        if p == recomp::rt::BUDGET_EXHAUSTED || q == recomp::rt::BUDGET_EXHAUSTED {{ continue; }}
        assert_eq!(p, q, "{name} exit, seed {{seed}}");
        a.budget = 0; b.budget = 0;
        // The test's whole-program context explicitly marks other return flags dead.
        if {return_mask} & 1 == 0 {{ a.n = b.n; }}
        if {return_mask} & 2 == 0 {{ a.z = b.z; }}
        if {return_mask} & 4 == 0 {{ a.c = b.c; }}
        if {return_mask} & 8 == 0 {{ a.v = b.v; }}
        assert_eq!(a.state(), b.state(), "{name} state, seed {{seed}}");
        assert_eq!(x.0.writes, y.0.writes, "{name} writes, seed {{seed}}");
        assert_eq!(x.0.events, y.0.events, "{name} calls, seed {{seed}}");
    }}
}}"#
        ));
        check_calls.push_str(&format!("check_{name}();\n"));
    }
    src.push_str(&checks);
    src.push_str(&format!("fn main() {{{check_calls}\n    println!(\"ok\");\n}}\n"));
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
    // push {lr}; bl arm_leaf; pop {pc}. Both sides must execute the real
    // leaf, rather than comparing a direct call against Ram's call stub.
    image.resize(0x70, 0);
    image.extend([0xE92D_4000u32, 0xEBFF_FFF1, 0xE8BD_8000].iter().flat_map(|w| w.to_le_bytes()));
    let img = Image::new(base, &image);
    let funcs = vec![
        ("thumb_loop".to_string(), base + 1, discover(&img, base + 1, Some((base, base + 14)), 100).unwrap()),
        ("arm_if".to_string(), base + 0x20, discover(&img, base + 0x20, Some((base + 0x20, base + 0x38)), 100).unwrap()),
        ("arm_leaf".to_string(), base + 0x40, discover(&img, base + 0x40, Some((base + 0x40, base + 0x60)), 100).unwrap()),
        ("arm_caller".to_string(), base + 0x70, discover(&img, base + 0x70, Some((base + 0x70, base + 0x7c)), 100).unwrap()),
    ];
    let (src, raised, direct) = build_nospill_checks(&funcs, &image, base, 200);
    assert_eq!(raised, 4, "all four are raised");
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
    let csv = std::fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../dictionary/FUNCTION_BOUNDARIES.csv")).unwrap();
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

/// Carry chains, signed conditions, shifter carry, and long/half multiplies from
/// random states and arithmetic edge values. Test both observable and dead returns.
#[test]
fn direct_flag_locals_preserve_arithmetic_and_drop_cpu_when_flags_do_not_escape() {
    let base = 0x0200_0000u32;
    let mut image = Vec::new();
    let mut ranges = Vec::new();
    let mut add = |name: &str, words: &[u32]| {
        let entry = base + image.len() as u32;
        image.extend(words.iter().flat_map(|w| w.to_le_bytes()));
        ranges.push((name.to_string(), entry, base + image.len() as u32));
    };
    // adds; movlt; movge; adcs; sbcs; movs ror; adc; bx lr
    add("arith_flags", &[0xE0900001, 0xB3A06001, 0xA3A06002, 0xE0B11002,
        0xE0D22003, 0xE1B030E3, 0xE0A44005, 0xE12FFF1E]);
    // Initial carry must come from the caller, including zero-minus-borrow.
    add("incoming_carry", &[0xE0D00001, 0xE0B11002, 0xE12FFF1E]);
    for signed in [false, true] {
        for accumulate in [false, true] {
            let mul = 0xE0910392 | ((signed as u32) << 22) | ((accumulate as u32) << 21);
            add(&format!("long_{signed}_{accumulate}"), &[mul, 0x03A04001, 0x43A05001, 0xE12FFF1E]);
        }
    }
    // SMULBB, SMULWT, SMLABB (the last keeps its sticky Q helper).
    add("half_products", &[0xE1600281, 0xE12305E4, 0xE1069887, 0xE12FFF1E]);
    // Conditional definitions at a join must keep incoming flags on the untaken path.
    add("conditional_flags", &[0x00900001, 0xE0A22003, 0xE12FFF1E]);
    let img = Image::new(base, &image);
    let funcs: Vec<_> = ranges.into_iter().map(|(name, e, end)|
        (name, e, discover(&img, e, Some((e, end)), 100).unwrap())).collect();
    for mask in [15, 0] {
        let (mut src, raised, direct) = build_nospill_checks_flags(&funcs, &image, base, 2048, Some(mask));
        assert_eq!((raised, direct), (funcs.len(), funcs.len()));
        assert!(src.contains("overflowing_add"));
        assert!(src.contains("overflowing_sub"));
        assert!(src.contains("__flag_product"));
        assert!(src.contains("R::half_mul(c, recomp::arm::HalfMul::Smla"), "sticky Q stays observable");
        if mask == 0 {
            assert!(src.contains("pub fn d_arith_flags(m:"), "internal flags need no CPU");
            assert!(src.contains("pub fn d_incoming_carry(c:"), "incoming carry keeps the CPU");
        } else {
            assert!(src.contains("c.n = flag_n;"), "observable return flags are materialized");
        }
        src = src.replace("if seed % 3 == 0 { a.r[0] = 0; a.r[1] = 1; }", r#"
        if seed < 256 {
            let edge = [0, 1, u32::MAX, 0x7fffffff, 0x80000000, 0xffff, 0xffff0000, 2];
            for i in 0..13 { a.r[i] = edge[((seed as usize / 16) + i) % edge.len()]; }
        }"#);
        compile_and_run(&src, &image, &format!("direct_flags_{mask}"));
    }
}

#[test]
fn direct_flag_locals_synchronize_observable_calls_and_keep_locals_across_cpu_free_calls() {
    let base = 0x0200_0000u32;
    // push {lr}; adds; bl pure; adcs; bl flags; adcs; pop {lr}; bx lr.
    // The first callee has no CPU argument and preserves local flags. The second
    // reads the carry and returns flags the caller uses, requiring a boundary sync.
    let image: Vec<u8> = [0xE92D4000u32, 0xE0900001, 0xEB000004, 0xE0B11002,
        0xEB000004, 0xE0B22003, 0xE8BD4000, 0xE12FFF1E,
        0xE0800001, 0xE12FFF1E, 0xE0D00001, 0xE12FFF1E]
        .iter().flat_map(|w| w.to_le_bytes()).collect();
    let img = Image::new(base, &image);
    let funcs: Vec<_> = [("call_flags", 0, 32), ("pure", 32, 40), ("return_flags", 40, 48)]
        .into_iter().map(|(name, start, end)| (name.to_string(), base + start,
            discover(&img, base + start, Some((base + start, base + end)), 100).unwrap())).collect();
    let (src, raised, direct) = build_nospill_checks_flags(&funcs, &image, base, 2048, Some(15));
    assert_eq!((raised, direct), (3, 3));
    assert!(src.contains("pub fn d_pure(m:"));
    assert!(src.contains("d_pure(m,"));
    assert!(src.contains("flag_c = c.c;"));
    compile_and_run(&src, &image, "direct_flag_calls");
}

/// A register-dump callee (`stmia r0, {r4-r11}; bx lr`) reads r4-r11 as implicit arguments through a
/// block store with a non-sp base: it must not be certified for a boundary call. `push {r4-r11}; pop`
/// (sp base) still is.
#[test]
fn boundary_rejects_register_dump_through_block_store() {
    use recomp::raise_emit::{set_current, take_facts};
    let _lock = emit_lock();
    let _native = NativeTiming::on();
    let _boundary = BoundaryMode::on();
    let base = 0x0200_0000;
    let words: [u32; 4] = [0xE880_0FF0, 0xE12F_FF1E, 0xE92D_0FF0, 0xE8BD_0FF0];
    let mut image: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
    image.extend(0xE12F_FF1Eu32.to_le_bytes());
    let img = Image::new(base, &image);
    let preserved = |entry: u32, end: u32| {
        let f = discover(&img, entry, Some((entry, end)), 100).unwrap();
        set_current("arm9_main", None);
        emit_t1(&f, "x", Some(&img)).unwrap();
        take_facts().unwrap().preserved
    };
    assert_ne!(preserved(base, base + 8), 0x2FF0, "register dump must not be certified");
    assert_eq!(preserved(base + 8, base + 20), 0x2FF0, "sp-based save/restore stays certified");
}

#[test]
fn direct_flag_locals_keep_flags_read_by_an_open_caller() {
    use recomp::raise::{self, FlagContext};
    let base = 0x0200_0000u32;
    // push {lr}; bl leaf; movcs/movcc r2; a one-iteration loop; pop {lr}; bx lr.
    // The loop excludes the caller from the closed/direct set. It observes the
    // leaf's carry immediately after the call, before overwriting its own flags.
    let mut image: Vec<u8> = [0xE92D4000u32, 0xEB000009, 0x23A02001, 0x33A02000,
        0xE3A03001, 0xE2533001, 0x1AFFFFFD, 0xE8BD4000, 0xE12FFF1E]
        .iter().flat_map(|w| w.to_le_bytes()).collect();
    image.resize(0x30, 0);
    image.extend([0xE0900001u32, 0xE12FFF1E].iter().flat_map(|w| w.to_le_bytes()));
    let img = Image::new(base, &image);
    let funcs: Vec<_> = [("open_flag_caller", 0, 36), ("flag_leaf", 0x30, 0x38)]
        .into_iter().map(|(name, start, end)| (name.to_string(), base + start,
            discover(&img, base + start, Some((base + start, base + end)), 100).unwrap())).collect();
    let mut ctx = FlagContext {
        entry: funcs.iter().flat_map(|(_, _, f)| f.blocks.iter().map(|&b| (b, 0))).collect(),
        ret: funcs.iter().map(|(_, e, _)| (*e, 0)).collect(),
        ..Default::default()
    };
    {
        let _lock = emit_lock();
        raise::set_flag_context(Some(ctx.clone()));
        let (_, calls, _, _) = raise::flag_facts(&funcs[0].2, Some(&img));
        raise::set_flag_context(None);
        let carry = calls.iter().find(|c| c.0 == Some(base + 0x30)).unwrap().1;
        assert_eq!(carry, raise::C);
        ctx.ret.insert(base + 0x30, carry);
    }
    let (src, raised, direct) = build_nospill_checks_context(&funcs, &image, base, 2048, Some(0), Some(ctx));
    assert_eq!((raised, direct), (2, 1));
    assert!(src.contains("pub fn d_flag_leaf(c:"));
    assert!(src.contains("c.c = flag_c;"));
    assert!(src.contains("direct::d_flag_leaf(c, m"));
    assert!(!src.contains("pub fn d_open_flag_caller("));
    compile_and_run(&src, &image, "direct_flags_open_caller");
}

#[test]
fn direct_flag_locals_keep_cpu_flag_emission_for_computed_targets() {
    use recomp::raise::FlagContext;
    let base = 0x0200_0000u32;
    // adds; adc (carry is locally live); bx lr. No caller-observed
    // return flags are known, but the stored pointer keeps this target guarded.
    let image: Vec<u8> = [0xE0900001u32, 0xE0A22003, 0xE12FFF1E]
        .iter().flat_map(|w| w.to_le_bytes()).collect();
    let img = Image::new(base, &image);
    let funcs = vec![("computed_flags".to_string(), base,
        discover(&img, base, Some((base, base + 12)), 100).unwrap())];
    let ctx = FlagContext {
        ret: [(base, 0)].into_iter().collect(),
        taken: [base].into_iter().collect(),
        any_ret: 0,
        ..Default::default()
    };
    let (src, raised, direct) = build_nospill_checks_context(&funcs, &image, base, 2048, Some(0), Some(ctx));
    assert_eq!((raised, direct), (1, 1));
    assert!(src.contains("pub fn d_computed_flags(c:"));
    assert!(src.contains("R::adds(c,"));
    assert!(!src.contains("let mut flag_c"));
    compile_and_run(&src, &image, "direct_flags_computed_target");
}

#[test]
fn native_fall_through_is_not_closed() {
    use recomp::raise_emit::{set_current, take_direct, take_facts};
    let _lock = emit_lock();
    let _native = NativeTiming::on();
    let base = 0x0200_0000;
    // A flag write at the boundary, without any explicit return statement.
    let image: Vec<u8> = [0xE0900001u32].iter().flat_map(|w| w.to_le_bytes()).collect();
    let img = Image::new(base, &image);
    let f = discover(&img, base, Some((base, base + 4)), 100).unwrap();
    set_current("arm9_main", None);
    let _ = take_direct();
    let code = emit_t1(&f, "fall_off", Some(&img)).unwrap();
    assert_eq!(take_facts().unwrap().escape, Some("falls through"));
    assert!(take_direct().is_none());
    assert!(code.contains("exit!(0x2000004)"));
}

#[test]
fn flag_locals_default_to_native_with_an_explicit_opt_out() {
    let _lock = emit_lock();
    struct Restore(Option<std::ffi::OsString>);
    impl Drop for Restore {
        fn drop(&mut self) {
            unsafe { match &self.0 { Some(v) => std::env::set_var("MKDS_FLAG_LOCALS", v), None => std::env::remove_var("MKDS_FLAG_LOCALS") } }
        }
    }
    let _restore = Restore(std::env::var_os("MKDS_FLAG_LOCALS"));
    let _native = NativeTiming::on();
    unsafe { std::env::remove_var("MKDS_FLAG_LOCALS"); }
    assert!(recomp::raise_emit::flag_locals_enabled());
    unsafe { std::env::set_var("MKDS_FLAG_LOCALS", "0"); }
    assert!(!recomp::raise_emit::flag_locals_enabled());
    unsafe { std::env::set_var("MKDS_FLAG_LOCALS", "1"); }
    recomp::lift::set_timing_native(false, &[]);
    assert!(!recomp::raise_emit::flag_locals_enabled(), "exact timing stays unchanged");
}
