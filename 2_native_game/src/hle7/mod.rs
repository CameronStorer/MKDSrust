//! ARM7 → PXI service HLE (Block 5, stage 5.2a; `4_docs/BLOCK5_PLAN.md`, lane `b5-hle7`).
//!
//! The ARM9 talks to the ARM7 only through the IPC FIFO (PXI words: bits 0-4 tag, bit 5 error, bits 6-31
//! data), the IPCSYNC nibble and shared main memory. [`Router`] receives the ARM9's FIFO words for the tags it
//! claims and answers as the game's own ARM7 code would: the same reply words, in the same order, and the
//! same shared-memory writes. The behaviour of every service was read from the lifted ARM7 code
//! (`dictionary/game_names.csv`: `spi_*`, `tp_*`, `rtc_*`, `pm_*`, `nvram_*`, `card_*`, `ctrdg_*`, `wvr_*`)
//! and checked against a recording of the exact model ([`trace`], [`replay`]).
//!
//! Switch: `MKDS_HW_ARM7=native` (default `exact` = the lifted ARM7 CPU answers everything, unchanged).
//!
//! **Split with the ARM7 CPU (5.2a).** Sound stays on the lifted driver until lane 5.2b: in native mode the
//! ARM7 CPU keeps running and still owns the SOUND tag (7), its boot (it must run its own start-up so the
//! sound driver exists: PXI_Init sync-nibble handshake, the remote-ready bitmask at 0x027FFF8C, and the
//! CTRDG word its boot waits for), the OS tag (12: reset requests), the CARD tag (14: card pulled out) and
//! MIC (see [`CLAIMED`]), plus its background jobs that are not PXI replies (pad X/Y/hinge at 0x027FFFA8,
//! its V-blank counter, the lock words). The other tags (NVRAM, RTC, touch, PM, WM, backup, WVR) are
//! claimed here: its words never reach the CPU ([`crate::ds_mem::Bus::hle7_tags`]), so the CPU's own services for
//! them stay idle. A command of a claimed tag that this module does not implement (none occur in the
//! acceptance race) is *forwarded* to the CPU unchanged, so behaviour degrades to the exact model instead
//! of breaking; [`Router::forwarded`] counts them.
//!
//! **Without the ARM7 CPU (`MKDS_HW_SND=native`, Block 5.4).** Every tag is claimed from the start. The CPU's
//! boot runs once as plain code ([`snd::Snd::boot`]); NVRAM, RTC, touch, PM, WM, backup, WVR and CTRDG start-up
//! are answered here; SOUND, OS (12: reset request, answered by [`snd`]'s main-loop poll), CARD (14) and MIC (9)
//! and any command without a native handler go to the ARM7's own lifted callbacks as plain calls, with the
//! SPI service thread's queue drained the same way (MIC, and touch / PM / NVRAM commands forwarded there).
//!
//! Timing: replies are queued when a command completes and delivered at the next [`Router::advance`]
//! (called after every ARM9 slice), not after the ARM7's thread and SPI latency; flash writes complete at
//! once. Touch auto-sampling runs on the V-count lines the ARM7's periodic V-alarms would use.
pub mod backup;
pub mod replay;
pub mod snd;
pub mod spi;
pub mod trace;

mod clock;
use crate::ds_mem::Bus;
use crate::peripherals::Peripherals;
use crate::rt::Mem;
use std::collections::VecDeque;

/// PXI FIFO tags (NitroSDK numbering, as the ARM7 registers its callbacks).
pub mod tag {
    pub const NVRAM: u32 = 4;
    pub const RTC: u32 = 5;
    pub const TOUCH: u32 = 6;
    pub const SOUND: u32 = 7;
    pub const PM: u32 = 8;
    pub const MIC: u32 = 9;
    pub const WM: u32 = 10;
    /// Card backup (save memory) commands; the SDK calls the tag "FS".
    pub const FS: u32 = 11;
    pub const OS: u32 = 12;
    pub const CTRDG: u32 = 13;
    pub const CARD: u32 = 14;
    /// Wireless VRAM: the ARM9 lends a VRAM bank for the ARM7's wireless code.
    pub const WVR: u32 = 15;

    pub fn name(t: u32) -> &'static str {
        match t {
            NVRAM => "nvram",
            RTC => "rtc",
            TOUCH => "touch",
            SOUND => "sound",
            PM => "pm",
            MIC => "mic",
            WM => "wm",
            FS => "backup",
            OS => "os",
            CTRDG => "ctrdg",
            CARD => "card",
            WVR => "wvr",
            _ => "other",
        }
    }
}

