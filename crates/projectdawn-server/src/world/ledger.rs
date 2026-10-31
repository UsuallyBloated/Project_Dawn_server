//! The item ledger: one row per server-made movement of items or coin into or
//! out of a character's holdings, kept in `item_events` (migration 0017). The
//! design and the user's four calls: `docs/design/item_log_research.md` in the
//! client repo, option A, 2026-10-30.
//!
//! How a row gets written. The handler that moves an item queues the row on
//! the connection (`PerConnection::ledger`) at the moment it makes the move in
//! memory, beside the dirty flag it sets. The queue is written INSIDE whichever
//! transaction next persists that character's stores (the store flush, the
//! trade commit, the corpse loot, the death strip: `db::write_item_events`) and
//! cleared only once that commit lands, so the table and the stores agree at
//! every commit: a crash loses the row and the move together, never one alone.
//! A handler that reverts an in-memory move on a failed persist rolls the queue
//! back to the mark it took first (`PerConnection::ledger_mark` /
//! `ledger_rollback`). Nothing the client sends is ever a row.
//!
//! What a row means (the columns are described in the migration): `delta` is
//! the signed change to the ACCOUNT's holdings of the item, so a vault deposit
//! is a row with `delta` 0 (the item stayed the character's) and `copper` the
//! same for coin. The books balance per account: the sum of `delta` over an
//! account's rows for an item equals the item's count across its inventories
//! and vaults, and the sum of `copper` equals its wallets. The conservation
//! query (`db::ledger_mismatches`) names every account and item where the two
//! differ: a dupe or a loss, dated. Rearrangements inside the holdings (a move,
//! a split, an equip) write nothing.
//!
//! Retention. The daily sweep folds every row older than `LEDGER_RETENTION_SECS`
//! into one `carry` row per character and item holding the folded sums, then
//! deletes the old rows, so the sums (and the check) survive the prune. Then it
//! runs the check and logs each mismatch at warn. It runs on its own task, as
//! the ban sweep does, so the world loop never waits on the database for it.

use crate::db::{self, ItemEventRow};
use sqlx::SqlitePool;
use std::time::Duration;

/// How long a row is kept before the sweep folds it into a carry row.
pub const LEDGER_RETENTION_SECS: i64 = 30 * 24 * 60 * 60;
/// How often the world loop runs the sweep (and once at boot).
pub const LEDGER_SWEEP_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

/// What happened to the item or the coin. The name is the `action` column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemAction {
    /// Looted from a creature's corpse (a loot bag with a creature name).
    Loot,
    /// Picked up from the ground (a loot bag a player dropped).
    Pickup,
    /// A share of a corpse's coin purse.
    LootCoins,
    /// Taken back off one's own corpse.
    CorpseLoot,
    /// One's own coin, back off one's own corpse.
    CorpseCoins,
    /// Dropped on the ground.
    Drop,
    /// Destroyed on purpose.
    Destroy,
    /// Eaten or drunk, by hand or by hunger.
    Eat,
    /// Any other consumable used.
    Use,
    /// Handed to a trade partner, who gets a `TradeIn`.
    TradeOut,
    TradeIn,
    /// The coin side of a trade, one row per side with the signed net.
    TradeCoins,
    /// Bought from a merchant (the item in, the coin out, one row).
    Buy,
    /// Sold to a merchant (the item out, the coin in, one row).
    Sell,
    /// Into a vault (`delta` 0: still the character's).
    BankIn,
    /// Out of a vault (`delta` 0).
    BankOut,
    /// Coin into the bank wallet (`copper` 0: still the character's).
    BankCoinIn,
    BankCoinOut,
    /// Coin changed between tiers at the banker (`copper` 0).
    BankExchange,
    /// A quest's item reward.
    QuestReward,
    /// Conjured by a spell.
    Summon,
    /// A GM's `/give` or the Test Panel.
    GmGive,
    /// A GM's coin grant.
    GmCoins,
    /// Stripped onto one's own corpse at death.
    Death,
    DeathCoins,
    /// A NO RENT item gone at a login after thirty minutes away.
    NoRent,
}

