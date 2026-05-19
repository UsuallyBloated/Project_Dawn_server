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
        _ => None,
    }
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
    fn unknown_pet_type_returns_none() {
        assert!(lookup("eldritch-horror").is_none());
    }
}
