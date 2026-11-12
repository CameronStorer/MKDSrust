//! Mario Kart DS save files (the 256 KiB cartridge backup image): decoding, a progress summary, re-encoding,
//! "unlock all" for testing, and importing saves from emulators and flashcarts.
//!
//! The game stores its data in blocks, most of them twice (a primary copy and a backup copy). Each block is
//! scrambled with four XOR key tables and ends in a 4-byte tail: two "tick" bytes (random, ignored by the check)
//! and a big-endian CRC-16 chosen so that the CRC over the whole block, with the tick bytes zeroed, is 0. The first
//! word of each block, once unscrambled, is a magic number naming the block. The key tables and the CRC
//! polynomial are read from the player's own ROM at run time; nothing from the game is stored in this file.
//!
//! Layout research: the MIT-licensed reference decompilation of MKDS (tylzars/mkds) and our dictionary
//! (`dictionary/game_names.csv`, `save_*`). The code here is our own.
use std::path::{Path, PathBuf};

/// Size of the MKDS backup memory (256 KiB flash).
pub const SAVE_SIZE: usize = 0x40000;

/// ARM9 addresses (Mario Kart DS USA, revision 0) of the scramble key tables and the CRC polynomial.
const KEY_TABLES: [(u32, usize); 4] = [(0x0215_4694, 29), (0x0215_4648, 19), (0x0215_45D8, 11), (0x0215_4604, 17)];
const CRC_POLY_ADDR: u32 = 0x0215_4708;
/// The 8 unscrambled bytes in front of the profile block ("MKDSSV10").
const PROFILE_PREFIX: &[u8; 8] = b"MKDSSV10";

/// The scramble keys and CRC polynomial, read from the player's ROM.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SaveKeys {
    tables: [Vec<u32>; 4],
    crc_poly: u16,
}

impl SaveKeys {
    /// Reads the keys from a Mario Kart DS (USA) ROM image.
    pub fn from_rom(rom: &[u8]) -> Result<Self, String> {
        if rom.get(0x0C..0x10) != Some(b"AMCE") {
            return Err("Save tools need your Mario Kart DS (USA) ROM.".into());
        }
        let sections = crate::extract::arm9_sections(rom).map_err(|e| e.to_string())?;
        let read = |addr: u32, len: usize| -> Result<&[u8], String> {
            sections
                .iter()
                .find_map(|s| {
                    let off = addr.checked_sub(s.ram_address)? as usize;
                    s.data.get(off..off + len)
                })
                .ok_or_else(|| format!("The ROM's ARM9 has no data at {addr:#010x}."))
        };
        let mut tables: [Vec<u32>; 4] = Default::default();
        for (table, &(addr, count)) in tables.iter_mut().zip(&KEY_TABLES) {
            *table = read(addr, count * 4)?.chunks_exact(4).map(|c| u32::from_le_bytes(c.try_into().unwrap())).collect();
        }
        let poly = read(CRC_POLY_ADDR, 2)?;
        let crc_poly = u16::from_le_bytes([poly[0], poly[1]]);
        if crc_poly == 0 || tables.iter().any(|t| t.iter().all(|&w| w == 0)) {
            return Err("The ROM's save keys look wrong (is it a Mario Kart DS USA revision 0 dump?).".into());
        }
        Ok(Self { tables, crc_poly })
    }

    pub fn from_rom_file(path: &Path) -> Result<Self, String> {
        let rom = std::fs::read(path).map_err(|e| format!("Cannot read {}: {e}", path.display()))?;
        Self::from_rom(&rom)
    }

    fn key(&self, i: usize, j: usize, k: usize, l: usize) -> u32 {
        self.tables[0][i].wrapping_add(self.tables[1][j]).wrapping_add(self.tables[2][k]).wrapping_add(self.tables[3][l])
    }

    /// The first-table index whose stream turns `first` into `magic`, as the game searches it.
    fn find_index(&self, first: u32, magic: u32) -> Option<usize> {
        (0..self.tables[0].len()).find(|&i| first ^ self.key(i, 0, 0, 0) == magic)
    }

