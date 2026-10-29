//! Display settings shared by the game window and the builder's menu: `settings.txt` beside `controls.txt` in the
//! user's settings folder. Command-line options of the game override it for one run.
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Renderer {
    /// 3D and screen composition on the graphics card (block 2).
    Gpu,
    /// The CPU renderer (block 1); also the reference the GPU renderer is tested against.
    Software,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layout {
    /// Top screen above the bottom screen, like the console.
    Stacked,
    /// Top screen left, bottom screen right (wide monitors).
    SideBySide,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    pub vsync: bool,
    pub single_screen: bool,
    pub swap_screens: bool,
    pub gap: u32,
    pub window_size: u32,
    pub fps: bool,
    pub fps_position: u32,
    pub frame_limiter: bool,
    pub speed: u32,
    pub volume: u32,
    pub mute: bool,
    pub deadzone: u32,
    pub touch_mouse: bool,
    pub verbose: bool,
    pub renderer: Renderer,
    /// Internal resolution as a multiple of 256x192 (1-8); `None` = the sharpest that fits the window (the default).
    /// 1 is the original look.
    pub scale: Option<u32>,
    /// Linear filtering when the screens are stretched (`false` = sharp pixels).
    pub smooth: bool,
    pub layout: Layout,
    pub fullscreen: bool,
    /// Folder of replacement textures (`<id>.png`).
    pub texture_pack: Option<PathBuf>,
    /// Folder to save every texture the game uses into.
    pub dump_textures: Option<PathBuf>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            vsync: true, single_screen: false, swap_screens: false, gap: 0,
            window_size: 2, fps: false, fps_position: 0, frame_limiter: true,
            speed: 100, volume: 100, mute: false, deadzone: 50, touch_mouse: true, verbose: false,
            renderer: Renderer::Gpu,
            scale: None,
            smooth: false,
            layout: Layout::Stacked,
            fullscreen: false,
            texture_pack: None,
            dump_textures: None,
        }
    }
}

fn yes_no(v: &str) -> Option<bool> {
    match v.to_ascii_lowercase().as_str() {
        "yes" | "on" | "true" => Some(true),
        "no" | "off" | "false" => Some(false),
        _ => None,
    }
}

