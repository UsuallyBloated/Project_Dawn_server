//! Spell power: what a caster's stats and casting skills add to a spell. The
//! server applies it at cast time (client-authority audit fix 3, 2026-10-19:
//! "Events happening on the client are a no-go"). Until then only the client
//! computed it, so online every spell landed at its flat `spells.toml` number.
//!
//! The formula is the client's, ported as is (user, 2026-10-19: "stick with
//! the client's spell formula for now"; `autoloads/spells.gd::_apply_spell`,
//! `autoloads/combat.gd`, `autoloads/casting_skills.gd`):
//!
//! * a nuke (ENEMY or AOE base damage): `(base + INT x 0.5) x evocation`;
//! * a heal (any heal amount, lifetaps and pet heals included): `+ WIS x 0.3`;
//! * an absorb shield: `x abjuration`;
//! * every timed effect (HoT, DoT, damage shield, mez, root, snare, slow,
//!   silence, stat buffs; for a non-song also speed, mana regen, haste and
//!   accuracy/crit buffs): `x alteration`;
//! * a spell crit per hit: chance `(INT - 10) x 0.002`, at most 20%, for
//!   1.5x to 2x.
//!
//! Each skill multiplier applies only to a spell of that discipline and runs
//! from 1.0 at no skill to its maximum at the skill's cap for the caster's
//! class and level. The alignment multiplier the client also applies
//! (Paladin and Shadow Knight off their alignment) is not here: the server
//! has no alignment yet (audit, finding b4), and at every starting score it is
//! 1.0 anyway. The client's numbers must stay in lockstep with these by hand.

use super::connection::PerConnection;
use super::skills::{self, Skill};
use super::spells::Spell;

pub const NUKE_INT_FACTOR: f32 = 0.5;
pub const HEAL_WIS_FACTOR: f32 = 0.3;
pub const SPELL_CRIT_PER_INT: f32 = 0.002;
pub const SPELL_CRIT_MAX: f32 = 0.20;
/// Crit damage is a uniform roll between these.
pub const SPELL_CRIT_MULT_MIN: f32 = 1.5;
pub const SPELL_CRIT_MULT_MAX: f32 = 2.0;
/// Each skill's bonus at its cap: Evocation damage +15%, Alteration
/// duration +25%, Abjuration absorb +20%.
pub const EVOCATION_MAX_BONUS: f32 = 0.15;
pub const ALTERATION_MAX_BONUS: f32 = 0.25;
pub const ABJURATION_MAX_BONUS: f32 = 0.20;

/// The caster's score in a casting skill as a fraction of its cap, 0 to 1;
/// 0 for a class that cannot train it.
fn skill_ratio(conn: &PerConnection, key: &str) -> f32 {
    let cap = skills::cap_for(Skill::Casting, &conn.class, conn.level, key);
    if cap <= 0 {
        return 0.0;
    }
    let score = conn.casting_skills.get(key).copied().unwrap_or(0);
    (score as f32 / cap as f32).clamp(0.0, 1.0)
}

/// The skill multiplier a spell of `discipline` gets from the skill `key`.
fn discipline_mult(conn: &PerConnection, discipline: &str, key: &str, max_bonus: f32) -> f32 {
    if discipline != key {
        return 1.0;
    }
    1.0 + max_bonus * skill_ratio(conn, key)
}

/// A copy of `spell` with the caster's power applied. Everything after the
/// cast gates reads this copy, so each spell arm needs no formula of its own.
pub fn empower(spell: &Spell, caster: &PerConnection) -> Spell {
    let mut s = spell.clone();
    let discipline = skills::discipline_for_spell(&spell.name);
    let dmg_mult = discipline_mult(caster, discipline, "evocation", EVOCATION_MAX_BONUS);
    let dur_mult = discipline_mult(caster, discipline, "alteration", ALTERATION_MAX_BONUS);
    let absorb_mult = discipline_mult(caster, discipline, "abjuration", ABJURATION_MAX_BONUS);

    // Base damage is a nuke only on a hostile spell; on a SELF spell it is
    // the caster's own cost (a blood-price spell), which stats never touch.
    if matches!(s.target_type.as_str(), "ENEMY" | "AOE") && s.base_damage > 0.0 {
        s.base_damage = (s.base_damage + caster.intelligence as f32 * NUKE_INT_FACTOR) * dmg_mult;
    }
    if s.heal_amount > 0.0 {
        s.heal_amount += caster.wisdom as f32 * HEAL_WIS_FACTOR;
    }
    s.absorb_amount *= absorb_mult;

    s.hot_duration *= dur_mult;
    s.dot_duration *= dur_mult;
    s.damage_shield_duration *= dur_mult;
    s.cc_duration *= dur_mult;
    s.root_duration *= dur_mult;
    s.slow_duration *= dur_mult;
    s.attack_slow_duration *= dur_mult;
    s.silence_duration *= dur_mult;
    s.primary_stat_buff_duration *= dur_mult;
    if !s.is_song {
        s.move_speed_duration *= dur_mult;
        s.mp_regen_duration *= dur_mult;
        s.haste_duration *= dur_mult;
        s.stat_buff_duration *= dur_mult;
    }
    s
}

/// The chance a caster with `intelligence` lands a spell crit.
pub fn crit_chance(intelligence: i32) -> f32 {
    ((intelligence - 10) as f32 * SPELL_CRIT_PER_INT).clamp(0.0, SPELL_CRIT_MAX)
}

