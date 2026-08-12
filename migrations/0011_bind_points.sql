-- Server-authoritative bind points (respawn location).
--
-- Before this, respawn restored HP but never moved the player, and the server
-- owns position — so you always woke up exactly where you died, next to whatever
-- killed you. The first external tester (2026-08-11) died, respawned on the spot
-- beside two mobs, and died again 20 seconds later.
--
-- The characters table already carried `bind_zone` / `bind_entry` /
-- `bind_zone_name` from 0001_init, but they are TEXT and model a zone+entry-point
-- bind for the OLD client-local save; the server has never read or written them
-- and has no notion of a zone's entry position (zones are client scenes). The
-- server needs a literal position to teleport to, so add coordinates and reuse
-- the existing `bind_zone` column for the zone name.
--
-- NULL coordinates mean "never bound" and fall back to the starter spawn, so
-- every existing character migrates cleanly without a data backfill.

ALTER TABLE characters ADD COLUMN bind_x REAL;
ALTER TABLE characters ADD COLUMN bind_y REAL;
ALTER TABLE characters ADD COLUMN bind_z REAL;
