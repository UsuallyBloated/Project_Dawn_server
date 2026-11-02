//! Melee: enemies aggroing and swinging, player swings, the server's miss roll and
//! swing-rate limit, dual wield and the equip rules, and the active skills.

use crate::common::*;

/// Track 5 sub-task 1C: AI state machine drives Idle â†’ Chase â†’ Attack
/// for a server-spawned enemy when a player enters aggro range.
///
/// The player walks toward the Bonepile's isolated [-16, 0, -14] spawn
/// long enough to land well inside the Decrepit Skeleton's 8 m aggro
/// radius even with the Â±3 m XZ spawn jitter, then stops. The test asserts:
///
///   * an `EntityTarget` broadcast lands targeting the player (Idle â†’
///     Chase transition fired server-side);
///   * a `Hit` broadcast lands targeting the player from an enemy id
///     (Attack state fired its melee swing).
///
/// Death lifecycle (EntityDied + EntityDespawn) is reserved for sub-task
/// 3, where the player-attacks-enemy path lands enemy HP authoritatively.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn enemy_aggros_chases_and_attacks_player() {
    let h = start_both().await;

    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "zeta", "Zett", "Human", "Warrior").await;

    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;

    // Unit vector toward camp 0's [20, 0, 5] spawn.
    // Phase 4 layout note: every camp-walking test aims at the Bonepile's
    // isolated [-16, 0, -14] spawn â€” the one ring 1 spawn whose aggro circle
    // overlaps no other, so exactly ONE slow (1.8 m/s, 2.5 s swing) level 1
    // Decrepit Skeleton pulls, the same single-puller semantics these tests
    // were written against. Do NOT aim at the Wolf Run: it is a four-wolf
    // pack, and standing in it turns every cast into interrupt rolls.

    // Walk for ~2 s at 50 ms cadence. Each Move refreshes the server-side
    // stale-move clock so integration continues until we stop sending.
    // 3.5 s, not 2 s: at 2 s the player only clips camp 0's aggro radius, so
    // whether an enemy locks on before the wait expires depended on where it
    // happened to be wandering. Walking fully in makes the pull deterministic.
    // Walk in on the server's own word, not the wall clock (a slow test
    // thread used to come up short): to 2 m from the spawn point, well
    // inside its aggro whatever the spawn jitter.
    let mut seq: u32 = 1;
    walk_until_within(&mut a, &mut seq, a_char_id, Vec3 { x: 0.0, y: 0.0, z: 0.0 }, BONEPILE_SPAWN, 2.0).await;
    // Stop. The server's STALE_MOVE_THRESHOLD (500 ms) will park the
    // player at its current pos within ~10 ticks.

    // Generous timeouts because `cargo test --release` runs the whole
    // world test binary's tests in parallel by default â€” under
    // CPU contention the AI tick + position fan-out can fall behind by
    // several seconds while still being functionally correct. Run this
    // test in isolation (`cargo test ... enemy_aggros...`) and it
    // completes in ~3-4 s.
    let target_evt = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(35), |m| {
            matches!(
                m,
                ServerWorldMsg::EntityTarget { target: Some(t), .. } if *t == a_char_id as u64
            )
        })
        .await
        .expect("an enemy locks onto the player");
    if let ServerWorldMsg::EntityTarget { id, .. } = target_evt {
        assert!(
            id >= ENEMY_ID_BASE,
            "EntityTarget origin must be an enemy id (got {id}, base {ENEMY_ID_BASE})"
        );
    }

    let hit_evt = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(35), |m| {
            matches!(m, ServerWorldMsg::Hit { target, .. } if *target == a_char_id as u64)
        })
        .await
        .expect("enemy fires a melee swing on the player");
    if let ServerWorldMsg::Hit { attacker, amount, dmg_type, .. } = hit_evt {
        assert!(
            attacker >= ENEMY_ID_BASE,
            "Hit attacker must be an enemy id (got {attacker}, base {ENEMY_ID_BASE})"
        );
        assert!(amount > 0, "enemy hit amount must be positive (got {amount})");
        assert!(
            matches!(dmg_type, DamageType::Physical),
            "enemy melee broadcasts as Physical damage (got {dmg_type:?})"
        );
    }
}

