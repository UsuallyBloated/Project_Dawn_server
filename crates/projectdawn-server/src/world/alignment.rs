//! Alignment: one good-to-evil score per character, the server's since the
//! factions build (docs/design/factions_and_alignment.md, decided
//! 2026-10-23). It used to live only on the client (`autoloads/alignment.gd`),
//! moved by the client's own hand, which the client-authority audit listed
//! as its last open part. Moved by three things: kills (the victim's
//! alignment tier, `docs/concepts/alignment/events.md`), quests (a per-quest
//! delta) and spells at a hundred-thousandth of their mana cost (the user's
//! equation: "spells: .001% of their spell cost"), each through the momentum
//! rule. Read by the prestige-class gates (a Paladin at Evil casts as a
//! `Paladin_Fallen`, a Shadow Knight at Exalted as a `Shadow Knight_Redeemed`)
//! and the off-side penalty on their spells. The five tiers, the class
//! starting scores and the momentum rule are the client's, kept in lockstep
//! by hand with `alignment.gd` and `CharacterData.CLASS_STARTING_ALIGNMENT`.

use super::spells::Spell;

/// The score's range; the momentum rule makes the ends hard to reach.
pub const SCORE_CAP: f32 = 2000.0;
/// A cast moves alignment by this fraction of the spell's mana cost, toward
/// its school's side: 0.001% (user, 2026-10-20). A Smite (12 mana) is
/// 0.00012 a cast; eight thousand casts move one point.
pub const SPELL_MANA_FRACTION: f32 = 0.00001;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    Evil,
    Bad,
    Neutral,
    Good,
    Exalted,
}

impl Tier {
    pub fn name(self) -> &'static str {
        match self {
            Tier::Evil => "Evil",
            Tier::Bad => "Bad",
            Tier::Neutral => "Neutral",
            Tier::Good => "Good",
            Tier::Exalted => "Exalted",
        }
    }
}

/// The client's `_calc_tier`: Exalted from 1500, Good from 300, Neutral
/// from -300, Bad from -1500, Evil below.
pub fn tier_for(score: f32) -> Tier {
    if score >= 1500.0 {
        Tier::Exalted
    } else if score >= 300.0 {
        Tier::Good
    } else if score >= -300.0 {
        Tier::Neutral
    } else if score >= -1500.0 {
        Tier::Bad
    } else {
        Tier::Evil
    }
}

/// Where a class begins (`CharacterData.CLASS_STARTING_ALIGNMENT`): a
/// character with no stored score is seeded here once, at creation or on
/// its first load after the migration.
pub fn starting_score(class: &str) -> f32 {
    match class {
        "Paladin" => 500.0,
        "Cleric" => 400.0,
        "Monk" | "Witch Hunter" => 100.0,
        "Blood Mage" => -500.0,
        "Shadow Knight" | "Necromancer" => -1600.0,
        _ => 0.0,
    }
}

/// Apply a delta through the momentum rule (`events.md`): a move away from
/// the centre is scaled by `1 - |score| / 2000`, so the extremes resist; a
/// move back toward the centre lands whole. Clamped to the cap.
pub fn apply(score: f32, delta: f32) -> f32 {
    if delta == 0.0 {
        return score;
    }
    let away = (score > 0.0 && delta > 0.0) || (score < 0.0 && delta < 0.0);
    let scaled = if away {
        delta * (1.0 - (score.abs() / SCORE_CAP)).max(0.0)
    } else {
        delta
    };
    (score + scaled).clamp(-SCORE_CAP, SCORE_CAP)
}

/// What killing a mob of `tier` does to the killer (`events.md`'s table):
/// an Exalted -80, a Good -30, a Neutral nothing, a Bad +10, an Evil +25 (a
/// named Evil +50), an Evil undead +20 (a named one +50), an innocent -60.
pub fn kill_delta(tier: Option<&str>, named: bool) -> f32 {
    match tier.unwrap_or("neutral") {
        "exalted" => -80.0,
        "good" => -30.0,
        "innocent" => -60.0,
        "bad" => 10.0,
        "evil" => {
            if named {
                50.0
            } else {
                25.0
            }
        }
        "undead" => {
            if named {
                50.0
            } else {
                20.0
            }
        }
        _ => 0.0,
    }
}

