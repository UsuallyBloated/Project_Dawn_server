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
use projectdawn_server::{auth, char_data, db, world, Config};
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
    /// Send `true` to give the world a stop signal (as systemd's SIGTERM does).
    shutdown: tokio::sync::watch::Sender<bool>,
    /// The world task; it ends after a stop signal.
    world: Option<tokio::task::JoinHandle<anyhow::Result<()>>>,
}

/// A server with fizzles OFF: a cast fizzles about one time in ten at par
/// (`cast_gate::fizzle_chance`), which would make every single-cast test here
/// a coin flip. The one test about fizzles starts with `start_both_opts(true)`.
async fn start_both() -> Harness {
    start_both_opts(false).await
}

async fn start_both_opts(fizzles: bool) -> Harness {
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
        netcode_private_key: projectdawn_server::config::NetcodeKey(netcode_key),
        fizzles,
    });

    let pool = db::open(&url).await.expect("open pool");
    db::migrate(&pool).await.expect("migrate");

    let (shutdown, shutdown_rx) = tokio::sync::watch::channel(false);
    let world_handle =
        world::serve_with_socket_until(cfg.clone(), pool.clone(), world_socket, shutdown_rx)
            .await
            .expect("world serve_with_socket_until");
    let (auth_addr, _auth_handle) = auth::serve_bound(cfg, pool).await.expect("auth bind");

    let auth_url = format!("ws://{auth_addr}");
    DB_FOR_AUTH.lock().unwrap().insert(auth_url.clone(), url.clone());
    Harness {
        auth_url,
        db_url: url,
        _world_addr: world_addr,
        _tmp: tmp,
        shutdown,
        world: Some(world_handle),
    }
}

/// Which database each harness's auth server writes, so `provision_client`
/// (which only knows the auth URL) can prepare the character's rows.
static DB_FOR_AUTH: std::sync::Mutex<std::collections::BTreeMap<String, String>> =
    std::sync::Mutex::new(std::collections::BTreeMap::new());

/// Put every spell the class can ever cast into the character's server-held
/// spell bar (one gem each, past the ten a player has: the server trusts its
/// own rows), marked as held. The server refuses a cast of a spell not
/// memorized (client-authority audit fix 5), so without this every casting
/// test would first have to memorize. The spell bar's own tests start from
/// `forget_all_spells`.
async fn memorize_whole_class(db_url: &str, char_id: i64, class: &str) {
    let gems: Vec<(i32, String)> = world::class_spell_names(class)
        .into_iter()
        .enumerate()
        .map(|(i, n)| (i as i32, n))
        .collect();
    let pool = db::open(db_url).await.expect("open pool");
    db::save_spell_gems(&pool, char_id, &gems, true).await.expect("memorize the class");
}

/// Empty a character's spell bar and mark it never held, as a character from
/// before the server held bars would be.
async fn forget_all_spells(db_url: &str, char_id: i64) {
    let pool = db::open(db_url).await.expect("open pool");
    db::save_spell_gems(&pool, char_id, &[], false).await.expect("forget all spells");
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
    let db_url = DB_FOR_AUTH.lock().unwrap().get(auth_url).cloned();
    if let Some(db_url) = db_url {
        memorize_whole_class(&db_url, char_id, class).await;
    }

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

/// Food and water: set a character's counters (seconds of fullness) and,
/// optionally, its HP, before it logs in (the row is read at login).
async fn set_char_appetite(db_url: &str, char_id: i64, hunger: f32, thirst: f32, hp: Option<f32>) {
    let pool = projectdawn_server::db::open(db_url).await.expect("open pool");
    sqlx::query("UPDATE characters SET hunger = ?1, thirst = ?2, hp = COALESCE(?3, hp) WHERE id = ?4")
        .bind(hunger)
        .bind(thirst)
        .bind(hp)
        .bind(char_id)
        .execute(&pool)
        .await
        .expect("set appetite");
}

/// Store a weapon skill far over any cap (read at login, over the starting
/// scores), so the server's miss roll sits at its 5% floor: tests that need a
/// melee swing to land stop depending on a 35% coin flip.
async fn master_weapon_skill(db_url: &str, char_id: i64, key: &str) {
    let pool = projectdawn_server::db::open(db_url).await.expect("open pool");
    sqlx::query(
        "INSERT OR REPLACE INTO character_skills (char_id, kind, key, score) VALUES (?1, 'weapon', ?2, 1000)",
    )
    .bind(char_id)
    .bind(key)
    .execute(&pool)
    .await
    .expect("master weapon skill");
}

/// Set a character's persisted casting skill score (read at login).
async fn set_casting_skill(db_url: &str, char_id: i64, key: &str, score: i32) {
    let pool = projectdawn_server::db::open(db_url).await.expect("open pool");
    sqlx::query(
        "INSERT OR REPLACE INTO character_skills (char_id, kind, key, score) VALUES (?1, 'casting', ?2, ?3)",
    )
    .bind(char_id)
    .bind(key)
    .bind(score)
    .execute(&pool)
    .await
    .expect("set casting skill");
}

/// Set a character's persisted stamina (read at login).
async fn set_char_stamina(db_url: &str, char_id: i64, stamina: f32) {
    let pool = projectdawn_server::db::open(db_url).await.expect("open pool");
    sqlx::query("UPDATE characters SET stamina = ?1 WHERE id = ?2")
        .bind(stamina)
        .bind(char_id)
        .execute(&pool)
        .await
        .expect("set stamina");
}

/// Put a stack straight into a character's inventory row (read at login).
async fn give_char_item(db_url: &str, char_id: i64, location: &str, slot: u32, path: &str, count: u32) {
    let pool = projectdawn_server::db::open(db_url).await.expect("open pool");
    sqlx::query(
        "INSERT OR REPLACE INTO character_items (char_id, location, slot, item_path, count)
         VALUES (?1, ?2, ?3, ?4, ?5)",
    )
    .bind(char_id)
    .bind(location)
    .bind(slot)
    .bind(path)
    .bind(count)
    .execute(&pool)
    .await
    .expect("give item");
}

async fn read_char_appetite(db_url: &str, char_id: i64) -> (f32, f32) {
    let pool = projectdawn_server::db::open(db_url).await.expect("open pool");
    let row: (f32, f32) = sqlx::query_as("SELECT hunger, thirst FROM characters WHERE id = ?1")
        .bind(char_id)
        .fetch_one(&pool)
        .await
        .expect("read appetite");
    row
}

/// How long past a cast bar's end a test releases the cast (or waits out the
/// global cooldown). The server times a cast from when IT processed the
/// CastStart and accepts the release from 100 ms before the bar's end
/// (`CAST_TOLERANCE_MS`) to 2 s after it (`CAST_HOLD_GRACE_MS`); the test can
/// only time from when it SENT the CastStart. A 200 ms margin left 300 ms
/// for the server to process the start, which a loaded full suite overran
/// (a_pet_stays_with_its_owner_after_gate, 2026-10-13: "not ready", no
/// Teleport). One second sits mid-window: it absorbs a second of server lag
/// and can never pass the 2 s hold limit, since the server's clock starts no
/// earlier than the send.
const CAST_RELEASE_MARGIN: Duration = Duration::from_millis(1_000);

const BREAD: &str = "res://data/loot/items/bread_loaf.tres";

struct WorldClient {
    client: RenetClient,
    transport: NetcodeClientTransport,
    _char_id: i64,
}

impl WorldClient {
    /// Drive renet handshake + send app-layer Connect; spin until ConnectOk.
    /// Spin the renet handshake and send the app-layer Connect; the caller
    /// waits for the answer (ConnectOk, or a Kick for a refused login).
    async fn handshake_and_connect(
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
        Self { client, transport, _char_id: char_id }
    }

    /// Connect a login the server is expected to REFUSE; returns the Kick.
    async fn start_refused(
        token_bytes: Vec<u8>,
        session_token_hex: &str,
        char_id: i64,
    ) -> ServerWorldMsg {
        let mut this = Self::handshake_and_connect(token_bytes, session_token_hex, char_id).await;
        let answer = this
            .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
                matches!(m, ServerWorldMsg::ConnectOk { .. } | ServerWorldMsg::Kick { .. })
            })
            .await
            .expect("an answer to Connect (ConnectOk or Kick)");
        // Close the transport as the real client's kick handler does: a refused
        // connection is never disconnected by the server, so a client that just
        // vanished would hold its netcode slot (and client id) until the
        // timeout, and the next login attempt's handshake would be ignored.
        this.transport.disconnect();
        for _ in 0..4 {
            tick_one(&mut this.client, &mut this.transport);
            tokio::time::sleep(TICK_DT).await;
        }
        answer
    }

    async fn start(
        token_bytes: Vec<u8>,
        session_token_hex: &str,
        char_id: i64,
    ) -> Self {
        let mut this = Self::handshake_and_connect(token_bytes, session_token_hex, char_id).await;
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
        let msg = ClientWorldMsg::Move { sequence, direction, jumping: false, yaw: 0.0 };
        send_msg(&mut self.client, CHANNEL_POSITION, &msg);
    }

    /// A Move carrying the real `jumping` flag (the client sets it on the Move
    /// after a jump begins).
    fn send_move_jump(&mut self, sequence: u32, direction: Vec3, jumping: bool) {
        let msg = ClientWorldMsg::Move { sequence, direction, jumping, yaw: 0.0 };
        send_msg(&mut self.client, CHANNEL_POSITION, &msg);
    }

    /// A Move carrying a facing (PD_W0029): the client's body yaw.
    fn send_move_facing(&mut self, sequence: u32, direction: Vec3, yaw: f32) {
        let msg = ClientWorldMsg::Move { sequence, direction, jumping: false, yaw };
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

    fn send_resurrect_accept(&mut self, corpse_id: u64, accept: bool) {
        send_msg(
            &mut self.client,
            CHANNEL_SYSTEM,
            &ClientWorldMsg::ResurrectAccept { corpse_id, accept },
        );
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

    /// Dev/GM command: take `amount` damage. Gated like the other dev tools.
    fn send_damage_self(&mut self, amount: i32) {
        send_msg(&mut self.client, CHANNEL_SYSTEM, &ClientWorldMsg::DamageSelf { amount });
    }

    /// The Test Panel's Full Heal: tops HP, mana and stamina off (GM only).
    fn send_heal_self(&mut self) {
        send_msg(&mut self.client, CHANNEL_SYSTEM, &ClientWorldMsg::HealSelf { amount: 0 });
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

    fn send_sit(&mut self) {
        send_msg(&mut self.client, CHANNEL_SYSTEM, &ClientWorldMsg::Sit);
    }

    fn send_stand(&mut self) {
        send_msg(&mut self.client, CHANNEL_SYSTEM, &ClientWorldMsg::Stand);
    }

    fn send_memorize_start(&mut self, gem: u8, spell: &str) {
        send_msg(
            &mut self.client,
            CHANNEL_SYSTEM,
            &ClientWorldMsg::MemorizeStart { gem, spell_name: spell.into() },
        );
    }

    fn send_memorize_finish(&mut self) {
        send_msg(&mut self.client, CHANNEL_SYSTEM, &ClientWorldMsg::MemorizeFinish);
    }

    fn send_seed_spell_gems(&mut self, gems: &[&str]) {
        let gems = gems.iter().map(|g| g.to_string()).collect();
        send_msg(&mut self.client, CHANNEL_SYSTEM, &ClientWorldMsg::SeedSpellGems { gems });
    }

    /// `/camp`: the one intent that needs the SERVER's seated flag, which
    /// makes it the probe for whether the server thinks a player sits.
    fn send_camp(&mut self) {
        send_msg(&mut self.client, CHANNEL_SYSTEM, &ClientWorldMsg::Camp);
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

    /// Factions (2026-10-23): consider an NPC by id or a mob by entity id.
    fn send_consider(&mut self, npc_id: Option<&str>, target_id: Option<u64>) {
        let msg = ClientWorldMsg::Consider { npc_id: npc_id.map(str::to_string), target_id };
        send_msg(&mut self.client, CHANNEL_SYSTEM, &msg);
    }

    /// The GM faction panel's slider: set our own standing with one faction.
    /// Gated like the other dev tools.
    fn send_dev_set_faction(&mut self, faction_id: &str, standing: f32) {
        let msg = ClientWorldMsg::DevSetFaction { faction_id: faction_id.into(), standing };
        send_msg(&mut self.client, CHANNEL_SYSTEM, &msg);
    }

    fn send_accept_quest(&mut self, quest_id: &str) {
        let msg = ClientWorldMsg::AcceptQuest { quest_id: quest_id.into(), giver_id: 0 };
        send_msg(&mut self.client, CHANNEL_SYSTEM, &msg);
    }

    fn send_complete_quest(&mut self, quest_id: &str) {
        let msg = ClientWorldMsg::CompleteQuest { quest_id: quest_id.into() };
        send_msg(&mut self.client, CHANNEL_SYSTEM, &msg);
    }

    /// Dev-gated "Despawn All Enemies" (the Test Panel button).
    fn send_dev_despawn_enemies(&mut self) {
        send_msg(&mut self.client, CHANNEL_SYSTEM, &ClientWorldMsg::DevDespawnEnemies);
    }

    fn send_pet_command(&mut self, command: u8, target_id: Option<u64>) {
        let msg = ClientWorldMsg::PetCommand { command, target_id };
        send_msg(&mut self.client, CHANNEL_SYSTEM, &msg);
    }

    fn send_use_skill(&mut self, skill_name: &str, target_id: Option<u64>) {
        let msg = ClientWorldMsg::UseSkill { skill_name: skill_name.into(), target_id };
        send_msg(&mut self.client, CHANNEL_SYSTEM, &msg);
    }

    fn send_stop_song(&mut self) {
        send_msg(&mut self.client, CHANNEL_SYSTEM, &ClientWorldMsg::StopSong);
    }

    /// Pump for `d` with a heartbeat every 2 s, so a quiet wait longer than
    /// the server's 10 s `HEARTBEAT_TIMEOUT` does not read as a drop.
    async fn pump_alive_for(&mut self, d: Duration) {
        let end = Instant::now() + d;
        let mut next_beat = Instant::now();
        while Instant::now() < end {
            if Instant::now() >= next_beat {
                self.send_heartbeat();
                next_beat = Instant::now() + Duration::from_secs(2);
            }
            tick_one(&mut self.client, &mut self.transport);
            self.drain_positions();
            tokio::time::sleep(TICK_DT).await;
        }
    }

    /// Throw away buffered Position messages. The unreliable channel's
    /// receive buffer is memory-limited: left undrained through a long wait
    /// it fills, renet drops what arrives after, and a later measurement
    /// (`median_step`) reads the stale positions it kept.
    fn drain_positions(&mut self) {
        while self.client.receive_message(CHANNEL_POSITION).is_some() {}
    }

    /// Pump for `d`, keeping every SYSTEM message that arrives meanwhile (a
    /// `wait_for` drops what it does not match; a window that must count
    /// several kinds needs them all). Heartbeats every 2 s, as above.
    async fn collect_for(&mut self, d: Duration) -> Vec<ServerWorldMsg> {
        let end = Instant::now() + d;
        let mut next_beat = Instant::now();
        let mut out = Vec::new();
        while Instant::now() < end {
            if Instant::now() >= next_beat {
                self.send_heartbeat();
                next_beat = Instant::now() + Duration::from_secs(2);
            }
            tick_one(&mut self.client, &mut self.transport);
            self.drain_positions();
            while let Some(bytes) = self.client.receive_message(CHANNEL_SYSTEM) {
                if let Ok((msg, _)) =
                    bincode::serde::decode_from_slice::<ServerWorldMsg, _>(&bytes, bincode_cfg())
                {
                    out.push(msg);
                }
            }
            tokio::time::sleep(TICK_DT).await;
        }
        out
    }

    fn send_use_consumable_base(&mut self, idx: u8) {
        send_msg(
            &mut self.client,
            CHANNEL_SYSTEM,
            &ClientWorldMsg::UseConsumable { slot: SlotRef::BaseSlot { idx } },
        );
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

    fn send_say(&mut self, text: &str) {
        let msg = ClientWorldMsg::Chat {
            channel: protocol::world::ChatChannel::Say,
            text: text.into(),
            target_name: None,
        };
        send_msg(&mut self.client, CHANNEL_SYSTEM, &msg);
    }

    fn send_bank_withdraw_item(&mut self, vault_slot: u32) {
        let msg = ClientWorldMsg::BankWithdrawItem { shared: false, vault_slot };
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

    // A is a Human Warrior, so char_data::compute gives EQ's pools (the
    // parity audit, 2026-10-20): max_hp 28 (5 + 220 / 10 + CON 20 x 220 /
    // 3000), max_mp the token 20 (no mana stat), max_stamina 120 (BASE_ST
    // 100 + Warrior 20). `create_character` seeds current = max.
    let h_at_b = b
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::HealthUpdate { id, .. } if *id == a_char_id as u64)
        })
        .await
        .expect("B receives HealthUpdate for A via DB-seeded fan-out");
    if let ServerWorldMsg::HealthUpdate { hp, max_hp, .. } = h_at_b {
        assert!((hp - 28.0).abs() < 0.01, "Warrior hp: {hp}");
        assert!((max_hp - 28.0).abs() < 0.01, "Warrior max_hp: {max_hp}");
    }

    let m_at_b = b
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::ManaUpdate { id, .. } if *id == a_char_id as u64)
        })
        .await
        .expect("B receives ManaUpdate for A via DB-seeded fan-out");
    if let ServerWorldMsg::ManaUpdate { mp, max_mp, .. } = m_at_b {
        assert!((mp - 20.0).abs() < 0.01, "Warrior mp: {mp}");
        assert!((max_mp - 20.0).abs() < 0.01, "Warrior max_mp: {max_mp}");
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

/// The client-made HitBroadcast is a dev command (the Test Panel's). A plain
/// account's forged hit on another player reaches nobody: before, it was
/// relayed as a real Hit, putting fake combat text on screens and, with the
/// seated-combat rule, standing the victim up. A GM's still fans to the target
/// with its payload, and the sender gets no echo.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_clients_hit_fanout() {
    let h = start_both().await;

    let (a_session, a_char_id, _stale) =
        provision_client(&h.auth_url, "iota", "Iot", "Human", "Warrior").await;
    let (b_session, b_char_id, b_token) =
        provision_client(&h.auth_url, "kappa", "Kap", "Elf", "Wizard").await;
    let pool = db::open(&h.db_url).await.expect("open pool");
    db::set_account_gm(&pool, "iota", true).await.expect("set is_gm");
    let a_token = request_world_token(&h.auth_url, &a_session, a_char_id).await;

    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    let mut b = WorldClient::start(b_token, &b_session, b_char_id).await;

    // Only meaningful when the process is not in dev mode, where every
    // connection may use dev commands.
    if std::env::var("PD_DEV_CMDS").as_deref() != Ok("1") {
        b.send_hit(a_char_id as u64, 99, false, DamageType::Physical);
        b.pump_for(Duration::from_millis(300)).await;
        let forged = a
            .wait_for(CHANNEL_SYSTEM, Duration::from_secs(1), |m| {
                matches!(m, ServerWorldMsg::Hit { attacker, .. } if *attacker == b_char_id as u64)
            })
            .await;
        assert!(forged.is_none(), "a plain account's made-up hit reaches nobody");
    }

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

    let (a_session, a_char_id, _a_stale) =
        provision_client(&h.auth_url, "lambda", "Lam", "Human", "Cleric").await;
    let (b_session, b_char_id, b_token) =
        provision_client(&h.auth_url, "mumu", "Mumu", "Elf", "Druid").await;

    let a_token = gm_world_token(&h.auth_url, &h.db_url, "lambda", &a_session, a_char_id).await;
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
    // Spirit of the Bear requires level 10; provisioned characters start at 1.
    set_char_level(&h.db_url, a_char_id, 10).await;
    let (b_session, b_char_id, b_token) =
        provision_client(&h.auth_url, "theta", "The", "Elf", "Druid").await;

    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    let mut b = WorldClient::start(b_token, &b_session, b_char_id).await;

    // A casts Spirit of the Bear (ALLY, 15 immediate heal + 4 hps Ã— 18s HoT,
    // cast_time 1.0s). Track 10 â€” the cast-time gate now rejects
    // CastSpell unless a matching CastStart ran long enough, so we
    // pump CastStart out first (transport doesn't advance during a
    // bare tokio sleep), then wait the cast time, then send CastSpell.
    // Server applies the HoT to A.active_buffs and fans BuffSnapshot
    // to all in-world peers including B.
    a.send_cast_start("Spirit of the Bear", 2.0);
    for _ in 0..3 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }
    a.pump_for(Duration::from_millis(2100)).await;
    a.send_cast_spell("Spirit of the Bear", None);
    for _ in 0..6 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }

    // Filter for the Spirit of the Bear entry so we don't latch onto the
    // empty seed snapshot the server fans at step-4a join time.
    let _ = b
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::BuffSnapshot { target, buffs }
                if *target == a_char_id as u64
                    && buffs.iter().any(|(n, _)| n == "Spirit of the Bear"))
        })
        .await
        .expect("B receives server-driven BuffSnapshot containing Spirit of the Bear");
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

    // Walk for ~2 s at 50 ms cadence. Each Move refreshes the server-side
    // stale-move clock so integration continues until we stop sending.
    // 3.5 s, not 2 s: at 2 s the player only clips camp 0's aggro radius, so
    // whether an enemy locks on before the wait expires depended on where it
    // happened to be wandering. Walking fully in makes the pull deterministic.
    // Walk in on the server's own word, not the wall clock (a slow test
    // thread used to come up short): to 2 m from the spawn point, well
    // inside its aggro whatever the spawn jitter.
    let mut seq: u32 = 1;
    walk_until_within(&mut a, &mut seq, a_char_id, Vec3 { x: 0.0, y: 0.0, z: 0.0 }, BONEPILE_SPAWN, 2.0).await;
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

    master_weapon_skill(&h.db_url, a_char_id, "hand_to_hand").await;
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;

    // Walk toward the Bonepile's isolated [-16, -14] spawn (one level 1
    // Decrepit Skeleton â€” see the shared phase 4 layout note above).
    // Phase 4 layout note: every camp-walking test aims at the Bonepile's
    // isolated [-16, 0, -14] spawn â€” the one ring 1 spawn whose aggro circle
    // overlaps no other, so exactly ONE slow (1.8 m/s, 2.5 s swing) level 1
    // Decrepit Skeleton pulls, the same single-puller semantics these tests
    // were written against. Do NOT aim at the Wolf Run: it is a four-wolf
    // pack, and standing in it turns every cast into interrupt rolls.
    // 3.5 s, not 2 s: at 2 s the player only clips camp 0's aggro radius, so
    // whether an enemy locks on before the wait expires depended on where it
    // happened to be wandering. Walking fully in makes the pull deterministic.
    // Walk in on the server's own word, not the wall clock (a slow test
    // thread used to come up short): to 2 m from the spawn point, well
    // inside its aggro whatever the spawn jitter.
    let mut seq: u32 = 1;
    walk_until_within(&mut a, &mut seq, a_char_id, Vec3 { x: 0.0, y: 0.0, z: 0.0 }, BONEPILE_SPAWN, 2.0).await;

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
/// A sits at the origin (cell (0, 0)). B starts at x = 260 (cell (2, 0))
/// and runs in the -X direction at RUN_SPEED = 5 m/s. After
/// ~4 s B crosses x = 240 (cell (1, 0)), which is adjacent to A's cell.
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
    db::set_character_position(&pool, b_char_id, 260.0, 0.0, 0.0)
        .await
        .expect("set B starting position");

    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    let mut b = WorldClient::start(b_token, &b_session, b_char_id).await;

    // Walk B toward A. 260 to 240 = 20 m at 5 m/s = 4 s; 12 s covers
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
    // dmg_type=Fire. A spell does exactly its listed number (45, the parity
    // audit of 2026-10-20); only a crit changes it, 1.5x to 2x.
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
    if let ServerWorldMsg::Hit { amount, crit, .. } = aoe_hit {
        if crit {
            assert!((67..=90).contains(&amount), "a crit of 45: {amount}");
        } else {
            assert_eq!(amount, 45, "Inferno's listed 45, nothing added");
        }
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
    // Spirit of the Bear requires level 10; provisioned characters start at 1.
    set_char_level(&h.db_url, a_char_id, 10).await;

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
    a.send_cast_start("Spirit of the Bear", 2.0);
    for _ in 0..3 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }
    // Now wait past the cast time (2.0 s) on the server's wall clock.
    // 2100 ms includes a 100 ms cushion above the gate's tolerance.
    a.pump_for(Duration::from_millis(2100)).await;
    a.send_cast_spell("Spirit of the Bear", None);
    for _ in 0..6 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }

    // Spirit of the Bear applies a 4 hps Ã— 18 s HoT; the server fans a
    // BuffSnapshot containing "Spirit of the Bear" once the cast lands.
    // That's the cleanest "cast actually applied server-side" signal
    // and doesn't race against regen ticks like HealthUpdate would.
    let snap = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::BuffSnapshot { target, buffs }
                if *target == a_char_id as u64
                    && buffs.iter().any(|(n, _)| n == "Spirit of the Bear"))
        })
        .await
        .expect("Spirit of the Bear applies once the cast bar has run");
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
        // Stats ride the camp curve at PET_STAT_SCALAR (70%); the curve's
        // low levels are the parity audit's (37, 52 and 64 at 3, 4 and 5).
        let expected_hp = match level {
            3 => 25.9,
            4 => 36.4,
            5 => 44.8,
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
    // Walk in on the server's own word, not the wall clock (a slow test
    // thread used to come up short): to 2 m from the spawn point, well
    // inside its aggro whatever the spawn jitter.
    let mut seq: u32 = 1;
    walk_until_within(&mut a, &mut seq, a_char_id, Vec3 { x: 0.0, y: 0.0, z: 0.0 }, BONEPILE_SPAWN, 2.0).await;

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
    // 3.5 s, not 2 s: at 2 s the player only clips camp 0's aggro radius, so
    // whether an enemy locks on before the wait expires depended on where it
    // happened to be wandering. Walking fully in makes the pull deterministic.
    // Walk in on the server's own word, not the wall clock (a slow test
    // thread used to come up short): to 2 m from the spawn point, well
    // inside its aggro whatever the spawn jitter.
    let mut seq: u32 = 1;
    walk_until_within(&mut a, &mut seq, a_char_id, Vec3 { x: 0.0, y: 0.0, z: 0.0 }, BONEPILE_SPAWN, 2.0).await;
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

/// Track 12 Piece A2: the pet pulls aggro through the threat re-evaluation.
/// A mob targets the player, the pet is sent on it, and once the pet's
/// threat clears 1.3x the player's the mob switches to the pet, fanning an
/// EntityTarget. The mob is the test's own dev spawn (500 HP, 1 damage),
/// not a camp mob: since the EQ parity pass a camp's Decrepit Skeleton has
/// 16 HP and the pet killed it before the switch could happen, so the test
/// read camp numbers instead of the threat rule.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pet_pulls_aggro_via_threat_reaggro() {
    let h = start_both().await;

    let (a_session, a_char_id, _stale_token) =
        provision_client(&h.auth_url, "tnk", "Tanker", "Human", "Necromancer").await;
    // Summon Skeleton requires level 6; provisioned characters start at 1.
    set_char_level(&h.db_url, a_char_id, 6).await;
    // GM for the dev spawn; the token is minted after the grant.
    let pool = db::open(&h.db_url).await.expect("open pool");
    db::set_account_gm(&pool, "tnk", true).await.expect("set is_gm");
    let a_token = request_world_token(&h.auth_url, &a_session, a_char_id).await;

    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;

    // Summon first, in peace: casting the 3 s summon while a mob swings at
    // you rolls an interrupt per hit taken (channeling 0).
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

    // A sturdy target 3 m away that notices the player at once and walks
    // into melee: it outlives the whole exchange, and its 1 damage a swing
    // cannot kill the player first.
    a.send_dev_spawn("Threat Dummy", 1, 500.0, 1, 2.0, 10.0);

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

    // One player swing first gives the player real threat, so the switch is
    // the 1.3x rule at work and not a pull off an empty list; the pet is sent
    // in only after it, so no switch can happen (and be skipped by a wait)
    // before the test listens for it. The swing waits for the dummy to reach
    // melee and hit the player, so it is in reach.
    let in_melee = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(5), |m| {
            matches!(m, ServerWorldMsg::Hit { attacker, target, .. }
                if *attacker == enemy_id && *target == a_char_id as u64)
        })
        .await;
    assert!(in_melee.is_some(), "the dummy walks into melee and swings at the player");
    a.send_attack(enemy_id, "", false, DamageType::Physical);
    for _ in 0..3 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }

    // Now lock the pet onto the enemy via /pet attack.
    a.send_pet_command(protocol::world::pet_command::ATTACK, Some(enemy_id));

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
        // And the charm HOLDS. The decay sweep used to compare with a
        // saturating duration, so every charm "expired" 50 ms after it
        // landed (seen in play 2026-10-06). Charm lasts 60 s; nothing may
        // despawn the pet in the next second and a half.
        let early_release = a
            .wait_for(CHANNEL_SYSTEM, Duration::from_millis(1500), |m| {
                matches!(m, ServerWorldMsg::EntityDespawn { id: gone } if *gone == id)
            })
            .await;
        assert!(early_release.is_none(), "a fresh charm must not release on the next tick");
    }
}

/// Seen in play 2026-10-06: a Wild Boar (aggro 10, so a 20 m leash) nuked
/// from about 22 m stood there and took it. An idle mob turns on an attacker
/// only inside its leash, and spells reach 25 m. Every aggressive mob's leash
/// now has a floor past spell reach. Here a short-sighted mob (aggro 1 m, so
/// a 2 m leash before the floor) is nuked from 20 m and must turn on the
/// caster. The radius is 1 m rather than the boar's 10 because a dev spawn
/// lands 3 m from the requester, and a mob that notices the caster on its
/// first tick would have targeted them before the nuke.
#[tokio::test]
async fn a_mob_nuked_from_beyond_its_old_leash_turns_on_the_caster() {
    let h = start_both().await;
    // Smite: Cleric, level 1, instant.
    let (a_session, a_char_id, _stale_token) =
        provision_client(&h.auth_url, "leashnuke", "Leashnuke", "Human", "Cleric").await;
    let pool = db::open(&h.db_url).await.expect("open pool");
    db::set_account_gm(&pool, "leashnuke", true).await.expect("set is_gm");
    let a_token = request_world_token(&h.auth_url, &a_session, a_char_id).await;
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;

    // Aggro 1 m, no leash given: 2 m before the floor. Speed 0 so the test
    // reads the mob's decision (its target), not a chase.
    a.send_dev_spawn("Timid Skeleton", 1, 500.0, 0, 0.0, 1.0);
    let spawn_evt = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::EnemySpawn { mob_name, .. } if mob_name == "Timid Skeleton")
        })
        .await
        .expect("the skeleton spawns");
    let (mob, mob_pos): (u64, Vec3) = match spawn_evt {
        ServerWorldMsg::EnemySpawn { id, pos, .. } => (id, pos),
        _ => unreachable!(),
    };

    // The dev spawn lands 3 m ahead (-Z at yaw 0); back off to about 20 m:
    // outside the old 16 m leash, inside the 25 m spell reach. 14.5 m of
    // Moves at RUN_SPEED, plus the 3 m and the ~2.5 m the server keeps
    // integrating after the last Move (STALE_MOVE_THRESHOLD).
    let moves = (14.5 / (world::RUN_SPEED * 0.05)).ceil() as u32;
    let mut seq: u32 = 1;
    for _ in 0..moves {
        a.send_move(seq, Vec3 { x: 0.0, y: 0.0, z: 1.0 });
        seq += 1;
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }
    a.pump_for(Duration::from_millis(600)).await; // let the park settle

    // Read the distance off the server's own Position fan rather than
    // trusting the arithmetic: the test is only evidence if the caster really
    // stands past the old leash and inside spell reach.
    let mut own_pos: Option<Vec3> = None;
    while let Some(bytes) = a.client.receive_message(CHANNEL_POSITION) {
        if let Ok((ServerWorldMsg::Position { id, pos, .. }, _)) =
            bincode::serde::decode_from_slice::<ServerWorldMsg, _>(&bytes, bincode_cfg())
        {
            if id == a_char_id as u64 {
                own_pos = Some(pos);
            }
        }
    }
    let own_pos = own_pos.expect("the server fans the caster their own position");
    let dist = ((own_pos.x - mob_pos.x).powi(2) + (own_pos.z - mob_pos.z).powi(2)).sqrt();
    assert!(
        (17.0..25.0).contains(&dist),
        "test setup: caster must stand past the old leash and inside 25 m reach, is at {dist:.1} m"
    );
    // And the mob must not have noticed the caster yet, or a target message
    // from the setup would answer for the nuke (the first cut of this test
    // passed on the old code for exactly that reason).
    while let Some(bytes) = a.client.receive_message(CHANNEL_SYSTEM) {
        if let Ok((ServerWorldMsg::EntityTarget { id, target: Some(_) }, _)) =
            bincode::serde::decode_from_slice::<ServerWorldMsg, _>(&bytes, bincode_cfg())
        {
            assert_ne!(id, mob, "test setup: the mob targeted someone before the nuke");
        }
    }

    a.send_cast_spell("Smite", Some(mob));
    let turned = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::EntityTarget { id, target: Some(t) }
                if *id == mob && *t == a_char_id as u64)
        })
        .await;
    assert!(
        turned.is_some(),
        "a mob hit from inside spell reach must turn on the caster, not stand and take it"
    );
}

