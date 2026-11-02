//! The trade window: the atomic commit, the locks, and what a trade refuses.

use crate::common::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn trade_commits_atomically_and_pays_both_sides() {
    let h = start_both().await;
    let (mut a, _a_char_id, mut b, b_char_id, bread_loc, bread_slot) =
        trade_pair(&h, "tradea", "Tradealy", "tradeb", "Tradebel").await;

    // Flush A's outbound intents before waiting on the peer: wait_for only
    // pumps the client it is called on, so A's packets otherwise sit in A's
    // transport buffer.
    macro_rules! pump_a {
        () => {{
            for _ in 0..4 {
                tick_one(&mut a.client, &mut a.transport);
                tokio::time::sleep(TICK_DT).await;
            }
        }};
    }

    a.send_trade_request(b_char_id as u64);
    pump_a!();
    a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::TradeOpened { partner_id, .. }
            if *partner_id == b_char_id as u64)
    })
    .await
    .expect("A's window opens");
    b.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::TradeOpened { .. })
    })
    .await
    .expect("B's window opens instantly (no accept prompt)");

    // A offers the bread and the copper; B offers nothing (a gift).
    a.send_trade_offer_item(0, &bread_loc, bread_slot);
    pump_a!();
    b.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::TradeOfferUpdate { mine: false, slots, .. }
            if slots.first().map(|(p, c)| p.contains("bread") && *c == 3).unwrap_or(false))
    })
    .await
    .expect("B sees A's offered stack, path and count");
    a.send_trade_offer_coins(protocol::world::Coins {
        platinum: 0,
        gold: 0,
        silver: 0,
        copper: 25,
    });
    pump_a!();

    a.send_trade_accept();
    pump_a!();
    b.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::TradeAcceptState { you: false, them: true })
    })
    .await
    .expect("B sees A standing accepted");
    b.send_trade_accept();

    // B receives the goods, then the committed close. (wait_for drops
    // non-matching messages, so assert in fan order: delta, coins, close.)
    b.wait_for(CHANNEL_SYSTEM, Duration::from_secs(5), |m| {
        matches!(m, ServerWorldMsg::InventoryDelta { item_path: Some(p), count: 3, .. }
            if p.contains("bread"))
    })
    .await
    .expect("B receives the bread stack");
    b.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::CoinsUpdate { coins } if coins.copper >= 25)
    })
    .await
    .expect("B receives the copper");
    b.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::TradeClosed { committed: true, .. })
    })
    .await
    .expect("B's close says committed");

    // A's side: the offered slot cleared and the close says committed.
    a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(5), |m| {
        matches!(m, ServerWorldMsg::InventoryDelta { location, slot, item_path: None, .. }
            if *location == bread_loc && *slot == bread_slot)
    })
    .await
    .expect("A's offered slot empties");
    a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::TradeClosed { committed: true, .. })
    })
    .await
    .expect("A's close says committed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn trade_edits_clear_accepts_and_locks_hold() {
    let h = start_both().await;
    let (mut a, _a_char_id, mut b, b_char_id, bread_loc, bread_slot) =
        trade_pair(&h, "tradec", "Tradecyn", "traded", "Tradedor").await;

    a.send_trade_request(b_char_id as u64);
    a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::TradeOpened { .. })
    })
    .await
    .expect("A's window opens");
    a.send_trade_offer_item(0, &bread_loc, bread_slot);
    a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::TradeOfferUpdate { mine: true, .. })
    })
    .await
    .expect("A sees its own offer");

    // Ledger 2 — the escrow: the offered slot refuses a MoveItem with the
    // lock's chat line, and the stack stays put.
    a.send_move_item(&bread_loc, bread_slot, "base", 7);
    a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::ChatMessage { text, .. }
            if text.contains("offered in a trade"))
    })
    .await
    .expect("locked slot refuses the move, with the line");

    // Ledger 1 — bait-and-switch: A accepts, then B edits; BOTH accepts
    // clear, and B's later lone accept commits nothing.
    a.send_trade_accept();
    for _ in 0..4 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }
    b.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::TradeAcceptState { them: true, .. })
    })
    .await
    .expect("B sees A accepted");
    b.send_trade_offer_coins(protocol::world::Coins::ZERO);
    for _ in 0..4 {
        tick_one(&mut b.client, &mut b.transport);
        tokio::time::sleep(TICK_DT).await;
    }
    a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::TradeAcceptState { you: false, them: false })
    })
    .await
    .expect("the edit cleared BOTH accepts on A's display");
    b.send_trade_accept();
    let premature = b
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(2), |m| {
            matches!(m, ServerWorldMsg::TradeClosed { committed: true, .. })
        })
        .await;
    assert!(
        premature.is_none(),
        "one accept after the edit must not commit — A's stale accept is dead"
    );
}

