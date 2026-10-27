//! The DS's second CPU (ARM7TDMI, 33 MHz) running the game's own ARM7 binary from the user's ROM
//! (interpreted: it is small), so sound, touch, RTC, power and the ARM9<->ARM7 PXI conversation come
//! from the original code instead of a hand-written stand-in. Shares main RAM, shared WRAM and the IPC
//! registers with the ARM9 through [`ds_mem::Bus`]; has its own WRAM, I/O, interrupt controller and timers.
//!
//! Not modelled yet (recorded in [`Arm7::unhandled`]): SPI devices (touch screen, power management,
//! firmware), RTC, sound output, ARM7 DMA, BIOS sound tables. Contains no game code or data; the IRQ
//! vector code is our own ([`rt::ARM7_IRQ_STUB`]).
use crate::ds_mem;
#[cfg(feature = "interpreter")]
use crate::interp;
use crate::rt::{self, Bus, Cpu, Mem};
use std::collections::HashMap;

const WRAM_SIZE: usize = 0x1_0000;
const IO_SIZE: usize = 0x1000;

/// One ARM7 timer (counts at 33.5 MHz / prescaler).
#[derive(Clone, Copy, Default, Debug)]
pub struct Timer7 {
    pub counter: u16,
    pub reload: u16,
    pub ctrl: u16,
    acc: u64,
}

/// Everything of the ARM7 but its registers (split so the CPU and its memory view can be borrowed apart).
/// Decoded instructions of the ARM7's code area (shared WRAM + ARM7 WRAM, 0x037F8000..0x03810000),
/// by halfword; cleared where the ARM7 writes.
const CODE_LO: u32 = 0x037F_8000;
const CODE_HI: u32 = 0x0381_0000;

pub struct State7 {
    /// Sound output rendered as emulated time passes (stereo, at the SPU output rate), for the platform's
    /// audio device to drain; bounded (old samples are dropped when nobody listens).
    pub audio_out: std::collections::VecDeque<[i16; 2]>,
    /// Fractional output frames, in units of ARM7 clock ticks.
    audio_cycles: u64,
    /// Decode cache for [`CODE_LO`, `CODE_HI`) (entries per halfword; bit 0 of the pc picks ARM/Thumb).
    pub icache: Vec<Option<crate::arm::Decoded>>,
    pub wram: Vec<u8>,
    /// 16 KB at 0: zero except our IRQ vector code at 0x18.
    pub bios: Vec<u8>,
    pub io: Vec<u8>,
    pub ime: u32,
    pub ie: u32,
    pub iflag: u32,
    pub timers: [Timer7; 4],
    /// Halted (HALTCNT / BIOS Halt / IntrWait) until an enabled interrupt is requested.
    pub halted: bool,
    /// BIOS IntrWait in progress: the flags to wait for in the BIOS check word (0x0380FFF8).
    pub intr_wait: Option<u32>,
    /// Next ARM9 word to receive (index into `ds_mem::Bus::ipc_out`).
    pub fifo_pos: usize,
    /// IPCFIFOCNT enables we keep (15 enable, 10 recv IRQ, 2 send IRQ) and the error flag (14).
    pub fifo_ctl: u32,
    pub last_recv: u32,
    /// IPCSYNC bits we keep: 8-11 our output, 14 our sync IRQ enable.
    pub sync_ctl: u32,
    /// Scanline last seen (VBlank / VCount interrupts).
    last_line: u64,
    /// Services nothing implements yet: what, how many times.
    pub unhandled: HashMap<String, u32>,
    /// The last pcs executed, and a copy of them taken when the ARM7 first ran from unmapped memory.
    pub history: std::collections::VecDeque<(u32, u32)>,
    /// SPI bus devices, RTC and cartridge backup (T085): I/O offsets 0x138, 0x1A0-0x1A2, 0x1C0-0x1C2.
    pub periph: crate::peripherals::Peripherals,
    /// The sound hardware behind I/O 0x400-0x50F (T062).
    pub spu: crate::spu::Spu,
    /// Memory behind the ARM7's VRAM window (0x06000000, 256 KB).
    pub vram7: Vec<u8>,
    pub crash: Option<Vec<(u32, u32)>>,
    /// Diagnostics: count executed pcs when set.
    pub profile: Option<HashMap<u32, u64>>,
    /// An undecodable instruction was met (its entry is in `unhandled`); checked every ARM9 slice.
    pub undecodable: bool,
}

pub struct Arm7 {
    pub cpu: Cpu,
    /// Next instruction (bit 0 = Thumb).
    pub pc: u32,
    pub s: State7,
    dispatcher: Option<crate::dispatch::Dispatcher>,
    diagnostic_interpreter: bool,
    pub execution_error: Option<String>,
    pub lifted_cycles: u64,
    pub interpreted_instructions: u64,
}

/// IRQ bits (same layout as the ARM9's for the shared sources).
pub mod irq {
    pub const VBLANK: u32 = 1 << 0;
    pub const HBLANK: u32 = 1 << 1;
    pub const VCOUNT: u32 = 1 << 2;
    pub const TIMER0: u32 = 1 << 3;
    pub const IPC_SYNC: u32 = 1 << 16;
    pub const IPC_SEND_EMPTY: u32 = 1 << 17;
    pub const IPC_RECV_NOT_EMPTY: u32 = 1 << 18;
}

