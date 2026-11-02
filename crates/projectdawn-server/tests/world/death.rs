//! Death: the dead's refusals, respawn, the corpse and looting it, resurrection.

use crate::common::*;

/// The dead-state gate says its line once per death, not once per refused
/// message (2026-10-26: Trigger Death filled the chat with it eight times),
/// and lets a target change through: clicking things while dead is natural and
/// harmless. A new death says it again. On the previous gate the target change
/// drew a line and three refused messages drew three.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_dead_are_told_once_per_death() {
    let h = start_both().await;
    let (session, char_id, _stale) =
        provision_client(&h.auth_url, "deadonce", "Onceler", "Human", "Warrior").await;
    let token = gm_world_token(&h.auth_url, &h.db_url, "deadonce", &session, char_id).await;
    let mut a = WorldClient::start(token, &session, char_id).await;
    let me = char_id as u64;

    let die = |a: &mut WorldClient| a.send_damage_self(1_000_000);
    die(&mut a);
    a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::HealthUpdate { id, hp, .. } if *id == me && *hp <= 0.01)
    })
    .await
    .expect("the dev damage kills");
    a.collect_for(Duration::from_millis(500)).await;

    send_msg(&mut a.client, CHANNEL_SYSTEM, &ClientWorldMsg::SetTarget { target_id: None });
    let msgs = a.collect_for(Duration::from_millis(800)).await;
    assert_eq!(dead_lines(&msgs), 0, "a target change is allowed while dead");

    for _ in 0..3 {
        a.send_sit();
    }
    let msgs = a.collect_for(Duration::from_millis(1200)).await;
    assert_eq!(dead_lines(&msgs), 1, "three refused messages, one line; got {:?}", system_lines(&msgs));

    a.send_sit();
    let msgs = a.collect_for(Duration::from_millis(800)).await;
    assert_eq!(dead_lines(&msgs), 0, "still the same death: no more lines");

    // Alive again, then a new death: told once more.
    a.send_respawn();
    a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::HealthUpdate { id, hp, .. } if *id == me && *hp > 0.01)
    })
    .await
    .expect("the respawn restores some HP");
    die(&mut a);
    a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::HealthUpdate { id, hp, .. } if *id == me && *hp <= 0.01)
    })
    .await
    .expect("the second death");
    a.collect_for(Duration::from_millis(500)).await;
    a.send_sit();
    a.send_sit();
    let msgs = a.collect_for(Duration::from_millis(1200)).await;
    assert_eq!(dead_lines(&msgs), 1, "a new death is told once again");
}

/// Phase 1 finding 3 â€” the Respawn dead-check. A LIVING player's Respawn must be a
/// no-op (the exploit spammed it to floor HP at 25% for near-invulnerability); a
/// DEAD player's Respawn must still restore ~25% (the legit path). Drives the real
/// flow: connect at full HP, Respawn (rejected), DeathBroadcast (kill), Respawn
/// (restores). HP is observed via HealthUpdate. ~25% is the Respawn floor value.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn respawn_requires_being_dead() {
    let h = start_both().await;
    let (session, char_id, _stale) =
        provision_client(&h.auth_url, "respawner", "Respawna", "Human", "Warrior").await;
    let token = gm_world_token(&h.auth_url, &h.db_url, "respawner", &session, char_id).await;
    let mut c = WorldClient::start(token, &session, char_id).await;
    let cid = char_id as u64;

    // (1) LIVING player: Respawn must NOT floor HP. We start at full HP and take no
    //     damage, so a ~25% HealthUpdate would only come from a wrongly-honored Respawn.
    c.send_respawn();
    let floored = c
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(2), |m| {
            matches!(m, ServerWorldMsg::HealthUpdate { id, hp, max_hp }
                if *id == cid && *hp >= *max_hp * 0.2 && *hp <= *max_hp * 0.3)
        })
        .await;
    assert!(
        floored.is_none(),
        "a living player's Respawn must be a no-op, not a floor to 25% (finding 3)"
    );

    // (2) Legit path intact: die (DeathBroadcast -> kill_player -> hp 0) ...
    c.send_death();
    let dead = c
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::HealthUpdate { id, hp, .. } if *id == cid && *hp <= 0.01)
        })
        .await;
    assert!(dead.is_some(), "DeathBroadcast should kill the player (hp -> 0)");

    // ... then Respawn now restores HP to ~25%.
    c.send_respawn();
    let respawned = c
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::HealthUpdate { id, hp, max_hp }
                if *id == cid && *hp >= *max_hp * 0.2 && *hp <= *max_hp * 0.3)
        })
        .await;
    assert!(
        respawned.is_some(),
        "a dead player's Respawn should still restore HP to ~25%"
    );
}