/// One spell hit's damage after the crit roll: `(amount, crit)`. The roll is
/// per hit, so each target of an area spell rolls its own, as on the client.
pub fn roll_hit(amount: i32, intelligence: i32, rng: &mut impl rand::Rng) -> (i32, bool) {
    if rng.gen::<f32>() >= crit_chance(intelligence) {
        return (amount, false);
    }
    let mult = rng.gen_range(SPELL_CRIT_MULT_MIN..=SPELL_CRIT_MULT_MAX);
    (((amount as f32 * mult) as i32).max(1), true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::spells;

    fn caster(class: &str, level: i32, int: i32, wis: i32) -> PerConnection {
        // The server loads the discipline map at start (tick.rs); without it
        // every spell reads as evocation.
        skills::init_discipline_map();
        let spawn = crate::db::CharacterSpawn {
            hunger: 14400.0,
            thirst: 14400.0,
            char_id: 1, account_id: 1,
            name: "Caster".into(), race: "Human".into(), class: class.into(),
            level, xp: 0, xp_to_next: 100,
            strength: 10, dexterity: 10, agility: 10, intelligence: int,
            wisdom: wis, charisma: 10, constitution: 10,
            max_hp: 100.0, max_mp: 100.0, max_stamina: 100.0,
            hp: 100.0, mp: 100.0, stamina: 100.0,
            coins: protocol::world::Coins::ZERO, bank_coins: protocol::world::Coins::ZERO, zone: None,
            pos: (0.0, 0.0, 0.0), yaw: 0.0,
            completed_quests: Vec::new(),
            active_quests: Vec::new(),
            bind: None,
        };
        let mut c = PerConnection::from_spawn(spawn, std::time::Instant::now());
        c.casting_skills.clear();
        c
    }

    fn spell(name: &str) -> &'static Spell {
        spells::lookup(name).unwrap_or_else(|| panic!("{name} in spells.toml"))
    }

    #[test]
    fn a_nuke_adds_half_the_casters_int() {
        // Smite is evocation; with no evocation trained the multiplier is 1.
        let s = empower(spell("Smite"), &caster("Cleric", 20, 40, 10));
        assert!((s.base_damage - (35.0 + 20.0)).abs() < 1e-4, "{}", s.base_damage);
    }

    #[test]
    fn evocation_at_its_cap_adds_fifteen_percent() {
        let mut c = caster("Cleric", 20, 40, 10);
        let cap = skills::cap_for(Skill::Casting, "Cleric", 20, "evocation");
        assert!(cap > 0, "Clerics train evocation");
        c.casting_skills.insert("evocation".into(), cap);
        let s = empower(spell("Smite"), &c);
        assert!((s.base_damage - 55.0 * 1.15).abs() < 1e-3, "{}", s.base_damage);
    }

    #[test]
    fn a_heal_adds_three_tenths_of_wis_and_a_nuke_gets_no_wis() {
        let c = caster("Cleric", 20, 10, 50);
        let heal = empower(spell("Healing Light"), &c);
        assert!((heal.heal_amount - (60.0 + 15.0)).abs() < 1e-4, "{}", heal.heal_amount);
        assert_eq!(heal.base_damage, 0.0, "a heal deals nothing");
        let smite = empower(spell("Smite"), &c);
        assert!((smite.base_damage - 40.0).abs() < 1e-4, "INT 10 adds 5, WIS adds nothing");
    }

    #[test]
    fn a_self_spells_base_damage_is_a_cost_and_stays_flat() {
        let mut selfish = spell("Smite").clone();
        selfish.target_type = "SELF".into();
        let s = empower(&selfish, &caster("Cleric", 20, 100, 10));
        assert_eq!(s.base_damage, 35.0);
    }

    #[test]
    fn alteration_stretches_timed_effects_only_for_its_own_spells() {
        // Entangle is alteration: its DoT runs longer with the skill, and its
        // direct hit takes INT but no evocation bonus.
        let mut c = caster("Druid", 20, 30, 10);
        let cap = skills::cap_for(Skill::Casting, "Druid", 20, "alteration");
        assert!(cap > 0, "Druids train alteration");
        c.casting_skills.insert("alteration".into(), cap);
        let e = empower(spell("Entangle"), &c);
        assert!((e.dot_duration - 18.0 * 1.25).abs() < 1e-3, "{}", e.dot_duration);
        assert!((e.base_damage - 25.0).abs() < 1e-4, "{}", e.base_damage);
        assert_eq!(e.dot_dps, spell("Entangle").dot_dps, "the DoT's damage is not stat-scaled");
        // Smite is evocation: the trained alteration changes nothing on it.
        assert_eq!(empower(spell("Smite"), &c).base_damage, 35.0 + 15.0);
    }

    #[test]
    fn crit_chance_follows_int_and_is_capped() {
        assert_eq!(crit_chance(10), 0.0);
        assert!((crit_chance(60) - 0.10).abs() < 1e-6);
        assert_eq!(crit_chance(500), SPELL_CRIT_MAX);
        assert_eq!(crit_chance(0), 0.0);
    }

    #[test]
    fn a_crit_lands_between_one_and_a_half_and_double() {
        let mut rng = rand::thread_rng();
        let mut crits = 0;
        for _ in 0..2000 {
            let (dmg, crit) = roll_hit(100, 110, &mut rng); // a 20% crit chance
            if crit {
                crits += 1;
                assert!((150..=200).contains(&dmg), "crit {dmg}");
            } else {
                assert_eq!(dmg, 100);
            }
        }
        assert!((250..=550).contains(&crits), "about a fifth crit: {crits}");
    }
}
