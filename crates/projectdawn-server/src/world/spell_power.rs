//! Spell power: what the caster adds to a spell beyond its listed number.
//! Since the EQ parity audit of 2026-10-20 that is only the spell crit.
//!
//! The client's formula used to run here as well (audit fix 3, 2026-10-19:
//! a nuke took `+ INT x 0.5`, a heal `+ WIS x 0.3`, and the casting skills
//! multiplied damage, durations and absorbs). None of it is classic EQ, where
//! a spell did exactly its listed, level-scaled number and the casting skills
//! decided whether it fizzled (`cast_gate::fizzle_chance`), so the user had
//! it removed (2026-10-20: "If the INT and WIS bonus is not P99 accurate,
//! then we can discard it"; "casting-skill multiplier should follow p99
//! rules"). The INT-based spell crit stays by the same decision, though it is
//! not classic either ("should be added even though its not classic").
//!
//! The alignment multiplier the client applies (Paladin and Shadow Knight off
//! their alignment) is not here: the server has no alignment yet, and at
//! every starting score it is 1.0 anyway.

pub const SPELL_CRIT_PER_INT: f32 = 0.002;
pub const SPELL_CRIT_MAX: f32 = 0.20;
/// Crit damage is a uniform roll between these.
pub const SPELL_CRIT_MULT_MIN: f32 = 1.5;
pub const SPELL_CRIT_MULT_MAX: f32 = 2.0;

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