    /// XORs `bytes` (whole words) with the key stream that starts at first-table index `start`. The same call
    /// scrambles and unscrambles.
    fn xor(&self, bytes: &mut [u8], start: usize) {
        let lens = [self.tables[0].len(), self.tables[1].len(), self.tables[2].len(), self.tables[3].len()];
        let (mut i, mut j, mut k, mut l) = (start, 0, 0, 0);
        for word in bytes.chunks_exact_mut(4) {
            let v = u32::from_le_bytes((&*word).try_into().unwrap()) ^ self.key(i, j, k, l);
            word.copy_from_slice(&v.to_le_bytes());
            k += 1;
            if k == lens[2] {
                k = 0;
                l += 1;
                if l == lens[3] {
                    l = 0;
                    j += 1;
                    if j == lens[1] {
                        j = 0;
                        i = (i + 1) % lens[0];
                    }
                }
            }
        }
    }

    /// The game's CRC-16: bits enter MSB first at the bottom, the polynomial is applied on carry out of the top.
    fn crc(&self, bytes: &[u8]) -> u16 {
        let mut crc = 0u16;
        for &byte in bytes {
            for bit in (0..8).rev() {
                crc = if crc & 0x8000 != 0 { (crc << 1) ^ self.crc_poly } else { crc << 1 };
                crc ^= u16::from(byte >> bit & 1);
            }
        }
        crc
    }
}

/// The save blocks the game reads at start-up (each stored twice).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockKind {
    Profile,
    Emblem,
    GrandPrix,
    TimeTrial,
    Missions,
    Friends,
}

#[derive(Clone, Copy, Debug)]
pub struct BlockSpec {
    pub kind: BlockKind,
    pub name: &'static str,
    pub magic: u32,
    pub offsets: [usize; 2],
    pub size: usize,
    /// Starts with the unscrambled `MKDSSV10` prefix; the magic is the word after it.
    prefix: bool,
}

const fn magic(tag: &[u8; 4]) -> u32 {
    u32::from_le_bytes(*tag)
}

pub const BLOCKS: [BlockSpec; 6] = [
    BlockSpec { kind: BlockKind::Profile, name: "Profile and unlocks", magic: magic(b"NKSY"), offsets: [0x0000, 0x30F00], size: 0x100, prefix: true },
    BlockSpec { kind: BlockKind::Emblem, name: "Emblem", magic: magic(b"NKEM"), offsets: [0x0100, 0x31000], size: 0x300, prefix: false },
    BlockSpec { kind: BlockKind::GrandPrix, name: "Grand Prix records", magic: magic(b"NKGP"), offsets: [0x0400, 0x31300], size: 0x400, prefix: false },
    BlockSpec { kind: BlockKind::TimeTrial, name: "Time trial records", magic: magic(b"NKTA"), offsets: [0x0800, 0x31700], size: 0x1600, prefix: false },
    BlockSpec { kind: BlockKind::Missions, name: "Mission results", magic: magic(b"NKMR"), offsets: [0x1E00, 0x32D00], size: 0x100, prefix: false },
    BlockSpec { kind: BlockKind::Friends, name: "Friend roster", magic: magic(b"NKFL"), offsets: [0x1F00, 0x32E00], size: 0xC00, prefix: false },
];

pub fn spec(kind: BlockKind) -> &'static BlockSpec {
    BLOCKS.iter().find(|b| b.kind == kind).unwrap()
}

/// One unscrambled, CRC-checked block copy. `data` keeps the stored tick bytes, so encoding it with the same
/// `xor_index` gives back the exact original bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Block {
    pub data: Vec<u8>,
    pub xor_index: usize,
}

/// What a block copy holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CopyState {
    Valid(Block),
    /// Never written (erased flash or zeros): the game starts that block fresh.
    Empty,
    /// Written but unreadable (wrong magic or CRC).
    Damaged,
}

impl CopyState {
    pub fn block(&self) -> Option<&Block> {
        match self {
            CopyState::Valid(b) => Some(b),
            _ => None,
        }
    }
}

