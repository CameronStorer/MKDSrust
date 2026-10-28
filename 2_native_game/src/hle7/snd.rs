//! Event-driven ARM7 sound driver (Block 5.2b, `4_docs/BLOCK5_PLAN.md`, lane `b5-sound`).
//!
//! Switch: `MKDS_HW_SND=native` (default `exact`: the ARM7 CPU runs the sound driver as before). It implies the
//! native small services of 5.2a (`MKDS_HW_ARM7=native`), because the CPU stops.
//!
//! The game's own lifted ARM7 sound driver still does all the sound work (sequencer, channels, envelopes,
//! alarms, shared work area); only *how it is driven* changes. In the exact model it is a thread on the ARM7
//! CPU, woken by a periodic OS alarm (`snd_tick_alarm_callback`, every 0xAA8 OS ticks = 5.2 ms) and by
//! `snd_pxi_command_callback` (PXI tag 7). Here the CPU is not run at all once it is idle after its start-up:
//! - an ARM9 SND word is delivered by calling `snd_pxi_command_callback` (0x037FF8B4) as a plain function call
//!   ([`crate::arm7::Arm7::plain_call`], private [`Cpu`] in IRQ mode, no time budget);
//! - one driver tick is posted every [`TICK_CYCLES`] of virtual time from a fractional accumulator (3-4 per
//!   frame) by calling `snd_tick_alarm_callback` (0x037FC248), whose OS alarm is cancelled at take-over;
//! - the driver thread's loop (`snd_thread_main` 0x037FC1C8) is replaced by [`Snd::drain`]: while the driver's
//!   message queue (0x038079B4) has a message, run one loop body with the lifted calls the thread makes
//!   (`snd_update_ex_channel`, `snd_command_execute`, `snd_seq_main` 0x037FEEB4, `snd_ex_channel_main`,
//!   `snd_update_shared_status`, `snd_calc_random`), exactly as the thread would after its blocking receive;
//! - the ARM7's remaining interrupt work runs the same way: V-blank (the SDK V-blank counter), timer 0/1 (OS
//!   tick and alarms: SND reply alarms, the pad X/Y poll) call the handler the ARM7 OS registered in its IRQ
//!   table, as plain calls. PXI words of tags without a native service go to the callback the ARM7 registered.
//! - SPU writes go to [`crate::spu`] as before; audio is rendered up to the current time before every driver
//!   call (so per tick), not per CPU burst.
//!
//! Start-up (take-over): the ARM7 CPU still runs its own boot (PXI_Init sync handshake, CTRDG wait, OS, SND
//! init, thread start). When its main loop has been reached and it is halted with nothing pending, this
//! module takes over for good ([`Snd::try_take_over`]); the tick phase is kept from the OS alarm.
//!
//! Diagnostics: `MKDS_SND_LOG=<file>` (both modes, record only) writes the driver's event stream (see [`log`]).
//!
//! Message-level oracle (5.2b verification; scripts in `native/diagnostics/`):
//! ```text
//! MKDS_SND_LOG=exact.log acceptance rom.nds route.csv out 15000        # exact model: events, reads, exact.log.snap
//! python snd_sched.py exact.log exact.sched                              # the exact schedule, reads per context
//! MKDS_HW_SND=native MKDS_SND_REPLAY=exact.sched MKDS_SND_SNAP=exact.log.snap MKDS_SND_LOG=replay.log \
//!     acceptance rom.nds route.csv out2 100                              # replays at take-over, then exits
//! python snd_replay_cmp.py exact.log replay.log                          # replies / main writes / SPU writes
//! ```
//! The replay runs the same driver loop bodies and interrupt handlers as plain calls from the exact model's
//! take-over snapshot, in the exact model's order (an interrupt that came in the middle of a loop body runs
//! after the body call it interrupted), with every main-RAM and I/O read answered from the recording; each
//! loop body's SPU and main-RAM writes and each interrupt's writes and replies must match.
//! `MKDS_AUDIO_DUMP=<file>` (any mode) writes the rendered audio for `audio_cmp.py`.
use crate::arm7::{Arm7, View};
use crate::ds_mem::Bus;
use crate::rt::{Cpu, Mem};
use std::collections::VecDeque;

