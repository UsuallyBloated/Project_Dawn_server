//! End-of-tick upkeep (steps 6-ter to 7): quest progress flushed, the 60 s checkpoint,
//! the `world health` line, logout-save retries, the world clock, the ban sweep and the
//! ledger's daily sweep.

use super::*;

pub(super) async fn run_upkeep(
    world: &mut WorldState,
    _q: &mut Intents,
    now: Instant,
    in_world_ids: &[ClientId],
) {
    let WorldState {
        pool,
        server,
        connections,
        last_checkpoint,
        pending_saves,
        last_pending_retry,
        tick_health,
        last_health_report,
        last_ban_sweep,
        last_ledger_sweep,
        last_time_broadcast,
        ban_tx,
        ban_rx,
        ban_sweep_in_flight,
        ..
    } = world;
    // 6-ter. PD_W0024 — flush quest state touched this tick (kill
    //    increments, accepts, abandons — all sync, they only mark
    //    `quests_dirty`). Drained EVERY tick (not the 60s checkpoint:
    //    losing counted kills to a crash would be worse than the extra
    //    writes) and RECONCILED to the DB: upsert the progress if the quest
    //    is still active, else DELETE the row (abandoned). One write per
    //    touched quest per tick, so a forged accept/abandon storm can't
    //    amplify into an awaited write per message. A failed write
    //    re-queues for next tick.
    for conn in connections.values_mut() {
        if !conn.quests_dirty.is_empty() {
            persistence::flush_quests(pool, conn, "tick").await;
        }
    }

    // 7. Periodic checkpoint.
    if now.duration_since(*last_checkpoint) >= CHECKPOINT_INTERVAL {
        let started = Instant::now();
        let mut dirty: Vec<&mut PerConnection> = connections.values_mut().collect();
        let walked = dirty.len();
        persistence::checkpoint_dirty(pool, &mut dirty).await;
        tick_health.record_checkpoint(started.elapsed(), walked);
        *last_checkpoint = now;
    }

    // 7-health. The `world health` line: tick work times (median, 99th
    //    percentile, worst), the ticks that ran past TICK_DT, the
    //    checkpoint's time, and the player steps the slope rule refused.
    //    Logged while anyone is in the world, and with nobody in it only
    //    when a tick overran (a slow disk with no players still deserves a
    //    line) or a step was refused (its sender may have left since);
    //    otherwise the window just resets.
    if now.duration_since(*last_health_report) >= ENEMY_FAN_REPORT_INTERVAL {
        let report = tick_health.take_report();
        if !in_world_ids.is_empty() || report.demands_a_line() {
            tracing::info!(
                window_secs = now.duration_since(*last_health_report).as_secs(),
                ticks = report.ticks,
                p50_ms = report.p50_ms,
                p99_ms = report.p99_ms,
                max_ms = report.max_ms,
                over_budget = report.over_budget,
                slope_refusals = report.slope_refusals,
                checkpoints = report.checkpoints,
                checkpoint_max_ms = report.checkpoint_max_ms,
                checkpoint_conns = report.checkpoint_conns,
                players_in_world = in_world_ids.len(),
                "world health"
            );
        }
        *last_health_report = now;
    }

    // 7-bis. Retry the logout saves that did not land. Once one does, that
    //    account may log in again.
    if !pending_saves.is_empty() && now.duration_since(*last_pending_retry) >= PENDING_SAVE_RETRY {
        *last_pending_retry = now;
        let mut still_pending = Vec::new();
        for mut pending in pending_saves.drain(..) {
            if persistence::flush_connection(pool, &mut pending.conn, "logout retry").await {
                tracing::info!(
                    char_id = pending.conn.char_id,
                    "queued logout save landed; relogin open"
                );
            } else if now.duration_since(pending.since) >= PENDING_SAVE_GIVE_UP {
                tracing::error!(
                    char_id = pending.conn.char_id,
                    account_id = pending.conn.account_id,
                    "logout save given up after retrying; the character keeps its last saved state and may log in"
                );
            } else {
                still_pending.push(pending);
            }
        }
        *pending_saves = still_pending;
    }

    // 7-ter. The world clock. Everyone in the world is told the hour on a
    //    slow cadence; clients run their own clock at the same rate in
    //    between, so this only corrects drift. Joiners are seeded at
    //    EnterWorld. Reliable channel: a lost update would leave one
    //    client's sky wrong for a whole interval.
    if now.duration_since(*last_time_broadcast) >= clock::BROADCAST_INTERVAL {
        handlers::fan_out_time_of_day(&mut *server, in_world_ids, clock::current_hour());
        *last_time_broadcast = now;
    }

    // 7-bis. Ban sweep. Login, session redemption and world connect all
    //    refuse a banned account, but none of them touches a character
    //    that is already in the world, so a ban used to need a server
    //    restart (and its 60 s of lost progress for everyone) to remove
    //    a player who was online. Every BAN_SWEEP_INTERVAL, ask which
    //    accounts are banned and kick any connection that belongs to one.
    //
    //    The query runs on its own task and answers over a channel: this
    //    loop is the whole simulation, and a read that waited on a busy
    //    database or an exhausted pool would freeze every player six
    //    times a minute for a result that is almost always empty. A
    //    failed query reports nothing banned; the other three gates still
    //    hold and the next sweep retries.
    if now.duration_since(*last_ban_sweep) >= BAN_SWEEP_INTERVAL {
        *last_ban_sweep = now;
        if !connections.is_empty() && !*ban_sweep_in_flight {
            *ban_sweep_in_flight = true;
            let (sweep_pool, tx) = (pool.clone(), ban_tx.clone());
            tokio::spawn(async move {
                let banned = match db::banned_account_ids(&sweep_pool).await {
                    Ok(ids) => ids,
                    Err(e) => {
                        tracing::warn!(error = %e, "ban sweep query failed — will retry");
                        Vec::new()
                    }
                };
                let _ = tx.send(banned);
            });
        }
    }
    //    The item ledger's sweep (world/ledger.rs): the fold-and-prune of
    //    rows past the retention, then the conservation check, once at
    //    boot and every LEDGER_SWEEP_INTERVAL, on its own task like the
    //    ban sweep so the loop never waits on the database for it.
    if last_ledger_sweep.is_none_or(|t| now.duration_since(t) >= ledger::LEDGER_SWEEP_INTERVAL) {
        *last_ledger_sweep = Some(now);
        let sweep_pool = pool.clone();
        tokio::spawn(async move {
            ledger::sweep(&sweep_pool).await;
        });
    }
    while let Ok(banned) = ban_rx.try_recv() {
        *ban_sweep_in_flight = false;
        for (client_id, conn) in connections.iter_mut() {
            if conn.kicked_at.is_none()
                && conn.linkdead_since.is_none()
                && banned.contains(&conn.account_id)
            {
                tracing::info!(
                    account_id = conn.account_id,
                    char_id = conn.char_id,
                    "banned account kicked from the world"
                );
                handlers::send_kick(
                    &mut *server,
                    *client_id,
                    KickCode::BannedNow,
                    "This account is banned.",
                );
                // Stamped at the send, not at tick start: the grace before
                // the transport is dropped (step 4, KICK_FLUSH_GRACE) is
                // for THIS message to arrive, so it runs from here. The
                // connection's messages are ignored from now on
                // (`handle_message`).
                conn.kicked_at = Some(Instant::now());
            }
        }
    }
}
