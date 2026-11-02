//! The banker (steps 4i-bank and 4i-bank2): coin deposits, withdrawals and exchanges,
//! then whole stacks into and out of the character and account vaults, each with a banker
//! in reach.

use super::*;

pub(super) fn apply_coin_bank(world: &mut WorldState, q: &mut Intents, _now: Instant) {
    let WorldState { server, connections, .. } = world;
    let Intents { bank_deposit_intents, bank_withdraw_intents, bank_exchange_intents, .. } = q;
    // 4i-bank. PD_W0015 — Banker, slice 1 (coins). Deposit / withdraw move
    //          per-tier amounts between the carried wallet and the
    //          zero-weight bank balance; exchange converts tiers on the
    //          wallet. All validated against minting (non-negative,
    //          affordable), then fan CoinsUpdate (wallet) + BankSnapshot
    //          (bank) to the actor; failures fan a private BankRejected.
    for intent in bank_deposit_intents.drain(..) {
        let cid = intent.owner as ClientId;
        let Some(conn) = connections.get_mut(&cid) else { continue };
        // Proximity gate (dead-intents audit, 2026-08-24): banking requires
        // the banker. This is the arm that voided the death penalty — a
        // modified client could BankStoreItem its whole inventory from a
        // dungeon one second before dying, leaving the corpse empty.
        match conn.npc_service("banker") {
            Err(None) => {
                tracing::info!(owner = intent.owner, pos = ?conn.pos, "bank op rejected — no banker within range");
                handlers::send_bank_rejected(&mut *server, cid, "There is no banker near you.".to_string());
                continue;
            }
            Err(Some(npc)) => {
                tracing::info!(owner = intent.owner, npc = %npc.id, level = conn.npc_faction_level(npc).name(), "bank op rejected — faction");
                handlers::send_bank_rejected(&mut *server, cid, format!("{} will not deal with you.", npc.name));
                continue;
            }
            Ok(_) => {}
        }
        if intent.coins.has_negative() || intent.coins == protocol::world::Coins::ZERO {
            handlers::send_bank_rejected(&mut *server, cid, "Nothing to deposit.".to_string());
            continue;
        }
        // Pay by VALUE, not tier by tier. Depositing 5 silver from a purse
        // of 10 gold leaves 9 gold 95 silver, and depositing 1 gold from a
        // purse of 100 silver works too. Refusing either reads as a bug
        // rather than as the independent-stacks rule it actually is, and the
        // Banker already offers a free tier exchange, so refusing protected
        // nothing.
        let mut wallet_after = conn.coins;
        if !wallet_after.pay_value_of(intent.coins) {
            handlers::send_bank_rejected(
                &mut *server, cid, "You don't have that much coin to deposit.".to_string(),
            );
            continue;
        }
        conn.coins = wallet_after;
        conn.bank_coins = conn.bank_coins.add_each(intent.coins);
        conn.coins_dirty = true;
        conn.bank_dirty = true;
        conn.ledger(ledger::coin_event(
            ItemAction::BankCoinIn,
            0,
            Some(format!("{} copper", intent.coins.total_copper())),
        ));
        let (wallet, bank) = (conn.coins, conn.bank_coins);
        handlers::send_coins_update(&mut *server, cid, wallet);
        handlers::send_bank_snapshot(&mut *server, cid, bank);
        tracing::info!(
            owner = intent.owner,
            deposited = intent.coins.total_copper(),
            bank = bank.total_copper(),
            "bank deposit"
        );
    }
    for intent in bank_withdraw_intents.drain(..) {
        let cid = intent.owner as ClientId;
        let Some(conn) = connections.get_mut(&cid) else { continue };
        // Same banker proximity gate as the deposit loop above.
        match conn.npc_service("banker") {
            Err(None) => {
                tracing::info!(owner = intent.owner, pos = ?conn.pos, "bank op rejected — no banker within range");
                handlers::send_bank_rejected(&mut *server, cid, "There is no banker near you.".to_string());
                continue;
            }
            Err(Some(npc)) => {
                tracing::info!(owner = intent.owner, npc = %npc.id, level = conn.npc_faction_level(npc).name(), "bank op rejected — faction");
                handlers::send_bank_rejected(&mut *server, cid, format!("{} will not deal with you.", npc.name));
                continue;
            }
            Ok(_) => {}
        }
        if intent.coins.has_negative() || intent.coins == protocol::world::Coins::ZERO {
            handlers::send_bank_rejected(&mut *server, cid, "Nothing to withdraw.".to_string());
            continue;
        }
        // Same rule on the way out, so the bank is symmetric: a balance is
        // worth what it is worth, whichever coins it happens to be in.
        let mut bank_after = conn.bank_coins;
        if !bank_after.pay_value_of(intent.coins) {
            handlers::send_bank_rejected(
                &mut *server, cid, "Your bank doesn't hold that much coin.".to_string(),
            );
            continue;
        }
        conn.bank_coins = bank_after;
        conn.coins = conn.coins.add_each(intent.coins);
        conn.coins_dirty = true;
        conn.bank_dirty = true;
        conn.ledger(ledger::coin_event(
            ItemAction::BankCoinOut,
            0,
            Some(format!("{} copper", intent.coins.total_copper())),
        ));
        let (wallet, bank) = (conn.coins, conn.bank_coins);
        handlers::send_coins_update(&mut *server, cid, wallet);
        handlers::send_bank_snapshot(&mut *server, cid, bank);
        tracing::info!(
            owner = intent.owner,
            withdrew = intent.coins.total_copper(),
            bank = bank.total_copper(),
            "bank withdraw"
        );
    }
    for intent in bank_exchange_intents.drain(..) {
        let cid = intent.owner as ClientId;
        let Some(conn) = connections.get_mut(&cid) else { continue };
        // Same banker proximity gate as the deposit loop above.
        match conn.npc_service("banker") {
            Err(None) => {
                tracing::info!(owner = intent.owner, pos = ?conn.pos, "bank op rejected — no banker within range");
                handlers::send_bank_rejected(&mut *server, cid, "There is no banker near you.".to_string());
                continue;
            }
            Err(Some(npc)) => {
                tracing::info!(owner = intent.owner, npc = %npc.id, level = conn.npc_faction_level(npc).name(), "bank op rejected — faction");
                handlers::send_bank_rejected(&mut *server, cid, format!("{} will not deal with you.", npc.name));
                continue;
            }
            Ok(_) => {}
        }
        match conn.coins.exchange(intent.from_tier, intent.to_tier, intent.qty) {
            Ok(()) => {
                conn.coins_dirty = true;
                conn.ledger(ledger::coin_event(
                    ItemAction::BankExchange,
                    0,
                    Some(format!(
                        "{} of tier {} to tier {}",
                        intent.qty, intent.from_tier, intent.to_tier
                    )),
                ));
                let wallet = conn.coins;
                handlers::send_coins_update(&mut *server, cid, wallet);
                tracing::info!(
                    owner = intent.owner,
                    from = intent.from_tier,
                    to = intent.to_tier,
                    qty = intent.qty,
                    "bank exchange"
                );
            }
            Err(reason) => {
                handlers::send_bank_rejected(&mut *server, cid, reason.to_string());
            }
        }
    }
}

