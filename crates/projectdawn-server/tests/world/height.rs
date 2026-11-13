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

/// A GM of `class` at `level`, standing at (x, z) when it enters.
async fn gm_at_xz(
    h: &Harness,
    user: &str,
    class: &str,
    level: i32,
    x: f32,
    z: f32,
) -> (WorldClient, i64) {
    let (session, char_id, _stale) =
        provision_client(&h.auth_url, user, &format!("{user}a"), "Human", class).await;
    let pool = db::open(&h.db_url).await.expect("open pool");
    db::set_account_gm(&pool, user, true)
        .await
        .expect("set is_gm");
    if level > 1 {
        set_char_level(&h.db_url, char_id, level).await;
    }
    seed_position(&h.db_url, char_id, x, z).await;
    let token = request_world_token(&h.auth_url, &session, char_id).await;
    let mut c = WorldClient::start(token, &session, char_id).await;
    c.pump_for(Duration::from_millis(300)).await;
    (c, char_id)
}

/// A GM of `class` at `level`, standing at (x, Z) when it enters.
async fn gm_at(h: &Harness, user: &str, class: &str, level: i32, x: f32) -> (WorldClient, i64) {
    gm_at_xz(h, user, class, level, x, Z).await
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
    let before = own_position_now(&mut a, a_char_id).await;
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
    let after = own_position_now(&mut a, a_char_id).await;
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
    let after = own_position_now(&mut a, a_char_id).await;
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

// ── Mobs at a face (the first hills, decision 6) ──────────────────────────────

/// A plateau 15 m up for x in [-100, -60] and z in [85, 115]: a ramp up its west side
/// from x = -130 (26.6 degrees, a floor), one-cell faces on the other three (15 m over
/// 5 m, 71.6 degrees). 30 x 30 cells of 5 m centred on (-90, 100), clear of the town.
fn plateau_ground() -> Terrain {
    let mut heights = vec![0.0f32; 31 * 31];
    for iz in 0..31 {
        let z = 25.0 + 5.0 * iz as f32;
        if !(85.0..=115.0).contains(&z) {
            continue;
        }
        for ix in 0..31 {
            let x = -165.0 + 5.0 * ix as f32;
            heights[iz * 31 + ix] = if (-100.0..=-60.0).contains(&x) {
                15.0
            } else if (-130.0..-100.0).contains(&x) {
                15.0 * (x + 130.0) / 30.0
            } else {
                0.0
            };
        }
    }
    let t =
        Terrain::from_heights(30, 5.0, [-90.0, 0.0, 100.0], 0.0, heights).expect("a valid plateau");
    for x in [-2.0, 0.0, 5.0, 8.0, 11.0, 14.0] {
        assert_eq!(
            t.ground(x, 5.0),
            0.0,
            "the town must stay flat under a test ground"
        );
    }
    t
}

/// What a watcher saw while a Cleric on a lip nuked a dummy below a face it cannot climb.
struct FaceRun {
    dummy: u64,
    home: Vec3,
    cleric: u64,
    /// The dummy's positions, with when they arrived.
    positions: Vec<(Instant, Vec3)>,
    /// The watcher's system messages, with when they arrived.
    watched: Vec<(Instant, ServerWorldMsg)>,
    /// The Cleric's system messages.
    cleric_msgs: Vec<ServerWorldMsg>,
    /// The Warrior at the foot, if there was one, and its system messages.
    warrior: Option<u64>,
    warrior_msgs: Vec<ServerWorldMsg>,
}

/// When the second strike goes in, after the first Smite: the dummy has
/// walked to the foot (about 3 s at speed 3), been held past the evade
/// grace, and not yet given up (`MOB_STUCK_SECS` after it reached the foot).
const SECOND_STRIKE: Duration = Duration::from_secs(7);

/// A GM watcher at `watch` spawns a passive dummy (aggro 0.5, speed 3, 500 HP) beside
/// itself; a level 5 Cleric seeded at `lip`, at least `lip_y` up, Smites it, and Smites
/// it again `SECOND_STRIKE` later, while it is held; a Warrior seeded at `warrior_at`,
/// if given, swings at it then too. Every client is serviced every tick for 26 s, with
/// a heartbeat every 2 s (the run outlasts the server's 10 s heartbeat timeout).
async fn nuke_from_a_lip(
    h: &Harness,
    user: &str,
    watch: (f32, f32),
    lip: (f32, f32),
    lip_y: f32,
    warrior_at: Option<(f32, f32)>,
) -> FaceRun {
    let (mut a, _) = gm_at_xz(h, &format!("{user}w"), "Warrior", 1, watch.0, watch.1).await;
    let (b_session, b_char_id, _stale) = provision_client(
        &h.auth_url,
        &format!("{user}c"),
        &format!("{user}ca"),
        "Human",
        "Cleric",
    )
    .await;
    set_char_level(&h.db_url, b_char_id, 5).await;
    seed_position(&h.db_url, b_char_id, lip.0, lip.1).await;
    let b_token = request_world_token(&h.auth_url, &b_session, b_char_id).await;
    let mut b = WorldClient::start(b_token, &b_session, b_char_id).await;
    b.pump_for(Duration::from_millis(300)).await;
    let mut w = match warrior_at {
        Some((x, z)) => {
            let (session, char_id, _stale) = provision_client(
                &h.auth_url,
                &format!("{user}r"),
                &format!("{user}ra"),
                "Human",
                "Warrior",
            )
            .await;
            seed_position(&h.db_url, char_id, x, z).await;
            let token = request_world_token(&h.auth_url, &session, char_id).await;
            let mut c = WorldClient::start(token, &session, char_id).await;
            c.pump_for(Duration::from_millis(300)).await;
            Some((c, char_id as u64))
        }
        None => None,
    };
    let on_lip = own_position_now(&mut b, b_char_id).await;
    assert!(
        on_lip.y > lip_y,
        "the Cleric stands up on the lip: {on_lip:?}"
    );

    a.send_dev_spawn("Face Dummy", 1, 500.0, 0, 3.0, 0.5);
    let spawn = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::EnemySpawn { mob_name, .. } if mob_name == "Face Dummy")
        })
        .await
        .expect("the dummy spawns");
    let ServerWorldMsg::EnemySpawn {
        id: dummy,
        pos: home,
        ..
    } = spawn
    else {
        unreachable!()
    };
    b.pump_for(Duration::from_millis(300)).await;

    b.send_cast_spell("Smite", Some(dummy));

    let mut positions: Vec<(Instant, Vec3)> = Vec::new();
    let mut watched: Vec<(Instant, ServerWorldMsg)> = Vec::new();
    let mut cleric_msgs: Vec<ServerWorldMsg> = Vec::new();
    let mut warrior_msgs: Vec<ServerWorldMsg> = Vec::new();
    let start = Instant::now();
    let mut beat = start;
    let mut struck = false;
    while start.elapsed() < Duration::from_secs(26) {
        if !struck && start.elapsed() >= SECOND_STRIKE {
            struck = true;
            b.send_cast_spell("Smite", Some(dummy));
            if let Some((wc, _)) = w.as_mut() {
                wc.send_attack(dummy, "", false, DamageType::Physical);
            }
        }
        let beat_due = beat.elapsed() >= Duration::from_secs(2);
        let mut clients = vec![&mut a, &mut b];
        if let Some((wc, _)) = w.as_mut() {
            clients.push(wc);
        }
        for c in clients {
            if beat_due {
                c.send_heartbeat();
            }
            tick_one(&mut c.client, &mut c.transport);
        }
        if beat_due {
            beat = Instant::now();
        }
        if let Some((wc, _)) = w.as_mut() {
            while let Some(bytes) = wc.client.receive_message(CHANNEL_SYSTEM) {
                if let Ok((msg, _)) =
                    bincode::serde::decode_from_slice::<ServerWorldMsg, _>(&bytes, bincode_cfg())
                {
                    warrior_msgs.push(msg);
                }
            }
            while wc.client.receive_message(CHANNEL_POSITION).is_some() {}
        }
        let now = Instant::now();
        while let Some(bytes) = a.client.receive_message(CHANNEL_POSITION) {
            if let Ok((ServerWorldMsg::Position { id, pos, .. }, _)) =
                bincode::serde::decode_from_slice::<ServerWorldMsg, _>(&bytes, bincode_cfg())
            {
                if id == dummy {
                    positions.push((now, pos));
                }
            }
        }
        while let Some(bytes) = a.client.receive_message(CHANNEL_SYSTEM) {
            if let Ok((msg, _)) =
                bincode::serde::decode_from_slice::<ServerWorldMsg, _>(&bytes, bincode_cfg())
            {
                watched.push((now, msg));
            }
        }
        while let Some(bytes) = b.client.receive_message(CHANNEL_SYSTEM) {
            if let Ok((msg, _)) =
                bincode::serde::decode_from_slice::<ServerWorldMsg, _>(&bytes, bincode_cfg())
            {
                cleric_msgs.push(msg);
            }
        }
        while b.client.receive_message(CHANNEL_POSITION).is_some() {}
        tokio::time::sleep(TICK_DT).await;
    }
    FaceRun {
        dummy,
        home,
        cleric: b_char_id as u64,
        positions,
        watched,
        cleric_msgs,
        warrior: w.map(|(_, id)| id),
        warrior_msgs,
    }
}

