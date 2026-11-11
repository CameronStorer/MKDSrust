//! Native window (block 2 stage B2.1): a `winit` window presented with `wgpu` on the main thread. The emulation runs on
//! its own thread and talks to the window only through [`HostLink`]: it reads the input state and hands over finished
//! frames; the window owns the event loop, scaling, fullscreen and the mouse-to-touch-screen mapping.
use crate::gpu::{compose::OverlayStyle, Compositor, Gpu, Renderer3d};
use recomp::machine::RenderSnapshot;
use mkds_builder::controls;
use std::collections::{HashMap, VecDeque};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU32, Ordering},
};
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition, PhysicalSize};
use winit::event::{ElementState, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, KeyCode, PhysicalKey};
use winit::window::{Fullscreen, Window, WindowId};

/// Decode the compile-time embedded RGBA icon; no runtime asset path is needed.
fn brand_icon() -> winit::window::Icon {
    let decoder = png::Decoder::new(std::io::Cursor::new(
        include_bytes!("../../../assets/branding/icon.png").as_slice(),
    ));
    let mut reader = decoder.read_info().expect("valid embedded icon PNG");
    let mut rgba = vec![0; reader.output_buffer_size().expect("icon buffer size")];
    let info = reader.next_frame(&mut rgba).expect("decode embedded icon");
    rgba.truncate(info.buffer_size());
    winit::window::Icon::from_rgba(rgba, info.width, info.height)
        .expect("embedded icon is RGBA")
}

/// Both screens, top first: 256 x 384 pixels, `0x00RRGGBB`.
pub const W: u32 = 256;
pub const H: u32 = 384;

/// A frame for the window.
pub enum Frame {
    /// Finished screens from the software renderer: 256 x 384, `0x00RRGGBB`.
    Pixels(Vec<u32>),
    /// Raw renderer input; the window draws both 2D engines, the 3D layer and the screens on the GPU.
    Gpu(Box<GpuFrame>),
}

pub struct GpuFrame {
    pub snap: RenderSnapshot,
}

#[derive(Default)]
struct InputState {
    /// Held keys as `controls::KEYS` indexes, by the physical key that pressed them (so a release always matches).
    held: HashMap<KeyCode, usize>,
    touch: Option<(u8, u8)>,
    focused: bool,
}

/// What the emulation thread and the window share.
pub struct HostLink {
    input: Mutex<InputState>,
    pub(crate) preferences: Mutex<Option<(mkds_builder::settings::Settings, controls::Controls)>>,
    pub(crate) menu_open: AtomicBool,
    /// Frames not yet shown, oldest first.
    frames: Mutex<VecDeque<Frame>>,
    closed: AtomicBool,
    finished: AtomicBool,
    proxy: Mutex<Option<EventLoopProxy<()>>>,
    gpu_2d_stats: Mutex<(std::time::Duration, u64, usize)>,
    /// The game's scene and race phase ([`crate::arrange::phase`]) for the Combined layout.
    phase: AtomicU32,
}

impl HostLink {
    fn new() -> Arc<HostLink> {
        Arc::new(HostLink {
            input: Mutex::new(InputState::default()),
            preferences: Mutex::new(None),
            menu_open: AtomicBool::new(false),
            frames: Mutex::new(VecDeque::new()),
            closed: AtomicBool::new(false),
            finished: AtomicBool::new(false),
            proxy: Mutex::new(None),
            gpu_2d_stats: Mutex::new(Default::default()),
            phase: AtomicU32::new(crate::arrange::UNKNOWN),
        })
    }

    pub(crate) fn take_2d_stats(&self) -> (std::time::Duration, u64, usize) {
        std::mem::take(&mut *self.gpu_2d_stats.lock().unwrap())
    }

    /// False once the player closed the window.
    pub(crate) fn close(&self) { self.closed.store(true, Ordering::Release); self.wake(); }

    pub fn is_open(&self) -> bool {
        !self.closed.load(Ordering::Acquire)
    }

    pub fn focused(&self) -> bool {
        !self.menu_open.load(Ordering::Acquire) && self.input.lock().unwrap().focused
    }

    /// Whether any of `keys` (`controls::KEYS` indexes) is held in the focused window.
    pub fn any_down(&self, keys: &[usize]) -> bool {
        let s = self.input.lock().unwrap();
        !self.menu_open.load(Ordering::Acquire) && s.focused && s.held.values().any(|k| keys.contains(k))
    }

    /// The stylus position on the bottom screen while the left mouse button is held there.
    pub fn touch(&self) -> Option<(u8, u8)> {
        let s = self.input.lock().unwrap();
        s.touch.filter(|_| s.focused && !self.menu_open.load(Ordering::Acquire))
    }

    /// Queues `frame`; each screen refresh shows the next one. Frames arrive unevenly (the emulation runs in
    /// bursts), so a short queue keeps two frames that land in the same refresh from replacing each other, which
    /// showed as a dropped frame followed by a repeated one (visible stutter in the course preview videos).
    pub fn present(&self, frame: Frame) {
        let mut q = self.frames.lock().unwrap();
        q.push_back(frame);
        while q.len() > MAX_QUEUED {
            q.pop_front(); // the display fell behind: catch up rather than lag
        }
        drop(q);
        self.wake();
    }

    /// The game's current scene ([`crate::arrange::phase`]), published with each frame.
    pub fn set_phase(&self, phase: u32) {
        self.phase.store(phase, Ordering::Release);
    }

    /// The emulation ended (normally or with an error): close the window.
    pub fn finish(&self) {
        self.finished.store(true, Ordering::Release);
        self.wake();
    }

    #[cfg(test)]
    pub(crate) fn for_tests() -> Arc<HostLink> {
        HostLink::new()
    }

    /// Test input: these `controls::KEYS` indexes held (on arbitrary physical keys), the stylus, and focus.
    #[cfg(test)]
    pub(crate) fn set_for_tests(&self, keys: &[usize], touch: Option<(u8, u8)>, focused: bool) {
        const CODES: [KeyCode; 4] = [KeyCode::F21, KeyCode::F22, KeyCode::F23, KeyCode::F24];
        let mut s = self.input.lock().unwrap();
        s.held = CODES.iter().copied().zip(keys.iter().copied()).collect();
        s.touch = touch;
        s.focused = focused;
    }

