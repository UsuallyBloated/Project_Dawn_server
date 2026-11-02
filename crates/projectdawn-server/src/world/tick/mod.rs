//! 20 Hz tick scheduler. Owns the connection map exclusively (no locking)
//! and drives both the renet transport and the application-layer message
//! pipeline.

use super::{
    active_skills,
    aoi::{self, AoiGrid},
    buffs::{self, ActiveBuff},
    clock,
    combat,
    connection::{PerConnection, Vec3f},
    entity::{self, ActiveCc, Entity, EnemyState, HitIntent},
    groups::{self, GroupManager},
    handlers::{self, Outcome},
    inventory,
    items,
    ledger::{self, ItemAction},
    loot::{self, LootBag},
    persistence,
    pet_templates,
    hunger, regen,
    skills,
    spawn_points::Spawner,
    spells,
    ATTACK_RANGE_TOLERANCE, BAN_SWEEP_INTERVAL, CAMP_SECS, CHANNEL_POSITION, CHANNEL_SYSTEM,
    CHECKPOINT_INTERVAL, ENEMY_FAN_REPORT_INTERVAL,
    ENEMY_DESPAWN_LINGER_SECS, GROUP_SHARE_RANGE, INSPECT_RANGE, KICK_FLUSH_GRACE,
    LINKDEAD_SECS,
    LOOT_BAG_LINGER_SECS,
    LOOT_PICKUP_RANGE, RUN_SPEED,
    RANGED_ATTACK_RANGE, STALE_MOVE_THRESHOLD, TICK_DT,
};
use crate::{db, Config};
use protocol::world::{DamageType, EntityId, KickCode};
use rand::Rng;
use renet::{ClientId, RenetServer, ServerEvent};
use renet_netcode::NetcodeServerTransport;
use sqlx::SqlitePool;
use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

mod upkeep;
mod position_fan;
mod regen_tick;
mod death_sweep;
mod camp;
mod buff_tick;
mod movement;
mod looting;
mod quest_steps;
mod binds_and_res;
mod enemy_ai;
mod banking;
mod vendors;
mod inventory_ops;
mod trading;
mod pet_commands;
mod grouping;
mod casting;
mod melee;
mod spawning;
mod fanout;
mod session;
mod intake;
mod state;
use state::*;

/// Cast lifecycle event collected during message dispatch, fanned out
/// after the dispatch loop in a single sweep so we don't need to re-fetch
/// `connections` for each one.
enum CastEvent {
    Start { spell_name: String, duration: f32 },
    Complete { spell_name: String },
    Fail { reason: String },
}

/// Track 6 sub-task 2 — buffered attack intent. Decoded by the handler;
/// the post-dispatch sweep below runs `combat::calc_swing` (server
/// authority on damage roll) and applies the resulting damage to the
/// target — both enemies and (sub-task 3) players.
struct AttackIntent {
    attacker: u64,
    target_id: protocol::world::EntityId,
    // The wire `weapon_path` is intentionally dropped here: the resolver derives
    // the weapon from the server's equipment map, not the client's claim (Phase 1
    // exploit gate, finding 5). `is_offhand` is kept — it selects the equip slot
    // (0 main / 1 off) and applies the off-hand damage penalty.
    is_offhand: bool,
    dmg_type: protocol::world::DamageType,
    /// `Some` when this swing is an active skill's (client-authority audit
    /// item 6): the ordinary main-hand roll times the skill's multiplier,
    /// outside the auto-attack swing-rate limiter (the skill's own cooldown
    /// paces it), with the skill's effect on a landed hit. The stamina and
    /// the cooldown were charged when the intent was resolved.
    skill: Option<SkillSwing>,
}

/// The part of an active skill that rides a swing.
struct SkillSwing {
    mult: f32,
    effect: active_skills::Effect,
    stun_duration: f32,
}

/// A `UseSkill` the handler accepted, resolved in step 4g: the target
/// checked, the stamina and cooldown charged, then a SELF effect applied or
/// a skill swing queued for step 4h.
struct UseSkillIntent {
    user: u64,
    skill_name: String,
    target_id: Option<protocol::world::EntityId>,
}

// ── Melee swing-rate limit (Phase 1 exploit gate) ───────────────────────────
// Player auto-attack is client-paced, so the server must floor how fast the
// SAME hand may swing or a modified client can spam Attack for an attack-speed
// hack. These mirror the client pacing in `autoloads/combat.gd`; keep in step.

/// Fist (empty-hand) attack delay in seconds — the client's fallback when no
/// weapon is equipped (`combat.gd::_get_weapon_delay` returns 2.0).
const FIST_WEAPON_DELAY: f32 = 2.0;
/// Off-hand swings are slower than main-hand by this factor (client
/// `OFFHAND_DELAY_MULT`).
const OFFHAND_DELAY_MULT: f32 = 1.5;
/// The strongest haste the gate honours, however many Haste buffs the server
/// holds for a player: a cap on their sum (the Enchanter's 30% and a song's
/// 10% are the most the data stacks), never an assumption about anyone.
/// Until 2026-10-30 the gate assumed everyone had this much, since player
/// haste was client-paced; a modified client could swing at twice an
/// unhasted honest rate. It reads each player's real haste now.
const MAX_MODELED_HASTE: f32 = 0.5;
/// The client's own floor on a swing interval (`combat.gd`'s `maxf(0.5, ...)`).
const CLIENT_MIN_SWING_SECS: f32 = 0.5;
/// Tolerance subtracted from the computed minimum to absorb the 20 Hz tick, UDP
/// bunching, and Godot timer granularity — so honest jitter never trips it.
/// Sized for the tightest legit case: a fully-hasted player on the fastest
/// weapon swings its main hand every 1.0s, and arrival gaps compress under
/// real-internet jitter (mobile / congested / transcontinental). 0.35s keeps
/// that player's margin comfortably above realistic jitter once the target
/// server is remote (schedule Phase 2), at the cost of letting a forged client
/// swing at most ~1.5x the fully-hasted rate — a bounded residual, since the
/// gate's job is to kill wire-speed spam, not enforce exact cadence.
const SWING_RATE_GRACE_SECS: f32 = 0.35;
/// Absolute per-hand swing floor. The client's `maxf(0.5, ...)` makes any
/// same-hand interval below ~0.5s physically impossible for an honest client, so
/// 0.4s (0.5 minus jitter) is a safe unconditional forgery line.
const HARD_MIN_SWING_SECS: f32 = 0.4;

/// What a player is told when the server shuts down cleanly (a deploy or any
/// `systemctl stop`/`restart`) and their character was saved. Rides
/// `KickCode::Restart`, sent only after the save landed so it is true.
const SHUTDOWN_MESSAGE: &str =
    "The world is being restarted. Your progress has been saved; please log in again in a minute.";
/// The same, for a player whose save did not land within the shutdown budget.
const SHUTDOWN_UNSAVED_MESSAGE: &str =
    "The world is being restarted. Your last few moments could not be saved; please log in again in a minute.";
/// How long a clean shutdown keeps retrying saves that failed, inside
/// systemd's 20 s `TimeoutStopSec`.
const SHUTDOWN_SAVE_BUDGET: Duration = Duration::from_secs(12);

/// How often a logout whose save did not fully land is retried.
const PENDING_SAVE_RETRY: Duration = Duration::from_secs(1);
/// How long a failing logout save is retried before it is given up (logged
/// as an error) and the account may log in again, to its last saved state.
/// That state is whole (drops, trades and loot are saved as they happen), so
/// giving up loses progress but cannot duplicate anything; holding the
/// account forever would lock the player out for the life of the process.
const PENDING_SAVE_GIVE_UP: Duration = Duration::from_secs(120);

/// A character whose logout save did not fully land, and when it was queued.
struct PendingSave {
    conn: PerConnection,
    since: Instant,
}

/// How many DropItem intents one player may have applied per tick. Each
/// dropper's drops are saved in one commit per tick; the cap bounds what a
/// forged burst can make that commit (and the bag fan) carry. An honest
/// client drops one stack per click, far below it.
const MAX_DROPS_PER_TICK: u32 = 4;

/// The fastest same-hand interval an honest client paces at: `weapon_delay`
/// (fists 2.0) times the off-hand multiplier, scaled by the player's haste
/// (`combat.gd::_update_attack_interval`, which reads the Haste buff the
/// server fans; the server sums its Haste buffs, which can only be as
/// lenient as the client's single strongest), floored at the client's own
/// 0.5 s. The client sends one Attack per swing at this cadence with no burst
/// or catch-up, and restarts the interval on a re-target or a toggle, so real
/// arrivals are already >= it.
fn honest_swing_pace(weapon_delay: f32, is_offhand: bool, haste: f32) -> f32 {
    let hand_mult = if is_offhand { OFFHAND_DELAY_MULT } else { 1.0 };
    let haste = haste.clamp(0.0, MAX_MODELED_HASTE);
    (weapon_delay * hand_mult * (1.0 - haste)).max(CLIENT_MIN_SWING_SECS)
}

/// Minimum wall-clock seconds allowed between two SAME-HAND swings before the
/// faster one is a forgery: the honest pace minus a jitter grace, floored at
/// `HARD_MIN_SWING_SECS`. Only ever used to reject "too fast", never "too
/// slow". The pace is computed at `carried_haste`, which covers the client's
/// timer lag; the weapon does not carry: a client alternating a fast and a
/// slow weapon every swing would otherwise swing the slow one at the fast
/// one's pace indefinitely (the 2026-10-30 review); the client re-times its
/// swing when a weapon changes instead.
fn min_swing_interval_secs(pace: f32) -> f32 {
    (pace - SWING_RATE_GRACE_SECS).max(HARD_MIN_SWING_SECS)
}

/// The haste a same-hand swing is paced at: the greatest of the haste now
/// and the haste of the hand's last two ACCEPTED swings (newest first).
/// Two, not one, because the client's attack timer lags by two: a Godot
/// Timer reloads its `wait_time` before the timeout handler runs, so a wait
/// time set in the handler of swing N first times the gap from N+1 to N+2
/// (measured 2026-11-01: a 1.4 s timer changed to 2.0 fired after 1.4, 1.4,
/// then 2.0), and a fresh `start()` after a stop runs the wait time set at
/// the last swing before the stop. So a Haste that ran out costs no honest
/// swing, whether it ends mid-fight or between fights, and a forged client
/// gains at most two swings at the expired haste, since every accepted swing
/// pushes its own haste into the history.
fn carried_haste(haste_now: f32, history: [f32; 2]) -> f32 {
    haste_now.max(history[0]).max(history[1])
}

/// Records an accepted swing's haste in the hand's history, newest first.
fn stamp_swing_haste(history: &mut [f32; 2], haste_now: f32) {
    *history = [haste_now, history[0]];
}

/// True if a same-hand swing arriving `now` is faster than `min_interval` since
/// that hand's last accepted swing (`last`). A hand that has never swung
/// (`None`) is always allowed. Split out so the accept/reject decision is unit
/// testable without the whole attack loop.
fn swing_too_fast(last: Option<Instant>, now: Instant, min_interval: f32) -> bool {
    last.is_some_and(|t| now.duration_since(t).as_secs_f32() < min_interval)
}

/// Track 6 sub-task 3b — buffered spell-cast intent. Server resolves
/// the spell in spells.toml, validates mana / target, and applies
/// damage or heal authoritatively.
///
/// Track 10 — `cast_name_at_dispatch` / `cast_set_at_at_dispatch` are
/// the caster's cast cache state at the moment handle_message decoded
/// this intent. The gate uses them to verify a CastStartBroadcast
/// for this spell actually ran for long enough; snapshotted at
/// dispatch (not gate) time so a same-batch CastCompleteBroadcast
/// doesn't blank the cache before the gate fires.
/// Track 19A — outcome of the on-hit cast-interrupt roll.
/// `Interrupted` means the cast cache was cleared; caller fans
/// `CastFail`. `Survived` means channeling advanced (or didn't);
/// caller fans a `SkillProgressUpdate` when `advanced_to` is Some.
/// `NotCasting` skips both.
enum InterruptOutcome {
    NotCasting,
    /// The cast cache was cleared; `mana_charged` is what the interrupt
    /// cost (spell batch step 3), already taken off the caster.
    Interrupted { spell_name: String, mana_charged: f32 },
    Survived { advanced_to: Option<i32> },
}

/// Track 19A — roll the channeling-based interrupt on a target who
/// just took damage. Mirrors GDScript `Spells.try_interrupt_cast` +
/// `_finish_cast`'s "advance on survival" path. Mutates the conn's
/// cast cache + channeling score in place; caller handles fan-out.
/// `damage_applied` is what the hit really took off the caster: an
/// interrupt charges the spell's mana in proportion to how far the bar
/// had run (`cast_gate::interrupt_charge`), and a hit that did nothing
/// charges nothing.
fn roll_cast_interrupt(conn: &mut PerConnection, now: Instant, damage_applied: i32) -> InterruptOutcome {
    if conn.cast_spell_name.is_empty() || conn.cast_set_at.is_none() {
        return InterruptOutcome::NotCasting;
    }
    let cap = skills::cap_for(
        skills::Skill::Casting,
        &conn.class,
        conn.level,
        "channeling",
    );
    let score = conn.casting_skills.get("channeling").copied().unwrap_or(0);
    let chance = skills::channeling_interrupt_chance(score, cap);
    if rand::thread_rng().gen::<f32>() < chance {
        let spell_name = conn.cast_spell_name.clone();
        // The server's cast time, never the client's bar length: the
        // fraction is elapsed over the spell's own duration.
        let elapsed_ms = conn.cast_set_at.map(|t| now.duration_since(t).as_millis());
        let mana_charged = spells::lookup(&spell_name)
            .map(|s| super::cast_gate::interrupt_charge(s.mana_cost, s.cast_time, elapsed_ms, damage_applied, conn.mp))
            .unwrap_or(0.0);
        if mana_charged > 0.0 {
            conn.mp = (conn.mp - mana_charged).max(0.0);
            regen::mark_dirty(conn);
        }
        conn.cast_spell_name.clear();
        conn.cast_total_duration = 0.0;
        conn.cast_set_at = None;
        // Stamped so a CastSpell for this bar that is already in the tick's
        // queue is refused too (`cast_gate::beaten_by_interrupt`).
        conn.cast_interrupted_at = Some(now);
        InterruptOutcome::Interrupted { spell_name, mana_charged }
    } else {
        let advanced_to = skills::try_advance(conn, skills::Skill::Casting, "channeling");
        InterruptOutcome::Survived { advanced_to }
    }
}

struct CastSpellIntent {
    caster: u64,
    spell_name: String,
    target_id: Option<protocol::world::EntityId>,
    cast_name_at_dispatch: String,
    cast_set_at_at_dispatch: Option<Instant>,
    /// Track 17.2 — caster pos at CastStart, for the movement gate.
    cast_start_pos_at_dispatch: Vec3f,
}

/// Track 6 sub-task 4a — apply an active buff to a connection.
/// Re-cast of a same-named buff refreshes the duration (matches
/// `autoloads/buff_manager.gd::add_hot`'s behaviour). Caller is
/// responsible for fanning a BuffSnapshot afterwards.
fn apply_buff(conn: &mut PerConnection, buff: ActiveBuff) {
    if let Some(existing) = conn
        .active_buffs
        .iter_mut()
        .find(|b| b.name == buff.name)
    {
        existing.effect = buff.effect;
        existing.remaining = buff.remaining;
        existing.tick_acc = 0.0;
        existing.applied_at = buff.applied_at;
    } else {
        conn.active_buffs.push(buff);
    }
}

