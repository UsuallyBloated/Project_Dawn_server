//! Decoded application-message handlers. The tick loop ([`super::tick`])
//! dispatches incoming bytes to these; they mutate the connection state
//! and may queue replies on the renet server for the next packet flush.

use super::{
    connection::{PerConnection, Vec3f},
    CHANNEL_POSITION, CHANNEL_SYSTEM,
};
use bincode::config::standard as bincode_cfg;
use protocol::world::{ClientWorldMsg, KickCode, ServerWorldMsg, Vec3};
use renet::{ClientId, RenetServer};
use std::time::Instant;

/// Outcome of dispatching a single decoded `ClientWorldMsg`. The tick loop
/// uses this to decide whether to keep the connection, tear it down, or
/// follow up with multi-client side effects (EntitySpawn fan-out for a
/// newly app-connected client).
pub enum Outcome {
    Continue,
    Disconnect,
    /// `conn.ready` transitioned from false to true this dispatch. The tick
    /// loop owes the new client an EntitySpawn for every existing ready peer,
    /// and every existing ready peer an EntitySpawn for the new client.
    JustConnected,
    /// Client broadcast its current resources. The handler has already
    /// updated `conn.last_*`; the tick loop fans out to every other ready
    /// peer in a post-dispatch sweep so a single sender's update isn't
    /// duplicated across multiple ResourceUpdate messages in one tick.
    ResourceFanOut,
}

pub fn handle_message(
    server: &mut RenetServer,
    conn: &mut PerConnection,
    client_id: ClientId,
    msg: ClientWorldMsg,
    now: Instant,
) -> Outcome {
    conn.touch(now);
    match msg {
        ClientWorldMsg::Connect {
            session_token: _,
            char_id,
            client_version: _,
        } => {
            // The renet ConnectToken's `client_id` already proved this
            // connection owns `char_id` (auth signed it). Defense-in-depth:
            // if the app-layer message disagrees, hang up.
            if char_id != conn.char_id as u64 {
                tracing::warn!(
                    expected = conn.char_id,
                    got = char_id,
                    %client_id,
                    "Connect char_id mismatch — kicking"
                );
                send_kick(server, client_id, KickCode::Unknown, "char_id mismatch");
                return Outcome::Disconnect;
            }
            if conn.ready {
                // Duplicate Connect — ignore, don't tear down.
                return Outcome::Continue;
            }
            conn.ready = true;
            send_connect_ok(server, client_id, conn);
            // Initial position so the client has something to render against
            // before the first broadcast tick.
            send_position(server, client_id, conn);
            // Tick loop runs the EntitySpawn fan-out (which needs the full
            // connections map, not just this conn).
            Outcome::JustConnected
        }

        ClientWorldMsg::Disconnect => {
            tracing::info!(char_id = conn.char_id, "client requested disconnect");
            Outcome::Disconnect
        }

        ClientWorldMsg::Heartbeat => Outcome::Continue,

        ClientWorldMsg::Move {
            sequence,
            direction,
            jumping: _,
        } => {
            if !conn.ready {
                // Client started sending moves before completing the
                // handshake — drop silently.
                return Outcome::Continue;
            }
            // Drop out-of-order Move packets (unreliable channel can reorder).
            // Also handles wraparound: u32 holds ~2.4 years at 20 Hz.
            if sequence <= conn.last_move_seq && conn.last_move_seq != 0 {
                return Outcome::Continue;
            }
            conn.last_move_seq = sequence;

            // Store the latest intent for the tick loop to integrate exactly
            // once per tick. Integrating here would advance pos N times when
            // N Moves arrive between ticks — at typical client send rates
            // that's a ~3× speedup. Server-authoritative speed cap is
            // enforced by clamping the direction to unit length; the tick
            // loop multiplies by MAX_MOVE_SPEED × TICK_DT.
            let dir = Vec3f { x: direction.x, y: direction.y, z: direction.z };
            conn.latest_direction = dir.clamp_length(1.0);
            conn.last_move_received = Some(now);
            Outcome::Continue
        }

        ClientWorldMsg::ResourceUpdate {
            hp,
            max_hp,
            mp,
            max_mp,
            stamina,
            max_stamina,
        } => {
            if !conn.ready {
                // Client started broadcasting before completing the
                // handshake — drop silently rather than caching garbage.
                return Outcome::Continue;
            }
            conn.last_hp = hp;
            conn.last_max_hp = max_hp;
            conn.last_mp = mp;
            conn.last_max_mp = max_mp;
            conn.last_stamina = stamina;
            conn.last_max_stamina = max_stamina;
            conn.resource_state_set = true;
            Outcome::ResourceFanOut
        }

        // The other ~30 ClientWorldMsg variants land in later tracks.
        // Unknown-but-decoded messages: ignore, don't kick. Unknown-and-
        // failed-to-decode messages don't reach here (decode error is
        // logged in tick.rs).
        _ => Outcome::Continue,
    }
}

