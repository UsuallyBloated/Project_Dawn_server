//! Connections coming and going: connects and disconnects (step 2), the GM audit flush
//! (3-bis), heartbeat timeouts and the linkdead reaper (4 and 4-bis), the logout save and
//! the graceful shutdown.

use super::*;

/// What a player is told when the server shuts down cleanly (a deploy or any
/// `systemctl stop`/`restart`) and their character was saved. Rides
/// `KickCode::Restart`, sent only after the save landed so it is true.
pub(super) const SHUTDOWN_MESSAGE: &str =
    "The world is being restarted. Your progress has been saved; please log in again in a minute.";

/// The same, for a player whose save did not land within the shutdown budget.
pub(super) const SHUTDOWN_UNSAVED_MESSAGE: &str =
    "The world is being restarted. Your last few moments could not be saved; please log in again in a minute.";

/// How long a clean shutdown keeps retrying saves that failed, inside
/// systemd's 20 s `TimeoutStopSec`.
pub(super) const SHUTDOWN_SAVE_BUDGET: Duration = Duration::from_secs(12);

/// How often a logout whose save did not fully land is retried.
pub(super) const PENDING_SAVE_RETRY: Duration = Duration::from_secs(1);

/// How long a failing logout save is retried before it is given up (logged
/// as an error) and the account may log in again, to its last saved state.
/// That state is whole (drops, trades and loot are saved as they happen), so
/// giving up loses progress but cannot duplicate anything; holding the
/// account forever would lock the player out for the life of the process.
pub(super) const PENDING_SAVE_GIVE_UP: Duration = Duration::from_secs(120);

/// Move a connection's queued audit entries into `out` as `gm_actions` rows.
/// Commands refused over budget are summarized as one `audit_overflow` row,
/// written at most once a second per connection (`force` writes it now: the
/// connection is leaving), so a flood is recorded as a flood without the
/// summary itself becoming a write per tick.
pub(super) fn collect_gm_audit(
    conn: &mut PerConnection,
    now: Instant,
    force: bool,
    out: &mut Vec<db::GmActionRow>,
) {
    let (actor_account, target_char) = (conn.account_id, conn.char_id);
    for entry in conn.gm_audit.drain(..) {
        out.push(db::GmActionRow {
            actor_account,
            target_char,
            command: entry.command.to_string(),
            args: entry.args,
        });
    }
    if conn.gm_cmd_refused > 0 {
        let due = force
            || conn
                .gm_overflow_row_at
                .is_none_or(|t| now.saturating_duration_since(t).as_secs_f32() >= 1.0);
        if due {
            out.push(db::GmActionRow {
                actor_account,
                target_char,
                command: "audit_overflow".to_string(),
                args: format!("refused={}", conn.gm_cmd_refused),
            });
            conn.gm_cmd_refused = 0;
            conn.gm_overflow_row_at = Some(now);
        }
    }
}

/// Write collected audit rows in one transaction. A failed write is logged
/// with its rows and dropped; the per-command info lines in the server log
/// remain as the fallback record.
pub(super) async fn write_gm_audit(pool: &SqlitePool, rows: &[db::GmActionRow]) {
    if rows.is_empty() {
        return;
    }
    if let Err(e) = db::record_gm_actions(pool, rows).await {
        tracing::error!(error = %e, rows = ?rows, "gm audit write failed — rows dropped");
    }
}

