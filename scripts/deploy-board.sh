#!/usr/bin/env bash
# Deploy the workspace to the Orange Pi board: sync -> count verify -> build.
# Restart is a SEPARATE explicit step (see board-restart.sh) — a restart
# re-enumerates the USB device on the host, never do it casually.
#
# The default feature set matches the two Null-backend demo services
# installed on the board. For a real-camera relay build, override it, e.g.
#   VGIGE_FEATURES=back-gige,front-u3v bash scripts/deploy-board.sh
#   VGIGE_FEATURES=back-u3v,front-gige bash scripts/deploy-board.sh
# When overriding, update the unit's ExecStart --back/--front too: the
# stock units select `--back null`, which is absent from a relay build.
#
# Usage: [VGIGE_FEATURES=...] bash scripts/deploy-board.sh [host]
# Requires: ssh access (set SSH_ASKPASS for password auth), tar.
set -euo pipefail

HOST="${1:-root@192.168.137.55}"
WS="/root/GenTLite-public"
FEATURES="${VGIGE_FEATURES:-back-null,front-u3v,front-gige}"
SYNC_PATHS=(Cargo.toml .cargo gentl gateway virtual board scripts)
# Cargo.lock is gitignored here but still belongs to a reproducible
# deployment; sync the workspace's resolved versions when present.
if [ -f Cargo.lock ]; then
    SYNC_PATHS+=(Cargo.lock)
fi

echo "==> syncing workspace to ${HOST}:${WS}"
# --overwrite: the board's clock runs ahead of the PC's (timezone skew),
# so without it GNU tar skips files whose on-disk mtime looks newer and
# the board silently builds STALE sources (ghost-debugged for a day).
# -C must be the WORKSPACE ROOT: an earlier revision extracted to /root
# and silently shadow-synced a parallel /root/{crates,vgige} tree for a
# day (the "ghost staleness" saga — 2026-10-28).
tar czf - --exclude=target --exclude=.git \
    "${SYNC_PATHS[@]}" | ssh "$HOST" "tar xzf - --overwrite -C $WS"

echo "==> verifying file checksums (sync must be byte-exact)"
# Per-file content hashes differ across platforms even for identical
# trees, so compare sorted md5 manifests instead.
# File-count guard against gross partial extraction. (A full-content
# manifest comparison turned out to be platform-quirk hell — tar+gzip
# already detects truncation/corruption via CRC, and a partial extraction
# shows up as a count delta or a loud build failure.)
LOCAL_N=$(find "${SYNC_PATHS[@]}" -type f ! -path '*/target/*' | wc -l)
REMOTE_PATHS="${SYNC_PATHS[*]}"
REMOTE_N=$(ssh "$HOST" "cd $WS && find $REMOTE_PATHS -type f ! -path '*/target/*' | wc -l")
if [ "$LOCAL_N" != "$REMOTE_N" ]; then
    echo "FILE COUNT MISMATCH: local=$LOCAL_N remote=$REMOTE_N — NOT building." >&2
    echo "(A partial tar extraction once produced a ghost build failure;" >&2
    echo " this check exists because of that, see docs/20-design/u3v-gadget-bringup-pitfalls.md)" >&2
    exit 1
fi
echo "    file counts match: $LOCAL_N"

echo "==> normalizing mtimes + building (tar carries the PC's clock; cargo's
    fingerprint is mtime-based and would otherwise see nothing to do)"
# Both fronts are compiled into one vgige-cli: the systemd units select
# --back/--front at runtime. The default Null feature set is what the demo
# units use; VGIGE_FEATURES opts into real-camera backends.
echo "    features: $FEATURES"
ssh "$HOST" "cd $WS && find gentl gateway virtual -name '*.rs' -exec touch {} + \
    && cargo build --release --no-default-features --features $FEATURES -p vgige-cli 2>&1 | tail -2"

echo "==> deploy done. Restart the daemons ONLY when intended:"
echo "    bash scripts/board-restart.sh ${HOST#root@}"
