//! Native host for the locally generated game. Call `run` with the generated lift table.
mod audio;
pub mod assets;
pub mod gpu;
mod pad;
pub mod window;
mod script;
use mkds_builder::controls::{self, Controls};
use mkds_builder::settings::{Layout, Renderer, Settings};
use window::HostLink;
use recomp::{boot::session::Session, dispatch::Dispatcher, mem::Lifted, rt::Mem};
use std::{
    cell::RefCell,
    path::PathBuf,
    rc::Rc,
    sync::{
        Arc, mpsc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

#[derive(Default, Debug, Clone)]
pub struct Options {
    /// Explicit oracle fallback for ARM7 integration diagnostics; never enable in acceptance.
    pub diagnostic_arm7_interpreter: bool,
    pub headless: bool,
    pub mute: bool,
    pub instruction_limit: Option<u64>,
    pub frame_limit: Option<u64>,
    pub capture_dir: Option<PathBuf>,
    /// Write the renderer input (`MKSNAP01`) every `snapshot_every` frames into this folder (local diagnostics:
    /// snapshots contain game graphics).
    pub snapshots: Option<PathBuf>,
    pub snapshot_every: Option<u64>,
    pub input_script: Option<PathBuf>,
    pub gx_trace: Option<PathBuf>,
    /// Print once per second: emulated vs wall time, frames presented/rendered, audio underruns.
    pub stats: bool,
    /// Write every button change as `wall_s,guest_frame,guest_s,buttons` (diagnoses double/missed taps).
    pub input_log: Option<PathBuf>,
    /// Keyboard controls file (default: the user's `controls.txt`, see `mkds_builder::controls`).
    pub controls: Option<PathBuf>,
    /// `--renderer software|gpu`; `None` = from `settings.txt`. Software is the block 1 CPU renderer (the GPU's reference).
    pub software_renderer: Option<bool>,
    /// `--scale N`: 3D and screen resolution as a multiple of the DS's 256x192 (GPU renderer, 1-8).
    pub scale: Option<u32>,
}

impl Options {
    pub fn from_args(args: impl IntoIterator<Item = String>) -> Result<Self, String> {
        let mut result = Self::default();
        let mut args = args.into_iter();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--headless" => result.headless = true,
                "--mute" => result.mute = true,
                "--stats" => result.stats = true,
                "--diagnostic-arm7-interpreter" => result.diagnostic_arm7_interpreter = true,
                "--renderer" => match args.next().as_deref() {
                    Some("gpu") => result.software_renderer = Some(false),
                    Some("software") => result.software_renderer = Some(true),
                    _ => return Err("--renderer needs gpu or software".into()),
                },
                "--scale" => {
                    let n = positive(args.next(), "--scale")?;
                    if n > 8 {
                        return Err("--scale must be 1-8".into());
                    }
                    result.scale = Some(n as u32);
                }
                "--controls" => {
                    result.controls = Some(args.next().ok_or("--controls needs a file path")?.into())
                }
                "--input-log" => {
                    result.input_log =
                        Some(args.next().ok_or("--input-log needs a file path")?.into())
                }
                "--instructions" => {
                    result.instruction_limit = Some(positive(args.next(), "--instructions")?)
                }
                "--frames" => result.frame_limit = Some(positive(args.next(), "--frames")?),
                "--snapshots" => {
                    result.snapshots = Some(args.next().ok_or("--snapshots needs a directory")?.into())
                }
                "--snapshot-every" => {
                    result.snapshot_every = Some(positive(args.next(), "--snapshot-every")?)
                }
                "--capture" => {
                    result.capture_dir =
                        Some(args.next().ok_or("--capture needs a directory")?.into())
                }
                "--input-script" => {
                    result.input_script =
                        Some(args.next().ok_or("--input-script needs a CSV path")?.into())
                }
                "--gx-trace" => {
                    result.gx_trace =
                        Some(args.next().ok_or("--gx-trace needs a file path")?.into())
                }
                _ => return Err(format!("Unknown runtime option: {arg}")),
            }
        }
        if result.headless && result.instruction_limit.is_none() {
            return Err("--headless requires --instructions as a finite diagnostic bound".into());
        }
        Ok(result)
    }
}

/// Once-per-second host pacing diagnostics (`--stats`).
struct PacingStats {
    wall_mark: Instant,
    emulated_mark: Duration,
    frame_mark: u64,
    rendered: u64,
    presented: u64,
    parity_ok: u64,
    /// Time the emulation thread spent copying render snapshots this interval.
    snapshot_time: Duration,
    render_time: Duration,
    /// 3D raster, 2D engine A (+3D layer), 2D engine B, accumulated this interval.
    parts: [Duration; 3],
    audio_mark: (u64, u64),
    /// (lifted, interpreted, overlay) ARM9 instruction counters at the last report.
    instr_mark: (u64, u64, u64),
}