/// Tags the router answers in native mode (bit n = tag n). Not claimed (stay on the ARM7 CPU): SOUND
/// (lane 5.2b), OS (reset), CARD (pulled out); CTRDG, because its one command is part of the ARM7's own
/// start-up: `ctrdg_check_cartridge_maybe` (0x03803EB4) spins in the CPU's boot until the ARM9's CTRDG word
/// arrives, so while the CPU runs it must receive it (claiming it starves the boot and the sound driver never
/// starts); and MIC: the microphone's timer-driven auto-sampling into ARM9 buffers is not modelled natively
/// yet and the game sends no MIC command in the acceptance race, so the CPU's code keeps serving it. The
/// native CTRDG handler below is ready for a CPU-less mode (5.2b).
pub const CLAIMED: u32 =
    1 << tag::NVRAM | 1 << tag::RTC | 1 << tag::TOUCH | 1 << tag::PM | 1 << tag::WM | 1 << tag::FS | 1 << tag::WVR;

/// Default bus cycles between a command and its reply: about what the ARM7 needs to wake its service
/// thread and answer a simple command in the exact model (930-1,500 cycles measured for CTRDG, RTC and
/// NVRAM status). The ARM9 side arms some waits only after sending, so an instant reply can be missed.
pub const REPLY_LATENCY: u64 = 1024;

/// A PXI FIFO word.
pub fn pack(tag: u32, data: u32, err: bool) -> u32 {
    tag & 0x1F | (err as u32) << 5 | (data & 0x03FF_FFFF) << 6
}

/// `MKDS_HW_ARM7=native`.
pub fn native_selected() -> bool {
    std::env::var("MKDS_HW_ARM7").is_ok_and(|v| v.eq_ignore_ascii_case("native"))
}

/// Shared main-RAM addresses the ARM7 services write.
pub mod shared {
    /// Touch sample (32 bits written as two halves): x 0-11, y 12-23, touch 24, validity 25-26.
    pub const TOUCH: u32 = 0x027F_FFAA;
    /// RTC date (year, month, day, weekday) then time (hour, minute, second), BCD.
    pub const RTC_DATE: u32 = 0x027F_FDE8;
    pub const RTC_TIME: u32 = 0x027F_FDEC;
    /// Cartridge flags byte; bit 0 = a valid slot-2 cartridge, set when the ARM9's CTRDG library starts.
    pub const CTRDG_TYPE: u32 = 0x027F_FC35;
}

/// The native ARM7 services and the PXI router in front of them.
pub struct Router {
    /// Tags claimed (also written to [`Bus::hle7_tags`] by [`Router::install`]).
    pub claimed: u32,
    /// Replay routers pin the RTC without mutating the process environment.
    pub(super) fixed_rtc: bool,
    /// Replies not yet in the ARM7 → ARM9 FIFO, oldest first, with the bus cycle they become due.
    pending: VecDeque<(u64, u32)>,
    /// Bus cycles between a command and its reply ([`REPLY_LATENCY`]; `MKDS_HLE7_LATENCY` overrides).
    pub latency: u64,
    /// Bus cycle of the current [`Router::advance`].
    now: u64,
    pub spi: spi::Spi,
    pub backup: backup::Backup,
    /// Words handed to the ARM7 CPU because no native handler exists, by tag.
    pub forwarded: [u64; 32],
    /// Words received and replies sent, by tag (diagnostics).
    pub received: [u64; 32],
    pub sent: [u64; 32],
    /// Every reply in order, when recording (tests).
    pub log: Option<Vec<u32>>,
    /// Backing peripherals when there is no ARM7 CPU (tests); otherwise the CPU's are used.
    pub own_periph: Option<Box<Peripherals>>,
    /// The event-driven ARM7 sound driver (`MKDS_HW_SND=native`, Block 5.2b); once it has taken over, every
    /// tag is claimed and words without a native service go to the ARM7's own (lifted) PXI callbacks.
    pub snd: Option<snd::Snd>,
}

impl Default for Router {
    fn default() -> Self {
        Router::new()
    }
}

