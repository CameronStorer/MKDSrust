//! ARM7-side peripherals the console's second CPU talks to over serial buses (T085, recomp lane L7):
//! the SPI bus (power management chip, firmware flash holding the user settings, touch-screen
//! controller), the real-time clock, and the cartridge backup memory (save data) on the card SPI bus.
//!
//! These are DS hardware, not game data. The firmware image is *synthetic*: it carries only what the
//! SDK reads (a valid header and a user-settings block with a blank nickname, English, and a touch
//! calibration we choose), never Nintendo's firmware. Save data starts erased (0xFF) and is owned by
//! the host (`Backup::data`), which can load and store it as a `.sav`.
//!
//! Register map (ARM7 I/O, offset from `0x04000000`): `0x1A0` AUXSPICNT, `0x1A2` AUXSPIDATA,
//! `0x138` RTC, `0x1C0` SPICNT, `0x1C2` SPIDATA. [`Peripherals::read`] / [`Peripherals::write`]
//! return `None` / `false` for any other offset so a caller can chain them in front of its own I/O.
use crate::bios::crc16;

/// Touch calibration written into the synthetic user settings (and used to turn a screen pixel into
/// the raw 12-bit ADC value a real panel would report): ADC and pixel for two corners.
pub const CAL_ADC1: (u16, u16) = (0x0100, 0x0100);
pub const CAL_PX1: (u8, u8) = (16, 16);
pub const CAL_ADC2: (u16, u16) = (0x0F00, 0x0B00);
pub const CAL_PX2: (u8, u8) = (240, 176);

/// Raw ADC values for a touch at screen pixel (x, y).
pub fn touch_raw(x: u8, y: u8) -> (u16, u16) {
    let lerp = |p: u8, p1: u8, p2: u8, a1: u16, a2: u16| -> u16 {
        let num = (p as i32 - p1 as i32) * (a2 as i32 - a1 as i32);
        (a1 as i32 + num / (p2 as i32 - p1 as i32)).clamp(0, 0xFFF) as u16
    };
    (lerp(x, CAL_PX1.0, CAL_PX2.0, CAL_ADC1.0, CAL_ADC2.0), lerp(y, CAL_PX1.1, CAL_PX2.1, CAL_ADC1.1, CAL_ADC2.1))
}

// ---------------------------------------------------------------------------------------------
// Firmware flash
// ---------------------------------------------------------------------------------------------

pub const FIRMWARE_SIZE: usize = 0x4_0000;
pub const USER_SETTINGS_OFFSET: usize = 0x3_FE00;

/// A synthetic 256 KiB firmware image: header fields the SDK reads, and two user-settings blocks
/// (the second one is the newer copy, as on a console that has saved settings once).
pub fn synthetic_firmware() -> Vec<u8> {
    let mut f = vec![0xFFu8; FIRMWARE_SIZE];
    f[0x20..0x22].copy_from_slice(&((USER_SETTINGS_OFFSET >> 3) as u16).to_le_bytes());
    // console type 0x20 = original DS; MAC address 00:09:BF:00:00:01
    f[0x1D] = 0x20;
    f[0x36..0x3C].copy_from_slice(&[0x00, 0x09, 0xBF, 0x00, 0x00, 0x01]);
    for (i, counter) in [(0usize, 0u16), (1, 1)] {
        let at = USER_SETTINGS_OFFSET + 0x100 * i;
        let mut b = [0u8; 0x100];
        b[0..2].copy_from_slice(&5u16.to_le_bytes()); // version
        b[2] = 0; // favourite colour
        b[3] = 1; // birthday: month
        b[4] = 1; // day
        for (j, ch) in "Player".encode_utf16().enumerate() {
            b[6 + 2 * j..8 + 2 * j].copy_from_slice(&ch.to_le_bytes());
        }
        b[0x1A..0x1C].copy_from_slice(&6u16.to_le_bytes()); // nickname length
        b[0x58..0x5A].copy_from_slice(&CAL_ADC1.0.to_le_bytes());
        b[0x5A..0x5C].copy_from_slice(&CAL_ADC1.1.to_le_bytes());
        b[0x5C] = CAL_PX1.0;
        b[0x5D] = CAL_PX1.1;
        b[0x5E..0x60].copy_from_slice(&CAL_ADC2.0.to_le_bytes());
        b[0x60..0x62].copy_from_slice(&CAL_ADC2.1.to_le_bytes());
        b[0x62] = CAL_PX2.0;
        b[0x63] = CAL_PX2.1;
        b[0x64..0x66].copy_from_slice(&1u16.to_le_bytes()); // language: English
        b[0x70..0x72].copy_from_slice(&counter.to_le_bytes());
        let crc = crc16(0xFFFF, &b[..0x70]);
        b[0x72..0x74].copy_from_slice(&crc.to_le_bytes());
        f[at..at + 0x100].copy_from_slice(&b);
    }
    f
}

