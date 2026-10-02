//! Two-client end-to-end test for Track 3 multi-player replication.
//!
//! Brings up auth + world in-process, drives two independent renet
//! clients through the full handshake, and asserts the multi-player
//! invariants:
//!
//!   1. After client B app-connects, A receives an EntitySpawn for B
//!      and B receives an EntitySpawn for A. Neither receives a spawn
//!      for themselves (that's what ConnectOk is for).
//!   2. Position broadcasts fan out: each client's Move produces a
//!      Position carrying their id at the other client.
//!   3. When B disconnects cleanly, A receives EntityDespawn(B.char_id).
//!
//! This complements `world_smoke.rs` (single-client own-Position echo).

use bincode::config::standard as bincode_cfg;
use futures_util::{SinkExt, StreamExt};
use projectdawn_server::{auth, db, world, Config};
use protocol::world::{ClientWorldMsg, DamageType, ServerWorldMsg, SkillKind, SlotRef, Vec3, ENEMY_ID_BASE, PET_ID_BASE};
use renet::{ConnectionConfig, RenetClient};
use renet_netcode::{ClientAuthentication, ConnectToken, NetcodeClientTransport};
use std::{
    io::Cursor,
    net::{SocketAddr, UdpSocket},
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tempfile::TempDir;
use tokio_tungstenite::tungstenite::Message;

const CHANNEL_SYSTEM: u8 = 0;
const CHANNEL_POSITION: u8 = 1;
const TICK_DT: Duration = Duration::from_millis(50);

struct Harness {
    auth_url: String,
    db_url: String,
    _world_addr: SocketAddr,
    _tmp: TempDir,
}

async fn start_both() -> Harness {
    // Opt-in server tracing for triage: RUST_LOG=info cargo test ... --nocapture
    // makes the in-process server's tracing lines visible. try_init so the
    // second test in the process doesn't panic on double-init.
    if std::env::var("RUST_LOG").is_ok() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .try_init();
    }
    let tmp = TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("two_clients_test.db");
    let url = format!("sqlite://{}?mode=rwc", db_path.display());

    let mut netcode_key = [0u8; 32];
    use rand::RngCore;
    rand::rngs::OsRng.fill_bytes(&mut netcode_key);

    let world_socket = world::bind_world_socket("127.0.0.1:0").expect("bind world");
    let world_addr = world_socket.local_addr().expect("world local_addr");

    let cfg = Arc::new(Config {
        auth_bind: "127.0.0.1:0".into(),
        world_bind: "127.0.0.1:0".into(),
        world_endpoint: world_addr.to_string(),
        database_url: url.clone(),
        min_client_version: projectdawn_server::config::semver::Version {
            major: 0,
            minor: 1,
            patch: 0,
        },
        netcode_private_key: netcode_key,
    });

    let pool = db::open(&url).await.expect("open pool");
    db::migrate(&pool).await.expect("migrate");

    let _world_handle = world::serve_with_socket(cfg.clone(), pool.clone(), world_socket)
        .await
        .expect("world serve_with_socket");
    let (auth_addr, _auth_handle) = auth::serve_bound(cfg, pool).await.expect("auth bind");

    Harness {
        auth_url: format!("ws://{auth_addr}"),
        db_url: url,
        _world_addr: world_addr,
        _tmp: tmp,
    }
}

/// Register + login + char-create + request-token for one client. Returns
/// (session_token_hex, char_id, world_token_bytes).
async fn provision_client(
    auth_url: &str,
    username: &str,
    char_name: &str,
    race: &str,
    class: &str,
) -> (String, i64, Vec<u8>) {
    let (mut ws, _) = tokio_tungstenite::connect_async(auth_url)
        .await
        .expect("auth ws connect");

    let resp = rpc(&mut ws, serde_json::json!({
        "type": "Register",
        "username": username,
        "password": "hunter2!",
    })).await;
    assert_eq!(resp["type"], "RegisterOk", "register {username}: {resp}");

    let resp = rpc(&mut ws, serde_json::json!({
        "type": "Login",
        "username": username,
        "password": "hunter2!",
        "client_version": "0.1.0",
    })).await;
    assert_eq!(resp["type"], "LoginOk", "login {username}: {resp}");
    let session = resp["session_token"].as_str().unwrap().to_string();

    let resp = rpc(&mut ws, serde_json::json!({
        "type": "CharCreate",
        "session_token": session,
        "name": char_name,
        "race": race,
        "class": class,
    })).await;
    assert_eq!(resp["type"], "CharCreated", "charcreate {char_name}: {resp}");
    let char_id = resp["char_id"].as_i64().unwrap();

    let resp = rpc(&mut ws, serde_json::json!({
        "type": "RequestWorldToken",
        "session_token": session,
        "char_id": char_id,
    })).await;
    assert_eq!(resp["type"], "WorldConnectToken", "request token {char_name}: {resp}");
    let token_bytes: Vec<u8> = resp["token_bytes"]
        .as_array()
        .expect("token_bytes is array")
        .iter()
        .map(|v| v.as_u64().expect("byte") as u8)
        .collect();

    (session, char_id, token_bytes)
}

/// Raise a freshly-provisioned character's level (and top up its mana).
///
/// `provision_client` always creates a LEVEL 1 character (`db::create_character`
/// hardcodes level 1), but the CastSpell class/level gate added 2026-07-20
/// rejects a cast unless the caster meets the spell's `min_level`. Any test that
/// casts something above level 1 has to say so, or the server correctly refuses
/// and the awaited message never arrives.
///
/// Mana matters too: the character loader recomputes `max_mp` from the stored
/// level, but carries current `mp` over as `row.mp.min(computed.max_mp)`. A
/// bumped character would otherwise still hold its level-1 mana and fail on cost
/// instead of on the gate â€” swapping one confusing failure for another. Setting
/// mp high lets the loader clamp it to the new maximum, i.e. "full mana at the
/// new level".
async fn set_char_level(db_url: &str, char_id: i64, level: i32) {
    let pool = projectdawn_server::db::open(db_url).await.expect("open pool");
    sqlx::query("UPDATE characters SET level = ?1, mp = 99999.0 WHERE id = ?2")
        .bind(level)
        .bind(char_id)
        .execute(&pool)
        .await
        .expect("bump character level");
}

struct WorldClient {
    client: RenetClient,
    transport: NetcodeClientTransport,
    _char_id: i64,
}

impl WorldClient {
    /// Drive renet handshake + send app-layer Connect; spin until ConnectOk.
    async fn start(
        token_bytes: Vec<u8>,
        session_token_hex: &str,
        char_id: i64,
    ) -> Self {
        let connect_token = ConnectToken::read(&mut Cursor::new(&token_bytes[..]))
            .expect("ConnectToken::read");
        let socket = UdpSocket::bind("127.0.0.1:0").expect("udp bind");
        socket.set_nonblocking(true).expect("nonblocking");

        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
        let auth = ClientAuthentication::Secure { connect_token };
        let mut transport = NetcodeClientTransport::new(now, auth, socket).expect("transport");
        let mut client = RenetClient::new(connection_config_matching_server());

        // Spin renet handshake.
        let deadline = Instant::now() + Duration::from_secs(5);
        while !client.is_connected() {
            tick_one(&mut client, &mut transport);
            if Instant::now() > deadline {
                panic!("renet handshake did not complete within 5 s");
            }
            tokio::time::sleep(TICK_DT).await;
        }

        // App-layer Connect.
        let token = decode_token_bytes(session_token_hex);
        let msg = ClientWorldMsg::Connect {
            session_token: token,
            char_id: char_id as u64,
            client_version: "0.1.0".into(),
        };
        send_msg(&mut client, CHANNEL_SYSTEM, &msg);

        let mut this = Self { client, transport, _char_id: char_id };
        this.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::ConnectOk { .. })
        })
        .await
        .expect("ConnectOk arrived");
        // Track 4 follow-up E: EntitySpawn / Position fan-out is gated on
        // EnterWorld. Tests are post-lobby by design, so flip the gate
        // immediately after ConnectOk. Pump the transport for a few ticks
        // so the message actually goes out before start() returns â€”
        // otherwise the bytes sit in the outgoing buffer until the next
        // wait_for ticks the client, which may be after the test has
        // moved on to another client's setup.
        send_msg(&mut this.client, CHANNEL_SYSTEM, &ClientWorldMsg::EnterWorld);
        for _ in 0..4 {
            tick_one(&mut this.client, &mut this.transport);
            tokio::time::sleep(TICK_DT).await;
        }
        this
    }

    fn send_move(&mut self, sequence: u32, direction: Vec3) {
        let msg = ClientWorldMsg::Move { sequence, direction, jumping: false };
        send_msg(&mut self.client, CHANNEL_POSITION, &msg);
    }

    fn send_disconnect(&mut self) {
        send_msg(&mut self.client, CHANNEL_SYSTEM, &ClientWorldMsg::Disconnect);
    }

    fn send_cast_start(&mut self, spell_name: &str, duration: f32) {
        let msg = ClientWorldMsg::CastStartBroadcast {
            spell_name: spell_name.into(),
            duration,
        };
        send_msg(&mut self.client, CHANNEL_SYSTEM, &msg);
    }

    fn send_cast_complete(&mut self, spell_name: &str) {
        let msg = ClientWorldMsg::CastCompleteBroadcast {
            spell_name: spell_name.into(),
        };
        send_msg(&mut self.client, CHANNEL_SYSTEM, &msg);
    }

    fn send_buff_snapshot(&mut self, buffs: Vec<(String, f32)>) {
        let msg = ClientWorldMsg::BuffSnapshotBroadcast { buffs };
        send_msg(&mut self.client, CHANNEL_SYSTEM, &msg);
    }

    fn send_cast_spell(&mut self, spell_name: &str, target_id: Option<u64>) {
        let msg = ClientWorldMsg::CastSpell {
            spell_name: spell_name.into(),
            target_id,
        };
        send_msg(&mut self.client, CHANNEL_SYSTEM, &msg);
    }

    fn send_hit(&mut self, target: u64, amount: i32, crit: bool, dmg_type: DamageType) {
        let msg = ClientWorldMsg::HitBroadcast {
            target,
            amount,
            crit,
            dmg_type,
        };
        send_msg(&mut self.client, CHANNEL_SYSTEM, &msg);
    }

    fn send_death(&mut self) {
        send_msg(&mut self.client, CHANNEL_SYSTEM, &ClientWorldMsg::DeathBroadcast);
    }

    fn send_respawn(&mut self) {
        send_msg(&mut self.client, CHANNEL_SYSTEM, &ClientWorldMsg::Respawn);
    }

    fn send_loot_all(&mut self, bag_id: u64) {
        send_msg(&mut self.client, CHANNEL_SYSTEM, &ClientWorldMsg::LootAll { bag_id });
    }

    fn send_gm_command(&mut self, line: &str) {
        send_msg(
            &mut self.client,
            CHANNEL_SYSTEM,
            &ClientWorldMsg::GmCommand { line: line.to_string() },
        );
    }

    // ── PD_W0028 — trade window intents. ──
    fn send_trade_request(&mut self, target: u64) {
        send_msg(
            &mut self.client,
            CHANNEL_SYSTEM,
            &ClientWorldMsg::TradeRequest { target_id: target },
        );
    }

    fn send_trade_offer_item(&mut self, window_slot: u8, loc: &str, slot: u32) {
        send_msg(
            &mut self.client,
            CHANNEL_SYSTEM,
            &ClientWorldMsg::TradeOfferItem {
                window_slot,
                from_location: loc.to_string(),
                from_slot: slot,
            },
        );
    }

    fn send_trade_offer_coins(&mut self, coins: protocol::world::Coins) {
        send_msg(
            &mut self.client,
            CHANNEL_SYSTEM,
            &ClientWorldMsg::TradeOfferCoins { coins },
        );
    }

    fn send_trade_accept(&mut self) {
        send_msg(&mut self.client, CHANNEL_SYSTEM, &ClientWorldMsg::TradeAccept);
    }

    fn send_group_invite(&mut self, name: &str) {
        send_msg(
            &mut self.client,
            CHANNEL_SYSTEM,
            &ClientWorldMsg::GroupInvite { name: name.to_string() },
        );
    }

    fn send_group_accept(&mut self, from: u64) {
        send_msg(
            &mut self.client,
            CHANNEL_SYSTEM,
            &ClientWorldMsg::GroupAcceptInvite { from },
        );
    }

    fn send_heartbeat(&mut self) {
        send_msg(&mut self.client, CHANNEL_SYSTEM, &ClientWorldMsg::Heartbeat);
    }

    fn send_inspect_player(&mut self, target_char_id: i64) {
        send_msg(
            &mut self.client,
            CHANNEL_SYSTEM,
            &ClientWorldMsg::InspectPlayer { target_char_id },
        );
    }

    fn send_attack(&mut self, target_id: u64, weapon_path: &str, is_offhand: bool, dmg_type: DamageType) {
        let msg = ClientWorldMsg::Attack {
            target_id,
            weapon_path: weapon_path.into(),
            is_offhand,
            dmg_type,
        };
        send_msg(&mut self.client, CHANNEL_SYSTEM, &msg);
    }

    fn send_pvp_toggle(&mut self, on: bool) {
        send_msg(&mut self.client, CHANNEL_SYSTEM, &ClientWorldMsg::PvpToggle { on });
    }

    /// Dev/GM command probe: awards xp through the server's authoritative path.
    /// Gated on `can_use_dev_cmds()` (dev server or GM account), so it applies
    /// for a GM and is a silent no-op for a plain account.
    fn send_grant_quest_xp(&mut self, amount: i32) {
        send_msg(
            &mut self.client,
            CHANNEL_SYSTEM,
            &ClientWorldMsg::GrantQuestXp { amount },
        );
    }

    /// Dev-gated world-mob spawn (the Test Panel path). The server places the
    /// mob exactly 3 m behind the requester. Requires the connection to be
    /// dev or GM â€” grant `is_gm` via `db::set_account_gm` and mint a FRESH
    /// token, as `is_gm_gates_dev_commands` does.
    fn send_dev_spawn(&mut self, name: &str, level: u32, hp: f32, dmg: i32, speed: f32, aggro: f32) {
        send_msg(
            &mut self.client,
            CHANNEL_SYSTEM,
            &ClientWorldMsg::DevSpawnMob {
                name: name.to_string(),
                level,
                hp,
                dmg,
                speed,
                aggro,
            },
        );
    }

    fn send_pet_command(&mut self, command: u8, target_id: Option<u64>) {
        let msg = ClientWorldMsg::PetCommand { command, target_id };
        send_msg(&mut self.client, CHANNEL_SYSTEM, &msg);
    }

    fn send_move_item(
        &mut self,
        src_location: &str,
        src_slot: u32,
        dst_location: &str,
        dst_slot: u32,
    ) {
        let msg = ClientWorldMsg::MoveItem {
            src_location: src_location.into(),
            src_slot,
            dst_location: dst_location.into(),
            dst_slot,
        };
        send_msg(&mut self.client, CHANNEL_SYSTEM, &msg);
    }

    fn send_split_stack(
        &mut self,
        src_location: &str,
        src_slot: u32,
        dst_location: &str,
        dst_slot: u32,
        count: u32,
    ) {
        let msg = ClientWorldMsg::SplitStack {
            src_location: src_location.into(),
            src_slot,
            dst_location: dst_location.into(),
            dst_slot,
            count,
        };
        send_msg(&mut self.client, CHANNEL_SYSTEM, &msg);
    }

    fn send_drop_item(&mut self, location: &str, slot: u32, count: u32) {
        let msg = ClientWorldMsg::DropItem {
            location: location.into(),
            slot,
            count,
        };
        send_msg(&mut self.client, CHANNEL_SYSTEM, &msg);
    }

    fn send_equip_item(&mut self, src_location: &str, src_slot: u32, equip_slot: u8) {
        let msg = ClientWorldMsg::EquipItem {
            src_location: src_location.into(),
            src_slot,
            equip_slot,
        };
        send_msg(&mut self.client, CHANNEL_SYSTEM, &msg);
    }

    fn send_unequip_item(&mut self, equip_slot: u8, dst_location: &str, dst_slot: u32) {
        let msg = ClientWorldMsg::UnequipItem {
            equip_slot,
            dst_location: dst_location.into(),
            dst_slot,
        };
        send_msg(&mut self.client, CHANNEL_SYSTEM, &msg);
    }

    fn send_buy_item(&mut self, vendor_id: u64, item_name: &str, qty: u32) {
        let msg = ClientWorldMsg::BuyItem {
            vendor_id,
            item_name: item_name.into(),
            qty,
        };
        send_msg(&mut self.client, CHANNEL_SYSTEM, &msg);
    }

    fn send_sell_item(&mut self, slot: protocol::world::SlotRef, qty: u32) {
        let msg = ClientWorldMsg::SellItem { slot, qty };
        send_msg(&mut self.client, CHANNEL_SYSTEM, &msg);
    }

    /// Sleep for `d` while continuing to service the transport at TICK_DT
    /// cadence. Use instead of a bare tokio sleep for any wait longer than
    /// a tick or two (cast bars especially): with the phase 4 world
    /// population, an unserviced client socket overflows during a
    /// multi-second sleep and datagrams â€” including ones carrying RELIABLE
    /// channel slices â€” are lost faster than the 150 ms resend lands
    /// between ticks, so a message the server provably sent (e.g.
    /// PetSpawn) can miss a 3 s wait entirely. A real client services the
    /// socket every frame; the harness must too.
    async fn pump_for(&mut self, d: Duration) {
        let end = Instant::now() + d;
        while Instant::now() < end {
            tick_one(&mut self.client, &mut self.transport);
            tokio::time::sleep(TICK_DT).await;
        }
    }

    async fn wait_for(
        &mut self,
        channel: u8,
        timeout: Duration,
        pred: impl Fn(&ServerWorldMsg) -> bool,
    ) -> Option<ServerWorldMsg> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            tick_one(&mut self.client, &mut self.transport);
            // A transport-level disconnect (e.g. a reliable channel blowing
            // its max_memory_usage_bytes) otherwise presents as a silent
            // timeout here, which reads like a missing server message.
            // Surface it loudly instead.
            if self.client.is_disconnected() {
                eprintln!(
                    "wait_for: client DISCONNECTED mid-wait (reason: {:?})",
                    self.client.disconnect_reason()
                );
                return None;
            }
            while let Some(bytes) = self.client.receive_message(channel) {
                if let Ok((msg, _)) = bincode::serde::decode_from_slice::<ServerWorldMsg, _>(
                    &bytes,
                    bincode_cfg(),
                ) {
                    if pred(&msg) {
                        return Some(msg);
                    }
                }
            }
            tokio::time::sleep(TICK_DT).await;
        }
        None
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_clients_see_each_other() {
    let h = start_both().await;

    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "alpha", "Alphacha", "Human", "Warrior").await;
    let (b_session, b_char_id, b_token) =
        provision_client(&h.auth_url, "beta", "Betacha", "Elf", "Cleric").await;

    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    let mut b = WorldClient::start(b_token, &b_session, b_char_id).await;

    // â”€â”€ 1. EntitySpawn fan-out â”€â”€
    // B was newly-connected; the server should have sent A an EntitySpawn for B.
    // A also gets EntitySpawn for itself? No â€” fan-out skips the subject;
    // A's own ConnectOk is the own-self signal. So only spawn(B) at A.
    let spawn_b_at_a = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::EntitySpawn { id, .. } if *id == b_char_id as u64)
        })
        .await
        .expect("A receives EntitySpawn for B");
    if let ServerWorldMsg::EntitySpawn { name, race, class, level, .. } = &spawn_b_at_a {
        assert_eq!(name, "Betacha", "B's name in spawn");
        assert_eq!(race, "Elf", "B's race in spawn");
        assert_eq!(class, "Cleric", "B's class in spawn");
        assert_eq!(*level, 1, "B's level in spawn (newly-created character)");
    }

    let spawn_a_at_b = b
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::EntitySpawn { id, .. } if *id == a_char_id as u64)
        })
        .await
        .expect("B receives EntitySpawn for A");
    if let ServerWorldMsg::EntitySpawn { name, race, class, .. } = &spawn_a_at_b {
        assert_eq!(name, "Alphacha");
        assert_eq!(race, "Human");
        assert_eq!(class, "Warrior");
    }

    // â”€â”€ 2. Position fan-out â”€â”€
    // Each client moves; both should see Positions for both ids.
    a.send_move(1, Vec3 { x: 1.0, y: 0.0, z: 0.0 });
    b.send_move(1, Vec3 { x: 0.0, y: 0.0, z: 1.0 });

    let _pos_b_at_a = a
        .wait_for(CHANNEL_POSITION, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::Position { id, .. } if *id == b_char_id as u64)
        })
        .await
        .expect("A sees a Position broadcast for B");

    let _pos_a_at_b = b
        .wait_for(CHANNEL_POSITION, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::Position { id, .. } if *id == a_char_id as u64)
        })
        .await
        .expect("B sees a Position broadcast for A");

    // â”€â”€ 3. EntityDespawn on disconnect â”€â”€
    // B leaves. A should see EntityDespawn(B.char_id) within a tick or two.
    b.send_disconnect();
    // Pump B's transport so the Disconnect message reaches the wire before
    // the socket goes out of scope.
    for _ in 0..6 {
        b.client.update(TICK_DT);
        if b.transport.update(TICK_DT, &mut b.client).is_err() {
            break;
        }
        if b.transport.send_packets(&mut b.client).is_err() {
            break;
        }
        tokio::time::sleep(TICK_DT).await;
    }

    a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::EntityDespawn { id } if *id == b_char_id as u64)
    })
    .await
    .expect("A receives EntityDespawn for B");
}

