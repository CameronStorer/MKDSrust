//! MKDSrust builder: validates a user-supplied ROM, builds the port with locked
//! dependencies, and writes a launcher bundle. Also acts as the bundle's launcher (`play`).
mod build;

mod tui;

use clap::Parser;
use mkds_builder::extract;
use mkds_builder::rom::{self, BuildError, RomInfo, err};
use std::io::IsTerminal;
use std::path::PathBuf;

#[derive(Parser)]
#[command(version, about = "MKDSrust: build and launch a native game from your own Mario Kart DS ROM")]
struct Args {
    /// Path to your own .nds file
    rom: Option<PathBuf>,
    /// Validate the ROM without building
    #[arg(long, conflicts_with_all = ["build", "run"])]
    inspect: bool,
    /// Rip the ROM's files into <out>/assets (loose files + manifest.json), then exit
    #[arg(long, conflicts_with_all = ["inspect", "build", "run", "reference_port", "emit_tables"])]
    rip: bool,
    /// Generate Rust from your ROM, compile it, and stage a native game bundle
    #[arg(long, conflicts_with_all = ["run", "reference_port"])]
    build: bool,
    /// Build the native bundle, then launch it
    #[arg(long, conflicts_with = "reference_port")]
    run: bool,
    /// Legacy development action: build the frozen hand-written reference port instead (not ROM-driven code)
    #[arg(long, conflicts_with_all = ["inspect", "emit_tables"])]
    reference_port: bool,
    /// Empty output directory (default: ~/MKDS Builds/<hash prefix>)
    #[arg(long)]
    out: Option<PathBuf>,
    /// Open the menu at the "your ROM" step (what double-clicking build.bat does)
    #[arg(long, conflicts_with = "rom")]
    pick_rom: bool,
    /// Source checkout (default: search upward from cwd and the executable)
    #[arg(long)]
    source: Option<PathBuf>,
    /// Allow experimental builds from modified AMCE revision 0 ROMs
    #[arg(long)]
    allow_unverified: bool,
    /// Diagnostics only: continue although some function entries were rejected
    #[arg(long)]
    allow_incomplete: bool,
    /// Do not keep the generated Rust source tree in the bundle (it is kept by default)
    #[arg(long)]
    no_source: bool,
    /// Verification only: also build the acceptance executable (adaptive race driver on generated code)
    #[arg(long)]
    with_acceptance: bool,
    /// Parallel compile jobs for the generated code (default: half your logical CPUs, 2..=8)
    #[arg(long, value_name = "N")]
    jobs: Option<usize>,
    /// Fresh build in a temporary directory instead of the persistent build cache
    #[arg(long, conflicts_with = "cache_dir")]
    no_cache: bool,
    /// Build cache location (default: LocalAppData on Windows, ~/Library/Caches on macOS, XDG cache on Linux)
    #[arg(long, value_name = "DIR")]
    cache_dir: Option<PathBuf>,
    /// Delete the build cache, then exit
    #[arg(long)]
    clear_cache: bool,
    /// Diagnostics only: compile the reference interpreter into the game (enables --arm7-interpreted)
    #[arg(long)]
    with_interpreter: bool,
    /// Instruction timing (default: native; MKDS_TIMING also supported)
    #[arg(long, value_parser = ["native", "exact"])]
    timing: Option<String>,
    /// Prevent Cargo network access
    #[arg(long)]
    offline: bool,
    /// Course archive name to start in (omit for the game menu)
    #[arg(long)]
    course: Option<String>,
    /// Write ROM-derived Rust tables (generated source) to this file, then exit
    #[arg(long, value_name = "FILE")]
    emit_tables: Option<PathBuf>,
    /// Print machine-readable ROM info as JSON (with --inspect)
    #[arg(long)]
    json: bool,
}

fn describe(i: &RomInfo) {
    println!(
        "\nROM: {}\nGame: {} revision {}\nCompatibility: {}\nSHA-256: {}\nFiles: {}; course archives: {}",
        i.path,
        i.game_code,
        i.revision,
        i.compatibility,
        i.sha256,
        i.file_count,
        i.courses.len()
    );
}

fn find_source(explicit: Option<PathBuf>) -> Result<PathBuf, BuildError> {
    if let Some(s) = explicit {
        return Ok(s);
    }
    let cwd = std::env::current_dir()
        .ok()
        .and_then(|d| build::find_source(&d));
    let exe = std::env::current_exe()
        .ok()
        .and_then(|e| build::find_source(&e));
    cwd.or(exe).map_or_else(
        || err("No checkout found (needs 2_native_game/Cargo.toml). Run from inside the repository or pass --source."),
        Ok,
    )
}

