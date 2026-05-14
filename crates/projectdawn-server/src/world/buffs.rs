//! Track 6 sub-task 4a — server-authoritative buff state.
//!
//! Holds per-connection active buffs (HoT / MP regen / Lich Form for
//! 4a; DoTs on enemies, stat buffs, speed, haste, shields, CC land in
//! 4b). Mirror of `autoloads/buff_manager.gd`'s per-bucket state but
//! collapsed into a single `Vec<ActiveBuff>` per connection for tick
//! simplicity.
//!
//! The client's BuffManager becomes a render-only cache in launcher
//! mode (sub-task 4a updates the `add_hot` / `add_mp_regen_buff` /
//! `toggle_lich_form` autoload paths to no-op when Net.is_launcher_mode
//! is true). Server fans BuffSnapshot whenever the active set changes;
//! the client uses the existing world_buff_snapshot signal to render.

use serde::Serialize;
use std::time::Instant;

/// Per-tick effect kind. The tick loop matches on this to decide what
/// to mutate on the connection each second.
#[derive(Debug, Clone, Copy, Serialize)]
pub enum BuffEffect {
    /// Heal-over-time. `hps` HP restored per second to the bearer.
    Hot { hps: f32 },
    /// Mana-over-time. `mps` MP restored per second.
    MpRegen { mps: f32 },
    /// Lich Form toggle. Disables natural HP regen, grants
    /// `lich_mp_regen` MP/sec via the regen tick. Stored as a buff so
    /// it shows in BuffSnapshot + naturally expires (cast_time 3s for
    /// the toggle; duration is `f32::INFINITY` for the "on" entry —
    /// it lingers until re-cast clears).
    LichForm { lich_mp_regen: f32 },
    /// Track 6 sub-task 4b — primary stat buff. Adds the listed
    /// deltas to PerConnection stats on apply, subtracts on expire.
    /// Same model as `autoloads/buff_manager.gd::add_primary_stat_buff`:
    /// the buff carries the deltas, not the resulting stats. Combat
    /// math reads conn.strength/etc directly so the buffed value
    /// participates without an extra lookup.
    StatBuff {
        strength: i32,
        agility: i32,
        intelligence: i32,
        wisdom: i32,
        constitution: i32,
        max_hp_delta: f32,
        max_mp_delta: f32,
    },
}

#[derive(Debug, Clone)]
pub struct ActiveBuff {
    pub name: String,
    pub effect: BuffEffect,
    /// Seconds left before the buff expires. `f32::INFINITY` for
    /// toggle-style buffs (Lich Form).
    pub remaining: f32,
    /// Accumulator for sub-integer effects per tick (e.g. 5 HP/s ×
    /// 50 ms = 0.25 HP/tick). Flushes to the resource when ≥ 1.0.
    pub tick_acc: f32,
    /// Wall-clock time of apply. Used for BuffSnapshot duration
    /// computation if we want to surface "time elapsed" later.
    pub applied_at: Instant,
}

impl ActiveBuff {
    pub fn new_hot(name: String, hps: f32, duration: f32, now: Instant) -> Self {
        Self {
            name,
            effect: BuffEffect::Hot { hps },
            remaining: duration,
            tick_acc: 0.0,
            applied_at: now,
        }
    }

    pub fn new_mp_regen(name: String, mps: f32, duration: f32, now: Instant) -> Self {
        Self {
            name,
            effect: BuffEffect::MpRegen { mps },
            remaining: duration,
            tick_acc: 0.0,
            applied_at: now,
        }
    }

    pub fn new_lich_form(name: String, lich_mp_regen: f32, now: Instant) -> Self {
        Self {
            name,
            effect: BuffEffect::LichForm { lich_mp_regen },
            remaining: f32::INFINITY,
            tick_acc: 0.0,
            applied_at: now,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new_stat_buff(
        name: String,
        strength: i32,
        agility: i32,
        intelligence: i32,
        wisdom: i32,
        constitution: i32,
        max_hp_delta: f32,
        max_mp_delta: f32,
        duration: f32,
        now: Instant,
    ) -> Self {
        Self {
            name,
            effect: BuffEffect::StatBuff {
                strength,
                agility,
                intelligence,
                wisdom,
                constitution,
                max_hp_delta,
                max_mp_delta,
            },
            remaining: duration,
            tick_acc: 0.0,
            applied_at: now,
        }
    }
}

/// Returns true if `conn` currently has Lich Form active. The regen
/// tick uses this to skip natural HP regeneration.
pub fn is_lich_form_active(buffs: &[ActiveBuff]) -> bool {
    buffs.iter().any(|b| matches!(b.effect, BuffEffect::LichForm { .. }))
}

/// Track 6 sub-task 4b — apply a StatBuff's deltas to the connection's
/// effective stats. Mirror of `PlayerStats.apply_item_bonuses` /
/// `BuffManager.add_primary_stat_buff`: deltas add directly without
/// re-deriving CON-based max_hp. `max_hp_delta` and `max_mp_delta` are
/// authored explicitly per spell to cover any "this buff also raises
/// max HP" intent.
pub fn apply_stat_deltas(
    conn: &mut super::connection::PerConnection,
    strength: i32,
    agility: i32,
    intelligence: i32,
    wisdom: i32,
    constitution: i32,
    max_hp_delta: f32,
    max_mp_delta: f32,
) {
    conn.strength += strength;
    conn.agility += agility;
    conn.intelligence += intelligence;
    conn.wisdom += wisdom;
    conn.constitution += constitution;
    conn.max_hp = (conn.max_hp + max_hp_delta).max(1.0);
    conn.max_mp = (conn.max_mp + max_mp_delta).max(0.0);
}

/// Track 6 sub-task 4b — reverse a StatBuff's deltas. Called from the
/// buff tick on expire and from `apply_buff` when refreshing an
/// existing stat buff. Clamps current hp / mp against the new max_hp /
/// max_mp so a max-reducing un-apply doesn't leave the player at
/// hp > max.
pub fn undo_stat_deltas(
    conn: &mut super::connection::PerConnection,
    strength: i32,
    agility: i32,
    intelligence: i32,
    wisdom: i32,
    constitution: i32,
    max_hp_delta: f32,
    max_mp_delta: f32,
) {
    conn.strength -= strength;
    conn.agility -= agility;
    conn.intelligence -= intelligence;
    conn.wisdom -= wisdom;
    conn.constitution -= constitution;
    conn.max_hp = (conn.max_hp - max_hp_delta).max(1.0);
    conn.max_mp = (conn.max_mp - max_mp_delta).max(0.0);
    if conn.hp > conn.max_hp {
        conn.hp = conn.max_hp;
    }
    if conn.mp > conn.max_mp {
        conn.mp = conn.max_mp;
    }
}

/// Snapshot the active buffs for BuffSnapshot fan-out. Returns Vec of
/// `(name, remaining_seconds)`. Infinite-duration buffs (Lich Form)
/// surface as a large sentinel (999999.0); the client's HUD treats
/// anything above the typical buff cap as "permanent" visually.
pub fn snapshot_pairs(buffs: &[ActiveBuff]) -> Vec<(String, f32)> {
    buffs
        .iter()
        .map(|b| {
            let dur = if b.remaining.is_infinite() {
                999999.0
            } else {
                b.remaining
            };
            (b.name.clone(), dur)
        })
        .collect()
}
