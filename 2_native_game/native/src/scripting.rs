//! Lua (Luau) scripting for mods, step 1 (design: MKDSrust-Docs `4_docs/LUA_SCRIPTING_DESIGN.md`).
//!
//! Each script runs in its own sandboxed Luau VM (no files, OS or network), with a CPU budget per frame and a
//! memory limit. A script that errors or runs over budget is switched off with a log line; the game keeps going.
//! API v1:
//! - `game.on("frame", fn(frame))`, `game.frame()`, `game.scene()`
//! - `race.racers()` -> list of `{ index, x, y, z, is_player }` (positions in world units)
//! - `race.player()` -> `{ x, y, z, lap, place, ticks }` or nil outside a race
//! - `input.A`.. `input.Y` (button bits), `input.buttons()` (held this frame; X/Y can't be read yet),
//!   `input.held(input.A)`, `input.set(mask)` (hold exactly these buttons next frame, instead of the player's)
//! - `store.get(key)`, `store.set(key, value)`: small saved settings (strings, numbers, booleans; 64 KiB)
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
/// KEYINPUT (active low): A B Select Start Right Left Up Down R L.
const KEYINPUT: u32 = 0x0400_0130;
/// Saved-settings size limit per script.
const STORE_LIMIT: usize = 64 << 10;
const BUTTONS: [(&str, u16); 12] = [
    ("A", 1), ("B", 2), ("SELECT", 4), ("START", 8), ("RIGHT", 16), ("LEFT", 32), ("UP", 64), ("DOWN", 128),
    ("R", 256), ("L", 512), ("X", 1024), ("Y", 2048),
];

struct Script {
    name: String,
    lua: Lua,
    deadline: Rc<Cell<Option<Instant>>>,
    /// The data the API functions return this frame (set before the handlers run).
    snap: Rc<RefCell<(u64, Snapshot)>>,
    handlers: Rc<RefCell<Vec<Function>>>,
    /// Buttons `input.set` asked for this frame.
    input: Rc<Cell<Option<u16>>>,
    /// Saved settings, and whether they changed this frame.
    store: Rc<RefCell<(serde_json::Map<String, serde_json::Value>, bool)>>,
    store_path: Option<PathBuf>,
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
    #[cfg(test)]
    fn load(name: String, source: &str) -> Result<Script, String> {
        Self::load_with_store(name, source, None)
    }

    fn load_with_store(name: String, source: &str, store_path: Option<PathBuf>) -> Result<Script, String> {
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
        // input
        let input_set: Rc<Cell<Option<u16>>> = Rc::new(Cell::new(None));
        let input = lua.create_table().map_err(err)?;
        for (k, v) in BUTTONS {
            input.set(k, v).map_err(err)?;
        }
        let sn = snap.clone();
        input.set("buttons", lua.create_function(move |_, ()| Ok(sn.borrow().1.buttons)).map_err(err)?).map_err(err)?;
        let sn = snap.clone();
        input
            .set("held", lua.create_function(move |_, mask: u16| Ok(sn.borrow().1.buttons & mask == mask)).map_err(err)?)
            .map_err(err)?;
        let set = input_set.clone();
        input
            .set(
                "set",
                lua.create_function(move |_, mask: u16| {
                    set.set(Some(mask & 0xFFF));
                    Ok(())
                })
                .map_err(err)?,
            )
            .map_err(err)?;
        g.set("input", input).map_err(err)?;
        // store
        let saved: serde_json::Map<String, serde_json::Value> = store_path
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        let store_cell = Rc::new(RefCell::new((saved, false)));
        let store = lua.create_table().map_err(err)?;
        let st = store_cell.clone();
        store
            .set(
                "get",
                lua.create_function(move |lua, key: String| {
                    Ok(match st.borrow().0.get(&key) {
                        Some(serde_json::Value::String(v)) => Value::String(lua.create_string(v)?),
                        Some(serde_json::Value::Bool(b)) => Value::Boolean(*b),
                        Some(serde_json::Value::Number(n)) => Value::Number(n.as_f64().unwrap_or(0.0)),
                        _ => Value::Nil,
                    })
                })
                .map_err(err)?,
            )
            .map_err(err)?;
        let st = store_cell.clone();
        store
            .set(
                "set",
                lua.create_function(move |_, (key, v): (String, Value)| {
                    let j = match v {
                        Value::Nil => serde_json::Value::Null,
                        Value::Boolean(b) => serde_json::Value::Bool(b),
                        Value::Integer(i) => serde_json::Value::from(i),
                        Value::Number(n) => serde_json::Value::from(n),
                        Value::String(s) => serde_json::Value::String(s.to_str()?.to_string()),
                        _ => return Err(mlua::Error::runtime("store values are strings, numbers or booleans")),
                    };
                    let mut s = st.borrow_mut();
                    if j.is_null() {
                        s.0.remove(&key);
                    } else {
                        s.0.insert(key, j);
                    }
                    if serde_json::to_string(&s.0).map_or(0, |t| t.len()) > STORE_LIMIT {
                        return Err(mlua::Error::runtime("store is full (64 KiB)"));
                    }
                    s.1 = true;
                    Ok(())
                })
                .map_err(err)?,
            )
            .map_err(err)?;
        g.set("store", store).map_err(err)?;
        lua.sandbox(true).map_err(err)?;
        let s = Script { name, lua, deadline, snap, handlers, input: input_set, store: store_cell, store_path, off: false };
        s.deadline.set(Some(Instant::now() + FRAME_BUDGET * 50));
        let r = s.lua.load(source).set_name(s.name.clone()).exec();
        s.deadline.set(None);
        r.map_err(|e| format!("{}: {e}", s.name))?;
        Ok(s)
    }

