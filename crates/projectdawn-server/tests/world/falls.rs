//! Falls (docs/design/server_fall_damage.md, slice 1): the server simulates the fall off
//! its own ground and charges the landing. Each test boots a server with a ledge of its
//! own (`start_with_terrain`): a plateau west of x = -140 on the row z = 100, north of
//! every camp, with a near-vertical face (0.5 m cells) and the floor beyond. Every test
//! here fails on the server before this slice, which pinned a player to the ground on
//! every tick and knew no fall.

use crate::common::*;
use projectdawn_server::world::{fall, terrain::Terrain};

const Z: f32 = 100.0;
/// The edge: the plateau runs west of it, the floor east.
const EDGE_X: f32 = -140.0;
/// Where each walker starts, on the plateau, six ticks from the edge.
const START_X: f32 = -141.5;
const EAST: Vec3 = Vec3 {
    x: 1.0,
    y: 0.0,
    z: 0.0,
};

/// 8 x 8 cells of 0.5 m centred on the edge (x from -142 to -138): the vertices at or
/// west of the edge carry the plateau, the rest the floor, so the face drops the whole
/// height over half a metre; past the grid the ground is the floor. The town, 150 m
/// east, is untouched (the NPC tables stand on the compiled ground).
fn ledge(height: f32) -> Terrain {
    let mut heights = vec![0.0f32; 9 * 9];
    for iz in 0..9 {
        for ix in 0..=4 {
            heights[iz * 9 + ix] = height;
        }
    }
    Terrain::from_heights(8, 0.5, [EDGE_X, 0.0, Z], 0.0, heights).expect("a valid ledge")
}

/// A level 1 Human Warrior standing on the plateau, with the max HP the server told
/// it on entering (a joiner is sent its own pools once).
async fn walker_on_the_plateau(h: &Harness, user: &str) -> (WorldClient, i64, f32) {
    let (session, char_id, _stale) =
        provision_client(&h.auth_url, user, &format!("{user}a"), "Human", "Warrior").await;
    seed_position(&h.db_url, char_id, START_X, Z).await;
    let token = request_world_token(&h.auth_url, &session, char_id).await;
    let mut c = WorldClient::start(token, &session, char_id).await;
    let health = c
        .wait_for(
            CHANNEL_SYSTEM,
            Duration::from_secs(3),
            |m| matches!(m, ServerWorldMsg::HealthUpdate { id, .. } if *id == char_id as u64),
        )
        .await
        .expect("a joiner is told its own HP");
    let ServerWorldMsg::HealthUpdate { max_hp, .. } = health else {
        unreachable!()
    };
    (c, char_id, max_hp)
}

/// Every message on the system channel for `d`, in the order they arrived.
async fn system_messages(c: &mut WorldClient, d: Duration) -> Vec<ServerWorldMsg> {
    let mut out = Vec::new();
    let end = Instant::now() + d;
    while Instant::now() < end {
        tick_one(&mut c.client, &mut c.transport);
        while let Some(bytes) = c.client.receive_message(CHANNEL_SYSTEM) {
            if let Ok((msg, _)) =
                bincode::serde::decode_from_slice::<ServerWorldMsg, _>(&bytes, bincode_cfg())
            {
                out.push(msg);
            }
        }
        tokio::time::sleep(TICK_DT).await;
    }
    out
}

/// Send Moves east, one a tick, until the server's own Position fan shows the walker
/// past the edge and below the plateau: in the air, on the server's word (a wall-clock
/// count of sends could coalesce under load and leave it grounded).
async fn walk_east_until_airborne(c: &mut WorldClient, char_id: i64, plateau: f32) -> Vec3 {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut seq: u32 = 1;
    loop {
        assert!(
            Instant::now() < deadline,
            "the walker never left the plateau in 5 s"
        );
        c.send_move(seq, EAST);
        seq += 1;
        tick_one(&mut c.client, &mut c.transport);
        let mut own: Option<Vec3> = None;
        while let Some(bytes) = c.client.receive_message(CHANNEL_POSITION) {
            if let Ok((ServerWorldMsg::Position { id, pos, .. }, _)) =
                bincode::serde::decode_from_slice::<ServerWorldMsg, _>(&bytes, bincode_cfg())
            {
                if id == char_id as u64 {
                    own = Some(pos);
                }
            }
        }
        if let Some(pos) = own {
            if pos.x > EDGE_X + 0.1 && pos.y < plateau - 0.3 {
                return pos;
            }
        }
        tokio::time::sleep(TICK_DT).await;
    }
}