/// Full disconnect cleanup, shared by the immediate clean-disconnect path and
/// the linkdead reaper sweep: despawn the player body (and any surviving pet)
/// for AOI peers, drop the player from the grid, remove them from their group,
/// then remove the connection and flush its dirty state to the DB.
///
/// Safe to call for a `client_id` that isn't in `connections` (a refused
/// duplicate login never inserted): every step degrades to a no-op. Does NOT
/// call `server.disconnect` — the transport is already torn down by the time
/// we get here on either path.
#[allow(clippy::too_many_arguments)] // the tick loop's state, passed piecewise
pub(super) async fn reap_connection(
    server: &mut RenetServer,
    connections: &mut HashMap<ClientId, PerConnection>,
    aoi: &mut AoiGrid,
    enemies: &mut HashMap<EntityId, Entity>,
    group_manager: &mut GroupManager,
    pool: &SqlitePool,
    client_id: ClientId,
    now: std::time::Instant,
    pending_saves: &mut Vec<PendingSave>,
) {
    // Track 7: remove the leaver from the AOI grid BEFORE computing recipients
    // so entities_visible_from gives the correct set of peers who could see
    // this player. Send EntityDespawn only to that AOI-visible set.
    let despawn_info = connections
        .get(&client_id)
        .filter(|c| c.in_world)
        .map(|c| (c.char_id as u64, c.aoi_cell));
    if let Some((entity_id, leaver_cell)) = despawn_info {
        aoi.remove(entity_id, leaver_cell);
        let visible_peers = aoi.entities_visible_from(leaver_cell);
        let peer_ids: Vec<ClientId> = connections
            .iter()
            .filter(|(id, c)| **id != client_id && c.in_world && visible_peers.contains(*id))
            .map(|(id, _)| *id)
            .collect();
        for peer_id in peer_ids {
            handlers::send_entity_despawn(server, peer_id, entity_id);
        }
        // Despawn any pet still alive (no-op if linkdead already dropped it).
        despawn_owned_pets(server, connections, aoi, enemies, entity_id, client_id, now);
        // Their DoTs end with them (spell batch step 5): a logout pays
        // nothing for what was left ticking. A linkdead body keeps its DoTs
        // for the linger, since it is still in the world to be killed.
        for entity in enemies.values_mut() {
            entity.clear_dots_from(entity_id);
        }
        // And any PvP DoT they left on a player.
        let bearers = clear_player_dots_from(connections, &[entity_id]);
        if !bearers.is_empty() {
            let recipients: Vec<ClientId> = connections
                .iter()
                .filter(|(id, c)| **id != client_id && c.in_world)
                .map(|(id, _)| *id)
                .collect();
            for bearer in bearers {
                if let Some(conn) = connections.get(&bearer) {
                    handlers::fan_out_buff_snapshot(server, &recipients, conn);
                }
            }
        }
    }

    // Track 6 sub-task 5 — remove the leaver from their group. If the group
    // dissolves (one member left), notify them too. The rest of the roster
    // gets a fresh GroupRoster.
    if let Some((gid, remaining, dissolved)) = group_manager.leave(client_id) {
        if dissolved {
            // Group dissolved. Survivors (0 or 1) get an empty roster so their
            // HUD clears. The leaver's transport is already torn down.
            for m in &remaining {
                handlers::fan_group_roster(
                    server,
                    std::slice::from_ref(m),
                    gid,
                    *m,
                    Vec::new(),
                    0,
                );
            }
        } else if let Some(g) = group_manager.groups.get(&gid) {
            // Re-fetch the group with name lookups for the survivor fan-out.
            let members_with_names: Vec<(u64, String)> = g
                .members
                .iter()
                .filter_map(|m| connections.get(m).map(|c| (*m, c.name.clone())))
                .collect();
            let recipients: Vec<ClientId> = g.members.clone();
            handlers::fan_group_roster(
                server,
                &recipients,
                gid,
                g.leader,
                members_with_names,
                g.loot_mode.to_u8(),
            );
            // Same panel-row seed as the intent-path roster fan: the
            // survivors' rosters just changed shape, so refill every row.
            for m in &g.members {
                if let Some(c) = connections.get(m) {
                    handlers::fan_out_resources(server, &recipients, c);
                }
            }
        }
    }

    if let Some(mut conn) = connections.remove(&client_id) {
        // Whatever the audit queue still holds goes out with the connection.
        // The per-tick flush runs before the reaps, so this is normally
        // empty; it is here so a dev command handled in the same pass as a
        // disconnect can never have run unrecorded.
        let mut audit_rows = Vec::new();
        collect_gm_audit(&mut conn, Instant::now(), true, &mut audit_rows);
        write_gm_audit(pool, &audit_rows).await;

        // The moment the character left, for NO RENT items (purged at the
        // next login once thirty minutes have passed). Every departure comes
        // through here: a clean logout, or a linkdead body at the end of its
        // linger (the clean shutdown stamps its own). A crash writes nothing,
        // which only ever spares the items.
        if let Err(e) = db::set_logged_out_at(pool, conn.char_id, unix_now_secs()).await {
            tracing::warn!(char_id = conn.char_id, error = %e, "set_logged_out_at failed");
        }
        // A connection whose message handling panicked is never saved: its
        // in-memory state may be half changed, so its last saved state stands.
        if conn.panicked {
            tracing::error!(char_id = conn.char_id, "panicked connection removed without saving");
            return;
        }
        // One last save for the road, through the same flush as the checkpoint.
        // A character whose save did not fully land is NOT dropped: it waits in
        // `pending_saves`, retried every PENDING_SAVE_RETRY, and its account's
        // relogin is refused until it lands. Dropping it (as this used to)
        // lost everything since the last checkpoint, and letting the player
        // back in early would load the older saved state over it.
        if !persistence::flush_connection(pool, &mut conn, "logout").await {
            tracing::error!(
                char_id = conn.char_id,
                "logout save incomplete; queued for retry, relogin held until it lands"
            );
            pending_saves.push(PendingSave { conn, since: Instant::now() });
        }
    }
}

