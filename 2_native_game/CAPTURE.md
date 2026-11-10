# Function-input capture (`MKDS_CAPTURE`)

Records real inputs and outputs of lifted ARM9 functions during a race, so an equivalence harness
(for example the C2Rust harness: translated decomp C vs. our translation of the machine code) can
replay a function call in isolation with the state it really had, instead of random memory.

Source: `src/capture.rs` (recorder, Rust reader `capture::read_file`), hooks in `src/fibered.rs`.
Python reader: `tools/capture_read.py`.

## Use

```
MKDS_CAPTURE=C:/out/cap acceptance.exe rom.nds route.csv race 15000
python 2_native_game/tools/capture_read.py C:/out/cap            # summary
python 2_native_game/tools/capture_read.py C:/out/cap/arm9_main_02012345.cap -v
```

- `MKDS_CAPTURE=<dir>`: on (unset or empty: off; nothing is hooked and the run is unchanged).
- `MKDS_CAPTURE_FRAMES=0,2000,6000,10000` (default): call *slots*. Each function gets at most one
  recorded call per slot: slot 0 is its first call, slot *i* its first call at or after frame
  `FRAMES[i]` (hardware frames, as in `telemetry.csv`). So N = the number of listed frames (max 8).
- Works in the fiber runtime (the default of `acceptance`/the player), both `--timing exact` and
  native builds as long as calls go through `Bus::call` (exact builds: every call). `MKDS_FLAT=1`
  records nothing. Only ARM9 code is recorded.
- Recording is passive: it observes the values the bus returns and never accesses guest memory
  itself. A race with capture on is identical to one with it off (checked: see the commit message).
- Files are written when the session ends: one `<segment>_<ENTRY>.cap` per function with at least
  one finished recorded call, `index.csv` (segment, entry, calls, bytes) and `summary.txt`
  (coverage line, also printed to stderr). Calls still running at the end (thread main loops) are
  not written.

## What is recorded per call

Only the function's **own** code is attributed to its recording: accesses made by its callees go to
the callee (which may have its own recording), accesses by an IRQ handler that interrupts it go to
nobody.

- **Entry / exit registers**: r0-r15 and CPSR (r15 is not meaningful; the entry pc is the file's
  entry, Thumb in bit 0). `back` = the pc control returned to (the return address, except
  `abnormal`).
- **Read set**: every read of the function's own code that reads a byte it has not read or written
  before, with the value. Each read carries `after` = the number of events the function had made at
  that time. `after == 0` reads form the initial memory. A byte that was already known but reads
  differently after an event (a callee wrote it) is recorded again with that `after`: a replay applies
  it when event `after - 1` returns (that is the stubbed callee's effect on memory the caller reads).
  Reads are kept at the access's width (1/2/4 bytes, word reads at the aligned address).
- **Events**, in order:
  - `call`: target (bit 0 Thumb), return address, registers before, pc it came back to, registers
    after. Arguments are r0-r3 plus the stack (stack words are memory: the caller wrote them, or they
    are in the read set);
  - `tail`: the function branched into another function's code (tail call, or a branch out of its
    own blocks). Registers at the branch. From here on nothing more is recorded: the call's exit
    registers are that code's result;
  - `swi`: BIOS call number, registers before and after (the BIOS's memory effects are not
    recorded; a later read of changed memory shows up with the swi's `after`);
  - `mrc` / `mcr`: coprocessor access (cp, op1, CRn, CRm, op2) and value;
  - `io_read` / `io_write`: every access to 0x04xxxxxx (address, width, value). I/O reads are not in
    the read set: a replay serves them in order.
- **Writes**: the final value of every byte the function's own code wrote outside I/O (including
  its own stack frame below the entry sp; a harness usually excludes that window), as runs.

### Flags (bit set)

| bit | name | meaning |
|---|---|---|
| 1 | slice | a slice ended during the call (the ARM7, DMA and timers ran in between) |
| 2 | irq | an IRQ handler ran during the call |
| 4 | switch | another guest thread ran during the call |
| 8 | conflict | a byte changed between two reads with no event in between (IRQ/ARM7/DMA/other thread): the read set does not reproduce the call |
| 16 | truncated | the call hit the size limit (2^18 entries); the rest is missing |
| 32 | abnormal | control came back to a pc other than the return address |
| 64 | tail | the call contains a `tail` event |

1, 2 and 4 alone are harmless when there is no `conflict`. A harness should skip 8, 16 and 32
(`Call.safe` in the Python reader).

## Replaying a call

1. Map memory; write every `after == 0` read (`initial_memory()`).
2. Set r0-r15/CPSR from `entry` (sp and lr as recorded; Thumb from the entry's bit 0).
3. Run the function. Serve I/O reads in order from the `io_read` events. Stub every call: check the
   target (and arguments r0-r3) against the next `call`/`swi`/`tail` event, set r0-r3 (and r12, flags
   as the harness wishes) from its `after` registers, apply the `after == k + 1` reads
   (`memory_after(k)`), and continue at the return address. For `tail`: compare target and
   registers, then the call is over; compare the exit registers of the whole call against the stub's
   expected result `exit`.
4. Compare: exit r0/r1 (and callee-saved r4-r11, sp), the ordered call/tail/swi list with their
   arguments, the ordered `io_write` events, and the final bytes written (`writes`) against what the
   function under test wrote.

## File format (little endian)

```
file   := "MKCAP1\0\0" u32 entry  u16 n  u8[n] segment  u32 ncalls  call[ncalls]
call   := u8 slot  u64 frame  u32 flags  regs entry  u32 back  regs exit
          u32 nreads  read[nreads]  u32 nevents  event[nevents]  u32 nruns  run[nruns]
regs   := u32[17]                       r0..r15, CPSR
read   := u32 after  u32 addr  u8 width  u32 value
event  := u8 kind, then by kind:
          1 call     u32 target  u32 ret  regs before  u32 back  regs after
          2 tail     u32 target  regs before
          3 swi      u32 n  regs before  regs after
          4 mrc      u8 cp u8 op1 u8 crn u8 crm u8 op2  u32 value
          5 mcr      (as mrc)
          6 io_read  u32 addr  u8 width  u32 value
          7 io_write u32 addr  u8 width  u32 value
run    := u32 addr  u32 len  u8[len]
```

## Numbers (acceptance race, exact build, 2026-11-10)

`acceptance.exe rom route out 15000` (stops at frame 12,362) with `MKDS_CAPTURE` on: race IDENTICAL to
the reference (and to the same build with capture off); about 44 s instead of 39 s.

- 3,268 functions with at least one recorded call, of 3,379 lifted ARM9 functions that ran at all
  (the table has 23,254 entries over every overlay; most never run in a race). The rest ran only
  through branches (tail calls), as continuations, or only inside a call that never returned.
- 5,890 calls; 5,876 replay-safe (7 `conflict`, 7 `abnormal`, 0 `truncated`); 5 still running at
  the end. Other flags: slice 3,954, irq 275, switch 142, tail 328.
- 7.8 MB in 3,270 files (1.7 MB zipped).
