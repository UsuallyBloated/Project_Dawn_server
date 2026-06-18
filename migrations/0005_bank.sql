-- Banker NPC, slice 1 (coins). A per-character bank wallet: four tiers,
-- held at the Banker NPC, zero weight on the player. This is the relief
-- valve for the four-tier coin-weight system (deposit heavy copper, carry
-- nothing). Mirrors the wallet columns added in 0004; defaults to empty.
--
-- Item storage (the per-character item vault + the 2 account-shared slots)
-- is slice 2 and lands in a later migration with its own tables.

ALTER TABLE characters ADD COLUMN bank_platinum INTEGER NOT NULL DEFAULT 0;
ALTER TABLE characters ADD COLUMN bank_gold     INTEGER NOT NULL DEFAULT 0;
ALTER TABLE characters ADD COLUMN bank_silver   INTEGER NOT NULL DEFAULT 0;
ALTER TABLE characters ADD COLUMN bank_copper   INTEGER NOT NULL DEFAULT 0;
