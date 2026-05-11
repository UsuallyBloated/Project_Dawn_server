//! Rust GDExtension exposing a renet 2.0 client to GDScript.
//!
//! Wraps `RenetClient` + Secure-mode `NetcodeClientTransport` and bincode
//! ser/de against `protocol::world::*`. The wire format mirrors the server's
//! `crates/projectdawn-server` exactly via the shared `protocol` crate; any
//! drift would surface at compile time, not at runtime.
//!
//! Handled message types are decoded into typed signals: `ConnectOk`,
//! `Heartbeat`, `Kick`, `Position`, `EntitySpawn`, `EntityDespawn`. Other
//! variants get bubbled up via `unhandled_server_message(channel, bytes)`
//! for forward-compat — when their handlers land, add a typed `match` arm
//! in `classify` and a matching emit in `fire`.

use bincode::config::standard as bincode_cfg;
use godot::classes::{INode, Node};
use godot::prelude::*;
use protocol::world::{ClientWorldMsg, ServerWorldMsg, Vec3 as WireVec3};
use renet::{ChannelConfig, ConnectionConfig, RenetClient, SendType};
use renet_netcode::{ClientAuthentication, ConnectToken, NetcodeClientTransport};
use std::io::Cursor;
use std::net::UdpSocket;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Channel ids — must mirror the server's `crates/projectdawn-server/src/world/mod.rs`.
const CHANNEL_SYSTEM: u8 = 0;
const CHANNEL_POSITION: u8 = 1;

struct GdextNetExtension;

#[gdextension]
unsafe impl ExtensionLibrary for GdextNetExtension {}

/// `Node`-derived class so it can sit at the root of the `Net` autoload tree.
/// State is owned by the renet client + transport; `was_connected` tracks
/// the connected/disconnected edge so we only emit transport_* signals once
/// per state change.
#[derive(GodotClass)]
#[class(base=Node)]
pub struct NetClient {
    base: Base<Node>,
    client: Option<RenetClient>,
    transport: Option<NetcodeClientTransport>,
    was_connected: bool,
}

#[godot_api]
impl INode for NetClient {
    fn init(base: Base<Node>) -> Self {
        Self {
            base,
            client: None,
            transport: None,
            was_connected: false,
        }
    }
}

#[godot_api]
impl NetClient {
    /// Renet handshake completed; safe to send the app-layer `Connect` message.
    #[signal]
    fn transport_connected();

    /// Renet handshake or session torn down. `reason` is best-effort human text.
    #[signal]
    fn transport_disconnected(reason: GString);

    /// Server accepted the app-layer `Connect`; `player_id` is the entity id.
    #[signal]
    fn connect_ok(player_id: i64);

    /// Server sent a `Kick`. `code` is the `KickCode` variant name.
    #[signal]
    fn kicked(reason: GString, code: GString);

    /// Server position broadcast. `sequence` echoes the last accepted Move seq.
    #[signal]
    fn position(id: i64, pos: Vector3, vel: Vector3, yaw: f32, sequence: i64);

    /// Server announces a new entity in the recipient's AOI (slice 3: same
    /// zone, no spatial filter). Carries identity fields the client needs on
    /// first sight; ongoing Positions stay lean.
    #[signal]
    fn entity_spawn(
        id: i64,
        name: GString,
        race: GString,
        class: GString,
        level: i64,
        pos: Vector3,
        yaw: f32,
    );

    /// Server announces an entity left the recipient's AOI (disconnect for
    /// player entities; future: out-of-range, despawn timer, etc.).
    #[signal]
    fn entity_despawn(id: i64);

    /// Server-initiated app-layer Heartbeat (informational).
    #[signal]
    fn heartbeat();

    /// Catch-all for ServerWorldMsg variants slice 1 doesn't decode into a
    /// typed signal yet. GDScript can ignore until a future track wires them.
    #[signal]
    fn unhandled_server_message(channel: i64, bytes: PackedByteArray);

