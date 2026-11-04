//! The height model (`docs/design/server_height_model.md`, slice 1): everything stands
//! on the ground, a player's step is the client's, and sight runs through the terrain.
//! Each test boots a server on a ground of its own (`start_with_terrain`): three full
//! lines across the zone, well north of every camp at z = 100. Every test here fails on
//! the server before the model, which zeroed Y, chased by the 3D delta and saw through
//! hills.

use crate::common::*;
use projectdawn_server::world::terrain::Terrain;

/// The tests' row: north of every camp (the nearest, at (-8, 124), keeps
/// its 22 m aggro and 3 m jitter clear of x < -20 on this row).
const Z: f32 = 100.0;
/// The ridge's crest at x = -40, 20 m high: 45° slopes from x = -60 to -20.
const RIDGE_X: f32 = -40.0;
const RIDGE_HEIGHT: f32 = 20.0;
/// A 30° hill with its crest at x = -80: feet at -100 and -60.
const HILL_X: f32 = -80.0;
/// A 70° face either side of x = -140, approached from x = -120.
const FACE_FOOT_X: f32 = -120.0;
const BOW: &str = "res://data/loot/items/hunters_shortbow.tres";

/// 20 x 20 cells of 20 m, like the real world, with the three features as full
/// lines along z (one raised column of vertices each).
fn test_ground() -> Terrain {
    let mut heights = vec![0.0f32; 21 * 21];
    for iz in 0..21 {
        heights[iz * 21 + 3] = 20.0 * 70f32.to_radians().tan();
        heights[iz * 21 + 6] = 20.0 * 30f32.to_radians().tan();
        heights[iz * 21 + 8] = RIDGE_HEIGHT;
    }
    let t = Terrain::from_heights(20, 20.0, [0.0; 3], 0.0, heights).expect("a valid test terrain");
    // The NPC and safe-area tables stand on the COMPILED ground (process-wide
    // tables, one per binary), so a test ground must keep the town flat or
    // the banker and the square would be measured against a height the
    // world does not use. The features begin 20 m west of the last NPC.
    for x in [-2.0, 0.0, 5.0, 8.0, 11.0, 14.0] {
        assert_eq!(
            t.ground(x, 5.0),
            0.0,
            "the town must stay flat under a test ground"
        );
    }
    t
}

/// The ridge's ground at `x` (the slopes are planar).
fn ridge_ground(x: f32) -> f32 {
    (RIDGE_HEIGHT * (1.0 - (x - RIDGE_X).abs() / 20.0)).max(0.0)
}

fn hill_ground(x: f32) -> f32 {
    (20.0 * 30f32.to_radians().tan() * (1.0 - (x - HILL_X).abs() / 20.0)).max(0.0)
}

async fn seed_position(db_url: &str, char_id: i64, x: f32, z: f32) {
    let pool = db::open(db_url).await.expect("open pool");
    sqlx::query("UPDATE characters SET pos_x = ?1, pos_z = ?2 WHERE id = ?3")
        .bind(x)
        .bind(z)
        .bind(char_id)
        .execute(&pool)
        .await
        .expect("seed the position");
}

/// A GM of `class` at `level`, standing at (x, Z) when it enters.
async fn gm_at(h: &Harness, user: &str, class: &str, level: i32, x: f32) -> (WorldClient, i64) {
    let (session, char_id, _stale) =
        provision_client(&h.auth_url, user, &format!("{user}a"), "Human", class).await;
    let pool = db::open(&h.db_url).await.expect("open pool");
    db::set_account_gm(&pool, user, true)
        .await
        .expect("set is_gm");
    if level > 1 {
        set_char_level(&h.db_url, char_id, level).await;
    }
    seed_position(&h.db_url, char_id, x, Z).await;
    let token = request_world_token(&h.auth_url, &session, char_id).await;
    let mut c = WorldClient::start(token, &session, char_id).await;
    c.pump_for(Duration::from_millis(300)).await;
    (c, char_id)
}

/// The newest Position the server has fanned for `char_id` in the next 400 ms.
async fn own_position(c: &mut WorldClient, char_id: i64) -> Vec3 {
    let mut own: Option<Vec3> = None;
    let end = Instant::now() + Duration::from_millis(400);
    while Instant::now() < end {
        tick_one(&mut c.client, &mut c.transport);
        while let Some(bytes) = c.client.receive_message(CHANNEL_POSITION) {
            if let Ok((ServerWorldMsg::Position { id, pos, .. }, _)) =
                bincode::serde::decode_from_slice::<ServerWorldMsg, _>(&bytes, bincode_cfg())
            {
                if id == char_id as u64 {
                    own = Some(pos);
                }
            }
        }
        tokio::time::sleep(TICK_DT).await;
    }
    own.expect("the server fans the player's own position")
}

