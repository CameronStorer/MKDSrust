//! A resumable two-CPU session. ROM loading stays explicit; no local fallback paths.
use super::{Flat, boot_shared_area};
use crate::rt::Mem;
use crate::{arm7::Arm7, dispatch::Dispatcher, ds_mem, machine::Machine, rt};

#[path = "fibered.rs"]
mod fibered;

pub const FRAME_CYCLES: u64 = 263 * 2130;

#[derive(Default, Debug, Clone, Copy)]
pub struct Stats {
    pub instructions: u64,
    pub lifted_instructions: u64,
    pub interpreted_instructions: u64,
    pub irqs: u64,
    pub overlay_instructions: u64,
}

pub struct Session<'a> {
    pub machine: Machine<'a>,
    pub cpu: rt::Cpu,
    pub pc: u32,
    pub stats: Stats,
    dispatcher: &'a Dispatcher,
    half_cycle: u64,
    signatures: &'static [crate::residency::SegmentSig],
    code_epoch: u64,
    residency_frame: u64,
    /// `code_epoch` at the last periodic residency check: unchanged code needs no re-hash.
    residency_epoch: u64,
    pub resident_overlays: Vec<&'static str>,
    /// Continue into the next lifted block within a slice (default). Off: every call, return or
    /// computed jump ends the slice, as before 2026-10-15 (finer CPU interleaving, much slower).
    pub chain: bool,
    overlay_windows: Vec<(u32, u64, &'static str)>,
    /// Native functions checked against their translations on every call ([`Session::enable_shadow`]).
    pub shadow: Option<crate::shadow::Shadow>,
    /// Block 4A.2: the ARM9 runs in fibers with nested calls ([`Session::enable_fibers`],
    /// or `MKDS_FIBERS=1` when the session is created).
    fibers: Option<fibered::Fibers>,
    fiber_error: Option<String>,
    /// Diagnostics (`MKDS_SLICE_TRACE=<file>`): per frame, a hash of the CPU at every slice end;
    /// `MKDS_TRACE_FRAME=<n>` also lists that frame's slices. Compares flat and fiber runs.
    trace: Option<(std::io::BufWriter<std::fs::File>, u64, u64, u64)>,
}

