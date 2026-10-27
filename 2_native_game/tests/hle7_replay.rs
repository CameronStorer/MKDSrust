//! Message-level oracle for the native ARM7 services (Block 5.2a): replays a PXI recording of the exact
//! model into `hle7::Router` alone and requires the same replies in the same order and the same
//! service shared-memory writes. The recording comes from the user's ROM, so it is not in the repo:
//!
//!     MKDS_PXI_LOG=pxi.log acceptance.exe rom.nds route.csv out 15000      (exact model, the default)
//!     MKDS_PXI_REPLAY=pxi.log cargo test --release --test hle7_replay -- --nocapture
//!
//! Without `MKDS_PXI_REPLAY` the test passes trivially.
use recomp::hle7::{replay, shared};

/// Addresses excluded from the memory comparison, each with its reason.
fn skip(a: u32) -> bool {
    // ARM7 start-up writes, not service replies (the CPU still makes them in native mode): its load image,
    // the user settings it copies from the firmware, its OS arena bounds, its PXI ready mask, its thread word
    (0x027E_0000..0x027F_FC30).contains(&a)
        || (0x027F_FC80..0x027F_FDE8).contains(&a)
        || a == 0x027F_FF8C
        || (0x027F_FFA4..0x027F_FFA8).contains(&a)
        // RTC: the exact model's bit-banged serial RTC (`peripherals::Rtc`) reads back a garbled date and
        // time; the native service publishes the clock's fixed value (documented difference)
        || (shared::RTC_DATE - 8..shared::RTC_TIME + 4).contains(&a)
}

#[test]
fn exact_model_pxi_traffic_replays_identically() {
    let Ok(path) = std::env::var("MKDS_PXI_REPLAY") else {
        eprintln!("MKDS_PXI_REPLAY not set: skipped");
        return;
    };
    let events = replay::parse(&std::fs::read_to_string(&path).expect("PXI log"));
    let rep = replay::run(&events, &skip);
    eprintln!("{rep:#x?}");
    assert!(rep.expected_replies > 0, "no claimed-tag replies in the recording");
    assert_eq!(rep.first_reply_mismatch, None);
    assert_eq!(rep.matched_replies, rep.expected_replies);
    assert_eq!(rep.write_mismatch_count, 0, "{:x?}", rep.write_mismatches);
}