fn at(x: f32) -> Vec3 {
    Vec3 { x, y: 0.0, z: Z }
}

/// A mob spawned on the crest closes to melee with a player at the foot of the
/// slope. The dummy is passive until the player's Smite from the foot puts
/// them on its threat table; it then comes 22 m down a 45° slope and swings.
/// On the previous server the mob kept the crest's Y while the player stood at
/// 0, so the 3D distance never closed below melee reach and it stood there.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_mob_on_a_rise_closes_to_melee_with_a_player_at_its_foot() {
    let h = start_with_terrain(test_ground()).await;
    let (mut a, a_char_id) = gm_at(&h, "hillfoot", "Cleric", 5, RIDGE_X).await;
    let mut seq: u32 = 1;

    a.send_dev_spawn("Hill Dummy", 1, 100_000.0, 1, 4.0, 0.5);
    let spawn = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::EnemySpawn { mob_name, .. } if mob_name == "Hill Dummy")
        })
        .await
        .expect("the dummy spawns");
    let (dummy, dummy_pos) = match spawn {
        ServerWorldMsg::EnemySpawn { id, pos, .. } => (id, pos),
        _ => unreachable!(),
    };
    assert!(
        (dummy_pos.y - ridge_ground(dummy_pos.x)).abs() < 0.05,
        "a dev spawn stands on the ground: {dummy_pos:?}"
    );

    // Down to the foot, 20 m west: x = -60, the one flat line between the
    // ridge and the hill (the hill's east slope begins there).
    let stood = walk_until_within(&mut a, &mut seq, a_char_id, at(RIDGE_X), at(-60.0), 0.3).await;
    assert!(stood.y < 0.35, "the foot of the slope is at 0: {stood:?}");

    a.send_cast_spell("Smite", Some(dummy));
    let hit = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(12), |m| {
            matches!(m, ServerWorldMsg::Hit { attacker, target, .. }
                if *attacker == dummy && *target == a_char_id as u64)
        })
        .await;
    assert!(
        hit.is_some(),
        "the mob came down the slope and swung at the player at its foot"
    );
}

/// Up a 30° hill a player covers cos²(30°), three quarters, of the flat
/// distance per tick, as the client's body does (tools/probe_ramp_speed.gd),
/// and stands on the slope. On the previous server the step was the flat
/// 0.25 m and Y stayed 0.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_walk_up_a_slope_covers_three_quarters_of_the_flat_distance() {
    let h = start_with_terrain(test_ground()).await;
    let start_x = -99.0;
    let (mut a, a_char_id) = gm_at(&h, "climber", "Warrior", 1, start_x).await;
    let mut seq: u32 = 1;
    let before = own_position(&mut a, a_char_id).await;
    assert!(
        (before.y - hill_ground(before.x)).abs() < 0.05,
        "entering, the player stands on the slope: {before:?}"
    );

    // 40 Moves east, then the held direction runs out (STALE_MOVE_THRESHOLD,
    // up to ten more ticks): 40 to 50 ticks of 0.1875 m, where the flat
    // step would give 0.25.
    walk_dir(
        &mut a,
        &mut seq,
        Vec3 {
            x: 1.0,
            y: 0.0,
            z: 0.0,
        },
        10.0,
    )
    .await;
    let after = own_position(&mut a, a_char_id).await;
    let advanced = after.x - before.x;
    assert!(
        (6.5..=10.0).contains(&advanced),
        "40 to 50 ticks up a 30° slope cover 7.5 to 9.4 m, not {advanced:.2} (the flat step gives 10 to 12.5)"
    );
    assert!(
        (after.y - hill_ground(after.x)).abs() < 0.05 && after.y > 3.0,
        "the player stands on the slope after the walk: {after:?}"
    );
}

/// A face steeper than the slope rule (70°) stops a player at its foot,
/// however long they push: the server's own climb limit, what a modified
/// client without the body's 55° would otherwise walk up.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_step_up_a_face_too_steep_to_climb_is_refused() {
    let h = start_with_terrain(test_ground()).await;
    let (mut a, a_char_id) = gm_at(&h, "wallpush", "Warrior", 1, -110.0).await;
    let mut seq: u32 = 1;

    walk_dir(
        &mut a,
        &mut seq,
        Vec3 {
            x: -1.0,
            y: 0.0,
            z: 0.0,
        },
        20.0,
    )
    .await;
    let after = own_position(&mut a, a_char_id).await;
    assert!(
        after.x >= FACE_FOOT_X - 0.3 && after.x < FACE_FOOT_X + 1.0,
        "the player is held at the foot of the face (x = {FACE_FOOT_X}), not at {:.2}",
        after.x
    );
    assert!(
        after.y.abs() < 0.05,
        "and on the flat ground there: {after:?}"
    );
}

