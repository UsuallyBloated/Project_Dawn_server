-- Server-authoritative quest rewards: record each character's completed quest
-- ids so a quest pays its XP once per character, ever (closes the relog +
-- re-turn-in XP farm; the reward amount itself now comes from the server's
-- data/quests.toml, never from the client).
CREATE TABLE IF NOT EXISTS completed_quests (
    char_id  INTEGER NOT NULL REFERENCES characters(id) ON DELETE CASCADE,
    quest_id TEXT    NOT NULL,
    PRIMARY KEY (char_id, quest_id)
);
