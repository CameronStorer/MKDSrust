//! Function discovery and Rust code generation for ARM and Thumb code (IR in `arm.rs`).
//!
//! A lifted function is `fn(c, m, pc) -> pc`: a `loop { match pc { block => ..., _ => return pc } }`
//! over its basic blocks, entered at any of its block starts. It returns the first pc that is not
//! one of its blocks: a return, a tail call, a computed jump it cannot resolve, or the address of
//! an instruction it could not decode. The dispatcher (`dispatch.rs`) continues from there, with
//! the interpreter as fallback, so nothing is ever guessed.
//!
//! Blocks are straight-line: they end at any instruction that may write the pc, at calls, and
//! before every branch target, so a block always runs to its end.
use crate::arm::{self, pc_read, Decoded, HalfMul, Inst, Off, Op2};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;

/// Code memory for discovery: one or more loaded segments (ARM9 main, ITCM, overlays).
pub struct Image<'a> {
    pub segs: Vec<(u32, &'a [u8])>,
}

impl<'a> Image<'a> {
    pub fn new(base: u32, bytes: &'a [u8]) -> Self {
        Image { segs: vec![(base, bytes)] }
    }
    fn bytes(&self, a: u32, n: usize) -> Option<&'a [u8]> {
        self.segs.iter().find_map(|&(b, s)| a.checked_sub(b).and_then(|o| s.get(o as usize..o as usize + n)))
    }
    pub fn read32(&self, a: u32) -> Option<u32> {
        self.bytes(a, 4).map(|b| u32::from_le_bytes(b.try_into().unwrap()))
    }
    pub fn read16(&self, a: u32) -> Option<u16> {
        self.bytes(a, 2).map(|b| u16::from_le_bytes(b.try_into().unwrap()))
    }
}

/// Decodes the instruction at code address `pc` (bit 0 = Thumb).
pub fn decode_at(img: &Image, pc: u32) -> Option<Decoded> {
    let a = pc & !1;
    if pc & 1 != 0 {
        let h = img.read16(a)?;
        Some(crate::thumb::decode(h, img.read16(a + 2).unwrap_or(0), a))
    } else {
        Some(arm::decode(img.read32(a)?, a))
    }
}

pub struct Func {
    /// Entry (bit 0 = Thumb).
    pub entry: u32,
    /// Decoded instructions by code address (bit 0 = Thumb).
    pub insts: BTreeMap<u32, Decoded>,
    /// Block starts.
    pub blocks: BTreeSet<u32>,
    /// Addresses where discovery stopped at an undecodable instruction.
    pub undecoded: Vec<(u32, u32)>,
}

impl Func {
    /// Next code address after the instruction at `pc`.
    pub fn next(pc: u32, d: &Decoded) -> u32 {
        pc.wrapping_add(d.size)
    }

    fn ends_block(d: &Decoded) -> bool {
        d.inst.writes_pc() || d.inst.is_call() || matches!(d.inst, Inst::Swi { .. })
    }
}

/// ARM `add pc, pc, rm, lsl #n` (any condition): the jump-table dispatch of a compiled `switch`.
fn is_switch_add(d: &Decoded) -> bool {
    !d.thumb && matches!(d.inst, Inst::Dp { op: 4, rn: 15, rd: 15, op2: Op2::Reg { kind: 0, .. }, .. })
}

/// The return point a `mov lr, pc` / `add lr, pc, #k` sets up (bit 0 = Thumb): an indirect call
/// through a following `bx rX` / `ldr pc, ...` comes back there, so it must be a block start.
fn link_return(d: &Decoded, pc: u32) -> Option<u32> {
    let Inst::Dp { op, rn, rd: 14, op2, .. } = d.inst else { return None };
    let base = pc_read(pc, d.thumb);
    let v = match (op, op2) {
        (13, Op2::Reg { rm: 15, kind: 0, amt: 0 }) => base,
        (13, Op2::Const(v)) if d.thumb && v == base => v,
        (4, Op2::Imm { imm8, rot }) if rn == 15 => base.wrapping_add(imm8.rotate_right(rot * 2)),
        (4, Op2::Const(k)) if rn == 15 => base.wrapping_add(k),
        _ => return None,
    };
    Some((v & !1) | d.thumb as u32)
}

/// The pc value an ARM `stm {.., pc}` / `str pc, [..]` stores (address + 12, as the interpreter and
/// `store_val` model it): a context save (OS thread switch, setjmp) later resumes there through an
/// `ldm ..^` / `ldr pc`, so it must be a block start.
fn saved_pc(d: &Decoded, pc: u32) -> Option<u32> {
    let stores_pc = match d.inst {
        Inst::Block { load: false, list, .. } | Inst::BlockUser { load: false, list, .. } => list & 0x8000 != 0,
        Inst::Mem { load: false, rd: 15, .. } => true,
        _ => false,
    };
    (stores_pc && !d.thumb).then(|| pc_read(pc, false).wrapping_add(4))
}

/// Pc-relative literal words a function reads (their addresses): the literal pool, which is data.
pub fn literal_addresses(f: &Func) -> Vec<u32> {
    f.insts
        .iter()
        .filter_map(|(&pc, d)| match d.inst {
            Inst::Mem { load: true, pre: true, rn: 15, off: Off::Imm(v), up, .. } | Inst::MemH { load: true, pre: true, rn: 15, off: Off::Imm(v), up, .. } => {
                let base = pc_read(pc, d.thumb);
                Some((if up { base.wrapping_add(v) } else { base.wrapping_sub(v) }) & !3)
            }
            _ => None,
        })
        .collect()
}

