#!/usr/bin/env python3
"""Reader for MKDS_CAPTURE function-input files (format: 2_native_game/CAPTURE.md).

    python capture_read.py <dir>               coverage and size summary of a capture directory
    python capture_read.py <file.cap> [-v]     the calls in one file (-v: every read, event, write)

As a module: `load(path)` returns a `Capture` (segment, entry, calls); `Call.initial_memory()` and
`Call.memory_after(k)` give the bytes a replay harness writes before running / after event k returns.
"""
import os
import struct
import sys
from dataclasses import dataclass, field

MAGIC = b"MKCAP1\0\0"
FLAGS = {1: "slice", 2: "irq", 4: "switch", 8: "conflict", 16: "truncated", 32: "abnormal", 64: "tail"}
# flags that make a call unusable as an isolated replay
UNSAFE = 8 | 16 | 32
E_CALL, E_TAIL, E_SWI, E_MRC, E_MCR, E_IO_READ, E_IO_WRITE = range(1, 8)
EVENT_NAMES = {E_CALL: "call", E_TAIL: "tail", E_SWI: "swi", E_MRC: "mrc", E_MCR: "mcr", E_IO_READ: "io_read", E_IO_WRITE: "io_write"}


@dataclass
class Read:
    after: int  # events made before the read: apply after event `after - 1` returns (0: initial memory)
    addr: int
    width: int
    value: int


@dataclass
class Event:
    kind: int
    target: int = 0  # call / tail: target (bit 0 = Thumb); swi: number
    ret: int = 0  # call: return address
    before: tuple = ()  # r0..r15, cpsr
    back: int = 0  # call: pc it came back to
    after: tuple = ()  # call / swi: r0..r15, cpsr
    op: bytes = b""  # mrc / mcr: cp, op1, crn, crm, op2
    addr: int = 0  # io: address
    width: int = 0
    value: int = 0  # mrc / mcr / io

    @property
    def name(self):
        return EVENT_NAMES[self.kind]


@dataclass
class Call:
    slot: int
    frame: int
    flags: int
    entry: tuple  # r0..r15, cpsr at entry
    back: int  # pc control came back to
    exit: tuple  # r0..r15, cpsr after the return
    reads: list = field(default_factory=list)
    events: list = field(default_factory=list)
    writes: list = field(default_factory=list)  # (address, bytes): final bytes the function's own code wrote

    @property
    def safe(self):
        return self.flags & UNSAFE == 0

    def flag_names(self):
        return [n for b, n in FLAGS.items() if self.flags & b] or ["-"]

    def _bytes(self, pred):
        mem = {}
        for r in self.reads:
            if pred(r.after):
                for i in range(r.width):
                    mem[(r.addr + i) & 0xFFFFFFFF] = (r.value >> (8 * i)) & 0xFF
        return mem

    def initial_memory(self):
        """{byte address: value} to set up before the call."""
        return self._bytes(lambda a: a == 0)

    def memory_after(self, k):
        """{byte address: value} to apply when event k (0-based) returns (a stubbed callee's effects)."""
        return self._bytes(lambda a: a == k + 1)

    def io_reads(self):
        return [e for e in self.events if e.kind == E_IO_READ]

    def calls(self):
        return [e for e in self.events if e.kind in (E_CALL, E_TAIL, E_SWI)]

    def final_writes(self):
        out = {}
        for a, b in self.writes:
            for i, v in enumerate(b):
                out[a + i] = v
        return out


@dataclass
class Capture:
    segment: str
    entry: int
    calls: list


class _R:
    def __init__(self, data):
        self.d, self.o = data, 0

    def take(self, n):
        if self.o + n > len(self.d):
            raise ValueError("capture file is truncated")
        b = self.d[self.o:self.o + n]
        self.o += n
        return b

    def u8(self):
        return self.take(1)[0]

    def u32(self):
        return struct.unpack_from("<I", self.take(4))[0]

    def u64(self):
        return struct.unpack_from("<Q", self.take(8))[0]

    def regs(self):
        return struct.unpack("<17I", self.take(68))


