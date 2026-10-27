//! The Nintendo DS ARM9 address space (T058, recomp lane L3): the memory a recompiled game sees.
//!
//! Regions (ARM9 view, GBATEK "DS Memory Map"):
//! - `0x01000000..` ITCM, 32 KiB, mirrored.
//! - `0x02000000..0x03000000` main RAM, 4 MiB, mirrored. DTCM (16 KiB, base from CP15) is checked first.
//! - `0x03000000..0x04000000` shared WRAM, 32 KiB as seen by the ARM9 (WRAMCNT = 0), mirrored.
//! - `0x04000000..0x04001000` I/O registers (see [`REGS`]; side effects in [`Bus::post_write`]).
//! - `0x05000000..` palette RAM, 2 KiB, mirrored (engine A first KiB, engine B second).
//! - `0x06000000..0x07000000` VRAM, banks A..I placed by VRAMCNT_A..I ([`vram_place`]).
//! - `0x07000000..` OAM, 2 KiB, mirrored.
//! - `0xFFFF0000..` BIOS (not provided: reads return 0; the SDK's BIOS calls are replaced, lane L5).
//!
//! Hardware modelled with real behaviour because the game depends on it every frame: the interrupt
//! registers (IME/IE/IF, IF is write-1-to-clear), the four timers, the four DMA channels (immediate,
//! V-blank, H-blank, geometry-FIFO triggers), the scanline counter / DISPSTAT / VCOUNT, KEYINPUT, the
//! hardware divider and square-root unit. The 3D command ports (`0x04000400..0x04000600`) are not
//! executed here: every write is appended to [`Bus::gx`] for the GX lane (L6) to interpret.
//!
//! Nothing here comes from the game; it is DS hardware behaviour.
use crate::input::{Core, HostInput, IrqEvents, Keypad};
use crate::rt::Mem;

pub const MAIN_RAM: u32 = 0x0200_0000;
pub const MAIN_RAM_SIZE: usize = 0x40_0000;
pub const ITCM_SIZE: usize = 0x8000;
pub const DTCM_SIZE: usize = 0x4000;
pub const SWRAM_SIZE: usize = 0x8000;
pub const IO_BASE: u32 = 0x0400_0000;
/// Engine A and shared registers at 0x04000000, engine B's at 0x04001000.
pub const IO_SIZE: usize = 0x2000;
/// Bus clock the timers and scanline counter run at (cycles per second).
pub const BUS_HZ: u64 = 33_513_982;
/// 355 dots x 6 cycles per scanline, 263 scanlines (192 visible) per frame.
pub const CYCLES_PER_LINE: u64 = 355 * 6;
pub const LINES: u64 = 263;
pub const VISIBLE_LINES: u64 = 192;
/// The H-blank flag is set from dot 256.
const HBLANK_START: u64 = 256 * 6;

/// Interrupt bits in IE / IF.
pub mod irq {
    pub const VBLANK: u32 = 1 << 0;
    pub const HBLANK: u32 = 1 << 1;
    pub const VCOUNT: u32 = 1 << 2;
    pub const TIMER0: u32 = 1 << 3;
    pub const DMA0: u32 = 1 << 8;
    pub const KEYPAD: u32 = 1 << 12;
    pub const IPC_SYNC: u32 = 1 << 16;
    pub const IPC_SEND_EMPTY: u32 = 1 << 17;
    pub const IPC_RECV_NOT_EMPTY: u32 = 1 << 18;
    pub const GXFIFO: u32 = 1 << 21;
}

/// DMA start timing field (CNT bits 11..13, ARM9).
pub mod dma_timing {
    pub const IMMEDIATE: u32 = 0;
    pub const VBLANK: u32 = 1;
    pub const HBLANK: u32 = 2;
    pub const DISPLAY_START: u32 = 3;
    pub const MAIN_MEM_DISPLAY: u32 = 4;
    pub const GEOMETRY_FIFO: u32 = 7;
}

/// Named I/O registers (offset from `0x04000000`, width in bytes, name, meaning). Offsets not listed
/// are plain storage. Source: GBATEK; the names are the hardware's, not the game's.
pub const REGS: &[(u32, u8, &str, &str)] = &[
    (
        0x000,
        4,
        "DISPCNT_A",
        "engine A display control (BG mode, layer enables, display mode)",
    ),
    (
        0x004,
        2,
        "DISPSTAT",
        "V-blank/H-blank/VCOUNT-match flags (bits 0-2, read-only) and their IRQ enables (bits 3-5), VCOUNT compare",
    ),
    (0x006, 2, "VCOUNT", "current scanline 0..262"),
    (0x008, 2, "BG0CNT_A", "engine A BG0 control"),
    (0x00A, 2, "BG1CNT_A", "engine A BG1 control"),
    (0x00C, 2, "BG2CNT_A", "engine A BG2 control"),
    (0x00E, 2, "BG3CNT_A", "engine A BG3 control"),
    (0x010, 2, "BG0HOFS_A", "engine A BG0 horizontal scroll"),
    (0x012, 2, "BG0VOFS_A", "engine A BG0 vertical scroll"),
    (
        0x060,
        2,
        "DISP3DCNT",
        "3D engine control (texture, shading, alpha test/blend, fog, edge marking)",
    ),
    (
        0x0B0,
        4,
        "DMA0SAD",
        "DMA channel 0 source address (channels 1-3 follow every 12 bytes)",
    ),
    (0x0B4, 4, "DMA0DAD", "DMA channel 0 destination address"),
    (
        0x0B8,
        4,
        "DMA0CNT",
        "DMA channel 0 count (low 21 bits) and control (bits 21-31); bit 31 enables",
    ),
    (
        0x100,
        2,
        "TM0CNT_L",
        "timer 0 counter (read) / reload (write); timers 1-3 follow every 4 bytes",
    ),
    (
        0x102,
        2,
        "TM0CNT_H",
        "timer 0 control: prescaler 0-1, count-up bit 2, IRQ bit 6, enable bit 7",
    ),
    (
        0x130,
        2,
        "KEYINPUT",
        "buttons, active low: A B Select Start Right Left Up Down R L",
    ),
    (0x132, 2, "KEYCNT", "key interrupt control"),
    (
        0x180,
        4,
        "IPCSYNC",
        "ARM9/ARM7 sync nibbles and IRQ trigger",
    ),
    (0x184, 2, "IPCFIFOCNT", "IPC FIFO control/status"),
    (
        0x188,
        4,
        "IPCFIFOSEND",
        "IPC FIFO send (push a word to the ARM7)",
    ),
    (0x1A0, 2, "AUXSPICNT", "cartridge SPI control"),
    (
        0x204,
        2,
        "EXMEMCNT",
        "external memory control (GBA slot / main RAM priority)",
    ),
    (0x208, 4, "IME", "interrupt master enable (bit 0)"),
    (0x210, 4, "IE", "interrupt enable mask"),
    (0x214, 4, "IF", "interrupt flags; writing 1 clears"),
    (
        0x240,
        1,
        "VRAMCNT_A",
        "VRAM bank A: MST bits 0-2, offset bits 3-4, enable bit 7 (B..G follow, 0x244 E, 0x245 F, 0x246 G)",
    ),
    (
        0x247,
        1,
        "WRAMCNT",
        "shared WRAM split between ARM9 and ARM7",
    ),
    (0x248, 1, "VRAMCNT_H", "VRAM bank H"),
    (0x249, 1, "VRAMCNT_I", "VRAM bank I"),
    (
        0x280,
        2,
        "DIVCNT",
        "hardware divider mode (bits 0-1), div-by-zero (bit 14), busy (bit 15)",
    ),
    (0x290, 8, "DIV_NUMER", "dividend (64-bit)"),
    (0x298, 8, "DIV_DENOM", "divisor (64-bit)"),
    (0x2A0, 8, "DIV_RESULT", "quotient (64-bit)"),
    (0x2A8, 8, "DIVREM_RESULT", "remainder (64-bit)"),
    (
        0x2B0,
        2,
        "SQRTCNT",
        "square-root mode (bit 0: 64-bit input), busy (bit 15)",
    ),
    (0x2B4, 4, "SQRT_RESULT", "square-root result (32-bit)"),
    (0x2B8, 8, "SQRT_PARAM", "square-root input (64-bit)"),
    (0x300, 1, "POSTFLG", "post-boot flag"),
    (
        0x304,
        2,
        "POWCNT1",
        "power control: LCDs, 2D engines, 3D rendering and geometry engines, display swap",
    ),
    (
        0x400,
        4,
        "GXFIFO",
        "geometry command FIFO (0x400..0x43F packed commands)",
    ),
    (
        0x440,
        4,
        "GXCMD_PORTS",
        "geometry command ports 0x440..0x5FF, one per command (address = 0x400 + 4*cmd)",
    ),
    (
        0x600,
        4,
        "GXSTAT",
        "geometry engine status: busy, FIFO level, matrix stack, IRQ mode",
    ),
    (0x604, 2, "RAM_COUNT", "polygon and vertex RAM counts"),
    (
        0x640,
        4,
        "CLIPMTX_RESULT",
        "clip matrix read-back (0x640..0x67F)",
    ),
    (
        0x680,
        4,
        "VECMTX_RESULT",
        "vector matrix read-back (0x680..0x6A3)",
    ),
    (
        0x1000,
        4,
        "DISPCNT_B",
        "engine B display control (I/O offset 0x1000, outside the 4 KiB block modelled by the bus)",
    ),
];

