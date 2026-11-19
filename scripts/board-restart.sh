#!/usr/bin/env bash
# Restart the vgige daemons on the board through systemd.
# WARNING: the U3V unit re-enumerates the USB gadget on the Windows host;
# after repeated failures Windows may disable the device (then a cable
# replug or PC reboot is needed — see docs/20-design/u3v-gadget-bringup-pitfalls.md #21).
#
# Usage: bash scripts/board-restart.sh [host]   (default 192.168.137.55)
set -euo pipefail

HOST="${1:-root@192.168.137.55}"

ssh "$HOST" 'bash -s' <<'REMOTE'
set -euo pipefail

restart_unit() {
    unit="$1"
    if systemctl cat "$unit" >/dev/null 2>&1; then
        systemctl restart "$unit"
        echo "$unit: $(systemctl is-active "$unit")"
    else
        echo "$unit: not installed (skipped)"
    fi
}

# The unit ExecStartPre sections own the configfs/gadget teardown; do not
# duplicate the old manual rmmod/unmount sequence here.
restart_unit vgige.service
restart_unit vgige-gige.service
sleep 2

echo "daemon processes: $(pgrep -xc vgige-cli || true)"
echo "--- vgige.service ---"
journalctl -u vgige.service -n 5 --no-pager 2>/dev/null || true
echo "--- vgige-gige.service ---"
journalctl -u vgige-gige.service -n 5 --no-pager 2>/dev/null || true

if [ -e /sys/class/udc/fc000000.usb/state ]; then
    cat /sys/class/udc/fc000000.usb/state /sys/class/udc/fc000000.usb/current_speed
fi
REMOTE