/// Track 6 sub-task 1: server-authoritative resources. The server loads
/// HP/MP/Stamina from the DB at `CharacterSpawn` and fans
/// HealthUpdate/ManaUpdate/StaminaUpdate to in-world peers at the step-4a
/// EnterWorld seed (and continuously on regen-tick threshold crossings).
/// `create_character` seeds each new character with hp=mp=stamina=100; the
/// test asserts B sees those values for A via the seed path.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_clients_server_authoritative_resources() {
    let h = start_both().await;

    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "gamma", "Gam", "Human", "Warrior").await;
    let (b_session, b_char_id, b_token) =
        provision_client(&h.auth_url, "delta", "Del", "Elf", "Cleric").await;

    // A connects first; B joins second. The step-4a seed loop runs for
    // each new joiner: when B sends EnterWorld, the server replays every
    // existing peer's EntitySpawn + resources to B. So B should observe
    // A's HealthUpdate / ManaUpdate / StaminaUpdate at the seed step,
    // even though nothing else has happened in the world.
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    let mut b = WorldClient::start(b_token, &b_session, b_char_id).await;

    // Pump A a few times so the server's step-4a seed broadcast can land
    // (this includes the resources from the DB).
    for _ in 0..6 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }

    // Track 6 sub-task 2 â€” A is Human Warrior, so char_data::compute
    // gives max_hp=200 (BASE_HP 100 + Warrior 50 + CON-bonus 50),
    // max_mp=100 (BASE_MP 100 + 0), max_stamina=120 (BASE_ST 100 +
    // Warrior 20). `create_character` seeds current = max.
    let h_at_b = b
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::HealthUpdate { id, .. } if *id == a_char_id as u64)
        })
        .await
        .expect("B receives HealthUpdate for A via DB-seeded fan-out");
    if let ServerWorldMsg::HealthUpdate { hp, max_hp, .. } = h_at_b {
        assert!((hp - 200.0).abs() < 0.01, "Warrior hp: {hp}");
        assert!((max_hp - 200.0).abs() < 0.01, "Warrior max_hp: {max_hp}");
    }

    let m_at_b = b
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::ManaUpdate { id, .. } if *id == a_char_id as u64)
        })
        .await
        .expect("B receives ManaUpdate for A via DB-seeded fan-out");
    if let ServerWorldMsg::ManaUpdate { mp, max_mp, .. } = m_at_b {
        assert!((mp - 100.0).abs() < 0.01, "Warrior mp: {mp}");
        assert!((max_mp - 100.0).abs() < 0.01, "Warrior max_mp: {max_mp}");
    }

    let s_at_b = b
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::StaminaUpdate { id, .. } if *id == a_char_id as u64)
        })
        .await
        .expect("B receives StaminaUpdate for A via DB-seeded fan-out");
    if let ServerWorldMsg::StaminaUpdate { stamina, max, .. } = s_at_b {
        assert!((stamina - 120.0).abs() < 0.01, "Warrior stamina: {stamina}");
        assert!((max - 120.0).abs() < 0.01, "Warrior max stamina: {max}");
    }
}

/// Track 4 sub-task 2: cast lifecycle replication. A broadcasts CastStart
/// then CastComplete; B sees both ServerWorldMsg variants carrying A's id
/// and the spell name.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_clients_cast_fanout() {
    let h = start_both().await;

    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "epsilon", "Eps", "Human", "Wizard").await;
    let (b_session, b_char_id, b_token) =
        provision_client(&h.auth_url, "zeta", "Zet", "Elf", "Cleric").await;

    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    let mut b = WorldClient::start(b_token, &b_session, b_char_id).await;

    a.send_cast_start("Frostbolt", 2.5);
    for _ in 0..4 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }

    let cast_at_b = b
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::CastStart { caster, .. } if *caster == a_char_id as u64)
        })
        .await
        .expect("B receives CastStart for A");
    if let ServerWorldMsg::CastStart {
        spell_name,
        duration,
        ..
    } = cast_at_b
    {
        assert_eq!(spell_name, "Frostbolt");
        assert!((duration - 2.5).abs() < 0.01, "duration roundtrip: {duration}");
    }

    a.send_cast_complete("Frostbolt");
    for _ in 0..4 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }

    let complete_at_b = b
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::CastComplete { caster, .. } if *caster == a_char_id as u64)
        })
        .await
        .expect("B receives CastComplete for A");
    if let ServerWorldMsg::CastComplete { spell_name, .. } = complete_at_b {
        assert_eq!(spell_name, "Frostbolt");
    }

    // Owner should not receive own cast events back.
    let echoed = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_millis(500), |m| {
            matches!(m, ServerWorldMsg::CastStart { caster, .. } if *caster == a_char_id as u64)
                || matches!(m, ServerWorldMsg::CastComplete { caster, .. } if *caster == a_char_id as u64)
        })
        .await;
    assert!(echoed.is_none(), "A should not receive own cast events");
}

/// Track 4 sub-task 4: combat hit fan-out. A broadcasts a hit on B; B (the
/// target) and any other observer should receive ServerWorldMsg::Hit
/// carrying the attacker/target ids and the damage payload. Owner does not
/// receive own echo.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_clients_hit_fanout() {
    let h = start_both().await;

    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "iota", "Iot", "Human", "Warrior").await;
    let (b_session, b_char_id, b_token) =
        provision_client(&h.auth_url, "kappa", "Kap", "Elf", "Wizard").await;

    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    let mut b = WorldClient::start(b_token, &b_session, b_char_id).await;

    a.send_hit(b_char_id as u64, 42, true, DamageType::Fire);
    for _ in 0..4 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }

    let hit_at_b = b
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::Hit { attacker, target, .. }
                if *attacker == a_char_id as u64 && *target == b_char_id as u64)
        })
        .await
        .expect("B receives Hit from A");
    if let ServerWorldMsg::Hit {
        amount,
        crit,
        dmg_type,
        ..
    } = hit_at_b
    {
        assert_eq!(amount, 42);
        assert!(crit);
        assert_eq!(dmg_type, DamageType::Fire);
    }

    let echoed = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_millis(500), |m| {
            matches!(m, ServerWorldMsg::Hit { attacker, .. } if *attacker == a_char_id as u64)
        })
        .await;
    assert!(echoed.is_none(), "A should not receive own Hit echo");
}

/// Track 4 sub-task 5: death fan-out. A broadcasts DeathBroadcast; B
/// receives ServerWorldMsg::EntityDied { id = A.char_id }. Owner does not
/// receive own echo.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_clients_death_fanout() {
    let h = start_both().await;

    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "lambda", "Lam", "Human", "Cleric").await;
    let (b_session, b_char_id, b_token) =
        provision_client(&h.auth_url, "mumu", "Mumu", "Elf", "Druid").await;

    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    let mut b = WorldClient::start(b_token, &b_session, b_char_id).await;

    a.send_death();
    for _ in 0..4 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }

    let died_at_b = b
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::EntityDied { id } if *id == a_char_id as u64)
        })
        .await
        .expect("B receives EntityDied for A");
    assert!(matches!(died_at_b, ServerWorldMsg::EntityDied { .. }));

    let echoed = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_millis(500), |m| {
            matches!(m, ServerWorldMsg::EntityDied { id } if *id == a_char_id as u64)
        })
        .await;
    assert!(echoed.is_none(), "A should not receive own EntityDied echo");
    let _ = b_char_id; // silence unused if test order changes
}

/// Track 6 sub-task 4a: server-authoritative buff state. A casts
/// Healing Wave on self; server applies the HoT to A's active_buffs
/// and fans BuffSnapshot to in-world peers. B observes the buff
/// without A ever sending a BuffSnapshotBroadcast.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_clients_buff_snapshot_fanout() {
    let h = start_both().await;

    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "eta", "Eta", "Human", "Shaman").await;
    // Healing Wave requires level 4; provisioned characters start at 1.
    set_char_level(&h.db_url, a_char_id, 4).await;
    let (b_session, b_char_id, b_token) =
        provision_client(&h.auth_url, "theta", "The", "Elf", "Druid").await;

    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    let mut b = WorldClient::start(b_token, &b_session, b_char_id).await;

    // A casts Healing Wave (ALLY, 15 immediate heal + 4 hps Ã— 18s HoT,
    // cast_time 1.0s). Track 10 â€” the cast-time gate now rejects
    // CastSpell unless a matching CastStart ran long enough, so we
    // pump CastStart out first (transport doesn't advance during a
    // bare tokio sleep), then wait the cast time, then send CastSpell.
    // Server applies the HoT to A.active_buffs and fans BuffSnapshot
    // to all in-world peers including B.
    a.send_cast_start("Healing Wave", 1.0);
    for _ in 0..3 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }
    a.pump_for(Duration::from_millis(1100)).await;
    a.send_cast_spell("Healing Wave", None);
    for _ in 0..6 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }

    // Filter for the Healing Wave entry so we don't latch onto the
    // empty seed snapshot the server fans at step-4a join time.
    let _ = b
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::BuffSnapshot { target, buffs }
                if *target == a_char_id as u64
                    && buffs.iter().any(|(n, _)| n == "Healing Wave"))
        })
        .await
        .expect("B receives server-driven BuffSnapshot containing Healing Wave");
}

/// Track 5 sub-task 1C: AI state machine drives Idle â†’ Chase â†’ Attack
/// for a server-spawned enemy when a player enters aggro range.
///
/// The player walks toward the Bonepile's isolated [-16, 0, -14] spawn
/// long enough to land well inside the Decrepit Skeleton's 8 m aggro
/// radius even with the Â±3 m XZ spawn jitter, then stops. The test asserts:
///
///   * an `EntityTarget` broadcast lands targeting the player (Idle â†’
///     Chase transition fired server-side);
///   * a `Hit` broadcast lands targeting the player from an enemy id
///     (Attack state fired its melee swing).
///
/// Death lifecycle (EntityDied + EntityDespawn) is reserved for sub-task
/// 3, where the player-attacks-enemy path lands enemy HP authoritatively.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn enemy_aggros_chases_and_attacks_player() {
    let h = start_both().await;

    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "zeta", "Zett", "Human", "Warrior").await;

    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;

    // Unit vector toward camp 0's [20, 0, 5] spawn.
    // Phase 4 layout note: every camp-walking test aims at the Bonepile's
    // isolated [-16, 0, -14] spawn â€” the one ring 1 spawn whose aggro circle
    // overlaps no other, so exactly ONE slow (1.8 m/s, 2.5 s swing) level 1
    // Decrepit Skeleton pulls, the same single-puller semantics these tests
    // were written against. Do NOT aim at the Wolf Run: it is a four-wolf
    // pack, and standing in it turns every cast into interrupt rolls.
    let len: f32 = (16.0_f32 * 16.0 + 14.0_f32 * 14.0).sqrt();
    let dir = Vec3 { x: -16.0 / len, y: 0.0, z: -14.0 / len };

    // Walk for ~2 s at 50 ms cadence. Each Move refreshes the server-side
    // stale-move clock so integration continues until we stop sending.
    // 3.5 s, not 2 s: at 2 s the player only clips camp 0's aggro radius, so
    // whether an enemy locks on before the wait expires depended on where it
    // happened to be wandering. Walking fully in makes the pull deterministic.
    let walk_end = Instant::now() + Duration::from_millis(3_500);
    let mut seq: u32 = 1;
    while Instant::now() < walk_end {
        a.send_move(seq, dir);
        seq += 1;
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }
    // Stop. The server's STALE_MOVE_THRESHOLD (500 ms) will park the
    // player at its current pos within ~10 ticks.

    // Generous timeouts because `cargo test --release` runs the whole
    // world_two_clients.rs file's tests in parallel by default â€” under
    // CPU contention the AI tick + position fan-out can fall behind by
    // several seconds while still being functionally correct. Run this
    // test in isolation (`cargo test ... enemy_aggros...`) and it
    // completes in ~3-4 s.
    let target_evt = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(35), |m| {
            matches!(
                m,
                ServerWorldMsg::EntityTarget { target: Some(t), .. } if *t == a_char_id as u64
            )
        })
        .await
        .expect("an enemy locks onto the player");
    if let ServerWorldMsg::EntityTarget { id, .. } = target_evt {
        assert!(
            id >= ENEMY_ID_BASE,
            "EntityTarget origin must be an enemy id (got {id}, base {ENEMY_ID_BASE})"
        );
    }

    let hit_evt = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(35), |m| {
            matches!(m, ServerWorldMsg::Hit { target, .. } if *target == a_char_id as u64)
        })
        .await
        .expect("enemy fires a melee swing on the player");
    if let ServerWorldMsg::Hit { attacker, amount, dmg_type, .. } = hit_evt {
        assert!(
            attacker >= ENEMY_ID_BASE,
            "Hit attacker must be an enemy id (got {attacker}, base {ENEMY_ID_BASE})"
        );
        assert!(amount > 0, "enemy hit amount must be positive (got {amount})");
        assert!(
            matches!(dmg_type, DamageType::Physical),
            "enemy melee broadcasts as Physical damage (got {dmg_type:?})"
        );
    }
}

/// Track 5 sub-task 3: player â†’ server Attack intent, enemy HP authority,
/// death lifecycle.
///
/// The player walks into camp 0's aggro radius so the AI engages, waits
/// for an enemy-originated Hit broadcast (confirming the player and one
/// enemy are now within 1.2 Ã— melee_range of each other), then sends an
/// `Attack` with enough damage to one-shot the Decrepit Skeleton
/// (25 HP). Asserts:
///
///   * `HealthUpdate { id = enemy_id, hp = 0.0 }` arrives;
///   * `EntityDied { id = enemy_id }` arrives;
///   * `EntityDespawn { id = enemy_id }` arrives after the corpse linger
///     (ENEMY_DESPAWN_LINGER_SECS = 5 s server-side).
///
/// Respawn cadence (35 s default for camp 0) is covered by the
/// `death_notification_arms_respawn_timer` unit test in
/// `spawn_points.rs`; replicating it here would balloon the test
/// runtime past 35 s for marginal value.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn player_attack_kills_enemy_and_corpse_despawns() {
    let h = start_both().await;

    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "eta", "Etta", "Human", "Warrior").await;

    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;

    // Walk toward the Bonepile's isolated [-16, -14] spawn (one level 1
    // Decrepit Skeleton â€” see the shared phase 4 layout note above).
    // Phase 4 layout note: every camp-walking test aims at the Bonepile's
    // isolated [-16, 0, -14] spawn â€” the one ring 1 spawn whose aggro circle
    // overlaps no other, so exactly ONE slow (1.8 m/s, 2.5 s swing) level 1
    // Decrepit Skeleton pulls, the same single-puller semantics these tests
    // were written against. Do NOT aim at the Wolf Run: it is a four-wolf
    // pack, and standing in it turns every cast into interrupt rolls.
    let len: f32 = (16.0_f32 * 16.0 + 14.0_f32 * 14.0).sqrt();
    let dir = Vec3 { x: -16.0 / len, y: 0.0, z: -14.0 / len };
    // 3.5 s, not 2 s: at 2 s the player only clips camp 0's aggro radius, so
    // whether an enemy locks on before the wait expires depended on where it
    // happened to be wandering. Walking fully in makes the pull deterministic.
    let walk_end = Instant::now() + Duration::from_millis(3_500);
    let mut seq: u32 = 1;
    while Instant::now() < walk_end {
        a.send_move(seq, dir);
        seq += 1;
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }

    // Wait for an enemy hit on the player â€” proves the AI walked an
    // enemy into melee with us. The Hit carries the attacker id (in
    // the enemy partition). Generous timeout because parallel tests
    // in this file contend for CPU; isolated runtime is ~5 s.
    let hit_evt = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(35), |m| {
            matches!(m, ServerWorldMsg::Hit { target, .. } if *target == a_char_id as u64)
        })
        .await
        .expect("an enemy locks on and swings");
    let enemy_id: u64 = match hit_evt {
        ServerWorldMsg::Hit { attacker, .. } => attacker,
        _ => unreachable!(),
    };
    assert!(
        enemy_id >= ENEMY_ID_BASE,
        "attacker id must be an enemy id (got {enemy_id}, base {ENEMY_ID_BASE})"
    );

    // Track 6 sub-task 2: server runs the damage formula now â€” the
    // client can't claim 999 anymore. A bare-handed Human Warrior
    // lands ~5-8/swing (1-4 + STR/5 with STR 22). Decrepit Skeleton has
    // 25 HP, so ~5 landed swings cover the worst case.
    //
    // Swings must be PACED. This used to burst 10 Attacks in a single
    // frame, which the melee swing-rate limit (added 2026-07-29) now
    // correctly treats as forgery: it enforces a per-hand minimum
    // interval derived from the weapon's delay â€” 0.65 s bare-handed â€”
    // and silently drops anything faster. All but the first swing
    // vanished and the skeleton survived. Sleeping past the floor
    // between swings is what an honest client does anyway.
    // 1000 ms, not 700: the bare-hand swing-rate floor is 0.65 s, so a 700 ms
    // pace left only 50 ms of margin and timing jitter pushed swings under the
    // floor, where they are silently dropped and the skeleton survives. Widening
    // the margin makes it much more reliable, though this test still has residual
    // enemy-AI timing sensitivity (see docs/flaky_integration_tests.md) â€” it
    // depends on an enemy wandering into range, locking on, and STAYING in melee.
    const SWING_GAP: Duration = Duration::from_millis(1000);
    for _ in 0..8 {
        a.send_attack(enemy_id, "", false, DamageType::Physical);
        // Pump the transport across the gap so the Attack actually goes
        // out (a bare sleep does not advance renet).
        let until = Instant::now() + SWING_GAP;
        while Instant::now() < until {
            tick_one(&mut a.client, &mut a.transport);
            tokio::time::sleep(TICK_DT).await;
        }
    }

    let hu = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(5), |m| {
            matches!(m, ServerWorldMsg::HealthUpdate { id, hp, .. }
                if *id == enemy_id && *hp <= 0.0)
        })
        .await
        .expect("HealthUpdate(hp=0) arrives for the killed enemy");
    if let ServerWorldMsg::HealthUpdate { hp, .. } = hu {
        assert!(hp <= 0.0, "killed enemy must broadcast hp <= 0 (got {hp})");
    }

    a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(5), |m| {
        matches!(m, ServerWorldMsg::EntityDied { id } if *id == enemy_id)
    })
    .await
    .expect("EntityDied arrives for the killed enemy");

    // Track 5 sub-task 5 â€” kill credit. Per-kill XP is the EQ quadratic
    // (mob_level^2 * ZEM * 3.5, see progression::kill_xp): a level-1
    // Decrepit Skeleton pays round(1 * 75 * 3.5) = 263. Sole attacker â†’
    // sole top damager â†’ private XpGained.
    let xp_evt = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(5), |m| {
            matches!(m, ServerWorldMsg::XpGained { .. })
        })
        .await
        .expect("XpGained arrives for the kill-credit recipient");
    if let ServerWorldMsg::XpGained { amount, .. } = xp_evt {
        assert_eq!(amount, 263, "level-1 mob kill pays kill_xp(1) = 263");
    }

    // ENEMY_DESPAWN_LINGER_SECS is 5 s; budget extra for tick jitter and
    // parallel contention.
    a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(15), |m| {
        matches!(m, ServerWorldMsg::EntityDespawn { id } if *id == enemy_id)
    })
    .await
    .expect("EntityDespawn arrives after the corpse linger window");
}

/// Track 5 sub-task 1B: server-authoritative enemy spawn lifecycle.
///
/// The server boots, the spawner instantiates 27 enemies from the embedded
/// starter-zone TOML, and they sit idle (1C adds AI). When a client sends
/// EnterWorld, step 4a's seed loop fires an `EnemySpawn` for each of the
/// 27 live mobs. This test asserts: at least one EnemySpawn arrives, its
/// id falls inside the reserved enemy-id partition, and the carried mob
/// data matches a known starter-zone entry.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn enemies_visible_after_enter_world() {
    let h = start_both().await;

    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "epsilon", "Eps", "Human", "Warrior").await;

    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;

    let spawn = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(5), |m| {
            matches!(m, ServerWorldMsg::EnemySpawn { .. })
        })
        .await
        .expect("A receives at least one EnemySpawn after EnterWorld");

    if let ServerWorldMsg::EnemySpawn { id, mob_name, level, max_hp, hp, .. } = spawn {
        assert!(
            id >= ENEMY_ID_BASE,
            "enemy id must be in the reserved partition (got {id}, base {ENEMY_ID_BASE})"
        );
        assert!(!mob_name.is_empty(), "mob_name must be non-empty");
        assert!(level >= 1, "starter-zone mobs are level >= 1");
        assert!(max_hp > 0.0 && hp > 0.0, "fresh spawn has positive HP");
        assert!((hp - max_hp).abs() < 0.01, "fresh spawn is at full HP");
    }
}

