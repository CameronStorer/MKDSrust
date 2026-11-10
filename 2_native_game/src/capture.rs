//! Function-input capture (`MKDS_CAPTURE=<dir>`, default off): during a race, for every lifted ARM9
//! function, up to one call per *slot* (by default: the first call, then the first call after frames
//! 2000, 6000 and 10000; `MKDS_CAPTURE_FRAMES=0,2000,6000,10000`) is recorded with everything needed to
//! replay it in isolation against another implementation (the C2Rust equivalence harness):
//!
//! - entry registers (r0-r15, CPSR) and exit registers;
//! - the **read set**: each memory access of the function's *own* code (not its callees') that reads
//!   bytes it has not written or read before, with the value read, tagged with the number of events
//!   (calls, BIOS calls, ...) the function had made by then. A byte that was known (read or written)
//!   but reads differently after a call is recorded again: the callee changed it;
//! - the ordered **events**: calls (target, registers in and out), tail calls, BIOS calls, coprocessor
//!   accesses and every I/O (0x04xxxxxx) read and write;
//! - the final value of every byte the function's own code wrote.
//!
//! Recording is passive: it only observes values the bus returns and never accesses guest memory
//! itself. It works in the fiber runtime (the default of `acceptance`); `MKDS_FLAT=1` records nothing.
//! Files: `<dir>/<segment>_<ENTRY>.cap`, one per function, plus `index.csv` and `summary.txt`.
//! Format: `2_native_game/CAPTURE.md`; reader: [`read_file`] and `2_native_game/tools/capture_read.py`.
use crate::mem::Lifted;
use crate::rt::Cpu;
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

pub const MAGIC: &[u8; 8] = b"MKCAP1\0\0";

/// The call was interrupted by a slice end (the ARM7, DMA and timers ran meanwhile).
pub const F_SLICE: u32 = 1;
/// An IRQ handler ran during the call.
pub const F_IRQ: u32 = 2;
/// Another guest thread ran during the call.
pub const F_SWITCH: u32 = 4;
/// A byte read twice gave different values with no event in between (changed by an IRQ, the ARM7,
/// DMA or another thread): the read set alone does not reproduce the call.
pub const F_CONFLICT: u32 = 8;
/// The recording hit its size limit; reads/events/writes after that point are missing.
pub const F_TRUNCATED: u32 = 16;
/// Control came back to a pc other than the return address (a non-local exit).
pub const F_ABNORMAL: u32 = 32;
/// The function left its own code by a branch to another function (see the `Tail` event).
pub const F_TAIL: u32 = 64;

/// Recorded accesses per call before [`F_TRUNCATED`].
const LIMIT: usize = 1 << 18;

pub const E_CALL: u8 = 1;
pub const E_TAIL: u8 = 2;
pub const E_SWI: u8 = 3;
pub const E_MRC: u8 = 4;
pub const E_MCR: u8 = 5;
pub const E_IO_READ: u8 = 6;
pub const E_IO_WRITE: u8 = 7;

/// Registers r0-r15 then CPSR.
pub type Regs = [u32; 17];

pub fn regs(c: &Cpu) -> Regs {
    let mut r = [0; 17];
    r[..16].copy_from_slice(&c.r);
    r[16] = c.cpsr();
    r
}

fn io(a: u32) -> bool {
    a >> 24 == 0x04
}

#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// A call: target (bit 0 = Thumb), return address, registers before, the pc it came back to,
    /// registers after.
    Call { target: u32, ret: u32, before: Regs, back: u32, after: Regs },
    /// A branch to another function's code: from here on the function's behaviour is that function's
    /// (its accesses are not recorded); the call's exit registers are its result.
    Tail { target: u32, before: Regs },
    Swi { n: u32, before: Regs, after: Regs },
    Mrc { op: [u8; 5], value: u32 },
    Mcr { op: [u8; 5], value: u32 },
    IoRead { addr: u32, width: u8, value: u32 },
    IoWrite { addr: u32, width: u8, value: u32 },
}

