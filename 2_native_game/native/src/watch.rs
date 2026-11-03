//! Diagnostics: `MKDS_WATCH` logs guest memory ranges whenever they change, once per frame.
//! Each comma-separated entry is `[*]0xADDR[+0xOFF]:LEN`: with `*` the word at ADDR is read as a pointer
//! first (`*0x021759A0+0:0x3D4` watches the race configuration wherever it lives).
//! Reading main RAM has no side effects, so watching never changes what the game does.

use recomp::rt::Mem;

struct Range {
    spec: String,
    deref: bool,
    addr: u32,
    off: u32,
    len: u32,
    last: Option<(u32, Vec<u8>)>,
}

pub struct Watch {
    ranges: Vec<Range>,
}

fn num(s: &str) -> Option<u32> {
    let s = s.trim();
    match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        Some(h) => u32::from_str_radix(h, 16).ok(),
        None => s.parse().ok(),
    }
}

impl Watch {
    pub fn from_env() -> Option<Self> {
        let spec = std::env::var("MKDS_WATCH").ok()?;
        let mut ranges = Vec::new();
        for entry in spec.split(',').map(str::trim).filter(|e| !e.is_empty()) {
            let (at, len) = entry.split_once(':')?;
            let deref = at.starts_with('*');
            let at = at.trim_start_matches('*');
            let (addr, off) = match at.split_once('+') {
                Some((a, o)) => (num(a)?, num(o)?),
                None => (num(at)?, 0),
            };
            ranges.push(Range { spec: entry.to_string(), deref, addr, off, len: num(len)?.min(0x10000), last: None });
        }
        eprintln!("[watch] {} ranges", ranges.len());
        Some(Watch { ranges })
    }

    /// Compares every range with the last frame and logs the changed words.
    pub fn check(&mut self, frame: u64, m: &mut impl Mem) {
        for r in &mut self.ranges {
            let base = if r.deref { m.r32(r.addr) } else { r.addr };
            if r.deref && !(0x0200_0000..0x0240_0000).contains(&base) {
                continue;
            }
            let start = base.wrapping_add(r.off);
            let bytes: Vec<u8> = (0..r.len).map(|i| m.r8(start.wrapping_add(i))).collect();
            match &r.last {
                Some((b, old)) if *b == base => {
                    let mut i = 0;
                    while i < bytes.len() {
                        let end = (i + 4).min(bytes.len());
                        if bytes[i..end] != old[i..end] {
                            let w = |v: &[u8]| v.iter().rev().map(|b| format!("{b:02x}")).collect::<String>();
                            eprintln!("[watch] f{frame} {} +{:#x}: {} -> {}", r.spec, r.off as usize + i, w(&old[i..end]), w(&bytes[i..end]));
                        }
                        i = end;
                    }
                }
                Some((b, _)) => eprintln!("[watch] f{frame} {} moved {b:#x} -> {base:#x}", r.spec),
                None => {
                    eprintln!("[watch] f{frame} {} first seen at {start:#x}", r.spec);
                    for (i, w) in bytes.chunks(4).enumerate() {
                        let v = w.iter().rev().fold(0u32, |a, &b| a << 8 | b as u32);
                        if v != 0 {
                            eprintln!("[watch]   +{:#x} = {v:#x}", r.off as usize + i * 4);
                        }
                    }
                }
            }
            r.last = Some((base, bytes));
        }
    }
}

/// Diagnostics: `MKDS_POKE` writes guest memory at given frames, for experiments.
/// Comma-separated `fFRAME:[*]0xADDR[+0xOFF]:BITS:VALUE` (BITS 8, 16 or 32).
pub struct Poke {
    writes: Vec<(u64, bool, u32, u32, u8, u32)>,
}

impl Poke {
    pub fn from_env() -> Option<Self> {
        let spec = std::env::var("MKDS_POKE").ok()?;
        let mut writes = Vec::new();
        for e in spec.split(',').map(str::trim).filter(|e| !e.is_empty()) {
            let p: Vec<&str> = e.split(':').collect();
            if p.len() != 4 {
                eprintln!("[poke] bad entry {e}");
                return None;
            }
            let frame = p[0].trim_start_matches('f').parse().ok()?;
            let deref = p[1].starts_with('*');
            let at = p[1].trim_start_matches('*');
            let (addr, off) = match at.split_once('+') {
                Some((a, o)) => (num(a)?, num(o)?),
                None => (num(at)?, 0),
            };
            writes.push((frame, deref, addr, off, p[2].parse().ok()?, num(p[3])?));
        }
        Some(Poke { writes })
    }

    pub fn apply(&mut self, frame: u64, m: &mut impl Mem) {
        for &(f, deref, addr, off, bits, v) in &self.writes {
            if f != frame {
                continue;
            }
            let a = if deref { m.r32(addr) } else { addr }.wrapping_add(off);
            match bits {
                8 => m.w8(a, v as u8),
                16 => m.w16(a, v as u16),
                _ => m.w32(a, v),
            }
            eprintln!("[poke] f{frame} {a:#x} <- {v:#x} ({bits} bits)");
        }
    }
}
