//! Food and water: eating by itself, the regen gate, eating by hand, the hunger window.

use crate::common::*;

/// A famished character does not regenerate; the tick it runs out with food
/// in the bags, the server eats the first food in slot order, says so, fans the
/// delta, and regen comes back. Logged in famished with bread in base slot 3
/// and more bread in a bag: the base slot is eaten, the bag is not touched.
#[tokio::test]
async fn a_famished_character_eats_the_first_food_in_slot_order_and_regenerates_again() {
    let h = start_both().await;
    let (session, char_id, _) =
        provision_client(&h.auth_url, "hungry", "Hungrus", "Human", "Warrior").await;
    // Out of both, at 1 HP, with bread in a bag AND in a later base slot.
    set_char_appetite(&h.db_url, char_id, 0.0, 0.0, Some(1.0)).await;
    give_char_item(
        &h.db_url,
        char_id,
        "base",
        0,
        "res://data/loot/items/small_pouch.tres",
        1,
    )
    .await;
    give_char_item(
        &h.db_url,
        char_id,
        "base",
        1,
        "res://data/loot/items/cloth_cap.tres",
        1,
    )
    .await;
    give_char_item(&h.db_url, char_id, "base", 3, BREAD, 2).await;
    give_char_item(&h.db_url, char_id, "bag_0", 0, BREAD, 2).await;
    let token = request_world_token(&h.auth_url, &session, char_id).await;
    let mut a = WorldClient::start(token, &session, char_id).await;
    let cid = char_id as u64;

    // The eat happens on the first tick in the world: the delta names base 3.
    let delta = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(5), |m| {
            matches!(m, ServerWorldMsg::InventoryDelta { location, slot, .. } if location == "base" && *slot == 3)
        })
        .await
        .expect("the bread in base slot 3 is eaten first");
    if let ServerWorldMsg::InventoryDelta { count, .. } = delta {
        assert_eq!(count, 1, "one loaf of the two is gone");
    }
    let line = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::ChatMessage { text, .. } if text == "You eat a Bread Loaf.")
        })
        .await;
    assert!(line.is_some(), "the character is told what it ate");
    // Still out of water: HP regenerates (fed), stamina does not (needs both).
    // Heartbeats keep the app-layer timeout (10 s) off while we watch.
    let end = Instant::now() + Duration::from_secs(15);
    let mut rose = None;
    while Instant::now() < end && rose.is_none() {
        a.send_heartbeat();
        rose = a
            .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
                matches!(m, ServerWorldMsg::HealthUpdate { id, hp, .. } if *id == cid && *hp > 1.5)
            })
            .await;
    }
    assert!(rose.is_some(), "fed again, HP regenerates");

    // The counters persist: leave, and the fullness is the loaf's size less the walk.
    a.send_disconnect();
    a.pump_for(Duration::from_millis(500)).await;
    drop(a);
    tokio::time::sleep(Duration::from_millis(500)).await;
    let (hunger, thirst) = read_char_appetite(&h.db_url, char_id).await;
    assert!(
        hunger > 2880.0 - 60.0 && hunger <= 2880.0,
        "a meal of twenty units, less what ran: {hunger}"
    );
    assert_eq!(thirst, 0.0, "no drink was carried, so still out of water");
}

