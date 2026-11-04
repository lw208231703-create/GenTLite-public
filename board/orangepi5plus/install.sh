#!/bin/sh
set -eu

if [ "$(id -u)" -ne 0 ]; then
    echo "install.sh must run as root" >&2
    exit 1
fi

HERE=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
WORKSPACE=${1:-/root/GenTLite-public}
KVER=${KVER:-$(uname -r)}
START=${START:-0}

if [ ! -d "$WORKSPACE" ]; then
    echo "workspace not found: $WORKSPACE" >&2
    exit 1
fi

echo "==> DTB overlay"
if [ -f "$HERE/dtb/custom-peripheral.dtbo" ] &&
   [ -d "/boot/dtb-$KVER" ]; then
    install -m 0644 "$HERE/dtb/custom-peripheral.dtbo" \
        "/boot/dtb-$KVER/rockchip/overlay/custom-peripheral.dtbo"
else
    echo "    skipped: DTB overlay or /boot/dtb-$KVER not present"
fi

echo "==> u3vffs module"
if [ -d "/lib/modules/$KVER/build" ]; then
    MAKE_DIR="$HERE/u3vffs-src"
    make -C "/lib/modules/$KVER/build" M="$MAKE_DIR" modules
    install -m 0644 "$MAKE_DIR/u3vffs.ko" \
        "/lib/modules/$KVER/extra/u3vffs.ko"
elif [ -f "$HERE/prebuilt/$KVER/u3vffs.ko" ]; then
    install -m 0644 "$HERE/prebuilt/$KVER/u3vffs.ko" \
        "/lib/modules/$KVER/extra/u3vffs.ko"
else
    echo "    no kernel build tree and no matching prebuilt module" >&2
    exit 1
fi
depmod -a

echo "==> systemd units"
install -d /etc/systemd/system/vgige.service.d
sed "s#/root/GenTLite-public#$WORKSPACE#g" \
    "$HERE/systemd/vgige.service" > /etc/systemd/system/vgige.service
sed "s#/root/GenTLite-public#$WORKSPACE#g" \
    "$HERE/systemd/vgige-gige.service" > /etc/systemd/system/vgige-gige.service
install -m 0644 "$HERE/systemd/cpufreq-performance.service" \
    /etc/systemd/system/cpufreq-performance.service
install -m 0644 "$HERE/systemd/vgige.service.d.conf" \
    /etc/systemd/system/vgige.service.d/vendor-ffs.conf

echo "==> udev and helpers"
install -m 0644 "$HERE/udev/99-vgige-typec.rules" \
    /etc/udev/rules.d/99-vgige-typec.rules
install -m 0755 "$HERE/scripts/typec-ss-recover.sh" /root/typec-ss-recover.sh
install -m 0755 "$HERE/scripts/switch-kernel-current.sh" /root/switch-kernel-current.sh
install -m 0755 "$HERE/scripts/switch-kernel-vendor.sh" /root/switch-kernel-vendor.sh
udevadm control --reload-rules
udevadm trigger --subsystem-match=typec || true

echo "==> enabling units"
systemctl daemon-reload
systemctl enable vgige.service vgige-gige.service cpufreq-performance.service
modprobe u3vffs || true

if [ "$START" = "1" ]; then
    echo "==> starting services"
    systemctl restart vgige.service vgige-gige.service
fi

echo "==> done"
echo "    workspace: $WORKSPACE"
echo "    start:    START=1 $0 $WORKSPACE"
echo "    verify:   systemctl is-active vgige.service vgige-gige.service"
echo "              cat /sys/class/udc/fc000000.usb/current_speed"
