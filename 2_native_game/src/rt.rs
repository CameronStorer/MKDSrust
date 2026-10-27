//! Runtime shared by lifted code and the reference interpreter: CPU state, the memory/system
//! interface ([`Bus`]) and the ARMv5TE semantics helpers (shifter, ALU, flags, conditions, memory
//! access, multiplies, saturation, LDM/STM). No game data lives here.

/// ARM9 register state. Code addresses (`r[15]` as passed around) carry the Thumb bit in bit 0.
#[derive(Clone, Debug)]
pub struct Cpu {
    pub r: [u32; 16],
    pub n: bool,
    pub z: bool,
    pub c: bool,
    pub v: bool,
    /// Sticky saturation flag (CPSR bit 27).
    pub q: bool,
    /// CPSR control byte: mode (bits 0..4), F (6), I (7). The T bit is implied by the pc.
    pub ctl: u32,
    /// The current mode's SPSR.
    pub spsr: u32,
    /// Instructions left before lifted code / the interpreter return [`BUDGET_EXHAUSTED`].
    pub budget: u32,
    /// Banked r13/r14 and SPSR of the modes not current, by [`bank_of`] (0 user/system, 1 FIQ,
    /// 2 IRQ, 3 supervisor, 4 abort, 5 undefined), and r8..r12 of FIQ / of every other mode.
    pub banked: [[u32; 3]; 6],
    pub fiq_hi: [u32; 5],
    pub usr_hi: [u32; 5],
}

impl Default for Cpu {
    fn default() -> Self {
        Cpu { r: [0; 16], n: false, z: false, c: false, v: false, q: false, ctl: 0x1F, spsr: 0, budget: u32::MAX, banked: [[0; 3]; 6], fiq_hi: [0; 5], usr_hi: [0; 5] }
    }
}

/// Register bank of a CPSR mode: 0 user/system, 1 FIQ, 2 IRQ, 3 supervisor, 4 abort, 5 undefined.
pub fn bank_of(mode: u32) -> usize {
    match mode & 0x1F {
        0x11 => 1,
        0x12 => 2,
        0x13 => 3,
        0x17 => 4,
        0x1B => 5,
        _ => 0,
    }
}

impl Cpu {
    /// Architectural state (everything but the budget), for comparisons.
    pub fn state(&self) -> ([u32; 16], [bool; 5], u32, u32, [[u32; 3]; 6]) {
        (self.r, [self.n, self.z, self.c, self.v, self.q], self.ctl, self.spsr, self.banked)
    }

    /// Switches to CPSR mode `mode`, swapping the banked registers.
    pub fn set_mode(&mut self, mode: u32) {
        let (old, new) = (bank_of(self.ctl), bank_of(mode));
        if old != new {
            self.banked[old] = [self.r[13], self.r[14], self.spsr];
            [self.r[13], self.r[14], self.spsr] = self.banked[new];
            if (old == 1) != (new == 1) {
                let (save, load) = if old == 1 { (&mut self.fiq_hi, self.usr_hi) } else { (&mut self.usr_hi, self.fiq_hi) };
                save.copy_from_slice(&self.r[8..13]);
                self.r[8..13].copy_from_slice(&load);
            }
        }
        self.ctl = (self.ctl & !0x1F) | (mode & 0x1F);
    }

    /// Takes an exception: enters `mode` with SPSR = the old CPSR (T from `thumb`), lr = `lr`,
    /// IRQs (and for FIQ, FIQs) disabled. The caller continues at the vector.
    pub fn exception(&mut self, mode: u32, lr: u32, thumb: bool) {
        let old = self.cpsr() | ((thumb as u32) << 5);
        self.set_mode(mode);
        self.spsr = old;
        self.r[14] = lr;
        self.ctl |= 0x80 | if mode & 0x1F == 0x11 { 0x40 } else { 0 };
    }

