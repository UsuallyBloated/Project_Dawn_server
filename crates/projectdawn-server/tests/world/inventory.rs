//! A player's inventory: the snapshot, moves, splits, drops, equipping, vendors and
//! holding an item at the bank or merchant.

use crate::common::*;

/// Track 13.2 â€” on EnterWorld the server seeds the client with a
/// full inventory snapshot. New character has zero items, so the
/// snapshot's entries list is empty. Just asserts the message
/// arrives â€” proves the seed loop is wired.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inventory_snapshot_arrives_on_enter_world() {
    let h = start_both().await;
    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "inv", "Inv", "Human", "Warrior").await;
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;

    let snap = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::InventorySnapshot { .. })
        })
        .await
        .expect("InventorySnapshot fans privately on EnterWorld");
    if let ServerWorldMsg::InventorySnapshot { entries } = snap {
        assert!(entries.is_empty(), "fresh character has no items");
    }
}

/// Track 13.2.b â€” SplitStack carves part of a stack into another
/// slot. Seeds 10 of a known item into slot 0 via direct DB write
/// before EnterWorld, asserts the Snapshot reflects it, sends
/// SplitStack(slot 0 â†’ slot 5, count 3), asserts two
/// InventoryDeltas arrive (slot 0 with count 7, slot 5 with count
/// 3).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn split_stack_carves_off_count() {
    let h = start_both().await;
    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "spl", "Splita", "Human", "Warrior").await;

    // Seed inventory via the public DB helper before the client
    // connects to the world server. This bypasses needing a real
    // loot-drop flow to populate items in test time.
    let db_url = h.db_url.clone();
    let pool = projectdawn_server::db::open(&db_url)
        .await
        .expect("open pool");
    projectdawn_server::db::save_inventory(
        &pool,
        a_char_id,
        &[projectdawn_server::db::InventoryRow {
            location: "base".into(),
            slot: 0,
            item_path: "res://items/cloth.tres".into(),
            count: 10,
        }],
    )
    .await
    .expect("seed inventory");

    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;

    // Confirm the seed lands on the wire.
    let snap = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::InventorySnapshot { .. })
        })
        .await
        .expect("snapshot");
    if let ServerWorldMsg::InventorySnapshot { entries } = snap {
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].2, "res://items/cloth.tres");
        assert_eq!(entries[0].3, 10);
    }

    a.send_split_stack("base", 0, "base", 5, 3);
    for _ in 0..4 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }

    // Two Deltas land: slot 0 with residual count 7, slot 5 with
    // the new stack of 3.
    let mut saw_src = false;
    let mut saw_dst = false;
    let deadline = Instant::now() + Duration::from_secs(2);
    while (!saw_src || !saw_dst) && Instant::now() < deadline {
        if let Some(msg) = a
            .wait_for(CHANNEL_SYSTEM, Duration::from_millis(200), |m| {
                matches!(m, ServerWorldMsg::InventoryDelta { .. })
            })
            .await
        {
            if let ServerWorldMsg::InventoryDelta {
                slot,
                item_path,
                count,
                ..
            } = msg
            {
                let path = item_path.unwrap_or_default();
                if slot == 0 && path == "res://items/cloth.tres" && count == 7 {
                    saw_src = true;
                }
                if slot == 5 && path == "res://items/cloth.tres" && count == 3 {
                    saw_dst = true;
                }
            }
        }
    }
    assert!(
        saw_src && saw_dst,
        "expected both src and dst Deltas (src={saw_src}, dst={saw_dst})"
    );
}

/// Track 13.2.b â€” DropItem removes from inventory and spawns a
/// server-owned LootBag at the player's pos. Asserts:
///   - InventoryDelta clears the source slot.
///   - LootBagSpawn fans out with the dropped item.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn drop_item_creates_loot_bag_at_player_pos() {
    let h = start_both().await;
    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "drp", "Dropper", "Human", "Warrior").await;
    let db_url = h.db_url.clone();
    let pool = projectdawn_server::db::open(&db_url)
        .await
        .expect("open pool");
    projectdawn_server::db::save_inventory(
        &pool,
        a_char_id,
        &[projectdawn_server::db::InventoryRow {
            location: "base".into(),
            slot: 0,
            item_path: "res://items/cloth.tres".into(),
            count: 5,
        }],
    )
    .await
    .expect("seed inventory");
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    let _ = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::InventorySnapshot { .. })
        })
        .await
        .expect("snapshot");

    // Drop the whole stack (count=0).
    a.send_drop_item("base", 0, 0);
    for _ in 0..4 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }

    let delta = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(2), |m| {
            matches!(m, ServerWorldMsg::InventoryDelta { slot, item_path, .. }
                if *slot == 0 && item_path.is_none())
        })
        .await
        .expect("inventory slot cleared");
    let _ = delta;

    let bag_spawn = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(2), |m| {
            matches!(m, ServerWorldMsg::LootBagSpawn { items, .. }
                if items.iter().any(|(p, c)| p == "res://items/cloth.tres" && *c == 5))
        })
        .await
        .expect("LootBag spawns with the dropped stack");
    let _ = bag_spawn;
}

