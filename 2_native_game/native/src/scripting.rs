//! Lua (Luau) scripting for mods, step 1 (design: MKDSrust-Docs `4_docs/LUA_SCRIPTING_DESIGN.md`).
//!
//! Each script runs in its own sandboxed Luau VM (no files, OS or network), with a CPU budget per frame and a
//! memory limit. A script that errors or runs over budget is switched off with a log line; the game keeps going.
//! API v1 (read-only for now):
//! - `game.on("frame", fn(frame))`, `game.frame()`, `game.scene()`
//! - `race.racers()` -> list of `{ index, x, y, z, is_player }` (positions in world units)
//! - `race.player()` -> `{ x, y, z, lap, place, ticks }` or nil outside a race
//! - `log(...)`
//! Scripts come from `--script FILE` (repeatable) and from enabled mods' `scripts/*.luau`.

use mlua::{Function, Lua, LuaOptions, StdLib, Value, VmState};
use recomp::rt::Mem;
use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

/// CPU time a script may use per frame.
const FRAME_BUDGET: Duration = Duration::from_millis(2);
/// Memory a script's VM may use.
const MEMORY_LIMIT: usize = 16 << 20;

const KART_ARRAY: u32 = 0x0217_ACF8;
const KART_SIZE: u32 = 0x5A8;
const RACE_STATE: u32 = 0x0217_55FC;
const RACE_SETUP: u32 = 0x0217_59A0;
const SCENE_HISTORY: u32 = 0x0216_53B8;

struct Script {
    name: String,
    lua: Lua,
    deadline: Rc<Cell<Option<Instant>>>,
    /// The data the API functions return this frame (set before the handlers run).
    snap: Rc<RefCell<(u64, Snapshot)>>,
    handlers: Rc<RefCell<Vec<Function>>>,
    off: bool,
}

pub struct Scripts {
    scripts: Vec<Script>,
}

fn valid(p: u32, len: u32) -> bool {
    p >= 0x0200_0000 && p.checked_add(len).is_some_and(|e| e <= 0x0240_0000)
}

fn fx(v: u32) -> f64 {
    v as i32 as f64 / 4096.0
}

fn err(e: mlua::Error) -> String {
    e.to_string()
}

impl Script {
    fn load(name: String, source: &str) -> Result<Script, String> {
        let lua = Lua::new_with(StdLib::ALL_SAFE, LuaOptions::default()).map_err(err)?;
        lua.set_memory_limit(MEMORY_LIMIT).map_err(err)?;
        let deadline: Rc<Cell<Option<Instant>>> = Rc::new(Cell::new(None));
        let d = deadline.clone();
        lua.set_interrupt(move |_| match d.get() {
            Some(t) if Instant::now() > t => Err(mlua::Error::runtime("over its CPU budget for this frame")),
            _ => Ok(VmState::Continue),
        });
        let snap: Rc<RefCell<(u64, Snapshot)>> = Rc::new(RefCell::new((0, Snapshot::default())));
        let handlers: Rc<RefCell<Vec<Function>>> = Rc::new(RefCell::new(Vec::new()));
        let g = lua.globals();
        let tag = name.clone();
        g.set(
            "log",
            lua.create_function(move |_, args: mlua::Variadic<Value>| {
                let parts: Vec<String> = args.iter().map(|v| v.to_string().unwrap_or_else(|_| "?".into())).collect();
                eprintln!("[lua {tag}] {}", parts.join(" "));
                Ok(())
            })
            .map_err(err)?,
        )
        .map_err(err)?;
        let game = lua.create_table().map_err(err)?;
        let h = handlers.clone();
        game.set(
            "on",
            lua.create_function(move |_, (event, f): (String, Function)| {
                if event != "frame" {
                    return Err(mlua::Error::runtime(format!("unknown event {event} (v1 has: frame)")));
                }
                h.borrow_mut().push(f);
                Ok(())
            })
            .map_err(err)?,
        )
        .map_err(err)?;
        let sn = snap.clone();
        game.set("frame", lua.create_function(move |_, ()| Ok(sn.borrow().0)).map_err(err)?).map_err(err)?;
        let sn = snap.clone();
        game.set("scene", lua.create_function(move |_, ()| Ok(sn.borrow().1.scene)).map_err(err)?).map_err(err)?;
        g.set("game", game).map_err(err)?;
        let race = lua.create_table().map_err(err)?;
        let sn = snap.clone();
        race.set(
            "racers",
            lua.create_function(move |lua, ()| {
                let t = lua.create_table()?;
                for (i, k) in sn.borrow().1.racers.iter().enumerate() {
                    let r = lua.create_table()?;
                    r.set("index", i)?;
                    r.set("x", k.0)?;
                    r.set("y", k.1)?;
                    r.set("z", k.2)?;
                    r.set("is_player", i == 0)?;
                    t.set(i + 1, r)?;
                }
                Ok(t)
            })
            .map_err(err)?,
        )
        .map_err(err)?;
        let sn = snap.clone();
        race.set(
            "player",
            lua.create_function(move |lua, ()| {
                let Some(p) = sn.borrow().1.player else { return Ok(Value::Nil) };
                let t = lua.create_table()?;
                t.set("x", p.0)?;
                t.set("y", p.1)?;
                t.set("z", p.2)?;
                t.set("lap", p.3)?;
                t.set("place", p.4)?;
                t.set("ticks", p.5)?;
                Ok(Value::Table(t))
            })
            .map_err(err)?,
        )
        .map_err(err)?;
        g.set("race", race).map_err(err)?;
        lua.sandbox(true).map_err(err)?;
        let s = Script { name, lua, deadline, snap, handlers, off: false };
        s.deadline.set(Some(Instant::now() + FRAME_BUDGET * 50));
        let r = s.lua.load(source).set_name(s.name.clone()).exec();
        s.deadline.set(None);
        r.map_err(|e| format!("{}: {e}", s.name))?;
        Ok(s)
    }

