//! `--race SPEC`: skip the menus and start a race directly (for testing mods, e.g. the Toolbox Test Bench).
//!
//! SPEC is comma-separated `key=value`:
//! - `course=<id or name>`: a course id, or the course's internal name as the game's course table lists it
//!   (`--list-courses` prints them from your own game);
//! - `character=N`, `kart=N`: the player's character and kart ids;
//! - `racers=N`: number of racers (1-8).
//!
//! How it works (USA revision 0 only, the one supported dump): once the game has reached a scene of its own,
//! the scene manager's "next scene" slot is set to the race scene. The game restores its race setup when the
//! scene starts, so the chosen values are written for a short while after that, before the race reads them.
//! Only the values you set are touched; everything else stays as the game set it.

use recomp::rt::Mem;

/// `g_scene_history`: previous, current and requested next scene (u16 each; 0xFFFF = none).
const SCENE_HISTORY: u32 = 0x0216_53B8;
/// The race scene id.
const SCENE_RACE: u16 = 2;
/// Pointer to the race setup.
const RACE_SETUP: u32 = 0x0217_59A0;
/// The course table: 8 bytes per course, the course's name pointer at +4.
const COURSE_TABLE: u32 = 0x0215_422C;
/// Frames after the scene switch during which the chosen values are written.
const HOLD_FRAMES: u64 = 90;

#[derive(Debug, Default, Clone)]
pub struct RaceSpec {
    pub course: Option<String>,
    pub character: Option<u32>,
    pub kart: Option<u32>,
    pub racers: Option<u32>,
}

impl RaceSpec {
    pub fn parse(spec: &str) -> Result<Self, String> {
        let mut r = RaceSpec::default();
        for part in spec.split(',').map(str::trim).filter(|p| !p.is_empty()) {
            let (k, v) = part.split_once('=').ok_or(format!("--race: expected key=value, got {part}"))?;
            let n = || v.trim().parse::<u32>().map_err(|_| format!("--race: {k} needs a number"));
            match k.trim() {
                "course" => r.course = Some(v.trim().to_string()),
                "character" => r.character = Some(n()?),
                "kart" => r.kart = Some(n()?),
                "racers" => r.racers = Some(n()?.clamp(1, 8)),
                other => return Err(format!("--race: unknown key {other} (course, character, kart, racers)")),
            }
        }
        Ok(r)
    }
}

fn valid(p: u32) -> bool {
    (0x0200_0000..0x0240_0000).contains(&p)
}

fn c_string(m: &mut impl Mem, mut a: u32) -> String {
    let mut s = String::new();
    for _ in 0..64 {
        let b = m.r8(a);
        if b == 0 || !b.is_ascii_graphic() {
            break;
        }
        s.push(b as char);
        a += 1;
    }
    s
}

/// The courses in the game's own course table: (id, internal name).
pub fn courses(m: &mut impl Mem) -> Vec<(u32, String)> {
    let mut out = Vec::new();
    for i in 0..64u32 {
        let p = m.r32(COURSE_TABLE + i * 8 + 4);
        if !valid(p) {
            break;
        }
        let name = c_string(m, p);
        if name.is_empty() || name.contains('/') || name.contains('.') {
            break;
        }
        out.push((i, name));
    }
    out
}

enum State {
    Waiting,
    Requested,
    Holding { until: u64 },
    Done,
}

pub struct Race {
    spec: RaceSpec,
    course: Option<u32>,
    state: State,
}

impl Race {
    pub fn new(spec: RaceSpec) -> Self {
        Race { spec, course: None, state: State::Waiting }
    }

    fn write(&self, m: &mut impl Mem) {
        let base = m.r32(RACE_SETUP);
        if !valid(base) {
            return;
        }
        if let Some(c) = self.course {
            m.w32(base, c);
        }
        if let Some(v) = self.spec.character {
            m.w32(base + 0x68, v);
        }
        if let Some(v) = self.spec.kart {
            m.w32(base + 0x6C, v);
        }
        if let Some(v) = self.spec.racers {
            m.w8(base + 0x3D0, v as u8);
        }
    }

    /// Called once per frame.
    pub fn frame(&mut self, frame: u64, m: &mut impl Mem) -> Result<(), String> {
        let current = m.r16(SCENE_HISTORY + 2);
        let next = m.r16(SCENE_HISTORY + 4);
        match self.state {
            State::Waiting => {
                // wait until the game runs a scene of its own and no switch is pending
                if frame >= 30 && current != 0xFFFF && next == 0xFFFF {
                    if let Some(c) = &self.spec.course {
                        let list = courses(m);
                        let id = match c.parse::<u32>() {
                            Ok(n) => n,
                            Err(_) => list
                                .iter()
                                .find(|(_, n)| n.eq_ignore_ascii_case(c))
                                .map(|(i, _)| *i)
                                .ok_or_else(|| format!("--race: no course named {c} (try --list-courses)"))?,
                        };
                        self.course = Some(id);
                    }
                    m.w16(SCENE_HISTORY + 4, SCENE_RACE);
                    eprintln!("Race setup: starting a race from scene {current} at frame {frame}");
                    self.state = State::Requested;
                }
            }
            State::Requested => {
                if current == SCENE_RACE {
                    self.write(m);
                    self.state = State::Holding { until: frame + HOLD_FRAMES };
                }
            }
            State::Holding { until } => {
                self.write(m);
                if frame >= until {
                    self.state = State::Done;
                }
            }
            State::Done => {}
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::RaceSpec;

    #[test]
    fn parses_race_specs() {
        let r = RaceSpec::parse("course=mario_course, character=3,kart=9,racers=12").unwrap();
        assert_eq!(r.course.as_deref(), Some("mario_course"));
        assert_eq!((r.character, r.kart, r.racers), (Some(3), Some(9), Some(8)));
        assert!(RaceSpec::parse("class=2").is_err());
        assert!(RaceSpec::parse("course").is_err());
        assert!(RaceSpec::parse("speed=3").is_err());
        assert!(RaceSpec::parse("kart=x").is_err());
    }
}
