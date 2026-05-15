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
    /// Track 6 sub-task 4c — movement speed multiplier. The server's
    /// Move integration applies the highest active multiplier to
    /// MAX_MOVE_SPEED. Spirit of Wolf = 1.4, Selos' Melody = 1.35.
    Speed { mult: f32 },
    /// Track 6 sub-task 4c — attack speed buff. Tracked for
    /// snapshot completeness but no server-side behavioral effect:
    /// auto-attack pacing is still client-driven (the client's
    /// `combat.gd::_update_attack_interval` reads BuffManager.haste
    /// locally). Server stores the duration so peers can see "Haste"
    /// in the target frame's buff bar.
    Haste { amount: f32 },
    /// Track 6 sub-task 4c — damage shield (Thorns, Spellshield).
    /// When this connection takes damage, the attacker takes
    /// `amount` damage back. Applied in step 4h/4ha after the
    /// incoming damage lands.
    DamageShield { amount: f32 },
    /// Track 6 sub-task 4c — damage absorption pool (Rune, Primal
    /// Bond). `remaining` is the HP-pool left to absorb; reduced
    /// when incoming damage hits the bearer; buff removed when
    /// the pool reaches 0. Duration is `f32::INFINITY` — absorb
    /// persists until consumed.
    Absorb { pool: f32 },
    /// Track 6 sub-task 4c — accuracy + crit boosts (Hunter's Eye,
    /// Anthem of the Hunt). `combat::calc_swing` reads these from
    /// attacker.active_buffs and adds to its crit chance + reduces
    /// the miss chance.
    AccuracyCrit { accuracy: f32, crit: f32 },
    /// Track 6 sub-task 4d — Mesmerize. Target can't move, cast, or
    /// attack. Server drops Move / CastSpell / Attack intents from a
    /// mezzed player. (Damage-breaks-mez is NOT modelled in 4d —
    /// players stay mezzed for the full duration; that's a follow-up.)
    Mez,
    /// Track 6 sub-task 4d — Root. Target can't move (can still
    /// attack + cast). Server drops Move intents from a rooted
    /// player.
    Root,
    /// Track 6 sub-task 4d — Snare / Slow. Target moves slower.
    /// `amount` is the slowdown fraction (0.5 = 50% slower).
    /// Movement integration multiplies speed by (1 - amount).
    /// Stacking takes the highest active amount.
    Snare { amount: f32 },
    /// Track 6 sub-task 4d — Attack slow. Target attacks slower.
    /// Tracked for snapshot completeness but no server-side
    /// behavioral effect — auto-attack pacing is still client-paced
    /// (same caveat as Haste in sub-task 4c).
    AttackSlow { amount: f32 },
    /// Track 6 sub-task 4d — Silence. Target can't cast spells.
    /// Server drops CastSpell intents from a silenced player.
    Silence,
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

    pub fn new_speed(name: String, mult: f32, duration: f32, now: Instant) -> Self {
        Self {
            name,
            effect: BuffEffect::Speed { mult },
            remaining: duration,
            tick_acc: 0.0,
            applied_at: now,
        }
    }

    pub fn new_haste(name: String, amount: f32, duration: f32, now: Instant) -> Self {
        Self {
            name,
            effect: BuffEffect::Haste { amount },
            remaining: duration,
            tick_acc: 0.0,
            applied_at: now,
        }
    }

    pub fn new_damage_shield(name: String, amount: f32, duration: f32, now: Instant) -> Self {
        Self {
            name,
            effect: BuffEffect::DamageShield { amount },
            remaining: duration,
            tick_acc: 0.0,
            applied_at: now,
        }
    }

    pub fn new_absorb(name: String, pool: f32, now: Instant) -> Self {
        Self {
            name,
            effect: BuffEffect::Absorb { pool },
            remaining: f32::INFINITY,
            tick_acc: 0.0,
            applied_at: now,
        }
    }

    pub fn new_accuracy_crit(name: String, accuracy: f32, crit: f32, duration: f32, now: Instant) -> Self {
        Self {
            name,
            effect: BuffEffect::AccuracyCrit { accuracy, crit },
            remaining: duration,
            tick_acc: 0.0,
            applied_at: now,
        }
    }

    pub fn new_mez(name: String, duration: f32, now: Instant) -> Self {
        Self { name, effect: BuffEffect::Mez, remaining: duration, tick_acc: 0.0, applied_at: now }
    }

    pub fn new_root(name: String, duration: f32, now: Instant) -> Self {
        Self { name, effect: BuffEffect::Root, remaining: duration, tick_acc: 0.0, applied_at: now }
    }

    pub fn new_snare(name: String, amount: f32, duration: f32, now: Instant) -> Self {
        Self {
            name,
            effect: BuffEffect::Snare { amount },
            remaining: duration,
            tick_acc: 0.0,
            applied_at: now,
        }
    }

    pub fn new_attack_slow(name: String, amount: f32, duration: f32, now: Instant) -> Self {
        Self {
            name,
            effect: BuffEffect::AttackSlow { amount },
            remaining: duration,
            tick_acc: 0.0,
            applied_at: now,
        }
    }

    pub fn new_silence(name: String, duration: f32, now: Instant) -> Self {
        Self { name, effect: BuffEffect::Silence, remaining: duration, tick_acc: 0.0, applied_at: now }
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

/// Track 6 sub-task 4c — highest active Speed multiplier (1.0 if no
/// Speed buff). Movement integration multiplies MAX_MOVE_SPEED by
/// this each tick.
pub fn speed_mult(buffs: &[ActiveBuff]) -> f32 {
    let mut max_mult: f32 = 1.0;
    for b in buffs {
        if let BuffEffect::Speed { mult } = b.effect {
            if mult > max_mult {
                max_mult = mult;
            }
        }
    }
    max_mult
}

/// Track 6 sub-task 4c — sum of active damage-shield amounts. When
/// the bearer is hit, the attacker takes this much damage back.
/// Multiple shields stack additively (matches the GDScript buff
/// manager's behaviour: only one damage shield slot, but if the
/// data model is ever extended to allow multiple, this sums them).
pub fn damage_shield_total(buffs: &[ActiveBuff]) -> f32 {
    buffs.iter().filter_map(|b| match b.effect {
        BuffEffect::DamageShield { amount } => Some(amount),
        _ => None,
    }).sum()
}

/// Track 6 — name of the first active DamageShield buff, for combat
/// log attribution ("attacker took N damage from your <shield_name>").
/// Returns the buff's spell name (e.g. "Thorns", "Spellshield").
pub fn first_damage_shield_name(buffs: &[ActiveBuff]) -> Option<&str> {
    buffs.iter().find_map(|b| match b.effect {
        BuffEffect::DamageShield { .. } => Some(b.name.as_str()),
        _ => None,
    })
}

/// Track 6 sub-task 4c — sum of (accuracy, crit) buff bonuses. Used
/// by `combat::calc_swing` to push crit chance up and miss chance
/// down (1 - accuracy_bonus). Both are in 0.0..=1.0 ratio form
/// (Hunter's Eye: 0.15 accuracy / 0.10 crit).
pub fn accuracy_crit_bonus(buffs: &[ActiveBuff]) -> (f32, f32) {
    let mut acc: f32 = 0.0;
    let mut crit: f32 = 0.0;
    for b in buffs {
        if let BuffEffect::AccuracyCrit { accuracy, crit: c } = b.effect {
            acc += accuracy;
            crit += c;
        }
    }
    (acc, crit)
}

/// Track 6 sub-task 4d — true if the bearer is currently Mezzed.
/// Server drops Move / CastSpell / Attack intents from a mezzed
/// player.
pub fn is_mezzed(buffs: &[ActiveBuff]) -> bool {
    buffs.iter().any(|b| matches!(b.effect, BuffEffect::Mez))
}

/// Track 6 sub-task 4d — true if the bearer is currently Rooted.
/// Server drops Move intents (still allows Attack + CastSpell).
pub fn is_rooted(buffs: &[ActiveBuff]) -> bool {
    buffs.iter().any(|b| matches!(b.effect, BuffEffect::Root))
}

/// Track 6 sub-task 4d — true if the bearer is Silenced. Server
/// drops CastSpell intents (still allows Move + Attack).
pub fn is_silenced(buffs: &[ActiveBuff]) -> bool {
    buffs.iter().any(|b| matches!(b.effect, BuffEffect::Silence))
}

/// Track 6 sub-task 4d — highest active snare amount (0.0 if none).
/// Movement integration multiplies effective speed by
/// (1 - snare_amount), clamped to a 10% floor so the player can
/// still inch along.
pub fn snare_amount(buffs: &[ActiveBuff]) -> f32 {
    let mut max_slow: f32 = 0.0;
    for b in buffs {
        if let BuffEffect::Snare { amount } = b.effect {
            if amount > max_slow {
                max_slow = amount;
            }
        }
    }
    max_slow
}

/// Track 6 sub-task 4d — strip the first active dispellable buff
/// from the bearer's active list. Returns the name of the stripped
/// buff (for log + UI feedback) or None if nothing to dispel.
/// Mirror of GDScript `enemy.strip_one_buff()`. Stat buffs need
/// their deltas undone before removal so the caller (apply_dispel
/// in tick.rs) does that.
pub fn first_dispellable_index(buffs: &[ActiveBuff]) -> Option<usize> {
    // Anything except CC-debuffs is dispellable: HoT / MP regen /
    // stat / speed / haste / shield / absorb / accuracy+crit /
    // Lich. Cc / Snare / Silence / Mez / Root / AttackSlow are
    // hostile and shouldn't be stripped by a friendly dispel.
    buffs.iter().position(|b| !matches!(
        b.effect,
        BuffEffect::Mez | BuffEffect::Root | BuffEffect::Snare { .. }
            | BuffEffect::AttackSlow { .. } | BuffEffect::Silence
    ))
}

/// Track 6 sub-task 4c — consume up to `incoming` damage from the
/// first active Absorb buff; returns (remaining damage after
/// absorption, whether the absorb pool is exhausted). The caller
/// removes the buff if the pool reached zero. Matches GDScript
/// `consume_absorb`'s single-shield-pool behaviour.
pub fn consume_absorb(buffs: &mut [ActiveBuff], incoming: i32) -> (i32, bool) {
    if incoming <= 0 {
        return (incoming, false);
    }
    for b in buffs.iter_mut() {
        if let BuffEffect::Absorb { ref mut pool } = b.effect {
            if *pool <= 0.0 {
                continue;
            }
            let absorbed = (incoming as f32).min(*pool);
            *pool -= absorbed;
            let remaining = (incoming as f32 - absorbed).max(0.0) as i32;
            return (remaining, *pool <= 0.0);
        }
    }
    (incoming, false)
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
