//! PD_W0028 — player-to-player trading, slice 1 (docs/design/trade_window.md).
//!
//! Escrow by LOCKING, not by moving: an offered item never leaves its
//! owner's inventory. A session records `(location, slot)` REFERENCES, and
//! every other inventory intent that touches a locked slot is refused while
//! the trade is open (`is_locked`, checked in tick.rs). There is therefore
//! no moment where an item exists only inside a session object — a server
//! crash mid-trade loses the trade, never an item.
//!
//! This module is deliberately dumb data + reference bookkeeping so it unit
//! tests without a world: everything that needs the real inventory (peeks,
//! bag-empty checks, capacity, the commit itself) lives in tick.rs next to
//! the state it reads. The single most important rule lives here though:
//! EVERY offer edit clears BOTH accepts (the anti-bait-and-switch law).

use protocol::world::Coins;
use renet::ClientId;
use std::collections::HashMap;

/// Window slots per side (decided 2026-09-19).
pub const TRADE_SLOTS: usize = 8;
/// Session range in meters, checked every tick (decided 2026-09-19; the
/// user flags 10 m as far — tuning candidate after the first playtest,
/// probably downward toward the 6 m UI-interact feel).
pub const TRADE_RANGE: f32 = 10.0;

#[derive(Debug, Clone)]
pub struct TradeSide {
    pub cid: ClientId,
    /// Offered stacks as `(location, slot, count)` references into the
    /// OWNER's inventory. The count is snapshotted AT OFFER TIME and
    /// re-verified at commit: locks stop the slot being moved/consumed,
    /// but external growth (loot or purchase top-ups merging into the
    /// same stack) would otherwise silently raise what changes hands
    /// after both accepts — a bait-and-switch nobody clicked.
    pub offers: [Option<(String, u32, u32)>; TRADE_SLOTS],
    pub coins: Coins,
    pub accepted: bool,
}

impl TradeSide {
    fn new(cid: ClientId) -> Self {
        Self {
            cid,
            offers: Default::default(),
            coins: Coins::ZERO,
            accepted: false,
        }
    }

    pub fn offered_slots(&self) -> impl Iterator<Item = &(String, u32, u32)> {
        self.offers.iter().flatten()
    }
}

#[derive(Debug, Clone)]
pub struct TradeSession {
    pub a: TradeSide,
    pub b: TradeSide,
}

impl TradeSession {
    pub fn side_of(&self, cid: ClientId) -> Option<&TradeSide> {
        if self.a.cid == cid {
            Some(&self.a)
        } else if self.b.cid == cid {
            Some(&self.b)
        } else {
            None
        }
    }

    pub fn side_of_mut(&mut self, cid: ClientId) -> Option<&mut TradeSide> {
        if self.a.cid == cid {
            Some(&mut self.a)
        } else if self.b.cid == cid {
            Some(&mut self.b)
        } else {
            None
        }
    }

    /// The anti-bait-and-switch law: any edit to either side clears BOTH
    /// accept states. Every mutating method below routes through this.
    fn clear_accepts(&mut self) {
        self.a.accepted = false;
        self.b.accepted = false;
    }
}

/// All open sessions plus the one-session-per-player index. Owned by the
/// tick loop like `GroupManager`.
#[derive(Debug, Default)]
pub struct TradeManager {
    sessions: HashMap<u64, TradeSession>,
    by_cid: HashMap<ClientId, u64>,
    next_id: u64,
}

