//! 20 Hz tick scheduler. Owns the connection map exclusively (no locking)
//! and drives both the renet transport and the application-layer message
//! pipeline.

use super::{
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
    loot::{self, LootBag},
    persistence,
    pet_templates,
    regen,
    skills,
    spawn_points::Spawner,
    spells,
    ATTACK_RANGE_TOLERANCE, BAN_SWEEP_INTERVAL, CAMP_SECS, CHANNEL_POSITION, CHANNEL_SYSTEM,
    CHECKPOINT_INTERVAL, ENEMY_FAN_REPORT_INTERVAL,
    ENEMY_DESPAWN_LINGER_SECS, GROUP_SHARE_RANGE, INSPECT_RANGE, KICK_FLUSH_GRACE,
    LINKDEAD_SECS,
    LOOT_BAG_LINGER_SECS,
    LOOT_PICKUP_RANGE, MAX_MOVE_SPEED,
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
/// Strongest single non-stacking haste the client models (Enchanter "Haste",
/// `haste_amount` 0.5 = 50% faster). The floor assumes a player is fully hasted
/// so a legitimately-hasted swing is never rejected. (Player haste is otherwise
/// a server no-op today; this is the safe over-estimate.)
const MAX_MODELED_HASTE: f32 = 0.5;
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

/// Minimum wall-clock seconds allowed between two SAME-HAND swings before the
/// faster one is a forgery. Mirrors the client pacing (`combat.gd`):
/// `weapon_delay` (fists 2.0) times the off-hand multiplier, scaled by the
/// strongest modeled haste so a fully-hasted player never trips, minus a jitter
/// grace, floored at `HARD_MIN_SWING_SECS`. Only ever used to reject "too fast"
/// — never "too slow" (the client sends an Attack only on a landed hit, so real
/// arrivals are already >= the interval).
///
/// Deliberate residual: because we can't tell a hasted client from a non-hasted
/// one at the gate (haste is client-paced, a server no-op today), we ASSUME max
/// haste for everyone. A forged non-hasted client can therefore swing up to the
/// fully-hasted rate (~2x a non-hasted honest swing). That is an accepted trade
/// — the gate exists to kill wire-speed spam (dozens/sec), not to enforce exact
/// cadence. Tighten `MAX_MODELED_HASTE` toward the player's real haste only once
/// server-authoritative haste exists, or false positives return for hasted play.
fn min_swing_interval_secs(weapon_delay: f32, is_offhand: bool) -> f32 {
    let hand_mult = if is_offhand { OFFHAND_DELAY_MULT } else { 1.0 };
    let fastest_legit =
        (weapon_delay * hand_mult * (1.0 - MAX_MODELED_HASTE)).max(HARD_MIN_SWING_SECS);
    (fastest_legit - SWING_RATE_GRACE_SECS).max(HARD_MIN_SWING_SECS)
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
    Interrupted { spell_name: String },
    Survived { advanced_to: Option<i32> },
}

/// Track 19A — roll the channeling-based interrupt on a target who
/// just took damage. Mirrors GDScript `Spells.try_interrupt_cast` +
/// `_finish_cast`'s "advance on survival" path. Mutates the conn's
/// cast cache + channeling score in place; caller handles fan-out.
fn roll_cast_interrupt(conn: &mut PerConnection, now: Instant) -> InterruptOutcome {
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
        conn.cast_spell_name.clear();
        conn.cast_total_duration = 0.0;
        conn.cast_set_at = None;
        // Stamped so a CastSpell for this bar that is already in the tick's
        // queue is refused too (`cast_gate::beaten_by_interrupt`).
        conn.cast_interrupted_at = Some(now);
        InterruptOutcome::Interrupted { spell_name }
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

/// Track 13 — fan a BuffSnapshot for a pet's active buffs under the pet
/// id, so the owner + bystanders render its buff bar (the client routes
/// the snapshot by id partition). Mirror of `fan_out_server_buff_snapshot`.
fn fan_out_pet_buff_snapshot(
    server: &mut renet::RenetServer,
    recipients: &[renet::ClientId],
    pet: &Entity,
) {
    let payload = buffs::snapshot_pairs(&pet.active_buffs);
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
    let (died, credit_id_opt, mob_xp, mob_level, death_pos, mob_name, damage_done, warder_owner_opt, victim_owned) = {
        let Some(entity) = enemies.get_mut(&target_id) else {
            return false;
        };
        if !entity.is_alive() {
            return false;
        }
        let dmg = spell.base_damage.max(0.0) as i32;
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
            false,
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
            entity.transition(EnemyState::Dead, now);
            handlers::fan_out_entity_died(server, in_world_recipients, entity_id);
            let credit_id_opt = entity
                .aggro
                .iter()
                .max_by(|a, b| {
                    a.1.partial_cmp(b.1)
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .map(|(&id, _)| id);
            // EQ quadratic per-kill award, derived from the mob's level (not the
            // legacy flat mob.xp constant). See progression::kill_xp.
            let mob_xp = super::progression::kill_xp(entity.mob.level as i32, super::progression::ZEM_NORMAL);
            let mob_level = entity.mob.level;
            let death_pos = entity.pos;
            let mob_name = entity.mob.name.clone();
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
            (true, credit_id_opt, mob_xp, mob_level, death_pos, mob_name, damage_done, warder_owner_opt, victim_owned)
        } else {
            if dmg > 0 {
                entity.clear_mez();
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
            (false, None, 0, 0, entity.pos, String::new(), damage_done, None, false)
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
        // Warder respawn — mirrors the melee path. If the dying entity
        // is a Beast Master warder, schedule the owner's
        // warder_respawn_at so the warder-respawn sweep brings it back.
        if let Some(owner_id) = warder_owner_opt {
            const WARDER_RETREAT_SECS: f32 = 15.0;
            let due = now + std::time::Duration::from_secs_f32(WARDER_RETREAT_SECS);
            let owner_cid = owner_id as ClientId;
            if let Some(conn) = connections.get_mut(&owner_cid) {
                conn.warder_respawn_at = Some(due);
                tracing::info!(
                    owner = owner_cid as u64,
                    killer = caster_id,
                    retreat_secs = WARDER_RETREAT_SECS,
                    "warder retreating after spell kill",
                );
            }
        }
        if let Some(credit_id) = credit_id_opt {
            // Spell kills use the same group XP split + quest credit as melee
            // kills (EQ semantics: the split is method-agnostic).
            award_kill(
                server,
                connections,
                group_manager,
                credit_id,
                mob_xp,
                &mob_name,
                victim_owned,
                death_pos,
            );
        }
        let loot_items = loot::roll_for_mob(&mob_name).unwrap_or_default();
        let loot_coins = loot::roll_coin_for_mob(&mob_name, mob_level);
        if !loot_items.is_empty() || loot_coins != protocol::world::Coins::ZERO {
            // Loot ownership: the spell kill-creditor owns the corpse;
            // their group shares rights, resolved at loot time.
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
            tracing::info!(
                mob = %mob_name,
                bag_id,
                "loot bag spawned (spell kill)"
            );
        }
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
        handlers::fan_out_pet_spawn(server, &pet_recipients, &pet);
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
    handlers::fan_out_enemy_spawn(server, &audience, &mob);
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
async fn reap_connection(
    server: &mut RenetServer,
    connections: &mut HashMap<ClientId, PerConnection>,
    aoi: &mut AoiGrid,
    enemies: &mut HashMap<EntityId, Entity>,
    group_manager: &mut GroupManager,
    pool: &SqlitePool,
    client_id: ClientId,
    now: std::time::Instant,
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

        // One last save for the road. Failure is non-fatal — worst case the
        // player rolls back to the last 60 s checkpoint.
        if conn.is_dirty_for_persist() {
            let zone = conn.zone.clone();
            if let Err(e) = db::checkpoint_position(
                pool,
                conn.char_id,
                zone.as_deref(),
                conn.pos.into_tuple(),
                conn.yaw,
            )
            .await
            {
                tracing::warn!(
                    char_id = conn.char_id,
                    error = %e,
                    "final checkpoint on disconnect failed"
                );
            } else {
                conn.mark_persisted();
            }
        }
        // Every item/coin store flushes on the way out, in ONE transaction.
        // A logout right after a vendor run or a bank deposit shouldn't roll
        // back to the last 60 s checkpoint — and, more importantly, these
        // stores must not be written separately: an item moving between two of
        // them (inventory to vault, wallet to bank) would otherwise have a
        // crash window that loses or duplicates it. Same reasoning and same
        // helper as the periodic sweep. See db::save_stores_atomic.
        let any_store_dirty = conn.inventory_dirty
            || conn.coins_dirty
            || conn.bank_dirty
            || conn.bank_items_dirty
            || conn.account_bank_items_dirty;
        if any_store_dirty {
            let inv_rows = conn.inventory_dirty.then(|| conn.inventory.to_rows());
            let bank_rows = conn.bank_items_dirty.then(|| conn.bank_items.to_rows());
            let acct_rows = conn
                .account_bank_items_dirty
                .then(|| conn.account_bank_items.to_rows());
            match db::save_stores_atomic(
                pool,
                conn.char_id,
                conn.account_id,
                inv_rows.as_deref(),
                conn.coins_dirty.then_some(conn.coins),
                conn.bank_dirty.then_some(conn.bank_coins),
                bank_rows.as_deref(),
                acct_rows.as_deref(),
            )
            .await
            {
                Ok(()) => {
                    conn.inventory_dirty = false;
                    conn.coins_dirty = false;
                    conn.bank_dirty = false;
                    conn.bank_items_dirty = false;
                    conn.account_bank_items_dirty = false;
                }
                Err(e) => {
                    tracing::warn!(
                        char_id = conn.char_id,
                        error = %e,
                        "final store save on disconnect failed"
                    );
                }
            }
        }
        // Resources (hp / mp / stamina / xp / level) — a kill's XP or a de-level
        // right before logout shouldn't roll back either.
        if conn.is_dirty_for_resource_persist() {
            if let Err(e) = db::checkpoint_resources(
                pool,
                conn.char_id,
                conn.hp,
                conn.mp,
                conn.stamina,
                conn.xp,
                conn.xp_to_next,
                conn.level,
            )
            .await
            {
                tracing::warn!(
                    char_id = conn.char_id,
                    error = %e,
                    "final resource save on disconnect failed"
                );
            } else {
                conn.mark_resources_persisted();
            }
        }
        // Passive skill scores — an advance (weapon / armor / casting) that
        // landed since the last checkpoint shouldn't roll back on relog.
        if conn.skills_dirty {
            let mut rows: Vec<db::SkillRow> = Vec::new();
            for (key, score) in &conn.weapon_skills {
                rows.push(db::SkillRow {
                    kind: "weapon".to_string(),
                    key: key.clone(),
                    score: *score,
                });
            }
            for (key, score) in &conn.armor_skills {
                rows.push(db::SkillRow {
                    kind: "armor".to_string(),
                    key: key.clone(),
                    score: *score,
                });
            }
            for (key, score) in &conn.casting_skills {
                rows.push(db::SkillRow {
                    kind: "casting".to_string(),
                    key: key.clone(),
                    score: *score,
                });
            }
            if let Err(e) = db::save_skills(pool, conn.char_id, &rows).await {
                tracing::warn!(
                    char_id = conn.char_id,
                    error = %e,
                    "final skill save on disconnect failed"
                );
            } else {
                conn.skills_dirty = false;
            }
        }
        // PD_W0024 — reconcile quest state touched this tick on the way out (a
        // reap can run before the end-of-tick flush reaches this connection;
        // after removal from the map that flush won't see it). Same rule as
        // step 6-ter: upsert if the quest is still active, else delete the row.
        let dirty_quests: Vec<String> = conn.quests_dirty.drain().collect();
        for quest_id in dirty_quests {
            let result = match conn.active_quests.get(&quest_id) {
                Some(progress) => {
                    db::save_quest_progress(pool, conn.char_id, &quest_id, progress).await
                }
                None => db::delete_active_quest(pool, conn.char_id, &quest_id).await,
            };
            if let Err(e) = result {
                tracing::warn!(
                    char_id = conn.char_id,
                    quest_id = %quest_id,
                    error = %e,
                    "final quest state flush on disconnect failed"
                );
            }
        }
    }
}

pub async fn run(
    cfg: Arc<Config>,
    pool: SqlitePool,
    mut server: RenetServer,
    mut transport: NetcodeServerTransport,
) -> anyhow::Result<()> {
    let _ = cfg; // Reserved for future config-driven tuning (max_clients live-reload, etc.).

    // Track 18.1 — build the spell discipline lookup once at startup
    // so casting-skill advance dispatch is a HashMap hit per cast.
    skills::init_discipline_map();

    let mut connections: HashMap<ClientId, PerConnection> = HashMap::new();
    // Track 7 — AOI spatial index. Tracks which grid cell each in_world
    // entity (player, enemy, loot bag) occupies so position broadcasts
    // and EntitySpawn/Despawn can be filtered to the 3×3 neighbourhood
    // instead of broadcasting to every in_world client.
    let mut aoi = AoiGrid::new();
    let mut interval = tokio::time::interval(TICK_DT);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    let mut last_checkpoint = Instant::now();
    // Bandwidth counter for the enemy Position stream (step 6b): sends in the
    // current report window, logged every ENEMY_FAN_REPORT_INTERVAL.
    let mut enemy_pos_sent: u64 = 0;
    let mut last_fan_report = Instant::now();
    let mut last_ban_sweep = Instant::now();
    let mut last_time_broadcast = Instant::now();
    // The ban sweep's query runs on its own task and reports back here, so
    // the world loop never waits on the database for it.
    let (ban_tx, mut ban_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<i64>>();
    let mut ban_sweep_in_flight = false;

    // Track 5 sub-task 1B — server-authoritative enemies. The spawner owns
    // respawn timers per authored spawn point; `enemies` holds the live
    // instances. AI ticking + position/HP fan-out land in 1C; this commit
    // wires spawn lifecycle + EnemySpawn fan-out only (mobs stand idle).
    let mut spawner = Spawner::new(Instant::now());
    let mut enemies: HashMap<EntityId, Entity> = HashMap::new();
    // Track 5 sub-task 4 — server-owned loot bags. Rolled and spawned
    // in step 4h's on-death branch; expire after LOOT_BAG_LINGER_SECS
    // in step 4k. 4B will add the LootItem / LootAll handlers that
    // remove items mid-life.
    let mut loot_bags: HashMap<EntityId, LootBag> = HashMap::new();
    // Track 6 sub-task 5 — server-authoritative group state.
    // Ephemeral; lives only for the tick loop's lifetime. Disconnect
    // removes the member; one-member-left groups dissolve.
    let mut group_manager = GroupManager::new();
    // PD_W0028 — server-owned trade sessions (escrow by locking; see
    // world/trade.rs). Ephemeral like groups: a crash or restart drops every
    // session and every item is exactly where it always was.
    let mut trade_manager = super::trade::TradeManager::new();

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
                    .map(|(item_path, count)| super::loot::LootItemStack { item_path, count })
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

    loop {
        interval.tick().await;
        let now = Instant::now();

        // 1. Advance the transport (reads UDP packets, processes netcode handshakes).
        if let Err(e) = transport.update(TICK_DT, &mut server) {
            tracing::warn!(error = %e, "transport update error");
        }

        // 2. Drain transport-level events (Connected/Disconnected).
        while let Some(event) = server.get_event() {
            match event {
                ServerEvent::ClientConnected { client_id } => {
                    let user_data = transport.user_data(client_id);
                    let account_id = parse_account_id_from_user_data(user_data);
                    let is_gm = parse_is_gm_from_user_data(user_data);
                    // The renet ClientId equals the ConnectToken's `client_id`,
                    // which the auth handler set to `char_id`.
                    let char_id = client_id_to_char(client_id);
                    tracing::info!(%client_id, char_id, account_id, "client connected (transport)");

                    match db::load_character(&pool, char_id).await {
                        Ok(spawn) => {
                            if spawn.account_id != account_id {
                                tracing::warn!(
                                    expected_account = account_id,
                                    actual_account = spawn.account_id,
                                    char_id,
                                    "ConnectToken account_id mismatch — kicking"
                                );
                                handlers::send_kick(
                                    &mut server,
                                    client_id,
                                    KickCode::Unknown,
                                    "account/char mismatch",
                                );
                                server.disconnect(client_id);
                                continue;
                            }
                            // Banker slice 2 — one character per account in-world.
                            // If the account is already connected, REFUSE this new
                            // login and leave the existing session untouched. We
                            // never force-disconnect the session already playing:
                            // "log in again to boot your other session" is a known
                            // grief / force-off vector (Lineage II). Denying the new
                            // login also keeps the account-shared vault trivially
                            // single-owner. Tradeoff: after an UNCLEAN disconnect the
                            // player waits for the stale session to time out before
                            // reconnecting; a clean logout frees the account at once.
                            if let Some(existing) =
                                connections.values().find(|c| c.account_id == account_id)
                            {
                                // If the existing session is lingering linkdead, the
                                // soonest a fresh relogin can succeed is when its window
                                // elapses; hand the client that countdown. A LIVE session
                                // (not linkdead) has no countdown — the player must log
                                // that one out first.
                                let reconnect_after_secs = existing.linkdead_since.map(|t| {
                                    LINKDEAD_SECS
                                        .saturating_sub(now.duration_since(t))
                                        .as_secs() as u32
                                });
                                tracing::info!(
                                    account_id, char_id, ?reconnect_after_secs,
                                    "duplicate login refused — account already in-world"
                                );
                                // The EQ phrasing, plus a "try again in ~Ns"
                                // estimate when the existing session is lingering
                                // linkdead (computed once at kick time; a live
                                // session has no countdown — the player must log it
                                // out first). The structured `reconnect_after_secs`
                                // is also sent for a future live-ticking client UI.
                                let reason = match reconnect_after_secs {
                                    Some(secs) => format!(
                                        "You already have a character in this world. You can try again in about {secs}s."
                                    ),
                                    None => {
                                        "You already have a character in this world.".to_string()
                                    }
                                };
                                // Send the reason and let the CLIENT tear down its own
                                // transport (its kick handler calls disconnect_now). Calling
                                // server.disconnect() here would evict this connection's
                                // channel buffers before the reliable Kick flushes, so the
                                // client would only ever see a generic transport drop — the
                                // same race leave_session() avoids on the client side. The
                                // refused connection was never inserted into `connections`, so
                                // it holds no world state and its messages are ignored; if the
                                // client ignores the kick, the netcode timeout reaps it.
                                handlers::send_kick_with_reconnect(
                                    &mut server,
                                    client_id,
                                    KickCode::DuplicateLogin,
                                    &reason,
                                    reconnect_after_secs,
                                );
                                continue;
                            }
                            // A ban holds at world connect too. Banning purges
                            // the account's sessions and session redemption
                            // refuses a banned account, but a connect token
                            // minted just BEFORE the ban is still valid for its
                            // short life; this is what stops it at the door.
                            // Fails closed: if the flag cannot be read, refuse.
                            // Same no-disconnect kick as the refusal above, so
                            // the reliable Kick reaches the client.
                            let banned = match db::account_is_banned(&pool, account_id).await {
                                Ok(banned) => banned,
                                Err(e) => {
                                    tracing::error!(account_id, char_id, error = %e,
                                        "could not read the ban flag at connect — refusing");
                                    true
                                }
                            };
                            if banned {
                                tracing::info!(account_id, char_id, "banned account refused at world connect");
                                handlers::send_kick(
                                    &mut server,
                                    client_id,
                                    KickCode::BannedNow,
                                    "This account is banned.",
                                );
                                continue;
                            }
                            // Track 13.1 — load any persisted inventory
                            // rows. Failure here is non-fatal (logged
                            // and the character keeps an empty
                            // inventory snapshot); we don't want a
                            // transient DB error to kick the client
                            // out of the world.
                            let inv_rows = match db::load_inventory(&pool, char_id).await {
                                Ok(rows) => rows,
                                Err(e) => {
                                    tracing::warn!(
                                        char_id,
                                        error = %e,
                                        "load_inventory failed; defaulting to empty"
                                    );
                                    Vec::new()
                                }
                            };
                            // Track 18.1 — load persisted skill scores
                            // alongside inventory. Same non-fatal stance.
                            let skill_rows = match db::load_skills(&pool, char_id).await {
                                Ok(rows) => rows,
                                Err(e) => {
                                    tracing::warn!(
                                        char_id,
                                        error = %e,
                                        "load_skills failed; defaulting to starting values"
                                    );
                                    Vec::new()
                                }
                            };
                            // Banker slice 2 — load the two item vaults:
                            // personal (char-keyed) + account-shared (keyed on
                            // account_id, freshly flushed above if a sibling
                            // session was just kicked).
                            let bank_item_rows = match db::load_bank_items(&pool, char_id).await {
                                Ok(r) => r,
                                Err(e) => {
                                    tracing::warn!(char_id, error = %e, "load_bank_items failed; empty");
                                    Vec::new()
                                }
                            };
                            let account_bank_item_rows =
                                match db::load_account_bank_items(&pool, account_id).await {
                                    Ok(r) => r,
                                    Err(e) => {
                                        tracing::warn!(account_id, error = %e, "load_account_bank_items failed; empty");
                                        Vec::new()
                                    }
                                };
                            connections.insert(
                                client_id,
                                PerConnection::from_spawn(spawn, now),
                            );
                            // Set the real AOI cell from spawn position +
                            // populate the inventory snapshot. Track 14.2:
                            // run the stat recompute pass so persisted
                            // equipped items reapply their max-HP / armor /
                            // stat bonuses before the EnterWorld snapshot
                            // fans the resource update.
                            if let Some(conn) = connections.get_mut(&client_id) {
                                // Stamp the GM flag from the signed token
                                // (from_spawn only sees the character spawn).
                                conn.is_gm = is_gm;
                                if is_gm {
                                    tracing::info!(account_id, char_id, "GM account connected");
                                }
                                conn.aoi_cell = aoi::cell_for(conn.pos.x, conn.pos.z);
                                conn.inventory = inventory::PlayerInventory::from_rows(&inv_rows);
                                conn.bank_items = inventory::ItemVault::from_rows(
                                    inventory::BANK_VAULT_SLOTS,
                                    &bank_item_rows,
                                );
                                conn.account_bank_items = inventory::ItemVault::from_rows(
                                    inventory::ACCOUNT_VAULT_SLOTS,
                                    &account_bank_item_rows,
                                );
                                let _ = inventory::recompute_equipped_stats(conn);
                                // Track 18.1 — seed all three skill maps
                                // with starting values (untrained classes
                                // get 0; trainable classes get the L1 cap)
                                // and then overlay persisted rows. New
                                // characters with no DB rows still wind
                                // up with the full key set populated; the
                                // GDScript autoloads expect this shape.
                                skills::seed_starting_scores(conn);
                                for row in &skill_rows {
                                    let map = match row.kind.as_str() {
                                        "weapon" => Some(&mut conn.weapon_skills),
                                        "armor" => Some(&mut conn.armor_skills),
                                        "casting" => Some(&mut conn.casting_skills),
                                        _ => None,
                                    };
                                    if let Some(m) = map {
                                        m.insert(row.key.clone(), row.score);
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            tracing::warn!(char_id, error = %e, "failed to load character");
                            handlers::send_kick(
                                &mut server,
                                client_id,
                                KickCode::Unknown,
                                "character not found",
                            );
                            server.disconnect(client_id);
                        }
                    }
                }
                ServerEvent::ClientDisconnected { client_id, reason } => {
                    tracing::info!(%client_id, ?reason, "client disconnected (transport)");

                    // Camp + linkdead: if a body is ALREADY lingering linkdead
                    // under this client_id, this event is a stray echo, not the
                    // body dropping again. Because client_id == char_id, a refused
                    // same-character relogin collides with the lingering body's key
                    // and emits its own ClientDisconnected when it tears down after
                    // the deny-login kick. Draining its messages or re-marking would
                    // reset the reap timer and let the body linger forever, so a
                    // player could dodge the reap by spamming relogin. Ignore it; the
                    // reaper still removes the body when its window elapses.
                    if connections
                        .get(&client_id)
                        .is_some_and(|c| c.linkdead_since.is_some())
                    {
                        tracing::debug!(
                            %client_id,
                            "ignoring disconnect for already-linkdead body (refused-relogin echo)"
                        );
                        continue;
                    }

                    // Drain any pending app-layer messages before removing the
                    // connection. Without this, if the client sent
                    // ClientWorldMsg::Disconnect and then tore down the
                    // transport in the same UDP burst, the message is silently
                    // lost because this event handler runs before the
                    // message-drain phase below — and that phase skips clients
                    // not in `connections`. Outcome is ignored; we're already
                    // disconnecting.
                    if let Some(conn) = connections.get_mut(&client_id) {
                        for &channel in &[CHANNEL_SYSTEM, CHANNEL_POSITION] {
                            while let Some(bytes) =
                                server.receive_message(client_id, channel)
                            {
                                if let Some(msg) = handlers::decode_client(&bytes) {
                                    let _ = handlers::handle_message(
                                        &mut server, conn, client_id, msg, now,
                                    );
                                }
                            }
                        }
                    }

                    // Camp + linkdead: a CLEAN leave (Quit Game, or a completed
                    // /camp — both set `clean_disconnect`) reaps the body at once.
                    // An UNCLEAN drop (crash, killed client, network loss) instead
                    // marks the connection linkdead: the body stays in `connections`
                    // with `in_world = true`, so it remains in the targeting
                    // snapshots (vulnerable) and the AOI grid (peers keep seeing it).
                    // The reaper sweep removes it once LINKDEAD_SECS elapse. A
                    // connection not in `connections` (e.g. a refused duplicate login
                    // that never spawned) counts as clean — the reap is then a no-op.
                    let clean = connections
                        .get(&client_id)
                        .map(|c| c.clean_disconnect)
                        .unwrap_or(true);
                    if clean {
                        reap_connection(
                            &mut server,
                            &mut connections,
                            &mut aoi,
                            &mut enemies,
                            &mut group_manager,
                            &pool,
                            client_id,
                            now,
                        )
                        .await;
                    } else {
                        // Despawn the pet now — a linkdead player can't command it,
                        // and an orphaned warder still chasing mobs is worse than a
                        // brief gap. Group membership is KEPT for the window so a
                        // short drop doesn't double-vanish the player from the roster
                        // (the reaper removes them from the group at the end).
                        let owner_entity = connections
                            .get(&client_id)
                            .filter(|c| c.in_world)
                            .map(|c| c.char_id as u64);
                        if let Some(owner_entity) = owner_entity {
                            despawn_owned_pets(
                                &mut server,
                                &connections,
                                &mut aoi,
                                &mut enemies,
                                owner_entity,
                                client_id,
                                now,
                            );
                        }
                        if let Some(conn) = connections.get_mut(&client_id) {
                            conn.linkdead_since = Some(now);
                            // Linkdead governs from here: drop any in-progress
                            // /camp so the camp sweep doesn't also try to complete
                            // it and race the linkdead reaper.
                            conn.camp_since = None;
                            // Freeze the body so it doesn't keep drifting on its
                            // last movement intent (the movement-integration step
                            // also skips linkdead connections; this is belt-and-
                            // suspenders).
                            conn.latest_direction = Vec3f::ZERO;
                            tracing::info!(
                                char_id = conn.char_id,
                                linger_secs = LINKDEAD_SECS.as_secs(),
                                "client linkdead — body lingers (vulnerable) before reap"
                            );
                        }
                    }
                }
            }
        }

        // 3. Drain incoming application messages on each channel for each client.
        let client_ids: Vec<ClientId> = server.clients_id_iter().collect();
        let mut to_disconnect: Vec<ClientId> = Vec::new();
        let mut newly_in_world: Vec<ClientId> = Vec::new();
        // Cast lifecycle events from this tick. Stored verbatim and fanned
        // out in order — coalescing CastStart + CastComplete from the same
        // sender would silently drop a fast-cast complete.
        let mut cast_fanouts: Vec<(ClientId, CastEvent)> = Vec::new();
        // Track 4 sub-task 3 — like resources, dedup per sender so a burst
        // of buff changes inside one tick produces a single fan-out.
        let mut buff_fanouts: Vec<ClientId> = Vec::new();
        // Track 4 sub-task 4 combat events. Verbatim queue (Hit/Miss/Evade
        // are one-shot visuals, ordered).
        let mut combat_fanouts: Vec<(ClientId, CombatEvent)> = Vec::new();
        // Track 4 sub-task 5 — dying clients to fan out as EntityDied.
        // Dedup-on-insert in case the dying client somehow sends Death
        // twice in one tick.
        let mut death_fanouts: Vec<ClientId> = Vec::new();
        // Track 22.H — player target broadcasts queued for the
        // post-dispatch fan-out (which needs the in_world recipients
        // list, not available inside the per-message dispatch).
        let mut player_target_fanouts: Vec<(ClientId, Option<EntityId>)> = Vec::new();
        // Chat fan-out queue. Each entry carries the sender's client id
        // so the drain step can exclude the sender from broadcast
        // recipients and resolve the sender's connection for Tell
        // bounce-backs when the target name doesn't match.
        let mut chat_fanouts: Vec<(ClientId, protocol::world::ChatChannel, String, String, Option<String>)> = Vec::new();
        // Player-inspect requests. `(inspector_client_id, target_char_id)`.
        let mut inspect_intents: Vec<(ClientId, i64)> = Vec::new();
        // Track 5 sub-task 3 — player → server attack intents queued for
        // the apply phase after dispatch. Verbatim queue (each swing is
        // a distinct event; coalescing would silently drop multi-hit
        // combos).
        let mut attack_intents: Vec<AttackIntent> = Vec::new();
        let mut cast_spell_intents: Vec<CastSpellIntent> = Vec::new();
        // Casters already told this tick that an extra CastSpell was dropped.
        let mut cast_overflow_told: Vec<ClientId> = Vec::new();
        // Track 6 sub-task 5 — group intents buffered for the
        // post-dispatch sweep. The sweep needs the full connections
        // map (to resolve names to ids + fan rosters to multiple
        // members), so we can't process inline in handle_message.
        struct GroupInviteI { inviter: u64, target_name: String }
        struct GroupAcceptI { invitee: u64, from: u64 }
        struct GroupLeaveI { member: u64 }
        struct GroupKickI { leader: u64, target_name: String }
        struct GroupLootModeI { leader: u64, mode: u8 }
        struct GroupPassLeadershipI { leader: u64, new_leader: u64 }
        struct AutosplitNoticeI { char_id: u64, on: bool }
        let mut group_invite_intents: Vec<GroupInviteI> = Vec::new();
        let mut group_accept_intents: Vec<GroupAcceptI> = Vec::new();
        let mut group_leave_intents: Vec<GroupLeaveI> = Vec::new();
        let mut group_kick_intents: Vec<GroupKickI> = Vec::new();
        let mut group_loot_mode_intents: Vec<GroupLootModeI> = Vec::new();
        let mut group_pass_leadership_intents: Vec<GroupPassLeadershipI> = Vec::new();
        let mut autosplit_notice_intents: Vec<AutosplitNoticeI> = Vec::new();
        // Track 5 sub-task 4 — player → server loot pickup intents.
        // Verbatim queue; sub-task 4 is FFA loot so order matters for
        // contested bags (first arrival wins the slot).
        let mut loot_intents: Vec<LootIntent> = Vec::new();
        // PD_W0027 — ground pickups to the cursor slot: (looter, bag_id).
        let mut loot_to_cursor_intents: Vec<(u64, u64)> = Vec::new();
        // Corpse / resurrection Slice 3 — (responder, corpse_id, accept) responses
        // to a res offer, applied after dispatch where the corpses map is in scope.
        let mut resurrect_accept_intents: Vec<(u64, protocol::world::EntityId, bool)> = Vec::new();
        // (char_id, zone, pos) — bind writes, persisted immediately (see db::set_bind_point).
        let mut bind_intents: Vec<(i64, Option<String>, (f32, f32, f32))> = Vec::new();
        // PD_W0023 — dev-gated Test Panel spawns, applied alongside the natural
        // spawner pass where the enemies map + AOI grid are in scope.
        let mut dev_spawn_intents: Vec<(super::connection::Vec3f, super::zones::MobTemplate)> =
            Vec::new();
        // PD_W0023 — quest turn-ins (responder, quest_id), applied after dispatch
        // where the DB pool is in scope (the completion persists before the award).
        let mut complete_quest_intents: Vec<(u64, String)> = Vec::new();
        // PD_W0024 — quest accept / abandon lifecycle intents in ONE
        // arrival-order queue (not bucketed per-type), so a same-tick
        // abandon-then-accept (or accept-then-abandon) on the same quest
        // resolves to the client's LAST-stated intent instead of a per-type
        // drain order. The drain only mutates in-memory `active_quests` +
        // marks `quests_dirty`; the end-of-tick flush (step 6-ter) does the
        // single reconciling DB write, so a forged storm can't amplify into an
        // awaited write per message.
        enum QuestLifecycle {
            Accept,
            Abandon,
        }
        let mut quest_lifecycle_intents: Vec<(u64, QuestLifecycle, String)> = Vec::new();
        // Track 12 Piece A — pet commands. Buffered to apply after
        // message dispatch so we can mutate `enemies` (where pets
        // live) without overlapping the handler's mutable
        // `connections` borrow.
        struct PetCommandI { owner: u64, command: u8, target_id: Option<EntityId> }
        let mut pet_command_intents: Vec<PetCommandI> = Vec::new();
        // Track 13.2 — move-item intents. Buffered like pet commands;
        // dispatch runs after the message-drain so we don't overlap
        // the handler's mut borrow on `conn`.
        struct MoveItemI {
            owner: u64,
            src_location: String,
            src_slot: u32,
            dst_location: String,
            dst_slot: u32,
        }
        let mut move_item_intents: Vec<MoveItemI> = Vec::new();
        // PD_W0028 — trade intents, buffered like everything else.
        enum TradeI {
            Request { requester: u64, target: u64 },
            OfferItem { owner: u64, window_slot: u8, from_location: String, from_slot: u32 },
            RetrieveItem { owner: u64, window_slot: u8 },
            OfferCoins { owner: u64, coins: protocol::world::Coins },
            Accept { owner: u64 },
            Cancel { owner: u64 },
        }
        let mut trade_intents: Vec<TradeI> = Vec::new();
        // Track 13.2.b — split + drop intents.
        struct SplitStackI {
            owner: u64,
            src_location: String,
            src_slot: u32,
            dst_location: String,
            dst_slot: u32,
            count: u32,
        }
        let mut split_stack_intents: Vec<SplitStackI> = Vec::new();
        struct DropItemI {
            owner: u64,
            location: String,
            slot: u32,
            count: u32,
        }
        let mut drop_item_intents: Vec<DropItemI> = Vec::new();
        // Track 15.1 — destroy intents (no loot bag).
        struct DestroyItemI {
            owner: u64,
            location: String,
            slot: u32,
            count: u32,
        }
        let mut destroy_item_intents: Vec<DestroyItemI> = Vec::new();
        // Track 15.2 — use-consumable intents.
        struct UseConsumableI {
            owner: u64,
            location: String,
            slot: u32,
        }
        let mut use_consumable_intents: Vec<UseConsumableI> = Vec::new();
        // Track 15.2 follow-up — GM /give intents (server-side spawn).
        struct GmGiveI {
            owner: u64,
            item_name: String,
            qty: u32,
        }
        let mut gm_give_intents: Vec<GmGiveI> = Vec::new();
        // Track 13.3 — equip / unequip intents.
        struct EquipItemI {
            owner: u64,
            src_location: String,
            src_slot: u32,
            equip_slot: u8,
        }
        let mut equip_item_intents: Vec<EquipItemI> = Vec::new();
        struct UnequipItemI {
            owner: u64,
            equip_slot: u8,
            dst_location: String,
            dst_slot: u32,
        }
        let mut unequip_item_intents: Vec<UnequipItemI> = Vec::new();
        struct BuyItemI {
            owner: u64,
            #[allow(dead_code)] // vendor stock validation lands once server NPCs do
            vendor_id: EntityId,
            item_name: String,
            qty: u32,
        }
        let mut buy_item_intents: Vec<BuyItemI> = Vec::new();
        struct SellItemI {
            owner: u64,
            slot: protocol::world::SlotRef,
            qty: u32,
        }
        let mut sell_item_intents: Vec<SellItemI> = Vec::new();
        // PD_W0015 — Banker, slice 1 (coins): deposit / withdraw / exchange.
        struct BankDepositI { owner: u64, coins: protocol::world::Coins }
        let mut bank_deposit_intents: Vec<BankDepositI> = Vec::new();
        struct BankWithdrawI { owner: u64, coins: protocol::world::Coins }
        let mut bank_withdraw_intents: Vec<BankWithdrawI> = Vec::new();
        struct BankExchangeI { owner: u64, from_tier: u8, to_tier: u8, qty: u32 }
        let mut bank_exchange_intents: Vec<BankExchangeI> = Vec::new();
        // PD_W0016 — Banker, slice 2 (item vaults): store / withdraw whole stacks.
        struct BankStoreItemI { owner: u64, src_location: String, src_slot: u32, shared: bool }
        let mut bank_store_item_intents: Vec<BankStoreItemI> = Vec::new();
        struct BankWithdrawItemI { owner: u64, shared: bool, vault_slot: u32 }
        let mut bank_withdraw_item_intents: Vec<BankWithdrawItemI> = Vec::new();
        for client_id in client_ids {
            // Skip clients whose Connected event is in the queue but whose
            // PerConnection row hasn't been built yet (load_character failed
            // and we already queued the kick).
            if !connections.contains_key(&client_id) {
                continue;
            }
            for &channel in &[CHANNEL_SYSTEM, CHANNEL_POSITION] {
                while let Some(bytes) = server.receive_message(client_id, channel) {
                    let Some(msg) = handlers::decode_client(&bytes) else {
                        continue;
                    };
                    let conn = connections.get_mut(&client_id).expect("checked above");
                    match handlers::handle_message(&mut server, conn, client_id, msg, now) {
                        Outcome::Disconnect => to_disconnect.push(client_id),
                        Outcome::JustEnteredWorld => newly_in_world.push(client_id),
                        Outcome::CastStartFanOut {
                            spell_name,
                            duration,
                        } => {
                            cast_fanouts.push((
                                client_id,
                                CastEvent::Start {
                                    spell_name,
                                    duration,
                                },
                            ));
                        }
                        Outcome::CastCompleteFanOut { spell_name } => {
                            cast_fanouts.push((
                                client_id,
                                CastEvent::Complete { spell_name },
                            ));
                        }
                        Outcome::CastFailFanOut { reason } => {
                            cast_fanouts
                                .push((client_id, CastEvent::Fail { reason }));
                        }
                        Outcome::BuffSnapshotFanOut => {
                            if !buff_fanouts.contains(&client_id) {
                                buff_fanouts.push(client_id);
                            }
                        }
                        Outcome::HitFanOut {
                            target,
                            amount,
                            crit,
                            dmg_type,
                        } => {
                            combat_fanouts.push((
                                client_id,
                                CombatEvent::Hit {
                                    target,
                                    amount,
                                    crit,
                                    dmg_type,
                                },
                            ));
                        }
                        Outcome::MissFanOut { target } => {
                            combat_fanouts.push((client_id, CombatEvent::Miss { target }));
                        }
                        Outcome::EvadeFanOut { target } => {
                            combat_fanouts.push((client_id, CombatEvent::Evade { target }));
                        }
                        Outcome::DeathFanOut => {
                            if !death_fanouts.contains(&client_id) {
                                death_fanouts.push(client_id);
                            }
                        }
                        Outcome::PlayerTargetFanOut { target } => {
                            // Track 22.H — queue for the post-dispatch
                            // sweep where in_world_recipients_now is
                            // computed. Drain happens alongside the
                            // other fan-outs.
                            player_target_fanouts.push((client_id, target));
                        }
                        Outcome::ChatFanOut { channel, text, speaker, target_name } => {
                            chat_fanouts.push((client_id, channel, text, speaker, target_name));
                        }
                        Outcome::InspectIntent { target_char_id } => {
                            inspect_intents.push((client_id, target_char_id));
                        }
                        Outcome::AttackIntent {
                            attacker,
                            target_id,
                            is_offhand,
                            dmg_type,
                        } => {
                            attack_intents.push(AttackIntent {
                                attacker,
                                target_id,
                                is_offhand,
                                dmg_type,
                            });
                        }
                        Outcome::CastSpellIntent {
                            caster,
                            spell_name,
                            target_id,
                            cast_name_at_dispatch,
                            cast_set_at_at_dispatch,
                            cast_start_pos_at_dispatch,
                        } => {
                            // One CastSpell per caster per tick. Every CastSpell
                            // in a batch reads the same CastStart cache (it is
                            // cleared only when a cast resolves), so one cast
                            // bar used to authorise any number of them in a
                            // single datagram, and a refused cast is free, so
                            // nothing else metered them. An honest client
                            // cannot finish two casts inside 50 ms. The first
                            // extra is answered (the client spent its mana
                            // optimistically and needs the correction); the
                            // rest of a flood are dropped in silence.
                            if cast_spell_intents.iter().any(|i| i.caster == caster) {
                                if !cast_overflow_told.contains(&client_id) {
                                    cast_overflow_told.push(client_id);
                                    handlers::fan_out_cast_fail(
                                        &mut server,
                                        &[client_id],
                                        caster,
                                        "One spell at a time.".to_string(),
                                    );
                                    if let Some(c) = connections.get(&client_id) {
                                        handlers::fan_out_mana_update(
                                            &mut server,
                                            &[client_id],
                                            caster,
                                            c.mp,
                                            c.max_mp,
                                        );
                                    }
                                }
                            } else {
                                cast_spell_intents.push(CastSpellIntent {
                                    caster,
                                    spell_name,
                                    target_id,
                                    cast_name_at_dispatch,
                                    cast_set_at_at_dispatch,
                                    cast_start_pos_at_dispatch,
                                });
                            }
                        }
                        Outcome::PetCommandIntent { owner, command, target_id } => {
                            pet_command_intents.push(PetCommandI { owner, command, target_id });
                        }
                        Outcome::MoveItemIntent {
                            owner,
                            src_location,
                            src_slot,
                            dst_location,
                            dst_slot,
                        } => {
                            move_item_intents.push(MoveItemI {
                                owner,
                                src_location,
                                src_slot,
                                dst_location,
                                dst_slot,
                            });
                        }
                        Outcome::TradeRequestIntent { requester, target } => {
                            trade_intents.push(TradeI::Request { requester, target });
                        }
                        Outcome::TradeOfferItemIntent {
                            owner,
                            window_slot,
                            from_location,
                            from_slot,
                        } => {
                            trade_intents.push(TradeI::OfferItem {
                                owner,
                                window_slot,
                                from_location,
                                from_slot,
                            });
                        }
                        Outcome::TradeRetrieveItemIntent { owner, window_slot } => {
                            trade_intents.push(TradeI::RetrieveItem { owner, window_slot });
                        }
                        Outcome::TradeOfferCoinsIntent { owner, coins } => {
                            trade_intents.push(TradeI::OfferCoins { owner, coins });
                        }
                        Outcome::TradeAcceptIntent { owner } => {
                            trade_intents.push(TradeI::Accept { owner });
                        }
                        Outcome::TradeCancelIntent { owner } => {
                            trade_intents.push(TradeI::Cancel { owner });
                        }
                        Outcome::SplitStackIntent {
                            owner,
                            src_location,
                            src_slot,
                            dst_location,
                            dst_slot,
                            count,
                        } => {
                            split_stack_intents.push(SplitStackI {
                                owner,
                                src_location,
                                src_slot,
                                dst_location,
                                dst_slot,
                                count,
                            });
                        }
                        Outcome::DropItemIntent {
                            owner,
                            location,
                            slot,
                            count,
                        } => {
                            drop_item_intents.push(DropItemI {
                                owner,
                                location,
                                slot,
                                count,
                            });
                        }
                        Outcome::DestroyItemIntent {
                            owner,
                            location,
                            slot,
                            count,
                        } => {
                            destroy_item_intents.push(DestroyItemI {
                                owner,
                                location,
                                slot,
                                count,
                            });
                        }
                        Outcome::UseConsumableIntent {
                            owner,
                            location,
                            slot,
                        } => {
                            use_consumable_intents.push(UseConsumableI {
                                owner,
                                location,
                                slot,
                            });
                        }
                        Outcome::GmGiveIntent {
                            owner,
                            item_name,
                            qty,
                        } => {
                            gm_give_intents.push(GmGiveI {
                                owner,
                                item_name,
                                qty,
                            });
                        }
                        Outcome::EquipItemIntent {
                            owner,
                            src_location,
                            src_slot,
                            equip_slot,
                        } => {
                            equip_item_intents.push(EquipItemI {
                                owner,
                                src_location,
                                src_slot,
                                equip_slot,
                            });
                        }
                        Outcome::UnequipItemIntent {
                            owner,
                            equip_slot,
                            dst_location,
                            dst_slot,
                        } => {
                            unequip_item_intents.push(UnequipItemI {
                                owner,
                                equip_slot,
                                dst_location,
                                dst_slot,
                            });
                        }
                        Outcome::BuyItemIntent {
                            owner,
                            vendor_id,
                            item_name,
                            qty,
                        } => {
                            buy_item_intents.push(BuyItemI {
                                owner,
                                vendor_id,
                                item_name,
                                qty,
                            });
                        }
                        Outcome::SellItemIntent { owner, slot, qty } => {
                            sell_item_intents.push(SellItemI { owner, slot, qty });
                        }
                        Outcome::BankDepositIntent { owner, coins } => {
                            bank_deposit_intents.push(BankDepositI { owner, coins });
                        }
                        Outcome::BankWithdrawIntent { owner, coins } => {
                            bank_withdraw_intents.push(BankWithdrawI { owner, coins });
                        }
                        Outcome::BankExchangeIntent { owner, from_tier, to_tier, qty } => {
                            bank_exchange_intents.push(BankExchangeI {
                                owner, from_tier, to_tier, qty,
                            });
                        }
                        Outcome::BankStoreItemIntent { owner, src_location, src_slot, shared } => {
                            bank_store_item_intents.push(BankStoreItemI {
                                owner, src_location, src_slot, shared,
                            });
                        }
                        Outcome::BankWithdrawItemIntent { owner, shared, vault_slot } => {
                            bank_withdraw_item_intents.push(BankWithdrawItemI {
                                owner, shared, vault_slot,
                            });
                        }
                        Outcome::GroupInviteIntent { inviter, target_name } => {
                            group_invite_intents.push(GroupInviteI { inviter, target_name });
                        }
                        Outcome::GroupAcceptIntent { invitee, from } => {
                            group_accept_intents.push(GroupAcceptI { invitee, from });
                        }
                        Outcome::GroupLeaveIntent { member } => {
                            group_leave_intents.push(GroupLeaveI { member });
                        }
                        Outcome::GroupKickIntent { leader, target_name } => {
                            group_kick_intents.push(GroupKickI { leader, target_name });
                        }
                        Outcome::SetGroupLootModeIntent { leader, mode } => {
                            group_loot_mode_intents.push(GroupLootModeI { leader, mode });
                        }
                        Outcome::PassLeadershipIntent { leader, new_leader } => {
                            group_pass_leadership_intents
                                .push(GroupPassLeadershipI { leader, new_leader });
                        }
                        Outcome::AutosplitNoticeIntent { char_id, on } => {
                            autosplit_notice_intents
                                .push(AutosplitNoticeI { char_id, on });
                        }
                        Outcome::LootItemIntent {
                            looter,
                            bag_id,
                            slot,
                        } => {
                            loot_intents.push(LootIntent {
                                looter,
                                bag_id,
                                slot: Some(slot),
                            });
                        }
                        Outcome::LootAllIntent { looter, bag_id } => {
                            loot_intents.push(LootIntent {
                                looter,
                                bag_id,
                                slot: None,
                            });
                        }
                        Outcome::LootToCursorIntent { looter, bag_id } => {
                            loot_to_cursor_intents.push((looter, bag_id));
                        }
                        Outcome::ResurrectAcceptIntent { responder, corpse_id, accept } => {
                            resurrect_accept_intents.push((responder, corpse_id, accept));
                        }
                        Outcome::BindIntent { char_id, zone, pos } => {
                            bind_intents.push((char_id, zone, pos));
                        }
                        Outcome::RespawnTeleport { pos } => {
                            // Snap the client to the bind point. Without this the
                            // client keeps rendering the death site until the next
                            // Position broadcast drags it back — the teleport IS
                            // the respawn from the player's point of view.
                            handlers::send_teleport(&mut server, client_id, pos);
                        }
                        Outcome::DevSpawnMobIntent { pos, mob } => {
                            dev_spawn_intents.push((pos, mob));
                        }
                        Outcome::CompleteQuestIntent { responder, quest_id } => {
                            complete_quest_intents.push((responder, quest_id));
                        }
                        Outcome::AcceptQuestIntent { responder, quest_id } => {
                            quest_lifecycle_intents.push((responder, QuestLifecycle::Accept, quest_id));
                        }
                        Outcome::AbandonQuestIntent { responder, quest_id } => {
                            quest_lifecycle_intents.push((responder, QuestLifecycle::Abandon, quest_id));
                        }
                        Outcome::Continue => {}
                    }
                }
            }
        }

        // 3-bis. GM action audit flush. A dev/GM command runs only if
        //    `authorize_dev_cmd` queued its row, and the rows are written
        //    here, straight after the message drain (`reap_connection`
        //    flushes its own connection too, so a command handled in the
        //    same pass as a disconnect is still recorded). One transaction
        //    for the tick. The write volume is bounded over TIME, not just
        //    per tick, by the per-connection command budget: after its burst
        //    a flooding GM client is held to GM_CMD_REFILL_PER_SEC rows a
        //    second plus one overflow summary, and the commands it is refused
        //    do not run.
        let mut audit_rows: Vec<db::GmActionRow> = Vec::new();
        for conn in connections.values_mut() {
            collect_gm_audit(conn, now, false, &mut audit_rows);
        }
        write_gm_audit(&pool, &audit_rows).await;

        // 4. App-layer heartbeat timeout — catches frozen game windows that
        //    transport-level keepalive doesn't notice. Skip connections that
        //    are already lingering linkdead: their transport is gone, so
        //    re-flagging them would just spam disconnect every tick while the
        //    reaper below counts down their window.
        for (client_id, conn) in connections.iter() {
            if conn.linkdead_since.is_some() {
                continue;
            }
            if conn.is_app_idle(now) {
                tracing::info!(
                    char_id = conn.char_id,
                    "app-layer heartbeat timeout — disconnecting"
                );
                to_disconnect.push(*client_id);
            } else if conn
                .kicked_at
                .is_some_and(|t| now.duration_since(t) >= KICK_FLUSH_GRACE)
            {
                // The ban sweep sent this connection its Kick a moment ago;
                // the message has had time to arrive, so drop the transport
                // whether or not the client left by itself. The next tick's
                // disconnect event takes the character out by the ordinary
                // unclean-exit path (linger, final save, reap).
                tracing::info!(char_id = conn.char_id, "kicked connection dropped");
                to_disconnect.push(*client_id);
            }
        }

        for client_id in &to_disconnect {
            server.disconnect(*client_id);
        }

        // 4-bis. Linkdead reaper. A connection marked linkdead on an unclean
        //        disconnect lingers (vulnerable) for LINKDEAD_SECS, then we run
        //        the full disconnect cleanup. renet already dropped its
        //        transport, so reap_connection does NOT call server.disconnect
        //        again — it just despawns, flushes, and removes.
        let to_reap: Vec<ClientId> = connections
            .iter()
            .filter(|(_, c)| c.linkdead_expired(now, LINKDEAD_SECS))
            .map(|(id, _)| *id)
            .collect();
        for client_id in to_reap {
            tracing::info!(%client_id, "linkdead window elapsed — reaping");
            reap_connection(
                &mut server,
                &mut connections,
                &mut aoi,
                &mut enemies,
                &mut group_manager,
                &pool,
                client_id,
                now,
            )
            .await;
        }

        // 4a. EntitySpawn fan-out for clients that just sent `EnterWorld`.
        //     Each new client gets an EntitySpawn for every in_world peer;
        //     each in_world peer gets an EntitySpawn for the new client.
        //     Subject itself skipped — `ConnectOk` is the new client's
        //     own-spawn signal. Reliable channel ⇒ no race-induced lost
        //     spawns.
        for new_id in &newly_in_world {
            // Skip if the new client got disconnected in this same tick
            // (handle_message returned Disconnect on a later message).
            if to_disconnect.contains(new_id) {
                continue;
            }
            if !connections.contains_key(new_id) {
                continue;
            }
            // Track 7: insert the new player into the AOI grid so
            // entities_visible_from returns the correct neighbourhood.
            // peer_ids is filtered to AOI-visible peers only: they're
            // the ones who received EntitySpawn for the new player and
            // should also seed their state in return.
            if let Some(conn) = connections.get(new_id) {
                aoi.insert(conn.char_id as u64, conn.aoi_cell);
            }
            let visible_to_new = connections
                .get(new_id)
                .map(|c| aoi.entities_visible_from(c.aoi_cell))
                .unwrap_or_default();
            let peer_ids: Vec<ClientId> = connections
                .iter()
                .filter(|(id, c)| *id != new_id && c.in_world && visible_to_new.contains(*id))
                .map(|(id, _)| *id)
                .collect();
            // New client → existing peers. Mirror of the "Existing peers
            // → new client" loop below: each existing peer needs EntitySpawn
            // PLUS the new joiner's last-known resource / cast / buff state.
            // Without the cached-state half, a peer who's already in_world
            // when the new client's first ResourceUpdate fans out (step 4b)
            // drops the broadcast (no spawn data yet), and the new client
            // looks like 0/0 HP/MP/Stamina on the existing peer's target
            // frame until the next natural broadcast. Seed at EnterWorld
            // closes that race.
            if let Some(new_conn) = connections.get(new_id) {
                for peer_id in &peer_ids {
                    handlers::send_entity_spawn(&mut server, *peer_id, new_conn);
                    handlers::fan_out_resources(
                        &mut server,
                        std::slice::from_ref(peer_id),
                        new_conn,
                    );
                    if !new_conn.cast_spell_name.is_empty() {
                        if let Some(set_at) = new_conn.cast_set_at {
                            let elapsed = now.duration_since(set_at).as_secs_f32();
                            let remaining = new_conn.cast_total_duration - elapsed;
                            if remaining > 0.0 {
                                handlers::fan_out_cast_start(
                                    &mut server,
                                    std::slice::from_ref(peer_id),
                                    new_conn.char_id as u64,
                                    new_conn.cast_spell_name.clone(),
                                    remaining,
                                );
                            }
                        }
                    }
                    handlers::fan_out_buff_snapshot(
                        &mut server,
                        std::slice::from_ref(peer_id),
                        new_conn,
                    );
                }
            }
            // Existing peers → new client.
            for peer_id in &peer_ids {
                if let Some(peer_conn) = connections.get(peer_id) {
                    handlers::send_entity_spawn(&mut server, *new_id, peer_conn);
                    // Seed the new client with each existing peer's last-
                    // known resources (Track 4). No-op for peers that
                    // haven't broadcast yet; their values land naturally on
                    // the next ResourceUpdate fan-out.
                    handlers::fan_out_resources(
                        &mut server,
                        std::slice::from_ref(new_id),
                        peer_conn,
                    );
                    // Seed cast bar if a peer is mid-cast. Server estimates
                    // remaining time from `cast_set_at`; if it's already
                    // elapsed (peer's CastComplete just hasn't arrived yet,
                    // or the cast was abandoned without a Fail), skip the
                    // seed and let the natural broadcasts catch up.
                    if !peer_conn.cast_spell_name.is_empty() {
                        if let Some(set_at) = peer_conn.cast_set_at {
                            let elapsed = now.duration_since(set_at).as_secs_f32();
                            let remaining = peer_conn.cast_total_duration - elapsed;
                            if remaining > 0.0 {
                                handlers::fan_out_cast_start(
                                    &mut server,
                                    std::slice::from_ref(new_id),
                                    peer_conn.char_id as u64,
                                    peer_conn.cast_spell_name.clone(),
                                    remaining,
                                );
                            }
                        }
                    }
                    // Seed buff snapshot. Sends empty list if the peer has
                    // explicitly broadcast "no buffs" (i.e. cleared), so
                    // the new client doesn't see stale buffs from the
                    // peer's own cache state.
                    handlers::fan_out_buff_snapshot(
                        &mut server,
                        std::slice::from_ref(new_id),
                        peer_conn,
                    );
                }
            }
            // Round-7B fix — seed the new joiner with PetSpawn for every
            // *existing* pet owned by a visible peer. Without this, a
            // pet auto-summoned before the new client entered world
            // never reaches them — the auto-summon's fan_out_pet_spawn
            // only targets `in_world_recipients_now`, which doesn't
            // include the not-yet-arrived player. Playtest report:
            // Boring saw Fun's pet but Fun never saw Boring's pet,
            // and the asymmetry tracked back to spawn ordering.
            for (entity_id, entity) in enemies.iter() {
                if !entity.is_pet() || !entity.is_alive() {
                    continue;
                }
                let Some(owner_id) = entity.owner else { continue };
                // Keyed on the PET's own cell (`visible_to_new` is every id
                // in the joiner's 3x3), the same key step 6b Positions and
                // the cell-crossing fans use. Keying on the owner's cell
                // instead left a joiner either holding a ghost pet it never
                // got Positions for (owner near, pet far) or receiving
                // Positions for a pet it had no spawn for (owner far, pet
                // near).
                if !visible_to_new.contains(entity_id) {
                    continue;
                }
                handlers::fan_out_pet_spawn(
                    &mut server,
                    std::slice::from_ref(new_id),
                    entity,
                );
                tracing::debug!(
                    new_client = *new_id as u64,
                    pet_id = *entity_id,
                    owner = owner_id,
                    "EnterWorld seeded existing pet to new client"
                );
            }
            // Track 13.2 — seed the new joiner with their own inventory
            // snapshot. Private message; peers don't see it. Always
            // fans (even if the snapshot is empty) so the client knows
            // when the seed is complete and can flip into "render
            // from server state" mode.
            if let Some(new_conn) = connections.get(new_id) {
                let entries = new_conn.inventory.to_snapshot_entries();
                handlers::send_inventory_snapshot(&mut server, *new_id, entries);
            }
            // Track 15.1 follow-up — seed the new joiner with their
            // own coin balance. Without this the client sits at the
            // default PlayerStats.coins (0) and the vendor pre-flight
            // "you don't have enough coins" check fires for every
            // purchase. Buy/sell apply phases will keep the client
            // in sync after this seed.
            if let Some(new_conn) = connections.get(new_id) {
                handlers::send_coins_update(&mut server, *new_id, new_conn.coins);
            }
            // The shared sky: a joiner is told the hour at once instead of
            // waiting up to a minute for the next broadcast (step 7-ter).
            handlers::fan_out_time_of_day(
                &mut server,
                std::slice::from_ref(new_id),
                clock::current_hour(),
            );
            // Seed the new joiner with their persisted XP into the current level.
            // ConnectOk carries level but not xp, so `apply_character` leaves the
            // client bar at 0/band until the first XpGained; without this seed a
            // relog reads "0/X" until the next kill snaps it up (playtest report).
            // amount = 0 sets the bar absolutely with NO combat line client-side
            // (`apply_remote_xp` only emits the "gained" line when amount > 0);
            // kill / quest XpGained keep it in sync after.
            if let Some(new_conn) = connections.get(new_id) {
                handlers::send_xp_gained(&mut server, *new_id, 0, new_conn.xp, new_conn.xp_to_next);
            }
            // PD_W0015 — seed the new joiner with their bank balance so the
            // BankWindow shows the right total the moment they open it (the
            // client caches it; deposit/withdraw keep it in sync after).
            if let Some(new_conn) = connections.get(new_id) {
                handlers::send_bank_snapshot(&mut server, *new_id, new_conn.bank_coins);
            }
            // PD_W0016 — seed both item vaults (personal + account-shared) so
            // the BankWindow's Items tab renders correctly on first open.
            if let Some(new_conn) = connections.get(new_id) {
                let personal = new_conn.bank_items.to_snapshot_entries();
                let shared = new_conn.account_bank_items.to_snapshot_entries();
                handlers::send_bank_item_snapshot(&mut server, *new_id, false, personal);
                handlers::send_bank_item_snapshot(&mut server, *new_id, true, shared);
            }
            // Track 18.1 — seed the new joiner with their three
            // passive skill score maps (weapon / armor / casting).
            // Without this the client autoloads sit at uninitialized
            // dicts and the character window renders zeros until the
            // first advance fires.
            if let Some(new_conn) = connections.get(new_id) {
                handlers::send_skill_progress_snapshot(&mut server, *new_id, new_conn);
            }
            // PD_W0024 — seed the joiner's quest journal: every active quest
            // with its per-objective progress, plus the full completed set
            // (so the client greys out re-offers of finished quests). This is
            // what makes the journal survive relog and server restart.
            if let Some(new_conn) = connections.get(new_id) {
                let active: Vec<(String, Vec<i32>)> = new_conn
                    .active_quests
                    .iter()
                    .map(|(id, p)| (id.clone(), p.clone()))
                    .collect();
                let completed: Vec<String> =
                    new_conn.completed_quests.iter().cloned().collect();
                handlers::send_quest_snapshot(&mut server, *new_id, active, completed);
            }
            // Track 5 sub-task 1B — seed the new joiner with alive enemies
            // in their AOI neighbourhood. Track 7: filter by aoi.can_see
            // so only nearby enemies are seeded on enter-world.
            let joiner_cell = connections.get(new_id).map(|c| c.aoi_cell).unwrap_or((0, 0));
            for entity in enemies.values() {
                // Pets share this map and were seeded here as EnemySpawn on
                // top of their PetSpawn above; the client's enemy manager
                // has no partition guard, so a late joiner got a second,
                // enemy-flavoured node riding the pet's Positions.
                if !entity.is_alive() || entity.is_pet() {
                    continue;
                }
                let enemy_cell = aoi::cell_for(entity.pos.x, entity.pos.z);
                if aoi.can_see(joiner_cell, enemy_cell) {
                    handlers::fan_out_enemy_spawn(
                        &mut server,
                        std::slice::from_ref(new_id),
                        entity,
                    );
                }
            }
            // Track 5 sub-task 4 — seed the new joiner with live loot bags
            // in their AOI neighbourhood. Track 7: filter by aoi.can_see.
            for bag in loot_bags.values() {
                let bag_cell = aoi::cell_for(bag.pos.x, bag.pos.z);
                if aoi.can_see(joiner_cell, bag_cell) {
                    handlers::fan_out_loot_bag_spawn(
                        &mut server,
                        std::slice::from_ref(new_id),
                        bag,
                    );
                }
            }
            // Corpse / resurrection Slice 1 — seed the joiner with nearby
            // corpses too (so a relog near your body, or a walk-back, shows it).
            for corpse in corpses.values() {
                let corpse_cell = aoi::cell_for(corpse.pos.x, corpse.pos.z);
                if aoi.can_see(joiner_cell, corpse_cell) {
                    handlers::fan_out_corpse_spawn(
                        &mut server,
                        std::slice::from_ref(new_id),
                        corpse,
                    );
                    // Slice 2 — seed the owner with the contents on relog /
                    // enter-world so they can loot a corpse they spawn next to.
                    if corpse.owner_char == *new_id as i64 {
                        handlers::send_corpse_contents(&mut server, *new_id, corpse);
                    }
                }
            }
        }

        // 4b. (Track 6 removed the client-driven ResourceUpdate fan-out.
        //     Resources are now server-authoritative: regen mutates
        //     `conn.hp` / `conn.mp` / `conn.stamina` in step 4l below and
        //     fans `HealthUpdate` / `ManaUpdate` / `StaminaUpdate` to all
        //     in_world clients including the owner. Step 4a still uses
        //     `fan_out_resources` to seed new joiners with the current
        //     value.)
        let in_world_recipients: Vec<ClientId> = connections
            .iter()
            .filter(|(_, c)| c.in_world)
            .map(|(id, _)| *id)
            .collect();

        // 4d. Buff snapshot fan-out — owning client → every other in_world
        //     peer.
        for sender_id in &buff_fanouts {
            if to_disconnect.contains(sender_id) {
                continue;
            }
            let Some(sender) = connections.get(sender_id) else {
                continue;
            };
            let recipients: Vec<ClientId> = in_world_recipients
                .iter()
                .filter(|id| *id != sender_id)
                .copied()
                .collect();
            handlers::fan_out_buff_snapshot(&mut server, &recipients, sender);
        }

        // 4c. Cast lifecycle fan-out — owning client → every other in_world
        //     peer. Events processed in arrival order so CastStart precedes
        //     CastComplete from the same sender. Sender receives nothing
        //     back; they already know their own cast state.
        for (sender_id, event) in cast_fanouts.drain(..) {
            if to_disconnect.contains(&sender_id) {
                continue;
            }
            let Some(sender) = connections.get(&sender_id) else {
                continue;
            };
            let caster = sender.char_id as u64;
            let recipients: Vec<ClientId> = in_world_recipients
                .iter()
                .filter(|id| **id != sender_id)
                .copied()
                .collect();
            match event {
                CastEvent::Start {
                    spell_name,
                    duration,
                } => handlers::fan_out_cast_start(
                    &mut server,
                    &recipients,
                    caster,
                    spell_name,
                    duration,
                ),
                CastEvent::Complete { spell_name } => handlers::fan_out_cast_complete(
                    &mut server,
                    &recipients,
                    caster,
                    spell_name,
                ),
                CastEvent::Fail { reason } => handlers::fan_out_cast_fail(
                    &mut server,
                    &recipients,
                    caster,
                    reason,
                ),
            }
        }

        // 4e. Combat event fan-out (Hit / Miss / Evade). Same in_world
        //     recipient filter as the cast / buff paths. The target's
        //     own client also receives the broadcast — RemotePlayerManager
        //     filters target == own_id and routes through the
        //     incoming-damage UI path (different render from outgoing).
        for (sender_id, event) in combat_fanouts.drain(..) {
            if to_disconnect.contains(&sender_id) {
                continue;
            }
            let Some(sender) = connections.get(&sender_id) else {
                continue;
            };
            let attacker = sender.char_id as u64;
            let recipients: Vec<ClientId> = in_world_recipients
                .iter()
                .filter(|id| **id != sender_id)
                .copied()
                .collect();
            match event {
                CombatEvent::Hit {
                    target,
                    amount,
                    crit,
                    dmg_type,
                } => handlers::fan_out_hit(
                    &mut server,
                    &recipients,
                    attacker,
                    target,
                    amount,
                    crit,
                    dmg_type,
                ),
                CombatEvent::Miss { target } => {
                    handlers::fan_out_miss(&mut server, &recipients, attacker, target)
                }
                CombatEvent::Evade { target } => {
                    handlers::fan_out_evade(&mut server, &recipients, attacker, target)
                }
            }
        }

        // 4f. Death fan-out — EntityDied to in_world peers. Receiver
        //     RemotePlayer plays a fall-over animation in place; respawn
        //     is implied by the next ResourceUpdate (peer's HP coming
        //     back from 0 → non-zero stands them up). No separate Respawn
        //     variant by design (handoff Q3 option a).
        for sender_id in &death_fanouts {
            if to_disconnect.contains(sender_id) {
                continue;
            }
            let Some(sender) = connections.get(sender_id) else {
                continue;
            };
            let entity_id = sender.char_id as u64;
            let recipients: Vec<ClientId> = in_world_recipients
                .iter()
                .filter(|id| *id != sender_id)
                .copied()
                .collect();
            handlers::fan_out_entity_died(&mut server, &recipients, entity_id);
        }

        // Track 22.H — player target fan-out. Fan an EntityTarget per
        // queued SetTarget intent, skipping the sender (they already
        // know their own choice) and clients flagged for disconnect.
        for (sender_id, target) in player_target_fanouts.drain(..) {
            if to_disconnect.contains(&sender_id) {
                continue;
            }
            let Some(sender) = connections.get(&sender_id) else {
                continue;
            };
            let entity_id = sender.char_id as u64;
            let recipients: Vec<ClientId> = in_world_recipients
                .iter()
                .filter(|id| **id != sender_id)
                .copied()
                .collect();
            handlers::fan_out_entity_target(&mut server, &recipients, entity_id, target);
        }

        // Chat fan-out. Recipient set depends on channel:
        // - Say: AOI 3×3 neighbourhood of the sender, sender excluded.
        // - Shout / Ooc: every in-world client, sender excluded.
        // - Tell: the one connection whose `name` matches `target_name`
        //   case-insensitively. If not found, fan a system message back
        //   to the sender so they know the tell didn't land.
        // - Other channels are ignored at this layer.
        for (sender_id, channel, text, speaker, target_name) in chat_fanouts.drain(..) {
            if to_disconnect.contains(&sender_id) {
                continue;
            }
            use protocol::world::ChatChannel;
            match channel {
                ChatChannel::Say => {
                    let Some(sender) = connections.get(&sender_id) else {
                        continue;
                    };
                    let sender_cell = sender.aoi_cell;
                    let visible = aoi.entities_visible_from(sender_cell);
                    let recipients: Vec<ClientId> = connections
                        .iter()
                        .filter(|(id, c)| {
                            **id != sender_id && c.in_world && visible.contains(&(c.char_id as u64))
                        })
                        .map(|(id, _)| *id)
                        .collect();
                    handlers::fan_out_chat_message(&mut server, &recipients, &speaker, channel, &text);
                }
                ChatChannel::Shout | ChatChannel::Ooc => {
                    let recipients: Vec<ClientId> = in_world_recipients
                        .iter()
                        .filter(|id| **id != sender_id)
                        .copied()
                        .collect();
                    handlers::fan_out_chat_message(&mut server, &recipients, &speaker, channel, &text);
                }
                ChatChannel::Group => {
                    // Fan to every member of the sender's group (except
                    // the sender themself, who added the local echo).
                    let Some(group) = group_manager.group_of(sender_id) else {
                        continue; // sender isn't in a group
                    };
                    let recipients: Vec<ClientId> = group.members
                        .iter()
                        .filter(|id| {
                            **id != sender_id
                                && connections.get(*id).map_or(false, |c| c.in_world)
                        })
                        .copied()
                        .collect();
                    handlers::fan_out_chat_message(&mut server, &recipients, &speaker, channel, &text);
                }
                ChatChannel::Tell => {
                    let Some(target) = target_name.as_deref() else {
                        continue;
                    };
                    let target_lower = target.to_lowercase();
                    let recipient_id = connections
                        .iter()
                        .find(|(_, c)| c.in_world && c.name.to_lowercase() == target_lower)
                        .map(|(id, _)| *id);
                    if let Some(rid) = recipient_id {
                        handlers::fan_out_chat_message(&mut server, &[rid], &speaker, channel, &text);
                    } else {
                        // Bounce: tell the sender the target isn't online.
                        // Use System channel so the client can colour it
                        // distinctly from a real tell.
                        let bounce = format!("{} is not currently playing.", target);
                        handlers::fan_out_chat_message(
                            &mut server,
                            &[sender_id],
                            "",
                            ChatChannel::System,
                            &bounce,
                        );
                    }
                }
                _ => {}
            }
        }

        // Inspect-player drain. Look up target by char_id, ensure they're
        // in-world and within INSPECT_RANGE, pack their paperdoll slot map
        // into `(slot, item_path)` pairs and send back to the inspector only.
        // Empty result for unknown / offline targets — client renders "—"
        // everywhere.
        for (inspector_id, target_char_id) in inspect_intents.drain(..) {
            if to_disconnect.contains(&inspector_id) {
                continue;
            }
            let Some(inspector_pos) = connections.get(&inspector_id).map(|c| c.pos) else {
                continue;
            };
            // Range gate (exploit audit finding 10): a paperdoll used to be
            // readable from anywhere in the world. Written as "keep only
            // when dist <= RANGE" so a non-finite distance refuses instead of
            // slipping past a `dist > RANGE` test (the NaN Move lesson).
            //
            // A target that is too far, offline, or an id that never existed
            // all get the SAME answer: one generic line and an empty result.
            // Naming the target in the refusal, or answering "far" and
            // "absent" differently, would turn the gate into a char_id to
            // name map and an is-online oracle for anyone enumerating ids.
            let target = connections
                .iter()
                .find(|(_, c)| c.char_id == target_char_id && c.in_world)
                .filter(|(_, c)| inspector_pos.distance_to(c.pos) <= INSPECT_RANGE);
            if target.is_none() {
                handlers::send_refusal(
                    &mut server,
                    inspector_id,
                    "That player is not close enough to inspect.",
                );
            }
            let (target_name, slots) = match target {
                Some((_, c)) => {
                    let mut slots: Vec<(u8, String)> = c
                        .inventory
                        .equipment
                        .iter()
                        .map(|(slot, entry)| (*slot, entry.item_path.clone()))
                        .collect();
                    slots.sort_by_key(|(slot, _)| *slot);
                    (c.name.clone(), slots)
                }
                None => (String::new(), Vec::new()),
            };
            handlers::send_inspect_result(
                &mut server,
                inspector_id,
                target_char_id,
                target_name,
                slots,
            );
        }

        // 4g. Enemy spawner phase. Tick the respawn timers; for any spawn
        //     point that fires this frame, instantiate the entity, register
        //     it in the world map, and fan EnemySpawn out to every in_world
        //     client. Recipients computed AFTER the spawner tick so a
        //     client that just sent EnterWorld this tick (and was seeded
        //     with the prior enemy set in step 4a) also receives the new
        //     spawn — no duplicate seeds because the seed loop above ran
        //     against the pre-spawn map.
        {
            let newly_spawned = spawner.tick(now);
            if !newly_spawned.is_empty() {
                let spawn_recipients: Vec<ClientId> = connections
                    .iter()
                    .filter(|(_, c)| c.in_world)
                    .map(|(id, _)| *id)
                    .collect();
                for entity in newly_spawned {
                    // Track 7: add the enemy to the AOI grid so player
                    // cell-change fan-outs can find it, then fan EnemySpawn
                    // only to players who can see its cell.
                    let enemy_cell = aoi::cell_for(entity.pos.x, entity.pos.z);
                    aoi.insert(entity.id, enemy_cell);
                    if !spawn_recipients.is_empty() {
                        let visible = aoi.entities_visible_from(enemy_cell);
                        let aoi_recipients: Vec<ClientId> = spawn_recipients
                            .iter()
                            .copied()
                            .filter(|id| visible.contains(id))
                            .collect();
                        if !aoi_recipients.is_empty() {
                            handlers::fan_out_enemy_spawn(
                                &mut server,
                                &aoi_recipients,
                                &entity,
                            );
                        }
                    }
                    enemies.insert(entity.id, entity);
                }
            }
        }

        // PD_W0023 — dev-gated Test Panel spawns: same AOI-insert + EnemySpawn
        // fan as the natural spawner above, but one-shot (spawn_point_idx =
        // usize::MAX like pets, so death never arms a respawn timer — the
        // spawner's on_enemy_died .get_mut() on it is a safe no-op).
        if !dev_spawn_intents.is_empty() {
            let spawn_recipients: Vec<ClientId> = connections
                .iter()
                .filter(|(_, c)| c.in_world)
                .map(|(id, _)| *id)
                .collect();
            for (pos, mob) in dev_spawn_intents.drain(..) {
                let entity = super::entity::Entity::from_spawn(usize::MAX, pos, mob, now);
                let enemy_cell = aoi::cell_for(entity.pos.x, entity.pos.z);
                aoi.insert(entity.id, enemy_cell);
                if !spawn_recipients.is_empty() {
                    let visible = aoi.entities_visible_from(enemy_cell);
                    let aoi_recipients: Vec<ClientId> = spawn_recipients
                        .iter()
                        .copied()
                        .filter(|id| visible.contains(id))
                        .collect();
                    if !aoi_recipients.is_empty() {
                        handlers::fan_out_enemy_spawn(&mut server, &aoi_recipients, &entity);
                    }
                }
                enemies.insert(entity.id, entity);
            }
        }

        // Recipients snapshot for the enemy-related fan-outs below. Held
        // by the apply / AI / cleanup phases; recomputed here because
        // step 4g (spawner) may have added new in_world states... no, it
        // only adds enemies. Still useful to hoist this once.
        let dt = TICK_DT.as_secs_f32();
        let in_world_recipients_now: Vec<ClientId> = connections
            .iter()
            .filter(|(_, c)| c.in_world)
            .map(|(id, _)| *id)
            .collect();

        // 4g2. Track 12 Piece B — auto-summon a warder for each
        //      freshly-EnterWorld'd Beast Master. Runs once when
        //      `newly_in_world` is non-empty so the spawn lands the
        //      same tick the client's EntitySpawn fan-out happens;
        //      AOI is already populated by the spawn loop above.
        if !newly_in_world.is_empty() {
            let beast_master_summons: Vec<(EntityId, Vec3f, u32)> = newly_in_world
                .iter()
                .filter_map(|cid| {
                    let conn = connections.get(cid)?;
                    if conn.class.eq_ignore_ascii_case("Beast Master") && conn.in_world {
                        Some((conn.char_id as u64, conn.pos, conn.level.max(1) as u32))
                    } else {
                        None
                    }
                })
                .collect();
            for (owner_id, caster_pos, owner_level) in beast_master_summons {
                // Owner-derived warder (pet interim A): deterministic
                // owner - 1, no variance on a free auto-summon.
                let Some((pet_id, pet_level)) = summon_scaled_pet(
                    &mut server,
                    &in_world_recipients_now,
                    &mut enemies,
                    &mut aoi,
                    owner_id,
                    owner_level,
                    caster_pos,
                    "warder",
                    0,
                    1.0,
                    now,
                ) else {
                    continue;
                };
                tracing::info!(
                    owner = owner_id,
                    pet_id,
                    level = pet_level,
                    "Beast Master warder auto-summoned"
                );
            }
        }

        // 4g3. Track 12 Piece B — warder respawn sweep. Beast Masters
        //      whose warder died get a fresh one at 30% HP after
        //      WARDER_RETREAT_SECS (set on death; checked here).
        //      Collected then drained so we don't overlap a
        //      `connections` borrow with the `enemies` mutation in
        //      the helper.
        let due_warder_respawns: Vec<(EntityId, Vec3f, u32)> = connections
            .iter()
            .filter_map(|(_, c)| {
                if !c.in_world { return None; }
                let due = c.warder_respawn_at?;
                // `Instant::duration_since` saturates to zero when the
                // argument is in the future, so the previous
                // `>= 0.0` check was always true and the warder
                // respawned on the next tick instead of after the
                // WARDER_RETREAT_SECS retreat.
                if now >= due {
                    Some((c.char_id as u64, c.pos, c.level.max(1) as u32))
                } else {
                    None
                }
            })
            .collect();
        for (owner_id, caster_pos, owner_level) in due_warder_respawns {
            // Same deterministic owner - 1 as the auto-summon: the respawn
            // is the same free ritual, so no variance here either.
            let Some((pet_id, pet_level)) = summon_scaled_pet(
                &mut server,
                &in_world_recipients_now,
                &mut enemies,
                &mut aoi,
                owner_id,
                owner_level,
                caster_pos,
                "warder",
                0,
                0.3,
                now,
            ) else {
                continue;
            };
            if let Some(conn) = connections.get_mut(&(owner_id as ClientId)) {
                conn.warder_respawn_at = None;
            }
            tracing::info!(
                owner = owner_id,
                pet_id,
                level = pet_level,
                "warder respawned after retreat"
            );
        }

        // 4g4. Track 12 Piece C — charm decay sweep. A pet whose
        //      charm_expires_at has passed is handed back as the mob it
        //      was, and comes for its charmer if they are still here to
        //      be hated (EQ's rule, user call 2026-10-06; v1 despawned it
        //      as "mob runs away"). Collect then drain to avoid an
        //      `enemies` iter+mut overlap.
        let expired_charms: Vec<EntityId> = enemies
            .iter()
            .filter_map(|(id, e)| {
                let exp = e.charm_expires_at?;
                // `Instant::duration_since` saturates to zero when the
                // argument is still in the future, so the old `>= 0.0`
                // test was true on the very next tick: every charm ended
                // 50 ms after it landed (seen in play 2026-10-06, twice).
                // Same bug the warder retreat timer had.
                if now >= exp {
                    Some(*id)
                } else {
                    None
                }
            })
            .collect();
        for pet_id in expired_charms {
            let Some(pet) = enemies.remove(&pet_id) else {
                continue;
            };
            // The charmer, if still in the world and alive.
            let charmer = pet.owner.filter(|o| {
                connections
                    .get(&(*o as ClientId))
                    .map(|c| c.in_world && c.hp > 0.0 && !c.death_processed)
                    .unwrap_or(false)
            });
            let mob_id = return_charmed_mob(
                &mut server,
                &mut aoi,
                &mut enemies,
                &in_world_recipients_now,
                pet,
                charmer,
                now,
            );
            tracing::info!(pet_id, mob_id, charmer = ?charmer, "charm expired — mob returned hostile");
        }

        // 4h. Apply player → server attack intents. The handler queued
        //     these without touching the enemies map; here we run the
        //     server-authoritative damage formula against the attacker's
        //     PerConnection + weapon path, validate the target (alive,
        //     in range), apply damage, and fan out Hit/Miss + HealthUpdate
        //     + (if HP hit zero) EntityDied. A range mismatch or dead
        //     target produces a Miss broadcast so the attacker sees
        //     their swing landed even if cheaty.
        if !attack_intents.is_empty() && !in_world_recipients_now.is_empty() {
            for intent in attack_intents.drain(..) {
                // ClientId is renet's u64 alias and we minted it as char_id,
                // so the attacker's char_id (also u64 on the wire) is the
                // map key directly.
                let attacker_cid = intent.attacker as ClientId;
                // A swing forces the attacker to STAND (you can't fight seated)
                // and marks them in combat, so the seated regen bonus stops (see
                // regen::sitting_bonus_applies). Closes the attack-while-seated
                // behaviour + the latent free-in-combat-regen exploit.
                if let Some(a) = connections.get_mut(&attacker_cid) {
                    a.is_sitting = false;
                    a.last_attack_at = Some(now);
                }
                let Some(attacker_conn) = connections.get(&attacker_cid) else {
                    // Attacker disconnected between sending and apply.
                    continue;
                };
                let attacker_pos = attacker_conn.pos;
                let attacker_zone = attacker_conn.zone.clone();
                // Phase 1 exploit gate — weapon_path trust (audit finding 5). The
                // Attack message carries the weapon the client CLAIMS to swing; a
                // modified client could name a heavier weapon (more damage), a
                // ranged one (more reach), or one that trains a skill it hasn't
                // equipped. Ignore the wire field and read the SERVER's equipment
                // map: slot 0 = main hand, slot 1 = off hand (protocol EquipSlot
                // order, items.rs). Equipping only ever happens through the
                // server-side EquipItem intent, so this map is authoritative and
                // in sync for anything actually worn. An empty slot is an unarmed
                // swing — items::lookup("") falls through to the 1-4 fist /
                // hand_to_hand path in calc_swing + the skill lookup below, so
                // bare-handed combat still works. is_offhand now only selects the
                // slot, so a client can't fabricate an off-hand swing it isn't
                // geared for (empty slot 1 -> fists, not the main-hand weapon).
                let server_weapon_path: String =
                    attacker_conn.equipped_weapon_path(intent.is_offhand);
                // Phase 1 exploit gate — off-hand swings require an actual off-hand
                // weapon. A stock client only sends is_offhand=true when
                // dual-wielding a real slot-1 weapon (combat.gd `_is_dual_wielding`);
                // an off-hand Attack with an EMPTY slot 1 is a forged free second
                // damage stream (it would otherwise resolve to a 1-4 fist swing x the
                // off-hand mult). A bare hand is not an off-hand weapon — reject it.
                if intent.is_offhand && server_weapon_path.is_empty() {
                    tracing::info!(
                        attacker = intent.attacker,
                        "Attack rejected — off-hand swing with no off-hand weapon equipped"
                    );
                    continue;
                }
                // Phase 1 exploit gate — melee swing-rate limit. No swing timer
                // existed, so a modified client could spam Attack for an
                // attack-speed hack. Player auto-attack is client-paced, so the
                // server floors how fast the SAME hand may swing. Keyed PER HAND
                // (main = slot 0, off = slot 1) because dual-wield emits two
                // independent Attack streams that legitimately co-fire in one tick
                // — a single combined timer would false-throttle a legit
                // dual-wielder. The client sends an Attack only on a landed hit and
                // has no burst / catch-up (combat.gd), so arrivals are already >=
                // its swing interval; we only reject "too fast", never "too slow".
                // Dropped silently (no Miss fan-out — that would reward the spammer
                // and desync their local timer). The timestamp advances only on an
                // ACCEPTED swing (below, after calc_swing), so a spammer can't walk
                // the window forward with rejected swings.
                let hand = intent.is_offhand as usize;
                let base_delay = items::lookup(&server_weapon_path)
                    .map(|w| w.weapon_delay)
                    .unwrap_or(FIST_WEAPON_DELAY);
                let min_interval = min_swing_interval_secs(base_delay, intent.is_offhand);
                if swing_too_fast(attacker_conn.last_swing_at[hand], now, min_interval) {
                    tracing::info!(
                        attacker = intent.attacker,
                        is_offhand = intent.is_offhand,
                        weapon_delay = base_delay,
                        min_interval,
                        "Attack rejected — swing-rate limit (too fast)"
                    );
                    continue;
                }
                // Track 6 sub-task 2: server computes the damage roll.
                // Client-supplied amount is ignored — even a malicious
                // client can't claim 999 damage anymore.
                let swing = combat::calc_swing(
                    attacker_conn,
                    &server_weapon_path,
                    intent.is_offhand,
                );
                // Swing accepted at a legal cadence — advance THIS hand's timer.
                // Stamped here on cadence-acceptance (before the range / hit / PvP
                // resolution below) because the client paces by swing attempt, not
                // by landed hit; the attacker_conn immutable borrow ends at
                // calc_swing above, so this get_mut is clear.
                if let Some(a) = connections.get_mut(&attacker_cid) {
                    a.last_swing_at[hand] = Some(now);
                }

                // Track 6 sub-task 3: player-target branch (PvP). The
                // attack-id partition has player char_ids below
                // ENEMY_ID_BASE; anything in that range is a peer. We
                // resolve, gate via combat::can_attack (which today
                // requires both sides flipped /pvp on), apply HP delta,
                // and fan Hit + HealthUpdate. Self-attack guarded;
                // dying via PvP routes through the regular client-side
                // PlayerDeath flow (which fires DeathBroadcast on its
                // own) until sub-task 4 lifts death detection server-
                // authoritative.
                if intent.target_id < protocol::world::ENEMY_ID_BASE {
                    if intent.target_id == intent.attacker {
                        continue;
                    }
                    let target_cid = intent.target_id as ClientId;
                    let target_zone = connections
                        .get(&target_cid)
                        .and_then(|c| c.zone.clone());
                    let allowed_pvp = match (
                        connections.get(&attacker_cid),
                        connections.get(&target_cid),
                    ) {
                        (Some(a), Some(t)) => combat::can_attack(
                            a, t,
                            attacker_zone.as_deref(),
                            target_zone.as_deref(),
                        ),
                        _ => false,
                    };
                    let target_in_range_alive = connections
                        .get(&target_cid)
                        .map(|t| {
                            t.in_world
                                && t.hp > 0.0
                                && t.pos.distance_to(attacker_pos) <= match items::lookup(
                                    &server_weapon_path,
                                ) {
                                    Some(w) if w.is_ranged => RANGED_ATTACK_RANGE,
                                    _ => 3.0 * ATTACK_RANGE_TOLERANCE,
                                }
                        })
                        .unwrap_or(false);
                    if !allowed_pvp {
                        let name = connections
                            .get(&target_cid)
                            .map(|c| c.name.clone())
                            .unwrap_or_else(|| "that target".to_string());
                        handlers::fan_out_chat_message(
                            &mut server,
                            &[attacker_cid],
                            "",
                            protocol::world::ChatChannel::System,
                            &format!("Unable to attack {}.", name),
                        );
                        tracing::debug!(
                            attacker = intent.attacker,
                            target = intent.target_id,
                            "PvP not authorized"
                        );
                        continue;
                    }
                    if !target_in_range_alive {
                        handlers::fan_out_miss(
                            &mut server,
                            &in_world_recipients_now,
                            intent.attacker,
                            intent.target_id,
                        );
                        continue;
                    }
                    // Apply damage. Armor reduction matches the
                    // GDScript Combat.receive_player_damage:
                    //   reduction = armor / (armor + ARMOR_DR_DIVISOR)
                    // with ARMOR_DR_DIVISOR = 100. Track 6 sub-task
                    // 4c: absorb pool consumed before HP deduction;
                    // damage shield reflects damage back to attacker
                    // after.
                    let raw_swing = swing.amount;
                    let shield_to_attacker_pvp: f32;
                    let shield_name_pvp: Option<String>;
                    let mut absorb_strip_pvp: Option<usize> = None;
                    let (new_hp, max_hp, amount, target_armor) = {
                        let target_conn = connections.get_mut(&target_cid).expect("checked");
                        let armor = target_conn.equipped_armor.max(0) as f32;
                        let reduction = armor / (armor + 100.0);
                        let mut amount = ((swing.amount as f32 * (1.0 - reduction)) as i32).max(1);
                        let (after_absorb, exhausted) =
                            buffs::consume_absorb(&mut target_conn.active_buffs, amount);
                        amount = after_absorb;
                        if exhausted {
                            absorb_strip_pvp = target_conn.active_buffs.iter().position(|b| {
                                matches!(b.effect, buffs::BuffEffect::Absorb { .. })
                            });
                        }
                        shield_to_attacker_pvp =
                            buffs::damage_shield_total(&target_conn.active_buffs);
                        shield_name_pvp = buffs::first_damage_shield_name(&target_conn.active_buffs).map(|s| s.to_string());
                        target_conn.hp = (target_conn.hp - amount as f32).max(0.0);
                        regen::mark_dirty(target_conn);
                        target_conn.note_damage_taken(now); // camp breaks on damage
                        (target_conn.hp, target_conn.max_hp, amount, target_conn.equipped_armor)
                    };
                    // Strip exhausted absorb buff + fan snapshot.
                    if let Some(idx) = absorb_strip_pvp {
                        if let Some(tc) = connections.get_mut(&target_cid) {
                            if idx < tc.active_buffs.len() {
                                tc.active_buffs.remove(idx);
                            }
                        }
                        if let Some(tc) = connections.get(&target_cid) {
                            fan_out_server_buff_snapshot(
                                &mut server,
                                &in_world_recipients_now,
                                tc,
                            );
                        }
                    }
                    // Damage shield reflects damage to the attacker
                    // (also a player here). Skip if the attacker has
                    // since disconnected.
                    if shield_to_attacker_pvp > 0.0 {
                        if let Some(att) = connections.get_mut(&attacker_cid) {
                            if att.hp > 0.0 {
                                let dmg = shield_to_attacker_pvp;
                                let reflect_dmg = dmg as i32;
                                att.hp = (att.hp - dmg).max(0.0);
                                regen::mark_dirty(att);
                                att.note_damage_taken(now); // camp breaks on damage
                                let new_att_hp = att.hp;
                                let att_max = att.max_hp;
                                handlers::fan_out_health_update(
                                    &mut server,
                                    &in_world_recipients_now,
                                    intent.attacker,
                                    new_att_hp,
                                    att_max,
                                );
                                if let Some(name) = shield_name_pvp.as_ref() {
                                    handlers::fan_out_damage_shield_trigger(
                                        &mut server,
                                        &in_world_recipients_now,
                                        intent.target_id,
                                        intent.attacker,
                                        reflect_dmg,
                                        name.clone(),
                                    );
                                }
                                // Track 22.E — reflected damage also
                                // interrupts an in-flight cast. Classic
                                // EQ: any incoming damage rolls the
                                // channeling check, regardless of source.
                                let outcome = roll_cast_interrupt(att, now);
                                match &outcome {
                                    InterruptOutcome::Interrupted { spell_name } => {
                                        handlers::fan_out_cast_fail(
                                            &mut server,
                                            &in_world_recipients_now,
                                            intent.attacker,
                                            "interrupted (hit during cast)".to_string(),
                                        );
                                        tracing::info!(
                                            caster = intent.attacker,
                                            spell = %spell_name,
                                            "PvP cast interrupted by damage shield reflect"
                                        );
                                    }
                                    InterruptOutcome::Survived {
                                        advanced_to: Some(new_score),
                                    } => {
                                        handlers::send_skill_progress_update(
                                            &mut server,
                                            attacker_cid,
                                            skills::Skill::Casting.as_protocol(),
                                            "channeling".to_string(),
                                            *new_score,
                                        );
                                    }
                                    InterruptOutcome::Survived { advanced_to: None }
                                    | InterruptOutcome::NotCasting => {}
                                }
                            }
                        }
                    }
                    tracing::info!(
                        attacker = intent.attacker,
                        target = intent.target_id,
                        raw_swing,
                        target_armor,
                        applied = amount,
                        target_hp = new_hp,
                        "PvP attack applied"
                    );
                    // Track 19A — PvP hit can interrupt a cast too.
                    // Re-borrow target_conn now that the damage block
                    // has released it; same helper as the enemy arm.
                    if let Some(tc) = connections.get_mut(&target_cid) {
                        let interrupt_outcome = roll_cast_interrupt(tc, now);
                        match &interrupt_outcome {
                            InterruptOutcome::Interrupted { spell_name } => {
                                handlers::fan_out_cast_fail(
                                    &mut server,
                                    &in_world_recipients_now,
                                    intent.target_id,
                                    "interrupted (hit during cast)".to_string(),
                                );
                                tracing::info!(
                                    caster = intent.target_id,
                                    spell = %spell_name,
                                    "PvP cast interrupted by incoming damage"
                                );
                            }
                            InterruptOutcome::Survived {
                                advanced_to: Some(new_score),
                            } => {
                                handlers::send_skill_progress_update(
                                    &mut server,
                                    target_cid,
                                    skills::Skill::Casting.as_protocol(),
                                    "channeling".to_string(),
                                    *new_score,
                                );
                            }
                            InterruptOutcome::Survived { advanced_to: None }
                            | InterruptOutcome::NotCasting => {}
                        }
                    }
                    handlers::fan_out_hit(
                        &mut server,
                        &in_world_recipients_now,
                        intent.attacker,
                        intent.target_id,
                        amount,
                        swing.crit,
                        intent.dmg_type,
                    );
                    handlers::fan_out_health_update(
                        &mut server,
                        &in_world_recipients_now,
                        intent.target_id,
                        new_hp,
                        max_hp,
                    );
                    continue;
                }

                // Pet PvP gate. Pets inherit their owner's PvP flag —
                // a player can't damage another player's pet unless
                // can_attack between the players would already allow
                // a direct hit. NPC-owned pets (none today, but
                // charm/PetSummon paths exist) and regular mobs have
                // `entity.owner == None` and fall through. Look up
                // owner first via an immutable borrow so the
                // mutable damage borrow below remains valid.
                if intent.target_id >= protocol::world::PET_ID_BASE {
                    let pet_owner = enemies.get(&intent.target_id).and_then(|e| e.owner);
                    if let Some(owner_id) = pet_owner {
                        let owner_cid = owner_id as ClientId;
                        let allowed = if owner_cid == attacker_cid {
                            false  // attacking your own pet — defensive guard
                        } else {
                            match (
                                connections.get(&attacker_cid),
                                connections.get(&owner_cid),
                            ) {
                                (Some(a), Some(o)) => combat::can_attack(
                                    a, o,
                                    a.zone.as_deref(),
                                    o.zone.as_deref(),
                                ),
                                _ => false,
                            }
                        };
                        if !allowed {
                            // Resolve "Owner's PetName." for the chat
                            // line. Owner name lives on the connection;
                            // pet display name lives on the Entity's mob
                            // template. Either may be missing if the
                            // owner just disconnected or the pet's
                            // template lookup is degraded — fall back
                            // to a generic message in that case.
                            let owner_name = connections
                                .get(&(owner_id as ClientId))
                                .map(|c| c.name.clone())
                                .unwrap_or_default();
                            let pet_name = enemies
                                .get(&intent.target_id)
                                .map(|p| p.mob.name.clone())
                                .unwrap_or_default();
                            let line = if !owner_name.is_empty() && !pet_name.is_empty() {
                                format!("Unable to attack {}'s {}.", owner_name, pet_name)
                            } else if !pet_name.is_empty() {
                                format!("Unable to attack the {}.", pet_name)
                            } else {
                                "Unable to attack that target.".to_string()
                            };
                            handlers::fan_out_chat_message(
                                &mut server,
                                &[attacker_cid],
                                "",
                                protocol::world::ChatChannel::System,
                                &line,
                            );
                            tracing::debug!(
                                attacker = intent.attacker,
                                target = intent.target_id,
                                owner = owner_id,
                                "PvP not authorized vs pet"
                            );
                            continue;
                        }
                    }
                }
                let Some(entity) = enemies.get_mut(&intent.target_id) else {
                    handlers::fan_out_miss(
                        &mut server,
                        &in_world_recipients_now,
                        intent.attacker,
                        intent.target_id,
                    );
                    continue;
                };
                if !entity.is_alive() {
                    handlers::fan_out_miss(
                        &mut server,
                        &in_world_recipients_now,
                        intent.attacker,
                        intent.target_id,
                    );
                    continue;
                }
                let dist = entity.pos.distance_to(attacker_pos);
                // Track 6 sub-task 2 (fix): ranged weapons use a much
                // larger range. Without this branch, bows at >2.7m
                // produce silent Miss broadcasts even though the swing
                // visually fired. Lookup is by weapon_path; an empty or
                // unknown path uses the melee envelope.
                let allowed = match items::lookup(&server_weapon_path) {
                    Some(w) if w.is_ranged => RANGED_ATTACK_RANGE,
                    _ => entity.melee_range() * ATTACK_RANGE_TOLERANCE,
                };
                if dist > allowed {
                    tracing::debug!(
                        attacker = intent.attacker,
                        target = intent.target_id,
                        dist,
                        allowed,
                        weapon = %server_weapon_path,
                        "attack out of range, fanning Miss"
                    );
                    handlers::fan_out_miss(
                        &mut server,
                        &in_world_recipients_now,
                        intent.attacker,
                        intent.target_id,
                    );
                    continue;
                }
                let amount = swing.amount.max(0);
                entity.hp = (entity.hp - amount as f32).max(0.0);
                *entity.aggro.entry(intent.attacker).or_insert(0.0) += amount as f32;
                // Track 12 Piece A2 — also tracks per-actual-attacker
                // threat for the AI re-target check. Mirrors aggro
                // for a player attacker (no pet remapping here).
                *entity.threat.entry(intent.attacker).or_insert(0.0) += amount as f32;
                // Track 11.3 — record the attacker's last hit on an
                // enemy so their pet (if any) can inherit the target
                // on its next AI tick. Decayed by the pet's AI
                // resolution step; refreshed on every swing.
                let target_for_pet = intent.target_id;
                if let Some(att) = connections.get_mut(&attacker_cid) {
                    att.last_attacked_enemy = Some(target_for_pet);
                    att.last_attacked_at = Some(now);
                }
                // Track 18.1 — weapon skill advance on a landed hit.
                // Skill key from the weapon's `skill` field (defaults
                // to `hand_to_hand` for fists / unrecognised items).
                // Fan a SkillProgressUpdate privately if the roll
                // lands; client mirrors the score for the character
                // window.
                let weapon_skill_key: String = items::lookup(&server_weapon_path)
                    .map(|w| {
                        if w.skill.is_empty() {
                            "hand_to_hand".to_string()
                        } else {
                            w.skill.clone()
                        }
                    })
                    .unwrap_or_else(|| "hand_to_hand".to_string());
                if let Some(att) = connections.get_mut(&attacker_cid) {
                    if let Some(new_score) =
                        skills::try_advance(att, skills::Skill::Weapon, &weapon_skill_key)
                    {
                        handlers::send_skill_progress_update(
                            &mut server,
                            attacker_cid,
                            skills::Skill::Weapon.as_protocol(),
                            weapon_skill_key.clone(),
                            new_score,
                        );
                    }
                    // Defense skill also advances on a successful
                    // offensive swing (mirrors the GDScript Combat
                    // path: defense ticks on connect AND on dodge).
                    if let Some(new_score) =
                        skills::try_advance(att, skills::Skill::Weapon, "defense")
                    {
                        handlers::send_skill_progress_update(
                            &mut server,
                            attacker_cid,
                            skills::Skill::Weapon.as_protocol(),
                            "defense".to_string(),
                            new_score,
                        );
                    }
                }
                // PD_W0025 — server-authoritative weapon proc. Roll the equipped
                // weapon's proc_chance on this landed swing; on a proc, fold the
                // proc_damage into THIS swing's resolution (same entity borrow +
                // aggro, and the death/loot/xp block below), so a proc killing
                // blow is credited/looted correctly with no duplicate cascade, and
                // announce it via ProcTriggered so the client renders the named
                // "<proc> for N" hit. Only fires if the main swing didn't already
                // kill (no proc-on-a-corpse). Flat 5% proc crit (1.5-2.0x) mirrors
                // the old client roll; no elemental resist (the server has no
                // enemy-resist model — enemies take raw damage). This replaces the
                // old client-sent proc Attack (which double-hit and is now dropped
                // by the swing-rate limit).
                if entity.hp > 0.0 {
                    if let Some(w) = items::lookup(&server_weapon_path) {
                        if w.proc_chance > 0.0 && w.proc_damage > 0 {
                            let mut rng = rand::thread_rng();
                            if rng.gen::<f32>() < w.proc_chance {
                                let proc_crit = rng.gen::<f32>() < 0.05;
                                let proc_dmg = (if proc_crit {
                                    (w.proc_damage as f32 * rng.gen_range(1.5..=2.0)) as i32
                                } else {
                                    w.proc_damage
                                })
                                .max(1);
                                entity.hp = (entity.hp - proc_dmg as f32).max(0.0);
                                *entity.aggro.entry(intent.attacker).or_insert(0.0) +=
                                    proc_dmg as f32;
                                *entity.threat.entry(intent.attacker).or_insert(0.0) +=
                                    proc_dmg as f32;
                                // Private to the attacker — the proc's named
                                // number/flash/log is their flavor; the mob's HP
                                // drop is already fanned to everyone via the
                                // HealthUpdate below.
                                handlers::fan_out_proc_triggered(
                                    &mut server,
                                    &[attacker_cid],
                                    intent.attacker,
                                    intent.target_id,
                                    w.proc_name.clone(),
                                    proc_dmg,
                                    proc_crit,
                                    spells::proc_damage_type_to_wire(w.proc_damage_type),
                                );
                            }
                        }
                    }
                }
                handlers::fan_out_hit(
                    &mut server,
                    &in_world_recipients_now,
                    intent.attacker,
                    intent.target_id,
                    amount,
                    swing.crit,
                    intent.dmg_type,
                );
                handlers::fan_out_health_update(
                    &mut server,
                    &in_world_recipients_now,
                    entity.id,
                    entity.hp,
                    entity.max_hp,
                );
                if entity.hp <= 0.0 {
                    entity.transition(EnemyState::Dead, now);
                    handlers::fan_out_entity_died(
                        &mut server,
                        &in_world_recipients_now,
                        entity.id,
                    );
                    // Warder respawn — if a player kill (PvP or
                    // accidental) drops a Beast Master warder, schedule
                    // the same retreat-and-respawn the enemy-kill path
                    // at line ~5011 uses. Without this the pet stays
                    // dead until the owner re-zones.
                    if super::pet_templates::is_warder_template(&entity.mob.name) {
                        if let Some(owner_id) = entity.owner {
                            const WARDER_RETREAT_SECS: f32 = 15.0;
                            let due = now
                                + std::time::Duration::from_secs_f32(WARDER_RETREAT_SECS);
                            let owner_cid = owner_id as ClientId;
                            if let Some(conn) = connections.get_mut(&owner_cid) {
                                conn.warder_respawn_at = Some(due);
                                tracing::info!(
                                    owner = owner_cid as u64,
                                    killer = intent.attacker,
                                    retreat_secs = WARDER_RETREAT_SECS,
                                    "warder retreating after player kill",
                                );
                            }
                        }
                    }
                    // Kill credit: pick the top damager from the aggro
                    // table and send a private XpGained. Solo-only
                    // semantics — the legacy enet GroupManager path
                    // splits XP locally and is out of scope for the
                    // server's renet view. HashMap iteration order is
                    // non-deterministic, so max_by with the partial_cmp
                    // tiebreak is stable enough for the single-attacker
                    // case (only one entry).
                    if let Some((&credit_id, _)) = entity
                        .aggro
                        .iter()
                        .max_by(|a, b| {
                            a.1.partial_cmp(b.1)
                                .unwrap_or(std::cmp::Ordering::Equal)
                        })
                    {
                        // EQ quadratic per-kill award from the mob's level (see
                        // progression::kill_xp), split across the killer's
                        // group + quest credit via the shared helper (same
                        // path as spell and pet kills).
                        let base_xp = super::progression::kill_xp(entity.mob.level as i32, super::progression::ZEM_NORMAL);
                        let mob_name = entity.mob.name.clone();
                        // Owned victims (pets / charmed mobs) grant no quest
                        // credit — the warder is literally named "Wolf".
                        let victim_owned = entity.owner.is_some();
                        let victim_pos = entity.pos;
                        award_kill(
                            &mut server,
                            &mut connections,
                            &group_manager,
                            credit_id,
                            base_xp,
                            &mob_name,
                            victim_owned,
                            victim_pos,
                        );
                    }
                    // Roll loot from the mob's archetype table; spawn
                    // a server-owned bag at the death pos if any
                    // stacks landed. Empty rolls produce no bag at all
                    // (matches the GDScript behaviour where the local
                    // Loot autoload simply returns without instantiating
                    // a node).
                    // Loot ownership: the top damager (kill-creditor) owns
                    // the corpse; their group shares rights, resolved at
                    // loot time (see loot::LootBag::can_loot). Filter to the
                    // player id range — if a pet/enemy was the top damager
                    // the bag falls back to public rather than locking out.
                    let owner_cid_opt: Option<ClientId> = entity
                        .aggro
                        .iter()
                        .max_by(|a, b| {
                            a.1.partial_cmp(b.1)
                                .unwrap_or(std::cmp::Ordering::Equal)
                        })
                        .map(|(&id, _)| id)
                        .filter(|&id| id < protocol::world::ENEMY_ID_BASE)
                        .map(|id| id as ClientId);
                    let loot_items = loot::roll_for_mob(&entity.mob.name).unwrap_or_default();
                    let loot_coins =
                        loot::roll_coin_for_mob(&entity.mob.name, entity.mob.level);
                    if !loot_items.is_empty() || loot_coins != protocol::world::Coins::ZERO {
                        let stacks_for_log = loot_items.len();
                        let bag = LootBag::new(
                            entity.pos,
                            loot_items,
                            loot_coins,
                            entity.mob.name.clone(),
                            owner_cid_opt,
                            now,
                        );
                        let bag_id = bag.id;
                        // Track 7: add bag to AOI; fan LootBagSpawn only
                        // to players who can see the bag's cell.
                        let bag_cell = aoi::cell_for(bag.pos.x, bag.pos.z);
                        aoi.insert(bag_id, bag_cell);
                        let visible = aoi.entities_visible_from(bag_cell);
                        let bag_recipients: Vec<ClientId> = in_world_recipients_now
                            .iter()
                            .copied()
                            .filter(|id| visible.contains(id))
                            .collect();
                        if !bag_recipients.is_empty() {
                            handlers::fan_out_loot_bag_spawn(
                                &mut server,
                                &bag_recipients,
                                &bag,
                            );
                        }
                        loot_bags.insert(bag.id, bag);
                        tracing::info!(
                            mob = %entity.mob.name,
                            bag_id,
                            stacks = stacks_for_log,
                            "loot bag spawned"
                        );
                    }
                }
            }
        }

        // 4ha. Apply player → server spell-cast intents. Server resolves
        //      the spell in spells.toml, validates the cast-time gate
        //      (Track 10) + mana cost + target, and applies
        //      authoritative damage (ENEMY/AOE) or heal (SELF/ALLY).
        if !cast_spell_intents.is_empty() {
            for intent in cast_spell_intents.drain(..) {
                let caster_cid = intent.caster as ClientId;
                let Some(spell) = spells::lookup(&intent.spell_name) else {
                    tracing::info!(
                        caster = intent.caster,
                        spell = %intent.spell_name,
                        "unknown spell name — server-side cast dropped"
                    );
                    // The client spends mana, starts the cooldown and applies
                    // local buffs BEFORE it sends the cast, so a spell the server
                    // has never heard of drains the bar and does nothing at all.
                    // Roughly 32 spells exist client-side but not in spells.toml,
                    // so this fires in ordinary play and reads as the spell being
                    // broken rather than missing.
                    //
                    // The server never deducted anything here, so its own mana is
                    // still correct: sending it back corrects the client's
                    // optimistic spend. Same principle as correcting a rejected
                    // MoveItem's slots — answer with the truth.
                    if let Some(conn) = connections.get(&caster_cid) {
                        let (id, mp, max_mp) = (conn.char_id as u64, conn.mp, conn.max_mp);
                        handlers::fan_out_mana_update(&mut server, &[caster_cid], id, mp, max_mp);
                    }
                    handlers::send_refusal(
                        &mut server,
                        caster_cid,
                        "That spell fizzles — it isn't known here yet.",
                    );
                    continue;
                };
                // Spell batch step 0: a corpse casts nothing. The death sweep
                // runs after this step in the tick, so a cast whose bar ended
                // in the tick the caster's hp hit zero used to complete: a
                // self-heal would have revived them past the death penalty,
                // and Gate would have moved the corpse.
                if connections
                    .get(&caster_cid)
                    .map_or(true, |c| c.hp <= 0.0 || c.death_processed)
                {
                    tracing::info!(
                        caster = intent.caster,
                        spell = %spell.name,
                        "CastSpell rejected — caster is dead"
                    );
                    refuse_cast(&mut server, &connections, caster_cid, intent.caster, "You are dead.");
                    continue;
                }
                // Phase 1 exploit gate — class/level eligibility. Any cast that
                // reaches here names a real spell; verify the caster's class is in
                // the spell's `classes` and the caster meets `min_level`. A legit
                // client only offers spells your class knows, so this rejects a
                // forged CastSpell (the audit's "any class can cast any spell it can
                // name"). Checked before the mana / cooldown / skill side effects so
                // a rejected forgery costs nothing to retry against. Every spell in
                // spells.toml has a non-empty `classes` and a `min_level`, so no
                // legit spell trips this on missing data. The CORPSE / resurrection
                // arm keeps its own copy as defense-in-depth.
                {
                    let caster_cl = connections
                        .get(&caster_cid)
                        .map(|c| (c.class.clone(), c.level));
                    let class_ok = caster_cl
                        .as_ref()
                        .map(|(class, _)| spell.classes.iter().any(|cl| cl == class))
                        .unwrap_or(false);
                    let level_ok = caster_cl
                        .as_ref()
                        .map(|(_, level)| *level >= spell.min_level)
                        .unwrap_or(false);
                    if !(class_ok && level_ok) {
                        let (class, level) = caster_cl.unwrap_or_default();
                        let reason = if !class_ok {
                            "Your class cannot cast that."
                        } else {
                            "You are not high enough level for that spell."
                        };
                        tracing::info!(
                            caster = intent.caster,
                            spell = %spell.name,
                            class = %class,
                            level,
                            required_level = spell.min_level,
                            class_ok,
                            "CastSpell rejected — class/level not eligible"
                        );
                        refuse_cast(&mut server, &connections, caster_cid, intent.caster, reason);
                        continue;
                    }
                }
                // Track 10 — cast-time gate. Instant casts (cast_time
                // == 0) skip; for timed casts we require a matching
                // CastStartBroadcast on file with enough wall time
                // elapsed. The 100 ms tolerance absorbs network jitter
                // — packet loss can push arrival past the cast bar
                // ending, but a forged CastSpell sent the same tick as
                // CastStart will fall well short.
                if spell.cast_time > 0.0 {
                    use super::cast_gate::{self, CastTiming};
                    let required_ms = (spell.cast_time * 1000.0) as u128;
                    let name_matches = intent.cast_name_at_dispatch == spell.name;
                    let elapsed_ms = intent
                        .cast_set_at_at_dispatch
                        .map(|set_at| now.duration_since(set_at).as_millis());
                    let timing = if name_matches {
                        cast_gate::cast_timing(elapsed_ms, required_ms)
                    } else {
                        CastTiming::NotReady
                    };
                    match timing {
                        CastTiming::Ready => {}
                        CastTiming::NotReady => {
                            tracing::info!(
                                caster = intent.caster,
                                spell = %spell.name,
                                cast_time = spell.cast_time,
                                in_flight = %intent.cast_name_at_dispatch,
                                "CastSpell rejected — cast-time gate (no matching CastStart or too early)"
                            );
                            refuse_cast(&mut server, &connections, caster_cid, intent.caster, "cast not ready");
                            continue;
                        }
                        CastTiming::Held => {
                            // Spell batch step 0: a finished bar kept in hand
                            // and released later (a pre-cast heal fired the
                            // instant it is needed) is refused, and the stale
                            // bar is dropped so the next cast starts clean.
                            tracing::info!(
                                caster = intent.caster,
                                spell = %spell.name,
                                elapsed_ms = elapsed_ms.unwrap_or(0),
                                "CastSpell rejected — held past the bar"
                            );
                            if let Some(cc) = connections.get_mut(&caster_cid) {
                                cc.cast_spell_name.clear();
                                cc.cast_total_duration = 0.0;
                                cc.cast_set_at = None;
                            }
                            refuse_cast(&mut server, &connections, caster_cid, intent.caster, "That cast has lapsed.");
                            continue;
                        }
                    }
                    // Spell batch step 0: an interrupt that landed after this
                    // bar began beats the cast, even in the same tick. The
                    // gate above reads the cache as it was at dispatch, which
                    // also meant a hit processed earlier in this tick had
                    // already cleared the live cache and fanned "interrupted",
                    // and the cast still went through: both won.
                    let interrupted_at = connections
                        .get(&caster_cid)
                        .and_then(|c| c.cast_interrupted_at);
                    if cast_gate::beaten_by_interrupt(interrupted_at, intent.cast_set_at_at_dispatch) {
                        tracing::info!(
                            caster = intent.caster,
                            spell = %spell.name,
                            "CastSpell rejected — interrupted after the bar began"
                        );
                        refuse_cast(&mut server, &connections, caster_cid, intent.caster, "interrupted (hit during cast)");
                        continue;
                    }
                    // Track 17.2 — movement-during-cast gate. Compare
                    // current caster pos to the snapshot taken at
                    // CastStart. >MAX_CAST_MOVE_DISTANCE cancels the
                    // cast.
                    //
                    // CORRECTED 2026-08-25: an earlier version of this
                    // comment claimed "the client cancels its own cast on
                    // movement already". It does not — the 08-24 audit
                    // found no client-side movement cancel at all (the
                    // interrupt path early-returns in launcher mode, and no
                    // keybind/ESC/cancel-button exists). This gate is the
                    // ONLY line of defence, for honest and forged clients
                    // alike, so do not weaken it on the assumption there is
                    // a client backstop.
                    //
                    // Threshold is generous (5 m, vs ~4 m max coast
                    // from STALE_MOVE_THRESHOLD) so a player who
                    // stopped just before the cast doesn't get pinged
                    // for the half-second of server-side drift their
                    // last Move authorized. A continuously-moving
                    // forged client easily clears the gate during a
                    // multi-second cast.
                    const MAX_CAST_MOVE_DISTANCE: f32 = 5.0;
                    if let Some(caster_conn_now) = connections.get(&caster_cid) {
                        let dx = caster_conn_now.pos.x - intent.cast_start_pos_at_dispatch.x;
                        let dz = caster_conn_now.pos.z - intent.cast_start_pos_at_dispatch.z;
                        let moved = (dx * dx + dz * dz).sqrt();
                        if moved > MAX_CAST_MOVE_DISTANCE {
                            tracing::info!(
                                caster = intent.caster,
                                spell = %spell.name,
                                moved,
                                "CastSpell rejected — moved during cast"
                            );
                            // Clear cast cache so the next cast isn't
                            // gated by this stale in-flight state.
                            if let Some(cc) = connections.get_mut(&caster_cid) {
                                cc.cast_spell_name.clear();
                                cc.cast_total_duration = 0.0;
                                cc.cast_set_at = None;
                            }
                            refuse_cast(&mut server, &connections, caster_cid, intent.caster, "interrupted (moved)");
                            continue;
                        }
                    }
                }
                // Track 17.2 — per-player per-spell cooldown gate.
                // Independent of cast-time (an instant cast still
                // honours the spell's cooldown). Cleanup of expired
                // entries happens lazily on the next lookup.
                if let Some(caster_conn_now) = connections.get(&caster_cid) {
                    if let Some(ready_at) = caster_conn_now.spell_cooldowns.get(&spell.name) {
                        if now < *ready_at {
                            let remaining = ready_at.duration_since(now).as_secs_f32();
                            tracing::info!(
                                caster = intent.caster,
                                spell = %spell.name,
                                remaining_secs = remaining,
                                "CastSpell rejected — on cooldown"
                            );
                            refuse_cast(&mut server, &connections, caster_cid, intent.caster, "Spell is on cooldown.");
                            continue;
                        }
                    }
                }
                // Resolve caster's snapshot (immutable) — we need pos
                // for range / AOE. mp deduction lands later under a
                // mutable borrow.
                let Some(caster_conn) = connections.get(&caster_cid) else {
                    continue;
                };
                if caster_conn.mp < spell.mana_cost {
                    tracing::info!(
                        caster = intent.caster,
                        spell = %spell.name,
                        mp = caster_conn.mp,
                        cost = spell.mana_cost,
                        "spell cast rejected — insufficient mana"
                    );
                    // Tell the caster so they see "Cast failed: Not enough
                    // mana." instead of a silent no-op behind the optimistic
                    // local "You cast X" line, with the mana they really have.
                    refuse_cast(&mut server, &connections, caster_cid, intent.caster, "Not enough mana.");
                    continue;
                }
                let caster_pos = caster_conn.pos;
                let caster_max_mp = caster_conn.max_mp;
                let (caster_mp_now, caster_hp_now, caster_max_hp_now) =
                    (caster_conn.mp, caster_conn.hp, caster_conn.max_hp);
                let mana_cost = spell.mana_cost;
                let hp_cost = spell.hp_cost;
                let dmg_type = spells::parse_damage_type(&spell.damage_type);

                // Cast pre-flight: presence, reach, the PvP gates and "the
                // server has no effect for this spell", decided before the mana
                // comes off, the cooldown is stamped or the casting skill rolls
                // (see `cast_target_refusal`). A refused cast costs nothing, so
                // nothing has to be handed back. The CLIENT, though, spent the
                // mana and started its own cooldown when it sent the cast: the
                // private CastFail is its signal to undo that, and the
                // ManaUpdate carries the server's untouched value.
                if let Some(line) = cast_target_refusal(
                    spell,
                    intent.target_id,
                    intent.caster,
                    caster_cid,
                    caster_pos,
                    &connections,
                    &enemies,
                    &corpses,
                    &group_manager,
                ) {
                    tracing::info!(
                        caster = intent.caster,
                        spell = %spell.name,
                        target = ?intent.target_id,
                        reason = %line,
                        "CastSpell refused — target pre-flight"
                    );
                    // Clear the cast cache so this refused cast's in-flight
                    // state cannot gate the next one.
                    if let Some(cc) = connections.get_mut(&caster_cid) {
                        cc.cast_spell_name.clear();
                        cc.cast_total_duration = 0.0;
                        cc.cast_set_at = None;
                    }
                    handlers::fan_out_cast_fail(&mut server, &[caster_cid], intent.caster, line);
                    handlers::fan_out_mana_update(
                        &mut server,
                        &[caster_cid],
                        intent.caster,
                        caster_mp_now,
                        caster_max_mp,
                    );
                    if hp_cost > 0.0 {
                        handlers::fan_out_health_update(
                            &mut server,
                            &[caster_cid],
                            intent.caster,
                            caster_hp_now,
                            caster_max_hp_now,
                        );
                    }
                    continue;
                }

                // Deduct mana (+ optional hp_cost for blood / fallen
                // spells). Both are caster-side; target-side effects
                // come next. Track 10 — also clear the cast cache so a
                // CastSpell sent without a follow-up CastComplete (or
                // before it arrives) doesn't leave stale state that
                // gates the next timed cast.
                // Track 17.2 — stamp the per-spell cooldown so the
                // next cast of the same spell is rejected until it
                // expires. Skipped when cooldown == 0 (most damage
                // spells; cooldown matters mainly for utility / heals).
                let (new_mp, new_hp_after_cost, max_hp) = {
                    let cc = connections.get_mut(&caster_cid).expect("checked");
                    cc.mp = (cc.mp - mana_cost).max(0.0);
                    if hp_cost > 0.0 {
                        cc.hp = (cc.hp - hp_cost).max(0.0);
                    }
                    // Casting stands you, as a swing does (spell batch step 0):
                    // no seated regen through an instant cast. A timed cast
                    // already stood at its CastStart.
                    cc.is_sitting = false;
                    cc.cast_spell_name.clear();
                    cc.cast_total_duration = 0.0;
                    cc.cast_set_at = None;
                    if spell.cooldown > 0.0 {
                        cc.spell_cooldowns.insert(
                            spell.name.clone(),
                            now + Duration::from_secs_f32(spell.cooldown),
                        );
                    }
                    regen::mark_dirty(cc);
                    (cc.mp, cc.hp, cc.max_hp)
                };
                handlers::fan_out_mana_update(
                    &mut server,
                    &in_world_recipients_now,
                    intent.caster,
                    new_mp,
                    caster_max_mp,
                );
                if hp_cost > 0.0 {
                    handlers::fan_out_health_update(
                        &mut server,
                        &in_world_recipients_now,
                        intent.caster,
                        new_hp_after_cost,
                        max_hp,
                    );
                }

                // Track 18.1 — casting skill advance. Discipline
                // comes from the GDScript-mirrored DISCIPLINE map
                // (base spell name lookup, Rank suffixes stripped).
                // Channeling advances separately when an interrupt
                // is survived; that path is server-side-pending and
                // lands when the cast-interrupt formula is ported.
                let discipline = skills::discipline_for_spell(&spell.name).to_string();
                if let Some(cc) = connections.get_mut(&caster_cid) {
                    if let Some(new_score) =
                        skills::try_advance(cc, skills::Skill::Casting, &discipline)
                    {
                        handlers::send_skill_progress_update(
                            &mut server,
                            caster_cid,
                            skills::Skill::Casting.as_protocol(),
                            discipline.clone(),
                            new_score,
                        );
                    }
                }

                match spell.target_type.as_str() {
                    "CORPSE" => {
                        // Corpse / resurrection Slice 3 — a res spell targets a
                        // corpse; offer the res to its owner, who summons + gets an
                        // xp refund on accept. Mana was already deducted above
                        // (consistent with the rest of the cast pipeline).
                        let Some(corpse_id) = intent.target_id else {
                            tracing::info!(caster = intent.caster, spell = %spell.name, "resurrection rejected — no corpse targeted (target_id 0)");
                            refuse_cast(&mut server, &connections, caster_cid, intent.caster, "No corpse targeted.");
                            continue;
                        };
                        tracing::info!(caster = intent.caster, spell = %spell.name, corpse_id, "resurrection cast received");
                        // Defense-in-depth: only the spell's own classes (Cleric /
                        // Paladin) at the required level may cast it. The client
                        // gates this, but don't trust a forged client with a free
                        // xp grant.
                        let caster_class_level = connections.get(&caster_cid).map(|c| (c.class.clone(), c.level));
                        let caster_ok = caster_class_level.as_ref().map(|(class, level)| {
                            spell.classes.iter().any(|cl| cl == class) && *level >= spell.min_level
                        }).unwrap_or(false);
                        if !caster_ok {
                            if let Some((class, level)) = &caster_class_level {
                                tracing::info!(caster = intent.caster, spell = %spell.name, class = %class, level, required = spell.min_level, "resurrection rejected — caster class/level");
                            }
                            refuse_cast(&mut server, &connections, caster_cid, intent.caster, "You cannot cast that.");
                            continue;
                        }
                        // Validate the corpse: exists, in range, not already rezzed.
                        let mut reason: Option<&str> = None;
                        let corpse_owner = match corpses.get(&corpse_id) {
                            None => { reason = Some("That is not a corpse."); 0 }
                            Some(c) if !(c.pos.distance_to(caster_pos) <= RES_CAST_RANGE) => { reason = Some("You are too far from the corpse."); 0 }
                            Some(c) if c.resurrected => { reason = Some("That corpse has already been resurrected."); 0 }
                            Some(c) => c.owner_char,
                        };
                        if let Some(r) = reason {
                            tracing::info!(caster = intent.caster, corpse_id, reason = r, "resurrection rejected — corpse");
                            refuse_cast(&mut server, &connections, caster_cid, intent.caster, r);
                            continue;
                        }
                        // The owner must be in-world to receive + accept the offer.
                        let owner_cid = corpse_owner as ClientId;
                        if !connections.get(&owner_cid).map(|c| c.in_world).unwrap_or(false) {
                            tracing::info!(caster = intent.caster, corpse_id, owner = corpse_owner, "resurrection rejected — owner not in world");
                            refuse_cast(&mut server, &connections, caster_cid, intent.caster, "Their spirit is not present.");
                            continue;
                        }
                        let xp_percent = spell.res_xp_percent.round() as u32;
                        let caster_name = connections.get(&caster_cid).map(|c| c.name.clone()).unwrap_or_default();
                        // Record the pending offer on the owner so the accept can't
                        // forge the refund %, then send the offer privately.
                        if let Some(owner_conn) = connections.get_mut(&owner_cid) {
                            owner_conn.pending_res_offer = Some((corpse_id, xp_percent));
                        }
                        handlers::send_resurrect_offer(&mut server, owner_cid, corpse_id, caster_name, xp_percent);
                        tracing::info!(caster = intent.caster, corpse_id, xp_percent, "resurrection offered");
                    }
                    "SELF" => {
                        tracing::info!(
                            caster = intent.caster,
                            spell = %spell.name,
                            absorb = spell.absorb_amount,
                            "SELF cast received"
                        );
                        // Heal the caster (or damage in the rare "self
                        // damage" case). base_damage is treated as a
                        // self-damage; heal_amount as a heal.
                        let heal = spell.heal_amount;
                        let dmg = spell.base_damage;
                        if heal > 0.0 || dmg > 0.0 {
                            let (final_hp, max_hp) = {
                                let cc = connections.get_mut(&caster_cid).expect("checked");
                                cc.hp = (cc.hp + heal - dmg).clamp(0.0, cc.max_hp);
                                regen::mark_dirty(cc);
                                (cc.hp, cc.max_hp)
                            };
                            handlers::fan_out_health_update(
                                &mut server,
                                &in_world_recipients_now,
                                intent.caster,
                                final_hp,
                                max_hp,
                            );
                            tracing::info!(
                                caster = intent.caster,
                                spell = %spell.name,
                                heal,
                                dmg,
                                final_hp,
                                "spell self effect applied"
                            );
                        }
                        // Track 6 — apply caster buffs. Lich Form is a
                        // SELF-only toggle handled inline; the rest of the
                        // beneficial buffs (HoT / MP-regen / speed / haste /
                        // shield / absorb / accuracy / primary stat) flow
                        // through the shared apply_player_spell_buffs so the
                        // SELF and ALLY arms stay in lockstep.
                        let mut buff_changed = false;
                        if spell.is_lich_form {
                            // Toggle semantics — second cast clears.
                            let cc = connections.get_mut(&caster_cid).expect("checked");
                            let was_active = buffs::is_lich_form_active(&cc.active_buffs);
                            cc.active_buffs.retain(|b| !matches!(
                                b.effect,
                                buffs::BuffEffect::LichForm { .. }
                            ));
                            if !was_active {
                                cc.active_buffs.push(ActiveBuff::new_lich_form(
                                    spell.name.clone(),
                                    spell.lich_mp_regen,
                                    now,
                                ));
                            }
                            buff_changed = true;
                        }
                        if apply_player_spell_buffs(
                            connections.get_mut(&caster_cid).expect("checked"),
                            spell,
                            now,
                        ) {
                            buff_changed = true;
                        }
                        if buff_changed {
                            if let Some(cc) = connections.get(&caster_cid) {
                                fan_out_server_buff_snapshot(
                                    &mut server,
                                    &in_world_recipients_now,
                                    cc,
                                );
                            }
                        }
                    }
                    "ALLY" => {
                        use protocol::world::{ENEMY_ID_BASE, LOOT_BAG_ID_BASE, PET_ID_BASE};
                        let raw_target = intent.target_id.unwrap_or(0);
                        // Pet ALLY heal: any pet whose owner is in-world is
                        // a valid heal target. No group requirement —
                        // Beast Master can support a stranger's warder.
                        if raw_target >= PET_ID_BASE {
                            let pet_alive = enemies
                                .get(&raw_target)
                                .map_or(false, |p| p.is_alive() && p.owner.is_some());
                            if !pet_alive {
                                handlers::fan_out_cast_fail(
                                    &mut server,
                                    &[caster_cid],
                                    intent.caster,
                                    "That pet is no longer in the world.".to_string(),
                                );
                                continue;
                            }
                            // PvP heal gate. If the caster and the pet's
                            // owner are mutually attackable (can_attack
                            // returns true), the pet is treated as a
                            // hostile target for heal purposes — same
                            // gate as direct attacks, just mirrored.
                            // Stops a healer from topping up the duel
                            // partner's warder mid-fight.
                            let pet_owner = enemies
                                .get(&raw_target)
                                .and_then(|p| p.owner);
                            if let Some(owner_id) = pet_owner {
                                if owner_id != intent.caster {
                                    let owner_cid = owner_id as ClientId;
                                    let hostile = match (
                                        connections.get(&caster_cid),
                                        connections.get(&owner_cid),
                                    ) {
                                        (Some(a), Some(o)) => combat::can_attack(
                                            a, o,
                                            a.zone.as_deref(),
                                            o.zone.as_deref(),
                                        ),
                                        _ => false,
                                    };
                                    // Group-mates can always support each other —
                                    // a shared /pvp flag never blocks healing a
                                    // group-mate's pet.
                                    let allied = group_manager.same_group(caster_cid, owner_cid);
                                    if hostile && !allied {
                                        let owner_name = connections
                                            .get(&owner_cid)
                                            .map(|c| c.name.clone())
                                            .unwrap_or_default();
                                        let pet_name = enemies
                                            .get(&raw_target)
                                            .map(|p| p.mob.name.clone())
                                            .unwrap_or_default();
                                        let line = if !owner_name.is_empty() && !pet_name.is_empty() {
                                            format!("You cannot heal {}'s {}.", owner_name, pet_name)
                                        } else {
                                            "You cannot heal an enemy's pet.".to_string()
                                        };
                                        handlers::fan_out_chat_message(
                                            &mut server,
                                            &[caster_cid],
                                            "",
                                            protocol::world::ChatChannel::System,
                                            &line,
                                        );
                                        tracing::debug!(
                                            caster = intent.caster,
                                            target = raw_target,
                                            owner = owner_id,
                                            "ALLY pet heal rejected — PvP gate"
                                        );
                                        continue;
                                    }
                                }
                            }
                            let heal = spell.heal_amount;
                            if heal > 0.0 {
                                if let Some(pet) = enemies.get_mut(&raw_target) {
                                    pet.hp = (pet.hp + heal).min(pet.max_hp);
                                    let new_hp = pet.hp;
                                    let max_hp = pet.max_hp;
                                    handlers::fan_out_health_update(
                                        &mut server,
                                        &in_world_recipients_now,
                                        raw_target,
                                        new_hp,
                                        max_hp,
                                    );
                                    tracing::info!(
                                        caster = intent.caster,
                                        target = raw_target,
                                        spell = %spell.name,
                                        heal,
                                        "ALLY pet heal applied"
                                    );
                                }
                            }
                            // Track 13 — apply the pet-relevant buff set
                            // (HoT / speed / haste / damage shield / stat)
                            // to the pet and fan a BuffSnapshot under the
                            // pet id.
                            let mut pet_buff_changed = false;
                            if let Some(pet) = enemies.get_mut(&raw_target) {
                                pet_buff_changed = apply_pet_spell_buffs(pet, spell, now);
                            }
                            if pet_buff_changed {
                                if let Some(pet) = enemies.get(&raw_target) {
                                    // Valor-style buffs grow max_hp — fan a
                                    // HealthUpdate so the bar shows the new cap.
                                    if spell.max_hp_buff != 0.0 {
                                        handlers::fan_out_health_update(
                                            &mut server,
                                            &in_world_recipients_now,
                                            raw_target,
                                            pet.hp,
                                            pet.max_hp,
                                        );
                                    }
                                    fan_out_pet_buff_snapshot(
                                        &mut server,
                                        &in_world_recipients_now,
                                        pet,
                                    );
                                    tracing::info!(
                                        caster = intent.caster,
                                        target = raw_target,
                                        spell = %spell.name,
                                        "ALLY pet buff applied"
                                    );
                                }
                            }
                            continue;
                        }
                        // Loot bags / out-of-range / unknown high ids are
                        // not heal targets.
                        if raw_target >= LOOT_BAG_ID_BASE {
                            handlers::fan_out_cast_fail(
                                &mut server,
                                &[caster_cid],
                                intent.caster,
                                "Cannot heal that target.".to_string(),
                            );
                            continue;
                        }
                        // NPC enemy → explicit reject so the player sees a
                        // chat line instead of a silent no-op.
                        if raw_target >= ENEMY_ID_BASE {
                            handlers::fan_out_cast_fail(
                                &mut server,
                                &[caster_cid],
                                intent.caster,
                                "Cannot heal enemies.".to_string(),
                            );
                            continue;
                        }
                        // raw_target == 0 → self-heal. raw_target > 0 and
                        // below ENEMY_ID_BASE is a player char_id.
                        let target_entity_id = if raw_target > 0 {
                            raw_target
                        } else {
                            intent.caster
                        };
                        let target_cid = target_entity_id as ClientId;
                        let target_ok = connections
                            .get(&target_cid)
                            .map_or(false, |c| c.in_world && c.hp > 0.0);
                        if !target_ok {
                            // Healing someone who just died or zoned out. Mana
                            // came off at the top of the handler, so silence
                            // here reads as the heal being broken — and in the
                            // exact moment a healer is panicking. Refund and say so.
                            refund_spell_cost(
                                &mut server,
                                &mut connections,
                                &in_world_recipients_now,
                                caster_cid,
                                intent.caster,
                                mana_cost,
                                hp_cost,
                            );
                            handlers::send_refusal(
                                &mut server, caster_cid, "That target is no longer here.",
                            );
                            continue;
                        }
                        // PvP heal gate (player target). If caster and
                        // target are mutually attackable, treat the
                        // target as hostile for heal purposes. Self-
                        // heal (caster == target) bypasses naturally
                        // since combat::can_attack rejects same-id.
                        if target_cid != caster_cid {
                            let hostile = match (
                                connections.get(&caster_cid),
                                connections.get(&target_cid),
                            ) {
                                (Some(a), Some(t)) => combat::can_attack(
                                    a, t,
                                    a.zone.as_deref(),
                                    t.zone.as_deref(),
                                ),
                                _ => false,
                            };
                            // Group-mates can always heal/buff each other; a
                            // shared /pvp flag doesn't make a group-mate a
                            // valid beneficial-cast block.
                            let allied = group_manager.same_group(caster_cid, target_cid);
                            if hostile && !allied {
                                let t_name = connections
                                    .get(&target_cid)
                                    .map(|c| c.name.clone())
                                    .unwrap_or_default();
                                let line = if !t_name.is_empty() {
                                    format!("You cannot heal {}.", t_name)
                                } else {
                                    "You cannot heal an enemy.".to_string()
                                };
                                handlers::fan_out_chat_message(
                                    &mut server,
                                    &[caster_cid],
                                    "",
                                    protocol::world::ChatChannel::System,
                                    &line,
                                );
                                tracing::debug!(
                                    caster = intent.caster,
                                    target = target_entity_id,
                                    "ALLY heal rejected — PvP gate"
                                );
                                continue;
                            }
                        }
                        let heal = spell.heal_amount;
                        if heal > 0.0 {
                            let (final_hp, max_hp) = {
                                let tc =
                                    connections.get_mut(&target_cid).expect("checked");
                                tc.hp = (tc.hp + heal).min(tc.max_hp);
                                regen::mark_dirty(tc);
                                (tc.hp, tc.max_hp)
                            };
                            handlers::fan_out_health_update(
                                &mut server,
                                &in_world_recipients_now,
                                target_entity_id,
                                final_hp,
                                max_hp,
                            );
                            tracing::info!(
                                caster = intent.caster,
                                target = target_entity_id,
                                spell = %spell.name,
                                heal,
                                "ALLY heal applied"
                            );
                        }
                        // Apply HoT + every beneficial buff to the ally,
                        // same set the SELF arm grants the caster. Stat
                        // buffs mutate the target conn's effective stats
                        // (combat math reads them); the fanned BuffSnapshot
                        // lets the target's client reconstruct the buff
                        // locally (BuffManager.reconcile_with_server_snapshot).
                        let ally_buff_changed = apply_player_spell_buffs(
                            connections.get_mut(&target_cid).expect("checked"),
                            spell,
                            now,
                        );
                        if ally_buff_changed {
                            if let Some(tc) = connections.get(&target_cid) {
                                fan_out_server_buff_snapshot(
                                    &mut server,
                                    &in_world_recipients_now,
                                    tc,
                                );
                            }
                            tracing::info!(
                                caster = intent.caster,
                                target = target_entity_id,
                                spell = %spell.name,
                                "ALLY buff applied"
                            );
                        }
                    }
                    "ENEMY" => {
                        let Some(target_id) = intent.target_id else {
                            // Nuking with nothing targeted. The user reported
                            // this UX in 2026-05-05 ("player can cast attack
                            // spells when nothing is targeted... should receive
                            // an error"); the server half is that it also ate
                            // the mana.
                            refund_spell_cost(
                                &mut server,
                                &mut connections,
                                &in_world_recipients_now,
                                caster_cid,
                                intent.caster,
                                mana_cost,
                                hp_cost,
                            );
                            handlers::send_refusal(
                                &mut server, caster_cid, "You need a target for that spell.",
                            );
                            continue;
                        };
                        // Player target → PvP path (gate via can_attack
                        // for parity with melee swings).
                        if target_id < protocol::world::ENEMY_ID_BASE {
                            let target_cid = target_id as ClientId;
                            if target_cid == caster_cid {
                                refund_spell_cost(
                                    &mut server,
                                    &mut connections,
                                    &in_world_recipients_now,
                                    caster_cid,
                                    intent.caster,
                                    mana_cost,
                                    hp_cost,
                                );
                                handlers::send_refusal(
                                    &mut server,
                                    caster_cid,
                                    "You cannot cast that on yourself.",
                                );
                                continue;
                            }
                            let pvp_ok = match (
                                connections.get(&caster_cid),
                                connections.get(&target_cid),
                            ) {
                                (Some(a), Some(t)) => combat::can_attack(
                                    a, t,
                                    a.zone.as_deref(),
                                    t.zone.as_deref(),
                                ),
                                _ => false,
                            };
                            if !pvp_ok {
                                let name = connections
                                    .get(&target_cid)
                                    .map(|c| c.name.clone())
                                    .unwrap_or_else(|| "that target".to_string());
                                handlers::fan_out_chat_message(
                                    &mut server,
                                    &[caster_cid],
                                    "",
                                    protocol::world::ChatChannel::System,
                                    &format!("Unable to attack {}.", name),
                                );
                                tracing::debug!(
                                    caster = intent.caster,
                                    target = target_id,
                                    spell = %spell.name,
                                    "PvP spell not authorized"
                                );
                                continue;
                            }
                            // Track 6 sub-task 4c — absorb pool +
                            // damage shield apply on PvP spell hit
                            // too. Armor reduction is skipped for
                            // spells (matches GDScript wrapping).
                            // Liveness checked BEFORE the mutable borrow so the
                            // refusal path can refund (refund_spell_cost needs
                            // `&mut connections` of its own).
                            let pvp_target_alive = connections
                                .get(&target_cid)
                                .map_or(false, |t| t.hp > 0.0 && t.in_world);
                            if !pvp_target_alive {
                                refund_spell_cost(
                                    &mut server,
                                    &mut connections,
                                    &in_world_recipients_now,
                                    caster_cid,
                                    intent.caster,
                                    mana_cost,
                                    hp_cost,
                                );
                                handlers::send_refusal(
                                    &mut server, caster_cid, "That target is no longer here.",
                                );
                                continue;
                            }
                            let shield_back: f32;
                            let shield_back_name: Option<String>;
                            let mut absorb_strip_idx: Option<usize> = None;
                            let (final_hp, max_hp, applied) = {
                                let tc = connections.get_mut(&target_cid).expect("checked");
                                let mut dmg = spell.base_damage.max(0.0) as i32;
                                let (after_absorb, exhausted) =
                                    buffs::consume_absorb(&mut tc.active_buffs, dmg);
                                dmg = after_absorb;
                                if exhausted {
                                    absorb_strip_idx = tc.active_buffs.iter().position(|b| {
                                        matches!(b.effect, buffs::BuffEffect::Absorb { .. })
                                    });
                                }
                                shield_back = buffs::damage_shield_total(&tc.active_buffs);
                                shield_back_name = buffs::first_damage_shield_name(&tc.active_buffs).map(|s| s.to_string());
                                tc.hp = (tc.hp - dmg as f32).max(0.0);
                                regen::mark_dirty(tc);
                                tc.note_damage_taken(now); // camp breaks on damage
                                (tc.hp, tc.max_hp, dmg)
                            };
                            if let Some(idx) = absorb_strip_idx {
                                if let Some(tc) = connections.get_mut(&target_cid) {
                                    if idx < tc.active_buffs.len() {
                                        tc.active_buffs.remove(idx);
                                    }
                                }
                                if let Some(tc) = connections.get(&target_cid) {
                                    fan_out_server_buff_snapshot(
                                        &mut server,
                                        &in_world_recipients_now,
                                        tc,
                                    );
                                }
                            }
                            handlers::fan_out_hit(
                                &mut server,
                                &in_world_recipients_now,
                                intent.caster,
                                target_id,
                                applied,
                                false,
                                dmg_type,
                            );
                            handlers::fan_out_health_update(
                                &mut server,
                                &in_world_recipients_now,
                                target_id,
                                final_hp,
                                max_hp,
                            );
                            tracing::info!(
                                caster = intent.caster,
                                target = target_id,
                                spell = %spell.name,
                                applied,
                                "PvP spell applied"
                            );
                            // Track 22.E — PvP spell damage interrupts
                            // the target's cast (mirror of the PvP
                            // melee interrupt wired in Track 19A).
                            if let Some(tc) = connections.get_mut(&target_cid) {
                                let outcome = roll_cast_interrupt(tc, now);
                                match &outcome {
                                    InterruptOutcome::Interrupted { spell_name } => {
                                        handlers::fan_out_cast_fail(
                                            &mut server,
                                            &in_world_recipients_now,
                                            target_id,
                                            "interrupted (hit during cast)".to_string(),
                                        );
                                        tracing::info!(
                                            caster = target_id,
                                            spell = %spell_name,
                                            "PvP cast interrupted by incoming spell damage"
                                        );
                                    }
                                    InterruptOutcome::Survived {
                                        advanced_to: Some(new_score),
                                    } => {
                                        handlers::send_skill_progress_update(
                                            &mut server,
                                            target_cid,
                                            skills::Skill::Casting.as_protocol(),
                                            "channeling".to_string(),
                                            *new_score,
                                        );
                                    }
                                    InterruptOutcome::Survived { advanced_to: None }
                                    | InterruptOutcome::NotCasting => {}
                                }
                            }
                            // Damage shield reflects to caster.
                            if shield_back > 0.0 {
                                if let Some(att) = connections.get_mut(&caster_cid) {
                                    if att.hp > 0.0 {
                                        let reflect_dmg = shield_back as i32;
                                        att.hp = (att.hp - shield_back).max(0.0);
                                        regen::mark_dirty(att);
                                        att.note_damage_taken(now); // camp breaks on damage
                                        let new_att_hp = att.hp;
                                        let att_max = att.max_hp;
                                        handlers::fan_out_health_update(
                                            &mut server,
                                            &in_world_recipients_now,
                                            intent.caster,
                                            new_att_hp,
                                            att_max,
                                        );
                                        if let Some(name) = shield_back_name.as_ref() {
                                            handlers::fan_out_damage_shield_trigger(
                                                &mut server,
                                                &in_world_recipients_now,
                                                target_id,
                                                intent.caster,
                                                reflect_dmg,
                                                name.clone(),
                                            );
                                        }
                                        // Track 22.E — shield reflect
                                        // damage also interrupts the
                                        // caster (their own thorns
                                        // hits their cast). Note this
                                        // is rarely meaningful since
                                        // CastSpell already cleared
                                        // cast_spell_name above when
                                        // the cast resolved — left
                                        // here for the case where a
                                        // future spell type leaves
                                        // the cache populated.
                                        let outcome = roll_cast_interrupt(att, now);
                                        if let InterruptOutcome::Interrupted { spell_name } = &outcome {
                                            handlers::fan_out_cast_fail(
                                                &mut server,
                                                &in_world_recipients_now,
                                                intent.caster,
                                                "interrupted (hit during cast)".to_string(),
                                            );
                                            tracing::info!(
                                                caster = intent.caster,
                                                spell = %spell_name,
                                                "PvP caster interrupted by own shield reflect"
                                            );
                                        }
                                    }
                                }
                            }
                            // Track 6 sub-task 4d — CC application on
                            // PvP spell. Mez, Root, Snare, AttackSlow,
                            // Silence, Dispel land on the target's
                            // active_buffs after damage. Refresh
                            // semantics (same-name re-cast renews
                            // duration via apply_buff).
                            let mut cc_changed = false;
                            if spell.cc_duration > 0.0 {
                                if let Some(tc) = connections.get_mut(&target_cid) {
                                    apply_buff(tc, ActiveBuff::new_mez(spell.name.clone(), spell.cc_duration, now));
                                    cc_changed = true;
                                }
                            }
                            if spell.root_duration > 0.0 {
                                if let Some(tc) = connections.get_mut(&target_cid) {
                                    apply_buff(tc, ActiveBuff::new_root(spell.name.clone(), spell.root_duration, now));
                                    cc_changed = true;
                                }
                            }
                            if spell.slow_amount > 0.0 && spell.slow_duration > 0.0 {
                                if let Some(tc) = connections.get_mut(&target_cid) {
                                    apply_buff(tc, ActiveBuff::new_snare(spell.name.clone(), spell.slow_amount, spell.slow_duration, now));
                                    cc_changed = true;
                                }
                            }
                            if spell.attack_slow_amount > 0.0 && spell.attack_slow_duration > 0.0 {
                                if let Some(tc) = connections.get_mut(&target_cid) {
                                    apply_buff(tc, ActiveBuff::new_attack_slow(spell.name.clone(), spell.attack_slow_amount, spell.attack_slow_duration, now));
                                    cc_changed = true;
                                }
                            }
                            if spell.silence_duration > 0.0 {
                                if let Some(tc) = connections.get_mut(&target_cid) {
                                    apply_buff(tc, ActiveBuff::new_silence(spell.name.clone(), spell.silence_duration, now));
                                    cc_changed = true;
                                }
                            }
                            if spell.is_dispel {
                                // Strip one non-CC buff from target.
                                // Stat buffs need their deltas undone
                                // first.
                                if let Some(tc) = connections.get_mut(&target_cid) {
                                    if let Some(idx) = buffs::first_dispellable_index(&tc.active_buffs) {
                                        if let buffs::BuffEffect::StatBuff {
                                            strength, agility, intelligence, wisdom, constitution,
                                            max_hp_delta, max_mp_delta,
                                        } = tc.active_buffs[idx].effect
                                        {
                                            buffs::undo_stat_deltas(
                                                tc,
                                                strength, agility, intelligence, wisdom, constitution,
                                                max_hp_delta, max_mp_delta,
                                            );
                                            regen::mark_dirty(tc);
                                        }
                                        tc.active_buffs.remove(idx);
                                        cc_changed = true;
                                    }
                                }
                            }
                            if cc_changed {
                                if let Some(tc) = connections.get(&target_cid) {
                                    fan_out_server_buff_snapshot(
                                        &mut server,
                                        &in_world_recipients_now,
                                        tc,
                                    );
                                }
                            }
                            continue;
                        }
                        // Enemy target — range-check, then apply spell
                        // damage via the shared helper (Track 9; same
                        // path AOE uses per victim).
                        let in_range = match enemies.get(&target_id) {
                            Some(e) if e.is_alive() => {
                                let dist = e.pos.distance_to(caster_pos);
                                if dist > RANGED_ATTACK_RANGE {
                                    // info!, not debug!: a player-visible failure
                                    // should not be invisible at the default log
                                    // level. A 2026-08-24 playtest saw the chat
                                    // line with no matching server.log entry.
                                    tracing::info!(
                                        caster = intent.caster,
                                        target = target_id,
                                        spell = %spell.name,
                                        dist,
                                        "spell out of range"
                                    );
                                    false
                                } else {
                                    true
                                }
                            }
                            _ => false,
                        };
                        if !in_range {
                            // Mana came off at the top of this handler, so an
                            // out-of-range nuke charged full price. Melee already
                            // tells you (an out-of-range Attack fans a Miss), so
                            // players expect a reply and read silence as the spell
                            // being broken. Reporting it was only half the fix:
                            // give the cost back too.
                            refund_spell_cost(
                                &mut server,
                                &mut connections,
                                &in_world_recipients_now,
                                caster_cid,
                                intent.caster,
                                mana_cost,
                                hp_cost,
                            );
                            handlers::send_refusal(
                                &mut server, caster_cid, "That target is too far away.",
                            );
                            continue;
                        }
                        // Pet PvP gate, mirroring the melee/ranged path.
                        // Spell hits on another player's pet require
                        // can_attack between the caster and the pet's
                        // owner.
                        if target_id >= protocol::world::PET_ID_BASE {
                            let pet_owner = enemies.get(&target_id).and_then(|e| e.owner);
                            if let Some(owner_id) = pet_owner {
                                let owner_cid = owner_id as ClientId;
                                let allowed = if owner_cid == caster_cid {
                                    false  // own pet — defensive guard
                                } else {
                                    match (
                                        connections.get(&caster_cid),
                                        connections.get(&owner_cid),
                                    ) {
                                        (Some(a), Some(o)) => combat::can_attack(
                                            a, o,
                                            a.zone.as_deref(),
                                            o.zone.as_deref(),
                                        ),
                                        _ => false,
                                    }
                                };
                                if !allowed {
                                    let owner_name = connections
                                        .get(&(owner_id as ClientId))
                                        .map(|c| c.name.clone())
                                        .unwrap_or_default();
                                    let pet_name = enemies
                                        .get(&target_id)
                                        .map(|p| p.mob.name.clone())
                                        .unwrap_or_default();
                                    let line = if !owner_name.is_empty() && !pet_name.is_empty() {
                                        format!("Unable to attack {}'s {}.", owner_name, pet_name)
                                    } else if !pet_name.is_empty() {
                                        format!("Unable to attack the {}.", pet_name)
                                    } else {
                                        "Unable to attack that target.".to_string()
                                    };
                                    handlers::fan_out_chat_message(
                                        &mut server,
                                        &[caster_cid],
                                        "",
                                        protocol::world::ChatChannel::System,
                                        &line,
                                    );
                                    tracing::debug!(
                                        caster = intent.caster,
                                        target = target_id,
                                        owner = owner_id,
                                        spell = %spell.name,
                                        "PvP spell not authorized vs pet"
                                    );
                                    continue;
                                }
                            }
                        }
                        apply_spell_damage_to_enemy(
                            &mut server,
                            &in_world_recipients_now,
                            &mut connections,
                            &mut enemies,
                            &mut loot_bags,
                            &mut aoi,
                            &group_manager,
                            intent.caster,
                            target_id,
                            spell,
                            dmg_type,
                            now,
                        );
                        // Visibility log for PvP spell-on-pet. The
                        // pet-target path doesn't reach the
                        // "PvP spell applied" trace at line ~3002 (which
                        // is in the player-target arm), so without this
                        // line a spell-on-pet hit is invisible in
                        // server.log and hard to triage.
                        if target_id >= protocol::world::PET_ID_BASE {
                            tracing::info!(
                                caster = intent.caster,
                                target = target_id,
                                spell = %spell.name,
                                "PvP spell applied vs pet"
                            );
                        }
                        // Track 11.3 — refresh pet target inheritance
                        // on every single-target damage spell too, so
                        // a Necromancer casting Bone Shards at an
                        // enemy directs the skeleton onto it.
                        if let Some(att) = connections.get_mut(&caster_cid) {
                            att.last_attacked_enemy = Some(target_id);
                            att.last_attacked_at = Some(now);
                        }
                    }
                    "AOE" => {
                        // Track 9 — server-authoritative AOE damage.
                        // Search the caster's AOI neighbourhood (3×3
                        // cells around the caster, 360 m on a side at
                        // CELL_SIZE=120), then filter by spell radius.
                        // AOE spell radii top out around 6 m today, so
                        // this is a tiny working set in practice.
                        if spell.aoe_radius <= 0.0 {
                            tracing::info!(
                                caster = intent.caster,
                                spell = %spell.name,
                                "AOE spell with no radius; nothing to apply"
                            );
                            // Only reachable from a malformed spells.toml, but
                            // the player paid for it either way.
                            refund_spell_cost(
                                &mut server,
                                &mut connections,
                                &in_world_recipients_now,
                                caster_cid,
                                intent.caster,
                                mana_cost,
                                hp_cost,
                            );
                            handlers::send_refusal(
                                &mut server, caster_cid, "That magic has no effect here yet.",
                            );
                            continue;
                        }
                        let radius = spell.aoe_radius;
                        let radius_sq = radius * radius;
                        let caster_cell = aoi::cell_for(caster_pos.x, caster_pos.z);
                        let visible = aoi.entities_visible_from(caster_cell);
                        let mut victims: Vec<EntityId> = Vec::new();
                        for id in visible {
                            // Eligible AOE victims: world enemies
                            // [ENEMY_ID_BASE, LOOT_BAG_ID_BASE) and pets
                            // [PET_ID_BASE, …). Players (< ENEMY_ID_BASE) and
                            // loot bags ([LOOT_BAG_ID_BASE, PET_ID_BASE)) are
                            // excluded. Pets are included so AOE hits them like
                            // melee/single-target does — the owner PvP gate
                            // below decides whether each pet is actually hit.
                            let is_enemy = id >= protocol::world::ENEMY_ID_BASE
                                && id < protocol::world::LOOT_BAG_ID_BASE;
                            let is_pet = id >= protocol::world::PET_ID_BASE;
                            if !is_enemy && !is_pet {
                                continue;
                            }
                            if let Some(entity) = enemies.get(&id) {
                                if !entity.is_alive() {
                                    continue;
                                }
                                // Pet PvP gate (mirrors the single-target
                                // melee/spell paths): a player-owned pet is
                                // only a valid AOE victim if the caster
                                // could attack its owner directly — and
                                // never the caster's own pet. NPC mobs
                                // (owner None) are always eligible.
                                if let Some(owner_id) = entity.owner {
                                    let owner_cid = owner_id as ClientId;
                                    let allowed = owner_cid != caster_cid
                                        && match (
                                            connections.get(&caster_cid),
                                            connections.get(&owner_cid),
                                        ) {
                                            (Some(a), Some(o)) => combat::can_attack(
                                                a, o,
                                                a.zone.as_deref(),
                                                o.zone.as_deref(),
                                            ),
                                            _ => false,
                                        };
                                    if !allowed {
                                        continue;
                                    }
                                }
                                let dx = entity.pos.x - caster_pos.x;
                                let dy = entity.pos.y - caster_pos.y;
                                let dz = entity.pos.z - caster_pos.z;
                                if dx * dx + dy * dy + dz * dz <= radius_sq {
                                    victims.push(id);
                                }
                            }
                        }
                        let mut hits = 0;
                        for victim in victims {
                            if apply_spell_damage_to_enemy(
                                &mut server,
                                &in_world_recipients_now,
                                &mut connections,
                                &mut enemies,
                                &mut loot_bags,
                                &mut aoi,
                                &group_manager,
                                intent.caster,
                                victim,
                                spell,
                                dmg_type,
                                now,
                            ) {
                                hits += 1;
                            }
                        }
                        tracing::info!(
                            caster = intent.caster,
                            spell = %spell.name,
                            radius,
                            hits,
                            "AOE spell applied"
                        );
                    }
                    "PET_SUMMON" => {
                        // Track 11 — spawn a player-owned pet at the
                        // caster's position. One pet per owner: any
                        // existing pet for this caster is despawned
                        // first (mark Dead + EntityDespawn fan-out;
                        // corpse cleanup phase removes it from the
                        // map and AOI next tick).
                        let pet_type = spell.pet_type.clone();
                        if pet_type.is_empty() {
                            tracing::info!(
                                caster = intent.caster,
                                spell = %spell.name,
                                "PET_SUMMON spell has no pet_type; ignored"
                            );
                            refund_spell_cost(
                                &mut server,
                                &mut connections,
                                &in_world_recipients_now,
                                caster_cid,
                                intent.caster,
                                mana_cost,
                                hp_cost,
                            );
                            handlers::send_refusal(
                                &mut server, caster_cid, "That magic has no effect here yet.",
                            );
                            continue;
                        }
                        // Owner-derived level (pet interim A): manual
                        // summons take the EQ re-summon gamble — a 0..=2
                        // roll off owner - 1 — so paying mana again can
                        // land a better pet. The skeleton's tier cap
                        // lives in `pet_templates::scaled`.
                        let Some(owner_level) = connections
                            .get(&(intent.caster as ClientId))
                            .map(|c| c.level.max(1) as u32)
                        else {
                            // Caster vanished between queueing the intent and
                            // resolution — skip, like the other resolver arms,
                            // rather than spawning an ownerless level-1 pet.
                            tracing::debug!(
                                caster = intent.caster,
                                spell = %spell.name,
                                "PET_SUMMON dropped — caster no longer connected"
                            );
                            continue;
                        };
                        let variance = rand::thread_rng().gen_range(0..=2u32);
                        let owner_id = intent.caster;
                        // Helper handles the scaled template, spawn offset,
                        // despawn-existing, AOI insert, fan-out, and map
                        // insert. None = unknown pet_type.
                        let Some((pet_id, pet_level)) = summon_scaled_pet(
                            &mut server,
                            &in_world_recipients_now,
                            &mut enemies,
                            &mut aoi,
                            owner_id,
                            owner_level,
                            caster_pos,
                            &pet_type,
                            variance,
                            1.0,
                            now,
                        ) else {
                            tracing::info!(
                                caster = intent.caster,
                                spell = %spell.name,
                                pet_type = %pet_type,
                                "unknown pet_type — server-side summon dropped"
                            );
                            // Reachable in ordinary play: the client-only spell
                            // backlog includes pet spells whose pet_type the
                            // server has never heard of (Warder's Mend's
                            // sibling case). Same refund-and-say-so as the
                            // unknown-spell arm above.
                            refund_spell_cost(
                                &mut server,
                                &mut connections,
                                &in_world_recipients_now,
                                caster_cid,
                                intent.caster,
                                mana_cost,
                                hp_cost,
                            );
                            handlers::send_refusal(
                                &mut server, caster_cid, "That magic has no effect here yet.",
                            );
                            continue;
                        };
                        tracing::info!(
                            owner = owner_id,
                            pet_id,
                            spell = %spell.name,
                            pet_type = %pet_type,
                            level = pet_level,
                            owner_level,
                            "pet summoned"
                        );
                    }
                    "PET_CHARM" => {
                        // Track 12 Piece C — convert the targeted
                        // enemy into a player-owned pet for
                        // `spell.duration` seconds. Re-keys the id
                        // partition: EntityDespawn the old enemy id,
                        // PetSpawn a fresh pet id at the same pos
                        // with the same hp/max_hp, owner = caster.
                        // When the charm ends (the sweep below, or the
                        // owner leaving) the mob is handed back as the
                        // enemy it was, hostile to its charmer; see
                        // `return_charmed_mob`.
                        let Some(target_id) = intent.target_id else {
                            tracing::info!(caster = intent.caster, spell = %spell.name, "PET_CHARM dropped — no target");
                            refund_spell_cost(
                                &mut server,
                                &mut connections,
                                &in_world_recipients_now,
                                caster_cid,
                                intent.caster,
                                mana_cost,
                                hp_cost,
                            );
                            handlers::send_refusal(
                                &mut server, caster_cid, "You need a target for that spell.",
                            );
                            continue;
                        };
                        if target_id < protocol::world::ENEMY_ID_BASE
                            || target_id >= protocol::world::LOOT_BAG_ID_BASE
                        {
                            tracing::info!(caster = intent.caster, target = target_id, "PET_CHARM dropped — target id not in enemy partition");
                            refund_spell_cost(
                                &mut server,
                                &mut connections,
                                &in_world_recipients_now,
                                caster_cid,
                                intent.caster,
                                mana_cost,
                                hp_cost,
                            );
                            handlers::send_refusal(
                                &mut server, caster_cid, "You cannot charm that.",
                            );
                            continue;
                        }
                        let owner_id = intent.caster;
                        // Look up the target; copy out the stats we
                        // need then remove it from the map + AOI.
                        let extracted: Option<(crate::world::zones::MobTemplate, f32, f32, Vec3f, f32, usize, Vec3f)> = enemies
                            .get(&target_id)
                            .filter(|e| e.is_alive() && !e.is_pet())
                            .map(|e| (e.mob.clone(), e.hp, e.max_hp, e.pos, e.yaw, e.spawn_point_idx, e.spawn_pos));
                        let Some((mob, hp, max_hp, pos, yaw, slot_idx, home_pos)) = extracted else {
                            tracing::info!(caster = owner_id, target = target_id, "PET_CHARM dropped — target gone or not a live enemy");
                            refund_spell_cost(
                                &mut server,
                                &mut connections,
                                &in_world_recipients_now,
                                caster_cid,
                                intent.caster,
                                mana_cost,
                                hp_cost,
                            );
                            handlers::send_refusal(
                                &mut server, caster_cid, "That target is no longer here.",
                            );
                            continue;
                        };
                        // Range gate, the same reach as a nuke. Charm had none,
                        // so any live enemy id in the world could be charmed
                        // from anywhere (a named mob from the safety of town).
                        // Kept-only-when-in-range, so a non-finite distance
                        // refuses.
                        if !(pos.distance_to(caster_pos) <= RANGED_ATTACK_RANGE) {
                            tracing::info!(caster = owner_id, target = target_id, "PET_CHARM dropped — out of range");
                            refund_spell_cost(
                                &mut server,
                                &mut connections,
                                &in_world_recipients_now,
                                caster_cid,
                                intent.caster,
                                mana_cost,
                                hp_cost,
                            );
                            handlers::send_refusal(
                                &mut server, caster_cid, "That target is too far away.",
                            );
                            continue;
                        }
                        // Despawn the old enemy id from AOI + fan.
                        let enemy_cell = aoi::cell_for(pos.x, pos.z);
                        aoi.remove(target_id, enemy_cell);
                        let despawn_visible = aoi.entities_visible_from(enemy_cell);
                        for &recipient in &in_world_recipients_now {
                            if despawn_visible.contains(&recipient) {
                                handlers::send_entity_despawn(&mut server, recipient, target_id);
                            }
                        }
                        // The camp slot stays OCCUPIED for the charm's
                        // life. It used to be freed here, so the camp
                        // respawned a replacement while the charmed mob
                        // still existed; now that the mob comes back
                        // when the charm ends, freeing it would let a
                        // charm-and-wait add a mob to the camp per cast.
                        // The slot rides on the pet: the return puts the
                        // mob back into it, and a pet that dies charmed
                        // frees it through the corpse sweep like any
                        // death.
                        enemies.remove(&target_id);
                        // Build the fresh pet entity. Override hp/
                        // max_hp/yaw from the original so the charm
                        // preserves the mob's current state, and carry
                        // its slot and home so it can be given back.
                        let mut pet = Entity::from_pet_summon(owner_id, pos, mob, now);
                        pet.max_hp = max_hp;
                        pet.hp = hp;
                        pet.yaw = yaw;
                        pet.spawn_point_idx = slot_idx;
                        pet.spawn_pos = home_pos;
                        let duration = if spell.duration > 0.0 { spell.duration } else { 30.0 };
                        pet.charm_expires_at = Some(now + std::time::Duration::from_secs_f32(duration));
                        let pet_id = pet.id;
                        let pet_cell = aoi::cell_for(pet.pos.x, pet.pos.z);
                        aoi.insert(pet_id, pet_cell);
                        let spawn_visible = aoi.entities_visible_from(pet_cell);
                        let pet_recipients: Vec<ClientId> = in_world_recipients_now
                            .iter()
                            .copied()
                            .filter(|id| spawn_visible.contains(id))
                            .collect();
                        if !pet_recipients.is_empty() {
                            handlers::fan_out_pet_spawn(&mut server, &pet_recipients, &pet);
                        }
                        enemies.insert(pet_id, pet);
                        tracing::info!(
                            owner = owner_id,
                            old_enemy_id = target_id,
                            new_pet_id = pet_id,
                            duration_secs = duration,
                            spell = %spell.name,
                            "enemy charmed into pet"
                        );
                    }
                    "PET_HEAL" => {
                        // Mends the caster's own pet, whatever is targeted;
                        // the pre-flight found it alive and in reach. Warder's
                        // Mend was the one client-only pet spell (ported
                        // 2026-10-07 after the tester asked for none).
                        let own_pet_id = enemies
                            .iter()
                            .find(|(_, e)| e.owner == Some(intent.caster) && e.is_alive())
                            .map(|(id, _)| *id);
                        let Some(pet_id) = own_pet_id else {
                            refund_spell_cost(
                                &mut server,
                                &mut connections,
                                &in_world_recipients_now,
                                caster_cid,
                                intent.caster,
                                mana_cost,
                                hp_cost,
                            );
                            handlers::send_refusal(&mut server, caster_cid, "You have no pet to mend.");
                            continue;
                        };
                        let heal = spell.heal_amount.max(0.0);
                        if let Some(pet) = enemies.get_mut(&pet_id) {
                            pet.hp = (pet.hp + heal).min(pet.max_hp);
                            let (new_hp, max_hp) = (pet.hp, pet.max_hp);
                            handlers::fan_out_health_update(
                                &mut server,
                                &in_world_recipients_now,
                                pet_id,
                                new_hp,
                                max_hp,
                            );
                            tracing::info!(
                                caster = intent.caster,
                                pet_id,
                                spell = %spell.name,
                                heal,
                                "PET_HEAL applied"
                            );
                        }
                    }
                    "BIND" => {
                        // Spell batch step 2: bind the target (self, or the
                        // group member the pre-flight admitted) where THEY
                        // stand, and persist it like the Soul Binder does.
                        let raw = intent.target_id.unwrap_or(0);
                        let bound_cid: ClientId =
                            if raw == 0 || raw == intent.caster || raw >= protocol::world::ENEMY_ID_BASE {
                                caster_cid
                            } else {
                                raw as ClientId
                            };
                        let Some(bc) = connections.get_mut(&bound_cid) else {
                            continue;
                        };
                        let pos = bc.pos.into_tuple();
                        bc.bind = Some(crate::db::BindPoint { pos });
                        bind_intents.push((bc.char_id, bc.zone.clone(), pos));
                        tracing::info!(
                            caster = intent.caster,
                            bound = bound_cid as u64,
                            x = pos.0, y = pos.1, z = pos.2,
                            "BIND applied"
                        );
                        handlers::send_system_line(&mut server, bound_cid, "Your soul is bound to this place.");
                        if bound_cid != caster_cid {
                            handlers::send_system_line(&mut server, caster_cid, "Their soul is bound to this place.");
                        }
                    }
                    "PORT" => {
                        // Spell batch step 2: the destination is the server's
                        // (the caster's bind, or the nearest safe area's
                        // arrival), never the client's. Evacuate also moves the
                        // caster's alive, present group members within friendly
                        // reach; a corpse or a linkdead body stays. Everyone
                        // moved is wiped from every mob's tables first, so no
                        // train follows them, and their pets come along.
                        let caster_pos_now = connections
                            .get(&caster_cid)
                            .map(|c| c.pos)
                            .unwrap_or(caster_pos);
                        let dest = match spell.port.as_str() {
                            "bind" => connections
                                .get(&caster_cid)
                                .and_then(|c| c.bind)
                                .map(|b| Vec3f::from_tuple(b.pos))
                                .unwrap_or(super::STARTER_SPAWN),
                            _ => super::safe_areas::nearest_arrival(caster_pos_now)
                                .unwrap_or(super::STARTER_SPAWN),
                        };
                        let mut movers: Vec<ClientId> = vec![caster_cid];
                        if spell.port_group {
                            if let Some(g) = group_manager.group_of(caster_cid) {
                                for &m in &g.members {
                                    if m == caster_cid {
                                        continue;
                                    }
                                    let eligible = connections.get(&m).is_some_and(|c| {
                                        c.in_world
                                            && c.linkdead_since.is_none()
                                            && c.hp > 0.0
                                            && !c.death_processed
                                            && c.pos.distance_to(caster_pos_now) <= super::FRIENDLY_SPELL_RANGE
                                    });
                                    if eligible {
                                        movers.push(m);
                                    }
                                }
                            }
                        }
                        let mover_ids: Vec<EntityId> = movers.iter().map(|m| *m as EntityId).collect();
                        for mob_id in wipe_hate(&mut enemies, &mover_ids) {
                            handlers::fan_out_entity_target(
                                &mut server,
                                &in_world_recipients_now,
                                mob_id,
                                None,
                            );
                        }
                        for &m in &movers {
                            if let Some(c) = connections.get_mut(&m) {
                                c.pos = dest;
                                // The fight is over for the mover: without this
                                // the pet pre-pass (step 4i) re-inherits the
                                // owner's last target for up to 10 s and the
                                // pet walks straight back out of town to it.
                                c.last_attacked_enemy = None;
                                c.last_attacked_at = None;
                                regen::mark_dirty(c);
                            }
                            handlers::send_teleport(&mut server, m, dest);
                            // The mover's summoned pets come along (user call
                            // D4), set to follow so a parked one does not stand
                            // alone across the zone. Their AOI cell changes
                            // right here, so the players who gain or lose sight
                            // of them are told now rather than never. A CHARMED
                            // mob is not a pet the mover owns: it is released
                            // where it stands and walks home, as when its
                            // charmer logs out. Carrying it would put a camp
                            // mob (a named one, even) inside the safe area to
                            // turn hostile when the charm ends.
                            let pets: Vec<EntityId> = enemies
                                .iter()
                                .filter(|(_, e)| e.owner == Some(m as EntityId) && e.is_alive())
                                .map(|(id, _)| *id)
                                .collect();
                            for pet_id in pets {
                                if enemies.get(&pet_id).is_some_and(|e| e.charm_expires_at.is_some()) {
                                    if let Some(pet) = enemies.remove(&pet_id) {
                                        let mob_id = return_charmed_mob(
                                            &mut server,
                                            &mut aoi,
                                            &mut enemies,
                                            &in_world_recipients_now,
                                            pet,
                                            None,
                                            now,
                                        );
                                        tracing::info!(
                                            owner = m,
                                            pet_id,
                                            mob_id,
                                            "charm broken by the owner porting away — mob returned"
                                        );
                                    }
                                    continue;
                                }
                                let (old_cell, new_cell) = {
                                    let Some(pet) = enemies.get_mut(&pet_id) else { continue };
                                    let old_cell = aoi::cell_for(pet.pos.x, pet.pos.z);
                                    pet.pos = Vec3f { x: dest.x + 1.5, y: dest.y, z: dest.z };
                                    // last_bcast_pos is left where it was, so the
                                    // move fans on the next tick instead of
                                    // waiting for the keepalive.
                                    pet.target = None;
                                    pet.stance = entity::PetStance::Follow;
                                    (old_cell, aoi::cell_for(pet.pos.x, pet.pos.z))
                                };
                                if new_cell != old_cell {
                                    let (gained, lost) = aoi.update(pet_id, old_cell, new_cell);
                                    if let Some(pet) = enemies.get(&pet_id) {
                                        fan_entity_cell_crossing(&mut server, &aoi, &connections, pet, &gained, &lost);
                                    }
                                }
                            }
                        }
                        tracing::info!(
                            caster = intent.caster,
                            spell = %spell.name,
                            port = %spell.port,
                            moved = movers.len(),
                            x = dest.x, z = dest.z,
                            "PORT applied"
                        );
                    }
                    "NONE" | _ => {
                        // A target type with no arm. Every type in today's
                        // spells.toml has one (BIND and PORT gained theirs in
                        // spell batch step 2), and the pre-flight refuses an
                        // unknown type before the mana comes off, so this is
                        // reached only by a new type added to the data ahead of
                        // its arm. Mana is gone and nothing happens, which is
                        // indistinguishable from a bug. Say so, and give the cost
                        // back. Raised to info! as well: a player-visible
                        // failure should not be invisible at the default log
                        // level.
                        tracing::info!(
                            caster = intent.caster,
                            spell = %spell.name,
                            target_type = %spell.target_type,
                            "spell target_type not yet processed server-side; mana deducted only"
                        );
                        refund_spell_cost(
                            &mut server,
                            &mut connections,
                            &in_world_recipients_now,
                            caster_cid,
                            intent.caster,
                            mana_cost,
                            hp_cost,
                        );
                        handlers::send_refusal(
                            &mut server,
                            caster_cid,
                            "That magic has no effect here yet.",
                        );
                    }
                }
            }
        }

        // 4hb. Track 6 sub-task 5 — group-intent processing.
        //      Invite: resolve target_name → char_id, record pending
        //      invite, forward GroupInvited to invitee.
        //      Accept: GroupManager.accept; fan GroupRoster to all
        //      members on success.
        //      Leave: GroupManager.leave; fan roster (or empty for
        //      dissolved) to remaining + the leaver.
        //      Kick: leader-only action; remove target; fan rosters.
        // Helper to fan the current roster of a group with names
        // looked up from the connections map. Empty members = group
        // dissolved (last-member signal).
        let fan_roster =
            |srv: &mut renet::RenetServer,
             conns: &HashMap<ClientId, PerConnection>,
             gm: &GroupManager,
             gid: groups::GroupId,
             also_notify_dissolved: Option<ClientId>| {
                if let Some(g) = gm.groups.get(&gid) {
                    let members_with_names: Vec<(u64, String)> = g.members.iter()
                        .filter_map(|m| conns.get(m).map(|c| (*m, c.name.clone())))
                        .collect();
                    let recipients: Vec<ClientId> = g.members.clone();
                    handlers::fan_group_roster(
                        srv,
                        &recipients,
                        gid,
                        g.leader,
                        members_with_names,
                        g.loot_mode.to_u8(),
                    );
                    // Seed the group panel rows (playtest 2026-08-27: rows
                    // build zeroed client-side and resources otherwise fan
                    // only on CHANGE, so a full-HP idle member showed blank
                    // bars until something moved). One shot of the ordinary
                    // resource updates per member, sent to the group, fills
                    // every row the moment the roster lands. Existing
                    // message types; no protocol change.
                    for m in &g.members {
                        if let Some(c) = conns.get(m) {
                            handlers::fan_out_resources(srv, &recipients, c);
                        }
                    }
                } else if let Some(last) = also_notify_dissolved {
                    // Group dissolved — send an empty roster to the
                    // last member as a "your group dissolved" signal.
                    handlers::fan_group_roster(
                        srv,
                        std::slice::from_ref(&last),
                        gid,
                        last,
                        Vec::new(),
                        0,
                    );
                }
            };

        for intent in group_invite_intents.drain(..) {
            // Resolve target by name (case-insensitive). The
            // characters table has a NOCASE collation on name.
            let target_cid = connections
                .iter()
                .find(|(_, c)| c.name.eq_ignore_ascii_case(&intent.target_name))
                .map(|(id, _)| *id);
            let Some(target_cid) = target_cid else {
                tracing::debug!(
                    inviter = intent.inviter,
                    target = %intent.target_name,
                    "GroupInvite — target offline or unknown"
                );
                // The client already printed "Invited X to your group." A
                // misspelled or offline name otherwise produces nothing at all,
                // on either side.
                handlers::send_refusal(
                    &mut server,
                    intent.inviter as ClientId,
                    &format!("{} isn't online.", intent.target_name),
                );
                continue;
            };
            if target_cid as u64 == intent.inviter {
                continue; // can't invite self
            }
            let from_name = connections.get(&(intent.inviter as ClientId))
                .map(|c| c.name.clone())
                .unwrap_or_default();
            let _gid = group_manager.record_invite(intent.inviter as ClientId, target_cid);
            handlers::send_group_invited(
                &mut server,
                target_cid,
                intent.inviter,
                from_name,
            );
            tracing::info!(
                inviter = intent.inviter,
                invitee = target_cid as u64,
                "GroupInvite recorded; GroupInvited forwarded"
            );
        }

        for intent in group_accept_intents.drain(..) {
            let invitee_cid = intent.invitee as ClientId;
            let from_cid = intent.from as ClientId;
            if let Some(gid) = group_manager.accept(invitee_cid, from_cid) {
                tracing::info!(
                    invitee = intent.invitee,
                    inviter = intent.from,
                    gid,
                    "GroupAccept — invitee joined"
                );
                fan_roster(&mut server, &connections, &group_manager, gid, None);
            } else {
                tracing::debug!(
                    invitee = intent.invitee,
                    inviter = intent.from,
                    "GroupAccept rejected (no pending invite / already in a group)"
                );
            }
        }

        for intent in group_leave_intents.drain(..) {
            let cid = intent.member as ClientId;
            if let Some((gid, remaining, dissolved)) = group_manager.leave(cid) {
                tracing::info!(
                    member = intent.member,
                    gid,
                    remaining = remaining.len(),
                    dissolved,
                    "GroupLeave processed"
                );
                // The leaver always gets an empty roster so their HUD clears.
                handlers::fan_group_roster(
                    &mut server,
                    std::slice::from_ref(&cid),
                    gid,
                    cid,
                    Vec::new(),
                    0,
                );
                if dissolved {
                    // Survivors (0 or 1) also need a dissolution notice
                    // so their HUD clears.
                    for m in &remaining {
                        handlers::fan_group_roster(
                            &mut server,
                            std::slice::from_ref(m),
                            gid,
                            *m,
                            Vec::new(),
                            0,
                        );
                    }
                } else {
                    fan_roster(&mut server, &connections, &group_manager, gid, None);
                }
            }
        }

        for intent in group_kick_intents.drain(..) {
            let leader_cid = intent.leader as ClientId;
            let Some(group) = group_manager.group_of(leader_cid) else {
                // Previously no log AND no reply, so a failed /kick was
                // invisible on both sides and untriageable from server.log.
                tracing::debug!(leader = intent.leader, "GroupKick — not in a group");
                handlers::send_refusal(&mut server, leader_cid, "You aren't in a group.");
                continue;
            };
            if group.leader != leader_cid {
                // Only a forged client reaches this (the UI hides kick for
                // non-leaders), so log for triage but say nothing: a reply is an
                // oracle with no honest beneficiary.
                tracing::debug!(leader = intent.leader, "GroupKick — not the leader");
                continue;
            }
            let gid = group.id;
            // Resolve target name within the group's roster.
            let target_cid: Option<ClientId> = group.members.iter().copied()
                .find(|m| {
                    connections.get(m)
                        .map(|c| c.name.eq_ignore_ascii_case(&intent.target_name))
                        .unwrap_or(false)
                });
            let Some(target_cid) = target_cid else {
                // Had neither log nor reply: a misspelled /kick simply did
                // nothing, visibly or in server.log.
                tracing::debug!(
                    leader = intent.leader,
                    target = %intent.target_name,
                    "GroupKick — no such member in the group"
                );
                handlers::send_refusal(
                    &mut server,
                    leader_cid,
                    &format!("{} isn't in your group.", intent.target_name),
                );
                continue;
            };
            if target_cid == leader_cid {
                // Reachable from the UI by typing your own name, unlike the
                // non-leader case, so it earns a reply.
                handlers::send_refusal(
                    &mut server, leader_cid, "Use /leave to leave your own group.",
                );
                continue;
            }
            if let Some((_gid, remaining, dissolved)) = group_manager.leave(target_cid) {
                tracing::info!(
                    leader = intent.leader,
                    kicked = target_cid as u64,
                    gid,
                    remaining = remaining.len(),
                    dissolved,
                    "GroupKick processed"
                );
                // Notify the kicked member their group ended (from their POV).
                handlers::fan_group_roster(
                    &mut server,
                    std::slice::from_ref(&target_cid),
                    gid,
                    target_cid,
                    Vec::new(),
                    0,
                );
                if dissolved {
                    // Survivors (0 or 1) also need a dissolution notice.
                    for m in &remaining {
                        handlers::fan_group_roster(
                            &mut server,
                            std::slice::from_ref(m),
                            gid,
                            *m,
                            Vec::new(),
                            0,
                        );
                    }
                } else {
                    fan_roster(&mut server, &connections, &group_manager, gid, None);
                }
            }
        }

        // 4hbb. PD_W0014 — leader sets the group's loot distribution
        //       mode. Validate the sender is the leader, set the mode,
        //       and re-fan the roster so every member sees it.
        for intent in group_loot_mode_intents.drain(..) {
            let leader_cid = intent.leader as ClientId;
            let gid = match group_manager.group_of(leader_cid) {
                Some(g) if g.leader == leader_cid => g.id,
                _ => continue, // not grouped, or not the leader
            };
            if let Some(g) = group_manager.groups.get_mut(&gid) {
                g.loot_mode = groups::LootMode::from_u8(intent.mode);
            }
            tracing::info!(
                leader = intent.leader,
                gid,
                mode = intent.mode,
                "group loot mode set"
            );
            fan_roster(&mut server, &connections, &group_manager, gid, None);
        }

        // 4hbb-bis. PD_W0014 — a player toggled /autosplit. Fan a one-line
        //       notice to their group-mates (transparency: it changes
        //       whether the group shares coin from that member's loots).
        //       The toggler already echoes locally (hud.gd), so they're
        //       excluded here; solo players get no notice at all.
        for intent in autosplit_notice_intents.drain(..) {
            let toggler_cid = intent.char_id as ClientId;
            let gid = match group_manager.group_of(toggler_cid) {
                Some(g) => g.id,
                None => continue, // solo → nobody to notify
            };
            let toggler_name = connections
                .get(&toggler_cid)
                .map(|c| c.name.clone())
                .unwrap_or_else(|| "Someone".to_string());
            let text = format!(
                "{} set auto-split {}.",
                toggler_name,
                if intent.on { "on" } else { "off" }
            );
            let members: Vec<ClientId> = match group_manager.groups.get(&gid) {
                Some(g) => g.members.clone(),
                None => continue,
            };
            for member_cid in members {
                if member_cid == toggler_cid {
                    continue; // toggler echoed locally already
                }
                if connections.get(&member_cid).map_or(false, |c| c.in_world) {
                    handlers::send_group_notice(&mut server, member_cid, text.clone());
                }
            }
        }

        // 4hbc. PD_W0014 — leader hands leadership to a member. Validate
        //       (current leader → existing member) and re-fan the roster
        //       so both old and new leader see the change. Fixes the
        //       launcher-mode gap where pass-leadership was local-only.
        for intent in group_pass_leadership_intents.drain(..) {
            if let Some(gid) = group_manager
                .pass_leadership(intent.leader as ClientId, intent.new_leader as ClientId)
            {
                tracing::info!(
                    from = intent.leader,
                    to = intent.new_leader,
                    gid,
                    "group leadership passed"
                );
                fan_roster(&mut server, &connections, &group_manager, gid, None);
            }
        }

        // 4hc. Track 12 Piece A — apply pet commands. Locate each
        //      caster's pet in `enemies`; for Attack validate the
        //      target enemy is alive; for Back clear the pet's
        //      target. Both stamp `command_at` so the pre-AI
        //      inheritance pass (4i) skips re-targeting from
        //      `last_attacked_enemy` while the sticky window holds.
        if !pet_command_intents.is_empty() {
            for intent in pet_command_intents.drain(..) {
                use protocol::world::pet_command as cmd;
                let pet_id_opt: Option<EntityId> = enemies
                    .iter()
                    .find(|(_, e)| e.owner == Some(intent.owner) && e.is_alive())
                    .map(|(id, _)| *id);
                let Some(pet_id) = pet_id_opt else {
                    tracing::debug!(owner = intent.owner, command = intent.command, "PetCommand dropped — no live pet");
                    continue;
                };
                match intent.command {
                    cmd::ATTACK => {
                        let Some(target_id) = intent.target_id else {
                            tracing::debug!(owner = intent.owner, "PetCommand ATTACK dropped — no target_id");
                            continue;
                        };
                        // Pet-on-player engage: target ids below
                        // ENEMY_ID_BASE are player char_ids. PvP rules
                        // mirror direct player-on-player melee (both
                        // need /pvp on, can't target self).
                        if target_id < protocol::world::ENEMY_ID_BASE {
                            let owner_cid = intent.owner as ClientId;
                            let target_cid = target_id as ClientId;
                            let allowed = if target_cid == owner_cid {
                                false
                            } else {
                                match (
                                    connections.get(&owner_cid),
                                    connections.get(&target_cid),
                                ) {
                                    (Some(a), Some(t)) if t.in_world && t.hp > 0.0 => combat::can_attack(
                                        a, t,
                                        a.zone.as_deref(),
                                        t.zone.as_deref(),
                                    ),
                                    _ => false,
                                }
                            };
                            if !allowed {
                                let t_name = connections
                                    .get(&target_cid)
                                    .map(|c| c.name.clone())
                                    .unwrap_or_default();
                                let line = if !t_name.is_empty() {
                                    format!("Unable to attack {}.", t_name)
                                } else {
                                    "Unable to attack that target.".to_string()
                                };
                                handlers::fan_out_chat_message(
                                    &mut server,
                                    &[owner_cid],
                                    "",
                                    protocol::world::ChatChannel::System,
                                    &line,
                                );
                                tracing::debug!(
                                    owner = intent.owner,
                                    target = target_id,
                                    "PetCommand ATTACK rejected — player PvP gate"
                                );
                                continue;
                            }
                            if let Some(pet) = enemies.get_mut(&pet_id) {
                                pet.target = Some(target_id);
                                pet.command_at = Some(now);
                                pet.stance = entity::PetStance::Follow;
                            }
                            tracing::info!(owner = intent.owner, pet_id, target = target_id, "PetCommand ATTACK (player)");
                            continue;
                        }
                        // Resolve target's liveness, owner (for pet
                        // targets), and pet-ness in one immutable
                        // lookup so the PvP gate below can run before
                        // the mutable pet-borrow at line ~3767.
                        let (target_alive, target_owner_opt, target_is_pet) =
                            match enemies.get(&target_id) {
                                Some(e) => (e.is_alive(), e.owner, e.is_pet()),
                                None => (false, None, false),
                            };
                        if !target_alive {
                            tracing::debug!(owner = intent.owner, target = target_id, "PetCommand ATTACK dropped — target not alive");
                            continue;
                        }
                        // Pet-on-pet engage: owner cannot point their
                        // pet at their own pet, and the two owners must
                        // both have /pvp on (mirrors the player-on-pet
                        // gate at line ~2085). Rejection routes the
                        // same "Unable to attack" line to the owner so
                        // the failure mode matches direct attacks.
                        if target_is_pet {
                            let owner_cid = intent.owner as ClientId;
                            // Own pet — refuse with a distinct line so
                            // the player isn't confused by an
                            // "Unable to attack Self's Wolf." rejection.
                            if target_owner_opt == Some(intent.owner) {
                                handlers::fan_out_chat_message(
                                    &mut server,
                                    &[owner_cid],
                                    "",
                                    protocol::world::ChatChannel::System,
                                    "Your pet won't attack itself.",
                                );
                                continue;
                            }
                            let allowed = match target_owner_opt {
                                Some(t_owner) => {
                                    let t_cid = t_owner as ClientId;
                                    match (
                                        connections.get(&owner_cid),
                                        connections.get(&t_cid),
                                    ) {
                                        (Some(a), Some(b)) => combat::can_attack(
                                            a, b,
                                            a.zone.as_deref(),
                                            b.zone.as_deref(),
                                        ),
                                        _ => false,
                                    }
                                }
                                None => true, // unowned pet (charm etc.) — treat as enemy
                            };
                            if !allowed {
                                let t_owner_name = target_owner_opt
                                    .and_then(|id| connections.get(&(id as ClientId)).map(|c| c.name.clone()))
                                    .unwrap_or_default();
                                let pet_name = enemies
                                    .get(&target_id)
                                    .map(|p| p.mob.name.clone())
                                    .unwrap_or_default();
                                let line = if !t_owner_name.is_empty() && !pet_name.is_empty() {
                                    format!("Unable to attack {}'s {}.", t_owner_name, pet_name)
                                } else if !pet_name.is_empty() {
                                    format!("Unable to attack the {}.", pet_name)
                                } else {
                                    "Unable to attack that target.".to_string()
                                };
                                handlers::fan_out_chat_message(
                                    &mut server,
                                    &[owner_cid],
                                    "",
                                    protocol::world::ChatChannel::System,
                                    &line,
                                );
                                tracing::debug!(
                                    owner = intent.owner,
                                    target = target_id,
                                    "PetCommand ATTACK rejected — pet PvP gate"
                                );
                                continue;
                            }
                        }
                        if let Some(pet) = enemies.get_mut(&pet_id) {
                            pet.target = Some(target_id);
                            pet.command_at = Some(now);
                            // ATTACK implies "engage" — restore Follow
                            // stance so the pet chases / returns to
                            // the owner after the kill.
                            pet.stance = entity::PetStance::Follow;
                        }
                        tracing::info!(owner = intent.owner, pet_id, target = target_id, "PetCommand ATTACK");
                    }
                    cmd::BACK | cmd::FOLLOW => {
                        if let Some(pet) = enemies.get_mut(&pet_id) {
                            pet.target = None;
                            pet.command_at = Some(now);
                            pet.stance = entity::PetStance::Follow;
                        }
                        tracing::info!(owner = intent.owner, pet_id, "PetCommand BACK/FOLLOW");
                    }
                    cmd::GUARD => {
                        if let Some(pet) = enemies.get_mut(&pet_id) {
                            // Park the pet at its current spot. Drop
                            // any inherited target so the pet doesn't
                            // immediately chase off — Guard only
                            // engages via incoming damage or a follow-
                            // up Attack command.
                            pet.target = None;
                            pet.command_at = Some(now);
                            pet.stance = entity::PetStance::Guard;
                        }
                        tracing::info!(owner = intent.owner, pet_id, "PetCommand GUARD");
                    }
                    cmd::SIT => {
                        if let Some(pet) = enemies.get_mut(&pet_id) {
                            // Full passive: drop target, stand still,
                            // ignore incoming damage retargeting.
                            pet.target = None;
                            pet.command_at = Some(now);
                            pet.stance = entity::PetStance::Sit;
                        }
                        tracing::info!(owner = intent.owner, pet_id, "PetCommand SIT");
                    }
                    _ => {
                        tracing::debug!(owner = intent.owner, command = intent.command, "PetCommand variant not yet implemented");
                    }
                }
            }
        }

        // 4hd. Track 13.2 / 14.3 — apply move-item intents. Routes
        //      through `move_across`, which handles every combination
        //      of "base" and "bag_<i>" locations (equip slots stay
        //      on the dedicated equip/unequip path). Touched slots
        //      come back tagged with their location string so the
        //      InventoryDelta fan-out uses the right address per
        //      slot.
        if !move_item_intents.is_empty() {
            for intent in move_item_intents.drain(..) {
                let owner_cid = intent.owner as ClientId;
                // PD_W0028 escrow: an offered slot moves for no one — not
                // out of (src) and not merged/swapped into (dst).
                if trade_locked(&mut server, &trade_manager, owner_cid, &intent.src_location, intent.src_slot)
                    || trade_locked(&mut server, &trade_manager, owner_cid, &intent.dst_location, intent.dst_slot)
                {
                    continue;
                }
                let Some(conn) = connections.get_mut(&owner_cid) else {
                    continue;
                };
                let touched = match conn.inventory.move_across(
                    &intent.src_location,
                    intent.src_slot,
                    &intent.dst_location,
                    intent.dst_slot,
                ) {
                    Ok(t) => t,
                    Err(e) => {
                        tracing::info!(
                            owner = intent.owner,
                            src_loc = %intent.src_location,
                            src_slot = intent.src_slot,
                            dst_loc = %intent.dst_location,
                            dst_slot = intent.dst_slot,
                            error = %e,
                            "MoveItem rejected"
                        );
                        // A rejection used to send the client NOTHING, which made
                        // every divergence permanent: the client asked to move an
                        // item the server does not have there, learned nothing from
                        // being refused, and asked again. Playtest 2026-08-18 shows
                        // the same slot refused for half an hour, only resolving on
                        // relog. Answer with the truth about both slots so a wrong
                        // client corrects itself on its next mistake.
                        correct_client_slots(
                            &mut server,
                            owner_cid,
                            conn,
                            &intent.src_location,
                            intent.src_slot,
                            &intent.dst_location,
                            intent.dst_slot,
                        );
                        continue;
                    }
                };
                if touched.is_empty() {
                    // A move the server treats as a no-op. The client still moved
                    // something on its own screen to get here, so it needs the same
                    // correction as an outright rejection.
                    correct_client_slots(
                        &mut server,
                        owner_cid,
                        conn,
                        &intent.src_location,
                        intent.src_slot,
                        &intent.dst_location,
                        intent.dst_slot,
                    );
                    continue;
                }
                conn.inventory_dirty = true;
                // Snapshot the touched slot contents so we can fan
                // Deltas without holding a mutable borrow on conn.
                // peek_at speaks every location move_across can touch
                // (base, bag_<i>, and PD_W0027's cursor), so the fan
                // reads through it rather than re-deriving each address.
                let deltas: Vec<(String, u32, Option<(String, u32)>)> = touched
                    .iter()
                    .map(|(loc, slot)| (loc.clone(), *slot, conn.inventory.peek_at(loc, *slot)))
                    .collect();
                for (loc, slot, payload) in deltas {
                    let (item_path, count) = match payload {
                        Some((p, c)) => (Some(p), c),
                        None => (None, 0),
                    };
                    handlers::send_inventory_delta(
                        &mut server,
                        owner_cid,
                        loc,
                        slot,
                        item_path,
                        count,
                    );
                }
                tracing::info!(
                    owner = intent.owner,
                    src_loc = %intent.src_location,
                    src_slot = intent.src_slot,
                    dst_loc = %intent.dst_location,
                    dst_slot = intent.dst_slot,
                    "MoveItem applied"
                );
            }
        }

        // 4hd2. PD_W0028 — apply trade intents (docs/design/trade_window.md).
        //       Escrow by locking: offers are references into the owner's
        //       inventory, locks derive from live sessions, and the commit
        //       is ONE db transaction spanning both characters. Refusals
        //       answer in chat except arms only a forged client can reach.
        if !trade_intents.is_empty() {
            for intent in trade_intents.drain(..) {
                match intent {
                    TradeI::Request { requester, target } => {
                        let rcid = requester as ClientId;
                        // Ledger 10: players only. Enemy / bag / pet ids live
                        // in the reserved partitions; NPCs are not entities.
                        if target >= protocol::world::ENEMY_ID_BASE {
                            handlers::send_refusal(
                                &mut server,
                                rcid,
                                "You can only trade with players.",
                            );
                            continue;
                        }
                        let tcid = target as ClientId;
                        let target_state = connections.get(&tcid).and_then(|t| {
                            if t.in_world && !t.death_processed && t.hp > 0.0 {
                                Some((t.pos, t.name.clone()))
                            } else {
                                None
                            }
                        });
                        let Some((tpos, tname)) = target_state else {
                            handlers::send_refusal(&mut server, rcid, "They are not here to trade.");
                            continue;
                        };
                        let dist = connections
                            .get(&rcid)
                            .map(|r| r.pos.distance_to(tpos))
                            .unwrap_or(f32::MAX);
                        if dist > super::trade::TRADE_RANGE {
                            handlers::send_refusal(&mut server, rcid, "They are too far away to trade.");
                            continue;
                        }
                        let rname = connections
                            .get(&rcid)
                            .map(|c| c.name.clone())
                            .unwrap_or_default();
                        match trade_manager.open(rcid, tcid) {
                            Ok(()) => {
                                // Instant open, both sides (decided 2026-09-19).
                                handlers::send_trade_opened(&mut server, rcid, target, tname);
                                handlers::send_trade_opened(&mut server, tcid, requester, rname);
                                tracing::info!(a = requester, b = target, "trade opened");
                            }
                            // Ledger 8: busy / self refusals answer the
                            // SENDER only — quiet to the target, so a spammy
                            // requester cannot use refusal echoes to harass.
                            Err(e) => handlers::send_refusal(&mut server, rcid, e),
                        }
                    }
                    TradeI::OfferItem { owner, window_slot, from_location, from_slot } => {
                        let cid = owner as ClientId;
                        if trade_manager.session_of(cid).is_none() {
                            handlers::send_refusal(&mut server, cid, "You are not trading.");
                            continue;
                        }
                        // Ledger 9: the server re-reads everything from ITS
                        // inventory. A held bag's inners (bag_255) are never
                        // addressable, mirroring the inventory entry points.
                        if from_location == format!("bag_{}", inventory::CURSOR_BAG_KEY) {
                            continue; // forged-only
                        }
                        // Resolve the source. "cursor" parks the held stack
                        // into an empty inventory slot first (no merging, so
                        // the offer stays one discrete referenced stack) and
                        // frees the hand for the next pickup.
                        let (loc, slot) = if from_location == "cursor" {
                            match connections
                                .get_mut(&cid)
                                .map(|c| c.inventory.park_cursor_stack())
                            {
                                Some(Ok(((l, s), touched))) => {
                                    if let Some(conn) = connections.get_mut(&cid) {
                                        conn.inventory_dirty = true;
                                        let deltas: Vec<(String, u32, Option<(String, u32)>)> =
                                            touched
                                                .iter()
                                                .map(|(tl, ts)| {
                                                    (tl.clone(), *ts, conn.inventory.peek_at(tl, *ts))
                                                })
                                                .collect();
                                        for (tl, ts, payload) in deltas {
                                            let (p, c2) = match payload {
                                                Some((p, c2)) => (Some(p), c2),
                                                None => (None, 0),
                                            };
                                            handlers::send_inventory_delta(
                                                &mut server, cid, tl, ts, p, c2,
                                            );
                                        }
                                    }
                                    (l, s)
                                }
                                Some(Err(e)) => {
                                    handlers::send_refusal(&mut server, cid, e);
                                    continue;
                                }
                                None => continue,
                            }
                        } else {
                            (from_location, from_slot)
                        };
                        // The referenced slot must hold something, and a
                        // bag-typed offer must be an EMPTY bag (a full bag's
                        // contents transfer is its own later slice).
                        let peek = connections
                            .get(&cid)
                            .and_then(|c| c.inventory.peek_at(&loc, slot));
                        let Some((path, offer_count)) = peek else {
                            handlers::send_refusal(
                                &mut server,
                                cid,
                                "There's nothing in that slot to offer.",
                            );
                            continue;
                        };
                        if items::bag_num_slots(&path).is_some() {
                            // A bag can only rest in a base slot, so its key
                            // is its base index.
                            let nonempty = loc == "base"
                                && connections
                                    .get(&cid)
                                    .map(|c| c.inventory.bag_key_is_nonempty(slot as u8))
                                    .unwrap_or(false);
                            if nonempty {
                                handlers::send_refusal(
                                    &mut server,
                                    cid,
                                    "Empty the bag before trading it.",
                                );
                                continue;
                            }
                        }
                        match trade_manager.offer_item(
                            cid,
                            window_slot as usize,
                            loc,
                            slot,
                            offer_count,
                        ) {
                            Ok(()) => {
                                if let Some(session) = trade_manager.session_of(cid) {
                                    fan_trade_state(&mut server, &connections, session);
                                }
                            }
                            Err(e) => handlers::send_refusal(&mut server, cid, e),
                        }
                    }
                    TradeI::RetrieveItem { owner, window_slot } => {
                        let cid = owner as ClientId;
                        match trade_manager.retrieve_item(cid, window_slot as usize) {
                            Ok(()) => {
                                if let Some(session) = trade_manager.session_of(cid) {
                                    fan_trade_state(&mut server, &connections, session);
                                }
                            }
                            Err(e) => handlers::send_refusal(&mut server, cid, e),
                        }
                    }
                    TradeI::OfferCoins { owner, coins } => {
                        let cid = owner as ClientId;
                        // Forged negatives would STEAL from the partner at
                        // commit; only a modified client can send one.
                        if coins.platinum < 0
                            || coins.gold < 0
                            || coins.silver < 0
                            || coins.copper < 0
                        {
                            continue; // forged-only
                        }
                        // Ledger 4: absolute per-tier amounts, each within
                        // the live wallet (the four stacks are independent
                        // by design — no auto-breaking in a trade).
                        let covered = connections
                            .get(&cid)
                            .map(|c| {
                                coins.platinum <= c.coins.platinum
                                    && coins.gold <= c.coins.gold
                                    && coins.silver <= c.coins.silver
                                    && coins.copper <= c.coins.copper
                            })
                            .unwrap_or(false);
                        if !covered {
                            handlers::send_refusal(&mut server, cid, "You don't have that much coin.");
                            // Re-fan the APPLIED state so the client's coin
                            // fields snap back to what the server holds
                            // instead of displaying the refused amount.
                            if let Some(session) = trade_manager.session_of(cid) {
                                fan_trade_state(&mut server, &connections, session);
                            }
                            continue;
                        }
                        match trade_manager.offer_coins(cid, coins) {
                            Ok(()) => {
                                if let Some(session) = trade_manager.session_of(cid) {
                                    fan_trade_state(&mut server, &connections, session);
                                }
                            }
                            Err(e) => handlers::send_refusal(&mut server, cid, e),
                        }
                    }
                    TradeI::Cancel { owner } => {
                        close_trade(
                            &mut server,
                            &mut trade_manager,
                            owner as ClientId,
                            "Trade cancelled.",
                        );
                    }
                    TradeI::Accept { owner } => {
                        let cid = owner as ClientId;
                        let both = match trade_manager.accept(cid) {
                            Err(e) => {
                                handlers::send_refusal(&mut server, cid, e);
                                continue;
                            }
                            Ok(b) => b,
                        };
                        if !both {
                            if let Some(session) = trade_manager.session_of(cid) {
                                fan_trade_state(&mut server, &connections, session);
                            }
                            continue;
                        }
                        // ── COMMIT. Everything below runs inside this tick,
                        // so nothing interleaves between validation and
                        // application (ledger 4's argument). ──
                        let Some(session) = trade_manager.session_of(cid).cloned() else {
                            continue;
                        };
                        let (acid, bcid) = (session.a.cid, session.b.cid);
                        // Re-validate liveness + range (ledger 7).
                        let both_ok = match (connections.get(&acid), connections.get(&bcid)) {
                            (Some(a), Some(b)) => {
                                a.in_world
                                    && b.in_world
                                    && !a.death_processed
                                    && !b.death_processed
                                    && a.hp > 0.0
                                    && b.hp > 0.0
                                    && a.pos.distance_to(b.pos) <= super::trade::TRADE_RANGE
                            }
                            _ => false,
                        };
                        if !both_ok {
                            close_trade(
                                &mut server,
                                &mut trade_manager,
                                acid,
                                "Trade failed — you are no longer together.",
                            );
                            continue;
                        }
                        // Re-validate coin coverage per tier (ledger 4 —
                        // wallets can shrink between offer and commit via a
                        // vendor purchase; items cannot move, they are locked).
                        let coins_covered = |cid: ClientId, offered: protocol::world::Coins| {
                            connections
                                .get(&cid)
                                .map(|c| {
                                    offered.platinum <= c.coins.platinum
                                        && offered.gold <= c.coins.gold
                                        && offered.silver <= c.coins.silver
                                        && offered.copper <= c.coins.copper
                                })
                                .unwrap_or(false)
                        };
                        if !coins_covered(acid, session.a.coins)
                            || !coins_covered(bcid, session.b.coins)
                        {
                            trade_manager.clear_accepts_of(acid);
                            for c in [acid, bcid] {
                                handlers::send_refusal(
                                    &mut server,
                                    c,
                                    "The coin offer is no longer covered.",
                                );
                            }
                            if let Some(s) = trade_manager.session_of(acid) {
                                fan_trade_state(&mut server, &connections, s);
                            }
                            continue;
                        }
                        // Count verification: locks stop a slot moving, but
                        // external growth (a loot or purchase top-up merging
                        // into the offered stack) would raise what changes
                        // hands after both accepts. The offer snapshotted its
                        // count; a live mismatch refuses and re-opens.
                        let counts_match = |cid: ClientId, side: &super::trade::TradeSide| {
                            connections
                                .get(&cid)
                                .map(|conn| {
                                    side.offered_slots().all(|(l, s, offered)| {
                                        conn.inventory
                                            .peek_at(l, *s)
                                            .map(|(_, live)| live == *offered)
                                            .unwrap_or(false)
                                    })
                                })
                                .unwrap_or(false)
                        };
                        if !counts_match(acid, &session.a) || !counts_match(bcid, &session.b) {
                            trade_manager.clear_accepts_of(acid);
                            for c in [acid, bcid] {
                                handlers::send_refusal(
                                    &mut server,
                                    c,
                                    "An offered stack changed — check the window and retry.",
                                );
                            }
                            if let Some(s) = trade_manager.session_of(acid) {
                                fan_trade_state(&mut server, &connections, s);
                            }
                            continue;
                        }
                        // Resolve every offered reference to (loc, slot,
                        // path, count). Locked slots cannot have changed and
                        // counts just verified, so a None here is internal
                        // inconsistency — fail shut.
                        let resolve = |cid: ClientId, side: &super::trade::TradeSide| {
                            let conn = connections.get(&cid)?;
                            side.offered_slots()
                                .map(|(l, s, _)| {
                                    conn.inventory
                                        .peek_at(l, *s)
                                        .map(|(p, c)| (l.clone(), *s, p, c))
                                })
                                .collect::<Option<Vec<(String, u32, String, u32)>>>()
                        };
                        let (Some(a_gives), Some(b_gives)) =
                            (resolve(acid, &session.a), resolve(bcid, &session.b))
                        else {
                            close_trade(&mut server, &mut trade_manager, acid, "Trade failed.");
                            continue;
                        };
                        // Capacity (ledger 6): simulate each receiver on a
                        // CLONE — outgoing removed first, then the partner's
                        // stacks through the real placer (merge_capped
                        // semantics, ledger 5). Refuse cleanly, window open.
                        let simulate = |cid: ClientId,
                                        gives: &[(String, u32, String, u32)],
                                        receives: &[(String, u32, String, u32)]|
                         -> bool {
                            let Some(conn) = connections.get(&cid) else {
                                return false;
                            };
                            let mut sim = conn.inventory.clone();
                            for (l, s, _, _) in gives {
                                if sim.destroy_at(l, *s, 0).is_err() {
                                    return false;
                                }
                            }
                            for (_, _, path, count) in receives {
                                match sim.add_item_locating(path, *count) {
                                    Ok((_, leftover)) if leftover == 0 => {}
                                    _ => return false,
                                }
                            }
                            true
                        };
                        let a_fits = simulate(acid, &a_gives, &b_gives);
                        let b_fits = simulate(bcid, &b_gives, &a_gives);
                        if !a_fits || !b_fits {
                            trade_manager.clear_accepts_of(acid);
                            let full = if a_fits { bcid } else { acid };
                            for c in [acid, bcid] {
                                let text = if c == full {
                                    "Your bags cannot hold their offer."
                                } else {
                                    "Their bags cannot hold your offer."
                                };
                                handlers::send_refusal(&mut server, c, text);
                            }
                            if let Some(s) = trade_manager.session_of(acid) {
                                fan_trade_state(&mut server, &connections, s);
                            }
                            continue;
                        }
                        // Apply for real, with full revert clones (the db
                        // write below can still fail; the in-memory swap
                        // must be reversible until it commits).
                        let revert = |connections: &HashMap<ClientId, PerConnection>,
                                      cid: ClientId| {
                            connections.get(&cid).map(|c| (c.inventory.clone(), c.coins))
                        };
                        let (Some(a_before), Some(b_before)) =
                            (revert(&connections, acid), revert(&connections, bcid))
                        else {
                            close_trade(&mut server, &mut trade_manager, acid, "Trade failed.");
                            continue;
                        };
                        let mut touched_by: HashMap<ClientId, Vec<(String, u32)>> =
                            HashMap::new();
                        let mut apply_side = |cid: ClientId,
                                              gives: &[(String, u32, String, u32)],
                                              receives: &[(String, u32, String, u32)],
                                              pay: protocol::world::Coins,
                                              gain: protocol::world::Coins,
                                              connections: &mut HashMap<ClientId, PerConnection>|
                         -> bool {
                            let Some(conn) = connections.get_mut(&cid) else {
                                return false;
                            };
                            let touched = touched_by.entry(cid).or_default();
                            for (l, s, _, _) in gives {
                                if conn.inventory.destroy_at(l, *s, 0).is_err() {
                                    return false;
                                }
                                touched.push((l.clone(), *s));
                            }
                            for (_, _, path, count) in receives {
                                match conn.inventory.add_item_locating(path, *count) {
                                    Ok((placed, 0)) => touched.extend(placed),
                                    _ => return false,
                                }
                            }
                            conn.coins.platinum -= pay.platinum;
                            conn.coins.gold -= pay.gold;
                            conn.coins.silver -= pay.silver;
                            conn.coins.copper -= pay.copper;
                            conn.coins.platinum += gain.platinum;
                            conn.coins.gold += gain.gold;
                            conn.coins.silver += gain.silver;
                            conn.coins.copper += gain.copper;
                            conn.inventory_dirty = true;
                            true
                        };
                        let applied = apply_side(
                            acid,
                            &a_gives,
                            &b_gives,
                            session.a.coins,
                            session.b.coins,
                            &mut connections,
                        ) && apply_side(
                            bcid,
                            &b_gives,
                            &a_gives,
                            session.b.coins,
                            session.a.coins,
                            &mut connections,
                        );
                        let persist_ok = if applied {
                            let rows_a = connections
                                .get(&acid)
                                .map(|c| (c.inventory.to_rows(), c.coins));
                            let rows_b = connections
                                .get(&bcid)
                                .map(|c| (c.inventory.to_rows(), c.coins));
                            match (rows_a, rows_b) {
                                (Some((ra, ca)), Some((rb, cb))) => {
                                    match db::commit_trade(
                                        &pool,
                                        acid as i64,
                                        &ra,
                                        ca,
                                        bcid as i64,
                                        &rb,
                                        cb,
                                    )
                                    .await
                                    {
                                        Ok(()) => true,
                                        Err(e) => {
                                            tracing::error!(
                                                a = acid as u64,
                                                b = bcid as u64,
                                                error = %e,
                                                "commit_trade persist failed"
                                            );
                                            false
                                        }
                                    }
                                }
                                _ => false,
                            }
                        } else {
                            false
                        };
                        if !persist_ok {
                            // Revert BOTH sides to the pre-apply snapshots;
                            // nothing was sent yet, so there is nothing to
                            // un-send (the corpse-loot doctrine).
                            if let Some(c) = connections.get_mut(&acid) {
                                c.inventory = a_before.0.clone();
                                c.coins = a_before.1;
                            }
                            if let Some(c) = connections.get_mut(&bcid) {
                                c.inventory = b_before.0.clone();
                                c.coins = b_before.1;
                            }
                            close_trade(&mut server, &mut trade_manager, acid, "Trade failed.");
                            continue;
                        }
                        // Committed. Close the session, then fan the truth:
                        // deltas for every touched slot, wallets, and the
                        // committed close. The window itself granted nothing.
                        trade_manager.close(acid);
                        for c in [acid, bcid] {
                            if let Some(conn) = connections.get(&c) {
                                if let Some(touched) = touched_by.get(&c) {
                                    for (l, s) in touched {
                                        let (p, cnt) = match conn.inventory.peek_at(l, *s) {
                                            Some((p, cnt)) => (Some(p), cnt),
                                            None => (None, 0),
                                        };
                                        handlers::send_inventory_delta(
                                            &mut server,
                                            c,
                                            l.clone(),
                                            *s,
                                            p,
                                            cnt,
                                        );
                                    }
                                }
                                handlers::send_coins_update(&mut server, c, conn.coins);
                            }
                            handlers::send_trade_closed(&mut server, c, true, "Trade complete.");
                        }
                        tracing::info!(
                            a = acid as u64,
                            b = bcid as u64,
                            a_stacks = a_gives.len(),
                            b_stacks = b_gives.len(),
                            a_coins = session.a.coins.total_copper(),
                            b_coins = session.b.coins.total_copper(),
                            "trade committed"
                        );
                    }
                }
            }
        }

        // 4hd3. PD_W0028 — trade session sweep: range, liveness, and
        //       presence, every tick (ledger 7). A session survives only
        //       while both parties are connected, in-world, alive, and
        //       within TRADE_RANGE of each other.
        for (acid, bcid) in trade_manager.open_session_cids() {
            let healthy = match (connections.get(&acid), connections.get(&bcid)) {
                (Some(a), Some(b)) => {
                    a.in_world
                        && b.in_world
                        && !a.death_processed
                        && !b.death_processed
                        && a.hp > 0.0
                        && b.hp > 0.0
                        && a.pos.distance_to(b.pos) <= super::trade::TRADE_RANGE
                }
                _ => false,
            };
            if !healthy {
                close_trade(
                    &mut server,
                    &mut trade_manager,
                    acid,
                    "Trade cancelled — you are no longer together.",
                );
            }
        }

        // 4he. Track 13.2.b — apply split-stack intents. Splits part
        //      of one base stack into another slot (empty dst or
        //      merge same-path dst). Bag/equip locations defer.
        if !split_stack_intents.is_empty() {
            for intent in split_stack_intents.drain(..) {
                if intent.src_location != "base" || intent.dst_location != "base" {
                    tracing::debug!(
                        owner = intent.owner,
                        src_loc = %intent.src_location,
                        dst_loc = %intent.dst_location,
                        "SplitStack rejected — non-base locations not yet supported"
                    );
                    handlers::send_refusal(
                        &mut server,
                        intent.owner as ClientId,
                        "You can't split a stack there yet.",
                    );
                    continue;
                }
                let owner_cid = intent.owner as ClientId;
                // PD_W0028 escrow: an offered slot neither splits nor
                // receives a split.
                if trade_locked(&mut server, &trade_manager, owner_cid, "base", intent.src_slot)
                    || trade_locked(&mut server, &trade_manager, owner_cid, "base", intent.dst_slot)
                {
                    continue;
                }
                let Some(conn) = connections.get_mut(&owner_cid) else {
                    continue;
                };
                let src = intent.src_slot as usize;
                let dst = intent.dst_slot as usize;
                let touched = match conn.inventory.split_base(src, dst, intent.count) {
                    Ok(t) => t,
                    Err(e) => {
                        tracing::debug!(
                            owner = intent.owner,
                            src,
                            dst,
                            count = intent.count,
                            error = %e,
                            "SplitStack rejected"
                        );
                        handlers::send_refusal(&mut server, owner_cid, "That stack won't split like that.");
                        continue;
                    }
                };
                conn.inventory_dirty = true;
                let deltas: Vec<(u32, Option<(String, u32)>)> = touched
                    .iter()
                    .map(|&i| {
                        let payload = conn
                            .inventory
                            .base
                            .get(i)
                            .and_then(|s| s.as_ref())
                            .map(|e| (e.item_path.clone(), e.count));
                        (i as u32, payload)
                    })
                    .collect();
                for (slot, payload) in deltas {
                    let (item_path, count) = match payload {
                        Some((p, c)) => (Some(p), c),
                        None => (None, 0),
                    };
                    handlers::send_inventory_delta(
                        &mut server,
                        owner_cid,
                        "base".to_string(),
                        slot,
                        item_path,
                        count,
                    );
                }
                tracing::debug!(owner = intent.owner, src, dst, count = intent.count, "SplitStack applied");
            }
        }

        // 4hf. Track 13.2.b — apply drop-item intents. Removes
        //      `count` from `(base, slot)` (0 = whole stack) and
        //      spawns a server-owned LootBag at the player's pos
        //      via the existing pipeline. The bag is FFA — any
        //      player nearby can pick it up — matching the GDScript
        //      behaviour of "drop on ground".
        if !drop_item_intents.is_empty() {
            for intent in drop_item_intents.drain(..) {
                let owner_cid = intent.owner as ClientId;
                // PD_W0028 escrow: an offered slot cannot be dropped.
                if trade_locked(&mut server, &trade_manager, owner_cid, &intent.location, intent.slot) {
                    continue;
                }
                let drop_pos: Vec3f;
                let dropped: Option<(String, u32)>;
                if let Some(conn) = connections.get_mut(&owner_cid) {
                    drop_pos = conn.pos;
                    // Bag-aware removal (base or bag_<i>); mirrors the
                    // DestroyItem apply path. Rejects a non-empty
                    // bag-typed slot (the bag must be emptied first).
                    dropped = match conn
                        .inventory
                        .destroy_at(&intent.location, intent.slot, intent.count)
                    {
                        Ok(d) => {
                            conn.inventory_dirty = true;
                            Some(d)
                        }
                        Err(e) => {
                            tracing::debug!(
                                owner = intent.owner,
                                loc = %intent.location,
                                slot = intent.slot,
                                error = %e,
                                "DropItem rejected"
                            );
                            None
                        }
                    };
                } else {
                    handlers::send_refusal(&mut server, owner_cid, "You can't drop that.");
                    continue;
                }
                let Some((item_path, count)) = dropped else {
                    continue;
                };
                // InventoryDelta for the source slot — reflect the new
                // state (residual count or empty). peek_at speaks every
                // droppable location: base, bag_<i>, and the cursor,
                // whose residual a partial drop must not zero out.
                let post = connections
                    .get(&owner_cid)
                    .and_then(|c| c.inventory.peek_at(&intent.location, intent.slot));
                let (delta_path, delta_count) = match post {
                    Some((p, c)) => (Some(p), c),
                    None => (None, 0),
                };
                handlers::send_inventory_delta(
                    &mut server,
                    owner_cid,
                    intent.location.clone(),
                    intent.slot,
                    delta_path,
                    delta_count,
                );
                // Spawn a single-stack LootBag at the player's feet
                // and fan via the existing AOI-filtered loot pipeline.
                // Dropped items are public (no kill-owner) — anyone in
                // range may pick them up, as before.
                let bag = loot::LootBag::new(
                    drop_pos,
                    vec![loot::LootItemStack { item_path: item_path.clone(), count }],
                    protocol::world::Coins::ZERO,
                    String::new(), // player-dropped public bag — no creature, keep the sack visual
                    None,
                    now,
                );
                let bag_id = bag.id;
                let bag_cell = aoi::cell_for(bag.pos.x, bag.pos.z);
                aoi.insert(bag_id, bag_cell);
                let visible = aoi.entities_visible_from(bag_cell);
                let bag_recipients: Vec<ClientId> = in_world_recipients_now
                    .iter()
                    .copied()
                    .filter(|id| visible.contains(id))
                    .collect();
                if !bag_recipients.is_empty() {
                    handlers::fan_out_loot_bag_spawn(
                        &mut server,
                        &bag_recipients,
                        &bag,
                    );
                }
                loot_bags.insert(bag_id, bag);
                tracing::info!(
                    owner = intent.owner,
                    slot = intent.slot,
                    %item_path,
                    count,
                    bag_id,
                    "DropItem spawned loot bag"
                );
            }
        }

        // 4hg. Track 13.3 / 14.1 / 14.2 / 15.1 — apply equip-item
        //      intents. Validates src location (base or bag_<i>) +
        //      slot in range + equip_slot in range; item type vs
        //      slot validation lives inside `equip_from_location`;
        //      moves the entry; fans one `InventoryDelta` per
        //      touched slot. Track 14.2 then recomputes the
        //      connection's max HP / MP / stamina / armor from the
        //      registry's stat affixes and fans resource updates on
        //      max change.
        if !equip_item_intents.is_empty() {
            for intent in equip_item_intents.drain(..) {
                let owner_cid = intent.owner as ClientId;
                // PD_W0028 escrow: an offered slot cannot be equipped from.
                if trade_locked(&mut server, &trade_manager, owner_cid, &intent.src_location, intent.src_slot) {
                    continue;
                }
                let Some(conn) = connections.get_mut(&owner_cid) else {
                    continue;
                };
                let touched = match conn
                    .inventory
                    .equip_from_location(&intent.src_location, intent.src_slot, intent.equip_slot)
                {
                    Ok(t) => t,
                    Err(e) => {
                        tracing::info!(
                            owner = intent.owner,
                            src_loc = %intent.src_location,
                            src_slot = intent.src_slot,
                            equip_slot = intent.equip_slot,
                            error = %e,
                            "EquipItem rejected"
                        );
                        handlers::send_refusal(&mut server, owner_cid, "You can't equip that there.");
                        continue;
                    }
                };
                conn.inventory_dirty = true;
                let recompute = inventory::recompute_equipped_stats(conn);
                let deltas: Vec<(String, u32, Option<(String, u32)>)> = touched
                    .iter()
                    .map(|(loc, slot)| {
                        // peek_at speaks base / bag_<i> / cursor; only the
                        // paperdoll needs its own read here. The old
                        // hand-rolled chain fanned an EMPTY cursor delta on
                        // an equip-from-hand SWAP (playtest 2026-09-09): the
                        // client's hand went blank while the server still
                        // held the swapped-out item, and only a lift-click
                        // swap recovered it.
                        let payload = if loc == "equip" {
                            conn.inventory
                                .equipment
                                .get(&(*slot as u8))
                                .map(|e| (e.item_path.clone(), e.count))
                        } else {
                            conn.inventory.peek_at(loc, *slot)
                        };
                        (loc.clone(), *slot, payload)
                    })
                    .collect();
                for (loc, slot, payload) in deltas {
                    let (item_path, count) = match payload {
                        Some((p, c)) => (Some(p), c),
                        None => (None, 0),
                    };
                    handlers::send_inventory_delta(
                        &mut server,
                        owner_cid,
                        loc,
                        slot,
                        item_path,
                        count,
                    );
                }
                if recompute.any_resource_max_changed() {
                    handlers::fan_out_resources(
                        &mut server,
                        &in_world_recipients_now,
                        conn,
                    );
                }
                tracing::info!(
                    owner = intent.owner,
                    src_loc = %intent.src_location,
                    src_slot = intent.src_slot,
                    equip_slot = intent.equip_slot,
                    max_hp = conn.max_hp,
                    max_mp = conn.max_mp,
                    armor = conn.equipped_armor,
                    "EquipItem applied"
                );
            }
        }

        // 4hh. Track 13.3 / 14.2 — apply unequip-item intents.
        //      Mirror of 4hg in the other direction. Track 14.2
        //      runs the same stat recompute + resource fan after
        //      the mutation lands.
        if !unequip_item_intents.is_empty() {
            for intent in unequip_item_intents.drain(..) {
                if intent.dst_location != "base" && intent.dst_location != "cursor" {
                    tracing::debug!(
                        owner = intent.owner,
                        dst_loc = %intent.dst_location,
                        "UnequipItem rejected — dst must be 'base' or 'cursor'"
                    );
                    handlers::send_refusal(
                        &mut server,
                        intent.owner as ClientId,
                        "There's nowhere to put that.",
                    );
                    continue;
                }
                let owner_cid = intent.owner as ClientId;
                // PD_W0028 escrow: an unequip cannot land in (or swap with)
                // an offered slot.
                if intent.dst_location == "base"
                    && trade_locked(&mut server, &trade_manager, owner_cid, "base", intent.dst_slot)
                {
                    continue;
                }
                let Some(conn) = connections.get_mut(&owner_cid) else {
                    continue;
                };
                let dst = intent.dst_slot as usize;
                // PD_W0027 slice 1.5 — "cursor" lifts the worn item into the
                // hand; "base" is the classic unequip-to-slot.
                let result = if intent.dst_location == "cursor" {
                    conn.inventory.unequip_to_cursor(intent.equip_slot)
                } else {
                    conn.inventory.unequip_to_base(intent.equip_slot, dst)
                };
                let touched = match result {
                    Ok(t) => t,
                    Err(e) => {
                        tracing::info!(
                            owner = intent.owner,
                            equip_slot = intent.equip_slot,
                            dst_loc = %intent.dst_location,
                            dst,
                            error = %e,
                            "UnequipItem rejected"
                        );
                        let text = if e == "cursor occupied" {
                            "You're already holding something."
                        } else {
                            "You can't take that off right now."
                        };
                        handlers::send_refusal(&mut server, owner_cid, text);
                        continue;
                    }
                };
                conn.inventory_dirty = true;
                let recompute = inventory::recompute_equipped_stats(conn);
                let deltas: Vec<(String, u32, Option<(String, u32)>)> = touched
                    .iter()
                    .map(|&(loc, slot)| {
                        let payload = match loc {
                            "base" => conn
                                .inventory
                                .base
                                .get(slot as usize)
                                .and_then(|s| s.as_ref())
                                .map(|e| (e.item_path.clone(), e.count)),
                            "equip" => conn
                                .inventory
                                .equipment
                                .get(&(slot as u8))
                                .map(|e| (e.item_path.clone(), e.count)),
                            "cursor" => conn
                                .inventory
                                .cursor
                                .as_ref()
                                .map(|e| (e.item_path.clone(), e.count)),
                            _ => None,
                        };
                        (loc.to_string(), slot, payload)
                    })
                    .collect();
                for (loc, slot, payload) in deltas {
                    let (item_path, count) = match payload {
                        Some((p, c)) => (Some(p), c),
                        None => (None, 0),
                    };
                    handlers::send_inventory_delta(
                        &mut server,
                        owner_cid,
                        loc,
                        slot,
                        item_path,
                        count,
                    );
                }
                if recompute.any_resource_max_changed() {
                    handlers::fan_out_resources(
                        &mut server,
                        &in_world_recipients_now,
                        conn,
                    );
                }
                tracing::info!(
                    owner = intent.owner,
                    equip_slot = intent.equip_slot,
                    dst,
                    max_hp = conn.max_hp,
                    max_mp = conn.max_mp,
                    armor = conn.equipped_armor,
                    "UnequipItem applied"
                );
            }
        }

        // 4hh-bis. Track 15.1 — apply destroy-item intents. Decrements
        //      `count` from `(location, slot)` (0 = whole stack) and
        //      fans a single `InventoryDelta` for the touched slot.
        //      Unlike `DropItem` this does NOT spawn a loot bag; the
        //      item is gone for good. Used by trash cell + Destroy
        //      button UI in launcher mode.
        if !destroy_item_intents.is_empty() {
            for intent in destroy_item_intents.drain(..) {
                let owner_cid = intent.owner as ClientId;
                // PD_W0028 escrow: an offered slot cannot be destroyed.
                if trade_locked(&mut server, &trade_manager, owner_cid, &intent.location, intent.slot) {
                    continue;
                }
                let Some(conn) = connections.get_mut(&owner_cid) else {
                    continue;
                };
                let destroyed = match conn
                    .inventory
                    .destroy_at(&intent.location, intent.slot, intent.count)
                {
                    Ok(d) => d,
                    Err(e) => {
                        tracing::debug!(
                            owner = intent.owner,
                            loc = %intent.location,
                            slot = intent.slot,
                            error = %e,
                            "DestroyItem rejected"
                        );
                        handlers::send_refusal(&mut server, owner_cid, "You can't destroy that.");
                        continue;
                    }
                };
                conn.inventory_dirty = true;
                let (item_path, count) = destroyed;
                let payload = conn.inventory.peek_at(&intent.location, intent.slot);
                let (delta_path, delta_count) = match payload {
                    Some((p, c)) => (Some(p), c),
                    None => (None, 0),
                };
                handlers::send_inventory_delta(
                    &mut server,
                    owner_cid,
                    intent.location.clone(),
                    intent.slot,
                    delta_path,
                    delta_count,
                );
                tracing::info!(
                    owner = intent.owner,
                    loc = %intent.location,
                    slot = intent.slot,
                    %item_path,
                    count,
                    "DestroyItem applied"
                );
            }
        }

        // 4hh-tris. Track 15.2 — apply use-consumable intents. Looks
        //      up the item, validates it's consumable, decrements one
        //      from the stack, fans `InventoryDelta`, applies the
        //      effect (heal-on-use / food / drink) via the buff
        //      pipeline, fans `HealthUpdate` / `ManaUpdate` /
        //      `BuffSnapshot` as needed.
        if !use_consumable_intents.is_empty() {
            for intent in use_consumable_intents.drain(..) {
                let owner_cid = intent.owner as ClientId;
                // PD_W0028 escrow: an offered slot cannot be eaten from.
                if trade_locked(&mut server, &trade_manager, owner_cid, &intent.location, intent.slot) {
                    continue;
                }
                let Some(conn) = connections.get_mut(&owner_cid) else {
                    continue;
                };
                // Peek the item so we know what effect to apply
                // BEFORE the decrement removes it from the slot.
                let peek_path: Option<String> = conn
                    .inventory
                    .peek_at(&intent.location, intent.slot)
                    .map(|(p, _)| p);
                let Some(item_path) = peek_path else {
                    tracing::debug!(
                        owner = intent.owner,
                        loc = %intent.location,
                        slot = intent.slot,
                        "UseConsumable rejected — slot empty"
                    );
                    handlers::send_refusal(&mut server, owner_cid, "There's nothing there to use.");
                    continue;
                };
                let Some(item) = items::lookup(&item_path) else {
                    tracing::debug!(
                        owner = intent.owner,
                        %item_path,
                        "UseConsumable rejected — unknown item path"
                    );
                    handlers::send_refusal(&mut server, owner_cid, "You can't use that.");
                    continue;
                };
                let is_heal_potion = item.heal_on_use > 0.0 || item.mp_on_use > 0.0;
                if !item.is_food && !item.is_drink && !is_heal_potion {
                    tracing::debug!(
                        owner = intent.owner,
                        %item_path,
                        "UseConsumable rejected — item is not consumable"
                    );
                    handlers::send_refusal(&mut server, owner_cid, "You can't use that.");
                    continue;
                }
                // Track 15.2 — match the client's "already eating /
                // drinking" rule: a same-kind food/drink buff already
                // present rejects without consuming the stack.
                if item.is_food
                    && conn
                        .active_buffs
                        .iter()
                        .any(|b| b.name.starts_with("Food: "))
                {
                    tracing::debug!(owner = intent.owner, "UseConsumable rejected — already eating");
                    handlers::send_refusal(&mut server, owner_cid, "You're already eating something.");
                    continue;
                }
                if item.is_drink
                    && conn
                        .active_buffs
                        .iter()
                        .any(|b| b.name.starts_with("Drink: "))
                {
                    tracing::debug!(owner = intent.owner, "UseConsumable rejected — already drinking");
                    handlers::send_refusal(&mut server, owner_cid, "You're already drinking something.");
                    continue;
                }
                // Decrement-and-fan first so the UI loses the slot
                // immediately; the heal / buff effect lands right
                // after. Snapshot item fields we'll need post-decrement
                // (since `items::lookup` borrows the registry, not
                // `conn`, this is just convenience).
                let item_name = item.name.clone();
                let food_hp = item.food_hp_regen;
                let food_mp = item.food_mp_regen;
                let food_dur = item.food_duration;
                let heal_amt = item.heal_on_use;
                let mp_amt = item.mp_on_use;
                let is_food = item.is_food;
                let is_drink = item.is_drink;
                let _ = match conn.inventory.decrement_at(&intent.location, intent.slot) {
                    Ok(p) => p,
                    Err(e) => {
                        tracing::debug!(
                            owner = intent.owner,
                            loc = %intent.location,
                            slot = intent.slot,
                            error = %e,
                            "UseConsumable rejected — decrement failed"
                        );
                        handlers::send_refusal(&mut server, owner_cid, "You can't use that right now.");
                        continue;
                    }
                };
                conn.inventory_dirty = true;
                // Inventory Delta for the touched slot.
                let payload = conn.inventory.peek_at(&intent.location, intent.slot);
                let (delta_path, delta_count) = match payload {
                    Some((p, c)) => (Some(p), c),
                    None => (None, 0),
                };
                handlers::send_inventory_delta(
                    &mut server,
                    owner_cid,
                    intent.location.clone(),
                    intent.slot,
                    delta_path,
                    delta_count,
                );
                // Apply the effect.
                let mut buffs_changed = false;
                if heal_amt > 0.0 {
                    let before = conn.hp;
                    conn.hp = (conn.hp + heal_amt).min(conn.max_hp);
                    if conn.hp != before {
                        handlers::fan_out_health_update(
                            &mut server,
                            &in_world_recipients_now,
                            conn.char_id as u64,
                            conn.hp,
                            conn.max_hp,
                        );
                    }
                }
                if mp_amt > 0.0 {
                    let before = conn.mp;
                    conn.mp = (conn.mp + mp_amt).min(conn.max_mp);
                    if conn.mp != before {
                        handlers::fan_out_mana_update(
                            &mut server,
                            &in_world_recipients_now,
                            conn.char_id as u64,
                            conn.mp,
                            conn.max_mp,
                        );
                    }
                }
                if (is_food || is_drink) && food_dur > 0.0 {
                    // Mirror the client's BuffManager naming so the
                    // client-side bar shows "Food: <name>" / "Drink:
                    // <name>" after the BuffSnapshot lands.
                    let prefix = if is_food { "Food" } else { "Drink" };
                    if food_hp > 0.0 {
                        let name = format!("{prefix}: {item_name}");
                        apply_buff(
                            conn,
                            buffs::ActiveBuff::new_hot(name, food_hp, food_dur, now),
                        );
                        buffs_changed = true;
                    }
                    if food_mp > 0.0 {
                        // Distinct name so the MP-regen leg doesn't
                        // collide with the HP-regen entry under the
                        // same key (apply_buff is keyed by name).
                        let name = format!("{prefix} MP: {item_name}");
                        apply_buff(
                            conn,
                            buffs::ActiveBuff::new_mp_regen(name, food_mp, food_dur, now),
                        );
                        buffs_changed = true;
                    }
                }
                if buffs_changed {
                    fan_out_server_buff_snapshot(
                        &mut server,
                        &in_world_recipients_now,
                        conn,
                    );
                }
                tracing::info!(
                    owner = intent.owner,
                    loc = %intent.location,
                    slot = intent.slot,
                    %item_path,
                    is_food,
                    is_drink,
                    heal_amt,
                    mp_amt,
                    "UseConsumable applied"
                );
            }
        }

        // 4hh-quad. Track 15.2 follow-up — apply GM /give intents.
        //      Server-side spawn of a stack into the player's
        //      inventory by item display name. Looks up via
        //      `items::lookup_by_name`; adds via `add_item_locating`
        //      (caps stacks at registry max_stack, spills into new
        //      slots); fans `InventoryDelta` per touched slot. Unknown
        //      names or full-inventory cases reject with an INFO log.
        if !gm_give_intents.is_empty() {
            for intent in gm_give_intents.drain(..) {
                let owner_cid = intent.owner as ClientId;
                let Some(conn) = connections.get_mut(&owner_cid) else {
                    continue;
                };
                let Some(item) = items::lookup_by_name(&intent.item_name) else {
                    tracing::info!(
                        owner = intent.owner,
                        item_name = %intent.item_name,
                        "GmGive rejected — unknown item name"
                    );
                    continue;
                };
                let item_path = item.path.clone();
                let (touched, leftover) =
                    match conn.inventory.add_item_locating(&item_path, intent.qty) {
                        Ok(t) => t,
                        Err(e) => {
                            tracing::info!(
                                owner = intent.owner,
                                item_name = %intent.item_name,
                                error = %e,
                                "GmGive rejected — add_item_locating error"
                            );
                            continue;
                        }
                    };
                let placed = intent.qty - leftover;
                if placed == 0 {
                    tracing::info!(
                        owner = intent.owner,
                        item_name = %intent.item_name,
                        "GmGive rejected — inventory full, no stack placed"
                    );
                    continue;
                }
                conn.inventory_dirty = true;
                let deltas: Vec<(String, u32, Option<(String, u32)>)> = touched
                    .iter()
                    .map(|(loc, slot)| (loc.clone(), *slot, conn.inventory.peek_at(loc, *slot)))
                    .collect();
                for (loc, slot, payload) in deltas {
                    let (dp, dc) = match payload {
                        Some((p, c)) => (Some(p), c),
                        None => (None, 0),
                    };
                    handlers::send_inventory_delta(&mut server, owner_cid, loc, slot, dp, dc);
                }
                tracing::info!(
                    owner = intent.owner,
                    item_name = %intent.item_name,
                    qty = placed,
                    leftover,
                    "GmGive applied"
                );
            }
        }

        // 4hi. Track 14 follow-up — apply vendor BuyItem intents.
        //      Server validates: item exists, player has enough coins,
        //      inventory has room. On success: deducts coins, grants
        //      via add_item_locating, fans CoinsUpdate + one
        //      InventoryDelta per touched slot. Stock-by-vendor
        //      validation is deferred until server NPCs land (the
        //      `vendor_id` field is informational for now).
        if !buy_item_intents.is_empty() {
            for intent in buy_item_intents.drain(..) {
                let owner_cid = intent.owner as ClientId;
                let Some(conn) = connections.get_mut(&owner_cid) else {
                    continue;
                };
                // Proximity gate (dead-intents audit, 2026-08-24): a merchant
                // transaction requires an actual merchant nearby. Without this,
                // a modified client could trade from anywhere; the honest
                // client has always had a 6 m UI gate, so only remote abuse is
                // refused here. `vendor_id` remains informational until the
                // client sends real ids (per-vendor stocking is a later step).
                if super::npcs::any_within_range("vendor", conn.pos).is_none() {
                    tracing::info!(
                        owner = intent.owner,
                        pos = ?conn.pos,
                        "BuyItem rejected — no vendor within range"
                    );
                    handlers::send_refusal(&mut server, owner_cid, "There is no merchant near you.");
                    continue;
                }
                let Some(item) = items::lookup_by_name(&intent.item_name) else {
                    tracing::info!(
                        owner = intent.owner,
                        item_name = %intent.item_name,
                        "BuyItem rejected — unknown item name"
                    );
                    handlers::send_refusal(
                        &mut server, owner_cid, "The merchant doesn't stock that.",
                    );
                    continue;
                };
                let unit_price = item.vendor_price as i64;
                if unit_price <= 0 {
                    tracing::info!(
                        owner = intent.owner,
                        item_name = %intent.item_name,
                        "BuyItem rejected — item has no vendor_price"
                    );
                    handlers::send_refusal(
                        &mut server, owner_cid, "That isn't for sale.",
                    );
                    continue;
                }
                let total_cost = unit_price.saturating_mul(intent.qty as i64);
                if !conn.coins.can_afford(total_cost) {
                    tracing::info!(
                        owner = intent.owner,
                        item_name = %intent.item_name,
                        coins = conn.coins.total_copper(),
                        cost = total_cost,
                        "BuyItem rejected — insufficient coins"
                    );
                    handlers::send_refusal(
                        &mut server, owner_cid, "You can't afford that.",
                    );
                    continue;
                }
                let item_path = item.path.clone();
                let (touched, leftover) =
                    match conn.inventory.add_item_locating(&item_path, intent.qty) {
                        Ok(t) => t,
                        Err(e) => {
                            tracing::info!(
                                owner = intent.owner,
                                item_name = %intent.item_name,
                                error = %e,
                                "BuyItem rejected — add_item_locating error"
                            );
                            handlers::send_refusal(
                                &mut server, owner_cid, "You can't carry that.",
                            );
                            continue;
                        }
                    };
                let placed = intent.qty - leftover;
                if placed == 0 {
                    tracing::info!(
                        owner = intent.owner,
                        item_name = %intent.item_name,
                        "BuyItem rejected — inventory full, no stack placed"
                    );
                    // The case a tester hit on 2026-08-21: the client had already
                    // said "Ordered", so with no reply the item looked like it
                    // vanished. Nothing was charged.
                    handlers::send_refusal(
                        &mut server, owner_cid, "Your bags are full.",
                    );
                    continue;
                }
                // Charge only for what we actually placed. The pre-flight
                // can_afford check above used total_cost ≥ actual_cost, so this
                // spend always succeeds; make-change handles tier breaking.
                let actual_cost = unit_price.saturating_mul(placed as i64);
                conn.coins.spend(actual_cost);
                conn.coins_dirty = true;
                conn.inventory_dirty = true;
                let coins_after = conn.coins;
                let deltas: Vec<(String, u32, Option<(String, u32)>)> = touched
                    .iter()
                    .map(|(loc, slot)| (loc.clone(), *slot, conn.inventory.peek_at(loc, *slot)))
                    .collect();
                for (loc, slot, payload) in deltas {
                    let (dp, dc) = match payload {
                        Some((p, c)) => (Some(p), c),
                        None => (None, 0),
                    };
                    handlers::send_inventory_delta(&mut server, owner_cid, loc, slot, dp, dc);
                }
                handlers::send_coins_update(&mut server, owner_cid, coins_after);
                if leftover > 0 {
                    tracing::info!(
                        owner = intent.owner,
                        item_name = %intent.item_name,
                        qty_requested = intent.qty,
                        qty_placed = placed,
                        leftover,
                        "BuyItem partially filled — inventory ran out of room"
                    );
                    // Billing is already correct — only `placed` is charged — but
                    // asking for ten and receiving six with no word said reads as
                    // the game losing four of them.
                    handlers::send_refusal(
                        &mut server,
                        owner_cid,
                        &format!("Only {placed} fit in your bags."),
                    );
                }
                tracing::info!(
                    owner = intent.owner,
                    item_name = %intent.item_name,
                    qty = placed,
                    cost = actual_cost,
                    coins_after = coins_after.total_copper(),
                    "BuyItem applied"
                );
            }
        }

        // 4hj. Track 14 follow-up — apply vendor SellItem intents.
        //      Server: looks up the slot, validates item count, computes
        //      sell price (vendor_price / 2 per unit, mirroring the
        //      GDScript), credits coins, removes the sold stack (base
        //      or bag-inner slot), fans CoinsUpdate + InventoryDelta.
        //      Equip-slot sells reject (player should unequip first).
        if !sell_item_intents.is_empty() {
            for intent in sell_item_intents.drain(..) {
                let owner_cid = intent.owner as ClientId;
                // PD_W0028 escrow: an offered slot cannot be sold.
                let (lock_loc, lock_slot) = match intent.slot {
                    protocol::world::SlotRef::BaseSlot { idx } => ("base".to_string(), idx as u32),
                    protocol::world::SlotRef::BagSlot { base, slot } => {
                        (format!("bag_{base}"), slot as u32)
                    }
                    protocol::world::SlotRef::EquipSlot(_) => (String::new(), 0),
                };
                if !lock_loc.is_empty()
                    && trade_locked(&mut server, &trade_manager, owner_cid, &lock_loc, lock_slot)
                {
                    continue;
                }
                let Some(conn) = connections.get_mut(&owner_cid) else {
                    continue;
                };
                // Same proximity gate as BuyItem above.
                if super::npcs::any_within_range("vendor", conn.pos).is_none() {
                    tracing::info!(
                        owner = intent.owner,
                        pos = ?conn.pos,
                        "SellItem rejected — no vendor within range"
                    );
                    handlers::send_refusal(&mut server, owner_cid, "There is no merchant near you.");
                    continue;
                }
                // Resolve the slot reference + read the held item
                // path/count without mutating yet so we can compute
                // the price first.
                let (location_str, slot_u32, item_path, available_count) = match intent.slot {
                    protocol::world::SlotRef::BaseSlot { idx } => {
                        let i = idx as usize;
                        if i >= inventory::BASE_SLOT_COUNT {
                            continue;
                        }
                        let Some(entry) = conn.inventory.base.get(i).and_then(|s| s.as_ref())
                        else {
                            tracing::info!(
                                owner = intent.owner,
                                "SellItem rejected — base slot empty"
                            );
                            handlers::send_refusal(&mut server, owner_cid, "There's nothing in that slot to sell.");
                            continue;
                        };
                        (
                            "base".to_string(),
                            i as u32,
                            entry.item_path.clone(),
                            entry.count,
                        )
                    }
                    protocol::world::SlotRef::BagSlot { base, slot } => {
                        // A held bag's contents (bag_255) are not sellable —
                        // no window is open on it, so the intent is forged.
                        if base == inventory::CURSOR_BAG_KEY {
                            continue;
                        }
                        let Some(arr) = conn.inventory.bags.get(&base) else {
                            tracing::info!(
                                owner = intent.owner,
                                "SellItem rejected — bag slot has no bag at base"
                            );
                            handlers::send_refusal(&mut server, owner_cid, "There's nothing in that slot to sell.");
                            continue;
                        };
                        let Some(entry) = arr.get(slot as usize).and_then(|s| s.as_ref())
                        else {
                            tracing::info!(
                                owner = intent.owner,
                                "SellItem rejected — bag slot empty"
                            );
                            handlers::send_refusal(&mut server, owner_cid, "There's nothing in that slot to sell.");
                            continue;
                        };
                        (
                            format!("bag_{base}"),
                            slot as u32,
                            entry.item_path.clone(),
                            entry.count,
                        )
                    }
                    protocol::world::SlotRef::EquipSlot(_) => {
                        tracing::info!(
                            owner = intent.owner,
                            "SellItem rejected — equip slot sells not supported"
                        );
                        handlers::send_refusal(&mut server, owner_cid, "Take it off before selling it.");
                        continue;
                    }
                };
                let Some(item) = items::lookup(&item_path) else {
                    tracing::info!(
                        owner = intent.owner,
                        item_path = %item_path,
                        "SellItem rejected — unknown item path"
                    );
                    handlers::send_refusal(&mut server, owner_cid, "The merchant won't take that.");
                    continue;
                };
                let unit_price = (item.vendor_price as i64) / 2;
                if unit_price <= 0 {
                    tracing::info!(
                        owner = intent.owner,
                        item_path = %item_path,
                        "SellItem rejected — item has no sell value"
                    );
                    handlers::send_refusal(&mut server, owner_cid, "The merchant won't take that.");
                    continue;
                }
                let qty = intent.qty.min(available_count);
                let total_credit = unit_price.saturating_mul(qty as i64);
                // Refuse to sell a non-empty bag.
                if let protocol::world::SlotRef::BaseSlot { idx } = intent.slot {
                    if conn.inventory.bags.get(&idx).map_or(false, |arr| {
                        arr.iter().any(|s| s.is_some())
                    }) {
                        tracing::info!(
                            owner = intent.owner,
                            "SellItem rejected — bag has contents"
                        );
                        handlers::send_refusal(&mut server, owner_cid, "Empty the bag before selling it.");
                        continue;
                    }
                }
                // Mutate: subtract from the source slot.
                let new_count = match intent.slot {
                    protocol::world::SlotRef::BaseSlot { idx } => {
                        let entry = conn.inventory.base[idx as usize].as_mut().expect("checked");
                        entry.count -= qty;
                        let nc = entry.count;
                        if nc == 0 {
                            conn.inventory.base[idx as usize] = None;
                            conn.inventory.ensure_bag_init(idx as usize);
                        }
                        nc
                    }
                    protocol::world::SlotRef::BagSlot { base, slot } => {
                        let arr = conn.inventory.bags.get_mut(&base).expect("checked");
                        let entry = arr[slot as usize].as_mut().expect("checked");
                        entry.count -= qty;
                        let nc = entry.count;
                        if nc == 0 {
                            arr[slot as usize] = None;
                        }
                        nc
                    }
                    protocol::world::SlotRef::EquipSlot(_) => unreachable!(),
                };
                conn.coins.add_payout(total_credit);
                conn.coins_dirty = true;
                conn.inventory_dirty = true;
                let coins_after = conn.coins;
                let delta_item: Option<String> = if new_count > 0 {
                    Some(item_path.clone())
                } else {
                    None
                };
                handlers::send_inventory_delta(
                    &mut server,
                    owner_cid,
                    location_str,
                    slot_u32,
                    delta_item,
                    new_count,
                );
                handlers::send_coins_update(&mut server, owner_cid, coins_after);
                tracing::info!(
                    owner = intent.owner,
                    item_path = %item_path,
                    qty,
                    credit = total_credit,
                    coins_after = coins_after.total_copper(),
                    "SellItem applied"
                );
            }
        }

        // 4i-bank. PD_W0015 — Banker, slice 1 (coins). Deposit / withdraw move
        //          per-tier amounts between the carried wallet and the
        //          zero-weight bank balance; exchange converts tiers on the
        //          wallet. All validated against minting (non-negative,
        //          affordable), then fan CoinsUpdate (wallet) + BankSnapshot
        //          (bank) to the actor; failures fan a private BankRejected.
        for intent in bank_deposit_intents.drain(..) {
            let cid = intent.owner as ClientId;
            let Some(conn) = connections.get_mut(&cid) else { continue };
            // Proximity gate (dead-intents audit, 2026-08-24): banking requires
            // the banker. This is the arm that voided the death penalty — a
            // modified client could BankStoreItem its whole inventory from a
            // dungeon one second before dying, leaving the corpse empty.
            if super::npcs::any_within_range("banker", conn.pos).is_none() {
                tracing::info!(owner = intent.owner, pos = ?conn.pos, "bank op rejected — no banker within range");
                handlers::send_bank_rejected(&mut server, cid, "There is no banker near you.".to_string());
                continue;
            }
            if intent.coins.has_negative() || intent.coins == protocol::world::Coins::ZERO {
                handlers::send_bank_rejected(&mut server, cid, "Nothing to deposit.".to_string());
                continue;
            }
            // Pay by VALUE, not tier by tier. Depositing 5 silver from a purse
            // of 10 gold leaves 9 gold 95 silver, and depositing 1 gold from a
            // purse of 100 silver works too. Refusing either reads as a bug
            // rather than as the independent-stacks rule it actually is, and the
            // Banker already offers a free tier exchange, so refusing protected
            // nothing.
            let mut wallet_after = conn.coins;
            if !wallet_after.pay_value_of(intent.coins) {
                handlers::send_bank_rejected(
                    &mut server, cid, "You don't have that much coin to deposit.".to_string(),
                );
                continue;
            }
            conn.coins = wallet_after;
            conn.bank_coins = conn.bank_coins.add_each(intent.coins);
            conn.coins_dirty = true;
            conn.bank_dirty = true;
            let (wallet, bank) = (conn.coins, conn.bank_coins);
            handlers::send_coins_update(&mut server, cid, wallet);
            handlers::send_bank_snapshot(&mut server, cid, bank);
            tracing::info!(
                owner = intent.owner,
                deposited = intent.coins.total_copper(),
                bank = bank.total_copper(),
                "bank deposit"
            );
        }
        for intent in bank_withdraw_intents.drain(..) {
            let cid = intent.owner as ClientId;
            let Some(conn) = connections.get_mut(&cid) else { continue };
            // Same banker proximity gate as the deposit loop above.
            if super::npcs::any_within_range("banker", conn.pos).is_none() {
                tracing::info!(owner = intent.owner, pos = ?conn.pos, "bank op rejected — no banker within range");
                handlers::send_bank_rejected(&mut server, cid, "There is no banker near you.".to_string());
                continue;
            }
            if intent.coins.has_negative() || intent.coins == protocol::world::Coins::ZERO {
                handlers::send_bank_rejected(&mut server, cid, "Nothing to withdraw.".to_string());
                continue;
            }
            // Same rule on the way out, so the bank is symmetric: a balance is
            // worth what it is worth, whichever coins it happens to be in.
            let mut bank_after = conn.bank_coins;
            if !bank_after.pay_value_of(intent.coins) {
                handlers::send_bank_rejected(
                    &mut server, cid, "Your bank doesn't hold that much coin.".to_string(),
                );
                continue;
            }
            conn.bank_coins = bank_after;
            conn.coins = conn.coins.add_each(intent.coins);
            conn.coins_dirty = true;
            conn.bank_dirty = true;
            let (wallet, bank) = (conn.coins, conn.bank_coins);
            handlers::send_coins_update(&mut server, cid, wallet);
            handlers::send_bank_snapshot(&mut server, cid, bank);
            tracing::info!(
                owner = intent.owner,
                withdrew = intent.coins.total_copper(),
                bank = bank.total_copper(),
                "bank withdraw"
            );
        }
        for intent in bank_exchange_intents.drain(..) {
            let cid = intent.owner as ClientId;
            let Some(conn) = connections.get_mut(&cid) else { continue };
            // Same banker proximity gate as the deposit loop above.
            if super::npcs::any_within_range("banker", conn.pos).is_none() {
                tracing::info!(owner = intent.owner, pos = ?conn.pos, "bank op rejected — no banker within range");
                handlers::send_bank_rejected(&mut server, cid, "There is no banker near you.".to_string());
                continue;
            }
            match conn.coins.exchange(intent.from_tier, intent.to_tier, intent.qty) {
                Ok(()) => {
                    conn.coins_dirty = true;
                    let wallet = conn.coins;
                    handlers::send_coins_update(&mut server, cid, wallet);
                    tracing::info!(
                        owner = intent.owner,
                        from = intent.from_tier,
                        to = intent.to_tier,
                        qty = intent.qty,
                        "bank exchange"
                    );
                }
                Err(reason) => {
                    handlers::send_bank_rejected(&mut server, cid, reason.to_string());
                }
            }
        }

        // 4i-bank2. PD_W0016 — Banker, slice 2 (item vaults). Quick-transfer
        //           whole stacks between inventory and a vault. Store sizes the
        //           deposit against the vault's room first (capacity_for) so the
        //           inventory side moves exactly what fits and the source never
        //           relocates; bag-typed items are rejected (MVP). Withdraw
        //           takes the stack then refunds any inventory overflow back to
        //           the vault. Each op fans an InventoryDelta (inventory side) +
        //           a full BankItemSnapshot (the tiny vault).
        for intent in bank_store_item_intents.drain(..) {
            let cid = intent.owner as ClientId;
            // PD_W0028 escrow: an offered slot cannot be banked (this is
            // also the death-penalty-void slot, so the lock matters twice).
            if trade_locked(&mut server, &trade_manager, cid, &intent.src_location, intent.src_slot) {
                continue;
            }
            let Some(conn) = connections.get_mut(&cid) else { continue };
            // Same banker proximity gate as the deposit loop above.
            if super::npcs::any_within_range("banker", conn.pos).is_none() {
                tracing::info!(owner = intent.owner, pos = ?conn.pos, "bank op rejected — no banker within range");
                handlers::send_bank_rejected(&mut server, cid, "There is no banker near you.".to_string());
                continue;
            }
            let Some((path, avail)) =
                conn.inventory.peek_at(&intent.src_location, intent.src_slot)
            else {
                handlers::send_bank_rejected(&mut server, cid, "Nothing to deposit there.".to_string());
                continue;
            };
            if items::bag_num_slots(&path).is_some() {
                handlers::send_bank_rejected(&mut server, cid, "Bags can't go in the bank vault.".to_string());
                continue;
            }
            let room = if intent.shared {
                conn.account_bank_items.capacity_for(&path)
            } else {
                conn.bank_items.capacity_for(&path)
            };
            let take = avail.min(room);
            if take == 0 {
                handlers::send_bank_rejected(&mut server, cid, "Your bank vault is full.".to_string());
                continue;
            }
            // Remove from inventory FIRST and let the actual removed count drive
            // the deposit, so the inventory side is the single source of truth
            // for how much moved (the vault is never credited without debiting
            // inventory). destroy_at can't fail here today — peek_at vetted the
            // slot and bags are rejected above — but bailing on Err keeps a
            // future change from turning this into a dup.
            let removed = match conn.inventory.destroy_at(&intent.src_location, intent.src_slot, take) {
                Ok((_, removed)) => removed,
                Err(_) => {
                    handlers::send_bank_rejected(&mut server, cid, "Couldn't move that item.".to_string());
                    continue;
                }
            };
            if removed == 0 {
                continue;
            }
            conn.inventory_dirty = true;
            if intent.shared {
                conn.account_bank_items.deposit(&path, removed);
                conn.account_bank_items_dirty = true;
            } else {
                conn.bank_items.deposit(&path, removed);
                conn.bank_items_dirty = true;
            }
            let post = conn.inventory.peek_at(&intent.src_location, intent.src_slot);
            let (dpath, dcount) = match post {
                Some((p, c)) => (Some(p), c),
                None => (None, 0),
            };
            handlers::send_inventory_delta(
                &mut server, cid, intent.src_location.clone(), intent.src_slot, dpath, dcount,
            );
            let entries = if intent.shared {
                conn.account_bank_items.to_snapshot_entries()
            } else {
                conn.bank_items.to_snapshot_entries()
            };
            handlers::send_bank_item_snapshot(&mut server, cid, intent.shared, entries);
            tracing::info!(owner = intent.owner, %path, deposited = take, shared = intent.shared, "bank store item");
        }
        for intent in bank_withdraw_item_intents.drain(..) {
            let cid = intent.owner as ClientId;
            let Some(conn) = connections.get_mut(&cid) else { continue };
            // Same banker proximity gate as the deposit loop above.
            if super::npcs::any_within_range("banker", conn.pos).is_none() {
                tracing::info!(owner = intent.owner, pos = ?conn.pos, "bank op rejected — no banker within range");
                handlers::send_bank_rejected(&mut server, cid, "There is no banker near you.".to_string());
                continue;
            }
            let slot = intent.vault_slot as usize;
            let present = if intent.shared {
                conn.account_bank_items.peek(slot).cloned()
            } else {
                conn.bank_items.peek(slot).cloned()
            };
            let Some(entry) = present else {
                handlers::send_bank_rejected(&mut server, cid, "Nothing to withdraw there.".to_string());
                continue;
            };
            let path = entry.item_path;
            let count = entry.count;
            if intent.shared {
                conn.account_bank_items.take_all(slot);
                conn.account_bank_items_dirty = true;
            } else {
                conn.bank_items.take_all(slot);
                conn.bank_items_dirty = true;
            }
            let (touched, leftover) = conn
                .inventory
                .add_item_locating(&path, count)
                .unwrap_or((Vec::new(), count));
            if leftover > 0 {
                // Inventory had no room for all of it. Put the overflow straight
                // back into the slot it came from (take_all just emptied it),
                // not via deposit() — restore() guarantees it lands in the same
                // slot and never trips the max_stack cap, so nothing is lost even
                // for an oddly-sized stored stack.
                if intent.shared {
                    conn.account_bank_items.restore(slot, &path, leftover);
                } else {
                    conn.bank_items.restore(slot, &path, leftover);
                }
                let reason = if leftover == count {
                    "No room in your inventory."
                } else {
                    "Only part of that fit in your inventory."
                };
                handlers::send_bank_rejected(&mut server, cid, reason.to_string());
            }
            if !touched.is_empty() {
                conn.inventory_dirty = true;
            }
            for (loc, slot) in &touched {
                let (dp, dc) = match conn.inventory.peek_at(loc, *slot) {
                    Some((p, c)) => (Some(p), c),
                    None => (None, 0),
                };
                handlers::send_inventory_delta(&mut server, cid, loc.clone(), *slot, dp, dc);
            }
            let entries = if intent.shared {
                conn.account_bank_items.to_snapshot_entries()
            } else {
                conn.bank_items.to_snapshot_entries()
            };
            handlers::send_bank_item_snapshot(&mut server, cid, intent.shared, entries);
            tracing::info!(owner = intent.owner, %path, withdrew = count - leftover, shared = intent.shared, "bank withdraw item");
        }

        // 4i. Enemy AI tick. Each alive enemy evaluates its state machine
        //     against the snapshot of in_world player positions, advances
        //     its own pos / target / state, and yields events for the
        //     post-loop fan-out (target switch, melee swing). Position
        //     broadcasts ride the same step-6 fan-out as players.
        //
        //     Track 11 — pets share this loop. Before the per-entity
        //     tick, resolve each pet's inherited target from its owner's
        //     `last_attacked_enemy` (if fresh and the target is still a
        //     live enemy). Pets also need a snapshot of all live enemy
        //     positions so they can chase their target inside the
        //     per-entity borrow.
        if !enemies.is_empty() && !in_world_recipients_now.is_empty() {
            const PET_TARGET_DECAY_SECS: f32 = 10.0;
            // Dead players are NOT aggro-able. Enemies previously kept chasing and
            // hitting a corpse because this list filtered only on `in_world` —
            // note the pet-AI snapshot a few lines below always carried
            // `c.hp > 0.0`, so pets already knew and only the enemy AI did not.
            // Dropping the dead from the list makes an enemy lose its target the
            // moment you die, so it leashes home instead of beating your body
            // (playtest 2026-08-12).
            let player_snapshots: Vec<(EntityId, Vec3f)> = connections
                .values()
                .filter(|c| c.in_world && c.hp > 0.0)
                .map(|c| (c.char_id as u64, c.pos))
                .collect();
            // Track 11.4 — enemies aggro on pets too. Build a combined
            // targets slice (players + alive pets) so enemy tick_idle
            // can pick the nearest of either. Pets share melee
            // melee range / chase behaviour with players from the
            // enemy's POV; the new enemy→pet hit dispatch below
            // applies damage to pets.
            let mut targets_for_enemy_ai: Vec<(EntityId, Vec3f)> = player_snapshots.clone();
            for (id, entity) in enemies.iter() {
                if entity.is_pet() && entity.is_alive() {
                    targets_for_enemy_ai.push((*id, entity.pos));
                }
            }
            // Snapshot live enemies (mobs AND pets) plus players so pet
            // AI can read its target's position + alive-status without
            // re-borrowing the map inside the per-entity loop. Pets and
            // players were excluded pre-Round-4 because pet AI only
            // engaged NPC mobs; /pet attack on a peer player or peer's
            // pet now passes the PvP gate at command time, so the
            // snapshot has to cover all three target classes or the pet
            // just stands still (no target_info → falls back to follow).
            let mut enemy_target_snapshots: Vec<(EntityId, Vec3f, bool)> = enemies
                .iter()
                .map(|(id, e)| (*id, e.pos, e.is_alive()))
                .collect();
            for c in connections.values() {
                if c.in_world {
                    enemy_target_snapshots.push((c.char_id as u64, c.pos, c.hp > 0.0));
                }
            }
            // Pre-pass: drive each pet's target from its owner's last
            // attack. None if the inheritance has decayed or the
            // target is gone. Track 12 Piece A — pets with a fresh
            // `command_at` are excluded from re-inheritance: the
            // commanded target sticks until the window decays or the
            // owner explicitly issues another command.
            const PET_COMMAND_STICKY_SECS: f32 = 30.0;
            let pet_target_updates: Vec<(EntityId, Option<EntityId>)> = enemies
                .iter()
                .filter(|(_, e)| e.is_pet() && e.is_alive())
                // Track 15.3 — Guard / Sit pets never auto-inherit
                // from the owner's last attack. They only engage via
                // an explicit /pet attack or (for Guard) by being
                // attacked themselves.
                .filter(|(_, e)| e.stance == entity::PetStance::Follow)
                .filter(|(_, e)| {
                    // Skip pets under an active command.
                    e.command_at
                        .map(|t| {
                            now.duration_since(t).as_secs_f32() > PET_COMMAND_STICKY_SECS
                        })
                        .unwrap_or(true)
                })
                .map(|(pet_id, pet)| {
                    // Track 16.0 bug 4 — only inherit when the pet has
                    // no live target. The old pre-pass overrode every
                    // tick, so a stale owner attack (>10s) would zero
                    // out the pet's target mid-fight (pet wanders back
                    // to owner). Keep the current target whenever it's
                    // still alive.
                    let current_alive = pet.target.and_then(|t| {
                        enemy_target_snapshots
                            .iter()
                            .find(|(id, _, alive)| *id == t && *alive)
                            .map(|_| t)
                    });
                    if current_alive.is_some() {
                        return (*pet_id, current_alive);
                    }
                    let owner_id = pet.owner.expect("pet must have owner");
                    let owner_cid = owner_id as ClientId;
                    let owner = connections.get(&owner_cid);
                    let inherited = owner.and_then(|c| {
                        let attacked = c.last_attacked_enemy?;
                        let at = c.last_attacked_at?;
                        if now.duration_since(at).as_secs_f32() > PET_TARGET_DECAY_SECS {
                            return None;
                        }
                        // Confirm the target is still a live enemy.
                        let alive = enemy_target_snapshots
                            .iter()
                            .any(|(id, _, alive)| *id == attacked && *alive);
                        if alive { Some(attacked) } else { None }
                    });
                    (*pet_id, inherited)
                })
                .collect();
            for (pet_id, target) in pet_target_updates {
                if let Some(pet) = enemies.get_mut(&pet_id) {
                    pet.target = target;
                }
            }
            // Drop expired command stickiness so a stale Attack
            // command doesn't keep the pet pinned to a dead target
            // when the player just stops giving commands.
            //
            // Additionally, clear the sticky as soon as a Follow-stance
            // pet (no target) reaches its owner — the /pet back case.
            // Without this, the 30 s window blocks inheritance for
            // several seconds AFTER the pet has visibly arrived,
            // making the player feel like the pet "forgot" how to
            // engage. Round-5 playtest feedback: re-engage should kick
            // in the moment the pet is in close proximity to the
            // owner. Guard / Sit stances keep their sticky (those
            // stances *intentionally* suppress inheritance).
            const PET_BACK_HOME_DISTANCE: f32 = 4.0;
            for pet in enemies.values_mut() {
                if !pet.is_pet() { continue; }
                let Some(t) = pet.command_at else { continue; };
                if now.duration_since(t).as_secs_f32() > PET_COMMAND_STICKY_SECS {
                    pet.command_at = None;
                    continue;
                }
                if pet.target.is_some() {
                    continue;
                }
                if pet.stance != entity::PetStance::Follow {
                    continue;
                }
                let Some(owner_id) = pet.owner else { continue; };
                let Some(owner_pos) = connections
                    .get(&(owner_id as ClientId))
                    .filter(|c| c.in_world)
                    .map(|c| c.pos)
                else { continue; };
                if pet.pos.distance_to(owner_pos) <= PET_BACK_HOME_DISTANCE {
                    pet.command_at = None;
                }
            }
            let mut target_changes: Vec<(EntityId, Option<EntityId>)> = Vec::new();
            let mut enemy_hits: Vec<(EntityId, HitIntent)> = Vec::new();
            // Collect enemy and pet cell changes so the aoi grid stays
            // current (player cell-change fan-outs read it to find what is
            // now visible) AND so each crossing can fan its own spawn /
            // despawn to the players it brought into or out of view.
            let mut enemy_cell_changes: Vec<(EntityId, (i32, i32), (i32, i32))> = Vec::new();
            for entity in enemies.values_mut() {
                if !entity.is_alive() {
                    continue;
                }
                // Named-mob enrage. Checked here rather than at each damage
                // site because enemy HP is reduced in six separate places with
                // no shared helper, and this sweep runs downstream of all of
                // them in the same tick. One check, every damage source, plus
                // any added later.
                if let Some(named) = entity
                    .mob
                    .named_id
                    .as_deref()
                    .and_then(super::named::lookup)
                {
                    if entity.maybe_enrage(named) {
                        tracing::info!(
                            entity_id = entity.id,
                            mob = %entity.mob.name,
                            hp = entity.hp,
                            max_hp = entity.max_hp,
                            "named mob enraged"
                        );
                    }
                }
                let old_enemy_cell = aoi::cell_for(entity.pos.x, entity.pos.z);
                let events = entity.tick_ai(&targets_for_enemy_ai, &enemy_target_snapshots, dt, now);
                if let Some(new_target) = events.target_changed {
                    target_changes.push((entity.id, new_target));
                }
                if let Some(hit) = events.hit {
                    enemy_hits.push((entity.id, hit));
                }
                let new_enemy_cell = aoi::cell_for(entity.pos.x, entity.pos.z);
                if new_enemy_cell != old_enemy_cell {
                    enemy_cell_changes.push((entity.id, old_enemy_cell, new_enemy_cell));
                }
            }
            for (id, old_cell, new_cell) in enemy_cell_changes {
                let (gained, lost) = aoi.update(id, old_cell, new_cell);
                if let Some(entity) = enemies.get(&id) {
                    fan_entity_cell_crossing(&mut server, &aoi, &connections, entity, &gained, &lost);
                }
            }
            for (id, target) in target_changes {
                handlers::fan_out_entity_target(
                    &mut server,
                    &in_world_recipients_now,
                    id,
                    target,
                );
            }
            for (attacker, hit) in enemy_hits {
                // Track 11.4 — enemy → pet. Mirror of the enemy →
                // player branch below but simpler: pets have no armor /
                // absorb and no XP awarded on death. Just apply HP, fan
                // Hit + HealthUpdate, and let corpse-cleanup remove the
                // pet after the linger window.
                // Track 13: a pet carries its buffs here too — a Thorns
                // damage shield reflects onto the attacker below (mirrors
                // the enemy→player reflect; a reflect-kill drops the enemy
                // with no XP/loot, same as the player path).
                if attacker >= protocol::world::ENEMY_ID_BASE
                    && attacker < protocol::world::PET_ID_BASE
                    && hit.target >= protocol::world::PET_ID_BASE
                {
                    let amount = hit.amount.max(0);
                    // Track 13 — Thorns reflect: captured while the pet is
                    // borrowed, applied to the attacker after the borrow drops.
                    let mut pet_shield: f32 = 0.0;
                    let mut pet_shield_name: Option<String> = None;
                    if let Some(pet) = enemies.get_mut(&hit.target) {
                        if !pet.is_alive() {
                            continue;
                        }
                        pet.hp = (pet.hp - amount as f32).max(0.0);
                        let new_hp = pet.hp;
                        let max_hp = pet.max_hp;
                        let died = new_hp <= 0.0;
                        pet_shield = buffs::damage_shield_total(&pet.active_buffs);
                        pet_shield_name =
                            buffs::first_damage_shield_name(&pet.active_buffs).map(|s| s.to_string());
                        if died {
                            pet.transition(EnemyState::Dead, now);
                        }
                        // Track 15.3 — Guard pets retaliate on
                        // incoming damage when they don't already
                        // have a target. Follow stance gets its
                        // target via the owner-inheritance pre-pass
                        // (or by being commanded); Sit ignores
                        // incoming damage on purpose.
                        if !died
                            && pet.stance == entity::PetStance::Guard
                            && pet.target.is_none()
                        {
                            pet.target = Some(attacker);
                            pet.command_at = Some(now);
                        }
                        handlers::fan_out_hit(
                            &mut server,
                            &in_world_recipients_now,
                            attacker,
                            hit.target,
                            amount,
                            false,
                            protocol::world::DamageType::Physical,
                        );
                        handlers::fan_out_health_update(
                            &mut server,
                            &in_world_recipients_now,
                            hit.target,
                            new_hp,
                            max_hp,
                        );
                        if died {
                            // Track 12 Piece B — if this pet was a
                            // Beast Master warder, schedule its
                            // respawn on the owner. WARDER_RETREAT_SECS
                            // matches the GDScript WarderAI constant.
                            const WARDER_RETREAT_SECS: f32 = 15.0;
                            let respawn_owner: Option<(ClientId, std::time::Instant)> =
                                enemies.get(&hit.target).and_then(|p| {
                                    if super::pet_templates::is_warder_template(&p.mob.name) {
                                        let due = now
                                            + std::time::Duration::from_secs_f32(
                                                WARDER_RETREAT_SECS,
                                            );
                                        p.owner.map(|o| (o as ClientId, due))
                                    } else {
                                        None
                                    }
                                });
                            if let Some((cid, due)) = respawn_owner {
                                if let Some(conn) = connections.get_mut(&cid) {
                                    conn.warder_respawn_at = Some(due);
                                    tracing::info!(
                                        owner = cid as u64,
                                        retreat_secs = WARDER_RETREAT_SECS,
                                        "warder retreating; respawn scheduled",
                                    );
                                }
                            }
                            handlers::fan_out_entity_died(
                                &mut server,
                                &in_world_recipients_now,
                                hit.target,
                            );
                            tracing::info!(
                                pet_id = hit.target,
                                killer = attacker,
                                "pet killed by enemy"
                            );
                        }
                    }
                    // Track 13 — Thorns reflect on a pet: the enemy that
                    // struck a shielded pet takes the shield damage back.
                    // Mirrors the enemy→player reflect; a reflect-kill just
                    // drops the enemy (no XP/loot, same as the player path).
                    if pet_shield > 0.0 {
                        if let Some(att_entity) = enemies.get_mut(&attacker) {
                            if att_entity.is_alive() {
                                let dmg = pet_shield as i32;
                                att_entity.hp = (att_entity.hp - dmg as f32).max(0.0);
                                handlers::fan_out_health_update(
                                    &mut server,
                                    &in_world_recipients_now,
                                    attacker,
                                    att_entity.hp,
                                    att_entity.max_hp,
                                );
                                if let Some(name) = pet_shield_name.as_ref() {
                                    handlers::fan_out_damage_shield_trigger(
                                        &mut server,
                                        &in_world_recipients_now,
                                        hit.target,
                                        attacker,
                                        dmg,
                                        name.clone(),
                                    );
                                }
                                if att_entity.hp <= 0.0 {
                                    att_entity.transition(EnemyState::Dead, now);
                                    handlers::fan_out_entity_died(
                                        &mut server,
                                        &in_world_recipients_now,
                                        attacker,
                                    );
                                }
                            }
                        }
                    }
                    continue;
                }
                // Track 11.3 — pet swings hit enemies. Attacker id
                // identifies the source: pets are in the PET_ID_BASE
                // partition. For pet→enemy hits we apply damage to
                // the target enemy directly, fan Hit + HealthUpdate,
                // and handle death (kill credit + loot routed to the
                // pet's owner, not the pet itself, via the aggro map
                // we accumulate under the owner's id).
                if attacker >= protocol::world::PET_ID_BASE
                    && hit.target >= protocol::world::ENEMY_ID_BASE
                    && (hit.target < protocol::world::LOOT_BAG_ID_BASE
                        || hit.target >= protocol::world::PET_ID_BASE)
                {
                    let owner_id_opt = enemies
                        .get(&attacker)
                        .and_then(|p| p.owner);
                    let amount = hit.amount.max(0);
                    if let Some(target_entity) = enemies.get_mut(&hit.target) {
                        if !target_entity.is_alive() {
                            continue;
                        }
                        target_entity.hp = (target_entity.hp - amount as f32).max(0.0);
                        // Aggro credit under the owner so XP / top-
                        // damager logic naturally routes to them
                        // (pets don't level themselves).
                        if let Some(owner) = owner_id_opt {
                            *target_entity
                                .aggro
                                .entry(owner)
                                .or_insert(0.0) += amount as f32;
                        }
                        // Track 12 Piece A2 — threat accrues under
                        // the pet's own id so the enemy can pull
                        // onto a hard-hitting pet via the AI re-eval
                        // even when the owner has been damaging it
                        // longer.
                        *target_entity
                            .threat
                            .entry(attacker)
                            .or_insert(0.0) += amount as f32;
                        let new_hp = target_entity.hp;
                        let max_hp = target_entity.max_hp;
                        let died = new_hp <= 0.0;
                        // EQ quadratic per-kill award from the mob's level (see
                        // progression::kill_xp); pet kills credit the owner below.
                        let mob_xp = super::progression::kill_xp(target_entity.mob.level as i32, super::progression::ZEM_NORMAL);
                        let mob_level = target_entity.mob.level;
                        let mob_name_dead = if died {
                            target_entity.transition(EnemyState::Dead, now);
                            Some(target_entity.mob.name.clone())
                        } else {
                            None
                        };
                        // Owned victims (pets / charmed mobs) grant no quest
                        // credit — the warder is literally named "Wolf".
                        let victim_owned = target_entity.owner.is_some();
                        // Capture the dying entity's owner so a
                        // warder-template kill can schedule the
                        // owner's respawn after the borrow drops. None
                        // for NPC enemies; Some for pets.
                        let dead_pet_owner_opt: Option<EntityId> = if died {
                            target_entity.owner
                        } else {
                            None
                        };
                        let death_pos = target_entity.pos;
                        let credit_id_opt = if died {
                            target_entity
                                .aggro
                                .iter()
                                .max_by(|a, b| {
                                    a.1.partial_cmp(b.1)
                                        .unwrap_or(std::cmp::Ordering::Equal)
                                })
                                .map(|(&id, _)| id)
                        } else {
                            None
                        };
                        // dmg breaks mez on the victim, matching the
                        // single-target ENEMY arm semantics.
                        if amount > 0 {
                            target_entity.clear_mez();
                        }
                        handlers::fan_out_hit(
                            &mut server,
                            &in_world_recipients_now,
                            attacker,
                            hit.target,
                            amount,
                            false,
                            protocol::world::DamageType::Physical,
                        );
                        handlers::fan_out_health_update(
                            &mut server,
                            &in_world_recipients_now,
                            hit.target,
                            new_hp,
                            max_hp,
                        );
                        if died {
                            handlers::fan_out_entity_died(
                                &mut server,
                                &in_world_recipients_now,
                                hit.target,
                            );
                            // Warder respawn — if a pet kills another
                            // pet that's a Beast Master warder, schedule
                            // the same retreat-and-respawn the melee /
                            // spell paths use.
                            if let Some(mob_name) = mob_name_dead.as_ref() {
                                if super::pet_templates::is_warder_template(mob_name) {
                                    if let Some(owner_id) = dead_pet_owner_opt {
                                        const WARDER_RETREAT_SECS: f32 = 15.0;
                                        let due = now
                                            + std::time::Duration::from_secs_f32(WARDER_RETREAT_SECS);
                                        let owner_cid = owner_id as ClientId;
                                        if let Some(conn) = connections.get_mut(&owner_cid) {
                                            conn.warder_respawn_at = Some(due);
                                            tracing::info!(
                                                owner = owner_cid as u64,
                                                killer = attacker,
                                                retreat_secs = WARDER_RETREAT_SECS,
                                                "warder retreating after pet kill",
                                            );
                                        }
                                    }
                                }
                            }
                            if let Some(credit_id) = credit_id_opt {
                                // Pet kills credit the OWNER (aggro accrues
                                // under the owner's id), through the same
                                // group split + quest credit as melee/spell
                                // kills. Name from the dying mob; a live mob
                                // never reaches here with credit set.
                                if let Some(name) = mob_name_dead.as_ref() {
                                    award_kill(
                                        &mut server,
                                        &mut connections,
                                        &group_manager,
                                        credit_id,
                                        mob_xp,
                                        name,
                                        victim_owned,
                                        death_pos,
                                    );
                                }
                            }
                            if let Some(mob_name) = mob_name_dead.as_ref() {
                                let loot_items =
                                    loot::roll_for_mob(mob_name).unwrap_or_default();
                                let loot_coins =
                                    loot::roll_coin_for_mob(mob_name, mob_level);
                                if !loot_items.is_empty()
                                    || loot_coins != protocol::world::Coins::ZERO
                                {
                                    // Pet kills credit the pet's player owner;
                                    // that player (and group) owns the corpse.
                                    let owner_cid_opt = credit_id_opt
                                        .filter(|&id| id < protocol::world::ENEMY_ID_BASE)
                                        .map(|id| id as ClientId);
                                    let bag = LootBag::new(
                                        death_pos,
                                        loot_items,
                                        loot_coins,
                                        mob_name.clone(),
                                        owner_cid_opt,
                                        now,
                                    );
                                    let bag_id = bag.id;
                                    let bag_cell = aoi::cell_for(bag.pos.x, bag.pos.z);
                                    aoi.insert(bag_id, bag_cell);
                                    let visible = aoi.entities_visible_from(bag_cell);
                                    let bag_recipients: Vec<ClientId> = in_world_recipients_now
                                        .iter()
                                        .copied()
                                        .filter(|id| visible.contains(id))
                                        .collect();
                                    if !bag_recipients.is_empty() {
                                        handlers::fan_out_loot_bag_spawn(
                                            &mut server,
                                            &bag_recipients,
                                            &bag,
                                        );
                                    }
                                    loot_bags.insert(bag.id, bag);
                                    tracing::info!(
                                        mob = %mob_name,
                                        bag_id,
                                        "loot bag spawned (pet kill)"
                                    );
                                }
                            }
                        }
                    }
                    continue;
                }
                // Track 6: apply HP delta server-side when the enemy's
                // target is a player. The target_id space encodes
                // players below ENEMY_ID_BASE — anything in that range
                // is a char_id we can look up directly. Bigger ids
                // (other enemies, loot bags) shouldn't happen here
                // (enemy AI never targets non-players) but the
                // partition guards against it. Sub-task 3 applies the
                // same armor reduction the player-target branch uses.
                // The fan_out_hit amount tracks the post-reduction
                // value so the floating number matches the bar drop.
                let mut damaged_player: Option<u64> = None;
                let mut shield_to_attacker: f32 = 0.0;
                let mut shield_name_pve: Option<String> = None;
                let mut absorb_buff_to_strip: Option<usize> = None;
                let final_amount = if hit.target < protocol::world::ENEMY_ID_BASE {
                    let target_cid = hit.target as ClientId;
                    if let Some(target_conn) = connections.get_mut(&target_cid) {
                        if target_conn.in_world && target_conn.hp > 0.0 {
                            let armor = target_conn.equipped_armor.max(0) as f32;
                            let reduction = armor / (armor + 100.0);
                            let mut reduced = ((hit.amount as f32 * (1.0 - reduction)) as i32).max(1);
                            // Track 6 sub-task 4c — consume absorb
                            // pool before applying damage. Returns
                            // the residual + whether the pool hit 0
                            // (caller removes the buff).
                            let (after_absorb, absorb_exhausted) =
                                buffs::consume_absorb(&mut target_conn.active_buffs, reduced);
                            reduced = after_absorb;
                            if absorb_exhausted {
                                if let Some(idx) = target_conn.active_buffs.iter().position(|b| {
                                    matches!(b.effect, buffs::BuffEffect::Absorb { .. })
                                }) {
                                    absorb_buff_to_strip = Some(idx);
                                }
                            }
                            // Track 6 sub-task 4c — damage shield
                            // reflects damage back at the attacker.
                            // Read amount before mutating HP so a
                            // killing blow still triggers thorns.
                            shield_to_attacker = buffs::damage_shield_total(&target_conn.active_buffs);
                            shield_name_pve = buffs::first_damage_shield_name(&target_conn.active_buffs).map(|s| s.to_string());
                            target_conn.hp = (target_conn.hp - reduced as f32).max(0.0);
                            regen::mark_dirty(target_conn);
                            target_conn.note_damage_taken(now); // camp breaks on damage
                            // Track 15.2 follow-up — feed the pet
                            // inheritance pre-pass so a FOLLOW-stance
                            // pet auto-engages whatever's hitting its
                            // owner (matches player behaviour: pet
                            // helps when you're being attacked, not
                            // only when you swing). Uses the same
                            // last_attacked_enemy/at fields the
                            // owner-attacks path already populates.
                            target_conn.last_attacked_enemy = Some(attacker);
                            target_conn.last_attacked_at = Some(now);
                            // Track 18.1 — armor skill advance: each
                            // unique equipped armor_type gets one
                            // try_advance roll per incoming hit. Also
                            // gives the defender a dodge roll. Mirrors
                            // GDScript ArmorSkills.try_advance_worn
                            // plus WeaponSkills "dodge" path. Collect
                            // armor types first (immutable borrow of
                            // target_conn via its inventory map),
                            // then drive try_advance (mutable borrow).
                            let mut armor_types: Vec<String> = Vec::new();
                            let mut seen: std::collections::HashSet<String> =
                                std::collections::HashSet::new();
                            for entry in target_conn.inventory.equipment.values() {
                                if let Some(item) = items::lookup(&entry.item_path) {
                                    if !item.armor_type.is_empty()
                                        && seen.insert(item.armor_type.clone())
                                    {
                                        armor_types.push(item.armor_type.clone());
                                    }
                                }
                            }
                            for at in &armor_types {
                                if let Some(new_score) = skills::try_advance(
                                    target_conn,
                                    skills::Skill::Armor,
                                    at,
                                ) {
                                    handlers::send_skill_progress_update(
                                        &mut server,
                                        target_cid,
                                        skills::Skill::Armor.as_protocol(),
                                        at.clone(),
                                        new_score,
                                    );
                                }
                            }
                            if let Some(new_score) = skills::try_advance(
                                target_conn,
                                skills::Skill::Weapon,
                                "dodge",
                            ) {
                                handlers::send_skill_progress_update(
                                    &mut server,
                                    target_cid,
                                    skills::Skill::Weapon.as_protocol(),
                                    "dodge".to_string(),
                                    new_score,
                                );
                            }
                            // Track 19A — channeling-based cast interrupt
                            // on a hit landing during a cast. Helper
                            // mutates the cast cache + channeling score;
                            // we fan CastFail / SkillProgressUpdate
                            // based on the outcome.
                            let interrupt_outcome = roll_cast_interrupt(target_conn, now);
                            match &interrupt_outcome {
                                InterruptOutcome::Interrupted { spell_name } => {
                                    handlers::fan_out_cast_fail(
                                        &mut server,
                                        &in_world_recipients_now,
                                        hit.target,
                                        "interrupted (hit during cast)".to_string(),
                                    );
                                    tracing::info!(
                                        caster = hit.target,
                                        spell = %spell_name,
                                        "cast interrupted by incoming damage"
                                    );
                                }
                                InterruptOutcome::Survived {
                                    advanced_to: Some(new_score),
                                } => {
                                    handlers::send_skill_progress_update(
                                        &mut server,
                                        target_cid,
                                        skills::Skill::Casting.as_protocol(),
                                        "channeling".to_string(),
                                        *new_score,
                                    );
                                }
                                InterruptOutcome::Survived { advanced_to: None }
                                | InterruptOutcome::NotCasting => {}
                            }
                            damaged_player = Some(hit.target);
                            tracing::info!(
                                attacker,
                                target = hit.target,
                                raw_amount = hit.amount,
                                reduced,
                                armor = target_conn.equipped_armor,
                                hp_after = target_conn.hp,
                                "player damaged by enemy"
                            );
                            reduced
                        } else {
                            hit.amount
                        }
                    } else {
                        hit.amount
                    }
                } else {
                    hit.amount
                };
                // Strip the exhausted absorb buff after the immutable
                // borrow chain ends. Fan BuffSnapshot too.
                if let (Some(target_id), Some(idx)) = (damaged_player, absorb_buff_to_strip) {
                    let target_cid = target_id as ClientId;
                    if let Some(tc) = connections.get_mut(&target_cid) {
                        if idx < tc.active_buffs.len() {
                            tc.active_buffs.remove(idx);
                        }
                    }
                    if let Some(tc) = connections.get(&target_cid) {
                        fan_out_server_buff_snapshot(
                            &mut server,
                            &in_world_recipients_now,
                            tc,
                        );
                    }
                }
                // Apply damage shield to attacker (the enemy entity).
                // Look up by attacker id in the enemies map; if not
                // present (attacker died this tick), skip silently.
                if shield_to_attacker > 0.0 {
                    if let Some(att_entity) = enemies.get_mut(&attacker) {
                        if att_entity.is_alive() {
                            let dmg = shield_to_attacker as i32;
                            att_entity.hp = (att_entity.hp - dmg as f32).max(0.0);
                            handlers::fan_out_health_update(
                                &mut server,
                                &in_world_recipients_now,
                                attacker,
                                att_entity.hp,
                                att_entity.max_hp,
                            );
                            if let (Some(defender), Some(name)) = (damaged_player, shield_name_pve.as_ref()) {
                                handlers::fan_out_damage_shield_trigger(
                                    &mut server,
                                    &in_world_recipients_now,
                                    defender,
                                    attacker,
                                    dmg,
                                    name.clone(),
                                );
                            }
                            if att_entity.hp <= 0.0 {
                                att_entity.transition(EnemyState::Dead, now);
                                handlers::fan_out_entity_died(
                                    &mut server,
                                    &in_world_recipients_now,
                                    attacker,
                                );
                            }
                        }
                    }
                }
                handlers::fan_out_hit(
                    &mut server,
                    &in_world_recipients_now,
                    attacker,
                    hit.target,
                    final_amount,
                    false,
                    DamageType::Physical,
                );
                if let Some(target_id) = damaged_player {
                    // Server's regen broadcast loop (step 4l) would catch
                    // this within MAX_BROADCAST_GAP, but a fresh HP fan-out
                    // *now* keeps the target's HUD in lockstep with the Hit
                    // floating-number landing on the same tick.
                    if let Some(target_conn) = connections.get(&(target_id as ClientId)) {
                        handlers::fan_out_health_update(
                            &mut server,
                            &in_world_recipients_now,
                            target_id,
                            target_conn.hp,
                            target_conn.max_hp,
                        );
                    }
                }
            }
        }

        // 4j. Corpse cleanup. Dead enemies hold at their death pos for
        //     ENEMY_DESPAWN_LINGER_SECS so the client can play the fall-over
        //     animation; afterwards we fan out EntityDespawn, arm the
        //     spawn point's respawn timer, and drop the row from the
        //     world map. Collect ids in a first pass to avoid borrowing
        //     `enemies` mutably twice in the same loop.
        let corpse_linger = Duration::from_secs_f32(ENEMY_DESPAWN_LINGER_SECS);
        let mut expired_ids: Vec<EntityId> = Vec::new();
        for entity in enemies.values() {
            if entity.is_alive() {
                continue;
            }
            if now.duration_since(entity.state_entered_at) >= corpse_linger {
                expired_ids.push(entity.id);
            }
        }
        for id in expired_ids {
            let Some(entity) = enemies.remove(&id) else {
                continue;
            };
            // Track 7: remove from AOI; fan EntityDespawn only to players
            // who could see the enemy's cell (plus a dead pet's owner,
            // wherever they are).
            let enemy_cell = aoi::cell_for(entity.pos.x, entity.pos.z);
            aoi.remove(entity.id, enemy_cell);
            let visible = aoi.entities_visible_from(enemy_cell);
            for recipient in despawn_recipients(&visible, &in_world_recipients_now, &entity) {
                handlers::send_entity_despawn(&mut server, recipient, entity.id);
            }
            spawner.on_enemy_died(entity.spawn_point_idx, now);
        }

        // 4k-bind. Persist bind points immediately rather than waiting for the
        //      60 s checkpoint. Binding is deliberate and rare, and losing it to
        //      an ungraceful shutdown (there is no SIGTERM handler) would respawn
        //      the player somewhere they explicitly chose not to be — the exact
        //      failure this feature exists to prevent.
        for (char_id, zone, pos) in bind_intents.drain(..) {
            if let Err(e) = db::set_bind_point(&pool, char_id, zone.as_deref(), pos).await {
                tracing::warn!(char_id, error = %e, "set_bind_point failed; bind is live in memory but not persisted");
            }
        }

        // 4k-res. Corpse / resurrection Slice 3 — apply accepted res offers.
        //      Re-validate against the owner's recorded pending offer (so the
        //      refund % can't be forged), summon the living player to their corpse,
        //      refund a % of that death's lost xp, and mark the corpse resurrected
        //      (persisted) so it can't be re-rezzed for free xp.
        for (responder, corpse_id, accept) in resurrect_accept_intents.drain(..) {
            let responder_cid = responder as ClientId;
            // Read the pending offer, but do NOT consume it yet. It used to be
            // cleared unconditionally right here, before any validation, so a
            // stale or mismatched accept burned a perfectly good offer and a
            // transient DB failure below ate an accepted resurrection outright —
            // the old comment conceded as much ("the offer was already consumed
            // above"). Consume it only once the outcome is decided.
            let Some((offered_corpse, xp_percent)) =
                connections.get(&responder_cid).and_then(|c| c.pending_res_offer)
            else {
                continue; // nothing pending; nothing to lose
            };
            if !accept {
                // A decline is decisive: clear it.
                if let Some(c) = connections.get_mut(&responder_cid) {
                    c.pending_res_offer = None;
                }
                continue;
            }
            if offered_corpse != corpse_id {
                // Doesn't match what was offered. Leave the real offer standing
                // so the player can still accept it.
                tracing::info!(
                    char_id = responder,
                    corpse_id,
                    offered_corpse,
                    "resurrection accept ignored — corpse does not match the offer"
                );
                continue;
            }
            // Re-validate the corpse: still exists, owned by the responder, unrezzed.
            let (corpse_pos, lost_xp) = match corpses.get(&corpse_id) {
                Some(c) if c.owner_char == responder as i64 && !c.resurrected => (c.pos, c.lost_xp),
                _ => {
                    // Corpse gone, not theirs, or already rezzed: decisive, so
                    // the offer is spent and the player is told why.
                    if let Some(c) = connections.get_mut(&responder_cid) {
                        c.pending_res_offer = None;
                    }
                    tracing::info!(
                        char_id = responder, corpse_id,
                        "resurrection failed — corpse missing, not owned, or already resurrected"
                    );
                    handlers::send_refusal(
                        &mut server, responder_cid, "That corpse can't be resurrected.",
                    );
                    continue;
                }
            };
            // Persist the rezzed flag FIRST — only grant the res once it's durable,
            // so a write failure can't hand out a refund that a restart would let
            // the player claim a second time. On failure, skip the grant (a fresh
            // cast can retry); the offer was already consumed above.
            if let Err(e) = db::set_corpse_resurrected(&pool, corpse_id as i64).await {
                tracing::error!(corpse_id, error = %e, "set_corpse_resurrected failed — res not granted");
                // Transient, and NOT decisive: the offer stays live so the player
                // can simply accept again, rather than needing the cleric to
                // notice and re-cast. Previously this ate the resurrection.
                handlers::send_refusal(
                    &mut server,
                    responder_cid,
                    "The resurrection failed. Try accepting again.",
                );
                continue;
            }
            // Durable now, so the offer is finally spent.
            if let Some(c) = connections.get_mut(&responder_cid) {
                c.pending_res_offer = None;
            }
            if let Some(c) = corpses.get_mut(&corpse_id) {
                c.resurrected = true;
            }
            // Summon the living player to their corpse + refund the xp (award_xp
            // re-levels + fans XpGained/LevelUp).
            let refund = ((lost_xp as f32) * (xp_percent as f32 / 100.0)).round() as i32;
            if let Some(conn) = connections.get_mut(&responder_cid) {
                conn.pos = corpse_pos;
                handlers::send_teleport(&mut server, responder_cid, corpse_pos);
                if refund > 0 {
                    super::progression::award_xp(&mut server, conn, refund);
                }
            }
            tracing::info!(char_id = responder, corpse_id, xp_percent, refund, "resurrection accepted");
        }

        // 4k-quest. PD_W0023/24 — server-authoritative quest turn-ins. The
        //      reward comes from the server's own quest table (never a client
        //      amount), the turn-in must be backed by real counted gameplay
        //      (in the journal + every objective met — PD_W0024), and each
        //      quest pays once per character, ever: reject ids the character
        //      already completed, persist the completion FIRST (a crash
        //      between award and record could otherwise leave a paid quest
        //      replayable after relog), then award through award_xp. Every
        //      rejection answers with a private QuestRejected so the player
        //      sees WHY instead of silently getting nothing.
        for (responder, quest_id) in complete_quest_intents.drain(..) {
            let responder_cid = responder as ClientId;
            let Some(quest) = super::quests::lookup(&quest_id) else {
                // {:?} escapes control chars — the id is attacker-controlled and
                // a raw newline could forge log lines in the triage artifact.
                tracing::info!(char_id = responder, quest_id = ?quest_id, "quest turn-in rejected — unknown quest id");
                handlers::send_quest_rejected(&mut server, responder_cid, &quest_id, "Unknown quest.", false);
                continue;
            };
            let Some(reward) = super::quests::xp_reward_for(&quest_id) else {
                tracing::warn!(char_id = responder, quest_id = %quest_id, "quest turn-in rejected — bad reward tier in quests.toml");
                handlers::send_quest_rejected(&mut server, responder_cid, &quest_id, "Quest data error.", false);
                continue;
            };
            let Some(conn_ro) = connections.get(&responder_cid) else {
                continue; // disconnected mid-tick — nothing to award
            };
            // Proximity gate (dead-intents audit, 2026-08-24): a turn-in
            // requires standing near the quest's own NPC, when one is named.
            // Dev quests (test_q1) name none and stay ungated — the dev tools
            // are deliberate. This is what stops a modified client collecting
            // real rewards + XP without ever walking back.
            if let Some(npc_id) = quest.turn_in_npc.as_deref() {
                if !super::npcs::id_within_range(npc_id, conn_ro.pos) {
                    let npc_name = super::npcs::lookup(npc_id).map(|n| n.name.as_str()).unwrap_or(npc_id);
                    tracing::info!(char_id = responder, quest_id = %quest_id, npc = npc_id, "quest turn-in rejected — too far from the turn-in NPC");
                    handlers::send_quest_rejected(
                        &mut server,
                        responder_cid,
                        &quest_id,
                        &format!("You must return to {npc_name}."),
                        false,
                    );
                    continue;
                }
            }
            // A legit client can only HOLD a quest it met level_req for, so this
            // costs a real player nothing — it only shrinks a forged burst
            // (send-every-known-id-at-login) to the quests a fresh char could
            // actually have.
            if conn_ro.level < quest.level_req {
                tracing::info!(char_id = responder, quest_id = %quest_id, level = conn_ro.level, level_req = quest.level_req, "quest turn-in rejected — below level_req");
                handlers::send_quest_rejected(&mut server, responder_cid, &quest_id, "You are too low level for this quest.", false);
                continue;
            }
            if conn_ro.completed_quests.contains(&quest_id) {
                tracing::info!(char_id = responder, quest_id = %quest_id, "quest turn-in rejected — already completed");
                handlers::send_quest_rejected(&mut server, responder_cid, &quest_id, "You have already completed this quest.", false);
                continue;
            }
            // PD_W0024 — the objective check. The quest must actually be in
            // the server-side journal (an AcceptQuest landed) with every
            // count met. This closes the phase-1 residual exploit: a forged
            // CompleteQuest with no kills behind it pays nothing.
            let Some(progress) = conn_ro.active_quests.get(&quest_id) else {
                tracing::info!(char_id = responder, quest_id = %quest_id, "quest turn-in rejected — not in journal");
                handlers::send_quest_rejected(&mut server, responder_cid, &quest_id, "That quest is not in your journal.", false);
                continue;
            };
            if !super::quests::objectives_met(quest, progress) {
                tracing::info!(char_id = responder, quest_id = %quest_id, progress = ?progress, "quest turn-in rejected — objectives incomplete");
                handlers::send_quest_rejected(&mut server, responder_cid, &quest_id, "Quest objectives are not complete.", false);
                continue;
            }
            // PD_W0024 slice B — item rewards. Pre-check capacity BEFORE the
            // once-ever completion record: a full bag rejects the turn-in (keep
            // the quest, make room, retry) rather than burning the completion
            // with no item. rollback=false so the quest stays in the journal.
            let rewards: Vec<(String, u32)> =
                quest.item_rewards.iter().map(|p| (p.clone(), 1u32)).collect();
            if !rewards.is_empty() && !conn_ro.inventory.can_accept(&rewards) {
                tracing::info!(char_id = responder, quest_id = %quest_id, "quest turn-in rejected — inventory full");
                handlers::send_quest_rejected(&mut server, responder_cid, &quest_id, "Your inventory is full — make room and try again.", false);
                continue;
            }
            if let Err(e) = db::record_quest_completion(&pool, responder as i64, &quest_id).await
            {
                tracing::error!(char_id = responder, quest_id = %quest_id, error = %e, "record_quest_completion failed — reward not granted");
                handlers::send_quest_rejected(&mut server, responder_cid, &quest_id, "Server error — try again.", false);
                continue;
            }
            // The active row is now redundant (completed_quests is the
            // permanent record). A failed delete self-heals: the login
            // normalization drops actives that are already completed.
            if let Err(e) = db::delete_active_quest(&pool, responder as i64, &quest_id).await {
                tracing::warn!(char_id = responder, quest_id = %quest_id, error = %e, "delete_active_quest after completion failed");
            }
            if let Some(conn) = connections.get_mut(&responder_cid) {
                conn.completed_quests.insert(quest_id.clone());
                conn.active_quests.remove(&quest_id);
                conn.quests_dirty.remove(&quest_id);
                // Send the completion confirm BEFORE the reward XP: both ride the
                // same reliable-ordered channel, so the client sees QuestCompleted
                // first and can tag the immediately-following XpGained as
                // quest-sourced (for the "quest experience" chat line).
                handlers::send_quest_completed(&mut server, responder_cid, &quest_id);
                super::progression::award_xp(&mut server, conn, reward);
                // Grant item reward(s). Capacity was pre-checked, so
                // add_item_locating fits; the completion is already durable, so
                // a crash here can only lose the item, never dup it (the forced
                // save below closes even that window). Items ride the existing
                // InventoryDelta + LootGranted path — same as looting.
                for (path, count) in &rewards {
                    match conn.inventory.add_item_locating(path, *count) {
                        Ok((touched, leftover)) => {
                            conn.inventory_dirty = true;
                            if leftover > 0 {
                                tracing::error!(char_id = responder, quest_id = %quest_id, %path, leftover, "quest reward partially placed despite capacity pre-check");
                            }
                            for (loc, slot) in &touched {
                                let (dpath, dcount) = match conn.inventory.peek_at(loc, *slot) {
                                    Some((p, c)) => (Some(p), c),
                                    None => (None, 0),
                                };
                                handlers::send_inventory_delta(&mut server, responder_cid, loc.clone(), *slot, dpath, dcount);
                            }
                            handlers::send_loot_granted(&mut server, responder_cid, path.clone(), count.saturating_sub(leftover));
                        }
                        Err(e) => {
                            tracing::error!(char_id = responder, quest_id = %quest_id, %path, error = e, "quest reward grant failed after completion recorded");
                        }
                    }
                }
                tracing::info!(char_id = responder, quest_id = %quest_id, reward, "quest completed");
            }
            // Force-persist the granted inventory so a crash after the (already
            // durable) completion record can't lose the reward. Rare path; a
            // failed save just falls back to the periodic checkpoint.
            if !rewards.is_empty() {
                let inv = connections
                    .get(&responder_cid)
                    .map(|c| (c.char_id, c.inventory.to_rows()));
                if let Some((cid, rows)) = inv {
                    if let Err(e) = db::save_inventory(&pool, cid, &rows).await {
                        tracing::warn!(char_id = responder, quest_id = %quest_id, error = %e, "quest reward inventory save failed (will retry on checkpoint)");
                    }
                }
            }
        }

        // 4k-quest-lifecycle. PD_W0024 — accept / abandon, drained in ARRIVAL
        //      order so a same-tick abandon-then-accept (or the reverse) on one
        //      quest ends in the client's last-stated state. These only mutate
        //      in-memory `active_quests` and mark `quests_dirty`; the single
        //      reconciling DB write happens in the end-of-tick flush (step
        //      6-ter: upsert if still active, delete if not). So a forged
        //      accept/abandon storm costs at most one write per quest per tick,
        //      never an awaited write per message.
        for (responder, action, quest_id) in quest_lifecycle_intents.drain(..) {
            let responder_cid = responder as ClientId;
            match action {
                QuestLifecycle::Accept => {
                    // Rejections here are accept-phase, so `rollback = true`:
                    // the client undoes its optimistic journal add.
                    let Some(quest) = super::quests::lookup(&quest_id) else {
                        tracing::info!(char_id = responder, quest_id = ?quest_id, "quest accept rejected — unknown quest id");
                        handlers::send_quest_rejected(&mut server, responder_cid, &quest_id, "Unknown quest.", true);
                        continue;
                    };
                    let Some(conn) = connections.get_mut(&responder_cid) else {
                        continue; // disconnected mid-tick
                    };
                    if conn.level < quest.level_req {
                        tracing::info!(char_id = responder, quest_id = %quest_id, level = conn.level, level_req = quest.level_req, "quest accept rejected — below level_req");
                        handlers::send_quest_rejected(&mut server, responder_cid, &quest_id, "You are too low level for this quest.", true);
                        continue;
                    }
                    if conn.completed_quests.contains(&quest_id) {
                        tracing::info!(char_id = responder, quest_id = %quest_id, "quest accept rejected — already completed");
                        handlers::send_quest_rejected(&mut server, responder_cid, &quest_id, "You have already completed this quest.", true);
                        continue;
                    }
                    if conn.active_quests.contains_key(&quest_id) {
                        // Already tracking (a duplicate send, e.g. a re-click
                        // before the journal refreshed) — not an error, and no
                        // rollback (the quest legitimately stays).
                        tracing::debug!(char_id = responder, quest_id = %quest_id, "quest accept ignored — already active");
                        continue;
                    }
                    if conn.active_quests.len() >= super::quests::MAX_ACTIVE {
                        tracing::info!(char_id = responder, quest_id = %quest_id, "quest accept rejected — journal full");
                        handlers::send_quest_rejected(&mut server, responder_cid, &quest_id, "Your quest journal is full.", true);
                        continue;
                    }
                    conn.active_quests.insert(quest_id.clone(), vec![0i32; quest.objectives.len()]);
                    conn.quests_dirty.insert(quest_id.clone());
                    tracing::info!(char_id = responder, quest_id = %quest_id, "quest accepted");
                }
                QuestLifecycle::Abandon => {
                    // Re-accepting later starts from zero; the completion record
                    // is a different table, so abandon can never re-open a payout.
                    let Some(conn) = connections.get_mut(&responder_cid) else {
                        continue;
                    };
                    if conn.active_quests.remove(&quest_id).is_none() {
                        tracing::info!(char_id = responder, quest_id = ?quest_id, "quest abandon ignored — not active");
                        continue;
                    }
                    // Mark dirty (not clear): the flush reconciles a now-absent
                    // active quest into a row DELETE.
                    conn.quests_dirty.insert(quest_id.clone());
                    tracing::info!(char_id = responder, quest_id = %quest_id, "quest abandoned");
                }
            }
        }

        // 4ka. Apply loot pickup intents. For each: validate bag, slot
        //      bounds, and looter range. On success drain the relevant
        //      stack(s), send LootGranted privately to the looter, and
        //      either re-broadcast LootBagSpawn (bag still has items)
        //      or EntityDespawn (bag is empty) to every in_world peer.
        //      Out-of-range / unknown-bag / out-of-slot intents drop
        //      silently — the GDScript UI already gates the click on
        //      LOOT_RANGE so a legitimate user can't trip this.
        if !loot_intents.is_empty() {
            for intent in loot_intents.drain(..) {
                let Some(looter_conn) = connections.get(&(intent.looter as ClientId)) else {
                    continue;
                };
                let looter_pos = looter_conn.pos;

                // Corpse / resurrection Slice 2 — corpse loot. Corpses share the
                // loot-bag id partition, so a LootItem/LootAll keyed by a
                // corpse_id lands here; resolve corpses FIRST, then fall through
                // to loot bags. Owner-only, no group/round-robin/coin-split (you
                // take your own gear back to your own bags), and unlike a
                // transient bag the corpse is DB-backed so the take is persisted
                // atomically (db::apply_corpse_loot). A corpse LOOTED empty
                // despawns; a corpse that was BORN empty (naked death) lingers as
                // a res anchor — the despawn here only fires from a loot action.
                if corpses.contains_key(&intent.bag_id) {
                    let corpse_id = intent.bag_id;
                    let looter_cid = intent.looter as ClientId;
                    // Range + owner gate (read-only).
                    {
                        let corpse = corpses.get(&corpse_id).unwrap();
                        if corpse.pos.distance_to(looter_pos) > LOOT_PICKUP_RANGE {
                            continue;
                        }
                        if corpse.owner_char != intent.looter as i64 {
                            handlers::send_loot_rejected(
                                &mut server,
                                looter_cid,
                                "That is not your corpse.".to_string(),
                            );
                            continue;
                        }
                    }
                    // Pre-loot snapshot. The take mutates in-memory BEFORE it is
                    // persisted; if the atomic persist fails we roll the in-memory
                    // side back to exactly this so it matches the rolled-back DB
                    // (no dupe, no loss). Every client message is also DEFERRED
                    // until the persist commits, so a rollback has nothing to
                    // un-send. `had_content` decides linger-vs-despawn: only a
                    // corpse that HELD something and is now empty is "looted clean";
                    // a born-empty corpse (naked-death res anchor) lingers.
                    let corpse_items_before: Vec<(String, u32)> = {
                        let corpse = corpses.get(&corpse_id).unwrap();
                        corpse.items.iter().map(|s| (s.item_path.clone(), s.count)).collect()
                    };
                    let corpse_coins_before = corpses.get(&corpse_id).unwrap().coins;
                    let had_content = !corpse_items_before.is_empty()
                        || corpse_coins_before != protocol::world::Coins::ZERO;
                    let (inv_before, coins_before) = match connections.get(&looter_cid) {
                        Some(c) => (c.inventory.clone(), c.coins),
                        None => continue,
                    };

                    // Coins: credited WHOLE to the owner (their own carried wallet
                    // returning — no group split), then the corpse's coin is zeroed.
                    let mut coin_update: Option<protocol::world::Coins> = None;
                    // Returned TIER BY TIER, not as a copper total.
                    //
                    // This used to flatten the corpse to `total_copper()` and add
                    // it back with `add_payout`, which re-normalises: a player who
                    // died holding 312 copper got back 3 silver 12 copper. The
                    // value matched, but coin weight is charged flat PER COIN, so
                    // 312 coins became 15 and they rose lighter than they fell.
                    // Dying was a way to compress your purse. The four tiers are
                    // deliberately independent stacks, so what went onto the corpse
                    // is what comes back off it.
                    let corpse_coins = {
                        let corpse = corpses.get_mut(&corpse_id).unwrap();
                        let c = corpse.coins;
                        corpse.coins = protocol::world::Coins::ZERO;
                        c
                    };
                    if corpse_coins != protocol::world::Coins::ZERO {
                        if let Some(c) = connections.get_mut(&looter_cid) {
                            c.coins = c.coins.add_each(corpse_coins);
                            c.coins_dirty = true;
                            coin_update = Some(c.coins);
                        }
                    }
                    // Items: take one (slot) or all (None) off the corpse.
                    let mut granted: Vec<(String, u32)> = Vec::new();
                    {
                        let corpse = corpses.get_mut(&corpse_id).unwrap();
                        match intent.slot {
                            Some(idx) => {
                                let i = idx as usize;
                                if i < corpse.items.len() {
                                    let stack = corpse.items.remove(i);
                                    granted.push((stack.item_path, stack.count));
                                }
                            }
                            None => {
                                for stack in corpse.items.drain(..) {
                                    granted.push((stack.item_path, stack.count));
                                }
                            }
                        }
                    }
                    // Move each stack into the looter's bags (same add_item_locating
                    // path the bag loot uses); a full inventory refunds the unplaced
                    // portion to the corpse. Collect the client deltas — don't send.
                    let mut inv_deltas: Vec<(String, u32, String, u32)> = Vec::new();
                    let mut granted_lines: Vec<(String, u32)> = Vec::new();
                    for (path, count) in granted {
                        let mut placed_count: u32 = 0;
                        let mut leftover_count: u32 = count;
                        if let Some(conn) = connections.get_mut(&looter_cid) {
                            if let Ok((touched, leftover)) =
                                conn.inventory.add_item_locating(&path, count)
                            {
                                for (loc, slot) in &touched {
                                    if let Some((p, c)) = conn.inventory.peek_at(loc, *slot) {
                                        inv_deltas.push((loc.clone(), *slot, p, c));
                                    }
                                }
                                placed_count = count - leftover;
                                leftover_count = leftover;
                                conn.inventory_dirty = true;
                            }
                        }
                        if placed_count > 0 {
                            granted_lines.push((path.clone(), placed_count));
                        }
                        if leftover_count > 0 {
                            corpses.get_mut(&corpse_id).unwrap().items.push(
                                loot::LootItemStack { item_path: path, count: leftover_count },
                            );
                        }
                    }
                    // Post-drain corpse state for the atomic persist + the despawn
                    // decision (corpse borrow dropped before the await below).
                    let (corpse_items_now, corpse_coins_now, corpse_pos, corpse_empty) = {
                        let corpse = corpses.get(&corpse_id).unwrap();
                        (
                            corpse
                                .items
                                .iter()
                                .map(|s| (s.item_path.clone(), s.count))
                                .collect::<Vec<(String, u32)>>(),
                            corpse.coins,
                            corpse.pos,
                            corpse.items.is_empty() && corpse.coins == protocol::world::Coins::ZERO,
                        )
                    };
                    let delete_now = corpse_emptied_by_loot(had_content, corpse_empty);
                    // Persist ATOMICALLY: looter inventory + wallet AND the corpse
                    // (rewritten, or DELETED when looted empty) in ONE tx, so a
                    // crash can't dupe/lose. Extract the owned rows + wallet FIRST
                    // so no `connections` borrow is held across the await.
                    let persist_data = connections
                        .get(&looter_cid)
                        .map(|c| (c.inventory.to_rows(), c.coins));
                    let persist_ok = if let Some((inv_rows, looter_coins)) = persist_data {
                        match db::apply_corpse_loot(
                            &pool,
                            intent.looter as i64,
                            &inv_rows,
                            looter_coins,
                            corpse_id as i64,
                            &corpse_items_now,
                            corpse_coins_now,
                            delete_now,
                        )
                        .await
                        {
                            Ok(()) => true,
                            Err(e) => {
                                tracing::error!(corpse_id, error = %e, "apply_corpse_loot persist failed");
                                false
                            }
                        }
                    } else {
                        false
                    };

                    if persist_ok {
                        // Durable — clear the dirty flags and NOW push the deferred
                        // client updates, then despawn / refresh the window.
                        if let Some(c) = connections.get_mut(&looter_cid) {
                            c.inventory_dirty = false;
                            c.coins_dirty = false;
                        }
                        if let Some(coins) = coin_update {
                            handlers::send_coins_update(&mut server, looter_cid, coins);
                        }
                        for (loc, slot, item_path, total_count) in inv_deltas {
                            handlers::send_inventory_delta(
                                &mut server,
                                looter_cid,
                                loc,
                                slot,
                                Some(item_path),
                                total_count,
                            );
                        }
                        for (path, n) in granted_lines {
                            handlers::send_loot_granted(&mut server, looter_cid, path, n);
                        }
                        if delete_now {
                            // Looted clean -> the body vanishes (the DB rows were
                            // already deleted inside apply_corpse_loot's tx).
                            corpses.remove(&corpse_id);
                            let cell = aoi::cell_for(corpse_pos.x, corpse_pos.z);
                            aoi.remove(corpse_id, cell);
                            let visible = aoi.entities_visible_from(cell);
                            for &recipient in &in_world_recipients_now {
                                if visible.contains(&recipient) {
                                    handlers::send_entity_despawn(&mut server, recipient, corpse_id);
                                }
                            }
                            tracing::info!(corpse_id, char_id = intent.looter, "corpse looted empty — despawned");
                        } else if let Some(corpse) = corpses.get(&corpse_id) {
                            handlers::send_corpse_contents(&mut server, looter_cid, corpse);
                        }
                    } else {
                        // Persist failed — roll the in-memory side back to the
                        // pre-loot snapshot so it matches the rolled-back DB (no
                        // dupe, no loss), and tell the owner the loot didn't take.
                        // No optimistic messages were sent, so there's nothing to
                        // correct on the client beyond the refreshed window.
                        if let Some(corpse) = corpses.get_mut(&corpse_id) {
                            corpse.items = corpse_items_before
                                .into_iter()
                                .map(|(item_path, count)| loot::LootItemStack { item_path, count })
                                .collect();
                            corpse.coins = corpse_coins_before;
                        }
                        if let Some(conn) = connections.get_mut(&looter_cid) {
                            conn.inventory = inv_before;
                            conn.coins = coins_before;
                        }
                        handlers::send_loot_rejected(
                            &mut server,
                            looter_cid,
                            "Couldn't loot the corpse — try again.".to_string(),
                        );
                        if let Some(corpse) = corpses.get(&corpse_id) {
                            handlers::send_corpse_contents(&mut server, looter_cid, corpse);
                        }
                    }
                    continue;
                }

                let Some(bag) = loot_bags.get_mut(&intent.bag_id) else {
                    continue;
                };
                if bag.pos.distance_to(looter_pos) > LOOT_PICKUP_RANGE {
                    continue;
                }
                // Loot rights: only the kill-creditor and their group may
                // take an owned corpse. Public bags (dropped items) are
                // open to anyone in range. Strangers are rejected; a
                // user-facing "that isn't your loot" message rides in with
                // the wire changes (Layer 4).
                if !bag.can_loot(intent.looter as ClientId, &group_manager) {
                    tracing::info!(
                        looter = intent.looter,
                        bag_id = intent.bag_id,
                        "loot rejected: not the owner or owner's group"
                    );
                    handlers::send_loot_rejected(
                        &mut server,
                        intent.looter as ClientId,
                        "That isn't your loot.".to_string(),
                    );
                    continue;
                }
                // Round Robin: the first loot attempt claims this corpse
                // for the next eligible group member (online + within the
                // coin-share range), advancing the group's turn; only they
                // may take its items. FFA / solo / public bags are
                // unrestricted (next_loot_turn returns None). The coin
                // block below ignores the turn, but a rejected click
                // `continue`s before it — so coin is only ever paid out to
                // the rightful looter, not whoever clicks first.
                if let Some(owner_cid) = bag.owner_killer {
                    if bag.assigned_looter.is_none() {
                        let bag_pos = bag.pos;
                        bag.assigned_looter = group_manager.next_loot_turn(owner_cid, |cand| {
                            connections
                                .get(&cand)
                                .map(|c| c.pos.distance_to(bag_pos) <= GROUP_SHARE_RANGE)
                                .unwrap_or(false)
                        });
                    }
                    if let Some(turn) = bag.assigned_looter {
                        if (intent.looter as ClientId) != turn {
                            tracing::info!(
                                looter = intent.looter,
                                bag_id = intent.bag_id,
                                assigned = turn,
                                "loot rejected: not your turn (round robin)"
                            );
                            handlers::send_loot_rejected(
                                &mut server,
                                intent.looter as ClientId,
                                "Not your turn to loot.".to_string(),
                            );
                            continue;
                        }
                    }
                }
                // Validate the slot BEFORE paying any coin. This check used to
                // sit after the payout, so a bad slot index still zeroed the
                // bag's coin and credited it, then bailed with nothing said: the
                // player saw coin arrive and no item. Not a dupe (the coin is
                // credited once either way), but mutate-then-validate is the
                // opposite of the rule the store transactions follow.
                if let Some(idx) = intent.slot {
                    if idx as usize >= bag.items.len() {
                        tracing::info!(
                            looter = intent.looter,
                            bag_id = intent.bag_id,
                            slot = idx,
                            "loot rejected: no such slot in the bag"
                        );
                        handlers::send_loot_rejected(
                            &mut server,
                            intent.looter as ClientId,
                            "That isn't there any more.".to_string(),
                        );
                        continue;
                    }
                }
                // Coin: credited on the first loot action against the bag,
                // then zeroed. Unified rule (group_loot_and_coin.md): the
                // looter alone gets it if the group is Free-for-all or the
                // looter has /autosplit off; otherwise it splits evenly
                // among online group members within GROUP_SHARE_RANGE
                // of the corpse, remainder copper to the looter.
                if bag.coins != protocol::world::Coins::ZERO {
                    let pot = bag.coins.total_copper();
                    bag.coins = protocol::world::Coins::ZERO;
                    let bag_pos = bag.pos;
                    let looter_cid = intent.looter as ClientId;
                    let looter_autosplit = connections
                        .get(&looter_cid)
                        .map(|c| c.autosplit)
                        .unwrap_or(false);
                    let group = group_manager.group_of(looter_cid);
                    // Coin distribution follows the looter's /autosplit
                    // flag regardless of loot mode (RR vs FFA only governs
                    // item turns): autosplit on → split among the nearby
                    // group; off → the looter keeps it (master-looter case
                    // is FFA + autosplit off). Solo/ungrouped never splits.
                    let do_split = looter_autosplit;
                    let mut recipients: Vec<ClientId> = Vec::new();
                    if do_split {
                        if let Some(g) = group {
                            for &m in &g.members {
                                if let Some(c) = connections.get(&m) {
                                    if c.pos.distance_to(bag_pos) <= GROUP_SHARE_RANGE {
                                        recipients.push(m);
                                    }
                                }
                            }
                        }
                    }
                    // FFA, autosplit off, or nobody eligible nearby → the
                    // looter takes it all (coin is never destroyed).
                    if recipients.is_empty() {
                        recipients.push(looter_cid);
                    }
                    let n = recipients.len() as i64;
                    let base_share = pot / n;
                    let remainder = pot - base_share * n;
                    for &r in &recipients {
                        let mut amount = base_share;
                        if r == looter_cid {
                            amount += remainder;
                        }
                        if amount <= 0 {
                            continue;
                        }
                        if let Some(c) = connections.get_mut(&r) {
                            c.coins.add_payout(amount);
                            c.coins_dirty = true;
                            let coins_after = c.coins;
                            handlers::send_coins_update(&mut server, r, coins_after);
                        }
                    }
                    tracing::info!(
                        looter = intent.looter,
                        bag_id = intent.bag_id,
                        pot,
                        recipients = recipients.len(),
                        do_split,
                        "coin looted"
                    );
                }
                let mut granted: Vec<(String, u32)> = Vec::new();
                match intent.slot {
                    Some(idx) => {
                        let i = idx as usize;
                        if i >= bag.items.len() {
                            continue;
                        }
                        let stack = bag.items.remove(i);
                        granted.push((stack.item_path, stack.count));
                    }
                    None => {
                        let drained: Vec<_> = bag.items.drain(..).collect();
                        for stack in drained {
                            granted.push((stack.item_path, stack.count));
                        }
                    }
                }
                for (path, count) in granted {
                    // Track 13.2 / 14.1 — server-side inventory mutation
                    // + authoritative slot pick. add_item_locating may
                    // touch multiple slots (stack-top-up + spill into
                    // new slots, each capped at the registry's
                    // max_stack); we fan one InventoryDelta per
                    // touched slot. Anything that doesn't fit becomes
                    // `leftover`, which we refund to the loot bag so
                    // the player can pick it up later.
                    let looter_cid = intent.looter as ClientId;
                    let mut touched_deltas: Vec<(String, u32, String, u32)> = Vec::new();
                    let mut placed_count: u32 = 0;
                    let mut leftover_count: u32 = 0;
                    if let Some(conn) = connections.get_mut(&looter_cid) {
                        match conn.inventory.add_item_locating(&path, count) {
                            Ok((touched, leftover)) => {
                                if !touched.is_empty() {
                                    conn.inventory_dirty = true;
                                }
                                for (loc, slot) in &touched {
                                    if let Some((p, c)) = conn.inventory.peek_at(loc, *slot) {
                                        touched_deltas.push((loc.clone(), *slot, p, c));
                                    }
                                }
                                placed_count = count - leftover;
                                leftover_count = leftover;
                            }
                            Err(e) => {
                                tracing::info!(
                                    looter = intent.looter,
                                    item_path = %path,
                                    count,
                                    error = %e,
                                    "server inventory add_item rejected; loot stack refunded to bag",
                                );
                                leftover_count = count;
                            }
                        }
                    }
                    for (loc, slot, item_path, total_count) in touched_deltas {
                        handlers::send_inventory_delta(
                            &mut server,
                            looter_cid,
                            loc,
                            slot,
                            Some(item_path),
                            total_count,
                        );
                    }
                    if placed_count > 0 {
                        handlers::send_loot_granted(
                            &mut server,
                            looter_cid,
                            path.clone(),
                            placed_count,
                        );
                    }
                    if leftover_count > 0 {
                        // Refund — push the unplaced portion back into
                        // the bag so it stays available. Bag fan-out
                        // below re-broadcasts the updated contents.
                        bag.items.push(loot::LootItemStack {
                            item_path: path,
                            count: leftover_count,
                        });
                        tracing::info!(
                            looter = intent.looter,
                            bag_id = bag.id,
                            leftover = leftover_count,
                            "loot partially placed; remainder refunded to bag"
                        );
                    }
                }
                // Track 7: capture position before potentially removing the bag.
                let bag_id = bag.id;
                let bag_pos = bag.pos;
                let bag_cell = aoi::cell_for(bag_pos.x, bag_pos.z);
                let bag_visible = aoi.entities_visible_from(bag_cell);
                if bag.is_empty() {
                    loot_bags.remove(&bag_id);
                    aoi.remove(bag_id, bag_cell);
                    for &recipient in &in_world_recipients_now {
                        if bag_visible.contains(&recipient) {
                            handlers::send_entity_despawn(&mut server, recipient, bag_id);
                        }
                    }
                } else {
                    let bag_recipients: Vec<ClientId> = in_world_recipients_now
                        .iter()
                        .copied()
                        .filter(|id| bag_visible.contains(id))
                        .collect();
                    handlers::fan_out_loot_bag_spawn(
                        &mut server,
                        &bag_recipients,
                        bag,
                    );
                }
            }
        }

        // 4jc. PD_W0027 — ground pickup to the cursor. Same gates as the
        //      loot window (range, loot rights, round-robin turn), plus the
        //      cursor rules: only a bag holding exactly one item stack and
        //      zero coin qualifies, and the hand must be empty. The take is
        //      atomic within this block — stack out, cursor in, bag gone,
        //      deltas fanned — so no error path leaves a half-state.
        if !loot_to_cursor_intents.is_empty() {
            for (looter, bag_id) in loot_to_cursor_intents.drain(..) {
                let looter_cid = looter as ClientId;
                let Some(looter_pos) = connections.get(&looter_cid).map(|c| c.pos) else {
                    continue;
                };
                let stack = {
                    let Some(bag) = loot_bags.get_mut(&bag_id) else {
                        handlers::send_refusal(
                            &mut server,
                            looter_cid,
                            "That isn't there any more.",
                        );
                        continue;
                    };
                    if bag.pos.distance_to(looter_pos) > LOOT_PICKUP_RANGE {
                        continue;
                    }
                    if !bag.can_loot(looter_cid, &group_manager) {
                        tracing::info!(
                            looter,
                            bag_id,
                            "pickup rejected: not the owner or owner's group"
                        );
                        handlers::send_loot_rejected(
                            &mut server,
                            looter_cid,
                            "That isn't your loot.".to_string(),
                        );
                        continue;
                    }
                    // Round Robin claims the turn exactly as the loot-window
                    // path does — a left-click pickup must not dodge the
                    // group's rotation.
                    if let Some(owner_cid) = bag.owner_killer {
                        if bag.assigned_looter.is_none() {
                            let bag_pos = bag.pos;
                            bag.assigned_looter =
                                group_manager.next_loot_turn(owner_cid, |cand| {
                                    connections
                                        .get(&cand)
                                        .map(|c| {
                                            c.pos.distance_to(bag_pos)
                                                <= GROUP_SHARE_RANGE
                                        })
                                        .unwrap_or(false)
                                });
                        }
                        if let Some(turn) = bag.assigned_looter {
                            if looter_cid != turn {
                                tracing::info!(
                                    looter,
                                    bag_id,
                                    assigned = turn,
                                    "pickup rejected: not your turn (round robin)"
                                );
                                handlers::send_loot_rejected(
                                    &mut server,
                                    looter_cid,
                                    "Not your turn to loot.".to_string(),
                                );
                                continue;
                            }
                        }
                    }
                    if bag.items.len() != 1 || bag.coins != protocol::world::Coins::ZERO {
                        handlers::send_refusal(
                            &mut server,
                            looter_cid,
                            "There's more than one thing there. Right-click to loot.",
                        );
                        continue;
                    }
                    bag.items[0].clone()
                };
                let Some(conn) = connections.get_mut(&looter_cid) else {
                    continue;
                };
                if conn.inventory.cursor.is_some() {
                    handlers::send_refusal(
                        &mut server,
                        looter_cid,
                        "You're already holding something.",
                    );
                    continue;
                }
                // Commit: cursor in, bag out, world told. All in this tick.
                conn.inventory.cursor = Some(super::inventory::InventoryEntry {
                    item_path: stack.item_path.clone(),
                    count: stack.count,
                });
                conn.inventory_dirty = true;
                handlers::send_inventory_delta(
                    &mut server,
                    looter_cid,
                    "cursor".to_string(),
                    0,
                    Some(stack.item_path.clone()),
                    stack.count,
                );
                let item_name = items::lookup(&stack.item_path)
                    .map(|i| i.name.clone())
                    .unwrap_or_else(|| "the item".to_string());
                handlers::send_system_line(
                    &mut server,
                    looter_cid,
                    &format!("You pick up {}.", item_name),
                );
                let bag = loot_bags
                    .remove(&bag_id)
                    .expect("bag existed under the borrow above");
                let bag_cell = aoi::cell_for(bag.pos.x, bag.pos.z);
                let bag_visible = aoi.entities_visible_from(bag_cell);
                aoi.remove(bag_id, bag_cell);
                for &recipient in &in_world_recipients_now {
                    if bag_visible.contains(&recipient) {
                        handlers::send_entity_despawn(&mut server, recipient, bag_id);
                    }
                }
                tracing::info!(
                    looter,
                    bag_id,
                    item = %stack.item_path,
                    count = stack.count,
                    "ground item picked up to cursor"
                );
            }
        }

        // 4k. Loot bag expiry. Bags linger LOOT_BAG_LINGER_SECS so
        //     players have time to walk over and click; afterwards we
        //     fan out EntityDespawn and drop the bag. Bags emptied
        //     mid-life by the apply phase above already despawned via
        //     EntityDespawn there — this loop only catches bags that
        //     ran out the clock without being looted.
        let bag_linger = Duration::from_secs_f32(LOOT_BAG_LINGER_SECS);
        let mut expired_bags: Vec<EntityId> = Vec::new();
        for bag in loot_bags.values() {
            if now.duration_since(bag.spawned_at) >= bag_linger {
                expired_bags.push(bag.id);
            }
        }
        for id in expired_bags {
            if let Some(bag) = loot_bags.remove(&id) {
                // Track 7: remove from AOI; fan EntityDespawn only to visible players.
                let bag_cell = aoi::cell_for(bag.pos.x, bag.pos.z);
                aoi.remove(id, bag_cell);
                let visible = aoi.entities_visible_from(bag_cell);
                for &recipient in &in_world_recipients_now {
                    if visible.contains(&recipient) {
                        handlers::send_entity_despawn(&mut server, recipient, id);
                    }
                }
            }
        }

        // 4k-bis. Corpse / resurrection Slice 1 — corpse decay. Corpses linger
        //     much longer than loot bags (CORPSE_LINGER_SECS); on expiry the
        //     corpse + its gear are gone for good: despawn to visible peers,
        //     remove from AOI + map, AND delete the DB rows so a restart can't
        //     bring it back.
        let corpse_linger = Duration::from_secs_f32(super::corpses::CORPSE_LINGER_SECS);
        let mut expired_corpses: Vec<EntityId> = Vec::new();
        for corpse in corpses.values() {
            if now.duration_since(corpse.spawned_at) >= corpse_linger {
                expired_corpses.push(corpse.id);
            }
        }
        for id in expired_corpses {
            if let Some(corpse) = corpses.remove(&id) {
                let cell = aoi::cell_for(corpse.pos.x, corpse.pos.z);
                aoi.remove(id, cell);
                let visible = aoi.entities_visible_from(cell);
                for &recipient in &in_world_recipients_now {
                    if visible.contains(&recipient) {
                        handlers::send_entity_despawn(&mut server, recipient, id);
                    }
                }
                if let Err(e) = db::delete_corpse(&pool, id as i64).await {
                    tracing::error!(corpse_id = id, error = %e, "delete_corpse on decay failed");
                }
                tracing::info!(corpse_id = id, char_id = corpse.owner_char, "corpse decayed — gear lost");
            }
        }

        // 5. Integrate movement intent exactly once per tick. The Move
        //    handler stores the latest direction on the connection; we
        //    advance position here so the rate is bound to wall-clock
        //    ticks rather than client message arrival rate. Stale-move
        //    threshold: if no Move has arrived in STALE_MOVE_THRESHOLD,
        //    integrate zero — protects against a crashed client visually
        //    running forward until the heartbeat timeout.
        // dt was computed above for the AI tick; reuse.
        //
        // Track 7: collect (client_id, old_cell, new_cell) for any player
        // who crosses a cell boundary this tick. Applied to `aoi` and
        // fanned as EntitySpawn/Despawn AFTER the loop so we don't hold
        // a mutable borrow on `connections` while also needing it for
        // the fan-out reads.
        let mut cell_changes: Vec<(ClientId, (i32, i32), (i32, i32))> = Vec::new();
        // Linkdead bodies are frozen: they stay in `connections` (vulnerable)
        // but must not integrate movement, or a body that dropped mid-run would
        // keep drifting through the world for the whole linger window.
        for (client_id, conn) in connections
            .iter_mut()
            .filter(|(_, c)| c.ready && c.linkdead_since.is_none())
        {
            let dir = match conn.last_move_received {
                Some(t) if now.duration_since(t) < STALE_MOVE_THRESHOLD => {
                    conn.latest_direction
                }
                _ => Vec3f::ZERO,
            };
            // Track 6: any non-zero move auto-stands the connection. The
            // client side of regen.gd already does this for the local
            // player; the server mirrors it so a Sit intent dropped on
            // the wire doesn't leave the server thinking the player is
            // seated while they're running around.
            if dir.x != 0.0 || dir.z != 0.0 {
                conn.is_sitting = false;
            }
            // Track 6 sub-task 4c: speed buff (Spirit of Wolf, Selos'
            // Melody) multiplies MAX_MOVE_SPEED.
            // Track 6 sub-task 4d: snare multiplies effective speed
            // by (1 - snare_amount), floored at 10% so the player
            // can still inch along.
            let speed_buff = buffs::speed_mult(&conn.active_buffs);
            let snare = buffs::snare_amount(&conn.active_buffs);
            let snare_mult = (1.0 - snare).max(0.1);
            let speed = MAX_MOVE_SPEED * speed_buff * snare_mult;
            conn.pos.x += dir.x * speed * dt;
            conn.pos.z += dir.z * speed * dt;
            // Y is not integrated — server tracks only XZ; gravity is client-side.

            // Track 7: detect cell boundary crossing.
            let new_cell = aoi::cell_for(conn.pos.x, conn.pos.z);
            if new_cell != conn.aoi_cell {
                cell_changes.push((*client_id, conn.aoi_cell, new_cell));
                conn.aoi_cell = new_cell;
            }
        }

        // 5b. Track 7 — AOI cell-change fan-out. For each player who
        //     crossed a cell boundary, update the grid and fan
        //     EntitySpawn to newly-visible peers (both directions) and
        //     EntityDespawn to peers who left the neighbourhood.
        for (mover_id, old_cell, new_cell) in &cell_changes {
            let mover_entity = connections
                .get(mover_id)
                .map(|c| c.char_id as u64)
                .unwrap_or(0);
            if mover_entity == 0 {
                continue;
            }
            let (gained_cells, lost_cells) = aoi.update(mover_entity, *old_cell, *new_cell);

            // Newly visible — entities in cells that entered our neighbourhood.
            let newly_visible = aoi.entities_in_cells(gained_cells.iter());
            for &peer_entity in &newly_visible {
                if peer_entity == mover_entity {
                    continue;
                }
                // Enemies and pets are looked up first and dispatched by
                // kind rather than by id partition: the partitions stack
                // (player < ENEMY_ID_BASE < bag/corpse < PET_ID_BASE), and a
                // partition test in the wrong order once sent pets into the
                // bag arm, where they matched nothing and were skipped, so a
                // player walking into view of an existing pet never saw it.
                // An enemy or pet id with no live entry has already been
                // cleaned up and matches no arm.
                if let Some(entity) = enemies.get(&peer_entity) {
                    // The mover's own pet is skipped: they have had it all
                    // along (`pet_owner_cid`).
                    if entity.is_alive() && pet_owner_cid(entity) != Some(*mover_id) {
                        handlers::fan_out_entity_spawn(
                            &mut server,
                            std::slice::from_ref(mover_id),
                            entity,
                        );
                    }
                } else if peer_entity >= protocol::world::LOOT_BAG_ID_BASE {
                    // Loot bag OR corpse — they share the id partition (corpses
                    // mint from mint_bag_id). Seed the mover with whichever it is.
                    if let Some(bag) = loot_bags.get(&peer_entity) {
                        handlers::fan_out_loot_bag_spawn(
                            &mut server,
                            std::slice::from_ref(mover_id),
                            bag,
                        );
                    } else if let Some(corpse) = corpses.get(&peer_entity) {
                        handlers::fan_out_corpse_spawn(
                            &mut server,
                            std::slice::from_ref(mover_id),
                            corpse,
                        );
                        // Slice 2 — if the approaching player owns this corpse,
                        // privately seed its contents so they can loot it.
                        if corpse.owner_char == *mover_id as i64 {
                            handlers::send_corpse_contents(&mut server, *mover_id, corpse);
                        }
                    }
                } else if peer_entity < protocol::world::ENEMY_ID_BASE {
                    // Player — mutual EntitySpawn.
                    let peer_id = peer_entity as ClientId;
                    // Mover → peer: peer can now see the mover
                    if let Some(mover_conn) = connections.get(mover_id) {
                        if mover_conn.in_world {
                            handlers::send_entity_spawn(&mut server, peer_id, mover_conn);
                            handlers::fan_out_resources(
                                &mut server,
                                std::slice::from_ref(&peer_id),
                                mover_conn,
                            );
                            handlers::fan_out_buff_snapshot(
                                &mut server,
                                std::slice::from_ref(&peer_id),
                                mover_conn,
                            );
                        }
                    }
                    // Peer → mover: mover can now see the peer
                    if let Some(peer_conn) = connections.get(&peer_id) {
                        if peer_conn.in_world {
                            handlers::send_entity_spawn(&mut server, *mover_id, peer_conn);
                            handlers::fan_out_resources(
                                &mut server,
                                std::slice::from_ref(mover_id),
                                peer_conn,
                            );
                            handlers::fan_out_buff_snapshot(
                                &mut server,
                                std::slice::from_ref(mover_id),
                                peer_conn,
                            );
                        }
                    }
                }
            }

            // No longer visible — entities in cells that left our neighbourhood.
            let no_longer_visible = aoi.entities_in_cells(lost_cells.iter());
            for &peer_entity in &no_longer_visible {
                if peer_entity == mover_entity {
                    continue;
                }
                if peer_entity >= protocol::world::ENEMY_ID_BASE {
                    // Enemy, pet or bag — just tell the mover it's gone. Not
                    // the mover's own pet: the owner always sees it
                    // (`pet_owner_cid`).
                    let own_pet = enemies
                        .get(&peer_entity)
                        .is_some_and(|e| pet_owner_cid(e) == Some(*mover_id));
                    if !own_pet {
                        handlers::send_entity_despawn(&mut server, *mover_id, peer_entity);
                    }
                } else {
                    // Player — mutual despawn.
                    let peer_id = peer_entity as ClientId;
                    handlers::send_entity_despawn(&mut server, peer_id, mover_entity);
                    handlers::send_entity_despawn(&mut server, *mover_id, peer_entity);
                }
            }
        }

        // 5a. Track 6 sub-task 4a buff tick — process HoT / MP regen
        //     for each connection's active_buffs. Decrements
        //     remaining, applies per-tick effects, removes expired
        //     buffs, and fans BuffSnapshot when the set changes. Runs
        //     BEFORE regen so HoT increments land in the same tick as
        //     the regen-driven HealthUpdate fan-out — one ManaUpdate
        //     / HealthUpdate per affected resource per tick at most.
        let mut buff_snapshot_dirty: Vec<u64> = Vec::new();
        for conn in connections.values_mut().filter(|c| c.ready) {
            let mut snapshot_changed = false;
            let mut i = 0;
            while i < conn.active_buffs.len() {
                let buff = &mut conn.active_buffs[i];
                if buff.remaining.is_finite() {
                    buff.remaining -= dt;
                }
                let expired = buff.remaining <= 0.0 && buff.remaining.is_finite();
                match buff.effect {
                    buffs::BuffEffect::Hot { hps } => {
                        if !expired && hps > 0.0 && conn.hp < conn.max_hp {
                            buff.tick_acc += hps * dt;
                            if buff.tick_acc >= 1.0 {
                                let heal = buff.tick_acc.floor();
                                buff.tick_acc -= heal;
                                conn.hp = (conn.hp + heal).min(conn.max_hp);
                                regen::mark_dirty(conn);
                            }
                        }
                    }
                    buffs::BuffEffect::MpRegen { mps } => {
                        if !expired && mps > 0.0 && conn.mp < conn.max_mp {
                            buff.tick_acc += mps * dt;
                            if buff.tick_acc >= 1.0 {
                                let gain = buff.tick_acc.floor();
                                buff.tick_acc -= gain;
                                conn.mp = (conn.mp + gain).min(conn.max_mp);
                                regen::mark_dirty(conn);
                            }
                        }
                    }
                    buffs::BuffEffect::LichForm { lich_mp_regen } => {
                        // Lich Form is a passive toggle — regen.rs
                        // skips natural HP regen when this is
                        // present, and we add the MP/sec here.
                        if !expired && lich_mp_regen > 0.0 && conn.mp < conn.max_mp {
                            buff.tick_acc += lich_mp_regen * dt;
                            if buff.tick_acc >= 1.0 {
                                let gain = buff.tick_acc.floor();
                                buff.tick_acc -= gain;
                                conn.mp = (conn.mp + gain).min(conn.max_mp);
                                regen::mark_dirty(conn);
                            }
                        }
                    }
                    buffs::BuffEffect::StatBuff { .. } => {
                        // Stat buffs are duration-only — no per-tick
                        // effect. Deltas were applied at cast time
                        // (apply_stat_buff); the un-apply happens
                        // below in the expire branch.
                    }
                    buffs::BuffEffect::Speed { .. }
                    | buffs::BuffEffect::Haste { .. }
                    | buffs::BuffEffect::DamageShield { .. }
                    | buffs::BuffEffect::AccuracyCrit { .. }
                    | buffs::BuffEffect::Mez
                    | buffs::BuffEffect::Root
                    | buffs::BuffEffect::Snare { .. }
                    | buffs::BuffEffect::AttackSlow { .. }
                    | buffs::BuffEffect::Silence => {
                        // Track 6 sub-task 4c/4d — duration-only
                        // buffs. Effects applied at read sites
                        // (movement integration / damage-shield
                        // path / calc_swing / intent gating); the
                        // tick just decrements remaining.
                    }
                    buffs::BuffEffect::Absorb { pool } => {
                        // Absorb's "duration" is infinite by design
                        // (consumed by damage, not by time). If the
                        // pool reached zero via consume_absorb in
                        // step 4h / 4ha, the expire branch below
                        // would have removed it. This match arm is
                        // a safety check — if a stale entry with
                        // pool <= 0 lingers, force-expire it here.
                        if pool <= 0.0 {
                            // Force the buff to expire by zeroing
                            // its remaining. Next iteration removes.
                            buff.remaining = 0.0;
                            // Avoid the is_finite gate failing the
                            // expired check.
                        }
                    }
                }
                if expired {
                    // Track 6 sub-task 4b — stat buffs need their
                    // deltas undone before the entry is removed.
                    // Other effect kinds were already accounted for
                    // by the tick body.
                    if let buffs::BuffEffect::StatBuff {
                        strength, agility, intelligence, wisdom, constitution,
                        max_hp_delta, max_mp_delta,
                    } = conn.active_buffs[i].effect
                    {
                        buffs::undo_stat_deltas(
                            conn,
                            strength, agility, intelligence, wisdom, constitution,
                            max_hp_delta, max_mp_delta,
                        );
                        regen::mark_dirty(conn);
                    }
                    conn.active_buffs.remove(i);
                    snapshot_changed = true;
                } else {
                    i += 1;
                }
            }
            if snapshot_changed {
                buff_snapshot_dirty.push(conn.char_id as u64);
            }
        }
        // Fan BuffSnapshot for any connection whose buff set changed
        // this tick (expirations only — applies fanned inline at the
        // cast site). Collect first to drop the mut borrow before
        // re-borrowing immutably.
        if !buff_snapshot_dirty.is_empty() {
            let recipients: Vec<ClientId> = connections
                .iter()
                .filter(|(_, c)| c.in_world)
                .map(|(id, _)| *id)
                .collect();
            for id in buff_snapshot_dirty {
                if let Some(conn) = connections.get(&(id as ClientId)) {
                    fan_out_server_buff_snapshot(&mut server, &recipients, conn);
                }
            }
        }

        // 5a-pet. Track 13 — pet buff tick. HoT heal + stat-buff expiry
        // for each pet's active_buffs. Collect the changed pets first, then
        // fan HealthUpdate / BuffSnapshot once the mut borrow drops.
        let mut pet_buff_changed: Vec<(EntityId, bool, bool)> = Vec::new();
        for entity in enemies.values_mut() {
            if entity.active_buffs.is_empty() {
                continue;
            }
            let (hp_changed, set_changed) = entity.tick_buffs(dt);
            if hp_changed || set_changed {
                pet_buff_changed.push((entity.id, hp_changed, set_changed));
            }
        }
        if !pet_buff_changed.is_empty() {
            let recipients: Vec<ClientId> = connections
                .iter()
                .filter(|(_, c)| c.in_world)
                .map(|(id, _)| *id)
                .collect();
            for (pet_id, hp_changed, set_changed) in pet_buff_changed {
                if let Some(pet) = enemies.get(&pet_id) {
                    if hp_changed {
                        handlers::fan_out_health_update(
                            &mut server, &recipients, pet_id, pet.hp, pet.max_hp,
                        );
                    }
                    if set_changed {
                        fan_out_pet_buff_snapshot(&mut server, &recipients, pet);
                    }
                }
            }
        }

        // 5c. Camp sweep (slice B). Advance each in-progress /camp. Runs after
        //     movement integration (which clears is_sitting on a move) and after
        //     all damage application this tick, so both cancel conditions are
        //     current. A camp cancels if the player is no longer seated (covers
        //     an explicit stand AND movement) or has taken damage since it began;
        //     on cancel we fan a CampUpdate so the client hides its countdown.
        //
        //     Completion is CLIENT-DRIVEN: at CAMP_SECS the client runs the same
        //     clean logout as Quit Game (a clean Disconnect), which reaps the body
        //     and frees the account. The server does NOT force the disconnect here
        //     because a mid-world server kick has no client-side return-to-lobby
        //     path yet (it would strand the player in a frozen world). The server's
        //     job is the vulnerability window + the cancel-on-damage/move rule (the
        //     security-relevant parts); once the window elapses it just stops
        //     tracking. The client backstops itself, and if it never disconnects it
        //     simply stays in-world (it gained nothing — it sat vulnerable the whole
        //     time). The `>= start` compare catches damage applied on the very tick
        //     the camp began (same `now`); prior-tick damage has a strictly smaller
        //     Instant, so this never spuriously cancels a fresh camp.
        for (camp_cid, conn) in connections.iter_mut() {
            let Some(start) = conn.camp_since else {
                continue;
            };
            let cancelled =
                !conn.is_sitting || conn.last_damaged_at.is_some_and(|t| t >= start);
            if cancelled {
                conn.camp_since = None;
                handlers::send_camp_update(&mut server, *camp_cid, 0, false);
            } else if now.duration_since(start) >= CAMP_SECS {
                // Window elapsed; stop tracking. The client logs itself out.
                conn.camp_since = None;
                tracing::info!(char_id = conn.char_id, "camp window elapsed — client logs out");
            }
        }

        // 5a-bis. Corpse / resurrection Slice 0 — server-authoritative death
        //     detection. After all combat damage this tick, any in-world player
        //     whose hp reached zero (and hasn't already been processed) dies
        //     here: the xp penalty + de-level apply, the on-death resets run,
        //     and EntityDied fans to peers. `death_processed` keeps this from
        //     re-firing every tick while hp stays 0; the Respawn handler clears
        //     it. The client's DeathBroadcast still covers death causes the
        //     server doesn't simulate (e.g. client-side fall damage).
        let death_recipients: Vec<ClientId> = connections
            .iter()
            .filter(|(_, c)| c.in_world)
            .map(|(id, _)| *id)
            .collect();
        let mut newly_dead: Vec<EntityId> = Vec::new();
        for conn in connections.values_mut() {
            if conn.in_world && conn.hp <= 0.0 && !conn.death_processed {
                super::progression::kill_player(&mut server, conn);
                conn.death_processed = true;
                handlers::fan_out_entity_died(&mut server, &death_recipients, conn.char_id as u64);
                newly_dead.push(conn.char_id as u64);
                tracing::info!(
                    char_id = conn.char_id,
                    level = conn.level,
                    "server-detected player death",
                );
            }
        }
        // Break aggro on death. Excluding the dead from the AI target list (step
        // 4-ai) stops enemies picking them up again, but a mob mid-Chase still
        // holds them in its aggro/threat tables; wiping the entry makes it drop
        // the target immediately and leash home rather than standing over the
        // corpse. Also keeps a dead player from skewing kill credit on a mob
        // someone else finishes.
        if !newly_dead.is_empty() {
            for mob_id in wipe_hate(&mut enemies, &newly_dead) {
                handlers::fan_out_entity_target(&mut server, &in_world_recipients_now, mob_id, None);
            }
        }

        // Corpse / resurrection Slice 1 — move each freshly-dead player's gear +
        // coin onto a persisted corpse, then strip them naked. Keyed off the
        // `corpse_pending` flag `kill_player` sets, so EVERY death path leaves a
        // corpse: the server-detected sweep above AND a client-first
        // DeathBroadcast (Test Panel Trigger Death, fall damage, a linkdead body
        // that dies) handled earlier this tick. ORDER MATTERS: persist the corpse
        // FIRST so a crash before the inventory clear leaves the gear recoverable
        // on the corpse (never duped-then-lost). If save_corpse fails we skip the
        // strip entirely (player keeps their gear, no corpse). Outside the
        // values_mut loop so we can await the DB.
        let corpse_pending: Vec<i64> = connections
            .values()
            .filter(|c| c.corpse_pending)
            .map(|c| c.char_id)
            .collect();
        for cid in corpse_pending {
            let client_id = cid as ClientId;
            if let Some(c) = connections.get_mut(&client_id) {
                c.corpse_pending = false;
            }
            let (owner_name, zone, pos, items, coins, lost_xp) = match connections.get(&client_id) {
                Some(c) => (
                    c.name.clone(),
                    c.zone.clone().unwrap_or_default(),
                    c.pos,
                    c.inventory.all_stacks(),
                    c.coins,
                    c.death_lost_xp,
                ),
                None => continue,
            };
            // Always leave a corpse, even an empty one (player died naked +
            // broke). A corpse is the Cleric's resurrection anchor (Slice 3), so
            // someone who dies mid-corpse-run with nothing on them STILL needs a
            // body to be rezzed back to. An empty corpse is just a corpses row
            // with no items; it renders and decays like any other.
            let corpse_id = super::loot::mint_bag_id();
            // 1. Create the corpse AND strip the owner in ONE transaction. These
            //    are two halves of a single movement — the gear and coin leave
            //    the player and arrive on the body — so they must not be able to
            //    half-happen. Written separately (as they were), a crash between
            //    them left the corpse holding everything while the player still
            //    had it: a straight duplication, of coin especially.
            if let Err(e) = db::save_corpse(
                &pool,
                corpse_id as i64,
                cid,
                &owner_name,
                &zone,
                (pos.x, pos.y, pos.z),
                coins,
                &items,
                lost_xp,
                true, // strip the owner in the same transaction
            )
            .await
            {
                tracing::error!(char_id = cid, error = %e, "save_corpse failed; leaving inventory intact (no strip)");
                continue;
            }
            // 2. Mirror that committed state in memory and tell the client: the
            //    emptied snapshot drives the naked respawn, plus gear-free max
            //    stats and the zeroed wallet. The DB write already happened
            //    above, so there is nothing further to persist here.
            if let Some(conn) = connections.get_mut(&client_id) {
                conn.inventory.clear_all();
                inventory::recompute_equipped_stats(conn);
                conn.coins = protocol::world::Coins::ZERO;
                // Already durable via save_corpse's transaction. Clearing these
                // stops the next checkpoint rewriting rows it does not need to.
                conn.inventory_dirty = false;
                conn.coins_dirty = false;
                let entries = conn.inventory.to_snapshot_entries();
                handlers::send_inventory_snapshot(&mut server, client_id, entries);
                handlers::fan_out_resources(&mut server, std::slice::from_ref(&client_id), conn);
                handlers::send_coins_update(&mut server, client_id, protocol::world::Coins::ZERO);
            }
            // 3. Spawn the corpse: AOI + map, fan CorpseSpawn to nearby peers.
            let cell = aoi::cell_for(pos.x, pos.z);
            aoi.insert(corpse_id, cell);
            let visible = aoi.entities_visible_from(cell);
            let recipients: Vec<ClientId> = in_world_recipients_now
                .iter()
                .copied()
                .filter(|id| visible.contains(id))
                .collect();
            let stacks: Vec<super::loot::LootItemStack> = items
                .iter()
                .map(|(p, n)| super::loot::LootItemStack { item_path: p.clone(), count: *n })
                .collect();
            let corpse = super::corpses::Corpse::new(
                corpse_id, cid, owner_name, zone, pos, stacks, coins, lost_xp, false, now,
            );
            handlers::fan_out_corpse_spawn(&mut server, &recipients, &corpse);
            // Slice 2 — privately seed the owner with the corpse contents so they
            // can loot it (peers got only the render-only CorpseSpawn above).
            handlers::send_corpse_contents(&mut server, client_id, &corpse);
            corpses.insert(corpse_id, corpse);
            tracing::info!(char_id = cid, corpse_id, item_stacks = items.len(), "corpse created");
        }

        // 5b. Track 6 regen tick — HP/MP/Stamina recovery, then fan
        //     `HealthUpdate` / `ManaUpdate` / `StaminaUpdate` for any
        //     connection whose values crossed the broadcast threshold.
        //     Runs for every ready connection (regardless of in_world)
        //     so a player in the lobby keeps regenerating between
        //     sessions, but only in_world peers receive the fan-outs.
        let mut regen_fanouts: Vec<u64> = Vec::new();
        for conn in connections.values_mut().filter(|c| c.ready) {
            let result = regen::tick_one(conn, dt, now);
            if result.hp_fanout || result.mp_fanout || result.stamina_fanout {
                regen_fanouts.push(conn.char_id as u64);
            }
            // Meditate skill-up: while actually meditating (seated, out of combat,
            // mana below full) advance the `meditate` casting skill once per ~6 s
            // med-tick — server-authoritative, gated to that cadence via
            // `last_meditate_at`. `try_advance` returns None for classes that can't
            // train it (cap 0), and drives the sitting MP regen in regen.rs. Fans
            // a private SkillProgressUpdate on a gain so the client mirror keeps up.
            if conn.mp < conn.max_mp && regen::sitting_bonus_applies(conn, now) {
                let due = conn
                    .last_meditate_at
                    .map_or(true, |t| now.duration_since(t) >= Duration::from_secs(6));
                if due {
                    conn.last_meditate_at = Some(now);
                    if let Some(new_score) =
                        skills::try_advance(conn, skills::Skill::Casting, "meditate")
                    {
                        let cid = conn.char_id as ClientId;
                        handlers::send_skill_progress_update(
                            &mut server,
                            cid,
                            skills::Skill::Casting.as_protocol(),
                            "meditate".to_string(),
                            new_score,
                        );
                    }
                }
            }
        }
        if !regen_fanouts.is_empty() {
            let recipients: Vec<ClientId> = connections
                .iter()
                .filter(|(_, c)| c.in_world)
                .map(|(id, _)| *id)
                .collect();
            if !recipients.is_empty() {
                for id in regen_fanouts {
                    if let Some(conn) = connections.get(&(id as ClientId)) {
                        handlers::fan_out_resources(&mut server, &recipients, conn);
                    }
                }
            }
        }

        // 6. Position fan-out. Track 7: AOI-filtered. Each sender's
        //    position goes only to in_world players whose AOI cell is
        //    within the 3×3 neighbourhood of the sender's cell.
        //    Self-broadcast is preserved (sender is in its own
        //    neighbourhood) — Track 2 snap-or-lerp still needs it.
        //    Encode each sender once; clone bytes only to visible peers.
        let in_world_ids: Vec<ClientId> = connections
            .iter()
            .filter(|(_, c)| c.in_world)
            .map(|(id, _)| *id)
            .collect();
        for sender_id in &in_world_ids {
            let Some(sender) = connections.get(sender_id) else {
                continue;
            };
            let Some(bytes) = handlers::build_position_msg(sender) else {
                continue;
            };
            let visible = aoi.entities_visible_from(sender.aoi_cell);
            for recipient_id in &in_world_ids {
                if visible.contains(recipient_id) {
                    server.send_message(*recipient_id, CHANNEL_POSITION, bytes.clone());
                }
            }
        }

        // 6b. Enemy and pet Position fan-out, AOI-filtered: only players
        //     whose cell is in the 3×3 neighbourhood of the entity's cell
        //     receive it. Entities ARE in the AoiGrid (inserted on spawn,
        //     moved by the AI pass, removed on cleanup); the cell is read
        //     from entity.pos because that is what the grid was keyed on.
        //     Gated on change-or-keepalive: an entity that has not moved or
        //     turned since its last send fans only every
        //     ENEMY_POSITION_KEEPALIVE, not every tick. The counter feeds
        //     the "enemy position fan" log line so the stream is measured
        //     rather than eyeballed.
        if in_world_ids.is_empty() {
            // Nobody to fan to: keep the report window anchored to the next
            // stretch with players in it instead of spanning the idle time.
            enemy_pos_sent = 0;
            last_fan_report = now;
        } else {
            for entity in enemies.values_mut() {
                if !entity.is_alive() || !entity.position_broadcast_due(now) {
                    continue;
                }
                // Audience first, with the allocation-free cell test (each
                // connection carries its cell): an entity nobody is near
                // neither marks nor encodes, so the sequence really does
                // advance only on a send. A pet's owner is always in the
                // audience: the owner sees their own pet wherever it is
                // (`pet_owner_cid`).
                let enemy_cell = aoi::cell_for(entity.pos.x, entity.pos.z);
                let owner_cid = pet_owner_cid(entity);
                let can_see = |cid: &ClientId| {
                    owner_cid == Some(*cid)
                        || connections
                            .get(cid)
                            .is_some_and(|c| aoi.can_see(c.aoi_cell, enemy_cell))
                };
                if !in_world_ids.iter().any(|cid| can_see(cid)) {
                    continue;
                }
                // Mark before building: the message carries the advanced
                // sequence and the snapshotted pos / yaw.
                entity.mark_position_broadcast(now);
                let Some(bytes) = handlers::build_enemy_position_msg(entity) else {
                    continue;
                };
                for recipient_id in in_world_ids.iter().filter(|cid| can_see(cid)) {
                    server.send_message(*recipient_id, CHANNEL_POSITION, bytes.clone());
                    enemy_pos_sent += 1;
                }
            }
            if now.duration_since(last_fan_report) >= ENEMY_FAN_REPORT_INTERVAL {
                let enemies_alive = enemies.values().filter(|e| e.is_alive()).count();
                tracing::info!(
                    sent = enemy_pos_sent,
                    window_secs = now.duration_since(last_fan_report).as_secs(),
                    enemies_alive,
                    players_in_world = in_world_ids.len(),
                    "enemy position fan"
                );
                enemy_pos_sent = 0;
                last_fan_report = now;
            }
        }

        // 6-ter. PD_W0024 — flush quest state touched this tick (kill
        //    increments, accepts, abandons — all sync, they only mark
        //    `quests_dirty`). Drained EVERY tick (not the 60s checkpoint:
        //    losing counted kills to a crash would be worse than the extra
        //    writes) and RECONCILED to the DB: upsert the progress if the quest
        //    is still active, else DELETE the row (abandoned). One write per
        //    touched quest per tick, so a forged accept/abandon storm can't
        //    amplify into an awaited write per message. A failed write
        //    re-queues for next tick.
        for conn in connections.values_mut() {
            if conn.quests_dirty.is_empty() {
                continue;
            }
            let dirty: Vec<String> = conn.quests_dirty.drain().collect();
            for quest_id in dirty {
                let result = match conn.active_quests.get(&quest_id) {
                    Some(progress) => {
                        db::save_quest_progress(&pool, conn.char_id, &quest_id, progress).await
                    }
                    None => db::delete_active_quest(&pool, conn.char_id, &quest_id).await,
                };
                if let Err(e) = result {
                    tracing::error!(char_id = conn.char_id, quest_id = %quest_id, error = %e,
                        "quest state flush failed — re-queueing for next tick");
                    conn.quests_dirty.insert(quest_id);
                }
            }
        }

        // 7. Periodic checkpoint.
        if now.duration_since(last_checkpoint) >= CHECKPOINT_INTERVAL {
            let mut dirty: Vec<&mut PerConnection> = connections.values_mut().collect();
            persistence::checkpoint_dirty(&pool, &mut dirty).await;
            last_checkpoint = now;
        }

        // 7-ter. The world clock. Everyone in the world is told the hour on a
        //    slow cadence; clients run their own clock at the same rate in
        //    between, so this only corrects drift. Joiners are seeded at
        //    EnterWorld. Reliable channel: a lost update would leave one
        //    client's sky wrong for a whole interval.
        if now.duration_since(last_time_broadcast) >= clock::BROADCAST_INTERVAL {
            handlers::fan_out_time_of_day(&mut server, &in_world_ids, clock::current_hour());
            last_time_broadcast = now;
        }

        // 7-bis. Ban sweep. Login, session redemption and world connect all
        //    refuse a banned account, but none of them touches a character
        //    that is already in the world, so a ban used to need a server
        //    restart (and its 60 s of lost progress for everyone) to remove
        //    a player who was online. Every BAN_SWEEP_INTERVAL, ask which
        //    accounts are banned and kick any connection that belongs to one.
        //
        //    The query runs on its own task and answers over a channel: this
        //    loop is the whole simulation, and a read that waited on a busy
        //    database or an exhausted pool would freeze every player six
        //    times a minute for a result that is almost always empty. A
        //    failed query reports nothing banned; the other three gates still
        //    hold and the next sweep retries.
        if now.duration_since(last_ban_sweep) >= BAN_SWEEP_INTERVAL {
            last_ban_sweep = now;
            if !connections.is_empty() && !ban_sweep_in_flight {
                ban_sweep_in_flight = true;
                let (sweep_pool, tx) = (pool.clone(), ban_tx.clone());
                tokio::spawn(async move {
                    let banned = match db::banned_account_ids(&sweep_pool).await {
                        Ok(ids) => ids,
                        Err(e) => {
                            tracing::warn!(error = %e, "ban sweep query failed — will retry");
                            Vec::new()
                        }
                    };
                    let _ = tx.send(banned);
                });
            }
        }
        while let Ok(banned) = ban_rx.try_recv() {
            ban_sweep_in_flight = false;
            for (client_id, conn) in connections.iter_mut() {
                if conn.kicked_at.is_none()
                    && conn.linkdead_since.is_none()
                    && banned.contains(&conn.account_id)
                {
                    tracing::info!(
                        account_id = conn.account_id,
                        char_id = conn.char_id,
                        "banned account kicked from the world"
                    );
                    handlers::send_kick(
                        &mut server,
                        *client_id,
                        KickCode::BannedNow,
                        "This account is banned.",
                    );
                    // Stamped at the send, not at tick start: the grace before
                    // the transport is dropped (step 4, KICK_FLUSH_GRACE) is
                    // for THIS message to arrive, so it runs from here. The
                    // connection's messages are ignored from now on
                    // (`handle_message`).
                    conn.kicked_at = Some(Instant::now());
                }
            }
        }

        // 8. Push outbound packets to the network.
        transport.send_packets(&mut server);

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

/// How close (metres) a resurrection's caster must be to the corpse.
const RES_CAST_RANGE: f32 = 30.0;

/// Cast eligibility, decided BEFORE any side effect. Returns the refusal line,
/// or `None` when the cast may proceed.
///
/// Two reasons this is a pre-flight rather than checks inside the
/// per-target-type arms:
///
/// 1. **Reach.** ALLY spells (heals and buffs, on a player or a pet) and ENEMY
///    spells against a player had no range check at all, so a modified client
///    could heal a friend in a dungeon from town, or nuke a PvP-flagged player
///    from across the world, by naming an id. Every targeted cast now needs
///    its target within `RANGED_ATTACK_RANGE`, the one spell reach until
///    spells carry their own. Written keep-only-when-in-reach, so a
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
/// arm. The arms keep their own checks as a second line; after this they
/// should not fire.
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
) -> Option<String> {
    use protocol::world::{ENEMY_ID_BASE, LOOT_BAG_ID_BASE, PET_ID_BASE};
    let caster = connections.get(&caster_cid)?;
    // Friendly spells reach further than hostile ones (spell batch step 1);
    // binding a group member is a friendly cast too.
    let reach = match spell.target_type.as_str() {
        "ALLY" | "PET_HEAL" | "BIND" => super::FRIENDLY_SPELL_RANGE,
        _ => RANGED_ATTACK_RANGE,
    };
    let in_reach = |pos: Vec3f| pos.distance_to(caster_pos) <= reach;
    let unreachable = || Some(TARGET_UNREACHABLE.to_string());
    let no_effect = || Some(NO_EFFECT_YET.to_string());
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
                return Some("You need a target for that spell.".to_string());
            };
            if target < ENEMY_ID_BASE {
                // A player: the PvP path.
                let target_cid = target as ClientId;
                if target_cid == caster_cid {
                    return Some("You cannot cast that on yourself.".to_string());
                }
                let Some(t) = player_in_reach(target_cid) else {
                    return unreachable();
                };
                if !attackable(t) {
                    return Some("You cannot attack that player.".to_string());
                }
                if t.hp <= 0.0 {
                    return Some("That target is no longer here.".to_string());
                }
                return None;
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
                    return Some("You cannot attack that pet.".to_string());
                }
            }
            // One attack slow at a time, strongest wins (spell batch step 1):
            // a weaker slow on a target that already carries a stronger one
            // is refused here, at no cost, rather than replacing it.
            if spell.attack_slow_amount > 0.0
                && e.attack_slow_pct() > (spell.attack_slow_amount.clamp(0.0, 1.0) * 100.0) as u8
            {
                return Some("A stronger slow already holds that target.".to_string());
            }
            None
        }
        "ALLY" => {
            let raw = target_id.unwrap_or(0);
            if raw == 0 || raw == caster_id {
                return None; // self
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
                        return Some("You cannot heal that pet.".to_string());
                    }
                }
                return None;
            }
            if raw >= LOOT_BAG_ID_BASE {
                return Some("Cannot heal that target.".to_string());
            }
            if raw >= ENEMY_ID_BASE {
                return Some("Cannot heal enemies.".to_string());
            }
            let target_cid = raw as ClientId;
            let Some(t) = player_in_reach(target_cid) else {
                return unreachable();
            };
            if t.hp <= 0.0 {
                return Some("That target is no longer here.".to_string());
            }
            if attackable(t) && !group_manager.same_group(caster_cid, target_cid) {
                return Some("You cannot heal that player.".to_string());
            }
            None
        }
        "PET_CHARM" => {
            let Some(target) = target_id else {
                return Some("You need a target for that spell.".to_string());
            };
            if !(ENEMY_ID_BASE..LOOT_BAG_ID_BASE).contains(&target) {
                return Some("You cannot charm that.".to_string());
            }
            match entity_in_reach(target) {
                Some(e) if !e.is_pet() => None,
                _ => unreachable(),
            }
        }
        "CORPSE" => {
            let Some(corpse_id) = target_id else {
                return Some("No corpse targeted.".to_string());
            };
            let Some(corpse) = corpses.get(&corpse_id) else {
                return Some("That is not a corpse.".to_string());
            };
            if !(corpse.pos.distance_to(caster_pos) <= RES_CAST_RANGE) {
                return Some("You are too far from the corpse.".to_string());
            }
            if corpse.resurrected {
                return Some("That corpse has already been resurrected.".to_string());
            }
            // The owner must be in the world to receive the offer.
            let owner_cid = corpse.owner_char as ClientId;
            if !connections.get(&owner_cid).is_some_and(|c| c.in_world) {
                return Some("Their spirit is not present.".to_string());
            }
            None
        }
        "AOE" => {
            if spell.aoe_radius > 0.0 {
                None
            } else {
                no_effect()
            }
        }
        "PET_SUMMON" => {
            let known = !spell.pet_type.is_empty()
                && pet_templates::scaled(&spell.pet_type, caster.level.max(1) as u32, 0)
                    .is_some();
            if known {
                None
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
                return None;
            }
            let target_cid = raw as ClientId;
            if !group_manager.same_group(caster_cid, target_cid) {
                return Some("You can only bind yourself or a group member.".to_string());
            }
            let Some(t) = player_in_reach(target_cid) else {
                return unreachable();
            };
            if t.hp <= 0.0 || t.death_processed {
                return Some("That target is no longer here.".to_string());
            }
            if super::safe_areas::containing(t.pos).is_none() {
                return Some("You can only bind another in a safe place, such as the town.".to_string());
            }
            None
        }
        "PORT" => {
            // Gate (port = "bind") always has somewhere to go, since every
            // character is bound from birth; a safe-area port needs one safe
            // area to exist. Anything else is a port the server has no
            // destination for (the zone ports) and costs nothing.
            match spell.port.as_str() {
                "bind" => None,
                "safe" if super::safe_areas::nearest_arrival(caster_pos).is_some() => None,
                _ => no_effect(),
            }
        }
        "PET_HEAL" => {
            // The caster's own pet, whatever is targeted (the spell names it).
            // Having no pet says nothing about anyone else, so it can be
            // said plainly.
            let own_pet = enemies
                .values()
                .find(|e| e.owner == Some(caster_id) && e.is_alive());
            match own_pet {
                None => Some("You have no pet to mend.".to_string()),
                Some(p) if in_reach(p.pos) => None,
                Some(_) => unreachable(),
            }
        }
        "SELF" => None,
        // No arm exists for anything else: a target type new to the data.
        _ => no_effect(),
    }
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
            if entity.target == Some(*id) {
                entity.target = None;
                dropped.push(entity.id);
            }
        }
    }
    dropped
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

/// Project one trade side's offers into the wire payload by re-reading each
/// referenced slot from the OWNER's live inventory (escrow-by-locking: the
/// session stores references, never item copies).
fn trade_side_payload(
    connections: &HashMap<ClientId, PerConnection>,
    side: &super::trade::TradeSide,
) -> (Vec<(String, u32)>, protocol::world::Coins) {
    let mut slots = vec![(String::new(), 0u32); super::trade::TRADE_SLOTS];
    if let Some(conn) = connections.get(&side.cid) {
        for (i, offer) in side.offers.iter().enumerate() {
            if let Some((loc, slot, offered_count)) = offer {
                // Path from the live slot; count from the OFFER snapshot —
                // what is displayed is what commit will verify and hand over.
                if let Some((path, _)) = conn.inventory.peek_at(loc, *slot) {
                    slots[i] = (path, *offered_count);
                }
            }
        }
    }
    (slots, side.coins)
}

/// Fan BOTH parties the full state of BOTH sides plus the accept flags —
/// after any edit. Full-state per update, not deltas: a tiny payload with
/// no desync class.
fn fan_trade_state(
    server: &mut RenetServer,
    connections: &HashMap<ClientId, PerConnection>,
    session: &super::trade::TradeSession,
) {
    for (me, them) in [(&session.a, &session.b), (&session.b, &session.a)] {
        let (my_slots, my_coins) = trade_side_payload(connections, me);
        let (their_slots, their_coins) = trade_side_payload(connections, them);
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

    // Melee swing-rate limit: the per-hand minimum interval the server allows
    // between same-hand swings. It must (a) sit comfortably BELOW the fastest a
    // legit fully-hasted client swings for each weapon (so no false positives),
    // and (b) never fall below the absolute hard floor. Mirror of combat.gd
    // pacing (weapon_delay x offhand-mult x (1-haste), maxf 0.5).
    #[test]
    fn swing_rate_min_interval_is_below_client_fastest_but_above_floor() {
        use super::{
            min_swing_interval_secs, HARD_MIN_SWING_SECS, MAX_MODELED_HASTE, OFFHAND_DELAY_MULT,
        };
        let approx = |a: f32, b: f32| (a - b).abs() < 1e-4;
        // Default 2.0 weapon, main hand: 2.0*0.5=1.0 fastest legit, minus 0.35 grace = 0.65.
        assert!(approx(min_swing_interval_secs(2.0, false), 0.65));
        // Off hand 2.0: 2.0*1.5*0.5=1.5, minus grace = 1.15.
        assert!(approx(min_swing_interval_secs(2.0, true), 1.15));
        // Slow war axe 3.2 main: 3.2*0.5=1.6, minus grace = 1.25.
        assert!(approx(min_swing_interval_secs(3.2, false), 1.25));
        // The gate must ALWAYS be <= the client's fastest legit same-hand interval
        // (weapon_delay * hand_mult * (1-max_haste), floored 0.5 client-side) so a
        // fully-hasted legit swing is never rejected. Sweep the real range PLUS
        // hypothetical fast weapons (<2.0) where the client's 0.5s floor and the
        // server's 0.4s floor diverge most.
        for &delay in &[1.0_f32, 1.4, 2.0, 2.5, 2.8, 3.0, 3.2] {
            for &off in &[false, true] {
                let hand_mult = if off { OFFHAND_DELAY_MULT } else { 1.0 };
                let client_fastest = (delay * hand_mult * (1.0 - MAX_MODELED_HASTE)).max(0.5);
                assert!(
                    min_swing_interval_secs(delay, off) <= client_fastest,
                    "gate {} must be <= client fastest {} (delay {delay}, off {off})",
                    min_swing_interval_secs(delay, off),
                    client_fastest
                );
            }
        }
        // Never below the absolute floor, even for an implausibly fast weapon.
        assert!(min_swing_interval_secs(0.1, false) >= HARD_MIN_SWING_SECS);
        assert!(min_swing_interval_secs(0.0, true) >= HARD_MIN_SWING_SECS);
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
