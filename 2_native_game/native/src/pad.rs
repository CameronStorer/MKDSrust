//! Game controllers (block 2 stage B2.1) through `gilrs`: XInput / Windows.Gaming.Input on Windows, evdev on Linux,
//! IOKit on macOS. Buttons are named by position (`controls::PAD`), so the same `controls.txt` works for any pad.
use gilrs::{Axis, Button, Gilrs};
use mkds_builder::controls::PAD;

/// How far the left stick must be pushed to count as a direction.
#[cfg(test)]
const STICK: f32 = 0.5;

pub struct Pads {
    gilrs: Option<Gilrs>,
}

impl Pads {
    /// Controllers are optional: without a working controller system the game simply has none.
    pub fn new() -> Pads {
        match Gilrs::new() {
            Ok(g) => {
                for (_, pad) in g.gamepads() {
                    eprintln!("Controller: {}", pad.name());
                }
                Pads { gilrs: Some(g) }
            }
            Err(e) => {
                eprintln!("Controllers unavailable: {e}");
                Pads { gilrs: None }
            }
        }
    }

    /// No controllers (headless and scripted runs).
    pub fn none() -> Pads {
        Pads { gilrs: None }
    }

    /// Brings the button states up to date; call once before a round of [`Pads::any_down_at`].
    pub fn poll(&mut self) {
        if let Some(g) = self.gilrs.as_mut() {
            while g.next_event().is_some() {}
        }
    }

    /// Whether any connected controller holds any of `buttons` (`controls::PAD` indexes).
    pub fn any_down_at(&self, buttons: &[usize], deadzone: f32) -> bool {
        let Some(g) = self.gilrs.as_ref() else { return false };
        if buttons.is_empty() {
            return false;
        }
        g.gamepads().any(|(_, pad)| buttons.iter().any(|&b| held_at(PAD[b], |x| pad.is_pressed(x), |a| pad.value(a), deadzone)))
    }
    pub fn names(&self) -> Vec<String> {
        self.gilrs.as_ref().map(|g| g.gamepads().map(|(_,p)| p.name().to_owned()).collect()).unwrap_or_default()
    }
    pub fn pressed(&self, deadzone: f32) -> Vec<usize> {
        (0..PAD.len()).filter(|&i| self.any_down_at(&[i], deadzone)).collect()
    }
}

/// Whether the controller button `name` is held, given the pad's button and axis readers.
#[cfg(test)]
fn held(name: &str, pressed: impl Fn(Button) -> bool, axis: impl Fn(Axis) -> f32) -> bool {
    held_at(name, pressed, axis, STICK)
}

fn held_at(name: &str, pressed: impl Fn(Button) -> bool, axis: impl Fn(Axis) -> f32, deadzone: f32) -> bool {
    match name {
        "South" => pressed(Button::South),
        "East" => pressed(Button::East),
        "North" => pressed(Button::North),
        "West" => pressed(Button::West),
        "LB" => pressed(Button::LeftTrigger),
        "RB" => pressed(Button::RightTrigger),
        "LT" => pressed(Button::LeftTrigger2),
        "RT" => pressed(Button::RightTrigger2),
        "Select" => pressed(Button::Select),
        "Start" => pressed(Button::Start),
        "DPadUp" => pressed(Button::DPadUp),
        "DPadDown" => pressed(Button::DPadDown),
        "DPadLeft" => pressed(Button::DPadLeft),
        "DPadRight" => pressed(Button::DPadRight),
        "StickUp" => axis(Axis::LeftStickY) > deadzone,
        "StickDown" => axis(Axis::LeftStickY) < -deadzone,
        "StickLeft" => axis(Axis::LeftStickX) < -deadzone,
        "StickRight" => axis(Axis::LeftStickX) > deadzone,
        "LeftStick" => pressed(Button::LeftThumb),
        "RightStick" => pressed(Button::RightThumb),
        "Guide" => pressed(Button::Mode),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn custom_deadzone_applies_without_changing_digital_buttons() {
        let off = |_: Button| false;
        let axis = |_: Axis| 0.3;
        assert!(held_at("StickUp",off,axis,0.2));
        assert!(!held_at("StickUp",off,axis,0.5));
        assert!(held_at("South",|b| b == Button::South,axis,0.95));
    }

    #[test]
    fn every_controls_name_reads_a_button_or_the_stick() {
        for name in PAD {
            let any = |_: Button| true;
            let full = |_: Axis| 1.0;
            let none = |_: Axis| -1.0;
            assert!(held(name, any, full) || held(name, any, none) || name.starts_with("Stick"), "{name} is never held");
        }
        let stick = |x: f32, y: f32| move |a: Axis| if a == Axis::LeftStickX { x } else { y };
        let off = |_: Button| false;
        assert!(held("StickUp", off, stick(0.0, 0.8)) && !held("StickUp", off, stick(0.0, 0.3)));
        assert!(held("StickLeft", off, stick(-0.9, 0.0)) && !held("StickRight", off, stick(-0.9, 0.0)));
        assert!(held("RT", |b| b == Button::RightTrigger2, stick(0.0, 0.0)));
        assert!(!held("RB", |b| b == Button::RightTrigger2, stick(0.0, 0.0)));
    }
}
