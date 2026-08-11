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
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::Message;

// ── Auth-path rate limiting ─────────────────────────────────────────────────
// Brute-force / enumeration defense: cap Login+Register attempts per client IP
// within a rolling window. Keyed by IP (not username) so an attacker can only
// throttle THEIR OWN source address — a victim's account can never be locked out
// by someone guessing at it. A slot is consumed per attempt at CHECK TIME (one
// atomic lock), so concurrent same-IP attempts can't all slip through before the
// count catches up; a successful LOGIN clears the IP so an honest user's earlier
// fumbles don't linger. Shared across every per-connection task behind a Mutex
// (the critical section is tiny and never held across an await).

/// Auth attempts allowed per IP per `LOGIN_WINDOW` before further attempts are
/// rejected until the window rolls over. A successful login clears the count.
const MAX_LOGIN_ATTEMPTS: u32 = 5;
/// The rolling window for `MAX_LOGIN_ATTEMPTS`.
const LOGIN_WINDOW: Duration = Duration::from_secs(60);
/// Once this many IPs are tracked, drop expired windows on the next attempt so
/// the map can't grow without bound if an attacker cycles source addresses.
const RATE_LIMIT_PRUNE_AT: usize = 4096;

/// Returns true when the server was launched with `PD_NO_RATE_LIMIT=1`, which
/// turns the auth rate limiter into a no-op. Checked once, cached for the
/// process lifetime — same idiom as `world::connection::dev_cmds_enabled`.
///
/// **This disables a security control.** It exists because the 5-per-60s cap
/// is too tight for a real person on their first evening: the first external
/// tester (2026-08-11) tripped it after a clean logout and created a SECOND
/// ACCOUNT rather than wait, and there is no account-deletion tooling to tidy
/// that up. Acceptable only while the server is reachable solely over a private
/// tailnet, where brute-force risk is near zero.
///
/// **Must be unset before any public exposure.** The startup log line reports
/// `rate_limit` so this is never ambiguous, and an extra WARN fires at boot.
fn rate_limit_disabled() -> bool {
    static DISABLED: OnceLock<bool> = OnceLock::new();
    *DISABLED.get_or_init(|| std::env::var("PD_NO_RATE_LIMIT").as_deref() == Ok("1"))
}

struct Window {
    count: u32,
    start: Instant,
}

/// Which auth flow an attempt belongs to. Login and Register keep SEPARATE
/// per-IP budgets: a login success clears only the Login budget, so it can't
/// wipe an in-progress Register (enumeration) budget. Without this split, the
/// launcher's auto-login after a Register would clear the shared counter every
/// account, and Register would never throttle (playtest 2026-07-30).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum AuthKind {
    Login,
    Register,
}

pub struct LoginRateLimiter {
    inner: Mutex<HashMap<(IpAddr, AuthKind), Window>>,
}

impl LoginRateLimiter {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }

    /// Try to consume one attempt slot for `(ip, kind)`. Returns true (and
    /// consumes a slot) if under `MAX_LOGIN_ATTEMPTS` for the current window;
    /// false if already at the cap. Check-and-consume happen under a SINGLE lock,
    /// so N concurrent same-IP attempts can never all pass before the count is
    /// bumped (the TOCTOU a check-then-increment design would have).
    fn try_acquire(&self, ip: IpAddr, kind: AuthKind, now: Instant) -> bool {
        // Single choke point for both the Login and Register gates, so the
        // kill switch belongs here rather than at the two call sites.
        if rate_limit_disabled() {
            return true;
        }
        let mut map = self.inner.lock().expect("login limiter poisoned");
        if map.len() >= RATE_LIMIT_PRUNE_AT {
            map.retain(|_, w| now.duration_since(w.start) < LOGIN_WINDOW);
        }
        let w = map.entry((ip, kind)).or_insert(Window { count: 0, start: now });
        if now.duration_since(w.start) >= LOGIN_WINDOW {
            w.count = 0;
            w.start = now;
        }
        if w.count >= MAX_LOGIN_ATTEMPTS {
            return false;
        }
        w.count += 1;
        true
    }

    /// Clear `(ip, kind)` after a success so a legit user isn't penalized for
    /// earlier fumbles. Only ever called for `Login` — a Register success must
    /// NOT clear (its budget is what throttles enumeration), and clearing Login
    /// must not touch the Register budget (hence the per-kind key).
    fn clear(&self, ip: IpAddr, kind: AuthKind) {
        self.inner
            .lock()
            .expect("login limiter poisoned")
            .remove(&(ip, kind));
    }
}

