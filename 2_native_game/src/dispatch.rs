//! Dispatcher: runs guest code from any pc. Lifted functions are found by block start. Any other
//! pc (code that was not lifted, a computed jump into the middle of a function) runs on the
//! interpreter until it reaches a lifted block again when the `interpreter` feature is on (tests,
//! diagnostics); without it such a pc is an error naming the pc and its segment. A machine's
//! `Bus::call` uses [`Dispatcher::call`].
#[cfg(feature = "interpreter")]
use crate::interp;
use crate::mem::{Lifted, LiftedFn};
use crate::native::Native;
use crate::residency::SegmentSig;
use crate::rt::{Bus, Cpu, BUDGET_EXHAUSTED};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};

/// Fast hasher for block addresses (SipHash showed up in profiles: the map is probed on every
/// dispatch and every call). Keys are plain u32 code addresses, not attacker-controlled.
#[derive(Default)]
pub struct AddrHasher(u64);

impl Hasher for AddrHasher {
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0.rotate_left(5) ^ b as u64).wrapping_mul(0x517c_c1b7_2722_0a95);
        }
    }
    fn write_u32(&mut self, x: u32) {
        self.0 = (self.0.rotate_left(5) ^ x as u64).wrapping_mul(0x517c_c1b7_2722_0a95);
    }
    fn finish(&self) -> u64 {
        // fold the high bits (where the multiply puts the entropy) into the bucket-selecting low bits
        self.0 ^ (self.0 >> 32)
    }
}

type BlockMap = HashMap<u32, LiftedFn, BuildHasherDefault<AddrHasher>>;

/// Block lookup. Non-overlay segments (main image, autoloads) live in `fixed`, changed only by
/// [`Dispatcher::set_loaded`]; overlays share guest addresses, so only the resident ones are in
/// `overlay`, which is rebuilt when the resident set changes (not on every refresh).
pub struct Dispatcher {
    table: &'static [Lifted],
    fixed: RefCell<BlockMap>,
    overlay: RefCell<BlockMap>,
    resident: RefCell<Vec<&'static str>>,
    /// Overlay signatures and the address window each covers ([`Dispatcher::configure_residency`]).
    sigs: Cell<&'static [SegmentSig]>,
    windows: RefCell<Vec<(u32, u64)>>,
    /// Bumped by every [`Dispatcher::invalidate_overlays`]; `miss_refresh` is the generation in which
    /// a dispatch miss inside an overlay window last re-verified residency (once per generation).
    generation: Cell<u64>,
    miss_refresh: Cell<u64>,
    /// Instructions the interpreter ran inside [`Dispatcher::call`] (the caller's budget paid them).
    interpreted: Cell<u64>,
    /// Why execution had to stop inside [`Dispatcher::call`] (a miss without the interpreter).
    error: RefCell<Option<String>>,
    /// Native functions installed over their lifted entry blocks ([`Dispatcher::set_natives`]), and the
    /// non-overlay segments last loaded (to rebuild `fixed` when the natives change).
    natives: Cell<&'static [Native]>,
    fixed_loaded: RefCell<Vec<&'static str>>,
}

impl Dispatcher {
    /// Code of the segments in `loaded` (`arm9_main`, `arm9_autoload1_itcm`, and the overlays
    /// currently in memory: overlays share addresses, so only loaded ones may be dispatched to).
    pub fn with_loaded(table: &'static [Lifted], loaded: &[&str]) -> Self {
        let d = Dispatcher {
            table,
            fixed: RefCell::new(BlockMap::default()),
            overlay: RefCell::new(BlockMap::default()),
            resident: RefCell::new(Vec::new()),
            sigs: Cell::new(&[]),
            windows: RefCell::new(Vec::new()),
            generation: Cell::new(0),
            miss_refresh: Cell::new(u64::MAX),
            interpreted: Cell::new(0),
            error: RefCell::new(None),
            natives: Cell::new(&[]),
            fixed_loaded: RefCell::new(Vec::new()),
        };
        d.set_loaded(loaded);
        d
    }

