//! GM tools and the books: dev commands gated and audited, bans, the dev despawn,
//! and the item ledger.

use crate::common::*;

/// `DeathBroadcast` is a dev command (housekeeping audit B1, 2026-10-23,
/// `docs/design/server_fall_damage.md` slice 0). Ungated it killed the sender on
/// demand: a free trip to the bind point and a wipe of every mob's hate. A plain
/// account's send is ignored and writes nothing; a GM's kills them (the Test
/// Panel's Trigger Death) and writes one audit row; a second send while dead,
/// which is what an honest client does after every death, writes no more. On
/// the previous handler the plain account died.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn death_broadcast_is_a_dev_command() {
    if std::env::var("PD_DEV_CMDS").as_deref() == Ok("1") {
        eprintln!(
            "skipping death_broadcast_is_a_dev_command: PD_DEV_CMDS=1 makes every connection dev"
        );
        return;
    }
    let h = start_both().await;
    let pool = db::open(&h.db_url).await.expect("open pool");
    let death_rows = |pool: sqlx::SqlitePool| async move {
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM gm_actions WHERE command = 'death_broadcast'",
        )
        .fetch_one(&pool)
        .await
        .expect("count death_broadcast rows")
    };

    // A plain account: the forged death does nothing.
    let (pl_session, pl_char, pl_token) =
        provision_client(&h.auth_url, "deathplain", "Liveson", "Human", "Warrior").await;
    let mut plain = WorldClient::start(pl_token, &pl_session, pl_char).await;
    let pl_id = pl_char as u64;
    plain.send_death();
    let died = plain
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(2), |m| {
            matches!(m, ServerWorldMsg::HealthUpdate { id, hp, .. } if *id == pl_id && *hp <= 0.01)
        })
        .await;
    assert!(
        died.is_none(),
        "a plain account's DeathBroadcast must not kill it"
    );
    assert_eq!(
        death_rows(pool.clone()).await,
        0,
        "a refused dev command writes no audit row"
    );

    // A GM: the Test Panel's Trigger Death still works, and is audited once.
    let (gm_session, gm_char, _stale) =
        provision_client(&h.auth_url, "deathgm", "Fallsdown", "Human", "Warrior").await;
    let gm_token = gm_world_token(&h.auth_url, &h.db_url, "deathgm", &gm_session, gm_char).await;
    let mut gm = WorldClient::start(gm_token, &gm_session, gm_char).await;
    let gm_id = gm_char as u64;
    gm.send_death();
    let died = gm
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::HealthUpdate { id, hp, .. } if *id == gm_id && *hp <= 0.01)
        })
        .await;
    assert!(died.is_some(), "a GM's DeathBroadcast kills them");
    // The honest client's follow-up send, already dead: no second row.
    gm.send_death();
    gm.pump_for(Duration::from_millis(600)).await;
    assert_eq!(
        death_rows(pool.clone()).await,
        1,
        "one death, one audit row"
    );
}

