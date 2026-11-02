//! Saving: what is written at once and what survives a restart, a failed load, a
//! failed logout save or a stop signal.

use crate::common::*;

/// Track 14.3 â€” a bag plus its contents persist across a
/// reconnect. Seed `base[0] = Small Pouch` + `bag_0[2] = potions`
/// via DB, EnterWorld, assert the InventorySnapshot includes both
/// rows so the client can reconstruct the bag interior.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bag_contents_persist_across_reconnect() {
    const POUCH: &str = "res://data/loot/items/small_pouch.tres";
    const POTION: &str = "res://data/loot/items/minor_healing_potion.tres";
    let h = start_both().await;
    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "bag1", "Bagholder", "Human", "Warrior").await;
    let db_url = h.db_url.clone();
    let pool = projectdawn_server::db::open(&db_url).await.expect("open pool");
    projectdawn_server::db::save_inventory(
        &pool,
        a_char_id,
        &[
            projectdawn_server::db::InventoryRow {
                location: "base".into(),
                slot: 0,
                item_path: POUCH.into(),
                count: 1,
            },
            projectdawn_server::db::InventoryRow {
                location: "bag_0".into(),
                slot: 2,
                item_path: POTION.into(),
                count: 5,
            },
        ],
    )
    .await
    .expect("seed");
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;

    let snap = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::InventorySnapshot { .. })
        })
        .await
        .expect("snapshot");
    if let ServerWorldMsg::InventorySnapshot { entries } = snap {
        let has_bag = entries
            .iter()
            .any(|(loc, slot, path, _)| loc == "base" && *slot == 0 && path == POUCH);
        let has_potion = entries.iter().any(|(loc, slot, path, count)| {
            loc == "bag_0" && *slot == 2 && path == POTION && *count == 5
        });
        assert!(has_bag, "snapshot must include the parent pouch row");
        assert!(has_potion, "snapshot must include the bag_0 inner stack");
    }
}

/// Track 13.3 â€” InventorySnapshot on EnterWorld includes persisted
/// equip rows. Seed an equipped sword + a base cloth via DB, assert
/// the snapshot's entries list contains both with the right
/// location strings.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn snapshot_includes_persisted_equipment() {
    let h = start_both().await;
    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "eq3", "Equipsnap", "Human", "Warrior").await;
    let db_url = h.db_url.clone();
    let pool = projectdawn_server::db::open(&db_url).await.expect("open pool");
    projectdawn_server::db::save_inventory(
        &pool,
        a_char_id,
        &[
            projectdawn_server::db::InventoryRow {
                location: "base".into(),
                slot: 0,
                item_path: "res://items/cloth.tres".into(),
                count: 5,
            },
            projectdawn_server::db::InventoryRow {
                location: "equip".into(),
                slot: 0,
                item_path: "res://items/sword.tres".into(),
                count: 1,
            },
        ],
    )
    .await
    .expect("seed");
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;

    let snap = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::InventorySnapshot { .. })
        })
        .await
        .expect("snapshot");
    if let ServerWorldMsg::InventorySnapshot { entries } = snap {
        let has_base = entries
            .iter()
            .any(|(loc, _, path, count)| loc == "base" && path == "res://items/cloth.tres" && *count == 5);
        let has_equip = entries
            .iter()
            .any(|(loc, slot, path, count)| {
                loc == "equip" && *slot == 0 && path == "res://items/sword.tres" && *count == 1
            });
        assert!(has_base, "snapshot must include the base cloth row");
        assert!(has_equip, "snapshot must include the equipped sword row");
    }
}