impl Arm7 {
    /// Loads the ARM7 binary from the ROM header (offset 0x30, entry 0x34, RAM address 0x38, size 0x3C)
    /// into shared memory and sets the CPU up as the console's boot code leaves it (system mode,
    /// stacks at the top of ARM7 WRAM). Returns `None` if the header is unusable.
    pub fn from_rom(rom: &[u8], hw: &mut ds_mem::Bus) -> Option<Arm7> {
        let w = |o: usize| {
            rom.get(o..o + 4)
                .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
        };
        let (off, entry, ram, size) = (w(0x30)? as usize, w(0x34)?, w(0x38)?, w(0x3C)? as usize);
        let code = rom.get(off..off + size)?;
        let mut a = Arm7::new();
        {
            let mut v = View { s: &mut a.s, hw };
            for (i, &b) in code.iter().enumerate() {
                v.w8(ram.wrapping_add(i as u32), b);
            }
        }
        a.pc = entry & !3;
        Some(a)
    }

    pub fn new() -> Arm7 {
        let mut bios = vec![0u8; 0x4000];
        for (i, w) in rt::ARM7_IRQ_STUB.iter().enumerate() {
            bios[0x18 + 4 * i..0x1C + 4 * i].copy_from_slice(&w.to_le_bytes());
        }
        let mut cpu = Cpu::default();
        // stacks as the boot code leaves them (system/user, IRQ, supervisor)
        cpu.set_mode(0x12);
        cpu.r[13] = 0x0380_FFB0;
        cpu.set_mode(0x13);
        cpu.r[13] = 0x0380_FFDC;
        cpu.set_mode(0x1F);
        cpu.r[13] = 0x0380_FF00;
        Arm7 {
            cpu,
            pc: 0,
            dispatcher: None,
            diagnostic_interpreter: true,
            execution_error: None,
            lifted_cycles: 0,
            interpreted_instructions: 0,
            s: State7 {
                wram: vec![0; WRAM_SIZE],
                bios,
                io: vec![0; IO_SIZE],
                ime: 0,
                ie: 0,
                iflag: 0,
                timers: [Timer7::default(); 4],
                halted: false,
                intr_wait: None,
                fifo_pos: 0,
                fifo_ctl: 0,
                last_recv: 0,
                sync_ctl: 0,
                last_line: 0,
                unhandled: HashMap::new(),
                history: std::collections::VecDeque::new(),
                periph: crate::peripherals::Peripherals::new(),
                spu: crate::spu::Spu::new(),
                vram7: vec![0; 0x4_0000],
                crash: None,
                profile: None,
                undecodable: false,
                audio_out: std::collections::VecDeque::new(),
                audio_cycles: 0,
                icache: vec![None; ((CODE_HI - CODE_LO) / 2) as usize],
            },
        }
    }

    /// Runs about `cycles` ARM7 cycles (one instruction counted as one cycle; halted time passes
    /// at once), delivering interrupts from the shared hardware. Call it as the ARM9 side's time advances.
    /// Renders `frames` stereo samples at the SPU output rate of what the sound channels are playing, reading sample
    /// data from memory as the hardware would.
    pub fn render_audio(&mut self, hw: &mut ds_mem::Bus, frames: usize) -> Vec<[i16; 2]> {
        let mut spu = std::mem::take(&mut self.s.spu);
        let out = {
            let mut v = View { s: &mut self.s, hw };
            spu.render(&mut v, frames)
        };
        self.s.spu = spu;
        out
    }

    pub fn run(&mut self, hw: &mut ds_mem::Bus, cycles: u32) {
        self.run_cpu(hw, cycles);
        // the sound hardware runs with time whether or not anyone listens: one-shot channels must end
        // (the game waits for that) even in a headless run
        const ARM7_HZ: u64 = ds_mem::BUS_HZ;
        self.s.audio_cycles += cycles as u64 * self.s.spu.output_rate() as u64;
        let frames = self.s.audio_cycles / ARM7_HZ;
        self.s.audio_cycles %= ARM7_HZ;
        if frames > 0 {
            let out = self.render_audio(hw, frames as usize);
            self.s.audio_out.extend(out);
            let cap = self.s.spu.output_rate() as usize; // one second
            if self.s.audio_out.len() > cap {
                let drop = self.s.audio_out.len() - cap;
                self.s.audio_out.drain(..drop);
            }
        }
    }

    /// Install verified generated ARM7 code. Missing targets fail closed unless explicitly diagnostic.
    pub fn configure_lifted(&mut self, table: &'static [crate::mem::Lifted], diagnostic_interpreter: bool) {
        self.dispatcher = Some(crate::dispatch::Dispatcher::new(table));
        self.diagnostic_interpreter = diagnostic_interpreter;
    }

