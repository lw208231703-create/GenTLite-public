# 宿主软件互通矩阵（GenTLite）

目标：度量"高兼容性"——宿主软件 × 我们的 producer × 我们的虚拟相机 的三维互通。
每次发布前更新本表；新增宿主软件加列。

图例：✅ 通过　❌ 失败　⬜ 未测　🟡 部分（见备注）

## 2026-11-04 当前状态（vgige 虚拟相机）

- 板端：Orange Pi 5 Plus，Armbian 26.5.1，vendor 内核 `6.1.115-rk35xx`，
  Type-C SuperSpeed；U3V + GigE 两个 vgige 服务 active。
- 图像：`1280×1024 Mono16`，payload `2,621,440` 字节，30 fps，约 79 MB/s。
- NI-IMAQdx：
  - GigE：100/100 帧，NI MAX 可显示。
  - U3V：100/100 帧。
- GenTLite producer：
  - GigE：5/5 帧；与 NI 同帧号、与模拟端生成图均逐字节 0 差异。
  - U3V：5/5 帧；与 NI 重叠帧、与模拟端生成图均逐字节 0 差异。
  - Windows GenTL U3V 使用 WinUSB；IAD 复合接口的 associated-interface
    支持已加入 `gentl-u3v`。
- MVS / LD：
  - 8-bit 虚拟图像可显示。
  - 16-bit 能收流但不显示；不是 pixel-format 缺失（XML、GVSP、GenTL
    BufferInfo 均为 `Mono16 = 0x01100001`），是宿主底层 16-bit 显示/转换限制。
  - LD 同一设备重复三次是旧 CTI/缓存重复枚举；当前 CTI 只枚举 1 个设备。
- NI test packet：设备侧 `SCPS fire_test_packet` 已实现；NI 5/5 帧通过。
- 驱动互斥：U3V 同一时间只能绑定 NI-IMAQdx 或 WinUSB；当前 PC 侧为
  NI-IMAQdx，GenTL U3V 需临时切 WinUSB。
- 详细记录：`docs/vgige-board-kernel-interop-2026-11-04.md`。

## 维度 1：第三方宿主 + 我们的 CTI + 真实相机

| 宿主 | 加载 CTI | 发现相机 | 打开相机 | GenApi 读参数 | 采图 | 备注 |
|---|---|---|---|---|---|---|
| ld（自家软件） | ⬜ | ⬜ | ⬜ | ⬜ | ⬜ | 用户机器已有 |
| pylon (Basler) | ⬜ | ⬜ | ⬜ | ⬜ | ⬜ | 官网免费注册下载 |
| 海康 MVS | ⬜ | ⬜ | ⬜ | ⬜ | ⬜ | |
| Halcon (MVTec) | ⬜ | ⬜ | ⬜ | ⬜ | ⬜ | 需授权 |
| Aravis (开源) | — | ⬜ | ⬜ | ⬜ | ⬜ | 走 scripts/interop-aravis.sh |
| 自有 grab_gige/grab_u3v | ✅ | ✅ | ✅ | ✅ | ✅ | 基线，全绿 |

## 维度 2：第三方宿主 + 我们的 CTI + 虚拟相机（板载网关）

| 宿主 | 发现 | 打开 | 采图 | 备注 |
|---|---|---|---|---|
| 官方 GenApi C++ consumer | ✅ | ✅ | ✅（参数读写） | 52 节点；ExposureTimeAbs 写读通过 |
| LDVision | ✅ | ✅ | ✅ | 我方 `gentl_u3v.cti`；ROI/曝光/帧率写读及 100/100 帧通过 |
| pylon | ❌ | ⬜ | ⬜ | CInstantCamera 未发现设备；与 Device Info CS/第三方发现限制一致 |
| MVS | ❌ | ⬜ | ⬜ | MV_CC_EnumDevices=0；与 Device Info CS/第三方发现限制一致 |
| Aravis | ⬜ | ⬜ | ⬜ | 本次验证走维度 3 的原生 GigE 路径 |

## 维度 3：第三方 producer + 我们的虚拟相机（最严格，多半卡在 Device Info CS）

| producer | 发现 | 打开 | 备注 |
|---|---|---|---|
| pylon 自带 | ❌ | ⬜ | 未发现设备；A1 的已知范围外项 |
| MVS 自带 | 🟡 | ✅ | GigE 打开并采集 100/100 帧；U3V 原生发现仍为 0 |
| LDVision 原生 | ✅ | 🟡 | GigE 可发现；本次打开时因其他宿主占用而拒绝，非发现失败 |
| Aravis 自带 | ✅ | 🟡 | GigE：150/150 帧通过；U3V 自环能发现但打开失败，见下方 CS 限制 |

## 2026-10-30 交叉验证记录

- GigE 服务只绑定单播地址时，`192.168.137.255` 和 `255.255.255.255` 发现均超时；改为监听通配地址后三种地址均返回 `NULL-0001`，MVS 与 LD 均可发现设备。
- LDVision 3.0.0 通过 `LDV_Gentl_AddProducer` 显式加载我方 CTI，避免误用 LD 原生发现通道；640×512、曝光 5000 µs、帧率 60 设置正确，收到 100/100 帧。
- Aravis 0.8.34 在板端访问 `null → front-gige`：默认 1280×1024 和 ROI 640×512 两组均为 150/150 帧；曝光 5000 µs、增益 8 dB 和 PayloadSize 变化正确。
- Aravis U3V 自环发现 1 台设备，但 GUID 和流端点为空，GenICam 读取连续 `LIBUSB_ERROR_IO`，最终 `Failed to bootstrap USB device`。这与 Device Info CS 缺失一致，不能用 GigE 通过代替 U3V 通过。
- 240 fps 帧源优化后，GigE 稳定段达到 236–239 fps；高负载下仍有 UDP 丢包，因此该结果只用于性能分析，不记为零丢包验收。
- Windows U3V 在 640×512、240 fps 设置下实测 238.830 fps、2400/2400 帧完整非零；1280×1024 下经 SS burst=15 和 256 KiB 分段优化后约 197–208 fps（此前约 160–170 fps）。ROI 后 SIRM Required Payload Size 也已改为随后端动态更新，避免旧几何导致全零帧。
- Aravis 自环命令：`ARV_NO_PACKET_SOCKET=1 ARV_CAMERA=192.168.137.55 bash scripts/interop-aravis.sh arv-camera-test-0.8`。Linux 自环拓扑的 packet socket 会把包计为 underrun/ignored，普通 UDP socket 正常；该选项不代表真实网络也需要关闭 packet socket。
- 本轮同时修复了 SFNC 浮点节点类型、8 字节 PayloadSize、虚拟流通道计数、READMEM/WRITEMEM 虚拟寄存器分派和 WRITEREG_ACK Data Index。

## 测试规程（统一步骤，保证可复现）

1. 环境：板子跑 `vgige-cli --back null --front u3v`，USB 线接 Windows；
   真实相机测试用对应后端。
2. 第三方宿主加载 `target/release/gentl_u3v.cti`；Aravis 原生 GigE 验证则运行 `null → front-gige`。
3. 记录：能否列出设备 → 打开时报错原文 → GenApi 树能否展开 → 采 50 帧结果。
4. 失败一律附**原文报错**，区分"枚举层没看到"（驱动/发现）与"看到但拒绝"（描述符/XML）。