/// Phase 1 keystone: the per-account GM gate. A dev/GM command must APPLY for an
/// `is_gm` account and be a silent no-op for a plain account, when the server is
/// NOT in process-wide dev mode. This exercises the whole real path: the account
/// flag is packed into the signed connect token's `user_data`, read into
/// `PerConnection.is_gm` at connect, and every dev command gates on
/// `can_use_dev_cmds()` (`is_dev || is_gm`). `GrantQuestXp` is the probe â€” the
/// GM sees an `XpGained`, the plain account sees nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn is_gm_gates_dev_commands() {
    // Only meaningful when the process is NOT in dev mode: `dev_cmds_enabled()`
    // reads PD_DEV_CMDS once, process-wide, so if it were "1" both accounts
    // would be dev and the plain-account assertion would be a false failure.
    if std::env::var("PD_DEV_CMDS").as_deref() == Ok("1") {
        eprintln!("skipping is_gm_gates_dev_commands: PD_DEV_CMDS=1 makes every connection dev");
        return;
    }

    let h = start_both().await;

    // GM account: provision (mints a token while is_gm is still 0), flip the DB
    // flag, then mint a FRESH token that actually carries is_gm=1.
    let (gm_session, gm_char, _stale_token) =
        provision_client(&h.auth_url, "gmuser", "GmHero", "Human", "Warrior").await;
    let pool = db::open(&h.db_url).await.expect("open pool");
    let prev = db::set_account_gm(&pool, "gmuser", true)
        .await
        .expect("set is_gm");
    assert_eq!(prev, Some(false), "account existed and was non-GM before");
    let gm_token = request_world_token(&h.auth_url, &gm_session, gm_char).await;
    let mut gm = WorldClient::start(gm_token, &gm_session, gm_char).await;

    // Plain account: is_gm stays 0.
    let (pl_session, pl_char, pl_token) =
        provision_client(&h.auth_url, "plainuser", "PlainJane", "Human", "Warrior").await;
    let mut plain = WorldClient::start(pl_token, &pl_session, pl_char).await;

    // Every client gets a connect-time XpGained { amount: 0 } to seed its xp bar
    // (tick.rs). Match on the PROBE amount, not just any XpGained, so the seed is
    // not mistaken for a GrantQuestXp-caused gain.
    const PROBE_XP: i32 = 250;

    // GM sends the dev command â†’ the server applies it and fans the gain back.
    gm.send_grant_quest_xp(PROBE_XP);
    let gm_xp = gm
        .wait_for(
            CHANNEL_SYSTEM,
            Duration::from_secs(3),
            |m| matches!(m, ServerWorldMsg::XpGained { amount, .. } if *amount == PROBE_XP),
        )
        .await;
    assert!(
        gm_xp.is_some(),
        "GM account: GrantQuestXp should apply and produce an XpGained(amount={PROBE_XP})"
    );

    // Plain account sends the same â†’ the server ignores it, so no gain arrives
    // (only the connect-time seed, which the amount filter excludes).
    plain.send_grant_quest_xp(PROBE_XP);
    let plain_xp = plain
        .wait_for(
            CHANNEL_SYSTEM,
            Duration::from_secs(2),
            |m| matches!(m, ServerWorldMsg::XpGained { amount, .. } if *amount == PROBE_XP),
        )
        .await;
    assert!(
        plain_xp.is_none(),
        "plain account: GrantQuestXp must be a silent no-op (no gain applied)"
    );
}

/// Every AUTHORIZED dev/GM command lands in `gm_actions`, tagged with the
/// authority that allowed it; the same commands from a plain account are
/// ignored by the gate and leave no row (a refused attempt must not be a way
/// for any client to make the server write).
#[tokio::test]
async fn gm_commands_are_audited_only_when_authorized() {
    use sqlx::Row;
    // Same guard as is_gm_gates_dev_commands: with PD_DEV_CMDS=1 every
    // connection is dev, so the plain account would be authorized too.
    if std::env::var("PD_DEV_CMDS").as_deref() == Ok("1") {
        eprintln!("skipping gm_commands_are_audited_only_when_authorized: PD_DEV_CMDS=1");
        return;
    }
    let h = start_both().await;
    let (gm_session, gm_char, _stale_token) =
        provision_client(&h.auth_url, "auditgm", "Auditgm", "Human", "Warrior").await;
    let pool = db::open(&h.db_url).await.expect("open pool");
    db::set_account_gm(&pool, "auditgm", true)
        .await
        .expect("set is_gm");
    let gm_token = request_world_token(&h.auth_url, &gm_session, gm_char).await;
    let (pl_session, pl_char, pl_token) =
        provision_client(&h.auth_url, "auditpl", "Auditpl", "Human", "Warrior").await;

    let mut gm = WorldClient::start(gm_token, &gm_session, gm_char).await;
    let mut plain = WorldClient::start(pl_token, &pl_session, pl_char).await;

    gm.send_grant_quest_xp(250);
    gm.send_dev_spawn("Audit Dummy", 1, 10.0, 0, 0.0, 0.0);
    plain.send_grant_quest_xp(250);
    plain.send_dev_spawn("Forged Dummy", 1, 10.0, 0, 0.0, 0.0);
    // Probe: the plain account's self-inspect rides the same ordered channel
    // AFTER its two dev commands, and its answer is sent later in the tick
    // than the audit flush. Once it arrives, both commands have been handled
    // and any row they caused would be committed, so "no row" is not vacuous.
    plain.send_inspect_player(pl_char);

    // Wait on effects, not on a clock. The GM's dev spawn is fanned in the
    // same tick as the flush that records it, after it.
    wait_on_a_pumping_b(
        &mut gm,
        &mut plain,
        Duration::from_secs(5),
        |m| matches!(m, ServerWorldMsg::EnemySpawn { mob_name, .. } if mob_name == "Audit Dummy"),
    )
    .await
    .expect("the GM's dev spawn takes effect");
    wait_on_a_pumping_b(&mut plain, &mut gm, Duration::from_secs(5), |m| {
        matches!(m, ServerWorldMsg::InspectResult { target_char_id, .. }
            if *target_char_id == pl_char)
    })
    .await
    .expect("the plain account's probe is answered");

    let rows = sqlx::query(
        "SELECT a.username AS actor, g.target_char, g.command, g.args
         FROM gm_actions g JOIN accounts a ON a.id = g.actor_account
         ORDER BY g.id",
    )
    .fetch_all(&pool)
    .await
    .expect("read gm_actions");
    let seen: Vec<(String, i64, String, String)> = rows
        .iter()
        .map(|r| {
            (
                r.get("actor"),
                r.get("target_char"),
                r.get("command"),
                r.get("args"),
            )
        })
        .collect();
    assert_eq!(
        seen.len(),
        2,
        "exactly the GM's two commands are recorded, got {seen:?}"
    );
    for (actor, target_char, _, args) in &seen {
        assert_eq!(actor, "auditgm", "only the authorized account appears");
        assert_eq!(*target_char, gm_char);
        assert!(
            args.ends_with("via=gm"),
            "the row names the authority: {args}"
        );
    }
    assert_eq!(seen[0].2, "grant_xp");
    assert!(seen[0].3.starts_with("amount=250"));
    assert_eq!(seen[1].2, "dev_spawn_mob");
    assert!(seen[1].3.contains("Audit Dummy"));
}

