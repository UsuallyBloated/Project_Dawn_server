//! Project Dawn server binary entry point. Thin wrapper around the
//! library crate.

use anyhow::Context;
use projectdawn_server::{auth, db, Config};
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

    let cfg = Config::load().context("loading server config")?;
    tracing::info!(
        auth_bind = %cfg.auth_bind,
        db = %cfg.database_url,
        min_client = %cfg.min_client_version,
        "starting projectdawn-server"
    );

    let pool = db::open(&cfg.database_url).await?;
    db::migrate(&pool).await?;

    let cfg_arc = std::sync::Arc::new(cfg);
    auth::serve(cfg_arc, pool).await?;
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