/// Track 7 AOI â€” far-apart clients do not see each other.
///
/// CELL_SIZE = 120 m; cell boundaries at x = 120, 240, 360 â€¦
/// B is placed at x = 300 (cell (2, 0)) before connecting. A spawns at
/// the origin (cell (0, 0)). |2 âˆ’ 0| = 2 > 1, so they fall outside
/// each other's 3Ã—3 neighbourhood: neither should receive EntitySpawn
/// for the other. Previously (broadcast-to-all) both would appear.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn aoi_far_apart_clients_dont_see_each_other() {
    let h = start_both().await;

    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "aoifar1", "FarA", "Human", "Warrior").await;
    let (b_session, b_char_id, b_token) =
        provision_client(&h.auth_url, "aoifar2", "FarB", "Elf", "Cleric").await;

    // Place B at x = 300 (cell 2) before it connects. The world server
    // reads pos from the DB at ClientConnected time.
    let pool = db::open(&h.db_url).await.expect("open pool for pos update");
    db::set_character_position(&pool, b_char_id, 300.0, 0.0, 0.0)
        .await
        .expect("set B starting position");

    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    let mut b = WorldClient::start(b_token, &b_session, b_char_id).await;

    // 2 s is far beyond the 1-tick fanout window; if EntitySpawn were
    // going to arrive it would do so within ~100 ms.
    let spawn_b_at_a = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(2), |m| {
            matches!(m, ServerWorldMsg::EntitySpawn { id, .. } if *id == b_char_id as u64)
        })
        .await;
    assert!(
        spawn_b_at_a.is_none(),
        "A at cell (0,0) must NOT receive EntitySpawn for B at cell (2,0)"
    );

    let spawn_a_at_b = b
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(2), |m| {
            matches!(m, ServerWorldMsg::EntitySpawn { id, .. } if *id == a_char_id as u64)
        })
        .await;
    assert!(
        spawn_a_at_b.is_none(),
        "B at cell (2,0) must NOT receive EntitySpawn for A at cell (0,0)"
    );
}

/// Track 7 AOI â€” approaching client triggers mutual EntitySpawn on cell
/// boundary crossing.
///
/// A sits at the origin (cell (0, 0)). B starts at x = 300 (cell (2, 0))
/// and walks in the âˆ’X direction at MAX_MOVE_SPEED = 7.5 m/s. After
/// ~8 s B crosses x = 240 (cell (1, 0)), which is adjacent to A's cell.
/// The tick loop's step-5b fan-out must fire mutual EntitySpawn at that
/// moment. We budget 12 s of walking to absorb test-parallelism jitter.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn aoi_approaching_client_triggers_entity_spawn() {
    let h = start_both().await;

    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "aoiappr1", "ApprA", "Human", "Warrior").await;
    let (b_session, b_char_id, b_token) =
        provision_client(&h.auth_url, "aoiappr2", "ApprB", "Elf", "Ranger").await;

    let pool = db::open(&h.db_url).await.expect("open pool");
    db::set_character_position(&pool, b_char_id, 300.0, 0.0, 0.0)
        .await
        .expect("set B starting position");

    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    let mut b = WorldClient::start(b_token, &b_session, b_char_id).await;

    // Walk B toward A. 300 â†’ 240 = 60 m Ã· 7.5 m/s = 8 s; 12 s covers
    // the full crossing with margin. Both transports are pumped each tick:
    // without pumping A's transport for ~12 s the server's 15 s netcode
    // timeout would fire and disconnect A before we can assert.
    let walk_end = Instant::now() + Duration::from_secs(12);
    let mut seq: u32 = 1;
    let mut heartbeat_tick: u32 = 0;
    while Instant::now() < walk_end {
        b.send_move(seq, Vec3 { x: -1.0, y: 0.0, z: 0.0 });
        seq += 1;
        // A sends a Heartbeat every ~4 s (80 ticks) to satisfy the
        // server's 10 s app-layer idle timeout while B walks.
        heartbeat_tick += 1;
        if heartbeat_tick % 80 == 0 {
            a.send_heartbeat();
        }
        tick_one(&mut b.client, &mut b.transport);
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }

    // At this point B is at ~x = 210 (cell 1). EntitySpawn messages
    // arrived at both clients during the walk; wait_for drains the queue.
    a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(5), |m| {
        matches!(m, ServerWorldMsg::EntitySpawn { id, .. } if *id == b_char_id as u64)
    })
    .await
    .expect("A receives EntitySpawn for B once B enters A's 3Ã—3 neighbourhood");

    b.wait_for(CHANNEL_SYSTEM, Duration::from_secs(5), |m| {
        matches!(m, ServerWorldMsg::EntitySpawn { id, .. } if *id == a_char_id as u64)
    })
    .await
    .expect("B receives EntitySpawn for A when it crosses into A's neighbourhood");
}

/// Track 9 â€” server-side AOE damage. A Magician casts Inferno (5 m
/// radius, 45 base damage, FIRE) with an enemy standing 3 m away. The
/// server's AOE arm searches the caster's AOI neighbourhood, filters by
/// radius, and fans a Hit per victim. Asserts at least one enemy
/// receives a Fire Hit authored by the caster.
///
/// The victim is DEV-SPAWNED after the cast bar has already run, not
/// pulled from a world camp. The old walk-into-a-camp version was
/// flaky for two structural reasons: an enemy in melee during the
/// 2.5 s cast rolls a real interrupt (70% per hit at channeling 0),
/// and the "first hit" it keyed on could be a stale drive-by swing
/// from an enemy that had already leashed home, leaving nothing in
/// radius at resolution. A mob that appears 3 m away AFTER the bar
/// completes can do neither. Dev spawning needs a GM connection:
/// same provision, flip `is_gm`, re-mint token flow as
/// `is_gm_gates_dev_commands`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn aoe_spell_damages_nearby_enemies() {
    let h = start_both().await;

    let (a_session, a_char_id, _stale_token) =
        provision_client(&h.auth_url, "ino", "Inora", "Human", "Magician").await;
    // Inferno requires level 12; provisioned characters start at 1.
    set_char_level(&h.db_url, a_char_id, 12).await;
    let pool = db::open(&h.db_url).await.expect("open pool");
    db::set_account_gm(&pool, "ino", true).await.expect("set is_gm");
    let a_token = request_world_token(&h.auth_url, &a_session, a_char_id).await;

    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;

    // Let enter-world settle so the connection is fully in_world.
    for _ in 0..6 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }

    // Run the cast bar first. `target_id: None` because AOE doesn't take
    // a single target â€” the server searches the caster's AOI for anything
    // in radius at resolution time. Track 10's gate rejects CastSpell
    // without a matching CastStart that ran long enough, so pump
    // CastStart out first (sleeping doesn't advance the transport).
    a.send_cast_start("Inferno", 2.5);
    for _ in 0..3 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }
    a.pump_for(Duration::from_millis(2600)).await;

    // Bar has run; NOW conjure the victim 3 m away. Unique name so the
    // EntitySpawn predicate can't match a world camp mob fanned at
    // connect time.
    a.send_dev_spawn("Inferno Target Dummy", 1, 25.0, 3, 1.8, 8.0);
    let spawn_evt = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::EnemySpawn { mob_name, .. }
                if mob_name == "Inferno Target Dummy")
        })
        .await
        .expect("the dev-spawned victim fans an EnemySpawn");
    let enemy_id: u64 = match spawn_evt {
        ServerWorldMsg::EnemySpawn { id, .. } => id,
        _ => unreachable!(),
    };
    assert!(
        enemy_id >= ENEMY_ID_BASE,
        "victim id must be an enemy id (got {enemy_id}, base {ENEMY_ID_BASE})"
    );

    a.send_cast_spell("Inferno", None);
    for _ in 0..6 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }

    // Inferno must fan at least one Hit with attacker=a, target=enemy,
    // dmg_type=Fire. The amount is the authored base_damage (45) â€”
    // server doesn't apply INT scale yet, matching single-target.
    let aoe_hit = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(5), |m| {
            matches!(
                m,
                ServerWorldMsg::Hit { attacker, target, dmg_type, .. }
                    if *attacker == a_char_id as u64
                    && *target == enemy_id
                    && matches!(dmg_type, DamageType::Fire)
            )
        })
        .await
        .expect("Inferno fans a Fire Hit to the enemy standing 3 m away");
    if let ServerWorldMsg::Hit { amount, .. } = aoe_hit {
        assert_eq!(amount, 45, "Inferno authored base_damage is 45");
    }
}

/// Track 10 â€” cast-time gate rejects a CastSpell that arrives before
/// the cast bar had time to run. Send CastStart for Fireball (1.5 s
/// cast), then *immediately* send CastSpell. Assert: a CastFail
/// arrives with reason "cast not ready", and no Hit fires for the
/// next half-second.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cast_spell_rejected_before_cast_time() {
    let h = start_both().await;

    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "muu", "Mura", "Human", "Magician").await;

    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;

    // Skip walking â€” we don't need an enemy in range; the gate fails
    // long before target resolution. Sit at spawn.
    for _ in 0..4 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }

    // Forge a CastStart + immediate CastSpell with no real wait.
    a.send_cast_start("Fireball", 1.5);
    a.send_cast_spell("Fireball", Some(ENEMY_ID_BASE));
    for _ in 0..4 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }

    let fail = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(2), |m| {
            matches!(m, ServerWorldMsg::CastFail { caster, reason }
                if *caster == a_char_id as u64 && reason == "cast not ready")
        })
        .await
        .expect("server fans CastFail when CastSpell arrives before cast time elapsed");
    if let ServerWorldMsg::CastFail { reason, .. } = fail {
        assert_eq!(reason, "cast not ready");
    }

    // No Hit should fire â€” gate ran before mana deduction and spell
    // application. (Target id was a stub anyway; we want to confirm
    // no side effects, not target resolution.)
    let stray = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_millis(500), |m| {
            matches!(m, ServerWorldMsg::Hit { attacker, .. } if *attacker == a_char_id as u64)
        })
        .await;
    assert!(stray.is_none(), "rejected cast must not fan a Hit");
}

/// Track 10 â€” cast-time gate passes once the cast bar has actually
/// run. Send CastStart for Healing Wave (1.0 s cast, ALLY target_type
/// â€” self-heal when target_id is None), wait the cast time + jitter,
/// then send CastSpell. Assert: a HealthUpdate arrives for the caster
/// (proof the cast went through the helper that fans on heal apply).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cast_spell_accepted_after_cast_time() {
    let h = start_both().await;

    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "nuu", "Nura", "Human", "Shaman").await;
    // Healing Wave requires level 4; provisioned characters start at 1.
    set_char_level(&h.db_url, a_char_id, 4).await;

    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;

    // Pump a few ticks so the world-enter handshake completes before
    // the server's first regen tick can fire a HealthUpdate we'd
    // mistake for a heal apply later.
    for _ in 0..4 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }

    // Queue CastStart and pump immediately so the packet reaches the
    // server's `cast_set_at` clock before we start counting wait time.
    // tokio::time::sleep alone doesn't advance the transport â€” we'd
    // otherwise be measuring "time until the test got around to
    // ticking" not "time the cast bar ran on the server".
    a.send_cast_start("Healing Wave", 1.0);
    for _ in 0..3 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }
    // Now wait past the cast time (1.0 s) on the server's wall clock.
    // 1100 ms includes a 100 ms cushion above the gate's tolerance.
    a.pump_for(Duration::from_millis(1100)).await;
    a.send_cast_spell("Healing Wave", None);
    for _ in 0..6 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }

    // Healing Wave applies a 4 hps Ã— 18 s HoT; the server fans a
    // BuffSnapshot containing "Healing Wave" once the cast lands.
    // That's the cleanest "cast actually applied server-side" signal
    // and doesn't race against regen ticks like HealthUpdate would.
    let snap = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::BuffSnapshot { target, buffs }
                if *target == a_char_id as u64
                    && buffs.iter().any(|(n, _)| n == "Healing Wave"))
        })
        .await
        .expect("Healing Wave applies once cast bar has run");
    let _ = snap;

    // No CastFail should have fired.
    let fail = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_millis(300), |m| {
            matches!(m, ServerWorldMsg::CastFail { caster, .. } if *caster == a_char_id as u64)
        })
        .await;
    assert!(fail.is_none(), "well-timed cast must not produce a CastFail");
}

/// Track 11 â€” server-side pet summon. Necromancer A casts Summon
/// Skeleton; the server spawns a player-owned pet entity, fans
/// `PetSpawn` to AOI peers, and B (in the same cell) receives the
/// broadcast carrying A's char_id as owner and a pet id in the
/// reserved partition.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pet_summon_visible_to_peer() {
    let h = start_both().await;

    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "xio", "Xiora", "Human", "Necromancer").await;
    // Summon Skeleton requires level 6; provisioned characters start at 1.
    set_char_level(&h.db_url, a_char_id, 6).await;
    let (b_session, b_char_id, b_token) =
        provision_client(&h.auth_url, "yuu", "Yuusu", "Elf", "Cleric").await;

    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    let mut b = WorldClient::start(b_token, &b_session, b_char_id).await;

    // Let both clients settle into the world so EntitySpawn fan-outs
    // complete before we start kicking off the cast.
    for _ in 0..6 {
        tick_one(&mut a.client, &mut a.transport);
        tick_one(&mut b.client, &mut b.transport);
        tokio::time::sleep(TICK_DT).await;
    }

    // Summon Skeleton: cast_time 3.0s. Pump the start packet out
    // before sleeping (tokio::sleep doesn't advance the renet
    // transport â€” same gotcha as Track 10's gate tests).
    a.send_cast_start("Summon Skeleton", 3.0);
    for _ in 0..3 {
        tick_one(&mut a.client, &mut a.transport);
        tick_one(&mut b.client, &mut b.transport);
        tokio::time::sleep(TICK_DT).await;
    }
    a.pump_for(Duration::from_millis(3100)).await;
    a.send_cast_spell("Summon Skeleton", None);
    for _ in 0..6 {
        tick_one(&mut a.client, &mut a.transport);
        tick_one(&mut b.client, &mut b.transport);
        tokio::time::sleep(TICK_DT).await;
    }

    let pet_spawn = b
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(5), |m| {
            matches!(m, ServerWorldMsg::PetSpawn { owner, .. } if *owner == a_char_id as u64)
        })
        .await
        .expect("B receives PetSpawn fanned from A's Summon Skeleton cast");
    if let ServerWorldMsg::PetSpawn { id, owner, pet_name, level, max_hp, hp, .. } = pet_spawn {
        assert!(
            id >= PET_ID_BASE,
            "pet id must be in the pet partition (got {id}, base {PET_ID_BASE})"
        );
        assert_eq!(owner, a_char_id as u64);
        assert_eq!(pet_name, "Skeletal Warrior");
        // Owner-derived level (pet interim A): owner 6 -> base 5, minus
        // the 0..=2 manual-summon variance roll.
        assert!(
            (3..=5).contains(&level),
            "owner-6 skeleton rolls level 3-5 (got {level})"
        );
        // Stats ride the camp curve at PET_STAT_SCALAR (70%).
        let expected_hp = match level {
            3 => 39.9,
            4 => 49.0,
            5 => 63.0,
            _ => unreachable!(),
        };
        assert!(
            (max_hp - expected_hp).abs() < 0.1,
            "level-{level} pet hp follows the 70% camp curve (expected {expected_hp}, got {max_hp})"
        );
        assert!((hp - max_hp).abs() < 0.01, "fresh pet spawns at full hp");
    }
}

/// Track 11.2 â€” pet follows its owner across the world. Necromancer
/// summons the skeleton at spawn, then walks away. The peer sees
/// Position updates for the pet id closing the gap to the owner.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pet_follows_owner() {
    let h = start_both().await;

    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "zii", "Ziorel", "Human", "Necromancer").await;
    // Summon Skeleton requires level 6; provisioned characters start at 1.
    set_char_level(&h.db_url, a_char_id, 6).await;
    let (b_session, b_char_id, b_token) =
        provision_client(&h.auth_url, "qqq", "Qqua", "Elf", "Cleric").await;

    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    let mut b = WorldClient::start(b_token, &b_session, b_char_id).await;

    // Settle both clients in-world.
    for _ in 0..6 {
        tick_one(&mut a.client, &mut a.transport);
        tick_one(&mut b.client, &mut b.transport);
        tokio::time::sleep(TICK_DT).await;
    }

    // Cast Summon Skeleton (cast_time 3.0s) following the gate flow.
    a.send_cast_start("Summon Skeleton", 3.0);
    for _ in 0..3 {
        tick_one(&mut a.client, &mut a.transport);
        tick_one(&mut b.client, &mut b.transport);
        tokio::time::sleep(TICK_DT).await;
    }
    a.pump_for(Duration::from_millis(3100)).await;
    a.send_cast_spell("Summon Skeleton", None);
    for _ in 0..6 {
        tick_one(&mut a.client, &mut a.transport);
        tick_one(&mut b.client, &mut b.transport);
        tokio::time::sleep(TICK_DT).await;
    }

    let pet_id: u64 = match b
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(5), |m| {
            matches!(m, ServerWorldMsg::PetSpawn { owner, .. } if *owner == a_char_id as u64)
        })
        .await
        .expect("B sees A's pet spawn")
    {
        ServerWorldMsg::PetSpawn { id, .. } => id,
        _ => unreachable!(),
    };

    // Owner walks ~5 m east over 2 s. PET_FOLLOW_DISTANCE = 3.0 m,
    // skeleton speed = 3.0 m/s, so the pet has plenty of headroom to
    // close the gap. B should observe at least one Position update
    // for the pet id with x > 0 (it spawned at owner_pos + 1.5 east,
    // so x starts > 0; we want to see x KEEP increasing as the owner
    // walks).
    let dir = Vec3 { x: 1.0, y: 0.0, z: 0.0 };
    let walk_end = Instant::now() + Duration::from_millis(2_500);
    let mut seq: u32 = 1;
    let mut max_pet_x: f32 = f32::MIN;
    while Instant::now() < walk_end {
        a.send_move(seq, dir);
        seq += 1;
        tick_one(&mut a.client, &mut a.transport);
        tick_one(&mut b.client, &mut b.transport);
        // Drain CHANNEL_POSITION on B for pet position updates as we
        // walk; latch the highest x value we see.
        while let Some(bytes) = b.client.receive_message(CHANNEL_POSITION) {
            if let Ok((msg, _)) = bincode::serde::decode_from_slice::<ServerWorldMsg, _>(
                &bytes,
                bincode_cfg(),
            ) {
                if let ServerWorldMsg::Position { id, pos, .. } = msg {
                    if id == pet_id {
                        max_pet_x = max_pet_x.max(pos.x);
                    }
                }
            }
        }
        tokio::time::sleep(TICK_DT).await;
    }

    // Pet spawned at ~(owner_pos.x + 1.5, _, _). Owner walked ~5 m
    // east; the pet should have moved well past its spawn x to keep
    // up. Use 3.0 as the threshold â€” generous to absorb tick jitter
    // and the FOLLOW_DISTANCE hysteresis at the boundary.
    assert!(
        max_pet_x > 3.0,
        "pet should follow owner east (saw max x = {max_pet_x})"
    );
}

