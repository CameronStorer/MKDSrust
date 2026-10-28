//! Fiber execution of the ARM9 (block 4A.2, `https://github.com/CameronStorer/MKDSrust-Docs`).
//!
//! The flat runtime returns to its run loop at every call, return and slice end, so a lifted
//! function must be resumable at any block. Readable (raised) code needs real Rust calls instead.
//! Here the guest runs inside fibers:
//! - **Calls nest.** [`Bus::call`] runs the callee in a nested dispatch loop until it returns to
//!   `ret` with the stack pointer of the call.
//! - **Slices end in place.** When a block cannot pay its budget, [`Bus::slice_end`] suspends the
//!   fiber; the host ticks the hardware and the ARM7, then resumes it. A pending IRQ is taken right
//!   there, as a nested run of the vector until the exception return comes back.
//! - **Each SDK thread has its own fiber.** When control reaches code of another thread (the
//!   SDK's current-thread pointer changed), the fiber asks the host to switch. The suspended fiber
//!   keeps its Rust frames and continues where its thread is resumed.
//!
//! Slice boundaries are exactly the flat runtime's (budget used up, code written, a dispatch miss),
//! so a run is instruction-for-instruction identical; `race_check` telemetry compares the two.
//!
//! Safety: the session is reached through a raw pointer that the host passes on every resume, and
//! the session must not move while fibers exist (checked). Host and guest never run at the same
//! time (one OS thread), so their accesses never overlap.
use super::Session;
use crate::rt::{self, Bus, Cpu, Mem};
use corosensei::stack::DefaultStack;
use corosensei::{Coroutine, CoroutineResult, Yielder};

/// AMCE revision 0: `OSi_CurrentThreadPtr`, which holds the address of the SDK's current-thread
/// field (`*(*0x0216FF34)` is the running `OSThread`). Found in OS_SleepThread / OS_ExitThread.
const CURRENT_THREAD_PTR: u32 = 0x0216_FF34;

/// Host stack size of one guest thread's fiber (address space; committed as used).
const FIBER_STACK: usize = 16 << 20;

