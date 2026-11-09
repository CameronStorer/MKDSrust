//! Fixed-point math pilot, derived from lifted behavior (not SDK source).
//!
//! The value functions in this module have no CPU or simulated-memory dependency.
//! The `*_block` functions below are the temporary ABI/device boundary. Trial only.
use super::{enter, ret};
use crate::rt::{self, Cpu};

/// Signed 20.12 fixed point; all overflowing arithmetic wraps, as on ARM9.
pub type Fx32 = i32;
pub const FX_ONE: Fx32 = 1 << 12;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(C)]
pub struct VecFx32 {
    pub x: Fx32,
    pub y: Fx32,
    pub z: Fx32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MtxFx22(pub [[Fx32; 2]; 2]);
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MtxFx33(pub [[Fx32; 3]; 3]);
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MtxFx43(pub [[Fx32; 3]; 4]);
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MtxFx44(pub [[Fx32; 4]; 4]);

/// The minimum signed value stays negative (two's-complement wrapping absolute).
#[inline]
pub fn fx_abs(value: Fx32) -> Fx32 {
    value.wrapping_abs()
}

/// Arithmetic truncation, toward negative infinity before narrowing to 32 bits.
#[inline]
pub fn fx_mul_trunc(a: Fx32, b: Fx32) -> Fx32 {
    ((a as i64 * b as i64) >> 12) as Fx32
}

/// Add half a fixed-point unit before shifting; ties go toward positive infinity.
#[inline]
pub fn fx_mul(a: Fx32, b: Fx32) -> Fx32 {
    ((a as i64 * b as i64).wrapping_add(0x800) >> 12) as Fx32
}

#[inline]
fn dot_wide(a: VecFx32, b: VecFx32) -> i64 {
    (a.x as i64 * b.x as i64)
        .wrapping_add(a.y as i64 * b.y as i64)
        .wrapping_add(a.z as i64 * b.z as i64)
}

#[inline]
pub fn vec_dot(a: VecFx32, b: VecFx32) -> Fx32 {
    (dot_wide(a, b).wrapping_add(0x800) >> 12) as Fx32
}

#[inline]
pub fn vec_dot_trunc(a: VecFx32, b: VecFx32) -> Fx32 {
    (dot_wide(a, b) >> 12) as Fx32
}

#[inline]
pub fn vec_cross(a: VecFx32, b: VecFx32) -> VecFx32 {
    let component = |a: Fx32, b: Fx32, c: Fx32, d: Fx32| {
        (a as i64 * b as i64).wrapping_sub(c as i64 * d as i64) >> 12
    };
    VecFx32 {
        x: component(a.y, b.z, a.z, b.y) as Fx32,
        y: component(a.z, b.x, a.x, b.z) as Fx32,
        z: component(a.x, b.y, a.y, b.x) as Fx32,
    }
}

/// Unsigned floor square root, including u64::MAX. Integer arithmetic only.
/// This is the result of the DS sqrt unit in 64-bit mode.
pub fn sqrt_u64(mut value: u64) -> u32 {
    let mut root = 0u64;
    let mut bit = 1u64 << 62;
    while bit > value {
        bit >>= 2;
    }
    while bit != 0 {
        if value >= root + bit {
            value -= root + bit;
            root = (root >> 1) + bit;
        } else {
            root >>= 1;
        }
        bit >>= 2;
    }
    root as u32
}

#[inline]
fn distance_squared(a: VecFx32, b: VecFx32) -> u64 {
    let d = VecFx32 {
        x: a.x.wrapping_sub(b.x),
        y: a.y.wrapping_sub(b.y),
        z: a.z.wrapping_sub(b.z),
    };
    dot_wide(d, d) as u64
}

/// Matches the leaf's signed final shift, even outside ordinary coordinate ranges.
pub fn vec_distance(a: VecFx32, b: VecFx32) -> Fx32 {
    (sqrt_u64(distance_squared(a, b).wrapping_shl(2)).wrapping_add(1) as i32) >> 1
}

/// Full DS mode-0 divider result. The quotient's upper word is inverted on overflow.
/// GBATEK: https://problemkaputt.de/gbatek.htm#dsarithmeticunits
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Division {
    pub quotient: i64,
    pub remainder: i64,
}

pub fn div_s32(numerator: i32, denominator: i32) -> Division {
    if denominator == 0 {
        let low = if numerator < 0 { 1u32 } else { u32::MAX };
        Division {
            quotient: (((!low as u64) << 32) | low as u64) as i64,
            remainder: numerator as i64,
        }
    } else {
        // Widen first: MIN / -1 produces the hardware's positive 64-bit quotient.
        Division {
            quotient: numerator as i64 / denominator as i64,
            remainder: numerator as i64 % denominator as i64,
        }
    }
}

fn identity<const ROWS: usize, const COLS: usize>() -> [[Fx32; COLS]; ROWS] {
    let mut result = [[0; COLS]; ROWS];
    for (row, values) in result.iter_mut().enumerate() {
        if row < COLS {
            values[row] = FX_ONE;
        }
    }
    result
}
pub fn mtx_identity22() -> MtxFx22 {
    MtxFx22(identity())
}
pub fn mtx_identity33() -> MtxFx33 {
    MtxFx33(identity())
}
pub fn mtx_identity43() -> MtxFx43 {
    MtxFx43(identity())
}
pub fn mtx_identity44() -> MtxFx44 {
    MtxFx44(identity())
}

// ABI adapters. Scratch registers/flags are observable to the current shadow checker;
// these details deliberately stay out of the value functions above.
fn save(c: &Cpu, m: &mut dyn rt::Bus, regs: &[usize]) {
    let base = c.r[13].wrapping_sub(4 * regs.len() as u32);
    for (i, &r) in regs.iter().enumerate() {
        rt::str_(m, base.wrapping_add(i as u32 * 4), c.r[r]);
    }
}
fn load_vec(m: &mut dyn rt::Bus, address: u32) -> VecFx32 {
    VecFx32 {
        x: rt::ldr(m, address) as i32,
        y: rt::ldr(m, address.wrapping_add(4)) as i32,
        z: rt::ldr(m, address.wrapping_add(8)) as i32,
    }
}

pub fn abs_block(c: &mut Cpu, _: &mut dyn rt::Bus, pc: u32) -> u32 {
    if !enter(c, pc, 3) {
        return rt::BUDGET_EXHAUSTED;
    }
    rt::subs(c, c.r[0], 0);
    c.r[0] = fx_abs(c.r[0] as i32) as u32;
    ret(c)
}

pub fn mul_trunc_block(c: &mut Cpu, _: &mut dyn rt::Bus, pc: u32) -> u32 {
    if !enter(c, pc, 4) {
        return rt::BUDGET_EXHAUSTED;
    }
    let (a, b) = (c.r[0] as i32, c.r[1] as i32);
    let product = a as i64 * b as i64;
    c.r[2] = product as u32;
    c.r[1] = (product >> 32) as u32;
    c.r[0] = fx_mul_trunc(a, b) as u32;
    ret(c)
}

pub fn mul_block(c: &mut Cpu, _: &mut dyn rt::Bus, pc: u32) -> u32 {
    if !enter(c, pc, 7) {
        return rt::BUDGET_EXHAUSTED;
    }
    let (a, b) = (c.r[0] as i32, c.r[1] as i32);
    let product = a as i64 * b as i64;
    c.r[2] = product as u32;
    rt::adds(c, c.r[2], 0x800);
    c.r[1] = (product.wrapping_add(0x800) >> 32) as u32;
    c.r[0] = fx_mul(a, b) as u32;
    ret(c)
}

fn dot_block(c: &mut Cpu, m: &mut dyn rt::Bus, pc: u32, round: bool) -> u32 {
    if !enter(c, pc, if round { 17 } else { 14 }) {
        return rt::BUDGET_EXHAUSTED;
    }
    save(c, m, &[4, 14]);
    let a = load_vec(m, c.r[0]);
    let b = load_vec(m, c.r[1]);
    c.r[2] = b.x as u32;
    c.r[3] = a.z as u32;
    c.r[12] = a.x as u32;
    if round {
        let sum = dot_wide(a, b);
        rt::adds(c, sum as u32, 0x800);
        c.r[1] = (sum.wrapping_add(0x800) >> 32) as u32;
        c.r[0] = vec_dot(a, b) as u32;
    } else {
        c.r[0] = vec_dot_trunc(a, b) as u32;
    }
    ret(c)
}
pub fn dot_round_block(c: &mut Cpu, m: &mut dyn rt::Bus, pc: u32) -> u32 {
    dot_block(c, m, pc, true)
}
pub fn dot_trunc_block(c: &mut Cpu, m: &mut dyn rt::Bus, pc: u32) -> u32 {
    dot_block(c, m, pc, false)
}

pub fn cross_block(c: &mut Cpu, m: &mut dyn rt::Bus, pc: u32) -> u32 {
    if !enter(c, pc, 35) {
        return rt::BUDGET_EXHAUSTED;
    }
    save(c, m, &[4, 5, 6, 7, 8, 9, 14]);
    let a = load_vec(m, c.r[0]);
    let b = load_vec(m, c.r[1]);
    let result = vec_cross(a, b);
    let z_positive = a.x as i64 * b.y as i64;
    let z_negative = a.y as i64 * b.x as i64;
    rt::subs(c, z_positive as u32, z_negative as u32);
    c.r[0] = (z_positive.wrapping_sub(z_negative) >> 32) as u32;
    c.r[1] = result.z as u32;
    c.r[3] = result.y as u32;
    c.r[12] = result.x as u32;
    for (offset, value) in [(0, result.x), (4, result.y), (8, result.z)] {
        rt::str_(m, c.r[2].wrapping_add(offset), value as u32);
    }
    ret(c)
}

pub fn distance_block(c: &mut Cpu, m: &mut dyn rt::Bus, pc: u32) -> u32 {
    if !enter(c, pc, 31) {
        return rt::BUDGET_EXHAUSTED;
    }
    save(c, m, &[4, 14]);
    let a = load_vec(m, c.r[0]);
    let b = load_vec(m, c.r[1]);
    let operand = distance_squared(a, b).wrapping_shl(2);
    m.w16(0x0400_02b0, 1);
    m.w32(0x0400_02b8, operand as u32);
    m.w32(0x0400_02bc, (operand >> 32) as u32);
    while m.r16(0x0400_02b0) & 0x8000 != 0 {}
    // Keep the observable device read; arithmetic itself is computed in Rust.
    let _ = m.r32(0x0400_02b4);
    c.n = false;
    c.z = true;
    c.c = false;
    c.r[0] = vec_distance(a, b) as u32;
    c.r[1] = (operand >> 32) as u32;
    c.r[2] = operand as u32;
    c.r[3] = 0x0400_02b0;
    // The original retains the low word of the unshifted squared sum.
    c.r[12] = distance_squared(a, b) as u32;
    ret(c)
}

fn divide_block(c: &mut Cpu, m: &mut dyn rt::Bus, pc: u32, remainder: bool) -> u32 {
    if !enter(c, pc, 20) {
        return rt::BUDGET_EXHAUSTED;
    }
    save(c, m, &[14]);
    let result = div_s32(c.r[0] as i32, c.r[1] as i32);
    m.w16(0x0400_0280, 0);
    m.w32(0x0400_0290, c.r[0]);
    m.w32(0x0400_0298, c.r[1]);
    m.w32(0x0400_029c, 0);
    while m.r16(0x0400_0280) & 0x8000 != 0 {}
    let _ = m.r32(if remainder { 0x0400_02a8 } else { 0x0400_02a0 });
    c.n = false;
    c.z = true;
    c.c = false;
    c.r[0] = if remainder {
        result.remainder
    } else {
        result.quotient
    } as u32;
    c.r[2] = 0x0400_0298;
    c.r[3] = 0x0400_0290;
    c.r[12] = 0x0400_0280;
    ret(c)
}
pub fn div_s32_block(c: &mut Cpu, m: &mut dyn rt::Bus, pc: u32) -> u32 {
    divide_block(c, m, pc, false)
}
pub fn mod_block(c: &mut Cpu, m: &mut dyn rt::Bus, pc: u32) -> u32 {
    divide_block(c, m, pc, true)
}

fn store_matrix<const R: usize, const C: usize>(
    m: &mut dyn rt::Bus,
    address: u32,
    values: [[Fx32; C]; R],
) {
    for (i, &value) in values.iter().flatten().enumerate() {
        rt::str_(m, address.wrapping_add(i as u32 * 4), value as u32);
    }
}
pub fn identity22_block(c: &mut Cpu, m: &mut dyn rt::Bus, pc: u32) -> u32 {
    if !enter(c, pc, 6) {
        return rt::BUDGET_EXHAUSTED;
    }
    store_matrix(m, c.r[0], mtx_identity22().0);
    c.r[0] = c.r[0].wrapping_add(16);
    c.r[1] = 0;
    c.r[2] = FX_ONE as u32;
    c.r[3] = 0;
    ret(c)
}
pub fn identity33_block(c: &mut Cpu, m: &mut dyn rt::Bus, pc: u32) -> u32 {
    if !enter(c, pc, 9) {
        return rt::BUDGET_EXHAUSTED;
    }
    let matrix = mtx_identity33();
    rt::str_(m, c.r[0].wrapping_add(32), matrix.0[2][2] as u32);
    for (i, &value) in matrix.0.iter().flatten().take(8).enumerate() {
        rt::str_(m, c.r[0].wrapping_add(i as u32 * 4), value as u32);
    }
    c.r[0] = c.r[0].wrapping_add(32);
    c.r[1] = 0;
    c.r[2] = FX_ONE as u32;
    c.r[3] = 0;
    ret(c)
}
pub fn identity43_block(c: &mut Cpu, m: &mut dyn rt::Bus, pc: u32) -> u32 {
    if !enter(c, pc, 10) {
        return rt::BUDGET_EXHAUSTED;
    }
    store_matrix(m, c.r[0], mtx_identity43().0);
    c.r[0] = c.r[0].wrapping_add(48);
    c.r[1] = 0;
    c.r[2] = FX_ONE as u32;
    c.r[3] = 0;
    ret(c)
}
pub fn identity44_block(c: &mut Cpu, m: &mut dyn rt::Bus, pc: u32) -> u32 {
    if !enter(c, pc, 11) {
        return rt::BUDGET_EXHAUSTED;
    }
    store_matrix(m, c.r[0], mtx_identity44().0);
    c.r[0] = c.r[0].wrapping_add(64);
    c.r[1] = 0;
    c.r[2] = FX_ONE as u32;
    c.r[3] = 0;
    ret(c)
}

/// AMCE revision-0 entries covered by this pilot (including the existing trial divider leaves).
pub const ENTRIES: &[u32] = &[
    0x020738f8, 0x02091f44, 0x021488e8, 0x02039828, 0x0214848c, 0x020397a8, 0x02147f50, 0x02146c98,
    0x02146cbc, 0x021471bc, 0x02147954, 0x02147cd8, 0x02147d30,
];

/// The divider leaves were already trial natives before this pilot. Only the
/// additional entries must be gated when choosing the generator's direct calls.
pub fn is_new_entry(entry: u32) -> bool {
    ENTRIES.contains(&entry) && !matches!(entry, 0x0214_7cd8 | 0x0214_7d30)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integer_sqrt_full_range() {
        let mut seed = 0x7519_318a_917d_832du64;
        for i in 0..4096 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let value = if i < 4 {
                [0, 1, u64::MAX, 1 << 63][i]
            } else {
                seed
            };
            let root = sqrt_u64(value) as u128;
            assert!(root * root <= value as u128);
            assert!((root + 1) * (root + 1) > value as u128);
        }
    }

