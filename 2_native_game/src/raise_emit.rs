//! Block 4B: the T1 emitter. One raised Rust function per eligible guest function:
//! - registers are local variables (`r0`..`r12`, `sp`, `lr`); flags stay in `c`;
//! - single-use temporaries are folded ([`ir::fold_temps`]) and dead flags dropped ([`raise`]);
//! - control flow is structured: `if`, labeled blocks and `loop` with `break`/`continue`, laid out
//!   over the dominator tree as in "Beyond Relooper" (N. Ramsey, ICFP 2022).
//!
//! Exactness rules (the run must stay identical to T0, instruction for instruction):
//! - every original block start still pays its budget with the same charge;
//! - before anything that reads or writes `c.r` directly (calls, slice ends, SWI, MSR, block
//!   transfers, every exit) the locals are written back ("spill"), and read again afterwards
//!   ("reload");
//! - T1 is installed at the entry only. Leaving early is always safe: a call that returns somewhere
//!   unexpected, or a slice end in the flat runtime, spills and returns the pc, and the dispatcher
//!   continues in the T0 translation, which stays registered for every block.
use crate::ir::{self, Width, E, H, S};
use crate::lift::{Func, Image};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;

/// Why a function stays T0.
pub type Reason = &'static str;

struct Block {
    start: u32,
    insts: Vec<ir::Ins>,
    /// Internal successors (blocks of this function reached by a constant jump or by falling through).
    succ: Vec<u32>,
    /// The address after the last instruction, if control can fall through.
    falls_to: Option<u32>,
    charge: u32,
}

struct Ctx<'a> {
    blocks: &'a BTreeMap<u32, Block>,
    rpo: BTreeMap<u32, usize>,
    idom: BTreeMap<u32, u32>,
    merge: BTreeSet<u32>,
    header: BTreeSet<u32>,
    children: BTreeMap<u32, Vec<u32>>,
}

/// Needs the locals written to `c.r` first (and read back after): the helper uses `c.r` itself.
fn syncs(e: &E) -> bool {
    matches!(e, E::Call(H::Block | H::BlockUser | H::Swi | H::Msr, _))
}

fn label(a: u32) -> String {
    format!("{:X}", a & !1)
}

/// Registers (as T1 locals) an expression or statement reads or writes.
fn regs_e(e: &E, set: &mut BTreeSet<u8>) {
    if let E::Reg(i) = e {
        set.insert(*i);
    }
    match e {
        E::Bin(_, a, b) => {
            regs_e(a, set);
            regs_e(b, set);
        }
        E::Not(a) | E::Shl(a, _) | E::Lsr(a, _) | E::Asr(a, _) | E::Ror(a, _) | E::Rrx(a) | E::Bit(a, _) | E::Msb(a) | E::Lsb(a) | E::BoolU32(a) | E::Clz(a) | E::Iw(a) => regs_e(a, set),
        E::Load { addr, .. } => regs_e(addr, set),
        E::Call(_, a) => a.iter().for_each(|x| regs_e(x, set)),
        _ => {}
    }
}

fn regs_s(s: &S, used: &mut BTreeSet<u8>, written: &mut BTreeSet<u8>) {
    match s {
        S::Let(_, e) | S::Let2(_, _, e) | S::SetFlag(_, e) | S::SetNz(e) | S::Do(e) | S::Goto(e) | S::ExcReturn(e) => regs_e(e, used),
        S::SetReg(i, e) => {
            written.insert(*i);
            regs_e(e, used);
        }
        S::Store { addr, val, .. } => {
            regs_e(addr, used);
            regs_e(val, used);
        }
        S::Call { target, .. } => regs_e(target, used),
        S::IfSome(e, b) => {
            regs_e(e, used);
            b.iter().for_each(|x| regs_s(x, used, written));
        }
    }
}