impl Settings {
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut s = Settings::default();
        for (n, line) in text.lines().enumerate() {
            let (n, line) = (n + 1, line.split('#').next().unwrap_or("").trim());
            if line.is_empty() {
                continue;
            }
            let (key, value) = line.split_once('=').ok_or_else(|| format!("line {n}: expected `Name = value`"))?;
            let (key, value) = (key.trim().to_ascii_lowercase(), value.trim());
            let bad = |what: &str| format!("line {n}: {what}, not `{value}`");
            match key.as_str() {
                "vsync" => s.vsync = yes_no(value).ok_or_else(|| bad("Vsync must be Yes or No"))?,
                "singlescreen" => s.single_screen = yes_no(value).ok_or_else(|| bad("SingleScreen must be Yes or No"))?,
                "swapscreens" => s.swap_screens = yes_no(value).ok_or_else(|| bad("SwapScreens must be Yes or No"))?,
                "fps" => s.fps = yes_no(value).ok_or_else(|| bad("FPS must be Yes or No"))?,
                "framelimiter" => s.frame_limiter = yes_no(value).ok_or_else(|| bad("FrameLimiter must be Yes or No"))?,
                "mute" => s.mute = yes_no(value).ok_or_else(|| bad("Mute must be Yes or No"))?,
                "touchmouse" => s.touch_mouse = yes_no(value).ok_or_else(|| bad("TouchMouse must be Yes or No"))?,
                "verbose" => s.verbose = yes_no(value).ok_or_else(|| bad("Verbose must be Yes or No"))?,
                "gap" | "windowsize" | "fpsposition" | "speed" | "volume" | "deadzone" => {
                    let (min,max) = match key.as_str() { "gap" => (0,192), "windowsize" => (1,4), "fpsposition" => (0,3), "speed" => (25,400), "volume" => (0,100), _ => (5,95) };
                    let n = value.parse::<u32>().ok().filter(|n| (min..=max).contains(n)).ok_or_else(|| bad(&format!("{key} must be {min}-{max}")))?;
                    match key.as_str() { "gap" => s.gap=n, "windowsize" => s.window_size=n, "fpsposition" => s.fps_position=n, "speed" => s.speed=n, "volume" => s.volume=n, _ => s.deadzone=n }
                }
                "renderer" => {
                    s.renderer = match value.to_ascii_lowercase().as_str() {
                        "gpu" => Renderer::Gpu,
                        "software" => Renderer::Software,
                        _ => return Err(bad("Renderer must be GPU or Software")),
                    }
                }
                "scale" => {
                    s.scale = if value.eq_ignore_ascii_case("auto") {
                        None
                    } else {
                        Some(value.parse().ok().filter(|v| (1..=8).contains(v)).ok_or_else(|| bad("Scale must be 1-8 or Auto"))?)
                    }
                }
                "filter" => {
                    s.smooth = match value.to_ascii_lowercase().as_str() {
                        "sharp" => false,
                        "smooth" => true,
                        _ => return Err(bad("Filter must be Sharp or Smooth")),
                    }
                }
                "layout" => {
                    s.layout = match value.to_ascii_lowercase().as_str() {
                        "stacked" => Layout::Stacked,
                        "sidebyside" | "side-by-side" | "side by side" => Layout::SideBySide,
                        _ => return Err(bad("Layout must be Stacked or SideBySide")),
                    }
                }
                "fullscreen" => s.fullscreen = yes_no(value).ok_or_else(|| bad("Fullscreen must be Yes or No"))?,
                "texturepack" => s.texture_pack = (!value.is_empty()).then(|| PathBuf::from(value)),
                "dumptextures" => s.dump_textures = (!value.is_empty()).then(|| PathBuf::from(value)),
                _ => {},
            }
        }
        Ok(s)
    }

    pub fn to_text(&self) -> String {
        let path = |p: &Option<PathBuf>| p.as_ref().map(|p| p.display().to_string()).unwrap_or_default();
        let mut text = format!(
            "# MKDSrust display settings\n\
             #\n\
             # By default the 3D is drawn sharper than on a DS, as sharp as your window allows; everything else looks like\n\
             # the DS: sharp pixels, top screen above the bottom one. Set Scale = 1 for the exact original look.\n\
             # Everything below only changes how the screens look, never how the game plays.\n\
             # Change a value after the `=`, save, and start the game again. Or use mkds.bat -> \"Display settings\".\n\
             # Delete this file to get the defaults back. For one run, `play.bat --scale 4` or\n\
             # `play.bat --renderer software` override this file.\n\
             \n\
             # GPU (default): the graphics card draws the 3D and the screens; smooth 60 frames per second.\n\
             # Software: the CPU draws everything, pixel for pixel like the console (can drop frames on slower PCs).\n\
             Renderer = {}\n\
             \n\
             # 3D sharpness, as a multiple of the DS's 256 x 192 (GPU renderer only).\n\
             #   Auto = the sharpest that fits the window (up to 4), recalculated when you resize it (default).\n\
             #   1    = the original look.\n\
             #   2-8  = a fixed sharpness: 2 suits a 1080p monitor, 3-4 suit 1440p / 4K. Higher numbers need a faster graphics card.\n\
             # The 2D parts (menus, map, HUD) always keep the DS's resolution.\n\
             Scale = {}\n\
             \n\
             # How the screens look when the window is bigger than the DS: Sharp keeps crisp square pixels (default),\n\
             # Smooth blurs them slightly, which some people prefer with Scale = 1.\n\
             Filter = {}\n\
             \n\
             # Stacked: top screen above the bottom screen, like the console (default).\n\
             # SideBySide: top screen on the left, touch screen on the right; better on wide monitors.\n\
             Layout = {}\n\
             \n\
             # Yes: start in fullscreen. F11 switches between window and fullscreen at any time.\n\
             Fullscreen = {}\n\
             \n\
             # Texture packs (GPU renderer): replace the game's textures with your own PNGs, e.g. hand-made or upscaled.\n\
             # 1. Set DumpTextures to a folder and play: every texture the game shows is saved once as <id>.png.\n\
             # 2. Edit or upscale the PNGs you want (whole-number size factors 1-8, same file names).\n\
             # 3. Put them in a folder, set TexturePack to it, and empty DumpTextures again.\n\
             # Dumped textures come from your copy of the game: do not share them.\n\
             TexturePack = {}\n\
             DumpTextures = {}\n",
            if self.renderer == Renderer::Gpu { "GPU" } else { "Software" },
            self.scale.map_or("Auto".to_string(), |s| s.to_string()),
            if self.smooth { "Smooth" } else { "Sharp" },
            if self.layout == Layout::Stacked { "Stacked" } else { "SideBySide" },
            if self.fullscreen { "Yes" } else { "No" },
            path(&self.texture_pack),
            path(&self.dump_textures),
        );
        let yn = |v| if v { "Yes" } else { "No" };
        text += &format!("\nVsync = {}\nSingleScreen = {}\nSwapScreens = {}\nGap = {}\nWindowSize = {}\nFPS = {}\nFPSPosition = {}\nFrameLimiter = {}\nSpeed = {}\nVolume = {}\nMute = {}\nDeadzone = {}\nTouchMouse = {}\nVerbose = {}\n",
            yn(self.vsync), yn(self.single_screen), yn(self.swap_screens), self.gap, self.window_size,
            yn(self.fps), self.fps_position, yn(self.frame_limiter), self.speed, self.volume, yn(self.mute), self.deadzone, yn(self.touch_mouse), yn(self.verbose));
        text
    }

    /// Reads `path`, writing the defaults there first when it does not exist yet.
    pub fn load_or_create(path: &std::path::Path) -> Result<Self, String> {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::parse(&text).map_err(|e| format!("{}: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let s = Self::default();
                s.save(path)?;
                Ok(s)
            }
            Err(e) => Err(format!("Cannot read {}: {e}", path.display())),
        }
    }

    pub fn save(&self, path: &std::path::Path) -> Result<(), String> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("Cannot create {}: {e}", dir.display()))?;
        }
        std::fs::write(path, self.to_text()).map_err(|e| format!("Cannot write {}: {e}", path.display()))
    }
}