/// A charm that ends hands the mob back (user call 2026-10-06, EQ's rule): the
/// pet is despawned, the same mob respawns in its place with the HP it had,
/// and it comes straight for the one who charmed it. Siren's Song is the
/// shortest charm (30 s), so the wait is real time; heartbeats keep the
/// app-layer idle timeout quiet meanwhile. Fails on the previous code at the
/// EnemySpawn: the old sweep deleted the pet and spawned nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_expired_charm_hands_the_mob_back_and_it_turns_on_the_charmer() {
    let h = start_both().await;
    let (a_session, a_char_id, _stale_token) =
        provision_client(&h.auth_url, "siren", "Sirenna", "Human", "Bard").await;
    // Siren's Song is Bard level 14.
    set_char_level(&h.db_url, a_char_id, 14).await;
    let pool = db::open(&h.db_url).await.expect("open pool");
    db::set_account_gm(&pool, "siren", true).await.expect("set is_gm");
    let a_token = request_world_token(&h.auth_url, &a_session, a_char_id).await;
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;

    // Aggro 1 m: it does not notice the bard 3 m away before the charm (so
    // the 1.5 s cast cannot be interrupted) and has a real leash afterwards;
    // speed 2 and dmg 5 so, once returned, it can reach and hit the bard.
    a.send_dev_spawn("Charm Dummy", 1, 500.0, 5, 2.0, 1.0);
    let spawn_evt = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::EnemySpawn { mob_name, .. } if mob_name == "Charm Dummy")
        })
        .await
        .expect("the dummy spawns");
    let enemy_id: u64 = match spawn_evt {
        ServerWorldMsg::EnemySpawn { id, replaces, .. } => {
            assert_eq!(replaces, None, "an ordinary spawn replaces nothing");
            id
        }
        _ => unreachable!(),
    };

    a.send_cast_start("Siren's Song", 1.5);
    a.pump_for(Duration::from_millis(1700)).await;
    a.send_cast_spell("Siren's Song", Some(enemy_id));
    let pet_spawn = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(5), |m| {
            matches!(m, ServerWorldMsg::PetSpawn { owner, .. } if *owner == a_char_id as u64)
        })
        .await
        .expect("the charm lands");
    // One creature throughout (EQ): the pet names the enemy it was, so a
    // client that had the mob targeted keeps it targeted.
    let pet_id: u64 = match pet_spawn {
        ServerWorldMsg::PetSpawn { id, replaces, .. } => {
            assert_eq!(replaces, Some(enemy_id), "the charmed pet replaces the enemy id");
            id
        }
        _ => unreachable!(),
    };

    // 30 s of charm. A heartbeat every few seconds so the 10 s idle timeout
    // does not reap the bard first.
    let deadline = Instant::now() + Duration::from_secs(40);
    let mut released = None;
    while released.is_none() && Instant::now() < deadline {
        a.send_heartbeat();
        released = a
            .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
                matches!(m, ServerWorldMsg::EntityDespawn { id } if *id == pet_id)
            })
            .await;
    }
    assert!(released.is_some(), "the charm ends and the pet is despawned");

    let back = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::EnemySpawn { mob_name, .. } if mob_name == "Charm Dummy")
        })
        .await
        .expect("the mob comes back as an enemy where the pet stood");
    let (mob_id, hp) = match back {
        ServerWorldMsg::EnemySpawn { id, hp, replaces, .. } => {
            assert_eq!(replaces, Some(pet_id), "the returned mob replaces the pet id");
            (id, hp)
        }
        _ => unreachable!(),
    };
    assert_ne!(mob_id, enemy_id, "a fresh enemy id; the old one is never reused");
    assert!((hp - 500.0).abs() < 1e-3, "it keeps the HP it had, got {hp}");

    a.send_heartbeat();
    let turned = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::EntityTarget { id, target: Some(t) }
                if *id == mob_id && *t == a_char_id as u64)
        })
        .await;
    assert!(turned.is_some(), "the returned mob targets its former charmer");
    let hit = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(10), |m| {
            matches!(m, ServerWorldMsg::Hit { attacker, target, .. }
                if *attacker == mob_id && *target == a_char_id as u64)
        })
        .await;
    assert!(hit.is_some(), "and comes for them: a hit on the bard within seconds");
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

/// Warder's Mend was the one client-only pet spell: the server had no PET_HEAL
/// arm and refused it as "no effect yet" (playtest 2026-10-07; the tester asked
/// for no client-only pet spells). It now mends the caster's own pet, whatever
/// is targeted. Fails on the previous code: no HealthUpdate for the warder
/// ever arrives.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn warders_mend_heals_the_beast_masters_own_warder() {
    let h = start_both().await;
    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "mender", "Mendra", "Human", "Beast Master").await;
    set_char_level(&h.db_url, a_char_id, 22).await;
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;

    let pet_spawn = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(5), |m| {
            matches!(m, ServerWorldMsg::PetSpawn { owner, .. } if *owner == a_char_id as u64)
        })
        .await
        .expect("the warder auto-summons");
    let pet_id: u64 = match pet_spawn {
        ServerWorldMsg::PetSpawn { id, .. } => id,
        _ => unreachable!(),
    };

    // No target at all: the spell finds the caster's own pet.
    a.send_cast_start("Warder's Mend", 1.5);
    a.pump_for(Duration::from_millis(1700)).await;
    a.send_cast_spell("Warder's Mend", None);
    let mended = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(5), |m| {
            matches!(m, ServerWorldMsg::HealthUpdate { id, .. } if *id == pet_id)
        })
        .await;
    let Some(ServerWorldMsg::HealthUpdate { hp, max_hp, .. }) = mended else {
        panic!("the server mends the warder and fans its health");
    };
    assert!((hp - max_hp).abs() < 0.01, "an unhurt warder stays at full, got {hp}/{max_hp}");
}

/// Spell batch step 0: a corpse casts nothing. The dead-state gate at message
/// arrival already refuses a cast once the death SWEEP has run, so the window
/// is the tick in which hp hits zero: a hit or dev damage lands before the
/// cast step, the death sweep runs after it, and a cast queued in between
/// completed. Here a GM Cleric sends the dev DamageSelf and a Smite in the
/// same batch. The server refuses the Smite privately ("You are dead.") and
/// the dummy is never hit. On the previous code the Smite landed; a self-heal
/// in that gap would have revived the caster past the death penalty.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dead_caster_casts_nothing() {
    let h = start_both().await;
    let (a_session, a_char_id, _stale) =
        provision_client(&h.auth_url, "deadcaster", "Morbid", "Human", "Cleric").await;
    let pool = db::open(&h.db_url).await.expect("open pool");
    db::set_account_gm(&pool, "deadcaster", true).await.expect("set is_gm");
    let a_token = request_world_token(&h.auth_url, &a_session, a_char_id).await;
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;

    a.send_dev_spawn("Charm Dummy", 1, 500.0, 0, 0.0, 0.0);
    let spawn = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::EnemySpawn { mob_name, .. } if mob_name == "Charm Dummy")
        })
        .await
        .expect("the dummy spawns");
    let dummy: u64 = match spawn {
        ServerWorldMsg::EnemySpawn { id, .. } => id,
        _ => unreachable!(),
    };

    // Same batch, no tick between: both arrive in one message drain, the
    // damage first.
    a.send_damage_self(1_000_000);
    a.send_cast_spell("Smite", Some(dummy));
    let refused = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::CastFail { caster, reason }
                if *caster == a_char_id as u64 && reason.contains("dead"))
        })
        .await;
    assert!(refused.is_some(), "a dead caster is told so");
    let hit = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(1), |m| {
            matches!(m, ServerWorldMsg::Hit { attacker, target, .. }
                if *attacker == a_char_id as u64 && *target == dummy)
        })
        .await;
    assert!(hit.is_none(), "and the dummy is never hit");
}

/// Spell batch step 0: a finished cast cannot be held. Healing Light's bar
/// (1.2 s) is started, then released 4 s later: refused as lapsed. Released
/// on time it lands and costs its mana. On the previous code the late release
/// healed: the gate only asked "long enough?", never "too long?".
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_finished_cast_cannot_be_held() {
    let h = start_both().await;
    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "holder", "Holdred", "Human", "Cleric").await;
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    a.pump_for(Duration::from_millis(300)).await;

    a.send_cast_start("Healing Light", 1.2);
    a.pump_for(Duration::from_millis(4_000)).await; // 1.2 s bar, 2 s grace, margin
    a.send_cast_spell("Healing Light", None);
    let lapsed = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::CastFail { caster, reason }
                if *caster == a_char_id as u64 && reason.contains("lapsed"))
        })
        .await;
    assert!(lapsed.is_some(), "a bar held 4 s past a 1.2 s cast is refused");

    // Released on time, the same cast lands: a ManaUpdate carrying the spend.
    a.send_cast_start("Healing Light", 1.2);
    a.pump_for(Duration::from_millis(1_400)).await;
    a.send_cast_spell("Healing Light", None);
    let charged = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::ManaUpdate { id, mp, max_mp }
                if *id == a_char_id as u64 && *mp < *max_mp)
        })
        .await;
    assert!(charged.is_some(), "released in time, the cast lands and costs mana");
}

/// Spell batch step 0: a refusal reaches only the caster and carries the
/// server's mana. A casts Smite twice inside its 3 s cooldown: A gets the
/// cooldown CastFail and then a ManaUpdate; B, in the world beside A, gets no
/// CastFail for A at all. On the previous code B received it too.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refusal_reaches_only_the_caster_with_the_true_mana() {
    let h = start_both().await;
    let (a_session, a_char_id, _stale) =
        provision_client(&h.auth_url, "private", "Privy", "Human", "Cleric").await;
    let (b_session, b_char_id, b_token) =
        provision_client(&h.auth_url, "bystander", "Bystan", "Human", "Warrior").await;
    let pool = db::open(&h.db_url).await.expect("open pool");
    db::set_account_gm(&pool, "private", true).await.expect("set is_gm");
    let a_token = request_world_token(&h.auth_url, &a_session, a_char_id).await;
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    let mut b = WorldClient::start(b_token, &b_session, b_char_id).await;
    a.pump_for(Duration::from_millis(300)).await;
    b.pump_for(Duration::from_millis(300)).await;

    a.send_dev_spawn("Charm Dummy", 1, 500.0, 0, 0.0, 0.0);
    let spawn = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::EnemySpawn { mob_name, .. } if mob_name == "Charm Dummy")
        })
        .await
        .expect("the dummy spawns");
    let dummy: u64 = match spawn {
        ServerWorldMsg::EnemySpawn { id, .. } => id,
        _ => unreachable!(),
    };

    a.send_cast_spell("Smite", Some(dummy));
    let landed = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::Hit { attacker, target, .. }
                if *attacker == a_char_id as u64 && *target == dummy)
        })
        .await;
    assert!(landed.is_some(), "the first Smite lands");
    a.send_cast_spell("Smite", Some(dummy));
    let refused = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::CastFail { caster, reason }
                if *caster == a_char_id as u64 && reason.contains("cooldown"))
        })
        .await;
    assert!(refused.is_some(), "the second, inside the cooldown, is refused to the caster");
    let settled = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(2), |m| {
            matches!(m, ServerWorldMsg::ManaUpdate { id, .. } if *id == a_char_id as u64)
        })
        .await;
    assert!(settled.is_some(), "and the refusal carries the caster's true mana");

    let leaked = b
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(1), |m| {
            matches!(m, ServerWorldMsg::CastFail { caster, .. } if *caster == a_char_id as u64)
        })
        .await;
    assert!(leaked.is_none(), "a bystander hears nothing of another player's refusal");
}

/// Shared setup for the share-range tests: A (a GM Cleric at spawn) and B (a
/// Warrior seeded `b_x` metres east) grouped, and a one-Smite dummy beside A.
/// Returns the clients and the dummy's id.
async fn grouped_pair_with_dummy(
    h: &Harness,
    a_user: &str,
    b_user: &str,
    b_x: f32,
) -> (WorldClient, WorldClient, u64, i64) {
    let (a_session, a_char_id, _stale) =
        provision_client(&h.auth_url, a_user, "Sharer", "Human", "Cleric").await;
    let (b_session, b_char_id, b_token) =
        provision_client(&h.auth_url, b_user, "Farmate", "Human", "Warrior").await;
    let pool = db::open(&h.db_url).await.expect("open pool");
    db::set_account_gm(&pool, a_user, true).await.expect("set is_gm");
    set_char_pos(&h.db_url, b_char_id, b_x, 0.0).await;
    let a_token = request_world_token(&h.auth_url, &a_session, a_char_id).await;
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    let mut b = WorldClient::start(b_token, &b_session, b_char_id).await;
    a.pump_for(Duration::from_millis(300)).await;
    b.pump_for(Duration::from_millis(300)).await;

    a.send_group_invite("Farmate");
    a.pump_for(Duration::from_millis(300)).await;
    b.pump_for(Duration::from_millis(300)).await;
    b.send_group_accept(a_char_id as u64);
    a.pump_for(Duration::from_millis(400)).await;
    b.pump_for(Duration::from_millis(400)).await;

    // 5 hp: one Smite (7, the parity audit's number) kills it.
    a.send_dev_spawn("Charm Dummy", 1, 5.0, 0, 0.0, 0.0);
    let spawn = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::EnemySpawn { mob_name, .. } if mob_name == "Charm Dummy")
        })
        .await
        .expect("the dummy spawns");
    let dummy: u64 = match spawn {
        ServerWorldMsg::EnemySpawn { id, .. } => id,
        _ => unreachable!(),
    };
    (a, b, dummy, b_char_id)
}

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
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::XpGained { amount, .. } if *amount > 0)
        })
        .await;
    assert!(a_gain.is_some(), "the killer is paid");
    let b_gain = b
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::XpGained { amount, .. } if *amount > 0)
        })
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
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::XpGained { amount, .. } if *amount > 0)
        })
        .await;
    assert!(a_gain.is_some(), "the killer is paid");
    let b_gain = b
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(2), |m| {
            matches!(m, ServerWorldMsg::XpGained { amount, .. } if *amount > 0)
        })
        .await;
    assert!(b_gain.is_none(), "a group-mate 250 m away shares nothing");
}

/// Spell batch step 1: friendly spells reach 30 m. A Cleric heals a player
/// 28 m away (the cast lands and costs mana); the same player walked out to
/// about 34 m is refused as too far. On the previous code 28 m was already
/// past the one 25 m reach.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn heals_reach_30_m_and_not_past_it() {
    let h = start_both().await;
    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "healer30", "Mendy", "Human", "Cleric").await;
    let (b_session, b_char_id, b_token) =
        provision_client(&h.auth_url, "patient30", "Patient", "Human", "Warrior").await;
    set_char_pos(&h.db_url, b_char_id, 28.0, 0.0).await;
    let pool = db::open(&h.db_url).await.expect("open pool");
    db::set_account_gm(&pool, "healer30", true).await.expect("set is_gm");
    // The GM flag rides the connect token: mint it after the grant.
    let a_token = request_world_token(&h.auth_url, &a_session, a_char_id).await;
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    let mut b = WorldClient::start(b_token, &b_session, b_char_id).await;
    a.pump_for(Duration::from_millis(300)).await;
    b.pump_for(Duration::from_millis(300)).await;

    a.send_cast_start("Healing Light", 1.2);
    a.pump_for(Duration::from_millis(1_400)).await;
    a.send_cast_spell("Healing Light", Some(b_char_id as u64));
    let landed = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::ManaUpdate { id, mp, max_mp }
                if *id == a_char_id as u64 && *mp < *max_mp)
        })
        .await;
    assert!(landed.is_some(), "a heal at 28 m lands and costs its mana");

    // B walks 6 m further east: about 34 m. Healing Light's own cooldown is
    // 5 s, so the second cast waits it out.
    let mut seq: u32 = 1;
    for _ in 0..16 {
        b.send_move(seq, Vec3 { x: 1.0, y: 0.0, z: 0.0 });
        seq += 1;
        tick_one(&mut b.client, &mut b.transport);
        tokio::time::sleep(TICK_DT).await;
    }
    a.pump_for(Duration::from_millis(5_200)).await;
    b.pump_for(Duration::from_millis(100)).await;
    // A level 1 pool (about 16) does not hold two Healing Lights (10 each),
    // and the mana gate runs before the reach gate: top it off first.
    a.send_heal_self();
    a.pump_for(Duration::from_millis(200)).await;
    a.send_cast_start("Healing Light", 1.2);
    a.pump_for(Duration::from_millis(1_400)).await;
    a.send_cast_spell("Healing Light", Some(b_char_id as u64));
    let refused = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::CastFail { caster, reason }
                if *caster == a_char_id as u64 && reason.contains("too far"))
        })
        .await;
    assert!(refused.is_some(), "a heal at about 34 m is refused as too far");
}

/// Spell batch step 1: one attack slow at a time, strongest wins. A Shaman
/// lands Torpor (10%) on a dummy, then Slow (5%) is refused at no cost. On the
/// previous code the weaker slow replaced the stronger one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_weaker_slow_is_refused_while_a_stronger_one_holds() {
    let h = start_both().await;
    let (a_session, a_char_id, _stale) =
        provision_client(&h.auth_url, "slower", "Torpid", "Human", "Shaman").await;
    set_char_level(&h.db_url, a_char_id, 20).await;
    let pool = db::open(&h.db_url).await.expect("open pool");
    db::set_account_gm(&pool, "slower", true).await.expect("set is_gm");
    let a_token = request_world_token(&h.auth_url, &a_session, a_char_id).await;
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;

    a.send_dev_spawn("Charm Dummy", 1, 500.0, 0, 0.0, 0.0);
    let spawn = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::EnemySpawn { mob_name, .. } if mob_name == "Charm Dummy")
        })
        .await
        .expect("the dummy spawns");
    let dummy: u64 = match spawn {
        ServerWorldMsg::EnemySpawn { id, .. } => id,
        _ => unreachable!(),
    };

    a.send_cast_start("Torpor", 1.5);
    a.pump_for(Duration::from_millis(1_700)).await;
    a.send_cast_spell("Torpor", Some(dummy));
    let landed = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::ManaUpdate { id, mp, max_mp }
                if *id == a_char_id as u64 && *mp < *max_mp)
        })
        .await;
    assert!(landed.is_some(), "Torpor lands and costs its mana");

    // Past the global cooldown, so the refusal below is the slow rule's.
    a.pump_for(Duration::from_millis(2_100)).await;
    a.send_cast_start("Slow", 1.0);
    a.pump_for(Duration::from_millis(1_200)).await;
    a.send_cast_spell("Slow", Some(dummy));
    let refused = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::CastFail { caster, reason }
                if *caster == a_char_id as u64 && reason.contains("stronger"))
        })
        .await;
    assert!(refused.is_some(), "Slow is refused while Torpor holds the target");
}

/// Walk `metres` east in full-run Moves (RUN_SPEED), then let the park settle.
async fn walk_east(c: &mut WorldClient, seq: &mut u32, metres: f32) {
    walk_dir(c, seq, Vec3 { x: 1.0, y: 0.0, z: 0.0 }, metres).await;
}

/// Walk about `metres` along the unit direction `dir`, one Move per tick,
/// then let the server park the mover. Lands a little long (the server
/// integrates its own dt), so read the arrival off the Position fan when it
/// matters.
/// Send Moves along `dir` for about `metres` at full speed, then pump past
/// STALE_MOVE_THRESHOLD so the server parks the mover. The metres are a
/// wall-clock estimate, not a promise: the server keeps integrating the
/// held direction for the whole threshold after the last Move, so every
/// walk runs about 3.75 m long, and longer when a sleep stretches under
/// load. Fine for walking AWAY from something; to ARRIVE within reach of
/// something, use `walk_until_within`.
async fn walk_dir(c: &mut WorldClient, seq: &mut u32, dir: Vec3, metres: f32) {
    let moves = (metres / (world::RUN_SPEED * 0.05)).ceil() as u32;
    for _ in 0..moves {
        c.send_move(*seq, dir);
        *seq += 1;
        tick_one(&mut c.client, &mut c.transport);
        tokio::time::sleep(TICK_DT).await;
    }
    c.pump_for(Duration::from_millis(600)).await;
}

/// The Bonepile's isolated [-16, 0, -14] spawn: the one ring 1 spawn whose
/// aggro circle overlaps no other, so exactly one slow level 1 Decrepit
/// Skeleton pulls (the camp-walking tests' target).
const BONEPILE_SPAWN: Vec3 = Vec3 { x: -16.0, y: 0.0, z: -14.0 };

/// Walk toward `target` on the server's own word: each tick a Move aims at
/// `target` from the newest Position the server has fanned for `char_id`
/// (`from` until the first arrives), and once that Position is inside
/// `stop_at` a zero Move parks the mover where it stands. Overshooting
/// between two Positions turns the next Move around, so the walk converges
/// however long a sleep took. Returns the position it stopped on.
async fn walk_until_within(
    c: &mut WorldClient,
    seq: &mut u32,
    char_id: i64,
    from: Vec3,
    target: Vec3,
    stop_at: f32,
) -> Vec3 {
    let deadline = Instant::now() + Duration::from_secs(8);
    let mut own = from;
    loop {
        let dx = target.x - own.x;
        let dz = target.z - own.z;
        let dist = (dx * dx + dz * dz).sqrt();
        if dist <= stop_at {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "walk_until_within: still {dist:.2} m from the target after 8 s"
        );
        c.send_move(*seq, Vec3 { x: dx / dist, y: 0.0, z: dz / dist });
        *seq += 1;
        tick_one(&mut c.client, &mut c.transport);
        while let Some(bytes) = c.client.receive_message(CHANNEL_POSITION) {
            if let Ok((ServerWorldMsg::Position { id, pos, .. }, _)) =
                bincode::serde::decode_from_slice::<ServerWorldMsg, _>(&bytes, bincode_cfg())
            {
                if id == char_id as u64 {
                    own = pos;
                }
            }
        }
        tokio::time::sleep(TICK_DT).await;
    }
    // Park. A zero direction is held like any other, so one Move would do;
    // the position channel is unreliable, so send a few.
    for _ in 0..3 {
        c.send_move(*seq, Vec3 { x: 0.0, y: 0.0, z: 0.0 });
        *seq += 1;
        tick_one(&mut c.client, &mut c.transport);
        tokio::time::sleep(TICK_DT).await;
    }
    own
}

/// Spell batch step 2, the exploit ledger's items 1, 3 and 6: the destination
/// is the server's (bind here, walk on, Gate: the server puts you at the
/// bind); a second Gate inside its cooldown is refused and moves nobody; a
/// mob holding the caster as its target drops them when they Gate. Cleric 8
/// has Gate now (the Bind Affinity list). On the previous code Gate was an
/// unknown spell and Bind Affinity had no arm.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gate_returns_the_caster_to_their_bind_and_wipes_hate() {
    let h = start_both().await;
    let (a_session, a_char_id, _stale) =
        provision_client(&h.auth_url, "gater", "Gatrix", "Human", "Cleric").await;
    set_char_level(&h.db_url, a_char_id, 8).await;
    let pool = db::open(&h.db_url).await.expect("open pool");
    db::set_account_gm(&pool, "gater", true).await.expect("set is_gm");
    let a_token = request_world_token(&h.auth_url, &a_session, a_char_id).await;
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    a.pump_for(Duration::from_millis(300)).await;
    let mut seq: u32 = 1;

    // Bind about 10 m east of the spawn. The whole test stays on the clear
    // stretch of the +X axis (x under 28): past it the Gnoll Raider camp's
    // aggro reaches z = 0, and four gnolls on the caster interrupt a 5 s bar
    // every time (the channeling roll is real).
    walk_east(&mut a, &mut seq, 10.0).await;
    let bind_pos = own_position(&mut a, a_char_id).await.expect("the caster's own Position");
    a.send_cast_start("Bind Affinity", 3.0);
    a.pump_for(Duration::from_millis(3_200)).await;
    a.send_cast_spell("Bind Affinity", None);
    let bound = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::ChatMessage { text, .. } if text.contains("bound"))
        })
        .await;
    assert!(bound.is_some(), "Bind Affinity binds the caster where they stand");

    // Walk on, and provoke a mob so it holds the caster as its target. The
    // dummy cannot move or hit (speed 0, dmg 0), so the bar is never
    // interrupted; its hate is the only thing being tested.
    walk_east(&mut a, &mut seq, 10.0).await;
    let far_pos = own_position(&mut a, a_char_id).await.expect("the caster's own Position");
    assert!(
        far_pos.x - bind_pos.x > 7.0,
        "the caster walked on from the bind ({} to {})",
        bind_pos.x, far_pos.x
    );
    a.send_dev_spawn("Charm Dummy", 1, 500.0, 0, 0.0, 1.0);
    let spawn = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::EnemySpawn { mob_name, .. } if mob_name == "Charm Dummy")
        })
        .await
        .expect("the dummy spawns");
    let dummy: u64 = match spawn {
        ServerWorldMsg::EnemySpawn { id, .. } => id,
        _ => unreachable!(),
    };
    a.send_cast_spell("Smite", Some(dummy));
    let provoked = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::EntityTarget { id, target: Some(t) }
                if *id == dummy && *t == a_char_id as u64)
        })
        .await;
    assert!(provoked.is_some(), "the dummy turns on the caster");

    // Gate: a 5 s bar, then the server moves the caster to the bind and the
    // dummy drops them. The two messages land in the same tick in either
    // order (wait_for discards what it skips), so take them as they come.
    // The Smite started the global cooldown; let it pass first.
    a.pump_for(Duration::from_millis(2_100)).await;
    a.send_cast_start("Gate", 5.0);
    a.pump_for(Duration::from_millis(5_200)).await;
    a.send_cast_spell("Gate", None);
    let mut landed: Option<Vec3> = None;
    let mut dropped = false;
    for _ in 0..2 {
        let msg = a
            .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
                matches!(m, ServerWorldMsg::Teleport { .. })
                    || matches!(m, ServerWorldMsg::EntityTarget { id, target: None } if *id == dummy)
            })
            .await;
        match msg {
            Some(ServerWorldMsg::Teleport { pos }) => landed = Some(pos),
            Some(ServerWorldMsg::EntityTarget { .. }) => dropped = true,
            _ => break,
        }
    }
    let Some(pos) = landed else {
        panic!("Gate teleports the caster");
    };
    assert!(
        (pos.x - bind_pos.x).abs() < 1.0 && (pos.z - bind_pos.z).abs() < 1.0,
        "Gate lands at the bind ({}, {}), got ({}, {})",
        bind_pos.x, bind_pos.z, pos.x, pos.z
    );
    assert!(dropped, "the mob drops the caster as its target: no train");

    // A second Gate inside the 300 s cooldown is refused (past the global
    // cooldown, so the refusal is the spell's own).
    a.pump_for(Duration::from_millis(2_100)).await;
    a.send_cast_start("Gate", 5.0);
    a.pump_for(Duration::from_millis(5_200)).await;
    a.send_cast_spell("Gate", None);
    let refused = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::CastFail { caster, reason }
                if *caster == a_char_id as u64 && reason.contains("cooldown"))
        })
        .await;
    assert!(refused.is_some(), "a second Gate inside the cooldown is refused");
}

/// Ledger item 2: the class gate. A Warrior naming Gate is refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_warrior_cannot_gate() {
    let h = start_both().await;
    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "nogate", "Grunt", "Human", "Warrior").await;
    set_char_level(&h.db_url, a_char_id, 10).await;
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    a.pump_for(Duration::from_millis(300)).await;
    a.send_cast_start("Gate", 5.0);
    a.pump_for(Duration::from_millis(5_200)).await;
    a.send_cast_spell("Gate", None);
    let refused = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::CastFail { caster, reason }
                if *caster == a_char_id as u64 && reason.contains("class"))
        })
        .await;
    assert!(refused.is_some(), "a Warrior cannot cast Gate");
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
    assert!(refused.is_some(), "a group member outside the safe area cannot be bound");

    a.send_cast_start("Bind Affinity", 3.0);
    pump_all_for(&mut [&mut a, &mut b, &mut c], Duration::from_millis(3_200)).await;
    a.send_cast_spell("Bind Affinity", Some(c_char_id as u64));
    // A send only queues; the caster's transport has to be serviced for the
    // message to leave before another seat is watched for the answer.
    a.pump_for(Duration::from_millis(300)).await;
    let bound = c
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::ChatMessage { text, .. } if text.contains("bound"))
        })
        .await;
    assert!(bound.is_some(), "a group member in the town square is bound where they stand");

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

/// Ledger item 12 (user call D5): Evacuate moves the caster and the alive
/// group members within friendly reach to the safe area's arrival point, and
/// nobody else. B (grouped, 10 m off) arrives with A; C (not grouped, 5 m off)
/// stays.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn evacuate_moves_the_group_in_reach_and_nobody_else() {
    let h = start_both().await;
    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "evac", "Evalyn", "Human", "Druid").await;
    let (b_session, b_char_id, b_token) =
        provision_client(&h.auth_url, "evacmate", "Mateo", "Human", "Warrior").await;
    let (c_session, c_char_id, c_token) =
        provision_client(&h.auth_url, "evacstranger", "Stray", "Human", "Warrior").await;
    let (d_session, d_char_id, d_token) =
        provision_client(&h.auth_url, "evacfar", "Farrow", "Human", "Warrior").await;
    set_char_level(&h.db_url, a_char_id, 16).await;
    // West of the square on the z = 0 line, which every camp keeps clear
    // (the Bonepile's nearest spawn is 12 m off it); the +X side past x = 28
    // is the Gnoll Raider camp, and a mob on the caster interrupts the bar.
    // D is grouped but 40 m out, past the 30 m friendly reach.
    set_char_pos(&h.db_url, a_char_id, -15.0, 0.0).await;
    set_char_pos(&h.db_url, b_char_id, -25.0, 0.0).await;
    set_char_pos(&h.db_url, c_char_id, -20.0, 0.0).await;
    set_char_pos(&h.db_url, d_char_id, -55.0, 0.0).await;
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    let mut b = WorldClient::start(b_token, &b_session, b_char_id).await;
    let mut c = WorldClient::start(c_token, &c_session, c_char_id).await;
    let mut d = WorldClient::start(d_token, &d_session, d_char_id).await;
    for cl in [&mut a, &mut b, &mut c, &mut d] {
        cl.pump_for(Duration::from_millis(300)).await;
    }
    a.send_group_invite("Mateo");
    a.pump_for(Duration::from_millis(300)).await;
    b.pump_for(Duration::from_millis(300)).await;
    b.send_group_accept(a_char_id as u64);
    a.send_group_invite("Farrow");
    a.pump_for(Duration::from_millis(300)).await;
    d.pump_for(Duration::from_millis(300)).await;
    d.send_group_accept(a_char_id as u64);
    for cl in [&mut a, &mut b, &mut c, &mut d] {
        cl.pump_for(Duration::from_millis(400)).await;
    }

    a.send_cast_start("Evacuate", 5.0);
    pump_all_for(&mut [&mut a, &mut b, &mut c, &mut d], Duration::from_millis(5_200)).await;
    a.send_cast_spell("Evacuate", None);
    let a_tp = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::Teleport { .. })
        })
        .await;
    let Some(ServerWorldMsg::Teleport { pos }) = a_tp else {
        panic!("Evacuate moves the caster");
    };
    assert!(pos.x.abs() < 1.0 && pos.z.abs() < 1.0, "to the town square, got ({}, {})", pos.x, pos.z);
    let b_tp = b
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::Teleport { .. })
        })
        .await;
    assert!(b_tp.is_some(), "the grouped member in reach comes along");
    let c_tp = c
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(1), |m| {
            matches!(m, ServerWorldMsg::Teleport { .. })
        })
        .await;
    assert!(c_tp.is_none(), "a stranger standing beside them stays");
    let d_tp = d
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(1), |m| {
            matches!(m, ServerWorldMsg::Teleport { .. })
        })
        .await;
    assert!(d_tp.is_none(), "a grouped member 40 m out, past friendly reach, stays");
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
    sqlx::query("UPDATE characters SET level = 10, hp = 40.0, mp = 99999.0 WHERE id = ?1")
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
    // 3.5 s, not 2 s: at 2 s the player only clips camp 0's aggro radius, so
    // whether an enemy locks on before the wait expires depended on where it
    // happened to be wandering. Walking fully in makes the pull deterministic.
    // Walk in on the server's own word, not the wall clock (a slow test
    // thread used to come up short): to 2 m from the spawn point, well
    // inside its aggro whatever the spawn jitter.
    let mut seq: u32 = 1;
    walk_until_within(&mut a, &mut seq, a_char_id, Vec3 { x: 0.0, y: 0.0, z: 0.0 }, BONEPILE_SPAWN, 2.0).await;

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
    // regen-this-tick. Lifetap Rk. II heals 20 (the parity audit's number);
    // we require at least +10 to stay comfortably above any regen drift the
    // tick loop ran in the meantime.
    let post = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(
                m,
                ServerWorldMsg::HealthUpdate { id, hp, .. }
                    if *id == a_char_id as u64 && *hp >= baseline_hp + 10.0
            )
        })
        .await
        .expect("caster HealthUpdate with lifesteal heal");
    if let ServerWorldMsg::HealthUpdate { hp, .. } = post {
        assert!(
            hp >= baseline_hp + 10.0,
            "expected hp >= {} (baseline {} + at least 10 of the 20 lifesteal); got {}",
            baseline_hp + 10.0,
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
    // EQ's formula (the parity audit, 2026-10-20): 5 + 220 / 10 + 20 x 220 / 3000.
    const HUMAN_WARRIOR_BASE_MAX_HP: f32 = 28.0;
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
            "expected max_hp = {EXPECTED_MAX_HP} (base 28 + 25 from vest), got {max_hp}"
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
        provision_client(&h.auth_url, "cdc", "Cooler", "Human", "Enchanter").await;
    // Rune requires level 4; provisioned characters start at 1.
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;

    for _ in 0..4 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }

    // First cast â€” full gate flow (CastStart â†’ wait â†’ CastSpell).
    a.send_cast_start("Rune", 0.0);
    for _ in 0..3 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }
    a.pump_for(Duration::from_millis(1100)).await;
    a.send_cast_spell("Rune", None);
    for _ in 0..6 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }

    // Confirm the first cast landed (BuffSnapshot includes Rune).
    let _snap = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::BuffSnapshot { target, buffs }
                if *target == a_char_id as u64
                    && buffs.iter().any(|(n, _)| n == "Rune"))
        })
        .await
        .expect("the first Rune lands (its absorb shows in the snapshot)");

    // The global cooldown (2 s) has to pass, or the second start itself is
    // refused and the per-spell cooldown is never reached.
    a.pump_for(Duration::from_millis(2_100)).await;

    // Second cast â€” same full flow, fired well inside the 6 s
    // cooldown window. Should be rejected with the cooldown reason.
    a.send_cast_start("Rune", 0.0);
    for _ in 0..3 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }
    a.pump_for(Duration::from_millis(1100)).await;
    a.send_cast_spell("Rune", None);
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
    // Walk forward for the full cast duration. RUN_SPEED x 1.1 s
    // = 5.5 m on the server, well past the 1 m gate.
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
    master_weapon_skill(&h.db_url, b_char_id, "hand_to_hand").await;
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
    // The server rolls the miss now (combat.rs), and only a hit interrupts.
    // B's hand_to_hand is mastered (a 5% miss), and B swings again, at a
    // legal cadence, until a hit lands inside A's 5 s cast: four misses in a
    // row is about one in 160,000.
    let mut fail = None;
    for _ in 0..4 {
        b.send_attack(a_char_id as u64, "", false, DamageType::Physical);
        for _ in 0..2 {
            tick_one(&mut a.client, &mut a.transport);
            tick_one(&mut b.client, &mut b.transport);
            tokio::time::sleep(TICK_DT).await;
        }
        fail = a
            .wait_for(CHANNEL_SYSTEM, Duration::from_millis(1_050), |m| {
                matches!(m, ServerWorldMsg::CastFail { caster, reason }
                    if *caster == a_char_id as u64
                        && reason == "interrupted (hit during cast)")
            })
            .await;
        if fail.is_some() {
            break;
        }
    }
    let fail = fail.expect("Warrior with no channeling skill is always interrupted on incoming hit");
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

