//! What stands on the ground (`docs/design/obstacle_collision.md`): the server
//! takes every step itself, so a tree, a rock or a wall stops a body whatever
//! its client believes. Each test boots a server on the flat plate with
//! obstacles of its own (`start_with_obstacles`) on the row z = 100, inside
//! the town's clearing where no camp reaches. Every test here fails on the
//! server before the model, which knew the ground and nothing on it.

use crate::common::*;
use projectdawn_server::world::obstacles::{Obstacles, Prism, BODY_RADIUS, P2};

const Z: f32 = 100.0;
/// A house 10 m square, its west face at x = 10.
const HOUSE: (f32, f32, f32, f32) = (10.0, Z - 5.0, 20.0, Z + 5.0);
/// A trunk 0.3 m in radius, 20 m west of the house, a hand's width off the row
/// so a walk along the row meets its corner and not a flat face.
const TRUNK: (f32, f32, f32) = (-10.0, Z + 0.2, 0.3);

fn yard() -> Obstacles {
    Obstacles::from_prisms(&[
        Prism::rect(HOUSE.0, HOUSE.1, HOUSE.2, HOUSE.3, 0.0, 4.0),
        Prism::round(TRUNK.0, TRUNK.1, TRUNK.2, 6, 0.0, 8.0),
    ])
    .expect("valid test obstacles")
}

/// A plain account's character, standing at (x, z) when it enters.
async fn player_at(h: &Harness, user: &str, x: f32, z: f32) -> (WorldClient, i64) {
    let (session, char_id, _stale) =
        provision_client(&h.auth_url, user, &format!("{user}a"), "Human", "Warrior").await;
    seed_position(&h.db_url, char_id, x, z).await;
    let token = request_world_token(&h.auth_url, &session, char_id).await;
    let mut c = WorldClient::start(token, &session, char_id).await;
    c.pump_for(Duration::from_millis(300)).await;
    (c, char_id)
}

/// Hold `dir` for `ticks` Moves, whatever the server answers: what a client
/// that ignores its own obstacles (or has none) sends. Returns every Position
/// the server fanned for the character on the way, then parks it.
async fn hold_direction(
    c: &mut WorldClient,
    seq: &mut u32,
    char_id: i64,
    dir: Vec3,
    ticks: u32,
) -> Vec<Vec3> {
    let mut seen = Vec::new();
    for tick in 0..ticks + 3 {
        let d = if tick < ticks {
            dir
        } else {
            Vec3 {
                x: 0.0,
                y: 0.0,
                z: 0.0,
            }
        };
        c.send_move(*seq, d);
        *seq += 1;
        tick_one(&mut c.client, &mut c.transport);
        while let Some(bytes) = c.client.receive_message(CHANNEL_POSITION) {
            if let Ok((ServerWorldMsg::Position { id, pos, .. }, _)) =
                bincode::serde::decode_from_slice::<ServerWorldMsg, _>(&bytes, bincode_cfg())
            {
                if id == char_id as u64 {
                    seen.push(pos);
                }
            }
        }
        tokio::time::sleep(TICK_DT).await;
    }
    seen
}

const EAST: Vec3 = Vec3 {
    x: 1.0,
    y: 0.0,
    z: 0.0,
};

/// A client holds "east" for six seconds from 6 m west of a house, as one with
/// no walls of its own would: 30 m of running that would end 14 m past the
/// house's far wall. The server stops the body at the near face and holds it
/// there; no Position is ever inside the house or beyond it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_direction_held_through_a_house_ends_at_its_face() {
    let h = start_with_obstacles(yard()).await;
    let (mut a, a_id) = player_at(&h, "wallrunner", 4.0, Z).await;
    let mut seq: u32 = 1;

    let seen = hold_direction(&mut a, &mut seq, a_id, EAST, 120).await;
    assert!(
        seen.len() > 60,
        "the server fanned {} positions",
        seen.len()
    );
    let furthest = seen.iter().map(|p| p.x).fold(f32::MIN, f32::max);
    assert!(
        furthest <= HOUSE.0 - BODY_RADIUS + 0.01,
        "never past the near face (x = {}): reached x = {furthest}",
        HOUSE.0 - BODY_RADIUS
    );
    let rest = own_position_now(&mut a, a_id).await;
    assert!(
        (rest.x - (HOUSE.0 - BODY_RADIUS)).abs() < 0.02 && (rest.z - Z).abs() < 0.02,
        "rests against the face where it met it: {rest:?}"
    );
}