/// Track 13.3 / 14.1 â€” EquipItem moves a base entry into the
/// paperdoll. Seed a registered weapon in base slot 0, send
/// EquipItem(0 â†’ equip slot 0), assert two Deltas land: base slot
/// 0 cleared, equip slot 0 holds the weapon. The path has to be
/// in items.toml or Track 14.1's `is_equippable_in_slot` check
/// would reject the equip.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn equip_item_moves_base_to_paperdoll() {
    const WEAPON: &str = "res://data/loot/items/iron_short_sword.tres";
    let h = start_both().await;
    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "eq1", "Equipper", "Human", "Warrior").await;
    let db_url = h.db_url.clone();
    let pool = projectdawn_server::db::open(&db_url)
        .await
        .expect("open pool");
    projectdawn_server::db::save_inventory(
        &pool,
        a_char_id,
        &[projectdawn_server::db::InventoryRow {
            location: "base".into(),
            slot: 0,
            item_path: WEAPON.into(),
            count: 1,
        }],
    )
    .await
    .expect("seed");
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    let _ = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::InventorySnapshot { .. })
        })
        .await
        .expect("snapshot");

    a.send_equip_item("base", 0, 0); // weapon slot
    for _ in 0..4 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }

    let mut saw_base_clear = false;
    let mut saw_equip_set = false;
    let deadline = Instant::now() + Duration::from_secs(2);
    while (!saw_base_clear || !saw_equip_set) && Instant::now() < deadline {
        if let Some(msg) = a
            .wait_for(CHANNEL_SYSTEM, Duration::from_millis(200), |m| {
                matches!(m, ServerWorldMsg::InventoryDelta { .. })
            })
            .await
        {
            if let ServerWorldMsg::InventoryDelta {
                location,
                slot,
                item_path,
                ..
            } = msg
            {
                if location == "base" && slot == 0 && item_path.is_none() {
                    saw_base_clear = true;
                }
                if location == "equip" && slot == 0 && item_path.as_deref() == Some(WEAPON) {
                    saw_equip_set = true;
                }
            }
        }
    }
    assert!(
        saw_base_clear && saw_equip_set,
        "expected both deltas (base_clear={saw_base_clear}, equip_set={saw_equip_set})"
    );
}

/// Track 14 follow-up â€” BuyItem charges coins and grants the item
/// via InventoryDelta + CoinsUpdate. Seed the player with 100
/// coins, buy 3 Minor Healing Potions (price 12 ea = 36 total),
/// assert the player ends with 64 coins + a 3-stack in base[0].
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn buy_item_charges_coins_and_grants_stack() {
    const POTION_NAME: &str = "Minor Healing Potion";
    const POTION_PATH: &str = "res://data/loot/items/minor_healing_potion.tres";
    let h = start_both().await;
    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "buy1", "Buyer", "Human", "Warrior").await;
    let pool = projectdawn_server::db::open(&h.db_url)
        .await
        .expect("open pool");
    sqlx::query("UPDATE characters SET copper = 100 WHERE id = ?1")
        .bind(a_char_id)
        .execute(&pool)
        .await
        .expect("seed coins");
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    let _ = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::InventorySnapshot { .. })
        })
        .await
        .expect("initial snapshot");

    // vendor_id is informational for now (no server NPCs); 0 is fine.
    a.send_buy_item(0, POTION_NAME, 3);
    for _ in 0..4 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }

    // Inventory delta: base[0] = potion x3.
    let delta = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(2), |m| {
            matches!(
                m,
                ServerWorldMsg::InventoryDelta { location, slot, item_path, count }
                    if location == "base"
                    && *slot == 0
                    && item_path.as_deref() == Some(POTION_PATH)
                    && *count == 3
            )
        })
        .await
        .expect("InventoryDelta with bought stack");
    let _ = delta;

    // Coins update: 100 - (12 * 3) = 64.
    let coins = a
        .wait_for(
            CHANNEL_SYSTEM,
            Duration::from_secs(2),
            |m| matches!(m, ServerWorldMsg::CoinsUpdate { coins } if coins.total_copper() == 64),
        )
        .await
        .expect("CoinsUpdate at 64");
    let _ = coins;
}

