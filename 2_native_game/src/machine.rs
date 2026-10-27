//! The machine around lifted code: DS hardware memory ([`ds_mem::Bus`]), the dispatcher for
//! calls, HLE hooks for functions that must be replaced, BIOS calls and CP15. Implements the
//! runtime's [`rt::Bus`]. Hook *addresses* are supplied by the builder from the user's ROM
//! analysis (address-only metadata); none are stored here. Contains no game code or data.
use crate::bios;
use crate::dispatch::Dispatcher;
use crate::ds_mem;
use crate::rt::{Bus, Cpu, Mem};
use std::collections::HashMap;
use std::io::{BufWriter, Write};

/// How a function is replaced. After a hook runs the call returns to the caller.
#[derive(Clone, Copy)]
pub enum Hook {
    /// Do nothing (cache maintenance, debug output); r0 is left alone.
    Nop,
    /// Return a constant in r0.
    Return(u32),
    /// Custom behaviour with access to the registers and the hardware.
    Func(fn(&mut Cpu, &mut ds_mem::Bus)),
}

/// Host events polled during a guest interrupt wait. No native UI types enter the runtime.
#[derive(Default)]
pub struct IdleAction {
    pub quit: bool,
    pub buttons: Option<u16>,
    pub touch: Option<(u8, u8)>,
}

pub struct Machine<'a> {
    pub hw: ds_mem::Bus,
    disp: &'a Dispatcher,
    hooks: HashMap<u32, Hook>,
    cp15: HashMap<(u8, u8, u8, u8), u32>,
    /// Services the game asked for that nothing implements yet: what, how many times.
    pub unhandled: HashMap<String, u32>,
    /// Calls that were replaced by a hook, by entry address.
    pub hook_hits: HashMap<u32, u32>,
    /// The game's own ARM7 (crate::arm7), run alongside as hardware time passes.
    pub arm7: Option<crate::arm7::Arm7>,
    /// Native ARM7 services (`MKDS_HW_ARM7=native`, Block 5.2a): the PXI router answers the tags it claims;
    /// the ARM7 CPU keeps the rest (sound).
    pub hle7: Option<crate::hle7::Router>,
    /// Audio produced so far (stereo, at the configured SPU rate) and not yet taken by the host; see [`Machine::take_audio`].
    audio: Vec<[i16; 2]>,
    /// Diagnostics over the whole run: frames rendered, non-silent frames, peak amplitude.
    pub audio_stats: (u64, u64, i32),
    /// Keep all rendered audio instead of only the last two seconds (for recording tools).
    pub keep_all_audio: bool,
    /// Geometry engine state fed from the GX command log, and the polygons of the last completed frame
    /// (those submitted before the most recent SWAP_BUFFERS) with the viewport in force.
    pub gx: crate::gx::Gx,
    gx_swaps: u32,
    /// Diagnostics: GX command-port writes seen, and a histogram of the offsets written.
    pub gx_writes: u64,
    /// Time spent in hw.advance / sync_gx / ARM7, accumulated only while `profile_time` is set (reading
    /// the clock three times per slice cost ~20% of emulation time).
    pub prof_ns: [u64; 3],
    pub profile_time: bool,
    /// Reused buffer for GX command-port writes being fed to the geometry engine.
    gx_scratch: Vec<(u32, u32, u8)>,
    pub gx_offsets: std::collections::BTreeMap<u32, u64>,
    pub frame_polys: Vec<crate::gx::Polygon>,
    pub frame_viewport: [u8; 4],
    gx_trace: Option<BufWriter<std::fs::File>>,
    gx_trace_error: Option<String>,
    pub idle_poll: Option<Box<dyn FnMut() -> Option<IdleAction> + 'a>>,
    /// Observe applied button snapshots, including input delivered inside IRQ waits.
    /// Arguments are hardware cycles and active-high DS buttons.
    pub input_observer: Option<Box<dyn FnMut(u64, u16) + 'a>>,
    pub host_quit: bool,
}

/// Longest stretch (in 1000-cycle steps) a wait-for-interrupt may fast-forward before giving up.
const WFI_LIMIT: u32 = 2_000_000;

impl<'a> Machine<'a> {
    pub fn new(mut hw: ds_mem::Bus, disp: &'a Dispatcher) -> Self {
        let hle7 = crate::hle7::Router::from_env(&mut hw);
        Machine {
            hw,
            disp,
            hooks: HashMap::new(),
            cp15: HashMap::new(),
            unhandled: HashMap::new(),
            hook_hits: HashMap::new(),
            arm7: None,
            hle7,
            audio: Vec::new(),
            audio_stats: (0, 0, 0),
            keep_all_audio: false,
            gx: crate::gx::Gx::new(),
            gx_swaps: 0,
            gx_writes: 0,
            prof_ns: [0; 3],
            profile_time: false,
            gx_scratch: Vec::new(),
            gx_offsets: Default::default(),
            frame_polys: Vec::new(),
            frame_viewport: [0, 0, 255, 191],
            gx_trace: None,
            gx_trace_error: None,
            idle_poll: None,
            input_observer: None,
            host_quit: false,
        }
    }

    /// Replace the function at `entry` (bit 0 ignored).
    pub fn hook(&mut self, entry: u32, h: Hook) {
        self.hooks.insert(entry & !1, h);
    }

    /// Stand-in for the ARM7 having booted: it publishes the PXI channels it serves in shared main
    /// memory (0x027FFF8C; the ARM9's own bits are at 0x027FFF88), which the ARM9's start-up polls.
    /// All channels are reported ready; per-channel behaviour is the ARM7 HLE's job (lane L7).
    pub fn arm7_hle_boot(&mut self) {
        self.hw.w32(0x027F_FF8C, 0xFFFF_FFFF);
    }

