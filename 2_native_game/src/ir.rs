//! Block 4A: the lifter's intermediate representation.
//!
//! Each decoded instruction is lowered to a short list of statements whose reads and writes are
//! explicit: registers ([`E::Reg`], [`S::SetReg`]), flags ([`E::Flag`], [`S::SetFlag`], [`S::SetNz`]),
//! memory ([`E::Load`], [`S::Store`]), helper calls with known names ([`H`]) and control flow
//! ([`S::Goto`], [`S::Call`]). Decompiler passes (block 4B) work on this form; [`print`] turns it
//! back into the exact Rust the generator has always produced (tier T0).
//!
//! The T0 text is part of the contract: `tests/ir_t0.rs` checks that [`print`] of [`lower`] equals
//! the legacy string emitter for every instruction encoding it tries, up to whitespace.
use crate::arm::{pc_read, Decoded, HalfMul, Inst, Off, Op2};
use crate::lift::Image;
use std::fmt::Write;

/// Statement-local temporaries. Their names are part of the T0 text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tmp {
    X,
    Sc,
    Res,
    Ea,
    Ax,
    V,
    V0,
    V1,
    L,
    H,
    Target,
    Old,
}

impl Tmp {
    pub fn name(self) -> &'static str {
        match self {
            Tmp::X => "x",
            Tmp::Sc => "sc",
            Tmp::Res => "res",
            Tmp::Ea => "ea",
            Tmp::Ax => "ax",
            Tmp::V => "v",
            Tmp::V0 => "v0",
            Tmp::V1 => "v1",
            Tmp::L => "l",
            Tmp::H => "h",
            Tmp::Target => "target",
            Tmp::Old => "old",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flag {
    N,
    Z,
    C,
    V,
}

/// Wrapping arithmetic and bitwise operators on `u32`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bin {
    And,
    Or,
    Xor,
    Add,
    Sub,
    Mul,
}

/// Runtime helpers. Their effects on the CPU are documented per variant; block 4B relies on them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum H {
    /// `R::subs(c, a, b)`: a - b, sets N Z C V.
    Subs,
    /// `R::adds(c, a, b)`: a + b, sets N Z C V.
    Adds,
    /// `R::adcs(c, a, b)`: a + b + C, sets N Z C V.
    Adcs,
    /// `R::sbcs(c, a, b)`: a - b - !C, sets N Z C V.
    Sbcs,
    /// `R::shift_reg(kind, v, rs, c.c)` -> (value, carry). Pure.
    ShiftReg,
    /// `R::shift_imm(kind, v, n, c.c).0`. Pure.
    ShiftImmValue,
    /// `R::ldr(m, ea)`: rotated word load.
    Ldr,
    /// `R::mul_long(c, signed, acc, s, rm, rs, lo, hi)` -> (lo, hi); sets N Z when `s`.
    MulLong,
    /// `R::smlal_xy(x, y, rm, rs, lo, hi)` -> (lo, hi). Pure.
    SmlalXy,
    /// `R::half_mul(c, op, x, y, rm, rs, rn)`; may set Q.
    HalfMul,
    /// `R::qalu(c, op, rm, rn)`; may set Q.
    Qalu,
    /// `R::block(c, m, load, pre, up, wb, rn, list, pc+12)` -> loaded pc. Reads/writes the listed registers and rn.
    Block,
    /// `R::block_user(...)`: as `Block`, user-bank registers.
    BlockUser,
    /// `R::swp(m, addr, value, byte)` -> old value.
    Swp,
    /// `m.swi(c, n)`: the BIOS call; reads and writes any register.
    Swi,
    /// `m.mrc(c, cp, op1, crn, crm, op2)`.
    Mrc,
    /// `m.mcr(c, cp, op1, crn, crm, op2, v)`.
    Mcr,
    /// `c.msr(spsr, mask, v)`: writes CPSR/SPSR fields (flags, mode, banks).
    Msr,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Width {
    B8,
    B16,
    B32,
}

#[derive(Clone, Debug, PartialEq)]
pub enum E {
    /// `c.r[i]`.
    Reg(u8),
    /// `0x..u32`.
    Const(u32),
    /// `{v}u32` (decimal).
    ConstDec(u32),
    /// A bare integer literal `{v}` (type inferred).
    Lit(u32),
    /// A bare hexadecimal literal `{v:#X}` (type inferred).
    Hex(u32),
    Bool(bool),
    Tmp(Tmp),
    Flag(Flag),
    /// The condition code `cc` over the flags.
    Cond(u8),
    /// `c.spsr`.
    Spsr,
    /// `c.cpsr()`.
    Cpsr,
    /// `c` / `m` passed to a helper.
    CpuArg,
    MemArg,
    /// `recomp::arm::HalfMul::{op}`.
    HalfMulOp(HalfMul),
    Bin(Bin, Box<E>, Box<E>),
    /// Bitwise (u32) or logical (bool) not.
    Not(Box<E>),
    /// `(a << n)`, `(a >> n)`, `(((a as i32) >> n) as u32)`, `a.rotate_right(n)`.
    Shl(Box<E>, u32),
    Lsr(Box<E>, u32),
    Asr(Box<E>, u32),
    Ror(Box<E>, u32),
    /// `((a >> 1) | ((c.c as u32) << 31))`.
    Rrx(Box<E>),
    /// Bit `k` of `a` as a bool: `(a >> k) & 1 != 0`.
    Bit(Box<E>, u32),
    /// Bit 31: `a >> 31 != 0`.
    Msb(Box<E>),
    /// Bit 0: `a & 1 != 0`.
    Lsb(Box<E>),
    /// `b as u32` for a bool.
    BoolU32(Box<E>),
    /// `a.leading_zeros()`.
    Clz(Box<E>),
    /// `iw(a)`: BX interworking of a target address.
    Iw(Box<E>),
    /// A memory read; `addr` already includes alignment masking.
    Load { w: Width, signed: bool, addr: Box<E> },
    /// A helper call.
    Call(H, Vec<E>),
    /// A comparison (T1 only, from a compare whose flags only one condition reads): `signed`
    /// compares as `i32`.
    Cmp(CmpOp, bool, Box<E>, Box<E>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

#[derive(Clone, Debug, PartialEq)]
pub enum S {
    /// `let t = e;`
    Let(Tmp, E),
    /// `let (a, b) = e;`
    Let2(Tmp, Tmp, E),
    /// `c.r[i] = e;`
    SetReg(u8, E),
    /// `c.n = e;` etc.
    SetFlag(Flag, E),
    /// `R::set_nz(c, e);`
    SetNz(E),
    /// `m.w8(addr, v as u8);` etc.
    Store { w: Width, addr: E, val: E },
    /// A helper call for its effects: `e;`
    Do(E),
    /// `pc = e; continue;`
    Goto(E),
    /// Exception return from a value `e`: CPSR = SPSR, then jump in the restored state.
    ExcReturn(E),
    /// `let x = m.call(c, target, ret); if x != ret { pc = x; continue; }` (r14 is set before).
    Call { target: E, ret: u32 },
    /// `if let Some(v) = e { body }`: a block load that loaded the pc.
    IfSome(E, Vec<S>),
}

fn b(e: E) -> Box<E> {
    Box::new(e)
}

fn tmp(t: Tmp) -> E {
    E::Tmp(t)
}

fn bin(op: Bin, a: E, x: E) -> E {
    E::Bin(op, b(a), b(x))
}

/// `a & !k` with a bare literal mask.
fn mask(a: E, k: u32) -> E {
    bin(Bin::And, a, E::Not(b(E::Lit(k))))
}

fn call(h: H, args: Vec<E>) -> E {
    E::Call(h, args)
}

/// A register read by the instruction at `addr` (r15 reads the pipeline pc as a constant).
fn r(i: usize, addr: u32, t: bool) -> E {
    if i == 15 { E::Const(pc_read(addr, t)) } else { E::Reg(i as u8) }
}

/// The value a store of `rd` writes (r15: address + 12 in ARM state).
fn store_val(rd: usize, addr: u32, t: bool) -> E {
    if rd == 15 { E::Const(pc_read(addr, t) + 4) } else { E::Reg(rd as u8) }
}

/// A shifter operand: (statements binding it, value, carry-out; `None` leaves C unchanged).
fn operand(o: Op2, addr: u32, t: bool) -> (Vec<S>, E, Option<E>) {
    let x = || tmp(Tmp::X);
    match o {
        Op2::Imm { imm8, rot } => {
            let v = imm8.rotate_right(rot * 2);
            (vec![], E::Const(v), if rot == 0 { None } else { Some(E::Bool(v >> 31 != 0)) })
        }
        Op2::Const(v) => (vec![], E::Const(v), None),
        Op2::Reg { rm, kind, amt } => {
            let src = r(rm, addr, t);
            if (kind, amt) == (0, 0) {
                return (vec![], src, None);
            }
            let pre = vec![S::Let(Tmp::X, src)];
            let (value, carry) = match (kind, amt) {
                (0, n) => (E::Shl(b(x()), n), E::Bit(b(x()), 32 - n)),
                (1, 0) => (E::ConstDec(0), E::Msb(b(x()))),
                (1, n) => (E::Lsr(b(x()), n), E::Bit(b(x()), n - 1)),
                (2, 0) => (E::Asr(b(x()), 31), E::Msb(b(x()))),
                (2, n) => (E::Asr(b(x()), n), E::Bit(b(x()), n - 1)),
                (_, 0) => (E::Rrx(b(x())), E::Lsb(b(x()))),
                (_, n) => (E::Ror(b(x()), n), E::Bit(b(x()), n - 1)),
            };
            (pre, value, Some(carry))
        }
        // register-shifted register: r15 reads one word further
        Op2::RegShift { rm, kind, rs } => {
            let v = if rm == 15 { E::Const(pc_read(addr, t) + 4) } else { E::Reg(rm as u8) };
            let sh = call(H::ShiftReg, vec![E::Lit(kind as u32), v, r(rs, addr, t), E::Flag(Flag::C)]);
            (vec![S::Let2(Tmp::X, Tmp::Sc, sh)], x(), Some(tmp(Tmp::Sc)))
        }
    }
}

/// The offset of a single transfer: `None` for zero.
fn off(o: Off, addr: u32, t: bool) -> Option<E> {
    match o {
        Off::Imm(0) => None,
        Off::Imm(v) => Some(E::Const(v)),
        Off::Reg { rm, kind: 0, amt: 0 } => Some(r(rm, addr, t)),
        Off::Reg { rm, kind: 0, amt } => Some(E::Shl(b(r(rm, addr, t)), amt)),
        Off::Reg { rm, kind, amt } => {
            Some(call(H::ShiftImmValue, vec![E::Lit(kind as u32), r(rm, addr, t), E::Lit(amt), E::Flag(Flag::C)]))
        }
    }
}

/// Statements binding `ea` (and `ax`, the written-back address). The constant address when the
/// base is the pc and the offset an immediate.
fn ea(rn: usize, o: Off, up: bool, pre: bool, wb: bool, addr: u32, t: bool) -> (Vec<S>, Option<u32>) {
    if rn == 15 {
        if let Off::Imm(v) = o {
            let base = pc_read(addr, t);
            let a = if !pre { base } else if up { base.wrapping_add(v) } else { base.wrapping_sub(v) };
            return (vec![S::Let(Tmp::Ea, E::Const(a))], Some(a));
        }
    }
    let base = r(rn, addr, t);
    let Some(o) = off(o, addr, t) else {
        return (vec![S::Let(Tmp::Ea, base), S::Let(Tmp::Ax, tmp(Tmp::Ea))], None);
    };
    let op = if up { Bin::Add } else { Bin::Sub };
    let s = if pre && !wb {
        vec![S::Let(Tmp::Ea, bin(op, base, o))]
    } else if pre {
        vec![S::Let(Tmp::Ax, bin(op, base, o)), S::Let(Tmp::Ea, tmp(Tmp::Ax))]
    } else {
        vec![S::Let(Tmp::Ea, base), S::Let(Tmp::Ax, bin(op, tmp(Tmp::Ea), o))]
    };
    (s, None)
}

/// `pc = iw(e); continue;`
fn goto_iw(e: E) -> S {
    S::Goto(E::Iw(b(e)))
}

/// The statements of one instruction (its condition is applied by the caller). `next` is the code
/// address after it; `img` allows folding literal pool loads (word loads from a constant address
/// inside the function's own image).
pub fn lower(d: &Decoded, addr: u32, next: u32, img: Option<&Image>) -> Vec<S> {
    let t = d.thumb;
    match d.inst {
        Inst::Dp { op, s, rn, rd, op2: o } => {
            let (mut x, rhs, carry) = operand(o, addr, t);
            let a = r(rn, addr, t);
            let logical = matches!(op, 0 | 1 | 8 | 9 | 12 | 13 | 14 | 15);
            let value = match op {
                0 | 8 => bin(Bin::And, a, rhs),
                1 | 9 => bin(Bin::Xor, a, rhs),
                12 => bin(Bin::Or, a, rhs),
                13 => rhs,
                14 => bin(Bin::And, a, E::Not(b(rhs))),
                15 => E::Not(b(rhs)),
                _ if s => match op {
                    2 | 10 => call(H::Subs, vec![E::CpuArg, a, rhs]),
                    3 => call(H::Subs, vec![E::CpuArg, rhs, a]),
                    4 | 11 => call(H::Adds, vec![E::CpuArg, a, rhs]),
                    5 => call(H::Adcs, vec![E::CpuArg, a, rhs]),
                    6 => call(H::Sbcs, vec![E::CpuArg, a, rhs]),
                    _ => call(H::Sbcs, vec![E::CpuArg, rhs, a]),
                },
                2 => bin(Bin::Sub, a, rhs),
                3 => bin(Bin::Sub, rhs, a),
                4 => bin(Bin::Add, a, rhs),
                5 => bin(Bin::Add, bin(Bin::Add, a, rhs), E::BoolU32(b(E::Flag(Flag::C)))),
                6 => bin(Bin::Sub, bin(Bin::Sub, a, rhs), E::BoolU32(b(E::Not(b(E::Flag(Flag::C)))))),
                _ => bin(Bin::Sub, bin(Bin::Sub, rhs, a), E::BoolU32(b(E::Not(b(E::Flag(Flag::C)))))),
            };
            x.push(S::Let(Tmp::Res, value));
            if s && logical {
                x.push(S::SetNz(tmp(Tmp::Res)));
                if let Some(cy) = carry {
                    x.push(S::SetFlag(Flag::C, cy));
                }
            }
            if !Inst::is_compare(op) {
                if rd == 15 && s {
                    x.push(S::ExcReturn(tmp(Tmp::Res)));
                } else if rd == 15 {
                    x.push(S::Goto(if t { bin(Bin::Or, tmp(Tmp::Res), E::Lit(1)) } else { mask(tmp(Tmp::Res), 3) }));
                } else {
                    x.push(S::SetReg(rd as u8, tmp(Tmp::Res)));
                }
            }
            x
        }
        Inst::Mul { acc, s, rd, rn, rs, rm } => {
            let mut v = bin(Bin::Mul, r(rm, addr, t), r(rs, addr, t));
            if acc {
                v = bin(Bin::Add, v, r(rn, addr, t));
            }
            let mut x = vec![S::Let(Tmp::Res, v)];
            if s {
                x.push(S::SetNz(tmp(Tmp::Res)));
            }
            x.push(S::SetReg(rd as u8, tmp(Tmp::Res)));
            x
        }
        Inst::MulLong { signed, acc, s, lo, hi, rs, rm } => {
            let args = vec![
                E::CpuArg,
                E::Bool(signed),
                E::Bool(acc),
                E::Bool(s),
                r(rm, addr, t),
                r(rs, addr, t),
                E::Reg(lo as u8),
                E::Reg(hi as u8),
            ];
            vec![S::Let2(Tmp::L, Tmp::H, call(H::MulLong, args)), S::SetReg(lo as u8, tmp(Tmp::L)), S::SetReg(hi as u8, tmp(Tmp::H))]
        }
        Inst::MulHalf { op: HalfMul::Smlal, x, y, rd, rn, rs, rm } => {
            let args = vec![E::Bool(x), E::Bool(y), r(rm, addr, t), r(rs, addr, t), E::Reg(rn as u8), E::Reg(rd as u8)];
            vec![S::Let2(Tmp::L, Tmp::H, call(H::SmlalXy, args)), S::SetReg(rn as u8, tmp(Tmp::L)), S::SetReg(rd as u8, tmp(Tmp::H))]
        }
        Inst::MulHalf { op, x, y, rd, rn, rs, rm } => {
            let args = vec![E::CpuArg, E::HalfMulOp(op), E::Bool(x), E::Bool(y), r(rm, addr, t), r(rs, addr, t), r(rn, addr, t)];
            vec![S::SetReg(rd as u8, call(H::HalfMul, args))]
        }
        Inst::Qalu { op, rd, rn, rm } => {
            vec![S::SetReg(rd as u8, call(H::Qalu, vec![E::CpuArg, E::Lit(op as u32), r(rm, addr, t), r(rn, addr, t)]))]
        }
        Inst::Clz { rd, rm } => vec![S::SetReg(rd as u8, E::Clz(b(r(rm, addr, t))))],
        Inst::Mrs { rd, spsr } => vec![S::SetReg(rd as u8, if spsr { E::Spsr } else { E::Cpsr })],
        Inst::Msr { spsr, mask: m, src } => {
            let (mut x, v, _) = operand(src, addr, t);
            x.push(S::Do(call(H::Msr, vec![E::Bool(spsr), E::Lit(m as u32), v])));
            x
        }
        Inst::B { target } => vec![S::Goto(E::Const(target))],
        Inst::Call { target } => {
            let ret = next | t as u32;
            vec![S::SetReg(14, E::Const(ret)), S::Call { target: E::Const(target), ret }]
        }
        Inst::Bx { rm, link } => {
            if link {
                let ret = next | t as u32;
                vec![
                    S::Let(Tmp::Target, E::Iw(b(r(rm, addr, t)))),
                    S::SetReg(14, E::Const(ret)),
                    S::Call { target: tmp(Tmp::Target), ret },
                ]
            } else {
                vec![goto_iw(r(rm, addr, t))]
            }
        }
        Inst::Mem { load, byte, pre, up, wb, rn, rd, off: o } => {
            let (mut x, at) = ea(rn, o, up, pre, wb, addr, t);
            if load {
                // literal pool: a word in the function's own (read-only) image
                if let (Some(a), Some(img), false, false) = (at, img, byte, wb) {
                    if a % 4 == 0 {
                        if let Some(v) = img.read32(a) {
                            return vec![if rd == 15 { goto_iw(E::Const(v)) } else { S::SetReg(rd as u8, E::Const(v)) }];
                        }
                    }
                }
                let get = if byte {
                    E::Load { w: Width::B8, signed: false, addr: b(tmp(Tmp::Ea)) }
                } else {
                    call(H::Ldr, vec![E::MemArg, tmp(Tmp::Ea)])
                };
                x.push(S::Let(Tmp::V, get));
                if wb {
                    x.push(S::SetReg(rn as u8, tmp(Tmp::Ax)));
                }
                x.push(if rd == 15 { goto_iw(tmp(Tmp::V)) } else { S::SetReg(rd as u8, tmp(Tmp::V)) });
            } else {
                let val = store_val(rd, addr, t);
                x.push(if byte {
                    S::Store { w: Width::B8, addr: tmp(Tmp::Ea), val }
                } else {
                    S::Store { w: Width::B32, addr: mask(tmp(Tmp::Ea), 3), val }
                });
                if wb {
                    x.push(S::SetReg(rn as u8, tmp(Tmp::Ax)));
                }
            }
            x
        }
        Inst::MemH { load, kind, pre, up, wb, rn, rd, off: o } => {
            let (mut x, _) = ea(rn, o, up, pre, wb, addr, t);
            if load {
                let get = match kind {
                    1 => E::Load { w: Width::B16, signed: false, addr: b(mask(tmp(Tmp::Ea), 1)) },
                    2 => E::Load { w: Width::B8, signed: true, addr: b(tmp(Tmp::Ea)) },
                    _ => E::Load { w: Width::B16, signed: true, addr: b(mask(tmp(Tmp::Ea), 1)) },
                };
                x.push(S::Let(Tmp::V, get));
                if wb {
                    x.push(S::SetReg(rn as u8, tmp(Tmp::Ax)));
                }
                x.push(S::SetReg(rd as u8, tmp(Tmp::V)));
            } else {
                x.push(S::Store { w: Width::B16, addr: mask(tmp(Tmp::Ea), 1), val: store_val(rd, addr, t) });
                if wb {
                    x.push(S::SetReg(rn as u8, tmp(Tmp::Ax)));
                }
            }
            x
        }
        Inst::MemD { load, pre, up, wb, rn, rd, off: o } => {
            let (mut x, _) = ea(rn, o, up, pre, wb, addr, t);
            let lo = mask(tmp(Tmp::Ea), 3);
            let hi = mask(bin(Bin::Add, tmp(Tmp::Ea), E::Lit(4)), 3);
            if load {
                x.push(S::Let(Tmp::V0, E::Load { w: Width::B32, signed: false, addr: b(lo) }));
                x.push(S::Let(Tmp::V1, E::Load { w: Width::B32, signed: false, addr: b(hi) }));
                if wb {
                    x.push(S::SetReg(rn as u8, tmp(Tmp::Ax)));
                }
                x.push(S::SetReg(rd as u8, tmp(Tmp::V0)));
                x.push(S::SetReg(rd as u8 + 1, tmp(Tmp::V1)));
            } else {
                x.push(S::Store { w: Width::B32, addr: lo, val: E::Reg(rd as u8) });
                x.push(S::Store { w: Width::B32, addr: hi, val: E::Reg(rd as u8 + 1) });
                if wb {
                    x.push(S::SetReg(rn as u8, tmp(Tmp::Ax)));
                }
            }
            x
        }
        Inst::Block { load, pre, up, wb, rn, list } | Inst::BlockUser { load, pre, up, wb, rn, list } => {
            let user = matches!(d.inst, Inst::BlockUser { .. });
            let args = vec![
                E::CpuArg,
                E::MemArg,
                E::Bool(load),
                E::Bool(pre),
                E::Bool(up),
                E::Bool(wb),
                E::Lit(rn as u32),
                E::Hex(list as u32),
                E::Const(pc_read(addr, t) + 4),
            ];
            let c = call(if user { H::BlockUser } else { H::Block }, args);
            if load && list & 0x8000 != 0 {
                let body = if user { S::ExcReturn(tmp(Tmp::V)) } else { goto_iw(tmp(Tmp::V)) };
                vec![S::IfSome(c, vec![body])]
            } else {
                vec![S::Do(c)]
            }
        }
        Inst::Swp { byte, rn, rd, rm } => vec![
            S::Let(Tmp::Old, call(H::Swp, vec![E::MemArg, E::Reg(rn as u8), E::Reg(rm as u8), E::Bool(byte)])),
            S::SetReg(rd as u8, tmp(Tmp::Old)),
        ],
        Inst::Swi { imm } => vec![S::Do(call(H::Swi, vec![E::CpuArg, E::Lit(crate::rt::swi_number(imm, t))]))],
        Inst::Cp { load, cp, op1, crn, crm, op2: o2, rd } => {
            let mut args: Vec<E> = vec![E::CpuArg];
            args.extend([cp, op1, crn, crm, o2].map(|v| E::Lit(v as u32)));
            if load {
                if rd == 15 {
                    let v = || b(tmp(Tmp::V));
                    vec![
                        S::Let(Tmp::V, call(H::Mrc, args)),
                        S::SetFlag(Flag::N, E::Msb(v())),
                        S::SetFlag(Flag::Z, E::Bit(v(), 30)),
                        S::SetFlag(Flag::C, E::Bit(v(), 29)),
                        S::SetFlag(Flag::V, E::Bit(v(), 28)),
                    ]
                } else {
                    vec![S::SetReg(rd as u8, call(H::Mrc, args))]
                }
            } else {
                args.push(r(rd, addr, t));
                vec![S::Do(call(H::Mcr, args))]
            }
        }
        Inst::Nop => vec![],
        Inst::Undecoded(_) => unreachable!("discovery never keeps undecoded instructions"),
    }
}

// ---- T0 printer ----------------------------------------------------------------------------

/// Binding strength for parenthesisation: atoms and postfix forms bind tightest.
fn prec(e: &E) -> u8 {
    match e {
        E::Bin(Bin::And, ..) => 3,
        E::Bin(Bin::Xor, ..) => 2,
        E::Bin(Bin::Or, ..) => 1,
        E::Not(_) => 8,
        E::Load { w, .. } if *w != Width::B32 => 6, // `... as u32`
        E::BoolU32(_) => 6,
        E::Bit(..) | E::Msb(_) | E::Lsb(_) | E::Cmp(..) => 0, // `... != 0`
        E::Cond(cc) if matches!(cc, 8..=13) => 9, // printed with its own parentheses
        _ => 9,
    }
}

fn flag(f: Flag) -> &'static str {
    match f {
        Flag::N => "c.n",
        Flag::Z => "c.z",
        Flag::C => "c.c",
        Flag::V => "c.v",
    }
}

/// The condition `cc` as a Rust expression over the flags.
pub fn cond_expr(cc: u8) -> &'static str {
    ["c.z", "!c.z", "c.c", "!c.c", "c.n", "!c.n", "c.v", "!c.v", "(c.c && !c.z)", "(!c.c || c.z)", "(c.n == c.v)", "(c.n != c.v)", "(!c.z && c.n == c.v)", "(c.z || c.n != c.v)", "true", "true"][cc as usize & 15]
}

/// `e`, parenthesised if it binds looser than `min`.
fn sub(out: &mut String, e: &E, min: u8) {
    if prec(e) < min {
        out.push('(');
        expr(out, e);
        out.push(')');
    } else {
        expr(out, e);
    }
}

fn args(out: &mut String, a: &[E]) {
    for (i, e) in a.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        expr(out, e);
    }
}

