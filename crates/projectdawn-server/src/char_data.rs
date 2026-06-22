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
pub const BASE_HP: f32 = 100.0;
pub const BASE_MP: f32 = 100.0;
pub const BASE_ST: f32 = 100.0;

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
    pub hp_bonus: f32,
    pub mp_bonus: f32,
    pub stamina_bonus: f32,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct LevelGain {
    pub stats: StatBlock,
    pub max_hp: f32,
    pub max_mp: f32,
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
            hp_bonus: 50.0, mp_bonus: 0.0, stamina_bonus: 20.0,
        });
        m.insert("Magician", ClassData {
            bonuses: StatBlock { intelligence: 15, wisdom: 10, ..Default::default() },
            hp_bonus: -10.0, mp_bonus: 100.0, stamina_bonus: 0.0,
        });
        m.insert("Wizard", ClassData {
            bonuses: StatBlock { intelligence: 18, wisdom: 5, ..Default::default() },
            hp_bonus: -15.0, mp_bonus: 110.0, stamina_bonus: 0.0,
        });
        m.insert("Sorcerer", ClassData {
            bonuses: StatBlock { intelligence: 10, charisma: 15, wisdom: 5, ..Default::default() },
            hp_bonus: -5.0, mp_bonus: 90.0, stamina_bonus: 0.0,
        });
        m.insert("Rogue", ClassData {
            bonuses: StatBlock { dexterity: 15, agility: 10, ..Default::default() },
            hp_bonus: 20.0, mp_bonus: 0.0, stamina_bonus: 20.0,
        });
        m.insert("Cleric", ClassData {
            bonuses: StatBlock { wisdom: 12, constitution: 8, ..Default::default() },
            hp_bonus: 30.0, mp_bonus: 60.0, stamina_bonus: 0.0,
        });
        m.insert("Druid", ClassData {
            bonuses: StatBlock { wisdom: 10, intelligence: 8, ..Default::default() },
            hp_bonus: 10.0, mp_bonus: 70.0, stamina_bonus: 0.0,
        });
        m.insert("Shaman", ClassData {
            bonuses: StatBlock { wisdom: 10, constitution: 8, strength: 5, ..Default::default() },
            hp_bonus: 30.0, mp_bonus: 40.0, stamina_bonus: 15.0,
        });
        m.insert("Beast Master", ClassData {
            bonuses: StatBlock { strength: 8, constitution: 8, agility: 5, wisdom: 5, ..Default::default() },
            hp_bonus: 30.0, mp_bonus: 30.0, stamina_bonus: 20.0,
        });
        m.insert("Blood Mage", ClassData {
            bonuses: StatBlock { intelligence: 15, constitution: 5, ..Default::default() },
            hp_bonus: 0.0, mp_bonus: 80.0, stamina_bonus: 0.0,
        });
        m.insert("Paladin", ClassData {
            bonuses: StatBlock { strength: 10, wisdom: 8, constitution: 5, ..Default::default() },
            hp_bonus: 40.0, mp_bonus: 30.0, stamina_bonus: 15.0,
        });
        m.insert("Shadow Knight", ClassData {
            bonuses: StatBlock { strength: 10, intelligence: 8, constitution: 5, ..Default::default() },
            hp_bonus: 35.0, mp_bonus: 30.0, stamina_bonus: 10.0,
        });
        m.insert("Necromancer", ClassData {
            bonuses: StatBlock { intelligence: 15, wisdom: 5, ..Default::default() },
            hp_bonus: -10.0, mp_bonus: 100.0, stamina_bonus: 0.0,
        });
        m.insert("Enchanter", ClassData {
            bonuses: StatBlock { intelligence: 12, charisma: 12, wisdom: 5, ..Default::default() },
            hp_bonus: -10.0, mp_bonus: 90.0, stamina_bonus: 0.0,
        });
        m.insert("Bard", ClassData {
            bonuses: StatBlock { dexterity: 8, charisma: 12, agility: 5, ..Default::default() },
            hp_bonus: 20.0, mp_bonus: 30.0, stamina_bonus: 20.0,
        });
        m.insert("Ranger", ClassData {
            bonuses: StatBlock { dexterity: 12, agility: 8, wisdom: 5, ..Default::default() },
            hp_bonus: 20.0, mp_bonus: 20.0, stamina_bonus: 20.0,
        });
        m.insert("Monk", ClassData {
            bonuses: StatBlock { strength: 8, dexterity: 8, agility: 8, ..Default::default() },
            hp_bonus: 25.0, mp_bonus: 10.0, stamina_bonus: 30.0,
        });
        m.insert("Witch Hunter", ClassData {
            bonuses: StatBlock { intelligence: 8, constitution: 8, wisdom: 8, ..Default::default() },
            hp_bonus: 20.0, mp_bonus: 40.0, stamina_bonus: 10.0,
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
            max_hp: 20.0, max_mp: 2.0, max_stamina: 8.0,
        });
        m.insert("Magician", LevelGain {
            stats: StatBlock { intelligence: 3, wisdom: 2, ..Default::default() },
            max_hp: 5.0, max_mp: 25.0, max_stamina: 2.0,
        });
        m.insert("Wizard", LevelGain {
            stats: StatBlock { intelligence: 4, wisdom: 1, ..Default::default() },
            max_hp: 3.0, max_mp: 28.0, max_stamina: 1.0,
        });
        m.insert("Sorcerer", LevelGain {
            stats: StatBlock { charisma: 2, intelligence: 2, ..Default::default() },
            max_hp: 5.0, max_mp: 22.0, max_stamina: 2.0,
        });
        m.insert("Rogue", LevelGain {
            stats: StatBlock { dexterity: 2, agility: 2, ..Default::default() },
            max_hp: 12.0, max_mp: 3.0, max_stamina: 10.0,
        });
        m.insert("Cleric", LevelGain {
            stats: StatBlock { wisdom: 3, constitution: 1, ..Default::default() },
            max_hp: 12.0, max_mp: 20.0, max_stamina: 2.0,
        });
        m.insert("Druid", LevelGain {
            stats: StatBlock { wisdom: 2, intelligence: 2, ..Default::default() },
            max_hp: 8.0, max_mp: 22.0, max_stamina: 2.0,
        });
        m.insert("Shaman", LevelGain {
            stats: StatBlock { wisdom: 2, constitution: 1, strength: 1, ..Default::default() },
            max_hp: 12.0, max_mp: 15.0, max_stamina: 5.0,
        });
        m.insert("Beast Master", LevelGain {
            stats: StatBlock { strength: 2, constitution: 1, agility: 1, wisdom: 1, ..Default::default() },
            max_hp: 14.0, max_mp: 10.0, max_stamina: 6.0,
        });
        m.insert("Blood Mage", LevelGain {
            stats: StatBlock { intelligence: 3, constitution: 1, ..Default::default() },
            max_hp: 7.0, max_mp: 22.0, max_stamina: 2.0,
        });
        m.insert("Paladin", LevelGain {
            stats: StatBlock { strength: 2, wisdom: 2, constitution: 1, ..Default::default() },
            max_hp: 15.0, max_mp: 12.0, max_stamina: 5.0,
        });
        m.insert("Shadow Knight", LevelGain {
            stats: StatBlock { strength: 2, intelligence: 2, constitution: 1, ..Default::default() },
            max_hp: 14.0, max_mp: 12.0, max_stamina: 4.0,
        });
        m.insert("Necromancer", LevelGain {
            stats: StatBlock { intelligence: 3, wisdom: 2, ..Default::default() },
            max_hp: 3.0, max_mp: 25.0, max_stamina: 1.0,
        });
        m.insert("Enchanter", LevelGain {
            stats: StatBlock { intelligence: 3, charisma: 2, ..Default::default() },
            max_hp: 3.0, max_mp: 23.0, max_stamina: 1.0,
        });
        m.insert("Bard", LevelGain {
            stats: StatBlock { dexterity: 2, charisma: 2, agility: 1, ..Default::default() },
            max_hp: 10.0, max_mp: 8.0, max_stamina: 8.0,
        });
        m.insert("Ranger", LevelGain {
            stats: StatBlock { dexterity: 2, agility: 2, wisdom: 1, ..Default::default() },
            max_hp: 10.0, max_mp: 8.0, max_stamina: 8.0,
        });
        m.insert("Monk", LevelGain {
            stats: StatBlock { strength: 2, dexterity: 2, agility: 1, ..Default::default() },
            max_hp: 12.0, max_mp: 4.0, max_stamina: 10.0,
        });
        m.insert("Witch Hunter", LevelGain {
            stats: StatBlock { intelligence: 2, wisdom: 2, constitution: 1, ..Default::default() },
            max_hp: 10.0, max_mp: 15.0, max_stamina: 4.0,
        });
        m
    })
}

