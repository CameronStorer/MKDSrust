//! Locked Cargo build and bundle staging (port of `build` in 5_tools/rom_builder.py).
use mkds_builder::rom::{BuildError, RomInfo, err, sha256_file};
use serde_json::json;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Finds a checkout containing the recompiler or reference game, searching upward from `start`.
pub fn find_source(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .find(|d| {
            d.join("2_native_game/Cargo.toml").is_file()
                || d.join("3_hand_port/game/Cargo.toml").is_file()
        })
        .map(Path::to_path_buf)
}

fn home() -> PathBuf {
    std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .map(PathBuf::from)
        .unwrap_or_default()
}

pub fn cargo_path() -> Result<PathBuf, BuildError> {
    let exe = if cfg!(windows) { "cargo.exe" } else { "cargo" };
    let on_path = std::env::var_os("PATH").and_then(|p| {
        std::env::split_paths(&p)
            .map(|d| d.join(exe))
            .find(|c| c.is_file())
    });
    if let Some(p) = on_path {
        return Ok(p);
    }
    let fallback = home().join(".cargo/bin").join(exe);
    if fallback.is_file() {
        return Ok(fallback);
    }
    err("Cargo is missing. Install Rust (https://rustup.rs) and reopen your terminal.")
}

pub fn default_output(info: &RomInfo) -> PathBuf {
    home().join("MKDS Builds").join(&info.sha256[..16])
}

pub struct Options {
    pub source: PathBuf,
    pub output: PathBuf,
    pub allow_unverified: bool,
    pub offline: bool,
}

/// Options for the ROM -> generated Rust -> native game build.
pub struct RecompileOptions {
    pub source: PathBuf,
    pub output: PathBuf,
    pub allow_unverified: bool,
    pub offline: bool,
    /// Proceed (diagnostics only) when some metadata entries are rejected or absent.
    pub allow_incomplete: bool,
    /// Keep the generated Rust workspace in the bundle as `source/` (viewable, rebuildable).
    pub keep_source: bool,
    /// Also build the acceptance executable: the adaptive race driver (2_native_game/native/src/bin/race_check.rs)
    /// running on this build's generated table. Verification only; not part of a normal bundle.
    pub with_acceptance: bool,
    /// Parallel compiler jobs for the generated workspace (`None`: half the logical CPUs, 2..=8).
    pub jobs: Option<usize>,
    /// Persistent build cache (`None`: fresh build in a temporary directory, as for acceptance runs).
    pub cache_dir: Option<PathBuf>,
    /// Diagnostics only: compile the reference interpreter into the game (`--arm7-interpreted`). Shipped
    /// builds leave it out, so code without a lifted block is an error naming the pc, never interpreted.
    pub with_interpreter: bool,
}

/// Every generated crate's `recomp` dependency without default features. Cargo unifies features across
/// the workspace, so one crate with defaults would compile the interpreter into the whole game.
fn recomp_without_defaults(ws: &Path, with_interpreter: bool) -> Result<usize, BuildError> {
    let mut changed = 0;
    for entry in std::fs::read_dir(ws)? {
        let toml = entry?.path().join("Cargo.toml");
        if !toml.is_file() {
            continue;
        }
        let text = std::fs::read_to_string(&toml)?;
        let mut out = String::with_capacity(text.len());
        for line in text.lines() {
            let t = line.trim_start();
            if (t.starts_with("recomp = {") || t.starts_with("mkds_native = {"))
                && !t.contains("default-features")
            {
                let extra = if with_interpreter {
                    ", features = [\"interpreter\"]"
                } else {
                    ""
                };
                let close = line.rfind('}').ok_or_else(|| {
                    BuildError(format!("Unexpected dependency line in {}", toml.display()))
                })?;
                out.push_str(&line[..close].trim_end());
                out.push_str(&format!(", default-features = false{extra} }}"));
                changed += 1;
            } else {
                out.push_str(line);
            }
            out.push('\n');
        }
        if out != text {
            std::fs::write(&toml, out)?;
        }
    }
    Ok(changed)
}

/// Default cache: LocalAppData on Windows, Library/Caches on macOS, XDG cache on Linux.
pub fn default_cache_dir() -> PathBuf {
    let base = if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
    } else if cfg!(target_os = "macos") {
        Some(home().join("Library/Caches"))
    } else {
        std::env::var_os("XDG_CACHE_HOME").map(PathBuf::from)
    };
    base.unwrap_or_else(|| home().join(".cache"))
        .join("MKDSrust").join("cache")
}

/// Makes `dst` hold exactly the files of `src` (a `target` directory is never touched), rewriting
/// only files whose bytes differ. Unchanged generated crates keep their timestamps, so Cargo's own
/// fingerprints skip them. Returns (written, unchanged, removed).
fn sync_tree(src: &Path, dst: &Path) -> Result<(usize, usize, usize), BuildError> {
    fn walk(src: &Path, dst: &Path, n: &mut (usize, usize, usize)) -> Result<(), BuildError> {
        std::fs::create_dir_all(dst)?;
        let mut keep = std::collections::HashSet::new();
        for entry in std::fs::read_dir(src)? {
            let entry = entry?;
            let name = entry.file_name();
            if name == "target" {
                continue;
            }
            keep.insert(name.clone());
            let (s, d) = (entry.path(), dst.join(&name));
            if entry.file_type()?.is_dir() {
                if d.is_file() {
                    std::fs::remove_file(&d)?;
                }
                walk(&s, &d, n)?;
            } else {
                let data = std::fs::read(&s)?;
                if d.is_dir() {
                    std::fs::remove_dir_all(&d)?;
                }
                if std::fs::read(&d).ok().as_deref() == Some(&data[..]) {
                    n.1 += 1;
                } else {
                    std::fs::write(&d, &data)?;
                    n.0 += 1;
                }
            }
        }
        for entry in std::fs::read_dir(dst)? {
            let entry = entry?;
            let name = entry.file_name();
            if name == "target" || keep.contains(&name) {
                continue;
            }
            if entry.file_type()?.is_dir() {
                std::fs::remove_dir_all(entry.path())?;
            } else {
                std::fs::remove_file(entry.path())?;
            }
            n.2 += 1;
        }
        Ok(())
    }
    let mut n = (0, 0, 0);
    walk(src, dst, &mut n)?;
    Ok(n)
}

/// Exclusive use of a cache directory for the lifetime of one build.
#[derive(Debug)]
struct CacheLock(PathBuf);

impl Drop for CacheLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn lock_cache(dir: &Path) -> Result<CacheLock, BuildError> {
    std::fs::create_dir_all(dir)?;
    let lock = dir.join("build.lock");
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&lock)
    {
        Ok(mut f) => {
            use std::io::Write;
            let _ = writeln!(f, "pid {}", std::process::id());
            Ok(CacheLock(lock))
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => err(format!(
            "Another build is using the cache {} (if none is running, delete {}), or build with --no-cache.",
            dir.display(),
            lock.display()
        )),
        Err(e) => Err(e.into()),
    }
}

/// Default compile parallelism: each generated part crate needs ~0.5-1.5 GB while compiling.
pub fn default_jobs() -> usize {
    std::thread::available_parallelism().map_or(2, |n| (n.get() / 2).clamp(2, 8))
}