    fn note(&mut self, what: String) {
        *self.unhandled.entry(what).or_insert(0) += 1;
    }

    fn geometry_read(&mut self, address: u32, width: usize) -> Option<u32> {
        if !(0x0400_0640..0x0400_06a4).contains(&address) {
            return None;
        }
        // A CPU can write a matrix command and read the result within one slice.
        // Consume queued commands before exposing result ports, not just on tick.
        self.sync_gx();
        let offset = address - ds_mem::IO_BASE;
        let word = self.gx.matrix_result(offset & !3)?;
        let shift = (offset & 3) * 8;
        Some(if width == 4 {
            word
        } else {
            (word >> shift) & ((1 << (width * 8)) - 1)
        })
    }
}

impl Mem for Machine<'_> {
    fn r8(&mut self, a: u32) -> u8 {
        self.geometry_read(a, 1)
            .map_or_else(|| self.hw.r8(a), |v| v as u8)
    }
    fn r16(&mut self, a: u32) -> u16 {
        self.geometry_read(a & !1, 2)
            .map_or_else(|| self.hw.r16(a), |v| v as u16)
    }
    fn r32(&mut self, a: u32) -> u32 {
        self.geometry_read(a & !3, 4)
            .unwrap_or_else(|| self.hw.r32(a))
    }
    fn w8(&mut self, a: u32, v: u8) {
        self.hw.w8(a, v)
    }
    fn w16(&mut self, a: u32, v: u16) {
        self.hw.w16(a, v)
    }
    fn w32(&mut self, a: u32, v: u32) {
        self.hw.w32(a, v)
    }
}

impl Bus for Machine<'_> {
    fn call(&mut self, c: &mut Cpu, target: u32, ret: u32) -> u32 {
        if let Some(&h) = self.hooks.get(&(target & !1)) {
            *self.hook_hits.entry(target & !1).or_insert(0) += 1;
            match h {
                Hook::Nop => {}
                Hook::Return(v) => c.r[0] = v,
                Hook::Func(f) => f(c, &mut self.hw),
            }
            return ret;
        }
        let d = self.disp;
        d.call(c, self, target, ret)
    }

    fn swi(&mut self, c: &mut Cpu, n: u32) {
        match bios::swi(n, &mut c.r, &mut self.hw) {
            bios::Swi::Done => {}
            // Halt / IntrWait / VBlankIntrWait: run the hardware until an interrupt is pending
            bios::Swi::Wait { .. } => self.wait_for_interrupt(),
            bios::Swi::Reset => self.note("swi SoftReset".into()),
            bios::Swi::Unsupported(n) => self.note(format!("swi {n:#04x}")),
        }
    }

    fn mrc(&mut self, _c: &mut Cpu, cp: u8, op1: u8, crn: u8, crm: u8, op2: u8) -> u32 {
        if cp != 15 {
            self.note(format!("mrc p{cp}"));
            return 0;
        }
        if (crn, crm, op1, op2) == (9, 1, 0, 0) {
            // data TCM region: base in the top bits, size encoded in bits 1..5
            return self.hw.dtcm_base.unwrap_or(0) | (5 << 1);
        }
        *self.cp15.get(&(crn, crm, op1, op2)).unwrap_or(&0)
    }

    fn mcr(&mut self, _c: &mut Cpu, cp: u8, op1: u8, crn: u8, crm: u8, op2: u8, v: u32) {
        if cp != 15 {
            self.note(format!("mcr p{cp}"));
            return;
        }
        match (crn, crm, op1, op2) {
            (7, 0, 0, 4) => self.wait_for_interrupt(), // wait for interrupt
            (7, _, _, _) => {}                         // cache maintenance: memory is coherent
            (9, 1, 0, 0) => self.hw.dtcm_base = Some(v & 0xFFFF_F000),
            _ => {
                self.cp15.insert((crn, crm, op1, op2), v);
            }
        }
    }
}

