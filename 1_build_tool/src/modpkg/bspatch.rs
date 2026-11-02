//! bsdiff 4.x (`BSDIFF40`) patch application with the same results as `qbsdiff::Bspatch`, in pure Rust (bzip2 via
//! libbz2-rs-sys). The game links only this; making patches (`qbsdiff::Bsdiff`, whose suffix sorting is C) is behind
//! the builder's `diff` feature, so a game build needs no C compiler.
use bzip2::read::BzDecoder;
use std::io::{Error, ErrorKind, Read, Result, Write};

const CHUNK: u64 = 128 * 1024;

pub struct Bspatch<'p> {
    tsize: u64,
    ctrl: &'p [u8],
    delta: &'p [u8],
    extra: &'p [u8],
}

/// bsdiff's sign-magnitude little-endian integer.
fn int(b: &[u8]) -> i64 {
    let x = u64::from_le_bytes(b[..8].try_into().unwrap());
    if x >> 63 == 0 || x == 1 << 63 { x as i64 } else { ((x & !(1 << 63)) as i64).wrapping_neg() }
}

fn bad(msg: &str) -> Error {
    Error::new(ErrorKind::InvalidData, msg)
}

/// Fills `buf` completely, or reads nothing at end of stream.
fn read_record(r: &mut impl Read, buf: &mut [u8]) -> Result<bool> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    match n {
        0 => Ok(false),
        n if n == buf.len() => Ok(true),
        _ => Err(Error::new(ErrorKind::UnexpectedEof, "truncated patch control block")),
    }
}

impl<'p> Bspatch<'p> {
    pub fn new(patch: &'p [u8]) -> Result<Self> {
        if patch.len() < 32 || &patch[..8] != b"BSDIFF40" {
            return Err(bad("not a valid patch"));
        }
        let (c, d) = (int(&patch[8..]) as u64, int(&patch[16..]) as u64);
        let end = c.checked_add(d).and_then(|n| n.checked_add(32));
        if end.is_none_or(|end| end > patch.len() as u64) {
            return Err(bad("patch corrupted"));
        }
        let (ctrl, rest) = patch[32..].split_at(c as usize);
        let (delta, extra) = rest.split_at(d as usize);
        Ok(Bspatch { tsize: int(&patch[24..]) as u64, ctrl, delta, extra })
    }

    /// Target size recorded in the header (not trusted; callers bound it).
    pub fn hint_target_size(&self) -> u64 {
        self.tsize
    }

    /// Writes the patched `source` to `target` and returns the number of bytes written.
    pub fn apply<W: Write>(self, source: &[u8], mut target: W) -> Result<u64> {
        let mut ctrl = BzDecoder::new(self.ctrl);
        let mut delta = BzDecoder::new(self.delta);
        let mut extra = BzDecoder::new(self.extra);
        let (mut pos, mut total, mut rec, mut buf) = (0u64, 0u64, [0u8; 24], Vec::new());
        let eof = || Error::new(ErrorKind::UnexpectedEof, "patch reads past the end of its source or data");
        while read_record(&mut ctrl, &mut rec)? {
            let (mut add, copy, seek) = (int(&rec[0..]) as u64, int(&rec[8..]) as u64, int(&rec[16..]));
            while add > 0 {
                let k = add.min(CHUNK) as usize;
                let from = usize::try_from(pos).ok().filter(|&p| p <= source.len() && source.len() - p >= k).ok_or_else(eof)?;
                buf.resize(k, 0);
                delta.read_exact(&mut buf)?;
                buf.iter_mut().zip(&source[from..from + k]).for_each(|(b, s)| *b = b.wrapping_add(*s));
                target.write_all(&buf)?;
                (pos, total, add) = (pos + k as u64, total + k as u64, add - k as u64);
            }
            if std::io::copy(&mut (&mut extra).take(copy), &mut target)? != copy {
                return Err(eof());
            }
            total += copy;
            pos = pos
                .checked_add_signed(seek)
                .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "patch seeks before the start of its source"))?;
        }
        target.flush()?;
        Ok(total)
    }
}

#[cfg(all(test, feature = "diff"))]
mod tests {
    use super::Bspatch;
    #[test]
    fn matches_qbsdiff_on_edits_inserts_and_deletes() {
        let mut seed = 7u32;
        let mut rnd = || { seed = seed.wrapping_mul(1664525).wrapping_add(1013904223); (seed >> 24) as u8 };
        let source: Vec<u8> = (0..300_000).map(|i| if i % 7 == 0 { rnd() } else { (i % 251) as u8 }).collect();
        let mut target = source.clone();
        target[10] ^= 1;
        target.splice(50_000..50_000, (0..9000).map(|_| rnd()));
        target.drain(200_000..230_000);
        target.extend((0..200_000).map(|_| rnd()));
        for (s, t) in [(&source[..], &target[..]), (&target[..], &source[..]), (&[][..], &target[..1000]), (&source[..], &[][..])] {
            let mut patch = Vec::new();
            qbsdiff::Bsdiff::new(s, t).compare(&mut patch).unwrap();
            let (mut ours, mut theirs) = (Vec::new(), Vec::new());
            let n = Bspatch::new(&patch).unwrap().apply(s, &mut ours).unwrap();
            qbsdiff::Bspatch::new(&patch).unwrap().apply(s, &mut theirs).unwrap();
            assert_eq!((ours.as_slice(), n), (t, t.len() as u64));
            assert_eq!(ours, theirs);
            assert_eq!(Bspatch::new(&patch).unwrap().hint_target_size(), t.len() as u64);
            // Wrong or truncated input is an error, never a panic.
            let _ = Bspatch::new(&patch).unwrap().apply(&s[..s.len() / 2], &mut Vec::new());
            if let Ok(p) = Bspatch::new(&patch[..patch.len() - 1]) {
                let _ = p.apply(s, &mut Vec::new());
            }
        }
        assert!(Bspatch::new(b"BSDIFF40").is_err());
        let mut huge = b"BSDIFF40".to_vec();
        huge.extend([0xff; 24]);
        huge[15] = 0x7f;
        assert!(Bspatch::new(&huge).is_err());
    }
}