    fn run_cpu(&mut self, hw: &mut ds_mem::Bus, cycles: u32) {
        if self.execution_error.is_some() || self.s.crash.is_some() { return; }
        let mut left = cycles;
        while left > 0 {
            self.s.poll(hw);
            if self.s.halted {
                if self.s.ie & self.s.iflag != 0 {
                    self.s.halted = false;
                } else {
                    self.s.tick_timers(left as u64);
                    return;
                }
            }
            if rt::irq_ready(&self.cpu, self.s.ime, self.s.ie, self.s.iflag) {
                self.pc = rt::take_irq(&mut self.cpu, self.pc, 0);
            }
            // a short burst between interrupt checks
            let n = left.min(64);
            let mut v = View { s: &mut self.s, hw };
            let mut spent = 0u32;
            while spent < n {
                if v.s.history.len() >= 48 {
                    v.s.history.pop_front();
                }
                v.s.history.push_back((self.pc, self.cpu.cpsr()));
                if v.s.crash.is_none() && !matches!(self.pc >> 24, 0x00 | 0x02 | 0x03 | 0x06) {
                    v.s.crash = Some(v.s.history.iter().copied().collect());
                    // stop here, so the state at the crash is kept for the report
                    return;
                }
                if let Some(p) = v.s.profile.as_mut() {
                    *p.entry(self.pc).or_insert(0) += 1;
                }
                if let Some(f) = self.dispatcher.as_ref().and_then(|d| d.get(self.pc)) {
                    let budget = n - spent;
                    self.cpu.budget = budget;
                    let next = f(&mut self.cpu, &mut v, self.pc);
                    let Some(used) = budget.checked_sub(self.cpu.budget) else {
                        self.execution_error = Some(format!("ARM7 budget increased at {:#010x}", self.pc));
                        return;
                    };
                    if used == 0 {
                        self.execution_error = Some(format!("ARM7 lift made no progress at {:#010x}", self.pc));
                        return;
                    }
                    self.pc = if next == rt::BUDGET_EXHAUSTED { self.cpu.r[15] } else { next };
                    spent += used;
                    self.lifted_cycles += used as u64;
                    // Return to the outer loop for IRQ and halt checks after every lifted call.
                    break;
                }
                if !self.diagnostic_interpreter || cfg!(not(feature = "interpreter")) {
                    let at = self.dispatcher.as_ref().map_or_else(|| "no ARM7 table installed".into(), |d| d.describe(self.pc));
                    self.execution_error = Some(format!("No lifted ARM7 block at {:#010x}: {at}", self.pc));
                    return;
                }
                #[cfg(feature = "interpreter")]
                {
                let d = match self.pc & !1 {
                    a if (CODE_LO..CODE_HI).contains(&a) => {
                        let slot = ((a - CODE_LO) / 2) as usize;
                        match v.s.icache[slot] {
                            Some(d) if d.thumb == (self.pc & 1 != 0) => d,
                            _ => {
                                let d = interp::fetch(&mut v, self.pc);
                                v.s.icache[slot] = Some(d);
                                d
                            }
                        }
                    }
                    _ => interp::fetch(&mut v, self.pc),
                };
                if let crate::arm::Inst::Undecoded(w) = d.inst {
                    *v.s.unhandled
                        .entry(format!("undecodable {w:#010X} at {:#010X}", self.pc))
                        .or_insert(0) += 1;
                    v.s.undecodable = true;
                    v.s.halted = true;
                    break;
                }
                let passed = rt::cond(d.cond, &self.cpu);
                let fall = self.pc.wrapping_add(d.size);
                self.pc = interp::exec(&d, self.pc, &mut self.cpu, &mut v);
                self.interpreted_instructions += 1;
                spent += cost(&d, passed, self.pc != fall);
                if v.s.halted {
                    break;
                }
                }
            }
            let spent = spent.min(left);
            self.s.tick_timers(spent as u64);
            self.s.resume_intr_wait(hw, &mut self.cpu);
            left -= spent.max(1).min(left);
        }
    }
}

/// ARM7TDMI cycles for one instruction (code and data in 32-bit WRAM: no wait states): 1 per
/// sequential cycle, +1 internal for register-specified shifts and loads, a refill (+2) when the pc is
/// written, one per transferred register for LDM/STM. A failed condition costs one cycle.
#[cfg(feature = "interpreter")]
fn cost(d: &crate::arm::Decoded, passed: bool, jumped: bool) -> u32 {
    rt::arm7_cycles(d, passed, jumped)
}

impl Default for Arm7 {
    fn default() -> Self {
        Self::new()
    }
}

impl State7 {
    fn note(&mut self, what: String) {
        *self.unhandled.entry(what).or_insert(0) += 1;
    }

    fn raise(&mut self, bits: u32) {
        self.iflag |= bits;
    }

    /// Shared-hardware events since the last look: ARM9 FIFO words, sync IRQ requests, VBlank/VCount.
    fn poll(&mut self, hw: &mut ds_mem::Bus) {
        if self.fifo_pos < hw.ipc_out.len() && self.fifo_ctl & 0x8400 == 0x8400 {
            self.raise(irq::IPC_RECV_NOT_EMPTY);
        }
        if std::mem::take(&mut hw.sync_irq_to_arm7) && self.sync_ctl & 0x4000 != 0 {
            self.raise(irq::IPC_SYNC);
        }
        let line = hw.line;
        if line != self.last_line {
            let stat = u16::from_le_bytes([self.io[4], self.io[5]]) as u32;
            if line == 192 && stat & 8 != 0 {
                self.raise(irq::VBLANK);
            }
            let target = (stat >> 8) | ((stat & 0x80) << 1);
            if line as u32 == target && stat & 0x20 != 0 {
                self.raise(irq::VCOUNT);
            }
            self.last_line = line;
        }
    }