thread_local! {
    /// Print registers as the T1 local variables (`r0`.., `sp`, `lr`) instead of `c.r[i]`.
    static LOCALS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// Named struct fields by offset for the function being printed (T1 accessors).
    static FIELDS: std::cell::RefCell<std::collections::HashMap<u32, FieldRef>> = std::cell::RefCell::new(Default::default());
    /// The accessors printed since the last [`take_field_uses`].
    static FIELD_USES: std::cell::RefCell<Vec<(FieldRef, Access)>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// A named struct field: T1 prints `base + off` memory accesses as accessors
/// (`kart::racer_index(m, r4)`, `kart::set_racer_index(m, r4, r0)`), defined in the `layout` crate.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FieldRef {
    /// The research struct name (`Kart`).
    pub strukt: String,
    /// Its Rust module (`kart`).
    pub module: String,
    /// The field's Rust name (`racer_index`).
    pub name: String,
    pub off: u32,
    /// The access its type implies (the accessor without a width suffix), if the type is a scalar.
    pub primary: Option<Access>,
}

/// A memory access kind, by the runtime helper that does it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Access {
    U8,
    I8,
    U16,
    I16,
    /// `R::ldr` (word load, rotated if unaligned).
    U32,
    /// `R::ld32` (aligned word load).
    W32,
    St8,
    St16,
    St32,
}

