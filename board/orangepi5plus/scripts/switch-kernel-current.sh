#!/bin/sh
set -e
cp /boot/armbianEnv-mainline-6.12.txt /boot/armbianEnv.txt
ln -sfn vmlinuz-6.12.0-current-rockchip-rk3588 /boot/Image
ln -sfn uInitrd-6.12.0-current-rockchip-rk3588 /boot/uInitrd
ln -sfn dtb-6.12.0-current-rockchip-rk3588 /boot/dtb
sync
ls -l /boot/Image /boot/uInitrd /boot/dtb
