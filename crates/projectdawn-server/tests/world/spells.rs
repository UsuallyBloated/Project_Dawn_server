//! Spells: the cast gates (class, level, memorized gem, cast time, cooldown, global
//! cooldown, movement, fizzles), reach, refusals, heals and nukes, slows, damage over
//! time, binds and ports, food conjuring, and the alignment kits.

use crate::common::*;

/// Track 9 â€” server-side AOE damage. A Magician casts Inferno (5 m
/// radius, 45 base damage, FIRE) with an enemy standing 3 m away. The
/// server's AOE arm searches the caster's AOI neighbourhood, filters by
/// radius, and fans a Hit per victim. Asserts at least one enemy
/// receives a Fire Hit authored by the caster.
///
/// The victim is DEV-SPAWNED after the cast bar has already run, not
/// pulled from a world camp. The old walk-into-a-camp version was
/// flaky for two structural reasons: an enemy in melee during the
/// 2.5 s cast rolls a real interrupt (70% per hit at channeling 0),
/// and the "first hit" it keyed on could be a stale drive-by swing
/// from an enemy that had already leashed home, leaving nothing in
/// radius at resolution. A mob that appears 3 m away AFTER the bar
/// completes can do neither. Dev spawning needs a GM connection:
/// same provision, flip `is_gm`, re-mint token flow as
/// `is_gm_gates_dev_commands`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn aoe_spell_damages_nearby_enemies() {
    let h = start_both().await;

    let (a_session, a_char_id, _stale_token) =
        provision_client(&h.auth_url, "ino", "Inora", "Human", "Magician").await;
    // Inferno requires level 12; provisioned characters start at 1.
    set_char_level(&h.db_url, a_char_id, 12).await;
    let pool = db::open(&h.db_url).await.expect("open pool");
    db::set_account_gm(&pool, "ino", true).await.expect("set is_gm");
    let a_token = request_world_token(&h.auth_url, &a_session, a_char_id).await;

    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;

    // Let enter-world settle so the connection is fully in_world.
    for _ in 0..6 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }

    // Run the cast bar first. `target_id: None` because AOE doesn't take
    // a single target â€” the server searches the caster's AOI for anything
    // in radius at resolution time. Track 10's gate rejects CastSpell
    // without a matching CastStart that ran long enough, so pump
    // CastStart out first (sleeping doesn't advance the transport).
    a.send_cast_start("Inferno", 2.5);
    for _ in 0..3 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }
    a.pump_for(Duration::from_millis(2600)).await;

    // Bar has run; NOW conjure the victim 3 m away. Unique name so the
    // EntitySpawn predicate can't match a world camp mob fanned at
    // connect time.
    a.send_dev_spawn("Inferno Target Dummy", 1, 25.0, 3, 1.8, 8.0);
    let spawn_evt = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::EnemySpawn { mob_name, .. }
                if mob_name == "Inferno Target Dummy")
        })
        .await
        .expect("the dev-spawned victim fans an EnemySpawn");
    let enemy_id: u64 = match spawn_evt {
        ServerWorldMsg::EnemySpawn { id, .. } => id,
        _ => unreachable!(),
    };
    assert!(
        enemy_id >= ENEMY_ID_BASE,
        "victim id must be an enemy id (got {enemy_id}, base {ENEMY_ID_BASE})"
    );

    a.send_cast_spell("Inferno", None);
    for _ in 0..6 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }

    // Inferno must fan at least one Hit with attacker=a, target=enemy,
    // dmg_type=Fire. A spell does exactly its listed number (45, the parity
    // audit of 2026-10-20); only a crit changes it, 1.5x to 2x.
    let aoe_hit = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(5), |m| {
            matches!(
                m,
                ServerWorldMsg::Hit { attacker, target, dmg_type, .. }
                    if *attacker == a_char_id as u64
                    && *target == enemy_id
                    && matches!(dmg_type, DamageType::Fire)
            )
        })
        .await
        .expect("Inferno fans a Fire Hit to the enemy standing 3 m away");
    if let ServerWorldMsg::Hit { amount, crit, .. } = aoe_hit {
        if crit {
            assert!((67..=90).contains(&amount), "a crit of 45: {amount}");
        } else {
            assert_eq!(amount, 45, "Inferno's listed 45, nothing added");
        }
    }
}

/// Track 10 â€” cast-time gate rejects a CastSpell that arrives before
/// the cast bar had time to run. Send CastStart for Fireball (1.5 s
/// cast), then *immediately* send CastSpell. Assert: a CastFail
/// arrives with reason "cast not ready", and no Hit fires for the
/// next half-second.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cast_spell_rejected_before_cast_time() {
    let h = start_both().await;

    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "muu", "Mura", "Human", "Magician").await;

    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;

    // Skip walking â€” we don't need an enemy in range; the gate fails
    // long before target resolution. Sit at spawn.
    for _ in 0..4 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }

    // Forge a CastStart + immediate CastSpell with no real wait.
    a.send_cast_start("Fireball", 1.5);
    a.send_cast_spell("Fireball", Some(ENEMY_ID_BASE));
    for _ in 0..4 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }

    let fail = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(2), |m| {
            matches!(m, ServerWorldMsg::CastFail { caster, reason }
                if *caster == a_char_id as u64 && reason == "cast not ready")
        })
        .await
        .expect("server fans CastFail when CastSpell arrives before cast time elapsed");
    if let ServerWorldMsg::CastFail { reason, .. } = fail {
        assert_eq!(reason, "cast not ready");
    }

    // No Hit should fire â€” gate ran before mana deduction and spell
    // application. (Target id was a stub anyway; we want to confirm
    // no side effects, not target resolution.)
    let stray = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_millis(500), |m| {
            matches!(m, ServerWorldMsg::Hit { attacker, .. } if *attacker == a_char_id as u64)
        })
        .await;
    assert!(stray.is_none(), "rejected cast must not fan a Hit");
}

/// Track 10 â€” cast-time gate passes once the cast bar has actually
/// run. Send CastStart for Healing Wave (1.0 s cast, ALLY target_type
/// â€” self-heal when target_id is None), wait the cast time + jitter,
/// then send CastSpell. Assert: a HealthUpdate arrives for the caster
/// (proof the cast went through the helper that fans on heal apply).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cast_spell_accepted_after_cast_time() {
    let h = start_both().await;

    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "nuu", "Nura", "Human", "Shaman").await;
    // Spirit of the Bear requires level 10; provisioned characters start at 1.
    set_char_level(&h.db_url, a_char_id, 10).await;

    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;

    // Pump a few ticks so the world-enter handshake completes before
    // the server's first regen tick can fire a HealthUpdate we'd
    // mistake for a heal apply later.
    for _ in 0..4 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }

    // Queue CastStart and pump immediately so the packet reaches the
    // server's `cast_set_at` clock before we start counting wait time.
    // tokio::time::sleep alone doesn't advance the transport â€” we'd
    // otherwise be measuring "time until the test got around to
    // ticking" not "time the cast bar ran on the server".
    a.send_cast_start("Spirit of the Bear", 2.0);
    for _ in 0..3 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }
    // Now wait past the cast time (2.0 s) on the server's wall clock.
    // 2100 ms includes a 100 ms cushion above the gate's tolerance.
    a.pump_for(Duration::from_millis(2100)).await;
    a.send_cast_spell("Spirit of the Bear", None);
    for _ in 0..6 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }

    // Spirit of the Bear applies a 4 hps Ã— 18 s HoT; the server fans a
    // BuffSnapshot containing "Spirit of the Bear" once the cast lands.
    // That's the cleanest "cast actually applied server-side" signal
    // and doesn't race against regen ticks like HealthUpdate would.
    let snap = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::BuffSnapshot { target, buffs }
                if *target == a_char_id as u64
                    && buffs.iter().any(|(n, _)| n == "Spirit of the Bear"))
        })
        .await
        .expect("Spirit of the Bear applies once the cast bar has run");
    let _ = snap;

    // No CastFail should have fired.
    let fail = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_millis(300), |m| {
            matches!(m, ServerWorldMsg::CastFail { caster, .. } if *caster == a_char_id as u64)
        })
        .await;
    assert!(fail.is_none(), "well-timed cast must not produce a CastFail");
}

/// Seen in play 2026-10-06: a Wild Boar (aggro 10, so a 20 m leash) nuked
/// from about 22 m stood there and took it. An idle mob turns on an attacker
/// only inside its leash, and spells reach 25 m. Every aggressive mob's leash
/// now has a floor past spell reach. Here a short-sighted mob (aggro 1 m, so
/// a 2 m leash before the floor) is nuked from 20 m and must turn on the
/// caster. The radius is 1 m rather than the boar's 10 because a dev spawn
/// lands 3 m from the requester, and a mob that notices the caster on its
/// first tick would have targeted them before the nuke.
#[tokio::test]
async fn a_mob_nuked_from_beyond_its_old_leash_turns_on_the_caster() {
    let h = start_both().await;
    // Smite: Cleric, level 1, instant.
    let (a_session, a_char_id, _stale_token) =
        provision_client(&h.auth_url, "leashnuke", "Leashnuke", "Human", "Cleric").await;
    let pool = db::open(&h.db_url).await.expect("open pool");
    db::set_account_gm(&pool, "leashnuke", true).await.expect("set is_gm");
    let a_token = request_world_token(&h.auth_url, &a_session, a_char_id).await;
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;

    // Aggro 1 m, no leash given: 2 m before the floor. Speed 0 so the test
    // reads the mob's decision (its target), not a chase.
    a.send_dev_spawn("Timid Skeleton", 1, 500.0, 0, 0.0, 1.0);
    let spawn_evt = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::EnemySpawn { mob_name, .. } if mob_name == "Timid Skeleton")
        })
        .await
        .expect("the skeleton spawns");
    let (mob, mob_pos): (u64, Vec3) = match spawn_evt {
        ServerWorldMsg::EnemySpawn { id, pos, .. } => (id, pos),
        _ => unreachable!(),
    };

    // The dev spawn lands 3 m ahead (-Z at yaw 0); back off to about 20 m:
    // outside the old 16 m leash, inside the 25 m spell reach. 14.5 m of
    // Moves at RUN_SPEED, plus the 3 m and the ~2.5 m the server keeps
    // integrating after the last Move (STALE_MOVE_THRESHOLD).
    let moves = (14.5 / (world::RUN_SPEED * 0.05)).ceil() as u32;
    let mut seq: u32 = 1;
    for _ in 0..moves {
        a.send_move(seq, Vec3 { x: 0.0, y: 0.0, z: 1.0 });
        seq += 1;
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }
    a.pump_for(Duration::from_millis(600)).await; // let the park settle

    // Read the distance off the server's own Position fan rather than
    // trusting the arithmetic: the test is only evidence if the caster really
    // stands past the old leash and inside spell reach.
    let mut own_pos: Option<Vec3> = None;
    while let Some(bytes) = a.client.receive_message(CHANNEL_POSITION) {
        if let Ok((ServerWorldMsg::Position { id, pos, .. }, _)) =
            bincode::serde::decode_from_slice::<ServerWorldMsg, _>(&bytes, bincode_cfg())
        {
            if id == a_char_id as u64 {
                own_pos = Some(pos);
            }
        }
    }
    let own_pos = own_pos.expect("the server fans the caster their own position");
    let dist = ((own_pos.x - mob_pos.x).powi(2) + (own_pos.z - mob_pos.z).powi(2)).sqrt();
    assert!(
        (17.0..25.0).contains(&dist),
        "test setup: caster must stand past the old leash and inside 25 m reach, is at {dist:.1} m"
    );
    // And the mob must not have noticed the caster yet, or a target message
    // from the setup would answer for the nuke (the first cut of this test
    // passed on the old code for exactly that reason).
    while let Some(bytes) = a.client.receive_message(CHANNEL_SYSTEM) {
        if let Ok((ServerWorldMsg::EntityTarget { id, target: Some(_) }, _)) =
            bincode::serde::decode_from_slice::<ServerWorldMsg, _>(&bytes, bincode_cfg())
        {
            assert_ne!(id, mob, "test setup: the mob targeted someone before the nuke");
        }
    }

    a.send_cast_spell("Smite", Some(mob));
    let turned = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::EntityTarget { id, target: Some(t) }
                if *id == mob && *t == a_char_id as u64)
        })
        .await;
    assert!(
        turned.is_some(),
        "a mob hit from inside spell reach must turn on the caster, not stand and take it"
    );
}