impl Machine<'_> {
    /// Record the complete local command stream, including initialization and matrix state.
    /// Trace data belongs to the user's ROM and must not be distributed with the tools.
    pub fn start_gx_trace(&mut self, path: &std::path::Path) -> std::io::Result<()> {
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)?;
        let mut writer = BufWriter::new(file);
        writer.write_all(b"GXTR0001")?;
        self.gx_trace = Some(writer);
        self.gx_trace_error = None;
        Ok(())
    }

    pub fn finish_gx_trace(&mut self) -> Result<(), String> {
        if let Some(mut writer) = self.gx_trace.take() {
            writer
                .flush()
                .map_err(|e| format!("GX trace write failed: {e}"))?;
        }
        self.gx_trace_error.take().map_or(Ok(()), Err)
    }

    pub fn gx_trace_error(&self) -> Option<&str> {
        self.gx_trace_error.as_deref()
    }

    /// Fast-forward the hardware until an interrupt is pending (or give up and note it).
    /// Advances the hardware (and the ARM7, if running) by `cycles` bus cycles.
    pub fn tick(&mut self, cycles: u64) {
        if self.profile_time {
            return self.tick_timed(cycles);
        }
        self.hw.advance(cycles);
        self.sync_gx();
        self.run_arm7(cycles);
    }

    fn tick_timed(&mut self, cycles: u64) {
        let t0 = std::time::Instant::now();
        self.hw.advance(cycles);
        let t1 = std::time::Instant::now();
        self.sync_gx();
        let t2 = std::time::Instant::now();
        self.run_arm7(cycles);
        let t3 = std::time::Instant::now();
        self.prof_ns[0] += (t1 - t0).as_nanos() as u64;
        self.prof_ns[1] += (t2 - t1).as_nanos() as u64;
        self.prof_ns[2] += (t3 - t2).as_nanos() as u64;
    }

    fn run_arm7(&mut self, cycles: u64) {
        if let Some(r) = self.hle7.as_mut() {
            r.advance(&mut self.hw, self.arm7.as_mut().map(|a| &mut a.s.periph));
        }
        if let Some(a) = self.arm7.as_mut() {
            a.run(&mut self.hw, cycles as u32);
            // ARM7::run owns the sound clock. Drain its output; rendering again
            // here advances sample positions twice and discards alternate chunks.
            if !a.s.audio_out.is_empty() {
                self.audio_stats.0 += a.s.audio_out.len() as u64;
                for f in &a.s.audio_out {
                    if *f != [0, 0] {
                        self.audio_stats.1 += 1;
                        self.audio_stats.2 = self
                            .audio_stats
                            .2
                            .max((f[0] as i32).abs())
                            .max((f[1] as i32).abs());
                    }
                }
                self.audio.extend(a.s.audio_out.drain(..));
                // A host that never drains must not grow without bound: keep the last two seconds,
                // trimming only once four have piled up (trimming every tick moved ~512 KB per call).
                if !self.keep_all_audio && self.audio.len() > 65_536 * 4 {
                    let drop = self.audio.len() - 65_536 * 2;
                    self.audio.drain(..drop);
                }
            }
        }
    }

    /// Feeds new GX command-port writes to the geometry engine; on SWAP_BUFFERS the polygons submitted so
    /// far become the frame to draw ([`Machine::frame_polys`]). Called as time advances.
    pub fn sync_gx(&mut self) {
        if self.hw.gx.is_empty() {
            return;
        }
        // swap in the reused buffer (keeps both allocations alive across ticks)
        let mut log = std::mem::take(&mut self.gx_scratch);
        std::mem::swap(&mut log, &mut self.hw.gx);
        if let Some(writer) = self.gx_trace.as_mut() {
            for &(off, value, width) in &log {
                let mut record = [0u8; 9];
                record[..4].copy_from_slice(&off.to_le_bytes());
                record[4..8].copy_from_slice(&value.to_le_bytes());
                record[8] = width;
                if let Err(e) = writer.write_all(&record) {
                    self.gx_trace_error = Some(format!("GX trace write failed: {e}"));
                    break;
                }
            }
        }
        self.gx_writes += log.len() as u64;
        for &(off, _, _) in &log {
            *self.gx_offsets.entry(off).or_insert(0) += 1;
        }
        // a swap inside the log ends a frame: feed command by command so polygons are split at the swap
        for &(off, v, n) in &log {
            let before = self.gx.swaps;
            self.gx.write(off, v);
            let _ = n;
            if self.gx.swaps != before {
                self.frame_polys = std::mem::take(&mut self.gx.polygons);
                self.frame_viewport = self.gx.viewport;
                self.gx_swaps = self.gx.swaps;
            }
        }
        log.clear();
        self.gx_scratch = log;
    }

    /// Renders the last completed 3D frame to packed RGBA (alpha 0 = no 3D pixel), using the texture and
    /// texture-palette banks as VRAMCNT currently maps them, and CLEAR_COLOR for the background.
    pub fn render_frame3d(&self) -> Vec<u32> {
        let mut polys = self.frame_polys.clone();
        crate::gx::resolve_toon(&mut polys, &self.hw.io);
        render_3d(&self.hw.io, &self.hw.vram, &polys, self.frame_viewport)
    }

    /// The host's controller state in one call: `buttons` (A B Select Start Right Left Up Down R L = bits 0-9,
    /// X = 10, Y = 11) and the stylus position in screen pixels (`None` = pen up). Sets the key registers,
    /// the pen pin the ARM7 polls, and the touch controller's coordinates (via the screen calibration).
    pub fn set_input(&mut self, buttons: u16, touch: Option<(u8, u8)>) {
        let input = crate::input::HostInput {
            buttons,
            pen_down: touch.is_some(),
            debug_down: false,
            lid_closed: false,
        };
        self.hw.set_input(input);
        if let Some(observer) = self.input_observer.as_mut() {
            observer(self.hw.cycles, buttons);
        }
        if let Some(a) = self.arm7.as_mut() {
            a.s.periph.spi.pen = touch.map(|(x, y)| crate::peripherals::touch_raw(x, y));
        }
    }

    /// Set the device rate before running the machine (defaults to the DS rate).
    /// Discards buffered audio from the previous rate when changing a running machine.
    pub fn set_audio_rate(&mut self, hz: u32) {
        assert!(hz > 0, "audio output rate must be nonzero");
        if let Some(a) = self.arm7.as_mut() {
            if a.s.spu.output_rate() != hz {
                a.s.spu.set_output_rate(hz);
                a.s.audio_out.clear();
                self.audio.clear();
            }
        }
    }

    /// Takes audio rendered since the last call (stereo frames at the configured SPU output rate).
    pub fn take_audio(&mut self) -> Vec<[i16; 2]> {
        std::mem::take(&mut self.audio)
    }

    fn wait_for_interrupt(&mut self) {
        let mut n = 0;
        while !self.hw.irq_pending() {
            if let Some(action) = self.idle_poll.as_mut().and_then(|poll| poll()) {
                if let Some(buttons) = action.buttons {
                    self.set_input(buttons, action.touch);
                }
                self.host_quit |= action.quit;
            }
            if self.host_quit {
                return;
            }
            self.tick(1000);
            n += 1;
            if n % 100_000 == 0 && std::env::var_os("MKDS_TRACE_WFI").is_some() {
                let a7 = self
                    .arm7
                    .as_ref()
                    .map_or((0, false, 0, 0), |a| (a.pc, a.s.halted, a.s.ie, a.s.iflag));
                eprintln!(
                    "wfi: {n}k cycles, IE {:#X} IF {:#X} IME {}, arm7 pc {:#010X} halted {} IE {:#X} IF {:#X}",
                    self.hw.r32(0x0400_0210),
                    self.hw.r32(0x0400_0214),
                    self.hw.r32(0x0400_0208),
                    a7.0,
                    a7.1,
                    a7.2,
                    a7.3
                );
            }
            if n > WFI_LIMIT {
                self.note("wait-for-interrupt never woke".into());
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_observer_sees_normal_and_irq_wait_input_at_applied_cycles() {
        let d = Dispatcher::new(&[]);
        let seen = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let mut m = Machine::new(ds_mem::Bus::new(), &d);
        let log = seen.clone();
        m.input_observer = Some(Box::new(move |cycles, buttons| log.borrow_mut().push((cycles, buttons))));
        m.hw.cycles = 123;
        m.set_input(1, None);
        m.idle_poll = Some(Box::new(|| Some(IdleAction {
            quit: true, buttons: Some(0), touch: None,
        })));
        m.wait_for_interrupt();
        assert_eq!(*seen.borrow(), [(123, 1), (123, 0)]);
    }

    #[test]
    fn host_close_interrupts_a_guest_wait_without_needing_an_enabled_irq() {
        let d = Dispatcher::new(&[]);
        let mut m = Machine::new(ds_mem::Bus::new(), &d);
        m.idle_poll = Some(Box::new(|| {
            Some(IdleAction {
                quit: true,
                ..IdleAction::default()
            })
        }));
        let mut c = Cpu::default();
        m.mcr(&mut c, 15, 0, 7, 0, 4, 0);
        assert!(m.host_quit);
        assert_eq!(m.hw.cycles, 0, "close does not burn the wait timeout");
        assert!(!m.unhandled.contains_key("wait-for-interrupt never woke"));
    }

    #[test]
    fn sound_clock_advances_once_and_preserves_fractional_frames() {
        let disp = Dispatcher::new(&[]);
        let mut m = Machine::new(ds_mem::Bus::new(), &disp);
        let mut a = crate::arm7::Arm7::new();
        a.s.halted = true;
        for i in 0..128 {
            a.s.wram[i * 2..i * 2 + 2].copy_from_slice(&((i as i16 + 1) * 128).to_le_bytes());
        }
        a.s.spu.write(0x500, 0x807f, 2);
        a.s.spu.write(0x404, 0x0380_0000, 4);
        a.s.spu.write(0x408, 65536 - 512, 2);
        a.s.spu.write(0x40c, 64, 4);
        a.s.spu.write(0x400, 0xa800_007f, 4);
        m.arm7 = Some(a);
        // Many small slices exercise fractional accounting, not just one batch.
        for _ in 0..512 {
            m.tick(128);
        }
        let samples = m.take_audio();
        assert_eq!(
            samples.len(),
            (65536 * crate::spu::SAMPLE_RATE as u64 / ds_mem::BUS_HZ) as usize
        );
        assert!(
            samples.last().unwrap()[0] > 7800 && samples.last().unwrap()[0] < 8400,
            "64 frames must advance the PCM ramp once, not twice: {:?}",
            samples.last()
        );
        assert!(m.arm7.as_ref().unwrap().s.audio_out.is_empty());
    }

    #[test]
    fn matrix_readback_consumes_commands_before_the_next_tick() {
        let disp = Dispatcher::new(&[]);
        let mut m = Machine::new(ds_mem::Bus::new(), &disp);
        m.w32(0x0400_0440, 2); // position/vector mode
        m.w32(0x0400_0454, 0); // identity
        for value in [0x12345678, 0, 0] {
            m.w32(0x0400_0470, value);
        } // translate
        assert_eq!(m.hw.cycles, 0);
        assert_eq!(m.r32(0x0400_0670), 0x12345678);
        assert_eq!(m.r16(0x0400_0672), 0x1234);
        assert_eq!(m.r8(0x0400_0671), 0x56);
        assert_eq!(m.r32(0x0400_0680), 4096);
        assert!(m.hw.gx.is_empty());
    }

    #[test]
    fn hooks_replace_calls_and_are_counted() {
        let disp = Dispatcher::new(&[]);
        let mut m = Machine::new(ds_mem::Bus::new(), &disp);
        m.hook(0x0200_EE74, Hook::Nop);
        m.hook(0x0200_FC8C | 1, Hook::Return(0x80));
        let mut c = Cpu::default();
        c.r[0] = 7;
        assert_eq!(m.call(&mut c, 0x0200_EE74, 0x0200_0100), 0x0200_0100);
        assert_eq!(c.r[0], 7);
        m.call(&mut c, 0x0200_FC8C, 0x0200_0100);
        assert_eq!(c.r[0], 0x80);
        assert_eq!(m.hook_hits[&0x0200_EE74], 1);
    }

    #[test]
    fn host_input_reaches_keys_pen_and_touch_controller() {
        let disp = Dispatcher::new(&[]);
        let mut m = Machine::new(ds_mem::Bus::new(), &disp);
        m.arm7 = Some(crate::arm7::Arm7::new());
        m.set_input(0b1000, Some((128, 96)));
        assert_eq!(
            m.hw.r16(0x0400_0130) & 0x3FF,
            0x3FF & !0b1000,
            "Start held, active low"
        );
        assert_eq!(
            m.hw.extended_keys() & 0x40,
            0,
            "pen down pulls EXTKEYIN bit 6 low"
        );
        let pen = m.arm7.as_ref().unwrap().s.periph.spi.pen;
        assert_eq!(pen, Some(crate::peripherals::touch_raw(128, 96)));
        m.set_input(0, None);
        assert_eq!(m.hw.extended_keys() & 0x40, 0x40);
        assert_eq!(m.arm7.as_ref().unwrap().s.periph.spi.pen, None);
    }

    #[test]
    fn bios_and_cp15_go_through_the_machine() {
        let disp = Dispatcher::new(&[]);
        let mut m = Machine::new(ds_mem::Bus::new(), &disp);
        let mut c = Cpu::default();
        c.r[0] = 100;
        c.r[1] = 7;
        m.swi(&mut c, 0x09);
        assert_eq!((c.r[0], c.r[1]), (14, 2));
        m.mcr(&mut c, 15, 0, 9, 1, 0, 0x027E_0000 | 0x14);
        assert_eq!(m.hw.dtcm_base, Some(0x027E_0000));
        m.mcr(&mut c, 15, 0, 7, 14, 0, 0); // clean+invalidate: ignored
        m.mcr(&mut c, 15, 0, 1, 0, 0, 0x1234);
        assert_eq!(m.mrc(&mut c, 15, 0, 1, 0, 0), 0x1234);
        m.swi(&mut c, 0x77);
        assert_eq!(m.unhandled["swi 0x77"], 1);
    }

    #[test]
    fn vblank_wait_runs_the_hardware_to_the_interrupt() {
        let disp = Dispatcher::new(&[]);
        let mut m = Machine::new(ds_mem::Bus::new(), &disp);
        let mut c = Cpu::default();
        // enable VBlank in DISPSTAT and IE/IME, then wait
        m.hw.w16(ds_mem::IO_BASE + 0x004, 1 << 3);
        m.hw.w32(ds_mem::IO_BASE + 0x210, 1);
        m.hw.w32(ds_mem::IO_BASE + 0x208, 1);
        let before = m.hw.cycles;
        m.swi(&mut c, 0x05);
        assert!(m.hw.cycles > before);
        assert!(m.hw.irq_pending() || m.unhandled.contains_key("wait-for-interrupt never woke"));
        assert!(m.hw.irq_pending(), "VBlank should wake the wait");
    }
}


/// Owned GPU inputs. No CPU memory, device state or references to the running machine.
#[derive(Clone)]
pub struct RenderSnapshot {
    /// ARM9 I/O registers (both 2D engines, 3D control, VRAMCNT, CLEAR_COLOR ...).
    pub io: Vec<u8>,
    /// Standard palettes: engine A BG/OBJ, then engine B.
    pub palette: Vec<u8>,
    /// OAM: engine A, then engine B.
    pub oam: Vec<u8>,
    /// VRAM banks A-I.
    pub vram: Vec<Vec<u8>>,
    /// The last completed 3D frame's polygons, in submission order.
    pub polys: Vec<crate::gx::Polygon>,
    /// VIEWPORT (x1, y1, x2, y2) of that frame.
    pub viewport: [u8; 4],
}

const SNAPSHOT_MAGIC: &[u8; 8] = b"MKSNAP01";

impl RenderSnapshot {
    /// The renderer input as one file (`MKSNAP01`). It holds game graphics: keep snapshots local.
    pub fn encode(&self) -> Vec<u8> {
        let mut o = SNAPSHOT_MAGIC.to_vec();
        let bytes = |o: &mut Vec<u8>, b: &[u8]| {
            o.extend_from_slice(&(b.len() as u32).to_le_bytes());
            o.extend_from_slice(b);
        };
        bytes(&mut o, &self.io);
        bytes(&mut o, &self.palette);
        bytes(&mut o, &self.oam);
        o.extend_from_slice(&(self.vram.len() as u32).to_le_bytes());
        for bank in &self.vram {
            bytes(&mut o, bank);
        }
        o.extend_from_slice(&self.viewport);
        o.extend_from_slice(&(self.polys.len() as u32).to_le_bytes());
        for p in &self.polys {
            for v in [p.attr, p.tex_param, p.pltt_base, p.verts.len() as u32] {
                o.extend_from_slice(&v.to_le_bytes());
            }
            for v in &p.verts {
                for c in v.clip {
                    o.extend_from_slice(&c.to_le_bytes());
                }
                o.extend_from_slice(&v.color.to_le_bytes());
                o.extend_from_slice(&v.st[0].to_le_bytes());
                o.extend_from_slice(&v.st[1].to_le_bytes());
            }
        }
        o
    }

    pub fn decode(data: &[u8]) -> Result<Self, String> {
        struct Reader<'a>(&'a [u8], usize);
        impl<'a> Reader<'a> {
            fn take(&mut self, n: usize) -> Result<&'a [u8], String> {
                let b = self.0.get(self.1..self.1 + n).ok_or("render snapshot is truncated")?;
                self.1 += n;
                Ok(b)
            }
            fn u32(&mut self) -> Result<u32, String> {
                Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
            }
            fn bytes(&mut self) -> Result<Vec<u8>, String> {
                let n = self.u32()? as usize;
                if n > 1 << 24 {
                    return Err("render snapshot field too large".into());
                }
                Ok(self.take(n)?.to_vec())
            }
        }
        let mut r = Reader(data, 0);
        if r.take(8)? != SNAPSHOT_MAGIC {
            return Err("not a render snapshot (MKSNAP01)".into());
        }
        let (io, palette, oam) = (r.bytes()?, r.bytes()?, r.bytes()?);
        let banks = r.u32()? as usize;
        if banks > 16 {
            return Err("render snapshot has too many VRAM banks".into());
        }
        let vram = (0..banks).map(|_| r.bytes()).collect::<Result<Vec<_>, _>>()?;
        let viewport: [u8; 4] = r.take(4)?.try_into().unwrap();
        let count = r.u32()? as usize;
        let mut polys = Vec::with_capacity(count.min(1 << 16));
        for _ in 0..count {
            let (attr, tex_param, pltt_base, n) = (r.u32()?, r.u32()?, r.u32()?, r.u32()? as usize);
            if n > 64 {
                return Err("render snapshot polygon has too many vertices".into());
            }
            let mut verts = Vec::with_capacity(n);
            for _ in 0..n {
                let b = r.take(22)?;
                let i = |k: usize| i32::from_le_bytes(b[k..k + 4].try_into().unwrap());
                let h = |k: usize| [b[k], b[k + 1]];
                verts.push(crate::gx::Vertex {
                    clip: [i(0), i(4), i(8), i(12)],
                    color: u16::from_le_bytes(h(16)),
                    st: [i16::from_le_bytes(h(18)), i16::from_le_bytes(h(20))],
                });
            }
            polys.push(crate::gx::Polygon { verts, attr, tex_param, pltt_base });
        }
        let s = Self { io, palette, oam, vram, polys, viewport };
        if s.io.len() < 0x1070 || s.palette.len() < 0x800 || s.oam.len() < 0x800 || s.vram.len() < 9 {
            return Err("render snapshot is missing renderer state".into());
        }
        Ok(s)
    }

    /// Both engines with the 3D layer left open (engine A, engine B) and whether engine A is on the top screen.
    pub fn splits(&self) -> (crate::gfx2d::Split, crate::gfx2d::Split, bool) {
        let a = crate::gfx2d::split_memory(&self.io, &self.palette, &self.oam, &self.vram, 0);
        let b = crate::gfx2d::split_memory(&self.io, &self.palette, &self.oam, &self.vram, 1);
        (a, b, u16::from_le_bytes([self.io[0x304], self.io[0x305]]) & 0x8000 != 0)
    }

    /// [`RenderSnapshot::splits`] with the two engines on two threads.
    pub fn splits_parallel(&self) -> (crate::gfx2d::Split, crate::gfx2d::Split, bool) {
        std::thread::scope(|s| {
            let a = s.spawn(|| crate::gfx2d::split_memory(&self.io, &self.palette, &self.oam, &self.vram, 0));
            let b = crate::gfx2d::split_memory(&self.io, &self.palette, &self.oam, &self.vram, 1);
            (a.join().expect("engine A split"), b, u16::from_le_bytes([self.io[0x304], self.io[0x305]]) & 0x8000 != 0)
        })
    }

    /// The software 3D layer alone (packed `0xRRGGBBAA`, alpha 0 = no 3D pixel): the GPU renderer's reference.
    pub fn layer3d(&self) -> Vec<u32> {
        render_3d(&self.io, &self.vram, &self.polys, self.viewport)
    }

    /// Texture memory as the 3D engine sees it (VRAMCNT-mapped slots) and CLEAR_COLOR as packed RGBA.
    pub fn texture_memory(&self) -> (Vec<u8>, Vec<u8>) {
        let cnt: Vec<u8> = (0x240..0x247).map(|i| self.io[i]).collect();
        crate::texture::assemble(&self.vram[..7], &cnt)
    }
}