    /// Every segment in the table (on shared overlay addresses the first listed wins); use
    /// [`Dispatcher::with_loaded`] once overlay loading is tracked.
    pub fn new(table: &'static [Lifted]) -> Self {
        let mut segs: Vec<&str> = table.iter().map(|l| l.segment).collect();
        segs.dedup();
        Self::with_loaded(table, &segs)
    }

    fn build(&self, loaded: &[&str], overlays: bool) -> BlockMap {
        let mut map = BlockMap::default();
        for l in self.table.iter().filter(|l| crate::residency::is_overlay(l.segment) == overlays && loaded.contains(&l.segment)) {
            for &b in l.blocks {
                map.entry(b).or_insert(l.f);
            }
        }
        for n in self.natives.get().iter().filter(|n| crate::residency::is_overlay(n.segment) == overlays && loaded.contains(&n.segment)) {
            map.insert(n.entry, n.f);
        }
        map
    }

    /// The lifted functions this dispatcher was made from.
    pub fn table(&self) -> &'static [Lifted] {
        self.table
    }

    /// The translation of the function at `entry` (bit 0 = Thumb) in `segment`, ignoring natives.
    pub fn translation(&self, segment: &str, entry: u32) -> Option<LiftedFn> {
        self.table.iter().find(|l| l.segment == segment && l.entry == entry).map(|l| l.f)
    }

    /// Runs `natives` instead of the lifted code at their entries (each in its own segment only, and an
    /// overlay's only while that overlay is resident). `&[]` restores the lifted code everywhere.
    pub fn set_natives(&self, natives: &'static [Native]) {
        self.natives.set(natives);
        let fixed = self.fixed_loaded.borrow().clone();
        *self.fixed.borrow_mut() = self.build(&fixed, false);
        let resident = self.resident.borrow().clone();
        *self.overlay.borrow_mut() = self.build(&resident, true);
    }

    /// Replaces the dispatchable segments with `loaded`.
    pub fn set_loaded(&self, loaded: &[&str]) {
        *self.fixed_loaded.borrow_mut() = self.segment_names(loaded.iter().copied().filter(|s| !crate::residency::is_overlay(s)));
        *self.fixed.borrow_mut() = self.build(loaded, false);
        *self.overlay.borrow_mut() = self.build(loaded, true);
        *self.resident.borrow_mut() = self.segment_names(loaded.iter().copied().filter(|s| crate::residency::is_overlay(s)));
    }

    /// The table's own (`'static`) names for `names`, in table order, without duplicates.
    fn segment_names<'n>(&self, names: impl Iterator<Item = &'n str>) -> Vec<&'static str> {
        let names: Vec<&str> = names.collect();
        let mut out: Vec<&'static str> = Vec::new();
        for l in self.table {
            if names.contains(&l.segment) && !out.contains(&l.segment) {
                out.push(l.segment);
            }
        }
        out
    }

    /// Overlay signatures (`lifted::SEGMENT_SIGS`) for [`Dispatcher::resolve`]: a dispatch miss inside
    /// one of their address windows re-verifies residency before giving up, so an overlay the game
    /// has just loaded runs lifted at once instead of waiting for the host's next periodic check.
    pub fn configure_residency(&self, sigs: &'static [SegmentSig]) {
        self.sigs.set(sigs);
        *self.windows.borrow_mut() = sigs
            .iter()
            .filter(|s| crate::residency::is_overlay(s.segment))
            .filter_map(|s| {
                let lo = s.ranges.iter().map(|r| r.start).min()?;
                let hi = s.ranges.iter().map(|r| r.start as u64 + r.len as u64).max()?;
                Some((lo, hi))
            })
            .collect();
        self.invalidate_overlays();
    }

    /// Dispatch only the non-overlay segments now (main image, autoloads); overlays are not
    /// dispatched until [`Dispatcher::refresh_residency`] verifies them. Call when guest writes touch
    /// the overlay window so code of a swapped-out overlay can never run at a reused address. Cheap:
    /// only the overlay map is cleared.
    pub fn invalidate_overlays(&self) {
        self.generation.set(self.generation.get().wrapping_add(1));
        if !self.resident.borrow().is_empty() {
            self.overlay.borrow_mut().clear();
            self.resident.borrow_mut().clear();
        }
    }

    /// The overlays currently dispatched.
    pub fn resident(&self) -> Vec<&'static str> {
        self.resident.borrow().clone()
    }

    /// How many overlays are currently dispatched (no allocation).
    pub fn resident_len(&self) -> usize {
        self.resident.borrow().len()
    }

    /// Instructions interpreted inside [`Dispatcher::call`] so far (included in the budget the
    /// calling lifted code reports as used).
    pub fn interpreted(&self) -> u64 {
        self.interpreted.get()
    }

    /// The error that stopped execution inside [`Dispatcher::call`], if any (taken once).
    pub fn take_error(&self) -> Option<String> {
        self.error.borrow_mut().take()
    }

    /// The lifted function for block `pc`. On a miss inside an overlay window, residency is verified
    /// again (at most once per [`Dispatcher::invalidate_overlays`] generation) and the lookup retried.
    /// `read` fills a buffer from guest memory without side effects.
    pub fn resolve(&self, pc: u32, read: &dyn Fn(u32, &mut [u8]) -> bool) -> Option<LiftedFn> {
        if let Some(f) = self.get(pc) {
            return Some(f);
        }
        let a = (pc & !1) as u64;
        if self.miss_refresh.get() == self.generation.get()
            || !self.windows.borrow().iter().any(|&(lo, hi)| a >= lo as u64 && a < hi)
        {
            return None;
        }
        self.miss_refresh.set(self.generation.get());
        self.refresh_residency(self.sigs.get(), read);
        self.get(pc)
    }

    /// [`Dispatcher::resolve`] reading guest memory through the bus (main RAM reads have no side effects).
    fn resolve_bus(&self, pc: u32, m: &mut dyn Bus) -> Option<LiftedFn> {
        if let Some(f) = self.get(pc) {
            return Some(f);
        }
        let m = RefCell::new(m);
        self.resolve(pc, &|a, buf: &mut [u8]| {
            let mut m = m.borrow_mut();
            for (i, b) in buf.iter_mut().enumerate() {
                *b = m.r8(a.wrapping_add(i as u32));
            }
            true
        })
    }

    /// Where `pc` is, for error messages: the lifted function containing it (it is not a block
    /// start), else the segment whose address span holds it.
    pub fn describe(&self, pc: u32) -> String {
        let resident = self.resident.borrow();
        let state = |seg: &str| {
            if !crate::residency::is_overlay(seg) {
                ""
            } else if resident.contains(&seg) {
                ", overlay resident"
            } else {
                ", overlay not verified in memory"
            }
        };
        // prefer code that is in memory now (overlays share addresses)
        let live = |seg: &str| !crate::residency::is_overlay(seg) || resident.contains(&seg);
        let containing = || self.table.iter().filter(|l| l.insts.contains(&pc));
        if let Some(l) = containing().find(|l| live(l.segment)).or_else(|| containing().next()) {
            return format!("inside lifted function {}_{:08X} but not one of its block starts (segment {}{})", l.segment, l.entry & !1, l.segment, state(l.segment));
        }
        let mut spans: Vec<(&'static str, u32, u32)> = Vec::new();
        for l in self.table {
            let lo = l.insts.iter().chain([&l.entry]).map(|&a| a & !1).min().unwrap_or(l.entry);
            let hi = l.insts.iter().map(|&a| a & !1).max().unwrap_or(l.entry);
            match spans.iter_mut().find(|s| s.0 == l.segment) {
                Some(s) => {
                    s.1 = s.1.min(lo);
                    s.2 = s.2.max(hi);
                }
                None => spans.push((l.segment, lo, hi)),
            }
        }
        let hits: Vec<String> = spans
            .iter()
            .filter(|s| (s.1..=s.2.saturating_add(3)).contains(&(pc & !1)))
            .map(|s| format!("{}{}", s.0, state(s.0)))
            .collect();
        if hits.is_empty() {
            "outside every lifted segment".into()
        } else {
            format!("not lifted code within segment {}", hits.join(" / "))
        }
    }

    /// Verifies which overlays are in guest memory (see [`crate::residency`]) and dispatches
    /// exactly those, plus the non-overlay segments. Returns the resident overlay names. The block
    /// map is rebuilt only when the resident set changed.
    pub fn refresh_residency(
        &self,
        sigs: &'static [crate::residency::SegmentSig],
        read: &dyn Fn(u32, &mut [u8]) -> bool,
    ) -> Vec<&'static str> {
        let resident = crate::residency::resident_overlays(sigs, read);
        let unchanged = {
            let cur = self.resident.borrow();
            cur.len() == resident.len() && cur.iter().zip(&resident).all(|(a, b)| a == b)
        };
        if !unchanged {
            *self.overlay.borrow_mut() = self.build(&resident, true);
            *self.resident.borrow_mut() = resident.clone();
        }
        resident
    }

    /// The lifted function containing block `pc`, if any (for flat run loops).
    #[inline]
    pub fn get(&self, pc: u32) -> Option<LiftedFn> {
        if let Some(&f) = self.fixed.borrow().get(&pc) {
            return Some(f);
        }
        self.overlay.borrow().get(&pc).copied()
    }

    #[inline]
    pub fn is_lifted(&self, pc: u32) -> bool {
        self.fixed.borrow().contains_key(&pc) || self.overlay.borrow().contains_key(&pc)
    }

    /// Runs from `target` until control returns to `ret` with the stack pointer back where it was
    /// at the call (so a recursive callee passing the same address does not end the call early).
    /// Returns `ret`, or `BUDGET_EXHAUSTED`.
    pub fn call(&self, c: &mut Cpu, m: &mut dyn Bus, target: u32, ret: u32) -> u32 {
        let sp = c.r[13];
        let done = |pc: u32, c: &Cpu| pc == ret && c.r[13] == sp;
        let mut pc = target;
        loop {
            if pc == BUDGET_EXHAUSTED || done(pc, c) {
                return pc;
            }
            let lifted = self.resolve_bus(pc, m);
            pc = match lifted {
                Some(f) => f(c, m, pc),
                None => self.miss(c, m, pc, ret, sp),
            };
        }
    }

    /// A call reached `pc`, which has no lifted block: interpret until a lifted block or the return
    /// (counted in [`Dispatcher::interpreted`]).
    #[cfg(feature = "interpreter")]
    fn miss(&self, c: &mut Cpu, m: &mut dyn Bus, pc: u32, ret: u32, sp: u32) -> u32 {
        let budget = c.budget;
        let mut p = pc;
        let next = loop {
            p = interp::run_until(c, m, p, &|q| q == ret || self.is_lifted(q));
            if p != ret || c.r[13] == sp || p == BUDGET_EXHAUSTED || self.is_lifted(p) {
                break p;
            }
            // the return address reached by a deeper frame: keep interpreting
            let d = interp::fetch(m, p);
            if c.budget == 0 {
                break BUDGET_EXHAUSTED;
            }
            c.budget -= 1;
            p = interp::exec(&d, p, c, m);
        };
        self.interpreted.set(self.interpreted.get() + budget.saturating_sub(c.budget) as u64);
        next
    }

    /// Without the interpreter a miss stops execution: the reason is kept for [`Dispatcher::take_error`]
    /// and the budget is emptied so the run loop regains control at once.
    #[cfg(not(feature = "interpreter"))]
    fn miss(&self, c: &mut Cpu, _m: &mut dyn Bus, pc: u32, _ret: u32, _sp: u32) -> u32 {
        let mut e = self.error.borrow_mut();
        if e.is_none() {
            *e = Some(format!("No lifted code at PC {pc:#010x} (called): {}", self.describe(pc)));
        }
        c.r[15] = pc;
        c.budget = 0;
        BUDGET_EXHAUSTED
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::residency::{fnv1a64, CodeRange, SegmentSig};

    fn f_a(_: &mut Cpu, _: &mut dyn Bus, _: u32) -> u32 { 0xA }
    fn f_b(_: &mut Cpu, _: &mut dyn Bus, _: u32) -> u32 { 0xB }
    fn f_main(_: &mut Cpu, _: &mut dyn Bus, _: u32) -> u32 { 0xC }

    static TABLE: [Lifted; 3] = [
        Lifted { segment: "arm9_main", entry: 0x0200_0000, blocks: &[0x0200_0000], insts: &[0x0200_0000], f: f_main },
        Lifted { segment: "ov9_000", entry: 0x0210_0000, blocks: &[0x0210_0000], insts: &[0x0210_0000], f: f_a },
        Lifted { segment: "ov9_001", entry: 0x0210_0000, blocks: &[0x0210_0000], insts: &[0x0210_0000], f: f_b },
    ];

    fn sigs() -> &'static [SegmentSig] {
        let mk = |bytes: &[u8]| -> &'static [CodeRange] {
            Box::leak(Box::new([CodeRange { start: 0x0210_0000, len: bytes.len() as u32, hash: fnv1a64(bytes) }]))
        };
        Box::leak(Box::new([
            SegmentSig { segment: "ov9_000", ranges: mk(&[1, 1, 1, 1]) },
            SegmentSig { segment: "ov9_001", ranges: mk(&[2, 2, 2, 2]) },
        ]))
    }

    #[test]
    fn residency_selects_the_overlay_in_ram_and_invalidation_drops_it() {
        let d = Dispatcher::with_loaded(&TABLE, &["arm9_main"]);
        let mut cpu = Cpu::default();
        struct NoBus;
        let _ = NoBus;
        let ram = |bytes: &'static [u8]| move |_: u32, buf: &mut [u8]| { buf.copy_from_slice(bytes); true };
        assert!(d.get(0x0210_0000).is_none(), "overlays are not dispatched until verified");
        assert_eq!(d.refresh_residency(sigs(), &ram(&[2, 2, 2, 2])), ["ov9_001"]);
        let mut bus = crate::mem::Ram::new(&[], 0, 0);
        assert_eq!(d.get(0x0210_0000).unwrap()(&mut cpu, &mut bus, 0x0210_0000), 0xB);
        // overlay swapped: invalidate first (stale code must not run), then re-verify
        d.invalidate_overlays();
        assert!(d.get(0x0210_0000).is_none());
        assert!(d.get(0x0200_0000).is_some(), "main image stays dispatchable");
        assert_eq!(d.refresh_residency(sigs(), &ram(&[1, 1, 1, 1])), ["ov9_000"]);
        assert_eq!(d.get(0x0210_0000).unwrap()(&mut cpu, &mut bus, 0x0210_0000), 0xA);
        assert!(d.refresh_residency(sigs(), &ram(&[1, 2, 1, 2])).is_empty());
        assert!(d.get(0x0210_0000).is_none());
    }

    #[cfg(feature = "interpreter")]
    /// A bus whose calls go through the dispatcher (as `Machine::call` does).
    struct CallBus<'a> {
        ram: crate::mem::Ram<'static>,
        d: &'a Dispatcher,
    }
    #[cfg(feature = "interpreter")]
    impl crate::rt::Mem for CallBus<'_> {
        fn r8(&mut self, a: u32) -> u8 { self.ram.r8(a) }
        fn r16(&mut self, a: u32) -> u16 { self.ram.r16(a) }
        fn r32(&mut self, a: u32) -> u32 { self.ram.r32(a) }
        fn w8(&mut self, a: u32, v: u8) { self.ram.w8(a, v) }
        fn w16(&mut self, a: u32, v: u16) { self.ram.w16(a, v) }
        fn w32(&mut self, a: u32, v: u32) { self.ram.w32(a, v) }
    }
    #[cfg(feature = "interpreter")]
    impl Bus for CallBus<'_> {
        fn call(&mut self, c: &mut Cpu, target: u32, ret: u32) -> u32 {
            let d = self.d;
            d.call(c, self, target, ret)
        }
        fn swi(&mut self, c: &mut Cpu, n: u32) { self.ram.swi(c, n) }
        fn mrc(&mut self, c: &mut Cpu, cp: u8, o1: u8, n: u8, cm: u8, o2: u8) -> u32 { self.ram.mrc(c, cp, o1, n, cm, o2) }
        fn mcr(&mut self, c: &mut Cpu, cp: u8, o1: u8, n: u8, cm: u8, o2: u8, v: u32) { self.ram.mcr(c, cp, o1, n, cm, o2, v) }
    }

    #[cfg(feature = "interpreter")]
    fn f_ov(c: &mut Cpu, _: &mut dyn Bus, _: u32) -> u32 {
        crate::rt::spend(c, 1);
        c.r[1] = 0xAB;
        c.r[14]
    }
    #[cfg(feature = "interpreter")]
    static CALL_TABLE: [Lifted; 1] = [Lifted { segment: "ov9_000", entry: 0x0210_0000, blocks: &[0x0210_0000], insts: &[0x0210_0000], f: f_ov }];

    #[test]
    #[cfg(feature = "interpreter")]
    fn a_call_into_a_freshly_loaded_overlay_verifies_it_once_and_runs_lifted() {
        use crate::rt::Mem;
        let code = 0xE12F_FF1Eu32; // bx lr
        let sigs: &'static [SegmentSig] = Box::leak(Box::new([SegmentSig {
            segment: "ov9_000",
            ranges: Box::leak(Box::new([CodeRange { start: 0x0210_0000, len: 4, hash: fnv1a64(&code.to_le_bytes()) }])),
        }]));
        let d = Dispatcher::with_loaded(&CALL_TABLE, &[]);
        d.configure_residency(sigs);
        let mut bus = CallBus { ram: crate::mem::Ram::new(&[], 0, 1), d: &d };
        bus.w32(0x0210_0000, code);
        let mut c = Cpu::default();
        c.budget = 100;
        c.r[14] = 0x0200_0004;
        assert_eq!(bus.call(&mut c, 0x0210_0000, 0x0200_0004), 0x0200_0004);
        assert_eq!(c.r[1], 0xAB, "ran the lifted overlay function, not the interpreter");
        assert_eq!(d.interpreted(), 0);
        assert_eq!(d.resident(), ["ov9_000"]);
        // a miss outside every overlay window does not re-verify, and is counted when interpreted
        d.invalidate_overlays();
        bus.w32(0x0200_2000, 0xE3A0_0042); // mov r0, #0x42
        bus.w32(0x0200_2004, code);
        c.r[14] = 0x0200_0008;
        assert_eq!(bus.call(&mut c, 0x0200_2000, 0x0200_0008), 0x0200_0008);
        assert_eq!(c.r[0], 0x42);
        assert_eq!(d.interpreted(), 2);
        assert!(d.resident().is_empty());
        assert!(d.describe(0x0200_2000).contains("outside every lifted segment"));
    }

    #[test]
    fn describe_names_the_function_and_overlay_state() {
        let d = Dispatcher::with_loaded(&TABLE, &["arm9_main"]);
        assert!(d.describe(0x0210_0000).contains("inside lifted function ov9_000_02100000"));
        assert!(d.describe(0x0210_0000).contains("overlay not verified in memory"));
    }
}
