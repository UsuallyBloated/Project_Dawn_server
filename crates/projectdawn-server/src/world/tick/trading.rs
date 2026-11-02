//! The trade window (steps 4hd2 and 4hd3): requests, offers, accepts and the
//! one-transaction commit, then the sweep that closes sessions on range or liveness.

use super::*;

/// Project one trade side's offers into the wire payload: the item and count
/// snapshotted at offer time, which is exactly what the commit verifies
/// against the live slot and hands over (escrow-by-locking: the session
/// stores references and a snapshot, never the item itself).
pub(super) fn trade_side_payload(
    side: &crate::world::trade::TradeSide,
) -> (Vec<(String, u32)>, protocol::world::Coins) {
    let mut slots = vec![(String::new(), 0u32); crate::world::trade::TRADE_SLOTS];
    for (i, offer) in side.offers.iter().enumerate() {
        if let Some(o) = offer {
            slots[i] = (o.item_path.clone(), o.count);
        }
    }
    (slots, side.coins)
}

/// Fan BOTH parties the full state of BOTH sides plus the accept flags —
/// after any edit. Full-state per update, not deltas: a tiny payload with
/// no desync class.
pub(super) fn fan_trade_state(
    server: &mut RenetServer,
    session: &crate::world::trade::TradeSession,
) {
    for (me, them) in [(&session.a, &session.b), (&session.b, &session.a)] {
        let (my_slots, my_coins) = trade_side_payload(me);
        let (their_slots, their_coins) = trade_side_payload(them);
        handlers::send_trade_offer_update(server, me.cid, true, my_slots, my_coins);
        handlers::send_trade_offer_update(server, me.cid, false, their_slots, their_coins);
        handlers::send_trade_accept_state(server, me.cid, me.accepted, them.accepted);
    }
}

/// Close the session covering `cid` (if any) and tell both parties why.
/// Every non-commit exit routes here: cancel, range, death, disconnect,
/// commit failure. Unlocking is implicit — locks derive from live sessions.
pub(super) fn close_trade(
    server: &mut RenetServer,
    trade_manager: &mut crate::world::trade::TradeManager,
    cid: ClientId,
    reason: &str,
) {
    if let Some(session) = trade_manager.close(cid) {
        for c in [session.a.cid, session.b.cid] {
            handlers::send_trade_closed(server, c, false, reason);
        }
        tracing::info!(
            a = session.a.cid as u64,
            b = session.b.cid as u64,
            reason,
            "trade closed"
        );
    }
}

/// The escrow: a slot offered in an open trade refuses every other
/// inventory intent. Reports the refusal (the silent-refusals doctrine);
/// callers just `continue` on `true`.
pub(super) fn trade_locked(
    server: &mut RenetServer,
    trade_manager: &crate::world::trade::TradeManager,
    cid: ClientId,
    location: &str,
    slot: u32,
) -> bool {
    if trade_manager.is_locked(cid, location, slot) {
        handlers::send_refusal(server, cid, "That item is offered in a trade.");
        return true;
    }
    false
}

