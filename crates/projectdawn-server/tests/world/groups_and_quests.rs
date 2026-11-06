//! Groups, quests and factions: XP shares and their range, quest turn-ins, and
//! faction hits.

use crate::common::*;

/// Spell batch step 1: the group share range is 200 m. A group-mate 150 m
/// from the kill gets an XP share. On the previous code (30 m) B got nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_xp_share_reaches_a_group_mate_150_m_away() {
    let h = start_both().await;
    let (mut a, mut b, dummy, _b_id) =
        grouped_pair_with_dummy(&h, "sharer150", "mate150", 150.0).await;
    a.send_cast_spell("Smite", Some(dummy));
    // `amount > 0`: the server also seeds each client's xp state on entering
    // the world as an XpGained of 0, which is not a share.
    let a_gain = a
        .wait_for(
            CHANNEL_SYSTEM,
            Duration::from_secs(3),
            |m| matches!(m, ServerWorldMsg::XpGained { amount, .. } if *amount > 0),
        )
        .await;
    assert!(a_gain.is_some(), "the killer is paid");
    let b_gain = b
        .wait_for(
            CHANNEL_SYSTEM,
            Duration::from_secs(3),
            |m| matches!(m, ServerWorldMsg::XpGained { amount, .. } if *amount > 0),
        )
        .await;
    assert!(b_gain.is_some(), "a group-mate 150 m away shares the kill");
}

/// Spell batch step 1: the group share range is 200 m, not more. A group-mate
/// 250 m from the kill gets nothing, the killer the full amount.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_xp_share_stops_past_200_m() {
    let h = start_both().await;
    let (mut a, mut b, dummy, _b_id) =
        grouped_pair_with_dummy(&h, "sharer250", "mate250", 250.0).await;
    a.send_cast_spell("Smite", Some(dummy));
    let a_gain = a
        .wait_for(
            CHANNEL_SYSTEM,
            Duration::from_secs(3),
            |m| matches!(m, ServerWorldMsg::XpGained { amount, .. } if *amount > 0),
        )
        .await;
    assert!(a_gain.is_some(), "the killer is paid");
    let b_gain = b
        .wait_for(
            CHANNEL_SYSTEM,
            Duration::from_secs(2),
            |m| matches!(m, ServerWorldMsg::XpGained { amount, .. } if *amount > 0),
        )
        .await;
    assert!(b_gain.is_none(), "a group-mate 250 m away shares nothing");
}

