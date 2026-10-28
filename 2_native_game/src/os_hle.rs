//! Block 5.3: OS time and interrupts on the host (`MKDS_HW_TIME=native`; the default `exact` model is
//! unchanged). See `4_docs/BLOCK5_PLAN.md` ("5.3") and `4_docs/BLOCK5_CENSUS.md`.
//!
//! Two parts, both off unless the switch is set:
//! - [`VirtualTime`] owns timers 0 and 1, the two the SDK uses: TM0 is the `OS_GetTick` counter (its
//!   overflow IRQ counts the high bits), TM1 is the `OS_SetAlarm` timer (the alarm code programs it for the
//!   head of its queue and checks `OS_GetTick` in the IRQ). Their counters no longer follow the instruction
//!   count. Virtual time is in bus cycles: each V-blank sets it to at least `frames * FRAME_CYCLES` (a
//!   deterministic per-frame base), every counter read adds [`PER_READ`], and a wait with nothing pending
//!   jumps it to the next timer overflow inside the current frame. Overflows set their IF bit when virtual
//!   time passes them, as the hardware would, so the SDK's own `OS_GetTick`/alarm code runs unchanged and
//!   sees consistent values; delivery of the IRQ is the [`IrqGate`]'s decision.
//! - [`IrqGate`]: IRQs are delivered only at safe points: waits (CP15 halt, BIOS `Swi::Wait`), the
//!   critical-section exits that re-enable IRQs (`OS_EnableInterrupts`, `OS_RestoreInterrupts`), polls in
//!   the busy-wait loops the census found ([`SAFE_LOOPS`]), and a watchdog (`MKDS_WATCHDOG`, million
//!   instructions) that reports any other loop holding a pending IRQ back instead of hanging.
//!   While the budget still exists (until 5.4) the IRQ is taken at the first slice start after the
//!   safe point; the gate stays open while IRQs are pending and closes once nothing is.
//!
//! DMA, GX FIFO and IPC IRQs keep being raised by the hardware model when they happen (DMA is
//! immediate); the gate defers their delivery to the next safe point. IPC stays with the ARM7 model
//! until the ARM7 HLE lane lands.

use std::sync::OnceLock;

/// `MKDS_HW_TIME=native` (read once per process).
pub fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("MKDS_HW_TIME").as_deref() == Ok("native"))
}

/// Bus cycles per frame (263 lines of 2130 cycles), as `Session::FRAME_CYCLES`.
pub const FRAME_CYCLES: u64 = 263 * 2130;

/// Virtual bus cycles each read of a TM0/TM1 counter adds: one tick of the SDK's /64 prescaler, so
/// consecutive `OS_GetTick` calls differ and tick-bounded busy loops end.
pub const PER_READ: u64 = 64;

/// One virtual timer (TM0 or TM1).
#[derive(Clone, Copy, Default, Debug)]
struct VTimer {
    on: bool,
    irq: bool,
    /// Prescaler in bus cycles (1, 64, 256, 1024).
    div: u64,
    reload: u16,
    /// Virtual time of the next overflow (valid while `on`).
    next: u64,
}

impl VTimer {
    /// Bus cycles of one full period from `reload`.
    fn period(&self) -> u64 {
        (0x1_0000 - self.reload as u64) * self.div
    }
}

/// Virtual OS time: timers 0 and 1 (see the module notes). Held by `ds_mem::Bus::vtime`.
#[derive(Clone, Debug, Default)]
pub struct VirtualTime {
    /// Virtual bus cycles since power-on.
    pub now: u64,
    /// V-blanks seen (the frame index of `now`'s base).
    pub frames: u64,
    timers: [VTimer; 2],
    /// Diagnostics: counter reads, wait jumps, overflows raised.
    pub reads: u64,
    pub jumps: u64,
    pub overflows: [u64; 2],
}

impl VirtualTime {
    pub fn new() -> Self {
        Self::default()
    }