/// Spell batch step 0: a corpse casts nothing. The dead-state gate at message
/// arrival already refuses a cast once the death SWEEP has run, so the window
/// is the tick in which hp hits zero: a hit or dev damage lands before the
/// cast step, the death sweep runs after it, and a cast queued in between
/// completed. Here a GM Cleric sends the dev DamageSelf and a Smite in the
/// same batch. The server refuses the Smite privately ("You are dead.") and
/// the dummy is never hit. On the previous code the Smite landed; a self-heal
/// in that gap would have revived the caster past the death penalty.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dead_caster_casts_nothing() {
    let h = start_both().await;
    let (a_session, a_char_id, _stale) =
        provision_client(&h.auth_url, "deadcaster", "Morbid", "Human", "Cleric").await;
    let pool = db::open(&h.db_url).await.expect("open pool");
    db::set_account_gm(&pool, "deadcaster", true).await.expect("set is_gm");
    let a_token = request_world_token(&h.auth_url, &a_session, a_char_id).await;
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;

    a.send_dev_spawn("Charm Dummy", 1, 500.0, 0, 0.0, 0.0);
    let spawn = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::EnemySpawn { mob_name, .. } if mob_name == "Charm Dummy")
        })
        .await
        .expect("the dummy spawns");
    let dummy: u64 = match spawn {
        ServerWorldMsg::EnemySpawn { id, .. } => id,
        _ => unreachable!(),
    };

    // Same batch, no tick between: both arrive in one message drain, the
    // damage first.
    a.send_damage_self(1_000_000);
    a.send_cast_spell("Smite", Some(dummy));
    let refused = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::CastFail { caster, reason }
                if *caster == a_char_id as u64 && reason.contains("dead"))
        })
        .await;
    assert!(refused.is_some(), "a dead caster is told so");
    let hit = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(1), |m| {
            matches!(m, ServerWorldMsg::Hit { attacker, target, .. }
                if *attacker == a_char_id as u64 && *target == dummy)
        })
        .await;
    assert!(hit.is_none(), "and the dummy is never hit");
}

/// Spell batch step 0: a finished cast cannot be held. Healing Light's bar
/// (1.2 s) is started, then released 4 s later: refused as lapsed. Released
/// on time it lands and costs its mana. On the previous code the late release
/// healed: the gate only asked "long enough?", never "too long?".
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_finished_cast_cannot_be_held() {
    let h = start_both().await;
    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "holder", "Holdred", "Human", "Cleric").await;
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    a.pump_for(Duration::from_millis(300)).await;

    a.send_cast_start("Healing Light", 1.2);
    a.pump_for(Duration::from_millis(4_000)).await; // 1.2 s bar, 2 s grace, margin
    a.send_cast_spell("Healing Light", None);
    let lapsed = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::CastFail { caster, reason }
                if *caster == a_char_id as u64 && reason.contains("lapsed"))
        })
        .await;
    assert!(lapsed.is_some(), "a bar held 4 s past a 1.2 s cast is refused");

    // Released on time, the same cast lands: a ManaUpdate carrying the spend.
    a.send_cast_start("Healing Light", 1.2);
    a.pump_for(Duration::from_millis(1_400)).await;
    a.send_cast_spell("Healing Light", None);
    let charged = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::ManaUpdate { id, mp, max_mp }
                if *id == a_char_id as u64 && *mp < *max_mp)
        })
        .await;
    assert!(charged.is_some(), "released in time, the cast lands and costs mana");
}

/// Spell batch step 0: a refusal reaches only the caster and carries the
/// server's mana. A casts Smite twice inside its 3 s cooldown: A gets the
/// cooldown CastFail and then a ManaUpdate; B, in the world beside A, gets no
/// CastFail for A at all. On the previous code B received it too.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refusal_reaches_only_the_caster_with_the_true_mana() {
    let h = start_both().await;
    let (a_session, a_char_id, _stale) =
        provision_client(&h.auth_url, "private", "Privy", "Human", "Cleric").await;
    let (b_session, b_char_id, b_token) =
        provision_client(&h.auth_url, "bystander", "Bystan", "Human", "Warrior").await;
    let pool = db::open(&h.db_url).await.expect("open pool");
    db::set_account_gm(&pool, "private", true).await.expect("set is_gm");
    let a_token = request_world_token(&h.auth_url, &a_session, a_char_id).await;
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    let mut b = WorldClient::start(b_token, &b_session, b_char_id).await;
    a.pump_for(Duration::from_millis(300)).await;
    b.pump_for(Duration::from_millis(300)).await;

    a.send_dev_spawn("Charm Dummy", 1, 500.0, 0, 0.0, 0.0);
    let spawn = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::EnemySpawn { mob_name, .. } if mob_name == "Charm Dummy")
        })
        .await
        .expect("the dummy spawns");
    let dummy: u64 = match spawn {
        ServerWorldMsg::EnemySpawn { id, .. } => id,
        _ => unreachable!(),
    };

    a.send_cast_spell("Smite", Some(dummy));
    let landed = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::Hit { attacker, target, .. }
                if *attacker == a_char_id as u64 && *target == dummy)
        })
        .await;
    assert!(landed.is_some(), "the first Smite lands");
    a.send_cast_spell("Smite", Some(dummy));
    let refused = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::CastFail { caster, reason }
                if *caster == a_char_id as u64 && reason.contains("cooldown"))
        })
        .await;
    assert!(refused.is_some(), "the second, inside the cooldown, is refused to the caster");
    let settled = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(2), |m| {
            matches!(m, ServerWorldMsg::ManaUpdate { id, .. } if *id == a_char_id as u64)
        })
        .await;
    assert!(settled.is_some(), "and the refusal carries the caster's true mana");

    let leaked = b
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(1), |m| {
            matches!(m, ServerWorldMsg::CastFail { caster, .. } if *caster == a_char_id as u64)
        })
        .await;
    assert!(leaked.is_none(), "a bystander hears nothing of another player's refusal");
}

/// Spell batch step 1: friendly spells reach 30 m. A Cleric heals a player
/// 28 m away (the cast lands and costs mana); the same player walked out to
/// about 34 m is refused as too far. On the previous code 28 m was already
/// past the one 25 m reach.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn heals_reach_30_m_and_not_past_it() {
    let h = start_both().await;
    let (a_session, a_char_id, _a_token) =
        provision_client(&h.auth_url, "healer30", "Mendy", "Human", "Cleric").await;
    let (b_session, b_char_id, b_token) =
        provision_client(&h.auth_url, "patient30", "Patient", "Human", "Warrior").await;
    set_char_pos(&h.db_url, b_char_id, 28.0, 0.0).await;
    let pool = db::open(&h.db_url).await.expect("open pool");
    db::set_account_gm(&pool, "healer30", true).await.expect("set is_gm");
    // The GM flag rides the connect token: mint it after the grant.
    let a_token = request_world_token(&h.auth_url, &a_session, a_char_id).await;
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    let mut b = WorldClient::start(b_token, &b_session, b_char_id).await;
    a.pump_for(Duration::from_millis(300)).await;
    b.pump_for(Duration::from_millis(300)).await;

    a.send_cast_start("Healing Light", 1.2);
    a.pump_for(Duration::from_millis(1_400)).await;
    a.send_cast_spell("Healing Light", Some(b_char_id as u64));
    let landed = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::ManaUpdate { id, mp, max_mp }
                if *id == a_char_id as u64 && *mp < *max_mp)
        })
        .await;
    assert!(landed.is_some(), "a heal at 28 m lands and costs its mana");

    // B walks 6 m further east: about 34 m. Healing Light's own cooldown is
    // 5 s, so the second cast waits it out.
    let mut seq: u32 = 1;
    for _ in 0..16 {
        b.send_move(seq, Vec3 { x: 1.0, y: 0.0, z: 0.0 });
        seq += 1;
        tick_one(&mut b.client, &mut b.transport);
        tokio::time::sleep(TICK_DT).await;
    }
    a.pump_for(Duration::from_millis(5_200)).await;
    b.pump_for(Duration::from_millis(100)).await;
    // A level 1 pool (about 16) does not hold two Healing Lights (10 each),
    // and the mana gate runs before the reach gate: top it off first.
    a.send_heal_self();
    a.pump_for(Duration::from_millis(200)).await;
    a.send_cast_start("Healing Light", 1.2);
    a.pump_for(Duration::from_millis(1_400)).await;
    a.send_cast_spell("Healing Light", Some(b_char_id as u64));
    let refused = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::CastFail { caster, reason }
                if *caster == a_char_id as u64 && reason.contains("too far"))
        })
        .await;
    assert!(refused.is_some(), "a heal at about 34 m is refused as too far");
}

/// Spell batch step 1: one attack slow at a time, strongest wins. A Shaman
/// lands Torpor (10%) on a dummy, then Slow (5%) is refused at no cost. On the
/// previous code the weaker slow replaced the stronger one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_weaker_slow_is_refused_while_a_stronger_one_holds() {
    let h = start_both().await;
    let (a_session, a_char_id, _stale) =
        provision_client(&h.auth_url, "slower", "Torpid", "Human", "Shaman").await;
    set_char_level(&h.db_url, a_char_id, 20).await;
    let pool = db::open(&h.db_url).await.expect("open pool");
    db::set_account_gm(&pool, "slower", true).await.expect("set is_gm");
    let a_token = request_world_token(&h.auth_url, &a_session, a_char_id).await;
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;

    a.send_dev_spawn("Charm Dummy", 1, 500.0, 0, 0.0, 0.0);
    let spawn = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::EnemySpawn { mob_name, .. } if mob_name == "Charm Dummy")
        })
        .await
        .expect("the dummy spawns");
    let dummy: u64 = match spawn {
        ServerWorldMsg::EnemySpawn { id, .. } => id,
        _ => unreachable!(),
    };

    a.send_cast_start("Torpor", 1.5);
    a.pump_for(Duration::from_millis(1_700)).await;
    a.send_cast_spell("Torpor", Some(dummy));
    let landed = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::ManaUpdate { id, mp, max_mp }
                if *id == a_char_id as u64 && *mp < *max_mp)
        })
        .await;
    assert!(landed.is_some(), "Torpor lands and costs its mana");

    // Past the global cooldown, so the refusal below is the slow rule's.
    a.pump_for(Duration::from_millis(2_100)).await;
    a.send_cast_start("Slow", 1.0);
    a.pump_for(Duration::from_millis(1_200)).await;
    a.send_cast_spell("Slow", Some(dummy));
    let refused = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::CastFail { caster, reason }
                if *caster == a_char_id as u64 && reason.contains("stronger"))
        })
        .await;
    assert!(refused.is_some(), "Slow is refused while Torpor holds the target");
}