/// The rule, as the watcher saw it: the Smite turned the dummy on the Cleric, it gave up
/// `MOB_STUCK_SECS` or more later, walked home and was back at full health there (told to
/// the watcher), and never hit the Cleric; and it evaded while held (WoW's rule, the
/// user's call of 2026-11-05): the first Smite was the only hit it ever took, and the
/// Warrior's swing (if any) was an Evade. Returns the highest the dummy ever stood.
fn assert_gave_up_and_went_home(run: &FaceRun) -> f32 {
    let dummy = run.dummy;
    let when = |pred: &dyn Fn(&ServerWorldMsg) -> bool| {
        run.watched.iter().find(|(_, m)| pred(m)).map(|(t, _)| *t)
    };
    let chased = when(&|m| matches!(m, ServerWorldMsg::EntityTarget { id, target } if *id == dummy && *target == Some(run.cleric)))
        .expect("the Smite turns the dummy on the Cleric");
    let gave_up =
        when(&|m| matches!(m, ServerWorldMsg::EntityTarget { id, target: None } if *id == dummy))
            .expect("the dummy gives up on a target it cannot reach");
    let held = gave_up.duration_since(chased).as_secs_f32();
    assert!(
        (projectdawn_server::world::MOB_STUCK_SECS..16.0).contains(&held),
        "it gave up {held:.1} s after it began the chase"
    );
    let home = run.home;
    let back = run.positions.iter().find(|(t, p)| {
        *t > gave_up && ((p.x - home.x).powi(2) + (p.z - home.z).powi(2)).sqrt() < 1.5
    });
    assert!(back.is_some(), "the dummy walked home after giving up");
    assert!(
        run.watched.iter().any(|(t, m)| *t > gave_up
            && matches!(m, ServerWorldMsg::HealthUpdate { id, hp, max_hp } if *id == dummy && *hp == *max_hp && *max_hp == 500.0)),
        "the dummy is back at full health at home, and the watcher is told"
    );
    assert!(
        !run.cleric_msgs
            .iter()
            .any(|m| matches!(m, ServerWorldMsg::Hit { attacker, .. } if *attacker == dummy)),
        "the dummy never reached the Cleric"
    );
    let hits = run
        .watched
        .iter()
        .filter(|(_, m)| matches!(m, ServerWorldMsg::Hit { target, .. } if *target == dummy))
        .count();
    assert_eq!(hits, 1, "the first Smite is the only hit the dummy takes");
    if let Some(warrior) = run.warrior {
        assert!(
            run.warrior_msgs.iter().any(|m| matches!(m, ServerWorldMsg::Evade { attacker, target } if *attacker == warrior && *target == dummy)),
            "the Warrior's swing at the held dummy is an Evade"
        );
    }
    run.positions
        .iter()
        .map(|(_, p)| p.y)
        .fold(f32::MIN, f32::max)
}