fn default_level_gain() -> LevelGain {
    LevelGain {
        stats: StatBlock::default(),
        max_hp: 10.0,
        max_mp: 10.0,
        max_stamina: 5.0,
    }
}

/// Mirror of `PlayerStats.apply_character(race, class, level)`. Returns the
/// stat block + max-resource snapshot the server caches on `PerConnection`
/// (and writes to the characters row at create time). Unknown races / classes
/// fall back to the all-10 BASE block with no class bonus — defensive, since
/// `create_character` already validates the inputs.
pub fn compute(race: &str, class: &str, level: i32) -> ComputedCharacter {
    let mut stats = StatBlock::flat(BASE);
    if let Some(r) = races().get(race) {
        stats = stats.add(*r);
    }
    let cls = classes().get(class).copied().unwrap_or_default();
    stats = stats.add(cls.bonuses);

    let con_hp_bonus = (stats.constitution as f32 - 10.0) * 5.0;
    let mut max_hp = (BASE_HP + cls.hp_bonus + con_hp_bonus).max(50.0);
    let mut max_mp = (BASE_MP + cls.mp_bonus).max(20.0);
    let mut max_stamina = (BASE_ST + cls.stamina_bonus).max(20.0);

    let gain = level_gains().get(class).copied().unwrap_or_else(default_level_gain);
    let lvl = level.clamp(1, 99);
    for _ in 1..lvl {
        let con_before = stats.constitution;
        stats = stats.add(gain.stats);
        max_hp += gain.max_hp;
        max_mp += gain.max_mp;
        max_stamina += gain.max_stamina;
        max_hp += (stats.constitution - con_before) as f32 * 5.0;
    }

    let mut xp_to_next: i32 = 100;
    for _ in 1..lvl {
        xp_to_next = ((xp_to_next as f32) * 1.5) as i32;
    }

    ComputedCharacter { stats, max_hp, max_mp, max_stamina, xp_to_next }
}

