//! A player's own inventory (steps 4hd to 4hh-quad): moves, splits, drops to the
//! ground, equips and unequips, destroys, eating and drinking by hand, and GM gives. A
//! refused move answers with the true slots (`correct_client_slots`).

use super::*;

/// How many DropItem intents one player may have applied per tick. Each
/// dropper's drops are saved in one commit per tick; the cap bounds what a
/// forged burst can make that commit (and the bag fan) carry. An honest
/// client drops one stack per click, far below it.
pub(super) const MAX_DROPS_PER_TICK: u32 = 4;

/// Tell one client the true contents of the two slots a failed `MoveItem`
/// named, so a client whose view has drifted can correct itself.
///
/// The server is authoritative over inventory, but authority only helps if
/// disagreement is *reported*. Before this, a rejected or no-op move sent the
/// client nothing at all: it kept rendering a phantom item, kept asking to move
/// it, and kept being refused, with no path back to the truth short of a relog.
/// A playtest on 2026-08-18 caught the same slots refused for half an hour.
///
/// Sending the real contents of exactly the two slots involved keeps this
/// proportional: at most two small messages per bad request the client made, so
/// it cannot be used to amplify traffic, and it converges because every wrong
/// belief is corrected the moment the client acts on it.
pub(super) fn correct_client_slots(
    server: &mut RenetServer,
    cid: ClientId,
    conn: &PerConnection,
    src_loc: &str,
    src_slot: u32,
    dst_loc: &str,
    dst_slot: u32,
) {
    let mut targets: Vec<(&str, u32)> = vec![(src_loc, src_slot)];
    // A move onto itself names one slot twice; no reason to send it twice.
    if !(src_loc == dst_loc && src_slot == dst_slot) {
        targets.push((dst_loc, dst_slot));
    }
    for (loc, slot) in targets {
        // Empty is a real answer and the one the client most needs: it is
        // usually the phantom item that started the divergence.
        handlers::send_slot_delta(
            server,
            cid,
            loc.to_string(),
            slot,
            conn.inventory.peek_at(loc, slot),
        );
        // Full-bags-move: when the corrected slot is a base slot holding a
        // bag, a different-bag correction makes the client re-init that
        // bag's contents vec and wait for inner rows — so re-fan the
        // occupied inners, or the recovery path itself would render the
        // bag empty (the 08-18 vanished-items symptom, on the fix path).
        if loc == "base" {
            if let Some(arr) = conn.inventory.bags.get(&(slot as u8)) {
                for (i, entry) in arr.iter().enumerate() {
                    if let Some(e) = entry {
                        handlers::send_inventory_delta(
                            server,
                            cid,
                            format!("bag_{slot}"),
                            i as u32,
                            Some(e.item_path.clone()),
                            e.count,
                        );
                    }
                }
            }
        }
    }
}

