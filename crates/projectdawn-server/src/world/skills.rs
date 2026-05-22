//! Track 18.1 — server-authoritative passive skill leveling.
//!
//! Ports the three GDScript autoloads (WeaponSkills, ArmorSkills,
//! CastingSkills) and their backing definition tables to Rust. The
//! tick loop calls `weapon_try_advance` / `armor_try_advance` /
//! `casting_try_advance` from the attack / cast / armor-hit paths;
//! each returns `Some(new_score)` on a successful advance, else
//! `None`. Callers fan out `ServerWorldMsg::SkillProgressUpdate` on
//! the Some path.
//!
//! Cap tables and starting values mirror
//! `data/weapon_skill_definitions.gd` / `armor_skill_definitions.gd`
//! / `casting_skill_definitions.gd`. Keep these in sync if the
//! GDScript dicts change — there's no canonical export for these yet
//! (lower volatility than items/spells, no need for TOML).

use super::connection::PerConnection;
use rand::Rng;
use std::collections::HashMap;

pub const MAX_LEVEL: i32 = 60;
const ADVANCE_CHANCE_BASE: f32 = 0.2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Skill {
    Weapon,
    Armor,
    Casting,
}

impl Skill {
    pub fn as_protocol(self) -> protocol::world::SkillKind {
        match self {
            Skill::Weapon => protocol::world::SkillKind::Weapon,
            Skill::Armor => protocol::world::SkillKind::Armor,
            Skill::Casting => protocol::world::SkillKind::Casting,
        }
    }
}

