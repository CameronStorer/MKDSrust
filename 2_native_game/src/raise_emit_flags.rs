//! Arithmetic flag locals for closed native functions (MKDS_FLAG_LOCALS=0 opts out). The ordinary T0/T1 printer
//! is unchanged. Calls that still accept a CPU synchronize at that boundary; only flags
//! live outside the function are written at returns.
use super::*;

pub fn enabled() -> bool {
    nospill_enabled() && std::env::var("MKDS_FLAG_LOCALS").map_or(true, |v| v != "0")
}

pub fn uses_cpu(text: &str) -> bool {
    // Only emitter-produced Rust is examined, never input source. Dictionary notes
    // in line comments must not retain a CPU argument merely by mentioning C.
    text.lines().any(|line| line.split_once("//").map_or(line, |(code, _)| code)
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_').any(|word| word == "c"))
}

pub fn call_count(text: &str) -> usize {
    ["subs", "set_nz", "adds", "adcs", "sbcs", "mul_long", "half_mul"]
        .iter().map(|h| text.matches(&format!("R::{h}(c,")).count()).sum()
}

pub fn sync(mask: u8, reload: bool) -> String {
    ["n", "z", "c", "v"].iter().enumerate().filter(|(i, _)| mask & (1 << i) != 0)
        .map(|(_, f)| if reload { format!("flag_{f} = c.{f}; ") } else { format!("c.{f} = flag_{f}; ") }).collect()
}

pub fn init(mask: u8) -> String {
    ["n", "z", "c", "v"].iter().enumerate().map(|(i, f)| {
        format!("let mut flag_{f} = {}; ", if mask & (1 << i) != 0 { format!("c.{f}") } else { "false".into() })
    }).collect()
}

/// Entry reads and observable return writes, after compare folding/materialization.
/// Unwritten return flags stay in the CPU and need neither a local load nor a store.
fn plan(blocks: &BTreeMap<u32, Block>, entry: u32, order: &[u32], live: &BTreeMap<u32, u8>, sf: &dyn Fn(&S, u32) -> (u8, u8), returned: u8, polls: bool) -> (u8, u8) {
    let internal = |t: u32| blocks.contains_key(&((t & !1) | (entry & 1)));
    let exit = |s: &S| match s {
        S::Goto(E::Const(t)) => !internal(*t),
        S::Goto(_) => true,
        S::IfSome(..) => block_return(s).is_some(),
        _ => false,
    };
    let effects = |s: &S| match s {
        S::Goto(E::Const(t)) if !internal(*t) && (direct_for(*t).is_some_and(|d| d.cpu) || boundary_for(*t).is_some()) => (crate::raise::NZCV, crate::raise::NZCV),
        S::Call { target: E::Const(t), .. } if direct_for(*t).is_some_and(|d| !d.cpu) => (0, 0),
        S::Call { .. } => (crate::raise::NZCV, crate::raise::NZCV),
        S::IfSome(..) if block_return(s).is_some() => (0, 0),
        S::Do(e) if plain_block(e).is_some() => (0, 0),
        S::Goto(e) => (crate::raise::expr_reads(e), 0),
        _ => crate::raise::stmt_flags(s),
    };
    let mut written = 0;
    // This contract comes from all callers, not just the direct-call graph. Do
    // not infer it solely from statements that happen to survive rewriting.
    let mut observed = returned;
    for b in blocks.values() {
        for ins in &b.insts {
            for s in &ins.stmts {
                written |= effects(s).1;
                if exit(s) { observed |= live.get(&ins.addr).copied().unwrap_or(crate::raise::NZCV) | sf(s, ins.addr).0; }
                // A tail call that still takes a CPU can observe all incoming flags.
                if let S::Goto(E::Const(t)) = s {
                    if !internal(*t) && (direct_for(*t).is_some_and(|d| d.cpu) || boundary_for(*t).is_some()) { observed |= crate::raise::NZCV; }
                }
            }
        }
    }
    let out = written & observed;
    let mut inn: BTreeMap<u32, u8> = blocks.keys().map(|&b| (b, 0)).collect();
    loop {
        let mut changed = false;
        for &b in order.iter().rev() {
            let blk = &blocks[&b];
            let mut state = blk.succ.iter().fold(0, |v, s| v | inn[s]);
            for ins in blk.insts.iter().rev() {
                let after = state;
                for s in ins.stmts.iter().rev() {
                    let (r, w) = effects(s);
                    if exit(s) { state |= out; }
                    state = (state & !w) | r;
                }
                if let Some(g) = &ins.guard {
                    state |= after | crate::raise::expr_reads(g);
                } else if ins.cond != 14 {
                    state |= after | crate::raise::cond_reads(ins.cond);
                }
            }
            // IRQ entry snapshots the entire CPSR, including flags dead to the
            // ordinary return. Keep their incoming values available at polls.
            if polls { state |= crate::raise::NZCV; }
            if inn[&b] != state { inn.insert(b, state); changed = true; }
        }
        if !changed { return (inn[&entry], out); }
    }
}

pub(super) fn for_function(blocks: &BTreeMap<u32, Block>, entry: u32, order: &[u32], live: &BTreeMap<u32, u8>, sf: &dyn Fn(&S, u32) -> (u8, u8), polls: bool) -> Option<(u8, u8)> {
    // Closed-function selection already rejects external fall-throughs. Keep
    // this check beside the plan too: the emitter's fall-off exit has no S.
    if blocks.values().any(|b| b.falls_to.is_some_and(|t| !blocks.contains_key(&((t & !1) | (entry & 1))))) {
        return None;
    }
    let (returned, computed_target) = crate::raise::direct_flag_observers(entry);
    // Computed callers and wrappers retain the established CPU flag emitter.
    // In particular, any_ret == 0 is not proof that a target has no flag ABI.
    if computed_target { return None; }
    // Mode/status-register operations retain the existing emitter, including bank syncs.
    let status = blocks.values().flat_map(|b| &b.insts).flat_map(|i| &i.stmts).any(|s| {
        matches!(s, S::Do(E::Call(H::Msr, _))) || matches!(s, S::SetReg(_, E::Cpsr | E::Spsr))
    });
    (!status).then(|| plan(blocks, entry, order, live, sf, returned, polls))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fall_off_without_a_statement_is_excluded() {
        let entry = 0x0200_0000;
        let blocks = [(entry, Block { start: entry, insts: vec![], succ: vec![], falls_to: Some(entry + 4), charge: 1 })].into_iter().collect();
        assert_eq!(for_function(&blocks, entry, &[entry], &BTreeMap::new(), &|_, _| (0, 0), false), None);
    }

    #[test]
    fn caller_return_contract_survives_statement_rewriting() {
        let entry = 0x0200_0000;
        let blocks = [(entry, Block { start: entry, insts: vec![ir::Ins {
            addr: entry, cond: 14, guard: None,
            stmts: vec![S::SetFlag(ir::Flag::C, E::Bool(true)), S::Goto(E::Reg(14))],
        }], succ: vec![], falls_to: None, charge: 1 })].into_iter().collect();
        // No statement or instruction fact advertises a reader. The whole-
        // program return contract must still force the carry write-back.
        assert_eq!(plan(&blocks, entry, &[entry], &[(entry, 0)].into_iter().collect(), &|_, _| (0, 0), crate::raise::C, false), (0, crate::raise::C));
    }
}