    fn tick_timers(&mut self, cycles: u64) {
        let mut overflow_prev = 0u64;
        for t in 0..4 {
            let tm = &mut self.timers[t];
            if tm.ctrl & 0x80 == 0 {
                overflow_prev = 0;
                continue;
            }
            let ticks = if tm.ctrl & 4 != 0 && t > 0 {
                overflow_prev
            } else {
                let shift = [0, 6, 8, 10][(tm.ctrl & 3) as usize];
                tm.acc += cycles;
                let k = tm.acc >> shift;
                tm.acc -= k << shift;
                k
            };
            let mut overflows = 0u64;
            let mut c = tm.counter as u64 + ticks;
            while c > 0xFFFF {
                overflows += 1;
                c = c - 0x1_0000 + tm.reload as u64;
            }
            tm.counter = c as u16;
            if overflows > 0 && tm.ctrl & 0x40 != 0 {
                self.iflag |= irq::TIMER0 << t;
            }
            overflow_prev = overflows;
        }
    }

    /// BIOS IntrWait: when an interrupt has set one of the waited-for flags in the BIOS check word
    /// (0x0380FFF8, written by the game's IRQ handler), clear them and continue; otherwise halt again.
    fn resume_intr_wait(&mut self, hw: &mut ds_mem::Bus, _c: &mut Cpu) {
        let Some(mask) = self.intr_wait else { return };
        if self.halted {
            return;
        }
        let mut v = View { s: self, hw };
        let flags = v.r32(0x0380_FFF8);
        if flags & mask != 0 {
            v.w32(0x0380_FFF8, flags & !mask);
            v.s.intr_wait = None;
        } else {
            v.s.halted = true;
        }
    }

    fn io_read(&mut self, hw: &mut ds_mem::Bus, off: u32) -> u32 {
        if let Some(v) = self.spu.read(off) {
            return v;
        }
        if let Some(mut v) = self.periph.read(off) {
            if off == 0x1A0 || off == 0x1C0 {
                v |= self.periph.read(off + 2).unwrap_or(0) << 16; // control and data share a word
            }
            return v;
        }
        match off {
            0x004 => {
                let stat = u16::from_le_bytes([self.io[4], self.io[5]]) as u32 & 0xFFB8;
                let line = hw.line as u32;
                let target = (stat >> 8) | ((stat & 0x80) << 1);
                // DISPSTAT, with VCOUNT in the upper half (halfword reads at +6 come through here)
                stat | (line >= 192 && line < 262) as u32
                    | ((line == target) as u32) << 2
                    | line << 16
            }
            0x006 => hw.line as u32,
            0x100 | 0x104 | 0x108 | 0x10C => {
                let t = &self.timers[((off - 0x100) / 4) as usize];
                t.counter as u32 | (t.ctrl as u32) << 16
            }
            0x102 | 0x106 | 0x10A | 0x10E => self.timers[((off - 0x102) / 4) as usize].ctrl as u32,
            0x130 => hw.r16(0x0400_0130) as u32,
            // RCNT, with EXTKEYIN (X, Y, debug, pen, hinge) in the upper half: `read` asks for the
            // aligned word, so a halfword read of 0x136 arrives here
            0x134 => u16::from_le_bytes([self.io[0x134], self.io[0x135]]) as u32 | (hw.extended_keys() as u32) << 16,
            0x180 => self.sync_ctl & 0x4F00 | hw.arm9_sync_out() as u32,
            0x184 => {
                let recv_empty = self.fifo_pos >= hw.ipc_out.len();
                let recv_full = hw.ipc_out.len() - self.fifo_pos.min(hw.ipc_out.len()) >= 16;
                let send_empty = hw.fifo_in.is_empty();
                let send_full = hw.fifo_in.len() >= 16;
                self.fifo_ctl & 0xC404
                    | send_empty as u32
                    | (send_full as u32) << 1
                    | (recv_empty as u32) << 8
                    | (recv_full as u32) << 9
            }
            0x208 => self.ime,
            0x210 => self.ie,
            0x214 => self.iflag,
            0x241 => hw.wramcnt() as u32,
            0x240 => {
                // VRAMSTAT: banks C / D mapped to the ARM7
                let mut m = |r: u32| hw.r8(0x0400_0000 + r) & 0x87 == 0x82;
                m(0x242) as u32 | (m(0x243) as u32) << 1 | (hw.wramcnt() as u32) << 8
            }
            0x300 => 1, // POSTFLG: booted
            _ => {
                if !(0x400..0x520).contains(&off) && !matches!(off, 0x134 | 0x204 | 0x301 | 0x304) {
                    self.note(format!("io read {:#06X}", off));
                }
                let o = off as usize;
                u32::from_le_bytes([self.io[o], self.io[o + 1], self.io[o + 2], self.io[o + 3]])
            }
        }
    }