pub(super) fn apply_moves(world: &mut WorldState, q: &mut Intents, _now: Instant) {
    let WorldState {
        server,
        connections,
        trade_manager,
        ..
    } = world;
    let Intents {
        move_item_intents, ..
    } = q;
    // 4hd. Track 13.2 / 14.3 — apply move-item intents. Routes
    //      through `move_across`, which handles every combination
    //      of "base" and "bag_<i>" locations (equip slots stay
    //      on the dedicated equip/unequip path). Touched slots
    //      come back tagged with their location string so the
    //      InventoryDelta fan-out uses the right address per
    //      slot.
    if !move_item_intents.is_empty() {
        for intent in move_item_intents.drain(..) {
            let owner_cid = intent.owner as ClientId;
            // PD_W0028 escrow: an offered slot moves for no one — not
            // out of (src) and not merged/swapped into (dst).
            if trade_locked(
                &mut *server,
                trade_manager,
                owner_cid,
                &intent.src_location,
                intent.src_slot,
            ) || trade_locked(
                &mut *server,
                trade_manager,
                owner_cid,
                &intent.dst_location,
                intent.dst_slot,
            ) {
                continue;
            }
            let Some(conn) = connections.get_mut(&owner_cid) else {
                continue;
            };
            let touched = match conn.inventory.move_across(
                &intent.src_location,
                intent.src_slot,
                &intent.dst_location,
                intent.dst_slot,
            ) {
                Ok(t) => t,
                Err(e) => {
                    tracing::info!(
                        char_id = intent.owner,
                        src_loc = %intent.src_location,
                        src_slot = intent.src_slot,
                        dst_loc = %intent.dst_location,
                        dst_slot = intent.dst_slot,
                        error = %e,
                        "MoveItem rejected"
                    );
                    // A rejection used to send the client NOTHING, which made
                    // every divergence permanent: the client asked to move an
                    // item the server does not have there, learned nothing from
                    // being refused, and asked again. Playtest 2026-08-18 shows
                    // the same slot refused for half an hour, only resolving on
                    // relog. Answer with the truth about both slots so a wrong
                    // client corrects itself on its next mistake.
                    correct_client_slots(
                        &mut *server,
                        owner_cid,
                        conn,
                        &intent.src_location,
                        intent.src_slot,
                        &intent.dst_location,
                        intent.dst_slot,
                    );
                    continue;
                }
            };
            if touched.is_empty() {
                // A move the server treats as a no-op. The client still moved
                // something on its own screen to get here, so it needs the same
                // correction as an outright rejection.
                correct_client_slots(
                    &mut *server,
                    owner_cid,
                    conn,
                    &intent.src_location,
                    intent.src_slot,
                    &intent.dst_location,
                    intent.dst_slot,
                );
                continue;
            }
            conn.inventory_dirty = true;
            // Snapshot the touched slot contents so we can fan
            // Deltas without holding a mutable borrow on conn.
            // peek_at speaks every location move_across can touch
            // (base, bag_<i>, and PD_W0027's cursor), so the fan
            // reads through it rather than re-deriving each address.
            let deltas: Vec<(String, u32, Option<(String, u32)>)> = touched
                .iter()
                .map(|(loc, slot)| (loc.clone(), *slot, conn.inventory.peek_at(loc, *slot)))
                .collect();
            for (loc, slot, payload) in deltas {
                handlers::send_slot_delta(&mut *server, owner_cid, loc, slot, payload);
            }
            tracing::info!(
                char_id = intent.owner,
                src_loc = %intent.src_location,
                src_slot = intent.src_slot,
                dst_loc = %intent.dst_location,
                dst_slot = intent.dst_slot,
                "MoveItem applied"
            );
        }
    }
}

pub(super) fn apply_splits(world: &mut WorldState, q: &mut Intents, _now: Instant) {
    let WorldState {
        server,
        connections,
        trade_manager,
        ..
    } = world;
    let Intents {
        split_stack_intents,
        ..
    } = q;
    // 4he. Track 13.2.b — apply split-stack intents. Splits part
    //      of one base stack into another slot (empty dst or
    //      merge same-path dst). Bag/equip locations defer.
    if !split_stack_intents.is_empty() {
        for intent in split_stack_intents.drain(..) {
            if intent.src_location != "base" || intent.dst_location != "base" {
                tracing::debug!(
                    char_id = intent.owner,
                    src_loc = %intent.src_location,
                    dst_loc = %intent.dst_location,
                    "SplitStack rejected — non-base locations not yet supported"
                );
                handlers::send_refusal(
                    &mut *server,
                    intent.owner as ClientId,
                    "You can't split a stack there yet.",
                );
                continue;
            }
            let owner_cid = intent.owner as ClientId;
            // PD_W0028 escrow: an offered slot neither splits nor
            // receives a split.
            if trade_locked(
                &mut *server,
                trade_manager,
                owner_cid,
                "base",
                intent.src_slot,
            ) || trade_locked(
                &mut *server,
                trade_manager,
                owner_cid,
                "base",
                intent.dst_slot,
            ) {
                continue;
            }
            let Some(conn) = connections.get_mut(&owner_cid) else {
                continue;
            };
            let src = intent.src_slot as usize;
            let dst = intent.dst_slot as usize;
            let touched = match conn.inventory.split_base(src, dst, intent.count) {
                Ok(t) => t,
                Err(e) => {
                    tracing::debug!(
                        char_id = intent.owner,
                        src,
                        dst,
                        count = intent.count,
                        error = %e,
                        "SplitStack rejected"
                    );
                    handlers::send_refusal(
                        &mut *server,
                        owner_cid,
                        "That stack won't split like that.",
                    );
                    continue;
                }
            };
            conn.inventory_dirty = true;
            let deltas: Vec<(u32, Option<(String, u32)>)> = touched
                .iter()
                .map(|&i| {
                    let payload = conn
                        .inventory
                        .base
                        .get(i)
                        .and_then(|s| s.as_ref())
                        .map(|e| (e.item_path.clone(), e.count));
                    (i as u32, payload)
                })
                .collect();
            for (slot, payload) in deltas {
                handlers::send_slot_delta(
                    &mut *server,
                    owner_cid,
                    "base".to_string(),
                    slot,
                    payload,
                );
            }
            tracing::debug!(
                char_id = intent.owner,
                src,
                dst,
                count = intent.count,
                "SplitStack applied"
            );
        }
    }
}