impl<'a> Session<'a> {
    /// `image` is the whole decompressed ARM9 binary, including autoload sources.
    /// The caller must select resident segments in `dispatcher`; overlays overlap.
    pub fn new(rom: &[u8], image: &[u8], dispatcher: &'a Dispatcher) -> Result<Self, String> {
        if rom.len() < 0x200 {
            return Err("ROM header is truncated".into());
        }
        if &rom[12..16] != b"AMCE" || rom[0x1e] != 0 {
            return Err("Runtime currently supports AMCE revision 0 only".into());
        }
        let word = |o| u32::from_le_bytes(rom[o..o + 4].try_into().unwrap());
        let base = word(0x28);
        let entry = word(0x24);
        let end = (base as u64) + image.len() as u64;
        if image.is_empty()
            || base < 0x0200_0000
            || end > 0x0240_0000
            || (entry & !3) < base
            || (entry & !3) as u64 >= end
            || entry & 3 != 0
        {
            return Err("ARM9 image or entry is outside the supported main RAM range".into());
        }
        // Validate ARM7 file ranges before its loader indexes or allocates memory.
        let (off, size, ram, start) = (
            word(0x30) as usize,
            word(0x3c) as usize,
            word(0x38),
            word(0x34),
        );
        let arm7_end = ram as u64 + size as u64;
        let arm7_ram = (ram >= 0x0200_0000 && arm7_end <= 0x0240_0000)
            || (ram >= 0x037f_8000 && arm7_end <= 0x0381_0000);
        if size == 0
            || off.checked_add(size).is_none_or(|e| e > rom.len())
            || !arm7_ram
            || start < ram
            || start as u64 >= ram as u64 + size as u64
            || start & 3 != 0
        {
            return Err("ARM7 image or entry is outside supported RAM".into());
        }
        let mut hw = ds_mem::Bus::new();
        hw.arm7_lle = true;
        for (i, &b) in image.iter().enumerate() {
            hw.w8(base + i as u32, b);
        }
        let magic = [0x21, 0x06, 0xc0, 0xde, 0xde, 0xc0, 0x06, 0x21];
        if let Some(p) = image.windows(8).take(0x8000).position(|w| w == magic) {
            if p < 0x1c {
                return Err("Invalid ARM9 module parameters".into());
            }
            hw.w32(base + p as u32 - 8, 0); // compressed_static_end: already unpacked
        }
        boot_shared_area(&mut hw, rom);
        hw.card = Some(crate::card::Card::new(rom.to_vec(), 0x0000_1fc2));
        let arm7 = Arm7::from_rom(rom, &mut hw).ok_or("Cannot load ARM7")?;
        let mut machine = Machine::new(hw, dispatcher);
        machine.arm7 = Some(arm7);
        let mut cpu = rt::Cpu::default();
        cpu.r[13] = 0x0380_ff00;
        Ok(Self {
            machine,
            cpu,
            pc: entry,
            stats: Stats::default(),
            dispatcher,
            half_cycle: 0,
            signatures: &[],
            code_epoch: 0,
            residency_frame: u64::MAX,
            residency_epoch: u64::MAX,
            resident_overlays: Vec::new(),
            chain: true,
            overlay_windows: Vec::new(),
            shadow: None,
            fibers: std::env::var_os("MKDS_FIBERS").map(|_| fibered::Fibers::new(entry)),
            fiber_error: None,
            trace: std::env::var_os("MKDS_SLICE_TRACE").map(|p| {
                let detail = std::env::var("MKDS_TRACE_FRAME").ok().and_then(|v| v.parse().ok()).unwrap_or(u64::MAX);
                (std::io::BufWriter::new(std::fs::File::create(p).expect("MKDS_SLICE_TRACE file")), 0xcbf2_9ce4_8422_2325, u64::MAX, detail)
            }),
        })
    }

    /// Installs `natives` and runs each call to one of them through the shadow checker
    /// ([`crate::shadow`]); results in `self.shadow`.
    pub fn enable_shadow(&mut self, natives: &'static [crate::native::Native]) {
        self.dispatcher.set_natives(natives);
        let d = self.dispatcher;
        self.shadow = Some(crate::shadow::Shadow::new(natives, &|s, e| d.translation(s, e)));
    }

    /// Runs the ARM9 in fibers from now on (block 4A.2): calls nest, slices end in place, each SDK
    /// thread has its own fiber. The session must not move afterwards.
    pub fn enable_fibers(&mut self) {
        self.fibers = Some(fibered::Fibers::new(self.pc));
    }

    /// Guest thread switches served by fibers (0 when fibers are off).
    pub fn fiber_switches(&self) -> u64 {
        self.fibers.as_ref().map_or(0, |f| f.switches)
    }

    pub fn configure_residency(
        &mut self,
        signatures: &'static [crate::residency::SegmentSig],
    ) -> Result<(), String> {
        self.machine.hw.watch_overlay_code(signatures)?;
        self.dispatcher.configure_residency(signatures);
        self.signatures = signatures;
        self.overlay_windows = signatures
            .iter()
            .filter(|s| crate::residency::is_overlay(s.segment))
            .filter_map(|s| {
                Some((
                    s.ranges.iter().map(|r| r.start).min()?,
                    s.ranges
                        .iter()
                        .map(|r| r.start as u64 + r.len as u64)
                        .max()?,
                    s.segment,
                ))
            })
            .collect();
        self.code_epoch = self.machine.hw.code_epoch;
        self.residency_frame = u64::MAX;
        self.residency_epoch = u64::MAX;
        self.resident_overlays.clear();
        Ok(())
    }