/// The same held direction at a tree trunk: the body is turned aside, slides
/// round it and carries on east, and no Position on the way is inside the
/// trunk's outline grown by the body.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_body_slides_round_a_trunk_and_carries_on() {
    let h = start_with_obstacles(yard()).await;
    let obstacles = yard();
    let (mut a, a_id) = player_at(&h, "treewalker", -14.0, Z).await;
    let mut seq: u32 = 1;

    let seen = hold_direction(&mut a, &mut seq, a_id, EAST, 60).await;
    for p in &seen {
        assert!(
            obstacles.is_free(P2::new(p.x, p.z), BODY_RADIUS),
            "a position inside the trunk: {p:?}"
        );
    }
    let rest = own_position_now(&mut a, a_id).await;
    assert!(
        rest.x > TRUNK.0 + 2.0,
        "got past the trunk and carried on: {rest:?}"
    );
    // Round its south side (the trunk sits a hand north of the row), and no
    // further aside than the trunk and the body are wide.
    let widest = seen.iter().map(|p| Z - p.z).fold(f32::MIN, f32::max);
    assert!(
        widest > 0.2 && widest < TRUNK.2 + BODY_RADIUS + 0.05,
        "turned aside by {widest} m"
    );
}

/// A character saved inside a house (the scenery changed since it logged
/// out) enters the world beside it, at the nearest free point, and not inside.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_login_inside_a_solid_lands_beside_it() {
    let h = start_with_obstacles(yard()).await;
    let obstacles = yard();
    // 1 m inside the west face.
    let (mut a, a_id) = player_at(&h, "walledin", HOUSE.0 + 1.0, Z).await;
    let stood = own_position_now(&mut a, a_id).await;
    assert!(
        obstacles.is_free(P2::new(stood.x, stood.z), BODY_RADIUS),
        "entered inside the house: {stood:?}"
    );
    assert!(
        (stood.x - (HOUSE.0 - BODY_RADIUS)).abs() < 0.02 && (stood.z - Z).abs() < 0.02,
        "put out through the nearest face: {stood:?}"
    );
}

/// A GM of `class` at `level`, standing at (x, z) when it enters (a dev spawn
/// needs one).
async fn gm_at(
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

/// A Smite at a mob on the far side of a house is refused with the sight line
/// and costs nothing: a wall blocks a cast as a hill does. Walked round to
/// where the house is no longer between them, the same Smite lands. On the
/// server before the model the first Smite landed through the house.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_spell_through_a_house_is_refused_and_lands_round_the_corner() {
    let h = start_with_obstacles(yard()).await;
    let (mut a, a_id) = gm_at(&h, "housecaster", "Cleric", 5, 4.0, Z).await;
    let (mut b, _b_id) = gm_at(&h, "housetarget", "Warrior", 5, 26.0, Z).await;
    let mut seq: u32 = 1;
    let (dummy, dummy_pos) = spawn_dummy(&mut b, "House Dummy", 0, 0.0).await;
    assert!(
        dummy_pos.x > HOUSE.2 + 0.5,
        "the dummy stands east of the house: {dummy_pos:?}"
    );
    a.pump_for(Duration::from_millis(300)).await;

    a.send_cast_spell("Smite", Some(dummy));
    assert!(
        a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::CastFail { caster, reason }
                if *caster == a_id as u64 && reason == "You cannot see your target.")
        })
        .await
        .is_some(),
        "a Smite through the house is refused as unseen"
    );
    let mana = a
        .wait_for(
            CHANNEL_SYSTEM,
            Duration::from_secs(2),
            |m| matches!(m, ServerWorldMsg::ManaUpdate { id, .. } if *id == a_id as u64),
        )
        .await
        .expect("the refusal carries the true mana");
    if let ServerWorldMsg::ManaUpdate { mp, max_mp, .. } = mana {
        assert!(
            (mp - max_mp).abs() < 0.01,
            "a refused cast costs nothing: {mp} of {max_mp}"
        );
    }

    // Round the house's north side to a spot straight north of the dummy:
    // nothing between them there.
    let north = |x: f32| Vec3 {
        x,
        y: 0.0,
        z: Z + 12.0,
    };
    let here = Vec3 {
        x: 4.0,
        y: 0.0,
        z: Z,
    };
    let corner = walk_until_within(&mut a, &mut seq, a_id, here, north(4.0), 0.4).await;
    let stood = walk_until_within(&mut a, &mut seq, a_id, corner, north(dummy_pos.x), 0.4).await;
    assert!(
        (stood.x - dummy_pos.x).abs() < 1.0 && stood.z > HOUSE.3 + 5.0,
        "north of the dummy, clear of the house: {stood:?}"
    );
    // Smite's own cooldown and the walk have both passed.
    a.send_cast_spell("Smite", Some(dummy));
    assert!(
        a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::Hit { attacker, target, .. }
                if *attacker == a_id as u64 && *target == dummy)
        })
        .await
        .is_some(),
        "with nothing between them the Smite lands"
    );
}
