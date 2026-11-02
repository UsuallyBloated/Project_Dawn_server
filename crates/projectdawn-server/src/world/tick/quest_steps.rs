//! (phase module, doc to come)

use super::*;

pub(super) fn apply_quest_lifecycle(world: &mut WorldState, q: &mut Intents, _now: Instant) {
    let WorldState { server, connections, .. } = world;
    let Intents { quest_lifecycle_intents, .. } = q;
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
                let Some(quest) = crate::world::quests::lookup(&quest_id) else {
                    tracing::info!(char_id = responder, quest_id = ?quest_id, "quest accept rejected — unknown quest id");
                    handlers::send_quest_rejected(&mut *server, responder_cid, &quest_id, "Unknown quest.", true);
                    continue;
                };
                let Some(conn) = connections.get_mut(&responder_cid) else {
                    continue; // disconnected mid-tick
                };
                if conn.level < quest.level_req {
                    tracing::info!(char_id = responder, quest_id = %quest_id, level = conn.level, level_req = quest.level_req, "quest accept rejected — below level_req");
                    handlers::send_quest_rejected(&mut *server, responder_cid, &quest_id, "You are too low level for this quest.", true);
                    continue;
                }
                if conn.completed_quests.contains(&quest_id) {
                    tracing::info!(char_id = responder, quest_id = %quest_id, "quest accept rejected — already completed");
                    handlers::send_quest_rejected(&mut *server, responder_cid, &quest_id, "You have already completed this quest.", true);
                    continue;
                }
                if conn.active_quests.contains_key(&quest_id) {
                    // Already tracking (a duplicate send, e.g. a re-click
                    // before the journal refreshed) — not an error, and no
                    // rollback (the quest legitimately stays).
                    tracing::debug!(char_id = responder, quest_id = %quest_id, "quest accept ignored — already active");
                    continue;
                }
                if conn.active_quests.len() >= crate::world::quests::MAX_ACTIVE {
                    tracing::info!(char_id = responder, quest_id = %quest_id, "quest accept rejected — journal full");
                    handlers::send_quest_rejected(&mut *server, responder_cid, &quest_id, "Your quest journal is full.", true);
                    continue;
                }
                // The giver's faction (2026-10-23): the quest's NPC (its
                // turn-in NPC gives it too) has nothing to say at
                // Apprehensive or below, so the quest is not offered.
                if let Some(npc) = quest.turn_in_npc.as_deref().and_then(crate::world::npcs::lookup) {
                    if !conn.npc_serves(npc, crate::world::factions::Level::Indifferent) {
                        tracing::info!(char_id = responder, quest_id = %quest_id, npc = %npc.id, "quest accept rejected — faction");
                        handlers::send_quest_rejected(&mut *server, responder_cid, &quest_id, &format!("{} has nothing to say to you.", npc.name), true);
                        continue;
                    }
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
}

pub(super) async fn apply_quest_turn_ins(world: &mut WorldState, q: &mut Intents, _now: Instant) {
    let WorldState { pool, server, connections, enemies, .. } = world;
    let Intents { complete_quest_intents, consider_intents, .. } = q;
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
    // Considers of mobs (factions, 2026-10-23): the faction phrase from
    // the considerer's standing with the mob's faction (Indifferent for a
    // mob with none, a dev spawn or a pet), then the level phrase. A dead
    // or unknown id gets the same line a player id gets in the handler.
    for (requester, target_id) in consider_intents.drain(..) {
        let Some(conn) = connections.get(&requester) else {
            continue;
        };
        let line = match enemies.get(&target_id).filter(|e| e.is_alive()) {
            Some(e) => {
                let level = e
                    .mob
                    .faction
                    .as_deref()
                    .and_then(crate::world::factions::lookup)
                    .map(|f| conn.faction_level(f))
                    .unwrap_or(crate::world::factions::Level::Indifferent);
                crate::world::factions::mob_con_line(&e.mob.name, level, conn.level, e.mob.level as i32)
            }
            None => "You cannot consider that.".to_string(),
        };
        handlers::send_system_line(&mut *server, requester, &line);
    }
    for (responder, quest_id) in complete_quest_intents.drain(..) {
        let responder_cid = responder as ClientId;
        let Some(quest) = crate::world::quests::lookup(&quest_id) else {
            // {:?} escapes control chars — the id is attacker-controlled and
            // a raw newline could forge log lines in the triage artifact.
            tracing::info!(char_id = responder, quest_id = ?quest_id, "quest turn-in rejected — unknown quest id");
            handlers::send_quest_rejected(&mut *server, responder_cid, &quest_id, "Unknown quest.", false);
            continue;
        };
        let Some(reward) = crate::world::quests::xp_reward_for(&quest_id) else {
            tracing::warn!(char_id = responder, quest_id = %quest_id, "quest turn-in rejected — bad reward tier in quests.toml");
            handlers::send_quest_rejected(&mut *server, responder_cid, &quest_id, "Quest data error.", false);
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
            if !crate::world::npcs::id_within_range(npc_id, conn_ro.pos) {
                let npc_name = crate::world::npcs::lookup(npc_id).map(|n| n.name.as_str()).unwrap_or(npc_id);
                tracing::info!(char_id = responder, quest_id = %quest_id, npc = npc_id, "quest turn-in rejected — too far from the turn-in NPC");
                handlers::send_quest_rejected(
                    &mut *server,
                    responder_cid,
                    &quest_id,
                    &format!("You must return to {npc_name}."),
                    false,
                );
                continue;
            }
        }
        // The turn-in NPC's faction (2026-10-23): at Apprehensive or
        // below the NPC has nothing to say, as EQ's quest givers.
        if let Some(npc) = quest.turn_in_npc.as_deref().and_then(crate::world::npcs::lookup) {
            if !conn_ro.npc_serves(npc, crate::world::factions::Level::Indifferent) {
                tracing::info!(char_id = responder, quest_id = %quest_id, npc = %npc.id, level = conn_ro.npc_faction_level(npc).name(), "quest turn-in rejected — faction");
                handlers::send_quest_rejected(
                    &mut *server,
                    responder_cid,
                    &quest_id,
                    &format!("{} has nothing to say to you.", npc.name),
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
            handlers::send_quest_rejected(&mut *server, responder_cid, &quest_id, "You are too low level for this quest.", false);
            continue;
        }
        if conn_ro.completed_quests.contains(&quest_id) {
            tracing::info!(char_id = responder, quest_id = %quest_id, "quest turn-in rejected — already completed");
            handlers::send_quest_rejected(&mut *server, responder_cid, &quest_id, "You have already completed this quest.", false);
            continue;
        }
        // PD_W0024 — the objective check. The quest must actually be in
        // the server-side journal (an AcceptQuest landed) with every
        // count met. This closes the phase-1 residual exploit: a forged
        // CompleteQuest with no kills behind it pays nothing.
        let Some(progress) = conn_ro.active_quests.get(&quest_id) else {
            tracing::info!(char_id = responder, quest_id = %quest_id, "quest turn-in rejected — not in journal");
            handlers::send_quest_rejected(&mut *server, responder_cid, &quest_id, "That quest is not in your journal.", false);
            continue;
        };
        if !crate::world::quests::objectives_met(quest, progress) {
            tracing::info!(char_id = responder, quest_id = %quest_id, progress = ?progress, "quest turn-in rejected — objectives incomplete");
            handlers::send_quest_rejected(&mut *server, responder_cid, &quest_id, "Quest objectives are not complete.", false);
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
            handlers::send_quest_rejected(&mut *server, responder_cid, &quest_id, "Your inventory is full — make room and try again.", false);
            continue;
        }
        // The reward XP is written in the completion's own transaction:
        // award_xp below lands on these same numbers in memory.
        let after_reward = crate::world::progression::progress_after(conn_ro, reward);
        if let Err(e) =
            db::record_quest_completion(&pool, responder as i64, &quest_id, after_reward).await
        {
            tracing::error!(char_id = responder, quest_id = %quest_id, error = %e, "record_quest_completion failed — reward not granted");
            handlers::send_quest_rejected(&mut *server, responder_cid, &quest_id, "Server error — try again.", false);
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
            handlers::send_quest_completed(&mut *server, responder_cid, &quest_id);
            crate::world::progression::award_xp(&mut *server, conn, reward);
            // Factions and alignment (2026-10-23): the quest's explicit
            // hits (sorted, so the lines come in one order) and its
            // alignment delta, after the XP as EQ's turn-ins paid.
            let mut hits: Vec<(String, f32)> =
                quest.faction_hits.iter().map(|(k, v)| (k.clone(), *v)).collect();
            hits.sort_by(|a, b| a.0.cmp(&b.0));
            handlers::apply_faction_hits(&mut *server, responder_cid, conn, &hits);
            handlers::move_alignment(&mut *server, responder_cid, conn, quest.alignment_delta);
            // Grant item reward(s). Capacity was pre-checked, so
            // add_item_locating fits; the completion is already durable, so
            // a crash here can only lose the item, never dup it (the forced
            // save below closes even that window). Items ride the existing
            // InventoryDelta + LootGranted path — same as looting.
            for (path, count) in &rewards {
                match conn.inventory.add_item_locating(path, *count) {
                    Ok((touched, leftover)) => {
                        conn.inventory_dirty = true;
                        if *count > leftover {
                            conn.ledger(ledger::item_event(
                                ItemAction::QuestReward,
                                path,
                                *count - leftover,
                                i64::from(*count - leftover),
                                Some(quest_id.clone()),
                            ));
                        }
                        if leftover > 0 {
                            tracing::error!(char_id = responder, quest_id = %quest_id, %path, leftover, "quest reward partially placed despite capacity pre-check");
                        }
                        for (loc, slot) in &touched {
                            let (dpath, dcount) = match conn.inventory.peek_at(loc, *slot) {
                                Some((p, c)) => (Some(p), c),
                                None => (None, 0),
                            };
                            handlers::send_inventory_delta(&mut *server, responder_cid, loc.clone(), *slot, dpath, dcount);
                        }
                        handlers::send_loot_granted(&mut *server, responder_cid, path.clone(), count.saturating_sub(leftover));
                    }
                    Err(e) => {
                        tracing::error!(char_id = responder, quest_id = %quest_id, %path, error = e, "quest reward grant failed after completion recorded");
                    }
                }
            }
            tracing::info!(char_id = responder, quest_id = %quest_id, reward, "quest completed");
        }
        // Force-persist the granted inventory so a crash after the (already
        // durable) completion record can't lose the reward. It goes through
        // the all-stores flush, never the inventory alone: writing only the
        // inventory while a bank withdrawal or a purchase sat unsaved left
        // the item in the bag AND the vault (or the coins unspent) after a
        // crash. Rare path; a failed save falls back to the checkpoint.
        if !rewards.is_empty() {
            if let Some(conn) = connections.get_mut(&responder_cid) {
                if let Err(e) = persistence::persist_dirty_stores(&pool, conn).await {
                    tracing::warn!(char_id = responder, quest_id = %quest_id, error = %e, "quest reward store save failed (will retry on checkpoint)");
                }
            }
        }
    }
}
