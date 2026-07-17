//! Grant or revoke per-account GM (`accounts.is_gm`) for Project Dawn.
//!
//! GM is what lets a specific account use the dev/GM commands (`/give`, `/heal`,
//! spawn-mob, grant-XP, Level Up) on a server started WITHOUT `PD_DEV_CMDS`,
//! i.e. a hosted friends server. The world server reads this flag from the
//! signed connect token at login, so a change takes effect on the account's
//! NEXT world login (re-log a live session to pick it up).
//!
//! Run from the server repo root (where `world.db` lives):
//!   cargo run -p projectdawn-server --bin grant_gm                 # list accounts + GM status
//!   cargo run -p projectdawn-server --bin grant_gm -- <name> on    # grant GM
//!   cargo run -p projectdawn-server --bin grant_gm -- <name> off   # revoke GM
//!
//! The database defaults to `world.db`; override with the same env var the
//! server uses, e.g. PROJECTDAWN_DATABASE_URL=sqlite://other.db?mode=rwc.

use anyhow::{bail, Context, Result};
use sqlx::sqlite::SqlitePoolOptions;
use sqlx::Row;

#[tokio::main]
async fn main() -> Result<()> {
    let url = std::env::var("PROJECTDAWN_DATABASE_URL")
        .unwrap_or_else(|_| "sqlite://world.db?mode=rwc".to_string());
    let args: Vec<String> = std::env::args().skip(1).collect();

    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .with_context(|| format!("opening database '{url}'"))?;

    // No args (or `list`) → read-only listing of every account's GM status.
    if args.is_empty() || args[0].eq_ignore_ascii_case("list") {
        return list(&pool).await;
    }

    if args.len() != 2 {
        eprintln!("usage:");
        eprintln!("  grant_gm                     list accounts + GM status");
        eprintln!("  grant_gm <username> on|off   set an account's GM flag");
        std::process::exit(2);
    }

    let username = &args[0];
    let value = match args[1].to_ascii_lowercase().as_str() {
        "on" | "true" | "1" | "yes" => true,
        "off" | "false" | "0" | "no" => false,
        other => bail!("second argument must be on|off (got '{other}')"),
    };

    let row = sqlx::query("SELECT id, is_gm FROM accounts WHERE username = ?1 COLLATE NOCASE")
        .bind(username)
        .fetch_optional(&pool)
        .await
        .context("looking up account")?;
    let Some(row) = row else {
        bail!("no account named '{username}' (try `grant_gm list`)");
    };
    let id: i64 = row.get("id");
    let before: bool = row.get("is_gm");

    if before == value {
        println!("{username} (id {id}) is already GM={value}; no change.");
        return Ok(());
    }

    sqlx::query("UPDATE accounts SET is_gm = ?1 WHERE id = ?2")
        .bind(value)
        .bind(id)
        .execute(&pool)
        .await
        .context("updating is_gm")?;

    println!("Set GM for {username} (id {id}): was {before}, now {value}.");
    println!("Takes effect on that account's next world login (re-log a live session).");
    Ok(())
}

async fn list(pool: &sqlx::SqlitePool) -> Result<()> {
    let rows = sqlx::query("SELECT id, username, is_gm FROM accounts ORDER BY id")
        .fetch_all(pool)
        .await
        .context("listing accounts")?;
    if rows.is_empty() {
        println!("No accounts.");
        return Ok(());
    }
    let gm_count = rows.iter().filter(|r| r.get::<bool, _>("is_gm")).count();
    println!("Accounts ({} total, {gm_count} GM):", rows.len());
    for r in rows {
        let id: i64 = r.get("id");
        let username: String = r.get("username");
        let is_gm: bool = r.get("is_gm");
        println!("  #{id:<3} {username}{}", if is_gm { "   [GM]" } else { "" });
    }
    Ok(())
}
