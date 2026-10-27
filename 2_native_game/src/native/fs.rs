//! Block 5.1: files from the user's ROM without the card hardware (`MKDS_HW_FS=native`; the default
//! `exact` keeps the card emulation of `card.rs` and the SDK's card driver).
//!
//! Boundary: the read callback the FS library's ROM archive is attached with (`FSi_ReadRomCallback`
//! in the dictionary). Every ROM-archive read ends there: `FS_ReadFile` / `FS_ReadFileAsync` → the
//! read worker → `FSi_SendCommand` → `FSi_TranslateCommand` → `FSi_ReadFileCommand` (which advances
//! the file position first) → the archive's read callback `(archive, dst, rom_offset, bytes)`. The
//! original starts an asynchronous card read (`CARDi_ReadRom`, DMA + card IRQ, completion callback
//! `FSi_OnRomReadDone` → `FS_NotifyArchiveAsyncEnd`) and returns `FS_RESULT_PROC_ASYNC` (6).
//!
//! The native copies the bytes from the in-memory ROM at once and returns `FS_RESULT_SUCCESS` (0),
//! the way the library's own memory-archive callbacks do. `FSi_TranslateCommand` then completes the
//! command in place (result stored, waiter not needed, next queued command started), so a blocking
//! read returns the same byte count with the same `FSFile` state, and an async read is already done
//! when `FS_WaitAsync` is called. The archive's lock/unlock commands (`FSi_RomArchiveProc`, card bus
//! lock) still run as before.
//!
//! The destination is written through the bus, so overlay code loaded this way bumps the code
//! watch (`ds_mem::Bus::code_epoch`) exactly like a card DMA: the slice ends ("code written") and the
//! session re-verifies residency before dispatching the new code.
use std::sync::{Arc, Mutex, OnceLock};

use super::{enter, ret, Native};
use crate::rt::{self, Cpu};

/// `MKDS_HW_FS=native` (read once per process).
pub fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("MKDS_HW_FS").as_deref() == Ok("native"))
}

/// The ROM the native reads come from: the card's image, attached by [`crate::card::Card::new`]
/// while [`enabled`]. Natives only see the guest bus, so the image is kept here.
static ROM: Mutex<Option<Arc<[u8]>>> = Mutex::new(None);

pub fn attach_rom(rom: &[u8]) {
    *ROM.lock().unwrap() = Some(Arc::from(rom));
}

fn rom() -> Option<Arc<[u8]>> {
    ROM.lock().unwrap().clone()
}

/// The byte the card delivers for ROM offset `o` (`card::Card` command 0xB7): offsets below 0x8000
/// (the protected secure area) read from 0x8000 + (o & 0x1FF), offsets past the end wrap.
pub fn rom_byte(rom: &[u8], o: u32) -> u8 {
    let mut a = o as usize;
    if a < 0x8000 {
        a = 0x8000 + (a & 0x1FF);
    }
    rom.get(a % rom.len().max(1)).copied().unwrap_or(0xFF)
}

/// Copies `len` ROM bytes from `src` to guest `dst`: words where both line up (the card DMA's
/// width; VRAM ignores byte writes), bytes at unaligned edges.
pub fn copy_from_rom(m: &mut dyn rt::Bus, rom: &[u8], dst: u32, src: u32, len: u32) {
    let mut i = 0u32;
    while i < len {
        let (d, s) = (dst.wrapping_add(i), src.wrapping_add(i));
        if d & 3 == 0 && len - i >= 4 {
            let b = [rom_byte(rom, s), rom_byte(rom, s + 1), rom_byte(rom, s + 2), rom_byte(rom, s + 3)];
            m.w32(d, u32::from_le_bytes(b));
            i += 4;
        } else {
            m.w8(d, rom_byte(rom, s));
            i += 1;
        }
    }
}

/// `FS_RESULT_SUCCESS`: the command is complete.
const FS_RESULT_SUCCESS: u32 = 0;

/// `FSi_ReadRomCallback(archive r0, dst r1, rom offset r2, bytes r3) -> FSResult`.
pub fn read_rom_callback_block(c: &mut Cpu, m: &mut dyn rt::Bus, pc: u32) -> u32 {
    // the original's own instructions (13 + 4); the card transfer itself takes no time
    if !enter(c, pc, 17) {
        return rt::BUDGET_EXHAUSTED;
    }
    let (dst, src, len) = (c.r[1], c.r[2], c.r[3]);
    match rom() {
        Some(rom) => copy_from_rom(m, &rom, dst, src, len),
        None => panic!("MKDS_HW_FS=native: no ROM attached (the card was not created)"),
    }
    c.r[0] = FS_RESULT_SUCCESS;
    ret(c)
}

