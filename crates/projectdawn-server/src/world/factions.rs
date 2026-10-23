//! Factions: a character's standing with each of the world's peoples
//! (docs/design/factions_and_alignment.md, decided 2026-10-23). EverQuest's
//! model, as EQEmu keeps it: every mob and NPC belongs to one faction
//! (`data/factions.toml`); killing a mob costs standing with its faction and
//! pays standing to that faction's enemies, to every player on the mob's
//! hate list; a quest pays explicit hits; standing is capped per faction and
//! conned in nine levels at EQEmu's rule thresholds. The server owns every
//! number here; the client is told its standings and the con lines.

use serde::Deserialize;
use std::collections::HashMap;
use std::sync::OnceLock;

const FACTIONS_TOML: &str = include_str!("../../data/factions.toml");

/// The personal standing's range, EQ's (P99's tables end at 2000 either way).
pub const STANDING_CAP: f32 = 2000.0;
/// A kill's hit is `HIT_BASE + the mob's level` (a Plague Rat 6, a Gnoll Brute
/// 14; EQ's hits ran about ten), tripled for a named mob.
pub const HIT_BASE: f32 = 4.0;
pub const NAMED_HIT_MULT: f32 = 3.0;
/// The share of a kill's hit each enemy of the mob's faction receives.
pub const ENEMY_SHARE: f32 = 0.5;

#[derive(Debug, Deserialize)]
struct FactionsFile {
    faction: Vec<Faction>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Faction {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub allies: Vec<String>,
    #[serde(default)]
    pub enemies: Vec<String>,
    /// Starting modifiers by race and class (EQ's creation-time standing).
    /// Empty for every faction in v1; read from day one.
    #[serde(default)]
    pub start: StartTable,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct StartTable {
    #[serde(default)]
    pub race: HashMap<String, i32>,
    #[serde(default)]
    pub class: HashMap<String, i32>,
}

pub fn table() -> &'static [Faction] {
    static TABLE: OnceLock<Vec<Faction>> = OnceLock::new();
    TABLE.get_or_init(|| {
        let parsed: FactionsFile = toml::from_str(FACTIONS_TOML).expect("factions.toml parses");
        for f in &parsed.faction {
            for link in f.allies.iter().chain(f.enemies.iter()) {
                assert!(
                    parsed.faction.iter().any(|o| &o.id == link),
                    "factions.toml: '{}' links to unknown faction '{link}'",
                    f.id
                );
            }
        }
        parsed.faction
    })
}

pub fn lookup(id: &str) -> Option<&'static Faction> {
    table().iter().find(|f| f.id == id)
}

/// The nine levels, worst first so they order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Scowls,
    Threatening,
    Dubious,
    Apprehensive,
    Indifferent,
    Amiable,
    Kindly,
    Warmly,
    Ally,
}

/// EQEmu's rule defaults (`CalculateFaction`), the user's call over P99's
/// two disagreeing tables: Ally 1100, Warmly 750, Kindly 500, Amiable 100,
/// Indifferent 0, Apprehensive -100, Dubious -500, Threatening -750.
pub fn level_for(value: f32) -> Level {
    if value >= 1100.0 {
        Level::Ally
    } else if value >= 750.0 {
        Level::Warmly
    } else if value >= 500.0 {
        Level::Kindly
    } else if value >= 100.0 {
        Level::Amiable
    } else if value >= 0.0 {
        Level::Indifferent
    } else if value >= -100.0 {
        Level::Apprehensive
    } else if value >= -500.0 {
        Level::Dubious
    } else if value >= -750.0 {
        Level::Threatening
    } else {
        Level::Scowls
    }
}

