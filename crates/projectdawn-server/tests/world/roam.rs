//! Wandering wildlife (docs/design/wandering_wildlife.md): animals that live in a region
//! instead of at a spawn point. Each test runs a small roam table of its own on an 800 m
//! flat plate (`start_with_roamers`): the town clearance is 150 m, so the regions sit
//! 330 m south of the town, 290 m from the nearest camp spawn. Every other harness runs
//! with roaming off. Every test here fails on the server before wandering wildlife, which
//! had no roam table and never moved an idle mob.

use crate::common::*;
use projectdawn_server::world::terrain::Terrain;
use std::collections::HashMap;

/// The middle of every test region.
const REGION: Vec3 = Vec3 {
    x: 0.0,
    y: 0.0,
    z: 330.0,
};

fn plate() -> Terrain {
    Terrain::flat(40, 20.0, 0.0)
}

fn flat_dist(a: Vec3, b: Vec3) -> f32 {
    ((a.x - b.x).powi(2) + (a.z - b.z).powi(2)).sqrt()
}

/// A character of `class` in the world at (x, z), on a GM account when `gm`.
async fn enter_at(
    h: &Harness,
    user: &str,
    class: &str,
    gm: bool,
    x: f32,
    z: f32,
) -> (WorldClient, i64) {
    let (session, char_id, _stale) =
        provision_client(&h.auth_url, user, &format!("{user}a"), "Human", class).await;
    if gm {
        let pool = db::open(&h.db_url).await.expect("open pool");
        db::set_account_gm(&pool, user, true)
            .await
            .expect("set is_gm");
    }
    seed_position(&h.db_url, char_id, x, z).await;
    let token = request_world_token(&h.auth_url, &session, char_id).await;
    (WorldClient::start(token, &session, char_id).await, char_id)
}

/// Pump for `d`, keeping every system message and the newest position of every body.
async fn watch(
    c: &mut WorldClient,
    d: Duration,
    at: &mut HashMap<u64, Vec3>,
) -> Vec<ServerWorldMsg> {
    let end = Instant::now() + d;
    let mut next_beat = Instant::now();
    let mut out = Vec::new();
    while Instant::now() < end {
        if Instant::now() >= next_beat {
            c.send_heartbeat();
            next_beat = Instant::now() + Duration::from_secs(2);
        }
        tick_one(&mut c.client, &mut c.transport);
        drain(c, at, &mut out);
        tokio::time::sleep(TICK_DT).await;
    }
    out
}

fn drain(c: &mut WorldClient, at: &mut HashMap<u64, Vec3>, out: &mut Vec<ServerWorldMsg>) {
    while let Some(bytes) = c.client.receive_message(CHANNEL_POSITION) {
        if let Ok((ServerWorldMsg::Position { id, pos, .. }, _)) =
            bincode::serde::decode_from_slice::<ServerWorldMsg, _>(&bytes, bincode_cfg())
        {
            at.insert(id, pos);
        }
    }
    while let Some(bytes) = c.client.receive_message(CHANNEL_SYSTEM) {
        if let Ok((msg, _)) =
            bincode::serde::decode_from_slice::<ServerWorldMsg, _>(&bytes, bincode_cfg())
        {
            out.push(msg);
        }
    }
}

/// The wanderers named `name` among these messages: (id, where it appeared).
fn spawns_named(msgs: &[ServerWorldMsg], name: &str) -> Vec<(u64, Vec3)> {
    msgs.iter()
        .filter_map(|m| match m {
            ServerWorldMsg::EnemySpawn {
                id, mob_name, pos, ..
            } if mob_name == name => Some((*id, *pos)),
            _ => None,
        })
        .collect()
}

fn targeted(msgs: &[ServerWorldMsg], mob: u64, who: i64) -> bool {
    msgs.iter().any(|m| {
        matches!(m, ServerWorldMsg::EntityTarget { id, target: Some(t) }
            if *id == mob && *t == who as u64)
    })
}