/// The clean end of the world loop (a stop signal). Every character in the
/// world (linkdead bodies and the logout retry queue included) is saved first,
/// failed saves retried until `SHUTDOWN_SAVE_BUDGET`; the queued GM audit rows
/// are written; then each connected player is told the world is restarting,
/// with the line that is true for them (saved or not), and every connection is
/// closed. Returns how many characters could not be saved. systemd allows
/// 20 s (`TimeoutStopSec`); a healthy shutdown takes well under a second.
pub(super) async fn shut_down_world(
    server: &mut RenetServer,
    transport: &mut NetcodeServerTransport,
    connections: &mut HashMap<ClientId, PerConnection>,
    pending_saves: &mut [PendingSave],
    pool: &SqlitePool,
) -> u32 {
    tracing::info!(
        connections = connections.len(),
        pending_saves = pending_saves.len(),
        "stop signal: saving everyone and closing the world"
    );
    let mut audit_rows = Vec::new();
    for conn in connections.values_mut() {
        collect_gm_audit(conn, Instant::now(), true, &mut audit_rows);
    }
    write_gm_audit(pool, &audit_rows).await;

    let deadline = Instant::now() + SHUTDOWN_SAVE_BUDGET;
    loop {
        let mut all_saved = true;
        for conn in connections
            .values_mut()
            .chain(pending_saves.iter_mut().map(|p| &mut p.conn))
        {
            if persistence::has_unsaved_state(conn)
                && !persistence::flush_connection(pool, conn, "shutdown").await
            {
                all_saved = false;
            }
        }
        if all_saved || Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    // A shutdown is a departure for everyone in the world: stamp the logout
    // time NO RENT items are judged by, as a logout does.
    for conn in connections.values() {
        if let Err(e) = db::set_logged_out_at(pool, conn.char_id, unix_now_secs()).await {
            tracing::warn!(char_id = conn.char_id, error = %e, "set_logged_out_at failed at shutdown");
        }
    }

    let mut unsaved = 0u32;
    for conn in connections
        .values()
        .chain(pending_saves.iter().map(|p| &p.conn))
    {
        if persistence::has_unsaved_state(conn) {
            unsaved += 1;
            tracing::error!(char_id = conn.char_id, "shutdown save incomplete; progress since the last save is lost");
        }
    }
    for (client_id, conn) in connections.iter() {
        if conn.linkdead_since.is_some() {
            continue;
        }
        let line = if persistence::has_unsaved_state(conn) {
            SHUTDOWN_UNSAVED_MESSAGE
        } else {
            SHUTDOWN_MESSAGE
        };
        handlers::send_kick(server, *client_id, KickCode::Restart, line);
    }

    // A few ticks of transport service so the reliable kicks are delivered
    // (and resent if one was lost), then close every connection. The clock
    // before the transport, as in the live loop.
    for _ in 0..5 {
        transport.send_packets(server);
        tokio::time::sleep(TICK_DT).await;
        server.update(TICK_DT);
        if let Err(e) = transport.update(TICK_DT, server) {
            tracing::warn!(error = %e, "transport update error during shutdown");
        }
    }
    transport.send_packets(server);
    transport.disconnect_all(server);
    tracing::info!(
        saved = connections.len() + pending_saves.len() - unsaved as usize,
        unsaved,
        "world closed"
    );
    unsaved
}

pub(super) fn client_id_to_char(client_id: ClientId) -> i64 {
    // ClientId in renet 2.0 is `pub type ClientId = u64;`. We minted it as
    // char_id (i64) cast to u64; cast back. Positive ids roundtrip cleanly.
    client_id as i64
}

pub(super) fn parse_account_id_from_user_data(user_data: Option<[u8; 256]>) -> i64 {
    let Some(data) = user_data else { return 0 };
    let mut buf = [0u8; 8];
    buf.copy_from_slice(&data[..8]);
    i64::from_le_bytes(buf)
}

/// Per-account GM flag, packed at `user_data[8]` by `mint_connect_token`.
/// Absent user_data (shouldn't happen for a Secure token) reads as non-GM.
pub(super) fn parse_is_gm_from_user_data(user_data: Option<[u8; 256]>) -> bool {
    user_data.map(|d| d[8] != 0).unwrap_or(false)
}

pub(super) async fn handle_transport_events(world: &mut WorldState, _q: &mut Intents, now: Instant, transport: &NetcodeServerTransport) {
    let WorldState { cfg, pool, server, connections, aoi, pending_saves, enemies, group_manager, .. } = world;
    // 2. Drain transport-level events (Connected/Disconnected).
    while let Some(event) = server.get_event() {
        match event {
            ServerEvent::ClientConnected { client_id } => {
                let user_data = transport.user_data(client_id);
                let account_id = parse_account_id_from_user_data(user_data);
                let is_gm = parse_is_gm_from_user_data(user_data);
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
                                &mut *server,
                                client_id,
                                KickCode::Unknown,
                                "account/char mismatch",
                            );
                            server.disconnect(client_id);
                            continue;
                        }
                        // Banker slice 2 — one character per account in-world.
                        // If the account is already connected, REFUSE this new
                        // login and leave the existing session untouched. We
                        // never force-disconnect the session already playing:
                        // "log in again to boot your other session" is a known
                        // grief / force-off vector (Lineage II). Denying the new
                        // login also keeps the account-shared vault trivially
                        // single-owner. Tradeoff: after an UNCLEAN disconnect the
                        // player waits for the stale session to time out before
                        // reconnecting; a clean logout frees the account at once.
                        if let Some(existing) =
                            connections.values().find(|c| c.account_id == account_id)
                        {
                            // If the existing session is lingering linkdead, the
                            // soonest a fresh relogin can succeed is when its window
                            // elapses; hand the client that countdown. A LIVE session
                            // (not linkdead) has no countdown — the player must log
                            // that one out first.
                            let reconnect_after_secs = existing.linkdead_since.map(|t| {
                                LINKDEAD_SECS
                                    .saturating_sub(now.duration_since(t))
                                    .as_secs() as u32
                            });
                            tracing::info!(
                                account_id, char_id, ?reconnect_after_secs,
                                "duplicate login refused — account already in-world"
                            );
                            // The EQ phrasing, plus a "try again in ~Ns"
                            // estimate when the existing session is lingering
                            // linkdead (computed once at kick time; a live
                            // session has no countdown — the player must log it
                            // out first). The structured `reconnect_after_secs`
                            // is also sent for a future live-ticking client UI.
                            let reason = match reconnect_after_secs {
                                Some(secs) => format!(
                                    "You already have a character in this world. You can try again in about {secs}s."
                                ),
                                None => {
                                    "You already have a character in this world.".to_string()
                                }
                            };
                            // Send the reason and let the CLIENT tear down its own
                            // transport (its kick handler calls disconnect_now). Calling
                            // server.disconnect() here would evict this connection's
                            // channel buffers before the reliable Kick flushes, so the
                            // client would only ever see a generic transport drop — the
                            // same race leave_session() avoids on the client side. The
                            // refused connection was never inserted into `connections`, so
                            // it holds no world state and its messages are ignored; if the
                            // client ignores the kick, the netcode timeout reaps it.
                            handlers::send_kick_with_reconnect(
                                &mut *server,
                                client_id,
                                KickCode::DuplicateLogin,
                                &reason,
                                reconnect_after_secs,
                            );
                            continue;
                        }
                        // The account's last character is still being saved
                        // (its logout save failed and waits in the retry
                        // queue). Letting it in now would load the older
                        // saved state, and the retry would then write over
                        // whatever this session did. Same no-disconnect kick
                        // as the duplicate-login refusal, with a short retry.
                        // No countdown: when the save lands is not known, and
                        // the client draws one as "log in again in N s".
                        if pending_saves.iter().any(|p| p.conn.account_id == account_id) {
                            tracing::warn!(account_id, char_id, "relogin refused — the last session's save is still pending");
                            handlers::send_kick(
                                &mut *server,
                                client_id,
                                KickCode::DuplicateLogin,
                                "Your last session is still being saved. Please try again in a moment.",
                            );
                            continue;
                        }
                        // A ban holds at world connect too. Banning purges
                        // the account's sessions and session redemption
                        // refuses a banned account, but a connect token
                        // minted just BEFORE the ban is still valid for its
                        // short life; this is what stops it at the door.
                        // Fails closed: if the flag cannot be read, refuse.
                        // Same no-disconnect kick as the refusal above, so
                        // the reliable Kick reaches the client.
                        let banned = match db::account_is_banned(&pool, account_id).await {
                            Ok(banned) => banned,
                            Err(e) => {
                                tracing::error!(account_id, char_id, error = %e,
                                    "could not read the ban flag at connect — refusing");
                                true
                            }
                        };
                        if banned {
                            tracing::info!(account_id, char_id, "banned account refused at world connect");
                            handlers::send_kick(
                                &mut *server,
                                client_id,
                                KickCode::BannedNow,
                                "This account is banned.",
                            );
                            continue;
                        }
                        // Load the persisted stores: inventory, skill
                        // scores, the two item vaults (personal and
                        // account-shared), the server-held spell bar and
                        // the last logout time (migration 0015). Any
                        // failure REFUSES the login. Playing on with an
                        // empty store is not harmless: the next save
                        // rewrites the table from memory, so a transient
                        // read error (a busy database) would wipe the real
                        // inventory, the skills, the whole account bank or
                        // the spell bar, and an empty unseeded bar would
                        // invite the client to seed it again without the
                        // memorize. Same no-disconnect kick as the ban
                        // refusal, so the reason arrives.
                        let loaded = async {
                            let inv = db::load_inventory(&pool, char_id).await?;
                            let skills = db::load_skills(&pool, char_id).await?;
                            let bank = db::load_bank_items(&pool, char_id).await?;
                            let acct = db::load_account_bank_items(&pool, account_id).await?;
                            let gems = db::load_spell_gems(&pool, char_id).await?;
                            let logged_out_at = db::load_logged_out_at(&pool, char_id).await?;
                            Ok::<_, crate::error::AuthError>((inv, skills, bank, acct, gems, logged_out_at))
                        }
                        .await;
                        let (
                            inv_rows,
                            skill_rows,
                            bank_item_rows,
                            account_bank_item_rows,
                            (gem_rows, gems_seeded),
                            logged_out_at,
                        ) = match loaded {
                                Ok(rows) => rows,
                                Err(e) => {
                                    tracing::error!(account_id, char_id, error = %e,
                                        "could not load the character's stores at connect — refusing");
                                    handlers::send_kick(
                                        &mut *server,
                                        client_id,
                                        KickCode::Unknown,
                                        "The server could not load your character. Please try again in a moment.",
                                    );
                                    continue;
                                }
                            };
                        let mut fresh = PerConnection::from_spawn(spawn, now);
                        fresh.fizzles = cfg.fizzles;
                        fresh.panic_probe = cfg.panic_probe.clone();
                        connections.insert(client_id, fresh);
                        // Set the real AOI cell from spawn position +
                        // populate the inventory snapshot. Track 14.2:
                        // run the stat recompute pass so persisted
                        // equipped items reapply their max-HP / armor /
                        // stat bonuses before the EnterWorld snapshot
                        // fans the resource update.
                        if let Some(conn) = connections.get_mut(&client_id) {
                            // Stamp the GM flag from the signed token
                            // (from_spawn only sees the character spawn).
                            conn.is_gm = is_gm;
                            if is_gm {
                                tracing::info!(account_id, char_id, "GM account connected");
                            }
                            conn.aoi_cell = aoi::cell_for(conn.pos.x, conn.pos.z);
                            conn.inventory = inventory::PlayerInventory::from_rows(&inv_rows);
                            conn.bank_items = inventory::ItemVault::from_rows(
                                inventory::BANK_VAULT_SLOTS,
                                &bank_item_rows,
                            );
                            conn.account_bank_items = inventory::ItemVault::from_rows(
                                inventory::ACCOUNT_VAULT_SLOTS,
                                &account_bank_item_rows,
                            );
                            let _ = inventory::recompute_equipped_stats(conn);
                            // Track 18.1 — seed all three skill maps
                            // with starting values (untrained classes
                            // get 0; trainable classes get the L1 cap)
                            // and then overlay persisted rows. New
                            // characters with no DB rows still wind
                            // up with the full key set populated; the
                            // GDScript autoloads expect this shape.
                            skills::seed_starting_scores(conn);
                            for row in &skill_rows {
                                let map = match row.kind.as_str() {
                                    "weapon" => Some(&mut conn.weapon_skills),
                                    "armor" => Some(&mut conn.armor_skills),
                                    "casting" => Some(&mut conn.casting_skills),
                                    _ => None,
                                };
                                if let Some(m) = map {
                                    m.insert(row.key.clone(), row.score);
                                }
                            }
                            conn.spell_gems = gem_rows.into_iter().collect();
                            conn.spell_gems_seeded = gems_seeded;
                            // NO RENT: out for thirty minutes or more, its
                            // NO RENT items are gone (the bank's too, so a
                            // vault is no way to keep them). Marked dirty
                            // so the next checkpoint writes the loss.
                            let away = logged_out_at.map(|t| unix_now_secs() - t);
                            if away.is_some_and(|a| a >= items::NO_RENT_AFTER_SECS) {
                                let mut gone = conn.inventory.purge_no_rent();
                                gone.extend(conn.bank_items.purge_no_rent());
                                gone.extend(conn.account_bank_items.purge_no_rent());
                                if !gone.is_empty() {
                                    conn.inventory_dirty = true;
                                    conn.bank_items_dirty = true;
                                    conn.account_bank_items_dirty = true;
                                    let stacks = gone.len();
                                    let detail = Some(format!("away {} s", away.unwrap_or(0)));
                                    for (path, count) in gone {
                                        conn.ledger(ledger::item_event(
                                            ItemAction::NoRent,
                                            &path,
                                            count,
                                            -i64::from(count),
                                            detail.clone(),
                                        ));
                                    }
                                    tracing::info!(char_id, gone = stacks, away_secs = away, "NO RENT items purged at login");
                                }
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!(char_id, error = %e, "failed to load character");
                        handlers::send_kick(
                            &mut *server,
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

                // Camp + linkdead: if a body is ALREADY lingering linkdead
                // under this client_id, this event is a stray echo, not the
                // body dropping again. Because client_id == char_id, a refused
                // same-character relogin collides with the lingering body's key
                // and emits its own ClientDisconnected when it tears down after
                // the deny-login kick. Draining its messages or re-marking would
                // reset the reap timer and let the body linger forever, so a
                // player could dodge the reap by spamming relogin. Ignore it; the
                // reaper still removes the body when its window elapses.
                if connections
                    .get(&client_id)
                    .is_some_and(|c| c.linkdead_since.is_some())
                {
                    tracing::debug!(
                        %client_id,
                        "ignoring disconnect for already-linkdead body (refused-relogin echo)"
                    );
                    continue;
                }

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
                                // A panic marks the connection, which the
                                // reap below then removes without saving.
                                let _ = intake::handle_contained(
                                    &mut *server, conn, client_id, msg, now,
                                );
                            }
                        }
                    }
                }

                // Camp + linkdead: a CLEAN leave (Quit Game, or a completed
                // /camp — both set `clean_disconnect`) reaps the body at once.
                // An UNCLEAN drop (crash, killed client, network loss) instead
                // marks the connection linkdead: the body stays in `connections`
                // with `in_world = true`, so it remains in the targeting
                // snapshots (vulnerable) and the AOI grid (peers keep seeing it).
                // The reaper sweep removes it once LINKDEAD_SECS elapse. A
                // connection not in `connections` (e.g. a refused duplicate login
                // that never spawned) counts as clean — the reap is then a no-op.
                // A panicked connection is reaped at once too: it may not
                // linger, because the reap at the end of a linger saves.
                let clean = connections
                    .get(&client_id)
                    .map(|c| c.clean_disconnect || c.panicked)
                    .unwrap_or(true);
                if clean {
                    reap_connection(
                        &mut *server,
                        &mut *connections,
                        &mut *aoi,
                        &mut *enemies,
                        &mut *group_manager,
                        &pool,
                        client_id,
                        now,
                        &mut *pending_saves,
                    )
                    .await;
                } else {
                    // Despawn the pet now — a linkdead player can't command it,
                    // and an orphaned warder still chasing mobs is worse than a
                    // brief gap. Group membership is KEPT for the window so a
                    // short drop doesn't double-vanish the player from the roster
                    // (the reaper removes them from the group at the end).
                    let owner_entity = connections
                        .get(&client_id)
                        .filter(|c| c.in_world)
                        .map(|c| c.char_id as u64);
                    if let Some(owner_entity) = owner_entity {
                        despawn_owned_pets(
                            &mut *server,
                            &connections,
                            &mut *aoi,
                            &mut *enemies,
                            owner_entity,
                            client_id,
                            now,
                        );
                    }
                    if let Some(conn) = connections.get_mut(&client_id) {
                        conn.linkdead_since = Some(now);
                        // Linkdead governs from here: drop any in-progress
                        // /camp so the camp sweep doesn't also try to complete
                        // it and race the linkdead reaper.
                        conn.camp_since = None;
                        // Freeze the body so it doesn't keep drifting on its
                        // last movement intent (the movement-integration step
                        // also skips linkdead connections; this is belt-and-
                        // suspenders).
                        conn.latest_direction = Vec3f::ZERO;
                        tracing::info!(
                            char_id = conn.char_id,
                            linger_secs = LINKDEAD_SECS.as_secs(),
                            "client linkdead — body lingers (vulnerable) before reap"
                        );
                    }
                }
            }
        }
    }
}