/// Track 5 sub-task 3: player â†’ server Attack intent, enemy HP authority,
/// death lifecycle.
///
/// The player walks into camp 0's aggro radius so the AI engages, waits
/// for an enemy-originated Hit broadcast (confirming the player and one
/// enemy are now within 1.2 Ã— melee_range of each other), then sends an
/// `Attack` with enough damage to one-shot the Decrepit Skeleton
/// (25 HP). Asserts:
///
///   * `HealthUpdate { id = enemy_id, hp = 0.0 }` arrives;
///   * `EntityDied { id = enemy_id }` arrives;
///   * `EntityDespawn { id = enemy_id }` arrives after the corpse linger
///     (ENEMY_DESPAWN_LINGER_SECS = 5 s server-side).
///
/// Respawn cadence (35 s default for camp 0) is covered by the
/// `death_notification_arms_respawn_timer` unit test in
/// `spawn_points.rs`; replicating it here would balloon the test
/// runtime past 35 s for marginal value.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn player_attack_kills_enemy_and_corpse_despawns() {
    let h = start_both().await;

    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "eta", "Etta", "Human", "Warrior").await;

    master_weapon_skill(&h.db_url, a_char_id, "hand_to_hand").await;
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;

    // Walk toward the Bonepile's isolated [-16, -14] spawn (one level 1
    // Decrepit Skeleton â€” see the shared phase 4 layout note above).
    // Phase 4 layout note: every camp-walking test aims at the Bonepile's
    // isolated [-16, 0, -14] spawn â€” the one ring 1 spawn whose aggro circle
    // overlaps no other, so exactly ONE slow (1.8 m/s, 2.5 s swing) level 1
    // Decrepit Skeleton pulls, the same single-puller semantics these tests
    // were written against. Do NOT aim at the Wolf Run: it is a four-wolf
    // pack, and standing in it turns every cast into interrupt rolls.
    // 3.5 s, not 2 s: at 2 s the player only clips camp 0's aggro radius, so
    // whether an enemy locks on before the wait expires depended on where it
    // happened to be wandering. Walking fully in makes the pull deterministic.
    // Walk in on the server's own word, not the wall clock (a slow test
    // thread used to come up short): to 2 m from the spawn point, well
    // inside its aggro whatever the spawn jitter.
    let mut seq: u32 = 1;
    walk_until_within(&mut a, &mut seq, a_char_id, Vec3 { x: 0.0, y: 0.0, z: 0.0 }, BONEPILE_SPAWN, 2.0).await;

    // Wait for an enemy hit on the player â€” proves the AI walked an
    // enemy into melee with us. The Hit carries the attacker id (in
    // the enemy partition). Generous timeout because parallel tests
    // in this file contend for CPU; isolated runtime is ~5 s.
    let hit_evt = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(35), |m| {
            matches!(m, ServerWorldMsg::Hit { target, .. } if *target == a_char_id as u64)
        })
        .await
        .expect("an enemy locks on and swings");
    let enemy_id: u64 = match hit_evt {
        ServerWorldMsg::Hit { attacker, .. } => attacker,
        _ => unreachable!(),
    };
    assert!(
        enemy_id >= ENEMY_ID_BASE,
        "attacker id must be an enemy id (got {enemy_id}, base {ENEMY_ID_BASE})"
    );

    // Track 6 sub-task 2: server runs the damage formula now â€” the
    // client can't claim 999 anymore. A bare-handed Human Warrior
    // lands ~5-8/swing (1-4 + STR/5 with STR 22). Decrepit Skeleton has
    // 25 HP, so ~5 landed swings cover the worst case.
    //
    // Swings must be PACED. This used to burst 10 Attacks in a single
    // frame, which the melee swing-rate limit (added 2026-07-29) now
    // correctly treats as forgery: it enforces a per-hand minimum
    // interval derived from the weapon's delay and the player's haste,
    // and silently drops anything faster. All but the first swing
    // vanished and the skeleton survived. Sleeping past the floor
    // between swings is what an honest client does anyway.
    // 1800 ms: since 2026-10-30 the limiter reads the player's own haste, so an
    // unhasted bare hand is held to 1.65 s (the 2.0 s fist delay minus the
    // jitter grace); swinging at the weapon's own delay is what an honest
    // client does, and a slow test thread only ever lengthens a gap. A 1000 ms
    // pace used to pass under the fully-hasted allowance everyone got, and a
    // 700 ms one before that left 50 ms of margin to jitter, where swings are
    // silently dropped and the skeleton survives. This test still has residual
    // enemy-AI timing sensitivity (see docs/flaky_integration_tests.md) â€” it
    // depends on an enemy wandering into range, locking on, and STAYING in melee.
    const SWING_GAP: Duration = Duration::from_millis(1800);
    for _ in 0..8 {
        a.send_attack(enemy_id, "", false, DamageType::Physical);
        // Pump the transport across the gap so the Attack actually goes
        // out (a bare sleep does not advance renet).
        let until = Instant::now() + SWING_GAP;
        while Instant::now() < until {
            tick_one(&mut a.client, &mut a.transport);
            tokio::time::sleep(TICK_DT).await;
        }
    }

    let hu = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(5), |m| {
            matches!(m, ServerWorldMsg::HealthUpdate { id, hp, .. }
                if *id == enemy_id && *hp <= 0.0)
        })
        .await
        .expect("HealthUpdate(hp=0) arrives for the killed enemy");
    if let ServerWorldMsg::HealthUpdate { hp, .. } = hu {
        assert!(hp <= 0.0, "killed enemy must broadcast hp <= 0 (got {hp})");
    }

    a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(5), |m| {
        matches!(m, ServerWorldMsg::EntityDied { id } if *id == enemy_id)
    })
    .await
    .expect("EntityDied arrives for the killed enemy");

    // Track 5 sub-task 5 â€” kill credit. Per-kill XP is the EQ quadratic
    // (mob_level^2 * ZEM * 3.5, see progression::kill_xp): a level-1
    // Decrepit Skeleton pays round(1 * 75 * 3.5) = 263. Sole attacker â†’
    // sole top damager â†’ private XpGained.
    let xp_evt = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(5), |m| {
            matches!(m, ServerWorldMsg::XpGained { .. })
        })
        .await
        .expect("XpGained arrives for the kill-credit recipient");
    if let ServerWorldMsg::XpGained { amount, .. } = xp_evt {
        assert_eq!(amount, 263, "level-1 mob kill pays kill_xp(1) = 263");
    }

    // ENEMY_DESPAWN_LINGER_SECS is 5 s; budget extra for tick jitter and
    // parallel contention.
    a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(15), |m| {
        matches!(m, ServerWorldMsg::EntityDespawn { id } if *id == enemy_id)
    })
    .await
    .expect("EntityDespawn arrives after the corpse linger window");
}