/// Spell batch step 2, the exploit ledger's items 1, 3 and 6: the destination
/// is the server's (bind here, walk on, Gate: the server puts you at the
/// bind); a second Gate inside its cooldown is refused and moves nobody; a
/// mob holding the caster as its target drops them when they Gate. Cleric 8
/// has Gate now (the Bind Affinity list). On the previous code Gate was an
/// unknown spell and Bind Affinity had no arm.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gate_returns_the_caster_to_their_bind_and_wipes_hate() {
    let h = start_both().await;
    let (a_session, a_char_id, _stale) =
        provision_client(&h.auth_url, "gater", "Gatrix", "Human", "Cleric").await;
    set_char_level(&h.db_url, a_char_id, 8).await;
    let pool = db::open(&h.db_url).await.expect("open pool");
    db::set_account_gm(&pool, "gater", true).await.expect("set is_gm");
    let a_token = request_world_token(&h.auth_url, &a_session, a_char_id).await;
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    a.pump_for(Duration::from_millis(300)).await;
    let mut seq: u32 = 1;

    // Bind about 10 m east of the spawn. The whole test stays on the clear
    // stretch of the +X axis (x under 28): past it the Gnoll Raider camp's
    // aggro reaches z = 0, and four gnolls on the caster interrupt a 5 s bar
    // every time (the channeling roll is real).
    walk_east(&mut a, &mut seq, 10.0).await;
    let bind_pos = own_position(&mut a, a_char_id).await.expect("the caster's own Position");
    a.send_cast_start("Bind Affinity", 3.0);
    a.pump_for(Duration::from_millis(3_200)).await;
    a.send_cast_spell("Bind Affinity", None);
    let bound = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::ChatMessage { text, .. } if text.contains("bound"))
        })
        .await;
    assert!(bound.is_some(), "Bind Affinity binds the caster where they stand");

    // Walk on, and provoke a mob so it holds the caster as its target. The
    // dummy cannot move or hit (speed 0, dmg 0), so the bar is never
    // interrupted; its hate is the only thing being tested.
    walk_east(&mut a, &mut seq, 10.0).await;
    let far_pos = own_position(&mut a, a_char_id).await.expect("the caster's own Position");
    assert!(
        far_pos.x - bind_pos.x > 7.0,
        "the caster walked on from the bind ({} to {})",
        bind_pos.x, far_pos.x
    );
    a.send_dev_spawn("Charm Dummy", 1, 500.0, 0, 0.0, 1.0);
    let spawn = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::EnemySpawn { mob_name, .. } if mob_name == "Charm Dummy")
        })
        .await
        .expect("the dummy spawns");
    let dummy: u64 = match spawn {
        ServerWorldMsg::EnemySpawn { id, .. } => id,
        _ => unreachable!(),
    };
    a.send_cast_spell("Smite", Some(dummy));
    let provoked = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::EntityTarget { id, target: Some(t) }
                if *id == dummy && *t == a_char_id as u64)
        })
        .await;
    assert!(provoked.is_some(), "the dummy turns on the caster");

    // Gate: a 5 s bar, then the server moves the caster to the bind and the
    // dummy drops them. The two messages land in the same tick in either
    // order (wait_for discards what it skips), so take them as they come.
    // The Smite started the global cooldown; let it pass first.
    a.pump_for(Duration::from_millis(2_100)).await;
    a.send_cast_start("Gate", 5.0);
    a.pump_for(Duration::from_millis(5_200)).await;
    a.send_cast_spell("Gate", None);
    let mut landed: Option<Vec3> = None;
    let mut dropped = false;
    for _ in 0..2 {
        let msg = a
            .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
                matches!(m, ServerWorldMsg::Teleport { .. })
                    || matches!(m, ServerWorldMsg::EntityTarget { id, target: None } if *id == dummy)
            })
            .await;
        match msg {
            Some(ServerWorldMsg::Teleport { pos }) => landed = Some(pos),
            Some(ServerWorldMsg::EntityTarget { .. }) => dropped = true,
            _ => break,
        }
    }
    let Some(pos) = landed else {
        panic!("Gate teleports the caster");
    };
    assert!(
        (pos.x - bind_pos.x).abs() < 1.0 && (pos.z - bind_pos.z).abs() < 1.0,
        "Gate lands at the bind ({}, {}), got ({}, {})",
        bind_pos.x, bind_pos.z, pos.x, pos.z
    );
    assert!(dropped, "the mob drops the caster as its target: no train");

    // A second Gate inside the 300 s cooldown is refused (past the global
    // cooldown, so the refusal is the spell's own).
    a.pump_for(Duration::from_millis(2_100)).await;
    a.send_cast_start("Gate", 5.0);
    a.pump_for(Duration::from_millis(5_200)).await;
    a.send_cast_spell("Gate", None);
    let refused = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::CastFail { caster, reason }
                if *caster == a_char_id as u64 && reason.contains("cooldown"))
        })
        .await;
    assert!(refused.is_some(), "a second Gate inside the cooldown is refused");
}

/// Ledger item 2: the class gate. A Warrior naming Gate is refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_warrior_cannot_gate() {
    let h = start_both().await;
    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "nogate", "Grunt", "Human", "Warrior").await;
    set_char_level(&h.db_url, a_char_id, 10).await;
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    a.pump_for(Duration::from_millis(300)).await;
    a.send_cast_start("Gate", 5.0);
    a.pump_for(Duration::from_millis(5_200)).await;
    a.send_cast_spell("Gate", None);
    let refused = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::CastFail { caster, reason }
                if *caster == a_char_id as u64 && reason.contains("class"))
        })
        .await;
    assert!(refused.is_some(), "a Warrior cannot cast Gate");
}

/// Ledger item 12 (user call D5): Evacuate moves the caster and the alive
/// group members within friendly reach to the safe area's arrival point, and
/// nobody else. B (grouped, 10 m off) arrives with A; C (not grouped, 5 m off)
/// stays.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn evacuate_moves_the_group_in_reach_and_nobody_else() {
    let h = start_both().await;
    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "evac", "Evalyn", "Human", "Druid").await;
    let (b_session, b_char_id, b_token) =
        provision_client(&h.auth_url, "evacmate", "Mateo", "Human", "Warrior").await;
    let (c_session, c_char_id, c_token) =
        provision_client(&h.auth_url, "evacstranger", "Stray", "Human", "Warrior").await;
    let (d_session, d_char_id, d_token) =
        provision_client(&h.auth_url, "evacfar", "Farrow", "Human", "Warrior").await;
    set_char_level(&h.db_url, a_char_id, 16).await;
    // West of the square on the z = 0 line, which every camp keeps clear
    // (the Bonepile's nearest spawn is 12 m off it); the +X side past x = 28
    // is the Gnoll Raider camp, and a mob on the caster interrupts the bar.
    // D is grouped but 40 m out, past the 30 m friendly reach.
    set_char_pos(&h.db_url, a_char_id, -15.0, 0.0).await;
    set_char_pos(&h.db_url, b_char_id, -25.0, 0.0).await;
    set_char_pos(&h.db_url, c_char_id, -20.0, 0.0).await;
    set_char_pos(&h.db_url, d_char_id, -55.0, 0.0).await;
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    let mut b = WorldClient::start(b_token, &b_session, b_char_id).await;
    let mut c = WorldClient::start(c_token, &c_session, c_char_id).await;
    let mut d = WorldClient::start(d_token, &d_session, d_char_id).await;
    for cl in [&mut a, &mut b, &mut c, &mut d] {
        cl.pump_for(Duration::from_millis(300)).await;
    }
    a.send_group_invite("Mateo");
    a.pump_for(Duration::from_millis(300)).await;
    b.pump_for(Duration::from_millis(300)).await;
    b.send_group_accept(a_char_id as u64);
    a.send_group_invite("Farrow");
    a.pump_for(Duration::from_millis(300)).await;
    d.pump_for(Duration::from_millis(300)).await;
    d.send_group_accept(a_char_id as u64);
    for cl in [&mut a, &mut b, &mut c, &mut d] {
        cl.pump_for(Duration::from_millis(400)).await;
    }

    a.send_cast_start("Evacuate", 5.0);
    pump_all_for(&mut [&mut a, &mut b, &mut c, &mut d], Duration::from_millis(5_200)).await;
    a.send_cast_spell("Evacuate", None);
    let a_tp = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::Teleport { .. })
        })
        .await;
    let Some(ServerWorldMsg::Teleport { pos }) = a_tp else {
        panic!("Evacuate moves the caster");
    };
    assert!(pos.x.abs() < 1.0 && pos.z.abs() < 1.0, "to the town square, got ({}, {})", pos.x, pos.z);
    let b_tp = b
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::Teleport { .. })
        })
        .await;
    assert!(b_tp.is_some(), "the grouped member in reach comes along");
    let c_tp = c
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(1), |m| {
            matches!(m, ServerWorldMsg::Teleport { .. })
        })
        .await;
    assert!(c_tp.is_none(), "a stranger standing beside them stays");
    let d_tp = d
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(1), |m| {
            matches!(m, ServerWorldMsg::Teleport { .. })
        })
        .await;
    assert!(d_tp.is_none(), "a grouped member 40 m out, past friendly reach, stays");
}

/// Track 14 follow-up â€” lifesteal on an ENEMY-target spell heals
/// the caster. Casts Lifetap Rk. II at an enemy in melee range
/// and asserts a HealthUpdate for the caster arrives with hp
/// strictly above the pre-cast baseline by more than natural
/// regen could explain.
///
/// Setup wrinkle: Lifetap Rk. II requires level 10, so we SQL
/// the freshly-provisioned character up to level 10 + drop their
/// hp to a low starting value so the heal contribution is
/// unambiguous against regen ticks.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lifesteal_spell_heals_caster() {
    let h = start_both().await;
    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "lsk", "Lifedrinker", "Human", "Shadow Knight").await;
    let pool = projectdawn_server::db::open(&h.db_url).await.expect("open pool");
    sqlx::query("UPDATE characters SET level = 10, hp = 40.0, mp = 99999.0 WHERE id = ?1")
        .bind(a_char_id)
        .execute(&pool)
        .await
        .expect("bump level + low hp");

    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;

    // Walk toward camp 0's enemy spawn â€” same pattern as the AOE
    // and aggro tests.
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

    // Wait for an enemy hit to confirm an enemy is in range. The
    // attacker id is the spell target.
    let hit_evt = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(35), |m| {
            matches!(m, ServerWorldMsg::Hit { target, .. } if *target == a_char_id as u64)
        })
        .await
        .expect("enemy hits player");
    let enemy_id: u64 = match hit_evt {
        ServerWorldMsg::Hit { attacker, .. } => attacker,
        _ => unreachable!(),
    };
    assert!(enemy_id >= ENEMY_ID_BASE, "attacker is an enemy id");

    // Capture the latest caster HP from a self-HealthUpdate. The
    // walk-and-take-hits phase has fanned several; the most recent
    // is the baseline we need to compare against post-cast.
    let mut baseline_hp: f32 = 0.0;
    while let Some(msg) = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_millis(200), |m| {
            matches!(m, ServerWorldMsg::HealthUpdate { id, .. } if *id == a_char_id as u64)
        })
        .await
    {
        if let ServerWorldMsg::HealthUpdate { hp, .. } = msg {
            baseline_hp = hp;
        }
    }

    // Cast Lifetap Rk. II (cast_time 0.5s, base_damage 50,
    // heal_amount 35).
    a.send_cast_start("Lifetap Rk. II", 0.5);
    for _ in 0..3 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }
    tokio::time::sleep(Duration::from_millis(600)).await;
    a.send_cast_spell("Lifetap Rk. II", Some(enemy_id));
    for _ in 0..6 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }

    // The lifesteal should bump caster HP well above baseline +
    // regen-this-tick. Lifetap Rk. II heals 20 (the parity audit's number);
    // we require at least +10 to stay comfortably above any regen drift the
    // tick loop ran in the meantime.
    let post = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(
                m,
                ServerWorldMsg::HealthUpdate { id, hp, .. }
                    if *id == a_char_id as u64 && *hp >= baseline_hp + 10.0
            )
        })
        .await
        .expect("caster HealthUpdate with lifesteal heal");
    if let ServerWorldMsg::HealthUpdate { hp, .. } = post {
        assert!(
            hp >= baseline_hp + 10.0,
            "expected hp >= {} (baseline {} + at least 10 of the 20 lifesteal); got {}",
            baseline_hp + 10.0,
            baseline_hp,
            hp,
        );
    }
}

/// Track 17.2 â€” per-spell cooldown gate. Cast Healing Wave (6 s
/// cooldown) once successfully, then immediately cast it again
/// following the full cast-time flow. The second cast should land
/// well after the cast-time gate but be rejected by the cooldown
/// gate with a "Spell is on cooldown." reason.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cast_spell_rejected_during_cooldown() {
    let h = start_both().await;

    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "cdc", "Cooler", "Human", "Enchanter").await;
    // Rune requires level 4; provisioned characters start at 1.
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;

    for _ in 0..4 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }

    // First cast â€” full gate flow (CastStart â†’ wait â†’ CastSpell).
    a.send_cast_start("Rune", 0.0);
    for _ in 0..3 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }
    a.pump_for(Duration::from_millis(1100)).await;
    a.send_cast_spell("Rune", None);
    for _ in 0..6 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }

    // Confirm the first cast landed (BuffSnapshot includes Rune).
    let _snap = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::BuffSnapshot { target, buffs }
                if *target == a_char_id as u64
                    && buffs.iter().any(|(n, _)| n == "Rune"))
        })
        .await
        .expect("the first Rune lands (its absorb shows in the snapshot)");

    // The global cooldown (2 s) has to pass, or the second start itself is
    // refused and the per-spell cooldown is never reached.
    a.pump_for(Duration::from_millis(2_100)).await;

    // Second cast â€” same full flow, fired well inside the 6 s
    // cooldown window. Should be rejected with the cooldown reason.
    a.send_cast_start("Rune", 0.0);
    for _ in 0..3 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }
    a.pump_for(Duration::from_millis(1100)).await;
    a.send_cast_spell("Rune", None);
    for _ in 0..6 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }

    let fail = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(2), |m| {
            matches!(m, ServerWorldMsg::CastFail { caster, reason }
                if *caster == a_char_id as u64 && reason == "Spell is on cooldown.")
        })
        .await
        .expect("second cast within the cooldown window is rejected");
    if let ServerWorldMsg::CastFail { reason, .. } = fail {
        assert_eq!(reason, "Spell is on cooldown.");
    }
}