/// (class, [(skill_key, max_cap_at_level_60), ...])
type CapTable<'a> = &'a [(&'a str, &'a [(&'a str, i32)])];

const WEAPON_CAPS: CapTable<'static> = &[
    ("Warrior",       &[("1h_slashing", 250), ("2h_slashing", 250), ("1h_blunt", 250), ("2h_blunt", 250), ("piercing", 250), ("hand_to_hand", 100), ("archery",  75), ("defense", 250), ("dodge", 200), ("dual_wield", 200)]),
    ("Rogue",         &[("1h_slashing", 225), ("2h_slashing",   0), ("1h_blunt",  75), ("2h_blunt",   0), ("piercing", 250), ("hand_to_hand", 200), ("archery", 100), ("defense", 200), ("dodge", 250), ("dual_wield", 250)]),
    ("Magician",      &[("1h_slashing",   0), ("2h_slashing",   0), ("1h_blunt",  75), ("2h_blunt",   0), ("piercing",   0), ("hand_to_hand",  25), ("archery",   0), ("defense", 100), ("dodge",  75), ("dual_wield",   0)]),
    ("Wizard",        &[("1h_slashing",   0), ("2h_slashing",   0), ("1h_blunt",  75), ("2h_blunt",   0), ("piercing",   0), ("hand_to_hand",  25), ("archery",   0), ("defense", 100), ("dodge",  75), ("dual_wield",   0)]),
    ("Sorcerer",      &[("1h_slashing",   0), ("2h_slashing",   0), ("1h_blunt",  75), ("2h_blunt",   0), ("piercing",   0), ("hand_to_hand",  25), ("archery",   0), ("defense", 100), ("dodge",  75), ("dual_wield",   0)]),
    ("Necromancer",   &[("1h_slashing",   0), ("2h_slashing",   0), ("1h_blunt",  75), ("2h_blunt",   0), ("piercing",   0), ("hand_to_hand",  25), ("archery",   0), ("defense", 100), ("dodge",  75), ("dual_wield",   0)]),
    ("Enchanter",     &[("1h_slashing",   0), ("2h_slashing",   0), ("1h_blunt",  75), ("2h_blunt",   0), ("piercing",   0), ("hand_to_hand",  25), ("archery",   0), ("defense", 100), ("dodge",  75), ("dual_wield",   0)]),
    ("Blood Mage",    &[("1h_slashing",   0), ("2h_slashing",   0), ("1h_blunt", 100), ("2h_blunt",   0), ("piercing",  50), ("hand_to_hand",  50), ("archery",   0), ("defense", 100), ("dodge",  75), ("dual_wield",   0)]),
    ("Cleric",        &[("1h_slashing",   0), ("2h_slashing",   0), ("1h_blunt", 200), ("2h_blunt", 200), ("piercing",   0), ("hand_to_hand",  50), ("archery",   0), ("defense", 200), ("dodge", 150), ("dual_wield",   0)]),
    ("Druid",         &[("1h_slashing",   0), ("2h_slashing",   0), ("1h_blunt", 175), ("2h_blunt", 175), ("piercing",   0), ("hand_to_hand",  50), ("archery",  75), ("defense", 175), ("dodge", 150), ("dual_wield",   0)]),
    ("Shaman",        &[("1h_slashing", 150), ("2h_slashing",   0), ("1h_blunt", 200), ("2h_blunt", 200), ("piercing", 100), ("hand_to_hand", 100), ("archery",   0), ("defense", 200), ("dodge", 150), ("dual_wield",   0)]),
    ("Paladin",       &[("1h_slashing", 225), ("2h_slashing", 225), ("1h_blunt", 225), ("2h_blunt", 225), ("piercing", 150), ("hand_to_hand",  75), ("archery",  75), ("defense", 225), ("dodge", 175), ("dual_wield",   0)]),
    ("Shadow Knight", &[("1h_slashing", 225), ("2h_slashing", 225), ("1h_blunt", 225), ("2h_blunt", 225), ("piercing", 150), ("hand_to_hand",  75), ("archery",  50), ("defense", 225), ("dodge", 175), ("dual_wield", 150)]),
    ("Bard",          &[("1h_slashing", 200), ("2h_slashing", 150), ("1h_blunt", 175), ("2h_blunt", 150), ("piercing", 200), ("hand_to_hand", 100), ("archery",  75), ("defense", 175), ("dodge", 200), ("dual_wield", 175)]),
    ("Ranger",        &[("1h_slashing", 225), ("2h_slashing", 225), ("1h_blunt", 175), ("2h_blunt", 175), ("piercing", 225), ("hand_to_hand", 100), ("archery", 250), ("defense", 200), ("dodge", 225), ("dual_wield", 225)]),
    ("Monk",          &[("1h_slashing",  50), ("2h_slashing",   0), ("1h_blunt", 200), ("2h_blunt", 100), ("piercing",   0), ("hand_to_hand", 250), ("archery",   0), ("defense", 250), ("dodge", 250), ("dual_wield",   0)]),
    ("Witch Hunter",  &[("1h_slashing", 200), ("2h_slashing", 150), ("1h_blunt", 100), ("2h_blunt",   0), ("piercing", 225), ("hand_to_hand", 125), ("archery", 150), ("defense", 200), ("dodge", 200), ("dual_wield", 200)]),
    ("Beast Master",  &[("1h_slashing", 150), ("2h_slashing", 100), ("1h_blunt", 150), ("2h_blunt", 100), ("piercing", 125), ("hand_to_hand", 200), ("archery", 100), ("defense", 200), ("dodge", 175), ("dual_wield", 150)]),
];

const ARMOR_CAPS: CapTable<'static> = &[
    ("Warrior",       &[("cloth",  75), ("leather", 150), ("chain", 225), ("plate", 250), ("shield", 250)]),
    ("Paladin",       &[("cloth",  75), ("leather", 150), ("chain", 200), ("plate", 225), ("shield", 250)]),
    ("Shadow Knight", &[("cloth",  75), ("leather", 150), ("chain", 200), ("plate", 225), ("shield", 225)]),
    ("Cleric",        &[("cloth", 100), ("leather", 100), ("chain", 200), ("plate", 225), ("shield", 225)]),
    ("Druid",         &[("cloth", 100), ("leather", 200), ("chain", 175), ("plate",   0), ("shield", 125)]),
    ("Shaman",        &[("cloth", 100), ("leather", 175), ("chain", 225), ("plate",   0), ("shield", 175)]),
    ("Rogue",         &[("cloth", 100), ("leather", 250), ("chain", 100), ("plate",   0), ("shield", 100)]),
    ("Monk",          &[("cloth", 150), ("leather",   0), ("chain",   0), ("plate",   0), ("shield",   0)]),
    ("Ranger",        &[("cloth",  75), ("leather", 225), ("chain", 225), ("plate",   0), ("shield", 175)]),
    ("Beast Master",  &[("cloth",  75), ("leather", 200), ("chain", 200), ("plate",   0), ("shield", 150)]),
    ("Bard",          &[("cloth",  75), ("leather", 200), ("chain", 225), ("plate",   0), ("shield", 150)]),
    ("Witch Hunter",  &[("cloth",  75), ("leather", 200), ("chain", 225), ("plate",   0), ("shield", 150)]),
    ("Magician",      &[("cloth", 250), ("leather",  50), ("chain",   0), ("plate",   0), ("shield",   0)]),
    ("Wizard",        &[("cloth", 250), ("leather",  50), ("chain",   0), ("plate",   0), ("shield",   0)]),
    ("Sorcerer",      &[("cloth", 250), ("leather",  50), ("chain",   0), ("plate",   0), ("shield",   0)]),
    ("Necromancer",   &[("cloth", 250), ("leather",  50), ("chain",   0), ("plate",   0), ("shield",   0)]),
    ("Enchanter",     &[("cloth", 250), ("leather",  50), ("chain",   0), ("plate",   0), ("shield",   0)]),
    ("Blood Mage",    &[("cloth", 200), ("leather", 100), ("chain",  50), ("plate",   0), ("shield",   0)]),
];

const CASTING_CAPS: CapTable<'static> = &[
    ("Warrior",       &[("evocation",   0), ("alteration",   0), ("abjuration",   0), ("conjuration",   0), ("divination",   0), ("channeling",   0)]),
    ("Paladin",       &[("evocation", 125), ("alteration", 150), ("abjuration", 225), ("conjuration",   0), ("divination",  50), ("channeling", 225)]),
    ("Shadow Knight", &[("evocation", 125), ("alteration", 125), ("abjuration", 100), ("conjuration",   0), ("divination",  50), ("channeling", 225)]),
    ("Cleric",        &[("evocation", 150), ("alteration", 200), ("abjuration", 250), ("conjuration",  75), ("divination", 200), ("channeling", 175)]),
    ("Druid",         &[("evocation", 175), ("alteration", 225), ("abjuration", 175), ("conjuration",  75), ("divination", 175), ("channeling", 175)]),
    ("Shaman",        &[("evocation", 175), ("alteration", 250), ("abjuration", 125), ("conjuration",  75), ("divination", 150), ("channeling", 200)]),
    ("Rogue",         &[("evocation",   0), ("alteration",   0), ("abjuration",   0), ("conjuration",   0), ("divination",   0), ("channeling",   0)]),
    ("Monk",          &[("evocation",   0), ("alteration",   0), ("abjuration",   0), ("conjuration",   0), ("divination",   0), ("channeling",   0)]),
    ("Ranger",        &[("evocation", 125), ("alteration", 150), ("abjuration",  75), ("conjuration",   0), ("divination", 150), ("channeling", 200)]),
    ("Beast Master",  &[("evocation", 125), ("alteration", 150), ("abjuration", 100), ("conjuration",  75), ("divination",  75), ("channeling", 175)]),
    ("Bard",          &[("evocation", 125), ("alteration", 200), ("abjuration", 100), ("conjuration", 150), ("divination",  75), ("channeling", 200)]),
    ("Witch Hunter",  &[("evocation", 225), ("alteration", 125), ("abjuration", 100), ("conjuration",   0), ("divination",  75), ("channeling", 200)]),
    ("Magician",      &[("evocation", 225), ("alteration", 100), ("abjuration",  75), ("conjuration", 250), ("divination",  75), ("channeling", 150)]),
    ("Wizard",        &[("evocation", 250), ("alteration", 100), ("abjuration", 150), ("conjuration", 100), ("divination", 225), ("channeling", 150)]),
    ("Sorcerer",      &[("evocation", 250), ("alteration", 100), ("abjuration",  75), ("conjuration",  75), ("divination",  50), ("channeling", 150)]),
    ("Necromancer",   &[("evocation", 200), ("alteration", 175), ("abjuration",  50), ("conjuration", 225), ("divination", 100), ("channeling", 150)]),
    ("Enchanter",     &[("evocation", 150), ("alteration", 250), ("abjuration", 200), ("conjuration", 175), ("divination", 125), ("channeling", 150)]),
    ("Blood Mage",    &[("evocation", 200), ("alteration", 175), ("abjuration",  75), ("conjuration",   0), ("divination",  50), ("channeling", 175)]),
];

/// All weapon keys (10). Used by load_character to seed starting
/// values on a fresh character so the row count matches the GDScript
/// `_skills` dict populated by `initialize()`.
pub const WEAPON_KEYS: &[&str] = &[
    "1h_slashing", "2h_slashing", "1h_blunt", "2h_blunt",
    "piercing", "hand_to_hand", "archery", "defense", "dodge", "dual_wield",
];

pub const ARMOR_KEYS: &[&str] = &["cloth", "leather", "chain", "plate", "shield"];

pub const CASTING_KEYS: &[&str] = &[
    "evocation", "alteration", "abjuration", "conjuration", "divination", "channeling",
];

fn lookup_table(skill: Skill) -> CapTable<'static> {
    match skill {
        Skill::Weapon => WEAPON_CAPS,
        Skill::Armor => ARMOR_CAPS,
        Skill::Casting => CASTING_CAPS,
    }
}