/// Seated-combat rule (classic EQ; user call 2026-10-14). A hit on a SITTING
/// player lands for the top of the attacker's damage band with no armor, and
/// any damage stands them. The stand is proven through `/camp`, the one
/// intent that needs the server's own seated flag: after the hit the camp is
/// refused outright, and after sitting again the same camp is accepted. Fails
/// on the previous code at the camp assertion (the server left the target
/// seated, so the camp was accepted).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_seated_player_is_hit_for_the_max_and_stood_up() {
    let h = start_both().await;
    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "seat_a", "Seata", "Human", "Warrior").await;
    let (b_session, b_char_id, b_token) =
        provision_client(&h.auth_url, "seat_b", "Seatb", "Human", "Warrior").await;
    master_weapon_skill(&h.db_url, a_char_id, "hand_to_hand").await;
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    let mut b = WorldClient::start(b_token, &b_session, b_char_id).await;
    pump_both_for(&mut a, &mut b, Duration::from_millis(300)).await;
    a.send_pvp_toggle(true);
    b.send_pvp_toggle(true);
    pump_both_for(&mut a, &mut b, Duration::from_millis(150)).await;

    // B sits beside A (both spawn on the same point); A swings bare fists,
    // which roll 1..=4 plus STR/5, so the seated max is 4 plus STR/5, times
    // A's mastered hand_to_hand (x1.15, combat.rs).
    let strength = char_data::compute("Human", "Warrior", 1).stats.strength;
    let seated_max = ((4 + strength / 5) as f32 * 1.15) as i32;
    b.send_sit();
    pump_both_for(&mut a, &mut b, Duration::from_millis(150)).await;
    a.send_attack(b_char_id as u64, "", false, DamageType::Physical);
    // Service A's transport too, or the swing never leaves its socket.
    pump_both_for(&mut a, &mut b, Duration::from_millis(100)).await;
    let hit = b
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::Hit { attacker, target, .. }
                if *attacker == a_char_id as u64 && *target == b_char_id as u64)
        })
        .await
        .expect("B is hit");
    let ServerWorldMsg::Hit { amount, crit, .. } = hit else { unreachable!() };
    if crit {
        assert!(
            amount >= seated_max,
            "a crit on a seated target lands at least the max ({seated_max}), got {amount}"
        );
    } else {
        assert_eq!(amount, seated_max, "a seated target takes the top of the band");
    }

    // The hit stood B: a /camp, which needs the server's seated flag, is
    // refused with no CampUpdate at all.
    b.send_camp();
    let refused = b
        .wait_for(CHANNEL_SYSTEM, Duration::from_millis(1200), |m| {
            matches!(m, ServerWorldMsg::CampUpdate { .. })
        })
        .await;
    assert!(
        refused.is_none(),
        "the hit stood the seated player, so /camp is refused; got {refused:?}"
    );

    // Control: seated again, the same /camp is accepted.
    b.send_sit();
    b.pump_for(Duration::from_millis(150)).await;
    b.send_camp();
    let accepted = b
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(2), |m| {
            matches!(m, ServerWorldMsg::CampUpdate { active: true, .. })
        })
        .await;
    assert!(accepted.is_some(), "seated again, /camp is accepted");
    a.send_disconnect();
    b.send_disconnect();
    a.pump_for(Duration::from_millis(200)).await;
    b.pump_for(Duration::from_millis(200)).await;
}

/// Client-authority audit fix 4: the server rolls the melee miss. A Rogue
/// swings the Carved Staff, two-handed blunt, a weapon type the class cannot
/// use (an 80% miss for half damage), at a dummy beside them. Across eight
/// swings the server answers with at least one Miss (eight straight landings
/// would be about one in 400,000). The old server landed every in-range swing:
/// only the client ever rolled a miss, and a modified client never did.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_server_rolls_the_melee_miss() {
    const STAFF: &str = "res://data/loot/items/carved_staff.tres";
    let h = start_both().await;
    let (a_session, a_char_id, _stale) =
        provision_client(&h.auth_url, "misser", "Missa", "Human", "Rogue").await;
    let pool = db::open(&h.db_url).await.expect("open pool");
    db::set_account_gm(&pool, "misser", true).await.expect("set is_gm");
    give_char_item(&h.db_url, a_char_id, "equip", 0, STAFF, 1).await;
    let a_token = request_world_token(&h.auth_url, &a_session, a_char_id).await;
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    a.pump_for(Duration::from_millis(300)).await;

    a.send_dev_spawn("Miss Dummy", 1, 100_000.0, 0, 0.0, 0.0);
    let spawn = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::EnemySpawn { mob_name, .. } if mob_name == "Miss Dummy")
        })
        .await
        .expect("the dummy spawns");
    let (dummy, dummy_pos) = match spawn {
        ServerWorldMsg::EnemySpawn { id, pos, .. } => (id, pos),
        _ => unreachable!(),
    };
    let mut seq: u32 = 1;
    walk_until_within(&mut a, &mut seq, a_char_id, Vec3 { x: 0.0, y: 0.0, z: 0.0 }, dummy_pos, 1.0).await;

    // Six swings at the staff's own 2.0 s delay (the limiter holds an
    // unhasted hand to 1.65 s): six straight landings at an 80% miss would be
    // about one in 16,000.
    let (mut misses, mut hits) = (0, 0);
    for _ in 0..6 {
        a.send_attack(dummy, "", false, DamageType::Physical);
        let until = Instant::now() + Duration::from_millis(1_800);
        while Instant::now() < until {
            tick_one(&mut a.client, &mut a.transport);
            while let Some(bytes) = a.client.receive_message(CHANNEL_SYSTEM) {
                match bincode::serde::decode_from_slice::<ServerWorldMsg, _>(&bytes, bincode_cfg()) {
                    Ok((ServerWorldMsg::Miss { attacker, target }, _))
                        if attacker == a_char_id as u64 && target == dummy => misses += 1,
                    Ok((ServerWorldMsg::Hit { attacker, target, .. }, _))
                        if attacker == a_char_id as u64 && target == dummy => hits += 1,
                    _ => {}
                }
            }
            tokio::time::sleep(TICK_DT).await;
        }
    }
    assert_eq!(misses + hits, 6, "every swing was answered: {misses} misses, {hits} hits");
    assert!(misses >= 1, "the server rolled at least one miss ({hits} hits)");
}