/// The FS natives installed with `MKDS_HW_FS=native` (beside [`super::selected`]'s set).
pub static NATIVES: &[Native] = &[
    Native { segment: "arm9_main", entry: 0x0200_5064, name: "FSi_ReadRomCallback", f: read_rom_callback_block },
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::card::Card;
    use crate::rt::Mem;

    /// Plain guest memory for the tests: `data` at `base`.
    struct Buf {
        base: u32,
        data: Vec<u8>,
    }
    impl Mem for Buf {
        fn r8(&mut self, a: u32) -> u8 {
            self.data[(a - self.base) as usize]
        }
        fn r16(&mut self, a: u32) -> u16 {
            u16::from_le_bytes([self.r8(a), self.r8(a + 1)])
        }
        fn r32(&mut self, a: u32) -> u32 {
            u32::from_le_bytes([self.r8(a), self.r8(a + 1), self.r8(a + 2), self.r8(a + 3)])
        }
        fn w8(&mut self, a: u32, v: u8) {
            self.data[(a - self.base) as usize] = v;
        }
        fn w16(&mut self, a: u32, v: u16) {
            for (i, b) in v.to_le_bytes().into_iter().enumerate() {
                self.w8(a + i as u32, b);
            }
        }
        fn w32(&mut self, a: u32, v: u32) {
            assert_eq!(a & 3, 0, "word writes are aligned");
            for (i, b) in v.to_le_bytes().into_iter().enumerate() {
                self.w8(a + i as u32, b);
            }
        }
    }
    impl rt::Bus for Buf {
        fn call(&mut self, _: &mut Cpu, _: u32, _: u32) -> u32 {
            unreachable!("a leaf")
        }
        fn swi(&mut self, _: &mut Cpu, _: u32) {
            unreachable!("a leaf")
        }
        fn mrc(&mut self, _: &mut Cpu, _: u8, _: u8, _: u8, _: u8, _: u8) -> u32 {
            unreachable!("a leaf")
        }
        fn mcr(&mut self, _: &mut Cpu, _: u8, _: u8, _: u8, _: u8, _: u8, _: u32) {
            unreachable!("a leaf")
        }
    }

    /// What the SDK card driver receives for `len` bytes at `src`: whole 0x200-byte pages read with
    /// command 0xB7 through the data port, the wanted bytes cut out.
    fn card_read(card: &mut Card, src: u32, len: u32) -> Vec<u8> {
        let mut out = Vec::with_capacity(len as usize);
        let mut page = src & !0x1FF;
        while page < src + len {
            let a = page.to_be_bytes();
            card.cmd = [0xB7, a[0], a[1], a[2], a[3], 0, 0, 0];
            card.write_ctrl(0xA1 << 24); // start, 0x200 bytes
            for w in 0..0x80u32 {
                let (v, _) = card.read_data();
                for (i, b) in v.to_le_bytes().into_iter().enumerate() {
                    let o = page + 4 * w + i as u32;
                    if o >= src && o < src + len {
                        out.push(b);
                    }
                }
            }
            page += 0x200;
        }
        out
    }

    #[test]
    fn every_fat_entry_reads_like_the_card() {
        let Some(path) = std::env::var_os("MKDS_ROM") else {
            eprintln!("MKDS_ROM not set: skipped");
            return;
        };
        let rom = std::fs::read(path).expect("MKDS_ROM");
        let word = |o: usize| u32::from_le_bytes(rom[o..o + 4].try_into().unwrap());
        let (fat, fat_len) = (word(0x48) as usize, word(0x4C) as usize);
        let mut card = Card::new(rom.clone(), 0x1FC2);
        let base = 0x0200_0000u32;
        let mut ram = Buf { base, data: vec![0; 0x20_0004] };
        let (mut files, mut bytes) = (0, 0u64);
        for e in 0..fat_len / 8 {
            let (start, end) = (word(fat + 8 * e), word(fat + 8 * e + 4));
            if end <= start {
                continue;
            }
            // large files: compare in pieces that fit the scratch RAM, at a misaligned destination
            // for every other entry (the byte path)
            let mut off = start;
            while off < end {
                let n = (end - off).min(0x20_0000);
                let dst = base + (e as u32 & 1);
                copy_from_rom(&mut ram, &rom, dst, off, n);
                let o = (dst - base) as usize;
                let native = &ram.data[o..o + n as usize];
                assert!(*native == card_read(&mut card, off, n), "file {e}: bytes {off:#x}..{:#x} differ", off + n);
                off += n;
                bytes += n as u64;
            }
            files += 1;
        }
        assert!(files > 0);
        eprintln!("{files} FAT entries ({bytes} bytes) equal");
    }

    #[test]
    fn callback_returns_success_and_copies() {
        let rom: Vec<u8> = (0..0x10000u32).map(|i| (i * 7) as u8).collect();
        attach_rom(&rom);
        let mut ram = Buf { base: 0x0200_0000, data: vec![0; 0x1000] };
        let mut c = Cpu { budget: 100, ..Cpu::default() };
        c.r[0] = 0x0216_0000; // archive (unused)
        c.r[1] = 0x0200_0102;
        c.r[2] = 0x9001;
        c.r[3] = 0x45;
        c.r[14] = 0x0200_4000;
        assert_eq!(read_rom_callback_block(&mut c, &mut ram, 0x0200_5064), 0x0200_4000);
        assert_eq!(c.r[0], 0);
        for i in 0..0x45 {
            assert_eq!(ram.r8(0x0200_0102 + i), rom[0x9001 + i as usize]);
        }
    }
}