/// Lifted ARM7 entry points and driver data (from `dictionary/game_names.csv`).
pub mod addr {
    pub const SND_PXI_COMMAND_CALLBACK: u32 = 0x037F_F8B4;
    pub const SND_TICK_ALARM_CALLBACK: u32 = 0x037F_C248;
    pub const SND_STOP_TICK_ALARM: u32 = 0x037F_C2E4;
    /// `snd_thread_main`'s loop, after the blocking receive returned (r0 = message).
    pub const SND_THREAD_LOOP_BODY: u32 = 0x037F_C210;
    pub const SND_UPDATE_EX_CHANNEL: u32 = 0x037F_D130;
    pub const SND_COMMAND_EXECUTE: u32 = 0x037F_F908;
    pub const SND_SEQ_MAIN: u32 = 0x037F_EEB4;
    pub const SND_EX_CHANNEL_MAIN: u32 = 0x037F_CE24;
    pub const SND_UPDATE_SHARED: u32 = 0x037F_F2F4;
    pub const SND_CALC_RANDOM: u32 = 0x037F_BF70;
    /// What one loop body of `snd_thread_main` calls, in order.
    pub const BODY_CALLS: [u32; 6] = [
        SND_UPDATE_EX_CHANNEL,
        SND_COMMAND_EXECUTE,
        SND_SEQ_MAIN,
        SND_EX_CHANNEL_MAIN,
        SND_UPDATE_SHARED,
        SND_CALC_RANDOM,
    ];
    pub const OS_RECEIVE_MESSAGE: u32 = 0x037F_97B4;
    pub const OS_GET_TICK: u32 = 0x037F_A278;
    /// `os_irq_handler` (installed at 0x0380FFFC) and where every handler returns (`os_irq_switch_thread`).
    pub const OS_IRQ_HANDLER: u32 = 0x037F_8534;
    pub const OS_IRQ_SWITCH_THREAD: u32 = 0x037F_8588;
    /// `snd_wait_for_tick`: the driver thread finished a loop body.
    pub const SND_WAIT_FOR_TICK: u32 = 0x037F_C2B8;
    /// The ARM7 OS's current-thread pointer.
    pub const CURRENT_THREAD: u32 = 0x0380_7700;
    /// The ARM7 main loop (`arm7_main` 0x037F846C) calls this right after every halt: reaching it means the
    /// start-up is over.
    pub const MAIN_LOOP_MARK: u32 = 0x037F_B0D4;
    /// The driver thread's message queue (1 = tick, 2 = command wake-up) and its tick alarm (`OSAlarm`).
    pub const SND_QUEUE: u32 = 0x0380_79B4;
    pub const SND_TICK_ALARM: u32 = 0x0380_79D4;
    /// PXI receive callbacks by tag, and the OS IRQ handler table by IE bit.
    pub const PXI_CALLBACKS: u32 = 0x0380_78BC;
    pub const IRQ_TABLE: u32 = 0x0380_7534;
}

/// OS tick = 64 ARM7 cycles; the driver's period is 0xAA8 OS ticks (`snd_start_tick_alarm`).
pub const OS_TICK_CYCLES: u64 = 64;
pub const TICK_CYCLES: u64 = 0xAA8 * OS_TICK_CYCLES;

/// IE/IF bits delivered through the PXI router instead (IPC sync, send empty, receive not empty).
const IPC_BITS: u32 = 7 << 16;

/// `MKDS_HW_SND=native`.
pub fn native_selected() -> bool {
    std::env::var("MKDS_HW_SND").is_ok_and(|v| v.eq_ignore_ascii_case("native"))
}

/// The event-driven ARM7 (sound driver and interrupt work), see the module docs.
#[derive(Default)]
pub struct Snd {
    /// Took over from the ARM7 CPU (which is not run any more).
    pub active: bool,
    /// Registers for the plain calls (IRQ mode, IRQs masked: the OS never switches threads here).
    cpu: Cpu,
    /// Scratch word above the private stack (the received message).
    buf: u32,
    /// Bus cycles until the next driver tick (a fractional accumulator in bus-cycle units: ticks fall every
    /// [`TICK_CYCLES`], 3-4 per frame).
    next_tick: u64,
    /// Bus cycles not yet rendered as audio.
    unrendered: u64,
    /// Words the driver sent to the ARM9, waiting for room in the FIFO.
    sent: VecDeque<u32>,
    /// ARM9 words for lifted PXI callbacks (sound, and tags without a native service), oldest first.
    pub inbox: Vec<u32>,
    last_line: u64,
    /// Diagnostics: ticks, commands delivered, loop bodies run, interrupts handled, bus cycle of the take-over.
    pub ticks: u64,
    pub commands: u64,
    pub bodies: u64,
    pub irqs: u64,
    pub took_over_at: Option<u64>,
    pub error: Option<String>,
}