/// The swing-rate limit reads the player's real haste (the client-authority
/// audit's Phase 4, 2026-10-30): an Enchanter under their own Haste swings
/// bare-handed every 1.3 s and every swing is answered (2.0 s at 30% is 1.4 s,
/// and the gate sits a grace below it), while an unhasted Warrior swinging
/// every 0.9 s has every second swing dropped, held to the unhasted 1.65 s
/// (0.9 leaves the dropped swings 750 ms of margin against a slow test
/// thread, which only ever lengthens a gap; the accepted ones can only get
/// later). The previous server assumed everyone fully hasted (a 0.65 s gate)
/// and answered all four of the Warrior's; it fails here at the Warrior's
/// count.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_swing_rate_limit_reads_the_players_haste() {
    let h = start_both().await;
    let (e_session, e_char_id, _stale) =
        provision_client(&h.auth_url, "hasteench", "Hastea", "Human", "Enchanter").await;
    let (w_session, w_char_id, w_token) =
        provision_client(&h.auth_url, "hastewar", "Hasteb", "Human", "Warrior").await;
    set_char_level(&h.db_url, e_char_id, 16).await;
    let pool = db::open(&h.db_url).await.expect("open pool");
    db::set_account_gm(&pool, "hasteench", true).await.expect("set is_gm");
    let e_token = request_world_token(&h.auth_url, &e_session, e_char_id).await;
    let mut e = WorldClient::start(e_token, &e_session, e_char_id).await;
    let mut w = WorldClient::start(w_token, &w_session, w_char_id).await;
    pump_both_for(&mut e, &mut w, Duration::from_millis(300)).await;

    e.send_dev_spawn("Haste Dummy", 1, 100_000.0, 0, 0.0, 0.0);
    let spawn = wait_on_a_pumping_b(&mut e, &mut w, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::EnemySpawn { mob_name, .. } if mob_name == "Haste Dummy")
    })
    .await
    .expect("the dummy spawns");
    let (dummy, dummy_pos) = match spawn {
        ServerWorldMsg::EnemySpawn { id, pos, .. } => (id, pos),
        _ => unreachable!(),
    };
    let origin = Vec3 { x: 0.0, y: 0.0, z: 0.0 };
    let (mut se, mut sw) = (1u32, 1u32);
    walk_until_within(&mut e, &mut se, e_char_id, origin, dummy_pos, 1.0).await;
    walk_until_within(&mut w, &mut sw, w_char_id, origin, dummy_pos, 1.0).await;

    // The Enchanter hastes itself (an untargeted ALLY cast lands on the caster).
    e.send_cast_start("Haste", 1.5);
    pump_both_for(&mut e, &mut w, Duration::from_millis(1_700)).await;
    e.send_cast_spell("Haste", None);
    wait_on_a_pumping_b(&mut e, &mut w, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::BuffSnapshot { target, buffs }
            if *target == e_char_id as u64 && buffs.iter().any(|(n, _)| n == "Haste"))
    })
    .await
    .expect("the Enchanter is hasted");

    /// Swing `n` times at `gap`, servicing both sockets, and count the
    /// swings the server answered with a Hit or a Miss.
    async fn paced_swings(
        c: &mut WorldClient,
        other: &mut WorldClient,
        attacker: i64,
        target: u64,
        n: usize,
        gap: Duration,
    ) -> usize {
        let mut answered = 0;
        for _ in 0..n {
            c.send_attack(target, "", false, DamageType::Physical);
            let until = Instant::now() + gap;
            while Instant::now() < until {
                tick_one(&mut c.client, &mut c.transport);
                tick_one(&mut other.client, &mut other.transport);
                while let Some(bytes) = c.client.receive_message(CHANNEL_SYSTEM) {
                    if let Ok((msg, _)) = bincode::serde::decode_from_slice::<ServerWorldMsg, _>(&bytes, bincode_cfg()) {
                        if matches!(msg, ServerWorldMsg::Hit { attacker: a, target: t, .. }
                            | ServerWorldMsg::Miss { attacker: a, target: t }
                            if a == attacker as u64 && t == target)
                        {
                            answered += 1;
                        }
                    }
                }
                tokio::time::sleep(TICK_DT).await;
            }
        }
        answered
    }

    let hasted = paced_swings(&mut e, &mut w, e_char_id, dummy, 4, Duration::from_millis(1_300)).await;
    assert_eq!(hasted, 4, "the hasted Enchanter's four swings at 1.3 s are all answered");
    let unhasted = paced_swings(&mut w, &mut e, w_char_id, dummy, 4, Duration::from_millis(900)).await;
    assert_eq!(unhasted, 2, "the unhasted Warrior at 0.9 s: every second swing is dropped");
}

