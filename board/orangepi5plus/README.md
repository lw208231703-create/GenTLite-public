# Orange Pi 5 Plus board assets

This directory captures the board-specific changes used by the vgige
virtual U3V / GigE setup. It complements
`docs/vgige-board-kernel-interop-2026-11-04.md`.

## Target

- Orange Pi 5 Plus
- Armbian 26.5.1 trixie
- kernel `6.1.115-vendor-rk35xx`
- Type-C device port in SuperSpeed
- board IP for the GigE front: `192.168.137.55`

## Layout

```text
board/orangepi5plus/
├── dtb/
│   ├── custom-peripheral.dtbo
│   └── custom-peripheral.dts
├── prebuilt/
│   └── 6.1.115-vendor-rk35xx/u3vffs.ko
├── u3vffs-src/
│   ├── f_fs-u3v.c
│   ├── Makefile
│   └── *.h
├── systemd/
│   ├── vgige.service
│   ├── vgige-gige.service
│   ├── cpufreq-performance.service
│   └── vgige.service.d.conf
├── udev/
│   └── 99-vgige-typec.rules
└── scripts/
    ├── switch-kernel-current.sh
    ├── switch-kernel-vendor.sh
    └── typec-ss-recover.sh
```

## DTB overlay

The overlay's effective content is:

```dts
/dts-v1/;
/ {
    fragment@0 {
        target-path = "/usbdrd3_0/usb@fc000000";
        __overlay__ {
            maximum-speed = "super-speed";
        };
    };
};
```

Install:

```bash
cp custom-peripheral.dtbo \
  /boot/dtb-6.1.115-vendor-rk35xx/rockchip/overlay/custom-peripheral.dtbo
```

Ensure the active env file contains:

```text
overlay_prefix=rockchip-rk3588
overlays=custom-peripheral
```

The overlay deliberately omits `dr_mode = "peripheral"`; forcing peripheral
mode was found to break the Type-C direction/rate recovery. The remaining
`maximum-speed` keeps both plug orientations at SuperSpeed.

## u3vffs module

Source is in `u3vffs-src/`. It derives from the Linux kernel
functionfs code and is licensed GPL-2.0+ (see the SPDX header); the
prebuilt module is likewise GPL. Build against the active vendor kernel:

```bash
KVER=$(uname -r)
cd u3vffs-src
make -C /lib/modules/$KVER/build M=$PWD modules
install -m 0644 u3vffs.ko /lib/modules/$KVER/extra/u3vffs.ko
depmod -a
```

A prebuilt `.ko` for `6.1.115-vendor-rk35xx` is kept under `prebuilt/` for
recovery, but rebuild from source whenever kernel headers change.

Install the systemd drop-in as:

```bash
mkdir -p /etc/systemd/system/vgige.service.d
cp systemd/vgige.service.d.conf \
  /etc/systemd/system/vgige.service.d/vendor-ffs.conf
systemctl daemon-reload
```

Drop-in contents:

```ini
[Service]
Environment=VGIGE_FFS_FUNCTION=u3vffs
Environment=VGIGE_FFS_FS=functionfs_u3v
ExecStartPre=/sbin/modprobe u3vffs
```

## Services

Install the service files:

```bash
cp systemd/vgige.service /etc/systemd/system/
cp systemd/vgige-gige.service /etc/systemd/system/
cp systemd/cpufreq-performance.service /etc/systemd/system/
systemctl daemon-reload
systemctl enable vgige.service vgige-gige.service cpufreq-performance.service
```

The service `ExecStart` assumes the repository is deployed at
`/root/GenTLite-public`. Update the path if the workspace lives elsewhere.

- `vgige.service`: U3V gadget front, payload `2621440` bytes
- `vgige-gige.service`: GigE front on `192.168.137.55:3956`
- `cpufreq-performance.service`: forces `performance` governor

## Type-C recovery

Install the udev rule and helper script:

```bash
install -m 0644 udev/99-vgige-typec.rules \
  /etc/udev/rules.d/99-vgige-typec.rules
install -m 0755 scripts/typec-ss-recover.sh /root/typec-ss-recover.sh
udevadm control --reload-rules
```

The rule triggers `/root/typec-ss-recover.sh` on Type-C orientation changes;
the script performs a DWC3 unbind/rebind when the port drops out of SuperSpeed.

## Kernel switching

The scripts expect the corresponding Armbian boot env files and kernel
symlinks to exist:

```bash
# current 6.12
/root/switch-kernel-current.sh

# vendor 6.1.115
/root/switch-kernel-vendor.sh
```

They are recovery aids, not a substitute for a full kernel package install.

## Verification

```bash
systemctl is-active vgige.service vgige-gige.service
cat /sys/class/udc/fc000000.usb/current_speed   # super-speed
modinfo u3vffs
```

See `docs/vgige-board-kernel-interop-2026-11-04.md` for the full pitfall log,
GigE endian/XML/GVSP fixes, 16-bit Null behavior, and Windows driver notes.