    fn update_residency(&mut self) {
        if self.signatures.is_empty() {
            return;
        }
        if self.machine.hw.code_epoch != self.code_epoch {
            // always: it also re-arms the dispatcher's one residency check on a miss
            self.dispatcher.invalidate_overlays();
            self.resident_overlays.clear();
            self.code_epoch = self.machine.hw.code_epoch;
        }
        let frame = self.frame_number();
        // Once a frame, and only if overlay code was written since the last check (watched writes
        // bump `code_epoch`; without one the verification result cannot change).
        if frame != self.residency_frame && self.code_epoch != self.residency_epoch {
            self.residency_epoch = self.code_epoch;
            let before = self.resident_overlays.clone();
            self.resident_overlays = self
                .dispatcher
                .refresh_residency(self.signatures, &|address, bytes| {
                    self.machine.hw.peek_into(address, bytes)
                });
            self.residency_frame = frame;
            if self.resident_overlays != before {
                eprintln!(
                    "Frame {frame}: verified overlays {:?}",
                    self.resident_overlays
                );
            }
        }
    }

    /// Advance at most 32 ARM9 instructions, keeping the boot-time PXI handshake interleaved.
    pub fn step(&mut self, limit: u32) -> Result<u32, String> {
        let limit = limit.min(32);
        if limit == 0 {
            return Ok(0);
        }
        self.update_residency();
        if self.fibers.is_some() {
            return self.step_fibered(limit);
        }
        let hw = &mut self.machine.hw;
        if rt::irq_ready(
            &self.cpu,
            hw.r32(0x0400_0208),
            hw.r32(0x0400_0210),
            hw.r32(0x0400_0214),
        ) {
            if hw.probe.is_some() {
                let pending = hw.r32(0x0400_0210) & hw.r32(0x0400_0214);
                hw.probe.as_mut().unwrap().irq(pending);
                if pending & 1 != 0 {
                    self.machine.oracle_wait(crate::machine::ORACLE_VBLANK_IRQ);
                }
            }
            self.pc = rt::take_irq(&mut self.cpu, self.pc, 0xffff_0000);
            self.stats.irqs += 1;
        }
        self.cpu.budget = limit;
        let found = {
            let hw = &self.machine.hw;
            self.dispatcher
                .resolve(self.pc, &|address, bytes| hw.peek_into(address, bytes))
        };
        if found.is_some() && self.dispatcher.resident_len() != self.resident_overlays.len() {
            // the miss path just verified a freshly loaded overlay
            self.resident_overlays = self.dispatcher.resident();
        }
        let overlay = in_overlay(&self.overlay_windows, &self.resident_overlays, self.pc);
        let mut bus = Flat {
            m: &mut self.machine,
            watch: None,
            pc: self.pc,
        };
        let called_interp = self.dispatcher.interpreted();
        let mut overlay_used = 0u32;
        let lifted = if let Some(mut f) = found {
            let mut overlay = overlay;
            loop {
                let before = self.cpu.budget;
                let next = match &mut self.shadow {
                    Some(s) => s.run(f, &mut self.cpu, &mut bus, self.pc),
                    None => f(&mut self.cpu, &mut bus, self.pc),
                };
                if overlay {
                    overlay_used += before.saturating_sub(self.cpu.budget);
                }
                if next == rt::BUDGET_EXHAUSTED {
                    self.pc = self.cpu.r[15];
                    break;
                }
                self.pc = next;
                // Chain into the next lifted block while the slice has budget: a call or return
                // no longer ends the slice (each slice costs an IRQ check, a hardware tick and an
                // ARM7 burst). Stop at a miss (resolved or reported by the next slice) and when
                // overlay code was written (residency must be re-checked before dispatching).
                if !self.chain || self.cpu.budget == 0 || bus.m.hw.code_epoch != self.code_epoch {
                    break;
                }
                match self.dispatcher.get(self.pc) {
                    Some(g) => f = g,
                    None => break,
                }
                overlay = in_overlay(&self.overlay_windows, &self.resident_overlays, self.pc);
                bus.pc = self.pc;
            }
            if let Some(e) = self.dispatcher.take_error() {
                return Err(e);
            }
            true
        } else {
            Self::interpret(&mut self.cpu, &mut self.pc, self.dispatcher, &mut bus)?;
            false
        };
        let used = limit
            .checked_sub(self.cpu.budget)
            .ok_or("Runtime instruction budget underflow")?;
        if used == 0 {
            return Err(format!(
                "No execution progress at ARM9 PC {:#010x}",
                self.pc
            ));
        }
        // instructions the dispatcher interpreted inside calls from lifted code
        let inner = (self.dispatcher.interpreted() - called_interp).min(used as u64);
        self.stats.instructions += used as u64;
        if lifted {
            self.stats.lifted_instructions += used as u64 - inner;
            self.stats.interpreted_instructions += inner;
            self.stats.overlay_instructions += (overlay_used as u64).min(used as u64 - inner);
        } else {
            self.stats.interpreted_instructions += used as u64;
        }
        if self.trace.is_some() {
            self.trace_slice(self.pc, used);
        }
        if let Some(p) = self.machine.hw.probe.as_mut() {
            p.slice_end(self.pc, used);
        }
        // Preserve half bus cycles between slices instead of rounding every slice up.
        self.half_cycle += used as u64;
        self.machine.tick(self.half_cycle / 2);
        self.half_cycle %= 2;
        if let Some(a) = &self.machine.arm7 {
            if let Some(reason) = &a.execution_error {
                return Err(format!("{reason}; ARM9 PC {:#010x}", self.pc));
            }
            if a.s.crash.is_some() {
                return Err(format!(
                    "ARM7 left mapped memory at PC {:#010x}; ARM9 PC {:#010x}",
                    a.pc, self.pc
                ));
            }
            if a.s.undecodable {
                let reason = a.s.unhandled.keys().find(|s| s.starts_with("undecodable ")).cloned().unwrap_or_default();
                return Err(format!("ARM7 stopped: {reason}; ARM9 PC {:#010x}", self.pc));
            }
        }
        Ok(used)
    }