impl PacingStats {
    fn new(start: Instant) -> Self {
        Self {
            wall_mark: start,
            emulated_mark: Duration::ZERO,
            frame_mark: 0,
            rendered: 0,
            presented: 0,
            parity_ok: 0,
            snapshot_time: Duration::ZERO,
            render_time: Duration::ZERO,
            parts: [Duration::ZERO; 3],
            audio_mark: (0, 0),
            instr_mark: (0, 0, 0),
        }
    }

    fn report(
        &mut self,
        emulated: Duration,
        frame: u64,
        audio: Option<(u64, u64, usize)>,
        instr: (u64, u64, u64),
        link: &window::HostLink,
    ) {
        let wall = self.wall_mark.elapsed();
        if wall < Duration::from_secs(1) {
            return;
        }
        let (gpu_2d, gpu_frames, uploaded) = link.take_2d_stats();
        if gpu_frames != 0 {
            eprintln!("[stats] GPU 2D upload + compose submission CPU {:.3} ms/frame, {:.1} KiB/frame, {} frames", avg_ms(gpu_2d, gpu_frames), uploaded as f64 / 1024.0 / gpu_frames as f64, gpu_frames);
        }
        let emu = emulated.saturating_sub(self.emulated_mark);
        let (dl, di, dov) = (
            instr.0 - self.instr_mark.0,
            instr.1 - self.instr_mark.1,
            instr.2 - self.instr_mark.2,
        );
        self.instr_mark = instr;
        let frames = frame.saturating_sub(self.frame_mark);
        let audio_text = audio.map_or(String::from("muted"), |(u, d, queued)| {
            let s = format!(
                "underruns +{} dropped +{} queued {}",
                u - self.audio_mark.0,
                d - self.audio_mark.1,
                queued
            );
            self.audio_mark = (u, d);
            s
        });
        eprintln!(
            "[stats] emulated {:.2}s in wall {:.2}s (speed {:.0}%), frames {} ({:.1}/s), presented {}, rendered {} (avg {:.1} ms: 3D {:.1}, 2D-A {:.1}, 2D-B {:.1}), audio {}, parity checks ok {}, snapshot {:.1} ms/s, ARM9 M-instr lifted {:.1} interpreted {:.2} overlay {:.2}",
            emu.as_secs_f64(),
            wall.as_secs_f64(),
            100.0 * emu.as_secs_f64() / wall.as_secs_f64(),
            frames,
            frames as f64 / wall.as_secs_f64(),
            self.presented,
            self.rendered,
            avg_ms(self.render_time, self.rendered),
            avg_ms(self.parts[0], self.rendered),
            avg_ms(self.parts[1], self.rendered),
            avg_ms(self.parts[2], self.rendered),
            audio_text,
            self.parity_ok,
            self.snapshot_time.as_secs_f64() * 1000.0 / wall.as_secs_f64(),
            dl as f64 / 1e6,
            di as f64 / 1e6,
            dov as f64 / 1e6
        );
        self.wall_mark = Instant::now();
        self.emulated_mark = emulated;
        self.frame_mark = frame;
        self.rendered = 0;
        self.presented = 0;
        self.snapshot_time = Duration::ZERO;
        self.render_time = Duration::ZERO;
        self.parts = [Duration::ZERO; 3];
    }
}

fn avg_ms(total: Duration, n: u64) -> f64 {
    if n == 0 { 0.0 } else { total.as_secs_f64() * 1000.0 / n as f64 }
}

// The core owns all rendering; the host only transports snapshots and presents pixels.
use recomp::machine::RenderSnapshot;

/// A finished render job: the frame for the window and its CPU cost (total; 3D, 2D-A, 2D-B).
struct Done {
    frame: window::Frame,
    took: Duration,
    parts: [Duration; 3],
}

/// Spawns the render worker. `busy` is set by the caller when it submits a job and cleared here
/// when the frame is done, so at most one frame is ever in flight (a busy worker means "skip").
fn spawn_renderer(
    busy: Arc<AtomicBool>,
    gpu: bool,
) -> (mpsc::Sender<RenderSnapshot>, mpsc::Receiver<Done>) {
    let (job_tx, job_rx) = mpsc::channel::<RenderSnapshot>();
    let (done_tx, done_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut capture = recomp::machine::CaptureCarry::default();
        for job in job_rx {
            let done = if gpu {
                // Ownership transfer only: raw inputs are decoded and composed by the window's GPU.
                Done { frame: window::Frame::Gpu(Box::new(window::GpuFrame { snap: job })), took: Duration::ZERO, parts: [Duration::ZERO; 3] }
            } else {
                let mut done = job.screens_profiled();
                capture.apply(&job, &mut done.pixels);
                drop(job);
                Done { frame: window::Frame::Pixels(done.pixels), took: done.took, parts: done.parts }
            };
            let _ = done_tx.send(done);
            busy.store(false, Ordering::Release);
        }
    });
    (job_tx, done_rx)
}

