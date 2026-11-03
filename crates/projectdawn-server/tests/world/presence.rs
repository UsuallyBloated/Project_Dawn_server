//! Who sees what: entering the world, the area-of-interest grid, positions,
//! movement and its speed, jumps, facing, say range, chat, the world clock, inspect, and
//! the fan-outs every peer receives.

use crate::common::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_clients_see_each_other() {
    let h = start_both().await;

    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "alpha", "Alphacha", "Human", "Warrior").await;
    let (b_session, b_char_id, b_token) =
        provision_client(&h.auth_url, "beta", "Betacha", "Elf", "Cleric").await;

    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    let mut b = WorldClient::start(b_token, &b_session, b_char_id).await;

    // â”€â”€ 1. EntitySpawn fan-out â”€â”€
    // B was newly-connected; the server should have sent A an EntitySpawn for B.
    // A also gets EntitySpawn for itself? No â€” fan-out skips the subject;
    // A's own ConnectOk is the own-self signal. So only spawn(B) at A.
    let spawn_b_at_a = a
        .wait_for(
            CHANNEL_SYSTEM,
            Duration::from_secs(3),
            |m| matches!(m, ServerWorldMsg::EntitySpawn { id, .. } if *id == b_char_id as u64),
        )
        .await
        .expect("A receives EntitySpawn for B");
    if let ServerWorldMsg::EntitySpawn {
        name,
        race,
        class,
        level,
        ..
    } = &spawn_b_at_a
    {
        assert_eq!(name, "Betacha", "B's name in spawn");
        assert_eq!(race, "Elf", "B's race in spawn");
        assert_eq!(class, "Cleric", "B's class in spawn");
        assert_eq!(*level, 1, "B's level in spawn (newly-created character)");
    }

    let spawn_a_at_b = b
        .wait_for(
            CHANNEL_SYSTEM,
            Duration::from_secs(3),
            |m| matches!(m, ServerWorldMsg::EntitySpawn { id, .. } if *id == a_char_id as u64),
        )
        .await
        .expect("B receives EntitySpawn for A");
    if let ServerWorldMsg::EntitySpawn {
        name, race, class, ..
    } = &spawn_a_at_b
    {
        assert_eq!(name, "Alphacha");
        assert_eq!(race, "Human");
        assert_eq!(class, "Warrior");
    }

    // â”€â”€ 2. Position fan-out â”€â”€
    // Each client moves; both should see Positions for both ids.
    a.send_move(
        1,
        Vec3 {
            x: 1.0,
            y: 0.0,
            z: 0.0,
        },
    );
    b.send_move(
        1,
        Vec3 {
            x: 0.0,
            y: 0.0,
            z: 1.0,
        },
    );

    let _pos_b_at_a = a
        .wait_for(
            CHANNEL_POSITION,
            Duration::from_secs(3),
            |m| matches!(m, ServerWorldMsg::Position { id, .. } if *id == b_char_id as u64),
        )
        .await
        .expect("A sees a Position broadcast for B");

    let _pos_a_at_b = b
        .wait_for(
            CHANNEL_POSITION,
            Duration::from_secs(3),
            |m| matches!(m, ServerWorldMsg::Position { id, .. } if *id == a_char_id as u64),
        )
        .await
        .expect("B sees a Position broadcast for A");

    // â”€â”€ 3. EntityDespawn on disconnect â”€â”€
    // B leaves. A should see EntityDespawn(B.char_id) within a tick or two.
    b.send_disconnect();
    // Pump B's transport so the Disconnect message reaches the wire before
    // the socket goes out of scope.
    for _ in 0..6 {
        b.client.update(TICK_DT);
        if b.transport.update(TICK_DT, &mut b.client).is_err() {
            break;
        }
        if b.transport.send_packets(&mut b.client).is_err() {
            break;
        }
        tokio::time::sleep(TICK_DT).await;
    }

    a.wait_for(
        CHANNEL_SYSTEM,
        Duration::from_secs(3),
        |m| matches!(m, ServerWorldMsg::EntityDespawn { id } if *id == b_char_id as u64),
    )
    .await
    .expect("A receives EntityDespawn for B");
}

/// Track 6 sub-task 1: server-authoritative resources. The server loads
/// HP/MP/Stamina from the DB at `CharacterSpawn` and fans
/// HealthUpdate/ManaUpdate/StaminaUpdate to in-world peers at the step-4a
/// EnterWorld seed (and continuously on regen-tick threshold crossings).
/// `create_character` seeds each new character with hp=mp=stamina=100; the
/// test asserts B sees those values for A via the seed path.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_clients_server_authoritative_resources() {
    let h = start_both().await;

    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "gamma", "Gam", "Human", "Warrior").await;
    let (b_session, b_char_id, b_token) =
        provision_client(&h.auth_url, "delta", "Del", "Elf", "Cleric").await;

    // A connects first; B joins second. The step-4a seed loop runs for
    // each new joiner: when B sends EnterWorld, the server replays every
    // existing peer's EntitySpawn + resources to B. So B should observe
    // A's HealthUpdate / ManaUpdate / StaminaUpdate at the seed step,
    // even though nothing else has happened in the world.
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    let mut b = WorldClient::start(b_token, &b_session, b_char_id).await;

    // Pump A a few times so the server's step-4a seed broadcast can land
    // (this includes the resources from the DB).
    for _ in 0..6 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }

    // A is a Human Warrior, so char_data::compute gives EQ's pools (the
    // parity audit, 2026-10-20): max_hp 28 (5 + 220 / 10 + CON 20 x 220 /
    // 3000), max_mp the token 20 (no mana stat), max_stamina 120 (BASE_ST
    // 100 + Warrior 20). `create_character` seeds current = max.
    let h_at_b = b
        .wait_for(
            CHANNEL_SYSTEM,
            Duration::from_secs(3),
            |m| matches!(m, ServerWorldMsg::HealthUpdate { id, .. } if *id == a_char_id as u64),
        )
        .await
        .expect("B receives HealthUpdate for A via DB-seeded fan-out");
    if let ServerWorldMsg::HealthUpdate { hp, max_hp, .. } = h_at_b {
        assert!((hp - 28.0).abs() < 0.01, "Warrior hp: {hp}");
        assert!((max_hp - 28.0).abs() < 0.01, "Warrior max_hp: {max_hp}");
    }

    let m_at_b = b
        .wait_for(
            CHANNEL_SYSTEM,
            Duration::from_secs(3),
            |m| matches!(m, ServerWorldMsg::ManaUpdate { id, .. } if *id == a_char_id as u64),
        )
        .await
        .expect("B receives ManaUpdate for A via DB-seeded fan-out");
    if let ServerWorldMsg::ManaUpdate { mp, max_mp, .. } = m_at_b {
        assert!((mp - 20.0).abs() < 0.01, "Warrior mp: {mp}");
        assert!((max_mp - 20.0).abs() < 0.01, "Warrior max_mp: {max_mp}");
    }

    let s_at_b = b
        .wait_for(
            CHANNEL_SYSTEM,
            Duration::from_secs(3),
            |m| matches!(m, ServerWorldMsg::StaminaUpdate { id, .. } if *id == a_char_id as u64),
        )
        .await
        .expect("B receives StaminaUpdate for A via DB-seeded fan-out");
    if let ServerWorldMsg::StaminaUpdate { stamina, max, .. } = s_at_b {
        assert!((stamina - 120.0).abs() < 0.01, "Warrior stamina: {stamina}");
        assert!((max - 120.0).abs() < 0.01, "Warrior max stamina: {max}");
    }
}

