//! Keyboard controls shared by the game window and the builder's menu: one `controls.txt` per user,
//! kept in the user's settings folder so it survives rebuilds. Key names are the game window's
//! (`minifb::Key`) variant names; the game maps them, this module only validates and stores them.
use std::path::PathBuf;

/// The DS buttons in KEYINPUT bit order (A = bit 0 .. L = bit 9, then X = 10, Y = 11), then Quit.
pub const ACTIONS: [&str; 13] = ["A", "B", "Select", "Start", "Right", "Left", "Up", "Down", "R", "L", "X", "Y", "Quit"];
/// What each action does in the game, for the file's comments and the builder's menu.
pub const ROLES: [&str; 13] = [
    "accelerate / confirm", "brake / back", "", "pause / start", "steer right", "steer left", "", "",
    "hop / drift", "use item", "", "", "close the game",
];
pub const QUIT: usize = 12;
const DEFAULTS: [&[&str]; 13] = [
    &["Z"], &["X"], &["RightShift"], &["Enter"], &["Right"], &["Left"], &["Up"], &["Down"],
    &["S"], &["A"], &["C"], &["V"], &["Escape"],
];

/// Every key name the game window knows, in `minifb::Key` order (the game relies on this order).
pub const KEYS: [&str; 106] = [
    "Key0", "Key1", "Key2", "Key3", "Key4", "Key5", "Key6", "Key7", "Key8", "Key9",
    "A", "B", "C", "D", "E", "F", "G", "H", "I", "J", "K", "L", "M", "N", "O", "P", "Q", "R", "S", "T", "U", "V",
    "W", "X", "Y", "Z",
    "F1", "F2", "F3", "F4", "F5", "F6", "F7", "F8", "F9", "F10", "F11", "F12", "F13", "F14", "F15",
    "Down", "Left", "Right", "Up",
    "Apostrophe", "Backquote", "Backslash", "Comma", "Equal", "LeftBracket", "Minus", "Period", "RightBracket",
    "Semicolon", "Slash",
    "Backspace", "Delete", "End", "Enter", "Escape", "Home", "Insert", "Menu", "PageDown", "PageUp", "Pause",
    "Space", "Tab", "NumLock", "CapsLock", "ScrollLock",
    "LeftShift", "RightShift", "LeftCtrl", "RightCtrl",
    "NumPad0", "NumPad1", "NumPad2", "NumPad3", "NumPad4", "NumPad5", "NumPad6", "NumPad7", "NumPad8", "NumPad9",
    "NumPadDot", "NumPadSlash", "NumPadAsterisk", "NumPadMinus", "NumPadPlus", "NumPadEnter",
    "LeftAlt", "RightAlt", "LeftSuper", "RightSuper",
];

/// Game controller buttons (positions, so `South` is Xbox A / PlayStation Cross / Nintendo B). `Stick*` is the left
/// stick pushed past half way. The game maps them to `gilrs` buttons and axes in this order.
pub const PAD: [&str; 21] = [
    "South", "East", "North", "West", "LB", "RB", "LT", "RT", "Select", "Start", "DPadUp", "DPadDown", "DPadLeft",
    "DPadRight", "StickUp", "StickDown", "StickLeft", "StickRight", "LeftStick", "RightStick", "Guide",
];
const PAD_DEFAULTS: [&[&str]; 13] = [
    &["South", "RT"], &["East", "LT"], &["Select"], &["Start"], &["DPadRight", "StickRight"], &["DPadLeft", "StickLeft"],
    &["DPadUp", "StickUp"], &["DPadDown", "StickDown"], &["RB"], &["LB"], &["North"], &["West"], &[],
];

/// Keys per action (indexes into [`KEYS`]) and controller buttons per action (indexes into [`PAD`]); an action may
/// have several or none.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Controls {
    pub keys: [Vec<usize>; 13],
    pub pad: [Vec<usize>; 13],
}

impl Default for Controls {
    fn default() -> Self {
        Self {
            keys: DEFAULTS.map(|names| names.iter().map(|n| key_index(n).unwrap()).collect()),
            pad: PAD_DEFAULTS.map(|names| names.iter().map(|n| pad_index(n).unwrap()).collect()),
        }
    }
}

/// A controller button's index in [`PAD`], in any letter case.
pub fn pad_index(name: &str) -> Option<usize> {
    PAD.iter().position(|k| k.eq_ignore_ascii_case(name.trim()))
}

/// The canonical name's index for a key name in any letter case (`z`, `rightshift`, also `0`-`9`).
pub fn key_index(name: &str) -> Option<usize> {
    let name = name.trim();
    let digit = (name.len() == 1 && name.as_bytes()[0].is_ascii_digit()).then(|| format!("Key{name}"));
    let name = digit.as_deref().unwrap_or(name);
    KEYS.iter().position(|k| k.eq_ignore_ascii_case(name))
}