impl Snd {
    pub fn new() -> Snd {
        Snd::default()
    }

    /// Take over from the ARM7 CPU if its start-up is over and it is idle. Returns whether this is active.
    pub fn try_take_over(&mut self, hw: &mut Bus, a: &mut Arm7) -> bool {
        if self.active {
            return true;
        }
        if a.watch_pc != addr::MAIN_LOOP_MARK {
            a.watch_pc = addr::MAIN_LOOP_MARK;
        }
        if !a.watch_hit || !a.idle(hw) {
            return false;
        }
        let top = a.irq_stack_top();
        self.cpu = Cpu::default();
        self.cpu.set_mode(0x12);
        self.cpu.ctl |= 0x80;
        self.cpu.r[13] = top - 0x20;
        self.buf = top - 0x10;
        self.last_line = hw.line;
        // keep the driver tick's phase: the OS alarm's next fire time
        let r = (|| -> Result<(), String> {
            let lo = a.plain_call(hw, &mut self.cpu, &mut self.sent, addr::OS_GET_TICK, [0; 4])?;
            let now = lo as u64 | (self.cpu.r[1] as u64) << 32;
            let fire = {
                let mut v = View { s: &mut a.s, hw };
                v.r32(addr::SND_TICK_ALARM + 0xC) as u64 | (v.r32(addr::SND_TICK_ALARM + 0x10) as u64) << 32
            };
            // the first tick fires 0x10000 OS ticks after the driver started, then every period
            self.next_tick = fire.saturating_sub(now).saturating_mul(OS_TICK_CYCLES).max(1);
            a.plain_call(hw, &mut self.cpu, &mut self.sent, addr::SND_STOP_TICK_ALARM, [0; 4])?;
            Ok(())
        })();
        self.active = true;
        self.took_over_at = Some(hw.cycles);
        if let Ok(path) = std::env::var("MKDS_SND_REPLAY") {
            let r = r.and_then(|_| replay(hw, a, self, &path));
            eprintln!("[snd] replay of {path}: {r:?}");
            log::flush();
            std::process::exit(if r.is_ok() { 0 } else { 2 });
        }
        let vblank = hw.r32(crate::ds_mem::VBLANK_COUNT);
        eprintln!(
            "[snd] native sound driver took over from the ARM7 CPU at bus cycle {} (V-blank {vblank}), next tick in {} cycles",
            hw.cycles, self.next_tick
        );
        if let Err(e) = r.and_then(|_| self.drain(hw, a)) {
            self.fail(a, e);
        }
        true
    }

    fn fail(&mut self, a: &mut Arm7, e: String) {
        if self.error.is_none() {
            eprintln!("[snd] {e}");
            self.error = Some(e.clone());
            a.execution_error = Some(format!("native sound driver: {e}"));
        }
    }

    /// `cycles` bus cycles have passed: deliver the ARM9's words, the ARM7's interrupts and the driver
    /// ticks due, then the driver's replies.
    pub fn advance(&mut self, hw: &mut Bus, a: &mut Arm7, cycles: u64) {
        if self.error.is_some() {
            return;
        }
        self.unrendered += cycles;
        if let Err(e) = self.step(hw, a, cycles) {
            self.fail(a, e);
        }
        self.deliver(hw);
    }

    fn step(&mut self, hw: &mut Bus, a: &mut Arm7, cycles: u64) -> Result<(), String> {
        for w in std::mem::take(&mut self.inbox) {
            self.word(hw, a, w)?;
        }
        a.s.tick_timers(cycles);
        self.interrupts(hw, a)?;
        // scanline interrupts are handled at the next step: the ARM9 takes its own V-blank interrupt first
        // and reads the V-blank count before the ARM7's handler increments it, as in the exact model
        self.lines(hw, a);
        let mut left = cycles;
        while left >= self.next_tick {
            left -= self.next_tick;
            self.next_tick = TICK_CYCLES;
            self.ticks += 1;
            self.call(hw, a, addr::SND_TICK_ALARM_CALLBACK, [0; 4])?;
            self.drain(hw, a)?;
        }
        self.next_tick -= left;
        Ok(())
    }