    fn wake(&self) {
        if let Some(p) = self.proxy.lock().unwrap().as_ref() {
            let _ = p.send_event(());
        }
    }
}

/// Frames the window may hold back: absorbs uneven arrival without adding noticeable lag (2 refreshes).
const MAX_QUEUED: usize = 2;

/// Runs the window on this (main) thread and `emulation` on a second thread until the emulation returns.
/// How the window shows the screens (from `settings.txt` and the command line).
#[derive(Clone, Debug, PartialEq)]
pub struct Display {
    pub settings: mkds_builder::settings::Settings,
    pub controls_path: Option<std::path::PathBuf>,
    /// GPU resolution as a multiple of 256x192 (1 = the original look); `None` (the default) = the sharpest that fits
    /// the window (up to 4x).
    pub scale: Option<u32>,
    /// Linear filtering when stretching (`false` = sharp pixels).
    pub smooth: bool,
    pub side_by_side: bool,
    pub fullscreen: bool,
    /// Replacement textures (`<id>.png`) for the GPU renderer.
    pub texture_pack: Option<std::path::PathBuf>,
    /// Save every texture the game uses into this folder.
    pub dump_textures: Option<std::path::PathBuf>,
    /// The Widescreen mod is compiled into this build: `settings.widescreen` applies (GPU renderer).
    pub widescreen_mod: bool,
}

impl Display {
    /// DS pixels added on each side of the screens: the Widescreen mod's setting, with the GPU renderer only.
    pub fn margin(&self, gpu: bool) -> u32 {
        if self.widescreen_mod && gpu && self.settings.renderer == mkds_builder::settings::Renderer::Gpu {
            match self.settings.widescreen.margin() {
                // Combined is a widescreen picture: 16:9 unless another shape is chosen.
                0 if self.combined(gpu) => mkds_builder::settings::Widescreen::W16x9.margin(),
                m => m,
            }
        } else {
            0
        }
    }

    /// The Combined layout is on: Widescreen mod, GPU renderer on an adapter that runs it (`gpu`).
    pub fn combined(&self, gpu: bool) -> bool {
        self.widescreen_mod && gpu && self.settings.renderer == mkds_builder::settings::Renderer::Gpu
            && self.settings.layout == mkds_builder::settings::Layout::Combined
    }
}

/// Highest scale `Auto` picks: beyond 4x the GPU cost grows faster than the screens get sharper.
const AUTO_MAX: u32 = 4;

pub fn run<T: Send>(title: &str, display: Display, emulation: impl FnOnce(Arc<HostLink>) -> T + Send) -> Result<T, String> {
    let event_loop = EventLoop::<()>::with_user_event().build().map_err(|e| format!("Cannot start the window system: {e}"))?;
    let link = HostLink::new();
    let controls = display.controls_path.as_ref().map(|p| controls::Controls::load_or_create(p)).transpose()?.unwrap_or_default();
    *link.preferences.lock().unwrap() = Some((display.settings.clone(), controls));
    *link.proxy.lock().unwrap() = Some(event_loop.create_proxy());
    std::thread::scope(|scope| {
        let worker_link = Arc::clone(&link);
        let worker = std::thread::Builder::new()
            .name("emulation".into())
            .spawn_scoped(scope, move || {
                // Close the window however the emulation ends, panics included.
                struct Done(Arc<HostLink>);
                impl Drop for Done {
                    fn drop(&mut self) {
                        self.0.finish();
                    }
                }
                let done = Done(worker_link);
                emulation(Arc::clone(&done.0))
            })
            .map_err(|e| format!("Cannot start the emulation thread: {e}"))?;
        let mut app = App { link: Arc::clone(&link), title: title.to_owned(), display, window: None, gfx: None, error: None, cursor: None, mouse_down: false };
        // A panic in the window must still stop the game thread (the scope waits for it) before it is reported.
        let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| event_loop.run_app(&mut app)));
        link.closed.store(true, Ordering::Release);
        let ran = match ran {
            Ok(r) => r,
            Err(panic) => {
                let _ = worker.join();
                std::panic::resume_unwind(panic);
            }
        };
        // Diagnostics: `MKDS_SCREENSHOT=file.ppm` saves the last GPU screens (game graphics: keep it local).
        if let (Some(path), Some(g)) = (std::env::var_os("MKDS_SCREENSHOT"), &app.gfx) {
            if let Some((_, compositor, _)) = &g.gpu_path {
                let out = compositor.output();
                let (w, h) = (out.width(), out.height());
                let mut ppm = format!("P6
{w} {h}
255
").into_bytes();
                for p in g.gpu.read_rgba(out) {
                    ppm.extend_from_slice(&[(p >> 24) as u8, (p >> 16) as u8, (p >> 8) as u8]);
                }
                let _ = std::fs::write(path, ppm);
            }
        }
        link.closed.store(true, Ordering::Release); // the window is gone, whatever happened
        let result = worker.join().map_err(|_| "The emulation thread panicked".to_string());
        ran.map_err(|e| format!("Window system error: {e}"))?;
        if let Some(e) = app.error {
            return Err(e);
        }
        result
    })
}

struct App {
    link: Arc<HostLink>,
    title: String,
    display: Display,
    window: Option<Arc<Window>>,
    gfx: Option<Presenter>,
    error: Option<String>,
    cursor: Option<PhysicalPosition<f64>>,
    mouse_down: bool,
}

