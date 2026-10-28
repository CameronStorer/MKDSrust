//! Shadow checker for native functions (block 3, `https://github.com/CameronStorer/MKDSrust-Docs`): every time the game reaches a
//! native function, the exact translation runs first with a write journal, its writes are rolled back, the
//! native runs for real, and the two are compared: the return pc, r0-r3, r12, sp, lr, the flags, the final
//! value of every memory byte written outside the stack below the entry sp, and the order of math-unit
//! (divider / square root) register writes. A whole recorded race with no mismatch is what promotes a
//! native from on trial to verified.
//!
//! Only leaves are shadowed (the contract of [`crate::native`]); hardware writes other than the math unit
//! cannot be rolled back, so a translation that makes one is reported as a mismatch rather than run twice.
use crate::mem::LiftedFn;
use crate::native::Native;
use crate::rt::{Bus, Cpu, Mem};
use std::collections::{BTreeMap, HashMap};

/// Bytes below the entry sp that are the function's own stack frame (saved registers): not compared.
const STACK_FRAME: u32 = 1024;
const MATH: std::ops::Range<u32> = 0x0400_0280..0x0400_02c0;

/// Hardware registers that behave like memory (writing the same value twice changes nothing): the 2D
/// engines' registers and the 3D engine's tables (edge colours, alpha test, clear colour/depth, fog,
/// toon). Their writes are compared in order instead of refusing the native; command ports and FIFOs
/// (geometry 0x0400_0400.., DMA, timers, IPC) are never written twice.
fn plain_io(a: u32) -> bool {
    (0x0400_0000..0x0400_0070).contains(&a) || (0x0400_1000..0x0400_1070).contains(&a) || (0x0400_0330..0x0400_03c0).contains(&a)
}

#[derive(Default, Clone, Copy, Debug)]
pub struct Tally {
    pub calls: u64,
    pub mismatches: u64,
    /// Instructions the translation ran, in total (to set a native's budget charge).
    pub instructions: u64,
}

pub struct Shadow {
    /// Native function pointer -> (its table entry, the translation it replaces).
    by_fn: HashMap<usize, (&'static Native, LiftedFn)>,
    pub tally: BTreeMap<&'static str, Tally>,
    /// The first mismatches, described (at most [`Shadow::KEEP`]).
    pub reports: Vec<String>,
}

/// What one run did.
#[derive(PartialEq)]
struct Effect {
    next: u32,
    regs: [u32; 7],
    flags: [bool; 4],
    /// Final value of every byte written (outside the entry frame), by address.
    bytes: BTreeMap<u32, u8>,
    /// Math-unit register writes in order: (address, width, value).
    math: Vec<(u32, u8, u32)>,
    /// Hardware writes outside the math unit, in order (not rolled back: only [`plain_io`] is allowed).
    other_io: Vec<(u32, u8, u32)>,
    unsafe_io: bool,
}

/// A bus that journals writes on top of the game's bus.
struct Journal<'a> {
    inner: &'a mut dyn Bus,
    /// (address, old byte) in write order, for rollback.
    undo: Vec<(u32, u8)>,
    bytes: BTreeMap<u32, u8>,
    math: Vec<(u32, u8, u32)>,
    other_io: Vec<(u32, u8, u32)>,
    /// A write to hardware that is not [`plain_io`].
    unsafe_io: bool,
    frame: std::ops::Range<u32>,
}

impl Journal<'_> {
    fn write(&mut self, a: u32, width: u8, v: u32) -> bool {
        if (0x0400_0000..0x0500_0000).contains(&a) {
            if MATH.contains(&a) {
                self.math.push((a, width, v));
            } else {
                self.other_io.push((a, width, v));
                if !plain_io(a) {
                    self.unsafe_io = true;
                }
            }
            return true;
        }
        for i in 0..width as u32 {
            let b = a.wrapping_add(i);
            self.undo.push((b, self.inner.r8(b)));
            if !self.frame.contains(&b) {
                self.bytes.insert(b, (v >> (8 * i)) as u8);
            }
        }
        true
    }
}

impl Mem for Journal<'_> {
    fn r8(&mut self, a: u32) -> u8 { self.inner.r8(a) }
    fn r16(&mut self, a: u32) -> u16 { self.inner.r16(a) }
    fn r32(&mut self, a: u32) -> u32 { self.inner.r32(a) }
    fn w8(&mut self, a: u32, v: u8) {
        self.write(a, 1, v as u32);
        self.inner.w8(a, v)
    }
    fn w16(&mut self, a: u32, v: u16) {
        self.write(a & !1, 2, v as u32);
        self.inner.w16(a, v)
    }
    fn w32(&mut self, a: u32, v: u32) {
        self.write(a & !3, 4, v);
        self.inner.w32(a, v)
    }
}

