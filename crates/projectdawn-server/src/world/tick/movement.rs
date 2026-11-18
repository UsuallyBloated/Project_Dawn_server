//! Movement (steps 5 and 5b): each player's Move integrated once (the step on the
//! ground, the fall off an edge and the landing), then the area-of-interest crossings
//! that spawn and despawn what a mover can see.

use super::*;
use crate::world::fall;
use crate::world::obstacles::{BODY_RADIUS, P2};

/// A landing's consequences (`fall.rs`): the damage as a share of max HP, with
/// no armor, evasion or shield (the user's call of 2026-10-24), no attacker
/// and no credit to anyone; the player is told, and the ordinary damage marker
/// stands them and breaks a `/camp`. The HP bar follows on the resource fan,
/// and a death goes through the death sweep like any other, the corpse where
/// the body landed. A drop inside the safe height costs nothing and says
/// nothing, and so does a harmless one (a GM's `/fly off` in the air).
pub(super) fn land(
    server: &mut RenetServer,
    client_id: ClientId,
    conn: &mut PerConnection,
    landing: fall::Landing,
    now: Instant,
) {
    if landing.harmless {
        return;
    }
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
    let obstacles = terrain.obstacles();
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
        // (Only a GM in flight ever carries a vertical; it stands them too.)
        if dir.x != 0.0 || dir.y != 0.0 || dir.z != 0.0 {
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
        // is only worked out for a player who is actually moving, on foot (a
        // flier's speed is its own, below).
        let speed = if !conn.is_flying() && (dir.x != 0.0 || dir.z != 0.0) {
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
            // already on the ground below it. Death ends a GM's flight; the
            // client ends its own on the death screen.
            conn.vertical = fall::Vertical::default();
            conn.end_flight();
            conn.pos = terrain.ground_vec(before);
        } else if let Some(flight) = conn.flight.as_mut() {
            // GM flight (docs/design/handoff_gm_flight.md): three dimensions
            // at the flier's own speed (the boost is the direction's length),
            // untouched by speed buffs, snare or encumbrance (Minecraft's
            // Speed effect does not touch flight either). No slope rule and
            // no fall: the body is held above the server's own ground, on the
            // plate and under the ceiling, the same clamps the client's body
            // applies to itself. Nothing is counted as a slope refusal.
            let reach = flight.speed * dt;
            let (x, z) = terrain.clamp_to_plate(before.x + dir.x * reach, before.z + dir.z * reach);
            let ground = terrain.ground(x, z);
            let ceiling = crate::world::FLY_CEILING.max(ground);
            let y = (before.y + dir.y * reach).clamp(ground, ceiling);
            flight.ground_y = ground;
            conn.vertical = fall::Vertical::default();
            conn.pos = Vec3f { x, y, z };
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
            // What stands on the ground is a column of any height
            // (world/obstacles.rs): a falling body is turned aside by it as a
            // walking one is, so it comes down beside a trunk or a house and
            // never inside one. Turned onto ground above it, it holds its place.
            let walked = obstacles.resolve(P2::of(before), P2::new(x, z), BODY_RADIUS);
            if walked.deflected {
                (x, z) = if terrain.ground(walked.x, walked.z) > before.y {
                    (before.x, before.z)
                } else {
                    (walked.x, walked.z)
                };
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
            // Then what stands on the ground (world/obstacles.rs): the step
            // the terrain allowed is turned aside or stopped by a trunk, a
            // rock or a wall, the body sliding along it as the client's does.
            // The server takes the step itself, so a client that ignores its
            // own obstacles is held at the server's. A step turned onto a face
            // the slope rule forbids is not taken (a rock at a cliff's foot
            // must not push a body up the cliff). A standing body is asked
            // too: one found inside a solid (the scenery changed under it) is
            // put out at the nearest free point, slope rule or not, since
            // that is a placement and not a step.
            let mut to = step.pos;
            let walked = obstacles.resolve(P2::of(before), P2::of(step.pos), BODY_RADIUS);
            if walked.deflected {
                let aside = Vec3f {
                    x: walked.x,
                    y: before.y,
                    z: walked.z,
                };
                to = if obstacles.is_free(P2::of(before), BODY_RADIUS)
                    && !terrain.slope_ok(before, aside)
                {
                    terrain.ground_vec(before)
                } else {
                    terrain.ground_vec(aside)
                };
            }
            let (y, landing) = fall::vertical_step(
                &mut conn.vertical,
                before.y,
                to.y,
                fall::is_floor(terrain.gradient(to.x, to.z)),
                before.xz_distance_to(to),
                dt,
            );
            conn.pos = Vec3f {
                x: to.x,
                y,
                z: to.z,
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
