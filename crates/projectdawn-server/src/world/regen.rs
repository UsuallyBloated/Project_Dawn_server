//! Track 6 sub-task 1 — server-authoritative HP/MP/Stamina regen.
//!
//! Mirror of the GDScript `autoloads/regen.gd` model, ticked on the 20 Hz server
//! clock instead of once per `CLIENT_TICK_INTERVAL_SECS` on the client. The two
//! MUST stay in lockstep so Test Room and launcher show the same numbers.
//!
//! EQ-authentic model (`docs/design/regen_model.md`, 2026-07-20): regen is a flat
//! amount per 6 s tick by level bracket + posture (+ a Troll bonus for HP, +
//! Meditate for MP), NOT stat-scaled — the stat drives the pool size, not the
//! rate. The *sitting* rate applies only when seated AND out of combat
//! (`sitting_bonus_applies`); a seated player who dealt or took damage within
//! `COMBAT_REGEN_LOCKOUT` regens at the standing rate (closes free-in-combat
//! regen). Stamina is a flat 10/tick.
//!
//! Broadcast policy: regen mutates `conn.hp` / `conn.mp` / `conn.stamina`
//! continuously, but a fan-out only fires when one of:
//!   • current value diverged from `last_bcast_*` by > 5 % of max
//!     (big jumps land immediately — e.g. heals, damage)
//!   • at least `MAX_BROADCAST_GAP` elapsed since the last fan-out
//!     (slow trickle still ticks the UI on a clock)
//! Matches `autoloads/net_combat_broadcaster.gd`'s threshold logic so peer
//! HUDs stay responsive without flooding the wire.

use super::connection::PerConnection;
use std::time::{Duration, Instant};

// EQ-authentic regen (docs/design/regen_model.md). Mirrored in the client
// `regen.gd` — RETUNE BOTH TOGETHER. HP/MP come from the level-bracket tables in
// `hp_regen_per_tick` / `mp_regen_per_tick`; stamina is flat.

/// Per-6s-tick stamina regen. Flat, posture-independent.
const STAMINA_REGEN_PER_TICK: f32 = 10.0;

/// The seated (meditation) rate is suppressed for this long after the player last
/// dealt OR took damage — a seated player in combat regens at the standing rate.
const COMBAT_REGEN_LOCKOUT: Duration = Duration::from_secs(6);

/// The regen tick cadence. The server integrates every `TICK_DT` (50 ms) but the
/// model's numbers are per this cadence, so multiply by `dt /
/// CLIENT_TICK_INTERVAL_SECS` to land identical totals over time. Matches the
/// client `regen.gd` `TICK_INTERVAL` (both 6 s).
const CLIENT_TICK_INTERVAL_SECS: f32 = 6.0;

/// 5 % of max — same threshold `net_combat_broadcaster.gd` uses for
/// "big delta, broadcast now."
const DELTA_PCT_OF_MAX: f32 = 0.05;
/// Maximum wall-clock gap between fan-outs while regen is moving the numbers.
pub const MAX_BROADCAST_GAP: Duration = Duration::from_millis(500);

/// HP regen per 6 s tick — flat by level bracket and posture, with the Troll
/// fast-regen bonus (EQ's Troll/Iksar). `sitting` means "use the sitting rate"
/// (seated AND out of combat). See docs/design/regen_model.md.
fn hp_regen_per_tick(level: i32, sitting: bool, troll: bool) -> f32 {
    // (standing, sitting) per tick by (race, level bracket).
    let (standing, seated) = match (troll, level) {
        (false, l) if l <= 19 => (1, 2),
        (false, l) if l <= 49 => (1, 3),
        (false, l) if l <= 50 => (1, 4),
        (false, l) if l <= 55 => (2, 5),
        (false, l) if l <= 59 => (3, 6),
        (false, _) => (4, 7),
        (true, l) if l <= 19 => (2, 4),
        (true, l) if l <= 49 => (2, 6),
        (true, l) if l <= 50 => (2, 8),
        (true, l) if l <= 55 => (6, 12),
        (true, l) if l <= 59 => (10, 16),
        (true, _) => (12, 18),
    };
    (if sitting { seated } else { standing }) as f32
}