/// A famished character with no food does NOT regenerate HP, and a dead one
/// does not drain.
#[tokio::test]
async fn out_of_food_means_no_hp_regen_and_the_dead_do_not_starve() {
    let h = start_both().await;
    let (session, char_id, _) =
        provision_client(&h.auth_url, "starving", "Starvo", "Human", "Warrior").await;
    set_char_appetite(&h.db_url, char_id, 0.0, 1000.0, Some(1.0)).await;
    let token = gm_world_token(&h.auth_url, &h.db_url, "starving", &session, char_id).await;
    let mut a = WorldClient::start(token, &session, char_id).await;
    let cid = char_id as u64;
    // Heartbeats keep the app-layer timeout (10 s) off while we watch.
    let end = Instant::now() + Duration::from_secs(15);
    let mut rose = None;
    while Instant::now() < end && rose.is_none() {
        a.send_heartbeat();
        rose = a
            .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
                matches!(m, ServerWorldMsg::HealthUpdate { id, hp, .. } if *id == cid && *hp > 1.5)
            })
            .await;
    }
    assert!(rose.is_none(), "out of food: HP must not regenerate");
    // Thirst drained for those 15 s alive; death freezes it.
    a.send_death();
    let dead = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::HealthUpdate { id, hp, .. } if *id == cid && *hp <= 0.01)
        })
        .await;
    assert!(dead.is_some());
    // Fifteen seconds dead, with heartbeats. Alive it drained about 15 s
    // (to about 984); code that drained the dead would take 15 more (to
    // about 969) and fail the lower bound below.
    let end = Instant::now() + Duration::from_secs(15);
    while Instant::now() < end {
        a.send_heartbeat();
        a.pump_for(Duration::from_secs(3)).await;
    }
    a.send_disconnect();
    a.pump_for(Duration::from_millis(500)).await;
    drop(a);
    tokio::time::sleep(Duration::from_millis(500)).await;
    let (_, thirst) = read_char_appetite(&h.db_url, char_id).await;
    assert!(thirst < 1000.0 - 10.0, "thirst ran while alive: {thirst}");
    assert!(
        thirst > 1000.0 - 25.0,
        "and stopped at death (15 s dead would have taken 15 more): {thirst}"
    );
}

/// Eating by hand: refused with EQ's line when full (nothing taken), and an
/// early hand-eat feeds for half the item's size.
#[tokio::test]
async fn hand_eating_is_refused_when_full_and_halved_when_early() {
    let h = start_both().await;
    let (session, char_id, _) =
        provision_client(&h.auth_url, "snacker", "Snacko", "Human", "Warrior").await;
    let cap = projectdawn_server::world::hunger::FULLNESS_CAP_SECS;
    set_char_appetite(&h.db_url, char_id, cap, cap, None).await;
    give_char_item(&h.db_url, char_id, "base", 0, BREAD, 2).await;
    let token = request_world_token(&h.auth_url, &session, char_id).await;
    let mut a = WorldClient::start(token, &session, char_id).await;
    a.pump_for(Duration::from_millis(300)).await;
    // The counter drains from the moment the character enters the world, so
    // by the time a single eat lands it may be just under the cap, where a
    // hand-eat is rightly accepted for half (it once raced that way). Two eats
    // in one packet are handled in the same tick with no drain between them:
    // if the first finds room it refills to exactly the cap, so the second is
    // always refused. Either way the second loaf is never eaten.
    a.send_use_consumable_base(0);
    a.send_use_consumable_base(0);
    let refused = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::ChatMessage { text, .. } if text.contains("you would explode"))
        })
        .await;
    assert!(refused.is_some(), "full: refused with EQ's line");
    let emptied = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_millis(800), |m| {
            matches!(m, ServerWorldMsg::InventoryDelta { location, slot, count, .. }
                if location == "base" && *slot == 0 && *count == 0)
        })
        .await;
    assert!(
        emptied.is_none(),
        "nothing is taken on a refusal: the second loaf stays"
    );
    a.send_disconnect();
    a.pump_for(Duration::from_millis(500)).await;
    drop(a);
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Early: well below the cap but not hungry. The loaf is taken and feeds
    // for half of 2880.
    // Two loaves again: the first session may have eaten one.
    set_char_appetite(&h.db_url, char_id, 3000.0, cap, None).await;
    give_char_item(&h.db_url, char_id, "base", 0, BREAD, 2).await;
    let token = request_world_token(&h.auth_url, &session, char_id).await;
    let mut b = WorldClient::start(token, &session, char_id).await;
    b.pump_for(Duration::from_millis(300)).await;
    b.send_use_consumable_base(0);
    let taken = b
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::InventoryDelta { location, slot, count, .. } if location == "base" && *slot == 0 && *count == 1)
        })
        .await;
    assert!(taken.is_some(), "an early hand-eat takes the loaf");
    b.send_disconnect();
    b.pump_for(Duration::from_millis(500)).await;
    drop(b);
    tokio::time::sleep(Duration::from_millis(500)).await;
    let (hunger, _) = read_char_appetite(&h.db_url, char_id).await;
    assert!(
        hunger > 3000.0 + 1440.0 - 30.0 && hunger <= 3000.0 + 1440.0,
        "half a meal was added: {hunger}"
    );
}