/// Dead-intents audit (2026-08-24): a merchant transaction requires a merchant
/// nearby. A client positioned far from any vendor â€” e.g. a modified client at
/// the bottom of a dungeon â€” must be refused, with no coin or item change. The
/// honest client is gated in UI at 6 m; this is the server backstop against a
/// forged position.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn buy_item_rejected_when_no_vendor_in_range() {
    const POTION_NAME: &str = "Minor Healing Potion";
    let h = start_both().await;
    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "farbuy", "Wanderer", "Human", "Warrior").await;
    let pool = projectdawn_server::db::open(&h.db_url)
        .await
        .expect("open pool");
    // Plenty of coin, but stranded 200 m out â€” well beyond the 15 m service range.
    sqlx::query("UPDATE characters SET copper = 1000, pos_x = 200.0, pos_z = 200.0 WHERE id = ?1")
        .bind(a_char_id)
        .execute(&pool)
        .await
        .expect("seed coins + far position");
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    let _ = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::InventorySnapshot { .. })
        })
        .await
        .expect("snapshot");
    let _ = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(2), |m| {
            matches!(m, ServerWorldMsg::CoinsUpdate { .. })
        })
        .await;

    a.send_buy_item(0, POTION_NAME, 1);
    for _ in 0..6 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }

    // No coin change: the far buy is refused before charging.
    let coins = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_millis(500), |m| {
            matches!(m, ServerWorldMsg::CoinsUpdate { .. })
        })
        .await;
    assert!(
        coins.is_none(),
        "a buy with no vendor in range must not charge coin"
    );
}

/// The same character, moved to the town spawn (origin, in range of Brom the
/// vendor), buys normally. Proves the gate is a proximity check, not a block.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn buy_item_succeeds_at_the_town_vendor() {
    const POTION_NAME: &str = "Minor Healing Potion";
    let h = start_both().await;
    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "nearbuy", "Townie", "Human", "Warrior").await;
    let pool = projectdawn_server::db::open(&h.db_url)
        .await
        .expect("open pool");
    // At spawn (0,0,0), ~8 m from Brom â€” inside service range.
    sqlx::query("UPDATE characters SET copper = 1000, pos_x = 0.0, pos_z = 0.0 WHERE id = ?1")
        .bind(a_char_id)
        .execute(&pool)
        .await
        .expect("seed");
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    let _ = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::InventorySnapshot { .. })
        })
        .await
        .expect("snapshot");

    a.send_buy_item(0, POTION_NAME, 1);
    let delta = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(2), |m| {
            matches!(m, ServerWorldMsg::InventoryDelta { .. })
        })
        .await;
    assert!(delta.is_some(), "a buy at the town vendor must succeed");
}

/// Track 14 follow-up â€” BuyItem rejects when the player can't
/// afford the purchase; no Delta / CoinsUpdate lands.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn buy_item_rejects_insufficient_coins() {
    const POTION_NAME: &str = "Minor Healing Potion";
    let h = start_both().await;
    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "buy2", "Poorguy", "Human", "Warrior").await;
    let pool = projectdawn_server::db::open(&h.db_url)
        .await
        .expect("open pool");
    sqlx::query("UPDATE characters SET coins = 5 WHERE id = ?1")
        .bind(a_char_id)
        .execute(&pool)
        .await
        .expect("seed coins");
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    let _ = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::InventorySnapshot { .. })
        })
        .await
        .expect("snapshot");
    // Track 15.1 â€” drain the initial CoinsUpdate seed fired on
    // EnterWorld so it doesn't contaminate the "rejected buy must
    // not fire CoinsUpdate" assertion below.
    let _ = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(2), |m| {
            matches!(m, ServerWorldMsg::CoinsUpdate { .. })
        })
        .await
        .expect("initial coins seed");

    a.send_buy_item(0, POTION_NAME, 1);
    for _ in 0..6 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }

    // No InventoryDelta and no CoinsUpdate should land for a rejected buy.
    let coins = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_millis(500), |m| {
            matches!(m, ServerWorldMsg::CoinsUpdate { .. })
        })
        .await;
    assert!(coins.is_none(), "rejected buy must not fire CoinsUpdate");
}

