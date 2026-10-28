#!/usr/bin/env bash
# Interop smoke test with Aravis (open-source GenTL/U3V consumer) against
# the vgige virtual camera.
#
# Two topologies:
#   a) aravis on a Linux host with the board's Type-C connected to that
#      host's USB port (the normal setup);
#   b) SELF-LOOP: the board itself runs this script with an A-to-C cable
#      between one of its own USB-A ports (host mode) and its Type-C
#      (device mode) — no second machine needed.
#
# Usage: bash scripts/interop-aravis.sh [arv-tool-0.8 | arv-camera-test-0.8]
set -euo pipefail

ARV="${1:-}"
if [ -z "$ARV" ]; then
    for c in arv-tool-0.8 arv-camera-test-0.8; do
        if command -v "$c" >/dev/null 2>&1; then ARV="$c"; break; fi
    done
fi
if [ -z "$ARV" ]; then
    echo "FAIL: neither arv-tool-0.8 nor arv-camera-test-0.8 found." >&2
    echo "      Debian/Ubuntu: apt install aravis-tools" >&2
    exit 1
fi
echo "==> using $ARV ($($ARV --version 2>/dev/null || echo unknown))"

if [[ "$ARV" == *tool* ]]; then
    echo "==> all discovered devices:"
    $ARV || true
    echo "==> looking for the vgige virtual camera (VID 0x1209)..."
    if $ARV | grep -qi "1209"; then
        echo "PASS: aravis discovered the virtual U3V camera"
    else
        echo "FAIL: virtual camera not in aravis discovery output" >&2
        echo "      (check the USB topology; for the self-loop use a" >&2
        echo "       DATA-capable A-to-C cable, charging-only won't enumerate)" >&2
        exit 1
    fi
else
    # arv-camera-test-0.8: discovers the first camera and grabs briefly —
    # covers discovery + open + streaming in one shot.
    echo "==> discovering + grabbing (5s) ..."
    if timeout 20 "$ARV" --duration=5 2>&1 | tee /tmp/arv-run.log; then
        if grep -qi "1209\|GenTLite\|Virtual\|frames" /tmp/arv-run.log; then
            echo "PASS: aravis discovered, opened and streamed the virtual camera"
        else
            echo "FAIL: ran but no camera output" >&2
            exit 1
        fi
    else
        echo "FAIL: arv-camera-test could not open a camera" >&2
        echo "      (self-loop: use a DATA-capable A-to-C cable)" >&2
        exit 1
    fi
fi