/// Cast lifecycle replication, carrying the server's numbers (Track 4 sub-task
/// 2, then the client-authority audit's Phase 4 on 2026-10-30). A broadcasts
/// CastStart then CastComplete for Ice Spear with a forged 2.5 s bar; B sees
/// both carrying A's id, the spell's name and the server's own 1.0 s cast
/// time, never the 2.5. A CastStart or CastComplete naming a spell the server
/// does not know reaches nobody, and a cancelled cast's CastFail carries the
/// fixed word, not the string the client wrote. The caster gets no echo. On
/// the previous server B's bar read 2.5, the made-up spell was relayed under
/// its made-up name and the forged reason went out as written.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_clients_cast_fanout() {
    let h = start_both().await;

    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "epsilon", "Eps", "Human", "Wizard").await;
    let (b_session, b_char_id, b_token) =
        provision_client(&h.auth_url, "zeta", "Zet", "Elf", "Cleric").await;

    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    let mut b = WorldClient::start(b_token, &b_session, b_char_id).await;

    a.send_cast_start("Ice Spear", 2.5);
    for _ in 0..4 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }

    let cast_at_b = b
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::CastStart { caster, .. } if *caster == a_char_id as u64)
        })
        .await
        .expect("B receives CastStart for A");
    if let ServerWorldMsg::CastStart {
        spell_name,
        duration,
        ..
    } = cast_at_b
    {
        assert_eq!(spell_name, "Ice Spear");
        assert!(
            (duration - 1.0).abs() < 0.01,
            "the server's cast time, not the client's 2.5: {duration}"
        );
    }

    a.send_cast_complete("Ice Spear");
    for _ in 0..4 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }

    let complete_at_b = b
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::CastComplete { caster, .. } if *caster == a_char_id as u64)
        })
        .await
        .expect("B receives CastComplete for A");
    if let ServerWorldMsg::CastComplete { spell_name, .. } = complete_at_b {
        assert_eq!(spell_name, "Ice Spear");
    }

    // Owner should not receive own cast events back.
    let echoed = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_millis(500), |m| {
            matches!(m, ServerWorldMsg::CastStart { caster, .. } if *caster == a_char_id as u64)
                || matches!(m, ServerWorldMsg::CastComplete { caster, .. } if *caster == a_char_id as u64)
        })
        .await;
    assert!(echoed.is_none(), "A should not receive own cast events");

    // A spell the server does not know is relayed under no name at all, and
    // a cancel carries the fixed word.
    a.send_cast_start("Totally Made Up <script>", 5.0);
    a.send_cast_complete("Totally Made Up <script>");
    a.send_cast_start("Ice Spear", 1.0);
    a.send_cast_fail("<forged reason on every screen>");
    a.pump_for(Duration::from_millis(200)).await;
    let seen = b.collect_for(Duration::from_millis(600)).await;
    assert!(
        !seen.iter().any(|m| matches!(m,
            ServerWorldMsg::CastStart { spell_name, .. } | ServerWorldMsg::CastComplete { spell_name, .. }
                if spell_name.contains("Made Up"))),
        "the made-up spell reaches nobody: {seen:?}"
    );
    let fail = seen.iter().find(
        |m| matches!(m, ServerWorldMsg::CastFail { caster, .. } if *caster == a_char_id as u64),
    );
    assert!(
        matches!(fail, Some(ServerWorldMsg::CastFail { reason, .. }) if reason == "cancelled"),
        "the cancel carries the fixed word, not the client's: {fail:?}"
    );
}

/// A player entering the world in the middle of a peer's cast gets the bar
/// with the time that is left, read off the server's own cast time (the
/// late-joiner seed), not the client's. Gate is 5 s on the server; A's client
/// claims 9 s, so a seed built from the client's number would run past 5.
/// The 2026-11-01 sitting could not tell the seed from the ordinary relay;
/// this settles the server half.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_late_joiner_sees_a_cast_already_under_way() {
    let h = start_both().await;

    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "latecaster", "Lca", "Human", "Wizard").await;
    let (b_session, b_char_id, b_token) =
        provision_client(&h.auth_url, "latejoiner", "Ljo", "Elf", "Cleric").await;

    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    a.send_cast_start("Gate", 9.0);
    let cast_began = std::time::Instant::now();
    a.pump_for(Duration::from_millis(1200)).await;

    let mut b = WorldClient::start(b_token, &b_session, b_char_id).await;
    let seed = b
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::CastStart { caster, .. } if *caster == a_char_id as u64)
        })
        .await
        .expect("B, entering mid-cast, is sent A's bar");
    let since_cast = cast_began.elapsed().as_secs_f32();
    assert!(
        since_cast < 4.5,
        "the setup took too long to test the seed: {since_cast} s"
    );
    if let ServerWorldMsg::CastStart {
        spell_name,
        duration,
        ..
    } = seed
    {
        assert_eq!(spell_name, "Gate");
        assert!(
            duration > 0.0 && duration <= 5.0 - 1.2 + 0.05,
            "the time left on the server's 5 s, not the client's 9 s: {duration}"
        );
        assert!(
            duration >= 5.0 - since_cast - 0.1,
            "no more taken off than has passed ({since_cast} s): {duration}"
        );
    }
}

/// The client-made HitBroadcast is a dev command (the Test Panel's). A plain
/// account's forged hit on another player reaches nobody: before, it was
/// relayed as a real Hit, putting fake combat text on screens and, with the
/// seated-combat rule, standing the victim up. A GM's still fans to the target
/// with its payload, and the sender gets no echo.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_clients_hit_fanout() {
    let h = start_both().await;

    let (a_session, a_char_id, _stale) =
        provision_client(&h.auth_url, "iota", "Iot", "Human", "Warrior").await;
    let (b_session, b_char_id, b_token) =
        provision_client(&h.auth_url, "kappa", "Kap", "Elf", "Wizard").await;
    let pool = db::open(&h.db_url).await.expect("open pool");
    db::set_account_gm(&pool, "iota", true)
        .await
        .expect("set is_gm");
    let a_token = request_world_token(&h.auth_url, &a_session, a_char_id).await;

    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    let mut b = WorldClient::start(b_token, &b_session, b_char_id).await;

    // Only meaningful when the process is not in dev mode, where every
    // connection may use dev commands.
    if std::env::var("PD_DEV_CMDS").as_deref() != Ok("1") {
        b.send_hit(a_char_id as u64, 99, false, DamageType::Physical);
        b.pump_for(Duration::from_millis(300)).await;
        let forged = a
            .wait_for(CHANNEL_SYSTEM, Duration::from_secs(1), |m| {
                matches!(m, ServerWorldMsg::Hit { attacker, .. } if *attacker == b_char_id as u64)
            })
            .await;
        assert!(
            forged.is_none(),
            "a plain account's made-up hit reaches nobody"
        );
    }

    a.send_hit(b_char_id as u64, 42, true, DamageType::Fire);
    for _ in 0..4 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }

    let hit_at_b = b
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::Hit { attacker, target, .. }
                if *attacker == a_char_id as u64 && *target == b_char_id as u64)
        })
        .await
        .expect("B receives Hit from A");
    if let ServerWorldMsg::Hit {
        amount,
        crit,
        dmg_type,
        ..
    } = hit_at_b
    {
        assert_eq!(amount, 42);
        assert!(crit);
        assert_eq!(dmg_type, DamageType::Fire);
    }

    let echoed = a
        .wait_for(
            CHANNEL_SYSTEM,
            Duration::from_millis(500),
            |m| matches!(m, ServerWorldMsg::Hit { attacker, .. } if *attacker == a_char_id as u64),
        )
        .await;
    assert!(echoed.is_none(), "A should not receive own Hit echo");
}

/// Track 4 sub-task 5: death fan-out. A broadcasts DeathBroadcast; B
/// receives ServerWorldMsg::EntityDied { id = A.char_id }. Owner does not
/// receive own echo.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_clients_death_fanout() {
    let h = start_both().await;

    let (a_session, a_char_id, _a_stale) =
        provision_client(&h.auth_url, "lambda", "Lam", "Human", "Cleric").await;
    let (b_session, b_char_id, b_token) =
        provision_client(&h.auth_url, "mumu", "Mumu", "Elf", "Druid").await;

    let a_token = gm_world_token(&h.auth_url, &h.db_url, "lambda", &a_session, a_char_id).await;
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    let mut b = WorldClient::start(b_token, &b_session, b_char_id).await;

    a.send_death();
    for _ in 0..4 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }

    let died_at_b = b
        .wait_for(
            CHANNEL_SYSTEM,
            Duration::from_secs(3),
            |m| matches!(m, ServerWorldMsg::EntityDied { id } if *id == a_char_id as u64),
        )
        .await
        .expect("B receives EntityDied for A");
    assert!(matches!(died_at_b, ServerWorldMsg::EntityDied { .. }));

    let echoed = a
        .wait_for(
            CHANNEL_SYSTEM,
            Duration::from_millis(500),
            |m| matches!(m, ServerWorldMsg::EntityDied { id } if *id == a_char_id as u64),
        )
        .await;
    assert!(echoed.is_none(), "A should not receive own EntityDied echo");
    let _ = b_char_id; // silence unused if test order changes
}