impl ItemAction {
    pub const fn as_str(self) -> &'static str {
        match self {
            ItemAction::Loot => "loot",
            ItemAction::Pickup => "pickup",
            ItemAction::LootCoins => "loot_coins",
            ItemAction::CorpseLoot => "corpse_loot",
            ItemAction::CorpseCoins => "corpse_coins",
            ItemAction::Drop => "drop",
            ItemAction::Destroy => "destroy",
            ItemAction::Eat => "eat",
            ItemAction::Use => "use",
            ItemAction::TradeOut => "trade_out",
            ItemAction::TradeIn => "trade_in",
            ItemAction::TradeCoins => "trade_coins",
            ItemAction::Buy => "buy",
            ItemAction::Sell => "sell",
            ItemAction::BankIn => "bank_in",
            ItemAction::BankOut => "bank_out",
            ItemAction::BankCoinIn => "bank_coin_in",
            ItemAction::BankCoinOut => "bank_coin_out",
            ItemAction::BankExchange => "bank_exchange",
            ItemAction::QuestReward => "quest_reward",
            ItemAction::Summon => "summon",
            ItemAction::GmGive => "gm_give",
            ItemAction::GmCoins => "gm_coins",
            ItemAction::Death => "death",
            ItemAction::DeathCoins => "death_coins",
            ItemAction::NoRent => "no_rent",
        }
    }
}

/// A row about an item: `delta` is the signed holdings change (0 for a move
/// into or out of a vault), `count` the quantity involved. `at` and `char_id`
/// are stamped by `PerConnection::ledger` when the row is queued.
pub fn item_event(
    action: ItemAction,
    item_path: &str,
    count: u32,
    delta: i64,
    detail: Option<String>,
) -> ItemEventRow {
    ItemEventRow {
        at: 0,
        char_id: 0,
        action: action.as_str(),
        item_path: Some(item_path.to_string()),
        count: i64::from(count),
        delta,
        copper: 0,
        detail,
    }
}

/// A row about coin alone: `copper` is the signed change to the account's
/// coin (0 for a bank deposit or an exchange, which keep it the character's).
pub fn coin_event(action: ItemAction, copper: i64, detail: Option<String>) -> ItemEventRow {
    ItemEventRow {
        at: 0,
        char_id: 0,
        action: action.as_str(),
        item_path: None,
        count: 0,
        delta: 0,
        copper,
        detail,
    }
}

/// Wall-clock unix seconds: the `at` of a row, and the NO RENT logout stamp.
pub fn unix_now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// The daily sweep: fold and prune, then the conservation check, each logged.
/// A failure of either half is logged and retried at the next sweep; nothing
/// in the world depends on it.
pub async fn sweep(pool: &SqlitePool) {
    let cutoff = unix_now_secs() - LEDGER_RETENTION_SECS;
    match db::fold_item_events_before(pool, cutoff).await {
        Ok((carried, deleted)) => {
            tracing::info!(cutoff, carried, deleted, "ledger swept");
        }
        Err(e) => tracing::warn!(error = %e, "ledger sweep failed; retried at the next sweep"),
    }
    match db::ledger_mismatches(pool).await {
        Ok(rows) => {
            for m in &rows {
                tracing::warn!(
                    account_id = m.account_id,
                    item = m.item_path.as_deref().unwrap_or("coin"),
                    ledger = m.ledger,
                    held = m.held,
                    "ledger mismatch"
                );
            }
            tracing::info!(mismatches = rows.len(), "ledger check");
        }
        Err(e) => tracing::warn!(error = %e, "ledger check failed; retried at the next sweep"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_item_row_carries_its_quantity_and_sign_separately() {
        let row = item_event(ItemAction::Destroy, "res://x.tres", 3, -3, None);
        assert_eq!(row.action, "destroy");
        assert_eq!((row.count, row.delta, row.copper), (3, -3, 0));
        let vault = item_event(ItemAction::BankIn, "res://x.tres", 3, 0, Some("vault".into()));
        assert_eq!((vault.count, vault.delta), (3, 0), "a vault deposit keeps the item the character's");
        let coin = coin_event(ItemAction::Sell, 250, None);
        assert_eq!((coin.item_path.as_deref(), coin.copper), (None, 250));
    }

    #[test]
    fn every_action_has_a_distinct_name() {
        let all = [
            ItemAction::Loot, ItemAction::Pickup, ItemAction::LootCoins, ItemAction::CorpseLoot,
            ItemAction::CorpseCoins, ItemAction::Drop, ItemAction::Destroy, ItemAction::Eat,
            ItemAction::Use, ItemAction::TradeOut, ItemAction::TradeIn, ItemAction::TradeCoins,
            ItemAction::Buy, ItemAction::Sell, ItemAction::BankIn, ItemAction::BankOut,
            ItemAction::BankCoinIn, ItemAction::BankCoinOut, ItemAction::BankExchange,
            ItemAction::QuestReward, ItemAction::Summon, ItemAction::GmGive, ItemAction::GmCoins,
            ItemAction::Death, ItemAction::DeathCoins, ItemAction::NoRent,
        ];
        let names: std::collections::HashSet<&str> = all.iter().map(|a| a.as_str()).collect();
        assert_eq!(names.len(), all.len());
        assert!(!names.contains("opening") && !names.contains("carry"), "those two are the database's own");
    }
}