impl SaveKeys {
    /// Decodes and checks one copy of a block, as the game does when it loads.
    pub fn decode_copy(&self, save: &[u8], spec: &BlockSpec, copy: usize) -> CopyState {
        let off = spec.offsets[copy];
        let Some(raw) = save.get(off..off + spec.size) else { return CopyState::Damaged };
        if raw.iter().all(|&b| b == 0xFF) || raw.iter().all(|&b| b == 0) {
            return CopyState::Empty;
        }
        let start = if spec.prefix { 8 } else { 0 };
        if spec.prefix && &raw[..8] != PROFILE_PREFIX {
            return CopyState::Damaged;
        }
        let mut data = raw.to_vec();
        let first = u32::from_le_bytes(data[start..start + 4].try_into().unwrap());
        let Some(xor_index) = self.find_index(first, spec.magic) else { return CopyState::Damaged };
        self.xor(&mut data[start..], xor_index);
        let mut check = data.clone();
        let tail = check.len() - 4;
        check[tail] = 0;
        check[tail + 1] = 0;
        if self.crc(&check) != 0 {
            return CopyState::Damaged;
        }
        CopyState::Valid(Block { data, xor_index })
    }

    /// Scrambles a block for storage, refreshing its CRC (the tick bytes in `block.data` are kept).
    pub fn encode(&self, spec: &BlockSpec, block: &Block) -> Vec<u8> {
        assert_eq!(block.data.len(), spec.size);
        let mut out = block.data.clone();
        let start = if spec.prefix { 8 } else { 0 };
        if spec.prefix {
            out[..8].copy_from_slice(PROFILE_PREFIX);
        }
        out[start..start + 4].copy_from_slice(&spec.magic.to_le_bytes());
        let tail = out.len() - 4;
        let tick = [out[tail], out[tail + 1]];
        out[tail..].fill(0);
        let crc = self.crc(&out);
        out[tail..tail + 2].copy_from_slice(&tick);
        out[tail + 2..].copy_from_slice(&crc.to_be_bytes());
        self.xor(&mut out[start..], block.xor_index);
        out
    }

    /// The copy the game uses: the primary when it is valid, else the backup.
    pub fn current(&self, save: &[u8], spec: &BlockSpec) -> Option<Block> {
        [0, 1].into_iter().find_map(|c| self.decode_copy(save, spec, c).block().cloned())
    }

    /// Writes `block` to both copies (as the game does when it saves).
    pub fn store(&self, save: &mut [u8], spec: &BlockSpec, block: &Block) {
        let bytes = self.encode(spec, block);
        for off in spec.offsets {
            save[off..off + spec.size].copy_from_slice(&bytes);
        }
    }
}

pub const CLASSES: [&str; 4] = ["50cc", "100cc", "150cc", "Mirror"];
pub const CUPS: [&str; 8] = ["Mushroom", "Flower", "Star", "Special", "Shell", "Banana", "Leaf", "Lightning"];
pub const CHARACTERS: [&str; 13] = [
    "Mario", "Donkey Kong", "Toad", "Bowser", "Peach", "Wario", "Yoshi", "Luigi", "Dry Bones", "Daisy", "Waluigi",
    "R.O.B.", "Shy Guy",
];
/// Grand Prix and mission grades, worst to best.
pub const GRADES: [&str; 8] = ["E", "D", "C", "B", "A", "1 star", "2 stars", "3 stars"];
pub const TROPHIES: [&str; 3] = ["Gold", "Silver", "Bronze"];

/// Profile flag bits (the 32-bit word at offset 0x20 of the profile block).
pub mod flag {
    /// Cup unlocks, one bit per class (50cc, 100cc, 150cc, Mirror): Star cup (gold in Mushroom and Flower)…
    pub const STAR_CUP: u32 = 0;
    /// …Special cup (gold in Star)…
    pub const SPECIAL_CUP: u32 = 4;
    /// …Leaf cup (gold in Shell and Banana)…
    pub const LEAF_CUP: u32 = 8;
    /// …and Lightning cup (gold in Leaf).
    pub const LIGHTNING_CUP: u32 = 12;
    /// Mirror class (gold in 150cc Special and Lightning).
    pub const MIRROR: u32 = 16;
    /// Kart unlock stages 2, 3 and 4 (100cc Special; 150cc Special or Lightning; Mirror Special and Lightning).
    pub const KART_STAGE_2: u32 = 17;
    /// Secret characters: Dry Bones (50cc Special), Daisy (50cc Lightning), Waluigi (100cc Lightning),
    /// R.O.B. (Mirror Special or Lightning). Character index 8 + n uses bit 20 + n.
    pub const DRY_BONES: u32 = 20;
    /// Staff roll seen (lets the credits be skipped), three bits. Not unlocks.
    pub const CREDITS_SEEN: u32 = 24;
}

