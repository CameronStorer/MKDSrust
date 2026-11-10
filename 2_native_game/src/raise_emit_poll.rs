//! Native direct functions may suspend at loop polls. The IRQ proof is fail-closed:
//! unknown handlers (including the SDK's context-switching IRQ exit) require the
//! complete register file. The narrow form needs both preservation and proof that
//! handlers do not consume the interrupted function's unspilled values.
use super::*;

thread_local! {
    static IRQ_PRESERVED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

pub fn enabled() -> bool {
    nospill_enabled() && std::env::var("MKDS_POLL_DIRECT").as_deref() == Ok("1")
}

pub fn set_irq_preserved(proven: bool) {
    IRQ_PRESERVED.with(|p| p.set(proven));
}

pub fn narrow() -> bool { enabled() && IRQ_PRESERVED.with(|p| p.get()) }

/// Save the dispatcher's stale register file as well: a direct caller may have
/// an outer stack frame in locals, and its preserved registers are not results.
/// Restoring that snapshot after the poll prevents an inner frame leaking out.
pub(super) fn macro_body(local_flags: bool) -> String {
    let regs = if narrow() { 0x6000 } else { 0x7FFF };
    let spill: String = mask_regs(regs).map(|r|
        format!("c.r[{r}] = {}; ", ir::reg_name(r))).collect();
    let reload: String = if narrow() { String::new() } else {
        mask_regs(regs).map(|r| format!("{} = c.r[{r}]; ", ir::reg_name(r))).collect()
    };
    let sync = if local_flags { flag_locals::sync(crate::raise::NZCV, false) } else { String::new() };
    let flags = if local_flags && !narrow() { flag_locals::sync(crate::raise::NZCV, true) } else { String::new() };
    let kind = if narrow() { "checked IRQ preservation" } else { "full IRQ/context spill" };
    format!("($pc:expr, $n:expr) => {{ poll!($pc, $n, {{}}) }}; \
        ($pc:expr, $n:expr, $f:block) => {{ if !R::spend(c, $n) {{ \
        let poll_regs = c.r; {spill}$f {sync}\
        assert!(m.slice_end(c, $pc, $n), \"direct poll requires a suspending bus\"); \
        {reload}{flags}c.r = poll_regs; /* {kind} */ }} }}")
}
