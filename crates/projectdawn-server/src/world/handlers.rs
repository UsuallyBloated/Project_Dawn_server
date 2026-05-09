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
/// uses this to decide whether to keep the connection or tear it down.
pub enum Outcome {
    Continue,
    Disconnect,
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
            Outcome::Continue
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

        // The other ~30 ClientWorldMsg variants land in later tracks.
        // Unknown-but-decoded messages: ignore, don't kick. Unknown-and-
        // failed-to-decode messages don't reach here (decode error is
        // logged in tick.rs).
        _ => Outcome::Continue,
    }
}

/// Broadcast the client's current position on the unreliable channel. For
/// slice 1 with one player this is just an echo back to the owner; once
/// area-of-interest lands it'll fan out to nearby players too.
pub fn broadcast_position(server: &mut RenetServer, client_id: ClientId, conn: &PerConnection) {
    send_position(server, client_id, conn);
}

fn send_position(server: &mut RenetServer, client_id: ClientId, conn: &PerConnection) {
    let msg = ServerWorldMsg::Position {
        id: conn.char_id as u64,
        pos: Vec3 { x: conn.pos.x, y: conn.pos.y, z: conn.pos.z },
        vel: Vec3 { x: 0.0, y: 0.0, z: 0.0 },
        yaw: conn.yaw,
        sequence: conn.last_move_seq,
    };
    if let Some(bytes) = encode(&msg) {
        server.send_message(client_id, CHANNEL_POSITION, bytes);
    }
}

fn send_connect_ok(server: &mut RenetServer, client_id: ClientId, conn: &PerConnection) {
    let msg = ServerWorldMsg::ConnectOk {
        player_id: conn.char_id as u64,
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