    /// IF bits for every overflow that `now` has passed (timer `t` = bit 3 + t); reloads the counters.
    fn catch_up(&mut self) -> u32 {
        let mut bits = 0;
        for (t, tm) in self.timers.iter_mut().enumerate() {
            if !tm.on || tm.next > self.now {
                continue;
            }
            let p = tm.period();
            let n = (self.now - tm.next) / p + 1;
            tm.next += n * p;
            self.overflows[t] += n;
            if tm.irq {
                bits |= 1 << (3 + t);
            }
        }
        bits
    }

    /// TMxCNT_L read (`t` 0 or 1): the counter at `now` after the per-read increment, and the IF bits to raise.
    pub fn read_counter(&mut self, t: usize) -> (u16, u32) {
        self.now += PER_READ;
        self.reads += 1;
        let bits = self.catch_up();
        let tm = self.timers[t];
        if !tm.on {
            return (tm.reload, bits);
        }
        // `next - now` is in (0, period]: the counter is 0x10000 minus the ticks left (rounded up).
        let left = (tm.next - self.now).div_ceil(tm.div);
        ((0x1_0000 - left) as u16, bits)
    }

    /// TMxCNT_L write: the reload value, used from the next start or overflow.
    pub fn write_reload(&mut self, t: usize, reload: u16) {
        self.timers[t].reload = reload;
    }

    /// TMxCNT_H write. Starting (enable 0 -> 1) loads the counter from the reload value now.
    pub fn write_control(&mut self, t: usize, ctrl: u16) {
        let tm = &mut self.timers[t];
        let on = ctrl & 0x80 != 0;
        tm.irq = ctrl & 0x40 != 0;
        tm.div = [1, 64, 256, 1024][(ctrl & 3) as usize];
        if on && !tm.on {
            tm.next = self.now + tm.period();
        }
        tm.on = on;
    }

    /// A V-blank started (hardware frame `frames + 1`): virtual time moves to the frame's base at least.
    /// Returns the IF bits of the timer events that are now due (the frame's scheduled events).
    pub fn frame(&mut self) -> u32 {
        self.frames += 1;
        self.now = self.now.max(self.frames * FRAME_CYCLES);
        self.catch_up()
    }

    /// Where waits stop jumping: the next frame's base.
    pub fn frame_end(&self) -> u64 {
        (self.frames + 1) * FRAME_CYCLES
    }

    /// The next overflow that raises an IRQ, if any timer has one armed.
    pub fn next_event(&self) -> Option<u64> {
        self.timers.iter().filter(|t| t.on && t.irq).map(|t| t.next).min()
    }

    /// A wait with nothing pending: jump to the next timer event if it falls in this frame. Returns the IF
    /// bits to raise, `None` when there is no such event (the wait runs the hardware to the V-blank).
    pub fn jump(&mut self) -> Option<u32> {
        let t = self.next_event().filter(|&t| t < self.frame_end())?;
        self.now = self.now.max(t);
        self.jumps += 1;
        Some(self.catch_up())
    }
}

/// Busy-wait loops from the census (`4_docs/BLOCK5_CENSUS.md`) that poll for pending IRQs: a slice that
/// starts inside one is a safe point. (start, end, what).
pub const SAFE_LOOPS: &[(u32, u32, &str)] = &[
    (0x0214_D830, 0x0214_D920, "PXI_Init handshake on IPCSYNC (boot)"),
    (0x0203_99A8, 0x0203_9A14, "display_wait_task_maybe: spins until a queued task changes its state"),
    (0x0203_9A1C, 0x0203_9AB0, "display_clear_vram_and_wait: spins until a queued task changes its state"),
    (0x0214_63A4, 0x0214_6498, "card_read_rom_polled: card data-port loop (gone with MKDS_HW_FS=native)"),
    // found by the watchdog (MKDS_HW_TIME=native race): the ARM9 spins until the IPC-receive IRQ's PXI
    // callback marks the ARM7's reply
    (0x0200_C6B8, 0x0200_C7B2, "CTRDG_Init: waits for the ARM7's cartridge reply"),
    (0x0215_1288, 0x0215_15CC, "nvram_send_command_maybe: waits for the ARM7's NVRAM reply"),
    (0x0214_D93C, 0x0214_D950, "rtc_wait_busy: waits for the ARM7's RTC reply"),
    (0x0215_01C0, 0x0215_01D4, "pm_wait_command_complete: waits for the ARM7's power-management reply"),
    (0x0214_E31C, 0x0214_E330, "TP_WaitBusy: waits for the ARM7's touch-panel reply"),
];
// Also reported by the watchdog, and not a wait: MI_CpuFill32's loop (0x0214D018, a fill of up to 1.1 M
// instructions while loading). Delivering an IRQ there is harmless.