/// Makes `username` a GM and mints a fresh connect token carrying the flag (it
/// is packed at mint time). A test that kills a player with `send_death` needs
/// it: `DeathBroadcast` is a dev command since 2026-10-23 (housekeeping audit B1,
/// `docs/design/server_fall_damage.md`), as it is for the Test Panel's Trigger
/// Death in play.
async fn gm_world_token(
    auth_url: &str,
    db_url: &str,
    username: &str,
    session_token_hex: &str,
    char_id: i64,
) -> Vec<u8> {
    let pool = db::open(db_url).await.expect("open pool");
    db::set_account_gm(&pool, username, true).await.expect("set is_gm");
    request_world_token(auth_url, session_token_hex, char_id).await
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
        eprintln!("skipping death_broadcast_is_a_dev_command: PD_DEV_CMDS=1 makes every connection dev");
        return;
    }
    let h = start_both().await;
    let pool = db::open(&h.db_url).await.expect("open pool");
    let death_rows = |pool: sqlx::SqlitePool| async move {
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM gm_actions WHERE command = 'death_broadcast'")
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
    assert!(died.is_none(), "a plain account's DeathBroadcast must not kill it");
    assert_eq!(death_rows(pool.clone()).await, 0, "a refused dev command writes no audit row");

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
    assert_eq!(death_rows(pool.clone()).await, 1, "one death, one audit row");
}

const DEAD_LINE: &str = "You cannot do that while dead.";

fn dead_lines(msgs: &[ServerWorldMsg]) -> usize {
    system_lines(msgs).iter().filter(|l| l.as_str() == DEAD_LINE).count()
}

/// The dead-state gate says its line once per death, not once per refused
/// message (2026-10-26: Trigger Death filled the chat with it eight times),
/// and lets a target change through: clicking things while dead is natural and
/// harmless. A new death says it again. On the previous gate the target change
/// drew a line and three refused messages drew three.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_dead_are_told_once_per_death() {
    let h = start_both().await;
    let (session, char_id, _stale) =
        provision_client(&h.auth_url, "deadonce", "Onceler", "Human", "Warrior").await;
    let token = gm_world_token(&h.auth_url, &h.db_url, "deadonce", &session, char_id).await;
    let mut a = WorldClient::start(token, &session, char_id).await;
    let me = char_id as u64;

    let die = |a: &mut WorldClient| a.send_damage_self(1_000_000);
    die(&mut a);
    a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::HealthUpdate { id, hp, .. } if *id == me && *hp <= 0.01)
    })
    .await
    .expect("the dev damage kills");
    a.collect_for(Duration::from_millis(500)).await;

    send_msg(&mut a.client, CHANNEL_SYSTEM, &ClientWorldMsg::SetTarget { target_id: None });
    let msgs = a.collect_for(Duration::from_millis(800)).await;
    assert_eq!(dead_lines(&msgs), 0, "a target change is allowed while dead");

    for _ in 0..3 {
        a.send_sit();
    }
    let msgs = a.collect_for(Duration::from_millis(1200)).await;
    assert_eq!(dead_lines(&msgs), 1, "three refused messages, one line; got {:?}", system_lines(&msgs));

    a.send_sit();
    let msgs = a.collect_for(Duration::from_millis(800)).await;
    assert_eq!(dead_lines(&msgs), 0, "still the same death: no more lines");

    // Alive again, then a new death: told once more.
    a.send_respawn();
    a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::HealthUpdate { id, hp, .. } if *id == me && *hp > 0.01)
    })
    .await
    .expect("the respawn restores some HP");
    die(&mut a);
    a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::HealthUpdate { id, hp, .. } if *id == me && *hp <= 0.01)
    })
    .await
    .expect("the second death");
    a.collect_for(Duration::from_millis(500)).await;
    a.send_sit();
    a.send_sit();
    let msgs = a.collect_for(Duration::from_millis(1200)).await;
    assert_eq!(dead_lines(&msgs), 1, "a new death is told once again");
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
    let (session, char_id, _stale) =
        provision_client(&h.auth_url, "respawner", "Respawna", "Human", "Warrior").await;
    let token = gm_world_token(&h.auth_url, &h.db_url, "respawner", &session, char_id).await;
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
/// resumes after it. The line is said once per death (2026-10-26), so each
/// intent is proven refused by its line in a death of its own, and a second
/// refused intent in the same death draws no second line.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dead_players_cannot_loot_or_touch_inventory() {
    let h = start_both().await;
    let (a_session, a_char_id, _a_stale) =
        provision_client(&h.auth_url, "dedguy", "Deddy", "Human", "Warrior").await;
    let a_token = gm_world_token(&h.auth_url, &h.db_url, "dedguy", &a_session, a_char_id).await;
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

    // A second refused intent in the same death draws no second line.
    a.send_loot_all(999_999);
    let again = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_millis(1500), |m| {
            matches!(m, ServerWorldMsg::ChatMessage { text, .. }
                if text.contains("while dead"))
        })
        .await;
    assert!(again.is_none(), "the line is said once per death");

    // Respawn is still allowed and clears the gate: the bind/spawn Teleport
    // arrives.
    a.send_respawn();
    let tp = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(5), |m| {
            matches!(m, ServerWorldMsg::Teleport { .. })
        })
        .await;
    assert!(tp.is_some(), "Respawn must still work for the dead");

    // A second death: the loot intent, first this time, is refused with the
    // line, BEFORE any bag lookup (the id need not exist).
    a.send_death();
    for _ in 0..4 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }
    a.send_loot_all(999_999);
    let refusal2 = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::ChatMessage { text, .. }
                if text.contains("while dead"))
        })
        .await;
    assert!(refusal2.is_some(), "dead LootAll must be refused with a chat line");

    // Respawn again; a subsequent inventory intent is no longer dead-refused.
    a.send_respawn();
    let tp = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(5), |m| {
            matches!(m, ServerWorldMsg::Teleport { .. })
        })
        .await;
    assert!(tp.is_some(), "Respawn must still work after the second death");

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
    let (b_session, b_char_id, _b_stale) =
        provision_client(&h.auth_url, "xpdead", "Xpdead", "Elf", "Cleric").await;

    master_weapon_skill(&h.db_url, a_char_id, "hand_to_hand").await;
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    let b_token = gm_world_token(&h.auth_url, &h.db_url, "xpdead", &b_session, b_char_id).await;
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

    // The refusal arrives as a private CastFail (the client's signal to undo
    // its optimistic spend and cooldown).
    let refusal = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::CastFail { reason, .. }
                if reason.contains("need a target"))
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

/// Review 2026-10-12: an offer is a reference to `bag_<k>` slot `s`, and a
/// full bag moves with its contents (re-keyed to its new base slot) since
/// 2026-09-27. Swapping the PARENT bag was not locked, so after B accepted,
/// A could swap two pouches and the reference would name the other pouch's
/// item: the count still matched (1 = 1) and B's window never refreshed.
/// The scam offers the good sword and swaps in the junk; this test runs the
/// mirror (junk shown, so receiving the good sword proves the re-point). The
/// parent bag's base slot is locked now, and the commit checks the item too.
/// Fails on the previous code at the refusal, and without that assertion,
/// B receives the good sword.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn trade_locks_the_bag_holding_an_offered_item() {
    let h = start_both().await;
    let (mut a, _a_char_id, mut b, b_char_id, _bread_loc, _bread_slot) =
        trade_pair(&h, "tradeg", "Tradegan", "tradeh", "Tradehal").await;

    macro_rules! pump_a {
        () => {{
            for _ in 0..4 {
                tick_one(&mut a.client, &mut a.transport);
                tokio::time::sleep(TICK_DT).await;
            }
        }};
    }
    // Wait for one item to land and return its (location, slot).
    async fn landed(c: &mut WorldClient, needle: &str) -> (String, u32) {
        match c
            .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
                matches!(m, ServerWorldMsg::InventoryDelta { item_path: Some(p), .. }
                    if p.contains(needle))
            })
            .await
            .expect("the item lands")
        {
            ServerWorldMsg::InventoryDelta { location, slot, .. } => (location, slot),
            _ => unreachable!(),
        }
    }

    // Two pouches in base slots, a junk sword in one, a good sword in the other.
    a.send_gm_command("give Small Pouch");
    let (_, p1) = landed(&mut a, "small_pouch").await;
    a.send_gm_command("give Small Pouch");
    let (_, p2) = landed(&mut a, "small_pouch").await;
    a.send_gm_command("give Rusty Short Sword");
    let (jl, js) = landed(&mut a, "rusty_short_sword").await;
    a.send_gm_command("give Iron Short Sword");
    let (gl, gs) = landed(&mut a, "iron_short_sword").await;
    let junk_bag = format!("bag_{p1}");
    let good_bag = format!("bag_{p2}");
    a.send_move_item(&jl, js, &junk_bag, 0);
    a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::InventoryDelta { location, slot: 0, item_path: Some(p), .. }
            if *location == junk_bag && p.contains("rusty_short_sword"))
    })
    .await
    .expect("the junk sword goes into the first pouch");
    a.send_move_item(&gl, gs, &good_bag, 0);
    a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::InventoryDelta { location, slot: 0, item_path: Some(p), .. }
            if *location == good_bag && p.contains("iron_short_sword"))
    })
    .await
    .expect("the good sword goes into the second pouch");

    // A offers the junk sword; B sees it and accepts.
    a.send_trade_request(b_char_id as u64);
    pump_a!();
    b.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::TradeOpened { .. })
    })
    .await
    .expect("B's window opens");
    a.send_trade_offer_item(0, &junk_bag, 0);
    pump_a!();
    b.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::TradeOfferUpdate { mine: false, slots, .. }
            if slots.first().map(|(p, c)| p.contains("rusty_short_sword") && *c == 1).unwrap_or(false))
    })
    .await
    .expect("B sees the junk sword offered");
    b.send_trade_accept();
    for _ in 0..4 {
        tick_one(&mut b.client, &mut b.transport);
        tokio::time::sleep(TICK_DT).await;
    }

    // The switch: swap the two pouches. The pouch holding the offer is locked.
    a.send_move_item("base", p1, "base", p2);
    a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::ChatMessage { text, .. }
            if text.contains("offered in a trade"))
    })
    .await
    .expect("moving the bag that holds an offered item is refused");

    // A accepts; B gets exactly what B saw, never the good sword.
    a.send_trade_accept();
    pump_a!();
    let mut got_junk = false;
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        let Some(m) = b
            .wait_for(CHANNEL_SYSTEM, Duration::from_millis(500), |m| {
                matches!(m, ServerWorldMsg::InventoryDelta { item_path: Some(_), .. }
                    | ServerWorldMsg::TradeClosed { .. })
            })
            .await
        else {
            continue;
        };
        match m {
            ServerWorldMsg::InventoryDelta { item_path: Some(p), .. } => {
                assert!(!p.contains("iron_short_sword"), "B must never receive the good sword");
                if p.contains("rusty_short_sword") {
                    got_junk = true;
                }
            }
            ServerWorldMsg::TradeClosed { committed, .. } => {
                assert!(committed, "the trade commits");
                break;
            }
            _ => {}
        }
    }
    assert!(got_junk, "B receives the junk sword it was shown");
}

/// Review 2026-10-12: loot, purchases and grants place through the server's
/// placer, which can top up an offered stack. The commit then refuses, but the
/// window kept showing the old snapshot and the stack stayed locked, so every
/// later accept failed the same way with nothing visibly wrong. A changed
/// offer now leaves the window (which unlocks it); offering it again commits.
/// Fails on the previous code at "the changed offer leaves A's window".
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn trade_drops_an_offer_that_changed_so_the_trade_can_go_on() {
    let h = start_both().await;
    let (mut a, _a_char_id, mut b, b_char_id, bread_loc, bread_slot) =
        trade_pair(&h, "tradei", "Tradeira", "tradej", "Tradejon").await;
    macro_rules! pump {
        ($c:expr) => {{
            for _ in 0..4 {
                tick_one(&mut $c.client, &mut $c.transport);
                tokio::time::sleep(TICK_DT).await;
            }
        }};
    }

    a.send_trade_request(b_char_id as u64);
    pump!(a);
    b.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::TradeOpened { .. })
    })
    .await
    .expect("B's window opens");
    a.send_trade_offer_item(0, &bread_loc, bread_slot);
    a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::TradeOfferUpdate { mine: true, slots, .. }
            if slots.first().map(|(p, c)| p.contains("bread") && *c == 3).unwrap_or(false))
    })
    .await
    .expect("A offers 3 bread");

    // A grant tops the offered stack up to 4 through the placer.
    a.send_gm_command("give Bread Loaf 1");
    a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::InventoryDelta { location, slot, count: 4, .. }
            if *location == bread_loc && *slot == bread_slot)
    })
    .await
    .expect("the grant merges into the offered stack");

    b.send_trade_accept();
    pump!(b);
    a.send_trade_accept();
    a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::ChatMessage { text, .. } if text.contains("changed"))
    })
    .await
    .expect("the commit refuses the changed offer, with a line");
    a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::TradeOfferUpdate { mine: true, slots, .. }
            if slots.first().map(|(p, _)| p.is_empty()).unwrap_or(false))
    })
    .await
    .expect("the changed offer leaves A's window");

    // Offered again, the trade goes through with what B now sees.
    a.send_trade_offer_item(0, &bread_loc, bread_slot);
    pump!(a);
    b.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::TradeOfferUpdate { mine: false, slots, .. }
            if slots.first().map(|(p, c)| p.contains("bread") && *c == 4).unwrap_or(false))
    })
    .await
    .expect("B sees the re-offer at 4");
    b.send_trade_accept();
    pump!(b);
    a.send_trade_accept();
    pump!(a);
    b.wait_for(CHANNEL_SYSTEM, Duration::from_secs(5), |m| {
        matches!(m, ServerWorldMsg::InventoryDelta { item_path: Some(p), count: 4, .. }
            if p.contains("bread"))
    })
    .await
    .expect("B receives all 4");
    b.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::TradeClosed { committed: true, .. })
    })
    .await
    .expect("the trade commits");
}

/// The player target broadcast (target-of-target) was dead on the client
/// from 2026-05-29 to 2026-10-12, so the server path was never exercised in
/// play. Reviving it, the server fanned every SetTarget to EVERY player in the
/// world, once per message: a modified client could amplify a burst to the
/// whole server, and anyone anywhere learned who was fighting what. It now
/// goes only to players who can see the sender, at most once per tick (the
/// latest). Fails on the previous code: the far player receives it, and the
/// near one receives the whole burst.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_player_target_reaches_only_those_who_can_see_them_once_per_tick() {
    let h = start_both().await;
    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "tgta", "Targeta", "Human", "Warrior").await;
    let (b_session, b_char_id, b_token) =
        provision_client(&h.auth_url, "tgtb", "Targetb", "Human", "Warrior").await;
    let (c_session, c_char_id, c_token) =
        provision_client(&h.auth_url, "tgtc", "Targetc", "Human", "Warrior").await;
    // C two cells east of spawn: outside A's 3x3, so C cannot see A.
    set_char_pos(&h.db_url, c_char_id, 248.0, 0.0).await;
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    let mut b = WorldClient::start(b_token, &b_session, b_char_id).await;
    let mut c = WorldClient::start(c_token, &c_session, c_char_id).await;
    a.pump_for(Duration::from_millis(300)).await;
    b.pump_for(Duration::from_millis(300)).await;
    c.pump_for(Duration::from_millis(300)).await;

    // A burst of five retargets in one go; the last names B.
    for t in [9001u64, 9002, 9003, 9004, b_char_id as u64] {
        send_msg(&mut a.client, CHANNEL_SYSTEM, &ClientWorldMsg::SetTarget { target_id: Some(t) });
    }
    for _ in 0..4 {
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }

    // B (beside A) hears A's target exactly once, and it is the last one.
    let mut seen: Vec<Option<u64>> = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if let Some(ServerWorldMsg::EntityTarget { target, .. }) = b
            .wait_for(CHANNEL_SYSTEM, Duration::from_millis(300), |m| {
                matches!(m, ServerWorldMsg::EntityTarget { id, .. } if *id == a_char_id as u64)
            })
            .await
        {
            seen.push(target);
        }
    }
    assert_eq!(seen, vec![Some(b_char_id as u64)], "B gets the latest target, once");

    // C (two cells away) hears nothing about A.
    let far = c
        .wait_for(CHANNEL_SYSTEM, Duration::from_millis(800), |m| {
            matches!(m, ServerWorldMsg::EntityTarget { id, .. } if *id == a_char_id as u64)
        })
        .await;
    assert!(far.is_none(), "a player who cannot see A must not learn A's target");
}

/// Food and water's auto-eat takes the first food in slot order when a
/// character runs out. Merged onto the trade branch, it did not consult the
/// trade lock, so a loaf offered in an open trade could be eaten mid-trade (the
/// commit then refused the changed offer, so no dupe, but the offer was gone).
/// Auto-eat now skips offered slots. Fails on the previous code: the delta
/// names the offered base slot 3.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auto_eat_skips_food_offered_in_a_trade() {
    let h = start_both().await;
    let (a_session, a_char_id, _) =
        provision_client(&h.auth_url, "eata", "Eatalot", "Human", "Warrior").await;
    let (b_session, b_char_id, b_token) =
        provision_client(&h.auth_url, "eatb", "Eatbee", "Human", "Warrior").await;
    // Six seconds of food left, plenty of water; bread in base 3 and base 5.
    set_char_appetite(&h.db_url, a_char_id, 6.0, 5000.0, None).await;
    give_char_item(&h.db_url, a_char_id, "base", 3, BREAD, 1).await;
    give_char_item(&h.db_url, a_char_id, "base", 5, BREAD, 1).await;
    let a_token = request_world_token(&h.auth_url, &a_session, a_char_id).await;
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    let mut b = WorldClient::start(b_token, &b_session, b_char_id).await;
    a.pump_for(Duration::from_millis(300)).await;
    b.pump_for(Duration::from_millis(300)).await;

    // A opens a trade and offers the loaf in base 3 before the food runs out.
    a.send_trade_request(b_char_id as u64);
    a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::TradeOpened { .. })
    })
    .await
    .expect("the trade opens");
    a.send_trade_offer_item(0, "base", 3);
    a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::TradeOfferUpdate { mine: true, slots, .. }
            if slots.first().map(|(p, _)| p.contains("bread")).unwrap_or(false))
    })
    .await
    .expect("the loaf in base 3 is offered");

    // The food runs out: the auto-eat takes base 5, never the offered base 3.
    let mut eaten: Option<u32> = None;
    let end = Instant::now() + Duration::from_secs(12);
    while Instant::now() < end && eaten.is_none() {
        a.send_heartbeat();
        b.send_heartbeat();
        b.pump_for(Duration::from_millis(50)).await;
        if let Some(ServerWorldMsg::InventoryDelta { slot, .. }) = a
            .wait_for(CHANNEL_SYSTEM, Duration::from_secs(2), |m| {
                matches!(m, ServerWorldMsg::InventoryDelta { location, .. } if location == "base")
            })
            .await
        {
            eaten = Some(slot);
        }
    }
    assert_eq!(eaten, Some(5), "the auto-eat takes the loaf that is not offered");
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
// west (40 Moves at 5 m/s is 10 m, plus the ~2.5 m the server keeps
// integrating after the last one) crosses the x=240 line into cell (1,0),
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

/// `pump_both_for` for any number of seats: a three-client test whose
/// caster holds a 5 s bar twice would otherwise run the silent seats past
/// the 10 s app-layer heartbeat timeout.
async fn pump_all_for(clients: &mut [&mut WorldClient], d: Duration) {
    for c in clients.iter_mut() {
        c.send_heartbeat();
    }
    let end = Instant::now() + d;
    while Instant::now() < end {
        for c in clients.iter_mut() {
            tick_one(&mut c.client, &mut c.transport);
        }
        tokio::time::sleep(TICK_DT).await;
    }
}

/// The newest Position the server has fanned for the client's own
/// character, read over about a second of pumping; None if none arrives.
async fn own_position(c: &mut WorldClient, char_id: i64) -> Option<Vec3> {
    let deadline = Instant::now() + Duration::from_millis(1_000);
    let mut last = None;
    while Instant::now() < deadline {
        let left = deadline.saturating_duration_since(Instant::now());
        match c
            .wait_for(CHANNEL_POSITION, left, |m| {
                matches!(m, ServerWorldMsg::Position { id, .. } if *id == char_id as u64)
            })
            .await
        {
            Some(ServerWorldMsg::Position { pos, .. }) => last = Some(pos),
            _ => break,
        }
    }
    last
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
    // distance to the TARGET (aggro x 2), and B at 5 m/s opens the gap by
    // 2 m per wall-clock second of walking, so aggro is set to the 50 cap
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

/// The owner always sees their own pet (user call, 2026-10-02). B parks a
/// skeleton with `/pet guard` in cell (1,0) and walks west until B stands in
/// cell (-1,0), two cells from the pet and outside the 3x3 that decides what
/// anyone else sees. Before this change B's own cell crossing sent B an
/// EntityDespawn for the pet, so the pet panel and every `/pet` command went
/// dark until B walked back. Now: no despawn, the pet's Position keeps
/// arriving (the guard keepalive), and `/pet follow` from out there starts it
/// back, which B watches it do.
#[tokio::test]
async fn owner_keeps_seeing_a_pet_parked_two_cells_away() {
    let h = start_both().await;
    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "petfar_w", "Petfarw", "Human", "Warrior").await;
    let (b_session, b_char_id, b_token) =
        provision_client(&h.auth_url, "petfar_o", "Petfaro", "Human", "Necromancer").await;
    set_char_level(&h.db_url, b_char_id, 6).await;
    // B in cell (0,0) a metre short of the x=120 line; the summon lands the
    // pet 1.5 m east of B, in cell (1,0). Row z = 80: the walk west passes no
    // camp's aggro by 14 m. On row 60 it ended 7.4 m from a Cave Bat (aggro
    // 12), and since the EQ parity pass a level 6 Necromancer can die to the
    // bats before the pet arrives; a dead owner drops out of the AI's target
    // list and the pet stands still, which failed this test about one full
    // run in four.
    set_char_pos(&h.db_url, b_char_id, 119.0, 80.0).await;

    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    let mut b = WorldClient::start(b_token, &b_session, b_char_id).await;
    pump_both_for(&mut a, &mut b, Duration::from_millis(300)).await;

    let pet_id = summon_skeleton(&mut b, &mut a, b_char_id).await;
    b.send_pet_command(protocol::world::pet_command::GUARD, None);
    pump_both_for(&mut a, &mut b, Duration::from_millis(300)).await;

    // Walk B west until its own echoed Position is past x = -2 (cell -1),
    // draining B's channels the whole way: a pet despawn anywhere in here
    // is the old behaviour.
    let mut seq: u32 = 1;
    let mut b_x: f32 = 119.0;
    let mut pet_despawned = false;
    let mut last_pet_pos: Option<Vec3> = None;
    let west = Vec3 { x: -1.0, y: 0.0, z: 0.0 };
    let drain_b = |b: &mut WorldClient, b_x: &mut f32, pet_despawned: &mut bool, last_pet_pos: &mut Option<Vec3>| {
        while let Some(bytes) = b.client.receive_message(CHANNEL_POSITION) {
            if let Ok((msg, _)) =
                bincode::serde::decode_from_slice::<ServerWorldMsg, _>(&bytes, bincode_cfg())
            {
                if let ServerWorldMsg::Position { id, pos, .. } = msg {
                    if id == b_char_id as u64 {
                        *b_x = pos.x;
                    } else if id == pet_id {
                        *last_pet_pos = Some(pos);
                    }
                }
            }
        }
        while let Some(bytes) = b.client.receive_message(CHANNEL_SYSTEM) {
            if let Ok((msg, _)) =
                bincode::serde::decode_from_slice::<ServerWorldMsg, _>(&bytes, bincode_cfg())
            {
                if matches!(msg, ServerWorldMsg::EntityDespawn { id } if id == pet_id) {
                    *pet_despawned = true;
                }
            }
        }
    };
    let mut ticks: u32 = 0;
    while b_x > -2.0 {
        assert!(ticks < 800, "B never reached cell (-1,0): x = {b_x} after {ticks} moves");
        b.send_move(seq, west);
        seq += 1;
        ticks += 1;
        if ticks % 80 == 0 {
            a.send_heartbeat();
        }
        tick_one(&mut a.client, &mut a.transport);
        tick_one(&mut b.client, &mut b.transport);
        assert_both_connected(&a, &b, "the owner's walk west");
        drain_b(&mut b, &mut b_x, &mut pet_despawned, &mut last_pet_pos);
        tokio::time::sleep(TICK_DT).await;
    }
    assert!(!pet_despawned, "B was sent an EntityDespawn for its own pet on walking out of view");

    // Out here only the owner rule delivers the parked pet's keepalives:
    // the pet's cell (1,0) is not adjacent to B's (-1,0). Wait for the next
    // one rather than for a fixed window: under a loaded test run the
    // server's 500 ms keepalive can arrive late, and that is not the bug.
    last_pet_pos = None;
    let end = Instant::now() + Duration::from_secs(8);
    a.send_heartbeat();
    b.send_heartbeat();
    while Instant::now() < end && last_pet_pos.is_none() {
        tick_one(&mut a.client, &mut a.transport);
        tick_one(&mut b.client, &mut b.transport);
        assert_both_connected(&a, &b, "the wait for a parked keepalive");
        drain_b(&mut b, &mut b_x, &mut pet_despawned, &mut last_pet_pos);
        tokio::time::sleep(TICK_DT).await;
    }
    let parked = last_pet_pos.expect("the parked pet's Position must keep reaching its owner two cells away");
    assert!(
        (parked.x - 120.5).abs() < 1.0,
        "a guarding pet should still be where it was parked, got x = {}",
        parked.x
    );
    assert!(!pet_despawned, "B was sent an EntityDespawn for its own pet while parked out of view");

    // The point of the rule: the pet can be called back from out of view.
    // The proof is that the owner SEES it set off (a few metres west of
    // where it was parked), not that it covers a distance against a clock:
    // how far a pet gets in N wall-clock seconds depends on how loaded the
    // machine is, and a deadline on distance failed in a full parallel run.
    b.send_pet_command(protocol::world::pet_command::FOLLOW, None);
    let end = Instant::now() + Duration::from_secs(30);
    let mut came_back = false;
    let mut t: u32 = 0;
    while Instant::now() < end {
        tick_one(&mut a.client, &mut a.transport);
        tick_one(&mut b.client, &mut b.transport);
        assert_both_connected(&a, &b, "the pet's walk back");
        drain_b(&mut b, &mut b_x, &mut pet_despawned, &mut last_pet_pos);
        if last_pet_pos.is_some_and(|p| p.x < parked.x - 3.0) {
            came_back = true;
            break;
        }
        t += 1;
        if t % 80 == 0 {
            a.send_heartbeat();
            b.send_heartbeat();
        }
        tokio::time::sleep(TICK_DT).await;
    }
    assert!(
        came_back,
        "after /pet follow the owner must watch the pet set off toward them (last pet x = {:?})",
        last_pet_pos.map(|p| p.x)
    );
    assert!(!pet_despawned, "B was sent an EntityDespawn for its own pet during the walk back");
}

// ── The world clock ───────────────────────────────────────────────────────

/// Every player is told the hour as they enter the world, and two players
/// are told the same hour. Before this the variant existed on the wire with
/// no sender, so each client ran a private clock that restarted at 8 AM with
/// the game process, and no two friends shared a sky. B joins after A here,
/// so B is also the late-joiner case: seeded at once, not left waiting up to
/// a minute for the next broadcast.
#[tokio::test]
async fn every_player_is_told_the_same_hour_on_entering_the_world() {
    let h = start_both().await;
    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "skyone", "Skyone", "Human", "Warrior").await;
    let (b_session, b_char_id, b_token) =
        provision_client(&h.auth_url, "skytwo", "Skytwo", "Elf", "Cleric").await;

    let hour_of = |m: Option<ServerWorldMsg>, who: &str| -> f32 {
        match m {
            Some(ServerWorldMsg::TimeOfDay { hour }) => hour,
            _ => panic!("{who} was not told the hour on entering the world"),
        }
    };

    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    let a_hour = hour_of(
        a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::TimeOfDay { .. })
        })
        .await,
        "A",
    );
    let mut b = WorldClient::start(b_token, &b_session, b_char_id).await;
    let b_hour = hour_of(
        b.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::TimeOfDay { .. })
        })
        .await,
        "B",
    );

    for (who, hour) in [("A", a_hour), ("B", b_hour)] {
        assert!((0.0..24.0).contains(&hour), "{who} was told hour {hour}, outside the day");
    }
    // B entered a second or two after A; a game hour is 50 real seconds, so
    // the two differ by a few hundredths of an hour at most. Compare around
    // the clock so a midnight between the two sends does not read as 24.
    let apart = (b_hour - a_hour).rem_euclid(24.0);
    let apart = apart.min(24.0 - apart);
    assert!(
        apart < 0.5,
        "A was told {a_hour} and B {b_hour}: the same clock should not differ by {apart} hours"
    );
    assert_both_connected(&a, &b, "the world clock test");
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
    let (a_session, a_char_id, _stale_a) =
        provision_client(&h.auth_url, "farench", "Farench", "Human", "Enchanter").await;
    set_char_level(&h.db_url, a_char_id, 20).await;
    let (b_session, b_char_id, _stale_b) =
        provision_client(&h.auth_url, "farspawn", "Farspawn", "Human", "Warrior").await;
    let pool = db::open(&h.db_url).await.expect("open pool");
    // Both are GMs: B conjures the far dummy, A conjures a near one at the end.
    db::set_account_gm(&pool, "farench", true).await.expect("set is_gm");
    db::set_account_gm(&pool, "farspawn", true).await.expect("set is_gm");
    set_char_pos(&h.db_url, b_char_id, 248.0, 60.0).await;
    let a_token = request_world_token(&h.auth_url, &a_session, a_char_id).await;
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
        matches!(m, ServerWorldMsg::CastFail { reason, .. } if reason.contains("too far away"))
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

    // A refused cast costs NOTHING, cooldown included. Charm's cooldown is
    // 60 s; when refusals were handled after the stamp, the cast above would
    // have locked Charm out for a minute. A charms a dummy at its feet at once.
    a.send_dev_spawn("Near Dummy", 1, 30.0, 0, 0.0, 0.0);
    let near_evt = wait_on_a_pumping_b(&mut a, &mut b, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::EnemySpawn { mob_name, .. } if mob_name == "Near Dummy")
    })
    .await
    .expect("the near dummy spawns beside A");
    let near_id: u64 = match near_evt {
        ServerWorldMsg::EnemySpawn { id, .. } => id,
        _ => unreachable!(),
    };
    a.send_cast_start("Charm", 2.0);
    pump_both_for(&mut a, &mut b, Duration::from_millis(2150)).await;
    a.send_cast_spell("Charm", Some(near_id));
    wait_on_a_pumping_b(&mut a, &mut b, Duration::from_secs(5), |m| {
        matches!(m, ServerWorldMsg::PetSpawn { owner, .. } if *owner == a_char_id as u64)
    })
    .await
    .expect("the refused cast burned no cooldown: the next Charm lands");
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
    let moves = (18.0 / (world::RUN_SPEED * 0.05)).ceil() as u32;
    walk_pumping(&mut b, &mut a, &mut seq, dir, moves).await;
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


// ── Cast target pre-flight: reach, and refusals that cost nothing ─────────

/// A heal had no range check at all: a healer could keep a friend alive in a
/// dungeon from the safety of town by naming their char id. B is hurt and
/// two cells away; A's heal must be refused, B must not be healed, and A must
/// not pay for it in mana or in cooldown.
#[tokio::test]
async fn ally_heal_refuses_out_of_range_and_costs_nothing() {
    let h = start_both().await;
    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "farheal", "Farheal", "Human", "Shaman").await;
    // Healing Wave is a level 4 Shaman spell: 1.0 s cast, 30 mana, 6 s cooldown.
    set_char_level(&h.db_url, a_char_id, 4).await;
    let (b_session, b_char_id, _stale_token) =
        provision_client(&h.auth_url, "farhurt", "Farhurt", "Human", "Warrior").await;
    let pool = db::open(&h.db_url).await.expect("open pool");
    db::set_account_gm(&pool, "farhurt", true).await.expect("set is_gm");
    // (248, 60): two cells from spawn and clear of every camp. Not (200, 0),
    // which sits inside the Ancient Wraith's aggro circle.
    set_char_pos(&h.db_url, b_char_id, 248.0, 60.0).await;
    let b_token = request_world_token(&h.auth_url, &b_session, b_char_id).await;

    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    let mut b = WorldClient::start(b_token, &b_session, b_char_id).await;
    pump_both_for(&mut a, &mut b, Duration::from_millis(300)).await;

    // Hurt B (a dev command, so B is a GM) and latch the damaged value.
    b.send_damage_self(40);
    let hurt_evt = wait_on_a_pumping_b(&mut b, &mut a, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::HealthUpdate { id, hp, max_hp }
            if *id == b_char_id as u64 && *hp < *max_hp)
    })
    .await
    .expect("B takes the dev damage");
    let hurt_hp = match hurt_evt {
        ServerWorldMsg::HealthUpdate { hp, .. } => hp,
        _ => unreachable!(),
    };

    a.send_cast_start("Healing Wave", 1.0);
    pump_both_for(&mut a, &mut b, Duration::from_millis(1150)).await;
    a.send_cast_spell("Healing Wave", Some(b_char_id as u64));

    wait_on_a_pumping_b(&mut a, &mut b, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::CastFail { reason, .. } if reason.contains("too far away"))
    })
    .await
    .expect("an out-of-range heal is refused with the line");
    // "Costs no mana" means the mana never DIPS: not deducted and handed back,
    // never taken. Any ManaUpdate for A below full in this window is the old
    // deduct-then-refund order (or the heal landing).
    let dipped = wait_on_a_pumping_b(&mut a, &mut b, Duration::from_millis(700), |m| {
        matches!(m, ServerWorldMsg::ManaUpdate { id, mp, max_mp }
            if *id == a_char_id as u64 && *mp < *max_mp - 1.0)
    })
    .await;
    assert!(dipped.is_none(), "a refused heal never takes the mana (saw {dipped:?})");
    // B was not healed (Healing Wave lands 15 at once; regen is 1 or 2 a tick).
    let healed = wait_on_a_pumping_b(&mut b, &mut a, Duration::from_millis(700), |m| {
        matches!(m, ServerWorldMsg::HealthUpdate { id, hp, .. }
            if *id == b_char_id as u64 && *hp >= hurt_hp + 10.0)
    })
    .await;
    assert!(healed.is_none(), "a target two cells away must not be healed");

    // And no cooldown was burned: A heals ITSELF straight away, inside what
    // would have been Healing Wave's 6 s cooldown.
    a.send_cast_start("Healing Wave", 1.0);
    pump_both_for(&mut a, &mut b, Duration::from_millis(1150)).await;
    a.send_cast_spell("Healing Wave", None);
    wait_on_a_pumping_b(&mut a, &mut b, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::ManaUpdate { id, mp, max_mp }
            if *id == a_char_id as u64 && *mp < *max_mp - 1.0)
    })
    .await
    .expect("the next cast goes through: the refused one stamped no cooldown");
}