/// Track 18.1 â€” on EnterWorld the server fans a SkillProgressSnapshot
/// containing the player's three score maps. The lib unit tests verify
/// the cap math; this test verifies the wire shape and that the seed
/// matches WeaponSkills' starting values for the character's class.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn skill_progress_snapshot_seeded_on_enter_world() {
    let h = start_both().await;
    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "skl", "Skiller", "Human", "Warrior").await;
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;

    let snap = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::SkillProgressSnapshot { .. })
        })
        .await
        .expect("SkillProgressSnapshot arrives on EnterWorld");

    if let ServerWorldMsg::SkillProgressSnapshot { weapon, armor, casting } = snap {
        // Shape: 10 weapon keys, 5 armor keys, 7 casting keys (one per
        // GDScript definition entry, regardless of whether the class
        // can train it). Casting went 6 -> 7 on 2026-07-20 when the
        // `meditate` regen skill was added (skills.rs CASTING_KEYS).
        assert_eq!(weapon.len(), 10, "weapon map has 10 keys");
        assert_eq!(armor.len(), 5, "armor map has 5 keys");
        assert_eq!(casting.len(), 7, "casting map has 7 keys");

        // Track 22.F rebalance: starting score is cap(L1) / 4 with
        // a floor of 1. Warrior 1h_slashing L1 cap = 4 â†’ 4/4 = 1.
        let weapon_map: std::collections::HashMap<String, u32> = weapon.into_iter().collect();
        assert_eq!(weapon_map.get("1h_slashing").copied(), Some(1));
        // Warrior plate L1 cap = 4 â†’ 4/4 = 1 (same floor).
        let armor_map: std::collections::HashMap<String, u32> = armor.into_iter().collect();
        assert_eq!(armor_map.get("plate").copied(), Some(1));
        // Warrior has no casting; all six rows present at 0.
        let casting_map: std::collections::HashMap<String, u32> = casting.into_iter().collect();
        for key in &["evocation", "alteration", "abjuration", "conjuration", "divination", "channeling"] {
            assert_eq!(casting_map.get(*key).copied(), Some(0), "warrior casting {} is 0", key);
        }
    }
}

/// Track 18.1 â€” `character_skills` rows survive disconnect and are
/// re-applied on reconnect. We pre-write a row directly to SQLite
/// before EnterWorld so the load path overlays our value on top of
/// the seed_starting_scores baseline; the snapshot must reflect the
/// pre-written score. Mirrors the bag_contents_persist_across_reconnect
/// pattern from Track 14.3.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn skill_progress_persists_load_path() {
    let h = start_both().await;
    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "skp", "Sklp", "Human", "Cleric").await;

    // Pre-seed a non-default casting score via direct SQL. Mimics what
    // the server's save path would write at checkpoint / disconnect
    // after an in-flight advance â€” proves the load + snapshot path
    // independently of the rng-driven try_advance roll.
    let pool = projectdawn_server::db::open(&h.db_url).await.expect("open pool");
    sqlx::query(
        "INSERT INTO character_skills (char_id, kind, key, score)
         VALUES (?1, 'casting', 'alteration', 17)",
    )
    .bind(a_char_id)
    .execute(&pool)
    .await
    .expect("seed alteration row");

    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;

    let snap = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::SkillProgressSnapshot { .. })
        })
        .await
        .expect("SkillProgressSnapshot arrives");

    if let ServerWorldMsg::SkillProgressSnapshot { casting, .. } = snap {
        let casting_map: std::collections::HashMap<String, u32> = casting.into_iter().collect();
        assert_eq!(
            casting_map.get("alteration").copied(),
            Some(17),
            "persisted alteration score overlays the L1 default"
        );
    }
}

/// A character that leaves while dead (a crash, a window-X, a restart) loads
/// at 0 HP; it used to die AGAIN on its first tick back (`death_processed`
/// was false for every spawn), paying a second XP penalty and leaving a
/// second, empty corpse. Found by the 2026-10-12 review of the public
/// hosting branch's restart path, which makes everyone dead at that moment
/// pay it at once; the bug itself is older than that path.
#[tokio::test]
async fn a_dead_character_stays_dead_on_relogin_and_pays_once() {
    let h = start_both().await;
    let (session, char_id, _) =
        provision_client(&h.auth_url, "corpse", "Corpsey", "Human", "Warrior").await;
    // Below the level floor there is no XP penalty to double; level 10 has one.
    set_char_level(&h.db_url, char_id, 10).await;
    let token = gm_world_token(&h.auth_url, &h.db_url, "corpse", &session, char_id).await;
    let mut a = WorldClient::start(token, &session, char_id).await;
    let cid = char_id as u64;
    a.send_death();
    let dead = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::HealthUpdate { id, hp, .. } if *id == cid && *hp <= 0.01)
        })
        .await;
    assert!(dead.is_some(), "the first death lands");
    a.pump_for(Duration::from_millis(300)).await;
    // Leave dead: the clean disconnect reaps at once and checkpoints 0 HP.
    a.send_disconnect();
    a.pump_for(Duration::from_millis(500)).await;
    drop(a);
    tokio::time::sleep(Duration::from_millis(500)).await;

    let pool = db::open(&h.db_url).await.expect("open pool");
    let after_first = db::load_character(&pool, char_id).await.expect("load");
    assert!(after_first.hp <= 0.0, "left dead: hp {}", after_first.hp);
    let corpses_after_first: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM corpses WHERE char_id = ?1")
            .bind(char_id)
            .fetch_one(&pool)
            .await
            .expect("count");
    assert_eq!(corpses_after_first, 1, "one corpse from one death");

    // Log back in dead, stand there, leave again (another checkpoint).
    let token = request_world_token(&h.auth_url, &session, char_id).await;
    let mut b = WorldClient::start(token, &session, char_id).await;
    let seeded_dead = b
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::HealthUpdate { id, hp, .. } if *id == cid && *hp <= 0.01)
        })
        .await;
    assert!(seeded_dead.is_some(), "the client is told it is dead by the HP seed");
    b.pump_for(Duration::from_millis(600)).await;
    b.send_disconnect();
    b.pump_for(Duration::from_millis(500)).await;
    drop(b);
    tokio::time::sleep(Duration::from_millis(500)).await;

    let after_relog = db::load_character(&pool, char_id).await.expect("load");
    assert_eq!(
        (after_relog.level, after_relog.xp),
        (after_first.level, after_first.xp),
        "no second XP penalty on relogging dead"
    );
    let corpses_after_relog: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM corpses WHERE char_id = ?1")
            .bind(char_id)
            .fetch_one(&pool)
            .await
            .expect("count");
    assert_eq!(corpses_after_relog, 1, "no second corpse either");
}