impl TradeManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// Open a session between two players (instant open — decided
    /// 2026-09-19, no accept prompt). Refuses self-trade and either party
    /// already being in a session (one session per player, which is also
    /// what makes "the same slot cannot enter a second session" free).
    pub fn open(&mut self, a: ClientId, b: ClientId) -> Result<(), &'static str> {
        if a == b {
            return Err("you cannot trade with yourself");
        }
        if self.by_cid.contains_key(&a) {
            return Err("you are already trading");
        }
        if self.by_cid.contains_key(&b) {
            return Err("they are busy");
        }
        let id = self.next_id;
        self.next_id += 1;
        self.sessions.insert(
            id,
            TradeSession {
                a: TradeSide::new(a),
                b: TradeSide::new(b),
            },
        );
        self.by_cid.insert(a, id);
        self.by_cid.insert(b, id);
        Ok(())
    }

    pub fn session_of(&self, cid: ClientId) -> Option<&TradeSession> {
        self.sessions.get(self.by_cid.get(&cid)?)
    }

    pub fn session_of_mut(&mut self, cid: ClientId) -> Option<&mut TradeSession> {
        self.sessions.get_mut(self.by_cid.get(&cid)?)
    }

    /// Record an offer reference (validation of the referenced item —
    /// existence, bag-emptiness, cursor parking — happens in tick.rs BEFORE
    /// this call). Refuses a `(location, slot)` that is already offered by
    /// this player in any window slot: one reference, one offer.
    pub fn offer_item(
        &mut self,
        cid: ClientId,
        window_slot: usize,
        location: String,
        slot: u32,
        count: u32,
    ) -> Result<(), &'static str> {
        let session = self
            .sessions
            .get_mut(self.by_cid.get(&cid).ok_or("you are not trading")?)
            .ok_or("you are not trading")?;
        if window_slot >= TRADE_SLOTS {
            return Err("no such trade slot");
        }
        let side = session.side_of_mut(cid).ok_or("you are not trading")?;
        if side
            .offered_slots()
            .any(|(l, s, _)| *l == location && *s == slot)
        {
            return Err("that item is already offered");
        }
        if side.offers[window_slot].is_some() {
            return Err("that trade slot is taken");
        }
        side.offers[window_slot] = Some((location, slot, count));
        session.clear_accepts();
        Ok(())
    }

    pub fn retrieve_item(
        &mut self,
        cid: ClientId,
        window_slot: usize,
    ) -> Result<(), &'static str> {
        let session = self
            .sessions
            .get_mut(self.by_cid.get(&cid).ok_or("you are not trading")?)
            .ok_or("you are not trading")?;
        if window_slot >= TRADE_SLOTS {
            return Err("no such trade slot");
        }
        let side = session.side_of_mut(cid).ok_or("you are not trading")?;
        if side.offers[window_slot].take().is_none() {
            return Err("that trade slot is empty");
        }
        session.clear_accepts();
        Ok(())
    }

    /// Absolute per-tier coin offer (wallet validation happens in tick.rs,
    /// at offer time AND again at commit).
    pub fn offer_coins(&mut self, cid: ClientId, coins: Coins) -> Result<(), &'static str> {
        let session = self
            .sessions
            .get_mut(self.by_cid.get(&cid).ok_or("you are not trading")?)
            .ok_or("you are not trading")?;
        let side = session.side_of_mut(cid).ok_or("you are not trading")?;
        side.coins = coins;
        session.clear_accepts();
        Ok(())
    }

    /// Press Trade. Returns `true` when BOTH sides now stand accepted —
    /// the caller runs the commit in the same tick.
    pub fn accept(&mut self, cid: ClientId) -> Result<bool, &'static str> {
        let session = self
            .sessions
            .get_mut(self.by_cid.get(&cid).ok_or("you are not trading")?)
            .ok_or("you are not trading")?;
        session
            .side_of_mut(cid)
            .ok_or("you are not trading")?
            .accepted = true;
        Ok(session.a.accepted && session.b.accepted)
    }

    /// Clear both accepts of the session covering `cid` — the commit's
    /// keep-the-window-open failure paths (capacity, coin coverage) use
    /// this so a failed commit never leaves a stale accept standing.
    pub fn clear_accepts_of(&mut self, cid: ClientId) {
        if let Some(s) = self.session_of_mut(cid) {
            s.a.accepted = false;
            s.b.accepted = false;
        }
    }

    /// Drop the session covering `cid` (cancel, commit-complete, range,
    /// death, disconnect — every exit path). Returns the session so the
    /// caller can notify both parties. Unlocking is implicit: locks are
    /// derived from live sessions, so a removed session holds nothing.
    pub fn close(&mut self, cid: ClientId) -> Option<TradeSession> {
        let id = self.by_cid.remove(&cid)?;
        let session = self.sessions.remove(&id)?;
        self.by_cid.remove(&session.a.cid);
        self.by_cid.remove(&session.b.cid);
        Some(session)
    }

    /// Is `(location, slot)` of `cid`'s inventory locked by an open trade?
    /// The lock set IS the offer set — no second copy to drift.
    pub fn is_locked(&self, cid: ClientId, location: &str, slot: u32) -> bool {
        self.session_of(cid)
            .and_then(|s| s.side_of(cid))
            .map(|side| {
                side.offered_slots()
                    .any(|(l, sl, _)| l == location && *sl == slot)
            })
            .unwrap_or(false)
    }

    /// Every cid with an open session — the tick sweep iterates this to
    /// enforce range / liveness without borrowing the map mutably.
    pub fn open_session_cids(&self) -> Vec<(ClientId, ClientId)> {
        self.sessions.values().map(|s| (s.a.cid, s.b.cid)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_refuses_self_and_busy() {
        let mut tm = TradeManager::new();
        assert_eq!(tm.open(1, 1).unwrap_err(), "you cannot trade with yourself");
        tm.open(1, 2).expect("first session opens");
        assert_eq!(tm.open(1, 3).unwrap_err(), "you are already trading");
        assert_eq!(tm.open(3, 2).unwrap_err(), "they are busy");
    }

    #[test]
    fn every_offer_edit_clears_both_accepts() {
        // The anti-bait-and-switch law, on all three edit paths.
        let mut tm = TradeManager::new();
        tm.open(1, 2).unwrap();
        let both_accepted_then = |tm: &mut TradeManager| {
            tm.accept(1).unwrap();
            assert!(tm.accept(2).unwrap(), "both stand accepted before the edit");
        };
        let assert_cleared = |tm: &TradeManager, edit: &str| {
            let s = tm.session_of(1).unwrap();
            assert!(
                !s.a.accepted && !s.b.accepted,
                "{edit} must clear BOTH accepts"
            );
        };
        both_accepted_then(&mut tm);
        tm.offer_item(1, 0, "base".into(), 0, 1).unwrap();
        assert_cleared(&tm, "offer_item");
        both_accepted_then(&mut tm);
        tm.offer_coins(2, Coins { platinum: 0, gold: 1, silver: 0, copper: 0 })
            .unwrap();
        assert_cleared(&tm, "offer_coins");
        both_accepted_then(&mut tm);
        tm.retrieve_item(1, 0).unwrap();
        assert_cleared(&tm, "retrieve_item");
    }

    #[test]
    fn same_inventory_slot_cannot_be_offered_twice() {
        let mut tm = TradeManager::new();
        tm.open(1, 2).unwrap();
        tm.offer_item(1, 0, "base".into(), 3, 5).unwrap();
        assert_eq!(
            tm.offer_item(1, 1, "base".into(), 3, 5).unwrap_err(),
            "that item is already offered"
        );
    }

    #[test]
    fn locks_are_derived_from_offers_and_die_with_the_session() {
        let mut tm = TradeManager::new();
        tm.open(1, 2).unwrap();
        tm.offer_item(1, 0, "bag_2".into(), 4, 2).unwrap();
        assert!(tm.is_locked(1, "bag_2", 4));
        assert!(!tm.is_locked(1, "bag_2", 5));
        assert!(!tm.is_locked(2, "bag_2", 4), "locks are per-owner");
        tm.retrieve_item(1, 0).unwrap();
        assert!(!tm.is_locked(1, "bag_2", 4), "retrieve unlocks");
        tm.offer_item(1, 0, "bag_2".into(), 4, 2).unwrap();
        let closed = tm.close(2).expect("close by either party");
        assert!(
            (closed.a.cid == 1 && closed.b.cid == 2)
                || (closed.a.cid == 2 && closed.b.cid == 1),
            "the closed session names both parties"
        );
        assert!(!tm.is_locked(1, "bag_2", 4), "a closed session holds nothing");
        assert!(tm.session_of(1).is_none() && tm.session_of(2).is_none());
    }

    #[test]
    fn both_accepts_signal_commit_exactly_once() {
        let mut tm = TradeManager::new();
        tm.open(1, 2).unwrap();
        assert!(!tm.accept(1).unwrap(), "one accept is not a commit");
        assert!(tm.accept(2).unwrap(), "the second accept completes the pair");
    }
}