impl Access {
    pub fn is_store(self) -> bool {
        matches!(self, Access::St8 | Access::St16 | Access::St32)
    }
    /// The `R::` helper.
    pub fn helper(self) -> &'static str {
        match self {
            Access::U8 => "ld8",
            Access::I8 => "ld8s",
            Access::U16 => "ld16",
            Access::I16 => "ld16s",
            Access::U32 => "ldr",
            Access::W32 => "ld32",
            Access::St8 => "st8",
            Access::St16 => "st16",
            Access::St32 => "st32",
        }
    }
    fn store(self) -> Access {
        match self {
            Access::U8 | Access::I8 | Access::St8 => Access::St8,
            Access::U16 | Access::I16 | Access::St16 => Access::St16,
            _ => Access::St32,
        }
    }
    fn suffix(self) -> &'static str {
        match self {
            Access::U8 | Access::St8 => "_u8",
            Access::I8 => "_i8",
            Access::U16 | Access::St16 => "_u16",
            Access::I16 => "_i16",
            Access::U32 | Access::St32 => "_u32",
            Access::W32 => "_w32",
        }
    }
}

impl FieldRef {
    /// The accessor's name: `racer_index` / `set_racer_index` for the field's own width, else with
    /// the width (`racer_index_u8`).
    pub fn accessor(&self, a: Access) -> String {
        let base = if a.is_store() { format!("set_{}", self.name) } else { self.name.clone() };
        let own = self.primary.is_some_and(|p| if a.is_store() { p.store() == a } else { p == a });
        if own { base } else { format!("{base}{}", a.suffix()) }
    }
}