/// Review 2026-10-12: an offer is a reference to `bag_<k>` slot `s`, and a
/// full bag moves with its contents (re-keyed to its new base slot) since
/// 2026-09-27. Swapping the PARENT bag was not locked, so after B accepted,
/// A could swap two pouches and the reference would name the other pouch's
/// item: the count still matched (1 = 1) and B's window never refreshed.
/// The scam offers the good sword and swaps in the junk; this test runs the
/// mirror (junk shown, so receiving the good sword proves the re-point). The
/// parent bag's base slot is locked now, and the commit checks the item too.
/// Fails on the previous code at the refusal, and without that assertion,
/// B receives the good sword.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn trade_locks_the_bag_holding_an_offered_item() {
    let h = start_both().await;
    let (mut a, _a_char_id, mut b, b_char_id, _bread_loc, _bread_slot) =
        trade_pair(&h, "tradeg", "Tradegan", "tradeh", "Tradehal").await;

    macro_rules! pump_a {
        () => {{
            for _ in 0..4 {
                tick_one(&mut a.client, &mut a.transport);
                tokio::time::sleep(TICK_DT).await;
            }
        }};
    }
    // Wait for one item to land and return its (location, slot).
    async fn landed(c: &mut WorldClient, needle: &str) -> (String, u32) {
        match c
            .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
                matches!(m, ServerWorldMsg::InventoryDelta { item_path: Some(p), .. }
                    if p.contains(needle))
            })
            .await
            .expect("the item lands")
        {
            ServerWorldMsg::InventoryDelta { location, slot, .. } => (location, slot),
            _ => unreachable!(),
        }
    }

    // Two pouches in base slots, a junk sword in one, a good sword in the other.
    a.send_gm_command("give Small Pouch");
    let (_, p1) = landed(&mut a, "small_pouch").await;
    a.send_gm_command("give Small Pouch");
    let (_, p2) = landed(&mut a, "small_pouch").await;
    a.send_gm_command("give Rusty Short Sword");
    let (jl, js) = landed(&mut a, "rusty_short_sword").await;
    a.send_gm_command("give Iron Short Sword");
    let (gl, gs) = landed(&mut a, "iron_short_sword").await;
    let junk_bag = format!("bag_{p1}");
    let good_bag = format!("bag_{p2}");
    a.send_move_item(&jl, js, &junk_bag, 0);
    a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::InventoryDelta { location, slot: 0, item_path: Some(p), .. }
            if *location == junk_bag && p.contains("rusty_short_sword"))
    })
    .await
    .expect("the junk sword goes into the first pouch");
    a.send_move_item(&gl, gs, &good_bag, 0);
    a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::InventoryDelta { location, slot: 0, item_path: Some(p), .. }
            if *location == good_bag && p.contains("iron_short_sword"))
    })
    .await
    .expect("the good sword goes into the second pouch");

    // A offers the junk sword; B sees it and accepts.
    a.send_trade_request(b_char_id as u64);
    pump_a!();
    b.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::TradeOpened { .. })
    })
    .await
    .expect("B's window opens");
    a.send_trade_offer_item(0, &junk_bag, 0);
    pump_a!();
    b.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::TradeOfferUpdate { mine: false, slots, .. }
            if slots.first().map(|(p, c)| p.contains("rusty_short_sword") && *c == 1).unwrap_or(false))
    })
    .await
    .expect("B sees the junk sword offered");
    b.send_trade_accept();
    for _ in 0..4 {
        tick_one(&mut b.client, &mut b.transport);
        tokio::time::sleep(TICK_DT).await;
    }

    // The switch: swap the two pouches. The pouch holding the offer is locked.
    a.send_move_item("base", p1, "base", p2);
    a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::ChatMessage { text, .. }
            if text.contains("offered in a trade"))
    })
    .await
    .expect("moving the bag that holds an offered item is refused");

    // A accepts; B gets exactly what B saw, never the good sword.
    a.send_trade_accept();
    pump_a!();
    let mut got_junk = false;
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        let Some(m) = b
            .wait_for(CHANNEL_SYSTEM, Duration::from_millis(500), |m| {
                matches!(m, ServerWorldMsg::InventoryDelta { item_path: Some(_), .. }
                    | ServerWorldMsg::TradeClosed { .. })
            })
            .await
        else {
            continue;
        };
        match m {
            ServerWorldMsg::InventoryDelta { item_path: Some(p), .. } => {
                assert!(!p.contains("iron_short_sword"), "B must never receive the good sword");
                if p.contains("rusty_short_sword") {
                    got_junk = true;
                }
            }
            ServerWorldMsg::TradeClosed { committed, .. } => {
                assert!(committed, "the trade commits");
                break;
            }
            _ => {}
        }
    }
    assert!(got_junk, "B receives the junk sword it was shown");
}

