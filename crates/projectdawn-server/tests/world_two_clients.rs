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
use protocol::world::{ClientWorldMsg, DamageType, ServerWorldMsg, Vec3, ENEMY_ID_BASE, PET_ID_BASE};
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
        // so the message actually goes out before start() returns —
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

    fn send_heartbeat(&mut self) {
        send_msg(&mut self.client, CHANNEL_SYSTEM, &ClientWorldMsg::Heartbeat);
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

    fn send_pet_command(&mut self, command: u8, target_id: Option<u64>) {
        let msg = ClientWorldMsg::PetCommand { command, target_id };
        send_msg(&mut self.client, CHANNEL_SYSTEM, &msg);
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

    // ── 1. EntitySpawn fan-out ──
    // B was newly-connected; the server should have sent A an EntitySpawn for B.
    // A also gets EntitySpawn for itself? No — fan-out skips the subject;
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

    // ── 2. Position fan-out ──
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

    // ── 3. EntityDespawn on disconnect ──
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

    // Track 6 sub-task 2 — A is Human Warrior, so char_data::compute
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
    let (b_session, b_char_id, b_token) =
        provision_client(&h.auth_url, "theta", "The", "Elf", "Druid").await;

    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    let mut b = WorldClient::start(b_token, &b_session, b_char_id).await;

    // A casts Healing Wave (ALLY, 15 immediate heal + 4 hps × 18s HoT,
    // cast_time 1.0s). Track 10 — the cast-time gate now rejects
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
    tokio::time::sleep(Duration::from_millis(1100)).await;
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

/// Track 5 sub-task 1C: AI state machine drives Idle → Chase → Attack
/// for a server-spawned enemy when a player enters aggro range.
///
/// The player walks toward camp 0's first spawn at [20, 0, 5] for ~2 s
/// (covering ~15 m at MAX_MOVE_SPEED = 7.5 m/s, landing well inside
/// the Decrepit Skeleton's 8 m aggro radius even with the ±3 m XZ
/// spawn jitter), then stops. The test asserts:
///
///   * an `EntityTarget` broadcast lands targeting the player (Idle →
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
    let len: f32 = (20.0_f32 * 20.0 + 5.0_f32 * 5.0).sqrt();
    let dir = Vec3 { x: 20.0 / len, y: 0.0, z: 5.0 / len };

    // Walk for ~2 s at 50 ms cadence. Each Move refreshes the server-side
    // stale-move clock so integration continues until we stop sending.
    let walk_end = Instant::now() + Duration::from_millis(2_000);
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
    // world_two_clients.rs file's tests in parallel by default — under
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

/// Track 5 sub-task 3: player → server Attack intent, enemy HP authority,
/// death lifecycle.
///
/// The player walks into camp 0's aggro radius so the AI engages, waits
/// for an enemy-originated Hit broadcast (confirming the player and one
/// enemy are now within 1.2 × melee_range of each other), then sends an
/// `Attack` with enough damage to one-shot the Decrepit Skeleton
/// (25 HP). Asserts:
///
///   * `HealthUpdate { id = enemy_id, hp = 0.0 }` arrives;
///   * `EntityDied { id = enemy_id }` arrives;
///   * `EntityDespawn { id = enemy_id }` arrives after the corpse linger
///     (CORPSE_LINGER_SECS = 5 s server-side).
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

    // Walk toward camp 0's [20, 0, 5] for ~2 s.
    let len: f32 = (20.0_f32 * 20.0 + 5.0_f32 * 5.0).sqrt();
    let dir = Vec3 { x: 20.0 / len, y: 0.0, z: 5.0 / len };
    let walk_end = Instant::now() + Duration::from_millis(2_000);
    let mut seq: u32 = 1;
    while Instant::now() < walk_end {
        a.send_move(seq, dir);
        seq += 1;
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }

    // Wait for an enemy hit on the player — proves the AI walked an
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

    // Track 6 sub-task 2: server runs the damage formula now — the
    // client can't claim 999 anymore. A bare-handed Human Warrior
    // lands ~5-8/swing (1-4 + STR/5 with STR 22). Decrepit Skeleton has
    // 25 HP; ~5 swings cover worst case. Burst 10 to absorb the
    // 50% miss-on-edge if the server-side fan-out drops an Attack on
    // the unreliable boundary, then drain.
    for _ in 0..10 {
        a.send_attack(enemy_id, "", false, DamageType::Physical);
    }
    for _ in 0..6 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
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

    // Track 5 sub-task 5 — kill credit. Decrepit Skeleton's authored
    // xp is 10. Sole attacker → sole top damager → private XpGained.
    let xp_evt = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(5), |m| {
            matches!(m, ServerWorldMsg::XpGained { .. })
        })
        .await
        .expect("XpGained arrives for the kill-credit recipient");
    if let ServerWorldMsg::XpGained { amount, .. } = xp_evt {
        assert_eq!(amount, 10, "Decrepit Skeleton authored xp is 10");
    }

    // CORPSE_LINGER_SECS is 5 s; budget extra for tick jitter and
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

/// Track 7 AOI — far-apart clients do not see each other.
///
/// CELL_SIZE = 120 m; cell boundaries at x = 120, 240, 360 …
/// B is placed at x = 300 (cell (2, 0)) before connecting. A spawns at
/// the origin (cell (0, 0)). |2 − 0| = 2 > 1, so they fall outside
/// each other's 3×3 neighbourhood: neither should receive EntitySpawn
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

/// Track 7 AOI — approaching client triggers mutual EntitySpawn on cell
/// boundary crossing.
///
/// A sits at the origin (cell (0, 0)). B starts at x = 300 (cell (2, 0))
/// and walks in the −X direction at MAX_MOVE_SPEED = 7.5 m/s. After
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

    // Walk B toward A. 300 → 240 = 60 m ÷ 7.5 m/s = 8 s; 12 s covers
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
    .expect("A receives EntitySpawn for B once B enters A's 3×3 neighbourhood");

    b.wait_for(CHANNEL_SYSTEM, Duration::from_secs(5), |m| {
        matches!(m, ServerWorldMsg::EntitySpawn { id, .. } if *id == a_char_id as u64)
    })
    .await
    .expect("B receives EntitySpawn for A when it crosses into A's neighbourhood");
}

