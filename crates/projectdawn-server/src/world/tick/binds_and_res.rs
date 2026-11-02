//! Binds and resurrection (steps 4k-bind and 4k-res): bind points written the moment
//! they are set, and accepted resurrection offers (the owner moved to the corpse, the XP
//! refund, the sickness).

use super::*;

pub(super) async fn persist_binds(world: &mut WorldState, q: &mut Intents, _now: Instant) {
    let WorldState { pool, .. } = world;
    let Intents { bind_intents, .. } = q;
    // 4k-bind. Persist bind points immediately rather than waiting for the
    //      60 s checkpoint. Binding is deliberate and rare, and losing it to
    //      a crash (a clean stop saves everyone, but a crash does not) would
    //      respawn the player somewhere they explicitly chose not to be —
    //      the exact failure this feature exists to prevent.
    for (char_id, zone, pos) in bind_intents.drain(..) {
        if let Err(e) = db::set_bind_point(&pool, char_id, zone.as_deref(), pos).await {
            tracing::warn!(char_id, error = %e, "set_bind_point failed; bind is live in memory but not persisted");
        }
    }
}

pub(super) async fn apply_res_accepts(world: &mut WorldState, q: &mut Intents, now: Instant) {
    let WorldState { pool, server, connections, corpses, .. } = world;
    let Intents { resurrect_accept_intents, .. } = q;
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
                    &mut *server, responder_cid, "That corpse can't be resurrected.",
                );
                continue;
            }
        };
        // Persist the rezzed flag FIRST — only grant the res once it's durable,
        // so a write failure can't hand out a refund that a restart would let
        // the player claim a second time. The refunded XP is written in the
        // same transaction, so a crash cannot spend the resurrection and lose
        // the refund. On failure, skip the grant (a fresh cast can retry).
        let refund = ((lost_xp as f32) * (xp_percent as f32 / 100.0)).round() as i32;
        let Some(after_refund) = connections
            .get(&responder_cid)
            .map(|c| crate::world::progression::progress_after(c, refund.max(0)))
        else {
            continue;
        };
        if let Err(e) =
            db::set_corpse_resurrected(&pool, corpse_id as i64, responder as i64, after_refund).await
        {
            tracing::error!(corpse_id, error = %e, "set_corpse_resurrected failed — res not granted");
            // Transient, and NOT decisive: the offer stays live so the player
            // can simply accept again, rather than needing the cleric to
            // notice and re-cast. Previously this ate the resurrection.
            handlers::send_refusal(
                &mut *server,
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
        // re-levels + fans XpGained/LevelUp, landing on `after_refund`).
        if let Some(conn) = connections.get_mut(&responder_cid) {
            conn.pos = corpse_pos;
            handlers::send_teleport(&mut *server, responder_cid, corpse_pos);
            if refund > 0 {
                crate::world::progression::award_xp(&mut *server, conn, refund);
            }
            // Resurrection sickness (the corpse plan's slice 3, the P99
            // design): the stat cut rides the stat-buff path, so the
            // buff tick expires it and undoes its deltas like any other
            // stat buff; the cut is over the INTRINSIC stats so the
            // client can compute the same from race, class and level.
            // Then the landing itself: a quarter of max HP and no mana,
            // one-shot, however the player arrived. Not damage, so no
            // stand and no camp cancel ride on it.
            let level = conn.level;
            let intrinsic = crate::char_data::compute(&conn.race, &conn.class, level).stats;
            apply_stat_buff(conn, buffs::res_sickness_buff(level, &intrinsic, now));
            conn.hp = conn
                .hp
                .min((conn.max_hp * buffs::RES_SICKNESS_ARRIVAL_HP_FRACTION).max(1.0));
            conn.mp = 0.0;
            regen::mark_dirty(conn);
        }
        let recipients: Vec<ClientId> = connections
            .iter()
            .filter(|(_, c)| c.in_world)
            .map(|(id, _)| *id)
            .collect();
        if let Some(conn) = connections.get(&responder_cid) {
            fan_out_server_buff_snapshot(&mut *server, &recipients, conn);
            handlers::fan_out_health_update(&mut *server, &recipients, responder, conn.hp, conn.max_hp);
            handlers::fan_out_mana_update(&mut *server, &recipients, responder, conn.mp, conn.max_mp);
        }
        handlers::fan_out_chat_message(
            &mut *server,
            &[responder_cid],
            "",
            protocol::world::ChatChannel::System,
            "You are exhausted.",
        );
        tracing::info!(char_id = responder, corpse_id, xp_percent, refund, "resurrection accepted");
    }
}