// ---------------------------------------------------------------------------------------------
// SPI bus
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Dev {
    Power,
    Firmware,
    Touch,
    None,
}

/// SPICNT/SPIDATA with the three devices behind it. A transfer is one byte; the chip stays selected
/// while the *hold* bit (11) is set on the transfers of a transaction, and the transaction's state
/// is reset when a transfer is made without it.
pub struct Spi {
    pub cnt: u16,
    reply: u8,
    pub power_regs: [u8; 8],
    pub firmware: Vec<u8>,
    /// Raw ADC position of a pen on the screen, `None` when up.
    pub pen: Option<(u16, u16)>,
    // transaction state
    sel: Dev,
    step: usize,
    cmd: u8,
    addr: usize,
    touch_out: std::collections::VecDeque<u8>,
    /// Firmware flash write-enable latch (WEL), cleared when a program/erase transaction ends.
    wel: bool,
}

impl Default for Spi {
    fn default() -> Self {
        Spi::new()
    }
}

impl Spi {
    pub fn new() -> Spi {
        Spi {
            cnt: 0,
            reply: 0,
            power_regs: [0; 8],
            firmware: synthetic_firmware(),
            pen: None,
            sel: Dev::None,
            step: 0,
            cmd: 0,
            addr: 0,
            touch_out: Default::default(),
            wel: false,
        }
    }

    fn device(&self) -> Dev {
        match (self.cnt >> 8) & 3 {
            0 => Dev::Power,
            1 => Dev::Firmware,
            2 => Dev::Touch,
            _ => Dev::None,
        }
    }

    pub fn write_cnt(&mut self, v: u16) {
        // bit 7 (busy) is read-only; transfers complete instantly in this model
        self.cnt = v & 0xCF03;
    }

    pub fn read_cnt(&self) -> u16 {
        self.cnt
    }

    pub fn read_data(&self) -> u8 {
        self.reply
    }

    /// Writes SPIDATA: exchange one byte with the selected device. Ignored while the bus is disabled.
    pub fn write_data(&mut self, byte: u8) {
        if self.cnt & 0x8000 == 0 {
            return;
        }
        let dev = self.device();
        if dev != self.sel {
            self.sel = dev;
            self.step = 0;
            self.touch_out.clear();
        }
        let hold = self.cnt & 0x800 != 0;
        self.reply = match dev {
            Dev::Power => self.power(byte),
            Dev::Firmware => self.flash(byte),
            Dev::Touch => self.touch(byte),
            Dev::None => 0,
        };
        if trace_enabled() {
            eprintln!("spi dev {:?} step {} in {:#04X} -> {:#04X}{}", dev, self.step, byte, self.reply, if hold { " (hold)" } else { "" });
        }
        self.step += 1;
        if !hold {
            if dev == Dev::Firmware && matches!(self.cmd, 0x02 | 0x0A | 0xDB | 0xD8) && self.step > 0 {
                self.wel = false; // a completed program/erase clears the write-enable latch
            }
            self.step = 0;
            self.sel = Dev::None;
            self.touch_out.clear();
        }
    }

    fn power(&mut self, b: u8) -> u8 {
        match self.step {
            0 => {
                self.cmd = b;
                0
            }
            _ => {
                let reg = (self.cmd & 7) as usize;
                if self.cmd & 0x80 != 0 {
                    // register 1 is the battery status: bit 0 = low battery (never)
                    if reg == 1 { 0 } else { self.power_regs[reg] }
                } else {
                    self.power_regs[reg] = b;
                    0
                }
            }
        }
    }