/// Track 11.3 â€” pet inherits owner's attack target and damages the
/// enemy. Walk Necromancer A into camp 0 until an enemy is in melee,
/// summon the skeleton, A swings on the enemy once to seed
/// last_attacked_enemy, then the skeleton's swings produce Hit
/// broadcasts with the pet id as attacker.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pet_attacks_owners_target() {
    let h = start_both().await;

    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "rrr", "Rune", "Human", "Necromancer").await;
    // Summon Skeleton requires level 6; provisioned characters start at 1.
    set_char_level(&h.db_url, a_char_id, 6).await;

    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;

    // Summon FIRST, in peace, before walking into aggro range â€” the same
    // reorder pet_command_attack_locks_onto_target got: casting the 3 s
    // summon while an enemy swings at you rolls a ~70% interrupt per hit
    // taken (channeling 0), which is why this test spent months on the
    // flaky list. Summoning before the pull does not weaken the test: the
    // point below is that the pet inherits the owner's target from ONE
    // seeding attack, which is unchanged.
    a.send_cast_start("Summon Skeleton", 3.0);
    for _ in 0..3 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }
    a.pump_for(Duration::from_millis(3100)).await;
    a.send_cast_spell("Summon Skeleton", None);
    for _ in 0..3 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }

    // Latch the pet id from the PetSpawn the server fans to A as
    // well (caster receives own PetSpawn â€” caller's AOI cell
    // includes themselves).
    let pet_id: u64 = match a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::PetSpawn { owner, .. } if *owner == a_char_id as u64)
                || matches!(m, ServerWorldMsg::CastFail { caster, .. } if *caster == a_char_id as u64)
        })
        .await
        .expect("A receives own PetSpawn (or a CastFail explaining why not)")
    {
        ServerWorldMsg::PetSpawn { id, .. } => id,
        ServerWorldMsg::CastFail { reason, .. } => {
            panic!("Summon Skeleton failed with CastFail: {reason:?}")
        }
        _ => unreachable!(),
    };

    // Pet in hand; NOW walk into the camp and take a hit to learn an
    // enemy id. Phase 4 layout note: aim at the Bonepile's isolated
    // [-16, 0, -14] spawn â€” the one ring 1 spawn whose aggro circle
    // overlaps no other, so exactly ONE slow level 1 Decrepit Skeleton
    // pulls, the single-puller semantics this test was written against.
    let len: f32 = (16.0_f32 * 16.0 + 14.0_f32 * 14.0).sqrt();
    let dir = Vec3 { x: -16.0 / len, y: 0.0, z: -14.0 / len };
    let walk_end = Instant::now() + Duration::from_millis(3_500);
    let mut seq: u32 = 1;
    while Instant::now() < walk_end {
        a.send_move(seq, dir);
        seq += 1;
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }

    // Wait for an enemy hit on us â€” proves the AI walked an enemy into
    // melee with us.
    let hit_evt = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(35), |m| {
            matches!(m, ServerWorldMsg::Hit { target, .. } if *target == a_char_id as u64)
        })
        .await
        .expect("an enemy locks on and swings");
    let enemy_id: u64 = match hit_evt {
        ServerWorldMsg::Hit { attacker, .. } => attacker,
        _ => unreachable!(),
    };

    // Send one Attack against the enemy to seed last_attacked_enemy.
    // Bare-handed swing; the server runs its own calc. We just need
    // the attack to land server-side so the pet inherits the target.
    a.send_attack(enemy_id, "", false, DamageType::Physical);
    for _ in 0..3 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }

    // The skeleton (speed 3 m/s) needs a moment to close to melee +
    // its 2.2 s attack interval. Pump for up to ~8 s; once a Hit
    // arrives with attacker=pet_id, target=enemy_id, the inheritance
    // pipeline is proven end-to-end.
    let pet_hit = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(10), |m| {
            matches!(m, ServerWorldMsg::Hit { attacker, target, .. }
                if *attacker == pet_id && *target == enemy_id)
        })
        .await
        .expect("skeleton inherits target and lands a Hit on the enemy");
    if let ServerWorldMsg::Hit { amount, .. } = pet_hit {
        // Owner-derived stats: owner 6 -> level 3-5 -> dmg 5-7 (70% of
        // the camp curve's 7/9/10, rounded).
        assert!(
            (5..=7).contains(&amount),
            "owner-6 skeleton swings for 5-7 (got {amount})"
        );
    }
}

/// Track 12 Piece A â€” explicit `/pet attack` command locks the pet
/// onto a specific enemy id, bypassing the `last_attacked_enemy`
/// inheritance pipeline. Necromancer summons, then commands the
/// pet to attack an enemy WITHOUT first hitting it themselves;
/// assert a Hit with attacker=pet, target=that enemy arrives.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pet_command_attack_locks_onto_target() {
    let h = start_both().await;

    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "sss", "Suun", "Human", "Necromancer").await;
    // Summon Skeleton requires level 6; provisioned characters start at 1.
    set_char_level(&h.db_url, a_char_id, 6).await;

    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;

    // Summon FIRST, in peace, before walking into aggro range.
    //
    // Ordering matters: this used to walk in, wait to be hit, and only then
    // start the 3 s Summon Skeleton cast â€” i.e. it cast while an enemy was
    // actively swinging at it. Track 19A's on-hit cast interrupt
    // (`roll_cast_interrupt`) then cleared the cast, so the pet never spawned.
    // That is not a fixable-by-tuning race: the interrupt chance is
    // `max(0.10, 0.70 - channeling_ratio * 0.60)`, which is ~0.70 for a level-6
    // caster and never drops below 0.10 even at max skill, so casting under fire
    // is inherently unreliable. Summoning before the pull avoids the interrupt
    // entirely and does not weaken the test: the point below is that the player
    // never attacks the enemy themselves, which is still true.
    a.send_cast_start("Summon Skeleton", 3.0);
    for _ in 0..3 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }
    a.pump_for(Duration::from_millis(3100)).await;
    a.send_cast_spell("Summon Skeleton", None);
    for _ in 0..3 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }
    let pet_id: u64 = match a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::PetSpawn { owner, .. } if *owner == a_char_id as u64)
                || matches!(m, ServerWorldMsg::CastFail { caster, .. } if *caster == a_char_id as u64)
        })
        .await
        .expect("A receives own PetSpawn (or a CastFail explaining why not)")
    {
        ServerWorldMsg::PetSpawn { id, .. } => id,
        ServerWorldMsg::CastFail { reason, .. } => {
            panic!("Summon Skeleton failed with CastFail: {reason:?}")
        }
        _ => unreachable!(),
    };

    // Now walk into camp 0; wait until at least one enemy aggros and
    // hits the player so we have an enemy id to command on.
    // Phase 4 layout note: every camp-walking test aims at the Bonepile's
    // isolated [-16, 0, -14] spawn â€” the one ring 1 spawn whose aggro circle
    // overlaps no other, so exactly ONE slow (1.8 m/s, 2.5 s swing) level 1
    // Decrepit Skeleton pulls, the same single-puller semantics these tests
    // were written against. Do NOT aim at the Wolf Run: it is a four-wolf
    // pack, and standing in it turns every cast into interrupt rolls.
    let len: f32 = (16.0_f32 * 16.0 + 14.0_f32 * 14.0).sqrt();
    let dir = Vec3 { x: -16.0 / len, y: 0.0, z: -14.0 / len };
    // 3.5 s, not 2 s: at 2 s the player only clips camp 0's aggro radius, so
    // whether an enemy locks on before the wait expires depended on where it
    // happened to be wandering. Walking fully in makes the pull deterministic.
    let walk_end = Instant::now() + Duration::from_millis(3_500);
    let mut seq: u32 = 1;
    while Instant::now() < walk_end {
        a.send_move(seq, dir);
        seq += 1;
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }
    let hit_evt = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(35), |m| {
            matches!(m, ServerWorldMsg::Hit { target, .. } if *target == a_char_id as u64)
        })
        .await
        .expect("an enemy aggros and hits the player");
    let enemy_id: u64 = match hit_evt {
        ServerWorldMsg::Hit { attacker, .. } => attacker,
        _ => unreachable!(),
    };

    // Issue ATTACK command. The player has NOT attacked the enemy
    // themselves (only the enemy has attacked them) so without the
    // explicit command, the pet would default to follow.
    a.send_pet_command(protocol::world::pet_command::ATTACK, Some(enemy_id));
    for _ in 0..3 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }

    let pet_hit = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(10), |m| {
            matches!(m, ServerWorldMsg::Hit { attacker, target, .. }
                if *attacker == pet_id && *target == enemy_id)
        })
        .await
        .expect("pet attacks the commanded target");
    if let ServerWorldMsg::Hit { amount, .. } = pet_hit {
        // Owner-derived stats: owner 6 -> level 3-5 -> dmg 5-7.
        assert!(
            (5..=7).contains(&amount),
            "owner-6 skeleton swings for 5-7 (got {amount})"
        );
    }
}

/// Track 12 Piece A2 â€” pet pulls aggro via threat re-eval. Walk
/// player into camp, get aggro'd, summon skeleton, command attack;
/// pet's accumulated threat eventually clears the 1.3Ã— current-
/// target multiplier and the enemy re-targets onto the pet,
/// broadcasting an EntityTarget switch.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pet_pulls_aggro_via_threat_reaggro() {
    let h = start_both().await;

    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "tnk", "Tanker", "Human", "Necromancer").await;
    // Summon Skeleton requires level 6; provisioned characters start at 1.
    set_char_level(&h.db_url, a_char_id, 6).await;

    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;

    // Summon FIRST, in peace, before walking into aggro range â€” the same
    // reorder its two sibling pet tests got: casting the 3 s summon while
    // an enemy swings at you rolls a ~70% interrupt per hit taken
    // (channeling 0). The test's substance (threat re-aggro between owner
    // and pet) starts after the pull and is unchanged.
    a.send_cast_start("Summon Skeleton", 3.0);
    for _ in 0..3 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }
    a.pump_for(Duration::from_millis(3100)).await;
    a.send_cast_spell("Summon Skeleton", None);
    for _ in 0..3 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }
    let pet_id: u64 = match a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::PetSpawn { owner, .. } if *owner == a_char_id as u64)
        })
        .await
        .expect("PetSpawn for own pet")
    {
        ServerWorldMsg::PetSpawn { id, .. } => id,
        _ => unreachable!(),
    };

    // Pet in hand; NOW walk into the camp. Phase 4 layout note: aim at
    // the Bonepile's isolated [-16, 0, -14] spawn â€” the one ring 1 spawn
    // whose aggro circle overlaps no other, so exactly ONE slow level 1
    // Decrepit Skeleton pulls, the single-puller semantics this test was
    // written against.
    let len: f32 = (16.0_f32 * 16.0 + 14.0_f32 * 14.0).sqrt();
    let dir = Vec3 { x: -16.0 / len, y: 0.0, z: -14.0 / len };
    let walk_end = Instant::now() + Duration::from_millis(3_500);
    let mut seq: u32 = 1;
    while Instant::now() < walk_end {
        a.send_move(seq, dir);
        seq += 1;
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }

    // Wait for enemy to lock on us and start swinging. We capture
    // the enemy id and confirm the player is the target.
    let initial_target_evt = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(35), |m| {
            matches!(m, ServerWorldMsg::EntityTarget { target: Some(t), .. }
                if *t == a_char_id as u64)
        })
        .await
        .expect("enemy targets the player initially");
    let enemy_id: u64 = match initial_target_evt {
        ServerWorldMsg::EntityTarget { id, .. } => id,
        _ => unreachable!(),
    };

    // Lock the pet onto the enemy via /pet attack.
    a.send_pet_command(protocol::world::pet_command::ATTACK, Some(enemy_id));
    for _ in 0..3 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }

    // Skeleton dmg is 8/swing on 2.2 s interval; bare-handed human
    // Necromancer is doing single-digit damage / 2-3 s. After ~3-4
    // pet swings (each adding +8 threat against pet_id) plus the
    // player's accumulated threat, the pet's threat passes 1.3Ã— the
    // player's and the enemy switches. Generous timeout because
    // the player keeps adding threat too via auto-attacks... wait,
    // the test doesn't send player attacks. Player threat only
    // accumulates if THEY swing; the test client doesn't. So pet
    // threat starts at 0, climbs by 8 per 2.2 s; player threat is
    // 0 (the test doesn't send Attack). Pet pulls on the first
    // swing because 8 >= 0 * 1.3 (but the > 0 guard catches that).
    //
    // To make this deterministic, send one player Attack so the
    // player has > 0 threat; pet's accumulated swings then have
    // to surpass it by 1.3Ã—.
    a.send_attack(enemy_id, "", false, DamageType::Physical);
    for _ in 0..3 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }

    let switch_evt = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(35), |m| {
            matches!(m, ServerWorldMsg::EntityTarget { id, target: Some(t), .. }
                if *id == enemy_id && *t == pet_id)
        })
        .await
        .expect("enemy re-targets onto the pet once threat passes 1.3Ã— the player's");
    let _ = switch_evt;
}

/// Track 12 Piece C â€” Enchanter's Charm converts a targeted enemy
/// into a player-owned pet. Server fan-outs: EntityDespawn for the
/// old enemy id, PetSpawn for a fresh pet id at the same pos with
/// owner == caster.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn charm_converts_enemy_to_pet() {
    let h = start_both().await;

    let (a_session, a_char_id, _stale_token) =
        provision_client(&h.auth_url, "ench", "Enchanted", "Human", "Enchanter").await;
    // Charm requires level 20; provisioned characters start at 1.
    set_char_level(&h.db_url, a_char_id, 20).await;
    // GM for the dev spawn below; re-mint the token so the flag rides it.
    let pool = db::open(&h.db_url).await.expect("open pool");
    db::set_account_gm(&pool, "ench", true).await.expect("set is_gm");
    let a_token = request_world_token(&h.auth_url, &a_session, a_char_id).await;

    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;

    // Charm a mob that is NOT attacking. This test used to walk into the
    // Bonepile, wait for the skeleton's hit, then cast the 2.0 s Charm while
    // that skeleton kept swinging every 2.5 s: the cast finished about 0.2 s
    // before the next swing, so any stretch of the test loop landed a hit
    // mid-cast and rolled the ~70% channeling-0 interrupt. That race was the
    // whole of its "load sensitivity". A dev-spawned mob with aggro 0 and
    // speed 0 never targets or swings, so an interrupt is impossible rather
    // than unlikely (the same fix the AOE test got on 2026-09-16).
    a.send_dev_spawn("Charm Dummy", 1, 30.0, 0, 0.0, 0.0);
    let spawn_evt = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::EnemySpawn { mob_name, .. } if mob_name == "Charm Dummy")
        })
        .await
        .expect("the dev-spawned dummy fans an EnemySpawn");
    let enemy_id: u64 = match spawn_evt {
        ServerWorldMsg::EnemySpawn { id, .. } => id,
        _ => unreachable!(),
    };

    // Cast Charm (cast_time 2.0s, mana 40) following the gate flow.
    a.send_cast_start("Charm", 2.0);
    for _ in 0..3 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }
    a.pump_for(Duration::from_millis(2100)).await;
    a.send_cast_spell("Charm", Some(enemy_id));
    for _ in 0..6 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }

    let despawn = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(5), |m| {
            matches!(m, ServerWorldMsg::EntityDespawn { id } if *id == enemy_id)
        })
        .await
        .expect("old enemy id is despawned on charm");
    let _ = despawn;

    let pet_spawn = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(5), |m| {
            matches!(m, ServerWorldMsg::PetSpawn { owner, .. } if *owner == a_char_id as u64)
        })
        .await
        .expect("a fresh pet id spawns for the caster");
    if let ServerWorldMsg::PetSpawn { id, pet_name, .. } = pet_spawn {
        assert!(id >= PET_ID_BASE, "charmed pet id must be in pet partition");
        // Mob name preserved: the charmed dummy keeps its name on the
        // pet entity.
        assert_eq!(pet_name, "Charm Dummy");
    }
}

/// Track 12 Piece B â€” Beast Masters auto-summon a Wolf warder when
/// they enter the world. No PET_SUMMON cast required; the server
/// detects the class on first EnterWorld and spawns the warder
/// alongside the EntitySpawn fan-out.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn beast_master_auto_summons_warder() {
    let h = start_both().await;

    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "bms", "Beastly", "Human", "Beast Master").await;

    // The exact scenario that opened the To-Do item: a level 22 Beast
    // Master whose warder was stuck at the static level 5.
    set_char_level(&h.db_url, a_char_id, 22).await;
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;

    let pet_spawn = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(5), |m| {
            matches!(m, ServerWorldMsg::PetSpawn { owner, .. } if *owner == a_char_id as u64)
        })
        .await
        .expect("Beast Master receives an auto-summoned warder on EnterWorld");
    if let ServerWorldMsg::PetSpawn { pet_name, level, max_hp, hp, .. } = pet_spawn {
        assert_eq!(pet_name, "Wolf", "Beast Master's auto-summon is a Wolf warder");
        // Deterministic owner - 1: the free auto-summon takes no variance
        // roll (pet interim A).
        assert_eq!(level, 21, "warder tracks its owner: 22 - 1");
        // 70% of the extrapolated camp curve at 21: (365 + 50*7) * 0.7.
        assert!(
            (max_hp - 500.5).abs() < 0.1,
            "level-21 warder hp rides the 70% curve (got {max_hp})"
        );
        assert!((hp - max_hp).abs() < 0.01, "auto-summon spawns at full HP");
    }
}

/// Track 12 Piece B â€” non-Beast-Master classes do NOT get an
/// auto-summoned warder. Counter-test to make sure the class check
/// is wired correctly.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_beast_master_gets_no_auto_warder() {
    let h = start_both().await;

    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "wrr", "Warlock", "Human", "Warrior").await;

    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;

    let stray = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(2), |m| {
            matches!(m, ServerWorldMsg::PetSpawn { owner, .. } if *owner == a_char_id as u64)
        })
        .await;
    assert!(stray.is_none(), "Warrior must not receive an auto-summoned pet");
}

/// Track 13.2 â€” on EnterWorld the server seeds the client with a
/// full inventory snapshot. New character has zero items, so the
/// snapshot's entries list is empty. Just asserts the message
/// arrives â€” proves the seed loop is wired.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inventory_snapshot_arrives_on_enter_world() {
    let h = start_both().await;
    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "inv", "Inv", "Human", "Warrior").await;
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;

    let snap = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::InventorySnapshot { .. })
        })
        .await
        .expect("InventorySnapshot fans privately on EnterWorld");
    if let ServerWorldMsg::InventorySnapshot { entries } = snap {
        assert!(entries.is_empty(), "fresh character has no items");
    }
}

/// Track 13.2.b â€” SplitStack carves part of a stack into another
/// slot. Seeds 10 of a known item into slot 0 via direct DB write
/// before EnterWorld, asserts the Snapshot reflects it, sends
/// SplitStack(slot 0 â†’ slot 5, count 3), asserts two
/// InventoryDeltas arrive (slot 0 with count 7, slot 5 with count
/// 3).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn split_stack_carves_off_count() {
    let h = start_both().await;
    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "spl", "Splita", "Human", "Warrior").await;

    // Seed inventory via the public DB helper before the client
    // connects to the world server. This bypasses needing a real
    // loot-drop flow to populate items in test time.
    let db_url = h.db_url.clone();
    let pool = projectdawn_server::db::open(&db_url).await.expect("open pool");
    projectdawn_server::db::save_inventory(
        &pool,
        a_char_id,
        &[projectdawn_server::db::InventoryRow {
            location: "base".into(),
            slot: 0,
            item_path: "res://items/cloth.tres".into(),
            count: 10,
        }],
    )
    .await
    .expect("seed inventory");

    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;

    // Confirm the seed lands on the wire.
    let snap = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::InventorySnapshot { .. })
        })
        .await
        .expect("snapshot");
    if let ServerWorldMsg::InventorySnapshot { entries } = snap {
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].2, "res://items/cloth.tres");
        assert_eq!(entries[0].3, 10);
    }

    a.send_split_stack("base", 0, "base", 5, 3);
    for _ in 0..4 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }

    // Two Deltas land: slot 0 with residual count 7, slot 5 with
    // the new stack of 3.
    let mut saw_src = false;
    let mut saw_dst = false;
    let deadline = Instant::now() + Duration::from_secs(2);
    while (!saw_src || !saw_dst) && Instant::now() < deadline {
        if let Some(msg) = a
            .wait_for(CHANNEL_SYSTEM, Duration::from_millis(200), |m| {
                matches!(m, ServerWorldMsg::InventoryDelta { .. })
            })
            .await
        {
            if let ServerWorldMsg::InventoryDelta { slot, item_path, count, .. } = msg {
                let path = item_path.unwrap_or_default();
                if slot == 0 && path == "res://items/cloth.tres" && count == 7 {
                    saw_src = true;
                }
                if slot == 5 && path == "res://items/cloth.tres" && count == 3 {
                    saw_dst = true;
                }
            }
        }
    }
    assert!(saw_src && saw_dst, "expected both src and dst Deltas (src={saw_src}, dst={saw_dst})");
}

/// Track 13.2.b â€” DropItem removes from inventory and spawns a
/// server-owned LootBag at the player's pos. Asserts:
///   - InventoryDelta clears the source slot.
///   - LootBagSpawn fans out with the dropped item.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn drop_item_creates_loot_bag_at_player_pos() {
    let h = start_both().await;
    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "drp", "Dropper", "Human", "Warrior").await;
    let db_url = h.db_url.clone();
    let pool = projectdawn_server::db::open(&db_url).await.expect("open pool");
    projectdawn_server::db::save_inventory(
        &pool,
        a_char_id,
        &[projectdawn_server::db::InventoryRow {
            location: "base".into(),
            slot: 0,
            item_path: "res://items/cloth.tres".into(),
            count: 5,
        }],
    )
    .await
    .expect("seed inventory");
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    let _ = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::InventorySnapshot { .. })
        })
        .await
        .expect("snapshot");

    // Drop the whole stack (count=0).
    a.send_drop_item("base", 0, 0);
    for _ in 0..4 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }

    let delta = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(2), |m| {
            matches!(m, ServerWorldMsg::InventoryDelta { slot, item_path, .. }
                if *slot == 0 && item_path.is_none())
        })
        .await
        .expect("inventory slot cleared");
    let _ = delta;

    let bag_spawn = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(2), |m| {
            matches!(m, ServerWorldMsg::LootBagSpawn { items, .. }
                if items.iter().any(|(p, c)| p == "res://items/cloth.tres" && *c == 5))
        })
        .await
        .expect("LootBag spawns with the dropped stack");
    let _ = bag_spawn;
}

