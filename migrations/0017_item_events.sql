-- The item ledger (docs/design/item_log_research.md in the client repo, option
-- A, the user's four calls of 2026-10-30: build it, every server item
-- mutation, the operator reads it first, 30 days kept).
--
-- One row per movement of items or coin into or out of a character's own
-- holdings (inventory, cursor, worn gear, the two vaults): loot, a coin purse,
-- a drop, a pickup, a destroy, a meal, both sides of a trade, a purchase, a
-- sale, a vault deposit or withdrawal, a quest reward, a conjured item, a GM
-- grant, the death strip, the NO RENT purge. Rearrangements inside the
-- holdings (a move, a split, an equip) write nothing. A row is written by the
-- server INSIDE the transaction that persists the move it describes, never by
-- itself and never from anything the client sends, so the table and the
-- stores agree at every commit and a row is evidence.
--
--   delta   the signed change to the account's holdings of `item_path`
--           (+3 looted, -1 destroyed, 0 for a vault deposit: the item stayed
--           the character's)
--   count   the quantity the action involved, never negative
--   copper  the signed change to the account's coin, in copper
--   detail  the counterparty or the circumstance: the vendor, the trade
--           partner's character id, the bag or corpse id, the quest, the spell
--
-- The books balance by construction: per account and item, the sum of
-- `delta` over every row equals what the stores hold, and the sum of `copper`
-- equals the wallets. Every character's holdings at the moment this table is
-- created are entered below as one `opening` row per item, so the rule holds
-- from the first boot on; the daily sweep keeps it true while pruning, by
-- folding rows older than 30 days into one `carry` row per item (world/
-- ledger.rs). The conservation query (db::ledger_mismatches) names any account
-- and item where the two sums disagree: a dupe or a loss, with a date on it.
--
-- No foreign key on char_id, on purpose: an audit row outlives the character
-- it is about (gm_actions keeps its rows the same way).
--
-- Forward-only like every migration: a binary without it refuses to boot on a
-- database that has run it (sqlx VersionMissing).

CREATE TABLE IF NOT EXISTS item_events (
    id        INTEGER PRIMARY KEY AUTOINCREMENT,
    at        INTEGER NOT NULL,
    char_id   INTEGER NOT NULL,
    action    TEXT    NOT NULL,
    item_path TEXT,
    count     INTEGER NOT NULL DEFAULT 0,
    delta     INTEGER NOT NULL DEFAULT 0,
    copper    INTEGER NOT NULL DEFAULT 0,
    detail    TEXT
);

CREATE INDEX IF NOT EXISTS item_events_char_at ON item_events (char_id, at);
CREATE INDEX IF NOT EXISTS item_events_at ON item_events (at);

-- Opening balances: what every character holds as the table is created.
INSERT INTO item_events (at, char_id, action, item_path, count, delta, copper, detail)
SELECT CAST(strftime('%s', 'now') AS INTEGER), char_id, 'opening', item_path,
       SUM(count), SUM(count), 0, 'inventory'
FROM character_items
GROUP BY char_id, item_path;

INSERT INTO item_events (at, char_id, action, item_path, count, delta, copper, detail)
SELECT CAST(strftime('%s', 'now') AS INTEGER), char_id, 'opening', item_path,
       SUM(count), SUM(count), 0, 'vault'
FROM bank_items
GROUP BY char_id, item_path;

-- The account vault belongs to the account, not a character; its opening row
-- sits under the account's first character, and the books are kept per
-- account, so it does not matter which.
INSERT INTO item_events (at, char_id, action, item_path, count, delta, copper, detail)
SELECT CAST(strftime('%s', 'now') AS INTEGER),
       (SELECT MIN(c.id) FROM characters c WHERE c.account_id = a.account_id),
       'opening', a.item_path, SUM(a.count), SUM(a.count), 0, 'account vault'
FROM account_bank_items a
GROUP BY a.account_id, a.item_path
HAVING (SELECT MIN(c.id) FROM characters c WHERE c.account_id = a.account_id) IS NOT NULL;

INSERT INTO item_events (at, char_id, action, item_path, count, delta, copper, detail)
SELECT CAST(strftime('%s', 'now') AS INTEGER), id, 'opening', NULL, 0, 0,
       platinum * 1000000 + gold * 10000 + silver * 100 + copper
         + bank_platinum * 1000000 + bank_gold * 10000 + bank_silver * 100 + bank_copper,
       'wallet and bank'
FROM characters
WHERE platinum * 1000000 + gold * 10000 + silver * 100 + copper
        + bank_platinum * 1000000 + bank_gold * 10000 + bank_silver * 100 + bank_copper != 0;