pub(super) async fn apply_drops(
    world: &mut WorldState,
    q: &mut Intents,
    now: Instant,
    in_world_recipients_now: &[ClientId],
) {
    let WorldState {
        pool,
        server,
        connections,
        aoi,
        loot_bags,
        trade_manager,
        ..
    } = world;
    let Intents {
        drop_item_intents, ..
    } = q;
    // 4hf. Track 13.2.b — apply drop-item intents. Removes
    //      `count` from `(base, slot)` (0 = whole stack) and
    //      spawns a server-owned LootBag at the player's pos
    //      via the existing pipeline. The bag is FFA — any
    //      player nearby can pick it up — matching the GDScript
    //      behaviour of "drop on ground".
    //
    //      The removal is made DURABLE before the bag exists. A bag lives
    //      only in memory and its picker is saved at their own logout, so
    //      a dropper left waiting for the 60 s checkpoint could see a
    //      restart hand the item back to them after someone else had
    //      saved it: a dupe. Saved first, a crash can at worst lose a bag
    //      lying on the ground, never copy it.
    //
    //      Three passes so the save costs one commit per dropper per tick,
    //      not one per intent (a forged burst of drops would otherwise run
    //      a commit each inside the tick): remove in memory, at most
    //      MAX_DROPS_PER_TICK per dropper; save each dropper once, undoing
    //      all of their drops if it fails; then tell the client and spawn
    //      the bags for the drops that were saved.
    if !drop_item_intents.is_empty() {
        struct DoneDrop {
            owner_cid: ClientId,
            location: String,
            slot: u32,
            pos: Vec3f,
            item_path: String,
            count: u32,
        }
        let mut before: HashMap<ClientId, (inventory::PlayerInventory, bool, usize)> =
            HashMap::new();
        let mut drops_seen: HashMap<ClientId, u32> = HashMap::new();
        let mut done: Vec<DoneDrop> = Vec::new();
        for intent in drop_item_intents.drain(..) {
            let owner_cid = intent.owner as ClientId;
            // PD_W0028 escrow: an offered slot cannot be dropped.
            if trade_locked(
                &mut *server,
                trade_manager,
                owner_cid,
                &intent.location,
                intent.slot,
            ) {
                continue;
            }
            let Some(conn) = connections.get_mut(&owner_cid) else {
                handlers::send_refusal(&mut *server, owner_cid, "You can't drop that.");
                continue;
            };
            // An honest client drops one stack per click; anything past
            // the cap in one tick is a flood and is ignored.
            let seen = drops_seen.entry(owner_cid).or_insert(0);
            *seen += 1;
            if *seen > MAX_DROPS_PER_TICK {
                continue;
            }
            // Snapshot once per dropper, before their first removal this
            // tick, so a failed save can restore exactly what was there.
            before.entry(owner_cid).or_insert_with(|| {
                (
                    conn.inventory.clone(),
                    conn.inventory_dirty,
                    conn.ledger_mark(),
                )
            });
            // Bag-aware removal (base or bag_<i>); mirrors the
            // DestroyItem apply path. Rejects a non-empty
            // bag-typed slot (the bag must be emptied first).
            match conn
                .inventory
                .destroy_at(&intent.location, intent.slot, intent.count)
            {
                Ok((item_path, count)) => {
                    conn.inventory_dirty = true;
                    conn.ledger(ledger::item_event(
                        ItemAction::Drop,
                        &item_path,
                        count,
                        -i64::from(count),
                        Some("ground".to_string()),
                    ));
                    done.push(DoneDrop {
                        owner_cid,
                        location: intent.location,
                        slot: intent.slot,
                        pos: conn.pos,
                        item_path,
                        count,
                    });
                }
                Err(e) => {
                    tracing::debug!(
                        char_id = intent.owner,
                        loc = %intent.location,
                        slot = intent.slot,
                        error = %e,
                        "DropItem rejected"
                    );
                }
            }
        }
        let mut unsaved: Vec<ClientId> = Vec::new();
        for (owner_cid, (inventory_before, was_dirty, ledger_mark)) in before {
            if !done.iter().any(|d| d.owner_cid == owner_cid) {
                continue;
            }
            let Some(conn) = connections.get_mut(&owner_cid) else {
                continue;
            };
            if let Err(e) = persistence::persist_dirty_stores(pool, conn).await {
                // Nothing was written: put every stack back and refuse.
                // The client was told nothing yet.
                conn.inventory = inventory_before;
                conn.inventory_dirty = was_dirty;
                conn.ledger_rollback(ledger_mark);
                tracing::error!(
                    owner = owner_cid,
                    error = %e,
                    "DropItem persist failed; drops undone"
                );
                handlers::send_refusal(
                    &mut *server,
                    owner_cid,
                    "You can't drop that right now. Try again.",
                );
                unsaved.push(owner_cid);
            }
        }
        for drop in done.into_iter().filter(|d| !unsaved.contains(&d.owner_cid)) {
            // InventoryDelta for the source slot — reflect the new
            // state (residual count or empty). peek_at speaks every
            // droppable location: base, bag_<i>, and the cursor,
            // whose residual a partial drop must not zero out.
            let post = connections
                .get(&drop.owner_cid)
                .and_then(|c| c.inventory.peek_at(&drop.location, drop.slot));
            handlers::send_slot_delta(
                &mut *server,
                drop.owner_cid,
                drop.location.clone(),
                drop.slot,
                post,
            );
            // Spawn a single-stack LootBag at the player's feet
            // and fan via the existing AOI-filtered loot pipeline.
            // Dropped items are public (no kill-owner) — anyone in
            // range may pick them up, as before.
            let bag = loot::LootBag::new(
                drop.pos,
                vec![loot::LootItemStack {
                    item_path: drop.item_path.clone(),
                    count: drop.count,
                    equip_slot: None,
                }],
                protocol::world::Coins::ZERO,
                String::new(), // player-dropped public bag — no creature, keep the sack visual
                None,
                now,
            );
            let bag_id = bag.id;
            let bag_cell = aoi::cell_for(bag.pos.x, bag.pos.z);
            aoi.insert(bag_id, bag_cell);
            let visible = aoi.entities_visible_from(bag_cell);
            let bag_recipients: Vec<ClientId> = in_world_recipients_now
                .iter()
                .copied()
                .filter(|id| visible.contains(id))
                .collect();
            if !bag_recipients.is_empty() {
                handlers::fan_out_loot_bag_spawn(&mut *server, &bag_recipients, &bag);
            }
            loot_bags.insert(bag_id, bag);
            tracing::info!(
                owner = drop.owner_cid,
                slot = drop.slot,
                item_path = %drop.item_path,
                count = drop.count,
                bag_id,
                "DropItem spawned loot bag"
            );
        }
    }
}