fn max_cap(skill: Skill, player_class: &str, key: &str) -> i32 {
    let table = lookup_table(skill);
    let class_row = table
        .iter()
        .find(|(cls, _)| *cls == player_class)
        .or_else(|| table.iter().find(|(cls, _)| *cls == "Magician"))
        .map(|(_, entries)| *entries)
        .unwrap_or(&[]);
    class_row
        .iter()
        .find(|(k, _)| *k == key)
        .map(|(_, v)| *v)
        .unwrap_or(0)
}

/// Cap for a (class, level, skill_key). Mirrors the GDScript
/// `get_cap` math: `max(1, max_cap * level / MAX_LEVEL)` when the
/// class trains the skill (max_cap > 0); else 0.
pub fn cap_for(skill: Skill, player_class: &str, level: i32, key: &str) -> i32 {
    let mc = max_cap(skill, player_class, key);
    if mc == 0 {
        return 0;
    }
    (mc * level / MAX_LEVEL).max(1)
}

/// Starting value for a fresh character at level 1. Matches
/// `get_starting_value` in the GDScript files (cap at level 1, i.e.
/// untrained-but-allowed classes start at 1; classes that can't
/// train the skill start at 0).
pub fn starting_value(skill: Skill, player_class: &str, key: &str) -> i32 {
    cap_for(skill, player_class, 1, key)
}