fn positive(value: Option<String>, flag: &str) -> Result<u64, String> {
    value
        .and_then(|s| s.parse().ok())
        .filter(|n| *n > 0)
        .ok_or_else(|| format!("{flag} needs a positive integer"))
}

#[derive(Debug)]
pub struct RunReport {
    pub arm7_lifted_cycles: u64,
    pub arm7_interpreted_instructions: u64,
    pub instructions: u64,
    pub lifted_instructions: u64,
    pub interpreted_instructions: u64,
    pub frames: u64,
    pub arm9_pc: u32,
    pub arm7_pc: u32,
    pub overlay_instructions: u64,
}

/// Run the exact user-ROM image with the caller's locally generated lift table.
/// The caller validates the ROM hash against generation provenance before calling.
/// Run using only verified loose assets; the source ROM is not accessed.
pub fn run_from_assets(
    path: &std::path::Path,
    expected_rom_sha256: &str,
    expected_assets_sha256: &str,
    table: &'static [Lifted],
    signatures: &'static [recomp::residency::SegmentSig],
    options: Options,
) -> Result<RunReport, String> {
    let assets = assets::load(path, expected_rom_sha256, expected_assets_sha256)?;
    eprintln!("Verified loose assets; manifest sha256={}", assets.manifest_sha256);
    run_with_signatures(&assets.card, &assets.arm9, table, signatures, options)
}

pub fn run(
    rom: &[u8],
    image: &[u8],
    table: &'static [Lifted],
    options: Options,
) -> Result<RunReport, String> {
    run_with_signatures(rom, image, table, &[], options)
}

/// Dual-CPU integration entry. Supply the verified cycle-charged ARM7 table.
pub fn run_from_assets_dual(
    path: &std::path::Path,
    expected_rom_sha256: &str,
    expected_assets_sha256: &str,
    table: &'static [Lifted],
    signatures: &'static [recomp::residency::SegmentSig],
    table7: &'static [Lifted],
    options: Options,
) -> Result<RunReport, String> {
    let assets = assets::load(path, expected_rom_sha256, expected_assets_sha256)?;
    eprintln!("Verified loose assets; manifest sha256={}", assets.manifest_sha256);
    run_with_dual_tables(&assets.card, &assets.arm9, table, signatures, table7, options)
}

pub fn run_with_dual_tables(
    rom: &[u8], image: &[u8], table: &'static [Lifted],
    signatures: &'static [recomp::residency::SegmentSig],
    table7: &'static [Lifted], options: Options,
) -> Result<RunReport, String> {
    if table7.is_empty() { return Err("Dual-CPU execution requires an ARM7 lift table".into()); }
    run_internal(rom, image, table, signatures, Some(table7), options)
}

/// Generated games pass their exact-ROM `lifted::SEGMENT_SIGS` to enable verified overlays.
pub fn run_with_signatures(
    rom: &[u8],
    image: &[u8],
    table: &'static [Lifted],
    signatures: &'static [recomp::residency::SegmentSig],
    options: Options,
) -> Result<RunReport, String> {
    run_internal(rom, image, table, signatures, None, options)
}

fn run_internal(
    rom: &[u8], image: &[u8], table: &'static [Lifted],
    signatures: &'static [recomp::residency::SegmentSig],
    table7: Option<&'static [Lifted]>, options: Options,
) -> Result<RunReport, String> {
    if options.headless && options.instruction_limit.is_none() {
        return Err("Headless diagnostics require a finite instruction limit".into());
    }
    if options.headless {
        return emulate(rom, image, table, signatures, table7, &options, None, Bindings::from(&Controls::default()));
    }
    let bindings = load_bindings(&options)?;
    let title = bindings.title.clone();
    let settings = load_settings()?;
    let mut options = options;
    options.software_renderer = Some(options.software_renderer.unwrap_or(settings.renderer == Renderer::Software));
    let display = window::Display {
        scale: options.scale.or(settings.scale),
        smooth: settings.smooth,
        side_by_side: settings.layout == Layout::SideBySide,
        fullscreen: settings.fullscreen,
        texture_pack: settings.texture_pack.clone(),
        dump_textures: settings.dump_textures.clone(),
    };
    // The window needs the main thread; the game runs on a second one and meets it through `HostLink`.
    window::run(&title, display, move |link| emulate(rom, image, table, signatures, table7, &options, Some(link), bindings))?
}

