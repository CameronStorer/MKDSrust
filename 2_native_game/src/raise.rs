//! Block 4B: decompiler passes over the IR ([`crate::ir`]).
//!
//! Pass 1, flag liveness: an N/Z/C/V value that no later instruction can read is not computed.
//! Liveness runs backwards over the function's blocks. Anything that leaves the function (a return,
//! a tail call, a computed jump), every call and every opaque helper (block transfers, SWI, MSR,
//! exception returns) counts as reading all four flags, so the pass is conservative. Re-entering a
//! block after a slice end or a call keeps the CPU state, so liveness at block starts stays valid in
//! the resumable (flat) runtime as well as in fibers.
use crate::arm::Decoded;
use crate::ir::{self, Flag, Ins, E, H, S};
use crate::lift::{Func, Image};
use std::collections::BTreeMap;

pub const N: u8 = 1;
pub const Z: u8 = 2;
pub const C: u8 = 4;
pub const V: u8 = 8;
pub const NZCV: u8 = 15;

fn bit(f: Flag) -> u8 {
    match f {
        Flag::N => N,
        Flag::Z => Z,
        Flag::C => C,
        Flag::V => V,
    }
}

/// Flags the condition `cc` reads.
pub fn cond_reads(cc: u8) -> u8 {
    match cc {
        0 | 1 => Z,
        2 | 3 => C,
        4 | 5 => N,
        6 | 7 => V,
        8 | 9 => C | Z,
        10 | 11 => N | V,
        12 | 13 => Z | N | V,
        _ => 0,
    }
}

/// (flags read, flags written) by a helper call, beyond its arguments.
fn helper_flags(h: H, args: &[E]) -> (u8, u8) {
    match h {
        H::Subs | H::Adds => (0, NZCV),
        H::Adcs | H::Sbcs => (C, NZCV),
        H::MulLong => (0, if args.get(3) == Some(&E::Bool(true)) { N | Z } else { 0 }),
        H::ShiftReg | H::ShiftImmValue | H::Ldr | H::SmlalXy | H::HalfMul | H::Qalu | H::Swp | H::Mrc | H::Mcr => (0, 0),
        // opaque: may switch modes, restore the CPSR or run BIOS code
        H::Block | H::BlockUser | H::Swi | H::Msr => (NZCV, NZCV),
    }
}

/// Flags an expression reads.
pub fn expr_reads(e: &E) -> u8 {
    match e {
        E::Flag(f) => bit(*f),
        E::Cond(cc) => cond_reads(*cc),
        E::Cpsr => NZCV,
        E::Rrx(a) => C | expr_reads(a),
        E::Bin(_, a, b) => expr_reads(a) | expr_reads(b),
        E::Not(a) | E::Shl(a, _) | E::Lsr(a, _) | E::Asr(a, _) | E::Ror(a, _) | E::Bit(a, _) | E::Msb(a) | E::Lsb(a) | E::BoolU32(a) | E::Clz(a) | E::Iw(a) => expr_reads(a),
        E::Load { addr, .. } => expr_reads(addr),
        E::Call(h, args) => helper_flags(*h, args).0 | args.iter().map(expr_reads).fold(0, |x, y| x | y),
        _ => 0,
    }
}

fn expr_writes(e: &E) -> u8 {
    match e {
        E::Call(h, args) => helper_flags(*h, args).1 | args.iter().map(expr_writes).fold(0, |x, y| x | y),
        E::Bin(_, a, b) => expr_writes(a) | expr_writes(b),
        _ => 0,
    }
}

/// (reads, definite writes) of one statement. Control transfers read all flags (the code they
/// reach is outside this analysis) unless they jump to a known block, which the caller handles.
pub fn stmt_flags(s: &S) -> (u8, u8) {
    match s {
        S::Let(_, e) | S::Let2(_, _, e) | S::SetReg(_, e) | S::Do(e) => (expr_reads(e), expr_writes(e)),
        S::SetFlag(f, e) => (expr_reads(e), bit(*f) | expr_writes(e)),
        S::SetNz(e) => (expr_reads(e), N | Z),
        S::Store { addr, val, .. } => (expr_reads(addr) | expr_reads(val), 0),
        S::Goto(E::Const(_)) => (0, 0),
        S::Goto(e) => (NZCV | expr_reads(e), 0),
        S::ExcReturn(_) | S::Call { .. } | S::IfSome(..) => (NZCV, NZCV),
    }
}

/// Flags live after each statement of a block's instructions, given the flags live at its end.
/// Returns the flags live at the block's start.
fn block_live_in(insts: &[Ins], live_out: u8) -> u8 {
    let mut live = live_out;
    for ins in insts.iter().rev() {
        let mut inner = live;
        for s in ins.stmts.iter().rev() {
            let (r, w) = stmt_flags(s);
            inner = (inner & !w) | r;
        }
        // a conditional instruction may not run: what was live after it stays live
        live = if ins.cond == 14 { inner } else { inner | live | cond_reads(ins.cond) };
    }
    live
}