/// An ENEMY spell on a PLAYER had no range check either: with both sides
/// flagged for PvP, one could nuke the other from across the world. Both are
/// flagged here and two cells apart; the nuke must be refused and must not
/// land.
#[tokio::test]
async fn pvp_nuke_refuses_out_of_range() {
    let h = start_both().await;
    // Smite: level 1 Cleric, instant, 7 damage.
    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "farnuke", "Farnuke", "Human", "Cleric").await;
    let (b_session, b_char_id, b_token) =
        provision_client(&h.auth_url, "farmark", "Farmark", "Human", "Warrior").await;
    // (248, 60): clear of every camp, so the only thing that could hit B is
    // the nuke under test. (200, 0) is inside the Ancient Wraith's aggro.
    set_char_pos(&h.db_url, b_char_id, 248.0, 60.0).await;

    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    let mut b = WorldClient::start(b_token, &b_session, b_char_id).await;
    pump_both_for(&mut a, &mut b, Duration::from_millis(300)).await;
    a.send_pvp_toggle(true);
    b.send_pvp_toggle(true);
    pump_both_for(&mut a, &mut b, Duration::from_millis(300)).await;

    a.send_cast_spell("Smite", Some(b_char_id as u64));
    let line = wait_on_a_pumping_b(&mut a, &mut b, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::CastFail { reason, .. } if reason.contains("too far away"))
    })
    .await
    .expect("an out-of-range PvP nuke is refused with the line");
    if let ServerWorldMsg::CastFail { reason, .. } = line {
        assert!(!reason.contains("Farmark"), "a target out of reach is not named (got {reason:?})");
    }
    let hit = wait_on_a_pumping_b(&mut b, &mut a, Duration::from_millis(700), |m| {
        matches!(m, ServerWorldMsg::Hit { target, .. } if *target == b_char_id as u64)
    })
    .await;
    assert!(hit.is_none(), "a player two cells away must not be hit");
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
    assert!(a.client.is_disconnected(), "the kicked connection is dropped by the server");
}


/// A cast the server was always going to refuse must cost NOTHING. Until the
/// bind sprint, Bind Affinity had no server arm and the cast took 30 mana,
/// rolled the caster's Alteration skill, and then refunded the mana: a free
/// skill-up attempt every three seconds from an honest hotbar. The pre-flight
/// refuses before any of that. Bind Affinity has an arm now, so the refusal
/// exercised here is the bind rule itself (user call D2, 2026-10-05): a
/// STRANGER cannot be bound, even one standing in the town square; the one
/// line answers absent and ungrouped targets alike, so it is no is-online
/// oracle. The mana never dips at all.
#[tokio::test]
async fn binding_a_stranger_is_refused_and_costs_nothing() {
    let h = start_both().await;
    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "binder", "Binder", "Human", "Cleric").await;
    let (b_session, b_char_id, b_token) =
        provision_client(&h.auth_url, "stranger", "Passerby", "Human", "Warrior").await;
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    let mut b = WorldClient::start(b_token, &b_session, b_char_id).await;
    a.pump_for(Duration::from_millis(300)).await;
    b.pump_for(Duration::from_millis(300)).await;

    a.send_cast_start("Bind Affinity", 3.0);
    pump_both_for(&mut a, &mut b, Duration::from_millis(3150)).await;
    a.send_cast_spell("Bind Affinity", Some(b_char_id as u64));

    // Watch everything that follows the cast for a second: the refusal must
    // arrive, and no ManaUpdate for the caster may show less than full.
    let end = Instant::now() + Duration::from_secs(1);
    let mut refused = false;
    let mut dip: Option<(f32, f32)> = None;
    while Instant::now() < end {
        tick_one(&mut a.client, &mut a.transport);
        while let Some(bytes) = a.client.receive_message(CHANNEL_SYSTEM) {
            match bincode::serde::decode_from_slice::<ServerWorldMsg, _>(&bytes, bincode_cfg()) {
                Ok((ServerWorldMsg::CastFail { reason, .. }, _))
                    if reason.contains("yourself or a group member") =>
                {
                    refused = true;
                }
                Ok((ServerWorldMsg::ManaUpdate { id, mp, max_mp }, _))
                    if id == a_char_id as u64 && mp < max_mp - 1.0 =>
                {
                    dip = Some((mp, max_mp));
                }
                _ => {}
            }
        }
        tokio::time::sleep(TICK_DT).await;
    }
    assert!(refused, "binding a stranger is refused with the line");
    assert!(
        dip.is_none(),
        "it is refused BEFORE the mana comes off, not deducted and refunded (saw {dip:?})"
    );
}

/// User call D4 (2026-10-05): the pet comes along on Gate and STAYS. The
/// review of the first cut found the port arm cleared the pet's target but
/// not the owner's `last_attacked_enemy`, so the pet pre-pass re-inherited
/// the mob within 10 s and the pet walked straight back out of town to it,
/// fighting unsupervised (and a pet kill credits the owner). Necromancer 8
/// (Summon Skeleton at 6, Gate at 8), born bound at the spawn, starts 15 m
/// out; the dummy cannot move or hit, so only the pet engages.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pet_stays_with_its_owner_after_gate() {
    let h = start_both().await;
    let (a_session, a_char_id, _stale) =
        provision_client(&h.auth_url, "petgate", "Morrow", "Human", "Necromancer").await;
    set_char_level(&h.db_url, a_char_id, 8).await;
    set_char_pos(&h.db_url, a_char_id, 15.0, 0.0).await;
    let pool = db::open(&h.db_url).await.expect("open pool");
    db::set_account_gm(&pool, "petgate", true).await.expect("set is_gm");
    let a_token = request_world_token(&h.auth_url, &a_session, a_char_id).await;
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    a.pump_for(Duration::from_millis(300)).await;

    a.send_cast_start("Summon Skeleton", 3.0);
    a.pump_for(Duration::from_millis(3_000) + CAST_RELEASE_MARGIN).await;
    a.send_cast_spell("Summon Skeleton", None);
    let pet_spawn = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::PetSpawn { owner, .. } if *owner == a_char_id as u64)
        })
        .await
        .expect("the skeleton is summoned");
    let pet_id: u64 = match pet_spawn {
        ServerWorldMsg::PetSpawn { id, .. } => id,
        _ => unreachable!(),
    };

    // A dummy 3 m ahead; one swing from the owner and the pet inherits it.
    a.send_dev_spawn("Charm Dummy", 1, 500.0, 0, 0.0, 1.0);
    let spawn = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::EnemySpawn { mob_name, .. } if mob_name == "Charm Dummy")
        })
        .await
        .expect("the dummy spawns");
    let (dummy, dummy_pos) = match spawn {
        ServerWorldMsg::EnemySpawn { id, pos, .. } => (id, pos),
        _ => unreachable!(),
    };
    // Bare-handed reach is 2.7 m and the dummy lands 3 m off: step in, on
    // the server's word for where the owner stands. A wall-clock walk
    // (`walk_dir`, 1.5 m asked) ran the owner 2.25 m PAST the dummy with
    // one tick of reach to spare, and a stretched sleep under the full
    // suite ran it out of reach: the swing missed and the pet had nothing
    // to inherit.
    let mut seq: u32 = 1;
    let start = Vec3 { x: 15.0, y: 0.0, z: 0.0 };
    walk_until_within(&mut a, &mut seq, a_char_id, start, dummy_pos, 1.5).await;
    a.send_attack(dummy, "", false, DamageType::Physical);
    let engaged = a
        .wait_for(CHANNEL_POSITION, Duration::from_secs(4), |m| {
            matches!(m, ServerWorldMsg::Position { id, pos, .. }
                if *id == pet_id && pos.x.is_finite()
                    && ((pos.x - dummy_pos.x).powi(2) + (pos.z - dummy_pos.z).powi(2)).sqrt() < 3.0)
        })
        .await;
    assert!(engaged.is_some(), "the pet inherits the owner's target and closes on the dummy");

    // Gate home. The dummy cannot interrupt (no damage, no legs). The summon
    // started the global cooldown; let it pass first, with the same margin,
    // since the server starts that clock when IT accepts the summon.
    a.pump_for(Duration::from_millis(2_000) + CAST_RELEASE_MARGIN).await;
    a.send_cast_start("Gate", 5.0);
    a.pump_for(Duration::from_millis(5_000) + CAST_RELEASE_MARGIN).await;
    a.send_cast_spell("Gate", None);
    let tp = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| matches!(m, ServerWorldMsg::Teleport { .. }))
        .await;
    let Some(ServerWorldMsg::Teleport { pos: dest }) = tp else {
        panic!("Gate teleports the owner");
    };
    assert!(dest.x.abs() < 1.0 && dest.z.abs() < 1.0, "to the birth bind at the spawn, got ({}, {})", dest.x, dest.z);

    // For the next three seconds every Position of the pet stays beside the
    // owner. On the previous code it is walking back to the dummy at 3 m/s,
    // past 6 m inside two seconds. The position channel is unreliable and
    // unordered against the system channel, so a Position fanned just
    // before the port can be read after the Teleport: the first 400 ms are
    // let through.
    let ported_at = Instant::now();
    let end = ported_at + Duration::from_secs(3);
    let mut pet_positions = 0;
    while Instant::now() < end {
        let left = end.saturating_duration_since(Instant::now());
        let Some(ServerWorldMsg::Position { pos, .. }) = a
            .wait_for(CHANNEL_POSITION, left, |m| {
                matches!(m, ServerWorldMsg::Position { id, .. } if *id == pet_id)
            })
            .await
        else {
            break;
        };
        if ported_at.elapsed() < Duration::from_millis(400) {
            continue;
        }
        pet_positions += 1;
        let off = ((pos.x - dest.x).powi(2) + (pos.z - dest.z).powi(2)).sqrt();
        assert!(off < 6.0, "the pet stays with its owner after Gate; it was {off:.1} m away");
    }
    assert!(pet_positions > 0, "the pet's position is still fanned to its owner after the port");
}

/// The review's other find: a CHARMED mob rode Gate into the town square and,
/// when the charm ended, stood there hostile beside the spawn (a named mob,
/// even). A charmed mob is not carried: it is released where it stands and
/// walks home, as when its charmer logs out. Bard 14 (Siren's Song, Gate at
/// 8) charms a dummy 15 m out and Gates to the spawn.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_charmed_mob_is_released_in_place_when_its_charmer_gates() {
    let h = start_both().await;
    let (a_session, a_char_id, _stale) =
        provision_client(&h.auth_url, "charmgate", "Lirael", "Human", "Bard").await;
    set_char_level(&h.db_url, a_char_id, 14).await;
    set_char_pos(&h.db_url, a_char_id, 15.0, 0.0).await;
    let pool = db::open(&h.db_url).await.expect("open pool");
    db::set_account_gm(&pool, "charmgate", true).await.expect("set is_gm");
    let a_token = request_world_token(&h.auth_url, &a_session, a_char_id).await;
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    a.pump_for(Duration::from_millis(300)).await;

    // Aggro 1 m so it never notices the bard before the charm lands.
    a.send_dev_spawn("Charm Dummy", 1, 500.0, 5, 2.0, 1.0);
    let spawn = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::EnemySpawn { mob_name, .. } if mob_name == "Charm Dummy")
        })
        .await
        .expect("the dummy spawns");
    let dummy: u64 = match spawn {
        ServerWorldMsg::EnemySpawn { id, .. } => id,
        _ => unreachable!(),
    };
    a.send_cast_start("Siren's Song", 1.5);
    a.pump_for(Duration::from_millis(1_700)).await;
    a.send_cast_spell("Siren's Song", Some(dummy));
    let pet_spawn = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(5), |m| {
            matches!(m, ServerWorldMsg::PetSpawn { owner, .. } if *owner == a_char_id as u64)
        })
        .await
        .expect("the charm lands");
    let pet_id: u64 = match pet_spawn {
        ServerWorldMsg::PetSpawn { id, .. } => id,
        _ => unreachable!(),
    };

    // The charm started the global cooldown; let it pass before the Gate.
    a.pump_for(Duration::from_millis(2_100)).await;
    a.send_cast_start("Gate", 5.0);
    a.pump_for(Duration::from_millis(5_200)).await;
    a.send_cast_spell("Gate", None);
    // The Teleport, the charmed pet's despawn and the mob's return land in
    // the same tick; take them in whatever order they come.
    let end = Instant::now() + Duration::from_secs(3);
    let (mut teleported, mut pet_gone, mut mob_back) = (false, false, false);
    while Instant::now() < end && !(teleported && pet_gone && mob_back) {
        let left = end.saturating_duration_since(Instant::now());
        match a
            .wait_for(CHANNEL_SYSTEM, left, |m| {
                matches!(m, ServerWorldMsg::Teleport { .. })
                    || matches!(m, ServerWorldMsg::EntityDespawn { id } if *id == pet_id)
                    || matches!(m, ServerWorldMsg::EnemySpawn { mob_name, .. } if mob_name == "Charm Dummy")
            })
            .await
        {
            Some(ServerWorldMsg::Teleport { pos }) => {
                assert!(pos.x.abs() < 1.0 && pos.z.abs() < 1.0, "Gate lands at the spawn");
                teleported = true;
            }
            Some(ServerWorldMsg::EntityDespawn { .. }) => pet_gone = true,
            Some(ServerWorldMsg::EnemySpawn { pos, .. }) => {
                assert!(
                    (pos.x - 15.0).abs() < 4.0 && pos.z > -8.0 && pos.z < 2.0,
                    "the mob comes back where it stood, 15 m out, not in the square: got ({}, {})",
                    pos.x, pos.z
                );
                mob_back = true;
            }
            _ => break,
        }
    }
    assert!(teleported, "Gate teleports the charmer");
    assert!(pet_gone, "the charmed pet is despawned for its charmer");
    assert!(mob_back, "the mob is back as a hostile where it stood");
}

/// Spell batch step 3 (user call of 2026-10-05): a cast that a HIT interrupts
/// costs the spell's mana in proportion to how far the bar had run, and the
/// caster is told the true mana and the amount. Two dummies walk in and beat
/// on a Cleric 10 casting Bind Affinity (30 mana, 3 s); the interrupt is a
/// real ~70% roll per hit, so the bar is retried until one lands (four hits
/// per attempt, three attempts: the odds of none are below one in a
/// million). On the previous code the CastFail arrived with no charge and
/// no line, and the mana stayed full.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_interrupted_cast_costs_mana_in_proportion() {
    let h = start_both().await;
    let (a_session, a_char_id, _stale) =
        provision_client(&h.auth_url, "channel", "Channa", "Human", "Cleric").await;
    set_char_level(&h.db_url, a_char_id, 10).await;
    let pool = db::open(&h.db_url).await.expect("open pool");
    db::set_account_gm(&pool, "channel", true).await.expect("set is_gm");
    let a_token = request_world_token(&h.auth_url, &a_session, a_char_id).await;
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    a.pump_for(Duration::from_millis(300)).await;

    // Two that walk in (3 m/s), notice the caster (aggro 10) and hit for 5.
    for _ in 0..2 {
        a.send_dev_spawn("Charm Dummy", 1, 500.0, 5, 3.0, 10.0);
        a.pump_for(Duration::from_millis(150)).await;
    }

    let mut interrupted = false;
    let mut charged: Option<i32> = None;
    let mut settled: Option<(f32, f32)> = None;
    'attempts: for _ in 0..3 {
        a.send_cast_start("Bind Affinity", 3.0);
        let end = Instant::now() + Duration::from_millis(3_400);
        while Instant::now() < end {
            tick_one(&mut a.client, &mut a.transport);
            while let Some(bytes) = a.client.receive_message(CHANNEL_SYSTEM) {
                let Ok((msg, _)) = bincode::serde::decode_from_slice::<ServerWorldMsg, _>(&bytes, bincode_cfg()) else {
                    continue;
                };
                match msg {
                    ServerWorldMsg::CastFail { caster, reason }
                        if caster == a_char_id as u64 && reason.contains("interrupted") =>
                    {
                        interrupted = true;
                    }
                    ServerWorldMsg::ChatMessage { text, .. } if text.contains("concentration breaks") => {
                        let digits: String = text.chars().filter(|c| c.is_ascii_digit()).collect();
                        charged = digits.parse().ok();
                    }
                    ServerWorldMsg::ManaUpdate { id, mp, max_mp }
                        if id == a_char_id as u64 && interrupted && settled.is_none() =>
                    {
                        settled = Some((mp, max_mp));
                    }
                    _ => {}
                }
            }
            if interrupted && charged.is_some() && settled.is_some() {
                break 'attempts;
            }
            tokio::time::sleep(TICK_DT).await;
        }
        if interrupted {
            break;
        }
    }
    assert!(interrupted, "a hit during the bar interrupts the cast");
    let n = charged.expect("the caster is told what the interrupt cost");
    assert!((1..=10).contains(&n), "the charge is a fraction of Bind Affinity's 10 mana, got {n}");
    let (mp, max_mp) = settled.expect("the true mana follows the CastFail");
    assert!(
        ((max_mp - mp) - n as f32).abs() < 1.5,
        "the mana bar lands the charge below full: max {max_mp}, now {mp}, charged {n}"
    );
}

/// Spell batch step 4 (user call of 2026-10-05): the global cooldown. After
/// an accepted cast no spell may START for GLOBAL_COOLDOWN (2 s). A timed
/// cast is refused at its CastStart, privately and with the true mana, and
/// nothing is cached, so its CastSpell fails the cast-time gate too; an
/// instant cast is refused at its CastSpell. Once the cooldown has run, the
/// same casts go through. Cleric 8: Smite (instant) then Bind Affinity
/// (3 s). On the previous code every cast here lands.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cast_inside_the_global_cooldown_is_refused_at_its_start() {
    let h = start_both().await;
    let (a_session, a_char_id, _stale) =
        provision_client(&h.auth_url, "gcd", "Hasty", "Human", "Cleric").await;
    set_char_level(&h.db_url, a_char_id, 8).await;
    let pool = db::open(&h.db_url).await.expect("open pool");
    db::set_account_gm(&pool, "gcd", true).await.expect("set is_gm");
    let a_token = request_world_token(&h.auth_url, &a_session, a_char_id).await;
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    a.pump_for(Duration::from_millis(300)).await;

    // Something to Smite: a passive dummy 3 m off.
    a.send_dev_spawn("Charm Dummy", 1, 500.0, 0, 0.0, 0.0);
    let spawn = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::EnemySpawn { mob_name, .. } if mob_name == "Charm Dummy")
        })
        .await
        .expect("the dummy spawns");
    let dummy: u64 = match spawn {
        ServerWorldMsg::EnemySpawn { id, .. } => id,
        _ => unreachable!(),
    };
    a.send_cast_spell("Smite", Some(dummy));
    let hit = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::Hit { target, .. } if *target == dummy)
        })
        .await;
    assert!(hit.is_some(), "the first Smite lands");

    // A timed cast started 300 ms later: refused at its start.
    a.pump_for(Duration::from_millis(300)).await;
    a.send_cast_start("Bind Affinity", 3.0);
    let refused = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(2), |m| {
            matches!(m, ServerWorldMsg::CastFail { caster, reason }
                if *caster == a_char_id as u64 && reason.contains("cannot cast again yet"))
        })
        .await;
    assert!(refused.is_some(), "a cast started inside the global cooldown is refused at its start");
    // The client that sends the CastSpell anyway finds no bar on file.
    a.pump_for(Duration::from_millis(3_200)).await;
    a.send_cast_spell("Bind Affinity", None);
    let no_bar = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(2), |m| {
            matches!(m, ServerWorldMsg::CastFail { caster, reason }
                if *caster == a_char_id as u64 && reason.contains("not ready"))
        })
        .await;
    assert!(no_bar.is_some(), "with nothing cached the release fails the cast-time gate");

    // Past the cooldown (the Smite was over 3.5 s ago) the same cast lands.
    a.send_cast_start("Bind Affinity", 3.0);
    a.pump_for(Duration::from_millis(3_200)).await;
    a.send_cast_spell("Bind Affinity", None);
    let bound = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::ChatMessage { text, .. } if text.contains("bound"))
        })
        .await;
    assert!(bound.is_some(), "after the cooldown the cast goes through");

    // And an instant cast inside the cooldown is refused at its CastSpell:
    // Smite again right behind the bind (Smite's own 3 s cooldown is long
    // past), 200 ms later.
    a.pump_for(Duration::from_millis(200)).await;
    a.send_cast_spell("Smite", Some(dummy));
    let instant_refused = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(2), |m| {
            matches!(m, ServerWorldMsg::CastFail { caster, reason }
                if *caster == a_char_id as u64 && reason.contains("cannot cast again yet"))
        })
        .await;
    assert!(instant_refused.is_some(), "an instant cast inside the global cooldown is refused");
}

/// Bard songs neither start nor honour the global cooldown, or twisting
/// (songs sung back to back) would stop working. A Bard 10 sings Poet's
/// Mending (a 3 s sing since the song rework) and begins Selos' Melody the
/// moment it lands, inside the 2 s window that refuses any other cast; both
/// are accepted, and neither costs mana (EQ's songs were free).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bard_songs_ignore_the_global_cooldown() {
    let h = start_both().await;
    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "twist", "Lyric", "Human", "Bard").await;
    set_char_level(&h.db_url, a_char_id, 10).await;
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    a.pump_for(Duration::from_millis(300)).await;

    let first = sing(&mut a, "Poet's Mending", None).await;
    assert!(first.iter().any(|m| matches!(m, ServerWorldMsg::ChatMessage { text, .. } if text == "You begin singing Poet's Mending.")), "the first song lands");
    a.send_cast_start("Selos' Melody", 3.0);
    let refused = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_millis(600), |m| matches!(m, ServerWorldMsg::CastFail { caster, .. } if *caster == a_char_id as u64))
        .await;
    assert!(refused.is_none(), "a song's CastStart inside the global cooldown is accepted");
    a.pump_alive_for(Duration::from_millis(3_300)).await;
    a.send_cast_spell("Selos' Melody", None);
    let second = a.collect_for(Duration::from_millis(1_000)).await;
    assert!(second.iter().any(|m| matches!(m, ServerWorldMsg::ChatMessage { text, .. } if text == "You begin singing Selos' Melody.")), "and so is its release");
    assert!(!first.iter().chain(second.iter()).any(|m| matches!(m, ServerWorldMsg::ManaUpdate { id, mp, max_mp } if *id == a_char_id as u64 && *mp < max_mp - 0.5)),
        "songs cost no mana");
}

/// Sing `song` the honest way (CastStart, the 3 s sing, the release) and
/// return the next second's messages.
async fn sing(c: &mut WorldClient, song: &str, target: Option<u64>) -> Vec<ServerWorldMsg> {
    c.send_cast_start(song, 3.0);
    c.pump_alive_for(Duration::from_millis(3_300)).await;
    c.send_cast_spell(song, target);
    c.collect_for(Duration::from_millis(1_000)).await
}

fn song_remaining(msgs: &[ServerWorldMsg], who: u64, song: &str) -> Option<f32> {
    msgs.iter().rev().find_map(|m| match m {
        ServerWorldMsg::BuffSnapshot { target, buffs } if *target == who => {
            buffs.iter().find(|(n, _)| n == song).map(|(_, r)| *r)
        }
        _ => None,
    })
}

/// Spell batch step 5: a DoT ticks on the server every three seconds, the
/// caster sees each tick as a named line, and a tick that kills pays exactly
/// what a direct hit would (XP through the one kill step). Druid 6's Entangle
/// (the parity audit's numbers, 2026-10-20): 5 direct (a crit doubles it at
/// most), then 3 per tick (1 dps x 3 s). The dummy has one HP more than the
/// largest direct hit can take, so a tick always does the killing, crit or
/// not. On the previous code Entangle was an unknown spell.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dot_kills_with_the_same_rewards_as_a_hit() {
    let h = start_both().await;
    let (a_session, a_char_id, _stale) =
        provision_client(&h.auth_url, "dotter", "Vinessa", "Human", "Druid").await;
    set_char_level(&h.db_url, a_char_id, 6).await;
    let pool = db::open(&h.db_url).await.expect("open pool");
    db::set_account_gm(&pool, "dotter", true).await.expect("set is_gm");
    let a_token = request_world_token(&h.auth_url, &a_session, a_char_id).await;
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    a.pump_for(Duration::from_millis(300)).await;

    // Entangle's direct hit is its listed 5; a crit takes it to 10 at most.
    a.send_dev_spawn("Charm Dummy", 1, 11.0, 0, 0.0, 1.0);
    let spawn = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::EnemySpawn { mob_name, .. } if mob_name == "Charm Dummy")
        })
        .await
        .expect("the dummy spawns");
    let dummy: u64 = match spawn {
        ServerWorldMsg::EnemySpawn { id, .. } => id,
        _ => unreachable!(),
    };
    a.send_cast_spell("Entangle", Some(dummy));

    // Four ticks of 3 to clear 11 HP: 12 s of ticking, with room to spare.
    let end = Instant::now() + Duration::from_secs(16);
    let (mut ticks_seen, mut died, mut xp) = (0, false, 0i64);
    while Instant::now() < end && !(died && xp > 0) {
        tick_one(&mut a.client, &mut a.transport);
        while let Some(bytes) = a.client.receive_message(CHANNEL_SYSTEM) {
            let Ok((msg, _)) = bincode::serde::decode_from_slice::<ServerWorldMsg, _>(&bytes, bincode_cfg()) else {
                continue;
            };
            match msg {
                ServerWorldMsg::ProcTriggered { target, proc_name, amount, .. }
                    if target == dummy && proc_name == "Entangle" =>
                {
                    assert_eq!(amount, 3, "a tick is dps x 3");
                    ticks_seen += 1;
                }
                ServerWorldMsg::EntityDied { id } if id == dummy => died = true,
                ServerWorldMsg::XpGained { amount, .. } if amount > 0 => xp += amount as i64,
                _ => {}
            }
        }
        tokio::time::sleep(TICK_DT).await;
    }
    assert!(ticks_seen >= 1, "the caster saw the ticks as named lines (saw {ticks_seen})");
    assert!(died, "a tick kills the dummy");
    assert!(xp > 0, "a DoT kill pays XP like a direct hit");
}

/// Ledger rule: your DoTs end when you Gate. Druid 8 (Gate at 8) tags a
/// 500 HP dummy 15 m from the spawn and Gates home; after the Teleport no
/// further tick of theirs lands on it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dot_ends_when_its_caster_gates() {
    let h = start_both().await;
    let (a_session, a_char_id, _stale) =
        provision_client(&h.auth_url, "dotgate", "Rowan", "Human", "Druid").await;
    set_char_level(&h.db_url, a_char_id, 8).await;
    set_char_pos(&h.db_url, a_char_id, 15.0, 0.0).await;
    let pool = db::open(&h.db_url).await.expect("open pool");
    db::set_account_gm(&pool, "dotgate", true).await.expect("set is_gm");
    let a_token = request_world_token(&h.auth_url, &a_session, a_char_id).await;
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    a.pump_for(Duration::from_millis(300)).await;

    a.send_dev_spawn("Charm Dummy", 1, 500.0, 0, 0.0, 1.0);
    let spawn = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::EnemySpawn { mob_name, .. } if mob_name == "Charm Dummy")
        })
        .await
        .expect("the dummy spawns");
    let dummy: u64 = match spawn {
        ServerWorldMsg::EnemySpawn { id, .. } => id,
        _ => unreachable!(),
    };
    a.send_cast_spell("Entangle", Some(dummy));
    let first_tick = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(5), |m| {
            matches!(m, ServerWorldMsg::ProcTriggered { target, proc_name, .. }
                if *target == dummy && proc_name == "Entangle")
        })
        .await;
    assert!(first_tick.is_some(), "the DoT is ticking before the Gate");

    a.send_cast_start("Gate", 5.0);
    a.pump_for(Duration::from_millis(5_200)).await;
    a.send_cast_spell("Gate", None);
    let tp = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| matches!(m, ServerWorldMsg::Teleport { .. }))
        .await;
    assert!(tp.is_some(), "Gate teleports the caster");
    // Entangle runs 18 s; eight more seconds would hold two more ticks.
    let late = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(8), |m| {
            matches!(m, ServerWorldMsg::ProcTriggered { target, proc_name, .. }
                if *target == dummy && proc_name == "Entangle")
        })
        .await;
    assert!(late.is_none(), "no tick lands after the caster Gated");
}

/// Ledger rule: a mob that starts leashing home sheds its DoTs. A chasing
/// dummy (aggro 10, 2 m/s) is tagged and the caster runs 50 m west at
/// 5 m/s; past the 30 m leash the mob turns home and the DoT is gone,
/// so no tick lands in the eight seconds after the run.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dot_ends_when_the_mob_leashes() {
    let h = start_both().await;
    let (a_session, a_char_id, _stale) =
        provision_client(&h.auth_url, "dotleash", "Fern", "Human", "Druid").await;
    set_char_level(&h.db_url, a_char_id, 6).await;
    let pool = db::open(&h.db_url).await.expect("open pool");
    db::set_account_gm(&pool, "dotleash", true).await.expect("set is_gm");
    let a_token = request_world_token(&h.auth_url, &a_session, a_char_id).await;
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    a.pump_for(Duration::from_millis(300)).await;
    let mut seq: u32 = 1;

    // Slow (1 m/s) so the gap opens past the 30 m leash inside five seconds.
    a.send_dev_spawn("Charm Dummy", 1, 500.0, 0, 1.0, 10.0);
    let spawn = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::EnemySpawn { mob_name, .. } if mob_name == "Charm Dummy")
        })
        .await
        .expect("the dummy spawns");
    let dummy: u64 = match spawn {
        ServerWorldMsg::EnemySpawn { id, .. } => id,
        _ => unreachable!(),
    };
    a.send_cast_spell("Entangle", Some(dummy));
    a.pump_for(Duration::from_millis(300)).await;
    // 50 m west at 5 m/s (the west side of the z = 0 line is clear of
    // camps; east past x = 28 is the gnoll camp): about 10 s. The first tick,
    // at three seconds, lands on the way out and is queued; drain it before
    // watching, so only ticks after the leash count.
    walk_dir(&mut a, &mut seq, Vec3 { x: -1.0, y: 0.0, z: 0.0 }, 50.0).await;
    a.wait_for(CHANNEL_SYSTEM, Duration::from_millis(200), |_| false).await;
    let late = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(8), |m| {
            matches!(m, ServerWorldMsg::ProcTriggered { target, proc_name, .. }
                if *target == dummy && proc_name == "Entangle")
        })
        .await;
    assert!(late.is_none(), "a mob walking home carries no DoT: no tick after the leash");
}

/// Design review item (f): a mob finished off by a damage shield paid
/// nothing. Druid 10 wears Thorns (8 per hit); a 7 HP dummy walks in,
/// swings once, and the reflect kills it through the one kill step, so XP
/// arrives. On the previous code the reflect marked it dead and nothing
/// else.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_damage_shield_kill_pays_like_any_other() {
    let h = start_both().await;
    let (a_session, a_char_id, _stale) =
        provision_client(&h.auth_url, "thorny", "Bramble", "Human", "Druid").await;
    set_char_level(&h.db_url, a_char_id, 10).await;
    let pool = db::open(&h.db_url).await.expect("open pool");
    db::set_account_gm(&pool, "thorny", true).await.expect("set is_gm");
    let a_token = request_world_token(&h.auth_url, &a_session, a_char_id).await;
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    a.pump_for(Duration::from_millis(300)).await;

    a.send_cast_start("Thorns", 1.5);
    a.pump_for(Duration::from_millis(1_700)).await;
    a.send_cast_spell("Thorns", None);
    let shielded = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::BuffSnapshot { target, buffs }
                if *target == a_char_id as u64 && buffs.iter().any(|(n, _)| n == "Thorns"))
        })
        .await;
    assert!(shielded.is_some(), "Thorns is up");

    // Walks in at 2 m/s, notices at 10 m, hits for a point: the reflect does 8.
    a.send_dev_spawn("Charm Dummy", 1, 7.0, 1, 2.0, 10.0);
    let spawn = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::EnemySpawn { mob_name, .. } if mob_name == "Charm Dummy")
        })
        .await
        .expect("the dummy spawns");
    let dummy: u64 = match spawn {
        ServerWorldMsg::EnemySpawn { id, .. } => id,
        _ => unreachable!(),
    };
    let end = Instant::now() + Duration::from_secs(10);
    let (mut died, mut xp) = (false, 0i64);
    while Instant::now() < end && !(died && xp > 0) {
        tick_one(&mut a.client, &mut a.transport);
        while let Some(bytes) = a.client.receive_message(CHANNEL_SYSTEM) {
            match bincode::serde::decode_from_slice::<ServerWorldMsg, _>(&bytes, bincode_cfg()) {
                Ok((ServerWorldMsg::EntityDied { id }, _)) if id == dummy => died = true,
                Ok((ServerWorldMsg::XpGained { amount, .. }, _)) if amount > 0 => xp += amount as i64,
                _ => {}
            }
        }
        tokio::time::sleep(TICK_DT).await;
    }
    assert!(died, "the reflect kills the dummy");
    assert!(xp > 0, "and the kill pays XP");
}

/// A spell ported from the client's definitions on 2026-10-05 actually lands.
/// Bloodfire (Sorcerer, level 4, instant) had no entry in spells.toml, so the
/// server refused it as unknown and a Sorcerer's second nuke did nothing
/// online. With the entry it goes through the ordinary ENEMY arm.
#[tokio::test]
async fn a_ported_spell_lands_on_its_target() {
    let h = start_both().await;
    let (a_session, a_char_id, _stale_token) =
        provision_client(&h.auth_url, "bloodfire", "Bloodfire", "Human", "Sorcerer").await;
    set_char_level(&h.db_url, a_char_id, 4).await;
    let pool = db::open(&h.db_url).await.expect("open pool");
    db::set_account_gm(&pool, "bloodfire", true).await.expect("set is_gm");
    let a_token = request_world_token(&h.auth_url, &a_session, a_char_id).await;
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;

    a.send_dev_spawn("Nuke Dummy", 1, 500.0, 0, 0.0, 0.0);
    let spawn_evt = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::EnemySpawn { mob_name, .. } if mob_name == "Nuke Dummy")
        })
        .await
        .expect("the dummy spawns");
    let dummy: u64 = match spawn_evt {
        ServerWorldMsg::EnemySpawn { id, .. } => id,
        _ => unreachable!(),
    };

    a.send_cast_spell("Bloodfire", Some(dummy));
    let hit = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::Hit { attacker, target, .. }
                if *attacker == a_char_id as u64 && *target == dummy)
        })
        .await;
    assert!(hit.is_some(), "Bloodfire must land on the dummy now that the server knows it");
}