impl Router {
    pub fn new() -> Router {
        Router {
            claimed: CLAIMED,
            fixed_rtc: cfg!(test),
            pending: VecDeque::new(),
            latency: REPLY_LATENCY,
            now: 0,
            spi: spi::Spi::default(),
            backup: backup::Backup::default(),
            forwarded: [0; 32],
            received: [0; 32],
            sent: [0; 32],
            log: None,
            own_periph: None,
            snd: None,
        }
    }

    /// A router claiming its tags on `hw`, when `MKDS_HW_ARM7=native`.
    /// Diagnostics: `MKDS_HLE7_TAGS=<hex mask>` claims only those tags, `MKDS_HLE7_LATENCY=<cycles>` sets
    /// the reply latency.
    pub fn from_env(hw: &mut Bus) -> Option<Router> {
        (native_selected() || snd::native_selected()).then(|| {
            let mut r = Router::new();
            if snd::native_selected() {
                r.snd = Some(snd::Snd::new());
                if !snd::cpu_boot_selected() {
                    // no ARM7 CPU at all (Block 5.4): every tag is answered here, natively or by the lifted
                    // callbacks run as plain calls
                    r.claimed = u32::MAX;
                }
            }
            let var = |k: &str| std::env::var(k).ok();
            if let Some(m) =
                var("MKDS_HLE7_TAGS").and_then(|v| u32::from_str_radix(v.trim_start_matches("0x"), 16).ok())
            {
                r.claimed &= m;
            }
            if let Some(l) = var("MKDS_HLE7_LATENCY").and_then(|v| v.parse().ok()) {
                r.latency = l;
            }
            r.install(hw);
            r
        })
    }

    /// Route the claimed tags' ARM9 words to this router.
    pub fn install(&self, hw: &mut Bus) {
        hw.hle7_tags = self.claimed;
    }

    /// Queue a reply word for the ARM9.
    pub(crate) fn reply(&mut self, word: u32) {
        self.sent[(word & 0x1F) as usize] += 1;
        if let Some(l) = self.log.as_mut() {
            l.push(word);
        }
        self.pending.push_back((self.now + self.latency, word));
    }

    /// Hand `word` to the ARM7 CPU as if the router were not there.
    pub(crate) fn forward(&mut self, hw: &mut Bus, word: u32) {
        self.forwarded[(word & 0x1F) as usize] += 1;
        match self.snd.as_mut() {
            Some(s) if s.active => s.inbox.push(word),
            _ => hw.ipc_out.push(word),
        }
    }

    /// One step of the ARM7 side for `cycles` bus cycles (`Machine::run_arm7`): the native services, and with
    /// `MKDS_HW_SND=native` the event-driven ARM7 once it has taken over. Returns whether the ARM7 CPU must
    /// still run.
    pub fn run(&mut self, hw: &mut Bus, a: Option<&mut crate::arm7::Arm7>, cycles: u64) -> bool {
        let Some(a) = a else {
            self.advance(hw, None);
            return true;
        };
        // Block 5.4: the ARM7 boots without its CPU the first time it would run (`MKDS_SND_BOOT=cpu`: the CPU
        // boots and the driver takes over later)
        if let Some(s) = self.snd.as_mut().filter(|s| !s.active && !snd::cpu_boot_selected()) {
            s.boot(hw, a);
            self.claimed = u32::MAX;
            hw.hle7_tags = u32::MAX;
        }
        self.advance(hw, Some(&mut a.s.periph));
        let Some(s) = self.snd.as_mut() else { return true };
        if !s.active {
            if !s.try_take_over(hw, a) {
                return true;
            }
            self.claimed = u32::MAX;
            hw.hle7_tags = u32::MAX;
        }
        s.advance(hw, a, cycles);
        false
    }

    /// Take the ARM9's new words, run time-driven service work (touch auto-sampling), deliver replies.
    pub fn advance(&mut self, hw: &mut Bus, p: Option<&mut Peripherals>) {
        match p {
            Some(p) => self.advance_with(hw, p),
            None => {
                let mut own = self.own_periph.take().unwrap_or_default();
                self.advance_with(hw, &mut own);
                self.own_periph = Some(own);
            }
        }
    }

    fn advance_with(&mut self, hw: &mut Bus, p: &mut Peripherals) {
        self.now = hw.cycles;
        if !hw.hle7_in.is_empty() {
            let words = std::mem::take(&mut hw.hle7_in);
            for w in words {
                self.receive(hw, p, w);
            }
        }
        spi::touch_alarms(self, hw, p);
        self.deliver(hw);
    }