// Dead-XP gate (decided 2026-09-19): a group member lying dead beside the
// mob collects NOTHING â€” no XP share, no quest tick, and no dilution of the
// pool. B groups with A and dies at A's feet; A then solo-kills a
// dev-spawned mob. A's XpGained must be the FULL solo amount (263 for a
// level-1 mob â€” not the 157 a two-way group split would pay), and B must
// see no positive XpGained at all. The range half of the eligibility rule
// rides the same `eligible` closure in `award_kill`, so this test covers
// the mechanism for both.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dead_group_member_gets_no_xp_share() {
    let h = start_both().await;

    // A needs is_gm for the dev-spawn; re-mint the token after the grant so
    // the flag rides it (same pattern as the AOE test).
    let (a_session, a_char_id, _stale_token) =
        provision_client(&h.auth_url, "xpsolo", "Xpsolo", "Human", "Warrior").await;
    let pool = db::open(&h.db_url).await.expect("open pool");
    db::set_account_gm(&pool, "xpsolo", true).await.expect("set is_gm");
    let a_token = request_world_token(&h.auth_url, &a_session, a_char_id).await;
    let (b_session, b_char_id, _b_stale) =
        provision_client(&h.auth_url, "xpdead", "Xpdead", "Elf", "Cleric").await;

    master_weapon_skill(&h.db_url, a_char_id, "hand_to_hand").await;
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    let b_token = gm_world_token(&h.auth_url, &h.db_url, "xpdead", &b_session, b_char_id).await;
    let mut b = WorldClient::start(b_token, &b_session, b_char_id).await;
    let _ = b_char_id;

    // Group up: A invites by name, B accepts by A's id, A sees the roster.
    a.send_group_invite("Xpdead");
    for _ in 0..3 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }
    b.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::GroupInvited { from_id, .. }
            if *from_id == a_char_id as u64)
    })
    .await
    .expect("B receives the group invite");
    b.send_group_accept(a_char_id as u64);
    // Pump B so the accept actually reaches the wire (wait_for only ticks
    // the client it is called on).
    for _ in 0..4 {
        tick_one(&mut b.client, &mut b.transport);
        tokio::time::sleep(TICK_DT).await;
    }
    a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::GroupRoster { members, .. } if members.len() == 2)
    })
    .await
    .expect("A sees the two-member roster");

    // B dies right here at the spawn â€” a corpse beside the coming kill,
    // the exact scenario the pure-proximity rule would have paid.
    b.send_death();
    for _ in 0..6 {
        tick_one(&mut b.client, &mut b.transport);
        tokio::time::sleep(TICK_DT).await;
    }

    // A conjures a level-1 dummy that walks to A on its own (aggro 12,
    // 1 dmg so its swings are harmless), then kills it with paced swings
    // (the swing-rate limiter drops anything faster than the 1.65 s
    // unhasted bare-hand gate). 10 HP vs ~5-8 per swing = 2-3 landed hits. Same
    // shape as player_attack_kills_enemy_and_corpse_despawns: waiting for
    // ITS hit on us proves it closed into melee range first.
    a.send_dev_spawn("XP Filter Dummy", 1, 10.0, 1, 1.8, 12.0);
    let spawn_evt = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::EnemySpawn { mob_name, .. }
                if mob_name == "XP Filter Dummy")
        })
        .await
        .expect("the dev-spawned dummy fans an EnemySpawn");
    let enemy_id: u64 = match spawn_evt {
        ServerWorldMsg::EnemySpawn { id, .. } => id,
        _ => unreachable!(),
    };
    // Wait for the dummy's swing on us (proves melee adjacency) while
    // pumping BOTH transports â€” a one-sided wait leaves B's socket
    // unserviced under the 20 Hz position fan (the 2026-09-16 starvation
    // class), and a B disconnect would make the no-leak assertion below
    // vacuous (an offline B was already excluded before this change).
    let deadline = Instant::now() + Duration::from_secs(35);
    let mut adjacent = false;
    'adjacency: while Instant::now() < deadline {
        tick_one(&mut a.client, &mut a.transport);
        tick_one(&mut b.client, &mut b.transport);
        while let Some(bytes) = a.client.receive_message(CHANNEL_SYSTEM) {
            if let Ok((msg, _)) = bincode::serde::decode_from_slice::<ServerWorldMsg, _>(
                &bytes,
                bincode_cfg(),
            ) {
                if matches!(msg, ServerWorldMsg::Hit { attacker, .. } if attacker == enemy_id)
                {
                    adjacent = true;
                    break 'adjacency;
                }
            }
        }
        tokio::time::sleep(TICK_DT).await;
    }
    assert!(adjacent, "the dummy walks into melee and swings");

    const SWING_GAP: Duration = Duration::from_millis(1800);
    for _ in 0..6 {
        a.send_attack(enemy_id, "", false, DamageType::Physical);
        let until = Instant::now() + SWING_GAP;
        while Instant::now() < until {
            tick_one(&mut a.client, &mut a.transport);
            tick_one(&mut b.client, &mut b.transport);
            tokio::time::sleep(TICK_DT).await;
        }
    }

    // A collects the FULL solo amount: the dead member neither shares nor
    // dilutes (263 = kill_xp(1); a leaked two-way split would pay 157).
    let xp_evt = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(5), |m| {
            matches!(m, ServerWorldMsg::XpGained { amount, .. } if *amount > 0)
        })
        .await
        .expect("the living killer still collects");
    if let ServerWorldMsg::XpGained { amount, .. } = xp_evt {
        assert_eq!(
            amount, 263,
            "dead member must not dilute the pool: solo 263, not a 157 split"
        );
    }

    // B (dead, still connected under the death lock) gets nothing. The
    // connect-time bar seed is amount 0, so filter on positive amounts.
    let leak = b
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(2), |m| {
            matches!(m, ServerWorldMsg::XpGained { amount, .. } if *amount > 0)
        })
        .await;
    assert!(leak.is_none(), "a dead group member must receive no XP share");
}

