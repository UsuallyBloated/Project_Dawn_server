//! Auth WebSocket service.
//!
//! One Tokio task per connection. Reads JSON frames, dispatches to the DB
//! layer, writes JSON responses. No connection-level state — every
//! message except `Register`/`Login` carries its own `session_token`.
//!
//! Built to sit on a public port behind a TLS proxy (the client repo's
//! `docs/design/public_hosting_plan.md`, slice 1): the socket has size and
//! time limits and a per-address connection cap; the client address comes
//! from the proxy's `X-Forwarded-For` only when the TCP peer is loopback;
//! password guessing is metered per source (an IPv6 /64, not an address),
//! per account and in total; and registration can require an invite code.

use crate::{
    db,
    error::{AuthError, AuthResult},
    world, Config,
};
use futures_util::{SinkExt, StreamExt};
use protocol::auth::{ClientAuthMsg, ServerAuthMsg};
use sqlx::SqlitePool;
use std::collections::HashMap;
use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::{CloseFrame, WebSocketConfig};
use tokio_tungstenite::tungstenite::Message;

// ── Policy ──────────────────────────────────────────────────────────────────

/// The knobs that make the auth socket safe on a public port. Everything
/// here has a default an honest launcher never notices; tests shorten the
/// timeouts and tighten the caps to exercise them.
#[derive(Debug, Clone)]
pub struct AuthPolicy {
    /// `Register` must carry an unused invite code (`PROJECTDAWN_REQUIRE_INVITE=1`).
    /// Off by default so a dev box and the test harness register freely; the
    /// boot line reports it and a WARN fires when it is off, the same shape as
    /// `PD_DEV_CMDS` and `PD_NO_RATE_LIMIT`.
    pub require_invite: bool,
    /// A client that has not finished the WebSocket handshake by then is
    /// dropped, so a half-open connection cannot hold a task forever.
    pub handshake_timeout: Duration,
    /// A connection that sends nothing for this long is closed. The launcher
    /// reconnects on its next request (`_send_or_queue`), so a character
    /// select screen left open costs the player nothing.
    pub idle_timeout: Duration,
    /// Largest message and frame accepted. tungstenite's own default is 64 MB,
    /// which one client could ask the server to buffer per connection.
    pub max_message_bytes: usize,
    /// Open connections allowed per client address at once (counted after the
    /// handshake, by the address the limiter sees).
    pub max_conns_per_ip: u32,
}

impl Default for AuthPolicy {
    fn default() -> Self {
        Self {
            require_invite: false,
            handshake_timeout: Duration::from_secs(10),
            idle_timeout: Duration::from_secs(600),
            max_message_bytes: 64 * 1024,
            max_conns_per_ip: 8,
        }
    }
}

impl AuthPolicy {
    /// The production policy: the defaults, with the invite requirement read
    /// from the environment (`PROJECTDAWN_REQUIRE_INVITE`, on only when it is
    /// exactly `"1"`).
    pub fn from_env() -> Self {
        Self {
            require_invite: std::env::var("PROJECTDAWN_REQUIRE_INVITE").as_deref() == Ok("1"),
            ..Self::default()
        }
    }
}

// ── Auth-path rate limiting ─────────────────────────────────────────────────
// Brute-force / enumeration defense, three budgets checked under one lock:
//   - per SOURCE: an IPv4 address or an IPv6 /64 (`source_key`), per kind,
//     so an attacker throttles their own network and nobody else's;
//   - GLOBAL: every source together, per kind, so a spread of addresses (a
//     rotating /64 is a fresh per-source budget per attempt) still meets a
//     ceiling on the total guessing rate the server will do Argon2 work for;
//   - per ACCOUNT (Login only): guesses at one username from anywhere. This
//     one can be turned on a victim, which is why its budget is generous and
//     its window long; a successful login clears it.
// A slot is consumed per attempt at CHECK TIME (one atomic lock), so
// concurrent attempts can't all slip through before the count catches up,
// and a refused attempt consumes nothing. A successful LOGIN clears the
// source's Login budget and the account's, so an honest user's earlier
// fumbles don't linger.