/// Track 17.2 â€” movement-during-cast gate. Send CastStart at the
/// spawn position, then walk well past the 1 m gate threshold while
/// the cast bar runs, then send CastSpell after the cast time elapsed.
/// The cast-time gate passes (enough time elapsed); the movement gate
/// rejects with an "interrupted (moved)" reason.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cast_spell_rejected_when_caster_moved_during_cast() {
    let h = start_both().await;

    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "mvc", "Walker", "Human", "Shaman").await;
    // Healing Wave requires level 4; provisioned characters start at 1.
    set_char_level(&h.db_url, a_char_id, 4).await;
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;

    for _ in 0..4 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }

    a.send_cast_start("Healing Wave", 1.0);
    for _ in 0..3 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }
    // Walk forward for the full cast duration. RUN_SPEED x 1.1 s
    // = 5.5 m on the server, well past the 1 m gate.
    for seq in 1..=22u32 {
        a.send_move(seq, Vec3 { x: 0.0, y: 0.0, z: 1.0 });
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    a.send_cast_spell("Healing Wave", None);
    for _ in 0..6 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }

    let fail = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(2), |m| {
            matches!(m, ServerWorldMsg::CastFail { caster, reason }
                if *caster == a_char_id as u64 && reason.starts_with("interrupted"))
        })
        .await
        .expect("cast is rejected when caster moved past the gate threshold");
    if let ServerWorldMsg::CastFail { reason, .. } = fail {
        assert_eq!(reason, "interrupted (moved)");
    }

    // Confirm the buff did NOT apply â€” there should be no BuffSnapshot
    // containing Healing Wave after the rejection.
    let snap = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_millis(500), |m| {
            matches!(m, ServerWorldMsg::BuffSnapshot { target, buffs }
                if *target == a_char_id as u64
                    && buffs.iter().any(|(n, _)| n == "Healing Wave"))
        })
        .await;
    assert!(snap.is_none(), "movement-interrupted cast must not apply the buff");
}

/// Track 19A â€” a hit on a casting player rolls the channeling-based
/// interrupt; for a non-caster (Warrior, channeling cap = 0) the
/// chance is 1.0 â†’ always interrupted. Using PvP path avoids the
/// flaky AI-walks-into-melee timing. A: Warrior; B: Warrior (any
/// attacker works). Both /pvp on, A "starts" a long cast via a
/// CastStartBroadcast (server doesn't validate class on CastStart;
/// validation happens at CastSpell). B attacks A; server fans
/// CastFail("interrupted (hit during cast)") to A.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cast_interrupted_by_incoming_pvp_hit() {
    let h = start_both().await;
    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "intv", "Inta", "Human", "Warrior").await;
    let (b_session, b_char_id, b_token) =
        provision_client(&h.auth_url, "intw", "Intb", "Human", "Warrior").await;

    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    master_weapon_skill(&h.db_url, b_char_id, "hand_to_hand").await;
    let mut b = WorldClient::start(b_token, &b_session, b_char_id).await;

    // Both clients enter world + flip /pvp on.
    for _ in 0..4 {
        tick_one(&mut a.client, &mut a.transport);
        tick_one(&mut b.client, &mut b.transport);
        tokio::time::sleep(TICK_DT).await;
    }
    a.send_pvp_toggle(true);
    b.send_pvp_toggle(true);
    for _ in 0..3 {
        tick_one(&mut a.client, &mut a.transport);
        tick_one(&mut b.client, &mut b.transport);
        tokio::time::sleep(TICK_DT).await;
    }

    // A "starts" a 5-second cast â€” long enough to keep the cast
    // cache populated until B's attack lands. Warrior has channeling
    // cap = 0, so the on-hit interrupt chance is 1.0 (deterministic).
    a.send_cast_start("Fake Long Cast", 5.0);
    for _ in 0..3 {
        tick_one(&mut a.client, &mut a.transport);
        tick_one(&mut b.client, &mut b.transport);
        tokio::time::sleep(TICK_DT).await;
    }
    // B attacks A. Both spawned at the same DB-loaded position so
    // they're in melee range immediately; bare-fist attack uses the
    // default melee envelope.
    // The server rolls the miss now (combat.rs), and only a hit interrupts.
    // B's hand_to_hand is mastered (a 5% miss), and B swings again, at a
    // legal cadence (an unhasted bare hand is held to 1.65 s; the cache keeps
    // the cast until something ends it), until a hit lands: four misses in a
    // row is about one in 160,000.
    let mut fail = None;
    for _ in 0..4 {
        b.send_attack(a_char_id as u64, "", false, DamageType::Physical);
        for _ in 0..2 {
            tick_one(&mut a.client, &mut a.transport);
            tick_one(&mut b.client, &mut b.transport);
            tokio::time::sleep(TICK_DT).await;
        }
        fail = a
            .wait_for(CHANNEL_SYSTEM, Duration::from_millis(1_750), |m| {
                matches!(m, ServerWorldMsg::CastFail { caster, reason }
                    if *caster == a_char_id as u64
                        && reason == "interrupted (hit during cast)")
            })
            .await;
        if fail.is_some() {
            break;
        }
    }
    let fail = fail.expect("Warrior with no channeling skill is always interrupted on incoming hit");
    if let ServerWorldMsg::CastFail { reason, .. } = fail {
        assert_eq!(reason, "interrupted (hit during cast)");
    }
}

// Silent-refusals closeout (2026-09-30): the cast resolver deducts mana at
// the top of the handler, so an arm that rejected afterwards and returned
// silently charged full price for nothing. The headline case is the one a
// player hits by accident: firing a nuke with no target selected. Assert
// BOTH halves of the fix â€” the refusal line, and the mana coming back.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn enemy_spell_without_a_target_refunds_and_reports() {
    let h = start_both().await;
    // Fireball is Magician-only in spells.toml, and the class/level gate runs
    // BEFORE the target arm â€” roll the class that can actually cast it, or
    // this tests the wrong refusal.
    let (a_session, a_char_id, _stale) =
        provision_client(&h.auth_url, "notarget", "Notarg", "Human", "Magician").await;
    set_char_level(&h.db_url, a_char_id, 12).await;
    let a_token = request_world_token(&h.auth_url, &a_session, a_char_id).await;
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;

    // Let enter-world settle, then latch the starting mana from the seed.
    a.pump_for(Duration::from_millis(400)).await;

    // Run the cast bar, then send the cast with NO target id.
    a.send_cast_start("Fireball", 1.5);
    a.pump_for(Duration::from_millis(1700)).await;
    a.send_cast_spell("Fireball", None);

    // The refusal arrives as a private CastFail (the client's signal to undo
    // its optimistic spend and cooldown).
    let refusal = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::CastFail { reason, .. }
                if reason.contains("need a target"))
        })
        .await;
    assert!(
        refusal.is_some(),
        "a targetless ENEMY cast must answer instead of failing silently"
    );

    // And the mana is given back: the LAST ManaUpdate for this caster must
    // report full mana, not the post-deduct value.
    let mut last_mp: Option<(f32, f32)> = None;
    a.pump_for(Duration::from_millis(600)).await;
    while let Some(bytes) = a.client.receive_message(CHANNEL_SYSTEM) {
        if let Ok((msg, _)) =
            bincode::serde::decode_from_slice::<ServerWorldMsg, _>(&bytes, bincode_cfg())
        {
            if let ServerWorldMsg::ManaUpdate { id, mp, max_mp } = msg {
                if id == a_char_id as u64 {
                    last_mp = Some((mp, max_mp));
                }
            }
        }
    }
    if let Some((mp, max_mp)) = last_mp {
        assert!(
            (mp - max_mp).abs() < 0.01,
            "mana must be refunded after a refused cast (got {mp} of {max_mp})"
        );
    }
}

/// A heal had no range check at all: a healer could keep a friend alive in a
/// dungeon from the safety of town by naming their char id. B is hurt and
/// two cells away; A's heal must be refused, B must not be healed, and A must
/// not pay for it in mana or in cooldown.
#[tokio::test]
async fn ally_heal_refuses_out_of_range_and_costs_nothing() {
    let h = start_both().await;
    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "farheal", "Farheal", "Human", "Shaman").await;
    // Healing Wave is a level 4 Shaman spell: 1.0 s cast, 30 mana, 6 s cooldown.
    set_char_level(&h.db_url, a_char_id, 4).await;
    let (b_session, b_char_id, _stale_token) =
        provision_client(&h.auth_url, "farhurt", "Farhurt", "Human", "Warrior").await;
    let pool = db::open(&h.db_url).await.expect("open pool");
    db::set_account_gm(&pool, "farhurt", true).await.expect("set is_gm");
    // (248, 60): two cells from spawn and clear of every camp. Not (200, 0),
    // which sits inside the Ancient Wraith's aggro circle.
    set_char_pos(&h.db_url, b_char_id, 248.0, 60.0).await;
    let b_token = request_world_token(&h.auth_url, &b_session, b_char_id).await;

    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    let mut b = WorldClient::start(b_token, &b_session, b_char_id).await;
    pump_both_for(&mut a, &mut b, Duration::from_millis(300)).await;

    // Hurt B (a dev command, so B is a GM) and latch the damaged value.
    b.send_damage_self(40);
    let hurt_evt = wait_on_a_pumping_b(&mut b, &mut a, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::HealthUpdate { id, hp, max_hp }
            if *id == b_char_id as u64 && *hp < *max_hp)
    })
    .await
    .expect("B takes the dev damage");
    let hurt_hp = match hurt_evt {
        ServerWorldMsg::HealthUpdate { hp, .. } => hp,
        _ => unreachable!(),
    };

    a.send_cast_start("Healing Wave", 1.0);
    pump_both_for(&mut a, &mut b, Duration::from_millis(1150)).await;
    a.send_cast_spell("Healing Wave", Some(b_char_id as u64));

    wait_on_a_pumping_b(&mut a, &mut b, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::CastFail { reason, .. } if reason.contains("too far away"))
    })
    .await
    .expect("an out-of-range heal is refused with the line");
    // "Costs no mana" means the mana never DIPS: not deducted and handed back,
    // never taken. Any ManaUpdate for A below full in this window is the old
    // deduct-then-refund order (or the heal landing).
    let dipped = wait_on_a_pumping_b(&mut a, &mut b, Duration::from_millis(700), |m| {
        matches!(m, ServerWorldMsg::ManaUpdate { id, mp, max_mp }
            if *id == a_char_id as u64 && *mp < *max_mp - 1.0)
    })
    .await;
    assert!(dipped.is_none(), "a refused heal never takes the mana (saw {dipped:?})");
    // B was not healed (Healing Wave lands 15 at once; regen is 1 or 2 a tick).
    let healed = wait_on_a_pumping_b(&mut b, &mut a, Duration::from_millis(700), |m| {
        matches!(m, ServerWorldMsg::HealthUpdate { id, hp, .. }
            if *id == b_char_id as u64 && *hp >= hurt_hp + 10.0)
    })
    .await;
    assert!(healed.is_none(), "a target two cells away must not be healed");

    // And no cooldown was burned: A heals ITSELF straight away, inside what
    // would have been Healing Wave's 6 s cooldown.
    a.send_cast_start("Healing Wave", 1.0);
    pump_both_for(&mut a, &mut b, Duration::from_millis(1150)).await;
    a.send_cast_spell("Healing Wave", None);
    wait_on_a_pumping_b(&mut a, &mut b, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::ManaUpdate { id, mp, max_mp }
            if *id == a_char_id as u64 && *mp < *max_mp - 1.0)
    })
    .await
    .expect("the next cast goes through: the refused one stamped no cooldown");
}

/// An ENEMY spell on a PLAYER had no range check either: with both sides
/// flagged for PvP, one could nuke the other from across the world. Both are
/// flagged here and two cells apart; the nuke must be refused and must not
/// land.
#[tokio::test]
async fn pvp_nuke_refuses_out_of_range() {
    let h = start_both().await;
    // Smite: level 1 Cleric, instant, 7 damage.
    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "farnuke", "Farnuke", "Human", "Cleric").await;
    let (b_session, b_char_id, b_token) =
        provision_client(&h.auth_url, "farmark", "Farmark", "Human", "Warrior").await;
    // (248, 60): clear of every camp, so the only thing that could hit B is
    // the nuke under test. (200, 0) is inside the Ancient Wraith's aggro.
    set_char_pos(&h.db_url, b_char_id, 248.0, 60.0).await;

    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    let mut b = WorldClient::start(b_token, &b_session, b_char_id).await;
    pump_both_for(&mut a, &mut b, Duration::from_millis(300)).await;
    a.send_pvp_toggle(true);
    b.send_pvp_toggle(true);
    pump_both_for(&mut a, &mut b, Duration::from_millis(300)).await;

    a.send_cast_spell("Smite", Some(b_char_id as u64));
    let line = wait_on_a_pumping_b(&mut a, &mut b, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::CastFail { reason, .. } if reason.contains("too far away"))
    })
    .await
    .expect("an out-of-range PvP nuke is refused with the line");
    if let ServerWorldMsg::CastFail { reason, .. } = line {
        assert!(!reason.contains("Farmark"), "a target out of reach is not named (got {reason:?})");
    }
    let hit = wait_on_a_pumping_b(&mut b, &mut a, Duration::from_millis(700), |m| {
        matches!(m, ServerWorldMsg::Hit { target, .. } if *target == b_char_id as u64)
    })
    .await;
    assert!(hit.is_none(), "a player two cells away must not be hit");
}

