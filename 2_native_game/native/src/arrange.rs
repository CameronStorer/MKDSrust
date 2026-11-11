//! Combined layout (Widescreen mod): the whole game in one widescreen picture, built from both DS screens and
//! rearranged per scene. Pure CPU geometry: which part of which screen goes where in the window ("pieces"), the
//! transitions between arrangements and the mouse-to-touch-screen mapping. The window draws the pieces
//! (`window.rs`), the compositor provides the screens and the menus' live backdrop layer (`gpu/compose.rs`).
//! Presentation only: nothing here changes what the game does.

/// `g_scene_history` (u16 previous, current, next) in main RAM.
const SCENE_HISTORY: usize = 0x0216_53B8 - 0x0200_0000;
/// Pointer to the race manager; its +0x14 is the player's race state (non-zero once the player has finished).
const RACE_MANAGER_PTR: usize = 0x0217_55FC - 0x0200_0000;
/// [`Phase`] bit: the race is over (results on the touch screen).
pub const FINISHED: u32 = 1 << 16;
/// No scene known yet.
pub const UNKNOWN: u32 = 0xFFFF;

/// The current scene id and [`FINISHED`], read from main RAM (no side effects).
pub fn phase(main: &[u8]) -> u32 {
    let r16 = |a: usize| main.get(a..a + 2).map_or(0xFFFF, |b| u16::from_le_bytes([b[0], b[1]]) as u32);
    let r32 = |a: usize| main.get(a..a + 4).map_or(0, |b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
    let scene = r16(SCENE_HISTORY + 2);
    let race = r32(RACE_MANAGER_PTR);
    let finished = scene == 2
        && (0x0200_0000..0x0240_0000 - 0x500).contains(&race)
        && r32(race as usize - 0x0200_0000 + 0x14) != 0;
    scene | if finished { FINISHED } else { 0 }
}

/// Pointer to the race's 2D screen object; its +0x828 is the touch-screen map's view (1 close-up with the
/// positions list, 0 the whole course). A tap on the touch screen switches it.
const RACE_SCREEN_PTR: usize = 0x0217_B330 - 0x0200_0000;
const MAP_VIEW: u32 = 0x828;

/// The touch-screen map shows its close-up view (`Some(false)`), the whole course (`Some(true)`), or no race map.
pub fn whole_course_map(main: &[u8]) -> Option<bool> {
    let r32 = |a: usize| main.get(a..a + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
    if phase(main) != 2 {
        return None; // not racing, or finished (results)
    }
    let p = r32(RACE_SCREEN_PTR)?;
    if !(0x0200_0000..0x0240_0000 - 0x1000).contains(&p) {
        return None;
    }
    match r32(p as usize - 0x0200_0000 + MAP_VIEW as usize)? {
        0 => Some(true),
        1 => Some(false),
        _ => None,
    }
}

/// Map overlay, Track style: keeps the touch-screen map in its whole-course view by tapping it the way the player
/// would (the game's own toggle), only while the style is shown. Off, it never touches anything.
#[derive(Default)]
pub struct WholeCourseTap {
    /// Frames left of the current tap, then frames to wait before checking again.
    press: u32,
    wait: u32,
}

impl WholeCourseTap {
    /// Where to touch this frame, if anywhere. `wanted`: the Track style is on; `player`: the player's own touch.
    pub fn frame(&mut self, main: &[u8], wanted: bool, player: Option<(u8, u8)>) -> Option<(u8, u8)> {
        if self.press > 0 {
            self.press -= 1;
            return Some((184, 96)); // on the map, away from the positions list
        }
        if self.wait > 0 {
            self.wait -= 1;
            return None;
        }
        if wanted && player.is_none() && whole_course_map(main) == Some(false) {
            self.press = 4;
            self.wait = 30;
        }
        None
    }
}

/// What kind of picture a scene shows, which decides its arrangement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scene {
    /// Title screen and main menu (scene 0): logo picture and its buttons.
    Title,
    /// Every "Select ..." screen (scene 1): title bar, preview, buttons.
    Select,
    /// The race (scene 2): the wide 3D screen with the map overlay.
    Race,
    /// After the finish (scene 2, results on the touch screen).
    Results,
    /// The boot splash (scene 9: Nintendo logo, ESRB notice): both screens stacked on black, as on the DS.
    Splash,
    /// First-time setup dialogs (scene 11) and anything else: both screens side by side.
    Other,
}

impl Scene {
    pub fn of(phase: u32) -> Scene {
        match (phase & 0xFFFF, phase & FINISHED != 0) {
            (0, _) => Scene::Title,
            (1, _) => Scene::Select,
            (2, false) => Scene::Race,
            (2, true) => Scene::Results,
            (9, _) => Scene::Splash,
            _ => Scene::Other,
        }
    }
}

/// Where a piece's pixels come from.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Source {
    /// A composed screen (`bottom` = the touch screen). `src` is in DS pixels of that screen; x may reach into the
    /// widescreen columns (-margin .. 256 + margin).
    Screen { bottom: bool },
    /// The screen's background layer alone, wrapping (BG maps repeat), in DS pixels of that screen.
    Backdrop { bottom: bool },
    /// A plain colour (0xRRGGBB).
    Solid(u32),
}

