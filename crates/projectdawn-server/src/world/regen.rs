//! Track 6 sub-task 1 — server-authoritative HP/MP/Stamina regen.
//!
//! Mirror of the GDScript `autoloads/regen.gd` math, ticked on the 20 Hz
//! server clock instead of every 3 s on the client. Constants match the
//! client baseline; sit multiplier applies when `conn.is_sitting` is true.
//! Combat suppression (no regen while engaged) is left to a future track —
//! the server doesn't yet model "in combat" as state (sub-task 3 lands
//! PvP damage which is the natural trigger).
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

// Match GDScript constants. Mirrors are documented in regen.gd's
// `TICK_INTERVAL` / `*_BASE_REGEN` / `*_SCALE` block — kept identical so
// Test Room single-player and launcher mode show the same numbers.
const HP_BASE_REGEN: f32 = 2.0;
const HP_CON_SCALE: f32 = 0.15;
const MP_BASE_REGEN: f32 = 2.0;
const MP_WIS_SCALE: f32 = 0.20;
const ST_BASE_REGEN: f32 = 3.0;
const ST_AGI_SCALE: f32 = 0.10;
const SITTING_HP_MULT: f32 = 5.0;
const SITTING_MP_MULT: f32 = 5.0;
const SITTING_ST_MULT: f32 = 3.0;

/// GDScript regen.gd ticks every 3.0 s; the per-second rate is the value
/// divided by `TICK_INTERVAL`. We tick the server every `TICK_DT` (50 ms),
/// so multiply by `TICK_DT / 3.0` to land identical totals over time.
const CLIENT_TICK_INTERVAL_SECS: f32 = 3.0;

/// 5 % of max — same threshold `net_combat_broadcaster.gd` uses for
/// "big delta, broadcast now."
const DELTA_PCT_OF_MAX: f32 = 0.05;
/// Maximum wall-clock gap between fan-outs while regen is moving the
/// numbers. 500 ms ≈ 2 Hz when seated (~5× HP/sec), enough resolution
/// for a smooth bar without spamming.
pub const MAX_BROADCAST_GAP: Duration = Duration::from_millis(500);

/// Result of a single regen tick for one connection — the caller fans out
/// the flagged resources at the end of the tick phase.
#[derive(Default)]
pub struct RegenResult {
    pub hp_fanout: bool,
    pub mp_fanout: bool,
    pub stamina_fanout: bool,
}

/// Apply one tick (`dt` wall-clock seconds since the previous tick) of
/// regen to `conn`. Returns which resources crossed the broadcast
/// threshold; the tick loop fans them out to in-world recipients.
pub fn tick_one(conn: &mut PerConnection, dt: f32, now: Instant) -> RegenResult {
    let mut result = RegenResult::default();
    let scale = dt / CLIENT_TICK_INTERVAL_SECS;
    let hp_mult = if conn.is_sitting { SITTING_HP_MULT } else { 1.0 };
    let mp_mult = if conn.is_sitting { SITTING_MP_MULT } else { 1.0 };
    let st_mult = if conn.is_sitting { SITTING_ST_MULT } else { 1.0 };

    if conn.hp > 0.0 && conn.hp < conn.max_hp {
        let per_tick = (HP_BASE_REGEN + conn.constitution as f32 * HP_CON_SCALE) * hp_mult;
        conn.regen_hp_acc += per_tick * scale;
        if conn.regen_hp_acc >= 1.0 {
            let delta = conn.regen_hp_acc.floor();
            conn.regen_hp_acc -= delta;
            conn.hp = (conn.hp + delta).min(conn.max_hp);
        }
    } else {
        // Don't accumulate while at full HP or while dead — protects
        // against the seated-at-full-hp case suddenly dumping a buffered
        // amount the moment the player takes damage.
        conn.regen_hp_acc = 0.0;
    }

    if conn.mp < conn.max_mp {
        let per_tick = (MP_BASE_REGEN + conn.wisdom as f32 * MP_WIS_SCALE) * mp_mult;
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
        let per_tick = (ST_BASE_REGEN + conn.agility as f32 * ST_AGI_SCALE) * st_mult;
        conn.regen_stamina_acc += per_tick * scale;
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
