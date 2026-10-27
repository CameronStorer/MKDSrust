//! Native functions (block 3, `4_docs/BLOCK3_PLAN.md`): readable Rust that replaces a whole guest
//! function once the function dictionary knows what it does. Contains no game code or data: each
//! entry is our own implementation, found by the address of the function it replaces.
//!
//! Contract for every native function:
//! - It has the shape of a lifted block ([`LiftedFn`]) and is installed by the [`Dispatcher`] at the
//!   function's entry block of its segment (overlays share addresses, so the segment matters). It
//!   runs when execution reaches that entry, the way the lifted code would.
//! - It does the function's whole job: reads arguments from r0-r3 / the stack, works on DS memory
//!   through the bus (an array; DS addresses stay DS addresses), sets the results, and returns where
//!   the original's `bx lr` would go ([`ret`]). Registers the original leaves changed must end up the
//!   same (the shadow checker compares r0-r3, r12, sp, lr and the flags); stack bytes below the entry
//!   sp are dead and not compared.
//! - It pays its instruction budget first ([`enter`]), so time keeps moving and a slice can end.
//! - It is a leaf: it may not run other guest code. In the game, `Bus::call` does not run the callee
//!   (calls continue in the dispatch loop), so functions that call others are not native yet.
//!
//! [`Dispatcher`]: crate::dispatch::Dispatcher
use crate::mem::LiftedFn;
use crate::rt::{self, Cpu};

/// One native function: the segment and entry (bit 0 = Thumb) of the function it replaces.
pub struct Native {
    pub segment: &'static str,
    pub entry: u32,
    pub name: &'static str,
    pub f: LiftedFn,
}

/// Charges `cost` instructions; `false` means the slice is out of budget and the caller must return
/// [`rt::BUDGET_EXHAUSTED`] (the dispatch loop comes back to the same entry later).
#[inline]
pub fn enter(c: &mut Cpu, pc: u32, cost: u32) -> bool {
    if rt::spend(c, cost) {
        return true;
    }
    c.r[15] = pc;
    false
}

/// The pc after the function returns (`bx lr`).
#[inline]
pub fn ret(c: &Cpu) -> u32 {
    crate::arm::interwork(c.r[14])
}

/// Nintendo SDK / C library leaves (lane B2), each differential-tested against the ROM's code
/// (`tests/native_sdk.rs`).
mod sdk;

/// Block 5.1: ROM-archive file reads from the in-memory ROM (`MKDS_HW_FS=native`).
pub mod fs;

/// Every native function (on trial until it passes the shadow checker), in no particular order.
pub static NATIVE: &[Native] = &[
    Native { segment: "arm9_main", entry: 0x0200_fe74, name: "MATH_CountLeadingZeros", f: sdk::count_leading_zeros_block },
    Native { segment: "arm9_main", entry: 0x0213_3048, name: "msl_multiply_u64", f: sdk::multiply64_block },
    Native { segment: "arm9_main", entry: 0x0213_a434, name: "strlen", f: sdk::strlen_block },
    Native { segment: "arm9_main", entry: 0x0214_cfe4, name: "MIi_CpuClear16", f: sdk::fill16_block },
    Native { segment: "arm9_main", entry: 0x0214_cffc, name: "MIi_CpuCopy16", f: sdk::copy16_block },
    Native { segment: "arm9_main", entry: 0x0214_d02c, name: "MIi_CpuCopy32", f: sdk::copy32_block },
    Native { segment: "arm9_main", entry: 0x0214_d05c, name: "MIi_CpuClearFast", f: sdk::fill_fast_block },
    Native { segment: "arm9_main", entry: 0x0214_d168, name: "MI_CpuFill8", f: sdk::fill8_block },
    Native { segment: "arm9_main", entry: 0x0214_7cd8, name: "FX_Mod", f: sdk::remainder_block },
    Native { segment: "arm9_main", entry: 0x0214_7d30, name: "FX_DivS32", f: sdk::divide_block },
    Native { segment: "arm9_main", entry: 0x0214_7d88, name: "FX_DivAsync", f: sdk::div_async_block },
    Native { segment: "arm9_main", entry: 0x0214_7dc0, name: "FX_GetSqrtResult", f: sdk::sqrt_result_block },
    Native { segment: "arm9_main", entry: 0x0214_7e28, name: "FX_GetDivResult", f: sdk::div_result_block },
];