impl App {
    fn update_touch(&self) {
        let enabled = !self.link.menu_open.load(Ordering::Acquire) && self.link.preferences.lock().unwrap().as_ref().is_none_or(|(s,_)| s.touch_mouse);
        let touch = match (self.mouse_down && enabled, self.cursor, &self.gfx) {
            (true, Some(c), Some(g)) if g.combined_active() => crate::arrange::touch(&g.shown, c.x as f32, c.y as f32)
                .or_else(|| overlay_touch(&g.display, &g.layout, c.x, c.y)),
            (true, Some(c), Some(g)) => {
                let l = g.classic_layout();
                l.touch(c.x, c.y).or_else(|| overlay_touch(&g.display, &l, c.x, c.y))
            }
            _ => None,
        };
        self.link.input.lock().unwrap().touch = touch;
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let combined = self.display.settings.layout == mkds_builder::settings::Layout::Combined && self.display.widescreen_mod;
        let (w, h) = if combined { ((256 + 2 * self.display.margin(true)) * 3 / 2, 288) } else if self.display.side_by_side { (W * 2, H / 2) } else { (W, H) };
        let factor = self.display.settings.window_size;
        let attrs = Window::default_attributes()
            .with_title(self.title.clone())
            .with_window_icon(Some(brand_icon()))
            .with_inner_size(LogicalSize::new(w * factor, h * factor))
            .with_min_inner_size(LogicalSize::new(w / 2, h / 2))
            .with_fullscreen(self.display.fullscreen.then_some(Fullscreen::Borderless(None)));
        // The taskbar shows the large icon, which Windows does not take from the small title-bar one.
        #[cfg(windows)]
        let attrs = {
            use winit::platform::windows::WindowAttributesExtWindows;
            attrs.with_taskbar_icon(Some(brand_icon()))
        };
        let made = event_loop.create_window(attrs).map_err(|e| format!("Cannot open game window: {e}")).and_then(|w| {
            let w = Arc::new(w);
            Presenter::new(Arc::clone(&w), self.display.clone()).map(|p| (w, p))
        });
        match made {
            Ok((w, p)) => {
                self.link.input.lock().unwrap().focused = w.has_focus();
                self.window = Some(w);
                self.gfx = Some(p);
            }
            Err(e) => {
                self.error = Some(e);
                event_loop.exit();
            }
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, _: ()) {
        if self.link.finished.load(Ordering::Acquire) {
            event_loop.exit();
        } else if let Some(w) = &self.window {
            w.request_redraw();
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        if let (Some(g), Some(w)) = (&mut self.gfx, &self.window) {
            g.menu.event(w, &event);
            if let WindowEvent::KeyboardInput { event, .. } = &event {
                if event.state == ElementState::Pressed && !event.repeat {
                    if event.physical_key == PhysicalKey::Code(KeyCode::F10) {
                        g.menu.open = !g.menu.open;
                        g.menu.cancel_capture();
                        self.mouse_down = false;
                        let mut input = self.link.input.lock().unwrap();
                        input.held.clear(); input.touch = None;
                        w.request_redraw();
                        self.link.menu_open.store(g.menu.open, Ordering::Release);
                        return;
                    } else if g.menu.open {
                        g.menu.key(event);
                        self.link.menu_open.store(g.menu.open, Ordering::Release);
                        w.request_redraw();
                        return;
                    }
                }
            }
            self.link.menu_open.store(g.menu.open, Ordering::Release);
            if g.menu.open {
                match &event {
                    WindowEvent::KeyboardInput { .. } | WindowEvent::CursorMoved { .. } | WindowEvent::CursorLeft { .. } | WindowEvent::MouseInput { .. } | WindowEvent::MouseWheel { .. } => { w.request_redraw(); return; }
                    _ => {}
                }
            }
        }
        match event {
            WindowEvent::CloseRequested => {
                self.link.closed.store(true, Ordering::Release);
            }
            WindowEvent::Focused(focused) => {
                let mut s = self.link.input.lock().unwrap();
                s.focused = focused;
                if !focused {
                    s.held.clear(); // an inactive window releases every key (keys released elsewhere never arrive)
                }
            }
            WindowEvent::KeyboardInput { event, .. } => {
                let PhysicalKey::Code(code) = event.physical_key else { return };
                if event.state == ElementState::Pressed && !event.repeat && code == KeyCode::F9 && self.display.widescreen_mod {
                    if let Some(g) = &mut self.gfx {
                        // Off -> Picture -> Track -> Off; Combined always shows the map in races: Picture <-> Track
                        let combined = g.combined_active();
                        let s = &mut g.menu.settings;
                        (s.map_overlay, s.map_overlay_track) = match (s.map_overlay || combined, s.map_overlay_track) {
                            (false, _) => (true, false),
                            (true, false) => (true, true),
                            (true, true) => (combined, false),
                        };
                    }
                    if let Some(w) = &self.window { w.request_redraw(); }
                    return;
                }
                if event.state == ElementState::Pressed && !event.repeat && code == KeyCode::F11 {
                    if let Some(w) = &self.window {
                        if let Some(g) = &mut self.gfx { g.menu.settings.fullscreen = w.fullscreen().is_none(); }
                        w.request_redraw();
                    }
                }
                let mut s = self.link.input.lock().unwrap();
                match event.state {
                    ElementState::Pressed => {
                        if let Some(k) = key_index(code, &event.logical_key) {
                            s.held.insert(code, k);
                        }
                    }
                    ElementState::Released => {
                        s.held.remove(&code);
                    }
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor = Some(position);
                if self.mouse_down {
                    self.update_touch();
                }
            }
            WindowEvent::CursorLeft { .. } => {
                self.cursor = None;
                self.update_touch();
            }
            WindowEvent::MouseInput { state, button: MouseButton::Left, .. } => {
                self.mouse_down = state == ElementState::Pressed;
                self.update_touch();
            }
            WindowEvent::Resized(size) => {
                if let Some(g) = &mut self.gfx {
                    g.resize(size);
                }
            }
            WindowEvent::RedrawRequested => {
                let (frame, more) = {
                    let mut q = self.link.frames.lock().unwrap();
                    let f = q.pop_front();
                    (f, !q.is_empty())
                };
                if let Some(g) = &mut self.gfx {
                    if let Err(e) = g.draw(frame, &self.link) {
                        self.error = Some(e);
                        self.link.closed.store(true, Ordering::Release);
                        event_loop.exit();
                        return;
                    }
                }
                if more {
                    if let Some(w) = &self.window {
                        w.request_redraw(); // the next queued frame at the next refresh
                    }
                }
            }
            _ => {}
        }
    }
}

/// The `controls.txt` key for a key event: letters by what the key types (so `Z` means the key labelled Z on any
/// layout), everything else by its position.
pub fn key_index(code: KeyCode, logical: &Key) -> Option<usize> {
    if let Key::Character(s) = logical {
        let mut chars = s.chars();
        if let (Some(c), None) = (chars.next(), chars.next()) {
            if c.is_ascii_alphabetic() && !physical_is_numpad(code) {
                return controls::key_index(&c.to_ascii_uppercase().to_string());
            }
        }
    }
    controls::key_index(physical_name(code)?)
}

fn physical_is_numpad(code: KeyCode) -> bool {
    physical_name(code).is_some_and(|n| n.starts_with("NumPad"))
}

fn physical_name(code: KeyCode) -> Option<&'static str> {
    use KeyCode::*;
    Some(match code {
        KeyA => "A", KeyB => "B", KeyC => "C", KeyD => "D", KeyE => "E", KeyF => "F", KeyG => "G", KeyH => "H",
        KeyI => "I", KeyJ => "J", KeyK => "K", KeyL => "L", KeyM => "M", KeyN => "N", KeyO => "O", KeyP => "P",
        KeyQ => "Q", KeyR => "R", KeyS => "S", KeyT => "T", KeyU => "U", KeyV => "V", KeyW => "W", KeyX => "X",
        KeyY => "Y", KeyZ => "Z",
        Digit0 => "Key0", Digit1 => "Key1", Digit2 => "Key2", Digit3 => "Key3", Digit4 => "Key4",
        Digit5 => "Key5", Digit6 => "Key6", Digit7 => "Key7", Digit8 => "Key8", Digit9 => "Key9",
        F1 => "F1", F2 => "F2", F3 => "F3", F4 => "F4", F5 => "F5", F6 => "F6", F7 => "F7", F8 => "F8",
        F9 => "F9", F10 => "F10", F11 => "F11", F12 => "F12", F13 => "F13", F14 => "F14", F15 => "F15",
        ArrowDown => "Down", ArrowLeft => "Left", ArrowRight => "Right", ArrowUp => "Up",
        Quote => "Apostrophe", Backquote => "Backquote", Backslash => "Backslash", Comma => "Comma", Equal => "Equal",
        BracketLeft => "LeftBracket", Minus => "Minus", Period => "Period", BracketRight => "RightBracket",
        Semicolon => "Semicolon", Slash => "Slash",
        Backspace => "Backspace", Delete => "Delete", End => "End", Enter => "Enter", Escape => "Escape",
        Home => "Home", Insert => "Insert", ContextMenu => "Menu", PageDown => "PageDown", PageUp => "PageUp",
        Pause => "Pause", Space => "Space", Tab => "Tab", NumLock => "NumLock", CapsLock => "CapsLock",
        ScrollLock => "ScrollLock",
        ShiftLeft => "LeftShift", ShiftRight => "RightShift", ControlLeft => "LeftCtrl", ControlRight => "RightCtrl",
        Numpad0 => "NumPad0", Numpad1 => "NumPad1", Numpad2 => "NumPad2", Numpad3 => "NumPad3", Numpad4 => "NumPad4",
        Numpad5 => "NumPad5", Numpad6 => "NumPad6", Numpad7 => "NumPad7", Numpad8 => "NumPad8", Numpad9 => "NumPad9",
        NumpadDecimal => "NumPadDot", NumpadDivide => "NumPadSlash", NumpadMultiply => "NumPadAsterisk",
        NumpadSubtract => "NumPadMinus", NumpadAdd => "NumPadPlus", NumpadEnter => "NumPadEnter",
        AltLeft => "LeftAlt", AltRight => "RightAlt", SuperLeft => "LeftSuper", SuperRight => "RightSuper",
        _ => return None,
    })
}

/// The map overlay's rectangle (DS pixels of the 3D screen, bottom-right with a small inset), opacity 0-255 and
/// style, when the Widescreen mod's MapOverlay is on.
/// `force`: the Combined layout's race, which always shows it.
pub fn map_overlay(display: &Display, margin: u32, force: bool) -> Option<([u32; 4], u32, OverlayStyle)> {
    let s = &display.settings;
    if !display.widescreen_mod || !(s.map_overlay || force) {
        return None;
    }
    let style = if s.map_overlay_track { OverlayStyle::Track } else { OverlayStyle::Picture };
    let h = 192 * s.map_overlay_size.clamp(25, 75) / 100;
    let w = h * style.source_columns().1 / 192;
    let inset = 4;
    let width = 256 + 2 * margin;
    Some(([width - w - inset, 192 - h - inset, w, h], s.map_overlay_opacity.clamp(30, 100) * 255 / 100, style))
}

/// Window pixel inside the map overlay -> the touch-screen pixel it shows (clicking the overlay taps the map).
fn overlay_touch(display: &Display, layout: &Layout, px: f64, py: f64) -> Option<(u8, u8)> {
    let (r, _, style) = map_overlay(display, layout.margin, layout.force_overlay)?;
    let t = layout.top;
    let sw = 256.0 + 2.0 * layout.margin as f64;
    let (x, y) = ((px - t.x) / t.w * sw, (py - t.y) / t.h * 192.0);
    let (rx, ry, rw, rh) = (r[0] as f64, r[1] as f64, r[2] as f64, r[3] as f64);
    if t.w <= 0.0 || x < rx || x >= rx + rw || y < ry || y >= ry + rh {
        return None;
    }
    let (x0, cols) = style.source_columns();
    Some(((x0 as f64 + (x - rx) / rw * cols as f64).min(255.0) as u8, ((y - ry) / rh * 192.0).min(191.0) as u8))
}

/// A screen's rectangle in window pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

/// Where the two screens sit in the window: the largest arrangement that fits (stacked 256x384 or side by side
/// 512x192), centred, keeping the DS's pixel shape.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Layout {
    pub top: Rect,
    pub bottom: Rect,
    pub side_by_side: bool,
    /// Widescreen DS pixels on each side of both screens.
    pub margin: u32,
    /// Combined layout in a race: the map overlay shows whatever the MapOverlay setting says.
    pub force_overlay: bool,
}

impl Layout {
    pub fn fit(width: u32, height: u32, side_by_side: bool) -> Layout {
        let (cw, ch) = if side_by_side { (512.0, 192.0) } else { (256.0, 384.0) };
        let s = (width as f64 / cw).min(height as f64 / ch).max(0.0);
        let (x, y) = ((width as f64 - cw * s) / 2.0, (height as f64 - ch * s) / 2.0);
        let (w, h) = (256.0 * s, 192.0 * s);
        let top = Rect { x, y, w, h };
        let bottom = if side_by_side { Rect { x: x + w, y, w, h } } else { Rect { x, y: y + h, w, h } };
        Layout { top, bottom, side_by_side, margin: 0, force_overlay: false }
    }