pub(super) fn apply_equips(
    world: &mut WorldState,
    q: &mut Intents,
    _now: Instant,
    in_world_recipients_now: &[ClientId],
) {
    let WorldState {
        server,
        connections,
        trade_manager,
        ..
    } = world;
    let Intents {
        equip_item_intents, ..
    } = q;
    // 4hg. Track 13.3 / 14.1 / 14.2 / 15.1 — apply equip-item
    //      intents. Validates src location (base or bag_<i>) +
    //      slot in range + equip_slot in range; item type vs
    //      slot validation lives inside `equip_from_location`;
    //      moves the entry; fans one `InventoryDelta` per
    //      touched slot. Track 14.2 then recomputes the
    //      connection's max HP / MP / stamina / armor from the
    //      registry's stat affixes and fans resource updates on
    //      max change.
    if !equip_item_intents.is_empty() {
        for intent in equip_item_intents.drain(..) {
            let owner_cid = intent.owner as ClientId;
            // PD_W0028 escrow: an offered slot cannot be equipped from.
            if trade_locked(
                &mut *server,
                trade_manager,
                owner_cid,
                &intent.src_location,
                intent.src_slot,
            ) {
                continue;
            }
            let Some(conn) = connections.get_mut(&owner_cid) else {
                continue;
            };
            // The class gate (skills::equip_refusal): a weapon or armor
            // the class never trains, or a weapon in the off hand before
            // it can dual wield, is refused with the line the client
            // already printed; only a modified client reaches this.
            if let Some((path, _)) = conn
                .inventory
                .peek_at(&intent.src_location, intent.src_slot)
            {
                // The other weapon hand, for the two-hander rule.
                let other_hand = match intent.equip_slot {
                    0 => conn.equipped_weapon_path(true),
                    1 => conn.equipped_weapon_path(false),
                    _ => String::new(),
                };
                if let Some(line) = skills::equip_refusal(
                    &conn.class,
                    conn.level,
                    &path,
                    intent.equip_slot,
                    &other_hand,
                ) {
                    tracing::info!(
                        char_id = intent.owner,
                        src_loc = %intent.src_location,
                        src_slot = intent.src_slot,
                        equip_slot = intent.equip_slot,
                        item_path = %path,
                        line,
                        "EquipItem refused — the class cannot use it"
                    );
                    handlers::send_refusal(&mut *server, owner_cid, &line);
                    continue;
                }
            }
            let touched = match conn.inventory.equip_from_location(
                &intent.src_location,
                intent.src_slot,
                intent.equip_slot,
            ) {
                Ok(t) => t,
                Err(e) => {
                    tracing::info!(
                        char_id = intent.owner,
                        src_loc = %intent.src_location,
                        src_slot = intent.src_slot,
                        equip_slot = intent.equip_slot,
                        error = %e,
                        "EquipItem rejected"
                    );
                    handlers::send_refusal(&mut *server, owner_cid, "You can't equip that there.");
                    continue;
                }
            };
            conn.inventory_dirty = true;
            let recompute = inventory::recompute_equipped_stats(conn);
            let deltas: Vec<(String, u32, Option<(String, u32)>)> = touched
                .iter()
                .map(|(loc, slot)| {
                    // peek_at speaks base / bag_<i> / cursor; only the
                    // paperdoll needs its own read here. The old
                    // hand-rolled chain fanned an EMPTY cursor delta on
                    // an equip-from-hand SWAP (playtest 2026-09-09): the
                    // client's hand went blank while the server still
                    // held the swapped-out item, and only a lift-click
                    // swap recovered it.
                    let payload = if loc == "equip" {
                        conn.inventory
                            .equipment
                            .get(&(*slot as u8))
                            .map(|e| (e.item_path.clone(), e.count))
                    } else {
                        conn.inventory.peek_at(loc, *slot)
                    };
                    (loc.clone(), *slot, payload)
                })
                .collect();
            for (loc, slot, payload) in deltas {
                handlers::send_slot_delta(&mut *server, owner_cid, loc, slot, payload);
            }
            if recompute.any_resource_max_changed() {
                handlers::fan_out_resources(&mut *server, in_world_recipients_now, conn);
            }
            tracing::info!(
                char_id = intent.owner,
                src_loc = %intent.src_location,
                src_slot = intent.src_slot,
                equip_slot = intent.equip_slot,
                max_hp = conn.max_hp,
                max_mp = conn.max_mp,
                armor = conn.equipped_armor,
                "EquipItem applied"
            );
        }
    }
}

