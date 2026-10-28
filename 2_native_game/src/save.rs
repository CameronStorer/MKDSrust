//! Flat cartridge images. Only hosts explicitly attaching a path enable disk I/O.
use std::{fs, io::{self, Write}, path::{Path, PathBuf}, time::{Duration, Instant}};

pub struct Save {
    path: PathBuf,
    pub dirty: bool,
    last: Instant,
}

impl Save {
    pub fn load(path: PathBuf, import: Option<&Path>, data: &mut [u8]) -> io::Result<Self> {
        let bytes = match fs::read(&path) {
            Ok(bytes) => Some(bytes),
            Err(e) if e.kind() == io::ErrorKind::NotFound => match import {
                Some(p) => match fs::read(p) {
                    Ok(b) => Some(b),
                    Err(e) if e.kind() == io::ErrorKind::NotFound => None,
                    Err(e) => return Err(e),
                },
                None => None,
            },
            Err(e) => return Err(e),
        };
        let imported = bytes.is_some() && !path.exists();
        if let Some(bytes) = bytes {
            if bytes.len() != data.len() {
                return Err(io::Error::new(io::ErrorKind::InvalidData, format!("{}: expected {} save bytes, got {}", path.display(), data.len(), bytes.len())));
            }
            data.copy_from_slice(&bytes);
            eprintln!("[save] loaded {} ({} bytes)", path.display(), bytes.len());
        }
        let mut s = Self { path, dirty: imported, last: Instant::now() };
        s.flush(data)?;
        Ok(s)
    }

    pub fn poll(&mut self, data: &[u8]) -> io::Result<()> {
        if self.dirty && self.last.elapsed() >= Duration::from_secs(1) { self.flush(data)?; }
        Ok(())
    }

    pub fn flush(&mut self, data: &[u8]) -> io::Result<()> {
        if !self.dirty { return Ok(()); }
        let parent = self.path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
        fs::create_dir_all(parent)?;
        let temp = self.path.with_extension(format!("sav.{}.tmp", std::process::id()));
        let result = (|| {
            let mut f = fs::File::create(&temp)?;
            f.write_all(data)?;
            f.sync_all()?;
            drop(f);
            replace(&temp, &self.path)
        })();
        if result.is_err() { let _ = fs::remove_file(&temp); }
        result?;
        self.dirty = false;
        self.last = Instant::now();
        eprintln!("[save] wrote {}", self.path.display());
        Ok(())
    }
}

#[cfg(not(windows))]
fn replace(from: &Path, to: &Path) -> io::Result<()> { fs::rename(from, to) }

#[cfg(windows)]
fn replace(from: &Path, to: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "kernel32")]
    unsafe extern "system" { fn MoveFileExW(from: *const u16, to: *const u16, flags: u32) -> i32; }
    let from: Vec<_> = from.as_os_str().encode_wide().chain(Some(0)).collect();
    let to: Vec<_> = to.as_os_str().encode_wide().chain(Some(0)).collect();
    if unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), 1 | 8) } == 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn imports_once_rejects_wrong_size_and_keeps_previous_image() {
        let dir = std::env::temp_dir().join(format!("mkdsrust-import-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("user.sav");
        let import = dir.join("save.sav");
        let _ = fs::remove_file(&path);
        fs::write(&import, [1, 2, 3, 4]).unwrap();
        let mut data = [0xff; 4];
        let mut save = Save::load(path.clone(), Some(&import), &mut data).unwrap();
        assert_eq!(data, [1, 2, 3, 4]);
        assert_eq!(fs::read(&path).unwrap(), data);
        data[0] = 9;
        save.dirty = true;
        save.poll(&data).unwrap();
        assert_eq!(fs::read(&path).unwrap(), [1, 2, 3, 4], "debounced");
        save.flush(&data).unwrap();
        fs::write(&import, [7; 4]).unwrap();
        let mut reloaded = [0; 4];
        Save::load(path.clone(), Some(&import), &mut reloaded).unwrap();
        assert_eq!(reloaded, data, "existing user save takes priority");
        assert!(Save::load(path.clone(), None, &mut [0; 3]).is_err());
        assert_eq!(fs::read(&path).unwrap(), data, "invalid image is never overwritten");
        fs::remove_dir_all(dir).unwrap();
    }
}
