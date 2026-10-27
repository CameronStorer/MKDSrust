//! Block 4A contract: the IR printer reproduces the legacy string emitter exactly (up to
//! whitespace) for every instruction encoding tried, so generated T0 code is unchanged.
use recomp::arm::{self, Decoded, Inst};
use recomp::ir;
use recomp::lift::{legacy_body, Image};

/// Compares both emitters on one decoded instruction; returns a description on mismatch.
fn check(d: &Decoded, addr: u32, img: Option<&Image>) -> Option<String> {
    if matches!(d.inst, Inst::Undecoded(_)) {
        return None;
    }
    let next = addr.wrapping_add(d.size);
    let old = legacy_body(d, addr, next, img);
    let new = ir::print(&ir::lower(d, addr, next, img));
    (ir::normalize(&old) != ir::normalize(&new)).then(|| format!("{addr:#X} {:?}\n  legacy: {old}\n  ir:     {new}", d.inst))
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 16) as u32
    }
}

fn report(bad: Vec<String>, tried: usize) {
    assert!(bad.is_empty(), "{} of {tried} instructions differ; first:\n{}", bad.len(), bad[..bad.len().min(10)].join("\n"));
}

#[test]
fn arm_random_words_print_identically() {
    let base = 0x0200_0000u32;
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    // a random image so pc-relative loads fold literal words
    let bytes: Vec<u8> = (0..0x2000).map(|_| rng.next() as u8).collect();
    let img = Image::new(base, &bytes);
    let mut bad = Vec::new();
    let n = 400_000;
    for i in 0..n {
        let mut w = rng.next();
        if i % 2 == 0 {
            w = (w & 0x0FFF_FFFF) | 0xE000_0000; // mostly unconditional, as real code is
        }
        let addr = base + (rng.next() % 0x1000) * 4;
        let d = arm::decode(w, addr);
        if let Some(m) = check(&d, addr, if i % 3 == 0 { None } else { Some(&img) }) {
            bad.push(m);
        }
    }
    report(bad, n);
}

#[test]
fn every_thumb_halfword_prints_identically() {
    let base = 0x0200_0000u32;
    let mut rng = Rng(0xD1B5_4A32_D192_ED03);
    let bytes: Vec<u8> = (0..0x2000).map(|_| rng.next() as u8).collect();
    let img = Image::new(base, &bytes);
    let mut bad = Vec::new();
    let mut tried = 0;
    for h in 0..=u16::MAX {
        for k in 0..4 {
            let addr = base + (rng.next() % 0x1000) * 2;
            let second = rng.next() as u16;
            let d = recomp::thumb::decode(h, second, addr);
            tried += 1;
            if let Some(m) = check(&d, addr, if k == 0 { None } else { Some(&img) }) {
                bad.push(m);
            }
        }
    }
    report(bad, tried);
}

/// Real ROM (`MKDS_ROM`): every word and halfword of the ARM9 and ARM7 binaries, decoded as ARM
/// and as Thumb at their load addresses (a superset of the code the generator lifts).
#[test]
fn rom_binaries_print_identically() {
    let Ok(path) = std::env::var("MKDS_ROM") else {
        eprintln!("MKDS_ROM not set: skipped");
        return;
    };
    let rom = std::fs::read(path).unwrap();
    let rd = |o: usize| u32::from_le_bytes(rom[o..o + 4].try_into().unwrap());
    let mut bad = Vec::new();
    let mut tried = 0;
    for hdr in [0x20usize, 0x30] {
        let (off, ram, size) = (rd(hdr) as usize, rd(hdr + 8), rd(hdr + 12) as usize);
        let code = &rom[off..off + size];
        let img = Image::new(ram, code);
        for o in (0..size.saturating_sub(3)).step_by(2) {
            let addr = ram + o as u32;
            let h = u16::from_le_bytes([code[o], code[o + 1]]);
            let h2 = u16::from_le_bytes([code[o + 2], code[o + 3]]);
            let mut ds = vec![recomp::thumb::decode(h, h2, addr)];
            if o % 4 == 0 {
                ds.push(arm::decode(rd(off + o), addr));
            }
            for d in ds {
                tried += 1;
                if let Some(m) = check(&d, addr, Some(&img)) {
                    bad.push(m);
                }
            }
        }
    }
    eprintln!("{tried} ROM instruction encodings compared");
    report(bad, tried);
}