fn run() -> Result<i32, BuildError> {
    // Launcher mode: a copy of this binary named play(.exe) beside manifest.json.
    if let Ok(exe) = std::env::current_exe() {
        let dir = exe.parent().map(PathBuf::from).unwrap_or_default();
        let stem = exe.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        if stem == "play" && dir.join("manifest.json").is_file() {
            let extra: Vec<String> = std::env::args().skip(1).collect();
            return build::launch(&dir, &extra);
        }
    }
    let args = Args::parse();
    if let Some(timing) = &args.timing { unsafe { std::env::set_var("MKDS_TIMING", timing); } }
    if args.clear_cache {
        let dir = args.cache_dir.clone().unwrap_or_else(build::default_cache_dir);
        if dir.join("build.lock").exists() {
            return err(format!("A build is using the cache {}; try again when it finishes.", dir.display()));
        }
        if dir.exists() {
            std::fs::remove_dir_all(&dir)?;
        }
        println!("Build cache cleared: {}", dir.display());
        return Ok(0);
    }
    let automated =
        args.inspect || args.rip || args.build || args.run || args.reference_port || args.emit_tables.is_some();
    let interactive = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    if args.rom.is_none() && automated {
        return err("Provide the path to your .nds file.");
    }
    if !automated {
        if !interactive {
            return err(
                "Use --inspect, --rip, --build or --run in a noninteractive terminal.",
            );
        }
        let source = find_source(args.source)?;
        tui::run(tui::Config {
            output: args.out.unwrap_or_else(|| source.join("builds").join("mkds")),
            source,
            offline: args.offline,
            allow_unverified: args.allow_unverified,
            initial_rom: args.rom,
            pick_rom: args.pick_rom,
        })?;
        return Ok(0);
    }
    let info = rom::inspect_rom(&args.rom.unwrap())?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&info).unwrap());
    } else {
        describe(&info);
    }
    if let Some(out) = &args.emit_tables {
        let rom = std::fs::read(&info.path)?;
        std::fs::write(out, extract::generate(&rom, &info.sha256)?)?;
        println!("Wrote {}", out.display());
        return Ok(0);
    }
    if args.inspect {
        return Ok(0);
    }
    if args.rip {
        let Some(out) = &args.out else {
            return err("--rip needs --out <empty folder>.");
        };
        let rom = std::fs::read(&info.path)?;
        if mkds_builder::rom::hex(&<sha2::Sha256 as sha2::Digest>::digest(&rom)) != info.sha256 {
            return err("ROM changed after inspection; inspect it again.");
        }
        let ripped = mkds_builder::rip::rip(&rom, &info.sha256, out, &mut |l| eprintln!("{l}"))?;
        println!(
            "Ripped {} files ({:.1} MB) to {}",
            ripped.file_count,
            ripped.bytes as f64 / 1e6,
            out.join("assets").display()
        );
        return Ok(0);
    }
    if let Some(c) = &args.course {
        if !info.courses.contains(c) {
            return err(format!("Course archive not found: {c}"));
        }
    }
    let out = args.out.unwrap_or_else(|| build::default_output(&info));
    let source = find_source(args.source)?;
    let bundle = if args.reference_port {
        eprintln!("NOTE: --reference-port builds the frozen hand-written port, not your ROM's code.");
        build::build(
            &info,
            &build::Options {
                source,
                output: out,
                allow_unverified: args.allow_unverified,
                offline: args.offline,
            },
            &mut |l| eprintln!("{l}"),
        )?
    } else {
        if args.course.is_some() {
            return err("--course is not supported by the native build yet.");
        }
        build::recompile(
            &info,
            &build::RecompileOptions {
                source,
                output: out,
                allow_unverified: args.allow_unverified,
                offline: args.offline,
                allow_incomplete: args.allow_incomplete,
                keep_source: !args.no_source,
                with_acceptance: args.with_acceptance,
                jobs: args.jobs,
                with_interpreter: args.with_interpreter,
                cache_dir: if args.no_cache {
                    None
                } else {
                    Some(args.cache_dir.clone().unwrap_or_else(build::default_cache_dir))
                },
            },
            &mut |l| eprintln!("{l}"),
        )?
    };
    if args.run {
        return build::launch(&bundle, &args.course.into_iter().collect::<Vec<_>>());
    }
    println!(
        "Launch: {}",
        bundle
            .join(if cfg!(windows) { "play.exe" } else { "play" })
            .display()
    );
    Ok(0)
}

fn main() {
    match run() {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod defaults_tests {
    use super::*;
    #[test]
    fn timing_cli_accepts_only_supported_modes() {
        assert!(Args::try_parse_from(["builder", "--timing", "exact"]).is_ok());
        assert!(Args::try_parse_from(["builder", "--timing", "native"]).is_ok());
        assert!(Args::try_parse_from(["builder", "--timing", "invalid"]).is_err());
    }
}