/// The Test Panel's crafting-materials button sends about 34 dev commands in
/// one frame. Every one of them must be recorded (the first cut of the audit
/// capped a tick at 8 rows and let the rest RUN unrecorded), and a burst
/// that size is inside the budget, so nothing is refused.
#[tokio::test]
async fn gm_command_burst_is_recorded_whole() {
    if std::env::var("PD_DEV_CMDS").as_deref() == Ok("1") {
        eprintln!("skipping gm_command_burst_is_recorded_whole: PD_DEV_CMDS=1");
        return;
    }
    let h = start_both().await;
    let (gm_session, gm_char, _stale_token) =
        provision_client(&h.auth_url, "burstgm", "Burstgm", "Human", "Warrior").await;
    let pool = db::open(&h.db_url).await.expect("open pool");
    db::set_account_gm(&pool, "burstgm", true)
        .await
        .expect("set is_gm");
    let gm_token = request_world_token(&h.auth_url, &gm_session, gm_char).await;
    let mut gm = WorldClient::start(gm_token, &gm_session, gm_char).await;

    for _ in 0..34 {
        gm.send_grant_quest_xp(1);
    }
    // Marker: ordered after the burst, fanned after the flush that records it.
    gm.send_dev_spawn("Burst Marker", 1, 10.0, 0, 0.0, 0.0);
    gm.wait_for(
        CHANNEL_SYSTEM,
        Duration::from_secs(5),
        |m| matches!(m, ServerWorldMsg::EnemySpawn { mob_name, .. } if mob_name == "Burst Marker"),
    )
    .await
    .expect("the marker spawn takes effect");

    let recorded: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM gm_actions WHERE command = 'grant_xp'")
            .fetch_one(&pool)
            .await
            .expect("count grant_xp rows");
    assert_eq!(recorded, 34, "every command in the burst has its own row");
    let overflow: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM gm_actions WHERE command = 'audit_overflow'")
            .fetch_one(&pool)
            .await
            .expect("count overflow rows");
    assert_eq!(overflow, 0, "an honest burst is inside the budget");
}

/// A ban holds at world connect: a connect token minted BEFORE the ban is
/// still valid for its short life, and must not get the account into the
/// world. The server answers the transport connect with a BannedNow kick.
#[tokio::test]
async fn banned_account_is_refused_at_world_connect() {
    let h = start_both().await;
    // The token is minted here, while the account is still in good standing.
    let (_session, _char_id, token) =
        provision_client(&h.auth_url, "bannedguy", "Bannedguy", "Human", "Warrior").await;
    let pool = db::open(&h.db_url).await.expect("open pool");
    db::set_account_banned(&pool, "bannedguy", true, Some("test ban"))
        .await
        .expect("ban")
        .expect("account exists");

    // The first half of WorldClient::start: the renet handshake only.
    let connect_token =
        ConnectToken::read(&mut Cursor::new(&token[..])).expect("ConnectToken::read");
    let socket = UdpSocket::bind("127.0.0.1:0").expect("udp bind");
    socket.set_nonblocking(true).expect("nonblocking");
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    let auth = ClientAuthentication::Secure { connect_token };
    let mut transport = NetcodeClientTransport::new(now, auth, socket).expect("transport");
    let mut client = RenetClient::new(connection_config_matching_server());

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut kick: Option<(protocol::world::KickCode, String)> = None;
    'wait: while Instant::now() < deadline {
        tick_one(&mut client, &mut transport);
        while let Some(bytes) = client.receive_message(CHANNEL_SYSTEM) {
            if let Ok((ServerWorldMsg::Kick { code, reason, .. }, _)) =
                bincode::serde::decode_from_slice::<ServerWorldMsg, _>(&bytes, bincode_cfg())
            {
                kick = Some((code, reason));
                break 'wait;
            }
        }
        tokio::time::sleep(TICK_DT).await;
    }
    let (code, reason) = kick.expect("a banned account's connect is answered with a Kick");
    assert!(
        matches!(code, protocol::world::KickCode::BannedNow),
        "the kick says why (got {code:?}: {reason})"
    );
}