    fn io_write(&mut self, hw: &mut ds_mem::Bus, off: u32, v: u32, n: usize) {
        if self.spu.write(off, v, n) {
            if let Some(p) = hw.probe.as_mut() {
                p.spu_write(&hw.main, off, v, n); // Block 5.0 oracle (off by default)
            }
            return;
        }
        if self.periph.write(off, v, n) {
            return;
        }
        match (off, n) {
            (0x102 | 0x106 | 0x10A | 0x10E, _) | (0x100 | 0x104 | 0x108 | 0x10C, 4) => {
                let t = ((off & 0xC) / 4) as usize;
                let ctrl = if off & 2 != 0 { v } else { v >> 16 } as u16;
                let tm = &mut self.timers[t];
                if off & 2 == 0 {
                    tm.reload = v as u16;
                }
                if tm.ctrl & 0x80 == 0 && ctrl & 0x80 != 0 {
                    tm.counter = tm.reload;
                    tm.acc = 0;
                }
                tm.ctrl = ctrl;
            }
            (0x100 | 0x104 | 0x108 | 0x10C, _) => {
                self.timers[((off - 0x100) / 4) as usize].reload = v as u16
            }
            (0x180, _) => {
                self.sync_ctl = v & 0x4F00;
                hw.arm7_set_sync(((v >> 8) & 0xF) as u8, v & 0x2000 != 0);
            }
            (0x181, 1) => {
                self.sync_ctl = (v << 8) & 0x4F00;
                hw.arm7_set_sync((v & 0xF) as u8, v & 0x20 != 0);
            }
            (0x184, _) => {
                if v & 0x4000 != 0 {
                    self.fifo_ctl &= !0x4000;
                }
                if v & 8 != 0 {
                    hw.fifo_in.clear(); // clear our send FIFO
                }
                self.fifo_ctl = self.fifo_ctl & 0x4000 | v & 0x8404;
                if self.fifo_ctl & 0x8400 == 0x8400 && self.fifo_pos < hw.ipc_out.len() {
                    self.raise(irq::IPC_RECV_NOT_EMPTY);
                }
            }
            (0x188, _) => hw.arm7_send(v),
            (0x208, _) => self.ime = v & 1,
            (0x210, _) => self.ie = v,
            (0x214, _) => self.iflag &= !v,
            (0x301, _) if v & 0xC0 == 0x80 => self.halted = true,
            _ => {
                if !(0x400..0x520).contains(&off) && !matches!(off, 0x134 | 0x204 | 0x304 | 0x004) {
                    self.note(format!("io write {:#06X}", off));
                }
                let o = off as usize;
                for i in 0..n {
                    self.io[o + i] = (v >> (8 * i)) as u8;
                }
            }
        }
    }
}

/// The ARM7's view of memory.
pub struct View<'a> {
    pub s: &'a mut State7,
    pub hw: &'a mut ds_mem::Bus,
}

enum Region {
    Bios(usize),
    Main(usize),
    Shared(usize),
    Wram(usize),
    Vram7(usize),
    Io(u32),
    None,
}

impl View<'_> {
    fn region(&self, a: u32) -> Region {
        match a >> 24 {
            0x00 if a < 0x4000 => Region::Bios(a as usize),
            0x02 => Region::Main(a as usize & (ds_mem::MAIN_RAM_SIZE - 1)),
            0x03 if a < 0x0380_0000 => match self.hw.wramcnt() {
                // shared WRAM given to the ARM7: all, or one half; none mirrors ARM7 WRAM
                3 => Region::Shared(a as usize & 0x7FFF),
                1 => Region::Shared(a as usize & 0x3FFF),
                2 => Region::Shared(0x4000 + (a as usize & 0x3FFF)),
                _ => Region::Wram(a as usize & (WRAM_SIZE - 1)),
            },
            0x03 => Region::Wram(a as usize & (WRAM_SIZE - 1)),
            0x04 if a < 0x0400_1000 => Region::Io(a & 0xFFF),
            // The ARM7's VRAM window (banks C / D mapped to it by VRAMCNT MST 2). The ARM7 start-up copies a
            // 100 KB block of its own code and data there before the ARM9 has finished its VRAM set-up (the ARM9
            // maps and clears banks with the ARM7 already running), so the window is backed by its own memory
            // that the ARM9's VRAMCNT changes cannot wipe. (T087)
            0x06 if a < 0x0604_0000 => Region::Vram7((a & 0x3_FFFF) as usize),
            _ => Region::None,
        }
    }

    fn read(&mut self, a: u32, n: usize) -> u32 {
        if a & !3 == 0x0410_0000 {
            // IPCFIFORECV: the ARM9's next word
            if self.s.fifo_pos < self.hw.ipc_out.len() {
                self.s.last_recv = self.hw.ipc_out[self.s.fifo_pos];
                self.s.fifo_pos += 1;
            } else {
                self.s.fifo_ctl |= 0x4000;
            }
            return self.s.last_recv;
        }
        let get = |buf: &[u8], o: usize| {
            (0..n).fold(0u32, |v, i| {
                v | (buf[(o + i) % buf.len()] as u32) << (8 * i)
            })
        };
        match self.region(a) {
            Region::Bios(o) => get(&self.s.bios, o),
            Region::Main(o) => get(&self.hw.main, o),
            Region::Shared(o) => get(&self.hw.swram, o),
            Region::Wram(o) => get(&self.s.wram, o),
            Region::Vram7(o) => get(&self.s.vram7, o),
            Region::Io(off) => {
                let v = self.s.io_read(self.hw, off & !3);
                v >> (8 * (off & 3))
            }
            Region::None => {
                self.s.note(format!("read unmapped {:#04X}xxxxxx", a >> 24));
                0
            }
        }
    }

    fn write(&mut self, a: u32, v: u32, n: usize) {
        // code written over: drop its decoded instructions (a Thumb BL pair spans the halfword before)
        if (CODE_LO..CODE_HI).contains(&a) {
            let first = ((a & !1).saturating_sub(2).max(CODE_LO) - CODE_LO) / 2;
            let last = (((a + n as u32 - 1) & !1) - CODE_LO) / 2;
            for i in first..=last.min((CODE_HI - CODE_LO) / 2 - 1) {
                self.s.icache[i as usize] = None;
            }
        }
        let put = |buf: &mut [u8], o: usize| {
            let len = buf.len();
            for i in 0..n {
                buf[(o + i) % len] = (v >> (8 * i)) as u8;
            }
        };
        match self.region(a) {
            Region::Main(o) => {
                self.hw.note_main_write(o, n);
                put(&mut self.hw.main, o);
            }
            Region::Shared(o) => put(&mut self.hw.swram, o),
            Region::Wram(o) => put(&mut self.s.wram, o),
            Region::Vram7(o) => put(&mut self.s.vram7, o),
            Region::Io(off) => self.s.io_write(self.hw, off, v, n),
            Region::None
                if a >> 24 == 6
                    && std::env::var_os("MKDS_IO_TRACE").is_some()
                    && self
                        .s
                        .unhandled
                        .get("write unmapped 0x06000000")
                        .copied()
                        .unwrap_or(0)
                        < 3 =>
            {
                eprintln!(
                    "[io] ARM7 write to unmapped VRAM {a:#010X} = {v:#X} at bus cycle {}",
                    self.hw.cycles
                );
                self.s
                    .note(format!("write unmapped {:#010X}", a & 0xFF00_0000));
            }
            Region::Bios(_) | Region::None => self
                .s
                .note(format!("write unmapped {:#010X}", a & 0xFF00_0000)),
        }
    }
}