/// A bind made on a slope respawns the player on that slope, and a death
/// there leaves the corpse on the ground. On the previous server both were
/// at Y = 0, inside the hill.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_bind_on_a_slope_respawns_and_leaves_the_corpse_on_the_ground() {
    let h = start_with_terrain(test_ground()).await;
    let x = -52.0;
    let (mut a, a_char_id) = gm_at(&h, "slopebind", "Cleric", 5, x).await;
    let ground = ridge_ground(x);

    a.send_cast_start("Bind Affinity", 3.0);
    a.pump_for(Duration::from_millis(3_200)).await;
    a.send_cast_spell("Bind Affinity", None);
    a.pump_for(Duration::from_millis(400)).await;

    a.send_damage_self(1_000_000);
    let corpse = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(4), |m| {
            matches!(m, ServerWorldMsg::CorpseSpawn { owner_id, .. } if *owner_id == a_char_id as u64)
        })
        .await
        .expect("the corpse spawns");
    let ServerWorldMsg::CorpseSpawn {
        pos: corpse_pos, ..
    } = corpse
    else {
        unreachable!()
    };
    assert!(
        (corpse_pos.y - ground).abs() < 0.05,
        "the corpse lies on the slope ({ground:.2}), not at {corpse_pos:?}"
    );

    a.send_respawn();
    let teleport = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(4), |m| {
            matches!(m, ServerWorldMsg::Teleport { .. })
        })
        .await
        .expect("the respawn teleports");
    let ServerWorldMsg::Teleport { pos } = teleport else {
        unreachable!()
    };
    assert!(
        (pos.x - x).abs() < 0.5 && (pos.y - ground).abs() < 0.05,
        "the respawn lands on the slope at the bind ({x}, {ground:.2}), not at {pos:?}"
    );
}

/// A nuke and a heal across the ridge are refused with EQ's line and cost
/// nothing; from the crest both land. The heal is the user's call (EQEmu
/// exempts beneficial spells).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_nuke_and_a_heal_across_a_ridge_are_refused_and_land_from_the_crest() {
    let h = start_with_terrain(test_ground()).await;
    let (mut a, a_char_id) = gm_at(&h, "ridgecaster", "Cleric", 5, -52.0).await;
    let (mut b, b_char_id) = gm_at(&h, "ridgetarget", "Warrior", 5, -28.0).await;
    let mut seq: u32 = 1;
    let (dummy, _) = spawn_dummy(&mut b, "Ridge Dummy", 0, 0.0).await;
    a.pump_for(Duration::from_millis(300)).await;

    let cannot_see = |m: &ServerWorldMsg| {
        matches!(m, ServerWorldMsg::CastFail { caster, reason }
            if *caster == a_char_id as u64 && reason == "You cannot see your target.")
    };
    a.send_cast_spell("Smite", Some(dummy));
    assert!(
        a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), cannot_see)
            .await
            .is_some(),
        "a Smite through the ridge is refused as unseen"
    );
    let mana = a
        .wait_for(
            CHANNEL_SYSTEM,
            Duration::from_secs(2),
            |m| matches!(m, ServerWorldMsg::ManaUpdate { id, .. } if *id == a_char_id as u64),
        )
        .await
        .expect("the refusal carries the true mana");
    if let ServerWorldMsg::ManaUpdate { mp, max_mp, .. } = mana {
        assert!(
            (mp - max_mp).abs() < 0.01,
            "a refused cast costs nothing: {mp} of {max_mp}"
        );
    }

    a.send_cast_start("Healing Light", 1.2);
    a.pump_for(Duration::from_millis(1_300)).await;
    a.send_cast_spell("Healing Light", Some(b_char_id as u64));
    assert!(
        a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), cannot_see)
            .await
            .is_some(),
        "a heal through the ridge is refused as unseen (the user's call)"
    );

    // From the crest both sides are in view. Smite's own cooldown is 3 s.
    let stood = walk_until_within(&mut a, &mut seq, a_char_id, at(-52.0), at(RIDGE_X), 0.4).await;
    assert!(stood.y > RIDGE_HEIGHT - 1.0, "on the crest: {stood:?}");
    a.send_cast_spell("Smite", Some(dummy));
    assert!(
        a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::Hit { attacker, target, .. }
                if *attacker == a_char_id as u64 && *target == dummy)
        })
        .await
        .is_some(),
        "from the crest the Smite lands"
    );

    b.send_damage_self(15);
    let hurt = b
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::HealthUpdate { id, hp, max_hp } if *id == b_char_id as u64 && *hp < *max_hp)
        })
        .await
        .expect("B is hurt");
    let ServerWorldMsg::HealthUpdate { hp: hurt_hp, .. } = hurt else {
        unreachable!()
    };
    a.pump_for(Duration::from_millis(2_300)).await;
    a.send_cast_start("Healing Light", 1.2);
    a.pump_for(Duration::from_millis(1_300)).await;
    a.send_cast_spell("Healing Light", Some(b_char_id as u64));
    assert!(
        b.wait_for(CHANNEL_SYSTEM, Duration::from_secs(4), |m| {
            matches!(m, ServerWorldMsg::HealthUpdate { id, hp, .. } if *id == b_char_id as u64 && *hp > hurt_hp)
        })
        .await
        .is_some(),
        "from the crest the heal lands"
    );
}

