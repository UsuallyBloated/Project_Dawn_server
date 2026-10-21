//! Track 6 sub-task 2 — Rust mirror of GDScript `data/character_data.gd`.
//!
//! Stat tables for race / class / level. Used at character creation to
//! populate the flat base_* columns on the characters row, and read at
//! `load_character` time so the damage formula has authoritative stats
//! without a follow-up DB sync from the client.
//!
//! When the GDScript constants change, mirror here in the same commit —
//! the unit tests below assert a couple of known combinations against
//! the GDScript baseline to flag drift.

use std::collections::HashMap;
use std::sync::OnceLock;

pub const BASE: i32 = 10;
pub const BASE_ST: f32 = 100.0;
/// The pure melee classes have no mana in EQ; they keep a token pool here so
/// nothing that divides by a maximum (bars, regen) meets a zero.
pub const MIN_MANA: f32 = 20.0;

#[derive(Debug, Clone, Copy, Default)]
pub struct StatBlock {
    pub strength: i32,
    pub dexterity: i32,
    pub agility: i32,
    pub intelligence: i32,
    pub wisdom: i32,
    pub charisma: i32,
    pub constitution: i32,
}

impl StatBlock {
    pub const fn flat(v: i32) -> Self {
        Self {
            strength: v, dexterity: v, agility: v, intelligence: v,
            wisdom: v, charisma: v, constitution: v,
        }
    }