/// The natives that passed the shadow checker over whole recorded races (`native-verified` in the
/// dictionary): what the game installs. Evidence (2026-10-16, GP race to the finish, `MKDS_SHADOW=1`):
/// calls without a mismatch in brackets; natives with fewer than 50 calls stay on trial.
pub static VERIFIED: &[Native] = &[
    Native { segment: "arm9_main", entry: 0x0200_fe74, name: "MATH_CountLeadingZeros", f: sdk::count_leading_zeros_block }, // [91]
    Native { segment: "arm9_main", entry: 0x0213_3048, name: "msl_multiply_u64", f: sdk::multiply64_block }, // [21,538]
    Native { segment: "arm9_main", entry: 0x0213_a434, name: "strlen", f: sdk::strlen_block }, // [763]
    Native { segment: "arm9_main", entry: 0x0214_cfe4, name: "MIi_CpuClear16", f: sdk::fill16_block }, // [89]
    Native { segment: "arm9_main", entry: 0x0214_d05c, name: "MIi_CpuClearFast", f: sdk::fill_fast_block }, // [74,473]
    Native { segment: "arm9_main", entry: 0x0214_d168, name: "MI_CpuFill8", f: sdk::fill8_block }, // [120]
    // second race, with memory-like 3D table writes compared
    Native { segment: "arm9_main", entry: 0x0214_cffc, name: "MIi_CpuCopy16", f: sdk::copy16_block }, // [12,448]
    Native { segment: "arm9_main", entry: 0x0214_7d88, name: "FX_DivAsync", f: sdk::div_async_block }, // [842,552]
    Native { segment: "arm9_main", entry: 0x0214_7dc0, name: "FX_GetSqrtResult", f: sdk::sqrt_result_block }, // [168,974]
    Native { segment: "arm9_main", entry: 0x0214_7e28, name: "FX_GetDivResult", f: sdk::div_result_block }, // [852,381]
];

/// What a game or tool installs: [`VERIFIED`], or for diagnostics per `MKDS_NATIVES`: `off` = none (the
/// exact translation everywhere), `all` = every [`NATIVE`] including those on trial.
pub fn selected() -> &'static [Native] {
    let base = match std::env::var("MKDS_NATIVES").as_deref() {
        Ok("off") => &[],
        Ok("all") => NATIVE,
        _ => VERIFIED,
    };
    if !fs::enabled() {
        return base;
    }
    // Block 5 hardware services (`MKDS_HW_<UNIT>=native`) come on top of the selection.
    let all: Vec<Native> = base
        .iter()
        .chain(fs::NATIVES)
        .map(|n| Native { segment: n.segment, entry: n.entry, name: n.name, f: n.f })
        .collect();
    Box::leak(all.into_boxed_slice())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dispatch::Dispatcher;
    use crate::mem::Lifted;
    use crate::rt::{Bus, BUDGET_EXHAUSTED};

    fn lifted_double(c: &mut Cpu, _m: &mut dyn Bus, pc: u32) -> u32 {
        if pc == 0x0200_0000 {
            c.r[0] = c.r[0].wrapping_mul(2);
            return 0x0200_0010; // a second block of the same function
        }
        c.r[1] = 0xdead;
        ret(c)
    }
    fn native_double(c: &mut Cpu, _m: &mut dyn Bus, pc: u32) -> u32 {
        if !enter(c, pc, 3) {
            return BUDGET_EXHAUSTED;
        }
        c.r[0] = c.r[0].wrapping_mul(2) | 1;
        ret(c)
    }
    static TABLE: &[Lifted] = &[Lifted { segment: "arm9_main", entry: 0x0200_0000, blocks: &[0x0200_0000, 0x0200_0010], insts: &[], f: lifted_double }];
    static NAT: &[Native] = &[
        Native { segment: "arm9_main", entry: 0x0200_0000, name: "double", f: native_double },
        Native { segment: "ov9_000", entry: 0x0200_0000, name: "elsewhere", f: native_double },
    ];

    #[test]
    fn natives_replace_only_the_entry_of_their_own_segment() {
        let d = Dispatcher::new(TABLE);
        let mut m = crate::mem::Ram::new(&[], 0, 1);
        let mut c = Cpu { budget: 10, ..Cpu::default() };
        c.r[0] = 5;
        c.r[14] = 0x0200_1000;
        assert_eq!(d.get(0x0200_0000).unwrap()(&mut c, &mut m, 0x0200_0000), 0x0200_0010, "lifted before");
        d.set_natives(NAT);
        c.r[0] = 5;
        assert_eq!(d.get(0x0200_0000).unwrap()(&mut c, &mut m, 0x0200_0000), 0x0200_1000);
        assert_eq!((c.r[0], c.budget), (11, 7), "native ran and paid its cost");
        assert!(d.get(0x0200_0010).is_some(), "the function's other blocks stay lifted");
        c.budget = 0;
        assert_eq!(d.get(0x0200_0000).unwrap()(&mut c, &mut m, 0x0200_0000), BUDGET_EXHAUSTED);
        assert_eq!(c.r[15], 0x0200_0000, "resumes at the entry");
        d.set_natives(&[]);
        assert_eq!(d.get(0x0200_0000).unwrap() as usize, lifted_double as LiftedFn as usize, "removable");
    }
}