/// One cast bar authorises ONE cast. Every CastSpell in a batch used to read
/// the same CastStart cache, so a forged client could send fifty in one
/// datagram after a single bar. Two sent back to back are in one datagram:
/// the first resolves, the second is told to wait.
#[tokio::test]
async fn one_cast_per_caster_per_tick() {
    let h = start_both().await;
    // Smite: level 1 Cleric, instant.
    let (a_session, a_char_id, _stale_token) =
        provision_client(&h.auth_url, "twocast", "Twocast", "Human", "Cleric").await;
    let pool = db::open(&h.db_url).await.expect("open pool");
    db::set_account_gm(&pool, "twocast", true).await.expect("set is_gm");
    let a_token = request_world_token(&h.auth_url, &a_session, a_char_id).await;
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;

    a.send_dev_spawn("Cast Dummy", 1, 500.0, 0, 0.0, 0.0);
    let spawn_evt = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::EnemySpawn { mob_name, .. } if mob_name == "Cast Dummy")
        })
        .await
        .expect("the dummy spawns");
    let dummy: u64 = match spawn_evt {
        ServerWorldMsg::EnemySpawn { id, .. } => id,
        _ => unreachable!(),
    };

    a.send_cast_spell("Smite", Some(dummy));
    a.send_cast_spell("Smite", Some(dummy));
    let end = Instant::now() + Duration::from_secs(1);
    let mut hits = 0u32;
    let mut told = false;
    while Instant::now() < end {
        tick_one(&mut a.client, &mut a.transport);
        while let Some(bytes) = a.client.receive_message(CHANNEL_SYSTEM) {
            match bincode::serde::decode_from_slice::<ServerWorldMsg, _>(&bytes, bincode_cfg()) {
                Ok((ServerWorldMsg::Hit { attacker, target, .. }, _))
                    if attacker == a_char_id as u64 && target == dummy =>
                {
                    hits += 1;
                }
                Ok((ServerWorldMsg::CastFail { reason, .. }, _))
                    if reason.contains("One spell at a time") =>
                {
                    told = true;
                }
                _ => {}
            }
        }
        tokio::time::sleep(TICK_DT).await;
    }
    assert_eq!(hits, 1, "exactly one of the two casts resolves");
    assert!(told, "the extra cast in the same tick is answered, not silently dropped");
}

/// A character that leaves while dead (a crash, a window-X, a restart) loads
/// at 0 HP; it used to die AGAIN on its first tick back (`death_processed`
/// was false for every spawn), paying a second XP penalty and leaving a
/// second, empty corpse. Found by the 2026-10-12 review of the public
/// hosting branch's restart path, which makes everyone dead at that moment
/// pay it at once; the bug itself is older than that path.
#[tokio::test]
async fn a_dead_character_stays_dead_on_relogin_and_pays_once() {
    let h = start_both().await;
    let (session, char_id, _) =
        provision_client(&h.auth_url, "corpse", "Corpsey", "Human", "Warrior").await;
    // Below the level floor there is no XP penalty to double; level 10 has one.
    set_char_level(&h.db_url, char_id, 10).await;
    let token = gm_world_token(&h.auth_url, &h.db_url, "corpse", &session, char_id).await;
    let mut a = WorldClient::start(token, &session, char_id).await;
    let cid = char_id as u64;
    a.send_death();
    let dead = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::HealthUpdate { id, hp, .. } if *id == cid && *hp <= 0.01)
        })
        .await;
    assert!(dead.is_some(), "the first death lands");
    a.pump_for(Duration::from_millis(300)).await;
    // Leave dead: the clean disconnect reaps at once and checkpoints 0 HP.
    a.send_disconnect();
    a.pump_for(Duration::from_millis(500)).await;
    drop(a);
    tokio::time::sleep(Duration::from_millis(500)).await;

    let pool = db::open(&h.db_url).await.expect("open pool");
    let after_first = db::load_character(&pool, char_id).await.expect("load");
    assert!(after_first.hp <= 0.0, "left dead: hp {}", after_first.hp);
    let corpses_after_first: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM corpses WHERE char_id = ?1")
            .bind(char_id)
            .fetch_one(&pool)
            .await
            .expect("count");
    assert_eq!(corpses_after_first, 1, "one corpse from one death");

    // Log back in dead, stand there, leave again (another checkpoint).
    let token = request_world_token(&h.auth_url, &session, char_id).await;
    let mut b = WorldClient::start(token, &session, char_id).await;
    let seeded_dead = b
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::HealthUpdate { id, hp, .. } if *id == cid && *hp <= 0.01)
        })
        .await;
    assert!(seeded_dead.is_some(), "the client is told it is dead by the HP seed");
    b.pump_for(Duration::from_millis(600)).await;
    b.send_disconnect();
    b.pump_for(Duration::from_millis(500)).await;
    drop(b);
    tokio::time::sleep(Duration::from_millis(500)).await;

    let after_relog = db::load_character(&pool, char_id).await.expect("load");
    assert_eq!(
        (after_relog.level, after_relog.xp),
        (after_first.level, after_first.xp),
        "no second XP penalty on relogging dead"
    );
    let corpses_after_relog: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM corpses WHERE char_id = ?1")
            .bind(char_id)
            .fetch_one(&pool)
            .await
            .expect("count");
    assert_eq!(corpses_after_relog, 1, "no second corpse either");
}

/// A character that left dead with full mana and stamina (a Warrior, whose
/// token mana is always full) is told its HP on entering the world. The regen
/// fan sends only what changed since its last send, and nothing changes on the
/// dead with full pools, so this character used to hear nothing: the client
/// kept the full bars of its login reset, never learned it was dead and never
/// respawned, while the server refused every move (playtest 2026-10-26, "Bun
/// is stuck"). The test above passed only because its stamina was regenerating.
#[tokio::test]
async fn a_dead_character_with_full_pools_is_told_it_is_dead() {
    let h = start_both().await;
    let (session, char_id, _) =
        provision_client(&h.auth_url, "stuckbun", "Stuckbun", "Gnome", "Warrior").await;
    // Dead, with mana and stamina at their maximum (the load clamps 99999).
    let pool = db::open(&h.db_url).await.expect("open pool");
    sqlx::query("UPDATE characters SET hp = 0.0, mp = 99999.0, stamina = 99999.0 WHERE id = ?1")
        .bind(char_id)
        .execute(&pool)
        .await
        .expect("leave dead with full pools");
    let token = request_world_token(&h.auth_url, &session, char_id).await;
    let mut a = WorldClient::start(token, &session, char_id).await;
    let cid = char_id as u64;
    let told_dead = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::HealthUpdate { id, hp, .. } if *id == cid && *hp <= 0.01)
        })
        .await;
    assert!(told_dead.is_some(), "a dead joiner is told its HP even when nothing regenerates");

    // Told it is dead, the client respawns, and the server stands it back up.
    a.send_respawn();
    let alive = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::HealthUpdate { id, hp, .. } if *id == cid && *hp > 0.0)
        })
        .await;
    assert!(alive.is_some(), "the respawn lands");
}

// ── Food and water, the EQ way (docs/design/food_and_water.md) ─────────────

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
    give_char_item(&h.db_url, char_id, "base", 0, "res://data/loot/items/small_pouch.tres", 1).await;
    give_char_item(&h.db_url, char_id, "base", 1, "res://data/loot/items/cloth_cap.tres", 1).await;
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
    assert!(hunger > 2880.0 - 60.0 && hunger <= 2880.0, "a meal of twenty units, less what ran: {hunger}");
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
    assert!(thirst > 1000.0 - 25.0, "and stopped at death (15 s dead would have taken 15 more): {thirst}");
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
    assert!(emptied.is_none(), "nothing is taken on a refusal: the second loaf stays");
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

/// Every system-channel message `c` receives over `d`, decoded, pumping the
/// transport the whole time.
async fn collect_system(c: &mut WorldClient, d: Duration) -> Vec<ServerWorldMsg> {
    let end = Instant::now() + d;
    let mut out = Vec::new();
    while Instant::now() < end {
        tick_one(&mut c.client, &mut c.transport);
        while let Some(bytes) = c.client.receive_message(CHANNEL_SYSTEM) {
            if let Ok((msg, _)) =
                bincode::serde::decode_from_slice::<ServerWorldMsg, _>(&bytes, bincode_cfg())
            {
                out.push(msg);
            }
        }
        tokio::time::sleep(TICK_DT).await;
    }
    out
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
        w_first.iter().any(|m| matches!(m, ServerWorldMsg::HungerUpdate { hunger_secs, drain_per_sec, .. }
            if *hunger_secs > hunger_cap() - 5.0 && (*drain_per_sec - 1.0).abs() < 1e-6)),
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
            ServerWorldMsg::HungerUpdate { hunger_secs, thirst_secs, cap_secs, drain_per_sec } => {
                Some((*hunger_secs, *thirst_secs, *cap_secs, *drain_per_sec))
            }
            _ => None,
        })
        .expect("the eater gets a HungerUpdate on entering");
    let (hunger, thirst, cap, rate) = update;
    assert!(hunger > 2880.0 - 30.0 && hunger <= 2880.0, "the update carries the meal just eaten: {hunger}");
    assert!(thirst > cap - 30.0, "thirst still full: {thirst}");
    assert_eq!(cap, hunger_cap(), "the cap");
    assert!((rate - 2.0).abs() < 1e-6, "a living Halfling drains at twice the Human rate: {rate}");
    assert!(
        e_msgs.iter().any(|m| matches!(m, ServerWorldMsg::ChatMessage { text, .. } if text == "You eat a Bread Loaf.")),
        "the eater is told what it ate"
    );
    assert!(
        !e_msgs.iter().any(|m| matches!(m, ServerWorldMsg::ChatMessage { text, .. } if text.starts_with("Chomp"))),
        "the eater does not get the emote about itself"
    );

    assert!(
        w_msgs.iter().any(|m| matches!(m, ServerWorldMsg::ChatMessage { text, .. }
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

fn hunger_cap() -> f32 {
    projectdawn_server::world::hunger::FULLNESS_CAP_SECS
}

/// How the server answered a connect attempt: ConnectOk (a live client, past
/// EnterWorld as `WorldClient::start` leaves it), a Kick at the door, or a
/// renet handshake that never completed. Netcode ignores a connect request
/// for a client id that is still connected and sends nothing back, so the
/// third is a real answer and not a harness failure.
enum ConnectOutcome {
    Connected(WorldClient),
    Kicked { code: protocol::world::KickCode, reconnect_after_secs: Option<u32> },
    HandshakeTimedOut,
}

/// `WorldClient::start` without the panics, reporting how the server
/// answered. A kicked attempt sends the netcode disconnect the real client's
/// kick handler sends (`disconnect_now`), so the refused connection frees its
/// client id at once instead of holding it until a timeout.
async fn try_connect(
    token_bytes: Vec<u8>,
    session_token_hex: &str,
    char_id: i64,
    handshake_wait: Duration,
) -> ConnectOutcome {
    let connect_token =
        ConnectToken::read(&mut Cursor::new(&token_bytes[..])).expect("ConnectToken::read");
    let socket = UdpSocket::bind("127.0.0.1:0").expect("udp bind");
    socket.set_nonblocking(true).expect("nonblocking");
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    let auth = ClientAuthentication::Secure { connect_token };
    let mut transport = NetcodeClientTransport::new(now, auth, socket).expect("transport");
    let mut client = RenetClient::new(connection_config_matching_server());
    let deadline = Instant::now() + handshake_wait;
    while !client.is_connected() {
        tick_one(&mut client, &mut transport);
        if Instant::now() > deadline {
            transport.disconnect();
            return ConnectOutcome::HandshakeTimedOut;
        }
        tokio::time::sleep(TICK_DT).await;
    }
    let token = decode_token_bytes(session_token_hex);
    let msg = ClientWorldMsg::Connect {
        session_token: token,
        char_id: char_id as u64,
        client_version: "0.1.0".into(),
    };
    send_msg(&mut client, CHANNEL_SYSTEM, &msg);
    let mut this = WorldClient { client, transport, _char_id: char_id };
    let answer = this
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::ConnectOk { .. } | ServerWorldMsg::Kick { .. })
        })
        .await;
    match answer {
        Some(ServerWorldMsg::Kick { code, reconnect_after_secs, .. }) => {
            this.transport.disconnect();
            tokio::time::sleep(TICK_DT).await;
            ConnectOutcome::Kicked { code, reconnect_after_secs }
        }
        Some(_) => {
            send_msg(&mut this.client, CHANNEL_SYSTEM, &ClientWorldMsg::EnterWorld);
            for _ in 0..4 {
                tick_one(&mut this.client, &mut this.transport);
                tokio::time::sleep(TICK_DT).await;
            }
            ConnectOutcome::Connected(this)
        }
        None => panic!("the server answered the Connect with neither ConnectOk nor Kick within 3 s"),
    }
}

/// The camp-and-linkdead design's deny-login rule from the client's seat.
/// `banker_slice2_checklist.md` row 54 is ticked with a note saying the
/// opposite ("killed A's client, then immediately logged back in
/// successfully"), so this pins which is true. A client that dies without a
/// Disconnect leaves its body in the world, a same-account relogin is refused
/// until the body is reaped, and the refusals never shorten the window: the
/// Lineage II force-off (relog to boot your own live session) and the
/// dodge-the-reap (knock until the body is gone) patterns both fail. Three
/// phases on the server's own clocks: before the 10 s heartbeat timeout the
/// old netcode connection still holds the client id, so a second handshake
/// is ignored outright; after it the body is linkdead and the Connect is
/// answered with a DuplicateLogin kick carrying the countdown; after
/// LINKDEAD_SECS the body is gone and the login succeeds. About 45 s of
/// wall clock, all of it the server's own timers.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unclean_drop_refuses_relogin_until_the_body_is_reaped() {
    let h = start_both().await;
    let (session, char_id, _stale) =
        provision_client(&h.auth_url, "crasher", "Dropsy", "Human", "Warrior").await;
    let token = request_world_token(&h.auth_url, &session, char_id).await;
    let mut a = WorldClient::start(token, &session, char_id).await;
    a.pump_for(Duration::from_millis(300)).await;
    // The crash: no Disconnect, no netcode goodbye, the socket just closes.
    drop(a);
    let dropped_at = Instant::now();

    // Phase 1: the dropped session is still live as far as the server knows.
    // A relogin must not boot it; netcode ignores the second handshake.
    let token = request_world_token(&h.auth_url, &session, char_id).await;
    match try_connect(token, &session, char_id, Duration::from_secs(4)).await {
        ConnectOutcome::HandshakeTimedOut => {}
        ConnectOutcome::Kicked { code, .. } => {
            panic!("expected the handshake to be ignored while the old connection lives, got a kick: {code:?}")
        }
        ConnectOutcome::Connected(_) => {
            panic!("a relogin got in while the dropped session was still live: the force-off door is open")
        }
    }

    // Phase 2: the heartbeat timeout (10 s) drops the transport and the body
    // goes linkdead; from then on the Connect is refused with the countdown.
    // Keep knocking: every knock inside the window is a relogin that must
    // not reset it.
    let mut countdown: Option<(u32, Instant)> = None;
    let deadline = dropped_at + Duration::from_secs(25);
    while countdown.is_none() && Instant::now() < deadline {
        let token = request_world_token(&h.auth_url, &session, char_id).await;
        match try_connect(token, &session, char_id, Duration::from_secs(2)).await {
            ConnectOutcome::HandshakeTimedOut => {
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            ConnectOutcome::Kicked { code, reconnect_after_secs } => {
                assert!(
                    matches!(code, protocol::world::KickCode::DuplicateLogin),
                    "a relogin during the linger is refused as a duplicate login, got {code:?}"
                );
                let secs = reconnect_after_secs
                    .expect("a refusal while the body is linkdead carries the countdown");
                assert!(
                    secs <= world::LINKDEAD_SECS.as_secs() as u32,
                    "the countdown is at most the linger window, got {secs}"
                );
                countdown = Some((secs, Instant::now()));
            }
            ConnectOutcome::Connected(_) => {
                panic!("a relogin got in while the body was lingering linkdead")
            }
        }
    }
    let (secs, told_at) = countdown
        .expect("within 25 s of the drop the body is linkdead and a relogin is refused with a countdown");

    // Phase 3: the window is the body's clock, not the relogin's. Knock every
    // few seconds through the countdown the server itself announced (each a
    // refusal), then expect the door open within a few seconds of it. A reap
    // timer reset by the knocking would leave this waiting past the deadline.
    let open_by = told_at + Duration::from_secs(secs as u64 + 10);
    let mut connected: Option<WorldClient> = None;
    while connected.is_none() && Instant::now() < open_by {
        let token = request_world_token(&h.auth_url, &session, char_id).await;
        match try_connect(token, &session, char_id, Duration::from_secs(2)).await {
            ConnectOutcome::Connected(c) => connected = Some(c),
            ConnectOutcome::Kicked { code, .. } => {
                assert!(
                    matches!(code, protocol::world::KickCode::DuplicateLogin),
                    "still the duplicate-login refusal while the body lingers, got {code:?}"
                );
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
            ConnectOutcome::HandshakeTimedOut => {
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    }
    let mut b = connected.expect(
        "the relogin succeeds once the announced window has elapsed; a refused relogin must not have extended it",
    );
    assert!(
        b.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::HealthUpdate { .. } | ServerWorldMsg::Position { .. } | ServerWorldMsg::TimeOfDay { .. })
        })
        .await
        .is_some(),
        "the fresh login is a real session: the server talks to it"
    );
    b.send_disconnect();
    b.pump_for(Duration::from_millis(300)).await;
    drop(b);
}

/// Corpse auto-re-equip (the cursor epic's slice 2). A Warrior wearing a
/// cloth cap (head, slot 2) and a cloth robe (chest, slot 3) and carrying
/// bread dies, respawns, and loots the corpse: the cap and robe go straight
/// back onto slots 2 and 3 (an InventoryDelta to "equip" for each, the
/// client's ordinary equip path), the bread goes to a base slot, and the
/// worn rows are what the database holds afterwards. On the previous code all
/// three landed in the bags.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn looting_your_own_corpse_puts_your_gear_back_on() {
    const CAP: &str = "res://data/loot/items/cloth_cap.tres";
    const ROBE: &str = "res://data/loot/items/cloth_robe.tres";
    let h = start_both().await;
    let (session, char_id, _) =
        provision_client(&h.auth_url, "rewear", "Rewear", "Human", "Warrior").await;
    give_char_item(&h.db_url, char_id, "equip", 2, CAP, 1).await;
    give_char_item(&h.db_url, char_id, "equip", 3, ROBE, 1).await;
    give_char_item(&h.db_url, char_id, "base", 0, BREAD, 2).await;
    let pool = db::open(&h.db_url).await.expect("open pool");
    db::set_account_gm(&pool, "rewear", true).await.expect("set is_gm");
    let token = request_world_token(&h.auth_url, &session, char_id).await;
    let mut a = WorldClient::start(token, &session, char_id).await;
    a.pump_for(Duration::from_millis(300)).await;

    a.send_damage_self(1_000_000);
    let contents = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(5), |m| {
            matches!(m, ServerWorldMsg::CorpseContents { .. })
        })
        .await
        .expect("dying leaves a corpse and its owner is told what is on it");
    let ServerWorldMsg::CorpseContents { corpse_id, items, .. } = contents else {
        unreachable!()
    };
    assert_eq!(items.len(), 3, "cap, robe and bread are on the corpse: {items:?}");
    assert_eq!(items[0].0, CAP, "worn gear first on the corpse, in paperdoll order");
    assert_eq!(items[1].0, ROBE);

    a.send_respawn();
    a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(5), |m| matches!(m, ServerWorldMsg::Teleport { .. }))
        .await
        .expect("respawned at the bind, beside the corpse");

    a.send_loot_all(corpse_id);
    let mut msgs = Vec::new();
    let end = Instant::now() + Duration::from_secs(2);
    while Instant::now() < end {
        tick_one(&mut a.client, &mut a.transport);
        while let Some(bytes) = a.client.receive_message(CHANNEL_SYSTEM) {
            if let Ok((msg, _)) =
                bincode::serde::decode_from_slice::<ServerWorldMsg, _>(&bytes, bincode_cfg())
            {
                msgs.push(msg);
            }
        }
        tokio::time::sleep(TICK_DT).await;
    }
    let equip_delta = |slot: u32, path: &str| {
        msgs.iter().any(|m| matches!(m, ServerWorldMsg::InventoryDelta { location, slot: s, item_path: Some(p), .. }
            if location == "equip" && *s == slot && p == path))
    };
    assert!(equip_delta(2, CAP), "the cap goes back on the head");
    assert!(equip_delta(3, ROBE), "the robe goes back on the chest");
    assert!(
        msgs.iter().any(|m| matches!(m, ServerWorldMsg::InventoryDelta { location, item_path: Some(p), .. }
            if location == "base" && p == BREAD)),
        "the carried bread goes to the bags"
    );
    assert!(
        !msgs.iter().any(|m| matches!(m, ServerWorldMsg::InventoryDelta { location, item_path: Some(p), .. }
            if location != "equip" && (p == CAP || p == ROBE))),
        "the worn gear does not also land in the bags"
    );

    // Persisted with the loot, in one transaction.
    let worn: Vec<(i64, String)> = sqlx::query_as(
        "SELECT slot, item_path FROM character_items WHERE char_id = ?1 AND location = 'equip' ORDER BY slot",
    )
    .bind(char_id)
    .fetch_all(&pool)
    .await
    .expect("read worn rows");
    assert_eq!(worn, vec![(2, CAP.to_string()), (3, ROBE.to_string())], "the database holds the gear as worn");
    a.send_disconnect();
    a.pump_for(Duration::from_millis(200)).await;
}

/// Seated-combat rule (classic EQ; user call 2026-10-14). A hit on a SITTING
/// player lands for the top of the attacker's damage band with no armor, and
/// any damage stands them. The stand is proven through `/camp`, the one
/// intent that needs the server's own seated flag: after the hit the camp is
/// refused outright, and after sitting again the same camp is accepted. Fails
/// on the previous code at the camp assertion (the server left the target
/// seated, so the camp was accepted).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_seated_player_is_hit_for_the_max_and_stood_up() {
    let h = start_both().await;
    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "seat_a", "Seata", "Human", "Warrior").await;
    let (b_session, b_char_id, b_token) =
        provision_client(&h.auth_url, "seat_b", "Seatb", "Human", "Warrior").await;
    master_weapon_skill(&h.db_url, a_char_id, "hand_to_hand").await;
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    let mut b = WorldClient::start(b_token, &b_session, b_char_id).await;
    pump_both_for(&mut a, &mut b, Duration::from_millis(300)).await;
    a.send_pvp_toggle(true);
    b.send_pvp_toggle(true);
    pump_both_for(&mut a, &mut b, Duration::from_millis(150)).await;

    // B sits beside A (both spawn on the same point); A swings bare fists,
    // which roll 1..=4 plus STR/5, so the seated max is 4 plus STR/5, times
    // A's mastered hand_to_hand (x1.15, combat.rs).
    let strength = char_data::compute("Human", "Warrior", 1).stats.strength;
    let seated_max = ((4 + strength / 5) as f32 * 1.15) as i32;
    b.send_sit();
    pump_both_for(&mut a, &mut b, Duration::from_millis(150)).await;
    a.send_attack(b_char_id as u64, "", false, DamageType::Physical);
    // Service A's transport too, or the swing never leaves its socket.
    pump_both_for(&mut a, &mut b, Duration::from_millis(100)).await;
    let hit = b
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::Hit { attacker, target, .. }
                if *attacker == a_char_id as u64 && *target == b_char_id as u64)
        })
        .await
        .expect("B is hit");
    let ServerWorldMsg::Hit { amount, crit, .. } = hit else { unreachable!() };
    if crit {
        assert!(
            amount >= seated_max,
            "a crit on a seated target lands at least the max ({seated_max}), got {amount}"
        );
    } else {
        assert_eq!(amount, seated_max, "a seated target takes the top of the band");
    }

    // The hit stood B: a /camp, which needs the server's seated flag, is
    // refused with no CampUpdate at all.
    b.send_camp();
    let refused = b
        .wait_for(CHANNEL_SYSTEM, Duration::from_millis(1200), |m| {
            matches!(m, ServerWorldMsg::CampUpdate { .. })
        })
        .await;
    assert!(
        refused.is_none(),
        "the hit stood the seated player, so /camp is refused; got {refused:?}"
    );

    // Control: seated again, the same /camp is accepted.
    b.send_sit();
    b.pump_for(Duration::from_millis(150)).await;
    b.send_camp();
    let accepted = b
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(2), |m| {
            matches!(m, ServerWorldMsg::CampUpdate { active: true, .. })
        })
        .await;
    assert!(accepted.is_some(), "seated again, /camp is accepted");
    a.send_disconnect();
    b.send_disconnect();
    a.pump_for(Duration::from_millis(200)).await;
    b.pump_for(Duration::from_millis(200)).await;
}

/// Resurrection sickness (the corpse plan's slice 3; the P99 design, user call
/// 2026-10-15). A dies and respawns beside the corpse; B, a Cleric 20, casts
/// Resurrection (Minor) on it; A accepts. A is summoned, told "You are
/// exhausted.", lands at a quarter of its max HP with no mana, and its buff
/// snapshot carries the five-minute sickness.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_accepted_resurrection_leaves_its_sickness() {
    let h = start_both().await;
    let (a_session, a_char_id, _) =
        provision_client(&h.auth_url, "ressick_a", "Ressa", "Human", "Warrior").await;
    let (b_session, b_char_id, _) =
        provision_client(&h.auth_url, "ressick_b", "Ressb", "Human", "Cleric").await;
    let pool = db::open(&h.db_url).await.expect("open pool");
    db::set_account_gm(&pool, "ressick_a", true).await.expect("set is_gm");
    set_char_level(&h.db_url, b_char_id, 20).await;
    let a_token = request_world_token(&h.auth_url, &a_session, a_char_id).await;
    let b_token = request_world_token(&h.auth_url, &b_session, b_char_id).await;
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    let mut b = WorldClient::start(b_token, &b_session, b_char_id).await;
    pump_both_for(&mut a, &mut b, Duration::from_millis(300)).await;

    // A dies where it stands and respawns at the bind, which is the same
    // spot, so the corpse is in the cleric's reach.
    a.send_damage_self(1_000_000);
    let contents = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(5), |m| {
            matches!(m, ServerWorldMsg::CorpseContents { .. })
        })
        .await
        .expect("dying leaves a corpse");
    let ServerWorldMsg::CorpseContents { corpse_id, .. } = contents else { unreachable!() };
    a.send_respawn();
    a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(5), |m| matches!(m, ServerWorldMsg::Teleport { .. }))
        .await
        .expect("respawned");
    pump_both_for(&mut a, &mut b, Duration::from_millis(200)).await;

    // B casts the six-second resurrection on the corpse.
    b.send_cast_start("Resurrection (Minor)", 6.0);
    pump_both_for(&mut a, &mut b, Duration::from_millis(6200)).await;
    b.send_cast_spell("Resurrection (Minor)", Some(corpse_id));
    pump_both_for(&mut a, &mut b, Duration::from_millis(100)).await;
    let offer = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(5), |m| {
            matches!(m, ServerWorldMsg::ResurrectOffer { corpse_id: c, .. } if *c == corpse_id)
        })
        .await
        .expect("the owner is offered the resurrection");
    let ServerWorldMsg::ResurrectOffer { xp_percent, .. } = offer else { unreachable!() };
    assert_eq!(xp_percent, 25);

    // A accepts: summoned, told, and sick for five minutes.
    a.send_resurrect_accept(corpse_id, true);
    pump_both_for(&mut a, &mut b, Duration::from_millis(100)).await;
    let mut msgs = Vec::new();
    let end = Instant::now() + Duration::from_secs(3);
    while Instant::now() < end {
        tick_one(&mut a.client, &mut a.transport);
        tick_one(&mut b.client, &mut b.transport);
        while let Some(bytes) = a.client.receive_message(CHANNEL_SYSTEM) {
            if let Ok((msg, _)) =
                bincode::serde::decode_from_slice::<ServerWorldMsg, _>(&bytes, bincode_cfg())
            {
                msgs.push(msg);
            }
        }
        tokio::time::sleep(TICK_DT).await;
    }
    assert!(
        msgs.iter().any(|m| matches!(m, ServerWorldMsg::Teleport { .. })),
        "the accept summons the player to the corpse"
    );
    let sickness = msgs.iter().find_map(|m| match m {
        ServerWorldMsg::BuffSnapshot { target, buffs } if *target == a_char_id as u64 => buffs
            .iter()
            .find(|(name, _)| name == "Resurrection Sickness")
            .map(|(_, remaining)| *remaining),
        _ => None,
    });
    let remaining = sickness.expect("the owner's buff snapshot carries Resurrection Sickness");
    assert!(
        (295.0..=300.0).contains(&remaining),
        "the sickness lasts five minutes from the accept, got {remaining}"
    );
    assert!(
        msgs.iter().any(|m| matches!(m, ServerWorldMsg::ChatMessage { text, .. } if text == "You are exhausted.")),
        "the owner is told EQ's line"
    );
    // The landing: a quarter of max HP, no mana, both told at once. Regen
    // fans its own updates through the window, so look for the landing
    // values rather than the last word.
    let hp_updates: Vec<(f32, f32)> = msgs.iter().filter_map(|m| match m {
        ServerWorldMsg::HealthUpdate { id, hp, max_hp } if *id == a_char_id as u64 => Some((*hp, *max_hp)),
        _ => None,
    }).collect();
    assert!(
        hp_updates.iter().any(|(hp, max_hp)| (hp - max_hp * 0.25).abs() < 1.0),
        "lands at a quarter of max HP; saw {hp_updates:?}"
    );
    let mp_updates: Vec<f32> = msgs.iter().filter_map(|m| match m {
        ServerWorldMsg::ManaUpdate { id, mp, .. } if *id == a_char_id as u64 => Some(*mp),
        _ => None,
    }).collect();
    assert!(mp_updates.contains(&0.0), "lands with no mana; saw {mp_updates:?}");
    a.send_disconnect();
    b.send_disconnect();
    a.pump_for(Duration::from_millis(200)).await;
    b.pump_for(Duration::from_millis(200)).await;
}

/// Summon Food and Summon Drink (food slice 2, user call 2026-10-15). A level
/// 1 Magician conjures one Conjured Bread into its bags and is told so; a
/// level 35 Magician conjures ten Conjured Water (the cap). Both arrive as inventory
/// deltas, and the merchant will not buy the result (the item has no vendor
/// value; covered by the SellItem rule).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn summon_food_conjures_bread_that_scales_with_level() {
    const BREAD: &str = "res://data/loot/items/conjured_bread.tres";
    const WATER: &str = "res://data/loot/items/conjured_water.tres";
    let h = start_both().await;
    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "sumfood1", "Sumone", "Human", "Magician").await;
    let (b_session, b_char_id, _) =
        provision_client(&h.auth_url, "sumfood2", "Sumtwo", "Human", "Magician").await;
    set_char_level(&h.db_url, b_char_id, 35).await;
    let b_token = request_world_token(&h.auth_url, &b_session, b_char_id).await;
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    let mut b = WorldClient::start(b_token, &b_session, b_char_id).await;
    pump_both_for(&mut a, &mut b, Duration::from_millis(300)).await;

    a.send_cast_start("Summon Food", 4.0);
    b.send_cast_start("Summon Drink", 4.0);
    pump_both_for(&mut a, &mut b, Duration::from_millis(4200)).await;
    a.send_cast_spell("Summon Food", None);
    b.send_cast_spell("Summon Drink", None);
    pump_both_for(&mut a, &mut b, Duration::from_millis(100)).await;

    let bread = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::InventoryDelta { item_path: Some(p), .. } if p == BREAD)
        })
        .await
        .expect("the level 1 Magician's bread lands in its bags");
    let ServerWorldMsg::InventoryDelta { location, count, .. } = bread else { unreachable!() };
    assert_eq!((location.as_str(), count), ("base", 1), "one loaf at level 1");
    a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(2), |m| {
        matches!(m, ServerWorldMsg::ChatMessage { text, .. } if text == "You conjure Conjured Bread x1.")
    })
    .await
    .expect("and is told");

    let water = b
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::InventoryDelta { item_path: Some(p), .. } if p == WATER)
        })
        .await
        .expect("the level 35 Magician's water lands in its bags");
    let ServerWorldMsg::InventoryDelta { count, .. } = water else { unreachable!() };
    assert_eq!(count, 10, "ten flasks at level 35 (one, plus one per level past 1, capped at ten)");
    a.send_disconnect();
    b.send_disconnect();
    a.pump_for(Duration::from_millis(200)).await;
    b.pump_for(Duration::from_millis(200)).await;
}

/// Send a Move along `dir` every tick for `ticks` ticks and return the median
/// distance the server moved this player between consecutive Positions of its
/// own. The server integrates a fixed TICK_DT and fans a moving player's
/// Position each tick, so one step is speed x 0.05 whatever the wall clock
/// does; the median shrugs off a skipped or doubled tick.
async fn median_step(c: &mut WorldClient, char_id: i64, seq: &mut u32, dir: Vec3, ticks: u32) -> f32 {
    let mut xs: Vec<f32> = Vec::new();
    for _ in 0..ticks {
        c.send_move(*seq, dir);
        *seq += 1;
        tick_one(&mut c.client, &mut c.transport);
        while let Some(bytes) = c.client.receive_message(CHANNEL_POSITION) {
            if let Ok((ServerWorldMsg::Position { id, pos, .. }, _)) =
                bincode::serde::decode_from_slice::<ServerWorldMsg, _>(&bytes, bincode_cfg())
            {
                if id == char_id as u64 {
                    xs.push(pos.x);
                }
            }
        }
        tokio::time::sleep(TICK_DT).await;
    }
    let mut steps: Vec<f32> = xs
        .windows(2)
        .map(|w| (w[1] - w[0]).abs())
        .filter(|d| *d > 0.001)
        .collect();
    assert!(steps.len() >= 5, "too few moving Positions to measure ({})", steps.len());
    steps.sort_by(|a, b| a.partial_cmp(b).unwrap());
    steps[steps.len() / 2]
}