/// Case targets of the compiler's Thumb switch, ending at `bx rX` (`pc` = its address):
/// `add rX, rX, rX; add rX, pc; ldrh rX, [rX, #k]; lsl rX, #16; asr rX, #16; add rX, pc; bx rX`.
/// The table holds signed halfword offsets from the second `add rX, pc` base (bit 0 set: Thumb);
/// it has `n + 1` entries for a guarding `cmp rX, #n`, and in any case ends where the first case's
/// code begins (case code follows the table).
pub fn thumb_switch_targets(img: &Image, insts: &BTreeMap<u32, Decoded>, pc: u32) -> Vec<u32> {
    let Some(Inst::Bx { rm, link: false }) = insts.get(&pc).map(|d| d.inst) else { return Vec::new() };
    if pc & 1 == 0 {
        return Vec::new();
    }
    // The ldrh .. bx core must be this function's code; the table base (`add rX, pc`) and the guard
    // may sit before its entry when metadata splits the switch in two, so they are read from the
    // image when the function does not contain them.
    let at = |back: u32| {
        let q = pc.wrapping_sub(2 * back);
        insts.get(&q).map(|d| d.inst).or_else(|| if back >= 5 { decode_at(img, q).map(|d| d.inst) } else { None })
    };
    let (Some(Inst::Dp { op: 4, rd: a, rn: b, op2: Op2::Const(base2), .. }), Some(Inst::Dp { op: 13, rd: c, op2: Op2::Reg { kind: 2, amt: 16, .. }, .. })) = (at(1), at(2)) else {
        return Vec::new();
    };
    if a != rm || b != rm || c != rm {
        return Vec::new();
    }
    // ldrh rX, [rX, #k] and add rX, pc (base1) before the sign extension
    let (Some(Inst::MemH { load: true, kind: 1, rn, rd, off: Off::Imm(k), .. }), Some(Inst::Dp { op: 4, rd: e, rn: f, op2: Op2::Const(base1), .. })) = (at(4), at(5)) else {
        return Vec::new();
    };
    if rn != rm || rd != rm || e != rm || f != rm {
        return Vec::new();
    }
    let table = base1.wrapping_add(k);
    // the guarding `cmp rX, #n` (then `bhi default` / `bls` + `b default`) bounds the table to n + 1
    // the index is doubled first: `add rX, rI, rI` (rI may be rX); the guard compares rI
    let idx = match at(6) {
        Some(Inst::Dp { op: 4, rd, rn, op2: Op2::Reg { rm: r2, kind: 0, amt: 0 }, .. }) if rd == rm && rn == r2 => rn,
        _ => rm,
    };
    let limit = (7..=10)
        .find_map(|back| match at(back) {
            Some(Inst::Dp { op: 10, rn, op2: Op2::Const(n), .. }) if rn == idx => Some(n as usize + 1),
            _ => None,
        })
        .unwrap_or(1024);
    let mut out = Vec::new();
    let mut end = u32::MAX;
    let mut q = table;
    while q < end && out.len() < limit {
        let Some(h) = img.read16(q) else { break };
        let t = base2.wrapping_add(h as i16 as i32 as u32);
        if t & 1 == 0 || img.read16(t & !1).is_none() {
            break;
        }
        // cases may also lie before the table (a loop back); only code after it bounds the table
        if t & !1 > table {
            end = end.min(t & !1);
        }
        out.push(t);
        q += 2;
    }
    out
}

/// Targets of an `add pc, pc, rX, lsl #n` with no branch table after it: a jump into an unrolled
/// straight-line run starting at `start` (SDK divide: `add r2, r2, r2, lsl #1; add pc, pc, r2, lsl
/// #2` into 3-instruction groups). Any instruction of the run up to its first unconditional pc write
/// may be the target. The run can extend past the function's metadata bounds (into the next listed
/// function), so this ignores bounds; callers filter.
pub fn jump_run(img: &Image, start: u32) -> Vec<u32> {
    let mut out = Vec::new();
    let mut q = start;
    for _ in 0..4096 {
        match decode_at(img, q) {
            Some(e) if !matches!(e.inst, Inst::Undecoded(_)) => {
                out.push(q);
                if e.cond == 14 && e.inst.writes_pc() {
                    break;
                }
                q = q.wrapping_add(4);
            }
            _ => break,
        }
    }
    out
}

/// Follows control flow from `entry` (bit 0 = Thumb). Branches leaving `[lo, hi)` (when given)
/// are treated as tail calls. Fails only when the entry itself cannot be read.
pub fn discover(img: &Image, entry: u32, bounds: Option<(u32, u32)>, max: usize) -> Result<Func, String> {
    let inside = |a: u32| bounds.is_none_or(|(lo, hi)| (a & !1) >= lo && (a & !1) < hi);
    discover_with(img, entry, &inside, max)
}

/// [`discover`] with an arbitrary predicate: a branch to an address for which `inside` is false
/// is a tail call (it ends the function there).
pub fn discover_with(img: &Image, entry: u32, inside: &dyn Fn(u32) -> bool, max: usize) -> Result<Func, String> {
    discover_seeded(img, entry, &[], inside, max)
}