/// Track 13.3 / 14.1 â€” EquipItem moves a base entry into the
/// paperdoll. Seed a registered weapon in base slot 0, send
/// EquipItem(0 â†’ equip slot 0), assert two Deltas land: base slot
/// 0 cleared, equip slot 0 holds the weapon. The path has to be
/// in items.toml or Track 14.1's `is_equippable_in_slot` check
/// would reject the equip.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn equip_item_moves_base_to_paperdoll() {
    const WEAPON: &str = "res://data/loot/items/iron_short_sword.tres";
    let h = start_both().await;
    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "eq1", "Equipper", "Human", "Warrior").await;
    let db_url = h.db_url.clone();
    let pool = projectdawn_server::db::open(&db_url).await.expect("open pool");
    projectdawn_server::db::save_inventory(
        &pool,
        a_char_id,
        &[projectdawn_server::db::InventoryRow {
            location: "base".into(),
            slot: 0,
            item_path: WEAPON.into(),
            count: 1,
        }],
    )
    .await
    .expect("seed");
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    let _ = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::InventorySnapshot { .. })
        })
        .await
        .expect("snapshot");

    a.send_equip_item("base", 0, 0); // weapon slot
    for _ in 0..4 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }

    let mut saw_base_clear = false;
    let mut saw_equip_set = false;
    let deadline = Instant::now() + Duration::from_secs(2);
    while (!saw_base_clear || !saw_equip_set) && Instant::now() < deadline {
        if let Some(msg) = a
            .wait_for(CHANNEL_SYSTEM, Duration::from_millis(200), |m| {
                matches!(m, ServerWorldMsg::InventoryDelta { .. })
            })
            .await
        {
            if let ServerWorldMsg::InventoryDelta { location, slot, item_path, .. } = msg {
                if location == "base" && slot == 0 && item_path.is_none() {
                    saw_base_clear = true;
                }
                if location == "equip"
                    && slot == 0
                    && item_path.as_deref() == Some(WEAPON)
                {
                    saw_equip_set = true;
                }
            }
        }
    }
    assert!(
        saw_base_clear && saw_equip_set,
        "expected both deltas (base_clear={saw_base_clear}, equip_set={saw_equip_set})"
    );
}

/// Track 14 follow-up â€” BuyItem charges coins and grants the item
/// via InventoryDelta + CoinsUpdate. Seed the player with 100
/// coins, buy 3 Minor Healing Potions (price 12 ea = 36 total),
/// assert the player ends with 64 coins + a 3-stack in base[0].
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn buy_item_charges_coins_and_grants_stack() {
    const POTION_NAME: &str = "Minor Healing Potion";
    const POTION_PATH: &str = "res://data/loot/items/minor_healing_potion.tres";
    let h = start_both().await;
    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "buy1", "Buyer", "Human", "Warrior").await;
    let pool = projectdawn_server::db::open(&h.db_url).await.expect("open pool");
    sqlx::query("UPDATE characters SET copper = 100 WHERE id = ?1")
        .bind(a_char_id)
        .execute(&pool)
        .await
        .expect("seed coins");
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    let _ = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::InventorySnapshot { .. })
        })
        .await
        .expect("initial snapshot");

    // vendor_id is informational for now (no server NPCs); 0 is fine.
    a.send_buy_item(0, POTION_NAME, 3);
    for _ in 0..4 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }

    // Inventory delta: base[0] = potion x3.
    let delta = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(2), |m| {
            matches!(
                m,
                ServerWorldMsg::InventoryDelta { location, slot, item_path, count }
                    if location == "base"
                    && *slot == 0
                    && item_path.as_deref() == Some(POTION_PATH)
                    && *count == 3
            )
        })
        .await
        .expect("InventoryDelta with bought stack");
    let _ = delta;

    // Coins update: 100 - (12 * 3) = 64.
    let coins = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(2), |m| {
            matches!(m, ServerWorldMsg::CoinsUpdate { coins } if coins.total_copper() == 64)
        })
        .await
        .expect("CoinsUpdate at 64");
    let _ = coins;
}

/// Dead-intents audit (2026-08-24): a merchant transaction requires a merchant
/// nearby. A client positioned far from any vendor â€” e.g. a modified client at
/// the bottom of a dungeon â€” must be refused, with no coin or item change. The
/// honest client is gated in UI at 6 m; this is the server backstop against a
/// forged position.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn buy_item_rejected_when_no_vendor_in_range() {
    const POTION_NAME: &str = "Minor Healing Potion";
    let h = start_both().await;
    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "farbuy", "Wanderer", "Human", "Warrior").await;
    let pool = projectdawn_server::db::open(&h.db_url).await.expect("open pool");
    // Plenty of coin, but stranded 200 m out â€” well beyond the 15 m service range.
    sqlx::query("UPDATE characters SET copper = 1000, pos_x = 200.0, pos_z = 200.0 WHERE id = ?1")
        .bind(a_char_id)
        .execute(&pool)
        .await
        .expect("seed coins + far position");
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    let _ = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::InventorySnapshot { .. })
        })
        .await
        .expect("snapshot");
    let _ = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(2), |m| {
            matches!(m, ServerWorldMsg::CoinsUpdate { .. })
        })
        .await;

    a.send_buy_item(0, POTION_NAME, 1);
    for _ in 0..6 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }

    // No coin change: the far buy is refused before charging.
    let coins = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_millis(500), |m| {
            matches!(m, ServerWorldMsg::CoinsUpdate { .. })
        })
        .await;
    assert!(coins.is_none(), "a buy with no vendor in range must not charge coin");
}

/// The same character, moved to the town spawn (origin, in range of Brom the
/// vendor), buys normally. Proves the gate is a proximity check, not a block.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn buy_item_succeeds_at_the_town_vendor() {
    const POTION_NAME: &str = "Minor Healing Potion";
    let h = start_both().await;
    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "nearbuy", "Townie", "Human", "Warrior").await;
    let pool = projectdawn_server::db::open(&h.db_url).await.expect("open pool");
    // At spawn (0,0,0), ~8 m from Brom â€” inside service range.
    sqlx::query("UPDATE characters SET copper = 1000, pos_x = 0.0, pos_z = 0.0 WHERE id = ?1")
        .bind(a_char_id)
        .execute(&pool)
        .await
        .expect("seed");
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    let _ = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::InventorySnapshot { .. })
        })
        .await
        .expect("snapshot");

    a.send_buy_item(0, POTION_NAME, 1);
    let delta = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(2), |m| {
            matches!(m, ServerWorldMsg::InventoryDelta { .. })
        })
        .await;
    assert!(delta.is_some(), "a buy at the town vendor must succeed");
}

/// Track 14 follow-up â€” BuyItem rejects when the player can't
/// afford the purchase; no Delta / CoinsUpdate lands.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn buy_item_rejects_insufficient_coins() {
    const POTION_NAME: &str = "Minor Healing Potion";
    let h = start_both().await;
    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "buy2", "Poorguy", "Human", "Warrior").await;
    let pool = projectdawn_server::db::open(&h.db_url).await.expect("open pool");
    sqlx::query("UPDATE characters SET coins = 5 WHERE id = ?1")
        .bind(a_char_id)
        .execute(&pool)
        .await
        .expect("seed coins");
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    let _ = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::InventorySnapshot { .. })
        })
        .await
        .expect("snapshot");
    // Track 15.1 â€” drain the initial CoinsUpdate seed fired on
    // EnterWorld so it doesn't contaminate the "rejected buy must
    // not fire CoinsUpdate" assertion below.
    let _ = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(2), |m| {
            matches!(m, ServerWorldMsg::CoinsUpdate { .. })
        })
        .await
        .expect("initial coins seed");

    a.send_buy_item(0, POTION_NAME, 1);
    for _ in 0..6 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }

    // No InventoryDelta and no CoinsUpdate should land for a rejected buy.
    let coins = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_millis(500), |m| {
            matches!(m, ServerWorldMsg::CoinsUpdate { .. })
        })
        .await;
    assert!(coins.is_none(), "rejected buy must not fire CoinsUpdate");
}

/// Track 14 follow-up â€” SellItem credits coins and fans an
/// InventoryDelta that clears the slot. Seed a potion stack via DB,
/// sell the whole stack, assert delta-cleared + coins = stack *
/// (vendor_price / 2).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sell_item_credits_coins_and_removes_stack() {
    const POTION_PATH: &str = "res://data/loot/items/minor_healing_potion.tres";
    let h = start_both().await;
    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "sell1", "Seller", "Human", "Warrior").await;
    let pool = projectdawn_server::db::open(&h.db_url).await.expect("open pool");
    // Seed inventory: 4 potions in base[0].
    projectdawn_server::db::save_inventory(
        &pool,
        a_char_id,
        &[projectdawn_server::db::InventoryRow {
            location: "base".into(),
            slot: 0,
            item_path: POTION_PATH.into(),
            count: 4,
        }],
    )
    .await
    .expect("seed inventory");
    sqlx::query("UPDATE characters SET copper = 0 WHERE id = ?1")
        .bind(a_char_id)
        .execute(&pool)
        .await
        .expect("seed coins=0");
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    let _ = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::InventorySnapshot { .. })
        })
        .await
        .expect("snapshot");

    // Sell all 4. Vendor price = 12; sell price = 6; total = 24.
    a.send_sell_item(SlotRef::BaseSlot { idx: 0 }, 4);
    for _ in 0..4 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }

    let delta = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(2), |m| {
            matches!(
                m,
                ServerWorldMsg::InventoryDelta { location, slot, item_path, count }
                    if location == "base" && *slot == 0
                    && item_path.is_none() && *count == 0
            )
        })
        .await
        .expect("InventoryDelta clears the slot");
    let _ = delta;

    let coins = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(2), |m| {
            matches!(m, ServerWorldMsg::CoinsUpdate { coins } if coins.total_copper() == 24)
        })
        .await
        .expect("CoinsUpdate at 24");
    let _ = coins;
}

/// Track 14 follow-up â€” lifesteal on an ENEMY-target spell heals
/// the caster. Casts Lifetap Rk. II at an enemy in melee range
/// and asserts a HealthUpdate for the caster arrives with hp
/// strictly above the pre-cast baseline by more than natural
/// regen could explain.
///
/// Setup wrinkle: Lifetap Rk. II requires level 10, so we SQL
/// the freshly-provisioned character up to level 10 + drop their
/// hp to a low starting value so the heal contribution is
/// unambiguous against regen ticks.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lifesteal_spell_heals_caster() {
    let h = start_both().await;
    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "lsk", "Lifedrinker", "Human", "Shadow Knight").await;
    let pool = projectdawn_server::db::open(&h.db_url).await.expect("open pool");
    sqlx::query("UPDATE characters SET level = 10, hp = 5.0 WHERE id = ?1")
        .bind(a_char_id)
        .execute(&pool)
        .await
        .expect("bump level + low hp");

    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;

    // Walk toward camp 0's enemy spawn â€” same pattern as the AOE
    // and aggro tests.
    // Phase 4 layout note: every camp-walking test aims at the Bonepile's
    // isolated [-16, 0, -14] spawn â€” the one ring 1 spawn whose aggro circle
    // overlaps no other, so exactly ONE slow (1.8 m/s, 2.5 s swing) level 1
    // Decrepit Skeleton pulls, the same single-puller semantics these tests
    // were written against. Do NOT aim at the Wolf Run: it is a four-wolf
    // pack, and standing in it turns every cast into interrupt rolls.
    let len: f32 = (16.0_f32 * 16.0 + 14.0_f32 * 14.0).sqrt();
    let dir = Vec3 { x: -16.0 / len, y: 0.0, z: -14.0 / len };
    // 3.5 s, not 2 s: at 2 s the player only clips camp 0's aggro radius, so
    // whether an enemy locks on before the wait expires depended on where it
    // happened to be wandering. Walking fully in makes the pull deterministic.
    let walk_end = Instant::now() + Duration::from_millis(3_500);
    let mut seq: u32 = 1;
    while Instant::now() < walk_end {
        a.send_move(seq, dir);
        seq += 1;
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }

    // Wait for an enemy hit to confirm an enemy is in range. The
    // attacker id is the spell target.
    let hit_evt = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(35), |m| {
            matches!(m, ServerWorldMsg::Hit { target, .. } if *target == a_char_id as u64)
        })
        .await
        .expect("enemy hits player");
    let enemy_id: u64 = match hit_evt {
        ServerWorldMsg::Hit { attacker, .. } => attacker,
        _ => unreachable!(),
    };
    assert!(enemy_id >= ENEMY_ID_BASE, "attacker is an enemy id");

    // Capture the latest caster HP from a self-HealthUpdate. The
    // walk-and-take-hits phase has fanned several; the most recent
    // is the baseline we need to compare against post-cast.
    let mut baseline_hp: f32 = 0.0;
    while let Some(msg) = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_millis(200), |m| {
            matches!(m, ServerWorldMsg::HealthUpdate { id, .. } if *id == a_char_id as u64)
        })
        .await
    {
        if let ServerWorldMsg::HealthUpdate { hp, .. } = msg {
            baseline_hp = hp;
        }
    }

    // Cast Lifetap Rk. II (cast_time 0.5s, base_damage 50,
    // heal_amount 35).
    a.send_cast_start("Lifetap Rk. II", 0.5);
    for _ in 0..3 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }
    tokio::time::sleep(Duration::from_millis(600)).await;
    a.send_cast_spell("Lifetap Rk. II", Some(enemy_id));
    for _ in 0..6 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }

    // The lifesteal should bump caster HP well above baseline +
    // regen-this-tick. Lifetap Rk. II heals up to 35; we require
    // at least +20 to stay comfortably above any regen drift the
    // tick loop ran in the meantime.
    let post = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(
                m,
                ServerWorldMsg::HealthUpdate { id, hp, .. }
                    if *id == a_char_id as u64 && *hp >= baseline_hp + 20.0
            )
        })
        .await
        .expect("caster HealthUpdate with lifesteal heal");
    if let ServerWorldMsg::HealthUpdate { hp, .. } = post {
        assert!(
            hp >= baseline_hp + 20.0,
            "expected hp >= {} (baseline {} + â‰¥20 lifesteal); got {}",
            baseline_hp + 20.0,
            baseline_hp,
            hp,
        );
    }
}

/// Track 14.3 â€” a bag plus its contents persist across a
/// reconnect. Seed `base[0] = Small Pouch` + `bag_0[2] = potions`
/// via DB, EnterWorld, assert the InventorySnapshot includes both
/// rows so the client can reconstruct the bag interior.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bag_contents_persist_across_reconnect() {
    const POUCH: &str = "res://data/loot/items/small_pouch.tres";
    const POTION: &str = "res://data/loot/items/minor_healing_potion.tres";
    let h = start_both().await;
    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "bag1", "Bagholder", "Human", "Warrior").await;
    let db_url = h.db_url.clone();
    let pool = projectdawn_server::db::open(&db_url).await.expect("open pool");
    projectdawn_server::db::save_inventory(
        &pool,
        a_char_id,
        &[
            projectdawn_server::db::InventoryRow {
                location: "base".into(),
                slot: 0,
                item_path: POUCH.into(),
                count: 1,
            },
            projectdawn_server::db::InventoryRow {
                location: "bag_0".into(),
                slot: 2,
                item_path: POTION.into(),
                count: 5,
            },
        ],
    )
    .await
    .expect("seed");
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;

    let snap = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::InventorySnapshot { .. })
        })
        .await
        .expect("snapshot");
    if let ServerWorldMsg::InventorySnapshot { entries } = snap {
        let has_bag = entries
            .iter()
            .any(|(loc, slot, path, _)| loc == "base" && *slot == 0 && path == POUCH);
        let has_potion = entries.iter().any(|(loc, slot, path, count)| {
            loc == "bag_0" && *slot == 2 && path == POTION && *count == 5
        });
        assert!(has_bag, "snapshot must include the parent pouch row");
        assert!(has_potion, "snapshot must include the bag_0 inner stack");
    }
}

/// Track 14.2 â€” equipping an item with +max_hp bonus fans a
/// HealthUpdate carrying the new max. Iron Chain Vest in
/// items.toml has `max_hp_bonus = 25.0`; a Human Warrior at
/// level 1 has base max_hp = 200 (see char_data tests), so the
/// post-equip max should land at 225. Asserts the post-equip
/// fan-out hits the equipping player themselves â€” the initial
/// EnterWorld snapshot only fans to peers, so without a peer
/// the player wouldn't see their own max_hp until something
/// changed it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn equip_increases_max_hp() {
    const VEST: &str = "res://data/loot/items/iron_chain_vest.tres";
    const HUMAN_WARRIOR_BASE_MAX_HP: f32 = 200.0;
    const EXPECTED_MAX_HP: f32 = HUMAN_WARRIOR_BASE_MAX_HP + 25.0;
    let h = start_both().await;
    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "eq4", "Vestguy", "Human", "Warrior").await;
    let db_url = h.db_url.clone();
    let pool = projectdawn_server::db::open(&db_url).await.expect("open pool");
    projectdawn_server::db::save_inventory(
        &pool,
        a_char_id,
        &[projectdawn_server::db::InventoryRow {
            location: "base".into(),
            slot: 0,
            item_path: VEST.into(),
            count: 1,
        }],
    )
    .await
    .expect("seed");
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    let _ = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::InventorySnapshot { .. })
        })
        .await
        .expect("snapshot");

    a.send_equip_item("base", 0, 3); // chest slot
    for _ in 0..4 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }

    let post = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(2), |m| {
            matches!(
                m,
                ServerWorldMsg::HealthUpdate { id, max_hp, .. }
                    if *id == a_char_id as u64 && (*max_hp - EXPECTED_MAX_HP).abs() < 0.1
            )
        })
        .await
        .expect("HealthUpdate with vest-augmented max_hp");
    if let ServerWorldMsg::HealthUpdate { max_hp, .. } = post {
        assert!(
            (max_hp - EXPECTED_MAX_HP).abs() < 0.1,
            "expected max_hp = {EXPECTED_MAX_HP} (base 200 + 25 from vest), got {max_hp}"
        );
    }
}

/// Track 13.3 â€” EquipItem with no source rejects silently. The wire
/// round-trips, server returns no Delta. Mirrors the negative-path
/// MoveItem test.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn equip_item_empty_source_drops_silently() {
    let h = start_both().await;
    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "eq2", "Empty", "Human", "Warrior").await;
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    let _ = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::InventorySnapshot { .. })
        })
        .await
        .expect("snapshot");

    a.send_equip_item("base", 0, 0);
    for _ in 0..4 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }

    let stray = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_millis(500), |m| {
            matches!(m, ServerWorldMsg::InventoryDelta { .. })
        })
        .await;
    assert!(stray.is_none(), "EquipItem on empty source must not fan a Delta");
}

/// Track 13.3 â€” InventorySnapshot on EnterWorld includes persisted
/// equip rows. Seed an equipped sword + a base cloth via DB, assert
/// the snapshot's entries list contains both with the right
/// location strings.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn snapshot_includes_persisted_equipment() {
    let h = start_both().await;
    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "eq3", "Equipsnap", "Human", "Warrior").await;
    let db_url = h.db_url.clone();
    let pool = projectdawn_server::db::open(&db_url).await.expect("open pool");
    projectdawn_server::db::save_inventory(
        &pool,
        a_char_id,
        &[
            projectdawn_server::db::InventoryRow {
                location: "base".into(),
                slot: 0,
                item_path: "res://items/cloth.tres".into(),
                count: 5,
            },
            projectdawn_server::db::InventoryRow {
                location: "equip".into(),
                slot: 0,
                item_path: "res://items/sword.tres".into(),
                count: 1,
            },
        ],
    )
    .await
    .expect("seed");
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;

    let snap = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::InventorySnapshot { .. })
        })
        .await
        .expect("snapshot");
    if let ServerWorldMsg::InventorySnapshot { entries } = snap {
        let has_base = entries
            .iter()
            .any(|(loc, _, path, count)| loc == "base" && path == "res://items/cloth.tres" && *count == 5);
        let has_equip = entries
            .iter()
            .any(|(loc, slot, path, count)| {
                loc == "equip" && *slot == 0 && path == "res://items/sword.tres" && *count == 1
            });
        assert!(has_base, "snapshot must include the base cloth row");
        assert!(has_equip, "snapshot must include the equipped sword row");
    }
}