/// The ids of every connection in the world (linkdead bodies included: they
/// are still in the world to be seen and killed).
pub(super) fn in_world_ids(connections: &HashMap<ClientId, PerConnection>) -> Vec<ClientId> {
    connections
        .iter()
        .filter(|(_, c)| c.in_world)
        .map(|(id, _)| *id)
        .collect()
}

/// Step 3-bis: drop every client whose message handling panicked this tick
/// (`intake::handle_contained`). The transport goes, and the character leaves
/// the world at once through the ordinary reap (peers see it despawn, its pet
/// and group are cleaned up) except that nothing of it is saved: its state
/// may be half changed, and the outcomes it queued this tick were discarded
/// with it. No linger, because the end of a linger saves. The relogin loads
/// the last save.
pub(super) async fn drop_panicked(world: &mut WorldState, q: &mut Intents, now: Instant) {
    let WorldState { server, connections, aoi, enemies, group_manager, pool, pending_saves, .. } = world;
    for client_id in std::mem::take(&mut q.panicked) {
        server.disconnect(client_id);
        reap_connection(
            &mut *server,
            &mut *connections,
            &mut *aoi,
            &mut *enemies,
            &mut *group_manager,
            &*pool,
            client_id,
            now,
            &mut *pending_saves,
        )
        .await;
    }
}