#[allow(clippy::too_many_arguments)]
fn emulate(
    rom: &[u8], image: &[u8], table: &'static [Lifted],
    signatures: &'static [recomp::residency::SegmentSig],
    table7: Option<&'static [Lifted]>, options: &Options, window: Option<Arc<HostLink>>, bindings: Bindings,
) -> Result<RunReport, String> {
    // Never pick the first of several overlapping overlays. Until residency is tracked,
    // they execute from actual guest memory via the interpreter and count as fallback.
    let resident: Vec<_> = table
        .iter()
        .map(|l| l.segment)
        .filter(|s| *s == "arm9_main" || *s == "bios9" || s.starts_with("arm9_autoload"))
        .collect();
    let dispatcher = Dispatcher::with_loaded(table, &resident);
    dispatcher.set_natives(recomp::native::selected());
    let mut session = Session::new(rom, image, &dispatcher)?;
    // Headless `--stats` prints a per-stage cost profile; the clock is only read when asked for.
    session.machine.profile_time = options.headless && options.stats;
    if let Some(table7) = table7 {
        session.machine.arm7.as_mut().ok_or("ARM7 missing from session")?
            .configure_lifted(table7, options.diagnostic_arm7_interpreter);
    }
    session.configure_residency(signatures)?;
    // Block 4A.2: the ARM9 runs in fibers (nested calls; proven identical to the flat loop over a
    // full race). MKDS_FLAT=1 restores the flat loop.
    if std::env::var_os("MKDS_FLAT").is_none() {
        session.enable_fibers();
    }
    if let Some(path) = &options.gx_trace {
        session
            .machine
            .start_gx_trace(path)
            .map_err(|e| format!("Cannot create GX trace {}: {e}", path.display()))?;
    }
    let mut scripted = options
        .input_script
        .as_ref()
        .map(|path| {
            let text = std::fs::read_to_string(path)
                .map_err(|e| format!("Cannot read input script {}: {e}", path.display()))?;
            script::Script::parse(&text)
        })
        .transpose()?;
    eprintln!(
        "Runtime: {} generated functions; {} overlay signatures (unsigned overlays remain interpreted)",
        table.len(),
        signatures.len()
    );
    let bindings = Rc::new(bindings);
    let pads = Rc::new(RefCell::new(if window.is_some() { pad::Pads::new() } else { pad::Pads::none() }));
    let sound = if options.mute || options.headless {
        None
    } else {
        Some(audio::Output::new()?)
    };
    if let Some(sound) = &sound {
        // The sound unit mixes straight at the device's rate: no second resampling on the way out.
        session.machine.set_audio_rate(sound.rate());
        eprintln!("Audio: {} Hz", sound.rate());
    }
    let start = Instant::now();
    let idle_error = Rc::new(RefCell::new(None::<String>));
    if let Some(path) = &options.input_log {
        let mut log = std::io::BufWriter::new(std::fs::File::create(path)
            .map_err(|e| format!("Cannot create input log {}: {e}", path.display()))?);
        let error = Rc::clone(&idle_error);
        let mut last = u16::MAX;
        session.machine.input_observer = Some(Box::new(move |cycles, buttons| {
            if buttons == last { return; }
            use std::io::Write;
            let result = writeln!(log, "{:.3},{},{:.3},{:#06x}",
                start.elapsed().as_secs_f64(),
                cycles / recomp::boot::session::FRAME_CYCLES,
                cycles as f64 / recomp::ds_mem::BUS_HZ as f64, buttons)
                .and_then(|_| log.flush());
            if let Err(e) = result { *error.borrow_mut() = Some(format!("Input log failed: {e}")); }
            last = buttons;
        }));
    }
    if let Some(window) = &window {
        let w = Arc::clone(window);
        let live_input = scripted.is_none();
        let bindings = Rc::clone(&bindings);
        let pads = Rc::clone(&pads);
        let mut last_poll = Instant::now();
        session.machine.idle_poll = Some(Box::new(move || {
            if last_poll.elapsed() < Duration::from_millis(8) {
                return None;
            }
            last_poll = Instant::now();
            let mut pads = pads.borrow_mut();
            pads.poll();
            let quit = !w.is_open() || bindings.quit(&w, &pads);
            let (buttons, touch) = if live_input { input(&w, &bindings, &pads) } else { (0, None) };
            Some(recomp::machine::IdleAction {
                quit,
                buttons: live_input.then_some(buttons),
                touch,
            })
        }));
    }
    let mut last_frame = u64::MAX;
    let mut last_snapshot = u64::MAX;
    if let Some(dir) = &options.snapshots {
        std::fs::create_dir_all(dir).map_err(|e| format!("Cannot create {}: {e}", dir.display()))?;
    }
    let busy = Arc::new(AtomicBool::new(false));
    let renderer = window.is_some().then(|| spawn_renderer(busy.clone(), options.software_renderer != Some(true)));
    // Wall-clock time spent re-anchoring the schedule after a slow stretch (no catch-up sprint).
    let mut skew = Duration::ZERO;
    let mut parity: Option<Vec<u32>> = None;
    let mut jobs = 0u64;
    let mut stats = PacingStats::new(start);
    let mut arm7_stats_mark = (start, 0u64, 0u64);
    let mut next_progress = 10_000_000;
    let (mut prof_frame, mut prof_wall, mut prof_mix) = (0u64, Instant::now(), (0u64, 0u64, 0u64));
    let outcome =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<(), String> {
            loop {
                if options
                    .instruction_limit
                    .is_some_and(|n| session.stats.instructions >= n)
                    || options
                        .frame_limit
                        .is_some_and(|n| session.frame_number() >= n)
                {
                    break;
                }
                if let Some(script) = scripted.as_mut() {
                    let snapshot = script.at(session.frame_number());
                    session.machine.set_input(snapshot.buttons, snapshot.touch);
                }
                if let Some(w) = window.as_ref() {
                    let mut pads = pads.borrow_mut();
                    pads.poll();
                    if !w.is_open() || bindings.quit(w, &pads) {
                        break;
                    }
                    if scripted.is_none() {
                        let (buttons, touch) = input(w, &bindings, &pads);
                        session.machine.set_input(buttons, touch);
                    }
                }
                let remaining = options.instruction_limit.map_or(16_384, |n| {
                    n.saturating_sub(session.stats.instructions).min(16_384) as u32
                });
                session.advance(remaining)?;
                if let Some(dir) = &options.snapshots {
                    let frame = session.frame_number();
                    if frame != last_snapshot && frame % options.snapshot_every.unwrap_or(60) == 0 {
                        last_snapshot = frame;
                        let path = dir.join(format!("f{frame:06}.mksnap"));
                        std::fs::write(&path, session.render_snapshot().encode())
                            .map_err(|e| format!("Cannot write snapshot {}: {e}", path.display()))?;
                    }
                }
                if options.stats && arm7_stats_mark.0.elapsed() >= Duration::from_secs(1) {
                    if let Some(a) = &session.machine.arm7 {
                        eprintln!("[stats] ARM7 lifted_cycles={} interpreted_instructions={} pc={:#010x}",
                            a.lifted_cycles - arm7_stats_mark.1,
                            a.interpreted_instructions - arm7_stats_mark.2, a.pc);
                        arm7_stats_mark = (Instant::now(), a.lifted_cycles, a.interpreted_instructions);
                    }
                }
                if let Some(error) = idle_error.borrow_mut().take() {
                    return Err(error);
                }
                if session.machine.host_quit {
                    break;
                }
                if let Some(error) = session.machine.gx_trace_error() {
                    return Err(error.to_owned());
                }
                if options.headless && session.stats.instructions >= next_progress {
                    eprintln!(
                        "[{}M] frame {} ARM9 {:#010x}, ARM7 {:#010x}, GX writes {}",
                        session.stats.instructions / 1_000_000,
                        session.frame_number(),
                        session.pc,
                        session.machine.arm7.as_ref().map_or(0, |a| a.pc),
                        session.machine.gx_writes
                    );
                    next_progress = session.stats.instructions + 10_000_000;
                }
                if options.headless && options.stats {
                    // Headless cost profile: wall time per ~60 guest frames (>1.0 s per guest second
                    // means slower than real time) with the instruction mix of that stretch.
                    let frame = session.frame_number();
                    if frame >= prof_frame + 60 {
                        let s = &session.stats;
                        let now = Instant::now();
                        eprintln!(
                            "[prof] ns {:?} frame {frame} wall {:.2}s/guest {:.2}s, M-instr lifted {:.2} interp {:.3} overlay {:.3}",
                            session.machine.prof_ns.map(|n| n / 1_000_000),
                            now.duration_since(prof_wall).as_secs_f64(),
                            (frame - prof_frame) as f64 / 59.8261,
                            (s.lifted_instructions - prof_mix.0) as f64 / 1e6,
                            (s.interpreted_instructions - prof_mix.1) as f64 / 1e6,
                            (s.overlay_instructions - prof_mix.2) as f64 / 1e6,
                        );
                        session.machine.prof_ns = [0; 3];
                        prof_frame = frame;
                        prof_wall = now;
                        prof_mix = (s.lifted_instructions, s.interpreted_instructions, s.overlay_instructions);
                    }
                }
                let samples = session.machine.take_audio();
                if let Some(sound) = &sound {
                    sound.push(samples)?;
                }
                if let Some(w) = window.as_ref() {
                    let frame = session.frame_number();
                    let emulated = Duration::from_secs_f64(
                        session.machine.hw.cycles as f64 / recomp::ds_mem::BUS_HZ as f64,
                    );
                    // A slow stretch must not turn into a catch-up sprint (that overflows audio
                    // and starves video): past 100 ms behind, move the schedule instead.
                    let lag = start.elapsed().saturating_sub(skew).saturating_sub(emulated);
                    if lag > Duration::from_millis(100) {
                        skew += lag - Duration::from_millis(20);
                    }
                    // Frames drawn by the render worker arrive here; the newest is shown.
                    if let Some((_, done_rx)) = &renderer {
                        while let Ok(done) = done_rx.try_recv() {
                            stats.rendered += 1;
                            stats.render_time += done.took;
                            for (total, part) in stats.parts.iter_mut().zip(done.parts) {
                                *total += part;
                            }
                            if let (Some(expected), window::Frame::Pixels(pixels)) = (parity.take(), &done.frame) {
                                if &expected != pixels {
                                    eprintln!("[stats] RENDER PARITY MISMATCH: worker frame differs from Session::screens()");
                                } else {
                                    stats.parity_ok += 1;
                                }
                            }
                            w.present(done.frame);
                            stats.presented += 1;
                        }
                    }
                    // Every new guest frame goes to the render worker as soon as it is idle. This must not
                    // depend on the presentation timer below: the pacing wait re-presents the last image,
                    // which resets that timer, and gating submission on it starved the worker (8-34
                    // rendered frames/s at full game speed).
                    if let Some((job_tx, _)) = &renderer {
                        if frame != last_frame && !busy.load(Ordering::Acquire) {
                            busy.store(true, Ordering::Release);
                            let snap_started = Instant::now();
                            let job = session.render_snapshot();
                            stats.snapshot_time += snap_started.elapsed();
                            if options.stats && options.software_renderer == Some(true) && jobs % 120 == 0 {
                                parity = Some(session.screens());
                            }
                            jobs += 1;
                            if job_tx.send(job).is_err() {
                                return Err("Render worker stopped".into());
                            }
                            last_frame = frame;
                        }
                    }
                    if options.stats {
                        stats.report(
                            emulated,
                            session.frame_number(),
                            sound.as_ref().map(|s| s.counters()),
                            (
                                session.stats.lifted_instructions,
                                session.stats.interpreted_instructions,
                                session.stats.overlay_instructions,
                            ),
                            w,
                        );
                    }
                    // Hold emulation to real time. One step can move guest time forward by many
                    // milliseconds (the guest idling on a load), so wait until the wall clock
                    // catches up instead of sleeping a fixed 2 ms per step (the window has its own thread).
                    while let Some(ahead) = emulated.checked_sub(start.elapsed().saturating_sub(skew)) {
                        if ahead < Duration::from_millis(1) || !w.is_open() || bindings.quit(w, &pads.borrow()) {
                            break;
                        }
                        std::thread::sleep(ahead.min(Duration::from_millis(4)));
                    }
                }
            }
            Ok(())
        }));
    if let Some(dir) = &options.capture_dir {
        capture(&mut session, dir)?;
    }
    session.machine.finish_gx_trace()?;
    match outcome {
        Ok(result) => result?,
        Err(e) => {
            let why = e
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| e.downcast_ref::<&str>().copied())
                .unwrap_or("unknown runtime panic");
            return Err(format!(
                "Runtime stopped at ARM9 PC {:#010x}, ARM7 PC {:#010x}: {why}",
                session.pc,
                session.machine.arm7.as_ref().map_or(0, |a| a.pc)
            ));
        }
    }
    let report = RunReport {
        arm7_lifted_cycles: session.machine.arm7.as_ref().map_or(0, |a| a.lifted_cycles),
        arm7_interpreted_instructions: session.machine.arm7.as_ref().map_or(0, |a| a.interpreted_instructions),
        instructions: session.stats.instructions,
        lifted_instructions: session.stats.lifted_instructions,
        interpreted_instructions: session.stats.interpreted_instructions,
        frames: session.frame_number(),
        arm9_pc: session.pc,
        arm7_pc: session.machine.arm7.as_ref().map_or(0, |a| a.pc),
        overlay_instructions: session.stats.overlay_instructions,
    };
    eprintln!(
        "Runtime report: {report:?}; elapsed {:?}; audio {:?}; GX writes {}",
        start.elapsed(),
        session.machine.audio_stats,
        session.machine.gx_writes
    );
    eprintln!(
        "GX: {} completed polygons, {} pending, {} swaps; VRAMCNT {:02x?}",
        session.machine.frame_polys.len(),
        session.machine.gx.polygons.len(),
        session.machine.gx.swaps,
        [
            0x240, 0x241, 0x242, 0x243, 0x244, 0x245, 0x246, 0x248, 0x249
        ]
        .map(|i| session.machine.hw.io[i])
    );
    eprintln!(
        "Display A={:#010x} B={:#010x}; PXI ARM9={:#x} ARM7={:#x}",
        session.machine.hw.r32(0x0400_0000),
        session.machine.hw.r32(0x0400_1000),
        session.machine.hw.r32(0x027f_ff88),
        session.machine.hw.r32(0x027f_ff8c)
    );
    for (service, count) in &session.machine.unhandled {
        eprintln!("ARM9 unhandled {service}: {count}");
    }
    if let Some(a) = &session.machine.arm7 {
        for (service, count) in &a.s.unhandled {
            eprintln!("ARM7 unhandled {service}: {count}");
        }
    }
    Ok(report)
}