/// The hunger window (food design section 8). A watcher stands at the spawn;
/// a starving Halfling with bread enters beside them and eats on its first
/// tick. The Halfling's own HungerUpdate carries the meal (about 2880 s),
/// thirst still full, the cap, and the Halfling rate of 2.0; the watcher
/// hears EQ's "Chomp, chomp, chomp..." naming the eater and the bread, and
/// never receives the Halfling's counters (owner only: the watcher's own
/// updates all read full). The eater gets "You eat a Bread Loaf." and not
/// the emote.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_hunger_window_reaches_its_owner_and_the_room_hears_the_meal() {
    let h = start_both().await;
    let (w_session, w_char_id, _) =
        provision_client(&h.auth_url, "watcher", "Lookon", "Human", "Warrior").await;
    let (e_session, e_char_id, _) =
        provision_client(&h.auth_url, "muncher", "Munchkin", "Halfling", "Rogue").await;
    set_char_appetite(&h.db_url, e_char_id, 0.0, hunger_cap(), None).await;
    give_char_item(&h.db_url, e_char_id, "base", 0, BREAD, 1).await;

    let w_token = request_world_token(&h.auth_url, &w_session, w_char_id).await;
    let mut w = WorldClient::start(w_token, &w_session, w_char_id).await;
    let w_first = collect_system(&mut w, Duration::from_millis(500)).await;
    assert!(
        w_first.iter().any(
            |m| matches!(m, ServerWorldMsg::HungerUpdate { hunger_secs, drain_per_sec, .. }
            if *hunger_secs > hunger_cap() - 5.0 && (*drain_per_sec - 1.0).abs() < 1e-6)
        ),
        "the watcher's own update arrives on entering: full, at the Human rate"
    );

    w.send_heartbeat();
    let e_token = request_world_token(&h.auth_url, &e_session, e_char_id).await;
    let mut e = WorldClient::start(e_token, &e_session, e_char_id).await;
    let (e_msgs, w_msgs) = tokio::join!(
        collect_system(&mut e, Duration::from_secs(2)),
        collect_system(&mut w, Duration::from_secs(2)),
    );

    let update = e_msgs
        .iter()
        .find_map(|m| match m {
            ServerWorldMsg::HungerUpdate {
                hunger_secs,
                thirst_secs,
                cap_secs,
                drain_per_sec,
            } => Some((*hunger_secs, *thirst_secs, *cap_secs, *drain_per_sec)),
            _ => None,
        })
        .expect("the eater gets a HungerUpdate on entering");
    let (hunger, thirst, cap, rate) = update;
    assert!(
        hunger > 2880.0 - 30.0 && hunger <= 2880.0,
        "the update carries the meal just eaten: {hunger}"
    );
    assert!(thirst > cap - 30.0, "thirst still full: {thirst}");
    assert_eq!(cap, hunger_cap(), "the cap");
    assert!(
        (rate - 2.0).abs() < 1e-6,
        "a living Halfling drains at twice the Human rate: {rate}"
    );
    assert!(
        e_msgs.iter().any(|m| matches!(m, ServerWorldMsg::ChatMessage { text, .. } if text == "You eat a Bread Loaf.")),
        "the eater is told what it ate"
    );
    assert!(
        !e_msgs.iter().any(
            |m| matches!(m, ServerWorldMsg::ChatMessage { text, .. } if text.starts_with("Chomp"))
        ),
        "the eater does not get the emote about itself"
    );

    assert!(
        w_msgs
            .iter()
            .any(|m| matches!(m, ServerWorldMsg::ChatMessage { text, .. }
            if text == "Chomp, chomp, chomp... Munchkin takes a bite from a Bread Loaf.")),
        "the watcher beside the eater hears EQ's emote"
    );
    for m in w_msgs.iter().chain(w_first.iter()) {
        if let ServerWorldMsg::HungerUpdate { hunger_secs, .. } = m {
            assert!(
                *hunger_secs > hunger_cap() - 60.0,
                "the watcher only ever receives its own counters, never the eater's ({hunger_secs})"
            );
        }
    }

    e.send_disconnect();
    w.send_disconnect();
    e.pump_for(Duration::from_millis(200)).await;
    w.pump_for(Duration::from_millis(200)).await;
}