pub(super) async fn flush_gm_audit(world: &mut WorldState, _q: &mut Intents, now: Instant) {
    let WorldState { pool, connections, .. } = world;
    // 3-bis. GM action audit flush. A dev/GM command runs only if
    //    `authorize_dev_cmd` queued its row, and the rows are written
    //    here, straight after the message drain (`reap_connection`
    //    flushes its own connection too, so a command handled in the
    //    same pass as a disconnect is still recorded). One transaction
    //    for the tick. The write volume is bounded over TIME, not just
    //    per tick, by the per-connection command budget: after its burst
    //    a flooding GM client is held to GM_CMD_REFILL_PER_SEC rows a
    //    second plus one overflow summary, and the commands it is refused
    //    do not run.
    let mut audit_rows: Vec<db::GmActionRow> = Vec::new();
    for conn in connections.values_mut() {
        collect_gm_audit(conn, now, false, &mut audit_rows);
    }
    write_gm_audit(&pool, &audit_rows).await;
}

pub(super) fn reap_timed_out(world: &mut WorldState, q: &mut Intents, now: Instant) {
    let WorldState { server, connections, .. } = world;
    let Intents { to_disconnect, .. } = q;
    // 4. App-layer heartbeat timeout — catches frozen game windows that
    //    transport-level keepalive doesn't notice. Skip connections that
    //    are already lingering linkdead: their transport is gone, so
    //    re-flagging them would just spam disconnect every tick while the
    //    reaper below counts down their window.
    for (client_id, conn) in connections.iter() {
        if conn.linkdead_since.is_some() {
            continue;
        }
        if conn.is_app_idle(now) {
            tracing::info!(
                char_id = conn.char_id,
                "app-layer heartbeat timeout — disconnecting"
            );
            to_disconnect.push(*client_id);
        } else if conn
            .kicked_at
            .is_some_and(|t| now.duration_since(t) >= KICK_FLUSH_GRACE)
        {
            // The ban sweep sent this connection its Kick a moment ago;
            // the message has had time to arrive, so drop the transport
            // whether or not the client left by itself. The next tick's
            // disconnect event takes the character out by the ordinary
            // unclean-exit path (linger, final save, reap).
            tracing::info!(char_id = conn.char_id, "kicked connection dropped");
            to_disconnect.push(*client_id);
        }
    }

    for client_id in to_disconnect.iter() {
        server.disconnect(*client_id);
    }
}