/// A rejected `MoveItem` must TELL the client the truth about the slots it
/// named, rather than being dropped silently.
///
/// This test previously asserted the opposite ("must not fan an
/// InventoryDelta"), which enshrined a real bug: authority over inventory only
/// helps if disagreement is reported. A client that asked to move an item the
/// server does not have there learned nothing from being refused, kept
/// rendering the phantom, and kept asking. A playtest on 2026-08-18 caught the
/// same slots refused for half an hour, resolving only on relog, and presenting
/// to the player as items vanishing.
///
/// Both slots here are genuinely empty, so the corrective deltas should say so.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn move_item_empty_source_corrects_client() {
    let h = start_both().await;
    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "mvi", "Mover", "Human", "Warrior").await;
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;

    // Wait for the EnterWorld snapshot so we know we're past the seed loop.
    let _ = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::InventorySnapshot { .. })
        })
        .await
        .expect("snapshot seed");

    // Empty slot 0 -> empty slot 3. The server rejects it, and must answer with
    // the real contents of both named slots.
    a.send_move_item("base", 0, "base", 3);
    for _ in 0..4 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }

    let mut corrected: Vec<(String, u32, bool)> = Vec::new();
    for _ in 0..2 {
        let msg = a
            .wait_for(CHANNEL_SYSTEM, Duration::from_millis(700), |m| {
                matches!(m, ServerWorldMsg::InventoryDelta { .. })
            })
            .await;
        match msg {
            Some(ServerWorldMsg::InventoryDelta {
                location,
                slot,
                item_path,
                count,
            }) => corrected.push((location, slot, item_path.is_none() && count == 0)),
            _ => break,
        }
    }

    assert_eq!(
        corrected.len(),
        2,
        "a rejected MoveItem must correct both named slots; got {corrected:?}"
    );
    assert!(
        corrected.iter().any(|(l, s, _)| l == "base" && *s == 0),
        "source slot must be corrected; got {corrected:?}"
    );
    assert!(
        corrected.iter().any(|(l, s, _)| l == "base" && *s == 3),
        "destination slot must be corrected; got {corrected:?}"
    );
    assert!(
        corrected.iter().all(|(_, _, empty)| *empty),
        "both slots are genuinely empty, so both corrections must say empty; got {corrected:?}"
    );
}

/// Track 17.2 â€” per-spell cooldown gate. Cast Healing Wave (6 s
/// cooldown) once successfully, then immediately cast it again
/// following the full cast-time flow. The second cast should land
/// well after the cast-time gate but be rejected by the cooldown
/// gate with a "Spell is on cooldown." reason.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cast_spell_rejected_during_cooldown() {
    let h = start_both().await;

    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "cdc", "Cooler", "Human", "Shaman").await;
    // Healing Wave requires level 4; provisioned characters start at 1.
    set_char_level(&h.db_url, a_char_id, 4).await;
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;

    for _ in 0..4 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }

    // First cast â€” full gate flow (CastStart â†’ wait â†’ CastSpell).
    a.send_cast_start("Healing Wave", 1.0);
    for _ in 0..3 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }
    a.pump_for(Duration::from_millis(1100)).await;
    a.send_cast_spell("Healing Wave", None);
    for _ in 0..6 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }

    // Confirm the first cast landed (BuffSnapshot includes Healing Wave).
    let _snap = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::BuffSnapshot { target, buffs }
                if *target == a_char_id as u64
                    && buffs.iter().any(|(n, _)| n == "Healing Wave"))
        })
        .await
        .expect("first Healing Wave cast lands");

    // Second cast â€” same full flow, fired well inside the 6 s
    // cooldown window. Should be rejected with the cooldown reason.
    a.send_cast_start("Healing Wave", 1.0);
    for _ in 0..3 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }
    a.pump_for(Duration::from_millis(1100)).await;
    a.send_cast_spell("Healing Wave", None);
    for _ in 0..6 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }

    let fail = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(2), |m| {
            matches!(m, ServerWorldMsg::CastFail { caster, reason }
                if *caster == a_char_id as u64 && reason == "Spell is on cooldown.")
        })
        .await
        .expect("second cast within the cooldown window is rejected");
    if let ServerWorldMsg::CastFail { reason, .. } = fail {
        assert_eq!(reason, "Spell is on cooldown.");
    }
}

/// Track 17.2 â€” movement-during-cast gate. Send CastStart at the
/// spawn position, then walk well past the 1 m gate threshold while
/// the cast bar runs, then send CastSpell after the cast time elapsed.
/// The cast-time gate passes (enough time elapsed); the movement gate
/// rejects with an "interrupted (moved)" reason.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cast_spell_rejected_when_caster_moved_during_cast() {
    let h = start_both().await;

    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "mvc", "Walker", "Human", "Shaman").await;
    // Healing Wave requires level 4; provisioned characters start at 1.
    set_char_level(&h.db_url, a_char_id, 4).await;
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;

    for _ in 0..4 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }

    a.send_cast_start("Healing Wave", 1.0);
    for _ in 0..3 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }
    // Walk forward for the full cast duration. MAX_MOVE_SPEED Ã— 1.1 s
    // â‰ˆ 8 m on the server â€” well past the 1 m gate.
    for seq in 1..=22u32 {
        a.send_move(seq, Vec3 { x: 0.0, y: 0.0, z: 1.0 });
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    a.send_cast_spell("Healing Wave", None);
    for _ in 0..6 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }

    let fail = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(2), |m| {
            matches!(m, ServerWorldMsg::CastFail { caster, reason }
                if *caster == a_char_id as u64 && reason.starts_with("interrupted"))
        })
        .await
        .expect("cast is rejected when caster moved past the gate threshold");
    if let ServerWorldMsg::CastFail { reason, .. } = fail {
        assert_eq!(reason, "interrupted (moved)");
    }

    // Confirm the buff did NOT apply â€” there should be no BuffSnapshot
    // containing Healing Wave after the rejection.
    let snap = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_millis(500), |m| {
            matches!(m, ServerWorldMsg::BuffSnapshot { target, buffs }
                if *target == a_char_id as u64
                    && buffs.iter().any(|(n, _)| n == "Healing Wave"))
        })
        .await;
    assert!(snap.is_none(), "movement-interrupted cast must not apply the buff");
}

/// Track 19A â€” a hit on a casting player rolls the channeling-based
/// interrupt; for a non-caster (Warrior, channeling cap = 0) the
/// chance is 1.0 â†’ always interrupted. Using PvP path avoids the
/// flaky AI-walks-into-melee timing. A: Warrior; B: Warrior (any
/// attacker works). Both /pvp on, A "starts" a long cast via a
/// CastStartBroadcast (server doesn't validate class on CastStart;
/// validation happens at CastSpell). B attacks A; server fans
/// CastFail("interrupted (hit during cast)") to A.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cast_interrupted_by_incoming_pvp_hit() {
    let h = start_both().await;
    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "intv", "Inta", "Human", "Warrior").await;
    let (b_session, b_char_id, b_token) =
        provision_client(&h.auth_url, "intw", "Intb", "Human", "Warrior").await;

    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    let mut b = WorldClient::start(b_token, &b_session, b_char_id).await;

    // Both clients enter world + flip /pvp on.
    for _ in 0..4 {
        tick_one(&mut a.client, &mut a.transport);
        tick_one(&mut b.client, &mut b.transport);
        tokio::time::sleep(TICK_DT).await;
    }
    a.send_pvp_toggle(true);
    b.send_pvp_toggle(true);
    for _ in 0..3 {
        tick_one(&mut a.client, &mut a.transport);
        tick_one(&mut b.client, &mut b.transport);
        tokio::time::sleep(TICK_DT).await;
    }

    // A "starts" a 5-second cast â€” long enough to keep the cast
    // cache populated until B's attack lands. Warrior has channeling
    // cap = 0, so the on-hit interrupt chance is 1.0 (deterministic).
    a.send_cast_start("Fake Long Cast", 5.0);
    for _ in 0..3 {
        tick_one(&mut a.client, &mut a.transport);
        tick_one(&mut b.client, &mut b.transport);
        tokio::time::sleep(TICK_DT).await;
    }
    // B attacks A. Both spawned at the same DB-loaded position so
    // they're in melee range immediately; bare-fist attack uses the
    // default melee envelope.
    b.send_attack(a_char_id as u64, "", false, DamageType::Physical);
    for _ in 0..6 {
        tick_one(&mut a.client, &mut a.transport);
        tick_one(&mut b.client, &mut b.transport);
        tokio::time::sleep(TICK_DT).await;
    }

    let fail = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(2), |m| {
            matches!(m, ServerWorldMsg::CastFail { caster, reason }
                if *caster == a_char_id as u64
                    && reason == "interrupted (hit during cast)")
        })
        .await
        .expect("Warrior with no channeling skill is always interrupted on incoming hit");
    if let ServerWorldMsg::CastFail { reason, .. } = fail {
        assert_eq!(reason, "interrupted (hit during cast)");
    }
}

/// Track 18.1 â€” on EnterWorld the server fans a SkillProgressSnapshot
/// containing the player's three score maps. The lib unit tests verify
/// the cap math; this test verifies the wire shape and that the seed
/// matches WeaponSkills' starting values for the character's class.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn skill_progress_snapshot_seeded_on_enter_world() {
    let h = start_both().await;
    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "skl", "Skiller", "Human", "Warrior").await;
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;

    let snap = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::SkillProgressSnapshot { .. })
        })
        .await
        .expect("SkillProgressSnapshot arrives on EnterWorld");

    if let ServerWorldMsg::SkillProgressSnapshot { weapon, armor, casting } = snap {
        // Shape: 10 weapon keys, 5 armor keys, 7 casting keys (one per
        // GDScript definition entry, regardless of whether the class
        // can train it). Casting went 6 -> 7 on 2026-07-20 when the
        // `meditate` regen skill was added (skills.rs CASTING_KEYS).
        assert_eq!(weapon.len(), 10, "weapon map has 10 keys");
        assert_eq!(armor.len(), 5, "armor map has 5 keys");
        assert_eq!(casting.len(), 7, "casting map has 7 keys");

        // Track 22.F rebalance: starting score is cap(L1) / 4 with
        // a floor of 1. Warrior 1h_slashing L1 cap = 4 â†’ 4/4 = 1.
        let weapon_map: std::collections::HashMap<String, u32> = weapon.into_iter().collect();
        assert_eq!(weapon_map.get("1h_slashing").copied(), Some(1));
        // Warrior plate L1 cap = 4 â†’ 4/4 = 1 (same floor).
        let armor_map: std::collections::HashMap<String, u32> = armor.into_iter().collect();
        assert_eq!(armor_map.get("plate").copied(), Some(1));
        // Warrior has no casting; all six rows present at 0.
        let casting_map: std::collections::HashMap<String, u32> = casting.into_iter().collect();
        for key in &["evocation", "alteration", "abjuration", "conjuration", "divination", "channeling"] {
            assert_eq!(casting_map.get(*key).copied(), Some(0), "warrior casting {} is 0", key);
        }
    }
}

/// Track 18.1 â€” `character_skills` rows survive disconnect and are
/// re-applied on reconnect. We pre-write a row directly to SQLite
/// before EnterWorld so the load path overlays our value on top of
/// the seed_starting_scores baseline; the snapshot must reflect the
/// pre-written score. Mirrors the bag_contents_persist_across_reconnect
/// pattern from Track 14.3.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn skill_progress_persists_load_path() {
    let h = start_both().await;
    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "skp", "Sklp", "Human", "Cleric").await;

    // Pre-seed a non-default casting score via direct SQL. Mimics what
    // the server's save path would write at checkpoint / disconnect
    // after an in-flight advance â€” proves the load + snapshot path
    // independently of the rng-driven try_advance roll.
    let pool = projectdawn_server::db::open(&h.db_url).await.expect("open pool");
    sqlx::query(
        "INSERT INTO character_skills (char_id, kind, key, score)
         VALUES (?1, 'casting', 'alteration', 17)",
    )
    .bind(a_char_id)
    .execute(&pool)
    .await
    .expect("seed alteration row");

    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;

    let snap = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::SkillProgressSnapshot { .. })
        })
        .await
        .expect("SkillProgressSnapshot arrives");

    if let ServerWorldMsg::SkillProgressSnapshot { casting, .. } = snap {
        let casting_map: std::collections::HashMap<String, u32> = casting.into_iter().collect();
        assert_eq!(
            casting_map.get("alteration").copied(),
            Some(17),
            "persisted alteration score overlays the L1 default"
        );
    }
}

fn tick_one(client: &mut RenetClient, transport: &mut NetcodeClientTransport) {
    client.update(TICK_DT);
    if let Err(e) = transport.update(TICK_DT, client) {
        eprintln!("client transport update: {e}");
    }
    if let Err(e) = transport.send_packets(client) {
        eprintln!("client transport send: {e}");
    }
}

fn send_msg(client: &mut RenetClient, channel: u8, msg: &ClientWorldMsg) {
    let bytes = bincode::serde::encode_to_vec(msg, bincode_cfg()).expect("encode");
    client.send_message(channel, bytes);
}

fn connection_config_matching_server() -> ConnectionConfig {
    use renet::{ChannelConfig, SendType};
    let chans = vec![
        ChannelConfig {
            channel_id: CHANNEL_SYSTEM,
            max_memory_usage_bytes: 64 * 1024,
            send_type: SendType::ReliableOrdered {
                resend_time: Duration::from_millis(150),
            },
        },
        ChannelConfig {
            channel_id: CHANNEL_POSITION,
            max_memory_usage_bytes: 32 * 1024,
            send_type: SendType::Unreliable,
        },
    ];
    ConnectionConfig {
        available_bytes_per_tick: 60_000,
        server_channels_config: chans.clone(),
        client_channels_config: chans,
    }
}

async fn rpc(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    payload: serde_json::Value,
) -> serde_json::Value {
    ws.send(Message::Text(payload.to_string()))
        .await
        .expect("send");
    let frame = ws.next().await.expect("frame").expect("ok frame");
    let text = match frame {
        Message::Text(t) => t.to_string(),
        other => panic!("unexpected non-text frame: {other:?}"),
    };
    serde_json::from_str(&text).expect("parse server json")
}

fn decode_token_bytes(hex_str: &str) -> [u8; 32] {
    let v = hex::decode(hex_str).expect("hex decode session_token");
    v.try_into().expect("32 bytes")
}

/// Request a FRESH world ConnectToken for an existing session + char. Used when
/// account state changed after the initial `provision_client` (e.g. `is_gm` was
/// flipped), so the new token reflects it â€” the flag is packed at mint time.
async fn request_world_token(auth_url: &str, session_token_hex: &str, char_id: i64) -> Vec<u8> {
    let (mut ws, _) = tokio_tungstenite::connect_async(auth_url)
        .await
        .expect("auth ws connect");
    let resp = rpc(
        &mut ws,
        serde_json::json!({
            "type": "RequestWorldToken",
            "session_token": session_token_hex,
            "char_id": char_id,
        }),
    )
    .await;
    assert_eq!(resp["type"], "WorldConnectToken", "request token: {resp}");
    resp["token_bytes"]
        .as_array()
        .expect("token_bytes is array")
        .iter()
        .map(|v| v.as_u64().expect("byte") as u8)
        .collect()
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
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::XpGained { amount, .. } if *amount == PROBE_XP)
        })
        .await;
    assert!(
        gm_xp.is_some(),
        "GM account: GrantQuestXp should apply and produce an XpGained(amount={PROBE_XP})"
    );

    // Plain account sends the same â†’ the server ignores it, so no gain arrives
    // (only the connect-time seed, which the amount filter excludes).
    plain.send_grant_quest_xp(PROBE_XP);
    let plain_xp = plain
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(2), |m| {
            matches!(m, ServerWorldMsg::XpGained { amount, .. } if *amount == PROBE_XP)
        })
        .await;
    assert!(
        plain_xp.is_none(),
        "plain account: GrantQuestXp must be a silent no-op (no gain applied)"
    );
}

/// Phase 1 finding 3 â€” the Respawn dead-check. A LIVING player's Respawn must be a
/// no-op (the exploit spammed it to floor HP at 25% for near-invulnerability); a
/// DEAD player's Respawn must still restore ~25% (the legit path). Drives the real
/// flow: connect at full HP, Respawn (rejected), DeathBroadcast (kill), Respawn
/// (restores). HP is observed via HealthUpdate. ~25% is the Respawn floor value.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn respawn_requires_being_dead() {
    let h = start_both().await;
    let (session, char_id, token) =
        provision_client(&h.auth_url, "respawner", "Respawna", "Human", "Warrior").await;
    let mut c = WorldClient::start(token, &session, char_id).await;
    let cid = char_id as u64;

    // (1) LIVING player: Respawn must NOT floor HP. We start at full HP and take no
    //     damage, so a ~25% HealthUpdate would only come from a wrongly-honored Respawn.
    c.send_respawn();
    let floored = c
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(2), |m| {
            matches!(m, ServerWorldMsg::HealthUpdate { id, hp, max_hp }
                if *id == cid && *hp >= *max_hp * 0.2 && *hp <= *max_hp * 0.3)
        })
        .await;
    assert!(
        floored.is_none(),
        "a living player's Respawn must be a no-op, not a floor to 25% (finding 3)"
    );

    // (2) Legit path intact: die (DeathBroadcast -> kill_player -> hp 0) ...
    c.send_death();
    let dead = c
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::HealthUpdate { id, hp, .. } if *id == cid && *hp <= 0.01)
        })
        .await;
    assert!(dead.is_some(), "DeathBroadcast should kill the player (hp -> 0)");

    // ... then Respawn now restores HP to ~25%.
    c.send_respawn();
    let respawned = c
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::HealthUpdate { id, hp, max_hp }
                if *id == cid && *hp >= *max_hp * 0.2 && *hp <= *max_hp * 0.3)
        })
        .await;
    assert!(
        respawned.is_some(),
        "a dead player's Respawn should still restore HP to ~25%"
    );
}


/// Playtest 2026-09-23 â€” the dead-state gate. A dead player could loot
/// their own corpse before respawning and keep all the gear, voiding the
/// corpse-run penalty. Now any economy/combat intent from a dead player is
/// refused with a chat line, Respawn still works, and normal handling
/// resumes after it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dead_players_cannot_loot_or_touch_inventory() {
    let h = start_both().await;
    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "dedguy", "Deddy", "Human", "Warrior").await;
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;

    a.send_death();
    for _ in 0..4 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }

    // Inventory intent while dead: refused with the line.
    a.send_move_item("base", 0, "base", 1);
    let refusal = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::ChatMessage { text, .. }
                if text.contains("while dead"))
        })
        .await;
    assert!(refusal.is_some(), "dead MoveItem must be refused with a chat line");

    // Loot intent while dead: refused BEFORE any bag lookup (id need not exist).
    a.send_loot_all(999_999);
    let refusal2 = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::ChatMessage { text, .. }
                if text.contains("while dead"))
        })
        .await;
    assert!(refusal2.is_some(), "dead LootAll must be refused with a chat line");

    // Respawn is still allowed and clears the gate: the bind/spawn Teleport
    // arrives, and a subsequent inventory intent is no longer dead-refused.
    a.send_respawn();
    let tp = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(5), |m| {
            matches!(m, ServerWorldMsg::Teleport { .. })
        })
        .await;
    assert!(tp.is_some(), "Respawn must still work for the dead");

    a.send_move_item("base", 0, "base", 1);
    let post = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_millis(1500), |m| {
            matches!(m, ServerWorldMsg::ChatMessage { text, .. }
                if text.contains("while dead"))
        })
        .await;
    assert!(post.is_none(), "after respawn the dead-gate must be lifted");
}

// A forged Move carrying NaN/Infinity direction components must be dropped
// whole: `clamp_length` passes NaN through (`len > max` is false for NaN),
// after which conn.pos goes permanently NaN and every `dist > RANGE` refusal
// gate silently passes. The handler guard drops the packet before ANY state
// is touched â€” including the sequence bookkeeping, so a later honest move
// re-using that sequence number still applies.
#[tokio::test]
async fn nan_move_direction_is_dropped() {
    let h = start_both().await;
    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "nanmover", "Nanmover", "Human", "Warrior").await;
    let (b_session, b_char_id, b_token) =
        provision_client(&h.auth_url, "nanwatch", "Nanwatch", "Elf", "Cleric").await;
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    let mut b = WorldClient::start(b_token, &b_session, b_char_id).await;
    let _ = b_char_id;

    // Baseline: an honest move fans a finite Position for A at B.
    a.send_move(1, Vec3 { x: 1.0, y: 0.0, z: 0.0 });
    let baseline = b
        .wait_for(CHANNEL_POSITION, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::Position { id, .. } if *id == a_char_id as u64)
        })
        .await
        .expect("B sees A's baseline Position");
    if let ServerWorldMsg::Position { pos, .. } = &baseline {
        assert!(
            pos.x.is_finite() && pos.y.is_finite() && pos.z.is_finite(),
            "baseline position is finite"
        );
    }

    // Stop, then send the forgery: NaN x, Infinity z. If the guard is
    // missing, the tick integrates this into conn.pos and every Position
    // broadcast for A goes NaN from here on.
    a.send_move(2, Vec3 { x: 0.0, y: 0.0, z: 0.0 });
    a.send_move(3, Vec3 { x: f32::NAN, y: 0.0, z: f32::INFINITY });
    for _ in 0..8 {
        tick_one(&mut a.client, &mut a.transport);
        tick_one(&mut b.client, &mut b.transport);
        tokio::time::sleep(TICK_DT).await;
    }

    // The forged packet must not have consumed its sequence number (it was
    // dropped before bookkeeping), so an honest move re-using seq 3 applies â€”
    // and the Position it produces is still finite.
    a.send_move(3, Vec3 { x: 0.0, y: 0.0, z: 1.0 });
    let after = b
        .wait_for(CHANNEL_POSITION, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::Position { id, .. } if *id == a_char_id as u64)
        })
        .await
        .expect("A still moves after the forged packet");
    if let ServerWorldMsg::Position { pos, .. } = &after {
        assert!(
            pos.x.is_finite() && pos.y.is_finite() && pos.z.is_finite(),
            "position stayed finite after a NaN/Inf Move: {:?}",
            pos
        );
    }
}