/// The XP needed to clear `level` (the size of that level's band). Depends
/// only on the level, not race/class. Mirrors the geometric 1.5x growth in
/// `compute` and the GDScript `apply_character` loop. Used by the leveling
/// path (`world::progression`) to resize the band on a level up/down without
/// recomputing the whole character.
pub fn xp_to_next_for(level: i32) -> i32 {
    let lvl = level.clamp(1, 99);
    let mut xp_to_next: i32 = 100;
    for _ in 1..lvl {
        xp_to_next = ((xp_to_next as f32) * 1.5) as i32;
    }
    xp_to_next
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
        // max_hp: BASE_HP(100) + Warrior(50) + con_bonus(20-10)*5 = 200
        assert!((c.max_hp - 200.0).abs() < 0.01, "max_hp = {}", c.max_hp);
        // max_mp: BASE_MP(100) + Warrior(0) = 100
        assert!((c.max_mp - 100.0).abs() < 0.01);
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
        // max_hp: 100 + 30 + (13-10)*5 = 145
        assert!((c.max_hp - 145.0).abs() < 0.01, "max_hp = {}", c.max_hp);
    }

    #[test]
    fn warrior_level_5_includes_level_gains() {
        let c1 = compute("Human", "Warrior", 1);
        let c5 = compute("Human", "Warrior", 5);
        // 4 level-ups × +2 STR each = +8 over level 1
        assert_eq!(c5.stats.strength, c1.stats.strength + 8);
        // 4 level-ups × +20 max_hp + 4 × con-gain×5 (=2 con × 5 = 10)
        // = 80 + 40 = 120 over level 1
        assert!((c5.max_hp - c1.max_hp - 120.0).abs() < 0.01);
    }

    #[test]
    fn xp_to_next_grows_geometrically() {
        let c1 = compute("Human", "Warrior", 1);
        let c2 = compute("Human", "Warrior", 2);
        let c3 = compute("Human", "Warrior", 3);
        assert_eq!(c1.xp_to_next, 100);
        assert_eq!(c2.xp_to_next, 150);
        assert_eq!(c3.xp_to_next, 225);
    }

    #[test]
    fn unknown_inputs_fall_back_safely() {
        let c = compute("Goblin", "Cardsharp", 3);
        // All-10 base, no class bonuses applied. Level 3 with default
        // gain (no stat stats, +10 hp per level).
        assert_eq!(c.stats.strength, 10);
        assert!((c.max_hp - 120.0).abs() < 0.01); // 100 + 2*10
    }
}
