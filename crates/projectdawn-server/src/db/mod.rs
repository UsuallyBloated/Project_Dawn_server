//! SQLite pool, migrations, and the small set of queries auth needs.
//!
//! Uses runtime-checked `sqlx::query`/`query_as` rather than the compile-time
//! `query!` macros so the build doesn't need a live DATABASE_URL.

use crate::error::{AuthError, AuthResult};
use argon2::{
    password_hash::{rand_core::OsRng, PasswordHasher, SaltString},
    Argon2, PasswordHash, PasswordVerifier,
};
use chrono::{DateTime, Duration, Utc};
use protocol::auth::CharacterSummary;
use protocol::world::Coins;
use rand::RngCore;
use sqlx::{sqlite::SqlitePoolOptions, FromRow, Row, SqlitePool};

pub const SESSION_TTL: Duration = Duration::minutes(30);

pub async fn open(database_url: &str) -> anyhow::Result<SqlitePool> {
    let pool = SqlitePoolOptions::new()
        .max_connections(8)
        .connect(database_url)
        .await?;
    Ok(pool)
}

pub async fn migrate(pool: &SqlitePool) -> anyhow::Result<()> {
    sqlx::migrate!("../../migrations").run(pool).await?;
    Ok(())
}

// ─── Account creation ────────────────────────────────────────────────────

const USERNAME_MIN: usize = 3;
const USERNAME_MAX: usize = 20;
const PASSWORD_MIN: usize = 8;

pub fn validate_username(name: &str) -> AuthResult<()> {
    if name.len() < USERNAME_MIN || name.len() > USERNAME_MAX {
        return Err(AuthError::InvalidInput(format!(
            "username must be {USERNAME_MIN}–{USERNAME_MAX} characters"
        )));
    }
    if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(AuthError::InvalidInput(
            "username may contain only letters, digits, and underscore".into(),
        ));
    }
    Ok(())
}

pub fn validate_password(pw: &str) -> AuthResult<()> {
    if pw.len() < PASSWORD_MIN {
        return Err(AuthError::InvalidInput(format!(
            "password must be at least {PASSWORD_MIN} characters"
        )));
    }
    Ok(())
}

pub async fn create_account(
    pool: &SqlitePool,
    username: &str,
    password: &str,
    email: Option<&str>,
) -> AuthResult<i64> {
    validate_username(username)?;
    validate_password(password)?;

    let salt = SaltString::generate(&mut OsRng);
    let argon = Argon2::default();
    let hash = argon
        .hash_password(password.as_bytes(), &salt)?
        .to_string();

    let res = sqlx::query("INSERT INTO accounts (username, password_hash, email) VALUES (?1, ?2, ?3)")
        .bind(username)
        .bind(&hash)
        .bind(email)
        .execute(pool)
        .await;

    match res {
        Ok(out) => Ok(out.last_insert_rowid()),
        Err(sqlx::Error::Database(dbe)) if dbe.is_unique_violation() => Err(AuthError::NameTaken),
        Err(e) => Err(AuthError::from(e)),
    }
}

// ─── Login & sessions ────────────────────────────────────────────────────

pub struct LoginOutcome {
    pub account_id: i64,
    pub is_gm: bool,
    pub session_token_hex: String,
}

#[derive(FromRow)]
struct AccountAuthRow {
    id: i64,
    password_hash: String,
    is_gm: bool,
    is_banned: bool,
    ban_reason: Option<String>,
}

/// A fixed, valid Argon2 PHC hash used only to burn the same CPU as a real
/// password verify when the username doesn't exist (see `verify_login`).
/// Computed once with the server's Argon2 params so its verify cost matches
/// production. The password it hashes is irrelevant — the verify always fails.
fn dummy_login_hash() -> &'static str {
    static H: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    H.get_or_init(|| {
        let salt = SaltString::generate(&mut OsRng);
        Argon2::default()
            .hash_password(b"timing-equalizer", &salt)
            .expect("hash dummy password")
            .to_string()
    })
}

/// Precompute the timing-equalizer hash at startup so the first missing-user
/// login never pays the one-time `OnceLock` init cost (which would make that one
/// request ~2x Argon2 and stand out). Call once during boot.
pub fn warm_login_timing_defense() {
    let _ = dummy_login_hash();
}

pub async fn verify_login(
    pool: &SqlitePool,
    username: &str,
    password: &str,
) -> AuthResult<LoginOutcome> {
    let row: Option<AccountAuthRow> = sqlx::query_as(
        "SELECT id, password_hash, is_gm, is_banned, ban_reason
         FROM accounts WHERE username = ?1 COLLATE NOCASE",
    )
    .bind(username)
    .fetch_optional(pool)
    .await?;

    let row = match row {
        Some(r) => r,
        None => {
            // Enumeration defense (auth-timing): a missing username must cost the
            // same wall-clock as a wrong password. A real account runs an Argon2
            // verify below; without this, a missing account would return instantly,
            // letting an attacker distinguish "user exists" by response time. Burn
            // one dummy verify against a fixed valid hash, discard it, then fail
            // with the SAME error as a wrong password.
            let _ = Argon2::default().verify_password(
                password.as_bytes(),
                &PasswordHash::new(dummy_login_hash()).expect("dummy hash is valid PHC"),
            );
            return Err(AuthError::AuthFailed);
        }
    };
    if row.is_banned {
        return Err(AuthError::Banned(row.ban_reason.unwrap_or_default()));
    }

    let parsed = PasswordHash::new(&row.password_hash)?;
    if Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_err()
    {
        return Err(AuthError::AuthFailed);
    }

    let token = issue_session(pool, row.id).await?;

    sqlx::query("UPDATE accounts SET last_login = CURRENT_TIMESTAMP WHERE id = ?1")
        .bind(row.id)
        .execute(pool)
        .await?;

    Ok(LoginOutcome {
        account_id: row.id,
        is_gm: row.is_gm,
        session_token_hex: token,
    })
}

pub async fn issue_session(pool: &SqlitePool, account_id: i64) -> AuthResult<String> {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    let token_hex = hex::encode(bytes);

    let now = Utc::now();
    let expires = now + SESSION_TTL;

    sqlx::query(
        "INSERT INTO sessions (token, account_id, issued_at, expires_at, last_seen)
         VALUES (?1, ?2, ?3, ?4, ?3)",
    )
    .bind(&bytes[..])
    .bind(account_id)
    .bind(now)
    .bind(expires)
    .execute(pool)
    .await?;

    Ok(token_hex)
}