/// Native-window pixels and renderer stage costs (3D, engine A, engine B).
pub struct RenderedScreens {
    pub pixels: Vec<u32>,
    pub took: std::time::Duration,
    pub parts: [std::time::Duration; 3],
}

impl RenderSnapshot {
    pub fn screens(&self) -> Vec<u32> {
        self.screens_profiled().pixels
    }

    /// Renders directly from borrowed snapshot memory, without another GPU-state copy.
    pub fn screens_profiled(&self) -> RenderedScreens {
        use std::time::{Duration, Instant};
        let started = Instant::now();
        let t = Instant::now();
        let layer = render_3d(&self.io, &self.vram, &self.polys, self.viewport);
        let mut parts = [t.elapsed(), Duration::ZERO, Duration::ZERO];
        let t = Instant::now();
        let a = crate::gfx2d::render_memory(
            &self.io,
            &self.palette,
            &self.oam,
            &self.vram,
            0,
            Some(&layer),
        );
        parts[1] = t.elapsed();
        let t = Instant::now();
        let b =
            crate::gfx2d::render_memory(&self.io, &self.palette, &self.oam, &self.vram, 1, None);
        parts[2] = t.elapsed();
        let a_top = u16::from_le_bytes([self.io[0x304], self.io[0x305]]) & 0x8000 != 0;
        let (top, bottom) = if a_top { (a, b) } else { (b, a) };
        let pixels = top
            .into_iter()
            .chain(bottom)
            .map(|rgba| rgba >> 8)
            .collect();
        RenderedScreens {
            pixels,
            took: started.elapsed(),
            parts,
        }
    }
}

