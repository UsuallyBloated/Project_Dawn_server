//! Loot (steps 4ka, 4jc, 4k and 4k-bis): taking from loot bags and corpses, ground
//! pickups to the cursor, and bags and corpses expiring.

use super::*;

/// Corpse / resurrection — a corpse despawns ONLY when a loot action emptied it:
/// it held something before and is empty now. A corpse that was ALREADY empty (a
/// naked-death res anchor) is NOT removed by a Take-All — it lingers for a Cleric
/// resurrection. A named predicate so this linger rule is testable and hard to
/// flip by accident (see the corpse-loot branch above).
pub(super) fn corpse_emptied_by_loot(had_content: bool, empty_now: bool) -> bool {
    had_content && empty_now
}

pub(super) async fn apply_loot_intents(
    world: &mut WorldState,
    q: &mut Intents,
    _now: Instant,
    in_world_recipients_now: &[ClientId],
) {
    let WorldState {
        pool,
        server,
        connections,
        aoi,
        loot_bags,
        group_manager,
        corpses,
        ..
    } = world;
    let Intents {
        loot_intents,
        loot_to_cursor_intents,
        ..
    } = q;
    // 4ka. Apply loot pickup intents. For each: validate bag, slot
    //      bounds, and looter range. On success drain the relevant
    //      stack(s), send LootGranted privately to the looter, and
    //      either re-broadcast LootBagSpawn (bag still has items)
    //      or EntityDespawn (bag is empty) to every in_world peer.
    //      Out-of-range / unknown-bag / out-of-slot intents drop
    //      silently — the GDScript UI already gates the click on
    //      LOOT_RANGE so a legitimate user can't trip this.
    //
    //      Everyone who took an item out of a loot bag this tick (here or
    //      by the ground pickup in 4jc) is saved once after both phases.
    //      A dropped stack left its dropper's saved inventory the moment
    //      it hit the ground; left unsaved in the picker's hands until the
    //      checkpoint, a crash would lose it, the dropper's own regrab
    //      included.
    let mut bag_looters: Vec<ClientId> = Vec::new();
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
                            &mut *server,
                            looter_cid,
                            "That is not your corpse.".to_string(),
                        );
                        continue;
                    }
                }
                // Flush the looter's other dirty stores FIRST. The corpse
                // transaction below rewrites the inventory and wallet but
                // not the vaults, so an unsaved bank withdrawal would ride
                // into the DB inventory while the vault row still held the
                // item: a crash then leaves it in both. Flushed beforehand,
                // each commit records a state that was whole in memory.
                if let Some(c) = connections.get_mut(&looter_cid) {
                    if let Err(e) = persistence::persist_dirty_stores(&pool, c).await {
                        tracing::error!(corpse_id, error = %e, "pre-loot store flush failed; corpse loot refused");
                        handlers::send_loot_rejected(
                            &mut *server,
                            looter_cid,
                            "You can't loot that right now. Try again.".to_string(),
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
                let corpse_items_before: Vec<(String, u32, Option<u8>)> = {
                    let corpse = corpses.get(&corpse_id).unwrap();
                    corpse
                        .items
                        .iter()
                        .map(|s| (s.item_path.clone(), s.count, s.equip_slot))
                        .collect()
                };
                let corpse_coins_before = corpses.get(&corpse_id).unwrap().coins;
                let had_content = !corpse_items_before.is_empty()
                    || corpse_coins_before != protocol::world::Coins::ZERO;
                let (inv_before, coins_before, ledger_before) = match connections.get(&looter_cid) {
                    Some(c) => (c.inventory.clone(), c.coins, c.ledger_mark()),
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
                        c.ledger(ledger::coin_event(
                            ItemAction::CorpseCoins,
                            corpse_coins.total_copper(),
                            Some(format!("corpse:{corpse_id}")),
                        ));
                        coin_update = Some(c.coins);
                    }
                }
                // Items: take one (slot) or all (None) off the corpse.
                let mut granted: Vec<(String, u32, Option<u8>)> = Vec::new();
                {
                    let corpse = corpses.get_mut(&corpse_id).unwrap();
                    match intent.slot {
                        Some(idx) => {
                            let i = idx as usize;
                            if i < corpse.items.len() {
                                let stack = corpse.items.remove(i);
                                granted.push((stack.item_path, stack.count, stack.equip_slot));
                            }
                        }
                        None => {
                            for stack in corpse.items.drain(..) {
                                granted.push((stack.item_path, stack.count, stack.equip_slot));
                            }
                        }
                    }
                }
                // Worn gear first, so it goes back on before the carried
                // stacks fill the bags (a stable sort keeps each group in
                // corpse order).
                granted.sort_by_key(|(_, _, worn)| worn.is_none());
                // Corpse auto-re-equip, then the bags. Gear worn at death goes
                // back on the slot it was worn in when that slot is empty and
                // still takes it (`restore_worn`); anything else goes to the
                // bags by the same add_item_locating path the bag loot uses,
                // and a full inventory refunds the unplaced portion to the
                // corpse, keeping its worn slot for a later loot. Collect the
                // client deltas: nothing is sent until the persist commits.
                let mut inv_deltas: Vec<(String, u32, String, u32)> = Vec::new();
                let mut granted_lines: Vec<(String, u32)> = Vec::new();
                let mut reequipped: u32 = 0;
                let mut left_on_corpse: u32 = 0;
                for (path, count, worn) in granted {
                    if let Some(slot) = worn {
                        if let Some(conn) = connections.get_mut(&looter_cid) {
                            if conn.inventory.restore_worn(&path, count, slot) {
                                conn.inventory_dirty = true;
                                conn.ledger(ledger::item_event(
                                    ItemAction::CorpseLoot,
                                    &path,
                                    count,
                                    i64::from(count),
                                    Some(format!("corpse:{corpse_id}")),
                                ));
                                reequipped += 1;
                                inv_deltas.push((
                                    "equip".to_string(),
                                    slot as u32,
                                    path.clone(),
                                    count,
                                ));
                                granted_lines.push((path, count));
                                continue;
                            }
                        }
                    }
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
                            if placed_count > 0 {
                                conn.ledger(ledger::item_event(
                                    ItemAction::CorpseLoot,
                                    &path,
                                    placed_count,
                                    i64::from(placed_count),
                                    Some(format!("corpse:{corpse_id}")),
                                ));
                            }
                        }
                    }
                    if placed_count > 0 {
                        granted_lines.push((path.clone(), placed_count));
                    }
                    if leftover_count > 0 {
                        left_on_corpse += 1;
                        corpses
                            .get_mut(&corpse_id)
                            .unwrap()
                            .items
                            .push(loot::LootItemStack {
                                item_path: path,
                                count: leftover_count,
                                equip_slot: worn,
                            });
                    }
                }
                // Gear went back on: the gear bonuses and max resources change
                // with it (fanned after the persist commits, below).
                let mut resources_changed = false;
                if reequipped > 0 {
                    if let Some(conn) = connections.get_mut(&looter_cid) {
                        resources_changed =
                            inventory::recompute_equipped_stats(conn).any_resource_max_changed();
                    }
                    tracing::info!(
                        corpse_id,
                        char_id = intent.looter,
                        reequipped,
                        "corpse loot put worn gear back on"
                    );
                }
                // Post-drain corpse state for the atomic persist + the despawn
                // decision (corpse borrow dropped before the await below).
                let (corpse_items_now, corpse_coins_now, corpse_pos, corpse_empty) = {
                    let corpse = corpses.get(&corpse_id).unwrap();
                    (
                        corpse
                            .items
                            .iter()
                            .map(|s| (s.item_path.clone(), s.count, s.equip_slot))
                            .collect::<Vec<(String, u32, Option<u8>)>>(),
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
                    .map(|c| (c.inventory.to_rows(), c.coins, c.item_events.clone()));
                let persist_ok = if let Some((inv_rows, looter_coins, events)) = persist_data {
                    match db::apply_corpse_loot(
                        &pool,
                        intent.looter as i64,
                        &inv_rows,
                        looter_coins,
                        corpse_id as i64,
                        &corpse_items_now,
                        corpse_coins_now,
                        delete_now,
                        &events,
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
                        // apply_corpse_loot wrote the whole queue.
                        c.item_events.clear();
                    }
                    if let Some(coins) = coin_update {
                        handlers::send_coins_update(&mut *server, looter_cid, coins);
                    }
                    for (loc, slot, item_path, total_count) in inv_deltas {
                        handlers::send_inventory_delta(
                            &mut *server,
                            looter_cid,
                            loc,
                            slot,
                            Some(item_path),
                            total_count,
                        );
                    }
                    for (path, n) in granted_lines {
                        handlers::send_loot_granted(&mut *server, looter_cid, path, n);
                    }
                    if resources_changed {
                        if let Some(conn) = connections.get(&looter_cid) {
                            handlers::fan_out_resources(
                                &mut *server,
                                &in_world_recipients_now,
                                conn,
                            );
                        }
                    }
                    // Say why anything is still on the body, instead of
                    // leaving the player to work out that the bags filled.
                    if left_on_corpse > 0 {
                        let line = if left_on_corpse == 1 {
                            "Your bags are full: one item stays on your corpse.".to_string()
                        } else {
                            format!(
                                "Your bags are full: {left_on_corpse} items stay on your corpse."
                            )
                        };
                        handlers::send_refusal(&mut *server, looter_cid, &line);
                    }
                    if delete_now {
                        // Looted clean -> the body vanishes (the DB rows were
                        // already deleted inside apply_corpse_loot's tx).
                        corpses.remove(&corpse_id);
                        let cell = aoi::cell_for(corpse_pos.x, corpse_pos.z);
                        aoi.remove(corpse_id, cell);
                        let visible = aoi.entities_visible_from(cell);
                        for &recipient in in_world_recipients_now.iter() {
                            if visible.contains(&recipient) {
                                handlers::send_entity_despawn(&mut *server, recipient, corpse_id);
                            }
                        }
                        tracing::info!(
                            corpse_id,
                            char_id = intent.looter,
                            "corpse looted empty — despawned"
                        );
                    } else if let Some(corpse) = corpses.get(&corpse_id) {
                        handlers::send_corpse_contents(&mut *server, looter_cid, corpse);
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
                            .map(|(item_path, count, equip_slot)| loot::LootItemStack {
                                item_path,
                                count,
                                equip_slot,
                            })
                            .collect();
                        corpse.coins = corpse_coins_before;
                    }
                    if let Some(conn) = connections.get_mut(&looter_cid) {
                        conn.inventory = inv_before;
                        conn.coins = coins_before;
                        conn.ledger_rollback(ledger_before);
                        // Undo the gear bonuses of anything put back on.
                        if reequipped > 0 {
                            inventory::recompute_equipped_stats(conn);
                        }
                    }
                    handlers::send_loot_rejected(
                        &mut *server,
                        looter_cid,
                        "Couldn't loot the corpse — try again.".to_string(),
                    );
                    if let Some(corpse) = corpses.get(&corpse_id) {
                        handlers::send_corpse_contents(&mut *server, looter_cid, corpse);
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
                    char_id = intent.looter,
                    bag_id = intent.bag_id,
                    "loot rejected: not the owner or owner's group"
                );
                handlers::send_loot_rejected(
                    &mut *server,
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
                            char_id = intent.looter,
                            bag_id = intent.bag_id,
                            assigned = turn,
                            "loot rejected: not your turn (round robin)"
                        );
                        handlers::send_loot_rejected(
                            &mut *server,
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
                        char_id = intent.looter,
                        bag_id = intent.bag_id,
                        slot = idx,
                        "loot rejected: no such slot in the bag"
                    );
                    handlers::send_loot_rejected(
                        &mut *server,
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
                        c.ledger(ledger::coin_event(
                            ItemAction::LootCoins,
                            amount,
                            Some(format!("bag:{}", intent.bag_id)),
                        ));
                        let coins_after = c.coins;
                        handlers::send_coins_update(&mut *server, r, coins_after);
                    }
                }
                tracing::info!(
                    char_id = intent.looter,
                    bag_id = intent.bag_id,
                    pot,
                    recipients = recipients.len(),
                    do_split,
                    "coin looted"
                );
            }
            // The ledger: a creature's bag is loot, a player's drop a
            // pickup, named by the bag and what it was.
            let loot_kind = if bag.creature_name.is_empty() {
                ItemAction::Pickup
            } else {
                ItemAction::Loot
            };
            let loot_detail = if bag.creature_name.is_empty() {
                format!("bag:{}", intent.bag_id)
            } else {
                format!("bag:{} {}", intent.bag_id, bag.creature_name)
            };
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
                            if placed_count > 0 {
                                conn.ledger(ledger::item_event(
                                    loot_kind,
                                    &path,
                                    placed_count,
                                    i64::from(placed_count),
                                    Some(loot_detail.clone()),
                                ));
                            }
                        }
                        Err(e) => {
                            tracing::info!(
                                char_id = intent.looter,
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
                        &mut *server,
                        looter_cid,
                        loc,
                        slot,
                        Some(item_path),
                        total_count,
                    );
                }
                if placed_count > 0 {
                    handlers::send_loot_granted(
                        &mut *server,
                        looter_cid,
                        path.clone(),
                        placed_count,
                    );
                    if !bag_looters.contains(&looter_cid) {
                        bag_looters.push(looter_cid);
                    }
                }
                if leftover_count > 0 {
                    // Refund — push the unplaced portion back into
                    // the bag so it stays available. Bag fan-out
                    // below re-broadcasts the updated contents.
                    bag.items.push(loot::LootItemStack {
                        item_path: path,
                        count: leftover_count,
                        equip_slot: None,
                    });
                    tracing::info!(
                        char_id = intent.looter,
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
                for &recipient in in_world_recipients_now.iter() {
                    if bag_visible.contains(&recipient) {
                        handlers::send_entity_despawn(&mut *server, recipient, bag_id);
                    }
                }
            } else {
                let bag_recipients: Vec<ClientId> = in_world_recipients_now
                    .iter()
                    .copied()
                    .filter(|id| bag_visible.contains(id))
                    .collect();
                handlers::fan_out_loot_bag_spawn(&mut *server, &bag_recipients, bag);
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
            let (stack, dropped_by_a_player) = {
                let Some(bag) = loot_bags.get_mut(&bag_id) else {
                    handlers::send_refusal(&mut *server, looter_cid, "That isn't there any more.");
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
                        &mut *server,
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
                        bag.assigned_looter = group_manager.next_loot_turn(owner_cid, |cand| {
                            connections
                                .get(&cand)
                                .map(|c| c.pos.distance_to(bag_pos) <= GROUP_SHARE_RANGE)
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
                                &mut *server,
                                looter_cid,
                                "Not your turn to loot.".to_string(),
                            );
                            continue;
                        }
                    }
                }
                if bag.items.len() != 1 || bag.coins != protocol::world::Coins::ZERO {
                    handlers::send_refusal(
                        &mut *server,
                        looter_cid,
                        "There's more than one thing there. Right-click to loot.",
                    );
                    continue;
                }
                (bag.items[0].clone(), bag.creature_name.is_empty())
            };
            let Some(conn) = connections.get_mut(&looter_cid) else {
                continue;
            };
            if conn.inventory.cursor.is_some() {
                handlers::send_refusal(
                    &mut *server,
                    looter_cid,
                    "You're already holding something.",
                );
                continue;
            }
            // Commit: cursor in, bag out, world told. All in this tick.
            conn.inventory.cursor = Some(crate::world::inventory::InventoryEntry {
                item_path: stack.item_path.clone(),
                count: stack.count,
            });
            conn.inventory_dirty = true;
            conn.ledger(ledger::item_event(
                if dropped_by_a_player {
                    ItemAction::Pickup
                } else {
                    ItemAction::Loot
                },
                &stack.item_path,
                stack.count,
                i64::from(stack.count),
                Some(format!("bag:{bag_id}")),
            ));
            handlers::send_inventory_delta(
                &mut *server,
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
                &mut *server,
                looter_cid,
                &format!("You pick up {}.", item_name),
            );
            let bag = loot_bags
                .remove(&bag_id)
                .expect("bag existed under the borrow above");
            let bag_cell = aoi::cell_for(bag.pos.x, bag.pos.z);
            let bag_visible = aoi.entities_visible_from(bag_cell);
            aoi.remove(bag_id, bag_cell);
            for &recipient in in_world_recipients_now.iter() {
                if bag_visible.contains(&recipient) {
                    handlers::send_entity_despawn(&mut *server, recipient, bag_id);
                }
            }
            tracing::info!(
                looter,
                bag_id,
                item = %stack.item_path,
                count = stack.count,
                "ground item picked up to cursor"
            );
            if !bag_looters.contains(&looter_cid) {
                bag_looters.push(looter_cid);
            }
        }
    }
    // One save per looter for everything taken from bags this tick (see 4ka).
    // A failure leaves the stores dirty for the checkpoint to retry.
    for looter_cid in bag_looters {
        if let Some(conn) = connections.get_mut(&looter_cid) {
            if let Err(e) = persistence::persist_dirty_stores(&pool, conn).await {
                tracing::warn!(looter = looter_cid, error = %e, "post-loot store save failed; the checkpoint will retry");
            }
        }
    }
}

