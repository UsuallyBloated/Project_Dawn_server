//! Track 6 sub-task 2 — server-authoritative damage formula.
//!
//! Port of `autoloads/combat.gd::calc_damage` (and its offhand variant)
//! onto Rust. The client may still compute predictively for the local
//! floating-number flash, but the actual HP delta the server applies
//! to the target — and the `amount` it broadcasts in `Hit` — is the
//! one this module returns.
//!
//! The swing is the server's whole verdict (client-authority audit fix 4,
//! 2026-10-19): the miss roll from the weapon skill (5% to 35%, 80% with a
//! weapon the class cannot use; the off hand misses 20 points more; accuracy
//! buffs take points off), the skill damage multiplier (1.0 to 1.15, 0.5 with
//! a weapon the class cannot use), the DEX crit. Until then the client rolled
//! the miss and simply never sent a missed swing, so a modified client never
//! missed, and the skill multiplier existed only on the client. Armor on the
//! target is `mitigated`; spell damage is `spell_power.rs`.

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
/// The miss chance runs from MISS_AT_NO_SKILL down to MISS_FLOOR as the
/// weapon skill nears its cap (`weapon_skills.gd::get_miss_chance`).
const MISS_AT_NO_SKILL: f32 = 0.35;
const MISS_FLOOR: f32 = 0.05;
/// A weapon type the class cannot use (its skill cap is 0) misses this
/// often and hits for SKILL_UNUSABLE_DAMAGE_MULT.
const MISS_UNUSABLE: f32 = 0.80;
const SKILL_UNUSABLE_DAMAGE_MULT: f32 = 0.5;
/// Skill at its cap adds this much damage (`get_damage_multiplier`).
const SKILL_MAX_DAMAGE_BONUS: f32 = 0.15;
/// The off hand misses this much more often, never above OFFHAND_MISS_CAP
/// (`combat.gd`, DUAL_WIELD_MISS_PENALTY).
const DUAL_WIELD_MISS_PENALTY: f32 = 0.20;
const OFFHAND_MISS_CAP: f32 = 0.95;

/// Outcome of one swing — the amount + crit flag the server fans out as
/// `ServerWorldMsg::Hit` and applies to the target's HP.
#[derive(Debug, Clone, Copy)]
pub struct Swing {
    pub amount: i32,
    pub crit: bool,
    /// The swing missed: no damage, a Miss for everyone who can see it.
    pub missed: bool,
}

/// The weapon skill a swing in this hand trains and is judged by: the item's
/// `skill`, or `hand_to_hand` for a bare hand or a weapon with none.
pub fn weapon_skill_key(weapon_path: &str) -> String {
    items::lookup(weapon_path)
        .map(|w| if w.skill.is_empty() { "hand_to_hand".to_string() } else { w.skill.clone() })
        .unwrap_or_else(|| "hand_to_hand".to_string())
}

/// The attacker's score in a weapon skill against its cap for their class
/// and level; `None` when the class cannot use that weapon type at all.
fn skill_ratio(attacker: &PerConnection, key: &str) -> Option<f32> {
    let cap = super::skills::cap_for(super::skills::Skill::Weapon, &attacker.class, attacker.level, key);
    if cap <= 0 {
        return None;
    }
    let score = attacker.weapon_skills.get(key).copied().unwrap_or(0);
    Some((score as f32 / cap as f32).clamp(0.0, 1.0))
}

/// The chance this swing misses, before the roll.
pub fn miss_chance(attacker: &PerConnection, key: &str, is_offhand: bool) -> f32 {
    let base = match skill_ratio(attacker, key) {
        None => MISS_UNUSABLE,
        Some(r) => (MISS_AT_NO_SKILL * (1.0 - r)).max(MISS_FLOOR),
    };
    let (accuracy, _) = buffs::accuracy_crit_bonus(&attacker.active_buffs);
    if is_offhand {
        (base + DUAL_WIELD_MISS_PENALTY - accuracy).clamp(0.0, OFFHAND_MISS_CAP)
    } else {
        (base - accuracy).max(0.0)
    }
}

