//! GM flight (docs/design/handoff_gm_flight.md): a GM's `fly <m/s>` lifts the server's
//! own body into three dimensions, held above the ground, on the plate and under the
//! ceiling; `fly off` lets go and the fall lands for nothing; mobs forget a flier and a
//! flier cannot fight; a save puts a flier on the ground under them; a fling or a death
//! ends flight; a plain account's `fly` does nothing. Every test here fails on the server
//! before flight, which pinned every player to the ground.

use crate::common::*;
use projectdawn_server::world::fall;

const ZERO: Vec3 = Vec3 {
    x: 0.0,
    y: 0.0,
    z: 0.0,
};
const UP: Vec3 = Vec3 {
    x: 0.0,
    y: 1.0,
    z: 0.0,
};
const DOWN: Vec3 = Vec3 {
    x: 0.0,
    y: -1.0,
    z: 0.0,
};
const EAST: Vec3 = Vec3 {
    x: 1.0,
    y: 0.0,
    z: 0.0,
};
/// What the flying gate answers a cast or a skill with (`handlers::FLYING_REFUSAL`).
const FLYING_REFUSAL: &str = "You cannot do that while flying.";

/// A level 1 Human Warrior on a GM account, in the world at the origin, with the max HP
/// the server told it on entering.
async fn gm(h: &Harness, user: &str) -> (WorldClient, i64, f32) {
    let (session, char_id, _stale) =
        provision_client(&h.auth_url, user, &format!("{user}a"), "Human", "Warrior").await;
    let pool = db::open(&h.db_url).await.expect("open pool");
    db::set_account_gm(&pool, user, true)
        .await
        .expect("set is_gm");
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

/// Send a GM line and keep what the system channel says for a moment.
async fn gm_line(c: &mut WorldClient, line: &str) -> Vec<ServerWorldMsg> {
    c.send_gm_command(line);
    c.collect_for(Duration::from_millis(400)).await
}

/// Steer along `dir` for `ticks` Moves, then a zero Move so the server stops at once
/// (without it the held direction carries on for STALE_MOVE_THRESHOLD).
async fn steer(c: &mut WorldClient, seq: &mut u32, dir: Vec3, ticks: u32) {
    for _ in 0..ticks {
        c.send_move(*seq, dir);
        *seq += 1;
        tick_one(&mut c.client, &mut c.transport);
        c.drain_positions();
        tokio::time::sleep(TICK_DT).await;
    }
    c.send_move(*seq, ZERO);
    *seq += 1;
    c.pump_for(Duration::from_millis(150)).await;
}

/// How many `fly` rows the GM action audit holds, read after the tick's flush.
async fn fly_rows(db_url: &str) -> i64 {
    tokio::time::sleep(Duration::from_millis(200)).await;
    let pool = db::open(db_url).await.expect("open pool");
    sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM gm_actions WHERE command = 'fly'")
        .fetch_one(&pool)
        .await
        .expect("count fly rows")
}

fn says(msgs: &[ServerWorldMsg], prefix: &str) -> bool {
    system_lines(msgs).iter().any(|l| l.starts_with(prefix))
}

/// A GM flies where it steers: up at its speed, sideways at twice it with the boost,
/// and no further for a forged longer direction; `fly off` from 20 m lands on the ground
/// with no damage and no line. Bad speeds, and `fly off` with no flight, record nothing.
/// A plain account's `fly` does nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_gm_flies_where_it_steers_and_lands_unhurt() {
    let h = start_both().await;
    let (mut c, char_id, max_hp) = gm(&h, "flyer").await;
    let mut seq: u32 = 1;

    for bad in [
        "fly 0", "fly 101", "fly -5", "fly nan", "fly inf", "fly abc", "fly off",
    ] {
        let msgs = gm_line(&mut c, bad).await;
        assert!(
            !says(&msgs, "You take to the air") && !says(&msgs, "You stop flying"),
            "`{bad}` does nothing: {msgs:?}"
        );
    }
    steer(&mut c, &mut seq, UP, 10).await;
    let grounded = own_position_now(&mut c, char_id).await;
    assert!(grounded.y.abs() < 0.05, "still on foot: {grounded:?}");
    assert_eq!(fly_rows(&h.db_url).await, 0, "refused lines record nothing");

    let msgs = gm_line(&mut c, "fly 20").await;
    assert!(
        says(&msgs, "You take to the air (20 m/s)."),
        "takeoff is announced: {msgs:?}"
    );
    steer(&mut c, &mut seq, UP, 20).await;
    let up = own_position_now(&mut c, char_id).await;
    assert!(
        (14.0..=26.0).contains(&up.y),
        "a second of climbing at 20 m/s is about 20 m: {up:?}"
    );

    // The boost doubles the speed; a forged direction of length 10 is held to it.
    for (dir, what) in [
        (
            Vec3 {
                x: 2.0,
                y: 0.0,
                z: 0.0,
            },
            "the boost",
        ),
        (
            Vec3 {
                x: 10.0,
                y: 0.0,
                z: 0.0,
            },
            "a forged long direction",
        ),
    ] {
        let from = own_position_now(&mut c, char_id).await;
        steer(&mut c, &mut seq, dir, 10).await;
        let to = own_position_now(&mut c, char_id).await;
        let moved = to.x - from.x;
        assert!(
            (14.0..=26.0).contains(&moved),
            "{what}: half a second at twice 20 m/s is about 20 m, moved {moved}"
        );
        assert!((to.y - from.y).abs() < 0.05, "{what} keeps the height");
    }

    let msgs = gm_line(&mut c, "fly off").await;
    assert!(says(&msgs, "You stop flying."), "{msgs:?}");
    let msgs = c.collect_for(Duration::from_secs(3)).await;
    assert!(
        !says(&msgs, "You hit the ground"),
        "the drop after flight costs nothing: {msgs:?}"
    );
    assert!(
        !msgs
            .iter()
            .any(|m| matches!(m, ServerWorldMsg::HealthUpdate { id, hp, .. }
            if *id == char_id as u64 && *hp < max_hp)),
        "no HP lost: {msgs:?}"
    );
    let landed = own_position_now(&mut c, char_id).await;
    assert!(landed.y.abs() < 0.05, "back on the ground: {landed:?}");
    steer(&mut c, &mut seq, UP, 10).await;
    let after = own_position_now(&mut c, char_id).await;
    assert!(after.y.abs() < 0.05, "on foot again: {after:?}");
    assert_eq!(fly_rows(&h.db_url).await, 2, "one row per toggle");

    // A plain account's `fly` is ignored silently. Only meaningful when the process is
    // not in dev mode, where every connection may use dev commands.
    if std::env::var("PD_DEV_CMDS").as_deref() != Ok("1") {
        let (pl_session, pl_char, pl_token) =
            provision_client(&h.auth_url, "flyplain", "Groundb", "Human", "Warrior").await;
        let mut plain = WorldClient::start(pl_token, &pl_session, pl_char).await;
        plain.pump_for(Duration::from_millis(300)).await;
        let msgs = gm_line(&mut plain, "fly 20").await;
        assert!(
            !says(&msgs, "You take to the air"),
            "a plain account is told nothing: {msgs:?}"
        );
        let mut pseq: u32 = 1;
        steer(&mut plain, &mut pseq, UP, 20).await;
        let p = own_position_now(&mut plain, pl_char).await;
        assert!(p.y.abs() < 0.05, "a plain account stays on foot: {p:?}");
        assert_eq!(fly_rows(&h.db_url).await, 2, "and writes no row");
    }
}

