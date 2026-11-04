# vgige 板端内核、U3V/GigE 联调与踩坑记录

> 日期：2026-11-04  
> 板端：Orange Pi 5 Plus / Armbian 26.5.1 trixie / 内核 `6.1.115-vendor-rk35xx`  
> 目标：板端同时提供虚拟 U3V 与虚拟 GigE，兼容 NI MAX / NI-IMAQdx、GenTLite CTI、MVS/LD 等消费者。  
> 板端可重放资产：`board/orangepi5plus/`

## 0. 2026-11-04 无真机收尾

- `cargo cti` / `gentl-packager` 已跨平台：Windows 复制 `.dll`，Linux 复制
  `lib*.so` 为 `.cti`；板端 `dump_cti` 可直接加载生成的 `.cti`。
- `TL_INFO_PATHNAME` 已在 GigE/U3V producer 实现（Windows
  `GetModuleFileNameW`，Linux `dladdr`）。
- Null 后端同时暴露 `Mono8` 和 `Mono16`，`PixelFormat` 可写；Mono8
  使用 8-bit 梯度/帧号图案，PayloadSize 动态变化。
- GigE `FrameMeta` 增加实际 payload 字节数提示；未知 PixelFormat 的
  `BUFFER_INFO_SIZE_FILLED` 不再是 0。
- GVCP 增加 peer-aware `write_reg_from`；CCP 授权绑定首个控制主机，
  心跳过期后释放，另一 peer 的写请求会被拒绝。
- 板端新增 `board/orangepi5plus/install.sh`，自动安装 DTB、u3vffs、
  systemd、udev 和 helper scripts；仅 `START=1` 时重启服务。
- Windows 验证：NI GigE/U3V 各 5/5；GenTL GigE Mono16 5/5 与生成图
  0 差异；GenTL GigE Mono8 5/5 与生成图 0 差异；GenTL U3V 5/5，
  Mono16 `pf=0x01100001`。
- 性能基准（Null 后端，端到端）：30 fps 板端 CPU 约单核 12%；
  GigE 1 Gbps 上限约 44.7 fps / 117 MB/s；U3V 上限约
  95 fps / 249 MB/s。无 incomplete、FrameID 缺口或 stream underrun。
  详见 `docs/vgige-performance-2026-11-04.md`。

## 1. 当前状态

- 板端地址：`192.168.137.55`
- U3V：`vgige.service`，SuperSpeed，当前 PC 侧绑定 **NI-IMAQdx**
- GigE：`vgige-gige.service`，`0.0.0.0:3956`，PC 侧 NI GigE 正常
- 虚拟图像：`1280×1024 Mono16`，每帧 `2,621,440` 字节，默认 `30 fps`，约 `79 MB/s`
- 当前板端二进制：
  `/root/GenTLite-public/target/release/vgige-cli`
- U3V 启动参数：
  `--back null --front u3v --u3v-payload 2621440`
- GigE 启动参数：
  `--back null --front gige --up 192.168.137.55 --port 3956`

## 2. 板端内核与驱动修改

### 2.1 内核切换

- 已从 6.12 current 切换到 **6.1.115 vendor**。
- 回滚脚本：
  - `/root/switch-kernel-current.sh`：切回 6.12
  - `/root/switch-kernel-vendor.sh`：切到 vendor 6.1.115
- vendor 内核下 U3V gadget 需要独立模块 `u3vffs`，不能只依赖主线 `usb_f_fs`。

### 2.2 Type-C / DTB overlay

修改文件：

`/boot/dtb-6.1.115-vendor-rk35xx/rockchip/overlay/custom-peripheral.dtbo`

处理：

- 去掉 `dr_mode="peripheral"`，避免方向切换后枚举/速率异常。
- 保留 `maximum-speed="super-speed"`。
- 正反插均验证为 `super-speed`。

相关恢复：

- udev 规则：`/etc/udev/rules.d/99-vgige-typec.rules`
- 恢复脚本：`/root/typec-ss-recover.sh`
- 手动恢复思路：对 DWC3 做 unbind/rebind，使 Type-C 方向异常后重新进入 SuperSpeed。

