# Block 4: from translated code to readable source (the full decompilation)

Owner direction (2026-10-27): finish the NDS ROM -> source pipeline. The hand port is out of scope.

## Where we are
| Block | State |
|---|---|
| 1. Translate the ROM's code to Rust, with no interpreter | Done. 11,297 ARM9 functions plus the ARM7 program. A race runs to the results screen with 0 interpreted instructions |
| 2. Native window, GPU 3D, audio, controllers | Done |
| 3. Function dictionary, shadow checker, first natives | Running. 11,297 rows (8,246 with notes), 129 structs, 10 verified natives |

**The gap:** the generated `source/` runs, but it isn't readable source. Every function is a register machine:
`c.r[3] = R::subs(c, c.r[1], 0x10u32); ... bus.write32(ea, c.r[0])`. A full decompilation means that every
function comes out like this instead:

```rust
/// Kart_UpdateSpeed(kart: Ptr<Kart>, input: u32)
pub fn kart_update_speed(g: &mut Game, kart: Ptr<Kart>, input: u32) {
    let mut speed = kart.speed(g);
    if input & KEY_A != 0 {
        speed = fx_mul(speed, kart.accel(g));
    }
    while speed > MAX_SPEED { ... }
    kart.set_speed(g, speed);
}
```

The code is still generated on the user's machine from their own ROM, and it still behaves exactly like the game.

## Decision: stay with Rust
C would be simpler in one way: C has `goto`, so any control flow can be written without first turning it into
loops and `if`s. That isn't worth switching for:
- The whole working pipeline is Rust: lifter, runtime, shadow checker, GPU host and builder.
- The structuring problem is solved for Rust too: labeled blocks and `loop`, with a small state machine for the rare
  graphs that can't be structured.
- The one thing C would buy is reusing the community's matching C decomps. We can't ship that code, and it covers
  only about 30% of functions.

## Decision: raise the translation, don't do a matching decomp
A *matching* decomp writes C by hand until Nintendo's compiler produces the exact same bytes. It needs the
proprietary `mwccarm` compiler. After a lot of work, the USA project is at about 30% of functions. A port would
also still need a PC platform layer on top.

A *raising* decompiler starts from what we already have: correct, complete, verified code. It improves that code
automatically, and the shadow checker proves each step. This covers 100% of functions from day one, and readability
gets better over time.

## Tiers (each function is in exactly one; the report counts them)
| Tier | What the function looks like | How it gets there |
|---|---|---|
| T0 exact | Today's register machine | Generator (done) |
| T1 raised | Local variables instead of registers, no dead flag math, `if`/`while`/`match` instead of jumps, stack slots as locals, typed calls `f(g, a, b) -> r` | Automatic decompiler passes (4A, 4B) |
| T2 typed | Real name, typed arguments, struct fields (`kart.speed(g)`), `Fx32`/`VecFx32`, named globals and constants | Dictionary plus type propagation (4C), filled in by agent batches (4D) |
| T3 native | Hand-tuned Rust that talks to native services (files, GX, sound) instead of hardware registers | Block 3 natives and Block 5 |

**Rule:** a function moves up a tier only after its new version passes the shadow checker with 0 mismatches over the
recorded corpus (races, battle, mission, menus). If it fails, the build automatically keeps the previous tier for
that function. Behaviour never changes.

## Work packages

### 4A. An IR inside the lifter (foundation)
`lift.rs` currently formats Rust strings straight from each decoded instruction. Put a small IR in between:
decode -> CFG of IR operations (explicit reads and writes of registers, flags and memory) -> emit.
- The first emitter prints T0 from the IR. Its output must match today's generator exactly (text diff, or the
  shadow corpus where the text changes).
- ARM7 goes through the same IR.
- **Done when:** a fresh build from the IR emitter passes the race acceptance with 0 interpreted instructions, and
  `race_check` under `MKDS_SHADOW=1` shows 0 mismatches.

### 4B. Decompiler passes (T1), whole program
In order. Each pass is separately switchable, so a failing function can be traced to the pass that broke it.
1. **Flag liveness.** Drop every N/Z/C/V calculation that nothing reads. This is most of today's noise.
2. **SSA and register promotion.** Registers become `let` locals. Arguments and returns come from
   `recomp::signature` (already whole-program). The callee-saved discipline is checked per function. Functions
   that break the ARM calling convention (r12 uses, extra returns in r1-r3, reading flags across a call) are
   detected and stay T0.
3. **Stack frames.** `sp`-relative slots become locals. A slot whose address escapes stays in a small frame array
   in DS memory.
4. **Constants.** Literal-pool loads fold into constants. Addresses inside known sections become symbols
   (`G_0217ACF8`, named once the dictionary knows them).
5. **Calls.** `BL`/`BLX`/tail calls become direct typed calls. Calls through function tables become
   `call_indirect(g, addr, args)`, using the existing overlay-safe dispatcher.
