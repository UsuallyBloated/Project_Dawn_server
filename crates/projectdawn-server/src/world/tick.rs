//! 20 Hz tick scheduler. Owns the connection map exclusively (no locking)
//! and drives both the renet transport and the application-layer message
//! pipeline.

use super::{
    aoi::{self, AoiGrid},
    buffs::{self, ActiveBuff},
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
    ATTACK_RANGE_TOLERANCE, CHANNEL_POSITION, CHANNEL_SYSTEM, CHECKPOINT_INTERVAL,
    CORPSE_LINGER_SECS, LOOT_BAG_LINGER_SECS, LOOT_PICKUP_RANGE, MAX_MOVE_SPEED,
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
    weapon_path: String,
    is_offhand: bool,
    dmg_type: protocol::world::DamageType,
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
fn roll_cast_interrupt(conn: &mut PerConnection) -> InterruptOutcome {
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
    caster_id: u64,
    target_id: EntityId,
    spell: &spells::Spell,
    dmg_type: DamageType,
    now: Instant,
) -> bool {
    let (died, credit_id_opt, mob_xp, death_pos, mob_name, damage_done) = {
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
            let mob_xp = entity.mob.xp;
            let death_pos = entity.pos;
            let mob_name = entity.mob.name.clone();
            (true, credit_id_opt, mob_xp, death_pos, mob_name, damage_done)
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
            (false, None, 0, entity.pos, String::new(), damage_done)
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
        if let Some(credit_id) = credit_id_opt {
            if mob_xp > 0 {
                let cid = credit_id as ClientId;
                if connections.contains_key(&cid) {
                    handlers::send_xp_gained(server, cid, mob_xp);
                }
            }
        }
        if let Some(items) = loot::roll_for_mob(&mob_name) {
            let bag = LootBag::new(death_pos, items, now);
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
                                conn.aoi_cell = aoi::cell_for(conn.pos.x, conn.pos.z);
                                conn.inventory = inventory::PlayerInventory::from_rows(&inv_rows);
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

                    // Track 7: remove leaver from the AOI grid BEFORE
                    // computing recipients so entities_visible_from gives
                    // the correct set of peers who could see this player.
                    // Send EntityDespawn only to that AOI-visible set.
                    let despawn_info = connections
                        .get(&client_id)
                        .filter(|c| c.in_world)
                        .map(|c| (c.char_id as u64, c.aoi_cell));
                    if let Some((entity_id, leaver_cell)) = despawn_info {
                        aoi.remove(entity_id, leaver_cell);
                        let visible_peers = aoi.entities_visible_from(leaver_cell);
                        let peer_ids: Vec<ClientId> = connections
                            .iter()
                            .filter(|(id, c)| {
                                **id != client_id
                                    && c.in_world
                                    && visible_peers.contains(*id)
                            })
                            .map(|(id, _)| *id)
                            .collect();
                        for peer_id in peer_ids {
                            handlers::send_entity_despawn(
                                &mut server,
                                peer_id,
                                entity_id,
                            );
                        }

                        // Track 11 — leaver's pet (if any) dies with
                        // them. Drop from AOI + enemies map, fan
                        // EntityDespawn to peers who could see the
                        // pet's cell. Future work: hand off pets on
                        // zone change rather than instant despawn.
                        let owned_pets: Vec<(EntityId, Vec3f)> = enemies
                            .iter()
                            .filter(|(_, e)| e.owner == Some(entity_id))
                            .map(|(id, e)| (*id, e.pos))
                            .collect();
                        for (pet_id, pet_pos) in owned_pets {
                            let pet_cell = aoi::cell_for(pet_pos.x, pet_pos.z);
                            aoi.remove(pet_id, pet_cell);
                            let pet_visible = aoi.entities_visible_from(pet_cell);
                            let pet_recipients: Vec<ClientId> = connections
                                .iter()
                                .filter(|(id, c)| {
                                    **id != client_id
                                        && c.in_world
                                        && pet_visible.contains(*id)
                                })
                                .map(|(id, _)| *id)
                                .collect();
                            for peer_id in pet_recipients {
                                handlers::send_entity_despawn(
                                    &mut server,
                                    peer_id,
                                    pet_id,
                                );
                            }
                            enemies.remove(&pet_id);
                            tracing::info!(
                                owner = entity_id,
                                pet_id,
                                "pet despawned on owner disconnect"
                            );
                        }
                    }

                    // Track 6 sub-task 5 — remove the leaver from
                    // their group. If the group dissolves (one
                    // member left), notify them too. The rest of the
                    // roster gets a fresh GroupRoster.
                    if let Some((gid, remaining, dissolved)) = group_manager.leave(client_id) {
                        if dissolved {
                            // Group dissolved. Survivors (0 or 1) get
                            // an empty roster so their HUD clears.
                            // The leaver is the disconnecting client;
                            // their transport is already torn down.
                            for m in &remaining {
                                handlers::fan_group_roster(
                                    &mut server,
                                    std::slice::from_ref(m),
                                    gid,
                                    *m,
                                    Vec::new(),
                                );
                            }
                        } else {
                            // Re-fetch the group with name lookups
                            // for the survivor fan-out.
                            if let Some(g) = group_manager.groups.get(&gid) {
                                let members_with_names: Vec<(u64, String)> = g.members.iter()
                                    .filter_map(|m| connections.get(m).map(|c| (*m, c.name.clone())))
                                    .collect();
                                let recipients: Vec<ClientId> = g.members.clone();
                                handlers::fan_group_roster(
                                    &mut server,
                                    &recipients,
                                    gid,
                                    g.leader,
                                    members_with_names,
                                );
                            }
                        }
                    }

                    if let Some(mut conn) = connections.remove(&client_id) {
                        // One last save for the road. Failure is non-fatal —
                        // worst case the player rolls back to the last 60 s
                        // checkpoint.
                        if conn.is_dirty_for_persist() {
                            let zone = conn.zone.clone();
                            if let Err(e) = db::checkpoint_position(
                                &pool,
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
        // Track 6 sub-task 5 — group intents buffered for the
        // post-dispatch sweep. The sweep needs the full connections
        // map (to resolve names to ids + fan rosters to multiple
        // members), so we can't process inline in handle_message.
        struct GroupInviteI { inviter: u64, target_name: String }
        struct GroupAcceptI { invitee: u64, from: u64 }
        struct GroupLeaveI { member: u64 }
        struct GroupKickI { leader: u64, target_name: String }
        let mut group_invite_intents: Vec<GroupInviteI> = Vec::new();
        let mut group_accept_intents: Vec<GroupAcceptI> = Vec::new();
        let mut group_leave_intents: Vec<GroupLeaveI> = Vec::new();
        let mut group_kick_intents: Vec<GroupKickI> = Vec::new();
        // Track 5 sub-task 4 — player → server loot pickup intents.
        // Verbatim queue; sub-task 4 is FFA loot so order matters for
        // contested bags (first arrival wins the slot).
        let mut loot_intents: Vec<LootIntent> = Vec::new();
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
                            weapon_path,
                            is_offhand,
                            dmg_type,
                        } => {
                            attack_intents.push(AttackIntent {
                                attacker,
                                target_id,
                                weapon_path,
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
                            cast_spell_intents.push(CastSpellIntent {
                                caster,
                                spell_name,
                                target_id,
                                cast_name_at_dispatch,
                                cast_set_at_at_dispatch,
                                cast_start_pos_at_dispatch,
                            });
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
                        Outcome::Continue => {}
                    }
                }
            }
        }

        // 4. App-layer heartbeat timeout — catches frozen game windows that
        //    transport-level keepalive doesn't notice.
        for (client_id, conn) in connections.iter() {
            if conn.is_app_idle(now) {
                tracing::info!(
                    char_id = conn.char_id,
                    "app-layer heartbeat timeout — disconnecting"
                );
                to_disconnect.push(*client_id);
            }
        }

        for client_id in &to_disconnect {
            server.disconnect(*client_id);
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
            // Track 18.1 — seed the new joiner with their three
            // passive skill score maps (weapon / armor / casting).
            // Without this the client autoloads sit at uninitialized
            // dicts and the character window renders zeros until the
            // first advance fires.
            if let Some(new_conn) = connections.get(new_id) {
                handlers::send_skill_progress_snapshot(&mut server, *new_id, new_conn);
            }
            // Track 5 sub-task 1B — seed the new joiner with alive enemies
            // in their AOI neighbourhood. Track 7: filter by aoi.can_see
            // so only nearby enemies are seeded on enter-world.
            let joiner_cell = connections.get(new_id).map(|c| c.aoi_cell).unwrap_or((0, 0));
            for entity in enemies.values() {
                if !entity.is_alive() {
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
        // in-world, pack their paperdoll slot map into `(slot, item_path)`
        // pairs and send back to the inspector only. Empty result for
        // unknown / offline targets — client renders "—" everywhere.
        for (inspector_id, target_char_id) in inspect_intents.drain(..) {
            if to_disconnect.contains(&inspector_id) {
                continue;
            }
            let target = connections
                .iter()
                .find(|(_, c)| c.char_id == target_char_id && c.in_world);
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
            let beast_master_summons: Vec<(EntityId, Vec3f)> = newly_in_world
                .iter()
                .filter_map(|cid| {
                    let conn = connections.get(cid)?;
                    if conn.class.eq_ignore_ascii_case("Beast Master") && conn.in_world {
                        Some((conn.char_id as u64, conn.pos))
                    } else {
                        None
                    }
                })
                .collect();
            for (owner_id, caster_pos) in beast_master_summons {
                let Some(template) = pet_templates::lookup("warder") else { continue };
                let spawn_pos = Vec3f {
                    x: caster_pos.x + 1.5,
                    y: caster_pos.y,
                    z: caster_pos.z,
                };
                let pet_id = summon_pet_for_owner(
                    &mut server,
                    &in_world_recipients_now,
                    &mut enemies,
                    &mut aoi,
                    owner_id,
                    spawn_pos,
                    template,
                    1.0,
                    now,
                );
                tracing::info!(owner = owner_id, pet_id, "Beast Master warder auto-summoned");
            }
        }

        // 4g3. Track 12 Piece B — warder respawn sweep. Beast Masters
        //      whose warder died get a fresh one at 30% HP after
        //      WARDER_RETREAT_SECS (set on death; checked here).
        //      Collected then drained so we don't overlap a
        //      `connections` borrow with the `enemies` mutation in
        //      the helper.
        let due_warder_respawns: Vec<(EntityId, Vec3f)> = connections
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
                    Some((c.char_id as u64, c.pos))
                } else {
                    None
                }
            })
            .collect();
        for (owner_id, caster_pos) in due_warder_respawns {
            let Some(template) = pet_templates::lookup("warder") else { continue };
            let spawn_pos = Vec3f {
                x: caster_pos.x + 1.5,
                y: caster_pos.y,
                z: caster_pos.z,
            };
            let pet_id = summon_pet_for_owner(
                &mut server,
                &in_world_recipients_now,
                &mut enemies,
                &mut aoi,
                owner_id,
                spawn_pos,
                template,
                0.3,
                now,
            );
            if let Some(conn) = connections.get_mut(&(owner_id as ClientId)) {
                conn.warder_respawn_at = None;
            }
            tracing::info!(owner = owner_id, pet_id, "warder respawned after retreat");
        }

        // 4g4. Track 12 Piece C — charm decay sweep. Pets whose
        //      charm_expires_at has passed despawn cleanly ("mob
        //      runs away" — no death broadcast, no loot). Collect
        //      then drain to avoid a `enemies` iter+mut overlap.
        let expired_charms: Vec<(EntityId, Vec3f)> = enemies
            .iter()
            .filter_map(|(id, e)| {
                let exp = e.charm_expires_at?;
                if now.duration_since(exp).as_secs_f32() >= 0.0 {
                    Some((*id, e.pos))
                } else {
                    None
                }
            })
            .collect();
        for (pet_id, pet_pos) in expired_charms {
            let cell = aoi::cell_for(pet_pos.x, pet_pos.z);
            aoi.remove(pet_id, cell);
            let visible = aoi.entities_visible_from(cell);
            for &recipient in &in_world_recipients_now {
                if visible.contains(&recipient) {
                    handlers::send_entity_despawn(&mut server, recipient, pet_id);
                }
            }
            enemies.remove(&pet_id);
            tracing::info!(pet_id, "charm expired — pet released");
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
                let Some(attacker_conn) = connections.get(&attacker_cid) else {
                    // Attacker disconnected between sending and apply.
                    continue;
                };
                let attacker_pos = attacker_conn.pos;
                let attacker_zone = attacker_conn.zone.clone();
                // Track 6 sub-task 2: server computes the damage roll.
                // Client-supplied amount is ignored — even a malicious
                // client can't claim 999 damage anymore.
                let swing = combat::calc_swing(
                    attacker_conn,
                    &intent.weapon_path,
                    intent.is_offhand,
                );

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
                                    &intent.weapon_path,
                                ) {
                                    Some(w) if w.is_ranged => RANGED_ATTACK_RANGE,
                                    _ => 3.0 * ATTACK_RANGE_TOLERANCE,
                                }
                        })
                        .unwrap_or(false);
                    if !allowed_pvp || !target_in_range_alive {
                        if !allowed_pvp {
                            tracing::debug!(
                                attacker = intent.attacker,
                                target = intent.target_id,
                                "PvP not authorized, fanning Miss"
                            );
                        }
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
                                let outcome = roll_cast_interrupt(att);
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
                        let interrupt_outcome = roll_cast_interrupt(tc);
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
                let allowed = match items::lookup(&intent.weapon_path) {
                    Some(w) if w.is_ranged => RANGED_ATTACK_RANGE,
                    _ => entity.melee_range() * ATTACK_RANGE_TOLERANCE,
                };
                if dist > allowed {
                    tracing::debug!(
                        attacker = intent.attacker,
                        target = intent.target_id,
                        dist,
                        allowed,
                        weapon = %intent.weapon_path,
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
                let weapon_skill_key: String = items::lookup(&intent.weapon_path)
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
                        let base_xp = entity.mob.xp;
                        if base_xp > 0 {
                            // Track 6 sub-task 5 — group XP split.
                            // Killer's group (if any): boost base by
                            // GROUP_XP_BONUS and divide evenly among
                            // online members. Solo killer: full base
                            // XP. Mirrors GroupManager.distribute_kill_xp
                            // semantics from the legacy enet path.
                            let credit_cid = credit_id as ClientId;
                            let online_members: Vec<ClientId> =
                                match group_manager.group_of(credit_cid) {
                                    Some(g) => g.members.iter()
                                        .filter(|m| connections.contains_key(m))
                                        .copied()
                                        .collect(),
                                    None => vec![credit_cid],
                                };
                            let pool = if online_members.len() > 1 {
                                ((base_xp as f32) * (1.0 + groups::GROUP_XP_BONUS)) as i32
                            } else {
                                base_xp
                            };
                            let per_member = pool / online_members.len() as i32;
                            let per_member = per_member.max(1);
                            for m in &online_members {
                                if connections.contains_key(m) {
                                    handlers::send_xp_gained(&mut server, *m, per_member);
                                }
                            }
                            tracing::info!(
                                killer = credit_id,
                                mob = %entity.mob.name,
                                base_xp,
                                pool,
                                per_member,
                                members = online_members.len(),
                                "kill credit granted"
                            );
                        }
                    }
                    // Roll loot from the mob's archetype table; spawn
                    // a server-owned bag at the death pos if any
                    // stacks landed. Empty rolls produce no bag at all
                    // (matches the GDScript behaviour where the local
                    // Loot autoload simply returns without instantiating
                    // a node).
                    if let Some(items) = loot::roll_for_mob(&entity.mob.name) {
                        let stacks_for_log = items.len();
                        let bag = LootBag::new(entity.pos, items, now);
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
                    continue;
                };
                // Track 10 — cast-time gate. Instant casts (cast_time
                // == 0) skip; for timed casts we require a matching
                // CastStartBroadcast on file with enough wall time
                // elapsed. The 100 ms tolerance absorbs network jitter
                // — packet loss can push arrival past the cast bar
                // ending, but a forged CastSpell sent the same tick as
                // CastStart will fall well short.
                if spell.cast_time > 0.0 {
                    const CAST_TOLERANCE_MS: u128 = 100;
                    let required_ms = (spell.cast_time * 1000.0) as u128;
                    let name_matches = intent.cast_name_at_dispatch == spell.name;
                    let elapsed_ok = intent
                        .cast_set_at_at_dispatch
                        .map(|set_at| {
                            now.duration_since(set_at).as_millis() + CAST_TOLERANCE_MS
                                >= required_ms
                        })
                        .unwrap_or(false);
                    if !(name_matches && elapsed_ok) {
                        tracing::info!(
                            caster = intent.caster,
                            spell = %spell.name,
                            cast_time = spell.cast_time,
                            in_flight = %intent.cast_name_at_dispatch,
                            "CastSpell rejected — cast-time gate (no matching CastStart or too early)"
                        );
                        handlers::fan_out_cast_fail(
                            &mut server,
                            &in_world_recipients_now,
                            intent.caster,
                            "cast not ready".to_string(),
                        );
                        continue;
                    }
                    // Track 17.2 — movement-during-cast gate. Compare
                    // current caster pos to the snapshot taken at
                    // CastStart. >MAX_CAST_MOVE_DISTANCE cancels the
                    // cast (client cancels its own cast on movement
                    // already; this catches forged clients that omit
                    // the cancel and keep the cast alive while running).
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
                            handlers::fan_out_cast_fail(
                                &mut server,
                                &in_world_recipients_now,
                                intent.caster,
                                "interrupted (moved)".to_string(),
                            );
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
                            handlers::fan_out_cast_fail(
                                &mut server,
                                &in_world_recipients_now,
                                intent.caster,
                                "Spell is on cooldown.".to_string(),
                            );
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
                    continue;
                }
                let caster_pos = caster_conn.pos;
                let caster_max_mp = caster_conn.max_mp;
                let mana_cost = spell.mana_cost;
                let hp_cost = spell.hp_cost;
                let dmg_type = spells::parse_damage_type(&spell.damage_type);

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
                        // Track 6 sub-task 4a — apply buffs to caster.
                        // HoT, MP regen, Lich Form. Push onto
                        // conn.active_buffs (overwrites same-name to
                        // refresh duration). Snapshot fans below.
                        let mut buff_changed = false;
                        if spell.hot_hps > 0.0 && spell.hot_duration > 0.0 {
                            apply_buff(
                                connections.get_mut(&caster_cid).expect("checked"),
                                ActiveBuff::new_hot(
                                    spell.name.clone(),
                                    spell.hot_hps,
                                    spell.hot_duration,
                                    now,
                                ),
                            );
                            buff_changed = true;
                        }
                        if spell.mp_regen_hps > 0.0 && spell.mp_regen_duration > 0.0 {
                            apply_mp_regen_exclusive(
                                connections.get_mut(&caster_cid).expect("checked"),
                                ActiveBuff::new_mp_regen(
                                    spell.name.clone(),
                                    spell.mp_regen_hps,
                                    spell.mp_regen_duration,
                                    now,
                                ),
                            );
                            buff_changed = true;
                        }
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
                        // Track 6 sub-task 4c — combat-modifier buffs.
                        // Speed / Haste / DamageShield / Absorb /
                        // AccuracyCrit. All apply on the caster (SELF
                        // target). Refresh same-name on re-cast.
                        if spell.move_speed_mult > 0.0 && spell.move_speed_duration > 0.0 {
                            apply_buff(
                                connections.get_mut(&caster_cid).expect("checked"),
                                ActiveBuff::new_speed(
                                    spell.name.clone(),
                                    spell.move_speed_mult,
                                    spell.move_speed_duration,
                                    now,
                                ),
                            );
                            buff_changed = true;
                        }
                        if spell.haste_amount > 0.0 && spell.haste_duration > 0.0 {
                            apply_buff(
                                connections.get_mut(&caster_cid).expect("checked"),
                                ActiveBuff::new_haste(
                                    spell.name.clone(),
                                    spell.haste_amount,
                                    spell.haste_duration,
                                    now,
                                ),
                            );
                            buff_changed = true;
                        }
                        if spell.damage_shield_amount > 0.0 && spell.damage_shield_duration > 0.0 {
                            apply_damage_shield_exclusive(
                                connections.get_mut(&caster_cid).expect("checked"),
                                ActiveBuff::new_damage_shield(
                                    spell.name.clone(),
                                    spell.damage_shield_amount,
                                    spell.damage_shield_duration,
                                    now,
                                ),
                            );
                            buff_changed = true;
                        }
                        if spell.absorb_amount > 0.0 {
                            apply_absorb_exclusive(
                                connections.get_mut(&caster_cid).expect("checked"),
                                ActiveBuff::new_absorb(
                                    spell.name.clone(),
                                    spell.absorb_amount,
                                    now,
                                ),
                            );
                            buff_changed = true;
                            tracing::info!(
                                caster = intent.caster,
                                spell = %spell.name,
                                pool = spell.absorb_amount,
                                "absorb buff applied"
                            );
                        }
                        if (spell.accuracy_buff > 0.0 || spell.crit_buff > 0.0)
                            && spell.stat_buff_duration > 0.0
                        {
                            apply_buff(
                                connections.get_mut(&caster_cid).expect("checked"),
                                ActiveBuff::new_accuracy_crit(
                                    spell.name.clone(),
                                    spell.accuracy_buff,
                                    spell.crit_buff,
                                    spell.stat_buff_duration,
                                    now,
                                ),
                            );
                            buff_changed = true;
                        }
                        // Track 6 sub-task 4b — primary stat buff. Any
                        // spell with primary_stat_buff_duration > 0 +
                        // at least one non-zero stat delta pushes a
                        // StatBuff. apply_stat_buff handles refresh
                        // (undo old deltas before applying new ones)
                        // so re-cast doesn't double-stack.
                        if spell.primary_stat_buff_duration > 0.0 {
                            let any_nonzero = spell.str_buff != 0
                                || spell.agi_buff != 0
                                || spell.int_buff != 0
                                || spell.wis_buff != 0
                                || spell.con_buff != 0
                                || spell.max_hp_buff != 0.0
                                || spell.max_mp_buff != 0.0;
                            if any_nonzero {
                                let buff = ActiveBuff::new_stat_buff(
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
                                );
                                apply_stat_buff(
                                    connections.get_mut(&caster_cid).expect("checked"),
                                    buff,
                                );
                                // max_hp / max_mp may have changed —
                                // mark resources dirty so the next
                                // regen tick fans HealthUpdate /
                                // ManaUpdate reflecting the new caps.
                                regen::mark_dirty(
                                    connections.get_mut(&caster_cid).expect("checked"),
                                );
                                buff_changed = true;
                            }
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
                            // HoT / buff path skipped for pets — pets don't
                            // have an active_buffs field on Entity yet
                            // (see ALLY-includes-pets scope notes).
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
                            continue;
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
                        let mut ally_buff_changed = false;
                        if spell.hot_hps > 0.0 && spell.hot_duration > 0.0 {
                            apply_buff(
                                connections.get_mut(&target_cid).expect("checked"),
                                ActiveBuff::new_hot(
                                    spell.name.clone(),
                                    spell.hot_hps,
                                    spell.hot_duration,
                                    now,
                                ),
                            );
                            ally_buff_changed = true;
                        }
                        if ally_buff_changed {
                            if let Some(tc) = connections.get(&target_cid) {
                                fan_out_server_buff_snapshot(
                                    &mut server,
                                    &in_world_recipients_now,
                                    tc,
                                );
                            }
                        }
                    }
                    "ENEMY" => {
                        let Some(target_id) = intent.target_id else {
                            continue;
                        };
                        // Player target → PvP path (gate via can_attack
                        // for parity with melee swings).
                        if target_id < protocol::world::ENEMY_ID_BASE {
                            let target_cid = target_id as ClientId;
                            if target_cid == caster_cid {
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
                            let shield_back: f32;
                            let shield_back_name: Option<String>;
                            let mut absorb_strip_idx: Option<usize> = None;
                            let (final_hp, max_hp, applied) = {
                                let tc = connections.get_mut(&target_cid).expect("checked");
                                if tc.hp <= 0.0 || !tc.in_world {
                                    continue;
                                }
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
                                let outcome = roll_cast_interrupt(tc);
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
                                        let outcome = roll_cast_interrupt(att);
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
                                    tracing::debug!(
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
                            continue;
                        }
                        apply_spell_damage_to_enemy(
                            &mut server,
                            &in_world_recipients_now,
                            &mut connections,
                            &mut enemies,
                            &mut loot_bags,
                            &mut aoi,
                            intent.caster,
                            target_id,
                            spell,
                            dmg_type,
                            now,
                        );
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
                            tracing::debug!(
                                caster = intent.caster,
                                spell = %spell.name,
                                "AOE spell with no radius; nothing to apply"
                            );
                            continue;
                        }
                        let radius = spell.aoe_radius;
                        let radius_sq = radius * radius;
                        let caster_cell = aoi::cell_for(caster_pos.x, caster_pos.z);
                        let visible = aoi.entities_visible_from(caster_cell);
                        let mut victims: Vec<EntityId> = Vec::new();
                        for id in visible {
                            if id < protocol::world::ENEMY_ID_BASE
                                || id >= protocol::world::LOOT_BAG_ID_BASE
                            {
                                continue;
                            }
                            if let Some(entity) = enemies.get(&id) {
                                if !entity.is_alive() {
                                    continue;
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
                            continue;
                        }
                        let Some(template) = pet_templates::lookup(&pet_type) else {
                            tracing::info!(
                                caster = intent.caster,
                                spell = %spell.name,
                                pet_type = %pet_type,
                                "unknown pet_type — server-side summon dropped"
                            );
                            continue;
                        };
                        let owner_id = intent.caster;
                        // Spawn at the caster's pos + 1.5 m east
                        // offset so the pet doesn't clip the player
                        // capsule. Helper handles despawn-existing,
                        // AOI insert, fan-out, and map insert.
                        let spawn_pos = Vec3f {
                            x: caster_pos.x + 1.5,
                            y: caster_pos.y,
                            z: caster_pos.z,
                        };
                        let pet_id = summon_pet_for_owner(
                            &mut server,
                            &in_world_recipients_now,
                            &mut enemies,
                            &mut aoi,
                            owner_id,
                            spawn_pos,
                            template,
                            1.0,
                            now,
                        );
                        tracing::info!(
                            owner = owner_id,
                            pet_id,
                            spell = %spell.name,
                            pet_type = %pet_type,
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
                        // Charm decay (in the sweep below) simply
                        // despawns the pet — "mob runs away"
                        // semantics matching the GDScript charm.
                        let Some(target_id) = intent.target_id else {
                            tracing::debug!(caster = intent.caster, spell = %spell.name, "PET_CHARM dropped — no target");
                            continue;
                        };
                        if target_id < protocol::world::ENEMY_ID_BASE
                            || target_id >= protocol::world::LOOT_BAG_ID_BASE
                        {
                            tracing::debug!(caster = intent.caster, target = target_id, "PET_CHARM dropped — target id not in enemy partition");
                            continue;
                        }
                        let owner_id = intent.caster;
                        // Look up the target; copy out the stats we
                        // need then remove it from the map + AOI.
                        let extracted: Option<(crate::world::zones::MobTemplate, f32, f32, Vec3f, f32)> = enemies
                            .get(&target_id)
                            .filter(|e| e.is_alive() && !e.is_pet())
                            .map(|e| (e.mob.clone(), e.hp, e.max_hp, e.pos, e.yaw));
                        let Some((mob, hp, max_hp, pos, yaw)) = extracted else {
                            tracing::debug!(caster = owner_id, target = target_id, "PET_CHARM dropped — target gone or not a live enemy");
                            continue;
                        };
                        // Despawn the old enemy id from AOI + fan.
                        let enemy_cell = aoi::cell_for(pos.x, pos.z);
                        aoi.remove(target_id, enemy_cell);
                        let despawn_visible = aoi.entities_visible_from(enemy_cell);
                        for &recipient in &in_world_recipients_now {
                            if despawn_visible.contains(&recipient) {
                                handlers::send_entity_despawn(&mut server, recipient, target_id);
                            }
                        }
                        // Also fire spawn-point respawn so the camp
                        // doesn't think the slot is still occupied.
                        // (Charmed mobs that get re-keyed shouldn't
                        // block respawn at their old anchor.)
                        let old_idx = enemies
                            .get(&target_id)
                            .map(|e| e.spawn_point_idx)
                            .unwrap_or(usize::MAX);
                        enemies.remove(&target_id);
                        if old_idx != usize::MAX {
                            spawner.on_enemy_died(old_idx, now);
                        }
                        // Build the fresh pet entity. Override hp/
                        // max_hp/yaw from the original so the charm
                        // preserves the mob's current state.
                        let mut pet = Entity::from_pet_summon(owner_id, pos, mob, now);
                        pet.max_hp = max_hp;
                        pet.hp = hp;
                        pet.yaw = yaw;
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
                    "NONE" | _ => {
                        // port / bind — not yet applied server-side.
                        // Mana already deducted; the client-local
                        // handler covers the rest until a later
                        // track lifts that authority.
                        tracing::debug!(
                            caster = intent.caster,
                            spell = %spell.name,
                            target_type = %spell.target_type,
                            "spell target_type not yet processed server-side; mana deducted only"
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
                    );
                } else if let Some(last) = also_notify_dissolved {
                    // Group dissolved — send an empty roster to the
                    // last member as a "your group dissolved" signal.
                    handlers::fan_group_roster(
                        srv,
                        std::slice::from_ref(&last),
                        gid,
                        last,
                        Vec::new(),
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
                continue;
            };
            if group.leader != leader_cid {
                continue; // only leader can kick
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
                continue;
            };
            if target_cid == leader_cid {
                continue; // leader can't kick self (use /leave)
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
                        );
                    }
                } else {
                    fan_roster(&mut server, &connections, &group_manager, gid, None);
                }
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
                        let target_alive_enemy = enemies
                            .get(&target_id)
                            .map(|e| e.is_alive() && !e.is_pet())
                            .unwrap_or(false);
                        if !target_alive_enemy {
                            tracing::debug!(owner = intent.owner, target = target_id, "PetCommand ATTACK dropped — target not a live enemy");
                            continue;
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
                        continue;
                    }
                };
                if touched.is_empty() {
                    continue;
                }
                conn.inventory_dirty = true;
                // Snapshot the touched slot contents so we can fan
                // Deltas without holding a mutable borrow on conn.
                let deltas: Vec<(String, u32, Option<(String, u32)>)> = touched
                    .iter()
                    .map(|(loc, slot)| {
                        let payload = if loc == "base" {
                            conn.inventory
                                .base
                                .get(*slot as usize)
                                .and_then(|s| s.as_ref())
                                .map(|e| (e.item_path.clone(), e.count))
                        } else if let Some(base_idx) = loc
                            .strip_prefix("bag_")
                            .and_then(|s| s.parse::<u8>().ok())
                        {
                            conn.inventory
                                .bags
                                .get(&base_idx)
                                .and_then(|arr| arr.get(*slot as usize))
                                .and_then(|s| s.as_ref())
                                .map(|e| (e.item_path.clone(), e.count))
                        } else {
                            None
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
                    continue;
                }
                let owner_cid = intent.owner as ClientId;
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
                if intent.location != "base" {
                    tracing::debug!(
                        owner = intent.owner,
                        loc = %intent.location,
                        "DropItem rejected — non-base locations not yet supported"
                    );
                    continue;
                }
                let owner_cid = intent.owner as ClientId;
                let drop_pos: Vec3f;
                let dropped: Option<(String, u32)>;
                if let Some(conn) = connections.get_mut(&owner_cid) {
                    drop_pos = conn.pos;
                    dropped = conn.inventory.drop_base(intent.slot as usize, intent.count);
                    if dropped.is_some() {
                        conn.inventory_dirty = true;
                    }
                } else {
                    continue;
                }
                let Some((item_path, count)) = dropped else {
                    tracing::debug!(
                        owner = intent.owner,
                        slot = intent.slot,
                        "DropItem rejected — empty slot"
                    );
                    continue;
                };
                // Inventory Delta for the source slot — reflect the
                // new state (residual count or empty).
                let post = connections
                    .get(&owner_cid)
                    .and_then(|c| {
                        c.inventory
                            .base
                            .get(intent.slot as usize)
                            .and_then(|s| s.as_ref())
                            .map(|e| (e.item_path.clone(), e.count))
                    });
                let (delta_path, delta_count) = match post {
                    Some((p, c)) => (Some(p), c),
                    None => (None, 0),
                };
                handlers::send_inventory_delta(
                    &mut server,
                    owner_cid,
                    "base".to_string(),
                    intent.slot,
                    delta_path,
                    delta_count,
                );
                // Spawn a single-stack LootBag at the player's feet
                // and fan via the existing AOI-filtered loot pipeline.
                let bag = loot::LootBag::new(
                    drop_pos,
                    vec![loot::LootItemStack { item_path: item_path.clone(), count }],
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
                        continue;
                    }
                };
                conn.inventory_dirty = true;
                let recompute = inventory::recompute_equipped_stats(conn);
                let deltas: Vec<(String, u32, Option<(String, u32)>)> = touched
                    .iter()
                    .map(|(loc, slot)| {
                        let payload = if loc == "base" {
                            conn.inventory
                                .base
                                .get(*slot as usize)
                                .and_then(|s| s.as_ref())
                                .map(|e| (e.item_path.clone(), e.count))
                        } else if loc == "equip" {
                            conn.inventory
                                .equipment
                                .get(&(*slot as u8))
                                .map(|e| (e.item_path.clone(), e.count))
                        } else if let Some(base_idx) =
                            loc.strip_prefix("bag_").and_then(|s| s.parse::<u8>().ok())
                        {
                            conn.inventory
                                .bags
                                .get(&base_idx)
                                .and_then(|arr| arr.get(*slot as usize))
                                .and_then(|s| s.as_ref())
                                .map(|e| (e.item_path.clone(), e.count))
                        } else {
                            None
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
                if intent.dst_location != "base" {
                    tracing::debug!(
                        owner = intent.owner,
                        dst_loc = %intent.dst_location,
                        "UnequipItem rejected — only 'base' dst supported in Track 13.3"
                    );
                    continue;
                }
                let owner_cid = intent.owner as ClientId;
                let Some(conn) = connections.get_mut(&owner_cid) else {
                    continue;
                };
                let dst = intent.dst_slot as usize;
                let touched = match conn
                    .inventory
                    .unequip_to_base(intent.equip_slot, dst)
                {
                    Ok(t) => t,
                    Err(e) => {
                        tracing::info!(
                            owner = intent.owner,
                            equip_slot = intent.equip_slot,
                            dst,
                            error = %e,
                            "UnequipItem rejected"
                        );
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
                        continue;
                    }
                };
                conn.inventory_dirty = true;
                let (item_path, count) = destroyed;
                let payload = if intent.location == "base" {
                    conn.inventory
                        .base
                        .get(intent.slot as usize)
                        .and_then(|s| s.as_ref())
                        .map(|e| (e.item_path.clone(), e.count))
                } else if let Some(base_idx) = intent
                    .location
                    .strip_prefix("bag_")
                    .and_then(|s| s.parse::<u8>().ok())
                {
                    conn.inventory
                        .bags
                        .get(&base_idx)
                        .and_then(|arr| arr.get(intent.slot as usize))
                        .and_then(|s| s.as_ref())
                        .map(|e| (e.item_path.clone(), e.count))
                } else {
                    None
                };
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
                let Some(conn) = connections.get_mut(&owner_cid) else {
                    continue;
                };
                // Peek the item so we know what effect to apply
                // BEFORE the decrement removes it from the slot.
                let peek_path: Option<String> = if intent.location == "base" {
                    conn.inventory
                        .base
                        .get(intent.slot as usize)
                        .and_then(|s| s.as_ref())
                        .map(|e| e.item_path.clone())
                } else if let Some(base_idx) = intent
                    .location
                    .strip_prefix("bag_")
                    .and_then(|s| s.parse::<u8>().ok())
                {
                    conn.inventory
                        .bags
                        .get(&base_idx)
                        .and_then(|arr| arr.get(intent.slot as usize))
                        .and_then(|s| s.as_ref())
                        .map(|e| e.item_path.clone())
                } else {
                    None
                };
                let Some(item_path) = peek_path else {
                    tracing::debug!(
                        owner = intent.owner,
                        loc = %intent.location,
                        slot = intent.slot,
                        "UseConsumable rejected — slot empty"
                    );
                    continue;
                };
                let Some(item) = items::lookup(&item_path) else {
                    tracing::debug!(
                        owner = intent.owner,
                        %item_path,
                        "UseConsumable rejected — unknown item path"
                    );
                    continue;
                };
                let is_heal_potion = item.heal_on_use > 0.0 || item.mp_on_use > 0.0;
                if !item.is_food && !item.is_drink && !is_heal_potion {
                    tracing::debug!(
                        owner = intent.owner,
                        %item_path,
                        "UseConsumable rejected — item is not consumable"
                    );
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
                    continue;
                }
                if item.is_drink
                    && conn
                        .active_buffs
                        .iter()
                        .any(|b| b.name.starts_with("Drink: "))
                {
                    tracing::debug!(owner = intent.owner, "UseConsumable rejected — already drinking");
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
                        continue;
                    }
                };
                conn.inventory_dirty = true;
                // Inventory Delta for the touched slot.
                let payload = if intent.location == "base" {
                    conn.inventory
                        .base
                        .get(intent.slot as usize)
                        .and_then(|s| s.as_ref())
                        .map(|e| (e.item_path.clone(), e.count))
                } else if let Some(base_idx) = intent
                    .location
                    .strip_prefix("bag_")
                    .and_then(|s| s.parse::<u8>().ok())
                {
                    conn.inventory
                        .bags
                        .get(&base_idx)
                        .and_then(|arr| arr.get(intent.slot as usize))
                        .and_then(|s| s.as_ref())
                        .map(|e| (e.item_path.clone(), e.count))
                } else {
                    None
                };
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
                let deltas: Vec<(u32, String, u32)> = touched
                    .iter()
                    .map(|&slot_idx| {
                        let entry = conn.inventory.base[slot_idx]
                            .as_ref()
                            .expect("just inserted");
                        (slot_idx as u32, entry.item_path.clone(), entry.count)
                    })
                    .collect();
                for (slot_idx, path, count) in deltas {
                    handlers::send_inventory_delta(
                        &mut server,
                        owner_cid,
                        "base".to_string(),
                        slot_idx,
                        Some(path),
                        count,
                    );
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
                let Some(item) = items::lookup_by_name(&intent.item_name) else {
                    tracing::info!(
                        owner = intent.owner,
                        item_name = %intent.item_name,
                        "BuyItem rejected — unknown item name"
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
                    continue;
                }
                let total_cost = unit_price.saturating_mul(intent.qty as i64);
                if conn.coins < total_cost {
                    tracing::info!(
                        owner = intent.owner,
                        item_name = %intent.item_name,
                        coins = conn.coins,
                        cost = total_cost,
                        "BuyItem rejected — insufficient coins"
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
                    continue;
                }
                // Charge only for what we actually placed.
                let actual_cost = unit_price.saturating_mul(placed as i64);
                conn.coins -= actual_cost;
                conn.inventory_dirty = true;
                let coins_after = conn.coins;
                let deltas: Vec<(u32, String, u32)> = touched
                    .iter()
                    .map(|&slot_idx| {
                        let entry = conn.inventory.base[slot_idx]
                            .as_ref()
                            .expect("just inserted");
                        (slot_idx as u32, entry.item_path.clone(), entry.count)
                    })
                    .collect();
                for (slot_idx, path, count) in deltas {
                    handlers::send_inventory_delta(
                        &mut server,
                        owner_cid,
                        "base".to_string(),
                        slot_idx,
                        Some(path),
                        count,
                    );
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
                }
                tracing::info!(
                    owner = intent.owner,
                    item_name = %intent.item_name,
                    qty = placed,
                    cost = actual_cost,
                    coins_after,
                    "BuyItem applied"
                );
            }
        }

        // 4hj. Track 14 follow-up — apply vendor SellItem intents.
        //      Server: looks up the slot, validates item count, computes
        //      sell price (vendor_price / 2 per unit, mirroring the
        //      GDScript), credits coins, removes via drop_base /
        //      bag-inner mutation, fans CoinsUpdate + InventoryDelta.
        //      Equip-slot sells reject (player should unequip first).
        if !sell_item_intents.is_empty() {
            for intent in sell_item_intents.drain(..) {
                let owner_cid = intent.owner as ClientId;
                let Some(conn) = connections.get_mut(&owner_cid) else {
                    continue;
                };
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
                        let Some(arr) = conn.inventory.bags.get(&base) else {
                            tracing::info!(
                                owner = intent.owner,
                                "SellItem rejected — bag slot has no bag at base"
                            );
                            continue;
                        };
                        let Some(entry) = arr.get(slot as usize).and_then(|s| s.as_ref())
                        else {
                            tracing::info!(
                                owner = intent.owner,
                                "SellItem rejected — bag slot empty"
                            );
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
                        continue;
                    }
                };
                let Some(item) = items::lookup(&item_path) else {
                    tracing::info!(
                        owner = intent.owner,
                        item_path = %item_path,
                        "SellItem rejected — unknown item path"
                    );
                    continue;
                };
                let unit_price = (item.vendor_price as i64) / 2;
                if unit_price <= 0 {
                    tracing::info!(
                        owner = intent.owner,
                        item_path = %item_path,
                        "SellItem rejected — item has no sell value"
                    );
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
                conn.coins = conn.coins.saturating_add(total_credit);
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
                    coins_after,
                    "SellItem applied"
                );
            }
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
            let player_snapshots: Vec<(EntityId, Vec3f)> = connections
                .values()
                .filter(|c| c.in_world)
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
            // Snapshot live non-pet enemies so pets can read target
            // position + alive-status without re-borrowing the map
            // inside the per-entity loop. (id, pos, alive)
            let enemy_target_snapshots: Vec<(EntityId, Vec3f, bool)> = enemies
                .iter()
                .filter(|(_, e)| !e.is_pet())
                .map(|(id, e)| (*id, e.pos, e.is_alive()))
                .collect();
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
            for pet in enemies.values_mut() {
                if !pet.is_pet() { continue; }
                if let Some(t) = pet.command_at {
                    if now.duration_since(t).as_secs_f32() > PET_COMMAND_STICKY_SECS {
                        pet.command_at = None;
                    }
                }
            }
            let mut target_changes: Vec<(EntityId, Option<EntityId>)> = Vec::new();
            let mut enemy_hits: Vec<(EntityId, HitIntent)> = Vec::new();
            // Track 7: collect enemy cell changes so the aoi grid stays
            // current. Player cell-change fan-outs read the grid to find
            // which enemies are now visible.
            let mut enemy_cell_changes: Vec<(EntityId, (i32, i32), (i32, i32))> = Vec::new();
            for entity in enemies.values_mut() {
                if !entity.is_alive() {
                    continue;
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
                aoi.update(id, old_cell, new_cell);
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
                // player branch below but simpler: pets have no
                // armor / absorb / shield buffs and no XP awarded
                // on death. Just apply HP, fan Hit + HealthUpdate,
                // and let the existing corpse-cleanup phase remove
                // the pet from the world after the linger window.
                if attacker >= protocol::world::ENEMY_ID_BASE
                    && attacker < protocol::world::PET_ID_BASE
                    && hit.target >= protocol::world::PET_ID_BASE
                {
                    let amount = hit.amount.max(0);
                    if let Some(pet) = enemies.get_mut(&hit.target) {
                        if !pet.is_alive() {
                            continue;
                        }
                        pet.hp = (pet.hp - amount as f32).max(0.0);
                        let new_hp = pet.hp;
                        let max_hp = pet.max_hp;
                        let died = new_hp <= 0.0;
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
                    && hit.target < protocol::world::LOOT_BAG_ID_BASE
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
                        let mob_xp = target_entity.mob.xp;
                        let mob_name_dead = if died {
                            target_entity.transition(EnemyState::Dead, now);
                            Some(target_entity.mob.name.clone())
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
                            if let Some(credit_id) = credit_id_opt {
                                if mob_xp > 0 && credit_id < protocol::world::ENEMY_ID_BASE {
                                    let cid = credit_id as ClientId;
                                    if connections.contains_key(&cid) {
                                        handlers::send_xp_gained(
                                            &mut server,
                                            cid,
                                            mob_xp,
                                        );
                                    }
                                }
                            }
                            if let Some(mob_name) = mob_name_dead.as_ref() {
                                if let Some(items) = loot::roll_for_mob(mob_name) {
                                    let bag = LootBag::new(death_pos, items, now);
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
                            let interrupt_outcome = roll_cast_interrupt(target_conn);
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
        //     CORPSE_LINGER_SECS so the client can play the fall-over
        //     animation; afterwards we fan out EntityDespawn, arm the
        //     spawn point's respawn timer, and drop the row from the
        //     world map. Collect ids in a first pass to avoid borrowing
        //     `enemies` mutably twice in the same loop.
        let corpse_linger = Duration::from_secs_f32(CORPSE_LINGER_SECS);
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
            // who could see the enemy's cell.
            let enemy_cell = aoi::cell_for(entity.pos.x, entity.pos.z);
            aoi.remove(entity.id, enemy_cell);
            let visible = aoi.entities_visible_from(enemy_cell);
            for &recipient in &in_world_recipients_now {
                if visible.contains(&recipient) {
                    handlers::send_entity_despawn(&mut server, recipient, entity.id);
                }
            }
            spawner.on_enemy_died(entity.spawn_point_idx, now);
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
                let Some(bag) = loot_bags.get_mut(&intent.bag_id) else {
                    continue;
                };
                if bag.pos.distance_to(looter_pos) > LOOT_PICKUP_RANGE {
                    continue;
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
                    let mut touched_deltas: Vec<(u32, String, u32)> = Vec::new();
                    let mut placed_count: u32 = 0;
                    let mut leftover_count: u32 = 0;
                    if let Some(conn) = connections.get_mut(&looter_cid) {
                        match conn.inventory.add_item_locating(&path, count) {
                            Ok((touched, leftover)) => {
                                if !touched.is_empty() {
                                    conn.inventory_dirty = true;
                                }
                                for slot_idx in &touched {
                                    let entry = conn.inventory.base[*slot_idx]
                                        .as_ref()
                                        .expect("just inserted");
                                    touched_deltas.push((
                                        *slot_idx as u32,
                                        entry.item_path.clone(),
                                        entry.count,
                                    ));
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
                    for (slot_idx, item_path, total_count) in touched_deltas {
                        handlers::send_inventory_delta(
                            &mut server,
                            looter_cid,
                            "base".to_string(),
                            slot_idx,
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
                if bag.items.is_empty() {
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
        for (client_id, conn) in connections.iter_mut().filter(|(_, c)| c.ready) {
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
                if peer_entity >= protocol::world::LOOT_BAG_ID_BASE {
                    // Loot bag — seed the mover with LootBagSpawn.
                    if let Some(bag) = loot_bags.get(&peer_entity) {
                        handlers::fan_out_loot_bag_spawn(
                            &mut server,
                            std::slice::from_ref(mover_id),
                            bag,
                        );
                    }
                } else if peer_entity >= protocol::world::ENEMY_ID_BASE {
                    // Enemy — seed the mover with EnemySpawn.
                    if let Some(entity) = enemies.get(&peer_entity) {
                        if entity.is_alive() {
                            handlers::fan_out_enemy_spawn(
                                &mut server,
                                std::slice::from_ref(mover_id),
                                entity,
                            );
                        }
                    }
                } else {
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
                    // Enemy or bag — just tell the mover it's gone.
                    handlers::send_entity_despawn(&mut server, *mover_id, peer_entity);
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

        // 6b. Enemy Position fan-out. Track 7: AOI-filtered by the
        //     enemy's current XZ position. Only players whose cell is
        //     in the 3×3 neighbourhood of the enemy's cell receive
        //     the broadcast. Enemy entities are not yet in the AoiGrid
        //     (that lands in Track 7 sub-task 5 with EnemySpawn
        //     narrowing); the cell is computed inline from entity.pos.
        if !in_world_ids.is_empty() {
            for entity in enemies.values_mut() {
                if !entity.is_alive() {
                    continue;
                }
                entity.seq = entity.seq.wrapping_add(1);
                let Some(bytes) = handlers::build_enemy_position_msg(entity) else {
                    continue;
                };
                let enemy_cell = aoi::cell_for(entity.pos.x, entity.pos.z);
                let visible = aoi.entities_visible_from(enemy_cell);
                for recipient_id in &in_world_ids {
                    if visible.contains(recipient_id) {
                        server.send_message(*recipient_id, CHANNEL_POSITION, bytes.clone());
                    }
                }
            }
        }

        // 7. Periodic checkpoint.
        if now.duration_since(last_checkpoint) >= CHECKPOINT_INTERVAL {
            let mut dirty: Vec<&mut PerConnection> = connections.values_mut().collect();
            persistence::checkpoint_dirty(&pool, &mut dirty).await;
            last_checkpoint = now;
        }

        // 8. Push outbound packets to the network.
        transport.send_packets(&mut server);

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