def _call(r):
    c = Call(slot=r.u8(), frame=r.u64(), flags=r.u32(), entry=r.regs(), back=r.u32(), exit=r.regs())
    for _ in range(r.u32()):
        c.reads.append(Read(r.u32(), r.u32(), r.u8(), r.u32()))
    for _ in range(r.u32()):
        k = r.u8()
        if k == E_CALL:
            e = Event(k, target=r.u32(), ret=r.u32(), before=r.regs(), back=r.u32(), after=r.regs())
        elif k == E_TAIL:
            e = Event(k, target=r.u32(), before=r.regs())
        elif k == E_SWI:
            e = Event(k, target=r.u32(), before=r.regs(), after=r.regs())
        elif k in (E_MRC, E_MCR):
            e = Event(k, op=r.take(5), value=r.u32())
        elif k in (E_IO_READ, E_IO_WRITE):
            e = Event(k, addr=r.u32(), width=r.u8(), value=r.u32())
        else:
            raise ValueError(f"unknown event kind {k}")
        c.events.append(e)
    for _ in range(r.u32()):
        a, n = r.u32(), r.u32()
        c.writes.append((a, r.take(n)))
    return c


def parse(data):
    r = _R(data)
    if r.take(8) != MAGIC:
        raise ValueError("not a capture file")
    entry = r.u32()
    seg = r.take(struct.unpack("<H", r.take(2))[0]).decode()
    calls = [_call(r) for _ in range(r.u32())]
    if r.o != len(data):
        raise ValueError("trailing bytes after the last call")
    return Capture(seg, entry, calls)


def load(path):
    with open(path, "rb") as f:
        return parse(f.read())


def _summary(d):
    files = [f for f in os.listdir(d) if f.endswith(".cap")]
    calls = safe = size = 0
    flags = {}
    for f in files:
        cap = load(os.path.join(d, f))
        size += os.path.getsize(os.path.join(d, f))
        for c in cap.calls:
            calls += 1
            safe += c.safe
            for n in c.flag_names():
                flags[n] = flags.get(n, 0) + 1
    print(f"{len(files)} functions, {calls} calls ({safe} replay-safe), {size} bytes")
    print("flags:", ", ".join(f"{k} {v}" for k, v in sorted(flags.items())))
    s = os.path.join(d, "summary.txt")
    if os.path.exists(s):
        print(open(s).read().strip())


def _show(path, verbose):
    cap = load(path)
    print(f"{cap.segment} {cap.entry:#010x}: {len(cap.calls)} calls")
    for i, c in enumerate(cap.calls):
        print(f"  [{i}] slot {c.slot} frame {c.frame} flags {','.join(c.flag_names())} "
              f"r0-r3 {' '.join(f'{v:08x}' for v in c.entry[:4])} sp {c.entry[13]:08x} lr {c.entry[14]:08x} "
              f"-> r0 {c.exit[0]:08x} r1 {c.exit[1]:08x}; {len(c.reads)} reads, {len(c.events)} events, "
              f"{sum(len(b) for _, b in c.writes)} bytes written")
        if verbose:
            for r in c.reads:
                print(f"      read  after {r.after} [{r.addr:08x}]{r.width} = {r.value:#x}")
            for k, e in enumerate(c.events):
                if e.kind in (E_IO_READ, E_IO_WRITE):
                    print(f"      ev {k} {e.name} [{e.addr:08x}]{e.width} = {e.value:#x}")
                elif e.kind in (E_MRC, E_MCR):
                    print(f"      ev {k} {e.name} {list(e.op)} = {e.value:#x}")
                elif e.kind == E_TAIL:
                    print(f"      ev {k} tail {e.target:08x} r0-r3 {' '.join(f'{v:08x}' for v in e.before[:4])}")
                else:
                    print(f"      ev {k} {e.name} {e.target:08x} r0-r3 {' '.join(f'{v:08x}' for v in e.before[:4])} "
                          f"-> r0 {e.after[0]:08x} r1 {e.after[1]:08x}")
            for a, b in c.writes:
                print(f"      write [{a:08x}] {b.hex()}")


if __name__ == "__main__":
    if len(sys.argv) < 2:
        sys.exit(__doc__)
    p = sys.argv[1]
    if os.path.isdir(p):
        _summary(p)
    else:
        _show(p, "-v" in sys.argv[2:])