/// Everything `unlock_all` sets: every cup in every class, Mirror, all kart stages, all four secret characters.
pub const UNLOCK_ALL_FLAGS: u32 = 0x00FF_FFFF;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GrandPrixResult {
    pub class: usize,
    pub cup: usize,
    /// 0 gold, 1 silver, 2 bronze; higher means no trophy.
    pub trophy: u8,
    /// Index into [`GRADES`].
    pub grade: u8,
    pub points: u8,
    pub character: u8,
    pub kart: u8,
    pub time: (u8, u8, u16),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockReport {
    pub name: &'static str,
    pub primary: &'static str,
    pub backup: &'static str,
}

/// A plain description of a save's progress.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Summary {
    pub player_name: String,
    pub flags: u32,
    pub blocks: Vec<BlockReport>,
    pub grand_prix: Vec<GrandPrixResult>,
    /// (level 1-7, mission 1-9, grade index) for every mission played.
    pub missions: Vec<(u8, u8, u8)>,
    /// Courses (of 32) with a time trial record.
    pub time_trial_courses: usize,
    pub has_emblem: bool,
}

impl Summary {
    pub fn flag(&self, bit: u32) -> bool {
        self.flags >> bit & 1 != 0
    }

    /// Cups the player can race in a class (Mushroom, Flower, Shell and Banana are always open).
    pub fn cup_unlocked(&self, class: usize, cup: usize) -> bool {
        match cup {
            2 => self.flag(flag::STAR_CUP + class as u32),
            3 => self.flag(flag::SPECIAL_CUP + class as u32),
            6 => self.flag(flag::LEAF_CUP + class as u32),
            7 => self.flag(flag::LIGHTNING_CUP + class as u32),
            _ => true,
        }
    }

    pub fn mirror(&self) -> bool {
        self.flag(flag::MIRROR)
    }

    /// Kart unlock stage 1-4 (4: every kart).
    pub fn kart_stage(&self) -> u8 {
        (1..=3).rev().find(|&n| self.flag(flag::KART_STAGE_2 + n - 1)).map_or(1, |n| n as u8 + 1)
    }

    /// Secret characters unlocked.
    pub fn characters(&self) -> Vec<&'static str> {
        (0..4).filter(|&n| self.flag(flag::DRY_BONES + n)).map(|n| CHARACTERS[8 + n as usize]).collect()
    }

    pub fn golds(&self) -> usize {
        self.grand_prix.iter().filter(|r| r.trophy == 0).count()
    }

    /// Short plain-language lines for the launcher.
    pub fn lines(&self) -> Vec<String> {
        let mut out = Vec::new();
        out.push(format!("Player: {}", if self.player_name.is_empty() { "(no name yet)" } else { &self.player_name }));
        out.push(format!("Grand Prix: {} of 32 cups finished, {} gold", self.grand_prix.len(), self.golds()));
        for r in &self.grand_prix {
            let trophy = TROPHIES.get(r.trophy as usize).copied().unwrap_or("no trophy");
            let grade = GRADES.get(r.grade as usize).copied().unwrap_or("?");
            out.push(format!(
                "    {} {} cup: {trophy}, grade {grade}, {} points ({})",
                CLASSES[r.class],
                CUPS[r.cup],
                r.points,
                CHARACTERS.get(r.character as usize).copied().unwrap_or("?")
            ));
        }
        let locked: Vec<String> = (0..4)
            .flat_map(|c| [2, 3, 6, 7].into_iter().map(move |cup| (c, cup)))
            .filter(|&(c, cup)| !self.cup_unlocked(c, cup))
            .map(|(c, cup)| format!("{} {}", CLASSES[c], CUPS[cup]))
            .collect();
        out.push(if locked.is_empty() { "Cups: all unlocked".into() } else { format!("Cups: {} of 32 unlocked", 32 - locked.len()) });
        out.push(format!("Mirror class: {}", if self.mirror() { "unlocked" } else { "locked" }));
        let chars = self.characters();
        out.push(format!(
            "Secret characters: {}",
            if chars.is_empty() { "none yet".to_string() } else { chars.join(", ") }
        ));
        out.push(format!("Karts: unlock stage {} of 4", self.kart_stage()));
        out.push(format!("Missions: {} of 63 played", self.missions.len()));
        out.push(format!("Time trials: records on {} of 32 courses", self.time_trial_courses));
        let damaged: Vec<_> = self.blocks.iter().filter(|b| b.primary == "damaged" && b.backup == "damaged").map(|b| b.name).collect();
        if !damaged.is_empty() {
            out.push(format!("Unreadable (the game will reset them): {}", damaged.join(", ")));
        }
        out
    }
}

