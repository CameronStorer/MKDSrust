//! ROM-free visual QA for the F10 overlay. Run, then press F10.
use mkds_native::window::{self, Display, Frame};
fn main() {
    let settings = mkds_builder::settings::Settings::default();
    let display = Display {
        settings,
        controls_path: mkds_builder::controls::default_path(),
        scale: None,
        smooth: false,
        side_by_side: false,
        fullscreen: false,
        texture_pack: None,
        dump_textures: None,
    };
    window::run("MKDSrust menu preview (no ROM) — F10", display, |link| {
        while link.is_open() {
            link.present(Frame::Pixels(vec![
                0x18202a;
                (window::W * window::H) as usize
            ]));
            std::thread::sleep(std::time::Duration::from_millis(17));
        }
    })
    .unwrap();
}