    fn flash(&mut self, b: u8) -> u8 {
        if self.step == 0 {
            self.cmd = b;
            self.addr = 0;
            match b {
                0x06 => self.wel = true,
                0x04 => self.wel = false,
                _ => {}
            }
            return 0;
        }
        let size = self.firmware.len();
        match self.cmd {
            // program (0x02, ANDs into the page), page write (0x0A, erases the page first), page/sector erase
            0x02 | 0x0A | 0xDB | 0xD8 => {
                if self.step <= 3 {
                    self.addr = self.addr << 8 | b as usize;
                    if self.step == 3 && self.wel {
                        match self.cmd {
                            0x0A => self.firmware[(self.addr & !0xFF) % size..][..0x100].fill(0xFF),
                            0xDB => self.firmware[(self.addr & !0xFF) % size..][..0x100].fill(0xFF),
                            0xD8 => self.firmware[(self.addr & !0xFFFF) % size..][..0x1_0000].fill(0xFF),
                            _ => {}
                        }
                    }
                } else if self.wel && matches!(self.cmd, 0x02 | 0x0A) {
                    let a = self.addr % size;
                    self.firmware[a] &= b;
                    self.addr = (self.addr & !0xFF) | ((self.addr + 1) & 0xFF); // wraps within the page
                }
                0
            }
            0x03 | 0x0B => {
                // READ: 3 address bytes, then data (fast read has one dummy byte after the address)
                if self.step <= 3 {
                    self.addr = self.addr << 8 | b as usize;
                    0
                } else if self.cmd == 0x0B && self.step == 4 {
                    0
                } else {
                    let v = self.firmware[self.addr % self.firmware.len()];
                    self.addr += 1;
                    v
                }
            }
            0x05 => (self.wel as u8) << 1, // status: WEL; never busy (WIP = 0)
            0x9F => [0x20, 0x40, 0x12][(self.step - 1).min(2)], // JEDEC id of a 256 KiB part
            _ => 0,
        }
    }

    fn touch(&mut self, b: u8) -> u8 {
        let reply = self.touch_out.pop_front().unwrap_or(0);
        if b & 0x80 != 0 {
            let raw = |s: &Spi| s.pen.unwrap_or((0, 0xFFF));
            let v: u16 = match (b >> 4) & 7 {
                1 => raw(self).1, // Y position
                5 => raw(self).0, // X position
                3 => if self.pen.is_some() { 0x100 } else { 0 }, // Z1 pressure
                4 => if self.pen.is_some() { 0x800 } else { 0xFFF }, // Z2 pressure
                6 => 0x600, // auxiliary input / temperature
                _ => 0,
            };
            // a conversion shifts out MSB first over the next 16 clocks: 0, v[11..5], then v[4..0] + padding
            self.touch_out.clear();
            self.touch_out.push_back(((v >> 5) & 0x7F) as u8);
            self.touch_out.push_back(((v << 3) & 0xF8) as u8);
        }
        reply
    }
}

// ---------------------------------------------------------------------------------------------
// Real-time clock (bit-banged serial, register 0x138)
// ---------------------------------------------------------------------------------------------

/// The RTC chip behind the single serial register. Commands are `0b0110_rrrR` (register `r`, `R` =
/// read) sent MSB first; data bytes go LSB first, with the device driving the data bit after each
/// rising clock edge when reading.
pub struct Rtc {
    pub status1: u8,
    pub status2: u8,
    /// Year (0-99), month, day, weekday, hour, minute, second, as plain numbers.
    pub datetime: [u8; 7],
    reg: u8, // last value written to the port (data/clock/select + direction bits)
    in_bits: u8,
    nbits: usize,
    cmd: Option<u8>,
    out: Vec<u8>,
    out_bit: usize,
    in_data: Vec<u8>,
    data_bit: u8,
}

fn bcd(v: u8) -> u8 {
    (v / 10) << 4 | v % 10
}

impl Default for Rtc {
    fn default() -> Self {
        Rtc::new()
    }
}