/// A flier never goes under the ground, past the plate's edge or over the ceiling.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_flier_is_held_above_the_ground_on_the_plate_and_under_the_ceiling() {
    let h = start_both().await;
    let (mut c, char_id, _max_hp) = gm(&h, "flybound").await;
    let mut seq: u32 = 1;
    let msgs = gm_line(&mut c, "fly 100").await;
    assert!(says(&msgs, "You take to the air"), "{msgs:?}");

    steer(&mut c, &mut seq, DOWN, 10).await;
    let floor = own_position_now(&mut c, char_id).await;
    assert!(
        floor.y.abs() < 0.05,
        "held on the ground, not under it: {floor:?}"
    );

    // The harness plate is 400 m, x from -200 to 200: 300 m of boosted flight east.
    steer(
        &mut c,
        &mut seq,
        Vec3 {
            x: 2.0,
            y: 0.0,
            z: 0.0,
        },
        30,
    )
    .await;
    let edge = own_position_now(&mut c, char_id).await;
    assert!(
        edge.x <= 200.01 && edge.x >= 190.0,
        "held at the plate's edge: {edge:?}"
    );

    // 200 m/s for 5.5 s would reach 1,100 m.
    steer(
        &mut c,
        &mut seq,
        Vec3 {
            x: 0.0,
            y: 2.0,
            z: 0.0,
        },
        110,
    )
    .await;
    let top = own_position_now(&mut c, char_id).await;
    assert!(
        (top.y - world::FLY_CEILING).abs() < 0.05,
        "held at the ceiling: {top:?}"
    );
}