/// [`discover_with`] plus `extra` block starts (those inside): addresses other code enters this
/// function at (a branch or computed jump from a neighbouring function, a code pointer).
pub fn discover_seeded(img: &Image, entry: u32, extra: &[u32], inside: &dyn Fn(u32) -> bool, max: usize) -> Result<Func, String> {
    let mut insts = BTreeMap::new();
    let mut leaders = BTreeSet::from([entry]);
    let mut undecoded = Vec::new();
    let mut work = vec![entry];
    for &x in extra.iter().filter(|&&x| inside(x)) {
        leaders.insert(x);
        work.push(x);
    }
    while let Some(pc) = work.pop() {
        if insts.contains_key(&pc) || undecoded.iter().any(|&(a, _)| a == pc) {
            continue;
        }
        let Some(d) = decode_at(img, pc) else {
            if pc == entry {
                return Err(format!("entry {entry:#X} outside the image"));
            }
            continue;
        };
        if let Inst::Undecoded(w) = d.inst {
            undecoded.push((pc, w));
            continue;
        }
        insts.insert(pc, d);
        if is_switch_add(&d) {
            // compiler switch: `add pc, pc, rX, lsl #2` jumps into a table of unconditional branches
            // placed right after the following default branch (entry k at pc + 8 + 4k). Each entry is
            // a computed-jump target, so it must be a block start with its case body discovered.
            let mut q = pc.wrapping_add(8);
            while inside(q) {
                match decode_at(img, q) {
                    Some(e) if e.cond == 14 && matches!(e.inst, Inst::B { .. }) => {
                        leaders.insert(q);
                        work.push(q);
                        q = q.wrapping_add(4);
                    }
                    _ => break,
                }
            }
            if q == pc.wrapping_add(8) {
                for t in jump_run(img, q).into_iter().take_while(|&t| inside(t)) {
                    leaders.insert(t);
                    work.push(t);
                }
            }
        }
        if d.thumb && matches!(d.inst, Inst::Bx { link: false, .. }) {
            for t in thumb_switch_targets(img, &insts, pc) {
                if inside(t) {
                    leaders.insert(t);
                    work.push(t);
                }
            }
        }
        if insts.len() > max {
            return Err(format!("more than {max} instructions"));
        }
        let next = Func::next(pc, &d);
        if let Inst::B { target } = d.inst {
            if inside(target) {
                leaders.insert(target);
                work.push(target);
            }
        }
        if let Some(ret) = link_return(&d, pc).or_else(|| saved_pc(&d, pc)) {
            if inside(ret) {
                leaders.insert(ret);
                work.push(ret);
            }
        }
        let always = d.cond == 14;
        if Func::ends_block(&d) && !(always && d.inst.writes_pc()) {
            leaders.insert(next);
        }
        if !(always && d.inst.writes_pc()) && inside(next) {
            work.push(next);
        }
    }
    leaders.retain(|a| insts.contains_key(a));
    Ok(Func { entry, insts, blocks: leaders, undecoded })
}

/// The known targets of the computed jump at `pc` in `f` (a compiled `switch`): the ARM branch
/// table after `add pc, pc, rX, lsl #n`, or the Thumb halfword table of `ldrh` .. `bx`. Only
/// block starts of `f` are returned; an empty list means the targets are not known.
pub fn computed_targets(f: &Func, img: &Image, pc: u32) -> Vec<u32> {
    let Some(d) = f.insts.get(&pc) else { return Vec::new() };
    let mut out = Vec::new();
    if is_switch_add(d) {
        let mut q = pc.wrapping_add(8);
        while let Some(e) = f.insts.get(&q) {
            if e.cond == 14 && matches!(e.inst, Inst::B { .. }) && f.blocks.contains(&q) {
                out.push(q);
                q = q.wrapping_add(4);
            } else {
                break;
            }
        }
        if out.is_empty() {
            out = jump_run(img, pc.wrapping_add(8)).into_iter().take_while(|t| f.insts.contains_key(t)).collect();
        }
    } else if d.thumb && matches!(d.inst, Inst::Bx { link: false, .. }) {
        out = thumb_switch_targets(img, &f.insts, pc);
    }
    let mut seen = BTreeSet::new();
    out.retain(|t| f.blocks.contains(t) && seen.insert(*t));
    out
}

/// Code addresses a function refers to (bit 0 = Thumb): direct call targets, branches that leave
/// it (tail calls), and code pointers it materialises (literal-pool words and `adr`). Pointers are
/// only candidates: [`discover_program`] keeps one when `is_code` accepts it and it lifts cleanly.
pub fn references(f: &Func, img: &Image) -> Vec<(u32, bool)> {
    let mut out = Vec::new();
    for (&pc, d) in &f.insts {
        match d.inst {
            Inst::Call { target } => out.push((target, false)),
            Inst::B { target } if !f.blocks.contains(&target) => out.push((target, false)),
            Inst::Bx { link: false, .. } if d.thumb => {
                out.extend(thumb_switch_targets(img, &f.insts, pc).into_iter().filter(|t| !f.blocks.contains(t)).map(|t| (t, false)));
            }
            _ if is_switch_add(d) => {
                let q = pc.wrapping_add(8);
                let table = decode_at(img, q).is_some_and(|e| e.cond == 14 && matches!(e.inst, Inst::B { .. }));
                if !table {
                    out.extend(jump_run(img, q).into_iter().filter(|t| !f.blocks.contains(t)).map(|t| (t, false)));
                }
            }
            Inst::Mem { load: true, byte: false, pre: true, rn: 15, off: Off::Imm(v), up, .. } => {
                let base = pc_read(pc, d.thumb);
                let at = if up { base.wrapping_add(v) } else { base.wrapping_sub(v) };
                if let Some(w) = img.read32(at & !3) {
                    out.push((w, true));
                }
            }
            Inst::Dp { op: 4, rn: 15, op2, .. } => {
                let k = match op2 {
                    Op2::Imm { imm8, rot } => Some(imm8.rotate_right(rot * 2)),
                    Op2::Const(k) => Some(k),
                    _ => None,
                };
                if let Some(k) = k {
                    let v = pc_read(pc, d.thumb).wrapping_add(k) | d.thumb as u32;
                    out.push((v, true));
                    // NitroSDK OS_SaveContext stores `adr` + 8 as the saved pc and OS_LoadContext
                    // resumes with `subs pc, lr, #4`: the resume point is the value - 4
                    out.push((v.wrapping_sub(4), true));
                }
            }
            _ => {}
        }
    }
    out
}

