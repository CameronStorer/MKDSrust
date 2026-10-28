//! Message-level oracle: replays a PXI recording of the exact model ([`super::trace`], `MKDS_PXI_LOG`)
//! into a [`Router`] alone (no ARM7 CPU) and compares, in order, the replies of the claimed tags and the
//! shared-memory bytes each service wrote. Timestamps are only used to move the scanline (touch
//! auto-sampling) and to order events; they are not compared.
use super::Router;
use crate::ds_mem::Bus;
use crate::rt::Mem;

/// One recorded event.
#[derive(Clone, Debug)]
pub enum Event {
    /// ARM9 → ARM7 FIFO word.
    A9(u64, u32),
    /// ARM7 → ARM9 FIFO word.
    A7(u64, u32),
    /// The ARM7 wrote `bytes` bytes of `value` at `addr` in main RAM.
    Write(u64, u32, u32, usize),
    /// The ARM7 read `value` (`bytes` wide) at `addr` (inputs the ARM9 prepared).
    Read(u64, u32, u32, usize),
}

impl Event {
    pub fn cycles(&self) -> u64 {
        match *self {
            Event::A9(c, _) | Event::A7(c, _) | Event::Write(c, ..) | Event::Read(c, ..) => c,
        }
    }
}

pub fn parse(text: &str) -> Vec<Event> {
    let mut out = Vec::new();
    for line in text.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 3 {
            continue;
        }
        let (Ok(c), Ok(v)) = (u64::from_str_radix(f[1], 16), u32::from_str_radix(f[2], 16)) else {
            continue;
        };
        let addr = f.get(3).and_then(|a| u32::from_str_radix(a, 16).ok());
        match (f[0], addr) {
            ("9W", _) => out.push(Event::A9(c, v)),
            ("7W", _) => out.push(Event::A7(c, v)),
            (k, Some(a)) if k.starts_with("7M") => out.push(Event::Write(c, a, v, k[2..].parse().unwrap_or(4))),
            (k, Some(a)) if k.starts_with("7L") => out.push(Event::Read(c, a, v, k[2..].parse().unwrap_or(4))),
            _ => {}
        }
    }
    out
}

/// Result of a replay.
#[derive(Default, Debug)]
pub struct Report {
    /// Claimed-tag replies of the exact model, and how many the router reproduced in order.
    pub expected_replies: usize,
    pub matched_replies: usize,
    /// First per-tag mismatch: (tag, index within the tag, expected, got).
    pub first_reply_mismatch: Option<(u32, usize, Option<u32>, Option<u32>)>,
    /// How many replies match in global order before the first cross-tag interleaving difference.
    pub global_order_prefix: usize,
    /// Service writes checked, and mismatches (address, expected, got) — capped list.
    pub checked_writes: usize,
    pub write_mismatches: Vec<(u32, u32, u32)>,
    pub write_mismatch_count: usize,
    /// Replies per tag (exact model).
    pub per_tag: [usize; 32],
}

/// Replays `events` into a fresh router. `skip_write(addr)` excludes addresses from the memory check
/// (documented differences, e.g. the RTC).
pub fn run(events: &[Event], skip_write: &dyn Fn(u32) -> bool) -> Report {
    let mut hw = Bus::new();
    let mut r = Router::new();
    r.latency = 0;
    r.install(&mut hw);
    r.log = Some(Vec::new());
    let claimed = r.claimed;
    let claimed_word = |w: u32| claimed >> (w & 0x1F) & 1 != 0;
    let mut rep = Report::default();
    let mut expected = Vec::new();
    // Writes of the exact model, checked against the router's memory at the reply that follows them.
    let mut due: Vec<(u32, u32, usize)> = Vec::new();
    let mut queued: Vec<u32> = Vec::new();
    for e in events {
        // move the hardware's scanline to the event's time
        let c = e.cycles();
        if c > hw.cycles {
            hw.advance(c - hw.cycles);
        }
        match *e {
            // run the touch alarms due before this word, then queue it: it is processed at its tag's next
            // reply, after the inputs the exact ARM7 read for it (`Read` events) are in memory
            Event::A9(_, w) if claimed_word(w) => {
                r.catch_up(&mut hw);
                queued.push(w);
            }
            Event::A7(_, w) if claimed_word(w) => {
                // this tag's commands run now (other tags' commands wait for their own replies)
                let t = w & 0x1F;
                hw.hle7_in.extend(queued.iter().copied().filter(|q| q & 0x1F == t));
                queued.retain(|q| q & 0x1F != t);
                r.advance(&mut hw, None);
                rep.per_tag[(w & 0x1F) as usize] += 1;
                expected.push(w);
                // the bytes the exact model wrote before this reply must be in place now; a byte that
                // differs while other tags' commands are still queued may belong to one of them: it is
                // checked again at the next reply
                let mut keep = Vec::new();
                for (a, v, n) in std::mem::take(&mut due) {
                    let want: Vec<u8> = (0..n).map(|i| (v >> (8 * i)) as u8).collect();
                    let got: Vec<u8> = (0..n).map(|i| hw.r8(a + i as u32)).collect();
                    if want == got {
                        rep.checked_writes += n;
                    } else if !queued.is_empty() {
                        keep.push((a, v, n));
                    } else {
                        rep.checked_writes += n;
                        rep.write_mismatch_count += 1;
                        if rep.write_mismatches.len() < 16 {
                            rep.write_mismatches.push((
                                a,
                                v,
                                u32::from_le_bytes([0, 1, 2, 3].map(|i| *got.get(i).unwrap_or(&0))),
                            ));
                        }
                    }
                }
                due = keep;
            }
            Event::Write(_, a, v, n) if !skip_write(a) => due.push((a, v, n)),
            Event::Read(_, a, v, n) => {
                // the ARM9 prepared this input; make it visible to the router
                for i in 0..n {
                    hw.w8(a + i as u32, (v >> (8 * i)) as u8);
                }
            }
            _ => {}
        }
        // keep the FIFO drained (the ARM9 reads replies as they come)
        hw.fifo_in.clear();
    }
    hw.hle7_in.append(&mut queued);
    r.advance(&mut hw, None);
    for (a, v, n) in due {
        rep.checked_writes += n;
        if (0..n).any(|i| hw.r8(a + i as u32) != (v >> (8 * i)) as u8) {
            rep.write_mismatch_count += 1;
            if rep.write_mismatches.len() < 16 {
                rep.write_mismatches.push((a, v, hw.r32(a & !3)));
            }
        }
    }
    let got = r.log.take().unwrap_or_default();
    rep.expected_replies = expected.len();
    rep.global_order_prefix = expected.iter().zip(&got).take_while(|(a, b)| a == b).count();
    // Per tag: the ARM9 handles each tag in its own callback, so a tag's replies must match in order;
    // the interleaving across tags depends on the ARM7's thread latency and is only reported.
    for t in 0..32u32 {
        let e: Vec<u32> = expected.iter().copied().filter(|w| w & 0x1F == t).collect();
        let g: Vec<u32> = got.iter().copied().filter(|w| w & 0x1F == t).collect();
        let same = e.iter().zip(&g).take_while(|(a, b)| a == b).count();
        rep.matched_replies += same;
        if (same < e.len() || g.len() != e.len()) && rep.first_reply_mismatch.is_none() {
            rep.first_reply_mismatch = Some((t, same, e.get(same).copied(), g.get(same).copied()));
        }
    }
    rep
}
