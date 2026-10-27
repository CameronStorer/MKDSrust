//! Block 4B: decompiler passes over the IR ([`crate::ir`]).
//!
//! Pass 1, flag liveness: an N/Z/C/V value that no later instruction can read is not computed.
//! Liveness runs backwards over the function's blocks. Anything that leaves the function (a return,
//! a tail call, a computed jump), every call and every opaque helper (block transfers, SWI, MSR,
//! exception returns) counts as reading all four flags, so the pass is conservative. With the
//! whole-program facts ([`FlagContext`], ARM9) calls and returns read only what the code on the
//! other side reads. Re-entering a
//! block after a slice end or a call keeps the CPU state, so liveness at block starts stays valid in
//! the resumable (flat) runtime as well as in fibers.
use crate::arm::Decoded;
use crate::ir::{self, Flag, Ins, E, H, S};
use crate::lift::{Func, Image};
use std::collections::{BTreeMap, HashMap};

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
        E::Bin(_, a, b) | E::Cmp(_, _, a, b) => expr_reads(a) | expr_reads(b),
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

/// Whole-program flag facts (ARM9): with them, calls and returns read only the flags the code on
/// the other side can read, instead of all four. Compiled code never reads flags across a call,
/// but hand-written routines may (a compare helper returning its result in the flags); the facts
/// come from analysing every function, so those keep their flags.
#[derive(Clone, Default, Debug, PartialEq)]
pub struct FlagContext {
    /// Flags read before written from each block start of each function (without the Thumb bit).
    pub entry: HashMap<u32, u8>,
    /// Flags code after some call to the function reads (its returns must keep them).
    pub ret: HashMap<u32, u8>,
    /// Functions whose address is stored somewhere (a word in the program's data or a constant in
    /// its code): the only possible targets of computed calls.
    pub taken: std::collections::HashSet<u32>,
    /// Flags some address-taken function reads at entry (what a computed call may need).
    pub any_entry: u8,
    /// Flags code reads after a computed call returns: every address-taken function's returns
    /// keep them.
    pub any_ret: u8,
    /// Flags read after some MSR writes the CPSR flags from a register (restoring a value an
    /// MRS saved): every MRS keeps them.
    pub restored: u8,
}

static CONTEXT: std::sync::RwLock<Option<std::sync::Arc<FlagContext>>> = std::sync::RwLock::new(None);

/// Installs (or, with `None`, removes) the whole-program flag facts used by [`flag_liveness`].
pub fn set_flag_context(c: Option<FlagContext>) {
    *CONTEXT.write().unwrap() = c.map(std::sync::Arc::new);
}

fn context() -> Option<std::sync::Arc<FlagContext>> {
    CONTEXT.read().unwrap().clone()
}

/// How a function's control transfers read flags under a [`FlagContext`].
struct Exits {
    ctx: std::sync::Arc<FlagContext>,
    /// Flags this function's returns keep.
    ret: u8,
}

impl Exits {
    fn new(f: &Func) -> Option<Exits> {
        let ctx = context()?;
        let me = f.entry & !1;
        let ret = ctx.ret.get(&me).copied().unwrap_or(NZCV) | if ctx.taken.contains(&me) { ctx.any_ret } else { 0 };
        Some(Exits { ctx, ret })
    }
    /// Flags read on entering code at `t` (another function's entry, else all).
    fn entry(&self, t: u32) -> u8 {
        self.ctx.entry.get(&(t & !1)).copied().unwrap_or(NZCV)
    }
}

/// [`stmt_flags`] under the whole-program facts: a call reads what the callee reads at entry and
/// leaves the flags as they were for the code after it (whose needs become the callee's return
/// needs); a return reads what the callers need; push/pop don't touch flags.
fn stmt_flags_in(s: &S, x: Option<&Exits>, f: &Func, img: Option<&Image>, addr: u32) -> (u8, u8) {
    let Some(x) = x else { return stmt_flags(s) };
    let block = |e: &E| matches!(e, E::Call(H::Block, _));
    match s {
        S::Call { target: E::Const(t), .. } => (x.entry(*t), 0),
        S::Call { .. } => (x.ctx.any_entry, 0),
        S::Goto(e @ E::Const(_)) => (expr_reads(e), 0),
        S::Goto(e) => {
            let internal = img.is_some_and(|img| !crate::lift::computed_targets(f, img, addr).is_empty());
            (expr_reads(e) | if internal { 0 } else { x.ret | x.ctx.any_entry }, 0)
        }
        S::IfSome(e, _) if block(e) => (x.ret, 0),
        S::Do(e) if block(e) => (0, 0),
        // reading the CPSR (to save it) keeps what this function's callers need and what a later
        // restore of a saved CPSR makes live; other uses of the saved word don't read the flags
        _ if matches!(s, S::SetReg(_, E::Cpsr)) => (x.ret | x.ctx.restored, 0),
        // the BIOS keeps the caller's flags (its routines return through the SPSR)
        S::Do(E::Call(H::Swi, a)) => (a.iter().map(expr_reads).fold(0, |x, y| x | y), 0),
        // an exception return replaces the flags from the SPSR: the current ones are dead
        S::ExcReturn(_) => (0, NZCV),
        S::IfSome(E::Call(H::BlockUser, _), _) | S::Do(E::Call(H::BlockUser, _)) => (0, 0),
        // MSR writes the CPSR flags only with the flag field; a mode change keeps them
        S::Do(E::Call(H::Msr, a)) => match a.as_slice() {
            [E::Bool(spsr), E::Lit(mask), v] => (expr_reads(v), if !*spsr && mask & 8 != 0 { NZCV } else { 0 }),
            _ => stmt_flags(s),
        },
        _ => stmt_flags(s),
    }
}