/// Sets the named fields (by offset) for the function about to be printed; empty turns accessors off.
pub fn set_fields(fields: std::collections::HashMap<u32, FieldRef>) {
    FIELDS.with(|f| *f.borrow_mut() = fields);
}

/// The accessors printed since the last call.
pub fn take_field_uses() -> Vec<(FieldRef, Access)> {
    FIELD_USES.with(|u| std::mem::take(&mut *u.borrow_mut()))
}

/// The number of accessors printed so far (to tell whether a statement printed one).
pub fn field_uses_len() -> usize {
    FIELD_USES.with(|u| u.borrow().len())
}

/// Every constant in `s` (candidate field offsets).
pub fn consts_in(s: &S, out: &mut Vec<u32>) {
    fn walk(e: &mut E, out: &mut Vec<u32>) {
        if let E::Const(v) = e {
            out.push(*v);
        }
        for c in expr_children(e) {
            walk(c, out);
        }
    }
    let mut s = s.clone();
    for e in stmt_exprs(&mut s) {
        walk(e, out);
    }
}

/// `base + off` (or `base` alone) at a named field: prints the accessor call's start
/// (`kart::racer_index(m, base`) and records the use.
fn field_access(out: &mut String, addr: &E, a: Access) -> bool {
    let (base, off) = match addr {
        E::Bin(Bin::Add, b, k) => match **k {
            E::Const(v) => (&**b, v),
            _ => return false,
        },
        E::Reg(_) => (addr, 0),
        _ => return false,
    };
    let Some(f) = FIELDS.with(|m| m.borrow().get(&off).cloned()) else { return false };
    let _ = write!(out, "{}::{}(m, ", f.module, f.accessor(a));
    expr(out, base);
    FIELD_USES.with(|u| u.borrow_mut().push((f, a)));
    true
}