pub async fn touch_session(pool: &SqlitePool, token_hex: &str) -> AuthResult<i64> {
    let bytes = decode_token(token_hex)?;
    let now = Utc::now();

    let row = sqlx::query("SELECT account_id, expires_at FROM sessions WHERE token = ?1")
        .bind(&bytes[..])
        .fetch_optional(pool)
        .await?;

    let row = row.ok_or(AuthError::SessionExpired)?;
    let account_id: i64 = row.get("account_id");
    let expires_at: DateTime<Utc> = row.get("expires_at");

    if expires_at < now {
        return Err(AuthError::SessionExpired);
    }

    sqlx::query("UPDATE sessions SET last_seen = ?1 WHERE token = ?2")
        .bind(now)
        .bind(&bytes[..])
        .execute(pool)
        .await?;

    Ok(account_id)
}

pub async fn revoke_session(pool: &SqlitePool, token_hex: &str) -> AuthResult<()> {
    let bytes = decode_token(token_hex)?;
    sqlx::query("DELETE FROM sessions WHERE token = ?1")
        .bind(&bytes[..])
        .execute(pool)
        .await?;
    Ok(())
}

fn decode_token(token_hex: &str) -> AuthResult<[u8; 32]> {
    let v = hex::decode(token_hex).map_err(|_| AuthError::SessionExpired)?;
    let arr: [u8; 32] = v.try_into().map_err(|_| AuthError::SessionExpired)?;
    Ok(arr)
}

// ─── Characters ──────────────────────────────────────────────────────────

const CHAR_NAME_MIN: usize = 2;
const CHAR_NAME_MAX: usize = 24;

#[derive(FromRow)]
struct CharRow {
    id: i64,
    name: String,
    race: String,
    class: String,
    level: i32,
    zone: Option<String>,
}

pub async fn list_characters(
    pool: &SqlitePool,
    account_id: i64,
) -> AuthResult<Vec<CharacterSummary>> {
    let rows: Vec<CharRow> = sqlx::query_as(
        "SELECT id, name, race, class, level, zone
         FROM characters
         WHERE account_id = ?1 AND deleted_at IS NULL
         ORDER BY id ASC",
    )
    .bind(account_id)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|r| CharacterSummary {
            id: r.id,
            name: r.name,
            race: r.race,
            class: r.class,
            level: r.level,
            zone: r.zone,
        })
        .collect())
}

pub async fn create_character(
    pool: &SqlitePool,
    account_id: i64,
    name: &str,
    race: &str,
    class: &str,
) -> AuthResult<i64> {
    let trimmed = name.trim();
    if trimmed.len() < CHAR_NAME_MIN || trimmed.len() > CHAR_NAME_MAX {
        return Err(AuthError::InvalidInput(format!(
            "character name must be {CHAR_NAME_MIN}–{CHAR_NAME_MAX} characters"
        )));
    }
    if !trimmed
        .chars()
        .all(|c| c.is_ascii_alphabetic() || c == '\'' || c == '`')
    {
        return Err(AuthError::InvalidInput(
            "character name may contain only letters, apostrophes, and backticks".into(),
        ));
    }

    // Track 6 sub-task 2: compute race/class-derived base stats at
    // character creation. Without this, every fresh character starts
    // with the schema-default stats of 10 and max_hp=100, and the
    // server's HealthUpdate fan-out (now authoritative on the client)
    // would override the client's own apply_character with the wrong
    // values. Mirror of GDScript `PlayerStats.apply_character(race,
    // class, level=1)`.
    let computed = crate::char_data::compute(race, class, 1);

    let res = sqlx::query(
        "INSERT INTO characters (
            account_id, name, race, class, level, xp_to_next,
            base_strength, base_dexterity, base_agility,
            base_intelligence, base_wisdom, base_charisma,
            base_constitution,
            base_max_hp, base_max_mp, base_max_stamina,
            hp, mp, stamina
         ) VALUES (
            ?1, ?2, ?3, ?4, 1, ?5,
            ?6, ?7, ?8, ?9, ?10, ?11, ?12,
            ?13, ?14, ?15,
            ?16, ?17, ?18
         )",
    )
    .bind(account_id)
    .bind(trimmed)
    .bind(race)
    .bind(class)
    .bind(computed.xp_to_next)
    .bind(computed.stats.strength)
    .bind(computed.stats.dexterity)
    .bind(computed.stats.agility)
    .bind(computed.stats.intelligence)
    .bind(computed.stats.wisdom)
    .bind(computed.stats.charisma)
    .bind(computed.stats.constitution)
    .bind(computed.max_hp)
    .bind(computed.max_mp)
    .bind(computed.max_stamina)
    .bind(computed.max_hp)
    .bind(computed.max_mp)
    .bind(computed.max_stamina)
    .execute(pool)
    .await;

    match res {
        Ok(out) => Ok(out.last_insert_rowid()),
        Err(sqlx::Error::Database(dbe)) if dbe.is_unique_violation() => Err(AuthError::NameTaken),
        Err(e) => Err(AuthError::from(e)),
    }
}

pub async fn delete_character(
    pool: &SqlitePool,
    account_id: i64,
    char_id: i64,
) -> AuthResult<()> {
    // Append _del_{id} to the name so the UNIQUE constraint frees the original
    // name for a new character without requiring a schema migration.
    let res = sqlx::query(
        "UPDATE characters
         SET deleted_at = CURRENT_TIMESTAMP,
             name = name || '_del_' || CAST(id AS TEXT)
         WHERE id = ?1 AND account_id = ?2 AND deleted_at IS NULL",
    )
    .bind(char_id)
    .bind(account_id)
    .execute(pool)
    .await?;

    if res.rows_affected() == 0 {
        return Err(AuthError::NotFound);
    }
    Ok(())
}

/// Confirm `char_id` belongs to `account_id` and is not soft-deleted.
/// Used by the auth handler before minting a world ConnectToken — without
/// this a logged-in player could request a token for *any* character.
pub async fn verify_char_owned(
    pool: &SqlitePool,
    account_id: i64,
    char_id: i64,
) -> AuthResult<()> {
    let row = sqlx::query(
        "SELECT 1 FROM characters
         WHERE id = ?1 AND account_id = ?2 AND deleted_at IS NULL",
    )
    .bind(char_id)
    .bind(account_id)
    .fetch_optional(pool)
    .await?;
    if row.is_none() {
        return Err(AuthError::NotFound);
    }
    Ok(())
}

/// Read an account's GM flag, to stamp into the world connect token. A missing
/// account reads as non-GM (defensive; the caller has already validated the
/// session, so the row should exist).
pub async fn account_is_gm(pool: &SqlitePool, account_id: i64) -> AuthResult<bool> {
    let row = sqlx::query("SELECT is_gm FROM accounts WHERE id = ?1")
        .bind(account_id)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(|r| r.get::<bool, _>("is_gm")).unwrap_or(false))
}