fn word(data: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(data[off..off + 4].try_into().unwrap())
}

impl SaveKeys {
    /// Summarises a save. Fails when the profile block cannot be read in either copy (not an MKDS save, or a
    /// save from a different game).
    pub fn summary(&self, save: &[u8]) -> Result<Summary, String> {
        if save.len() < SAVE_SIZE {
            return Err(format!("A Mario Kart DS save is {SAVE_SIZE} bytes; this one has {}.", save.len()));
        }
        let state = |s: &CopyState| match s {
            CopyState::Valid(_) => "ok",
            CopyState::Empty => "empty",
            CopyState::Damaged => "damaged",
        };
        let blocks = BLOCKS
            .iter()
            .map(|b| BlockReport { name: b.name, primary: state(&self.decode_copy(save, b, 0)), backup: state(&self.decode_copy(save, b, 1)) })
            .collect();
        let profile = self
            .current(save, spec(BlockKind::Profile))
            .ok_or("This is not a Mario Kart DS save (or it is damaged): its profile block does not check out.")?;
        let name: Vec<u16> = profile.data[0x0C..0x20].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).take_while(|&c| c != 0).collect();
        let mut grand_prix = Vec::new();
        if let Some(gp) = self.current(save, spec(BlockKind::GrandPrix)) {
            for class in 0..4 {
                for cup in 0..8 {
                    let r = &gp.data[4 + (class * 8 + cup) * 0x1C..][..0x1C];
                    if r[0] & 1 == 0 {
                        continue;
                    }
                    let t = word(r, 4);
                    grand_prix.push(GrandPrixResult {
                        class,
                        cup,
                        trophy: r[4],
                        grade: r[3],
                        points: r[2],
                        character: r[0] >> 1,
                        kart: r[1],
                        time: ((t >> 8 & 0x7F) as u8, (t >> 15 & 0x7F) as u8, (t >> 22 & 0x3FF) as u16),
                    });
                }
            }
        }
        let missions = self
            .current(save, spec(BlockKind::Missions))
            .map(|m| {
                (0..63)
                    .filter(|&i| m.data[4 + i] & 1 != 0)
                    .map(|i| ((i / 9 + 1) as u8, (i % 9 + 1) as u8, m.data[4 + i] >> 1))
                    .collect()
            })
            .unwrap_or_default();
        let time_trial_courses = self
            .current(save, spec(BlockKind::TimeTrial))
            .map(|t| (0..32).filter(|&c| t.data[4 + c * 0x1C] & 1 != 0).count())
            .unwrap_or(0);
        let has_emblem = self.current(save, spec(BlockKind::Emblem)).is_some_and(|e| e.data[4] & 1 != 0);
        Ok(Summary {
            player_name: String::from_utf16_lossy(&name),
            flags: word(&profile.data, 0x20),
            blocks,
            grand_prix,
            missions,
            time_trial_courses,
            has_emblem,
        })
    }

    /// A copy of `save` with every unlock set: all cups in every class, the Mirror class, every kart and the four
    /// secret characters (profile flag bits 0-23, [`UNLOCK_ALL_FLAGS`]). No records are added or changed: Grand
    /// Prix trophies, mission results and times stay what the player earned. (50cc, 100cc and 150cc are open from
    /// the start; Shy Guy is not unlocked by the single-player save; credits-seen bits are left alone.)
    pub fn unlock_all(&self, save: &[u8]) -> Result<Vec<u8>, String> {
        self.summary(save)?;
        let spec = spec(BlockKind::Profile);
        let mut block = self.current(save, spec).unwrap();
        let flags = word(&block.data, 0x20) | UNLOCK_ALL_FLAGS;
        block.data[0x20..0x24].copy_from_slice(&flags.to_le_bytes());
        let mut out = save.to_vec();
        self.store(&mut out, spec, &block);
        Ok(out)
    }
}