/// The raised (T1) Rust function `f_<id>_t1` for `f`, or why it stays T0. `t0` is the name of the
/// exact translation it falls back to when entered anywhere but its entry.
pub fn emit_t1(f: &Func, id: &str, img: Option<&Image>) -> Result<String, Reason> {
    let live = crate::raise::flag_liveness(f, img);
    // blocks reachable from the entry through internal edges
    let mut blocks: BTreeMap<u32, Block> = BTreeMap::new();
    let mut work = vec![f.entry];
    while let Some(b) = work.pop() {
        if blocks.contains_key(&b) || !f.blocks.contains(&b) {
            continue;
        }
        let mut insts = ir::lower_block(f, b, img);
        if insts.is_empty() {
            return Err("empty block");
        }
        let thumb = b & 1;
        let mut succ = Vec::new();
        let mut falls = true;
        for ins in &mut insts {
            crate::raise::drop_dead_flags(&mut ins.stmts, live[&ins.addr]);
            ir::fold_temps(&mut ins.stmts);
            for s in &ins.stmts {
                let unconditional = ins.cond == 14;
                match s {
                    S::Goto(E::Const(t)) => {
                        let t = (*t & !1) | thumb;
                        if f.blocks.contains(&t) {
                            succ.push(t);
                        }
                        falls &= !unconditional;
                    }
                    S::Goto(_) | S::ExcReturn(_) => falls &= !unconditional,
                    _ => {}
                }
            }
        }
        let last = insts.last().unwrap().addr;
        let next = Func::next(last, &f.insts[&last]);
        let falls_to = falls.then_some(next);
        if falls && f.blocks.contains(&next) {
            succ.push(next);
        }
        succ.dedup();
        work.extend(succ.iter().copied());
        let charge = insts.len() as u32;
        blocks.insert(b, Block { start: b, insts, succ, falls_to, charge });
    }
    // reverse postorder
    let mut order = Vec::new();
    let mut seen = BTreeSet::new();
    fn dfs(b: u32, blocks: &BTreeMap<u32, Block>, seen: &mut BTreeSet<u32>, order: &mut Vec<u32>) {
        if !seen.insert(b) {
            return;
        }
        for &s in &blocks[&b].succ {
            dfs(s, blocks, seen, order);
        }
        order.push(b);
    }
    dfs(f.entry, &blocks, &mut seen, &mut order);
    order.reverse();
    let rpo: BTreeMap<u32, usize> = order.iter().enumerate().map(|(i, &b)| (b, i)).collect();
    let mut preds: BTreeMap<u32, Vec<u32>> = order.iter().map(|&b| (b, Vec::new())).collect();
    for &b in &order {
        for &s in &blocks[&b].succ {
            preds.get_mut(&s).unwrap().push(b);
        }
    }
    // dominators (Cooper, Harvey, Kennedy)
    let mut idom: BTreeMap<u32, u32> = BTreeMap::new();
    idom.insert(f.entry, f.entry);
    loop {
        let mut changed = false;
        for &b in order.iter().skip(1) {
            let mut new: Option<u32> = None;
            for &p in &preds[&b] {
                if !idom.contains_key(&p) {
                    continue;
                }
                new = Some(match new {
                    None => p,
                    Some(mut a) => {
                        let mut q = p;
                        while a != q {
                            while rpo[&a] > rpo[&q] {
                                a = idom[&a];
                            }
                            while rpo[&q] > rpo[&a] {
                                q = idom[&q];
                            }
                        }
                        a
                    }
                });
            }
            if let Some(n) = new {
                if idom.get(&b) != Some(&n) {
                    idom.insert(b, n);
                    changed = true;
                }
            }
        }
        if !changed {
            break;
        }
    }
    let dominates = |a: u32, mut b: u32| loop {
        if a == b {
            return true;
        }
        if b == f.entry {
            return false;
        }
        b = idom[&b];
    };
    let mut merge = BTreeSet::new();
    let mut header = BTreeSet::new();
    for &b in &order {
        let mut forward = 0;
        for &p in &preds[&b] {
            if rpo[&p] >= rpo[&b] {
                if !dominates(b, p) {
                    return Err("irreducible control flow");
                }
                header.insert(b);
            } else {
                forward += 1;
            }
        }
        if forward >= 2 {
            merge.insert(b);
        }
    }
    let mut children: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
    for &b in order.iter().skip(1) {
        children.entry(idom[&b]).or_default().push(b);
    }
    // registers the function touches
    let (mut used, mut written) = (BTreeSet::new(), BTreeSet::new());
    for b in blocks.values() {
        for ins in &b.insts {
            ins.stmts.iter().for_each(|s| regs_s(s, &mut used, &mut written));
        }
    }
    let all: BTreeSet<u8> = used.union(&written).copied().filter(|&i| i < 15).collect();
    let ctx = Ctx { blocks: &blocks, rpo, idom, merge, header, children };
    let mut body = String::new();
    do_tree(&ctx, f.entry, 2, &mut body);

    let mut s = String::new();
    let _ = writeln!(s, "#[allow(non_snake_case, unused_variables, unused_assignments, unused_mut, unreachable_code, unused_parens, unused_braces, unused_macros, unused_labels, clippy::all)]");
    let _ = writeln!(s, "pub fn f_{id}_t1(c: &mut recomp::rt::Cpu, m: &mut dyn recomp::rt::Bus, pc: u32) -> u32 {{");
    let _ = writeln!(s, "    #[allow(unused_imports)] use recomp::rt as R;\n    #[allow(unused_imports)] use recomp::arm::interwork as iw;\n    #[allow(unused_imports)] use recomp::rt::Mem as _;");
    let _ = writeln!(s, "    if pc != {:#X} || !R::t1_enabled({:#X}) {{ return f_{id}(c, m, pc); }}", f.entry, f.entry);
    let names: Vec<String> = all.iter().map(|&i| ir::reg_name(i)).collect();
    if !all.is_empty() {
        let loads: Vec<String> = all.iter().map(|&i| format!("c.r[{i}]")).collect();
        let _ = writeln!(s, "    let ({}) = ({});", names.iter().map(|n| format!("mut {n}")).collect::<Vec<_>>().join(", "), loads.join(", "));
    }
    let spill: String = written.iter().filter(|&&i| i < 15).map(|&i| format!("c.r[{i}] = {}; ", ir::reg_name(i))).collect();
    let reload: String = all.iter().map(|&i| format!("{} = c.r[{i}]; ", ir::reg_name(i))).collect();
    let _ = writeln!(s, "    macro_rules! spill {{ () => {{ {spill}}} }}");
    let _ = writeln!(s, "    macro_rules! reload {{ () => {{ {reload}}} }}");
    // leaving for anywhere but a fixed block of this function: continue exactly as the translation
    // would (it runs the target if it is one of its blocks, else returns it at once)
    let _ = writeln!(s, "    macro_rules! exit {{ ($e:expr) => {{{{ let t: u32 = $e; spill!(); return f_{id}(c, m, t); }}}} }}");
    // the same when `c.r` is already newer than the locals (after a call, a block load or a mode switch)
    let _ = writeln!(s, "    macro_rules! leave {{ ($e:expr) => {{{{ let t: u32 = $e; return f_{id}(c, m, t); }}}} }}");
    let _ = writeln!(s, "    macro_rules! budget {{ ($pc:expr, $n:expr) => {{ if !R::spend(c, $n) {{ spill!(); if !m.slice_end(c, $pc, $n) {{ c.r[15] = $pc; return R::BUDGET_EXHAUSTED; }} reload!(); }} }} }}");
    s.push_str(&body);
    s.push_str("}\n");
    Ok(s)
}