impl Level {
    pub fn name(self) -> &'static str {
        match self {
            Level::Scowls => "Scowls",
            Level::Threatening => "Threatening",
            Level::Dubious => "Dubious",
            Level::Apprehensive => "Apprehensive",
            Level::Indifferent => "Indifferent",
            Level::Amiable => "Amiable",
            Level::Kindly => "Kindly",
            Level::Warmly => "Warmly",
            Level::Ally => "Ally",
        }
    }

    /// EQ's consider line, the verb after the NPC's name.
    pub fn con_phrase(self) -> &'static str {
        match self {
            Level::Scowls => "scowls at you, ready to attack.",
            Level::Threatening => "glares at you threateningly.",
            Level::Dubious => "glowers at you dubiously.",
            Level::Apprehensive => "looks your way apprehensively.",
            Level::Indifferent => "regards you indifferently.",
            Level::Amiable => "judges you amiably.",
            Level::Kindly => "kindly considers you.",
            Level::Warmly => "looks upon you warmly.",
            Level::Ally => "regards you as an ally.",
        }
    }

    /// The level on the wire: 0 Scowls to 8 Ally.
    pub fn as_wire(self) -> u8 {
        self as u8
    }
}

/// A kill's hit for a mob of `level`: the template's own `faction_hit` when
/// it names one, else `HIT_BASE + level`, tripled for a named mob.
pub fn kill_hit(level: u32, named: bool, override_hit: Option<f32>) -> f32 {
    let base = override_hit.unwrap_or(HIT_BASE + level as f32);
    if named {
        base * NAMED_HIT_MULT
    } else {
        base
    }
}

/// What a kill of a member of `faction_id` does to standings: `-hit` with
/// that faction and `+hit x ENEMY_SHARE` with every faction that lists it as
/// an enemy. An unknown faction (a template naming one the table lacks)
/// moves nothing; `wildlife`, with no links, costs only itself.
pub fn kill_hits(faction_id: &str, hit: f32) -> Vec<(String, f32)> {
    let Some(victim) = lookup(faction_id) else {
        return Vec::new();
    };
    let mut hits = vec![(victim.id.clone(), -hit)];
    for other in table() {
        if other.id != victim.id && other.enemies.iter().any(|e| e == &victim.id) {
            hits.push((other.id.clone(), hit * ENEMY_SHARE));
        }
    }
    hits
}

/// The starting modifier a character of `race` and `class` carries with a
/// faction (0 unless the table says otherwise).
pub fn start_modifier(faction: &Faction, race: &str, class: &str) -> i32 {
    faction.start.race.get(race).copied().unwrap_or(0)
        + faction.start.class.get(class).copied().unwrap_or(0)
}

/// The standing an NPC of `faction` sees: the personal number plus the
/// character's starting modifier with it.
pub fn shown_standing(personal: f32, faction: &Faction, race: &str, class: &str) -> f32 {
    personal + start_modifier(faction, race, class) as f32
}

/// Apply a hit to a personal standing, clamped to the cap. Returns the
/// change that actually landed (zero at the cap, EQ's "could not possibly
/// get any better").
pub fn apply_hit(standing: &mut f32, delta: f32) -> f32 {
    let before = *standing;
    *standing = (*standing + delta).clamp(-STANDING_CAP, STANDING_CAP);
    *standing - before
}

/// EQ's lines for a changed standing.
pub fn change_line(faction: &Faction, applied: f32, standing_now: f32) -> String {
    if applied > 0.0 {
        format!("Your faction standing with {} has gotten better.", faction.name)
    } else if applied < 0.0 {
        format!("Your faction standing with {} has gotten worse.", faction.name)
    } else if standing_now >= STANDING_CAP {
        format!("Your faction standing with {} could not possibly get any better.", faction.name)
    } else {
        format!("Your faction standing with {} could not possibly get any worse.", faction.name)
    }
}

/// The con line for `npc_name` at `level`: "Brom looks upon you warmly."
pub fn con_line(npc_name: &str, level: Level) -> String {
    format!("{npc_name} {}", level.con_phrase())
}

/// The level con's colours, EQ's: how a mob's level reads against the
/// considerer's. White is even, yellow one or two above, red three or more
/// above, blue below down to a floor that widens with level, green past it
/// (the Project 1999 wiki's Consider page, whose colour ranges it calls
/// accurate: blue reaches 3 below at levels 1 to 12, 4 below at 13 to 17,
/// then one more level every four levels, 15 below at 58).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Con {
    Green,
    Blue,
    White,
    Yellow,
    Red,
}

