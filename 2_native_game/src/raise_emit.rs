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
use crate::ir::{self, CmpOp, Width, E, H, S};
use crate::lift::{Func, Image};
use std::collections::{BTreeMap, BTreeSet, HashMap};
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
    /// The named field accesses, per statement.
    fields: FieldMap,
    rpo: BTreeMap<u32, usize>,
    idom: BTreeMap<u32, u32>,
    merge: BTreeSet<u32>,
    header: BTreeSet<u32>,
    children: BTreeMap<u32, Vec<u32>>,
    names: &'a dyn Fn(u32) -> Option<String>,
    notes: &'a Notes<'a>,
    /// Block -> the cases of the compiled switch ending it.
    switches: &'a BTreeMap<u32, Vec<u32>>,
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
        E::Bin(_, a, b) | E::Cmp(_, _, a, b) => {
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
    emit_t1_named(f, id, img, &|_| None)
}

/// The condition `cc` after a compare as one expression, if it has a plain form: `cmp a, b`
/// (`R::subs`), `cmn a, b` (`R::adds`, Z/N only) or a flag-setting logical result (Z/N only).
fn fold_cond(cmp: &[S], cc: u8) -> Option<E> {
    use CmpOp::*;
    let b = |e: &E| Box::new(e.clone());
    let zn = |v: &E| -> Option<E> {
        let zero = Box::new(E::Lit(0));
        Some(match cc {
            0 => E::Cmp(Eq, false, b(v), zero),
            1 => E::Cmp(Ne, false, b(v), zero),
            4 => E::Cmp(Lt, true, b(v), zero),
            5 => E::Cmp(Ge, true, b(v), zero),
            _ => return None,
        })
    };
    match cmp {
        [S::Do(E::Call(H::Subs, a))] if a.len() == 3 => {
            let (x, y) = (&a[1], &a[2]);
            Some(match cc {
                0 => E::Cmp(Eq, false, b(x), b(y)),
                1 => E::Cmp(Ne, false, b(x), b(y)),
                2 => E::Cmp(Ge, false, b(x), b(y)),
                3 => E::Cmp(Lt, false, b(x), b(y)),
                8 => E::Cmp(Gt, false, b(x), b(y)),
                9 => E::Cmp(Le, false, b(x), b(y)),
                10 => E::Cmp(Ge, true, b(x), b(y)),
                11 => E::Cmp(Lt, true, b(x), b(y)),
                12 => E::Cmp(Gt, true, b(x), b(y)),
                13 => E::Cmp(Le, true, b(x), b(y)),
                4 | 5 => return zn(&E::Bin(ir::Bin::Sub, b(x), b(y))),
                _ => return None,
            })
        }
        [S::Do(E::Call(H::Adds, a))] if a.len() == 3 => zn(&E::Bin(ir::Bin::Add, b(&a[1]), b(&a[2]))),
        [S::SetNz(v)] => zn(v),
        _ => None,
    }
}

/// `cmp`/`cmn`/`tst`-like instruction followed by one conditional instruction that is the only
/// reader of the flags: the compare goes, the condition becomes an expression (`if r0 == 0x13`).
fn fold_compares(insts: &mut [ir::Ins], live: &BTreeMap<u32, u8>) {
    'outer: for k in 0..insts.len().saturating_sub(1) {
        if insts[k].cond != 14 || fold_cond(&insts[k].stmts, 0).is_none() {
            continue;
        }
        // registers the compare reads: nothing in between may change them
        let (mut read, mut _w) = (BTreeSet::new(), BTreeSet::new());
        insts[k].stmts.iter().for_each(|s| regs_s(s, &mut read, &mut _w));
        let mut j = k + 1;
        while j < insts.len() {
            let ins = &insts[j];
            if ins.cond != 14 {
                break;
            }
            // a plain instruction: no flags, no sync, no control transfer, keeps the compared registers
            let (mut r, mut w) = (BTreeSet::new(), BTreeSet::new());
            let mut flags = 0;
            for s in &ins.stmts {
                regs_s(s, &mut r, &mut w);
                let (fr, fw) = crate::raise::stmt_flags(s);
                flags |= fr | fw;
                if matches!(s, S::Goto(_) | S::Call { .. } | S::IfSome(..) | S::ExcReturn(_)) || matches!(s, S::Do(e) if syncs(e)) {
                    continue 'outer;
                }
            }
            if flags != 0 || w.iter().any(|x| read.contains(x)) {
                continue 'outer;
            }
            j += 1;
        }
        if j >= insts.len() {
            continue;
        }
        // the readers: a run of instructions with the same condition, evaluated once
        let cc = insts[j].cond;
        let Some(e) = cond_run(insts, j) else { continue };
        if insts[j..=e].iter().any(|i| i.guard.is_some()) || live[&insts[e].addr] != 0 {
            continue;
        }
        if live[&insts[k].addr] & !crate::raise::cond_reads(cc) != 0 {
            continue;
        }
        if let Some(g) = fold_cond(&insts[k].stmts, cc) {
            insts[j].guard = Some(g);
            insts[k].stmts.clear();
            join(insts, j, e);
        }
    }
}