/// A DMA channel's latched state.
#[derive(Clone, Copy, Default, Debug)]
pub struct Dma {
    pub src: u32,
    pub dst: u32,
    pub count: u32,
    pub ctrl: u32,
    /// Internal addresses/counts while a repeating transfer is armed.
    pub cur_src: u32,
    pub cur_dst: u32,
    pub cur_len: u32,
}

#[derive(Clone, Copy, Default, Debug)]
pub struct Timer {
    pub counter: u16,
    pub reload: u16,
    pub ctrl: u16,
    /// Cycles accumulated toward the next prescaled tick.
    pub acc: u64,
}

/// One VRAM bank's size and where the CPU sees it for a given MST/offset, or `None` (not CPU-addressable,
/// e.g. texture / extended palette / ARM7 mappings: the GPU reads those straight from [`Bus::vram_bank`]).
pub fn vram_place(bank: usize, mst: u8, off: u8) -> Option<u32> {
    let o = off as u32;
    Some(match (bank, mst) {
        (0, 0) => 0x0680_0000,
        (1, 0) => 0x0682_0000,
        (2, 0) => 0x0684_0000,
        (3, 0) => 0x0686_0000,
        (4, 0) => 0x0688_0000,
        (5, 0) => 0x0689_0000,
        (6, 0) => 0x0689_4000,
        (7, 0) => 0x0689_8000,
        (8, 0) => 0x068A_0000,
        (0..=3, 1) => 0x0600_0000 + 0x2_0000 * o,
        (0 | 1, 2) => 0x0640_0000 + 0x2_0000 * (o & 1),
        (2, 4) => 0x0620_0000,
        (3, 4) => 0x0660_0000,
        (4, 1) => 0x0600_0000,
        (4, 2) => 0x0640_0000,
        (5 | 6, 1) => 0x0600_0000 + 0x4000 * (o & 1) + 0x1_0000 * (o >> 1),
        (5 | 6, 2) => 0x0640_0000 + 0x4000 * (o & 1) + 0x1_0000 * (o >> 1),
        (7, 1) => 0x0620_0000,
        (8, 1) => 0x0620_8000,
        (8, 2) => 0x0660_0000,
        _ => return None,
    })
}

pub const VRAM_BANK_SIZE: [usize; 9] = [
    0x20000, 0x20000, 0x20000, 0x20000, 0x10000, 0x4000, 0x4000, 0x8000, 0x4000,
];

pub struct Bus {
    /// Shared host pins and independent keypad controls; this bus is ARM9-only.
    keypad: Keypad,
    pub main: Vec<u8>,
    pub itcm: Vec<u8>,
    pub dtcm: Vec<u8>,
    /// DTCM base address (CP15 c9,c1 region register); `None` until the game maps it.
    pub dtcm_base: Option<u32>,
    pub swram: Vec<u8>,
    pub io: Vec<u8>,
    pub palette: Vec<u8>,
    pub oam: Vec<u8>,
    pub vram: [Vec<u8>; 9],
    pub dma: [Dma; 4],
    pub timers: [Timer; 4],
    /// Every write to the geometry command ports / FIFO, in order: (I/O offset, value, bytes).
    pub gx: Vec<(u32, u32, u8)>,
    /// Cycles into the current scanline and the scanline number.
    pub line_cycles: u64,
    pub line: u64,
    /// DISPCAPCNT as latched when this frame's display started (0 = no capture this frame). The hardware clears
    /// the enable bit when the capture ends, so one enable captures one frame.
    pub capture_latch: u32,
    /// Total cycles advanced.
    pub cycles: u64,
    /// Words written to IPCFIFOSEND (for the ARM7/sound HLE, lane L7).
    pub ipc_out: Vec<u32>,
    /// The ARM7's side of the IPCSYNC nibble handshake (ARM9 reads it in bits 0-3).
    pub arm7_sync: u8,
    /// Words the (stand-in) ARM7 has sent and the ARM9 has not read yet (IPCFIFORECV, `0x04100000`).
    pub fifo_in: std::collections::VecDeque<u32>,
    /// The game's own ARM7 code runs (`crate::arm7`): the stand-in answers above are off.
    pub arm7_lle: bool,
    /// 32 KB at 0xFFFF0000: no console BIOS, only our IRQ vector code (`rt::ARM9_IRQ_STUB` at +0x18).
    pub bios9: Vec<u8>,
    /// The game card in the slot (the user's ROM), when inserted.
    pub card: Option<crate::card::Card>,
    /// The ARM9 wrote IPCSYNC with bit 13 (IRQ request to the ARM7) since the ARM7 last looked.
    pub sync_irq_to_arm7: bool,
    watched_main_code: Option<(usize, usize)>,
    /// Changes whenever either CPU or DMA writes a watched physical main-RAM byte.
    pub code_epoch: u64,
    /// Block 5.0 oracle (`acceptance --record`): read-only observation of PXI, SPU writes, polled I/O and
    /// IRQs. `None` (the default) costs one branch on those paths and changes nothing.
    pub probe: Option<Box<Probe>>,
    /// Block 5.3 (`MKDS_HW_TIME=native`): timers 0 and 1 run on virtual OS time ([`crate::os_hle`]).
    /// `None` (the default): every timer follows the bus cycles.
    pub vtime: Option<Box<crate::os_hle::VirtualTime>>,
}

/// SDK V-blank counter (`OS_GetVBlankCount`, HW_VBLANK_COUNT_BUF): the ARM7's V-blank handler increments it.
pub const VBLANK_COUNT: u32 = 0x027F_FC3C;

/// PXI event kinds in [`Probe::events`].
pub const EV_A9_WORD: u8 = 0; // ARM9 -> ARM7 FIFO word
pub const EV_A7_WORD: u8 = 1; // ARM7 -> ARM9 FIFO word
pub const EV_A9_SYNC: u8 = 2; // ARM9 IPCSYNC output nibble (value | 0x100 when it asks for an IRQ)
pub const EV_A7_SYNC: u8 = 3; // ARM7 IPCSYNC output nibble (value | 0x100 when it asks for an IRQ)
pub const EV_SPU: u8 = 4; // ARM7 SPU register write: value, with offset and width in `Event::off` / `bytes`

/// One observed event, tagged with the SDK V-blank count at the time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Event {
    pub frame: u32,
    pub kind: u8,
    pub bytes: u8,
    pub off: u16,
    pub value: u32,
}

/// Polled ARM9 I/O registers counted by [`Probe::io_reads`] (census): VCOUNT, DISPSTAT, TM0-3CNT_L,
/// IPCSYNC, IPCFIFOCNT, GXSTAT, KEYINPUT.
pub const POLLED_IO: [(usize, &str); 10] = [
    (0x006, "VCOUNT"), (0x004, "DISPSTAT"), (0x100, "TM0CNT_L"), (0x104, "TM1CNT_L"), (0x108, "TM2CNT_L"),
    (0x10C, "TM3CNT_L"), (0x180, "IPCSYNC"), (0x184, "IPCFIFOCNT"), (0x600, "GXSTAT"), (0x130, "KEYINPUT"),
];