    /// Exception return (`movs pc, lr` / `subs pc, lr, #4` / `ldm ... {pc}^`): CPSR = SPSR.
    /// Returns the state to continue in (true = Thumb).
    pub fn restore_spsr(&mut self) -> bool {
        let s = self.spsr;
        self.set_mode(s & 0x1F);
        self.n = s >> 31 != 0;
        self.z = (s >> 30) & 1 != 0;
        self.c = (s >> 29) & 1 != 0;
        self.v = (s >> 28) & 1 != 0;
        self.q = (s >> 27) & 1 != 0;
        self.ctl = s & 0xDF;
        s & 0x20 != 0
    }

    pub fn cpsr(&self) -> u32 {
        (self.n as u32) << 31 | (self.z as u32) << 30 | (self.c as u32) << 29 | (self.v as u32) << 28 | (self.q as u32) << 27 | (self.ctl & 0xDF)
    }

    /// MSR: `mask` bit 3 writes the flags byte, bit 0 the control byte.
    pub fn msr(&mut self, spsr: bool, mask: u8, v: u32) {
        let mut bytes = 0u32;
        for i in 0..4 {
            if mask & (1 << i) != 0 {
                bytes |= 0xFF << (8 * i);
            }
        }
        if spsr {
            self.spsr = (self.spsr & !bytes) | (v & bytes);
            return;
        }
        if mask & 8 != 0 {
            self.n = v >> 31 != 0;
            self.z = (v >> 30) & 1 != 0;
            self.c = (v >> 29) & 1 != 0;
            self.v = (v >> 28) & 1 != 0;
            self.q = (v >> 27) & 1 != 0;
        }
        if mask & 1 != 0 {
            self.set_mode(v & 0x1F);
            self.ctl = v & 0xDF;
        }
    }
}

pub trait Mem {
    fn r8(&mut self, a: u32) -> u8;
    fn r16(&mut self, a: u32) -> u16;
    fn r32(&mut self, a: u32) -> u32;
    fn w8(&mut self, a: u32, v: u8);
    fn w16(&mut self, a: u32, v: u16);
    fn w32(&mut self, a: u32, v: u32);
}

/// Everything lifted code needs from the machine around it: memory plus calls, BIOS calls (SWI)
/// and coprocessor access. The DS service layer (L3/L5) implements it; tests use `mem::Ram`.
pub trait Bus: Mem {
    /// Run code from `target` (bit 0 = Thumb) until control comes back to `ret`; returns the pc
    /// reached (`ret` normally; anything else means the callee left by another way, e.g. a
    /// switch helper that rewrote lr, and the caller must continue from there).
    fn call(&mut self, c: &mut Cpu, target: u32, ret: u32) -> u32;
    /// BIOS call `n` (the SWI number: ARM bits 23..16, Thumb bits 7..0).
    fn swi(&mut self, c: &mut Cpu, n: u32);
    /// MRC: coprocessor register to ARM register.
    fn mrc(&mut self, c: &mut Cpu, cp: u8, op1: u8, crn: u8, crm: u8, op2: u8) -> u32;
    /// MCR: ARM register to coprocessor register.
    fn mcr(&mut self, c: &mut Cpu, cp: u8, op1: u8, crn: u8, crm: u8, op2: u8, v: u32);
    /// The block at `pc` could not pay its `cost` (the slice's budget is used up). `false` (the
    /// default): lifted code stores `pc` in r15 and returns [`BUDGET_EXHAUSTED`] to its run loop.
    /// A fiber runtime (block 4A.2) instead ends the slice in place, takes a pending interrupt,
    /// charges `cost` from the next slice and returns `true`: the block then runs, and no Rust
    /// frame is unwound.
    fn slice_end(&mut self, _c: &mut Cpu, _pc: u32, _cost: u32) -> bool {
        false
    }
}