/// A ban reaches a character that is ALREADY in the world. Login, session use
/// and world connect all refuse a banned account, but none of them touched a
/// live connection, so removing an online griefer used to take a server
/// restart. The world loop's ban sweep kicks it within BAN_SWEEP_INTERVAL,
/// says why, and then drops the transport itself.
#[tokio::test]
async fn banned_account_is_kicked_out_of_the_world() {
    let h = start_both().await;
    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "onlineban", "Onlineban", "Human", "Warrior").await;
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    a.pump_for(Duration::from_millis(300)).await;

    let pool = db::open(&h.db_url).await.expect("open pool");
    db::set_account_banned(&pool, "onlineban", true, Some("test ban"))
        .await
        .expect("ban")
        .expect("account exists");

    // Up to two sweep intervals (10 s each) plus slack, in case the ban lands
    // just after a sweep. Heartbeat while waiting: only app-layer messages
    // keep a connection from the idle timeout.
    let deadline = Instant::now() + Duration::from_secs(25);
    let mut kick: Option<protocol::world::KickCode> = None;
    // Heartbeat by wall clock, not by loop count: under load an iteration
    // stretches, and 80 slow iterations would outlast the 10 s idle timeout.
    a.send_heartbeat();
    let mut last_heartbeat = Instant::now();
    'wait: while Instant::now() < deadline {
        tick_one(&mut a.client, &mut a.transport);
        if last_heartbeat.elapsed() >= Duration::from_secs(3) {
            a.send_heartbeat();
            last_heartbeat = Instant::now();
        }
        while let Some(bytes) = a.client.receive_message(CHANNEL_SYSTEM) {
            if let Ok((ServerWorldMsg::Kick { code, .. }, _)) =
                bincode::serde::decode_from_slice::<ServerWorldMsg, _>(&bytes, bincode_cfg())
            {
                kick = Some(code);
                break 'wait;
            }
        }
        tokio::time::sleep(TICK_DT).await;
    }
    assert!(
        matches!(kick, Some(protocol::world::KickCode::BannedNow)),
        "a banned account's live session is kicked, and told why (got {kick:?})"
    );

    // The server drops the transport itself a moment later; it does not rely
    // on the client leaving.
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && !a.client.is_disconnected() {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }
    assert!(
        a.client.is_disconnected(),
        "the kicked connection is dropped by the server"
    );
}