    /// `margin`: widescreen DS pixels on each side of both screens (0 = the DS's 256 columns).
    fn configured(width: u32, height: u32, settings: &mkds_builder::settings::Settings, margin: u32) -> Layout {
        let side = settings.layout == mkds_builder::settings::Layout::SideBySide;
        let gap = settings.gap as f64;
        let sw = 256.0 + 2.0 * margin as f64;
        let (cw,ch) = if settings.single_screen { (sw,192.0) } else if side { (2.0*sw+gap,192.0) } else { (sw,384.0+gap) };
        let scale = (width as f64 / cw).min(height as f64 / ch);
        let first = Rect { x: (width as f64-cw*scale)/2.0, y: (height as f64-ch*scale)/2.0, w: sw*scale, h: 192.0*scale };
        let second = if settings.single_screen { Rect { w: 0.0, h: 0.0, ..first } } else if side { Rect { x: first.x+(sw+gap)*scale, ..first } } else { Rect { y: first.y+(192.0+gap)*scale, ..first } };
        let (top,bottom) = if settings.swap_screens { (second,first) } else { (first,second) };
        Layout { top,bottom,side_by_side: side, margin, force_overlay: false }
    }

    /// Combined layout: `top` is where the race's wide 3D screen goes (the arrangement draws the rest).
    fn combined(width: u32, height: u32, margin: u32, race: bool) -> Layout {
        let pieces = crate::arrange::arrange(crate::arrange::Scene::Race, width as f32, height as f32, margin);
        let [x, y, w, h] = pieces[0].dst.map(|v| v as f64);
        Layout { top: Rect { x, y, w, h }, bottom: Rect { x, y, w: 0.0, h: 0.0 }, side_by_side: false, margin, force_overlay: race }
    }