/// The damage multiplier the weapon skill gives.
pub fn skill_damage_mult(attacker: &PerConnection, key: &str) -> f32 {
    match skill_ratio(attacker, key) {
        None => SKILL_UNUSABLE_DAMAGE_MULT,
        Some(r) => 1.0 + SKILL_MAX_DAMAGE_BONUS * r,
    }
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
    swing_with(attacker, weapon_path, is_offhand, false)
}

/// The same swing at the top of its damage band: what a SEATED target takes
/// (classic EQ, as Project 1999 and EQEmu keep it: a hit on a sitting player
/// cannot miss and lands for the most the attacker can do; user call
/// 2026-10-14). It never rolls a miss. The crit roll is untouched, since a crit is the attacker's
/// luck on top of the roll, not part of it. Pair with `mitigated`, which
/// takes no armor off a seated target.
pub fn calc_swing_max(
    attacker: &PerConnection,
    weapon_path: &str,
    is_offhand: bool,
) -> Swing {
    swing_with(attacker, weapon_path, is_offhand, true)
}

/// Melee damage after the target's armor: `armor / (armor + 100)` of the hit
/// comes off (the client's `Combat.receive_player_damage` formula, kept in
/// step), floored at 1. A SEATED target gets no mitigation at all: the one
/// home of the seated-combat rule's damage half, used by the mob swing and
/// the PvP swing alike.
pub fn mitigated(amount: i32, armor: i32, seated: bool) -> i32 {
    if seated {
        return amount.max(1);
    }
    let armor = armor.max(0) as f32;
    let reduction = armor / (armor + 100.0);
    ((amount as f32 * (1.0 - reduction)) as i32).max(1)
}

