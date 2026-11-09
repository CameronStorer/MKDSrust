//! ROM-backed leaf checks. No guest code or data is embedded in this test.
#![cfg(feature = "interpreter")]
use recomp::{ds_mem, interp, lift, mem::Ram, native, rt::{self, Cpu, Mem}};

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 as u32
    }
}

// Deterministic sparse memory plus the production divider/sqrt model. Record hardware
// transaction widths/order too: equal final RAM alone would miss device side effects.
struct Bus<'a> {
    ram: Ram<'a>,
    hw: &'a mut ds_mem::Bus,
    io: Vec<(u32, u32, u32)>,
    busy_reads: u32,
}
fn hardware(a: u32) -> bool { (0x0400_0280..0x0400_02c0).contains(&a) }
impl Mem for Bus<'_> {
    fn r8(&mut self, a: u32) -> u8 {
        if !hardware(a) { return self.ram.r8(a); }
        let v = self.hw.r8(a); self.io.push((a, 0x101, v as u32)); v
    }
    fn r16(&mut self, a: u32) -> u16 {
        if !hardware(a) { return self.ram.r16(a); }
        let mut v = self.hw.r16(a);
        // The production model completes synchronously. Also exercise finite busy polling.
        if matches!(a, 0x0400_0280 | 0x0400_02b0) && self.busy_reads > 0 {
            self.busy_reads -= 1;
            v |= 0x8000;
        }
        self.io.push((a, 0x102, v as u32)); v
    }
    fn r32(&mut self, a: u32) -> u32 {
        if !hardware(a) { return self.ram.r32(a); }
        let v = self.hw.r32(a); self.io.push((a, 0x104, v)); v
    }
    fn w8(&mut self, a: u32, v: u8) {
        if hardware(a) { self.io.push((a, 1, v as u32)); self.hw.w8(a, v); } else { self.ram.w8(a, v); }
    }
    fn w16(&mut self, a: u32, v: u16) {
        if hardware(a) { self.io.push((a, 2, v as u32)); self.hw.w16(a, v); } else { self.ram.w16(a, v); }
    }
    fn w32(&mut self, a: u32, v: u32) {
        if hardware(a) { self.io.push((a, 4, v)); self.hw.w32(a, v); } else { self.ram.w32(a, v); }
    }
}
impl rt::Bus for Bus<'_> {
    fn call(&mut self, _: &mut Cpu, _: u32, _: u32) -> u32 { panic!("native candidate is not a leaf") }
    fn swi(&mut self, _: &mut Cpu, _: u32) { panic!("unexpected BIOS call") }
    fn mrc(&mut self, _: &mut Cpu, _: u8, _: u8, _: u8, _: u8, _: u8) -> u32 { panic!("unexpected coprocessor read") }
    fn mcr(&mut self, _: &mut Cpu, _: u8, _: u8, _: u8, _: u8, _: u8, _: u32) { panic!("unexpected coprocessor write") }
}