/// Ends a statement list's run of same-condition instructions: control leaves or the flags may
/// change (a call, a CPU sync).
fn ends_run(s: &S) -> bool {
    matches!(s, S::Goto(_) | S::Call { .. } | S::IfSome(..) | S::ExcReturn(_)) || matches!(s, S::Do(e) | S::SetReg(_, e) if syncs(e))
}

/// The last instruction of the run starting at `j`: following instructions with the same
/// condition while none reads a flag besides its condition, and none but the last writes flags or
/// ends a run (control leaves, a call). `None` if an instruction reads flags itself.
fn cond_run(insts: &[ir::Ins], j: usize) -> Option<usize> {
    let cc = insts[j].cond;
    let mut e = j;
    loop {
        let ins = &insts[e];
        if ins.cond != cc {
            return None;
        }
        let mut ends = false;
        for s in &ins.stmts {
            let (r, w) = crate::raise::stmt_flags(s);
            let exit = matches!(s, S::Goto(_) | S::Call { .. });
            if r != 0 && !exit {
                return None;
            }
            // a flag write ends the run: later instructions would read the new flags
            ends |= ends_run(s) || w != 0;
        }
        if ends || e + 1 >= insts.len() || insts[e + 1].cond != cc || insts[e + 1].guard.is_some() {
            return Some(e);
        }
        e += 1;
    }
}

/// Moves the statements of instructions `j+1..=e` into instruction `j` (one `if`).
fn join(insts: &mut [ir::Ins], j: usize, e: usize) {
    for i in j + 1..=e {
        let moved = std::mem::take(&mut insts[i].stmts);
        insts[j].stmts.extend(moved);
        insts[i].cond = 14;
    }
}

/// Joins the remaining runs of conditional instructions (same condition, see [`cond_run`]) into
/// one `if` each.
fn join_runs(insts: &mut [ir::Ins]) {
    let mut j = 0;
    while j < insts.len() {
        if insts[j].cond != 14 && insts[j].guard.is_none() {
            if let Some(e) = cond_run(insts, j) {
                join(insts, j, e);
                j = e + 1;
                continue;
            }
        }
        j += 1;
    }
}

/// `{r4, r5, lr}` for a register list.
fn reg_list(list: u32) -> String {
    let regs: Vec<String> = (0..16).filter(|i| list >> i & 1 != 0).map(|i| if i == 15 { "pc".into() } else { ir::reg_name(i as u8) }).collect();
    format!("{{{}}}", regs.join(", "))
}

/// A readable note for a block transfer: `push {r4, lr}`, `pop {r4, pc}`, else `ldm`/`stm`.
fn block_note(e: &E) -> String {
    if let E::Call(H::Block | H::BlockUser, a) = e {
        if let [_, _, E::Bool(load), E::Bool(pre), E::Bool(up), E::Bool(wb), E::Lit(rn), E::Hex(list), _] = a.as_slice() {
            let what = match (load, pre, up, wb, rn) {
                (false, true, false, true, 13) => "push".to_string(),
                (true, false, true, true, 13) => "pop".to_string(),
                _ => format!("{} {}{}", if *load { "ldm" } else { "stm" }, ir::reg_name(*rn as u8), if *wb { "!" } else { "" }),
            };
            return format!(" // {what} {}", reg_list(*list));
        }
    }
    String::new()
}

