//! Windows inbox tools only: no Rust installation, VS, shell installer or global PATH edits. On Linux the official
//! Rust parts go into the same data folder (install-toolchain.sh); the C compiler and headers come from the system.
use mkds_builder::build::Toolchain;
use std::{
    io::BufReader,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};
pub const VERSION: &str = "1.96.0";
#[cfg(windows)]
pub const TARGET: &str = mkds_builder::build::PORTABLE_GNU_TARGET;
#[cfg(not(windows))]
pub const TARGET: &str = "x86_64-unknown-linux-gnu";
/// llvm-mingw (github.com/mstorsjo/llvm-mingw) supplies the dlltool for raw-dylib imports and the C/C++ compiler
/// (clang, headers, x86_64 sysroot) for crates whose build scripts compile C. Pinned release and the SHA-256 GitHub
/// publishes for `llvm-mingw-<release>-ucrt-x86_64.zip`.
#[cfg(windows)]
pub const LLVM_MINGW: &str = "20261020";
#[cfg(windows)]
pub const LLVM_MINGW_SHA256: &str = "fd5462b7f5d93b2ff981fcd3f9c7e427e47f3203c64ea91c27bcc444eebd74cd";
pub fn root(data: &Path) -> PathBuf {
    data.join("toolchain").join(format!("{VERSION}-{TARGET}"))
}
#[cfg(windows)]
fn llvm_mingw(dir: &Path) -> PathBuf {
    dir.join("llvm-mingw")
}
/// Written by install-toolchain.ps1 once every probe passed. Older markers (no " cc": llvm-mingw `bin/` only, no C
/// compiler) make `install` run again, which reuses the installed Rust and the cached llvm-mingw archive.
#[cfg(windows)]
fn marker() -> String {
    format!("{VERSION} llvm-mingw-{LLVM_MINGW} cc")
}
/// Written by install-toolchain.sh once rustc and cargo ran.
#[cfg(not(windows))]
fn marker() -> String {
    format!("{VERSION} linux")
}
#[cfg(not(windows))]
pub fn ready(data: &Path) -> bool {
    let complete = std::fs::read_to_string(root(data).join("complete")).unwrap_or_default();
    complete.trim() == marker()
        && ["bin/rustc", "bin/cargo", "lib/rustlib/x86_64-unknown-linux-gnu/lib"].iter().all(|p| root(data).join(p).exists())
}
#[cfg(windows)]
pub fn ready(data: &Path) -> bool {
    let complete = std::fs::read_to_string(root(data).join("complete")).unwrap_or_default();
    complete.trim() == marker()
        && [
            "bin/rustc.exe",
            "bin/cargo.exe",
            "lib/rustlib/x86_64-pc-windows-gnu/lib",
            "lib/rustlib/x86_64-pc-windows-gnu/bin/rust-lld.exe",
            "llvm-mingw/bin/x86_64-w64-mingw32-dlltool.exe",
            "llvm-mingw/bin/x86_64-w64-mingw32-clang.exe",
            "llvm-mingw/bin/llvm-ar.exe",
            "llvm-mingw/include/assert.h",
            "llvm-mingw/x86_64-w64-mingw32/lib",
        ]
        .iter()
        .all(|p| root(data).join(p).exists())
}
fn forward(reader: impl std::io::Read, tx: std::sync::mpsc::Sender<String>) {
    use std::io::Read;
    let mut pending = Vec::new();
    for byte in BufReader::new(reader).bytes().map_while(Result::ok) {
        if byte == b'\r' || byte == b'\n' {
            if !pending.is_empty() {
                let _ = tx.send(String::from_utf8_lossy(&pending).into_owned());
                pending.clear();
            }
        } else {
            pending.push(byte);
        }
    }
    if !pending.is_empty() {
        let _ = tx.send(String::from_utf8_lossy(&pending).into_owned());
    }
}
fn run(command: &mut Command, log: &mut dyn FnMut(String)) -> Result<(), String> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    let stderr = child.stderr.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let out = tx.clone();
    let stdout = child.stdout.take().unwrap();
    let a = std::thread::spawn(move || forward(stdout, out));
    let b = std::thread::spawn(move || forward(stderr, tx));
    for s in rx {
        log(s);
    }
    let _ = a.join();
    let _ = b.join();
    if child.wait().map_err(|e| e.to_string())?.success() {
        Ok(())
    } else {
        Err("Download or extraction failed. Check internet access and available disk space, then retry. Verified downloads are retained.".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn walk(d: &std::path::Path) -> Vec<std::path::PathBuf> {
        let mut out = Vec::new();
        for e in std::fs::read_dir(d).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() { out.extend(walk(&p)) } else { out.push(p) }
        }
        out
    }
    #[test]
    fn embedded_source_is_complete_and_has_no_game_payload() {
        let data = tempfile::tempdir().unwrap();
        let dir = source(data.path(), &mut |_| {}).unwrap();
        for file in [
            "1_build_tool/Cargo.toml",
            "2_native_game/Cargo.lock",
            "2_native_game/native/Cargo.lock",
            "dictionary/FUNCTION_BOUNDARIES.csv",
            "2_native_game/native/src/lib.rs",
        ] {
            assert!(dir.join(file).is_file(), "{file}");
        }
        // Only the project's own window icon may come from assets/, never game assets.
        assert!(dir.join("assets/branding/icon.png").is_file());
        let assets: Vec<_> = walk(&dir.join("assets"));
        assert_eq!(assets, vec![dir.join("assets/branding/icon.png")]);
        assert!(!dir.join("2_native_game/target").exists());
        assert_eq!(
            dir,
            source(data.path(), &mut |_| panic!("must reuse extracted source")).unwrap()
        );
    }
    #[cfg(windows)]
    #[test]
    fn portable_toolchain_uses_llvm_mingw_and_lld() {
        let data = tempfile::tempdir().unwrap();
        let dir = root(data.path());
        let t = toolchain(&dir);
        assert_eq!(t.cargo, dir.join("bin/cargo.exe"));
        let flags = t.rustflags.join(" ");
        assert!(flags.contains("llvm-mingw") && flags.contains("dlltool.exe"));
        assert!(flags.contains("rust-lld.exe") && !flags.contains("--target"));
        assert!(t.path.contains(&dir.join("llvm-mingw/bin")));
        assert!(!ready(data.path()));
        for p in [
            "bin/rustc.exe",
            "bin/cargo.exe",
            "lib/rustlib/x86_64-pc-windows-gnu/lib/x",
            "lib/rustlib/x86_64-pc-windows-gnu/bin/rust-lld.exe",
            "llvm-mingw/bin/x86_64-w64-mingw32-dlltool.exe",
            "llvm-mingw/bin/x86_64-w64-mingw32-clang.exe",
            "llvm-mingw/bin/llvm-ar.exe",
            "llvm-mingw/include/assert.h",
            "llvm-mingw/x86_64-w64-mingw32/lib/x",
        ] {
            std::fs::create_dir_all(dir.join(p).parent().unwrap()).unwrap();
            std::fs::write(dir.join(p), "").unwrap();
        }
        // An older install (Rust only) is completed again with llvm-mingw.
        std::fs::write(dir.join("complete"), VERSION).unwrap();
        assert!(!ready(data.path()));
        // v0.2.0 installs (llvm-mingw bin/ only, no C compiler) are repaired too.
        std::fs::write(dir.join("complete"), format!("{VERSION} llvm-mingw-{LLVM_MINGW}\r\n")).unwrap();
        assert!(!ready(data.path()));
        std::fs::write(dir.join("complete"), format!("{VERSION} llvm-mingw-{LLVM_MINGW} cc\r\n")).unwrap();
        assert!(ready(data.path()));
        std::fs::remove_file(dir.join("llvm-mingw/include/assert.h")).unwrap();
        assert!(!ready(data.path()));
        let cc: std::collections::HashMap<_, _> = t.env.iter().cloned().collect();
        assert!(cc["CC_x86_64_pc_windows_gnu"].ends_with("llvm-mingw/bin/x86_64-w64-mingw32-clang.exe"));
        assert!(cc["CXX"].ends_with("llvm-mingw/bin/x86_64-w64-mingw32-clang++.exe"));
        assert!(cc["AR"].ends_with("llvm-mingw/bin/llvm-ar.exe"));
        assert_eq!(LLVM_MINGW_SHA256.len(), 64);
    }
    #[test]
    fn progress_splits_carriage_returns_and_newlines() {
        let (tx, rx) = std::sync::mpsc::channel();
        forward(&b"10%\r20%\r\nverified\ncomplete"[..], tx);
        assert_eq!(
            rx.into_iter().collect::<Vec<_>>(),
            ["10%", "20%", "verified", "complete"]
        );
    }
}
pub fn source(data: &Path, log: &mut dyn FnMut(String)) -> Result<PathBuf, String> {
    use sha2::{Digest, Sha256};
    let bytes = include_bytes!(concat!(env!("OUT_DIR"), "/source.tar.gz"));
    let id = mkds_builder::rom::hex(&Sha256::digest(bytes));
    let dir = data.join("src").join(&id[..16]);
    if dir.join("complete").is_file() {
        return Ok(dir);
    }
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let archive = dir.join("source.tar.gz");
    std::fs::write(&archive, bytes).map_err(|e| e.to_string())?;
    log("Preparing embedded project source (no game code or ROM included).".into());
    run(
        Command::new("tar")
            .arg("-xzf")
            .arg(&archive)
            .arg("-C")
            .arg(&dir),
        log,
    )?;
    std::fs::write(dir.join("complete"), id).map_err(|e| e.to_string())?;
    Ok(dir)
}
#[cfg(not(windows))]
pub fn install(data: &Path, log: &mut dyn FnMut(String)) -> Result<Toolchain, String> {
    let dir = root(data);
    if !ready(data) {
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let script = dir.join("install.sh");
        std::fs::write(&script, include_str!("../install-toolchain.sh")).map_err(|e| e.to_string())?;
        run(Command::new("sh").arg(script).arg(&dir).arg(VERSION), log).map_err(|_| {
            "Toolchain setup stopped. The activity log says why: missing system packages are listed with the \
             command that installs them; downloads resume on retry."
                .to_string()
        })?;
    }
    drop_downloads(&dir);
    Ok(toolchain(&dir))
}
/// The system's `cc` links (Rust's default on Linux); only rustc and cargo come from the data folder.
#[cfg(not(windows))]
fn toolchain(dir: &Path) -> Toolchain {
    Toolchain {
        cargo: dir.join("bin/cargo"),
        rustc: dir.join("bin/rustc"),
        rustflags: Vec::new(),
        path: vec![dir.join("bin")],
        env: Vec::new(),
    }
}
#[cfg(windows)]
pub fn install(data: &Path, log: &mut dyn FnMut(String)) -> Result<Toolchain, String> {
    let dir = root(data);
    if !ready(data) {
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let script = dir.join("install.ps1");
        std::fs::write(&script, include_str!("../install-toolchain.ps1"))
            .map_err(|e| e.to_string())?;
        run(
            Command::new("powershell.exe")
                .args([
                    "-NoProfile",
                    "-NonInteractive",
                    "-ExecutionPolicy",
                    "Bypass",
                    "-File",
                ])
                .arg(script)
                .arg("-Destination")
                .arg(&dir)
                .arg("-Version")
                .arg(VERSION)
                .arg("-LlvmMingw")
                .arg(LLVM_MINGW)
                .arg("-LlvmMingwSha256")
                .arg(LLVM_MINGW_SHA256),
            log,
        )?;
    }
    drop_downloads(&dir);
    Ok(toolchain(&dir))
}
/// The verified installer archives (about 120 MB on Linux, 320 MB on Windows) are only needed while installing: once
/// the toolchain checks out they are removed. A later repair downloads them again.
fn drop_downloads(dir: &Path) {
    if ready_at(dir) {
        let _ = std::fs::remove_dir_all(dir.join("downloads"));
    }
}
/// [`ready`] for a toolchain folder.
fn ready_at(dir: &Path) -> bool {
    dir.parent().and_then(Path::parent).is_some_and(|data| root(data) == dir && ready(data))
}
/// GNU build: llvm-mingw's dlltool, rust-lld as the linker (GNU ld mislinks raw-dylib imports and the game
/// crashes at start-up), no `--target`; see `Toolchain::portable_gnu`.
#[cfg(windows)]
fn toolchain(dir: &Path) -> Toolchain {
    Toolchain::portable_gnu(dir, &llvm_mingw(dir))
}