/// A flier who logs out in the air is saved on the ground under them, and logs back in
/// standing there, unhurt and on foot.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_flier_is_saved_on_the_ground_and_logs_in_on_foot() {
    let h = start_both().await;
    let (session, char_id, _stale) =
        provision_client(&h.auth_url, "flysave", "Flysavea", "Human", "Warrior").await;
    let pool = db::open(&h.db_url).await.expect("open pool");
    db::set_account_gm(&pool, "flysave", true)
        .await
        .expect("set is_gm");
    let token = request_world_token(&h.auth_url, &session, char_id).await;
    let mut c = WorldClient::start(token, &session, char_id).await;
    c.pump_for(Duration::from_millis(300)).await;
    let mut seq: u32 = 1;
    let msgs = gm_line(&mut c, "fly 20").await;
    assert!(says(&msgs, "You take to the air"), "{msgs:?}");
    steer(&mut c, &mut seq, UP, 40).await;
    steer(&mut c, &mut seq, EAST, 10).await;
    let aloft = own_position_now(&mut c, char_id).await;
    assert!(aloft.y > 25.0 && aloft.x > 7.0, "up and east: {aloft:?}");

    c.send_disconnect();
    c.pump_for(Duration::from_millis(200)).await;
    let mut saved = (0.0f64, 0.0f64);
    for _ in 0..50 {
        let row: (Option<f64>, Option<f64>) =
            sqlx::query_as("SELECT pos_x, pos_y FROM characters WHERE id = ?1")
                .bind(char_id)
                .fetch_one(&pool)
                .await
                .expect("read the saved position");
        saved = (row.0.unwrap_or(0.0), row.1.unwrap_or(0.0));
        if saved.0 > 7.0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        saved.0 > 7.0 && saved.1.abs() < 0.05,
        "saved where the flier was, on the ground under them: {saved:?}"
    );

    tokio::time::sleep(Duration::from_millis(500)).await;
    let token = request_world_token(&h.auth_url, &session, char_id).await;
    let mut back = WorldClient::start(token, &session, char_id).await;
    let msgs = back.collect_for(Duration::from_secs(2)).await;
    assert!(
        !says(&msgs, "You hit the ground"),
        "the login is no fall: {msgs:?}"
    );
    let at = own_position_now(&mut back, char_id).await;
    assert!(
        at.y.abs() < 0.05 && at.x > 7.0,
        "standing where saved: {at:?}"
    );
    let mut seq: u32 = 1;
    steer(&mut back, &mut seq, UP, 10).await;
    let still = own_position_now(&mut back, char_id).await;
    assert!(still.y.abs() < 0.05, "a login is on foot: {still:?}");
}

