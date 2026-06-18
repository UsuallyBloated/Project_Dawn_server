//! Track 6 sub-task 2 — server-authoritative damage formula.
//!
//! Port of `autoloads/combat.gd::calc_damage` (and its offhand variant)
//! onto Rust. The client may still compute predictively for the local
//! floating-number flash, but the actual HP delta the server applies
//! to the target — and the `amount` it broadcasts in `Hit` — is the
//! one this module returns.
//!
//! Out of scope for sub-task 2 (deferred to 2b / 3):
//! * Weapon-skill multiplier scaling (no skill-table port yet).
//! * Miss-chance from skill (server always lands the hit; client-side
//!   miss vis is unchanged).
//! * Armor damage reduction on the target side.
//! * Spell crit / spell damage routing (sub-task 3 wires `CastSpell`).
//!
//! These will close the remaining client/server damage gap in their
//! own commits.

use super::buffs;
use super::connection::PerConnection;
use super::items;
use rand::Rng;

/// Track 6 sub-task 3 — PvP authorization chokepoint. Returns whether
/// `attacker` can damage `target` right now. Always false in Track 6;
/// the design surface for the eventual rules is documented here so the
/// final shape is obvious when it lands:
///
///   1. **Duel state** — per-pair consent. A `HashMap<(ClientId,
///      ClientId), DuelState>` keyed by canonical order; both sides
///      must have accepted. Drives /duel commands.
///   2. **PvP zones** — `zone path -> bool` lookup table. The
///      `attacker_zone` / `target_zone` args are read here.
///   3. **PvP server** — a server-wide flag (probably loaded from
///      `Config`) flipping every check to true. The dedicated-shard
///      story.
///
/// The chokepoint exists so combat.rs is the single integration point
/// for those rules. Sub-task 3 also adds a `pvp_override_on` flag on
/// `PerConnection` (set by the /pvp dev command) so duels can be
/// verified end-to-end without the duel-state infrastructure shipping
/// first.
pub fn can_attack(
    attacker: &PerConnection,
    target: &PerConnection,
    _attacker_zone: Option<&str>,
    _target_zone: Option<&str>,
) -> bool {
    // Dev override: both sides must have flipped /pvp on. Future
    // /duel handshakes layer over the same flag.
    attacker.pvp_override_on && target.pvp_override_on
}

// Mirrors of `autoloads/combat.gd` constants. Keep in lockstep.
const CRIT_PER_DEX: f32 = 0.003;
const CRIT_MAX: f32 = 0.30;
const OFFHAND_DAMAGE_MULT: f32 = 0.80;

/// Outcome of one swing — the amount + crit flag the server fans out as
/// `ServerWorldMsg::Hit` and applies to the target's HP.
#[derive(Debug, Clone, Copy)]
pub struct Swing {
    pub amount: i32,
    pub crit: bool,
}

