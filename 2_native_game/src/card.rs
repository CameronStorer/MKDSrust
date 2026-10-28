//! The game card on the ARM9's cartridge bus (hardware behaviour, data from the user's ROM): ROMCTRL
//! (0x040001A4), the 8-byte command (0x040001A8, first byte first) and the data port (0x04100010).
//!
//! The card is already in its main-data mode (the console's boot code did the key exchanges), so the
//! commands the game sends are `0xB7` (read: 4-byte big-endian address, data within the 4 KB page;
//! addresses below 0x8000 read from 0x8000 + (address & 0x1FF), as the secure area is protected) and
//! `0xB8` (chip ID). The ROM image is the decrypted one the builder extracted from the user's file.
use std::collections::VecDeque;

pub struct Card {
    pub rom: Vec<u8>,
    pub chip_id: u32,
    pub cmd: [u8; 8],
    /// ROMCTRL as last written (bits 31 busy and 23 ready are live).
    pub ctrl: u32,
    buf: VecDeque<u32>,
    /// Commands seen (first byte), for diagnostics.
    pub reads: u64,
}

/// Card transfer completion interrupt (ARM9 IF bit 19).
pub const IRQ_CARD_DONE: u32 = 1 << 19;

impl Card {
    /// `chip_id`: the value the console reports for this cartridge (0x00001FC2 for the reference card).
    pub fn new(rom: Vec<u8>, chip_id: u32) -> Card {
        if crate::native::fs::enabled() {
            // `MKDS_HW_FS=native`: ROM-archive reads come straight from this image
            crate::native::fs::attach_rom(&rom);
        }
        Card { rom, chip_id, cmd: [0; 8], ctrl: 0, buf: VecDeque::new(), reads: 0 }
    }

    /// Bytes a command transfers, from ROMCTRL bits 24-26.
    pub fn block_len(ctrl: u32) -> usize {
        match (ctrl >> 24) & 7 {
            0 => 0,
            7 => 4,
            n => 0x100 << n,
        }
    }

    /// A ROMCTRL write: with bit 31 the command in `cmd` runs and its data becomes readable.
    pub fn write_ctrl(&mut self, v: u32) {
        self.ctrl = v & !(1 << 23);
        if v & (1 << 31) == 0 {
            return;
        }
        let len = Self::block_len(v);
        self.buf.clear();
        match self.cmd[0] {
            0xB7 => {
                self.reads += 1;
                let mut addr = u32::from_be_bytes([self.cmd[1], self.cmd[2], self.cmd[3], self.cmd[4]]) as usize;
                if addr < 0x8000 {
                    addr = 0x8000 + (addr & 0x1FF);
                }
                for w in 0..len / 4 {
                    let mut b = [0u8; 4];
                    for (i, x) in b.iter_mut().enumerate() {
                        let a = (addr & !0xFFF) | ((addr + 4 * w + i) & 0xFFF);
                        *x = self.rom.get(a % self.rom.len().max(1)).copied().unwrap_or(0xFF);
                    }
                    self.buf.push_back(u32::from_le_bytes(b));
                }
            }
            0xB8 | 0x90 => self.buf.extend(std::iter::repeat_n(self.chip_id, len / 4)),
            _ => self.buf.extend(std::iter::repeat_n(u32::MAX, len / 4)),
        }
        if self.buf.is_empty() {
            self.ctrl &= !(1 << 31);
        }
    }

    pub fn has_data(&self) -> bool {
        !self.buf.is_empty()
    }

    /// ROMCTRL as read: busy while data remains, word ready while a word is waiting.
    pub fn read_ctrl(&self) -> u32 {
        let live = if self.buf.is_empty() { 0 } else { 1 << 31 | 1 << 23 };
        self.ctrl & !(1 << 31 | 1 << 23) | live
    }

    /// The next data word; `true` with it when that was the last one (the transfer is complete).
    pub fn read_data(&mut self) -> (u32, bool) {
        match self.buf.pop_front() {
            Some(w) => {
                let done = self.buf.is_empty();
                if done {
                    self.ctrl &= !(1 << 31);
                }
                (w, done)
            }
            None => (u32::MAX, false),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn b7_reads_a_block_and_finishes() {
        let rom: Vec<u8> = (0..0x10000u32).map(|i| i as u8).collect();
        let mut c = Card::new(rom, 0x1FC2);
        c.cmd = [0xB7, 0, 0, 0x90, 0x04, 0, 0, 0];
        c.write_ctrl(0xA1 << 24); // start, 0x200 bytes
        assert_eq!(c.read_ctrl() & (1 << 23 | 1 << 31), 1 << 23 | 1 << 31);
        let (w, _) = c.read_data();
        assert_eq!(w, u32::from_le_bytes([4, 5, 6, 7]));
        for _ in 1..0x7F {
            c.read_data();
        }
        let (_, done) = c.read_data();
        assert!(done && c.read_ctrl() & (1 << 31) == 0);
        // the secure area is redirected
        c.cmd = [0xB7, 0, 0, 0, 0x10, 0, 0, 0];
        c.write_ctrl(0xA7 << 24);
        assert_eq!(c.read_data().0, u32::from_le_bytes([0x10, 0x11, 0x12, 0x13]));
    }
}