/// The T1 local variable holding register `i`.
pub fn reg_name(i: u8) -> String {
    match i {
        13 => "sp".into(),
        14 => "lr".into(),
        _ => format!("r{i}"),
    }
}

fn reg(out: &mut String, i: u8) {
    if LOCALS.with(|l| l.get()) {
        out.push_str(&reg_name(i));
    } else {
        let _ = write!(out, "c.r[{i}]");
    }
}

/// [`print`] with registers as T1 local variables.
pub fn print_locals(stmts: &[S]) -> String {
    LOCALS.with(|l| l.set(true));
    let s = print(stmts);
    LOCALS.with(|l| l.set(false));
    s
}

/// [`expr`] text with registers as T1 local variables.
pub fn expr_locals(e: &E) -> String {
    LOCALS.with(|l| l.set(true));
    let mut s = String::new();
    expr(&mut s, e);
    LOCALS.with(|l| l.set(false));
    s
}

fn expr(out: &mut String, e: &E) {
    match e {
        E::Reg(i) => reg(out, *i),
        E::Const(v) => {
            let _ = write!(out, "{v:#X}u32");
        }
        E::ConstDec(v) => {
            let _ = write!(out, "{v}u32");
        }
        E::Lit(v) => {
            let _ = write!(out, "{v}");
        }
        E::Hex(v) => {
            let _ = write!(out, "{v:#X}");
        }
        E::Bool(v) => {
            let _ = write!(out, "{v}");
        }
        E::Tmp(t) => out.push_str(t.name()),
        E::Flag(f) => out.push_str(flag(*f)),
        E::Cond(cc) => out.push_str(cond_expr(*cc)),
        E::Spsr => out.push_str("c.spsr"),
        E::Cpsr => out.push_str("c.cpsr()"),
        E::CpuArg => out.push('c'),
        E::MemArg => out.push('m'),
        E::HalfMulOp(op) => {
            let _ = write!(out, "recomp::arm::HalfMul::{op:?}");
        }
        E::Bin(op @ (Bin::And | Bin::Or | Bin::Xor), a, x) => {
            let (p, sym) = match op {
                Bin::And => (3, " & "),
                Bin::Xor => (2, " ^ "),
                _ => (1, " | "),
            };
            sub(out, a, p);
            out.push_str(sym);
            sub(out, x, p + 1);
        }
        E::Bin(op, a, x) => {
            sub(out, a, 9);
            out.push_str(match op {
                Bin::Add => ".wrapping_add(",
                Bin::Sub => ".wrapping_sub(",
                _ => ".wrapping_mul(",
            });
            expr(out, x);
            out.push(')');
        }
        E::Not(a) => {
            out.push('!');
            sub(out, a, 9);
        }
        E::Shl(a, n) => {
            out.push('(');
            sub(out, a, 9);
            let _ = write!(out, " << {n})");
        }
        E::Lsr(a, n) => {
            out.push('(');
            sub(out, a, 9);
            let _ = write!(out, " >> {n})");
        }
        E::Asr(a, n) => {
            out.push_str("(((");
            sub(out, a, 9);
            let _ = write!(out, " as i32) >> {n}) as u32)");
        }
        E::Ror(a, n) => {
            sub(out, a, 9);
            let _ = write!(out, ".rotate_right({n})");
        }
        E::Rrx(a) => {
            out.push_str("((");
            sub(out, a, 9);
            out.push_str(" >> 1) | ((c.c as u32) << 31))");
        }
        E::Bit(a, k) => {
            out.push('(');
            sub(out, a, 9);
            let _ = write!(out, " >> {k}) & 1 != 0");
        }
        E::Msb(a) => {
            sub(out, a, 9);
            out.push_str(" >> 31 != 0");
        }
        E::Lsb(a) => {
            sub(out, a, 9);
            out.push_str(" & 1 != 0");
        }
        E::BoolU32(a) => {
            sub(out, a, 9);
            out.push_str(" as u32");
        }
        E::Clz(a) => {
            sub(out, a, 9);
            out.push_str(".leading_zeros()");
        }
        E::Iw(a) => {
            out.push_str("iw(");
            expr(out, a);
            out.push(')');
        }
        E::Load { w, signed, addr } if LOCALS.with(|l| l.get()) && unmask(addr, *w).is_some() => {
            let acc = match (w, signed) {
                (Width::B8, false) => Access::U8,
                (Width::B8, true) => Access::I8,
                (Width::B16, false) => Access::U16,
                (Width::B16, true) => Access::I16,
                (Width::B32, _) => Access::W32,
            };
            if field_access(out, unmask(addr, *w).unwrap(), acc) {
                out.push(')');
                return;
            }
            let name = match (w, signed) {
                (Width::B8, false) => "ld8",
                (Width::B8, true) => "ld8s",
                (Width::B16, false) => "ld16",
                (Width::B16, true) => "ld16s",
                (Width::B32, _) => "ld32",
            };
            let _ = write!(out, "R::{name}(m, ");
            expr(out, unmask(addr, *w).unwrap());
            out.push(')');
        }
        E::Load { w, signed, addr } => {
            out.push_str(match w {
                Width::B8 => "m.r8(",
                Width::B16 => "m.r16(",
                Width::B32 => "m.r32(",
            });
            expr(out, addr);
            out.push(')');
            out.push_str(match (w, signed) {
                (Width::B32, _) => "",
                (_, false) => " as u32",
                (Width::B8, true) => " as i8 as i32 as u32",
                (Width::B16, true) => " as i16 as i32 as u32",
            });
        }
        E::Cmp(op, signed, a, b) => {
            let side = |out: &mut String, e: &E| {
                if *signed {
                    out.push('(');
                    sub(out, e, 9);
                    out.push_str(" as i32)");
                } else {
                    sub(out, e, 9);
                }
            };
            side(out, a);
            out.push_str(match op {
                CmpOp::Eq => " == ",
                CmpOp::Ne => " != ",
                CmpOp::Lt => " < ",
                CmpOp::Le => " <= ",
                CmpOp::Gt => " > ",
                CmpOp::Ge => " >= ",
            });
            side(out, b);
        }
        E::Call(H::Ldr, a) if LOCALS.with(|l| l.get()) && a.len() == 2 => {
            if field_access(out, &a[1], Access::U32) {
                out.push(')');
                return;
            }
            out.push_str("R::ldr(m, ");
            expr(out, &a[1]);
            out.push(')');
        }
        E::Call(h, a) => {
            out.push_str(match h {
                H::Subs => "R::subs(",
                H::Adds => "R::adds(",
                H::Adcs => "R::adcs(",
                H::Sbcs => "R::sbcs(",
                H::ShiftReg => "R::shift_reg(",
                H::ShiftImmValue => "R::shift_imm(",
                H::Ldr => "R::ldr(",
                H::MulLong => "R::mul_long(",
                H::SmlalXy => "R::smlal_xy(",
                H::HalfMul => "R::half_mul(",
                H::Qalu => "R::qalu(",
                H::Block => "R::block(",
                H::BlockUser => "R::block_user(",
                H::Swp => "R::swp(",
                H::Swi => "m.swi(",
                H::Mrc => "m.mrc(",
                H::Mcr => "m.mcr(",
                H::Msr => "c.msr(",
            });
            args(out, a);
            out.push(')');
            if *h == H::ShiftImmValue {
                out.push_str(".0");
            }
        }
    }
}

