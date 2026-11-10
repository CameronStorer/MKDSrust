# Direct functions with loop polls

Direct functions with loop polls are a **generator** feature of native timing, on by
default (`MKDS_POLL_DIRECT=0` opts out). It composes with boundary calls (`MKDS_BOUNDARY=0`
opts out), typed signatures (`MKDS_TYPED_SIG=0`) and the default flag locals. Exact generation
ignores the switch. ARM7 is unchanged, and selection adds no direct functions in
the online area.

A poll still uses `R::spend` and `Bus::slice_end`, with the same charge and block
address. A suspending bus keeps the direct Rust frame alive. Flat dispatch selects
a separately emitted copy of the previous T1/direct graph; it cannot unwind a
nested poll-direct call, and falling back wholesale to T0 changes native timing.
That flat graph uses the unchanged boundary-off selection even when the fiber
graph enables boundary calls; the legacy boundary graph can return non-locally
under flat dispatch.

The generator checks a conservative superset of IRQ handlers (all address-taken
entries, plus the SDK IRQ dispatcher and rescheduling exit) using
`preserved_regs`. Unknown helpers, computed transfers, implicit saved-register
inputs and unproven callees reject the proof. Volatile incoming values must not
be implicit handler arguments either. If every candidate passes, polls synchronize
sp/lr and flags and reload no registers. Otherwise polls synchronize and reload
r0-r14. The current ROM's context-switching IRQ path fails the narrow proof, so
its polls use the full form. This is intentional, not a successful IRQ proof.

The full form makes its register reads explicit in signature liveness, including
values overwritten after a poll. It restores the dispatcher's previous register
file after reloading the locals, so a nested direct frame cannot leak its stack
pointer into an outer frame. Normal direct returns must preserve sp; this is
checked after signature convergence. Flags dead to an ordinary return still need
their incoming values for the IRQ's CPSR snapshot.

Context-save/load and rescheduling entries, their transitive callers, and paths
through unknown transfers remain full-spill raised functions. A narrow dispatcher
boundary cannot call an unproven polled function. Dispatcher boundaries also
synchronize/reload flag locals, including tail calls.

Generation prints the IRQ proof failures, direct counts before/after for each
requested boundary mode, and the remaining exclusion reasons. The differential
tests exercise ARM/Thumb loops, nested calls, both IRQ proof outcomes, a handler
that observes/clobbers saved registers, suspending/flat buses, both flag modes,
boundary composition, rejected handlers and unchanged exact output.

## Validation (2026-11-10, base b26989a)

| Selection | Before | Poll-direct |
|---|---:|---:|
| Boundary off | 3,474 | 4,889 |
| Boundary on | 4,327 | 4,982 |

Release tests: 244 passed, zero failed, three ignored; `t1_differential`: 23
passed. The poll test covers 6,144 program states, including repeated IRQs on
every loop iteration. Optional ROM-dependent unit samples were not enabled in
the final unit run; full-ROM races provide the integration proof below.

Both poll-direct alone and poll-direct plus boundary calls produce IDENTICAL
race output files versus the unchanged native baseline: 1st at frame 11,260
with fibers, 1st at frame 11,533 in flat mode. `frame_compare` is EQUAL in every
region and event: 12,161 logical frames / 71,750 events with fibers, 12,433 frames
/ 73,402 events in flat mode.

Exact timing with the poll switch set is IDENTICAL to the reference race in
both fiber and flat modes: 1,707,950,080 instructions, 581,521 IRQs, 1st at frame
11,462. The narrow IRQ form remains conservatively disabled for the ROM's
unproven dispatcher/context path.