/// Block 5.0 oracle state on the bus (see [`Bus::probe`]). The recorder drains it every logical frame.
#[derive(Default, Clone, Debug)]
pub struct Probe {
    /// PXI traffic (and SPU writes when `log_spu`) since the last drain.
    pub events: Vec<Event>,
    pub log_spu: bool,
    /// SPU register writes since the last drain, and a running hash of them (offset, value, width).
    pub spu_writes: u64,
    pub spu_hash: u64,
    /// Reads of [`POLLED_IO`] registers since the last drain (same order).
    pub io_reads: [u64; 10],
    /// IRQs taken since the last drain, by IE & IF bit at the time.
    pub irq_bits: [u64; 32],
    /// Census of tight loops: slice-end pc of the current run, its instructions, and finished runs of more
    /// than 1,000 instructions since the last drain as (first pc, instructions).
    pub run_pc: u32,
    pub run_len: u64,
    pub spins: Vec<(u32, u64)>,
    /// KEYINPUT as the ARM9 first read it since the last drain (the input the game saw).
    pub first_key: Option<u16>,
    /// Test perturbation (off = 0): added to TM0CNT_L reads, the counter `OS_GetTick` reads.
    pub tm0_bias: u16,
}

impl Probe {
    fn frame(main: &[u8]) -> u32 {
        let o = (VBLANK_COUNT as usize) & (MAIN_RAM_SIZE - 1);
        u32::from_le_bytes(main[o..o + 4].try_into().unwrap())
    }

    fn io_read(&mut self, off: usize) {
        if let Some(i) = POLLED_IO.iter().position(|&(o, _)| off & !1 == o) {
            self.io_reads[i] += 1;
        }
    }

    /// An ARM9 slice ended at `pc` after `used` instructions: extends or closes the tight-loop run.
    pub fn slice_end(&mut self, pc: u32, used: u32) {
        if pc.wrapping_sub(self.run_pc) < 0x40 || self.run_pc.wrapping_sub(pc) < 0x40 {
            self.run_len += used as u64;
        } else {
            if self.run_len > 1000 {
                self.spins.push((self.run_pc, self.run_len));
            }
            self.run_pc = pc;
            self.run_len = used as u64;
        }
    }

    pub fn irq(&mut self, pending: u32) {
        for b in 0..32 {
            if pending >> b & 1 != 0 {
                self.irq_bits[b] += 1;
            }
        }
    }

    /// The ARM7 wrote an SPU register (`crate::arm7`).
    pub fn spu_write(&mut self, main: &[u8], off: u32, v: u32, n: usize) {
        self.spu_writes += 1;
        for x in [off, v, n as u32] {
            self.spu_hash = (self.spu_hash ^ x as u64).wrapping_mul(0x100_0000_01b3).rotate_left(5);
        }
        if self.log_spu {
            self.events.push(Event { frame: Self::frame(main), kind: EV_SPU, bytes: n as u8, off: off as u16, value: v });
        }
    }

    fn pxi(&mut self, main: &[u8], kind: u8, value: u32) {
        self.events.push(Event { frame: Self::frame(main), kind, bytes: 4, off: 0, value });
    }
}

fn rd(buf: &[u8], off: usize, n: usize) -> u32 {
    let mut v = 0u32;
    for i in 0..n {
        v |= (buf[(off + i) % buf.len()] as u32) << (8 * i);
    }
    v
}

fn wr(buf: &mut [u8], off: usize, n: usize, v: u32) {
    let len = buf.len();
    for i in 0..n {
        buf[(off + i) % len] = (v >> (8 * i)) as u8;
    }
}

impl Default for Bus {
    fn default() -> Self {
        Self::new()
    }
}

impl Bus {
    pub fn new() -> Bus {
        let mut b = Bus {
            keypad: Keypad::new(),
            main: vec![0; MAIN_RAM_SIZE],
            itcm: vec![0; ITCM_SIZE],
            dtcm: vec![0; DTCM_SIZE],
            dtcm_base: None,
            swram: vec![0; SWRAM_SIZE],
            io: vec![0; IO_SIZE],
            palette: vec![0; 0x800],
            oam: vec![0; 0x800],
            vram: VRAM_BANK_SIZE.map(|n| vec![0; n]),
            dma: [Dma::default(); 4],
            timers: [Timer::default(); 4],
            gx: Vec::new(),
            line_cycles: 0,
            line: 0,
            capture_latch: 0,
            cycles: 0,
            ipc_out: Vec::new(),
            arm7_sync: 0,
            fifo_in: std::collections::VecDeque::new(),
            arm7_lle: false,
            sync_irq_to_arm7: false,
            watched_main_code: None,
            code_epoch: 0,
            probe: None,
            vtime: None,
            card: None,
            bios9: {
                let mut b = vec![0u8; 0x8000];
                for (i, w) in crate::rt::ARM9_IRQ_STUB.iter().enumerate() {
                    b[0x18 + 4 * i..0x1C + 4 * i].copy_from_slice(&w.to_le_bytes());
                }
                b
            },
        };
        b.io_set(0x130, 2, 0x03FF); // no buttons held
        b.io_set(0x184, 2, 0x0101); // IPC FIFO: send empty, receive empty
        b
    }

    /// ARM7 stand-in, part 2 (T069): the SDK's PXI layer keeps, per processor, a bitmask of FIFO tags
    /// whose receive callback is registered, in memory both CPUs see (`0x027FFC00 + 904 + 4 * cpu`;
    /// `sub_214D7C4` sets the ARM9's own bit, `sub_214D79C` reads the ARM7's). The ARM7 does this during
    /// its own start-up before the ARM9 proceeds, so announce every tag as ready.
    pub fn arm7_announce_ready(&mut self) {
        self.w32(0x027F_FC00 + 904 + 4, u32::MAX);
    }

    /// The stand-in ARM7 sends `word` to the ARM9: queued for IPCFIFORECV, and the "receive FIFO not empty"
    /// interrupt is raised when it was empty and the ARM9 enabled it (IPCFIFOCNT bit 10, FIFO enabled bit 15).
    pub fn arm7_send(&mut self, word: u32) {
        if let Some(p) = self.probe.as_mut() {
            p.pxi(&self.main, EV_A7_WORD, word);
        }
        let was_empty = self.fifo_in.is_empty();
        if self.fifo_in.len() < 16 {
            self.fifo_in.push_back(word);
        }
        let cnt = self.io_get(0x184, 2);
        if was_empty && cnt & 0x8400 == 0x8400 {
            self.raise(irq::IPC_RECV_NOT_EMPTY);
        }
    }

    /// The ARM7 writes its IPCSYNC output nibble (the ARM9 reads it in bits 0-3); `irq` (its bit 13)
    /// raises the ARM9's IPC sync interrupt when the ARM9 enabled it (its IPCSYNC bit 14).
    pub fn arm7_set_sync(&mut self, nibble: u8, irq: bool) {
        if let Some(p) = self.probe.as_mut() {
            p.pxi(&self.main, EV_A7_SYNC, (nibble & 0xF) as u32 | (irq as u32) << 8);
        }
        self.arm7_sync = nibble & 0xF;
        let cur = self.io_get(0x180, 2);
        self.io_set(0x180, 2, (cur & !0xF) | self.arm7_sync as u32);
        if irq && cur & 0x4000 != 0 {
            self.raise(irq::IPC_SYNC);
        }
    }

    /// The ARM9's IPCSYNC output nibble (the ARM7 reads it in its bits 0-3).
    pub fn arm9_sync_out(&self) -> u8 {
        ((self.io_get(0x180, 2) >> 8) & 0xF) as u8
    }

    /// WRAMCNT: how the 32 KB shared WRAM is split (0: all ARM9, 1/2: halves, 3: all ARM7).
    pub fn wramcnt(&self) -> u8 {
        self.io_get(0x247, 1) as u8 & 3
    }