#[derive(Clone, Debug, PartialEq)]
pub struct Read {
    /// Events made before this read: apply the value after event `after - 1` returns (0: initial memory).
    pub after: u32,
    pub addr: u32,
    pub width: u8,
    pub value: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Call {
    pub slot: u8,
    pub frame: u64,
    pub flags: u32,
    pub entry: Regs,
    /// The pc control came back to (the return address unless [`F_ABNORMAL`]).
    pub back: u32,
    pub exit: Regs,
    pub reads: Vec<Read>,
    pub events: Vec<Event>,
    /// Final bytes written by the function's own code, as runs (address, bytes).
    pub writes: Vec<(u32, Vec<u8>)>,
}

struct Rec {
    func: u32,
    slot: u8,
    frame: u64,
    flags: u32,
    entry: Regs,
    /// Byte -> (value, event count when it became known).
    known: HashMap<u32, (u8, u32)>,
    reads: Vec<Read>,
    events: Vec<Event>,
    writes: BTreeMap<u32, u8>,
    counters: (u64, u64, u64),
}

impl Rec {
    fn full(&mut self) -> bool {
        if self.reads.len() + self.events.len() + self.writes.len() >= LIMIT {
            self.flags |= F_TRUNCATED;
        }
        self.flags & F_TRUNCATED != 0
    }
    fn now(&self) -> u32 {
        self.events.len() as u32
    }
}

pub struct Capture {
    dir: PathBuf,
    /// Slot start frames (slot 0 starts at the first call whatever its value).
    frames: Vec<u64>,
    table: &'static [Lifted],
    /// (call target, fn pointer) -> function index in `table` (u32::MAX: not a function entry).
    keys: HashMap<(u32, usize), u32>,
    taken: Vec<u8>,
    out: Vec<Vec<u8>>,
    counts: Vec<u8>,
    active: Vec<Option<Rec>>,
    free: Vec<u32>,
    pub slices: u64,
    pub irqs: u64,
    pub switches: u64,
    finished: u64,
    flagged: u64,
    /// Every function the fiber dispatch loops ran (fn pointers), for the coverage line.
    pub ran: std::collections::HashSet<usize>,
}

impl Capture {
    /// `MKDS_CAPTURE=<dir>` set: a recorder for the ARM9 functions in `table`.
    pub fn from_env(table: &'static [Lifted]) -> Option<Box<Capture>> {
        let dir = std::env::var_os("MKDS_CAPTURE").filter(|d| !d.is_empty())?;
        let frames = std::env::var("MKDS_CAPTURE_FRAMES")
            .ok()
            .map(|v| v.split(',').filter_map(|f| f.trim().parse().ok()).collect::<Vec<u64>>())
            .filter(|f| !f.is_empty() && f.len() <= 8)
            .unwrap_or_else(|| vec![0, 2000, 6000, 10000]);
        Some(Box::new(Capture::new(PathBuf::from(dir), frames, table)))
    }

    pub fn new(dir: PathBuf, frames: Vec<u64>, table: &'static [Lifted]) -> Capture {
        Capture {
            dir,
            frames,
            table,
            keys: HashMap::new(),
            taken: vec![0; table.len()],
            out: vec![Vec::new(); table.len()],
            counts: vec![0; table.len()],
            active: Vec::new(),
            free: Vec::new(),
            slices: 0,
            irqs: 0,
            switches: 0,
            finished: 0,
            flagged: 0,
            ran: Default::default(),
        }
    }

    fn rec(&mut self, id: u32) -> &mut Rec {
        self.active[id as usize].as_mut().unwrap()
    }