/// A real D-pad cannot press opposite directions; a keyboard can (fast steering rolls Left+Right).
/// Resolve each opposing pair to the most recently pressed direction ("last input wins"), so the game
/// only ever sees inputs the hardware can produce. `state` = (previous raw, previous resolved).
fn dpad_resolve(raw: u16, state: &mut (u16, u16)) -> u16 {
    const RIGHT: u16 = 1 << 4;
    const LEFT: u16 = 1 << 5;
    const UP: u16 = 1 << 6;
    const DOWN: u16 = 1 << 7;
    let (prev_raw, prev_out) = *state;
    let mut out = raw;
    for (a, b) in [(RIGHT, LEFT), (UP, DOWN)] {
        if raw & (a | b) != a | b {
            continue;
        }
        let keep = if prev_raw & (a | b) == a {
            b // `b` was pressed while `a` was held
        } else if prev_raw & (a | b) == b {
            a
        } else if prev_out & (a | b) != 0 {
            prev_out & (a | b) // both held already: keep the previous choice
        } else {
            a
        };
        out = (out & !(a | b)) | keep;
    }
    *state = (raw, out);
    out
}

thread_local! {
    static DPAD: std::cell::Cell<(u16, u16)> = const { std::cell::Cell::new((0, 0)) };
}

