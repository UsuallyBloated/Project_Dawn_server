-- Project Dawn — Track 13.1 — server-side inventory.
--
-- Flat row-per-stack keyed by (char, location, slot). `location` is one
-- of:
--   'base'      — base inventory slot, slot ∈ [0, BASE_SLOT_COUNT-1]
--   'bag_<i>'   — contents of the bag held in base slot i (Track 13.2)
--   'equip'     — paperdoll, slot = numeric equip slot id (Track 13.3)
--
-- Track 13.1 only writes 'base' rows. The wider location semantics
-- live in app code; the table is intentionally loose so future
-- locations don't need another migration.

CREATE TABLE IF NOT EXISTS character_items (
    char_id      INTEGER NOT NULL REFERENCES characters(id) ON DELETE CASCADE,
    location     TEXT    NOT NULL,
    slot         INTEGER NOT NULL,
    item_path    TEXT    NOT NULL,
    count        INTEGER NOT NULL DEFAULT 1,
    PRIMARY KEY (char_id, location, slot)
);

CREATE INDEX IF NOT EXISTS idx_char_items_char ON character_items(char_id);
