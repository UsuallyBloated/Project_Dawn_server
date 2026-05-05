# Project Dawn Server

Authoritative server for Project Dawn. See `docs/server_design.md` for the
full architecture contract.

## Quick start

First time on a clean clone (Windows / PowerShell or any POSIX shell):

```sh
cd F:\Projects\server
cp .env.example .env
cargo run -p projectdawn-server
```

That's it — `rustup` auto-installs the pinned 1.95.0 toolchain on first
build, `sqlx` auto-applies migrations on first boot, and the auth
service starts listening on `0.0.0.0:8765`.

To run the test suite (5 tests, ~30 s including build):

```sh
cargo test
```

## Status

Pre-alpha. Currently provides the auth WebSocket service only:
`Register`, `Login`, `CharList`, `CharCreate`, `CharDelete`, `Logout`.
World UDP simulation is not yet implemented — clients still operate
local-save until the world server lands.

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

Integration tests under `tests/` spin up an in-process auth server on an
ephemeral port and exercise the full Register → Login → CharCreate flow.

## Crate layout

| Crate | Purpose |
|---|---|
| `crates/protocol` | Wire-format types shared by client and server. JSON for auth, bincode for world. |
| `crates/projectdawn-server` | The server binary. Auth WS handler, DB pool, future world tick. |
| `crates/shared` | Gameplay constants and formulas referenced by the server (and eventually a Godot GDExtension). |

## Promote a GM

```sh
sqlite3 world.db "UPDATE accounts SET is_gm = 1 WHERE username = 'you';"
```

No in-game promote command in the alpha.
