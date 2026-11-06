//! Bard songs: the sing, the pulse, twisting and running while singing.

use crate::common::*;

/// Bard songs neither start nor honour the global cooldown, or twisting
/// (songs sung back to back) would stop working. A Bard 10 sings Poet's
/// Mending (a 3 s sing since the song rework) and begins Selos' Melody the
/// moment it lands, inside the 2 s window that refuses any other cast; both
/// are accepted, and neither costs mana (EQ's songs were free).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bard_songs_ignore_the_global_cooldown() {
    let h = start_both().await;
    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "twist", "Lyric", "Human", "Bard").await;
    set_char_level(&h.db_url, a_char_id, 10).await;
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    a.pump_for(Duration::from_millis(300)).await;

    let first = sing(&mut a, "Poet's Mending", None).await;
    assert!(first.iter().any(|m| matches!(m, ServerWorldMsg::ChatMessage { text, .. } if text == "You begin singing Poet's Mending.")), "the first song lands");
    a.send_cast_start("Selos' Melody", 3.0);
    let refused = a
        .wait_for(
            CHANNEL_SYSTEM,
            Duration::from_millis(600),
            |m| matches!(m, ServerWorldMsg::CastFail { caster, .. } if *caster == a_char_id as u64),
        )
        .await;
    assert!(
        refused.is_none(),
        "a song's CastStart inside the global cooldown is accepted"
    );
    a.pump_alive_for(Duration::from_millis(3_300)).await;
    a.send_cast_spell("Selos' Melody", None);
    let second = a.collect_for(Duration::from_millis(1_000)).await;
    assert!(second.iter().any(|m| matches!(m, ServerWorldMsg::ChatMessage { text, .. } if text == "You begin singing Selos' Melody.")), "and so is its release");
    assert!(!first.iter().chain(second.iter()).any(|m| matches!(m, ServerWorldMsg::ManaUpdate { id, mp, max_mp } if *id == a_char_id as u64 && *mp < max_mp - 0.5)),
        "songs cost no mana");
}

/// A song takes its 3 s to sing (a forged instant release is refused), its
/// effect lands with the song's linger, the server pulses it every 6 s, and
/// after a StopSong the last pulse's effect lingers its 12 s and goes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_song_is_sung_pulsed_and_lingers_after_it_stops() {
    let h = start_both().await;
    let (session, a_id, _stale) =
        provision_client(&h.auth_url, "singer", "Singera", "Human", "Bard").await;
    set_char_level(&h.db_url, a_id, 10).await;
    let token = request_world_token(&h.auth_url, &session, a_id).await;
    let mut a = WorldClient::start(token, &session, a_id).await;
    a.pump_for(Duration::from_millis(300)).await;

    // An instant twist is a forgery: the release 0.3 s into a 3 s sing.
    a.send_cast_start("Selos' Melody", 3.0);
    a.pump_for(Duration::from_millis(300)).await;
    a.send_cast_spell("Selos' Melody", None);
    let early = a.collect_for(Duration::from_millis(800)).await;
    assert!(
        early.iter().any(
            |m| matches!(m, ServerWorldMsg::CastFail { caster, .. } if *caster == a_id as u64)
        ),
        "released early: refused"
    );
    assert!(!early.iter().any(|m| matches!(m, ServerWorldMsg::ChatMessage { text, .. } if text.starts_with("You begin singing"))));

    let begun = sing(&mut a, "Selos' Melody", None).await;
    assert!(begun.iter().any(|m| matches!(m, ServerWorldMsg::ChatMessage { text, .. } if text == "You begin singing Selos' Melody.")));
    let rem = song_remaining(&begun, a_id as u64, "Selos' Melody")
        .expect("the first pulse's effect is on the bard");
    assert!(
        rem > 11.0 && rem <= 12.05,
        "lingering the song linger: {rem}"
    );

    // The next pulse, 6 s on: a fresh snapshot with the effect refreshed.
    let _ = a.collect_for(Duration::from_millis(4_500)).await;
    let around_pulse = a.collect_for(Duration::from_millis(1_500)).await;
    let refreshed = song_remaining(&around_pulse, a_id as u64, "Selos' Melody")
        .expect("the second pulse re-fans the snapshot");
    assert!(refreshed > 11.0, "refreshed by the pulse: {refreshed}");

    let stop_at = Instant::now();
    a.send_stop_song();
    let stopped = a.collect_for(Duration::from_millis(500)).await;
    assert!(stopped.iter().any(|m| matches!(m, ServerWorldMsg::ChatMessage { text, .. } if text == "You stop singing Selos' Melody.")));
    // No pulse after the stop (a snapshot refreshed to the full linger would
    // be one), and the last pulse's effect runs out when its 12 s linger
    // does: that pulse fell within the 1.5 s window before the stop (about
    // 2 s of real time, the loop's sleeps run a little long), so the removal
    // lands 10 to 12 s after it. Read off the clock, not the windows; a
    // pulse after the stop would show first as a refreshed snapshot.
    let mut gone_at: Option<Duration> = None;
    let deadline = stop_at + Duration::from_secs(16);
    while Instant::now() < deadline && gone_at.is_none() {
        for m in a.collect_for(Duration::from_millis(250)).await {
            if let ServerWorldMsg::BuffSnapshot { target, buffs } = &m {
                if *target == a_id as u64 {
                    match buffs.iter().find(|(n, _)| n == "Selos' Melody") {
                        Some((_, r)) => assert!(*r < 11.5, "no pulse after the stop: {r}"),
                        None => gone_at = Some(stop_at.elapsed()),
                    }
                }
            }
        }
    }
    let gone_at = gone_at.expect("the effect runs out within 16 s of the stop");
    assert!(
        gone_at >= Duration::from_secs_f32(9.0) && gone_at <= Duration::from_secs_f32(15.0),
        "the linger ran its 12 s from the last pulse: {gone_at:?}"
    );
}

