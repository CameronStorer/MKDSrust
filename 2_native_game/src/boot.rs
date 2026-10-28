//! Two-CPU boot probe (T070): the ARM9 (lifted code, interpreter fallback, flat calls, IRQs through our
//! vector code) and the game's own ARM7 (interpreted) run side by side over the shared hardware.
//! Reports where each CPU is, the hottest ARM9 pcs and what nothing implements yet.
//!
//! Usage, inputs and diagnostic scripts: https://github.com/CameronStorer/MKDSrust-Docs.
//! Diagnostics by environment: MKDS_BOOT_INTERP, SLICE, MKDS_PROBE, MKDS_WATCH, ARM7_DUMP.

#[cfg(feature = "interpreter")]
use crate::arm7::Arm7;
#[cfg(feature = "interpreter")]
use crate::dispatch::Dispatcher;
use crate::ds_mem::Bus;
use crate::machine::Machine;
#[cfg(feature = "interpreter")]
use crate::mem::Lifted;
use crate::rt::{Bus as RtBus, Cpu, Mem};
#[cfg(feature = "interpreter")]
use crate::rt;
#[cfg(feature = "interpreter")]
use std::collections::HashMap;

/// Resumable runtime used by the native host; the diagnostic probe remains available.
#[path = "session.rs"]
pub mod session;

/// Calls are jumps (flat): thread switches and longjmp-style returns need no Rust stack.
pub struct Flat<'a, 'b> {
    m: &'b mut Machine<'a>,
    /// Diagnostics: report writes to this word (MKDS_WATCH) with the pc doing them.
    watch: Option<u32>,
    pc: u32,
}
impl Flat<'_, '_> {
    fn check(&self, a: u32, v: u32) {
        if self.watch.is_some_and(|w| a >= w && a < w + 0x40) {
            eprintln!("watch: [{a:#010X}] = {v:#010X} at pc {:#010X}", self.pc);
        }
    }
}
impl Mem for Flat<'_, '_> {
    fn r8(&mut self, a: u32) -> u8 { self.m.r8(a) }
    fn r16(&mut self, a: u32) -> u16 { self.m.r16(a) }
    fn r32(&mut self, a: u32) -> u32 { self.m.r32(a) }
    fn w8(&mut self, a: u32, v: u8) { self.check(a, v as u32); self.m.w8(a, v) }
    fn w16(&mut self, a: u32, v: u16) { self.check(a, v as u32); self.m.w16(a, v) }
    fn w32(&mut self, a: u32, v: u32) { self.check(a, v); self.m.w32(a, v) }
}
impl RtBus for Flat<'_, '_> {
    fn call(&mut self, _c: &mut Cpu, target: u32, _ret: u32) -> u32 { target }
    fn swi(&mut self, c: &mut Cpu, n: u32) { self.m.swi(c, n) }
    fn mrc(&mut self, c: &mut Cpu, cp: u8, o1: u8, n: u8, cm: u8, o2: u8) -> u32 { self.m.mrc(c, cp, o1, n, cm, o2) }
    fn mcr(&mut self, c: &mut Cpu, cp: u8, o1: u8, n: u8, cm: u8, o2: u8, v: u32) { self.m.mcr(c, cp, o1, n, cm, o2, v) }
}

/// What the console's boot code leaves in the shared area at 0x027FF800 / 0x027FFC00 before the game
/// starts: the ROM header copy, the cartridge chip ID and the boot flag (values seen on the console;
/// firmware user settings at 0x027FFC80 are not provided yet).
pub fn boot_shared_area(hw: &mut Bus, rom: &[u8]) {
    for (i, &b) in rom[..0x170].iter().enumerate() {
        hw.w8(0x027F_FE00 + i as u32, b);
    }
    for base in [0x027F_F800u32, 0x027F_FC00] {
        hw.w32(base, 0x0000_1FC2); // cartridge chip ID
        hw.w32(base + 4, 0x0000_1FC2);
    }
    hw.w16(0x027F_FC08, u16::from_le_bytes([rom[0x15E], rom[0x15F]])); // header CRC
    hw.w16(0x027F_FC0A, u16::from_le_bytes([rom[0x6C], rom[0x6D]])); // secure-area CRC
    hw.w32(0x027F_FC40, 1); // booted from the cartridge
    hw.w8(0x0400_0247, 3); // WRAMCNT as the boot code leaves it: shared WRAM all to the ARM7
    hw.w8(0x0400_0300, 1); // POSTFLG: booted (the SDK's start-up branches on it, e.g. sub_200C6B8)
}

