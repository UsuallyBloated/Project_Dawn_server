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

    let row = row.ok_or(AuthError::AuthFailed)?;
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
    pub coins: i64,
    pub zone: Option<String>,
    pub pos: (f32, f32, f32),
    pub yaw: f32,
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
    coins: i64,
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
                hp, mp, stamina, coins,
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
    let computed = crate::char_data::compute(&row.race, &row.class, row.level);
    let _ = (row.base_strength, row.base_dexterity, row.base_agility,
             row.base_intelligence, row.base_wisdom, row.base_charisma,
             row.base_constitution, row.base_max_hp, row.base_max_mp,
             row.base_max_stamina);

    Ok(CharacterSpawn {
        char_id: row.id,
        account_id: row.account_id,
        name: row.name,
        race: row.race,
        class: row.class,
        level: row.level,
        xp: row.xp,
        xp_to_next: row.xp_to_next,
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
        coins: row.coins,
        zone: row.zone,
        pos: (
            row.pos_x.unwrap_or(0.0),
            row.pos_y.unwrap_or(0.0),
            row.pos_z.unwrap_or(0.0),
        ),
        yaw: row.yaw.unwrap_or(0.0),
    })
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