    /// A call to `target`, which the dispatcher maps to `f`: starts a recording if the function's
    /// slot for `frame` is still free. `resident`: the overlays dispatched now (only asked for when
    /// `f` belongs to several functions).
    pub fn start(&mut self, target: u32, f: usize, c: &Cpu, frame: u64, resident: &dyn Fn() -> Vec<&'static str>) -> Option<u32> {
        let table = self.table;
        let func = *self.keys.entry((target, f)).or_insert_with(|| {
            let hits: Vec<usize> = (0..table.len()).filter(|&i| table[i].entry == target && table[i].f as usize == f).collect();
            match hits.len() {
                0 => u32::MAX,
                1 => hits[0] as u32,
                _ => {
                    let r = resident();
                    hits.iter().copied().find(|&i| r.contains(&table[i].segment)).unwrap_or(hits[0]) as u32
                }
            }
        });
        if func == u32::MAX {
            return None;
        }
        let slot = self.frames.iter().skip(1).filter(|&&s| frame >= s).count() as u8;
        if self.taken[func as usize] & (1 << slot) != 0 {
            return None;
        }
        self.taken[func as usize] |= 1 << slot;
        let rec = Rec {
            func,
            slot,
            frame,
            flags: 0,
            entry: regs(c),
            known: HashMap::new(),
            reads: Vec::new(),
            events: Vec::new(),
            writes: BTreeMap::new(),
            counters: (self.slices, self.irqs, self.switches),
        };
        Some(match self.free.pop() {
            Some(id) => {
                self.active[id as usize] = Some(rec);
                id
            }
            None => {
                self.active.push(Some(rec));
                self.active.len() as u32 - 1
            }
        })
    }

    pub fn read(&mut self, id: u32, a: u32, width: u8, value: u32) {
        let r = self.rec(id);
        if r.full() {
            return;
        }
        let now = r.now();
        if io(a) {
            r.events.push(Event::IoRead { addr: a, width, value });
            return;
        }
        let mut record = false;
        for i in 0..width as u32 {
            let b = (value >> (8 * i)) as u8;
            match r.known.insert(a.wrapping_add(i), (b, now)) {
                Some((old, at)) if old != b => {
                    record = true;
                    if at == now {
                        r.flags |= F_CONFLICT;
                    }
                }
                Some(_) => {}
                None => record = true,
            }
        }
        if record {
            r.reads.push(Read { after: now, addr: a, width, value });
        }
    }

    pub fn write(&mut self, id: u32, a: u32, width: u8, value: u32) {
        let r = self.rec(id);
        if r.full() {
            return;
        }
        if io(a) {
            r.events.push(Event::IoWrite { addr: a, width, value });
            return;
        }
        let now = r.now();
        for i in 0..width as u32 {
            let b = (value >> (8 * i)) as u8;
            r.known.insert(a.wrapping_add(i), (b, now));
            r.writes.insert(a.wrapping_add(i), b);
        }
    }

    /// The recorded function calls `target`; returns the event to complete with [`Capture::call_end`].
    pub fn call_begin(&mut self, id: u32, target: u32, ret: u32, c: &Cpu) -> Option<usize> {
        let r = self.rec(id);
        if r.full() {
            return None;
        }
        r.events.push(Event::Call { target, ret, before: regs(c), back: 0, after: [0; 17] });
        Some(r.events.len() - 1)
    }

    pub fn call_end(&mut self, id: u32, ev: usize, c: &Cpu, pc: u32) {
        if let Some(Event::Call { back, after, .. }) = self.rec(id).events.get_mut(ev) {
            *back = pc;
            *after = regs(c);
        }
    }

    pub fn tail(&mut self, id: u32, pc: u32, c: &Cpu) {
        let r = self.rec(id);
        r.flags |= F_TAIL;
        if !r.full() {
            r.events.push(Event::Tail { target: pc, before: regs(c) });
        }
    }

    pub fn swi(&mut self, id: u32, n: u32, before: Regs, c: &Cpu) {
        let r = self.rec(id);
        if !r.full() {
            r.events.push(Event::Swi { n, before, after: regs(c) });
        }
    }

    pub fn coproc(&mut self, id: u32, write: bool, op: [u8; 5], value: u32) {
        let r = self.rec(id);
        if !r.full() {
            r.events.push(if write { Event::Mcr { op, value } } else { Event::Mrc { op, value } });
        }
    }