impl Rtc {
    pub fn new() -> Rtc {
        Rtc {
            status1: 0x02, // 24-hour mode
            status2: 0,
            datetime: [10, 6, 15, 2, 12, 0, 0],
            reg: 0,
            in_bits: 0,
            nbits: 0,
            cmd: None,
            out: Vec::new(),
            out_bit: 0,
            in_data: Vec::new(),
            data_bit: 0,
        }
    }

    /// Date and time as the chip sends them (BCD: year, month, day, weekday, hour, minute, second); the
    /// native RTC service (`crate::hle7`) answers with these.
    pub fn datetime_bcd(&self) -> [u8; 7] {
        self.datetime_bytes()
    }

    fn datetime_bytes(&self) -> [u8; 7] {
        let d = self.datetime;
        [bcd(d[0]), bcd(d[1]), bcd(d[2]), bcd(d[3]), bcd(d[4]), bcd(d[5]), bcd(d[6])]
    }

    fn read_reg(&self, r: u8) -> Vec<u8> {
        let dt = self.datetime_bytes();
        match r {
            0 => vec![self.status1],
            1 => vec![self.status2],
            2 => dt.to_vec(),
            3 => dt[4..].to_vec(),
            _ => vec![0; 3],
        }
    }

    fn bytes_for(r: u8) -> usize {
        match r {
            0 | 1 | 6 | 7 => 1,
            2 => 7,
            _ => 3,
        }
    }

    pub fn read(&self) -> u8 {
        let mut v = self.reg & !1;
        if self.reg & 0x10 == 0 {
            v |= self.data_bit; // data pin is an input for the CPU: the device drives it
        } else {
            v |= self.reg & 1;
        }
        v
    }

    pub fn write(&mut self, v: u8) {
        let old = self.reg;
        self.reg = v;
        let (cs_old, cs_new) = (old & 4 != 0, v & 4 != 0);
        if !cs_new {
            if cs_old {
                self.finish();
            }
            self.reset();
            return;
        }
        let rising = old & 2 == 0 && v & 2 != 0;
        if !rising {
            return;
        }
        match self.cmd {
            None => {
                // command phase: 8 bits MSB first, driven by the CPU
                self.in_bits = self.in_bits << 1 | (v & 1);
                self.nbits += 1;
                if self.nbits == 8 {
                    let c = self.in_bits;
                    self.cmd = Some(c);
                    self.nbits = 0;
                    self.out_bit = 0;
                    self.in_data.clear();
                    if c & 1 != 0 {
                        self.out = self.read_reg((c >> 1) & 7);
                    }
                }
            }
            Some(c) if c & 1 != 0 => {
                let (byte, bit) = (self.out_bit / 8, self.out_bit % 8);
                self.data_bit = self.out.get(byte).map_or(0, |b| b >> bit & 1);
                self.out_bit += 1;
            }
            Some(_) => {
                // write phase: data bytes LSB first
                if self.nbits % 8 == 0 {
                    self.in_data.push(0);
                }
                let i = self.in_data.len() - 1;
                self.in_data[i] |= (v & 1) << (self.nbits % 8);
                self.nbits += 1;
            }
        }
    }

    fn reset(&mut self) {
        self.in_bits = 0;
        self.nbits = 0;
        self.cmd = None;
        self.out.clear();
        self.out_bit = 0;
        self.data_bit = 0;
    }

    /// Chip select dropped: a completed write command updates the register it addressed.
    fn finish(&mut self) {
        let Some(c) = self.cmd else { return };
        if c & 1 != 0 || self.in_data.len() < Self::bytes_for((c >> 1) & 7) {
            return;
        }
        let un = |b: u8| (b >> 4) * 10 + (b & 15);
        match (c >> 1) & 7 {
            0 => self.status1 = self.in_data[0],
            1 => self.status2 = self.in_data[0],
            2 => {
                for i in 0..7 {
                    self.datetime[i] = un(self.in_data[i] & if i == 4 { 0x3F } else { 0xFF });
                }
            }
            3 => {
                for i in 0..3 {
                    self.datetime[4 + i] = un(self.in_data[i] & if i == 0 { 0x3F } else { 0xFF });
                }
            }
            _ => {}
        }
    }
}

/// `MKDS_SPI_TRACE=1` prints every card-SPI byte (to see which save chip the game probes for).
fn trace_enabled() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("MKDS_SPI_TRACE").is_some())
}