/// Review 2026-10-12: loot, purchases and grants place through the server's
/// placer, which can top up an offered stack. The commit then refuses, but the
/// window kept showing the old snapshot and the stack stayed locked, so every
/// later accept failed the same way with nothing visibly wrong. A changed
/// offer now leaves the window (which unlocks it); offering it again commits.
/// Fails on the previous code at "the changed offer leaves A's window".
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn trade_drops_an_offer_that_changed_so_the_trade_can_go_on() {
    let h = start_both().await;
    let (mut a, _a_char_id, mut b, b_char_id, bread_loc, bread_slot) =
        trade_pair(&h, "tradei", "Tradeira", "tradej", "Tradejon").await;
    macro_rules! pump {
        ($c:expr) => {{
            for _ in 0..4 {
                tick_one(&mut $c.client, &mut $c.transport);
                tokio::time::sleep(TICK_DT).await;
            }
        }};
    }

    a.send_trade_request(b_char_id as u64);
    pump!(a);
    b.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::TradeOpened { .. })
    })
    .await
    .expect("B's window opens");
    a.send_trade_offer_item(0, &bread_loc, bread_slot);
    a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::TradeOfferUpdate { mine: true, slots, .. }
            if slots.first().map(|(p, c)| p.contains("bread") && *c == 3).unwrap_or(false))
    })
    .await
    .expect("A offers 3 bread");

    // A grant tops the offered stack up to 4 through the placer.
    a.send_gm_command("give Bread Loaf 1");
    a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::InventoryDelta { location, slot, count: 4, .. }
            if *location == bread_loc && *slot == bread_slot)
    })
    .await
    .expect("the grant merges into the offered stack");

    b.send_trade_accept();
    pump!(b);
    a.send_trade_accept();
    a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::ChatMessage { text, .. } if text.contains("changed"))
    })
    .await
    .expect("the commit refuses the changed offer, with a line");
    a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::TradeOfferUpdate { mine: true, slots, .. }
            if slots.first().map(|(p, _)| p.is_empty()).unwrap_or(false))
    })
    .await
    .expect("the changed offer leaves A's window");

    // Offered again, the trade goes through with what B now sees.
    a.send_trade_offer_item(0, &bread_loc, bread_slot);
    pump!(a);
    b.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::TradeOfferUpdate { mine: false, slots, .. }
            if slots.first().map(|(p, c)| p.contains("bread") && *c == 4).unwrap_or(false))
    })
    .await
    .expect("B sees the re-offer at 4");
    b.send_trade_accept();
    pump!(b);
    a.send_trade_accept();
    pump!(a);
    b.wait_for(CHANNEL_SYSTEM, Duration::from_secs(5), |m| {
        matches!(m, ServerWorldMsg::InventoryDelta { item_path: Some(p), count: 4, .. }
            if p.contains("bread"))
    })
    .await
    .expect("B receives all 4");
    b.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::TradeClosed { committed: true, .. })
    })
    .await
    .expect("the trade commits");
}

