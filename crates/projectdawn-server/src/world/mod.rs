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
mod cast_gate;
mod clock;
mod combat;
mod connection;
mod corpses;
mod entity;
mod groups;
mod handlers;
mod inventory;
mod items;
mod loot;
mod named;
mod npcs;
mod persistence;
mod pet_templates;
mod progression;
mod quests;
mod regen;
mod safe_areas;
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
use std::{
    collections::HashMap,
    net::{SocketAddr, ToSocketAddrs, UdpSocket},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, OnceLock,
    },
    time::Duration,
};
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
/// How often the address watch re-resolves `PROJECTDAWN_WORLD_ENDPOINT` when
/// it is a name (a DDNS name on the public host, whose home IP can change).
/// Five minutes is the DDNS updater's own cadence; a tighter watch would only
/// find the old address again.
pub const ADDRESS_WATCH_INTERVAL: Duration = Duration::from_secs(300);

// ── The advertised world address ──────────────────────────────────────────
// `PROJECTDAWN_WORLD_ENDPOINT` names where clients aim their UDP: `ip:port`,
// or on the public host `host:port` (a DDNS name). renet's netcode takes its
// public address ONCE, at construction, and refuses a connect token naming
// any other, so the name is resolved at boot and that address is what both
// the transport and every token carry. If the name later moves (the home IP
// changed), the address watch asks the world loop to checkpoint everyone and
// exit; systemd restarts the process on the new address, and the ~minute of
// reconnecting is the whole cost. The tokens never chase DNS on their own:
// one that named a new address while the old transport still ran would be
// refused for everyone.

/// Every address the advertised endpoint resolves to (one, for a literal).
pub fn resolve_world_endpoint_all(endpoint: &str) -> anyhow::Result<Vec<SocketAddr>> {
    if let Ok(addr) = endpoint.parse::<SocketAddr>() {
        return Ok(vec![addr]);
    }
    let addrs: Vec<SocketAddr> = endpoint
        .to_socket_addrs()
        .with_context(|| {
            format!("world_endpoint '{endpoint}' is neither ip:port nor a resolvable host:port")
        })?
        .collect();
    if addrs.is_empty() {
        anyhow::bail!("world_endpoint '{endpoint}' resolved to no address");
    }
    Ok(addrs)
}

/// Resolve the advertised endpoint to the one address clients are told. A
/// name with several addresses resolves to its first IPv4 one: the public
/// host's port forward is IPv4, and a token naming an IPv6 address the
/// router does not forward would fail for exactly the players whose DNS
/// returns IPv6 first.
pub fn resolve_world_endpoint(endpoint: &str) -> anyhow::Result<SocketAddr> {
    let addrs = resolve_world_endpoint_all(endpoint)?;
    Ok(addrs
        .iter()
        .copied()
        .find(|a| a.is_ipv4())
        .unwrap_or(addrs[0]))
}

/// Resolve `endpoint` and record the result as the address every connect
/// token for it names. `main` calls it before either service starts; the
/// world calls it again at bind and gets the recorded address back.
pub fn register_world_endpoint(endpoint: &str) -> anyhow::Result<SocketAddr> {
    let mut map = boot_addrs().lock().expect("boot addrs poisoned");
    if let Some(addr) = map.get(endpoint) {
        return Ok(*addr);
    }
    let addr = resolve_world_endpoint(endpoint)?;
    map.insert(endpoint.to_string(), addr);
    Ok(addr)
}

/// The address each running world was built with, by its endpoint string
/// (several worlds share a test process, each on its own port).
static BOOT_ADDRS: OnceLock<Mutex<HashMap<String, SocketAddr>>> = OnceLock::new();

fn boot_addrs() -> &'static Mutex<HashMap<String, SocketAddr>> {
    BOOT_ADDRS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The address a connect token for `endpoint` must name: the running world's
/// boot address, or, when no world in this process serves that endpoint (the
/// auth-only tests), a fresh resolve.
pub fn world_public_addr(endpoint: &str) -> anyhow::Result<SocketAddr> {
    if let Some(addr) = boot_addrs()
        .lock()
        .expect("boot addrs poisoned")
        .get(endpoint)
    {
        return Ok(*addr);
    }
    resolve_world_endpoint(endpoint)
}