/// Runs Cargo, forwarding diagnostics to `log`, and returns every executable Cargo reports, by
/// target name (never a guessed path).
fn run_cargo(
    command: Command,
    log: &mut dyn FnMut(String),
) -> Result<std::collections::HashMap<String, PathBuf>, BuildError> {
    run_cargo_output(command, log, true)
}

fn run_cargo_output(
    mut command: Command,
    log: &mut dyn FnMut(String),
    json_messages: bool,
) -> Result<std::collections::HashMap<String, PathBuf>, BuildError> {
    if json_messages {
        command.arg("--message-format=json-render-diagnostics");
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command.spawn()?;
    // Drain stderr on its own thread so a full pipe can never stall Cargo.
    let stderr = child.stderr.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    let stdout_tx = tx.clone();
    let stdout = child.stdout.take().unwrap();
    let stdout_reader = std::thread::spawn(move || {
        for l in BufReader::new(stdout).lines().map_while(Result::ok) {
            let _ = stdout_tx.send(l);
        }
    });
    let reader = std::thread::spawn(move || {
        for l in BufReader::new(stderr).lines().map_while(Result::ok) {
            let _ = tx.send(l);
        }
    });
    let mut executables = std::collections::HashMap::new();
    for line in rx.iter() {
        match serde_json::from_str::<serde_json::Value>(&line) {
            Ok(ev) => match ev["reason"].as_str() {
                Some("compiler-artifact") if ev["executable"].is_string() => {
                    if let (Some(name), Some(exe)) =
                        (ev["target"]["name"].as_str(), ev["executable"].as_str())
                    {
                        executables.insert(name.to_string(), PathBuf::from(exe));
                    }
                }
                Some("compiler-message") => {
                    if let Some(r) = ev["message"]["rendered"].as_str() {
                        log(r.trim_end().to_string());
                    }
                }
                _ => {}
            },
            Err(_) => log(line),
        }
    }
    let _ = reader.join();
    let _ = stdout_reader.join();
    while let Ok(l) = rx.try_recv() {
        log(l);
    }
    if !child.wait()?.success() {
        return err(
            "Cargo failed (diagnostics above). If downloads failed, check your internet connection and retry; the first build needs crates.io.",
        );
    }
    executables.retain(|_, e: &mut PathBuf| e.is_file());
    Ok(executables)
}

/// SHA-256 over every `Cargo.toml`, `src/lib.rs` and `src/main.rs` of the generated workspace
/// (sorted paths), so the manifest identifies the exact generated source the executable came from.
/// Also returns the number of entries in the generated lift table.
fn hash_generated(ws: &Path) -> Result<(String, usize), BuildError> {
    use sha2::{Digest, Sha256};
    let mut files = Vec::new();
    for entry in std::fs::read_dir(ws)? {
        let dir = entry?.path();
        if dir.is_dir() && dir.file_name().is_some_and(|n| n != "target") {
            for rel in ["Cargo.toml", "src/lib.rs", "src/main.rs"] {
                if dir.join(rel).is_file() {
                    files.push(dir.join(rel));
                }
            }
        }
    }
    files.sort();
    let mut h = Sha256::new();
    let mut functions = 0;
    for f in &files {
        let data = std::fs::read(f)?;
        if f.ends_with("lifted/src/lib.rs") {
            functions = String::from_utf8_lossy(&data)
                .matches("recomp::mem::Lifted {")
                .count();
        }
        h.update(f.strip_prefix(ws).unwrap().to_string_lossy().as_bytes());
        h.update(&data);
    }
    Ok((mkds_builder::rom::hex(&h.finalize()), functions))
}

fn copy_tree(from: &Path, to: &Path) -> Result<(), BuildError> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let dest = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            if entry.file_name() != "target" {
                copy_tree(&entry.path(), &dest)?;
            }
        } else {
            std::fs::copy(entry.path(), dest)?;
        }
    }
    Ok(())
}

/// ROM -> generated Rust workspace -> compiled native executable -> staged bundle.
/// Every stage receives the selected ROM and metadata explicitly. Nothing is staged on failure.
pub fn recompile(
    info: &RomInfo,
    opts: &RecompileOptions,
    log: &mut dyn FnMut(String),
) -> Result<PathBuf, BuildError> {
    recompile_with_toolchain(info, opts, None, log)
}

/// Explicit per-build compiler selection; never changes the parent process environment.
pub struct Toolchain {
    pub cargo: PathBuf,
    pub rustc: PathBuf,
    pub target: String,
}

