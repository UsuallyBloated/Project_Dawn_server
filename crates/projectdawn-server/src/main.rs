//! Project Dawn server binary entry point. Thin wrapper around the
//! library crate.

use anyhow::Context;
use projectdawn_server::{auth, db, world, Config};
use tracing_subscriber::EnvFilter;

/// A plain synchronous `main`, so `.env` is loaded before the tokio runtime
/// (and its worker threads) exist: `dotenv_load` calls `std::env::set_var`,
/// which is only sound while the process is single-threaded.
fn main() -> anyhow::Result<()> {
    let _ = dotenv_load();
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("building the tokio runtime")?
        .block_on(run())
}

async fn run() -> anyhow::Result<()> {
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

    // A stop signal (SIGTERM from systemd on `systemctl stop` or `restart`,
    // or Ctrl-C) asks the world to shut down cleanly: it tells every player,
    // saves everyone and returns. Without this, a restart threw away up to
    // 60 s of everyone's progress.
    // A SECOND stop signal ends the process at once: listening for the signal
    // replaces the default "terminate" action, so without this a shutdown stuck
    // on a locked database could not be interrupted from the keyboard.
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        wait_for_stop_signal().await;
        tracing::info!("stop signal received; saving everyone (signal again to quit at once)");
        let _ = shutdown_tx.send(true);
        wait_for_stop_signal().await;
        tracing::warn!("second stop signal; exiting without finishing the save");
        std::process::exit(130);
    });

    // Run auth (WS) and world (UDP) together. The process ends when the world
    // ends, cleanly after a stop signal or with its error, or if auth fails.
    // Auth is simply dropped at exit: nothing it holds needs saving.
    let auth_fut = auth::serve(cfg_arc.clone(), pool.clone());
    let world_fut = world::serve(cfg_arc, pool, shutdown_rx);
    tokio::select! {
        result = auth_fut => result?,
        result = world_fut => result?,
    }
    Ok(())
}

/// Resolves on the next stop signal: SIGTERM or Ctrl-C (SIGINT) on Linux;
/// Ctrl-C, Ctrl-Break, closing the console window, or the machine shutting
/// down on Windows (the last two allow about 5 s, enough for the save).
async fn wait_for_stop_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = term.recv() => {}
                    _ = tokio::signal::ctrl_c() => {}
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "could not listen for SIGTERM; Ctrl-C only");
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(windows)]
    {
        use tokio::signal::windows;
        match (windows::ctrl_break(), windows::ctrl_close(), windows::ctrl_shutdown()) {
            (Ok(mut brk), Ok(mut close), Ok(mut shut)) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = brk.recv() => {}
                    _ = close.recv() => {}
                    _ = shut.recv() => {}
                }
            }
            _ => {
                tracing::warn!("could not listen for console close; Ctrl-C only");
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
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