/// MP regen per 6 s tick: 1 standing, `2 + floor(meditate / 12)` sitting.
/// `sitting` means "use the sitting rate" (seated AND out of combat). Classes
/// with no mana pool never accumulate (the `mp < max_mp` gate below handles them).
fn mp_regen_per_tick(sitting: bool, meditate: i32) -> f32 {
    if sitting {
        (2 + meditate.max(0) / 12) as f32
    } else {
        1.0
    }
}

/// Result of a single regen tick for one connection — the caller fans out
/// the flagged resources at the end of the tick phase.
#[derive(Default)]
pub struct RegenResult {
    pub hp_fanout: bool,
    pub mp_fanout: bool,
    pub stamina_fanout: bool,
}

/// Does the *sitting* regen rate apply right now? Only when seated AND out of
/// combat: a player who dealt or took damage within `COMBAT_REGEN_LOCKOUT`
/// regens at the standing rate even while seated. Closes the free-in-combat-regen
/// exploit (a seated auto-attacker keeping the sit rate mid-fight).
pub(super) fn sitting_bonus_applies(conn: &PerConnection, now: Instant) -> bool {
    sitting_bonus_from(conn.is_sitting, conn.last_damaged_at, conn.last_attack_at, now)
}

/// The pure decision behind `sitting_bonus_applies`, split out so it can be unit
/// tested without constructing a whole `PerConnection`.
fn sitting_bonus_from(
    is_sitting: bool,
    last_damaged_at: Option<Instant>,
    last_attack_at: Option<Instant>,
    now: Instant,
) -> bool {
    if !is_sitting {
        return false;
    }
    let in_lockout =
        |t: Option<Instant>| t.is_some_and(|t| now.duration_since(t) < COMBAT_REGEN_LOCKOUT);
    !in_lockout(last_damaged_at) && !in_lockout(last_attack_at)
}

/// Apply one tick (`dt` wall-clock seconds since the previous tick) of
/// regen to `conn`. Returns which resources crossed the broadcast
/// threshold; the tick loop fans them out to in-world recipients.
pub fn tick_one(conn: &mut PerConnection, dt: f32, now: Instant) -> RegenResult {
    let mut result = RegenResult::default();
    let scale = dt / CLIENT_TICK_INTERVAL_SECS;
    let sitting = sitting_bonus_applies(conn, now);
    let troll = conn.race == "Troll";
    let meditate = conn.casting_skills.get("meditate").copied().unwrap_or(0);

    // Track 6 sub-task 4a: Lich Form disables natural HP regen. The
    // buff still grants its MP/sec via the buff tick (step 5a).
    let lich_active = super::buffs::is_lich_form_active(&conn.active_buffs);
    if conn.hp > 0.0 && conn.hp < conn.max_hp && !lich_active {
        let per_tick = hp_regen_per_tick(conn.level, sitting, troll);
        conn.regen_hp_acc += per_tick * scale;
        if conn.regen_hp_acc >= 1.0 {
            let delta = conn.regen_hp_acc.floor();
            conn.regen_hp_acc -= delta;
            conn.hp = (conn.hp + delta).min(conn.max_hp);
        }
    } else {
        // Don't accumulate while at full HP, dead, or Lich Form active.
        conn.regen_hp_acc = 0.0;
    }

    if conn.mp < conn.max_mp {
        let per_tick = mp_regen_per_tick(sitting, meditate);
        conn.regen_mp_acc += per_tick * scale;
        if conn.regen_mp_acc >= 1.0 {
            let delta = conn.regen_mp_acc.floor();
            conn.regen_mp_acc -= delta;
            conn.mp = (conn.mp + delta).min(conn.max_mp);
        }
    } else {
        conn.regen_mp_acc = 0.0;
    }

    if conn.stamina < conn.max_stamina {
        conn.regen_stamina_acc += STAMINA_REGEN_PER_TICK * scale;
        if conn.regen_stamina_acc >= 1.0 {
            let delta = conn.regen_stamina_acc.floor();
            conn.regen_stamina_acc -= delta;
            conn.stamina = (conn.stamina + delta).min(conn.max_stamina);
        }
    } else {
        conn.regen_stamina_acc = 0.0;
    }

    // Broadcast threshold. Any one of the three crossing >5% delta OR
    // the max-gap elapsing triggers all three to fan out together —
    // simpler logic, same wire shape as the GDScript broadcaster which
    // bundled all three into a single ResourceUpdate.
    let max_hp = conn.max_hp.max(1.0);
    let max_mp = conn.max_mp.max(1.0);
    let max_stamina = conn.max_stamina.max(1.0);
    let big_delta = (conn.hp - conn.last_bcast_hp).abs() > max_hp * DELTA_PCT_OF_MAX
        || (conn.mp - conn.last_bcast_mp).abs() > max_mp * DELTA_PCT_OF_MAX
        || (conn.stamina - conn.last_bcast_stamina).abs() > max_stamina * DELTA_PCT_OF_MAX;
    let any_change = conn.hp != conn.last_bcast_hp
        || conn.mp != conn.last_bcast_mp
        || conn.stamina != conn.last_bcast_stamina;
    let max_gap_elapsed = conn
        .last_bcast_at
        .map(|t| now.duration_since(t) >= MAX_BROADCAST_GAP)
        .unwrap_or(true);

    if any_change && (big_delta || max_gap_elapsed) {
        result.hp_fanout = true;
        result.mp_fanout = true;
        result.stamina_fanout = true;
        conn.last_bcast_hp = conn.hp;
        conn.last_bcast_mp = conn.mp;
        conn.last_bcast_stamina = conn.stamina;
        conn.last_bcast_at = Some(now);
    }

    result
}

