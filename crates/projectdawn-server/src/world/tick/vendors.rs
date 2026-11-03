//! Merchants (steps 4hi and 4hj): buying from a vendor's own stock and selling to it,
//! with the merchant in reach.

use super::*;

pub(super) fn apply_buys(world: &mut WorldState, q: &mut Intents, _now: Instant) {
    let WorldState {
        server,
        connections,
        ..
    } = world;
    let Intents {
        buy_item_intents, ..
    } = q;
    // 4hi. Track 14 follow-up — apply vendor BuyItem intents.
    //      Server validates: item exists, player has enough coins,
    //      inventory has room. On success: deducts coins, grants
    //      via add_item_locating, fans CoinsUpdate + one
    //      InventoryDelta per touched slot. Only an item a merchant
    //      in reach stocks is for sale (npcs.toml `stock`); the
    //      `vendor_id` field stays informational until the client
    //      sends real ids.
    if !buy_item_intents.is_empty() {
        for intent in buy_item_intents.drain(..) {
            let owner_cid = intent.owner as ClientId;
            let Some(conn) = connections.get_mut(&owner_cid) else {
                continue;
            };
            // Proximity gate (dead-intents audit, 2026-08-24): a merchant
            // transaction requires an actual merchant nearby. Without this,
            // a modified client could trade from anywhere; the honest
            // client has always had a 6 m UI gate, so only remote abuse is
            // refused here. `vendor_id` remains informational until the
            // client sends real ids.
            match conn.npc_service("vendor") {
                Err(None) => {
                    tracing::info!(
                        char_id = intent.owner,
                        pos = ?conn.pos,
                        "BuyItem rejected — no vendor within range"
                    );
                    handlers::send_refusal(
                        &mut *server,
                        owner_cid,
                        "There is no merchant near you.",
                    );
                    continue;
                }
                // Factions (2026-10-23): EQ's merchants refuse at Dubious.
                Err(Some(npc)) => {
                    tracing::info!(char_id = intent.owner, npc = %npc.id, level = conn.npc_faction_level(npc).name(), "BuyItem rejected — faction");
                    handlers::send_refusal(
                        &mut *server,
                        owner_cid,
                        &format!("{} will not deal with you.", npc.name),
                    );
                    continue;
                }
                Ok(_) => {}
            }
            // Stock gate: only what a merchant in reach who deals with
            // this character actually sells. Without it every item with a
            // sell-back price was buyable, named-mob drops included, by a
            // client that asked by name.
            let Some(seller) = crate::world::npcs::vendor_stocking_within_range(
                &intent.item_name,
                conn.pos,
                |n| conn.npc_serves(n, crate::world::factions::Level::Apprehensive),
            ) else {
                tracing::info!(
                    char_id = intent.owner,
                    item_name = %intent.item_name,
                    "BuyItem rejected — no merchant in reach stocks it"
                );
                handlers::send_refusal(&mut *server, owner_cid, "The merchant doesn't stock that.");
                continue;
            };
            let Some(item) = items::lookup_by_name(&intent.item_name) else {
                tracing::info!(
                    char_id = intent.owner,
                    item_name = %intent.item_name,
                    "BuyItem rejected — unknown item name"
                );
                handlers::send_refusal(&mut *server, owner_cid, "The merchant doesn't stock that.");
                continue;
            };
            let unit_price = item.vendor_price as i64;
            if unit_price <= 0 {
                tracing::info!(
                    char_id = intent.owner,
                    item_name = %intent.item_name,
                    "BuyItem rejected — item has no vendor_price"
                );
                handlers::send_refusal(&mut *server, owner_cid, "That isn't for sale.");
                continue;
            }
            let total_cost = unit_price.saturating_mul(intent.qty as i64);
            if !conn.coins.can_afford(total_cost) {
                tracing::info!(
                    char_id = intent.owner,
                    item_name = %intent.item_name,
                    coins = conn.coins.total_copper(),
                    cost = total_cost,
                    "BuyItem rejected — insufficient coins"
                );
                handlers::send_refusal(&mut *server, owner_cid, "You can't afford that.");
                continue;
            }
            let item_path = item.path.clone();
            let (touched, leftover) = match conn.inventory.add_item_locating(&item_path, intent.qty)
            {
                Ok(t) => t,
                Err(e) => {
                    tracing::info!(
                        char_id = intent.owner,
                        item_name = %intent.item_name,
                        error = %e,
                        "BuyItem rejected — add_item_locating error"
                    );
                    handlers::send_refusal(&mut *server, owner_cid, "You can't carry that.");
                    continue;
                }
            };
            let placed = intent.qty - leftover;
            if placed == 0 {
                tracing::info!(
                    char_id = intent.owner,
                    item_name = %intent.item_name,
                    "BuyItem rejected — inventory full, no stack placed"
                );
                // The case a tester hit on 2026-08-21: the client had already
                // said "Ordered", so with no reply the item looked like it
                // vanished. Nothing was charged.
                handlers::send_refusal(&mut *server, owner_cid, "Your bags are full.");
                continue;
            }
            // Charge only for what we actually placed. The pre-flight
            // can_afford check above used total_cost ≥ actual_cost, so this
            // spend always succeeds; make-change handles tier breaking.
            let actual_cost = unit_price.saturating_mul(placed as i64);
            conn.coins.spend(actual_cost);
            conn.coins_dirty = true;
            conn.inventory_dirty = true;
            let mut bought = ledger::item_event(
                ItemAction::Buy,
                &item_path,
                placed,
                i64::from(placed),
                Some(format!("npc:{}", seller.id)),
            );
            bought.copper = -actual_cost;
            conn.ledger(bought);
            let coins_after = conn.coins;
            let deltas: Vec<(String, u32, Option<(String, u32)>)> = touched
                .iter()
                .map(|(loc, slot)| (loc.clone(), *slot, conn.inventory.peek_at(loc, *slot)))
                .collect();
            for (loc, slot, payload) in deltas {
                handlers::send_slot_delta(&mut *server, owner_cid, loc, slot, payload);
            }
            handlers::send_coins_update(&mut *server, owner_cid, coins_after);
            if leftover > 0 {
                tracing::info!(
                    char_id = intent.owner,
                    item_name = %intent.item_name,
                    qty_requested = intent.qty,
                    qty_placed = placed,
                    leftover,
                    "BuyItem partially filled — inventory ran out of room"
                );
                // Billing is already correct — only `placed` is charged — but
                // asking for ten and receiving six with no word said reads as
                // the game losing four of them.
                handlers::send_refusal(
                    &mut *server,
                    owner_cid,
                    &format!("Only {placed} fit in your bags."),
                );
            }
            tracing::info!(
                char_id = intent.owner,
                item_name = %intent.item_name,
                qty = placed,
                cost = actual_cost,
                coins_after = coins_after.total_copper(),
                seller = %seller.id,
                vendor_type = seller.vendor_type.as_deref().unwrap_or(""),
                "BuyItem applied"
            );
        }
    }
}

