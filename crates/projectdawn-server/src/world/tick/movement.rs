//! Movement (steps 5 and 5b): each player's Move integrated once, then the area-of-
//! interest crossings that spawn and despawn what a mover can see.

use super::*;

pub(super) fn run_movement(world: &mut WorldState, _q: &mut Intents, now: Instant, dt: f32, in_world_recipients_now: &[ClientId]) {
    let WorldState { server, connections, aoi, enemies, loot_bags, corpses, .. } = world;
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
            // Moving ends a feign death (standing up) and a hide (active
            // skills): the mobs that forgot or never noticed the player
            // may notice them now.
            if buffs::end_on_move(&mut conn.active_buffs) {
                handlers::fan_out_buff_snapshot(&mut *server, &in_world_recipients_now, conn);
            }
        }
        // The server decides the speed (RUN_SPEED's note): its own speed
        // buff (Spirit of Wolf, Selos' Melody), snare (floored at 10% so
        // the player can still inch along) and encumbrance. The weight
        // is only worked out for a player who is actually moving.
        let speed = if dir.x != 0.0 || dir.z != 0.0 {
            let speed_buff = buffs::speed_mult(&conn.active_buffs);
            let snare = buffs::snare_amount(&conn.active_buffs);
            let snare_mult = (1.0 - snare).max(0.1);
            let (load_mult, _) = crate::world::encumbrance::for_conn(conn);
            RUN_SPEED * speed_buff * snare_mult * load_mult
        } else {
            0.0
        };
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
                        &mut *server,
                        std::slice::from_ref(mover_id),
                        entity,
                    );
                }
            } else if peer_entity >= protocol::world::LOOT_BAG_ID_BASE {
                // Loot bag OR corpse — they share the id partition (corpses
                // mint from mint_bag_id). Seed the mover with whichever it is.
                if let Some(bag) = loot_bags.get(&peer_entity) {
                    handlers::fan_out_loot_bag_spawn(
                        &mut *server,
                        std::slice::from_ref(mover_id),
                        bag,
                    );
                } else if let Some(corpse) = corpses.get(&peer_entity) {
                    handlers::fan_out_corpse_spawn(
                        &mut *server,
                        std::slice::from_ref(mover_id),
                        corpse,
                    );
                    // Slice 2 — if the approaching player owns this corpse,
                    // privately seed its contents so they can loot it.
                    if corpse.owner_char == *mover_id as i64 {
                        handlers::send_corpse_contents(&mut *server, *mover_id, corpse);
                    }
                }
            } else if peer_entity < protocol::world::ENEMY_ID_BASE {
                // Player — mutual EntitySpawn.
                let peer_id = peer_entity as ClientId;
                // Mover → peer: peer can now see the mover
                if let Some(mover_conn) = connections.get(mover_id) {
                    if mover_conn.in_world {
                        handlers::send_entity_spawn(&mut *server, peer_id, mover_conn);
                        handlers::fan_out_resources(
                            &mut *server,
                            std::slice::from_ref(&peer_id),
                            mover_conn,
                        );
                        handlers::fan_out_buff_snapshot(
                            &mut *server,
                            std::slice::from_ref(&peer_id),
                            mover_conn,
                        );
                    }
                }
                // Peer → mover: mover can now see the peer
                if let Some(peer_conn) = connections.get(&peer_id) {
                    if peer_conn.in_world {
                        handlers::send_entity_spawn(&mut *server, *mover_id, peer_conn);
                        handlers::fan_out_resources(
                            &mut *server,
                            std::slice::from_ref(mover_id),
                            peer_conn,
                        );
                        handlers::fan_out_buff_snapshot(
                            &mut *server,
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
                    handlers::send_entity_despawn(&mut *server, *mover_id, peer_entity);
                }
            } else {
                // Player — mutual despawn.
                let peer_id = peer_entity as ClientId;
                handlers::send_entity_despawn(&mut *server, peer_id, mover_entity);
                handlers::send_entity_despawn(&mut *server, *mover_id, peer_entity);
            }
        }
    }
}