fn swing_with(
    attacker: &PerConnection,
    weapon_path: &str,
    is_offhand: bool,
    max_roll: bool,
) -> Swing {
    let mut rng = rand::thread_rng();

    // The miss roll comes first: a missed swing deals nothing. A swing at a
    // seated target (max_roll) cannot miss.
    let skill_key = weapon_skill_key(weapon_path);
    if !max_roll && rng.gen::<f32>() < miss_chance(attacker, &skill_key, is_offhand) {
        return Swing { amount: 0, crit: false, missed: true };
    }

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
            let roll = if max_roll || span == 0 { span } else { rng.gen_range(0..=span) };
            w.damage_min + roll
        }
        _ if max_roll => 4,
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
    // Crit buffs (Hunter's Eye, Anthem of the Hunt) add to the chance; their
    // accuracy half took points off the miss roll above.
    let (_, crit_bonus) = buffs::accuracy_crit_bonus(&attacker.active_buffs);
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

    let skill_mult = skill_damage_mult(attacker, &skill_key);
    let amount = ((base as f32 * skill_mult * crit_mult) as i32).max(1);
    Swing { amount, crit, missed: false }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::CharacterSpawn;
    use std::time::Instant;

    fn make_attacker(str_val: i32, dex_val: i32) -> PerConnection {
        let spawn = CharacterSpawn {
            hunger: 14400.0,
            thirst: 14400.0,
            alignment: None,
            factions: Vec::new(),
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
            completed_quests: Vec::new(),
            active_quests: Vec::new(),
            bind: None,
        };
        PerConnection::from_spawn(spawn, Instant::now())
    }

    /// A swing that landed: the miss roll is the server's now, so the band
    /// tests re-roll past misses (a fresh Warrior with no skill misses 35%).
    fn landed(attacker: &PerConnection, path: &str, offhand: bool) -> Swing {
        for _ in 0..1000 {
            let s = calc_swing(attacker, path, offhand);
            if !s.missed {
                return s;
            }
        }
        panic!("a thousand misses in a row");
    }

    // The seated-combat rule's damage half: the top of the band, no armor.
    #[test]
    fn a_seated_target_takes_the_top_of_the_band_and_no_armor() {
        let attacker = make_attacker(50, 10);
        // Iron short sword 7..=15 plus STR 50/5: the max is 25, every time.
        for _ in 0..50 {
            let swing = calc_swing_max(&attacker, "res://data/loot/items/iron_short_sword.tres", false);
            assert_eq!(swing.amount, 25);
            assert!(!swing.crit);
        }
        // Fists: 4 plus STR 20/5.
        assert_eq!(calc_swing_max(&make_attacker(20, 10), "", false).amount, 8);
        // The offhand keeps its multiplier: floor(25 * 0.8) = 20.
        assert_eq!(calc_swing_max(&attacker, "res://data/loot/items/iron_short_sword.tres", true).amount, 20);
        // Armor takes armor/(armor+100) off a standing target and nothing off
        // a seated one; the floor of 1 holds for both.
        assert_eq!(mitigated(100, 100, false), 50);
        assert_eq!(mitigated(100, 100, true), 100);
        assert_eq!(mitigated(1, 1000, false), 1);
        assert_eq!(mitigated(0, 0, true), 1);
    }

    #[test]
    fn bare_handed_damage_in_range() {
        let attacker = make_attacker(20, 10);
        let swing = landed(&attacker, "", false);
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
            let swing = landed(&attacker, "res://data/loot/items/iron_short_sword.tres", false);
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
            let swing = landed(&attacker, "res://data/loot/items/iron_short_sword.tres", true);
            assert!(swing.amount >= 13 && swing.amount <= 20,
                    "offhand should land 13-20, got {}",
                    swing.amount);
        }
    }

    #[test]
    fn unknown_path_falls_back_to_fists() {
        let attacker = make_attacker(20, 10);
        let swing = landed(&attacker, "res://data/loot/items/nonexistent.tres", false);
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
        let swing = landed(&attacker, "", false);
        // Bare-handed always melee; this is a no-op until ranged
        // weapons land in items.toml.
        assert!(swing.amount > 0);
    }

    // Client-authority audit fix 4: the miss roll and the skill multiplier,
    // the client's weapon_skills.gd numbers.
    #[test]
    fn the_miss_chance_follows_the_weapon_skill() {
        let mut a = make_attacker(20, 10);
        a.weapon_skills.clear();
        let key = "1h_slashing";
        assert!((miss_chance(&a, key, false) - 0.35).abs() < 1e-6, "no skill: 35%");
        assert!((miss_chance(&a, key, true) - 0.55).abs() < 1e-6, "the off hand: 20 points more");
        let cap = super::super::skills::cap_for(super::super::skills::Skill::Weapon, "Warrior", 1, key);
        a.weapon_skills.insert(key.into(), cap);
        assert!((miss_chance(&a, key, false) - 0.05).abs() < 1e-6, "at the cap: the 5% floor");
        assert!((skill_damage_mult(&a, key) - 1.15).abs() < 1e-6, "at the cap: +15% damage");
        // An accuracy buff takes points off, never below zero.
        a.active_buffs.push(super::super::buffs::ActiveBuff::new_accuracy_crit(
            "Hunter's Eye".into(), 0.10, 0.0, 60.0, Instant::now(),
        ));
        assert_eq!(miss_chance(&a, key, false), 0.0);
    }

    #[test]
    fn a_weapon_the_class_cannot_use_misses_eighty_percent_for_half_damage() {
        let mut a = make_attacker(20, 10);
        a.class = "Wizard".into(); // a Wizard's 1h_slashing cap is 0
        assert!((miss_chance(&a, "1h_slashing", false) - 0.80).abs() < 1e-6);
        assert_eq!(skill_damage_mult(&a, "1h_slashing"), 0.5);
    }

    #[test]
    fn a_miss_deals_nothing_and_a_seated_target_is_never_missed() {
        let mut a = make_attacker(50, 10);
        a.weapon_skills.clear();
        let (mut misses, mut swings) = (0, 0);
        for _ in 0..2000 {
            let s = calc_swing(&a, "res://data/loot/items/iron_short_sword.tres", false);
            swings += 1;
            if s.missed {
                misses += 1;
                assert_eq!(s.amount, 0);
                assert!(!s.crit);
            }
        }
        assert!((500..=900).contains(&misses), "about 35% miss: {misses} of {swings}");
        for _ in 0..200 {
            assert!(!calc_swing_max(&a, "res://data/loot/items/iron_short_sword.tres", false).missed);
        }
    }
}

