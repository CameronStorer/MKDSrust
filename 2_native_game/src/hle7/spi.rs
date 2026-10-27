//! The ARM7's SPI-bus services: touch panel (tag 6), firmware flash "NVRAM" (tag 4) and power management
//! (tag 8); the microphone (tag 9) uses the same format but stays on the ARM7 CPU (see [`super::CLAIMED`]).
//! They share one command format (`spi_pxi_callback` and the per-device
//! `*_pxi_receive`): a command is a packet of up to 16 halfwords, each word carrying bit 25 (start, clears
//! the packet), bit 24 (end, runs it), bits 16-19 (halfword index) and bits 0-15 (the halfword). Halfword 0
//! is `command << 8 | argument byte`. The reply (`spi_command_device_maybe` 0x0380146C) is one word with
//! data `0x3000000 | (command | 0x80) << 8 | result`, on the tag the command's class selects
//! (`command & 0x70`: 0x00/0x10 touch, 0x20/0x30 NVRAM, 0x40/0x50 mic, 0x60/0x70 PM).
use super::{pack, shared, tag, Router};
use crate::ds_mem::Bus;
use crate::peripherals::Peripherals;
use crate::rt::Mem;

/// Result codes of the SPI services.
pub mod result {
    pub const OK: u32 = 0;
    pub const INVALID_COMMAND: u32 = 1;
    pub const INVALID_PARAMETER: u32 = 2;
    pub const ILLEGAL_STATUS: u32 = 3;
}

#[derive(Default)]
pub struct Spi {
    /// Packet buffers by tag.
    packets: [[u16; 16]; 16],
    /// Words of the current packet, by tag (kept so an unimplemented command can be forwarded whole).
    raw: [Vec<u32>; 16],
    pub touch: Touch,
    /// Firmware flash write-enable latch (set by WREN, cleared by WRDI and by a completed write).
    pub nvram_wel: bool,
    /// Power-management LED blink pattern (`pm_set_led_pattern`); the blinking itself is not modelled.
    pub led_pattern: u8,
}

/// Touch-panel service state (`tp_*`, work area 0x03809DD4).
#[derive(Default)]
pub struct Touch {
    /// 0 idle, 2 auto-sampling (1 and 3 are the ARM7's transient start/stop states).
    pub state: u8,
    /// V-count lines of the auto-sampling alarms, one per sample per frame.
    pub vcounts: Vec<u16>,
    /// Last scanline seen by [`touch_alarms`].
    last_line: Option<u64>,
}

/// The SPI reply word for `cmd` with `result` (the tag follows the command class).
pub fn reply_word(cmd: u32, result: u32) -> u32 {
    let t = match cmd & 0x70 {
        0x00 | 0x10 => tag::TOUCH,
        0x20 | 0x30 => tag::NVRAM,
        0x40 | 0x50 => tag::MIC,
        _ => tag::PM,
    };
    pack(t, 0x300_0000 | ((cmd & 0xFF) | 0x80) << 8 | result & 0xFF, false)
}

/// One touch sample as the ARM7 publishes it (`tp_read_sample` + `tp_update_shared_coords`) for the
/// SPI touch controller of [`crate::peripherals::Spi`]: pen down = raw x/y, touch bit, validity 0 (the
/// model's readings are perfectly stable); pen up = x 0, y 0, touch 0, validity 3 (both axes invalid).
pub fn touch_sample(p: &Peripherals) -> u32 {
    match p.spi.pen {
        Some((x, y)) => (x as u32 & 0xFFF) | (y as u32 & 0xFFF) << 12 | 1 << 24,
        None => 3 << 25,
    }
}

fn publish_touch(hw: &mut Bus, p: &Peripherals) {
    let s = touch_sample(p);
    hw.w16(shared::TOUCH, s as u16);
    hw.w16(shared::TOUCH + 2, (s >> 16) as u16);
}

/// Packet assembly; runs the command on the end bit.
pub fn receive(r: &mut Router, hw: &mut Bus, p: &mut Peripherals, w: u32) {
    let t = (w & 0xF) as usize;
    let data = w >> 6;
    if data & 0x200_0000 != 0 {
        r.spi.packets[t] = [0; 16];
        r.spi.raw[t].clear();
    }
    r.spi.packets[t][((data >> 16) & 0xF) as usize] = data as u16;
    r.spi.raw[t].push(w);
    if data & 0x100_0000 == 0 {
        return;
    }
    let buf = r.spi.packets[t];
    let handled = match t as u32 {
        tag::TOUCH => touch(r, hw, p, &buf),
        tag::NVRAM => nvram(r, hw, p, &buf),
        tag::PM => pm(r, p, &buf),
        _ => false,
    };
    if !handled {
        for w in std::mem::take(&mut r.spi.raw[t]) {
            r.forward(hw, w);
        }
    }
}

/// Touch panel (`tp_pxi_receive`, `tp_process_command`). Returns false for a command left to the CPU.
fn touch(r: &mut Router, hw: &mut Bus, p: &mut Peripherals, buf: &[u16; 16]) -> bool {
    let cmd = (buf[0] >> 8) as u32;
    let arg = (buf[0] & 0xFF) as u32;
    let tp = &mut r.spi.touch;
    let reply = match cmd {
        // one sample now
        0 => {
            publish_touch(hw, p);
            result::OK
        }
        // auto-sampling start: `arg` samples per frame (1-4) from V-count line buf[1]
        1 if tp.state != 0 => result::ILLEGAL_STATUS,
        1 if arg == 0 || arg > 4 || buf[1] >= 263 => result::INVALID_PARAMETER,
        1 => {
            tp.vcounts = (0..arg)
                .map(|i| ((buf[1] as u32 + i * 263 / arg) % 263) as u16)
                .collect();
            tp.state = 2;
            tp.last_line = Some(hw.line);
            result::OK
        }
        // auto-sampling stop
        2 if tp.state != 2 => result::ILLEGAL_STATUS,
        2 => {
            tp.state = 0;
            tp.vcounts.clear();
            result::OK
        }
        // 3: stability range (left to the CPU: it only tunes the CPU's own sampler)
        3 => return false,
        _ => result::INVALID_COMMAND,
    };
    r.reply(reply_word(cmd, reply));
    true
}