/// Corpse auto-re-equip (the cursor epic's slice 2). A Warrior wearing a
/// cloth cap (head, slot 2) and a cloth robe (chest, slot 3) and carrying
/// bread dies, respawns, and loots the corpse: the cap and robe go straight
/// back onto slots 2 and 3 (an InventoryDelta to "equip" for each, the
/// client's ordinary equip path), the bread goes to a base slot, and the
/// worn rows are what the database holds afterwards. On the previous code all
/// three landed in the bags.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn looting_your_own_corpse_puts_your_gear_back_on() {
    const CAP: &str = "res://data/loot/items/cloth_cap.tres";
    const ROBE: &str = "res://data/loot/items/cloth_robe.tres";
    let h = start_both().await;
    let (session, char_id, _) =
        provision_client(&h.auth_url, "rewear", "Rewear", "Human", "Warrior").await;
    give_char_item(&h.db_url, char_id, "equip", 2, CAP, 1).await;
    give_char_item(&h.db_url, char_id, "equip", 3, ROBE, 1).await;
    give_char_item(&h.db_url, char_id, "base", 0, BREAD, 2).await;
    let pool = db::open(&h.db_url).await.expect("open pool");
    db::set_account_gm(&pool, "rewear", true).await.expect("set is_gm");
    let token = request_world_token(&h.auth_url, &session, char_id).await;
    let mut a = WorldClient::start(token, &session, char_id).await;
    a.pump_for(Duration::from_millis(300)).await;

    a.send_damage_self(1_000_000);
    let contents = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(5), |m| {
            matches!(m, ServerWorldMsg::CorpseContents { .. })
        })
        .await
        .expect("dying leaves a corpse and its owner is told what is on it");
    let ServerWorldMsg::CorpseContents { corpse_id, items, .. } = contents else {
        unreachable!()
    };
    assert_eq!(items.len(), 3, "cap, robe and bread are on the corpse: {items:?}");
    assert_eq!(items[0].0, CAP, "worn gear first on the corpse, in paperdoll order");
    assert_eq!(items[1].0, ROBE);

    a.send_respawn();
    a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(5), |m| matches!(m, ServerWorldMsg::Teleport { .. }))
        .await
        .expect("respawned at the bind, beside the corpse");

    a.send_loot_all(corpse_id);
    let mut msgs = Vec::new();
    let end = Instant::now() + Duration::from_secs(2);
    while Instant::now() < end {
        tick_one(&mut a.client, &mut a.transport);
        while let Some(bytes) = a.client.receive_message(CHANNEL_SYSTEM) {
            if let Ok((msg, _)) =
                bincode::serde::decode_from_slice::<ServerWorldMsg, _>(&bytes, bincode_cfg())
            {
                msgs.push(msg);
            }
        }
        tokio::time::sleep(TICK_DT).await;
    }
    let equip_delta = |slot: u32, path: &str| {
        msgs.iter().any(|m| matches!(m, ServerWorldMsg::InventoryDelta { location, slot: s, item_path: Some(p), .. }
            if location == "equip" && *s == slot && p == path))
    };
    assert!(equip_delta(2, CAP), "the cap goes back on the head");
    assert!(equip_delta(3, ROBE), "the robe goes back on the chest");
    assert!(
        msgs.iter().any(|m| matches!(m, ServerWorldMsg::InventoryDelta { location, item_path: Some(p), .. }
            if location == "base" && p == BREAD)),
        "the carried bread goes to the bags"
    );
    assert!(
        !msgs.iter().any(|m| matches!(m, ServerWorldMsg::InventoryDelta { location, item_path: Some(p), .. }
            if location != "equip" && (p == CAP || p == ROBE))),
        "the worn gear does not also land in the bags"
    );

    // Persisted with the loot, in one transaction.
    let worn: Vec<(i64, String)> = sqlx::query_as(
        "SELECT slot, item_path FROM character_items WHERE char_id = ?1 AND location = 'equip' ORDER BY slot",
    )
    .bind(char_id)
    .fetch_all(&pool)
    .await
    .expect("read worn rows");
    assert_eq!(worn, vec![(2, CAP.to_string()), (3, ROBE.to_string())], "the database holds the gear as worn");
    a.send_disconnect();
    a.pump_for(Duration::from_millis(200)).await;
}