fn stmt(out: &mut String, s: &S) {
    match s {
        S::Let(t, e) => {
            let _ = write!(out, "let {} = ", t.name());
            expr(out, e);
            out.push(';');
        }
        S::Let2(a, x, e) => {
            let _ = write!(out, "let ({}, {}) = ", a.name(), x.name());
            expr(out, e);
            out.push(';');
        }
        S::SetReg(i, E::Const(v)) if LOCALS.with(|l| l.get()) => {
            reg(out, *i);
            let _ = write!(out, " = {v:#X};");
        }
        S::SetReg(i, e) => {
            reg(out, *i);
            out.push_str(" = ");
            expr(out, e);
            out.push(';');
        }
        S::SetFlag(f, e) => {
            let _ = write!(out, "{} = ", flag(*f));
            expr(out, e);
            out.push(';');
        }
        S::SetNz(e) => {
            out.push_str("R::set_nz(c, ");
            expr(out, e);
            out.push_str(");");
        }
        S::Store { w, addr, val } if LOCALS.with(|l| l.get()) && unmask(addr, *w).is_some() => {
            let acc = match w {
                Width::B8 => Access::St8,
                Width::B16 => Access::St16,
                Width::B32 => Access::St32,
            };
            if field_access(out, unmask(addr, *w).unwrap(), acc) {
                out.push_str(", ");
                expr(out, val);
                out.push_str(");");
                return;
            }
            let name = match w {
                Width::B8 => "st8",
                Width::B16 => "st16",
                Width::B32 => "st32",
            };
            let _ = write!(out, "R::{name}(m, ");
            expr(out, unmask(addr, *w).unwrap());
            out.push_str(", ");
            expr(out, val);
            out.push_str(");");
        }
        S::Store { w, addr, val } => {
            out.push_str(match w {
                Width::B8 => "m.w8(",
                Width::B16 => "m.w16(",
                Width::B32 => "m.w32(",
            });
            expr(out, addr);
            out.push_str(", ");
            match w {
                Width::B8 => {
                    sub(out, val, 9);
                    out.push_str(" as u8");
                }
                Width::B16 => {
                    sub(out, val, 9);
                    out.push_str(" as u16");
                }
                Width::B32 => expr(out, val),
            }
            out.push_str(");");
        }
        S::Do(e) => {
            expr(out, e);
            out.push(';');
        }
        S::Goto(e) => {
            out.push_str("pc = ");
            expr(out, e);
            out.push_str("; continue;");
        }
        S::ExcReturn(e) => {
            let mut v = String::new();
            sub(&mut v, e, 4);
            let _ = write!(out, "let th = c.restore_spsr(); pc = if th {{ {v} | 1 }} else {{ {v} & !3 }}; continue;");
        }
        S::Call { target, ret } => {
            out.push_str("let x = m.call(c, ");
            expr(out, target);
            let _ = write!(out, ", {ret:#X}u32); if x != {ret:#X}u32 {{ pc = x; continue; }}");
        }
        S::IfSome(e, body) => {
            out.push_str("if let Some(v) = ");
            expr(out, e);
            out.push_str(" { ");
            out.push_str(&print(body));
            out.push_str(" }");
        }
    }
}