/// Comment sources for raised code: call target names, field names by offset (for this function)
/// and global names by address.
pub struct Notes<'a> {
    pub call: &'a dyn Fn(u32) -> Option<String>,
    /// The named field at an offset (from a base register) in this function: printed as an accessor.
    pub field: &'a dyn Fn(u32) -> Option<ir::FieldRef>,
    /// The structs this function is known to use a field of at an offset (struct inference votes).
    pub fields_of: &'a dyn Fn(u32) -> Vec<String>,
    /// A struct's field at an offset.
    pub field_in: &'a dyn Fn(&str, u32) -> Option<ir::FieldRef>,
    /// A struct's Rust module name (for local names: `kart`).
    pub module_of: &'a dyn Fn(&str) -> Option<String>,
    /// The struct an argument register (r0..r3) points to, as the callers pass it (votes).
    pub arg_hint: &'a dyn Fn(u8) -> Option<String>,
    /// The struct the pointer stored at a global address points to (`*0x021755FC` is a race manager).
    pub global_points: &'a dyn Fn(u32) -> Option<String>,
    /// The struct the pointer in a struct's field points to.
    pub field_points: &'a dyn Fn(&str, u32) -> Option<String>,
    /// Every struct with a known field at an offset (layout matching for values without votes).
    pub structs_with: &'a dyn Fn(u32) -> Vec<String>,
    /// The share of a struct's word slots that have a known field (how easily offsets match it).
    pub density: &'a dyn Fn(&str) -> f64,
    pub global: &'a dyn Fn(u32) -> Option<String>,
}

impl Notes<'static> {
    /// No comments, no field names.
    pub const NONE: Notes<'static> = Notes { call: &|_| None, field: &|_| None, fields_of: &|_| Vec::new(), field_in: &|_, _| None, module_of: &|_| None, arg_hint: &|_| None, global_points: &|_| None, field_points: &|_, _| None, structs_with: &|_| Vec::new(), density: &|_| 1.0, global: &|_| None };
}

/// [`emit_t1`]; `names(target)` gives a call target's name for a comment.
pub fn emit_t1_named(f: &Func, id: &str, img: Option<&Image>, names: &dyn Fn(u32) -> Option<String>) -> Result<String, Reason> {
    emit_t1_annotated(f, id, img, &Notes { call: names, ..Notes::NONE })
}

/// The comment for one statement: the global a constant address or value names. (Struct fields
/// print as accessors, see [`infer_fields`].)
fn stmt_note(n: &Notes, s: &S) -> Option<String> {
    fn addr_of(e: &E) -> Option<&E> {
        match e {
            E::Load { addr, .. } => Some(addr),
            E::Call(H::Ldr, a) => a.get(1),
            E::Bin(_, a, b) | E::Cmp(_, _, a, b) => addr_of(a).or_else(|| addr_of(b)),
            _ => None,
        }
    }
    let (access, value) = match s {
        S::Store { addr, val, .. } => (Some(addr), Some(val)),
        S::SetReg(_, e) | S::Let(_, e) => (addr_of(e), Some(e)),
        _ => (None, None),
    };
    if let Some(a) = access {
        if let E::Const(g) = a {
            if let Some(g) = (n.global)(*g) {
                return Some(g);
            }
        }
    }
    if let Some(E::Const(v)) = value {
        return (n.global)(*v).map(|g| format!("&{g}"));
    }
    None
}

/// Accesses to name, per printed statement: `(block, instruction, statement index)` (the index
/// `usize::MAX` is the instruction's guard) -> `(base register, offset)` -> field.
type FieldMap = HashMap<(u32, u32, usize), HashMap<(u8, u32), ir::FieldRef>>;

/// `base + off` memory accesses in `e` with a register base (not `sp`/`pc`).
fn accesses_e(e: &E, out: &mut Vec<(u8, u32)>) {
    fn at(a: &E, out: &mut Vec<(u8, u32)>) {
        let a = match a {
            E::Bin(ir::Bin::And, x, k) if matches!(**k, E::Not(_)) => &**x,
            _ => a,
        };
        match a {
            E::Bin(ir::Bin::Add, b, k) => {
                if let (E::Reg(r), E::Const(v)) = (&**b, &**k) {
                    if *r < 13 {
                        out.push((*r, *v));
                    }
                }
            }
            E::Reg(r) if *r < 13 => out.push((*r, 0)),
            _ => {}
        }
    }
    match e {
        E::Load { addr, .. } => {
            at(addr, out);
            accesses_e(addr, out);
        }
        E::Call(H::Ldr, a) if a.len() == 2 => {
            at(&a[1], out);
            accesses_e(&a[1], out);
        }
        E::Bin(_, a, b) | E::Cmp(_, _, a, b) => {
            accesses_e(a, out);
            accesses_e(b, out);
        }
        E::Not(a) | E::Shl(a, _) | E::Lsr(a, _) | E::Asr(a, _) | E::Ror(a, _) | E::Rrx(a) | E::Bit(a, _) | E::Msb(a) | E::Lsb(a) | E::BoolU32(a) | E::Clz(a) | E::Iw(a) => accesses_e(a, out),
        E::Call(_, a) => a.iter().for_each(|x| accesses_e(x, out)),
        _ => {}
    }
}