/// A wanderer walks on its own: a watcher standing in view, beyond its aggro radius, sees
/// it leave the spot it appeared at and keep to its region, and it never targets anyone.
/// (Positions go only to players who can see the mob, so the watcher is what makes the
/// walk observable; the mob AI itself runs only while someone is in the world.)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_wanderer_walks_its_region_on_its_own() {
    let h = start_with_roamers(
        plate(),
        r#"
        [[roam]]
        desc = "a quick rat in a 60 m circle"
        mob = { name = "Roam Rat", level = 1, hp = 20.0, dmg = 1, speed = 5.0, aggro = 5.0 }
        region = { center = [0.0, 330.0], radius = 60.0 }
        count = 1
        respawn = 600.0
        "#,
    )
    .await;
    // 20 m from the nearest point of the region: in view, out of its 5 m aggro. (The
    // region is 60 m, not 30: a leg is 20 to 80 m, and `pick_leg` gives up after eight
    // draws that land outside the region, which in a 30 m circle happened often enough
    // that the rat then stood its 5 to 25 s pause through the whole watch; the audit of
    // 2026-11-17 saw it fail alone once in five runs, at 0.0 m.)
    let (mut c, _me) = enter_at(&h, "watcher", "Warrior", false, 0.0, 250.0).await;
    let mut at = HashMap::new();
    let seeded = watch(&mut c, Duration::from_millis(1_500), &mut at).await;
    let rats = spawns_named(&seeded, "Roam Rat");
    assert_eq!(
        rats.len(),
        1,
        "the one slot's rat is in the world: {seeded:?}"
    );
    let (rat, born) = rats[0];
    assert!(
        flat_dist(born, REGION) <= 60.0,
        "it appeared in its region, at {born:?}"
    );

    // An amble of 2 m/s (5.0 x 0.4): a leg is at least 20 m, so 5 m takes about 3 s once
    // it sets off. The watch ends as soon as it has, and otherwise runs long enough to
    // cover one failed leg draw and its longest pause (25 s) with room to walk after.
    let mut furthest = 0.0_f32;
    let mut msgs = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(40);
    while Instant::now() < deadline && furthest < 5.0 {
        msgs.extend(watch(&mut c, Duration::from_millis(250), &mut at).await);
        if let Some(pos) = at.get(&rat) {
            furthest = furthest.max(flat_dist(*pos, born));
            assert!(
                flat_dist(*pos, REGION) <= 60.5,
                "it left its region: {pos:?}"
            );
        }
    }
    assert!(
        furthest >= 5.0,
        "a wanderer walks; this one got {furthest:.1} m from where it appeared"
    );
    assert!(
        !msgs.iter().any(
            |m| matches!(m, ServerWorldMsg::EntityTarget { id, target: Some(_) } if *id == rat)
        ),
        "nobody came within its aggro radius, so it targets nobody"
    );
}