#[test]
fn sdk_leaves_match_rom() {
    let Some(path) = std::env::var_os("MKDS_ROM") else {
        eprintln!("native SDK differential checks skipped: set MKDS_ROM to your ROM");
        return;
    };
    let rom = std::fs::read(path).expect("read MKDS_ROM");
    let sections = mkds_builder::extract::arm9_sections(&rom).unwrap();
    let mut rng = Rng(0x615c_d093_32ef_1234);
    let (mut hw1, mut hw2) = (ds_mem::Bus::new(), ds_mem::Bus::new());
    for n in native::NATIVE.iter().rev() {
        let section = sections.iter().find(|s| s.name == n.segment).expect("native segment");
        let img = lift::Image::new(section.ram_address, &section.data);
        let f = lift::discover(&img, n.entry, None, 4096).expect("discover original leaf");
        assert!(f.undecoded.is_empty(), "{}: undecoded original", n.name);
        assert!(f.insts.values().all(|d| !d.inst.is_call()), "{}: original calls guest code", n.name);
        let blocks: Vec<_> = f.blocks.iter().copied().collect();
        let insts: Vec<_> = f.insts.keys().copied().collect();
        for case in 0..4096 {
            let mut start = Cpu::default();
            for r in &mut start.r[..15] { *r = rng.next(); }
            start.r[13] = 0x023f_8000;
            start.r[14] = 0x0100_0000 | (case & 1);
            start.n = case & 1 != 0; start.z = case & 2 != 0;
            start.c = case & 4 != 0; start.v = case & 8 != 0; start.q = case & 16 != 0;
            start.budget = 100_000;
            let edges = [0, 1, u32::MAX, 0x8000_0000, 0x7fff_ffff, 4096, 0xffff_f000, 2];
            if case < 64 { start.r[0] = edges[case as usize % 8]; start.r[1] = edges[case as usize / 8]; }
            if n.name.starts_with("MI") {
                let src = match case % 8 { 0 => 0xffff_ffc0, 1 => 0x7fff_ffc0, _ => 0x0230_1000 } + (case & 3);
                let dst = match case % 6 { 0 => src, 1 => src.wrapping_add(2), 2 => src.wrapping_sub(2), _ => 0x0230_2000 + (case & 3) };
                let size = if case < 256 { case } else { rng.next() % 513 };
                start.r[2] = size;
                if case >= 4094 && matches!(n.name, "MIi_CpuClear16" | "MIi_CpuCopy16") {
                    start.r[2] = if case == 4094 { 0x8000_0000 } else { u32::MAX };
                }
                if n.name.contains("Copy") { start.r[0] = src; start.r[1] = dst; }
                else if n.name == "MI_CpuFill8" { start.r[0] = dst; start.r[1] &= 255; }
                else { start.r[1] = dst; }
            }
            // Math pilot: exercise unaligned/overlapping vector storage, wrapped addresses,
            // and arguments in the saved stack frame, in addition to arbitrary addresses.
            if matches!(n.entry, 0x0214_848c | 0x0203_9828 | 0x0203_97a8 | 0x0214_7f50) {
                if case < 256 {
                    start.r[0] = [0x0230_1000, 0xffff_fffc, start.r[13] - 8][case as usize % 3] + (case & 3);
                    start.r[1] = if case & 4 == 0 { start.r[0].wrapping_add(4) } else { 0x0230_2000 + (case & 3) };
                    start.r[2] = [start.r[0], start.r[1], start.r[0].wrapping_sub(4), 0x0230_3000][case as usize % 4];
                    // Output may alias inputs but must not overwrite the callee saved-register area.
                    if (start.r[13] - 64..start.r[13]).contains(&start.r[2]) { start.r[2] = 0x0230_3000; }
                }
            }
            if n.name == "strlen" { start.r[0] = if case % 8 == 0 { 0xffff_ffc0 } else { 0x0230_1000 + (case & 3) }; }
            let seed = rng.next() as u64;
            // Prior hardware state matters for asynchronous and result-only functions.
            for a in (0x0400_0280..0x0400_02c0).step_by(4) {
                let v = match a { 0x0400_0280 => rng.next() & 3, 0x0400_02b0 => rng.next() & 1, _ => rng.next() };
                hw1.w32(a, v); hw2.w32(a, v);
            }
            let mut a = Bus { ram: Ram::new(&section.data, section.ram_address, seed), hw: &mut hw1, io: vec![], busy_reads: case % 4 };
            let mut b = Bus { ram: Ram::new(&section.data, section.ram_address, seed), hw: &mut hw2, io: vec![], busy_reads: case % 4 };
            if n.name == "strlen" {
                let len = case % 513;
                for i in 0..=len {
                    let v = if i == len { 0 } else { (rng.next() % 255 + 1) as u8 };
                    a.w8(start.r[0].wrapping_add(i), v); b.w8(start.r[0].wrapping_add(i), v);
                }
            }
            let (mut c1, mut c2) = (start.clone(), start.clone());
            let e1 = interp::run(&mut c1, &mut a, &blocks, &insts, n.entry);
            assert_ne!(e1, rt::BUDGET_EXHAUSTED, "{} case {case} exhausted", n.name);
            let e2 = (n.f)(&mut c2, &mut b, n.entry);
            assert_eq!(e1, e2, "{} case {case}: return", n.name);
            assert_eq!(&c1.r[..15], &c2.r[..15], "{} case {case}: registers", n.name);
            assert_eq!(c1.cpsr(), c2.cpsr(), "{} case {case}: flags", n.name);
            // Only the dead stack frame is excluded, not every address numerically below sp.
            let live = |addr: &u32, _: &mut u8| !((start.r[13] - 256)..start.r[13]).contains(addr);
            a.ram.writes.retain(live); b.ram.writes.retain(live);
            assert_eq!(a.ram.writes, b.ram.writes, "{} case {case}: memory", n.name);
            assert_eq!(a.io, b.io, "{} case {case}: hardware transactions", n.name);
            for port in (0x0400_0280..0x0400_02c0).step_by(4) {
                assert_eq!(a.r32(port), b.r32(port), "{} case {case}: hardware state", n.name);
            }
        }
        eprintln!("{}: 4096 differential cases passed", n.name);
    }
}

#[test]
fn exhausted_budget_has_no_side_effects() {
    for n in native::NATIVE.iter().rev() {
        let mut c = Cpu { budget: 0, ..Cpu::default() };
        let mut m = Ram::new(&[], 0, 1);
        assert_eq!((n.f)(&mut c, &mut m, n.entry), rt::BUDGET_EXHAUSTED);
        assert_eq!(c.r[15], n.entry);
        c.r[15] = 0;
        assert_eq!(c.state(), Cpu::default().state());
        assert!(m.writes.is_empty() && m.events.is_empty());
    }
}