/// Where execution can fall out of a function past its last instruction (no unconditional pc
/// write; also a call's return point). Often real code (metadata ended the function too early), but
/// after a call that never returns it is the literal pool, so callers must treat these as guesses.
pub fn fall_exits(f: &Func) -> Vec<u32> {
    f.insts
        .iter()
        .map(|(&pc, d)| (Func::next(pc, d), d))
        .filter(|(next, d)| !(d.cond == 14 && d.inst.writes_pc()) && !f.insts.contains_key(next))
        .map(|(next, _)| next)
        .collect()
}

/// Whole-program discovery for code without function metadata (the ARM7, unlisted ARM9 code).
/// Starting from `seeds`, every call target, tail call and accepted code pointer becomes a
/// function entry, to a fixpoint. A function ends at a branch to another known entry or out of
/// `is_code`. A pointer candidate is kept only if it is aligned for its mode, `is_code` holds and
/// its code lifts with no undecodable site (data words that merely look like code addresses are
/// dropped). Known entries found later are not split out of earlier functions: a block lifted
/// twice is the same code at the same address, so either copy is correct.
pub fn discover_program(
    img: &Image,
    seeds: &[u32],
    is_code: &dyn Fn(u32) -> bool,
    max: usize,
) -> BTreeMap<u32, Func> {
    discover_program_with(img, seeds, &[], &[], is_code, max)
}

/// [`discover_program`] with pointer candidates by strength. `strong`: evidence from trusted code
/// (a literal of listed code, the start of a gap after a function); kept like seeds if the entry
/// decodes. `weak`: guesses, e.g. every word of a data table (handler tables live in data, not in
/// literal pools); kept only if they lift with no undecodable site. Code reached from a weak
/// candidate stays weak, and literal pointers found in any code are weak.
pub fn discover_program_with(
    img: &Image,
    seeds: &[u32],
    strong: &[u32],
    weak: &[u32],
    is_code: &dyn Fn(u32) -> bool,
    max: usize,
) -> BTreeMap<u32, Func> {
    let mut entries: BTreeSet<u32> = BTreeSet::new();
    let mut funcs: BTreeMap<u32, Func> = BTreeMap::new();
    let mut rejected: BTreeSet<u32> = BTreeSet::new();
    // weakest first (popped last), so real entries claim their code before guesses do
    let mut work: Vec<(u32, bool)> = weak.iter().rev().map(|&s| (s, true)).collect();
    work.extend(strong.iter().rev().map(|&s| (s, false)));
    work.extend(seeds.iter().rev().map(|&s| (s, false)));
    while let Some((e, weak)) = work.pop() {
        let aligned = if e & 1 != 0 { true } else { e & 3 == 0 };
        if entries.contains(&e) || rejected.contains(&e) || !aligned || !is_code(e) {
            continue;
        }
        let stop = |a: u32| a == e || (is_code(a) && !entries.contains(&a));
        match discover_with(img, e, &stop, max) {
            Ok(f) if !f.insts.is_empty() && !(weak && !f.undecoded.is_empty()) => {
                entries.insert(e);
                work.extend(references(&f, img).into_iter().map(|(t, ptr)| (t, ptr || weak)));
                work.extend(fall_exits(&f).into_iter().map(|t| (t, true)));
                funcs.insert(e, f);
            }
            _ => {
                rejected.insert(e);
            }
        }
    }
    funcs
}

impl Func {
    /// The instructions of the block starting at `start`.
    pub fn block(&self, start: u32) -> Vec<(u32, Decoded)> {
        let mut out = Vec::new();
        let mut pc = start;
        while let Some(&d) = self.insts.get(&pc) {
            out.push((pc, d));
            let next = Self::next(pc, &d);
            if Self::ends_block(&d) || self.blocks.contains(&next) {
                break;
            }
            pc = next;
        }
        out
    }
}

/// Items every file of lifted code needs once (before the functions).
pub const PRELUDE: &str = "";

/// A register read by the instruction at `addr`.
fn r(i: usize, addr: u32, thumb: bool) -> String {
    if i == 15 { format!("{:#X}u32", pc_read(addr, thumb)) } else { format!("c.r[{i}]") }
}

