//! Card backup memory (save data) over PXI tag 11 (`fs_pxi_callback`, `card_thread_main`), backed by
//! [`crate::peripherals::Backup`] (the host owns its contents and can load/store them as a `.sav`).
//!
//! Protocol: every ARM9 word carries the error bit. The first word of a request is the command; command 0
//! (INIT) takes a second word, the main-RAM address of the shared request block. For every command the
//! ARM7 clears the block's result word (+0), performs the command with the block's parameters (+0x0C
//! source, +0x10 destination, +0x14 length) and replies `pack(11, 1, err = true)`.
//!
//! Commands: 0 INIT, 1 ACK, 2 IDENTIFY, 3 READ_ID, 4 READ_ROM (not native: unsupported), 5 WRITE_ROM (unsupported), 6 READ_BACKUP,
//! 7 WRITE_BACKUP, 8 PROGRAM_BACKUP, 9 VERIFY_BACKUP, 10 VERIFY_PROGRAM (unsupported), 11 ERASE_SECTOR,
//! 12 (unsupported). Results: 0 success, 1 failure (verify mismatch), 3 unsupported.
use super::{pack, tag, Router};
use crate::ds_mem::Bus;
use crate::peripherals::Peripherals;
use crate::rt::Mem;

pub mod result {
    pub const OK: u32 = 0;
    pub const FAILURE: u32 = 1;
    pub const UNSUPPORTED: u32 = 3;
}

#[derive(Default)]
pub struct Backup {
    /// Main-RAM address of the shared request block (from INIT).
    pub block: u32,
    /// INIT's second word (the block address) is expected next.
    awaiting_block: bool,
    /// Commands run, by number (diagnostics).
    pub commands: [u64; 16],
}

pub fn receive(r: &mut Router, hw: &mut Bus, p: &mut Peripherals, w: u32, data: u32, err: bool) {
    if !err {
        return; // the ARM7 ignores words without the error bit on this tag
    }
    if r.backup.awaiting_block {
        r.backup.awaiting_block = false;
        r.backup.block = data;
        run(r, hw, p, 0);
        return;
    }
    if data == 0 {
        r.backup.awaiting_block = true;
        return;
    }
    if r.backup.block == 0 {
        return r.forward(hw, w); // never initialised: leave it to the CPU
    }
    run(r, hw, p, data);
}

fn run(r: &mut Router, hw: &mut Bus, p: &mut Peripherals, cmd: u32) {
    let block = r.backup.block;
    r.backup.commands[(cmd & 15) as usize] += 1;
    hw.w32(block, result::OK);
    let (src, dst, len) = (hw.r32(block + 0xC), hw.r32(block + 0x10), hw.r32(block + 0x14));
    let save = &mut p.backup.data;
    let size = save.len().max(1);
    let res = match cmd {
        0 | 1 => result::OK,
        // IDENTIFY: the chip answers (the type and size come from the ARM9's block); READ_ID: card ROM id
        2 => result::OK,
        3 => {
            let id = hw.card.as_ref().map_or(0, |c| c.chip_id);
            hw.w32(block + 8, id);
            result::OK
        }
        6 => {
            for i in 0..len {
                hw.w8(dst.wrapping_add(i), save[(src.wrapping_add(i)) as usize % size]);
            }
            result::OK
        }
        // write (flash page write: erase + write) / program (flash program: bits can only clear)
        7 | 8 => {
            for i in 0..len {
                let b = hw.r8(src.wrapping_add(i));
                let a = dst.wrapping_add(i) as usize % size;
                save[a] = if cmd == 8 { save[a] & b } else { b };
            }
            result::OK
        }
        // verify: backup at +0x10 against RAM at +0x0C
        9 => {
            let same = (0..len).all(|i| save[dst.wrapping_add(i) as usize % size] == hw.r8(src.wrapping_add(i)));
            if same {
                result::OK
            } else {
                result::FAILURE
            }
        }
        // erase the 64 KiB sector holding +0x10
        11 => {
            let start = (dst as usize & !0xFFFF) % size;
            let end = (start + 0x1_0000).min(save.len());
            save[start..end].fill(0xFF);
            result::OK
        }
        _ => result::UNSUPPORTED,
    };
    if res != result::OK {
        hw.w32(block, res);
    }
    r.reply(pack(tag::FS, 1, true));
}