/// Dual wield is earned (user, 2026-10-19): Monks are born with it (bare
/// fists in both hands), Warriors get it at 15. A Warrior 1 with a sword in
/// each hand: the main hand is answered, the off hand is not; leveled past 15
/// (the GM XP tool), the off hand is answered. A bare-handed Monk 1's off hand
/// is answered. On the previous code any class's off hand swung from level 1,
/// and a Monk's empty off hand was refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dual_wield_is_earned_by_class_and_level() {
    const SWORD: &str = "res://data/loot/items/iron_short_sword.tres";
    let h = start_both().await;
    let (a_session, a_char_id, _stale) =
        provision_client(&h.auth_url, "dwwarrior", "Duala", "Human", "Warrior").await;
    let (b_session, b_char_id, b_token) =
        provision_client(&h.auth_url, "dwmonk", "Dualb", "Human", "Monk").await;
    let pool = db::open(&h.db_url).await.expect("open pool");
    db::set_account_gm(&pool, "dwwarrior", true).await.expect("set is_gm");
    give_char_item(&h.db_url, a_char_id, "equip", 0, SWORD, 1).await;
    give_char_item(&h.db_url, a_char_id, "equip", 1, SWORD, 1).await;
    let a_token = request_world_token(&h.auth_url, &a_session, a_char_id).await;
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    let mut b = WorldClient::start(b_token, &b_session, b_char_id).await;
    pump_both_for(&mut a, &mut b, Duration::from_millis(300)).await;

    a.send_dev_spawn("Dual Dummy", 1, 100_000.0, 0, 0.0, 0.0);
    let spawn = wait_on_a_pumping_b(&mut a, &mut b, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::EnemySpawn { mob_name, .. } if mob_name == "Dual Dummy")
    })
    .await
    .expect("the dummy spawns");
    let (dummy, dummy_pos) = match spawn {
        ServerWorldMsg::EnemySpawn { id, pos, .. } => (id, pos),
        _ => unreachable!(),
    };
    let origin = Vec3 { x: 0.0, y: 0.0, z: 0.0 };
    let (mut sa, mut sb) = (1u32, 1u32);
    walk_until_within(&mut a, &mut sa, a_char_id, origin, dummy_pos, 1.0).await;
    walk_until_within(&mut b, &mut sb, b_char_id, origin, dummy_pos, 1.0).await;
    b.send_heartbeat();

    assert!(swing_answered(&mut a, a_char_id, dummy, false).await, "the Warrior's main hand swings");
    assert!(!swing_answered(&mut a, a_char_id, dummy, true).await, "a Warrior 1's off hand does not");
    assert!(swing_answered(&mut b, b_char_id, dummy, true).await, "a bare-handed Monk 1's off hand punches");

    a.send_grant_quest_xp(20_000_000);
    let leveled = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::LevelUp { new_level, .. } if *new_level >= 15)
        })
        .await;
    assert!(leveled.is_some(), "the GM XP grant takes the Warrior past 15");
    a.pump_for(Duration::from_millis(1_100)).await;
    assert!(swing_answered(&mut a, a_char_id, dummy, true).await, "past 15, the Warrior's off hand swings");
}

/// EQ's equip rule (2026-10-27, after a Cleric wore two clubs on the R720):
/// the item refuses the class at the equip. A Cleric's club into the off hand
/// is refused with "Your class cannot dual wield." and the paperdoll never
/// changes; the same club into the main hand is worn; the Cleric's sword
/// (1h_slashing, a skill Clerics never train) is refused as unwieldable; a
/// Monk's club goes into the off hand (born with dual wield) while a leather
/// vest is refused as unwearable (Monks train cloth alone). On the previous
/// server every one of these went on; the swing gate alone kept the off hand
/// idle.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn equipping_is_refused_by_class_and_dual_wield() {
    const CLUB: &str = "res://data/loot/items/cracked_wooden_club.tres";
    const SWORD: &str = "res://data/loot/items/iron_short_sword.tres";
    const VEST: &str = "res://data/loot/items/cracked_leather_vest.tres";
    let h = start_both().await;
    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "eqcleric", "Equipa", "Human", "Cleric").await;
    let (b_session, b_char_id, b_token) =
        provision_client(&h.auth_url, "eqmonk", "Equipb", "Human", "Monk").await;
    give_char_item(&h.db_url, a_char_id, "base", 0, CLUB, 1).await;
    give_char_item(&h.db_url, a_char_id, "base", 1, SWORD, 1).await;
    give_char_item(&h.db_url, b_char_id, "base", 0, CLUB, 1).await;
    give_char_item(&h.db_url, b_char_id, "base", 1, VEST, 1).await;
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    let mut b = WorldClient::start(b_token, &b_session, b_char_id).await;
    pump_both_for(&mut a, &mut b, Duration::from_millis(300)).await;

    // The Cleric's club into the off hand: the line, and no paperdoll change.
    a.send_equip_item("base", 0, 1);
    let line = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::ChatMessage { text, .. } if text.contains("cannot dual wield"))
        })
        .await;
    assert!(line.is_some(), "a Cleric's off-hand weapon is refused with the dual wield line");
    assert!(
        next_equip_delta(&mut a, 1, Duration::from_millis(500)).await.is_none(),
        "the refused club never reaches the off hand"
    );

    // The same club into the main hand: worn (Clerics train 1h_blunt).
    a.send_equip_item("base", 0, 0);
    let worn = next_equip_delta(&mut a, 0, Duration::from_secs(3)).await;
    assert_eq!(worn, Some(Some(CLUB.to_string())), "the Cleric's club goes into the main hand");

    // The sword: a skill the class never trains, refused at the equip.
    a.send_equip_item("base", 1, 0);
    let line = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::ChatMessage { text, .. } if text.contains("cannot wield"))
        })
        .await;
    assert!(line.is_some(), "a Cleric's sword is refused as unwieldable");
    assert!(
        next_equip_delta(&mut a, 0, Duration::from_millis(500)).await.is_none(),
        "the sword never replaces the club"
    );

    // The Monk: the club into the off hand goes on; the leather vest does not.
    b.send_equip_item("base", 0, 1);
    let worn = next_equip_delta(&mut b, 1, Duration::from_secs(3)).await;
    assert_eq!(worn, Some(Some(CLUB.to_string())), "a Monk 1's club goes into the off hand");
    b.send_equip_item("base", 1, 3);
    let line = b
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::ChatMessage { text, .. } if text.contains("cannot wear"))
        })
        .await;
    assert!(line.is_some(), "a Monk's leather vest is refused as unwearable");
    assert!(
        next_equip_delta(&mut b, 3, Duration::from_millis(500)).await.is_none(),
        "the vest never reaches the chest slot"
    );
}