/// A wanderer's fight begins where it stands, and that is where it walks back to: it
/// notices a player on its way (its idle scan runs as it walks), the player takes off (a
/// GM's flight makes every mob forget them), and the wolf goes home to the spot where the
/// fight began and heals there, not to the spot it first appeared at.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_wanderer_goes_home_to_where_its_fight_began() {
    let h = start_with_roamers(
        plate(),
        r#"
        [[roam]]
        desc = "one wolf in a 60 m circle"
        mob = { name = "Roam Wolf", level = 1, hp = 500.0, dmg = 1, speed = 3.0, aggro = 12.0 }
        region = { center = [0.0, 330.0], radius = 60.0 }
        count = 1
        respawn = 600.0
        "#,
    )
    .await;
    // 15 m outside the region's nearest edge (the region is 60 m: a 30 m one made the 20 to 80 m
    // legs fail their draws and the wolf stand still for the pauses between): past the wolf's
    // 12 m aggro wherever it is.
    let (mut c, me) = enter_at(&h, "puller", "Warrior", true, 0.0, 255.0).await;
    let mut at = HashMap::new();
    let seeded = watch(&mut c, Duration::from_millis(1_500), &mut at).await;
    let (wolf, born) = *spawns_named(&seeded, "Roam Wolf")
        .first()
        .expect("the wolf is in the world");

    // Let it walk well away from where it appeared (its first leg is at least 20 m).
    let deadline = Instant::now() + Duration::from_secs(40);
    while at.get(&wolf).is_none_or(|p| flat_dist(*p, born) < 15.0) {
        assert!(Instant::now() < deadline, "the wolf never walked 15 m");
        let _ = watch(&mut c, Duration::from_millis(200), &mut at).await;
    }

    // Walk at it until it turns on us.
    let mut seq: u32 = 1;
    let mut msgs = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(30);
    while !targeted(&msgs, wolf, me) {
        assert!(
            Instant::now() < deadline,
            "the wolf never noticed the player"
        );
        let (own, its) = (at.get(&(me as u64)).copied(), at.get(&wolf).copied());
        if let (Some(own), Some(its)) = (own, its) {
            let d = flat_dist(own, its).max(0.01);
            c.send_move(
                seq,
                Vec3 {
                    x: (its.x - own.x) / d,
                    y: 0.0,
                    z: (its.z - own.z) / d,
                },
            );
            seq += 1;
        }
        tick_one(&mut c.client, &mut c.transport);
        drain(&mut c, &mut at, &mut msgs);
        tokio::time::sleep(TICK_DT).await;
    }
    let began = at[&wolf];
    assert!(
        flat_dist(began, born) >= 4.0,
        "the fight began {:.1} m from where the wolf appeared; the test needs them apart",
        flat_dist(began, born)
    );
    // Stand, let it close, then take off: the wolf forgets us and walks home.
    c.send_move(
        seq,
        Vec3 {
            x: 0.0,
            y: 0.0,
            z: 0.0,
        },
    );
    let _ = watch(&mut c, Duration::from_millis(1_500), &mut at).await;
    c.send_gm_command("fly 25");
    let mut healed_at = None;
    let deadline = Instant::now() + Duration::from_secs(20);
    while healed_at.is_none() {
        assert!(Instant::now() < deadline, "the wolf never reached its home");
        let msgs = watch(&mut c, Duration::from_millis(100), &mut at).await;
        // A mob that reaches home heals to full, and that is told to everyone.
        if msgs
            .iter()
            .any(|m| matches!(m, ServerWorldMsg::HealthUpdate { id, .. } if *id == wolf))
        {
            healed_at = at.get(&wolf).copied();
        }
    }
    let home = healed_at.expect("the wolf's position at its heal");
    assert!(
        flat_dist(home, began) <= 2.5,
        "it went home to {home:?}, {:.1} m from where the fight began ({began:?})",
        flat_dist(home, began)
    );
    assert!(
        flat_dist(home, born) >= 1.5,
        "it went back to where it first appeared, not to where the fight began"
    );
}