/// A shifter operand: (statements binding it, value expression, carry-out expression; `None`
/// leaves C as it is). Everything known at build time is resolved here.
fn operand(o: Op2, addr: u32, t: bool) -> (String, String, Option<String>) {
    match o {
        Op2::Imm { imm8, rot } => {
            let v = imm8.rotate_right(rot * 2);
            (String::new(), format!("{v:#X}u32"), if rot == 0 { None } else { Some(format!("{}", v >> 31 != 0)) })
        }
        Op2::Const(v) => (String::new(), format!("{v:#X}u32"), None),
        Op2::Reg { rm, kind, amt } => {
            let x = r(rm, addr, t);
            match (kind, amt) {
                (0, 0) => (String::new(), x, None),
                (0, n) => (format!("let x = {x}; "), format!("(x << {n})"), Some(format!("(x >> {}) & 1 != 0", 32 - n))),
                (1, 0) => (format!("let x = {x}; "), "0u32".into(), Some("x >> 31 != 0".into())),
                (1, n) => (format!("let x = {x}; "), format!("(x >> {n})"), Some(format!("(x >> {}) & 1 != 0", n - 1))),
                (2, 0) => (format!("let x = {x}; "), "(((x as i32) >> 31) as u32)".into(), Some("x >> 31 != 0".into())),
                (2, n) => (format!("let x = {x}; "), format!("(((x as i32) >> {n}) as u32)"), Some(format!("(x >> {}) & 1 != 0", n - 1))),
                (_, 0) => (format!("let x = {x}; "), "((x >> 1) | ((c.c as u32) << 31))".into(), Some("x & 1 != 0".into())),
                (_, n) => (format!("let x = {x}; "), format!("x.rotate_right({n})"), Some(format!("(x >> {}) & 1 != 0", n - 1))),
            }
        }
        // register-shifted register: r15 reads one word further
        Op2::RegShift { rm, kind, rs } => {
            let v = if rm == 15 { format!("{:#X}u32", pc_read(addr, t) + 4) } else { format!("c.r[{rm}]") };
            (format!("let (x, sc) = R::shift_reg({kind}, {v}, {}, c.c); ", r(rs, addr, t)), "x".into(), Some("sc".into()))
        }
    }
}

/// The condition `cc` as a Rust expression over the flags.
fn cond_expr(cc: u8) -> &'static str {
    ["c.z", "!c.z", "c.c", "!c.c", "c.n", "!c.n", "c.v", "!c.v", "(c.c && !c.z)", "(!c.c || c.z)", "(c.n == c.v)", "(c.n != c.v)", "(!c.z && c.n == c.v)", "(c.z || c.n != c.v)", "true", "true"][cc as usize & 15]
}

/// The offset of a single transfer: `None` for zero.
fn off(o: Off, addr: u32, t: bool) -> Option<String> {
    match o {
        Off::Imm(0) => None,
        Off::Imm(v) => Some(format!("{v:#X}u32")),
        Off::Reg { rm, kind: 0, amt } => Some(if amt == 0 { r(rm, addr, t) } else { format!("({} << {amt})", r(rm, addr, t)) }),
        Off::Reg { rm, kind, amt } => Some(format!("R::shift_imm({kind}, {}, {amt}, c.c).0", r(rm, addr, t))),
    }
}

/// The value a store of `rd` writes (r15: address + 12 in ARM state).
fn store_val(rd: usize, addr: u32, thumb: bool) -> String {
    if rd == 15 { format!("{:#X}u32", pc_read(addr, thumb) + 4) } else { format!("c.r[{rd}]") }
}

/// Statements computing `ea` (and `ax`, the written-back address, when `wb`). Constant when the
/// base is the pc and the offset an immediate.
fn ea(rn: usize, o: Off, up: bool, pre: bool, wb: bool, addr: u32, t: bool) -> (String, Option<u32>) {
    if rn == 15 {
        if let Off::Imm(v) = o {
            let base = pc_read(addr, t);
            let a = if !pre { base } else if up { base.wrapping_add(v) } else { base.wrapping_sub(v) };
            return (format!("let ea = {a:#X}u32; "), Some(a));
        }
    }
    let base = r(rn, addr, t);
    let Some(o) = off(o, addr, t) else {
        return (format!("let ea = {base}; let ax = ea; "), None);
    };
    let op = if up { "wrapping_add" } else { "wrapping_sub" };
    if pre && !wb {
        (format!("let ea = {base}.{op}({o}); "), None)
    } else if pre {
        (format!("let ax = {base}.{op}({o}); let ea = ax; "), None)
    } else {
        (format!("let ea = {base}; let ax = ea.{op}({o}); "), None)
    }
}