/// How a screen piece treats the 3D layer.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Mask {
    None,
    /// Above this source row only the 3D layer shows (a model that reaches past the piece's cut).
    ThreeDAbove(f32),
    /// The 3D layer is left out where it is in front (another piece shows it).
    Hide3D,
}

/// Identity of a piece across arrangements, so a change of scene can slide it to its new place.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Key {
    Backdrop,
    Shadow,
    Title,
    Top,
    Bottom,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Piece {
    pub key: Key,
    pub source: Source,
    /// Source rectangle: x, y, width, height (DS pixels).
    pub src: [f32; 4],
    /// Horizontal tiling: source x repeats inside [start, end) (title bar ends); `None` = no tiling.
    pub wrap: Option<[f32; 2]>,
    /// Destination rectangle in window pixels: x, y, width, height.
    pub dst: [f32; 4],
    /// Soft edges, window pixels: left, right, top, bottom (0 = hard edge).
    pub fade: [f32; 4],
    pub alpha: f32,
    /// Use the screen's alpha (0 where it shows only its backdrop), so the shared backdrop shows through.
    pub see_through: bool,
    pub mask: Mask,
}

/// Rows of the top screen's title bar on the "Select ..." screens.
pub const TITLE_ROWS: f32 = 38.0;

/// The arrangement of `scene` in a `width` x `height` window. `margin`: widescreen DS columns on each side of the
/// 3D screen. Pieces are drawn in order (back to front).
pub fn arrange(scene: Scene, width: f32, height: f32, margin: u32) -> Vec<Piece> {
    let u = height / 192.0; // window pixels per canvas unit; the canvas is 192 units tall
    let cw = width / u; // canvas width in units
    let soft = |k: f32| 5.0 * k * u; // edge fade: 5 DS pixels
    let rect = |x: f32, y: f32, w: f32, h: f32| [x * u, y * u, w * u, h * u];
    let screen = |key, bottom, src: [f32; 4], x: f32, y: f32, k: f32, fade: [f32; 4], see_through| Piece {
        key,
        source: Source::Screen { bottom },
        src,
        wrap: None,
        dst: rect(x, y, src[2] * k, src[3] * k),
        fade: fade.map(|f| f * soft(k)),
        alpha: 1.0,
        see_through,
        mask: Mask::None,
    };
    // The screen's background picture (its furthest-back layer, live: it scrolls and fades with the game) behind
    // everything, scaled to the window width (only narrow windows wrap it beyond 256 columns), top-aligned so
    // only the bottom of the picture is cut. Returns the piece and (scale, source x at canvas x = 0).
    let backdrop = |bottom| {
        let kb = (cw / 256.0).max(1.0); // cover: the picture spans the width, no repeat seams
        let sw = cw / kb;
        let sx = (256.0 - sw) / 2.0;
        let piece = Piece {
            key: Key::Backdrop,
            source: Source::Backdrop { bottom },
            src: [sx, 0.0, sw, 192.0 / kb],
            wrap: None,
            dst: [0.0, 0.0, width, height],
            fade: [0.0; 4],
            alpha: 1.0,
            see_through: false,
            mask: Mask::None,
        };
        (piece, kb, sx)
    };
    // Both whole screens side by side at the largest common scale, centred.
    let side_by_side = |top_opaque: bool, bottom_opaque: bool, back_bottom: bool| {
        let k = (cw / 512.0).min(1.0);
        let (x, y) = ((cw - 512.0 * k) / 2.0, (192.0 - 192.0 * k) / 2.0);
        vec![
            backdrop(back_bottom).0,
            screen(Key::Top, false, [0.0, 0.0, 256.0, 192.0], x, y, k, [1.0; 4], !top_opaque),
            screen(Key::Bottom, true, [0.0, 0.0, 256.0, 192.0], x + 256.0 * k, y, k, [1.0; 4], !bottom_opaque),
        ]
    };
    match scene {
        Scene::Race => {
            // The wide 3D screen, as large as fits, centred (the map overlay is drawn into it).
            let sw = 256.0 + 2.0 * margin as f32;
            let k = (cw / sw).min(1.0);
            let (x, y) = ((cw - sw * k) / 2.0, (192.0 - 192.0 * k) / 2.0);
            vec![screen(Key::Top, false, [-(margin as f32), 0.0, sw, 192.0], x, y, k, [0.0; 4], false)]
        }
        Scene::Select => {
            // The top screen's background picture across the window with its title bar on it, at the same scale;
            // under it the top screen's preview (left) and the whole touch screen (right).
            let t = TITLE_ROWS;
            let (back, kb, sx) = backdrop(false);
            let title_x = -sx * kb;
            let below = t * kb;
            let k = (cw / 512.0).min((192.0 - below) / 192.0);
            let x = (cw - 512.0 * k) / 2.0;
            let yb = below + (192.0 - below - 192.0 * k) / 2.0; // touch screen, centred under the title
            let yt = (yb + 192.0 * k - (192.0 - t) * k).min(yb + 8.0 * k).max(below);
            let fade_x = |left: bool| if (left && title_x > 0.5) || (!left && title_x + 256.0 * kb < cw - 0.5) { 1.0 } else { 0.0 };
            vec![
                back,
                Piece {
                    fade: [fade_x(true), fade_x(false), 0.0, 0.0].map(|f| f * soft(kb)),
                    mask: Mask::Hide3D,
                    ..screen(Key::Title, false, [0.0, 0.0, 256.0, t], title_x, 0.0, kb, [0.0; 4], true)
                },
                // The preview from the screen's top row: above the title bar's rows only its 3D model shows (a tall
                // character's head reaches over the title), below it everything.
                Piece {
                    mask: Mask::ThreeDAbove(t),
                    ..screen(Key::Top, false, [0.0, 0.0, 256.0, 192.0], x, yt - t * k, k, [1.0, 1.0, 0.0, 1.0], true)
                },
                screen(Key::Bottom, true, [0.0, 0.0, 256.0, 192.0], x + 256.0 * k, yb, k, [1.0, 1.0, 1.0, 0.6], true),
            ]
        }
        Scene::Title => {
            // The logo picture as a clean card with a soft shadow on the left, the menu's buttons on the right running
            // off the window's edge as they run off the DS screen's, both the same size and centred in height.
            let (back, _, _) = backdrop(true);
            let (margin_l, gap) = (10.0, 6.0);
            let k = ((cw - margin_l - gap) / 512.0).min(1.0);
            let free = cw - 512.0 * k - gap; // only on very wide windows: split it around the card
            let (x, y) = (if free > margin_l { free / 2.0 } else { margin_l }, (192.0 - 192.0 * k) / 2.0);
            let card = screen(Key::Top, false, [0.0, 0.0, 256.0, 192.0], x, y, k, [0.0; 4], false);
            let spread = 6.0 * u;
            let shadow = Piece {
                key: Key::Shadow,
                source: Source::Solid(0x0a1a40),
                src: [0.0; 4],
                wrap: None,
                dst: [card.dst[0] - spread + 2.0 * u, card.dst[1] - spread + 3.0 * u, card.dst[2] + 2.0 * spread, card.dst[3] + 2.0 * spread],
                fade: [spread * 1.6; 4],
                alpha: 0.55,
                see_through: false,
                mask: Mask::None,
            };
            let buttons = screen(Key::Bottom, true, [0.0, 0.0, 256.0, 192.0], (cw - 256.0 * k).min(x + 256.0 * k + gap + free / 2.0), y, k, [1.0, 0.0, 1.0, 1.0], true);
            vec![back, shadow, card, buttons]
        }
        Scene::Results => {
            let mut v = side_by_side(true, true, true);
            v[0] = Piece { source: Source::Solid(0x10182c), ..v[0] };
            v
        }
        Scene::Splash => {
            // Its pictures are white on black, so the screens' black edges vanish into a black window; stacked at
            // half height they read like the DS's own splash. (Its backdrop layer is the notice text itself.)
            let k = 0.5;
            let x = (cw - 256.0 * k) / 2.0;
            let black = Piece { key: Key::Backdrop, source: Source::Solid(0), src: [0.0; 4], wrap: None, dst: [0.0, 0.0, width, height], fade: [0.0; 4], alpha: 1.0, see_through: false, mask: Mask::None };
            vec![
                black,
                screen(Key::Top, false, [0.0, 0.0, 256.0, 192.0], x, 0.0, k, [0.0; 4], false),
                screen(Key::Bottom, true, [0.0, 0.0, 256.0, 192.0], x, 96.0, k, [0.0; 4], false),
            ]
        }
        Scene::Other => side_by_side(false, false, true),
    }
}

