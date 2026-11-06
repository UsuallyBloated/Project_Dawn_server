//! Movement (steps 5 and 5b): each player's Move integrated once (the step on the
//! ground, the fall off an edge and the landing), then the area-of-interest crossings
//! that spawn and despawn what a mover can see.

use super::*;
use crate::world::fall;

/// A landing's consequences (`fall.rs`): the damage as a share of max HP, with
/// no armor, evasion or shield (the user's call of 2026-10-24), no attacker
/// and no credit to anyone; the player is told, and the ordinary damage marker
/// stands them and breaks a `/camp`. The HP bar follows on the resource fan,
/// and a death goes through the death sweep like any other, the corpse where
/// the body landed. A drop inside the safe height costs nothing and says
/// nothing.
pub(super) fn land(
    server: &mut RenetServer,
    client_id: ClientId,
    conn: &mut PerConnection,
    landing: fall::Landing,
    now: Instant,
) {
    let damage = fall::damage(landing.drop, conn.max_hp);
    if damage <= 0.0 {
        return;
    }
    conn.hp = (conn.hp - damage).max(0.0);
    regen::mark_dirty(conn);
    conn.note_damage_taken(now);
    handlers::send_system_line(
        server,
        client_id,
        &format!("You hit the ground hard for {} damage.", damage as i64),
    );
    tracing::info!(
        char_id = conn.char_id,
        drop_m = landing.drop,
        impact_mps = landing.impact_speed,
        damage,
        hp = conn.hp,
        "fall damage"
    );
}

pub(super) fn run_movement(
    world: &mut WorldState,
    _q: &mut Intents,
    now: Instant,
    dt: f32,
    in_world_recipients_now: &[ClientId],
) {
    let WorldState {
        server,
        connections,
        aoi,
        enemies,
        loot_bags,
        corpses,
        terrain,
        tick_health,
        ..
    } = world;
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
    // but read no direction, or a body that dropped mid-run would keep
    // drifting through the world for the whole linger window. One that
    // dropped in mid-air still falls and lands (fall.rs; the fall design's
    // ledger, row 4): pulling the cable is no way out of a fall.
    for (client_id, conn) in connections.iter_mut().filter(|(_, c)| c.ready) {
        let dir = match conn.last_move_received {
            Some(t)
                if conn.linkdead_since.is_none()
                    && now.duration_since(t) < STALE_MOVE_THRESHOLD =>
            {
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
                handlers::fan_out_buff_snapshot(&mut *server, in_world_recipients_now, conn);
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
        let before = conn.pos;
        let (dx, dz) = (dir.x * speed * dt, dir.z * speed * dt);
        if conn.hp <= 0.0 || conn.death_processed {
            // The dead lie on the ground where they are (the death lock holds
            // their Moves); a body killed in the air stops falling, its corpse
            // already on the ground below it.
            conn.vertical = fall::Vertical::default();
            conn.pos = terrain.ground_vec(before);
        } else if conn.vertical.airborne {
            // In the air the body keeps its heading at its ground speed: no
            // slope under it to shrink the step or to refuse it. Ground it
            // would be under by the end of this tick is met two ways. A face
            // too steep to stand on is a wall: the body slides down it, as the
            // client's body slides off one, onto the first floor the slide
            // reaches, so it lands only on ground it can stand on (or rests
            // against the face for a tick, the fall still open). A floor
            // above the body blocks the step: the body holds its place and
            // falls on there.
            let (mut x, mut z) = (before.x + dx, before.z + dz);
            let falling_to = fall::next_height(&conn.vertical, before.y, dt);
            let met = terrain.ground(x, z);
            if met > falling_to && !fall::is_floor(terrain.gradient(x, z)) {
                (x, z) = terrain.slide_clear(x, z, falling_to, fall::floor_tan());
            } else if met > before.y {
                (x, z) = (before.x, before.z);
            }
            let to = Vec3f { x, y: before.y, z };
            let (y, landing) = fall::vertical_step(
                &mut conn.vertical,
                before.y,
                terrain.ground(x, z),
                fall::is_floor(terrain.gradient(x, z)),
                before.xz_distance_to(to),
                dt,
            );
            conn.pos = Vec3f { x, y, z };
            if let Some(landing) = landing {
                land(&mut *server, *client_id, conn, landing, now);
            }
        } else {
            // The step as the client's body takes it (the height model):
            // shorter up a slope, refused up a face too steep to climb. A
            // refusal is counted for the `world health` line: on flat ground
            // none can happen, so a count there is a modified client or a
            // ground the two sides disagree on; on hills a key held into a
            // cliff counts too (a Move carries the input, not what the body
            // managed). Then the ground under the new XZ: the body follows it
            // down as far as its own floor snap reaches, onto a floor only;
            // past that, or onto a face it cannot stand on, it has left the
            // ground and begins to fall (fall.rs). A standing player is held
            // the same way, so a teleport or a load never leaves one in the
            // air.
            let step = terrain.player_step(before, dx, dz);
            if step.refused {
                tick_health.record_slope_refusal();
            }
            let (y, landing) = fall::vertical_step(
                &mut conn.vertical,
                before.y,
                step.pos.y,
                fall::is_floor(terrain.gradient(step.pos.x, step.pos.z)),
                before.xz_distance_to(step.pos),
                dt,
            );
            conn.pos = Vec3f {
                x: step.pos.x,
                y,
                z: step.pos.z,
            };
            if let Some(landing) = landing {
                land(&mut *server, *client_id, conn, landing, now);
            }
        }

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
            } else if IdKind::of(peer_entity) == IdKind::Bag {
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
            } else if IdKind::of(peer_entity) == IdKind::Player {
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
            if IdKind::of(peer_entity) != IdKind::Player {
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