pub(super) async fn apply_trade_intents(world: &mut WorldState, q: &mut Intents, _now: Instant) {
    let WorldState { pool, server, connections, trade_manager, .. } = world;
    let Intents { trade_intents, .. } = q;
    // 4hd2. PD_W0028 — apply trade intents (docs/design/trade_window.md).
    //       Escrow by locking: offers are references into the owner's
    //       inventory, locks derive from live sessions, and the commit
    //       is ONE db transaction spanning both characters. Refusals
    //       answer in chat except arms only a forged client can reach.
    if !trade_intents.is_empty() {
        for intent in trade_intents.drain(..) {
            match intent {
                TradeI::Request { requester, target } => {
                    let rcid = requester as ClientId;
                    // Ledger 10: players only. Enemy / bag / pet ids live
                    // in the reserved partitions; NPCs are not entities.
                    if IdKind::of(target) != IdKind::Player {
                        handlers::send_refusal(
                            &mut *server,
                            rcid,
                            "You can only trade with players.",
                        );
                        continue;
                    }
                    let tcid = target as ClientId;
                    let target_state = connections.get(&tcid).and_then(|t| {
                        if t.in_world && !t.death_processed && t.hp > 0.0 {
                            Some((t.pos, t.name.clone()))
                        } else {
                            None
                        }
                    });
                    // Absent, dead and too far share ONE line, so a request
                    // by id cannot tell from anywhere whether a character
                    // is online (the inspect and cast refusals' rule).
                    // Written keep-only-when-in-reach, so a non-finite
                    // distance refuses.
                    let in_reach = target_state.filter(|(tpos, _)| {
                        connections
                            .get(&rcid)
                            .is_some_and(|r| r.pos.distance_to(*tpos) <= crate::world::trade::TRADE_RANGE)
                    });
                    let Some((_, tname)) = in_reach else {
                        handlers::send_refusal(&mut *server, rcid, "There is no one here to trade with.");
                        continue;
                    };
                    let rname = connections
                        .get(&rcid)
                        .map(|c| c.name.clone())
                        .unwrap_or_default();
                    match trade_manager.open(rcid, tcid) {
                        Ok(()) => {
                            // Instant open, both sides (decided 2026-09-19).
                            handlers::send_trade_opened(&mut *server, rcid, target, tname);
                            handlers::send_trade_opened(&mut *server, tcid, requester, rname);
                            tracing::info!(a = requester, b = target, "trade opened");
                        }
                        // Ledger 8: busy / self refusals answer the
                        // SENDER only — quiet to the target, so a spammy
                        // requester cannot use refusal echoes to harass.
                        Err(e) => handlers::send_refusal(&mut *server, rcid, e),
                    }
                }
                TradeI::OfferItem { owner, window_slot, from_location, from_slot } => {
                    let cid = owner as ClientId;
                    if trade_manager.session_of(cid).is_none() {
                        handlers::send_refusal(&mut *server, cid, "You are not trading.");
                        continue;
                    }
                    // Ledger 9: the server re-reads everything from ITS
                    // inventory. A held bag's inners (bag_255) are never
                    // addressable, mirroring the inventory entry points.
                    if from_location == format!("bag_{}", inventory::CURSOR_BAG_KEY) {
                        continue; // forged-only
                    }
                    // Resolve the source. "cursor" parks the held stack
                    // into an empty inventory slot first (no merging, so
                    // the offer stays one discrete referenced stack) and
                    // frees the hand for the next pickup.
                    let (loc, slot) = if from_location == "cursor" {
                        match connections
                            .get_mut(&cid)
                            .map(|c| c.inventory.park_cursor_stack())
                        {
                            Some(Ok(((l, s), touched))) => {
                                if let Some(conn) = connections.get_mut(&cid) {
                                    conn.inventory_dirty = true;
                                    let deltas: Vec<(String, u32, Option<(String, u32)>)> =
                                        touched
                                            .iter()
                                            .map(|(tl, ts)| {
                                                (tl.clone(), *ts, conn.inventory.peek_at(tl, *ts))
                                            })
                                            .collect();
                                    for (tl, ts, payload) in deltas {
                                        handlers::send_slot_delta(&mut *server, cid, tl, ts, payload);
                                    }
                                }
                                (l, s)
                            }
                            Some(Err(e)) => {
                                handlers::send_refusal(&mut *server, cid, e);
                                continue;
                            }
                            None => continue,
                        }
                    } else {
                        (from_location, from_slot)
                    };
                    // The referenced slot must hold something, and a
                    // bag-typed offer must be an EMPTY bag (a full bag's
                    // contents transfer is its own later slice).
                    let peek = connections
                        .get(&cid)
                        .and_then(|c| c.inventory.peek_at(&loc, slot));
                    let Some((path, offer_count)) = peek else {
                        handlers::send_refusal(
                            &mut *server,
                            cid,
                            "There's nothing in that slot to offer.",
                        );
                        continue;
                    };
                    if items::bag_num_slots(&path).is_some() {
                        // A bag can only rest in a base slot, so its key
                        // is its base index.
                        let nonempty = loc == "base"
                            && connections
                                .get(&cid)
                                .map(|c| c.inventory.bag_key_is_nonempty(slot as u8))
                                .unwrap_or(false);
                        if nonempty {
                            handlers::send_refusal(
                                &mut *server,
                                cid,
                                "Empty the bag before trading it.",
                            );
                            continue;
                        }
                    }
                    match trade_manager.offer_item(
                        cid,
                        window_slot as usize,
                        crate::world::trade::Offer {
                            location: loc,
                            slot,
                            item_path: path,
                            count: offer_count,
                        },
                    ) {
                        Ok(()) => {
                            if let Some(session) = trade_manager.session_of(cid) {
                                fan_trade_state(&mut *server, session);
                            }
                        }
                        Err(e) => handlers::send_refusal(&mut *server, cid, e),
                    }
                }
                TradeI::RetrieveItem { owner, window_slot } => {
                    let cid = owner as ClientId;
                    match trade_manager.retrieve_item(cid, window_slot as usize) {
                        Ok(()) => {
                            if let Some(session) = trade_manager.session_of(cid) {
                                fan_trade_state(&mut *server, session);
                            }
                        }
                        Err(e) => handlers::send_refusal(&mut *server, cid, e),
                    }
                }
                TradeI::OfferCoins { owner, coins: amount } => {
                    let cid = owner as ClientId;
                    // A forged negative would steal from the partner at
                    // commit; `CoinAmount` cannot hold one (refused at the
                    // message, handlers::client_coin_amount).
                    let coins = amount.get();
                    // Ledger 4: absolute per-tier amounts, each within
                    // the live wallet (the four stacks are independent
                    // by design — no auto-breaking in a trade).
                    let covered = connections
                        .get(&cid)
                        .map(|c| {
                            coins.platinum <= c.coins.platinum
                                && coins.gold <= c.coins.gold
                                && coins.silver <= c.coins.silver
                                && coins.copper <= c.coins.copper
                        })
                        .unwrap_or(false);
                    if !covered {
                        handlers::send_refusal(&mut *server, cid, "You don't have that much coin.");
                        // Re-fan the APPLIED state so the client's coin
                        // fields snap back to what the server holds
                        // instead of displaying the refused amount.
                        if let Some(session) = trade_manager.session_of(cid) {
                            fan_trade_state(&mut *server, session);
                        }
                        continue;
                    }
                    match trade_manager.offer_coins(cid, amount) {
                        Ok(()) => {
                            if let Some(session) = trade_manager.session_of(cid) {
                                fan_trade_state(&mut *server, session);
                            }
                        }
                        Err(e) => handlers::send_refusal(&mut *server, cid, e),
                    }
                }
                TradeI::Cancel { owner } => {
                    close_trade(
                        &mut *server,
                        &mut *trade_manager,
                        owner as ClientId,
                        "Trade cancelled.",
                    );
                }
                TradeI::Accept { owner } => {
                    let cid = owner as ClientId;
                    let both = match trade_manager.accept(cid) {
                        Err(e) => {
                            handlers::send_refusal(&mut *server, cid, e);
                            continue;
                        }
                        Ok(b) => b,
                    };
                    if !both {
                        if let Some(session) = trade_manager.session_of(cid) {
                            fan_trade_state(&mut *server, session);
                        }
                        continue;
                    }
                    // ── COMMIT. Everything below runs inside this tick,
                    // so nothing interleaves between validation and
                    // application (ledger 4's argument). ──
                    let Some(session) = trade_manager.session_of(cid).cloned() else {
                        continue;
                    };
                    let (acid, bcid) = (session.a.cid, session.b.cid);
                    // Re-validate liveness + range (ledger 7).
                    let both_ok = match (connections.get(&acid), connections.get(&bcid)) {
                        (Some(a), Some(b)) => {
                            a.in_world
                                && b.in_world
                                && !a.death_processed
                                && !b.death_processed
                                && a.hp > 0.0
                                && b.hp > 0.0
                                && a.pos.distance_to(b.pos) <= crate::world::trade::TRADE_RANGE
                        }
                        _ => false,
                    };
                    if !both_ok {
                        close_trade(
                            &mut *server,
                            &mut *trade_manager,
                            acid,
                            "Trade failed — you are no longer together.",
                        );
                        continue;
                    }
                    // Re-validate coin coverage per tier (ledger 4 —
                    // wallets can shrink between offer and commit via a
                    // vendor purchase; items cannot move, they are locked).
                    let coins_covered = |cid: ClientId, offered: protocol::world::Coins| {
                        connections
                            .get(&cid)
                            .map(|c| {
                                offered.platinum <= c.coins.platinum
                                    && offered.gold <= c.coins.gold
                                    && offered.silver <= c.coins.silver
                                    && offered.copper <= c.coins.copper
                            })
                            .unwrap_or(false)
                    };
                    if !coins_covered(acid, session.a.coins)
                        || !coins_covered(bcid, session.b.coins)
                    {
                        trade_manager.clear_accepts_of(acid);
                        for c in [acid, bcid] {
                            handlers::send_refusal(
                                &mut *server,
                                c,
                                "The coin offer is no longer covered.",
                            );
                        }
                        if let Some(s) = trade_manager.session_of(acid) {
                            fan_trade_state(&mut *server, s);
                        }
                        continue;
                    }
                    // Snapshot verification: every offered slot must still
                    // hold exactly the item and count the partner was
                    // shown. Locks stop a slot (and the bag around it)
                    // moving, but the placer (loot, purchases, grants,
                    // withdrawals) ignores locks and can top up an offered
                    // stack or drop something into an offered bag, and the
                    // item check makes any other drift fail shut instead
                    // of handing over something else. A changed offer
                    // leaves the window, which also unlocks it, so the
                    // owner sees what happened and can offer it again;
                    // leaving it in would refuse every later accept with
                    // nothing visibly wrong.
                    let changed = |cid: ClientId, side: &crate::world::trade::TradeSide| -> Vec<usize> {
                        let Some(conn) = connections.get(&cid) else {
                            return (0..crate::world::trade::TRADE_SLOTS).collect();
                        };
                        side.offers
                            .iter()
                            .enumerate()
                            .filter_map(|(i, offer)| {
                                let o = offer.as_ref()?;
                                let same = conn
                                    .inventory
                                    .peek_at(&o.location, o.slot)
                                    .map(|(p, c)| p == o.item_path && c == o.count)
                                    .unwrap_or(false);
                                let bag_still_empty = o.location != "base"
                                    || !conn.inventory.bag_key_is_nonempty(o.slot as u8);
                                (!(same && bag_still_empty)).then_some(i)
                            })
                            .collect()
                    };
                    let a_changed = changed(acid, &session.a);
                    let b_changed = changed(bcid, &session.b);
                    if !a_changed.is_empty() || !b_changed.is_empty() {
                        for (owner, slots) in [(acid, &a_changed), (bcid, &b_changed)] {
                            for &i in slots {
                                // An empty window slot just reports empty.
                                let _ = trade_manager.retrieve_item(owner, i);
                            }
                        }
                        trade_manager.clear_accepts_of(acid);
                        for (c, mine) in [(acid, !a_changed.is_empty()), (bcid, !b_changed.is_empty())] {
                            let text = if mine {
                                "An item you offered changed, so it was taken out of the trade."
                            } else {
                                "Their offer changed. Check the window before accepting again."
                            };
                            handlers::send_refusal(&mut *server, c, text);
                        }
                        if let Some(s) = trade_manager.session_of(acid) {
                            fan_trade_state(&mut *server, s);
                        }
                        continue;
                    }
                    // Resolve every offered reference to (loc, slot,
                    // path, count). Locked slots cannot have changed and
                    // counts just verified, so a None here is internal
                    // inconsistency — fail shut.
                    let resolve = |cid: ClientId, side: &crate::world::trade::TradeSide| {
                        let conn = connections.get(&cid)?;
                        side.offered_slots()
                            .map(|o| {
                                conn.inventory
                                    .peek_at(&o.location, o.slot)
                                    .map(|(p, c)| (o.location.clone(), o.slot, p, c))
                            })
                            .collect::<Option<Vec<(String, u32, String, u32)>>>()
                    };
                    let (Some(a_gives), Some(b_gives)) =
                        (resolve(acid, &session.a), resolve(bcid, &session.b))
                    else {
                        close_trade(&mut *server, &mut *trade_manager, acid, "Trade failed.");
                        continue;
                    };
                    // Capacity (ledger 6): simulate each receiver on a
                    // CLONE — outgoing removed first, then the partner's
                    // stacks through the real placer (merge_capped
                    // semantics, ledger 5). Refuse cleanly, window open.
                    let simulate = |cid: ClientId,
                                    gives: &[(String, u32, String, u32)],
                                    receives: &[(String, u32, String, u32)]|
                     -> bool {
                        let Some(conn) = connections.get(&cid) else {
                            return false;
                        };
                        let mut sim = conn.inventory.clone();
                        for (l, s, _, _) in gives {
                            if sim.destroy_at(l, *s, 0).is_err() {
                                return false;
                            }
                        }
                        for (_, _, path, count) in receives {
                            match sim.add_item_locating(path, *count) {
                                Ok((_, leftover)) if leftover == 0 => {}
                                _ => return false,
                            }
                        }
                        true
                    };
                    let a_fits = simulate(acid, &a_gives, &b_gives);
                    let b_fits = simulate(bcid, &b_gives, &a_gives);
                    if !a_fits || !b_fits {
                        trade_manager.clear_accepts_of(acid);
                        let full = if a_fits { bcid } else { acid };
                        for c in [acid, bcid] {
                            let text = if c == full {
                                "Your bags cannot hold their offer."
                            } else {
                                "Their bags cannot hold your offer."
                            };
                            handlers::send_refusal(&mut *server, c, text);
                        }
                        if let Some(s) = trade_manager.session_of(acid) {
                            fan_trade_state(&mut *server, s);
                        }
                        continue;
                    }
                    // Flush both traders' other dirty stores FIRST.
                    // commit_trade writes the two inventories and
                    // wallets but not the vaults, so an unsaved bank
                    // withdrawal would ride the trade into the DB while
                    // its vault row still held the item: a crash then
                    // leaves it in two places. A failed flush closes the
                    // trade with nothing moved.
                    let mut flushed = true;
                    for cid in [acid, bcid] {
                        if let Some(c) = connections.get_mut(&cid) {
                            if let Err(e) = persistence::persist_dirty_stores(&pool, c).await {
                                tracing::error!(trader = cid, error = %e, "pre-trade store flush failed; trade closed");
                                flushed = false;
                                break;
                            }
                        }
                    }
                    if !flushed {
                        close_trade(&mut *server, &mut *trade_manager, acid, "Trade failed.");
                        continue;
                    }
                    // Apply for real, with full revert clones (the db
                    // write below can still fail; the in-memory swap
                    // must be reversible until it commits).
                    let revert = |connections: &HashMap<ClientId, PerConnection>,
                                  cid: ClientId| {
                        connections.get(&cid).map(|c| (c.inventory.clone(), c.coins))
                    };
                    let (Some(a_before), Some(b_before)) =
                        (revert(&connections, acid), revert(&connections, bcid))
                    else {
                        close_trade(&mut *server, &mut *trade_manager, acid, "Trade failed.");
                        continue;
                    };
                    // The ledger queues roll back with the snapshots if
                    // the commit fails.
                    let a_mark = connections.get(&acid).map(|c| c.ledger_mark()).unwrap_or(0);
                    let b_mark = connections.get(&bcid).map(|c| c.ledger_mark()).unwrap_or(0);
                    let mut touched_by: HashMap<ClientId, Vec<(String, u32)>> =
                        HashMap::new();
                    let mut apply_side = |cid: ClientId,
                                          partner: ClientId,
                                          gives: &[(String, u32, String, u32)],
                                          receives: &[(String, u32, String, u32)],
                                          pay: protocol::world::Coins,
                                          gain: protocol::world::Coins,
                                          connections: &mut HashMap<ClientId, PerConnection>|
                     -> bool {
                        let Some(conn) = connections.get_mut(&cid) else {
                            return false;
                        };
                        let touched = touched_by.entry(cid).or_default();
                        for (l, s, _, _) in gives {
                            if conn.inventory.destroy_at(l, *s, 0).is_err() {
                                return false;
                            }
                            touched.push((l.clone(), *s));
                        }
                        for (_, _, path, count) in receives {
                            match conn.inventory.add_item_locating(path, *count) {
                                Ok((placed, 0)) => touched.extend(placed),
                                _ => return false,
                            }
                        }
                        conn.coins.platinum -= pay.platinum;
                        conn.coins.gold -= pay.gold;
                        conn.coins.silver -= pay.silver;
                        conn.coins.copper -= pay.copper;
                        conn.coins.platinum += gain.platinum;
                        conn.coins.gold += gain.gold;
                        conn.coins.silver += gain.silver;
                        conn.coins.copper += gain.copper;
                        conn.inventory_dirty = true;
                        // The ledger: one row per stack each way and one
                        // for the coin, written with the commit below.
                        let detail = Some(format!("char:{partner}"));
                        for (_, _, path, count) in gives {
                            conn.ledger(ledger::item_event(
                                ItemAction::TradeOut,
                                path,
                                *count,
                                -i64::from(*count),
                                detail.clone(),
                            ));
                        }
                        for (_, _, path, count) in receives {
                            conn.ledger(ledger::item_event(
                                ItemAction::TradeIn,
                                path,
                                *count,
                                i64::from(*count),
                                detail.clone(),
                            ));
                        }
                        let net = gain.total_copper() - pay.total_copper();
                        if net != 0 {
                            conn.ledger(ledger::coin_event(ItemAction::TradeCoins, net, detail));
                        }
                        true
                    };
                    let applied = apply_side(
                        acid,
                        bcid,
                        &a_gives,
                        &b_gives,
                        session.a.coins,
                        session.b.coins,
                        &mut *connections,
                    ) && apply_side(
                        bcid,
                        acid,
                        &b_gives,
                        &a_gives,
                        session.b.coins,
                        session.a.coins,
                        &mut *connections,
                    );
                    let persist_ok = if applied {
                        let rows_a = connections
                            .get(&acid)
                            .map(|c| (c.inventory.to_rows(), c.coins, c.item_events.clone()));
                        let rows_b = connections
                            .get(&bcid)
                            .map(|c| (c.inventory.to_rows(), c.coins, c.item_events.clone()));
                        match (rows_a, rows_b) {
                            (Some((ra, ca, ea)), Some((rb, cb, eb))) => {
                                match db::commit_trade(
                                    &pool,
                                    acid as i64,
                                    &ra,
                                    ca,
                                    &ea,
                                    bcid as i64,
                                    &rb,
                                    cb,
                                    &eb,
                                )
                                .await
                                {
                                    Ok(()) => true,
                                    Err(e) => {
                                        tracing::error!(
                                            a = acid as u64,
                                            b = bcid as u64,
                                            error = %e,
                                            "commit_trade persist failed"
                                        );
                                        false
                                    }
                                }
                            }
                            _ => false,
                        }
                    } else {
                        false
                    };
                    if !persist_ok {
                        // Revert BOTH sides to the pre-apply snapshots;
                        // nothing was sent yet, so there is nothing to
                        // un-send (the corpse-loot doctrine).
                        if let Some(c) = connections.get_mut(&acid) {
                            c.inventory = a_before.0.clone();
                            c.coins = a_before.1;
                            c.ledger_rollback(a_mark);
                        }
                        if let Some(c) = connections.get_mut(&bcid) {
                            c.inventory = b_before.0.clone();
                            c.coins = b_before.1;
                            c.ledger_rollback(b_mark);
                        }
                        close_trade(&mut *server, &mut *trade_manager, acid, "Trade failed.");
                        continue;
                    }
                    // Committed. Close the session, then fan the truth:
                    // deltas for every touched slot, wallets, and the
                    // committed close. The window itself granted nothing.
                    trade_manager.close(acid);
                    // commit_trade wrote both queues whole; they leave with
                    // the commit, as the store flush clears its own.
                    for c in [acid, bcid] {
                        if let Some(conn) = connections.get_mut(&c) {
                            conn.item_events.clear();
                        }
                    }
                    for c in [acid, bcid] {
                        if let Some(conn) = connections.get(&c) {
                            if let Some(touched) = touched_by.get(&c) {
                                for (l, s) in touched {
                                    handlers::send_slot_delta(&mut *server, c, l.clone(), *s, conn.inventory.peek_at(l, *s));
                                }
                            }
                            handlers::send_coins_update(&mut *server, c, conn.coins);
                        }
                        handlers::send_trade_closed(&mut *server, c, true, "Trade complete.");
                    }
                    tracing::info!(
                        a = acid as u64,
                        b = bcid as u64,
                        a_stacks = a_gives.len(),
                        b_stacks = b_gives.len(),
                        a_coins = session.a.coins.total_copper(),
                        b_coins = session.b.coins.total_copper(),
                        "trade committed"
                    );
                }
            }
        }
    }
}

pub(super) fn sweep_trades(world: &mut WorldState, _q: &mut Intents, _now: Instant) {
    let WorldState { server, connections, trade_manager, .. } = world;
    // 4hd3. PD_W0028 — trade session sweep: range, liveness, and
    //       presence, every tick (ledger 7). A session survives only
    //       while both parties are connected, in-world, alive, and
    //       within TRADE_RANGE of each other.
    for (acid, bcid) in trade_manager.open_session_cids() {
        let healthy = match (connections.get(&acid), connections.get(&bcid)) {
            (Some(a), Some(b)) => {
                a.in_world
                    && b.in_world
                    && !a.death_processed
                    && !b.death_processed
                    && a.hp > 0.0
                    && b.hp > 0.0
                    && a.pos.distance_to(b.pos) <= crate::world::trade::TRADE_RANGE
            }
            _ => false,
        };
        if !healthy {
            close_trade(
                &mut *server,
                &mut *trade_manager,
                acid,
                "Trade cancelled — you are no longer together.",
            );
        }
    }
}
