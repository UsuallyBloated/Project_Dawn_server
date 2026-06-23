-- Corpse / resurrection epic, Slice 1 — persisted player corpses. On death a
-- player's gear (equipped + bags) AND carried coin move onto a server-owned
-- corpse that sits where they died; they respawn naked. Corpses persist (a
-- server restart mid-corpse-run must not lose gear) and decay harshly after
-- CORPSE_LINGER_SECS, at which point the row (and its items, via cascade) is
-- deleted and the gear is gone for good.
--
-- corpse_id is the in-memory EntityId minted from the loot-bag id partition
-- (NEXT_BAG_ID); it is supplied, not AUTOINCREMENT, and the boot loader advances
-- the atomic past the max loaded id so a fresh bag/corpse can't collide.
-- corpse_items mirrors the loot-bag stack list (flat slot, item_path, count) —
-- equip-slot identity isn't preserved because looted gear goes to bags in EQ.

CREATE TABLE IF NOT EXISTS corpses (
    corpse_id  INTEGER NOT NULL PRIMARY KEY,
    char_id    INTEGER NOT NULL REFERENCES characters(id) ON DELETE CASCADE,
    owner_name TEXT    NOT NULL,            -- cached for the nameplate after a restart
    zone       TEXT    NOT NULL DEFAULT '',
    pos_x      REAL    NOT NULL,
    pos_y      REAL    NOT NULL,
    pos_z      REAL    NOT NULL,
    platinum   INTEGER NOT NULL DEFAULT 0,
    gold       INTEGER NOT NULL DEFAULT 0,
    silver     INTEGER NOT NULL DEFAULT 0,
    copper     INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL             -- unix seconds, for ordering / future age display
);
CREATE INDEX IF NOT EXISTS idx_corpses_char ON corpses(char_id);

CREATE TABLE IF NOT EXISTS corpse_items (
    corpse_id INTEGER NOT NULL REFERENCES corpses(corpse_id) ON DELETE CASCADE,
    slot      INTEGER NOT NULL,             -- flat enumeration [0, N)
    item_path TEXT    NOT NULL,
    count     INTEGER NOT NULL DEFAULT 1,
    PRIMARY KEY (corpse_id, slot)
);
CREATE INDEX IF NOT EXISTS idx_corpse_items_corpse ON corpse_items(corpse_id);