/// A character that left dead with full mana and stamina (a Warrior, whose
/// token mana is always full) is told its HP on entering the world. The regen
/// fan sends only what changed since its last send, and nothing changes on the
/// dead with full pools, so this character used to hear nothing: the client
/// kept the full bars of its login reset, never learned it was dead and never
/// respawned, while the server refused every move (playtest 2026-10-26, "Bun
/// is stuck"). The test above passed only because its stamina was regenerating.
#[tokio::test]
async fn a_dead_character_with_full_pools_is_told_it_is_dead() {
    let h = start_both().await;
    let (session, char_id, _) =
        provision_client(&h.auth_url, "stuckbun", "Stuckbun", "Gnome", "Warrior").await;
    // Dead, with mana and stamina at their maximum (the load clamps 99999).
    let pool = db::open(&h.db_url).await.expect("open pool");
    sqlx::query("UPDATE characters SET hp = 0.0, mp = 99999.0, stamina = 99999.0 WHERE id = ?1")
        .bind(char_id)
        .execute(&pool)
        .await
        .expect("leave dead with full pools");
    let token = request_world_token(&h.auth_url, &session, char_id).await;
    let mut a = WorldClient::start(token, &session, char_id).await;
    let cid = char_id as u64;
    let told_dead = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::HealthUpdate { id, hp, .. } if *id == cid && *hp <= 0.01)
        })
        .await;
    assert!(told_dead.is_some(), "a dead joiner is told its HP even when nothing regenerates");

    // Told it is dead, the client respawns, and the server stands it back up.
    a.send_respawn();
    let alive = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::HealthUpdate { id, hp, .. } if *id == cid && *hp > 0.0)
        })
        .await;
    assert!(alive.is_some(), "the respawn lands");
}