/// Track 6 sub-task 4a: server-authoritative buff state. A casts
/// Healing Wave on self; server applies the HoT to A's active_buffs
/// and fans BuffSnapshot to in-world peers. B observes the buff
/// without A ever sending a BuffSnapshotBroadcast.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_clients_buff_snapshot_fanout() {
    let h = start_both().await;

    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "eta", "Eta", "Human", "Shaman").await;
    // Spirit of the Bear requires level 10; provisioned characters start at 1.
    set_char_level(&h.db_url, a_char_id, 10).await;
    let (b_session, b_char_id, b_token) =
        provision_client(&h.auth_url, "theta", "The", "Elf", "Druid").await;

    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    let mut b = WorldClient::start(b_token, &b_session, b_char_id).await;

    // A casts Spirit of the Bear (ALLY, 15 immediate heal + 4 hps Ã— 18s HoT,
    // cast_time 1.0s). Track 10 â€” the cast-time gate now rejects
    // CastSpell unless a matching CastStart ran long enough, so we
    // pump CastStart out first (transport doesn't advance during a
    // bare tokio sleep), then wait the cast time, then send CastSpell.
    // Server applies the HoT to A.active_buffs and fans BuffSnapshot
    // to all in-world peers including B.
    a.send_cast_start("Spirit of the Bear", 2.0);
    for _ in 0..3 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }
    a.pump_for(Duration::from_millis(2100)).await;
    a.send_cast_spell("Spirit of the Bear", None);
    for _ in 0..6 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }

    // Filter for the Spirit of the Bear entry so we don't latch onto the
    // empty seed snapshot the server fans at step-4a join time.
    let _ = b
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::BuffSnapshot { target, buffs }
                if *target == a_char_id as u64
                    && buffs.iter().any(|(n, _)| n == "Spirit of the Bear"))
        })
        .await
        .expect("B receives server-driven BuffSnapshot containing Spirit of the Bear");
}

/// Track 5 sub-task 1B: server-authoritative enemy spawn lifecycle.
///
/// The server boots, the spawner instantiates 27 enemies from the embedded
/// starter-zone TOML, and they sit idle (1C adds AI). When a client sends
/// EnterWorld, step 4a's seed loop fires an `EnemySpawn` for each of the
/// 27 live mobs. This test asserts: at least one EnemySpawn arrives, its
/// id falls inside the reserved enemy-id partition, and the carried mob
/// data matches a known starter-zone entry.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn enemies_visible_after_enter_world() {
    let h = start_both().await;

    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "epsilon", "Eps", "Human", "Warrior").await;

    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;

    let spawn = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(5), |m| {
            matches!(m, ServerWorldMsg::EnemySpawn { .. })
        })
        .await
        .expect("A receives at least one EnemySpawn after EnterWorld");

    if let ServerWorldMsg::EnemySpawn {
        id,
        mob_name,
        level,
        max_hp,
        hp,
        ..
    } = spawn
    {
        assert!(
            id >= ENEMY_ID_BASE,
            "enemy id must be in the reserved partition (got {id}, base {ENEMY_ID_BASE})"
        );
        assert!(!mob_name.is_empty(), "mob_name must be non-empty");
        assert!(level >= 1, "starter-zone mobs are level >= 1");
        assert!(max_hp > 0.0 && hp > 0.0, "fresh spawn has positive HP");
        assert!((hp - max_hp).abs() < 0.01, "fresh spawn is at full HP");
    }
}

/// Track 7 AOI â€” far-apart clients do not see each other.
///
/// CELL_SIZE = 120 m; cell boundaries at x = 120, 240, 360 â€¦
/// B is placed at x = 300 (cell (2, 0)) before connecting. A spawns at
/// the origin (cell (0, 0)). |2 âˆ’ 0| = 2 > 1, so they fall outside
/// each other's 3Ã—3 neighbourhood: neither should receive EntitySpawn
/// for the other. Previously (broadcast-to-all) both would appear.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn aoi_far_apart_clients_dont_see_each_other() {
    let h = start_both().await;

    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "aoifar1", "FarA", "Human", "Warrior").await;
    let (b_session, b_char_id, b_token) =
        provision_client(&h.auth_url, "aoifar2", "FarB", "Elf", "Cleric").await;

    // Place B at x = 300 (cell 2) before it connects. The world server
    // reads pos from the DB at ClientConnected time.
    let pool = db::open(&h.db_url).await.expect("open pool for pos update");
    db::set_character_position(&pool, b_char_id, 300.0, 0.0, 0.0)
        .await
        .expect("set B starting position");

    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    let mut b = WorldClient::start(b_token, &b_session, b_char_id).await;

    // 2 s is far beyond the 1-tick fanout window; if EntitySpawn were
    // going to arrive it would do so within ~100 ms.
    let spawn_b_at_a = a
        .wait_for(
            CHANNEL_SYSTEM,
            Duration::from_secs(2),
            |m| matches!(m, ServerWorldMsg::EntitySpawn { id, .. } if *id == b_char_id as u64),
        )
        .await;
    assert!(
        spawn_b_at_a.is_none(),
        "A at cell (0,0) must NOT receive EntitySpawn for B at cell (2,0)"
    );

    let spawn_a_at_b = b
        .wait_for(
            CHANNEL_SYSTEM,
            Duration::from_secs(2),
            |m| matches!(m, ServerWorldMsg::EntitySpawn { id, .. } if *id == a_char_id as u64),
        )
        .await;
    assert!(
        spawn_a_at_b.is_none(),
        "B at cell (2,0) must NOT receive EntitySpawn for A at cell (0,0)"
    );
}

/// Track 7 AOI â€” approaching client triggers mutual EntitySpawn on cell
/// boundary crossing.
///
/// A sits at the origin (cell (0, 0)). B starts at x = 260 (cell (2, 0))
/// and runs in the -X direction at RUN_SPEED = 5 m/s. After
/// ~4 s B crosses x = 240 (cell (1, 0)), which is adjacent to A's cell.
/// The tick loop's step-5b fan-out must fire mutual EntitySpawn at that
/// moment. We budget 12 s of walking to absorb test-parallelism jitter.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn aoi_approaching_client_triggers_entity_spawn() {
    let h = start_both().await;

    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "aoiappr1", "ApprA", "Human", "Warrior").await;
    let (b_session, b_char_id, b_token) =
        provision_client(&h.auth_url, "aoiappr2", "ApprB", "Elf", "Ranger").await;

    let pool = db::open(&h.db_url).await.expect("open pool");
    db::set_character_position(&pool, b_char_id, 260.0, 0.0, 0.0)
        .await
        .expect("set B starting position");

    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    let mut b = WorldClient::start(b_token, &b_session, b_char_id).await;

    // Walk B toward A. 260 to 240 = 20 m at 5 m/s = 4 s; 12 s covers
    // the full crossing with margin. Both transports are pumped each tick:
    // without pumping A's transport for ~12 s the server's 15 s netcode
    // timeout would fire and disconnect A before we can assert.
    let walk_end = Instant::now() + Duration::from_secs(12);
    let mut seq: u32 = 1;
    let mut heartbeat_tick: u32 = 0;
    while Instant::now() < walk_end {
        b.send_move(
            seq,
            Vec3 {
                x: -1.0,
                y: 0.0,
                z: 0.0,
            },
        );
        seq += 1;
        // A sends a Heartbeat every ~4 s (80 ticks) to satisfy the
        // server's 10 s app-layer idle timeout while B walks.
        heartbeat_tick += 1;
        if heartbeat_tick % 80 == 0 {
            a.send_heartbeat();
        }
        tick_one(&mut b.client, &mut b.transport);
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }

    // At this point B is at ~x = 210 (cell 1). EntitySpawn messages
    // arrived at both clients during the walk; wait_for drains the queue.
    a.wait_for(
        CHANNEL_SYSTEM,
        Duration::from_secs(5),
        |m| matches!(m, ServerWorldMsg::EntitySpawn { id, .. } if *id == b_char_id as u64),
    )
    .await
    .expect("A receives EntitySpawn for B once B enters A's 3Ã—3 neighbourhood");

    b.wait_for(
        CHANNEL_SYSTEM,
        Duration::from_secs(5),
        |m| matches!(m, ServerWorldMsg::EntitySpawn { id, .. } if *id == a_char_id as u64),
    )
    .await
    .expect("B receives EntitySpawn for A when it crosses into A's neighbourhood");
}