/// Where control can go after the block: known blocks of the function, plus `true` if it can also
/// leave the function (return, tail call, computed jump).
fn successors(f: &Func, start: u32, insts: &[Ins]) -> (Vec<u32>, bool) {
    let mut next = Vec::new();
    let mut leaves = false;
    let mut falls = true;
    for ins in insts {
        for s in &ins.stmts {
            match s {
                S::Goto(E::Const(t)) => {
                    let t = *t | (start & 1);
                    if f.blocks.contains(&t) { next.push(t) } else { leaves = true }
                    if ins.cond == 14 {
                        falls = false;
                    }
                }
                S::Goto(_) | S::ExcReturn(_) => {
                    leaves = true;
                    if ins.cond == 14 {
                        falls = false;
                    }
                }
                S::Call { .. } | S::IfSome(..) => leaves = true,
                _ => {}
            }
        }
    }
    if falls {
        let last = insts.last().map(|i| i.addr).unwrap_or(start);
        let d = f.insts.get(&last);
        let end = d.map(|d: &Decoded| Func::next(last, d)).unwrap_or(last);
        if f.blocks.contains(&end) { next.push(end) } else { leaves = true }
    }
    (next, leaves)
}

/// Pass 1 result for one function: for each instruction (by address), the flags still needed after
/// it. A flag an instruction writes outside this mask is dead.
pub fn flag_liveness(f: &Func, img: Option<&Image>) -> BTreeMap<u32, u8> {
    let blocks: Vec<(u32, Vec<Ins>)> = f.blocks.iter().map(|&b| (b, ir::lower_block(f, b, img))).collect();
    let succ: BTreeMap<u32, (Vec<u32>, bool)> = blocks.iter().map(|(b, i)| (*b, successors(f, *b, i))).collect();
    let mut live_in: BTreeMap<u32, u8> = blocks.iter().map(|(b, _)| (*b, 0)).collect();
    loop {
        let mut changed = false;
        for (b, insts) in blocks.iter().rev() {
            let (next, leaves) = &succ[b];
            let out = next.iter().map(|n| live_in[n]).fold(if *leaves { NZCV } else { 0 }, |x, y| x | y);
            let v = block_live_in(insts, out);
            if v != live_in[b] {
                live_in.insert(*b, v);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    let mut after = BTreeMap::new();
    for (b, insts) in &blocks {
        let (next, leaves) = &succ[b];
        let mut live = next.iter().map(|n| live_in[n]).fold(if *leaves { NZCV } else { 0 }, |x, y| x | y);
        for ins in insts.iter().rev() {
            after.insert(ins.addr, live);
            let mut inner = live;
            for s in ins.stmts.iter().rev() {
                let (r, w) = stmt_flags(s);
                inner = (inner & !w) | r;
            }
            live = if ins.cond == 14 { inner } else { inner | live | cond_reads(ins.cond) };
        }
    }
    after
}

/// Removes flag computations nobody reads from one instruction's statements, given the flags
/// live after it. `R::subs(c, a, b)` with N Z C V all dead becomes `a.wrapping_sub(b)`.
pub fn drop_dead_flags(stmts: &mut Vec<S>, live_after: u8) {
    // walk backwards keeping the flags live between statements
    let mut live = live_after;
    let mut out: Vec<S> = Vec::with_capacity(stmts.len());
    for mut s in stmts.drain(..).rev() {
        let keep = match &mut s {
            S::SetNz(_) => live & (N | Z) != 0,
            S::SetFlag(f, _) => live & bit(*f) != 0,
            S::Let(_, e) | S::SetReg(_, e) => {
                if live & NZCV == 0 {
                    simplify_dead_arith(e);
                }
                true
            }
            _ => true,
        };
        if keep {
            let (r, w) = stmt_flags(&s);
            live = (live & !w) | r;
            out.push(s);
        }
    }
    out.reverse();
    *stmts = out;
}

/// `R::subs/adds(c, a, b)` whose flags are dead: plain wrapping arithmetic.
fn simplify_dead_arith(e: &mut E) {
    if let E::Call(h @ (H::Subs | H::Adds), args) = e {
        if let [E::CpuArg, a, b] = args.as_mut_slice() {
            let op = if *h == H::Subs { ir::Bin::Sub } else { ir::Bin::Add };
            *e = E::Bin(op, Box::new(std::mem::replace(a, E::Lit(0))), Box::new(std::mem::replace(b, E::Lit(0))));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lift::{discover, emit_full};

    #[test]
    fn dead_flags_go_live_flags_stay() {
        // movs r0, #1 (flags dead: overwritten); loop: subs r4, #1; bne loop (Z live); bx lr
        let code: [u16; 4] = [0x2001, 0x3C01, 0xD1FD, 0x4770];
        let bytes: Vec<u8> = code.iter().flat_map(|h| h.to_le_bytes()).collect();
        let img = Image::new(0x0200_0000, &bytes);
        let f = discover(&img, 0x0200_0001, Some((0x0200_0000, 0x0200_0008)), 100).unwrap();
        let raised = emit_full(&f, "t", Some(&img), None, true);
        let exact = emit_full(&f, "t", Some(&img), None, false);
        assert!(exact.contains("R::set_nz(c, res); c.r[0] = res;"), "{exact}");
        assert!(!raised.contains("R::set_nz"), "movs flags are dead before subs:\n{raised}");
        assert!(raised.contains("R::subs(c, c.r[4], 0x1u32)"), "subs feeds bne:\n{raised}");
    }
}