/// A cast the server was always going to refuse must cost NOTHING. Until the
/// bind sprint, Bind Affinity had no server arm and the cast took 30 mana,
/// rolled the caster's Alteration skill, and then refunded the mana: a free
/// skill-up attempt every three seconds from an honest hotbar. The pre-flight
/// refuses before any of that. Bind Affinity has an arm now, so the refusal
/// exercised here is the bind rule itself (user call D2, 2026-10-05): a
/// STRANGER cannot be bound, even one standing in the town square; the one
/// line answers absent and ungrouped targets alike, so it is no is-online
/// oracle. The mana never dips at all.
#[tokio::test]
async fn binding_a_stranger_is_refused_and_costs_nothing() {
    let h = start_both().await;
    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "binder", "Binder", "Human", "Cleric").await;
    let (b_session, b_char_id, b_token) =
        provision_client(&h.auth_url, "stranger", "Passerby", "Human", "Warrior").await;
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    let mut b = WorldClient::start(b_token, &b_session, b_char_id).await;
    a.pump_for(Duration::from_millis(300)).await;
    b.pump_for(Duration::from_millis(300)).await;

    a.send_cast_start("Bind Affinity", 3.0);
    pump_both_for(&mut a, &mut b, Duration::from_millis(3150)).await;
    a.send_cast_spell("Bind Affinity", Some(b_char_id as u64));

    // Watch everything that follows the cast for a second: the refusal must
    // arrive, and no ManaUpdate for the caster may show less than full.
    let end = Instant::now() + Duration::from_secs(1);
    let mut refused = false;
    let mut dip: Option<(f32, f32)> = None;
    while Instant::now() < end {
        tick_one(&mut a.client, &mut a.transport);
        while let Some(bytes) = a.client.receive_message(CHANNEL_SYSTEM) {
            match bincode::serde::decode_from_slice::<ServerWorldMsg, _>(&bytes, bincode_cfg()) {
                Ok((ServerWorldMsg::CastFail { reason, .. }, _))
                    if reason.contains("yourself or a group member") =>
                {
                    refused = true;
                }
                Ok((ServerWorldMsg::ManaUpdate { id, mp, max_mp }, _))
                    if id == a_char_id as u64 && mp < max_mp - 1.0 =>
                {
                    dip = Some((mp, max_mp));
                }
                _ => {}
            }
        }
        tokio::time::sleep(TICK_DT).await;
    }
    assert!(refused, "binding a stranger is refused with the line");
    assert!(
        dip.is_none(),
        "it is refused BEFORE the mana comes off, not deducted and refunded (saw {dip:?})"
    );
}

/// Spell batch step 3 (user call of 2026-10-05): a cast that a HIT interrupts
/// costs the spell's mana in proportion to how far the bar had run, and the
/// caster is told the true mana and the amount. Two dummies walk in and beat
/// on a Cleric 10 casting Bind Affinity (30 mana, 3 s); the interrupt is a
/// real ~70% roll per hit, so the bar is retried until one lands (four hits
/// per attempt, three attempts: the odds of none are below one in a
/// million). On the previous code the CastFail arrived with no charge and
/// no line, and the mana stayed full.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_interrupted_cast_costs_mana_in_proportion() {
    let h = start_both().await;
    let (a_session, a_char_id, _stale) =
        provision_client(&h.auth_url, "channel", "Channa", "Human", "Cleric").await;
    set_char_level(&h.db_url, a_char_id, 10).await;
    let pool = db::open(&h.db_url).await.expect("open pool");
    db::set_account_gm(&pool, "channel", true).await.expect("set is_gm");
    let a_token = request_world_token(&h.auth_url, &a_session, a_char_id).await;
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    a.pump_for(Duration::from_millis(300)).await;

    // Two that walk in (3 m/s), notice the caster (aggro 10) and hit for 5.
    for _ in 0..2 {
        a.send_dev_spawn("Charm Dummy", 1, 500.0, 5, 3.0, 10.0);
        a.pump_for(Duration::from_millis(150)).await;
    }

    let mut interrupted = false;
    let mut charged: Option<i32> = None;
    let mut settled: Option<(f32, f32)> = None;
    'attempts: for _ in 0..3 {
        a.send_cast_start("Bind Affinity", 3.0);
        let end = Instant::now() + Duration::from_millis(3_400);
        while Instant::now() < end {
            tick_one(&mut a.client, &mut a.transport);
            while let Some(bytes) = a.client.receive_message(CHANNEL_SYSTEM) {
                let Ok((msg, _)) = bincode::serde::decode_from_slice::<ServerWorldMsg, _>(&bytes, bincode_cfg()) else {
                    continue;
                };
                match msg {
                    ServerWorldMsg::CastFail { caster, reason }
                        if caster == a_char_id as u64 && reason.contains("interrupted") =>
                    {
                        interrupted = true;
                    }
                    ServerWorldMsg::ChatMessage { text, .. } if text.contains("concentration breaks") => {
                        let digits: String = text.chars().filter(|c| c.is_ascii_digit()).collect();
                        charged = digits.parse().ok();
                    }
                    ServerWorldMsg::ManaUpdate { id, mp, max_mp }
                        if id == a_char_id as u64 && interrupted && settled.is_none() =>
                    {
                        settled = Some((mp, max_mp));
                    }
                    _ => {}
                }
            }
            if interrupted && charged.is_some() && settled.is_some() {
                break 'attempts;
            }
            tokio::time::sleep(TICK_DT).await;
        }
        if interrupted {
            break;
        }
    }
    assert!(interrupted, "a hit during the bar interrupts the cast");
    let n = charged.expect("the caster is told what the interrupt cost");
    assert!((1..=10).contains(&n), "the charge is a fraction of Bind Affinity's 10 mana, got {n}");
    let (mp, max_mp) = settled.expect("the true mana follows the CastFail");
    assert!(
        ((max_mp - mp) - n as f32).abs() < 1.5,
        "the mana bar lands the charge below full: max {max_mp}, now {mp}, charged {n}"
    );
}

/// Spell batch step 4 (user call of 2026-10-05): the global cooldown. After
/// an accepted cast no spell may START for GLOBAL_COOLDOWN (2 s). A timed
/// cast is refused at its CastStart, privately and with the true mana, and
/// nothing is cached, so its CastSpell fails the cast-time gate too; an
/// instant cast is refused at its CastSpell. Once the cooldown has run, the
/// same casts go through. Cleric 8: Smite (instant) then Bind Affinity
/// (3 s). On the previous code every cast here lands.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cast_inside_the_global_cooldown_is_refused_at_its_start() {
    let h = start_both().await;
    let (a_session, a_char_id, _stale) =
        provision_client(&h.auth_url, "gcd", "Hasty", "Human", "Cleric").await;
    set_char_level(&h.db_url, a_char_id, 8).await;
    let pool = db::open(&h.db_url).await.expect("open pool");
    db::set_account_gm(&pool, "gcd", true).await.expect("set is_gm");
    let a_token = request_world_token(&h.auth_url, &a_session, a_char_id).await;
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    a.pump_for(Duration::from_millis(300)).await;

    // Something to Smite: a passive dummy 3 m off.
    a.send_dev_spawn("Charm Dummy", 1, 500.0, 0, 0.0, 0.0);
    let spawn = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::EnemySpawn { mob_name, .. } if mob_name == "Charm Dummy")
        })
        .await
        .expect("the dummy spawns");
    let dummy: u64 = match spawn {
        ServerWorldMsg::EnemySpawn { id, .. } => id,
        _ => unreachable!(),
    };
    a.send_cast_spell("Smite", Some(dummy));
    let hit = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::Hit { target, .. } if *target == dummy)
        })
        .await;
    assert!(hit.is_some(), "the first Smite lands");

    // A timed cast started 300 ms later: refused at its start.
    a.pump_for(Duration::from_millis(300)).await;
    a.send_cast_start("Bind Affinity", 3.0);
    let refused = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(2), |m| {
            matches!(m, ServerWorldMsg::CastFail { caster, reason }
                if *caster == a_char_id as u64 && reason.contains("cannot cast again yet"))
        })
        .await;
    assert!(refused.is_some(), "a cast started inside the global cooldown is refused at its start");
    // The client that sends the CastSpell anyway finds no bar on file.
    a.pump_for(Duration::from_millis(3_200)).await;
    a.send_cast_spell("Bind Affinity", None);
    let no_bar = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(2), |m| {
            matches!(m, ServerWorldMsg::CastFail { caster, reason }
                if *caster == a_char_id as u64 && reason.contains("not ready"))
        })
        .await;
    assert!(no_bar.is_some(), "with nothing cached the release fails the cast-time gate");

    // Past the cooldown (the Smite was over 3.5 s ago) the same cast lands.
    a.send_cast_start("Bind Affinity", 3.0);
    a.pump_for(Duration::from_millis(3_200)).await;
    a.send_cast_spell("Bind Affinity", None);
    let bound = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::ChatMessage { text, .. } if text.contains("bound"))
        })
        .await;
    assert!(bound.is_some(), "after the cooldown the cast goes through");

    // And an instant cast inside the cooldown is refused at its CastSpell:
    // Smite again right behind the bind (Smite's own 3 s cooldown is long
    // past), 200 ms later.
    a.pump_for(Duration::from_millis(200)).await;
    a.send_cast_spell("Smite", Some(dummy));
    let instant_refused = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(2), |m| {
            matches!(m, ServerWorldMsg::CastFail { caster, reason }
                if *caster == a_char_id as u64 && reason.contains("cannot cast again yet"))
        })
        .await;
    assert!(instant_refused.is_some(), "an instant cast inside the global cooldown is refused");
}

/// Spell batch step 5: a DoT ticks on the server every three seconds, the
/// caster sees each tick as a named line, and a tick that kills pays exactly
/// what a direct hit would (XP through the one kill step). Druid 6's Entangle
/// (the parity audit's numbers, 2026-10-20): 5 direct (a crit doubles it at
/// most), then 3 per tick (1 dps x 3 s). The dummy has one HP more than the
/// largest direct hit can take, so a tick always does the killing, crit or
/// not. On the previous code Entangle was an unknown spell.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dot_kills_with_the_same_rewards_as_a_hit() {
    let h = start_both().await;
    let (a_session, a_char_id, _stale) =
        provision_client(&h.auth_url, "dotter", "Vinessa", "Human", "Druid").await;
    set_char_level(&h.db_url, a_char_id, 6).await;
    let pool = db::open(&h.db_url).await.expect("open pool");
    db::set_account_gm(&pool, "dotter", true).await.expect("set is_gm");
    let a_token = request_world_token(&h.auth_url, &a_session, a_char_id).await;
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    a.pump_for(Duration::from_millis(300)).await;

    // Entangle's direct hit is its listed 5; a crit takes it to 10 at most.
    a.send_dev_spawn("Charm Dummy", 1, 11.0, 0, 0.0, 1.0);
    let spawn = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::EnemySpawn { mob_name, .. } if mob_name == "Charm Dummy")
        })
        .await
        .expect("the dummy spawns");
    let dummy: u64 = match spawn {
        ServerWorldMsg::EnemySpawn { id, .. } => id,
        _ => unreachable!(),
    };
    a.send_cast_spell("Entangle", Some(dummy));

    // Four ticks of 3 to clear 11 HP: 12 s of ticking, with room to spare.
    let end = Instant::now() + Duration::from_secs(16);
    let (mut ticks_seen, mut died, mut xp) = (0, false, 0i64);
    while Instant::now() < end && !(died && xp > 0) {
        tick_one(&mut a.client, &mut a.transport);
        while let Some(bytes) = a.client.receive_message(CHANNEL_SYSTEM) {
            let Ok((msg, _)) = bincode::serde::decode_from_slice::<ServerWorldMsg, _>(&bytes, bincode_cfg()) else {
                continue;
            };
            match msg {
                ServerWorldMsg::ProcTriggered { target, proc_name, amount, .. }
                    if target == dummy && proc_name == "Entangle" =>
                {
                    assert_eq!(amount, 3, "a tick is dps x 3");
                    ticks_seen += 1;
                }
                ServerWorldMsg::EntityDied { id } if id == dummy => died = true,
                ServerWorldMsg::XpGained { amount, .. } if amount > 0 => xp += amount as i64,
                _ => {}
            }
        }
        tokio::time::sleep(TICK_DT).await;
    }
    assert!(ticks_seen >= 1, "the caster saw the ticks as named lines (saw {ticks_seen})");
    assert!(died, "a tick kills the dummy");
    assert!(xp > 0, "a DoT kill pays XP like a direct hit");
}