/// Food and water's auto-eat takes the first food in slot order when a
/// character runs out. Merged onto the trade branch, it did not consult the
/// trade lock, so a loaf offered in an open trade could be eaten mid-trade (the
/// commit then refused the changed offer, so no dupe, but the offer was gone).
/// Auto-eat now skips offered slots. Fails on the previous code: the delta
/// names the offered base slot 3.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auto_eat_skips_food_offered_in_a_trade() {
    let h = start_both().await;
    let (a_session, a_char_id, _) =
        provision_client(&h.auth_url, "eata", "Eatalot", "Human", "Warrior").await;
    let (b_session, b_char_id, b_token) =
        provision_client(&h.auth_url, "eatb", "Eatbee", "Human", "Warrior").await;
    // Six seconds of food left, plenty of water; bread in base 3 and base 5.
    set_char_appetite(&h.db_url, a_char_id, 6.0, 5000.0, None).await;
    give_char_item(&h.db_url, a_char_id, "base", 3, BREAD, 1).await;
    give_char_item(&h.db_url, a_char_id, "base", 5, BREAD, 1).await;
    let a_token = request_world_token(&h.auth_url, &a_session, a_char_id).await;
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    let mut b = WorldClient::start(b_token, &b_session, b_char_id).await;
    a.pump_for(Duration::from_millis(300)).await;
    b.pump_for(Duration::from_millis(300)).await;

    // A opens a trade and offers the loaf in base 3 before the food runs out.
    a.send_trade_request(b_char_id as u64);
    a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::TradeOpened { .. })
    })
    .await
    .expect("the trade opens");
    a.send_trade_offer_item(0, "base", 3);
    a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::TradeOfferUpdate { mine: true, slots, .. }
            if slots.first().map(|(p, _)| p.contains("bread")).unwrap_or(false))
    })
    .await
    .expect("the loaf in base 3 is offered");

    // The food runs out: the auto-eat takes base 5, never the offered base 3.
    let mut eaten: Option<u32> = None;
    let end = Instant::now() + Duration::from_secs(12);
    while Instant::now() < end && eaten.is_none() {
        a.send_heartbeat();
        b.send_heartbeat();
        b.pump_for(Duration::from_millis(50)).await;
        if let Some(ServerWorldMsg::InventoryDelta { slot, .. }) = a
            .wait_for(CHANNEL_SYSTEM, Duration::from_secs(2), |m| {
                matches!(m, ServerWorldMsg::InventoryDelta { location, .. } if location == "base")
            })
            .await
        {
            eaten = Some(slot);
        }
    }
    assert_eq!(eaten, Some(5), "the auto-eat takes the loaf that is not offered");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn trade_refuses_self_and_coin_overdraft() {
    let h = start_both().await;
    let (mut a, a_char_id, mut b, b_char_id, _bread_loc, _bread_slot) =
        trade_pair(&h, "tradee", "Tradeeva", "tradef", "Tradefin").await;

    // Ledger 8 — self-trade.
    a.send_trade_request(a_char_id as u64);
    a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::ChatMessage { text, .. }
            if text.contains("yourself"))
    })
    .await
    .expect("self-trade refuses");

    // Ledger 4 — overdraft: B offers coin it cannot cover.
    a.send_trade_request(b_char_id as u64);
    for _ in 0..4 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }
    b.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::TradeOpened { .. })
    })
    .await
    .expect("window opens");
    b.send_trade_offer_coins(protocol::world::Coins {
        platinum: 999,
        gold: 0,
        silver: 0,
        copper: 0,
    });
    b.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::ChatMessage { text, .. }
            if text.contains("that much coin"))
    })
    .await
    .expect("the overdraft refuses with the line");
}