/// Which way a spell's school leans (`events.md`): holy damage and healing
/// up; shadow damage, necromantic summoning, blood prices and charms down;
/// everything else nowhere.
pub fn spell_sign(spell: &Spell) -> f32 {
    let hostile = matches!(spell.target_type.as_str(), "ENEMY" | "AOE") && spell.base_damage > 0.0;
    if spell.target_type == "PET_CHARM" {
        return -1.0;
    }
    if spell.hp_cost > 0.0 {
        return -1.0;
    }
    if spell.target_type == "PET_SUMMON" && spell.damage_type == "SHADOW" {
        return -1.0;
    }
    if hostile && spell.damage_type == "SHADOW" {
        return -1.0;
    }
    if hostile && spell.damage_type == "HOLY" {
        return 1.0;
    }
    if spell.heal_amount > 0.0 || spell.hot_hps > 0.0 {
        return 1.0;
    }
    0.0
}

/// A cast's alignment delta: the school's sign times a hundred-thousandth
/// of the mana cost.
pub fn spell_delta(spell: &Spell) -> f32 {
    spell_sign(spell) * spell.mana_cost * SPELL_MANA_FRACTION
}

/// The class a character casts and uses skills as: a Paladin at Evil is a
/// `Paladin_Fallen`, a Shadow Knight at Exalted a `Shadow Knight_Redeemed`,
/// anyone else their own class (the client's `get_effective_class`).
pub fn effective_class(class: &str, score: f32) -> String {
    match (class, tier_for(score)) {
        ("Paladin", Tier::Evil) => "Paladin_Fallen".to_string(),
        ("Shadow Knight", Tier::Exalted) => "Shadow Knight_Redeemed".to_string(),
        _ => class.to_string(),
    }
}

/// The off-side penalty on a Paladin's or Shadow Knight's spells (the
/// client's `_get_alignment_effectiveness`, kept by the user 2026-10-23): a
/// Paladin casts at 0.7 when Neutral and 0.4 when Bad, a Shadow Knight at
/// 0.7 when Neutral and 0.4 when Good; everyone else, and either at their own
/// end or as the prestige class, at 1.0.
pub fn off_side_multiplier(class: &str, score: f32) -> f32 {
    match (class, tier_for(score)) {
        ("Paladin", Tier::Neutral) | ("Shadow Knight", Tier::Neutral) => 0.7,
        ("Paladin", Tier::Bad) | ("Shadow Knight", Tier::Good) => 0.4,
        _ => 1.0,
    }
}