/// How far below the considerer a mob still cons blue (one past it is green).
pub fn blue_floor(player_level: i32) -> i32 {
    if player_level <= 12 {
        3
    } else if player_level <= 17 {
        4
    } else {
        5 + (player_level.min(60) - 18) / 4
    }
}

pub fn con_for(player_level: i32, mob_level: i32) -> Con {
    let diff = mob_level - player_level;
    if diff >= 3 {
        Con::Red
    } else if diff >= 1 {
        Con::Yellow
    } else if diff == 0 {
        Con::White
    } else if -diff <= blue_floor(player_level) {
        Con::Blue
    } else {
        Con::Green
    }
}

impl Con {
    /// EQ's level phrase, one per colour, from the lowest level band of the
    /// P99 Consider page (the bands above 6 vary the wording; the friends
    /// build plays in this one). Red is every band's.
    pub fn phrase(self) -> &'static str {
        match self {
            Con::Green => "Looks like a reasonably safe opponent.",
            Con::Blue => "Looks like you would have the upper hand.",
            Con::White => "Looks like an even fight.",
            Con::Yellow => "Looks like quite a gamble.",
            Con::Red => "What would you like your tombstone to say?",
        }
    }
}

/// The full con line for a mob: the faction phrase, then the level phrase
/// ("a gnoll pup regards you indifferently. Looks like an even fight.").
/// EQ joined the two with a double dash; two sentences here, by the
/// project's writing rule.
pub fn mob_con_line(mob_name: &str, level: Level, player_level: i32, mob_level: i32) -> String {
    format!("{} {}", con_line(mob_name, level), con_for(player_level, mob_level).phrase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_level_con_follows_p99s_colour_bands() {
        // Levels 1 to 12: blue to three below, green past it.
        assert_eq!(con_for(1, 1), Con::White);
        assert_eq!(con_for(1, 2), Con::Yellow);
        assert_eq!(con_for(1, 3), Con::Yellow);
        assert_eq!(con_for(1, 4), Con::Red);
        assert_eq!(con_for(5, 2), Con::Blue);
        assert_eq!(con_for(5, 1), Con::Green);
        assert_eq!(con_for(12, 9), Con::Blue);
        assert_eq!(con_for(12, 8), Con::Green);
        // 13 to 17: four below is still blue.
        assert_eq!(con_for(15, 11), Con::Blue);
        assert_eq!(con_for(15, 10), Con::Green);
        // Then one more every four levels: 18 to 21 five, 26 to 29 seven,
        // 58 to 60 fifteen.
        assert_eq!(blue_floor(18), 5);
        assert_eq!(blue_floor(21), 5);
        assert_eq!(blue_floor(22), 6);
        assert_eq!(blue_floor(29), 7);
        assert_eq!(blue_floor(50), 13);
        assert_eq!(blue_floor(58), 15);
        assert_eq!(blue_floor(60), 15);
        assert_eq!(con_for(60, 45), Con::Blue);
        assert_eq!(con_for(60, 44), Con::Green);
        assert_eq!(
            mob_con_line("a gnoll pup", Level::Indifferent, 1, 10),
            "a gnoll pup regards you indifferently. What would you like your tombstone to say?"
        );
    }

    #[test]
    fn the_table_parses_with_its_links_and_names() {
        let t = table();
        assert_eq!(t.len(), 6);
        let town = lookup("greyveil_town").expect("the town");
        assert_eq!(town.name, "The Folk of Greyveil");
        assert!(town.enemies.iter().any(|e| e == "gnoll_raiders"));
        assert!(lookup("wildlife").unwrap().enemies.is_empty(), "animals link to no one");
        assert!(lookup("nobody").is_none());
        assert!(town.start.race.is_empty() && town.start.class.is_empty(), "v1 ships the start table empty");
    }

    #[test]
    fn a_kill_costs_its_faction_and_pays_its_enemies_half() {
        let hits = kill_hits("gnoll_raiders", 14.0);
        assert_eq!(hits.len(), 3, "the gnolls, the town, the Watch: {hits:?}");
        assert!(hits.contains(&("gnoll_raiders".to_string(), -14.0)));
        assert!(hits.contains(&("greyveil_town".to_string(), 7.0)));
        assert!(hits.contains(&("greyveil_watch".to_string(), 7.0)));
        // An animal costs only wildlife; the town does not care.
        let wild = kill_hits("wildlife", 5.0);
        assert_eq!(wild, vec![("wildlife".to_string(), -5.0)]);
        assert!(kill_hits("martians", 5.0).is_empty(), "an unknown faction moves nothing");
    }

    #[test]
    fn the_hit_follows_the_level_and_the_named_flag() {
        assert_eq!(kill_hit(2, false, None), 6.0, "a Plague Rat");
        assert_eq!(kill_hit(10, false, None), 14.0, "a Gnoll Brute");
        assert_eq!(kill_hit(10, true, None), 42.0, "Greth, named");
        assert_eq!(kill_hit(10, false, Some(20.0)), 20.0, "a template's own number wins");
    }

    #[test]
    fn the_nine_levels_sit_at_eqemu_s_thresholds() {
        assert_eq!(level_for(1100.0), Level::Ally);
        assert_eq!(level_for(1099.0), Level::Warmly);
        assert_eq!(level_for(750.0), Level::Warmly);
        assert_eq!(level_for(500.0), Level::Kindly);
        assert_eq!(level_for(100.0), Level::Amiable);
        assert_eq!(level_for(0.0), Level::Indifferent);
        assert_eq!(level_for(-1.0), Level::Apprehensive);
        assert_eq!(level_for(-100.0), Level::Apprehensive);
        assert_eq!(level_for(-101.0), Level::Dubious);
        assert_eq!(level_for(-500.0), Level::Dubious);
        assert_eq!(level_for(-501.0), Level::Threatening);
        assert_eq!(level_for(-750.0), Level::Threatening);
        assert_eq!(level_for(-751.0), Level::Scowls);
        assert!(Level::Scowls < Level::Dubious && Level::Dubious < Level::Ally, "they order worst first");
        assert_eq!(Level::Scowls.as_wire(), 0);
        assert_eq!(Level::Ally.as_wire(), 8);
        assert_eq!(con_line("Brom", Level::Warmly), "Brom looks upon you warmly.");
        assert_eq!(con_line("Greth", Level::Scowls), "Greth scowls at you, ready to attack.");
    }

    #[test]
    fn standing_clamps_and_the_lines_say_so() {
        let town = lookup("greyveil_town").unwrap();
        let mut s = 1990.0;
        assert_eq!(apply_hit(&mut s, 25.0), 10.0, "only the room to the cap lands");
        assert_eq!(s, STANDING_CAP);
        assert_eq!(apply_hit(&mut s, 25.0), 0.0);
        assert!(change_line(town, 0.0, s).contains("could not possibly get any better"));
        let mut low = -1995.0;
        assert_eq!(apply_hit(&mut low, -50.0), -5.0);
        assert!(change_line(town, 0.0, low).contains("any worse"));
        assert!(change_line(town, 7.0, 7.0).contains("gotten better"));
        assert!(change_line(town, -7.0, -7.0).contains("gotten worse"));
    }

    #[test]
    fn the_start_table_adds_to_what_an_npc_sees() {
        let mut f = lookup("greyveil_town").unwrap().clone();
        assert_eq!(shown_standing(10.0, &f, "Dark Elf", "Necromancer"), 10.0, "empty table");
        f.start.race.insert("Dark Elf".into(), -300);
        f.start.class.insert("Necromancer".into(), -100);
        assert_eq!(start_modifier(&f, "Dark Elf", "Necromancer"), -400);
        assert_eq!(shown_standing(10.0, &f, "Dark Elf", "Necromancer"), -390.0);
        assert_eq!(level_for(shown_standing(10.0, &f, "Dark Elf", "Necromancer")), Level::Dubious);
        assert_eq!(shown_standing(10.0, &f, "Human", "Cleric"), 10.0);
    }
}