fn indent(out: &mut String, depth: usize) {
    for _ in 0..depth {
        out.push_str("    ");
    }
}

/// Code for the dominator subtree rooted at `x`.
fn do_tree(c: &Ctx, x: u32, depth: usize, out: &mut String) {
    let mut ys: Vec<u32> = c.children.get(&x).map(|v| v.iter().copied().filter(|y| c.merge.contains(y)).collect()).unwrap_or_default();
    ys.sort_by_key(|y| std::cmp::Reverse(c.rpo[y]));
    if c.header.contains(&x) {
        indent(out, depth);
        let _ = writeln!(out, "'L{}: loop {{", label(x));
        node_within(c, x, &ys, depth + 1, out);
        indent(out, depth);
        out.push_str("}\n");
    } else {
        node_within(c, x, &ys, depth, out);
    }
}

/// `x`'s code inside labeled blocks, each followed by the code of one merge child.
fn node_within(c: &Ctx, x: u32, ys: &[u32], depth: usize, out: &mut String) {
    match ys.split_first() {
        None => block_code(c, x, depth, out),
        Some((&y, rest)) => {
            indent(out, depth);
            let _ = writeln!(out, "'B{}: {{", label(y));
            node_within(c, x, rest, depth + 1, out);
            indent(out, depth);
            out.push_str("}\n");
            do_tree(c, y, depth, out);
        }
    }
}

