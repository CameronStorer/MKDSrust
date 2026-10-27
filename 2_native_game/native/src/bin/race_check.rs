//! Private AMCE0 acceptance driver. Reads guest telemetry, sends host buttons, never writes guest RAM.
//! Not a game runner or a substitute for compiled-game acceptance.
use recomp::{boot::session::Session, dispatch::Dispatcher, rt::Mem};
use std::{
    io::{BufWriter, Write},
    path::Path,
};

fn main() {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if let Err(error) = run_with_table(&args, &[], &[]) {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

fn pointer(value: u32, bytes: u32) -> bool {
    value >= 0x0200_0000
        && value
            .checked_add(bytes)
            .is_some_and(|end| end <= 0x0240_0000)
}

fn capture(session: &mut Session<'_>, dir: &Path, name: &str) -> std::io::Result<()> {
    let pixels = session.screens();
    for (part, pixels) in ["top", "bottom"].into_iter().zip(pixels.chunks(256 * 192)) {
        let rgba: Vec<_> = pixels.iter().map(|p| p << 8 | 255).collect();
        std::fs::write(
            dir.join(format!("{name}_{part}.ppm")),
            recomp::gfx2d::to_ppm(&rgba),
        )?;
    }
    Ok(())
}

pub fn run_with_table(
    args: &[String],
    table: &'static [recomp::mem::Lifted],
    signatures: &'static [recomp::residency::SegmentSig],
) -> Result<(), Box<dyn std::error::Error>> {
    run_with_tables(args, table, signatures, &[])
}

/// As `run_with_table`, with the generated ARM7 table: when `table7` is not empty the ARM7 runs only
/// from lifted code (a missing block is an error, not interpreted), as in the shipped game.
pub fn run_with_tables(
    args: &[String],
    table: &'static [recomp::mem::Lifted],
    signatures: &'static [recomp::residency::SegmentSig],
    table7: &'static [recomp::mem::Lifted],
) -> Result<(), Box<dyn std::error::Error>> {
    // Block 5.0 oracle options (anywhere among the positional arguments; see `oracle`):
    // --record DIR, --replay INPUTS.csv (open loop, route ignored: pass `-`), --mask FILE, --dump-frames a,b
    let mut positional = Vec::new();
    let (mut record, mut replay, mut mask, mut dumps) = (None, None, oracle::Mask::default(), Vec::new());
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut value = || it.next().cloned().ok_or(format!("{a} needs a value"));
        match a.as_str() {
            "--record" => record = Some(value()?),
            "--replay" => replay = Some(value()?),
            "--mask" => mask = oracle::Mask::parse(&std::fs::read_to_string(value()?)?)?,
            "--dump-frames" => {
                dumps = value()?.split(',').map(|f| f.trim().parse()).collect::<Result<Vec<u32>, _>>()?
            }
            _ => positional.push(a.clone()),
        }
    }
    if record.is_none() {
        record = std::env::var("MKDS_RECORD").ok();
    }
    if replay.is_some() && record.is_none() {
        return Err("--replay needs --record DIR".into());
    }
    let args = &positional;
    if !(3..=4).contains(&args.len()) {
        return Err(
            "usage: race_check <user.nds> <user-ROM-route.csv> <new-output-dir> [max-frames=15000] [--record DIR [--replay inputs.csv] [--mask FILE] [--dump-frames N,..]]"
                .into(),
        );
    }
    let rom = std::fs::read(&args[0])?;
    let image = mkds_builder::extract::arm9_main(&rom)?;
    let script = replay.as_deref().map(|p| oracle::read_inputs(Path::new(p))).transpose()?;
    let mut points = Vec::new();
    let route = if script.is_some() && args[1] == "-" {
        "x,z\n0,0\n0,1\n1,0\n".to_string()
    } else {
        std::fs::read_to_string(&args[1])?
    };
    for line in route.lines().skip(1) {
        let (x, z) = line.split_once(',').ok_or("Invalid route CSV")?;
        points.push((
            x.trim().parse::<i32>()? as f64,
            z.trim().parse::<i32>()? as f64,
        ));
    }
    if points.len() < 3 {
        return Err("Route needs at least three points".into());
    }
    let max_frames = args
        .get(3)
        .map(|v| v.parse())
        .transpose()?
        .unwrap_or(15_000u64);
    let dir = Path::new(&args[2]);
    std::fs::create_dir(dir)?;
    let mut inputs = BufWriter::new(std::fs::File::create(dir.join("inputs.csv"))?);
    let mut telemetry = BufWriter::new(std::fs::File::create(dir.join("telemetry.csv"))?);
    writeln!(inputs, "frame,buttons,x,y")?;
    writeln!(
        telemetry,
        "frame,kart,x,z,forward_x,forward_z,lap,state,place,race_ticks,buttons,target"
    )?;
    let fixed: Vec<_> = table
        .iter()
        .map(|l| l.segment)
        .filter(|s| !recomp::residency::is_overlay(s))
        .collect();
    let dispatcher = Dispatcher::with_loaded(table, &fixed);
    dispatcher.set_natives(recomp::native::selected());
    let mut session = Session::new(&rom, &image, &dispatcher)?;
    if !table7.is_empty() {
        session
            .machine
            .arm7
            .as_mut()
            .ok_or("ARM7 missing from session")?
            .configure_lifted(table7, false);
    }
    session.configure_residency(signatures)?;
    // Block 4A.2: the ARM9 runs in fibers (nested calls; proven identical to the flat loop over a
    // full race). MKDS_FLAT=1 restores the flat loop.
    if std::env::var_os("MKDS_FLAT").is_none() {
        session.enable_fibers();
    }
    // MKDS_SHADOW=1: every native function, each call checked against its translation (block 3)
    if std::env::var_os("MKDS_SHADOW").is_some() {
        session.enable_shadow(recomp::native::NATIVE);
    }
    let recorder = match &record {
        Some(r) => {
            let rec = std::rc::Rc::new(std::cell::RefCell::new(oracle::Recorder::new(
                Path::new(r),
                mask,
                script,
                dumps,
            )?));
            oracle::Recorder::install(rec.clone(), &mut session.machine);
            Some(rec)
        }
        None => None,
    };
    let replaying = recorder.as_ref().is_some_and(|r| r.borrow().replaying());
    let mut last_frame = u64::MAX;
    let mut last_buttons = u16::MAX;
    let mut target = 0usize;
    let mut started = false;
    let mut finish = None;
    let mut previous_lap = 0;
    let wall = std::time::Instant::now();
    while session.frame_number() < max_frames {
        let frame = session.frame_number();
        if frame != last_frame {
            last_frame = frame;
            let hw = &mut session.machine.hw;
            let kart = hw.r32(0x0217_acf8);
            let race = hw.r32(0x0217_55fc);
            let valid = pointer(kart, 0x5a8) && pointer(race, 0x500);
            let (x, z, fx, fz, lap, state, place, ticks) = if valid {
                (
                    hw.r32(kart + 0x80) as i32,
                    hw.r32(kart + 0x88) as i32,
                    hw.r32(kart + 0x50) as i32,
                    hw.r32(kart + 0x58) as i32,
                    hw.r32(race + 0x38),
                    hw.r32(race + 0x14),
                    hw.r16(race + 0x50) & 15,
                    hw.r32(race + 0x18),
                )
            } else {
                (0, 0, 0, 0, 0, 0, 0, 0)
            };
            let mut buttons = if frame >= 480 && frame % 120 < 8 {
                1
            } else {
                0
            };
            if frame >= 2280 && valid {
                if !started {
                    target = points
                        .iter()
                        .enumerate()
                        .min_by(|(_, a), (_, b)| {
                            let dist = |p: &&(f64, f64)| (p.0 - x as f64).hypot(p.1 - z as f64);
                            dist(a).total_cmp(&dist(b))
                        })
                        .map_or(0, |(i, _)| i);
                    started = true;
                }
                let (mut dx, mut dz) = (points[target].0 - x as f64, points[target].1 - z as f64);
                if dx.hypot(dz) < 250.0 * 4096.0 {
                    target = (target + 1) % points.len();
                    (dx, dz) = (points[target].0 - x as f64, points[target].1 - z as f64);
                }
                let side = (dx * fz as f64 - dz * fx as f64) / (dx.hypot(dz) * 4096.0 + 1.0);
                let ahead = dx * fx as f64 + dz * fz as f64 >= 0.0;
                buttons = 1 | if !ahead {
                    if side >= 0.0 { 32 } else { 16 }
                } else if side > 0.06 {
                    32
                } else if side < -0.06 {
                    16
                } else {
                    0
                };
                // Explicit controller checks on the opening straight: brake/release,
                // short left/right inputs, and pause followed by the default Resume.
                if (2420..2450).contains(&frame) {
                    buttons = 2;
                }
                if (2450..2470).contains(&frame) {
                    buttons = 0;
                }
                if (2500..2508).contains(&frame) {
                    buttons = 1 | 32;
                }
                if (2510..2518).contains(&frame) {
                    buttons = 1 | 16;
                }
                if (2700..2762).contains(&frame) {
                    buttons = if frame < 2702 {
                        8
                    } else if frame >= 2760 {
                        1
                    } else {
                        0
                    };
                }
                if finish.is_none() && matches!(state, 1..=3) && lap >= 3 {
                    finish = Some(frame);
                    eprintln!(
                        "Guest finish at frame {frame}, lap {lap}, state {state}, place {place}"
                    );
                }
                if finish.is_some() {
                    buttons = 0;
                }
            }
            if let Some(rec) = &recorder {
                let r = rec.borrow();
                if let Some(e) = &r.error {
                    return Err(format!("recorder: {e}").into());
                }
                if replaying {
                    // open loop: the recorder applies the script at logical-frame starts
                    buttons = r.buttons.get();
                } else {
                    r.buttons.set(buttons);
                }
            }
            if buttons != last_buttons {
                writeln!(inputs, "{frame},{buttons},,")?;
                last_buttons = buttons;
            }
            if !replaying {
                session.machine.set_input(buttons, None);
            }
            writeln!(
                telemetry,
                "{frame},{kart:#x},{x},{z},{fx},{fz},{lap},{state},{place},{ticks},{buttons},{target}"
            )?;
            if lap != previous_lap && started {
                eprintln!(
                    "Frame {frame}: lap {lap}, state {state}, place {place}, {:?}",
                    wall.elapsed()
                );
                previous_lap = lap;
            }
            if frame % 600 == 0 {
                eprintln!(
                    "Frame {frame}: x {x} z {z} target {target} lap {lap}, {:?}",
                    wall.elapsed()
                );
                telemetry.flush()?;
                inputs.flush()?;
            }
            if matches!(frame, 2410 | 2448 | 2468 | 2518 | 2720 | 2750 | 2800) {
                capture(&mut session, dir, &format!("controls_{frame}"))?;
            }
            if frame % 1200 == 0 && frame > 2200 {
                capture(&mut session, dir, &format!("race_{frame}"))?;
            }
            if finish == Some(frame) {
                capture(&mut session, dir, "finish")?;
            }
            if let Some(rec) = recorder.as_ref().filter(|_| replaying) {
                let r = rec.borrow();
                if r.last_frame().is_some_and(|l| l > r.script_end + 600) {
                    break;
                }
            }
            if finish.is_some_and(|at| frame >= at + 900) {
                capture(&mut session, dir, "results")?;
                break;
            }
        }
        session.advance(16_384)?;
        session.machine.take_audio();
    }
    capture(&mut session, dir, "last")?;
    inputs.flush()?;
    telemetry.flush()?;
    if let Some(rec) = &recorder {
        let mut r = rec.borrow_mut();
        r.flush()?;
        eprintln!("Oracle recording: {} logical frames, last {:?}", r.samples, r.last_frame());
        if let Some(e) = &r.error {
            return Err(format!("recorder: {e}").into());
        }
    }
    if replaying {
        return Ok(());
    }
    if let Some(shadow) = &session.shadow {
        eprintln!("Shadow check of native functions:
{}", shadow.summary());
        std::fs::write(dir.join("shadow.txt"), shadow.summary())?;
    }
    if finish.is_none() {
        return Err(format!(
            "Race did not finish within {} hardware frames",
            session.frame_number()
        )
        .into());
    }
    let arm7 = session.machine.arm7.as_ref();
    eprintln!(
        "Acceptance driver stopped at frame {} after {:?}; stats {:?}; ARM7 lifted cycles {} interpreted instructions {}{}; fiber thread switches {}; results captures require inspection",
        session.frame_number(),
        wall.elapsed(),
        session.stats,
        arm7.map_or(0, |a| a.lifted_cycles),
        arm7.map_or(0, |a| a.interpreted_instructions),
        arm7.and_then(|a| a.execution_error.as_deref()).map_or(String::new(), |e| format!("; ARM7 error: {e}")),
        session.fiber_switches()
    );
    if let Some(e) = arm7.and_then(|a| a.execution_error.clone()) {
        return Err(format!("ARM7 stopped: {e}").into());
    }
    Ok(())
}

/// Block 5.0 logical-frame oracle: the recorder behind `--record DIR` (see `frame_compare`).
///
/// One sample per logical frame (SDK V-blank count, `Session::logical_frame`), taken when the game's main
/// loop enters VBlankIntrWait (SWI 5) for the first time in that frame: the frame's work is done there, at
/// the same point of the game's own code in every build. Files (no RAM bytes, only hashes and I/O words):
/// - `regions.csv`: index, name, what the region covers. `mask.txt`: the mask the hashes were taken with.
/// - `frames.bin`: `MKORC001`, u32 region count; per sample u32 frame, u32 hardware frame, u64 cycles, then
///   one u64 hash per region (masked bytes hashed as zero, masked regions as 0).
/// - `inputs.csv`: buttons held from each logical frame on (change rows; `frame_compare` replays it).
/// - `events.bin`: 12-byte PXI events (u32 frame, u8 kind, u8 bytes, u16 offset, u32 value), kinds in
///   `recomp::ds_mem::EV_*`; SPU writes too with `MKDS_RECORD_SPU=1` (otherwise only their hash per frame).
/// - `census.csv` / `spins.csv`: per-frame counters (waits, IRQ sources, polled I/O, lag) and tight loops.
pub mod oracle {
    use recomp::{ds_mem, machine::Machine};
    use std::cell::{Cell, RefCell};
    use std::io::{BufWriter, Write};
    use std::path::{Path, PathBuf};
    use std::rc::Rc;

    const MAIN_BLOCK: u32 = 0x1_0000;
    const FRAME_CYCLES: u64 = 263 * 2130;

    /// Where a hashed region lives.
    #[derive(Clone)]
    enum Src {
        /// ARM9 address range.
        Fixed(u32, u32),
        /// `len` bytes at `*ptr + off`.
        Ptr(u32, u32, u32),
        Dtcm,
        Itcm,
        Io(usize, usize),
        Palette,
        Oam,
        Vram(usize),
        Polys,
        Pxi,
        Spu,
    }

    fn regions() -> Vec<(String, Src)> {
        let mut r: Vec<(String, Src)> = Vec::new();
        for k in 0..8 {
            r.push((format!("kart{k}"), Src::Ptr(0x0217_acf8, 0x5a8 * k, 0x5a8)));
        }
        r.push(("race_manager".into(), Src::Ptr(0x0217_55fc, 0, 0x500)));
        r.push(("item_slots".into(), Src::Ptr(0x027e_00c8, 0, 8 * 0x210)));
        r.push(("race_config".into(), Src::Ptr(0x0217_59a0, 0, 0x400)));
        r.push(("racer_records".into(), Src::Ptr(0x0217_b1dc, 0, 960)));
        r.push(("controller_slots".into(), Src::Fixed(0x0217_5600, 8 + 92 * 9)));
        r.push(("object_rng".into(), Src::Ptr(0x0217_b49c, 0, 16)));
        r.push(("dtcm".into(), Src::Dtcm));
        r.push(("itcm".into(), Src::Itcm));
        for b in 0..(ds_mem::MAIN_RAM_SIZE as u32 / MAIN_BLOCK) {
            let a = ds_mem::MAIN_RAM + b * MAIN_BLOCK;
            r.push((format!("main_{a:08x}"), Src::Fixed(a, MAIN_BLOCK)));
        }
        r.push(("io2d_a".into(), Src::Io(0x000, 0x060)));
        r.push(("io3d".into(), Src::Io(0x060, 0x070)));
        r.push(("io2d_b".into(), Src::Io(0x1000, 0x1070)));
        r.push(("io3d_tables".into(), Src::Io(0x320, 0x3c0)));
        r.push(("palette".into(), Src::Palette));
        r.push(("oam".into(), Src::Oam));
        for (i, c) in "ABCDEFGHI".chars().enumerate() {
            r.push((format!("vram_{c}"), Src::Vram(i)));
        }
        r.push(("polys".into(), Src::Polys));
        r.push(("pxi".into(), Src::Pxi));
        r.push(("spu".into(), Src::Spu));
        r
    }

    fn describe(s: &Src) -> String {
        match s {
            Src::Fixed(a, n) => format!("{a:#010x}+{n:#x}"),
            Src::Ptr(p, o, n) => format!("*{p:#010x}+{o:#x} len {n:#x}"),
            Src::Dtcm => "DTCM".into(),
            Src::Itcm => "ITCM".into(),
            Src::Io(a, b) => format!("I/O {a:#x}..{b:#x}"),
            Src::Palette => "palette RAM".into(),
            Src::Oam => "OAM".into(),
            Src::Vram(i) => format!("VRAM bank {i}"),
            Src::Polys => "3D: frame_polys (toon resolved) + viewport".into(),
            Src::Pxi => "PXI FIFO words and IPCSYNC nibbles this frame in order".into(),
            Src::Spu => "ARM7 SPU register writes this frame in order".into(),
        }
    }

    /// FNV-style 64-bit hash, 8 bytes per step.
    pub fn hash(mut h: u64, b: &[u8]) -> u64 {
        let mut chunks = b.chunks_exact(8);
        for c in &mut chunks {
            h = (h ^ u64::from_le_bytes(c.try_into().unwrap())).wrapping_mul(0x100_0000_01b3).rotate_left(23);
        }
        for &x in chunks.remainder() {
            h = (h ^ x as u64).wrapping_mul(0x100_0000_01b3);
        }
        (h ^ b.len() as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15)
    }

    /// `--mask FILE`: `0xSTART-0xEND reason` (end exclusive) or `region NAME reason`; `#` comments.
    /// Every entry needs a reason.
    #[derive(Default, Clone)]
    pub struct Mask {
        pub ranges: Vec<(u32, u32, String)>,
        pub regions: Vec<(String, String)>,
    }

    impl Mask {
        pub fn parse(text: &str) -> Result<Mask, String> {
            let mut m = Mask::default();
            for (n, line) in text.lines().enumerate() {
                let line = line.split('#').next().unwrap().trim();
                if line.is_empty() {
                    continue;
                }
                let (head, reason) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
                let reason = reason.trim();
                let num = |s: &str| u32::from_str_radix(s.trim().trim_start_matches("0x"), 16);
                if head == "region" {
                    let (name, why) = reason.split_once(char::is_whitespace).unwrap_or((reason, ""));
                    if why.trim().is_empty() {
                        return Err(format!("mask line {}: region {name} needs a reason", n + 1));
                    }
                    m.regions.push((name.into(), why.trim().into()));
                    continue;
                }
                if reason.is_empty() {
                    return Err(format!("mask line {}: a range needs a reason", n + 1));
                }
                let (a, b) = head.split_once('-').ok_or(format!("mask line {}: expected 0xSTART-0xEND", n + 1))?;
                let (a, b) = (num(a).map_err(|e| e.to_string())?, num(b).map_err(|e| e.to_string())?);
                if b <= a {
                    return Err(format!("mask line {}: empty range", n + 1));
                }
                m.ranges.push((a, b, reason.into()));
            }
            Ok(m)
        }

        /// Zero the masked bytes of `buf`, which holds ARM9 addresses `base..`.
        fn apply(&self, base: u32, buf: &mut [u8]) {
            let end = base as u64 + buf.len() as u64;
            for &(a, b, _) in &self.ranges {
                let (lo, hi) = ((a as u64).max(base as u64), (b as u64).min(end));
                if lo < hi {
                    buf[(lo - base as u64) as usize..(hi - base as u64) as usize].fill(0);
                }
            }
        }
    }

    pub const IRQ_NAMES: [&str; 12] =
        ["vblank", "hblank", "vcount", "tm0", "tm1", "tm2", "tm3", "ipcsync", "ipcrecv", "card", "gxfifo", "other"];

    fn irq_column(bit: usize) -> usize {
        match bit {
            0..=6 => bit,
            16 => 7,
            18 => 8,
            19 => 9,
            21 => 10,
            _ => 11,
        }
    }

    fn env_numbers(name: &str) -> Result<Option<Vec<u32>>, String> {
        let Some(v) = std::env::var_os(name) else { return Ok(None) };
        let v = v.to_string_lossy().to_string();
        v.split(':')
            .map(|p| {
                let p = p.trim();
                match p.strip_prefix("0x") {
                    Some(h) => u32::from_str_radix(h, 16),
                    None => p.parse(),
                }
            })
            .collect::<Result<Vec<_>, _>>()
            .map(Some)
            .map_err(|e| format!("{name}={v}: {e}"))
    }

    /// `inputs.csv` change rows: (logical frame, buttons).
    pub fn read_inputs(path: &Path) -> Result<Vec<(u32, u16)>, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut rows = Vec::new();
        for line in text.lines().skip(1).filter(|l| !l.trim().is_empty()) {
            let mut f = line.split(',');
            let frame = f.next().unwrap_or("").trim().parse().map_err(|e| format!("inputs: {line}: {e}"))?;
            let b = f.next().unwrap_or("").trim().parse().map_err(|e| format!("inputs: {line}: {e}"))?;
            rows.push((frame, b));
        }
        Ok(rows)
    }

    pub struct Recorder {
        regions: Vec<(String, Src)>,
        skip: Vec<bool>,
        mask: Mask,
        frames: BufWriter<std::fs::File>,
        events: BufWriter<std::fs::File>,
        census: BufWriter<std::fs::File>,
        spins: BufWriter<std::fs::File>,
        inputs: BufWriter<std::fs::File>,
        dir: PathBuf,
        last: Option<u32>,
        last_cycles: u64,
        logged_buttons: Option<u16>,
        /// Buttons currently applied (the race loop updates it in closed-loop mode).
        pub buttons: Rc<Cell<u16>>,
        /// Open-loop replay: (logical frame, buttons) change rows.
        script: Option<Vec<(u32, u16)>>,
        pub script_end: u32,
        poke: Option<(u32, u32, u32)>,
        tick: Option<(u32, u16)>,
        dumps: Vec<u32>,
        waits: [u64; 4],
        buf: Vec<u8>,
        pub samples: u64,
        pub error: Option<String>,
    }

    impl Recorder {
        pub fn new(dir: &Path, mask: Mask, replay: Option<Vec<(u32, u16)>>, dumps: Vec<u32>) -> Result<Self, String> {
            let io = |e: std::io::Error| format!("{}: {e}", dir.display());
            std::fs::create_dir(dir).map_err(io)?;
            let regions = regions();
            for (m, _) in &mask.regions {
                if !regions.iter().any(|(n, _)| n == m) {
                    return Err(format!("mask: no region named {m}"));
                }
            }
            let skip = regions.iter().map(|(n, _)| mask.regions.iter().any(|(m, _)| m == n)).collect();
            let mut list = String::from("index,name,covers\n");
            for (i, (n, s)) in regions.iter().enumerate() {
                list += &format!("{i},{n},{}\n", describe(s));
            }
            std::fs::write(dir.join("regions.csv"), list).map_err(io)?;
            let mut mtext = String::new();
            for (a, b, r) in &mask.ranges {
                mtext += &format!("{a:#010x}-{b:#010x} {r}\n");
            }
            for (n, r) in &mask.regions {
                mtext += &format!("region {n} {r}\n");
            }
            std::fs::write(dir.join("mask.txt"), mtext).map_err(io)?;
            let file = |n: &str| std::fs::File::create(dir.join(n)).map(BufWriter::new).map_err(io);
            let mut frames = file("frames.bin")?;
            frames.write_all(b"MKORC001").map_err(io)?;
            frames.write_all(&(regions.len() as u32).to_le_bytes()).map_err(io)?;
            let mut census = file("census.csv")?;
            let mut head = String::from("frame,lag,cycles,swi4,swi5,swi6,cp15_wait");
            for n in IRQ_NAMES {
                head += &format!(",irq_{n}");
            }
            for (_, n) in ds_mem::POLLED_IO {
                head += &format!(",rd_{n}");
            }
            writeln!(census, "{head},spu_writes,pxi_events,spins,spin_instructions").map_err(io)?;
            let mut spins = file("spins.csv")?;
            writeln!(spins, "frame,pc,instructions").map_err(io)?;
            let mut inputs = file("inputs.csv")?;
            writeln!(inputs, "frame,buttons,x,y").map_err(io)?;
            let poke = env_numbers("MKDS_ORACLE_POKE")?
                .map(|v| (v[0], v.get(1).copied().unwrap_or(0), v.get(2).copied().unwrap_or(0)));
            let tick = env_numbers("MKDS_ORACLE_TICK")?.map(|v| (v[0], v.get(1).copied().unwrap_or(1) as u16));
            Ok(Recorder {
                regions,
                skip,
                mask,
                frames,
                events: file("events.bin")?,
                census,
                spins,
                inputs,
                dir: dir.to_path_buf(),
                last: None,
                last_cycles: 0,
                logged_buttons: None,
                buttons: Rc::new(Cell::new(0)),
                script_end: replay.as_ref().and_then(|s| s.last()).map_or(0, |r| r.0),
                script: replay,
                poke,
                tick,
                dumps,
                waits: [0; 4],
                buf: Vec::new(),
                samples: 0,
                error: None,
            })
        }

        pub fn replaying(&self) -> bool {
            self.script.is_some()
        }

        /// Script value for logical frame `f` (the last change row at or before it).
        fn scripted(&self, f: u32) -> Option<u16> {
            let s = self.script.as_ref()?;
            let i = s.partition_point(|r| r.0 <= f);
            Some(if i == 0 { 0 } else { s[i - 1].1 })
        }

        /// Install on the machine: the probe on the bus and the wait hook.
        pub fn install(rec: Rc<RefCell<Recorder>>, m: &mut Machine<'_>) {
            let mut probe = ds_mem::Probe::default();
            probe.log_spu = std::env::var_os("MKDS_RECORD_SPU").is_some();
            m.hw.probe = Some(Box::new(probe));
            let first = rec.borrow().scripted(0);
            if let Some(b) = first {
                m.set_input(b, None);
                rec.borrow().buttons.set(b);
            }
            m.wait_hook = Some(Box::new(move |m, what| {
                let mut r = rec.borrow_mut();
                if r.error.is_none() {
                    if let Err(e) = r.on_wait(m, what) {
                        r.error = Some(e.to_string());
                    }
                }
            }));
        }

        /// Copies `len` bytes at ARM9 address `base` (main RAM or DTCM) into `self.buf`.
        fn read(&mut self, m: &Machine<'_>, base: u32, len: u32) -> bool {
            self.buf.clear();
            self.buf.resize(len as usize, 0);
            let hw = &m.hw;
            if let Some(d) = hw.dtcm_base {
                if base >= d && (base as u64 + len as u64) <= d as u64 + hw.dtcm.len() as u64 {
                    let o = (base - d) as usize;
                    self.buf.copy_from_slice(&hw.dtcm[o..o + len as usize]);
                    return true;
                }
            }
            if base >> 24 != 2 {
                return false;
            }
            let o = base as usize & (ds_mem::MAIN_RAM_SIZE - 1);
            if o + len as usize > ds_mem::MAIN_RAM_SIZE {
                return false;
            }
            self.buf.copy_from_slice(&hw.main[o..o + len as usize]);
            true
        }

        fn word(&mut self, m: &Machine<'_>, a: u32) -> u32 {
            if self.read(m, a, 4) { u32::from_le_bytes(self.buf[..4].try_into().unwrap()) } else { 0 }
        }

        fn on_wait(&mut self, m: &mut Machine<'_>, what: u32) -> std::io::Result<()> {
            self.waits[match what {
                4 => 0,
                5 => 1,
                6 => 2,
                _ => 3,
            }] += 1;
            if what != 5 {
                return Ok(());
            }
            let frame = self.word(m, ds_mem::VBLANK_COUNT);
            if self.last == Some(frame) {
                return Ok(());
            }
            // test perturbations (off unless MKDS_ORACLE_POKE / MKDS_ORACLE_TICK are set)
            if let Some((at, bias)) = self.tick {
                if frame >= at {
                    m.hw.probe.as_mut().unwrap().tm0_bias = bias;
                }
            }
            if let Some((at, addr, value)) = self.poke {
                if frame == at {
                    use recomp::rt::Mem;
                    m.hw.w32(addr, value);
                    eprintln!("oracle: poked {value:#x} at {addr:#010x} in logical frame {frame}");
                }
            }
            m.sync_gx();
            let lag = self.last.map_or(0, |l| frame.wrapping_sub(l).wrapping_sub(1));
            let probe = {
                let p = m.hw.probe.as_mut().unwrap();
                let keep = ds_mem::Probe {
                    log_spu: p.log_spu,
                    tm0_bias: p.tm0_bias,
                    run_pc: p.run_pc,
                    run_len: p.run_len,
                    ..Default::default()
                };
                std::mem::replace(p.as_mut(), keep)
            };
            let cycles = m.hw.cycles;
            let mut rec = Vec::with_capacity(16 + 8 * self.regions.len());
            rec.extend_from_slice(&frame.to_le_bytes());
            rec.extend_from_slice(&((cycles / FRAME_CYCLES) as u32).to_le_bytes());
            rec.extend_from_slice(&cycles.to_le_bytes());
            let mut pxi_hash = 0u64;
            for e in probe.events.iter().filter(|e| e.kind != ds_mem::EV_SPU) {
                let mut b = [0u8; 5];
                b[0] = e.kind;
                b[1..].copy_from_slice(&e.value.to_le_bytes());
                pxi_hash = hash(pxi_hash, &b);
            }
            for i in 0..self.regions.len() {
                let h = if self.skip[i] { 0 } else { self.region_hash(m, i, pxi_hash, &probe) };
                rec.extend_from_slice(&h.to_le_bytes());
            }
            self.frames.write_all(&rec)?;
            for e in &probe.events {
                let mut b = [0u8; 12];
                b[..4].copy_from_slice(&e.frame.to_le_bytes());
                b[4] = e.kind;
                b[5] = e.bytes;
                b[6..8].copy_from_slice(&e.off.to_le_bytes());
                b[8..].copy_from_slice(&e.value.to_le_bytes());
                self.events.write_all(&b)?;
            }
            // census
            let mut irq = [0u64; 12];
            for (b, n) in probe.irq_bits.iter().enumerate() {
                irq[irq_column(b)] += n;
            }
            write!(
                self.census,
                "{frame},{lag},{},{},{},{},{}",
                cycles - self.last_cycles,
                self.waits[0],
                self.waits[1],
                self.waits[2],
                self.waits[3]
            )?;
            for n in irq.iter().chain(&probe.io_reads) {
                write!(self.census, ",{n}")?;
            }
            let pxi = probe.events.iter().filter(|e| e.kind != ds_mem::EV_SPU).count();
            writeln!(
                self.census,
                ",{},{pxi},{},{}",
                probe.spu_writes,
                probe.spins.len(),
                probe.spins.iter().map(|s| s.1).sum::<u64>()
            )?;
            for (pc, n) in &probe.spins {
                writeln!(self.spins, "{frame},{pc:#010x},{n}")?;
            }
            // inputs: what is held now, then (replay) the script's value for the next frame
            let held = self.buttons.get();
            if self.logged_buttons != Some(held) {
                writeln!(self.inputs, "{frame},{held},,")?;
                self.logged_buttons = Some(held);
            }
            if let Some(next) = self.scripted(frame.wrapping_add(1)) {
                if next != held {
                    m.set_input(next, None);
                    self.buttons.set(next);
                }
            }
            if self.dumps.contains(&frame) {
                // diagnostics only: raw RAM goes to the (scratch) output directory, never to the repo
                std::fs::write(self.dir.join(format!("ram_{frame}.bin")), &m.hw.main)?;
            }
            self.waits = [0; 4];
            self.last = Some(frame);
            self.last_cycles = cycles;
            self.samples += 1;
            if self.samples % 600 == 0 {
                self.flush()?;
            }
            Ok(())
        }

        fn masked_hash(&mut self, base: u32, mut buf: Vec<u8>) -> u64 {
            self.mask.apply(base, &mut buf);
            hash(0, &buf)
        }

        fn region_hash(&mut self, m: &Machine<'_>, i: usize, pxi: u64, probe: &ds_mem::Probe) -> u64 {
            let src = self.regions[i].1.clone();
            let hw = &m.hw;
            let mem = |s: &mut Self, base: u32, len: u32| -> u64 {
                if !s.read(m, base, len) {
                    return 1;
                }
                let mut buf = std::mem::take(&mut s.buf);
                s.mask.apply(base, &mut buf);
                let h = hash(0, &buf);
                s.buf = buf;
                h
            };
            match src {
                Src::Fixed(a, n) => mem(self, a, n),
                Src::Ptr(p, o, n) => {
                    let base = self.word(m, p);
                    if base >> 24 != 2 { 2 } else { mem(self, base.wrapping_add(o), n) }
                }
                Src::Dtcm => {
                    let d = hw.dtcm_base.unwrap_or(0x027e_0000);
                    self.masked_hash(d, hw.dtcm.clone())
                }
                Src::Itcm => self.masked_hash(0x0100_0000, hw.itcm.clone()),
                Src::Io(a, b) => self.masked_hash(ds_mem::IO_BASE + a as u32, hw.io[a..b].to_vec()),
                Src::Palette => hash(0, &hw.palette),
                Src::Oam => hash(0, &hw.oam),
                Src::Vram(b) => hash(0, &hw.vram[b]),
                Src::Polys => {
                    let mut polys = m.frame_polys.clone();
                    recomp::gx::resolve_toon(&mut polys, &hw.io);
                    let mut h = hash(0, &m.frame_viewport);
                    let mut b = Vec::new();
                    for p in &polys {
                        b.clear();
                        for v in [p.attr, p.tex_param, p.pltt_base, p.verts.len() as u32] {
                            b.extend_from_slice(&v.to_le_bytes());
                        }
                        for v in &p.verts {
                            for c in v.clip {
                                b.extend_from_slice(&c.to_le_bytes());
                            }
                            b.extend_from_slice(&v.color.to_le_bytes());
                            b.extend_from_slice(&v.st[0].to_le_bytes());
                            b.extend_from_slice(&v.st[1].to_le_bytes());
                        }
                        h = hash(h, &b);
                    }
                    h
                }
                Src::Pxi => pxi,
                Src::Spu => probe.spu_hash ^ probe.spu_writes,
            }
        }

        pub fn last_frame(&self) -> Option<u32> {
            self.last
        }

        pub fn flush(&mut self) -> std::io::Result<()> {
            self.frames.flush()?;
            self.events.flush()?;
            self.census.flush()?;
            self.spins.flush()?;
            self.inputs.flush()
        }
    }
}