impl RenderSnapshot {
    /// Engine A is on the top screen (POWCNT1 bit 15).
    pub fn a_top(&self) -> bool {
        self.io[0x305] & 0x80 != 0
    }

    /// Display capture (DISPCAPCNT): the VRAM bank (0 = A .. 3 = D) this frame captures a whole 256 x 192 image
    /// of engine A's graphics into, if any. Games use it to show 3D on both screens: one frame renders the top
    /// view and captures it, the next renders the bottom view while engine B shows the captured bank.
    pub fn capture_bank(&self) -> Option<usize> {
        let d = u32::from_le_bytes([self.io[0x64], self.io[0x65], self.io[0x66], self.io[0x67]]);
        (d >> 31 == 1 && (d >> 20) & 3 == 3 && (d >> 29) & 3 == 0).then_some(((d >> 16) & 3) as usize)
    }

    /// Engine B displays VRAM bank `bank` (C as its BG memory, D as its OBJ memory) on a screen that is on.
    pub fn engine_b_shows(&self, bank: usize) -> bool {
        let cnt = self.io[0x240 + bank];
        let dispcnt_b = u32::from_le_bytes([self.io[0x1000], self.io[0x1001], self.io[0x1002], self.io[0x1003]]);
        matches!(bank, 2 | 3) && cnt & 0x80 != 0 && cnt & 7 == 4 && (dispcnt_b >> 16) & 3 == 1
    }
}

