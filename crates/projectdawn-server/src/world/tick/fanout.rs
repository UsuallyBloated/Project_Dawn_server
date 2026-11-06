//! Fan-outs to other players: a joiner's seed of everything in view (step 4a), this
//! tick's queued cast, combat, buff, jump and death events (4b to 4f), and the audience
//! helpers the other steps share (say range, despawn and cell-crossing recipients).

use super::*;

/// Who is told an entity is gone for real: the in-world players who could
/// see its cell, plus a pet's owner wherever they are (`pet_owner_cid`).
/// The room: every in-world player within `SAY_RANGE` of `speaker`, never the
/// speaker. What a `/say` and the eating emote reach (the jump relay reaches
/// the jumper's AOI view instead, since 2026-10-29: a jump is seen, not
/// heard). Written keep-only-when-in-range, so a non-finite position hears
/// nothing.
pub(super) fn say_recipients(
    speaker: ClientId,
    connections: &HashMap<ClientId, PerConnection>,
) -> Vec<ClientId> {
    let Some(from) = connections.get(&speaker) else {
        return Vec::new();
    };
    connections
        .iter()
        .filter(|(id, c)| {
            **id != speaker && c.in_world && c.pos.distance_to(from.pos) <= crate::world::SAY_RANGE
        })
        .map(|(id, _)| *id)
        .collect()
}