/// Auth attempts allowed per source per `LOGIN_WINDOW` before further
/// attempts are rejected until the window rolls over.
const MAX_LOGIN_ATTEMPTS: u32 = 5;
/// The rolling window for `MAX_LOGIN_ATTEMPTS` and `MAX_GLOBAL_ATTEMPTS`.
const LOGIN_WINDOW: Duration = Duration::from_secs(60);
/// Login attempts allowed from ALL sources together per `LOGIN_WINDOW`: the
/// ceiling on the Argon2 work the server will do for strangers (ten verifies
/// a second is well under one core). A success refunds its slot, and a
/// source that recently logged in to the account is exempt (below), so the
/// cap can slow a credential-stuffing run without ever shutting a returning
/// player out. The review of 2026-10-12 found the first cut (120) was a
/// denial of service from two dozen addresses.
const MAX_GLOBAL_LOGIN_ATTEMPTS: u32 = 600;
/// Register attempts allowed from all sources together per `LOGIN_WINDOW`.
/// Invite-gated and low value, so it stays tight.
const MAX_GLOBAL_REGISTER_ATTEMPTS: u32 = 120;
/// Login attempts allowed against ONE account per `ACCOUNT_WINDOW`, from
/// anywhere. Twenty wrong passwords in ten minutes is not an honest evening;
/// the owner is never caught by it from the source they last logged in from
/// (`TRUSTED_SOURCE_TTL`), so a stranger guessing at their name cannot lock
/// them out, only themselves.
const MAX_ACCOUNT_ATTEMPTS: u32 = 20;
/// The rolling window for `MAX_ACCOUNT_ATTEMPTS`.
const ACCOUNT_WINDOW: Duration = Duration::from_secs(600);
/// How long the source of an account's last successful login is exempt from
/// the account and global caps. An attacker cannot forge a source from
/// outside (the forwarded address is believed only from the proxy), so the
/// exemption is worth exactly one thing: the owner, from their own network,
/// gets in while their name is under attack. The owner on a brand-new network
/// during an attack waits out the window like anyone else.
const TRUSTED_SOURCE_TTL: Duration = Duration::from_secs(24 * 3600);
/// Once this many keys are tracked in a map, drop expired windows on the next
/// attempt so it can't grow without bound if an attacker cycles sources or
/// names.
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

/// The budget key for a client address: the address itself for IPv4, the /64
/// for IPv6. An IPv6 host is usually handed a whole /64, so keying on the
/// full address would give an attacker a fresh budget per attempt for free.
pub fn source_key(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V4(_) => ip,
        IpAddr::V6(v6) => {
            let prefix = u128::from(v6) & !((1u128 << 64) - 1);
            IpAddr::V6(Ipv6Addr::from(prefix))
        }
    }
}

/// The client address the limiter and the logs should see. Behind the TLS
/// proxy (Caddy, on the same host) every TCP peer is loopback and the real
/// address rides `X-Forwarded-For`; the proxy appends the address it saw, so
/// the LAST entry is the one it vouches for. From any other peer the header
/// is whatever the client chose to send, so it is ignored: a forged header
/// from outside buys nothing. No header from loopback is the dev launcher on
/// the same box, which keeps its loopback address. Addresses are made
/// canonical first: on a dual-stack bind the proxy's IPv4 dial arrives as
/// `::ffff:127.0.0.1`, which is loopback in every sense but `Ipv6Addr`'s,
/// and left as is it would ignore the header and fold every player into
/// one `::/64` budget.
pub fn client_ip_for(peer: IpAddr, forwarded_for: Option<&str>) -> IpAddr {
    let peer = peer.to_canonical();
    if !peer.is_loopback() {
        return peer;
    }
    forwarded_for
        .and_then(|h| h.rsplit(',').next())
        .and_then(|last| last.trim().parse::<IpAddr>().ok())
        .map(|ip| ip.to_canonical())
        .unwrap_or(peer)
}

struct Window {
    count: u32,
    start: Instant,
}

impl Window {
    fn fresh(now: Instant) -> Self {
        Self { count: 0, start: now }
    }

    /// Restart the window when `len` has passed since it opened.
    fn roll(&mut self, now: Instant, len: Duration) {
        if now.duration_since(self.start) >= len {
            self.count = 0;
            self.start = now;
        }
    }
}

/// Which auth flow an attempt belongs to. Login and Register keep SEPARATE
/// budgets: a login success clears only the Login budget, so it can't
/// wipe an in-progress Register (enumeration) budget. Without this split, the
/// launcher's auto-login after a Register would clear the shared counter every
/// account, and Register would never throttle (playtest 2026-07-30).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum AuthKind {
    Login,
    Register,
}

/// Which budget refused an attempt. Logged so an operator can tell one
/// address hammering from a spread of them, and a spread from one account
/// under attack.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LimitHit {
    Source,
    Global,
    Account,
}

struct Limits {
    sources: HashMap<(IpAddr, AuthKind), Window>,
    global: HashMap<AuthKind, Window>,
    accounts: HashMap<String, Window>,
    /// Account -> the source key of its last successful login and when.
    trusted: HashMap<String, (IpAddr, Instant)>,
}

fn global_cap(kind: AuthKind) -> u32 {
    match kind {
        AuthKind::Login => MAX_GLOBAL_LOGIN_ATTEMPTS,
        AuthKind::Register => MAX_GLOBAL_REGISTER_ATTEMPTS,
    }
}