/// Display capture for presenters, at the screens' own resolution: the image engine A showed in a frame that
/// captured it is shown again where engine B displays that bank afterwards. (Engine B shows the captured bank
/// unchanged in the games using this, e.g. Mario Kart DS's course intros; the bank itself is not written.) A
/// capture counts for [`CaptureCarry::FRAMES`] frames, like the bank keeping it across a camera cut, but not
/// after the game has gone back to drawing that bank itself.
#[derive(Default)]
pub struct CaptureCarry {
    /// Per bank A-D: the captured screen and how many frames ago.
    banks: [Option<(Vec<u32>, u32)>; 4],
}

impl CaptureCarry {
    pub const FRAMES: u32 = 3;

    /// The bank engine B shows from a recent capture, if any (`ages`: frames since each bank's capture).
    pub fn shown(snap: &RenderSnapshot, ages: [Option<u32>; 4]) -> Option<usize> {
        (0..4).find(|&bank| ages[bank].is_some_and(|age| age < Self::FRAMES) && snap.engine_b_shows(bank))
    }

    /// `screens`: the two screens of `snap`, top then bottom (equal halves).
    pub fn apply(&mut self, snap: &RenderSnapshot, screens: &mut [u32]) {
        let half = screens.len() / 2;
        let (a, b) = if snap.a_top() { (0, half) } else { (half, 0) };
        for bank in self.banks.iter_mut().flatten() {
            bank.1 += 1;
        }
        let ages = std::array::from_fn(|i| self.banks[i].as_ref().map(|b| b.1));
        if let Some(bank) = Self::shown(snap, ages) {
            let image = &self.banks[bank].as_ref().unwrap().0;
            if image.len() == half {
                screens[b..b + half].copy_from_slice(image);
            }
        }
        if let Some(bank) = snap.capture_bank() {
            self.banks[bank] = Some((screens[a..a + half].to_vec(), 0));
        }
    }
}