/// Ledger item 4 (user call D2): a caster may bind a GROUP MEMBER only while
/// that member stands in a safe area. B, grouped and 20 m out, is refused;
/// C, grouped and in the town square, is bound where they stand.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_group_member_is_bound_only_in_a_safe_area() {
    let h = start_both().await;
    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "binder", "Bindra", "Human", "Cleric").await;
    let (b_session, b_char_id, b_token) =
        provision_client(&h.auth_url, "farbound", "Farwell", "Human", "Warrior").await;
    let (c_session, c_char_id, c_token) =
        provision_client(&h.auth_url, "nearbound", "Nearby", "Human", "Warrior").await;
    // A stands at -4 (inside the square, away from the spawn and from C), so
    // a bind written from the CASTER's position would read as -4 and one left
    // at birth as 0: the DB check below tells all three apart.
    set_char_pos(&h.db_url, a_char_id, -4.0, 0.0).await;
    set_char_pos(&h.db_url, b_char_id, 20.0, 0.0).await;
    set_char_pos(&h.db_url, c_char_id, 3.0, 0.0).await;
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    let mut b = WorldClient::start(b_token, &b_session, b_char_id).await;
    let mut c = WorldClient::start(c_token, &c_session, c_char_id).await;
    for cl in [&mut a, &mut b, &mut c] {
        cl.pump_for(Duration::from_millis(300)).await;
    }
    a.send_group_invite("Farwell");
    a.pump_for(Duration::from_millis(300)).await;
    b.pump_for(Duration::from_millis(300)).await;
    b.send_group_accept(a_char_id as u64);
    a.send_group_invite("Nearby");
    a.pump_for(Duration::from_millis(300)).await;
    c.pump_for(Duration::from_millis(300)).await;
    c.send_group_accept(a_char_id as u64);
    for cl in [&mut a, &mut b, &mut c] {
        cl.pump_for(Duration::from_millis(400)).await;
    }

    a.send_cast_start("Bind Affinity", 3.0);
    pump_all_for(&mut [&mut a, &mut b, &mut c], Duration::from_millis(3_200)).await;
    a.send_cast_spell("Bind Affinity", Some(b_char_id as u64));
    let refused = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::CastFail { caster, reason }
                if *caster == a_char_id as u64 && reason.contains("safe place"))
        })
        .await;
    assert!(
        refused.is_some(),
        "a group member outside the safe area cannot be bound"
    );

    a.send_cast_start("Bind Affinity", 3.0);
    pump_all_for(&mut [&mut a, &mut b, &mut c], Duration::from_millis(3_200)).await;
    a.send_cast_spell("Bind Affinity", Some(c_char_id as u64));
    // A send only queues; the caster's transport has to be serviced for the
    // message to leave before another seat is watched for the answer.
    a.pump_for(Duration::from_millis(300)).await;
    let bound = c
        .wait_for(
            CHANNEL_SYSTEM,
            Duration::from_secs(3),
            |m| matches!(m, ServerWorldMsg::ChatMessage { text, .. } if text.contains("bound")),
        )
        .await;
    assert!(
        bound.is_some(),
        "a group member in the town square is bound where they stand"
    );

    // The bind written is C's OWN position, persisted; B's was never touched.
    pump_all_for(&mut [&mut a, &mut b, &mut c], Duration::from_millis(600)).await;
    let pool = db::open(&h.db_url).await.expect("open pool");
    let c_bind = db::load_character(&pool, c_char_id)
        .await
        .expect("load C")
        .bind
        .expect("C is bound")
        .pos;
    assert!(
        (c_bind.0 - 3.0).abs() < 0.5 && c_bind.2.abs() < 0.5,
        "C is bound where C stood (3, 0), not at the caster (-4, 0) or the spawn: got {c_bind:?}"
    );
    let b_bind = db::load_character(&pool, b_char_id)
        .await
        .expect("load B")
        .bind
        .expect("B is bound from birth")
        .pos;
    assert!(
        b_bind.0.abs() < 0.5 && b_bind.2.abs() < 0.5,
        "the refused bind left B at the birth bind: got {b_bind:?}"
    );
}