    /// IPCFIFOCNT as the ARM9 reads it: live empty/full flags over the stored enables and error bit.
    fn fifo_status(&self) -> u32 {
        let stored = self.io_get(0x184, 2) & 0xC40C; // enable, error, recv-IRQ, send-IRQ
        let recv = if self.fifo_in.is_empty() {
            0x100
        } else if self.fifo_in.len() >= 16 {
            0x200
        } else {
            0
        };
        stored | 0x1 | recv // our sends complete instantly: send FIFO always empty
    }

    pub fn vram_bank(&self, i: usize) -> &[u8] {
        &self.vram[i]
    }

    /// Watch the main-RAM window containing generated overlay code. Ranges are
    /// source metadata; no game-specific addresses are embedded in the runtime.
    pub fn watch_overlay_code(
        &mut self,
        sigs: &[crate::residency::SegmentSig],
    ) -> Result<(), String> {
        let mut window: Option<(usize, usize)> = None;
        for range in sigs
            .iter()
            .filter(|s| crate::residency::is_overlay(s.segment))
            .flat_map(|s| s.ranges)
        {
            let start = range
                .start
                .checked_sub(MAIN_RAM)
                .ok_or("Overlay code is outside main RAM")? as usize;
            let end = start
                .checked_add(range.len as usize)
                .filter(|e| *e <= MAIN_RAM_SIZE)
                .ok_or("Overlay code is outside physical main RAM")?;
            if range.len == 0 {
                return Err("Overlay code range is empty".into());
            }
            window = Some(window.map_or((start, end), |(lo, hi)| (lo.min(start), hi.max(end))));
        }
        self.watched_main_code = window;
        self.code_epoch = self.code_epoch.wrapping_add(1);
        Ok(())
    }

    /// Side-effect-free code verification; never reads MMIO or consumes FIFOs.
    pub fn peek_into(&self, address: u32, output: &mut [u8]) -> bool {
        if address >> 24 != 2 {
            return false;
        }
        if self
            .dtcm_base
            .is_some_and(|base| address >= base && address < base + DTCM_SIZE as u32)
        {
            return false;
        }
        let start = address as usize & (MAIN_RAM_SIZE - 1);
        let Some(end) = start
            .checked_add(output.len())
            .filter(|end| *end <= MAIN_RAM_SIZE)
        else {
            return false;
        };
        output.copy_from_slice(&self.main[start..end]);
        true
    }

    /// ARM7 main RAM bypasses ARM9's DTCM view, so it notifies using physical offsets.
    pub(crate) fn note_main_write(&mut self, offset: usize, bytes: usize) {
        if let Some((lo, hi)) = self.watched_main_code {
            if (offset < hi && offset.saturating_add(bytes) > lo)
                || (offset.saturating_add(bytes) > MAIN_RAM_SIZE
                    && offset + bytes - MAIN_RAM_SIZE > lo)
            {
                self.code_epoch = self.code_epoch.wrapping_add(1);
            }
        }
    }

    fn io_get(&self, off: usize, n: usize) -> u32 {
        rd(&self.io, off, n)
    }
    fn io_set(&mut self, off: usize, n: usize, v: u32) {
        wr(&mut self.io, off, n, v)
    }

    /// Set the buttons held (bit set = pressed): A B Select Start Right Left Up Down R L = bits 0..9.
    pub fn set_keys(&mut self, pressed: u16) {
        let mut input = self.keypad.snapshot();
        input.buttons = pressed;
        self.set_input(input);
    }

    /// Snapshot host pins; ARM9 IRQ is raised here. ARM7 requests are returned
    /// for the ARM7/HLE owner, not injected into this ARM9 IF register.
    pub fn set_input(&mut self, input: HostInput) -> IrqEvents {
        let events = self.keypad.update(input);
        self.io_set(0x130, 2, self.keypad.keyinput() as u32);
        self.raise(events.arm9);
        events
    }

    /// Physical extended pins for the ARM7/HLE service, not ARM9 MMIO.
    pub fn extended_keys(&self) -> u16 {
        self.keypad.extkeyin()
    }

    /// True when an enabled, pending interrupt should be taken (IME bit 0 and IE & IF).
    pub fn irq_pending(&self) -> bool {
        self.io_get(0x208, 4) & 1 != 0 && self.io_get(0x210, 4) & self.io_get(0x214, 4) != 0
    }

    pub fn raise(&mut self, bits: u32) {
        let f = self.io_get(0x214, 4) | bits;
        self.io_set(0x214, 4, f);
    }

    fn vram_find(&self, a: u32) -> Option<(usize, usize)> {
        for bank in 0..9 {
            let cnt = self.io[if bank < 7 {
                0x240 + bank
            } else {
                0x248 + bank - 7
            }];
            if cnt & 0x80 == 0 {
                continue;
            }
            let (mst, off) = (cnt & 7, (cnt >> 3) & 3);
            let Some(base) = vram_place(bank, mst, off) else {
                continue;
            };
            let size = VRAM_BANK_SIZE[bank] as u32;
            if a >= base && a < base + size {
                return Some((bank, (a - base) as usize));
            }
        }
        None
    }

    /// Resolve `a` to a backing buffer and offset for RAM-like regions.
    fn locate(&mut self, a: u32) -> Option<(&mut Vec<u8>, usize)> {
        match a >> 24 {
            // ITCM is mirrored through 0x00000000..0x01FFFFFF
            0x00 | 0x01 => Some((&mut self.itcm, (a & 0x7FFF) as usize)),
            0x02 => {
                if let Some(b) = self.dtcm_base {
                    if a >= b && a < b + DTCM_SIZE as u32 {
                        return Some((&mut self.dtcm, (a - b) as usize));
                    }
                }
                Some((&mut self.main, (a & 0x3F_FFFF) as usize))
            }
            0x03 => Some((&mut self.swram, (a & 0x7FFF) as usize)),
            0x05 => Some((&mut self.palette, (a & 0x7FF) as usize)),
            0x06 => {
                let (bank, off) = self.vram_find(a & 0xFF_FFFF | 0x0600_0000)?;
                Some((&mut self.vram[bank], off))
            }
            0x07 => Some((&mut self.oam, (a & 0x7FF) as usize)),
            0xFF => Some((&mut self.bios9, (a & 0x7FFF) as usize)),
            _ => None,
        }
    }

    fn read(&mut self, a: u32, n: usize) -> u32 {
        if a & !3 == 0x0410_0000 {
            // IPCFIFORECV: pop the oldest word; reading an empty FIFO sets the error flag and repeats the last word
            return match self.fifo_in.pop_front() {
                Some(w) => w,
                None => {
                    let c = self.io_get(0x184, 2) | 0x4000;
                    self.io_set(0x184, 2, c);
                    0
                }
            };
        }
        if a & !3 == 0x0410_0010 {
            // game card data port
            let Some(card) = self.card.as_mut() else {
                return u32::MAX;
            };
            let (w, done) = card.read_data();
            if done && self.io_get(0x1A0, 2) & 0x4000 != 0 {
                self.raise(crate::card::IRQ_CARD_DONE);
            }
            return w;
        }
        if a >> 13 == IO_BASE >> 13 {
            return self.read_io((a - IO_BASE) as usize, n);
        }
        match self.locate(a) {
            Some((buf, off)) => rd(buf, off, n),
            None => 0,
        }
    }

    /// Card DMA (start timing 5): a waiting channel moves the card's words as they become ready.
    fn card_pump(&mut self) {
        let mut guard = 0;
        while self.card.as_ref().is_some_and(|c| c.has_data()) && guard < 0x10000 {
            let waiting = self
                .dma
                .iter()
                .any(|d| d.ctrl & 0x8000_0000 != 0 && (d.ctrl >> 27) & 7 == 5);
            if !waiting {
                break;
            }
            self.trigger_dma(5);
            guard += 1;
        }
    }

    fn write(&mut self, a: u32, n: usize, v: u32) {
        if a >> 13 == IO_BASE >> 13 {
            return self.write_io((a - IO_BASE) as usize, n, v);
        }
        // 8-bit writes to palette / VRAM-BG / OAM are ignored by the hardware (they would corrupt both bytes).
        if n == 1 && matches!(a >> 24, 0x05 | 0x07) {
            return;
        }
        if a >> 24 == 2
            && !self
                .dtcm_base
                .is_some_and(|base| a >= base && a < base + DTCM_SIZE as u32)
        {
            self.note_main_write(a as usize & (MAIN_RAM_SIZE - 1), n);
        }
        if let Some((buf, off)) = self.locate(a) {
            wr(buf, off, n, v);
        }
    }