pub(super) fn despawn_recipients(
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
pub(super) fn fan_entity_cell_crossing(
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
            .filter(|id| IdKind::of(*id) == IdKind::Player)
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

pub(super) fn seed_joiners(world: &mut WorldState, q: &mut Intents, now: Instant) {
    let WorldState {
        server,
        connections,
        aoi,
        enemies,
        loot_bags,
        corpses,
        ..
    } = world;
    let Intents {
        to_disconnect,
        newly_in_world,
        ..
    } = q;
    // 4a. EntitySpawn fan-out for clients that just sent `EnterWorld`.
    //     Each new client gets an EntitySpawn for every in_world peer;
    //     each in_world peer gets an EntitySpawn for the new client.
    //     Subject itself skipped — `ConnectOk` is the new client's
    //     own-spawn signal. Reliable channel ⇒ no race-induced lost
    //     spawns.
    for new_id in newly_in_world.iter() {
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
                handlers::send_entity_spawn(&mut *server, *peer_id, new_conn);
                handlers::fan_out_resources(&mut *server, std::slice::from_ref(peer_id), new_conn);
                if !new_conn.cast_spell_name.is_empty() {
                    if let Some(set_at) = new_conn.cast_set_at {
                        let elapsed = now.duration_since(set_at).as_secs_f32();
                        let remaining = new_conn.cast_total_duration - elapsed;
                        if remaining > 0.0 {
                            handlers::fan_out_cast_start(
                                &mut *server,
                                std::slice::from_ref(peer_id),
                                new_conn.char_id as u64,
                                new_conn.cast_spell_name.clone(),
                                remaining,
                            );
                        }
                    }
                }
                handlers::fan_out_buff_snapshot(
                    &mut *server,
                    std::slice::from_ref(peer_id),
                    new_conn,
                );
            }
        }
        // Existing peers → new client.
        for peer_id in &peer_ids {
            if let Some(peer_conn) = connections.get(peer_id) {
                handlers::send_entity_spawn(&mut *server, *new_id, peer_conn);
                // Seed the new client with each existing peer's last-
                // known resources (Track 4). No-op for peers that
                // haven't broadcast yet; their values land naturally on
                // the next ResourceUpdate fan-out.
                handlers::fan_out_resources(&mut *server, std::slice::from_ref(new_id), peer_conn);
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
                                &mut *server,
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
                    &mut *server,
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
            let Some(owner_id) = entity.owner else {
                continue;
            };
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
            handlers::fan_out_pet_spawn(&mut *server, std::slice::from_ref(new_id), entity, None);
            tracing::debug!(
                new_client = *new_id,
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
            handlers::send_inventory_snapshot(&mut *server, *new_id, entries);
        }
        // Track 15.1 follow-up — seed the new joiner with their
        // own coin balance. Without this the client sits at the
        // default PlayerStats.coins (0) and the vendor pre-flight
        // "you don't have enough coins" check fires for every
        // purchase. Buy/sell apply phases will keep the client
        // in sync after this seed.
        if let Some(new_conn) = connections.get(new_id) {
            handlers::send_coins_update(&mut *server, *new_id, new_conn.coins);
        }
        // Seed the new joiner with their own HP, mana and stamina. The regen
        // fan only sends a value that changed since its last send, and
        // `from_spawn` starts that baseline at the persisted values, so a
        // character who left dead with full mana and stamina was never sent
        // anything: the client kept the full bars `apply_character` gives it,
        // never learned it was dead, and never respawned, while the server
        // refused every move (2026-10-26). The same gap left a hurt character
        // with no HP regen (out of food) showing a full bar.
        if let Some(new_conn) = connections.get(new_id) {
            handlers::fan_out_resources(&mut *server, std::slice::from_ref(new_id), new_conn);
        }
        // The shared sky: a joiner is told the hour at once instead of
        // waiting up to a minute for the next broadcast (step 7-ter).
        handlers::fan_out_time_of_day(
            &mut *server,
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
            handlers::send_xp_gained(&mut *server, *new_id, 0, new_conn.xp, new_conn.xp_to_next);
        }
        // PD_W0015 — seed the new joiner with their bank balance so the
        // BankWindow shows the right total the moment they open it (the
        // client caches it; deposit/withdraw keep it in sync after).
        if let Some(new_conn) = connections.get(new_id) {
            handlers::send_bank_snapshot(&mut *server, *new_id, new_conn.bank_coins);
        }
        // PD_W0016 — seed both item vaults (personal + account-shared) so
        // the BankWindow's Items tab renders correctly on first open.
        if let Some(new_conn) = connections.get(new_id) {
            let personal = new_conn.bank_items.to_snapshot_entries();
            let shared = new_conn.account_bank_items.to_snapshot_entries();
            handlers::send_bank_item_snapshot(&mut *server, *new_id, false, personal);
            handlers::send_bank_item_snapshot(&mut *server, *new_id, true, shared);
        }
        // Track 18.1 — seed the new joiner with their three
        // passive skill score maps (weapon / armor / casting).
        // Without this the client autoloads sit at uninitialized
        // dicts and the character window renders zeros until the
        // first advance fires.
        if let Some(new_conn) = connections.get(new_id) {
            handlers::send_skill_progress_snapshot(&mut *server, *new_id, new_conn);
            // The server-held spell bar (audit fix 5): the client's bar
            // adopts it, or seeds it once if the server holds none yet.
            handlers::send_spell_gems(&mut *server, *new_id, new_conn);
        }
        // Factions and alignment (2026-10-23): every standing the
        // character has moved and the alignment score, so the client
        // follows the server's numbers from the first frame.
        if let Some(new_conn) = connections.get_mut(new_id) {
            handlers::send_factions_snapshot(&mut *server, *new_id, new_conn);
            handlers::send_alignment_if_due(&mut *server, *new_id, new_conn, true);
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
            let completed: Vec<String> = new_conn.completed_quests.iter().cloned().collect();
            handlers::send_quest_snapshot(&mut *server, *new_id, active, completed);
        }
        // Track 5 sub-task 1B — seed the new joiner with alive enemies
        // in their AOI neighbourhood. Track 7: filter by aoi.can_see
        // so only nearby enemies are seeded on enter-world.
        let joiner_cell = connections
            .get(new_id)
            .map(|c| c.aoi_cell)
            .unwrap_or((0, 0));
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
                    &mut *server,
                    std::slice::from_ref(new_id),
                    entity,
                    None,
                );
            }
        }
        // Track 5 sub-task 4 — seed the new joiner with live loot bags
        // in their AOI neighbourhood. Track 7: filter by aoi.can_see.
        for bag in loot_bags.values() {
            let bag_cell = aoi::cell_for(bag.pos.x, bag.pos.z);
            if aoi.can_see(joiner_cell, bag_cell) {
                handlers::fan_out_loot_bag_spawn(&mut *server, std::slice::from_ref(new_id), bag);
            }
        }
        // Corpse / resurrection Slice 1 — seed the joiner with nearby
        // corpses too (so a relog near your body, or a walk-back, shows it).
        for corpse in corpses.values() {
            let corpse_cell = aoi::cell_for(corpse.pos.x, corpse.pos.z);
            if aoi.can_see(joiner_cell, corpse_cell) {
                handlers::fan_out_corpse_spawn(&mut *server, std::slice::from_ref(new_id), corpse);
                // Slice 2 — seed the owner with the contents on relog /
                // enter-world so they can loot a corpse they spawn next to.
                if corpse.owner_char == *new_id as i64 {
                    handlers::send_corpse_contents(&mut *server, *new_id, corpse);
                }
            }
        }
    }
}