/// Factions, EQ's rule: a kill's hits go to every player on the mob's hate
/// list, not only the killer, and the killer's alignment moves by the victim's
/// tier. A GM Cleric spawns a Gnoll Raider (the camp's faction and tier:
/// gnoll_raiders, bad; level 3, so a 7-point hit), a plain Cleric Smites it
/// once (on the hate list, never the top damager), the GM finishes it. Both
/// lose 7 with the gnolls and gain 3.5 with the town and the Watch, with EQ's
/// lines; the GM's alignment goes from the Cleric's 400 to 408 (a bad kill's
/// +10 through the momentum rule) and the bystander's does not move. Both
/// were told their 400 on entering. Fails on the previous server, which sent
/// no FactionUpdate and no AlignmentUpdate at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_kill_pays_faction_hits_to_the_hate_list_and_moves_alignment() {
    let h = start_both().await;
    let (gm_session, gm_char, _stale) =
        provision_client(&h.auth_url, "factgm", "Factgm", "Human", "Cleric").await;
    let pool = db::open(&h.db_url).await.expect("open pool");
    db::set_account_gm(&pool, "factgm", true)
        .await
        .expect("set is_gm");
    // Level 20 for the mana of several Smites; the class seed is still 400.
    set_char_level(&h.db_url, gm_char, 20).await;
    let gm_token = request_world_token(&h.auth_url, &gm_session, gm_char).await;
    let mut gm = WorldClient::start(gm_token, &gm_session, gm_char).await;
    let (b_session, b_char, b_token) =
        provision_client(&h.auth_url, "factby", "Factby", "Human", "Cleric").await;
    let mut b = WorldClient::start(b_token, &b_session, b_char).await;

    for (who, c) in [("the GM", &mut gm), ("the bystander", &mut b)] {
        let seed = c
            .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
                matches!(m, ServerWorldMsg::AlignmentUpdate { .. })
            })
            .await;
        assert!(
            matches!(seed, Some(ServerWorldMsg::AlignmentUpdate { score }) if (score - 400.0).abs() < 1e-3),
            "{who}: a Cleric is seeded at 400 and told so on entering, got {seed:?}"
        );
    }

    // 40 HP. The GM Smites three times first (21 at least), the bystander
    // once (7, or 14 on a crit), the GM finishes it: the GM is the top
    // damager whatever the crits, so the kill and its alignment are theirs.
    gm.send_dev_spawn("Gnoll Raider", 3, 40.0, 0, 0.0, 0.0);
    let mob = match gm
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::EnemySpawn { mob_name, pos, .. } if mob_name == "Gnoll Raider" && pos.x.abs() < 6.0 && pos.z.abs() < 6.0)
        })
        .await
    {
        Some(ServerWorldMsg::EnemySpawn { id, .. }) => id,
        other => panic!("the gnoll spawns: {other:?}"),
    };
    let seen = b
        .wait_for(
            CHANNEL_SYSTEM,
            Duration::from_secs(3),
            |m| matches!(m, ServerWorldMsg::EnemySpawn { id, .. } if *id == mob),
        )
        .await;
    assert!(seen.is_some(), "the bystander sees the gnoll");
    for i in 0..3 {
        gm.send_cast_spell("Smite", Some(mob));
        let hit = gm
            .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
                matches!(m, ServerWorldMsg::Hit { attacker, target, .. }
                    if *target == mob && *attacker == gm_char as u64)
            })
            .await;
        assert!(hit.is_some(), "the GM's Smite {i} lands");
        a_breath_for_two(&mut gm, &mut b).await;
    }
    b.send_cast_spell("Smite", Some(mob));
    let hit = b
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::Hit { attacker, target, .. }
                if *target == mob && *attacker == b_char as u64)
        })
        .await;
    assert!(
        hit.is_some(),
        "the bystander's Smite lands, which puts them on the hate list"
    );
    let mut died = None;
    for _ in 0..4 {
        gm.send_cast_spell("Smite", Some(mob));
        died = gm
            .wait_for(
                CHANNEL_SYSTEM,
                Duration::from_secs(3),
                |m| matches!(m, ServerWorldMsg::EntityDied { id } if *id == mob),
            )
            .await;
        if died.is_some() {
            break;
        }
        a_breath_for_two(&mut gm, &mut b).await;
    }
    assert!(died.is_some(), "the GM's Smites kill it");

    let gm_msgs = gm.collect_for(Duration::from_secs(2)).await;
    let b_msgs = b.collect_for(Duration::from_secs(2)).await;
    for (who, msgs) in [("the GM", &gm_msgs), ("the bystander", &b_msgs)] {
        assert_eq!(
            last_standing(msgs, "gnoll_raiders"),
            Some((-7.0, APPREHENSIVE)),
            "{who}: 4 + level 3 off the gnolls; got {:?}",
            faction_updates(msgs)
        );
        assert_eq!(
            last_standing(msgs, "greyveil_town"),
            Some((3.5, INDIFFERENT)),
            "{who}: half to the town"
        );
        assert_eq!(
            last_standing(msgs, "greyveil_watch"),
            Some((3.5, INDIFFERENT)),
            "{who}: half to the Watch"
        );
        let lines = system_lines(msgs);
        for want in [
            "Your faction standing with The Gnoll Raiders has gotten worse.",
            "Your faction standing with The Folk of Greyveil has gotten better.",
            "Your faction standing with The Greyveil Watch has gotten better.",
        ] {
            assert!(
                lines.iter().any(|l| l == want),
                "{who}: missing {want:?} in {lines:?}"
            );
        }
    }
    let gm_scores = alignment_scores(&gm_msgs);
    assert!(
        gm_scores.iter().any(|s| (s - 408.0).abs() < 1e-3),
        "the killer's alignment: 400 + 10 x (1 - 400/2000) = 408, got {gm_scores:?}"
    );
    assert!(
        alignment_scores(&b_msgs).is_empty(),
        "the bystander did not kill, so no alignment moves: {:?}",
        alignment_scores(&b_msgs)
    );
}