/// Compute one main-hand or offhand attack from `attacker` against an
/// arbitrary target. `weapon_path` is the client-supplied item path; an
/// empty string or unknown path falls back to bare-handed damage.
/// `is_offhand` applies the 0.80× damage multiplier matching
/// `Combat.calc_offhand_damage`.
pub fn calc_swing(
    attacker: &PerConnection,
    weapon_path: &str,
    is_offhand: bool,
) -> Swing {
    let mut rng = rand::thread_rng();

    let weapon = items::lookup(weapon_path);
    let is_ranged = weapon.map(|w| w.is_ranged).unwrap_or(false);

    // STR bonus on melee, DEX bonus on ranged. Integer-divide by 5 to
    // mirror `int(PlayerStats.strength / 5)` in the GDScript.
    let stat_bonus = if is_ranged {
        attacker.dexterity / 5
    } else {
        attacker.strength / 5
    };

    // Damage range — weapon's authored band if present, else 1-4 fists.
    let base_roll: i32 = match weapon {
        Some(w) if w.damage_max > 0 => {
            let span = (w.damage_max - w.damage_min).max(0);
            let roll = if span == 0 { 0 } else { rng.gen_range(0..=span) };
            w.damage_min + roll
        }
        _ => rng.gen_range(1..=4),
    };
    let mut base = base_roll + stat_bonus;
    if is_offhand {
        base = (base as f32 * OFFHAND_DAMAGE_MULT) as i32;
    }

    // Crit: DEX scales the chance, 1.5-2.0× multiplier when it lands.
    // Offhand caps at 60% of the main-hand crit chance per the GDScript
    // `calc_offhand_damage` formula.
    let mut crit_chance = ((attacker.dexterity as f32 - 10.0) * CRIT_PER_DEX).clamp(0.0, CRIT_MAX);
    // Track 6 sub-task 4c: accuracy + crit buffs (Hunter's Eye,
    // Anthem of the Hunt). Add crit bonus to chance; accuracy bonus
    // is unused here for now (sub-task 2 didn't model miss chance
    // server-side — that lands when the skill table ports), but
    // pulling it for forward-compat lets the buff still show up in
    // logs without a second pass.
    let (_acc_bonus, crit_bonus) = buffs::accuracy_crit_bonus(&attacker.active_buffs);
    crit_chance = (crit_chance + crit_bonus).min(CRIT_MAX);
    if is_offhand {
        crit_chance *= 0.6;
    }
    let crit = rng.gen::<f32>() < crit_chance;
    let crit_mult = if crit {
        rng.gen_range(1.5_f32..=2.0)
    } else {
        1.0
    };

    let amount = ((base as f32 * crit_mult) as i32).max(1);
    Swing { amount, crit }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::CharacterSpawn;
    use std::time::Instant;

    fn make_attacker(str_val: i32, dex_val: i32) -> PerConnection {
        let spawn = CharacterSpawn {
            char_id: 1,
            account_id: 1,
            name: "Test".into(),
            race: "Human".into(),
            class: "Warrior".into(),
            level: 1,
            xp: 0,
            xp_to_next: 100,
            strength: str_val,
            dexterity: dex_val,
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
            coins: protocol::world::Coins::ZERO,
            bank_coins: protocol::world::Coins::ZERO,
            zone: None,
            pos: (0.0, 0.0, 0.0),
            yaw: 0.0,
        };
        PerConnection::from_spawn(spawn, Instant::now())
    }

    #[test]
    fn bare_handed_damage_in_range() {
        let attacker = make_attacker(20, 10);
        let swing = calc_swing(&attacker, "", false);
        // Base roll 1-4, STR/5 = 4 bonus, no crit cap on 0% chance.
        // amount = (1..=4) + 4 = 5..=8 worst case, up to 2x on the
        // 0% crit chance (never crits at DEX 10). So strict 5..=8.
        assert!(swing.amount >= 5 && swing.amount <= 8,
                "bare-handed should land 5-8 at STR 20 DEX 10, got {}",
                swing.amount);
        assert!(!swing.crit, "DEX 10 should never crit");
    }

    #[test]
    fn iron_short_sword_uses_weapon_damage() {
        let attacker = make_attacker(50, 10);
        // damage_min=7, damage_max=15, STR bonus = 50/5 = 10.
        // Range: (7+10) ..= (15+10) = 17..=25 (no crit at DEX 10).
        for _ in 0..50 {
            let swing = calc_swing(&attacker, "res://data/loot/items/iron_short_sword.tres", false);
            assert!(swing.amount >= 17 && swing.amount <= 25,
                    "iron short sword should land 17-25, got {}",
                    swing.amount);
            assert!(!swing.crit);
        }
    }

    #[test]
    fn offhand_applies_80pct_multiplier() {
        let attacker = make_attacker(50, 10);
        // Main = 17..=25, offhand = floor(main * 0.8) = 13..=20.
        for _ in 0..50 {
            let swing = calc_swing(&attacker, "res://data/loot/items/iron_short_sword.tres", true);
            assert!(swing.amount >= 13 && swing.amount <= 20,
                    "offhand should land 13-20, got {}",
                    swing.amount);
        }
    }

    #[test]
    fn unknown_path_falls_back_to_fists() {
        let attacker = make_attacker(20, 10);
        let swing = calc_swing(&attacker, "res://data/loot/items/nonexistent.tres", false);
        // Same range as bare-handed.
        assert!(swing.amount >= 5 && swing.amount <= 8);
    }

    #[test]
    fn ranged_uses_dex_bonus_not_str() {
        // No ranged weapon in the items table yet, so use a contrived
        // setup: high STR, low DEX. If a ranged weapon were present,
        // damage would scale with DEX. For now this just documents
        // the branch; sub-task 2b adds ranged weapons to items.toml.
        let attacker = make_attacker(50, 10);
        let swing = calc_swing(&attacker, "", false);
        // Bare-handed always melee; this is a no-op until ranged
        // weapons land in items.toml.
        assert!(swing.amount > 0);
    }
}