    /// Window pixel -> bottom-screen pixel, if it lies on the bottom screen (outside a widescreen side bar).
    pub fn touch(&self, px: f64, py: f64) -> Option<(u8, u8)> {
        let r = self.bottom;
        if r.w <= 0.0 {
            return None;
        }
        let x = (px - r.x) / r.w * (256.0 + 2.0 * self.margin as f64) - self.margin as f64;
        let y = (py - r.y) / r.h * 192.0;
        ((0.0..256.0).contains(&x) && (0.0..192.0).contains(&y)).then(|| (x as u8, y as u8))
    }

    /// The scale `Auto` uses: enough GPU pixels for every window pixel of a screen, 1 to [`AUTO_MAX`].
    pub fn auto_scale(&self) -> u32 {
        ((self.top.h.max(self.bottom.h) / 192.0).ceil() as u32).clamp(1, AUTO_MAX)
    }
}

const PRESENT_WGSL: &str = r#"
@group(0) @binding(0) var screens: texture_2d<f32>;
@group(0) @binding(1) var nearest: sampler;
struct Out { @builtin(position) pos: vec4<f32>, @location(0) uv: vec2<f32> };
// Instance 0: the whole texture (stacked screens); 1: its top half; 2: its bottom half (side by side).
@vertex fn vs(@builtin(vertex_index) i: u32, @builtin(instance_index) part: u32) -> Out {
    let p = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u));
    var o: Out;
    o.pos = vec4<f32>(p * 2.0 - 1.0, 0.0, 1.0);
    let v = 1.0 - p.y;
    o.uv = vec2<f32>(p.x, select(select(0.5 + v * 0.5, v * 0.5, part == 1u), v, part == 0u));
    return o;
}
@fragment fn fs(o: Out) -> @location(0) vec4<f32> {
    return textureSample(screens, nearest, o.uv);
}
"#;

/// Uploads the frame and draws it into the window, scaled to fit.
struct Presenter {
    window: Arc<Window>,
    gpu: Gpu,
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    screens: wgpu::Texture,
    bind: wgpu::BindGroup,
    bind_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    pipeline: wgpu::RenderPipeline,
    layout: Layout,
    rgba: Vec<u8>,
    /// The GPU renderer, created with the first GPU frame, and the bind group showing its output.
    gpu_path: Option<(Renderer3d, Compositor, wgpu::BindGroup)>,
    scale: u32,
    display: Display,
    showing_gpu: bool,
    /// The adapter can run the GPU renderer (compute shaders, writable RGBA8 storage textures); otherwise GPU frames
    /// are finished on the CPU.
    compute_ok: bool,
    menu: crate::menu::Menu,
    /// Combined layout: the pass that draws the arrangement, the pieces shown last (touch maps through them), the
    /// arrangement a transition started from, the scene arranged for and when it changed.
    pieces: Option<crate::gpu::pieces::PiecesPass>,
    pub(crate) shown: Vec<crate::arrange::Piece>,
    from: Vec<crate::arrange::Piece>,
    scene: Option<crate::arrange::Scene>,
    since: std::time::Instant,
    a_top: bool,
}