#[cfg(test)]
mod capture_tests {
    use super::*;

    fn snap(capture_bank: Option<u32>, a_top: bool, b_shows_c: bool) -> RenderSnapshot {
        let mut io = vec![0u8; 0x2000];
        if let Some(bank) = capture_bank {
            io[0x64..0x68].copy_from_slice(&(1u32 << 31 | 3 << 20 | bank << 16).to_le_bytes());
        }
        io[0x305] = if a_top { 0x80 } else { 0 };
        io[0x1002] = 1; // engine B display on
        io[0x242] = if b_shows_c { 0x84 } else { 0x80 };
        RenderSnapshot { io, palette: vec![], oam: vec![], vram: vec![], polys: vec![], viewport: [0, 0, 255, 191] }
    }

    #[test]
    fn engine_b_shows_the_image_captured_into_its_bank_for_a_few_frames() {
        let mut carry = CaptureCarry::default();
        let mut f1 = vec![1, 1, 0, 0]; // top = engine A (1s), bottom = engine B
        carry.apply(&snap(Some(2), true, false), &mut f1);
        assert_eq!(f1, [1, 1, 0, 0], "nothing captured yet");
        let mut f2 = vec![0, 0, 7, 7]; // swapped: engine A (7s) at the bottom, engine B shows bank C on top
        carry.apply(&snap(None, false, true), &mut f2);
        assert_eq!(f2, [1, 1, 7, 7], "last frame's capture on engine B's screen");
        for _ in 0..CaptureCarry::FRAMES {
            let mut f = vec![0, 0, 7, 7];
            carry.apply(&snap(None, false, true), &mut f);
        }
        let mut stale = vec![0, 0, 7, 7];
        carry.apply(&snap(None, false, true), &mut stale);
        assert_eq!(stale, [0, 0, 7, 7], "an old capture is not shown over the game's own drawing");
    }
}

fn render_3d(
    io: &[u8],
    vram: &[Vec<u8>],
    polys: &[crate::gx::Polygon],
    viewport: [u8; 4],
) -> Vec<u32> {
    let cnt: Vec<u8> = (0x240..0x247).map(|i| io[i]).collect();
    let (tex, pal) = crate::texture::assemble(&vram[..7], &cnt);
    let cc = u32::from_le_bytes([io[0x350], io[0x351], io[0x352], io[0x353]]);
    let e = |v: u32| {
        let v = v & 31;
        (v << 3) | (v >> 2)
    };
    let alpha = ((cc >> 16) & 31) * 255 / 31;
    let clear = (e(cc) << 24) | (e(cc >> 5) << 16) | (e(cc >> 10) << 8) | alpha;
    crate::raster::render(polys, viewport, &tex, &pal, clear).color
}

impl Machine<'_> {
    /// Synchronize pending GX writes and capture exactly the inputs used by the renderer.
    pub fn render_snapshot(&mut self) -> RenderSnapshot {
        self.sync_gx();
        let mut io = self.hw.io.clone();
        // the capture of the frame being shown (the enable bit itself may already be cleared)
        io[0x64..0x68].copy_from_slice(&self.hw.capture_latch.to_le_bytes());
        RenderSnapshot {
            io,
            palette: self.hw.palette.clone(),
            oam: self.hw.oam.clone(),
            vram: self.hw.vram.to_vec(),
            polys: {
                let mut polys = self.frame_polys.clone();
                crate::gx::resolve_toon(&mut polys, &self.hw.io);
                polys
            },
            viewport: self.frame_viewport,
        }
    }
}