/// `settings.txt` in the same folder as `controls.txt`.
pub fn default_path() -> Option<PathBuf> {
    crate::controls::default_path().map(|p| p.with_file_name("settings.txt"))
}

/// Use the same platform directory selection as controls/settings, under the game name.
pub fn save_folder() -> Option<PathBuf> {
    default_path()?.parent()?.parent().map(|p| p.join("MKDSrust").join("saves"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_round_trip_and_values_parse() {
        let d = Settings::default();
        assert_eq!(Settings::parse(&d.to_text()).unwrap(), d);
        let s = Settings::parse("renderer = software\nScale=3\nfilter = SMOOTH\nLayout = SideBySide\nFullscreen = yes\nTexturePack = C:\\packs\\hd\n").unwrap();
        assert_eq!(s.renderer, Renderer::Software);
        assert_eq!(s.scale, Some(3));
        assert!(s.smooth && s.fullscreen);
        assert_eq!(s.layout, Layout::SideBySide);
        assert_eq!(s.texture_pack, Some(PathBuf::from("C:\\packs\\hd")));
        assert_eq!(Settings::parse(&s.to_text()).unwrap(), s);
        assert_eq!(Settings::parse("Scale = auto").unwrap().scale, None);
        assert_eq!(Settings::default().scale, None, "the default is the sharpest that fits the window");
        assert_eq!(Settings::parse("").unwrap(), Settings::default(), "an empty file means the defaults");
    }

    #[test]
    fn overlay_values_round_trip_and_unknown_keys_are_ignored() {
        let text = "Vsync=off\nSingleScreen=yes\nSwapScreens=yes\nGap=32\nWindowSize=3\nFPS=yes\nFPSPosition=3\nFrameLimiter=no\nSpeed=200\nVolume=42\nMute=yes\nDeadzone=25\nTouchMouse=no\nVerbose=yes\nFuturePreference=1";
        let s = Settings::parse(text).unwrap();
        assert!(!s.vsync && s.single_screen && s.swap_screens && s.fps && s.mute && s.verbose);
        assert!(!s.frame_limiter && !s.touch_mouse);
        assert_eq!((s.gap,s.window_size,s.fps_position,s.speed,s.volume,s.deadzone),(32,3,3,200,42,25));
        assert_eq!(Settings::parse(&s.to_text()).unwrap(),s);
        for text in ["Speed=0", "Volume=101", "Deadzone=100", "Gap=193", "WindowSize=0", "FPSPosition=4"] { assert!(Settings::parse(text).is_err(), "{text}"); }
        assert!(!Settings::default().fps);
    }

    #[test]
    fn mistakes_name_the_line() {
        for (text, expect) in [
            ("Scale = 9", "line 1: Scale must be 1-8 or Auto"),
            ("Renderer = gpu\nRenderer = vulkan", "line 2: Renderer must be GPU or Software"),

            ("Fullscreen", "line 1: expected"),
        ] {
            let e = Settings::parse(text).unwrap_err();
            assert!(e.starts_with(expect), "{e}");
        }
    }
}