/// Off a 10 m ledge the landing costs a fifth of max HP (2.5% a metre past the 2 m
/// safe drop), says so, and the walker ends on the floor past the face. On the previous
/// server the walker stepped down the face pinned to the ground and lost nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_walk_off_a_ten_metre_ledge_costs_a_fifth_of_max_hp() {
    let h = start_with_terrain(ledge(10.0)).await;
    let (mut a, a_char_id, max_hp) = walker_on_the_plateau(&h, "ledgewalker").await;
    let mut seq: u32 = 1;
    let before = own_position_now(&mut a, a_char_id).await;
    assert!(
        (before.y - 10.0).abs() < 0.05,
        "entering, the walker stands on the plateau: {before:?}"
    );

    // Eight Moves east: over the edge at the seventh, then the fall while the
    // held direction runs out; a second of falling lands it on the floor.
    walk_dir(&mut a, &mut seq, EAST, 2.0).await;
    let msgs = system_messages(&mut a, Duration::from_secs(2)).await;

    let expected = fall::damage(10.0, max_hp);
    assert!(expected > 0.0, "a 10 m drop hurts");
    let line = format!("You hit the ground hard for {} damage.", expected as i64);
    assert!(
        msgs.iter()
            .any(|m| matches!(m, ServerWorldMsg::ChatMessage { text, .. } if *text == line)),
        "the landing is announced ({line}); got {msgs:?}"
    );
    assert!(
        msgs.iter()
            .any(|m| matches!(m, ServerWorldMsg::HealthUpdate { id, hp, .. }
            if *id == a_char_id as u64 && (hp - (max_hp - expected)).abs() < 0.01)),
        "the HP bar drops by the formula's {expected} of {max_hp}; got {msgs:?}"
    );
    let after = own_position_now(&mut a, a_char_id).await;
    assert!(
        after.y.abs() < 0.05 && after.x > EDGE_X + 0.5,
        "the walker stands on the floor past the face: {after:?}"
    );
}

/// A Quit sent in mid-air changes nothing: the body lingers linkdead, slides down the
/// face, lands on the floor, takes the damage and is saved with it at the reap. On the
/// previous server the clean leave reaped the body at once, in the air, unhurt.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_quit_in_mid_air_still_takes_the_landing() {
    let h = start_with_terrain(ledge(30.0)).await;
    let (mut a, a_char_id, max_hp) = walker_on_the_plateau(&h, "midairquit").await;
    let in_the_air = walk_east_until_airborne(&mut a, a_char_id, 30.0).await;
    assert!(
        in_the_air.y > 20.0,
        "quit early in the fall: {in_the_air:?}"
    );
    a.send_disconnect();
    a.pump_for(Duration::from_millis(200)).await;
    drop(a);

    // The linger, then the reap's save.
    tokio::time::sleep(projectdawn_server::world::LINKDEAD_SECS + Duration::from_secs(6)).await;
    let pool = db::open(&h.db_url).await.expect("open pool");
    let (hp, x, y): (f64, f64, f64) =
        sqlx::query_as("SELECT hp, pos_x, pos_y FROM characters WHERE id = ?1")
            .bind(a_char_id)
            .fetch_one(&pool)
            .await
            .expect("the character row");
    let expected = fall::damage(30.0, max_hp);
    // Regen during the linger gives a few points back; the landing took
    // seventy percent.
    assert!(
        (hp as f32) < max_hp - expected / 2.0,
        "saved with the landing's damage: hp {hp} of {max_hp}, the landing cost {expected}"
    );
    assert!(
        y.abs() < 0.05 && (x as f32) >= EDGE_X + 0.49,
        "saved on the floor at the foot of the face: ({x:.2}, {y:.2})"
    );
}

/// A fall past the lethal height kills, and the corpse lies where the body landed, on
/// the floor past the face. On the previous server nobody died of a fall.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fall_to_zero_leaves_the_corpse_where_the_body_landed() {
    let h = start_with_terrain(ledge(fall::LETHAL_DROP + 8.0)).await;
    let (mut a, a_char_id, _max_hp) = walker_on_the_plateau(&h, "ledgedeath").await;
    let mut seq: u32 = 1;

    walk_dir(&mut a, &mut seq, EAST, 2.0).await;
    let corpse = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(6), |m| {
            matches!(m, ServerWorldMsg::CorpseSpawn { owner_id, .. } if *owner_id == a_char_id as u64)
        })
        .await
        .expect("the fall kills and the corpse spawns");
    let ServerWorldMsg::CorpseSpawn { pos, .. } = corpse else {
        unreachable!()
    };
    assert!(
        pos.y.abs() < 0.05 && pos.x > EDGE_X + 0.5,
        "the corpse lies on the floor where the body landed: {pos:?}"
    );
}