/// A mob at the foot of a face it cannot climb, its target on top, stops at the foot,
/// gives up after `MOB_STUCK_SECS` and walks home, where it is back at full health for
/// everyone who watches it (EQ's rule). A GM spawns a passive dummy at the plateau's
/// east foot; a Cleric half a metre from the lip Smites it (from there the sight line
/// clears the lip). On the previous server the mob climbed the face pinned to the
/// ground and reached the Cleric in about three seconds.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_mob_at_a_face_gives_up_and_walks_home() {
    let h = start_with_terrain(plateau_ground()).await;
    let run = nuke_from_a_lip(
        &h,
        "face",
        (-45.0, Z),
        (-60.5, Z),
        14.9,
        Some((-53.8, Z + 0.8)),
    )
    .await;
    assert!(
        run.home.x > -50.0 && run.home.y.abs() < 0.05,
        "the dummy spawned east of the face: {:?}",
        run.home
    );
    let highest = assert_gave_up_and_went_home(&run);
    let westmost = run
        .positions
        .iter()
        .map(|(_, p)| p.x)
        .fold(f32::INFINITY, f32::min);
    assert!(
        westmost > -56.0 && highest < 0.5,
        "the dummy never climbed the face (west to x = {westmost:.2}, up to y = {highest:.2})"
    );
    assert!(
        westmost < -54.0,
        "the dummy reached the foot of the face: x = {westmost:.2}"
    );
}

