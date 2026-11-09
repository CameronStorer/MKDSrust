//! Dev check: start a bundle's game like the launcher does, then exit early; the game must keep running.
fn main() {
    let bundle = std::path::PathBuf::from(std::env::args().nth(1).expect("bundle folder"));
    std::thread::spawn(move || {
        let extra: Vec<String> = ["--race", "course=20,character=0,kart=0,racers=8", "--mute", "--frames", "900"].map(String::from).to_vec();
        let _ = mkds_builder::build::launch_with_env(&bundle, &extra, &[("MKDS_VERBOSE", Some("0"))]);
    });
    std::thread::sleep(std::time::Duration::from_secs(5));
    println!("parent exiting while the game runs");
    std::process::exit(0);
}