    /// Run only the time-driven work due by now (touch alarms), without taking new ARM9 words.
    pub fn catch_up(&mut self, hw: &mut Bus) {
        self.now = hw.cycles;
        let mut own = self.own_periph.take().unwrap_or_default();
        spi::touch_alarms(self, hw, &mut own);
        self.own_periph = Some(own);
    }

    /// Send queued replies while the FIFO has room (the ARM7's `pxi_send_fifo` retries while it is full).
    pub fn deliver(&mut self, hw: &mut Bus) {
        while hw.fifo_in.len() < 16 {
            match self.pending.front() {
                Some(&(due, w)) if due <= hw.cycles => {
                    self.pending.pop_front();
                    hw.arm7_send(w);
                }
                _ => break,
            }
        }
    }

    /// One ARM9 word of a claimed tag.
    pub fn receive(&mut self, hw: &mut Bus, p: &mut Peripherals, w: u32) {
        let t = w & 0x1F;
        let err = w >> 5 & 1 != 0;
        let data = w >> 6;
        self.received[t as usize] += 1;
        match t {
            tag::NVRAM | tag::TOUCH | tag::PM => spi::receive(self, hw, p, w),
            tag::SOUND if self.snd.as_ref().is_some_and(|s| s.active) => {
                self.snd.as_mut().unwrap().inbox.push(w);
            }
            tag::RTC => self.rtc(hw, p, w, data),
            tag::FS => backup::receive(self, hw, p, w, data, err),
            tag::CTRDG if data & 0x3F == 1 => {
                // CTRDG start-up: the ARM7 checks the slot-2 cartridge header the word points to and sets bit 0
                // of the cartridge flags byte when it is valid (nothing inserted: clear), then answers 1.
                let flags = hw.r8(shared::CTRDG_TYPE);
                hw.w8(shared::CTRDG_TYPE, flags & !1);
                self.reply(pack(tag::CTRDG, 1, false));
            }
            tag::WVR if matches!(data, 0x1_0000 | 0x2_0000) => {
                // Wireless VRAM lend (1) / return (2). Wireless is not available natively; the ARM9 only
                // needs the bank handshake acknowledged (success = the command echoed).
                self.reply(pack(tag::WVR, data, false));
            }
            tag::WM => {
                // No wireless manager: what the ARM7's PXI layer does for a tag nobody registered.
                if !err {
                    self.reply(w | 0x20);
                }
            }
            _ => self.forward(hw, w),
        }
    }

    /// RTC (`rtc_pxi_receive`, `rtc_execute_command`): host local time by default; fixed via MKDS_RTC.
    /// Fixed values come from [`crate::peripherals::Rtc`]. Reply: `0x8000 | cmd << 8 | result`.
    fn rtc(&mut self, hw: &mut Bus, p: &mut Peripherals, w: u32, data: u32) {
        let cmd = (data >> 8) & 0x7F;
        let fixed = self.fixed_rtc || match std::env::var("MKDS_RTC") {
            Ok(mode) => mode == "fixed",
            Err(_) => std::env::var_os("MKDS_RECORD").is_some()
                || std::env::var_os("MKDS_PXI_LOG").is_some(),
        };
        let bytes = if fixed { p.rtc.datetime_bcd() } else { clock::datetime_bcd() };
        let (at, from, n) = match cmd {
            0x10 => (shared::RTC_DATE, 0, 7),
            0x11 => (shared::RTC_DATE, 0, 4),
            0x12 => (shared::RTC_TIME, 4, 3),
            _ => return self.forward(hw, w),
        };
        for i in 0..n {
            hw.w8(at + i as u32, bytes[from + i]);
        }
        self.reply(pack(tag::RTC, 0x8000 | cmd << 8, false));
    }

    /// Replies queued but not yet in the FIFO.
    pub fn pending(&self) -> usize {
        self.pending.len()
    }

    /// One line per tag with traffic (diagnostics).
    pub fn summary(&self) -> String {
        let mut s = String::new();
        for t in 0..32 {
            if self.received[t] + self.sent[t] + self.forwarded[t] > 0 {
                s += &format!(
                    "{}: {} in, {} out, {} forwarded; ",
                    tag::name(t as u32),
                    self.received[t],
                    self.sent[t],
                    self.forwarded[t]
                );
            }
        }
        s
    }
}

#[cfg(test)]
mod tests;