/// The mob hitting a GM forgets them at takeoff and never touches a flier, even one
/// hovering beside it; a flier's swing, cast and skill are refused; on landing the mob
/// notices the GM again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mobs_forget_a_flier_and_a_flier_cannot_fight() {
    let h = start_both().await;
    let (mut c, char_id, dummy) =
        gm_beside_dummy(&h, "flyfight", "Warrior", "Flight Dummy", 1, 10.0).await;
    let msgs = c.collect_for(Duration::from_secs(4)).await;
    assert!(
        mob_hits_on(&msgs, dummy, char_id) >= 1,
        "the dummy fights the GM on foot: {msgs:?}"
    );

    let msgs = gm_line(&mut c, "fly 10").await;
    let mut seen = msgs.clone();
    seen.extend(c.collect_for(Duration::from_millis(400)).await);
    assert!(
        seen.iter().any(
            |m| matches!(m, ServerWorldMsg::EntityTarget { id, target: None } if *id == dummy)
        ),
        "the dummy drops the flier at takeoff: {seen:?}"
    );
    let msgs = c.collect_for(Duration::from_secs(3)).await;
    assert_eq!(
        mob_hits_on(&msgs, dummy, char_id),
        0,
        "no mob touches a flier, even beside it: {msgs:?}"
    );

    c.send_attack(dummy, "", false, DamageType::Physical);
    c.send_cast_spell("Smite", Some(dummy));
    c.send_use_skill("Kick", Some(dummy));
    let msgs = c.collect_for(Duration::from_millis(800)).await;
    assert!(
        !msgs.iter().any(|m| matches!(m,
            ServerWorldMsg::Hit { attacker, .. } | ServerWorldMsg::Miss { attacker, .. }
            if *attacker == char_id as u64)),
        "a flier's swing lands nothing: {msgs:?}"
    );
    assert!(
        msgs.iter()
            .any(|m| matches!(m, ServerWorldMsg::CastFail { caster, reason }
            if *caster == char_id as u64 && reason == FLYING_REFUSAL)),
        "a flier's cast is refused: {msgs:?}"
    );
    assert!(
        msgs.iter()
            .any(|m| matches!(m, ServerWorldMsg::SkillFail { reason, .. }
            if reason == FLYING_REFUSAL)),
        "a flier's skill is refused: {msgs:?}"
    );

    let msgs = gm_line(&mut c, "fly off").await;
    assert!(says(&msgs, "You stop flying."), "{msgs:?}");
    let msgs = c.collect_for(Duration::from_secs(4)).await;
    assert!(
        mob_hits_on(&msgs, dummy, char_id) >= 1,
        "on foot again, the dummy notices the GM: {msgs:?}"
    );
}

/// A fling from flight ends it and the fling's fall costs what it costs; a death ends it
/// too, and the dead cannot take off.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fling_or_a_death_ends_flight() {
    let h = start_both().await;
    let (mut c, char_id, max_hp) = gm(&h, "flyend").await;
    let mut seq: u32 = 1;

    let msgs = gm_line(&mut c, "fly 20").await;
    assert!(says(&msgs, "You take to the air"), "{msgs:?}");
    c.send_dev_fling(20.0);
    let msgs = c.collect_for(Duration::from_secs(3)).await;
    let line = format!(
        "You hit the ground hard for {} damage.",
        fall::damage(20.0, max_hp) as i64
    );
    assert!(
        says(&msgs, &line),
        "the fling's fall costs ({line}): {msgs:?}"
    );
    steer(&mut c, &mut seq, UP, 10).await;
    let after = own_position_now(&mut c, char_id).await;
    assert!(
        after.y.abs() < 0.05,
        "the fling ended the flight: {after:?}"
    );

    c.send_heal_self();
    let msgs = gm_line(&mut c, "fly 20").await;
    assert!(says(&msgs, "You take to the air"), "{msgs:?}");
    steer(&mut c, &mut seq, UP, 10).await;
    let rows_before = fly_rows(&h.db_url).await;
    c.send_damage_self(max_hp as i32 + 100);
    c.wait_for(
        CHANNEL_SYSTEM,
        Duration::from_secs(3),
        |m| matches!(m, ServerWorldMsg::HealthUpdate { id, hp, .. } if *id == char_id as u64 && *hp <= 0.0),
    )
    .await
    .expect("the GM dies");
    c.pump_for(Duration::from_millis(300)).await;
    let dead = own_position_now(&mut c, char_id).await;
    assert!(dead.y.abs() < 0.05, "the body lies on the ground: {dead:?}");
    let msgs = gm_line(&mut c, "fly 20").await;
    assert!(
        !says(&msgs, "You take to the air"),
        "the dead cannot fly: {msgs:?}"
    );
    assert_eq!(fly_rows(&h.db_url).await, rows_before, "and write no row");

    c.send_respawn();
    c.pump_for(Duration::from_millis(500)).await;
    steer(&mut c, &mut seq, UP, 10).await;
    let respawned = own_position_now(&mut c, char_id).await;
    assert!(
        respawned.y.abs() < 0.05,
        "the respawn is on foot: {respawned:?}"
    );
}