/// The pre-IR string emitter for one instruction, kept as the oracle for the T0 contract
/// (`tests/ir_t0.rs`): [`crate::ir::print`] of [`crate::ir::lower`] must equal it up to whitespace.
#[doc(hidden)]
pub fn legacy_body(d: &Decoded, addr: u32, next: u32, img: Option<&Image>) -> String {
    let t = d.thumb;
    match d.inst {
        Inst::Dp { op, s, rn, rd, op2: o } => {
            let (pre, b, carry) = operand(o, addr, t);
            let a = r(rn, addr, t);
            let mut x = pre;
            let logical = matches!(op, 0 | 1 | 8 | 9 | 12 | 13 | 14 | 15);
            let value = match op {
                0 | 8 => format!("{a} & {b}"),
                1 | 9 => format!("{a} ^ {b}"),
                12 => format!("{a} | {b}"),
                13 => b.clone(),
                14 => format!("{a} & !{b}"),
                15 => format!("!{b}"),
                _ if s => match op {
                    2 | 10 => format!("R::subs(c, {a}, {b})"),
                    3 => format!("R::subs(c, {b}, {a})"),
                    4 | 11 => format!("R::adds(c, {a}, {b})"),
                    5 => format!("R::adcs(c, {a}, {b})"),
                    6 => format!("R::sbcs(c, {a}, {b})"),
                    _ => format!("R::sbcs(c, {b}, {a})"),
                },
                2 => format!("{a}.wrapping_sub({b})"),
                3 => format!("{b}.wrapping_sub({a})"),
                4 => format!("{a}.wrapping_add({b})"),
                5 => format!("{a}.wrapping_add({b}).wrapping_add(c.c as u32)"),
                6 => format!("{a}.wrapping_sub({b}).wrapping_sub((!c.c) as u32)"),
                _ => format!("{b}.wrapping_sub({a}).wrapping_sub((!c.c) as u32)"),
            };
            let _ = write!(x, "let res = {value}; ");
            if s && logical {
                x.push_str("R::set_nz(c, res); ");
                if let Some(cy) = carry {
                    let _ = write!(x, "c.c = {cy}; ");
                }
            }
            if !Inst::is_compare(op) {
                if rd == 15 && s {
                    // exception return: CPSR = SPSR, state from its T bit
                    x.push_str("let th = c.restore_spsr(); pc = if th { res | 1 } else { res & !3 }; continue;");
                } else if rd == 15 {
                    x.push_str(if t { "pc = res | 1; continue;" } else { "pc = res & !3; continue;" });
                } else {
                    let _ = write!(x, "c.r[{rd}] = res;");
                }
            }
            x
        }
        Inst::Mul { acc, s, rd, rn, rs, rm } => {
            let mut x = format!("let res = {}.wrapping_mul({})", r(rm, addr, t), r(rs, addr, t));
            if acc {
                let _ = write!(x, ".wrapping_add({})", r(rn, addr, t));
            }
            x.push_str("; ");
            if s {
                x.push_str("R::set_nz(c, res); ");
            }
            let _ = write!(x, "c.r[{rd}] = res;");
            x
        }
        Inst::MulLong { signed, acc, s, lo, hi, rs, rm } => format!(
            "let (l, h) = R::mul_long(c, {signed}, {acc}, {s}, {}, {}, c.r[{lo}], c.r[{hi}]); c.r[{lo}] = l; c.r[{hi}] = h;",
            r(rm, addr, t),
            r(rs, addr, t)
        ),
        Inst::MulHalf { op: HalfMul::Smlal, x, y, rd, rn, rs, rm } => format!(
            "let (l, h) = R::smlal_xy({x}, {y}, {}, {}, c.r[{rn}], c.r[{rd}]); c.r[{rn}] = l; c.r[{rd}] = h;",
            r(rm, addr, t),
            r(rs, addr, t)
        ),
        Inst::MulHalf { op, x, y, rd, rn, rs, rm } => format!(
            "c.r[{rd}] = R::half_mul(c, recomp::arm::HalfMul::{op:?}, {x}, {y}, {}, {}, {});",
            r(rm, addr, t),
            r(rs, addr, t),
            r(rn, addr, t)
        ),
        Inst::Qalu { op, rd, rn, rm } => format!("c.r[{rd}] = R::qalu(c, {op}, {}, {});", r(rm, addr, t), r(rn, addr, t)),
        Inst::Clz { rd, rm } => format!("c.r[{rd}] = {}.leading_zeros();", r(rm, addr, t)),
        Inst::Mrs { rd, spsr } => format!("c.r[{rd}] = {};", if spsr { "c.spsr" } else { "c.cpsr()" }),
        Inst::Msr { spsr, mask, src } => {
            let (pre, v, _) = operand(src, addr, t);
            format!("{pre}c.msr({spsr}, {mask}, {v});")
        }
        Inst::B { target } => format!("pc = {target:#X}u32; continue;"),
        Inst::Call { target } => {
            let ret = next | t as u32;
            format!("c.r[14] = {ret:#X}u32; let x = m.call(c, {target:#X}u32, {ret:#X}u32); if x != {ret:#X}u32 {{ pc = x; continue; }}")
        }
        Inst::Bx { rm, link } => {
            if link {
                let ret = next | t as u32;
                format!("let target = iw({}); c.r[14] = {ret:#X}u32; let x = m.call(c, target, {ret:#X}u32); if x != {ret:#X}u32 {{ pc = x; continue; }}", r(rm, addr, t))
            } else {
                format!("pc = iw({}); continue;", r(rm, addr, t))
            }
        }
        Inst::Mem { load, byte, pre, up, wb, rn, rd, off: o } => {
            let (mut x, at) = ea(rn, o, up, pre, wb, addr, t);
            if load {
                // literal pool: a word in the function's own (read-only) image
                if let (Some(a), Some(img), false, false) = (at, img, byte, wb) {
                    if a % 4 == 0 {
                        if let Some(v) = img.read32(a) {
                            return if rd == 15 { format!("pc = iw({v:#X}u32); continue;") } else { format!("c.r[{rd}] = {v:#X}u32;") };
                        }
                    }
                }
                let get = if byte { "m.r8(ea) as u32" } else { "R::ldr(m, ea)" };
                let _ = write!(x, "let v = {get}; ");
                if wb {
                    let _ = write!(x, "c.r[{rn}] = ax; ");
                }
                if rd == 15 {
                    x.push_str("pc = iw(v); continue;");
                } else {
                    let _ = write!(x, "c.r[{rd}] = v;");
                }
            } else {
                let v = store_val(rd, addr, t);
                if byte {
                    let _ = write!(x, "m.w8(ea, {v} as u8); ");
                } else {
                    let _ = write!(x, "m.w32(ea & !3, {v}); ");
                }
                if wb {
                    let _ = write!(x, "c.r[{rn}] = ax;");
                }
            }
            x
        }
        Inst::MemH { load, kind, pre, up, wb, rn, rd, off: o } => {
            let (mut x, _) = ea(rn, o, up, pre, wb, addr, t);
            if load {
                let get = match kind {
                    1 => "m.r16(ea & !1) as u32",
                    2 => "m.r8(ea) as i8 as i32 as u32",
                    _ => "m.r16(ea & !1) as i16 as i32 as u32",
                };
                let _ = write!(x, "let v = {get}; ");
                if wb {
                    let _ = write!(x, "c.r[{rn}] = ax; ");
                }
                let _ = write!(x, "c.r[{rd}] = v;");
            } else {
                let _ = write!(x, "m.w16(ea & !1, {} as u16); ", store_val(rd, addr, t));
                if wb {
                    let _ = write!(x, "c.r[{rn}] = ax;");
                }
            }
            x
        }
        Inst::MemD { load, pre, up, wb, rn, rd, off: o } => {
            let (mut x, _) = ea(rn, o, up, pre, wb, addr, t);
            if load {
                x.push_str("let v0 = m.r32(ea & !3); let v1 = m.r32(ea.wrapping_add(4) & !3); ");
                if wb {
                    let _ = write!(x, "c.r[{rn}] = ax; ");
                }
                let _ = write!(x, "c.r[{rd}] = v0; c.r[{}] = v1;", rd + 1);
            } else {
                let _ = write!(x, "m.w32(ea & !3, c.r[{rd}]); m.w32(ea.wrapping_add(4) & !3, c.r[{}]); ", rd + 1);
                if wb {
                    let _ = write!(x, "c.r[{rn}] = ax;");
                }
            }
            x
        }
        Inst::Block { load, pre, up, wb, rn, list } => {
            let call = format!("R::block(c, m, {load}, {pre}, {up}, {wb}, {rn}, {list:#X}, {:#X}u32)", pc_read(addr, t) + 4);
            if load && list & 0x8000 != 0 {
                format!("if let Some(v) = {call} {{ pc = iw(v); continue; }}")
            } else {
                format!("{call};")
            }
        }
        Inst::BlockUser { load, pre, up, wb, rn, list } => {
            let call = format!("R::block_user(c, m, {load}, {pre}, {up}, {wb}, {rn}, {list:#X}, {:#X}u32)", pc_read(addr, t) + 4);
            if load && list & 0x8000 != 0 {
                format!("if let Some(v) = {call} {{ let th = c.restore_spsr(); pc = if th {{ v | 1 }} else {{ v & !3 }}; continue; }}")
            } else {
                format!("{call};")
            }
        }
        Inst::Swp { byte, rn, rd, rm } => format!("let old = R::swp(m, c.r[{rn}], c.r[{rm}], {byte}); c.r[{rd}] = old;"),
        Inst::Swi { imm } => format!("m.swi(c, {});", crate::rt::swi_number(imm, t)),
        Inst::Cp { load, cp, op1, crn, crm, op2: o2, rd } => {
            if load {
                if rd == 15 {
                    format!("let v = m.mrc(c, {cp}, {op1}, {crn}, {crm}, {o2}); c.n = v >> 31 != 0; c.z = (v >> 30) & 1 != 0; c.c = (v >> 29) & 1 != 0; c.v = (v >> 28) & 1 != 0;")
                } else {
                    format!("c.r[{rd}] = m.mrc(c, {cp}, {op1}, {crn}, {crm}, {o2});")
                }
            } else {
                format!("m.mcr(c, {cp}, {op1}, {crn}, {crm}, {o2}, {});", r(rd, addr, t))
            }
        }
        Inst::Nop => String::new(),
        Inst::Undecoded(_) => unreachable!("discovery never keeps undecoded instructions"),
    }
}