pub struct LoginRateLimiter {
    inner: Mutex<Limits>,
}

impl LoginRateLimiter {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Limits {
                sources: HashMap::new(),
                global: HashMap::new(),
                accounts: HashMap::new(),
                trusted: HashMap::new(),
            }),
        }
    }

    /// Try to consume one attempt slot for `(source, kind)`, the kind's global
    /// budget, and (Login only) the `account` budget. All three are checked
    /// and then all three consumed under a SINGLE lock, so N concurrent
    /// attempts can never all pass before the counts are bumped (the TOCTOU a
    /// check-then-increment design would have). A refusal consumes nothing.
    /// A source that logged in to `account` within `TRUSTED_SOURCE_TTL` is
    /// held only to its own source budget: the account and global caps exist
    /// to bound strangers, and must never be the thing that keeps the owner
    /// out.
    fn try_acquire(
        &self,
        source: IpAddr,
        kind: AuthKind,
        account: Option<&str>,
        now: Instant,
    ) -> Result<(), LimitHit> {
        // Single choke point for both the Login and Register gates, so the
        // kill switch belongs here rather than at the call sites.
        if rate_limit_disabled() {
            return Ok(());
        }
        let key = source_key(source);
        let account_key = account.map(|a| a.trim().to_ascii_lowercase());
        let mut l = self.inner.lock().expect("login limiter poisoned");
        if l.sources.len() >= RATE_LIMIT_PRUNE_AT {
            l.sources
                .retain(|_, w| now.duration_since(w.start) < LOGIN_WINDOW);
        }
        if l.accounts.len() >= RATE_LIMIT_PRUNE_AT {
            l.accounts
                .retain(|_, w| now.duration_since(w.start) < ACCOUNT_WINDOW);
        }
        if l.trusted.len() >= RATE_LIMIT_PRUNE_AT {
            l.trusted
                .retain(|_, (_, t)| now.duration_since(*t) < TRUSTED_SOURCE_TTL);
        }
        let trusted = account_key
            .as_ref()
            .and_then(|a| l.trusted.get(a))
            .is_some_and(|(k, t)| *k == key && now.duration_since(*t) < TRUSTED_SOURCE_TTL);

        let s = l
            .sources
            .entry((key, kind))
            .or_insert_with(|| Window::fresh(now));
        s.roll(now, LOGIN_WINDOW);
        if s.count >= MAX_LOGIN_ATTEMPTS {
            return Err(LimitHit::Source);
        }
        if !trusted {
            let g = l.global.entry(kind).or_insert_with(|| Window::fresh(now));
            g.roll(now, LOGIN_WINDOW);
            if g.count >= global_cap(kind) {
                return Err(LimitHit::Global);
            }
            if let Some(name) = &account_key {
                let a = l
                    .accounts
                    .entry(name.clone())
                    .or_insert_with(|| Window::fresh(now));
                a.roll(now, ACCOUNT_WINDOW);
                if a.count >= MAX_ACCOUNT_ATTEMPTS {
                    return Err(LimitHit::Account);
                }
            }
        }

        // Every budget has room: consume one slot of each that applies.
        if let Some(s) = l.sources.get_mut(&(key, kind)) {
            s.count += 1;
        }
        if !trusted {
            if let Some(g) = l.global.get_mut(&kind) {
                g.count += 1;
            }
            if let Some(name) = &account_key {
                if let Some(a) = l.accounts.get_mut(name) {
                    a.count += 1;
                }
            }
        }
        Ok(())
    }

    /// Clear `(source, kind)` and the account's budget after a success so a
    /// legit user isn't penalized for earlier fumbles, hand the global slot
    /// the attempt reserved back (honest logins never count toward the
    /// ceiling), and remember the source as the account's trusted one. Only
    /// ever called for `Login` — a Register success must NOT clear (its budget
    /// is what throttles enumeration), and clearing Login must not touch the
    /// Register budget (hence the per-kind key).
    fn clear(&self, source: IpAddr, kind: AuthKind, account: Option<&str>) {
        let key = source_key(source);
        let mut l = self.inner.lock().expect("login limiter poisoned");
        l.sources.remove(&(key, kind));
        if let Some(g) = l.global.get_mut(&kind) {
            g.count = g.count.saturating_sub(1);
        }
        if let Some(a) = account {
            let name = a.trim().to_ascii_lowercase();
            l.accounts.remove(&name);
            l.trusted.insert(name, (key, Instant::now()));
        }
    }
}

impl Default for LoginRateLimiter {
    fn default() -> Self {
        Self::new()
    }
}

// ── Connection accounting ───────────────────────────────────────────────────

/// What every connection task shares: the limiter, the per-address open
/// connection counts, and the policy.
struct Shared {
    limiter: LoginRateLimiter,
    conns: Mutex<HashMap<IpAddr, u32>>,
    policy: AuthPolicy,
}

