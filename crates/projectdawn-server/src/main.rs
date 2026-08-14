//! Project Dawn server binary entry point. Thin wrapper around the
//! library crate.

use anyhow::Context;
use projectdawn_server::{auth, db, world, Config};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = dotenv_load();

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("projectdawn_server=info,sqlx=warn")),
        )
        .init();

    // `Config::load` enforces PROJECTDAWN_NETCODE_KEY presence — we'll fail
    // loud here before binding any sockets if it's missing or malformed.
    let cfg = Config::load().context("loading server config")?;
    // Surface the dev-command gate at boot so it's never ambiguous whether
    // PD_DEV_CMDS is active (a `$env:` var persists across restarts in the same
    // shell, the usual reason "/give still works after restarting without it").
    // Matches `world::connection::dev_cmds_enabled` (env == "1").
    let dev_cmds = std::env::var("PD_DEV_CMDS").as_deref() == Ok("1");
    // Same shape as dev_cmds: a dangerous env toggle belongs on the boot line so
    // it is never ambiguous which posture the process is running in.
    let rate_limit = std::env::var("PD_NO_RATE_LIMIT").as_deref() != Ok("1");
    tracing::info!(
        build = env!("PD_BUILD_COMMIT"),
        auth_bind = %cfg.auth_bind,
        world_bind = %cfg.world_bind,
        world_endpoint = %cfg.world_endpoint,
        db = %cfg.database_url,
        min_client = %cfg.min_client_version,
        dev_cmds,
        rate_limit,
        "starting projectdawn-server"
    );
    if dev_cmds {
        tracing::warn!(
            "PD_DEV_CMDS=1: dev commands (/give, dev spawn, HealSelf, GrantQuestXp) are ENABLED for ALL clients; do not run a public server with this set"
        );
    }
    if !rate_limit {
        tracing::warn!(
            "PD_NO_RATE_LIMIT=1: auth rate limiting is DISABLED — login and register are unthrottled and open to brute force. Acceptable only on a private tailnet; unset this before any public exposure"
        );
    }

    let pool = db::open(&cfg.database_url).await?;
    db::migrate(&pool).await?;

    let cfg_arc = std::sync::Arc::new(cfg);

    // Run auth (WS) and world (UDP) concurrently. The first to error wins.
    // For graceful shutdown we'd intercept Ctrl-C and tell both to drain;
    // alpha-stage is fine with abrupt termination since DB writes are
    // per-mutation atomic and the 60 s checkpoint bounds position loss.
    let auth_fut = auth::serve(cfg_arc.clone(), pool.clone());
    let world_fut = world::serve(cfg_arc, pool);
    tokio::try_join!(auth_fut, world_fut)?;
    Ok(())
}

/// Minimal `.env` loader: KEY=VALUE per line, `#` comments, no quoting magic.
fn dotenv_load() -> std::io::Result<()> {
    let bytes = std::fs::read(".env")?;
    let text = String::from_utf8_lossy(&bytes);
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let k = k.trim();
        let v = v.trim().trim_matches('"');
        if std::env::var_os(k).is_none() {
            // SAFETY: single-threaded startup before tokio runtime is up.
            unsafe { std::env::set_var(k, v) };
        }
    }
    Ok(())
}