/// Takes an IRQ before the instruction at `pc` (bit 0 = Thumb): IRQ mode, lr = next + 4, SPSR = the
/// interrupted CPSR. Returns the IRQ vector (`vectors` + 0x18: 0x0000_0000 on the ARM7, 0xFFFF_0000 on the ARM9).
pub fn take_irq(c: &mut Cpu, pc: u32, vectors: u32) -> u32 {
    c.exception(0x12, (pc & !1).wrapping_add(4), pc & 1 != 0);
    vectors.wrapping_add(0x18)
}

/// Whether an IRQ would be taken now: IME, IE & IF, and the CPSR I bit clear.
pub fn irq_ready(c: &Cpu, ime: u32, ie: u32, iflag: u32) -> bool {
    ime & 1 != 0 && ie & iflag != 0 && c.ctl & 0x80 == 0
}

/// Our own IRQ vector code (the console's BIOS is not used): push r0-r3, r12, lr; call the handler
/// the OS installed (ARM7: word at 0x0380FFFC, reached through the WRAM mirror 0x03FFFFFC; ARM9: word
/// at DTCM + 0x3FFC); pop; return from the exception. Placed at vector + 0x18 by the machine.
pub const ARM7_IRQ_STUB: [u32; 6] = [
    0xE92D_500F, // stmfd sp!, {r0-r3, r12, lr}
    0xE3A0_0301, // mov   r0, #0x04000000
    0xE28F_E000, // add   lr, pc, #0
    0xE510_F004, // ldr   pc, [r0, #-4]
    0xE8BD_500F, // ldmfd sp!, {r0-r3, r12, lr}
    0xE25E_F004, // subs  pc, lr, #4
];
pub const ARM9_IRQ_STUB: [u32; 9] = [
    0xE92D_500F, // stmfd sp!, {r0-r3, r12, lr}
    0xEE19_0F11, // mrc   p15, 0, r0, c9, c1, 0   (DTCM region)
    0xE1A0_0620, // mov   r0, r0, lsr #12
    0xE1A0_0600, // mov   r0, r0, lsl #12
    0xE280_0901, // add   r0, r0, #0x4000
    0xE28F_E000, // add   lr, pc, #0
    0xE510_F004, // ldr   pc, [r0, #-4]
    0xE8BD_500F, // ldmfd sp!, {r0-r3, r12, lr}
    0xE25E_F004, // subs  pc, lr, #4
];

/// ARM7 cycles for one instruction, the ARM7 runtime's timing model (`arm7.rs` `cost`): 1 when the
/// condition failed; pipeline refill (+2) when it jumped. The generator charges lifted ARM7 blocks the
/// sum of this with every condition passing and only the block's last instruction jumping, so a
/// lifted block's budget charge is in the same units as the interpreter's.
pub fn arm7_cycles(d: &crate::arm::Decoded, passed: bool, jumped: bool) -> u32 {
    use crate::arm::{Inst, Op2};
    if !passed {
        return 1;
    }
    let refill = if jumped { 2 } else { 0 };
    match d.inst {
        Inst::Dp { op2: Op2::RegShift { .. }, .. } => 2 + refill,
        Inst::Dp { .. } => 1 + refill,
        Inst::Mul { .. } => 3,
        Inst::MulLong { .. } => 4,
        Inst::Mem { load: true, .. } | Inst::MemH { load: true, .. } => 3 + refill,
        Inst::Mem { .. } | Inst::MemH { .. } => 2,
        Inst::MemD { .. } => 3,
        Inst::Block { load, list, .. } | Inst::BlockUser { load, list, .. } => list.count_ones() + if load { 2 } else { 1 } + refill,
        Inst::Swp { .. } => 4,
        Inst::B { .. } | Inst::Call { .. } | Inst::Bx { .. } | Inst::Swi { .. } => 3,
        _ => 1 + refill,
    }
}

/// Returned by lifted code / the interpreter when the instruction budget runs out.
pub const BUDGET_EXHAUSTED: u32 = u32::MAX;