/// Rust source for one function, plus its block and instruction address lists. `id` names the
/// items (`f_<id>`, `BLOCKS_<id>`, `INSTS_<id>`); it must be unique across overlays that share
/// addresses, e.g. `<segment>_<entry>`. The file needs [`PRELUDE`] once.
pub fn emit(f: &Func, id: &str) -> String {
    emit_with(f, id, None)
}

/// [`emit`], folding literal-pool loads from `img` (the function's own segment image).
pub fn emit_with(f: &Func, id: &str, img: Option<&Image>) -> String {
    emit_cost(f, id, img, None)
}

/// [`emit_with`] with a per-block budget charge: `cost(inst, ends_block_with_jump)` summed over the
/// block (the ARM7 table charges cycles, see `rt::arm7_cycles`); `None` charges one per instruction.
pub fn emit_cost(f: &Func, id: &str, img: Option<&Image>, cost: Option<&dyn Fn(&Decoded, bool) -> u32>) -> String {
    emit_full(f, id, img, cost, false)
}

/// [`emit_cost`]; `raise` applies the block 4B passes that keep the function's exact behaviour
/// (today: dead flag computations are dropped, see [`crate::raise`]).
pub fn emit_full(f: &Func, id: &str, img: Option<&Image>, cost: Option<&dyn Fn(&Decoded, bool) -> u32>, raise: bool) -> String {
    let live = if raise { Some(crate::raise::flag_liveness(f, img)) } else { None };
    let mut s = format!(
        "#[allow(non_snake_case, unused_variables, unused_assignments, unused_mut, unreachable_code, unused_parens, unused_braces, clippy::all)]\npub fn f_{id}(c: &mut recomp::rt::Cpu, m: &mut dyn recomp::rt::Bus, mut pc: u32) -> u32 {{\n    #[allow(unused_imports)] use recomp::rt as R;\n    #[allow(unused_imports)] use recomp::arm::interwork as iw;\n    #[allow(unused_imports)] use recomp::rt::Mem as _;\n    loop {{\n        match pc {{\n"
    );
    for &start in &f.blocks {
        let insts = f.block(start);
        let charge: u32 = match cost {
            None => insts.len() as u32,
            Some(cost) => insts.iter().enumerate().map(|(i, (_, d))| cost(d, i + 1 == insts.len() && (d.inst.writes_pc() || d.inst.is_call()))).sum(),
        };
        let _ = writeln!(s, "            {start:#X} => {{ if !R::spend(c, {charge}) && !m.slice_end(c, pc, {charge}) {{ c.r[15] = pc; return R::BUDGET_EXHAUSTED; }}");
        let mut end = start;
        for (a, d) in &insts {
            let next = Func::next(*a, d);
            let mut stmts = crate::ir::lower(d, *a & !1, next & !1, img);
            if let Some(live) = &live {
                crate::raise::drop_dead_flags(&mut stmts, live[a]);
            }
            let code = crate::ir::print(&stmts);
            if code.is_empty() {
            } else if d.cond == 14 {
                let _ = writeln!(s, "                {{ {code} }}");
            } else {
                let _ = writeln!(s, "                if {} {{ {code} }}", cond_expr(d.cond));
            }
            end = next;
        }
        let _ = writeln!(s, "                pc = {end:#X}u32; }}");
    }
    s.push_str("            _ => return pc,\n        }\n    }\n}\n");
    let blocks: Vec<String> = f.blocks.iter().map(|a| format!("{a:#X}")).collect();
    let insts: Vec<String> = f.insts.keys().map(|a| format!("{a:#X}")).collect();
    let _ = writeln!(s, "#[allow(non_upper_case_globals)]\npub static BLOCKS_{id}: &[u32] = &[{}];", blocks.join(", "));
    let _ = writeln!(s, "#[allow(non_upper_case_globals)]\npub static INSTS_{id}: &[u32] = &[{}];", insts.join(", "));
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thumb_leaf_with_call_and_loop() {
        // push {r4, lr}; movs r4, #3; loop: subs r4, #1; bne loop; bl +0; pop {r4, pc}
        let code: [u16; 7] = [0xB510, 0x2403, 0x3C01, 0xD1FD, 0xF000, 0xF800, 0xBD10];
        let bytes: Vec<u8> = code.iter().flat_map(|h| h.to_le_bytes()).collect();
        let img = Image::new(0x0200_0000, &bytes);
        let f = discover(&img, 0x0200_0001, Some((0x0200_0000, 0x0200_000E)), 100).unwrap();
        assert_eq!(f.insts.len(), 6);
        assert_eq!(f.blocks.iter().copied().collect::<Vec<_>>(), vec![0x0200_0001, 0x0200_0005, 0x0200_0009, 0x0200_000D]);
        let src = emit(&f, "t");
        assert!(src.contains("m.call(c, 0x200000Du32, 0x200000Du32)"), "{src}");
        assert!(src.contains("R::subs(c, c.r[4], 0x1u32)"), "{src}");
    }
}