/// The trade commit writes both inventories and wallets but not the vaults,
/// so each trader's other dirty stores are flushed first. A withdraws a loaf
/// from the bank (unsaved) and trades it to B: once the trade commits, the
/// loaf is in B's saved inventory and nowhere in A's saved vault.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_traded_bank_withdrawal_leaves_the_givers_saved_vault() {
    let h = start_both().await;
    let (a_session, a_char, a_token) =
        provision_client(&h.auth_url, "vaulttradea", "Vaulter", "Human", "Warrior").await;
    let (b_session, b_char, b_token) =
        provision_client(&h.auth_url, "vaulttradeb", "Receiver", "Elf", "Cleric").await;
    {
        let pool = projectdawn_server::db::open(&h.db_url).await.expect("open pool");
        sqlx::query("INSERT INTO bank_items (char_id, slot, item_path, count) VALUES (?1, 0, ?2, 1)")
            .bind(a_char)
            .bind(BREAD)
            .execute(&pool)
            .await
            .expect("seed the vault");
    }
    let mut a = WorldClient::start(a_token, &a_session, a_char).await;
    let mut b = WorldClient::start(b_token, &b_session, b_char).await;

    a.send_bank_withdraw_item(0);
    let (loc, slot) = match a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::InventoryDelta { item_path: Some(p), .. } if p == BREAD)
        })
        .await
        .expect("the loaf arrives in the bags")
    {
        ServerWorldMsg::InventoryDelta { location, slot, .. } => (location, slot),
        _ => unreachable!(),
    };

    a.send_trade_request(b_char as u64);
    a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::TradeOpened { .. })
    })
    .await
    .expect("the trade opens");
    a.send_trade_offer_item(0, &loc, slot);
    a.pump_for(Duration::from_millis(200)).await;
    b.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::TradeOfferUpdate { mine: false, slots, .. } if !slots.is_empty())
    })
    .await
    .expect("B sees the offer");
    a.send_trade_accept();
    a.pump_for(Duration::from_millis(200)).await;
    b.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::TradeAcceptState { them: true, .. })
    })
    .await
    .expect("B sees A accept");
    b.send_trade_accept();
    b.wait_for(CHANNEL_SYSTEM, Duration::from_secs(5), |m| {
        matches!(m, ServerWorldMsg::TradeClosed { committed: true, .. })
    })
    .await
    .expect("the trade commits");

    let b_has = count_item_rows(
        &h.db_url,
        "SELECT COUNT(*) FROM character_items WHERE char_id = ?1 AND item_path = ?2",
        b_char,
        BREAD,
    )
    .await;
    let a_vault = count_item_rows(
        &h.db_url,
        "SELECT COUNT(*) FROM bank_items WHERE char_id = ?1 AND item_path = ?2",
        a_char,
        BREAD,
    )
    .await;
    assert_eq!(b_has, 1, "the loaf is in the receiver's saved inventory");
    assert_eq!(a_vault, 0, "the loaf is ALSO still in the giver's saved vault: a dupe");
}