/// Window pixel -> touch-screen pixel, if it lies on the visible touch-screen piece.
pub fn touch(pieces: &[Piece], px: f32, py: f32) -> Option<(u8, u8)> {
    let p = pieces.iter().rev().find(|p| p.source == Source::Screen { bottom: true } && p.alpha >= 0.5)?;
    let [dx, dy, dw, dh] = p.dst;
    if dw <= 0.0 || dh <= 0.0 || px < dx || py < dy || px >= dx + dw || py >= dy + dh {
        return None;
    }
    let x = p.src[0] + (px - dx) / dw * p.src[2];
    let y = p.src[1] + (py - dy) / dh * p.src[3];
    ((0.0..256.0).contains(&x) && (0.0..192.0).contains(&y)).then(|| (x as u8, y as u8))
}

/// Transition length when the arrangement changes.
pub const TRANSITION: f32 = 0.22;

/// Blends from one arrangement to the next: pieces with the same key slide, the others fade.
pub fn blend(from: &[Piece], to: &[Piece], t: f32) -> Vec<Piece> {
    let t = t.clamp(0.0, 1.0);
    let e = t * t * (3.0 - 2.0 * t); // ease in and out
    let lerp = |a: [f32; 4], b: [f32; 4]| [0, 1, 2, 3].map(|i| a[i] + (b[i] - a[i]) * e);
    let mut out = Vec::new();
    // Pieces only in the old arrangement fade out underneath.
    for f in from.iter().filter(|f| !to.iter().any(|p| p.key == f.key)) {
        out.push(Piece { alpha: f.alpha * (1.0 - e), ..*f });
    }
    for p in to {
        match from.iter().find(|f| f.key == p.key) {
            Some(f) if f.source == p.source && f.wrap == p.wrap => out.push(Piece {
                src: lerp(f.src, p.src),
                dst: lerp(f.dst, p.dst),
                fade: lerp(f.fade, p.fade),
                alpha: f.alpha + (p.alpha - f.alpha) * e,
                ..*p
            }),
            Some(f) => {
                out.push(Piece { alpha: f.alpha * (1.0 - e), ..*f });
                out.push(Piece { alpha: p.alpha * e, ..*p });
            }
            None => out.push(Piece { alpha: p.alpha * e, ..*p }),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scene_phase_reads_ram_without_a_race() {
        let mut ram = vec![0u8; 0x40_0000];
        ram[SCENE_HISTORY + 2..SCENE_HISTORY + 4].copy_from_slice(&1u16.to_le_bytes());
        assert_eq!(Scene::of(phase(&ram)), Scene::Select);
        ram[SCENE_HISTORY + 2] = 2;
        assert_eq!(Scene::of(phase(&ram)), Scene::Race, "no race manager yet");
        ram[RACE_MANAGER_PTR..RACE_MANAGER_PTR + 4].copy_from_slice(&0x0210_0000u32.to_le_bytes());
        ram[0x10_0014] = 1;
        assert_eq!(Scene::of(phase(&ram)), Scene::Results);
        assert_eq!(Scene::of(UNKNOWN), Scene::Other);
        assert_eq!(Scene::of(phase(&[])), Scene::Other);
    }

    #[test]
    fn the_whole_course_tap_presses_once_while_the_close_up_map_shows() {
        let mut ram = vec![0u8; 0x40_0000];
        ram[SCENE_HISTORY + 2] = 2;
        ram[RACE_SCREEN_PTR..RACE_SCREEN_PTR + 4].copy_from_slice(&0x0220_0000u32.to_le_bytes());
        ram[0x20_0000 + MAP_VIEW as usize] = 1;
        assert_eq!(whole_course_map(&ram), Some(false));
        let mut tap = WholeCourseTap::default();
        assert_eq!(tap.frame(&ram, false, None), None, "off: never touches");
        assert_eq!(tap.frame(&ram, true, Some((1, 1))), None, "the player is touching");
        assert_eq!(tap.frame(&ram, true, None), None);
        let presses = (0..34).filter(|_| tap.frame(&ram, true, None).is_some()).count();
        assert_eq!(presses, 4, "one 4-frame tap, then a 30-frame pause");
        assert!(tap.frame(&ram, true, None).is_none() && tap.frame(&ram, true, None).is_some(), "still close-up: tap again");
        ram[0x20_0000 + MAP_VIEW as usize] = 0;
        assert_eq!(whole_course_map(&ram), Some(true));
        for _ in 0..40 {
            tap.frame(&ram, true, None); // the tap in progress ends
        }
        assert!((0..100).all(|_| tap.frame(&ram, true, None).is_none()), "already the whole course");
    }

    #[test]
    fn every_arrangement_fits_and_touch_inverts_the_touch_screen_piece() {
        for scene in [Scene::Title, Scene::Select, Scene::Race, Scene::Results, Scene::Other] {
            for (w, h) in [(1920.0, 1080.0), (1920.0, 1200.0), (2560.0, 1080.0), (1024.0, 768.0)] {
                let pieces = arrange(scene, w, h, 40);
                for p in &pieces {
                    let [x, y, pw, ph] = p.dst;
                    assert!(x >= -0.01 && y >= -0.01 && x + pw <= w + 0.01 && y + ph <= h + 0.01, "{scene:?} {w}x{h} {p:?}");
                }
                let bottom = pieces.iter().find(|p| p.key == Key::Bottom);
                assert_eq!(bottom.is_some(), scene != Scene::Race);
                if let Some(b) = bottom {
                    let [x, y, pw, ph] = b.dst;
                    assert_eq!(touch(&pieces, x + 0.5, y + 0.5), Some((0, 0)));
                    assert_eq!(touch(&pieces, x + pw - 0.5, y + ph - 0.5), Some((255, 191)));
                    assert_eq!(touch(&pieces, x - 1.0, y + 1.0), None);
                }
            }
        }
        // 16:9 select screens: the two panels share the width exactly.
        let s = arrange(Scene::Select, 1920.0, 1080.0, 40);
        let top = s.iter().find(|p| p.key == Key::Top).unwrap();
        let bottom = s.iter().find(|p| p.key == Key::Bottom).unwrap();
        assert!((top.dst[0]).abs() < 0.01 && (bottom.dst[0] + bottom.dst[2] - 1920.0).abs() < 0.01);
    }

    #[test]
    fn transitions_slide_shared_pieces_and_fade_the_rest() {
        let a = arrange(Scene::Race, 1920.0, 1080.0, 40);
        let b = arrange(Scene::Results, 1920.0, 1080.0, 40);
        assert_eq!(blend(&a, &b, 1.0).iter().filter(|p| p.alpha > 0.0).count(), b.len());
        let mid = blend(&a, &b, 0.5);
        let top = mid.iter().find(|p| p.key == Key::Top).unwrap();
        assert!(top.dst[2] < 1920.0 && top.dst[2] > 1920.0 / 2.0);
        assert!(blend(&a, &b, 0.0).iter().filter(|p| p.key != Key::Top).all(|p| p.alpha == 0.0));
    }
}