/// The camp-and-linkdead design's deny-login rule from the client's seat.
/// `banker_slice2_checklist.md` row 54 is ticked with a note saying the
/// opposite ("killed A's client, then immediately logged back in
/// successfully"), so this pins which is true. A client that dies without a
/// Disconnect leaves its body in the world, a same-account relogin is refused
/// until the body is reaped, and the refusals never shorten the window: the
/// Lineage II force-off (relog to boot your own live session) and the
/// dodge-the-reap (knock until the body is gone) patterns both fail. Three
/// phases on the server's own clocks: before the 10 s heartbeat timeout the
/// old netcode connection still holds the client id, so a second handshake
/// is ignored outright; after it the body is linkdead and the Connect is
/// answered with a DuplicateLogin kick carrying the countdown; after
/// LINKDEAD_SECS the body is gone and the login succeeds. About 45 s of
/// wall clock, all of it the server's own timers.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unclean_drop_refuses_relogin_until_the_body_is_reaped() {
    let h = start_both().await;
    let (session, char_id, _stale) =
        provision_client(&h.auth_url, "crasher", "Dropsy", "Human", "Warrior").await;
    let token = request_world_token(&h.auth_url, &session, char_id).await;
    let mut a = WorldClient::start(token, &session, char_id).await;
    a.pump_for(Duration::from_millis(300)).await;
    // The crash: no Disconnect, no netcode goodbye, the socket just closes.
    drop(a);
    let dropped_at = Instant::now();

    // Phase 1: the dropped session is still live as far as the server knows.
    // A relogin must not boot it; netcode ignores the second handshake.
    let token = request_world_token(&h.auth_url, &session, char_id).await;
    match try_connect(token, &session, char_id, Duration::from_secs(4)).await {
        ConnectOutcome::HandshakeTimedOut => {}
        ConnectOutcome::Kicked { code, .. } => {
            panic!("expected the handshake to be ignored while the old connection lives, got a kick: {code:?}")
        }
        ConnectOutcome::Connected(_) => {
            panic!("a relogin got in while the dropped session was still live: the force-off door is open")
        }
    }

    // Phase 2: the heartbeat timeout (10 s) drops the transport and the body
    // goes linkdead; from then on the Connect is refused with the countdown.
    // Keep knocking: every knock inside the window is a relogin that must
    // not reset it.
    let mut countdown: Option<(u32, Instant)> = None;
    let deadline = dropped_at + Duration::from_secs(25);
    while countdown.is_none() && Instant::now() < deadline {
        let token = request_world_token(&h.auth_url, &session, char_id).await;
        match try_connect(token, &session, char_id, Duration::from_secs(2)).await {
            ConnectOutcome::HandshakeTimedOut => {
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            ConnectOutcome::Kicked { code, reconnect_after_secs } => {
                assert!(
                    matches!(code, protocol::world::KickCode::DuplicateLogin),
                    "a relogin during the linger is refused as a duplicate login, got {code:?}"
                );
                let secs = reconnect_after_secs
                    .expect("a refusal while the body is linkdead carries the countdown");
                assert!(
                    secs <= world::LINKDEAD_SECS.as_secs() as u32,
                    "the countdown is at most the linger window, got {secs}"
                );
                countdown = Some((secs, Instant::now()));
            }
            ConnectOutcome::Connected(_) => {
                panic!("a relogin got in while the body was lingering linkdead")
            }
        }
    }
    let (secs, told_at) = countdown
        .expect("within 25 s of the drop the body is linkdead and a relogin is refused with a countdown");

    // Phase 3: the window is the body's clock, not the relogin's. Knock every
    // few seconds through the countdown the server itself announced (each a
    // refusal), then expect the door open within a few seconds of it. A reap
    // timer reset by the knocking would leave this waiting past the deadline.
    let open_by = told_at + Duration::from_secs(secs as u64 + 10);
    let mut connected: Option<WorldClient> = None;
    while connected.is_none() && Instant::now() < open_by {
        let token = request_world_token(&h.auth_url, &session, char_id).await;
        match try_connect(token, &session, char_id, Duration::from_secs(2)).await {
            ConnectOutcome::Connected(c) => connected = Some(c),
            ConnectOutcome::Kicked { code, .. } => {
                assert!(
                    matches!(code, protocol::world::KickCode::DuplicateLogin),
                    "still the duplicate-login refusal while the body lingers, got {code:?}"
                );
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
            ConnectOutcome::HandshakeTimedOut => {
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    }
    let mut b = connected.expect(
        "the relogin succeeds once the announced window has elapsed; a refused relogin must not have extended it",
    );
    assert!(
        b.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::HealthUpdate { .. } | ServerWorldMsg::Position { .. } | ServerWorldMsg::TimeOfDay { .. })
        })
        .await
        .is_some(),
        "the fresh login is a real session: the server talks to it"
    );
    b.send_disconnect();
    b.pump_for(Duration::from_millis(300)).await;
    drop(b);
}

/// The bars players already had lived on their own machines: a character the
/// server never held a bar for may hand its bar over once, every spell checked
/// as a memorize would check it (Fireball is no Cleric's; gem 3 is not open at
/// level 1). A second seed changes nothing, and the bar is the same after a
/// relog.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_bar_seeds_once_and_survives_a_relog() {
    let h = start_both().await;
    let (session, char_id, token) =
        provision_client(&h.auth_url, "seeder", "Seeda", "Human", "Cleric").await;
    forget_all_spells(&h.db_url, char_id).await;
    let mut a = WorldClient::start(token, &session, char_id).await;
    next_spell_gems(&mut a).await.expect("the bar arrives on entry");

    a.send_seed_spell_gems(&["Smite", "Fireball", "", "Healing Light"]);
    let (gems, seeded) = next_spell_gems(&mut a).await.expect("the seeded bar");
    assert_eq!(gems[0], "Smite");
    assert!(gems[1].is_empty() && gems[3].is_empty(), "another class's spell and a closed gem are dropped");
    assert!(seeded);

    a.send_seed_spell_gems(&["Healing Light"]);
    let (gems, _) = next_spell_gems(&mut a).await.expect("a second seed answers with the bar");
    assert_eq!(gems[0], "Smite", "a second seed changes nothing");

    a.send_disconnect();
    a.pump_for(Duration::from_millis(500)).await;
    drop(a);
    tokio::time::sleep(Duration::from_millis(500)).await;
    let token = request_world_token(&h.auth_url, &session, char_id).await;
    let mut b = WorldClient::start(token, &session, char_id).await;
    let (gems, seeded) = next_spell_gems(&mut b).await.expect("the bar arrives again");
    assert_eq!(gems[0], "Smite");
    assert!(seeded, "and is still the server's");
}

