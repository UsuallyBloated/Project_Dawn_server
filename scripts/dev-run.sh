#!/usr/bin/env bash
# Convenience wrapper: load .env, run the server with sensible logging.
set -euo pipefail

if [ -f .env ]; then
    set -a
    # shellcheck disable=SC1091
    source .env
    set +a
fi

export RUST_LOG="${RUST_LOG:-projectdawn_server=debug,sqlx=info}"
exec cargo run -p projectdawn-server "$@"
