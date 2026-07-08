//! World UDP service — renet 2.0 over Secure-mode netcode.
//!
//! Architecture: one `RenetServer` + `NetcodeServerTransport` driven by a
//! 20 Hz tick task. Per-connection state lives in [`connection::PerConnection`]
//! inside [`tick::WorldState`]; the tick loop owns it exclusively, so no
//! locking. Persistence runs inline (every 60 s) — for one player a
//! ~1 ms SQLite write inside the tick is acceptable; refactor when the
//! population grows.
//!
//! Auth handoff: the launcher hits `RequestWorldToken` over the auth WS;
//! the auth handler validates the session and char ownership, then calls
//! [`mint_connect_token`] to produce a renet `ConnectToken` signed with
//! the shared `netcode_private_key`. The launcher delivers those bytes to
//! the game .exe (track D will define the temp-file handoff).

mod aoi;
mod buffs;
mod combat;
mod connection;
mod corpses;
mod entity;
mod groups;
mod handlers;
mod inventory;
mod items;
mod loot;
mod persistence;
mod pet_templates;
mod progression;
mod quests;
mod regen;
// `pub` so the persistence layer (`crate::db::load_character`) can read MAX_LEVEL
// to clamp a stored level into the cap on load.
pub mod skills;
mod spawn_points;
mod spells;
mod tick;
mod zones;

use crate::Config;
use anyhow::Context;
use protocol::world::WORLD_PROTOCOL_ID;
use renet::{ChannelConfig, ConnectionConfig, RenetServer, SendType};
use renet_netcode::{
    ConnectToken, NetcodeServerTransport, ServerAuthentication, ServerConfig,
};
use sqlx::SqlitePool;
use std::{net::UdpSocket, sync::Arc, time::Duration};
use tokio::task::JoinHandle;

/// 20 Hz simulation tick.
pub const TICK_DT: Duration = Duration::from_millis(50);
/// Cap on simultaneous world connections. Pre-Steam alpha is friends-tier;
/// 64 covers any realistic load and lets renet pre-allocate slot buffers.
pub const MAX_CLIENTS: usize = 64;
/// Application-layer heartbeat timeout. renet's transport layer has its
/// own keepalive; this catches clients that stop sending app messages
/// (frozen game window) without the transport noticing.
pub const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long an UNCLEAN disconnect (crash, killed client, network drop)
/// leaves the character lingering in the world before it reaps. The body
/// stays targetable and killable for this window (the EverQuest "linkdead"
/// model) and a same-account relogin is refused until it elapses. A clean
/// Quit or a completed `/camp` reaps immediately instead. Tunable; see
/// docs/design/camp_and_linkdead.md.
pub const LINKDEAD_SECS: Duration = Duration::from_secs(30);
/// How long a voluntary `/camp` countdown runs before the player is logged
/// out cleanly. The player must be seated to start and the camp is cancelled
/// if they stand/move or take damage. Tunable; the voluntary mirror of
/// `LINKDEAD_SECS`. See docs/design/camp_and_linkdead.md.
pub const CAMP_SECS: Duration = Duration::from_secs(30);
/// Periodic position checkpoint cadence. Inventory/quest mutations write
/// per-mutation; this is just for "where was I when the power went out".
pub const CHECKPOINT_INTERVAL: Duration = Duration::from_secs(60);
/// Server-side speed cap for movement intents (m/s). Anything faster
/// than this gets clamped — server-authoritative, no exceptions.
pub const MAX_MOVE_SPEED: f32 = 7.5;
/// How long we keep integrating the last received direction after the
/// most recent Move message. Beyond this window, the connection is
/// treated as idle and integration stops — protects against a crashed
/// client visually running forward until the 10 s heartbeat timeout
/// fires. A single missed tick (50 ms) won't trigger this; ~10 missed
/// ticks (500 ms) will.
pub const STALE_MOVE_THRESHOLD: Duration = Duration::from_millis(500);
/// `ConnectToken` validity window. The launcher must hand off to the
/// game .exe and have it connect inside this many seconds.
pub const CONNECT_TOKEN_EXPIRE_SECS: u64 = 30;
/// renet's per-connection idle timeout (server-side). Independent of
/// our app-layer `HEARTBEAT_TIMEOUT` — covers transport-level loss too.
pub const NETCODE_TIMEOUT_SECS: i32 = 15;
/// Track 5 sub-task 3 — how long a dead enemy holds at its death pos
/// before EntityDespawn fires and the spawn point's respawn timer
/// arms. The GDScript `enemy.gd` uses 3s for normal mobs and 30s for
/// skinnable ones; server-authoritative version splits the difference
/// (sub-task 4's skinning support, when it lands, can branch on
/// MobTemplate.is_skinnable).
pub const CORPSE_LINGER_SECS: f32 = 5.0;
/// Slack factor on the server-side range check for player attack
/// intents. The client computes its own player-to-target distance,
/// which can disagree with the server's view by snapshot-interpolation
/// lag and movement integration timing. 1.5× melee range gives the
/// client room to be slightly behind without rejecting legitimate
/// swings.
pub const ATTACK_RANGE_TOLERANCE: f32 = 1.5;
/// Maximum distance a ranged attack (`weapon.is_ranged == true`) can
/// connect at. Mirrors the GDScript `Combat.RANGED_RANGE = 25.0` so
/// bows / crossbows feel the same on server-authoritative combat. The
/// melee-tolerance multiplier isn't applied; ranged already has its
/// own headroom.
pub const RANGED_ATTACK_RANGE: f32 = 25.0;
/// How long a loot bag stays on the ground before EntityDespawn fires
/// and the server drops it. Mirrors the GDScript LootBag's
/// `despawn_timer.wait_time = 120` so existing loot-pickup behaviour
/// stays familiar.
pub const LOOT_BAG_LINGER_SECS: f32 = 120.0;
/// Maximum distance between the looter's server-cached position and
/// the bag for a `LootItem` / `LootAll` intent to be honoured. Matches
/// the GDScript LootBag.LOOT_RANGE constant; the click-to-loot UI on
/// the client already enforces the same range so a legitimate user
/// can't trip this check by accident.
pub const LOOT_PICKUP_RANGE: f32 = 6.0;