/// The BIOS function number of a SWI instruction.
pub fn swi_number(imm: u32, thumb: bool) -> u32 {
    if thumb { imm & 0xFF } else { (imm >> 16) & 0xFF }
}

pub fn cond(cc: u8, c: &Cpu) -> bool {
    match cc {
        0 => c.z,
        1 => !c.z,
        2 => c.c,
        3 => !c.c,
        4 => c.n,
        5 => !c.n,
        6 => c.v,
        7 => !c.v,
        8 => c.c && !c.z,
        9 => !c.c || c.z,
        10 => c.n == c.v,
        11 => c.n != c.v,
        12 => !c.z && c.n == c.v,
        13 => c.z || c.n != c.v,
        _ => true,
    }
}

/// Shifter: kind 0 LSL, 1 LSR, 2 ASR, 3 ROR. `amt` is the immediate field (0..=31), with the
/// ARM special cases for 0. Returns (value, carry out).
pub fn shift_imm(kind: u8, val: u32, amt: u32, carry: bool) -> (u32, bool) {
    match (kind, amt) {
        (0, 0) => (val, carry),
        (0, n) => (val << n, (val >> (32 - n)) & 1 != 0),
        (1, 0) => (0, val >> 31 != 0),
        (1, n) => (val >> n, (val >> (n - 1)) & 1 != 0),
        (2, 0) => (((val as i32) >> 31) as u32, val >> 31 != 0),
        (2, n) => (((val as i32) >> n) as u32, (val >> (n - 1)) & 1 != 0),
        (_, 0) => ((val >> 1) | ((carry as u32) << 31), val & 1 != 0), // RRX
        (_, n) => (val.rotate_right(n), (val >> (n - 1)) & 1 != 0),
    }
}

/// Shift by a register: the amount is its bottom byte; 0 leaves value and carry alone.
pub fn shift_reg(kind: u8, val: u32, rs: u32, carry: bool) -> (u32, bool) {
    let n = rs & 0xFF;
    if n == 0 {
        return (val, carry);
    }
    match kind {
        0 => match n {
            1..=31 => (val << n, (val >> (32 - n)) & 1 != 0),
            32 => (0, val & 1 != 0),
            _ => (0, false),
        },
        1 => match n {
            1..=31 => (val >> n, (val >> (n - 1)) & 1 != 0),
            32 => (0, val >> 31 != 0),
            _ => (0, false),
        },
        2 => {
            if n < 32 {
                (((val as i32) >> n) as u32, (val >> (n - 1)) & 1 != 0)
            } else {
                (((val as i32) >> 31) as u32, val >> 31 != 0)
            }
        }
        _ => {
            let k = n & 31;
            if k == 0 { (val, val >> 31 != 0) } else { (val.rotate_right(k), (val >> (k - 1)) & 1 != 0) }
        }
    }
}

/// Rotated 8-bit immediate; carry is unchanged when the rotation is zero.
pub fn imm_op(imm8: u32, rot: u32, carry: bool) -> (u32, bool) {
    if rot == 0 {
        (imm8, carry)
    } else {
        let v = imm8.rotate_right(rot * 2);
        (v, v >> 31 != 0)
    }
}

fn add_cc(a: u32, b: u32, cin: bool) -> (u32, bool, bool) {
    let wide = a as u64 + b as u64 + cin as u64;
    let r = wide as u32;
    (r, wide > u32::MAX as u64, ((a ^ r) & (b ^ r)) >> 31 != 0)
}

