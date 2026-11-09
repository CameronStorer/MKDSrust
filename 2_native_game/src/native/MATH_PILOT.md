# Math readable-source pilot

13 trial entries in `native::NATIVE`, selected with `MKDS_NATIVES=all` or by the
acceptance tool's `MKDS_SHADOW=1`. `VERIFIED` is unchanged. This covers 13 of the
121 `code_sys_math` entries, not the whole area. No generator or dictionary edits.

## Pattern and semantics

`Fx32`, `VecFx32`, and the four matrix shapes are ordinary values. The public
arithmetic functions take values and return values: no CPU, flags, Bus, guest
pointers, floats, ROM tables, or assets. ABI adapters below them preserve the
legacy scratch registers, flags, stack stores, and math-unit transactions.

The two dot products deliberately differ: one truncates; the SDK-shaped one adds
2048 before shifting 12 bits. Signed 64-bit accumulators wrap before narrowing.
Cross products truncate. Distance wraps component subtraction and the squared
sum, shifts that sum left two, takes an integer floor square root, then performs
the original signed rounding shift. Consequently extreme, non-coordinate inputs
can produce a negative distance: preserving behavior is intentional.

The mode-0 divider handles zero and MIN/-1 without Rust panics, including the
hardware's quotient high-word anomaly. Hardware behavior was checked against
[GBATEK](https://problemkaputt.de/gbatek.htm#dsarithmeticunits). The sqrt uses only
integer arithmetic, including at `u64::MAX`. Adapters retain device writes in the
original width/order and busy/result reads; their numerical answer comes from
the value core. No Nintendo SDK source was consulted or copied.

## Evidence

- Unchanged native baseline: first at frame **11260**, zero interpreted instructions.
- ROM differential: **4096 cases per pilot entry (53,248 total)**, comparing all
  r0-r14, CPSR, live RAM, ordered hardware reads/writes, and final math-unit state.
  Includes edge scalar inputs, unaligned/overlapping vector storage, wrapping
  addresses, saved-stack inputs, and finite busy polling. Output must not corrupt
  the callee's saved-register frame (normal ABI precondition).
- Additional tests: 4096 full-range integer-sqrt inequalities (using u128),
  49 full-divider hardware edge pairs, fixed-point tie and wrapping cases.
- Required `cargo test --release --locked --features gen`: **227 passed, 0 failed,
  3 ignored**, without optional ROM sweeps. The ROM SDK test was run separately
  with `MKDS_ROM`; all 24 registered natives passed 4096 cases each.
- An additional whole-suite run with `MKDS_ROM` passed the 300-function T1 ROM
  sweep, but its existing no-spill ROM sweep did not terminate after several
  minutes on random inputs. Only this lane's generated child was stopped. That
  optional sweep is incomplete; no unrelated generator/test code was changed.
- Exact default races, fibers and flat: **IDENTICAL** to `race_main` for telemetry,
  inputs and every screenshot. Both: first at frame **11462**, **1,707,950,080**
  instructions and **581,521** IRQs. The fiber-only overlay-instruction counter
  remains its documented zero; the flat log also matches the reference's counter.
- Shadow: `MKDS_FLAT=1 MKDS_SHADOW=1`, **0 mismatches**. Default fiber execution
  bypasses the current shadow hook and produced an empty tally; that run is not
  counted as evidence. No runtime/shadow plumbing was changed in this lane.
- Changed native race results: pending below.

## Shadow coverage

| Function | Calls | Mismatches | At least 50 calls |
|---|---:|---:|---|
| `fx_abs` | 20,485 | 0 | yes |
| `fx_mul_091f44` | 0 | 0 | no |
| `FX_Mul` | 3,475 | 0 | yes |
| `vec_dot_product` | 1,788,266 | 0 | yes |
| `VEC_DotProduct` | 529,556 | 0 | yes |
| `vec_cross_product` | 179,293 | 0 | yes |
| `VEC_Distance` | 0 | 0 | no |
| `MTX_Identity22` | 738 | 0 | yes |
| `MTX_Identity33` | 380 | 0 | yes |
| `MTX_Identity43` | 22,827 | 0 | yes |
| `MTX_Identity44` | 8 | 0 | no |
| `FX_Mod` | 0 | 0 | no |
| `FX_DivS32` | 0 | 0 | no |

Eight entries meet this race's call threshold; five need more corpus coverage.
The pilot entries account for **2,545,028 checked calls**, with zero mismatches.
All remain on trial as requested.

## Cost per call

Optimized x64 release, five batches of 100,000 calls, median ns/call. Core numbers
measure throughput on fixed representative inputs passed through `black_box`;
matrix constructors are constants. Adapter and lifted columns include identical
CPU reset overhead, dense simulated RAM, and the production math-device model.
The lifted column is freshly emitted ROM T0 with instruction-budget charges
removed. Core costs exclude ABI, simulated memory and device traffic; they are
not an end-to-end speedup claim. Other fleet lanes were active; timings are indicative.

| Entry | Function | Core ns | Adapter ns | Lifted ns |
|---|---|---:|---:|---:|
| `0x020738f8` | `fx_abs` | 0.19 | 9.11 | 9.30 |
| `0x02091f44` | `fx_mul_091f44` | 0.36 | 9.91 | 9.99 |
| `0x021488e8` | `FX_Mul` | 0.38 | 10.29 | 10.89 |
| `0x02039828` | `vec_dot_product` | 0.79 | 13.21 | 18.25 |
| `0x0214848c` | `VEC_DotProduct` | 0.86 | 13.92 | 19.12 |
| `0x020397a8` | `vec_cross_product` | 1.18 | 17.45 | 27.57 |
| `0x02147f50` | `VEC_Distance` | 13.08 | 66.97 | 66.25 |
| `0x02146c98` | `MTX_Identity22` | 0.82 | 15.22 | 16.93 |
| `0x02146cbc` | `MTX_Identity33` | 0.73 | 12.36 | 24.61 |
| `0x021471bc` | `MTX_Identity43` | 0.73 | 18.62 | 32.45 |
| `0x02147954` | `MTX_Identity44` | 0.73 | 23.95 | 42.61 |
| `0x02147cd8` | `FX_Mod` | 1.28 | 60.77 | 61.17 |
| `0x02147d30` | `FX_DivS32` | 1.28 | 59.73 | 61.85 |

Dot/cross and matrix adapters save work even before removing simulated memory.
Distance currently gains nothing at the boundary: it retains hardware sqrt
traffic and also computes the integer result. Divider adapters have the same
transition cost. Keep these observational adapters until T3b owns device state.
Tiny scalar adapters are dominated by the legacy ABI, while their cores inline
into a handful of operations. Per-function machine-code sizes were not isolated
because inlining and shared helpers make executable-size subtraction misleading.

## Scaling and remaining work

- This is a template, not automatic conversion or a replacement for the T3b design.
  Scale by separating mathematical values from ABI artifacts, then generate the
  scratch-register/flag glue from verified liveness instead of hand-maintaining it.
- Preserve rounding variants explicitly. Do not substitute floats or ordinary
  unchecked addition, or assume that every similarly named entry is a true leaf.
- Table-backed trig needs a typed, owner-supplied table view. Random/RTC routines,
  normalization and matrix concatenation need their own call/device review.
  Those entries and the rest of the 121-function area remain lifted.
- Keep all 13 entries on trial. A single race is not the battle/mission/menu corpus;
  entries with fewer than 50 shadow calls do not qualify even for this race.

Reproduce from this worktree using the fleet build/race commands. For the benchmark:
`MKDS_ROM=<private ROM> MKDS_MATH_SCRATCH=<private lane folder> cargo test --release
--locked --features gen --test native_math_bench -- --ignored --nocapture`.
The benchmark emits Rust only to that private folder and writes `math_bench/results.csv`.
