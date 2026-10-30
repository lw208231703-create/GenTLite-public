#!/usr/bin/env bash
# Run on the Orange Pi after a Type-C orientation change. Linux 6.12's
# rockchip-usbdp driver updates its lane state on orientation changes but
# does not reinitialize an already-running PHY. Rebinding DWC3 forces that
# reinitialization without modifying the kernel.
set -euo pipefail

if [[ ${EUID} -ne 0 ]]; then
    echo "run as root on the Orange Pi" >&2
    exit 1
fi

UDC=fc000000.usb
DRIVER=/sys/bus/platform/drivers/dwc3

if [[ ! -e /sys/class/typec/port0/orientation ]]; then
    echo "Type-C port0 is unavailable" >&2
    exit 1
fi

systemctl stop vgige.service 2>/dev/null || true
pkill -x vgige-cli 2>/dev/null || true
sleep 1

echo "$UDC" > "$DRIVER/unbind"
sleep 1
echo "$UDC" > "$DRIVER/bind"
sleep 2

systemctl start vgige.service
sleep 4

orientation=$(cat /sys/class/typec/port0/orientation)
state=$(cat "/sys/class/udc/$UDC/state")
speed=$(cat "/sys/class/udc/$UDC/current_speed")
printf 'orientation=%s\nstate=%s\nspeed=%s\n' "$orientation" "$state" "$speed"

if [[ $speed != super-speed ]]; then
    echo "SuperSpeed did not recover; inspect /tmp/vgige.log and dmesg" >&2
    exit 1
fi