/// NO RENT (EverQuest's TEMPORARY tag; user 2026-10-19): a character logged
/// out for thirty minutes loses its NO RENT items at the next login; ten
/// minutes away keeps them; an ordinary item is never touched. Logging out
/// stamps the time.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_rent_items_go_after_thirty_minutes_logged_out() {
    const CONJURED: &str = "res://data/loot/items/conjured_bread.tres";
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64;
    let h = start_both().await;
    let pool = db::open(&h.db_url).await.expect("open pool");

    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "longgone", "Longa", "Human", "Magician").await;
    let (b_session, b_char_id, b_token) =
        provision_client(&h.auth_url, "quickrelog", "Quicka", "Human", "Magician").await;
    for (id, secs_ago) in [(a_char_id, 31 * 60), (b_char_id, 10 * 60)] {
        give_char_item(&h.db_url, id, "base", 0, CONJURED, 4).await;
        give_char_item(&h.db_url, id, "base", 1, BREAD, 2).await;
        sqlx::query("UPDATE characters SET logged_out_at = ?1 WHERE id = ?2")
            .bind(now - secs_ago)
            .bind(id)
            .execute(&pool)
            .await
            .expect("stamp the logout");
    }

    let mut paths_of = Vec::new();
    for (session, id, token) in [(a_session, a_char_id, a_token), (b_session, b_char_id, b_token)] {
        let mut c = WorldClient::start(token, &session, id).await;
        let snap = c
            .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| matches!(m, ServerWorldMsg::InventorySnapshot { .. }))
            .await;
        let Some(ServerWorldMsg::InventorySnapshot { entries }) = snap else { panic!("an inventory on entry") };
        paths_of.push(entries.into_iter().map(|e| e.2).collect::<Vec<String>>());
        c.send_disconnect();
        c.pump_for(Duration::from_millis(500)).await;
    }
    assert!(!paths_of[0].iter().any(|p| p == CONJURED), "thirty minutes away: the conjured bread is gone");
    assert!(paths_of[0].iter().any(|p| p == BREAD), "an ordinary loaf stays");
    assert!(paths_of[1].iter().any(|p| p == CONJURED), "ten minutes away: the conjured bread stays");

    tokio::time::sleep(Duration::from_millis(500)).await;
    let stamped: Option<i64> = sqlx::query_scalar("SELECT logged_out_at FROM characters WHERE id = ?1")
        .bind(b_char_id)
        .fetch_one(&pool)
        .await
        .expect("read the stamp");
    assert!(stamped.is_some_and(|t| (t - now).abs() < 30), "logging out stamped the time: {stamped:?}");
}

/// A drop is durable at once. The bag lives only in memory and its picker is
/// saved at their own logout, so a dropper whose removal waited for the 60 s
/// checkpoint could get the item back from a restart after someone else had
/// saved it. The dropper's row must be gone from the DB the moment the bag
/// exists, with no checkpoint and no logout in between.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dropped_item_leaves_the_droppers_saved_inventory_at_once() {
    let h = start_both().await;
    let (session, char_id, token) =
        provision_client(&h.auth_url, "dropdurable", "Dropper", "Human", "Warrior").await;
    give_char_item(&h.db_url, char_id, "base", 0, BREAD, 3).await;
    let mut a = WorldClient::start(token, &session, char_id).await;
    a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::InventorySnapshot { .. })
    })
    .await
    .expect("snapshot");

    a.send_drop_item("base", 0, 0);
    a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::LootBagSpawn { items, .. }
            if items.iter().any(|(p, c)| p == BREAD && *c == 3))
    })
    .await
    .expect("the bag spawns with the dropped stack");

    let saved = count_item_rows(
        &h.db_url,
        "SELECT COUNT(*) FROM character_items WHERE char_id = ?1 AND item_path = ?2",
        char_id,
        BREAD,
    )
    .await;
    assert_eq!(saved, 0, "the dropped stack is still in the dropper's saved inventory");
}

