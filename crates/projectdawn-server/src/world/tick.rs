//! 20 Hz tick scheduler. Owns the connection map exclusively (no locking)
//! and drives both the renet transport and the application-layer message
//! pipeline.

use super::{
    combat,
    connection::{PerConnection, Vec3f},
    entity::{Entity, EnemyState, HitIntent},
    handlers::{self, Outcome},
    items,
    loot::{self, LootBag},
    persistence,
    regen,
    spawn_points::Spawner,
    spells,
    ATTACK_RANGE_TOLERANCE, CHANNEL_POSITION, CHANNEL_SYSTEM, CHECKPOINT_INTERVAL,
    CORPSE_LINGER_SECS, LOOT_BAG_LINGER_SECS, LOOT_PICKUP_RANGE, MAX_MOVE_SPEED,
    RANGED_ATTACK_RANGE, STALE_MOVE_THRESHOLD, TICK_DT,
};
use crate::{db, Config};
use protocol::world::{DamageType, EntityId, KickCode};
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
struct CastSpellIntent {
    caster: u64,
    spell_name: String,
    target_id: Option<protocol::world::EntityId>,
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

    let mut connections: HashMap<ClientId, PerConnection> = HashMap::new();
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
                            connections.insert(
                                client_id,
                                PerConnection::from_spawn(spawn, now),
                            );
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