    fn read_io(&mut self, off: usize, n: usize) -> u32 {
        if self.probe.is_some() {
            let key = self.io_get(0x130, 2) as u16;
            let p = self.probe.as_mut().unwrap();
            p.io_read(off);
            if off & !1 == 0x130 && p.first_key.is_none() {
                p.first_key = Some(key);
            }
        }
        if (0x600..0x604).contains(&off) {
            // Commands are consumed synchronously by our GX backend: zero FIFO
            // entries, less than half full (25), empty (26), geometry not busy.
            // The diagnostic command log is not the hardware FIFO occupancy.
            let status = (self.io_get(0x600, 4) & 0xc000_0000) | 0x0600_0000;
            let value = status >> (8 * (off - 0x600));
            return if n == 4 {
                value
            } else {
                value & ((1 << (8 * n)) - 1)
            };
        }
        // timer counters are live
        if (0x100..0x110).contains(&off) && off % 4 < 2 {
            let t = self.timers[(off - 0x100) / 4];
            let mut v = t.counter as u32;
            if off < 0x108 {
                if let Some(vt) = self.vtime.as_mut() {
                    let (c, bits) = vt.read_counter((off - 0x100) / 4);
                    self.raise(bits);
                    v = c as u32;
                }
            }
            if off & !3 == 0x100 {
                if let Some(p) = &self.probe {
                    v = (v as u16).wrapping_add(p.tm0_bias) as u32;
                }
            }
            return if n >= 2 || off % 4 == 0 {
                v >> (8 * (off % 2)) & if n == 1 { 0xFF } else { 0xFFFF }
            } else {
                v
            };
        }
        if (0x184..0x186).contains(&off) {
            let v = self.fifo_status() >> (8 * (off - 0x184));
            return if n == 1 { v & 0xFF } else { v & 0xFFFF };
        }
        if (0x1A4..0x1A8).contains(&off) {
            if let Some(card) = self.card.as_ref() {
                let v = card.read_ctrl() >> (8 * (off - 0x1A4));
                return if n == 4 {
                    v
                } else {
                    v & ((1u32 << (8 * n)) - 1)
                };
            }
        }
        self.io_get(off, n)
    }

    fn write_io(&mut self, off: usize, n: usize, v: u32) {
        if (0x600..0x604).contains(&off) {
            // Only IRQ mode is retained. Stack-error acknowledgement is W1C;
            // an immediate backend has no pending stack/FIFO busy condition.
            let shift = 8 * (off - 0x600);
            let mask = if n == 4 { u32::MAX } else { (1 << (8 * n)) - 1 };
            let writable = (mask << shift) & 0xc000_0000;
            let control = (self.io_get(0x600, 4) & !writable) | ((v << shift) & writable);
            self.io_set(0x600, 4, control & 0xc000_0000);
            self.gx_fifo_irq();
            return;
        }
        if self.card.is_some() && (0x1A4..0x1B0).contains(&off) {
            self.io_set(off, n, v);
            let card = self.card.as_mut().unwrap();
            for i in 0..n {
                if (0x1A8..0x1B0).contains(&(off + i)) {
                    card.cmd[off + i - 0x1A8] = (v >> (8 * i)) as u8;
                }
            }
            // a write reaching ROMCTRL's top byte starts the command when bit 31 is set
            if off < 0x1A8 && off + n > 0x1A7 {
                let ctrl = self.io_get(0x1A4, 4);
                self.card.as_mut().unwrap().write_ctrl(ctrl);
                self.card_pump();
            }
            return;
        }
        if (0x130..0x134).contains(&off) {
            let handled = match n {
                1 => self
                    .keypad
                    .write8(Core::Arm9, IO_BASE + off as u32, v as u8),
                2 => self
                    .keypad
                    .write16(Core::Arm9, IO_BASE + off as u32, v as u16),
                4 => self.keypad.write32(Core::Arm9, IO_BASE + off as u32, v),
                _ => false,
            };
            if handled {
                self.io_set(0x130, 2, self.keypad.keyinput() as u32);
                self.io_set(0x132, 2, self.keypad.control(Core::Arm9) as u32);
                return;
            }
        }
        match off {
            // IF: write 1 to clear (any width covering the register)
            0x214..=0x217 => {
                let width = if n >= 4 {
                    u32::MAX
                } else {
                    (1u32 << (8 * n)) - 1
                };
                let cleared = (v & width) << (8 * (off - 0x214));
                let f = self.io_get(0x214, 4) & !cleared;
                self.io_set(0x214, 4, f);
                return;
            }
            // DISPSTAT low three bits and VCOUNT are read-only status
            0x004 if n == 1 => {
                let keep = self.io[0x004] & 7;
                self.io[0x004] = keep | (v as u8 & !7);
                return;
            }
            0x004 if n >= 2 => {
                let keep = self.io_get(0x004, 2) & 7;
                self.io_set(0x004, 2, keep | (v & 0xFFF8));
                return;
            }
            0x006 => return,
            0x400..=0x5FF => {
                self.gx.push((off as u32, v, n as u8));
                return;
            }
            // IPCSYNC: bits 0-3 are the ARM7's output (read-only here), bits 8-11 ours, 14 enables the recv IRQ.
            // ARM7 stand-in for the SDK's PXI_Init handshake (sub_214D830): the ARM9 echoes the nibble it
            // reads; the ARM7 side answers an echo of its current value by counting up (mod 16), and the ARM9
            // finishes once it has seen >4 changes and the value is back at 0. Documented in 4_docs/RECOMP_PLAN.md.
            0x180 | 0x181 => {
                let (mine, ctrl) = match (off, n) {
                    (0x180, 2) | (0x180, 4) => ((v >> 8) & 0xF, v & 0x4F00),
                    (0x181, 1) => (v & 0xF, (v & 0x4F) << 8),
                    _ => return, // byte write to the read-only low half
                };
                if let Some(p) = self.probe.as_mut() {
                    let irq = if (off, n) == (0x181, 1) { v & 0x20 } else { v & 0x2000 } != 0;
                    p.pxi(&self.main, EV_A9_SYNC, mine | (irq as u32) << 8);
                }
                if self.arm7_lle {
                    // the real ARM7 (crate::arm7) answers; bit 13 asks for an IPC sync IRQ on its side
                    if (off, n) != (0x181, 1) && v & 0x2000 != 0
                        || (off, n) == (0x181, 1) && v & 0x20 != 0
                    {
                        self.sync_irq_to_arm7 = true;
                    }
                } else if mine as u8 == self.arm7_sync {
                    self.arm7_sync = (self.arm7_sync + 1) & 0xF;
                }
                self.io_set(0x180, 2, ctrl | mine << 8 | self.arm7_sync as u32);
                return;
            }
            0x184 | 0x185 if n <= 2 => {
                let v16 = if off == 0x185 { v << 8 } else { v } & 0xFFFF;
                let mut cur = self.io_get(0x184, 2);
                if v16 & 0x4000 != 0 {
                    cur &= !0x4000; // error flag: write 1 to clear
                }
                cur = cur & !0x840C | v16 & 0x8404;
                self.io_set(0x184, 2, cur);
                // enabling "recv not empty" IRQ while data is waiting fires it
                if cur & 0x8400 == 0x8400 && !self.fifo_in.is_empty() {
                    self.raise(irq::IPC_RECV_NOT_EMPTY);
                }
                return;
            }
            0x188 => {
                if let Some(p) = self.probe.as_mut() {
                    p.pxi(&self.main, EV_A9_WORD, v);
                }
                self.ipc_out.push(v);
                return;
            }
            _ => {}
        }
        // arithmetic result registers are read-only
        if (0x2A0..0x2B0).contains(&off) || (0x2B4..0x2B8).contains(&off) {
            return;
        }
        self.io_set(off, n, v);
        self.post_write(off, n);
    }