fn accesses_s(s: &S, out: &mut Vec<(u8, u32)>) {
    match s {
        S::Let(_, e) | S::Let2(_, _, e) | S::SetFlag(_, e) | S::SetNz(e) | S::Do(e) | S::Goto(e) | S::ExcReturn(e) | S::SetReg(_, e) => accesses_e(e, out),
        S::Store { addr, val, .. } => {
            let mut a = Vec::new();
            accesses_e(&E::Load { w: ir::Width::B8, signed: false, addr: Box::new(addr.clone()) }, &mut a);
            out.extend(a);
            accesses_e(val, out);
        }
        S::Call { target, .. } => accesses_e(target, out),
        S::IfSome(e, b) => {
            accesses_e(e, out);
            b.iter().for_each(|x| accesses_s(x, out));
        }
    }
}

/// Registers a statement writes, each with the register it copies (`r6 = r0`), if a plain copy.
/// Calls clobber the argument registers, `r12` and `lr` (the ARM calling convention).
fn defs_s(s: &S) -> Vec<(u8, Option<u8>)> {
    fn defs_e(e: &E, out: &mut Vec<(u8, Option<u8>)>) {
        match e {
            E::Call(H::Block | H::BlockUser, a) if a.len() == 9 => {
                if a[2] == E::Bool(true) {
                    if let E::Hex(list) = a[7] {
                        out.extend((0..15u8).filter(|i| list >> i & 1 != 0).map(|i| (i, None)));
                    }
                }
                if let (E::Bool(true), E::Lit(rn)) = (&a[5], &a[6]) {
                    out.push((*rn as u8, None));
                }
            }
            E::Call(H::Swi, _) => out.extend((0..4u8).map(|i| (i, None))),
            _ => {}
        }
    }
    let mut out = Vec::new();
    match s {
        S::SetReg(i, E::Reg(j)) => out.push((*i, Some(*j))),
        S::SetReg(i, e) => {
            defs_e(e, &mut out);
            out.push((*i, None));
        }
        S::Do(e) | S::IfSome(e, _) => defs_e(e, &mut out),
        S::Call { .. } => out.extend([0u8, 1, 2, 3, 12, 14].map(|i| (i, None))),
        _ => {}
    }
    out
}

/// Struct inference: which struct each base register points to. Reaching definitions give each
/// register read the values it can hold; reads sharing a value, and plain copies, form one group.
/// Each access in a group votes for the structs this function is known to use at that offset;
/// a unique winner names every access of the group that is one of its fields. Accesses without a
/// winner keep the per-offset rule (`notes.field`).
/// What struct inference found: the named accesses, the registers that hold one struct pointer
/// wherever they are read (register -> struct), and the arguments known to point to a struct.
struct Typed {
    fields: FieldMap,
    regs: BTreeMap<u8, String>,
    args: BTreeMap<u8, String>,
    /// Struct-typed arguments at calls: (target, register, struct).
    calls: Vec<(u32, u8, String)>,
}

/// The types the last [`emit_t1_full`] found: its struct-typed arguments (register -> struct) and
/// the struct-typed arguments it passes (target, register, struct), for whole-program propagation.
pub type Types = (BTreeMap<u8, String>, Vec<(u32, u8, String)>);

thread_local! {
    static LAST_TYPES: std::cell::RefCell<Types> = std::cell::RefCell::new(Default::default());
}

/// Takes the types found by the last emission (empty if none).
pub fn take_types() -> Types {
    LAST_TYPES.with(|t| std::mem::take(&mut *t.borrow_mut()))
}