pub(super) fn apply_unequips(
    world: &mut WorldState,
    q: &mut Intents,
    _now: Instant,
    in_world_recipients_now: &[ClientId],
) {
    let WorldState {
        server,
        connections,
        trade_manager,
        ..
    } = world;
    let Intents {
        unequip_item_intents,
        ..
    } = q;
    // 4hh. Track 13.3 / 14.2 — apply unequip-item intents.
    //      Mirror of 4hg in the other direction. Track 14.2
    //      runs the same stat recompute + resource fan after
    //      the mutation lands.
    if !unequip_item_intents.is_empty() {
        for intent in unequip_item_intents.drain(..) {
            if intent.dst_location != "base" && intent.dst_location != "cursor" {
                tracing::debug!(
                    char_id = intent.owner,
                    dst_loc = %intent.dst_location,
                    "UnequipItem rejected — dst must be 'base' or 'cursor'"
                );
                handlers::send_refusal(
                    &mut *server,
                    intent.owner as ClientId,
                    "There's nowhere to put that.",
                );
                continue;
            }
            let owner_cid = intent.owner as ClientId;
            // PD_W0028 escrow: an unequip cannot land in (or swap with)
            // an offered slot.
            if intent.dst_location == "base"
                && trade_locked(
                    &mut *server,
                    trade_manager,
                    owner_cid,
                    "base",
                    intent.dst_slot,
                )
            {
                continue;
            }
            let Some(conn) = connections.get_mut(&owner_cid) else {
                continue;
            };
            let dst = intent.dst_slot as usize;
            // PD_W0027 slice 1.5 — "cursor" lifts the worn item into the
            // hand; "base" is the classic unequip-to-slot.
            let result = if intent.dst_location == "cursor" {
                conn.inventory.unequip_to_cursor(intent.equip_slot)
            } else {
                conn.inventory.unequip_to_base(intent.equip_slot, dst)
            };
            let touched = match result {
                Ok(t) => t,
                Err(e) => {
                    tracing::info!(
                        char_id = intent.owner,
                        equip_slot = intent.equip_slot,
                        dst_loc = %intent.dst_location,
                        dst,
                        error = %e,
                        "UnequipItem rejected"
                    );
                    let text = if e == inventory::MoveError::CursorOccupied {
                        "You're already holding something."
                    } else {
                        "You can't take that off right now."
                    };
                    handlers::send_refusal(&mut *server, owner_cid, text);
                    continue;
                }
            };
            conn.inventory_dirty = true;
            let recompute = inventory::recompute_equipped_stats(conn);
            let deltas: Vec<(String, u32, Option<(String, u32)>)> = touched
                .iter()
                .map(|&(loc, slot)| {
                    let payload = match loc {
                        "base" => conn
                            .inventory
                            .base
                            .get(slot as usize)
                            .and_then(|s| s.as_ref())
                            .map(|e| (e.item_path.clone(), e.count)),
                        "equip" => conn
                            .inventory
                            .equipment
                            .get(&(slot as u8))
                            .map(|e| (e.item_path.clone(), e.count)),
                        "cursor" => conn
                            .inventory
                            .cursor
                            .as_ref()
                            .map(|e| (e.item_path.clone(), e.count)),
                        _ => None,
                    };
                    (loc.to_string(), slot, payload)
                })
                .collect();
            for (loc, slot, payload) in deltas {
                handlers::send_slot_delta(&mut *server, owner_cid, loc, slot, payload);
            }
            if recompute.any_resource_max_changed() {
                handlers::fan_out_resources(&mut *server, in_world_recipients_now, conn);
            }
            tracing::info!(
                char_id = intent.owner,
                equip_slot = intent.equip_slot,
                dst,
                max_hp = conn.max_hp,
                max_mp = conn.max_mp,
                armor = conn.equipped_armor,
                "UnequipItem applied"
            );
        }
    }
}