/// One counted connection; the count drops with it, on every exit path.
struct ConnSlot {
    shared: Arc<Shared>,
    key: IpAddr,
}

impl Drop for ConnSlot {
    fn drop(&mut self) {
        let mut m = self.shared.conns.lock().expect("conn counts poisoned");
        if let Some(n) = m.get_mut(&self.key) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                m.remove(&self.key);
            }
        }
    }
}

/// Count a connection from `ip` against `max_conns_per_ip`, keyed like the
/// limiter (an IPv6 /64). `None` when the address already holds the cap.
fn acquire_conn(shared: &Arc<Shared>, ip: IpAddr) -> Option<ConnSlot> {
    let key = source_key(ip);
    let mut m = shared.conns.lock().expect("conn counts poisoned");
    let n = m.entry(key).or_insert(0);
    if *n >= shared.policy.max_conns_per_ip {
        return None;
    }
    *n += 1;
    Some(ConnSlot {
        shared: shared.clone(),
        key,
    })
}

// ── Service ─────────────────────────────────────────────────────────────────

pub async fn serve(cfg: Arc<Config>, pool: SqlitePool) -> anyhow::Result<()> {
    serve_with_policy(cfg, pool, AuthPolicy::from_env()).await
}

pub async fn serve_with_policy(
    cfg: Arc<Config>,
    pool: SqlitePool,
    policy: AuthPolicy,
) -> anyhow::Result<()> {
    let listener = TcpListener::bind(&cfg.auth_bind).await?;
    let actual = listener.local_addr()?;
    tracing::info!(addr = %actual, require_invite = policy.require_invite, "auth WS listening");

    // Precompute the auth-timing equalizer hash now so no live request pays the
    // one-time init (see db::verify_login's enumeration defense).
    db::warm_login_timing_defense();

    let shared = Arc::new(Shared {
        limiter: LoginRateLimiter::new(),
        conns: Mutex::new(HashMap::new()),
        policy,
    });
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
        let shared = shared.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_connection(stream, peer, cfg, pool, shared).await {
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
    serve_bound_with_policy(cfg, pool, AuthPolicy::default()).await
}

/// `serve_bound` with an explicit policy, for the tests that exercise the
/// socket limits and the invite gate.
pub async fn serve_bound_with_policy(
    cfg: Arc<Config>,
    pool: SqlitePool,
    policy: AuthPolicy,
) -> anyhow::Result<(std::net::SocketAddr, tokio::task::JoinHandle<()>)> {
    let listener = TcpListener::bind(&cfg.auth_bind).await?;
    let addr = listener.local_addr()?;
    db::warm_login_timing_defense();
    let shared = Arc::new(Shared {
        limiter: LoginRateLimiter::new(),
        conns: Mutex::new(HashMap::new()),
        policy,
    });
    let handle = tokio::spawn(async move {
        loop {
            let (stream, peer) = match listener.accept().await {
                Ok(p) => p,
                Err(_) => continue,
            };
            let cfg = cfg.clone();
            let pool = pool.clone();
            let shared = shared.clone();
            tokio::spawn(async move {
                let _ = handle_connection(stream, peer, cfg, pool, shared).await;
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
    shared: Arc<Shared>,
) -> anyhow::Result<()> {
    let policy = shared.policy.clone();

    // The handshake callback sees the HTTP headers; it records the forwarded
    // address and nothing else (whether to believe it is decided below, from
    // the TCP peer, which no header can forge).
    let forwarded: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let seen = forwarded.clone();
    let callback = move |req: &Request, resp: Response| {
        if let Some(v) = req
            .headers()
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok())
        {
            *seen.lock().expect("forwarded-for poisoned") = Some(v.to_string());
        }
        Ok(resp)
    };
    let config = WebSocketConfig {
        max_message_size: Some(policy.max_message_bytes),
        max_frame_size: Some(policy.max_message_bytes),
        ..Default::default()
    };
    let ws = match tokio::time::timeout(
        policy.handshake_timeout,
        tokio_tungstenite::accept_hdr_async_with_config(stream, callback, Some(config)),
    )
    .await
    {
        Ok(Ok(ws)) => ws,
        Ok(Err(e)) => return Err(e.into()),
        Err(_) => {
            tracing::debug!(%peer, "ws handshake timed out");
            return Ok(());
        }
    };
    let header = forwarded.lock().expect("forwarded-for poisoned").take();
    // Key the login rate limiter on the address only — the ephemeral source
    // port changes every connection, so keying on the full SocketAddr would
    // defeat it.
    let client_ip = client_ip_for(peer.ip(), header.as_deref());
    let (mut write, mut read) = ws.split();

    let Some(_slot) = acquire_conn(&shared, client_ip) else {
        // INFO: a pile of open connections from one address is the slow-loris
        // signal an operator needs to see.
        tracing::info!(
            ip = %client_ip,
            max = policy.max_conns_per_ip,
            "auth connection refused — too many open connections from this address"
        );
        let _ = tokio::time::timeout(
            policy.idle_timeout,
            write.send(Message::Close(Some(CloseFrame {
                code: CloseCode::Policy,
                reason: "too many connections".into(),
            }))),
        )
        .await;
        return Ok(());
    };
    tracing::debug!(%peer, ip = %client_ip, "ws upgraded");

    loop {
        let frame = match tokio::time::timeout(policy.idle_timeout, read.next()).await {
            Err(_) => {
                tracing::debug!(%peer, "ws idle timeout");
                let _ = tokio::time::timeout(
                    policy.idle_timeout,
                    write.send(Message::Close(Some(CloseFrame {
                        code: CloseCode::Normal,
                        reason: "idle".into(),
                    }))),
                )
                .await;
                break;
            }
            Ok(None) => break,
            Ok(Some(Ok(f))) => f,
            Ok(Some(Err(e))) => {
                // Includes a message over the size cap: tungstenite refuses it
                // as a capacity error and the connection ends here.
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
                send_bounded(&mut write, Message::Pong(p), policy.idle_timeout).await?;
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

        let response = dispatch(&cfg, &pool, msg, client_ip, &shared).await;
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

        send_bounded(
            &mut write,
            Message::Text(serde_json::to_string(&response)?),
            policy.idle_timeout,
        )
        .await?;
    }

    tracing::debug!(%peer, "connection closed");
    Ok(())
}

/// A send that gives up after `limit`. The idle timeout bounds the READ side;
/// without this a client that stops reading (the proxy passes the
/// backpressure through) parks the task in `flush` forever and keeps its
/// connection slot.
async fn send_bounded<S>(sink: &mut S, msg: Message, limit: Duration) -> anyhow::Result<()>
where
    S: SinkExt<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
{
    match tokio::time::timeout(limit, sink.send(msg)).await {
        Ok(r) => Ok(r?),
        Err(_) => Err(anyhow::anyhow!("send timed out: the client stopped reading")),
    }
}

/// INFO, not debug: a throttled source is the brute-force signal an operator
/// needs to see in server.log on a hosted server. The generic "auth error"
/// line is debug-level, so without this the gate leaves no trace at all.
fn log_throttle(client_ip: IpAddr, kind: AuthKind, hit: LimitHit) {
    tracing::info!(
        ip = %client_ip,
        source_key = %source_key(client_ip),
        kind = ?kind,
        budget = ?hit,
        window_secs = LOGIN_WINDOW.as_secs(),
        "auth attempt rejected — rate limited"
    );
}

async fn dispatch(
    cfg: &Config,
    pool: &SqlitePool,
    msg: ClientAuthMsg,
    client_ip: IpAddr,
    shared: &Shared,
) -> AuthResult<ServerAuthMsg> {
    let limiter = &shared.limiter;
    match msg {
        ClientAuthMsg::Register {
            username,
            password,
            email,
            invite,
        } => {
            // Same gate as Login: NameTaken vs RegisterOk is a username
            // existence oracle, so Register must be throttled too or it negates
            // the Login-path enumeration defense. No clear on success — a Register
            // success must NOT reset the budget (junk-registering would give an
            // attacker a way to keep enumerating).
            let now = Instant::now();
            if let Err(hit) = limiter.try_acquire(client_ip, AuthKind::Register, None, now) {
                log_throttle(client_ip, AuthKind::Register, hit);
                return Err(AuthError::RateLimited(
                    "Too many attempts. Please wait a minute and try again.".into(),
                ));
            }
            // An invite, when given, is checked and burned in the same
            // transaction that creates the account (`create_account_with_invite`),
            // so one code makes one account even when two registrations race,
            // and a refused name leaves the code unspent. When the policy
            // requires one, a missing code is refused before any DB work.
            let invite = invite
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty());
            let id = match invite {
                Some(code) => {
                    db::create_account_with_invite(
                        pool,
                        &username,
                        &password,
                        email.as_deref(),
                        code,
                    )
                    .await?
                }
                None if shared.policy.require_invite => {
                    return Err(AuthError::InviteInvalid(
                        "An invite code is required to register.".into(),
                    ));
                }
                None => db::create_account(pool, &username, &password, email.as_deref()).await?,
            };
            Ok(ServerAuthMsg::RegisterOk { account_id: id })
        }
        ClientAuthMsg::Login {
            username,
            password,
            client_version,
        } => {
            // Version check first — an outdated client is not a credential guess,
            // so it must not consume a rate-limit slot. The echo is cut short:
            // a reply must never mirror a client-sized string back.
            let cv = crate::config::semver::Version::parse(&client_version)
                .ok_or_else(|| AuthError::InvalidInput(format!(
                    "client_version is not semver: {}",
                    client_version.chars().take(32).collect::<String>()
                )))?;
            // A name no account could have is refused before it becomes a key
            // in the account budget (the map is pruned by time, not size, and
            // the string came from a stranger). A format check, not an
            // existence oracle: every account name passed the same rule.
            if username.len() > db::USERNAME_MAX {
                return Err(AuthError::InvalidInput("username too long".into()));
            }
            if !cv.at_least(&cfg.min_client_version) {
                return Err(AuthError::VersionMismatch {
                    client: client_version,
                    required: cfg.min_client_version.to_string(),
                });
            }
            // Brute-force gate: consume one attempt slot BEFORE the expensive
            // Argon2 verify — reserving at check time closes the concurrent-burst
            // TOCTOU. Applies to loopback too (so it's testable + protected
            // locally): a SUCCESSFUL login clears the source and the account, so
            // dev / the PD_DEV_CMDS relog loop (which logs in fine) never
            // accumulates — only a burst of BAD logins throttles, and it clears
            // after the window.
            let now = Instant::now();
            if let Err(hit) =
                limiter.try_acquire(client_ip, AuthKind::Login, Some(&username), now)
            {
                log_throttle(client_ip, AuthKind::Login, hit);
                return Err(AuthError::RateLimited(
                    "Too many login attempts. Please wait a minute and try again.".into(),
                ));
            }
            match db::verify_login(pool, &username, &password).await {
                Ok(outcome) => {
                    // Valid credentials: clear the source's LOGIN budget and the
                    // account's so earlier fumbles by an honest user (or a
                    // NAT-mate) stop counting. Does NOT touch the Register budget.
                    limiter.clear(client_ip, AuthKind::Login, Some(&username));
                    let chars = db::list_characters(pool, outcome.account_id).await?;
                    Ok(ServerAuthMsg::LoginOk {
                        session_token: outcome.session_token_hex,
                        account_id: outcome.account_id,
                        is_gm: outcome.is_gm,
                        world_endpoint: cfg.world_endpoint.clone(),
                        characters: chars,
                    })
                }
                // Any failure keeps the slots already consumed at try_acquire
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
            assert!(lim.try_acquire(ip, Login, None, t0).is_ok(), "attempt {i} within budget");
        }
        assert_eq!(lim.try_acquire(ip, Login, None, t0), Err(LimitHit::Source));
        // A different IP has its own budget.
        let ip2: IpAddr = "5.6.7.8".parse().unwrap();
        assert!(lim.try_acquire(ip2, Login, None, t0).is_ok());
        // Window rollover frees the original IP.
        assert!(lim
            .try_acquire(ip, Login, None, t0 + LOGIN_WINDOW + Duration::from_secs(1))
            .is_ok());
        // clear() (login success) frees the budget immediately.
        lim.clear(ip, Login, None);
        let t = t0 + LOGIN_WINDOW + Duration::from_secs(2);
        for _ in 0..MAX_LOGIN_ATTEMPTS {
            assert!(lim.try_acquire(ip, Login, None, t).is_ok());
        }
        assert_eq!(
            lim.try_acquire(ip, Login, None, t),
            Err(LimitHit::Source),
            "cap re-applies after clear + reuse"
        );
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
            assert!(lim.try_acquire(ip, Register, None, t).is_ok(), "register {i} within budget");
            assert!(lim.try_acquire(ip, Login, Some("someone"), t).is_ok()); // the auto-login
            lim.clear(ip, Login, Some("someone")); // login succeeded -> clears LOGIN only
        }
        // Register budget is now exhausted despite all the login-success clears.
        assert_eq!(lim.try_acquire(ip, Register, None, t), Err(LimitHit::Source));
        // And Login is still freely available (its budget was cleared each time).
        assert!(lim.try_acquire(ip, Login, Some("someone"), t).is_ok());
    }

    /// An IPv6 host usually holds a /64, so two addresses inside one /64 must
    /// draw on ONE budget; the neighbouring /64 is a different source.
    #[test]
    fn ipv6_addresses_in_one_64_share_a_budget() {
        use AuthKind::Login;
        let lim = LoginRateLimiter::new();
        let t = Instant::now() + Duration::from_secs(3600);
        let a: IpAddr = "2001:db8:1:2::1".parse().unwrap();
        let b: IpAddr = "2001:db8:1:2:ffff:ffff:ffff:ffff".parse().unwrap();
        let other: IpAddr = "2001:db8:1:3::1".parse().unwrap();
        assert_eq!(source_key(a), source_key(b));
        assert_ne!(source_key(a), source_key(other));
        for _ in 0..MAX_LOGIN_ATTEMPTS {
            assert!(lim.try_acquire(a, Login, None, t).is_ok());
        }
        assert_eq!(
            lim.try_acquire(b, Login, None, t),
            Err(LimitHit::Source),
            "a fresh address in the same /64 is not a fresh budget"
        );
        assert!(lim.try_acquire(other, Login, None, t).is_ok(), "the next /64 is");
        // IPv4 keys on the address itself.
        let v4: IpAddr = "203.0.113.7".parse().unwrap();
        assert_eq!(source_key(v4), v4);
    }

    /// A spread of sources, each under its own cap, still meets the global
    /// ceiling; a success hands its slot back, so honest logins never count.
    #[test]
    fn the_global_cap_refuses_a_spread_of_sources() {
        use AuthKind::Login;
        let lim = LoginRateLimiter::new();
        let t = Instant::now() + Duration::from_secs(3600);
        for i in 0..MAX_GLOBAL_LOGIN_ATTEMPTS {
            // One attempt per address: every source budget stays at 1 of 5.
            let ip: IpAddr = format!("10.{}.{}.{}", i / 65536, (i / 256) % 256, i % 256)
                .parse()
                .unwrap();
            assert!(lim.try_acquire(ip, Login, None, t).is_ok(), "attempt {i}");
        }
        let fresh: IpAddr = "192.0.2.1".parse().unwrap();
        assert_eq!(lim.try_acquire(fresh, Login, None, t), Err(LimitHit::Global));
        // One of the earlier attempts succeeds: its slot comes back, and only its.
        let earlier: IpAddr = "10.0.0.0".parse().unwrap();
        lim.clear(earlier, Login, Some("someone"));
        assert!(lim.try_acquire(fresh, Login, None, t).is_ok(), "the refunded slot");
        assert_eq!(lim.try_acquire(fresh, Login, None, t), Err(LimitHit::Global));
        // Register has its own, tighter global budget.
        for i in 0..MAX_GLOBAL_REGISTER_ATTEMPTS {
            let ip: IpAddr = format!("10.9.{}.{}", (i / 256) % 256, i % 256).parse().unwrap();
            assert!(lim.try_acquire(ip, AuthKind::Register, None, t).is_ok(), "register {i}");
        }
        assert_eq!(lim.try_acquire(fresh, AuthKind::Register, None, t), Err(LimitHit::Global));
        // The window rolls over like the others.
        assert!(lim
            .try_acquire(fresh, Login, None, t + LOGIN_WINDOW + Duration::from_secs(1))
            .is_ok());
    }

    /// The account and global caps bound strangers; the owner, from the source
    /// they last logged in from, is held only to their own source budget, so
    /// a stranger guessing at their name (finding 1 of the 2026-10-12 review)
    /// or a spread of strangers exhausting the global ceiling (finding 2)
    /// cannot keep them out.
    #[test]
    fn a_recent_successful_source_is_exempt_from_the_account_and_global_caps() {
        use AuthKind::Login;
        let lim = LoginRateLimiter::new();
        let t = Instant::now() + Duration::from_secs(3600);
        let home: IpAddr = "198.51.100.9".parse().unwrap();
        // The owner logged in from home.
        assert!(lim.try_acquire(home, Login, Some("Victim"), t).is_ok());
        lim.clear(home, Login, Some("Victim"));
        // Strangers spend the account budget on the name.
        for i in 0..MAX_ACCOUNT_ATTEMPTS {
            let ip: IpAddr = format!("203.0.113.{}", i + 1).parse().unwrap();
            assert!(lim.try_acquire(ip, Login, Some("victim"), t).is_ok(), "guess {i}");
        }
        let stranger: IpAddr = "203.0.113.200".parse().unwrap();
        assert_eq!(lim.try_acquire(stranger, Login, Some("victim"), t), Err(LimitHit::Account));
        // The owner from home still gets through.
        assert!(lim.try_acquire(home, Login, Some("VICTIM"), t).is_ok());
        // Even with the global ceiling spent by a crowd.
        for i in 0..MAX_GLOBAL_LOGIN_ATTEMPTS {
            let ip: IpAddr = format!("10.{}.{}.{}", i / 65536, (i / 256) % 256, i % 256)
                .parse()
                .unwrap();
            let _ = lim.try_acquire(ip, Login, None, t);
        }
        assert_eq!(lim.try_acquire(stranger, Login, Some("bystander"), t), Err(LimitHit::Global));
        assert!(lim.try_acquire(home, Login, Some("victim"), t).is_ok(), "home is exempt");
        // The exemption is per account: home gets no pass on another name.
        assert_eq!(
            lim.try_acquire(home, Login, Some("bystander"), t),
            Err(LimitHit::Global),
            "trust is for the account that logged in from here, not for the address"
        );
        // But the owner's own source budget still applies to them.
        for _ in 0..MAX_LOGIN_ATTEMPTS {
            let _ = lim.try_acquire(home, Login, Some("victim"), t);
        }
        assert_eq!(lim.try_acquire(home, Login, Some("victim"), t), Err(LimitHit::Source));
        // And it expires: a day on, with the global ceiling spent again by a
        // crowd, home is held to it like anyone.
        let stale = t + TRUSTED_SOURCE_TTL + Duration::from_secs(1);
        for i in 0..MAX_GLOBAL_LOGIN_ATTEMPTS {
            let ip: IpAddr = format!("10.{}.{}.{}", i / 65536, (i / 256) % 256, i % 256)
                .parse()
                .unwrap();
            let _ = lim.try_acquire(ip, Login, None, stale);
        }
        assert_eq!(lim.try_acquire(home, Login, Some("victim"), stale), Err(LimitHit::Global));
    }

    /// Guesses at ONE account from many sources meet the account budget; the
    /// name is matched case-insensitively (usernames are NOCASE in the DB);
    /// the owner's successful login clears it.
    #[test]
    fn an_account_budget_caps_guesses_at_one_name_and_clears_on_success() {
        use AuthKind::Login;
        let lim = LoginRateLimiter::new();
        let t = Instant::now() + Duration::from_secs(3600);
        for i in 0..MAX_ACCOUNT_ATTEMPTS {
            // A new source every time, so only the account budget can refuse.
            let ip: IpAddr = format!("198.51.100.{}", i + 1).parse().unwrap();
            let name = if i % 2 == 0 { "Victim" } else { "victim" };
            assert!(lim.try_acquire(ip, Login, Some(name), t).is_ok(), "guess {i}");
        }
        let next: IpAddr = "198.51.100.200".parse().unwrap();
        assert_eq!(lim.try_acquire(next, Login, Some("VICTIM"), t), Err(LimitHit::Account));
        // Someone else's account is untouched.
        assert!(lim.try_acquire(next, Login, Some("bystander"), t).is_ok());
        // The owner logs in (from anywhere): the account budget clears, and
        // that source becomes the account's trusted one (exempt from the cap,
        // tested on its own below).
        lim.clear(next, Login, Some("victim"));
        assert!(lim.try_acquire(next, Login, Some("victim"), t).is_ok());
        // And the window is the long one: a minute later the source windows
        // have rolled over, the account window has not.
        let later = t + LOGIN_WINDOW + Duration::from_secs(1);
        // The trusted source consumed no account slot, so the strangers get the full twenty.
        for i in 0..MAX_ACCOUNT_ATTEMPTS {
            let ip: IpAddr = format!("198.51.100.{}", i + 1).parse().unwrap();
            assert!(lim.try_acquire(ip, Login, Some("victim"), later).is_ok());
        }
        let untrusted: IpAddr = "198.51.100.201".parse().unwrap();
        assert_eq!(
            lim.try_acquire(untrusted, Login, Some("victim"), later),
            Err(LimitHit::Account),
            "the account window is longer than the source window"
        );
        assert!(lim
            .try_acquire(untrusted, Login, Some("victim"), later + ACCOUNT_WINDOW + Duration::from_secs(1))
            .is_ok());
    }

    /// The forwarded address is believed from loopback (the TLS proxy on the
    /// same host) and from nowhere else; the proxy's own entry is the last.
    #[test]
    fn forwarded_for_is_trusted_only_from_loopback() {
        let lo: IpAddr = "127.0.0.1".parse().unwrap();
        let lo6: IpAddr = "::1".parse().unwrap();
        let outside: IpAddr = "203.0.113.5".parse().unwrap();
        let real: IpAddr = "198.51.100.9".parse().unwrap();
        assert_eq!(client_ip_for(lo, Some("198.51.100.9")), real);
        assert_eq!(client_ip_for(lo6, Some(" 198.51.100.9 ")), real);
        // A client-supplied prefix is ignored: the proxy appends what it saw.
        assert_eq!(client_ip_for(lo, Some("1.1.1.1, 198.51.100.9")), real);
        // Garbage falls back to the peer.
        assert_eq!(client_ip_for(lo, Some("not an address")), lo);
        assert_eq!(client_ip_for(lo, None), lo);
        // From outside the header is whatever the client wrote: ignored.
        assert_eq!(client_ip_for(outside, Some("198.51.100.9")), outside);
        // On a dual-stack bind the proxy's dial is IPv4-mapped loopback: still
        // loopback, and the forwarded address comes back canonical.
        let mapped_lo: IpAddr = "::ffff:127.0.0.1".parse().unwrap();
        assert_eq!(client_ip_for(mapped_lo, Some("::ffff:198.51.100.9")), real);
        assert_eq!(client_ip_for(mapped_lo, None), lo);
    }
}