    /// Run lifted code (audio rendered up to now first: the code may change the SPU registers).
    fn call(&mut self, hw: &mut Bus, a: &mut Arm7, target: u32, args: [u32; 4]) -> Result<u32, String> {
        if self.unrendered > 0 {
            a.advance_audio(hw, self.unrendered);
            self.unrendered = 0;
        }
        a.plain_call(hw, &mut self.cpu, &mut self.sent, target, args)
    }

    /// One ARM9 word for a lifted PXI callback (`pxi_fifo_recv_irq_handler`'s dispatch).
    fn word(&mut self, hw: &mut Bus, a: &mut Arm7, w: u32) -> Result<(), String> {
        let (tag, data, err) = (w & 0x1F, w >> 6, w >> 5 & 1);
        let cb = View { s: &mut a.s, hw }.r32(addr::PXI_CALLBACKS + 4 * tag);
        if tag == super::tag::SOUND {
            self.commands += 1;
            log::command(data, err);
        }
        if cb == 0 {
            // nobody registered the tag: the ARM7's PXI layer echoes the word with the error bit
            if err == 0 {
                self.sent.push_back(w | 0x20);
            }
            return Ok(());
        }
        self.call(hw, a, cb, [tag, data, err, 0])?;
        self.drain(hw, a)
    }

    /// The driver thread: one loop body per message in its queue (`snd_thread_main` after its receive).
    pub fn drain(&mut self, hw: &mut Bus, a: &mut Arm7) -> Result<(), String> {
        loop {
            let got = self.call(hw, a, addr::OS_RECEIVE_MESSAGE, [addr::SND_QUEUE, self.buf, 0, 0])?;
            if got == 0 {
                return Ok(());
            }
            let msg = View { s: &mut a.s, hw }.r32(self.buf);
            log::body(msg);
            self.body(hw, a, msg)?;
            log::body_end();
        }
    }

    /// One loop body of the driver thread for message `msg` (1 = tick: sequencer and envelopes advance).
    fn body(&mut self, hw: &mut Bus, a: &mut Arm7, msg: u32) -> Result<(), String> {
        self.body_with(hw, a, msg, &mut |_, _, _, _| Ok(()))
    }

    /// [`Snd::body`], calling `between(k)` after the body's call `k` (1-6; 0 = before the first).
    fn body_with(
        &mut self,
        hw: &mut Bus,
        a: &mut Arm7,
        msg: u32,
        between: &mut dyn FnMut(&mut Snd, &mut Bus, &mut Arm7, usize) -> Result<(), String>,
    ) -> Result<(), String> {
        self.bodies += 1;
        let update = (msg == 1) as u32;
        between(self, hw, a, 0)?;
        for (k, &f) in addr::BODY_CALLS.iter().enumerate() {
            let arg = match f {
                addr::SND_UPDATE_EX_CHANNEL => msg,
                addr::SND_SEQ_MAIN | addr::SND_EX_CHANNEL_MAIN => update,
                _ => 0,
            };
            self.call(hw, a, f, [arg, 0, 0, 0])?;
            between(self, hw, a, k + 1)?;
        }
        Ok(())
    }

    /// V-blank / V-count interrupt requests for the scanlines passed since the last look.
    fn lines(&mut self, hw: &Bus, a: &mut Arm7) {
        let lines = crate::ds_mem::LINES;
        let stat = u16::from_le_bytes([a.s.io[4], a.s.io[5]]) as u64;
        let target = (stat >> 8) | ((stat & 0x80) << 1);
        let mut l = self.last_line;
        let n = (hw.line + lines - self.last_line) % lines;
        for _ in 0..n {
            l = (l + 1) % lines;
            if l == 192 && stat & 8 != 0 {
                a.s.iflag |= crate::arm7::irq::VBLANK;
            }
            if l == target && stat & 0x20 != 0 {
                a.s.iflag |= crate::arm7::irq::VCOUNT;
            }
        }
        self.last_line = hw.line;
    }