    /// Build a renet client from a serialized `ConnectToken` and start the
    /// transport handshake. `world_endpoint` is informational — the actual
    /// server address is signed inside the token, so this param exists for
    /// caller-side logging only and is otherwise unused.
    ///
    /// Returns false on bad token / socket / clock errors. Caller should
    /// listen for `transport_connected` (success) or `transport_disconnected`
    /// (failure) rather than treating `true` as "connected now".
    #[func]
    fn connect_to_server(
        &mut self,
        token_bytes: PackedByteArray,
        world_endpoint: GString,
    ) -> bool {
        let _ = world_endpoint;
        let bytes_vec: Vec<u8> = token_bytes.to_vec();
        let token = match ConnectToken::read(&mut Cursor::new(&bytes_vec[..])) {
            Ok(t) => t,
            Err(e) => {
                godot_error!("[gdext_net] ConnectToken::read failed: {e}");
                return false;
            }
        };
        let socket = match UdpSocket::bind("0.0.0.0:0") {
            Ok(s) => s,
            Err(e) => {
                godot_error!("[gdext_net] UDP bind failed: {e}");
                return false;
            }
        };
        if let Err(e) = socket.set_nonblocking(true) {
            godot_error!("[gdext_net] socket set_nonblocking failed: {e}");
            return false;
        }
        let now = match SystemTime::now().duration_since(UNIX_EPOCH) {
            Ok(d) => d,
            Err(_) => {
                godot_error!("[gdext_net] system clock before UNIX epoch");
                return false;
            }
        };
        let auth = ClientAuthentication::Secure {
            connect_token: token,
        };
        let transport = match NetcodeClientTransport::new(now, auth, socket) {
            Ok(t) => t,
            Err(e) => {
                godot_error!("[gdext_net] NetcodeClientTransport::new failed: {e}");
                return false;
            }
        };
        self.client = Some(RenetClient::new(connection_config_matching_server()));
        self.transport = Some(transport);
        self.was_connected = false;
        true
    }

    /// Tear down the transport and drop the client. Idempotent.
    #[func]
    fn disconnect_now(&mut self) {
        if let Some(t) = self.transport.as_mut() {
            t.disconnect();
        }
        self.client = None;
        self.transport = None;
        self.was_connected = false;
    }

    /// True only when the renet handshake has completed AND the connection
    /// is still live. Named `is_world_connected` so it doesn't shadow the
    /// inherited `Object::is_connected(signal, callable)`.
    #[func]
    fn is_world_connected(&self) -> bool {
        self.client
            .as_ref()
            .map(|c| c.is_connected())
            .unwrap_or(false)
    }

    /// Pump renet for one frame. Call from `_process(delta)` in GDScript.
    #[func]
    fn poll(&mut self, delta: f64) {
        let dt = Duration::from_secs_f64(delta.max(0.0));
        let pending = self.tick_renet(dt);
        self.fire(pending);
    }

    /// Send the app-layer `Connect` on channel 0. Server replies with
    /// `ConnectOk` (→ `connect_ok` signal) or `Kick` (→ `kicked` signal).
    /// `session_token` must be exactly 32 raw bytes — i.e. `bytes.from_hex_string`
    /// of the launcher's hex token, NOT the hex string itself.
    #[func]
    fn send_app_connect(
        &mut self,
        session_token: PackedByteArray,
        char_id: i64,
        client_version: GString,
    ) -> bool {
        let token_vec = session_token.to_vec();
        if token_vec.len() != 32 {
            godot_error!(
                "[gdext_net] session_token must be 32 bytes; got {}",
                token_vec.len()
            );
            return false;
        }
        let mut token = [0u8; 32];
        token.copy_from_slice(&token_vec);
        let msg = ClientWorldMsg::Connect {
            session_token: token,
            char_id: char_id as u64,
            client_version: client_version.to_string(),
        };
        self.send_app(CHANNEL_SYSTEM, &msg)
    }

    #[func]
    fn send_disconnect(&mut self) -> bool {
        self.send_app(CHANNEL_SYSTEM, &ClientWorldMsg::Disconnect)
    }

    #[func]
    fn send_heartbeat(&mut self) -> bool {
        self.send_app(CHANNEL_SYSTEM, &ClientWorldMsg::Heartbeat)
    }