/// Track 9 — server-side AOE damage. A Magician walks into camp 0's
/// aggro radius until an enemy chases into melee, then casts Inferno
/// (5 m radius, 45 base damage, FIRE). The server's AOE arm searches
/// the caster's AOI neighbourhood, filters by radius, and fans a Hit
/// per victim. Asserts at least one enemy receives a Fire Hit
/// authored by the caster.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn aoe_spell_damages_nearby_enemies() {
    let h = start_both().await;

    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "ino", "Inora", "Human", "Magician").await;

    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;

    // Walk toward camp 0's [20, 0, 5] for ~2 s — same pattern as
    // player_attack_kills_enemy_and_corpse_despawns.
    let len: f32 = (20.0_f32 * 20.0 + 5.0_f32 * 5.0).sqrt();
    let dir = Vec3 { x: 20.0 / len, y: 0.0, z: 5.0 / len };
    let walk_end = Instant::now() + Duration::from_millis(2_000);
    let mut seq: u32 = 1;
    while Instant::now() < walk_end {
        a.send_move(seq, dir);
        seq += 1;
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }

    // Wait for an enemy hit on us — proves an enemy chased into
    // melee range, which puts it well within Inferno's 5 m radius.
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

    // Cast Inferno. `target_id: None` because AOE doesn't take a
    // single target — the server searches the caster's AOI for
    // anything in radius. Inferno has cast_time 2.5s; Track 10's
    // gate rejects CastSpell without a matching CastStart that ran
    // long enough, so pump CastStart out first (sleeping doesn't
    // advance the transport), wait the cast time, then fire CastSpell.
    a.send_cast_start("Inferno", 2.5);
    for _ in 0..3 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }
    tokio::time::sleep(Duration::from_millis(2600)).await;
    a.send_cast_spell("Inferno", None);
    for _ in 0..6 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }

    // Inferno must fan at least one Hit with attacker=a, target=enemy,
    // dmg_type=Fire. The amount is the authored base_damage (45) —
    // server doesn't apply INT scale yet, matching single-target.
    let aoe_hit = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(5), |m| {
            matches!(
                m,
                ServerWorldMsg::Hit { attacker, target, dmg_type, .. }
                    if *attacker == a_char_id as u64
                    && *target >= ENEMY_ID_BASE
                    && matches!(dmg_type, DamageType::Fire)
            )
        })
        .await
        .expect("Inferno fans a Fire Hit to at least one enemy in range");
    if let ServerWorldMsg::Hit { amount, .. } = aoe_hit {
        assert_eq!(amount, 45, "Inferno authored base_damage is 45");
    }
}