/// Track 14 follow-up â€” SellItem credits coins and fans an
/// InventoryDelta that clears the slot. Seed a potion stack via DB,
/// sell the whole stack, assert delta-cleared + coins = stack *
/// (vendor_price / 2).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sell_item_credits_coins_and_removes_stack() {
    const POTION_PATH: &str = "res://data/loot/items/minor_healing_potion.tres";
    let h = start_both().await;
    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "sell1", "Seller", "Human", "Warrior").await;
    let pool = projectdawn_server::db::open(&h.db_url)
        .await
        .expect("open pool");
    // Seed inventory: 4 potions in base[0].
    projectdawn_server::db::save_inventory(
        &pool,
        a_char_id,
        &[projectdawn_server::db::InventoryRow {
            location: "base".into(),
            slot: 0,
            item_path: POTION_PATH.into(),
            count: 4,
        }],
    )
    .await
    .expect("seed inventory");
    sqlx::query("UPDATE characters SET copper = 0 WHERE id = ?1")
        .bind(a_char_id)
        .execute(&pool)
        .await
        .expect("seed coins=0");
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    let _ = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::InventorySnapshot { .. })
        })
        .await
        .expect("snapshot");

    // Sell all 4. Vendor price = 12; sell price = 6; total = 24.
    a.send_sell_item(SlotRef::BaseSlot { idx: 0 }, 4);
    for _ in 0..4 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }

    let delta = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(2), |m| {
            matches!(
                m,
                ServerWorldMsg::InventoryDelta { location, slot, item_path, count }
                    if location == "base" && *slot == 0
                    && item_path.is_none() && *count == 0
            )
        })
        .await
        .expect("InventoryDelta clears the slot");
    let _ = delta;

    let coins = a
        .wait_for(
            CHANNEL_SYSTEM,
            Duration::from_secs(2),
            |m| matches!(m, ServerWorldMsg::CoinsUpdate { coins } if coins.total_copper() == 24),
        )
        .await
        .expect("CoinsUpdate at 24");
    let _ = coins;
}

/// Track 14.2 â€” equipping an item with +max_hp bonus fans a
/// HealthUpdate carrying the new max. Iron Chain Vest in
/// items.toml has `max_hp_bonus = 25.0`; a Human Warrior at
/// level 1 has base max_hp = 200 (see char_data tests), so the
/// post-equip max should land at 225. Asserts the post-equip
/// fan-out hits the equipping player themselves â€” the initial
/// EnterWorld snapshot only fans to peers, so without a peer
/// the player wouldn't see their own max_hp until something
/// changed it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn equip_increases_max_hp() {
    const VEST: &str = "res://data/loot/items/iron_chain_vest.tres";
    // EQ's formula (the parity audit, 2026-10-20): 5 + 220 / 10 + 20 x 220 / 3000.
    const HUMAN_WARRIOR_BASE_MAX_HP: f32 = 28.0;
    const EXPECTED_MAX_HP: f32 = HUMAN_WARRIOR_BASE_MAX_HP + 25.0;
    let h = start_both().await;
    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "eq4", "Vestguy", "Human", "Warrior").await;
    let db_url = h.db_url.clone();
    let pool = projectdawn_server::db::open(&db_url)
        .await
        .expect("open pool");
    projectdawn_server::db::save_inventory(
        &pool,
        a_char_id,
        &[projectdawn_server::db::InventoryRow {
            location: "base".into(),
            slot: 0,
            item_path: VEST.into(),
            count: 1,
        }],
    )
    .await
    .expect("seed");
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    let _ = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::InventorySnapshot { .. })
        })
        .await
        .expect("snapshot");

    a.send_equip_item("base", 0, 3); // chest slot
    for _ in 0..4 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }

    let post = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(2), |m| {
            matches!(
                m,
                ServerWorldMsg::HealthUpdate { id, max_hp, .. }
                    if *id == a_char_id as u64 && (*max_hp - EXPECTED_MAX_HP).abs() < 0.1
            )
        })
        .await
        .expect("HealthUpdate with vest-augmented max_hp");
    if let ServerWorldMsg::HealthUpdate { max_hp, .. } = post {
        assert!(
            (max_hp - EXPECTED_MAX_HP).abs() < 0.1,
            "expected max_hp = {EXPECTED_MAX_HP} (base 28 + 25 from vest), got {max_hp}"
        );
    }
}

