//! End-to-end smoke test for the auth WebSocket service.
//!
//! Spins up the auth server on an ephemeral port against a fresh
//! per-test SQLite file, then drives a tokio-tungstenite client through
//! the full Register → Login → CharCreate → CharList → CharDelete
//! → Logout flow. No external test client (wscat/websocat) required.

use futures_util::{SinkExt, StreamExt};
use projectdawn_server::auth::AuthPolicy;
use projectdawn_server::{auth, db, Config};
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

async fn start_server() -> (String, TempDir) {
    let (ws_url, _db_url, tmp) = start_server_with(AuthPolicy::default()).await;
    (ws_url, tmp)
}

/// Start the auth server under an explicit policy; also returns the database
/// URL so a test can reach the same file (to mint invites, for instance).
async fn start_server_with(policy: AuthPolicy) -> (String, String, TempDir) {
    let tmp = TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("auth_test.db");
    let url = format!("sqlite://{}?mode=rwc", db_path.display());

    let cfg = Config {
        auth_bind: "127.0.0.1:0".into(),
        world_bind: "127.0.0.1:0".into(),
        world_endpoint: "127.0.0.1:7777".into(),
        database_url: url.clone(),
        min_client_version: projectdawn_server::config::semver::Version {
            major: 0,
            minor: 1,
            patch: 0,
        },
        // Fixed bytes — auth tests never actually mint a token, so the
        // value doesn't matter; we just need *some* 32-byte array.
        netcode_private_key: [0xA5; 32],
    };

    let pool = db::open(&url).await.expect("open pool");
    db::migrate(&pool).await.expect("migrate");

    let cfg = Arc::new(cfg);
    let (addr, _handle) = auth::serve_bound_with_policy(cfg, pool, policy)
        .await
        .expect("bind");
    (format!("ws://{addr}"), url, tmp)
}

/// A policy with short timeouts and tight caps, so the limit tests finish in
/// well under a second each.
fn tight_policy() -> AuthPolicy {
    AuthPolicy {
        require_invite: false,
        handshake_timeout: Duration::from_millis(300),
        idle_timeout: Duration::from_millis(300),
        max_message_bytes: 4 * 1024,
        max_conns_per_ip: 2,
    }
}

type Ws = tokio_tungstenite::WebSocketStream<
    tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
>;

/// The next frame, or `None` once the server has closed the connection
/// (a Close frame, an EOF, or a reset all count as closed).
async fn next_or_closed(ws: &mut Ws) -> Option<Message> {
    match tokio::time::timeout(Duration::from_secs(3), ws.next()).await {
        Ok(Some(Ok(Message::Close(_)))) | Ok(None) | Ok(Some(Err(_))) => None,
        Ok(Some(Ok(m))) => Some(m),
        Err(_) => panic!("the server neither answered nor closed within 3 s"),
    }
}

// ── The public-port hardening (public_hosting_plan.md, slice 1) ───────────

/// A message over the cap never reaches the JSON parser: tungstenite refuses
/// it and the server drops the connection, instead of buffering up to its
/// 64 MB default per client.
#[tokio::test]
async fn an_oversize_message_closes_the_connection() {
    let (url, _db, _tmp) = start_server_with(tight_policy()).await;
    let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.expect("connect");
    // Well-formed JSON so only the SIZE can be the reason it is refused.
    let big = serde_json::json!({
        "type": "Register",
        "username": "x".repeat(8 * 1024),
        "password": "hunter2!",
    });
    let _ = ws.send(Message::Text(big.to_string())).await;
    assert!(
        next_or_closed(&mut ws).await.is_none(),
        "the server must close rather than answer an oversize message"
    );
    // And the server is fine afterwards.
    let (mut ws2, _) = tokio_tungstenite::connect_async(&url).await.expect("reconnect");
    let resp = rpc(&mut ws2, serde_json::json!({
        "type": "Register", "username": "fine", "password": "hunter2!",
    })).await;
    assert_eq!(resp["type"], "RegisterOk", "{resp}");
}

/// A TCP connection that never completes the WebSocket handshake is dropped
/// at the handshake timeout, so a half-open socket cannot hold a task.
#[tokio::test]
async fn a_client_that_never_finishes_the_handshake_is_dropped() {
    let (url, _db, _tmp) = start_server_with(tight_policy()).await;
    let addr = url.trim_start_matches("ws://").to_string();
    let mut raw = tokio::net::TcpStream::connect(&addr).await.expect("tcp connect");
    // Send nothing. The server must hang up on its own.
    let mut buf = [0u8; 64];
    let read = tokio::time::timeout(Duration::from_secs(3), tokio::io::AsyncReadExt::read(&mut raw, &mut buf))
        .await
        .expect("the server closed the half-open connection within 3 s");
    assert!(matches!(read, Ok(0) | Err(_)), "EOF or reset, not data: {read:?}");
}

/// A connection that stops talking is closed at the idle timeout; the
/// launcher reconnects on its next request, so this costs an honest client
/// nothing.
#[tokio::test]
async fn an_idle_connection_is_closed() {
    let (url, _db, _tmp) = start_server_with(tight_policy()).await;
    let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.expect("connect");
    assert!(next_or_closed(&mut ws).await.is_none(), "closed after the idle timeout");
}