// Dead-XP gate (decided 2026-09-19): a group member lying dead beside the
// mob collects NOTHING â€” no XP share, no quest tick, and no dilution of the
// pool. B groups with A and dies at A's feet; A then solo-kills a
// dev-spawned mob. A's XpGained must be the FULL solo amount (263 for a
// level-1 mob â€” not the 157 a two-way group split would pay), and B must
// see no positive XpGained at all. The range half of the eligibility rule
// rides the same `eligible` closure in `award_kill`, so this test covers
// the mechanism for both.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dead_group_member_gets_no_xp_share() {
    let h = start_both().await;

    // A needs is_gm for the dev-spawn; re-mint the token after the grant so
    // the flag rides it (same pattern as the AOE test).
    let (a_session, a_char_id, _stale_token) =
        provision_client(&h.auth_url, "xpsolo", "Xpsolo", "Human", "Warrior").await;
    let pool = db::open(&h.db_url).await.expect("open pool");
    db::set_account_gm(&pool, "xpsolo", true).await.expect("set is_gm");
    let a_token = request_world_token(&h.auth_url, &a_session, a_char_id).await;
    let (b_session, b_char_id, b_token) =
        provision_client(&h.auth_url, "xpdead", "Xpdead", "Elf", "Cleric").await;

    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    let mut b = WorldClient::start(b_token, &b_session, b_char_id).await;
    let _ = b_char_id;

    // Group up: A invites by name, B accepts by A's id, A sees the roster.
    a.send_group_invite("Xpdead");
    for _ in 0..3 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }
    b.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::GroupInvited { from_id, .. }
            if *from_id == a_char_id as u64)
    })
    .await
    .expect("B receives the group invite");
    b.send_group_accept(a_char_id as u64);
    // Pump B so the accept actually reaches the wire (wait_for only ticks
    // the client it is called on).
    for _ in 0..4 {
        tick_one(&mut b.client, &mut b.transport);
        tokio::time::sleep(TICK_DT).await;
    }
    a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::GroupRoster { members, .. } if members.len() == 2)
    })
    .await
    .expect("A sees the two-member roster");

    // B dies right here at the spawn â€” a corpse beside the coming kill,
    // the exact scenario the pure-proximity rule would have paid.
    b.send_death();
    for _ in 0..6 {
        tick_one(&mut b.client, &mut b.transport);
        tokio::time::sleep(TICK_DT).await;
    }

    // A conjures a level-1 dummy that walks to A on its own (aggro 12,
    // 1 dmg so its swings are harmless), then kills it with paced swings
    // (the swing-rate limiter drops anything faster than the 0.65 s
    // bare-hand floor). 10 HP vs ~5-8 per swing = 2-3 landed hits. Same
    // shape as player_attack_kills_enemy_and_corpse_despawns: waiting for
    // ITS hit on us proves it closed into melee range first.
    a.send_dev_spawn("XP Filter Dummy", 1, 10.0, 1, 1.8, 12.0);
    let spawn_evt = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::EnemySpawn { mob_name, .. }
                if mob_name == "XP Filter Dummy")
        })
        .await
        .expect("the dev-spawned dummy fans an EnemySpawn");
    let enemy_id: u64 = match spawn_evt {
        ServerWorldMsg::EnemySpawn { id, .. } => id,
        _ => unreachable!(),
    };
    // Wait for the dummy's swing on us (proves melee adjacency) while
    // pumping BOTH transports â€” a one-sided wait leaves B's socket
    // unserviced under the 20 Hz position fan (the 2026-09-16 starvation
    // class), and a B disconnect would make the no-leak assertion below
    // vacuous (an offline B was already excluded before this change).
    let deadline = Instant::now() + Duration::from_secs(35);
    let mut adjacent = false;
    'adjacency: while Instant::now() < deadline {
        tick_one(&mut a.client, &mut a.transport);
        tick_one(&mut b.client, &mut b.transport);
        while let Some(bytes) = a.client.receive_message(CHANNEL_SYSTEM) {
            if let Ok((msg, _)) = bincode::serde::decode_from_slice::<ServerWorldMsg, _>(
                &bytes,
                bincode_cfg(),
            ) {
                if matches!(msg, ServerWorldMsg::Hit { attacker, .. } if attacker == enemy_id)
                {
                    adjacent = true;
                    break 'adjacency;
                }
            }
        }
        tokio::time::sleep(TICK_DT).await;
    }
    assert!(adjacent, "the dummy walks into melee and swings");

    const SWING_GAP: Duration = Duration::from_millis(1000);
    for _ in 0..6 {
        a.send_attack(enemy_id, "", false, DamageType::Physical);
        let until = Instant::now() + SWING_GAP;
        while Instant::now() < until {
            tick_one(&mut a.client, &mut a.transport);
            tick_one(&mut b.client, &mut b.transport);
            tokio::time::sleep(TICK_DT).await;
        }
    }

    // A collects the FULL solo amount: the dead member neither shares nor
    // dilutes (263 = kill_xp(1); a leaked two-way split would pay 157).
    let xp_evt = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(5), |m| {
            matches!(m, ServerWorldMsg::XpGained { amount, .. } if *amount > 0)
        })
        .await
        .expect("the living killer still collects");
    if let ServerWorldMsg::XpGained { amount, .. } = xp_evt {
        assert_eq!(
            amount, 263,
            "dead member must not dilute the pool: solo 263, not a 157 split"
        );
    }

    // B (dead, still connected under the death lock) gets nothing. The
    // connect-time bar seed is amount 0, so filter on positive amounts.
    let leak = b
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(2), |m| {
            matches!(m, ServerWorldMsg::XpGained { amount, .. } if *amount > 0)
        })
        .await;
    assert!(leak.is_none(), "a dead group member must receive no XP share");
}

// Silent-refusals closeout (2026-09-30): the cast resolver deducts mana at
// the top of the handler, so an arm that rejected afterwards and returned
// silently charged full price for nothing. The headline case is the one a
// player hits by accident: firing a nuke with no target selected. Assert
// BOTH halves of the fix â€” the refusal line, and the mana coming back.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn enemy_spell_without_a_target_refunds_and_reports() {
    let h = start_both().await;
    // Fireball is Magician-only in spells.toml, and the class/level gate runs
    // BEFORE the target arm â€” roll the class that can actually cast it, or
    // this tests the wrong refusal.
    let (a_session, a_char_id, _stale) =
        provision_client(&h.auth_url, "notarget", "Notarg", "Human", "Magician").await;
    set_char_level(&h.db_url, a_char_id, 12).await;
    let a_token = request_world_token(&h.auth_url, &a_session, a_char_id).await;
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;

    // Let enter-world settle, then latch the starting mana from the seed.
    a.pump_for(Duration::from_millis(400)).await;

    // Run the cast bar, then send the cast with NO target id.
    a.send_cast_start("Fireball", 1.5);
    a.pump_for(Duration::from_millis(1700)).await;
    a.send_cast_spell("Fireball", None);

    // The refusal arrives as a System chat line.
    let refusal = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::ChatMessage { text, .. }
                if text.contains("need a target"))
        })
        .await;
    assert!(
        refusal.is_some(),
        "a targetless ENEMY cast must answer instead of failing silently"
    );

    // And the mana is given back: the LAST ManaUpdate for this caster must
    // report full mana, not the post-deduct value.
    let mut last_mp: Option<(f32, f32)> = None;
    a.pump_for(Duration::from_millis(600)).await;
    while let Some(bytes) = a.client.receive_message(CHANNEL_SYSTEM) {
        if let Ok((msg, _)) =
            bincode::serde::decode_from_slice::<ServerWorldMsg, _>(&bytes, bincode_cfg())
        {
            if let ServerWorldMsg::ManaUpdate { id, mp, max_mp } = msg {
                if id == a_char_id as u64 {
                    last_mp = Some((mp, max_mp));
                }
            }
        }
    }
    if let Some((mp, max_mp)) = last_mp {
        assert!(
            (mp - max_mp).abs() < 0.01,
            "mana must be refunded after a refused cast (got {mp} of {max_mp})"
        );
    }
}

// ── PD_W0028 — the trade window (docs/design/trade_window.md). The tests
// below are the exploit ledger made executable: atomic two-sided commit,
// the every-edit-clears-both-accepts law, the escrow lock, self-trade and
// coin-overdraft refusals. ──

/// Shared setup: A (GM, so it can conjure goods/coins) and B connected at
/// the spawn, with A holding 3 Bread Loaf at a known slot and 25 copper.
async fn trade_pair(
    h: &Harness,
    a_user: &str,
    a_name: &str,
    b_user: &str,
    b_name: &str,
) -> (WorldClient, i64, WorldClient, i64, String, u32) {
    let (a_session, a_char_id, _stale) =
        provision_client(&h.auth_url, a_user, a_name, "Human", "Warrior").await;
    let pool = db::open(&h.db_url).await.expect("open pool");
    db::set_account_gm(&pool, a_user, true).await.expect("set is_gm");
    let a_token = request_world_token(&h.auth_url, &a_session, a_char_id).await;
    let (b_session, b_char_id, b_token) =
        provision_client(&h.auth_url, b_user, b_name, "Elf", "Cleric").await;

    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    let b = WorldClient::start(b_token, &b_session, b_char_id).await;

    // The wire line carries no slash: the client's /give strips it before
    // sending (`give <item name> [qty]`, handlers.rs).
    a.send_gm_command("give Bread Loaf 3");
    send_msg(
        &mut a.client,
        CHANNEL_SYSTEM,
        &ClientWorldMsg::GiveCoins { platinum: 0, gold: 0, silver: 0, copper: 25 },
    );
    let bread = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::InventoryDelta { item_path: Some(p), .. }
                if p.contains("bread"))
        })
        .await
        .expect("the bread lands");
    let (bread_loc, bread_slot) = match bread {
        ServerWorldMsg::InventoryDelta { location, slot, .. } => (location, slot),
        _ => unreachable!(),
    };
    (a, a_char_id, b, b_char_id, bread_loc, bread_slot)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn trade_commits_atomically_and_pays_both_sides() {
    let h = start_both().await;
    let (mut a, _a_char_id, mut b, b_char_id, bread_loc, bread_slot) =
        trade_pair(&h, "tradea", "Tradealy", "tradeb", "Tradebel").await;

    // Flush A's outbound intents before waiting on the peer: wait_for only
    // pumps the client it is called on, so A's packets otherwise sit in A's
    // transport buffer.
    macro_rules! pump_a {
        () => {{
            for _ in 0..4 {
                tick_one(&mut a.client, &mut a.transport);
                tokio::time::sleep(TICK_DT).await;
            }
        }};
    }

    a.send_trade_request(b_char_id as u64);
    pump_a!();
    a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::TradeOpened { partner_id, .. }
            if *partner_id == b_char_id as u64)
    })
    .await
    .expect("A's window opens");
    b.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::TradeOpened { .. })
    })
    .await
    .expect("B's window opens instantly (no accept prompt)");

    // A offers the bread and the copper; B offers nothing (a gift).
    a.send_trade_offer_item(0, &bread_loc, bread_slot);
    pump_a!();
    b.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::TradeOfferUpdate { mine: false, slots, .. }
            if slots.first().map(|(p, c)| p.contains("bread") && *c == 3).unwrap_or(false))
    })
    .await
    .expect("B sees A's offered stack, path and count");
    a.send_trade_offer_coins(protocol::world::Coins {
        platinum: 0,
        gold: 0,
        silver: 0,
        copper: 25,
    });
    pump_a!();

    a.send_trade_accept();
    pump_a!();
    b.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::TradeAcceptState { you: false, them: true })
    })
    .await
    .expect("B sees A standing accepted");
    b.send_trade_accept();

    // B receives the goods, then the committed close. (wait_for drops
    // non-matching messages, so assert in fan order: delta, coins, close.)
    b.wait_for(CHANNEL_SYSTEM, Duration::from_secs(5), |m| {
        matches!(m, ServerWorldMsg::InventoryDelta { item_path: Some(p), count: 3, .. }
            if p.contains("bread"))
    })
    .await
    .expect("B receives the bread stack");
    b.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::CoinsUpdate { coins } if coins.copper >= 25)
    })
    .await
    .expect("B receives the copper");
    b.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::TradeClosed { committed: true, .. })
    })
    .await
    .expect("B's close says committed");

    // A's side: the offered slot cleared and the close says committed.
    a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(5), |m| {
        matches!(m, ServerWorldMsg::InventoryDelta { location, slot, item_path: None, .. }
            if *location == bread_loc && *slot == bread_slot)
    })
    .await
    .expect("A's offered slot empties");
    a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::TradeClosed { committed: true, .. })
    })
    .await
    .expect("A's close says committed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn trade_edits_clear_accepts_and_locks_hold() {
    let h = start_both().await;
    let (mut a, _a_char_id, mut b, b_char_id, bread_loc, bread_slot) =
        trade_pair(&h, "tradec", "Tradecyn", "traded", "Tradedor").await;

    a.send_trade_request(b_char_id as u64);
    a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::TradeOpened { .. })
    })
    .await
    .expect("A's window opens");
    a.send_trade_offer_item(0, &bread_loc, bread_slot);
    a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::TradeOfferUpdate { mine: true, .. })
    })
    .await
    .expect("A sees its own offer");

    // Ledger 2 — the escrow: the offered slot refuses a MoveItem with the
    // lock's chat line, and the stack stays put.
    a.send_move_item(&bread_loc, bread_slot, "base", 7);
    a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::ChatMessage { text, .. }
            if text.contains("offered in a trade"))
    })
    .await
    .expect("locked slot refuses the move, with the line");

    // Ledger 1 — bait-and-switch: A accepts, then B edits; BOTH accepts
    // clear, and B's later lone accept commits nothing.
    a.send_trade_accept();
    for _ in 0..4 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }
    b.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::TradeAcceptState { them: true, .. })
    })
    .await
    .expect("B sees A accepted");
    b.send_trade_offer_coins(protocol::world::Coins::ZERO);
    for _ in 0..4 {
        tick_one(&mut b.client, &mut b.transport);
        tokio::time::sleep(TICK_DT).await;
    }
    a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::TradeAcceptState { you: false, them: false })
    })
    .await
    .expect("the edit cleared BOTH accepts on A's display");
    b.send_trade_accept();
    let premature = b
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(2), |m| {
            matches!(m, ServerWorldMsg::TradeClosed { committed: true, .. })
        })
        .await;
    assert!(
        premature.is_none(),
        "one accept after the edit must not commit — A's stale accept is dead"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn trade_refuses_self_and_coin_overdraft() {
    let h = start_both().await;
    let (mut a, a_char_id, mut b, b_char_id, _bread_loc, _bread_slot) =
        trade_pair(&h, "tradee", "Tradeeva", "tradef", "Tradefin").await;

    // Ledger 8 — self-trade.
    a.send_trade_request(a_char_id as u64);
    a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::ChatMessage { text, .. }
            if text.contains("yourself"))
    })
    .await
    .expect("self-trade refuses");

    // Ledger 4 — overdraft: B offers coin it cannot cover.
    a.send_trade_request(b_char_id as u64);
    for _ in 0..4 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }
    b.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::TradeOpened { .. })
    })
    .await
    .expect("window opens");
    b.send_trade_offer_coins(protocol::world::Coins {
        platinum: 999,
        gold: 0,
        silver: 0,
        copper: 0,
    });
    b.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::ChatMessage { text, .. }
            if text.contains("that much coin"))
    })
    .await
    .expect("the overdraft refuses with the line");
}

/// Scale readiness: an idle enemy's Position must not fan at 20 Hz. The
/// statue has aggro 0 and speed 0, so it never targets and never moves.
/// Before the change-or-keepalive gate it still produced one Position per
/// tick per visible player (~40 in 2 s); after, the first-tick send plus
/// one keepalive per `ENEMY_POSITION_KEEPALIVE` gap, so 2..=8 with slack.
#[tokio::test]
async fn idle_enemy_position_fan_is_gated() {
    let h = start_both().await;
    let (a_session, a_char_id, _stale_token) =
        provision_client(&h.auth_url, "idlefan", "Idlefan", "Human", "Warrior").await;
    let pool = db::open(&h.db_url).await.expect("open pool");
    db::set_account_gm(&pool, "idlefan", true).await.expect("set is_gm");
    let a_token = request_world_token(&h.auth_url, &a_session, a_char_id).await;
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;

    a.send_dev_spawn("Idle Statue", 1, 10.0, 0, 0.0, 0.0);
    let spawn_evt = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::EnemySpawn { mob_name, .. } if mob_name == "Idle Statue")
        })
        .await
        .expect("the statue fans an EnemySpawn");
    let statue_id: u64 = match spawn_evt {
        ServerWorldMsg::EnemySpawn { id, .. } => id,
        _ => unreachable!(),
    };

    // Count the statue's Position messages over a 2 s window, servicing the
    // transport every tick so nothing is lost in the socket buffer.
    let end = Instant::now() + Duration::from_secs(2);
    let mut count = 0u32;
    let mut last_seq: Option<u32> = None;
    while Instant::now() < end {
        tick_one(&mut a.client, &mut a.transport);
        while let Some(bytes) = a.client.receive_message(CHANNEL_POSITION) {
            if let Ok((msg, _)) = bincode::serde::decode_from_slice::<ServerWorldMsg, _>(
                &bytes,
                bincode_cfg(),
            ) {
                if let ServerWorldMsg::Position { id, sequence, .. } = msg {
                    if id == statue_id {
                        count += 1;
                        if let Some(prev) = last_seq {
                            assert!(
                                sequence > prev,
                                "statue Position sequence must be strictly increasing ({prev} then {sequence})"
                            );
                        }
                        last_seq = Some(sequence);
                    }
                }
            }
        }
        tokio::time::sleep(TICK_DT).await;
    }
    eprintln!("idle statue Position messages in 2 s: {count}");
    assert!(
        (2..=8).contains(&count),
        "an idle enemy should fan a first send plus ~500 ms keepalives over 2 s, got {count} (20 Hz would be ~40)"
    );
}

// ── AOI cell-crossing fan-out ─────────────────────────────────────────────
//
// Geometry shared by the tests below. STARTER_SPAWN is (0,0,0) = cell (0,0)
// and CELL_SIZE is 120 m, so a character seeded at x=248 sits in cell (2,0):
// two cells from spawn, outside its 3x3, invisible to anyone there. Walking
// 15 m west (40 Moves at 7.5 m/s) crosses the x=240 line into cell (1,0),
// which IS adjacent to (0,0). The server derives the AOI cell from the
// position it loads at EnterWorld, so the DB seed is all it takes.

/// Pre-seed a character's world position before it connects, the same slot
/// `set_char_level` uses.
async fn set_char_pos(db_url: &str, char_id: i64, x: f32, z: f32) {
    let pool = projectdawn_server::db::open(db_url).await.expect("open pool");
    sqlx::query("UPDATE characters SET pos_x = ?1, pos_y = 0.0, pos_z = ?2 WHERE id = ?3")
        .bind(x as f64)
        .bind(z as f64)
        .bind(char_id)
        .execute(&pool)
        .await
        .expect("seed character position");
}

/// A transport-level disconnect mid-helper otherwise reads as a silent
/// timeout on whatever the caller was waiting for (the trap `wait_for`
/// surfaces for a single client); fail loudly with the reason instead.
fn assert_both_connected(a: &WorldClient, b: &WorldClient, during: &str) {
    assert!(
        !a.client.is_disconnected() && !b.client.is_disconnected(),
        "a client DISCONNECTED during {during} (a: {:?}, b: {:?})",
        a.client.disconnect_reason(),
        b.client.disconnect_reason()
    );
}

