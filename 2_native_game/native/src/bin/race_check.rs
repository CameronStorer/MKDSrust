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
    if !(3..=4).contains(&args.len()) {
        return Err(
            "usage: race_check <user.nds> <user-ROM-route.csv> <new-output-dir> [max-frames=15000]"
                .into(),
        );
    }
    let rom = std::fs::read(&args[0])?;
    let image = mkds_builder::extract::arm9_main(&rom)?;
    let mut points = Vec::new();
    for line in std::fs::read_to_string(&args[1])?.lines().skip(1) {
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
            if buttons != last_buttons {
                writeln!(inputs, "{frame},{buttons},,")?;
                last_buttons = buttons;
            }
            session.machine.set_input(buttons, None);
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