impl Bus for Journal<'_> {
    fn call(&mut self, c: &mut Cpu, target: u32, ret: u32) -> u32 { self.inner.call(c, target, ret) }
    fn swi(&mut self, c: &mut Cpu, n: u32) { self.inner.swi(c, n) }
    fn mrc(&mut self, c: &mut Cpu, cp: u8, o1: u8, n: u8, cm: u8, o2: u8) -> u32 { self.inner.mrc(c, cp, o1, n, cm, o2) }
    fn mcr(&mut self, c: &mut Cpu, cp: u8, o1: u8, n: u8, cm: u8, o2: u8, v: u32) { self.inner.mcr(c, cp, o1, n, cm, o2, v) }
}

fn effect(next: u32, c: &Cpu, j: Journal) -> (Effect, Vec<(u32, u8)>) {
    let regs = [c.r[0], c.r[1], c.r[2], c.r[3], c.r[12], c.r[13], c.r[14]];
    let e = Effect { next, regs, flags: [c.n, c.z, c.c, c.v], bytes: j.bytes, math: j.math, other_io: j.other_io, unsafe_io: j.unsafe_io };
    (e, j.undo)
}

impl Shadow {
    pub const KEEP: usize = 20;

    /// Shadows every native in `natives` whose translation is in `translations` (segment, entry -> code).
    pub fn new(natives: &'static [Native], translation: &dyn Fn(&str, u32) -> Option<LiftedFn>) -> Shadow {
        let by_fn = natives
            .iter()
            .filter_map(|n| Some((n.f as usize, (n, translation(n.segment, n.entry)?))))
            .collect();
        Shadow { by_fn, tally: BTreeMap::new(), reports: Vec::new() }
    }

    /// Runs block function `f` at `pc`: shadowed if `f` is a native, else as is.
    pub fn run(&mut self, f: LiftedFn, c: &mut Cpu, m: &mut dyn Bus, pc: u32) -> u32 {
        let Some(&(native, original)) = self.by_fn.get(&(f as usize)) else {
            return f(c, m, pc);
        };
        if c.budget == 0 {
            return f(c, m, pc); // out of budget: the native returns at once, nothing to compare
        }
        let entry = c.clone();
        let args = entry_regs(&entry);
        let frame = entry.r[13].wrapping_sub(STACK_FRAME)..entry.r[13];
        // 1. the exact translation, journaled, with all the budget it needs
        let mut j = Journal { inner: m, undo: Vec::new(), bytes: BTreeMap::new(), math: Vec::new(), other_io: Vec::new(), unsafe_io: false, frame: frame.clone() };
        c.budget = u32::MAX / 2;
        let next = original(c, &mut j, pc);
        let used = (u32::MAX / 2 - c.budget) as u64;
        let (exact, undo) = effect(next, c, j);
        // 2. roll back (math-unit writes are redone by the native) and restore the registers
        for &(a, old) in undo.iter().rev() {
            m.w8(a, old);
        }
        *c = entry;
        // 3. the native, for real
        let mut j = Journal { inner: m, undo: Vec::new(), bytes: BTreeMap::new(), math: Vec::new(), other_io: Vec::new(), unsafe_io: false, frame };
        let next = f(c, &mut j, pc);
        let (native_effect, _) = effect(next, c, j);
        let t = self.tally.entry(native.name).or_default();
        t.calls += 1;
        t.instructions += used;
        if native_effect != exact || exact.unsafe_io {
            t.mismatches += 1;
            if self.reports.len() < Self::KEEP {
                self.reports.push(describe(native, pc, &args, &exact, &native_effect));
            }
        }
        next
    }

    /// One line per shadowed native that ran: calls, mismatches, the translation's average instructions.
    pub fn summary(&self) -> String {
        let mut s = String::new();
        for (name, t) in &self.tally {
            s += &format!("{name}: {} calls, {} mismatches, {:.1} instructions per call\n", t.calls, t.mismatches, t.instructions as f64 / t.calls.max(1) as f64);
        }
        for r in &self.reports {
            s += &format!("  {r}\n");
        }
        s
    }
}

fn entry_regs(c: &Cpu) -> [u32; 4] {
    [c.r[0], c.r[1], c.r[2], c.r[3]]
}