impl Default for LoginRateLimiter {
    fn default() -> Self {
        Self::new()
    }
}

pub async fn serve(cfg: Arc<Config>, pool: SqlitePool) -> anyhow::Result<()> {
    let listener = TcpListener::bind(&cfg.auth_bind).await?;
    let actual = listener.local_addr()?;
    tracing::info!(addr = %actual, "auth WS listening");

    // Precompute the auth-timing equalizer hash now so no live request pays the
    // one-time init (see db::verify_login's enumeration defense).
    db::warm_login_timing_defense();

    let limiter = Arc::new(LoginRateLimiter::new());
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
        let limiter = limiter.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_connection(stream, peer, cfg, pool, limiter).await {
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
    let limiter = Arc::new(LoginRateLimiter::new());
    let handle = tokio::spawn(async move {
        loop {
            let (stream, peer) = match listener.accept().await {
                Ok(p) => p,
                Err(_) => continue,
            };
            let cfg = cfg.clone();
            let pool = pool.clone();
            let limiter = limiter.clone();
            tokio::spawn(async move {
                let _ = handle_connection(stream, peer, cfg, pool, limiter).await;
            });
        }
    });
    Ok((addr, handle))
}

async fn handle_connection(
    stream: TcpStream,
    peer: SocketAddr,
    cfg: Arc<Config>,
    pool: SqlitePool,
    limiter: Arc<LoginRateLimiter>,
) -> anyhow::Result<()> {
    let ws = tokio_tungstenite::accept_async(stream).await?;
    let (mut write, mut read) = ws.split();
    // Key the login rate limiter on the IP only — the ephemeral source port
    // changes every connection, so keying on the full SocketAddr would defeat it.
    let client_ip = peer.ip();
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

        let response = dispatch(&cfg, &pool, msg, client_ip, &limiter).await;
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
    client_ip: IpAddr,
    limiter: &LoginRateLimiter,
) -> AuthResult<ServerAuthMsg> {
    match msg {
        ClientAuthMsg::Register {
            username,
            password,
            email,
        } => {
            // Same per-IP gate as Login: NameTaken vs RegisterOk is a username
            // existence oracle, so Register must be throttled too or it negates
            // the Login-path enumeration defense. No clear on success — a Register
            // success must NOT reset the budget (junk-registering would give an
            // attacker a way to keep enumerating).
            let now = Instant::now();
            if !limiter.try_acquire(client_ip, AuthKind::Register, now) {
                // INFO for the same reason as the Login throttle: account-spam /
                // username-enumeration attempts must be visible to an operator.
                tracing::info!(
                    ip = %client_ip,
                    window_secs = LOGIN_WINDOW.as_secs(),
                    max_attempts = MAX_LOGIN_ATTEMPTS,
                    "Register rejected — rate limited (too many attempts from this IP)"
                );
                return Err(AuthError::RateLimited(
                    "Too many attempts. Please wait a minute and try again.".into(),
                ));
            }
            let id = db::create_account(pool, &username, &password, email.as_deref()).await?;
            Ok(ServerAuthMsg::RegisterOk { account_id: id })
        }
        ClientAuthMsg::Login {
            username,
            password,
            client_version,
        } => {
            // Version check first — an outdated client is not a credential guess,
            // so it must not consume a rate-limit slot.
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
            // Brute-force gate: consume one attempt slot for this IP BEFORE the
            // expensive Argon2 verify — reserving at check time closes the
            // concurrent-burst TOCTOU. Applies to loopback too (so it's testable +
            // protected locally): a SUCCESSFUL login clears the IP, so dev / the
            // PD_DEV_CMDS relog loop (which logs in fine) never accumulates — only
            // a burst of BAD logins throttles, and it clears after the window.
            let now = Instant::now();
            if !limiter.try_acquire(client_ip, AuthKind::Login, now) {
                // INFO, not debug: a throttled IP is the brute-force signal an
                // operator needs to see in server.log on a hosted server. The
                // generic "auth error" line below is debug-level, so without this
                // the gate leaves no trace at all.
                tracing::info!(
                    ip = %client_ip,
                    window_secs = LOGIN_WINDOW.as_secs(),
                    max_attempts = MAX_LOGIN_ATTEMPTS,
                    "Login rejected — rate limited (too many attempts from this IP)"
                );
                return Err(AuthError::RateLimited(
                    "Too many login attempts. Please wait a minute and try again.".into(),
                ));
            }
            match db::verify_login(pool, &username, &password).await {
                Ok(outcome) => {
                    // Valid credentials: clear the IP's LOGIN budget so earlier
                    // fumbles by an honest user (or a NAT-mate) stop counting. Does
                    // NOT touch the Register budget.
                    limiter.clear(client_ip, AuthKind::Login);
                    let chars = db::list_characters(pool, outcome.account_id).await?;
                    Ok(ServerAuthMsg::LoginOk {
                        session_token: outcome.session_token_hex,
                        account_id: outcome.account_id,
                        is_gm: outcome.is_gm,
                        world_endpoint: cfg.world_endpoint.clone(),
                        characters: chars,
                    })
                }
                // Any failure keeps the slot already consumed at try_acquire
                // (wrong password, banned, etc.) — nothing more to record.
                Err(e) => Err(e),
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn try_acquire_caps_per_ip_window_and_clear_resets() {
        use AuthKind::Login;
        let lim = LoginRateLimiter::new();
        let ip: IpAddr = "1.2.3.4".parse().unwrap();
        // Base well in the future so `now - start` never underflows Instant.
        let t0 = Instant::now() + Duration::from_secs(3600);

        // MAX attempts consume their slots; the (MAX+1)th is refused. Because
        // consume is atomic, back-to-back calls at the SAME instant still cap at
        // MAX (this is what defeats the concurrent-burst TOCTOU).
        for i in 0..MAX_LOGIN_ATTEMPTS {
            assert!(lim.try_acquire(ip, Login, t0), "attempt {i} within budget");
        }
        assert!(!lim.try_acquire(ip, Login, t0), "over budget -> refused");
        // A different IP has its own budget.
        let ip2: IpAddr = "5.6.7.8".parse().unwrap();
        assert!(lim.try_acquire(ip2, Login, t0));
        // Window rollover frees the original IP.
        assert!(lim.try_acquire(ip, Login, t0 + LOGIN_WINDOW + Duration::from_secs(1)));
        // clear() (login success) frees the budget immediately.
        lim.clear(ip, Login);
        let t = t0 + LOGIN_WINDOW + Duration::from_secs(2);
        for _ in 0..MAX_LOGIN_ATTEMPTS {
            assert!(lim.try_acquire(ip, Login, t));
        }
        assert!(!lim.try_acquire(ip, Login, t), "cap re-applies after clear + reuse");
    }

    // The launcher auto-logs-in after each Register; a login success clears the
    // LOGIN budget but must NOT wipe the REGISTER budget, or Register never
    // throttles (playtest 2026-07-30: 8 accounts in 30s with no error).
    #[test]
    fn register_budget_survives_login_success_clear() {
        use AuthKind::{Login, Register};
        let lim = LoginRateLimiter::new();
        let ip: IpAddr = "9.9.9.9".parse().unwrap();
        let t = Instant::now() + Duration::from_secs(3600);
        // Simulate register-then-login-success MAX times: each register consumes a
        // Register slot; each login success clears only the Login budget.
        for i in 0..MAX_LOGIN_ATTEMPTS {
            assert!(lim.try_acquire(ip, Register, t), "register {i} within budget");
            assert!(lim.try_acquire(ip, Login, t)); // the auto-login attempt
            lim.clear(ip, Login); // login succeeded -> clears LOGIN only
        }
        // Register budget is now exhausted despite all the login-success clears.
        assert!(!lim.try_acquire(ip, Register, t), "register throttles at the cap");
        // And Login is still freely available (its budget was cleared each time).
        assert!(lim.try_acquire(ip, Login, t));
    }
}