/// `controls.txt` keys (`controls::KEYS` indexes) for the DS buttons (bit order A B Select Start Right Left Up Down
/// R L X Y) and for Quit.
struct Bindings {
    buttons: [Vec<usize>; 12],
    quit_keys: Vec<usize>,
    /// Controller buttons (`controls::PAD` indexes) per DS button, and for Quit.
    pad: [Vec<usize>; 12],
    quit_pad: Vec<usize>,
    title: String,
}

impl From<&Controls> for Bindings {
    fn from(c: &Controls) -> Self {
        let hint = |a: usize| c.describe(a);
        Self {
            buttons: std::array::from_fn(|a| c.keys[a].clone()),
            quit_keys: c.keys[controls::QUIT].clone(),
            pad: std::array::from_fn(|a| c.pad[a].clone()),
            quit_pad: c.pad[controls::QUIT].clone(),
            title: format!(
                "MKDS | A={} B={} L={} R={} Start={} | mouse = touch | F11 = fullscreen | {} = quit",
                hint(0), hint(1), hint(9), hint(8), hint(3), hint(controls::QUIT)
            ),
        }
    }
}

impl Bindings {
    fn quit(&self, window: &HostLink, pads: &pad::Pads) -> bool {
        window.any_down(&self.quit_keys) || window.focused() && pads.any_down(&self.quit_pad)
    }
}