/// Track 10 — cast-time gate rejects a CastSpell that arrives before
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

    // Skip walking — we don't need an enemy in range; the gate fails
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

    // No Hit should fire — gate ran before mana deduction and spell
    // application. (Target id was a stub anyway; we want to confirm
    // no side effects, not target resolution.)
    let stray = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_millis(500), |m| {
            matches!(m, ServerWorldMsg::Hit { attacker, .. } if *attacker == a_char_id as u64)
        })
        .await;
    assert!(stray.is_none(), "rejected cast must not fan a Hit");
}

/// Track 10 — cast-time gate passes once the cast bar has actually
/// run. Send CastStart for Healing Wave (1.0 s cast, ALLY target_type
/// — self-heal when target_id is None), wait the cast time + jitter,
/// then send CastSpell. Assert: a HealthUpdate arrives for the caster
/// (proof the cast went through the helper that fans on heal apply).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cast_spell_accepted_after_cast_time() {
    let h = start_both().await;

    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "nuu", "Nura", "Human", "Cleric").await;

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
    // tokio::time::sleep alone doesn't advance the transport — we'd
    // otherwise be measuring "time until the test got around to
    // ticking" not "time the cast bar ran on the server".
    a.send_cast_start("Healing Wave", 1.0);
    for _ in 0..3 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }
    // Now wait past the cast time (1.0 s) on the server's wall clock.
    // 1100 ms includes a 100 ms cushion above the gate's tolerance.
    tokio::time::sleep(Duration::from_millis(1100)).await;
    a.send_cast_spell("Healing Wave", None);
    for _ in 0..6 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }

    // Healing Wave applies a 4 hps × 18 s HoT; the server fans a
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

/// Track 11 — server-side pet summon. Necromancer A casts Summon
/// Skeleton; the server spawns a player-owned pet entity, fans
/// `PetSpawn` to AOI peers, and B (in the same cell) receives the
/// broadcast carrying A's char_id as owner and a pet id in the
/// reserved partition.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pet_summon_visible_to_peer() {
    let h = start_both().await;

    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "xio", "Xiora", "Human", "Necromancer").await;
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
    // transport — same gotcha as Track 10's gate tests).
    a.send_cast_start("Summon Skeleton", 3.0);
    for _ in 0..3 {
        tick_one(&mut a.client, &mut a.transport);
        tick_one(&mut b.client, &mut b.transport);
        tokio::time::sleep(TICK_DT).await;
    }
    tokio::time::sleep(Duration::from_millis(3100)).await;
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
        assert_eq!(level, 6);
        assert!((max_hp - 80.0).abs() < 0.01, "skeleton template authored hp is 80");
        assert!((hp - 80.0).abs() < 0.01, "fresh pet spawns at full hp");
    }
}

/// Track 11.2 — pet follows its owner across the world. Necromancer
/// summons the skeleton at spawn, then walks away. The peer sees
/// Position updates for the pet id closing the gap to the owner.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pet_follows_owner() {
    let h = start_both().await;

    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "zii", "Ziorel", "Human", "Necromancer").await;
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
    tokio::time::sleep(Duration::from_millis(3100)).await;
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
    // up. Use 3.0 as the threshold — generous to absorb tick jitter
    // and the FOLLOW_DISTANCE hysteresis at the boundary.
    assert!(
        max_pet_x > 3.0,
        "pet should follow owner east (saw max x = {max_pet_x})"
    );
}

/// Track 11.3 — pet inherits owner's attack target and damages the
/// enemy. Walk Necromancer A into camp 0 until an enemy is in melee,
/// summon the skeleton, A swings on the enemy once to seed
/// last_attacked_enemy, then the skeleton's swings produce Hit
/// broadcasts with the pet id as attacker.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pet_attacks_owners_target() {
    let h = start_both().await;

    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "rrr", "Rune", "Human", "Necromancer").await;

    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;

    // Walk toward camp 0's [20, 0, 5] for ~2 s.
    let len: f32 = (20.0_f32 * 20.0 + 5.0_f32 * 5.0).sqrt();
    let dir = Vec3 { x: 20.0 / len, y: 0.0, z: 5.0 / len };
    let walk_end = Instant::now() + Duration::from_millis(2_000);
    let mut seq: u32 = 1;
    while Instant::now() < walk_end {
        a.send_move(seq, dir);
        seq += 1;
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }

    // Wait for an enemy hit on us — proves the AI walked an enemy into
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

    // Summon Skeleton (cast_time 3.0s).
    a.send_cast_start("Summon Skeleton", 3.0);
    for _ in 0..3 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }
    tokio::time::sleep(Duration::from_millis(3100)).await;
    a.send_cast_spell("Summon Skeleton", None);
    for _ in 0..3 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }

    // Latch the pet id from the PetSpawn the server fans to A as
    // well (caster receives own PetSpawn — caller's AOI cell
    // includes themselves).
    let pet_id: u64 = match a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::PetSpawn { owner, .. } if *owner == a_char_id as u64)
        })
        .await
        .expect("A receives own PetSpawn")
    {
        ServerWorldMsg::PetSpawn { id, .. } => id,
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
        assert_eq!(amount, 8, "skeleton template authored dmg is 8");
    }
}