/// One address may hold only so many connections at once; the next is
/// closed with a policy code, and a slot frees when a connection ends.
#[tokio::test]
async fn connections_per_address_are_capped() {
    let (url, _db, _tmp) = start_server_with(AuthPolicy {
        idle_timeout: Duration::from_secs(30),
        ..tight_policy()
    })
    .await;
    let (mut a, _) = tokio_tungstenite::connect_async(&url).await.expect("first");
    let (mut b, _) = tokio_tungstenite::connect_async(&url).await.expect("second");
    let (mut c, _) = tokio_tungstenite::connect_async(&url).await.expect("third");
    assert!(next_or_closed(&mut c).await.is_none(), "the third connection is refused");
    // The first two still work.
    let resp = rpc(&mut a, serde_json::json!({"type": "CharList", "session_token": "00"})).await;
    assert_eq!(resp["type"], "Error");
    // Closing one frees a slot for a new one (the slot drops when the server's
    // task ends, so poll rather than guess the moment).
    a.close(None).await.expect("close a");
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    let mut admitted = false;
    while std::time::Instant::now() < deadline && !admitted {
        let (mut d, _) = tokio_tungstenite::connect_async(&url).await.expect("fourth");
        // A refused connection closes without answering; an admitted one answers.
        let _ = d
            .send(Message::Text(
                serde_json::json!({"type": "CharList", "session_token": "00"}).to_string(),
            ))
            .await;
        admitted = next_or_closed(&mut d).await.is_some();
        if !admitted {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    assert!(admitted, "a freed slot admits a new connection");
    let _ = b.close(None).await;
}

/// Behind the TLS proxy every TCP peer is loopback and the real address rides
/// `X-Forwarded-For`; the limiter must key on THAT, or the whole internet
/// shares one budget. (That the header is ignored from a non-loopback peer is
/// unit-tested on `client_ip_for`; a test cannot connect from elsewhere.)
#[tokio::test]
async fn the_forwarded_address_is_what_the_limiter_keys_on() {
    let (url, _db, _tmp) = start_server_with(AuthPolicy::default()).await;
    async fn connect_as(url: &str, forwarded: &str) -> Ws {
        let mut req = url.into_client_request().expect("request");
        req.headers_mut()
            .insert("x-forwarded-for", forwarded.parse().expect("header"));
        tokio_tungstenite::connect_async(req).await.expect("connect").0
    }
    let bad_login = serde_json::json!({
        "type": "Login", "username": "nobody", "password": "wrong", "client_version": "0.1.0",
    });
    let mut a = connect_as(&url, "198.51.100.9").await;
    let mut hit_limit = false;
    for _ in 0..8 {
        let resp = rpc(&mut a, bad_login.clone()).await;
        assert_eq!(resp["type"], "Error");
        if resp["msg"].as_str().unwrap_or("").contains("Too many") {
            hit_limit = true;
            break;
        }
    }
    assert!(hit_limit, "the forwarded address runs out of budget");
    // A different forwarded address, same loopback peer: its own budget.
    let mut b = connect_as(&url, "198.51.100.10").await;
    let resp = rpc(&mut b, bad_login).await;
    assert_eq!(resp["code"], "auth_failed", "a fresh address is not throttled: {resp}");
}

/// With `require_invite` on, Register needs a code the operator minted, and
/// each code registers exactly one account.
#[tokio::test]
async fn registration_needs_a_valid_unused_invite_when_required() {
    let (url, db_url, _tmp) = start_server_with(AuthPolicy {
        require_invite: true,
        ..AuthPolicy::default()
    })
    .await;
    let pool = db::open(&db_url).await.expect("open");
    let codes = db::create_invites(&pool, 1, Some("test")).await.expect("mint");
    let code = codes[0].clone();
    let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.expect("connect");

    let resp = rpc(&mut ws, serde_json::json!({
        "type": "Register", "username": "guest", "password": "hunter2!",
    })).await;
    assert_eq!(resp["code"], "invite_invalid", "no code: {resp}");
    assert!(resp["msg"].as_str().unwrap().contains("required"));

    let resp = rpc(&mut ws, serde_json::json!({
        "type": "Register", "username": "guest", "password": "hunter2!",
        "invite": "ffffffffffffffffffffffffffffffff",
    })).await;
    assert_eq!(resp["code"], "invite_invalid", "unknown code: {resp}");

    let resp = rpc(&mut ws, serde_json::json!({
        "type": "Register", "username": "guest", "password": "hunter2!",
        "invite": code,
    })).await;
    assert_eq!(resp["type"], "RegisterOk", "the minted code works: {resp}");

    let resp = rpc(&mut ws, serde_json::json!({
        "type": "Register", "username": "guest2", "password": "hunter2!",
        "invite": code,
    })).await;
    assert_eq!(resp["code"], "invite_invalid", "a spent code is refused: {resp}");

    // The account works like any other.
    let resp = rpc(&mut ws, serde_json::json!({
        "type": "Login", "username": "guest", "password": "hunter2!", "client_version": "0.1.0",
    })).await;
    assert_eq!(resp["type"], "LoginOk", "{resp}");
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

#[tokio::test]
async fn full_auth_flow() {
    let (url, _tmp) = start_server().await;
    let (mut ws, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("connect");

    // Register
    let resp = rpc(
        &mut ws,
        serde_json::json!({
            "type": "Register",
            "username": "tester",
            "password": "hunter2!",
            "email": null,
        }),
    )
    .await;
    assert_eq!(resp["type"], "RegisterOk", "register: {resp}");

    // Login
    let resp = rpc(
        &mut ws,
        serde_json::json!({
            "type": "Login",
            "username": "tester",
            "password": "hunter2!",
            "client_version": "0.1.0",
        }),
    )
    .await;
    assert_eq!(resp["type"], "LoginOk", "login: {resp}");
    let token = resp["session_token"].as_str().unwrap().to_string();

    // CharCreate
    let resp = rpc(
        &mut ws,
        serde_json::json!({
            "type": "CharCreate",
            "session_token": token,
            "name": "Chortle",
            "race": "Troll",
            "class": "Shadow Knight",
        }),
    )
    .await;
    assert_eq!(resp["type"], "CharCreated", "charcreate: {resp}");
    let char_id = resp["char_id"].as_i64().unwrap();

    // CharList
    let resp = rpc(
        &mut ws,
        serde_json::json!({ "type": "CharList", "session_token": token }),
    )
    .await;
    assert_eq!(resp["type"], "CharList");
    let chars = resp["characters"].as_array().unwrap();
    assert_eq!(chars.len(), 1);
    assert_eq!(chars[0]["name"], "Chortle");

    // CharDelete
    let resp = rpc(
        &mut ws,
        serde_json::json!({
            "type": "CharDelete",
            "session_token": token,
            "char_id": char_id,
        }),
    )
    .await;
    assert_eq!(resp["type"], "CharDeleted", "chardelete: {resp}");

    // Logout
    let resp = rpc(
        &mut ws,
        serde_json::json!({ "type": "Logout", "session_token": token }),
    )
    .await;
    assert_eq!(resp["type"], "LogoutOk", "logout: {resp}");
}

#[tokio::test]
async fn rejects_old_client_version() {
    let (url, _tmp) = start_server().await;
    let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.expect("connect");

    rpc(
        &mut ws,
        serde_json::json!({
            "type": "Register",
            "username": "alice",
            "password": "hunter2!",
        }),
    )
    .await;

    let resp = rpc(
        &mut ws,
        serde_json::json!({
            "type": "Login",
            "username": "alice",
            "password": "hunter2!",
            "client_version": "0.0.9",
        }),
    )
    .await;
    assert_eq!(resp["type"], "Error");
    assert_eq!(resp["code"], "version_mismatch");
}

#[tokio::test]
async fn rejects_duplicate_username() {
    let (url, _tmp) = start_server().await;
    let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.expect("connect");

    rpc(
        &mut ws,
        serde_json::json!({
            "type": "Register",
            "username": "bob",
            "password": "hunter2!",
        }),
    )
    .await;

    let resp = rpc(
        &mut ws,
        serde_json::json!({
            "type": "Register",
            "username": "BOB",
            "password": "differentpw",
        }),
    )
    .await;
    assert_eq!(resp["type"], "Error");
    assert_eq!(resp["code"], "name_taken");
}

/// A modified launcher sending a race or class the creation screen never
/// offers (the Revenant has a stat block, for the transformation) is refused
/// at creation; the sixteen races and eighteen classes still work.
#[tokio::test]
async fn character_creation_accepts_only_the_creation_lists() {
    let (url, _tmp) = start_server().await;
    let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.expect("connect");
    let resp = rpc(&mut ws, serde_json::json!({
        "type": "Register", "username": "picker", "password": "hunter2!",
    })).await;
    assert_eq!(resp["type"], "RegisterOk", "{resp}");
    let resp = rpc(&mut ws, serde_json::json!({
        "type": "Login", "username": "picker", "password": "hunter2!", "client_version": "0.1.0",
    })).await;
    let session = resp["session_token"].as_str().expect("session").to_string();
    let resp = rpc(&mut ws, serde_json::json!({
        "type": "CharCreate", "session_token": session, "name": "Revvy", "race": "Revenant", "class": "Warrior",
    })).await;
    assert_eq!(resp["code"], "invalid_input", "a transformation is not a birth race: {resp}");
    let resp = rpc(&mut ws, serde_json::json!({
        "type": "CharCreate", "session_token": session, "name": "Wardy", "race": "Human", "class": "Warlock",
    })).await;
    assert_eq!(resp["code"], "invalid_input", "a planned class is not playable yet: {resp}");
    let resp = rpc(&mut ws, serde_json::json!({
        "type": "CharCreate", "session_token": session, "name": "Halfy", "race": "Halfling", "class": "Rogue",
    })).await;
    assert_eq!(resp["type"], "CharCreated", "a real pair creates: {resp}");
}