// A forged Move carrying NaN/Infinity direction components must be dropped
// whole: `clamp_length` passes NaN through (`len > max` is false for NaN),
// after which conn.pos goes permanently NaN and every `dist > RANGE` refusal
// gate silently passes. The handler guard drops the packet before ANY state
// is touched â€” including the sequence bookkeeping, so a later honest move
// re-using that sequence number still applies.
#[tokio::test]
async fn nan_move_direction_is_dropped() {
    let h = start_both().await;
    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "nanmover", "Nanmover", "Human", "Warrior").await;
    let (b_session, b_char_id, b_token) =
        provision_client(&h.auth_url, "nanwatch", "Nanwatch", "Elf", "Cleric").await;
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    let mut b = WorldClient::start(b_token, &b_session, b_char_id).await;
    let _ = b_char_id;

    // Baseline: an honest move fans a finite Position for A at B.
    a.send_move(
        1,
        Vec3 {
            x: 1.0,
            y: 0.0,
            z: 0.0,
        },
    );
    let baseline = b
        .wait_for(
            CHANNEL_POSITION,
            Duration::from_secs(3),
            |m| matches!(m, ServerWorldMsg::Position { id, .. } if *id == a_char_id as u64),
        )
        .await
        .expect("B sees A's baseline Position");
    if let ServerWorldMsg::Position { pos, .. } = &baseline {
        assert!(
            pos.x.is_finite() && pos.y.is_finite() && pos.z.is_finite(),
            "baseline position is finite"
        );
    }

    // Stop, then send the forgery: NaN x, Infinity z. If the guard is
    // missing, the tick integrates this into conn.pos and every Position
    // broadcast for A goes NaN from here on.
    a.send_move(
        2,
        Vec3 {
            x: 0.0,
            y: 0.0,
            z: 0.0,
        },
    );
    a.send_move(
        3,
        Vec3 {
            x: f32::NAN,
            y: 0.0,
            z: f32::INFINITY,
        },
    );
    for _ in 0..8 {
        tick_one(&mut a.client, &mut a.transport);
        tick_one(&mut b.client, &mut b.transport);
        tokio::time::sleep(TICK_DT).await;
    }

    // The forged packet must not have consumed its sequence number (it was
    // dropped before bookkeeping), so an honest move re-using seq 3 applies â€”
    // and the Position it produces is still finite.
    a.send_move(
        3,
        Vec3 {
            x: 0.0,
            y: 0.0,
            z: 1.0,
        },
    );
    let after = b
        .wait_for(
            CHANNEL_POSITION,
            Duration::from_secs(3),
            |m| matches!(m, ServerWorldMsg::Position { id, .. } if *id == a_char_id as u64),
        )
        .await
        .expect("A still moves after the forged packet");
    if let ServerWorldMsg::Position { pos, .. } = &after {
        assert!(
            pos.x.is_finite() && pos.y.is_finite() && pos.z.is_finite(),
            "position stayed finite after a NaN/Inf Move: {:?}",
            pos
        );
    }
}

/// The player target broadcast (target-of-target) was dead on the client
/// from 2026-05-29 to 2026-10-12, so the server path was never exercised in
/// play. Reviving it, the server fanned every SetTarget to EVERY player in the
/// world, once per message: a modified client could amplify a burst to the
/// whole server, and anyone anywhere learned who was fighting what. It now
/// goes only to players who can see the sender, at most once per tick (the
/// latest). Fails on the previous code: the far player receives it, and the
/// near one receives the whole burst.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_player_target_reaches_only_those_who_can_see_them_once_per_tick() {
    let h = start_both().await;
    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "tgta", "Targeta", "Human", "Warrior").await;
    let (b_session, b_char_id, b_token) =
        provision_client(&h.auth_url, "tgtb", "Targetb", "Human", "Warrior").await;
    let (c_session, c_char_id, c_token) =
        provision_client(&h.auth_url, "tgtc", "Targetc", "Human", "Warrior").await;
    // C two cells east of spawn: outside A's 3x3, so C cannot see A.
    set_char_pos(&h.db_url, c_char_id, 248.0, 0.0).await;
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    let mut b = WorldClient::start(b_token, &b_session, b_char_id).await;
    let mut c = WorldClient::start(c_token, &c_session, c_char_id).await;
    a.pump_for(Duration::from_millis(300)).await;
    b.pump_for(Duration::from_millis(300)).await;
    c.pump_for(Duration::from_millis(300)).await;

    // A burst of five retargets in one go; the last names B.
    for t in [9001u64, 9002, 9003, 9004, b_char_id as u64] {
        send_msg(
            &mut a.client,
            CHANNEL_SYSTEM,
            &ClientWorldMsg::SetTarget { target_id: Some(t) },
        );
    }
    for _ in 0..4 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }

    // B (beside A) hears A's target exactly once, and it is the last one.
    let mut seen: Vec<Option<u64>> = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if let Some(ServerWorldMsg::EntityTarget { target, .. }) = b
            .wait_for(
                CHANNEL_SYSTEM,
                Duration::from_millis(300),
                |m| matches!(m, ServerWorldMsg::EntityTarget { id, .. } if *id == a_char_id as u64),
            )
            .await
        {
            seen.push(target);
        }
    }
    assert_eq!(
        seen,
        vec![Some(b_char_id as u64)],
        "B gets the latest target, once"
    );

    // C (two cells away) hears nothing about A.
    let far = c
        .wait_for(
            CHANNEL_SYSTEM,
            Duration::from_millis(800),
            |m| matches!(m, ServerWorldMsg::EntityTarget { id, .. } if *id == a_char_id as u64),
        )
        .await;
    assert!(
        far.is_none(),
        "a player who cannot see A must not learn A's target"
    );
}

/// Scale readiness: an idle enemy's Position must not fan at 20 Hz. The
/// statue has aggro 0 and speed 0, so it never targets and never moves.
/// Before the change-or-keepalive gate it still produced one Position per
/// tick per visible player (~40 in 2 s); after, the first-tick send plus
/// one keepalive per `ENEMY_POSITION_KEEPALIVE` gap, so 2..=8 with slack.
#[tokio::test]
async fn idle_enemy_position_fan_is_gated() {
    let h = start_both().await;
    let (a_session, a_char_id, _stale_token) =
        provision_client(&h.auth_url, "idlefan", "Idlefan", "Human", "Warrior").await;
    let pool = db::open(&h.db_url).await.expect("open pool");
    db::set_account_gm(&pool, "idlefan", true)
        .await
        .expect("set is_gm");
    let a_token = request_world_token(&h.auth_url, &a_session, a_char_id).await;
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;

    a.send_dev_spawn("Idle Statue", 1, 10.0, 0, 0.0, 0.0);
    let spawn_evt = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::EnemySpawn { mob_name, .. } if mob_name == "Idle Statue")
        })
        .await
        .expect("the statue fans an EnemySpawn");
    let statue_id: u64 = match spawn_evt {
        ServerWorldMsg::EnemySpawn { id, .. } => id,
        _ => unreachable!(),
    };

    // Count the statue's Position messages over a 2 s window, servicing the
    // transport every tick so nothing is lost in the socket buffer.
    let end = Instant::now() + Duration::from_secs(2);
    let mut count = 0u32;
    let mut last_seq: Option<u32> = None;
    while Instant::now() < end {
        tick_one(&mut a.client, &mut a.transport);
        while let Some(bytes) = a.client.receive_message(CHANNEL_POSITION) {
            if let Ok((msg, _)) =
                bincode::serde::decode_from_slice::<ServerWorldMsg, _>(&bytes, bincode_cfg())
            {
                if let ServerWorldMsg::Position { id, sequence, .. } = msg {
                    if id == statue_id {
                        count += 1;
                        if let Some(prev) = last_seq {
                            assert!(
                                sequence > prev,
                                "statue Position sequence must be strictly increasing ({prev} then {sequence})"
                            );
                        }
                        last_seq = Some(sequence);
                    }
                }
            }
        }
        tokio::time::sleep(TICK_DT).await;
    }
    eprintln!("idle statue Position messages in 2 s: {count}");
    assert!(
        (2..=8).contains(&count),
        "an idle enemy should fan a first send plus ~500 ms keepalives over 2 s, got {count} (20 Hz would be ~40)"
    );
}

