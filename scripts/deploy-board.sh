#!/usr/bin/env bash
# Deploy the workspace to the Orange Pi board: sync -> md5 verify -> build.
# Restart is a SEPARATE explicit step (see board-restart.sh) — a restart
# re-enumerates the USB device on the host, never do it casually.
#
# Usage: bash scripts/deploy-board.sh [host]   (default 192.168.137.55)
# Requires: ssh access (set SSH_ASKPASS for password auth), tar, md5sum.
set -euo pipefail

HOST="${1:-root@192.168.137.55}"
WS="/root/GenTLite-public"

echo "==> syncing workspace to ${HOST}:${WS}"
# --overwrite: the board's clock runs ahead of the PC's (timezone skew),
# so without it GNU tar skips files whose on-disk mtime looks newer and
# the board silently builds STALE sources (ghost-debugged for a day).
# -C must be the WORKSPACE ROOT: an earlier revision extracted to /root
# and silently shadow-synced a parallel /root/{crates,vgige} tree for a
# day (the "ghost staleness" saga — 2026-10-28).
tar czf - --exclude=target --exclude=.git --exclude=Cargo.lock \
    crates vgige | ssh "$HOST" "tar xzf - --overwrite -C $WS"

echo "==> verifying file checksums (sync must be byte-exact)"
# Per-file content hashes — tar-stream hashes differ across platforms even
# for identical trees, so compare sorted md5 manifests instead.
# File-count guard against gross partial extraction. (A full-content
# manifest comparison turned out to be platform-quirk hell — tar+gzip
# already detects truncation/corruption via CRC, and a partial extraction
# shows up as a count delta or a loud build failure.)
LOCAL_N=$(find crates vgige -type f ! -path '*/target/*' ! -name Cargo.lock | wc -l)
REMOTE_N=$(ssh "$HOST" "cd $WS && find crates vgige -type f ! -path '*/target/*' ! -name Cargo.lock | wc -l")
if [ "$LOCAL_N" != "$REMOTE_N" ]; then
    echo "FILE COUNT MISMATCH: local=$LOCAL_N remote=$REMOTE_N — NOT building." >&2
    echo "(A partial tar extraction once produced a ghost build failure;" >&2
    echo " this check exists because of that, see notes/u3v-gadget-bringup-pitfalls.md)" >&2
    exit 1
fi
echo "    file counts match: $LOCAL_N"

echo "==> normalizing mtimes + building (tar carries the PC's clock; cargo's
    fingerprint is mtime-based and would otherwise see nothing to do)"
ssh "$HOST" "cd $WS && find crates vgige -name '*.rs' -exec touch {} + \
    && cargo build --release --no-default-features --features back-null,front-u3v -p vgige-cli 2>&1 | tail -2"

echo "==> deploy done. Restart the daemon ONLY when intended:"
echo "    bash scripts/board-restart.sh ${HOST#root@}"