### 2.3 u3vffs 模块

源码目录：

`/root/vendor-ffs/ffs61`

关键文件：

- `f_fs-u3v.c`
- `Makefile`：`obj-m := u3vffs.o`
- 预编译模块：`u3vffs.ko`

重新编译与安装：

```bash
KVER=$(uname -r)
cd /root/vendor-ffs/ffs61
make -C /lib/modules/$KVER/build M=$PWD modules
cp u3vffs.ko /lib/modules/$KVER/extra/u3vffs.ko
depmod -a
modinfo u3vffs
```

systemd drop-in：

`/etc/systemd/system/vgige.service.d/vendor-ffs.conf`

```ini
[Service]
Environment=VGIGE_FFS_FUNCTION=u3vffs
Environment=VGIGE_FFS_FS=functionfs_u3v
ExecStartPre=/sbin/modprobe u3vffs
```

> 重新安装模块后建议重启板端，让旧模块引用完全释放；否则 `rmmod u3vffs` 可能报 `Module u3vffs is in use`。

### 2.4 CPU 性能模式

`/etc/systemd/system/cpufreq-performance.service` 已 enable，用于避免 governor 波动影响 U3V/GigE 吞吐。

## 3. GigE 关键踩坑

### 3.1 寄存器端序

现象：NI MAX 中 `AcquisitionFrameRate` 越界，`.icd` 出现：

- `Width=327680`（实际 1280）
- `Height=262144`（实际 1024）
- `AcquisitionFrameRate=503316480`（实际 30）

原因：

- Null/U3V 后端寄存器是 little-endian；
- GigE Vision 寄存器/XML 走 big-endian；
- 直接把 little-endian 的 4 字节块发给 GigE 客户端，客户端按大端解释，导致 `30` 变成 `30 << 24`。

处理：

- `CameraBack` 增加 `RegisterEndian` / `register_endian()`。
- Null、U3vBack 返回 `Little`，GigE 默认 `Big`。
- `RegisterSpace` 的 pass-through block read/write 在 backend 为 Little 时做 4 字节/8 字节交换。
- `XmlMerger` 支持目标端序：
  - GigE：`BigEndian`
  - U3V：`LittleEndian`
- U3V `RegisterFile` 对 Big backend 做反向交换。

### 3.2 NI XML 缓存串台（U3V/GigE 同身份）

现象：修正端序后 NI 仍然读到旧的大端误读值。

原因：

- U3V 和 GigE 虚拟设备原先都叫 `GenTLite / Virtual / NULL-0001`；
- NI-IMAQdx 按 vendor/model/serial 缓存 XML；
- 先打开 U3V 时会缓存小端 XML，GigE 复用同一缓存，导致大端数据被按小端解释。

处理：

- GigE 使用独立身份：
  - `DeviceModelName = Virtual GigE`
  - `DeviceSerialNumber = NULL-0001-GE`
- XML cache 与 `.icd` 文件名随之分开。
- 必要时删除 NI 缓存后重开设备。

NI 数据目录：

`C:\Users\Public\Documents\National Instruments\NI-IMAQdx\Data`

相关文件示例：

- `GenTLite Virtual GigE (#0000C0742BF821FC).icd`
- `cam5.iid`
- `XML\GenTLite__Virtual_NULL-0001_*.xml`

### 3.3 NI 需要 GVSP 1.2 legacy 头

现象：

- 使用 GVSP 3.0 扩展头（20 字节、EI=1）时，NI 能建立流但没有帧。
- 切换为 1.2 legacy（8 字节头、36 字节 leader body）后 NI 正常。

处理：

- `GvspSession` 默认 legacy 1.2。
- 需要 3.0 时设置环境变量：

```bash
VGIGE_GVSP_3_0=1
```

### 3.4 legacy 包长算错导致斜条纹

现象：

- NI 显示斜条纹/周期性错位；
- 原始图像每个包边界出现 **12 个 0 字节**。

