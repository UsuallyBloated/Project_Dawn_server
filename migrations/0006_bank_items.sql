-- Banker NPC slice 2 — item vaults. Two flat row-per-stack stores, like
-- character_items but with no `location` column (one store per table) and a
-- fixed slot range:
--   bank_items          per-character vault, 10 slots, char-keyed
--   account_bank_items  account-shared vault, 2 slots, account-keyed (EQ
--                       "shared bank" — items move between a player's own
--                       characters). Keyed on the auth account id, never char_id.

CREATE TABLE IF NOT EXISTS bank_items (
    char_id   INTEGER NOT NULL REFERENCES characters(id) ON DELETE CASCADE,
    slot      INTEGER NOT NULL,           -- [0, 9]
    item_path TEXT    NOT NULL,
    count     INTEGER NOT NULL DEFAULT 1,
    PRIMARY KEY (char_id, slot)
);
CREATE INDEX IF NOT EXISTS idx_bank_items_char ON bank_items(char_id);

CREATE TABLE IF NOT EXISTS account_bank_items (
    account_id INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    slot       INTEGER NOT NULL,          -- [0, 1]
    item_path  TEXT    NOT NULL,
    count      INTEGER NOT NULL DEFAULT 1,
    PRIMARY KEY (account_id, slot)
);
CREATE INDEX IF NOT EXISTS idx_account_bank_items_acct ON account_bank_items(account_id);