    /// Diagnostics: records the slice that just ended at `pc` (see `trace`).
    fn trace_slice(&mut self, pc: u32, used: u32) {
        use std::io::Write;
        let frame = self.frame_number();
        let Some((w, h, last, detail)) = &mut self.trace else { return };
        let c = &self.cpu;
        if *last != frame {
            if *last != u64::MAX {
                let _ = writeln!(w, "frame {last} instr {} irqs {} hash {h:016x}", self.stats.instructions, self.stats.irqs);
            }
            *last = frame;
        }
        for v in c.r[..15].iter().copied().chain([pc, used, c.ctl, c.n as u32 | (c.z as u32) << 1 | (c.c as u32) << 2 | (c.v as u32) << 3]) {
            *h = (*h ^ v as u64).wrapping_mul(0x100_0000_01b3);
        }
        if frame == *detail {
            let why = self.fibers.as_ref().map_or("", |f| f.last_end.1);
            let _ = writeln!(w, "  slice pc {pc:#010x} used {used} irqs {} r {:x?} epoch {}/{} {why}", self.stats.irqs, &c.r[..15], self.machine.hw.code_epoch, self.code_epoch);
        }
    }

    /// [`Session::step`] in fiber mode: the guest takes its own IRQs and slice ends.
    fn step_fibered(&mut self, limit: u32) -> Result<u32, String> {
        self.cpu.budget = limit;
        let called_interp = self.dispatcher.interpreted();
        fibered::run_slice(self)?;
        self.pc = self.fibers.as_ref().map_or(self.pc, |f| f.last_end.0); // diagnostics only
        if let Some(e) = self.dispatcher.take_error() {
            return Err(e);
        }
        let used = limit.checked_sub(self.cpu.budget).ok_or("Runtime instruction budget underflow")?;
        if used == 0 {
            let f = self.fibers.as_ref().unwrap();
            return Err(format!("No execution progress in a fiber slice: ended at {:#010x} ({}), thread {:#010x}, {} switches", f.last_end.0, f.last_end.1, f.current, f.switches));
        }
        let inner = (self.dispatcher.interpreted() - called_interp).min(used as u64);
        self.stats.instructions += used as u64;
        self.stats.lifted_instructions += used as u64 - inner;
        self.stats.interpreted_instructions += inner;
        if self.trace.is_some() {
            let pc = self.fibers.as_ref().unwrap().last_end.0;
            self.trace_slice(pc, used);
        }
        if let Some(p) = self.machine.hw.probe.as_mut() {
            p.slice_end(self.pc, used);
        }
        self.half_cycle += used as u64;
        self.machine.tick(self.half_cycle / 2);
        self.half_cycle %= 2;
        if let Some(a) = &self.machine.arm7 {
            if let Some(reason) = &a.execution_error {
                return Err(reason.clone());
            }
            if a.s.crash.is_some() {
                return Err(format!("ARM7 left mapped memory at PC {:#010x}", a.pc));
            }
            if a.s.undecodable {
                return Err("ARM7 stopped: undecodable instruction".into());
            }
        }
        Ok(used)
    }

