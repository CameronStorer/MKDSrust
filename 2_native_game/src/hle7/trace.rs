//! PXI traffic recorder (diagnostics, `MKDS_PXI_LOG=<file>`): every IPC FIFO word in both directions, every
//! IPCSYNC write on either side, and a per-address summary of the ARM7's writes to main RAM (`<file>.mainw`).
//! Off by default; when off each hook costs one load of an initialised `OnceLock`. Records only; it never
//! changes what either CPU sees.
//!
//! Line format (hex without `0x`): `<kind> <bus cycle> <value>[ <extra>]`, kinds:
//! `9W` ARM9 → ARM7 FIFO word, `7W` ARM7 → ARM9 FIFO word, `9S` ARM9 IPCSYNC write (bits 8-11 its nibble,
//! 13 IRQ request, 14 IRQ enable), `7S` ARM7 sync nibble + IRQ flag, `7R` ARM7 popped ARM9 word #n.
use std::collections::HashMap;
use std::io::Write;
use std::sync::{Mutex, OnceLock};

struct Log {
    out: std::io::BufWriter<std::fs::File>,
    path: String,
    /// ARM7 main-RAM writes: address → (count, first cycle, last cycle, last value, last ARM7 block pc).
    mainw: HashMap<u32, (u64, u64, u64, u32, u32)>,
    writes: u64,
    lines: u64,
}

static LOG: OnceLock<Option<Mutex<Log>>> = OnceLock::new();

fn log() -> Option<&'static Mutex<Log>> {
    LOG.get_or_init(|| {
        let path = std::env::var("MKDS_PXI_LOG").ok()?;
        let f = std::fs::File::create(&path).ok()?;
        Some(Mutex::new(Log {
            out: std::io::BufWriter::new(f),
            path,
            mainw: HashMap::new(),
            writes: 0,
            lines: 0,
        }))
    })
    .as_ref()
}

/// Whether the recorder is on.
#[inline]
pub fn on() -> bool {
    log().is_some()
}

#[inline]
pub fn event(kind: &str, cycles: u64, v: u32, extra: Option<u32>) {
    let Some(l) = log() else { return };
    let mut l = l.lock().unwrap();
    let _ = match extra {
        Some(e) => writeln!(l.out, "{kind} {cycles:x} {v:x} {e:x}"),
        None => writeln!(l.out, "{kind} {cycles:x} {v:x}"),
    };
    l.lines += 1;
    if l.lines % 4096 == 0 {
        let _ = l.out.flush();
    }
}

/// The ARM7 wrote `n` bytes of `v` at main-RAM address `a` (block `pc`).
#[inline]
pub fn arm7_main_write(cycles: u64, a: u32, v: u32, pc: u32) {
    let Some(l) = log() else { return };
    let mut l = l.lock().unwrap();
    let e = l.mainw.entry(a).or_insert((0, cycles, 0, 0, 0));
    e.0 += 1;
    e.2 = cycles;
    e.3 = v;
    e.4 = pc;
    l.writes += 1;
    if l.writes % (1 << 20) == 0 {
        dump(&mut l);
    }
}

fn dump(l: &mut Log) {
    let _ = l.out.flush();
    let mut rows: Vec<_> = l.mainw.iter().map(|(a, e)| (*a, *e)).collect();
    rows.sort();
    if let Ok(f) = std::fs::File::create(format!("{}.mainw", l.path)) {
        let mut w = std::io::BufWriter::new(f);
        for (a, (n, first, last, v, pc)) in rows {
            let _ = writeln!(w, "{a:08x} {n} {first:x} {last:x} {v:x} {pc:08x}");
        }
    }
}

/// Write everything out now (also done periodically).
pub fn flush() {
    if let Some(l) = log() {
        dump(&mut l.lock().unwrap());
    }
}

/// Main-RAM addresses the ARM7 writes as part of a PXI service reply (logged in full as `7M<n>` lines):
/// everything except the ARM7's load image, the sound driver's shared work area and the ARM7's own
/// background writers (pad X/Y 0x027FFFA8, V-blank count 0x027FFC3C, lock words 0x027FFFB8..0x027FFFFF).
pub fn service_address(a: u32) -> bool {
    !(0x0238_0000..0x0240_0000).contains(&a)
        && !(0x0217_3300..0x0217_3560).contains(&a)
        && a != 0x027F_FFA8
        && a != 0x027F_FC3C
        && !(0x027F_FFB8..0x0280_0000).contains(&a)
}

/// ARM7 code whose main-RAM reads are logged (`7L<n>` lines): the backup (card) and firmware-flash
/// services, which read the ARM9's request blocks and source buffers. With these the message-level
/// replay ([`super::replay`]) has the same inputs as the exact model.
pub fn service_reader(pc: u32) -> bool {
    (0x037F_FF54..0x0380_0F38).contains(&pc) || (0x0380_4338..0x0380_4DD0).contains(&pc)
}