    pub fn add(self, other: Self) -> Self {
        Self {
            strength: self.strength + other.strength,
            dexterity: self.dexterity + other.dexterity,
            agility: self.agility + other.agility,
            intelligence: self.intelligence + other.intelligence,
            wisdom: self.wisdom + other.wisdom,
            charisma: self.charisma + other.charisma,
            constitution: self.constitution + other.constitution,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ClassData {
    pub bonuses: StatBlock,
    pub stamina_bonus: f32,
}

/// What a level adds: the class's stat growth and its stamina. Hit points
/// and mana are not gains; they are recomputed from the level and the stats
/// by EQ's formulas (`base_hp`, `base_mana`).
#[derive(Debug, Clone, Copy, Default)]
pub struct LevelGain {
    pub stats: StatBlock,
    pub max_stamina: f32,
}

#[derive(Debug, Clone, Copy)]
pub struct ComputedCharacter {
    pub stats: StatBlock,
    pub max_hp: f32,
    pub max_mp: f32,
    pub max_stamina: f32,
    pub xp_to_next: i32,
}

fn races() -> &'static HashMap<&'static str, StatBlock> {
    static RACES: OnceLock<HashMap<&'static str, StatBlock>> = OnceLock::new();
    RACES.get_or_init(|| {
        let mut m = HashMap::new();
        m.insert("Human", StatBlock {
            strength: 2, dexterity: 2, agility: 2, intelligence: 2,
            wisdom: 2, charisma: 2, constitution: 2,
        });
        m.insert("Elf", StatBlock {
            dexterity: 10, agility: 10, intelligence: 10, wisdom: 5,
            strength: -5, constitution: -5, ..Default::default()
        });
        m.insert("Dark Elf", StatBlock {
            intelligence: 15, dexterity: 10, agility: 5, wisdom: -5,
            charisma: -10, ..Default::default()
        });
        m.insert("Gnome", StatBlock {
            intelligence: 15, wisdom: 5, strength: -5, constitution: -5,
            ..Default::default()
        });
        m.insert("Halfling", StatBlock {
            dexterity: 10, agility: 10, charisma: 5, strength: -5,
            constitution: -5, ..Default::default()
        });
        m.insert("Dwarf", StatBlock {
            constitution: 15, strength: 5, wisdom: 5, charisma: -5,
            agility: -5, ..Default::default()
        });
        m.insert("Wood Elf", StatBlock {
            dexterity: 10, agility: 10, wisdom: 5, intelligence: -5,
            charisma: -5, ..Default::default()
        });
        m.insert("Half-Elf", StatBlock {
            dexterity: 5, agility: 5, intelligence: 5, wisdom: 5,
            ..Default::default()
        });
        m.insert("Ogre", StatBlock {
            strength: 20, constitution: 10, charisma: -15, intelligence: -5,
            wisdom: -5, ..Default::default()
        });
        m.insert("Troll", StatBlock {
            constitution: 20, strength: 10, charisma: -15, wisdom: -10,
            intelligence: -5, ..Default::default()
        });
        m.insert("Kel`varath", StatBlock {
            constitution: 15, strength: 8, charisma: -15, wisdom: -5,
            ..Default::default()
        });
        m.insert("Minotaur", StatBlock {
            strength: 20, constitution: 10, charisma: -10, intelligence: -8,
            agility: -5, ..Default::default()
        });
        m.insert("Revenant", StatBlock {
            intelligence: 10, constitution: 10, strength: 5, charisma: -15,
            wisdom: -5, ..Default::default()
        });
        m.insert("Fae", StatBlock {
            intelligence: 20, wisdom: 10, agility: 15, charisma: 5,
            strength: -15, constitution: -10, ..Default::default()
        });
        m.insert("Felhari", StatBlock {
            dexterity: 12, agility: 12, constitution: 5, wisdom: -5,
            intelligence: -5, charisma: -5, ..Default::default()
        });
        m.insert("Kobold", StatBlock {
            dexterity: 10, intelligence: 8, agility: 5, strength: -8,
            constitution: -5, charisma: -10, ..Default::default()
        });
        m.insert("Half-Ogre", StatBlock {
            strength: 15, constitution: 8, charisma: -8, intelligence: -3,
            wisdom: -3, ..Default::default()
        });
        m
    })
}

fn classes() -> &'static HashMap<&'static str, ClassData> {
    static CLASSES: OnceLock<HashMap<&'static str, ClassData>> = OnceLock::new();
    CLASSES.get_or_init(|| {
        let mut m = HashMap::new();
        m.insert("Warrior", ClassData {
            bonuses: StatBlock { strength: 10, constitution: 8, ..Default::default() },
            stamina_bonus: 20.0,
        });
        m.insert("Magician", ClassData {
            bonuses: StatBlock { intelligence: 15, wisdom: 10, ..Default::default() },
            stamina_bonus: 0.0,
        });
        m.insert("Wizard", ClassData {
            bonuses: StatBlock { intelligence: 18, wisdom: 5, ..Default::default() },
            stamina_bonus: 0.0,
        });
        m.insert("Sorcerer", ClassData {
            bonuses: StatBlock { intelligence: 10, charisma: 15, wisdom: 5, ..Default::default() },
            stamina_bonus: 0.0,
        });
        m.insert("Rogue", ClassData {
            bonuses: StatBlock { dexterity: 15, agility: 10, ..Default::default() },
            stamina_bonus: 20.0,
        });
        m.insert("Cleric", ClassData {
            bonuses: StatBlock { wisdom: 12, constitution: 8, ..Default::default() },
            stamina_bonus: 0.0,
        });
        m.insert("Druid", ClassData {
            bonuses: StatBlock { wisdom: 10, intelligence: 8, ..Default::default() },
            stamina_bonus: 0.0,
        });
        m.insert("Shaman", ClassData {
            bonuses: StatBlock { wisdom: 10, constitution: 8, strength: 5, ..Default::default() },
            stamina_bonus: 15.0,
        });
        m.insert("Beast Master", ClassData {
            bonuses: StatBlock { strength: 8, constitution: 8, agility: 5, wisdom: 5, ..Default::default() },
            stamina_bonus: 20.0,
        });
        m.insert("Blood Mage", ClassData {
            bonuses: StatBlock { intelligence: 15, constitution: 5, ..Default::default() },
            stamina_bonus: 0.0,
        });
        m.insert("Paladin", ClassData {
            bonuses: StatBlock { strength: 10, wisdom: 8, constitution: 5, ..Default::default() },
            stamina_bonus: 15.0,
        });
        m.insert("Shadow Knight", ClassData {
            bonuses: StatBlock { strength: 10, intelligence: 8, constitution: 5, ..Default::default() },
            stamina_bonus: 10.0,
        });
        m.insert("Necromancer", ClassData {
            bonuses: StatBlock { intelligence: 15, wisdom: 5, ..Default::default() },
            stamina_bonus: 0.0,
        });
        m.insert("Enchanter", ClassData {
            bonuses: StatBlock { intelligence: 12, charisma: 12, wisdom: 5, ..Default::default() },
            stamina_bonus: 0.0,
        });
        m.insert("Bard", ClassData {
            bonuses: StatBlock { dexterity: 8, charisma: 12, agility: 5, ..Default::default() },
            stamina_bonus: 20.0,
        });
        m.insert("Ranger", ClassData {
            bonuses: StatBlock { dexterity: 12, agility: 8, wisdom: 5, ..Default::default() },
            stamina_bonus: 20.0,
        });
        m.insert("Monk", ClassData {
            bonuses: StatBlock { strength: 8, dexterity: 8, agility: 8, ..Default::default() },
            stamina_bonus: 30.0,
        });
        m.insert("Witch Hunter", ClassData {
            bonuses: StatBlock { intelligence: 8, constitution: 8, wisdom: 8, ..Default::default() },
            stamina_bonus: 10.0,
        });
        m
    })
}

fn level_gains() -> &'static HashMap<&'static str, LevelGain> {
    static GAINS: OnceLock<HashMap<&'static str, LevelGain>> = OnceLock::new();
    GAINS.get_or_init(|| {
        let mut m = HashMap::new();
        m.insert("Warrior", LevelGain {
            stats: StatBlock { strength: 2, constitution: 2, ..Default::default() },
            max_stamina: 8.0,
        });
        m.insert("Magician", LevelGain {
            stats: StatBlock { intelligence: 3, wisdom: 2, ..Default::default() },
            max_stamina: 2.0,
        });
        m.insert("Wizard", LevelGain {
            stats: StatBlock { intelligence: 4, wisdom: 1, ..Default::default() },
            max_stamina: 1.0,
        });
        m.insert("Sorcerer", LevelGain {
            stats: StatBlock { charisma: 2, intelligence: 2, ..Default::default() },
            max_stamina: 2.0,
        });
        m.insert("Rogue", LevelGain {
            stats: StatBlock { dexterity: 2, agility: 2, ..Default::default() },
            max_stamina: 10.0,
        });
        m.insert("Cleric", LevelGain {
            stats: StatBlock { wisdom: 3, constitution: 1, ..Default::default() },
            max_stamina: 2.0,
        });
        m.insert("Druid", LevelGain {
            stats: StatBlock { wisdom: 2, intelligence: 2, ..Default::default() },
            max_stamina: 2.0,
        });
        m.insert("Shaman", LevelGain {
            stats: StatBlock { wisdom: 2, constitution: 1, strength: 1, ..Default::default() },
            max_stamina: 5.0,
        });
        m.insert("Beast Master", LevelGain {
            stats: StatBlock { strength: 2, constitution: 1, agility: 1, wisdom: 1, ..Default::default() },
            max_stamina: 6.0,
        });
        m.insert("Blood Mage", LevelGain {
            stats: StatBlock { intelligence: 3, constitution: 1, ..Default::default() },
            max_stamina: 2.0,
        });
        m.insert("Paladin", LevelGain {
            stats: StatBlock { strength: 2, wisdom: 2, constitution: 1, ..Default::default() },
            max_stamina: 5.0,
        });
        m.insert("Shadow Knight", LevelGain {
            stats: StatBlock { strength: 2, intelligence: 2, constitution: 1, ..Default::default() },
            max_stamina: 4.0,
        });
        m.insert("Necromancer", LevelGain {
            stats: StatBlock { intelligence: 3, wisdom: 2, ..Default::default() },
            max_stamina: 1.0,
        });
        m.insert("Enchanter", LevelGain {
            stats: StatBlock { intelligence: 3, charisma: 2, ..Default::default() },
            max_stamina: 1.0,
        });
        m.insert("Bard", LevelGain {
            stats: StatBlock { dexterity: 2, charisma: 2, agility: 1, ..Default::default() },
            max_stamina: 8.0,
        });
        m.insert("Ranger", LevelGain {
            stats: StatBlock { dexterity: 2, agility: 2, wisdom: 1, ..Default::default() },
            max_stamina: 8.0,
        });
        m.insert("Monk", LevelGain {
            stats: StatBlock { strength: 2, dexterity: 2, agility: 1, ..Default::default() },
            max_stamina: 10.0,
        });
        m.insert("Witch Hunter", LevelGain {
            stats: StatBlock { intelligence: 2, wisdom: 2, constitution: 1, ..Default::default() },
            max_stamina: 4.0,
        });
        m
    })
}

fn default_level_gain() -> LevelGain {
    LevelGain {
        stats: StatBlock::default(),
        max_stamina: 5.0,
    }
}

/// EverQuest's class factor for hit points at a level (EQEmu's
/// `Mob::GetClassLevelFactor`, the `lm` of the classic HP formula). Our own
/// classes take the EQ class they are shaped after: Sorcerer and Blood Mage
/// the pure casters', Beast Master the Beastlord's, Witch Hunter the
/// Ranger's; anything unknown gets EQEmu's default band.
pub fn hp_level_factor(class: &str, level: i32) -> i32 {
    match class {
        "Warrior" => match level {
            l if l < 20 => 220,
            l if l < 30 => 230,
            l if l < 40 => 250,
            l if l < 53 => 270,
            l if l < 57 => 280,
            l if l < 60 => 290,
            l if l < 70 => 300,
            _ => 311,
        },
        "Cleric" | "Druid" | "Shaman" => if level < 70 { 150 } else { 157 },
        "Paladin" | "Shadow Knight" => match level {
            l if l < 35 => 210,
            l if l < 45 => 220,
            l if l < 51 => 230,
            l if l < 56 => 240,
            l if l < 60 => 250,
            l if l < 68 => 260,
            _ => 270,
        },
        "Monk" | "Bard" | "Rogue" | "Beast Master" => match level {
            l if l < 51 => 180,
            l if l < 58 => 190,
            l if l < 70 => 200,
            _ => 210,
        },
        "Ranger" | "Witch Hunter" => match level {
            l if l < 58 => 200,
            l if l < 70 => 210,
            _ => 220,
        },
        "Magician" | "Wizard" | "Necromancer" | "Enchanter" | "Sorcerer" | "Blood Mage" => {
            if level < 70 { 120 } else { 127 }
        }
        _ => match level {
            l if l < 35 => 210,
            l if l < 45 => 220,
            l if l < 51 => 230,
            l if l < 56 => 240,
            l if l < 60 => 250,
            _ => 260,
        },
    }
}

/// The stat a class builds its mana on (EQEmu's `GetCasterClass`): INT for
/// the arcane classes and the Bard, WIS for the priests and their hybrids,
/// none for the pure melee. Witch Hunter, ours, casts on INT.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManaStat {
    Int,
    Wis,
}

pub fn mana_stat(class: &str) -> Option<ManaStat> {
    match class {
        "Wizard" | "Magician" | "Necromancer" | "Enchanter" | "Shadow Knight" | "Bard"
        | "Sorcerer" | "Blood Mage" | "Witch Hunter" => Some(ManaStat::Int),
        "Cleric" | "Druid" | "Shaman" | "Paladin" | "Ranger" | "Beast Master" => Some(ManaStat::Wis),
        _ => None,
    }
}

/// Classic EverQuest's base hit points (EQEmu's `Client::CalcBaseHP`, the
/// pre-SoF branch): `5 + level x factor / 10 + STA x level x factor / 3000`
/// in integer arithmetic, with our CON in STA's place. EQ's STA ran 75 to
/// 255 and never grew with level; ours starts near 20 and grows, so the
/// stat term is small at level 1 and the level term carries the curve (a
/// Human Warrior 28 at 1, about 1,890 at 50; EQ's 32 and 1,692). The EQ
/// parity audit of 2026-10-20, recommendation 1.
pub fn base_hp(class: &str, level: i32, constitution: i32) -> i32 {
    let lm = hp_level_factor(class, level);
    let con = constitution.max(0);
    (5 + level * lm / 10 + con * level * lm / 3000).max(1)
}

/// Classic EverQuest's base mana (EQEmu's `Client::CalcBaseMana`, the
/// classic branch) from the class's mana stat: `22.5 x level` at a stat of
/// 100, less below it, more above, with the top end flattened past 199.
/// Integer arithmetic in EQEmu's order. Zero for a class with no mana stat.
pub fn base_mana(class: &str, level: i32, intelligence: i32, wisdom: i32) -> i32 {
    let stat = match mana_stat(class) {
        Some(ManaStat::Int) => intelligence,
        Some(ManaStat::Wis) => wisdom,
        None => return 0,
    };
    let stat = stat.max(0);
    let lesser = ((stat - 199) / 2).max(0);
    let mind = stat - lesser;
    if stat > 100 {
        (5 * (mind + 20)) / 2 * 3 * level / 40
    } else {
        (5 * (mind + 200)) / 2 * 3 * level / 100
    }
}

/// Mirror of `PlayerStats.apply_character(race, class, level)`. Returns the
/// stat block + max-resource snapshot the server caches on `PerConnection`
/// (and writes to the characters row at create time). Unknown races / classes
/// fall back to the all-10 BASE block with no class bonus — defensive, since
/// `create_character` already validates the inputs.
/// The races a character may be CREATED as: the client's creation list
/// (`data/character_data.gd`), and nothing else. `Revenant` has a stat block
/// below because the transformation needs one, but it is not a birth race;
/// `db::create_character` refuses anything off this list, so a modified
/// launcher cannot pick a block the creation screen never offers (the review
/// of 2026-10-13 found the Revenant's starvation immunity reachable that way).
pub const CREATION_RACES: [&str; 16] = [
    "Human", "Elf", "Dark Elf", "Wood Elf", "Gnome", "Halfling", "Dwarf", "Half-Elf",
    "Ogre", "Troll", "Kel`varath", "Minotaur", "Fae", "Felhari", "Kobold", "Half-Ogre",
];

/// The classes a character may be created as (the eighteen; Assassin and
/// Warlock are planned, not here).
pub const PLAYABLE_CLASSES: [&str; 18] = [
    "Warrior", "Paladin", "Shadow Knight", "Cleric", "Druid", "Shaman", "Beast Master",
    "Rogue", "Monk", "Ranger", "Witch Hunter", "Bard", "Magician", "Wizard", "Sorcerer",
    "Enchanter", "Necromancer", "Blood Mage",
];

pub fn is_creation_race(race: &str) -> bool {
    CREATION_RACES.contains(&race)
}

pub fn is_playable_class(class: &str) -> bool {
    PLAYABLE_CLASSES.contains(&class)
}

pub fn compute(race: &str, class: &str, level: i32) -> ComputedCharacter {
    let mut stats = StatBlock::flat(BASE);
    if let Some(r) = races().get(race) {
        stats = stats.add(*r);
    }
    let cls = classes().get(class).copied().unwrap_or_default();
    stats = stats.add(cls.bonuses);

    let mut max_stamina = (BASE_ST + cls.stamina_bonus).max(20.0);

    let gain = level_gains().get(class).copied().unwrap_or_else(default_level_gain);
    let lvl = level.clamp(1, 99);
    for _ in 1..lvl {
        stats = stats.add(gain.stats);
        max_stamina += gain.max_stamina;
    }
    // Hit points and mana are EQ's formulas over the level and the grown
    // stats, not a sum of per-level gains, so a level change recomputes them.
    let max_hp = base_hp(class, lvl, stats.constitution) as f32;
    let max_mp = match base_mana(class, lvl, stats.intelligence, stats.wisdom) {
        0 => MIN_MANA,
        mana => mana as f32,
    };

    let xp_to_next = xp_to_next_for(level);

    ComputedCharacter { stats, max_hp, max_mp, max_stamina, xp_to_next }
}

/// EverQuest "hell level" modifier. The per-level XP cost is cubic, and this
/// multiplier rises at bracket boundaries to create the classic difficulty
/// spikes (30/35/40/45, then every level 51-60 as Kunark crammed ten levels onto
/// the curve). Values are from the Project 1999 experience table (see
/// `docs/design/everquest_xp_curve_reference.md`). Mirrored in GDScript
/// `PlayerStats._hell_mod` — change both in the same commit.
fn hell_mod(level: i32) -> f64 {
    match level {
        1..=29 => 1.0,
        30..=34 => 1.1,
        35..=39 => 1.2,
        40..=44 => 1.3,
        45..=50 => 1.4,
        51 => 1.5,
        52 => 1.6,
        53 => 1.7,
        54 => 1.9,
        55 => 2.1,
        56 => 2.3,
        57 => 2.5,
        58 => 2.7,
        59 => 3.0,
        _ => 3.1, // 60 (clamped at the cap, so 60+ never differs)
    }
}

/// Cumulative XP to *complete* `level`: EverQuest's cubic curve
/// `L^3 x 1000 x hell_mod(L)` (e.g. completing level 60 totals 669,600,000).
/// `total_xp(0) == 0` so the level-1 band is well defined. f64 throughout to
/// match GDScript (whose `float` is f64) byte-for-byte after rounding.
fn total_xp(level: i32) -> f64 {
    if level < 1 {
        return 0.0;
    }
    let l = level as f64;
    l * l * l * 1000.0 * hell_mod(level)
}

/// The XP needed to clear `level` (the size of that level's band) — the cubic
/// total for this level minus the previous level's total. Depends only on the
/// level, not race/class. Mirrors the GDScript `PlayerStats._band_for`. Used by
/// the leveling path (`world::progression`) to resize the band on a level
/// up/down without recomputing the whole character. Clamped to the level cap so
/// the cubic can never overrun i32 (it would near level ~70).
pub fn xp_to_next_for(level: i32) -> i32 {
    let lvl = level.clamp(1, crate::world::skills::MAX_LEVEL);
    (total_xp(lvl) - total_xp(lvl - 1)).round() as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    // Anchor tests — keep these in lockstep with the GDScript constants.
    // If they ever drift, character creation will diverge from
    // `PlayerStats.apply_character`'s expectations.

    #[test]
    fn human_warrior_level_1() {
        let c = compute("Human", "Warrior", 1);
        // STR: BASE(10) + Human(2) + Warrior(10) = 22
        assert_eq!(c.stats.strength, 22);
        assert_eq!(c.stats.constitution, 20); // 10 + 2 + 8
        // EQ's formula: 5 + 1 x 220 / 10 + 20 x 1 x 220 / 3000 = 5 + 22 + 1
        assert!((c.max_hp - 28.0).abs() < 0.01, "max_hp = {}", c.max_hp);
        // A Warrior has no mana stat: the token minimum.
        assert!((c.max_mp - MIN_MANA).abs() < 0.01);
        // max_stamina: BASE_ST(100) + Warrior(20) = 120
        assert!((c.max_stamina - 120.0).abs() < 0.01);
    }

    #[test]
    fn elf_cleric_level_1() {
        let c = compute("Elf", "Cleric", 1);
        // WIS: 10 + 5 + 12 = 27
        assert_eq!(c.stats.wisdom, 27);
        // CON: 10 - 5 + 8 = 13
        assert_eq!(c.stats.constitution, 13);
        // 5 + 150 / 10 + 13 x 150 / 3000 = 5 + 15 + 0
        assert!((c.max_hp - 20.0).abs() < 0.01, "max_hp = {}", c.max_hp);
        // WIS 27: (5 x 227) / 2 = 567, x 3 = 1701, x 1 / 100 = 17
        assert!((c.max_mp - 17.0).abs() < 0.01, "max_mp = {}", c.max_mp);
    }

    #[test]
    fn warrior_level_5_includes_level_gains() {
        let c1 = compute("Human", "Warrior", 1);
        let c5 = compute("Human", "Warrior", 5);
        // 4 level-ups × +2 STR each = +8 over level 1
        assert_eq!(c5.stats.strength, c1.stats.strength + 8);
        // CON 28 at 5: 5 + 5 x 220 / 10 + 28 x 5 x 220 / 3000 = 5 + 110 + 10
        assert!((c5.max_hp - 125.0).abs() < 0.01, "max_hp = {}", c5.max_hp);
    }

    /// The EQ parity audit's anchors (2026-10-20): a level 1 Warrior dies to
    /// a dozen hits as on P99, level 50 lands near EQ's 1,692, and the
    /// casters start with EQ's twenty-odd mana instead of two hundred.
    #[test]
    fn pools_follow_eq_at_both_ends() {
        let w50 = compute("Human", "Warrior", 50);
        // CON 118, factor 270: 5 + 1350 + 118 x 50 x 270 / 3000 (= 531)
        assert!((w50.max_hp - 1886.0).abs() < 0.01, "max_hp = {}", w50.max_hp);
        let wiz1 = compute("Human", "Wizard", 1);
        // CON 12, factor 120: 5 + 12 + 0; INT 30: (5 x 230) / 2 x 3 / 100 = 17
        assert!((wiz1.max_hp - 17.0).abs() < 0.01, "max_hp = {}", wiz1.max_hp);
        assert!((wiz1.max_mp - 17.0).abs() < 0.01, "max_mp = {}", wiz1.max_mp);
        // At a stat of exactly 100 the classic branch is 22.5 x level.
        assert_eq!(base_mana("Wizard", 50, 100, 10), 1125);
        assert_eq!(base_mana("Cleric", 40, 10, 100), 900);
        // Past 100 the upper branch continues the line (101 -> 22.69 x level).
        assert_eq!(base_mana("Wizard", 100, 101, 10), 2265);
        // The top end flattens past 199: 250 counts as 225.
        assert_eq!(base_mana("Wizard", 1, 250, 10), (5 * 245) / 2 * 3 / 40);
        assert_eq!(base_mana("Rogue", 50, 200, 200), 0, "no mana stat");
        assert_eq!(hp_level_factor("Warrior", 19), 220);
        assert_eq!(hp_level_factor("Warrior", 50), 270);
        assert_eq!(hp_level_factor("Witch Hunter", 10), 200, "the Ranger's band");
    }

    #[test]
    fn xp_to_next_follows_cubic_curve() {
        // EQ cubic: band(L) = L^3*1000*hell(L) - (L-1)^3*1000*hell(L-1).
        // band(1)=1000, band(2)=8000-1000=7000, band(3)=27000-8000=19000.
        assert_eq!(compute("Human", "Warrior", 1).xp_to_next, 1000);
        assert_eq!(compute("Human", "Warrior", 2).xp_to_next, 7000);
        assert_eq!(compute("Human", "Warrior", 3).xp_to_next, 19000);
        // Hell level 30 and the level-60 cap match the P99 table exactly.
        assert_eq!(xp_to_next_for(30), 5_311_000); // 1.1 hell-mod kicks in
        assert_eq!(xp_to_next_for(60), 53_463_000); // 669.6M total - 616.137M
    }

    /// Every creation race and playable class has a stat block of its own,
    /// and the Revenant (a transformation with a block) is not a birth race.
    #[test]
    fn creation_lists_match_the_stat_blocks() {
        for race in CREATION_RACES {
            assert!(is_creation_race(race));
            assert!(races().contains_key(race), "{race} has no stat block");
        }
        for class in PLAYABLE_CLASSES {
            assert!(is_playable_class(class));
            assert!(classes().contains_key(class), "{class} has no class block");
        }
        assert!(races().contains_key("Revenant") && !is_creation_race("Revenant"),
            "the Revenant has a block for the transformation and is not a birth race");
        assert!(!is_creation_race("revenant") && !is_playable_class("warrior"), "exact names only");
    }

    #[test]
    fn unknown_inputs_fall_back_safely() {
        let c = compute("Goblin", "Cardsharp", 3);
        // All-10 base, no class bonuses applied; EQEmu's default HP band
        // (210 at level 3): 5 + 63 + 10 x 3 x 210 / 3000 = 70; no mana stat.
        assert_eq!(c.stats.strength, 10);
        assert!((c.max_hp - 70.0).abs() < 0.01, "max_hp = {}", c.max_hp);
        assert!((c.max_mp - MIN_MANA).abs() < 0.01);
    }
}