/// Flags live after each statement of a block's instructions, given the flags live at its end.
/// Returns the flags live at the block's start.
fn block_live_in(insts: &[Ins], live_out: u8, sf: &dyn Fn(&S, u32) -> (u8, u8)) -> u8 {
    let mut live = live_out;
    for ins in insts.iter().rev() {
        let mut inner = live;
        for s in ins.stmts.iter().rev() {
            let (r, w) = sf(s, ins.addr);
            inner = (inner & !w) | r;
        }
        // a conditional instruction may not run: what was live after it stays live
        live = if ins.cond == 14 { inner } else { inner | live | cond_reads(ins.cond) };
    }
    live
}

/// Where control can go after the block (known blocks of the function) and the flags read where
/// it leaves the function: all four, or under the whole-program facts what the code reached reads
/// (calls and returns are statements, see [`stmt_flags_in`]; compiled switches go to their cases).
fn successors_in(f: &Func, start: u32, insts: &[Ins], x: Option<&Exits>, img: Option<&Image>) -> (Vec<u32>, u8) {
    let mut next = Vec::new();
    let mut leaves = 0u8;
    let all = |m: &mut u8, v: u8| *m |= v;
    let ret = x.map_or(NZCV, |x| x.ret);
    let mut falls = true;
    for ins in insts {
        for s in &ins.stmts {
            match s {
                S::Goto(E::Const(t)) => {
                    let t = *t | (start & 1);
                    if f.blocks.contains(&t) {
                        next.push(t)
                    } else {
                        all(&mut leaves, x.map_or(NZCV, |x| x.entry(t) | ret));
                    }
                    if ins.cond == 14 {
                        falls = false;
                    }
                }
                S::Goto(_) => {
                    let cases = match (x, img) {
                        (Some(_), Some(img)) => crate::lift::computed_targets(f, img, ins.addr),
                        _ => Vec::new(),
                    };
                    if cases.is_empty() {
                        all(&mut leaves, if x.is_some() { 0 } else { NZCV });
                    } else {
                        next.extend(cases.into_iter().filter(|c| f.blocks.contains(c)));
                    }
                    if ins.cond == 14 {
                        falls = false;
                    }
                }
                S::ExcReturn(_) => {
                    all(&mut leaves, if x.is_some() { 0 } else { NZCV });
                    if ins.cond == 14 {
                        falls = false;
                    }
                }
                S::Call { .. } => all(&mut leaves, if x.is_some() { 0 } else { NZCV }),
                S::IfSome(..) => {
                    all(&mut leaves, if x.is_some() { 0 } else { NZCV });
                    // a block load of pc always jumps (a return)
                    if x.is_some() && ins.cond == 14 {
                        falls = false;
                    }
                }
                _ => {}
            }
        }
    }
    if falls {
        let last = insts.last().map(|i| i.addr).unwrap_or(start);
        let d = f.insts.get(&last);
        let end = d.map(|d: &Decoded| Func::next(last, d)).unwrap_or(last);
        if f.blocks.contains(&end) {
            next.push(end)
        } else {
            // runs on into the code after the function: a jump there
            all(&mut leaves, x.map_or(NZCV, |x| x.entry(end) | ret));
        }
    }
    (next, leaves)
}

/// Pass 1 result for one function: for each instruction (by address), the flags still needed after
/// it. A flag an instruction writes outside this mask is dead.
pub fn flag_liveness(f: &Func, img: Option<&Image>) -> BTreeMap<u32, u8> {
    flag_liveness_blocks(f, img).0
}

