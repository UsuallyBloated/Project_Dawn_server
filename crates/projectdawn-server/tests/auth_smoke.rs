//! End-to-end smoke test for the auth WebSocket service.
//!
//! Spins up the auth server on an ephemeral port against a fresh
//! per-test SQLite file, then drives a tokio-tungstenite client through
//! the full Register → Login → CharCreate → CharList → CharDelete
//! → Logout flow. No external test client (wscat/websocat) required.

use futures_util::{SinkExt, StreamExt};
use projectdawn_server::{auth, db, Config};
use std::sync::Arc;
use tempfile::TempDir;
use tokio_tungstenite::tungstenite::Message;

async fn start_server() -> (String, TempDir) {
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
    let (addr, _handle) = auth::serve_bound(cfg, pool).await.expect("bind");
    (format!("ws://{addr}"), tmp)
}

async fn rpc(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    payload: serde_json::Value,
) -> serde_json::Value {
    ws.send(Message::Text(payload.to_string().into()))
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