pub(super) async fn reap_linkdead(world: &mut WorldState, _q: &mut Intents, now: Instant) {
    let WorldState { pool, server, connections, aoi, pending_saves, enemies, group_manager, .. } = world;
    // 4-bis. Linkdead reaper. A connection marked linkdead on an unclean
    //        disconnect lingers (vulnerable) for LINKDEAD_SECS, then we run
    //        the full disconnect cleanup. renet already dropped its
    //        transport, so reap_connection does NOT call server.disconnect
    //        again — it just despawns, flushes, and removes.
    let to_reap: Vec<ClientId> = connections
        .iter()
        .filter(|(_, c)| c.linkdead_expired(now, LINKDEAD_SECS))
        .map(|(id, _)| *id)
        .collect();
    for client_id in to_reap {
        tracing::info!(%client_id, "linkdead window elapsed — reaping");
        reap_connection(
            &mut *server,
            &mut *connections,
            &mut *aoi,
            &mut *enemies,
            &mut *group_manager,
            &pool,
            client_id,
            now,
            &mut *pending_saves,
        )
        .await;
    }
}

#[cfg(test)]
mod tests {
    // The connect token's user_data layout is a contract between
    // mint_connect_token (packs) and these parsers (read): account_id LE in
    // bytes [0..8], is_gm at byte [8]. If the layout drifts, GM access breaks
    // silently, so pin it.
    #[test]
    fn user_data_roundtrips_account_id_and_gm_flag() {
        use super::{parse_account_id_from_user_data, parse_is_gm_from_user_data};
        let mut ud = [0u8; 256];
        ud[..8].copy_from_slice(&12345i64.to_le_bytes());
        ud[8] = 1;
        assert_eq!(parse_account_id_from_user_data(Some(ud)), 12345);
        assert!(parse_is_gm_from_user_data(Some(ud)));
        ud[8] = 0;
        assert!(!parse_is_gm_from_user_data(Some(ud)));
        // A Secure token always carries user_data, but be defensive: absent
        // reads as account 0 / non-GM (never accidentally-GM).
        assert_eq!(parse_account_id_from_user_data(None), 0);
        assert!(!parse_is_gm_from_user_data(None));
    }
}