impl Mem for View<'_> {
    fn r8(&mut self, a: u32) -> u8 {
        self.read(a, 1) as u8
    }
    fn r16(&mut self, a: u32) -> u16 {
        self.read(a, 2) as u16
    }
    fn r32(&mut self, a: u32) -> u32 {
        self.read(a, 4)
    }
    fn w8(&mut self, a: u32, v: u8) {
        self.write(a, v as u32, 1)
    }
    fn w16(&mut self, a: u32, v: u16) {
        self.write(a, v as u32, 2)
    }
    fn w32(&mut self, a: u32, v: u32) {
        self.write(a, v, 4)
    }
}

impl Bus for View<'_> {
    /// Calls are flat: continue at the target (the return comes back through lr like any jump).
    fn call(&mut self, _c: &mut Cpu, target: u32, _ret: u32) -> u32 {
        target
    }

    fn swi(&mut self, c: &mut Cpu, n: u32) {
        match n {
            0x03 => {} // WaitByLoop
            0x04 | 0x05 => {
                // IntrWait (r0: discard old flags, r1: mask) / VBlankIntrWait
                let (discard, mask) = if n == 5 {
                    (true, irq::VBLANK)
                } else {
                    (c.r[0] != 0, c.r[1])
                };
                if discard {
                    let f = self.r32(0x0380_FFF8);
                    self.w32(0x0380_FFF8, f & !mask);
                }
                self.s.intr_wait = Some(mask);
                self.s.halted = true;
                c.ctl &= !0x80; // the BIOS waits with interrupts enabled
                self.s.ime = 1;
            }
            0x06 => self.s.halted = true,
            0x08 => {} // SoundBias: ramps SOUNDBIAS; the sound output is not modelled
            0x09 => {
                let (num, den) = (c.r[0] as i32, c.r[1] as i32);
                if den == 0 {
                    self.s.note("swi Div by zero".into());
                } else {
                    let q = num.wrapping_div(den);
                    c.r[0] = q as u32;
                    c.r[1] = num.wrapping_rem(den) as u32;
                    c.r[3] = q.unsigned_abs();
                }
            }
            0x0B | 0x0C => {
                // CpuSet / CpuFastSet: r0 source, r1 destination, r2 count (bits 0..20) | fill (24) | 32-bit (26)
                let (mut src, mut dst, ctl) = (c.r[0], c.r[1], c.r[2]);
                let word = n == 0x0C || ctl & (1 << 26) != 0;
                let fill = ctl & (1 << 24) != 0;
                let mut count = ctl & 0x1F_FFFF;
                if n == 0x0C {
                    count = (count + 7) & !7;
                }
                let step = if word { 4 } else { 2 };
                let first = if word {
                    self.r32(src & !3)
                } else {
                    self.r16(src & !1) as u32
                };
                for _ in 0..count {
                    let v = if fill {
                        first
                    } else if word {
                        self.r32(src & !3)
                    } else {
                        self.r16(src & !1) as u32
                    };
                    if word {
                        self.w32(dst & !3, v);
                    } else {
                        self.w16(dst & !1, v as u16);
                    }
                    if !fill {
                        src = src.wrapping_add(step);
                    }
                    dst = dst.wrapping_add(step);
                }
            }
            0x0D => c.r[0] = (c.r[0] as u64).isqrt() as u32,
            0x0E => {
                // GetCRC16(r0 initial, r1 address, r2 length in bytes)
                // reflected CRC-16, polynomial 0xA001 (as the firmware's own CRCs)
                let mut crc = c.r[0] as u16;
                let (addr, len) = (c.r[1], c.r[2] & !1);
                for i in 0..len {
                    crc ^= self.r8(addr.wrapping_add(i)) as u16;
                    for _ in 0..8 {
                        crc = if crc & 1 != 0 {
                            (crc >> 1) ^ 0xA001
                        } else {
                            crc >> 1
                        };
                    }
                }
                c.r[0] = crc as u32;
            }
            0x0F => c.r[0] = 0, // IsDebugger
            // GetPitchTable(i, 0..767): round(65536 * 2^(i/768)) - 65536. Verified against all 768 entries
            // of the console BIOS (5_tools/bizhawk/bios_tables_check_lead.lua; no BIOS data is used).
            0x1B => {
                let i = (c.r[0] as i32).clamp(0, 767) as f64;
                c.r[0] = ((65536.0 * (i / 768.0).exp2()).round() as u32).wrapping_sub(65536);
            }
            // GetVolumeTable(i, 0..723) and GetSineTable(i, 0..63): APPROXIMATIONS, not yet verified against
            // the console (0.1 dB volume steps, the SDK's own shift handles the lower ranges; quarter sine).
            0x1C => {
                let i = (c.r[0] as i32).clamp(0, 723) as f64;
                let db = (i - 723.0) / 10.0;
                let mut v = 127.0 * 10f64.powf(db / 20.0);
                for (limit, k) in [(-6.0, 2.0), (-12.0, 2.0), (-24.0, 4.0)] {
                    if db < limit {
                        v *= k;
                    }
                }
                c.r[0] = v.round().clamp(0.0, 127.0) as u32;
                self.s
                    .note("swi 0x1c GetVolumeTable (approximation)".into());
            }
            0x1A => {
                let i = (c.r[0] as i32).clamp(0, 63) as f64;
                c.r[0] = ((i * std::f64::consts::PI / 128.0).sin() * 32768.0).round() as u32;
                self.s.note("swi 0x1a GetSineTable (approximation)".into());
            }
            _ => self.s.note(format!("swi {n:#04x}")),
        }
    }

    fn mrc(&mut self, _c: &mut Cpu, cp: u8, _op1: u8, _crn: u8, _crm: u8, _op2: u8) -> u32 {
        self.s.note(format!("mrc p{cp} on the ARM7"));
        0
    }

    fn mcr(&mut self, _c: &mut Cpu, cp: u8, _op1: u8, _crn: u8, _crm: u8, _op2: u8, _v: u32) {
        self.s.note(format!("mcr p{cp} on the ARM7"));
    }
}

