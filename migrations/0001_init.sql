-- Project Dawn — initial schema.
-- Subset of docs/server_design.md Section 7 sufficient for the auth service:
-- accounts, sessions, characters. Inventory / equipment / skills / quests
-- arrive with the world server.

PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS accounts (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    username      TEXT    NOT NULL UNIQUE COLLATE NOCASE,
    password_hash TEXT    NOT NULL,
    email         TEXT,
    is_gm         BOOLEAN NOT NULL DEFAULT FALSE,
    is_banned     BOOLEAN NOT NULL DEFAULT FALSE,
    ban_reason    TEXT,
    created_at    TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    last_login    TIMESTAMP
);

CREATE TABLE IF NOT EXISTS sessions (
    token       BLOB PRIMARY KEY,
    account_id  INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    issued_at   TIMESTAMP NOT NULL,
    expires_at  TIMESTAMP NOT NULL,
    last_seen   TIMESTAMP NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_sessions_account ON sessions(account_id);
CREATE INDEX IF NOT EXISTS idx_sessions_expires ON sessions(expires_at);

CREATE TABLE IF NOT EXISTS characters (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    account_id      INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    name            TEXT    NOT NULL UNIQUE COLLATE NOCASE,
    race            TEXT    NOT NULL,
    class           TEXT    NOT NULL,
    level           INTEGER NOT NULL DEFAULT 1,
    xp              INTEGER NOT NULL DEFAULT 0,
    xp_to_next      INTEGER NOT NULL DEFAULT 100,
    base_strength       INTEGER NOT NULL DEFAULT 10,
    base_dexterity      INTEGER NOT NULL DEFAULT 10,
    base_agility        INTEGER NOT NULL DEFAULT 10,
    base_intelligence   INTEGER NOT NULL DEFAULT 10,
    base_wisdom         INTEGER NOT NULL DEFAULT 10,
    base_charisma       INTEGER NOT NULL DEFAULT 10,
    base_constitution   INTEGER NOT NULL DEFAULT 10,
    base_max_hp         REAL    NOT NULL DEFAULT 100.0,
    base_max_mp         REAL    NOT NULL DEFAULT 100.0,
    base_max_stamina    REAL    NOT NULL DEFAULT 100.0,
    hp                  REAL    NOT NULL,
    mp                  REAL    NOT NULL,
    stamina             REAL    NOT NULL,
    coins               INTEGER NOT NULL DEFAULT 0,
    alignment_score     INTEGER NOT NULL DEFAULT 0,
    bind_zone           TEXT,
    bind_entry          TEXT,
    bind_zone_name      TEXT,
    transformation      TEXT,
    zone                TEXT,
    pos_x               REAL,
    pos_y               REAL,
    pos_z               REAL,
    yaw                 REAL,
    created_at          TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    last_played_at      TIMESTAMP,
    deleted_at          TIMESTAMP
);
CREATE INDEX IF NOT EXISTS idx_chars_account ON characters(account_id);

-- GM action audit, per design doc. Empty until a GM uses /gm anything.
CREATE TABLE IF NOT EXISTS gm_actions (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    actor_account   INTEGER NOT NULL REFERENCES accounts(id),
    target_account  INTEGER REFERENCES accounts(id),
    target_char     INTEGER REFERENCES characters(id),
    command         TEXT    NOT NULL,
    args            TEXT,
    timestamp       TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);