// ---------------------------------------------------------------------------------------------
// Cartridge backup memory (save data) on the card SPI bus
// ---------------------------------------------------------------------------------------------

/// How the save chip is addressed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BackupKind {
    /// Small EEPROM (4 Kbit .. 64 Kbit): `addr_bytes` = 1 or 2.
    Eeprom { addr_bytes: u8 },
    /// Flash (256 KiB and up): 3 address bytes, page program/erase commands, JEDEC id.
    Flash,
}

pub struct Backup {
    save: Option<crate::save::Save>,
    pub kind: BackupKind,
    pub data: Vec<u8>,
    pub cnt: u16,
    reply: u8,
    step: usize,
    cmd: u8,
    addr: usize,
    write_enabled: bool,
    selected: bool,
}

impl Backup {
    pub fn new(kind: BackupKind, size: usize) -> Backup {
        Backup { save: None, kind, data: vec![0xFF; size], cnt: 0, reply: 0, step: 0, cmd: 0, addr: 0, write_enabled: false, selected: false }
    }

    pub fn attach_save(&mut self, path: std::path::PathBuf, import: Option<&std::path::Path>) -> std::io::Result<()> {
        self.save = Some(crate::save::Save::load(path, import, &mut self.data)?);
        Ok(())
    }

    pub fn changed(&mut self) {
        if let Some(s) = &mut self.save { s.dirty = true; }
    }

    pub fn poll_save(&mut self) {
        if let Some(s) = &mut self.save {
            if let Err(e) = s.poll(&self.data) { eprintln!("[save] write failed: {e}"); }
        }
    }

    pub fn flush_save(&mut self) -> std::io::Result<()> {
        if let Some(s) = &mut self.save { s.flush(&self.data)?; }
        Ok(())
    }

    fn addr_bytes(&self) -> usize {
        match self.kind {
            BackupKind::Eeprom { addr_bytes } => addr_bytes as usize,
            BackupKind::Flash => 3,
        }
    }

    pub fn read_data(&self) -> u8 {
        self.reply
    }

    pub fn write_cnt(&mut self, v: u16) {
        self.cnt = v & 0xE043;
        if v & 0x8000 == 0 {
            self.selected = false;
            self.step = 0;
        }
    }

    pub fn write_data(&mut self, b: u8) {
        if self.cnt & 0x8000 == 0 {
            return;
        }
        let hold = self.cnt & 0x40 != 0;
        self.reply = self.exchange(b);
        self.poll_save();
        if self.cmd == 0x04 {
            if let Err(e) = self.flush_save() { eprintln!("[save] write failed: {e}"); }
        }
        if trace_enabled() {
            eprintln!("backup spi step {} cmd {:#04X} in {:#04X} -> {:#04X}{}", self.step, self.cmd, b, self.reply, if hold { " (hold)" } else { "" });
        }
        self.step += 1;
        if !hold {
            self.step = 0;
            self.selected = false;
        }
    }