pub fn recompile_with_toolchain(
    info: &RomInfo,
    opts: &RecompileOptions,
    toolchain: Option<&Toolchain>,
    log: &mut dyn FnMut(String),
) -> Result<PathBuf, BuildError> {
    if !info.reference_match && !opts.allow_unverified {
        return err(
            "This ROM differs from the reference. For an experimental modified-ROM build, allow unverified ROMs.",
        );
    }
    if sha256_file(Path::new(&info.path))? != info.sha256 {
        return err("ROM changed after inspection; inspect it again.");
    }
    let output = if opts.output.is_absolute() {
        opts.output.clone()
    } else {
        std::env::current_dir()?.join(&opts.output)
    };
    if output.exists() && std::fs::read_dir(&output)?.next().is_some() {
        return err(format!(
            "Output must be empty; choose a new directory: {}",
            output.display()
        ));
    }
    let recomp_manifest = opts.source.join("2_native_game/Cargo.toml");
    let seed_lock = opts.source.join("2_native_game/Cargo.lock");
    let metadata = opts.source.join("dictionary/FUNCTION_BOUNDARIES.csv");
    if !recomp_manifest.is_file() || !seed_lock.is_file() || !metadata.is_file() {
        return err(
            "Source checkout needs 2_native_game/Cargo.toml, 2_native_game/Cargo.lock, and address-only dictionary/FUNCTION_BOUNDARIES.csv.",
        );
    }
    let cargo = match toolchain {
        Some(t) => t.cargo.clone(),
        None => cargo_path()?,
    };
    let parent = output
        .parent()
        .ok_or_else(|| BuildError("Output has no parent directory.".into()))?;
    std::fs::create_dir_all(parent)?;
    let stage = tempfile::Builder::new()
        .prefix("mkds-native-")
        .tempdir_in(parent)?;
    let ws = stage.path().join("workspace");
    let bundle = stage.path().join("bundle");
    std::fs::create_dir(&bundle)?;
    // Persistent cache: generator binary shared, one workspace + target dir per ROM.
    let _cache_lock = opts.cache_dir.as_deref().map(lock_cache).transpose()?;
    let rom_cache = opts.cache_dir.as_ref().map(|c| c.join(&info.sha256[..16]));
    let gen_target = match &opts.cache_dir {
        Some(c) => c.join("gen-target"),
        None => stage.path().join("gen-target"),
    };
    let offline = |c: &mut Command| {
        if let Some(t) = toolchain {
            c.env("RUSTC", &t.rustc)
                .env("RUSTDOC", t.rustc.with_file_name("rustdoc.exe"));
            c.env("CARGO_BUILD_TARGET", &t.target);
            // Only child PATH: DLLs and rust-mingw linker tools need to be discoverable.
            let root = t.rustc.parent().unwrap();
            let mut paths = vec![
                root.to_path_buf(),
                root.join(format!("../lib/rustlib/{}/bin", t.target)),
                root.join(format!("../lib/rustlib/{}/bin/self-contained", t.target)),
            ];
            paths.extend(std::env::split_paths(
                &std::env::var_os("PATH").unwrap_or_default(),
            ));
            if let Ok(path) = std::env::join_paths(paths) {
                c.env("PATH", path);
            }
        }
        if opts.offline {
            c.arg("--offline");
        }
    };

    // 1. Rip the ROM's files into the bundle's assets/ (spec: https://github.com/CameronStorer/MKDSrust-Docs).
    log("[1/5] Ripping the ROM's files into assets/.".into());
    let rom_bytes = std::fs::read(&info.path)?;
    if mkds_builder::rom::hex(&<sha2::Sha256 as sha2::Digest>::digest(&rom_bytes)) != info.sha256 {
        return err("ROM changed after inspection; inspect it again.");
    }
    let ripped = mkds_builder::rip::rip(&rom_bytes, &info.sha256, &stage.path().join("rip"), log)?;
    drop(rom_bytes);
    std::fs::rename(stage.path().join("rip/assets"), bundle.join("assets"))?;

    // 2. Build and run the production generator with explicit inputs (no implicit ROM/metadata).
    log("[2/5] Building the generator.".into());
    let mut gen_build = Command::new(&cargo);
    gen_build
        .args([
            "build",
            "--release",
            "--locked",
            "--features",
            "gen",
            "--bin",
            "mkds_gen",
        ])
        .arg("--manifest-path")
        .arg(&recomp_manifest)
        .arg("--target-dir")
        .arg(&gen_target)
        .current_dir(&opts.source)
        .env_remove("MKDS_ROM")
        .env_remove("MKDS_FUNCTIONS")
        .env_remove("MKDS_LIFT_CAP");
    offline(&mut gen_build);
    if let Some(t) = toolchain {
        gen_build.args(["--target", &t.target]);
    }
    let generator = run_cargo(gen_build, log)?
        .remove("mkds_gen")
        .ok_or_else(|| BuildError("Cargo did not report the mkds_gen executable.".into()))?;
    log("[3/5] Generating Rust from the ROM's machine code (every metadata entry).".into());
    let mut gen_cmd = Command::new(&generator);
    gen_cmd
        .arg("--rom")
        .arg(&info.path)
        .arg("--metadata")
        .arg(&metadata)
        .arg("--out")
        .arg(&ws)
        .arg("--recomp")
        .arg(opts.source.join("2_native_game"))
        .arg("--no-game") // the builder writes the game crate: it must link the native host
        .arg("--allow-incomplete") // coverage is judged below so rejects can be reported
        .current_dir(&opts.source);
    // comments only: readable function names, signatures and notes in source/ from the function
    // dictionary (`dictionary/functions.csv`, then the hand-made tables that refine it)
    for names in dictionary_tables(&opts.source) {
        gen_cmd.arg("--names").arg(&names);
    }
    // Block 5.4 (native by default, `MKDS_TIMING=exact` to opt out): ARM9 code without the instruction budget. Such a
    // build needs the native hardware services, so its executables turn them on (see NATIVE_TIMING_MAIN).
    let timing_native = std::env::var("MKDS_TIMING").as_deref() != Ok("exact");
    if timing_native {
        gen_cmd.arg("--timing").arg("native");
        log("Timing: native (Block 5.4): ARM9 code without the instruction budget; MKDS_HW_*=native at run time.".into());
    }
    let with_timing = |main: String| {
        if timing_native {
            main.replacen(
                "fn main() {\n",
                &format!("fn main() {{\n{NATIVE_TIMING_MAIN}"),
                1,
            )
        } else {
            main
        }
    };
    let out = gen_cmd.output()?;
    for l in String::from_utf8_lossy(&out.stderr)
        .lines()
        .chain(String::from_utf8_lossy(&out.stdout).lines())
    {
        log(l.to_string());
    }
    if !out.status.success() || !ws.join("generation.json").is_file() {
        return err("Generation failed; nothing was staged.");
    }
    let generation: serde_json::Value =
        serde_json::from_slice(&std::fs::read(ws.join("generation.json"))?)
            .map_err(|e| BuildError(format!("Unreadable generation.json: {e}")))?;
    let unaccounted = generation["rejected_or_absent_entries"]
        .as_u64()
        .unwrap_or(u64::MAX);
    if unaccounted != 0 && !opts.allow_incomplete {
        let sample: Vec<String> = generation["rejected"]
            .as_array()
            .into_iter()
            .flatten()
            .take(5)
            .map(|r| format!("{} {} ({})", r["segment"], r["entry"], r["reason"]))
            .collect();
        return err(format!(
            "Generation is incomplete: {unaccounted} metadata entries were rejected or absent (e.g. {}). See coverage.csv; use --allow-incomplete only for diagnostics.",
            sample.join("; ")
        ));
    }

    // 3. Game crate linking the generated table to the native host, then lock + compile.
    let native = opts.source.join("2_native_game/native");
    let native_lock = native.join("Cargo.lock");
    if !native.join("Cargo.toml").is_file() || !native_lock.is_file() {
        return err("Source checkout needs 2_native_game/native (Cargo.toml and Cargo.lock).");
    }
    let toml_path = |p: &Path| {
        let s = p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
        format!("{:?}", s.to_string_lossy().trim_start_matches(r"\\?\"))
    };
    let gdir = ws.join("game");
    std::fs::create_dir_all(gdir.join("src"))?;
    std::fs::write(
        gdir.join("Cargo.toml"),
        format!(
            "[package]\nname = \"game\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\nrecomp = {{ path = {} }}\nlifted = {{ path = \"../lifted\" }}\nmkds_builder = {{ path = {} }}\nmkds_native = {{ path = {} }}\n",
            toml_path(&opts.source.join("2_native_game")),
            toml_path(&opts.source.join("1_build_tool")),
            toml_path(&native),
        ),
    )?;
    let has_arm7 =
        std::fs::read_to_string(ws.join("lifted/src/lib.rs"))?.contains("pub static LIFTED7");
    if !has_arm7 && !opts.with_interpreter {
        return err(
            "The generator produced no lifted ARM7 code (LIFTED7); without it the ARM7 can only be interpreted. Update the checkout, or build with --with-interpreter for diagnostics.",
        );
    }
    let arm7_entry = if has_arm7 {
        "mkds_native::run_from_assets_dual(&assets, lifted::ROM_SHA256, ASSETS_SHA256, lifted::LIFTED, lifted::SEGMENT_SIGS, lifted::LIFTED7, options)?"
    } else {
        // Older generators: no ARM7 table, so the ARM7 can only be interpreted. Say so at every start.
        "{ eprintln!(\"This build has no lifted ARM7 code; the ARM7 is interpreted.\"); mkds_native::run_from_assets(&assets, lifted::ROM_SHA256, ASSETS_SHA256, lifted::LIFTED, lifted::SEGMENT_SIGS, options)? }"
    };
    let assets_sha = ripped.manifest["assets_sha256"]
        .as_str()
        .ok_or_else(|| BuildError("Rip manifest lacks assets_sha256.".into()))?;
    std::fs::write(
        gdir.join("src/main.rs"),
        with_timing(
            GAME_MAIN
                .replace("@ASSETS_SHA256@", assets_sha)
                .replace("@ARM7@", arm7_entry)
                .replace(
                    "@INTERPRETED@",
                    if opts.with_interpreter {
                        INTERPRETED_ARM7
                    } else {
                        NO_INTERPRETER
                    },
                ),
        ),
    )?;
    let deps = recomp_without_defaults(&ws, opts.with_interpreter)?;
    log(format!(
        "Interpreter {}: {deps} generated dependencies set to default-features = false.",
        if opts.with_interpreter {
            "compiled in (diagnostics build)"
        } else {
            "not compiled in"
        }
    ));
    if opts.with_acceptance {
        let driver = opts
            .source
            .join("2_native_game/native/src/bin/race_check.rs");
        if !driver.is_file() {
            return err(
                "--with-acceptance needs 2_native_game/native/src/bin/race_check.rs in the checkout.",
            );
        }
        let driver = driver.canonicalize()?;
        let driver = driver.to_string_lossy();
        std::fs::create_dir_all(gdir.join("src/bin"))?;
        std::fs::write(
            gdir.join("src/bin/acceptance.rs"),
            // Acceptance shares this build's generated timing. Budget-free code must
            // enable the matching runtime before Session reads its cached switches.
            // Exact builds leave the oracle's startup and hardware defaults alone.
            with_timing(format!(
                "//! Acceptance driver on this build's generated code (verification only).\n\
                 //! `acceptance <rom.nds> <route.csv> <new-output-dir> [max-frames]`\n\
                 #[path = {:?}]\n#[allow(dead_code)]\nmod race_check;\n\n\
                 fn main() {{\n    let args: Vec<String> = std::env::args().skip(1).collect();\n    \
                 if let Err(e) = race_check::run_with_tables(&args, lifted::LIFTED, lifted::SEGMENT_SIGS, {}) {{\n        \
                 eprintln!(\"{{e}}\");\n        std::process::exit(1);\n    }}\n}}\n",
                driver.trim_start_matches(r"\\?\"),
                if has_arm7 { "lifted::LIFTED7" } else { "&[]" }
            )),
        )?;
    }
    let ws_toml = std::fs::read_to_string(ws.join("Cargo.toml"))?;
    if !ws_toml.contains("members = [") {
        return err("Unexpected generated workspace manifest.");
    }
    std::fs::write(
        ws.join("Cargo.toml"),
        ws_toml.replacen("members = [", "members = [\"game\", ", 1),
    )?;
    // Seeded from the native host's reviewed lock; only the generated crates are added.
    std::fs::copy(&native_lock, ws.join("Cargo.lock"))?;
    let mut update = Command::new(&cargo);
    update
        .args(["update", "--workspace", "--manifest-path"])
        .arg(ws.join("Cargo.toml"));
    offline(&mut update);
    run_cargo_output(update, log, false)?;
    let (ws, target_dir) = match &rom_cache {
        Some(c) => {
            let cached = c.join("workspace");
            let (written, unchanged, removed) = sync_tree(&ws, &cached)?;
            log(format!(
                "Build cache {}: {written} files changed, {unchanged} unchanged, {removed} removed.",
                c.display()
            ));
            (cached, c.join("target"))
        }
        None => (ws, stage.path().join("target")),
    };

    log("[4/5] Compiling the generated workspace (this takes a while).".into());
    let mut build_cmd = Command::new(&cargo);
    build_cmd
        .args(["build", "--release", "--locked", "-j"])
        .arg(opts.jobs.unwrap_or_else(default_jobs).max(1).to_string())
        .arg("--manifest-path")
        .arg(ws.join("Cargo.toml"))
        .arg("--target-dir")
        .arg(&target_dir)
        .current_dir(stage.path());
    offline(&mut build_cmd);
    if let Some(t) = toolchain {
        build_cmd.args(["--target", &t.target]);
    }
    let mut built = run_cargo(build_cmd, log)?;
    let exe = built
        .remove("game")
        .ok_or_else(|| BuildError("Cargo did not report a built game executable.".into()))?;
    let acceptance =
        if opts.with_acceptance {
            Some(built.remove("acceptance").ok_or_else(|| {
                BuildError("Cargo did not report the acceptance executable.".into())
            })?)
        } else {
            None
        };

    log("[5/5] Staging the bundle.".into());
    let (generated_sha, _) = hash_generated(&ws)?;
    std::fs::copy(ws.join("generation.json"), bundle.join("generation.json"))?;
    std::fs::copy(ws.join("coverage.csv"), bundle.join("coverage.csv"))?;
    if opts.keep_source {
        copy_tree(&ws, &bundle.join("source"))?;
        std::fs::write(bundle.join("source/README.txt"), SOURCE_README)?;
    }
    let exe_name = exe.file_name().unwrap().to_string_lossy().into_owned();
    std::fs::copy(&exe, bundle.join(&exe_name))?;
    let acceptance_name = match &acceptance {
        Some(a) => {
            let name = a.file_name().unwrap().to_string_lossy().into_owned();
            std::fs::copy(a, bundle.join(&name))?;
            Some(name)
        }
        None => None,
    };
    let launcher_name = if cfg!(windows) { "play.exe" } else { "play" };
    std::fs::copy(std::env::current_exe()?, bundle.join(launcher_name))?;
    let functions = generation["lifted_functions"].as_u64().unwrap_or(0);
    let manifest_data = json!({
        "format_version": 4,
        "artifact_kind": "generated_native_game",
        "uses_reference_port": false,
        "rom": info,
        "executable": exe_name,
        "launcher": launcher_name,
        "launch_args": ["--assets", "{assets}"],
        "runtime_reads_rom": false,
        "arm7": if has_arm7 { "lifted" } else { "interpreted (generator produced no LIFTED7)" },
        "interpreter_compiled_in": opts.with_interpreter,
        "readable_names": !dictionary_tables(&opts.source).is_empty(),
        "executable_sha256": sha256_file(&exe)?,
        "metadata_sha256": sha256_file(&metadata)?,
        "cargo_lock_sha256": sha256_file(&ws.join("Cargo.lock"))?,
        "generation_status": generation["status"],
        "generator_id": generation["generator_id"],
        "generated_function_count": functions,
        "metadata_entries": generation["metadata"]["entries"],
        "rejected_or_absent_entries": unaccounted,
        "undecoded_sites_left_to_interpreter": generation["undecoded_sites_left_to_interpreter"],
        "generation_report": "generation.json",
        "coverage_report": "coverage.csv",
        "coverage_sha256": sha256_file(&bundle.join("coverage.csv"))?,
        "generated_source_sha256": generated_sha,
        "generated_source": if opts.keep_source { json!("source") } else { json!(null) },
        "assets": {
            "path": "assets",
            "format_version": mkds_builder::rip::FORMAT_VERSION,
            "files": ripped.file_count,
            "assets_sha256": ripped.manifest["assets_sha256"],
            "manifest_sha256": sha256_file(&bundle.join("assets/manifest.json"))?,
        },
        "build_profile": "release",
        "acceptance_tool": acceptance_name,
    });
    std::fs::write(
        bundle.join("manifest.json"),
        serde_json::to_string_pretty(&manifest_data).unwrap() + "\n",
    )?;
    if output.exists() {
        std::fs::remove_dir(&output)?; // only an empty directory is ever removed
    }
    std::fs::rename(&bundle, &output)?;
    log(format!(
        "Native build: {} ({functions} generated functions, status {})",
        output.display(),
        generation["status"].as_str().unwrap_or("unknown")
    ));
    Ok(output)
}

/// Placed in the bundle's `source/`: how to read and rebuild the generated code.
const SOURCE_README: &str = "This is the Rust generated on your machine from your own ROM by mkds_builder.

part_NN/  the game's code, one Rust function per ROM function, in two versions:
          - the readable version (named after the function, e.g. `os_sleep_thread`, or `sub_<segment>_<address>`):
            registers as local variables, if/loop/match control flow, calls commented with the names the
            project's research found, and known struct fields read through named accessors
            (`kart::racer_index(m, r4)`). This is the version that runs at each function's entry.
          - the exact translation `f_<segment>_<address>` (a loop over the ARM blocks): the readable version
            continues there when control does something unusual (a thread switch, a computed jump).
          Both behave identically; each charges the same instruction budget at every original block.
layout/   the research structs: each a module with its field offsets and the accessors the code uses
          (each accessor is exactly the memory access it replaces)
lifted/   the table of all functions (both versions) and the ROM/metadata hashes they came from
game/     the program: checks your files, then runs the code on the native host
tiers.csv  every function: address, tier (T1 readable / T0 exact only), Rust name, crate, reason
coverage.csv / generation.json  every function the generator was given and what happened to it

How to read it: open tiers.csv, find a function by name, then its crate part_NN/src/lib.rs.
`budget!(..)` lines keep the console's timing; `spill!()`/`reload!()` keep the CPU registers exact around
calls; `R::ld32`/`R::st32` etc. are the game's memory accesses (DS addresses).

Rebuild (from this folder, with the project checkout still where it was when you built):
    cargo build --release --locked --offline
The runtime crates (recomp, mkds_native, mkds_builder) are referenced by absolute path into that checkout.
Environment switches when running: MKDS_T1=off (exact translations only), MKDS_FLAT=1 (no fibers).
This code is derived from copyrighted game data: keep it for your own use and do not distribute it.
";

/// `main.rs` of the generated `game` crate: runs the generated code from the ripped assets only (the
/// ROM is never opened). `@ASSETS_SHA256@` is the rip's `assets_sha256`, `@ARM7@` the ARM7 entry path.
const GAME_MAIN: &str = r#"//! Generated by the ROM builder: runs the game code generated from your ROM, reading the game's files
//! from the ripped assets folder (the ROM itself is not opened).
//! `game --assets <folder> [--arm7-interpreted] [--headless --instructions N] [--frames N] [--mute] [--capture DIR] [--input-script CSV]`
//! The ARM7 (sound/system CPU) runs from generated code; `--arm7-interpreted` is a diagnostics fallback only.
/// The ripped assets this game was built with (`assets_sha256` of the rip manifest).
const ASSETS_SHA256: &str = "@ASSETS_SHA256@";

fn run() -> Result<(), String> {
    let (mut assets, mut arm7_interpreted, mut rest) = (None, false, Vec::new());
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--assets" => assets = Some(std::path::PathBuf::from(args.next().ok_or("--assets needs a folder")?)),
            "--arm7-interpreted" => arm7_interpreted = true,
            "--arm7-lifted" => {} // the default; accepted for older launch scripts
            _ => rest.push(a),
        }
    }
    let assets = assets.ok_or("usage: game --assets <folder> [--arm7-interpreted] [runtime options]")?;
    let options = mkds_native::Options::from_args(rest)?;
    let report = if arm7_interpreted {
        @INTERPRETED@
    } else {
        @ARM7@
    };
    eprintln!("{report:?}");
    Ok(())
}

fn main() {
    if let Err(e) = run() {
        eprintln!("{e}");
        std::process::exit(1);
    }
}
"#;

/// Start of `main` in a `MKDS_TIMING=native` build (Block 5.4): the ARM9 code has no instruction budget, so
/// time may only come from the native services; they are switched on unless the caller chose otherwise.
const NATIVE_TIMING_MAIN: &str = "    // Block 5.4: built with --timing native
    unsafe { std::env::set_var(\"MKDS_TIMING\", \"native\") };
    for k in [\"MKDS_HW_TIME\", \"MKDS_HW_ARM7\", \"MKDS_HW_FS\", \"MKDS_HW_SND\"] {
        if std::env::var_os(k).is_none() {
            unsafe { std::env::set_var(k, \"native\") };
        }
    }
";

/// `--arm7-interpreted` in a diagnostics build (`--with-interpreter`).
const INTERPRETED_ARM7: &str = "eprintln!(\"Diagnostics: ARM7 interpreted (not the shipped configuration).\");\n        mkds_native::run_from_assets(&assets, lifted::ROM_SHA256, ASSETS_SHA256, lifted::LIFTED, lifted::SEGMENT_SIGS, options)?";
/// `--arm7-interpreted` in a shipped build: there is no interpreter to fall back to.
const NO_INTERPRETER: &str = "return Err(\"This game was built without the interpreter; rebuild with --with-interpreter for diagnostics.\".into())";

/// LEGACY/REFERENCE ONLY: builds the frozen hand-written port (`3_hand_port/game/`). Not ROM-driven code;
/// reachable only through the explicit `--reference-port` option.
pub fn build(
    info: &RomInfo,
    opts: &Options,
    log: &mut dyn FnMut(String),
) -> Result<PathBuf, BuildError> {
    if !info.reference_match && !opts.allow_unverified {
        return err(
            "This ROM differs from the reference. For an experimental modified-ROM build, allow unverified ROMs.",
        );
    }
    if sha256_file(Path::new(&info.path))? != info.sha256 {
        return err("ROM changed after inspection; inspect it again.");
    }
    let output = if opts.output.is_absolute() {
        opts.output.clone()
    } else {
        std::env::current_dir()?.join(&opts.output)
    };
    if output.exists() && std::fs::read_dir(&output)?.next().is_some() {
        return err(format!(
            "Output must be empty; choose a new directory: {}",
            output.display()
        ));
    }
    let manifest = opts.source.join("3_hand_port/game/Cargo.toml");
    let lock = opts.source.join("3_hand_port/game/Cargo.lock");
    if !manifest.is_file() || !lock.is_file() {
        return err(
            "Source checkout must contain 3_hand_port/game/Cargo.toml and 3_hand_port/game/Cargo.lock.",
        );
    }
    let cargo = cargo_path()?;
    let mut args: Vec<String> = ["build", "--release", "--locked", "--manifest-path"]
        .map(String::from)
        .into();
    args.push(manifest.to_string_lossy().into_owned());
    args.push("--message-format=json-render-diagnostics".into());
    if opts.offline {
        args.push("--offline".into());
    }
    log("Building the Rust port; Cargo may download locked dependencies.".into());
    let mut child = Command::new(&cargo)
        .args(&args)
        .current_dir(&opts.source)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    // Drain stderr on its own thread so a full pipe can never stall Cargo.
    let stderr = child.stderr.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    let stdout_tx = tx.clone();
    let stdout = child.stdout.take().unwrap();
    let stdout_reader = std::thread::spawn(move || {
        for l in BufReader::new(stdout).lines().map_while(Result::ok) {
            let _ = stdout_tx.send(l);
        }
    });
    let reader = std::thread::spawn(move || {
        for l in BufReader::new(stderr).lines().map_while(Result::ok) {
            let _ = tx.send(l);
        }
    });
    let mut executable: Option<PathBuf> = None;
    for line in BufReader::new(child.stdout.take().unwrap())
        .lines()
        .map_while(Result::ok)
    {
        while let Ok(l) = rx.try_recv() {
            log(l);
        }
        match serde_json::from_str::<serde_json::Value>(&line) {
            Ok(ev) => match ev["reason"].as_str() {
                Some("compiler-artifact")
                    if ev["target"]["name"] == "mkds_game" && ev["executable"].is_string() =>
                {
                    executable = ev["executable"].as_str().map(PathBuf::from);
                }
                Some("compiler-message") => {
                    if let Some(r) = ev["message"]["rendered"].as_str() {
                        log(r.trim_end().to_string());
                    }
                }
                _ => {}
            },
            Err(_) => log(line),
        }
    }
    let _ = reader.join();
    let _ = stdout_reader.join();
    while let Ok(l) = rx.try_recv() {
        log(l);
    }
    if !child.wait()?.success() {
        return err(
            "Cargo build failed; no bundle was produced. If Windows reports access denied on mkds_game.exe, close the running game and retry.",
        );
    }
    let Some(executable) = executable.filter(|e| e.is_file()) else {
        return err("Cargo did not report a built mkds_game executable.");
    };
    // Stage beside the destination, then rename into place.
    let parent = output
        .parent()
        .ok_or_else(|| BuildError("Output has no parent directory.".into()))?;
    std::fs::create_dir_all(parent)?;
    let stage = tempfile::Builder::new()
        .prefix("mkds-stage-")
        .tempdir_in(parent)?;
    let bundle = stage.path().join("bundle");
    std::fs::create_dir(&bundle)?;
    let exe_name = executable
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    std::fs::copy(&executable, bundle.join(&exe_name))?;
    // The builder doubles as the launcher: copy ourselves next to the game.
    let launcher_name = if cfg!(windows) { "play.exe" } else { "play" };
    std::fs::copy(std::env::current_exe()?, bundle.join(launcher_name))?;
    let provenance = json!({
        "format_version": 1, "rom": info, "executable": exe_name, "launcher": launcher_name,
        "executable_sha256": sha256_file(&executable)?, "cargo_lock_sha256": sha256_file(&lock)?,
        "build_command": args, "assets": "read directly from external ROM; no extraction",
    });
    std::fs::write(
        bundle.join("manifest.json"),
        serde_json::to_string_pretty(&provenance).unwrap() + "\n",
    )?;
    if output.exists() {
        std::fs::remove_dir(&output)?; // only an empty directory is ever removed
    }
    std::fs::rename(&bundle, &output)?;
    log(format!("Built: {}", output.display()));
    Ok(output)
}

/// Launcher mode: verify the ROM hash recorded in the bundle, then run the game.
pub fn launch(bundle: &Path, extra: &[String]) -> Result<i32, BuildError> {
    launch_with_env(bundle, extra, &[])
}

/// Launch with child-only environment overrides; None removes an inherited variable.
pub fn launch_with_env(
    bundle: &Path,
    extra: &[String],
    env: &[(&str, Option<&str>)],
) -> Result<i32, BuildError> {
    let m: serde_json::Value =
        serde_json::from_slice(&std::fs::read(bundle.join("manifest.json"))?)
            .map_err(|e| BuildError(format!("Bad manifest: {e}")))?;
    let rom = m["rom"]["path"]
        .as_str()
        .ok_or_else(|| BuildError("Manifest lacks ROM path.".into()))?;
    if let Some(recorded) = m["assets"]["assets_sha256"].as_str() {
        let got = mkds_builder::rip::verify(
            &bundle.join(m["assets"]["path"].as_str().unwrap_or("assets")),
        )?;
        if got != recorded {
            return err(
                "Assets differ from the ones this game was built with. Rebuild from your ROM.",
            );
        }
    }
    // Bundles whose game reads the ROM (`{rom}` in launch_args, or legacy without launch_args) need it
    // unchanged; ROM-free bundles rely on the verified assets instead.
    let needs_rom = match m["launch_args"].as_array() {
        Some(a) => a.iter().any(|v| v == "{rom}"),
        None => true,
    };
    if needs_rom {
        let want = m["rom"]["sha256"].as_str().unwrap_or_default();
        if !Path::new(rom).is_file() {
            return err(format!(
                "The ROM this bundle was built from is missing: {rom}"
            ));
        }
        if sha256_file(Path::new(rom))? != want {
            return err("ROM changed since build; run the ROM builder again.");
        }
    }
    let exe = m["executable"]
        .as_str()
        .ok_or_else(|| BuildError("Manifest lacks executable.".into()))?;
    if exe.contains(['/', '\\']) {
        return err("Manifest executable must be a plain file name.");
    }
    let mut command = launch_command(bundle, &m, exe, rom, extra);
    for (key, value) in env {
        if let Some(value) = value {
            command.env(key, value);
        } else {
            command.env_remove(key);
        }
    }
    let status = command.status()?;
    Ok(status.code().unwrap_or(1))
}

/// The game command for a verified bundle: `launch_args` from the manifest with `{rom}` (the verified
/// ROM) and `{assets}` (the bundle's ripped assets) substituted, then the user's extra arguments.
/// Bundles without `launch_args` (the legacy reference port) get `extra --rom <rom>`.
fn launch_command(
    bundle: &Path,
    m: &serde_json::Value,
    exe: &str,
    rom: &str,
    extra: &[String],
) -> Command {
    let mut command = Command::new(bundle.join(exe));
    match m["launch_args"].as_array() {
        Some(template) => {
            let assets = bundle.join(m["assets"]["path"].as_str().unwrap_or("assets"));
            for a in template.iter().filter_map(|a| a.as_str()) {
                match a {
                    "{rom}" => command.arg(rom),
                    "{assets}" => command.arg(&assets),
                    other => command.arg(other),
                };
            }
            command.args(extra);
        }
        None => {
            command.args(extra).arg("--rom").arg(rom);
        }
    }
    for key in ["MKDS_HW_FS", "MKDS_HW_ARM7", "MKDS_HW_TIME", "MKDS_HW_SND"] {
        if std::env::var_os(key).is_none() {
            command.env(key, "native");
        }
    }
    command.current_dir(bundle);
    command
}

/// The function dictionary's tables in `source`, in override order: `dictionary/functions.csv` (exported
/// from the mapping work), then the other `dictionary/*.csv` tables (hand-refined: SDK, game systems) by
/// name. Missing dictionary tables simply leave functions unnamed.
fn dictionary_tables(source: &Path) -> Vec<PathBuf> {
    let dir = source.join("dictionary");
    let base = dir.join("functions.csv");
    let mut refined: Vec<PathBuf> = std::fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok().map(|e| e.path()))
        // name tables only: `sdk_*.csv` and `game_*.csv` (fields, field uses and globals are layout tables)
        .filter(|p| {
            let n = p
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            n.ends_with(".csv") && (n.starts_with("sdk_") || n.starts_with("game_"))
        })
        .collect();
    refined.sort();
    let tables: Vec<PathBuf> = [base]
        .into_iter()
        .filter(|p| p.is_file())
        .chain(refined)
        .collect();
    tables
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dictionary_tables_come_in_override_order() {
        let d = tempfile::tempdir().unwrap();
        assert!(dictionary_tables(d.path()).is_empty());
        let dict = d.path().join("dictionary");
        std::fs::create_dir(&dict).unwrap();
        for f in [
            "sdk_functions.csv",
            "functions.csv",
            "game_karts.csv",
            "structs.csv",
            "fields.csv",
            "globals.csv",
            "export.py",
        ] {
            std::fs::write(dict.join(f), "").unwrap();
        }
        let names: Vec<_> = dictionary_tables(d.path())
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            ["functions.csv", "game_karts.csv", "sdk_functions.csv"]
        );
    }

    #[cfg(unix)]
    #[test]
    fn staged_unix_executable_launches_with_spaced_paths_and_arguments() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let source = d.path().join("source files");
        let bundle = d.path().join("bundle with spaces");
        std::fs::create_dir(&source).unwrap();
        let game = source.join("game");
        std::fs::write(
            &game,
            "#!/bin/sh\n[ \"$1\" = '--label' ] && [ \"$2\" = 'two words' ] || exit 9\nexit 23\n",
        )
        .unwrap();
        std::fs::set_permissions(&game, std::fs::Permissions::from_mode(0o755)).unwrap();
        copy_tree(&source, &bundle).unwrap();
        assert_ne!(
            std::fs::metadata(bundle.join("game"))
                .unwrap()
                .permissions()
                .mode()
                & 0o111,
            0
        );
        let m = json!({"rom": {"path": "absent.nds"}, "executable": "game", "launch_args": []});
        std::fs::write(bundle.join("manifest.json"), m.to_string()).unwrap();
        assert_eq!(
            launch(&bundle, &["--label".into(), "two words".into()]).unwrap(),
            23
        );
    }

    #[test]
    fn launcher_rejects_path_like_executable_names() {
        let d = tempfile::tempdir().unwrap();
        let rom = d.path().join("x.nds");
        std::fs::write(&rom, b"abc").unwrap();
        let m = json!({"rom": {"path": rom, "sha256": sha256_file(&rom).unwrap()}, "executable": "sub/evil.exe"});
        std::fs::write(d.path().join("manifest.json"), m.to_string()).unwrap();
        assert!(launch(d.path(), &[]).is_err());
    }

    fn fake_info(rom: &Path, reference_match: bool) -> RomInfo {
        RomInfo {
            path: rom.to_string_lossy().into_owned(),
            size: 3,
            game_code: "AMCE".into(),
            revision: 0,
            sha256: sha256_file(rom).unwrap(),
            arm9_sha256: String::new(),
            compatibility: "test".into(),
            reference_match,
            file_count: 0,
            courses: vec![],
        }
    }

    fn options(source: &Path, output: &Path) -> RecompileOptions {
        RecompileOptions {
            source: source.to_path_buf(),
            output: output.to_path_buf(),
            allow_unverified: false,
            offline: true,
            allow_incomplete: false,
            keep_source: false,
            with_acceptance: false,
            jobs: None,
            cache_dir: None,
            with_interpreter: false,
        }
    }

    #[test]
    fn native_build_refuses_unverified_rom_without_opt_in() {
        let d = tempfile::tempdir().unwrap();
        let rom = d.path().join("x.nds");
        std::fs::write(&rom, b"abc").unwrap();
        let out = d.path().join("out");
        let e = recompile(
            &fake_info(&rom, false),
            &options(d.path(), &out),
            &mut |_| {},
        );
        assert!(e.unwrap_err().0.contains("differs from the reference"));
        assert!(!out.exists());
    }

    #[test]
    fn native_build_detects_changed_rom_and_leaves_nothing() {
        let d = tempfile::tempdir().unwrap();
        let rom = d.path().join("x.nds");
        std::fs::write(&rom, b"abc").unwrap();
        let info = fake_info(&rom, true);
        std::fs::write(&rom, b"changed").unwrap();
        let out = d.path().join("out");
        let e = recompile(&info, &options(d.path(), &out), &mut |_| {});
        assert!(e.unwrap_err().0.contains("ROM changed"));
        assert!(!out.exists());
    }

    #[test]
    fn native_build_never_touches_a_nonempty_output() {
        let d = tempfile::tempdir().unwrap();
        let rom = d.path().join("x.nds");
        std::fs::write(&rom, b"abc").unwrap();
        let out = d.path().join("out");
        std::fs::create_dir(&out).unwrap();
        std::fs::write(out.join("keep.txt"), b"mine").unwrap();
        let e = recompile(
            &fake_info(&rom, true),
            &options(d.path(), &out),
            &mut |_| {},
        );
        assert!(e.unwrap_err().0.contains("Output must be empty"));
        assert_eq!(std::fs::read(out.join("keep.txt")).unwrap(), b"mine");
    }

    #[test]
    fn native_build_needs_a_real_checkout() {
        let d = tempfile::tempdir().unwrap();
        let rom = d.path().join("x.nds");
        std::fs::write(&rom, b"abc").unwrap();
        let out = d.path().join("out");
        let e = recompile(
            &fake_info(&rom, true),
            &options(d.path(), &out),
            &mut |_| {},
        );
        assert!(e.unwrap_err().0.contains("Source checkout needs"));
        assert!(!out.exists());
    }

    #[test]
    fn launcher_substitutes_verified_rom_into_manifest_args() {
        let d = tempfile::tempdir().unwrap();
        let rom = d.path().join("x.nds");
        std::fs::write(&rom, b"abc").unwrap();
        // the "game" is the builder's own test harness exiting 0 regardless of arguments
        let m = json!({"rom": {"path": rom, "sha256": sha256_file(&rom).unwrap()},
                       "executable": "missing-game.exe", "launch_args": ["{rom}"]});
        std::fs::write(d.path().join("manifest.json"), m.to_string()).unwrap();
        // a missing executable is an error, not a silent success
        assert!(launch(d.path(), &[]).is_err());
    }

    #[test]
    fn launcher_reports_missing_rom_by_path() {
        let d = tempfile::tempdir().unwrap();
        let rom = d.path().join("gone.nds");
        let m = json!({"rom": {"path": rom, "sha256": "00"}, "executable": "game.exe"});
        std::fs::write(d.path().join("manifest.json"), m.to_string()).unwrap();
        assert!(launch(d.path(), &[]).unwrap_err().0.contains("missing"));
    }

    #[test]
    fn finds_source_upward() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("3_hand_port/game")).unwrap();
        std::fs::write(d.path().join("3_hand_port/game/Cargo.toml"), "").unwrap();
        std::fs::create_dir_all(d.path().join("a/b")).unwrap();
        assert_eq!(
            find_source(&d.path().join("a/b")).as_deref(),
            Some(d.path())
        );
    }

    #[test]
    fn launch_args_substitute_rom_and_assets_then_user_arguments() {
        let bundle = Path::new("C:/bundle dir");
        let m =
            json!({"launch_args": ["--assets", "{assets}", "{rom}"], "assets": {"path": "assets"}});
        let c = launch_command(
            bundle,
            &m,
            "game.exe",
            "C:/roms/my game.nds",
            &["--mute".into()],
        );
        let args: Vec<String> = c
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            vec![
                "--assets".to_string(),
                bundle.join("assets").to_string_lossy().into_owned(),
                "C:/roms/my game.nds".into(),
                "--mute".into()
            ]
        );
        let legacy = launch_command(bundle, &json!({}), "mkds_game.exe", "r.nds", &[]);
        let args: Vec<String> = legacy
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args, vec!["--rom", "r.nds"]);
    }

    #[test]
    fn sync_rewrites_only_changed_files_and_removes_stale_ones() {
        let d = tempfile::tempdir().unwrap();
        let (src, dst) = (d.path().join("src"), d.path().join("dst"));
        std::fs::create_dir_all(src.join("part_00/src")).unwrap();
        std::fs::write(src.join("part_00/src/lib.rs"), "same").unwrap();
        std::fs::write(src.join("Cargo.toml"), "v1").unwrap();
        assert_eq!(sync_tree(&src, &dst).unwrap(), (2, 0, 0));
        let before = std::fs::metadata(dst.join("part_00/src/lib.rs"))
            .unwrap()
            .modified()
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(src.join("Cargo.toml"), "v2").unwrap();
        std::fs::create_dir_all(dst.join("part_99")).unwrap();
        std::fs::create_dir_all(dst.join("target/keep")).unwrap();
        assert_eq!(sync_tree(&src, &dst).unwrap(), (1, 1, 1));
        let after = std::fs::metadata(dst.join("part_00/src/lib.rs"))
            .unwrap()
            .modified()
            .unwrap();
        assert_eq!(before, after, "unchanged file keeps its timestamp");
        assert_eq!(
            std::fs::read_to_string(dst.join("Cargo.toml")).unwrap(),
            "v2"
        );
        assert!(!dst.join("part_99").exists() && dst.join("target/keep").exists());
    }

    #[test]
    fn cache_lock_is_exclusive_and_released() {
        let d = tempfile::tempdir().unwrap();
        let first = lock_cache(d.path()).unwrap();
        assert!(
            lock_cache(d.path())
                .unwrap_err()
                .0
                .contains("Another build is using the cache")
        );
        drop(first);
        assert!(lock_cache(d.path()).is_ok());
    }

    #[test]
    fn rom_free_bundle_launches_without_the_rom_but_still_checks_assets() {
        let d = tempfile::tempdir().unwrap();
        let ripped = mkds_builder::rip::rip(
            &sample_rom_for_launch(),
            "00",
            &d.path().join("b"),
            &mut |_| {},
        )
        .unwrap();
        let bundle = d.path().join("b");
        let m = json!({"rom": {"path": d.path().join("gone.nds"), "sha256": "00"},
                       "executable": "missing-game.exe", "launch_args": ["--assets", "{assets}"],
                       "assets": {"path": "assets", "assets_sha256": ripped.manifest["assets_sha256"]}});
        std::fs::write(bundle.join("manifest.json"), m.to_string()).unwrap();
        // ROM absent: the launcher gets past every check and fails only to start the (missing) game
        let e = launch(&bundle, &[]).unwrap_err().0;
        assert!(!e.contains("ROM"), "{e}");
        std::fs::write(bundle.join("assets/files/a"), b"edited").unwrap();
        assert!(
            launch(&bundle, &[])
                .unwrap_err()
                .0
                .contains("Asset changed")
        );
    }

    /// Minimal ROM image for the launcher tests: header + one named file "a".
    fn sample_rom_for_launch() -> Vec<u8> {
        let mut fnt = Vec::new();
        fnt.extend_from_slice(&8u32.to_le_bytes());
        fnt.extend_from_slice(&0u16.to_le_bytes());
        fnt.extend_from_slice(&1u16.to_le_bytes());
        fnt.extend_from_slice(&[1, b'a', 0]);
        let mut rom = vec![0u8; 0x200];
        let fnt_off = rom.len() as u32;
        rom.extend_from_slice(&fnt);
        let a = rom.len() as u32;
        rom.extend_from_slice(b"xy");
        let fat_off = rom.len() as u32;
        rom.extend_from_slice(&a.to_le_bytes());
        rom.extend_from_slice(&(a + 2).to_le_bytes());
        rom[0x40..0x44].copy_from_slice(&fnt_off.to_le_bytes());
        rom[0x44..0x48].copy_from_slice(&(fnt.len() as u32).to_le_bytes());
        rom[0x48..0x4C].copy_from_slice(&fat_off.to_le_bytes());
        rom[0x4C..0x50].copy_from_slice(&8u32.to_le_bytes());
        rom
    }

    #[test]
    fn generated_crates_drop_recomp_default_features_once() {
        let d = tempfile::tempdir().unwrap();
        for (name, deps) in [
            ("part_00", "recomp = { path = \"C:/x/recomp\" }\n"),
            (
                "game",
                "recomp = { path = \"C:/x/recomp\" }\nlifted = { path = \"../lifted\" }\nmkds_native = { path = \"C:/x/native\" }\n",
            ),
        ] {
            std::fs::create_dir_all(d.path().join(name)).unwrap();
            std::fs::write(
                d.path().join(name).join("Cargo.toml"),
                format!("[package]\nname = \"{name}\"\n\n[dependencies]\n{deps}"),
            )
            .unwrap();
        }
        assert_eq!(recomp_without_defaults(d.path(), false).unwrap(), 3);
        let game = std::fs::read_to_string(d.path().join("game/Cargo.toml")).unwrap();
        assert!(
            game.contains("recomp = { path = \"C:/x/recomp\", default-features = false }"),
            "{game}"
        );
        assert!(
            game.contains("mkds_native = { path = \"C:/x/native\", default-features = false }")
        );
        assert!(game.contains("lifted = { path = \"../lifted\" }"));
        assert_eq!(
            recomp_without_defaults(d.path(), false).unwrap(),
            0,
            "idempotent"
        );
        let e = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(e.path().join("game")).unwrap();
        std::fs::write(
            e.path().join("game/Cargo.toml"),
            "[dependencies]\nrecomp = { path = \"r\" }\n",
        )
        .unwrap();
        recomp_without_defaults(e.path(), true).unwrap();
        assert!(
            std::fs::read_to_string(e.path().join("game/Cargo.toml"))
                .unwrap()
                .contains("default-features = false, features = [\"interpreter\"] }")
        );
    }
}