原因：

- legacy 1.2 头的总开销是 `IP20 + UDP8 + GVSP8 = 36`；
- `SCPS=1500` 时 payload 应为 `1500 - 36 = 1464`；
- 旧代码按 3.0 的 `48` 计算，发送 `1452`，NI 每包补 12 字节，图像逐包错位。

处理：

- legacy 模式按 `GVSP_LEGACY_HEADERS_OVERHEAD` 计算 chunk：
  `packet_size - 36`。
- 3.0 模式仍按 48 计算。

### 3.5 NI test packet

现象：

- `start: 0xBFF69031`，系统没有收到 test packet。

处理：

- GigE front 已实现 `SCPS fire_test_packet`：
  - 拦截 `SCPS0` 的 fire bit；
  - 若 `SCP0`/`SCDA0` 尚未就绪，先挂起 pending；
  - 目标 IP/端口就绪后，发送 legacy 8-byte GVSP 头、block_id=0 的测试包；
  - 测试包总长严格等于 SCPS0 声明值（IP+UDP+GVSP+payload）。
- NI `.icd` 可保持默认 `TestPacketEnabled=true`；当前实测 NI 5/5 帧正常。
- 仍建议保留：

```ini
AcquisitionAttributes::PacketSize = "1500"
```

## 4. 16-bit Null 图像

配置：

- `1280×1024 Mono16`
- PFNC `Mono16 = 0x01100001`
- 每帧 `2,621,440` 字节
- 默认 30 fps
- GigE 约 `79 MB/s`，1Gb 链路可跑
- U3V 服务参数 `--u3v-payload 2621440`

测试图案：

- 16-bit 水平渐变背景；
- 左上角 16 个 32×32 黑白方块编码 frame block id；
- 每帧移动的白色竖条（每帧右移 64 px）；
- 约每帧可肉眼识别，也能从图像解码帧号。

验证结果：

- NI GigE：100/100 帧，约 30 fps；
- NI U3V：100/100 帧，约 30 fps；
- GenTL GigE：5 帧与模拟端逐字节一致，与 NI 同帧号逐字节一致；
- GenTL U3V：5 帧与模拟端逐字节一致，与 NI 重叠帧逐字节一致。

## 5. MVS / LD 的 16-bit 限制

- MVS 和 LD 对虚拟相机的 8-bit 图像可显示，16-bit 图像能收流但不显示。
- 核对了数据侧：
  - GigE XML `PixelFormat = Mono16`，值 `0x01100001`；
  - GVSP leader `pixel_format = 0x01100001`；
  - GenTL BufferInfo `pf = 0x01100001`；
  - PayloadSize `2,621,440`。
- 因此不是虚拟相机没声明像素格式，而是这两个软件底层的 16-bit 显示/转换路径支持不完整。
- MVS 日志曾出现 `Get XML Translate DomDocument Failed`；尝试过 `StandardNameSpace=GEV`、GenApi 1.0，未解决显示问题，已回退这些兼容性修改。
- LD 出现同一设备重复三个，是加载了旧 CTI/缓存重复枚举；当前 CTI 验证只枚举一个设备。

后续可选：

- 给 Null 增加 `Mono8` 可选模式，让 MVS/LD 用 Mono8，NI 继续用 Mono16。

## 6. Windows GenTL U3V 驱动坑

### 6.1 NI-IMAQdx 独占

- U3V 设备绑定 NI-IMAQdx USB3 Vision 驱动时，GenTLite U3V producer 通过 WinUSB 打不开设备：
  `IFOpenDevice: -1005 access denied`
- 需要使用 WinUSB/libusbK，或临时切换驱动。

### 6.2 IAD 复合接口

- 本 U3V gadget 是 IAD 复合接口：Control/Event/Stream 三个接口。
- Windows/WinUSB 可能只暴露控制节点；原 `gentl-u3v` 只打开独立 MI 节点，导致：
  `DevOpenDataStream: -1007 invalid ID`（stream_in 为空）。