/// Scale readiness: an enemy crossing an AOI cell boundary must fan an
/// EnemySpawn to the players its new neighbourhood brings it into view of,
/// and an EntityDespawn to the ones it leaves. Before this the crossing only
/// updated the grid, so a mob chasing into view streamed Position for an id
/// the client had never been given a spawn for and stayed invisible.
#[tokio::test]
async fn enemy_crossing_a_cell_boundary_spawns_and_despawns_for_players() {
    let h = start_both().await;
    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "aoiwatch", "Aoiwatch", "Human", "Warrior").await;
    let (b_session, b_char_id, _stale_token) =
        provision_client(&h.auth_url, "aoipull", "Aoipull", "Elf", "Cleric").await;
    let pool = db::open(&h.db_url).await.expect("open pool");
    db::set_account_gm(&pool, "aoipull", true)
        .await
        .expect("set is_gm");
    set_char_pos(&h.db_url, b_char_id, 248.0, 60.0).await;
    let b_token = request_world_token(&h.auth_url, &b_session, b_char_id).await;

    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    let mut b = WorldClient::start(b_token, &b_session, b_char_id).await;
    pump_both_for(&mut a, &mut b, Duration::from_millis(300)).await;

    // The runner spawns ~3 m from B, inside aggro, so it closes on B at once
    // and follows when B walks; 1 dmg swings are harmless. Chase leashes on
    // distance to the TARGET (aggro x 2), and B at 5 m/s opens the gap by
    // 2 m per wall-clock second of walking, so aggro is set to the 50 cap
    // (100 m leash) to keep a slow test loop from leashing it home.
    b.send_dev_spawn("Border Runner", 1, 10.0, 1, 3.0, 50.0);
    let spawn_evt = b
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::EnemySpawn { mob_name, .. } if mob_name == "Border Runner")
        })
        .await
        .expect("B, in the same cell, sees the dev spawn");
    let runner_id: u64 = match spawn_evt {
        ServerWorldMsg::EnemySpawn { id, .. } => id,
        _ => unreachable!(),
    };
    let early = wait_on_a_pumping_b(
        &mut a,
        &mut b,
        Duration::from_millis(300),
        |m| matches!(m, ServerWorldMsg::EnemySpawn { id, .. } if *id == runner_id),
    )
    .await;
    assert!(
        early.is_none(),
        "a mob two cells away must not be seeded to A"
    );

    let mut seq: u32 = 1;
    walk_pumping(
        &mut b,
        &mut a,
        &mut seq,
        Vec3 {
            x: -1.0,
            y: 0.0,
            z: 0.0,
        },
        40,
    )
    .await;
    let spawned = wait_on_a_pumping_b(
        &mut a,
        &mut b,
        Duration::from_secs(15),
        |m| matches!(m, ServerWorldMsg::EnemySpawn { id, .. } if *id == runner_id),
    )
    .await;
    assert!(
        spawned.is_some(),
        "A must get an EnemySpawn when the chasing mob crosses into a cell adjacent to A's"
    );

    walk_pumping(
        &mut b,
        &mut a,
        &mut seq,
        Vec3 {
            x: 1.0,
            y: 0.0,
            z: 0.0,
        },
        50,
    )
    .await;
    let despawned = wait_on_a_pumping_b(
        &mut a,
        &mut b,
        Duration::from_secs(15),
        |m| matches!(m, ServerWorldMsg::EntityDespawn { id } if *id == runner_id),
    )
    .await;
    assert!(
        despawned.is_some(),
        "A must get an EntityDespawn when the mob follows B back out of view"
    );
}

/// The player side of the pet gap: a player walking into view of an EXISTING
/// pet gets its PetSpawn. The id partitions stack (player < enemy < bag <
/// pet), so the player cell-change handler's bag/corpse arm swallowed pets.
#[tokio::test]
async fn player_crossing_into_view_of_an_existing_pet_gets_pet_spawn() {
    let h = start_both().await;
    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "petcomer", "Petcomer", "Human", "Warrior").await;
    set_char_pos(&h.db_url, a_char_id, 248.0, 60.0).await;
    let (b_session, b_char_id, b_token) =
        provision_client(&h.auth_url, "pethome", "Pethome", "Human", "Necromancer").await;
    set_char_level(&h.db_url, b_char_id, 6).await;

    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    let mut b = WorldClient::start(b_token, &b_session, b_char_id).await;
    pump_both_for(&mut a, &mut b, Duration::from_millis(300)).await;

    let pet_id = summon_skeleton(&mut b, &mut a, b_char_id).await;
    let early = wait_on_a_pumping_b(
        &mut a,
        &mut b,
        Duration::from_millis(300),
        |m| matches!(m, ServerWorldMsg::PetSpawn { id, .. } if *id == pet_id),
    )
    .await;
    assert!(
        early.is_none(),
        "A starts two cells from the pet and must not have it yet"
    );

    let mut seq: u32 = 1;
    walk_pumping(
        &mut a,
        &mut b,
        &mut seq,
        Vec3 {
            x: -1.0,
            y: 0.0,
            z: 0.0,
        },
        40,
    )
    .await;
    let spawned = wait_on_a_pumping_b(&mut a, &mut b, Duration::from_secs(10), |m| {
        matches!(m, ServerWorldMsg::PetSpawn { id, owner, .. }
            if *id == pet_id && *owner == b_char_id as u64)
    })
    .await;
    assert!(
        spawned.is_some(),
        "A must get the existing pet's PetSpawn on walking into its neighbourhood"
    );
}