/// EQ's two-hander rule (user, 2026-10-29: a two-hander beside an off-hand
/// weapon is an end-game Warrior ability; nobody until then). A Warrior 15
/// (dual wield earned) wearing a club in the off hand is refused a Carved
/// Staff for the main hand ("Empty your off hand first."); with the club off
/// and the staff on, the club is refused for the off hand ("You cannot hold
/// that with a two-handed weapon."). The swing backstop, for gear worn before
/// the rule: a second Warrior 15 seeded wearing the staff AND a club swings
/// the staff and gets no answer for the off hand. On the previous server the
/// staff went on beside the club (both staves lacked the two-handed flag)
/// and the off hand swung beside it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_two_hander_needs_an_empty_off_hand() {
    const STAFF: &str = "res://data/loot/items/carved_staff.tres";
    const CLUB: &str = "res://data/loot/items/cracked_wooden_club.tres";
    let h = start_both().await;
    let (a_session, a_char_id, _stale_a) =
        provision_client(&h.auth_url, "twohand_a", "Staffa", "Human", "Warrior").await;
    let (b_session, b_char_id, _stale_b) =
        provision_client(&h.auth_url, "twohand_b", "Staffb", "Human", "Warrior").await;
    let pool = db::open(&h.db_url).await.expect("open pool");
    db::set_account_gm(&pool, "twohand_a", true).await.expect("set is_gm");
    set_char_level(&h.db_url, a_char_id, 15).await;
    set_char_level(&h.db_url, b_char_id, 15).await;
    give_char_item(&h.db_url, a_char_id, "base", 0, STAFF, 1).await;
    give_char_item(&h.db_url, a_char_id, "base", 1, CLUB, 1).await;
    // Gear from before the rule: the pairing the equip now refuses.
    give_char_item(&h.db_url, b_char_id, "equip", 0, STAFF, 1).await;
    give_char_item(&h.db_url, b_char_id, "equip", 1, CLUB, 1).await;
    let a_token = request_world_token(&h.auth_url, &a_session, a_char_id).await;
    let b_token = request_world_token(&h.auth_url, &b_session, b_char_id).await;
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    let mut b = WorldClient::start(b_token, &b_session, b_char_id).await;
    pump_both_for(&mut a, &mut b, Duration::from_millis(300)).await;

    // The club into the off hand: worn (a Warrior 15 dual wields).
    a.send_equip_item("base", 1, 1);
    assert_eq!(
        next_equip_delta(&mut a, 1, Duration::from_secs(3)).await,
        Some(Some(CLUB.to_string())),
        "the club goes into the off hand"
    );
    // The staff into the main hand beside it: refused, no paperdoll change.
    a.send_equip_item("base", 0, 0);
    let line = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::ChatMessage { text, .. } if text == "Empty your off hand first.")
        })
        .await;
    assert!(line.is_some(), "a two-hander is refused beside an off-hand weapon");
    assert!(
        next_equip_delta(&mut a, 0, Duration::from_millis(500)).await.is_none(),
        "the staff never reaches the main hand"
    );
    // Off hand emptied, the staff goes on; then the club is refused beside it.
    a.send_unequip_item(1, "base", 1);
    assert_eq!(next_equip_delta(&mut a, 1, Duration::from_secs(3)).await, Some(None), "the off hand empties");
    a.send_equip_item("base", 0, 0);
    assert_eq!(
        next_equip_delta(&mut a, 0, Duration::from_secs(3)).await,
        Some(Some(STAFF.to_string())),
        "the staff goes into the empty-off-hand main hand"
    );
    a.send_equip_item("base", 1, 1);
    let line = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::ChatMessage { text, .. } if text == "You cannot hold that with a two-handed weapon.")
        })
        .await;
    assert!(line.is_some(), "nothing joins a two-hander");
    assert!(
        next_equip_delta(&mut a, 1, Duration::from_millis(500)).await.is_none(),
        "the club never reaches the off hand"
    );

    // The swing backstop for gear worn before the rule.
    a.send_dev_spawn("Staff Dummy", 1, 100_000.0, 0, 0.0, 0.0);
    let spawn = wait_on_a_pumping_b(&mut a, &mut b, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::EnemySpawn { mob_name, .. } if mob_name == "Staff Dummy")
    })
    .await
    .expect("the dummy spawns");
    let (dummy, dummy_pos) = match spawn {
        ServerWorldMsg::EnemySpawn { id, pos, .. } => (id, pos),
        _ => unreachable!(),
    };
    let origin = Vec3 { x: 0.0, y: 0.0, z: 0.0 };
    let mut sb = 1u32;
    walk_until_within(&mut b, &mut sb, b_char_id, origin, dummy_pos, 1.0).await;
    a.send_heartbeat();
    assert!(swing_answered(&mut b, b_char_id, dummy, false).await, "the staff swings");
    assert!(!swing_answered(&mut b, b_char_id, dummy, true).await, "the off hand beside it is unanswered");
}

