//! Track 11 — player-owned pet archetypes. Mirror of the (currently
//! single) PET_SUMMON entry in `Project_Dawn/data/spell_definitions.gd`,
//! looked up by `Spell.pet_type` from `spells.toml`.
//!
//! Pets reuse `MobTemplate` for their stat block (hp / dmg / level /
//! speed / aggro / leash / melee_range / attack_interval) since the
//! shapes are identical and Entity stores them either way.

use super::zones::MobTemplate;

/// Look up a pet template by its `pet_type` string (matches the
/// GDScript `SpellData.pet_type` field). Returns `None` for unknown
/// types — caller logs and drops the cast.
pub fn lookup(pet_type: &str) -> Option<MobTemplate> {
    match pet_type {
        "skeleton" => Some(MobTemplate {
            name: "Skeletal Warrior".into(),
            level: 6,
            hp: 80.0,
            dmg: 8,
            xp: 0,
            speed: 3.0,
            aggro: 0.0,
            leash: None,
            melee_range: Some(1.8),
            attack_interval: Some(2.2),
        }),
        // Track 12 Piece B — Beast Master's Wolf warder. Faster and
        // hits slightly less than the skeleton; the warder's edge is
        // staying alive (Beast Masters can heal it via Spirit Mend)
        // and the death-respawn mechanic that returns it at 30% HP
        // after RETREAT_SECS rather than requiring a re-summon.
        "warder" => Some(MobTemplate {
            name: "Wolf".into(),
            level: 5,
            hp: 60.0,
            dmg: 6,
            xp: 0,
            speed: 3.5,
            aggro: 0.0,
            leash: None,
            melee_range: Some(1.8),
            attack_interval: Some(2.0),
        }),
        _ => None,
    }
}

/// Returns true if a freshly-spawned entity from this template should
/// be treated as a Beast Master warder by the post-death respawn
/// scheduler. Keyed off `mob.name` so the discriminator survives
/// round-trips through the existing `MobTemplate` shape without
/// needing a new enum on Entity.
pub fn is_warder_template(mob_name: &str) -> bool {
    mob_name == "Wolf"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skeleton_resolves() {
        let t = lookup("skeleton").expect("skeleton template");
        assert_eq!(t.name, "Skeletal Warrior");
        assert!(t.hp > 0.0);
    }

    #[test]
    fn warder_resolves_and_is_classified() {
        let t = lookup("warder").expect("warder template");
        assert_eq!(t.name, "Wolf");
        assert!(is_warder_template(&t.name));
        assert!(!is_warder_template("Skeletal Warrior"));
    }

    #[test]
    fn unknown_pet_type_returns_none() {
        assert!(lookup("eldritch-horror").is_none());
    }
}