/// A copy of `spell` with its effects scaled by the off-side multiplier:
/// damage, heals, the heal-over-time and the mana regen, the fields the
/// client scaled by its `effectiveness` (`spells.gd`). Costs, times and
/// durations stay.
pub fn penalise(spell: &Spell, mult: f32) -> Spell {
    let mut s = spell.clone();
    s.base_damage *= mult;
    s.heal_amount *= mult;
    s.hot_hps *= mult;
    s.mp_regen_hps *= mult;
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::spells;

    #[test]
    fn the_tiers_and_starts_match_the_client() {
        assert_eq!(tier_for(1500.0), Tier::Exalted);
        assert_eq!(tier_for(1499.0), Tier::Good);
        assert_eq!(tier_for(300.0), Tier::Good);
        assert_eq!(tier_for(299.0), Tier::Neutral);
        assert_eq!(tier_for(-300.0), Tier::Neutral);
        assert_eq!(tier_for(-301.0), Tier::Bad);
        assert_eq!(tier_for(-1500.0), Tier::Bad);
        assert_eq!(tier_for(-1501.0), Tier::Evil);
        assert_eq!(starting_score("Paladin"), 500.0);
        assert_eq!(starting_score("Shadow Knight"), -1600.0);
        assert_eq!(starting_score("Warrior"), 0.0);
        assert_eq!(tier_for(starting_score("Necromancer")), Tier::Evil);
    }

    #[test]
    fn momentum_slows_the_climb_away_from_centre_and_never_the_return() {
        assert_eq!(apply(0.0, 25.0), 25.0, "from the centre a delta lands whole");
        assert!((apply(1000.0, 100.0) - 1050.0).abs() < 1e-4, "halfway out, half lands");
        assert!((apply(1000.0, -100.0) - 900.0).abs() < 1e-4, "coming back lands whole");
        assert!((apply(-1000.0, -100.0) - -1050.0).abs() < 1e-4);
        assert_eq!(apply(2000.0, 500.0), 2000.0, "nothing past the cap");
        assert_eq!(apply(1990.0, 5000.0), SCORE_CAP);
        assert_eq!(apply(500.0, 0.0), 500.0);
    }

    #[test]
    fn kills_pay_by_the_victims_tier() {
        assert_eq!(kill_delta(Some("undead"), false), 20.0);
        assert_eq!(kill_delta(Some("undead"), true), 50.0, "The Undying");
        assert_eq!(kill_delta(Some("bad"), false), 10.0, "a gnoll");
        assert_eq!(kill_delta(Some("neutral"), false), 0.0, "a wolf");
        assert_eq!(kill_delta(None, false), 0.0);
        assert_eq!(kill_delta(Some("good"), false), -30.0);
        assert_eq!(kill_delta(Some("innocent"), false), -60.0);
        assert_eq!(kill_delta(Some("evil"), true), 50.0);
    }

    #[test]
    fn a_spell_moves_a_hundred_thousandth_of_its_mana_by_its_school() {
        let smite = spells::lookup("Smite").expect("Smite");
        assert_eq!(spell_sign(smite), 1.0, "holy damage leans good");
        assert!((spell_delta(smite) - 12.0 * SPELL_MANA_FRACTION).abs() < 1e-9, "{}", spell_delta(smite));
        assert_eq!(spell_sign(spells::lookup("Healing Light").unwrap()), 1.0, "a heal leans good");
        assert_eq!(spell_sign(spells::lookup("Bone Shards").unwrap()), -1.0, "shadow damage leans evil");
        assert_eq!(spell_sign(spells::lookup("Fireball").unwrap()), 0.0, "fire leans nowhere");
        assert_eq!(spell_sign(spells::lookup("Blood Price").unwrap()), -1.0, "a blood price leans evil");
        assert_eq!(spell_sign(spells::lookup("Charm").unwrap()), -1.0, "a charm leans evil");
        // Eight thousand Smites move about one point from the centre.
        let mut s = 0.0;
        for _ in 0..8000 {
            s = apply(s, spell_delta(smite));
        }
        assert!((s - 0.96).abs() < 0.01, "{s}");
    }

    #[test]
    fn the_penalty_copy_scales_effects_and_not_costs() {
        let heal = spells::lookup("Crusader's Mend").expect("Crusader's Mend");
        let cut = penalise(heal, 0.7);
        assert!((cut.heal_amount - 7.0).abs() < 1e-5, "{}", cut.heal_amount);
        assert_eq!(cut.mana_cost, heal.mana_cost, "the cost stays");
        assert_eq!(cut.cast_time, heal.cast_time);
        let nuke = spells::lookup("Righteous Fire").expect("Righteous Fire");
        assert!((penalise(nuke, 0.4).base_damage - 2.8).abs() < 1e-5);
    }

    #[test]
    fn the_prestige_classes_and_the_penalty_follow_the_tier() {
        assert_eq!(effective_class("Paladin", -1600.0), "Paladin_Fallen");
        assert_eq!(effective_class("Paladin", -1000.0), "Paladin");
        assert_eq!(effective_class("Shadow Knight", 1600.0), "Shadow Knight_Redeemed");
        assert_eq!(effective_class("Shadow Knight", 500.0), "Shadow Knight");
        assert_eq!(effective_class("Cleric", -1600.0), "Cleric");
        assert_eq!(off_side_multiplier("Paladin", 500.0), 1.0, "a Good Paladin casts whole");
        assert_eq!(off_side_multiplier("Paladin", 0.0), 0.7);
        assert_eq!(off_side_multiplier("Paladin", -1000.0), 0.4);
        assert_eq!(off_side_multiplier("Paladin", -1600.0), 1.0, "a Fallen Paladin has its own kit");
        assert_eq!(off_side_multiplier("Shadow Knight", 0.0), 0.7);
        assert_eq!(off_side_multiplier("Shadow Knight", 1000.0), 0.4);
        assert_eq!(off_side_multiplier("Cleric", 0.0), 1.0);
    }
}