/// Ledger rule: your DoTs end when you Gate. Druid 8 (Gate at 8) tags a
/// 500 HP dummy 15 m from the spawn and Gates home; after the Teleport no
/// further tick of theirs lands on it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dot_ends_when_its_caster_gates() {
    let h = start_both().await;
    let (a_session, a_char_id, _stale) =
        provision_client(&h.auth_url, "dotgate", "Rowan", "Human", "Druid").await;
    set_char_level(&h.db_url, a_char_id, 8).await;
    set_char_pos(&h.db_url, a_char_id, 15.0, 0.0).await;
    let pool = db::open(&h.db_url).await.expect("open pool");
    db::set_account_gm(&pool, "dotgate", true).await.expect("set is_gm");
    let a_token = request_world_token(&h.auth_url, &a_session, a_char_id).await;
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    a.pump_for(Duration::from_millis(300)).await;

    a.send_dev_spawn("Charm Dummy", 1, 500.0, 0, 0.0, 1.0);
    let spawn = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::EnemySpawn { mob_name, .. } if mob_name == "Charm Dummy")
        })
        .await
        .expect("the dummy spawns");
    let dummy: u64 = match spawn {
        ServerWorldMsg::EnemySpawn { id, .. } => id,
        _ => unreachable!(),
    };
    a.send_cast_spell("Entangle", Some(dummy));
    let first_tick = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(5), |m| {
            matches!(m, ServerWorldMsg::ProcTriggered { target, proc_name, .. }
                if *target == dummy && proc_name == "Entangle")
        })
        .await;
    assert!(first_tick.is_some(), "the DoT is ticking before the Gate");

    a.send_cast_start("Gate", 5.0);
    a.pump_for(Duration::from_millis(5_200)).await;
    a.send_cast_spell("Gate", None);
    let tp = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| matches!(m, ServerWorldMsg::Teleport { .. }))
        .await;
    assert!(tp.is_some(), "Gate teleports the caster");
    // Entangle runs 18 s; eight more seconds would hold two more ticks.
    let late = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(8), |m| {
            matches!(m, ServerWorldMsg::ProcTriggered { target, proc_name, .. }
                if *target == dummy && proc_name == "Entangle")
        })
        .await;
    assert!(late.is_none(), "no tick lands after the caster Gated");
}

/// Ledger rule: a mob that starts leashing home sheds its DoTs. A chasing
/// dummy (aggro 10, 2 m/s) is tagged and the caster runs 50 m west at
/// 5 m/s; past the 30 m leash the mob turns home and the DoT is gone,
/// so no tick lands in the eight seconds after the run.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dot_ends_when_the_mob_leashes() {
    let h = start_both().await;
    let (a_session, a_char_id, _stale) =
        provision_client(&h.auth_url, "dotleash", "Fern", "Human", "Druid").await;
    set_char_level(&h.db_url, a_char_id, 6).await;
    let pool = db::open(&h.db_url).await.expect("open pool");
    db::set_account_gm(&pool, "dotleash", true).await.expect("set is_gm");
    let a_token = request_world_token(&h.auth_url, &a_session, a_char_id).await;
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    a.pump_for(Duration::from_millis(300)).await;
    let mut seq: u32 = 1;

    // Slow (1 m/s) so the gap opens past the 30 m leash inside five seconds.
    a.send_dev_spawn("Charm Dummy", 1, 500.0, 0, 1.0, 10.0);
    let spawn = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::EnemySpawn { mob_name, .. } if mob_name == "Charm Dummy")
        })
        .await
        .expect("the dummy spawns");
    let dummy: u64 = match spawn {
        ServerWorldMsg::EnemySpawn { id, .. } => id,
        _ => unreachable!(),
    };
    a.send_cast_spell("Entangle", Some(dummy));
    a.pump_for(Duration::from_millis(300)).await;
    // 50 m west at 5 m/s (the west side of the z = 0 line is clear of
    // camps; east past x = 28 is the gnoll camp): about 10 s. The first tick,
    // at three seconds, lands on the way out and is queued; drain it before
    // watching, so only ticks after the leash count.
    walk_dir(&mut a, &mut seq, Vec3 { x: -1.0, y: 0.0, z: 0.0 }, 50.0).await;
    a.wait_for(CHANNEL_SYSTEM, Duration::from_millis(200), |_| false).await;
    let late = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(8), |m| {
            matches!(m, ServerWorldMsg::ProcTriggered { target, proc_name, .. }
                if *target == dummy && proc_name == "Entangle")
        })
        .await;
    assert!(late.is_none(), "a mob walking home carries no DoT: no tick after the leash");
}

/// Design review item (f): a mob finished off by a damage shield paid
/// nothing. Druid 10 wears Thorns (8 per hit); a 7 HP dummy walks in,
/// swings once, and the reflect kills it through the one kill step, so XP
/// arrives. On the previous code the reflect marked it dead and nothing
/// else.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_damage_shield_kill_pays_like_any_other() {
    let h = start_both().await;
    let (a_session, a_char_id, _stale) =
        provision_client(&h.auth_url, "thorny", "Bramble", "Human", "Druid").await;
    set_char_level(&h.db_url, a_char_id, 10).await;
    let pool = db::open(&h.db_url).await.expect("open pool");
    db::set_account_gm(&pool, "thorny", true).await.expect("set is_gm");
    let a_token = request_world_token(&h.auth_url, &a_session, a_char_id).await;
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    a.pump_for(Duration::from_millis(300)).await;

    a.send_cast_start("Thorns", 1.5);
    a.pump_for(Duration::from_millis(1_700)).await;
    a.send_cast_spell("Thorns", None);
    let shielded = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::BuffSnapshot { target, buffs }
                if *target == a_char_id as u64 && buffs.iter().any(|(n, _)| n == "Thorns"))
        })
        .await;
    assert!(shielded.is_some(), "Thorns is up");

    // Walks in at 2 m/s, notices at 10 m, hits for a point: the reflect does 8.
    a.send_dev_spawn("Charm Dummy", 1, 7.0, 1, 2.0, 10.0);
    let spawn = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::EnemySpawn { mob_name, .. } if mob_name == "Charm Dummy")
        })
        .await
        .expect("the dummy spawns");
    let dummy: u64 = match spawn {
        ServerWorldMsg::EnemySpawn { id, .. } => id,
        _ => unreachable!(),
    };
    let end = Instant::now() + Duration::from_secs(10);
    let (mut died, mut xp) = (false, 0i64);
    while Instant::now() < end && !(died && xp > 0) {
        tick_one(&mut a.client, &mut a.transport);
        while let Some(bytes) = a.client.receive_message(CHANNEL_SYSTEM) {
            match bincode::serde::decode_from_slice::<ServerWorldMsg, _>(&bytes, bincode_cfg()) {
                Ok((ServerWorldMsg::EntityDied { id }, _)) if id == dummy => died = true,
                Ok((ServerWorldMsg::XpGained { amount, .. }, _)) if amount > 0 => xp += amount as i64,
                _ => {}
            }
        }
        tokio::time::sleep(TICK_DT).await;
    }
    assert!(died, "the reflect kills the dummy");
    assert!(xp > 0, "and the kill pays XP");
}

/// A spell ported from the client's definitions on 2026-10-05 actually lands.
/// Bloodfire (Sorcerer, level 4, instant) had no entry in spells.toml, so the
/// server refused it as unknown and a Sorcerer's second nuke did nothing
/// online. With the entry it goes through the ordinary ENEMY arm.
#[tokio::test]
async fn a_ported_spell_lands_on_its_target() {
    let h = start_both().await;
    let (a_session, a_char_id, _stale_token) =
        provision_client(&h.auth_url, "bloodfire", "Bloodfire", "Human", "Sorcerer").await;
    set_char_level(&h.db_url, a_char_id, 4).await;
    let pool = db::open(&h.db_url).await.expect("open pool");
    db::set_account_gm(&pool, "bloodfire", true).await.expect("set is_gm");
    let a_token = request_world_token(&h.auth_url, &a_session, a_char_id).await;
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;

    a.send_dev_spawn("Nuke Dummy", 1, 500.0, 0, 0.0, 0.0);
    let spawn_evt = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::EnemySpawn { mob_name, .. } if mob_name == "Nuke Dummy")
        })
        .await
        .expect("the dummy spawns");
    let dummy: u64 = match spawn_evt {
        ServerWorldMsg::EnemySpawn { id, .. } => id,
        _ => unreachable!(),
    };

    a.send_cast_spell("Bloodfire", Some(dummy));
    let hit = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::Hit { attacker, target, .. }
                if *attacker == a_char_id as u64 && *target == dummy)
        })
        .await;
    assert!(hit.is_some(), "Bloodfire must land on the dummy now that the server knows it");
}

/// One cast bar authorises ONE cast. Every CastSpell in a batch used to read
/// the same CastStart cache, so a forged client could send fifty in one
/// datagram after a single bar. Two sent back to back are in one datagram:
/// the first resolves, the second is told to wait.
#[tokio::test]
async fn one_cast_per_caster_per_tick() {
    let h = start_both().await;
    // Smite: level 1 Cleric, instant.
    let (a_session, a_char_id, _stale_token) =
        provision_client(&h.auth_url, "twocast", "Twocast", "Human", "Cleric").await;
    let pool = db::open(&h.db_url).await.expect("open pool");
    db::set_account_gm(&pool, "twocast", true).await.expect("set is_gm");
    let a_token = request_world_token(&h.auth_url, &a_session, a_char_id).await;
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;

    a.send_dev_spawn("Cast Dummy", 1, 500.0, 0, 0.0, 0.0);
    let spawn_evt = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::EnemySpawn { mob_name, .. } if mob_name == "Cast Dummy")
        })
        .await
        .expect("the dummy spawns");
    let dummy: u64 = match spawn_evt {
        ServerWorldMsg::EnemySpawn { id, .. } => id,
        _ => unreachable!(),
    };

    a.send_cast_spell("Smite", Some(dummy));
    a.send_cast_spell("Smite", Some(dummy));
    let end = Instant::now() + Duration::from_secs(1);
    let mut hits = 0u32;
    let mut told = false;
    while Instant::now() < end {
        tick_one(&mut a.client, &mut a.transport);
        while let Some(bytes) = a.client.receive_message(CHANNEL_SYSTEM) {
            match bincode::serde::decode_from_slice::<ServerWorldMsg, _>(&bytes, bincode_cfg()) {
                Ok((ServerWorldMsg::Hit { attacker, target, .. }, _))
                    if attacker == a_char_id as u64 && target == dummy =>
                {
                    hits += 1;
                }
                Ok((ServerWorldMsg::CastFail { reason, .. }, _))
                    if reason.contains("One spell at a time") =>
                {
                    told = true;
                }
                _ => {}
            }
        }
        tokio::time::sleep(TICK_DT).await;
    }
    assert_eq!(hits, 1, "exactly one of the two casts resolves");
    assert!(told, "the extra cast in the same tick is answered, not silently dropped");
}