/// The Test Panel's "Despawn All Enemies" removes the mobs on the SERVER. It
/// used to free only the pressing client's own nodes, so "cleared" mobs stayed
/// in the world, invisible to that player and still fighting everyone else.
/// A (a GM Cleric) spawns two dummies; B (a plain Beast Master beside A, with
/// its warder) asks first and nothing happens; A asks and B sees both dummies
/// despawn while the warder stays; a Smite at a despawned id is refused,
/// because the server no longer has it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dev_despawn_removes_enemies_on_the_server() {
    let h = start_both().await;
    let (a_session, a_char_id, _stale) =
        provision_client(&h.auth_url, "sweeper", "Sweepa", "Human", "Cleric").await;
    let (b_session, b_char_id, b_token) =
        provision_client(&h.auth_url, "sweepwatch", "Watcha", "Human", "Beast Master").await;
    let pool = db::open(&h.db_url).await.expect("open pool");
    db::set_account_gm(&pool, "sweeper", true)
        .await
        .expect("set is_gm");
    let a_token = request_world_token(&h.auth_url, &a_session, a_char_id).await;
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    let mut b = WorldClient::start(b_token, &b_session, b_char_id).await;

    let warder: u64 = match b
        .wait_for(
            CHANNEL_SYSTEM,
            Duration::from_secs(3),
            |m| matches!(m, ServerWorldMsg::PetSpawn { owner, .. } if *owner == b_char_id as u64),
        )
        .await
        .expect("B's warder is summoned on entering the world")
    {
        ServerWorldMsg::PetSpawn { id, .. } => id,
        _ => unreachable!(),
    };

    let mut dummies: Vec<u64> = Vec::new();
    for name in ["Despawn Dummy", "Despawn Target"] {
        a.send_dev_spawn(name, 1, 500.0, 0, 0.0, 0.0);
        let spawn = a
            .wait_for(
                CHANNEL_SYSTEM,
                Duration::from_secs(3),
                |m| matches!(m, ServerWorldMsg::EnemySpawn { mob_name, .. } if mob_name == name),
            )
            .await
            .expect("the dummy spawns");
        match spawn {
            ServerWorldMsg::EnemySpawn { id, .. } => dummies.push(id),
            _ => unreachable!(),
        }
    }

    // A plain account's request is ignored. Only meaningful when the process
    // is not in dev mode, where every connection may use dev commands.
    if std::env::var("PD_DEV_CMDS").as_deref() != Ok("1") {
        b.send_dev_despawn_enemies();
        b.pump_for(Duration::from_millis(300)).await;
        let gone = a
            .wait_for(
                CHANNEL_SYSTEM,
                Duration::from_secs(1),
                |m| matches!(m, ServerWorldMsg::EntityDespawn { id } if dummies.contains(id)),
            )
            .await;
        assert!(gone.is_none(), "a plain account cannot despawn anything");
    }

    a.send_dev_despawn_enemies();
    a.pump_for(Duration::from_millis(300)).await;
    let mut despawned: Vec<u64> = Vec::new();
    while despawned.len() < dummies.len() {
        let msg = b
            .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
                matches!(m, ServerWorldMsg::EntityDespawn { id }
                    if dummies.contains(id) || *id == warder)
            })
            .await
            .expect("the bystander sees every dummy despawn");
        let ServerWorldMsg::EntityDespawn { id } = msg else {
            unreachable!()
        };
        assert_ne!(id, warder, "a player's pet is not an enemy to despawn");
        despawned.push(id);
    }
    let warder_gone = b
        .wait_for(
            CHANNEL_SYSTEM,
            Duration::from_secs(1),
            |m| matches!(m, ServerWorldMsg::EntityDespawn { id } if *id == warder),
        )
        .await;
    assert!(warder_gone.is_none(), "the warder stays");

    // The GM is told the count (camp mobs in view may add to the two).
    let told = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(2), |m| {
            matches!(m, ServerWorldMsg::ChatMessage { text, .. } if text.starts_with("Despawned "))
        })
        .await
        .expect("the GM is told how many went");
    let ServerWorldMsg::ChatMessage { text, .. } = told else {
        unreachable!()
    };
    let count: usize = text
        .trim_start_matches("Despawned ")
        .trim_end_matches(" enemies.")
        .parse()
        .expect("the line carries a count");
    assert!(
        count >= dummies.len(),
        "the count covers both dummies ({text})"
    );

    a.send_cast_spell("Smite", Some(dummies[0]));
    let refused = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::CastFail { caster, reason }
                if *caster == a_char_id as u64 && reason.contains("no longer here"))
        })
        .await;
    assert!(
        refused.is_some(),
        "the server no longer has the mob, so a Smite at it is refused"
    );

    // Audited like every dev command: one row, the GM's; the plain account's
    // refused request wrote nothing.
    let rows: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM gm_actions WHERE command = 'dev_despawn_enemies'")
            .fetch_one(&pool)
            .await
            .expect("read gm_actions");
    assert_eq!(
        rows, 1,
        "the GM's despawn is recorded, the plain account's is not"
    );

    a.send_disconnect();
    b.send_disconnect();
    a.pump_for(Duration::from_millis(200)).await;
    b.pump_for(Duration::from_millis(200)).await;
}