/// Track 13.3 â€” EquipItem with no source rejects silently. The wire
/// round-trips, server returns no Delta. Mirrors the negative-path
/// MoveItem test.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn equip_item_empty_source_drops_silently() {
    let h = start_both().await;
    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "eq2", "Empty", "Human", "Warrior").await;
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    let _ = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::InventorySnapshot { .. })
        })
        .await
        .expect("snapshot");

    a.send_equip_item("base", 0, 0);
    for _ in 0..4 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }

    let stray = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_millis(500), |m| {
            matches!(m, ServerWorldMsg::InventoryDelta { .. })
        })
        .await;
    assert!(
        stray.is_none(),
        "EquipItem on empty source must not fan a Delta"
    );
}

/// A rejected `MoveItem` must TELL the client the truth about the slots it
/// named, rather than being dropped silently.
///
/// This test previously asserted the opposite ("must not fan an
/// InventoryDelta"), which enshrined a real bug: authority over inventory only
/// helps if disagreement is reported. A client that asked to move an item the
/// server does not have there learned nothing from being refused, kept
/// rendering the phantom, and kept asking. A playtest on 2026-08-18 caught the
/// same slots refused for half an hour, resolving only on relog, and presenting
/// to the player as items vanishing.
///
/// Both slots here are genuinely empty, so the corrective deltas should say so.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn move_item_empty_source_corrects_client() {
    let h = start_both().await;
    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "mvi", "Mover", "Human", "Warrior").await;
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;

    // Wait for the EnterWorld snapshot so we know we're past the seed loop.
    let _ = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::InventorySnapshot { .. })
        })
        .await
        .expect("snapshot seed");

    // Empty slot 0 -> empty slot 3. The server rejects it, and must answer with
    // the real contents of both named slots.
    a.send_move_item("base", 0, "base", 3);
    for _ in 0..4 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }

    let mut corrected: Vec<(String, u32, bool)> = Vec::new();
    for _ in 0..2 {
        let msg = a
            .wait_for(CHANNEL_SYSTEM, Duration::from_millis(700), |m| {
                matches!(m, ServerWorldMsg::InventoryDelta { .. })
            })
            .await;
        match msg {
            Some(ServerWorldMsg::InventoryDelta {
                location,
                slot,
                item_path,
                count,
            }) => corrected.push((location, slot, item_path.is_none() && count == 0)),
            _ => break,
        }
    }

    assert_eq!(
        corrected.len(),
        2,
        "a rejected MoveItem must correct both named slots; got {corrected:?}"
    );
    assert!(
        corrected.iter().any(|(l, s, _)| l == "base" && *s == 0),
        "source slot must be corrected; got {corrected:?}"
    );
    assert!(
        corrected.iter().any(|(l, s, _)| l == "base" && *s == 3),
        "destination slot must be corrected; got {corrected:?}"
    );
    assert!(
        corrected.iter().all(|(_, _, empty)| *empty),
        "both slots are genuinely empty, so both corrections must say empty; got {corrected:?}"
    );
}

/// Playtest 2026-09-23 â€” the dead-state gate. A dead player could loot
/// their own corpse before respawning and keep all the gear, voiding the
/// corpse-run penalty. Now any economy/combat intent from a dead player is
/// refused with a chat line, Respawn still works, and normal handling
/// resumes after it. The line is said once per death (2026-10-26), so each
/// intent is proven refused by its line in a death of its own, and a second
/// refused intent in the same death draws no second line.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dead_players_cannot_loot_or_touch_inventory() {
    let h = start_both().await;
    let (a_session, a_char_id, _a_stale) =
        provision_client(&h.auth_url, "dedguy", "Deddy", "Human", "Warrior").await;
    let a_token = gm_world_token(&h.auth_url, &h.db_url, "dedguy", &a_session, a_char_id).await;
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;

    a.send_death();
    for _ in 0..4 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }

    // Inventory intent while dead: refused with the line.
    a.send_move_item("base", 0, "base", 1);
    let refusal = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::ChatMessage { text, .. }
                if text.contains("while dead"))
        })
        .await;
    assert!(
        refusal.is_some(),
        "dead MoveItem must be refused with a chat line"
    );

    // A second refused intent in the same death draws no second line.
    a.send_loot_all(999_999);
    let again = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_millis(1500), |m| {
            matches!(m, ServerWorldMsg::ChatMessage { text, .. }
                if text.contains("while dead"))
        })
        .await;
    assert!(again.is_none(), "the line is said once per death");

    // Respawn is still allowed and clears the gate: the bind/spawn Teleport
    // arrives.
    a.send_respawn();
    let tp = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(5), |m| {
            matches!(m, ServerWorldMsg::Teleport { .. })
        })
        .await;
    assert!(tp.is_some(), "Respawn must still work for the dead");

    // A second death: the loot intent, first this time, is refused with the
    // line, BEFORE any bag lookup (the id need not exist).
    a.send_death();
    for _ in 0..4 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }
    a.send_loot_all(999_999);
    let refusal2 = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::ChatMessage { text, .. }
                if text.contains("while dead"))
        })
        .await;
    assert!(
        refusal2.is_some(),
        "dead LootAll must be refused with a chat line"
    );

    // Respawn again; a subsequent inventory intent is no longer dead-refused.
    a.send_respawn();
    let tp = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(5), |m| {
            matches!(m, ServerWorldMsg::Teleport { .. })
        })
        .await;
    assert!(
        tp.is_some(),
        "Respawn must still work after the second death"
    );

    a.send_move_item("base", 0, "base", 1);
    let post = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_millis(1500), |m| {
            matches!(m, ServerWorldMsg::ChatMessage { text, .. }
                if text.contains("while dead"))
        })
        .await;
    assert!(post.is_none(), "after respawn the dead-gate must be lifted");
}

