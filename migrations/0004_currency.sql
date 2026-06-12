-- Four-tier coin wallet: platinum / gold / silver / copper, at 100:1 ratios.
--
-- Replaces the single `coins` count. The legacy value carries forward into
-- copper (faithful: old `coins` was a flat count with no tiers). The old
-- `coins` column is left in place — SQLite can't cheaply drop a column and an
-- unused column is harmless — but nothing reads it after this migration.

ALTER TABLE characters ADD COLUMN platinum INTEGER NOT NULL DEFAULT 0;
ALTER TABLE characters ADD COLUMN gold     INTEGER NOT NULL DEFAULT 0;
ALTER TABLE characters ADD COLUMN silver   INTEGER NOT NULL DEFAULT 0;
ALTER TABLE characters ADD COLUMN copper   INTEGER NOT NULL DEFAULT 0;

UPDATE characters SET copper = coins;