/// Boots the user's ROM (`arm9_image`: its decompressed ARM9 binary) with the given lifted functions for
/// `budget_m` million ARM9 instructions and prints where both CPUs are and what is not implemented yet.
/// Diagnostics probe: interprets whatever is not lifted (needs the `interpreter` feature).
#[cfg(feature = "interpreter")]
pub fn probe(rom: &[u8], image: Vec<u8>, table: &'static [Lifted], budget_m: u64) {
    let mut hw = Bus::new();
    hw.arm7_lle = true;
    // As the console does: the whole ARM9 binary (decompressed here, autoload sources included) at its
    // RAM address; the game's start-up code itself copies the ITCM / DTCM sections once they are mapped.
    let base = u32::from_le_bytes(rom[0x28..0x2C].try_into().unwrap());
    for (i, &b) in image.iter().enumerate() {
        hw.w8(base.wrapping_add(i as u32), b);
    }
    let magic = [0x21u8, 0x06, 0xC0, 0xDE, 0xDE, 0xC0, 0x06, 0x21];
    if let Some(p) = image.windows(8).take(0x8000).position(|w| w == magic) {
        hw.w32(base + p as u32 - 0x1C + 0x14, 0); // already decompressed: crt0 must not unpack again
    }
    boot_shared_area(&mut hw, rom);
    hw.card = Some(crate::card::Card::new(rom.to_vec(), 0x0000_1FC2));
    let arm7 = Arm7::from_rom(rom, &mut hw).expect("ARM7 binary");
    println!("ARM7 entry {:#010X}", arm7.pc);
    let disp = if std::env::var_os("MKDS_BOOT_INTERP").is_some() { Dispatcher::new(&[]) } else { Dispatcher::new(table) };
    let mut machine = Machine::new(hw, &disp);
    machine.keep_all_audio = std::env::var_os("MKDS_WAV").is_some();
    let mut arm7 = arm7;
    arm7.s.profile = Some(HashMap::new());
    machine.arm7 = Some(arm7);

    let mut c = Cpu::default();
    c.r[13] = 0x0380_FF00;
    let mut pc = u32::from_le_bytes(rom[0x24..0x28].try_into().unwrap());
    let total = budget_m * 1_000_000;
    let (mut done, mut lifted_n, mut irqs) = (0u64, 0u64, 0u64);
    let mut hot: HashMap<u32, u64> = HashMap::new();
    let mut hist9: std::collections::VecDeque<(u32, u32, u32, u32)> = std::collections::VecDeque::new();
    let mut stop = String::from("budget used");
    let t0 = std::time::Instant::now();
    let autopress = std::env::var_os("MKDS_AUTOPRESS").is_some();
    // MKDS_HOT_THREADS: profile only system-mode code (threads, not IRQ handlers)
    let thread_only = std::env::var_os("MKDS_HOT_THREADS").is_some();
    let mut frames = 0u64;
    let probe: Vec<u32> = std::env::var("MKDS_PROBE").unwrap_or_default().split(',').filter_map(|v| u32::from_str_radix(v.trim().trim_start_matches("0x"), 16).ok()).collect();
    let watch = std::env::var("MKDS_WATCH").ok().and_then(|v| u32::from_str_radix(v.trim_start_matches("0x"), 16).ok());
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        while done < total {
            {
                let hw = &mut machine.hw;
                let (ime, ie, iflag) = (hw.r32(0x0400_0208), hw.r32(0x0400_0210), hw.r32(0x0400_0214));
                if rt::irq_ready(&c, ime, ie, iflag) {
                    pc = rt::take_irq(&mut c, pc, 0xFFFF_0000);
                    irqs += 1;
                }
            }
            let slice: u32 = std::env::var("SLICE").ok().and_then(|v| v.parse().ok()).unwrap_or(32);
            c.budget = slice;
            let mut f = Flat { m: &mut machine, watch, pc };
            if hist9.len() >= 4000 {
                hist9.pop_front();
            }
            hist9.push_back((pc, c.cpsr(), c.r[14], c.r[13]));
            if let Some(func) = disp.get(pc) {
                let r = func(&mut c, &mut f, pc);
                pc = if r == rt::BUDGET_EXHAUSTED { c.r[15] } else { r };
                lifted_n += 1;
            } else {
                while c.budget > 0 {
                    if !thread_only || c.ctl & 0x1F == 0x1F {
                        *hot.entry(pc).or_insert(0) += 1;
                    }
                    if hist9.len() >= 4000 {
                        hist9.pop_front();
                    }
                    hist9.push_back((pc, c.cpsr(), c.r[14], c.r[13]));
                    let d = crate::interp::fetch(&mut f, pc);
                    if let crate::arm::Inst::Undecoded(w) = d.inst {
                        stop = format!("undecodable {w:#010X} at {pc:#010X}");
                        let mut last_sp = 0;
                        for (p, ps, lr, sp) in &hist9 {
                            if *sp != last_sp {
                                eprintln!("  hist9 pc {p:#010X} cpsr {ps:#010X} lr {lr:#010X} sp {sp:#010X}");
                                last_sp = *sp;
                            }
                        }
                        return;
                    }
                    c.budget -= 1;
                    f.pc = pc;
                    if probe.contains(&pc) {
                        eprintln!("probe pc {pc:#010X} r0 {:#010X} sp {:#010X} r3 {:#010X} lr {:#010X} cpsr {:#010X}", c.r[0], c.r[13], c.r[3], c.r[14], c.cpsr());
                    }
                    pc = crate::interp::exec(&d, pc, &mut c, &mut f);
                    if disp.is_lifted(pc) {
                        break;
                    }
                }
            }
            let used = (slice - c.budget) as u64;
            if done / 1_000_000 != (done + used) / 1_000_000 {
                let a7 = machine.arm7.as_ref().map_or(0, |a| a.pc);
                eprintln!("[{}M] frame {frames} arm9 pc {pc:#010X} arm7 pc {a7:#010X} line {} cycles {} irqs {irqs} wall {:?}", (done + used) / 1_000_000, machine.hw.line, machine.hw.cycles, t0.elapsed());
            }
            // profile only the second half of the run (steady state, not boot work)
            if done < total / 2 && done + used >= total / 2 {
                hot.clear();
            }
            done += used;
            machine.tick(used.div_ceil(2));
            // frames from hardware time (263 lines x 2130 cycles), so long waits count every frame they cover
            let now = machine.hw.cycles / 560_190;
            if now != frames {
                frames = now;
                if autopress {
                    // MKDS_AUTOPRESS: every 120 frames hold A and touch the screen centre for 8 frames
                    let down = frames % 120 < 8;
                    machine.set_input(if down { 1 } else { 0 }, down.then_some((128, 96)));
                }
            }
        }
    }));
    if let Err(e) = r {
        stop = format!("panic: {}", e.downcast_ref::<String>().cloned().or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string())).unwrap_or_default());
    }
    println!("\n== ARM9: {stop} after {done} instructions ({lifted_n} lifted-function entries, {irqs} IRQs taken)");
    println!("pc {pc:#010X} lr {:#010X} sp {:#010X} cpsr {:#010X}", c.r[14], c.r[13], c.cpsr());
    machine.sync_gx();
    let layer3d = machine.render_frame3d();
    println!("3D: {} polygons in the last completed frame, {} pending, {} swaps, unknown cmds {:?}, {} 3D pixels", machine.frame_polys.len(), machine.gx.polygons.len(), machine.gx.swaps, machine.gx.unknown.iter().take(8).collect::<Vec<_>>(), layer3d.iter().filter(|c| **c & 0xFF != 0).count());
    println!("GX: {} command-port writes; offsets {:?}", machine.gx_writes, machine.gx_offsets.iter().take(12).collect::<Vec<_>>());
    let hw = &mut machine.hw;
    // MKDS_SHOT=<dir>: both screens' 2D output at the end of the run (engine A top / B bottom, as PPM)
    if let Ok(dir) = std::env::var("MKDS_SHOT") {
        for (engine, name) in [(0, "engine_a.ppm"), (1, "engine_b.ppm")] {
            let frame = crate::gfx2d::render_bus_with_3d(hw, engine, if engine == 0 { Some(&layer3d) } else { None });
            let path = std::path::Path::new(&dir).join(name);
            if std::fs::write(&path, crate::gfx2d::to_ppm(&frame)).is_ok() {
                println!("screen: {}", path.display());
            }
        }
        println!("DISPCNT A {:#010X}  B {:#010X}  POWCNT1 {:#06X}", hw.r32(0x0400_0000), hw.r32(0x0400_1000), hw.r16(0x0400_0304));
    }
    // MKDS_PEEK=addr,addr,...: the words at those addresses at the end of the run
    for a in std::env::var("MKDS_PEEK").unwrap_or_default().split(',').filter_map(|v| u32::from_str_radix(v.trim().trim_start_matches("0x"), 16).ok()) {
        let words: Vec<String> = (0..4).map(|i| format!("{:08X}", hw.r32(a + 4 * i))).collect();
        println!("peek {a:#010X}: {}", words.join(" "));
    }
    println!("ARM9 IME {} IE {:#X} IF {:#X}  scanline {}  cycles {}", hw.r32(0x0400_0208), hw.r32(0x0400_0210), hw.r32(0x0400_0214), hw.line, hw.cycles);
    println!("PXI ready masks: ARM9 {:#X}  ARM7 {:#X}", hw.r32(0x027F_FF88), hw.r32(0x027F_FF8C));
    println!("VRAMCNT A..I: {:02X?}  WRAMCNT {}", [0x240u32, 0x241, 0x242, 0x243, 0x244, 0x245, 0x246, 0x248, 0x249].map(|r| hw.r8(0x0400_0000 + r)), hw.wramcnt());
    println!("IPC: ARM9 sent {} words, ARM7 consumed {}, ARM7->ARM9 queued {}", hw.ipc_out.len(), machine.arm7.as_ref().map_or(0, |a| a.s.fifo_pos), hw.fifo_in.len());
    let mut top: Vec<_> = hot.into_iter().collect();
    top.sort_by(|a, b| b.1.cmp(&a.1));
    println!("hottest interpreted ARM9 pcs: {:?}", top.iter().take(std::env::var("MKDS_HOT").ok().and_then(|v| v.parse().ok()).unwrap_or(8)).map(|(a, n)| format!("{a:#X} x{n}")).collect::<Vec<_>>());
    let audio = machine.take_audio();
    if let Some(path) = std::env::var_os("MKDS_WAV") {
        // 16-bit stereo PCM WAV of everything the sound hardware produced
        let mut w = Vec::with_capacity(44 + audio.len() * 4);
        let data = (audio.len() * 4) as u32;
        w.extend_from_slice(b"RIFF");
        w.extend_from_slice(&(36 + data).to_le_bytes());
        w.extend_from_slice(b"WAVEfmt ");
        w.extend_from_slice(&16u32.to_le_bytes());
        w.extend_from_slice(&[1, 0, 2, 0]);
        w.extend_from_slice(&crate::spu::SAMPLE_RATE.to_le_bytes());
        w.extend_from_slice(&(crate::spu::SAMPLE_RATE * 4).to_le_bytes());
        w.extend_from_slice(&[4, 0, 16, 0]);
        w.extend_from_slice(b"data");
        w.extend_from_slice(&data.to_le_bytes());
        for f in &audio {
            w.extend_from_slice(&f[0].to_le_bytes());
            w.extend_from_slice(&f[1].to_le_bytes());
        }
        let _ = std::fs::write(&path, w);
        println!("wrote {}", std::path::Path::new(&path).display());
    }
    let peak = audio.iter().flat_map(|f| f.iter()).map(|v| (*v as i32).abs()).max().unwrap_or(0);
    let nonzero = audio.iter().filter(|f| **f != [0, 0]).count();
    println!("audio: {} frames rendered in total ({} non-silent, peak {}); last 2 s: {nonzero} non-silent, peak {peak}", machine.audio_stats.0, machine.audio_stats.1, machine.audio_stats.2);
    if let Some(mut a) = machine.arm7.take() {
        println!("sound: SOUNDCNT {:#06X}  channel starts {:?}  playing {}", a.s.spu.soundcnt, a.s.spu.starts, a.s.spu.ch.iter().filter(|c| c.playing()).count());
        for (n, c) in a.s.spu.ch.iter().enumerate().filter(|(n, _)| a.s.spu.starts[*n] > 0) {
            println!("  ch{n}: CNT {:#010X} SAD {:#010X} TMR {:#06X} PNT {} LEN {} (fmt {}, vol {}, pan {}, rep {})", c.cnt, c.sad, c.tmr, c.pnt, c.len, c.format(), c.volume(), c.pan(), c.repeat());
        }
        println!("\n== ARM7: pc {:#010X} lr {:#010X} sp {:#010X} cpsr {:#010X} halted {} IME {} IE {:#X} IF {:#X}", a.pc, a.cpu.r[14], a.cpu.r[13], a.cpu.cpsr(), a.s.halted, a.s.ime, a.s.ie, a.s.iflag);
        {
            // code around the ARM7's pc (for disassembly)
            let pc = a.pc;
            let mut v = crate::arm7::View { s: &mut a.s, hw: &mut machine.hw };
            let at = std::env::var("ARM7_DUMP").ok().and_then(|v| u32::from_str_radix(v.trim_start_matches("0x"), 16).ok()).unwrap_or(pc);
            let base = (at & !3).wrapping_sub(0x40);
            let words: Vec<String> = (0..48u32).map(|i| format!("{:08X}", v.r32(base + 4 * i))).collect();
            println!("ARM7 code at {base:#010X}: {}", words.join(" "));
            println!("ARM7 regs {:08X?}", a.cpu.r);
            if let Some(p) = &a.s.profile {
                let mut top: Vec<_> = p.iter().collect();
                top.sort_by(|x, y| y.1.cmp(x.1));
                println!("ARM7 hottest pcs: {:?}", top.iter().take(16).map(|(a, n)| format!("{a:#X} x{n}")).collect::<Vec<_>>());
                println!("ARM7 distinct pcs {}", p.len());
            }
        }
        if let Some(h) = &a.s.crash {
            println!("ARM7 left mapped memory; last pcs (pc, cpsr):");
            for (p, ps) in h {
                println!("    {p:#010X}  {ps:#010X}");
            }
        }
        let mut un: Vec<_> = a.s.unhandled.iter().collect();
        un.sort_by(|x, y| y.1.cmp(x.1));
        for (k, n) in un.iter().take(25) {
            println!("  ARM7 unhandled {n:>8}  {k}");
        }
    }
    let mut un: Vec<_> = machine.unhandled.iter().collect();
    un.sort_by(|x, y| y.1.cmp(x.1));
    for (k, n) in un.iter().take(25) {
        println!("  ARM9 unhandled {n:>8}  {k}");
    }
}