/// Twisting: a second song replaces the first as the active song while the
/// first's effect lingers, so both show; singing the active song again stops
/// it; a nuke (any non-song cast) stops the song too.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn twisting_holds_two_songs_and_a_nuke_ends_the_song() {
    let h = start_both().await;
    let (session, a_id, _stale) =
        provision_client(&h.auth_url, "twister", "Twistera", "Human", "Bard").await;
    let pool = db::open(&h.db_url).await.expect("open pool");
    db::set_account_gm(&pool, "twister", true)
        .await
        .expect("set is_gm");
    set_char_level(&h.db_url, a_id, 10).await;
    let token = request_world_token(&h.auth_url, &session, a_id).await;
    let mut a = WorldClient::start(token, &session, a_id).await;
    a.pump_for(Duration::from_millis(300)).await;
    let (dummy, _) = spawn_dummy(&mut a, "Song Dummy", 0, 0.0).await;

    let _ = sing(&mut a, "Selos' Melody", None).await;
    let second = sing(&mut a, "Wanderer's Chord", None).await;
    assert!(second.iter().any(|m| matches!(m, ServerWorldMsg::ChatMessage { text, .. } if text == "You begin singing Wanderer's Chord.")));
    assert!(second.iter().any(|m| matches!(m, ServerWorldMsg::BuffSnapshot { target, buffs } if *target == a_id as u64
        && buffs.iter().any(|(n, _)| n == "Selos' Melody") && buffs.iter().any(|(n, _)| n == "Wanderer's Chord"))),
        "both songs' effects are up");

    let again = sing(&mut a, "Wanderer's Chord", None).await;
    assert!(again.iter().any(|m| matches!(m, ServerWorldMsg::ChatMessage { text, .. } if text == "You stop singing Wanderer's Chord.")), "singing the active song stops it");

    let _ = sing(&mut a, "Selos' Melody", None).await;
    a.send_cast_spell("Dissonance", Some(dummy));
    let nuked = a.collect_for(Duration::from_millis(1_000)).await;
    assert!(nuked.iter().any(|m| matches!(m, ServerWorldMsg::ChatMessage { text, .. } if text == "You stop singing Selos' Melody.")), "a nuke ends the song");
    assert!(nuked.iter().any(|m| matches!(m, ServerWorldMsg::Hit { attacker, target, .. } if *attacker == a_id as u64 && *target == dummy)), "and lands");
}

/// A song is sung on the move (the movement gate passes songs), and a level 1
/// bard's Selos' is EQ's +20%: 0.31 of the listed +65%, 1.2015x run speed,
/// 0.300 m a tick, where the flat data would give 1.65x (0.4125 m) at any level.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_bard_sings_while_running_and_a_young_bard_runs_slower_than_the_data() {
    let h = start_both().await;
    let (session, a_id, a_token) =
        provision_client(&h.auth_url, "runner", "Runnera", "Human", "Bard").await;
    let mut a = WorldClient::start(a_token, &session, a_id).await;
    a.pump_for(Duration::from_millis(300)).await;
    let east = Vec3 {
        x: 1.0,
        y: 0.0,
        z: 0.0,
    };
    let mut seq: u32 = 1;

    a.send_cast_start("Selos' Melody", 3.0);
    for _ in 0..66 {
        a.send_move(seq, east);
        seq += 1;
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }
    a.send_cast_spell("Selos' Melody", None);
    let begun = a.collect_for(Duration::from_millis(1_000)).await;
    assert!(begun.iter().any(|m| matches!(m, ServerWorldMsg::ChatMessage { text, .. } if text == "You begin singing Selos' Melody.")),
        "fifteen metres of running did not interrupt the sing");
    assert!(!begun
        .iter()
        .any(|m| matches!(m, ServerWorldMsg::CastFail { caster, .. } if *caster == a_id as u64)));

    let step = median_step(&mut a, a_id, &mut seq, east, 30).await;
    assert!(
        (step - 0.300).abs() < 0.012,
        "a level 1 Selos' runs 1.2x, 0.300 m a tick: {step}"
    );
}