    #[test]
    fn divider_full_result_matches_hardware() {
        use crate::rt::Mem;
        let mut hw = crate::ds_mem::Bus::new();
        let edges = [i32::MIN, i32::MAX, -1, 0, 1, 4096, -4096];
        for n in edges {
            for d in edges {
                hw.w16(0x0400_0280, 0);
                hw.w32(0x0400_0290, n as u32);
                hw.w32(0x0400_0298, d as u32);
                hw.w32(0x0400_029c, 0);
                let pair = |hw: &mut crate::ds_mem::Bus, address| {
                    (hw.r32(address) as u64 | ((hw.r32(address + 4) as u64) << 32)) as i64
                };
                let actual = div_s32(n, d);
                assert_eq!(actual.quotient, pair(&mut hw, 0x0400_02a0), "{n}/{d}");
                assert_eq!(actual.remainder, pair(&mut hw, 0x0400_02a8), "{n}%{d}");
            }
        }
    }

    #[test]
    fn fixed_point_rounding_boundaries() {
        assert_eq!(fx_abs(i32::MIN), i32::MIN);
        assert_eq!(fx_mul(1, 2048), 1);
        assert_eq!(fx_mul(-1, 2048), 0);
        assert_eq!(fx_mul_trunc(-1, 2048), -1);
        let a = VecFx32 {
            x: i32::MIN,
            y: i32::MIN,
            z: i32::MIN,
        };
        assert_eq!(vec_dot(a, a), 0); // overflowing 64-bit sum, then narrow
    }
}