/// The owner always sees their own pet (user call, 2026-10-02). B parks a
/// skeleton with `/pet guard` in cell (1,0) and walks west until B stands in
/// cell (-1,0), two cells from the pet and outside the 3x3 that decides what
/// anyone else sees. Before this change B's own cell crossing sent B an
/// EntityDespawn for the pet, so the pet panel and every `/pet` command went
/// dark until B walked back. Now: no despawn, the pet's Position keeps
/// arriving (the guard keepalive), and `/pet follow` from out there starts it
/// back, which B watches it do.
#[tokio::test]
async fn owner_keeps_seeing_a_pet_parked_two_cells_away() {
    let h = start_both().await;
    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "petfar_w", "Petfarw", "Human", "Warrior").await;
    let (b_session, b_char_id, b_token) =
        provision_client(&h.auth_url, "petfar_o", "Petfaro", "Human", "Necromancer").await;
    set_char_level(&h.db_url, b_char_id, 6).await;
    // B in cell (0,0) a metre short of the x=120 line; the summon lands the
    // pet 1.5 m east of B, in cell (1,0). Row z = 80: the walk west passes no
    // camp's aggro by 14 m. On row 60 it ended 7.4 m from a Cave Bat (aggro
    // 12), and since the EQ parity pass a level 6 Necromancer can die to the
    // bats before the pet arrives; a dead owner drops out of the AI's target
    // list and the pet stands still, which failed this test about one full
    // run in four.
    set_char_pos(&h.db_url, b_char_id, 119.0, 80.0).await;

    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    let mut b = WorldClient::start(b_token, &b_session, b_char_id).await;
    pump_both_for(&mut a, &mut b, Duration::from_millis(300)).await;

    let pet_id = summon_skeleton(&mut b, &mut a, b_char_id).await;
    b.send_pet_command(protocol::world::pet_command::GUARD, None);
    pump_both_for(&mut a, &mut b, Duration::from_millis(300)).await;

    // Walk B west until its own echoed Position is past x = -2 (cell -1),
    // draining B's channels the whole way: a pet despawn anywhere in here
    // is the old behaviour.
    let mut seq: u32 = 1;
    let mut b_x: f32 = 119.0;
    let mut pet_despawned = false;
    let mut last_pet_pos: Option<Vec3> = None;
    let west = Vec3 {
        x: -1.0,
        y: 0.0,
        z: 0.0,
    };
    let drain_b = |b: &mut WorldClient,
                   b_x: &mut f32,
                   pet_despawned: &mut bool,
                   last_pet_pos: &mut Option<Vec3>| {
        while let Some(bytes) = b.client.receive_message(CHANNEL_POSITION) {
            if let Ok((msg, _)) =
                bincode::serde::decode_from_slice::<ServerWorldMsg, _>(&bytes, bincode_cfg())
            {
                if let ServerWorldMsg::Position { id, pos, .. } = msg {
                    if id == b_char_id as u64 {
                        *b_x = pos.x;
                    } else if id == pet_id {
                        *last_pet_pos = Some(pos);
                    }
                }
            }
        }
        while let Some(bytes) = b.client.receive_message(CHANNEL_SYSTEM) {
            if let Ok((msg, _)) =
                bincode::serde::decode_from_slice::<ServerWorldMsg, _>(&bytes, bincode_cfg())
            {
                if matches!(msg, ServerWorldMsg::EntityDespawn { id } if id == pet_id) {
                    *pet_despawned = true;
                }
            }
        }
    };
    let mut ticks: u32 = 0;
    while b_x > -2.0 {
        assert!(
            ticks < 800,
            "B never reached cell (-1,0): x = {b_x} after {ticks} moves"
        );
        b.send_move(seq, west);
        seq += 1;
        ticks += 1;
        if ticks % 80 == 0 {
            a.send_heartbeat();
        }
        tick_one(&mut a.client, &mut a.transport);
        tick_one(&mut b.client, &mut b.transport);
        assert_both_connected(&a, &b, "the owner's walk west");
        drain_b(&mut b, &mut b_x, &mut pet_despawned, &mut last_pet_pos);
        tokio::time::sleep(TICK_DT).await;
    }
    assert!(
        !pet_despawned,
        "B was sent an EntityDespawn for its own pet on walking out of view"
    );

    // Out here only the owner rule delivers the parked pet's keepalives:
    // the pet's cell (1,0) is not adjacent to B's (-1,0). Wait for the next
    // one rather than for a fixed window: under a loaded test run the
    // server's 500 ms keepalive can arrive late, and that is not the bug.
    last_pet_pos = None;
    let end = Instant::now() + Duration::from_secs(8);
    a.send_heartbeat();
    b.send_heartbeat();
    while Instant::now() < end && last_pet_pos.is_none() {
        tick_one(&mut a.client, &mut a.transport);
        tick_one(&mut b.client, &mut b.transport);
        assert_both_connected(&a, &b, "the wait for a parked keepalive");
        drain_b(&mut b, &mut b_x, &mut pet_despawned, &mut last_pet_pos);
        tokio::time::sleep(TICK_DT).await;
    }
    let parked = last_pet_pos
        .expect("the parked pet's Position must keep reaching its owner two cells away");
    assert!(
        (parked.x - 120.5).abs() < 1.0,
        "a guarding pet should still be where it was parked, got x = {}",
        parked.x
    );
    assert!(
        !pet_despawned,
        "B was sent an EntityDespawn for its own pet while parked out of view"
    );

    // The point of the rule: the pet can be called back from out of view.
    // The proof is that the owner SEES it set off (a few metres west of
    // where it was parked), not that it covers a distance against a clock:
    // how far a pet gets in N wall-clock seconds depends on how loaded the
    // machine is, and a deadline on distance failed in a full parallel run.
    b.send_pet_command(protocol::world::pet_command::FOLLOW, None);
    let end = Instant::now() + Duration::from_secs(30);
    let mut came_back = false;
    let mut t: u32 = 0;
    while Instant::now() < end {
        tick_one(&mut a.client, &mut a.transport);
        tick_one(&mut b.client, &mut b.transport);
        assert_both_connected(&a, &b, "the pet's walk back");
        drain_b(&mut b, &mut b_x, &mut pet_despawned, &mut last_pet_pos);
        if last_pet_pos.is_some_and(|p| p.x < parked.x - 3.0) {
            came_back = true;
            break;
        }
        t += 1;
        if t % 80 == 0 {
            a.send_heartbeat();
            b.send_heartbeat();
        }
        tokio::time::sleep(TICK_DT).await;
    }
    assert!(
        came_back,
        "after /pet follow the owner must watch the pet set off toward them (last pet x = {:?})",
        last_pet_pos.map(|p| p.x)
    );
    assert!(
        !pet_despawned,
        "B was sent an EntityDespawn for its own pet during the walk back"
    );
}

/// Every player is told the hour as they enter the world, and two players
/// are told the same hour. Before this the variant existed on the wire with
/// no sender, so each client ran a private clock that restarted at 8 AM with
/// the game process, and no two friends shared a sky. B joins after A here,
/// so B is also the late-joiner case: seeded at once, not left waiting up to
/// a minute for the next broadcast.
#[tokio::test]
async fn every_player_is_told_the_same_hour_on_entering_the_world() {
    let h = start_both().await;
    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "skyone", "Skyone", "Human", "Warrior").await;
    let (b_session, b_char_id, b_token) =
        provision_client(&h.auth_url, "skytwo", "Skytwo", "Elf", "Cleric").await;

    let hour_of = |m: Option<ServerWorldMsg>, who: &str| -> f32 {
        match m {
            Some(ServerWorldMsg::TimeOfDay { hour }) => hour,
            _ => panic!("{who} was not told the hour on entering the world"),
        }
    };

    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    let a_hour = hour_of(
        a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::TimeOfDay { .. })
        })
        .await,
        "A",
    );
    let mut b = WorldClient::start(b_token, &b_session, b_char_id).await;
    let b_hour = hour_of(
        b.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::TimeOfDay { .. })
        })
        .await,
        "B",
    );

    for (who, hour) in [("A", a_hour), ("B", b_hour)] {
        assert!(
            (0.0..24.0).contains(&hour),
            "{who} was told hour {hour}, outside the day"
        );
    }
    // B entered a second or two after A; a game hour is 50 real seconds, so
    // the two differ by a few hundredths of an hour at most. Compare around
    // the clock so a midnight between the two sends does not read as 24.
    let apart = (b_hour - a_hour).rem_euclid(24.0);
    let apart = apart.min(24.0 - apart);
    assert!(
        apart < 0.5,
        "A was told {a_hour} and B {b_hour}: the same clock should not differ by {apart} hours"
    );
    assert_both_connected(&a, &b, "the world clock test");
}

/// A paperdoll used to be readable from anywhere in the world. B is seeded
/// 200 m from A, far beyond INSPECT_RANGE: A's inspect must be refused with a
/// chat line and an empty result. An id that never existed must get the very
/// same answer, or the gate would be a char_id to name map and an is-online
/// oracle for anyone enumerating ids.
#[tokio::test]
async fn inspect_player_refuses_beyond_range() {
    let h = start_both().await;
    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "inspfar", "Inspfar", "Human", "Warrior").await;
    let (b_session, b_char_id, b_token) =
        provision_client(&h.auth_url, "insptgt", "Insptgt", "Elf", "Cleric").await;
    set_char_pos(&h.db_url, b_char_id, 200.0, 0.0).await;

    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    let mut b = WorldClient::start(b_token, &b_session, b_char_id).await;
    pump_both_for(&mut a, &mut b, Duration::from_millis(300)).await;

    const NOBODY: i64 = 999_999;
    for (label, target) in [("out of range", b_char_id), ("nonexistent", NOBODY)] {
        a.send_inspect_player(target);
        // The line, then the result, in the order the server sends them.
        let line = wait_on_a_pumping_b(&mut a, &mut b, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::ChatMessage { text, .. }
                if text.contains("not close enough to inspect"))
        })
        .await
        .unwrap_or_else(|| panic!("a {label} inspect must answer with the refusal line"));
        if let ServerWorldMsg::ChatMessage { text, .. } = line {
            assert!(
                !text.contains("Insptgt"),
                "the refusal must not name the target (got {text:?})"
            );
        }
        let result = wait_on_a_pumping_b(&mut a, &mut b, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::InspectResult { target_char_id, .. }
                if *target_char_id == target)
        })
        .await
        .unwrap_or_else(|| panic!("a {label} inspect still closes the client's waiting state"));
        if let ServerWorldMsg::InspectResult {
            target_name, slots, ..
        } = result
        {
            assert!(target_name.is_empty(), "a {label} inspect names nobody");
            assert!(slots.is_empty(), "a {label} inspect discloses no slots");
        }
    }
}

