//! Player deaths (step 5a-bis): anyone at 0 HP this tick dies once: hate wiped, the XP
//! penalty, the corpse written with the owner's stores in one transaction, and the death
//! fanned.

use super::*;

pub(super) async fn run_death_sweep(
    world: &mut WorldState,
    _q: &mut Intents,
    now: Instant,
    in_world_recipients_now: &[ClientId],
) {
    let WorldState {
        pool,
        server,
        connections,
        aoi,
        enemies,
        corpses,
        terrain,
        ..
    } = world;
    // 5a-bis. Corpse / resurrection Slice 0 — server-authoritative death
    //     detection. After all combat damage this tick, any in-world player
    //     whose hp reached zero (and hasn't already been processed) dies
    //     here: the xp penalty + de-level apply, the on-death resets run,
    //     and EntityDied fans to peers. `death_processed` keeps this from
    //     re-firing every tick while hp stays 0; the Respawn handler clears
    //     it. The client's DeathBroadcast still covers death causes the
    //     server doesn't simulate (e.g. client-side fall damage).
    let death_recipients: Vec<ClientId> = connections
        .iter()
        .filter(|(_, c)| c.in_world)
        .map(|(id, _)| *id)
        .collect();
    let mut newly_dead: Vec<EntityId> = Vec::new();
    for conn in connections.values_mut() {
        if conn.in_world && conn.hp <= 0.0 && !conn.death_processed {
            crate::world::progression::kill_player(&mut *server, conn);
            conn.death_processed = true;
            // The dead sing nothing (Bard songs): the lingering effects
            // run out on their own.
            conn.active_song = None;
            conn.song_next_pulse_at = None;
            handlers::fan_out_entity_died(&mut *server, &death_recipients, conn.char_id as u64);
            newly_dead.push(conn.char_id as u64);
            tracing::info!(
                char_id = conn.char_id,
                level = conn.level,
                "server-detected player death",
            );
        }
    }
    // Break aggro on death. Excluding the dead from the AI target list (step
    // 4-ai) stops enemies picking them up again, but a mob mid-Chase still
    // holds them in its aggro/threat tables; wiping the entry makes it drop
    // the target immediately and leash home rather than standing over the
    // corpse. Also keeps a dead player from skewing kill credit on a mob
    // someone else finishes.
    if !newly_dead.is_empty() {
        for mob_id in wipe_hate(&mut *enemies, &newly_dead) {
            handlers::fan_out_entity_target(&mut *server, in_world_recipients_now, mob_id, None);
        }
        for bearer in clear_player_dots_from(&mut *connections, &newly_dead) {
            if let Some(conn) = connections.get(&bearer) {
                handlers::fan_out_buff_snapshot(&mut *server, in_world_recipients_now, conn);
            }
        }
    }

    // Corpse / resurrection Slice 1 — move each freshly-dead player's gear +
    // coin onto a persisted corpse, then strip them naked. Keyed off the
    // `corpse_pending` flag `kill_player` sets, so EVERY death path leaves a
    // corpse: the server-detected sweep above AND a client-first
    // DeathBroadcast (Test Panel Trigger Death, fall damage, a linkdead body
    // that dies) handled earlier this tick. ORDER MATTERS: persist the corpse
    // FIRST so a crash before the inventory clear leaves the gear recoverable
    // on the corpse (never duped-then-lost). If a save fails we skip the
    // strip this tick and set corpse_pending again, so the corpse is
    // retried next tick rather than forgotten. Outside the values_mut loop
    // so we can await the DB.
    let corpse_pending: Vec<i64> = connections
        .values()
        .filter(|c| c.corpse_pending)
        .map(|c| c.char_id)
        .collect();
    for cid in corpse_pending {
        let client_id = cid as ClientId;
        if let Some(c) = connections.get_mut(&client_id) {
            c.corpse_pending = false;
        }
        let (owner_name, zone, pos, items, coins, lost_xp, after_death) =
            match connections.get(&client_id) {
                Some(c) => (
                    c.name.clone(),
                    c.zone.clone().unwrap_or_default(),
                    c.pos,
                    c.inventory.corpse_stacks(),
                    c.coins,
                    c.death_lost_xp,
                    crate::world::progression::current_progress(c),
                ),
                None => continue,
            };
        // Always leave a corpse, even an empty one (player died naked +
        // broke). A corpse is the Cleric's resurrection anchor (Slice 3), so
        // someone who dies mid-corpse-run with nothing on them STILL needs a
        // body to be rezzed back to. An empty corpse is just a corpses row
        // with no items; it renders and decays like any other.
        // 0. Flush the owner's dirty stores first. save_corpse writes the
        //    inventory and wallet but not the vaults, so an unsaved bank
        //    withdrawal would go onto the corpse while the vault row still
        //    held it. A failed flush is treated like a failed save_corpse:
        //    nothing is stripped this tick and the corpse is retried on
        //    the next one (corpse_pending is set again), so a passing
        //    database error cannot let a death skip its corpse.
        if let Some(c) = connections.get_mut(&client_id) {
            if let Err(e) = persistence::persist_dirty_stores(pool, c).await {
                tracing::error!(char_id = cid, error = %e, "pre-corpse store flush failed; corpse retried next tick");
                c.corpse_pending = true;
                continue;
            }
        }
        let corpse_id = crate::world::loot::mint_bag_id();
        // 1. Create the corpse AND strip the owner in ONE transaction. These
        //    are two halves of a single movement — the gear and coin leave
        //    the player and arrive on the body — so they must not be able to
        //    half-happen. Written separately (as they were), a crash between
        //    them left the corpse holding everything while the player still
        //    had it: a straight duplication, of coin especially.
        //    The ledger's death rows (one per stack and one for the coin)
        //    join the queue the flush above emptied and ride the same
        //    transaction; a failed save takes them back off the queue
        //    with the retry.
        let death_mark = connections
            .get(&client_id)
            .map(|c| c.ledger_mark())
            .unwrap_or(0);
        let events: Vec<db::ItemEventRow> = match connections.get_mut(&client_id) {
            Some(c) => {
                let detail = Some(format!("corpse:{corpse_id}"));
                for (path, count, _) in &items {
                    c.ledger(ledger::item_event(
                        ItemAction::Death,
                        path,
                        *count,
                        -i64::from(*count),
                        detail.clone(),
                    ));
                }
                let copper = coins.total_copper();
                if copper != 0 {
                    c.ledger(ledger::coin_event(ItemAction::DeathCoins, -copper, detail));
                }
                c.item_events.clone()
            }
            None => continue,
        };
        if let Err(e) = db::save_corpse(
            pool,
            corpse_id as i64,
            cid,
            &owner_name,
            &zone,
            (pos.x, pos.y, pos.z),
            coins,
            &items,
            lost_xp,
            true, // strip the owner in the same transaction
            // The death penalty's XP, with the corpse that records the loss:
            // a crash cannot hand the XP back while the corpse still holds
            // its refund.
            Some(after_death),
            &events,
        )
        .await
        {
            tracing::error!(char_id = cid, error = %e, "save_corpse failed; inventory intact, corpse retried next tick");
            if let Some(c) = connections.get_mut(&client_id) {
                c.corpse_pending = true;
                c.ledger_rollback(death_mark);
            }
            continue;
        }
        // 2. Mirror that committed state in memory and tell the client: the
        //    emptied snapshot drives the naked respawn, plus gear-free max
        //    stats and the zeroed wallet. The DB write already happened
        //    above, so there is nothing further to persist here.
        if let Some(conn) = connections.get_mut(&client_id) {
            conn.inventory.clear_all();
            inventory::recompute_equipped_stats(conn);
            conn.coins = protocol::world::Coins::ZERO;
            // Already durable via save_corpse's transaction. Clearing these
            // stops the next checkpoint rewriting rows it does not need to.
            conn.inventory_dirty = false;
            conn.coins_dirty = false;
            conn.item_events.clear();
            let entries = conn.inventory.to_snapshot_entries();
            handlers::send_inventory_snapshot(&mut *server, client_id, entries);
            handlers::fan_out_resources(&mut *server, std::slice::from_ref(&client_id), conn);
            handlers::send_coins_update(&mut *server, client_id, protocol::world::Coins::ZERO);
        }
        // 3. Spawn the corpse: AOI + map, fan CorpseSpawn to nearby peers.
        let cell = aoi::cell_for(pos.x, pos.z);
        aoi.insert(corpse_id, cell);
        let visible = aoi.entities_visible_from(cell);
        let recipients: Vec<ClientId> = in_world_recipients_now
            .iter()
            .copied()
            .filter(|id| visible.contains(id))
            .collect();
        let stacks: Vec<crate::world::loot::LootItemStack> = items
            .iter()
            .map(|(p, n, worn)| crate::world::loot::LootItemStack {
                item_path: p.clone(),
                count: *n,
                equip_slot: *worn,
            })
            .collect();
        // On the ground where they fell (the body is pinned each tick, so
        // this is the same point; it stays so if a pin is ever missed).
        let corpse = crate::world::corpses::Corpse::new(
            corpse_id,
            cid,
            owner_name,
            zone,
            terrain.ground_vec(pos),
            stacks,
            coins,
            lost_xp,
            false,
            now,
        );
        handlers::fan_out_corpse_spawn(&mut *server, &recipients, &corpse);
        // Slice 2 — privately seed the owner with the corpse contents so they
        // can loot it (peers got only the render-only CorpseSpawn above).
        handlers::send_corpse_contents(&mut *server, client_id, &corpse);
        corpses.insert(corpse_id, corpse);
        tracing::info!(
            char_id = cid,
            corpse_id,
            item_stacks = items.len(),
            "corpse created"
        );
    }
}