/// Resurrection sickness (the corpse plan's slice 3; the P99 design, user call
/// 2026-10-15). A dies and respawns beside the corpse; B, a Cleric 20, casts
/// Resurrection (Minor) on it; A accepts. A is summoned, told "You are
/// exhausted.", lands at a quarter of its max HP with no mana, and its buff
/// snapshot carries the five-minute sickness.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_accepted_resurrection_leaves_its_sickness() {
    let h = start_both().await;
    let (a_session, a_char_id, _) =
        provision_client(&h.auth_url, "ressick_a", "Ressa", "Human", "Warrior").await;
    let (b_session, b_char_id, _) =
        provision_client(&h.auth_url, "ressick_b", "Ressb", "Human", "Cleric").await;
    let pool = db::open(&h.db_url).await.expect("open pool");
    db::set_account_gm(&pool, "ressick_a", true).await.expect("set is_gm");
    set_char_level(&h.db_url, b_char_id, 20).await;
    let a_token = request_world_token(&h.auth_url, &a_session, a_char_id).await;
    let b_token = request_world_token(&h.auth_url, &b_session, b_char_id).await;
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    let mut b = WorldClient::start(b_token, &b_session, b_char_id).await;
    pump_both_for(&mut a, &mut b, Duration::from_millis(300)).await;

    // A dies where it stands and respawns at the bind, which is the same
    // spot, so the corpse is in the cleric's reach.
    a.send_damage_self(1_000_000);
    let contents = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(5), |m| {
            matches!(m, ServerWorldMsg::CorpseContents { .. })
        })
        .await
        .expect("dying leaves a corpse");
    let ServerWorldMsg::CorpseContents { corpse_id, .. } = contents else { unreachable!() };
    a.send_respawn();
    a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(5), |m| matches!(m, ServerWorldMsg::Teleport { .. }))
        .await
        .expect("respawned");
    pump_both_for(&mut a, &mut b, Duration::from_millis(200)).await;

    // B casts the six-second resurrection on the corpse.
    b.send_cast_start("Resurrection (Minor)", 6.0);
    pump_both_for(&mut a, &mut b, Duration::from_millis(6200)).await;
    b.send_cast_spell("Resurrection (Minor)", Some(corpse_id));
    pump_both_for(&mut a, &mut b, Duration::from_millis(100)).await;
    let offer = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(5), |m| {
            matches!(m, ServerWorldMsg::ResurrectOffer { corpse_id: c, .. } if *c == corpse_id)
        })
        .await
        .expect("the owner is offered the resurrection");
    let ServerWorldMsg::ResurrectOffer { xp_percent, .. } = offer else { unreachable!() };
    assert_eq!(xp_percent, 25);

    // A accepts: summoned, told, and sick for five minutes.
    a.send_resurrect_accept(corpse_id, true);
    pump_both_for(&mut a, &mut b, Duration::from_millis(100)).await;
    let mut msgs = Vec::new();
    let end = Instant::now() + Duration::from_secs(3);
    while Instant::now() < end {
        tick_one(&mut a.client, &mut a.transport);
        tick_one(&mut b.client, &mut b.transport);
        while let Some(bytes) = a.client.receive_message(CHANNEL_SYSTEM) {
            if let Ok((msg, _)) =
                bincode::serde::decode_from_slice::<ServerWorldMsg, _>(&bytes, bincode_cfg())
            {
                msgs.push(msg);
            }
        }
        tokio::time::sleep(TICK_DT).await;
    }
    assert!(
        msgs.iter().any(|m| matches!(m, ServerWorldMsg::Teleport { .. })),
        "the accept summons the player to the corpse"
    );
    let sickness = msgs.iter().find_map(|m| match m {
        ServerWorldMsg::BuffSnapshot { target, buffs } if *target == a_char_id as u64 => buffs
            .iter()
            .find(|(name, _)| name == "Resurrection Sickness")
            .map(|(_, remaining)| *remaining),
        _ => None,
    });
    let remaining = sickness.expect("the owner's buff snapshot carries Resurrection Sickness");
    assert!(
        (295.0..=300.0).contains(&remaining),
        "the sickness lasts five minutes from the accept, got {remaining}"
    );
    assert!(
        msgs.iter().any(|m| matches!(m, ServerWorldMsg::ChatMessage { text, .. } if text == "You are exhausted.")),
        "the owner is told EQ's line"
    );
    // The landing: a quarter of max HP, no mana, both told at once. Regen
    // fans its own updates through the window, so look for the landing
    // values rather than the last word.
    let hp_updates: Vec<(f32, f32)> = msgs.iter().filter_map(|m| match m {
        ServerWorldMsg::HealthUpdate { id, hp, max_hp } if *id == a_char_id as u64 => Some((*hp, *max_hp)),
        _ => None,
    }).collect();
    assert!(
        hp_updates.iter().any(|(hp, max_hp)| (hp - max_hp * 0.25).abs() < 1.0),
        "lands at a quarter of max HP; saw {hp_updates:?}"
    );
    let mp_updates: Vec<f32> = msgs.iter().filter_map(|m| match m {
        ServerWorldMsg::ManaUpdate { id, mp, .. } if *id == a_char_id as u64 => Some(*mp),
        _ => None,
    }).collect();
    assert!(mp_updates.contains(&0.0), "lands with no mana; saw {mp_updates:?}");
    a.send_disconnect();
    b.send_disconnect();
    a.pump_for(Duration::from_millis(200)).await;
    b.pump_for(Duration::from_millis(200)).await;
}