    /// No lifted block at `pc`: interpret until one (diagnostics builds only).
    #[cfg(feature = "interpreter")]
    fn interpret(cpu: &mut rt::Cpu, pc: &mut u32, dispatcher: &Dispatcher, bus: &mut Flat<'_, '_>) -> Result<(), String> {
        while cpu.budget > 0 {
            bus.pc = *pc;
            let d = crate::interp::fetch(bus, *pc);
            if let crate::arm::Inst::Undecoded(word) = d.inst {
                return Err(format!("Undecodable ARM9 instruction {word:#010x} at PC {:#010x}", *pc));
            }
            cpu.budget -= 1;
            *pc = crate::interp::exec(&d, *pc, cpu, bus);
            if dispatcher.is_lifted(*pc) {
                break;
            }
        }
        Ok(())
    }

    /// Without the interpreter a pc with no lifted block is an error naming it and its segment.
    #[cfg(not(feature = "interpreter"))]
    fn interpret(_: &mut rt::Cpu, pc: &mut u32, dispatcher: &Dispatcher, _: &mut Flat<'_, '_>) -> Result<(), String> {
        Err(format!("No lifted ARM9 code at PC {:#010x}: {}", *pc, dispatcher.describe(*pc)))
    }

    pub fn advance(&mut self, instructions: u32) -> Result<(), String> {
        let mut left = instructions;
        while left > 0 && !self.machine.host_quit {
            left -= self.step(left)?;
        }
        Ok(())
    }

    pub fn frame_number(&self) -> u64 {
        self.machine.hw.cycles / FRAME_CYCLES
    }

    /// Block 5.0: the logical frame, the SDK's V-blank count (`OS_GetVBlankCount`, `0x027FFC3C`, incremented
    /// by the OS V-blank handler on the ARM7) instead of hardware time. 0 until the ARM7 has booted.
    pub fn logical_frame(&self) -> u64 {
        let mut b = [0u8; 4];
        self.machine.hw.peek_into(ds_mem::VBLANK_COUNT, &mut b);
        u32::from_le_bytes(b) as u64
    }

    /// Immutable, Send GPU state for off-thread rendering.
    pub fn render_snapshot(&mut self) -> crate::machine::RenderSnapshot {
        self.machine.render_snapshot()
    }

    /// Both physical screens, top then bottom, packed 0x00RRGGBB for the native window.
    pub fn screens(&mut self) -> Vec<u32> {
        self.render_snapshot().screens()
    }

}