fn infer_fields(blocks: &BTreeMap<u32, Block>, order: &[u32], preds: &BTreeMap<u32, Vec<u32>>, entry: u32, notes: &Notes) -> Typed {
    type St = Vec<BTreeSet<usize>>;
    let mut ids: HashMap<(u32, usize, usize, u8), usize> = HashMap::new();
    let mut id = |k: (u32, usize, usize, u8)| -> usize {
        let n = ids.len() + 16;
        *ids.entry(k).or_insert(n)
    };
    let start: St = (0..16).map(|i| BTreeSet::from([i])).collect();
    let mut ins_state: HashMap<u32, St> = HashMap::new();
    let mut out_state: HashMap<u32, St> = HashMap::new();
    // reaching definitions to a fixed point
    loop {
        let mut changed = false;
        for &b in order {
            let mut st: St = if b == entry { start.clone() } else { vec![BTreeSet::new(); 16] };
            for p in preds.get(&b).into_iter().flatten() {
                if let Some(o) = out_state.get(p) {
                    for r in 0..16 {
                        st[r].extend(o[r].iter().copied());
                    }
                }
            }
            if ins_state.get(&b) == Some(&st) && out_state.contains_key(&b) {
                continue;
            }
            ins_state.insert(b, st.clone());
            for (ii, ins) in blocks[&b].insts.iter().enumerate() {
                let cond = ins.cond != 14 || ins.guard.is_some();
                for (si, s) in ins.stmts.iter().enumerate() {
                    for (r, _) in defs_s(s) {
                        let d = id((b, ii, si, r));
                        let set = &mut st[r as usize & 15];
                        if !cond {
                            set.clear();
                        }
                        set.insert(d);
                    }
                }
            }
            out_state.insert(b, st);
            changed = true;
        }
        if !changed {
            break;
        }
    }
    // group values: union-find over definitions
    let mut parent: Vec<usize> = (0..ids.len() + 16).collect();
    fn find(p: &mut [usize], mut x: usize) -> usize {
        while p[x] != x {
            p[x] = p[p[x]];
            x = p[x];
        }
        x
    }
    let union = |p: &mut Vec<usize>, a: usize, b: usize| {
        let (a, b) = (find(p, a), find(p, b));
        if a != b {
            p[a.max(b)] = a.min(b);
        }
    };
    let mut uses: Vec<((u32, u32, usize), u8, u32, usize)> = Vec::new();
    // argument registers at calls with a known target: (target, register, a reaching definition)
    let mut call_args: Vec<(u32, u8, usize)> = Vec::new();
    // word loads into registers: (definition, global address or (base definition, offset))
    let mut loads: Vec<(usize, Result<u32, (usize, u32)>)> = Vec::new();
    // every register read: (register, a reaching definition); its reaching definitions are united
    let mut reads_all: Vec<(u8, usize)> = Vec::new();
    let read_regs = |e: &[&E], st: &St, parent: &mut Vec<usize>, reads_all: &mut Vec<(u8, usize)>| {
        let mut used = BTreeSet::new();
        e.iter().for_each(|x| regs_e(x, &mut used));
        for r in used.into_iter().filter(|&r| r < 13) {
            let defs = &st[r as usize];
            let Some(&first) = defs.iter().next() else { continue };
            for &d in defs {
                union(parent, first, d);
            }
            reads_all.push((r, first));
        }
    };
    for &b in order {
        let Some(mut st) = ins_state.get(&b).cloned() else { continue };
        for (ii, ins) in blocks[&b].insts.iter().enumerate() {
            let cond = ins.cond != 14 || ins.guard.is_some();
            let mut reads = |key: (u32, u32, usize), acc: Vec<(u8, u32)>, st: &St, parent: &mut Vec<usize>| {
                for (r, off) in acc {
                    let defs = &st[r as usize];
                    let Some(&first) = defs.iter().next() else { continue };
                    for &d in defs {
                        union(parent, first, d);
                    }
                    uses.push((key, r, off, first));
                }
            };
            if let Some(g) = &ins.guard {
                let mut acc = Vec::new();
                accesses_e(g, &mut acc);
                reads((b, ins.addr, usize::MAX), acc, &st, &mut parent);
                read_regs(&[g], &st, &mut parent, &mut reads_all);
            }
            for (si, s) in ins.stmts.iter().enumerate() {
                let mut acc = Vec::new();
                accesses_s(s, &mut acc);
                reads((b, ins.addr, si), acc, &st, &mut parent);
                if let S::Call { target: E::Const(t), .. } = s {
                    for r in 0..4u8 {
                        if let Some(&d) = st[r as usize].iter().next() {
                            if st[r as usize].len() == 1 {
                                call_args.push((*t, r, d));
                            }
                        }
                    }
                }
                let (mut used, mut _w) = (BTreeSet::new(), BTreeSet::new());
                regs_s(s, &mut used, &mut _w);
                let regs: Vec<E> = used.into_iter().map(E::Reg).collect();
                read_regs(&regs.iter().collect::<Vec<_>>(), &st, &mut parent, &mut reads_all);
                if let S::SetReg(i, e) = s {
                    let addr = match e {
                        E::Call(H::Ldr, a) if a.len() == 2 => Some(&a[1]),
                        E::Load { w: ir::Width::B32, addr, .. } => Some(match &**addr {
                            E::Bin(ir::Bin::And, x, k) if matches!(**k, E::Not(_)) => &**x,
                            a => a,
                        }),
                        _ => None,
                    };
                    let from = match addr {
                        Some(E::Const(g)) => Some(Ok(*g)),
                        Some(E::Bin(ir::Bin::Add, x, k)) => match (&**x, &**k) {
                            (E::Reg(r), E::Const(off)) if *r < 13 => st[*r as usize].iter().next().map(|&bd| Err((bd, *off))),
                            _ => None,
                        },
                        Some(E::Reg(r)) if *r < 13 => st[*r as usize].iter().next().map(|&bd| Err((bd, 0))),
                        _ => None,
                    };
                    if let (Some(from), Some(&d)) = (from, ids.get(&(b, ii, si, *i))) {
                        loads.push((d, from));
                    }
                }
                for (r, src) in defs_s(s) {
                    let d = ids[&(b, ii, si, r)];
                    if let Some(j) = src {
                        for &x in &st[j as usize & 15] {
                            union(&mut parent, d, x);
                        }
                    }
                    let set = &mut st[r as usize & 15];
                    if !cond {
                        set.clear();
                    }
                    set.insert(d);
                }
            }
        }
    }
    // vote per group
    let mut offs: HashMap<usize, BTreeSet<u32>> = HashMap::new();
    for u in &uses {
        let g = find(&mut parent, u.3);
        offs.entry(g).or_default().insert(u.2);
    }
    let mut votes: HashMap<usize, BTreeMap<String, usize>> = HashMap::new();
    for (g, ks) in &offs {
        let score = votes.entry(*g).or_default();
        for &k in ks {
            for st in (notes.fields_of)(k) {
                *score.entry(st).or_default() += 1;
            }
        }
    }
    // what the callers pass counts double
    for r in 0..4u8 {
        if let Some(st) = (notes.arg_hint)(r) {
            let g = find(&mut parent, r as usize);
            *votes.entry(g).or_default().entry(st).or_default() += 2;
        }
    }
    let decide = |votes: &HashMap<usize, BTreeMap<String, usize>>| -> HashMap<usize, String> {
        let mut winner = HashMap::new();
        for (g, score) in votes {
            let best = score.values().copied().max().unwrap_or(0);
            let mut top = score.iter().filter(|(_, v)| **v == best && best > 0);
            if let (Some((st, _)), None) = (top.next(), top.next()) {
                winner.insert(*g, st.clone());
            }
        }
        winner
    };
    let mut winner = decide(&votes);
    // pointers loaded from typed globals and from pointer fields of typed values vote for what
    // they point to; a newly typed value can type the pointers loaded from it, so repeat
    for _ in 0..4 {
        let mut more = votes.clone();
        for (d, from) in &loads {
            let st = match from {
                Ok(g) => (notes.global_points)(*g),
                Err((bd, off)) => winner.get(&find(&mut parent, *bd)).and_then(|w| (notes.field_points)(w, *off)),
            };
            if let Some(st) = st {
                *more.entry(find(&mut parent, *d)).or_default().entry(st).or_default() += 2;
            }
        }
        let next = decide(&more);
        if next == winner {
            break;
        }
        winner = next;
    }
    // values still untyped: accessed at 3+ distinct offsets that are all fields of exactly one
    // known struct, and unlikely to all match it by chance (density^offsets under 2%)
    for (g, ks) in &offs {
        if winner.contains_key(g) || ks.len() < 3 {
            continue;
        }
        let mut count: BTreeMap<String, usize> = BTreeMap::new();
        for &k in ks {
            for st in (notes.structs_with)(k) {
                *count.entry(st).or_default() += 1;
            }
        }
        let mut full = count.iter().filter(|(_, n)| **n == ks.len());
        if let (Some((st, _)), None) = (full.next(), full.next()) {
            if (notes.density)(st).powi(ks.len() as i32) < 0.02 {
                winner.insert(*g, st.clone());
            }
        }
    }
    // a register read only where it holds one struct-typed value takes the struct's name
    let mut seen: BTreeMap<u8, Option<usize>> = BTreeMap::new();
    for &(r, d) in &reads_all {
        let g = find(&mut parent, d);
        let e = seen.entry(r).or_insert(Some(g));
        if *e != Some(g) {
            *e = None;
        }
    }
    let regs: BTreeMap<u8, String> = seen.iter().filter_map(|(&r, g)| Some((r, winner.get(&(*g)?)?.clone()))).collect();
    // arguments: entry values the function reads
    let mut entry_read = BTreeSet::new();
    for &(r, d) in &reads_all {
        if r < 4 && find(&mut parent, d) == find(&mut parent, r as usize) {
            entry_read.insert(r);
        }
    }
    let args: BTreeMap<u8, String> = entry_read.into_iter().filter_map(|r| {
        let g = find(&mut parent, r as usize);
        Some((r, winner.get(&g)?.clone()))
    }).collect();
    let calls: Vec<(u32, u8, String)> = call_args.into_iter().filter_map(|(t, r, d)| {
        let g = find(&mut parent, d);
        Some((t, r, winner.get(&g)?.clone()))
    }).collect();
    let mut map = FieldMap::new();
    for (key, r, off, d) in uses {
        let g = find(&mut parent, d);
        let f = match winner.get(&g) {
            Some(st) => (notes.field_in)(st, off),
            None => (notes.field)(off),
        };
        if let Some(f) = f {
            map.entry(key).or_default().insert((r, off), f);
        }
    }
    Typed { fields: map, regs, args, calls }
}