fn describe(n: &Native, pc: u32, args: &[u32; 4], exact: &Effect, native: &Effect) -> String {
    let mut d = Vec::new();
    if exact.next != native.next {
        d.push(format!("returns to {:#x} not {:#x}", native.next, exact.next));
    }
    const NAMES: [&str; 7] = ["r0", "r1", "r2", "r3", "r12", "sp", "lr"];
    for i in 0..7 {
        if exact.regs[i] != native.regs[i] {
            d.push(format!("{} {:#x} not {:#x}", NAMES[i], native.regs[i], exact.regs[i]));
        }
    }
    if exact.flags != native.flags {
        d.push(format!("flags nzcv {:?} not {:?}", native.flags, exact.flags));
    }
    if exact.bytes != native.bytes {
        let first = exact.bytes.iter().find(|(a, v)| native.bytes.get(a) != Some(v)).map(|(a, _)| *a)
            .or_else(|| native.bytes.keys().find(|a| !exact.bytes.contains_key(a)).copied());
        d.push(format!("memory: {} vs {} bytes written, first difference at {:#x}", native.bytes.len(), exact.bytes.len(), first.unwrap_or(0)));
    }
    if exact.math != native.math {
        d.push(format!("math-unit writes differ ({} vs {})", native.math.len(), exact.math.len()));
    }
    if exact.other_io != native.other_io {
        d.push(format!("hardware writes differ ({} vs {})", native.other_io.len(), exact.other_io.len()));
    }
    if let Some(w) = exact.other_io.iter().find(|w| !plain_io(w.0)) {
        d.push(format!("the translation writes hardware {:#x}: not a leaf to shadow", w.0));
    }
    format!("{} at {pc:#x} (r0-r3 {:x?}): {}", n.name, args, d.join("; "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mem::Ram;
    use crate::native::{enter, ret};
    use crate::rt::BUDGET_EXHAUSTED;

    // translation: store r0+r1 at [r2], r0 = r0+r1, return; natives: one exact, one with a wrong store
    fn translation(c: &mut Cpu, m: &mut dyn Bus, _pc: u32) -> u32 {
        crate::rt::spend(c, 3);
        let v = c.r[0].wrapping_add(c.r[1]);
        m.w32(c.r[2], v);
        m.w32(c.r[13].wrapping_sub(4), 0x1234); // own stack frame: ignored
        c.r[0] = v;
        ret(c)
    }
    fn good(c: &mut Cpu, m: &mut dyn Bus, pc: u32) -> u32 {
        if !enter(c, pc, 3) { return BUDGET_EXHAUSTED; }
        c.r[0] = c.r[0].wrapping_add(c.r[1]);
        m.w32(c.r[2], c.r[0]);
        ret(c)
    }
    fn bad(c: &mut Cpu, m: &mut dyn Bus, pc: u32) -> u32 {
        if !enter(c, pc, 3) { return BUDGET_EXHAUSTED; }
        c.r[0] = c.r[0].wrapping_add(c.r[1]);
        m.w32(c.r[2].wrapping_add(4), c.r[0]);
        ret(c)
    }
    static NATIVES: &[Native] = &[
        Native { segment: "arm9_main", entry: 0x0200_0000, name: "good", f: good },
        Native { segment: "arm9_main", entry: 0x0200_0100, name: "bad", f: bad },
    ];

    #[test]
    fn matches_and_mismatches_are_counted_and_only_the_native_takes_effect() {
        let mut s = Shadow::new(NATIVES, &|_, _| Some(translation as LiftedFn));
        let mut m = Ram::new(&[], 0, 7);
        let mut c = Cpu { budget: 100, ..Cpu::default() };
        c.r = [5, 6, 0x0210_0000, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x0230_0000, 0x0200_2000, 0];
        let stack = m.r32(0x022F_FFFC);
        assert_eq!(s.run(good, &mut c, &mut m, 0x0200_0000), 0x0200_2000);
        assert_eq!((c.r[0], m.r32(0x0210_0000), c.budget), (11, 11, 97));
        assert_eq!(m.r32(0x022F_FFFC), stack, "the translation's stack write was rolled back");
        let before = m.r32(0x0210_0000);
        c.r[0] = 1;
        s.run(bad, &mut c, &mut m, 0x0200_0100);
        assert_eq!(m.r32(0x0210_0000), before, "the translation's store was rolled back");
        assert_eq!(m.r32(0x0210_0004), 7, "the native's store stays");
        assert_eq!((s.tally["good"].calls, s.tally["good"].mismatches, s.tally["bad"].mismatches), (1, 0, 1));
        assert_eq!(s.tally["good"].instructions, 3);
        assert!(s.reports[0].contains("first difference at 0x2100000"), "{}", s.reports[0]);
        let plain = |c: &mut Cpu, _: &mut dyn Bus, _: u32| -> u32 { c.r[5] = 9; 0 };
        s.run(plain, &mut c, &mut m, 0);
        assert_eq!(c.r[5], 9, "other blocks run as they are");
    }
}
