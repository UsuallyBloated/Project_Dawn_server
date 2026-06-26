-- Corpse / resurrection epic, Slice 3 — Cleric/Paladin resurrection.
-- A corpse now remembers how much XP that death cost (so a res can refund a
-- percentage of it) and whether it has already been resurrected (so it can't be
-- rezzed twice for free XP). Both default to 0 so existing corpses load cleanly.

ALTER TABLE corpses ADD COLUMN lost_xp     INTEGER NOT NULL DEFAULT 0;
ALTER TABLE corpses ADD COLUMN resurrected INTEGER NOT NULL DEFAULT 0;
