#!/usr/bin/env bash
# Restart the vgige daemon on the board: clean teardown + fresh start.
# WARNING: this re-enumerates the USB gadget on the Windows host; after
# repeated failures Windows may disable the device (then a cable replug
# or PC reboot is needed — see notes/u3v-gadget-bringup-pitfalls.md #21).
#
# Usage: bash scripts/board-restart.sh [host]   (default 192.168.137.55)
set -euo pipefail

HOST="${1:-root@192.168.137.55}"

ssh "$HOST" 'bash -s' <<'REMOTE'
set -e
pkill -x vgige-cli 2>/dev/null || true
sleep 1
cd /sys/kernel/config/usb_gadget/vgige 2>/dev/null && {
    echo '' > UDC 2>/dev/null || true
    rm configs/c.1/* 2>/dev/null || true
    rmdir functions/* strings/0x409 configs/c.1 strings 2>/dev/null || true
    cd /sys/kernel/config/usb_gadget
    rmdir vgige 2>/dev/null || true
}
umount /dev/usb-ffs/vgige 2>/dev/null || true
rm -rf /dev/usb-ffs/vgige
modprobe libcomposite 2>/dev/null || true
mount -t configfs none /sys/kernel/config 2>/dev/null || true
cd /root/GenTLite-public
nohup ./target/release/vgige-cli --back null --front u3v --u3v-payload 1310720 \
    > /tmp/vgige.log 2>&1 </dev/null &
sleep 4
echo "daemon processes: $(pgrep -xc vgige-cli)"
head -1 /tmp/vgige.log
cat /sys/class/udc/fc000000.usb/state /sys/class/udc/fc000000.usb/current_speed
REMOTE