/// Only what a merchant in reach stocks is for sale. A priced item no vendor
/// stocks (a named-mob drop with a sell-back price) used to be buyable from
/// any vendor by name; the town's own stock still sells.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_priced_item_no_merchant_stocks_cannot_be_bought() {
    let h = start_both().await;
    let (session, char_id, token) =
        provision_client(&h.auth_url, "stockgate", "Shopper", "Human", "Warrior").await;
    {
        let pool = projectdawn_server::db::open(&h.db_url)
            .await
            .expect("open pool");
        sqlx::query("UPDATE characters SET platinum = 10 WHERE id = ?1")
            .bind(char_id)
            .execute(&pool)
            .await
            .expect("fund the shopper");
    }
    let mut a = WorldClient::start(token, &session, char_id).await;
    a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::InventorySnapshot { .. })
    })
    .await
    .expect("snapshot");

    a.send_buy_item(0, "Predator's Collar", 1);
    let refusal = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::ChatMessage { text, .. } if text.contains("doesn't stock"))
        })
        .await;
    assert!(refusal.is_some(), "an unstocked named drop was sold");

    a.send_buy_item(0, "Bread Loaf", 1);
    let bought = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::InventoryDelta { item_path: Some(p), .. } if p == BREAD)
        })
        .await;
    assert!(bought.is_some(), "a stocked loaf should still sell");
}