    /// Send a Move intent on the unreliable position channel. Server clamps
    /// `direction` to unit length and applies the speed cap; we just relay it.
    #[func]
    fn send_move(&mut self, sequence: i64, direction: Vector3, jumping: bool) -> bool {
        let msg = ClientWorldMsg::Move {
            sequence: sequence.max(0) as u32,
            direction: WireVec3 {
                x: direction.x,
                y: direction.y,
                z: direction.z,
            },
            jumping,
        };
        self.send_app(CHANNEL_POSITION, &msg)
    }
}

/// Pending side-effects from a single `tick_renet` call. We collect these
/// inside the `&mut self.client` borrow and emit them after, so signal
/// handlers can re-enter our own methods without violating borrow rules.
#[derive(Default)]
struct Pending {
    transport_connected: bool,
    transport_disconnected: Option<String>,
    incoming: Vec<Incoming>,
}

enum Incoming {
    ConnectOk {
        player_id: i64,
    },
    Heartbeat,
    Kick {
        reason: String,
        code: String,
    },
    Position {
        id: i64,
        pos: WireVec3,
        vel: WireVec3,
        yaw: f32,
        sequence: u32,
    },
    EntitySpawn {
        id: i64,
        name: String,
        race: String,
        class: String,
        level: u32,
        pos: WireVec3,
        yaw: f32,
    },
    EntityDespawn {
        id: i64,
    },
    Raw {
        channel: u8,
        bytes: Vec<u8>,
    },
}

impl NetClient {
    fn send_app(&mut self, channel: u8, msg: &ClientWorldMsg) -> bool {
        let Some(client) = self.client.as_mut() else {
            return false;
        };
        if !client.is_connected() {
            return false;
        }
        let bytes = match bincode::serde::encode_to_vec(msg, bincode_cfg()) {
            Ok(b) => b,
            Err(e) => {
                godot_error!("[gdext_net] encode ClientWorldMsg: {e}");
                return false;
            }
        };
        client.send_message(channel, bytes);
        true
    }

    fn tick_renet(&mut self, dt: Duration) -> Pending {
        let mut p = Pending::default();
        let (Some(client), Some(transport)) =
            (self.client.as_mut(), self.transport.as_mut())
        else {
            return p;
        };

        client.update(dt);
        if let Err(e) = transport.update(dt, client) {
            p.transport_disconnected = Some(format!("transport: {e}"));
        }

        let now_connected = client.is_connected();
        if now_connected && !self.was_connected {
            p.transport_connected = true;
        } else if !now_connected
            && self.was_connected
            && p.transport_disconnected.is_none()
        {
            // Server-initiated drop or timeout: lift renet's reason if any.
            let r = client
                .disconnect_reason()
                .map(|r| format!("{r:?}"))
                .unwrap_or_else(|| "unknown".to_string());
            p.transport_disconnected = Some(r);
        }
        self.was_connected = now_connected;

        for &channel in &[CHANNEL_SYSTEM, CHANNEL_POSITION] {
            while let Some(bytes) = client.receive_message(channel) {
                let raw = bytes.to_vec();
                match decode_server(&raw) {
                    Some(msg) => p.incoming.push(classify(channel, msg, &raw)),
                    None => p.incoming.push(Incoming::Raw {
                        channel,
                        bytes: raw,
                    }),
                }
            }
        }

        if let Err(e) = transport.send_packets(client) {
            godot_error!("[gdext_net] send_packets: {e}");
        }
        p
    }