impl Controls {
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut out = Self::default();
        let mut seen = [[false; 13]; 2];
        for (n, line) in text.lines().enumerate() {
            let (n, line) = (n + 1, line.split('#').next().unwrap_or("").trim()); // `# ...` is a comment
            if line.is_empty() {
                continue;
            }
            let (action, keys) = line.split_once('=').ok_or_else(|| format!("line {n}: expected `Button = Key`"))?;
            let action = action.trim();
            // `Pad A = South` sets the controller; `A = Z` the keyboard.
            let (pad, name) = match action.get(..4) {
                Some(p) if p.eq_ignore_ascii_case("pad ") => (true, action[4..].trim()),
                _ => (false, action),
            };
            let Some(a) = ACTIONS.iter().position(|x| x.eq_ignore_ascii_case(name)) else { continue };
            if std::mem::replace(&mut seen[pad as usize][a], true) {
                return Err(format!("line {n}: `{}{}` is listed twice", if pad { "Pad " } else { "" }, ACTIONS[a]));
            }
            let entries = keys.split(',').map(str::trim).filter(|k| !k.is_empty());
            if pad {
                out.pad[a] = entries
                    .map(|k| pad_index(k).ok_or_else(|| format!("line {n}: unknown controller button `{k}` (names: {})", PAD.join(", "))))
                    .collect::<Result<_, _>>()?;
            } else {
                out.keys[a] = entries
                    .map(|k| key_index(k).ok_or_else(|| format!("line {n}: unknown key `{k}` (key names are listed in README.md)")))
                    .collect::<Result<_, _>>()?;
            }
        }
        Ok(out)
    }

    pub fn to_text(&self) -> String {
        let mut s = String::from(
            "# MKDSrust controls: one line per DS button, `Button = Key`.\n\
             # Several keys for one button: `A = Z, Space`. No key: `Select =`.\n\
             # Key names: A-Z, Key0-Key9, F1-F15, Up, Down, Left, Right, Space, Enter, Escape, Tab, Backspace,\n\
             # LeftShift, RightShift, LeftCtrl, RightCtrl, LeftAlt, RightAlt, NumPad0-NumPad9 (full list in README.md).\n\
             # Edit this file, or run mkds.bat and choose \"Change controls\". Delete it to get the defaults back.\n\n",
        );
        let section = |s: &mut String, prefix: &str, lists: &[Vec<usize>; 13], names: &[&str]| {
            for (a, list) in lists.iter().enumerate() {
                let entries: Vec<&str> = list.iter().map(|&k| names[k]).collect();
                let line = format!("{prefix}{} = {}", ACTIONS[a], entries.join(", "));
                if ROLES[a].is_empty() {
                    *s += &format!("{}\n", line.trim_end());
                } else {
                    *s += &format!("{:<28}# {}\n", line, ROLES[a]);
                }
            }
        };
        section(&mut s, "", &self.keys, &KEYS);
        s += "\n# Game controller (Xbox, PlayStation, Switch Pro and others): `Pad Button = Name`.\n\
              # Names: South East North West (face buttons by position: South is Xbox A), LB RB LT RT, Start Select,\n\
              # DPadUp DPadDown DPadLeft DPadRight, StickUp StickDown StickLeft StickRight (left stick), LeftStick\n\
              # RightStick (pressed in), Guide.\n";
        section(&mut s, "Pad ", &self.pad, &PAD);
        s
    }

    /// Reads `path`, writing the defaults there first when it does not exist yet.
    pub fn load_or_create(path: &std::path::Path) -> Result<Self, String> {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::parse(&text).map_err(|e| format!("{}: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let controls = Self::default();
                controls.save(path)?;
                Ok(controls)
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

    /// The controller buttons of `action` for display, e.g. `South, RT` or `(none)`.
    pub fn describe_pad(&self, action: usize) -> String {
        if self.pad[action].is_empty() {
            return "(none)".into();
        }
        self.pad[action].iter().map(|&k| PAD[k]).collect::<Vec<_>>().join(", ")
    }

    /// The keys of `action` for display, e.g. `Z, Space` or `(none)`.
    pub fn describe(&self, action: usize) -> String {
        if self.keys[action].is_empty() {
            return "(none)".into();
        }
        self.keys[action].iter().map(|&k| KEYS[k]).collect::<Vec<_>>().join(", ")
    }
}

/// Shared MKDSrust data directory. Legacy data is copied without replacing newer files.
pub fn default_path() -> Option<PathBuf> {
    static PATH: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    PATH.get_or_init(migrated_default_path).clone()
}

fn migrated_default_path() -> Option<PathBuf> {
    let env = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from);
    let dir = if cfg!(windows) {
        env("APPDATA")?.join("MKDS Native Builder")
    } else if cfg!(target_os = "macos") {
        env("HOME")?.join("Library/Application Support/MKDS Native Builder")
    } else {
        env("XDG_CONFIG_HOME").or_else(|| env("HOME").map(|h| h.join(".config")))?.join("mkds-native-builder")
    };
    let current = dir.with_file_name("MKDSrust");
    // Keep the originals intact, including build output paths stored in launcher.json.
    // Fall back to the legacy directory on failure so existing data stays usable.
    if migrate_data(&dir, &current).is_err() { return Some(dir.join("controls.txt")); }
    let alternate = dir.with_file_name(if cfg!(windows) || cfg!(target_os = "macos") { "mkds-native-builder" } else { "MKDS Native Builder" });
    if migrate_data(&alternate, &current).is_err() { return Some(alternate.join("controls.txt")); }
    Some(current.join("controls.txt"))
}

pub fn migrate_data(old: &std::path::Path, new: &std::path::Path) -> std::io::Result<()> {
    if !old.is_dir() { return Ok(()); }
    std::fs::create_dir_all(new)?;
    for entry in std::fs::read_dir(old)? {
        let entry = entry?;
        let target = new.join(entry.file_name());
        let kind = entry.file_type()?;
        if kind.is_dir() { migrate_data(&entry.path(), &target)?; }
        else if kind.is_file() && !target.exists() { std::fs::copy(entry.path(), target)?; }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migration_preserves_old_data_and_newer_preferences() {
        let root = tempfile::tempdir().unwrap();
        let old = root.path().join("old"); let new = root.path().join("MKDSrust");
        std::fs::create_dir_all(old.join("toolchains/rust")).unwrap();
        std::fs::create_dir_all(&new).unwrap();
        std::fs::write(old.join("controls.txt"), "old").unwrap();
        std::fs::write(new.join("controls.txt"), "new").unwrap();
        std::fs::write(old.join("toolchains/rust/marker"), "tool").unwrap();
        migrate_data(&old,&new).unwrap(); migrate_data(&old,&new).unwrap();
        assert_eq!(std::fs::read_to_string(new.join("controls.txt")).unwrap(), "new");
        assert!(old.join("controls.txt").is_file());
        assert!(new.join("toolchains/rust/marker").is_file());
    }

    #[test]
    fn defaults_round_trip_through_the_file() {
        let c = Controls::default();
        assert_eq!(c.describe(9), "A"); // L = keyboard A uses items
        assert_eq!(Controls::parse(&c.to_text()).unwrap(), c);
    }

    #[test]
    fn edits_are_case_insensitive_and_allow_several_or_no_keys() {
        let c = Controls::parse("# mine\na = space, z\nselect =\nl=q\n").unwrap();
        assert_eq!(c.describe(0), "Space, Z");
        assert_eq!(c.describe(2), "(none)");
        assert_eq!(c.describe(9), "Q");
        assert_eq!(c.describe(1), "X", "missing lines keep their default");
        assert_eq!(Controls::parse("Start = 5").unwrap().describe(3), "Key5");
    }

    #[test]
    fn mistakes_name_the_line() {
        for (text, expect) in [

            ("A = Zed", "line 1: unknown key `Zed`"),
            ("A = Z\na = X", "line 2: `A` is listed twice"),
            ("Pad A = South\npad a = East", "line 2: `Pad A` is listed twice"),
            ("Pad A = Triangle", "line 1: unknown controller button `Triangle`"),
            ("A Z", "line 1: expected"),
        ] {
            let e = Controls::parse(text).unwrap_err();
            assert!(e.starts_with(expect), "{e}");
        }
    }

    #[test]
    fn controller_lines_are_separate_and_default_when_absent() {
        let c = Controls::parse("A = Q\nPad L = RT, LB\n").unwrap();
        assert_eq!(c.describe(0), "Q");
        assert_eq!(c.describe_pad(0), "South, RT", "keyboard edits leave the controller alone");
        assert_eq!(c.describe_pad(9), "RT, LB");
        let old_file = Controls::parse("A = Z\nL = A\n").unwrap();
        assert_eq!(old_file.pad, Controls::default().pad, "files from before controller support keep the defaults");
        assert!(Controls::default().to_text().contains("Pad A = South, RT"));
    }

    #[test]
    fn missing_file_is_created_with_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub/controls.txt");
        assert_eq!(Controls::load_or_create(&path).unwrap(), Controls::default());
        assert!(std::fs::read_to_string(&path).unwrap().contains("L = A"));
    }
}