/// Data-processing ALU. Returns (result, C, V); the caller derives N/Z and decides whether to write.
/// `sc` is the shifter carry (used by logical ops), `c`/`v` the current flags.
pub fn alu(op: u8, a: u32, b: u32, c: bool, sc: bool, v: bool) -> (u32, bool, bool) {
    match op {
        0 | 8 => (a & b, sc, v),
        1 | 9 => (a ^ b, sc, v),
        2 | 10 => add_cc(a, !b, true),
        3 => add_cc(b, !a, true),
        4 | 11 => add_cc(a, b, false),
        5 => add_cc(a, b, c),
        6 => add_cc(a, !b, c),
        7 => add_cc(b, !a, c),
        12 => (a | b, sc, v),
        13 => (b, sc, v),
        14 => (a & !b, sc, v),
        _ => (!b, sc, v),
    }
}

/// Flag-setting arithmetic for lifted code (ADDS/ADCS/SUBS/SBCS; RSBS/RSCS swap the operands).
#[inline(always)]
pub fn adds(c: &mut Cpu, a: u32, b: u32) -> u32 {
    let (r, cy, v) = add_cc(a, b, false);
    c.n = r >> 31 != 0;
    c.z = r == 0;
    c.c = cy;
    c.v = v;
    r
}
#[inline(always)]
pub fn adcs(c: &mut Cpu, a: u32, b: u32) -> u32 {
    let (r, cy, v) = add_cc(a, b, c.c);
    c.n = r >> 31 != 0;
    c.z = r == 0;
    c.c = cy;
    c.v = v;
    r
}
#[inline(always)]
pub fn subs(c: &mut Cpu, a: u32, b: u32) -> u32 {
    let (r, cy, v) = add_cc(a, !b, true);
    c.n = r >> 31 != 0;
    c.z = r == 0;
    c.c = cy;
    c.v = v;
    r
}
#[inline(always)]
pub fn sbcs(c: &mut Cpu, a: u32, b: u32) -> u32 {
    let (r, cy, v) = add_cc(a, !b, c.c);
    c.n = r >> 31 != 0;
    c.z = r == 0;
    c.c = cy;
    c.v = v;
    r
}
/// Spends `n` instructions of the budget for a block; false when nothing is left. A block may
/// start with less than `n` left (it then uses the rest), so a time slice shorter than a block
/// still makes progress.
#[inline(always)]
pub fn spend(c: &mut Cpu, n: u32) -> bool {
    if c.budget == 0 {
        return false;
    }
    c.budget = c.budget.saturating_sub(n);
    true
}

#[inline(always)]
pub fn set_nz(c: &mut Cpu, res: u32) {
    c.n = res >> 31 != 0;
    c.z = res == 0;
}

/// UMULL/UMLAL/SMULL/SMLAL. Returns (lo, hi).
pub fn mul_long(c: &mut Cpu, signed: bool, acc: bool, s: bool, rm: u32, rs: u32, lo: u32, hi: u32) -> (u32, u32) {
    let mut p = if signed { (rm as i32 as i64).wrapping_mul(rs as i32 as i64) as u64 } else { (rm as u64) * (rs as u64) };
    if acc {
        p = p.wrapping_add(((hi as u64) << 32) | lo as u64);
    }
    if s {
        c.n = p >> 63 != 0;
        c.z = p == 0;
    }
    (p as u32, (p >> 32) as u32)
}

fn half(v: u32, top: bool) -> i32 {
    if top { (v >> 16) as i16 as i32 } else { v as i16 as i32 }
}

fn add_q(c: &mut Cpu, a: i32, b: i32) -> i32 {
    let (r, o) = a.overflowing_add(b);
    if o {
        c.q = true;
    }
    r
}

/// SMLAxy / SMULxy / SMLAWy / SMULWy: the 32-bit result (`acc` is rn). SMLALxy uses [`smlal_xy`].
pub fn half_mul(c: &mut Cpu, op: crate::arm::HalfMul, x: bool, y: bool, rm: u32, rs: u32, acc: u32) -> u32 {
    use crate::arm::HalfMul::*;
    match op {
        Smul => (half(rm, x) * half(rs, y)) as u32,
        Smla => add_q(c, half(rm, x) * half(rs, y), acc as i32) as u32,
        Smulw => (((rm as i32 as i64) * half(rs, y) as i64) >> 16) as u32,
        Smlaw => add_q(c, (((rm as i32 as i64) * half(rs, y) as i64) >> 16) as i32, acc as i32) as u32,
        Smlal => unreachable!("SMLALxy goes through smlal_xy"),
    }
}