fn load_settings() -> Result<Settings, String> {
    let Some(path) = mkds_builder::settings::default_path() else {
        return Ok(Settings::default());
    };
    let s = Settings::load_or_create(&path)
        .map_err(|e| format!("Settings file problem: {e}
Fix it, or delete it to get the default settings back."))?;
    eprintln!("Settings: {}", path.display());
    Ok(s)
}

fn load_bindings(options: &Options) -> Result<Bindings, String> {
    let Some(path) = options.controls.clone().or_else(controls::default_path) else {
        eprintln!("Controls: defaults (no settings folder found)");
        return Ok(Bindings::from(&Controls::default()));
    };
    let c = Controls::load_or_create(&path)
        .map_err(|e| format!("Controls file problem: {e}
Fix it, or delete it to get the default controls back."))?;
    eprintln!("Controls: {}", path.display());
    Ok(Bindings::from(&c))
}

fn input(window: &HostLink, bindings: &Bindings, pads: &pad::Pads) -> (u16, Option<(u8, u8)>) {
    if !window.focused() {
        return (0, None); // an inactive window releases all guest input
    }
    let buttons = (0..12).fold(0, |bits, i| {
        bits | if window.any_down(&bindings.buttons[i]) || pads.any_down(&bindings.pad[i]) { 1 << i } else { 0 }
    });
    let buttons = DPAD.with(|d| {
        let mut state = d.get();
        let out = dpad_resolve(buttons, &mut state);
        d.set(state);
        out
    });
    (buttons, window.touch())
}

fn capture(session: &mut Session<'_>, dir: &std::path::Path) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("Cannot create capture directory: {e}"))?;
    let pixels = session.screens();
    for (name, screen) in ["top.ppm", "bottom.ppm"]
        .into_iter()
        .zip(pixels.chunks(256 * 192))
    {
        let rgba: Vec<_> = screen.iter().map(|p| (p << 8) | 255).collect();
        std::fs::write(dir.join(name), recomp::gfx2d::to_ppm(&rgba))
            .map_err(|e| format!("Cannot write capture: {e}"))?;
    }
    // Private local renderer evidence, never part of a distributable tool bundle.
    for (name, bytes) in [
        ("io.bin", &session.machine.hw.io),
        ("palette.bin", &session.machine.hw.palette),
        ("oam.bin", &session.machine.hw.oam),
    ] {
        std::fs::write(dir.join(name), bytes).map_err(|e| format!("Cannot write snapshot: {e}"))?;
    }
    for (i, bytes) in session.machine.hw.vram.iter().enumerate() {
        std::fs::write(dir.join(format!("vram_{i}.bin")), bytes)
            .map_err(|e| format!("Cannot write snapshot: {e}"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn default_bindings_read_the_documented_keys_through_the_window_link() {
        let key = |n: &str| controls::key_index(n).unwrap();
        let b = Bindings::from(&Controls::default());
        assert_eq!(b.buttons[9], [key("A")], "L (use item) is keyboard A");
        let link = HostLink::for_tests();
        let pads = pad::Pads::none();
        link.set_for_tests(&[key("Z"), key("Left"), key("A")], Some((10, 20)), true);
        assert_eq!(input(&link, &b, &pads), (1 | 32 | 1 << 9, Some((10, 20))));
        assert!(!b.quit(&link, &pads));
        link.set_for_tests(&[key("Z"), key("Escape")], Some((10, 20)), false);
        assert_eq!(input(&link, &b, &pads), (0, None), "inactive window releases all guest input");
        assert!(!b.quit(&link, &pads), "keys of an inactive window do not count");
        link.set_for_tests(&[key("Escape")], None, true);
        assert!(b.quit(&link, &pads));
    }

    #[test]
    fn headless_diagnostics_must_be_bounded_and_options_must_be_valid() {
        let parse = |s: &str| Options::from_args(s.split_whitespace().map(str::to_owned));
        assert!(parse("--headless").is_err());
        assert!(parse("--instructions 0").is_err());
        assert!(parse("--frames nope").is_err());
        assert!(parse("--unknown").is_err());
        assert_eq!(
            parse("--headless --instructions 123 --frames 2")
                .unwrap()
                .instruction_limit,
            Some(123)
        );
    }
}

#[cfg(test)]
mod dpad_tests {
    use super::dpad_resolve;
    const R: u16 = 1 << 4;
    const L: u16 = 1 << 5;
    const U: u16 = 1 << 6;
    const D: u16 = 1 << 7;
    const A: u16 = 1;

    #[test]
    fn opposite_directions_resolve_to_the_latest_press() {
        let mut s = (0, 0);
        assert_eq!(dpad_resolve(L | A, &mut s), L | A);
        assert_eq!(dpad_resolve(L | R | A, &mut s), R | A, "right pressed while left held");
        assert_eq!(dpad_resolve(L | R | A, &mut s), R | A, "still both: keep right");
        assert_eq!(dpad_resolve(L | A, &mut s), L | A, "right released: left again");
        assert_eq!(dpad_resolve(U | D, &mut (D, D)), U);
    }

    #[test]
    fn diagonals_and_single_directions_pass_through() {
        let mut s = (0, 0);
        assert_eq!(dpad_resolve(U | R, &mut s), U | R);
        assert_eq!(dpad_resolve(D, &mut s), D);
        assert_eq!(dpad_resolve(0, &mut s), 0);
    }
}