impl Presenter {
    fn new(window: Arc<Window>, display: Display) -> Result<Presenter, String> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
        let surface = instance.create_surface(Arc::clone(&window)).map_err(|e| format!("Cannot draw into the window: {e}"))?;
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: Some(&surface),
            ..Default::default()
        }))
        .map_err(|e| format!("No graphics adapter for the window: {e}"))?;
        let gpu = Gpu::from_adapter(adapter)?;
        let size = window.inner_size();
        let mut config = surface
            .get_default_config(&gpu.adapter, size.width.max(1), size.height.max(1))
            .ok_or("The graphics adapter cannot draw into this window")?;
        config.present_mode = if display.settings.vsync { wgpu::PresentMode::AutoVsync } else { wgpu::PresentMode::AutoNoVsync };
        surface.configure(&gpu.device, &config);
        let screens = gpu.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("screens"),
            size: wgpu::Extent3d { width: W, height: H, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            // Same encoding as the window, so the DS colours reach the screen unchanged.
            format: if config.format.is_srgb() { wgpu::TextureFormat::Rgba8UnormSrgb } else { wgpu::TextureFormat::Rgba8Unorm },
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let sampler = gpu.device.create_sampler(&wgpu::SamplerDescriptor {
            mag_filter: if display.smooth { wgpu::FilterMode::Linear } else { wgpu::FilterMode::Nearest },
            min_filter: if display.smooth { wgpu::FilterMode::Linear } else { wgpu::FilterMode::Nearest },
            ..Default::default()
        });
        let bind_layout = gpu.device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("present"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let bind = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("present"),
            layout: &bind_layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&screens.create_view(&Default::default())) },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&sampler) },
            ],
        });
        let shader = gpu.device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("present"),
            source: wgpu::ShaderSource::Wgsl(PRESENT_WGSL.into()),
        });
        let pipeline_layout = gpu.device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("present"),
            bind_group_layouts: &[Some(&bind_layout)],
            immediate_size: 0,
        });
        let pipeline = gpu.device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("present"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState { module: &shader, entry_point: Some("vs"), compilation_options: Default::default(), buffers: &[] },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState { format: config.format, blend: None, write_mask: wgpu::ColorWrites::ALL })],
            }),
            multiview_mask: None,
            cache: None,
        });
        let downlevel = gpu.adapter.get_downlevel_capabilities();
        let storage = gpu.adapter.get_texture_format_features(wgpu::TextureFormat::Rgba8Unorm).allowed_usages;
        let compute_ok = downlevel.flags.contains(wgpu::DownlevelFlags::COMPUTE_SHADERS)
            && storage.contains(wgpu::TextureUsages::STORAGE_BINDING);
        if !compute_ok {
            eprintln!("This graphics adapter cannot run the GPU renderer; the CPU finishes each frame (as --renderer software).");
        }
        let layout = Presenter::layout_for(config.width, config.height, &display, compute_ok, false);
        let scale = display.scale.unwrap_or_else(|| layout.auto_scale());
        eprintln!("Display: {} renderer at {scale}x, {}", gpu.adapter.get_info().name, if display.scale.is_none() { "Auto" } else { "fixed" });
        let menu = crate::menu::Menu::new(&window, &gpu, config.format, &display);
        Ok(Presenter {
            window, gpu, surface, config, screens, bind, bind_layout, sampler, pipeline, layout,
            rgba: vec![0; (W * H * 4) as usize], gpu_path: None, scale, display, showing_gpu: false, compute_ok, menu,
            pieces: None, shown: Vec::new(), from: Vec::new(), scene: None, since: std::time::Instant::now(), a_top: true,
        })
    }

    fn layout_for(width: u32, height: u32, display: &Display, compute_ok: bool, race: bool) -> Layout {
        let margin = display.margin(compute_ok);
        if display.combined(compute_ok) {
            Layout::combined(width, height, margin, race)
        } else {
            Layout::configured(width, height, &display.settings, margin)
        }
    }

    /// The layout for frames the Combined layout does not draw (software frames keep the classic layouts).
    fn classic_layout(&self) -> Layout {
        if self.display.combined(self.compute_ok) {
            Layout::configured(self.config.width, self.config.height, &self.display.settings, 0)
        } else {
            self.layout
        }
    }

    /// The Combined layout is drawing this frame (GPU frames; software frames keep the classic layouts).
    pub(crate) fn combined_active(&self) -> bool {
        self.display.combined(self.compute_ok) && self.showing_gpu
    }

    fn apply_settings(&mut self) {
        let settings = self.menu.settings.clone();
        let old = self.display.settings.clone();
        if settings == old { return; }
        self.display.scale = settings.scale;
        self.display.smooth = settings.smooth;
        self.display.side_by_side = settings.layout == mkds_builder::settings::Layout::SideBySide;
        if settings.fullscreen != old.fullscreen {
            self.window.set_fullscreen(settings.fullscreen.then_some(Fullscreen::Borderless(None)));
        }
        if settings.window_size != old.window_size {
            let (w,h) = if self.display.side_by_side { (512,192) } else { (256,384) };
            let _ = self.window.request_inner_size(LogicalSize::new(w * settings.window_size, h * settings.window_size));
        }
        self.config.present_mode = if settings.vsync { wgpu::PresentMode::AutoVsync } else { wgpu::PresentMode::AutoNoVsync };
        if settings.smooth != old.smooth {
            self.sampler = self.gpu.device.create_sampler(&wgpu::SamplerDescriptor {
                mag_filter: if settings.smooth { wgpu::FilterMode::Linear } else { wgpu::FilterMode::Nearest },
                min_filter: if settings.smooth { wgpu::FilterMode::Linear } else { wgpu::FilterMode::Nearest },
                ..Default::default()
            });
            self.bind = self.bind_texture(&self.screens, false);
            self.gpu_path = None;
        }
        self.display.settings = settings;
        self.resize(self.window.inner_size());
        let scale = self.display.scale.unwrap_or_else(|| self.layout.auto_scale());
        if scale != self.scale { self.scale = scale; self.gpu_path = None; }
    }

    fn resize(&mut self, size: PhysicalSize<u32>) {
        if size.width == 0 || size.height == 0 {
            return; // minimised
        }
        self.config.width = size.width;
        self.config.height = size.height;
        self.surface.configure(&self.gpu.device, &self.config);
        self.layout = Presenter::layout_for(size.width, size.height, &self.display, self.compute_ok, self.scene == Some(crate::arrange::Scene::Race));
        if self.gpu_path.as_ref().is_some_and(|(r, _, _)| r.margin() != self.layout.margin) {
            self.gpu_path = None; // rebuilt at the new width with the next frame
        }
        if self.display.scale.is_none() && self.layout.auto_scale() != self.scale {
            self.scale = self.layout.auto_scale();
            self.gpu_path = None; // rebuilt at the new scale with the next frame
        }
    }

    fn bind_texture(&self, texture: &wgpu::Texture, srgb_view: bool) -> wgpu::BindGroup {
        let view = texture.create_view(&wgpu::TextureViewDescriptor {
            format: srgb_view.then_some(wgpu::TextureFormat::Rgba8UnormSrgb),
            usage: Some(wgpu::TextureUsages::TEXTURE_BINDING), // sRGB views cannot be storage
            ..Default::default()
        });
        self.gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("present"),
            layout: &self.bind_layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&view) },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&self.sampler) },
            ],
        })
    }

    fn draw(&mut self, frame: Option<Frame>, link: &HostLink) -> Result<(), String> {
        self.menu.prepare(&self.window, link, frame.is_some());
        self.apply_settings();
        let frame = match frame {
            // Adapters without compute keep the complete software renderer.
            Some(Frame::Gpu(f)) if !self.compute_ok => Some(Frame::Pixels(f.snap.screens())),
            other => other,
        };
        if let Some(Frame::Gpu(f)) = &frame {
            if self.gpu_path.is_none() {
                let mut renderer = Renderer3d::new_wide(&self.gpu, self.scale, self.layout.margin);
                renderer.set_texture_dump(self.display.dump_textures.clone());
                if let Some(pack) = &self.display.texture_pack {
                    match renderer.set_texture_pack(Some(pack.clone())) {
                        Ok(n) => eprintln!("Texture pack: {} ({n} replacements)", pack.display()),
                        Err(e) => eprintln!("Texture pack not used: {e}"),
                    }
                }
                let mut compositor = Compositor::new_wide(&self.gpu, self.scale, self.layout.margin);
                compositor.set_combined(&self.gpu, self.display.combined(self.compute_ok));
                let bind = self.bind_texture(compositor.output(), self.config.format.is_srgb());
                self.gpu_path = Some((renderer, compositor, bind));
            }
            let combined = self.display.combined(self.compute_ok);
            let scene = crate::arrange::Scene::of(link.phase.load(Ordering::Acquire));
            self.layout.force_overlay = combined && scene == crate::arrange::Scene::Race;
            self.a_top = f.snap.a_top();
            let (renderer, compositor, _) = self.gpu_path.as_mut().unwrap();
            compositor.set_combined(&self.gpu, combined);
            let overlay = map_overlay(&self.display, self.layout.margin, self.layout.force_overlay);
            compositor.set_overlay(if combined && scene != crate::arrange::Scene::Race { None } else { overlay });
            let layer = renderer.render(&self.gpu, &f.snap);
            let start = std::time::Instant::now();
            compositor.render_raw(&self.gpu, &f.snap, layer);
            let took = start.elapsed();
            let mut stats = link.gpu_2d_stats.lock().unwrap();
            stats.0 += took;
            stats.1 += 1;
            stats.2 += compositor.uploaded_2d();
            self.showing_gpu = true;
        }
        if let Some(Frame::Pixels(pixels)) = &frame {
            self.showing_gpu = false;
            for (out, p) in self.rgba.chunks_exact_mut(4).zip(pixels) {
                out.copy_from_slice(&[(p >> 16) as u8, (p >> 8) as u8, *p as u8, 255]);
            }
            self.gpu.queue.write_texture(
                self.screens.as_image_copy(),
                &self.rgba,
                wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(W * 4), rows_per_image: Some(H) },
                self.screens.size(),
            );
        }
        let surface_texture = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(t) | wgpu::CurrentSurfaceTexture::Suboptimal(t) => t,
            wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => return Ok(()),
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                self.surface.configure(&self.gpu.device, &self.config);
                self.window.request_redraw();
                return Ok(());
            }
            other => return Err(format!("Display failed: {other:?}")),
        };
        let view = surface_texture.texture.create_view(&Default::default());
        let mut encoder = self.gpu.device.create_command_encoder(&Default::default());
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("present"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: wgpu::StoreOp::Store },
                })],
                ..Default::default()
            });
            let l = self.classic_layout();
            if self.combined_active() {
                self.draw_combined(&mut pass, link);
            } else if l.top.w >= 1.0 || l.bottom.w >= 1.0 {
                pass.set_pipeline(&self.pipeline);
                let bind = match (&self.gpu_path, self.showing_gpu) {
                    (Some((_, _, gpu_bind)), true) => gpu_bind,
                    _ => &self.bind,
                };
                pass.set_bind_group(0, bind, &[]);
                for (part, r) in [(1u32, l.top), (2, l.bottom)] {
                    if r.w < 1.0 || r.h < 1.0 { continue; }
                    pass.set_viewport(r.x as f32, r.y as f32, r.w as f32, r.h as f32, 0.0, 1.0);
                    pass.draw(0..3, part..part + 1);
                }
            }
        }
        self.menu.paint(&self.gpu, &self.window, &view, &mut encoder);
        self.gpu.queue.submit([encoder.finish()]);
        self.window.pre_present_notify();
        self.gpu.queue.present(surface_texture);
        Ok(())
    }
}