/// The other half of a drop: whoever picks the stack up is saved in the same
/// tick. The dropper's row is already gone, so a picker left unsaved until the
/// checkpoint would lose the item to a crash.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_picked_up_drop_is_saved_on_the_picker_at_once() {
    let h = start_both().await;
    let (a_session, a_char, a_token) =
        provision_client(&h.auth_url, "giftdrop", "Giver", "Human", "Warrior").await;
    let (b_session, b_char, b_token) =
        provision_client(&h.auth_url, "giftpick", "Taker", "Human", "Warrior").await;
    give_char_item(&h.db_url, a_char, "base", 0, BREAD, 2).await;
    let mut a = WorldClient::start(a_token, &a_session, a_char).await;
    let mut b = WorldClient::start(b_token, &b_session, b_char).await;

    a.send_drop_item("base", 0, 0);
    a.pump_for(Duration::from_millis(100)).await;
    let bag_id = match b
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::LootBagSpawn { items, .. }
                if items.iter().any(|(p, _)| p == BREAD))
        })
        .await
        .expect("the bag reaches the peer")
    {
        ServerWorldMsg::LootBagSpawn { bag_id, .. } => bag_id,
        _ => unreachable!(),
    };
    b.send_loot_all(bag_id);
    b.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::InventoryDelta { item_path: Some(p), .. } if p == BREAD)
    })
    .await
    .expect("the loaves reach the picker");

    let picker_saved = count_item_rows(
        &h.db_url,
        "SELECT COUNT(*) FROM character_items WHERE char_id = ?1 AND item_path = ?2",
        b_char,
        BREAD,
    )
    .await;
    assert_eq!(picker_saved, 1, "the picked-up stack is not in the picker's saved inventory");
}

/// A burst of drops in one tick is capped (MAX_DROPS_PER_TICK, 4): each
/// dropper is saved once per tick, and the cap bounds what a forged burst can
/// make that save and the bag fan carry. Six drops in one datagram: four
/// leave the saved inventory, two stay.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_burst_of_drops_in_one_tick_is_capped() {
    let h = start_both().await;
    let (session, char_id, token) =
        provision_client(&h.auth_url, "dropburst", "Flooder", "Human", "Warrior").await;
    for slot in 0..6 {
        give_char_item(&h.db_url, char_id, "base", slot, BREAD, 1).await;
    }
    let mut a = WorldClient::start(token, &session, char_id).await;
    a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::InventorySnapshot { .. })
    })
    .await
    .expect("snapshot");

    for slot in 0..6 {
        a.send_drop_item("base", slot, 0);
    }
    a.pump_for(Duration::from_millis(800)).await;

    let left = count_item_rows(
        &h.db_url,
        "SELECT COUNT(*) FROM character_items WHERE char_id = ?1 AND item_path = ?2",
        char_id,
        BREAD,
    )
    .await;
    assert_eq!(left, 2, "four drops apply per tick and are saved; the rest of the burst is ignored");
}

/// An immediate save writes every dirty store, never one alone. Withdraw a
/// loaf from the bank (vault and inventory both dirty, nothing saved yet),
/// then die before any checkpoint: the corpse transaction writes the
/// inventory onto the corpse, and the vault must be written with it, or the
/// loaf sits on the corpse AND in the vault row after a crash.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_death_after_an_unsaved_bank_withdrawal_leaves_the_item_in_one_place() {
    let h = start_both().await;
    let (session, char_id, _stale) =
        provision_client(&h.auth_url, "bankdeath", "Withdrawer", "Human", "Warrior").await;
    {
        let pool = projectdawn_server::db::open(&h.db_url).await.expect("open pool");
        sqlx::query("INSERT INTO bank_items (char_id, slot, item_path, count) VALUES (?1, 0, ?2, 1)")
            .bind(char_id)
            .bind(BREAD)
            .execute(&pool)
            .await
            .expect("seed the vault");
    }
    let token = gm_world_token(&h.auth_url, &h.db_url, "bankdeath", &session, char_id).await;
    let mut a = WorldClient::start(token, &session, char_id).await;
    a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::InventorySnapshot { .. })
    })
    .await
    .expect("snapshot");

    // The starter spawn is within reach of the banker (npcs.rs pins it).
    a.send_bank_withdraw_item(0);
    a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::InventoryDelta { item_path: Some(p), .. } if p == BREAD)
    })
    .await
    .expect("the loaf arrives in the bags");

    a.send_death();
    a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::InventorySnapshot { entries } if entries.is_empty())
    })
    .await
    .expect("the death strip empties the bags");

    let in_vault = count_item_rows(
        &h.db_url,
        "SELECT COUNT(*) FROM bank_items WHERE char_id = ?1 AND item_path = ?2",
        char_id,
        BREAD,
    )
    .await;
    let on_corpse = count_item_rows(
        &h.db_url,
        "SELECT COUNT(*) FROM corpse_items ci JOIN corpses c ON c.corpse_id = ci.corpse_id
         WHERE c.char_id = ?1 AND ci.item_path = ?2",
        char_id,
        BREAD,
    )
    .await;
    assert_eq!(on_corpse, 1, "the loaf went onto the corpse");
    assert_eq!(in_vault, 0, "the loaf is ALSO still in the saved vault: a dupe");
}