/// Client-authority audit fix 2: the server decides how fast a player moves.
/// A full-length Move is a 5 m/s run (0.25 m a tick), where it used to be the
/// 7.5 m/s ceiling the client scaled its own speed against, so a modified
/// client ran 1.5x; a forged over-long direction is no faster; and a heavily
/// overloaded character crawls at the encumbrance floor (a quarter speed),
/// which the client alone used to decide.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_server_decides_run_speed() {
    let h = start_both().await;
    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "runner", "Runna", "Human", "Warrior").await;
    let (b_session, b_char_id, _stale) =
        provision_client(&h.auth_url, "hoarder", "Hoarda", "Human", "Warrior").await;
    // Forty thousand copper weigh 800, far past any level 1 capacity.
    let pool = db::open(&h.db_url).await.expect("open pool");
    sqlx::query("UPDATE characters SET copper = 40000 WHERE id = ?1")
        .bind(b_char_id)
        .execute(&pool)
        .await
        .expect("seed coins");
    let b_token = request_world_token(&h.auth_url, &b_session, b_char_id).await;

    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    a.pump_for(Duration::from_millis(300)).await;
    let mut seq = 1u32;
    let east = Vec3 { x: 1.0, y: 0.0, z: 0.0 };
    let run = median_step(&mut a, a_char_id, &mut seq, east, 30).await;
    assert!((run - 0.25).abs() < 0.02, "a full-length Move runs 5 m/s, 0.25 m a tick: {run}");
    let forged = Vec3 { x: 3.0, y: 0.0, z: 0.0 };
    let forged_run = median_step(&mut a, a_char_id, &mut seq, forged, 30).await;
    assert!((forged_run - 0.25).abs() < 0.02, "an over-long direction is no faster: {forged_run}");
    a.send_disconnect();
    a.pump_for(Duration::from_millis(200)).await;

    let mut b = WorldClient::start(b_token, &b_session, b_char_id).await;
    b.pump_for(Duration::from_millis(300)).await;
    let mut seq = 1u32;
    let crawl = median_step(&mut b, b_char_id, &mut seq, east, 30).await;
    let floor = 0.25 * projectdawn_server::world::encumbrance::MIN_SPEED_MULT;
    assert!((crawl - floor).abs() < 0.01, "an overloaded character crawls at the floor: {crawl}");
    b.send_disconnect();
    b.pump_for(Duration::from_millis(200)).await;
}

/// The jump relay (the "Remote players never show a jump" To-Do, cosmetic
/// version). A watcher stands beside a jumper at the spawn. One Move with
/// `jumping: true` reaches the watcher as exactly one PlayerJumped naming the
/// jumper, and never reaches the jumper; a burst of ten alternating flags
/// inside the half-second interval relays nothing more; an edge after the
/// interval relays once more. The jumper's own Position keeps y at zero
/// throughout: the flag animates peers and moves nobody.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_jump_reaches_the_players_nearby_once_and_moves_nobody() {
    let h = start_both().await;
    let (j_session, j_char_id, _) =
        provision_client(&h.auth_url, "jumper", "Hoppy", "Human", "Warrior").await;
    let (w_session, w_char_id, _) =
        provision_client(&h.auth_url, "jumpwatch", "Seeson", "Human", "Warrior").await;
    let j_token = request_world_token(&h.auth_url, &j_session, j_char_id).await;
    let mut j = WorldClient::start(j_token, &j_session, j_char_id).await;
    let w_token = request_world_token(&h.auth_url, &w_session, w_char_id).await;
    let mut w = WorldClient::start(w_token, &w_session, w_char_id).await;
    pump_both_for(&mut j, &mut w, Duration::from_millis(400)).await;
    // Drain what entering fanned, so the counts below are the jumps only.
    let _ = collect_system(&mut w, Duration::from_millis(200)).await;
    let _ = collect_system(&mut j, Duration::from_millis(200)).await;
    let still = Vec3 { x: 0.0, y: 0.0, z: 0.0 };
    let jumps_in = |msgs: &[ServerWorldMsg]| {
        msgs.iter()
            .filter(|m| matches!(m, ServerWorldMsg::PlayerJumped { id } if *id == j_char_id as u64))
            .count()
    };

    // One jump, then a forged burst of ten alternating flags inside the
    // half-second interval (about 200 ms): exactly one relay in all.
    j.send_move_jump(1, still, true);
    for i in 2..12u32 {
        j.send_move_jump(i, still, i % 2 == 0);
        tick_one(&mut j.client, &mut j.transport);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let (w_msgs, j_msgs) = tokio::join!(
        collect_system(&mut w, Duration::from_millis(600)),
        collect_system(&mut j, Duration::from_millis(600)),
    );
    assert_eq!(jumps_in(&w_msgs), 1, "the watcher sees one jump; the burst inside the interval adds none");
    assert_eq!(jumps_in(&j_msgs), 0, "the jumper is not told about their own jump");

    // Past the interval, the next edge relays again.
    pump_both_for(&mut j, &mut w, Duration::from_millis(400)).await;
    j.send_move_jump(20, still, false);
    tick_one(&mut j.client, &mut j.transport);
    tokio::time::sleep(TICK_DT).await;
    j.send_move_jump(21, still, true);
    let (w_msgs, _) = tokio::join!(
        collect_system(&mut w, Duration::from_millis(600)),
        collect_system(&mut j, Duration::from_millis(600)),
    );
    assert_eq!(jumps_in(&w_msgs), 1, "a jump after the interval relays once more");

    // Nobody moved for any of it: the jumper's own Position keeps y at zero.
    j.send_move(30, still);
    let pos = own_position(&mut j, j_char_id).await.expect("the jumper's own Position");
    assert!(pos.y.abs() < 1e-3, "the server never lifts anyone for a jump: y = {}", pos.y);

    j.send_disconnect();
    w.send_disconnect();
    j.pump_for(Duration::from_millis(200)).await;
    w.pump_for(Duration::from_millis(200)).await;
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
    db::set_account_gm(&pool, "sweeper", true).await.expect("set is_gm");
    let a_token = request_world_token(&h.auth_url, &a_session, a_char_id).await;
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    let mut b = WorldClient::start(b_token, &b_session, b_char_id).await;

    let warder: u64 = match b
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::PetSpawn { owner, .. } if *owner == b_char_id as u64)
        })
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
            .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
                matches!(m, ServerWorldMsg::EnemySpawn { mob_name, .. } if mob_name == name)
            })
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
            .wait_for(CHANNEL_SYSTEM, Duration::from_secs(1), |m| {
                matches!(m, ServerWorldMsg::EntityDespawn { id } if dummies.contains(id))
            })
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
        let ServerWorldMsg::EntityDespawn { id } = msg else { unreachable!() };
        assert_ne!(id, warder, "a player's pet is not an enemy to despawn");
        despawned.push(id);
    }
    let warder_gone = b
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(1), |m| {
            matches!(m, ServerWorldMsg::EntityDespawn { id } if *id == warder)
        })
        .await;
    assert!(warder_gone.is_none(), "the warder stays");

    // The GM is told the count (camp mobs in view may add to the two).
    let told = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(2), |m| {
            matches!(m, ServerWorldMsg::ChatMessage { text, .. } if text.starts_with("Despawned "))
        })
        .await
        .expect("the GM is told how many went");
    let ServerWorldMsg::ChatMessage { text, .. } = told else { unreachable!() };
    let count: usize = text
        .trim_start_matches("Despawned ")
        .trim_end_matches(" enemies.")
        .parse()
        .expect("the line carries a count");
    assert!(count >= dummies.len(), "the count covers both dummies ({text})");

    a.send_cast_spell("Smite", Some(dummies[0]));
    let refused = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::CastFail { caster, reason }
                if *caster == a_char_id as u64 && reason.contains("no longer here"))
        })
        .await;
    assert!(refused.is_some(), "the server no longer has the mob, so a Smite at it is refused");

    // Audited like every dev command: one row, the GM's; the plain account's
    // refused request wrote nothing.
    let rows: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM gm_actions WHERE command = 'dev_despawn_enemies'")
            .fetch_one(&pool)
            .await
            .expect("read gm_actions");
    assert_eq!(rows, 1, "the GM's despawn is recorded, the plain account's is not");

    a.send_disconnect();
    b.send_disconnect();
    a.pump_for(Duration::from_millis(200)).await;
    b.pump_for(Duration::from_millis(200)).await;
}

/// Client-authority audit fix 3, the user's decision of 2026-10-19 ("move the
/// wisdom bonus on to the server"): a heal adds the caster's WIS x 0.3. B, a
/// GM Warrior beside A, takes 15 dev damage of its 28; A, a Cleric, casts
/// Healing Light (10) on B; B's HP comes back by exactly 10. A heal is its
/// listed number and nothing more (the EQ parity audit of 2026-10-20): fix 3
/// of the client-authority audit used to add WIS x 0.3, which classic EQ
/// never did.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_heal_is_its_listed_number() {
    let h = start_both().await;
    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "wisheal", "Wisa", "Human", "Cleric").await;
    let (b_session, b_char_id, _stale) =
        provision_client(&h.auth_url, "wishurt", "Hurta", "Human", "Warrior").await;
    let pool = db::open(&h.db_url).await.expect("open pool");
    db::set_account_gm(&pool, "wishurt", true).await.expect("set is_gm");
    let b_token = request_world_token(&h.auth_url, &b_session, b_char_id).await;
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    let mut b = WorldClient::start(b_token, &b_session, b_char_id).await;
    pump_both_for(&mut a, &mut b, Duration::from_millis(300)).await;

    b.send_damage_self(15);
    let hurt = wait_on_a_pumping_b(&mut a, &mut b, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::HealthUpdate { id, hp, max_hp } if *id == b_char_id as u64 && *hp < *max_hp)
    })
    .await
    .expect("B is hurt");
    let ServerWorldMsg::HealthUpdate { hp: hurt_hp, .. } = hurt else { unreachable!() };

    a.send_cast_start("Healing Light", 1.2);
    pump_both_for(&mut a, &mut b, Duration::from_millis(1_400)).await;
    a.send_cast_spell("Healing Light", Some(b_char_id as u64));
    let healed = wait_on_a_pumping_b(&mut a, &mut b, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::HealthUpdate { id, hp, .. } if *id == b_char_id as u64 && *hp > hurt_hp)
    })
    .await
    .expect("the heal lands");
    let ServerWorldMsg::HealthUpdate { hp: healed_hp, .. } = healed else { unreachable!() };
    // B's own regen may add a point or two in the 1.4 s before the heal; the
    // old server's 10 + WIS x 0.3 (WIS 24: 17) sits above the window.
    let gained = healed_hp - hurt_hp;
    assert!(
        gained >= 10.0 - 0.01 && gained <= 12.01,
        "healed {gained}; Healing Light's listed 10 and nothing more"
    );
}

/// Client-authority audit fix 4: the server rolls the melee miss. A Rogue
/// swings the Carved Staff, two-handed blunt, a weapon type the class cannot
/// use (an 80% miss for half damage), at a dummy beside them. Across eight
/// swings the server answers with at least one Miss (eight straight landings
/// would be about one in 400,000). The old server landed every in-range swing:
/// only the client ever rolled a miss, and a modified client never did.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_server_rolls_the_melee_miss() {
    const STAFF: &str = "res://data/loot/items/carved_staff.tres";
    let h = start_both().await;
    let (a_session, a_char_id, _stale) =
        provision_client(&h.auth_url, "misser", "Missa", "Human", "Rogue").await;
    let pool = db::open(&h.db_url).await.expect("open pool");
    db::set_account_gm(&pool, "misser", true).await.expect("set is_gm");
    give_char_item(&h.db_url, a_char_id, "equip", 0, STAFF, 1).await;
    let a_token = request_world_token(&h.auth_url, &a_session, a_char_id).await;
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    a.pump_for(Duration::from_millis(300)).await;

    a.send_dev_spawn("Miss Dummy", 1, 100_000.0, 0, 0.0, 0.0);
    let spawn = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::EnemySpawn { mob_name, .. } if mob_name == "Miss Dummy")
        })
        .await
        .expect("the dummy spawns");
    let (dummy, dummy_pos) = match spawn {
        ServerWorldMsg::EnemySpawn { id, pos, .. } => (id, pos),
        _ => unreachable!(),
    };
    let mut seq: u32 = 1;
    walk_until_within(&mut a, &mut seq, a_char_id, Vec3 { x: 0.0, y: 0.0, z: 0.0 }, dummy_pos, 1.0).await;

    let (mut misses, mut hits) = (0, 0);
    for _ in 0..8 {
        a.send_attack(dummy, "", false, DamageType::Physical);
        let until = Instant::now() + Duration::from_millis(1_100);
        while Instant::now() < until {
            tick_one(&mut a.client, &mut a.transport);
            while let Some(bytes) = a.client.receive_message(CHANNEL_SYSTEM) {
                match bincode::serde::decode_from_slice::<ServerWorldMsg, _>(&bytes, bincode_cfg()) {
                    Ok((ServerWorldMsg::Miss { attacker, target }, _))
                        if attacker == a_char_id as u64 && target == dummy => misses += 1,
                    Ok((ServerWorldMsg::Hit { attacker, target, .. }, _))
                        if attacker == a_char_id as u64 && target == dummy => hits += 1,
                    _ => {}
                }
            }
            tokio::time::sleep(TICK_DT).await;
        }
    }
    assert_eq!(misses + hits, 8, "every swing was answered: {misses} misses, {hits} hits");
    assert!(misses >= 1, "the server rolled at least one miss ({hits} hits)");
}

/// Send one Attack from `c` and report whether the server answered it with a
/// Hit or a Miss from `attacker` within 1.2 s (a refused swing gets nothing).
async fn swing_answered(c: &mut WorldClient, attacker: i64, target: u64, offhand: bool) -> bool {
    c.send_attack(target, "", offhand, DamageType::Physical);
    let answer = c
        .wait_for(CHANNEL_SYSTEM, Duration::from_millis(1_200), |m| {
            matches!(m, ServerWorldMsg::Hit { attacker: a, target: t, .. }
                | ServerWorldMsg::Miss { attacker: a, target: t }
                if *a == attacker as u64 && *t == target)
        })
        .await;
    answer.is_some()
}

/// Dual wield is earned (user, 2026-10-19): Monks are born with it (bare
/// fists in both hands), Warriors get it at 15. A Warrior 1 with a sword in
/// each hand: the main hand is answered, the off hand is not; leveled past 15
/// (the GM XP tool), the off hand is answered. A bare-handed Monk 1's off hand
/// is answered. On the previous code any class's off hand swung from level 1,
/// and a Monk's empty off hand was refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dual_wield_is_earned_by_class_and_level() {
    const SWORD: &str = "res://data/loot/items/iron_short_sword.tres";
    let h = start_both().await;
    let (a_session, a_char_id, _stale) =
        provision_client(&h.auth_url, "dwwarrior", "Duala", "Human", "Warrior").await;
    let (b_session, b_char_id, b_token) =
        provision_client(&h.auth_url, "dwmonk", "Dualb", "Human", "Monk").await;
    let pool = db::open(&h.db_url).await.expect("open pool");
    db::set_account_gm(&pool, "dwwarrior", true).await.expect("set is_gm");
    give_char_item(&h.db_url, a_char_id, "equip", 0, SWORD, 1).await;
    give_char_item(&h.db_url, a_char_id, "equip", 1, SWORD, 1).await;
    let a_token = request_world_token(&h.auth_url, &a_session, a_char_id).await;
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    let mut b = WorldClient::start(b_token, &b_session, b_char_id).await;
    pump_both_for(&mut a, &mut b, Duration::from_millis(300)).await;

    a.send_dev_spawn("Dual Dummy", 1, 100_000.0, 0, 0.0, 0.0);
    let spawn = wait_on_a_pumping_b(&mut a, &mut b, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::EnemySpawn { mob_name, .. } if mob_name == "Dual Dummy")
    })
    .await
    .expect("the dummy spawns");
    let (dummy, dummy_pos) = match spawn {
        ServerWorldMsg::EnemySpawn { id, pos, .. } => (id, pos),
        _ => unreachable!(),
    };
    let origin = Vec3 { x: 0.0, y: 0.0, z: 0.0 };
    let (mut sa, mut sb) = (1u32, 1u32);
    walk_until_within(&mut a, &mut sa, a_char_id, origin, dummy_pos, 1.0).await;
    walk_until_within(&mut b, &mut sb, b_char_id, origin, dummy_pos, 1.0).await;
    b.send_heartbeat();

    assert!(swing_answered(&mut a, a_char_id, dummy, false).await, "the Warrior's main hand swings");
    assert!(!swing_answered(&mut a, a_char_id, dummy, true).await, "a Warrior 1's off hand does not");
    assert!(swing_answered(&mut b, b_char_id, dummy, true).await, "a bare-handed Monk 1's off hand punches");

    a.send_grant_quest_xp(20_000_000);
    let leveled = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::LevelUp { new_level, .. } if *new_level >= 15)
        })
        .await;
    assert!(leveled.is_some(), "the GM XP grant takes the Warrior past 15");
    a.pump_for(Duration::from_millis(1_100)).await;
    assert!(swing_answered(&mut a, a_char_id, dummy, true).await, "past 15, the Warrior's off hand swings");
}

/// Per-spell ranges (user, 2026-10-19, on Monsters and Memories' 30 m
/// baseline): a hostile spell reaches 30 m, as friendly ones already did. A
/// GM Cleric backs off from a dummy to about 28 m and Smites it: it lands,
/// where the old server's 25 m hostile reach refused it. Just past 30 m the
/// same Smite is refused, before any mana comes off: the reach is exact
/// (user, 2026-10-27), with no slack for the client's own full-reach check.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_nuke_reaches_thirty_metres() {
    let h = start_both().await;
    let (a_session, a_char_id, _stale) =
        provision_client(&h.auth_url, "reacher", "Reacha", "Human", "Cleric").await;
    let pool = db::open(&h.db_url).await.expect("open pool");
    db::set_account_gm(&pool, "reacher", true).await.expect("set is_gm");
    let a_token = request_world_token(&h.auth_url, &a_session, a_char_id).await;
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    a.pump_for(Duration::from_millis(300)).await;

    a.send_dev_spawn("Reach Dummy", 1, 100_000.0, 0, 0.0, 0.0);
    let spawn = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::EnemySpawn { mob_name, .. } if mob_name == "Reach Dummy")
        })
        .await
        .expect("the dummy spawns");
    let (dummy, dummy_pos) = match spawn {
        ServerWorldMsg::EnemySpawn { id, pos, .. } => (id, pos),
        _ => unreachable!(),
    };
    let origin = Vec3 { x: 0.0, y: 0.0, z: 0.0 };
    let mut seq = 1u32;
    let at = |d: f32| Vec3 { x: dummy_pos.x, y: 0.0, z: dummy_pos.z + d };

    let stood = walk_until_within(&mut a, &mut seq, a_char_id, origin, at(28.0), 0.3).await;
    let dist = ((stood.x - dummy_pos.x).powi(2) + (stood.z - dummy_pos.z).powi(2)).sqrt();
    assert!((27.0..29.5).contains(&dist), "test setup: the caster stands at about 28 m, is at {dist:.1}");
    a.send_cast_spell("Smite", Some(dummy));
    let landed = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::Hit { attacker, target, .. }
                if *attacker == a_char_id as u64 && *target == dummy)
        })
        .await;
    assert!(landed.is_some(), "a Smite at about 28 m lands (the old 25 m reach refused it)");

    // Just past 30 m: refused, the reach being exact (user, 2026-10-27: a
    // 30 m spell is out of range at 30.1). Smite's own 3 s cooldown runs out
    // on the walk.
    a.pump_for(Duration::from_millis(3_100)).await;
    let stood = walk_until_within(&mut a, &mut seq, a_char_id, stood, at(30.6), 0.2).await;
    let dist = ((stood.x - dummy_pos.x).powi(2) + (stood.z - dummy_pos.z).powi(2)).sqrt();
    assert!((30.05..31.0).contains(&dist), "test setup: the caster stands just past 30 m, is at {dist:.2}");
    // A level 1 pool (about 16) does not hold two Smites (12 each), and the
    // mana gate runs before the reach gate: top it off first.
    a.send_heal_self();
    a.pump_for(Duration::from_millis(200)).await;
    a.send_cast_spell("Smite", Some(dummy));
    let refused = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::CastFail { caster, reason }
                if *caster == a_char_id as u64 && reason.contains("too far away"))
        })
        .await;
    assert!(refused.is_some(), "at {dist:.2} m, just past 30, the Smite is refused");
}

/// Wait for the next InventoryDelta naming this paperdoll slot, if one comes
/// within the window.
async fn next_equip_delta(c: &mut WorldClient, equip_slot: u32, window: Duration) -> Option<Option<String>> {
    match c
        .wait_for(CHANNEL_SYSTEM, window, |m| {
            matches!(m, ServerWorldMsg::InventoryDelta { location, slot, .. }
                if location == "equip" && *slot == equip_slot)
        })
        .await
    {
        Some(ServerWorldMsg::InventoryDelta { item_path, .. }) => Some(item_path),
        _ => None,
    }
}

/// EQ's equip rule (2026-10-27, after a Cleric wore two clubs on the R720):
/// the item refuses the class at the equip. A Cleric's club into the off hand
/// is refused with "Your class cannot dual wield." and the paperdoll never
/// changes; the same club into the main hand is worn; the Cleric's sword
/// (1h_slashing, a skill Clerics never train) is refused as unwieldable; a
/// Monk's club goes into the off hand (born with dual wield) while a leather
/// vest is refused as unwearable (Monks train cloth alone). On the previous
/// server every one of these went on; the swing gate alone kept the off hand
/// idle.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn equipping_is_refused_by_class_and_dual_wield() {
    const CLUB: &str = "res://data/loot/items/cracked_wooden_club.tres";
    const SWORD: &str = "res://data/loot/items/iron_short_sword.tres";
    const VEST: &str = "res://data/loot/items/cracked_leather_vest.tres";
    let h = start_both().await;
    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "eqcleric", "Equipa", "Human", "Cleric").await;
    let (b_session, b_char_id, b_token) =
        provision_client(&h.auth_url, "eqmonk", "Equipb", "Human", "Monk").await;
    give_char_item(&h.db_url, a_char_id, "base", 0, CLUB, 1).await;
    give_char_item(&h.db_url, a_char_id, "base", 1, SWORD, 1).await;
    give_char_item(&h.db_url, b_char_id, "base", 0, CLUB, 1).await;
    give_char_item(&h.db_url, b_char_id, "base", 1, VEST, 1).await;
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    let mut b = WorldClient::start(b_token, &b_session, b_char_id).await;
    pump_both_for(&mut a, &mut b, Duration::from_millis(300)).await;

    // The Cleric's club into the off hand: the line, and no paperdoll change.
    a.send_equip_item("base", 0, 1);
    let line = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::ChatMessage { text, .. } if text.contains("cannot dual wield"))
        })
        .await;
    assert!(line.is_some(), "a Cleric's off-hand weapon is refused with the dual wield line");
    assert!(
        next_equip_delta(&mut a, 1, Duration::from_millis(500)).await.is_none(),
        "the refused club never reaches the off hand"
    );

    // The same club into the main hand: worn (Clerics train 1h_blunt).
    a.send_equip_item("base", 0, 0);
    let worn = next_equip_delta(&mut a, 0, Duration::from_secs(3)).await;
    assert_eq!(worn, Some(Some(CLUB.to_string())), "the Cleric's club goes into the main hand");

    // The sword: a skill the class never trains, refused at the equip.
    a.send_equip_item("base", 1, 0);
    let line = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::ChatMessage { text, .. } if text.contains("cannot wield"))
        })
        .await;
    assert!(line.is_some(), "a Cleric's sword is refused as unwieldable");
    assert!(
        next_equip_delta(&mut a, 0, Duration::from_millis(500)).await.is_none(),
        "the sword never replaces the club"
    );

    // The Monk: the club into the off hand goes on; the leather vest does not.
    b.send_equip_item("base", 0, 1);
    let worn = next_equip_delta(&mut b, 1, Duration::from_secs(3)).await;
    assert_eq!(worn, Some(Some(CLUB.to_string())), "a Monk 1's club goes into the off hand");
    b.send_equip_item("base", 1, 3);
    let line = b
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::ChatMessage { text, .. } if text.contains("cannot wear"))
        })
        .await;
    assert!(line.is_some(), "a Monk's leather vest is refused as unwearable");
    assert!(
        next_equip_delta(&mut b, 3, Duration::from_millis(500)).await.is_none(),
        "the vest never reaches the chest slot"
    );
}

/// A `/say` carries SAY_RANGE (50 m; user, 2026-10-27: "the volume of a
/// conversation") and no further. A listener 40 m from the speaker hears a
/// say; walked out to 60 m it hears nothing, while a shout still reaches it
/// (so the silence is the range, not the link). On the previous server a say
/// reached the speaker's whole 3x3 AOI neighbourhood of 120 m cells, so the
/// 60 m listener heard it too. Row z = 80 is clear of every camp.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_say_carries_fifty_metres_and_no_further() {
    let h = start_both().await;
    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "sayrange_a", "Sayer", "Human", "Warrior").await;
    let (b_session, b_char_id, b_token) =
        provision_client(&h.auth_url, "sayrange_b", "Hearer", "Human", "Warrior").await;
    set_char_pos(&h.db_url, a_char_id, 0.0, 80.0).await;
    set_char_pos(&h.db_url, b_char_id, 40.0, 80.0).await;
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    let mut b = WorldClient::start(b_token, &b_session, b_char_id).await;
    pump_both_for(&mut a, &mut b, Duration::from_millis(300)).await;

    a.send_say("near");
    let heard = wait_on_a_pumping_b(&mut b, &mut a, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::ChatMessage { text, .. } if text == "near")
    })
    .await;
    assert!(heard.is_some(), "a say reaches a listener 40 m away");

    let mut seq = 1u32;
    let stood = walk_until_within(
        &mut b,
        &mut seq,
        b_char_id,
        Vec3 { x: 40.0, y: 0.0, z: 80.0 },
        Vec3 { x: 60.0, y: 0.0, z: 80.0 },
        0.3,
    )
    .await;
    assert!(stood.x > 55.0, "test setup: the listener stands past 55 m, is at x={:.1}", stood.x);
    a.send_say("far");
    let heard = wait_on_a_pumping_b(&mut b, &mut a, Duration::from_millis(1_500), |m| {
        matches!(m, ServerWorldMsg::ChatMessage { text, .. } if text == "far")
    })
    .await;
    assert!(heard.is_none(), "a say does not reach a listener 60 m away");

    send_msg(
        &mut a.client,
        CHANNEL_SYSTEM,
        &ClientWorldMsg::Chat {
            channel: protocol::world::ChatChannel::Shout,
            text: "still here".into(),
            target_name: None,
        },
    );
    let heard = wait_on_a_pumping_b(&mut b, &mut a, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::ChatMessage { text, .. } if text == "still here")
    })
    .await;
    assert!(heard.is_some(), "a shout still reaches the 60 m listener: the silence was the range");
}

/// Pump both clients for `window` and return every yaw `watcher` receives in
/// a Position for `id`, in order.
async fn yaws_seen(watcher: &mut WorldClient, other: &mut WorldClient, id: u64, window: Duration) -> Vec<f32> {
    let mut seen = Vec::new();
    watcher.send_heartbeat();
    other.send_heartbeat();
    let end = Instant::now() + window;
    while Instant::now() < end {
        tick_one(&mut watcher.client, &mut watcher.transport);
        tick_one(&mut other.client, &mut other.transport);
        while let Some(bytes) = watcher.client.receive_message(CHANNEL_POSITION) {
            if let Ok((ServerWorldMsg::Position { id: pid, yaw, .. }, _)) =
                bincode::serde::decode_from_slice::<ServerWorldMsg, _>(&bytes, bincode_cfg())
            {
                if pid == id {
                    seen.push(yaw);
                }
            }
        }
        tokio::time::sleep(TICK_DT).await;
    }
    seen
}

/// The facing (PD_W0029; user, 2026-10-27, option a): a Move carries the
/// client's own body yaw and the Position fan relays it, so B sees A turn.
/// A stands 5 m from B and sends a Move with yaw 1.25: B's Positions for A
/// carry 1.25. A Move with a non-finite yaw is dropped whole (B keeps seeing
/// 1.25), and a yaw past PI comes back wrapped into (-PI, PI]. On the previous
/// server A's yaw stayed at its login value whatever the client sent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_players_facing_reaches_the_watcher() {
    use std::f32::consts::TAU;
    let h = start_both().await;
    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "facing_a", "Turner", "Human", "Warrior").await;
    let (b_session, b_char_id, b_token) =
        provision_client(&h.auth_url, "facing_b", "Watcher", "Human", "Warrior").await;
    set_char_pos(&h.db_url, a_char_id, 0.0, 80.0).await;
    set_char_pos(&h.db_url, b_char_id, 5.0, 80.0).await;
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    let mut b = WorldClient::start(b_token, &b_session, b_char_id).await;
    pump_both_for(&mut a, &mut b, Duration::from_millis(300)).await;
    let a_id = a_char_id as u64;
    let still = Vec3 { x: 0.0, y: 0.0, z: 0.0 };

    a.send_move_facing(1, still, 1.25);
    let seen = yaws_seen(&mut b, &mut a, a_id, Duration::from_millis(600)).await;
    let last = *seen.last().expect("B receives A's Positions");
    assert!((last - 1.25).abs() < 1e-4, "B sees A facing 1.25, sees {last}");

    a.send_move_facing(2, still, f32::NAN);
    let seen = yaws_seen(&mut b, &mut a, a_id, Duration::from_millis(400)).await;
    assert!(!seen.is_empty(), "B keeps receiving A's Positions");
    assert!(
        seen.iter().all(|y| (y - 1.25).abs() < 1e-4),
        "a non-finite yaw is dropped whole: B still sees 1.25, saw {seen:?}"
    );

    a.send_move_facing(3, still, 4.0);
    let seen = yaws_seen(&mut b, &mut a, a_id, Duration::from_millis(600)).await;
    let last = *seen.last().expect("B receives A's Positions");
    assert!((last - (4.0 - TAU)).abs() < 1e-4, "a yaw past PI is wrapped into (-PI, PI]: {last}");
}

/// Wait for the next SpellGems and return (gems, seeded).
async fn next_spell_gems(c: &mut WorldClient) -> Option<(Vec<String>, bool)> {
    match c
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| matches!(m, ServerWorldMsg::SpellGems { .. }))
        .await
    {
        Some(ServerWorldMsg::SpellGems { gems, seeded }) => Some((gems, seeded)),
        _ => None,
    }
}

/// Client-authority audit fix 5: the server refuses a cast of a spell in no
/// gem, before anything is spent. A Cleric whose bar the server never held
/// (as every character was before migration 0015) enters with an empty,
/// unseeded bar and casts Smite: refused with the line and the true mana.
/// The old server cast any spell the class and level allowed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_spell_not_memorized_is_refused() {
    let h = start_both().await;
    let (session, char_id, token) =
        provision_client(&h.auth_url, "unmem", "Unmema", "Human", "Cleric").await;
    forget_all_spells(&h.db_url, char_id).await;
    let mut a = WorldClient::start(token, &session, char_id).await;
    let (gems, seeded) = next_spell_gems(&mut a).await.expect("the bar arrives on entry");
    assert!(gems.iter().all(|g| g.is_empty()) && !seeded, "an empty bar the server never held");

    a.send_cast_spell("Smite", None);
    let refused = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::CastFail { caster, reason }
                if *caster == char_id as u64 && reason == "You have not memorized that spell.")
        })
        .await;
    assert!(refused.is_some(), "a spell in no gem is refused");
    let mana = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(2), |m| {
            matches!(m, ServerWorldMsg::ManaUpdate { id, .. } if *id == char_id as u64)
        })
        .await;
    assert!(
        matches!(mana, Some(ServerWorldMsg::ManaUpdate { mp, max_mp, .. }) if mp == max_mp),
        "and nothing was spent"
    );
}

/// Memorizing is the server's: seated two seconds, an open gem, and no mana
/// (as in EQ: the mana is paid at the cast; user, 2026-10-26, where it used to
/// cost half the spell's mana). Finishing early is refused, so is standing up
/// in between, so is a gem not yet open; a memorize that runs its time fills
/// the gem with the mana untouched, and the cast then gets past the memorize
/// gate.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn memorizing_takes_two_seated_seconds_and_no_mana() {
    let h = start_both().await;
    let (session, char_id, token) =
        provision_client(&h.auth_url, "memer", "Memera", "Human", "Cleric").await;
    forget_all_spells(&h.db_url, char_id).await;
    let mut a = WorldClient::start(token, &session, char_id).await;
    next_spell_gems(&mut a).await.expect("the bar arrives on entry");
    a.send_sit();
    a.pump_for(Duration::from_millis(200)).await;

    // A gem not open at level 1 (three are).
    a.send_memorize_start(3, "Smite");
    let (gems, _) = next_spell_gems(&mut a).await.expect("a refusal answers with the bar");
    assert!(gems[3].is_empty(), "gem 3 is not open at level 1");

    // Finished too early.
    a.send_memorize_start(0, "Smite");
    a.pump_for(Duration::from_millis(300)).await;
    a.send_memorize_finish();
    let (gems, _) = next_spell_gems(&mut a).await.expect("a refusal answers with the bar");
    assert!(gems[0].is_empty(), "a third of a second is not a memorize");

    // Stood up and sat down again in between.
    a.send_memorize_start(0, "Smite");
    a.pump_for(Duration::from_millis(300)).await;
    a.send_stand();
    a.pump_for(Duration::from_millis(200)).await;
    a.send_sit();
    a.pump_for(Duration::from_millis(1_800)).await;
    a.send_memorize_finish();
    let (gems, _) = next_spell_gems(&mut a).await.expect("a refusal answers with the bar");
    assert!(gems[0].is_empty(), "standing up voids the memorize");

    // The real thing.
    a.send_memorize_start(0, "Smite");
    a.pump_for(Duration::from_millis(2_150)).await;
    a.send_memorize_finish();
    // Everything the server says for a second: the new bar, and no mana
    // taken (a born-full Cleric stays full; the old server sent mp = max - 6).
    let said = collect_system(&mut a, Duration::from_secs(1)).await;
    let bar = said.iter().find_map(|m| match m {
        ServerWorldMsg::SpellGems { gems, seeded } => Some((gems.clone(), *seeded)),
        _ => None,
    });
    let (gems, seeded) = bar.expect("the new bar");
    assert_eq!(gems[0], "Smite");
    assert!(seeded, "the server holds the bar now");
    for m in &said {
        if let ServerWorldMsg::ManaUpdate { id, mp, max_mp } = m {
            if *id == char_id as u64 {
                assert!((max_mp - mp).abs() < 0.01, "memorizing takes no mana: {mp} of {max_mp}");
            }
        }
    }

    a.send_cast_spell("Smite", None);
    let fail = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::CastFail { caster, .. } if *caster == char_id as u64)
        })
        .await;
    let Some(ServerWorldMsg::CastFail { reason, .. }) = fail else { panic!("an untargeted Smite still fails") };
    assert_ne!(reason, "You have not memorized that spell.", "the memorize gate lets it through ({reason})");
}