pub(super) fn apply_destroys(world: &mut WorldState, q: &mut Intents, _now: Instant) {
    let WorldState {
        server,
        connections,
        trade_manager,
        ..
    } = world;
    let Intents {
        destroy_item_intents,
        ..
    } = q;
    // 4hh-bis. Track 15.1 — apply destroy-item intents. Decrements
    //      `count` from `(location, slot)` (0 = whole stack) and
    //      fans a single `InventoryDelta` for the touched slot.
    //      Unlike `DropItem` this does NOT spawn a loot bag; the
    //      item is gone for good. Used by trash cell + Destroy
    //      button UI in launcher mode.
    if !destroy_item_intents.is_empty() {
        for intent in destroy_item_intents.drain(..) {
            let owner_cid = intent.owner as ClientId;
            // PD_W0028 escrow: an offered slot cannot be destroyed.
            if trade_locked(
                &mut *server,
                trade_manager,
                owner_cid,
                &intent.location,
                intent.slot,
            ) {
                continue;
            }
            let Some(conn) = connections.get_mut(&owner_cid) else {
                continue;
            };
            let destroyed =
                match conn
                    .inventory
                    .destroy_at(&intent.location, intent.slot, intent.count)
                {
                    Ok(d) => d,
                    Err(e) => {
                        tracing::debug!(
                            char_id = intent.owner,
                            loc = %intent.location,
                            slot = intent.slot,
                            error = %e,
                            "DestroyItem rejected"
                        );
                        handlers::send_refusal(&mut *server, owner_cid, "You can't destroy that.");
                        continue;
                    }
                };
            conn.inventory_dirty = true;
            let (item_path, count) = destroyed;
            conn.ledger(ledger::item_event(
                ItemAction::Destroy,
                &item_path,
                count,
                -i64::from(count),
                None,
            ));
            let payload = conn.inventory.peek_at(&intent.location, intent.slot);
            handlers::send_slot_delta(
                &mut *server,
                owner_cid,
                intent.location.clone(),
                intent.slot,
                payload,
            );
            tracing::info!(
                char_id = intent.owner,
                loc = %intent.location,
                slot = intent.slot,
                %item_path,
                count,
                "DestroyItem applied"
            );
        }
    }
}

