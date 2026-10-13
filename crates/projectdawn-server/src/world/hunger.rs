//! Food and water, the EverQuest way (the client repo's
//! `docs/design/food_and_water.md`, decided 2026-10-13).
//!
//! A character carries two counters, hunger and thirst, in seconds of
//! fullness. They run down in real time while the character is alive and in
//! the world (the dead and the offline do not starve), at a rate set by race.
//! When one reaches zero the server eats (or drinks) the first matching item
//! in slot order, base slots first then bags, and the counter rises by that
//! item's size; EQ's "duration" is a size, not a buff, in units of 144 s. Three
//! stages are told once each ("You are hungry." / "You are out of food."), and
//! at zero the regen tick stops natural recovery (`regen.rs`): no food, no HP
//! regen; no water, no mana regen; either, no stamina regen. Nobody dies of it.
//! Eating by hand is refused when full with EQ's own line, and an item eaten
//! early feeds for half its size, which is what makes the auto-eat order worth
//! managing (EQ's stat-food trick). This module holds the pure rules and the
//! inventory search; the world loop (`tick.rs`) runs them and sends the lines.

use super::connection::PerConnection;
use super::inventory::{PlayerInventory, CURSOR_BAG_KEY};
use super::items::{self, Item};

/// EQ's unit of fullness: one "duration" point is 144 real seconds.
pub const FULLNESS_UNIT_SECS: f32 = 144.0;
/// A full stomach: 100 units, four hours. A character is born full.
pub const FULLNESS_CAP_SECS: f32 = 100.0 * FULLNESS_UNIT_SECS;
/// "You are hungry." (or thirsty) below this: 12 units, about 29 minutes.
pub const HUNGRY_AT_SECS: f32 = 12.0 * FULLNESS_UNIT_SECS;
/// An item eaten by hand (a right-click) feeds for this fraction of its size:
/// EQ's rule, "food/drink lasts half as long if you click it".
pub const HAND_EAT_FRACTION: f32 = 0.5;

/// EQ's refusal when a hand-eat finds no room.
pub const EXPLODE_FOOD: &str = "You could not possibly eat any more, you would explode!";
pub const EXPLODE_DRINK: &str = "You could not possibly drink any more, you would explode!";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Need {
    Food,
    Drink,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Fed,
    Hungry,
    Out,
}

pub fn stage_of(secs: f32) -> Stage {
    if secs <= 0.0 {
        Stage::Out
    } else if secs < HUNGRY_AT_SECS {
        Stage::Hungry
    } else {
        Stage::Fed
    }
}

/// How fast a race runs its counters down, as a multiple of a Human's rate.
/// From the sources: a Human reaches the final stage after about 3 hours
/// without a meal, a Halfling after about 90 minutes (the P99 wiki). The rest
/// start at the Human rate; Ogre, Troll and Half-Ogre at 1.5 and the Fae at
/// 0.75 are this project's own calls, flagged for the user. The Revenant is
/// immune to starvation by its own design note (`transformations/revenant.md`).
pub fn race_drain_multiplier(race: &str) -> f32 {
    match race {
        "Halfling" => 2.0,
        "Ogre" | "Troll" | "Half-Ogre" => 1.5,
        "Fae" => 0.75,
        "Revenant" => 0.0,
        _ => 1.0,
    }
}

/// The fullness one item adds: its `food_duration`, read as seconds. A Bread
/// Loaf is 2880 (twenty units, a 48-minute meal); a Water Flask 2160 (fifteen,
/// the size of EQ's summoned Globe of Water).
pub fn fullness_of(item: &Item) -> f32 {
    item.food_duration.max(0.0)
}

fn counter(conn: &PerConnection, need: Need) -> f32 {
    match need {
        Need::Food => conn.hunger_secs,
        Need::Drink => conn.thirst_secs,
    }
}

fn set_counter(conn: &mut PerConnection, need: Need, secs: f32) {
    match need {
        Need::Food => conn.hunger_secs = secs,
        Need::Drink => conn.thirst_secs = secs,
    }
}

pub fn is_famished(conn: &PerConnection, need: Need) -> bool {
    counter(conn, need) <= 0.0
}

/// No room for a hand-eat: within a hair of the cap (the drain moves it off
/// the cap within a tick, so a freshly fed character can eat again a moment
/// later, which is also EQ).
pub fn is_full(conn: &PerConnection, need: Need) -> bool {
    counter(conn, need) >= FULLNESS_CAP_SECS - 0.5
}