/// The item ledger: a sale, a purchase and a drop leave the seller's rows, the
/// pickup leaves the picker's, each written with the transaction that
/// persisted the move (the sale and the purchase ride the logout flush, the
/// drop and the pickup their immediate saves), and the conservation query
/// finds the books balanced. A stack then planted straight into the table,
/// the shape of every dupe, is the one mismatch, named by account and item.
/// Fails on the previous server at the first row assertion: no table.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_ledger_records_a_sale_a_purchase_a_drop_and_a_pickup_and_balances() {
    const POTION: &str = "res://data/loot/items/minor_healing_potion.tres";
    let h = start_both().await;
    let (a_session, a_char, a_token) =
        provision_client(&h.auth_url, "ledgera", "Ledgera", "Human", "Warrior").await;
    let (b_session, b_char, b_token) =
        provision_client(&h.auth_url, "ledgerb", "Ledgerb", "Human", "Warrior").await;
    give_char_item(&h.db_url, a_char, "base", 0, BREAD, 3).await;
    {
        let pool = projectdawn_server::db::open(&h.db_url)
            .await
            .expect("open pool");
        // At the spawn, in reach of the merchant, with coin for the potion.
        sqlx::query("UPDATE characters SET copper = 1000, pos_x = 0.0, pos_z = 0.0 WHERE id = ?1")
            .bind(a_char)
            .execute(&pool)
            .await
            .expect("seed");
    }
    seed_ledger_openings(&h.db_url).await;
    let mut a = WorldClient::start(a_token, &a_session, a_char).await;
    let mut b = WorldClient::start(b_token, &b_session, b_char).await;
    a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::InventorySnapshot { .. })
    })
    .await
    .expect("snapshot");
    b.pump_for(Duration::from_millis(200)).await;

    a.send_sell_item(SlotRef::BaseSlot { idx: 0 }, 1);
    // The sale's own delta (the stack 3 to 2), not the enter-world wallet
    // send: the buy below must reach a later tick (within one tick the
    // server applies buys before sells, which would reorder the rows).
    a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::InventoryDelta { location, slot: 0, item_path: Some(p), count: 2 }
            if location == "base" && p == BREAD)
    })
    .await
    .expect("the sale takes one loaf");
    a.send_buy_item(0, "Minor Healing Potion", 1);
    a.wait_for(
        CHANNEL_SYSTEM,
        Duration::from_secs(3),
        |m| matches!(m, ServerWorldMsg::InventoryDelta { item_path: Some(p), .. } if p == POTION),
    )
    .await
    .expect("the potion arrives");
    a.send_drop_item("base", 0, 1);
    a.pump_for(Duration::from_millis(100)).await;
    let bag_id = match b
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::LootBagSpawn { items, .. }
                if items.iter().any(|(p, _)| p == BREAD))
        })
        .await
        .expect("the bag reaches B")
    {
        ServerWorldMsg::LootBagSpawn { bag_id, .. } => bag_id,
        _ => unreachable!(),
    };
    b.send_loot_all(bag_id);
    b.wait_for(
        CHANNEL_SYSTEM,
        Duration::from_secs(3),
        |m| matches!(m, ServerWorldMsg::InventoryDelta { item_path: Some(p), .. } if p == BREAD),
    )
    .await
    .expect("the loaf reaches B");

    a.send_disconnect();
    b.send_disconnect();
    a.pump_for(Duration::from_millis(300)).await;
    b.pump_for(Duration::from_millis(300)).await;

    let a_rows = ledger_rows(&h.db_url, a_char).await;
    let actions: Vec<&str> = a_rows.iter().map(|r| r.0.as_str()).collect();
    assert_eq!(
        actions,
        vec!["sell", "buy", "drop"],
        "A's rows in order: {a_rows:?}"
    );
    let sale = &a_rows[0];
    assert_eq!(
        (sale.1.as_deref(), sale.2, sale.3),
        (Some(BREAD), 1, -1),
        "{sale:?}"
    );
    assert!(sale.4 > 0, "the sale credits coin: {sale:?}");
    let purchase = &a_rows[1];
    assert_eq!(
        (purchase.1.as_deref(), purchase.2, purchase.3),
        (Some(POTION), 1, 1),
        "{purchase:?}"
    );
    assert!(purchase.4 < 0, "the purchase costs coin: {purchase:?}");
    let drop = &a_rows[2];
    assert_eq!(
        (drop.1.as_deref(), drop.2, drop.3, drop.4),
        (Some(BREAD), 1, -1, 0),
        "{drop:?}"
    );
    let b_rows = ledger_rows(&h.db_url, b_char).await;
    assert_eq!(
        b_rows,
        vec![("pickup".to_string(), Some(BREAD.to_string()), 1, 1, 0)],
        "B's one row is the pickup"
    );
    assert!(
        ledger_mismatches(&h.db_url).await.is_empty(),
        "the books balance after every move"
    );

    {
        let pool = projectdawn_server::db::open(&h.db_url)
            .await
            .expect("open pool");
        sqlx::query(
            "INSERT INTO character_items (char_id, location, slot, item_path, count) VALUES (?1, 'base', 7, ?2, 5)",
        )
        .bind(b_char)
        .bind(BREAD)
        .execute(&pool)
        .await
        .expect("plant a dupe");
    }
    let found = ledger_mismatches(&h.db_url).await;
    assert_eq!(
        found.len(),
        1,
        "the planted stack is the one mismatch: {found:?}"
    );
    assert_eq!(
        (
            found[0].item_path.as_deref(),
            found[0].ledger,
            found[0].held
        ),
        (Some(BREAD), 1, 6)
    );
}