/// Turns an imported file into a 256 KiB backup image: raw `.sav` (melonDS, flashcarts, MKDSrust), DeSmuME `.dsv`
/// (its footer stripped), larger dumps (the game only uses the first 256 KiB) and short dumps (padded with erased
/// flash, 0xFF).
pub fn normalize_import(bytes: &[u8]) -> Result<Vec<u8>, String> {
    if bytes.starts_with(b"NocashGbaBackupMediaSavDataFile") {
        return Err("This is a No$GBA save. Export it as a raw .sav first.".into());
    }
    let mut data = bytes;
    const DSV_END: &[u8] = b"|-DESMUME SAVE-|";
    const DSV_SNIP: &[u8] = b"|<--Snip above here to create a raw sav by excluding this DeSmuME savedata footer:";
    if data.ends_with(DSV_END) {
        let at = data.windows(DSV_SNIP.len()).rposition(|w| w == DSV_SNIP).ok_or("This DeSmuME save's footer is malformed.")?;
        data = &data[..at];
    }
    // The last block the game reads two copies of ends at 0x33A00; anything shorter cannot be an MKDS save.
    if data.len() < 0x33A00 {
        return Err(format!("This file is too small to be a Mario Kart DS save ({} bytes).", data.len()));
    }
    let mut out = data[..data.len().min(SAVE_SIZE)].to_vec();
    out.resize(SAVE_SIZE, 0xFF);
    Ok(out)
}

/// The launcher's save slots for a ROM, in the shared `saves/` folder.
pub fn save_path(saves: &Path, rom_sha1: &str) -> PathBuf {
    saves.join(format!("{}.sav", rom_sha1.to_ascii_lowercase()))
}

/// The separate "unlock all" test save, so the real save is never touched.
pub fn test_save_path(saves: &Path, rom_sha1: &str) -> PathBuf {
    saves.join(format!("{}.unlocked.sav", rom_sha1.to_ascii_lowercase()))
}

fn timestamp() -> String {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0) as i64;
    let (days, rest) = (secs.div_euclid(86400), secs.rem_euclid(86400));
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}_{:02}{:02}{:02}", rest / 3600, rest / 60 % 60, rest % 60)
}

/// Writes `bytes` to `path` through a temporary file, so a failure never leaves half a save.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let dir = path.parent().ok_or("Invalid save path")?;
    std::fs::create_dir_all(dir).map_err(|e| format!("Cannot create {}: {e}", dir.display()))?;
    let mut temp = tempfile::NamedTempFile::new_in(dir).map_err(|e| e.to_string())?;
    std::io::Write::write_all(&mut temp, bytes).map_err(|e| e.to_string())?;
    temp.as_file().sync_all().map_err(|e| e.to_string())?;
    temp.persist(path).map_err(|e| format!("Cannot write {}: {e}", path.display()))?;
    Ok(())
}