/// The bars players already had lived on their own machines: a character the
/// server never held a bar for may hand its bar over once, every spell checked
/// as a memorize would check it (Fireball is no Cleric's; gem 3 is not open at
/// level 1). A second seed changes nothing, and the bar is the same after a
/// relog.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_bar_seeds_once_and_survives_a_relog() {
    let h = start_both().await;
    let (session, char_id, token) =
        provision_client(&h.auth_url, "seeder", "Seeda", "Human", "Cleric").await;
    forget_all_spells(&h.db_url, char_id).await;
    let mut a = WorldClient::start(token, &session, char_id).await;
    next_spell_gems(&mut a).await.expect("the bar arrives on entry");

    a.send_seed_spell_gems(&["Smite", "Fireball", "", "Healing Light"]);
    let (gems, seeded) = next_spell_gems(&mut a).await.expect("the seeded bar");
    assert_eq!(gems[0], "Smite");
    assert!(gems[1].is_empty() && gems[3].is_empty(), "another class's spell and a closed gem are dropped");
    assert!(seeded);

    a.send_seed_spell_gems(&["Healing Light"]);
    let (gems, _) = next_spell_gems(&mut a).await.expect("a second seed answers with the bar");
    assert_eq!(gems[0], "Smite", "a second seed changes nothing");

    a.send_disconnect();
    a.pump_for(Duration::from_millis(500)).await;
    drop(a);
    tokio::time::sleep(Duration::from_millis(500)).await;
    let token = request_world_token(&h.auth_url, &session, char_id).await;
    let mut b = WorldClient::start(token, &session, char_id).await;
    let (gems, seeded) = next_spell_gems(&mut b).await.expect("the bar arrives again");
    assert_eq!(gems[0], "Smite");
    assert!(seeded, "and is still the server's");
}

/// NO RENT (EverQuest's TEMPORARY tag; user 2026-10-19): a character logged
/// out for thirty minutes loses its NO RENT items at the next login; ten
/// minutes away keeps them; an ordinary item is never touched. Logging out
/// stamps the time.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_rent_items_go_after_thirty_minutes_logged_out() {
    const CONJURED: &str = "res://data/loot/items/conjured_bread.tres";
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64;
    let h = start_both().await;
    let pool = db::open(&h.db_url).await.expect("open pool");

    let (a_session, a_char_id, a_token) =
        provision_client(&h.auth_url, "longgone", "Longa", "Human", "Magician").await;
    let (b_session, b_char_id, b_token) =
        provision_client(&h.auth_url, "quickrelog", "Quicka", "Human", "Magician").await;
    for (id, secs_ago) in [(a_char_id, 31 * 60), (b_char_id, 10 * 60)] {
        give_char_item(&h.db_url, id, "base", 0, CONJURED, 4).await;
        give_char_item(&h.db_url, id, "base", 1, BREAD, 2).await;
        sqlx::query("UPDATE characters SET logged_out_at = ?1 WHERE id = ?2")
            .bind(now - secs_ago)
            .bind(id)
            .execute(&pool)
            .await
            .expect("stamp the logout");
    }

    let mut paths_of = Vec::new();
    for (session, id, token) in [(a_session, a_char_id, a_token), (b_session, b_char_id, b_token)] {
        let mut c = WorldClient::start(token, &session, id).await;
        let snap = c
            .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| matches!(m, ServerWorldMsg::InventorySnapshot { .. }))
            .await;
        let Some(ServerWorldMsg::InventorySnapshot { entries }) = snap else { panic!("an inventory on entry") };
        paths_of.push(entries.into_iter().map(|e| e.2).collect::<Vec<String>>());
        c.send_disconnect();
        c.pump_for(Duration::from_millis(500)).await;
    }
    assert!(!paths_of[0].iter().any(|p| p == CONJURED), "thirty minutes away: the conjured bread is gone");
    assert!(paths_of[0].iter().any(|p| p == BREAD), "an ordinary loaf stays");
    assert!(paths_of[1].iter().any(|p| p == CONJURED), "ten minutes away: the conjured bread stays");

    tokio::time::sleep(Duration::from_millis(500)).await;
    let stamped: Option<i64> = sqlx::query_scalar("SELECT logged_out_at FROM characters WHERE id = ?1")
        .bind(b_char_id)
        .fetch_one(&pool)
        .await
        .expect("read the stamp");
    assert!(stamped.is_some_and(|t| (t - now).abs() < 30), "logging out stamped the time: {stamped:?}");
}

// ── Active skills on the server (audit item 6, 2026-10-19) ─────────────────
// None of these pass on the previous server: it had no UseSkill handler.

async fn spawn_dummy(c: &mut WorldClient, name: &str, dmg: i32, aggro: f32) -> (u64, Vec3) {
    c.send_dev_spawn(name, 1, 100_000.0, dmg, 0.0, aggro);
    let spawn = c
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::EnemySpawn { mob_name, .. } if mob_name == name)
        })
        .await
        .expect("the dummy spawns");
    match spawn {
        ServerWorldMsg::EnemySpawn { id, pos, .. } => (id, pos),
        _ => unreachable!(),
    }
}

/// A GM of `class`, fists mastered, standing within 1 m of a fresh dummy.
async fn gm_beside_dummy(h: &Harness, user: &str, class: &str, dummy: &str, dmg: i32, aggro: f32) -> (WorldClient, i64, u64) {
    let (session, char_id, _stale) = provision_client(&h.auth_url, user, &format!("{user}a"), "Human", class).await;
    let pool = db::open(&h.db_url).await.expect("open pool");
    db::set_account_gm(&pool, user, true).await.expect("set is_gm");
    master_weapon_skill(&h.db_url, char_id, "hand_to_hand").await;
    let token = request_world_token(&h.auth_url, &session, char_id).await;
    let mut c = WorldClient::start(token, &session, char_id).await;
    c.pump_for(Duration::from_millis(300)).await;
    let (id, pos) = spawn_dummy(&mut c, dummy, dmg, aggro).await;
    let mut seq: u32 = 1;
    walk_until_within(&mut c, &mut seq, char_id, Vec3 { x: 0.0, y: 0.0, z: 0.0 }, pos, 1.0).await;
    (c, char_id, id)
}

/// The server's answer to a skill swing within 1.5 s: Ok(Some(amount)) for a
/// Hit, Ok(None) for a Miss, Err(reason) for a SkillFail.
async fn skill_answer(c: &mut WorldClient, user: i64, skill: &str, target: u64) -> Result<Option<i32>, String> {
    c.send_use_skill(skill, Some(target));
    for m in c.collect_for(Duration::from_millis(1_500)).await {
        match m {
            ServerWorldMsg::Hit { attacker, target: t, amount, .. } if attacker == user as u64 && t == target => return Ok(Some(amount)),
            ServerWorldMsg::Miss { attacker, target: t } if attacker == user as u64 && t == target => return Ok(None),
            ServerWorldMsg::SkillFail { skill_name, reason } if skill_name == skill => return Err(reason),
            _ => {}
        }
    }
    Err("no answer".into())
}

fn mob_hits_on(msgs: &[ServerWorldMsg], mob: u64, player: i64) -> usize {
    msgs.iter()
        .filter(|m| matches!(m, ServerWorldMsg::Hit { attacker, target, .. } if *attacker == mob && *target == player as u64))
        .count()
}

fn snapshot_names(msgs: &[ServerWorldMsg], who: u64, name: &str) -> bool {
    msgs.iter().any(|m| matches!(m, ServerWorldMsg::BuffSnapshot { target, buffs }
        if *target == who && buffs.iter().any(|(n, _)| n == name)))
}

/// Backstab: three times a swing, 20 stamina fanned at once, refused inside
/// its cooldown; not a Cleric's skill; refused as too tired at 10 stamina.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn backstab_is_tripled_charged_and_gated() {
    let h = start_both().await;
    let (mut a, a_id, dummy) = gm_beside_dummy(&h, "bstab", "Rogue", "Skill Dummy", 0, 0.0).await;
    a.send_use_skill("Backstab", Some(dummy));
    let msgs = a.collect_for(Duration::from_millis(1_500)).await;
    let mut answered = false;
    for m in &msgs {
        match m {
            ServerWorldMsg::Hit { attacker, target, amount, crit, .. } if *attacker == a_id as u64 && *target == dummy => {
                // A plain non-crit fist swing tops out at 6 (1-4 plus STR/5,
                // times the skill's 1.15), so 9 pins the multiplier; a crit
                // (1.5 to 2x) could reach it on its own and proves nothing.
                if !*crit {
                    assert!(*amount >= 9, "fists 1-4 plus STR/5, times three: {amount}");
                }
                answered = true;
            }
            ServerWorldMsg::Miss { attacker, target } if *attacker == a_id as u64 && *target == dummy => answered = true, // the 5% floor
            ServerWorldMsg::SkillFail { reason, .. } => panic!("a Rogue's Backstab beside the dummy was refused: {reason}"),
            _ => {}
        }
    }
    assert!(answered, "the swing was answered");
    let st = msgs.iter().find_map(|m| match m {
        ServerWorldMsg::StaminaUpdate { id, stamina, max } if *id == a_id as u64 => Some((*stamina, *max)),
        _ => None,
    });
    let (stamina, max) = st.expect("the stamina spend fans at once");
    assert!((max - stamina - 20.0).abs() < 0.5, "20 of {max} stamina: {stamina}");
    assert_eq!(skill_answer(&mut a, a_id, "Backstab", dummy).await, Err("You cannot use that yet.".into()));

    let (b_session, b_id, b_token) = provision_client(&h.auth_url, "notarogue", "Notaroguea", "Human", "Cleric").await;
    let mut b = WorldClient::start(b_token, &b_session, b_id).await;
    b.pump_for(Duration::from_millis(300)).await;
    assert_eq!(skill_answer(&mut b, b_id, "Backstab", dummy).await, Err("You cannot use that skill.".into()));

    let (c_session, c_id, _stale) = provision_client(&h.auth_url, "winded", "Windeda", "Human", "Rogue").await;
    set_char_stamina(&h.db_url, c_id, 10.0).await;
    let c_token = request_world_token(&h.auth_url, &c_session, c_id).await;
    let mut c = WorldClient::start(c_token, &c_session, c_id).await;
    c.pump_for(Duration::from_millis(300)).await;
    assert_eq!(skill_answer(&mut c, c_id, "Backstab", dummy).await, Err("You are too tired.".into()));
}

/// Shield Bash: the mob's frame says Stunned and it swings at nobody for the
/// 3 s, then swings again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shield_bash_holds_the_mob_for_three_seconds() {
    let h = start_both().await;
    let (mut a, a_id, dummy) = gm_beside_dummy(&h, "basher", "Warrior", "Bash Dummy", 1, 5.0).await;
    let first = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(6), |m| matches!(m, ServerWorldMsg::Hit { attacker, target, .. } if *attacker == dummy && *target == a_id as u64))
        .await;
    assert!(first.is_some(), "the dummy is swinging at the warrior");
    // One window of 2.6 s from the send, inside the 3 s stun: the bash lands,
    // the frame says Stunned, and nothing comes back from the mob after the
    // landing. A Miss (the 5% floor) waits out the cooldown and tries again.
    let mut landed_at: Option<usize> = None;
    let mut window: Vec<ServerWorldMsg> = Vec::new();
    for _ in 0..3 {
        a.send_use_skill("Shield Bash", Some(dummy));
        window = a.collect_for(Duration::from_millis(2_600)).await;
        if let Some(i) = window.iter().position(|m| matches!(m, ServerWorldMsg::Hit { attacker, target, .. } if *attacker == a_id as u64 && *target == dummy)) {
            landed_at = Some(i);
            break;
        }
        if let Some(ServerWorldMsg::SkillFail { reason, .. }) = window.iter().find(|m| matches!(m, ServerWorldMsg::SkillFail { .. })) {
            panic!("Shield Bash refused: {reason}");
        }
        a.pump_alive_for(Duration::from_millis(6_000)).await;
    }
    let landed_at = landed_at.expect("three Shield Bashes in a row cannot all miss at 5%");
    assert!(snapshot_names(&window[landed_at..], dummy, "Stunned"), "the mob's frame says Stunned");
    assert_eq!(mob_hits_on(&window[landed_at..], dummy, a_id), 0, "no swing while stunned");
    let after = a.collect_for(Duration::from_millis(4_000)).await;
    assert!(mob_hits_on(&after, dummy, a_id) >= 1, "it swings again once the stun runs out");
}

/// Feign Death: the roll runs on the server and its outcome is told, and the
/// 20 stamina is charged either way. A failure changes nothing (the mob keeps
/// swinging). A success drops the monk from the mob, which does not notice
/// them lying there until they move. The roll is 80%, so fresh monks try
/// until one succeeds: six in a row failing is one run in fifteen thousand,
/// and every attempt checks its own outcome.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn feign_death_is_rolled_told_and_held_until_a_move() {
    let h = start_both().await;
    let mut succeeded = false;
    // Letters only: character names take no digits.
    for user in ["feigna", "feignb", "feignc", "feignd", "feigne", "feignf"] {
        let (mut a, a_id, dummy) = gm_beside_dummy(&h, user, "Monk", "Fd Dummy", 1, 5.0).await;
        let first = a
            .wait_for(CHANNEL_SYSTEM, Duration::from_secs(6), |m| matches!(m, ServerWorldMsg::Hit { attacker, target, .. } if *attacker == dummy && *target == a_id as u64))
            .await;
        assert!(first.is_some(), "the dummy is swinging at the monk");
        a.send_use_skill("Feign Death", None);
        let msgs = a.collect_for(Duration::from_millis(1_500)).await;
        let line = msgs.iter().find_map(|m| match m {
            ServerWorldMsg::ChatMessage { text, .. } if text.contains("feign death") => Some(text.clone()),
            _ => None,
        });
        let line = line.expect("the outcome is told");
        assert!(msgs.iter().any(|m| matches!(m, ServerWorldMsg::StaminaUpdate { id, stamina, max } if *id == a_id as u64 && (max - stamina - 20.0).abs() < 0.5)),
            "the attempt costs 20 stamina either way");
        if line == "You feign death successfully." {
            assert!(msgs.iter().any(|m| matches!(m, ServerWorldMsg::EntityTarget { id, target: None } if *id == dummy)), "the mob drops the monk");
            assert!(snapshot_names(&msgs, a_id as u64, "Feigned Death"));
            let quiet = a.collect_for(Duration::from_millis(3_000)).await;
            assert_eq!(mob_hits_on(&quiet, dummy, a_id), 0, "lying still beside it, unnoticed");
            a.send_move(10_000, Vec3 { x: 1.0, y: 0.0, z: 0.0 });
            let noticed = a
                .wait_for(CHANNEL_SYSTEM, Duration::from_secs(2), |m| matches!(m, ServerWorldMsg::EntityTarget { id, target: Some(t) } if *id == dummy && *t == a_id as u64))
                .await;
            assert!(noticed.is_some(), "standing up, the monk is noticed again");
            succeeded = true;
        } else {
            assert_eq!(line, "Your feign death fails!");
            let after = a.collect_for(Duration::from_millis(4_000)).await;
            assert!(mob_hits_on(&after, dummy, a_id) >= 1, "a failed feign changes nothing");
        }
        a.send_disconnect();
        a.pump_for(Duration::from_millis(300)).await;
        if succeeded {
            break;
        }
    }
    assert!(succeeded, "six feigns in a row failed at 80%: the roll is wrong");
}

/// Hide: an idle mob spawned with the hidden rogue inside its aggro radius
/// does not notice them; the rogue's first swing ends the hide and the mob
/// turns. A second hide (after its cooldown) ends on the first step, and a
/// fresh idle mob notices the rogue then; a hide while hidden is refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hide_keeps_an_idle_mob_from_noticing_until_a_swing_or_a_step() {
    let h = start_both().await;
    let (session, a_id, _stale) = provision_client(&h.auth_url, "sneak", "Sneaka", "Human", "Rogue").await;
    let pool = db::open(&h.db_url).await.expect("open pool");
    db::set_account_gm(&pool, "sneak", true).await.expect("set is_gm");
    let token = request_world_token(&h.auth_url, &session, a_id).await;
    let mut a = WorldClient::start(token, &session, a_id).await;
    a.pump_for(Duration::from_millis(300)).await;

    a.send_use_skill("Hide", None);
    let told = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(2), |m| matches!(m, ServerWorldMsg::ChatMessage { text, .. } if text == "You have hidden yourself from view."))
        .await;
    assert!(told.is_some());
    // The cooldown (10 s) answers first for a second Hide straight after.
    assert_eq!(skill_answer(&mut a, a_id, "Hide", 0).await, Err("You cannot use that yet.".into()));
    // Aggro 5 m, spawned 3 m ahead of a hidden rogue: idle, unnoticing.
    let (dummy, _) = spawn_dummy(&mut a, "Watch Dummy", 0, 5.0).await;
    let quiet = a.collect_for(Duration::from_millis(1_500)).await;
    assert!(!quiet.iter().any(|m| matches!(m, ServerWorldMsg::EntityTarget { id, target: Some(_) } if *id == dummy)),
        "inside its radius, hidden: unnoticed");
    a.send_attack(dummy, "", false, DamageType::Physical);
    let msgs = a.collect_for(Duration::from_millis(2_000)).await;
    assert!(msgs.iter().any(|m| matches!(m, ServerWorldMsg::EntityTarget { id, target: Some(t) } if *id == dummy && *t == a_id as u64)),
        "the swing ends the hide and the mob turns");
    assert!(msgs.iter().any(|m| matches!(m, ServerWorldMsg::BuffSnapshot { target, buffs } if *target == a_id as u64 && !buffs.iter().any(|(n, _)| n == "Hide"))),
        "the snapshot without Hide fans");

    // Hide again once its 10 s cooldown has run. With the cooldown over and
    // the hide (30 s) still holding, a third Hide is refused as pointless;
    // then a step ends it.
    a.pump_alive_for(Duration::from_millis(7_000)).await;
    a.send_use_skill("Hide", None);
    let told = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(2), |m| matches!(m, ServerWorldMsg::ChatMessage { text, .. } if text == "You have hidden yourself from view."))
        .await;
    assert!(told.is_some(), "hidden again after the cooldown");
    a.pump_alive_for(Duration::from_millis(10_300)).await;
    assert_eq!(skill_answer(&mut a, a_id, "Hide", 0).await, Err("You are already hidden.".into()));
    let (watcher, _) = spawn_dummy(&mut a, "Second Watcher", 0, 5.0).await;
    let quiet = a.collect_for(Duration::from_millis(1_000)).await;
    assert!(!quiet.iter().any(|m| matches!(m, ServerWorldMsg::EntityTarget { id, target: Some(_) } if *id == watcher)),
        "still unnoticed while standing still");
    a.send_move(10_000, Vec3 { x: 1.0, y: 0.0, z: 0.0 });
    let noticed = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(2), |m| matches!(m, ServerWorldMsg::EntityTarget { id, target: Some(t) } if *id == watcher && *t == a_id as u64))
        .await;
    assert!(noticed.is_some(), "a step ends the hide and the watcher notices");
}

/// Evade: for its 5 s every swing at the rogue fans as an Evade and lands
/// nothing; afterwards the swings land again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn evade_deflects_every_swing_while_it_holds() {
    let h = start_both().await;
    let (mut a, a_id, dummy) = gm_beside_dummy(&h, "dodger", "Rogue", "Evade Dummy", 1, 5.0).await;
    let first = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(6), |m| matches!(m, ServerWorldMsg::Hit { attacker, target, .. } if *attacker == dummy && *target == a_id as u64))
        .await;
    assert!(first.is_some());
    a.send_use_skill("Evade", None);
    let window = a.collect_for(Duration::from_millis(4_500)).await;
    let evades = window.iter().filter(|m| matches!(m, ServerWorldMsg::Evade { attacker, target } if *attacker == dummy && *target == a_id as u64)).count();
    assert!(evades >= 1, "a 2.5 s swing inside 4.5 s was deflected");
    assert_eq!(mob_hits_on(&window, dummy, a_id), 0, "nothing lands while Evade holds");
    assert!(snapshot_names(&window, a_id as u64, "Evade"));
    a.pump_for(Duration::from_millis(1_000)).await;
    let after = a.collect_for(Duration::from_millis(4_000)).await;
    assert!(mob_hits_on(&after, dummy, a_id) >= 1, "once it runs out the swings land again");
}

/// Holy Shield: a pool of 60 takes the next swings (fanned as hits of 0) and
/// the paladin's HP does not move.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn holy_shield_absorbs_the_next_swings() {
    let h = start_both().await;
    let (mut a, a_id, dummy) = gm_beside_dummy(&h, "warded", "Paladin", "Ward Dummy", 1, 5.0).await;
    let first = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(6), |m| matches!(m, ServerWorldMsg::HealthUpdate { id, .. } if *id == a_id as u64))
        .await;
    let Some(ServerWorldMsg::HealthUpdate { hp: hp_before, .. }) = first else { panic!("the first swing lands") };
    a.send_use_skill("Holy Shield", None);
    let window = a.collect_for(Duration::from_millis(6_000)).await;
    assert!(snapshot_names(&window, a_id as u64, "Holy Shield"));
    let zero_hits = window.iter().filter(|m| matches!(m, ServerWorldMsg::Hit { attacker, target, amount, .. } if *attacker == dummy && *target == a_id as u64 && *amount == 0)).count();
    assert!(zero_hits >= 2, "two swings in 6 s, both absorbed: {zero_hits}");
    for m in &window {
        if let ServerWorldMsg::HealthUpdate { id, hp, .. } = m {
            if *id == a_id as u64 {
                assert!(*hp >= hp_before, "HP never drops behind the shield: {hp} < {hp_before}");
            }
        }
    }
}

/// Warder's Fury: the hit, then the warder is sent at the target.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn warders_fury_sends_the_warder_at_the_target() {
    let h = start_both().await;
    let (session, a_id, _stale) = provision_client(&h.auth_url, "furious", "Furiousa", "Human", "Beast Master").await;
    let pool = db::open(&h.db_url).await.expect("open pool");
    db::set_account_gm(&pool, "furious", true).await.expect("set is_gm");
    master_weapon_skill(&h.db_url, a_id, "hand_to_hand").await;
    let token = request_world_token(&h.auth_url, &session, a_id).await;
    let mut a = WorldClient::start(token, &session, a_id).await;
    let pet = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(4), |m| matches!(m, ServerWorldMsg::PetSpawn { owner, .. } if *owner == a_id as u64))
        .await;
    let Some(ServerWorldMsg::PetSpawn { id: warder, .. }) = pet else { panic!("the warder comes") };
    let (dummy, dummy_pos) = spawn_dummy(&mut a, "Fury Dummy", 0, 0.0).await;
    let mut seq: u32 = 1;
    walk_until_within(&mut a, &mut seq, a_id, Vec3 { x: 0.0, y: 0.0, z: 0.0 }, dummy_pos, 1.0).await;
    // Guard stance: a guarding warder never inherits the owner's target on
    // its own (the follow-stance assist), so a swing from it here is the
    // skill's command and nothing else.
    a.send_pet_command(protocol::world::pet_command::GUARD, None);
    a.pump_for(Duration::from_millis(500)).await;
    a.send_use_skill("Warder's Fury", Some(dummy));
    let msgs = a.collect_for(Duration::from_millis(8_000)).await;
    assert!(msgs.iter().any(|m| matches!(m, ServerWorldMsg::Hit { attacker, target, .. } | ServerWorldMsg::Miss { attacker, target } if *attacker == a_id as u64 && *target == dummy)),
        "the user's own blow");
    assert!(msgs.iter().any(|m| matches!(m, ServerWorldMsg::Hit { attacker, target, .. } if *attacker == warder && *target == dummy)),
        "and the guarding warder, sent at the target, strikes it");
}

// ── Bard songs on the server (the rework of 2026-10-20) ─────────────────────
// None of these pass on the previous server: it applied a song once per cast
// and never pulsed, and songs were instant.

/// A song takes its 3 s to sing (a forged instant release is refused), its
/// effect lands with the song's linger, the server pulses it every 6 s, and
/// after a StopSong the last pulse's effect lingers its 12 s and goes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_song_is_sung_pulsed_and_lingers_after_it_stops() {
    let h = start_both().await;
    let (session, a_id, _stale) = provision_client(&h.auth_url, "singer", "Singera", "Human", "Bard").await;
    set_char_level(&h.db_url, a_id, 10).await;
    let token = request_world_token(&h.auth_url, &session, a_id).await;
    let mut a = WorldClient::start(token, &session, a_id).await;
    a.pump_for(Duration::from_millis(300)).await;

    // An instant twist is a forgery: the release 0.3 s into a 3 s sing.
    a.send_cast_start("Selos' Melody", 3.0);
    a.pump_for(Duration::from_millis(300)).await;
    a.send_cast_spell("Selos' Melody", None);
    let early = a.collect_for(Duration::from_millis(800)).await;
    assert!(early.iter().any(|m| matches!(m, ServerWorldMsg::CastFail { caster, .. } if *caster == a_id as u64)), "released early: refused");
    assert!(!early.iter().any(|m| matches!(m, ServerWorldMsg::ChatMessage { text, .. } if text.starts_with("You begin singing"))));

    let begun = sing(&mut a, "Selos' Melody", None).await;
    assert!(begun.iter().any(|m| matches!(m, ServerWorldMsg::ChatMessage { text, .. } if text == "You begin singing Selos' Melody.")));
    let rem = song_remaining(&begun, a_id as u64, "Selos' Melody").expect("the first pulse's effect is on the bard");
    assert!(rem > 11.0 && rem <= 12.05, "lingering the song linger: {rem}");

    // The next pulse, 6 s on: a fresh snapshot with the effect refreshed.
    let _ = a.collect_for(Duration::from_millis(4_500)).await;
    let around_pulse = a.collect_for(Duration::from_millis(1_500)).await;
    let refreshed = song_remaining(&around_pulse, a_id as u64, "Selos' Melody").expect("the second pulse re-fans the snapshot");
    assert!(refreshed > 11.0, "refreshed by the pulse: {refreshed}");

    let stop_at = Instant::now();
    a.send_stop_song();
    let stopped = a.collect_for(Duration::from_millis(500)).await;
    assert!(stopped.iter().any(|m| matches!(m, ServerWorldMsg::ChatMessage { text, .. } if text == "You stop singing Selos' Melody.")));
    // No pulse after the stop (a snapshot refreshed to the full linger would
    // be one), and the last pulse's effect runs out when its 12 s linger
    // does: that pulse fell within the 1.5 s window before the stop (about
    // 2 s of real time, the loop's sleeps run a little long), so the removal
    // lands 10 to 12 s after it. Read off the clock, not the windows; a
    // pulse after the stop would show first as a refreshed snapshot.
    let mut gone_at: Option<Duration> = None;
    let deadline = stop_at + Duration::from_secs(16);
    while Instant::now() < deadline && gone_at.is_none() {
        for m in a.collect_for(Duration::from_millis(250)).await {
            if let ServerWorldMsg::BuffSnapshot { target, buffs } = &m {
                if *target == a_id as u64 {
                    match buffs.iter().find(|(n, _)| n == "Selos' Melody") {
                        Some((_, r)) => assert!(*r < 11.5, "no pulse after the stop: {r}"),
                        None => gone_at = Some(stop_at.elapsed()),
                    }
                }
            }
        }
    }
    let gone_at = gone_at.expect("the effect runs out within 16 s of the stop");
    assert!(
        gone_at >= Duration::from_secs_f32(9.0) && gone_at <= Duration::from_secs_f32(15.0),
        "the linger ran its 12 s from the last pulse: {gone_at:?}"
    );
}

/// Twisting: a second song replaces the first as the active song while the
/// first's effect lingers, so both show; singing the active song again stops
/// it; a nuke (any non-song cast) stops the song too.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn twisting_holds_two_songs_and_a_nuke_ends_the_song() {
    let h = start_both().await;
    let (session, a_id, _stale) = provision_client(&h.auth_url, "twister", "Twistera", "Human", "Bard").await;
    let pool = db::open(&h.db_url).await.expect("open pool");
    db::set_account_gm(&pool, "twister", true).await.expect("set is_gm");
    set_char_level(&h.db_url, a_id, 10).await;
    let token = request_world_token(&h.auth_url, &session, a_id).await;
    let mut a = WorldClient::start(token, &session, a_id).await;
    a.pump_for(Duration::from_millis(300)).await;
    let (dummy, _) = spawn_dummy(&mut a, "Song Dummy", 0, 0.0).await;

    let _ = sing(&mut a, "Selos' Melody", None).await;
    let second = sing(&mut a, "Wanderer's Chord", None).await;
    assert!(second.iter().any(|m| matches!(m, ServerWorldMsg::ChatMessage { text, .. } if text == "You begin singing Wanderer's Chord.")));
    assert!(second.iter().any(|m| matches!(m, ServerWorldMsg::BuffSnapshot { target, buffs } if *target == a_id as u64
        && buffs.iter().any(|(n, _)| n == "Selos' Melody") && buffs.iter().any(|(n, _)| n == "Wanderer's Chord"))),
        "both songs' effects are up");

    let again = sing(&mut a, "Wanderer's Chord", None).await;
    assert!(again.iter().any(|m| matches!(m, ServerWorldMsg::ChatMessage { text, .. } if text == "You stop singing Wanderer's Chord.")), "singing the active song stops it");

    let _ = sing(&mut a, "Selos' Melody", None).await;
    a.send_cast_spell("Dissonance", Some(dummy));
    let nuked = a.collect_for(Duration::from_millis(1_000)).await;
    assert!(nuked.iter().any(|m| matches!(m, ServerWorldMsg::ChatMessage { text, .. } if text == "You stop singing Selos' Melody.")), "a nuke ends the song");
    assert!(nuked.iter().any(|m| matches!(m, ServerWorldMsg::Hit { attacker, target, .. } if *attacker == a_id as u64 && *target == dummy)), "and lands");
}

/// A song is sung on the move (the movement gate passes songs), and a level 1
/// bard's Selos' is EQ's +20%: 0.31 of the listed +65%, 1.2015x run speed,
/// 0.300 m a tick, where the flat data would give 1.65x (0.4125 m) at any level.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_bard_sings_while_running_and_a_young_bard_runs_slower_than_the_data() {
    let h = start_both().await;
    let (session, a_id, a_token) = provision_client(&h.auth_url, "runner", "Runnera", "Human", "Bard").await;
    let mut a = WorldClient::start(a_token, &session, a_id).await;
    a.pump_for(Duration::from_millis(300)).await;
    let east = Vec3 { x: 1.0, y: 0.0, z: 0.0 };
    let mut seq: u32 = 1;

    a.send_cast_start("Selos' Melody", 3.0);
    for _ in 0..66 {
        a.send_move(seq, east);
        seq += 1;
        tick_one(&mut a.client, &mut a.transport);
        tokio::time::sleep(TICK_DT).await;
    }
    a.send_cast_spell("Selos' Melody", None);
    let begun = a.collect_for(Duration::from_millis(1_000)).await;
    assert!(begun.iter().any(|m| matches!(m, ServerWorldMsg::ChatMessage { text, .. } if text == "You begin singing Selos' Melody.")),
        "fifteen metres of running did not interrupt the sing");
    assert!(!begun.iter().any(|m| matches!(m, ServerWorldMsg::CastFail { caster, .. } if *caster == a_id as u64)));

    let step = median_step(&mut a, a_id, &mut seq, east, 30).await;
    assert!((step - 0.300).abs() < 0.012, "a level 1 Selos' runs 1.2x, 0.300 m a tick: {step}");
}

/// The EQ parity audit (2026-10-20): a cast can FIZZLE, rolled by the server
/// at the cast's start on the casting skill against the spell's level (the
/// P99 rule the casting skills follow now, in place of scaling the effect).
/// A level 16 Magician with no evocation at all starts Fireball Rk. III (a
/// level 16 spell, about one start in four fizzling) until one does: the
/// start is refused with EQ's line, a quarter of the spell's mana is gone
/// (42, so 10.5) and the gem is on its refresh (a start inside the global
/// cooldown is refused). Then the instant Frost Bolt Rk. III fizzles the same
/// way from its CastSpell, the mana topped off between tries. On the previous
/// server no cast ever fizzled.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cast_can_fizzle_on_a_low_casting_skill() {
    let h = start_both_opts(true).await;
    let (a_session, a_char_id, _stale) =
        provision_client(&h.auth_url, "fizzler", "Fizzlebeth", "Human", "Magician").await;
    set_char_level(&h.db_url, a_char_id, 16).await;
    set_casting_skill(&h.db_url, a_char_id, "evocation", 0).await;
    let pool = db::open(&h.db_url).await.expect("open pool");
    db::set_account_gm(&pool, "fizzler", true).await.expect("set is_gm");
    let a_token = request_world_token(&h.auth_url, &a_session, a_char_id).await;
    let mut a = WorldClient::start(a_token, &a_session, a_char_id).await;
    a.pump_for(Duration::from_millis(300)).await;
    let full_mp = char_data::compute("Human", "Magician", 16).max_mp;
    let me = a_char_id as u64;
    let is_fizzle = |m: &ServerWorldMsg| {
        matches!(m, ServerWorldMsg::CastFail { caster, reason } if *caster == me && reason == "Your spell fizzles!")
    };

    // Timed: a start that does not fizzle only caches a bar the next start
    // replaces, so nothing is spent until one does.
    let mut mp_after_fizzle: Option<f32> = None;
    for _ in 0..40 {
        a.send_cast_start("Fireball Rk. III", 1.5);
        let msgs = a.collect_for(Duration::from_millis(150)).await;
        if let Some(at) = msgs.iter().position(is_fizzle) {
            // The fizzle's own ManaUpdate follows its CastFail on the same
            // ordered channel; the regen fan's periodic update (full mana)
            // can land in the same window just before it, so read the one
            // after the fizzle line, never the first in the batch.
            mp_after_fizzle = msgs[at..].iter().find_map(|m| match m {
                ServerWorldMsg::ManaUpdate { id, mp, .. } if *id == me => Some(*mp),
                _ => None,
            });
            assert!(mp_after_fizzle.is_some(), "the fizzle comes with the true mana");
            break;
        }
    }
    let mp = mp_after_fizzle.expect("forty starts with no evocation at all and not one fizzle");
    assert!((full_mp - mp - 10.5).abs() < 0.01, "a quarter of 42 burned: {full_mp} -> {mp}");
    a.send_cast_start("Fireball Rk. III", 1.5);
    let again = a.collect_for(Duration::from_millis(200)).await;
    assert!(
        again.iter().any(|m| matches!(m, ServerWorldMsg::CastFail { caster, reason } if *caster == me && reason == "You cannot cast again yet.")),
        "the fizzle started the gem's refresh"
    );

    // Instant: Frost Bolt Rk. III at a dummy, rolled at its CastSpell. Every
    // try, landed or fizzled, starts the global cooldown, so each waits it out.
    a.send_dev_spawn("Fizzle Dummy", 1, 100_000.0, 0, 0.0, 0.0);
    let spawn = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::EnemySpawn { mob_name, .. } if mob_name == "Fizzle Dummy")
        })
        .await
        .expect("the dummy spawns");
    let dummy = match spawn {
        ServerWorldMsg::EnemySpawn { id, .. } => id,
        _ => unreachable!(),
    };
    a.pump_alive_for(Duration::from_millis(2_200)).await;
    let mut fizzled = false;
    for _ in 0..40 {
        a.send_heal_self();
        a.send_cast_spell("Frost Bolt Rk. III", Some(dummy));
        let msgs = a.collect_for(Duration::from_millis(2_300)).await;
        assert!(
            !msgs.iter().any(|m| matches!(m, ServerWorldMsg::CastFail { caster, reason } if *caster == me && reason == "Not enough mana.")),
            "the mana was topped off before each try"
        );
        if msgs.iter().any(is_fizzle) {
            fizzled = true;
            break;
        }
    }
    assert!(fizzled, "forty instant casts with no evocation at all and not one fizzle");
}