/// The bank in the ledger: a vault deposit and withdrawal (delta 0, the
/// vault named: the item stays the account's), a coin deposit and withdrawal
/// (copper 0, the amount in the detail: the coin stays the account's) and a
/// tier exchange (copper 0: the value is unchanged), all written with the
/// logout flush, in order, and the books balance with the loaves back in
/// the bags and the coin on both sides of the counter. The three kinds the
/// 2026-10-30 sitting did not play (`item_ledger_and_rtt_checklist.md` §2)
/// had no test until this one; it fails on the previous server at the
/// first row assertion (no table).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_ledger_records_the_bank_and_balances() {
    use protocol::world::Coins;
    let h = start_both().await;
    let (session, char_id, token) =
        provision_client(&h.auth_url, "ledgerbank", "Ledgerbank", "Human", "Warrior").await;
    give_char_item(&h.db_url, char_id, "base", 0, BREAD, 3).await;
    {
        let pool = projectdawn_server::db::open(&h.db_url)
            .await
            .expect("open pool");
        // At the spawn, in reach of the banker, with coin to bank.
        sqlx::query("UPDATE characters SET copper = 1000, pos_x = 0.0, pos_z = 0.0 WHERE id = ?1")
            .bind(char_id)
            .execute(&pool)
            .await
            .expect("seed");
    }
    seed_ledger_openings(&h.db_url).await;
    let mut a = WorldClient::start(token, &session, char_id).await;
    a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::InventorySnapshot { .. })
    })
    .await
    .expect("snapshot");

    a.send_bank_store_item("base", 0);
    a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::BankItemSnapshot { shared: false, entries }
            if entries.iter().any(|(_, p, c)| p == BREAD && *c == 3))
    })
    .await
    .expect("the loaves reach the vault");
    a.send_bank_withdraw_item(0);
    a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::InventoryDelta { item_path: Some(p), count: 3, .. } if p == BREAD)
    })
    .await
    .expect("the loaves come back to the bags");
    a.send_bank_deposit_coins(Coins {
        platinum: 0,
        gold: 0,
        silver: 0,
        copper: 150,
    });
    a.wait_for(
        CHANNEL_SYSTEM,
        Duration::from_secs(3),
        |m| matches!(m, ServerWorldMsg::BankSnapshot { coins } if coins.copper == 150),
    )
    .await
    .expect("the bank holds the deposit");
    a.send_bank_withdraw_coins(Coins {
        platinum: 0,
        gold: 0,
        silver: 0,
        copper: 50,
    });
    a.wait_for(
        CHANNEL_SYSTEM,
        Duration::from_secs(3),
        |m| matches!(m, ServerWorldMsg::BankSnapshot { coins } if coins.copper == 100),
    )
    .await
    .expect("the bank gives some back");
    // 100 of the 900 copper in hand into one silver: the wallet's value is
    // unchanged, so the exchange is a row with no copper on it.
    a.send_bank_exchange(0, 1, 100);
    a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::CoinsUpdate { coins } if coins.silver == 1 && coins.copper == 800)
    })
    .await
    .expect("the exchange lands");
    a.send_disconnect();
    a.pump_for(Duration::from_millis(300)).await;

    let rows = ledger_rows(&h.db_url, char_id).await;
    assert_eq!(
        rows,
        vec![
            ("bank_in".to_string(), Some(BREAD.to_string()), 3, 0, 0),
            ("bank_out".to_string(), Some(BREAD.to_string()), 3, 0, 0),
            ("bank_coin_in".to_string(), None, 0, 0, 0),
            ("bank_coin_out".to_string(), None, 0, 0, 0),
            ("bank_exchange".to_string(), None, 0, 0, 0),
        ],
        "the bank's rows, written with the logout flush"
    );
    {
        let pool = projectdawn_server::db::open(&h.db_url)
            .await
            .expect("open pool");
        let details: Vec<(Option<String>,)> = sqlx::query_as(
            "SELECT detail FROM item_events WHERE char_id = ?1 AND action LIKE 'bank%' ORDER BY id",
        )
        .bind(char_id)
        .fetch_all(&pool)
        .await
        .expect("details");
        let details: Vec<&str> = details
            .iter()
            .map(|(d,)| d.as_deref().unwrap_or(""))
            .collect();
        assert_eq!(
            details,
            vec![
                "vault",
                "vault",
                "150 copper",
                "50 copper",
                "100 of tier 0 to tier 1"
            ],
            "each row names the vault, the amount or the conversion"
        );
        let (wallet, bank): (i64, i64) = sqlx::query_as(
            "SELECT silver * 100 + copper, bank_copper FROM characters WHERE id = ?1",
        )
        .bind(char_id)
        .fetch_one(&pool)
        .await
        .expect("the stores");
        assert_eq!(
            (wallet, bank),
            (900, 100),
            "the coin sits on both sides of the counter"
        );
    }
    assert!(
        ledger_mismatches(&h.db_url).await.is_empty(),
        "the books balance after the bank"
    );
}

