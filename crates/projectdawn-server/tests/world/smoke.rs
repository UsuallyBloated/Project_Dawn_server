//! End-to-end smoke test for the world UDP service, driven with a bare renet client
//! instead of WorldClient: Register, Login, CharCreate and RequestWorldToken over the
//! auth WebSocket, then ConnectToken, ConnectOk, Move, Position and Disconnect over
//! UDP. (World_smoke.rs, its own test binary with a copy of the harness, until 2026-11-01.)

use crate::common::*;

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
    let connect_token =
        ConnectToken::read(&mut Cursor::new(&token_bytes[..])).expect("ConnectToken::read");
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
        tick_one(&mut client, &mut transport);
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
    send_msg(&mut client, CHANNEL_SYSTEM, &connect_msg);
    tick_one(&mut client, &mut transport);

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
    send_msg(&mut client, CHANNEL_SYSTEM, &ClientWorldMsg::EnterWorld);

    // Send a Move intent and wait for the resulting Position broadcast.
    // Direction (1, 0, 0) at RUN_SPEED * TICK_DT should produce
    // pos.x = 5 * 0.05 = 0.25 m on the very next tick — but the server
    // may also broadcast positions before our move is applied, so we just
    // assert "we got a Position with our id and pos.x > 0".
    let move_msg = ClientWorldMsg::Move {
        sequence: 1,
        direction: Vec3 {
            x: 1.0,
            y: 0.0,
            z: 0.0,
        },
        jumping: false,
        yaw: 0.0,
    };
    send_msg(&mut client, CHANNEL_POSITION, &move_msg);

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
    send_msg(&mut client, CHANNEL_SYSTEM, &ClientWorldMsg::Disconnect);
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

/// Ticks the bare client until a message on `channel` matches `pred`, or the timeout.
async fn wait_for_msg(
    client: &mut RenetClient,
    transport: &mut NetcodeClientTransport,
    channel: u8,
    timeout: Duration,
    pred: impl Fn(&ServerWorldMsg) -> bool,
) -> Option<ServerWorldMsg> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        tick_one(client, transport);
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
