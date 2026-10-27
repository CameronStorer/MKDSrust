//! Synthetic unit tests for the native ARM7 services (no game data).
use super::*;

/// An SPI-family packet: words for halfwords `h` (start on the first, end on the last).
fn packet(t: u32, h: &[u16]) -> Vec<u32> {
    let n = h.len();
    h.iter()
        .enumerate()
        .map(|(i, &v)| {
            let mut d = (i as u32) << 16 | v as u32;
            if i == 0 {
                d |= 0x200_0000;
            }
            if i == n - 1 {
                d |= 0x100_0000;
            }
            pack(t, d, false)
        })
        .collect()
}

fn send(r: &mut Router, hw: &mut Bus, words: &[u32]) -> Vec<u32> {
    hw.hle7_in.extend_from_slice(words);
    r.advance(hw, None);
    hw.fifo_in.drain(..).collect()
}

fn setup() -> (Router, Bus) {
    let mut hw = Bus::new();
    hw.w16(crate::ds_mem::IO_BASE + 0x184, 0x8400);
    let mut r = Router::new();
    r.latency = 0;
    r.install(&mut hw);
    (r, hw)
}

#[test]
fn claimed_words_bypass_the_cpu_and_sound_does_not() {
    let (_, mut hw) = setup();
    hw.w32(crate::ds_mem::IO_BASE + 0x188, pack(tag::TOUCH, 0x123, false));
    hw.w32(crate::ds_mem::IO_BASE + 0x188, pack(tag::SOUND, 0x456, false));
    assert_eq!(hw.hle7_in, vec![pack(tag::TOUCH, 0x123, false)]);
    assert_eq!(hw.ipc_out, vec![pack(tag::SOUND, 0x456, false)]);
    let mut exact = Bus::new();
    exact.w32(crate::ds_mem::IO_BASE + 0x188, pack(tag::TOUCH, 0x123, false));
    assert!(
        exact.hle7_in.is_empty() && exact.ipc_out.len() == 1,
        "default: everything to the CPU"
    );
}

#[test]
fn touch_auto_sampling_start_samples_stop() {
    let (mut r, mut hw) = setup();
    // start: 4 samples per frame from line 10
    let out = send(&mut r, &mut hw, &packet(tag::TOUCH, &[0x0104, 10]));
    assert_eq!(out, vec![spi::reply_word(1, 0)]);
    assert_eq!(r.spi.touch.vcounts, vec![10, 75, 141, 207]);
    let mut samples = Vec::new();
    for _ in 0..263 {
        hw.advance(crate::ds_mem::CYCLES_PER_LINE);
        samples.extend(send(&mut r, &mut hw, &[]));
    }
    let data: Vec<u32> = samples.iter().map(|w| w >> 6 & 0xFFFF).collect();
    assert_eq!(data, vec![0x9000, 0x9001, 0x9002, 0x9003]);
    assert_eq!(
        (hw.r16(shared::TOUCH), hw.r16(shared::TOUCH + 2)),
        (0, 0x600),
        "pen up: validity 3"
    );
    // a second start is refused; stop
    assert_eq!(
        send(&mut r, &mut hw, &packet(tag::TOUCH, &[0x0104, 10])),
        vec![spi::reply_word(1, 3)]
    );
    assert_eq!(
        send(&mut r, &mut hw, &packet(tag::TOUCH, &[0x0200])),
        vec![spi::reply_word(2, 0)]
    );
    hw.advance(crate::ds_mem::CYCLES_PER_LINE * 263);
    assert!(send(&mut r, &mut hw, &[]).is_empty());
}

#[test]
fn touch_sample_with_pen_down() {
    let (mut r, mut hw) = setup();
    let mut p = Peripherals::new();
    p.spi.pen = Some(crate::peripherals::touch_raw(128, 96));
    hw.hle7_in.extend(packet(tag::TOUCH, &[0x0000]));
    r.advance(&mut hw, Some(&mut p));
    let (x, y) = p.spi.pen.unwrap();
    let s = hw.r16(shared::TOUCH) as u32 | (hw.r16(shared::TOUCH + 2) as u32) << 16;
    assert_eq!(s, x as u32 | (y as u32) << 12 | 1 << 24);
}

#[test]
fn nvram_status_read_and_page_write() {
    let (mut r, mut hw) = setup();
    let dst = 0x0210_0000u32;
    // RDSR before and after WREN
    let rdsr = packet(tag::NVRAM, &[0x2202, 0x1000, 0x0000]);
    assert_eq!(send(&mut r, &mut hw, &rdsr), vec![spi::reply_word(0x22, 0)]);
    assert_eq!(hw.r8(dst), 0);
    send(&mut r, &mut hw, &packet(tag::NVRAM, &[0x2000]));
    send(&mut r, &mut hw, &rdsr);
    assert_eq!(hw.r8(dst), 2);
    // page write 4 bytes at 0x03FA00 from 0x02110000, then read them back to 0x02120000
    for i in 0..4 {
        hw.w8(0x0211_0000 + i, 0xA0 + i as u8);
    }
    let out = send(
        &mut r,
        &mut hw,
        &packet(tag::NVRAM, &[0x2503, 0xFA00, 4, 0x0211, 0x0000]),
    );
    assert_eq!(out, vec![spi::reply_word(0x25, 0)]);
    send(&mut r, &mut hw, &rdsr);
    assert_eq!(hw.r8(dst), 0, "a write clears WEL");
    let out = send(
        &mut r,
        &mut hw,
        &packet(tag::NVRAM, &[0x2303, 0xFA00, 0, 4, 0x0212, 0x0000]),
    );
    assert_eq!(out, vec![spi::reply_word(0x23, 0)]);
    assert_eq!(hw.r32(0x0212_0000), 0xA3A2_A1A0);
    // a bad buffer address
    assert_eq!(
        send(&mut r, &mut hw, &packet(tag::NVRAM, &[0x2200, 0, 0])),
        vec![spi::reply_word(0x22, 2)]
    );
}