#[cfg(test)]
mod tests {
    fn test_lift(c: &mut crate::rt::Cpu, m: &mut dyn crate::rt::Bus, pc: u32) -> u32 {
        if pc == 0x18 { c.r[2] += 1; }
        if !crate::rt::spend(c, 3) { c.r[15] = pc; return crate::rt::BUDGET_EXHAUSTED; }
        c.r[0] += 1;
        m.swi(c, 6);
        pc + 4
    }
    static TEST_LIFT: &[crate::mem::Lifted] = &[crate::mem::Lifted {
        segment: "bios7", entry: 0x18, blocks: &[0x18, 0x03800000], insts: &[0x18, 0x03800000], f: test_lift,
    }];

    #[test]
    fn audio_pacing_uses_output_rate_and_caps_one_second() {
        for rate in [crate::spu::SAMPLE_RATE, 48000] {
            let mut a = super::Arm7::new();
            let mut hw = crate::ds_mem::Bus::new();
            a.s.halted = true;
            a.s.spu.set_output_rate(rate);
            let mut cycles = 0u64;
            for chunk in [1, 511, 65536, 123456, 1000000] {
                a.run(&mut hw, chunk);
                cycles += chunk as u64;
                assert_eq!(a.s.audio_out.len() as u64, cycles * rate as u64 / crate::ds_mem::BUS_HZ);
            }
            a.run(&mut hw, crate::ds_mem::BUS_HZ as u32);
            assert_eq!(a.s.audio_out.len(), rate as usize);
        }
    }

    #[test]
    fn lifted_arm7_halts_and_accounts_cycles_without_interpreting() {
        let mut a = super::Arm7::new();
        let mut hw = crate::ds_mem::Bus::new();
        a.pc = 0x03800000;
        a.configure_lifted(TEST_LIFT, false);
        a.run_cpu(&mut hw, 64);
        assert_eq!(a.pc, 0x03800004);
        assert_eq!(a.cpu.r[0], 1);
        assert_eq!(a.lifted_cycles, 3);
        assert_eq!(a.interpreted_instructions, 0);
        assert!(a.execution_error.is_none());
        assert!(a.s.halted);
    }