pub(super) fn apply_consumables(
    world: &mut WorldState,
    q: &mut Intents,
    _now: Instant,
    in_world_recipients_now: &[ClientId],
) {
    let WorldState {
        server,
        connections,
        trade_manager,
        ..
    } = world;
    let Intents {
        use_consumable_intents,
        eat_emotes,
        ..
    } = q;
    // 4hh-tris. Track 15.2 — apply use-consumable intents. Looks
    //      up the item, validates it's consumable, decrements one
    //      from the stack, fans `InventoryDelta`, applies the
    //      effect (heal-on-use / food / drink) via the buff
    //      pipeline, fans `HealthUpdate` / `ManaUpdate` /
    //      `BuffSnapshot` as needed.
    if !use_consumable_intents.is_empty() {
        for intent in use_consumable_intents.drain(..) {
            let owner_cid = intent.owner as ClientId;
            // PD_W0028 escrow: an offered slot cannot be eaten from.
            if trade_locked(
                &mut *server,
                trade_manager,
                owner_cid,
                &intent.location,
                intent.slot,
            ) {
                continue;
            }
            let Some(conn) = connections.get_mut(&owner_cid) else {
                continue;
            };
            // Peek the item so we know what effect to apply
            // BEFORE the decrement removes it from the slot.
            let peek_path: Option<String> = conn
                .inventory
                .peek_at(&intent.location, intent.slot)
                .map(|(p, _)| p);
            let Some(item_path) = peek_path else {
                tracing::debug!(
                    char_id = intent.owner,
                    loc = %intent.location,
                    slot = intent.slot,
                    "UseConsumable rejected — slot empty"
                );
                handlers::send_refusal(&mut *server, owner_cid, "There's nothing there to use.");
                continue;
            };
            let Some(item) = items::lookup(&item_path) else {
                tracing::debug!(
                    char_id = intent.owner,
                    %item_path,
                    "UseConsumable rejected — unknown item path"
                );
                handlers::send_refusal(&mut *server, owner_cid, "You can't use that.");
                continue;
            };
            let is_heal_potion = item.heal_on_use > 0.0 || item.mp_on_use > 0.0;
            if !item.is_food && !item.is_drink && !is_heal_potion {
                tracing::debug!(
                    char_id = intent.owner,
                    %item_path,
                    "UseConsumable rejected — item is not consumable"
                );
                handlers::send_refusal(&mut *server, owner_cid, "You can't use that.");
                continue;
            }
            // Food and water the EQ way (`hunger.rs`): a hand-eat is
            // refused when there is no room, with EQ's own line, and
            // nothing is taken.
            if item.is_food && hunger::is_full(conn, hunger::Need::Food) {
                tracing::debug!(char_id = intent.owner, "UseConsumable rejected — full");
                handlers::send_refusal(&mut *server, owner_cid, hunger::EXPLODE_FOOD);
                continue;
            }
            if item.is_drink && hunger::is_full(conn, hunger::Need::Drink) {
                tracing::debug!(char_id = intent.owner, "UseConsumable rejected — full");
                handlers::send_refusal(&mut *server, owner_cid, hunger::EXPLODE_DRINK);
                continue;
            }
            // Decrement-and-fan first so the UI loses the slot
            // immediately; the heal / buff effect lands right
            // after. Snapshot item fields we'll need post-decrement
            // (since `items::lookup` borrows the registry, not
            // `conn`, this is just convenience).
            let food_dur = item.food_duration;
            let heal_amt = item.heal_on_use;
            let mp_amt = item.mp_on_use;
            let is_food = item.is_food;
            let is_drink = item.is_drink;
            let _ = match conn.inventory.decrement_at(&intent.location, intent.slot) {
                Ok(p) => p,
                Err(e) => {
                    tracing::debug!(
                        char_id = intent.owner,
                        loc = %intent.location,
                        slot = intent.slot,
                        error = %e,
                        "UseConsumable rejected — decrement failed"
                    );
                    handlers::send_refusal(
                        &mut *server,
                        owner_cid,
                        "You can't use that right now.",
                    );
                    continue;
                }
            };
            conn.inventory_dirty = true;
            conn.ledger(ledger::item_event(
                if is_food || is_drink {
                    ItemAction::Eat
                } else {
                    ItemAction::Use
                },
                &item.path,
                1,
                -1,
                Some("by hand".to_string()),
            ));
            // Inventory Delta for the touched slot.
            let payload = conn.inventory.peek_at(&intent.location, intent.slot);
            handlers::send_slot_delta(
                &mut *server,
                owner_cid,
                intent.location.clone(),
                intent.slot,
                payload,
            );
            // Apply the effect.
            if heal_amt > 0.0 {
                let before = conn.hp;
                conn.hp = (conn.hp + heal_amt).min(conn.max_hp);
                if conn.hp != before {
                    handlers::fan_out_health_update(
                        &mut *server,
                        in_world_recipients_now,
                        conn.char_id as u64,
                        conn.hp,
                        conn.max_hp,
                    );
                }
            }
            if mp_amt > 0.0 {
                let before = conn.mp;
                conn.mp = (conn.mp + mp_amt).min(conn.max_mp);
                if conn.mp != before {
                    handlers::fan_out_mana_update(
                        &mut *server,
                        in_world_recipients_now,
                        conn.char_id as u64,
                        conn.mp,
                        conn.max_mp,
                    );
                }
            }
            // An item eaten by hand feeds for half its size (EQ: "food
            // lasts half as long if you click it"); there is no buff any
            // more, a fed character simply regenerates.
            if is_food && food_dur > 0.0 {
                hunger::feed(
                    conn,
                    hunger::Need::Food,
                    food_dur * hunger::HAND_EAT_FRACTION,
                );
                handlers::send_system_line(
                    &mut *server,
                    owner_cid,
                    &format!("You eat a {}.", item.name),
                );
                eat_emotes.push((
                    owner_cid,
                    hunger::eat_emote(&conn.name, &item.name, hunger::Need::Food),
                ));
            }
            if is_drink && food_dur > 0.0 {
                hunger::feed(
                    conn,
                    hunger::Need::Drink,
                    food_dur * hunger::HAND_EAT_FRACTION,
                );
                handlers::send_system_line(
                    &mut *server,
                    owner_cid,
                    &format!("You drink a {}.", item.name),
                );
                eat_emotes.push((
                    owner_cid,
                    hunger::eat_emote(&conn.name, &item.name, hunger::Need::Drink),
                ));
            }
            tracing::info!(
                char_id = intent.owner,
                loc = %intent.location,
                slot = intent.slot,
                %item_path,
                is_food,
                is_drink,
                heal_amt,
                mp_amt,
                "UseConsumable applied"
            );
        }
    }
}