/// A flier fights through nothing, a pet included (the review's finding): the skeleton
/// that was fighting for a Necromancer GM drops its target at takeoff and does not swing
/// again, a `/pet attack` from the air is refused with the flying line, and once the GM
/// lands the pet can be sent again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fliers_pet_stands_down_and_cannot_be_sent() {
    let h = start_both().await;
    let (session, char_id, _stale) =
        provision_client(&h.auth_url, "flypet", "Flypeta", "Human", "Necromancer").await;
    memorize_whole_class(&h.db_url, char_id, "Necromancer").await;
    set_char_level(&h.db_url, char_id, 6).await;
    let pool = db::open(&h.db_url).await.expect("open pool");
    db::set_account_gm(&pool, "flypet", true)
        .await
        .expect("set is_gm");
    let token = request_world_token(&h.auth_url, &session, char_id).await;
    let mut c = WorldClient::start(token, &session, char_id).await;
    c.pump_for(Duration::from_millis(300)).await;

    c.send_cast_start("Summon Skeleton", 3.0);
    c.pump_for(Duration::from_millis(3100)).await;
    c.send_cast_spell("Summon Skeleton", None);
    let pet_id: u64 = match c
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::PetSpawn { owner, .. } if *owner == char_id as u64)
                || matches!(m, ServerWorldMsg::CastFail { caster, .. } if *caster == char_id as u64)
        })
        .await
        .expect("the skeleton spawns (or a CastFail says why not)")
    {
        ServerWorldMsg::PetSpawn { id, .. } => id,
        ServerWorldMsg::CastFail { reason, .. } => {
            panic!("Summon Skeleton failed with CastFail: {reason:?}")
        }
        _ => unreachable!(),
    };
    // A sponge: it never swings or notices anyone, so only the pet's hits move.
    let (mob_id, _) = spawn_dummy(&mut c, "Air Target", 0, 0.0).await;
    let pet_hits = |msgs: &[ServerWorldMsg]| {
        msgs.iter()
            .filter(|m| matches!(m, ServerWorldMsg::Hit { attacker, target, .. } if *attacker == pet_id && *target == mob_id))
            .count()
    };

    c.send_pet_command(protocol::world::pet_command::ATTACK, Some(mob_id));
    let msgs = c.collect_for(Duration::from_secs(6)).await;
    assert!(
        pet_hits(&msgs) >= 1,
        "on foot the skeleton fights for the GM: {msgs:?}"
    );

    let mut seen = gm_line(&mut c, "fly 10").await;
    seen.extend(c.collect_for(Duration::from_millis(600)).await);
    assert!(says(&seen, "You take to the air"), "{seen:?}");
    assert!(
        seen.iter().any(
            |m| matches!(m, ServerWorldMsg::EntityTarget { id, target: None } if *id == pet_id)
        ),
        "the skeleton drops its target at takeoff: {seen:?}"
    );
    let msgs = c.collect_for(Duration::from_secs(3)).await;
    assert_eq!(pet_hits(&msgs), 0, "and swings no more: {msgs:?}");

    c.send_pet_command(protocol::world::pet_command::ATTACK, Some(mob_id));
    let msgs = c.collect_for(Duration::from_secs(3)).await;
    assert!(
        says(&msgs, FLYING_REFUSAL),
        "a /pet attack from the air is refused: {msgs:?}"
    );
    assert_eq!(pet_hits(&msgs), 0, "and the pet stays down: {msgs:?}");

    let msgs = gm_line(&mut c, "fly off").await;
    assert!(says(&msgs, "You stop flying."), "{msgs:?}");
    c.pump_for(Duration::from_millis(500)).await;
    c.send_pet_command(protocol::world::pet_command::ATTACK, Some(mob_id));
    let msgs = c.collect_for(Duration::from_secs(6)).await;
    assert!(
        pet_hits(&msgs) >= 1,
        "landed, the GM can send the pet again: {msgs:?}"
    );
}