/// The same on the zone's own compiled ground (the 4 km plate), on a perch a player can
/// walk to: the west edge of the ramp up the north side of the Wraith Gate plateau. At
/// z = 408 the ramp is 31 m up and its side is a wall down to the plain (24 m over one
/// cell, 67 degrees). A Cleric at the ramp's edge, (1450.1, 408), Smites a dummy a GM
/// spawns on the plain at (1435, 410), 28 m away; the dummy walks to the foot of the
/// wall (x = 1440) and no further, gives up and walks home. The Cleric's second Smite,
/// while the dummy is held, is refused: it is evading. The dummy starts north of the
/// Cleric's row on purpose: the wall grows to the south (the ramp climbs that way), and
/// from a foot south of the Cleric the wall's taller part hides the dummy, so sight
/// would refuse the Smite before the evade rule could. (The Greywood ridge and the
/// plateau's own cliff are no perch: both are more than a spell's reach high.) Every
/// other integration test runs on a flat plate; this one holds the shipped ground.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_mob_below_the_plateau_ramp_gives_up_on_a_player_on_its_edge() {
    let compiled = projectdawn_server::world::terrain::compiled().clone();
    let foot_y = compiled.ground(1438.0, 408.0);
    let h = start_with_terrain(compiled).await;
    // A dev spawn lands 3 m south of the GM (yaw 0): the watcher at z = 413.
    let run = nuke_from_a_lip(&h, "ramp", (1435.0, 413.0), (1450.1, 408.0), 30.0, None).await;
    let highest = assert_gave_up_and_went_home(&run);
    // The perch itself: the ramp's edge sees the wall's foot, so the second Smite
    // reaches the evade rule (on the plateau's own cliff the lip cannot see the
    // foot, and sight refuses it first).
    assert!(
        run.cleric_msgs.iter().any(|m| matches!(m, ServerWorldMsg::CastFail { reason, .. } if reason == "Your target is evading.")),
        "the Smite at the held dummy is refused: it is evading (refusals: {:?})",
        run.cleric_msgs
            .iter()
            .filter_map(|m| match m {
                ServerWorldMsg::CastFail { reason, .. } => Some(reason.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
    );
    let eastmost = run
        .positions
        .iter()
        .map(|(_, p)| p.x)
        .fold(f32::NEG_INFINITY, f32::max);
    assert!(
        eastmost < 1440.5 && highest < foot_y + 1.5,
        "the dummy never climbed the wall (east to x = {eastmost:.2}, up to y = {highest:.2}, the plain is at {foot_y:.2})"
    );
    assert!(
        eastmost > 1438.5,
        "the dummy reached the foot of the wall: x = {eastmost:.2}"
    );
}
