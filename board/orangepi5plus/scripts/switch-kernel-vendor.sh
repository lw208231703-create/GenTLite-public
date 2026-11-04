#!/bin/sh
set -e
cp /boot/armbianEnv-vendor-6.1.115.txt /boot/armbianEnv.txt
ln -sfn vmlinuz-6.1.115-vendor-rk35xx /boot/Image
ln -sfn uInitrd-6.1.115-vendor-rk35xx /boot/uInitrd
ln -sfn dtb-6.1.115-vendor-rk35xx /boot/dtb
sync
ls -l /boot/Image /boot/uInitrd /boot/dtb