/// SDK critical-section exits that may re-enable IRQs (AMCE revision 0 addresses; the dictionary's names for
/// these entries are shifted by one, the lifted code is what counts): (entry, r0 must clear the I bit).
pub const SAFE_ENTRIES: &[(u32, bool, &str)] = &[
    (0x0200_FC8C, false, "OS_EnableInterrupts (clears CPSR I)"),
    (0x0200_FCB4, true, "OS_RestoreInterrupts (CPSR I from r0)"),
    (0x0200_FCE0, true, "OS_RestoreInterrupts I/F (CPSR I/F from r0)"),
];

/// Where IRQs may be delivered (see the module notes). Held by `Machine::os_gate`.
#[derive(Debug)]
pub struct IrqGate {
    /// A safe point was passed: deliver while IRQs are pending; closes when none is.
    pub open: bool,
    /// The instruction count when a ready IRQ was first held back (watchdog), if one is held now.
    held_since: Option<u64>,
    /// Watchdog limit in instructions (`MKDS_WATCHDOG`, millions; default 1).
    pub limit: u64,
    /// Loops the watchdog reported (pc / 64), reported once each.
    pub reported: Vec<u32>,
    /// Diagnostics: IRQs delivered after a safe point, by a loop poll, by the watchdog; slice starts that
    /// held a ready IRQ back.
    pub delivered: u64,
    pub polled: u64,
    pub watchdog: u64,
    pub held: u64,
}

impl std::fmt::Display for IrqGate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "IRQs delivered after a safe point {}, by a loop poll {}, by the watchdog {} ({} sites); slice starts that held one back {}",
            self.delivered, self.polled, self.watchdog, self.reported.len(), self.held)
    }
}

impl std::fmt::Display for VirtualTime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "virtual time {} cycles over {} frames; counter reads {}, wait jumps {}, overflows TM0 {} TM1 {}",
            self.now, self.frames, self.reads, self.jumps, self.overflows[0], self.overflows[1])
    }
}

impl IrqGate {
    pub fn from_env() -> Self {
        let millions = std::env::var("MKDS_WATCHDOG").ok().and_then(|v| v.parse::<f64>().ok()).unwrap_or(1.0);
        Self::new((millions * 1e6) as u64)
    }

    pub fn new(limit: u64) -> Self {
        IrqGate { open: false, held_since: None, limit: limit.max(1), reported: Vec::new(), delivered: 0, polled: 0, watchdog: 0, held: 0 }
    }

    /// A wait (CP15 halt, BIOS wait): the safe point of every frame.
    pub fn wait(&mut self) {
        self.open = true;
    }

    /// Control reached `pc` (a block dispatch) with `r0`: opens the gate at a critical-section exit that
    /// re-enables IRQs. Cheap for every other pc.
    #[inline]
    pub fn entry(&mut self, pc: u32, r0: u32) {
        if pc & 0xFFFF_FF00 != 0x0200_FC00 {
            return;
        }
        if let Some(&(_, check, _)) = SAFE_ENTRIES.iter().find(|e| e.0 == pc) {
            if !check || r0 & 0x80 == 0 {
                self.open = true;
            }
        }
    }