/// SMLALxy: hi:lo += half(rm) * half(rs). Returns (lo, hi).
pub fn smlal_xy(x: bool, y: bool, rm: u32, rs: u32, lo: u32, hi: u32) -> (u32, u32) {
    let p = (half(rm, x) * half(rs, y)) as i64;
    let r = ((((hi as u64) << 32) | lo as u64) as i64).wrapping_add(p) as u64;
    (r as u32, (r >> 32) as u32)
}

fn sat(c: &mut Cpu, v: i64) -> u32 {
    if v > i32::MAX as i64 {
        c.q = true;
        i32::MAX as u32
    } else if v < i32::MIN as i64 {
        c.q = true;
        i32::MIN as u32
    } else {
        v as i32 as u32
    }
}

/// QADD (0), QSUB (1), QDADD (2), QDSUB (3).
pub fn qalu(c: &mut Cpu, op: u8, rm: u32, rn: u32) -> u32 {
    let b = if op >= 2 { sat(c, rn as i32 as i64 * 2) } else { rn } as i32 as i64;
    let a = rm as i32 as i64;
    if op & 1 == 0 { sat(c, a + b) } else { sat(c, a - b) }
}

pub fn ldr<M: Mem + ?Sized>(m: &mut M, a: u32) -> u32 {
    m.r32(a & !3).rotate_right((a & 3) * 8)
}
pub fn str_<M: Mem + ?Sized>(m: &mut M, a: u32, v: u32) {
    m.w32(a & !3, v)
}

/// Halfword/signed loads. kind: 1 LDRH, 2 LDRSB, 3 LDRSH.
pub fn load_h<M: Mem + ?Sized>(m: &mut M, a: u32, kind: u8) -> u32 {
    match kind {
        1 => m.r16(a & !1) as u32,
        2 => m.r8(a) as i8 as i32 as u32,
        _ => m.r16(a & !1) as i16 as i32 as u32,
    }
}

/// SWP / SWPB: returns the old memory value.
pub fn swp<M: Mem + ?Sized>(m: &mut M, a: u32, v: u32, byte: bool) -> u32 {
    if byte {
        let old = m.r8(a) as u32;
        m.w8(a, v as u8);
        old
    } else {
        let old = ldr(m, a);
        m.w32(a & !3, v);
        old
    }
}

/// LDM/STM with `^`. Without r15 in a load list it transfers the user-mode registers (the OS
/// thread switch saves / restores a thread's r0..r14 this way); with r15 it loads normally and the
/// caller then returns from the exception (CPSR = SPSR, [`Cpu::restore_spsr`]).
#[allow(clippy::too_many_arguments)]
pub fn block_user<M: Mem + ?Sized>(c: &mut Cpu, m: &mut M, load: bool, pre: bool, up: bool, wb: bool, rn: usize, list: u16, pc_store: u32) -> Option<u32> {
    if load && list & 0x8000 != 0 {
        return block(c, m, load, pre, up, wb, rn, list, pc_store);
    }
    // the base (and its writeback) is the current mode's register; only the transferred registers
    // are the user bank's (the ARM7 IRQ handler saves the interrupted sp/lr with `stmdb sp, {sp, lr}^`)
    let base = c.r[rn];
    let mode = c.ctl & 0x1F;
    c.set_mode(0x1F);
    let (r, end) = block_at(c, m, base, load, pre, up, list, pc_store);
    c.set_mode(mode);
    if wb {
        c.r[rn] = end;
    }
    r
}