6. **Structuring.** Turn the CFG into `if`/`else`, `loop`/`while`, `break`/`continue` and `match` (the switch
   tables are already found). Rust's labeled blocks handle early exits. Irreducible graphs fall back to a local
   `loop { match state { ... } }`.
7. **Simplifier.** Turn `wrapping_add` and casts into plain expressions where values can't overflow. Fold
   comparisons into conditions.
- **Done when:** at least 95% of ARM9 functions are T1 and shadow-clean, every T0 holdout has a recorded reason,
  and a race still finishes 1st at full speed.

### 4C. Types (T2 machinery)
- Generate typed views over DS memory from `structs.csv`: `Ptr<Kart>` with accessor methods
  (`speed()`/`set_speed()`) that compile to the same `Bus` reads and writes. Semantics stay exact while the code
  reads like source.
- Propagate types both ways along calls: a typed argument types the caller's value, and vice versa.
- Separate pointers from integers with relocation analysis. `ds-decomp`'s `relocs` (MIT, Rust crate) or our own
  pass both work.
- Add NitroSDK value types: `Fx32`, `Fx16`, `VecFx32`, `MtxFx43`. Use the SDK rows the dictionary already has.
- Sort the output into modules by the dictionary's `area` column (`kart/`, `item/`, `course/`, `sdk/gx/`, ...),
  not one giant file.

### 4D. Dictionary batches (the repetitive part, handed to Codex)
Batches of about 50 functions per area, hottest and most-called first. Each batch fills in name, `args`/`returns`,
struct fields and notes for its functions. Every batch is checked automatically before it merges:
`export.py` IP check (no ROM strings), the build type-checks, and a shadow race shows 0 mismatches.
- Inputs agents can use, all private and local: IDA databases (`5_tools/idb/mkds_plan*.i64`), the reference C in
  `6_data/export`, `function_notes/`, `STRUCTS.csv`, `mapping_leads.csv`. REA (installed at
  `C:\AI\reverse-engineering\rea`) gives agents MCP access to IDA/Ghidra for xrefs and pseudocode.
- Only our own facts and prose go into the repo: names, types, offsets, notes.
- Codex runs as `codex exec` in its own worktree per batch. Claude reviews and merges. Batches stay small because
  both have usage limits.

### 4E. Source package
The builder writes `source/` as a normal cargo workspace: `game/` (raised code by area), `sdk/`, `arm7/`, plus the
runtime as a dependency, and a `TIERS.md` report (counts per tier and area, each holdout and why).
`--keep-generated` stays the default. Two fresh builds must still produce identical source.

## External helpers
| Project | Use | Caution |
|---|---|---|
| [tylzars/mkds](https://github.com/tylzars/mkds) (USA matching decomp, 3,465 / 11,501 functions matched) | Cross-check our boundaries (11,297 vs 11,501). Source of names and struct layouts | No license stated, and they don't accept outside code. Ask the maintainer before importing names. Never copy C |
| [RonyxDumb/mkds](https://github.com/RonyxDumb/mkds), [ai-tdd-labs/mkds-decomp](https://github.com/ai-tdd-labs/mkds-decomp) (EUR) | Same, via EUR -> USA function matching | Check each license |
| [ds-decomp / dsd](https://github.com/AetiasHax/ds-decomp) (MIT, Rust) | `sig apply` for SDK identification, relocation analysis to tell pointers from integers, delink info | Can be used as a crate |
| [mstan/ndsrecomp](https://github.com/mstan/ndsrecomp) (C, early) | Design comparison only (two-CPU scheduling, oracle testing) | PolyForm Noncommercial + GPL: read, don't copy |
| WiiCompiled / aurora (local copy) | Reference for the Block 5 platform layer | GPLv3: ideas only |
| [REA](https://github.com/morluto/rea) | Agent access to IDA/Ghidra during 4D | Analysis only; it doesn't produce source |

## Order and checkpoints
1. **4A** (agent-lift, 1 lane). Nothing else changes in `lift.rs` until it lands.
2. **4B** passes 1-3 -> checkpoint: % T1 on the corpus.
3. **4B** passes 4-7, in parallel with **4C** typed views and **4D** batches (4D can start right away: it only
   touches the dictionary).
4. **4E** packaging, then a fresh build and acceptance.

**Block 4 is done when:**
- One command builds from the ROM only.
- 100% of functions are at T1 or above, except listed holdouts (under 5%, each with a reason).
- At least 60% are T2: named and typed (the dictionary already has notes for 73%).
- A full race finishes to results under shadow with 0 mismatches.
- Repeat builds produce identical source.
- The repo has no Nintendo material.

## Later (Block 5)
Real PC pointers in place of DS-memory views, where types are fully known. Native file system, GX and sound services
at the SDK boundary. HD and modding hooks.