    fn fire(&mut self, p: Pending) {
        if p.transport_connected {
            self.base_mut()
                .emit_signal("transport_connected", &[]);
        }
        if let Some(reason) = p.transport_disconnected {
            // Drop renet state so the caller can re-`connect_to_server` cleanly.
            self.client = None;
            self.transport = None;
            self.was_connected = false;
            let var = GString::from(reason.as_str()).to_variant();
            self.base_mut()
                .emit_signal("transport_disconnected", &[var]);
        }
        for ev in p.incoming {
            match ev {
                Incoming::ConnectOk { player_id } => {
                    self.base_mut()
                        .emit_signal("connect_ok", &[player_id.to_variant()]);
                }
                Incoming::Heartbeat => {
                    self.base_mut().emit_signal("heartbeat", &[]);
                }
                Incoming::Kick { reason, code } => {
                    self.base_mut().emit_signal(
                        "kicked",
                        &[
                            GString::from(reason.as_str()).to_variant(),
                            GString::from(code.as_str()).to_variant(),
                        ],
                    );
                }
                Incoming::Position {
                    id,
                    pos,
                    vel,
                    yaw,
                    sequence,
                } => {
                    self.base_mut().emit_signal(
                        "position",
                        &[
                            id.to_variant(),
                            Vector3::new(pos.x, pos.y, pos.z).to_variant(),
                            Vector3::new(vel.x, vel.y, vel.z).to_variant(),
                            yaw.to_variant(),
                            (sequence as i64).to_variant(),
                        ],
                    );
                }
                Incoming::EntitySpawn {
                    id,
                    name,
                    race,
                    class,
                    level,
                    pos,
                    yaw,
                } => {
                    self.base_mut().emit_signal(
                        "entity_spawn",
                        &[
                            id.to_variant(),
                            GString::from(name.as_str()).to_variant(),
                            GString::from(race.as_str()).to_variant(),
                            GString::from(class.as_str()).to_variant(),
                            (level as i64).to_variant(),
                            Vector3::new(pos.x, pos.y, pos.z).to_variant(),
                            yaw.to_variant(),
                        ],
                    );
                }
                Incoming::EntityDespawn { id } => {
                    self.base_mut()
                        .emit_signal("entity_despawn", &[id.to_variant()]);
                }
                Incoming::Raw { channel, bytes } => {
                    let pba = packed_byte_array_from(&bytes);
                    self.base_mut().emit_signal(
                        "unhandled_server_message",
                        &[(channel as i64).to_variant(), pba.to_variant()],
                    );
                }
            }
        }
    }
}

fn classify(channel: u8, msg: ServerWorldMsg, raw: &[u8]) -> Incoming {
    match msg {
        ServerWorldMsg::ConnectOk { player_id } => Incoming::ConnectOk {
            player_id: player_id as i64,
        },
        ServerWorldMsg::Heartbeat => Incoming::Heartbeat,
        ServerWorldMsg::Kick { reason, code, .. } => Incoming::Kick {
            reason,
            code: format!("{code:?}"),
        },
        ServerWorldMsg::Position {
            id,
            pos,
            vel,
            yaw,
            sequence,
        } => Incoming::Position {
            id: id as i64,
            pos,
            vel,
            yaw,
            sequence,
        },
        ServerWorldMsg::EntitySpawn {
            id,
            name,
            race,
            class,
            level,
            pos,
            yaw,
        } => Incoming::EntitySpawn {
            id: id as i64,
            name,
            race,
            class,
            level,
            pos,
            yaw,
        },
        ServerWorldMsg::EntityDespawn { id } => Incoming::EntityDespawn { id: id as i64 },
        // Other variants (HealthUpdate, BuffApplied, ChatMessage, ...) get
        // bubbled up raw. As their handlers land, add typed `match` arms here.
        _ => Incoming::Raw {
            channel,
            bytes: raw.to_vec(),
        },
    }
}

fn decode_server(bytes: &[u8]) -> Option<ServerWorldMsg> {
    bincode::serde::decode_from_slice::<ServerWorldMsg, _>(bytes, bincode_cfg())
        .ok()
        .map(|(m, _)| m)
}

fn packed_byte_array_from(bytes: &[u8]) -> PackedByteArray {
    bytes.iter().copied().collect()
}

/// Channel layout matching `crates/projectdawn-server/src/world/mod.rs`. Any
/// drift in `available_bytes_per_tick`, `max_memory_usage_bytes`, or the
/// `SendType` per channel will silently fail the renet handshake.
fn connection_config_matching_server() -> ConnectionConfig {
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
