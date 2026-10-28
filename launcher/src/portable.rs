//! Windows inbox tools only: no Rust installation, VS, shell installer or global PATH edits.
use mkds_builder::build::Toolchain;
use std::{
    io::BufReader,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};
pub const VERSION: &str = "1.96.0";
pub const TARGET: &str = mkds_builder::build::PORTABLE_GNU_TARGET;
/// llvm-mingw (github.com/mstorsjo/llvm-mingw) supplies the dlltool for raw-dylib imports. Pinned release and the
/// SHA-256 GitHub publishes for `llvm-mingw-<release>-ucrt-x86_64.zip`.
pub const LLVM_MINGW: &str = "20261020";
pub const LLVM_MINGW_SHA256: &str = "fd5462b7f5d93b2ff981fcd3f9c7e427e47f3203c64ea91c27bcc444eebd74cd";
pub fn root(data: &Path) -> PathBuf {
    data.join("toolchain").join(format!("{VERSION}-{TARGET}"))
}
fn llvm_mingw(dir: &Path) -> PathBuf {
    dir.join("llvm-mingw")
}
pub fn ready(data: &Path) -> bool {
    let complete = std::fs::read_to_string(root(data).join("complete")).unwrap_or_default();
    complete.trim() == format!("{VERSION} llvm-mingw-{LLVM_MINGW}")
        && [
            "bin/rustc.exe",
            "bin/cargo.exe",
            "lib/rustlib/x86_64-pc-windows-gnu/lib",
            "lib/rustlib/x86_64-pc-windows-gnu/bin/rust-lld.exe",
            "llvm-mingw/bin/x86_64-w64-mingw32-dlltool.exe",
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
        assert!(!dir.join("assets").exists());
        assert!(!dir.join("2_native_game/target").exists());
        assert_eq!(
            dir,
            source(data.path(), &mut |_| panic!("must reuse extracted source")).unwrap()
        );
    }
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
        ] {
            std::fs::create_dir_all(dir.join(p).parent().unwrap()).unwrap();
            std::fs::write(dir.join(p), "").unwrap();
        }
        // An older install (Rust only) is completed again with llvm-mingw.
        std::fs::write(dir.join("complete"), VERSION).unwrap();
        assert!(!ready(data.path()));
        std::fs::write(dir.join("complete"), format!("{VERSION} llvm-mingw-{LLVM_MINGW}\r\n")).unwrap();
        assert!(ready(data.path()));
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
    let dir = data.join("source").join(&id[..16]);
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
    Ok(toolchain(&dir))
}
/// GNU build: llvm-mingw's dlltool, rust-lld as the linker (GNU ld mislinks raw-dylib imports and the game
/// crashes at start-up), no `--target`; see `Toolchain::portable_gnu`.
fn toolchain(dir: &Path) -> Toolchain {
    Toolchain::portable_gnu(dir, &llvm_mingw(dir))
}