// ─── Factions and alignment (2026-10-23) ─────────────────────────────────

// A dev spawn lands 3 m from its requester, and the enter-world seed carries
// the camps' mobs of the same names (a Grey Wolf camp sits 38 m from the
// spawn), so every spawn below is matched beside the caster, not by name alone.

/// The faction standings a batch of messages carries, in order.
fn faction_updates(msgs: &[ServerWorldMsg]) -> Vec<(String, f32, u8)> {
    msgs.iter()
        .filter_map(|m| match m {
            ServerWorldMsg::FactionUpdate { faction_id, standing, level, .. } => {
                Some((faction_id.clone(), *standing, *level))
            }
            _ => None,
        })
        .collect()
}

/// The last standing a batch carries for one faction.
fn last_standing(msgs: &[ServerWorldMsg], faction_id: &str) -> Option<(f32, u8)> {
    faction_updates(msgs)
        .into_iter()
        .filter(|(id, _, _)| id == faction_id)
        .map(|(_, s, l)| (s, l))
        .last()
}

fn system_lines(msgs: &[ServerWorldMsg]) -> Vec<String> {
    msgs.iter()
        .filter_map(|m| match m {
            ServerWorldMsg::ChatMessage { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

fn alignment_scores(msgs: &[ServerWorldMsg]) -> Vec<f32> {
    msgs.iter()
        .filter_map(|m| match m {
            ServerWorldMsg::AlignmentUpdate { score } => Some(*score),
            _ => None,
        })
        .collect()
}

/// Smite's own cooldown (longer than the global one), with a margin.
async fn a_breath(c: &mut WorldClient) {
    c.pump_alive_for(Duration::from_millis(4200)).await;
}

/// The same wait with two clients in the world: each is pumped with
/// heartbeats for half of it, so neither sits silent past the server's 10 s
/// idle timeout across a run of casts.
async fn a_breath_for_two(a: &mut WorldClient, b: &mut WorldClient) {
    a.pump_alive_for(Duration::from_millis(2100)).await;
    b.pump_alive_for(Duration::from_millis(2100)).await;
}

const APPREHENSIVE: u8 = 3;
const INDIFFERENT: u8 = 4;
const KINDLY: u8 = 6;
const DUBIOUS: u8 = 2;

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
    db::set_account_gm(&pool, "factgm", true).await.expect("set is_gm");
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
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::EnemySpawn { id, .. } if *id == mob)
        })
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
    assert!(hit.is_some(), "the bystander's Smite lands, which puts them on the hate list");
    let mut died = None;
    for _ in 0..4 {
        gm.send_cast_spell("Smite", Some(mob));
        died = gm
            .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
                matches!(m, ServerWorldMsg::EntityDied { id } if *id == mob)
            })
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
        assert_eq!(last_standing(msgs, "greyveil_town"), Some((3.5, INDIFFERENT)), "{who}: half to the town");
        assert_eq!(last_standing(msgs, "greyveil_watch"), Some((3.5, INDIFFERENT)), "{who}: half to the Watch");
        let lines = system_lines(msgs);
        for want in [
            "Your faction standing with The Gnoll Raiders has gotten worse.",
            "Your faction standing with The Folk of Greyveil has gotten better.",
            "Your faction standing with The Greyveil Watch has gotten better.",
        ] {
            assert!(lines.iter().any(|l| l == want), "{who}: missing {want:?} in {lines:?}");
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
    db::set_account_gm(&pool, "questfac", true).await.expect("set is_gm");
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
            .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
                matches!(m, ServerWorldMsg::EntityDied { id } if *id == mob)
            })
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
        faction_updates(&after_kills).iter().all(|(id, _, _)| id == "wildlife"),
        "wildlife has no enemies, so nobody else moves: {:?}",
        faction_updates(&after_kills)
    );

    a.send_complete_quest("wolf_threat");
    let msgs = a.collect_for(Duration::from_secs(2)).await;
    assert!(
        msgs.iter().any(|m| matches!(m, ServerWorldMsg::QuestCompleted { quest_id } if quest_id == "wolf_threat")),
        "the turn-in lands: {msgs:?}"
    );
    assert_eq!(last_standing(&msgs, "greyveil_watch"), Some((25.0, INDIFFERENT)), "{:?}", faction_updates(&msgs));
    assert_eq!(last_standing(&msgs, "greyveil_town"), Some((10.0, INDIFFERENT)));
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
        Some(ServerWorldMsg::QuestRejected { reason, rollback, .. }) => {
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
    assert!(matches!(set, Some(ServerWorldMsg::FactionUpdate { level, .. }) if level == DUBIOUS), "{set:?}");
    a.send_buy_item(0, POTION_NAME, 1);
    let msgs = a.collect_for(Duration::from_millis(1500)).await;
    assert!(
        !msgs.iter().any(|m| matches!(m, ServerWorldMsg::InventoryDelta { .. })),
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
    db::set_account_gm(&pool, "congm", true).await.expect("set is_gm");
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
    assert!(faction_updates(&msgs).is_empty(), "a plain account's slider moves nothing: {msgs:?}");
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
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(2), |m| {
            matches!(m, ServerWorldMsg::ChatMessage { text, .. } if text.starts_with("Brom"))
        })
        .await;
    assert!(
        matches!(&line, Some(ServerWorldMsg::ChatMessage { text, .. }) if text == "Brom regards you indifferently."),
        "{line:?}"
    );
    for (what, npc, target) in [("an unknown id", Some("nobody"), None), ("a player id", None, Some(gm_char as u64))] {
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
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::EnemySpawn { id, .. } if *id == mob)
        })
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
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(2), |m| {
            matches!(m, ServerWorldMsg::ChatMessage { text, .. } if text.starts_with("Brom"))
        })
        .await;
    assert!(
        matches!(&line, Some(ServerWorldMsg::ChatMessage { text, .. }) if text == "Brom kindly considers you."),
        "{line:?}"
    );
}

/// The prestige gate is the server's: a Paladin driven to Evil casts as a
/// Paladin_Fallen. Seeded at -1600 (the server has nowhere else to set it but
/// the row), told so on entering, Death's Embrace (the Fallen kit) lands for
/// its 80 and Righteous Fire (the Paladin's) is refused as not the class's.
/// Both kits sit on the bar; the server decides. Fails on the previous
/// server, which gated on the plain class and knew no Fallen spells.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_paladin_at_evil_casts_the_fallen_kit_and_not_the_paladins() {
    let h = start_both().await;
    let (session, char_id, _stale) =
        provision_client(&h.auth_url, "fallen", "Fallen", "Human", "Paladin").await;
    let pool = db::open(&h.db_url).await.expect("open pool");
    db::set_account_gm(&pool, "fallen", true).await.expect("set is_gm");
    set_char_level(&h.db_url, char_id, 20).await;
    sqlx::query("UPDATE characters SET alignment_score = -1600.0, alignment_seeded = 1 WHERE id = ?1")
        .bind(char_id)
        .execute(&pool)
        .await
        .expect("seed the fall");
    let gems: Vec<(i32, String)> = world::class_spell_names("Paladin")
        .into_iter()
        .chain(world::class_spell_names("Paladin_Fallen"))
        .enumerate()
        .map(|(i, n)| (i as i32, n))
        .collect();
    db::save_spell_gems(&pool, char_id, &gems, true).await.expect("both kits on the bar");
    let token = request_world_token(&h.auth_url, &session, char_id).await;
    let mut a = WorldClient::start(token, &session, char_id).await;
    let seed = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::AlignmentUpdate { .. })
        })
        .await;
    assert!(
        matches!(seed, Some(ServerWorldMsg::AlignmentUpdate { score }) if (score + 1600.0).abs() < 1e-3),
        "told the stored score on entering: {seed:?}"
    );

    a.send_dev_spawn("Practice Dummy", 1, 500.0, 0, 0.0, 0.0);
    let mob = match a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::EnemySpawn { mob_name, pos, .. } if mob_name == "Practice Dummy" && pos.x.abs() < 6.0 && pos.z.abs() < 6.0)
        })
        .await
    {
        Some(ServerWorldMsg::EnemySpawn { id, .. }) => id,
        other => panic!("the dummy spawns: {other:?}"),
    };
    a.send_cast_spell("Death's Embrace", Some(mob));
    let hit = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::Hit { target, .. } if *target == mob)
        })
        .await;
    match hit {
        Some(ServerWorldMsg::Hit { amount, .. }) => assert!(amount >= 80, "the Fallen nuke's 80: {amount}"),
        other => panic!("a Fallen Paladin casts Death's Embrace: {other:?}"),
    }
    a.pump_alive_for(Duration::from_millis(2200)).await; // the global cooldown
    a.send_cast_spell("Righteous Fire", Some(mob));
    let fail = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::CastFail { caster, .. } if *caster == char_id as u64)
        })
        .await;
    match fail {
        Some(ServerWorldMsg::CastFail { reason, .. }) => {
            assert_eq!(reason, "Your class cannot cast that.", "the Paladin's own kit is lost to the Fallen")
        }
        other => panic!("Righteous Fire is refused: {other:?}"),
    }
}

/// The off-side penalty is the server's: a Paladin at Neutral casts at 0.7
/// (the client's old rule, kept by the user). Righteous Fire lists 7; a
/// non-crit hit lands for 4 (7 x 0.7 truncated). A crit is retried after the
/// cooldown. Fails on the previous server at 7.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_neutral_paladin_casts_at_seventy_percent() {
    let h = start_both().await;
    let (session, char_id, _stale) =
        provision_client(&h.auth_url, "lukewarm", "Lukewarm", "Human", "Paladin").await;
    let pool = db::open(&h.db_url).await.expect("open pool");
    db::set_account_gm(&pool, "lukewarm", true).await.expect("set is_gm");
    set_char_level(&h.db_url, char_id, 20).await;
    sqlx::query("UPDATE characters SET alignment_score = 0.0, alignment_seeded = 1 WHERE id = ?1")
        .bind(char_id)
        .execute(&pool)
        .await
        .expect("seed neutral");
    let token = request_world_token(&h.auth_url, &session, char_id).await;
    let mut a = WorldClient::start(token, &session, char_id).await;
    a.send_dev_spawn("Practice Dummy", 1, 500.0, 0, 0.0, 0.0);
    let mob = match a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::EnemySpawn { mob_name, pos, .. } if mob_name == "Practice Dummy" && pos.x.abs() < 6.0 && pos.z.abs() < 6.0)
        })
        .await
    {
        Some(ServerWorldMsg::EnemySpawn { id, .. }) => id,
        other => panic!("the dummy spawns: {other:?}"),
    };
    let mut landed: Option<i32> = None;
    for _ in 0..6 {
        a.send_cast_spell("Righteous Fire", Some(mob));
        let hit = a
            .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
                matches!(m, ServerWorldMsg::Hit { target, .. } if *target == mob)
            })
            .await;
        match hit {
            Some(ServerWorldMsg::Hit { amount, crit: false, .. }) => {
                landed = Some(amount);
                break;
            }
            Some(ServerWorldMsg::Hit { crit: true, .. }) => {
                // Righteous Fire has a 4 s cooldown of its own.
                a.pump_alive_for(Duration::from_millis(4200)).await;
            }
            other => panic!("a Neutral Paladin still casts the Paladin's kit: {other:?}"),
        }
    }
    assert_eq!(landed, Some(4), "7 listed, x 0.7 at Neutral, truncated");
}

// ---- Hotfix 2026-10-23: the crash and the dupes -------------------------
//
// Each of these fails on the code before the fix.

async fn count_item_rows(db_url: &str, sql: &str, id: i64, path: &str) -> i64 {
    let pool = projectdawn_server::db::open(db_url).await.expect("open pool");
    let (n,): (i64,) = sqlx::query_as(sql)
        .bind(id)
        .bind(path)
        .fetch_one(&pool)
        .await
        .expect("count rows");
    n
}

/// A chat line with a two-byte character straddling byte 512 used to panic
/// the slice `text[..512]` inside the tick task, which took the whole server
/// down. The server cuts chat to `MAX_CHAT_CHARS` (250) characters now
/// (2026-10-26, the client stops at the same cap): the listener gets exactly
/// 250, and the server keeps serving, so the speaker's next line still lands.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_chat_line_cut_inside_a_multibyte_char_does_not_crash_the_server() {
    let h = start_both().await;
    let (a_session, a_char, a_token) =
        provision_client(&h.auth_url, "chatcrasha", "Speaker", "Human", "Warrior").await;
    let (b_session, b_char, b_token) =
        provision_client(&h.auth_url, "chatcrashb", "Listener", "Human", "Warrior").await;
    let mut a = WorldClient::start(a_token, &a_session, a_char).await;
    let mut b = WorldClient::start(b_token, &b_session, b_char).await;

    let poison = format!("{}é and more", "a".repeat(511));
    assert!(!poison.is_char_boundary(512), "the test line must straddle byte 512");
    a.send_say(&poison);
    a.pump_for(Duration::from_millis(300)).await;
    a.send_say("still here");
    a.pump_for(Duration::from_millis(100)).await;

    let cut = b
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::ChatMessage { text, .. } if text.starts_with("aaa"))
        })
        .await;
    let Some(ServerWorldMsg::ChatMessage { text, .. }) = cut else { panic!("the long line reaches the listener") };
    assert_eq!(text, "a".repeat(250), "cut to 250 characters, as the client stops typing");

    let probe = b
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::ChatMessage { text, .. } if text == "still here")
        })
        .await;
    assert!(probe.is_some(), "the server stopped serving after the long chat line");
}

/// A drop is durable at once. The bag lives only in memory and its picker is
/// saved at their own logout, so a dropper whose removal waited for the 60 s
/// checkpoint could get the item back from a restart after someone else had
/// saved it. The dropper's row must be gone from the DB the moment the bag
/// exists, with no checkpoint and no logout in between.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dropped_item_leaves_the_droppers_saved_inventory_at_once() {
    let h = start_both().await;
    let (session, char_id, token) =
        provision_client(&h.auth_url, "dropdurable", "Dropper", "Human", "Warrior").await;
    give_char_item(&h.db_url, char_id, "base", 0, BREAD, 3).await;
    let mut a = WorldClient::start(token, &session, char_id).await;
    a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::InventorySnapshot { .. })
    })
    .await
    .expect("snapshot");

    a.send_drop_item("base", 0, 0);
    a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::LootBagSpawn { items, .. }
            if items.iter().any(|(p, c)| p == BREAD && *c == 3))
    })
    .await
    .expect("the bag spawns with the dropped stack");

    let saved = count_item_rows(
        &h.db_url,
        "SELECT COUNT(*) FROM character_items WHERE char_id = ?1 AND item_path = ?2",
        char_id,
        BREAD,
    )
    .await;
    assert_eq!(saved, 0, "the dropped stack is still in the dropper's saved inventory");
}

/// The other half of a drop: whoever picks the stack up is saved in the same
/// tick. The dropper's row is already gone, so a picker left unsaved until the
/// checkpoint would lose the item to a crash.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_picked_up_drop_is_saved_on_the_picker_at_once() {
    let h = start_both().await;
    let (a_session, a_char, a_token) =
        provision_client(&h.auth_url, "giftdrop", "Giver", "Human", "Warrior").await;
    let (b_session, b_char, b_token) =
        provision_client(&h.auth_url, "giftpick", "Taker", "Human", "Warrior").await;
    give_char_item(&h.db_url, a_char, "base", 0, BREAD, 2).await;
    let mut a = WorldClient::start(a_token, &a_session, a_char).await;
    let mut b = WorldClient::start(b_token, &b_session, b_char).await;

    a.send_drop_item("base", 0, 0);
    a.pump_for(Duration::from_millis(100)).await;
    let bag_id = match b
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::LootBagSpawn { items, .. }
                if items.iter().any(|(p, _)| p == BREAD))
        })
        .await
        .expect("the bag reaches the peer")
    {
        ServerWorldMsg::LootBagSpawn { bag_id, .. } => bag_id,
        _ => unreachable!(),
    };
    b.send_loot_all(bag_id);
    b.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::InventoryDelta { item_path: Some(p), .. } if p == BREAD)
    })
    .await
    .expect("the loaves reach the picker");

    let picker_saved = count_item_rows(
        &h.db_url,
        "SELECT COUNT(*) FROM character_items WHERE char_id = ?1 AND item_path = ?2",
        b_char,
        BREAD,
    )
    .await;
    assert_eq!(picker_saved, 1, "the picked-up stack is not in the picker's saved inventory");
}

/// A burst of drops in one tick is capped (MAX_DROPS_PER_TICK, 4): each
/// dropper is saved once per tick, and the cap bounds what a forged burst can
/// make that save and the bag fan carry. Six drops in one datagram: four
/// leave the saved inventory, two stay.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_burst_of_drops_in_one_tick_is_capped() {
    let h = start_both().await;
    let (session, char_id, token) =
        provision_client(&h.auth_url, "dropburst", "Flooder", "Human", "Warrior").await;
    for slot in 0..6 {
        give_char_item(&h.db_url, char_id, "base", slot, BREAD, 1).await;
    }
    let mut a = WorldClient::start(token, &session, char_id).await;
    a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::InventorySnapshot { .. })
    })
    .await
    .expect("snapshot");

    for slot in 0..6 {
        a.send_drop_item("base", slot, 0);
    }
    a.pump_for(Duration::from_millis(800)).await;

    let left = count_item_rows(
        &h.db_url,
        "SELECT COUNT(*) FROM character_items WHERE char_id = ?1 AND item_path = ?2",
        char_id,
        BREAD,
    )
    .await;
    assert_eq!(left, 2, "four drops apply per tick and are saved; the rest of the burst is ignored");
}

/// An immediate save writes every dirty store, never one alone. Withdraw a
/// loaf from the bank (vault and inventory both dirty, nothing saved yet),
/// then die before any checkpoint: the corpse transaction writes the
/// inventory onto the corpse, and the vault must be written with it, or the
/// loaf sits on the corpse AND in the vault row after a crash.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_death_after_an_unsaved_bank_withdrawal_leaves_the_item_in_one_place() {
    let h = start_both().await;
    let (session, char_id, _stale) =
        provision_client(&h.auth_url, "bankdeath", "Withdrawer", "Human", "Warrior").await;
    {
        let pool = projectdawn_server::db::open(&h.db_url).await.expect("open pool");
        sqlx::query("INSERT INTO bank_items (char_id, slot, item_path, count) VALUES (?1, 0, ?2, 1)")
            .bind(char_id)
            .bind(BREAD)
            .execute(&pool)
            .await
            .expect("seed the vault");
    }
    let token = gm_world_token(&h.auth_url, &h.db_url, "bankdeath", &session, char_id).await;
    let mut a = WorldClient::start(token, &session, char_id).await;
    a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::InventorySnapshot { .. })
    })
    .await
    .expect("snapshot");

    // The starter spawn is within reach of the banker (npcs.rs pins it).
    a.send_bank_withdraw_item(0);
    a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::InventoryDelta { item_path: Some(p), .. } if p == BREAD)
    })
    .await
    .expect("the loaf arrives in the bags");

    a.send_death();
    a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::InventorySnapshot { entries } if entries.is_empty())
    })
    .await
    .expect("the death strip empties the bags");

    let in_vault = count_item_rows(
        &h.db_url,
        "SELECT COUNT(*) FROM bank_items WHERE char_id = ?1 AND item_path = ?2",
        char_id,
        BREAD,
    )
    .await;
    let on_corpse = count_item_rows(
        &h.db_url,
        "SELECT COUNT(*) FROM corpse_items ci JOIN corpses c ON c.corpse_id = ci.corpse_id
         WHERE c.char_id = ?1 AND ci.item_path = ?2",
        char_id,
        BREAD,
    )
    .await;
    assert_eq!(on_corpse, 1, "the loaf went onto the corpse");
    assert_eq!(in_vault, 0, "the loaf is ALSO still in the saved vault: a dupe");
}

/// A store that fails to load refuses the login. Playing on with an empty
/// inventory let the next save replace the real rows with nothing. A row the
/// decoder rejects (a count stored as text) stands in for a transient read
/// error; the login must be refused and the row left exactly as it was.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_store_that_fails_to_load_refuses_the_login() {
    let h = start_both().await;
    let (session, char_id, token) =
        provision_client(&h.auth_url, "badload", "Unlucky", "Human", "Warrior").await;
    {
        let pool = projectdawn_server::db::open(&h.db_url).await.expect("open pool");
        sqlx::query(
            "INSERT INTO character_items (char_id, location, slot, item_path, count)
             VALUES (?1, 'base', 0, ?2, 'not a number')",
        )
        .bind(char_id)
        .bind(BREAD)
        .execute(&pool)
        .await
        .expect("seed an undecodable row");
    }

    let answer = WorldClient::start_refused(token, &session, char_id).await;
    assert!(
        matches!(answer, ServerWorldMsg::Kick { .. }),
        "the login was accepted with an empty inventory: {answer:?}"
    );
    let rows = count_item_rows(
        &h.db_url,
        "SELECT COUNT(*) FROM character_items WHERE char_id = ?1 AND item_path = ?2",
        char_id,
        BREAD,
    )
    .await;
    assert_eq!(rows, 1, "the stored row must be untouched");
}

/// Only what a merchant in reach stocks is for sale. A priced item no vendor
/// stocks (a named-mob drop with a sell-back price) used to be buyable from
/// any vendor by name; the town's own stock still sells.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_priced_item_no_merchant_stocks_cannot_be_bought() {
    let h = start_both().await;
    let (session, char_id, token) =
        provision_client(&h.auth_url, "stockgate", "Shopper", "Human", "Warrior").await;
    {
        let pool = projectdawn_server::db::open(&h.db_url).await.expect("open pool");
        sqlx::query("UPDATE characters SET platinum = 10 WHERE id = ?1")
            .bind(char_id)
            .execute(&pool)
            .await
            .expect("fund the shopper");
    }
    let mut a = WorldClient::start(token, &session, char_id).await;
    a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::InventorySnapshot { .. })
    })
    .await
    .expect("snapshot");

    a.send_buy_item(0, "Predator's Collar", 1);
    let refusal = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::ChatMessage { text, .. } if text.contains("doesn't stock"))
        })
        .await;
    assert!(refusal.is_some(), "an unstocked named drop was sold");

    a.send_buy_item(0, "Bread Loaf", 1);
    let bought = a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::InventoryDelta { item_path: Some(p), .. } if p == BREAD)
        })
        .await;
    assert!(bought.is_some(), "a stocked loaf should still sell");
}

/// The trade commit writes both inventories and wallets but not the vaults,
/// so each trader's other dirty stores are flushed first. A withdraws a loaf
/// from the bank (unsaved) and trades it to B: once the trade commits, the
/// loaf is in B's saved inventory and nowhere in A's saved vault.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_traded_bank_withdrawal_leaves_the_givers_saved_vault() {
    let h = start_both().await;
    let (a_session, a_char, a_token) =
        provision_client(&h.auth_url, "vaulttradea", "Vaulter", "Human", "Warrior").await;
    let (b_session, b_char, b_token) =
        provision_client(&h.auth_url, "vaulttradeb", "Receiver", "Elf", "Cleric").await;
    {
        let pool = projectdawn_server::db::open(&h.db_url).await.expect("open pool");
        sqlx::query("INSERT INTO bank_items (char_id, slot, item_path, count) VALUES (?1, 0, ?2, 1)")
            .bind(a_char)
            .bind(BREAD)
            .execute(&pool)
            .await
            .expect("seed the vault");
    }
    let mut a = WorldClient::start(a_token, &a_session, a_char).await;
    let mut b = WorldClient::start(b_token, &b_session, b_char).await;

    a.send_bank_withdraw_item(0);
    let (loc, slot) = match a
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::InventoryDelta { item_path: Some(p), .. } if p == BREAD)
        })
        .await
        .expect("the loaf arrives in the bags")
    {
        ServerWorldMsg::InventoryDelta { location, slot, .. } => (location, slot),
        _ => unreachable!(),
    };

    a.send_trade_request(b_char as u64);
    a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::TradeOpened { .. })
    })
    .await
    .expect("the trade opens");
    a.send_trade_offer_item(0, &loc, slot);
    a.pump_for(Duration::from_millis(200)).await;
    b.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::TradeOfferUpdate { mine: false, slots, .. } if !slots.is_empty())
    })
    .await
    .expect("B sees the offer");
    a.send_trade_accept();
    a.pump_for(Duration::from_millis(200)).await;
    b.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::TradeAcceptState { them: true, .. })
    })
    .await
    .expect("B sees A accept");
    b.send_trade_accept();
    b.wait_for(CHANNEL_SYSTEM, Duration::from_secs(5), |m| {
        matches!(m, ServerWorldMsg::TradeClosed { committed: true, .. })
    })
    .await
    .expect("the trade commits");

    let b_has = count_item_rows(
        &h.db_url,
        "SELECT COUNT(*) FROM character_items WHERE char_id = ?1 AND item_path = ?2",
        b_char,
        BREAD,
    )
    .await;
    let a_vault = count_item_rows(
        &h.db_url,
        "SELECT COUNT(*) FROM bank_items WHERE char_id = ?1 AND item_path = ?2",
        a_char,
        BREAD,
    )
    .await;
    assert_eq!(b_has, 1, "the loaf is in the receiver's saved inventory");
    assert_eq!(a_vault, 0, "the loaf is ALSO still in the giver's saved vault: a dupe");
}

// ---- Housekeeping Phase 1: durability -----------------------------------

async fn fund_char(db_url: &str, char_id: i64) {
    let pool = projectdawn_server::db::open(db_url).await.expect("open pool");
    sqlx::query("UPDATE characters SET platinum = 10 WHERE id = ?1")
        .bind(char_id)
        .execute(&pool)
        .await
        .expect("fund");
}

async fn char_xp_level(db_url: &str, char_id: i64) -> (i64, i64) {
    let pool = projectdawn_server::db::open(db_url).await.expect("open pool");
    sqlx::query_as("SELECT xp, level FROM characters WHERE id = ?1")
        .bind(char_id)
        .fetch_one(&pool)
        .await
        .expect("xp row")
}

/// Pump both clients until each has received a Kick, returning their codes.
/// Reads the channel directly rather than through `wait_for`, which gives up
/// on a disconnect: the server closes every connection right after the kick.
async fn kicks_for_both(
    a: &mut WorldClient,
    b: &mut WorldClient,
) -> (Option<protocol::world::KickCode>, Option<protocol::world::KickCode>) {
    let (mut ka, mut kb) = (None, None);
    let deadline = Instant::now() + Duration::from_secs(5);
    while (ka.is_none() || kb.is_none()) && Instant::now() < deadline {
        for (c, slot) in [(&mut *a, &mut ka), (&mut *b, &mut kb)] {
            tick_one(&mut c.client, &mut c.transport);
            while let Some(bytes) = c.client.receive_message(CHANNEL_SYSTEM) {
                if let Ok((ServerWorldMsg::Kick { code, .. }, _)) =
                    bincode::serde::decode_from_slice::<ServerWorldMsg, _>(&bytes, bincode_cfg())
                {
                    *slot = Some(code);
                }
            }
        }
        tokio::time::sleep(TICK_DT).await;
    }
    (ka, kb)
}

/// A stop signal (systemd's SIGTERM on `restart`) saves everyone and tells
/// them, instead of throwing away up to 60 s of progress. A walks and buys a
/// loaf (both only marked dirty, never saved before the 60 s checkpoint), the
/// world is stopped, and the walk and the loaf are both in the database; both
/// players were told the world is restarting; the world task ends cleanly.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stop_signal_saves_everyone_and_tells_them() {
    let mut h = start_both().await;
    let (a_session, a_char, a_token) =
        provision_client(&h.auth_url, "stopa", "Walker", "Human", "Warrior").await;
    let (b_session, b_char, b_token) =
        provision_client(&h.auth_url, "stopb", "Stander", "Human", "Warrior").await;
    fund_char(&h.db_url, a_char).await;
    let mut a = WorldClient::start(a_token, &a_session, a_char).await;
    let mut b = WorldClient::start(b_token, &b_session, b_char).await;

    a.send_buy_item(0, "Bread Loaf", 1);
    a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::InventoryDelta { item_path: Some(p), .. } if p == BREAD)
    })
    .await
    .expect("the loaf is bought");
    let mut seq = 0;
    walk_east(&mut a, &mut seq, 6.0).await;

    h.shutdown.send(true).expect("send the stop signal");
    let (ka, kb) = kicks_for_both(&mut a, &mut b).await;
    assert_eq!(ka, Some(protocol::world::KickCode::Restart), "the walker was told the world is restarting");
    assert_eq!(kb, Some(protocol::world::KickCode::Restart), "the bystander was told too");
    let world = h.world.take().expect("world handle");
    let ended = tokio::time::timeout(Duration::from_secs(5), world)
        .await
        .expect("the world task ends after the stop signal")
        .expect("the world task did not panic");
    assert!(ended.is_ok(), "a stop signal is a clean end: {ended:?}");

    let pool = projectdawn_server::db::open(&h.db_url).await.expect("open pool");
    let (pos_x,): (Option<f32>,) = sqlx::query_as("SELECT pos_x FROM characters WHERE id = ?1")
        .bind(a_char)
        .fetch_one(&pool)
        .await
        .expect("position row");
    assert!(pos_x.unwrap_or(0.0) > 3.0, "the walk was not saved: pos_x = {pos_x:?}");
    let loaves = count_item_rows(
        &h.db_url,
        "SELECT COUNT(*) FROM character_items WHERE char_id = ?1 AND item_path = ?2",
        a_char,
        BREAD,
    )
    .await;
    assert_eq!(loaves, 1, "the bought loaf was not saved");
}

/// A logout whose save fails is not thrown away: it is retried, and the
/// account's relogin is refused until it lands (letting it in would load the
/// older saved state). A test-only trigger makes every inventory write fail;
/// the player buys a loaf and logs out; the relogin is refused; the trigger
/// is dropped; the retry lands and the relogin succeeds with the loaf saved.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_logout_save_holds_the_relogin_until_it_lands() {
    let h = start_both().await;
    let (session, char_id, token) =
        provision_client(&h.auth_url, "savefail", "Unsaved", "Human", "Warrior").await;
    fund_char(&h.db_url, char_id).await;
    let mut a = WorldClient::start(token, &session, char_id).await;
    a.send_buy_item(0, "Bread Loaf", 1);
    a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::InventoryDelta { item_path: Some(p), .. } if p == BREAD)
    })
    .await
    .expect("the loaf is bought");

    let pool = projectdawn_server::db::open(&h.db_url).await.expect("open pool");
    sqlx::query(
        "CREATE TRIGGER fail_inventory_writes BEFORE INSERT ON character_items
         BEGIN SELECT RAISE(ABORT, 'test: inventory writes fail'); END",
    )
    .execute(&pool)
    .await
    .expect("install the failing trigger");

    a.send_disconnect();
    a.pump_for(Duration::from_millis(500)).await;
    drop(a);
    tokio::time::sleep(Duration::from_millis(500)).await;

    let token = request_world_token(&h.auth_url, &session, char_id).await;
    let answer = WorldClient::start_refused(token, &session, char_id).await;
    assert!(
        matches!(&answer, ServerWorldMsg::Kick { reason, .. } if reason.contains("still being saved")),
        "the relogin must wait for the pending save: {answer:?}"
    );

    sqlx::query("DROP TRIGGER fail_inventory_writes")
        .execute(&pool)
        .await
        .expect("drop the trigger");
    tokio::time::sleep(Duration::from_millis(2_500)).await;

    let token = request_world_token(&h.auth_url, &session, char_id).await;
    let answer = WorldClient::start_refused(token, &session, char_id).await;
    assert!(
        matches!(answer, ServerWorldMsg::ConnectOk { .. }),
        "once the retry lands the relogin succeeds: {answer:?}"
    );
    let loaves = count_item_rows(
        &h.db_url,
        "SELECT COUNT(*) FROM character_items WHERE char_id = ?1 AND item_path = ?2",
        char_id,
        BREAD,
    )
    .await;
    assert_eq!(loaves, 1, "the retried save holds the loaf");
}

/// A death's XP loss is saved with the corpse that records it. Before, the
/// corpse (and its refundable `lost_xp`) was written at once while the loss
/// waited for the checkpoint, so a crash gave the XP back and a resurrection
/// then refunded part of it again. A level 10 with no progress dies and
/// drops to level 9; the database says 9 the moment the corpse exists.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_death_saves_its_xp_loss_with_the_corpse() {
    let h = start_both().await;
    let (session, char_id, _stale) =
        provision_client(&h.auth_url, "xpdeath", "Fallen", "Human", "Warrior").await;
    set_char_level(&h.db_url, char_id, 10).await;
    let token = gm_world_token(&h.auth_url, &h.db_url, "xpdeath", &session, char_id).await;
    let mut a = WorldClient::start(token, &session, char_id).await;
    a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::InventorySnapshot { .. })
    })
    .await
    .expect("snapshot");

    a.send_death();
    a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::CorpseContents { .. })
    })
    .await
    .expect("the corpse is made");

    let (_xp, level) = char_xp_level(&h.db_url, char_id).await;
    assert_eq!(level, 9, "the de-level must be saved with the corpse");
}

/// A quest's reward XP is saved with its completion. Before, the completion
/// was written at once and the XP waited for the checkpoint, so a crash kept
/// the quest done and lost the reward for good.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_quest_turn_in_saves_its_reward_xp_with_the_completion() {
    let h = start_both().await;
    let (session, char_id, token) =
        provision_client(&h.auth_url, "questxp", "Rewarded", "Human", "Warrior").await;
    {
        let pool = projectdawn_server::db::open(&h.db_url).await.expect("open pool");
        projectdawn_server::db::save_quest_progress(&pool, char_id, "rat_infestation", &[8])
            .await
            .expect("seed a finished quest");
    }
    let before = char_xp_level(&h.db_url, char_id).await;
    let mut a = WorldClient::start(token, &session, char_id).await;

    send_msg(
        &mut a.client,
        CHANNEL_SYSTEM,
        &ClientWorldMsg::CompleteQuest { quest_id: "rat_infestation".into() },
    );
    a.wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
        matches!(m, ServerWorldMsg::QuestCompleted { quest_id } if quest_id == "rat_infestation")
    })
    .await
    .expect("the quest is completed");

    let after = char_xp_level(&h.db_url, char_id).await;
    assert_ne!(after, before, "the reward XP must be saved with the completion");
}