    #[test]
    fn lifted_arm7_takes_irq_before_dispatch() {
        let mut a = super::Arm7::new();
        let mut hw = crate::ds_mem::Bus::new();
        a.pc = 0x03800000;
        a.s.ime = 1; a.s.ie = 1; a.s.iflag = 1;
        a.configure_lifted(TEST_LIFT, false);
        a.run_cpu(&mut hw, 3);
        assert_eq!(a.cpu.r[2], 1);
        assert_eq!(a.cpu.ctl & 31, 0x12);
        assert_eq!(a.pc, 0x1c);
    }

    #[test]
    #[cfg(feature = "interpreter")]
    fn missing_arm7_lift_fails_closed_and_diagnostic_fallback_is_counted() {
        let mut a = super::Arm7::new();
        let mut hw = crate::ds_mem::Bus::new();
        a.configure_lifted(&[], false);
        a.run_cpu(&mut hw, 1);
        assert!(a.execution_error.as_ref().unwrap().contains("No lifted ARM7 block"));
        assert_eq!(a.interpreted_instructions, 0);
        let mut b = super::Arm7::new();
        b.configure_lifted(&[], true);
        b.run_cpu(&mut hw, 1);
        assert_eq!(b.interpreted_instructions, 1);
        assert!(b.execution_error.is_none());
    }

    #[test]
    fn lifted_budget_resume_uses_saved_pc() {
        fn resume(c: &mut crate::rt::Cpu, _: &mut dyn crate::rt::Bus, _: u32) -> u32 {
            c.budget = 0;
            c.r[15] = 0x03800004;
            crate::rt::BUDGET_EXHAUSTED
        }
        static TABLE: &[crate::mem::Lifted] = &[crate::mem::Lifted {
            segment: "arm7_main", entry: 0x03800000, blocks: &[0x03800000], insts: &[0x03800000], f: resume,
        }];
        let mut a = super::Arm7::new();
        a.pc = 0x03800000;
        a.configure_lifted(TABLE, false);
        a.run_cpu(&mut crate::ds_mem::Bus::new(), 1);
        assert_eq!(a.pc, 0x03800004);
        assert_eq!(a.lifted_cycles, 1);
    }
    #[cfg(feature = "interpreter")]
    use super::*;

    #[test]
    #[cfg(feature = "interpreter")]
    fn ipc_sync_and_fifo_between_the_cpus() {
        let mut hw = ds_mem::Bus::new();
        hw.arm7_lle = true;
        let mut a = Arm7::new();
        // ARM7 code at 0x03800000: write sync nibble 5 with an IRQ request, send 0x1234 to the ARM9,
        // then read the ARM9's first FIFO word into r2, and halt.
        let code: [u32; 9] = [
            0xE3A0_0301, // mov r0, #0x04000000
            0xE3A0_1C25, // mov r1, #0x2500
            0xE580_1180, // str r1, [r0, #0x180]
            0xE3A0_1A01, // mov r1, #0x1000
            0xE281_1F8D, // add r1, r1, #0x234
            0xE580_1188, // str r1, [r0, #0x188]
            0xE280_0601, // add r0, r0, #0x100000
            0xE590_2000, // ldr r2, [r0]
            0xEAFF_FFFE, // b .
        ];
        {
            let mut v = View {
                s: &mut a.s,
                hw: &mut hw,
            };
            for (i, w) in code.iter().enumerate() {
                v.w32(0x0380_0000 + 4 * i as u32, *w);
            }
        }
        hw.ipc_out.push(0xBEEF); // the ARM9 sent a word
        hw.w16(ds_mem::IO_BASE + 0x180, 0x4000); // the ARM9 enables its sync IRQ
        a.pc = 0x0380_0000;
        a.run(&mut hw, 20);
        assert_eq!(
            hw.r16(ds_mem::IO_BASE + 0x180) & 0xF,
            5,
            "ARM9 sees the ARM7's nibble"
        );
        assert!(
            hw.r32(ds_mem::IO_BASE + 0x214) & ds_mem::irq::IPC_SYNC != 0,
            "sync IRQ reached the ARM9"
        );
        assert_eq!(hw.fifo_in.front(), Some(&0x1234));
        assert_eq!(a.cpu.r[2], 0xBEEF);
    }

    #[test]
    fn extkeyin_halfword_read_sees_x_y_and_pen() {
        // The SDK's ARM7 pad alarm reads 0x04000136 as a halfword and copies X/Y/debug to 0x027FFFA8;
        // reading 0 there makes the ARM9 see X and Y held forever (L then never triggers an item).
        let mut hw = ds_mem::Bus::new();
        let mut a = Arm7::new();
        let mut v = View { s: &mut a.s, hw: &mut hw };
        assert_eq!(v.r16(0x0400_0136), 0x007F, "nothing held, pen up, lid open");
        v.hw.set_input(crate::input::HostInput { buttons: 1 << 10, pen_down: true, ..Default::default() });
        assert_eq!(v.r16(0x0400_0136), 0x003E, "X held and pen down read low");
        assert_eq!(v.r32(0x0400_0134) >> 16, 0x003E);
    }
}
