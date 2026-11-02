//! (phase module, doc to come)

use super::*;

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
                                let _ = handlers::handle_message(
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
                let clean = connections
                    .get(&client_id)
                    .map(|c| c.clean_disconnect)
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
