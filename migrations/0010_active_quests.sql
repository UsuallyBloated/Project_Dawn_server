-- Quest phase 2 (PD_W0024): server-side objective tracking. One row per
-- (character, accepted-but-not-completed quest); progress is a JSON array of
-- per-objective kill counts in the same order as the quest's objectives in
-- data/quests.toml. Rows are deleted on abandon and on turn-in (the permanent
-- record of a finished quest is completed_quests). This is what makes the
-- quest journal survive relog and server restart.
CREATE TABLE IF NOT EXISTS active_quests (
    char_id  INTEGER NOT NULL REFERENCES characters(id) ON DELETE CASCADE,
    quest_id TEXT    NOT NULL,
    progress TEXT    NOT NULL,
    PRIMARY KEY (char_id, quest_id)
);