/// The T0 Rust text of a statement list (one instruction's body, without its condition).
pub fn print(stmts: &[S]) -> String {
    let mut out = String::new();
    for (i, s) in stmts.iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        stmt(&mut out, s);
    }
    out
}

/// One lowered instruction.
#[derive(Clone, Debug)]
pub struct Ins {
    /// Code address (bit 0 = Thumb).
    pub addr: u32,
    /// Condition code (14 = always).
    pub cond: u8,
    pub stmts: Vec<S>,
    /// T1: the condition as an expression replacing `cond` (a folded compare).
    pub guard: Option<E>,
}

/// The lowered instructions of the block starting at `start`.
pub fn lower_block(f: &crate::lift::Func, start: u32, img: Option<&Image>) -> Vec<Ins> {
    f.block(start)
        .into_iter()
        .map(|(a, d)| {
            let next = crate::lift::Func::next(a, &d);
            Ins { addr: a, cond: d.cond, stmts: lower(&d, a & !1, next & !1, img), guard: None }
        })
        .collect()
}

/// Collapses whitespace runs to one space (the T0 contract compares text up to whitespace).
pub fn normalize(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

// ---- temp folding (T1) -----------------------------------------------------------------------

/// True if evaluating `e` has no effect besides its value (no helper with effects, no memory read).
pub fn is_pure(e: &E) -> bool {
    match e {
        E::Call(H::ShiftImmValue | H::ShiftReg, a) => a.iter().all(is_pure),
        E::Call(..) | E::Load { .. } => false,
        E::Bin(_, a, b) | E::Cmp(_, _, a, b) => is_pure(a) && is_pure(b),
        E::Not(a) | E::Shl(a, _) | E::Lsr(a, _) | E::Asr(a, _) | E::Ror(a, _) | E::Rrx(a) | E::Bit(a, _) | E::Msb(a) | E::Lsb(a) | E::BoolU32(a) | E::Clz(a) | E::Iw(a) => is_pure(a),
        _ => true,
    }
}

fn expr_children(e: &mut E) -> Vec<&mut E> {
    match e {
        E::Bin(_, a, b) | E::Cmp(_, _, a, b) => vec![&mut **a, &mut **b],
        E::Not(a) | E::Shl(a, _) | E::Lsr(a, _) | E::Asr(a, _) | E::Ror(a, _) | E::Rrx(a) | E::Bit(a, _) | E::Msb(a) | E::Lsb(a) | E::BoolU32(a) | E::Clz(a) | E::Iw(a) => vec![&mut **a],
        E::Load { addr, .. } => vec![&mut **addr],
        E::Call(_, a) => a.iter_mut().collect(),
        _ => vec![],
    }
}

/// Occurrences of `t` in `e`; with `with`, replaces them.
fn subst_expr(e: &mut E, t: Tmp, with: Option<&E>) -> usize {
    if *e == E::Tmp(t) {
        if let Some(w) = with {
            *e = w.clone();
        }
        return 1;
    }
    expr_children(e).into_iter().map(|c| subst_expr(c, t, with)).sum()
}

fn stmt_exprs(s: &mut S) -> Vec<&mut E> {
    match s {
        S::Let(_, e) | S::Let2(_, _, e) | S::SetReg(_, e) | S::SetFlag(_, e) | S::SetNz(e) | S::Do(e) | S::Goto(e) | S::ExcReturn(e) => vec![e],
        S::Store { addr, val, .. } => vec![addr, val],
        S::Call { target, .. } => vec![target],
        S::IfSome(e, body) => {
            let mut v = vec![e];
            for b in body.iter_mut() {
                v.extend(stmt_exprs(b));
            }
            v
        }
    }
}

fn binds(s: &S, t: Tmp) -> bool {
    match s {
        S::Let(a, _) => *a == t,
        S::Let2(a, b, _) => *a == t || *b == t,
        _ => false,
    }
}

/// Uses of `t` in `stmts` until it is bound again.
fn uses(stmts: &mut [S], t: Tmp) -> usize {
    let mut n = 0;
    for s in stmts.iter_mut() {
        n += stmt_exprs(s).into_iter().map(|e| subst_expr(e, t, None)).sum::<usize>();
        if binds(s, t) {
            break;
        }
    }
    n
}

/// Folds single-use temporaries into the statement that uses them and drops unused pure ones, so
/// `let res = a.wrapping_add(b); c.r[3] = res;` reads `r3 = a.wrapping_add(b);`. Evaluation order is
/// kept: an expression with effects only moves into the very next statement, and only when that
/// statement is just a copy of it.
pub fn fold_temps(stmts: &mut Vec<S>) {
    loop {
        let mut changed = false;
        let mut k = 0;
        while k < stmts.len() {
            let S::Let(t, e) = stmts[k].clone() else {
                k += 1;
                continue;
            };
            let n = uses(&mut stmts[k + 1..], t);
            if n == 0 {
                if is_pure(&e) {
                    stmts.remove(k);
                } else {
                    stmts[k] = S::Do(e);
                    k += 1;
                }
                changed = true;
                continue;
            }
            if n == 1 && k + 1 < stmts.len() {
                let next_uses = stmt_exprs(&mut stmts[k + 1]).into_iter().map(|x| subst_expr(x, t, None)).sum::<usize>();
                let copy = matches!(&stmts[k + 1], S::SetReg(_, E::Tmp(u)) | S::Let(_, E::Tmp(u)) if *u == t);
                // a pure value may move into any position of the next statement: nothing in a
                // statement writes registers or flags before its operands are evaluated
                if next_uses == 1 && (copy || is_pure(&e)) && !matches!(stmts[k + 1], S::IfSome(..)) {
                    for x in stmt_exprs(&mut stmts[k + 1]) {
                        subst_expr(x, t, Some(&e));
                    }
                    stmts.remove(k);
                    changed = true;
                    continue;
                }
            }
            k += 1;
        }
        if !changed {
            break;
        }
    }
}

/// The address without the alignment mask a `w`-wide access applies (`a & !3` -> `a`), if it has it.
fn unmask(addr: &E, w: Width) -> Option<&E> {
    let k = match w {
        Width::B8 => return Some(addr),
        Width::B16 => 1,
        Width::B32 => 3,
    };
    match addr {
        E::Bin(Bin::And, a, m) if **m == E::Not(Box::new(E::Lit(k))) => Some(a),
        _ => None,
    }
}