/// What the host passes when it resumes a fiber.
pub(crate) enum In {
    /// Continue after a slice end.
    Go(*mut Session<'static>),
    /// The fiber's thread was switched to: continue from `pc`. `true`: this is the slice start
    /// (the first fiber of the session), where a pending IRQ is taken first.
    Resume(*mut Session<'static>, u32, bool),
}

/// Why a fiber suspended.
pub(crate) enum Out {
    /// The slice is over: tick the hardware, then resume this fiber with [`In::Go`].
    SliceEnd,
    /// Control reached thread `thread` at `pc`: resume that thread's fiber (or start one).
    Switch(u32, u32),
}

type Fiber = Coroutine<In, Out, (), DefaultStack>;

/// The host's side: one fiber per guest thread id (0: before the SDK's threads exist).
pub(crate) struct Fibers {
    fibers: Vec<(u32, Fiber)>,
    /// The thread whose fiber runs next.
    pub(crate) current: u32,
    /// `Some(pc)`: the current thread's fiber must be resumed (or started) at `pc`.
    resume_at: Option<u32>,
    /// The next resume is the session's very first (a slice start).
    first: bool,
    /// The session address the fibers were started with (it must not move).
    home: *mut Session<'static>,
    pub switches: u64,
    /// Diagnostics: where the last slice ended and why.
    pub last_end: (u32, &'static str),
}

impl Fibers {
    pub(crate) fn new(start_pc: u32) -> Self {
        Fibers { fibers: Vec::new(), current: 0, resume_at: Some(start_pc), first: true, home: std::ptr::null_mut(), switches: 0, last_end: (0, "") }
    }
}

/// Runs one slice: resumes the current thread's fiber until it ends the slice. The caller has
/// set the budget and does the accounting afterwards.
pub(crate) fn run_slice(s: &mut Session<'_>) -> Result<(), String> {
    let me = s as *mut Session<'_> as *mut Session<'static>;
    let fb = s.fibers.as_mut().ok_or("fiber runtime not enabled")?;
    if fb.home.is_null() {
        fb.home = me;
    } else if fb.home != me {
        return Err("the session moved while its fibers were suspended".into());
    }
    loop {
        // SAFETY: `me` is this session; the fiber reaches it only through the pointer it is given.
        let fb = unsafe { (*me).fibers.as_mut().unwrap() };
        let thread = fb.current;
        let input = match fb.resume_at.take() {
            Some(pc) => {
                if !fb.fibers.iter().any(|(t, _)| *t == thread) {
                    fb.fibers.push((thread, new_fiber(thread)));
                }
                In::Resume(me, pc, std::mem::take(&mut fb.first))
            }
            None => In::Go(me),
        };
        let fiber = &mut fb.fibers.iter_mut().find(|(t, _)| *t == thread).ok_or("no fiber for the current thread")?.1;
        match fiber.resume(input) {
            CoroutineResult::Yield(Out::SliceEnd) => {
                return match unsafe { (*me).fiber_error.take() } {
                    Some(e) => Err(e),
                    None => Ok(()),
                };
            }
            CoroutineResult::Yield(Out::Switch(t, pc)) => {
                let fb = unsafe { (*me).fibers.as_mut().unwrap() };
                fb.current = t;
                fb.resume_at = Some(pc);
                fb.switches += 1;
            }
            CoroutineResult::Return(()) => return Err("a guest thread's fiber ended".into()),
        }
    }
}

fn new_fiber(thread: u32) -> Fiber {
    let stack = DefaultStack::new(FIBER_STACK).expect("cannot allocate a fiber stack");
    // SAFETY: the closure captures nothing; the session comes in through `In`.
    unsafe {
        Coroutine::with_stack_unchecked(stack, move |y: &Yielder<In, Out>, input: In| {
            let (s, pc, start) = match input {
                In::Resume(s, pc, start) => (s, pc, start),
                In::Go(_) => unreachable!("a fiber starts with Resume"),
            };
            let mut g = Guest { s, y: y as *const _, thread, loops: Vec::new(), fresh: false };
            let pc = if start { g.end_slice_resumed(pc) } else { pc };
            g.run(pc, None);
            unreachable!("the root loop never returns")
        })
    }
}

/// One fiber's guest side.
struct Guest {
    s: *mut Session<'static>,
    y: *const Yielder<In, Out>,
    /// The guest thread this fiber runs.
    thread: u32,
    /// Active nested loops on this fiber: (return pc, stack pointer at the call).
    loops: Vec<(u32, u32)>,
    /// A slice just started: the next dispatch uses the residency-checking resolver.
    fresh: bool,
}

impl Guest {
    fn s(&mut self) -> &mut Session<'static> {
        // SAFETY: see the module notes.
        unsafe { &mut *self.s }
    }

    fn cpu(&mut self) -> &mut Cpu {
        unsafe { &mut (*self.s).cpu }
    }

    fn suspend(&mut self, out: Out) -> In {
        // SAFETY: the yielder belongs to the fiber this code runs on.
        unsafe { (*self.y).suspend(out) }
    }

    /// The running SDK thread (`OSThread` address), 0 before the SDK set one up.
    fn current_thread(&mut self) -> u32 {
        let hw = &mut self.s().machine.hw;
        let p = hw.r32(CURRENT_THREAD_PTR);
        if !(0x0200_0000..0x0240_0000).contains(&p) {
            return 0;
        }
        let t = hw.r32(p);
        if (0x0200_0000..0x0240_0000).contains(&t) { t } else { 0 }
    }

    /// Ends the slice: suspend until the host has ticked the hardware, then take a pending IRQ
    /// before `pc`. Returns `pc` (where execution continues).
    fn end_slice(&mut self, pc: u32, why: &'static str) -> u32 {
        self.s().fibers.as_mut().unwrap().last_end = (pc, why);
        match self.suspend(Out::SliceEnd) {
            In::Go(s) => self.s = s,
            In::Resume(..) => unreachable!("a slice end is resumed with Go"),
        }
        if !self.irq_taken(pc) {
            // the flat runtime starts every slice with the residency-checking resolver at its pc
            if self.resolve(pc).is_none() {
                let e = format!("No lifted ARM9 code at PC {pc:#010x}: {}", self.s().dispatcher.describe(pc));
                self.s().fiber_error = Some(e);
            }
        }
        pc
    }

    /// The start of a slice at `pc` (also the session's very first): IRQ or resolver, as the flat
    /// runtime's slice start.
    fn end_slice_resumed(&mut self, pc: u32) -> u32 {
        if !self.irq_taken(pc) && self.resolve(pc).is_none() {
            let e = format!("No lifted ARM9 code at PC {pc:#010x}: {}", self.s().dispatcher.describe(pc));
            self.s().fiber_error = Some(e);
        }
        pc
    }

    /// Takes an IRQ before `pc` if one is ready (as the flat runtime does at a slice start) and runs
    /// the handler until it returns to `pc`. `true` if an IRQ was taken.
    fn irq_taken(&mut self, pc: u32) -> bool {
        // with native OS time (`MKDS_HW_TIME=native`) only at a safe point (`os_hle::IrqGate`)
        if !self.s().irq_allowed(pc) {
            return false;
        }
        if self.s().machine.hw.probe.is_some() {
            let hw = &mut self.s().machine.hw;
            let pending = hw.r32(0x0400_0210) & hw.r32(0x0400_0214);
            hw.probe.as_mut().unwrap().irq(pending);
            if pending & 1 != 0 {
                self.s().machine.oracle_wait(crate::machine::ORACLE_VBLANK_IRQ);
            }
        }
        let sp = self.cpu().r[13];
        let vector = rt::take_irq(self.cpu(), pc, 0xffff_0000);
        self.s().stats.irqs += 1;
        self.fresh = true; // the vector is the slice's first dispatch
        self.run(vector, Some((pc, sp)));
        true
    }

    /// The dispatch loop: runs from `pc` until control returns to `until` (`None`: forever).
    /// Returns the pc that ended it: `until`'s return pc, or the return pc of an outer loop that a
    /// non-local exit reached (the frames in between are resumable translations and pass it on).
    fn run(&mut self, mut pc: u32, until: Option<(u32, u32)>) -> u32 {
        if let Some(u) = until {
            self.loops.push(u);
        }
        let out = loop {
            let sp = self.cpu().r[13];
            if until.is_some_and(|(ret, at)| pc == ret && sp == at) {
                let s = self.s();
                if s.machine.hw.code_epoch != s.code_epoch || s.dispatcher.get(pc).is_none() {
                    // the flat loop ends the slice here: code was written, or the caller's block is
                    // not mapped (overlays invalidated); the next slice re-verifies at `pc`
                    pc = self.end_slice(pc, "return: code written or unmapped");
                    continue;
                }
                break pc;
            }
            let t = self.current_thread();
            if t != self.thread {
                if self.thread == 0 {
                    // the SDK made the running code a thread: this fiber becomes that thread's
                    let fb = self.s().fibers.as_mut().unwrap();
                    for f in fb.fibers.iter_mut().filter(|f| f.0 == 0) {
                        f.0 = t;
                    }
                    fb.current = t;
                    self.thread = t;
                } else {
                    pc = match self.suspend(Out::Switch(t, pc)) {
                        In::Resume(s, pc, _) => {
                            self.s = s;
                            pc
                        }
                        In::Go(_) => unreachable!("a switched-out fiber is resumed with Resume"),
                    };
                    continue;
                }
            }
            let depth = self.loops.len().saturating_sub(until.is_some() as usize);
            if self.loops[..depth].iter().any(|&(ret, at)| pc == ret && sp == at) {
                break pc;
            }
            let fresh = self.fresh;
            let s = self.s();
            let d = s.dispatcher;
            if !fresh && (s.cpu.budget == 0 || s.machine.hw.code_epoch != s.code_epoch || d.get(pc).is_none()) {
                let why = if s.cpu.budget == 0 { "budget" } else if s.machine.hw.code_epoch != s.code_epoch { "code written" } else { "miss" };
                pc = self.end_slice(pc, why);
                continue;
            }
            // the flat loop's place too: after the slice-end checks, before the block runs
            if let Some(g) = s.machine.os_gate.as_mut() {
                g.entry(pc, s.cpu.r[0]);
            }
            if let Some(p) = s.machine.tick_pin.as_mut() {
                p.entry(pc, &mut s.cpu);
            }
            let Some(f) = (if std::mem::take(&mut self.fresh) { self.resolve(pc) } else { d.get(pc) }) else {
                let e = format!("No lifted ARM9 code at PC {pc:#010x}: {}", d.describe(pc));
                self.s().fiber_error = Some(e);
                self.s().cpu.budget = 0;
                pc = self.end_slice(pc, "no code");
                continue;
            };
            let me: *mut Guest = self;
            let mut bus = GuestBus { g: me };
            pc = f(self.cpu(), &mut bus, pc);
            if pc == rt::BUDGET_EXHAUSTED {
                // a native function could not pay at its entry: the slice ends there
                pc = self.cpu().r[15];
            }
        };
        if until.is_some() {
            self.loops.pop();
        }
        out
    }

    /// The first dispatch of `pc` after a slice end uses the residency-checking resolver, like the
    /// flat runtime's slice start (it may verify an overlay that was just loaded).
    fn resolve(&mut self, pc: u32) -> Option<crate::mem::LiftedFn> {
        let s = self.s();
        let found = {
            let hw = &s.machine.hw;
            s.dispatcher.resolve(pc, &|address, bytes| hw.peek_into(address, bytes))
        };
        if found.is_some() && s.dispatcher.resident_len() != s.resident_overlays.len() {
            // the miss path just verified a freshly loaded overlay
            s.resident_overlays = s.dispatcher.resident();
        }
        found
    }
}

/// The bus lifted code sees inside a fiber.
struct GuestBus {
    g: *mut Guest,
}

impl GuestBus {
    fn g(&mut self) -> &mut Guest {
        unsafe { &mut *self.g }
    }
    fn m(&mut self) -> &mut crate::machine::Machine<'static> {
        unsafe { &mut (*(*self.g).s).machine }
    }
}

impl Mem for GuestBus {
    fn r8(&mut self, a: u32) -> u8 {
        self.m().r8(a)
    }
    fn r16(&mut self, a: u32) -> u16 {
        self.m().r16(a)
    }
    fn r32(&mut self, a: u32) -> u32 {
        self.m().r32(a)
    }
    fn w8(&mut self, a: u32, v: u8) {
        self.m().w8(a, v)
    }
    fn w16(&mut self, a: u32, v: u16) {
        self.m().w16(a, v)
    }
    fn w32(&mut self, a: u32, v: u32) {
        self.m().w32(a, v)
    }
}

impl Bus for GuestBus {
    fn call(&mut self, c: &mut Cpu, target: u32, ret: u32) -> u32 {
        let sp = c.r[13];

        let pc = self.g().run(target, Some((ret, sp)));
        // test-only `MKDS_TICK_PIN`: a pinned OS_GetTick call returns here (not through a dispatch)
        let s = self.g().s();
        if let Some(p) = s.machine.tick_pin.as_mut() {
            p.returned(pc, &mut s.cpu);
        }
        pc
    }
    fn swi(&mut self, c: &mut Cpu, n: u32) {
        self.m().swi(c, n)
    }
    fn mrc(&mut self, c: &mut Cpu, cp: u8, o1: u8, n: u8, cm: u8, o2: u8) -> u32 {
        self.m().mrc(c, cp, o1, n, cm, o2)
    }
    fn mcr(&mut self, c: &mut Cpu, cp: u8, o1: u8, n: u8, cm: u8, o2: u8, v: u32) {
        self.m().mcr(c, cp, o1, n, cm, o2, v)
    }
    fn slice_end(&mut self, _c: &mut Cpu, pc: u32, cost: u32) -> bool {
        // `_c` must not be used after a suspension: the host changed the CPU meanwhile (budget,
        // IRQ entry), and the compiler may assume a `&mut` argument is not changed behind it. The
        // session pointer is read afresh instead.
        loop {
            self.g().end_slice(pc, "block budget");
            if rt::spend(self.g().cpu(), cost) {
                return true;
            }
        }
    }
}
