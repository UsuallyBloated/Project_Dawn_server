# Project Dawn Server

Authoritative server for Project Dawn. See `docs/server_design.md` for the
full architecture contract.

## Quick start

First time on a clean clone (Windows / PowerShell or any POSIX shell):

```sh
cd F:\Projects\server
cp .env.example .env
# Set PROJECTDAWN_NETCODE_KEY in .env: 64 hex chars, e.g. `openssl rand -hex 32`
# (PowerShell: (1..32 | ForEach { '{0:x2}' -f (Get-Random -Max 256) }) -join '').
# The server refuses to start without it.
cargo run -p projectdawn-server
```

`rustup` auto-installs the pinned 1.95.0 toolchain on first build, `sqlx`
auto-applies migrations on first boot, and the auth service starts listening
on `0.0.0.0:8765`. For a playtest with the Test Panel's dev tools, run with
`PD_DEV_CMDS=1` (see the client repo's `CLAUDE.md`, "Commands").

To run the test suite (~260 unit + ~90 integration, under a minute including build):

```sh
cargo test
```

## Status

Alpha, and **hosted**: a friends build runs on a physical server reachable over
a private Tailscale tailnet. See `docs/deployment_linux.md` here, and
`docs/deployment/server_operations.md` in the client repo for running it.

This crate provides both halves:

- **Auth WebSocket** (`0.0.0.0:8765`): `Register`, `Login`, `CharList`,
  `CharCreate`, `CharDelete`, `Logout`, with per-IP rate limiting and Argon2
  timing equalization.
- **World UDP simulation** (renet, 20 Hz, `0.0.0.0:7777`): server-authoritative
  movement, combat, regen, enemy AI, pets, inventory and equipment, four-tier
  currency and coin loot, group state and loot rights, passive skills, XP and
  leveling, player corpses and resurrection, and quests. The wire protocol lives
  in the `protocol` crate; the Godot client bridges it through the `gdext-net`
  GDExtension, which shares that crate.

The architecture contract is `docs/server_design.md`. What exists and how it
behaves is catalogued in the client repo's
`docs/concepts/architecture/systems_overview.md`.

> This section claimed the world server did not exist for months after it
> shipped. If you change what the crate does, change it here too.

## Build

```sh
cargo build --release
```

Toolchain is pinned to **Rust 1.95.0** via `rust-toolchain.toml`; rustup
will install it on first build if missing.

## Run

```sh
cp .env.example .env       # adjust to taste
cargo run -p projectdawn-server
```

The auth service binds `0.0.0.0:8765` by default. SQLite database is
created at the path in `PROJECTDAWN_DATABASE_URL` (default `world.db`)
and migrations under `migrations/` apply automatically on boot.

## Test

```sh
cargo test
```

Integration tests under `tests/` spin up an in-process auth server AND world
server on ephemeral ports. `world_two_clients.rs` drives real renet clients
through the wire protocol and covers combat, pets, inventory, vendors, quests,
groups, corpses, trading, and the exploit gates.

If one fails, re-run it alone: a failure that reproduces **in isolation** is
real; one that passes alone is load-sensitivity
(`docs/flaky_integration_tests.md`).

## Ops tools

Five binaries ship from this crate, so `--bin` selects; a bare
`cargo run -p projectdawn-server` resolves to the server itself via the
`default-run` manifest key.

| Binary | Writes? | Purpose |
|---|---|---|
| `projectdawn-server` | — | the server (default) |
| `admin_report` | no | `world.db` summary to console + `world_report.html`, including the newest GM action audit rows |
| `grant_gm` | yes | set a per-account GM flag; no args lists accounts |
| `reset_password` | yes | reset a locked-out account's password and purge its sessions |
| `admin_account` | yes | list accounts; `ban <username> [reason]` / `unban <username>` |

The three tools that write open the database with `mode=rw`: they never create
one. Run them from the directory that holds `world.db`, or point
`PROJECTDAWN_DATABASE_URL` at it; a missing database is an error, not a fresh
empty file.

Every authorized dev/GM command (`/give`, coin grants, dev spawns, Full Heal,
XP grants) is recorded in `gm_actions` and is refused if it cannot be
recorded. `admin_report` is how you read the log.

## Crate layout

| Crate | Purpose |
|---|---|
| `crates/protocol` | Wire-format types shared by client and server. JSON for auth, bincode for world. |
| `crates/projectdawn-server` | The server binaries. Auth WS handler, DB pool, the world tick, and the ops tools. |
| `crates/gdext-net` | The Godot GDExtension the client uses to speak the world protocol. Built to a `.dll` that is hand-copied into the client's `addons/gdext_net/`. |
| `crates/shared` | Gameplay constants and formulas referenced by the server (and eventually a Godot GDExtension). |

## Promote a GM

```sh
cargo run -p projectdawn-server --bin grant_gm -- <username> on
```

Takes effect on that account's next world login (the flag rides the signed
connect token). No args lists every account and its GM status. There is no
in-game promote command by design: GM is what lets an account use dev tools on
a server running with `PD_DEV_CMDS` off, which is how the hosted build runs.