/// An idle mob notices only what it can see: a player 24 m away behind the
/// ridge is unnoticed, and is noticed the moment they reach the crest
/// (EQEmu's CheckWillAggro). On the previous server it noticed at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_idle_mob_behind_a_ridge_notices_a_player_only_at_the_crest() {
    let h = start_with_terrain(test_ground()).await;
    let (mut a, a_char_id) = gm_at(&h, "ridgewalker", "Warrior", 5, -52.0).await;
    let (mut b, _b_char_id) = gm_at(&h, "ridgebait", "Warrior", 5, -28.0).await;
    let mut seq: u32 = 1;
    // Aggressive, rooted (speed 0), harmless. It notices B at spawn and
    // loses B to B's death, then stands idle scanning.
    let (dummy, _) = spawn_dummy(&mut b, "Watch Dummy", 0, 30.0).await;
    b.send_damage_self(1_000_000);
    b.pump_for(Duration::from_millis(600)).await;

    let noticed_me = |m: &ServerWorldMsg| {
        matches!(m, ServerWorldMsg::EntityTarget { id, target }
            if *id == dummy && *target == Some(a_char_id as u64))
    };
    assert!(
        a.wait_for(CHANNEL_SYSTEM, Duration::from_millis(2_500), noticed_me)
            .await
            .is_none(),
        "behind the ridge, inside the aggro radius, the mob does not notice the player"
    );

    let stood = walk_until_within(&mut a, &mut seq, a_char_id, at(-52.0), at(RIDGE_X), 0.4).await;
    assert!(stood.y > RIDGE_HEIGHT - 1.0, "on the crest: {stood:?}");
    assert!(
        a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), noticed_me)
            .await
            .is_some(),
        "on the crest, in view, the mob notices the player"
    );
}

/// A point-blank AoE cast just below the crest hits the mob on the caster's
/// side and skips the one on the far side, inside the radius but out of sight.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_aoe_on_a_ridge_hits_the_near_side_and_skips_the_far() {
    let h = start_with_terrain(test_ground()).await;
    let (mut a, a_char_id) = gm_at(&h, "stormcaller", "Druid", 16, -38.0).await;
    let mut seq: u32 = 1;
    // The far dummy, 2 m down the east slope; the near one 5 m down the west.
    let (far, _) = spawn_dummy(&mut a, "Far Dummy", 0, 0.0).await;
    let stood = walk_until_within(&mut a, &mut seq, a_char_id, at(-38.0), at(-45.0), 0.3).await;
    assert!(stood.x < -44.5, "west of the crest: {stood:?}");
    let (near, _) = spawn_dummy(&mut a, "Near Dummy", 0, 0.0).await;
    let stood = walk_until_within(&mut a, &mut seq, a_char_id, stood, at(-42.5), 0.3).await;
    assert!(
        (-42.9..=-42.1).contains(&stood.x),
        "the caster stands 2.5 m below the crest on the west: {stood:?}"
    );

    a.send_cast_start("Nature's Wrath", 2.0);
    a.pump_for(Duration::from_millis(2_100)).await;
    a.send_cast_spell("Nature's Wrath", None);
    let hits = a.collect_for(Duration::from_secs(2)).await;
    let hit_on = |id: u64| {
        hits.iter().any(|m| {
            matches!(m, ServerWorldMsg::Hit { attacker, target, .. }
                if *attacker == a_char_id as u64 && *target == id)
        })
    };
    assert!(hit_on(near), "the mob on the caster's side is hit");
    assert!(
        !hit_on(far),
        "the mob over the crest, inside the radius, is not: the caster cannot see it"
    );
}