/// Summon Food and Summon Drink (food slice 2, user call 2026-10-15). A level
/// 1 Magician conjures one Conjured Bread into its bags and is told so; a
/// level 35 Magician conjures ten Conjured Water (the cap). Both arrive as inventory
/// deltas, and the merchant will not buy the result (the item has no vendor
/// value; covered by the SellItem rule).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn summon_food_conjures_bread_that_scales_with_level() {
    const BREAD: &str = "res://data/loot/items/conjured_bread.tres";
    const WATER: &str = "res://data/loot/items/conjured_water.tres";
    let h = start_both().await;
    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "sumfood1", "Sumone", "Human", "Magician").await;
    let (b_session, b_char_id, _) =
        provision_client(&h.auth_url, "sumfood2", "Sumtwo", "Human", "Magician").await;
    set_char_level(&h.db_url, b_char_id, 35).await;
    let b_token = request_world_token(&h.auth_url, &b_session, b_char_id).await;
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    let mut b = WorldClient::start(b_token, &b_session, b_char_id).await;
    pump_both_for(&mut a, &mut b, Duration::from_millis(300)).await;

    a.send_cast_start("Summon Food", 4.0);
    b.send_cast_start("Summon Drink", 4.0);
    pump_both_for(&mut a, &mut b, Duration::from_millis(4200)).await;
    a.send_cast_spell("Summon Food", None);
    b.send_cast_spell("Summon Drink", None);
    pump_both_for(&mut a, &mut b, Duration::from_millis(100)).await;

    let bread = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::InventoryDelta { item_path: Some(p), .. } if p == BREAD)
        })
        .await
        .expect("the level 1 Magician's bread lands in its bags");
    let ServerWorldMsg::InventoryDelta { location, count, .. } = bread else { unreachable!() };
    assert_eq!((location.as_str(), count), ("base", 1), "one loaf at level 1");
    a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(2), |m| {
        matches!(m, ServerWorldMsg::ChatMessage { text, .. } if text == "You conjure Conjured Bread x1.")
    })
    .await
    .expect("and is told");

    let water = b
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::InventoryDelta { item_path: Some(p), .. } if p == WATER)
        })
        .await
        .expect("the level 35 Magician's water lands in its bags");
    let ServerWorldMsg::InventoryDelta { count, .. } = water else { unreachable!() };
    assert_eq!(count, 10, "ten flasks at level 35 (one, plus one per level past 1, capped at ten)");
    a.send_disconnect();
    b.send_disconnect();
    a.pump_for(Duration::from_millis(200)).await;
    b.pump_for(Duration::from_millis(200)).await;
}

/// Client-authority audit fix 3, the user's decision of 2026-10-19 ("move the
/// wisdom bonus on to the server"): a heal adds the caster's WIS x 0.3. B, a
/// GM Warrior beside A, takes 15 dev damage of its 28; A, a Cleric, casts
/// Healing Light (10) on B; B's HP comes back by exactly 10. A heal is its
/// listed number and nothing more (the EQ parity audit of 2026-10-20): fix 3
/// of the client-authority audit used to add WIS x 0.3, which classic EQ
/// never did.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_heal_is_its_listed_number() {
    let h = start_both().await;
    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "wisheal", "Wisa", "Human", "Cleric").await;
    let (b_session, b_char_id, _stale) =
        provision_client(&h.auth_url, "wishurt", "Hurta", "Human", "Warrior").await;
    let pool = db::open(&h.db_url).await.expect("open pool");
    db::set_account_gm(&pool, "wishurt", true).await.expect("set is_gm");
    let b_token = request_world_token(&h.auth_url, &b_session, b_char_id).await;
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    let mut b = WorldClient::start(b_token, &b_session, b_char_id).await;
    pump_both_for(&mut a, &mut b, Duration::from_millis(300)).await;

    b.send_damage_self(15);
    let hurt = wait_on_a_pumping_b(&mut a, &mut b, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::HealthUpdate { id, hp, max_hp } if *id == b_char_id as u64 && *hp < *max_hp)
    })
    .await
    .expect("B is hurt");
    let ServerWorldMsg::HealthUpdate { hp: hurt_hp, .. } = hurt else { unreachable!() };

    a.send_cast_start("Healing Light", 1.2);
    pump_both_for(&mut a, &mut b, Duration::from_millis(1_400)).await;
    a.send_cast_spell("Healing Light", Some(b_char_id as u64));
    let healed = wait_on_a_pumping_b(&mut a, &mut b, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::HealthUpdate { id, hp, .. } if *id == b_char_id as u64 && *hp > hurt_hp)
    })
    .await
    .expect("the heal lands");
    let ServerWorldMsg::HealthUpdate { hp: healed_hp, .. } = healed else { unreachable!() };
    // B's own regen may add a point or two in the 1.4 s before the heal; the
    // old server's 10 + WIS x 0.3 (WIS 24: 17) sits above the window.
    let gained = healed_hp - hurt_hp;
    assert!(
        gained >= 10.0 - 0.01 && gained <= 12.01,
        "healed {gained}; Healing Light's listed 10 and nothing more"
    );
}

/// Per-spell ranges (user, 2026-10-19, on Monsters and Memories' 30 m
/// baseline): a hostile spell reaches 30 m, as friendly ones already did. A
/// GM Cleric backs off from a dummy to about 28 m and Smites it: it lands,
/// where the old server's 25 m hostile reach refused it. Just past 30 m the
/// same Smite is refused, before any mana comes off: the reach is exact
/// (user, 2026-10-27), with no slack for the client's own full-reach check.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_nuke_reaches_thirty_metres() {
    let h = start_both().await;
    let (a_session, a_char_id, _stale) =
        provision_client(&h.auth_url, "reacher", "Reacha", "Human", "Cleric").await;
    let pool = db::open(&h.db_url).await.expect("open pool");
    db::set_account_gm(&pool, "reacher", true).await.expect("set is_gm");
    let a_token = request_world_token(&h.auth_url, &a_session, a_char_id).await;
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    a.pump_for(Duration::from_millis(300)).await;

    a.send_dev_spawn("Reach Dummy", 1, 100_000.0, 0, 0.0, 0.0);
    let spawn = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::EnemySpawn { mob_name, .. } if mob_name == "Reach Dummy")
        })
        .await
        .expect("the dummy spawns");
    let (dummy, dummy_pos) = match spawn {
        ServerWorldMsg::EnemySpawn { id, pos, .. } => (id, pos),
        _ => unreachable!(),
    };
    let origin = Vec3 { x: 0.0, y: 0.0, z: 0.0 };
    let mut seq = 1u32;
    let at = |d: f32| Vec3 { x: dummy_pos.x, y: 0.0, z: dummy_pos.z + d };

    let stood = walk_until_within(&mut a, &mut seq, a_char_id, origin, at(28.0), 0.3).await;
    let dist = ((stood.x - dummy_pos.x).powi(2) + (stood.z - dummy_pos.z).powi(2)).sqrt();
    assert!((27.0..29.5).contains(&dist), "test setup: the caster stands at about 28 m, is at {dist:.1}");
    a.send_cast_spell("Smite", Some(dummy));
    let landed = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::Hit { attacker, target, .. }
                if *attacker == a_char_id as u64 && *target == dummy)
        })
        .await;
    assert!(landed.is_some(), "a Smite at about 28 m lands (the old 25 m reach refused it)");

    // Just past 30 m: refused, the reach being exact (user, 2026-10-27: a
    // 30 m spell is out of range at 30.1). Smite's own 3 s cooldown runs out
    // on the walk.
    a.pump_for(Duration::from_millis(3_100)).await;
    let stood = walk_until_within(&mut a, &mut seq, a_char_id, stood, at(30.6), 0.2).await;
    let dist = ((stood.x - dummy_pos.x).powi(2) + (stood.z - dummy_pos.z).powi(2)).sqrt();
    assert!((30.05..31.0).contains(&dist), "test setup: the caster stands just past 30 m, is at {dist:.2}");
    // A level 1 pool (about 16) does not hold two Smites (12 each), and the
    // mana gate runs before the reach gate: top it off first.
    a.send_heal_self();
    a.pump_for(Duration::from_millis(200)).await;
    a.send_cast_spell("Smite", Some(dummy));
    let refused = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::CastFail { caster, reason }
                if *caster == a_char_id as u64 && reason.contains("too far away"))
        })
        .await;
    assert!(refused.is_some(), "at {dist:.2} m, just past 30, the Smite is refused");
}

/// Client-authority audit fix 5: the server refuses a cast of a spell in no
/// gem, before anything is spent. A Cleric whose bar the server never held
/// (as every character was before migration 0015) enters with an empty,
/// unseeded bar and casts Smite: refused with the line and the true mana.
/// The old server cast any spell the class and level allowed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_spell_not_memorized_is_refused() {
    let h = start_both().await;
    let (session, char_id, token) =
        provision_client(&h.auth_url, "unmem", "Unmema", "Human", "Cleric").await;
    forget_all_spells(&h.db_url, char_id).await;
    let mut a = WorldClient::start(token, &session, char_id).await;
    let (gems, seeded) = next_spell_gems(&mut a).await.expect("the bar arrives on entry");
    assert!(gems.iter().all(|g| g.is_empty()) && !seeded, "an empty bar the server never held");

    a.send_cast_spell("Smite", None);
    let refused = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::CastFail { caster, reason }
                if *caster == char_id as u64 && reason == "You have not memorized that spell.")
        })
        .await;
    assert!(refused.is_some(), "a spell in no gem is refused");
    let mana = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(2), |m| {
            matches!(m, ServerWorldMsg::ManaUpdate { id, .. } if *id == char_id as u64)
        })
        .await;
    assert!(
        matches!(mana, Some(ServerWorldMsg::ManaUpdate { mp, max_mp, .. }) if mp == max_mp),
        "and nothing was spent"
    );
}

/// Memorizing is the server's: seated two seconds, an open gem, and no mana
/// (as in EQ: the mana is paid at the cast; user, 2026-10-26, where it used to
/// cost half the spell's mana). Finishing early is refused, so is standing up
/// in between, so is a gem not yet open; a memorize that runs its time fills
/// the gem with the mana untouched, and the cast then gets past the memorize
/// gate.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn memorizing_takes_two_seated_seconds_and_no_mana() {
    let h = start_both().await;
    let (session, char_id, token) =
        provision_client(&h.auth_url, "memer", "Memera", "Human", "Cleric").await;
    forget_all_spells(&h.db_url, char_id).await;
    let mut a = WorldClient::start(token, &session, char_id).await;
    next_spell_gems(&mut a).await.expect("the bar arrives on entry");
    a.send_sit();
    a.pump_for(Duration::from_millis(200)).await;

    // A gem not open at level 1 (three are).
    a.send_memorize_start(3, "Smite");
    let (gems, _) = next_spell_gems(&mut a).await.expect("a refusal answers with the bar");
    assert!(gems[3].is_empty(), "gem 3 is not open at level 1");

    // Finished too early.
    a.send_memorize_start(0, "Smite");
    a.pump_for(Duration::from_millis(300)).await;
    a.send_memorize_finish();
    let (gems, _) = next_spell_gems(&mut a).await.expect("a refusal answers with the bar");
    assert!(gems[0].is_empty(), "a third of a second is not a memorize");

    // Stood up and sat down again in between.
    a.send_memorize_start(0, "Smite");
    a.pump_for(Duration::from_millis(300)).await;
    a.send_stand();
    a.pump_for(Duration::from_millis(200)).await;
    a.send_sit();
    a.pump_for(Duration::from_millis(1_800)).await;
    a.send_memorize_finish();
    let (gems, _) = next_spell_gems(&mut a).await.expect("a refusal answers with the bar");
    assert!(gems[0].is_empty(), "standing up voids the memorize");

    // The real thing.
    a.send_memorize_start(0, "Smite");
    a.pump_for(Duration::from_millis(2_150)).await;
    a.send_memorize_finish();
    // Everything the server says for a second: the new bar, and no mana
    // taken (a born-full Cleric stays full; the old server sent mp = max - 6).
    let said = collect_system(&mut a, Duration::from_secs(1)).await;
    let bar = said.iter().find_map(|m| match m {
        ServerWorldMsg::SpellGems { gems, seeded } => Some((gems.clone(), *seeded)),
        _ => None,
    });
    let (gems, seeded) = bar.expect("the new bar");
    assert_eq!(gems[0], "Smite");
    assert!(seeded, "the server holds the bar now");
    for m in &said {
        if let ServerWorldMsg::ManaUpdate { id, mp, max_mp } = m {
            if *id == char_id as u64 {
                assert!((max_mp - mp).abs() < 0.01, "memorizing takes no mana: {mp} of {max_mp}");
            }
        }
    }

    a.send_cast_spell("Smite", None);
    let fail = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::CastFail { caster, .. } if *caster == char_id as u64)
        })
        .await;
    let Some(ServerWorldMsg::CastFail { reason, .. }) = fail else { panic!("an untargeted Smite still fails") };
    assert_ne!(reason, "You have not memorized that spell.", "the memorize gate lets it through ({reason})");
}

