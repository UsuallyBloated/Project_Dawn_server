-- Two server-held facts the client used to keep to itself (the client-authority
-- audit, docs/security/client_authority_audit_2026_10_19.md in the client repo;
-- approved by the user 2026-10-19).
--
-- 1. The memorized spell bar (audit fix 5). One row per filled gem; the server
--    refuses a cast of a spell that is not in one, and fills a gem only through
--    its own memorize (sitting, two seconds, half the spell's mana). The bars
--    players already have lived only on their own machines, so a character the
--    server has never held a bar for may hand its bar over once
--    (spell_gems_seeded 0 to 1), each spell checked for class, level and an
--    unlocked gem.
-- 2. When a character last left the world (unix seconds; NULL until it first
--    does), for NO RENT items: they are destroyed once the owner has been logged
--    out for 30 minutes, as EverQuest's TEMPORARY items are.
--
-- Forward-only like every migration: once this has run on a database, a binary
-- without it refuses to boot there (sqlx VersionMissing). It ships in the Oct 22
-- merged build with everything that needs it.

CREATE TABLE IF NOT EXISTS character_spell_gems (
    char_id    INTEGER NOT NULL REFERENCES characters(id) ON DELETE CASCADE,
    gem        INTEGER NOT NULL,
    spell_name TEXT    NOT NULL,
    PRIMARY KEY (char_id, gem)
);

ALTER TABLE characters ADD COLUMN spell_gems_seeded INTEGER NOT NULL DEFAULT 0;
ALTER TABLE characters ADD COLUMN logged_out_at INTEGER;