impl Presenter {
    /// Combined layout: the scene's arrangement (sliding from the previous one for [`crate::arrange::TRANSITION`]).
    fn draw_combined(&mut self, pass: &mut wgpu::RenderPass<'_>, link: &HostLink) {
        use crate::arrange;
        let Some((_, compositor, _)) = &self.gpu_path else { return };
        let Some(backdrop) = compositor.backdrop() else { return };
        let scene = arrange::Scene::of(link.phase.load(Ordering::Acquire));
        let (w, h) = (self.config.width, self.config.height);
        let target = arrange::arrange(scene, w as f32, h as f32, self.layout.margin);
        let now = std::time::Instant::now();
        if self.scene != Some(scene) {
            self.from = if self.scene.is_some() { std::mem::take(&mut self.shown) } else { target.clone() };
            self.since = now;
            self.scene = Some(scene);
        }
        let t = now.duration_since(self.since).as_secs_f32() / arrange::TRANSITION;
        self.shown = if t >= 1.0 { target } else { arrange::blend(&self.from, &target, t) };
        if t < 1.0 {
            self.window.request_redraw();
        }
        let out = compositor.output();
        let view = out.create_view(&wgpu::TextureViewDescriptor {
            format: self.config.format.is_srgb().then_some(wgpu::TextureFormat::Rgba8UnormSrgb),
            usage: Some(wgpu::TextureUsages::TEXTURE_BINDING),
            ..Default::default()
        });
        let inputs = crate::gpu::pieces::Inputs {
            screens: &view,
            size: (out.width(), out.height()),
            scale: compositor.scale(),
            margin: compositor.margin(),
            backdrop,
            a_top: self.a_top,
        };
        let pieces = self.pieces.get_or_insert_with(|| crate::gpu::pieces::PiecesPass::new(&self.gpu, self.config.format));
        pieces.draw(&self.gpu, pass, &inputs, &self.shown, w, h);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlay_blocks_all_input_until_closed() {
        let link = HostLink::for_tests();
        link.set_for_tests(&[0], Some((42,64)), true);
        assert!(link.any_down(&[0]) && link.focused());
        link.menu_open.store(true, Ordering::Release);
        assert!(!link.any_down(&[0]) && !link.focused());
        assert_eq!(link.touch(), None);
        link.menu_open.store(false, Ordering::Release);
        assert_eq!(link.touch(), Some((42,64)));
    }

    #[test]
    fn gap_swap_and_single_screen_keep_touch_on_the_visible_bottom_screen() {
        let mut s = mkds_builder::settings::Settings::default();
        assert_eq!(Layout::configured(512,768,&s,0), Layout::fit(512,768,false));
        // Widescreen 16:9 (40 extra DS pixels per side): a 1920x1080 single screen fills the height, 336:192 wide.
        let single = mkds_builder::settings::Settings { single_screen: true, ..s.clone() };
        let w = Layout::configured(1920,1080,&single,40);
        assert_eq!((w.top.w, w.top.h), (336.0 * 5.625, 1080.0));
        // Touch lands on the bottom screen's centred 256 columns; the side bars are not the touch screen.
        let w = Layout::configured(336,384,&s,40);
        assert_eq!(w.touch(40.0,192.0),Some((0,0)));
        assert_eq!(w.touch(295.0,192.0),Some((255,0)));
        assert_eq!(w.touch(20.0,192.0),None);
        // Clicking the map overlay taps the touch screen: single 16:9 screen, overlay at 50% height, Track style.
        let settings = mkds_builder::settings::Settings { single_screen: true, map_overlay: true, map_overlay_size: 50, map_overlay_track: true, ..s.clone() };
        let display = Display { settings: settings.clone(), controls_path: None, scale: None, smooth: false, side_by_side: false, fullscreen: false, texture_pack: None, dump_textures: None, widescreen_mod: true };
        let l = Layout::configured(336,192,&settings,40);
        let (r, _, _) = map_overlay(&display, 40, false).unwrap();
        assert_eq!(r, [336 - 72 - 4, 192 - 96 - 4, 72, 96]);
        assert_eq!(overlay_touch(&display, &l, (r[0] + 36) as f64, (r[1] + 48) as f64), Some((112 + 72, 96)));
        assert_eq!(overlay_touch(&display, &l, 10.0, 10.0), None);
        s.gap = 32;
        let l = Layout::configured(256,416,&s,0);
        assert_eq!(l.touch(128.0,200.0),None);
        assert_eq!(l.touch(128.0,224.0),Some((128,0)));
        s.swap_screens = true;
        assert_eq!(Layout::configured(256,416,&s,0).touch(128.0,0.0),Some((128,0)));
        s.single_screen = true;
        let l = Layout::configured(512,384,&s,0);
        assert_eq!(l.touch(256.0,192.0),Some((128,96)));
        assert_eq!(l.auto_scale(),2);
        s.swap_screens = false;
        assert_eq!(Layout::configured(512,384,&s,0).touch(256.0,192.0),None);
    }

    #[test]
    fn letters_follow_the_label_other_keys_their_position() {
        let name = |code, logical: Key| key_index(code, &logical).map(|k| controls::KEYS[k]);
        let ch = |s: &str| Key::Character(s.into());
        assert_eq!(name(KeyCode::KeyZ, ch("z")), Some("Z"));
        assert_eq!(name(KeyCode::KeyW, ch("z")), Some("Z"), "AZERTY: the key labelled Z");
        assert_eq!(name(KeyCode::KeyA, ch("Q")), Some("Q"), "shifted letters too");
        assert_eq!(name(KeyCode::KeyQ, ch("й")), Some("Q"), "non-Latin layouts fall back to the position");
        assert_eq!(name(KeyCode::Digit1, ch("&")), Some("Key1"));
        assert_eq!(name(KeyCode::Numpad7, ch("7")), Some("NumPad7"));
        assert_eq!(name(KeyCode::ShiftRight, Key::Named(winit::keyboard::NamedKey::Shift)), Some("RightShift"));
        assert_eq!(name(KeyCode::Enter, Key::Named(winit::keyboard::NamedKey::Enter)), Some("Enter"));
        assert_eq!(name(KeyCode::F20, Key::Named(winit::keyboard::NamedKey::F20)), None);
    }

    #[test]
    fn the_screens_keep_their_shape_and_touch_maps_to_the_bottom_screen() {
        let l = Layout::fit(512, 768, false);
        assert_eq!(l.top, Rect { x: 0.0, y: 0.0, w: 512.0, h: 384.0 });
        assert_eq!(l.bottom, Rect { x: 0.0, y: 384.0, w: 512.0, h: 384.0 });
        assert_eq!(l.touch(0.0, 384.0), Some((0, 0)));
        assert_eq!(l.touch(511.0, 767.0), Some((255, 191)));
        assert_eq!(l.touch(100.0, 383.0), None, "top screen is not touchable");
        assert_eq!(l.auto_scale(), 2);
        let wide = Layout::fit(1920, 1080, false);
        assert_eq!((wide.top.w, wide.top.h, wide.top.x, wide.top.y), (720.0, 540.0, 600.0, 0.0));
        assert_eq!(wide.touch(599.0, 900.0), None, "black bars");
        assert_eq!(wide.touch(600.0 + 360.0, 540.0 + 270.0), Some((128, 96)));
        assert_eq!(wide.auto_scale(), 3, "540 window pixels per screen need 3x");
        let side = Layout::fit(1920, 1080, true);
        assert_eq!(side.top, Rect { x: 0.0, y: 180.0, w: 960.0, h: 720.0 });
        assert_eq!(side.bottom.x, 960.0);
        assert_eq!(side.touch(960.0 + 480.0, 180.0 + 360.0), Some((128, 96)));
        assert_eq!(side.touch(480.0, 540.0), None, "left is the top screen");
        assert_eq!(Layout::fit(3840, 2160, false).auto_scale(), AUTO_MAX);
    }
}