/// How close (metres from the corpse) a group member must be to share in
/// an auto-split coin drop. Members further out (e.g. back in town) are
/// excluded. See docs/design/group_loot_and_coin.md.
pub const GROUP_COIN_SHARE_RANGE: f32 = 30.0;

/// Channel ids — kept in lockstep with `protocol/src/world.rs`. Slice 1
/// uses just two; the other two from `server_design.md` §3 (combat events,
/// system shutdown) get added when their handlers land.
pub const CHANNEL_SYSTEM: u8 = 0; // ReliableOrdered: Connect/Disconnect/Heartbeat/ConnectOk/Kick
pub const CHANNEL_POSITION: u8 = 1; // Unreliable: Move / Position

fn channels_config() -> Vec<ChannelConfig> {
    vec![
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
    ]
}

fn connection_config() -> ConnectionConfig {
    let chans = channels_config();
    ConnectionConfig {
        available_bytes_per_tick: 60_000,
        server_channels_config: chans.clone(),
        client_channels_config: chans,
    }
}

/// Bind the world UDP socket. Separated from `serve_with_socket` so tests
/// can grab an ephemeral port and read the bound address before starting
/// the tick task.
pub fn bind_world_socket(world_bind: &str) -> std::io::Result<UdpSocket> {
    let socket = UdpSocket::bind(world_bind)?;
    socket.set_nonblocking(true)?;
    Ok(socket)
}

/// Mint a renet `ConnectToken` signed with the shared netcode key. Called
/// from the auth handler when the launcher hits `RequestWorldToken`.
///
/// `account_id` is packed into the token's `user_data` so the world server
/// can attribute the connection back to an account without another DB
/// round-trip on `ClientConnected`.
///
/// Returns `(token_bytes, expires_at_unix)` — the bytes are the wire form
/// the launcher hands to the game .exe.
pub fn mint_connect_token(
    cfg: &Config,
    advertised_endpoint: &str,
    char_id: u64,
    account_id: i64,
) -> anyhow::Result<(Vec<u8>, i64)> {
    let server_addr = advertised_endpoint
        .parse()
        .with_context(|| format!("world_endpoint not a valid SocketAddr: {advertised_endpoint}"))?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .context("system clock before UNIX epoch")?;
    let expires_at_unix = (now.as_secs() + CONNECT_TOKEN_EXPIRE_SECS) as i64;

    // user_data is fixed at 256 bytes. We pack: [account_id_le u64][zeros].
    // Future fields (premium flag, group hint, etc.) take more of the slot.
    let mut user_data = [0u8; 256];
    user_data[..8].copy_from_slice(&account_id.to_le_bytes());

    let token = ConnectToken::generate(
        now,
        WORLD_PROTOCOL_ID,
        CONNECT_TOKEN_EXPIRE_SECS,
        char_id,
        NETCODE_TIMEOUT_SECS,
        vec![server_addr],
        Some(&user_data),
        &cfg.netcode_private_key,
    )
    .context("ConnectToken::generate failed")?;

    let mut bytes = Vec::with_capacity(2048);
    token
        .write(&mut bytes)
        .context("ConnectToken::write failed")?;
    Ok((bytes, expires_at_unix))
}

/// Production entry — binds from `cfg.world_bind` and runs the tick loop
/// until the task ends or the process exits.
pub async fn serve(cfg: Arc<Config>, pool: SqlitePool) -> anyhow::Result<()> {
    let socket = bind_world_socket(&cfg.world_bind)
        .with_context(|| format!("binding world UDP {}", cfg.world_bind))?;
    let local = socket.local_addr()?;
    tracing::info!(
        bind = %local,
        advertised = %cfg.world_endpoint,
        "world UDP listening (renet 2.0, Secure)"
    );
    let handle = serve_with_socket(cfg, pool, socket).await?;
    handle.await?;
    Ok(())
}

/// Test / advanced entry — caller pre-binds the socket (e.g. on ephemeral
/// port) and hands it in. Returns immediately with the JoinHandle for the
/// tick task; caller can either `.await` it or drop it for fire-and-forget.
pub async fn serve_with_socket(
    cfg: Arc<Config>,
    pool: SqlitePool,
    socket: UdpSocket,
) -> anyhow::Result<JoinHandle<()>> {
    let public_addr = cfg
        .world_endpoint
        .parse()
        .with_context(|| {
            format!("world_endpoint not a valid SocketAddr: {}", cfg.world_endpoint)
        })?;

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .context("system clock before UNIX epoch")?;

    let server_config = ServerConfig {
        current_time: now,
        max_clients: MAX_CLIENTS,
        protocol_id: WORLD_PROTOCOL_ID,
        public_addresses: vec![public_addr],
        authentication: ServerAuthentication::Secure {
            private_key: cfg.netcode_private_key,
        },
    };

    let transport = NetcodeServerTransport::new(server_config, socket)
        .context("constructing NetcodeServerTransport")?;
    let server = RenetServer::new(connection_config());

    let handle = tokio::spawn(async move {
        if let Err(e) = tick::run(cfg, pool, server, transport).await {
            tracing::error!(error = %e, "world tick loop exited with error");
        }
    });
    Ok(handle)
}