/// Track 6 sub-task 4b — apply a stat buff, mutating the
/// connection's effective stats. Refresh semantics for re-cast: undo
/// the existing same-named buff's deltas, then apply the new ones.
/// This keeps total stat additions from doubling on refresh.
fn apply_stat_buff(conn: &mut PerConnection, buff: ActiveBuff) {
    // Pull deltas out of the incoming buff.
    let (str_d, agi_d, int_d, wis_d, con_d, hp_d, mp_d) = match buff.effect {
        buffs::BuffEffect::StatBuff {
            strength, agility, intelligence, wisdom, constitution,
            max_hp_delta, max_mp_delta,
        } => (strength, agility, intelligence, wisdom, constitution, max_hp_delta, max_mp_delta),
        _ => return,
    };
    // If an existing same-named stat buff is present, undo its
    // deltas before removing it; preserves the invariant that
    // conn.strength etc. = base + sum(active stat buffs).
    if let Some(idx) = conn.active_buffs.iter().position(|b| b.name == buff.name) {
        if let buffs::BuffEffect::StatBuff {
            strength, agility, intelligence, wisdom, constitution,
            max_hp_delta, max_mp_delta,
        } = conn.active_buffs[idx].effect
        {
            buffs::undo_stat_deltas(
                conn, strength, agility, intelligence, wisdom, constitution,
                max_hp_delta, max_mp_delta,
            );
        }
        conn.active_buffs.remove(idx);
    }
    buffs::apply_stat_deltas(conn, str_d, agi_d, int_d, wis_d, con_d, hp_d, mp_d);
    conn.active_buffs.push(buff);
}

/// Track 6 sub-task 4a fix — apply an MP-regen buff with exclusive
/// semantics. The client's `BuffManager._mp_regen_buff` is a single
/// slot, so casting Clarity after Breeze replaces rather than stacks.
/// Mirror that here by purging any existing MpRegen entry before
/// pushing the new one. Lich Form's MP regen is a separate
/// `BuffEffect::LichForm` variant and isn't touched.
fn apply_mp_regen_exclusive(conn: &mut PerConnection, buff: ActiveBuff) {
    conn.active_buffs
        .retain(|b| !matches!(b.effect, buffs::BuffEffect::MpRegen { .. }));
    conn.active_buffs.push(buff);
}

/// "Latest wins" exclusive apply for DamageShield (Thorns / Spellshield)
/// and Absorb (Rune / Primal Bond). The client's BuffManager tracks
/// these as single slots, so the server matches by purging any
/// existing variant of the same effect kind before pushing the new
/// buff. Keeps both renderings (local buff bar + target HUD)
/// in agreement: one shield, one absorb at a time.
fn apply_damage_shield_exclusive(conn: &mut PerConnection, buff: ActiveBuff) {
    conn.active_buffs
        .retain(|b| !matches!(b.effect, buffs::BuffEffect::DamageShield { .. }));
    conn.active_buffs.push(buff);
}

fn apply_absorb_exclusive(conn: &mut PerConnection, buff: ActiveBuff) {
    conn.active_buffs
        .retain(|b| !matches!(b.effect, buffs::BuffEffect::Absorb { .. }));
    conn.active_buffs.push(buff);
}

/// Apply every beneficial buff a spell grants to one recipient — the
/// caster for a SELF spell, the targeted ally for an ALLY spell. Covers
/// HoT, MP-regen, speed, haste, damage shield, absorb, accuracy/crit, and
/// primary stat buffs. Lich Form and the self heal/damage are NOT here:
/// Lich is a SELF-only toggle and the heal lands before this in each arm.
/// Returns true if the active set changed so the caller fans a
/// BuffSnapshot. `mark_dirty` is called for stat/MP changes so the next
/// regen tick fans the updated max HP/MP caps.
fn apply_player_spell_buffs(conn: &mut PerConnection, spell: &spells::Spell, now: Instant) -> bool {
    let mut changed = false;
    if spell.hot_hps > 0.0 && spell.hot_duration > 0.0 {
        apply_buff(
            conn,
            ActiveBuff::new_hot(spell.name.clone(), spell.hot_hps, spell.hot_duration, now),
        );
        changed = true;
    }
    if spell.mp_regen_hps > 0.0 && spell.mp_regen_duration > 0.0 {
        apply_mp_regen_exclusive(
            conn,
            ActiveBuff::new_mp_regen(
                spell.name.clone(),
                spell.mp_regen_hps,
                spell.mp_regen_duration,
                now,
            ),
        );
        changed = true;
    }
    if spell.move_speed_mult > 0.0 && spell.move_speed_duration > 0.0 {
        apply_buff(
            conn,
            ActiveBuff::new_speed(
                spell.name.clone(),
                spell.move_speed_mult,
                spell.move_speed_duration,
                now,
            ),
        );
        changed = true;
    }
    if spell.haste_amount > 0.0 && spell.haste_duration > 0.0 {
        apply_buff(
            conn,
            ActiveBuff::new_haste(spell.name.clone(), spell.haste_amount, spell.haste_duration, now),
        );
        changed = true;
    }
    if spell.damage_shield_amount > 0.0 && spell.damage_shield_duration > 0.0 {
        apply_damage_shield_exclusive(
            conn,
            ActiveBuff::new_damage_shield(
                spell.name.clone(),
                spell.damage_shield_amount,
                spell.damage_shield_duration,
                now,
            ),
        );
        changed = true;
    }
    if spell.absorb_amount > 0.0 {
        apply_absorb_exclusive(
            conn,
            ActiveBuff::new_absorb(spell.name.clone(), spell.absorb_amount, now),
        );
        changed = true;
    }
    if (spell.accuracy_buff > 0.0 || spell.crit_buff > 0.0) && spell.stat_buff_duration > 0.0 {
        apply_buff(
            conn,
            ActiveBuff::new_accuracy_crit(
                spell.name.clone(),
                spell.accuracy_buff,
                spell.crit_buff,
                spell.stat_buff_duration,
                now,
            ),
        );
        changed = true;
    }
    if spell.primary_stat_buff_duration > 0.0 {
        let any_nonzero = spell.str_buff != 0
            || spell.agi_buff != 0
            || spell.int_buff != 0
            || spell.wis_buff != 0
            || spell.con_buff != 0
            || spell.max_hp_buff != 0.0
            || spell.max_mp_buff != 0.0;
        if any_nonzero {
            apply_stat_buff(
                conn,
                ActiveBuff::new_stat_buff(
                    spell.name.clone(),
                    spell.str_buff,
                    spell.agi_buff,
                    spell.int_buff,
                    spell.wis_buff,
                    spell.con_buff,
                    spell.max_hp_buff,
                    spell.max_mp_buff,
                    spell.primary_stat_buff_duration,
                    now,
                ),
            );
            // max_hp / max_mp may have changed — mark resources dirty so
            // the next regen tick fans HealthUpdate / ManaUpdate.
            regen::mark_dirty(conn);
            changed = true;
        }
    }
    changed
}

/// Track 13 — apply the pet-relevant beneficial buffs a spell grants to a
/// pet (ALLY buff cast on it). Mirrors `apply_player_spell_buffs` minus
/// the effects pets have no model for: mp regen (no mana pool),
/// accuracy/crit (no crit model), absorb (self-only spells). StatBuff
/// mutates the pet's stats + max_hp via `Entity::apply_buff`. Returns true
/// if anything was added so the caller fans a BuffSnapshot.
fn apply_pet_spell_buffs(pet: &mut Entity, spell: &spells::Spell, now: Instant) -> bool {
    let mut changed = false;
    if spell.hot_hps > 0.0 && spell.hot_duration > 0.0 {
        pet.apply_buff(ActiveBuff::new_hot(
            spell.name.clone(), spell.hot_hps, spell.hot_duration, now,
        ));
        changed = true;
    }
    if spell.move_speed_mult > 0.0 && spell.move_speed_duration > 0.0 {
        pet.apply_buff(ActiveBuff::new_speed(
            spell.name.clone(), spell.move_speed_mult, spell.move_speed_duration, now,
        ));
        changed = true;
    }
    if spell.haste_amount > 0.0 && spell.haste_duration > 0.0 {
        pet.apply_buff(ActiveBuff::new_haste(
            spell.name.clone(), spell.haste_amount, spell.haste_duration, now,
        ));
        changed = true;
    }
    if spell.damage_shield_amount > 0.0 && spell.damage_shield_duration > 0.0 {
        pet.apply_buff(ActiveBuff::new_damage_shield(
            spell.name.clone(), spell.damage_shield_amount, spell.damage_shield_duration, now,
        ));
        changed = true;
    }
    if spell.primary_stat_buff_duration > 0.0 {
        let any_nonzero = spell.str_buff != 0 || spell.agi_buff != 0 || spell.int_buff != 0
            || spell.wis_buff != 0 || spell.con_buff != 0 || spell.max_hp_buff != 0.0;
        if any_nonzero {
            pet.apply_buff(ActiveBuff::new_stat_buff(
                spell.name.clone(), spell.str_buff, spell.agi_buff, spell.int_buff,
                spell.wis_buff, spell.con_buff, spell.max_hp_buff, spell.max_mp_buff,
                spell.primary_stat_buff_duration, now,
            ));
            changed = true;
        }
    }
    changed
}

/// Track 6 sub-task 4a — rebuild conn.buff_snapshot from
/// active_buffs and fan a BuffSnapshot to in-world peers. Server is
/// authoritative on buff state now; the client-driven
/// BuffSnapshotBroadcast path is deprecated (kept as a no-op for one
/// release so transitional builds don't crash on the variant).
fn fan_out_server_buff_snapshot(
    server: &mut renet::RenetServer,
    recipients: &[renet::ClientId],
    conn: &PerConnection,
) {
    let payload = buffs::snapshot_pairs(&conn.active_buffs);
    let msg = protocol::world::ServerWorldMsg::BuffSnapshot {
        target: conn.char_id as u64,
        buffs: payload,
    };
    let Ok(bytes) = bincode::serde::encode_to_vec(&msg, bincode::config::standard()) else {
        return;
    };
    for &recipient in recipients {
        server.send_message(recipient, CHANNEL_SYSTEM, bytes.clone());
    }
}

/// Track 13 — fan a BuffSnapshot for a pet's or a mob's status under its
/// id, so the owner + bystanders render its buff bar (the client routes
/// the snapshot by id partition). Mirror of `fan_out_server_buff_snapshot`.
/// Since spell batch step 5 the payload is `Entity::status_pairs`: buffs,
/// DoTs and crowd control, so a mob's target frame shows what is on it.
fn fan_out_pet_buff_snapshot(
    server: &mut renet::RenetServer,
    recipients: &[renet::ClientId],
    pet: &Entity,
) {
    let payload = pet.status_pairs();
    let msg = protocol::world::ServerWorldMsg::BuffSnapshot {
        target: pet.id,
        buffs: payload,
    };
    let Ok(bytes) = bincode::serde::encode_to_vec(&msg, bincode::config::standard()) else {
        return;
    };
    for &recipient in recipients {
        server.send_message(recipient, CHANNEL_SYSTEM, bytes.clone());
    }
}

/// Award a kill to the creditor's group: split the XP pool (with
/// GROUP_XP_BONUS when grouped) among online members and count the kill
/// against each member's active quest objectives (PD_W0024 — the server
/// counts kills now; each increment fans a private `QuestProgress`, which
/// replaced the old `KillCredit`). EQ semantics: the group split applies
/// regardless of HOW the mob died — melee, spell, or pet — so all three kill
/// paths route here. Solo creditor = full base XP. Owned victims (pets /
/// charmed mobs) award nothing — no XP and no quest credit (the warder is
/// literally named "Wolf"). Non-player or fully-disconnected creditors award
/// nothing.
///
/// Eligibility (decided 2026-09-19): an XP share goes only to members who
/// are online, ALIVE, and within `GROUP_SHARE_RANGE` of the dying
/// enemy — the coin split's range rule, plus the alive filter, because a
/// corpse lying beside the mob would otherwise still collect under pure
/// proximity, and classic EQ pays a corpse nothing. Alive means
/// `!death_processed && hp > 0.0`: the death sweep runs at end of tick, so
/// a member dropped by the mob's last swing in the SAME tick as the kill
/// is a corpse by hp before the flag catches up. The kill-creditor is
/// What a kill does to standings and alignment (factions, 2026-10-23), read
/// off the dying entity before its borrow ends: its faction and alignment
/// tier, the hit it is worth, and the players on its hate list.
struct FactionKill {
    faction: Option<String>,
    alignment: Option<String>,
    hit: f32,
    named: bool,
    haters: Vec<ClientId>,
}

impl FactionKill {
    fn of(entity: &Entity) -> Self {
        let named = entity.mob.named_id.is_some();
        Self {
            faction: entity.mob.faction.clone(),
            alignment: entity.mob.alignment.clone(),
            hit: super::factions::kill_hit(entity.mob.level, named, entity.mob.faction_hit),
            named,
            haters: {
                // Everyone the mob hates: whoever damaged it (`aggro`) and
                // whoever it holds threat on (a miss adds a point; a healer's
                // hate rides the threat map), as EQ's hate list.
                let mut ids: Vec<ClientId> = entity
                    .aggro
                    .keys()
                    .chain(entity.threat.keys())
                    .filter(|&&id| id < protocol::world::ENEMY_ID_BASE)
                    .map(|&id| id as ClientId)
                    .collect();
                ids.sort_unstable();
                ids.dedup();
                ids
            },
        }
    }
}

/// exempt from the RANGE half only (a pet owner may legitimately direct a
/// kill from afar, and the coin analogue — the looter — always collects),
/// never from the alive half. The pool divides among eligible members only.
///
/// Quest kill-credit takes the same alive-and-in-range test since
/// 2026-10-05 (user: "Change the XP/journal credit distance to 200m"): at
/// 200 m the gate no longer costs a member chasing a runner their credit,
/// which was the reason journal ticks had stayed unranged at 30 m.
fn award_kill(
    server: &mut RenetServer,
    connections: &mut HashMap<ClientId, PerConnection>,
    group_manager: &groups::GroupManager,
    credit_id: u64,
    base_xp: i32,
    mob_name: &str,
    victim_owned: bool,
    victim_pos: Vec3f,
    faction_kill: FactionKill,
) {
    if base_xp <= 0 || credit_id >= protocol::world::ENEMY_ID_BASE {
        return; // no reward, or the top damager wasn't a player
    }
    // Owned victims (player pets, charmed mobs) award NOTHING — no XP and no
    // quest credit. EQ semantics, and the XP half matters as much as the quest
    // half: a Beast Master's warder respawns free every ~15s, so pet kills
    // paying XP would be an infinite (and group-amplified) leveling loop.
    if victim_owned {
        return;
    }
    let credit_cid = credit_id as ClientId;
    // Factions (2026-10-23, EQ's rule): the kill's hits go to every player
    // on the mob's hate list, grouped or not, in range or not, so a healer
    // who never swung still answers for the kill once the mob hated them.
    if let Some(faction_id) = faction_kill.faction.as_deref() {
        let hits = super::factions::kill_hits(faction_id, faction_kill.hit);
        for hater in &faction_kill.haters {
            if let Some(conn) = connections.get_mut(hater) {
                super::handlers::apply_faction_hits(server, *hater, conn, &hits);
            }
        }
    }
    // Alignment: the kill is the creditor's act (events.md's table by the
    // victim's tier; a named mob counts as its tier's named row).
    if let Some(conn) = connections.get_mut(&credit_cid) {
        let delta = super::alignment::kill_delta(faction_kill.alignment.as_deref(), faction_kill.named);
        super::handlers::move_alignment(server, credit_cid, conn, delta);
    }
    // Alive = neither flagged dead NOR at 0 hp — the flag alone misses a
    // member the mob dropped in this same tick (death sweep runs at end
    // of tick).
    let alive = |c: &PerConnection| !c.death_processed && c.hp > 0.0;
    // XP share: alive + in range (creditor exempt from the range half only).
    let xp_eligible = |cid: ClientId, c: &PerConnection| -> bool {
        alive(c)
            && (cid == credit_cid
                || c.pos.distance_to(victim_pos) <= GROUP_SHARE_RANGE)
    };
    // The candidate set: the group, or the solo creditor. Liveness check on
    // the solo killer too — they may have disconnected (or died to a last
    // exchange of blows) between dealing top damage and the mob dying.
    let group_members: Vec<ClientId> = match group_manager.group_of(credit_cid) {
        Some(g) => g.members.iter().copied().collect(),
        None => vec![credit_cid],
    };
    let xp_members: Vec<ClientId> = group_members
        .iter()
        .filter(|&&m| connections.get(&m).map(|c| xp_eligible(m, c)).unwrap_or(false))
        .copied()
        .collect();
    // Quest kill-credit: the same alive-and-in-range test (see doc comment).
    let quest_members: Vec<ClientId> = xp_members.clone();
    if xp_members.is_empty() && quest_members.is_empty() {
        return;
    }
    // Server-authoritative xp/leveling (Slice 0): every member's share runs
    // through award_xp so leveling stays authoritative.
    let (pool, per_member) = if xp_members.is_empty() {
        (0, 0)
    } else {
        let pool = if xp_members.len() > 1 {
            ((base_xp as f32) * (1.0 + groups::GROUP_XP_BONUS)) as i32
        } else {
            base_xp
        };
        (pool, (pool / xp_members.len() as i32).max(1))
    };
    for m in &xp_members {
        if let Some(conn) = connections.get_mut(m) {
            super::progression::award_xp(server, conn, per_member);
        }
    }
    // PD_W0024 — count the kill against each alive member's active quest
    // objectives (private; witnesses outside the group get none). Counts
    // clamp at the requirement, mirroring the client's old notify_kill;
    // each increment fans a private QuestProgress and marks the quest for
    // the end-of-tick persist flush.
    for m in &quest_members {
        if let Some(conn) = connections.get_mut(m) {
            let mut updates: Vec<(String, u32, i32)> = Vec::new();
            for (quest_id, progress) in conn.active_quests.iter_mut() {
                let Some(quest) = super::quests::lookup(quest_id) else {
                    continue; // normalized at login; stay defensive anyway
                };
                for (i, obj) in quest.objectives.iter().enumerate() {
                    let Some(p) = progress.get_mut(i) else { continue };
                    if *p < obj.count && super::quests::kill_matches(&obj.target, mob_name) {
                        *p += 1;
                        updates.push((quest_id.clone(), i as u32, *p));
                    }
                }
            }
            for (quest_id, index, count) in updates {
                conn.quests_dirty.insert(quest_id.clone());
                super::handlers::send_quest_progress(server, *m, &quest_id, index, count);
            }
        }
    }
    tracing::info!(
        killer = credit_id,
        mob = %mob_name,
        base_xp,
        pool,
        per_member,
        members = xp_members.len(),
        "kill credit granted"
    );
}