/// A trade in the ledger: the giver's queued GM rows and the trade rows (one
/// per stack out, one for the coin) and the receiver's (one per stack in, one
/// for the coin) are all written by the trade's own commit, both sides name
/// the partner, and the books balance with nothing but recorded moves behind
/// every holding. Fails on the previous server at the first row assertion.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_trade_writes_both_sides_of_the_ledger_and_balances() {
    let h = start_both().await;
    let (mut a, a_char_id, mut b, b_char_id, bread_loc, bread_slot) =
        trade_pair(&h, "ledgertra", "Ledgertra", "ledgertrb", "Ledgertrb").await;
    macro_rules! pump_a {
        () => {{
            for _ in 0..4 {
                tick_one(&mut a.client, &mut a.transport);
                tokio::time::sleep(TICK_DT).await;
            }
        }};
    }

    a.send_trade_request(b_char_id as u64);
    pump_a!();
    a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::TradeOpened { partner_id, .. }
            if *partner_id == b_char_id as u64)
    })
    .await
    .expect("A's window opens");
    b.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::TradeOpened { .. })
    })
    .await
    .expect("B's window opens");
    a.send_trade_offer_item(0, &bread_loc, bread_slot);
    pump_a!();
    b.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::TradeOfferUpdate { mine: false, .. })
    })
    .await
    .expect("B sees the offer");
    a.send_trade_offer_coins(protocol::world::Coins { platinum: 0, gold: 0, silver: 0, copper: 25 });
    pump_a!();
    a.send_trade_accept();
    pump_a!();
    b.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::TradeAcceptState { you: false, them: true })
    })
    .await
    .expect("B sees A accepted");
    b.send_trade_accept();
    b.wait_for(CHANNEL_SYSTEM, Duration::from_secs(5), |m| {
        matches!(m, ServerWorldMsg::TradeClosed { committed: true, .. })
    })
    .await
    .expect("the trade commits");
    a.pump_for(Duration::from_millis(200)).await;

    let mut a_rows = ledger_rows(&h.db_url, a_char_id).await;
    // The two GM grants land in one tick, the coin grant at intake and the
    // give at the apply phase, so their order between themselves is not a
    // rule; the trade's rows follow both.
    assert_eq!(a_rows.len(), 4, "{a_rows:?}");
    a_rows[..2].sort();
    assert_eq!(
        a_rows,
        vec![
            ("gm_coins".to_string(), None, 0, 0, 25),
            ("gm_give".to_string(), Some(BREAD.to_string()), 3, 3, 0),
            ("trade_out".to_string(), Some(BREAD.to_string()), 3, -3, 0),
            ("trade_coins".to_string(), None, 0, 0, -25),
        ],
        "the giver's rows, the queued GM grants and the trade, all with the commit"
    );
    let b_rows = ledger_rows(&h.db_url, b_char_id).await;
    assert_eq!(
        b_rows,
        vec![
            ("trade_in".to_string(), Some(BREAD.to_string()), 3, 3, 0),
            ("trade_coins".to_string(), None, 0, 0, 25),
        ],
        "the receiver's rows"
    );
    {
        let pool = projectdawn_server::db::open(&h.db_url).await.expect("open pool");
        let details: Vec<(Option<String>,)> = sqlx::query_as(
            "SELECT detail FROM item_events WHERE action LIKE 'trade%' ORDER BY id",
        )
        .fetch_all(&pool)
        .await
        .expect("details");
        assert!(
            details.iter().take(2).all(|(d,)| d.as_deref() == Some(&format!("char:{b_char_id}")))
                && details.iter().skip(2).all(|(d,)| d.as_deref() == Some(&format!("char:{a_char_id}"))),
            "each side's rows name the partner: {details:?}"
        );
    }
    assert!(ledger_mismatches(&h.db_url).await.is_empty(), "the books balance after the trade");
    a.send_disconnect();
    b.send_disconnect();
    a.pump_for(Duration::from_millis(200)).await;
    b.pump_for(Duration::from_millis(200)).await;
}