pub(super) async fn expire_bags_and_corpses(
    world: &mut WorldState,
    _q: &mut Intents,
    now: Instant,
    in_world_recipients_now: &[ClientId],
) {
    let WorldState {
        pool,
        server,
        aoi,
        loot_bags,
        corpses,
        ..
    } = world;
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
            for &recipient in in_world_recipients_now.iter() {
                if visible.contains(&recipient) {
                    handlers::send_entity_despawn(&mut *server, recipient, id);
                }
            }
        }
    }

    // 4k-bis. Corpse / resurrection Slice 1 — corpse decay. Corpses linger
    //     much longer than loot bags (CORPSE_LINGER_SECS); on expiry the
    //     corpse + its gear are gone for good: despawn to visible peers,
    //     remove from AOI + map, AND delete the DB rows so a restart can't
    //     bring it back.
    let corpse_linger = Duration::from_secs_f32(crate::world::corpses::CORPSE_LINGER_SECS);
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
            for &recipient in in_world_recipients_now.iter() {
                if visible.contains(&recipient) {
                    handlers::send_entity_despawn(&mut *server, recipient, id);
                }
            }
            if let Err(e) = db::delete_corpse(&pool, id as i64).await {
                tracing::error!(corpse_id = id, error = %e, "delete_corpse on decay failed");
            }
            tracing::info!(
                corpse_id = id,
                char_id = corpse.owner_char,
                "corpse decayed — gear lost"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::corpse_emptied_by_loot;

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