/// The one kill step for a mob or pet brought to 0 HP by anything other
/// than a melee swing (spell batch step 5): a direct spell hit, a DoT tick,
/// a damage shield. Marks it dead, fans EntityDied, credits the kill to
/// whoever holds the most aggro (XP, the group split, quest credit),
/// schedules a warder's return, and drops the loot. Idempotent on an
/// entity that is already dead or gone. Before this existed the spell arm
/// carried the sequence inline and the two damage-shield sites skipped it,
/// so a mob finished off by Thorns paid nothing.
/// `source` names the killing blow in the log ("spell kill", "reflect
/// kill", ...), so a scan for one kind keeps working.
#[allow(clippy::too_many_arguments)]
fn kill_enemy(
    server: &mut RenetServer,
    in_world_recipients: &[ClientId],
    connections: &mut HashMap<ClientId, PerConnection>,
    enemies: &mut HashMap<EntityId, Entity>,
    loot_bags: &mut HashMap<EntityId, LootBag>,
    aoi: &mut AoiGrid,
    group_manager: &groups::GroupManager,
    entity_id: EntityId,
    source: &str,
    now: Instant,
) {
    let (credit_id_opt, mob_xp, mob_level, death_pos, mob_name, warder_owner_opt, victim_owned, faction_kill) = {
        let Some(entity) = enemies.get_mut(&entity_id) else {
            return;
        };
        if !entity.is_alive() {
            return;
        }
        entity.transition(EnemyState::Dead, now);
        handlers::fan_out_entity_died(server, in_world_recipients, entity_id);
        let credit_id_opt = entity
            .aggro
            .iter()
            .max_by(|a, b| a.1.partial_cmp(b.1).unwrap_or(std::cmp::Ordering::Equal))
            .map(|(&id, _)| id);
        // EQ quadratic per-kill award, derived from the mob's level (not the
        // legacy flat mob.xp constant). See progression::kill_xp.
        let mob_xp = super::progression::kill_xp(entity.mob.level as i32, super::progression::ZEM_NORMAL);
        let warder_owner_opt: Option<EntityId> =
            if super::pet_templates::is_warder_template(&entity.mob.name) {
                entity.owner
            } else {
                None
            };
        // Owned victims (pets, charmed mobs) must not grant quest kill
        // credit — the warder is literally named "Wolf", so a respawning
        // PvP warder would farm the wolf quest otherwise.
        let victim_owned = entity.owner.is_some();
        let faction_kill = FactionKill::of(entity);
        (credit_id_opt, mob_xp, entity.mob.level, entity.pos, entity.mob.name.clone(), warder_owner_opt, victim_owned, faction_kill)
    };

    // Warder respawn — mirrors the melee path. If the dying entity is a
    // Beast Master warder, schedule the owner's warder_respawn_at so the
    // warder-respawn sweep brings it back.
    if let Some(owner_id) = warder_owner_opt {
        const WARDER_RETREAT_SECS: f32 = 15.0;
        let due = now + std::time::Duration::from_secs_f32(WARDER_RETREAT_SECS);
        let owner_cid = owner_id as ClientId;
        if let Some(conn) = connections.get_mut(&owner_cid) {
            conn.warder_respawn_at = Some(due);
            tracing::info!(
                owner = owner_cid as u64,
                killer = ?credit_id_opt,
                retreat_secs = WARDER_RETREAT_SECS,
                "warder retreating after kill",
            );
        }
    }
    if let Some(credit_id) = credit_id_opt {
        // Every kill uses the same group XP split + quest credit as a melee
        // kill (EQ semantics: the split is method-agnostic).
        award_kill(
            server,
            connections,
            group_manager,
            credit_id,
            mob_xp,
            &mob_name,
            victim_owned,
            death_pos,
            faction_kill,
        );
    }
    // An owned victim (a pet, a charmed mob) drops nothing, as it pays no
    // XP: a warder is "Wolf" with a wolf's loot table and returns free every
    // 15 s, so two flagged accounts could farm its bag (the review of step 5
    // found the melee path had always rolled it).
    if victim_owned {
        return;
    }
    let loot_items = loot::roll_for_mob(&mob_name).unwrap_or_default();
    let loot_coins = loot::roll_coin_for_mob(&mob_name, mob_level);
    if !loot_items.is_empty() || loot_coins != protocol::world::Coins::ZERO {
        // Loot ownership: the kill-creditor owns the corpse; their group
        // shares rights, resolved at loot time.
        let owner_cid_opt = credit_id_opt
            .filter(|&id| id < protocol::world::ENEMY_ID_BASE)
            .map(|id| id as ClientId);
        let bag = LootBag::new(death_pos, loot_items, loot_coins, mob_name.clone(), owner_cid_opt, now);
        let bag_id = bag.id;
        let bag_cell = aoi::cell_for(bag.pos.x, bag.pos.z);
        aoi.insert(bag_id, bag_cell);
        let visible = aoi.entities_visible_from(bag_cell);
        let bag_recipients: Vec<ClientId> = in_world_recipients
            .iter()
            .copied()
            .filter(|id| visible.contains(id))
            .collect();
        if !bag_recipients.is_empty() {
            handlers::fan_out_loot_bag_spawn(server, &bag_recipients, &bag);
        }
        loot_bags.insert(bag.id, bag);
        tracing::info!(mob = %mob_name, bag_id, "loot bag spawned ({source})");
    }
}

/// Track 9 — apply a single spell hit to one enemy. Shared by the
/// single-target ENEMY arm and the AOE fan-out so the damage / death /
/// kill-credit / loot / CC sequence stays in one place. Caller is
/// responsible for range checks; this just applies the hit to the
/// `target_id` enemy if it exists and is alive.
///
/// Returns `true` if damage landed (entity existed and was alive at
/// entry), `false` otherwise.
#[allow(clippy::too_many_arguments)]
fn apply_spell_damage_to_enemy(
    server: &mut RenetServer,
    in_world_recipients: &[ClientId],
    connections: &mut HashMap<ClientId, PerConnection>,
    enemies: &mut HashMap<EntityId, Entity>,
    loot_bags: &mut HashMap<EntityId, LootBag>,
    aoi: &mut AoiGrid,
    group_manager: &groups::GroupManager,
    caster_id: u64,
    target_id: EntityId,
    spell: &spells::Spell,
    dmg_type: DamageType,
    now: Instant,
) -> bool {
    // The spell crit (spell_power.rs), rolled per hit off the caster's INT;
    // a spell that deals no damage (a pure mez or root) has nothing to crit.
    let caster_int = connections
        .get(&(caster_id as ClientId))
        .map(|c| c.intelligence)
        .unwrap_or(0);
    let (died, damage_done) = {
        let Some(entity) = enemies.get_mut(&target_id) else {
            return false;
        };
        if !entity.is_alive() {
            return false;
        }
        let flat = spell.base_damage.max(0.0) as i32;
        let (dmg, crit) = if flat > 0 {
            super::spell_power::roll_hit(flat, caster_int, &mut rand::thread_rng())
        } else {
            (0, false)
        };
        let hp_before = entity.hp;
        entity.hp = (entity.hp - dmg as f32).max(0.0);
        let damage_done = (hp_before - entity.hp).max(0.0);
        *entity.aggro.entry(caster_id).or_insert(0.0) += dmg as f32;
        // Track 12 Piece A2 — caster threat mirrors aggro for the
        // re-target check.
        *entity.threat.entry(caster_id).or_insert(0.0) += dmg as f32;
        let entity_id = entity.id;
        let entity_hp = entity.hp;
        let entity_max_hp = entity.max_hp;
        let died = entity.hp <= 0.0;

        handlers::fan_out_hit(
            server,
            in_world_recipients,
            caster_id,
            target_id,
            dmg,
            crit,
            dmg_type,
        );
        handlers::fan_out_health_update(
            server,
            in_world_recipients,
            entity_id,
            entity_hp,
            entity_max_hp,
        );

        if died {
            (true, damage_done)
        } else {
            if dmg > 0 {
                entity.clear_mez();
            }
            // Damage over time (spell batch step 5): the direct hit above
            // is the cast's own; the DoT ticks from here, credited to the
            // caster, replacing an earlier cast of the same spell.
            if spell.dot_dps > 0.0 && spell.dot_duration > 0.0 {
                entity.apply_dot(entity::ActiveDot {
                    name: spell.name.clone(),
                    caster: caster_id,
                    dps: spell.dot_dps,
                    dmg_type,
                    remaining: spell.dot_duration,
                    tick_acc: 0.0,
                    ticks_left: 0,
                });
            }
            if spell.cc_duration > 0.0 {
                entity.apply_cc(ActiveCc::new_mez(spell.cc_duration));
            }
            if spell.root_duration > 0.0 {
                entity.apply_cc(ActiveCc::new_root(spell.root_duration));
            }
            if spell.slow_amount > 0.0 && spell.slow_duration > 0.0 {
                entity.apply_cc(ActiveCc::new_snare(spell.slow_amount, spell.slow_duration));
            }
            if spell.attack_slow_amount > 0.0 && spell.attack_slow_duration > 0.0 {
                entity.apply_cc(ActiveCc::new_attack_slow(
                    spell.attack_slow_amount,
                    spell.attack_slow_duration,
                ));
            }
            (false, damage_done)
        }
    };

    // Track 14 follow-up — lifesteal. Spells with both base_damage
    // and heal_amount (e.g. Lifetap, Soul Drain, Exsanguinate) heal
    // the caster for `min(heal_amount, damage_done)`. Capping at
    // damage_done matches the GDScript reference and stops a low-HP
    // mob from over-healing the caster. Skipped when the caster
    // isn't a connected player (NPC casters don't have HP we track).
    if spell.heal_amount > 0.0 && damage_done > 0.0 {
        let heal = spell.heal_amount.min(damage_done);
        let caster_cid = caster_id as ClientId;
        if let Some(caster) = connections.get_mut(&caster_cid) {
            let prev_hp = caster.hp;
            caster.hp = (caster.hp + heal).min(caster.max_hp);
            if caster.hp != prev_hp {
                handlers::fan_out_health_update(
                    server,
                    in_world_recipients,
                    caster.char_id as u64,
                    caster.hp,
                    caster.max_hp,
                );
            }
        }
    }

    if died {
        kill_enemy(
            server,
            in_world_recipients,
            connections,
            enemies,
            loot_bags,
            aoi,
            group_manager,
            target_id,
            "spell kill",
            now,
        );
    }

    true
}

/// Track 12 Piece B — spawn a player-owned pet at `spawn_pos`,
/// despawning the owner's existing pet first. Used by the
/// PET_SUMMON CastSpell arm, the Beast Master auto-summon on
/// EnterWorld, and the warder death-respawn sweep. `hp_fraction`
/// is 1.0 for fresh summons and 0.3 for the warder return.
/// Returns the new pet's id.
#[allow(clippy::too_many_arguments)]
/// Owner-derived summon wrapper (pet interim A): resolves the scaled
/// template for `pet_type` at the owner's level, applies the standard
/// +1.5 m east spawn offset, and delegates to `summon_pet_for_owner`.
/// Returns `(pet_id, pet_level)`, or `None` for an unknown `pet_type`.
/// The auto-summon, retreat-respawn and manual-summon paths all route
/// through here, so the next pet rule change happens ONCE instead of in
/// three near-identical blocks.
#[allow(clippy::too_many_arguments)]
fn summon_scaled_pet(
    server: &mut RenetServer,
    in_world_recipients: &[ClientId],
    enemies: &mut HashMap<EntityId, Entity>,
    aoi: &mut AoiGrid,
    owner_id: EntityId,
    owner_level: u32,
    owner_pos: Vec3f,
    pet_type: &str,
    variance: u32,
    hp_fraction: f32,
    now: Instant,
) -> Option<(EntityId, u32)> {
    let template = pet_templates::scaled(pet_type, owner_level, variance)?;
    let pet_level = template.level;
    // Spawn at the owner's pos + 1.5 m east so the pet doesn't clip the
    // player capsule.
    let spawn_pos = Vec3f {
        x: owner_pos.x + 1.5,
        y: owner_pos.y,
        z: owner_pos.z,
    };
    let pet_id = summon_pet_for_owner(
        server,
        in_world_recipients,
        enemies,
        aoi,
        owner_id,
        spawn_pos,
        template,
        hp_fraction,
        now,
    );
    Some((pet_id, pet_level))
}

fn summon_pet_for_owner(
    server: &mut RenetServer,
    in_world_recipients: &[ClientId],
    enemies: &mut HashMap<EntityId, Entity>,
    aoi: &mut AoiGrid,
    owner_id: EntityId,
    spawn_pos: Vec3f,
    template: crate::world::zones::MobTemplate,
    hp_fraction: f32,
    now: Instant,
) -> EntityId {
    // Despawn existing pet first (one-pet-per-owner). Mark Dead +
    // fan EntityDied; corpse cleanup runs naturally next tick.
    let existing_pet_id: Option<EntityId> = enemies
        .iter()
        .find(|(_, e)| e.owner == Some(owner_id) && e.is_alive())
        .map(|(id, _)| *id);
    if let Some(old_id) = existing_pet_id {
        if let Some(old) = enemies.get_mut(&old_id) {
            old.transition(EnemyState::Dead, now);
        }
        handlers::fan_out_entity_died(server, in_world_recipients, old_id);
    }

    let mut pet = Entity::from_pet_summon(owner_id, spawn_pos, template, now);
    let new_hp = (pet.max_hp * hp_fraction.clamp(0.0, 1.0)).max(1.0);
    pet.hp = new_hp;
    let pet_id = pet.id;
    let pet_cell = aoi::cell_for(pet.pos.x, pet.pos.z);
    aoi.insert(pet_id, pet_cell);
    let visible = aoi.entities_visible_from(pet_cell);
    let pet_recipients: Vec<ClientId> = in_world_recipients
        .iter()
        .copied()
        .filter(|id| visible.contains(id))
        .collect();
    if !pet_recipients.is_empty() {
        handlers::fan_out_pet_spawn(server, &pet_recipients, &pet, None);
    }
    enemies.insert(pet_id, pet);
    pet_id
}