#[test]
fn pm_registers_and_led_pattern() {
    let (mut r, mut hw) = setup();
    assert_eq!(
        send(&mut r, &mut hw, &packet(tag::PM, &[0x6402, 0x0001])),
        vec![spi::reply_word(0x64, 0)]
    );
    assert_eq!(
        send(&mut r, &mut hw, &packet(tag::PM, &[0x6502])),
        vec![spi::reply_word(0x72, 1)]
    );
    assert_eq!(
        send(&mut r, &mut hw, &packet(tag::PM, &[0x6500])),
        vec![spi::reply_word(0x70, 0)]
    );
    assert_eq!(
        send(&mut r, &mut hw, &packet(tag::PM, &[0x6603])),
        vec![spi::reply_word(0x66, 0)]
    );
    assert_eq!(
        send(&mut r, &mut hw, &packet(tag::PM, &[0x6700])),
        vec![spi::reply_word(0x67, 3)]
    );
    // sleep is not native: forwarded whole to the CPU
    send(&mut r, &mut hw, &packet(tag::PM, &[0x6100]));
    assert_eq!(hw.ipc_out, packet(tag::PM, &[0x6100]));
}

#[test]
fn backup_init_write_verify_read() {
    let (mut r, mut hw) = setup();
    let block = 0x0217_0000u32;
    let ok = pack(tag::FS, 1, true);
    assert_eq!(
        send(&mut r, &mut hw, &[pack(tag::FS, 0, true), pack(tag::FS, block, true)]),
        vec![ok]
    );
    // write 8 bytes from 0x02171000 to save offset 0x100
    for i in 0..8 {
        hw.w8(0x0217_1000 + i, i as u8 + 1);
    }
    hw.w32(block + 0xC, 0x0217_1000);
    hw.w32(block + 0x10, 0x100);
    hw.w32(block + 0x14, 8);
    assert_eq!(send(&mut r, &mut hw, &[pack(tag::FS, 7, true)]), vec![ok]);
    assert_eq!(send(&mut r, &mut hw, &[pack(tag::FS, 9, true)]), vec![ok]);
    assert_eq!(hw.r32(block), 0, "verify passed");
    hw.w8(0x0217_1003, 0xEE);
    send(&mut r, &mut hw, &[pack(tag::FS, 9, true)]);
    assert_eq!(hw.r32(block), 1, "verify mismatch");
    // read it back: source = save offset, destination = RAM
    hw.w32(block + 0xC, 0x100);
    hw.w32(block + 0x10, 0x0217_2000);
    send(&mut r, &mut hw, &[pack(tag::FS, 6, true)]);
    assert_eq!(hw.r32(0x0217_2000), 0x0403_0201);
    assert_eq!(r.own_periph.as_ref().unwrap().backup.data[0x107], 8);
    send(&mut r, &mut hw, &[pack(tag::FS, 5, true)]);
    assert_eq!(hw.r32(block), 3, "unsupported");
}

#[test]
fn rtc_wireless_and_cartridge() {
    let (mut r, mut hw) = setup();
    assert_eq!(
        send(&mut r, &mut hw, &[pack(tag::RTC, 0x1100, false)]),
        vec![pack(tag::RTC, 0x9100, false)]
    );
    assert_eq!(
        hw.r32(shared::RTC_DATE),
        0x0215_0610,
        "2010-06-15, Tuesday (peripherals::Rtc)"
    );
    assert_eq!(
        send(&mut r, &mut hw, &[pack(tag::RTC, 0x1200, false)]),
        vec![pack(tag::RTC, 0x9200, false)]
    );
    assert_eq!(hw.r8(shared::RTC_TIME), 0x12);
    let wm = pack(tag::WM, 0x1234, false);
    assert_eq!(
        send(&mut r, &mut hw, &[wm]),
        vec![wm | 0x20],
        "no wireless: echoed with the error bit"
    );
    let wvr = pack(tag::WVR, 0x1_0000, false);
    assert_eq!(send(&mut r, &mut hw, &[wvr]), vec![wvr]);
    hw.w8(shared::CTRDG_TYPE, 0xFF);
    assert_eq!(
        send(&mut r, &mut hw, &[pack(tag::CTRDG, 0x2D_FC01, false)]),
        vec![pack(tag::CTRDG, 1, false)]
    );
    assert_eq!(
        hw.r8(shared::CTRDG_TYPE),
        0xFE,
        "no cartridge: bit 0 cleared, the rest kept"
    );
    assert_eq!(CLAIMED >> tag::CTRDG & 1, 0, "CTRDG stays with the booting ARM7 CPU");
}

#[test]
fn replies_wait_for_room_in_the_fifo() {
    let (mut r, mut hw) = setup();
    hw.hle7_in.extend((0..20).map(|_| pack(tag::RTC, 0x1200, false)));
    r.advance(&mut hw, None);
    assert_eq!((hw.fifo_in.len(), r.pending()), (16, 4));
    hw.fifo_in.clear();
    r.advance(&mut hw, None);
    assert_eq!((hw.fifo_in.len(), r.pending()), (4, 0));
}
