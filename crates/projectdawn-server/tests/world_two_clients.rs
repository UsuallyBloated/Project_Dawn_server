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
use protocol::world::{ClientWorldMsg, ServerWorldMsg, Vec3};
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
        this
    }

    fn send_move(&mut self, sequence: u32, direction: Vec3) {
        let msg = ClientWorldMsg::Move { sequence, direction, jumping: false };
        send_msg(&mut self.client, CHANNEL_POSITION, &msg);
    }

    fn send_disconnect(&mut self) {
        send_msg(&mut self.client, CHANNEL_SYSTEM, &ClientWorldMsg::Disconnect);
    }

    fn send_resource_update(
        &mut self,
        hp: f32,
        max_hp: f32,
        mp: f32,
        max_mp: f32,
        stamina: f32,
        max_stamina: f32,
    ) {
        let msg = ClientWorldMsg::ResourceUpdate {
            hp,
            max_hp,
            mp,
            max_mp,
            stamina,
            max_stamina,
        };
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

/// Track 4 sub-task 1: resource bar replication. When the owning client
/// broadcasts a `ResourceUpdate`, the server fans out three separate
/// ServerWorldMsg variants (HealthUpdate / ManaUpdate / StaminaUpdate) to
/// every other ready peer. The owner does NOT receive its own broadcast
/// (it's the authority — it already has the values).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_clients_resource_fanout() {
    let h = start_both().await;

    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "gamma", "Gam", "Human", "Warrior").await;
    let (b_session, b_char_id, b_token) =
        provision_client(&h.auth_url, "delta", "Del", "Elf", "Cleric").await;

    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    let mut b = WorldClient::start(b_token, &b_session, b_char_id).await;

    // Drain any pending spawn / position broadcasts that landed before our
    // first ResourceUpdate so the wait_for below doesn't latch onto stale
    // pre-update state.
    let _ = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_millis(200), |m| {
            matches!(m, ServerWorldMsg::EntitySpawn { id, .. } if *id == b_char_id as u64)
        })
        .await;

    // A broadcasts resources. B should see all three variants for A.
    a.send_resource_update(73.0, 100.0, 42.0, 80.0, 55.0, 100.0);

    // Pump A so the queued ResourceUpdate actually reaches the wire.
    // `wait_for` on B doesn't tick A; without this nudge the bytes sit
    // in A's outgoing buffer forever.
    for _ in 0..4 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }

    let h_at_b = b
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::HealthUpdate { id, .. } if *id == a_char_id as u64)
        })
        .await
        .expect("B receives HealthUpdate for A");
    if let ServerWorldMsg::HealthUpdate { hp, max_hp, .. } = h_at_b {
        assert!((hp - 73.0).abs() < 0.01, "hp roundtrip: {hp}");
        assert!((max_hp - 100.0).abs() < 0.01, "max_hp roundtrip: {max_hp}");
    }

    let m_at_b = b
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::ManaUpdate { id, .. } if *id == a_char_id as u64)
        })
        .await
        .expect("B receives ManaUpdate for A");
    if let ServerWorldMsg::ManaUpdate { mp, max_mp, .. } = m_at_b {
        assert!((mp - 42.0).abs() < 0.01, "mp roundtrip: {mp}");
        assert!((max_mp - 80.0).abs() < 0.01, "max_mp roundtrip: {max_mp}");
    }

    let s_at_b = b
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::StaminaUpdate { id, .. } if *id == a_char_id as u64)
        })
        .await
        .expect("B receives StaminaUpdate for A");
    if let ServerWorldMsg::StaminaUpdate { stamina, max, .. } = s_at_b {
        assert!((stamina - 55.0).abs() < 0.01, "stamina roundtrip: {stamina}");
        assert!((max - 100.0).abs() < 0.01, "max stamina roundtrip: {max}");
    }

    // Owner should NOT receive its own HealthUpdate back.
    let echoed = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_millis(500), |m| {
            matches!(m, ServerWorldMsg::HealthUpdate { id, .. } if *id == a_char_id as u64)
        })
        .await;
    assert!(echoed.is_none(), "A should not receive own HealthUpdate echo");
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