/// A request for the world loop to finish the tick, tell everyone, write
/// everyone and exit non-zero (systemd runs `Restart=on-failure`). Raised by
/// the address watch; a test raises it by hand.
#[derive(Default)]
pub struct RestartSignal {
    requested: AtomicBool,
    reason: Mutex<String>,
}

impl RestartSignal {
    pub fn request(&self, reason: &str) {
        *self.reason.lock().expect("restart reason poisoned") = reason.to_string();
        self.requested.store(true, Ordering::SeqCst);
    }

    pub fn requested(&self) -> bool {
        self.requested.load(Ordering::SeqCst)
    }

    pub fn reason(&self) -> String {
        self.reason.lock().expect("restart reason poisoned").clone()
    }
}

/// What every player is told when the address watch restarts the server.
pub const RESTART_FOR_ADDRESS_CHANGE: &str =
    "The server's network address changed. Restarting now; your character is saved. Log back in in about a minute.";

/// Re-resolve `endpoint` every `ADDRESS_WATCH_INTERVAL` and ask for a restart
/// the first time it stops resolving to `boot`. A failed lookup keeps the
/// boot address (the name may be briefly unavailable; the transport is not).
async fn address_watch(endpoint: String, boot: SocketAddr, signal: Arc<RestartSignal>) {
    loop {
        tokio::time::sleep(ADDRESS_WATCH_INTERVAL).await;
        let name = endpoint.clone();
        // DNS blocks; keep it off the runtime's worker threads.
        // The boot address merely has to remain among the name's addresses: a
        // resolver that reorders two A records is not a move.
        match tokio::task::spawn_blocking(move || resolve_world_endpoint_all(&name)).await {
            Ok(Ok(addrs)) if !addrs.contains(&boot) => {
                tracing::warn!(
                    %boot,
                    now = ?addrs,
                    "world address changed — restarting so connect tokens name the new address"
                );
                signal.request(RESTART_FOR_ADDRESS_CHANGE);
                return;
            }
            Ok(Ok(_)) => {}
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "address watch: resolve failed; keeping the boot address")
            }
            Err(e) => tracing::warn!(error = %e, "address watch: resolver task failed"),
        }
    }
}
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
/// How often the world loop asks the database which accounts are banned, to
/// remove a banned account's character that is ALREADY in the world (login,
/// session use and world connect refuse a banned account on their own). One
/// small query per interval, skipped while nobody is connected; it is also
/// the longest a ban takes to reach a live session.
pub const BAN_SWEEP_INTERVAL: Duration = Duration::from_secs(10);
/// How long a kicked connection is left up after its Kick message is queued,
/// so the reliable message reaches the client before the transport drops (a
/// disconnect in the same tick would evict it and the player would see only a
/// generic connection loss). The server drops the connection itself after
/// this, so a client that ignores the kick gains one second.
pub const KICK_FLUSH_GRACE: Duration = Duration::from_secs(1);
/// Longest an enemy or pet goes without a Position fan-out while standing
/// still. Positions fan on change (the entity moved or turned since its last
/// send); this keepalive covers a dropped unreliable packet and bounds how
/// stale a client's view can get. Same value as `regen::MAX_BROADCAST_GAP`
/// (the resource fan-out's keepalive) but its own knob: an idle camp's whole
/// Position stream is one send per mob per visible player per this interval,
/// where it used to be one per 20 Hz tick.
pub const ENEMY_POSITION_KEEPALIVE: Duration = Duration::from_millis(500);
/// Cadence of the `enemy position fan` log line (sends in the window, alive
/// enemies, players in world) that measures the stream above. Skipped while
/// nobody is in world.
pub const ENEMY_FAN_REPORT_INTERVAL: Duration = Duration::from_secs(60);
/// Server-side speed cap for movement intents (m/s). Anything faster
/// than this gets clamped — server-authoritative, no exceptions.
/// Where a character with no bind point respawns, and the position a freshly
/// created character starts at (`create_character` leaves `pos_*` NULL, which
/// loads as 0,0,0). Keeping the fallback identical to the new-character spawn
/// means an unbound player always wakes up somewhere known-safe rather than
/// beside whatever just killed them.
pub const STARTER_SPAWN: connection::Vec3f = connection::Vec3f::ZERO;

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
///
/// NOT to be confused with the unrelated `corpses::CORPSE_LINGER_SECS`
/// (player-corpse decay, days). This one is dead-ENEMY despawn timing;
/// renamed off the shared `CORPSE_LINGER_SECS` name (2026-07-17) so a
/// grep can't return both and edit the wrong one.
pub const ENEMY_DESPAWN_LINGER_SECS: f32 = 5.0;
/// Slack factor on the server-side range check for player attack
/// intents. The client computes its own player-to-target distance,
/// which can disagree with the server's view by snapshot-interpolation
/// lag and movement integration timing. 1.5× melee range gives the
/// client room to be slightly behind without rejecting legitimate
/// swings.
pub const ATTACK_RANGE_TOLERANCE: f32 = 1.5;
/// Floor on every aggressive mob's leash (`Entity::leash_range`), in metres.
/// An idle mob only turns on an attacker inside its leash, and a leash
/// defaults to twice the aggro radius, so a mob with a small radius (8 to
/// 12 m) could be nuked from 17 to 24 m out and never respond: inside spell
/// reach, outside its leash. Seen in play 2026-10-06 (a Wild Boar, leash
/// 20 m). The floor sits past `RANGED_ATTACK_RANGE` so nothing a spell or a
/// bow can reach is out of a mob's reach back. Mobs with an aggro radius of
/// 0 (dev-spawned dummies) are left passive.
pub const MIN_LEASH_RANGE: f32 = 30.0;
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

