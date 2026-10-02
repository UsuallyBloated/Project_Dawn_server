//! Account housekeeping for Project Dawn (operator tool): list, ban, unban.
//!
//! `accounts.is_banned` has been enforced at login since the first schema
//! (`db::verify_login` refuses with the stored reason), but nothing could set
//! it short of hand-editing SQLite. This bin is the way to set it.
//!
//! Run from the server repo root (where `world.db` lives):
//!   cargo run -p projectdawn-server --bin admin_account
//!       list every account: flags, ban reason, characters, live sessions
//!   cargo run -p projectdawn-server --bin admin_account -- ban <username> [reason...]
//!       ban the account; the reason (optional) is shown to them at login
//!   cargo run -p projectdawn-server --bin admin_account -- unban <username>
//!
//! The database defaults to `world.db` in the current directory and must
//! already exist (this tool never creates one); override with the same env
//! var the server uses, e.g. PROJECTDAWN_DATABASE_URL=sqlite://other.db?mode=rw.
//!
//! WHAT A BAN DOES AND DOES NOT DO
//! - The flag, the reason and the deletion of every session row for the
//!   account happen in ONE transaction (`db::set_account_banned`).
//! - It bites at login (no new session), at every session redemption (so a
//!   login that was in flight when the ban landed gets nothing usable), and
//!   at world connect (a connect token minted just before the ban).
//! - A character ALREADY IN THE WORLD keeps playing until it drops: there is
//!   no kick-by-account tool yet. Restart the server to force a banned
//!   player out right away.
//! - Re-running `ban` with no reason purges sessions again and keeps the
//!   reason already on file.
//!
//! NOT BUILT YET: account delete and the purge of soft-deleted characters
//! (see `handoff_account_admin.md` in the client repo's session notes). Every
//! child table cascades from accounts or characters EXCEPT `gm_actions`, so a
//! delete has to decide what happens to audit rows first.

use anyhow::{bail, Context, Result};
use projectdawn_server::db;
use sqlx::sqlite::SqlitePoolOptions;
use sqlx::Row;

/// What the command line asked for, decided BEFORE the database is opened so
/// bad usage never touches a file.
enum Action {
    List,
    Ban { username: String, reason: String },
    Unban { username: String },
}

fn parse_args(args: &[String]) -> Action {
    match args.first().map(|s| s.to_ascii_lowercase()).as_deref() {
        None | Some("list") => Action::List,
        Some("ban") => {
            let Some(username) = args.get(1) else { usage() };
            Action::Ban {
                username: username.clone(),
                reason: args[2..].join(" "),
            }
        }
        Some("unban") => {
            if args.len() != 2 {
                usage();
            }
            Action::Unban { username: args[1].clone() }
        }
        Some(other) => {
            eprintln!("unknown subcommand '{other}'");
            usage()
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let action = parse_args(&args);

    // `mode=rw`, not `rwc`: this tool must never CREATE a database. Run from
    // the wrong directory, `rwc` would quietly leave an empty `world.db`
    // behind that looks like a real one to the next tool; `rw` makes a
    // missing database the error it is.
    let url = std::env::var("PROJECTDAWN_DATABASE_URL")
        .unwrap_or_else(|_| "sqlite://world.db?mode=rw".to_string());
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .with_context(|| {
            format!(
                "opening database '{url}' (run from the directory that holds world.db, \
                 or set PROJECTDAWN_DATABASE_URL)"
            )
        })?;

    match action {
        Action::List => list(&pool).await,
        Action::Ban { username, reason } => {
            set_banned(&pool, &username, true, Some(reason.as_str())).await
        }
        Action::Unban { username } => set_banned(&pool, &username, false, None).await,
    }
}

fn usage() -> ! {
    eprintln!("usage:");
    eprintln!("  admin_account                             list accounts");
    eprintln!("  admin_account ban <username> [reason...]  ban; the reason is shown at login");
    eprintln!("  admin_account unban <username>            lift a ban");
    std::process::exit(2);
}

async fn set_banned(
    pool: &sqlx::SqlitePool,
    username: &str,
    banned: bool,
    reason: Option<&str>,
) -> Result<()> {
    let outcome = db::set_account_banned(pool, username, banned, reason)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))
        .with_context(|| format!("updating the ban flag for '{username}'"))?;
    let Some(outcome) = outcome else {
        bail!("no account named '{username}' (run `admin_account` with no arguments to list them)");
    };
    let id = outcome.account_id;

    if banned {
        if outcome.was_banned {
            println!("{username} (account id {id}) was already banned; sessions purged again.");
        } else {
            println!("Banned {username} (account id {id}).");
        }
        // Print what is STORED (trimmed, cut at the length cap, or carried
        // over from the earlier ban), since that is what the player sees.
        match outcome.reason.as_deref() {
            Some(r) => println!("Reason shown at login: {r}"),
            None => println!("No reason on file; the login refusal will carry none."),
        }
        println!("Sessions invalidated: {}.", outcome.sessions_purged);
        println!("Logins, session use and world connects are refused from now on. A character");
        println!("already in the world keeps playing until it drops (there is no kick-by-account");
        println!("tool yet); restart the server to force it out.");
    } else if outcome.was_banned {
        println!("Unbanned {username} (account id {id}). They can log in again.");
    } else {
        println!("{username} (account id {id}) was not banned; no change.");
    }
    Ok(())
}

async fn list(pool: &sqlx::SqlitePool) -> Result<()> {
    // Live sessions are the unexpired rows. `expires_at` is bound from chrono
    // on both sides (here and in `db::issue_session`), so the comparison is
    // between two values in the same encoding.
    let rows = sqlx::query(
        "SELECT a.id, a.username, a.is_gm, a.is_banned, a.ban_reason,
                (SELECT COUNT(*) FROM characters c
                  WHERE c.account_id = a.id AND c.deleted_at IS NULL) AS chars,
                (SELECT COUNT(*) FROM sessions s
                  WHERE s.account_id = a.id AND s.expires_at > ?1) AS live_sessions
         FROM accounts a ORDER BY a.id",
    )
    .bind(chrono::Utc::now())
    .fetch_all(pool)
    .await
    .context("listing accounts")?;

    if rows.is_empty() {
        println!("No accounts.");
        return Ok(());
    }
    let banned = rows.iter().filter(|r| r.get::<bool, _>("is_banned")).count();
    println!("Accounts ({} total, {banned} banned):", rows.len());
    for r in rows {
        let id: i64 = r.get("id");
        let username: String = r.get("username");
        let mut flags = String::new();
        if r.get::<bool, _>("is_gm") {
            flags.push_str(" [GM]");
        }
        if r.get::<bool, _>("is_banned") {
            flags.push_str(" [BANNED]");
        }
        let chars: i64 = r.get("chars");
        let live: i64 = r.get("live_sessions");
        println!("  #{id:<3} {username}{flags}   characters: {chars}   live sessions: {live}");
        let reason: Option<String> = r.get("ban_reason");
        if let Some(reason) = reason.filter(|x| !x.is_empty()) {
            println!("        ban reason: {reason}");
        }
    }
    Ok(())
}