/// Copies `path` (when it exists) to `saves/backup/<timestamp>.sav` and returns the copy's path.
pub fn backup(saves: &Path, path: &Path) -> Result<Option<PathBuf>, String> {
    if !path.is_file() {
        return Ok(None);
    }
    let dir = saves.join("backup");
    std::fs::create_dir_all(&dir).map_err(|e| format!("Cannot create {}: {e}", dir.display()))?;
    let stamp = timestamp();
    let target = (1..)
        .map(|n| dir.join(if n == 1 { format!("{stamp}.sav") } else { format!("{stamp} ({n}).sav") }))
        .find(|p| !p.exists())
        .unwrap();
    std::fs::copy(path, &target).map_err(|e| format!("Cannot back up {}: {e}", path.display()))?;
    Ok(Some(target))
}

/// Reads and checks a save file to import. Returns the 256 KiB image and its summary.
pub fn read_import(keys: &SaveKeys, file: &Path) -> Result<(Vec<u8>, Summary), String> {
    let bytes = std::fs::read(file).map_err(|e| format!("Cannot read {}: {e}", file.display()))?;
    let save = normalize_import(&bytes)?;
    let summary = keys.summary(&save)?;
    Ok((save, summary))
}

/// Installs an already checked save image as the player's save, backing up the current one first. Returns the
/// backup's path (none when there was no save yet).
pub fn install(saves: &Path, rom_sha1: &str, save: &[u8]) -> Result<Option<PathBuf>, String> {
    let path = save_path(saves, rom_sha1);
    let backup = backup(saves, &path)?;
    write_atomic(&path, save)?;
    Ok(backup)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Synthetic keys for tests that need no ROM (the real tables come from the player's ROM).
    fn fake_keys() -> SaveKeys {
        let mut x = 0x1234_5678u32;
        let mut next = || {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x
        };
        let tables = [29, 19, 11, 17].map(|n| (0..n).map(|_| next()).collect());
        SaveKeys { tables, crc_poly: 0x1021 }
    }

    fn blank_save(keys: &SaveKeys) -> Vec<u8> {
        let mut save = vec![0xFF; SAVE_SIZE];
        for (n, spec) in BLOCKS.iter().enumerate() {
            let mut data = vec![0u8; spec.size];
            data[spec.size - 4] = 0x5A;
            data[spec.size - 3] = n as u8;
            keys.store(&mut save, spec, &Block { data, xor_index: 3 + n });
        }
        save
    }

    #[test]
    fn crc_of_block_with_its_tail_is_zero() {
        let keys = fake_keys();
        let save = blank_save(&keys);
        for spec in &BLOCKS {
            for copy in 0..2 {
                let CopyState::Valid(block) = keys.decode_copy(&save, spec, copy) else { panic!("{}", spec.name) };
                assert_eq!(keys.encode(spec, &block), save[spec.offsets[copy]..][..spec.size]);
            }
        }
        let mut broken = save.clone();
        broken[0x400 + 0x40] ^= 1;
        assert_eq!(keys.decode_copy(&broken, spec(BlockKind::GrandPrix), 0), CopyState::Damaged);
        assert!(keys.current(&broken, spec(BlockKind::GrandPrix)).is_some(), "the backup copy is used");
        assert_eq!(keys.decode_copy(&vec![0xFF; SAVE_SIZE], spec(BlockKind::Profile), 0), CopyState::Empty);
    }

    #[test]
    fn unlock_all_sets_unlocks_only() {
        let keys = fake_keys();
        let save = blank_save(&keys);
        let before = keys.summary(&save).unwrap();
        assert!(!before.mirror() && before.characters().is_empty() && before.kart_stage() == 1);
        let unlocked = keys.unlock_all(&save).unwrap();
        let after = keys.summary(&unlocked).unwrap();
        assert!(after.mirror());
        assert_eq!(after.kart_stage(), 4);
        assert_eq!(after.characters(), ["Dry Bones", "Daisy", "Waluigi", "R.O.B."]);
        assert!((0..4).all(|c| (0..8).all(|cup| after.cup_unlocked(c, cup))));
        assert_eq!(after.grand_prix, before.grand_prix);
        assert!(after.blocks.iter().all(|b| b.primary == "ok" && b.backup == "ok"));
        // Only the profile block changed.
        let p = spec(BlockKind::Profile);
        for (i, (a, b)) in save.iter().zip(&unlocked).enumerate() {
            if a != b {
                assert!(p.offsets.iter().any(|&o| (o..o + p.size).contains(&i)), "{i:#x}");
            }
        }
    }

    #[test]
    fn imports_dsv_and_odd_sizes() {
        let keys = fake_keys();
        let save = blank_save(&keys);
        let mut dsv = save.clone();
        dsv.extend_from_slice(b"|<--Snip above here to create a raw sav by excluding this DeSmuME savedata footer:");
        for v in [SAVE_SIZE as u32, SAVE_SIZE as u32, 2, 3, SAVE_SIZE as u32, 0] {
            dsv.extend_from_slice(&v.to_le_bytes());
        }
        dsv.extend_from_slice(b"|-DESMUME SAVE-|");
        assert_eq!(normalize_import(&dsv).unwrap(), save);
        let mut big = save.clone();
        big.extend(std::iter::repeat_n(0xFF, SAVE_SIZE));
        assert_eq!(normalize_import(&big).unwrap(), save);
        assert_eq!(normalize_import(&save[..0x34000]).unwrap(), save);
        assert!(normalize_import(&save[..0x1000]).is_err());
        assert!(keys.summary(&vec![0xFF; SAVE_SIZE]).is_err(), "an empty image is not an MKDS save");
    }

    #[test]
    fn install_backs_up_the_previous_save() {
        let dir = tempfile::tempdir().unwrap();
        let sha = "691E00D9A5DD80B04F80CC7559503E8B06848785";
        assert_eq!(install(dir.path(), sha, &[1; 4]).unwrap(), None);
        let backup = install(dir.path(), sha, &[2; 4]).unwrap().unwrap();
        assert_eq!(std::fs::read(&backup).unwrap(), [1; 4]);
        assert!(backup.starts_with(dir.path().join("backup")));
        assert_eq!(std::fs::read(save_path(dir.path(), sha)).unwrap(), [2; 4]);
        let second = install(dir.path(), sha, &[3; 4]).unwrap().unwrap();
        assert_ne!(second, backup);
        assert!(timestamp().len() == 17 && timestamp().starts_with("20"));
    }

    /// Real saves and the real ROM, when this machine has them (`MKDS_TEST_ROM`, `MKDS_TEST_SAVES` = folder).
    /// Never committed: they are the player's own files.
    #[test]
    fn real_saves_round_trip_and_unlock() {
        let (Some(rom), Some(dir)) = (std::env::var_os("MKDS_TEST_ROM"), std::env::var_os("MKDS_TEST_SAVES")) else {
            eprintln!("skipped: set MKDS_TEST_ROM and MKDS_TEST_SAVES");
            return;
        };
        let keys = SaveKeys::from_rom_file(Path::new(&rom)).unwrap();
        let mut seen = 0;
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            let Ok(save) = std::fs::read(&path) else { continue };
            if save.len() != SAVE_SIZE {
                continue;
            }
            seen += 1;
            let summary = keys.summary(&save).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            eprintln!("== {}\n{}\nflags {:#010x}", path.display(), summary.lines().join("\n"), summary.flags);
            let mut copy = save.clone();
            for spec in &BLOCKS {
                for c in 0..2 {
                    if let CopyState::Valid(b) = keys.decode_copy(&save, spec, c) {
                        let off = spec.offsets[c];
                        copy[off..off + spec.size].copy_from_slice(&keys.encode(spec, &b));
                    }
                }
            }
            assert_eq!(copy, save, "{}: decode then encode is lossless", path.display());
            let unlocked = keys.unlock_all(&save).unwrap();
            let after = keys.summary(&unlocked).unwrap();
            assert!(after.mirror() && after.kart_stage() == 4 && after.characters().len() == 4);
            assert_eq!(after.grand_prix, summary.grand_prix);
            assert_eq!(after.missions, summary.missions);
            for c in 0..2 {
                assert!(matches!(keys.decode_copy(&unlocked, spec(BlockKind::Profile), c), CopyState::Valid(_)));
            }
            assert_eq!(keys.unlock_all(&unlocked).unwrap(), unlocked, "idempotent");
        }
        assert!(seen > 0);
    }
}