#[cfg(test)]
mod snapshot_tests {
    use super::*;

    #[test]
    fn snapshot_file_round_trips_and_rejects_damage() {
        let dispatcher = crate::dispatch::Dispatcher::new(&[]);
        let mut m = Machine::new(crate::ds_mem::Bus::new(), &dispatcher);
        fixture(&mut m, 1);
        let mut snap = m.render_snapshot();
        snap.polys = vec![crate::gx::Polygon {
            verts: (0..3).map(|i| crate::gx::Vertex { clip: [i, -i, 4096 * i, 4096], color: 0x7c1f, st: [-16, 32] }).collect(),
            attr: 0x001f_00c0, tex_param: 0x1234_5678, pltt_base: 0x40,
        }];
        snap.viewport = [1, 2, 254, 190];
        let file = snap.encode();
        let back = RenderSnapshot::decode(&file).unwrap();
        assert_eq!(back.encode(), file);
        assert_eq!(back.polys, snap.polys);
        assert_eq!(back.screens(), snap.screens());
        assert!(RenderSnapshot::decode(&file[..file.len() - 1]).is_err());
        assert!(RenderSnapshot::decode(b"MKSNAP00").is_err());
    }
    use crate::rt::Mem;
    fn fixture(m: &mut Machine, variant: u32) {
        let h = &mut m.hw;
        for (i, b) in h.palette.iter_mut().enumerate() {
            *b = (i * 37 + 19) as u8;
        }
        for (k, bank) in h.vram.iter_mut().enumerate() {
            for (i, b) in bank.iter_mut().enumerate() {
                *b = (i * 13 + k * 17 + i / 91) as u8;
            }
        }
        for i in 0..256 {
            h.oam[i * 8..i * 8 + 2].copy_from_slice(&0x200u16.to_le_bytes());
        }
        for (reg, val) in [
            (0x240, 0x81),
            (0x241, 0x82),
            (0x242, 0x84),
            (0x243, 0x83),
            (0x244, 0x83),
            (0x245, 0x85),
            (0x246, 0x84),
            (0x248, 0x82),
            (0x249, 0x83),
        ] {
            h.io[reg] = val;
        }
        h.w32(
            0x04000000,
            (1 << 16) | 0x1100 | 0x10 | if variant == 1 { 8 } else { 0 },
        );
        h.w32(
            0x04001000,
            (1 << 16) | 0x500 | if variant == 2 { 5 | (1 << 30) } else { 0 },
        );
        h.w16(0x04000008, 1 << 8);
        h.w16(0x04001008, 2 << 8);
        h.w16(0x0400100c, 0x104);
        h.w16(0x04001020, 256);
        h.w16(0x04001026, 256);
        h.w16(0x04000304, if variant & 1 != 0 { 0x8000 } else { 0 });
        h.w32(0x04000350, 0x001f4210);
        h.oam[0..2].copy_from_slice(&24u16.to_le_bytes());
        h.oam[2..4].copy_from_slice(&0x8020u16.to_le_bytes());
        h.oam[4..6].copy_from_slice(&0x1004u16.to_le_bytes());
        m.frame_polys = vec![crate::gx::Polygon {
            verts: vec![
                crate::gx::Vertex {
                    clip: [-3000, -3000, 0, 4096],
                    color: 31,
                    st: [0, 0],
                },
                crate::gx::Vertex {
                    clip: [3000, -3000, 0, 4096],
                    color: 992,
                    st: [0, 0],
                },
                crate::gx::Vertex {
                    clip: [0, 3000, 0, 4096],
                    color: 31744,
                    st: [0, 0],
                },
            ],
            attr: (31 << 16) | (3 << 6),
            tex_param: 0,
            pltt_base: 0,
        }];
    }
    fn hash(p: &[u32]) -> u64 {
        p.iter()
            .flat_map(|v| v.to_le_bytes())
            .fold(0xcbf29ce484222325u64, |h, b| {
                (h ^ b as u64).wrapping_mul(0x100000001b3)
            })
    }

    #[test]
    fn snapshot_matches_pre_refactor_pixels_and_owns_all_inputs() {
        fn assert_send<T: Send>() {}
        assert_send::<RenderSnapshot>();
        // Recorded by the saved pre-refactor 2D path and 3D/composition algorithm.
        let expected = [0x3a4b9c9bc9847688, 0x63cd01b381c498af, 0x3c76eb0fe134c67c];
        let dispatcher = crate::dispatch::Dispatcher::new(&[]);
        for (variant, expected) in expected.into_iter().enumerate() {
            let mut m = Machine::new(crate::ds_mem::Bus::new(), &dispatcher);
            fixture(&mut m, variant as u32);
            let snapshot = m.render_snapshot();
            let pixels = snapshot.screens();
            assert_eq!(pixels.len(), 256 * 384);
            assert_eq!(hash(&pixels), expected, "fixture {variant}");
            m.hw.vram.iter_mut().for_each(|v| v.fill(0));
            m.hw.palette.fill(0);
            m.hw.oam.fill(0);
            m.hw.io.fill(0);
            m.frame_polys.clear();
            m.frame_viewport = [0; 4];
            let rendered = std::thread::spawn(move || snapshot.screens())
                .join()
                .unwrap();
            assert_eq!(rendered, pixels, "snapshot changed with live machine");
        }
    }
}