/// The honest path is untouched: 20 m apart, inside INSPECT_RANGE, the
/// inspect returns the target's name.
#[tokio::test]
async fn inspect_player_answers_within_range() {
    let h = start_both().await;
    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "inspnear", "Inspnear", "Human", "Warrior").await;
    let (b_session, b_char_id, b_token) =
        provision_client(&h.auth_url, "inspbud", "Inspbud", "Elf", "Cleric").await;
    set_char_pos(&h.db_url, b_char_id, 20.0, 0.0).await;

    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    let mut b = WorldClient::start(b_token, &b_session, b_char_id).await;
    pump_both_for(&mut a, &mut b, Duration::from_millis(300)).await;

    a.send_inspect_player(b_char_id);
    let result = wait_on_a_pumping_b(&mut a, &mut b, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::InspectResult { target_char_id, .. }
            if *target_char_id == b_char_id)
    })
    .await
    .expect("an in-range inspect answers");
    if let ServerWorldMsg::InspectResult { target_name, .. } = result {
        assert_eq!(target_name, "Inspbud");
    }
}

/// Client-authority audit fix 2: the server decides how fast a player moves.
/// A full-length Move is a 5 m/s run (0.25 m a tick), where it used to be the
/// 7.5 m/s ceiling the client scaled its own speed against, so a modified
/// client ran 1.5x; a forged over-long direction is no faster; and a heavily
/// overloaded character crawls at the encumbrance floor (a quarter speed),
/// which the client alone used to decide.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_server_decides_run_speed() {
    let h = start_both().await;
    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "runner", "Runna", "Human", "Warrior").await;
    let (b_session, b_char_id, _stale) =
        provision_client(&h.auth_url, "hoarder", "Hoarda", "Human", "Warrior").await;
    // Forty thousand copper weigh 800, far past any level 1 capacity.
    let pool = db::open(&h.db_url).await.expect("open pool");
    sqlx::query("UPDATE characters SET copper = 40000 WHERE id = ?1")
        .bind(b_char_id)
        .execute(&pool)
        .await
        .expect("seed coins");
    let b_token = request_world_token(&h.auth_url, &b_session, b_char_id).await;

    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    a.pump_for(Duration::from_millis(300)).await;
    let mut seq = 1u32;
    let east = Vec3 {
        x: 1.0,
        y: 0.0,
        z: 0.0,
    };
    let run = median_step(&mut a, a_char_id, &mut seq, east, 30).await;
    assert!(
        (run - 0.25).abs() < 0.02,
        "a full-length Move runs 5 m/s, 0.25 m a tick: {run}"
    );
    let forged = Vec3 {
        x: 3.0,
        y: 0.0,
        z: 0.0,
    };
    let forged_run = median_step(&mut a, a_char_id, &mut seq, forged, 30).await;
    assert!(
        (forged_run - 0.25).abs() < 0.02,
        "an over-long direction is no faster: {forged_run}"
    );
    a.send_disconnect();
    a.pump_for(Duration::from_millis(200)).await;

    let mut b = WorldClient::start(b_token, &b_session, b_char_id).await;
    b.pump_for(Duration::from_millis(300)).await;
    let mut seq = 1u32;
    let crawl = median_step(&mut b, b_char_id, &mut seq, east, 30).await;
    let floor = 0.25 * projectdawn_server::world::encumbrance::MIN_SPEED_MULT;
    assert!(
        (crawl - floor).abs() < 0.01,
        "an overloaded character crawls at the floor: {crawl}"
    );
    b.send_disconnect();
    b.pump_for(Duration::from_millis(200)).await;
}

/// The jump relay (the "Remote players never show a jump" To-Do, cosmetic
/// version). A watcher stands beside a jumper at the spawn; since 2026-10-29
/// (the user's jump check) a second watches from 128 m, in view but past the
/// say range, and sees the cue too, while a third two cells away, out of
/// view, sees nothing. One Move with
/// `jumping: true` reaches the watcher as exactly one PlayerJumped naming the
/// jumper, and never reaches the jumper; a burst of ten alternating flags
/// inside the half-second interval relays nothing more; an edge after the
/// interval relays once more. The jumper's own Position keeps y at zero
/// throughout: the flag animates peers and moves nobody.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_jump_reaches_the_players_nearby_once_and_moves_nobody() {
    let h = start_both().await;
    let (j_session, j_char_id, _) =
        provision_client(&h.auth_url, "jumper", "Hoppy", "Human", "Warrior").await;
    let (w_session, w_char_id, _) =
        provision_client(&h.auth_url, "jumpwatch", "Seeson", "Human", "Warrior").await;
    let j_token = request_world_token(&h.auth_url, &j_session, j_char_id).await;
    let mut j = WorldClient::start(j_token, &j_session, j_char_id).await;
    let w_token = request_world_token(&h.auth_url, &w_session, w_char_id).await;
    let mut w = WorldClient::start(w_token, &w_session, w_char_id).await;
    pump_both_for(&mut j, &mut w, Duration::from_millis(400)).await;
    // The jump check: a watcher in view past the say range (128 m away, the
    // same AOI cell; row z = 80 is clear of every camp) and one two cells
    // away, out of view.
    let (f_session, f_char_id, _) =
        provision_client(&h.auth_url, "jumpfar", "Farsee", "Human", "Warrior").await;
    let (o_session, o_char_id, _) =
        provision_client(&h.auth_url, "jumpout", "Outsee", "Human", "Warrior").await;
    set_char_pos(&h.db_url, f_char_id, 100.0, 80.0).await;
    set_char_pos(&h.db_url, o_char_id, 248.0, 60.0).await;
    let f_token = request_world_token(&h.auth_url, &f_session, f_char_id).await;
    let mut f = WorldClient::start(f_token, &f_session, f_char_id).await;
    let o_token = request_world_token(&h.auth_url, &o_session, o_char_id).await;
    let mut o = WorldClient::start(o_token, &o_session, o_char_id).await;
    pump_both_for(&mut f, &mut o, Duration::from_millis(400)).await;
    // Drain what entering fanned, so the counts below are the jumps only.
    let _ = collect_system(&mut w, Duration::from_millis(200)).await;
    let _ = collect_system(&mut j, Duration::from_millis(200)).await;
    let _ = collect_system(&mut f, Duration::from_millis(200)).await;
    let _ = collect_system(&mut o, Duration::from_millis(200)).await;
    let still = Vec3 {
        x: 0.0,
        y: 0.0,
        z: 0.0,
    };
    let jumps_in = |msgs: &[ServerWorldMsg]| {
        msgs.iter()
            .filter(|m| matches!(m, ServerWorldMsg::PlayerJumped { id } if *id == j_char_id as u64))
            .count()
    };

    // One jump, then a forged burst of ten alternating flags inside the
    // half-second interval (about 200 ms): exactly one relay in all.
    j.send_move_jump(1, still, true);
    for i in 2..12u32 {
        j.send_move_jump(i, still, i % 2 == 0);
        tick_one(&mut j.client, &mut j.transport);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let (w_msgs, j_msgs, f_msgs, o_msgs) = tokio::join!(
        collect_system(&mut w, Duration::from_millis(600)),
        collect_system(&mut j, Duration::from_millis(600)),
        collect_system(&mut f, Duration::from_millis(600)),
        collect_system(&mut o, Duration::from_millis(600)),
    );
    assert_eq!(
        jumps_in(&w_msgs),
        1,
        "the watcher sees one jump; the burst inside the interval adds none"
    );
    assert_eq!(
        jumps_in(&j_msgs),
        0,
        "the jumper is not told about their own jump"
    );
    assert_eq!(
        jumps_in(&f_msgs),
        1,
        "a watcher in view past the say range sees the jump: the jump check"
    );
    assert_eq!(
        jumps_in(&o_msgs),
        0,
        "a watcher two cells away, out of view, sees nothing"
    );

    // Past the interval, the next edge relays again.
    pump_both_for(&mut j, &mut w, Duration::from_millis(400)).await;
    j.send_move_jump(20, still, false);
    tick_one(&mut j.client, &mut j.transport);
    tokio::time::sleep(TICK_DT).await;
    j.send_move_jump(21, still, true);
    let (w_msgs, _) = tokio::join!(
        collect_system(&mut w, Duration::from_millis(600)),
        collect_system(&mut j, Duration::from_millis(600)),
    );
    assert_eq!(
        jumps_in(&w_msgs),
        1,
        "a jump after the interval relays once more"
    );

    // Nobody moved for any of it: the jumper's own Position keeps y at zero.
    j.send_move(30, still);
    let pos = own_position(&mut j, j_char_id)
        .await
        .expect("the jumper's own Position");
    assert!(
        pos.y.abs() < 1e-3,
        "the server never lifts anyone for a jump: y = {}",
        pos.y
    );

    j.send_disconnect();
    w.send_disconnect();
    f.send_disconnect();
    o.send_disconnect();
    j.pump_for(Duration::from_millis(200)).await;
    w.pump_for(Duration::from_millis(200)).await;
    f.pump_for(Duration::from_millis(200)).await;
    o.pump_for(Duration::from_millis(200)).await;
}