/// A respawn never lands near a player: a GM standing in the middle of a 70 m region
/// clears it six times, and each time the rat comes back in the ring past
/// `ROAM_POP_CLEARANCE` (60 m), never beside them. (Only a quarter of the region is that
/// far out, so six respawns by chance alone would land near the GM all but once in a
/// few thousand runs.)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_respawn_never_lands_beside_a_player() {
    let h = start_with_roamers(
        plate(),
        r#"
        [[roam]]
        desc = "a rat that comes back in two seconds"
        mob = { name = "Roam Rat", level = 1, hp = 20.0, dmg = 1, speed = 2.0, aggro = 3.0 }
        region = { center = [0.0, 330.0], radius = 70.0 }
        count = 1
        respawn = 2.0
        "#,
    )
    .await;
    let (mut c, _me) = enter_at(&h, "camper", "Warrior", true, REGION.x, REGION.z).await;
    let mut at = HashMap::new();
    let seeded = watch(&mut c, Duration::from_millis(1_500), &mut at).await;
    assert_eq!(spawns_named(&seeded, "Roam Rat").len(), 1);

    for round in 0..6 {
        c.send_dev_despawn_enemies();
        let mut back = None;
        let deadline = Instant::now() + Duration::from_secs(8);
        while back.is_none() {
            assert!(
                Instant::now() < deadline,
                "round {round}: the rat never came back"
            );
            let msgs = watch(&mut c, Duration::from_millis(200), &mut at).await;
            back = spawns_named(&msgs, "Roam Rat").first().copied();
        }
        let (_, pos) = back.expect("a respawn");
        let d = flat_dist(pos, REGION);
        assert!(
            d >= world::roam::ROAM_POP_CLEARANCE - 0.5,
            "round {round}: it came back {d:.1} m from the player"
        );
        assert!(d <= 70.5, "round {round}: outside its region, {d:.1} m out");
    }
}

/// A pack fights together. Two passive wolves (they never start a fight, so nothing but
/// the pack rule can bring the second in): a Cleric smites one from range, it turns on
/// the caster, and its packmate, which nobody touched, takes the same target.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_packmate_joins_its_packmates_fight() {
    let h = start_with_roamers(
        plate(),
        // A region too small for a leg (8 m across, a leg is 20 m at least), so the
        // pair stands where it appeared.
        r#"
        [[roam]]
        desc = "a passive pair"
        mob = { name = "Roam Wolf", level = 1, hp = 5000.0, dmg = 1, speed = 3.0, aggro = 0.0, leash = 35.0 }
        region = { center = [0.0, 330.0], radius = 4.0 }
        count = 1
        pack = 2
        respawn = 600.0
        "#,
    )
    .await;
    // 14 to 22 m from anywhere the pair can be: inside Smite's 30 m and both leashes.
    let (mut c, me) = enter_at(&h, "smiter", "Cleric", false, 0.0, 312.0).await;
    let mut at = HashMap::new();
    let seeded = watch(&mut c, Duration::from_millis(1_500), &mut at).await;
    let pack = spawns_named(&seeded, "Roam Wolf");
    assert_eq!(pack.len(), 2, "a pack of two: {seeded:?}");
    assert!(
        flat_dist(pack[0].1, pack[1].1) <= world::roam::PACK_SPREAD + 0.5,
        "the pack appears together"
    );
    // Passive: standing 14 m off for two seconds provokes nothing.
    let quiet = watch(&mut c, Duration::from_secs(2), &mut at).await;
    assert!(
        !targeted(&quiet, pack[0].0, me) && !targeted(&quiet, pack[1].0, me),
        "a passive animal starts no fight"
    );

    c.send_cast_spell("Smite", Some(pack[0].0));
    let msgs = watch(&mut c, Duration::from_secs(2), &mut at).await;
    assert!(
        targeted(&msgs, pack[0].0, me),
        "the smitten wolf turns on the caster: {msgs:?}"
    );
    assert!(
        targeted(&msgs, pack[1].0, me),
        "its packmate, untouched, joins the fight"
    );
    // And it comes: both wolves close on the caster (3 m/s over 22 m at most). A
    // passive animal nobody touched would stand where it was.
    let own = at
        .get(&(me as u64))
        .copied()
        .expect("the caster's own position");
    let deadline = Instant::now() + Duration::from_secs(12);
    loop {
        let near = |id: u64| at.get(&id).is_some_and(|p| flat_dist(*p, own) <= 3.0);
        if near(pack[0].0) && near(pack[1].0) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "both wolves reach the caster; they are at {:?} and {:?}",
            at.get(&pack[0].0),
            at.get(&pack[1].0)
        );
        let _ = watch(&mut c, Duration::from_millis(200), &mut at).await;
    }
}
