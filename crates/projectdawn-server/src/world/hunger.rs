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
/// 0.75 are this project's own calls, flagged for the user. The Revenant's
/// starvation immunity (`transformations/revenant.md`) is NOT keyed here: it
/// belongs to the transformation when that is server-side, not to a race
/// string a client could send at creation (the 2026-10-13 review).
pub fn race_drain_multiplier(race: &str) -> f32 {
    match race {
        "Halfling" => 2.0,
        "Ogre" | "Troll" | "Half-Ogre" => 1.5,
        "Fae" => 0.75,
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
    conn.hunger_dirty = true;
    after - before
}

/// The line for a stage, or none for Fed.
pub fn stage_line(need: Need, stage: Stage) -> Option<&'static str> {
    match (need, stage) {
        (Need::Food, Stage::Hungry) => Some("You are hungry."),
        (Need::Food, Stage::Out) => Some("You are out of food."),
        (Need::Drink, Stage::Hungry) => Some("You are thirsty."),
        (Need::Drink, Stage::Out) => Some("You are out of water."),
        (_, Stage::Fed) => None,
    }
}

/// Run both counters down for `dt` real seconds at the race's rate. Returns
/// the stages entered on the way down, at most one per need, with their
/// lines: a stage is told when it is entered, never per tick, and nothing is
/// said on the way back up (the eat line says it). The caller decides whether
/// to say "out of food": not when it eats something the same tick.
pub fn drain(conn: &mut PerConnection, dt: f32) -> Vec<(Need, Stage, &'static str)> {
    let mut entered = Vec::new();
    let mult = race_drain_multiplier(&conn.race);
    if mult <= 0.0 || dt <= 0.0 {
        return entered;
    }
    for need in [Need::Food, Need::Drink] {
        let before = counter(conn, need);
        let after = (before - dt * mult).max(0.0);
        set_counter(conn, need, after);
        let (was, now) = (stage_of(before), stage_of(after));
        if now != was {
            conn.hunger_dirty = true;
            if let Some(line) = stage_line(need, now) {
                entered.push((need, now, line));
            }
        }
    }
    entered
}

/// The lines for the stages a character is ALREADY in: told once when it
/// enters the world, since `drain` reports only transitions and a character
/// can log in hungry or out (the counters do not move offline).
pub fn current_stage_lines(conn: &PerConnection) -> Vec<&'static str> {
    [Need::Food, Need::Drink]
        .into_iter()
        .filter_map(|need| stage_line(need, stage_of(counter(conn, need))))
        .collect()
}

/// How often the hunger window is corrected when nothing else happened.
/// Between sends the client runs the bars down at the rate it was given.
pub const UPDATE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

/// Counter-seconds drained per real second right now: the race's rate while
/// the character is alive in the world, nothing while dead or in the lobby
/// (the same condition the regen step drains under).
pub fn drain_rate_now(conn: &PerConnection) -> f32 {
    if conn.in_world && conn.hp > 0.0 && !conn.death_processed {
        race_drain_multiplier(&conn.race)
    } else {
        0.0
    }
}

/// Whether the hunger window should be sent now: never sent, a counter
/// jumped or crossed a stage, the drain rate changed (a death, a respawn),
/// or a minute has passed.
pub fn update_due(conn: &PerConnection, now: std::time::Instant) -> bool {
    conn.hunger_dirty
        || (conn.hunger_rate_sent - drain_rate_now(conn)).abs() > f32::EPSILON
        || conn
            .hunger_sent_at
            .map_or(true, |t| now.duration_since(t) >= UPDATE_INTERVAL)
}

/// Record a send, so `update_due` waits for the next reason.
pub fn mark_sent(conn: &mut PerConnection, now: std::time::Instant) {
    conn.hunger_dirty = false;
    conn.hunger_sent_at = Some(now);
    conn.hunger_rate_sent = drain_rate_now(conn);
}