    fn frame(&mut self, frame: u64, snap: Snapshot) {
        if self.off {
            return;
        }
        *self.snap.borrow_mut() = (frame, snap);
        let handlers: Vec<Function> = self.handlers.borrow().clone();
        self.deadline.set(Some(Instant::now() + FRAME_BUDGET));
        let r = handlers.iter().try_for_each(|f| f.call::<()>(frame));
        self.deadline.set(None);
        if let Err(e) = r {
            eprintln!("[lua {}] switched off: {e}", self.name);
            self.off = true;
        }
    }
}

/// What scripts can read this frame (taken once per frame from guest memory).
#[derive(Default, Clone)]
struct Snapshot {
    scene: u16,
    racers: Vec<(f64, f64, f64)>,
    /// x, y, z, lap, place, ticks
    player: Option<(f64, f64, f64, u32, u32, u32)>,
}

fn snapshot(m: &mut impl Mem) -> Snapshot {
    let mut s = Snapshot { scene: m.r16(SCENE_HISTORY + 2), ..Default::default() };
    let karts = m.r32(KART_ARRAY);
    let setup = m.r32(RACE_SETUP);
    let count = if valid(setup, 0x3D4) { (m.r8(setup + 0x3D0) as u32).clamp(1, 8) } else { 0 };
    if valid(karts, KART_SIZE * count.max(1)) {
        for i in 0..count {
            let k = karts + i * KART_SIZE;
            s.racers.push((fx(m.r32(k + 0x80)), fx(m.r32(k + 0x84)), fx(m.r32(k + 0x88))));
        }
        let race = m.r32(RACE_STATE);
        if valid(race, 0x500) {
            let p = s.racers.first().copied().unwrap_or_default();
            s.player = Some((p.0, p.1, p.2, m.r32(race + 0x38), (m.r16(race + 0x50) & 15) as u32, m.r32(race + 0x18)));
        }
    }
    s
}

impl Scripts {
    /// Loads `--script` files and the `scripts/*.luau` of enabled mods (when `mods` is set).
    pub fn load(files: &[PathBuf], mods: Option<&Path>) -> Option<Scripts> {
        let mut paths: Vec<(String, PathBuf)> = files.iter().map(|p| (p.display().to_string(), p.clone())).collect();
        if let Some(game) = mods {
            for id in mkds_builder::modpkg::enabled(game) {
                let dir = game.join("mods").join(&id).join("scripts");
                let mut found: Vec<PathBuf> = std::fs::read_dir(&dir)
                    .into_iter()
                    .flatten()
                    .flatten()
                    .map(|e| e.path())
                    .filter(|p| p.extension().is_some_and(|x| x == "luau" || x == "lua"))
                    .collect();
                found.sort();
                paths.extend(found.into_iter().map(|p| (format!("{id}/{}", p.file_name().unwrap().to_string_lossy()), p)));
            }
        }
        if paths.is_empty() {
            return None;
        }
        let mut scripts = Vec::new();
        for (name, path) in paths {
            match std::fs::read_to_string(&path).map_err(|e| e.to_string()).and_then(|src| Script::load(name.clone(), &src)) {
                Ok(s) => {
                    eprintln!("[lua] loaded {name}");
                    scripts.push(s);
                }
                Err(e) => eprintln!("[lua] {name} not loaded: {e}"),
            }
        }
        Some(Scripts { scripts })
    }

    /// Runs every script's `frame` handlers once.
    pub fn frame(&mut self, frame: u64, m: &mut impl Mem) {
        if self.scripts.iter().all(|s| s.off) {
            return;
        }
        let snap = snapshot(m);
        for s in &mut self.scripts {
            s.frame(frame, snap.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap() -> Snapshot {
        Snapshot { scene: 2, racers: vec![(1.0, 2.0, 3.0), (4.0, 5.0, 6.0)], player: Some((1.0, 2.0, 3.0, 2, 1, 600)) }
    }

    #[test]
    fn frame_handlers_see_race_state() {
        let mut s = Script::load(
            "t".into(),
            r#"
            seen = 0
            game.on("frame", function(f)
              local r = race.racers()
              assert(#r == 2 and r[2].x == 4 and r[1].is_player)
              assert(race.player().lap == 2 and game.scene() == 2 and game.frame() == f)
            end)
            "#,
        )
        .unwrap();
        s.frame(7, snap());
        assert!(!s.off);
    }

    #[test]
    fn sandbox_has_no_files_or_os() {
        assert!(Script::load("t".into(), "io.open('x')").is_err());
        assert!(Script::load("t".into(), "os.execute('x')").is_err());
        assert!(Script::load("t".into(), "require('x')").is_err());
        assert!(Script::load("t".into(), "game.on = nil").is_err());
    }

    #[test]
    fn runaway_and_failing_scripts_are_switched_off() {
        let mut s = Script::load("t".into(), "game.on('frame', function() while true do end end)").unwrap();
        s.frame(1, snap());
        assert!(s.off);
        let mut s = Script::load("t".into(), "game.on('frame', function() error('boom') end)").unwrap();
        s.frame(1, snap());
        assert!(s.off);
        assert!(Script::load("t".into(), "game.on('lap', function() end)").is_err());
    }

    #[test]
    fn memory_limit_stops_a_hog() {
        let mut s = Script::load(
            "t".into(),
            "game.on('frame', function() local t = {} for i = 1, 1e8 do t[i] = string.rep('x', 64) .. i end end)",
        )
        .unwrap();
        s.frame(1, snap());
        assert!(s.off);
    }
}
