//! Invite codes for Project Dawn (operator tool): mint, list, revoke.
//!
//! On a public port anyone can reach the auth service, so with
//! `PROJECTDAWN_REQUIRE_INVITE=1` a `Register` needs one of these. One code
//! makes one account (`db::create_account_with_invite` burns it in the same
//! transaction that creates the account), and a code is shown ONCE, here.
//!
//! Run from the directory that holds `world.db`:
//!   cargo run -p projectdawn-server --bin invite
//!       list every code: minted when, note, redeemed by whom, revoked
//!   cargo run -p projectdawn-server --bin invite -- new [n] [note...]
//!       mint n codes (default 1) with an optional memo, e.g. `new 1 for Sam`
//!   cargo run -p projectdawn-server --bin invite -- revoke <code>
//!       an unredeemed code stops working; a redeemed one cannot be revoked
//!       (the account exists; ban it instead)
//!
//! The database defaults to `world.db` in the current directory and must
//! already exist (this tool never creates one); override with the same env
//! var the server uses, e.g. PROJECTDAWN_DATABASE_URL=sqlite://other.db?mode=rw.

use anyhow::{Context, Result};
use projectdawn_server::db::{self, RevokeOutcome};
use sqlx::sqlite::SqlitePoolOptions;

/// What the command line asked for, decided BEFORE the database is opened so
/// bad usage never touches a file.
enum Action {
    List,
    New { count: u32, note: Option<String> },
    Revoke { code: String },
}

/// Most codes one `new` will mint: a typo like `new 1000` should not fill the
/// table.
const MAX_PER_MINT: u32 = 50;

fn parse_args(args: &[String]) -> Action {
    match args.first().map(|s| s.to_ascii_lowercase()).as_deref() {
        None | Some("list") => Action::List,
        Some("new") => {
            // `new 3 for the Thursday group` and `new for Sam` both work: the
            // count is optional and only ever the first word.
            let (count, rest) = match args.get(1).and_then(|s| s.parse::<u32>().ok()) {
                Some(n) => (n, &args[2..]),
                None => (1, &args[1..]),
            };
            if count == 0 || count > MAX_PER_MINT {
                eprintln!("count must be 1 to {MAX_PER_MINT}");
                usage();
            }
            let note = rest.join(" ").trim().to_string();
            Action::New {
                count,
                note: if note.is_empty() { None } else { Some(note) },
            }
        }
        Some("revoke") => {
            if args.len() != 2 {
                usage();
            }
            Action::Revoke { code: args[1].clone() }
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

    // `mode=rw`, not `rwc`: this tool must never CREATE a database (the
    // `admin_account` rule; a missing database is the error it is).
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
        Action::New { count, note } => mint(&pool, count, note.as_deref()).await,
        Action::Revoke { code } => revoke(&pool, &code).await,
    }
}

fn usage() -> ! {
    eprintln!("usage:");
    eprintln!("  invite                       list invite codes");
    eprintln!("  invite new [n] [note...]     mint n codes (default 1), shown once");
    eprintln!("  invite revoke <code>         stop an unredeemed code working");
    std::process::exit(2);
}

async fn mint(pool: &sqlx::SqlitePool, count: u32, note: Option<&str>) -> Result<()> {
    let codes = db::create_invites(pool, count, note)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))
        .context("minting invite codes")?;
    println!(
        "Minted {} invite code{}{}. Each makes ONE account; this is the only time they are shown:",
        codes.len(),
        if codes.len() == 1 { "" } else { "s" },
        note.map(|n| format!(" ({n})")).unwrap_or_default()
    );
    for code in codes {
        println!("  {code}");
    }
    Ok(())
}

async fn revoke(pool: &sqlx::SqlitePool, code: &str) -> Result<()> {
    match db::revoke_invite(pool, code)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))
        .context("revoking the invite code")?
    {
        RevokeOutcome::Revoked => println!("Revoked. The code no longer registers anyone."),
        RevokeOutcome::AlreadyRevoked => println!("That code was already revoked; no change."),
        RevokeOutcome::AlreadyRedeemed => {
            println!("That code has already made an account, so there is nothing to revoke.");
            println!("To remove the player, ban the account: admin_account ban <username>.");
        }
        RevokeOutcome::NoSuchCode => {
            println!("No such code (run `invite` with no arguments to list them).");
        }
    }
    Ok(())
}

async fn list(pool: &sqlx::SqlitePool) -> Result<()> {
    let rows = db::list_invites(pool)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))
        .context("listing invite codes")?;
    if rows.is_empty() {
        println!("No invite codes. Mint one with: invite new [note]");
        return Ok(());
    }
    let open = rows
        .iter()
        .filter(|r| r.redeemed_at.is_none() && r.revoked_at.is_none())
        .count();
    println!("Invite codes ({} total, {open} unused):", rows.len());
    for r in rows {
        let state = if let Some(at) = r.revoked_at {
            format!("REVOKED {}", at.format("%Y-%m-%d %H:%M"))
        } else if let Some(at) = r.redeemed_at {
            format!(
                "redeemed {} by {}",
                at.format("%Y-%m-%d %H:%M"),
                r.redeemed_by.as_deref().unwrap_or("(account deleted)")
            )
        } else {
            "unused".to_string()
        };
        println!("  {}   minted {}   {state}", r.code, r.created_at);
        if let Some(note) = r.note.filter(|n| !n.is_empty()) {
            println!("        note: {note}");
        }
    }
    Ok(())
}