/// Shield Bash: the mob's frame says Stunned and it swings at nobody for the
/// 3 s, then swings again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shield_bash_holds_the_mob_for_three_seconds() {
    let h = start_both().await;
    let (mut a, a_id, dummy) = gm_beside_dummy(&h, "basher", "Warrior", "Bash Dummy", 1, 5.0).await;
    let first = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(6), |m| matches!(m, ServerWorldMsg::Hit { attacker, target, .. } if *attacker == dummy && *target == a_id as u64))
        .await;
    assert!(first.is_some(), "the dummy is swinging at the warrior");
    // One window of 2.6 s from the send, inside the 3 s stun: the bash lands,
    // the frame says Stunned, and nothing comes back from the mob after the
    // landing. A Miss (the 5% floor) waits out the cooldown and tries again.
    let mut landed_at: Option<usize> = None;
    let mut window: Vec<ServerWorldMsg> = Vec::new();
    for _ in 0..3 {
        a.send_use_skill("Shield Bash", Some(dummy));
        window = a.collect_for(Duration::from_millis(2_600)).await;
        if let Some(i) = window.iter().position(|m| matches!(m, ServerWorldMsg::Hit { attacker, target, .. } if *attacker == a_id as u64 && *target == dummy)) {
            landed_at = Some(i);
            break;
        }
        if let Some(ServerWorldMsg::SkillFail { reason, .. }) = window.iter().find(|m| matches!(m, ServerWorldMsg::SkillFail { .. })) {
            panic!("Shield Bash refused: {reason}");
        }
        a.pump_alive_for(Duration::from_millis(6_000)).await;
    }
    let landed_at = landed_at.expect("three Shield Bashes in a row cannot all miss at 5%");
    assert!(snapshot_names(&window[landed_at..], dummy, "Stunned"), "the mob's frame says Stunned");
    assert_eq!(mob_hits_on(&window[landed_at..], dummy, a_id), 0, "no swing while stunned");
    let after = a.collect_for(Duration::from_millis(4_000)).await;
    assert!(mob_hits_on(&after, dummy, a_id) >= 1, "it swings again once the stun runs out");
}

/// Feign Death: the roll runs on the server and its outcome is told, and the
/// 20 stamina is charged either way. A failure changes nothing (the mob keeps
/// swinging). A success drops the monk from the mob, which does not notice
/// them lying there until they move. The roll is 80%, so fresh monks try
/// until one succeeds: six in a row failing is one run in fifteen thousand,
/// and every attempt checks its own outcome.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn feign_death_is_rolled_told_and_held_until_a_move() {
    let h = start_both().await;
    let mut succeeded = false;
    // Letters only: character names take no digits.
    for user in ["feigna", "feignb", "feignc", "feignd", "feigne", "feignf"] {
        let (mut a, a_id, dummy) = gm_beside_dummy(&h, user, "Monk", "Fd Dummy", 1, 5.0).await;
        let first = a
            .wait_for(CHANNEL_SYSTEM, Duration::from_secs(6), |m| matches!(m, ServerWorldMsg::Hit { attacker, target, .. } if *attacker == dummy && *target == a_id as u64))
            .await;
        assert!(first.is_some(), "the dummy is swinging at the monk");
        a.send_use_skill("Feign Death", None);
        let msgs = a.collect_for(Duration::from_millis(1_500)).await;
        let line = msgs.iter().find_map(|m| match m {
            ServerWorldMsg::ChatMessage { text, .. } if text.contains("feign death") => Some(text.clone()),
            _ => None,
        });
        let line = line.expect("the outcome is told");
        assert!(msgs.iter().any(|m| matches!(m, ServerWorldMsg::StaminaUpdate { id, stamina, max } if *id == a_id as u64 && (max - stamina - 20.0).abs() < 0.5)),
            "the attempt costs 20 stamina either way");
        if line == "You feign death successfully." {
            assert!(msgs.iter().any(|m| matches!(m, ServerWorldMsg::EntityTarget { id, target: None } if *id == dummy)), "the mob drops the monk");
            assert!(snapshot_names(&msgs, a_id as u64, "Feigned Death"));
            let quiet = a.collect_for(Duration::from_millis(3_000)).await;
            assert_eq!(mob_hits_on(&quiet, dummy, a_id), 0, "lying still beside it, unnoticed");
            a.send_move(10_000, Vec3 { x: 1.0, y: 0.0, z: 0.0 });
            let noticed = a
                .wait_for(CHANNEL_SYSTEM, Duration::from_secs(2), |m| matches!(m, ServerWorldMsg::EntityTarget { id, target: Some(t) } if *id == dummy && *t == a_id as u64))
                .await;
            assert!(noticed.is_some(), "standing up, the monk is noticed again");
            succeeded = true;
        } else {
            assert_eq!(line, "Your feign death fails!");
            let after = a.collect_for(Duration::from_millis(4_000)).await;
            assert!(mob_hits_on(&after, dummy, a_id) >= 1, "a failed feign changes nothing");
        }
        a.send_disconnect();
        a.pump_for(Duration::from_millis(300)).await;
        if succeeded {
            break;
        }
    }
    assert!(succeeded, "six feigns in a row failed at 80%: the roll is wrong");
}

