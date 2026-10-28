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
    if matches!(cmd, 7 | 8 | 11) && res == result::OK {
        p.backup.changed();
        p.backup.poll_save();
    }
    if res != result::OK {
        hw.w32(block, res);
    }
    r.reply(pack(tag::FS, 1, true));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backup_service_persists_and_reloads_flat_image() {
        let dir = std::env::temp_dir().join(format!("mkdsrust-save-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("service.sav");
        let _ = std::fs::remove_file(&path);
        let mut r = Router::new();
        let mut hw = Bus::new();
        let mut p = Peripherals::new();
        p.backup.attach_save(path.clone(), None).unwrap();
        r.backup.block = 0x02000100;
        hw.w32(r.backup.block + 0xc, 0x02000200);
        hw.w32(r.backup.block + 0x10, 0x123);
        hw.w32(r.backup.block + 0x14, 3);
        for (i, b) in [0x12, 0x34, 0x56].into_iter().enumerate() { hw.w8(0x02000200 + i as u32, b); }
        run(&mut r, &mut hw, &mut p, 7);
        drop(p); // exit flush, including writes within the debounce interval
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 0x40000);
        let mut p = Peripherals::new();
        p.backup.attach_save(path.clone(), None).unwrap();
        hw.w32(r.backup.block + 0xc, 0x123);
        hw.w32(r.backup.block + 0x10, 0x02000300);
        run(&mut r, &mut hw, &mut p, 6);
        assert_eq!([hw.r8(0x02000300), hw.r8(0x02000301), hw.r8(0x02000302)], [0x12, 0x34, 0x56]);
        // Exact-mode SPI writes share the same image and atomic replacement.
        p.backup.write_cnt(0x8000);
        p.backup.write_data(0x06);
        for b in [0x0a, 0, 1, 0x23, 0xab] {
            p.backup.write_cnt(0x8040);
            p.backup.write_data(b);
        }
        p.backup.write_cnt(0);
        p.backup.write_cnt(0x8000);
        p.backup.write_data(0x04);
        assert_eq!(std::fs::read(&path).unwrap()[0x123], 0xab);
        drop(p);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