/// Track 5 sub-task 4 — buffered loot pickup intent. `Slot(None)` is
/// the "take everything" variant; `Slot(Some(idx))` is the
/// "take one specific slot" variant. Same buffering rationale as
/// AttackIntent.
struct LootIntent {
    looter: u64,
    bag_id: protocol::world::EntityId,
    slot: Option<u32>,
}

/// Track 4 sub-task 4 combat event. Same shape as CastEvent — one-shot
/// per arrival, not coalesced, ordered.
enum CombatEvent {
    Hit {
        target: u64,
        amount: i32,
        crit: bool,
        dmg_type: protocol::world::DamageType,
    },
    Miss {
        target: u64,
    },
    Evade {
        target: u64,
    },
}

/// The owner always sees their own pet, wherever either of them stands
/// (user call, 2026-10-02). A pet is never despawned for its owner by a loss
/// of view, its Position fans to the owner outside the AOI neighbourhood,
/// and the real end of it (charm expiry, the body expiring) reaches the owner
/// wherever they are. The alternative was a pet parked with `/pet guard`
/// vanishing from its owner's HUD, and every `/pet` command with it, once
/// the owner walked two cells away. Returns the owner's client id for a pet,
/// None for anything else.
fn pet_owner_cid(entity: &Entity) -> Option<ClientId> {
    if entity.is_pet() {
        entity.owner.map(|o| o as ClientId)
    } else {
        None
    }
}

/// Who is told an entity is gone for real: the in-world players who could
/// see its cell, plus a pet's owner wherever they are (`pet_owner_cid`).
/// The room: every in-world player within `SAY_RANGE` of `speaker`, never the
/// speaker. What a `/say` and the eating emote reach (the jump relay reaches
/// the jumper's AOI view instead, since 2026-10-29: a jump is seen, not
/// heard). Written keep-only-when-in-range, so a non-finite position hears
/// nothing.
fn say_recipients(speaker: ClientId, connections: &HashMap<ClientId, PerConnection>) -> Vec<ClientId> {
    let Some(from) = connections.get(&speaker) else {
        return Vec::new();
    };
    connections
        .iter()
        .filter(|(id, c)| **id != speaker && c.in_world && c.pos.distance_to(from.pos) <= super::SAY_RANGE)
        .map(|(id, _)| *id)
        .collect()
}

/// Wall-clock unix seconds (the logout stamp for NO RENT items; the ledger's
/// rows are stamped by the same clock).
fn unix_now_secs() -> i64 {
    ledger::unix_now_secs()
}

/// Track 18.1 — a swing at a mob trains the hand's weapon skill and defense,
/// hit or miss, and an off-hand swing trains dual wield too (as the client
/// always did); tells the attacker privately when a roll lands. The skill key
/// comes from `combat::weapon_skill_key` (the item's skill, or hand_to_hand).
fn train_swing_skills(
    server: &mut RenetServer,
    connections: &mut HashMap<ClientId, PerConnection>,
    attacker_cid: ClientId,
    weapon_skill_key: &str,
    is_offhand: bool,
) {
    let Some(att) = connections.get_mut(&attacker_cid) else {
        return;
    };
    let keys: &[&str] = if is_offhand {
        &[weapon_skill_key, "defense", "dual_wield"]
    } else {
        &[weapon_skill_key, "defense"]
    };
    for &key in keys {
        if let Some(new_score) = skills::try_advance(att, skills::Skill::Weapon, key) {
            handlers::send_skill_progress_update(
                server,
                attacker_cid,
                skills::Skill::Weapon.as_protocol(),
                key.to_string(),
                new_score,
            );
        }
    }
}

fn despawn_recipients(
    visible: &std::collections::HashSet<EntityId>,
    in_world: &[ClientId],
    entity: &Entity,
) -> Vec<ClientId> {
    let owner_cid = pet_owner_cid(entity);
    in_world
        .iter()
        .copied()
        .filter(|cid| visible.contains(cid) || owner_cid == Some(*cid))
        .collect()
}

/// Fan what an enemy or pet crossing an AOI cell implies: in-world players in
/// the cells its 3x3 neighbourhood gained get its spawn message, players in
/// the cells it lost get an EntityDespawn. Mirror of the player cell-change
/// fan-out in step 5b. Without it a mob chasing into view only ever streamed
/// Position for an id the client was never given a spawn for, which the
/// client drops (it has no name, level or hp to build a placeholder from), so
/// the mob stayed invisible until a re-enter. Callers pass live entities:
/// only alive ones take the AI pass that moves them between cells. A pet's
/// owner is left out of both lists: they never lost it.
fn fan_entity_cell_crossing(
    server: &mut RenetServer,
    aoi: &AoiGrid,
    connections: &HashMap<ClientId, PerConnection>,
    entity: &Entity,
    gained_cells: &[aoi::Cell],
    lost_cells: &[aoi::Cell],
) {
    let owner_cid = pet_owner_cid(entity);
    let in_world_players = |cells: &[aoi::Cell]| -> Vec<ClientId> {
        aoi.entities_in_cells(cells.iter())
            .into_iter()
            .filter(|id| *id < protocol::world::ENEMY_ID_BASE)
            .map(|id| id as ClientId)
            .filter(|cid| owner_cid != Some(*cid))
            .filter(|cid| connections.get(cid).is_some_and(|c| c.in_world))
            .collect()
    };
    handlers::fan_out_entity_spawn(server, &in_world_players(gained_cells), entity);
    for cid in in_world_players(lost_cells) {
        handlers::send_entity_despawn(server, cid, entity.id);
    }
}

/// Move a connection's queued audit entries into `out` as `gm_actions` rows.
/// Commands refused over budget are summarized as one `audit_overflow` row,
/// written at most once a second per connection (`force` writes it now: the
/// connection is leaving), so a flood is recorded as a flood without the
/// summary itself becoming a write per tick.
fn collect_gm_audit(
    conn: &mut PerConnection,
    now: Instant,
    force: bool,
    out: &mut Vec<db::GmActionRow>,
) {
    let (actor_account, target_char) = (conn.account_id, conn.char_id);
    for entry in conn.gm_audit.drain(..) {
        out.push(db::GmActionRow {
            actor_account,
            target_char,
            command: entry.command.to_string(),
            args: entry.args,
        });
    }
    if conn.gm_cmd_refused > 0 {
        let due = force
            || conn
                .gm_overflow_row_at
                .is_none_or(|t| now.saturating_duration_since(t).as_secs_f32() >= 1.0);
        if due {
            out.push(db::GmActionRow {
                actor_account,
                target_char,
                command: "audit_overflow".to_string(),
                args: format!("refused={}", conn.gm_cmd_refused),
            });
            conn.gm_cmd_refused = 0;
            conn.gm_overflow_row_at = Some(now);
        }
    }
}

/// Write collected audit rows in one transaction. A failed write is logged
/// with its rows and dropped; the per-command info lines in the server log
/// remain as the fallback record.
async fn write_gm_audit(pool: &SqlitePool, rows: &[db::GmActionRow]) {
    if rows.is_empty() {
        return;
    }
    if let Err(e) = db::record_gm_actions(pool, rows).await {
        tracing::error!(error = %e, rows = ?rows, "gm audit write failed — rows dropped");
    }
}

/// Despawn the pet(s) owned by `owner_entity`: fan EntityDespawn to the AOI
/// peers who could see each pet, then drop it from the grid and the enemies
/// map. A charmed mob is not dropped but handed back (`return_charmed_mob`),
/// walking home since the charmer is gone. Idempotent — a second
/// call after the pets are already gone is a no-op. Called both when a player
/// goes linkdead (the body lingers but the pet can't be commanded) and from
/// the final reap.
fn despawn_owned_pets(
    server: &mut RenetServer,
    connections: &HashMap<ClientId, PerConnection>,
    aoi: &mut AoiGrid,
    enemies: &mut HashMap<EntityId, Entity>,
    owner_entity: EntityId,
    owner_client_id: ClientId,
    now: std::time::Instant,
) {
    // Track 11 — owner's pet dies with them. Future work: hand off pets on
    // zone change rather than instant despawn.
    let owned_pets: Vec<EntityId> = enemies
        .iter()
        .filter(|(_, e)| e.owner == Some(owner_entity))
        .map(|(id, _)| *id)
        .collect();
    // The leaver's transport is going or gone, so they are not an audience.
    let in_world: Vec<ClientId> = connections
        .iter()
        .filter(|(id, c)| **id != owner_client_id && c.in_world)
        .map(|(id, _)| *id)
        .collect();
    for pet_id in owned_pets {
        let Some(pet) = enemies.remove(&pet_id) else {
            continue;
        };
        if pet.charm_expires_at.is_some() {
            let mob_id = return_charmed_mob(server, aoi, enemies, &in_world, pet, None, now);
            tracing::info!(owner = owner_entity, pet_id, mob_id, "charm broken by owner leaving — mob returned");
            continue;
        }
        let pet_cell = aoi::cell_for(pet.pos.x, pet.pos.z);
        aoi.remove(pet_id, pet_cell);
        let pet_visible = aoi.entities_visible_from(pet_cell);
        for peer_id in in_world.iter().copied().filter(|id| pet_visible.contains(id)) {
            handlers::send_entity_despawn(server, peer_id, pet_id);
        }
        tracing::info!(owner = owner_entity, pet_id, "pet despawned on owner disconnect");
    }
}

/// A charm ending, by expiry or by the charmer leaving: the pet (already
/// taken out of `enemies` by the caller) is despawned for everyone who saw it,
/// and the mob it was comes back in its place, into the camp slot the charm
/// carried, at the pet's position with the pet's HP (EQ's rule, user call
/// 2026-10-06). With `turn_on` set it comes straight for its former charmer;
/// with nobody to hate it walks home. Returns the new enemy id.
fn return_charmed_mob(
    server: &mut RenetServer,
    aoi: &mut AoiGrid,
    enemies: &mut HashMap<EntityId, Entity>,
    in_world: &[ClientId],
    pet: Entity,
    turn_on: Option<EntityId>,
    now: std::time::Instant,
) -> EntityId {
    let cell = aoi::cell_for(pet.pos.x, pet.pos.z);
    aoi.remove(pet.id, cell);
    let visible = aoi.entities_visible_from(cell);
    for recipient in despawn_recipients(&visible, in_world, &pet) {
        handlers::send_entity_despawn(server, recipient, pet.id);
    }
    let mut mob = Entity::released_from_charm(&pet, now);
    match turn_on {
        Some(charmer) => mob.turn_on(charmer, now),
        None => mob.go_home(now),
    }
    let mob_id = mob.id;
    aoi.insert(mob_id, cell);
    let audience: Vec<ClientId> = in_world
        .iter()
        .copied()
        .filter(|cid| visible.contains(cid))
        .collect();
    // `replaces` lets a client that had the pet targeted keep the mob targeted.
    handlers::fan_out_enemy_spawn(server, &audience, &mob, Some(pet.id));
    if let Some(charmer) = turn_on {
        handlers::fan_out_entity_target(server, in_world, mob_id, Some(charmer));
    }
    enemies.insert(mob_id, mob);
    mob_id
}

/// Full disconnect cleanup, shared by the immediate clean-disconnect path and
/// the linkdead reaper sweep: despawn the player body (and any surviving pet)
/// for AOI peers, drop the player from the grid, remove them from their group,
/// then remove the connection and flush its dirty state to the DB.
///
/// Safe to call for a `client_id` that isn't in `connections` (a refused
/// duplicate login never inserted): every step degrades to a no-op. Does NOT
/// call `server.disconnect` — the transport is already torn down by the time
/// we get here on either path.
#[allow(clippy::too_many_arguments)] // the tick loop's state, passed piecewise
async fn reap_connection(
    server: &mut RenetServer,
    connections: &mut HashMap<ClientId, PerConnection>,
    aoi: &mut AoiGrid,
    enemies: &mut HashMap<EntityId, Entity>,
    group_manager: &mut GroupManager,
    pool: &SqlitePool,
    client_id: ClientId,
    now: std::time::Instant,
    pending_saves: &mut Vec<PendingSave>,
) {
    // Track 7: remove the leaver from the AOI grid BEFORE computing recipients
    // so entities_visible_from gives the correct set of peers who could see
    // this player. Send EntityDespawn only to that AOI-visible set.
    let despawn_info = connections
        .get(&client_id)
        .filter(|c| c.in_world)
        .map(|c| (c.char_id as u64, c.aoi_cell));
    if let Some((entity_id, leaver_cell)) = despawn_info {
        aoi.remove(entity_id, leaver_cell);
        let visible_peers = aoi.entities_visible_from(leaver_cell);
        let peer_ids: Vec<ClientId> = connections
            .iter()
            .filter(|(id, c)| **id != client_id && c.in_world && visible_peers.contains(*id))
            .map(|(id, _)| *id)
            .collect();
        for peer_id in peer_ids {
            handlers::send_entity_despawn(server, peer_id, entity_id);
        }
        // Despawn any pet still alive (no-op if linkdead already dropped it).
        despawn_owned_pets(server, connections, aoi, enemies, entity_id, client_id, now);
        // Their DoTs end with them (spell batch step 5): a logout pays
        // nothing for what was left ticking. A linkdead body keeps its DoTs
        // for the linger, since it is still in the world to be killed.
        for entity in enemies.values_mut() {
            entity.clear_dots_from(entity_id);
        }
        // And any PvP DoT they left on a player.
        let bearers = clear_player_dots_from(connections, &[entity_id]);
        if !bearers.is_empty() {
            let recipients: Vec<ClientId> = connections
                .iter()
                .filter(|(id, c)| **id != client_id && c.in_world)
                .map(|(id, _)| *id)
                .collect();
            for bearer in bearers {
                if let Some(conn) = connections.get(&bearer) {
                    fan_out_server_buff_snapshot(server, &recipients, conn);
                }
            }
        }
    }

    // Track 6 sub-task 5 — remove the leaver from their group. If the group
    // dissolves (one member left), notify them too. The rest of the roster
    // gets a fresh GroupRoster.
    if let Some((gid, remaining, dissolved)) = group_manager.leave(client_id) {
        if dissolved {
            // Group dissolved. Survivors (0 or 1) get an empty roster so their
            // HUD clears. The leaver's transport is already torn down.
            for m in &remaining {
                handlers::fan_group_roster(
                    server,
                    std::slice::from_ref(m),
                    gid,
                    *m,
                    Vec::new(),
                    0,
                );
            }
        } else if let Some(g) = group_manager.groups.get(&gid) {
            // Re-fetch the group with name lookups for the survivor fan-out.
            let members_with_names: Vec<(u64, String)> = g
                .members
                .iter()
                .filter_map(|m| connections.get(m).map(|c| (*m, c.name.clone())))
                .collect();
            let recipients: Vec<ClientId> = g.members.clone();
            handlers::fan_group_roster(
                server,
                &recipients,
                gid,
                g.leader,
                members_with_names,
                g.loot_mode.to_u8(),
            );
            // Same panel-row seed as the intent-path roster fan: the
            // survivors' rosters just changed shape, so refill every row.
            for m in &g.members {
                if let Some(c) = connections.get(m) {
                    handlers::fan_out_resources(server, &recipients, c);
                }
            }
        }
    }

    if let Some(mut conn) = connections.remove(&client_id) {
        // Whatever the audit queue still holds goes out with the connection.
        // The per-tick flush runs before the reaps, so this is normally
        // empty; it is here so a dev command handled in the same pass as a
        // disconnect can never have run unrecorded.
        let mut audit_rows = Vec::new();
        collect_gm_audit(&mut conn, Instant::now(), true, &mut audit_rows);
        write_gm_audit(pool, &audit_rows).await;

        // The moment the character left, for NO RENT items (purged at the
        // next login once thirty minutes have passed). Every departure comes
        // through here: a clean logout, or a linkdead body at the end of its
        // linger (the clean shutdown stamps its own). A crash writes nothing,
        // which only ever spares the items.
        if let Err(e) = db::set_logged_out_at(pool, conn.char_id, unix_now_secs()).await {
            tracing::warn!(char_id = conn.char_id, error = %e, "set_logged_out_at failed");
        }
        // One last save for the road, through the same flush as the checkpoint.
        // A character whose save did not fully land is NOT dropped: it waits in
        // `pending_saves`, retried every PENDING_SAVE_RETRY, and its account's
        // relogin is refused until it lands. Dropping it (as this used to)
        // lost everything since the last checkpoint, and letting the player
        // back in early would load the older saved state over it.
        if !persistence::flush_connection(pool, &mut conn, "logout").await {
            tracing::error!(
                char_id = conn.char_id,
                "logout save incomplete; queued for retry, relogin held until it lands"
            );
            pending_saves.push(PendingSave { conn, since: Instant::now() });
        }
    }
}