/// Add `amount` seconds of fullness, capped. Returns what was actually added.
pub fn feed(conn: &mut PerConnection, need: Need, amount: f32) -> f32 {
    let before = counter(conn, need);
    let after = (before + amount.max(0.0)).min(FULLNESS_CAP_SECS);
    set_counter(conn, need, after);
    after - before
}

/// Run both counters down for `dt` real seconds at the race's rate. Returns
/// the stage lines that became due, at most one per need: a line is told when
/// a stage is entered on the way down, never per tick, and nothing is said on
/// the way back up (the eat line says it).
pub fn drain(conn: &mut PerConnection, dt: f32) -> Vec<&'static str> {
    let mut lines = Vec::new();
    let mult = race_drain_multiplier(&conn.race);
    if mult <= 0.0 || dt <= 0.0 {
        return lines;
    }
    for need in [Need::Food, Need::Drink] {
        let before = counter(conn, need);
        let after = (before - dt * mult).max(0.0);
        set_counter(conn, need, after);
        let (was, now) = (stage_of(before), stage_of(after));
        if now != was {
            match (need, now) {
                (Need::Food, Stage::Hungry) => lines.push("You are hungry."),
                (Need::Food, Stage::Out) => lines.push("You are out of food."),
                (Need::Drink, Stage::Hungry) => lines.push("You are thirsty."),
                (Need::Drink, Stage::Out) => lines.push("You are out of water."),
                (_, Stage::Fed) => {}
            }
        }
    }
    lines
}

fn matches(item: &Item, need: Need) -> bool {
    match need {
        Need::Food => item.is_food,
        Need::Drink => item.is_drink,
    }
}