/// [`flag_liveness`], plus the flags live at each block start (what a slice end there must keep).
pub fn flag_liveness_blocks(f: &Func, img: Option<&Image>) -> (BTreeMap<u32, u8>, BTreeMap<u32, u8>) {
    let blocks: Vec<(u32, Vec<Ins>)> = f.blocks.iter().map(|&b| (b, ir::lower_block(f, b, img))).collect();
    let x = Exits::new(f);
    let x = x.as_ref();
    let sf = |s: &S, a: u32| stmt_flags_in(s, x, f, img, a);
    let succ: BTreeMap<u32, (Vec<u32>, u8)> = blocks.iter().map(|(b, i)| (*b, successors_in(f, *b, i, x, img))).collect();
    let mut live_in: BTreeMap<u32, u8> = blocks.iter().map(|(b, _)| (*b, 0)).collect();
    loop {
        let mut changed = false;
        for (b, insts) in blocks.iter().rev() {
            let (next, leaves) = &succ[b];
            let out = next.iter().map(|n| live_in[n]).fold(*leaves, |x, y| x | y);
            let v = block_live_in(insts, out, &sf);
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
        let mut live = next.iter().map(|n| live_in[n]).fold(*leaves, |x, y| x | y);
        for ins in insts.iter().rev() {
            after.insert(ins.addr, live);
            let mut inner = live;
            for s in ins.stmts.iter().rev() {
                let (r, w) = sf(s, ins.addr);
                inner = (inner & !w) | r;
            }
            live = if ins.cond == 14 { inner } else { inner | live | cond_reads(ins.cond) };
        }
    }
    (after, live_in)
}

/// One function's part of the whole-program flag facts, under the current [`FlagContext`]:
/// flags read at each block start (code may jump into the middle of a function), each call
/// (target, `None` if computed; flags live after it) and each jump out of the function to a
/// constant address.
pub fn flag_facts(f: &Func, img: Option<&Image>) -> (BTreeMap<u32, u8>, Vec<(Option<u32>, u8)>, Vec<u32>, u8) {
    let (after, live_in) = flag_liveness_blocks(f, img);
    let (mut calls, mut tails) = (Vec::new(), Vec::new());
    let mut restored = 0u8;
    for &b in &f.blocks {
        // registers holding a constant in this block (`ldr r1, =f; blx r1` calls f)
        let mut known: HashMap<u8, u32> = HashMap::new();
        let mut temps: HashMap<ir::Tmp, u32> = HashMap::new();
        for ins in ir::lower_block(f, b, img) {
            for s in &ins.stmts {
                fn value(e: &E, known: &HashMap<u8, u32>, temps: &HashMap<ir::Tmp, u32>) -> Option<u32> {
                    match e {
                        E::Const(t) => Some(*t),
                        E::Reg(i) => known.get(i).copied(),
                        E::Tmp(t) => temps.get(t).copied(),
                        E::Iw(r) => value(r, known, temps),
                        _ => None,
                    }
                }
                if let S::Let(t, e) = s {
                    match value(e, &known, &temps) {
                        Some(v) => temps.insert(*t, v),
                        None => temps.remove(t),
                    };
                }
                match s {
                    S::Do(E::Call(H::Msr, a)) if matches!(a.as_slice(), [E::Bool(false), E::Lit(m), _] if m & 8 != 0) => restored |= after[&ins.addr],
                    S::Call { target: t, .. } => calls.push((value(t, &known, &temps), after[&ins.addr])),
                    S::Goto(E::Const(t)) if !f.blocks.contains(&(*t | (b & 1))) => tails.push(*t),
                    _ => {}
                }
                // track constants (conditional writes and anything else forget the register)
                match s {
                    S::SetReg(i, E::Const(v)) if ins.cond == 14 && ins.guard.is_none() => {
                        known.insert(*i, *v);
                    }
                    S::SetReg(i, _) => {
                        known.remove(i);
                    }
                    S::Call { .. } | S::Do(_) | S::IfSome(..) => known.clear(),
                    _ => {}
                }
            }
        }
    }
    // blocks that run on past the function's end jump into the code there
    for &b in &f.blocks {
        let insts = ir::lower_block(f, b, img);
        let (next, _) = successors_in(f, b, &insts, None, None);
        let last = insts.last().map(|i| i.addr).unwrap_or(b);
        if let Some(d) = f.insts.get(&last) {
            let end = Func::next(last, d);
            let jumps = insts.iter().any(|i| i.cond == 14 && i.stmts.iter().any(|s| matches!(s, S::Goto(_) | S::ExcReturn(_) | S::IfSome(..))));
            if !jumps && !next.contains(&end) && !f.blocks.contains(&end) {
                tails.push(end);
            }
        }
    }
    (live_in, calls, tails, restored)
}

/// The constants the function's code puts in registers (candidate code addresses).
pub fn code_constants(f: &Func, img: Option<&Image>) -> Vec<u32> {
    let mut out = Vec::new();
    for &b in &f.blocks {
        for ins in ir::lower_block(f, b, img) {
            for s in &ins.stmts {
                if let S::SetReg(_, E::Const(v)) | S::Let(_, E::Const(v)) = s {
                    out.push(*v);
                }
            }
        }
    }
    out
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
