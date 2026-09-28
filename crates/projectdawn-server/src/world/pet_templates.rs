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
            named_id: None,
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
            named_id: None,
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

// ── Interim owner-derived pet leveling ──────────────────────────────────
// pet_levels.md option A (decided 2026-09-19): pets derive their level from
// the owner at summon time and take stats from the same hand-authored ladder
// the phase 4 camps use, scaled down so a pet class is not a duo by itself.
// This whole block is the interim the SWG taming epic (option C) later
// replaces for the Beast Master; summoner classes keep it until the pet
// spell line (option B) arrives.

/// Fraction of the camp stat curve a pet gets. The camps' templates sit ON
/// the curve, so a full-strength owner-1 pet would equal an even-con mob.
/// Playtest tuning knob.
pub const PET_STAT_SCALAR: f32 = 0.70;

/// Summon Skeleton is a single authored spell, so its pet stops scaling
/// here; higher tiers arrive with the pet spell line (option B).
pub const SKELETON_LEVEL_CAP: u32 = 10;

/// The camp ladder's authored (level, hp, dmg) anchors, from
/// `zone_camps.toml` (same-level archetypes averaged; the Ancient Wraith's
/// low-hp speedster archetype excluded so the curve stays monotonic).
/// Linear interpolation between anchors; past the last anchor the 12-to-14
/// slope continues.
const CURVE: &[(u32, f32, f32)] = &[
    (1, 25.0, 3.0),
    (2, 38.0, 5.0),
    (3, 57.0, 7.0),
    (4, 70.0, 9.0),
    (5, 90.0, 10.0),
    (6, 116.0, 12.0),
    (7, 140.0, 15.0),
    (9, 188.0, 20.0),
    (10, 215.0, 22.0),
    (12, 265.0, 26.0),
    (14, 365.0, 35.0),
];

/// (hp, dmg) the camp ladder pays at `level`, interpolated.
fn curve_at(level: u32) -> (f32, f32) {
    let l = level.max(1);
    let (last_level, last_hp, last_dmg) = *CURVE.last().expect("curve non-empty");
    if l >= last_level {
        // Continue the 12-to-14 slope: +50 hp, +4.5 dmg per level.
        let over = (l - last_level) as f32;
        return (last_hp + 50.0 * over, last_dmg + 4.5 * over);
    }
    let mut prev = CURVE[0];
    for &anchor in CURVE {
        if anchor.0 == l {
            return (anchor.1, anchor.2);
        }
        if anchor.0 > l {
            let (l0, hp0, dmg0) = prev;
            let (l1, hp1, dmg1) = anchor;
            let t = (l - l0) as f32 / (l1 - l0) as f32;
            return (hp0 + (hp1 - hp0) * t, dmg0 + (dmg1 - dmg0) * t);
        }
        prev = anchor;
    }
    (prev.1, prev.2)
}

/// Owner-derived pet template. Level = owner minus 1 (floor 1), the
/// skeleton additionally capped at `SKELETON_LEVEL_CAP`, then `variance`
/// subtracted — the EQ re-summon gamble. Callers pass variance 0 for the
/// deterministic warder (a roll on a free 15 s auto-summon is invisible
/// noise) and a 0..=2 roll for manual summons. Identity fields (name,
/// speed, reach, swing timing) stay authored; only level/hp/dmg derive.
pub fn scaled(pet_type: &str, owner_level: u32, variance: u32) -> Option<MobTemplate> {
    let mut t = lookup(pet_type)?;
    let base = owner_level.saturating_sub(1).max(1);
    let capped = match pet_type {
        "skeleton" => base.min(SKELETON_LEVEL_CAP),
        _ => base,
    };
    let level = capped.saturating_sub(variance).max(1);
    let (hp, dmg) = curve_at(level);
    t.level = level;
    t.hp = (hp * PET_STAT_SCALAR).max(10.0);
    t.dmg = ((dmg * PET_STAT_SCALAR).round() as i32).max(1);
    Some(t)
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
        assert!(scaled("eldritch-horror", 20, 0).is_none());
    }

    #[test]
    fn warder_is_deterministic_owner_minus_one() {
        let t = scaled("warder", 22, 0).expect("warder");
        assert_eq!(t.level, 21, "warder = owner - 1, no cap");
        // Identity fields stay authored.
        assert_eq!(t.name, "Wolf");
        assert_eq!(t.speed, 3.5);
    }

    #[test]
    fn skeleton_caps_at_ten() {
        let t = scaled("skeleton", 30, 0).expect("skeleton");
        assert_eq!(t.level, SKELETON_LEVEL_CAP);
    }

    #[test]
    fn variance_lowers_manual_summons() {
        let t = scaled("skeleton", 8, 2).expect("skeleton");
        assert_eq!(t.level, 5, "owner 8 -> base 7, minus the 2 roll");
    }

    #[test]
    fn level_floor_is_one() {
        let t = scaled("warder", 1, 0).expect("warder");
        assert_eq!(t.level, 1);
        let t2 = scaled("skeleton", 2, 2).expect("skeleton");
        assert_eq!(t2.level, 1, "variance can never roll a pet below 1");
    }

    #[test]
    fn pet_stats_sit_below_even_con() {
        // A level-5 pet must be weaker than the level-5 camp mobs it fights
        // beside (Dire Wolf: 88 hp / 10 dmg) — the 70% scalar's whole point.
        let t = scaled("warder", 6, 0).expect("warder");
        assert_eq!(t.level, 5);
        assert!(t.hp < 88.0, "pet hp {} must sit below even-con 88", t.hp);
        assert!(t.dmg < 10, "pet dmg {} must sit below even-con 10", t.dmg);
        assert_eq!(t.hp, 90.0 * PET_STAT_SCALAR);
    }

    #[test]
    fn curve_interpolates_between_anchors() {
        // Level 8 sits between the authored 7 (140/15) and 9 (188/20).
        let (hp, dmg) = curve_at(8);
        assert_eq!(hp, 164.0);
        assert_eq!(dmg, 17.5);
        // Past the last anchor the 12-to-14 slope continues.
        let (hp16, dmg16) = curve_at(16);
        assert_eq!(hp16, 465.0);
        assert_eq!(dmg16, 44.0);
    }
}
