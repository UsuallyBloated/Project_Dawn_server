//! The world harness: a real server on a free port with its own database, and
//! WorldClient, a renet client that logs in, enters the world and reads what the
//! server sends. Every test module does `use crate::common::*;`.

pub use bincode::config::standard as bincode_cfg;
pub use futures_util::{SinkExt, StreamExt};
pub use projectdawn_server::{auth, char_data, db, world, Config};
pub use protocol::world::{
    ClientWorldMsg, DamageType, ServerWorldMsg, SlotRef, Vec3, ENEMY_ID_BASE, PET_ID_BASE,
};
pub use renet::{ConnectionConfig, RenetClient};
pub use renet_netcode::{ClientAuthentication, ConnectToken, NetcodeClientTransport};
pub use std::{
    io::Cursor,
    net::{SocketAddr, UdpSocket},
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
pub use tempfile::TempDir;
pub use tokio_tungstenite::tungstenite::Message;

pub const CHANNEL_SYSTEM: u8 = 0;
pub const CHANNEL_POSITION: u8 = 1;
pub const TICK_DT: Duration = Duration::from_millis(50);

pub struct Harness {
    pub auth_url: String,
    pub db_url: String,
    pub world_addr: SocketAddr,
    pub _tmp: TempDir,
    /// Send `true` to give the world a stop signal (as systemd's SIGTERM does).
    pub shutdown: tokio::sync::watch::Sender<bool>,
    /// The world task; it ends after a stop signal.
    pub world: Option<tokio::task::JoinHandle<anyhow::Result<()>>>,
}

/// A server with fizzles OFF: a cast fizzles about one time in ten at par
/// (`cast_gate::fizzle_chance`), which would make every single-cast test here
/// a coin flip. The one test about fizzles starts with `start_both_opts(true)`.
pub async fn start_both() -> Harness {
    start_both_opts(false).await
}

pub async fn start_both_opts(fizzles: bool) -> Harness {
    start_with(fizzles, None).await
}

/// A server where saying `line` panics the speaker's message handler
/// (`Config::panic_probe`), for the panic containment test.
pub async fn start_with_panic_probe(line: &str) -> Harness {
    start_with(false, Some(line.to_string())).await
}

async fn start_with(fizzles: bool, panic_probe: Option<String>) -> Harness {
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
        panic_probe,
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
    DB_FOR_AUTH
        .lock()
        .unwrap()
        .insert(auth_url.clone(), url.clone());
    Harness {
        auth_url,
        db_url: url,
        world_addr,
        _tmp: tmp,
        shutdown,
        world: Some(world_handle),
    }
}

/// Which database each harness's auth server writes, so `provision_client`
/// (which only knows the auth URL) can prepare the character's rows.
pub static DB_FOR_AUTH: std::sync::Mutex<std::collections::BTreeMap<String, String>> =
    std::sync::Mutex::new(std::collections::BTreeMap::new());

/// Put every spell the class can ever cast into the character's server-held
/// spell bar (one gem each, past the ten a player has: the server trusts its
/// own rows), marked as held. The server refuses a cast of a spell not
/// memorized (client-authority audit fix 5), so without this every casting
/// test would first have to memorize. The spell bar's own tests start from
/// `forget_all_spells`.
pub async fn memorize_whole_class(db_url: &str, char_id: i64, class: &str) {
    let gems: Vec<(i32, String)> = world::class_spell_names(class)
        .into_iter()
        .enumerate()
        .map(|(i, n)| (i as i32, n))
        .collect();
    let pool = db::open(db_url).await.expect("open pool");
    db::save_spell_gems(&pool, char_id, &gems, true)
        .await
        .expect("memorize the class");
}

/// Empty a character's spell bar and mark it never held, as a character from
/// before the server held bars would be.
pub async fn forget_all_spells(db_url: &str, char_id: i64) {
    let pool = db::open(db_url).await.expect("open pool");
    db::save_spell_gems(&pool, char_id, &[], false)
        .await
        .expect("forget all spells");
}

/// Register + login + char-create + request-token for one client. Returns
/// (session_token_hex, char_id, world_token_bytes).
pub async fn provision_client(
    auth_url: &str,
    username: &str,
    char_name: &str,
    race: &str,
    class: &str,
) -> (String, i64, Vec<u8>) {
    let (mut ws, _) = tokio_tungstenite::connect_async(auth_url)
        .await
        .expect("auth ws connect");

    let resp = rpc(
        &mut ws,
        serde_json::json!({
            "type": "Register",
            "username": username,
            "password": "hunter2!",
        }),
    )
    .await;
    assert_eq!(resp["type"], "RegisterOk", "register {username}: {resp}");

    let resp = rpc(
        &mut ws,
        serde_json::json!({
            "type": "Login",
            "username": username,
            "password": "hunter2!",
            "client_version": "0.1.0",
        }),
    )
    .await;
    assert_eq!(resp["type"], "LoginOk", "login {username}: {resp}");
    let session = resp["session_token"].as_str().unwrap().to_string();

    let resp = rpc(
        &mut ws,
        serde_json::json!({
            "type": "CharCreate",
            "session_token": session,
            "name": char_name,
            "race": race,
            "class": class,
        }),
    )
    .await;
    assert_eq!(
        resp["type"], "CharCreated",
        "charcreate {char_name}: {resp}"
    );
    let char_id = resp["char_id"].as_i64().unwrap();
    let db_url = DB_FOR_AUTH.lock().unwrap().get(auth_url).cloned();
    if let Some(db_url) = db_url {
        memorize_whole_class(&db_url, char_id, class).await;
    }

    let resp = rpc(
        &mut ws,
        serde_json::json!({
            "type": "RequestWorldToken",
            "session_token": session,
            "char_id": char_id,
        }),
    )
    .await;
    assert_eq!(
        resp["type"], "WorldConnectToken",
        "request token {char_name}: {resp}"
    );
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
pub async fn set_char_level(db_url: &str, char_id: i64, level: i32) {
    let pool = projectdawn_server::db::open(db_url)
        .await
        .expect("open pool");
    sqlx::query("UPDATE characters SET level = ?1, mp = 99999.0 WHERE id = ?2")
        .bind(level)
        .bind(char_id)
        .execute(&pool)
        .await
        .expect("bump character level");
}

/// Food and water: set a character's counters (seconds of fullness) and,
/// optionally, its HP, before it logs in (the row is read at login).
pub async fn set_char_appetite(
    db_url: &str,
    char_id: i64,
    hunger: f32,
    thirst: f32,
    hp: Option<f32>,
) {
    let pool = projectdawn_server::db::open(db_url)
        .await
        .expect("open pool");
    sqlx::query(
        "UPDATE characters SET hunger = ?1, thirst = ?2, hp = COALESCE(?3, hp) WHERE id = ?4",
    )
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
pub async fn master_weapon_skill(db_url: &str, char_id: i64, key: &str) {
    let pool = projectdawn_server::db::open(db_url)
        .await
        .expect("open pool");
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
pub async fn set_casting_skill(db_url: &str, char_id: i64, key: &str, score: i32) {
    let pool = projectdawn_server::db::open(db_url)
        .await
        .expect("open pool");
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
pub async fn set_char_stamina(db_url: &str, char_id: i64, stamina: f32) {
    let pool = projectdawn_server::db::open(db_url)
        .await
        .expect("open pool");
    sqlx::query("UPDATE characters SET stamina = ?1 WHERE id = ?2")
        .bind(stamina)
        .bind(char_id)
        .execute(&pool)
        .await
        .expect("set stamina");
}

/// Put a stack straight into a character's inventory row (read at login).
pub async fn give_char_item(
    db_url: &str,
    char_id: i64,
    location: &str,
    slot: u32,
    path: &str,
    count: u32,
) {
    let pool = projectdawn_server::db::open(db_url)
        .await
        .expect("open pool");
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

pub async fn read_char_appetite(db_url: &str, char_id: i64) -> (f32, f32) {
    let pool = projectdawn_server::db::open(db_url)
        .await
        .expect("open pool");
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
pub const CAST_RELEASE_MARGIN: Duration = Duration::from_millis(1_000);

pub const BREAD: &str = "res://data/loot/items/bread_loaf.tres";

pub struct WorldClient {
    pub client: RenetClient,
    pub transport: NetcodeClientTransport,
    pub _char_id: i64,
}

impl WorldClient {
    /// Drive renet handshake + send app-layer Connect; spin until ConnectOk.
    /// Spin the renet handshake and send the app-layer Connect; the caller
    /// waits for the answer (ConnectOk, or a Kick for a refused login).
    pub async fn handshake_and_connect(
        token_bytes: Vec<u8>,
        session_token_hex: &str,
        char_id: i64,
    ) -> Self {
        let connect_token =
            ConnectToken::read(&mut Cursor::new(&token_bytes[..])).expect("ConnectToken::read");
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
        Self {
            client,
            transport,
            _char_id: char_id,
        }
    }

    /// Connect a login the server is expected to REFUSE; returns the Kick.
    pub async fn start_refused(
        token_bytes: Vec<u8>,
        session_token_hex: &str,
        char_id: i64,
    ) -> ServerWorldMsg {
        let mut this = Self::handshake_and_connect(token_bytes, session_token_hex, char_id).await;
        let answer = this
            .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
                matches!(
                    m,
                    ServerWorldMsg::ConnectOk { .. } | ServerWorldMsg::Kick { .. }
                )
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

    pub async fn start(token_bytes: Vec<u8>, session_token_hex: &str, char_id: i64) -> Self {
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
        send_msg(
            &mut this.client,
            CHANNEL_SYSTEM,
            &ClientWorldMsg::EnterWorld,
        );
        for _ in 0..4 {
            tick_one(&mut this.client, &mut this.transport);
            tokio::time::sleep(TICK_DT).await;
        }
        this
    }

    pub fn send_move(&mut self, sequence: u32, direction: Vec3) {
        let msg = ClientWorldMsg::Move {
            sequence,
            direction,
            jumping: false,
            yaw: 0.0,
        };
        send_msg(&mut self.client, CHANNEL_POSITION, &msg);
    }

    /// A Move carrying the real `jumping` flag (the client sets it on the Move
    /// after a jump begins).
    pub fn send_move_jump(&mut self, sequence: u32, direction: Vec3, jumping: bool) {
        let msg = ClientWorldMsg::Move {
            sequence,
            direction,
            jumping,
            yaw: 0.0,
        };
        send_msg(&mut self.client, CHANNEL_POSITION, &msg);
    }

    /// A Move carrying a facing (PD_W0029): the client's body yaw.
    pub fn send_move_facing(&mut self, sequence: u32, direction: Vec3, yaw: f32) {
        let msg = ClientWorldMsg::Move {
            sequence,
            direction,
            jumping: false,
            yaw,
        };
        send_msg(&mut self.client, CHANNEL_POSITION, &msg);
    }

    pub fn send_disconnect(&mut self) {
        send_msg(
            &mut self.client,
            CHANNEL_SYSTEM,
            &ClientWorldMsg::Disconnect,
        );
    }

    pub fn send_cast_start(&mut self, spell_name: &str, duration: f32) {
        let msg = ClientWorldMsg::CastStartBroadcast {
            spell_name: spell_name.into(),
            duration,
        };
        send_msg(&mut self.client, CHANNEL_SYSTEM, &msg);
    }

    pub fn send_cast_fail(&mut self, reason: &str) {
        let msg = ClientWorldMsg::CastFailBroadcast {
            reason: reason.into(),
        };
        send_msg(&mut self.client, CHANNEL_SYSTEM, &msg);
    }

    pub fn send_cast_complete(&mut self, spell_name: &str) {
        let msg = ClientWorldMsg::CastCompleteBroadcast {
            spell_name: spell_name.into(),
        };
        send_msg(&mut self.client, CHANNEL_SYSTEM, &msg);
    }

    pub fn send_cast_spell(&mut self, spell_name: &str, target_id: Option<u64>) {
        let msg = ClientWorldMsg::CastSpell {
            spell_name: spell_name.into(),
            target_id,
        };
        send_msg(&mut self.client, CHANNEL_SYSTEM, &msg);
    }

    pub fn send_hit(&mut self, target: u64, amount: i32, crit: bool, dmg_type: DamageType) {
        let msg = ClientWorldMsg::HitBroadcast {
            target,
            amount,
            crit,
            dmg_type,
        };
        send_msg(&mut self.client, CHANNEL_SYSTEM, &msg);
    }

    pub fn send_death(&mut self) {
        send_msg(
            &mut self.client,
            CHANNEL_SYSTEM,
            &ClientWorldMsg::DeathBroadcast,
        );
    }

    pub fn send_resurrect_accept(&mut self, corpse_id: u64, accept: bool) {
        send_msg(
            &mut self.client,
            CHANNEL_SYSTEM,
            &ClientWorldMsg::ResurrectAccept { corpse_id, accept },
        );
    }

    pub fn send_respawn(&mut self) {
        send_msg(&mut self.client, CHANNEL_SYSTEM, &ClientWorldMsg::Respawn);
    }

    pub fn send_loot_all(&mut self, bag_id: u64) {
        send_msg(
            &mut self.client,
            CHANNEL_SYSTEM,
            &ClientWorldMsg::LootAll { bag_id },
        );
    }

    pub fn send_gm_command(&mut self, line: &str) {
        send_msg(
            &mut self.client,
            CHANNEL_SYSTEM,
            &ClientWorldMsg::GmCommand {
                line: line.to_string(),
            },
        );
    }

    // ── PD_W0028 — trade window intents. ──
    pub fn send_trade_request(&mut self, target: u64) {
        send_msg(
            &mut self.client,
            CHANNEL_SYSTEM,
            &ClientWorldMsg::TradeRequest { target_id: target },
        );
    }

    pub fn send_trade_offer_item(&mut self, window_slot: u8, loc: &str, slot: u32) {
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

    pub fn send_trade_offer_coins(&mut self, coins: protocol::world::Coins) {
        send_msg(
            &mut self.client,
            CHANNEL_SYSTEM,
            &ClientWorldMsg::TradeOfferCoins { coins },
        );
    }

    pub fn send_trade_accept(&mut self) {
        send_msg(
            &mut self.client,
            CHANNEL_SYSTEM,
            &ClientWorldMsg::TradeAccept,
        );
    }

    pub fn send_group_invite(&mut self, name: &str) {
        send_msg(
            &mut self.client,
            CHANNEL_SYSTEM,
            &ClientWorldMsg::GroupInvite {
                name: name.to_string(),
            },
        );
    }

    pub fn send_group_accept(&mut self, from: u64) {
        send_msg(
            &mut self.client,
            CHANNEL_SYSTEM,
            &ClientWorldMsg::GroupAcceptInvite { from },
        );
    }

    pub fn send_heartbeat(&mut self) {
        send_msg(&mut self.client, CHANNEL_SYSTEM, &ClientWorldMsg::Heartbeat);
    }

    /// Dev/GM command: take `amount` damage. Gated like the other dev tools.
    pub fn send_damage_self(&mut self, amount: i32) {
        send_msg(
            &mut self.client,
            CHANNEL_SYSTEM,
            &ClientWorldMsg::DamageSelf { amount },
        );
    }

    /// The Test Panel's Full Heal: tops HP, mana and stamina off (GM only).
    pub fn send_heal_self(&mut self) {
        send_msg(
            &mut self.client,
            CHANNEL_SYSTEM,
            &ClientWorldMsg::HealSelf { amount: 0 },
        );
    }

    pub fn send_inspect_player(&mut self, target_char_id: i64) {
        send_msg(
            &mut self.client,
            CHANNEL_SYSTEM,
            &ClientWorldMsg::InspectPlayer { target_char_id },
        );
    }

    pub fn send_attack(
        &mut self,
        target_id: u64,
        weapon_path: &str,
        is_offhand: bool,
        dmg_type: DamageType,
    ) {
        let msg = ClientWorldMsg::Attack {
            target_id,
            weapon_path: weapon_path.into(),
            is_offhand,
            dmg_type,
        };
        send_msg(&mut self.client, CHANNEL_SYSTEM, &msg);
    }

    pub fn send_pvp_toggle(&mut self, on: bool) {
        send_msg(
            &mut self.client,
            CHANNEL_SYSTEM,
            &ClientWorldMsg::PvpToggle { on },
        );
    }

    pub fn send_sit(&mut self) {
        send_msg(&mut self.client, CHANNEL_SYSTEM, &ClientWorldMsg::Sit);
    }

    pub fn send_stand(&mut self) {
        send_msg(&mut self.client, CHANNEL_SYSTEM, &ClientWorldMsg::Stand);
    }

    pub fn send_memorize_start(&mut self, gem: u8, spell: &str) {
        send_msg(
            &mut self.client,
            CHANNEL_SYSTEM,
            &ClientWorldMsg::MemorizeStart {
                gem,
                spell_name: spell.into(),
            },
        );
    }

    pub fn send_memorize_finish(&mut self) {
        send_msg(
            &mut self.client,
            CHANNEL_SYSTEM,
            &ClientWorldMsg::MemorizeFinish,
        );
    }

    pub fn send_seed_spell_gems(&mut self, gems: &[&str]) {
        let gems = gems.iter().map(|g| g.to_string()).collect();
        send_msg(
            &mut self.client,
            CHANNEL_SYSTEM,
            &ClientWorldMsg::SeedSpellGems { gems },
        );
    }

    /// `/camp`: the one intent that needs the SERVER's seated flag, which
    /// makes it the probe for whether the server thinks a player sits.
    pub fn send_camp(&mut self) {
        send_msg(&mut self.client, CHANNEL_SYSTEM, &ClientWorldMsg::Camp);
    }

    /// Dev/GM command probe: awards xp through the server's authoritative path.
    /// Gated on `can_use_dev_cmds()` (dev server or GM account), so it applies
    /// for a GM and is a silent no-op for a plain account.
    pub fn send_grant_quest_xp(&mut self, amount: i32) {
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
    pub fn send_dev_spawn(
        &mut self,
        name: &str,
        level: u32,
        hp: f32,
        dmg: i32,
        speed: f32,
        aggro: f32,
    ) {
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
    pub fn send_consider(&mut self, npc_id: Option<&str>, target_id: Option<u64>) {
        let msg = ClientWorldMsg::Consider {
            npc_id: npc_id.map(str::to_string),
            target_id,
        };
        send_msg(&mut self.client, CHANNEL_SYSTEM, &msg);
    }

    /// The GM faction panel's slider: set our own standing with one faction.
    /// Gated like the other dev tools.
    pub fn send_dev_set_faction(&mut self, faction_id: &str, standing: f32) {
        let msg = ClientWorldMsg::DevSetFaction {
            faction_id: faction_id.into(),
            standing,
        };
        send_msg(&mut self.client, CHANNEL_SYSTEM, &msg);
    }

    pub fn send_accept_quest(&mut self, quest_id: &str) {
        let msg = ClientWorldMsg::AcceptQuest {
            quest_id: quest_id.into(),
            giver_id: 0,
        };
        send_msg(&mut self.client, CHANNEL_SYSTEM, &msg);
    }

    pub fn send_complete_quest(&mut self, quest_id: &str) {
        let msg = ClientWorldMsg::CompleteQuest {
            quest_id: quest_id.into(),
        };
        send_msg(&mut self.client, CHANNEL_SYSTEM, &msg);
    }

    /// Dev-gated "Despawn All Enemies" (the Test Panel button).
    pub fn send_dev_despawn_enemies(&mut self) {
        send_msg(
            &mut self.client,
            CHANNEL_SYSTEM,
            &ClientWorldMsg::DevDespawnEnemies,
        );
    }

    pub fn send_pet_command(&mut self, command: u8, target_id: Option<u64>) {
        let msg = ClientWorldMsg::PetCommand { command, target_id };
        send_msg(&mut self.client, CHANNEL_SYSTEM, &msg);
    }

    pub fn send_use_skill(&mut self, skill_name: &str, target_id: Option<u64>) {
        let msg = ClientWorldMsg::UseSkill {
            skill_name: skill_name.into(),
            target_id,
        };
        send_msg(&mut self.client, CHANNEL_SYSTEM, &msg);
    }

    pub fn send_stop_song(&mut self) {
        send_msg(&mut self.client, CHANNEL_SYSTEM, &ClientWorldMsg::StopSong);
    }

    /// Pump for `d` with a heartbeat every 2 s, so a quiet wait longer than
    /// the server's 10 s `HEARTBEAT_TIMEOUT` does not read as a drop.
    pub async fn pump_alive_for(&mut self, d: Duration) {
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
    pub fn drain_positions(&mut self) {
        while self.client.receive_message(CHANNEL_POSITION).is_some() {}
    }

    /// Pump for `d`, keeping every SYSTEM message that arrives meanwhile (a
    /// `wait_for` drops what it does not match; a window that must count
    /// several kinds needs them all). Heartbeats every 2 s, as above.
    pub async fn collect_for(&mut self, d: Duration) -> Vec<ServerWorldMsg> {
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

    pub fn send_use_consumable_base(&mut self, idx: u8) {
        send_msg(
            &mut self.client,
            CHANNEL_SYSTEM,
            &ClientWorldMsg::UseConsumable {
                slot: SlotRef::BaseSlot { idx },
            },
        );
    }

    pub fn send_move_item(
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

    pub fn send_split_stack(
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

    pub fn send_drop_item(&mut self, location: &str, slot: u32, count: u32) {
        let msg = ClientWorldMsg::DropItem {
            location: location.into(),
            slot,
            count,
        };
        send_msg(&mut self.client, CHANNEL_SYSTEM, &msg);
    }

    pub fn send_equip_item(&mut self, src_location: &str, src_slot: u32, equip_slot: u8) {
        let msg = ClientWorldMsg::EquipItem {
            src_location: src_location.into(),
            src_slot,
            equip_slot,
        };
        send_msg(&mut self.client, CHANNEL_SYSTEM, &msg);
    }

    pub fn send_unequip_item(&mut self, equip_slot: u8, dst_location: &str, dst_slot: u32) {
        let msg = ClientWorldMsg::UnequipItem {
            equip_slot,
            dst_location: dst_location.into(),
            dst_slot,
        };
        send_msg(&mut self.client, CHANNEL_SYSTEM, &msg);
    }

    pub fn send_buy_item(&mut self, vendor_id: u64, item_name: &str, qty: u32) {
        let msg = ClientWorldMsg::BuyItem {
            vendor_id,
            item_name: item_name.into(),
            qty,
        };
        send_msg(&mut self.client, CHANNEL_SYSTEM, &msg);
    }

    pub fn send_say(&mut self, text: &str) {
        let msg = ClientWorldMsg::Chat {
            channel: protocol::world::ChatChannel::Say,
            text: text.into(),
            target_name: None,
        };
        send_msg(&mut self.client, CHANNEL_SYSTEM, &msg);
    }

    pub fn send_bank_withdraw_item(&mut self, vault_slot: u32) {
        let msg = ClientWorldMsg::BankWithdrawItem {
            shared: false,
            vault_slot,
        };
        send_msg(&mut self.client, CHANNEL_SYSTEM, &msg);
    }

    pub fn send_bank_store_item(&mut self, src_location: &str, src_slot: u32) {
        let msg = ClientWorldMsg::BankStoreItem {
            src_location: src_location.into(),
            src_slot,
            shared: false,
        };
        send_msg(&mut self.client, CHANNEL_SYSTEM, &msg);
    }

    pub fn send_bank_deposit_coins(&mut self, coins: protocol::world::Coins) {
        send_msg(
            &mut self.client,
            CHANNEL_SYSTEM,
            &ClientWorldMsg::BankDepositCoins { coins },
        );
    }

    pub fn send_bank_withdraw_coins(&mut self, coins: protocol::world::Coins) {
        send_msg(
            &mut self.client,
            CHANNEL_SYSTEM,
            &ClientWorldMsg::BankWithdrawCoins { coins },
        );
    }

    pub fn send_bank_exchange(&mut self, from_tier: u8, to_tier: u8, qty: u32) {
        let msg = ClientWorldMsg::BankExchange {
            from_tier,
            to_tier,
            qty,
        };
        send_msg(&mut self.client, CHANNEL_SYSTEM, &msg);
    }

    pub fn send_sell_item(&mut self, slot: protocol::world::SlotRef, qty: u32) {
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
    pub async fn pump_for(&mut self, d: Duration) {
        let end = Instant::now() + d;
        while Instant::now() < end {
            tick_one(&mut self.client, &mut self.transport);
            tokio::time::sleep(TICK_DT).await;
        }
    }

    pub async fn wait_for(
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
}

/// Shared setup for the share-range tests: A (a GM Cleric at spawn) and B (a
/// Warrior seeded `b_x` metres east) grouped, and a one-Smite dummy beside A.
/// Returns the clients and the dummy's id.
pub async fn grouped_pair_with_dummy(
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
    db::set_account_gm(&pool, a_user, true)
        .await
        .expect("set is_gm");
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

/// Walk `metres` east in full-run Moves (RUN_SPEED), then let the park settle.
pub async fn walk_east(c: &mut WorldClient, seq: &mut u32, metres: f32) {
    walk_dir(
        c,
        seq,
        Vec3 {
            x: 1.0,
            y: 0.0,
            z: 0.0,
        },
        metres,
    )
    .await;
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
pub async fn walk_dir(c: &mut WorldClient, seq: &mut u32, dir: Vec3, metres: f32) {
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
pub const BONEPILE_SPAWN: Vec3 = Vec3 {
    x: -16.0,
    y: 0.0,
    z: -14.0,
};

/// Walk toward `target` on the server's own word: each tick a Move aims at
/// `target` from the newest Position the server has fanned for `char_id`
/// (`from` until the first arrives), and once that Position is inside
/// `stop_at` a zero Move parks the mover where it stands. Overshooting
/// between two Positions turns the next Move around, so the walk converges
/// however long a sleep took. Returns the position it stopped on.
pub async fn walk_until_within(
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
        c.send_move(
            *seq,
            Vec3 {
                x: dx / dist,
                y: 0.0,
                z: dz / dist,
            },
        );
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
        c.send_move(
            *seq,
            Vec3 {
                x: 0.0,
                y: 0.0,
                z: 0.0,
            },
        );
        *seq += 1;
        tick_one(&mut c.client, &mut c.transport);
        tokio::time::sleep(TICK_DT).await;
    }
    own
}

pub fn tick_one(client: &mut RenetClient, transport: &mut NetcodeClientTransport) {
    client.update(TICK_DT);
    if let Err(e) = transport.update(TICK_DT, client) {
        eprintln!("client transport update: {e}");
    }
    if let Err(e) = transport.send_packets(client) {
        eprintln!("client transport send: {e}");
    }
}

pub fn send_msg(client: &mut RenetClient, channel: u8, msg: &ClientWorldMsg) {
    let bytes = bincode::serde::encode_to_vec(msg, bincode_cfg()).expect("encode");
    client.send_message(channel, bytes);
}

pub fn connection_config_matching_server() -> ConnectionConfig {
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

pub async fn rpc(
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

pub fn decode_token_bytes(hex_str: &str) -> [u8; 32] {
    let v = hex::decode(hex_str).expect("hex decode session_token");
    v.try_into().expect("32 bytes")
}

/// Makes `username` a GM and mints a fresh connect token carrying the flag (it
/// is packed at mint time). A test that kills a player with `send_death` needs
/// it: `DeathBroadcast` is a dev command since 2026-10-23 (housekeeping audit B1,
/// `docs/design/server_fall_damage.md`), as it is for the Test Panel's Trigger
/// Death in play.
pub async fn gm_world_token(
    auth_url: &str,
    db_url: &str,
    username: &str,
    session_token_hex: &str,
    char_id: i64,
) -> Vec<u8> {
    let pool = db::open(db_url).await.expect("open pool");
    db::set_account_gm(&pool, username, true)
        .await
        .expect("set is_gm");
    request_world_token(auth_url, session_token_hex, char_id).await
}

/// Request a FRESH world ConnectToken for an existing session + char. Used when
/// account state changed after the initial `provision_client` (e.g. `is_gm` was
/// flipped), so the new token reflects it â€” the flag is packed at mint time.
pub async fn request_world_token(auth_url: &str, session_token_hex: &str, char_id: i64) -> Vec<u8> {
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

pub const DEAD_LINE: &str = "You cannot do that while dead.";

pub fn dead_lines(msgs: &[ServerWorldMsg]) -> usize {
    system_lines(msgs)
        .iter()
        .filter(|l| l.as_str() == DEAD_LINE)
        .count()
}

// ── PD_W0028 — the trade window (docs/design/trade_window.md). The tests
// below are the exploit ledger made executable: atomic two-sided commit,
// the every-edit-clears-both-accepts law, the escrow lock, self-trade and
// coin-overdraft refusals. ──

/// Shared setup: A (GM, so it can conjure goods/coins) and B connected at
/// the spawn, with A holding 3 Bread Loaf at a known slot and 25 copper.
pub async fn trade_pair(
    h: &Harness,
    a_user: &str,
    a_name: &str,
    b_user: &str,
    b_name: &str,
) -> (WorldClient, i64, WorldClient, i64, String, u32) {
    let (a_session, a_char_id, _stale) =
        provision_client(&h.auth_url, a_user, a_name, "Human", "Warrior").await;
    let pool = db::open(&h.db_url).await.expect("open pool");
    db::set_account_gm(&pool, a_user, true)
        .await
        .expect("set is_gm");
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
        &ClientWorldMsg::GiveCoins {
            platinum: 0,
            gold: 0,
            silver: 0,
            copper: 25,
        },
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
pub async fn set_char_pos(db_url: &str, char_id: i64, x: f32, z: f32) {
    let pool = projectdawn_server::db::open(db_url)
        .await
        .expect("open pool");
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
pub fn assert_both_connected(a: &WorldClient, b: &WorldClient, during: &str) {
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
pub async fn wait_on_a_pumping_b(
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
        if ticks.is_multiple_of(80) {
            a.send_heartbeat();
            b.send_heartbeat();
        }
        while let Some(bytes) = a.client.receive_message(CHANNEL_SYSTEM) {
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

/// Service both transports for `d`, heart-beating both first so a long pump
/// (a cast bar) cannot run a silent client into the idle timeout.
pub async fn pump_both_for(a: &mut WorldClient, b: &mut WorldClient, d: Duration) {
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
pub async fn pump_all_for(clients: &mut [&mut WorldClient], d: Duration) {
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
pub async fn own_position(c: &mut WorldClient, char_id: i64) -> Option<Vec3> {
    let deadline = Instant::now() + Duration::from_millis(1_000);
    let mut last = None;
    while Instant::now() < deadline {
        let left = deadline.saturating_duration_since(Instant::now());
        match c
            .wait_for(
                CHANNEL_POSITION,
                left,
                |m| matches!(m, ServerWorldMsg::Position { id, .. } if *id == char_id as u64),
            )
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
pub async fn walk_pumping(
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
pub async fn summon_skeleton(b: &mut WorldClient, a: &mut WorldClient, b_char_id: i64) -> u64 {
    b.send_cast_start("Summon Skeleton", 3.0);
    pump_both_for(a, b, Duration::from_millis(150)).await;
    pump_both_for(a, b, Duration::from_millis(3100)).await;
    b.send_cast_spell("Summon Skeleton", None);
    let evt = b
        .wait_for(
            CHANNEL_SYSTEM,
            Duration::from_secs(5),
            |m| matches!(m, ServerWorldMsg::PetSpawn { owner, .. } if *owner == b_char_id as u64),
        )
        .await
        .expect("B sees its own PetSpawn");
    match evt {
        ServerWorldMsg::PetSpawn { id, .. } => id,
        _ => unreachable!(),
    }
}

// ── The world clock ───────────────────────────────────────────────────────

// ── InspectPlayer range gate (exploit audit finding 10) ───────────────────

// ── GM action audit log ───────────────────────────────────────────────────

// ── Cast target pre-flight: reach, and refusals that cost nothing ─────────

/// Sing `song` the honest way (CastStart, the 3 s sing, the release) and
/// return the next second's messages.
pub async fn sing(c: &mut WorldClient, song: &str, target: Option<u64>) -> Vec<ServerWorldMsg> {
    c.send_cast_start(song, 3.0);
    c.pump_alive_for(Duration::from_millis(3_300)).await;
    c.send_cast_spell(song, target);
    c.collect_for(Duration::from_millis(1_000)).await
}

pub fn song_remaining(msgs: &[ServerWorldMsg], who: u64, song: &str) -> Option<f32> {
    msgs.iter().rev().find_map(|m| match m {
        ServerWorldMsg::BuffSnapshot { target, buffs } if *target == who => {
            buffs.iter().find(|(n, _)| n == song).map(|(_, r)| *r)
        }
        _ => None,
    })
}

// ── Food and water, the EQ way (docs/design/food_and_water.md) ─────────────

/// Every system-channel message `c` receives over `d`, decoded, pumping the
/// transport the whole time.
pub async fn collect_system(c: &mut WorldClient, d: Duration) -> Vec<ServerWorldMsg> {
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

pub fn hunger_cap() -> f32 {
    projectdawn_server::world::hunger::FULLNESS_CAP_SECS
}

/// How the server answered a connect attempt: ConnectOk (a live client, past
/// EnterWorld as `WorldClient::start` leaves it), a Kick at the door, or a
/// renet handshake that never completed. Netcode ignores a connect request
/// for a client id that is still connected and sends nothing back, so the
/// third is a real answer and not a harness failure.
// A test harness value, made a handful of times per test: its size does not
// matter, and boxing the client would only add noise at every call site.
#[allow(clippy::large_enum_variant)]
pub enum ConnectOutcome {
    Connected(WorldClient),
    Kicked {
        code: protocol::world::KickCode,
        reconnect_after_secs: Option<u32>,
    },
    HandshakeTimedOut,
}

/// `WorldClient::start` without the panics, reporting how the server
/// answered. A kicked attempt sends the netcode disconnect the real client's
/// kick handler sends (`disconnect_now`), so the refused connection frees its
/// client id at once instead of holding it until a timeout.
pub async fn try_connect(
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
    let mut this = WorldClient {
        client,
        transport,
        _char_id: char_id,
    };
    let answer = this
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(
                m,
                ServerWorldMsg::ConnectOk { .. } | ServerWorldMsg::Kick { .. }
            )
        })
        .await;
    match answer {
        Some(ServerWorldMsg::Kick {
            code,
            reconnect_after_secs,
            ..
        }) => {
            this.transport.disconnect();
            tokio::time::sleep(TICK_DT).await;
            ConnectOutcome::Kicked {
                code,
                reconnect_after_secs,
            }
        }
        Some(_) => {
            send_msg(
                &mut this.client,
                CHANNEL_SYSTEM,
                &ClientWorldMsg::EnterWorld,
            );
            for _ in 0..4 {
                tick_one(&mut this.client, &mut this.transport);
                tokio::time::sleep(TICK_DT).await;
            }
            ConnectOutcome::Connected(this)
        }
        None => {
            panic!("the server answered the Connect with neither ConnectOk nor Kick within 3 s")
        }
    }
}

/// Send a Move along `dir` every tick for `ticks` ticks and return the median
/// distance the server moved this player between consecutive Positions of its
/// own. The server integrates a fixed TICK_DT and fans a moving player's
/// Position each tick, so one step is speed x 0.05 whatever the wall clock
/// does; the median shrugs off a skipped or doubled tick.
pub async fn median_step(
    c: &mut WorldClient,
    char_id: i64,
    seq: &mut u32,
    dir: Vec3,
    ticks: u32,
) -> f32 {
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
    assert!(
        steps.len() >= 5,
        "too few moving Positions to measure ({})",
        steps.len()
    );
    steps.sort_by(|a, b| a.partial_cmp(b).unwrap());
    steps[steps.len() / 2]
}

/// Send one Attack from `c` and report whether the server answered it with a
/// Hit or a Miss from `attacker` within 1.2 s (a refused swing gets nothing).
pub async fn swing_answered(
    c: &mut WorldClient,
    attacker: i64,
    target: u64,
    offhand: bool,
) -> bool {
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

/// Wait for the next InventoryDelta naming this paperdoll slot, if one comes
/// within the window.
pub async fn next_equip_delta(
    c: &mut WorldClient,
    equip_slot: u32,
    window: Duration,
) -> Option<Option<String>> {
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

/// Pump both clients for `window` and return every yaw `watcher` receives in
/// a Position for `id`, in order.
pub async fn yaws_seen(
    watcher: &mut WorldClient,
    other: &mut WorldClient,
    id: u64,
    window: Duration,
) -> Vec<f32> {
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

/// Wait for the next SpellGems and return (gems, seeded).
pub async fn next_spell_gems(c: &mut WorldClient) -> Option<(Vec<String>, bool)> {
    match c
        .wait_for(CHANNEL_SYSTEM, Duration::from_secs(3), |m| {
            matches!(m, ServerWorldMsg::SpellGems { .. })
        })
        .await
    {
        Some(ServerWorldMsg::SpellGems { gems, seeded }) => Some((gems, seeded)),
        _ => None,
    }
}

// ── Active skills on the server (audit item 6, 2026-10-19) ─────────────────
// None of these pass on the previous server: it had no UseSkill handler.

pub async fn spawn_dummy(c: &mut WorldClient, name: &str, dmg: i32, aggro: f32) -> (u64, Vec3) {
    c.send_dev_spawn(name, 1, 100_000.0, dmg, 0.0, aggro);
    let spawn = c
        .wait_for(
            CHANNEL_SYSTEM,
            Duration::from_secs(3),
            |m| matches!(m, ServerWorldMsg::EnemySpawn { mob_name, .. } if mob_name == name),
        )
        .await
        .expect("the dummy spawns");
    match spawn {
        ServerWorldMsg::EnemySpawn { id, pos, .. } => (id, pos),
        _ => unreachable!(),
    }
}

/// A GM of `class`, fists mastered, standing within 1 m of a fresh dummy.
pub async fn gm_beside_dummy(
    h: &Harness,
    user: &str,
    class: &str,
    dummy: &str,
    dmg: i32,
    aggro: f32,
) -> (WorldClient, i64, u64) {
    let (session, char_id, _stale) =
        provision_client(&h.auth_url, user, &format!("{user}a"), "Human", class).await;
    let pool = db::open(&h.db_url).await.expect("open pool");
    db::set_account_gm(&pool, user, true)
        .await
        .expect("set is_gm");
    master_weapon_skill(&h.db_url, char_id, "hand_to_hand").await;
    let token = request_world_token(&h.auth_url, &session, char_id).await;
    let mut c = WorldClient::start(token, &session, char_id).await;
    c.pump_for(Duration::from_millis(300)).await;
    let (id, pos) = spawn_dummy(&mut c, dummy, dmg, aggro).await;
    let mut seq: u32 = 1;
    walk_until_within(
        &mut c,
        &mut seq,
        char_id,
        Vec3 {
            x: 0.0,
            y: 0.0,
            z: 0.0,
        },
        pos,
        1.0,
    )
    .await;
    (c, char_id, id)
}

/// The server's answer to a skill swing within 1.5 s: Ok(Some(amount)) for a
/// Hit, Ok(None) for a Miss, Err(reason) for a SkillFail.
pub async fn skill_answer(
    c: &mut WorldClient,
    user: i64,
    skill: &str,
    target: u64,
) -> Result<Option<i32>, String> {
    c.send_use_skill(skill, Some(target));
    for m in c.collect_for(Duration::from_millis(1_500)).await {
        match m {
            ServerWorldMsg::Hit {
                attacker,
                target: t,
                amount,
                ..
            } if attacker == user as u64 && t == target => return Ok(Some(amount)),
            ServerWorldMsg::Miss {
                attacker,
                target: t,
            } if attacker == user as u64 && t == target => return Ok(None),
            ServerWorldMsg::SkillFail { skill_name, reason } if skill_name == skill => {
                return Err(reason)
            }
            _ => {}
        }
    }
    Err("no answer".into())
}

pub fn mob_hits_on(msgs: &[ServerWorldMsg], mob: u64, player: i64) -> usize {
    msgs.iter()
        .filter(|m| matches!(m, ServerWorldMsg::Hit { attacker, target, .. } if *attacker == mob && *target == player as u64))
        .count()
}

pub fn snapshot_names(msgs: &[ServerWorldMsg], who: u64, name: &str) -> bool {
    msgs.iter().any(|m| {
        matches!(m, ServerWorldMsg::BuffSnapshot { target, buffs }
        if *target == who && buffs.iter().any(|(n, _)| n == name))
    })
}

// ── Bard songs on the server (the rework of 2026-10-20) ─────────────────────
// None of these pass on the previous server: it applied a song once per cast
// and never pulsed, and songs were instant.

// ─── Factions and alignment (2026-10-23) ─────────────────────────────────

// A dev spawn lands 3 m from its requester, and the enter-world seed carries
// the camps' mobs of the same names (a Grey Wolf camp sits 38 m from the
// spawn), so every spawn below is matched beside the caster, not by name alone.

/// The faction standings a batch of messages carries, in order.
pub fn faction_updates(msgs: &[ServerWorldMsg]) -> Vec<(String, f32, u8)> {
    msgs.iter()
        .filter_map(|m| match m {
            ServerWorldMsg::FactionUpdate {
                faction_id,
                standing,
                level,
                ..
            } => Some((faction_id.clone(), *standing, *level)),
            _ => None,
        })
        .collect()
}

/// The last standing a batch carries for one faction.
pub fn last_standing(msgs: &[ServerWorldMsg], faction_id: &str) -> Option<(f32, u8)> {
    faction_updates(msgs)
        .into_iter()
        .filter(|(id, _, _)| id == faction_id)
        .map(|(_, s, l)| (s, l))
        .next_back()
}

pub fn system_lines(msgs: &[ServerWorldMsg]) -> Vec<String> {
    msgs.iter()
        .filter_map(|m| match m {
            ServerWorldMsg::ChatMessage { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

pub fn alignment_scores(msgs: &[ServerWorldMsg]) -> Vec<f32> {
    msgs.iter()
        .filter_map(|m| match m {
            ServerWorldMsg::AlignmentUpdate { score } => Some(*score),
            _ => None,
        })
        .collect()
}

/// Smite's own cooldown (longer than the global one), with a margin.
pub async fn a_breath(c: &mut WorldClient) {
    c.pump_alive_for(Duration::from_millis(4200)).await;
}

/// The same wait with two clients in the world: each is pumped with
/// heartbeats for half of it, so neither sits silent past the server's 10 s
/// idle timeout across a run of casts.
pub async fn a_breath_for_two(a: &mut WorldClient, b: &mut WorldClient) {
    a.pump_alive_for(Duration::from_millis(2100)).await;
    b.pump_alive_for(Duration::from_millis(2100)).await;
}

pub const APPREHENSIVE: u8 = 3;
pub const INDIFFERENT: u8 = 4;
pub const KINDLY: u8 = 6;
pub const DUBIOUS: u8 = 2;

// ---- Hotfix 2026-10-23: the crash and the dupes -------------------------
//
// Each of these fails on the code before the fix.

pub async fn count_item_rows(db_url: &str, sql: &str, id: i64, path: &str) -> i64 {
    let pool = projectdawn_server::db::open(db_url)
        .await
        .expect("open pool");
    let (n,): (i64,) = sqlx::query_as(sql)
        .bind(id)
        .bind(path)
        .fetch_one(&pool)
        .await
        .expect("count rows");
    n
}

// ---- Housekeeping Phase 1: durability -----------------------------------

pub async fn fund_char(db_url: &str, char_id: i64) {
    let pool = projectdawn_server::db::open(db_url)
        .await
        .expect("open pool");
    sqlx::query("UPDATE characters SET platinum = 10 WHERE id = ?1")
        .bind(char_id)
        .execute(&pool)
        .await
        .expect("fund");
}

pub async fn char_xp_level(db_url: &str, char_id: i64) -> (i64, i64) {
    let pool = projectdawn_server::db::open(db_url)
        .await
        .expect("open pool");
    sqlx::query_as("SELECT xp, level FROM characters WHERE id = ?1")
        .bind(char_id)
        .fetch_one(&pool)
        .await
        .expect("xp row")
}

/// Pump both clients until each has received a Kick, returning their codes.
/// Reads the channel directly rather than through `wait_for`, which gives up
/// on a disconnect: the server closes every connection right after the kick.
pub async fn kicks_for_both(
    a: &mut WorldClient,
    b: &mut WorldClient,
) -> (
    Option<protocol::world::KickCode>,
    Option<protocol::world::KickCode>,
) {
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

// ─── The item ledger (2026-10-30) ────────────────────────────────────────────

/// Opening rows for everything the database holds right now, as migration
/// 0017 writes them when the table is created: the tests seed stores straight
/// into the tables, which the ledger never saw.
pub async fn seed_ledger_openings(db_url: &str) {
    let pool = projectdawn_server::db::open(db_url)
        .await
        .expect("open pool");
    for sql in [
        "INSERT INTO item_events (at, char_id, action, item_path, count, delta, copper, detail)
         SELECT 1, char_id, 'opening', item_path, SUM(count), SUM(count), 0, 'inventory'
         FROM character_items GROUP BY char_id, item_path",
        "INSERT INTO item_events (at, char_id, action, item_path, count, delta, copper, detail)
         SELECT 1, char_id, 'opening', item_path, SUM(count), SUM(count), 0, 'vault'
         FROM bank_items GROUP BY char_id, item_path",
        "INSERT INTO item_events (at, char_id, action, item_path, count, delta, copper, detail)
         SELECT 1, id, 'opening', NULL, 0, 0,
                platinum * 1000000 + gold * 10000 + silver * 100 + copper
                  + bank_platinum * 1000000 + bank_gold * 10000 + bank_silver * 100 + bank_copper,
                'wallet and bank'
         FROM characters",
    ] {
        sqlx::query(sql).execute(&pool).await.expect("opening rows");
    }
}

/// A character's ledger rows after the opening ones, oldest first:
/// (action, item path, count, delta, copper).
pub async fn ledger_rows(
    db_url: &str,
    char_id: i64,
) -> Vec<(String, Option<String>, i64, i64, i64)> {
    let pool = projectdawn_server::db::open(db_url)
        .await
        .expect("open pool");
    sqlx::query_as(
        "SELECT action, item_path, count, delta, copper FROM item_events
         WHERE char_id = ?1 AND action != 'opening' ORDER BY id",
    )
    .bind(char_id)
    .fetch_all(&pool)
    .await
    .expect("ledger rows")
}

pub async fn ledger_mismatches(db_url: &str) -> Vec<projectdawn_server::db::LedgerMismatch> {
    let pool = projectdawn_server::db::open(db_url)
        .await
        .expect("open pool");
    projectdawn_server::db::ledger_mismatches(&pool)
        .await
        .expect("ledger check")
}
