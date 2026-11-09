//! Plain arithmetic printed only for opt-in native direct variants.
use crate::ir::H;

/// Inline helpers evaluate their operands once, before updating any flag. Q-setting
/// multiplies deliberately keep the runtime helper: Q is sticky and observable in CPSR.
pub fn helper(h: H, a: &[String]) -> Option<String> {
    let text = match h {
        H::Adds | H::Adcs | H::Subs | H::Sbcs => {
            let (carry, operation, overflow) = match h {
                H::Adds => ("0u32", "overflowing_add", "((!(__flag_a ^ __flag_b) & (__flag_a ^ __flag_result)) >> 31) != 0"),
                H::Adcs => ("flag_c as u32", "overflowing_add", "((!(__flag_a ^ __flag_b) & (__flag_a ^ __flag_result)) >> 31) != 0"),
                H::Subs => ("0u32", "overflowing_sub", "(((__flag_a ^ __flag_b) & (__flag_a ^ __flag_result)) >> 31) != 0"),
                _ => ("(!flag_c) as u32", "overflowing_sub", "(((__flag_a ^ __flag_b) & (__flag_a ^ __flag_result)) >> 31) != 0"),
            };
            let cy = if matches!(h, H::Subs | H::Sbcs) { "!(__flag_c1 || __flag_c2)" } else { "__flag_c1 || __flag_c2" };
            format!("{{ let (__flag_a, __flag_b): (u32, u32) = ({}, {}); let __flag_carry = {carry}; let (__flag_first, __flag_c1) = __flag_a.{operation}(__flag_b); let (__flag_result, __flag_c2) = __flag_first.{operation}(__flag_carry); flag_n = __flag_result >> 31 != 0; flag_z = __flag_result == 0; flag_c = {cy}; flag_v = {overflow}; __flag_result }}", a[1], a[2])
        }
        H::MulLong => format!("{{ let (__flag_rm, __flag_rs, __flag_lo, __flag_hi): (u32, u32, u32, u32) = ({}, {}, {}, {}); let mut __flag_product = if {} {{ (__flag_rm as i32 as i64).wrapping_mul(__flag_rs as i32 as i64) as u64 }} else {{ (__flag_rm as u64) * (__flag_rs as u64) }}; if {} {{ __flag_product = __flag_product.wrapping_add(((__flag_hi as u64) << 32) | __flag_lo as u64); }} if {} {{ flag_n = __flag_product >> 63 != 0; flag_z = __flag_product == 0; }} (__flag_product as u32, (__flag_product >> 32) as u32) }}", a[4], a[5], a[6], a[7], a[1], a[2], a[3]),
        H::HalfMul if a[1] == "recomp::arm::HalfMul::Smul" || a[1] == "recomp::arm::HalfMul::Smulw" => {
            let half = |v: &str, top: &str| format!("(if {top} {{ ({v} >> 16) as i16 as i32 }} else {{ {v} as i16 as i32 }})");
            let rs = half("__flag_rs", &a[3]);
            let product = if a[1].ends_with("::Smul") { format!("({} * {rs}) as u32", half("__flag_rm", &a[2])) }
                else { format!("(((__flag_rm as i32 as i64) * ({rs} as i64)) >> 16) as u32") };
            format!("{{ let (__flag_rm, __flag_rs): (u32, u32) = ({}, {}); {product} }}", a[4], a[5])
        }
        _ => return None,
    };
    Some(text)
}

pub fn nz(value: &str) -> String {
    format!("{{ let __flag_result: u32 = {value}; flag_n = __flag_result >> 31 != 0; flag_z = __flag_result == 0; }}")
}