/// Is `pc` inside a resident overlay's address window?
fn in_overlay(windows: &[(u32, u64, &'static str)], resident: &[&'static str], pc: u32) -> bool {
    windows.iter().any(|&(lo, hi, segment)| {
        pc & !1 >= lo && ((pc & !1) as u64) < hi && resident.contains(&segment)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(feature = "interpreter")]
    fn overlay_writes_from_arm9_arm7_and_dma_drop_stale_lifts_before_dispatch() {
        use crate::residency::{CodeRange, SegmentSig, fnv1a64};
        fn overlay_a(c: &mut rt::Cpu, _: &mut dyn rt::Bus, _: u32) -> u32 {
            rt::spend(c, 1);
            c.r[0] = 0xaa;
            0x0200_0000
        }
        fn overlay_b(c: &mut rt::Cpu, _: &mut dyn rt::Bus, _: u32) -> u32 {
            rt::spend(c, 1);
            c.r[0] = 0xbb;
            0x0200_0000
        }
        static TABLE: &[crate::mem::Lifted] = &[
            crate::mem::Lifted {
                segment: "ov9_000",
                entry: 0x0200_1000,
                blocks: &[0x0200_1000],
                insts: &[0x0200_1000],
                f: overlay_a,
            },
            crate::mem::Lifted {
                segment: "ov9_001",
                entry: 0x0200_1000,
                blocks: &[0x0200_1000],
                insts: &[0x0200_1000],
                f: overlay_b,
            },
        ];
        let signatures: &'static [SegmentSig] = Box::leak(Box::new([
            SegmentSig {
                segment: "ov9_000",
                ranges: Box::leak(Box::new([CodeRange {
                    start: 0x0200_1000,
                    len: 4,
                    hash: fnv1a64(&0xe3a0_0007u32.to_le_bytes()),
                }])),
            },
            SegmentSig {
                segment: "ov9_001",
                ranges: Box::leak(Box::new([CodeRange {
                    start: 0x0200_1000,
                    len: 4,
                    hash: fnv1a64(&0xe3a0_0009u32.to_le_bytes()),
                }])),
            },
        ]));
        let (rom, image) = fixture();
        let d = Dispatcher::with_loaded(TABLE, &[]);
        let mut s = Session::new(&rom, &image, &d).unwrap();
        s.configure_residency(signatures).unwrap();
        s.machine.hw.w32(0x0200_1000, 0xe3a0_0007);
        s.pc = 0x0200_1000;
        s.advance(1).unwrap();
        assert_eq!(s.cpu.r[0], 0xaa);
        assert_eq!(s.stats.overlay_instructions, 1);
        // ARM9 writes through a RAM mirror: the old compiled function must not run. Bytes that
        // match no overlay are interpreted; bytes of another overlay are verified on the first
        // miss and run lifted at once (no wait for the next frame's residency check).
        s.machine.w32(0x0240_1000, 0xe3a0_0005);
        s.pc = 0x0200_1000;
        s.advance(1).unwrap();
        assert_eq!(s.cpu.r[0], 5);
        assert!(s.resident_overlays.is_empty());
        assert_eq!(s.stats.interpreted_instructions, 1);
        s.machine.w32(0x0240_1000, 0xe3a0_0009);
        s.pc = 0x0200_1000;
        s.advance(1).unwrap();
        assert_eq!(s.cpu.r[0], 0xbb, "freshly loaded overlay B runs lifted mid-frame");
        assert_eq!(s.resident_overlays, ["ov9_001"]);
        assert_eq!(s.stats.interpreted_instructions, 1);
        {
            let m = &mut s.machine;
            let mut view = crate::arm7::View {
                s: &mut m.arm7.as_mut().unwrap().s,
                hw: &mut m.hw,
            };
            view.w32(0x0240_1000, 0xe3a0_0005);
        }
        s.pc = 0x0200_1000;
        s.advance(1).unwrap();
        assert_eq!(s.cpu.r[0], 5, "ARM7 writes invalidate overlay B");
        s.machine.w32(0x0200_1000, 0xe3a0_0007);
        s.pc = 0x0200_1000;
        s.advance(1).unwrap();
        assert_eq!(s.cpu.r[0], 0xaa);
        s.machine.w32(0x0200_2000, 0xe3a0_0005);
        s.machine.w32(0x0400_00b0, 0x0200_2000);
        s.machine.w32(0x0400_00b4, 0x0200_1000);
        s.machine.w32(0x0400_00b8, 0x8400_0001);
        s.pc = 0x0200_1000;
        s.advance(1).unwrap();
        assert_eq!(s.cpu.r[0], 5, "DMA writes invalidate overlay A");
        assert!(s.resident_overlays.is_empty());
        assert_eq!(s.stats.interpreted_instructions, 3);
    }

    #[test]
    #[cfg(not(feature = "interpreter"))]
    fn without_the_interpreter_a_miss_is_an_error_naming_pc_and_segment() {
        fn compiled(cpu: &mut rt::Cpu, _: &mut dyn rt::Bus, _: u32) -> u32 {
            assert!(rt::spend(cpu, 1));
            0x0200_0004
        }
        static TABLE: &[crate::mem::Lifted] = &[crate::mem::Lifted {
            segment: "arm9_main",
            entry: 0x0200_0000,
            blocks: &[0x0200_0000],
            insts: &[0x0200_0000, 0x0200_0004],
            f: compiled,
        }];
        let (rom, mut image) = fixture();
        image.extend_from_slice(&0xeaff_fffeu32.to_le_bytes());
        let d = Dispatcher::with_loaded(TABLE, &["arm9_main"]);
        let mut s = Session::new(&rom, &image, &d).unwrap();
        s.advance(1).unwrap();
        let e = s.advance(1).unwrap_err();
        assert!(e.contains("No lifted ARM9 code at PC 0x02000004"), "{e}");
        assert!(e.contains("arm9_main_02000000"), "{e}");
        assert_eq!(s.stats.interpreted_instructions, 0);
    }
    #[test]
    fn a_slice_chains_lifted_blocks_until_its_budget_or_a_miss() {
        fn first(c: &mut rt::Cpu, _: &mut dyn rt::Bus, _: u32) -> u32 {
            assert!(rt::spend(c, 3));
            0x0200_0010 // a call or return: the next lifted block
        }
        fn second(c: &mut rt::Cpu, _: &mut dyn rt::Bus, _: u32) -> u32 {
            assert!(rt::spend(c, 2));
            0x0200_0020 // no lifted block here
        }
        static TABLE: &[crate::mem::Lifted] = &[
            crate::mem::Lifted { segment: "arm9_main", entry: 0x0200_0000, blocks: &[0x0200_0000], insts: &[0x0200_0000], f: first },
            crate::mem::Lifted { segment: "arm9_main", entry: 0x0200_0010, blocks: &[0x0200_0010], insts: &[0x0200_0010], f: second },
        ];
        // the fixture's ARM7 (`b .` at 0x03800000) as lifted code, so this runs without the interpreter
        fn arm7_loop(c: &mut rt::Cpu, _: &mut dyn rt::Bus, _: u32) -> u32 {
            while rt::spend(c, 3) {}
            c.r[15] = 0x0380_0000;
            rt::BUDGET_EXHAUSTED
        }
        static ARM7: &[crate::mem::Lifted] = &[crate::mem::Lifted { segment: "arm7", entry: 0x0380_0000, blocks: &[0x0380_0000], insts: &[0x0380_0000], f: arm7_loop }];
        let (rom, image) = fixture();
        let d = Dispatcher::with_loaded(TABLE, &["arm9_main"]);
        let mut s = Session::new(&rom, &image, &d).unwrap();
        s.machine.arm7.as_mut().unwrap().configure_lifted(ARM7, false);
        assert_eq!(s.step(32).unwrap(), 5, "both blocks ran in one slice");
        assert_eq!(s.pc, 0x0200_0020);
        assert_eq!(s.stats.lifted_instructions, 5);
        let mut s = Session::new(&rom, &image, &d).unwrap();
        s.machine.arm7.as_mut().unwrap().configure_lifted(ARM7, false);
        s.chain = false;
        assert_eq!(s.step(32).unwrap(), 3, "unchained: the slice ends at the jump");
        assert_eq!(s.pc, 0x0200_0010);
    }
    fn fixture() -> (Vec<u8>, Vec<u8>) {
        let mut rom = vec![0; 0x210];
        rom[12..16].copy_from_slice(b"AMCE");
        for (o, value) in [
            (0x24, 0x0200_0000u32),
            (0x28, 0x0200_0000),
            (0x30, 0x200),
            (0x34, 0x0380_0000),
            (0x38, 0x0380_0000),
            (0x3c, 4),
        ] {
            rom[o..o + 4].copy_from_slice(&value.to_le_bytes());
        }
        rom[0x200..0x204].copy_from_slice(&0xeaff_fffeu32.to_le_bytes()); // ARM7 loop
        (rom, 0xeaff_fffeu32.to_le_bytes().to_vec())
    }
    #[test]
    fn explicit_inputs_are_validated_without_panicking() {
        let d = Dispatcher::new(&[]);
        assert!(Session::new(&[], &[], &d).is_err());
        let (mut rom, image) = fixture();
        rom[0x3c..0x40].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(Session::new(&rom, &image, &d).is_err());
    }
    #[test]
    #[cfg(feature = "interpreter")]
    fn resuming_preserves_pc_and_fractional_bus_cycles() {
        let (rom, image) = fixture();
        let d = Dispatcher::new(&[]);
        let mut s = Session::new(&rom, &image, &d).unwrap();
        s.advance(3).unwrap();
        assert_eq!(s.machine.hw.cycles, 1);
        s.advance(3).unwrap();
        assert_eq!(s.machine.hw.cycles, 3);
        assert_eq!(s.pc, 0x0200_0000);
        assert_eq!(s.stats.interpreted_instructions, 6);
    }
    #[test]
    #[cfg(feature = "interpreter")]
    fn supplied_lift_table_is_executed_and_fallback_is_counted_separately() {
        fn compiled(cpu: &mut rt::Cpu, _: &mut dyn rt::Bus, _: u32) -> u32 {
            assert!(rt::spend(cpu, 1));
            cpu.r[0] = 123;
            0x0200_0004
        }
        static TABLE: &[crate::mem::Lifted] = &[crate::mem::Lifted {
            segment: "arm9_main",
            entry: 0x0200_0000,
            blocks: &[0x0200_0000],
            insts: &[0x0200_0000],
            f: compiled,
        }];
        let (rom, mut image) = fixture();
        image.extend_from_slice(&0xeaff_fffeu32.to_le_bytes());
        let d = Dispatcher::with_loaded(TABLE, &["arm9_main"]);
        let mut s = Session::new(&rom, &image, &d).unwrap();
        s.advance(1).unwrap();
        assert_eq!(s.cpu.r[0], 123);
        assert_eq!(s.pc, 0x0200_0004);
        s.advance(2).unwrap();
        assert_eq!(s.stats.lifted_instructions, 1);
        assert_eq!(s.stats.interpreted_instructions, 2);
    }
    #[test]
    fn physical_screen_order_follows_power_control() {
        let (rom, image) = fixture();
        let d = Dispatcher::new(&[]);
        let mut s = Session::new(&rom, &image, &d).unwrap();
        s.machine.hw.w32(0x0400_0000, 1 << 16);
        s.machine.hw.w32(0x0400_1000, 1 << 16);
        s.machine.hw.w16(0x0500_0000, 31);
        s.machine.hw.w16(0x0500_0400, 31 << 10);
        s.machine.hw.w16(0x0400_0304, 0x800f);
        let ab = s.screens();
        assert_eq!(ab[0], 0xff0000);
        assert_eq!(ab[256 * 192], 0x0000ff);
        s.machine.hw.w16(0x0400_0304, 0x000f);
        assert_eq!(s.screens()[0], 0x0000ff);
    }
}