/// Wait for a CHANNEL_SYSTEM message on `a` that satisfies `pred` while
/// pumping BOTH transports and heart-beating both clients, so neither socket
/// starves under the position fan and neither trips the app-layer idle
/// timeout (only app-layer messages touch the connection; netcode traffic
/// from `tick_one` does not). `WorldClient::wait_for` only services the
/// client it is called on.
async fn wait_on_a_pumping_b(
    a: &mut WorldClient,
    b: &mut WorldClient,
    timeout: Duration,
    pred: impl Fn(&ServerWorldMsg) -> bool,
) -> Option<ServerWorldMsg> {
    let deadline = Instant::now() + timeout;
    let mut ticks: u32 = 0;
    a.send_heartbeat();
    b.send_heartbeat();
    while Instant::now() < deadline {
        tick_one(&mut a.client, &mut a.transport);
        tick_one(&mut b.client, &mut b.transport);
        assert_both_connected(a, b, "wait_on_a_pumping_b");
        ticks += 1;
        if ticks % 80 == 0 {
            a.send_heartbeat();
            b.send_heartbeat();
        }
        while let Some(bytes) = a.client.receive_message(CHANNEL_SYSTEM) {
            if let Ok((msg, _)) = bincode::serde::decode_from_slice::<ServerWorldMsg, _>(
                &bytes,
                bincode_cfg(),
            ) {
                if pred(&msg) {
                    return Some(msg);
                }
            }
        }
        tokio::time::sleep(TICK_DT).await;
    }
    None
}

/// Service both transports for `d`, heart-beating both first so a long pump
/// (a cast bar) cannot run a silent client into the idle timeout.
async fn pump_both_for(a: &mut WorldClient, b: &mut WorldClient, d: Duration) {
    a.send_heartbeat();
    b.send_heartbeat();
    let end = Instant::now() + d;
    while Instant::now() < end {
        tick_one(&mut a.client, &mut a.transport);
        tick_one(&mut b.client, &mut b.transport);
        assert_both_connected(a, b, "pump_both_for");
        tokio::time::sleep(TICK_DT).await;
    }
}

/// Send `ticks` Moves along `dir` from `who` while pumping `other`, then
/// stop; the server parks the mover after STALE_MOVE_THRESHOLD.
async fn walk_pumping(
    who: &mut WorldClient,
    other: &mut WorldClient,
    seq: &mut u32,
    dir: Vec3,
    ticks: u32,
) {
    other.send_heartbeat();
    for _ in 0..ticks {
        who.send_move(*seq, dir);
        *seq += 1;
        tick_one(&mut who.client, &mut who.transport);
        tick_one(&mut other.client, &mut other.transport);
        assert_both_connected(who, other, "walk_pumping");
        tokio::time::sleep(TICK_DT).await;
    }
}

/// B casts Summon Skeleton (3 s bar) and returns the pet id from B's own
/// PetSpawn. Nothing is attacking B, so there is no interrupt roll.
async fn summon_skeleton(b: &mut WorldClient, a: &mut WorldClient, b_char_id: i64) -> u64 {
    b.send_cast_start("Summon Skeleton", 3.0);
    pump_both_for(a, b, Duration::from_millis(150)).await;
    pump_both_for(a, b, Duration::from_millis(3100)).await;
    b.send_cast_spell("Summon Skeleton", None);
    let evt = b
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(5), |m| {
            matches!(m, ServerWorldMsg::PetSpawn { owner, .. } if *owner == b_char_id as u64)
        })
        .await
        .expect("B sees its own PetSpawn");
    match evt {
        ServerWorldMsg::PetSpawn { id, .. } => id,
        _ => unreachable!(),
    }
}

/// Scale readiness: an enemy crossing an AOI cell boundary must fan an
/// EnemySpawn to the players its new neighbourhood brings it into view of,
/// and an EntityDespawn to the ones it leaves. Before this the crossing only
/// updated the grid, so a mob chasing into view streamed Position for an id
/// the client had never been given a spawn for and stayed invisible.
#[tokio::test]
async fn enemy_crossing_a_cell_boundary_spawns_and_despawns_for_players() {
    let h = start_both().await;
    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "aoiwatch", "Aoiwatch", "Human", "Warrior").await;
    let (b_session, b_char_id, _stale_token) =
        provision_client(&h.auth_url, "aoipull", "Aoipull", "Elf", "Cleric").await;
    let pool = db::open(&h.db_url).await.expect("open pool");
    db::set_account_gm(&pool, "aoipull", true).await.expect("set is_gm");
    set_char_pos(&h.db_url, b_char_id, 248.0, 60.0).await;
    let b_token = request_world_token(&h.auth_url, &b_session, b_char_id).await;

    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    let mut b = WorldClient::start(b_token, &b_session, b_char_id).await;
    pump_both_for(&mut a, &mut b, Duration::from_millis(300)).await;

    // The runner spawns ~3 m from B, inside aggro, so it closes on B at once
    // and follows when B walks; 1 dmg swings are harmless. Chase leashes on
    // distance to the TARGET (aggro x 2), and B at 7.5 m/s opens the gap by
    // 4.5 m per wall-clock second of walking, so aggro is set to the 50 cap
    // (100 m leash) to keep a slow test loop from leashing it home.
    b.send_dev_spawn("Border Runner", 1, 10.0, 1, 3.0, 50.0);
    let spawn_evt = b
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::EnemySpawn { mob_name, .. } if mob_name == "Border Runner")
        })
        .await
        .expect("B, in the same cell, sees the dev spawn");
    let runner_id: u64 = match spawn_evt {
        ServerWorldMsg::EnemySpawn { id, .. } => id,
        _ => unreachable!(),
    };
    let early = wait_on_a_pumping_b(&mut a, &mut b, Duration::from_millis(300), |m| {
        matches!(m, ServerWorldMsg::EnemySpawn { id, .. } if *id == runner_id)
    })
    .await;
    assert!(early.is_none(), "a mob two cells away must not be seeded to A");

    let mut seq: u32 = 1;
    walk_pumping(&mut b, &mut a, &mut seq, Vec3 { x: -1.0, y: 0.0, z: 0.0 }, 40).await;
    let spawned = wait_on_a_pumping_b(&mut a, &mut b, Duration::from_secs(15), |m| {
        matches!(m, ServerWorldMsg::EnemySpawn { id, .. } if *id == runner_id)
    })
    .await;
    assert!(
        spawned.is_some(),
        "A must get an EnemySpawn when the chasing mob crosses into a cell adjacent to A's"
    );

    walk_pumping(&mut b, &mut a, &mut seq, Vec3 { x: 1.0, y: 0.0, z: 0.0 }, 50).await;
    let despawned = wait_on_a_pumping_b(&mut a, &mut b, Duration::from_secs(15), |m| {
        matches!(m, ServerWorldMsg::EntityDespawn { id } if *id == runner_id)
    })
    .await;
    assert!(
        despawned.is_some(),
        "A must get an EntityDespawn when the mob follows B back out of view"
    );
}

/// The pet arm of the same crossing: a pet following its owner across a cell
/// boundary fans a PetSpawn to the players it comes into view of.
#[tokio::test]
async fn pet_following_its_owner_across_a_cell_spawns_for_players() {
    let h = start_both().await;
    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "petwatch", "Petwatch", "Human", "Warrior").await;
    let (b_session, b_char_id, b_token) =
        provision_client(&h.auth_url, "petwalk", "Petwalk", "Human", "Necromancer").await;
    set_char_level(&h.db_url, b_char_id, 6).await;
    set_char_pos(&h.db_url, b_char_id, 248.0, 60.0).await;

    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    let mut b = WorldClient::start(b_token, &b_session, b_char_id).await;
    pump_both_for(&mut a, &mut b, Duration::from_millis(300)).await;

    let pet_id = summon_skeleton(&mut b, &mut a, b_char_id).await;
    assert!(pet_id >= PET_ID_BASE);
    let early = wait_on_a_pumping_b(&mut a, &mut b, Duration::from_millis(300), |m| {
        matches!(m, ServerWorldMsg::PetSpawn { id, .. } if *id == pet_id)
    })
    .await;
    assert!(early.is_none(), "a pet two cells away must not be seeded to A");

    let mut seq: u32 = 1;
    walk_pumping(&mut b, &mut a, &mut seq, Vec3 { x: -1.0, y: 0.0, z: 0.0 }, 40).await;
    let spawned = wait_on_a_pumping_b(&mut a, &mut b, Duration::from_secs(15), |m| {
        matches!(m, ServerWorldMsg::PetSpawn { id, owner, .. }
            if *id == pet_id && *owner == b_char_id as u64)
    })
    .await;
    assert!(
        spawned.is_some(),
        "A must get a PetSpawn when the pet follows its owner into a cell adjacent to A's"
    );
}

/// The player side of the pet gap: a player walking into view of an EXISTING
/// pet gets its PetSpawn. The id partitions stack (player < enemy < bag <
/// pet), so the player cell-change handler's bag/corpse arm swallowed pets.
#[tokio::test]
async fn player_crossing_into_view_of_an_existing_pet_gets_pet_spawn() {
    let h = start_both().await;
    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "petcomer", "Petcomer", "Human", "Warrior").await;
    set_char_pos(&h.db_url, a_char_id, 248.0, 60.0).await;
    let (b_session, b_char_id, b_token) =
        provision_client(&h.auth_url, "pethome", "Pethome", "Human", "Necromancer").await;
    set_char_level(&h.db_url, b_char_id, 6).await;

    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    let mut b = WorldClient::start(b_token, &b_session, b_char_id).await;
    pump_both_for(&mut a, &mut b, Duration::from_millis(300)).await;

    let pet_id = summon_skeleton(&mut b, &mut a, b_char_id).await;
    let early = wait_on_a_pumping_b(&mut a, &mut b, Duration::from_millis(300), |m| {
        matches!(m, ServerWorldMsg::PetSpawn { id, .. } if *id == pet_id)
    })
    .await;
    assert!(early.is_none(), "A starts two cells from the pet and must not have it yet");

    let mut seq: u32 = 1;
    walk_pumping(&mut a, &mut b, &mut seq, Vec3 { x: -1.0, y: 0.0, z: 0.0 }, 40).await;
    let spawned = wait_on_a_pumping_b(&mut a, &mut b, Duration::from_secs(10), |m| {
        matches!(m, ServerWorldMsg::PetSpawn { id, owner, .. }
            if *id == pet_id && *owner == b_char_id as u64)
    })
    .await;
    assert!(
        spawned.is_some(),
        "A must get the existing pet's PetSpawn on walking into its neighbourhood"
    );
}

// ── InspectPlayer range gate (exploit audit finding 10) ───────────────────

/// A paperdoll used to be readable from anywhere in the world. B is seeded
/// 200 m from A, far beyond INSPECT_RANGE: A's inspect must be refused with a
/// chat line and an empty result. An id that never existed must get the very
/// same answer, or the gate would be a char_id to name map and an is-online
/// oracle for anyone enumerating ids.
#[tokio::test]
async fn inspect_player_refuses_beyond_range() {
    let h = start_both().await;
    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "inspfar", "Inspfar", "Human", "Warrior").await;
    let (b_session, b_char_id, b_token) =
        provision_client(&h.auth_url, "insptgt", "Insptgt", "Elf", "Cleric").await;
    set_char_pos(&h.db_url, b_char_id, 200.0, 0.0).await;

    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    let mut b = WorldClient::start(b_token, &b_session, b_char_id).await;
    pump_both_for(&mut a, &mut b, Duration::from_millis(300)).await;

    const NOBODY: i64 = 999_999;
    for (label, target) in [("out of range", b_char_id), ("nonexistent", NOBODY)] {
        a.send_inspect_player(target);
        // The line, then the result, in the order the server sends them.
        let line = wait_on_a_pumping_b(&mut a, &mut b, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::ChatMessage { text, .. }
                if text.contains("not close enough to inspect"))
        })
        .await
        .unwrap_or_else(|| panic!("a {label} inspect must answer with the refusal line"));
        if let ServerWorldMsg::ChatMessage { text, .. } = line {
            assert!(
                !text.contains("Insptgt"),
                "the refusal must not name the target (got {text:?})"
            );
        }
        let result = wait_on_a_pumping_b(&mut a, &mut b, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::InspectResult { target_char_id, .. }
                if *target_char_id == target)
        })
        .await
        .unwrap_or_else(|| panic!("a {label} inspect still closes the client's waiting state"));
        if let ServerWorldMsg::InspectResult { target_name, slots, .. } = result {
            assert!(target_name.is_empty(), "a {label} inspect names nobody");
            assert!(slots.is_empty(), "a {label} inspect discloses no slots");
        }
    }
}

/// The honest path is untouched: 20 m apart, inside INSPECT_RANGE, the
/// inspect returns the target's name.
#[tokio::test]
async fn inspect_player_answers_within_range() {
    let h = start_both().await;
    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "inspnear", "Inspnear", "Human", "Warrior").await;
    let (b_session, b_char_id, b_token) =
        provision_client(&h.auth_url, "inspbud", "Inspbud", "Elf", "Cleric").await;
    set_char_pos(&h.db_url, b_char_id, 20.0, 0.0).await;

    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    let mut b = WorldClient::start(b_token, &b_session, b_char_id).await;
    pump_both_for(&mut a, &mut b, Duration::from_millis(300)).await;

    a.send_inspect_player(b_char_id);
    let result = wait_on_a_pumping_b(&mut a, &mut b, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::InspectResult { target_char_id, .. }
            if *target_char_id == b_char_id)
    })
    .await
    .expect("an in-range inspect answers");
    if let ServerWorldMsg::InspectResult { target_name, .. } = result {
        assert_eq!(target_name, "Inspbud");
    }
}

// ── GM action audit log ───────────────────────────────────────────────────

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
    db::set_account_gm(&pool, "auditgm", true).await.expect("set is_gm");
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
    wait_on_a_pumping_b(&mut gm, &mut plain, Duration::from_secs(5), |m| {
        matches!(m, ServerWorldMsg::EnemySpawn { mob_name, .. } if mob_name == "Audit Dummy")
    })
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
        .map(|r| (r.get("actor"), r.get("target_char"), r.get("command"), r.get("args")))
        .collect();
    assert_eq!(seen.len(), 2, "exactly the GM's two commands are recorded, got {seen:?}");
    for (actor, target_char, _, args) in &seen {
        assert_eq!(actor, "auditgm", "only the authorized account appears");
        assert_eq!(*target_char, gm_char);
        assert!(args.ends_with("via=gm"), "the row names the authority: {args}");
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
    db::set_account_gm(&pool, "burstgm", true).await.expect("set is_gm");
    let gm_token = request_world_token(&h.auth_url, &gm_session, gm_char).await;
    let mut gm = WorldClient::start(gm_token, &gm_session, gm_char).await;

    for _ in 0..34 {
        gm.send_grant_quest_xp(1);
    }
    // Marker: ordered after the burst, fanned after the flush that records it.
    gm.send_dev_spawn("Burst Marker", 1, 10.0, 0, 0.0, 0.0);
    gm.wait_for(CHANNEL_SYSTEM, Duration::from_secs(5), |m| {
        matches!(m, ServerWorldMsg::EnemySpawn { mob_name, .. } if mob_name == "Burst Marker")
    })
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

/// Charm had no range check: any live enemy id in the world could be charmed
/// from anywhere. This is that exploit as a client would run it: B, seeded two
/// cells away, conjures a dummy there; A, standing at the starter spawn and
/// never even told the dummy exists, names its id in a Charm. It must be
/// refused with the line, and the dummy must still be standing.
///
/// Nobody walks: an earlier shape walked the caster away from a dummy, and
/// under a loaded run the walk covered more ground in wall-clock time, strayed
/// into a camp, and the cast was interrupted by a mob instead.
#[tokio::test]
async fn charm_refuses_a_target_out_of_range() {
    let h = start_both().await;
    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "farench", "Farench", "Human", "Enchanter").await;
    set_char_level(&h.db_url, a_char_id, 20).await;
    let (b_session, b_char_id, _stale_token) =
        provision_client(&h.auth_url, "farspawn", "Farspawn", "Human", "Warrior").await;
    let pool = db::open(&h.db_url).await.expect("open pool");
    db::set_account_gm(&pool, "farspawn", true).await.expect("set is_gm");
    set_char_pos(&h.db_url, b_char_id, 248.0, 60.0).await;
    let b_token = request_world_token(&h.auth_url, &b_session, b_char_id).await;

    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    let mut b = WorldClient::start(b_token, &b_session, b_char_id).await;
    pump_both_for(&mut a, &mut b, Duration::from_millis(300)).await;

    b.send_dev_spawn("Far Dummy", 1, 30.0, 0, 0.0, 0.0);
    let spawn_evt = wait_on_a_pumping_b(&mut b, &mut a, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::EnemySpawn { mob_name, .. } if mob_name == "Far Dummy")
    })
    .await
    .expect("the dummy spawns beside B");
    let enemy_id: u64 = match spawn_evt {
        ServerWorldMsg::EnemySpawn { id, .. } => id,
        _ => unreachable!(),
    };

    a.send_cast_start("Charm", 2.0);
    pump_both_for(&mut a, &mut b, Duration::from_millis(2150)).await;
    a.send_cast_spell("Charm", Some(enemy_id));
    wait_on_a_pumping_b(&mut a, &mut b, Duration::from_secs(5), |m| {
        matches!(m, ServerWorldMsg::ChatMessage { text, .. } if text.contains("too far away"))
    })
    .await
    .expect("an out-of-range charm is refused with the line");

    // Nothing was charmed: no pet for A, and B never sees its dummy despawn.
    let pet = wait_on_a_pumping_b(&mut a, &mut b, Duration::from_millis(600), |m| {
        matches!(m, ServerWorldMsg::PetSpawn { owner, .. } if *owner == a_char_id as u64)
    })
    .await;
    assert!(pet.is_none(), "nothing is charmed from out of range");
    let gone = wait_on_a_pumping_b(&mut b, &mut a, Duration::from_millis(300), |m| {
        matches!(m, ServerWorldMsg::EntityDespawn { id } if *id == enemy_id)
    })
    .await;
    assert!(gone.is_none(), "the dummy is still standing where B conjured it");
}

/// The case that matters in play: charming a spawner-owned camp mob that is
/// aggroed and mid-fight. The dummy test above proves the conversion without
/// timing risk; this one keeps a live target, a threat table and a running
/// swing timer under the charm. The skeleton fights B, so nothing swings at
/// the caster and no interrupt can roll: the old single-client shape raced
/// the mob's own swing timer.
#[tokio::test]
async fn charm_converts_a_camp_mob_that_is_fighting_someone_else() {
    let h = start_both().await;
    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "campench", "Campench", "Human", "Enchanter").await;
    set_char_level(&h.db_url, a_char_id, 20).await;
    let (b_session, b_char_id, b_token) =
        provision_client(&h.auth_url, "camptank", "Camptank", "Human", "Warrior").await;

    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    let mut b = WorldClient::start(b_token, &b_session, b_char_id).await;
    pump_both_for(&mut a, &mut b, Duration::from_millis(300)).await;

    // B walks 18 m toward the Bonepile's isolated [-16, -14] spawn (21 m from
    // the starter spawn, aggro 8) and stops: inside its aggro circle, and the
    // fight then happens under 20 m from A, who stays at spawn, outside that
    // circle and inside charm range (25 m).
    let len: f32 = (16.0_f32 * 16.0 + 14.0_f32 * 14.0).sqrt();
    let dir = Vec3 { x: -16.0 / len, y: 0.0, z: -14.0 / len };
    let mut seq: u32 = 1;
    walk_pumping(&mut b, &mut a, &mut seq, dir, 48).await;
    b.send_move(seq, Vec3 { x: 0.0, y: 0.0, z: 0.0 });

    let hit_evt = wait_on_a_pumping_b(&mut b, &mut a, Duration::from_secs(35), |m| {
        matches!(m, ServerWorldMsg::Hit { target, .. } if *target == b_char_id as u64)
    })
    .await
    .expect("the skeleton engages B");
    let enemy_id: u64 = match hit_evt {
        ServerWorldMsg::Hit { attacker, .. } => attacker,
        _ => unreachable!(),
    };

    a.send_cast_start("Charm", 2.0);
    pump_both_for(&mut a, &mut b, Duration::from_millis(2150)).await;
    a.send_cast_spell("Charm", Some(enemy_id));

    wait_on_a_pumping_b(&mut a, &mut b, Duration::from_secs(5), |m| {
        matches!(m, ServerWorldMsg::EntityDespawn { id } if *id == enemy_id)
    })
    .await
    .expect("the fighting mob's enemy id despawns on charm");
    let pet_spawn = wait_on_a_pumping_b(&mut a, &mut b, Duration::from_secs(5), |m| {
        matches!(m, ServerWorldMsg::PetSpawn { owner, .. } if *owner == a_char_id as u64)
    })
    .await
    .expect("it comes back as the caster's pet");
    if let ServerWorldMsg::PetSpawn { id, pet_name, .. } = pet_spawn {
        assert!(id >= PET_ID_BASE, "charmed pet id must be in the pet partition");
        assert_eq!(pet_name, "Decrepit Skeleton");
    }
}