pub(super) fn fan_out_events(
    world: &mut WorldState,
    q: &mut Intents,
    _now: Instant,
    in_world_recipients: &[ClientId],
) {
    let WorldState {
        server,
        connections,
        aoi,
        group_manager,
        ..
    } = world;
    let Intents {
        to_disconnect,
        cast_fanouts,
        jump_fanouts,
        combat_fanouts,
        death_fanouts,
        player_target_fanouts,
        chat_fanouts,
        inspect_intents,
        ..
    } = q;
    // 4b. (Track 6 removed the client-driven ResourceUpdate fan-out.
    //     Resources are now server-authoritative: regen mutates
    //     `conn.hp` / `conn.mp` / `conn.stamina` in step 4l below and
    //     fans `HealthUpdate` / `ManaUpdate` / `StaminaUpdate` to all
    //     in_world clients including the owner. Step 4a still uses
    //     `fan_out_resources` to seed new joiners with the current
    //     value.)
    // 4b-jump. The jump relay: one PlayerJumped per jump begun this tick,
    //     to the in-world players who can see the jumper (the AOI view,
    //     the same audience as the jumper's Position fan in step 6; a
    //     jump is seen, not heard: the user's "jump check" on someone
    //     far off, 2026-10-29), never to the jumper, whose own client
    //     already played the hop. Position is untouched: the relay is
    //     the peers' animation cue and nothing else.
    for jumper_id in jump_fanouts.drain(..) {
        if to_disconnect.contains(&jumper_id) {
            continue;
        }
        let Some(jumper) = connections.get(&jumper_id) else {
            continue;
        };
        let visible = aoi.entities_visible_from(jumper.aoi_cell);
        let recipients: Vec<ClientId> = connections
            .iter()
            .filter(|(id, c)| **id != jumper_id && c.in_world && visible.contains(id))
            .map(|(id, _)| *id)
            .collect();
        handlers::fan_out_player_jumped(&mut *server, &recipients, jumper.char_id as u64);
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
                &mut *server,
                &recipients,
                caster,
                spell_name,
                duration,
            ),
            CastEvent::Complete { spell_name } => {
                handlers::fan_out_cast_complete(&mut *server, &recipients, caster, spell_name)
            }
            CastEvent::Fail { reason } => {
                handlers::fan_out_cast_fail(&mut *server, &recipients, caster, reason)
            }
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
                &mut *server,
                &recipients,
                attacker,
                target,
                amount,
                crit,
                dmg_type,
            ),
            CombatEvent::Miss { target } => {
                handlers::fan_out_miss(&mut *server, &recipients, attacker, target)
            }
            CombatEvent::Evade { target } => {
                handlers::fan_out_evade(&mut *server, &recipients, attacker, target)
            }
        }
    }

    // 4f. Death fan-out — EntityDied to in_world peers. Receiver
    //     RemotePlayer plays a fall-over animation in place; respawn
    //     is implied by the next ResourceUpdate (peer's HP coming
    //     back from 0 → non-zero stands them up). No separate Respawn
    //     variant by design (handoff Q3 option a).
    for sender_id in death_fanouts.iter() {
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
        handlers::fan_out_entity_died(&mut *server, &recipients, entity_id);
    }

    // Track 22.H: player target fan-out, for the target-of-target frame.
    // Only the LATEST target per sender per tick goes out (a burst of
    // retargets is one message, not one per player per retarget), and
    // only to players who can see the sender: nobody else has a frame
    // to draw it in, and a world-wide fan told everyone who was fighting
    // what. The sender and clients flagged for disconnect are skipped.
    let mut latest_target: HashMap<ClientId, Option<EntityId>> = HashMap::new();
    for (sender_id, target) in player_target_fanouts.drain(..) {
        latest_target.insert(sender_id, target);
    }
    for (sender_id, target) in latest_target {
        if to_disconnect.contains(&sender_id) {
            continue;
        }
        let Some(sender) = connections.get(&sender_id) else {
            continue;
        };
        let entity_id = sender.char_id as u64;
        let visible = aoi.entities_visible_from(sender.aoi_cell);
        let recipients: Vec<ClientId> = in_world_recipients
            .iter()
            .filter(|id| **id != sender_id && visible.contains(*id))
            .copied()
            .collect();
        handlers::fan_out_entity_target(&mut *server, &recipients, entity_id, target);
    }

    // Chat fan-out. Recipient set depends on channel:
    // - Say: everyone within SAY_RANGE of the sender, sender excluded.
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
                let recipients = say_recipients(sender_id, connections);
                handlers::fan_out_chat_message(&mut *server, &recipients, &speaker, channel, &text);
            }
            ChatChannel::Shout | ChatChannel::Ooc => {
                let recipients: Vec<ClientId> = in_world_recipients
                    .iter()
                    .filter(|id| **id != sender_id)
                    .copied()
                    .collect();
                handlers::fan_out_chat_message(&mut *server, &recipients, &speaker, channel, &text);
            }
            ChatChannel::Group => {
                // Fan to every member of the sender's group (except
                // the sender themself, who added the local echo).
                let Some(group) = group_manager.group_of(sender_id) else {
                    continue; // sender isn't in a group
                };
                let recipients: Vec<ClientId> = group
                    .members
                    .iter()
                    .filter(|id| {
                        **id != sender_id && connections.get(*id).is_some_and(|c| c.in_world)
                    })
                    .copied()
                    .collect();
                handlers::fan_out_chat_message(&mut *server, &recipients, &speaker, channel, &text);
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
                    handlers::fan_out_chat_message(&mut *server, &[rid], &speaker, channel, &text);
                } else {
                    // Bounce: tell the sender the target isn't online.
                    // Use System channel so the client can colour it
                    // distinctly from a real tell.
                    let bounce = format!("{} is not currently playing.", target);
                    handlers::fan_out_chat_message(
                        &mut *server,
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
                &mut *server,
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
            &mut *server,
            inspector_id,
            target_char_id,
            target_name,
            slots,
        );
    }
}