/// A jump from block `x` to block `y` of this function.
fn branch(c: &Ctx, x: u32, y: u32, depth: usize, out: &mut String) {
    if c.rpo[&y] <= c.rpo[&x] {
        indent(out, depth);
        let _ = writeln!(out, "continue 'L{};", label(y));
    } else if c.merge.contains(&y) {
        indent(out, depth);
        let _ = writeln!(out, "break 'B{};", label(y));
    } else {
        debug_assert_eq!(c.idom[&y], x);
        do_tree(c, y, depth, out);
    }
}

fn block_code(c: &Ctx, x: u32, depth: usize, out: &mut String) {
    let b = &c.blocks[&x];
    indent(out, depth);
    let _ = writeln!(out, "budget!({:#X}, {}); // {:#X}", b.start, b.charge, b.start & !1);
    for ins in &b.insts {
        let mut text = String::new();
        let inner = if ins.cond == 14 { depth } else { depth + 1 };
        for s in &ins.stmts {
            stmt_code(c, x, s, inner, &mut text);
        }
        if text.is_empty() {
            continue;
        }
        if ins.cond == 14 {
            out.push_str(&text);
        } else {
            indent(out, depth);
            let _ = writeln!(out, "if {} {{", ir::cond_expr(ins.cond));
            out.push_str(&text);
            indent(out, depth);
            out.push_str("}\n");
        }
    }
    if let Some(next) = b.falls_to {
        if c.blocks[&x].succ.contains(&next) && c.rpo.contains_key(&next) {
            branch(c, x, next, depth, out);
        } else {
            indent(out, depth);
            let _ = writeln!(out, "exit!({next:#X});");
        }
    }
}

fn stmt_code(c: &Ctx, x: u32, s: &S, depth: usize, out: &mut String) {
    let thumb = x & 1;
    let line = |out: &mut String, t: &str| {
        indent(out, depth);
        out.push_str(t);
        out.push('\n');
    };
    match s {
        S::Goto(E::Const(t)) if c.rpo.contains_key(&((*t & !1) | thumb)) => branch(c, x, (*t & !1) | thumb, depth, out),
        S::Goto(e) => line(out, &format!("exit!({});", ir::expr_locals(e))),
        S::ExcReturn(e) => {
            let v = ir::expr_locals(e);
            line(out, &format!("spill!(); let v = {v}; let th = c.restore_spsr(); leave!(if th {{ v | 1 }} else {{ v & !3 }});"))
        }
        S::Call { target, ret } => line(
            out,
            &format!("spill!(); let x = m.call(c, {}, {ret:#X}); if x != {ret:#X} {{ leave!(x); }} reload!();", ir::expr_locals(target)),
        ),
        S::IfSome(e, body) => {
            let exit = match body.as_slice() {
                [S::Goto(E::Iw(v))] if **v == E::Tmp(ir::Tmp::V) => "leave!(iw(v));".to_string(),
                [S::ExcReturn(E::Tmp(ir::Tmp::V))] => "let th = c.restore_spsr(); leave!(if th { v | 1 } else { v & !3 });".to_string(),
                _ => unreachable!("block loads only jump"),
            };
            line(out, &format!("spill!(); if let Some(v) = {} {{ {exit} }} reload!();", ir::expr_locals(e)))
        }
        S::Do(e) if syncs(e) => line(out, &format!("spill!(); {}; reload!();", ir::expr_locals(e))),
        S::SetReg(i, e) if syncs(e) => line(out, &format!("spill!(); let t = {}; reload!(); {} = t;", ir::expr_locals(e), ir::reg_name(*i))),
        S::Store { w: Width::B32, .. } | _ => line(out, &ir::print_locals(std::slice::from_ref(s))),
    }
}