/// Backstab: three times a swing, 20 stamina fanned at once, refused inside
/// its cooldown; not a Cleric's skill; refused as too tired at 10 stamina.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn backstab_is_tripled_charged_and_gated() {
    let h = start_both().await;
    let (mut a, a_id, dummy) = gm_beside_dummy(&h, "bstab", "Rogue", "Skill Dummy", 0, 0.0).await;
    a.send_use_skill("Backstab", Some(dummy));
    let msgs = a.collect_for(Duration::from_millis(1_500)).await;
    let mut answered = false;
    for m in &msgs {
        match m {
            ServerWorldMsg::Hit { attacker, target, amount, crit, .. } if *attacker == a_id as u64 && *target == dummy => {
                // A plain non-crit fist swing tops out at 6 (1-4 plus STR/5,
                // times the skill's 1.15), so 9 pins the multiplier; a crit
                // (1.5 to 2x) could reach it on its own and proves nothing.
                if !*crit {
                    assert!(*amount >= 9, "fists 1-4 plus STR/5, times three: {amount}");
                }
                answered = true;
            }
            ServerWorldMsg::Miss { attacker, target } if *attacker == a_id as u64 && *target == dummy => answered = true, // the 5% floor
            ServerWorldMsg::SkillFail { reason, .. } => panic!("a Rogue's Backstab beside the dummy was refused: {reason}"),
            _ => {}
        }
    }
    assert!(answered, "the swing was answered");
    let st = msgs.iter().find_map(|m| match m {
        ServerWorldMsg::StaminaUpdate { id, stamina, max } if *id == a_id as u64 => Some((*stamina, *max)),
        _ => None,
    });
    let (stamina, max) = st.expect("the stamina spend fans at once");
    assert!((max - stamina - 20.0).abs() < 0.5, "20 of {max} stamina: {stamina}");
    assert_eq!(skill_answer(&mut a, a_id, "Backstab", dummy).await, Err("You cannot use that yet.".into()));

    let (b_session, b_id, b_token) = provision_client(&h.auth_url, "notarogue", "Notaroguea", "Human", "Cleric").await;
    let mut b = WorldClient::start(b_token, &b_session, b_id).await;
    b.pump_for(Duration::from_millis(300)).await;
    assert_eq!(skill_answer(&mut b, b_id, "Backstab", dummy).await, Err("You cannot use that skill.".into()));

    let (c_session, c_id, _stale) = provision_client(&h.auth_url, "winded", "Windeda", "Human", "Rogue").await;
    set_char_stamina(&h.db_url, c_id, 10.0).await;
    let c_token = request_world_token(&h.auth_url, &c_session, c_id).await;
    let mut c = WorldClient::start(c_token, &c_session, c_id).await;
    c.pump_for(Duration::from_millis(300)).await;
    assert_eq!(skill_answer(&mut c, c_id, "Backstab", dummy).await, Err("You are too tired.".into()));
}

/// The EQ parity audit (2026-10-20): a cast can FIZZLE, rolled by the server
/// at the cast's start on the casting skill against the spell's level (the
/// P99 rule the casting skills follow now, in place of scaling the effect).
/// A level 16 Magician with no evocation at all starts Fireball Rk. III (a
/// level 16 spell, about one start in four fizzling) until one does: the
/// start is refused with EQ's line, a quarter of the spell's mana is gone
/// (42, so 10.5) and the gem is on its refresh (a start inside the global
/// cooldown is refused). Then the instant Frost Bolt Rk. III fizzles the same
/// way from its CastSpell, the mana topped off between tries. On the previous
/// server no cast ever fizzled.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cast_can_fizzle_on_a_low_casting_skill() {
    let h = start_both_opts(true).await;
    let (a_session, a_char_id, _stale) =
        provision_client(&h.auth_url, "fizzler", "Fizzlebeth", "Human", "Magician").await;
    set_char_level(&h.db_url, a_char_id, 16).await;
    set_casting_skill(&h.db_url, a_char_id, "evocation", 0).await;
    let pool = db::open(&h.db_url).await.expect("open pool");
    db::set_account_gm(&pool, "fizzler", true).await.expect("set is_gm");
    let a_token = request_world_token(&h.auth_url, &a_session, a_char_id).await;
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    a.pump_for(Duration::from_millis(300)).await;
    let full_mp = char_data::compute("Human", "Magician", 16).max_mp;
    let me = a_char_id as u64;
    let is_fizzle = |m: &ServerWorldMsg| {
        matches!(m, ServerWorldMsg::CastFail { caster, reason } if *caster == me && reason == "Your spell fizzles!")
    };

    // Timed: a start that does not fizzle only caches a bar the next start
    // replaces, so nothing is spent until one does.
    let mut mp_after_fizzle: Option<f32> = None;
    for _ in 0..40 {
        a.send_cast_start("Fireball Rk. III", 1.5);
        let msgs = a.collect_for(Duration::from_millis(150)).await;
        if let Some(at) = msgs.iter().position(is_fizzle) {
            // The fizzle's own ManaUpdate follows its CastFail on the same
            // ordered channel; the regen fan's periodic update (full mana)
            // can land in the same window just before it, so read the one
            // after the fizzle line, never the first in the batch.
            mp_after_fizzle = msgs[at..].iter().find_map(|m| match m {
                ServerWorldMsg::ManaUpdate { id, mp, .. } if *id == me => Some(*mp),
                _ => None,
            });
            assert!(mp_after_fizzle.is_some(), "the fizzle comes with the true mana");
            break;
        }
    }
    let mp = mp_after_fizzle.expect("forty starts with no evocation at all and not one fizzle");
    assert!((full_mp - mp - 10.5).abs() < 0.01, "a quarter of 42 burned: {full_mp} -> {mp}");
    a.send_cast_start("Fireball Rk. III", 1.5);
    let again = a.collect_for(Duration::from_millis(200)).await;
    assert!(
        again.iter().any(|m| matches!(m, ServerWorldMsg::CastFail { caster, reason } if *caster == me && reason == "You cannot cast again yet.")),
        "the fizzle started the gem's refresh"
    );

    // Instant: Frost Bolt Rk. III at a dummy, rolled at its CastSpell. Every
    // try, landed or fizzled, starts the global cooldown, so each waits it out.
    a.send_dev_spawn("Fizzle Dummy", 1, 100_000.0, 0, 0.0, 0.0);
    let spawn = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::EnemySpawn { mob_name, .. } if mob_name == "Fizzle Dummy")
        })
        .await
        .expect("the dummy spawns");
    let dummy = match spawn {
        ServerWorldMsg::EnemySpawn { id, .. } => id,
        _ => unreachable!(),
    };
    a.pump_alive_for(Duration::from_millis(2_200)).await;
    let mut fizzled = false;
    for _ in 0..40 {
        a.send_heal_self();
        a.send_cast_spell("Frost Bolt Rk. III", Some(dummy));
        let msgs = a.collect_for(Duration::from_millis(2_300)).await;
        assert!(
            !msgs.iter().any(|m| matches!(m, ServerWorldMsg::CastFail { caster, reason } if *caster == me && reason == "Not enough mana.")),
            "the mana was topped off before each try"
        );
        if msgs.iter().any(is_fizzle) {
            fizzled = true;
            break;
        }
    }
    assert!(fizzled, "forty instant casts with no evocation at all and not one fizzle");
}

/// The prestige gate is the server's: a Paladin driven to Evil casts as a
/// Paladin_Fallen. Seeded at -1600 (the server has nowhere else to set it but
/// the row), told so on entering, Death's Embrace (the Fallen kit) lands for
/// its 80 and Righteous Fire (the Paladin's) is refused as not the class's.
/// Both kits sit on the bar; the server decides. Fails on the previous
/// server, which gated on the plain class and knew no Fallen spells.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_paladin_at_evil_casts_the_fallen_kit_and_not_the_paladins() {
    let h = start_both().await;
    let (session, char_id, _stale) =
        provision_client(&h.auth_url, "fallen", "Fallen", "Human", "Paladin").await;
    let pool = db::open(&h.db_url).await.expect("open pool");
    db::set_account_gm(&pool, "fallen", true).await.expect("set is_gm");
    set_char_level(&h.db_url, char_id, 20).await;
    sqlx::query("UPDATE characters SET alignment_score = -1600.0, alignment_seeded = 1 WHERE id = ?1")
        .bind(char_id)
        .execute(&pool)
        .await
        .expect("seed the fall");
    let gems: Vec<(i32, String)> = world::class_spell_names("Paladin")
        .into_iter()
        .chain(world::class_spell_names("Paladin_Fallen"))
        .enumerate()
        .map(|(i, n)| (i as i32, n))
        .collect();
    db::save_spell_gems(&pool, char_id, &gems, true).await.expect("both kits on the bar");
    let token = request_world_token(&h.auth_url, &session, char_id).await;
    let mut a = WorldClient::start(token, &session, char_id).await;
    let seed = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::AlignmentUpdate { .. })
        })
        .await;
    assert!(
        matches!(seed, Some(ServerWorldMsg::AlignmentUpdate { score }) if (score + 1600.0).abs() < 1e-3),
        "told the stored score on entering: {seed:?}"
    );

    a.send_dev_spawn("Practice Dummy", 1, 500.0, 0, 0.0, 0.0);
    let mob = match a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::EnemySpawn { mob_name, pos, .. } if mob_name == "Practice Dummy" && pos.x.abs() < 6.0 && pos.z.abs() < 6.0)
        })
        .await
    {
        Some(ServerWorldMsg::EnemySpawn { id, .. }) => id,
        other => panic!("the dummy spawns: {other:?}"),
    };
    a.send_cast_spell("Death's Embrace", Some(mob));
    let hit = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::Hit { target, .. } if *target == mob)
        })
        .await;
    match hit {
        Some(ServerWorldMsg::Hit { amount, .. }) => assert!(amount >= 80, "the Fallen nuke's 80: {amount}"),
        other => panic!("a Fallen Paladin casts Death's Embrace: {other:?}"),
    }
    a.pump_alive_for(Duration::from_millis(2200)).await; // the global cooldown
    a.send_cast_spell("Righteous Fire", Some(mob));
    let fail = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::CastFail { caster, .. } if *caster == char_id as u64)
        })
        .await;
    match fail {
        Some(ServerWorldMsg::CastFail { reason, .. }) => {
            assert_eq!(reason, "Your class cannot cast that.", "the Paladin's own kit is lost to the Fallen")
        }
        other => panic!("Righteous Fire is refused: {other:?}"),
    }
}

/// The off-side penalty is the server's: a Paladin at Neutral casts at 0.7
/// (the client's old rule, kept by the user). Righteous Fire lists 7; a
/// non-crit hit lands for 4 (7 x 0.7 truncated). A crit is retried after the
/// cooldown. Fails on the previous server at 7.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_neutral_paladin_casts_at_seventy_percent() {
    let h = start_both().await;
    let (session, char_id, _stale) =
        provision_client(&h.auth_url, "lukewarm", "Lukewarm", "Human", "Paladin").await;
    let pool = db::open(&h.db_url).await.expect("open pool");
    db::set_account_gm(&pool, "lukewarm", true).await.expect("set is_gm");
    set_char_level(&h.db_url, char_id, 20).await;
    sqlx::query("UPDATE characters SET alignment_score = 0.0, alignment_seeded = 1 WHERE id = ?1")
        .bind(char_id)
        .execute(&pool)
        .await
        .expect("seed neutral");
    let token = request_world_token(&h.auth_url, &session, char_id).await;
    let mut a = WorldClient::start(token, &session, char_id).await;
    a.send_dev_spawn("Practice Dummy", 1, 500.0, 0, 0.0, 0.0);
    let mob = match a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::EnemySpawn { mob_name, pos, .. } if mob_name == "Practice Dummy" && pos.x.abs() < 6.0 && pos.z.abs() < 6.0)
        })
        .await
    {
        Some(ServerWorldMsg::EnemySpawn { id, .. }) => id,
        other => panic!("the dummy spawns: {other:?}"),
    };
    let mut landed: Option<i32> = None;
    for _ in 0..6 {
        a.send_cast_spell("Righteous Fire", Some(mob));
        let hit = a
            .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
                matches!(m, ServerWorldMsg::Hit { target, .. } if *target == mob)
            })
            .await;
        match hit {
            Some(ServerWorldMsg::Hit { amount, crit: false, .. }) => {
                landed = Some(amount);
                break;
            }
            Some(ServerWorldMsg::Hit { crit: true, .. }) => {
                // Righteous Fire has a 4 s cooldown of its own.
                a.pump_alive_for(Duration::from_millis(4200)).await;
            }
            other => panic!("a Neutral Paladin still casts the Paladin's kit: {other:?}"),
        }
    }
    assert_eq!(landed, Some(4), "7 listed, x 0.7 at Neutral, truncated");
}