/// Held-item bank and vendor interactions (2026-10-30, PD_W0029): a stack
/// lifted onto the cursor goes into the vault from the hand (`BankStoreItem`
/// with the cursor location, the delta emptying the hand), comes back, and
/// sells from the hand (`SlotRef::Cursor`, the coin credited, the rest still
/// held); a held bag with something in it is refused by the merchant and the
/// bank with their own lines; the ledger carries the three moves and
/// balances. The sell half cannot be expressed on the previous protocol (no
/// cursor variant), which is why it rides the bump.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_bank_and_the_merchant_take_what_you_hold() {
    const POUCH: &str = "res://data/loot/items/small_pouch.tres";
    const POTION: &str = "res://data/loot/items/minor_healing_potion.tres";
    let h = start_both().await;
    let (session, char_id, token) =
        provision_client(&h.auth_url, "heldsell", "Heldsell", "Human", "Warrior").await;
    give_char_item(&h.db_url, char_id, "base", 0, BREAD, 3).await;
    give_char_item(&h.db_url, char_id, "base", 1, POUCH, 1).await;
    give_char_item(&h.db_url, char_id, "bag_1", 0, POTION, 1).await;
    {
        let pool = projectdawn_server::db::open(&h.db_url)
            .await
            .expect("open pool");
        sqlx::query("UPDATE characters SET copper = 0, pos_x = 0.0, pos_z = 0.0 WHERE id = ?1")
            .bind(char_id)
            .execute(&pool)
            .await
            .expect("seed");
    }
    seed_ledger_openings(&h.db_url).await;
    let mut a = WorldClient::start(token, &session, char_id).await;
    a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::InventorySnapshot { .. })
    })
    .await
    .expect("snapshot");

    // Lift the loaves and put them in the vault from the hand.
    a.send_move_item("base", 0, "cursor", 0);
    a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::InventoryDelta { location, item_path: Some(p), count: 3, .. }
            if location == "cursor" && p == BREAD)
    })
    .await
    .expect("the loaves ride the cursor");
    a.send_bank_store_item("cursor", 0);
    a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::InventoryDelta { location, item_path: None, .. } if location == "cursor")
    })
    .await
    .expect("the hand empties");
    a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::BankItemSnapshot { shared: false, entries }
            if entries.iter().any(|(_, p, c)| p == BREAD && *c == 3))
    })
    .await
    .expect("the vault holds the loaves");

    // Back to the bags, lifted again, and two sold from the hand.
    a.send_bank_withdraw_item(0);
    let back = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::InventoryDelta { location, item_path: Some(p), count: 3, .. }
                if location == "base" && p == BREAD)
        })
        .await
        .expect("the loaves come back to the bags");
    let ServerWorldMsg::InventoryDelta {
        slot: bread_slot, ..
    } = back
    else {
        unreachable!()
    };
    a.send_move_item("base", bread_slot, "cursor", 0);
    a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::InventoryDelta { location, item_path: Some(p), count: 3, .. }
            if location == "cursor" && p == BREAD)
    })
    .await
    .expect("lifted again");
    a.send_sell_item(SlotRef::Cursor, 2);
    a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::InventoryDelta { location, item_path: Some(p), count: 1, .. }
            if location == "cursor" && p == BREAD)
    })
    .await
    .expect("one loaf stays in the hand");
    a.wait_for(
        CHANNEL_SYSTEM,
        Duration::from_secs(3),
        |m| matches!(m, ServerWorldMsg::CoinsUpdate { coins } if coins.copper == 4),
    )
    .await
    .expect("two loaves at half of 4 each");
    a.send_move_item("cursor", 0, "base", bread_slot);
    a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::InventoryDelta { location, item_path: Some(p), count: 1, .. }
            if location == "base" && p == BREAD)
    })
    .await
    .expect("the last loaf put down");

    // A held bag with something in it: neither the merchant nor the bank.
    a.send_move_item("base", 1, "cursor", 0);
    a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::InventoryDelta { location, item_path: Some(p), .. }
            if location == "cursor" && p == POUCH)
    })
    .await
    .expect("the pouch rides the cursor with its potion");
    a.send_sell_item(SlotRef::Cursor, 1);
    a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::ChatMessage { text, .. } if text.contains("Empty the bag before selling it."))
    })
    .await
    .expect("the merchant refuses the full pouch");
    a.send_bank_store_item("cursor", 0);
    a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::BankRejected { reason } if reason == "Bags can't go in the bank vault.")
    })
    .await
    .expect("the bank refuses the pouch");
    a.send_disconnect();
    a.pump_for(Duration::from_millis(300)).await;

    let rows = ledger_rows_at_least(&h.db_url, char_id, 3).await;
    assert_eq!(
        rows,
        vec![
            ("bank_in".to_string(), Some(BREAD.to_string()), 3, 0, 0),
            ("bank_out".to_string(), Some(BREAD.to_string()), 3, 0, 0),
            ("sell".to_string(), Some(BREAD.to_string()), 2, -2, 4),
        ],
        "the vault moves and the sale from the hand, nothing for the refusals"
    );
    assert!(
        ledger_mismatches(&h.db_url).await.is_empty(),
        "the books balance with the pouch still in hand"
    );
}

