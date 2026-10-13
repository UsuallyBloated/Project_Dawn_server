-- Food and water, the EverQuest way (the client repo's docs/design/food_and_water.md,
-- decided 2026-10-13).
--
-- Two counters per character, hunger and thirst, in seconds of fullness. They run
-- down while the character is alive in the world; at zero the server eats the first
-- food (or drinks the first drink) in slot order, and until then natural regeneration
-- stops. Every existing character migrates full (14400 s, four hours, the cap), so
-- nobody logs in starving the first evening.
--
-- Numbered 0013: 0012 is the public hosting branch's invites table. sqlx applies
-- an unapplied lower number after a higher one without complaint, BUT a binary
-- whose migrations/ lacks a version the database has applied refuses to boot
-- (VersionMissing). So once this has run on the host, every branch that deploys
-- there must carry it: deploy the merged tree, never a branch on its own.

ALTER TABLE characters ADD COLUMN hunger REAL NOT NULL DEFAULT 14400.0;
ALTER TABLE characters ADD COLUMN thirst REAL NOT NULL DEFAULT 14400.0;