/// LDM/STM. Returns the loaded pc value (raw: the caller applies interworking) when a load list
/// includes r15. Stores of r15 store `pc_store` (the instruction address + 12).
#[allow(clippy::too_many_arguments)]
pub fn block<M: Mem + ?Sized>(c: &mut Cpu, m: &mut M, load: bool, pre: bool, up: bool, wb: bool, rn: usize, list: u16, pc_store: u32) -> Option<u32> {
    let (new_pc, end) = block_at(c, m, c.r[rn], load, pre, up, list, pc_store);
    if wb {
        c.r[rn] = end;
    }
    new_pc
}

/// The transfer of [`block`] from `base`: (loaded pc, the written-back base).
#[allow(clippy::too_many_arguments)]
fn block_at<M: Mem + ?Sized>(c: &mut Cpu, m: &mut M, base: u32, load: bool, pre: bool, up: bool, list: u16, pc_store: u32) -> (Option<u32>, u32) {
    let n = list.count_ones();
    let start = match (up, pre) {
        (true, false) => base,
        (true, true) => base.wrapping_add(4),
        (false, false) => base.wrapping_sub(4 * (n - 1)),
        (false, true) => base.wrapping_sub(4 * n),
    };
    let end = if up { base.wrapping_add(4 * n) } else { base.wrapping_sub(4 * n) };
    let mut a = start;
    let mut new_pc = None;
    for i in 0..16 {
        if list & (1 << i) == 0 {
            continue;
        }
        if load {
            let v = m.r32(a & !3);
            if i == 15 {
                new_pc = Some(v);
            } else {
                c.r[i] = v;
            }
        } else {
            m.w32(a & !3, if i == 15 { pc_store } else { c.r[i] });
        }
        a = a.wrapping_add(4);
    }
    (new_pc, end)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `stmdb sp, {sp, lr}^` / `ldmia sp, {sp, lr}^` in IRQ mode (the ARM7 IRQ handler): the address
    /// comes from the IRQ sp, the data is the user/system sp and lr (T120: the user sp was used as the
    /// address, so the interrupted sp/lr came back from the wrong place and the ARM7 jumped to 0).
    #[test]
    fn user_bank_transfer_uses_the_current_mode_base() {
        struct Words(std::collections::HashMap<u32, u32>);
        impl Mem for Words {
            fn r8(&mut self, _: u32) -> u8 { unreachable!() }
            fn r16(&mut self, _: u32) -> u16 { unreachable!() }
            fn r32(&mut self, a: u32) -> u32 { self.0.get(&a).copied().unwrap_or(0xDEAD_BEEF) }
            fn w8(&mut self, _: u32, _: u8) { unreachable!() }
            fn w16(&mut self, _: u32, _: u16) { unreachable!() }
            fn w32(&mut self, a: u32, v: u32) { self.0.insert(a, v); }
        }
        let mut m = Words(Default::default());
        let mut c = Cpu::default();
        c.r[13] = 0x0380_FB74; // system sp
        c.r[14] = 0x037F_8500; // system lr
        c.exception(0x12, 0x0380_11C4, true);
        c.r[13] = 0x0380_FF60; // IRQ sp
        assert_eq!(block_user(&mut c, &mut m, false, true, false, false, 13, 0x6000, 0), None);
        assert_eq!((m.r32(0x0380_FF58), m.r32(0x0380_FF5C)), (0x0380_FB74, 0x037F_8500));
        assert_eq!(c.r[13], 0x0380_FF60, "IRQ sp unchanged (no writeback)");
        // the handler changes the system sp/lr, then restores them from the IRQ stack
        c.r[13] = 0x0380_FF58; // `sub sp, sp, #8`
        c.banked[0] = [0x0380_FE80, 0x0380_37EC, 0];
        assert_eq!(block_user(&mut c, &mut m, true, false, true, false, 13, 0x6000, 0), None);
        assert_eq!(c.r[13], 0x0380_FF58);
        assert_eq!(&c.banked[0][..2], &[0x0380_FB74, 0x037F_8500]);
        // writeback goes to the current mode's base
        assert_eq!(block_user(&mut c, &mut m, false, true, false, true, 13, 0x6000, 0), None);
        assert_eq!(c.r[13], 0x0380_FF50);
        assert_eq!(c.banked[0][0], 0x0380_FB74);
    }

    #[test]
    fn dsp_and_saturation() {
        let mut c = Cpu::default();
        // smulwb: (0x10000 * -2) >> 16 = -2
        assert_eq!(half_mul(&mut c, crate::arm::HalfMul::Smulw, false, false, 0x10000, 0xFFFE, 0) as i32, -2);
        assert_eq!(qalu(&mut c, 0, 0x7FFF_FFFF, 1), 0x7FFF_FFFF);
        assert!(c.q);
        assert_eq!(shift_reg(0, 1, 32, false), (0, true));
        assert_eq!(shift_reg(1, 0x8000_0000, 33, true), (0, false));
        assert_eq!(mul_long(&mut c, true, false, false, (-3i32) as u32, 5, 0, 0), ((-15i32) as u32, u32::MAX));
    }
}

