# Deploying the Project Dawn server on Linux (Phase 2)

Target: a dedicated Linux box on your Tailscale tailnet. The friend connects over
WireGuard, so nothing is exposed to the open internet and the cleartext `ws://` auth
socket is encrypted in transit by the tailnet.

Written 2026-08-05 from source. Every gotcha below was verified, not assumed.

---

## 0. What actually has to be true

Three things, and only three, decide whether a remote player can connect:

1. `PROJECTDAWN_WORLD_ENDPOINT` is the **server's tailnet IP** as a literal `IP:port`.
2. `PROJECTDAWN_NETCODE_KEY` is set (64 hex chars). The server refuses to boot without it.
3. TCP **8765** and UDP **7777** reach the box.

Everything else is comfort and safety.

---

## 1. Prerequisites on the box

```bash
sudo apt update
sudo apt install -y build-essential pkg-config git sqlite3 curl
# Rust (the repo pins 1.95.0 via rust-toolchain.toml; rustup honours it automatically)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
. "$HOME/.cargo/env"
```

`build-essential` is not optional: `sqlx`'s sqlite feature compiles SQLite from C source,
so a box without a C toolchain fails the build with a confusing `cc` error.

Tailscale:

```bash
curl -fsSL https://tailscale.com/install.sh | sh
sudo tailscale up
tailscale ip -4          # <-- this address is PROJECTDAWN_WORLD_ENDPOINT's host part
```

Clock: **verify NTP is running.** ConnectTokens live 30 s and the *client* validates them
against its own clock. A box more than ~30 s off makes login succeed and then the world
handshake fail — which looks exactly like a netcode bug.

```bash
timedatectl status | grep -E 'NTP|synchronized'
```

## 2. Build

```bash
sudo useradd --system --create-home --home-dir /opt/projectdawn projectdawn
sudo -u projectdawn -H bash
cd /opt/projectdawn
git clone <your-server-remote> src
cd src
cargo build --release            # first build takes a few minutes
cp target/release/projectdawn-server /opt/projectdawn/
```

Use the **release** binary. `scripts/run-server.ps1` and `dev-run.sh` both use `cargo run`,
which is a debug build: fine for the dev loop, needlessly slow for a host.

## 3. Configure

Create `/opt/projectdawn/.env` (owned by the `projectdawn` user, mode `600` — it holds the
netcode key):

```bash
# Mint a FRESH key for the host. 64 hex chars = 32 bytes.
openssl rand -hex 32
```

```ini
# /opt/projectdawn/.env
PROJECTDAWN_NETCODE_KEY=<the 64 hex chars you just generated>
PROJECTDAWN_WORLD_ENDPOINT=100.x.y.z:7777
PROJECTDAWN_DATABASE_URL=sqlite:///opt/projectdawn/world.db?mode=rwc
RUST_LOG=projectdawn_server=info,sqlx=warn
```

```bash
chmod 600 /opt/projectdawn/.env
```

### The four ways this file bites

- **`world_endpoint` is the address the friend's game actually dials.** It is signed *into*
  the ConnectToken; the client's CLI arg for it is explicitly discarded. Leave it at the
  default `127.0.0.1:7777` and the friend's client dials *its own loopback*: login,
  character list and character select all work perfectly, then Play hangs. It looks like a
  world-server bug and is a config bug.
- **It must be an `IP:port` literal.** It is parsed with `SocketAddr`, which does no DNS, so
  a hostname or DDNS name is a hard boot failure — and it fails *after* the auth socket is
  already listening, so the log shows two healthy startup lines and then the process exits.
- **Nothing validates reachability.** A stale tailnet IP parses and boots cleanly, then
  silently breaks every remote client. **The boot log line is your only confirmation.**
- **Never put `PD_DEV_CMDS` in `.env`.** The app's own loader would re-enable dev commands
  for *every* connected player (self-heal, mob spawning, instant levels). systemd already
  gives the service a clean environment; keep it that way.

Mint a fresh key rather than copying the dev one. Anyone holding the key can forge
ConnectTokens the world server accepts, **including the `is_gm` bit**. Back the key up
somewhere you will still have it after a rebuild: `.env` is gitignored, so a naive redeploy
leaves the server refusing to boot.

## 4. Fresh database + your GM account

Phase 2 decision: start clean. Migrations auto-apply on first boot, so just let the server
create `world.db`, then:

1. Start the server (section 5).
2. From the game client, **Register** your own account.
3. Grant yourself GM:
   ```bash
   cd /opt/projectdawn/src
   cargo run --release -p projectdawn-server --bin grant_gm -- <your-username> on
   cargo run --release -p projectdawn-server --bin grant_gm          # no args = audit the list
   ```
   `grant_gm` takes effect on that account's **next world login**, not immediately.

Grant it to exactly one account and audit with the no-args form before inviting anyone.

## 5. Run it

```bash
sudo cp src/scripts/projectdawn.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now projectdawn
journalctl -u projectdawn -f
```

**Check the boot line every single start.** It is the only place several silent
misconfigurations become visible:

```
starting projectdawn-server auth_bind=0.0.0.0:8765 world_bind=0.0.0.0:7777
  world_endpoint=100.x.y.z:7777 db=sqlite:///opt/projectdawn/world.db
  min_client=0.1.0 dev_cmds=false
```

- `world_endpoint` must be the **tailnet** IP, not `127.0.0.1`.
- `dev_cmds` must be **false**. If it says `true`, every player has dev commands.

## 6. Network

```bash
sudo ufw allow in on tailscale0 to any port 8765 proto tcp
sudo ufw allow in on tailscale0 to any port 7777 proto udp
```

Binding is IPv4 (`0.0.0.0`) for both. **UDP failures are silent** — if the friend can log in
but Play times out, suspect UDP 7777 before anything else.

## 7. Stopping safely (read this before your first restart)

**The server has no SIGTERM handler.** `systemctl restart` or `stop` discards up to
`CHECKPOINT_INTERVAL` (60 s) of position, HP/MP, XP, inventory and coins **for every player
online**. A clean player-side logout flushes immediately.

**Operational rule: get everyone to log out, then stop.** Inventory, quests and bank
transactions write per-mutation and are safe; it is the 60 s checkpoint state that is at
risk.

## 8. Backups

`scripts/backup.sh` uses `sqlite3 .backup`, which is safe against a live database, and
prunes on a retention window. Its paths are env-overridable:

```bash
sudo mkdir -p /var/lib/projectdawn/backups
sudo chown projectdawn:projectdawn /var/lib/projectdawn/backups
# as the projectdawn user, nightly at 04:00
crontab -e
0 4 * * * PROJECTDAWN_DB=/opt/projectdawn/world.db \
          PROJECTDAWN_BACKUP_DIR=/var/lib/projectdawn/backups \
          /opt/projectdawn/src/scripts/backup.sh
```

Verify a restore once, before you need it: `sqlite3 world-<ts>.db "SELECT COUNT(*) FROM accounts;"`

## 9. Known operational gaps (accepted for the friends build)

- **No kick, no ban, no shutdown warning.** `accounts.is_banned` is read at login but
  nothing writes it. Removing a disruptive player means stopping the server and running SQL,
  and even then it only blocks the next *login* — the live session keeps playing. There is a
  written plan for the tooling in `docs/session_notes/handoff_account_admin.md` (client repo).
- **The DB is not in WAL mode** despite two code comments claiming otherwise (verified:
  `journal_mode=delete`). Running `admin_report` against a live server can block its commits
  for up to the 5 s busy timeout. Switching to WAL is a cheap hardening but needs an
  exclusive lock — do it while stopped.
- **Log rotation:** the journal handles it here (unlike the Windows wrapper, which
  accumulates `logs/server_<timestamp>.log` forever). Cap it if the disk is small:
  `sudo journalctl --vacuum-time=30d`.
- **`server/README.md` is badly stale** — it claims the world simulation is "not yet
  implemented" and tells you to promote a GM with raw SQL. Trust `config.rs` and this file.

## 10. Smoke test before inviting anyone

From a machine that is **not** the server and **not** your dev box, on the tailnet:

1. Launch `ProjectDawn.exe`, type the server's tailnet address in the Server field
   (`100.x.y.z:8765`), Register a throwaway account.
2. Create a character (letters only in the name — digits and spaces are rejected).
3. Play. If login works but this hangs, it is `world_endpoint` or UDP 7777.
4. Kill something.
5. Log out cleanly (not the window X — that is a deliberate crash simulation that leaves you
   linkdead for ~30 s and refuses your own relogin during that window).

That sequence is the phase's definition of done, rehearsed with a known-good tester.