pub(super) fn apply_sells(world: &mut WorldState, q: &mut Intents, _now: Instant) {
    let WorldState {
        server,
        connections,
        trade_manager,
        ..
    } = world;
    let Intents {
        sell_item_intents, ..
    } = q;
    // 4hj. Track 14 follow-up — apply vendor SellItem intents.
    //      Server: looks up the slot, validates item count, computes
    //      sell price (vendor_price / 2 per unit, mirroring the
    //      GDScript), credits coins, removes the sold stack (base
    //      or bag-inner slot), fans CoinsUpdate + InventoryDelta.
    //      Equip-slot sells reject (player should unequip first).
    if !sell_item_intents.is_empty() {
        for intent in sell_item_intents.drain(..) {
            let owner_cid = intent.owner as ClientId;
            // PD_W0028 escrow: an offered slot cannot be sold.
            let (lock_loc, lock_slot) = match intent.slot {
                protocol::world::SlotRef::BaseSlot { idx } => ("base".to_string(), idx as u32),
                protocol::world::SlotRef::BagSlot { base, slot } => {
                    (format!("bag_{base}"), slot as u32)
                }
                protocol::world::SlotRef::Cursor => ("cursor".to_string(), 0),
                protocol::world::SlotRef::EquipSlot(_) => (String::new(), 0),
            };
            if !lock_loc.is_empty()
                && trade_locked(
                    &mut *server,
                    &trade_manager,
                    owner_cid,
                    &lock_loc,
                    lock_slot,
                )
            {
                continue;
            }
            let Some(conn) = connections.get_mut(&owner_cid) else {
                continue;
            };
            // Same proximity gate as BuyItem above. The merchant in reach
            // names the ledger row.
            let vendor_detail = match conn.npc_service("vendor") {
                Err(None) => {
                    tracing::info!(
                        char_id = intent.owner,
                        pos = ?conn.pos,
                        "SellItem rejected — no vendor within range"
                    );
                    handlers::send_refusal(
                        &mut *server,
                        owner_cid,
                        "There is no merchant near you.",
                    );
                    continue;
                }
                Err(Some(npc)) => {
                    tracing::info!(char_id = intent.owner, npc = %npc.id, level = conn.npc_faction_level(npc).name(), "SellItem rejected — faction");
                    handlers::send_refusal(
                        &mut *server,
                        owner_cid,
                        &format!("{} will not deal with you.", npc.name),
                    );
                    continue;
                }
                Ok(npc) => format!("npc:{}", npc.id),
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
                    let Some(entry) = conn.inventory.base.get(i).and_then(|s| s.as_ref()) else {
                        tracing::info!(
                            char_id = intent.owner,
                            "SellItem rejected — base slot empty"
                        );
                        handlers::send_refusal(
                            &mut *server,
                            owner_cid,
                            "There's nothing in that slot to sell.",
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
                    // A held bag's contents (bag_255) are not sellable —
                    // no window is open on it, so the intent is forged.
                    if base == inventory::CURSOR_BAG_KEY {
                        continue;
                    }
                    let Some(arr) = conn.inventory.bags.get(&base) else {
                        tracing::info!(
                            char_id = intent.owner,
                            "SellItem rejected — bag slot has no bag at base"
                        );
                        handlers::send_refusal(
                            &mut *server,
                            owner_cid,
                            "There's nothing in that slot to sell.",
                        );
                        continue;
                    };
                    let Some(entry) = arr.get(slot as usize).and_then(|s| s.as_ref()) else {
                        tracing::info!(
                            char_id = intent.owner,
                            "SellItem rejected — bag slot empty"
                        );
                        handlers::send_refusal(
                            &mut *server,
                            owner_cid,
                            "There's nothing in that slot to sell.",
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
                // Sell-from-cursor (PD_W0029, 2026-10-30): the held stack,
                // which the client lists ahead of the bags at a merchant.
                protocol::world::SlotRef::Cursor => {
                    let Some(entry) = conn.inventory.cursor.as_ref() else {
                        tracing::info!(
                            char_id = intent.owner,
                            "SellItem rejected — nothing in hand"
                        );
                        handlers::send_refusal(
                            &mut *server,
                            owner_cid,
                            "There's nothing in your hand to sell.",
                        );
                        continue;
                    };
                    (
                        "cursor".to_string(),
                        0,
                        entry.item_path.clone(),
                        entry.count,
                    )
                }
                protocol::world::SlotRef::EquipSlot(_) => {
                    tracing::info!(
                        char_id = intent.owner,
                        "SellItem rejected — equip slot sells not supported"
                    );
                    handlers::send_refusal(
                        &mut *server,
                        owner_cid,
                        "Take it off before selling it.",
                    );
                    continue;
                }
            };
            let Some(item) = items::lookup(&item_path) else {
                tracing::info!(
                    char_id = intent.owner,
                    item_path = %item_path,
                    "SellItem rejected — unknown item path"
                );
                handlers::send_refusal(&mut *server, owner_cid, "The merchant won't take that.");
                continue;
            };
            let unit_price = (item.vendor_price as i64) / 2;
            if unit_price <= 0 {
                tracing::info!(
                    char_id = intent.owner,
                    item_path = %item_path,
                    "SellItem rejected — item has no sell value"
                );
                handlers::send_refusal(&mut *server, owner_cid, "The merchant won't take that.");
                continue;
            }
            let qty = intent.qty.min(available_count);
            // Refuse to sell a non-empty bag, from a base slot or the hand
            // (a bag's contents ride the cursor with it since 2026-09-27).
            let bag_key = match intent.slot {
                protocol::world::SlotRef::BaseSlot { idx } => Some(idx),
                protocol::world::SlotRef::Cursor => Some(inventory::CURSOR_BAG_KEY),
                _ => None,
            };
            if bag_key.is_some_and(|k| conn.inventory.bag_key_is_nonempty(k)) {
                tracing::info!(
                    char_id = intent.owner,
                    "SellItem rejected — bag has contents"
                );
                handlers::send_refusal(&mut *server, owner_cid, "Empty the bag before selling it.");
                continue;
            }
            // Mutate: take the sold count off the source through the one
            // removal path (`destroy_at`: a base slot, a bag slot or the
            // hand; it empties the slot at zero and tidies a bag's inner
            // store), then read the slot back for the delta. The three
            // hand-rolled copies it replaces were the 2026-10-30 review's
            // finding when the cursor became the third.
            let qty = match conn.inventory.destroy_at(&location_str, slot_u32, qty) {
                Ok((_, removed)) => removed,
                Err(why) => {
                    tracing::warn!(
                        char_id = intent.owner,
                        %location_str,
                        slot = slot_u32,
                        %why,
                        "SellItem rejected — could not take the item"
                    );
                    handlers::send_refusal(
                        &mut *server,
                        owner_cid,
                        "The merchant won't take that.",
                    );
                    continue;
                }
            };
            let total_credit = unit_price.saturating_mul(qty as i64);
            let new_count = conn
                .inventory
                .peek_at(&location_str, slot_u32)
                .map_or(0, |(_, c)| c);
            conn.coins.add_payout(total_credit);
            conn.coins_dirty = true;
            conn.inventory_dirty = true;
            let mut sold = ledger::item_event(
                ItemAction::Sell,
                &item_path,
                qty,
                -i64::from(qty),
                Some(vendor_detail),
            );
            sold.copper = total_credit;
            conn.ledger(sold);
            let coins_after = conn.coins;
            let delta_item: Option<String> = if new_count > 0 {
                Some(item_path.clone())
            } else {
                None
            };
            handlers::send_inventory_delta(
                &mut *server,
                owner_cid,
                location_str,
                slot_u32,
                delta_item,
                new_count,
            );
            handlers::send_coins_update(&mut *server, owner_cid, coins_after);
            tracing::info!(
                char_id = intent.owner,
                item_path = %item_path,
                qty,
                credit = total_credit,
                coins_after = coins_after.total_copper(),
                "SellItem applied"
            );
        }
    }
}
