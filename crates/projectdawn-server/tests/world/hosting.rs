//! Public hosting (the client repo's `docs/design/public_hosting_plan.md`,
//! slice 2): the restart for a moved world address, and the per-connection
//! intent budgets. The auth socket's own hardening (slice 1: the limits, the
//! limiter, the invite gate) is tested in `tests/auth_smoke.rs`.

use crate::common::*;

/// A home IP change restarts the server (netcode takes its public address
/// once; see `world::address_watch`). The restart must lose nothing: it is
/// the same clean stop a signal gives, so everyone is saved, the kick carries
/// the Restart code and the address line, and the loop ends with an ERROR so
/// the process exits non-zero and systemd (`Restart=on-failure`) brings it
/// back on the new address.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_address_change_saves_everyone_tells_them_why_and_exits_for_systemd() {
    let mut h = start_both().await;
    let (session, char_id, token) =
        provision_client(&h.auth_url, "mover", "Mover", "Human", "Warrior").await;
    let mut a = WorldClient::start(token, &session, char_id).await;
    // Walk so there is unsaved position to lose (only marked dirty, never
    // saved before the 60 s checkpoint).
    let mut seq = 1u32;
    walk_east(&mut a, &mut seq, 6.0).await;
    let moved = own_position(&mut a, char_id)
        .await
        .expect("a position after the walk");
    assert!(moved.x > 4.0, "walked east: x = {}", moved.x);

    // What the address watch sends when the public name stops resolving to
    // the boot address.
    h.shutdown
        .send(world::Stop::AddressChanged)
        .expect("send the address change");

    let kick = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(5), |m| {
            matches!(
                m,
                ServerWorldMsg::Kick {
                    code: protocol::world::KickCode::Restart,
                    ..
                }
            )
        })
        .await
        .expect("the kick carries the Restart code");
    if let ServerWorldMsg::Kick { reason, .. } = kick {
        assert_eq!(
            reason,
            world::RESTART_FOR_ADDRESS_CHANGE,
            "the kick says why: the address changed, the character is saved"
        );
    }

    let world_task = h.world.take().expect("world handle");
    let ended = tokio::time::timeout(Duration::from_secs(5), world_task)
        .await
        .expect("the world loop exits within seconds")
        .expect("the world task did not panic");
    assert!(
        ended.is_err(),
        "an address change ends the loop with an error, so the process exits non-zero for systemd: {ended:?}"
    );

    // And the walk was written before the exit: the character loads where it
    // stood, not at the spawn.
    let pool = db::open(&h.db_url).await.expect("open pool");
    let (pos_x,): (Option<f32>,) = sqlx::query_as("SELECT pos_x FROM characters WHERE id = ?1")
        .bind(char_id)
        .fetch_one(&pool)
        .await
        .expect("position row");
    let saved_x = pos_x.unwrap_or(0.0);
    assert!(
        (saved_x - moved.x).abs() < 1.0,
        "the saved x {saved_x} should match the walked x {}",
        moved.x
    );
}

/// The cosmetic fan-outs are what a forged client can put on every peer's
/// screen at will; past a per-connection burst they are dropped, and an
/// honest client's two swings a second never come near it. A say is the
/// sharpest case (every player in reach gets a line per message).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_flood_of_cosmetic_intents_is_capped_per_connection() {
    let h = start_both().await;
    let (a_session, a_char, a_token) =
        provision_client(&h.auth_url, "flooder", "Flooder", "Human", "Warrior").await;
    let (b_session, b_char, b_token) =
        provision_client(&h.auth_url, "watcher", "Watcher", "Human", "Warrior").await;
    let mut a = WorldClient::start(a_token, &a_session, a_char).await;
    let mut b = WorldClient::start(b_token, &b_session, b_char).await;
    pump_all_for(&mut [&mut a, &mut b], Duration::from_millis(400)).await;

    // 200 says in one frame, from a client standing beside the watcher.
    for i in 0..200 {
        send_msg(
            &mut a.client,
            CHANNEL_SYSTEM,
            &ClientWorldMsg::Chat {
                channel: protocol::world::ChatChannel::Say,
                text: format!("flood {i}"),
                target_name: None,
            },
        );
    }
    let mut seen = 0u32;
    let end = Instant::now() + Duration::from_secs(2);
    while Instant::now() < end {
        tick_one(&mut a.client, &mut a.transport);
        tick_one(&mut b.client, &mut b.transport);
        while let Some(bytes) = b.client.receive_message(CHANNEL_SYSTEM) {
            if let Ok((ServerWorldMsg::ChatMessage { speaker, text, .. }, _)) =
                bincode::serde::decode_from_slice::<ServerWorldMsg, _>(&bytes, bincode_cfg())
            {
                if speaker == "Flooder" && text.starts_with("flood ") {
                    seen += 1;
                }
            }
        }
        tokio::time::sleep(TICK_DT).await;
    }
    let _ = b_char;
    // The burst (COSMETIC_BURST, 40) gets through; the rest is held to the
    // refill (COSMETIC_PER_SEC, 20): two seconds buy at most 40 more, and
    // the flood was 200.
    assert!(seen >= 30, "the burst reaches the watcher: saw {seen}");
    assert!(seen <= 90, "the flood is capped: saw {seen} of 200");
}