#[cfg(test)]
mod switch_tests {
    use super::*;

    fn b(from: u32, to: u32) -> u32 {
        0xEA00_0000 | ((to.wrapping_sub(from + 8) >> 2) & 0x00FF_FFFF)
    }

    #[test]
    fn switch_table_targets_become_blocks() {
        let base = 0x0200_0000u32;
        let mut w = vec![0u32; 14];
        w[0] = 0xE350_0002; // cmp r0, #2
        w[1] = 0x908F_F100; // addls pc, pc, r0, lsl #2
        w[2] = b(base + 8, base + 0x30); // b default
        w[3] = b(base + 0xC, base + 0x18); // case 0
        w[4] = b(base + 0x10, base + 0x20); // case 1
        w[5] = b(base + 0x14, base + 0x28); // case 2
        for (i, v) in [(6, 1u32), (8, 2), (10, 3), (12, 0)] {
            w[i] = 0xE3A0_1000 | v; // mov r1, #v
            w[i + 1] = 0xE12F_FF1E; // bx lr
        }
        let bytes: Vec<u8> = w.iter().flat_map(|x| x.to_le_bytes()).collect();
        let img = Image::new(base, &bytes);
        let f = discover(&img, base, Some((base, base + 0x38)), 100).unwrap();
        for case in [0x18, 0x20, 0x28, 0x30] {
            assert!(f.blocks.contains(&(base + case)), "case body {case:#x} must be a block");
        }
        for entry in [0xC, 0x10, 0x14] {
            assert!(f.blocks.contains(&(base + entry)), "table entry {entry:#x} must be a block");
        }
    }

    #[test]
    fn plain_add_to_pc_without_table_is_unchanged() {
        let base = 0x0200_0000u32;
        // add pc, pc, r0, lsl #2 followed by a non-branch: no table entries are invented
        let w = [0xE08F_F100u32, 0xE3A0_1001, 0xE12F_FF1E, 0xE12F_FF1E];
        let bytes: Vec<u8> = w.iter().flat_map(|x| x.to_le_bytes()).collect();
        let img = Image::new(base, &bytes);
        let f = discover(&img, base, Some((base, base + 16)), 100).unwrap();
        assert!(!f.blocks.contains(&(base + 8)) || f.insts.contains_key(&(base + 8)));
    }
}