    /// At a slice start at `pc`: may a `ready` IRQ (IME, IE & IF, CPSR I clear) be taken? `pending` is
    /// IE & IF != 0; `instructions` is the session's instruction count (for the watchdog).
    pub fn allow(&mut self, pc: u32, ready: bool, pending: bool, instructions: u64) -> bool {
        if !ready {
            if !pending {
                self.open = false;
            }
            self.held_since = None;
            return false;
        }
        if self.open {
            self.held_since = None;
            self.delivered += 1;
            return true;
        }
        if SAFE_LOOPS.iter().any(|&(lo, hi, _)| (lo..hi).contains(&(pc & !1))) {
            self.open = true;
            self.held_since = None;
            self.polled += 1;
            return true;
        }
        let since = *self.held_since.get_or_insert(instructions);
        if instructions - since >= self.limit {
            let key = pc >> 6;
            if !self.reported.contains(&key) {
                self.reported.push(key);
                eprintln!(
                    "MKDS_WATCHDOG: an IRQ was held back for {} instructions without a safe point; slice at pc {pc:#010x}. \
                     Delivered it; if this is a busy-wait, add it to os_hle::SAFE_LOOPS.",
                    instructions - since
                );
            }
            self.open = true;
            self.held_since = None;
            self.watchdog += 1;
            return true;
        }
        self.held += 1;
        false
    }
}

/// Test-only seed pin (`MKDS_RACE_SEED`, off by default), for seed-equalized oracle replays.
///
/// Race setup (`race_randomize_setup_maybe`, 0x0203B6AC) seeds its LCG with RTC seconds-of-day +
/// `OS_GetTick()` and passes the seed to its first `msl_multiply_u64` call (entry 0x02133048, return
/// 0x0203B739) in r2 (r3 = 0, the high word). Native OS time (and the native RTC) change that value, so
/// the race differs from the first race frame by design. To compare everything *else*, the seed is pinned
/// at that call:
/// - `MKDS_RACE_SEED=log`: report each seed (`MKDS_RACE_SEED: call N seed 0x...`), change nothing;
/// - `MKDS_RACE_SEED=0xA,0xB,...`: replace the seed of call N with the N-th value (from an exact run's log);
///   calls beyond the list are reported and left alone.
#[derive(Debug, Default)]
pub struct SeedPin {
    values: Option<Vec<u32>>,
    /// Calls seen so far.
    pub calls: usize,
    /// The last call (sp, seed after pinning): a slice end at the callee's entry re-dispatches it.
    last: Option<(u32, u32)>,
}

/// Entry of `msl_multiply_u64` and the return address of race setup's first call to it.
pub const SEED_CALL: (u32, u32) = (0x0213_3048, 0x0203_B739);

impl SeedPin {
    /// `MKDS_RACE_SEED` (`None` when unset).
    pub fn from_env() -> Option<Self> {
        let v = std::env::var("MKDS_RACE_SEED").ok()?;
        Some(Self::parse(&v))
    }

    pub fn parse(v: &str) -> Self {
        let values = (v.trim() != "log").then(|| {
            v.split(',')
                .filter_map(|x| {
                    let x = x.trim();
                    let r = x.strip_prefix("0x").map_or_else(|| x.parse(), |h| u32::from_str_radix(h, 16));
                    r.ok()
                })
                .collect()
        });
        SeedPin { values, calls: 0, last: None }
    }