fn current_score(conn: &PerConnection, skill: Skill, key: &str) -> i32 {
    let map = match skill {
        Skill::Weapon => &conn.weapon_skills,
        Skill::Armor => &conn.armor_skills,
        Skill::Casting => &conn.casting_skills,
    };
    *map.get(key).unwrap_or(&0)
}

fn set_score(conn: &mut PerConnection, skill: Skill, key: &str, score: i32) {
    let map = match skill {
        Skill::Weapon => &mut conn.weapon_skills,
        Skill::Armor => &mut conn.armor_skills,
        Skill::Casting => &mut conn.casting_skills,
    };
    map.insert(key.to_string(), score);
}

/// Roll the advance chance for a (skill, key). Returns the new score
/// when an advance landed; `None` when the roll missed or the skill
/// is at cap / not trainable. Mirrors PassiveSkillTracker.try_advance:
/// `chance = 0.2 * (1 - current/cap)`. The chance is highest at low
/// scores and decays to zero at cap.
pub fn try_advance(conn: &mut PerConnection, skill: Skill, key: &str) -> Option<i32> {
    let cap = cap_for(skill, &conn.class, conn.level, key);
    if cap == 0 {
        return None;
    }
    let current = current_score(conn, skill, key);
    if current >= cap {
        return None;
    }
    let chance = ADVANCE_CHANCE_BASE * (1.0 - current as f32 / cap as f32);
    if rand::thread_rng().gen::<f32>() < chance {
        let new_score = current + 1;
        set_score(conn, skill, key, new_score);
        conn.skills_dirty = true;
        Some(new_score)
    } else {
        None
    }
}

/// Seed the per-conn skill maps with starting values for a fresh
/// character. Existing rows from `character_skills` are merged on
/// top by the loader so a returning player keeps their progress.
pub fn seed_starting_scores(conn: &mut PerConnection) {
    for key in WEAPON_KEYS {
        let v = starting_value(Skill::Weapon, &conn.class, key);
        conn.weapon_skills.insert(key.to_string(), v);
    }
    for key in ARMOR_KEYS {
        let v = starting_value(Skill::Armor, &conn.class, key);
        conn.armor_skills.insert(key.to_string(), v);
    }
    for key in CASTING_KEYS {
        let v = starting_value(Skill::Casting, &conn.class, key);
        conn.casting_skills.insert(key.to_string(), v);
    }
}