/// A quest turn-in pays its explicit hits and its alignment delta, and an
/// NPC whose faction has been pushed down refuses as EQ's did. A GM Cleric
/// takes wolf_threat, kills five Grey Wolves (wildlife: a 5-point hit to
/// wildlife alone, nothing to anyone else, alignment unmoved by a neutral
/// kill), turns in at Aldric: the Watch +25, the town +10, alignment 400 to
/// 416. Then the GM slider on the town's standing: at -50 (Apprehensive)
/// Brom has nothing to say (rat_infestation refused at accept) but still
/// sells; at -200 (Dubious) he will not deal; the -200 survives a relog; at
/// 0 he sells again. Fails on the previous server at the first FactionUpdate.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_quest_turn_in_pays_its_hits_and_a_shunned_npc_refuses() {
    const POTION_NAME: &str = "Minor Healing Potion";
    let h = start_both().await;
    let (session, char_id, _stale) =
        provision_client(&h.auth_url, "questfac", "Questfac", "Human", "Cleric").await;
    let pool = db::open(&h.db_url).await.expect("open pool");
    db::set_account_gm(&pool, "questfac", true)
        .await
        .expect("set is_gm");
    // Level 20 for the mana of five Smites; a purse for the merchant.
    set_char_level(&h.db_url, char_id, 20).await;
    sqlx::query("UPDATE characters SET copper = 100000 WHERE id = ?1")
        .bind(char_id)
        .execute(&pool)
        .await
        .expect("seed coin");
    let token = request_world_token(&h.auth_url, &session, char_id).await;
    let mut a = WorldClient::start(token, &session, char_id).await;
    let _ = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::InventorySnapshot { .. })
        })
        .await
        .expect("in the world");

    a.send_accept_quest("wolf_threat");
    for i in 0..5 {
        a.send_dev_spawn("Grey Wolf", 1, 1.0, 0, 0.0, 0.0);
        let mob = match a
            .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
                matches!(m, ServerWorldMsg::EnemySpawn { mob_name, pos, .. } if mob_name == "Grey Wolf" && pos.x.abs() < 6.0 && pos.z.abs() < 6.0)
            })
            .await
        {
            Some(ServerWorldMsg::EnemySpawn { id, .. }) => id,
            other => panic!("wolf {i} spawns: {other:?}"),
        };
        a.send_cast_spell("Smite", Some(mob));
        let died = a
            .wait_for(
                CHANNEL_SYSTEM,
                Duration::from_secs(3),
                |m| matches!(m, ServerWorldMsg::EntityDied { id } if *id == mob),
            )
            .await;
        assert!(died.is_some(), "wolf {i} dies to one Smite");
        a_breath(&mut a).await;
    }
    // The five kills' own hits: wildlife alone, 5 a kill, and no alignment.
    let after_kills = a.collect_for(Duration::from_millis(500)).await;
    assert_eq!(
        last_standing(&after_kills, "wildlife"),
        Some((-25.0, APPREHENSIVE)),
        "five wolves at 4 + 1 each, and under zero is Apprehensive: {:?}",
        faction_updates(&after_kills)
    );
    assert!(
        faction_updates(&after_kills)
            .iter()
            .all(|(id, _, _)| id == "wildlife"),
        "wildlife has no enemies, so nobody else moves: {:?}",
        faction_updates(&after_kills)
    );

    a.send_complete_quest("wolf_threat");
    let msgs = a.collect_for(Duration::from_secs(2)).await;
    assert!(
        msgs.iter().any(|m| matches!(m, ServerWorldMsg::QuestCompleted { quest_id } if quest_id == "wolf_threat")),
        "the turn-in lands: {msgs:?}"
    );
    assert_eq!(
        last_standing(&msgs, "greyveil_watch"),
        Some((25.0, INDIFFERENT)),
        "{:?}",
        faction_updates(&msgs)
    );
    assert_eq!(
        last_standing(&msgs, "greyveil_town"),
        Some((10.0, INDIFFERENT))
    );
    let scores = alignment_scores(&msgs);
    assert!(
        scores.iter().any(|s| (s - 416.0).abs() < 1e-3),
        "400 + 20 x 0.8 from the quest, nothing from five neutral kills: {scores:?}"
    );

    // Apprehensive: the quest giver has nothing to say, the merchant still sells.
    a.send_dev_set_faction("greyveil_town", -50.0);
    let set = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(2), |m| {
            matches!(m, ServerWorldMsg::FactionUpdate { faction_id, .. } if faction_id == "greyveil_town")
        })
        .await;
    assert!(
        matches!(set, Some(ServerWorldMsg::FactionUpdate { standing, level, .. }) if standing == -50.0 && level == APPREHENSIVE),
        "{set:?}"
    );
    a.send_accept_quest("rat_infestation");
    let rejected = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(2), |m| {
            matches!(m, ServerWorldMsg::QuestRejected { quest_id, .. } if quest_id == "rat_infestation")
        })
        .await;
    match rejected {
        Some(ServerWorldMsg::QuestRejected {
            reason, rollback, ..
        }) => {
            assert_eq!(reason, "Brom has nothing to say to you.");
            assert!(rollback, "an accept-phase refusal rolls the journal back");
        }
        other => panic!("Brom at Apprehensive refuses the quest: {other:?}"),
    }
    a.send_buy_item(0, POTION_NAME, 1);
    let bought = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(2), |m| {
            matches!(m, ServerWorldMsg::InventoryDelta { .. })
        })
        .await;
    assert!(bought.is_some(), "at Apprehensive the merchant still sells");

    // Dubious: he will not deal.
    a.send_dev_set_faction("greyveil_town", -200.0);
    let set = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(2), |m| {
            matches!(m, ServerWorldMsg::FactionUpdate { faction_id, standing, .. }
                if faction_id == "greyveil_town" && *standing == -200.0)
        })
        .await;
    assert!(
        matches!(set, Some(ServerWorldMsg::FactionUpdate { level, .. }) if level == DUBIOUS),
        "{set:?}"
    );
    a.send_buy_item(0, POTION_NAME, 1);
    let msgs = a.collect_for(Duration::from_millis(1500)).await;
    assert!(
        !msgs
            .iter()
            .any(|m| matches!(m, ServerWorldMsg::InventoryDelta { .. })),
        "at Dubious no sale: {msgs:?}"
    );
    let lines = system_lines(&msgs);
    assert!(
        lines.iter().any(|l| l == "Brom will not deal with you."),
        "the refusal names the merchant: {lines:?}"
    );

    // The standing persists: leave, come back, and the snapshot carries it.
    a.send_disconnect();
    a.pump_for(Duration::from_millis(500)).await;
    drop(a);
    tokio::time::sleep(Duration::from_millis(500)).await;
    let token = request_world_token(&h.auth_url, &session, char_id).await;
    let mut a = WorldClient::start(token, &session, char_id).await;
    let snap = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::FactionUpdate { faction_id, .. } if faction_id == "greyveil_town")
        })
        .await;
    assert!(
        matches!(snap, Some(ServerWorldMsg::FactionUpdate { standing, .. }) if standing == -200.0),
        "the town's -200 survives a relog: {snap:?}"
    );
    a.send_dev_set_faction("greyveil_town", 0.0);
    let _ = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(2), |m| {
            matches!(m, ServerWorldMsg::FactionUpdate { faction_id, standing, .. }
                if faction_id == "greyveil_town" && *standing == 0.0)
        })
        .await
        .expect("back to Indifferent");
    a.send_buy_item(0, POTION_NAME, 1);
    let bought = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(2), |m| {
            matches!(m, ServerWorldMsg::InventoryDelta { .. })
        })
        .await;
    assert!(bought.is_some(), "Indifferent again, the merchant sells");
}