/// Local names for the struct-typed registers: the struct's module name (`kart`), numbered when
/// several registers hold the same struct (`kart`, `kart_2`); never a name the code already uses.
fn typed_names(regs: &BTreeMap<u8, String>, module: &dyn Fn(&str) -> Option<String>) -> HashMap<u8, String> {
    const TAKEN: &[&str] = &["c", "m", "pc", "t", "x", "v", "th", "sc", "res", "ea", "ax", "v0", "v1", "l", "h", "target", "old", "iw", "sp", "lr", "R"];
    let mut used: BTreeSet<String> = TAKEN.iter().map(|s| s.to_string()).collect();
    let mut out = HashMap::new();
    for (&r, st) in regs {
        let Some(base) = module(st) else { continue };
        let base = if base.starts_with('r') && base[1..].chars().all(|c| c.is_ascii_digit()) { format!("{base}_") } else { base };
        let mut n = base.clone();
        let mut k = 2;
        while !used.insert(n.clone()) {
            n = format!("{base}_{k}");
            k += 1;
        }
        out.insert(r, n);
    }
    out
}

/// [`emit_t1`] with comments from `notes`.
pub fn emit_t1_annotated(f: &Func, id: &str, img: Option<&Image>, notes: &Notes) -> Result<String, Reason> {
    emit_t1_full(f, id, img, notes, None)
}

