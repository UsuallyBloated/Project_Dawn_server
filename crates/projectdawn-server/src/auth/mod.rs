//! Auth WebSocket service.
//!
//! One Tokio task per connection. Reads JSON frames, dispatches to the DB
//! layer, writes JSON responses. No connection-level state — every
//! message except `Register`/`Login` carries its own `session_token`.

use crate::{
    db,
    error::{AuthError, AuthResult},
    world, Config,
};
use futures_util::{SinkExt, StreamExt};
use protocol::auth::{ClientAuthMsg, ServerAuthMsg};
use sqlx::SqlitePool;
use std::sync::Arc;
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::Message;

pub async fn serve(cfg: Arc<Config>, pool: SqlitePool) -> anyhow::Result<()> {
    let listener = TcpListener::bind(&cfg.auth_bind).await?;
    let actual = listener.local_addr()?;
    tracing::info!(addr = %actual, "auth WS listening");

    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %e, "accept failed");
                continue;
            }
        };
        let cfg = cfg.clone();
        let pool = pool.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_connection(stream, peer.to_string(), cfg, pool).await {
                tracing::warn!(peer = %peer, error = %e, "connection ended with error");
            }
        });
    }
}

/// Like `serve` but returns the bound address. Used by integration tests
/// to start the server on an ephemeral port and discover where it landed.
pub async fn serve_bound(
    cfg: Arc<Config>,
    pool: SqlitePool,
) -> anyhow::Result<(std::net::SocketAddr, tokio::task::JoinHandle<()>)> {
    let listener = TcpListener::bind(&cfg.auth_bind).await?;
    let addr = listener.local_addr()?;
    let handle = tokio::spawn(async move {
        loop {
            let (stream, peer) = match listener.accept().await {
                Ok(p) => p,
                Err(_) => continue,
            };
            let cfg = cfg.clone();
            let pool = pool.clone();
            tokio::spawn(async move {
                let _ = handle_connection(stream, peer.to_string(), cfg, pool).await;
            });
        }
    });
    Ok((addr, handle))
}

async fn handle_connection(
    stream: TcpStream,
    peer: String,
    cfg: Arc<Config>,
    pool: SqlitePool,
) -> anyhow::Result<()> {
    let ws = tokio_tungstenite::accept_async(stream).await?;
    let (mut write, mut read) = ws.split();
    tracing::debug!(%peer, "ws upgraded");

    while let Some(frame) = read.next().await {
        let frame = match frame {
            Ok(f) => f,
            Err(e) => {
                tracing::debug!(%peer, error = %e, "ws read error");
                break;
            }
        };

        let text = match frame {
            Message::Text(t) => t.to_string(),
            Message::Binary(_) => {
                send_error(
                    &mut write,
                    &AuthError::InvalidInput("auth channel is JSON text only".into()),
                )
                .await?;
                continue;
            }
            Message::Ping(p) => {
                write.send(Message::Pong(p)).await?;
                continue;
            }
            Message::Close(_) => break,
            Message::Pong(_) | Message::Frame(_) => continue,
        };

        let parsed: Result<ClientAuthMsg, _> = serde_json::from_str(&text);
        let msg = match parsed {
            Ok(m) => m,
            Err(e) => {
                send_error(
                    &mut write,
                    &AuthError::InvalidInput(format!("malformed JSON: {e}")),
                )
                .await?;
                continue;
            }
        };

        let response = dispatch(&cfg, &pool, msg).await;
        let response = match response {
            Ok(r) => r,
            Err(e) => {
                tracing::debug!(%peer, error = %e, "auth error");
                ServerAuthMsg::Error {
                    code: e.code(),
                    msg: e.user_msg(),
                }
            }
        };

        write
            .send(Message::Text(serde_json::to_string(&response)?))
            .await?;
    }

    tracing::debug!(%peer, "connection closed");
    Ok(())
}

async fn dispatch(
    cfg: &Config,
    pool: &SqlitePool,
    msg: ClientAuthMsg,
) -> AuthResult<ServerAuthMsg> {
    match msg {
        ClientAuthMsg::Register {
            username,
            password,
            email,
        } => {
            let id = db::create_account(pool, &username, &password, email.as_deref()).await?;
            Ok(ServerAuthMsg::RegisterOk { account_id: id })
        }
        ClientAuthMsg::Login {
            username,
            password,
            client_version,
        } => {
            let cv = crate::config::semver::Version::parse(&client_version)
                .ok_or_else(|| AuthError::InvalidInput(format!(
                    "client_version is not semver: {client_version}"
                )))?;
            if !cv.at_least(&cfg.min_client_version) {
                return Err(AuthError::VersionMismatch {
                    client: client_version,
                    required: cfg.min_client_version.to_string(),
                });
            }
            let outcome = db::verify_login(pool, &username, &password).await?;
            let chars = db::list_characters(pool, outcome.account_id).await?;
            Ok(ServerAuthMsg::LoginOk {
                session_token: outcome.session_token_hex,
                account_id: outcome.account_id,
                is_gm: outcome.is_gm,
                world_endpoint: cfg.world_endpoint.clone(),
                characters: chars,
            })
        }
        ClientAuthMsg::CharList { session_token } => {
            let account_id = db::touch_session(pool, &session_token).await?;
            let chars = db::list_characters(pool, account_id).await?;
            Ok(ServerAuthMsg::CharList { characters: chars })
        }
        ClientAuthMsg::CharCreate {
            session_token,
            name,
            race,
            class,
        } => {
            let account_id = db::touch_session(pool, &session_token).await?;
            let id = db::create_character(pool, account_id, &name, &race, &class).await?;
            Ok(ServerAuthMsg::CharCreated { char_id: id })
        }
        ClientAuthMsg::CharDelete {
            session_token,
            char_id,
        } => {
            let account_id = db::touch_session(pool, &session_token).await?;
            db::delete_character(pool, account_id, char_id).await?;
            Ok(ServerAuthMsg::CharDeleted)
        }
        ClientAuthMsg::Logout { session_token } => {
            db::revoke_session(pool, &session_token).await?;
            Ok(ServerAuthMsg::LogoutOk)
        }
        ClientAuthMsg::RequestWorldToken {
            session_token,
            char_id,
        } => {
            let account_id = db::touch_session(pool, &session_token).await?;
            db::verify_char_owned(pool, account_id, char_id).await?;
            let is_gm = db::account_is_gm(pool, account_id).await?;
            let (token_bytes, expires_at_unix) = world::mint_connect_token(
                cfg,
                &cfg.world_endpoint,
                char_id as u64,
                account_id,
                is_gm,
            )
            .map_err(AuthError::Internal)?;
            Ok(ServerAuthMsg::WorldConnectToken {
                token_bytes,
                world_endpoint: cfg.world_endpoint.clone(),
                expires_at_unix,
            })
        }
    }
}

async fn send_error<S>(sink: &mut S, err: &AuthError) -> anyhow::Result<()>
where
    S: SinkExt<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
{
    let resp = ServerAuthMsg::Error {
        code: err.code(),
        msg: err.user_msg(),
    };
    sink.send(Message::Text(serde_json::to_string(&resp)?))
        .await?;
    Ok(())
}