    /// Requested and enabled interrupts, lowest bit first, each by the handler in the OS table (what
    /// `os_irq_handler` does; its thread switch afterwards is not needed: no thread runs here).
    fn interrupts(&mut self, hw: &mut Bus, a: &mut Arm7) -> Result<(), String> {
        loop {
            let bits = a.s.ie & a.s.iflag & !IPC_BITS;
            if a.s.ime & 1 == 0 || bits == 0 {
                return Ok(());
            }
            let b = bits.trailing_zeros();
            a.s.iflag &= !(1 << b);
            let h = View { s: &mut a.s, hw }.r32(addr::IRQ_TABLE + 4 * b);
            if h == 0 {
                continue;
            }
            self.irqs += 1;
            log::irq_start(b);
            self.call(hw, a, h, [b, addr::IRQ_TABLE, 1 << b, 1])?;
            log::irq_end();
            self.drain(hw, a)?;
        }
    }

    /// Send queued words while the ARM9's receive FIFO has room.
    fn deliver(&mut self, hw: &mut Bus) {
        while hw.fifo_in.len() < 16 {
            match self.sent.pop_front() {
                Some(w) => hw.arm7_send(w),
                None => break,
            }
        }
    }

    pub fn summary(&self) -> String {
        format!(
            "native sound driver: took over at bus cycle {:?}; {} ticks, {} SND commands, {} loop bodies, {} interrupts{}",
            self.took_over_at,
            self.ticks,
            self.commands,
            self.bodies,
            self.irqs,
            self.error.as_ref().map_or(String::new(), |e| format!("; error: {e}"))
        )
    }
}

impl Drop for Snd {
    fn drop(&mut self) {
        eprintln!("[snd] {}", self.summary());
        log::flush();
    }
}

/// Recorded reads for one context of the replay, answered in order ([`crate::arm7::Plain`]).
#[derive(Default, Debug)]
pub struct Feed {
    pub reads: VecDeque<(u32, u32, u8)>,
    pub served: u64,
    pub mismatches: u64,
    pub first_mismatch: Option<String>,
}

impl Feed {
    /// The recorded value for a read of `n` bytes at `a`, if it is the next one recorded; otherwise the read
    /// is counted as a mismatch and served from memory.
    pub fn take(&mut self, a: u32, n: usize) -> Option<u32> {
        match self.reads.front() {
            Some(&(ra, v, rn)) if ra == a && rn as usize == n => {
                self.reads.pop_front();
                self.served += 1;
                Some(v)
            }
            other => {
                self.mismatches += 1;
                if self.first_mismatch.is_none() {
                    self.first_mismatch = Some(format!(
                        "read {a:#010x}/{n} after {} served, recording expected {other:x?}",
                        self.served
                    ));
                }
                None
            }
        }
    }
}

/// One step of the recorded schedule.
enum Mark {
    /// A loop body, and the interrupts that came during it in the exact model: (after call k, bit, reads).
    Body(u32, Feed, Vec<(usize, u32, Feed)>),
    Irq(u32, Feed),
}

