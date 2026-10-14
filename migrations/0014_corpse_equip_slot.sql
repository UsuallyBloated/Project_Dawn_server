-- Corpse auto-re-equip (the cursor epic's slice 2, docs/design/cursor_slot_and_reequip.md).
--
-- A corpse item remembers the equipment slot it was worn in when its owner
-- died (0 main hand to 8, as the paperdoll numbers them), or NULL for anything
-- that was carried. Looting your own corpse puts worn gear back on that slot
-- when the slot is empty and still takes the item; everything else goes to the
-- bags as before. Corpses made before this migration have NULL throughout and
-- loot into the bags exactly as they always did.
--
-- Forward-only like every migration: once this has run on a database, a
-- binary without it refuses to boot there (sqlx VersionMissing).

ALTER TABLE corpse_items ADD COLUMN equip_slot INTEGER;