    /// Side effects of a write to `[off, off + n)` after it has been stored.
    fn post_write(&mut self, off: usize, n: usize) {
        if (0x240..0x24A).contains(&off) && std::env::var_os("MKDS_IO_TRACE").is_some() {
            eprintln!(
                "[io] VRAMCNT/WRAMCNT write off {off:#X} n {n} = {:#X} at bus cycle {}",
                self.io_get(off, n),
                self.cycles
            );
        }
        let end = off + n;
        let hits = |lo: usize, hi: usize| off < hi && end > lo;
        if hits(0x280, 0x2A0) {
            self.divide();
        }
        if hits(0x2B0, 0x2C0) {
            self.sqrt();
        }
        for t in 0..4 {
            let base = 0x100 + 4 * t;
            if hits(base, base + 2) {
                self.timers[t].reload = self.io_get(base, 2) as u16;
                if let Some(vt) = self.vtime.as_mut().filter(|_| t < 2) {
                    vt.write_reload(t, self.timers[t].reload);
                }
            }
            if hits(base + 2, base + 4) {
                let ctrl = self.io_get(base + 2, 2) as u16;
                if let Some(vt) = self.vtime.as_mut().filter(|_| t < 2) {
                    vt.write_control(t, ctrl);
                }
                let was = self.timers[t].ctrl & 0x80 != 0;
                self.timers[t].ctrl = ctrl;
                if ctrl & 0x80 != 0 && !was {
                    self.timers[t].counter = self.timers[t].reload;
                    self.timers[t].acc = 0;
                }
            }
        }
        for ch in 0..4 {
            let base = 0xB0 + 12 * ch;
            if hits(base, base + 12) {
                self.dma_write(ch);
            }
        }
    }

    fn dma_write(&mut self, ch: usize) {
        let base = 0xB0 + 12 * ch;
        let d = &mut self.dma[ch];
        let was_on = d.ctrl & 0x8000_0000 != 0;
        d.src = rd(&self.io, base, 4) & 0x0FFF_FFFF;
        d.dst = rd(&self.io, base + 4, 4) & 0x0FFF_FFFF;
        let cnt = rd(&self.io, base + 8, 4);
        d.count = cnt & 0x1F_FFFF;
        d.ctrl = cnt & !0x1F_FFFF;
        if d.ctrl & 0x8000_0000 != 0 && !was_on {
            d.cur_src = d.src;
            d.cur_dst = d.dst;
            d.cur_len = if d.count == 0 { 0x20_0000 } else { d.count };
            // the geometry FIFO here never fills (every write goes to `gx`), so a GX FIFO DMA, which the
            // hardware feeds whenever the FIFO is under half full, can run straight through
            let timing = (d.ctrl >> 27) & 7;
            if timing == dma_timing::IMMEDIATE || timing == dma_timing::GEOMETRY_FIFO {
                self.run_dma(ch);
            }
        }
    }

    /// Run channel `ch` now (its trigger fired). Honours address control, word size, repeat and IRQ.
    pub fn run_dma(&mut self, ch: usize) {
        let d = self.dma[ch];
        if d.ctrl & 0x8000_0000 == 0 {
            return;
        }
        let word = d.ctrl & (1 << 26) != 0;
        let step = if word { 4i64 } else { 2 };
        let src_step = match (d.ctrl >> 23) & 3 {
            0 | 3 => step,
            1 => -step,
            _ => 0,
        };
        let dst_ctrl = (d.ctrl >> 21) & 3;
        let dst_step = match dst_ctrl {
            0 | 3 => step,
            1 => -step,
            _ => 0,
        };
        let (mut s, mut t) = (d.cur_src, d.cur_dst);
        for _ in 0..d.cur_len {
            if word {
                let v = self.read(s & !3, 4);
                self.write(t & !3, 4, v);
            } else {
                let v = self.read(s & !1, 2);
                self.write(t & !1, 2, v);
            }
            s = (s as i64 + src_step) as u32;
            t = (t as i64 + dst_step) as u32;
        }
        let d = &mut self.dma[ch];
        d.cur_src = s;
        let repeat = d.ctrl & (1 << 25) != 0 && (d.ctrl >> 27) & 7 != dma_timing::IMMEDIATE;
        if repeat {
            d.cur_len = if d.count == 0 { 0x20_0000 } else { d.count };
            d.cur_dst = if dst_ctrl == 3 { d.dst } else { t };
        } else {
            d.ctrl &= !0x8000_0000;
            let base = 0xB0 + 12 * ch + 8;
            let c = rd(&self.io, base, 4) & 0x7FFF_FFFF;
            wr(&mut self.io, base, 4, c);
        }
        if self.dma[ch].ctrl & (1 << 30) != 0 {
            self.raise(irq::DMA0 << ch);
        }
    }

    /// Fire every enabled channel whose start timing is `timing`.
    pub fn trigger_dma(&mut self, timing: u32) {
        for ch in 0..4 {
            let d = self.dma[ch];
            if d.ctrl & 0x8000_0000 != 0 && (d.ctrl >> 27) & 7 == timing {
                self.run_dma(ch);
            }
        }
    }

    /// Hardware divider (DIVCNT/DIV_NUMER/DIV_DENOM -> DIV_RESULT/DIVREM_RESULT). Mode 0: 32/32,
    /// 1: 64/32, 2: 64/64 (mode 3 behaves as 1). Division by zero sets DIVCNT bit 14 and gives +-1.
    fn divide(&mut self) {
        let mode = self.io_get(0x280, 2) & 3;
        let (n, d) = match mode {
            0 => (
                self.io_get(0x290, 4) as i32 as i64,
                self.io_get(0x298, 4) as i32 as i64,
            ),
            1 | 3 => (self.num64(0x290), self.io_get(0x298, 4) as i32 as i64),
            _ => (self.num64(0x290), self.num64(0x298)),
        };
        let (q, r, zero) = if d == 0 {
            let q = if n < 0 { 1i64 } else { -1 };
            // in the 32-bit mode the upper word of the quotient is the inverse of the lower
            let q = if mode == 0 {
                ((!(q as i32 as u32) as u64) << 32 | q as i32 as u32 as u64) as i64
            } else {
                q
            };
            (q, n, true)
        } else if n == i64::MIN && d == -1 {
            (i64::MIN, 0, false)
        } else {
            (n / d, n % d, false)
        };
        let st = self.io_get(0x280, 2) & !0x4000 | if zero { 0x4000 } else { 0 };
        self.io_set(0x280, 2, st);
        self.set64(0x2A0, q as u64);
        self.set64(0x2A8, r as u64);
    }

    fn num64(&self, off: usize) -> i64 {
        (self.io_get(off, 4) as u64 | (self.io_get(off + 4, 4) as u64) << 32) as i64
    }
    fn set64(&mut self, off: usize, v: u64) {
        self.io_set(off, 4, v as u32);
        self.io_set(off + 4, 4, (v >> 32) as u32);
    }

    /// Hardware square root: floor(sqrt(param)); mode bit 0 selects a 64-bit input, else the low 32 bits.
    fn sqrt(&mut self) {
        let p = if self.io_get(0x2B0, 2) & 1 != 0 {
            self.num64(0x2B8) as u64
        } else {
            self.io_get(0x2B8, 4) as u64
        };
        let mut r = (p as f64).sqrt() as u64;
        while r * r > p {
            r -= 1;
        }
        while (r + 1) * (r + 1) <= p {
            r += 1;
        }
        self.io_set(0x2B4, 4, r as u32);
    }