/// A `/say` carries SAY_RANGE (50 m; user, 2026-10-27: "the volume of a
/// conversation") and no further. A listener 40 m from the speaker hears a
/// say; walked out to 60 m it hears nothing, while a shout still reaches it
/// (so the silence is the range, not the link). On the previous server a say
/// reached the speaker's whole 3x3 AOI neighbourhood of 120 m cells, so the
/// 60 m listener heard it too. Row z = 80 is clear of every camp.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_say_carries_fifty_metres_and_no_further() {
    let h = start_both().await;
    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "sayrange_a", "Sayer", "Human", "Warrior").await;
    let (b_session, b_char_id, b_token) =
        provision_client(&h.auth_url, "sayrange_b", "Hearer", "Human", "Warrior").await;
    set_char_pos(&h.db_url, a_char_id, 0.0, 80.0).await;
    set_char_pos(&h.db_url, b_char_id, 40.0, 80.0).await;
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    let mut b = WorldClient::start(b_token, &b_session, b_char_id).await;
    pump_both_for(&mut a, &mut b, Duration::from_millis(300)).await;

    a.send_say("near");
    let heard = wait_on_a_pumping_b(
        &mut b,
        &mut a,
        Duration::from_secs(3),
        |m| matches!(m, ServerWorldMsg::ChatMessage { text, .. } if text == "near"),
    )
    .await;
    assert!(heard.is_some(), "a say reaches a listener 40 m away");

    let mut seq = 1u32;
    let stood = walk_until_within(
        &mut b,
        &mut seq,
        b_char_id,
        Vec3 {
            x: 40.0,
            y: 0.0,
            z: 80.0,
        },
        Vec3 {
            x: 60.0,
            y: 0.0,
            z: 80.0,
        },
        0.3,
    )
    .await;
    assert!(
        stood.x > 55.0,
        "test setup: the listener stands past 55 m, is at x={:.1}",
        stood.x
    );
    a.send_say("far");
    let heard = wait_on_a_pumping_b(
        &mut b,
        &mut a,
        Duration::from_millis(1_500),
        |m| matches!(m, ServerWorldMsg::ChatMessage { text, .. } if text == "far"),
    )
    .await;
    assert!(heard.is_none(), "a say does not reach a listener 60 m away");

    send_msg(
        &mut a.client,
        CHANNEL_SYSTEM,
        &ClientWorldMsg::Chat {
            channel: protocol::world::ChatChannel::Shout,
            text: "still here".into(),
            target_name: None,
        },
    );
    let heard = wait_on_a_pumping_b(
        &mut b,
        &mut a,
        Duration::from_secs(3),
        |m| matches!(m, ServerWorldMsg::ChatMessage { text, .. } if text == "still here"),
    )
    .await;
    assert!(
        heard.is_some(),
        "a shout still reaches the 60 m listener: the silence was the range"
    );
}

/// The facing (PD_W0029; user, 2026-10-27, option a): a Move carries the
/// client's own body yaw and the Position fan relays it, so B sees A turn.
/// A stands 5 m from B and sends a Move with yaw 1.25: B's Positions for A
/// carry 1.25. A Move with a non-finite yaw is dropped whole (B keeps seeing
/// 1.25), and a yaw past PI comes back wrapped into [-PI, PI). On the previous
/// server A's yaw stayed at its login value whatever the client sent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_players_facing_reaches_the_watcher() {
    use std::f32::consts::TAU;
    let h = start_both().await;
    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "facing_a", "Turner", "Human", "Warrior").await;
    let (b_session, b_char_id, b_token) =
        provision_client(&h.auth_url, "facing_b", "Watcher", "Human", "Warrior").await;
    set_char_pos(&h.db_url, a_char_id, 0.0, 80.0).await;
    set_char_pos(&h.db_url, b_char_id, 5.0, 80.0).await;
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    let mut b = WorldClient::start(b_token, &b_session, b_char_id).await;
    pump_both_for(&mut a, &mut b, Duration::from_millis(300)).await;
    let a_id = a_char_id as u64;
    let still = Vec3 {
        x: 0.0,
        y: 0.0,
        z: 0.0,
    };

    a.send_move_facing(1, still, 1.25);
    let seen = yaws_seen(&mut b, &mut a, a_id, Duration::from_millis(600)).await;
    let last = *seen.last().expect("B receives A's Positions");
    assert!(
        (last - 1.25).abs() < 1e-4,
        "B sees A facing 1.25, sees {last}"
    );

    a.send_move_facing(2, still, f32::NAN);
    let seen = yaws_seen(&mut b, &mut a, a_id, Duration::from_millis(400)).await;
    assert!(!seen.is_empty(), "B keeps receiving A's Positions");
    assert!(
        seen.iter().all(|y| (y - 1.25).abs() < 1e-4),
        "a non-finite yaw is dropped whole: B still sees 1.25, saw {seen:?}"
    );

    a.send_move_facing(3, still, 4.0);
    let seen = yaws_seen(&mut b, &mut a, a_id, Duration::from_millis(600)).await;
    let last = *seen.last().expect("B receives A's Positions");
    assert!(
        (last - (4.0 - TAU)).abs() < 1e-4,
        "a yaw past PI is wrapped into (-PI, PI]: {last}"
    );
}

/// A chat line with a two-byte character straddling byte 512 used to panic
/// the slice `text[..512]` inside the tick task, which took the whole server
/// down. The server cuts chat to `MAX_CHAT_CHARS` (250) characters now
/// (2026-10-26, the client stops at the same cap): the listener gets exactly
/// 250, and the server keeps serving, so the speaker's next line still lands.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_chat_line_cut_inside_a_multibyte_char_does_not_crash_the_server() {
    let h = start_both().await;
    let (a_session, a_char, a_token) =
        provision_client(&h.auth_url, "chatcrasha", "Speaker", "Human", "Warrior").await;
    let (b_session, b_char, b_token) =
        provision_client(&h.auth_url, "chatcrashb", "Listener", "Human", "Warrior").await;
    let mut a = WorldClient::start(a_token, &a_session, a_char).await;
    let mut b = WorldClient::start(b_token, &b_session, b_char).await;

    let poison = format!("{}é and more", "a".repeat(511));
    assert!(
        !poison.is_char_boundary(512),
        "the test line must straddle byte 512"
    );
    a.send_say(&poison);
    a.pump_for(Duration::from_millis(300)).await;
    a.send_say("still here");
    a.pump_for(Duration::from_millis(100)).await;

    let cut = b
        .wait_for(
            CHANNEL_SYSTEM,
            Duration::from_secs(3),
            |m| matches!(m, ServerWorldMsg::ChatMessage { text, .. } if text.starts_with("aaa")),
        )
        .await;
    let Some(ServerWorldMsg::ChatMessage { text, .. }) = cut else {
        panic!("the long line reaches the listener")
    };
    assert_eq!(
        text,
        "a".repeat(250),
        "cut to 250 characters, as the client stops typing"
    );

    let probe = b
        .wait_for(
            CHANNEL_SYSTEM,
            Duration::from_secs(3),
            |m| matches!(m, ServerWorldMsg::ChatMessage { text, .. } if text == "still here"),
        )
        .await;
    assert!(
        probe.is_some(),
        "the server stopped serving after the long chat line"
    );
}