/// Message-level oracle (`MKDS_SND_REPLAY=<exact log>`, Block 5.2b verification): replays the exact model's
/// recorded driver schedule into the plain-call driver from the exact model's take-over snapshot, with the
/// same inputs (every main-RAM / I/O read answered from the recording, per context: driver loop bodies and
/// interrupt handlers), and writes this run's own `MKDS_SND_LOG`. The two logs must then have the same
/// replies, main-RAM writes and SPU writes per loop body and per interrupt (`snd_cmp.py`).
pub fn replay(hw: &mut Bus, a: &mut Arm7, s: &mut Snd, path: &str) -> Result<String, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
    let snap_path = std::env::var("MKDS_SND_SNAP").unwrap_or_else(|_| format!("{path}.snap"));
    let snap = std::fs::read(&snap_path).map_err(|e| format!("{snap_path}: {e}"))?;
    if snap.get(..8) != Some(&b"SNDSNAP1"[..]) {
        return Err("bad snapshot".into());
    }
    let mut at = 8;
    for part in [&mut hw.main, &mut hw.swram, &mut a.s.wram, &mut a.s.vram7, &mut a.s.io] {
        let n = u32::from_le_bytes(snap[at..at + 4].try_into().unwrap()) as usize;
        at += 4;
        if n != part.len() {
            return Err("snapshot size mismatch".into());
        }
        part.copy_from_slice(&snap[at..at + n]);
        at += n;
    }
    // the schedule after `Z`: loop bodies (B..b) and interrupts (I..E), each with its own reads. A file
    // starting with `SCHED` is a schedule made by a script instead: `B msg` / `I bit` lines, each followed by
    // the `L` reads that belong to it.
    let mut marks = Vec::new();
    let sched = text.starts_with("SCHED");
    let (mut body, mut irq): (Option<usize>, Option<usize>) = (None, None);
    let hex = |t: &str| u32::from_str_radix(t, 16).unwrap_or(0);
    let (mut thread, mut body_thread) = (0u32, None::<u32>);
    for l in text.lines().skip_while(|l| !sched && !l.starts_with('Z')).skip(1) {
        let p: Vec<&str> = l.split_whitespace().collect();
        if sched {
            // `P k bit`: an interrupt inside the body above, after its call k; its reads follow
            match p.first().copied() {
                Some("B") => marks.push(Mark::Body(p[1].parse().unwrap_or(0), Feed::default(), Vec::new())),
                Some("I") => marks.push(Mark::Irq(p[1].parse().unwrap_or(0), Feed::default())),
                Some("P") => {
                    if let Some(Mark::Body(_, _, n)) = marks.last_mut() {
                        n.push((p[1].parse().unwrap_or(0), p[2].parse().unwrap_or(0), Feed::default()));
                    }
                }
                Some("L") => {
                    let r = (hex(p[1]), hex(p[2]), p[3].parse().unwrap_or(4));
                    match marks.last_mut() {
                        Some(Mark::Body(_, f, n)) => match n.last_mut() {
                            Some((_, _, nf)) => nf.reads.push_back(r),
                            None => f.reads.push_back(r),
                        },
                        Some(Mark::Irq(_, f)) => f.reads.push_back(r),
                        None => {}
                    }
                }
                _ => {}
            }
            continue;
        }
        match p.first().copied() {
            Some("T") => thread = hex(p[1]),
            Some("B") => {
                marks.push(Mark::Body(p[1].parse().unwrap_or(0), Feed::default(), Vec::new()));
                body = Some(marks.len() - 1);
                body_thread = None;
            }
            Some("b") => body = None,
            Some("I") => {
                marks.push(Mark::Irq(p[1].parse().unwrap_or(0), Feed::default()));
                irq = Some(marks.len() - 1);
            }
            Some("E") => irq = None,
            Some("L") => {
                let r = (hex(p[1]), hex(p[2]), p[3].parse().unwrap_or(4));
                // reads in a body but by another thread (switched to after an interrupt) are not the driver's;
                // the first read after the body started, even in an interrupt, is made on the driver thread
                if body.is_some() && body_thread.is_none() {
                    body_thread = Some(thread);
                }
                let body = body.filter(|_| Some(thread) == body_thread);
                if let Some(i) = irq.or(body) {
                    match &mut marks[i] {
                        Mark::Body(_, f, _) | Mark::Irq(_, f) => f.reads.push_back(r),
                    }
                }
            }
            _ => {}
        }
    }
    let (mut bodies, mut irqs, mut mism, mut left) = (0u64, 0u64, 0u64, 0usize);
    let mut first = None;
    let mut wrong_msg = 0u64;
    let mut body_mism = 0u64;
    log::replay_start();
    for m in marks {
        let (feed, what) = match m {
            Mark::Body(msg, f, nested) => {
                // the thread's receive (driver memory only, no recorded reads)
                a.feed = None;
                let got = s.call(hw, a, addr::OS_RECEIVE_MESSAGE, [addr::SND_QUEUE, s.buf, 0, 0])?;
                let have = View { s: &mut a.s, hw }.r32(s.buf);
                if got == 0 || have != msg {
                    wrong_msg += 1;
                }
                a.feed = Some(Box::new(f));
                log::body(msg);
                let mut nested: VecDeque<_> = nested.into();
                let mut nested_mism = 0;
                s.body_with(hw, a, msg, &mut |s, hw, a, k| {
                    while nested.front().is_some_and(|n| n.0 <= k) {
                        let (_, bit, nf) = nested.pop_front().unwrap();
                        let body_feed = a.feed.replace(Box::new(nf));
                        log::irq_start(bit);
                        let h = View { s: &mut a.s, hw }.r32(addr::IRQ_TABLE + 4 * bit);
                        s.call(hw, a, h, [bit, addr::IRQ_TABLE, 1 << bit, 1])?;
                        log::irq_end();
                        nested_mism += a.feed.take().map_or(0, |f| f.mismatches);
                        a.feed = body_feed;
                    }
                    Ok(())
                })?;
                log::body_end();
                bodies += 1;
                mism += nested_mism;
                (a.feed.take(), format!("body {bodies}"))
            }
            Mark::Irq(bit, f) => {
                a.feed = Some(Box::new(f));
                log::irq_start(bit);
                let h = View { s: &mut a.s, hw }.r32(addr::IRQ_TABLE + 4 * bit);
                s.call(hw, a, h, [bit, addr::IRQ_TABLE, 1 << bit, 1])?;
                log::irq_end();
                irqs += 1;
                (a.feed.take(), format!("irq {irqs} (bit {bit})"))
            }
        };
        let f = feed.unwrap();
        if f.mismatches > 0 && (mism == 0 || (what.starts_with("body") && body_mism == 0)) {
            eprintln!("[snd] replay: first read mismatch in {what}: {:?}", f.first_mismatch);
        }
        if what.starts_with("body") {
            body_mism += f.mismatches;
        }
        mism += f.mismatches;
        left += f.reads.len();
        if first.is_none() && (f.mismatches > 0 || !f.reads.is_empty()) {
            first = Some(format!("{what}: {:?}, {} reads left", f.first_mismatch, f.reads.len()));
        }
    }
    log::flush();
    Ok(format!(
        "replayed {bodies} loop bodies and {irqs} interrupts; {wrong_msg} wrong messages; {mism} read mismatches \
         ({body_mism} in loop bodies), {left} recorded reads unused; first problem: {first:?}"
    ))
}