- 修复：
  `crates/gentl-u3v/src/discovery.rs` 增加 `WinUsb_GetAssociatedInterface` fallback：
  - 只有一个 device interface 时，枚举 associated interfaces；
  - children handles 放在 parent 之前，避免 parent 先释放。
- 修复后 `grab_u3v` 5/5 帧，Mono16 正常。

### 6.3 驱动切换命令

WinUSB（GenTL U3V 测试）：

```powershell
wdi-simple.exe -v 0x1209 -p 0x7D01 -i 0 -t 0 -n "GenTLite U3V" -d <临时目录>
```

恢复 NI-IMAQdx：

```powershell
# 先查询当前 DriverInfPath，例如 oem67.inf / oem92.inf
pnputil /delete-driver oemXX.inf /uninstall /force
pnputil /scan-devices
# 必要时用 devcon update 指向 NI 的 oem86.inf
```

恢复后确认：

```powershell
Get-PnpDevice -PresentOnly | Where-Object { $_.InstanceId -match 'VID_1209&PID_7D01&MI_00' }
# 应为 NI Vision Acquisition Devices / NI-IMAQdx USB3 Vision Device
```

测试时 NI MAX 必须关闭，否则会占用相机并报 `0xBFF6900A`（Camera is already in use）。

## 7. Rust / 构建坑

- 板端 Rust 为 **1.85.1**。
- `zip 2.4.2` 的 `deflate` feature 会引入 `zopfli 0.8.4`，其 MSRV 为 Rust 1.88，板端无法编译。
- 处理：
  - `zip` 使用 `deflate-flate2` + `flate2` optional feature；
  - 显式添加 `flate2`，启用 `rust_backend`；
  - 避免 `zopfli`。
- 涉及文件：
  - `crates/gentl-gige/Cargo.toml`
  - `crates/gentl-u3v/Cargo.toml`
  - `vgige/vgige-back-gige/Cargo.toml`
- 板端无外网时用：

```bash
cargo build --release --offline -p vgige-cli \
  --no-default-features --features back-null,front-gige,front-u3v
```

## 8. 全量清理与重新部署流程

PC：

```powershell
cargo clean
cargo build --release --offline --workspace
cargo build --release --offline --example grab_u3v --example grab_camera --example dump_cti
Copy-Item target\release\gentl_u3v.dll target\release\gentl_u3v.cti -Force
Copy-Item target\release\gentl_gige.dll target\release\gentl_gige.cti -Force
```

板端：

```bash
cd /root/GenTLite-public
systemctl stop vgige.service vgige-gige.service
# 如重装模块，重启后再确认
cargo build --release --offline -p vgige-cli \
  --no-default-features --features back-null,front-gige,front-u3v
KVER=$(uname -r)
cd /root/vendor-ffs/ffs61
make -C /lib/modules/$KVER/build M=$PWD modules
cp u3vffs.ko /lib/modules/$KVER/extra/u3vffs.ko
depmod -a
systemctl restart vgige.service vgige-gige.service
```

验证：

```bash
systemctl is-active vgige.service vgige-gige.service
cat /sys/class/udc/fc000000.usb/current_speed
```

## 9. 当前遗留与注意事项

1. NI test packet 已由设备侧实现；`.icd` 的 `PacketSize=1500` 仍建议保留，删除 `.icd` 后重新确认。
2. MVS/LD 的 16-bit 显示限制不在虚拟相机侧；需要时加 Mono8 fallback。
3. U3V 当前绑定 NI-IMAQdx；GenTL U3V 需要临时切 WinUSB。
4. `crates/gentl-u3v/src/discovery.rs` 的 associated-interface 修复和 3 个 Cargo.toml 的 zip/flate2 调整已提交。
5. LD 如果仍显示三个重复设备，检查它实际加载的 `gentl_u3v.cti` 路径，不要使用旧副本。
6. 需要 3.0 GVSP 时使用 `VGIGE_GVSP_3_0=1`；NI 必须使用默认 legacy。
