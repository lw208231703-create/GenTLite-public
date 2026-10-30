# 宿主软件互通矩阵（GenTLite）

目标：度量"高兼容性"——宿主软件 × 我们的 producer × 我们的虚拟相机 的三维互通。
每次发布前更新本表；新增宿主软件加列。

图例：✅ 通过　❌ 失败　⬜ 未测　🟡 部分（见备注）

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
| 官方 GenApi C++ consumer | ✅ | ✅ | ✅（参数读写） | 39 节点；ExposureTimeAbs 写读通过 |
| ld | ✅ | ⬜ | ⬜ | 发现已通（加载我方 cti）；打开失败→Port 节点已修，待复测 |
| pylon | ❌ | ⬜ | ⬜ | CInstantCamera 未发现设备；与 Device Info CS/第三方发现限制一致 |
| MVS | ❌ | ⬜ | ⬜ | MV_CC_EnumDevices=0；与 Device Info CS/第三方发现限制一致 |
| Aravis | ⬜ | ⬜ | ⬜ | |

## 维度 3：第三方 producer + 我们的虚拟相机（最严格，多半卡在 Device Info CS）

| producer | 发现 | 打开 | 备注 |
|---|---|---|---|
| pylon 自带 | ❌ | ⬜ | 未发现设备；A1 的已知范围外项 |
| MVS 自带 | ❌ | ⬜ | 未发现设备；A1 的已知范围外项 |
| Aravis 自带 | ⬜ | ⬜ | arv-tool-0.8 |

## 测试规程（统一步骤，保证可复现）

1. 环境：板子跑 `vgige-cli --back null --front u3v`，USB 线接 Windows；
   真实相机测试用对应后端。
2. 第三方宿主加载 `target/release/gentl_u3v_cti.dll`（/ `.cti`）；
3. 记录：能否列出设备 → 打开时报错原文 → GenApi 树能否展开 → 采 50 帧结果。
4. 失败一律附**原文报错**，区分"枚举层没看到"（驱动/发现）与"看到但拒绝"（描述符/XML）。