                    // Broadcast EntityDespawn to every other in_world peer
                    // before removing the conn from the map. Skip if the
                    // leaver never entered the world (no EntitySpawn was
                    // ever sent for them, so a despawn would land at peers
                    // who never had a record).
                    let despawn_id = connections
                        .get(&client_id)
                        .filter(|c| c.in_world)
                        .map(|c| c.char_id as u64);
                    if let Some(entity_id) = despawn_id {
                        let peer_ids: Vec<ClientId> = connections
                            .iter()
                            .filter(|(id, c)| **id != client_id && c.in_world)
                            .map(|(id, _)| *id)
                            .collect();
                        for peer_id in peer_ids {
                            handlers::send_entity_despawn(
                                &mut server,
                                peer_id,
                                entity_id,
                            );
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
        // Track 5 sub-task 3 — player → server attack intents queued for
        // the apply phase after dispatch. Verbatim queue (each swing is
        // a distinct event; coalescing would silently drop multi-hit
        // combos).
        let mut attack_intents: Vec<AttackIntent> = Vec::new();
        let mut cast_spell_intents: Vec<CastSpellIntent> = Vec::new();
        // Track 5 sub-task 4 — player → server loot pickup intents.
        // Verbatim queue; sub-task 4 is FFA loot so order matters for
        // contested bags (first arrival wins the slot).
        let mut loot_intents: Vec<LootIntent> = Vec::new();
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
                        } => {
                            cast_spell_intents.push(CastSpellIntent {
                                caster,
                                spell_name,
                                target_id,
                            });
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
            let peer_ids: Vec<ClientId> = connections
                .iter()
                .filter(|(id, c)| *id != new_id && c.in_world)
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
            // Track 5 sub-task 1B — seed the new joiner with every alive
            // enemy. Subsequent live spawns fan out via the spawner phase
            // below; this catches everything that existed before the
            // joiner arrived.
            for entity in enemies.values() {
                if !entity.is_alive() {
                    continue;
                }
                handlers::fan_out_enemy_spawn(
                    &mut server,
                    std::slice::from_ref(new_id),
                    entity,
                );
            }
            // Track 5 sub-task 4 — seed the new joiner with every live
            // loot bag. Bags persist across player joins (until the
            // 120 s linger expires), so a late joiner can still see
            // unlooted drops from earlier kills.
            for bag in loot_bags.values() {
                handlers::fan_out_loot_bag_spawn(
                    &mut server,
                    std::slice::from_ref(new_id),
                    bag,
                );
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
                    if !spawn_recipients.is_empty() {
                        handlers::fan_out_enemy_spawn(
                            &mut server,
                            &spawn_recipients,
                            &entity,
                        );
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
                    // with ARMOR_DR_DIVISOR = 100. The reduced amount
                    // is clamped to at least 1 — same as the client
                    // so unarmoured swings still tick HP.
                    let raw_swing = swing.amount;
                    let (new_hp, max_hp, amount, target_armor) = {
                        let target_conn = connections.get_mut(&target_cid).expect("checked");
                        let armor = target_conn.equipped_armor.max(0) as f32;
                        let reduction = armor / (armor + 100.0);
                        let amount = ((swing.amount as f32 * (1.0 - reduction)) as i32).max(1);
                        target_conn.hp = (target_conn.hp - amount as f32).max(0.0);
                        regen::mark_dirty(target_conn);
                        (target_conn.hp, target_conn.max_hp, amount, target_conn.equipped_armor)
                    };
                    tracing::info!(
                        attacker = intent.attacker,
                        target = intent.target_id,
                        raw_swing,
                        target_armor,
                        applied = amount,
                        target_hp = new_hp,
                        "PvP attack applied"
                    );
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
                        let xp = entity.mob.xp;
                        if xp > 0 {
                            let cid = credit_id as ClientId;
                            if connections.contains_key(&cid) {
                                handlers::send_xp_gained(&mut server, cid, xp);
                                tracing::info!(
                                    killer = credit_id,
                                    mob = %entity.mob.name,
                                    xp,
                                    "kill credit granted"
                                );
                            }
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
                        handlers::fan_out_loot_bag_spawn(
                            &mut server,
                            &in_world_recipients_now,
                            &bag,
                        );
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
        //      the spell in spells.toml, validates mana cost + target,
        //      and applies authoritative damage (ENEMY) or heal (SELF).
        //      Cast-time gating is still client-side for sub-task 3b;
        //      sub-task 4 lifts it server-side.
        if !cast_spell_intents.is_empty() {
            for intent in cast_spell_intents.drain(..) {
                let caster_cid = intent.caster as ClientId;
                let Some(spell) = spells::lookup(&intent.spell_name) else {
                    tracing::debug!(
                        caster = intent.caster,
                        spell = %intent.spell_name,
                        "unknown spell name — server-side cast dropped"
                    );
                    continue;
                };
                // Resolve caster's snapshot (immutable) — we need pos
                // for range / AOE. mp deduction lands later under a
                // mutable borrow.
                let Some(caster_conn) = connections.get(&caster_cid) else {
                    continue;
                };
                if caster_conn.mp < spell.mana_cost {
                    tracing::debug!(
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
                // come next.
                let (new_mp, new_hp_after_cost, max_hp) = {
                    let cc = connections.get_mut(&caster_cid).expect("checked");
                    cc.mp = (cc.mp - mana_cost).max(0.0);
                    if hp_cost > 0.0 {
                        cc.hp = (cc.hp - hp_cost).max(0.0);
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

                match spell.target_type.as_str() {
                    "SELF" => {
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
                            let (final_hp, max_hp, applied) = {
                                let tc = connections.get_mut(&target_cid).expect("checked");
                                if tc.hp <= 0.0 || !tc.in_world {
                                    continue;
                                }
                                // Spells skip armor reduction —
                                // matches GDScript: armor reduces only
                                // physical melee damage in
                                // Combat.receive_player_damage's
                                // wrapping. Spell resists land in a
                                // later sub-task.
                                let dmg = spell.base_damage.max(0.0) as i32;
                                tc.hp = (tc.hp - dmg as f32).max(0.0);
                                regen::mark_dirty(tc);
                                (tc.hp, tc.max_hp, dmg)
                            };
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
                            continue;
                        }
                        // Enemy target — apply spell damage to the
                        // enemy and propagate Hit / HealthUpdate.
                        let dmg = spell.base_damage.max(0.0) as i32;
                        let Some(entity) = enemies.get_mut(&target_id) else {
                            continue;
                        };
                        if !entity.is_alive() {
                            continue;
                        }
                        let dist = entity.pos.distance_to(caster_pos);
                        // Spells use ranged range (GDScript Spells use
                        // 25-30m by default); be permissive.
                        if dist > RANGED_ATTACK_RANGE {
                            tracing::debug!(
                                caster = intent.caster,
                                target = target_id,
                                spell = %spell.name,
                                dist,
                                "spell out of range"
                            );
                            continue;
                        }
                        entity.hp = (entity.hp - dmg as f32).max(0.0);
                        *entity.aggro.entry(intent.caster).or_insert(0.0) += dmg as f32;
                        handlers::fan_out_hit(
                            &mut server,
                            &in_world_recipients_now,
                            intent.caster,
                            target_id,
                            dmg,
                            false,
                            dmg_type,
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
                            if let Some((&credit_id, _)) = entity
                                .aggro
                                .iter()
                                .max_by(|a, b| {
                                    a.1.partial_cmp(b.1)
                                        .unwrap_or(std::cmp::Ordering::Equal)
                                })
                            {
                                let xp = entity.mob.xp;
                                if xp > 0 {
                                    let cid = credit_id as ClientId;
                                    if connections.contains_key(&cid) {
                                        handlers::send_xp_gained(&mut server, cid, xp);
                                    }
                                }
                            }
                            if let Some(items) = loot::roll_for_mob(&entity.mob.name) {
                                let bag = LootBag::new(entity.pos, items, now);
                                let bag_id = bag.id;
                                handlers::fan_out_loot_bag_spawn(
                                    &mut server,
                                    &in_world_recipients_now,
                                    &bag,
                                );
                                loot_bags.insert(bag_id, bag);
                            }
                        }
                    }
                    "AOE" | "NONE" | _ => {
                        // AOE / port / charm / etc. aren't applied
                        // server-side in sub-task 3b. The mana already
                        // deducted is the only server-side effect;
                        // client-local handler covers the rest until a
                        // later track lifts AOE / port authority.
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

        // 4i. Enemy AI tick. Each alive enemy evaluates its state machine
        //     against the snapshot of in_world player positions, advances
        //     its own pos / target / state, and yields events for the
        //     post-loop fan-out (target switch, melee swing). Position
        //     broadcasts ride the same step-6 fan-out as players.
        if !enemies.is_empty() && !in_world_recipients_now.is_empty() {
            let player_snapshots: Vec<(EntityId, Vec3f)> = connections
                .values()
                .filter(|c| c.in_world)
                .map(|c| (c.char_id as u64, c.pos))
                .collect();
            let mut target_changes: Vec<(EntityId, Option<EntityId>)> = Vec::new();
            let mut enemy_hits: Vec<(EntityId, HitIntent)> = Vec::new();
            for entity in enemies.values_mut() {
                if !entity.is_alive() {
                    continue;
                }
                let events = entity.tick_ai(&player_snapshots, dt, now);
                if let Some(new_target) = events.target_changed {
                    target_changes.push((entity.id, new_target));
                }
                if let Some(hit) = events.hit {
                    enemy_hits.push((entity.id, hit));
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
                let final_amount = if hit.target < protocol::world::ENEMY_ID_BASE {
                    let target_cid = hit.target as ClientId;
                    if let Some(target_conn) = connections.get_mut(&target_cid) {
                        if target_conn.in_world && target_conn.hp > 0.0 {
                            let armor = target_conn.equipped_armor.max(0) as f32;
                            let reduction = armor / (armor + 100.0);
                            let reduced = ((hit.amount as f32 * (1.0 - reduction)) as i32).max(1);
                            target_conn.hp = (target_conn.hp - reduced as f32).max(0.0);
                            regen::mark_dirty(target_conn);
                            damaged_player = Some(hit.target);
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
            for &recipient in &in_world_recipients_now {
                handlers::send_entity_despawn(&mut server, recipient, entity.id);
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
                    handlers::send_loot_granted(
                        &mut server,
                        intent.looter as ClientId,
                        path,
                        count,
                    );
                }
                if bag.items.is_empty() {
                    let bag_id = bag.id;
                    loot_bags.remove(&bag_id);
                    for &recipient in &in_world_recipients_now {
                        handlers::send_entity_despawn(&mut server, recipient, bag_id);
                    }
                } else {
                    handlers::fan_out_loot_bag_spawn(
                        &mut server,
                        &in_world_recipients_now,
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
            if loot_bags.remove(&id).is_some() {
                for &recipient in &in_world_recipients_now {
                    handlers::send_entity_despawn(&mut server, recipient, id);
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
        for conn in connections.values_mut().filter(|c| c.ready) {
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
            conn.pos.x += dir.x * MAX_MOVE_SPEED * dt;
            conn.pos.y += dir.y * MAX_MOVE_SPEED * dt;
            conn.pos.z += dir.z * MAX_MOVE_SPEED * dt;
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

        // 6. Position fan-out. Every in_world client's position goes to
        //    every in_world client INCLUDING themselves — Track 2's
        //    snap-or-lerp depends on the owner receiving their own
        //    broadcasts. Gated on in_world both ways so lobby-state
        //    clients don't broadcast static positions and don't receive
        //    Position broadcasts they couldn't render anyway. No AOI yet
        //    (slice 3 is "everyone in zone sees everyone"); spatial
        //    filtering is a future track.
        //
        //    Encode each sender once, clone bytes per recipient. At
        //    MAX_CLIENTS=64 and ~50 B per Position, worst case is
        //    64×64×50 = 200 KB/tick of memcpy ≈ 4 MB/s. Trivial.
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
            for recipient_id in &in_world_ids {
                server.send_message(*recipient_id, CHANNEL_POSITION, bytes.clone());
            }
        }

        // 6b. Enemy Position fan-out. Every alive enemy → every in_world
        //     client. Idle / Attack / Dead enemies don't move so their
        //     `events.moved` from step 4h gates whether they enter this
        //     loop. Sequence increments per broadcast so the client can
        //     drop out-of-order Position arrivals on the unreliable
        //     channel (same role as PerConnection.last_move_seq).
        if !in_world_ids.is_empty() {
            for entity in enemies.values_mut() {
                if !entity.is_alive() {
                    continue;
                }
                // Always broadcast for now — moving enemies need the live
                // updates, stationary ones need the seed for a late
                // joiner. A later optimisation can dedup with a per-entity
                // `last_broadcast_pos` check; bandwidth at 27 × 20 Hz is
                // trivial.
                entity.seq = entity.seq.wrapping_add(1);
                let Some(bytes) = handlers::build_enemy_position_msg(entity) else {
                    continue;
                };
                for recipient_id in &in_world_ids {
                    server.send_message(*recipient_id, CHANNEL_POSITION, bytes.clone());
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