/// Returns the casting discipline for a given spell name, falling
/// back to "evocation" when unknown. Ranks (e.g. "Fireball Rk. II")
/// strip the suffix before lookup. Mirror of `DISCIPLINE` in
/// `data/spell_definitions.gd`.
pub fn discipline_for_spell(spell_name: &str) -> &'static str {
    // Strip " Rk. II" / " Rk. III" suffixes to find the base name.
    let base = spell_name
        .strip_suffix(" Rk. III")
        .or_else(|| spell_name.strip_suffix(" Rk. II"))
        .unwrap_or(spell_name);
    DISCIPLINE_MAP
        .get()
        .and_then(|m| m.get(base).copied())
        .unwrap_or("evocation")
}

use std::sync::OnceLock;
static DISCIPLINE_MAP: OnceLock<HashMap<&'static str, &'static str>> = OnceLock::new();

fn build_discipline_map() -> HashMap<&'static str, &'static str> {
    let mut m = HashMap::new();
    let entries: &[(&str, &str)] = &[
        // Magician
        ("Fireball", "evocation"), ("Frost Bolt", "evocation"), ("Lightning Strike", "evocation"),
        ("Heal", "alteration"), ("Arcane Missile", "evocation"), ("Inferno", "evocation"),
        // Cleric
        ("Healing Light", "alteration"), ("Greater Heal", "alteration"),
        ("Smite", "evocation"), ("Divine Wrath", "evocation"),
        // Druid
        ("Thorns", "abjuration"), ("Regrowth", "alteration"), ("Wrath", "evocation"),
        ("Call Lightning", "evocation"), ("Entangle", "alteration"), ("Snare", "alteration"),
        ("Nature's Wrath", "evocation"),
        // Shaman
        ("Healing Wave", "alteration"), ("Mending", "alteration"),
        ("Spirit Bolt", "evocation"), ("Ancestral Strike", "evocation"), ("Slow", "alteration"),
        // Blood Mage
        ("Blood Bolt", "evocation"), ("Crimson Bolt", "evocation"),
        ("Life Drain", "alteration"), ("Hemorrhage", "evocation"),
        // Paladin
        ("Lay on Hands", "alteration"), ("Crusader's Mend", "alteration"),
        ("Righteous Fire", "evocation"), ("Judgment", "evocation"),
        // Shadow Knight
        ("Lifetap", "alteration"), ("Siphon", "evocation"), ("Dark Shroud", "evocation"),
        // Necromancer
        ("Summon Skeleton", "conjuration"), ("Bone Shards", "evocation"),
        ("Soul Drain", "alteration"), ("Dark Decay", "alteration"), ("Enervation", "evocation"),
        // Enchanter
        ("Spellshield", "abjuration"), ("Charm", "conjuration"), ("Color Spray", "evocation"),
        ("Mesmerize", "alteration"), ("Rune", "abjuration"), ("Cascade of Stars", "evocation"),
        // Bard
        ("Siren's Song", "conjuration"), ("Dissonance", "evocation"),
        ("Battle Hymn", "alteration"), ("Chorus of Misery", "evocation"),
        // Ranger
        ("Hunter's Mark", "evocation"), ("Nature's Cure", "alteration"),
        // Witch Hunter
        ("Witchfire", "evocation"), ("Expose", "alteration"),
        ("Rite of Warding", "alteration"), ("Banishment", "evocation"),
        // Fallen Paladin
        ("Death's Embrace", "evocation"), ("Blood Price", "alteration"),
        ("Shadow Flame", "evocation"), ("Condemnation", "evocation"),
        // Redeemed SK
        ("Sacrificial Mend", "alteration"), ("Radiant Bolt", "evocation"),
        ("Holy Mantle", "evocation"),
        // Wizard
        ("Ice Spear", "evocation"), ("Flame Wave", "evocation"),
        ("Thunder Clap", "evocation"), ("Blizzard", "evocation"),
        ("Meteor", "evocation"), ("Ice Storm", "evocation"),
        // Sorcerer
        ("Arcane Burst", "evocation"), ("Void Lance", "evocation"),
        ("Bloodfire", "evocation"), ("Tempest Bolt", "evocation"),
        ("Soul Surge", "evocation"), ("Arcane Nova", "evocation"),
        ("Bind Affinity", "alteration"),
        // Druid / Wizard ports
        ("Gate", "alteration"), ("Succor", "alteration"), ("Evacuate", "alteration"),
        ("Circle of Ardenmoor", "alteration"), ("Circle of the Savannahs", "alteration"),
        ("Circle of Fae Mere", "alteration"),
        ("Teleport: Aelindra", "alteration"), ("Teleport: Greyveil", "alteration"),
        ("Teleport: Harrowmere", "alteration"), ("Teleport: Varek's Spire", "alteration"),
        // Beast Master
        ("Spirit Mend", "alteration"), ("Feral Shriek", "evocation"),
        ("Warder's Mend", "alteration"), ("Primal Bond", "abjuration"),
        ("Spirit Strike", "evocation"),
        // Cleric (new)
        ("Bless", "alteration"), ("Valor", "alteration"),
        ("Complete Heal", "alteration"), ("Resurrection", "alteration"),
        // Shaman (new)
        ("Spirit of the Bear", "alteration"), ("Gift of Insight", "alteration"),
        ("Torpor", "alteration"),
        // Enchanter (new)
        ("Strength", "alteration"), ("Brilliance", "alteration"),
        ("Immobilize", "alteration"), ("Clarity", "alteration"),
        ("Breeze", "alteration"), ("Haste", "alteration"),
        // Druid / Shaman (new)
        ("Spirit of Wolf", "alteration"),
        // Bard (new)
        ("Selos' Melody", "alteration"), ("Anthem of the Hunt", "alteration"),
        ("Poet's Mending", "alteration"), ("Wanderer's Chord", "alteration"),
        ("Mana Weave", "conjuration"), ("Aria of Dismay", "evocation"),
        // Necromancer (new)
        ("Lich Form", "alteration"),
        // Blood Mage (new)
        ("Exsanguinate", "alteration"),
        // Druid (new)
        ("Ensnare", "alteration"),
        // Ranger (new)
        ("Ensnaring Roots", "alteration"), ("Camouflage", "alteration"),
        ("Hunter's Eye", "alteration"),
        // Witch Hunter (new)
        ("Spellbreak", "alteration"), ("Antimagic Ward", "abjuration"),
    ];
    for (k, v) in entries {
        m.insert(*k, *v);
    }
    m
}