/// Set (or clear) an account's GM flag by username. Returns the previous value,
/// or `None` if no such account. Backs the `grant_gm` bin and the is_gm-gate
/// integration test.
pub async fn set_account_gm(
    pool: &SqlitePool,
    username: &str,
    value: bool,
) -> AuthResult<Option<bool>> {
    let row = sqlx::query("SELECT id, is_gm FROM accounts WHERE username = ?1 COLLATE NOCASE")
        .bind(username)
        .fetch_optional(pool)
        .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let id: i64 = row.get("id");
    let before: bool = row.get("is_gm");
    sqlx::query("UPDATE accounts SET is_gm = ?1 WHERE id = ?2")
        .bind(value)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(Some(before))
}

/// Loaded snapshot of the persistent fields the world server cares about
/// at character spawn. Track 6 promoted resources + stats + xp to load-time
/// (server is authoritative on these now); inventory / equipment land later.
#[derive(Debug, Clone)]
pub struct CharacterSpawn {
    pub char_id: i64,
    pub account_id: i64,
    pub name: String,
    pub race: String,
    pub class: String,
    pub level: i32,
    pub xp: i32,
    pub xp_to_next: i32,
    pub strength: i32,
    pub dexterity: i32,
    pub agility: i32,
    pub intelligence: i32,
    pub wisdom: i32,
    pub charisma: i32,
    pub constitution: i32,
    pub max_hp: f32,
    pub max_mp: f32,
    pub max_stamina: f32,
    pub hp: f32,
    pub mp: f32,
    pub stamina: f32,
    pub coins: Coins,
    /// Per-character bank wallet (Banker NPC, slice 1) — zero-weight coin
    /// storage at the Banker. Seeded from the `bank_*` columns.
    pub bank_coins: Coins,
    pub zone: Option<String>,
    pub pos: (f32, f32, f32),
    pub yaw: f32,
    /// Quest ids this character has already turned in (quests pay once, ever).
    /// Loaded into `PerConnection.completed_quests`; the tick loop consults it
    /// before granting a `CompleteQuest` reward.
    pub completed_quests: Vec<String>,
    /// Accepted-but-not-completed quests with per-objective progress counts
    /// (PD_W0024, `active_quests` table). Loaded into
    /// `PerConnection.active_quests` and fanned to the client as
    /// `QuestSnapshot` on EnterWorld — the persistent quest journal.
    pub active_quests: Vec<(String, Vec<i32>)>,
}

#[derive(FromRow)]
struct SpawnRow {
    id: i64,
    account_id: i64,
    name: String,
    race: String,
    class: String,
    level: i32,
    xp: i32,
    xp_to_next: i32,
    base_strength: i32,
    base_dexterity: i32,
    base_agility: i32,
    base_intelligence: i32,
    base_wisdom: i32,
    base_charisma: i32,
    base_constitution: i32,
    base_max_hp: f32,
    base_max_mp: f32,
    base_max_stamina: f32,
    hp: f32,
    mp: f32,
    stamina: f32,
    platinum: i64,
    gold: i64,
    silver: i64,
    copper: i64,
    bank_platinum: i64,
    bank_gold: i64,
    bank_silver: i64,
    bank_copper: i64,
    zone: Option<String>,
    pos_x: Option<f32>,
    pos_y: Option<f32>,
    pos_z: Option<f32>,
    yaw: Option<f32>,
}

pub async fn load_character(
    pool: &SqlitePool,
    char_id: i64,
) -> AuthResult<CharacterSpawn> {
    let row: Option<SpawnRow> = sqlx::query_as(
        "SELECT id, account_id, name, race, class, level, xp, xp_to_next,
                base_strength, base_dexterity, base_agility,
                base_intelligence, base_wisdom, base_charisma,
                base_constitution,
                base_max_hp, base_max_mp, base_max_stamina,
                hp, mp, stamina, platinum, gold, silver, copper,
                bank_platinum, bank_gold, bank_silver, bank_copper,
                zone, pos_x, pos_y, pos_z, yaw
         FROM characters
         WHERE id = ?1 AND deleted_at IS NULL",
    )
    .bind(char_id)
    .fetch_optional(pool)
    .await?;
    let row = row.ok_or(AuthError::NotFound)?;

    // Track 6 sub-task 2: always recompute stats + max resources from
    // race/class/level via `char_data`. The DB columns capture
    // intrinsic-stat-redistribution + gear-bonus persistence in a
    // future feature; until then they're write-only at create_character
    // time and the formula is the source of truth. This silently
    // upgrades any pre-sub-task-2 character row (created with schema
    // defaults of stat=10, max_hp=100) on next login. Current hp/mp/
    // stamina are clamped against the freshly-computed max.
    //
    // The same recompute reconciles the XP curve. Older saves can hold a level
    // above MAX_LEVEL (early builds let a char climb past 60) and an xp_to_next
    // from the previous 1.5x curve — often the i32::MAX-saturated band. Clamp the
    // level, take the band from `computed`, and clamp stored xp into it, so the
    // band, stats, and death-penalty math all use the live cubic curve from the
    // first tick instead of self-healing only after the first XP event (where a
    // death-before-award would otherwise charge 5% of a multi-billion stale band).
    let level = row.level.clamp(1, crate::world::skills::MAX_LEVEL);
    let computed = crate::char_data::compute(&row.race, &row.class, level);
    let xp = row.xp.clamp(0, computed.xp_to_next);
    let _ = (row.base_strength, row.base_dexterity, row.base_agility,
             row.base_intelligence, row.base_wisdom, row.base_charisma,
             row.base_constitution, row.base_max_hp, row.base_max_mp,
             row.base_max_stamina);

    // Quest completions (server-authoritative rewards): loaded with the
    // character so the world loop can reject a repeat turn-in without a query.
    let completed_quests: Vec<String> =
        sqlx::query_scalar("SELECT quest_id FROM completed_quests WHERE char_id = ?1")
            .bind(char_id)
            .fetch_all(pool)
            .await?;

    // Active quest journal (PD_W0024): progress is a JSON array of counts.
    // A malformed row decodes to an empty vector, which `objectives_met`
    // treats as NOT met — corruption can only under-credit, never unlock a
    // free turn-in.
    let active_rows: Vec<(String, String)> =
        sqlx::query_as("SELECT quest_id, progress FROM active_quests WHERE char_id = ?1")
            .bind(char_id)
            .fetch_all(pool)
            .await?;
    let active_quests: Vec<(String, Vec<i32>)> = active_rows
        .into_iter()
        .map(|(quest_id, json)| {
            let progress: Vec<i32> = serde_json::from_str(&json).unwrap_or_else(|e| {
                tracing::warn!(char_id, quest_id = %quest_id, error = %e,
                    "malformed active_quests.progress; resetting to empty");
                Vec::new()
            });
            (quest_id, progress)
        })
        .collect();

    Ok(CharacterSpawn {
        char_id: row.id,
        account_id: row.account_id,
        name: row.name,
        race: row.race,
        class: row.class,
        level,
        xp,
        xp_to_next: computed.xp_to_next,
        strength: computed.stats.strength,
        dexterity: computed.stats.dexterity,
        agility: computed.stats.agility,
        intelligence: computed.stats.intelligence,
        wisdom: computed.stats.wisdom,
        charisma: computed.stats.charisma,
        constitution: computed.stats.constitution,
        max_hp: computed.max_hp,
        max_mp: computed.max_mp,
        max_stamina: computed.max_stamina,
        hp: row.hp.min(computed.max_hp),
        mp: row.mp.min(computed.max_mp),
        stamina: row.stamina.min(computed.max_stamina),
        coins: Coins {
            platinum: row.platinum,
            gold: row.gold,
            silver: row.silver,
            copper: row.copper,
        },
        bank_coins: Coins {
            platinum: row.bank_platinum,
            gold: row.bank_gold,
            silver: row.bank_silver,
            copper: row.bank_copper,
        },
        zone: row.zone,
        pos: (
            row.pos_x.unwrap_or(0.0),
            row.pos_y.unwrap_or(0.0),
            row.pos_z.unwrap_or(0.0),
        ),
        yaw: row.yaw.unwrap_or(0.0),
        completed_quests,
        active_quests,
    })
}