/// Force a broadcast-baseline reset — used when a big external mutation
/// lands (damage from an enemy hit, kill credit, etc.) and the caller
/// wants the next regen tick to definitely emit a fan-out even if the
/// 5 % threshold wouldn't otherwise trigger.
pub fn mark_dirty(conn: &mut PerConnection) {
    conn.last_bcast_at = None;
}

#[cfg(test)]
mod tests {
    use super::*;

    // The sitting rate applies ONLY when seated and out of combat. A player who
    // dealt or took damage within COMBAT_REGEN_LOCKOUT gets the standing rate
    // even while seated — the free-in-combat-regen gate.
    #[test]
    fn sitting_bonus_only_when_seated_and_out_of_combat() {
        // Build times relative to a base so "now - N" never underflows Instant.
        let base = Instant::now();
        let now = base + Duration::from_secs(60);
        let recent = now - Duration::from_secs(1); // inside the lockout
        let old = now - Duration::from_secs(30); // well outside it

        // Standing: never a bonus, regardless of combat timers.
        assert!(!sitting_bonus_from(false, None, None, now));
        assert!(!sitting_bonus_from(false, Some(recent), Some(recent), now));

        // Seated and out of combat: use the sitting rate.
        assert!(sitting_bonus_from(true, None, None, now));
        assert!(sitting_bonus_from(true, Some(old), Some(old), now));

        // Seated but recently dealt damage: standing rate (the exploit case).
        assert!(!sitting_bonus_from(true, None, Some(recent), now));
        // Seated but recently took damage: standing rate.
        assert!(!sitting_bonus_from(true, Some(recent), None, now));
    }

    #[test]
    fn hp_regen_table_by_bracket_posture_race() {
        // Non-Troll: level 1 -> 1 standing / 2 sitting; level 60 -> 4 / 7.
        assert_eq!(hp_regen_per_tick(1, false, false), 1.0);
        assert_eq!(hp_regen_per_tick(1, true, false), 2.0);
        assert_eq!(hp_regen_per_tick(60, false, false), 4.0);
        assert_eq!(hp_regen_per_tick(60, true, false), 7.0);
        // Troll level 60 -> 12 / 18 (the fast column).
        assert_eq!(hp_regen_per_tick(60, false, true), 12.0);
        assert_eq!(hp_regen_per_tick(60, true, true), 18.0);
        // Bracket boundaries: 50 vs 51.
        assert_eq!(hp_regen_per_tick(50, true, false), 4.0);
        assert_eq!(hp_regen_per_tick(51, true, false), 5.0);
    }

    #[test]
    fn mp_regen_standing_is_flat_sitting_scales_with_meditate() {
        assert_eq!(mp_regen_per_tick(false, 240), 1.0); // standing: always 1
        assert_eq!(mp_regen_per_tick(true, 0), 2.0); // sitting, no meditate
        assert_eq!(mp_regen_per_tick(true, 12), 3.0); // +1 per 12 skill
        assert_eq!(mp_regen_per_tick(true, 240), 22.0); // 2 + 20
    }
}
