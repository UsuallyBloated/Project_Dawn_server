-- Project Dawn — Track 18.1 — server-side passive skill scores.
--
-- One row per (character, kind, key); kind ∈ {'weapon','armor','casting'}
-- mirroring the three GDScript autoloads (WeaponSkills, ArmorSkills,
-- CastingSkills). `key` is the GDScript skill key (e.g. '1h_slashing',
-- 'cloth', 'evocation'). `score` is the raw 0..cap value; the cap is
-- derived per-class per-level at read time on both sides.
--
-- Empty for first-connect; rows are inserted/updated by the periodic
-- checkpoint + disconnect save (mirrors `character_items`).

CREATE TABLE IF NOT EXISTS character_skills (
    char_id      INTEGER NOT NULL REFERENCES characters(id) ON DELETE CASCADE,
    kind         TEXT    NOT NULL,
    key          TEXT    NOT NULL,
    score        INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (char_id, kind, key)
);

CREATE INDEX IF NOT EXISTS idx_char_skills_char ON character_skills(char_id);