/// The first food (or drink) the character would eat on its own: base slots
/// 0 to 7 in order, then each bag by the base slot it sits in, its slots in
/// order. A bag held on the cursor is not searched (it is not "in" the
/// inventory, and the placer never fills it). Returns the slot's location
/// string and index as the inventory API names them, plus the item.
pub fn first_in_slot_order(
    inv: &PlayerInventory,
    need: Need,
) -> Option<(String, u32, &'static Item)> {
    for (i, entry) in inv.base.iter().enumerate() {
        if let Some(e) = entry {
            if let Some(item) = items::lookup(&e.item_path) {
                if matches(item, need) {
                    return Some(("base".to_string(), i as u32, item));
                }
            }
        }
    }
    let mut keys: Vec<u8> = inv
        .bags
        .keys()
        .copied()
        .filter(|k| *k != CURSOR_BAG_KEY)
        .collect();
    keys.sort_unstable();
    for key in keys {
        let Some(slots) = inv.bags.get(&key) else { continue };
        for (s, entry) in slots.iter().enumerate() {
            if let Some(e) = entry {
                if let Some(item) = items::lookup(&e.item_path) {
                    if matches(item, need) {
                        return Some((format!("bag_{key}"), s as u32, item));
                    }
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::inventory::InventoryEntry;
    use std::time::Instant;

    fn spawn() -> crate::db::CharacterSpawn {
        crate::db::CharacterSpawn {
            char_id: 1,
            account_id: 1,
            name: "Eater".into(),
            race: "Human".into(),
            class: "Warrior".into(),
            level: 1,
            xp: 0,
            xp_to_next: 100,
            strength: 10,
            dexterity: 10,
            agility: 10,
            intelligence: 10,
            wisdom: 10,
            charisma: 10,
            constitution: 10,
            max_hp: 100.0,
            max_mp: 100.0,
            max_stamina: 100.0,
            hp: 100.0,
            mp: 100.0,
            stamina: 100.0,
            coins: Default::default(),
            bank_coins: Default::default(),
            zone: None,
            pos: (0.0, 0.0, 0.0),
            yaw: 0.0,
            bind: None,
            completed_quests: Vec::new(),
            active_quests: Default::default(),
            hunger: FULLNESS_CAP_SECS,
            thirst: FULLNESS_CAP_SECS,
        }
    }

    fn conn(race: &str) -> PerConnection {
        let mut s = spawn();
        s.race = race.into();
        PerConnection::from_spawn(s, Instant::now())
    }

    #[test]
    fn the_counters_drain_by_race_and_the_revenant_never_does() {
        let mut human = conn("Human");
        let mut halfling = conn("Halfling");
        let mut revenant = conn("Revenant");
        for c in [&mut human, &mut halfling, &mut revenant] {
            drain(c, 60.0);
        }
        assert!((human.hunger_secs - (FULLNESS_CAP_SECS - 60.0)).abs() < 0.01);
        assert!((halfling.hunger_secs - (FULLNESS_CAP_SECS - 120.0)).abs() < 0.01);
        assert_eq!(revenant.hunger_secs, FULLNESS_CAP_SECS);
        assert!((human.thirst_secs - (FULLNESS_CAP_SECS - 60.0)).abs() < 0.01);
    }

    #[test]
    fn each_stage_is_told_once_on_the_way_down_and_never_on_the_way_up() {
        let mut c = conn("Human");
        c.hunger_secs = HUNGRY_AT_SECS + 1.0;
        c.thirst_secs = HUNGRY_AT_SECS + 1.0;
        assert!(drain(&mut c, 0.5).is_empty(), "still fed");
        assert_eq!(drain(&mut c, 1.0), vec!["You are hungry.", "You are thirsty."]);
        assert!(drain(&mut c, 1.0).is_empty(), "hungry is told once");
        assert_eq!(drain(&mut c, HUNGRY_AT_SECS), vec!["You are out of food.", "You are out of water."]);
        assert!(drain(&mut c, 100.0).is_empty(), "out is told once, and the counter floors at zero");
        assert_eq!(c.hunger_secs, 0.0);
        assert!(is_famished(&c, Need::Food) && is_famished(&c, Need::Drink));
        // Fed again: no line, the eat line says it.
        feed(&mut c, Need::Food, 2880.0);
        assert!(drain(&mut c, 1.0).iter().all(|l| !l.contains("hungry")));
        assert_eq!(stage_of(c.hunger_secs), Stage::Fed);
    }

    #[test]
    fn feeding_is_capped_and_fullness_is_the_items_size() {
        let mut c = conn("Human");
        c.hunger_secs = FULLNESS_CAP_SECS - 100.0;
        assert!((feed(&mut c, Need::Food, 2880.0) - 100.0).abs() < 0.01, "only the room is added");
        assert!(is_full(&c, Need::Food));
        let bread = items::lookup("res://data/loot/items/bread_loaf.tres").expect("bread");
        let water = items::lookup("res://data/loot/items/water_flask.tres").expect("water");
        assert_eq!(fullness_of(bread), 20.0 * FULLNESS_UNIT_SECS, "a meal of twenty units");
        assert_eq!(fullness_of(water), 15.0 * FULLNESS_UNIT_SECS, "a drink of fifteen");
    }

    #[test]
    fn the_first_food_in_slot_order_wins_base_slots_before_bags() {
        let bread = "res://data/loot/items/bread_loaf.tres".to_string();
        let water = "res://data/loot/items/water_flask.tres".to_string();
        let mut inv = PlayerInventory::new();
        // A bag lives in base slot 1; its contents are the bag_1 vector.
        inv.base[1] = Some(InventoryEntry { item_path: "res://data/loot/items/small_pouch.tres".into(), count: 1 });
        inv.ensure_bag_init(1);
        inv.bags.get_mut(&1).unwrap()[0] = Some(InventoryEntry { item_path: bread.clone(), count: 2 });
        inv.base[5] = Some(InventoryEntry { item_path: water.clone(), count: 1 });
        assert_eq!(
            first_in_slot_order(&inv, Need::Food).map(|(l, s, _)| (l, s)),
            Some(("bag_1".to_string(), 0)),
            "the only bread is in the bag"
        );
        inv.base[3] = Some(InventoryEntry { item_path: bread.clone(), count: 1 });
        assert_eq!(
            first_in_slot_order(&inv, Need::Food).map(|(l, s, _)| (l, s)),
            Some(("base".to_string(), 3)),
            "a base slot comes before any bag"
        );
        assert_eq!(
            first_in_slot_order(&inv, Need::Drink).map(|(l, s, _)| (l, s)),
            Some(("base".to_string(), 5))
        );
        // A held bag is not searched.
        inv.base[3] = None;
        let held = inv.bags.remove(&1).unwrap();
        inv.bags.insert(CURSOR_BAG_KEY, held);
        assert!(first_in_slot_order(&inv, Need::Food).is_none());
    }
}