/// Hide: an idle mob spawned with the hidden rogue inside its aggro radius
/// does not notice them; the rogue's first swing ends the hide and the mob
/// turns. A second hide (after its cooldown) ends on the first step, and a
/// fresh idle mob notices the rogue then; a hide while hidden is refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hide_keeps_an_idle_mob_from_noticing_until_a_swing_or_a_step() {
    let h = start_both().await;
    let (session, a_id, _stale) = provision_client(&h.auth_url, "sneak", "Sneaka", "Human", "Rogue").await;
    let pool = db::open(&h.db_url).await.expect("open pool");
    db::set_account_gm(&pool, "sneak", true).await.expect("set is_gm");
    let token = request_world_token(&h.auth_url, &session, a_id).await;
    let mut a = WorldClient::start(token, &session, a_id).await;
    a.pump_for(Duration::from_millis(300)).await;

    a.send_use_skill("Hide", None);
    let told = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(2), |m| matches!(m, ServerWorldMsg::ChatMessage { text, .. } if text == "You have hidden yourself from view."))
        .await;
    assert!(told.is_some());
    // The cooldown (10 s) answers first for a second Hide straight after.
    assert_eq!(skill_answer(&mut a, a_id, "Hide", 0).await, Err("You cannot use that yet.".into()));
    // Aggro 5 m, spawned 3 m ahead of a hidden rogue: idle, unnoticing.
    let (dummy, _) = spawn_dummy(&mut a, "Watch Dummy", 0, 5.0).await;
    let quiet = a.collect_for(Duration::from_millis(1_500)).await;
    assert!(!quiet.iter().any(|m| matches!(m, ServerWorldMsg::EntityTarget { id, target: Some(_) } if *id == dummy)),
        "inside its radius, hidden: unnoticed");
    a.send_attack(dummy, "", false, DamageType::Physical);
    let msgs = a.collect_for(Duration::from_millis(2_000)).await;
    assert!(msgs.iter().any(|m| matches!(m, ServerWorldMsg::EntityTarget { id, target: Some(t) } if *id == dummy && *t == a_id as u64)),
        "the swing ends the hide and the mob turns");
    assert!(msgs.iter().any(|m| matches!(m, ServerWorldMsg::BuffSnapshot { target, buffs } if *target == a_id as u64 && !buffs.iter().any(|(n, _)| n == "Hide"))),
        "the snapshot without Hide fans");

    // Hide again once its 10 s cooldown has run. With the cooldown over and
    // the hide (30 s) still holding, a third Hide is refused as pointless;
    // then a step ends it.
    a.pump_alive_for(Duration::from_millis(7_000)).await;
    a.send_use_skill("Hide", None);
    let told = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(2), |m| matches!(m, ServerWorldMsg::ChatMessage { text, .. } if text == "You have hidden yourself from view."))
        .await;
    assert!(told.is_some(), "hidden again after the cooldown");
    a.pump_alive_for(Duration::from_millis(10_300)).await;
    assert_eq!(skill_answer(&mut a, a_id, "Hide", 0).await, Err("You are already hidden.".into()));
    let (watcher, _) = spawn_dummy(&mut a, "Second Watcher", 0, 5.0).await;
    let quiet = a.collect_for(Duration::from_millis(1_000)).await;
    assert!(!quiet.iter().any(|m| matches!(m, ServerWorldMsg::EntityTarget { id, target: Some(_) } if *id == watcher)),
        "still unnoticed while standing still");
    a.send_move(10_000, Vec3 { x: 1.0, y: 0.0, z: 0.0 });
    let noticed = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(2), |m| matches!(m, ServerWorldMsg::EntityTarget { id, target: Some(t) } if *id == watcher && *t == a_id as u64))
        .await;
    assert!(noticed.is_some(), "a step ends the hide and the watcher notices");
}

/// Evade: for its 5 s every swing at the rogue fans as an Evade and lands
/// nothing; afterwards the swings land again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn evade_deflects_every_swing_while_it_holds() {
    let h = start_both().await;
    let (mut a, a_id, dummy) = gm_beside_dummy(&h, "dodger", "Rogue", "Evade Dummy", 1, 5.0).await;
    let first = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(6), |m| matches!(m, ServerWorldMsg::Hit { attacker, target, .. } if *attacker == dummy && *target == a_id as u64))
        .await;
    assert!(first.is_some());
    a.send_use_skill("Evade", None);
    let window = a.collect_for(Duration::from_millis(4_500)).await;
    let evades = window.iter().filter(|m| matches!(m, ServerWorldMsg::Evade { attacker, target } if *attacker == dummy && *target == a_id as u64)).count();
    assert!(evades >= 1, "a 2.5 s swing inside 4.5 s was deflected");
    assert_eq!(mob_hits_on(&window, dummy, a_id), 0, "nothing lands while Evade holds");
    assert!(snapshot_names(&window, a_id as u64, "Evade"));
    a.pump_for(Duration::from_millis(1_000)).await;
    let after = a.collect_for(Duration::from_millis(4_000)).await;
    assert!(mob_hits_on(&after, dummy, a_id) >= 1, "once it runs out the swings land again");
}

/// Holy Shield: a pool of 60 takes the next swings (fanned as hits of 0) and
/// the paladin's HP does not move.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn holy_shield_absorbs_the_next_swings() {
    let h = start_both().await;
    let (mut a, a_id, dummy) = gm_beside_dummy(&h, "warded", "Paladin", "Ward Dummy", 1, 5.0).await;
    let first = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(6), |m| matches!(m, ServerWorldMsg::HealthUpdate { id, .. } if *id == a_id as u64))
        .await;
    let Some(ServerWorldMsg::HealthUpdate { hp: hp_before, .. }) = first else { panic!("the first swing lands") };
    a.send_use_skill("Holy Shield", None);
    let window = a.collect_for(Duration::from_millis(6_000)).await;
    assert!(snapshot_names(&window, a_id as u64, "Holy Shield"));
    let zero_hits = window.iter().filter(|m| matches!(m, ServerWorldMsg::Hit { attacker, target, amount, .. } if *attacker == dummy && *target == a_id as u64 && *amount == 0)).count();
    assert!(zero_hits >= 2, "two swings in 6 s, both absorbed: {zero_hits}");
    for m in &window {
        if let ServerWorldMsg::HealthUpdate { id, hp, .. } = m {
            if *id == a_id as u64 {
                assert!(*hp >= hp_before, "HP never drops behind the shield: {hp} < {hp_before}");
            }
        }
    }
}