/// The clean end of the world loop (a stop signal). Every character in the
/// world (linkdead bodies and the logout retry queue included) is saved first,
/// failed saves retried until `SHUTDOWN_SAVE_BUDGET`; the queued GM audit rows
/// are written; then each connected player is told the world is restarting,
/// with the line that is true for them (saved or not), and every connection is
/// closed. Returns how many characters could not be saved. systemd allows
/// 20 s (`TimeoutStopSec`); a healthy shutdown takes well under a second.
async fn shut_down_world(
    server: &mut RenetServer,
    transport: &mut NetcodeServerTransport,
    connections: &mut HashMap<ClientId, PerConnection>,
    pending_saves: &mut [PendingSave],
    pool: &SqlitePool,
) -> u32 {
    tracing::info!(
        connections = connections.len(),
        pending_saves = pending_saves.len(),
        "stop signal: saving everyone and closing the world"
    );
    let mut audit_rows = Vec::new();
    for conn in connections.values_mut() {
        collect_gm_audit(conn, Instant::now(), true, &mut audit_rows);
    }
    write_gm_audit(pool, &audit_rows).await;

    let deadline = Instant::now() + SHUTDOWN_SAVE_BUDGET;
    loop {
        let mut all_saved = true;
        for conn in connections
            .values_mut()
            .chain(pending_saves.iter_mut().map(|p| &mut p.conn))
        {
            if persistence::has_unsaved_state(conn)
                && !persistence::flush_connection(pool, conn, "shutdown").await
            {
                all_saved = false;
            }
        }
        if all_saved || Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    // A shutdown is a departure for everyone in the world: stamp the logout
    // time NO RENT items are judged by, as a logout does.
    for conn in connections.values() {
        if let Err(e) = db::set_logged_out_at(pool, conn.char_id, unix_now_secs()).await {
            tracing::warn!(char_id = conn.char_id, error = %e, "set_logged_out_at failed at shutdown");
        }
    }

    let mut unsaved = 0u32;
    for conn in connections
        .values()
        .chain(pending_saves.iter().map(|p| &p.conn))
    {
        if persistence::has_unsaved_state(conn) {
            unsaved += 1;
            tracing::error!(char_id = conn.char_id, "shutdown save incomplete; progress since the last save is lost");
        }
    }
    for (client_id, conn) in connections.iter() {
        if conn.linkdead_since.is_some() {
            continue;
        }
        let line = if persistence::has_unsaved_state(conn) {
            SHUTDOWN_UNSAVED_MESSAGE
        } else {
            SHUTDOWN_MESSAGE
        };
        handlers::send_kick(server, *client_id, KickCode::Restart, line);
    }

    // A few ticks of transport service so the reliable kicks are delivered
    // (and resent if one was lost), then close every connection. The clock
    // before the transport, as in the live loop.
    for _ in 0..5 {
        transport.send_packets(server);
        tokio::time::sleep(TICK_DT).await;
        server.update(TICK_DT);
        if let Err(e) = transport.update(TICK_DT, server) {
            tracing::warn!(error = %e, "transport update error during shutdown");
        }
    }
    transport.send_packets(server);
    transport.disconnect_all(server);
    tracing::info!(
        saved = connections.len() + pending_saves.len() - unsaved as usize,
        unsaved,
        "world closed"
    );
    unsaved
}

pub async fn run(
    cfg: Arc<Config>,
    pool: SqlitePool,
    server: RenetServer,
    mut transport: NetcodeServerTransport,
    shutdown: tokio::sync::watch::Receiver<bool>,
) -> anyhow::Result<()> {

    // Track 18.1 — build the spell discipline lookup once at startup
    // so casting-skill advance dispatch is a HashMap hit per cast.
    skills::init_discipline_map();

    let connections: HashMap<ClientId, PerConnection> = HashMap::new();
    // Track 7 — AOI spatial index. Tracks which grid cell each in_world
    // entity (player, enemy, loot bag) occupies so position broadcasts
    // and EntitySpawn/Despawn can be filtered to the 3×3 neighbourhood
    // instead of broadcasting to every in_world client.
    let mut aoi = AoiGrid::new();
    let mut interval = tokio::time::interval(TICK_DT);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    let last_checkpoint = Instant::now();
    // Characters whose logout save did not fully land (see reap_connection):
    // retried every PENDING_SAVE_RETRY, their accounts' relogins refused
    // until the save lands, and flushed again at shutdown.
    let pending_saves: Vec<PendingSave> = Vec::new();
    let last_pending_retry = Instant::now();
    // Bandwidth counter for the enemy Position stream (step 6b): sends in the
    // current report window, logged every ENEMY_FAN_REPORT_INTERVAL.
    let enemy_pos_sent: u64 = 0;
    let last_fan_report = Instant::now();
    // The loop's own timings (world/health.rs): each tick's work and each
    // checkpoint, reported as the `world health` line.
    let tick_health = super::health::TickHealth::default();
    let mut tick_started: Option<Instant> = None;
    let last_health_report = Instant::now();
    let last_ban_sweep = Instant::now();
    // The item ledger's daily sweep (world/ledger.rs): None until it has run
    // once, so the first tick runs it at boot.
    let last_ledger_sweep: Option<Instant> = None;
    let last_time_broadcast = Instant::now();
    // The ban sweep's query runs on its own task and reports back here, so
    // the world loop never waits on the database for it.
    let (ban_tx, ban_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<i64>>();
    let ban_sweep_in_flight = false;

    // Track 5 sub-task 1B — server-authoritative enemies. The spawner owns
    // respawn timers per authored spawn point; `enemies` holds the live
    // instances. AI ticking + position/HP fan-out land in 1C; this commit
    // wires spawn lifecycle + EnemySpawn fan-out only (mobs stand idle).
    let spawner = Spawner::new(Instant::now());
    let enemies: HashMap<EntityId, Entity> = HashMap::new();
    // Track 5 sub-task 4 — server-owned loot bags. Rolled and spawned
    // in step 4h's on-death branch; expire after LOOT_BAG_LINGER_SECS
    // in step 4k. 4B will add the LootItem / LootAll handlers that
    // remove items mid-life.
    let loot_bags: HashMap<EntityId, LootBag> = HashMap::new();
    // Track 6 sub-task 5 — server-authoritative group state.
    // Ephemeral; lives only for the tick loop's lifetime. Disconnect
    // removes the member; one-member-left groups dissolve.
    let group_manager = GroupManager::new();
    // PD_W0028 — server-owned trade sessions (escrow by locking; see
    // world/trade.rs). Ephemeral like groups: a crash or restart drops every
    // session and every item is exactly where it always was.
    let trade_manager = super::trade::TradeManager::new();

    // Corpse / resurrection Slice 1 — server-owned player corpses (persisted
    // LootBags). Load every persisted corpse BEFORE the loop (so before any login
    // is accepted, no first-player race), seed the AOI, and advance the bag-id
    // minter past the highest loaded id so a fresh bag/corpse can't reuse a
    // loaded corpse's id after this restart.
    let mut corpses: HashMap<EntityId, super::corpses::Corpse> = HashMap::new();
    match db::load_corpses(&pool).await {
        Ok(rows) => {
            let boot = Instant::now();
            let mut max_id: EntityId = 0;
            for r in rows {
                let id = r.corpse_id as EntityId;
                let pos = super::connection::Vec3f::from_tuple(r.pos);
                let items: Vec<super::loot::LootItemStack> = r
                    .items
                    .into_iter()
                    .map(|(item_path, count, equip_slot)| super::loot::LootItemStack {
                        item_path,
                        count,
                        equip_slot,
                    })
                    .collect();
                aoi.insert(id, aoi::cell_for(pos.x, pos.z));
                max_id = max_id.max(id);
                corpses.insert(
                    id,
                    super::corpses::Corpse::new(
                        id, r.char_id, r.owner_name, r.zone, pos, items, r.coins,
                        r.lost_xp, r.resurrected, boot,
                    ),
                );
            }
            if max_id >= protocol::world::LOOT_BAG_ID_BASE {
                super::loot::reserve_bag_ids_through(max_id);
            }
            tracing::info!(count = corpses.len(), "loaded persisted corpses");
        }
        Err(e) => tracing::error!(error = %e, "load_corpses failed; starting with no corpses"),
    }

    let mut world = WorldState {
        cfg, pool, server, connections, aoi, last_checkpoint, pending_saves, last_pending_retry, enemy_pos_sent, last_fan_report, tick_health, last_health_report, last_ban_sweep, last_ledger_sweep, last_time_broadcast, ban_tx, ban_rx, ban_sweep_in_flight, spawner, enemies, loot_bags, group_manager, trade_manager, corpses,
    };

    loop {
        // The previous tick's work ends here, before the wait for the next:
        // measured at the top so every path out of the body (a `continue`
        // included) is counted.
        if let Some(started) = tick_started {
            world.tick_health.record_tick(started.elapsed());
        }
        interval.tick().await;
        let now = Instant::now();
        let mut q = Intents::default();
        tick_started = Some(now);
        let WorldState { pool, server, connections, pending_saves, .. } = &mut world; // @seg-world
        let dt = TICK_DT.as_secs_f32();

        // 0. A stop signal (SIGTERM from systemd, or Ctrl-C) ends the world
        //    cleanly: everyone is told, everyone is saved, then the loop
        //    returns. Checked at the top of a tick, so the state saved is
        //    whole, never half a tick.
        if *shutdown.borrow() {
            let unsaved =
                shut_down_world(&mut *server, &mut transport, &mut *connections, &mut *pending_saves, &pool)
                    .await;
            if unsaved > 0 {
                // A non-zero exit, so the journal and systemd both show it.
                anyhow::bail!("{unsaved} character(s) could not be saved at shutdown");
            }
            return Ok(());
        }

        // 1. Advance renet's per-connection clocks, then the transport (UDP
        //    packets in, netcode handshakes, the clients' acks handed to renet).
        //    That is renet's documented order, and the one its RTT needs: an
        //    ack is timed against the connection's clock as the transport hands
        //    it over, and that clock moves nowhere but in `server.update`. With
        //    the transport first (until 2026-10-30), an ack read in the tick
        //    after its packet went out measured zero, so the `player link`
        //    line read rtt_ms=0.0 for everyone. An RTT now reads in whole
        //    ticks, rounded up: 50 ms is the floor. The same clock drives the
        //    reliable resends: a lost packet is resent only when the clock
        //    passes its resend time, and this call was missing until
        //    2026-10-12, so a lost reliable packet (an inventory delta, a chat
        //    line, a kick) was never resent; invisible on a lossless tailnet,
        //    not on the internet.
        server.update(TICK_DT);
        if let Err(e) = transport.update(TICK_DT, &mut *server) {
            tracing::warn!(error = %e, "transport update error");
        }

        session::handle_transport_events(&mut world, &mut q, now, &transport).await; // @call

        intake::drain_messages(&mut world, &mut q, now); // @call

        session::flush_gm_audit(&mut world, &mut q, now).await; // @call

        session::reap_timed_out(&mut world, &mut q, now); // @call

        session::reap_linkdead(&mut world, &mut q, now).await; // @call

        fanout::seed_joiners(&mut world, &mut q, now); // @call

        fanout::fan_out_events(&mut world, &mut q, now); // @call

        let in_world_recipients_now = spawning::run_spawning(&mut world, &mut q, now); // @call

        melee::apply_active_skills(&mut world, &mut q, now, &in_world_recipients_now); // @call

        melee::apply_attacks(&mut world, &mut q, now, &in_world_recipients_now); // @call

        casting::apply_casts(&mut world, &mut q, now, &in_world_recipients_now); // @call

        grouping::apply_group_intents(&mut world, &mut q, now); // @call

        pet_commands::apply_pet_commands(&mut world, &mut q, now); // @call

        inventory_ops::apply_moves(&mut world, &mut q, now); // @call

        trading::apply_trade_intents(&mut world, &mut q, now).await; // @call

        trading::sweep_trades(&mut world, &mut q, now); // @call

        inventory_ops::apply_splits(&mut world, &mut q, now); // @call

        inventory_ops::apply_drops(&mut world, &mut q, now, &in_world_recipients_now).await; // @call

        inventory_ops::apply_equips(&mut world, &mut q, now, &in_world_recipients_now); // @call

        inventory_ops::apply_unequips(&mut world, &mut q, now, &in_world_recipients_now); // @call

        inventory_ops::apply_destroys(&mut world, &mut q, now); // @call

        inventory_ops::apply_consumables(&mut world, &mut q, now, &in_world_recipients_now); // @call

        inventory_ops::apply_gm_gives(&mut world, &mut q, now); // @call

        vendors::apply_buys(&mut world, &mut q, now); // @call

        vendors::apply_sells(&mut world, &mut q, now); // @call

        banking::apply_coin_bank(&mut world, &mut q, now); // @call

        banking::apply_item_bank(&mut world, &mut q, now); // @call

        enemy_ai::run_enemy_ai(&mut world, &mut q, now, dt, &in_world_recipients_now); // @call

        enemy_ai::clean_up_enemy_corpses(&mut world, &mut q, now, &in_world_recipients_now).await; // @call

        binds_and_res::persist_binds(&mut world, &mut q, now).await; // @call

        binds_and_res::apply_res_accepts(&mut world, &mut q, now).await; // @call

        quest_steps::apply_quest_turn_ins(&mut world, &mut q, now).await; // @call

        quest_steps::apply_quest_lifecycle(&mut world, &mut q, now); // @call

        looting::apply_loot_intents(&mut world, &mut q, now, &in_world_recipients_now).await; // @call

        looting::expire_bags_and_corpses(&mut world, &mut q, now, &in_world_recipients_now).await; // @call

        movement::run_movement(&mut world, &mut q, now, dt, &in_world_recipients_now); // @call

        buff_tick::run_buff_tick(&mut world, &mut q, now, dt, &in_world_recipients_now); // @call

        camp::camp_sweep(&mut world, &mut q, now); // @call

        death_sweep::run_death_sweep(&mut world, &mut q, now, &in_world_recipients_now).await; // @call

        regen_tick::run_regen(&mut world, &mut q, now, dt); // @call

        let in_world_ids = position_fan::fan_out_positions(&mut world, &mut q, now); // @call

        upkeep::run_upkeep(&mut world, &mut q, now, &in_world_ids).await; // @call
        let WorldState { server, .. } = &mut world; // @seg-world

        // 8. Push outbound packets to the network.
        transport.send_packets(&mut *server);

    }
}

/// The one line for a spell target that is out of reach, offline, or an id
/// that never existed. Deliberately the SAME line for all three: telling
/// "far" from "absent" would make a cast an is-online oracle for anyone
/// enumerating ids. No refusal names a player; the only names the caster is
/// given are the ones already on a nameplate within reach.
const TARGET_UNREACHABLE: &str = "That target is too far away or no longer here.";

/// The line for a spell the server has no effect for yet: a target type with
/// no arm (BIND, PORT), an AOE with no radius, a summon with no known pet.
const NO_EFFECT_YET: &str = "That magic has no effect here yet.";

/// What the cast pre-flight resolved the cast to, for the arm to act on
/// without looking the target up a second time or re-deciding reach,
/// liveness or the PvP gates. The pre-flight and the arm run back to back for
/// one intent, with nothing in between, so the arm may trust it; if an arm
/// ever finds its target gone anyway, `cast_resolution_lost` says so loudly
/// and refunds. One home for every rule (the 2026-10-02 review): a new target
/// kind or a per-spell range is decided here and nowhere else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResolvedTarget {
    /// Nothing to resolve: SELF, AOE, PET_SUMMON, PORT.
    Nothing,
    /// A player in the world and in reach: ENEMY's PvP path, ALLY on a
    /// player or on oneself, BIND on oneself or a group member.
    Player(ClientId),
    /// A live mob in reach: ENEMY's mob path, PET_CHARM's victim.
    Mob(EntityId),
    /// A live pet in reach: ENEMY on another player's pet, ALLY on a pet,
    /// PET_HEAL's own pet.
    Pet(EntityId),
    /// A corpse in reach that is not yet resurrected, and its owner, who is
    /// in the world.
    Corpse(EntityId, ClientId),
}

/// Cast eligibility, decided BEFORE any side effect. Returns the resolved
/// target for the arm to act on, or the refusal line.
///
/// Two reasons this is a pre-flight rather than checks inside the
/// per-target-type arms:
///
/// 1. **Reach.** ALLY spells (heals and buffs, on a player or a pet) and ENEMY
///    spells against a player had no range check at all, so a modified client
///    could heal a friend in a dungeon from town, or nuke a PvP-flagged player
///    from across the world, by naming an id. Every targeted cast now needs
///    its target within the spell's own reach (`Spell::reach`, 30 m unless
///    the spell names its own). Written keep-only-when-in-reach, so a
///    non-finite distance refuses.
/// 2. **Order.** The handler takes the mana, stamps the cooldown and rolls the
///    casting skill before its arms run. A refusal inside an arm could hand
///    the mana back but could not un-stamp the cooldown or un-roll the skill:
///    a cast the server was always going to refuse (Bind Affinity, which has
///    no server arm; a nuke at nothing) was a free casting-skill attempt from
///    any hotbar, and a refused Charm still burned its 60 s cooldown.
///    Everything decidable from immutable state is decided here, so a refused
///    cast costs nothing at all.
///
/// Every target type is covered: the three with a target (ENEMY, ALLY,
/// PET_CHARM), PET_HEAL (the caster's own pet), CORPSE, the spell-data checks
/// (an AOE with no radius, a summon with no known pet), and any type with no
/// arm. The arms carry NO copy of these checks (the 2026-10-02 review found
/// two homes for one rule): they consume the `ResolvedTarget` and treat a
/// target gone between here and there as the bug it would be.
#[allow(clippy::too_many_arguments)]
fn cast_target_refusal(
    spell: &spells::Spell,
    target_id: Option<EntityId>,
    caster_id: EntityId,
    caster_cid: ClientId,
    caster_pos: Vec3f,
    connections: &HashMap<ClientId, PerConnection>,
    enemies: &HashMap<EntityId, Entity>,
    corpses: &HashMap<EntityId, super::corpses::Corpse>,
    group_manager: &GroupManager,
) -> Result<ResolvedTarget, String> {
    use protocol::world::{ENEMY_ID_BASE, LOOT_BAG_ID_BASE, PET_ID_BASE};
    let Some(caster) = connections.get(&caster_cid) else {
        return Err(TARGET_UNREACHABLE.to_string());
    };
    // Each spell's own reach (per-spell ranges, 2026-10-19), exact: a 30 m
    // spell is out of range at 30.1 (user, 2026-10-27), and the client checks
    // the same full reach, so a target that walks out during a cast bar costs
    // a free refusal and nothing else. Binding a group member reaches as far
    // as the bind spell does.
    let reach = spell.reach();
    let in_reach = |pos: Vec3f| pos.distance_to(caster_pos) <= reach;
    let unreachable = || Err(TARGET_UNREACHABLE.to_string());
    let no_effect = || Err(NO_EFFECT_YET.to_string());
    let player_in_reach =
        |cid: ClientId| connections.get(&cid).filter(|c| c.in_world && in_reach(c.pos));
    let entity_in_reach =
        |id: EntityId| enemies.get(&id).filter(|e| e.is_alive() && in_reach(e.pos));
    let attackable = |other: &PerConnection| {
        combat::can_attack(caster, other, caster.zone.as_deref(), other.zone.as_deref())
    };

    match spell.target_type.as_str() {
        "ENEMY" => {
            let Some(target) = target_id else {
                return Err("You need a target for that spell.".to_string());
            };
            if target < ENEMY_ID_BASE {
                // A player: the PvP path.
                let target_cid = target as ClientId;
                if target_cid == caster_cid {
                    return Err("You cannot cast that on yourself.".to_string());
                }
                let Some(t) = player_in_reach(target_cid) else {
                    return unreachable();
                };
                if !attackable(t) {
                    return Err("You cannot attack that player.".to_string());
                }
                if t.hp <= 0.0 {
                    return Err("That target is no longer here.".to_string());
                }
                return Ok(ResolvedTarget::Player(target_cid));
            }
            let Some(e) = entity_in_reach(target) else {
                return unreachable();
            };
            // Another player's pet: allowed only if its owner is attackable,
            // and never the caster's own.
            if let Some(owner_id) = e.owner {
                let owner_cid = owner_id as ClientId;
                let allowed = owner_cid != caster_cid
                    && connections.get(&owner_cid).is_some_and(attackable);
                if !allowed {
                    return Err("You cannot attack that pet.".to_string());
                }
            }
            // One attack slow at a time, strongest wins (spell batch step 1):
            // a weaker slow on a target that already carries a stronger one
            // is refused here, at no cost, rather than replacing it.
            if spell.attack_slow_amount > 0.0
                && e.attack_slow_pct() > (spell.attack_slow_amount.clamp(0.0, 1.0) * 100.0) as u8
            {
                return Err("A stronger slow already holds that target.".to_string());
            }
            if e.is_pet() {
                Ok(ResolvedTarget::Pet(target))
            } else {
                Ok(ResolvedTarget::Mob(target))
            }
        }
        "ALLY" => {
            let raw = target_id.unwrap_or(0);
            if raw == 0 || raw == caster_id {
                return Ok(ResolvedTarget::Player(caster_cid)); // self
            }
            if raw >= PET_ID_BASE {
                let Some(owner_id) = entity_in_reach(raw).and_then(|p| p.owner) else {
                    return unreachable();
                };
                // The PvP heal gate: no topping up a duel partner's pet,
                // unless they are a group-mate.
                let owner_cid = owner_id as ClientId;
                if owner_cid != caster_cid {
                    let hostile = connections.get(&owner_cid).is_some_and(attackable);
                    if hostile && !group_manager.same_group(caster_cid, owner_cid) {
                        return Err("You cannot heal that pet.".to_string());
                    }
                }
                return Ok(ResolvedTarget::Pet(raw));
            }
            if raw >= LOOT_BAG_ID_BASE {
                return Err("Cannot heal that target.".to_string());
            }
            if raw >= ENEMY_ID_BASE {
                return Err("Cannot heal enemies.".to_string());
            }
            let target_cid = raw as ClientId;
            let Some(t) = player_in_reach(target_cid) else {
                return unreachable();
            };
            if t.hp <= 0.0 {
                return Err("That target is no longer here.".to_string());
            }
            if attackable(t) && !group_manager.same_group(caster_cid, target_cid) {
                return Err("You cannot heal that player.".to_string());
            }
            Ok(ResolvedTarget::Player(target_cid))
        }
        "PET_CHARM" => {
            let Some(target) = target_id else {
                return Err("You need a target for that spell.".to_string());
            };
            if !(ENEMY_ID_BASE..LOOT_BAG_ID_BASE).contains(&target) {
                return Err("You cannot charm that.".to_string());
            }
            match entity_in_reach(target) {
                Some(e) if !e.is_pet() => Ok(ResolvedTarget::Mob(target)),
                _ => unreachable(),
            }
        }
        "CORPSE" => {
            let Some(corpse_id) = target_id else {
                return Err("No corpse targeted.".to_string());
            };
            let Some(corpse) = corpses.get(&corpse_id) else {
                return Err("That is not a corpse.".to_string());
            };
            if !(corpse.pos.distance_to(caster_pos) <= reach) {
                return Err("You are too far from the corpse.".to_string());
            }
            if corpse.resurrected {
                return Err("That corpse has already been resurrected.".to_string());
            }
            // The owner must be in the world to receive the offer.
            let owner_cid = corpse.owner_char as ClientId;
            if !connections.get(&owner_cid).is_some_and(|c| c.in_world) {
                return Err("Their spirit is not present.".to_string());
            }
            Ok(ResolvedTarget::Corpse(corpse_id, owner_cid))
        }
        "AOE" => {
            if spell.aoe_radius > 0.0 {
                Ok(ResolvedTarget::Nothing)
            } else {
                no_effect()
            }
        }
        "PET_SUMMON" => {
            let known = !spell.pet_type.is_empty()
                && pet_templates::scaled(&spell.pet_type, caster.level.max(1) as u32, 0)
                    .is_some();
            if known {
                Ok(ResolvedTarget::Nothing)
            } else {
                no_effect()
            }
        }
        "BIND" => {
            // Spell batch step 2 (user calls D1, D2 of 2026-10-05): a caster
            // binds THEMSELVES anywhere; a GROUP MEMBER only while that member
            // stands in a safe area. No target, a self target or a mob target
            // all mean "myself". The bind written is always the target's own
            // position, never a client value.
            let raw = target_id.unwrap_or(0);
            if raw == 0 || raw == caster_id || raw >= ENEMY_ID_BASE {
                return Ok(ResolvedTarget::Player(caster_cid));
            }
            let target_cid = raw as ClientId;
            if !group_manager.same_group(caster_cid, target_cid) {
                return Err("You can only bind yourself or a group member.".to_string());
            }
            let Some(t) = player_in_reach(target_cid) else {
                return unreachable();
            };
            if t.hp <= 0.0 || t.death_processed {
                return Err("That target is no longer here.".to_string());
            }
            if super::safe_areas::containing(t.pos).is_none() {
                return Err("You can only bind another in a safe place, such as the town.".to_string());
            }
            Ok(ResolvedTarget::Player(target_cid))
        }
        "PORT" => {
            // Gate (port = "bind") always has somewhere to go, since every
            // character is bound from birth; a safe-area port needs one safe
            // area to exist. Anything else is a port the server has no
            // destination for (the zone ports) and costs nothing.
            match spell.port.as_str() {
                "bind" => Ok(ResolvedTarget::Nothing),
                "safe" if super::safe_areas::nearest_arrival(caster_pos).is_some() => {
                    Ok(ResolvedTarget::Nothing)
                }
                _ => no_effect(),
            }
        }
        "PET_HEAL" => {
            // The caster's own pet, whatever is targeted (the spell names it).
            // Having no pet says nothing about anyone else, so it can be
            // said plainly.
            let own_pet = enemies
                .iter()
                .find(|(_, e)| e.owner == Some(caster_id) && e.is_alive());
            match own_pet {
                None => Err("You have no pet to mend.".to_string()),
                Some((id, p)) if in_reach(p.pos) => Ok(ResolvedTarget::Pet(*id)),
                Some(_) => unreachable(),
            }
        }
        "SUMMON_ITEM" => {
            // Summon Food / Drink: the item must exist and the whole batch
            // must fit, decided before the mana comes off (EQ refused a
            // summon into full bags; here it costs nothing).
            let Some(item) = items::lookup(&spell.summon_item) else {
                return no_effect();
            };
            let n = spells::summon_count(spell, &caster.class, caster.level);
            if !caster.inventory.can_accept(&[(item.path.clone(), n)]) {
                return Err("Your bags are full.".to_string());
            }
            Ok(ResolvedTarget::Nothing)
        }
        "SELF" => Ok(ResolvedTarget::Nothing),
        // No arm exists for anything else: a target type new to the data.
        _ => no_effect(),
    }
}

/// An arm found the target the pre-flight resolved a moment ago gone. The
/// pre-flight and the arm run back to back for one intent, so this cannot
/// happen; if it ever does it is a bug, logged as one, and the caster is
/// refunded and told rather than silently charged for nothing.
#[allow(clippy::too_many_arguments)]
fn cast_resolution_lost(
    server: &mut RenetServer,
    connections: &mut HashMap<ClientId, PerConnection>,
    recipients: &[ClientId],
    caster_cid: ClientId,
    caster_id: u64,
    mana_cost: f32,
    hp_cost: f32,
    spell_name: &str,
    what: &str,
) {
    tracing::error!(
        caster = caster_id,
        spell = %spell_name,
        what,
        "cast target resolved by the pre-flight was gone at the arm; refunded"
    );
    refund_spell_cost(server, connections, recipients, caster_cid, caster_id, mana_cost, hp_cost);
    handlers::send_refusal(server, caster_cid, TARGET_UNREACHABLE);
}

/// End every PvP DoT these casters have on any player (spell batch step 5):
/// the ledger rule that your DoTs end when you die, port or log out holds
/// for a flagged player's bar too, or a caster could DoT someone and log
/// out with no counterplay. Returns the bearers whose buff set changed, for
/// the caller to re-fan.
fn clear_player_dots_from(connections: &mut HashMap<ClientId, PerConnection>, casters: &[EntityId]) -> Vec<ClientId> {
    let mut changed = Vec::new();
    for (cid, conn) in connections.iter_mut() {
        let before = conn.active_buffs.len();
        conn.active_buffs.retain(|b| {
            !matches!(b.effect, buffs::BuffEffect::Dot { caster, .. } if casters.contains(&caster))
        });
        if conn.active_buffs.len() != before {
            changed.push(*cid);
        }
    }
    changed
}

/// Remove these players from every mob's aggro and threat tables and from any
/// mob's target. A mob that loses its target leashes home and heals to full
/// (`entity.rs::tick_leash`). Shared by the death sweep and the port arm: a
/// player who Gates must not drag a train into town, and a mob that lost its
/// target pays nothing to "tag it, Gate out, let the damage finish it".
/// Returns the mobs whose target was cleared, for the caller to fan as
/// `EntityTarget`: the AI pass only reports a change it made itself
/// (`tick_ai` compares against the target it started with), so a wipe
/// here is invisible to clients unless the caller says so.
fn wipe_hate(enemies: &mut HashMap<EntityId, Entity>, players: &[EntityId]) -> Vec<EntityId> {
    let mut dropped = Vec::new();
    for entity in enemies.values_mut() {
        for id in players {
            entity.aggro.remove(id);
            entity.threat.remove(id);
            // Their DoTs end with their hate (spell batch step 5): a
            // player who died or ported is paid nothing by what they left
            // ticking.
            entity.clear_dots_from(*id);
            if entity.target == Some(*id) {
                entity.target = None;
                dropped.push(entity.id);
            }
        }
    }
    dropped
}

/// Tell an interrupted caster what the interrupt cost (spell batch step 3):
/// their true mana, privately, right behind the CastFail their client
/// answers with a local refund, so the bar settles on the charged value; and
/// one line saying how much. Nothing is sent when nothing was charged.
fn tell_interrupt_charge(
    server: &mut RenetServer,
    caster_cid: ClientId,
    caster_id: u64,
    mp: f32,
    max_mp: f32,
    mana_charged: f32,
) {
    if mana_charged <= 0.0 {
        return;
    }
    handlers::fan_out_mana_update(server, &[caster_cid], caster_id, mp, max_mp);
    handlers::send_system_line(
        server,
        caster_cid,
        &format!("Your concentration breaks: {} mana lost.", mana_charged.round() as i32),
    );
}

/// Stop a bard's song: clear the state and tell them. The lingering effects
/// run out on their own (EQ's rule). Nothing happens when nothing is sung.
fn stop_song(
    server: &mut RenetServer,
    connections: &mut HashMap<ClientId, PerConnection>,
    cid: ClientId,
) {
    let Some(c) = connections.get_mut(&cid) else { return };
    let Some(song) = c.active_song.take() else { return };
    c.song_next_pulse_at = None;
    let singer = c.char_id;
    handlers::send_system_line(server, cid, &format!("You stop singing {song}."));
    tracing::info!(singer, %song, "song stopped");
}

/// One pulse of a bard's song (the rework of 2026-10-20): the level-scaled
/// effects (`songs::pulse_effects`) as the ordinary buffs, each lingering
/// `SONG_LINGER_SECS` and refreshed by name on every pulse, fanned as a
/// snapshot when any landed. The ENEMY song's slow lands on the bard's
/// current target while it is a live mob in reach (out of reach, the pulse
/// is skipped; dead or gone, the song stops: EQ's invalid-target rule). A
/// PvP target is not a mob, so the song stops on one too (flagged).
fn song_pulse(
    server: &mut RenetServer,
    recipients: &[ClientId],
    connections: &mut HashMap<ClientId, PerConnection>,
    enemies: &mut HashMap<EntityId, Entity>,
    cid: ClientId,
    spell: &spells::Spell,
    now: Instant,
) {
    let linger = super::SONG_LINGER_SECS;
    let (effects, target, singer_pos) = {
        let Some(c) = connections.get(&cid) else { return };
        (super::songs::pulse_effects(spell, c.level, &c.class), c.current_target, c.pos)
    };
    let name = spell.name.clone();
    let mut changed = false;
    if let Some(c) = connections.get_mut(&cid) {
        if effects.hot_hps > 0.0 {
            apply_buff(c, ActiveBuff::new_hot(name.clone(), effects.hot_hps, linger, now));
            changed = true;
        }
        if effects.speed_mult > 1.0 {
            apply_buff(c, ActiveBuff::new_speed(name.clone(), effects.speed_mult, linger, now));
            changed = true;
        }
        if effects.mp_regen_hps > 0.0 {
            apply_mp_regen_exclusive(c, ActiveBuff::new_mp_regen(name.clone(), effects.mp_regen_hps, linger, now));
            changed = true;
        }
        if effects.haste > 0.0 {
            apply_buff(c, ActiveBuff::new_haste(name.clone(), effects.haste, linger, now));
            changed = true;
        }
        if effects.accuracy > 0.0 || effects.crit > 0.0 {
            apply_buff(c, ActiveBuff::new_accuracy_crit(name.clone(), effects.accuracy, effects.crit, linger, now));
            changed = true;
        }
    }
    if changed {
        if let Some(c) = connections.get(&cid) {
            fan_out_server_buff_snapshot(server, recipients, c);
        }
    }
    if effects.attack_slow > 0.0 {
        let target_stands = match target.and_then(|t| enemies.get_mut(&t)) {
            Some(e) if e.is_alive() => {
                if e.pos.distance_to(singer_pos) <= spell.reach() {
                    e.apply_cc(ActiveCc::new_attack_slow(effects.attack_slow, linger));
                }
                true
            }
            _ => false,
        };
        if !target_stands {
            stop_song(server, connections, cid);
        }
    }
}

/// An active skill's swing (client-authority audit item 6): the ordinary
/// roll times the skill's multiplier, never below one point; Truesight
/// Strike never misses. A plain swing passes through unchanged.
fn skill_swing(swing: combat::Swing, skill: Option<&SkillSwing>) -> combat::Swing {
    let Some(sk) = skill else { return swing };
    let mut out = swing;
    out.amount = ((swing.amount.max(0) as f32) * sk.mult).round().max(1.0) as i32;
    if sk.effect == active_skills::Effect::Truesight {
        out.missed = false;
    }
    out
}

/// Refuse a cast privately and hand the caster the server's true mana. The
/// client spent its mana and started a cooldown when it sent the cast, so the
/// CastFail is its signal to undo that and the ManaUpdate the value to settle
/// on. Nobody else is told: a refusal is the caster's business, and peers'
/// copies of a refused bar run out on their own (accepted 2026-10-02). The
/// three ways a RUNNING cast ends that peers can see (an interrupt, silence,
/// mez) keep their fan to peers so the bar they drew cancels; they reach the
/// caster too. Spell batch step 0.
fn refuse_cast(
    server: &mut RenetServer,
    connections: &HashMap<ClientId, PerConnection>,
    caster_cid: ClientId,
    caster_id: u64,
    line: &str,
) {
    handlers::fan_out_cast_fail(server, &[caster_cid], caster_id, line.to_string());
    if let Some(c) = connections.get(&caster_cid) {
        handlers::fan_out_mana_update(server, &[caster_cid], caster_id, c.mp, c.max_mp);
        // The caster's true buffs too: the client applies a self-buff at the
        // release, before the server has judged the cast, and a refusal left
        // it believing in a buff the server never held (a phantom Haste paced
        // its swings faster than the limiter allows; the 2026-10-30 review).
        // The snapshot's reconcile drops anything the server does not hold.
        handlers::fan_out_buff_snapshot(server, &[caster_cid], c);
    }
}

/// Give back a spell's cost after a cast is rejected downstream of the
/// deduction.
///
/// Mana (and any hp cost) comes off near the top of the CastSpell handler,
/// before the target-type match, so every rejection below that point used to
/// charge full price for nothing. Reporting the failure was only half the fix:
/// a 2026-08-24 playtest confirmed the player got "That target is too far away."
/// and stayed down the mana.
///
/// The unknown-spell arm higher up does NOT use this — the server never
/// deducted there, so it hands back its own untouched value instead.
#[allow(clippy::too_many_arguments)]
fn refund_spell_cost(
    server: &mut RenetServer,
    connections: &mut HashMap<ClientId, PerConnection>,
    recipients: &[ClientId],
    caster_cid: ClientId,
    caster_id: u64,
    mana_cost: f32,
    hp_cost: f32,
) {
    let Some(cc) = connections.get_mut(&caster_cid) else {
        return;
    };
    cc.mp = (cc.mp + mana_cost).min(cc.max_mp);
    if hp_cost > 0.0 {
        cc.hp = (cc.hp + hp_cost).min(cc.max_hp);
    }
    regen::mark_dirty(cc);
    let (mp, max_mp, hp, max_hp) = (cc.mp, cc.max_mp, cc.hp, cc.max_hp);
    handlers::fan_out_mana_update(server, recipients, caster_id, mp, max_mp);
    if hp_cost > 0.0 {
        handlers::fan_out_health_update(server, recipients, caster_id, hp, max_hp);
    }
}

// ── PD_W0028 — trade helpers (docs/design/trade_window.md). ──

/// Project one trade side's offers into the wire payload: the item and count
/// snapshotted at offer time, which is exactly what the commit verifies
/// against the live slot and hands over (escrow-by-locking: the session
/// stores references and a snapshot, never the item itself).
fn trade_side_payload(
    side: &super::trade::TradeSide,
) -> (Vec<(String, u32)>, protocol::world::Coins) {
    let mut slots = vec![(String::new(), 0u32); super::trade::TRADE_SLOTS];
    for (i, offer) in side.offers.iter().enumerate() {
        if let Some(o) = offer {
            slots[i] = (o.item_path.clone(), o.count);
        }
    }
    (slots, side.coins)
}

/// Fan BOTH parties the full state of BOTH sides plus the accept flags —
/// after any edit. Full-state per update, not deltas: a tiny payload with
/// no desync class.
fn fan_trade_state(
    server: &mut RenetServer,
    session: &super::trade::TradeSession,
) {
    for (me, them) in [(&session.a, &session.b), (&session.b, &session.a)] {
        let (my_slots, my_coins) = trade_side_payload(me);
        let (their_slots, their_coins) = trade_side_payload(them);
        handlers::send_trade_offer_update(server, me.cid, true, my_slots, my_coins);
        handlers::send_trade_offer_update(server, me.cid, false, their_slots, their_coins);
        handlers::send_trade_accept_state(server, me.cid, me.accepted, them.accepted);
    }
}

/// Close the session covering `cid` (if any) and tell both parties why.
/// Every non-commit exit routes here: cancel, range, death, disconnect,
/// commit failure. Unlocking is implicit — locks derive from live sessions.
fn close_trade(
    server: &mut RenetServer,
    trade_manager: &mut super::trade::TradeManager,
    cid: ClientId,
    reason: &str,
) {
    if let Some(session) = trade_manager.close(cid) {
        for c in [session.a.cid, session.b.cid] {
            handlers::send_trade_closed(server, c, false, reason);
        }
        tracing::info!(
            a = session.a.cid as u64,
            b = session.b.cid as u64,
            reason,
            "trade closed"
        );
    }
}

/// The escrow: a slot offered in an open trade refuses every other
/// inventory intent. Reports the refusal (the silent-refusals doctrine);
/// callers just `continue` on `true`.
fn trade_locked(
    server: &mut RenetServer,
    trade_manager: &super::trade::TradeManager,
    cid: ClientId,
    location: &str,
    slot: u32,
) -> bool {
    if trade_manager.is_locked(cid, location, slot) {
        handlers::send_refusal(server, cid, "That item is offered in a trade.");
        return true;
    }
    false
}

/// Tell one client the true contents of the two slots a failed `MoveItem`
/// named, so a client whose view has drifted can correct itself.
///
/// The server is authoritative over inventory, but authority only helps if
/// disagreement is *reported*. Before this, a rejected or no-op move sent the
/// client nothing at all: it kept rendering a phantom item, kept asking to move
/// it, and kept being refused, with no path back to the truth short of a relog.
/// A playtest on 2026-08-18 caught the same slots refused for half an hour.
///
/// Sending the real contents of exactly the two slots involved keeps this
/// proportional: at most two small messages per bad request the client made, so
/// it cannot be used to amplify traffic, and it converges because every wrong
/// belief is corrected the moment the client acts on it.
fn correct_client_slots(
    server: &mut RenetServer,
    cid: ClientId,
    conn: &PerConnection,
    src_loc: &str,
    src_slot: u32,
    dst_loc: &str,
    dst_slot: u32,
) {
    let mut targets: Vec<(&str, u32)> = vec![(src_loc, src_slot)];
    // A move onto itself names one slot twice; no reason to send it twice.
    if !(src_loc == dst_loc && src_slot == dst_slot) {
        targets.push((dst_loc, dst_slot));
    }
    for (loc, slot) in targets {
        let (item_path, count) = match conn.inventory.peek_at(loc, slot) {
            Some((p, c)) => (Some(p), c),
            // Empty is a real answer and the one the client most needs: it is
            // usually the phantom item that started the divergence.
            None => (None, 0),
        };
        handlers::send_inventory_delta(server, cid, loc.to_string(), slot, item_path, count);
        // Full-bags-move: when the corrected slot is a base slot holding a
        // bag, a different-bag correction makes the client re-init that
        // bag's contents vec and wait for inner rows — so re-fan the
        // occupied inners, or the recovery path itself would render the
        // bag empty (the 08-18 vanished-items symptom, on the fix path).
        if loc == "base" {
            if let Some(arr) = conn.inventory.bags.get(&(slot as u8)) {
                for (i, entry) in arr.iter().enumerate() {
                    if let Some(e) = entry {
                        handlers::send_inventory_delta(
                            server,
                            cid,
                            format!("bag_{slot}"),
                            i as u32,
                            Some(e.item_path.clone()),
                            e.count,
                        );
                    }
                }
            }
        }
    }
}

fn client_id_to_char(client_id: ClientId) -> i64 {
    // ClientId in renet 2.0 is `pub type ClientId = u64;`. We minted it as
    // char_id (i64) cast to u64; cast back. Positive ids roundtrip cleanly.
    client_id as i64
}

fn parse_account_id_from_user_data(user_data: Option<[u8; 256]>) -> i64 {
    let Some(data) = user_data else { return 0 };
    let mut buf = [0u8; 8];
    buf.copy_from_slice(&data[..8]);
    i64::from_le_bytes(buf)
}

/// Per-account GM flag, packed at `user_data[8]` by `mint_connect_token`.
/// Absent user_data (shouldn't happen for a Secure token) reads as non-GM.
fn parse_is_gm_from_user_data(user_data: Option<[u8; 256]>) -> bool {
    user_data.map(|d| d[8] != 0).unwrap_or(false)
}

/// Corpse / resurrection — a corpse despawns ONLY when a loot action emptied it:
/// it held something before and is empty now. A corpse that was ALREADY empty (a
/// naked-death res anchor) is NOT removed by a Take-All — it lingers for a Cleric
/// resurrection. A named predicate so this linger rule is testable and hard to
/// flip by accident (see the corpse-loot branch above).
fn corpse_emptied_by_loot(had_content: bool, empty_now: bool) -> bool {
    had_content && empty_now
}

#[cfg(test)]
mod tests {
    use super::corpse_emptied_by_loot;

    // The cast pre-flight is the ONE home of target resolution: the arms act
    // on the `ResolvedTarget` it returns and carry no copy of its checks (the
    // 2026-10-02 review's finding, closed 2026-10-14). So the VARIANT it
    // returns is load-bearing, not just the accept/refuse: an ENEMY cast
    // resolving a rival's pet as a Mob or a flagged player as anything else
    // would send the arm down the wrong path, and an untargeted heal is a
    // self-heal only because ALLY resolves nothing to the caster.
    mod cast_pre_flight {
        use super::super::{cast_target_refusal, ResolvedTarget, Vec3f, TARGET_UNREACHABLE};
        use crate::world::{
            connection::PerConnection, corpses::Corpse, entity::{mint_pet_id, Entity},
            groups::GroupManager, pet_templates, spells,
        };
        use protocol::world::{Coins, LOOT_BAG_ID_BASE};
        use renet::ClientId;
        use std::{collections::HashMap, time::Instant};

        fn conn_at(char_id: i64, pos: Vec3f, pvp: bool) -> PerConnection {
            let spawn = crate::db::CharacterSpawn {
                hunger: 14400.0,
                thirst: 14400.0,
                alignment: None,
                factions: Vec::new(),
                char_id,
                account_id: char_id,
                name: format!("Char{char_id}"),
                race: "Human".into(),
                class: "Cleric".into(),
                level: 20,
                xp: 0,
                xp_to_next: 100,
                strength: 10, dexterity: 10, agility: 10, intelligence: 10,
                wisdom: 10, charisma: 10, constitution: 10,
                max_hp: 100.0, max_mp: 100.0, max_stamina: 100.0,
                hp: 100.0, mp: 100.0, stamina: 100.0,
                coins: Coins::ZERO,
                bank_coins: Coins::ZERO,
                zone: None,
                pos: (pos.x, pos.y, pos.z),
                yaw: 0.0,
                completed_quests: Vec::new(),
                active_quests: Vec::new(),
                bind: None,
            };
            let mut c = PerConnection::from_spawn(spawn, Instant::now());
            c.in_world = true;
            c.pos = pos;
            c.pvp_override_on = pvp;
            c
        }

        /// A live skeleton at `pos`; with an owner it is that owner's pet,
        /// in the pet id partition like a real one.
        fn mob_at(pos: Vec3f, owner: Option<u64>) -> Entity {
            let template = pet_templates::lookup("skeleton").expect("skeleton template");
            let mut e = Entity::from_spawn(0, pos, template, Instant::now());
            if let Some(owner_id) = owner {
                e.id = mint_pet_id();
                e.owner = Some(owner_id);
            }
            e
        }

        #[test]
        fn resolves_each_target_kind_to_the_variant_its_arm_consumes() {
            let at = |x: f32| Vec3f { x, y: 0.0, z: 0.0 };
            let caster: ClientId = 1;
            let friend: ClientId = 2;
            let rival: ClientId = 3;
            let mut connections = HashMap::new();
            connections.insert(caster, conn_at(1, at(0.0), true));
            connections.insert(friend, conn_at(2, at(5.0), false));
            connections.insert(rival, conn_at(3, at(10.0), true));

            let mut enemies = HashMap::new();
            let mut add = |e: Entity| {
                let id = e.id;
                enemies.insert(id, e);
                id
            };
            let mob_id = add(mob_at(at(12.0), None));
            let own_pet_id = add(mob_at(at(3.0), Some(caster)));
            let rival_pet_id = add(mob_at(at(14.0), Some(rival)));
            // Just past the 30 m default reach (it was 26 m when hostile spells reached 25).
            let far_mob_id = add(mob_at(at(31.0), None));

            let corpse_id = LOOT_BAG_ID_BASE + 7;
            let mut corpses = HashMap::new();
            corpses.insert(
                corpse_id,
                Corpse::new(corpse_id, friend as i64, "Char2".into(), String::new(), at(8.0),
                    Vec::new(), Coins::ZERO, 0, false, Instant::now()),
            );
            let groups = GroupManager::new();
            let resolve = |name: &str, target: Option<u64>| {
                let spell = spells::lookup(name).unwrap_or_else(|| panic!("{name} in spells.toml"));
                cast_target_refusal(spell, target, caster, caster, at(0.0),
                    &connections, &enemies, &corpses, &groups)
            };
            let refused = |line: &str| Err(line.to_string());

            // ENEMY: a mob, a rival's pet (its owner attackable), a flagged
            // player; never one's own pet, nothing out of reach, nothing
            // untargeted.
            assert_eq!(resolve("Fireball", Some(mob_id)), Ok(ResolvedTarget::Mob(mob_id)));
            assert_eq!(resolve("Fireball", Some(rival_pet_id)), Ok(ResolvedTarget::Pet(rival_pet_id)));
            assert_eq!(resolve("Fireball", Some(rival)), Ok(ResolvedTarget::Player(rival)));
            assert_eq!(resolve("Fireball", Some(own_pet_id)), refused("You cannot attack that pet."));
            assert_eq!(resolve("Fireball", Some(far_mob_id)), refused(TARGET_UNREACHABLE));
            assert_eq!(resolve("Fireball", None), refused("You need a target for that spell."));
            // ALLY: nothing targeted (or oneself) is a self-heal; a player; a pet.
            assert_eq!(resolve("Heal", None), Ok(ResolvedTarget::Player(caster)));
            assert_eq!(resolve("Heal", Some(caster)), Ok(ResolvedTarget::Player(caster)));
            assert_eq!(resolve("Heal", Some(friend)), Ok(ResolvedTarget::Player(friend)));
            assert_eq!(resolve("Heal", Some(own_pet_id)), Ok(ResolvedTarget::Pet(own_pet_id)));
            // PET_HEAL: the caster's own pet, whatever the client targeted.
            assert_eq!(resolve("Warder's Mend", Some(mob_id)), Ok(ResolvedTarget::Pet(own_pet_id)));
            // PET_CHARM: a mob, never a pet (a pet id is outside the enemy
            // partition, so the partition check answers before the lookup).
            assert_eq!(resolve("Charm", Some(mob_id)), Ok(ResolvedTarget::Mob(mob_id)));
            assert_eq!(resolve("Charm", Some(rival_pet_id)), refused("You cannot charm that."));
            // CORPSE: the corpse and its owner, who is in the world.
            assert_eq!(
                resolve("Resurrection (Minor)", Some(corpse_id)),
                Ok(ResolvedTarget::Corpse(corpse_id, friend))
            );
            // BIND: oneself; a group member only (none here).
            assert_eq!(resolve("Bind Affinity", None), Ok(ResolvedTarget::Player(caster)));
            assert_eq!(
                resolve("Bind Affinity", Some(friend)),
                refused("You can only bind yourself or a group member.")
            );
            // SELF, AOE, PET_SUMMON, PORT: nothing to resolve.
            for name in ["Breeze", "Inferno", "Summon Skeleton", "Gate"] {
                assert_eq!(resolve(name, None), Ok(ResolvedTarget::Nothing), "{name}");
            }
        }
    }

    // Melee swing-rate limit: the per-hand minimum interval the server allows
    // between same-hand swings. It must (a) sit comfortably BELOW the fastest a
    // legit client swings for each weapon AT ITS OWN HASTE (so no false
    // positives), (b) never fall below the absolute hard floor, and (c) read
    // the player's real haste, so an unhasted forged client no longer gets the
    // fully-hasted allowance. Mirror of combat.gd pacing (weapon_delay x
    // offhand-mult x (1-haste), maxf 0.5).
    #[test]
    fn swing_rate_min_interval_is_below_client_fastest_but_above_floor() {
        use super::{honest_swing_pace, min_swing_interval_secs, HARD_MIN_SWING_SECS, OFFHAND_DELAY_MULT};
        let approx = |a: f32, b: f32| (a - b).abs() < 1e-4;
        let gate = |delay: f32, off: bool, haste: f32| {
            min_swing_interval_secs(honest_swing_pace(delay, off, haste))
        };
        // Fists, main hand, no haste: 2.0 minus the 0.35 grace = 1.65. (The
        // old gate gave everyone the 50% allowance, 0.65, hasted or not.)
        assert!(approx(gate(2.0, false, 0.0), 1.65));
        // The Enchanter's 30%: 2.0 * 0.7 = 1.4, minus grace = 1.05.
        assert!(approx(gate(2.0, false, 0.3), 1.05));
        // The cap: 2.0 * 0.5 = 1.0, minus grace = 0.65; a sum past it reads as the cap.
        assert!(approx(gate(2.0, false, 0.5), 0.65));
        assert!(approx(gate(2.0, false, 0.9), gate(2.0, false, 0.5)));
        // Off hand 2.0, no haste: 2.0 * 1.5 = 3.0, minus grace = 2.65.
        assert!(approx(gate(2.0, true, 0.0), 2.65));
        // Slow war axe 3.2 main, no haste: 2.85.
        assert!(approx(gate(3.2, false, 0.0), 2.85));
        // The gate must ALWAYS be <= the client's fastest legit same-hand interval
        // at that haste (weapon_delay * hand_mult * (1-haste), floored 0.5
        // client-side) so a legit swing is never rejected. Sweep the real range
        // PLUS hypothetical fast weapons (<2.0) where the client's 0.5s floor and
        // the server's 0.4s floor diverge most.
        for &haste in &[0.0_f32, 0.1, 0.3, 0.5] {
            for &delay in &[1.0_f32, 1.4, 2.0, 2.5, 2.8, 3.0, 3.2] {
                for &off in &[false, true] {
                    let hand_mult = if off { OFFHAND_DELAY_MULT } else { 1.0 };
                    let client_fastest = (delay * hand_mult * (1.0 - haste)).max(0.5);
                    assert!(
                        gate(delay, off, haste) <= client_fastest,
                        "gate {} must be <= client fastest {} (delay {delay}, off {off}, haste {haste})",
                        gate(delay, off, haste),
                        client_fastest
                    );
                }
            }
        }
        // Never below the absolute floor, even for an implausibly fast weapon.
        assert!(gate(0.1, false, 0.5) >= HARD_MIN_SWING_SECS);
        assert!(gate(0.0, true, 0.0) >= HARD_MIN_SWING_SECS);
    }

    // The weapon never carries: a staff swung after a dagger is held to the
    // staff's own gate at once (the review of 2026-10-30: alternating the two
    // every swing would otherwise swing the staff at the dagger's pace for
    // good), and a buff landing between two swings only ever loosens the gate.
    #[test]
    fn the_carried_haste_covers_the_timed_swing_but_the_weapon_does_not() {
        use super::{carried_haste, honest_swing_pace, min_swing_interval_secs};
        let approx = |a: f32, b: f32| (a - b).abs() < 1e-4;
        let gate = |delay: f32, haste_now: f32, history: [f32; 2]| {
            min_swing_interval_secs(honest_swing_pace(delay, false, carried_haste(haste_now, history)))
        };
        assert!(approx(gate(2.0, 0.0, [0.3, 0.3]), 1.05), "the swing timed under haste passes");
        assert!(approx(gate(2.0, 0.0, [0.0, 0.3]), 1.05), "and the one after it, timed two swings back");
        assert!(approx(gate(2.0, 0.0, [0.0, 0.0]), 1.65), "the third is held to the unhasted gate");
        assert!(approx(gate(3.2, 0.0, [0.0, 0.0]), 2.85), "a staff after a dagger is held to the staff's gate at once");
        assert!(approx(gate(1.4, 0.0, [0.0, 0.0]), 1.05), "and the dagger after the staff to the dagger's");
        assert!(gate(2.0, 0.3, [0.0, 0.0]) <= gate(2.0, 0.0, [0.0, 0.0]), "a buff landing between swings only loosens the gate");
    }

    // The client's attack timer as a Godot Timer runs it (measured
    // 2026-11-01): at a timeout the timer reloads its CURRENT wait_time, then
    // the handler sends the swing and sets a new wait_time for the haste it
    // holds; `start()` after a stop runs the current wait_time. Each fight is
    // the haste held at each of its swings, the fights ten minutes apart.
    // Returns how many of those honest swings the server's rule rejects;
    // `one_deep` judges by the rule built on 2026-10-30, which carried one.
    fn rejected_honest_swings(delay: f32, fights: &[&[f32]], one_deep: bool) -> usize {
        use super::{carried_haste, honest_swing_pace, min_swing_interval_secs, stamp_swing_haste};
        let mut wait_time = honest_swing_pace(delay, false, 0.0);
        let mut t = 0.0_f32;
        let mut last_at: Option<f32> = None;
        let mut history = [0.0_f32; 2];
        let mut rejected = 0;
        for fight in fights {
            t += 600.0;
            let mut gap = wait_time;
            for &haste in fight.iter() {
                t += gap;
                let carried = if one_deep { haste.max(history[0]) } else { carried_haste(haste, history) };
                let min = min_swing_interval_secs(honest_swing_pace(delay, false, carried));
                if last_at.is_some_and(|l| t - l < min) {
                    rejected += 1;
                } else {
                    last_at = Some(t);
                    stamp_swing_haste(&mut history, haste);
                }
                gap = wait_time;
                wait_time = honest_swing_pace(delay, false, haste);
            }
        }
        rejected
    }

    // The 2026-11-01 finding (WAROI, bare-handed: a Haste that ran out
    // between fights cost the second swing of the next fight), the same
    // expiry mid-fight, and a Haste landing mid-fight: no honest swing is
    // rejected, where the one-swing carry rejected one in each expiry case.
    #[test]
    fn no_honest_swing_is_rejected_when_a_haste_ends_or_begins() {
        let hasted: &[f32] = &[0.3; 6];
        let unhasted: &[f32] = &[0.0; 6];
        let ends_mid_fight: &[f32] = &[0.3, 0.3, 0.3, 0.0, 0.0, 0.0, 0.0];
        let begins_mid_fight: &[f32] = &[0.0, 0.0, 0.3, 0.3, 0.3, 0.3];
        for delay in [2.0_f32, 3.2, 1.4] {
            assert_eq!(rejected_honest_swings(delay, &[hasted, unhasted], false), 0, "between fights, delay {delay}");
            assert_eq!(rejected_honest_swings(delay, &[ends_mid_fight], false), 0, "mid-fight, delay {delay}");
            assert_eq!(rejected_honest_swings(delay, &[begins_mid_fight], false), 0, "a haste landing, delay {delay}");
            assert_eq!(rejected_honest_swings(delay, &[unhasted, hasted, unhasted], false), 0, "fights both ways, delay {delay}");
        }
        assert_eq!(rejected_honest_swings(2.0, &[hasted, unhasted], true), 1, "the one-swing carry dropped WAROI's swing");
        assert_eq!(rejected_honest_swings(2.0, &[ends_mid_fight], true), 1, "and one mid-fight");
    }

    // The bound on a forged client: once its Haste is gone, swinging on at
    // the hasted pace gets at most two swings through at that pace, then is
    // held to the unhasted gate.
    #[test]
    fn a_forged_client_gains_at_most_two_swings_at_an_expired_haste() {
        use super::{carried_haste, honest_swing_pace, min_swing_interval_secs, stamp_swing_haste};
        let hasted_gap = honest_swing_pace(2.0, false, 0.3);
        let mut history = [0.3_f32, 0.3];
        let mut last_at = 0.0_f32;
        let mut t = 0.0_f32;
        let mut at_the_hasted_pace = 0;
        for _ in 0..20 {
            t += hasted_gap;
            let min = min_swing_interval_secs(honest_swing_pace(2.0, false, carried_haste(0.0, history)));
            if t - last_at >= min {
                if t - last_at < 1.65 - 1e-4 {
                    at_the_hasted_pace += 1;
                }
                last_at = t;
                stamp_swing_haste(&mut history, 0.0);
            }
        }
        assert_eq!(at_the_hasted_pace, 2);
    }

    // The per-hand accept/reject decision: a never-swung hand is always allowed;
    // a swing within min_interval is "too fast" (rejected); one at/after it is
    // allowed. Per-hand independence is by construction (last_swing_at is indexed
    // by is_offhand), so a main + off swing at the same instant never interfere.
    #[test]
    fn swing_too_fast_gates_only_within_the_interval() {
        use super::swing_too_fast;
        use std::time::Duration;
        let base = std::time::Instant::now() + Duration::from_secs(60);
        // Never swung -> always accepted.
        assert!(!swing_too_fast(None, base, 0.65));
        // 0.5s after last, min 0.65 -> too fast (rejected).
        assert!(swing_too_fast(Some(base - Duration::from_millis(500)), base, 0.65));
        // Exactly at the interval -> allowed (not "less than").
        assert!(!swing_too_fast(Some(base - Duration::from_millis(650)), base, 0.65));
        // Well after -> allowed.
        assert!(!swing_too_fast(Some(base - Duration::from_secs(2)), base, 0.65));
    }

    // The connect token's user_data layout is a contract between
    // mint_connect_token (packs) and these parsers (read): account_id LE in
    // bytes [0..8], is_gm at byte [8]. If the layout drifts, GM access breaks
    // silently, so pin it.
    #[test]
    fn user_data_roundtrips_account_id_and_gm_flag() {
        use super::{parse_account_id_from_user_data, parse_is_gm_from_user_data};
        let mut ud = [0u8; 256];
        ud[..8].copy_from_slice(&12345i64.to_le_bytes());
        ud[8] = 1;
        assert_eq!(parse_account_id_from_user_data(Some(ud)), 12345);
        assert!(parse_is_gm_from_user_data(Some(ud)));
        ud[8] = 0;
        assert!(!parse_is_gm_from_user_data(Some(ud)));
        // A Secure token always carries user_data, but be defensive: absent
        // reads as account 0 / non-GM (never accidentally-GM).
        assert_eq!(parse_account_id_from_user_data(None), 0);
        assert!(!parse_is_gm_from_user_data(None));
    }

    #[test]
    fn corpse_lingers_unless_a_loot_emptied_it() {
        // Held gear, now empty -> looted clean -> the body despawns.
        assert!(corpse_emptied_by_loot(true, true));
        // Born empty (naked death), Take-All'd -> nothing was taken -> it LINGERS
        // as a res anchor. This is the edge the 2026-06-23 playtest didn't cover.
        assert!(!corpse_emptied_by_loot(false, true));
        // Partial loot -> still has items -> stays.
        assert!(!corpse_emptied_by_loot(true, false));
        // Empty and nothing taken -> lingers.
        assert!(!corpse_emptied_by_loot(false, false));
    }
}