    /// Runs the frame handlers; returns the buttons `input.set` asked for.
    fn frame(&mut self, frame: u64, snap: Snapshot) -> Option<u16> {
        if self.off {
            return None;
        }
        self.input.set(None);
        *self.snap.borrow_mut() = (frame, snap);
        let handlers: Vec<Function> = self.handlers.borrow().clone();
        self.deadline.set(Some(Instant::now() + FRAME_BUDGET));
        let r = handlers.iter().try_for_each(|f| f.call::<()>(frame));
        self.deadline.set(None);
        if let Err(e) = r {
            eprintln!("[lua {}] switched off: {e}", self.name);
            self.off = true;
            return None;
        }
        let mut st = self.store.borrow_mut();
        if st.1 {
            st.1 = false;
            if let Some(p) = &self.store_path {
                let saved = serde_json::to_string_pretty(&st.0)
                    .map_err(|e| e.to_string())
                    .and_then(|t| std::fs::write(p, t).map_err(|e| e.to_string()));
                if let Err(e) = saved {
                    eprintln!("[lua {}] cannot save its store: {e}", self.name);
                }
            }
        }
        self.input.get()
    }
}

/// What scripts can read this frame (taken once per frame from guest memory).
#[derive(Default, Clone)]
struct Snapshot {
    scene: u16,
    buttons: u16,
    racers: Vec<(f64, f64, f64)>,
    /// x, y, z, lap, place, ticks
    player: Option<(f64, f64, f64, u32, u32, u32)>,
}

fn snapshot(m: &mut impl Mem) -> Snapshot {
    let mut s = Snapshot { scene: m.r16(SCENE_HISTORY + 2), buttons: !m.r16(KEYINPUT) & 0x3FF, ..Default::default() };
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
        // (name, script, where its saved settings live)
        let mut paths: Vec<(String, PathBuf, PathBuf)> =
            files.iter().map(|p| (p.display().to_string(), p.clone(), p.with_extension("store.json"))).collect();
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
                let stores = game.join("mods").join(&id).join(".store");
                for p in found {
                    let file = p.file_name().unwrap().to_string_lossy().into_owned();
                    let _ = std::fs::create_dir_all(&stores);
                    paths.push((format!("{id}/{file}"), p, stores.join(format!("{file}.json"))));
                }
            }
        }
        if paths.is_empty() {
            return None;
        }
        let mut scripts = Vec::new();
        for (name, path, store) in paths {
            let loaded = std::fs::read_to_string(&path)
                .map_err(|e| e.to_string())
                .and_then(|src| Script::load_with_store(name.clone(), &src, Some(store)));
            match loaded {
                Ok(s) => {
                    eprintln!("[lua] loaded {name}");
                    scripts.push(s);
                }
                Err(e) => eprintln!("[lua] {name} not loaded: {e}"),
            }
        }
        Some(Scripts { scripts })
    }

    /// Runs every script's `frame` handlers once; returns the buttons to hold next frame, if a script set
    /// them (the last script in load order wins).
    pub fn frame(&mut self, frame: u64, m: &mut impl Mem) -> Option<u16> {
        if self.scripts.iter().all(|s| s.off) {
            return None;
        }
        let snap = snapshot(m);
        let mut held = None;
        for s in &mut self.scripts {
            if let Some(b) = s.frame(frame, snap.clone()) {
                held = Some(b);
            }
        }
        held
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap() -> Snapshot {
        Snapshot { scene: 2, buttons: 1, racers: vec![(1.0, 2.0, 3.0), (4.0, 5.0, 6.0)], player: Some((1.0, 2.0, 3.0, 2, 1, 600)) }
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
    fn input_reads_and_holds_buttons() {
        let mut s = Script::load(
            "t".into(),
            "game.on('frame', function() assert(input.held(input.A) and not input.held(input.B)) input.set(input.B + input.X) end)",
        )
        .unwrap();
        assert_eq!(s.frame(1, snap()), Some(2 | 1024));
        let mut quiet = Script::load("t".into(), "game.on('frame', function() end)").unwrap();
        assert_eq!(quiet.frame(1, snap()), None);
    }

    #[test]
    fn store_saves_and_reloads() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("s.json");
        let src = "game.on('frame', function() store.set('laps', (store.get('laps') or 0) + 1) store.set('name', 'x') end)";
        let mut s = Script::load_with_store("t".into(), src, Some(p.clone())).unwrap();
        s.frame(1, snap());
        s.frame(2, snap());
        let again = Script::load_with_store("t".into(), "assert(store.get('laps') == 2 and store.get('name') == 'x')", Some(p));
        assert!(again.is_ok());
        let mut big = Script::load("t".into(), "game.on('frame', function() store.set('k', string.rep('x', 70000)) end)").unwrap();
        big.frame(1, snap());
        assert!(big.off);
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