/// The generator that came with this launcher (release builds), unpacked once per embedded source next to the build
/// cache; `None` when the launcher carries none (development builds), and the builder compiles it.
pub fn generator(data: &Path, source: &Path) -> Option<PathBuf> {
    let bytes: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/mkds_gen.bin"));
    if bytes.is_empty() {
        return None;
    }
    let id = source.file_name()?;
    let exe = data.join("cache/generator").join(id).join(if cfg!(windows) { "mkds_gen.exe" } else { "mkds_gen" });
    if std::fs::metadata(&exe).map(|m| m.len()).ok() != Some(bytes.len() as u64) {
        std::fs::create_dir_all(exe.parent()?).ok()?;
        let part = exe.with_extension("part");
        std::fs::write(&part, bytes).ok()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&part, std::fs::Permissions::from_mode(0o755)).ok()?;
        }
        std::fs::rename(&part, &exe).ok()?;
    }
    Some(exe)
}

/// Select storage before any cached shared path is read. Migration always preserves originals.
pub fn initialize() {
    let Some(exe) = std::env::current_exe().ok() else { return };
    // Linux: the XDG data folder (~/.local/share/MKDSrust) when the launcher's own folder is not writable.
    let legacy = if cfg!(windows) { std::env::var_os("APPDATA").map(PathBuf::from) } else {
        std::env::var_os("XDG_DATA_HOME").map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
    }.unwrap_or_else(|| PathBuf::from(".")).join("MKDSrust");
    let disabled = std::env::var_os("MKDSRUST_NOT_PORTABLE").is_some()
        || legacy.join("MKDSRUST_NOT_PORTABLE").exists();
    let (data, portable) = mkds_builder::paths::select(&exe, &legacy, disabled);
    if portable && legacy.is_dir() && std::fs::read_dir(&data).map(|mut d| d.next().is_none()).unwrap_or(false) {
        let answer = rfd::MessageDialog::new().set_title("Existing MKDSrust data")
            .set_description("Copy existing data into this portable install? Yes: settings, controls, saves and launcher preferences. No: copy everything, including toolchain and cache (may take several minutes). Cancel: start fresh. Originals are always retained.")
            .set_buttons(rfd::MessageButtons::YesNoCancel).show();
        let result = match answer {
            rfd::MessageDialogResult::Yes => {
                (|| -> std::io::Result<()> {
                    for name in ["settings.txt", "controls.txt", "launcher.json", "saves"] {
                        let src = legacy.join(name); let dst = data.join(name);
                        if src.is_dir() { mkds_builder::controls::migrate_data(&src, &dst)?; }
                        else if src.is_file() { std::fs::copy(src, dst)?; }
                    }
                    Ok(())
                })()
            }
            rfd::MessageDialogResult::No => {
                mkds_builder::controls::migrate_data(&legacy, &data).and_then(|_| {
                    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
                        mkds_builder::controls::migrate_data(&PathBuf::from(local).join("MKDSrust/cache"), &data.join("cache"))?;
                    }
                    Ok(())
                })
            }
            _ => Ok(()),
        };
        if let Err(e) = result {
            rfd::MessageDialog::new().set_title("Migration incomplete").set_description(format!("{e}\nOriginal data is intact. Copied files remain available.")).show();
        }
    }
    if portable {
        let prefs = data.join("launcher.json");
        if let Ok(bytes) = std::fs::read(&prefs) {
            if let Ok(mut value) = serde_json::from_slice::<serde_json::Value>(&bytes) {
                if let Some(output) = value["output"].as_str() {
                    let path = PathBuf::from(output);
                    if path.is_absolute() && !path.exists() {
                        value["output"] = serde_json::Value::String(exe.parent().unwrap().join("games").join(path.file_name().unwrap_or_default()).to_string_lossy().into_owned());
                        if let Ok(bytes) = serde_json::to_vec_pretty(&value) { let _ = std::fs::write(&prefs, bytes); }
                    }
                }
            }
        }
    }
    // Initialization runs before worker threads or egui start.
    unsafe {
        std::env::set_var("MKDS_DATA_DIR", &data);
        if portable {
            std::env::set_var("MKDS_PORTABLE", "1");
            std::env::set_var("CARGO_HOME", data.join("cache/cargo"));
            let temp = data.join("cache/tmp");
            if std::fs::create_dir_all(&temp).is_ok() {
                std::env::set_var("TEMP", &temp);
                std::env::set_var("TMP", &temp);
                std::env::set_var("TMPDIR", &temp);
            }
        }
        else { std::env::remove_var("MKDS_PORTABLE"); }
    }
}