/// Track 12 Piece A — explicit `/pet attack` command locks the pet
/// onto a specific enemy id, bypassing the `last_attacked_enemy`
/// inheritance pipeline. Necromancer summons, then commands the
/// pet to attack an enemy WITHOUT first hitting it themselves;
/// assert a Hit with attacker=pet, target=that enemy arrives.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pet_command_attack_locks_onto_target() {
    let h = start_both().await;

    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "sss", "Suun", "Human", "Necromancer").await;

    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;

    // Walk into camp 0; wait until at least one enemy aggros and
    // hits the player so we have an enemy id to command on.
    let len: f32 = (20.0_f32 * 20.0 + 5.0_f32 * 5.0).sqrt();
    let dir = Vec3 { x: 20.0 / len, y: 0.0, z: 5.0 / len };
    let walk_end = Instant::now() + Duration::from_millis(2_000);
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

    // Summon Skeleton (cast_time 3.0s).
    a.send_cast_start("Summon Skeleton", 3.0);
    for _ in 0..3 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }
    tokio::time::sleep(Duration::from_millis(3100)).await;
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
        .expect("A receives own PetSpawn")
    {
        ServerWorldMsg::PetSpawn { id, .. } => id,
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
        assert_eq!(amount, 8);
    }
}

/// Track 12 Piece A2 — pet pulls aggro via threat re-eval. Walk
/// player into camp, get aggro'd, summon skeleton, command attack;
/// pet's accumulated threat eventually clears the 1.3× current-
/// target multiplier and the enemy re-targets onto the pet,
/// broadcasting an EntityTarget switch.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pet_pulls_aggro_via_threat_reaggro() {
    let h = start_both().await;

    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "tnk", "Tanker", "Human", "Necromancer").await;

    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;

    // Walk into camp 0.
    let len: f32 = (20.0_f32 * 20.0 + 5.0_f32 * 5.0).sqrt();
    let dir = Vec3 { x: 20.0 / len, y: 0.0, z: 5.0 / len };
    let walk_end = Instant::now() + Duration::from_millis(2_000);
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

    // Summon Skeleton and lock it onto the enemy via /pet attack.
    a.send_cast_start("Summon Skeleton", 3.0);
    for _ in 0..3 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }
    tokio::time::sleep(Duration::from_millis(3100)).await;
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
    a.send_pet_command(protocol::world::pet_command::ATTACK, Some(enemy_id));
    for _ in 0..3 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }

    // Skeleton dmg is 8/swing on 2.2 s interval; bare-handed human
    // Necromancer is doing single-digit damage / 2-3 s. After ~3-4
    // pet swings (each adding +8 threat against pet_id) plus the
    // player's accumulated threat, the pet's threat passes 1.3× the
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
    // to surpass it by 1.3×.
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
        .expect("enemy re-targets onto the pet once threat passes 1.3× the player's");
    let _ = switch_evt;
}

/// Track 12 Piece B — Beast Masters auto-summon a Wolf warder when
/// they enter the world. No PET_SUMMON cast required; the server
/// detects the class on first EnterWorld and spawns the warder
/// alongside the EntitySpawn fan-out.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn beast_master_auto_summons_warder() {
    let h = start_both().await;

    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "bms", "Beastly", "Human", "Beast Master").await;

    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;

    let pet_spawn = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(5), |m| {
            matches!(m, ServerWorldMsg::PetSpawn { owner, .. } if *owner == a_char_id as u64)
        })
        .await
        .expect("Beast Master receives an auto-summoned warder on EnterWorld");
    if let ServerWorldMsg::PetSpawn { pet_name, level, max_hp, hp, .. } = pet_spawn {
        assert_eq!(pet_name, "Wolf", "Beast Master's auto-summon is a Wolf warder");
        assert_eq!(level, 5);
        assert!((max_hp - 60.0).abs() < 0.01, "warder template hp is 60");
        assert!((hp - 60.0).abs() < 0.01, "auto-summon spawns at full HP");
    }
}

/// Track 12 Piece B — non-Beast-Master classes do NOT get an
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