    /// Advance hardware time by `cycles` bus cycles: scanline counter (V-blank/H-blank/VCOUNT flags,
    /// interrupts and DMA triggers) and timers (with count-up chaining and IRQs).
    pub fn advance(&mut self, cycles: u64) {
        self.gx_fifo_irq();
        self.cycles += cycles;
        let mut left = cycles;
        while left > 0 {
            let to_hblank = HBLANK_START.saturating_sub(self.line_cycles);
            let to_end = CYCLES_PER_LINE - self.line_cycles;
            let step = if to_hblank > 0 {
                to_hblank.min(left)
            } else {
                to_end.min(left)
            };
            self.line_cycles += step;
            left -= step;
            self.tick_timers(step);
            if self.line_cycles == HBLANK_START {
                let s = self.io_get(0x004, 2) | 2;
                self.io_set(0x004, 2, s);
                if s & 0x10 != 0 {
                    self.raise(irq::HBLANK);
                }
                if self.line < VISIBLE_LINES {
                    self.trigger_dma(dma_timing::HBLANK);
                }
            }
            if self.line_cycles >= CYCLES_PER_LINE {
                self.line_cycles = 0;
                self.line = (self.line + 1) % LINES;
                let mut s = self.io_get(0x004, 2) & !2;
                let vc = (s >> 8 | (s & 0x80) << 1) as u64;
                if self.line == 0 {
                    let d = self.io_get(0x064, 4);
                    self.capture_latch = if d >> 31 == 1 { d } else { 0 };
                }
                if self.line == VISIBLE_LINES && self.capture_latch != 0 {
                    let d = self.io_get(0x064, 4) & 0x7fff_ffff; // the capture is done
                    self.io_set(0x064, 4, d);
                }
                if self.line == VISIBLE_LINES {
                    if let Some(vt) = self.vtime.as_mut() {
                        let bits = vt.frame();
                        self.raise(bits);
                    }
                    s |= 1;
                    if s & 0x8 != 0 {
                        self.raise(irq::VBLANK);
                    }
                    self.trigger_dma(dma_timing::VBLANK);
                } else if self.line == 262 {
                    s &= !1;
                }
                if self.line == vc {
                    s |= 4;
                    if s & 0x20 != 0 {
                        self.raise(irq::VCOUNT);
                    }
                } else {
                    s &= !4;
                }
                self.io_set(0x004, 2, s);
                self.io_set(0x006, 2, self.line as u32);
                if self.line < VISIBLE_LINES {
                    self.trigger_dma(dma_timing::DISPLAY_START);
                    self.trigger_dma(dma_timing::MAIN_MEM_DISPLAY);
                }
            }
        }
    }

    fn gx_fifo_irq(&mut self) {
        // The immediate consumer satisfies both supported FIFO IRQ conditions.
        if matches!(self.io_get(0x600, 4) >> 30, 1 | 2) {
            self.raise(irq::GXFIFO);
        }
    }

    fn tick_timers(&mut self, cycles: u64) {
        let mut overflow_prev = 0u64;
        for t in 0..4 {
            let tm = self.timers[t];
            if tm.ctrl & 0x80 == 0 || (t < 2 && self.vtime.is_some()) {
                overflow_prev = 0;
                continue;
            }
            let ticks = if tm.ctrl & 4 != 0 && t > 0 {
                overflow_prev
            } else {
                let div = [1u64, 64, 256, 1024][(tm.ctrl & 3) as usize];
                let acc = tm.acc + cycles;
                self.timers[t].acc = acc % div;
                acc / div
            };
            let mut overflowed = 0u64;
            if ticks > 0 {
                let mut c = tm.counter as u64 + ticks;
                while c > 0xFFFF {
                    c = c - 0x1_0000 + tm.reload as u64;
                    overflowed += 1;
                    if tm.ctrl & 0x40 != 0 {
                        self.raise(irq::TIMER0 << t);
                    }
                }
                self.timers[t].counter = c as u16;
            }
            overflow_prev = overflowed;
        }
    }
}

