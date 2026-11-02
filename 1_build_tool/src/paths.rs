//! Shared portable paths. Game processes only discover existing data; launchers select and create it.
use std::path::{Path, PathBuf};
pub fn game_data(exe: &Path, explicit: Option<PathBuf>) -> Option<PathBuf> {
    explicit.or_else(|| {
        let p = exe.parent()?.parent()?.parent()?.join("data");
        p.is_dir().then_some(p)
    })
}
pub fn data_dir() -> Option<PathBuf> {
    game_data(&std::env::current_exe().ok()?, std::env::var_os("MKDS_DATA_DIR").filter(|s| !s.is_empty()).map(PathBuf::from))
}
pub fn select(exe: &Path, legacy: &Path, disabled: bool) -> (PathBuf, bool) {
    let Some(base) = exe.parent() else { return (legacy.to_owned(), false) };
    let data = base.join("data");
    if disabled || base.join("MKDSRUST_NOT_PORTABLE").exists() || data.join("MKDSRUST_NOT_PORTABLE").exists() {
        return (legacy.to_owned(), false);
    }
    let writable = || -> std::io::Result<()> {
        let probe = tempfile::NamedTempFile::new_in(base)?;
        drop(probe);
        std::fs::create_dir_all(&data)?;
        let probe = tempfile::NamedTempFile::new_in(&data)?;
        drop(probe);
        Ok(())
    };
    if writable().is_ok() { (data, true) } else { (legacy.to_owned(), false) }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test] fn portable_and_opt_out() {
        let t = tempfile::tempdir().unwrap();
        let exe = t.path().join("MKDSrust.exe"); let old = t.path().join("old");
        assert_eq!(select(&exe, &old, false), (t.path().join("data"), true));
        assert_eq!(select(&exe, &old, true), (old.clone(), false));
        std::fs::write(t.path().join("MKDSRUST_NOT_PORTABLE"), "").unwrap();
        assert_eq!(select(&exe, &old, false), (old, false));
    }
    #[test] fn unwritable_or_missing_parent_and_game_discovery() {
        let t = tempfile::tempdir().unwrap(); let old = t.path().join("old");
        assert_eq!(select(&t.path().join("missing/app.exe"), &old, false), (old, false));
        let exe = t.path().join("games/MKDSrust/game.exe");
        assert_eq!(game_data(&exe, None), None);
        std::fs::create_dir(t.path().join("data")).unwrap();
        assert_eq!(game_data(&exe, None), Some(t.path().join("data")));
        assert_eq!(game_data(&exe, Some(t.path().join("custom"))), Some(t.path().join("custom")));
    }
}