/// How close (metres) a group member must be to the kill to share in it: the
/// XP split, the quest journal tick, and an auto-split coin drop (measured
/// from the corpse). An alive filter sits on top for XP and journal credit
/// (see `award_kill`). 30 m until 2026-10-05, when the user set it to 200 m
/// for all three, knowing the trade-off: a group-mate in town shares from the
/// camps nearest town. See docs/design/group_loot_and_coin.md.
pub const GROUP_SHARE_RANGE: f32 = 200.0;

/// How far a friendly spell (ALLY: heals and buffs on a player or a pet;
/// PET_HEAL) reaches, metres. Hostile spells keep `RANGED_ATTACK_RANGE`.
/// Decided 2026-10-05 (user: 30 m); the client checks one metre inside it.
pub const FRIENDLY_SPELL_RANGE: f32 = 30.0;

/// The global cooldown (spell batch step 4, decided 2026-10-05): after any
/// accepted cast, no spell may START for this long, Bard songs excepted on
/// both sides (twisting is casting songs back to back). Checked at CastStart
/// for a timed cast, so a refused caster loses no cast time, and at CastSpell
/// for an instant one. The client greys every gem for 2.25 s; the server's
/// limit sits a quarter second inside that so an honest fast player with
/// ordinary latency is never refused, while a modified client gains at most
/// that quarter second over an honest one. (EQ's gem refresh is quoted at
/// 1.5 to 2.25 s across eras.)
pub const GLOBAL_COOLDOWN: Duration = Duration::from_millis(2_000);

/// How often a damage-over-time spell ticks on a mob, a pet or a player
/// (spell batch step 5, decided 2026-10-05): every three seconds, dealing
/// `dps x 3` per tick, EQ's cadence. The client's own DoT model ticks at the
/// same rate, so the numbers on screen match the server's.
pub const DOT_TICK_SECS: f32 = 3.0;