/// Auto-sampling: at each sampling line the ARM7's periodic V-alarm takes a sample, publishes it and
/// sends `0x90 << 8 | index`.
pub fn touch_alarms(r: &mut Router, hw: &mut Bus, p: &mut Peripherals) {
    let tp = &mut r.spi.touch;
    if tp.state != 2 {
        return;
    }
    let now = hw.line;
    let Some(mut line) = tp.last_line else {
        tp.last_line = Some(now);
        return;
    };
    let mut hits = Vec::new();
    while line != now {
        line = (line + 1) % crate::ds_mem::LINES;
        for (i, &v) in tp.vcounts.iter().enumerate() {
            if v as u64 == line {
                hits.push(i as u32);
            }
        }
    }
    tp.last_line = Some(now);
    for i in hits {
        publish_touch(hw, p);
        r.reply(reply_word(0x10, i));
    }
}

/// Firmware flash (`nvram_pxi_receive`, `nvram_*`), backed by the synthetic image in
/// [`crate::peripherals::Spi::firmware`]. Addresses of main-RAM buffers must lie in 0x02000000..0x02800000.
fn nvram(r: &mut Router, hw: &mut Bus, p: &mut Peripherals, buf: &[u16; 16]) -> bool {
    let cmd = (buf[0] >> 8) as u32;
    let arg = (buf[0] & 0xFF) as u32;
    let ram = |a: u32| (0x0200_0000..0x0280_0000).contains(&a);
    let w32 = |hi: u16, lo: u16| (hi as u32) << 16 | lo as u32;
    let fw = &mut p.spi.firmware;
    let size = fw.len();
    let result = match cmd {
        0x20 => {
            r.spi.nvram_wel = true;
            result::OK
        }
        0x21 => {
            r.spi.nvram_wel = false;
            result::OK
        }
        // RDSR: status byte (WEL = bit 1; never busy) to buffer `arg << 24 | buf[1] << 8 | buf[2] >> 8`
        0x22 => {
            let dst = arg << 24 | (buf[1] as u32) << 8 | (buf[2] as u32) >> 8;
            if !ram(dst) {
                result::INVALID_PARAMETER
            } else {
                hw.w8(dst, (r.spi.nvram_wel as u8) << 1);
                result::OK
            }
        }
        // READ: address `arg << 16 | buf[1]`, size `buf[2..3]`, destination `buf[4..5]`
        0x23 => {
            let (addr, len, dst) = (arg << 16 | buf[1] as u32, w32(buf[2], buf[3]), w32(buf[4], buf[5]));
            if !ram(dst) {
                result::INVALID_PARAMETER
            } else {
                for i in 0..len {
                    hw.w8(dst + i, fw[(addr + i) as usize % size]);
                }
                result::OK
            }
        }
        // PAGE WRITE (erase + write, wrapping within the 256-byte page): address `arg << 16 | buf[1]`,
        // size `buf[2]`, source `buf[3..4]`; needs WREN and clears it
        0x25 => {
            let (addr, len, src) = (arg << 16 | buf[1] as u32, buf[2] as u32, w32(buf[3], buf[4]));
            if !ram(src) {
                result::INVALID_PARAMETER
            } else {
                if r.spi.nvram_wel {
                    let page = addr as usize & !0xFF;
                    for i in 0..len {
                        let a = (page | (addr as usize + i as usize) & 0xFF) % size;
                        fw[a] = hw.r8(src + i);
                    }
                }
                r.spi.nvram_wel = false;
                result::OK
            }
        }
        _ => return false,
    };
    r.reply(reply_word(cmd, result));
    true
}

/// Power management (`pm_pxi_receive`, `pm_execute_command`), backed by the PM registers of
/// [`crate::peripherals::Spi`].
fn pm(r: &mut Router, p: &mut Peripherals, buf: &[u16; 16]) -> bool {
    let cmd = (buf[0] >> 8) as u32;
    let arg = (buf[0] & 0xFF) as u32;
    let (reply_cmd, result) = match cmd {
        0x60 => (0x60, result::OK),
        0x62 => (cmd, result::INVALID_COMMAND),
        // write register `arg` = low byte of buf[1]
        0x64 => {
            p.spi.power_regs[(arg & 7) as usize] = buf[1] as u8;
            (0x64, result::OK)
        }
        // read register: the reply's command byte is 0x70 + register, its result the value
        // (register 1, the battery status, reads "not low")
        0x65 => {
            let reg = arg & 7;
            let v = if reg == 1 { 0 } else { p.spi.power_regs[reg as usize] };
            ((0x70 + arg) & 0xFF, v as u32)
        }
        0x66 => {
            r.spi.led_pattern = arg as u8;
            (0x66, result::OK)
        }
        0x67 => (0x67, r.spi.led_pattern as u32),
        // 0x61 sleep, 0x63 LED status: left to the CPU
        _ => return false,
    };
    r.reply(reply_word(reply_cmd, result));
    true
}