/// A GM in flight earns nothing from the air (the review's finding): grouped with a
/// partner who kills a mob on the ground, the flier takes no XP share while the partner
/// is paid.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_flier_in_a_group_takes_no_xp_share() {
    let h = start_both().await;
    let (mut a, mut b, dummy, b_char_id) =
        grouped_pair_with_dummy(&h, "flyshare", "flymate", 0.0).await;
    // The dev spawn stands 3 m in front of A (at the origin, yaw 0 facing -z,
    // so at z = -3); B walks into melee reach of it on the server's word.
    let mut bseq: u32 = 1;
    walk_until_within(
        &mut b,
        &mut bseq,
        b_char_id,
        ZERO,
        Vec3 {
            x: 0.0,
            y: 0.0,
            z: -3.0,
        },
        1.5,
    )
    .await;

    let a_char_id = a._char_id;
    let mut seen = gm_line(&mut a, "fly 10").await;
    seen.extend(a.collect_for(Duration::from_millis(400)).await);
    assert!(says(&seen, "You take to the air"), "{seen:?}");
    let mut aseq: u32 = 1;
    steer(&mut a, &mut aseq, UP, 20).await;
    let up = own_position_now(&mut a, a_char_id).await;
    assert!(up.y > 5.0, "A is in the air: {up:?}");

    // B kills the 5 HP dummy with its fists; A pumps meanwhile.
    let mut a_msgs: Vec<ServerWorldMsg> = Vec::new();
    let mut b_msgs: Vec<ServerWorldMsg> = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(12);
    let mut died = false;
    while Instant::now() < deadline {
        if !died {
            b.send_attack(dummy, "", false, DamageType::Physical);
        }
        b_msgs.extend(b.collect_for(Duration::from_millis(300)).await);
        a_msgs.extend(a.collect_for(Duration::from_millis(100)).await);
        if b_msgs
            .iter()
            .any(|m| matches!(m, ServerWorldMsg::EntityDied { id } if *id == dummy))
        {
            died = true;
            // (The join seeds an XpGained of 0; the kill's share is more.)
            if b_msgs
                .iter()
                .any(|m| matches!(m, ServerWorldMsg::XpGained { amount, .. } if *amount > 0))
            {
                break;
            }
        }
    }
    assert!(died, "B kills the dummy: {b_msgs:?}");
    assert!(
        b_msgs
            .iter()
            .any(|m| matches!(m, ServerWorldMsg::XpGained { amount, .. } if *amount > 0)),
        "the partner on the ground is paid: {b_msgs:?}"
    );
    a_msgs.extend(a.collect_for(Duration::from_secs(1)).await);
    assert!(
        !a_msgs
            .iter()
            .any(|m| matches!(m, ServerWorldMsg::XpGained { amount, .. } if *amount > 0)),
        "the flier takes no share: {a_msgs:?}"
    );
}
