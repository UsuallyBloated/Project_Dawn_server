//! 20 Hz tick scheduler. Owns the connection map exclusively (no locking)
//! and drives both the renet transport and the application-layer message
//! pipeline.

use super::{
    connection::PerConnection,
    handlers::{self, Outcome},
    persistence, CHANNEL_POSITION, CHANNEL_SYSTEM, CHECKPOINT_INTERVAL, TICK_DT,
};
use crate::{db, Config};
use protocol::world::KickCode;
use renet::{ClientId, RenetServer, ServerEvent};
use renet_netcode::NetcodeServerTransport;
use sqlx::SqlitePool;
use std::{collections::HashMap, sync::Arc, time::Instant};

pub async fn run(
    cfg: Arc<Config>,
    pool: SqlitePool,
    mut server: RenetServer,
    mut transport: NetcodeServerTransport,
) -> anyhow::Result<()> {
    let _ = cfg; // Reserved for future config-driven tuning (max_clients live-reload, etc.).

    let mut connections: HashMap<ClientId, PerConnection> = HashMap::new();
    let mut interval = tokio::time::interval(TICK_DT);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    let mut last_checkpoint = Instant::now();

    loop {
        interval.tick().await;
        let now = Instant::now();

        // 1. Advance the transport (reads UDP packets, processes netcode handshakes).
        if let Err(e) = transport.update(TICK_DT, &mut server) {
            tracing::warn!(error = %e, "transport update error");
        }

        // 2. Drain transport-level events (Connected/Disconnected).
        while let Some(event) = server.get_event() {
            match event {
                ServerEvent::ClientConnected { client_id } => {
                    let user_data = transport.user_data(client_id);
                    let account_id = parse_account_id_from_user_data(user_data);
                    // The renet ClientId equals the ConnectToken's `client_id`,
                    // which the auth handler set to `char_id`.
                    let char_id = client_id_to_char(client_id);
                    tracing::info!(%client_id, char_id, account_id, "client connected (transport)");

                    match db::load_character(&pool, char_id).await {
                        Ok(spawn) => {
                            if spawn.account_id != account_id {
                                tracing::warn!(
                                    expected_account = account_id,
                                    actual_account = spawn.account_id,
                                    char_id,
                                    "ConnectToken account_id mismatch — kicking"
                                );
                                handlers::send_kick(
                                    &mut server,
                                    client_id,
                                    KickCode::Unknown,
                                    "account/char mismatch",
                                );
                                server.disconnect(client_id);
                                continue;
                            }
                            connections.insert(
                                client_id,
                                PerConnection::from_spawn(spawn, now),
                            );
                        }
                        Err(e) => {
                            tracing::warn!(char_id, error = %e, "failed to load character");
                            handlers::send_kick(
                                &mut server,
                                client_id,
                                KickCode::Unknown,
                                "character not found",
                            );
                            server.disconnect(client_id);
                        }
                    }
                }
                ServerEvent::ClientDisconnected { client_id, reason } => {
                    tracing::info!(%client_id, ?reason, "client disconnected (transport)");

                    // Drain any pending app-layer messages before removing the
                    // connection. Without this, if the client sent
                    // ClientWorldMsg::Disconnect and then tore down the
                    // transport in the same UDP burst, the message is silently
                    // lost because this event handler runs before the
                    // message-drain phase below — and that phase skips clients
                    // not in `connections`. Outcome is ignored; we're already
                    // disconnecting.
                    if let Some(conn) = connections.get_mut(&client_id) {
                        for &channel in &[CHANNEL_SYSTEM, CHANNEL_POSITION] {
                            while let Some(bytes) =
                                server.receive_message(client_id, channel)
                            {
                                if let Some(msg) = handlers::decode_client(&bytes) {
                                    let _ = handlers::handle_message(
                                        &mut server, conn, client_id, msg, now,
                                    );
                                }
                            }
                        }
                    }

                    if let Some(mut conn) = connections.remove(&client_id) {
                        // One last save for the road. Failure is non-fatal —
                        // worst case the player rolls back to the last 60 s
                        // checkpoint.
                        if conn.is_dirty_for_persist() {
                            let zone = conn.zone.clone();
                            if let Err(e) = db::checkpoint_position(
                                &pool,
                                conn.char_id,
                                zone.as_deref(),
                                conn.pos.into_tuple(),
                                conn.yaw,
                            )
                            .await
                            {
                                tracing::warn!(
                                    char_id = conn.char_id,
                                    error = %e,
                                    "final checkpoint on disconnect failed"
                                );
                            } else {
                                conn.mark_persisted();
                            }
                        }
                    }
                }
            }
        }

        // 3. Drain incoming application messages on each channel for each client.
        let client_ids: Vec<ClientId> = server.clients_id_iter().collect();
        let mut to_disconnect: Vec<ClientId> = Vec::new();
        for client_id in client_ids {
            // Skip clients whose Connected event is in the queue but whose
            // PerConnection row hasn't been built yet (load_character failed
            // and we already queued the kick).
            if !connections.contains_key(&client_id) {
                continue;
            }
            for &channel in &[CHANNEL_SYSTEM, CHANNEL_POSITION] {
                while let Some(bytes) = server.receive_message(client_id, channel) {
                    let Some(msg) = handlers::decode_client(&bytes) else {
                        continue;
                    };
                    let conn = connections.get_mut(&client_id).expect("checked above");
                    if let Outcome::Disconnect =
                        handlers::handle_message(&mut server, conn, client_id, msg, now)
                    {
                        to_disconnect.push(client_id);
                    }
                }
            }
        }

        // 4. App-layer heartbeat timeout — catches frozen game windows that
        //    transport-level keepalive doesn't notice.
        for (client_id, conn) in connections.iter() {
            if conn.is_app_idle(now) {
                tracing::info!(
                    char_id = conn.char_id,
                    "app-layer heartbeat timeout — disconnecting"
                );
                to_disconnect.push(*client_id);
            }
        }

        for client_id in to_disconnect {
            server.disconnect(client_id);
        }

        // 5. Broadcast position to each ready client. Slice 1 has no AOI
        //    (one player) so this is just an echo back to the owner.
        for (client_id, conn) in connections.iter_mut() {
            if !conn.ready {
                continue;
            }
            handlers::broadcast_position(&mut server, *client_id, conn);
        }

        // 6. Periodic checkpoint.
        if now.duration_since(last_checkpoint) >= CHECKPOINT_INTERVAL {
            let mut dirty: Vec<&mut PerConnection> = connections.values_mut().collect();
            persistence::checkpoint_dirty(&pool, &mut dirty).await;
            last_checkpoint = now;
        }

        // 7. Push outbound packets to the network.
        transport.send_packets(&mut server);

    }
}

fn client_id_to_char(client_id: ClientId) -> i64 {
    // ClientId in renet 2.0 is `pub type ClientId = u64;`. We minted it as
    // char_id (i64) cast to u64; cast back. Positive ids roundtrip cleanly.
    client_id as i64
}

fn parse_account_id_from_user_data(user_data: Option<[u8; 256]>) -> i64 {
    let Some(data) = user_data else { return 0 };
    let mut buf = [0u8; 8];
    buf.copy_from_slice(&data[..8]);
    i64::from_le_bytes(buf)
}