/// A bow needs sight of its target: shots through the ridge miss, every one;
/// from the crest the same archer hits.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_bow_needs_sight_of_its_target() {
    let h = start_with_terrain(test_ground()).await;
    let (mut b, _b_char_id) = gm_at(&h, "bowtarget", "Warrior", 5, -28.0).await;
    let (dummy, _) = spawn_dummy(&mut b, "Arrow Dummy", 0, 0.5).await;
    // The archer, bow in hand and mastered, 22 m from the dummy with the
    // ridge between (24.2 m would be a reach refusal, not a sight one).
    let (session, a_char_id, _stale) =
        provision_client(&h.auth_url, "archer", "archera", "Human", "Warrior").await;
    let pool = db::open(&h.db_url).await.expect("open pool");
    db::set_account_gm(&pool, "archer", true)
        .await
        .expect("set is_gm");
    set_char_level(&h.db_url, a_char_id, 10).await;
    master_weapon_skill(&h.db_url, a_char_id, "archery").await;
    give_char_item(&h.db_url, a_char_id, "equip", 0, BOW, 1).await;
    seed_position(&h.db_url, a_char_id, -50.0, Z).await;
    let token = request_world_token(&h.auth_url, &session, a_char_id).await;
    let mut a = WorldClient::start(token, &session, a_char_id).await;
    a.pump_for(Duration::from_millis(300)).await;
    let mut seq: u32 = 1;

    // Three shots through the ridge, each at the bow's own delay. A mastered
    // archer hits 19 times in 20, so three rolled misses in a row would be
    // one chance in 8,000.
    for shot in 0..3 {
        a.send_attack(dummy, BOW, false, DamageType::Physical);
        let answer = a
            .wait_for(CHANNEL_SYSTEM, Duration::from_secs(2), |m| {
                matches!(m, ServerWorldMsg::Hit { attacker, target, .. } | ServerWorldMsg::Miss { attacker, target }
                    if *attacker == a_char_id as u64 && *target == dummy)
            })
            .await
            .expect("the shot is answered");
        assert!(
            matches!(answer, ServerWorldMsg::Miss { .. }),
            "shot {shot} through the ridge misses, it does not land: {answer:?}"
        );
        a.pump_for(Duration::from_millis(3_300)).await;
    }

    let stood = walk_until_within(&mut a, &mut seq, a_char_id, at(-50.0), at(RIDGE_X), 0.4).await;
    assert!(stood.y > RIDGE_HEIGHT - 1.0, "on the crest: {stood:?}");
    let mut landed = false;
    for _ in 0..6 {
        a.send_attack(dummy, BOW, false, DamageType::Physical);
        if a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(2), |m| {
            matches!(m, ServerWorldMsg::Hit { attacker, target, .. }
                    if *attacker == a_char_id as u64 && *target == dummy)
        })
        .await
        .is_some()
        {
            landed = true;
            break;
        }
        a.pump_for(Duration::from_millis(3_300)).await;
    }
    assert!(landed, "from the crest the arrows land");
}

/// Sight is for noticing and for a new cast, not for staying engaged: a DoT
/// cast with sight keeps ticking after the caster steps behind the ridge
/// (EQ's rule; a guard against the gate ever reaching the ticks).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dot_keeps_ticking_behind_the_ridge() {
    let h = start_with_terrain(test_ground()).await;
    let (mut a, a_char_id) = gm_at(&h, "dotridge", "Druid", 6, -28.0).await;
    let mut seq: u32 = 1;
    let (dummy, _) = spawn_dummy(&mut a, "Dot Dummy", 0, 0.5).await;
    let tick = |m: &ServerWorldMsg| {
        matches!(m, ServerWorldMsg::ProcTriggered { attacker, target, proc_name, .. }
            if *attacker == a_char_id as u64 && *target == dummy && proc_name == "Entangle")
    };
    a.send_cast_spell("Entangle", Some(dummy));
    assert!(
        a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(5), tick)
            .await
            .is_some(),
        "the first tick, in view"
    );
    // Over the crest and down the far side: about 7 s of an 18 s DoT.
    let stood = walk_until_within(&mut a, &mut seq, a_char_id, at(-28.0), at(RIDGE_X), 0.4).await;
    let stood = walk_until_within(&mut a, &mut seq, a_char_id, stood, at(-52.0), 0.4).await;
    assert!(stood.y < RIDGE_HEIGHT - 8.0, "behind the ridge: {stood:?}");
    assert!(
        a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(4), tick)
            .await
            .is_some(),
        "the DoT ticks on behind the ridge"
    );
}