/// A store that fails to load refuses the login. Playing on with an empty
/// inventory let the next save replace the real rows with nothing. A row the
/// decoder rejects (a count stored as text) stands in for a transient read
/// error; the login must be refused and the row left exactly as it was.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_store_that_fails_to_load_refuses_the_login() {
    let h = start_both().await;
    let (session, char_id, token) =
        provision_client(&h.auth_url, "badload", "Unlucky", "Human", "Warrior").await;
    {
        let pool = projectdawn_server::db::open(&h.db_url).await.expect("open pool");
        sqlx::query(
            "INSERT INTO character_items (char_id, location, slot, item_path, count)
             VALUES (?1, 'base', 0, ?2, 'not a number')",
        )
        .bind(char_id)
        .bind(BREAD)
        .execute(&pool)
        .await
        .expect("seed an undecodable row");
    }

    let answer = WorldClient::start_refused(token, &session, char_id).await;
    assert!(
        matches!(answer, ServerWorldMsg::Kick { .. }),
        "the login was accepted with an empty inventory: {answer:?}"
    );
    let rows = count_item_rows(
        &h.db_url,
        "SELECT COUNT(*) FROM character_items WHERE char_id = ?1 AND item_path = ?2",
        char_id,
        BREAD,
    )
    .await;
    assert_eq!(rows, 1, "the stored row must be untouched");
}

/// A stop signal (systemd's SIGTERM on `restart`) saves everyone and tells
/// them, instead of throwing away up to 60 s of progress. A walks and buys a
/// loaf (both only marked dirty, never saved before the 60 s checkpoint), the
/// world is stopped, and the walk and the loaf are both in the database; both
/// players were told the world is restarting; the world task ends cleanly.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stop_signal_saves_everyone_and_tells_them() {
    let mut h = start_both().await;
    let (a_session, a_char, a_token) =
        provision_client(&h.auth_url, "stopa", "Walker", "Human", "Warrior").await;
    let (b_session, b_char, b_token) =
        provision_client(&h.auth_url, "stopb", "Stander", "Human", "Warrior").await;
    fund_char(&h.db_url, a_char).await;
    let mut a = WorldClient::start(a_token, &a_session, a_char).await;
    let mut b = WorldClient::start(b_token, &b_session, b_char).await;

    a.send_buy_item(0, "Bread Loaf", 1);
    a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::InventoryDelta { item_path: Some(p), .. } if p == BREAD)
    })
    .await
    .expect("the loaf is bought");
    let mut seq = 0;
    walk_east(&mut a, &mut seq, 6.0).await;

    h.shutdown.send(true).expect("send the stop signal");
    let (ka, kb) = kicks_for_both(&mut a, &mut b).await;
    assert_eq!(ka, Some(protocol::world::KickCode::Restart), "the walker was told the world is restarting");
    assert_eq!(kb, Some(protocol::world::KickCode::Restart), "the bystander was told too");
    let world = h.world.take().expect("world handle");
    let ended = tokio::time::timeout(Duration::from_secs(5), world)
        .await
        .expect("the world task ends after the stop signal")
        .expect("the world task did not panic");
    assert!(ended.is_ok(), "a stop signal is a clean end: {ended:?}");

    let pool = projectdawn_server::db::open(&h.db_url).await.expect("open pool");
    let (pos_x,): (Option<f32>,) = sqlx::query_as("SELECT pos_x FROM characters WHERE id = ?1")
        .bind(a_char)
        .fetch_one(&pool)
        .await
        .expect("position row");
    assert!(pos_x.unwrap_or(0.0) > 3.0, "the walk was not saved: pos_x = {pos_x:?}");
    let loaves = count_item_rows(
        &h.db_url,
        "SELECT COUNT(*) FROM character_items WHERE char_id = ?1 AND item_path = ?2",
        a_char,
        BREAD,
    )
    .await;
    assert_eq!(loaves, 1, "the bought loaf was not saved");
}