    /// The recorded call came back to `pc` (`ret`: its return address).
    pub fn finish(&mut self, id: u32, c: &Cpu, pc: u32, ret: u32) {
        let mut r = self.active[id as usize].take().unwrap();
        self.free.push(id);
        if pc != ret {
            r.flags |= F_ABNORMAL;
        }
        let (s, i, w) = r.counters;
        if self.slices != s {
            r.flags |= F_SLICE;
        }
        if self.irqs != i {
            r.flags |= F_IRQ;
        }
        if self.switches != w {
            r.flags |= F_SWITCH;
        }
        let mut writes: Vec<(u32, Vec<u8>)> = Vec::new();
        for (&a, &b) in &r.writes {
            match writes.last_mut() {
                Some((s, v)) if s.wrapping_add(v.len() as u32) == a => v.push(b),
                _ => writes.push((a, vec![b])),
            }
        }
        let call = Call {
            slot: r.slot,
            frame: r.frame,
            flags: r.flags,
            entry: r.entry,
            back: pc,
            exit: regs(c),
            reads: r.reads,
            events: r.events,
            writes,
        };
        let f = r.func as usize;
        encode(&call, &mut self.out[f]);
        self.counts[f] += 1;
        self.finished += 1;
        if call.flags & (F_CONFLICT | F_TRUNCATED | F_ABNORMAL) != 0 {
            self.flagged += 1;
        }
    }

    /// Writes the files (also done when the capture is dropped).
    pub fn flush(&mut self) -> std::io::Result<String> {
        use std::io::Write;
        std::fs::create_dir_all(&self.dir)?;
        let mut index = String::from("segment,entry,calls,bytes\n");
        let (mut covered, mut bytes) = (0u64, 0u64);
        for (i, l) in self.table.iter().enumerate() {
            if self.counts[i] == 0 {
                continue;
            }
            let mut data = Vec::with_capacity(self.out[i].len() + 32);
            data.extend_from_slice(MAGIC);
            data.extend_from_slice(&l.entry.to_le_bytes());
            data.extend_from_slice(&(l.segment.len() as u16).to_le_bytes());
            data.extend_from_slice(l.segment.as_bytes());
            data.extend_from_slice(&(self.counts[i] as u32).to_le_bytes());
            data.extend_from_slice(&self.out[i]);
            std::fs::File::create(self.dir.join(format!("{}_{:08X}.cap", l.segment, l.entry & !1)))?.write_all(&data)?;
            index.push_str(&format!("{},{:#010x},{},{}\n", l.segment, l.entry, self.counts[i], data.len()));
            covered += 1;
            bytes += data.len() as u64;
        }
        let unfinished = self.active.iter().filter(|r| r.is_some()).count();
        let ran = self.table.iter().filter(|l| self.ran.contains(&(l.f as usize))).count();
        let summary = format!(
            "capture: {covered} of {} functions with >= 1 recorded call ({ran} ran at all); {} calls ({} flagged conflict/truncated/abnormal, {unfinished} still running at the end, not written); {bytes} bytes; slots from frames {:?}",
            self.table.len(),
            self.finished,
            self.flagged,
            self.frames
        );
        std::fs::write(self.dir.join("index.csv"), index)?;
        std::fs::write(self.dir.join("summary.txt"), format!("{summary}\n"))?;
        Ok(summary)
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        match self.flush() {
            Ok(s) => eprintln!("{s}"),
            Err(e) => eprintln!("capture: cannot write {}: {e}", self.dir.display()),
        }
    }
}

// ---- encoding (little endian; see CAPTURE.md)

fn put_regs(o: &mut Vec<u8>, r: &Regs) {
    for v in r {
        o.extend_from_slice(&v.to_le_bytes());
    }
}

pub fn encode(c: &Call, o: &mut Vec<u8>) {
    let u = |o: &mut Vec<u8>, v: u32| o.extend_from_slice(&v.to_le_bytes());
    o.push(c.slot);
    o.extend_from_slice(&c.frame.to_le_bytes());
    u(o, c.flags);
    put_regs(o, &c.entry);
    u(o, c.back);
    put_regs(o, &c.exit);
    u(o, c.reads.len() as u32);
    for r in &c.reads {
        u(o, r.after);
        u(o, r.addr);
        o.push(r.width);
        u(o, r.value);
    }
    u(o, c.events.len() as u32);
    for e in &c.events {
        match e {
            Event::Call { target, ret, before, back, after } => {
                o.push(E_CALL);
                u(o, *target);
                u(o, *ret);
                put_regs(o, before);
                u(o, *back);
                put_regs(o, after);
            }
            Event::Tail { target, before } => {
                o.push(E_TAIL);
                u(o, *target);
                put_regs(o, before);
            }
            Event::Swi { n, before, after } => {
                o.push(E_SWI);
                u(o, *n);
                put_regs(o, before);
                put_regs(o, after);
            }
            Event::Mrc { op, value } | Event::Mcr { op, value } => {
                o.push(if matches!(e, Event::Mrc { .. }) { E_MRC } else { E_MCR });
                o.extend_from_slice(op);
                u(o, *value);
            }
            Event::IoRead { addr, width, value } | Event::IoWrite { addr, width, value } => {
                o.push(if matches!(e, Event::IoRead { .. }) { E_IO_READ } else { E_IO_WRITE });
                u(o, *addr);
                o.push(*width);
                u(o, *value);
            }
        }
    }
    u(o, c.writes.len() as u32);
    for (a, b) in &c.writes {
        u(o, *a);
        u(o, b.len() as u32);
        o.extend_from_slice(b);
    }
}

struct Cursor<'a>(&'a [u8]);

