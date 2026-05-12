//! End-to-end smoke test for the world UDP service.
//!
//! Drives the full cross-service handshake:
//!   Auth WS:  Register → Login → CharCreate → RequestWorldToken
//!   World UDP: ConnectToken → ConnectOk → Move → Position → Disconnect
//!
//! Both services run in-process on ephemeral ports against a per-test
//! SQLite file. No external test client; uses `renet` + `renet_netcode`
//! 2.0 directly to drive the wire as the real game client will.

use bincode::config::standard as bincode_cfg;
use futures_util::{SinkExt, StreamExt};
use projectdawn_server::{auth, db, world, Config};
use protocol::world::{
    ClientWorldMsg, ServerWorldMsg, Vec3, WORLD_PROTOCOL_ID,
};
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
    world_addr: SocketAddr,
    _tmp: TempDir,
}

async fn start_both() -> Harness {
    let tmp = TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("world_test.db");
    let url = format!("sqlite://{}?mode=rwc", db_path.display());

    // Per-test random key. The test alone signs and validates these.
    let mut netcode_key = [0u8; 32];
    use rand::RngCore;
    rand::rngs::OsRng.fill_bytes(&mut netcode_key);

    // Bind world UDP first so we know its port before building Config.
    let world_socket = world::bind_world_socket("127.0.0.1:0").expect("bind world");
    let world_addr = world_socket.local_addr().expect("world local_addr");

    let cfg = Config {
        auth_bind: "127.0.0.1:0".into(),
        world_bind: "127.0.0.1:0".into(), // unused — we hand in the prebound socket
        world_endpoint: world_addr.to_string(),
        database_url: url.clone(),
        min_client_version: projectdawn_server::config::semver::Version {
            major: 0,
            minor: 1,
            patch: 0,
        },
        netcode_private_key: netcode_key,
    };
    let cfg = Arc::new(cfg);

    let pool = db::open(&url).await.expect("open pool");
    db::migrate(&pool).await.expect("migrate");

    // World tick task — fire-and-forget for the test's lifetime.
    let _world_handle = world::serve_with_socket(cfg.clone(), pool.clone(), world_socket)
        .await
        .expect("world serve_with_socket");

    let (auth_addr, _auth_handle) =
        auth::serve_bound(cfg, pool).await.expect("auth bind");

    Harness {
        auth_url: format!("ws://{auth_addr}"),
        world_addr,
        _tmp: tmp,
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

/// Drives the renet client one tick: update + send. Used in spin-loops below.
fn tick_client(client: &mut RenetClient, transport: &mut NetcodeClientTransport) {
    client.update(TICK_DT);
    if let Err(e) = transport.update(TICK_DT, client) {
        eprintln!("client transport update: {e}");
    }
    if let Err(e) = transport.send_packets(client) {
        eprintln!("client transport send: {e}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn world_connect_move_position_disconnect() {
    let h = start_both().await;

    // ─── Auth phase ─────────────────────────────────────────────────────
    let (mut ws, _) = tokio_tungstenite::connect_async(&h.auth_url)
        .await
        .expect("connect auth ws");

    let resp = rpc(
        &mut ws,
        serde_json::json!({
            "type": "Register",
            "username": "worldtest",
            "password": "hunter2!",
        }),
    )
    .await;
    assert_eq!(resp["type"], "RegisterOk", "register: {resp}");

    let resp = rpc(
        &mut ws,
        serde_json::json!({
            "type": "Login",
            "username": "worldtest",
            "password": "hunter2!",
            "client_version": "0.1.0",
        }),
    )
    .await;
    assert_eq!(resp["type"], "LoginOk", "login: {resp}");
    let session_token = resp["session_token"].as_str().unwrap().to_string();
    assert_eq!(
        resp["world_endpoint"].as_str().unwrap(),
        h.world_addr.to_string(),
        "advertised world_endpoint matches bound addr"
    );

    let resp = rpc(
        &mut ws,
        serde_json::json!({
            "type": "CharCreate",
            "session_token": session_token,
            "name": "Smoketester",
            "race": "Human",
            "class": "Warrior",
        }),
    )
    .await;
    assert_eq!(resp["type"], "CharCreated", "charcreate: {resp}");
    let char_id = resp["char_id"].as_i64().unwrap();

    let resp = rpc(
        &mut ws,
        serde_json::json!({
            "type": "RequestWorldToken",
            "session_token": session_token,
            "char_id": char_id,
        }),
    )
    .await;
    assert_eq!(resp["type"], "WorldConnectToken", "request token: {resp}");
    let token_b64 = resp["token_bytes"]
        .as_array()
        .expect("token_bytes is array");
    let token_bytes: Vec<u8> = token_b64
        .iter()
        .map(|v| v.as_u64().expect("byte") as u8)
        .collect();

    // ─── World phase ────────────────────────────────────────────────────
    // Decode the token bytes and build a Secure-mode client.
    let connect_token = ConnectToken::read(&mut Cursor::new(&token_bytes[..]))
        .expect("ConnectToken::read");
    assert_eq!(
        connect_token.client_id, char_id as u64,
        "minted token client_id == char_id"
    );

    let client_socket = UdpSocket::bind("127.0.0.1:0").expect("client udp bind");
    client_socket
        .set_nonblocking(true)
        .expect("client nonblocking");

    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    let auth = ClientAuthentication::Secure { connect_token };

    let mut transport =
        NetcodeClientTransport::new(now, auth, client_socket).expect("client transport");
    let mut client = RenetClient::new(connection_config_matching_server());

    // Spin until renet handshake completes.
    let connect_deadline = Instant::now() + Duration::from_secs(5);
    while !client.is_connected() {
        tick_client(&mut client, &mut transport);
        if Instant::now() > connect_deadline {
            panic!("renet handshake did not complete within 5 s");
        }
        tokio::time::sleep(TICK_DT).await;
    }

    // App-layer Connect message.
    let connect_msg = ClientWorldMsg::Connect {
        session_token: decode_token_bytes(&session_token),
        char_id: char_id as u64,
        client_version: "0.1.0".into(),
    };
    send_client_msg(&mut client, CHANNEL_SYSTEM, &connect_msg);
    tick_client(&mut client, &mut transport);

    // Wait for ConnectOk on the system channel.
    let connect_ok = wait_for_msg(
        &mut client,
        &mut transport,
        CHANNEL_SYSTEM,
        Duration::from_secs(3),
        |m| matches!(m, ServerWorldMsg::ConnectOk { .. }),
    )
    .await
    .expect("ConnectOk arrived");
    let player_id = match connect_ok {
        ServerWorldMsg::ConnectOk { player_id, .. } => player_id,
        _ => unreachable!(),
    };
    assert_eq!(
        player_id, char_id as u64,
        "ConnectOk.player_id == requested char_id"
    );

    // Track 4 follow-up E: Position fan-out is gated on EnterWorld. The
    // real client sends this after the lobby's Enter World button; tests
    // do it immediately after ConnectOk.
    send_client_msg(&mut client, CHANNEL_SYSTEM, &ClientWorldMsg::EnterWorld);

    // Send a Move intent and wait for the resulting Position broadcast.
    // Direction (1, 0, 0) at MAX_MOVE_SPEED * TICK_DT should produce
    // pos.x ≈ 7.5 * 0.05 = 0.375 m on the very next tick — but the server
    // may also broadcast positions before our move is applied, so we just
    // assert "we got a Position with our id and pos.x > 0".
    let move_msg = ClientWorldMsg::Move {
        sequence: 1,
        direction: Vec3 { x: 1.0, y: 0.0, z: 0.0 },
        jumping: false,
    };
    send_client_msg(&mut client, CHANNEL_POSITION, &move_msg);

    let position = wait_for_msg(
        &mut client,
        &mut transport,
        CHANNEL_POSITION,
        Duration::from_secs(3),
        |m| matches!(m, ServerWorldMsg::Position { id, pos, .. } if *id == char_id as u64 && pos.x > 0.0),
    )
    .await
    .expect("moved Position arrived");

    if let ServerWorldMsg::Position { pos, .. } = position {
        assert!(
            pos.x > 0.0 && pos.x < 5.0,
            "position drift within speed cap: x={}",
            pos.x
        );
    }

    // Clean app-layer disconnect.
    send_client_msg(&mut client, CHANNEL_SYSTEM, &ClientWorldMsg::Disconnect);
    // Pump a few ticks so the message lands. Stop on first error: once
    // the server processes Disconnect it tears down the renet connection
    // and further send_packets calls will (correctly) error.
    for _ in 0..10 {
        client.update(TICK_DT);
        if transport.update(TICK_DT, &mut client).is_err() {
            break;
        }
        if transport.send_packets(&mut client).is_err() {
            break;
        }
        tokio::time::sleep(TICK_DT).await;
    }
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

fn send_client_msg(client: &mut RenetClient, channel: u8, msg: &ClientWorldMsg) {
    let bytes = bincode::serde::encode_to_vec(msg, bincode_cfg()).expect("encode");
    client.send_message(channel, bytes);
}

async fn wait_for_msg(
    client: &mut RenetClient,
    transport: &mut NetcodeClientTransport,
    channel: u8,
    timeout: Duration,
    pred: impl Fn(&ServerWorldMsg) -> bool,
) -> Option<ServerWorldMsg> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        tick_client(client, transport);
        while let Some(bytes) = client.receive_message(channel) {
            if let Ok((msg, _)) =
                bincode::serde::decode_from_slice::<ServerWorldMsg, _>(&bytes, bincode_cfg())
            {
                if pred(&msg) {
                    return Some(msg);
                }
            }
        }
        tokio::time::sleep(TICK_DT).await;
    }
    None
}

fn decode_token_bytes(hex_str: &str) -> [u8; 32] {
    let v = hex::decode(hex_str).expect("hex decode session_token");
    v.try_into().expect("32 bytes")
}

// Silence unused — exposed in protocol but slice 1 doesn't read it directly.
#[allow(dead_code)]
const _PROTOCOL_ID_USED: u64 = WORLD_PROTOCOL_ID;