/// A logout whose save fails is not thrown away: it is retried, and the
/// account's relogin is refused until it lands (letting it in would load the
/// older saved state). A test-only trigger makes every inventory write fail;
/// the player buys a loaf and logs out; the relogin is refused; the trigger
/// is dropped; the retry lands and the relogin succeeds with the loaf saved.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_logout_save_holds_the_relogin_until_it_lands() {
    let h = start_both().await;
    let (session, char_id, token) =
        provision_client(&h.auth_url, "savefail", "Unsaved", "Human", "Warrior").await;
    fund_char(&h.db_url, char_id).await;
    let mut a = WorldClient::start(token, &session, char_id).await;
    a.send_buy_item(0, "Bread Loaf", 1);
    a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::InventoryDelta { item_path: Some(p), .. } if p == BREAD)
    })
    .await
    .expect("the loaf is bought");

    let pool = projectdawn_server::db::open(&h.db_url).await.expect("open pool");
    sqlx::query(
        "CREATE TRIGGER fail_inventory_writes BEFORE INSERT ON character_items
         BEGIN SELECT RAISE(ABORT, 'test: inventory writes fail'); END",
    )
    .execute(&pool)
    .await
    .expect("install the failing trigger");

    a.send_disconnect();
    a.pump_for(Duration::from_millis(500)).await;
    drop(a);
    tokio::time::sleep(Duration::from_millis(500)).await;

    let token = request_world_token(&h.auth_url, &session, char_id).await;
    let answer = WorldClient::start_refused(token, &session, char_id).await;
    assert!(
        matches!(&answer, ServerWorldMsg::Kick { reason, .. } if reason.contains("still being saved")),
        "the relogin must wait for the pending save: {answer:?}"
    );

    sqlx::query("DROP TRIGGER fail_inventory_writes")
        .execute(&pool)
        .await
        .expect("drop the trigger");
    tokio::time::sleep(Duration::from_millis(2_500)).await;

    let token = request_world_token(&h.auth_url, &session, char_id).await;
    let answer = WorldClient::start_refused(token, &session, char_id).await;
    assert!(
        matches!(answer, ServerWorldMsg::ConnectOk { .. }),
        "once the retry lands the relogin succeeds: {answer:?}"
    );
    let loaves = count_item_rows(
        &h.db_url,
        "SELECT COUNT(*) FROM character_items WHERE char_id = ?1 AND item_path = ?2",
        char_id,
        BREAD,
    )
    .await;
    assert_eq!(loaves, 1, "the retried save holds the loaf");
}

/// A death's XP loss is saved with the corpse that records it. Before, the
/// corpse (and its refundable `lost_xp`) was written at once while the loss
/// waited for the checkpoint, so a crash gave the XP back and a resurrection
/// then refunded part of it again. A level 10 with no progress dies and
/// drops to level 9; the database says 9 the moment the corpse exists.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_death_saves_its_xp_loss_with_the_corpse() {
    let h = start_both().await;
    let (session, char_id, _stale) =
        provision_client(&h.auth_url, "xpdeath", "Fallen", "Human", "Warrior").await;
    set_char_level(&h.db_url, char_id, 10).await;
    let token = gm_world_token(&h.auth_url, &h.db_url, "xpdeath", &session, char_id).await;
    let mut a = WorldClient::start(token, &session, char_id).await;
    a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::InventorySnapshot { .. })
    })
    .await
    .expect("snapshot");

    a.send_death();
    a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::CorpseContents { .. })
    })
    .await
    .expect("the corpse is made");

    let (_xp, level) = char_xp_level(&h.db_url, char_id).await;
    assert_eq!(level, 9, "the de-level must be saved with the corpse");
}

/// A quest's reward XP is saved with its completion. Before, the completion
/// was written at once and the XP waited for the checkpoint, so a crash kept
/// the quest done and lost the reward for good.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_quest_turn_in_saves_its_reward_xp_with_the_completion() {
    let h = start_both().await;
    let (session, char_id, token) =
        provision_client(&h.auth_url, "questxp", "Rewarded", "Human", "Warrior").await;
    {
        let pool = projectdawn_server::db::open(&h.db_url).await.expect("open pool");
        projectdawn_server::db::save_quest_progress(&pool, char_id, "rat_infestation", &[8])
            .await
            .expect("seed a finished quest");
    }
    let before = char_xp_level(&h.db_url, char_id).await;
    let mut a = WorldClient::start(token, &session, char_id).await;

    send_msg(
        &mut a.client,
        CHANNEL_SYSTEM,
        &ClientWorldMsg::CompleteQuest { quest_id: "rat_infestation".into() },
    );
    a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::QuestCompleted { quest_id } if quest_id == "rat_infestation")
    })
    .await
    .expect("the quest is completed");

    let after = char_xp_level(&h.db_url, char_id).await;
    assert_ne!(after, before, "the reward XP must be saved with the completion");
}
