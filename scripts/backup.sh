#!/usr/bin/env bash
# Nightly backup. Copy to /opt/projectdawn/scripts/ on the deploy host
# and wire to cron / systemd timer:
#   0 4 * * * projectdawn /opt/projectdawn/scripts/backup.sh
set -euo pipefail

DB="${PROJECTDAWN_DB:-/var/lib/projectdawn/world.db}"
DEST="${PROJECTDAWN_BACKUP_DIR:-/var/lib/projectdawn/backups}"
RETAIN_DAYS="${PROJECTDAWN_BACKUP_RETAIN:-7}"

mkdir -p "$DEST"
TS=$(date +%Y%m%d-%H%M%S)
sqlite3 "$DB" ".backup '$DEST/world-$TS.db'"

# Prune from INSIDE the backup directory. find returns to its starting
# directory when it finishes and exits 1 if it cannot; run by hand through
# `sudo -u projectdawn` from an operator's home directory, that start is
# unreadable to this account, so `set -e` ended the script here, after the
# backup was written but before the confirmation line (hit 2026-10-02).
cd "$DEST"
find . -name 'world-*.db' -mtime "+$RETAIN_DAYS" -delete

echo "backup complete: $DEST/world-$TS.db"