/// Per-instruction budget cost `(instruction, ends its block with a jump)`, as for
/// [`crate::lift::emit_cost`] (the ARM7 charges cycles); `None` charges one per instruction.
pub type Cost<'a> = Option<&'a dyn Fn(&crate::arm::Decoded, bool) -> u32>;

/// [`emit_t1_annotated`] with the translation's budget cost model.
pub fn emit_t1_full(f: &Func, id: &str, img: Option<&Image>, notes: &Notes, cost: Cost) -> Result<String, Reason> {
    let names = notes.call;
    let live = crate::raise::flag_liveness(f, img);
    // blocks reachable from the entry through internal edges
    let mut blocks: BTreeMap<u32, Block> = BTreeMap::new();
    let mut switches: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
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
        }
        fold_compares(&mut insts, &live);
        join_runs(&mut insts);
        for ins in &mut insts {
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
                    S::Goto(_) => {
                        // a compiled switch: its table's cases become successors (`match`)
                        if let Some(img) = img {
                            let t = crate::lift::computed_targets(f, img, ins.addr);
                            if !t.is_empty() {
                                succ.extend(t.iter().copied());
                                switches.insert(b, t);
                            }
                        }
                        falls &= !unconditional;
                    }
                    S::ExcReturn(_) => falls &= !unconditional,
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
        let mut seen_s = BTreeSet::new();
        succ.retain(|x| seen_s.insert(*x));
        work.extend(succ.iter().copied());
        let charge = match cost {
            None => insts.len() as u32,
            Some(cost) => {
                let raw = f.block(b);
                raw.iter().enumerate().map(|(i, (_, d))| cost(d, i + 1 == raw.len() && (d.inst.writes_pc() || d.inst.is_call()))).sum()
            }
        };
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
            if let Some(g) = &ins.guard {
                regs_e(g, &mut used);
            }
        }
    }
    let all: BTreeSet<u8> = used.union(&written).copied().filter(|&i| i < 15).collect();
    // named struct fields at the accesses (printed as accessors)
    let typed = infer_fields(&blocks, &order, &preds, f.entry, notes);
    let reg_names = typed_names(&typed.regs, notes.module_of);
    let args_doc: Vec<String> = typed.args.iter().map(|(r, st)| match reg_names.get(r) {
        Some(n) => format!("r{r} = `{n}` (a `{st}`)"),
        None => format!("r{r} points to a `{st}`"),
    }).collect();
    let ctx = Ctx { blocks: &blocks, rpo, idom, merge, header, children, names, notes, switches: &switches, fields: typed.fields };
    ir::set_reg_names(reg_names);
    LAST_TYPES.with(|t| *t.borrow_mut() = (typed.args.clone(), typed.calls.clone()));
    ir::take_field_uses();
    let mut body = String::new();
    do_tree(&ctx, f.entry, 2, &mut body);
    ir::set_fields(Default::default());

    let mut s = String::new();
    if !args_doc.is_empty() {
        let _ = writeln!(s, "/// Arguments: {}.", args_doc.join(", "));
    }
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
    ir::set_reg_names(Default::default());
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
        let inner = if ins.cond == 14 && ins.guard.is_none() { depth } else { depth + 1 };
        let set = |k: usize| ir::set_fields(c.fields.get(&(x, ins.addr, k)).cloned().unwrap_or_default());
        for (si, s) in ins.stmts.iter().enumerate() {
            set(si);
            stmt_code(c, x, s, inner, &mut text);
        }
        if text.is_empty() {
            continue;
        }
        if let Some(g) = &ins.guard {
            set(usize::MAX);
            indent(out, depth);
            let _ = writeln!(out, "if {} {{", ir::expr_locals(g));
            out.push_str(&text);
            indent(out, depth);
            out.push_str("}
");
        } else if ins.cond == 14 {
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
        S::Goto(e) if c.switches.contains_key(&x) => {
            line(out, &format!("match {} {{", ir::expr_locals(e)));
            for &t in &c.switches[&x] {
                indent(out, depth + 1);
                let _ = writeln!(out, "{t:#X} => {{");
                branch(c, x, t, depth + 2, out);
                indent(out, depth + 1);
                out.push_str("}\n");
            }
            line(out, "    t => exit!(t),");
            line(out, "}");
        }
        S::Goto(e) => line(out, &format!("exit!({});", ir::expr_locals(e))),
        S::ExcReturn(e) => {
            let v = ir::expr_locals(e);
            line(out, &format!("spill!(); let v = {v}; let th = c.restore_spsr(); leave!(if th {{ v | 1 }} else {{ v & !3 }});"))
        }
        S::Call { target, ret } => {
            let name = match target {
                E::Const(t) => (c.names)(*t).map(|n| format!(" // {n}")).unwrap_or_default(),
                _ => String::new(),
            };
            line(out, &format!("spill!(); let x = m.call(c, {}, {ret:#X}); if x != {ret:#X} {{ leave!(x); }} reload!();{name}", ir::expr_locals(target)))
        }
        S::IfSome(e, body) => {
            let exit = match body.as_slice() {
                [S::Goto(E::Iw(v))] if **v == E::Tmp(ir::Tmp::V) => "leave!(iw(v));".to_string(),
                [S::ExcReturn(E::Tmp(ir::Tmp::V))] => "let th = c.restore_spsr(); leave!(if th { v | 1 } else { v & !3 });".to_string(),
                _ => unreachable!("block loads only jump"),
            };
            line(out, &format!("spill!(); if let Some(v) = {} {{ {exit} }} reload!();{}", ir::expr_locals(e), block_note(e)))
        }
        S::Do(e) if syncs(e) => line(out, &format!("spill!(); {}; reload!();{}", ir::expr_locals(e), block_note(e))),
        S::SetReg(i, e) if syncs(e) => line(out, &format!("spill!(); let t = {}; reload!(); {} = t;", ir::expr_locals(e), ir::reg_name(*i))),
        S::Store { w: Width::B32, .. } | _ => {
            let text = ir::print_locals(std::slice::from_ref(s));
            let note = stmt_note(c.notes, s).map(|n| format!(" // {n}")).unwrap_or_default();
            line(out, &format!("{text}{note}"))
        }
    }
}