/// EQ's line for the people around someone eating or drinking, word for
/// word from EQEmu's string table (`EATING_MESSAGE` / `DRINKING_MESSAGE`).
/// The eater gets their own "You eat a X." instead.
pub fn eat_emote(eater: &str, item_name: &str, need: Need) -> String {
    match need {
        Need::Food => format!("Chomp, chomp, chomp... {eater} takes a bite from a {item_name}."),
        Need::Drink => format!("Glug, glug, glug... {eater} takes a drink from a {item_name}."),
    }
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
/// inventory, and the placer never fills it). `skip` passes over slots the
/// caller rules out (the tick passes the trade lock, so food offered in an
/// open trade is never eaten). Returns the slot's location string and index as
/// the inventory API names them, plus the item.
pub fn first_in_slot_order(
    inv: &PlayerInventory,
    need: Need,
    skip: impl Fn(&str, u32) -> bool,
) -> Option<(String, u32, &'static Item)> {
    for (i, entry) in inv.base.iter().enumerate() {
        if let Some(e) = entry {
            if let Some(item) = items::lookup(&e.item_path) {
                if matches(item, need) && !skip("base", i as u32) {
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
                    if matches(item, need) && !skip(&format!("bag_{key}"), s as u32) {
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
            alignment: None,
            factions: Vec::new(),
        }
    }

    fn conn(race: &str) -> PerConnection {
        let mut s = spawn();
        s.race = race.into();
        PerConnection::from_spawn(s, Instant::now())
    }

    #[test]
    fn the_counters_drain_by_race_and_an_unknown_race_at_the_human_rate() {
        let mut human = conn("Human");
        let mut halfling = conn("Halfling");
        let mut odd = conn("Revenant");
        for c in [&mut human, &mut halfling, &mut odd] {
            drain(c, 60.0);
        }
        assert!((human.hunger_secs - (FULLNESS_CAP_SECS - 60.0)).abs() < 0.01);
        assert!((halfling.hunger_secs - (FULLNESS_CAP_SECS - 120.0)).abs() < 0.01);
        // No race string buys immunity; the transformation will, when it is
        // server-side.
        assert!((odd.hunger_secs - (FULLNESS_CAP_SECS - 60.0)).abs() < 0.01);
        assert!((human.thirst_secs - (FULLNESS_CAP_SECS - 60.0)).abs() < 0.01);
    }

    /// The hunger window's send rule: a fresh connection is due, a send
    /// clears it, a meal or a stage change makes it due again, the minute
    /// makes it due, and so does a change of drain rate (death, respawn).
    #[test]
    fn the_hunger_window_is_sent_on_each_reason_and_not_otherwise() {
        let t0 = Instant::now();
        let mut c = conn("Halfling");
        c.in_world = true;
        assert!(update_due(&c, t0), "never sent: due");
        mark_sent(&mut c, t0);
        assert_eq!(c.hunger_rate_sent, 2.0, "a living Halfling drains at twice the Human rate");
        assert!(!update_due(&c, t0 + std::time::Duration::from_secs(5)), "nothing happened: not due");

        // A small drain inside a stage is not a reason.
        drain(&mut c, 10.0);
        assert!(!update_due(&c, t0 + std::time::Duration::from_secs(5)));
        // Crossing into Hungry is.
        let to_hungry = (c.hunger_secs - HUNGRY_AT_SECS + 1.0) / 2.0;
        drain(&mut c, to_hungry);
        assert!(update_due(&c, t0 + std::time::Duration::from_secs(5)), "a stage change is a reason");
        mark_sent(&mut c, t0);

        // A meal is.
        feed(&mut c, Need::Food, 100.0);
        assert!(update_due(&c, t0), "an eat is a reason");
        mark_sent(&mut c, t0);

        // The minute is.
        assert!(update_due(&c, t0 + UPDATE_INTERVAL), "the minute is a reason");

        // Death stops the drain, and that is a reason too.
        c.hp = 0.0;
        assert_eq!(drain_rate_now(&c), 0.0);
        assert!(update_due(&c, t0), "a death changes the rate");
        mark_sent(&mut c, t0);
        assert!(!update_due(&c, t0));
        c.hp = 50.0;
        assert!(update_due(&c, t0), "a respawn changes it back");
    }

    #[test]
    fn the_eating_emote_is_eqs_line() {
        assert_eq!(
            eat_emote("Dropsy", "Bread Loaf", Need::Food),
            "Chomp, chomp, chomp... Dropsy takes a bite from a Bread Loaf."
        );
        assert_eq!(
            eat_emote("Dropsy", "Water Flask", Need::Drink),
            "Glug, glug, glug... Dropsy takes a drink from a Water Flask."
        );
    }

    #[test]
    fn each_stage_is_told_once_on_the_way_down_and_never_on_the_way_up() {
        let mut c = conn("Human");
        c.hunger_secs = HUNGRY_AT_SECS + 1.0;
        c.thirst_secs = HUNGRY_AT_SECS + 1.0;
        assert!(drain(&mut c, 0.5).is_empty(), "still fed");
        let lines: Vec<&str> = drain(&mut c, 1.0).into_iter().map(|(_, _, l)| l).collect();
        assert_eq!(lines, vec!["You are hungry.", "You are thirsty."]);
        assert!(drain(&mut c, 1.0).is_empty(), "hungry is told once");
        let out = drain(&mut c, HUNGRY_AT_SECS);
        assert_eq!(out.len(), 2);
        assert!(out.iter().all(|(_, stage, _)| *stage == Stage::Out));
        assert_eq!(out[0].2, "You are out of food.");
        assert!(drain(&mut c, 100.0).is_empty(), "out is told once, and the counter floors at zero");
        assert_eq!(c.hunger_secs, 0.0);
        assert!(is_famished(&c, Need::Food) && is_famished(&c, Need::Drink));
        // The character is still out: the login announcement names both.
        assert_eq!(current_stage_lines(&c), vec!["You are out of food.", "You are out of water."]);
        // Fed again: no line, the eat line says it.
        feed(&mut c, Need::Food, 2880.0);
        assert!(drain(&mut c, 1.0).is_empty());
        assert_eq!(stage_of(c.hunger_secs), Stage::Fed);
        assert_eq!(current_stage_lines(&c), vec!["You are out of water."]);
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
            first_in_slot_order(&inv, Need::Food, |_, _| false).map(|(l, s, _)| (l, s)),
            Some(("bag_1".to_string(), 0)),
            "the only bread is in the bag"
        );
        inv.base[3] = Some(InventoryEntry { item_path: bread.clone(), count: 1 });
        assert_eq!(
            first_in_slot_order(&inv, Need::Food, |_, _| false).map(|(l, s, _)| (l, s)),
            Some(("base".to_string(), 3)),
            "a base slot comes before any bag"
        );
        assert_eq!(
            first_in_slot_order(&inv, Need::Drink, |_, _| false).map(|(l, s, _)| (l, s)),
            Some(("base".to_string(), 5))
        );
        // A held bag is not searched.
        inv.base[3] = None;
        let held = inv.bags.remove(&1).unwrap();
        inv.bags.insert(CURSOR_BAG_KEY, held);
        assert!(first_in_slot_order(&inv, Need::Food, |_, _| false).is_none());
    }
}