/// Upsert one active quest's objective progress (PD_W0024). Written
/// per-mutation (accept = zeros, each counted kill) rather than on the 60s
/// checkpoint — quest kills are rare, and immediate writes mean neither a
/// crash nor the disconnect flush can lose counted progress.
pub async fn save_quest_progress(
    pool: &SqlitePool,
    char_id: i64,
    quest_id: &str,
    progress: &[i32],
) -> AuthResult<()> {
    let json = serde_json::to_string(progress).expect("Vec<i32> serializes");
    sqlx::query(
        "INSERT INTO active_quests (char_id, quest_id, progress) VALUES (?1, ?2, ?3)
         ON CONFLICT (char_id, quest_id) DO UPDATE SET progress = excluded.progress",
    )
    .bind(char_id)
    .bind(quest_id)
    .bind(json)
    .execute(pool)
    .await?;
    Ok(())
}

/// Drop one active quest row (abandon, or turn-in — the finished quest's
/// permanent record is `completed_quests`, not this table). Idempotent.
pub async fn delete_active_quest(
    pool: &SqlitePool,
    char_id: i64,
    quest_id: &str,
) -> AuthResult<()> {
    sqlx::query("DELETE FROM active_quests WHERE char_id = ?1 AND quest_id = ?2")
        .bind(char_id)
        .bind(quest_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Record a quest turn-in (server-authoritative quest rewards). Idempotent —
/// the (char_id, quest_id) primary key makes a repeat insert a no-op, and the
/// world loop checks the in-memory set before awarding anyway. Persisted
/// BEFORE the XP grant so a crash can't leave a paid-but-unrecorded quest that
/// a relog could turn in again.
pub async fn record_quest_completion(
    pool: &SqlitePool,
    char_id: i64,
    quest_id: &str,
) -> AuthResult<()> {
    sqlx::query("INSERT OR IGNORE INTO completed_quests (char_id, quest_id) VALUES (?1, ?2)")
        .bind(char_id)
        .bind(quest_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Periodic position checkpoint. Track 6 split the resources path into its
/// own `checkpoint_resources` since the server now mutates HP/MP/Stamina
/// on every regen tick — checkpointing them separately keeps the position
/// path cheap (single-row UPDATE).
pub async fn checkpoint_position(
    pool: &SqlitePool,
    char_id: i64,
    zone: Option<&str>,
    pos: (f32, f32, f32),
    yaw: f32,
) -> AuthResult<()> {
    sqlx::query(
        "UPDATE characters
         SET zone = ?1, pos_x = ?2, pos_y = ?3, pos_z = ?4, yaw = ?5,
             last_played_at = CURRENT_TIMESTAMP
         WHERE id = ?6 AND deleted_at IS NULL",
    )
    .bind(zone)
    .bind(pos.0)
    .bind(pos.1)
    .bind(pos.2)
    .bind(yaw)
    .bind(char_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Test-only: directly set a character's world position in the DB. Used by
/// AOI integration tests to place characters in specific grid cells before
/// connecting, so the world server loads the overridden position at spawn.
pub async fn set_character_position(
    pool: &SqlitePool,
    char_id: i64,
    x: f64,
    y: f64,
    z: f64,
) -> anyhow::Result<()> {
    sqlx::query(
        "UPDATE characters SET pos_x = ?1, pos_y = ?2, pos_z = ?3 WHERE id = ?4",
    )
    .bind(x)
    .bind(y)
    .bind(z)
    .bind(char_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Track 6 periodic checkpoint for the resources the server now owns:
/// current HP/MP/Stamina + accumulated XP. Called alongside
/// `checkpoint_position` on the 60 s cadence and on disconnect so a power
/// loss only rolls back ~60 s of regen / kill credit.
pub async fn checkpoint_resources(
    pool: &SqlitePool,
    char_id: i64,
    hp: f32,
    mp: f32,
    stamina: f32,
    xp: i32,
    xp_to_next: i32,
    level: i32,
) -> AuthResult<()> {
    sqlx::query(
        "UPDATE characters
         SET hp = ?1, mp = ?2, stamina = ?3, xp = ?4, xp_to_next = ?5,
             level = ?6
         WHERE id = ?7 AND deleted_at IS NULL",
    )
    .bind(hp)
    .bind(mp)
    .bind(stamina)
    .bind(xp)
    .bind(xp_to_next)
    .bind(level)
    .bind(char_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Track 13.1 — one row of `character_items`. `location` distinguishes
/// `'base'` (Track 13.1, base inventory slots) from `'bag_<i>'`
/// (Track 13.2) and `'equip'` (Track 13.3). For 13.1 only `'base'` is
/// written; the wider semantics live in app code and the table is
/// intentionally loose so future locations don't need another
/// migration.
#[derive(Debug, Clone, FromRow)]
pub struct InventoryRow {
    pub location: String,
    pub slot: i32,
    pub item_path: String,
    pub count: i32,
}

/// Load every inventory row for a character. Returns an empty Vec for
/// freshly-created characters (which never wrote any rows). Callers
/// project this into the in-memory `PlayerInventory` shape.
pub async fn load_inventory(
    pool: &SqlitePool,
    char_id: i64,
) -> AuthResult<Vec<InventoryRow>> {
    let rows: Vec<InventoryRow> = sqlx::query_as(
        "SELECT location, slot, item_path, count
         FROM character_items
         WHERE char_id = ?1
         ORDER BY location, slot",
    )
    .bind(char_id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Persist the full inventory snapshot for `char_id`. Atomic delete +
/// insert pattern — simpler than diffing in-memory state against the
/// DB and the row count per character is tiny (≤ 8 base + a few bags
/// in the worst case). The whole thing runs inside a transaction so a
/// crash partway through leaves the DB consistent.
pub async fn save_inventory(
    pool: &SqlitePool,
    char_id: i64,
    rows: &[InventoryRow],
) -> AuthResult<()> {
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM character_items WHERE char_id = ?1")
        .bind(char_id)
        .execute(&mut *tx)
        .await?;
    for row in rows {
        sqlx::query(
            "INSERT INTO character_items (char_id, location, slot, item_path, count)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )
        .bind(char_id)
        .bind(&row.location)
        .bind(row.slot)
        .bind(&row.item_path)
        .bind(row.count)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

// ── Corpses (corpse / resurrection epic, Slice 1) ─────────────────────────────

/// A corpse loaded from the DB at boot. Plain data; `world::tick` maps it into
/// the in-memory `world::corpses::Corpse`.
#[derive(Debug, Clone)]
pub struct CorpseRow {
    pub corpse_id: i64,
    pub char_id: i64,
    pub owner_name: String,
    pub zone: String,
    pub pos: (f32, f32, f32),
    pub coins: protocol::world::Coins,
    pub items: Vec<(String, u32)>,
    pub lost_xp: i32,
    pub resurrected: bool,
}

#[derive(FromRow)]
struct CorpseHeaderRow {
    corpse_id: i64,
    char_id: i64,
    owner_name: String,
    zone: String,
    pos_x: f64,
    pos_y: f64,
    pos_z: f64,
    platinum: i64,
    gold: i64,
    silver: i64,
    copper: i64,
    lost_xp: i64,
    resurrected: i64,
}

/// Persist a corpse + its item stacks atomically. Called BEFORE the player's
/// live inventory is cleared, so a crash between this commit and the clear
/// leaves the gear on the corpse (recoverable) rather than vaporizing it.
/// `corpse_id` is the minted EntityId (loot-bag id partition); `items` are
/// flat (item_path, count) stacks.
pub async fn save_corpse(
    pool: &SqlitePool,
    corpse_id: i64,
    char_id: i64,
    owner_name: &str,
    zone: &str,
    pos: (f32, f32, f32),
    coins: protocol::world::Coins,
    items: &[(String, u32)],
    lost_xp: i32,
) -> AuthResult<()> {
    let created_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let mut tx = pool.begin().await?;
    // `resurrected` defaults to 0 in the schema — a fresh corpse is never rezzed.
    sqlx::query(
        "INSERT INTO corpses
           (corpse_id, char_id, owner_name, zone, pos_x, pos_y, pos_z,
            platinum, gold, silver, copper, created_at, lost_xp)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
    )
    .bind(corpse_id)
    .bind(char_id)
    .bind(owner_name)
    .bind(zone)
    .bind(pos.0 as f64)
    .bind(pos.1 as f64)
    .bind(pos.2 as f64)
    .bind(coins.platinum)
    .bind(coins.gold)
    .bind(coins.silver)
    .bind(coins.copper)
    .bind(created_at)
    .bind(lost_xp as i64)
    .execute(&mut *tx)
    .await?;
    for (slot, (item_path, count)) in items.iter().enumerate() {
        sqlx::query(
            "INSERT INTO corpse_items (corpse_id, slot, item_path, count)
             VALUES (?1, ?2, ?3, ?4)",
        )
        .bind(corpse_id)
        .bind(slot as i64)
        .bind(item_path)
        .bind(*count as i64)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// Load every corpse (+ its items) from the DB. Called ONCE at server boot,
/// before any login is accepted, so a corpse is always in memory before its
/// owner could log in and loot it.
pub async fn load_corpses(pool: &SqlitePool) -> AuthResult<Vec<CorpseRow>> {
    let headers: Vec<CorpseHeaderRow> = sqlx::query_as(
        "SELECT corpse_id, char_id, owner_name, zone, pos_x, pos_y, pos_z,
                platinum, gold, silver, copper, lost_xp, resurrected
         FROM corpses",
    )
    .fetch_all(pool)
    .await?;
    let mut out = Vec::with_capacity(headers.len());
    for h in headers {
        let items: Vec<(String, i64)> = sqlx::query_as(
            "SELECT item_path, count FROM corpse_items WHERE corpse_id = ?1 ORDER BY slot",
        )
        .bind(h.corpse_id)
        .fetch_all(pool)
        .await?;
        out.push(CorpseRow {
            corpse_id: h.corpse_id,
            char_id: h.char_id,
            owner_name: h.owner_name,
            zone: h.zone,
            pos: (h.pos_x as f32, h.pos_y as f32, h.pos_z as f32),
            coins: protocol::world::Coins {
                platinum: h.platinum,
                gold: h.gold,
                silver: h.silver,
                copper: h.copper,
            },
            items: items.into_iter().map(|(p, n)| (p, n as u32)).collect(),
            lost_xp: h.lost_xp as i32,
            resurrected: h.resurrected != 0,
        });
    }
    Ok(out)
}

/// Delete a corpse + its items (decay, or fully-looted in Slice 2). Explicit
/// two-table delete so it works regardless of the FK-cascade pragma.
pub async fn delete_corpse(pool: &SqlitePool, corpse_id: i64) -> AuthResult<()> {
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM corpse_items WHERE corpse_id = ?1")
        .bind(corpse_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM corpses WHERE corpse_id = ?1")
        .bind(corpse_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

/// Mark a corpse resurrected (corpse / resurrection Slice 3) so it can't be
/// rezzed twice for free XP. Persisted so a restart can't reset the flag.
pub async fn set_corpse_resurrected(pool: &SqlitePool, corpse_id: i64) -> AuthResult<()> {
    sqlx::query("UPDATE corpses SET resurrected = 1 WHERE corpse_id = ?1")
        .bind(corpse_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Corpse / resurrection Slice 2 — persist ONE corpse-loot action ATOMICALLY:
/// the looter's full inventory + wallet AND the corpse itself, all in a single
/// transaction. Corpses are DB-backed (unlike transient loot bags), so without
/// this a crash between an inventory save and a corpse change could dupe the item
/// (in both) or lose it. When `delete_corpse` is true (the corpse was looted
/// fully empty) the body is DELETED inside this same tx — folding the delete in
/// (rather than a separate `delete_corpse` call) is what makes it crash-safe: the
/// body can never be removed while the matching inventory write is rolled back.
/// Otherwise the corpse's shrunk items + coins are rewritten and the row stays.
#[allow(clippy::too_many_arguments)]
pub async fn apply_corpse_loot(
    pool: &SqlitePool,
    char_id: i64,
    inv_rows: &[InventoryRow],
    looter_coins: protocol::world::Coins,
    corpse_id: i64,
    corpse_items: &[(String, u32)],
    corpse_coins: protocol::world::Coins,
    delete_corpse: bool,
) -> AuthResult<()> {
    let mut tx = pool.begin().await?;
    // Looter inventory — full DELETE + reINSERT (same shape as save_inventory).
    sqlx::query("DELETE FROM character_items WHERE char_id = ?1")
        .bind(char_id)
        .execute(&mut *tx)
        .await?;
    for row in inv_rows {
        sqlx::query(
            "INSERT INTO character_items (char_id, location, slot, item_path, count)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )
        .bind(char_id)
        .bind(&row.location)
        .bind(row.slot)
        .bind(&row.item_path)
        .bind(row.count)
        .execute(&mut *tx)
        .await?;
    }
    // Looter wallet (corpse coins credited back to the owner).
    sqlx::query("UPDATE characters SET platinum = ?1, gold = ?2, silver = ?3, copper = ?4 WHERE id = ?5")
        .bind(looter_coins.platinum)
        .bind(looter_coins.gold)
        .bind(looter_coins.silver)
        .bind(looter_coins.copper)
        .bind(char_id)
        .execute(&mut *tx)
        .await?;
    // Corpse — clear its items, then either DELETE the body (looted fully empty)
    // or rewrite its shrunk items + coins. Both happen in THIS tx so the corpse
    // change commits together with the inventory write, never half-applied.
    sqlx::query("DELETE FROM corpse_items WHERE corpse_id = ?1")
        .bind(corpse_id)
        .execute(&mut *tx)
        .await?;
    if delete_corpse {
        sqlx::query("DELETE FROM corpses WHERE corpse_id = ?1")
            .bind(corpse_id)
            .execute(&mut *tx)
            .await?;
    } else {
        for (slot, (item_path, count)) in corpse_items.iter().enumerate() {
            sqlx::query(
                "INSERT INTO corpse_items (corpse_id, slot, item_path, count)
                 VALUES (?1, ?2, ?3, ?4)",
            )
            .bind(corpse_id)
            .bind(slot as i64)
            .bind(item_path)
            .bind(*count as i64)
            .execute(&mut *tx)
            .await?;
        }
        sqlx::query("UPDATE corpses SET platinum = ?1, gold = ?2, silver = ?3, copper = ?4 WHERE corpse_id = ?5")
            .bind(corpse_coins.platinum)
            .bind(corpse_coins.gold)
            .bind(corpse_coins.silver)
            .bind(corpse_coins.copper)
            .bind(corpse_id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// Banker slice 2 — one stack in an item vault. No `location` column: the
/// table it came from (bank_items vs account_bank_items) is the store, and
/// the slot is a flat index into that store.
#[derive(Debug, Clone, FromRow)]
pub struct BankItemRow {
    pub slot: i32,
    pub item_path: String,
    pub count: i32,
}

/// Load the per-character bank vault rows (char-keyed). Empty for a
/// character that has never banked an item.
pub async fn load_bank_items(pool: &SqlitePool, char_id: i64) -> AuthResult<Vec<BankItemRow>> {
    let rows: Vec<BankItemRow> = sqlx::query_as(
        "SELECT slot, item_path, count FROM bank_items WHERE char_id = ?1 ORDER BY slot",
    )
    .bind(char_id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Persist the per-character bank vault (atomic DELETE + INSERT, like
/// `save_inventory`). Gated on `bank_items_dirty`.
pub async fn save_bank_items(
    pool: &SqlitePool,
    char_id: i64,
    rows: &[BankItemRow],
) -> AuthResult<()> {
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM bank_items WHERE char_id = ?1")
        .bind(char_id)
        .execute(&mut *tx)
        .await?;
    for row in rows {
        sqlx::query(
            "INSERT INTO bank_items (char_id, slot, item_path, count) VALUES (?1, ?2, ?3, ?4)",
        )
        .bind(char_id)
        .bind(row.slot)
        .bind(&row.item_path)
        .bind(row.count)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// Load the account-shared bank vault rows (ACCOUNT-keyed — shared across
/// all of the account's characters).
pub async fn load_account_bank_items(
    pool: &SqlitePool,
    account_id: i64,
) -> AuthResult<Vec<BankItemRow>> {
    let rows: Vec<BankItemRow> = sqlx::query_as(
        "SELECT slot, item_path, count FROM account_bank_items WHERE account_id = ?1 ORDER BY slot",
    )
    .bind(account_id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Persist the account-shared bank vault (atomic DELETE + INSERT, keyed on
/// account_id — never char_id).
pub async fn save_account_bank_items(
    pool: &SqlitePool,
    account_id: i64,
    rows: &[BankItemRow],
) -> AuthResult<()> {
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM account_bank_items WHERE account_id = ?1")
        .bind(account_id)
        .execute(&mut *tx)
        .await?;
    for row in rows {
        sqlx::query(
            "INSERT INTO account_bank_items (account_id, slot, item_path, count)
             VALUES (?1, ?2, ?3, ?4)",
        )
        .bind(account_id)
        .bind(row.slot)
        .bind(&row.item_path)
        .bind(row.count)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// Persist the four-tier wallet. Called from the checkpoint sweep +
/// disconnect flush whenever `coins_dirty` is set — without this the
/// in-session wallet (vendor buys/sells, dev grants) silently resets
/// to the stale DB row on next login.
pub async fn save_coins(pool: &SqlitePool, char_id: i64, coins: Coins) -> AuthResult<()> {
    sqlx::query(
        "UPDATE characters SET platinum = ?1, gold = ?2, silver = ?3, copper = ?4
         WHERE id = ?5",
    )
    .bind(coins.platinum)
    .bind(coins.gold)
    .bind(coins.silver)
    .bind(coins.copper)
    .bind(char_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Persist the per-character bank wallet (Banker NPC, slice 1). Mirror of
/// `save_coins`; gated on `bank_dirty` in the checkpoint sweep + disconnect
/// flush so deposits / withdrawals / exchanges survive logout.
pub async fn save_bank(pool: &SqlitePool, char_id: i64, bank: Coins) -> AuthResult<()> {
    sqlx::query(
        "UPDATE characters SET bank_platinum = ?1, bank_gold = ?2,
                bank_silver = ?3, bank_copper = ?4
         WHERE id = ?5",
    )
    .bind(bank.platinum)
    .bind(bank.gold)
    .bind(bank.silver)
    .bind(bank.copper)
    .bind(char_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Track 18.1 — one row of `character_skills`. `kind` is one of
/// `'weapon'`, `'armor'`, `'casting'`; `key` is the GDScript skill key
/// (e.g. `'1h_slashing'`, `'cloth'`, `'evocation'`).
#[derive(Debug, Clone, FromRow)]
pub struct SkillRow {
    pub kind: String,
    pub key: String,
    pub score: i32,
}

pub async fn load_skills(
    pool: &SqlitePool,
    char_id: i64,
) -> AuthResult<Vec<SkillRow>> {
    let rows: Vec<SkillRow> = sqlx::query_as(
        "SELECT kind, key, score
         FROM character_skills
         WHERE char_id = ?1
         ORDER BY kind, key",
    )
    .bind(char_id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Persist all three score maps for `char_id`. Atomic delete + insert
/// (same shape as `save_inventory`); ≤ 21 rows per character so
/// rewriting the whole set is cheap. Called from the same checkpoint
/// + disconnect cadence the inventory uses.
pub async fn save_skills(
    pool: &SqlitePool,
    char_id: i64,
    rows: &[SkillRow],
) -> AuthResult<()> {
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM character_skills WHERE char_id = ?1")
        .bind(char_id)
        .execute(&mut *tx)
        .await?;
    for row in rows {
        sqlx::query(
            "INSERT INTO character_skills (char_id, kind, key, score)
             VALUES (?1, ?2, ?3, ?4)",
        )
        .bind(char_id)
        .bind(&row.kind)
        .bind(&row.key)
        .bind(row.score)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

#[cfg(test)]
mod corpse_loot_tests {
    //! Corpse / resurrection Slice 2 — the atomic corpse-loot persist. Locks the
    //! two outcomes of `apply_corpse_loot`: a PARTIAL loot rewrites the corpse row
    //! (it stays) while updating the looter, and a FULL loot DELETES the corpse in
    //! the SAME tx as the inventory write — so a crash can never drop the body
    //! while losing the gear. Each asserts the looter side AND the corpse side.
    use super::*;
    use tempfile::TempDir;

    async fn fresh_pool() -> (sqlx::SqlitePool, TempDir) {
        let tmp = TempDir::new().expect("tempdir");
        let url = format!("sqlite://{}?mode=rwc", tmp.path().join("corpse_test.db").display());
        let pool = open(&url).await.expect("open");
        migrate(&pool).await.expect("migrate");
        (pool, tmp)
    }

    async fn wallet_cols(pool: &sqlx::SqlitePool, char_id: i64) -> (i64, i64, i64, i64) {
        sqlx::query_as("SELECT platinum, gold, silver, copper FROM characters WHERE id = ?1")
            .bind(char_id)
            .fetch_one(pool)
            .await
            .expect("wallet row")
    }

    /// Account + character + a corpse owned by them holding a sword, a shield, and
    /// 50 copper. Returns (char_id, corpse_id).
    async fn setup(pool: &sqlx::SqlitePool) -> (i64, i64) {
        let account = create_account(pool, "looter", "hunter2!", None)
            .await
            .expect("account");
        let char_id = create_character(pool, account, "Looter", "Human", "Warrior")
            .await
            .expect("char");
        let corpse_id = 2_000_000_000_i64;
        save_corpse(
            pool,
            corpse_id,
            char_id,
            "Looter",
            "test_zone",
            (1.0, 0.0, 2.0),
            protocol::world::Coins::from_copper(50),
            &[
                ("res://items/sword.tres".to_string(), 1),
                ("res://items/shield.tres".to_string(), 1),
            ],
            0, // lost_xp
        )
        .await
        .expect("save corpse");
        (char_id, corpse_id)
    }

    #[tokio::test]
    async fn partial_loot_rewrites_corpse_and_updates_looter() {
        let (pool, _tmp) = fresh_pool().await;
        let (char_id, corpse_id) = setup(&pool).await;

        // Owner took the sword + the coins; the shield stays on the corpse.
        let inv_rows = vec![InventoryRow {
            location: "base".into(),
            slot: 0,
            item_path: "res://items/sword.tres".into(),
            count: 1,
        }];
        apply_corpse_loot(
            &pool,
            char_id,
            &inv_rows,
            protocol::world::Coins::from_copper(50),
            corpse_id,
            &[("res://items/shield.tres".to_string(), 1)],
            protocol::world::Coins::ZERO,
            false, // partial -> keep the corpse
        )
        .await
        .expect("apply");

        // Looter got the sword + the coins.
        let inv = load_inventory(&pool, char_id).await.expect("inv");
        assert_eq!(inv.len(), 1);
        assert_eq!(inv[0].item_path, "res://items/sword.tres");
        assert_eq!(wallet_cols(&pool, char_id).await, (0, 0, 0, 50));

        // Corpse still exists, holding only the shield, its coins zeroed.
        let corpses = load_corpses(&pool).await.expect("corpses");
        assert_eq!(corpses.len(), 1);
        assert_eq!(corpses[0].items, vec![("res://items/shield.tres".to_string(), 1)]);
        assert_eq!(corpses[0].coins, protocol::world::Coins::ZERO);
    }

    #[tokio::test]
    async fn full_loot_deletes_corpse_atomically() {
        let (pool, _tmp) = fresh_pool().await;
        let (char_id, corpse_id) = setup(&pool).await;

        // Owner took everything; the corpse is emptied -> delete it in this tx.
        let inv_rows = vec![
            InventoryRow {
                location: "base".into(),
                slot: 0,
                item_path: "res://items/sword.tres".into(),
                count: 1,
            },
            InventoryRow {
                location: "base".into(),
                slot: 1,
                item_path: "res://items/shield.tres".into(),
                count: 1,
            },
        ];
        apply_corpse_loot(
            &pool,
            char_id,
            &inv_rows,
            protocol::world::Coins::from_copper(50),
            corpse_id,
            &[],
            protocol::world::Coins::ZERO,
            true, // looted clean -> delete the corpse
        )
        .await
        .expect("apply");

        // Corpse + its items are gone.
        let corpses = load_corpses(&pool).await.expect("corpses");
        assert!(corpses.is_empty(), "corpse should be deleted after a full loot");
        let leftover: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM corpse_items WHERE corpse_id = ?1")
                .bind(corpse_id)
                .fetch_one(&pool)
                .await
                .expect("count");
        assert_eq!(leftover, 0, "corpse_items rows should be gone too");

        // Looter kept both items + the coins.
        let inv = load_inventory(&pool, char_id).await.expect("inv");
        assert_eq!(inv.len(), 2);
        assert_eq!(wallet_cols(&pool, char_id).await, (0, 0, 0, 50));
    }

    // Slice 3 — the corpse remembers the death's lost XP (for the res refund) and
    // its un-resurrected state across a save/load (a server restart).
    #[tokio::test]
    async fn corpse_lost_xp_round_trips() {
        let (pool, _tmp) = fresh_pool().await;
        let account = create_account(&pool, "rez", "hunter2!", None)
            .await
            .expect("account");
        let char_id = create_character(&pool, account, "Rez", "Human", "Cleric")
            .await
            .expect("char");
        let corpse_id = 2_000_000_077_i64;
        save_corpse(
            &pool,
            corpse_id,
            char_id,
            "Rez",
            "test_zone",
            (3.0, 0.0, 4.0),
            protocol::world::Coins::ZERO,
            &[("res://items/staff.tres".to_string(), 1)],
            500, // lost_xp
        )
        .await
        .expect("save");

        let loaded = load_corpses(&pool).await.expect("load");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].lost_xp, 500, "lost_xp must survive a save/load round-trip");
        assert!(!loaded[0].resurrected, "a fresh corpse is not resurrected");
    }

    // Server-authoritative quest rewards — a completion persists across a
    // save/load (so a relogged character can't re-turn-in the same quest),
    // and recording twice is a harmless no-op.
    #[tokio::test]
    async fn quest_completion_round_trips_once_per_character() {
        let (pool, _tmp) = fresh_pool().await;
        let account = create_account(&pool, "quests", "hunter2!", None)
            .await
            .expect("account");
        let char_id = create_character(&pool, account, "Quests", "Human", "Warrior")
            .await
            .expect("char");

        let spawn = load_character(&pool, char_id).await.expect("load");
        assert!(spawn.completed_quests.is_empty(), "fresh char has no completions");

        record_quest_completion(&pool, char_id, "wolf_threat").await.expect("record");
        record_quest_completion(&pool, char_id, "wolf_threat").await.expect("repeat is a no-op");
        record_quest_completion(&pool, char_id, "rotfang_hunt").await.expect("record 2nd");

        let spawn = load_character(&pool, char_id).await.expect("reload");
        let mut done = spawn.completed_quests.clone();
        done.sort();
        assert_eq!(done, vec!["rotfang_hunt".to_string(), "wolf_threat".to_string()]);
    }

    // Quest phase 2 (PD_W0024) — the active journal round-trips: an accept
    // (zeroed progress) persists, a counted kill's upsert overwrites it, a
    // delete (abandon / turn-in) removes it, and a malformed progress blob
    // degrades to empty (which objectives_met treats as NOT met) instead of
    // failing the whole character load.
    #[tokio::test]
    async fn active_quest_progress_round_trips() {
        let (pool, _tmp) = fresh_pool().await;
        let account = create_account(&pool, "journal", "hunter2!", None)
            .await
            .expect("account");
        let char_id = create_character(&pool, account, "Journal", "Human", "Warrior")
            .await
            .expect("char");

        let spawn = load_character(&pool, char_id).await.expect("load");
        assert!(spawn.active_quests.is_empty(), "fresh char has an empty journal");

        // Accept seeds zeros; a counted kill upserts the new counts.
        save_quest_progress(&pool, char_id, "wolf_threat", &[0]).await.expect("accept");
        save_quest_progress(&pool, char_id, "wolf_threat", &[3]).await.expect("upsert");
        save_quest_progress(&pool, char_id, "rat_infestation", &[0]).await.expect("accept 2nd");

        let spawn = load_character(&pool, char_id).await.expect("reload");
        let mut active = spawn.active_quests.clone();
        active.sort();
        assert_eq!(
            active,
            vec![
                ("rat_infestation".to_string(), vec![0]),
                ("wolf_threat".to_string(), vec![3]),
            ],
        );

        // Abandon / turn-in drops the row; deleting twice is a no-op.
        delete_active_quest(&pool, char_id, "wolf_threat").await.expect("delete");
        delete_active_quest(&pool, char_id, "wolf_threat").await.expect("repeat no-op");
        let spawn = load_character(&pool, char_id).await.expect("reload 2");
        assert_eq!(spawn.active_quests, vec![("rat_infestation".to_string(), vec![0])]);

        // A hand-corrupted progress blob must not fail the load — it comes
        // back as empty progress (not met), never as a free turn-in.
        sqlx::query("UPDATE active_quests SET progress = 'not json' WHERE char_id = ?1")
            .bind(char_id)
            .execute(&pool)
            .await
            .expect("corrupt");
        let spawn = load_character(&pool, char_id).await.expect("load survives corruption");
        assert_eq!(spawn.active_quests, vec![("rat_infestation".to_string(), Vec::new())]);
    }

    // XP curve — a character saved on a previous curve (the old 1.5x geometric
    // band often saturated to i32::MAX) must be reconciled onto the live cubic
    // curve + level cap on load, so the death-penalty math and the bar never see
    // the stale multi-billion band and a stored level can't sit above the cap.
    #[tokio::test]
    async fn load_reconciles_stale_xp_curve() {
        let (pool, _tmp) = fresh_pool().await;
        let account = create_account(&pool, "old", "hunter2!", None)
            .await
            .expect("account");
        let char_id = create_character(&pool, account, "Fert", "Human", "Warrior")
            .await
            .expect("char");

        // A high-level old save: a level above the cap, the i32::MAX-saturated
        // band, and xp far past the new (much smaller) band.
        sqlx::query("UPDATE characters SET level = ?1, xp = ?2, xp_to_next = ?3 WHERE id = ?4")
            .bind(75_i64)
            .bind(1_771_674_013_i64)
            .bind(2_147_483_647_i64) // old i32::MAX band
            .bind(char_id)
            .execute(&pool)
            .await
            .expect("update");

        let spawn = load_character(&pool, char_id).await.expect("load");
        let cap = crate::world::skills::MAX_LEVEL;
        assert_eq!(spawn.level, cap, "a stored level above the cap clamps to it");
        assert_eq!(
            spawn.xp_to_next,
            crate::char_data::xp_to_next_for(cap),
            "the band must be the live cubic band, not the stale i32::MAX value",
        );
        assert_eq!(spawn.xp, spawn.xp_to_next, "over-band stored xp clamps to a full bar");
        assert!(spawn.xp_to_next < 100_000_000, "the new band is tens of millions, not billions");
    }
}
