//! Host opt-in policy, shared by both player modes and the acceptance driver.
pub fn configure(backup: &mut recomp::peripherals::Backup, default_id: Option<&str>, proof: bool) -> Result<(), String> {
    if proof || ["MKDS_RECORD", "MKDS_REPLAY", "MKDS_SND_REPLAY", "MKDS_PXI_LOG", "MKDS_SND_LOG"].iter().any(|k| std::env::var_os(k).is_some()) {
        return Ok(());
    }
    let path = match std::env::var_os("MKDS_SAVE") {
        Some(v) if v == "off" => return Ok(()),
        Some(v) if v.is_empty() => return Err("MKDS_SAVE needs off or a file path".into()),
        Some(v) => std::path::PathBuf::from(v),
        None => match default_id {
            None => return Ok(()),
            Some(id) => {
                if id.len() != 40 || !id.bytes().all(|b| b.is_ascii_hexdigit()) { return Err("Invalid ROM SHA-1 for save path".into()); }
                mkds_builder::settings::save_folder().ok_or("Cannot locate user save directory")?.join(format!("{}.sav", id.to_ascii_lowercase()))
            },
        },
    };
    let import = std::env::current_exe().map_err(|e| e.to_string())?.with_file_name("save.sav");
    backup.attach_save(path, Some(&import)).map_err(|e| format!("Cannot load save: {e}"))
}