/// The exploit lens on the GM slider, and the consider line. A plain account's
/// DevSetFaction is a silent no-op (nothing moves, nothing is said: a reply
/// would be an oracle for dev commands). Consider answers for an NPC by id
/// with EQ's faction phrase, for a mob with the faction phrase and the level
/// phrase (a level 10 Gnoll Brute to a level 1 is red), and with one line for
/// anything else: an unknown id and a player id get the same words. The GM
/// at Kindly with the town is kindly considered by Brom.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_plain_account_cannot_set_faction_and_a_consider_names_no_standing() {
    if std::env::var("PD_DEV_CMDS").as_deref() == Ok("1") {
        eprintln!("skipping: PD_DEV_CMDS=1 makes every connection dev");
        return;
    }
    let h = start_both().await;
    let (gm_session, gm_char, _stale) =
        provision_client(&h.auth_url, "congm", "Congm", "Human", "Warrior").await;
    let pool = db::open(&h.db_url).await.expect("open pool");
    db::set_account_gm(&pool, "congm", true)
        .await
        .expect("set is_gm");
    let gm_token = request_world_token(&h.auth_url, &gm_session, gm_char).await;
    let mut gm = WorldClient::start(gm_token, &gm_session, gm_char).await;
    let (p_session, p_char, p_token) =
        provision_client(&h.auth_url, "conplain", "Conplain", "Human", "Warrior").await;
    let mut plain = WorldClient::start(p_token, &p_session, p_char).await;
    let _ = plain
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::InventorySnapshot { .. })
        })
        .await
        .expect("in the world");

    plain.send_dev_set_faction("greyveil_town", 2000.0);
    let msgs = plain.collect_for(Duration::from_secs(1)).await;
    assert!(
        faction_updates(&msgs).is_empty(),
        "a plain account's slider moves nothing: {msgs:?}"
    );
    assert!(
        !system_lines(&msgs).iter().any(|l| l.contains("standing")),
        "and is told nothing: {:?}",
        system_lines(&msgs)
    );

    let con = |c: &mut WorldClient, npc: Option<&str>, target: Option<u64>| {
        c.send_consider(npc, target);
    };
    con(&mut plain, Some("brom"), None);
    let line = plain
        .wait_for(
            CHANNEL_SYSTEM,
            Duration::from_secs(2),
            |m| matches!(m, ServerWorldMsg::ChatMessage { text, .. } if text.starts_with("Brom")),
        )
        .await;
    assert!(
        matches!(&line, Some(ServerWorldMsg::ChatMessage { text, .. }) if text == "Brom regards you indifferently."),
        "{line:?}"
    );
    for (what, npc, target) in [
        ("an unknown id", Some("nobody"), None),
        ("a player id", None, Some(gm_char as u64)),
    ] {
        con(&mut plain, npc, target);
        let line = plain
            .wait_for(CHANNEL_SYSTEM, Duration::from_secs(2), |m| {
                matches!(m, ServerWorldMsg::ChatMessage { text, .. } if text.contains("consider"))
            })
            .await;
        assert!(
            matches!(&line, Some(ServerWorldMsg::ChatMessage { text, .. }) if text == "You cannot consider that."),
            "{what}: {line:?}"
        );
    }

    gm.send_dev_spawn("Gnoll Brute", 10, 100.0, 0, 0.0, 0.0);
    let mob = match gm
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::EnemySpawn { mob_name, pos, .. } if mob_name == "Gnoll Brute" && pos.x.abs() < 6.0 && pos.z.abs() < 6.0)
        })
        .await
    {
        Some(ServerWorldMsg::EnemySpawn { id, .. }) => id,
        other => panic!("the brute spawns: {other:?}"),
    };
    let _ = plain
        .wait_for(
            CHANNEL_SYSTEM,
            Duration::from_secs(3),
            |m| matches!(m, ServerWorldMsg::EnemySpawn { id, .. } if *id == mob),
        )
        .await
        .expect("the plain account sees the brute");
    con(&mut plain, None, Some(mob));
    let line = plain
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(2), |m| {
            matches!(m, ServerWorldMsg::ChatMessage { text, .. } if text.starts_with("Gnoll Brute"))
        })
        .await;
    assert!(
        matches!(&line, Some(ServerWorldMsg::ChatMessage { text, .. })
            if text == "Gnoll Brute regards you indifferently. What would you like your tombstone to say?"),
        "{line:?}"
    );

    gm.send_dev_set_faction("greyveil_town", 600.0);
    let set = gm
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(2), |m| {
            matches!(m, ServerWorldMsg::FactionUpdate { faction_id, .. } if faction_id == "greyveil_town")
        })
        .await;
    assert!(
        matches!(set, Some(ServerWorldMsg::FactionUpdate { standing, level, .. }) if standing == 600.0 && level == KINDLY),
        "{set:?}"
    );
    con(&mut gm, Some("brom"), None);
    let line = gm
        .wait_for(
            CHANNEL_SYSTEM,
            Duration::from_secs(2),
            |m| matches!(m, ServerWorldMsg::ChatMessage { text, .. } if text.starts_with("Brom")),
        )
        .await;
    assert!(
        matches!(&line, Some(ServerWorldMsg::ChatMessage { text, .. }) if text == "Brom kindly considers you."),
        "{line:?}"
    );
}
