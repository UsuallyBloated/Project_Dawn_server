//! The memorized spell bar, held by the server (client-authority audit fix 5,
//! 2026-10-19; migration 0015). Until then only the client knew which spells a
//! player had memorized, so a modified client could cast any spell its class
//! and level allowed without one. The rules are the client's (`spell_bar.gd`,
//! `memorize.gd`), kept in lockstep by hand:
//!
//! * ten gems, of which `3 + level / 5` are unlocked (never fewer than three);
//! * memorizing needs the caster seated for two seconds and costs nothing (the
//!   mana is paid at the cast, as in EQ); standing in between voids it;
//! * emptying a gem is free;
//! * a cast of a spell in no gem is refused.
//!
//! The bars players already had lived on their own machines, so a character
//! the server has never held a bar for may hand its bar over once (`seed`),
//! each spell checked as a memorize would check it.

use std::collections::BTreeMap;

use super::connection::PerConnection;
use super::spells;

pub const GEM_COUNT: i32 = 10;
pub const MIN_GEMS: i32 = 3;
pub const LEVELS_PER_EXTRA_GEM: i32 = 5;
pub const MEMORIZE_SECS: f32 = 2.0;
/// A memorize that finishes this much early still counts: the client times its
/// own bar, and a packet can arrive a little ahead of the server's clock.
pub const MEMORIZE_GRACE_SECS: f32 = 0.25;

/// How many gems a character of `level` may fill.
pub fn unlocked_gems(level: i32) -> i32 {
    (MIN_GEMS + level / LEVELS_PER_EXTRA_GEM).clamp(MIN_GEMS, GEM_COUNT)
}

/// Whether `conn` may put `spell_name` in `gem` now: the gem unlocked at its
/// level, the spell known to the server and castable by its class at its
/// level. The refusal line otherwise.
pub fn check_gem(conn: &PerConnection, gem: i32, spell_name: &str) -> Result<&'static spells::Spell, &'static str> {
    if gem < 0 || gem >= unlocked_gems(conn.level) {
        return Err("That gem is not open to you yet.");
    }
    let Some(spell) = spells::lookup(spell_name) else {
        return Err("That spell isn't known here yet.");
    };
    // The class the character casts as: a Fallen Paladin memorizes the
    // Fallen kit and not the Paladin's (alignment.rs).
    let class = conn.effective_class();
    if !spell.classes.iter().any(|c| c == &class) || conn.level < spell.required_level(&class) {
        return Err("You cannot memorize that spell.");
    }
    Ok(spell)
}

/// The bar as the client shows it: one entry per gem, "" for an empty one.
pub fn snapshot(gems: &BTreeMap<i32, String>) -> Vec<String> {
    (0..GEM_COUNT).map(|g| gems.get(&g).cloned().unwrap_or_default()).collect()
}

/// Whether a spell sits in any gem.
pub fn is_memorized(gems: &BTreeMap<i32, String>, spell_name: &str) -> bool {
    gems.values().any(|s| s == spell_name)
}

/// The one-time seed: every entry a memorize would allow, the rest dropped.
/// Returns how many were taken.
pub fn seed(conn: &mut PerConnection, offered: &[String]) -> usize {
    let mut taken = 0;
    for (gem, name) in offered.iter().enumerate() {
        if name.is_empty() {
            continue;
        }
        if check_gem(conn, gem as i32, name).is_ok() {
            conn.spell_gems.insert(gem as i32, name.clone());
            taken += 1;
        }
    }
    taken
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caster(class: &str, level: i32) -> PerConnection {
        let spawn = crate::db::CharacterSpawn {
            hunger: 14400.0,
            thirst: 14400.0,
            alignment: None,
            factions: Vec::new(),
            char_id: 1, account_id: 1,
            name: "Gemmer".into(), race: "Human".into(), class: class.into(),
            level, xp: 0, xp_to_next: 100,
            strength: 10, dexterity: 10, agility: 10, intelligence: 10,
            wisdom: 10, charisma: 10, constitution: 10,
            max_hp: 100.0, max_mp: 100.0, max_stamina: 100.0,
            hp: 100.0, mp: 100.0, stamina: 100.0,
            coins: protocol::world::Coins::ZERO, bank_coins: protocol::world::Coins::ZERO, zone: None,
            pos: (0.0, 0.0, 0.0), yaw: 0.0,
            completed_quests: Vec::new(),
            active_quests: Vec::new(),
            bind: None,
        };
        PerConnection::from_spawn(spawn, std::time::Instant::now())
    }

    #[test]
    fn gems_open_with_level_like_the_client_bar() {
        assert_eq!(unlocked_gems(1), 3);
        assert_eq!(unlocked_gems(4), 3);
        assert_eq!(unlocked_gems(5), 4);
        assert_eq!(unlocked_gems(20), 7);
        assert_eq!(unlocked_gems(35), 10);
        assert_eq!(unlocked_gems(60), 10);
    }

    #[test]
    fn a_gem_takes_only_a_spell_the_class_can_cast_in_an_open_gem() {
        let cleric = caster("Cleric", 1);
        assert!(check_gem(&cleric, 0, "Smite").is_ok());
        assert_eq!(check_gem(&cleric, 3, "Smite").err(), Some("That gem is not open to you yet."));
        assert_eq!(check_gem(&cleric, 0, "Fireball").err(), Some("You cannot memorize that spell."));
        assert_eq!(check_gem(&cleric, 0, "No Such Spell").err(), Some("That spell isn't known here yet."));
    }

    #[test]
    fn the_seed_keeps_what_a_memorize_would_allow() {
        let mut cleric = caster("Cleric", 1);
        let offered = vec![
            "Smite".to_string(),
            "Fireball".to_string(),
            "".to_string(),
            "Healing Light".to_string(), // gem 3: not open at level 1
        ];
        assert_eq!(seed(&mut cleric, &offered), 1);
        assert!(is_memorized(&cleric.spell_gems, "Smite"));
        assert!(!is_memorized(&cleric.spell_gems, "Fireball"));
        assert!(!is_memorized(&cleric.spell_gems, "Healing Light"));
        let snap = snapshot(&cleric.spell_gems);
        assert_eq!(snap.len(), GEM_COUNT as usize);
        assert_eq!(snap[0], "Smite");
        assert_eq!(snap[1], "");
    }
}