pub(super) fn apply_gm_gives(world: &mut WorldState, q: &mut Intents, _now: Instant) {
    let WorldState {
        server,
        connections,
        ..
    } = world;
    let Intents {
        gm_give_intents, ..
    } = q;
    // 4hh-quad. Track 15.2 follow-up — apply GM /give intents.
    //      Server-side spawn of a stack into the player's
    //      inventory by item display name. Looks up via
    //      `items::lookup_by_name`; adds via `add_item_locating`
    //      (caps stacks at registry max_stack, spills into new
    //      slots); fans `InventoryDelta` per touched slot. Unknown
    //      names or full-inventory cases reject with an INFO log.
    if !gm_give_intents.is_empty() {
        for intent in gm_give_intents.drain(..) {
            let owner_cid = intent.owner as ClientId;
            let Some(conn) = connections.get_mut(&owner_cid) else {
                continue;
            };
            let Some(item) = items::lookup_by_name(&intent.item_name) else {
                tracing::info!(
                    char_id = intent.owner,
                    item_name = %intent.item_name,
                    "GmGive rejected — unknown item name"
                );
                continue;
            };
            let item_path = item.path.clone();
            let (touched, leftover) = match conn.inventory.add_item_locating(&item_path, intent.qty)
            {
                Ok(t) => t,
                Err(e) => {
                    tracing::info!(
                        char_id = intent.owner,
                        item_name = %intent.item_name,
                        error = %e,
                        "GmGive rejected — add_item_locating error"
                    );
                    continue;
                }
            };
            let placed = intent.qty - leftover;
            if placed == 0 {
                tracing::info!(
                    char_id = intent.owner,
                    item_name = %intent.item_name,
                    "GmGive rejected — inventory full, no stack placed"
                );
                continue;
            }
            conn.inventory_dirty = true;
            conn.ledger(ledger::item_event(
                ItemAction::GmGive,
                &item.path,
                placed,
                i64::from(placed),
                Some("gm".to_string()),
            ));
            let deltas: Vec<(String, u32, Option<(String, u32)>)> = touched
                .iter()
                .map(|(loc, slot)| (loc.clone(), *slot, conn.inventory.peek_at(loc, *slot)))
                .collect();
            for (loc, slot, payload) in deltas {
                handlers::send_slot_delta(&mut *server, owner_cid, loc, slot, payload);
            }
            tracing::info!(
                char_id = intent.owner,
                item_name = %intent.item_name,
                qty = placed,
                leftover,
                "GmGive applied"
            );
        }
    }
}