pub(super) fn apply_item_bank(world: &mut WorldState, q: &mut Intents, _now: Instant) {
    let WorldState { server, connections, trade_manager, .. } = world;
    let Intents { bank_store_item_intents, bank_withdraw_item_intents, .. } = q;
    // 4i-bank2. PD_W0016 — Banker, slice 2 (item vaults). Quick-transfer
    //           whole stacks between inventory and a vault. Store sizes the
    //           deposit against the vault's room first (capacity_for) so the
    //           inventory side moves exactly what fits and the source never
    //           relocates; bag-typed items are rejected (MVP). Withdraw
    //           takes the stack then refunds any inventory overflow back to
    //           the vault. Each op fans an InventoryDelta (inventory side) +
    //           a full BankItemSnapshot (the tiny vault).
    for intent in bank_store_item_intents.drain(..) {
        let cid = intent.owner as ClientId;
        // PD_W0028 escrow: an offered slot cannot be banked (this is
        // also the death-penalty-void slot, so the lock matters twice).
        if trade_locked(&mut *server, &trade_manager, cid, &intent.src_location, intent.src_slot) {
            continue;
        }
        let Some(conn) = connections.get_mut(&cid) else { continue };
        // Same banker proximity gate as the deposit loop above.
        match conn.npc_service("banker") {
            Err(None) => {
                tracing::info!(owner = intent.owner, pos = ?conn.pos, "bank op rejected — no banker within range");
                handlers::send_bank_rejected(&mut *server, cid, "There is no banker near you.".to_string());
                continue;
            }
            Err(Some(npc)) => {
                tracing::info!(owner = intent.owner, npc = %npc.id, level = conn.npc_faction_level(npc).name(), "bank op rejected — faction");
                handlers::send_bank_rejected(&mut *server, cid, format!("{} will not deal with you.", npc.name));
                continue;
            }
            Ok(_) => {}
        }
        let Some((path, avail)) =
            conn.inventory.peek_at(&intent.src_location, intent.src_slot)
        else {
            handlers::send_bank_rejected(&mut *server, cid, "Nothing to deposit there.".to_string());
            continue;
        };
        if items::bag_num_slots(&path).is_some() {
            handlers::send_bank_rejected(&mut *server, cid, "Bags can't go in the bank vault.".to_string());
            continue;
        }
        let room = if intent.shared {
            conn.account_bank_items.capacity_for(&path)
        } else {
            conn.bank_items.capacity_for(&path)
        };
        let take = avail.min(room);
        if take == 0 {
            handlers::send_bank_rejected(&mut *server, cid, "Your bank vault is full.".to_string());
            continue;
        }
        // Remove from inventory FIRST and let the actual removed count drive
        // the deposit, so the inventory side is the single source of truth
        // for how much moved (the vault is never credited without debiting
        // inventory). destroy_at can't fail here today — peek_at vetted the
        // slot and bags are rejected above — but bailing on Err keeps a
        // future change from turning this into a dup.
        let removed = match conn.inventory.destroy_at(&intent.src_location, intent.src_slot, take) {
            Ok((_, removed)) => removed,
            Err(_) => {
                handlers::send_bank_rejected(&mut *server, cid, "Couldn't move that item.".to_string());
                continue;
            }
        };
        if removed == 0 {
            continue;
        }
        conn.inventory_dirty = true;
        if intent.shared {
            conn.account_bank_items.deposit(&path, removed);
            conn.account_bank_items_dirty = true;
        } else {
            conn.bank_items.deposit(&path, removed);
            conn.bank_items_dirty = true;
        }
        conn.ledger(ledger::item_event(
            ItemAction::BankIn,
            &path,
            removed,
            0,
            Some(if intent.shared { "account vault" } else { "vault" }.to_string()),
        ));
        let post = conn.inventory.peek_at(&intent.src_location, intent.src_slot);
        let (dpath, dcount) = match post {
            Some((p, c)) => (Some(p), c),
            None => (None, 0),
        };
        handlers::send_inventory_delta(
            &mut *server, cid, intent.src_location.clone(), intent.src_slot, dpath, dcount,
        );
        let entries = if intent.shared {
            conn.account_bank_items.to_snapshot_entries()
        } else {
            conn.bank_items.to_snapshot_entries()
        };
        handlers::send_bank_item_snapshot(&mut *server, cid, intent.shared, entries);
        tracing::info!(owner = intent.owner, %path, deposited = take, shared = intent.shared, "bank store item");
    }
    for intent in bank_withdraw_item_intents.drain(..) {
        let cid = intent.owner as ClientId;
        let Some(conn) = connections.get_mut(&cid) else { continue };
        // Same banker proximity gate as the deposit loop above.
        match conn.npc_service("banker") {
            Err(None) => {
                tracing::info!(owner = intent.owner, pos = ?conn.pos, "bank op rejected — no banker within range");
                handlers::send_bank_rejected(&mut *server, cid, "There is no banker near you.".to_string());
                continue;
            }
            Err(Some(npc)) => {
                tracing::info!(owner = intent.owner, npc = %npc.id, level = conn.npc_faction_level(npc).name(), "bank op rejected — faction");
                handlers::send_bank_rejected(&mut *server, cid, format!("{} will not deal with you.", npc.name));
                continue;
            }
            Ok(_) => {}
        }
        let slot = intent.vault_slot as usize;
        let present = if intent.shared {
            conn.account_bank_items.peek(slot).cloned()
        } else {
            conn.bank_items.peek(slot).cloned()
        };
        let Some(entry) = present else {
            handlers::send_bank_rejected(&mut *server, cid, "Nothing to withdraw there.".to_string());
            continue;
        };
        let path = entry.item_path;
        let count = entry.count;
        if intent.shared {
            conn.account_bank_items.take_all(slot);
            conn.account_bank_items_dirty = true;
        } else {
            conn.bank_items.take_all(slot);
            conn.bank_items_dirty = true;
        }
        let (touched, leftover) = conn
            .inventory
            .add_item_locating(&path, count)
            .unwrap_or((Vec::new(), count));
        if leftover > 0 {
            // Inventory had no room for all of it. Put the overflow straight
            // back into the slot it came from (take_all just emptied it),
            // not via deposit() — restore() guarantees it lands in the same
            // slot and never trips the max_stack cap, so nothing is lost even
            // for an oddly-sized stored stack.
            if intent.shared {
                conn.account_bank_items.restore(slot, &path, leftover);
            } else {
                conn.bank_items.restore(slot, &path, leftover);
            }
            let reason = if leftover == count {
                "No room in your inventory."
            } else {
                "Only part of that fit in your inventory."
            };
            handlers::send_bank_rejected(&mut *server, cid, reason.to_string());
        }
        if !touched.is_empty() {
            conn.inventory_dirty = true;
        }
        if count > leftover {
            conn.ledger(ledger::item_event(
                ItemAction::BankOut,
                &path,
                count - leftover,
                0,
                Some(if intent.shared { "account vault" } else { "vault" }.to_string()),
            ));
        }
        for (loc, slot) in &touched {
            let (dp, dc) = match conn.inventory.peek_at(loc, *slot) {
                Some((p, c)) => (Some(p), c),
                None => (None, 0),
            };
            handlers::send_inventory_delta(&mut *server, cid, loc.clone(), *slot, dp, dc);
        }
        let entries = if intent.shared {
            conn.account_bank_items.to_snapshot_entries()
        } else {
            conn.bank_items.to_snapshot_entries()
        };
        handlers::send_bank_item_snapshot(&mut *server, cid, intent.shared, entries);
        tracing::info!(owner = intent.owner, %path, withdrew = count - leftover, shared = intent.shared, "bank withdraw item");
    }
}