/// Encode a Position broadcast for `conn`. Returned bytes can be cloned and
/// sent to multiple recipients per tick; the tick loop calls this once per
/// sender and reuses the bytes across the fan-out.
pub fn build_position_msg(conn: &PerConnection) -> Option<Vec<u8>> {
    let msg = ServerWorldMsg::Position {
        id: conn.char_id as u64,
        pos: Vec3 { x: conn.pos.x, y: conn.pos.y, z: conn.pos.z },
        vel: Vec3 { x: 0.0, y: 0.0, z: 0.0 },
        yaw: conn.yaw,
        sequence: conn.last_move_seq,
    };
    encode(&msg)
}

fn send_position(server: &mut RenetServer, client_id: ClientId, conn: &PerConnection) {
    if let Some(bytes) = build_position_msg(conn) {
        server.send_message(client_id, CHANNEL_POSITION, bytes);
    }
}

/// Send an EntitySpawn for `conn` to a single recipient. Used both for the
/// "new client → existing peers" and "existing peers → new client" sides of
/// the connection handshake fan-out, plus any future per-recipient AOI
/// transitions.
pub fn send_entity_spawn(
    server: &mut RenetServer,
    recipient_id: ClientId,
    conn: &PerConnection,
) {
    let msg = ServerWorldMsg::EntitySpawn {
        id: conn.char_id as u64,
        name: conn.name.clone(),
        race: conn.race.clone(),
        class: conn.class.clone(),
        level: conn.level.max(0) as u32,
        pos: Vec3 { x: conn.pos.x, y: conn.pos.y, z: conn.pos.z },
        yaw: conn.yaw,
    };
    if let Some(bytes) = encode(&msg) {
        server.send_message(recipient_id, CHANNEL_SYSTEM, bytes);
    }
}

/// Send an EntityDespawn for `entity_id` to a single recipient. Reliable
/// system channel so the despawn is guaranteed to arrive even if the
/// preceding Position broadcasts are dropped on the unreliable channel.
pub fn send_entity_despawn(
    server: &mut RenetServer,
    recipient_id: ClientId,
    entity_id: u64,
) {
    let msg = ServerWorldMsg::EntityDespawn { id: entity_id };
    if let Some(bytes) = encode(&msg) {
        server.send_message(recipient_id, CHANNEL_SYSTEM, bytes);
    }
}

/// Fan out `conn`'s cached resources to every recipient as three separate
/// ServerWorldMsg variants (HealthUpdate / ManaUpdate / StaminaUpdate).
/// Each variant is encoded once and the bytes cloned per recipient —
/// matches the Position fan-out pattern.
///
/// No-op if the connection has never broadcast a `ResourceUpdate` (we
/// have nothing meaningful to send and don't want to broadcast zeros).
pub fn fan_out_resources(
    server: &mut RenetServer,
    recipients: &[ClientId],
    conn: &PerConnection,
) {
    if !conn.resource_state_set || recipients.is_empty() {
        return;
    }
    let id = conn.char_id as u64;
    let h = encode(&ServerWorldMsg::HealthUpdate {
        id,
        hp: conn.last_hp,
        max_hp: conn.last_max_hp,
    });
    let m = encode(&ServerWorldMsg::ManaUpdate {
        id,
        mp: conn.last_mp,
        max_mp: conn.last_max_mp,
    });
    let s = encode(&ServerWorldMsg::StaminaUpdate {
        id,
        stamina: conn.last_stamina,
        max: conn.last_max_stamina,
    });
    for recipient in recipients {
        if let Some(b) = &h {
            server.send_message(*recipient, CHANNEL_SYSTEM, b.clone());
        }
        if let Some(b) = &m {
            server.send_message(*recipient, CHANNEL_SYSTEM, b.clone());
        }
        if let Some(b) = &s {
            server.send_message(*recipient, CHANNEL_SYSTEM, b.clone());
        }
    }
}

fn send_connect_ok(server: &mut RenetServer, client_id: ClientId, conn: &PerConnection) {
    let msg = ServerWorldMsg::ConnectOk {
        player_id: conn.char_id as u64,
        name: conn.name.clone(),
        race: conn.race.clone(),
        class: conn.class.clone(),
        level: conn.level.max(0) as u32,
    };
    if let Some(bytes) = encode(&msg) {
        server.send_message(client_id, CHANNEL_SYSTEM, bytes);
    }
}

pub fn send_kick(server: &mut RenetServer, client_id: ClientId, code: KickCode, reason: &str) {
    let msg = ServerWorldMsg::Kick {
        reason: reason.to_string(),
        code,
        reconnect_after_secs: None,
    };
    if let Some(bytes) = encode(&msg) {
        server.send_message(client_id, CHANNEL_SYSTEM, bytes);
    }
}

fn encode(msg: &ServerWorldMsg) -> Option<Vec<u8>> {
    match bincode::serde::encode_to_vec(msg, bincode_cfg()) {
        Ok(b) => Some(b),
        Err(e) => {
            tracing::error!(error = %e, ?msg, "failed to bincode-encode ServerWorldMsg");
            None
        }
    }
}

/// Decode a single client message; returns None on protocol error.
pub fn decode_client(bytes: &[u8]) -> Option<ClientWorldMsg> {
    match bincode::serde::decode_from_slice::<ClientWorldMsg, _>(bytes, bincode_cfg()) {
        Ok((msg, _)) => Some(msg),
        Err(e) => {
            tracing::warn!(error = %e, "failed to bincode-decode ClientWorldMsg");
            None
        }
    }
}