    /// Control reached `pc` (a block dispatch). Cheap for every other pc.
    #[inline]
    pub fn entry(&mut self, pc: u32, c: &mut crate::rt::Cpu) {
        if pc != SEED_CALL.0 || c.r[14] != SEED_CALL.1 {
            return;
        }
        if self.last == Some((c.r[13], c.r[2])) {
            return; // the same call dispatched again after a slice end
        }
        let seen = c.r[2];
        let pinned = self.values.as_ref().and_then(|v| v.get(self.calls)).copied();
        if let Some(p) = pinned {
            c.r[2] = p;
        }
        eprintln!(
            "MKDS_RACE_SEED: call {} seed {seen:#010x}{}",
            self.calls,
            pinned.map_or(String::new(), |p| format!(" pinned to {p:#010x}"))
        );
        self.calls += 1;
        self.last = Some((c.r[13], c.r[2]));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tick_counter_is_virtual_and_overflow_sets_its_irq() {
        let mut v = VirtualTime::new();
        v.write_reload(0, 0);
        v.write_control(0, 0xC1); // enable, IRQ, /64
        let (a, bits) = v.read_counter(0);
        assert_eq!((a, bits), (1, 0), "one read = one tick");
        let (b, _) = v.read_counter(0);
        assert_eq!(b, 2);
        // the next V-blank moves time to the frame base: 560,190 cycles = 8,752 ticks
        assert_eq!(v.frame(), 0);
        let (c, _) = v.read_counter(0);
        assert_eq!(c as u64, (FRAME_CYCLES + PER_READ) / 64);
        // 8 frames later the 16-bit counter wrapped once: TM0 IRQ (IF bit 3)
        let mut raised = 0;
        for _ in 0..8 {
            raised |= v.frame();
        }
        assert_eq!(raised, 1 << 3);
        assert_eq!(v.overflows[0], 1);
    }

    #[test]
    fn alarm_timer_fires_at_a_wait_inside_the_frame_and_not_beyond() {
        let mut v = VirtualTime::new();
        v.write_reload(1, 0x1_0000u32.wrapping_sub(100) as u16); // 100 ticks
        v.write_control(1, 0xC1);
        assert_eq!(v.next_event(), Some(100 * 64));
        assert_eq!(v.jump(), Some(1 << 4), "jumped to the overflow: TM1 IRQ");
        assert_eq!(v.now, 6400);
        v.write_control(1, 0);
        v.write_reload(1, 0); // 65,536 ticks: beyond this frame
        v.write_control(1, 0xC1);
        assert_eq!(v.jump(), None, "the wait runs to the V-blank instead");
        let before = v.now;
        assert_eq!(v.read_counter(1).0, 1);
        assert_eq!(v.now, before + PER_READ);
    }

    #[test]
    fn gate_delivers_only_after_safe_points_and_reports_starving_loops() {
        let mut g = IrqGate::new(1000);
        assert!(!g.allow(0x0200_1000, true, true, 0), "not at a safe point");
        g.entry(0x0200_FCB4, 0x80); // restore with I still set: not a safe point
        assert!(!g.allow(0x0200_1000, true, true, 10));
        g.entry(0x0200_FCB4, 0);
        assert!(g.allow(0x0200_1000, true, true, 20));
        assert!(g.allow(0x0200_1000, true, true, 30), "open while IRQs are pending");
        assert!(!g.allow(0x0200_1000, false, false, 40));
        assert!(!g.open, "closed once nothing is pending");
        assert!(g.allow(0x0214_D8E4, true, true, 50), "census busy-wait polls");
        g.open = false;
        assert!(!g.allow(0x0200_2000, true, true, 100));
        assert!(g.allow(0x0200_2000, true, true, 1100), "watchdog");
        assert_eq!(g.reported, [0x0200_2000 >> 6]);
        g.open = false;
        g.wait();
        assert!(g.allow(0x0200_3000, true, true, 1200));
    }

    #[test]
    fn seed_pin_replaces_only_race_setups_seed_once_per_call() {
        let mut p = SeedPin::parse("0x1234,7");
        let mut c = crate::rt::Cpu::default();
        c.r[2] = 99;
        c.r[13] = 0x0200_0100;
        c.r[14] = 0x0200_0001;
        p.entry(SEED_CALL.0, &mut c);
        assert_eq!(c.r[2], 99, "other callers keep their arguments");
        c.r[14] = SEED_CALL.1;
        p.entry(SEED_CALL.0, &mut c);
        assert_eq!((c.r[2], p.calls), (0x1234, 1));
        p.entry(SEED_CALL.0, &mut c);
        assert_eq!(p.calls, 1, "re-dispatch of the same call");
        c.r[2] = 5;
        p.entry(SEED_CALL.0, &mut c);
        assert_eq!((c.r[2], p.calls), (7, 2));
        c.r[2] = 5;
        c.r[13] -= 8;
        p.entry(SEED_CALL.0, &mut c);
        assert_eq!((c.r[2], p.calls), (5, 3), "beyond the list: unchanged");
        let mut l = SeedPin::parse("log");
        c.r[2] = 42;
        l.entry(SEED_CALL.0, &mut c);
        assert_eq!((c.r[2], l.calls), (42, 1));
    }
}