    fn exchange(&mut self, b: u8) -> u8 {
        if self.step == 0 {
            self.cmd = b;
            self.addr = 0;
            match b {
                0x06 => self.write_enabled = true,
                0x04 => self.write_enabled = false,
                _ => {}
            }
            return 0;
        }
        let n = self.addr_bytes();
        let size = self.data.len();
        match self.cmd {
            0x03 | 0x0B => {
                if self.step <= n {
                    self.addr = self.addr << 8 | b as usize;
                    0
                } else if self.cmd == 0x0B && self.step == n + 1 {
                    0
                } else {
                    let v = self.data[self.addr % size];
                    self.addr += 1;
                    v
                }
            }
            0x05 => if self.write_enabled { 0x02 } else { 0 }, // WEL; never busy
            0x9F => [0x20, 0x40, 0x12][(self.step - 1).min(2)],
            0x02 | 0x0A => {
                // EEPROM write / flash page write (0x0A erases the page first, 0x02 programs)
                if self.step <= n {
                    self.addr = self.addr << 8 | b as usize;
                } else if self.write_enabled {
                    let a = self.addr % size;
                    self.data[a] = if self.cmd == 0x0A || self.kind != BackupKind::Flash { b } else { self.data[a] & b };
                    self.changed();
                    // wraps within the 256-byte page
                    self.addr = (self.addr & !0xFF) | ((self.addr + 1) & 0xFF);
                }
                0
            }
            0xDB | 0xD8 | 0xC7 if self.step == n => {
                // page / sector / chip erase on the last address byte
                self.addr = self.addr << 8 | b as usize;
                if self.write_enabled {
                    let (len, mask) = match self.cmd {
                        0xDB => (0x100, !0xFFusize),
                        0xD8 => (0x1_0000, !0xFFFFusize),
                        _ => (size, 0),
                    };
                    let start = if self.cmd == 0xC7 { 0 } else { (self.addr & mask) % size };
                    for i in 0..len.min(size - start) {
                        self.data[start + i] = 0xFF;
                    }
                    self.changed();
                }
                0
            }
            0xDB | 0xD8 | 0xC7 => {
                self.addr = self.addr << 8 | b as usize;
                0
            }
            _ => 0,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The set, as the ARM7's I/O sees it
// ---------------------------------------------------------------------------------------------

impl Drop for Backup {
    fn drop(&mut self) {
        if let Err(e) = self.flush_save() { eprintln!("[save] exit write failed: {e}"); }
    }
}

pub struct Peripherals {
    pub spi: Spi,
    pub rtc: Rtc,
    pub backup: Backup,
}

impl Default for Peripherals {
    fn default() -> Self {
        Peripherals::new()
    }
}

impl Peripherals {
    pub fn new() -> Peripherals {
        // 256 KiB flash save; the real size/type is a property of the game's cartridge and is set by the host
        Peripherals { spi: Spi::new(), rtc: Rtc::new(), backup: Backup::new(BackupKind::Flash, 0x4_0000) }
    }

    /// Read the register at ARM7 I/O offset `off` (`None` = not a peripheral register).
    pub fn read(&mut self, off: u32) -> Option<u32> {
        Some(match off {
            0x1A0 => self.backup.cnt as u32,
            0x1A2 => self.backup.read_data() as u32,
            0x138 => self.rtc.read() as u32,
            0x1C0 => self.spi.read_cnt() as u32,
            0x1C2 => self.spi.read_data() as u32,
            _ => return None,
        })
    }

    /// Write the register; returns whether it was a peripheral register.
    pub fn write(&mut self, off: u32, v: u32, n: usize) -> bool {
        match off {
            // AUXSPICNT is the low half of a word that also holds AUXSPIDATA at +2
            0x1A0 => {
                self.backup.write_cnt(v as u16);
                if n == 4 {
                    self.backup.write_data((v >> 16) as u8);
                }
            }
            0x1A2 => self.backup.write_data(v as u8),
            0x138 => self.rtc.write(v as u8),
            0x1C0 => {
                self.spi.write_cnt(v as u16);
                if n == 4 {
                    self.spi.write_data((v >> 16) as u8);
                }
            }
            0x1C2 => self.spi.write_data(v as u8),
            _ => return false,
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spi_xfer(s: &mut Spi, dev: u16, bytes: &[u8]) -> Vec<u8> {
        let mut out = vec![];
        for (i, &b) in bytes.iter().enumerate() {
            let hold = if i + 1 < bytes.len() { 0x800 } else { 0 };
            s.write_cnt(0x8000 | dev << 8 | hold);
            s.write_data(b);
            out.push(s.read_data());
        }
        out
    }

    #[test]
    fn firmware_user_settings_are_valid() {
        let f = synthetic_firmware();
        assert_eq!(u16::from_le_bytes([f[0x20], f[0x21]]) as usize * 8, USER_SETTINGS_OFFSET);
        for i in 0..2 {
            let b = &f[USER_SETTINGS_OFFSET + 0x100 * i..][..0x100];
            assert_eq!(crc16(0xFFFF, &b[..0x70]), u16::from_le_bytes([b[0x72], b[0x73]]));
        }
        // the second block is newer
        let c = |i: usize| u16::from_le_bytes([f[USER_SETTINGS_OFFSET + 0x100 * i + 0x70], f[USER_SETTINGS_OFFSET + 0x100 * i + 0x71]]);
        assert!(c(1) > c(0));
    }

    #[test]
    fn spi_firmware_read_streams_bytes() {
        let mut s = Spi::new();
        let at = USER_SETTINGS_OFFSET + 0x58;
        let r = spi_xfer(&mut s, 1, &[0x03, (at >> 16) as u8, (at >> 8) as u8, at as u8, 0, 0]);
        assert_eq!(&r[4..], &[CAL_ADC1.0 as u8, (CAL_ADC1.0 >> 8) as u8]);
        // jedec id
        assert_eq!(spi_xfer(&mut s, 1, &[0x9F, 0, 0, 0])[1..], [0x20, 0x40, 0x12]);
        // a new transaction starts from the command again
        assert_eq!(spi_xfer(&mut s, 1, &[0x05, 0])[1], 0);
    }

    #[test]
    fn firmware_write_enable_program_erase() {
        let mut s = Spi::new();
        spi_xfer(&mut s, 1, &[0x06]);
        assert_eq!(spi_xfer(&mut s, 1, &[0x05, 0])[1], 2, "WEL set after write enable");
        // page write at 0x100: erases the page then programs 2 bytes
        spi_xfer(&mut s, 1, &[0x0A, 0, 1, 0, 0x12, 0x34]);
        assert_eq!(spi_xfer(&mut s, 1, &[0x05, 0])[1], 0, "WEL cleared after the write");
        assert_eq!(spi_xfer(&mut s, 1, &[0x03, 0, 1, 0, 0, 0, 0])[4..], [0x12, 0x34, 0xFF]);
        // programming without write enable changes nothing
        spi_xfer(&mut s, 1, &[0x02, 0, 1, 0, 0x00]);
        assert_eq!(spi_xfer(&mut s, 1, &[0x03, 0, 1, 0, 0])[4], 0x12);
    }

    #[test]
    fn power_management_registers_round_trip() {
        let mut s = Spi::new();
        spi_xfer(&mut s, 0, &[0x00, 0x0D]); // write reg 0
        assert_eq!(spi_xfer(&mut s, 0, &[0x80, 0])[1], 0x0D);
        assert_eq!(spi_xfer(&mut s, 0, &[0x81, 0])[1], 0, "battery OK");
    }

    #[test]
    fn touch_reports_calibrated_positions() {
        let mut s = Spi::new();
        s.pen = Some(touch_raw(128, 96));
        let (ax, ay) = touch_raw(128, 96);
        let read = |s: &mut Spi, ctl: u8| {
            let r = spi_xfer(s, 2, &[ctl, 0, 0]);
            ((r[1] as u16) << 5) | (r[2] as u16 >> 3)
        };
        assert_eq!(read(&mut s, 0xD0), ax); // 0xD0 = start | channel 5 = X
        assert_eq!(read(&mut s, 0x90), ay); // channel 1 = Y
        s.pen = None;
        assert_eq!(read(&mut s, 0x90), 0xFFF); // Y with the pen up
        // touch_raw maps the calibration corners onto themselves
        assert_eq!(touch_raw(CAL_PX1.0, CAL_PX1.1), CAL_ADC1);
        assert_eq!(touch_raw(CAL_PX2.0, CAL_PX2.1), CAL_ADC2);
    }

    #[test]
    fn touch_conversion_selects_channels_and_pipelines() {
        let mut s = Spi::new();
        s.pen = Some((0x123, 0x456));
        // X (channel 5 -> control 0xD0) then Y (channel 1 -> 0x90) pipelined in the last byte
        let r = spi_xfer(&mut s, 2, &[0xD0, 0, 0x90, 0, 0]);
        assert_eq!(((r[1] as u16) << 5) | (r[2] as u16 >> 3), 0x123);
        assert_eq!(((r[3] as u16) << 5) | (r[4] as u16 >> 3), 0x456);
    }

    fn rtc_bits(r: &mut Rtc, cmd: u8, read_bytes: usize, write: &[u8]) -> Vec<u8> {
        // CS high, direction bits: data+clock+cs outputs (0x70 | ...)
        let port = |r: &mut Rtc, v: u8| r.write(v);
        port(r, 0x74); // CS=1, SCK=0, output
        for i in (0..8).rev() {
            let d = cmd >> i & 1;
            port(r, 0x74 | d);
            port(r, 0x76 | d);
        }
        let mut out = vec![];
        if read_bytes > 0 {
            for _ in 0..read_bytes {
                let mut b = 0u8;
                for bit in 0..8 {
                    port(r, 0x64); // data pin as input, SCK low
                    port(r, 0x66); // rising edge: device presents the bit
                    b |= (r.read() & 1) << bit;
                }
                out.push(b);
            }
        } else {
            for &byte in write {
                for bit in 0..8 {
                    let d = byte >> bit & 1;
                    port(r, 0x74 | d);
                    port(r, 0x76 | d);
                }
            }
        }
        port(r, 0x70); // CS low: end
        out
    }

    #[test]
    fn rtc_reads_bcd_datetime_and_status() {
        let mut r = Rtc::new();
        assert_eq!(rtc_bits(&mut r, 0x65, 7, &[]), vec![0x10, 0x06, 0x15, 0x02, 0x12, 0x00, 0x00]);
        assert_eq!(rtc_bits(&mut r, 0x61, 1, &[]), vec![0x02]);
        assert_eq!(rtc_bits(&mut r, 0x67, 3, &[]), vec![0x12, 0x00, 0x00]);
    }

    #[test]
    fn rtc_write_updates_registers() {
        let mut r = Rtc::new();
        rtc_bits(&mut r, 0x60, 0, &[0x40]); // write status 1
        assert_eq!(r.status1, 0x40);
        rtc_bits(&mut r, 0x64, 0, &[0x24, 0x12, 0x31, 0x03, 0x23, 0x59, 0x58]);
        assert_eq!(r.datetime, [24, 12, 31, 3, 23, 59, 58]);
    }

    fn backup_xfer(b: &mut Backup, bytes: &[u8]) -> Vec<u8> {
        let mut out = vec![];
        for (i, &x) in bytes.iter().enumerate() {
            let hold = if i + 1 < bytes.len() { 0x40 } else { 0 };
            b.write_cnt(0x8000 | 0x2000 | hold);
            b.write_data(x);
            out.push(b.read_data());
        }
        out
    }

    #[test]
    fn flash_program_read_erase() {
        let mut b = Backup::new(BackupKind::Flash, 0x4_0000);
        backup_xfer(&mut b, &[0x06]);
        assert_eq!(backup_xfer(&mut b, &[0x05, 0])[1] & 2, 2, "write enabled");
        backup_xfer(&mut b, &[0x0A, 0, 0x01, 0x00, 0xDE, 0xAD, 0xBE]);
        assert_eq!(backup_xfer(&mut b, &[0x03, 0, 0x01, 0x00, 0, 0, 0])[4..], [0xDE, 0xAD, 0xBE]);
        backup_xfer(&mut b, &[0xDB, 0, 0x01, 0x00]); // page erase (write enable still set)
        assert_eq!(backup_xfer(&mut b, &[0x03, 0, 0x01, 0x00, 0])[4], 0xFF);
    }

    #[test]
    fn eeprom_two_byte_addressing_and_write_enable_gate() {
        let mut b = Backup::new(BackupKind::Eeprom { addr_bytes: 2 }, 0x2000);
        backup_xfer(&mut b, &[0x02, 0x00, 0x10, 0x42]); // not write-enabled: ignored
        assert_eq!(backup_xfer(&mut b, &[0x03, 0x00, 0x10, 0])[3], 0xFF);
        backup_xfer(&mut b, &[0x06]);
        backup_xfer(&mut b, &[0x02, 0x00, 0x10, 0x42]);
        assert_eq!(backup_xfer(&mut b, &[0x03, 0x00, 0x10, 0])[3], 0x42);
    }

    #[test]
    fn registers_dispatch() {
        let mut p = Peripherals::new();
        assert!(p.write(0x1C0, 0x8000 | 1 << 8 | 0x800, 2));
        p.write(0x1C2, 0x9F, 1);
        assert!(p.read(0x1C2).is_some());
        assert_eq!(p.read(0x130), None);
        assert!(!p.write(0x130, 0, 2));
    }
}