/// How far apart (metres) two players may be for `InspectPlayer` to return a
/// paperdoll. Paperdoll slots are public and bags are excluded, so reading
/// one from across the world was only a minor disclosure (exploit audit
/// finding 10), but there was no reason to allow it. Sized as a backstop,
/// looser than the client's own 20 m check in `hud.gd`, so an honest player
/// is never refused over interpolation drift. Tuning knob.
pub const INSPECT_RANGE: f32 = 30.0;

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
/// `account_id` (and the account's `is_gm` flag) are packed into the token's
/// `user_data` so the world server can attribute the connection back to an
/// account, and know whether it is a GM, without another DB round-trip on
/// `ClientConnected`. The token is signed with the netcode private key, so a
/// client cannot forge either field.
///
/// Returns `(token_bytes, expires_at_unix)` — the bytes are the wire form
/// the launcher hands to the game .exe.
pub fn mint_connect_token(
    cfg: &Config,
    advertised_endpoint: &str,
    char_id: u64,
    account_id: i64,
    is_gm: bool,
) -> anyhow::Result<(Vec<u8>, i64)> {
    let server_addr = world_public_addr(advertised_endpoint)?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .context("system clock before UNIX epoch")?;
    let expires_at_unix = (now.as_secs() + CONNECT_TOKEN_EXPIRE_SECS) as i64;

    // user_data is fixed at 256 bytes. We pack:
    //   [0..8]  account_id (LE u64)
    //   [8]     is_gm (0 / 1)
    //   [9..]   zeros (future: premium flag, group hint, etc.)
    // Keep the world-side `parse_*_from_user_data` helpers in tick.rs in step
    // with this layout.
    let mut user_data = [0u8; 256];
    user_data[..8].copy_from_slice(&account_id.to_le_bytes());
    user_data[8] = is_gm as u8;

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
    // The loop returns only on an error or on a restart request, and either
    // way the process should end: `main`'s try_join finishes, the exit is
    // non-zero, and systemd brings the server back (`Restart=on-failure`).
    match handle.await? {
        Ok(()) => Err(anyhow::anyhow!("world tick loop exited")),
        Err(e) => Err(e),
    }
}

/// Test / advanced entry — caller pre-binds the socket (e.g. on ephemeral
/// port) and hands it in. Returns immediately with the JoinHandle for the
/// tick task; caller can either `.await` it or drop it for fire-and-forget.
pub async fn serve_with_socket(
    cfg: Arc<Config>,
    pool: SqlitePool,
    socket: UdpSocket,
) -> anyhow::Result<JoinHandle<anyhow::Result<()>>> {
    serve_with_socket_restartable(cfg, pool, socket, Arc::new(RestartSignal::default())).await
}

/// `serve_with_socket` with the restart signal supplied by the caller, so a
/// test can raise it and watch the loop checkpoint, tell everyone and exit.
pub async fn serve_with_socket_restartable(
    cfg: Arc<Config>,
    pool: SqlitePool,
    socket: UdpSocket,
    restart: Arc<RestartSignal>,
) -> anyhow::Result<JoinHandle<anyhow::Result<()>>> {
    let public_addr = register_world_endpoint(&cfg.world_endpoint)
        .with_context(|| format!("resolving world_endpoint '{}'", cfg.world_endpoint))?;
    tracing::info!(
        advertised = %cfg.world_endpoint,
        resolved = %public_addr,
        "world address resolved; connect tokens will name it"
    );

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

    // Only a NAME can move; a literal address is watched by nobody.
    if cfg.world_endpoint.parse::<SocketAddr>().is_err() {
        let endpoint = cfg.world_endpoint.clone();
        let signal = restart.clone();
        tokio::spawn(async move { address_watch(endpoint, public_addr, signal).await });
    }

    let handle = tokio::spawn(async move {
        let result = tick::run(cfg, pool, server, transport, restart).await;
        if let Err(e) = &result {
            tracing::error!(error = %e, "world tick loop exited");
        }
        result
    });
    Ok(handle)
}

#[cfg(test)]
mod endpoint_tests {
    use super::*;

    #[test]
    fn a_literal_address_resolves_to_itself_and_a_name_to_an_address() {
        let lit = resolve_world_endpoint("203.0.113.9:7777").expect("literal");
        assert_eq!(lit.to_string(), "203.0.113.9:7777");
        // `localhost` resolves everywhere without DNS; the port rides along.
        let local = resolve_world_endpoint("localhost:7777").expect("localhost");
        assert!(local.ip().is_loopback() && local.port() == 7777);
        assert!(resolve_world_endpoint("no.such.host.invalid:7777").is_err());
        assert!(resolve_world_endpoint("not an endpoint").is_err());
    }

    #[test]
    fn a_restart_signal_carries_its_reason() {
        let s = RestartSignal::default();
        assert!(!s.requested() && s.reason().is_empty());
        s.request("moving house");
        assert!(s.requested());
        assert_eq!(s.reason(), "moving house");
    }
}