pub fn init_discipline_map() {
    DISCIPLINE_MAP.get_or_init(build_discipline_map);
}

/// Snapshot a connection's score maps for fan-out to the client on
/// enter-world. Returns three parallel vectors (key, score) sorted by
/// key for deterministic ordering.
pub fn snapshot(conn: &PerConnection) -> (Vec<(String, u32)>, Vec<(String, u32)>, Vec<(String, u32)>) {
    let snap_map = |m: &HashMap<String, i32>| -> Vec<(String, u32)> {
        let mut v: Vec<(String, u32)> = m
            .iter()
            .map(|(k, s)| (k.clone(), (*s).max(0) as u32))
            .collect();
        v.sort_by(|a, b| a.0.cmp(&b.0));
        v
    };
    (
        snap_map(&conn.weapon_skills),
        snap_map(&conn.armor_skills),
        snap_map(&conn.casting_skills),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warrior_caps_match_gdscript() {
        assert_eq!(cap_for(Skill::Weapon, "Warrior", 60, "1h_slashing"), 250);
        assert_eq!(cap_for(Skill::Weapon, "Warrior", 1, "1h_slashing"), 4); // 250/60 = 4
        assert_eq!(cap_for(Skill::Weapon, "Warrior", 60, "archery"), 75);
    }

    #[test]
    fn class_without_skill_returns_zero_cap() {
        assert_eq!(cap_for(Skill::Weapon, "Wizard", 60, "2h_slashing"), 0);
        assert_eq!(cap_for(Skill::Armor, "Monk", 60, "plate"), 0);
    }

    #[test]
    fn starting_value_matches_gdscript_pattern() {
        // Warrior 1h_slashing starts at cap(L1) = max(1, 250*1/60) = 4
        assert_eq!(starting_value(Skill::Weapon, "Warrior", "1h_slashing"), 4);
        // Untrained class returns 0
        assert_eq!(starting_value(Skill::Weapon, "Magician", "archery"), 0);
    }

    #[test]
    fn discipline_lookup_known_spells() {
        init_discipline_map();
        assert_eq!(discipline_for_spell("Fireball"), "evocation");
        assert_eq!(discipline_for_spell("Healing Wave"), "alteration");
        assert_eq!(discipline_for_spell("Summon Skeleton"), "conjuration");
        // Rank suffix strip
        assert_eq!(discipline_for_spell("Fireball Rk. II"), "evocation");
        // Unknown falls back to evocation
        assert_eq!(discipline_for_spell("Bogus Made-Up Spell"), "evocation");
    }
}