/// Block 4B: whether the raised (T1) version of the function at `entry` runs (else its exact T0
/// translation does). `MKDS_T1` unset or `all`: every T1; `off`: none; otherwise a file of entry
/// addresses (hex, one per line) for bisecting.
pub fn t1_enabled(entry: u32) -> bool {
    use std::sync::OnceLock;
    static SET: OnceLock<Option<std::collections::HashSet<u32>>> = OnceLock::new();
    let set = SET.get_or_init(|| match std::env::var("MKDS_T1").ok().as_deref() {
        None | Some("all") | Some("") => None,
        Some("off") => Some(Default::default()),
        Some(path) => Some(
            std::fs::read_to_string(path)
                .unwrap_or_else(|e| panic!("MKDS_T1 file {path}: {e}"))
                .lines()
                .filter_map(|l| u32::from_str_radix(l.trim().trim_start_matches("0x").trim_start_matches("0X"), 16).ok())
                .collect(),
        ),
    });
    set.as_ref().is_none_or(|s| s.contains(&entry) || s.contains(&(entry & !1)))
}

// ---- T1 memory helpers: the exact accesses of the translation, with the alignment the ARM
// applies made implicit (a word access ignores address bits 0-1, a halfword access bit 0).
#[inline(always)]
pub fn ld32<M: Mem + ?Sized>(m: &mut M, a: u32) -> u32 {
    m.r32(a & !3)
}
#[inline(always)]
pub fn ld16<M: Mem + ?Sized>(m: &mut M, a: u32) -> u32 {
    m.r16(a & !1) as u32
}
#[inline(always)]
pub fn ld16s<M: Mem + ?Sized>(m: &mut M, a: u32) -> u32 {
    m.r16(a & !1) as i16 as i32 as u32
}
#[inline(always)]
pub fn ld8<M: Mem + ?Sized>(m: &mut M, a: u32) -> u32 {
    m.r8(a) as u32
}
#[inline(always)]
pub fn ld8s<M: Mem + ?Sized>(m: &mut M, a: u32) -> u32 {
    m.r8(a) as i8 as i32 as u32
}
#[inline(always)]
pub fn st32<M: Mem + ?Sized>(m: &mut M, a: u32, v: u32) {
    m.w32(a & !3, v)
}
#[inline(always)]
pub fn st16<M: Mem + ?Sized>(m: &mut M, a: u32, v: u32) {
    m.w16(a & !1, v as u16)
}
#[inline(always)]
pub fn st8<M: Mem + ?Sized>(m: &mut M, a: u32, v: u32) {
    m.w8(a, v as u8)
}
