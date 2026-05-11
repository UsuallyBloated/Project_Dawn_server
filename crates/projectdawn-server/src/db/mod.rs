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

    let res = sqlx::query(
        "INSERT INTO characters (account_id, name, race, class, hp, mp, stamina)
         VALUES (?1, ?2, ?3, ?4, 100, 100, 100)",
    )
    .bind(account_id)
    .bind(trimmed)
    .bind(race)
    .bind(class)
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
    let res = sqlx::query(
        "UPDATE characters
         SET deleted_at = CURRENT_TIMESTAMP
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
/// at character spawn. Inventory / equipment / skills land later.
#[derive(Debug, Clone)]
pub struct CharacterSpawn {
    pub char_id: i64,
    pub account_id: i64,
    pub name: String,
    pub race: String,
    pub class: String,
    pub level: i32,
    pub hp: f32,
    pub mp: f32,
    pub stamina: f32,
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
    hp: f32,
    mp: f32,
    stamina: f32,
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
        "SELECT id, account_id, name, race, class, level, hp, mp, stamina,
                zone, pos_x, pos_y, pos_z, yaw
         FROM characters
         WHERE id = ?1 AND deleted_at IS NULL",
    )
    .bind(char_id)
    .fetch_optional(pool)
    .await?;
    let row = row.ok_or(AuthError::NotFound)?;
    Ok(CharacterSpawn {
        char_id: row.id,
        account_id: row.account_id,
        name: row.name,
        race: row.race,
        class: row.class,
        level: row.level,
        hp: row.hp,
        mp: row.mp,
        stamina: row.stamina,
        zone: row.zone,
        pos: (
            row.pos_x.unwrap_or(0.0),
            row.pos_y.unwrap_or(0.0),
            row.pos_z.unwrap_or(0.0),
        ),
        yaw: row.yaw.unwrap_or(0.0),
    })
}

/// Periodic checkpoint — called by the world server every ~60 s and on
/// disconnect. Single-row UPDATE; cheap with WAL. We intentionally do NOT
/// touch HP/MP/stamina here — those have their own paths once combat lands.
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