impl Mem for Bus {
    fn r8(&mut self, a: u32) -> u8 {
        self.read(a, 1) as u8
    }
    fn r16(&mut self, a: u32) -> u16 {
        self.read(a & !1, 2) as u16
    }
    fn r32(&mut self, a: u32) -> u32 {
        self.read(a & !3, 4)
    }
    fn w8(&mut self, a: u32, v: u8) {
        self.write(a, 1, v as u32)
    }
    fn w16(&mut self, a: u32, v: u16) {
        self.write(a & !1, 2, v as u32)
    }
    fn w32(&mut self, a: u32, v: u32) {
        self.write(a & !3, 4, v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn main_ram_mirrors_and_dtcm_overrides() {
        let mut b = Bus::new();
        b.w32(0x0200_0010, 0xDEAD_BEEF);
        assert_eq!(b.r32(0x0240_0010), 0xDEAD_BEEF); // 4 MiB mirror
        b.dtcm_base = Some(0x027E_0000);
        b.w32(0x027E_0000, 7);
        assert_eq!(b.r32(0x027E_0000), 7);
        assert_eq!(b.r32(0x0200_0010), 0xDEAD_BEEF);
        assert_eq!(b.main[0x3E_0000], 0, "DTCM write must not reach main RAM");
    }

    #[test]
    fn if_is_write_one_to_clear() {
        let mut b = Bus::new();
        b.raise(irq::VBLANK | irq::TIMER0);
        b.w32(IO_BASE + 0x214, irq::VBLANK);
        assert_eq!(b.r32(IO_BASE + 0x214), irq::TIMER0);
        assert!(!b.irq_pending());
        b.w32(IO_BASE + 0x210, irq::TIMER0);
        b.w32(IO_BASE + 0x208, 1);
        assert!(b.irq_pending());
    }

    #[test]
    fn immediate_dma_copies_words_and_clears_enable() {
        let mut b = Bus::new();
        for i in 0..4u32 {
            b.w32(0x0200_0000 + 4 * i, 0x1111_1111 * (i + 1));
        }
        b.w32(IO_BASE + 0xB0, 0x0200_0000);
        b.w32(IO_BASE + 0xB4, 0x0200_1000);
        b.w32(IO_BASE + 0xB8, 4 | 1 << 26 | 1 << 31 | 1 << 30);
        assert_eq!(b.r32(0x0200_100C), 0x4444_4444);
        assert_eq!(b.r32(IO_BASE + 0xB8) & 0x8000_0000, 0);
        assert!(b.r32(IO_BASE + 0x214) & irq::DMA0 != 0);
    }

    #[test]
    fn dma_fixed_destination_and_halfwords() {
        let mut b = Bus::new();
        b.w16(0x0200_0000, 0xAAAA);
        b.w16(0x0200_0002, 0xBBBB);
        b.w32(IO_BASE + 0xBC, 0x0200_0000); // channel 1 src
        b.w32(IO_BASE + 0xC0, 0x0200_2000); // dst
        b.w32(IO_BASE + 0xC4, 2 | 2 << 21 | 1 << 31);
        assert_eq!(b.r16(0x0200_2000), 0xBBBB);
    }

    #[test]
    fn vblank_dma_waits_for_the_trigger() {
        let mut b = Bus::new();
        b.w32(0x0200_0000, 0x1234_5678);
        b.w32(IO_BASE + 0xB0, 0x0200_0000);
        b.w32(IO_BASE + 0xB4, 0x0200_0100);
        b.w32(IO_BASE + 0xB8, 1 | 1 << 26 | 1 << 27 | 1 << 31);
        assert_eq!(b.r32(0x0200_0100), 0);
        b.advance(CYCLES_PER_LINE * VISIBLE_LINES);
        assert_eq!(b.r32(0x0200_0100), 0x1234_5678);
        assert_eq!(b.r16(IO_BASE + 4) & 1, 1, "V-blank flag set");
    }

    #[test]
    fn vcount_and_vblank_irq() {
        let mut b = Bus::new();
        b.w16(IO_BASE + 4, 0x8);
        b.advance(CYCLES_PER_LINE * 191 + 10);
        assert_eq!(b.r16(IO_BASE + 6), 191);
        assert_eq!(b.r32(IO_BASE + 0x214) & irq::VBLANK, 0);
        b.advance(CYCLES_PER_LINE);
        assert_eq!(b.r16(IO_BASE + 6), 192);
        assert!(b.r32(IO_BASE + 0x214) & irq::VBLANK != 0);
        b.advance(CYCLES_PER_LINE * 71);
        assert_eq!(b.r16(IO_BASE + 6), 0);
    }

    #[test]
    fn timer_overflows_reloads_and_chains() {
        let mut b = Bus::new();
        b.w16(IO_BASE + 0x100, 0xFFF0);
        b.w16(IO_BASE + 0x102, 0x80 | 0x40);
        b.w16(IO_BASE + 0x104, 0);
        b.w16(IO_BASE + 0x106, 0x80 | 4);
        b.advance(0x20);
        assert_eq!(
            b.r16(IO_BASE + 0x100),
            0xFFF0,
            "two overflows of a 0x10-tick period"
        );
        assert_eq!(
            b.r16(IO_BASE + 0x104),
            2,
            "count-up timer ticked on overflow"
        );
        assert!(b.r32(IO_BASE + 0x214) & irq::TIMER0 != 0);
    }

    #[test]
    fn hardware_divider() {
        let mut b = Bus::new();
        b.w16(IO_BASE + 0x280, 0);
        b.w32(IO_BASE + 0x290, (-100i32) as u32);
        b.w32(IO_BASE + 0x298, 7);
        assert_eq!(b.r32(IO_BASE + 0x2A0) as i32, -14);
        assert_eq!(b.r32(IO_BASE + 0x2A8) as i32, -2);
        b.w16(IO_BASE + 0x280, 2);
        b.w32(IO_BASE + 0x290, 0);
        b.w32(IO_BASE + 0x294, 1); // 2^32
        b.w32(IO_BASE + 0x298, 16);
        b.w32(IO_BASE + 0x29C, 0);
        assert_eq!(b.r32(IO_BASE + 0x2A0), 0x1000_0000);
        b.w32(IO_BASE + 0x298, 0);
        assert!(b.r16(IO_BASE + 0x280) & 0x4000 != 0, "div by zero flag");
    }

    #[test]
    fn hardware_sqrt() {
        let mut b = Bus::new();
        b.w16(IO_BASE + 0x2B0, 0);
        b.w32(IO_BASE + 0x2B8, 1_000_000);
        assert_eq!(b.r32(IO_BASE + 0x2B4), 1000);
        b.w16(IO_BASE + 0x2B0, 1);
        b.w32(IO_BASE + 0x2B8, 0);
        b.w32(IO_BASE + 0x2BC, 1);
        assert_eq!(b.r32(IO_BASE + 0x2B4), 65536);
    }

    #[test]
    fn vram_banks_follow_vramcnt() {
        let mut b = Bus::new();
        assert_eq!(b.r32(0x0600_0000), 0);
        b.w8(IO_BASE + 0x240, 0x80 | 1); // bank A -> BG A
        b.w32(0x0600_0100, 42);
        assert_eq!(b.vram_bank(0)[0x100], 42);
        b.w8(IO_BASE + 0x241, 0x80 | 1 | 1 << 3); // bank B -> BG A + 0x20000
        b.w32(0x0602_0000, 9);
        assert_eq!(b.vram_bank(1)[0], 9);
        b.w8(IO_BASE + 0x240, 0x80); // bank A -> LCDC
        b.w32(0x0680_0000, 5);
        assert_eq!(b.vram_bank(0)[0], 5);
    }

    #[test]
    fn gx_ports_are_logged_not_stored() {
        let mut b = Bus::new();
        b.w32(IO_BASE + 0x400, 0x1234);
        b.w32(IO_BASE + 0x444, 0xFFFF);
        assert_eq!(b.gx, vec![(0x400, 0x1234, 4), (0x444, 0xFFFF, 4)]);
    }

    #[test]
    fn gx_fifo_ready_status_and_irq_unblock_streamed_display_lists() {
        let mut b = Bus::new();
        assert_eq!(b.r32(IO_BASE + 0x600), 0x0600_0000);
        assert_eq!(b.r8(IO_BASE + 0x603), 6);
        assert_eq!(b.r16(IO_BASE + 0x602), 0x0600);
        b.w32(IO_BASE + 0x600, 0x4000_8000); // half-full IRQ, clear stack error
        assert_eq!(b.r32(IO_BASE + 0x600), 0x4600_0000);
        assert_ne!(b.r32(IO_BASE + 0x214) & irq::GXFIFO, 0);
        assert!(
            b.gx.is_empty(),
            "status writes must not enter geometry command log"
        );
        b.w32(IO_BASE + 0x214, irq::GXFIFO);
        b.advance(1);
        assert_ne!(
            b.r32(IO_BASE + 0x214) & irq::GXFIFO,
            0,
            "level condition persists"
        );
        b.w8(IO_BASE + 0x603, 0x80); // empty IRQ via byte write
        assert_eq!(b.r32(IO_BASE + 0x600), 0x8600_0000);
        b.w16(IO_BASE + 0x602, 0); // disable IRQ via halfword write
        b.w32(IO_BASE + 0x214, irq::GXFIFO);
        b.advance(1);
        assert_eq!(b.r32(IO_BASE + 0x214) & irq::GXFIFO, 0);
        b.w32(IO_BASE + 0x400, 0x1234);
        assert_eq!(
            b.r32(IO_BASE + 0x600) & 0x0600_0000,
            0x0600_0000,
            "a retained diagnostic log is not a full hardware FIFO"
        );
    }

    #[test]
    fn arm7_sync_handshake_counts_up_on_echo() {
        let mut b = Bus::new();
        // what PXI_Init does: echo the nibble read, until >4 changes and the value is back at 0
        let (mut changes, mut done) = (0, false);
        for _ in 0..40 {
            let lr = b.r16(IO_BASE + 0x180) & 0xF;
            b.w16(IO_BASE + 0x180, lr << 8);
            if lr == 0 && changes > 4 {
                done = true;
                break;
            }
            if b.r16(IO_BASE + 0x180) & 0xF != lr {
                changes += 1;
            }
        }
        assert!(done);
    }

    #[test]
    fn arm7_fifo_delivers_and_interrupts() {
        let mut b = Bus::new();
        assert_eq!(
            b.r16(IO_BASE + 0x184) & 0x101,
            0x101,
            "send and recv empty at reset"
        );
        b.w16(IO_BASE + 0x184, 0x8400); // enable FIFO + recv-not-empty IRQ
        b.arm7_send(0xABCD);
        assert!(b.r32(IO_BASE + 0x214) & irq::IPC_RECV_NOT_EMPTY != 0);
        assert_eq!(b.r16(IO_BASE + 0x184) & 0x100, 0, "recv not empty");
        assert_eq!(b.r32(0x0410_0000), 0xABCD);
        assert_eq!(b.r16(IO_BASE + 0x184) & 0x100, 0x100);
        assert_eq!(b.r32(0x0410_0000), 0);
        assert!(
            b.r16(IO_BASE + 0x184) & 0x4000 != 0,
            "error flag after reading empty"
        );
        b.w16(IO_BASE + 0x184, 0x8400 | 0x4000);
        assert_eq!(b.r16(IO_BASE + 0x184) & 0x4000, 0);
    }

    #[test]
    fn keys_are_active_low() {
        let mut b = Bus::new();
        assert_eq!(b.r16(IO_BASE + 0x130), 0x03FF);
        b.set_keys(1);
        assert_eq!(b.r16(IO_BASE + 0x130), 0x03FE);
    }

    #[test]
    fn keypad_irq_edges_and_read_only_pins_on_bus() {
        let mut b = Bus::new();
        b.w8(IO_BASE + 0x132, 1);
        b.w8(IO_BASE + 0x133, 0x40);
        b.set_keys(1);
        assert_eq!(b.r32(IO_BASE + 0x214), irq::KEYPAD);
        b.w32(IO_BASE + 0x214, irq::KEYPAD);
        b.set_keys(1);
        assert_eq!(b.r32(IO_BASE + 0x214), 0);
        b.w16(IO_BASE + 0x130, 0);
        assert_eq!(b.r16(IO_BASE + 0x130), 0x03fe);
        b.set_keys(0);
        b.set_keys(1);
        assert_eq!(b.r32(IO_BASE + 0x214), irq::KEYPAD);
        b.w32(IO_BASE + 0x130, 0xc0030000);
        assert_eq!(b.r16(IO_BASE + 0x130), 0x03fe);
        assert_eq!(b.r16(IO_BASE + 0x132), 0xc003);
    }

    #[test]
    fn register_table_is_sorted_by_offset_per_block() {
        let mut seen = std::collections::HashSet::new();
        for (o, _, name, _) in REGS {
            assert!(seen.insert(*name), "duplicate {name} at {o:#X}");
        }
    }
}
