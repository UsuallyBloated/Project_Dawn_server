//! Position fan-out (steps 6 and 6b): each player's position to the players who can
//! see them, then each mob's and pet's, sent only when it moved or its keepalive is due.
//! Returns the in-world list the upkeep step reads.

use super::*;

pub(super) fn fan_out_positions(
    world: &mut WorldState,
    _q: &mut Intents,
    now: Instant,
    in_world_ids: &[ClientId],
) {
    let WorldState {
        server,
        connections,
        aoi,
        enemy_pos_sent,
        last_fan_report,
        enemies,
        ..
    } = world;
    // 6. Position fan-out. Track 7: AOI-filtered. Each sender's
    //    position goes only to in_world players whose AOI cell is
    //    within the 3×3 neighbourhood of the sender's cell.
    //    Self-broadcast is preserved (sender is in its own
    //    neighbourhood) — Track 2 snap-or-lerp still needs it.
    //    Encode each sender once; clone bytes only to visible peers.
    for sender_id in in_world_ids {
        let Some(sender) = connections.get(sender_id) else {
            continue;
        };
        let Some(bytes) = handlers::build_position_msg(sender) else {
            continue;
        };
        let visible = aoi.entities_visible_from(sender.aoi_cell);
        for recipient_id in in_world_ids {
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
        *enemy_pos_sent = 0;
        *last_fan_report = now;
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
                *enemy_pos_sent += 1;
            }
        }
        if now.duration_since(*last_fan_report) >= ENEMY_FAN_REPORT_INTERVAL {
            let enemies_alive = enemies.values().filter(|e| e.is_alive()).count();
            tracing::info!(
                sent = *enemy_pos_sent,
                window_secs = now.duration_since(*last_fan_report).as_secs(),
                enemies_alive,
                players_in_world = in_world_ids.len(),
                "enemy position fan"
            );
            // The operator's side of the client's `/ping`: renet's
            // smoothed round trip and loss estimate for each player's
            // link, once a minute, so "it feels laggy" on the public
            // host comes with a number in the log as well as in chat.
            for cid in in_world_ids.iter() {
                let Some(conn) = connections.get(cid) else {
                    continue;
                };
                tracing::info!(
                    char_id = conn.char_id,
                    name = %conn.name,
                    rtt_ms = (server.rtt(*cid) * 1000.0).round(),
                    packet_loss = server.packet_loss(*cid),
                    "player link"
                );
            }
            *enemy_pos_sent = 0;
            *last_fan_report = now;
        }
    }
}