impl Cursor<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8], String> {
        if self.0.len() < n {
            return Err("capture file is truncated".into());
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Ok(a)
    }
    fn u8(&mut self) -> Result<u8, String> {
        Ok(self.take(1)?[0])
    }
    fn u32(&mut self) -> Result<u32, String> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn regs(&mut self) -> Result<Regs, String> {
        let mut r = [0; 17];
        for v in &mut r {
            *v = self.u32()?;
        }
        Ok(r)
    }
}

/// A function's capture file: (segment, entry, calls).
pub fn read_file(data: &[u8]) -> Result<(String, u32, Vec<Call>), String> {
    let mut c = Cursor(data);
    if c.take(8)? != MAGIC {
        return Err("not a capture file".into());
    }
    let entry = c.u32()?;
    let n = u16::from_le_bytes(c.take(2)?.try_into().unwrap()) as usize;
    let segment = String::from_utf8_lossy(c.take(n)?).into_owned();
    let calls = (0..c.u32()?).map(|_| decode(&mut c)).collect::<Result<Vec<_>, _>>()?;
    if !c.0.is_empty() {
        return Err("trailing bytes after the last call".into());
    }
    Ok((segment, entry, calls))
}

fn decode(c: &mut Cursor<'_>) -> Result<Call, String> {
    let slot = c.u8()?;
    let frame = u64::from_le_bytes(c.take(8)?.try_into().unwrap());
    let flags = c.u32()?;
    let entry = c.regs()?;
    let back = c.u32()?;
    let exit = c.regs()?;
    let mut reads = Vec::new();
    for _ in 0..c.u32()? {
        reads.push(Read { after: c.u32()?, addr: c.u32()?, width: c.u8()?, value: c.u32()? });
    }
    let mut events = Vec::new();
    for _ in 0..c.u32()? {
        events.push(match c.u8()? {
            E_CALL => Event::Call { target: c.u32()?, ret: c.u32()?, before: c.regs()?, back: c.u32()?, after: c.regs()? },
            E_TAIL => Event::Tail { target: c.u32()?, before: c.regs()? },
            E_SWI => Event::Swi { n: c.u32()?, before: c.regs()?, after: c.regs()? },
            k @ (E_MRC | E_MCR) => {
                let op: [u8; 5] = c.take(5)?.try_into().unwrap();
                let value = c.u32()?;
                if k == E_MRC { Event::Mrc { op, value } } else { Event::Mcr { op, value } }
            }
            k @ (E_IO_READ | E_IO_WRITE) => {
                let (addr, width, value) = (c.u32()?, c.u8()?, c.u32()?);
                if k == E_IO_READ { Event::IoRead { addr, width, value } } else { Event::IoWrite { addr, width, value } }
            }
            k => return Err(format!("unknown event kind {k}")),
        });
    }
    let mut writes = Vec::new();
    for _ in 0..c.u32()? {
        let a = c.u32()?;
        let n = c.u32()? as usize;
        writes.push((a, c.take(n)?.to_vec()));
    }
    Ok(Call { slot, frame, flags, entry, back, exit, reads, events, writes })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f_a(_: &mut Cpu, _: &mut dyn crate::rt::Bus, _: u32) -> u32 { 1 }
    static TABLE: [Lifted; 1] = [Lifted { segment: "arm9_main", entry: 0x0200_0000, blocks: &[0x0200_0000], insts: &[0x0200_0000], f: f_a }];

    #[test]
    fn records_reads_before_writes_events_and_round_trips() {
        let dir = std::env::temp_dir().join(format!("mkds_capture_test_{}", std::process::id()));
        let mut cap = Capture::new(dir.clone(), vec![0, 100], &TABLE);
        let mut c = Cpu::default();
        c.r[0] = 0x0210_0000;
        let fp = f_a as crate::mem::LiftedFn as usize;
        let id = cap.start(0x0200_0000, fp, &c, 5, &Vec::new).unwrap();
        assert!(cap.start(0x0200_0000, fp, &c, 6, &Vec::new).is_none(), "slot 0 is taken");
        assert!(cap.start(0x0200_0004, fp, &c, 6, &Vec::new).is_none(), "not a function entry");
        cap.read(id, 0x0210_0000, 4, 0x1122_3344); // recorded
        cap.read(id, 0x0210_0001, 1, 0x33); // known, same value: not recorded
        cap.write(id, 0x0210_0004, 2, 0xBEEF);
        cap.read(id, 0x0210_0004, 4, 0x5566_BEEF); // bytes 6, 7 unknown: recorded
        cap.read(id, 0x0400_0280, 2, 7); // I/O: an event
        let ev = cap.call_begin(id, 0x0200_1000, 0x0200_0010, &c).unwrap();
        c.r[0] = 42;
        cap.call_end(id, ev, &c, 0x0200_0010);
        cap.read(id, 0x0210_0000, 4, 0x1122_3399); // the callee changed it: recorded again, no conflict
        cap.read(id, 0x0210_0000, 1, 0x98); // changed with no event in between: conflict
        cap.write(id, 0x0400_0000, 4, 1);
        cap.finish(id, &c, 0x0200_0010, 0x0200_0010);
        let id2 = cap.start(0x0200_0000, fp, &c, 150, &Vec::new).unwrap();
        cap.finish(id2, &c, 0x0200_0020, 0x0200_0010);
        cap.flush().unwrap();
        let data = std::fs::read(dir.join("arm9_main_02000000.cap")).unwrap();
        let (seg, entry, calls) = read_file(&data).unwrap();
        assert_eq!((seg.as_str(), entry, calls.len()), ("arm9_main", 0x0200_0000, 2));
        let k = &calls[0];
        assert_eq!(k.entry[0], 0x0210_0000);
        assert_eq!(k.exit[0], 42);
        assert_eq!(k.flags, F_CONFLICT);
        assert_eq!(k.reads.iter().map(|r| (r.after, r.addr, r.width)).collect::<Vec<_>>(), [(0, 0x0210_0000, 4), (0, 0x0210_0004, 4), (2, 0x0210_0000, 4), (2, 0x0210_0000, 1)]);
        assert_eq!(k.writes, [(0x0210_0004, vec![0xEF, 0xBE])]);
        assert_eq!(k.events.len(), 3);
        assert!(matches!(k.events[0], Event::IoRead { addr: 0x0400_0280, width: 2, value: 7 }));
        assert!(matches!(k.events[1], Event::Call { target: 0x0200_1000, back: 0x0200_0010, .. }));
        assert!(matches!(k.events[2], Event::IoWrite { addr: 0x0400_0000, .. }));
        assert_eq!((calls[1].slot, calls[1].flags), (1, F_ABNORMAL));
        drop(cap);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
