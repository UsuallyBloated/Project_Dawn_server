//! Reset an account's password for Project Dawn (operator tool).
//!
//! A tester who forgets their password is otherwise locked out permanently:
//! nothing in the server can change a password, and their only recovery is
//! registering ANOTHER account, which is exactly how the duplicate accounts
//! of 2026-08-11 happened. `accounts.email` exists but is nullable, nothing
//! collects or verifies it, and there is no mail path, so an emailed reset
//! link is both unbuildable and unnecessary while the server is reachable
//! only from inside the tailnet. This bin is the friends-build answer.
//!
//! Run from the server repo root (where `world.db` lives):
//!   cargo run -p projectdawn-server --bin reset_password -- <username>
//!       generate a strong random password, print it once, set it
//!   cargo run -p projectdawn-server --bin reset_password -- <username> --stdin
//!       read the new password from stdin instead (not echoed by this tool;
//!       your terminal still echoes, so prefer piping: `echo hunter2 | ...`)
//!
//! The database defaults to `world.db`; override with the same env var the
//! server uses, e.g. PROJECTDAWN_DATABASE_URL=sqlite://other.db?mode=rwc.
//!
//! SECURITY NOTES
//! - Every session row for the account is deleted in the SAME transaction as
//!   the hash write, so a live session cannot outlive the reset. Without that,
//!   whoever prompted the reset could still be logged in on the old password.
//! - The password is NOT accepted as a plain argv word: argv lands in shell
//!   history and in other users' `ps` output. Generated-and-printed (the
//!   default) or piped through stdin are both better.
//! - A later in-game or launcher "change my password" flow needs three things
//!   this bin does not: the OLD password re-verified, its own rate-limit
//!   budget (`LoginRateLimiter` is keyed to Login and Register only, so a
//!   change-password endpoint would be an unmetered Argon2 oracle), and this
//!   same session invalidation. That is the argument for doing the bin first.

use anyhow::{bail, Context, Result};
use projectdawn_server::db;
use rand::RngCore;
use sqlx::sqlite::SqlitePoolOptions;
use std::io::Read;

/// Length of a generated password, in characters of `ALPHABET`.
const GENERATED_LEN: usize = 20;

/// Unambiguous alphabet: no 0/O, 1/l/I. An operator reads this over the phone.
const ALPHABET: &[u8] = b"abcdefghijkmnopqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ23456789";

#[tokio::main]
async fn main() -> Result<()> {
    let url = std::env::var("PROJECTDAWN_DATABASE_URL")
        .unwrap_or_else(|_| "sqlite://world.db?mode=rwc".to_string());
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.is_empty() || args.len() > 2 {
        eprintln!("usage:");
        eprintln!("  reset_password <username>            generate + print a new password");
        eprintln!("  reset_password <username> --stdin    read the new password from stdin");
        std::process::exit(2);
    }
    let username = &args[0];
    let from_stdin = match args.get(1).map(|s| s.as_str()) {
        None => false,
        Some("--stdin") => true,
        Some(other) => bail!(
            "unexpected second argument '{other}'. The password is never taken as an \
             argument (it would land in shell history and in `ps` output); use --stdin \
             or let this tool generate one."
        ),
    };

    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .with_context(|| format!("opening database '{url}'"))?;

    let (password, generated) = if from_stdin {
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .context("reading the new password from stdin")?;
        // Trailing newline from a pipe or a typed line is not part of it.
        (buf.trim_end_matches(['\r', '\n']).to_string(), false)
    } else {
        (generate_password(), true)
    };

    // The hash write and the session purge share one transaction inside
    // `db::reset_password`, which is also where the password floor and the
    // unknown-account check live (one implementation, unit-tested there).
    let (account_id, purged) = db::reset_password(&pool, username, &password)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))
        .with_context(|| format!("resetting the password for '{username}'"))?;

    println!("Password reset for {username} (account id {account_id}).");
    println!("Sessions invalidated: {purged}.");
    if generated {
        println!();
        println!("  New password: {password}");
        println!();
        println!("Shown once. Hand it over out of band; there is no in-game change-password");
        println!("flow yet, so this stays their password until the next reset.");
    }
    if purged > 0 {
        println!("A live world session keeps playing until it drops (there is no kick-by-account");
        println!("tool yet); the old password cannot start a new one.");
    }
    Ok(())
}

/// Rejection-free sampling over `ALPHABET` (its length divides evenly enough
/// that modulo bias is negligible at 56 symbols; this is a temporary operator
/// password, not a key).
fn generate_password() -> String {
    let mut bytes = vec![0u8; GENERATED_LEN];
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes
        .iter()
        .map(|b| ALPHABET[*b as usize % ALPHABET.len()] as char)
        .collect()
}
