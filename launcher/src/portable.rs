//! Windows inbox tools only: no Rust installation, VS, shell installer or global PATH edits.
use mkds_builder::build::Toolchain;
use std::{
    io::BufReader,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};
pub const VERSION: &str = "1.96.0";
pub const TARGET: &str = "x86_64-pc-windows-gnu";
pub fn root(data: &Path) -> PathBuf {
    data.join("toolchain").join(format!("{VERSION}-{TARGET}"))
}
pub fn ready(data: &Path) -> bool {
    [
        "complete",
        "bin/rustc.exe",
        "bin/cargo.exe",
        "lib/rustlib/x86_64-pc-windows-gnu/lib",
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
                .arg(VERSION),
            log,
        )?;
    }
    Ok(Toolchain {
        cargo: dir.join("bin/cargo.exe"),
        rustc: dir.join("bin/rustc.exe"),
        target: TARGET.into(),
    })
}