/// The starting kit (user, 2026-11-03): a new character is born wearing its
/// class's Trainee's weapon, the Ragged Shirt and the Ragged Pants, written
/// with the character (the ledger balanced from the first login), and none of
/// it can leave: it cannot be sold (and is worth nothing anyway), dropped,
/// offered in a trade or put in the account's shared vault, each
/// refusal moving nothing. The character's own vault takes it, and a destroy
/// is allowed (the kit is made to be thrown away for something better).
/// Fails on the previous server at the snapshot: characters were born bare.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_new_character_is_born_dressed_and_the_kit_cannot_leave() {
    const STAFF: &str = "res://data/loot/items/trainees_staff.tres";
    const SHIRT: &str = "res://data/loot/items/ragged_shirt.tres";
    const PANTS: &str = "res://data/loot/items/ragged_pants.tres";
    let h = start_both().await;
    let (a_session, a_char, a_token) =
        provision_client_dressed(&h.auth_url, "kita", "Kitwiz", "Human", "Wizard").await;
    let (b_session, b_char, b_token) =
        provision_client(&h.auth_url, "kitb", "Kitbee", "Elf", "Cleric").await;
    {
        // At the spawn, in reach of the merchant and the banker.
        let pool = db::open(&h.db_url).await.expect("open pool");
        sqlx::query("UPDATE characters SET pos_x = 0.0, pos_z = 0.0 WHERE id IN (?1, ?2)")
            .bind(a_char)
            .bind(b_char)
            .execute(&pool)
            .await
            .expect("seed positions");
    }
    assert!(
        ledger_mismatches(&h.db_url).await.is_empty(),
        "the kit's ledger rows balance at birth"
    );

    let mut a = WorldClient::start(a_token, &a_session, a_char).await;
    let mut b = WorldClient::start(b_token, &b_session, b_char).await;
    let snap = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::InventorySnapshot { .. })
        })
        .await
        .expect("snapshot");
    let ServerWorldMsg::InventorySnapshot { entries } = snap else {
        unreachable!()
    };
    for (slot, path) in [(0u32, STAFF), (3, SHIRT), (4, PANTS)] {
        assert!(
            entries
                .iter()
                .any(|(loc, s, p, c)| loc == "equip" && *s == slot && p == path && *c == 1),
            "born wearing {path} in slot {slot}: {entries:?}"
        );
    }
    b.pump_for(Duration::from_millis(200)).await;

    // The staff into the bags, where the refusals can be tried on it.
    a.send_unequip_item(0, "base", 0);
    a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::InventoryDelta { location, slot: 0, item_path: Some(p), .. }
            if location == "base" && p == STAFF)
    })
    .await
    .expect("the staff comes off");

    a.send_sell_item(SlotRef::BaseSlot { idx: 0 }, 1);
    a.wait_for(
        CHANNEL_SYSTEM,
        Duration::from_secs(3),
        |m| matches!(m, ServerWorldMsg::ChatMessage { text, .. } if text.contains("NO DROP")),
    )
    .await
    .expect("the merchant is refused a NO DROP thing, whatever it is worth");

    a.send_drop_item("base", 0, 1);
    a.wait_for(
        CHANNEL_SYSTEM,
        Duration::from_secs(3),
        |m| matches!(m, ServerWorldMsg::ChatMessage { text, .. } if text.contains("NO DROP")),
    )
    .await
    .expect("the drop is refused");
    let bag = b
        .wait_for(CHANNEL_SYSTEM, Duration::from_millis(600), |m| {
            matches!(m, ServerWorldMsg::LootBagSpawn { .. })
        })
        .await;
    assert!(bag.is_none(), "nothing reaches the ground: {bag:?}");

    send_msg(
        &mut a.client,
        CHANNEL_SYSTEM,
        &ClientWorldMsg::BankStoreItem {
            src_location: "base".into(),
            src_slot: 0,
            shared: true,
        },
    );
    a.wait_for(
        CHANNEL_SYSTEM,
        Duration::from_secs(3),
        |m| matches!(m, ServerWorldMsg::BankRejected { reason } if reason.contains("NO DROP")),
    )
    .await
    .expect("the shared vault refuses it");

    a.send_trade_request(b_char as u64);
    a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::TradeOpened { .. })
    })
    .await
    .expect("the trade opens");
    a.send_trade_offer_item(0, "base", 0);
    a.wait_for(
        CHANNEL_SYSTEM,
        Duration::from_secs(3),
        |m| matches!(m, ServerWorldMsg::ChatMessage { text, .. } if text.contains("NO DROP")),
    )
    .await
    .expect("the offer is refused");
    let offer = b
        .wait_for(CHANNEL_SYSTEM, Duration::from_millis(600), |m| {
            matches!(m, ServerWorldMsg::TradeOfferUpdate { slots, .. }
                if slots.iter().any(|(p, _)| p == STAFF))
        })
        .await;
    assert!(
        offer.is_none(),
        "the partner never sees the staff offered: {offer:?}"
    );

    // The character's own vault is fine.
    a.send_bank_store_item("base", 0);
    a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::BankItemSnapshot { shared: false, entries }
            if entries.iter().any(|(_, p, _)| p == STAFF))
    })
    .await
    .expect("its own vault takes the staff");

    // And a destroy is allowed: the shirt off and thrown away.
    a.send_unequip_item(3, "base", 1);
    a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::InventoryDelta { location, slot: 1, item_path: Some(p), .. }
            if location == "base" && p == SHIRT)
    })
    .await
    .expect("the shirt comes off");
    send_msg(
        &mut a.client,
        CHANNEL_SYSTEM,
        &ClientWorldMsg::DestroyItem {
            location: "base".into(),
            slot: 1,
            count: 1,
        },
    );
    a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::InventoryDelta { location, slot: 1, item_path: None, .. }
            if location == "base")
    })
    .await
    .expect("the shirt is destroyed");

    a.send_disconnect();
    b.send_disconnect();
    a.pump_for(Duration::from_millis(600)).await;
    assert!(
        ledger_mismatches(&h.db_url).await.is_empty(),
        "the books balance after"
    );
}
