-- Factions and alignment, server-owned (docs/design/factions_and_alignment.md
-- in the client repo; the user's nine decisions of 2026-10-23).
--
-- 1. One row per faction a character has ever moved standing with (a kill, a
--    quest, or a GM's DevSetFaction). No row means the faction's starting
--    value for the character's race and class (factions.toml), which is zero
--    for everyone in v1. Clamped to +-2000 by the server.
-- 2. The character's alignment score, which the client used to keep to
--    itself (the client-authority audit's last open part). The column has
--    existed since 0001 (`alignment_score INTEGER NOT NULL DEFAULT 0`), never
--    written; SQLite keeps a fractional value in it as a REAL, so it serves
--    as is. What 0001 could not say is whether the server has ever seeded it,
--    and 0 is a real score (Neutral), so `alignment_seeded` says it: a
--    character at 0 is seeded from its class's starting score at its next
--    load (a Necromancer begins Evil, not Neutral) and written at the next
--    checkpoint with the flag set.
--
-- Forward-only like every migration: a binary without it refuses to boot on a
-- database that has run it (sqlx VersionMissing). Ships in the Oct 22 bump.

CREATE TABLE IF NOT EXISTS character_factions (
    char_id    INTEGER NOT NULL REFERENCES characters(id) ON DELETE CASCADE,
    faction_id TEXT    NOT NULL,
    standing   REAL    NOT NULL DEFAULT 0,
    PRIMARY KEY (char_id, faction_id)
);

ALTER TABLE characters ADD COLUMN alignment_seeded INTEGER NOT NULL DEFAULT 0;