/// `MKDS_SND_LOG=<file>`: the sound driver's event stream, the same lines in both modes, in order (record
/// only, off by default). Lines: `C <data> <err>` an ARM9 SND word reaches the driver's PXI callback;
/// `B <msg>` one driver-thread loop body starts (1 = tick, 2 = command wake-up); `R <word>` the ARM7 sends an
/// SND word to the ARM9; `M <addr> <value> <bytes>` the ARM7 writes main RAM (outside the system area at
/// 0x027FF000.. and the ARM7's backup / firmware services); `S <offset> <value> <bytes>` an SPU register write.
pub mod log {
    use std::io::Write;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Mutex, OnceLock};

    static LOG: OnceLock<Option<Mutex<std::io::BufWriter<std::fs::File>>>> = OnceLock::new();

    fn get() -> Option<&'static Mutex<std::io::BufWriter<std::fs::File>>> {
        LOG.get_or_init(|| {
            let path = std::env::var("MKDS_SND_LOG").ok()?;
            Some(Mutex::new(std::io::BufWriter::new(std::fs::File::create(path).ok()?)))
        })
        .as_ref()
    }

    #[inline]
    pub fn on() -> bool {
        get().is_some()
    }

    fn line(args: std::fmt::Arguments) {
        if let Some(l) = get() {
            let _ = l.lock().unwrap().write_fmt(args);
        }
    }

    /// After the take-over point (`Z`): log the ARM7's main-RAM and I/O reads (`L`) for the replay.
    static READS: AtomicBool = AtomicBool::new(false);

    /// The ARM7 CPU reached block `pc` (exact model); `irq` = IE & IF.
    pub fn cpu_block(pc: u32, c: &crate::rt::Cpu, irq: u32) {
        match pc {
            super::addr::SND_PXI_COMMAND_CALLBACK => command(c.r[1], c.r[2]),
            super::addr::SND_THREAD_LOOP_BODY => body(c.r[0]),
            super::addr::SND_WAIT_FOR_TICK => body_end(),
            super::addr::OS_IRQ_HANDLER => irq_start(irq.trailing_zeros()),
            super::addr::OS_IRQ_SWITCH_THREAD => irq_end(),
            _ => {
                if let Some(k) = super::addr::BODY_CALLS.iter().position(|&f| f == pc) {
                    line(format_args!("K {}\n", k + 1));
                }
            }
        }
    }

    /// Reads are not logged while one of these is alive (the SPU fetching samples).
    pub struct Quiet(bool);
    impl Quiet {
        pub fn new() -> Quiet {
            Quiet(READS.swap(false, Ordering::Relaxed))
        }
    }
    impl Drop for Quiet {
        fn drop(&mut self) {
            if self.0 {
                READS.store(true, Ordering::Relaxed);
            }
        }
    }

    pub fn irq_start(bit: u32) {
        line(format_args!("I {bit}\n"));
    }

    pub fn irq_end() {
        line(format_args!("E\n"));
    }

    /// The replay starts (`Z` in the replay's own log).
    pub fn replay_start() {
        line(format_args!("Z replay\n"));
    }

    pub fn body_end() {
        line(format_args!("b\n"));
    }

    /// An ARM7 read of main RAM or I/O (`os_irq_handler`'s own register reads are left out: the replay
    /// dispatches the interrupt itself).
    #[inline]
    pub fn read(a: u32, v: u32, n: usize, pc: u32, wram: &[u8]) {
        if READS.load(Ordering::Relaxed) && !(super::addr::OS_IRQ_HANDLER..super::addr::OS_IRQ_SWITCH_THREAD).contains(&pc) {
            // the running thread (`T` when it changes): a higher-priority thread can run inside a driver loop
            // body after an interrupt, and its reads are not the driver's
            let o = (super::addr::CURRENT_THREAD & 0xFFFF) as usize;
            let t = u32::from_le_bytes(wram[o..o + 4].try_into().unwrap());
            if THREAD.swap(t, Ordering::Relaxed) != t {
                line(format_args!("T {t:x}\n"));
            }
            line(format_args!("L {a:x} {v:x} {n}\n"));
        }
    }

    static THREAD: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

    /// The take-over point in the exact model: `Z` in the log, and the ARM7's memory in `<log>.snap`.
    pub fn snapshot(a: &crate::arm7::Arm7, hw: &crate::ds_mem::Bus) {
        let Ok(path) = std::env::var("MKDS_SND_LOG") else { return };
        let mut out = Vec::new();
        out.extend_from_slice(b"SNDSNAP1");
        for part in [&hw.main, &hw.swram, &a.s.wram, &a.s.vram7, &a.s.io] {
            out.extend_from_slice(&(part.len() as u32).to_le_bytes());
            out.extend_from_slice(part);
        }
        if let Err(e) = std::fs::write(format!("{path}.snap"), out) {
            eprintln!("[snd] snapshot: {e}");
        }
        line(format_args!("Z {:x}\n", hw.cycles));
        READS.store(true, Ordering::Relaxed);
    }

    pub fn command(data: u32, err: u32) {
        line(format_args!("C {data:x} {err}\n"));
    }

    pub fn body(msg: u32) {
        line(format_args!("B {msg}\n"));
    }

    #[inline]
    pub fn reply(w: u32) {
        if w & 0x1F == super::super::tag::SOUND && on() {
            line(format_args!("R {w:x}\n"));
        }
    }

    #[inline]
    pub fn spu(off: u32, v: u32, n: usize) {
        if on() {
            line(format_args!("S {off:x} {v:x} {n}\n"));
        }
    }

    #[inline]
    pub fn main_write(a: u32, v: u32, n: usize, pc: u32) {
        if on() && a < 0x027F_F000 && !super::super::trace::service_reader(pc) {
            line(format_args!("M {a:x} {v:x} {n}\n"));
        }
    }

    pub fn flush() {
        if let Some(l) = get() {
            let _ = l.lock().unwrap().flush();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feed_serves_recorded_reads_in_order_and_counts_mismatches() {
        let mut f = Feed::default();
        f.reads.extend([(0x0200_0000, 7, 4), (0x0400_0184, 0x8501, 2)]);
        assert_eq!(f.take(0x0200_0000, 4), Some(7));
        assert_eq!(f.take(0x0200_0004, 4), None, "not the next recorded read");
        assert_eq!(f.mismatches, 1);
        assert_eq!(f.take(0x0400_0184, 2), Some(0x8501));
        assert_eq!((f.served, f.reads.len()), (2, 0));
    }

    #[test]
    fn tick_period_is_about_3_2_per_frame() {
        let frame = crate::ds_mem::LINES * crate::ds_mem::CYCLES_PER_LINE;
        let per_frame = frame as f64 / TICK_CYCLES as f64;
        assert!((3.2..3.22).contains(&per_frame), "{per_frame}");
    }
}