/// The death strip and the corpse loot in the ledger: the strip's rows (one
/// per stack, one for the coin) ride the corpse transaction, the loot's rows
/// the corpse-loot transaction, and the books balance at both ends, with what
/// lies on the corpse counted as out of the holdings in between. Fails on the
/// previous server at the first row assertion.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_death_and_the_corpse_loot_keep_the_books_balanced() {
    const CAP: &str = "res://data/loot/items/cloth_cap.tres";
    let h = start_both().await;
    let (session, char_id, _) =
        provision_client(&h.auth_url, "ledgerdie", "Ledgerdie", "Human", "Warrior").await;
    give_char_item(&h.db_url, char_id, "equip", 2, CAP, 1).await;
    give_char_item(&h.db_url, char_id, "base", 0, BREAD, 2).await;
    let pool = db::open(&h.db_url).await.expect("open pool");
    sqlx::query("UPDATE characters SET copper = 40 WHERE id = ?1")
        .bind(char_id)
        .execute(&pool)
        .await
        .expect("coin");
    db::set_account_gm(&pool, "ledgerdie", true)
        .await
        .expect("set is_gm");
    seed_ledger_openings(&h.db_url).await;
    let token = request_world_token(&h.auth_url, &session, char_id).await;
    let mut a = WorldClient::start(token, &session, char_id).await;
    a.pump_for(Duration::from_millis(300)).await;

    a.send_damage_self(1_000_000);
    let contents = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(5), |m| {
            matches!(m, ServerWorldMsg::CorpseContents { .. })
        })
        .await
        .expect("dying leaves a corpse");
    let ServerWorldMsg::CorpseContents { corpse_id, .. } = contents else {
        unreachable!()
    };
    let rows = ledger_rows(&h.db_url, char_id).await;
    assert_eq!(
        rows,
        vec![
            ("death".to_string(), Some(CAP.to_string()), 1, -1, 0),
            ("death".to_string(), Some(BREAD.to_string()), 2, -2, 0),
            ("death_coins".to_string(), None, 0, 0, -40),
        ],
        "the strip's rows, written with the corpse"
    );
    assert!(
        ledger_mismatches(&h.db_url).await.is_empty(),
        "balanced with the gear on the corpse"
    );

    a.send_respawn();
    a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(5), |m| {
        matches!(m, ServerWorldMsg::Teleport { .. })
    })
    .await
    .expect("respawned beside the corpse");
    a.send_loot_all(corpse_id);
    a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(5), |m| {
        matches!(m, ServerWorldMsg::InventoryDelta { location, item_path: Some(p), .. }
            if location == "equip" && p == CAP)
    })
    .await
    .expect("the cap goes back on");
    a.pump_for(Duration::from_millis(200)).await;
    let rows = ledger_rows(&h.db_url, char_id).await;
    assert_eq!(
        rows[3..].to_vec(),
        vec![
            ("corpse_coins".to_string(), None, 0, 0, 40),
            ("corpse_loot".to_string(), Some(CAP.to_string()), 1, 1, 0),
            ("corpse_loot".to_string(), Some(BREAD.to_string()), 2, 2, 0),
        ],
        "the loot's rows, written with the loot: {rows:?}"
    );
    assert!(
        ledger_mismatches(&h.db_url).await.is_empty(),
        "balanced with everything back"
    );
    a.send_disconnect();
    a.pump_for(Duration::from_millis(200)).await;
}
