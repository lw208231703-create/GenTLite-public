# GenTLite 规范符合性状态（2026-11-11）

## 结论

**不能对外宣称“完全符合” GigE Vision 3.0 / USB3 Vision 1.2 的全部条款。**
更准确的说法是：

> 通用 GenTL producer 的核心控制/采集路径符合规范；未实现的特性不应答或明确拒绝；
> Linux 虚拟 U3V 设备仍有一个明确的描述符偏差（Device Info CS）。
> 目前是“核心路径合规 + 明确的支持子集”，不是完整标准认证。

这里不把模拟器测试、NI 单宿主测试等同于完整合规认证。

## 范围

- 通用 GenTL producer：`gentl/core`、`gentl/gvcp`、`gentl/gige`、`gentl/u3v`、`gentl/simulator`
- GenTL C ABI：`gentl/gige/src/lib.rs`、`gentl/u3v/src/lib.rs`
- Linux 虚拟相机：`virtual/gige`、`virtual/u3v`、`virtual/null`
- 网关：`gateway/core`、`gateway/backends`

Windows 和 Linux 不是两套协议实现：Rust 源码共用，差异主要在 USB I/O 后端
（WinUSB / libusb）和打包产物（`.dll` / `.so`）。

## 分项判定

### 通用 GigE producer

| 项目 | 状态 |
|---|---|
| GVCP 发现、命令/应答头、BE、状态码、req_id 重试 | 符合（`gige-audit.md`） |
| READREG/WRITEREG/READMEM 分片与回退 | 符合 |
| SCP/SCPS/CCDA/CCP/流通道寄存器 | 符合 |
| GVSP 3.0 EI=1 / 1.2 EI=0 头、leader/trailer | 符合 |
| PACKETRESEND、重复包、尾部缺口、CR-204/205 | 符合 |
| 多通道 | 有基础支持；多 tap 并行块未完整实现 |
| GVRSP / RoCEv2 | 未实现数据面，明确拒绝 |
| Chunk / Buffer Parts / Composite / Stacked | GenTL 可选特性，当前 NOT_IMPLEMENTED |
| 断线检测与自动恢复 | 控制面心跳存在，完整恢复流程未闭合 |
| `TL_INFO_PATHNAME` | 已实现（旧审计报告中的缺口已过时） |
| `STREAM_INFO_PAYLOAD_SIZE` 启动前取值 | 打开数据流时从设备 XML 的 `PayloadSize` 特性解析到 `IntReg`/`MaskedIntReg`，再经 GVCP 读取；`local:` First URL（含 zip）支持，解析失败才回退到首帧 leader |

### 通用 U3V producer

| 项目 | 状态 |
|---|---|
| GenCP 命令/应答、prefix、request_id、PENDING_ACK、状态码 | 符合 |
| ABRM / SBRM / SIRM 寄存器与全小端 | 符合 |
| leader/payload/trailer、SIRM 几何、可变帧高 | 符合 |
| DEI 事件泵、`EVENT_REMOTE_DEVICE` | 已接线并测试 |
| 未知/畸形命令、GenDC | 明确拒绝或 NOT_IMPLEMENTED |
| 多流 / GenDC | 未实现，不能宣称完整覆盖 |
| 真实相机短包、可变帧、ENDPOINT_HALT 重同步 | 仍需更多真机矩阵 |

### Linux 虚拟 U3V（`virtual/u3v`）

**明确偏差 A1：**

- U3V 1.2 §3.2.5 / R-15cd 要求 Device Info CS 描述符必须且仅一份出现在默认
  Device Control Interface 之后。
- 当前 Linux FunctionFS 内核组合 config 不携带该 CS 描述符；标准请求由
  composite 应答，用户态 ep0 分支无法补上。
- 影响：pylon/MVS 等第三方 producer 原生发现虚拟 U3V 失败；项目自己的
  CTI 用 EF/05 描述符兜底可以打开。
- 这不是通用 GenTL producer 的缺陷，而是**虚拟 U3V 设备**的偏差；项目
  charter 已将其列为范围外，但按标准不能算“完全符合”。

其他：

- 事件接口、SS 端点、SIRM 单通道、U3V 1.2 流格式已通过 NI/自写 CTI 测试。
- pylon/MVS 原生发现失败是已知边界，不能拿“我方 CTI 能打开”替代。

### Linux 虚拟 GigE（`virtual/gige`）

- 对客户端呈现的是 GigE Vision 设备；SCP/SCPS/SCPS 流通道、GVSP legacy 1.2
  输出、XML/寄存器透传已验证。
- GigE 人格的 XML 会补齐标准传输节点 `GevSCPSPacketSize`、`GevSCPHostPort`、
  `GevSCDA`、`GevSCPD`，映射到虚拟通道 0 的真实寄存器；真实 GigE 相机 XML
  已有的节点不重复注入，U3V 人格不注入。`MaskedIntReg` 对 big-endian 采用
  GenICam 的 MSB-first 编号：低 16 位写作 `LSB=31` / `MSB=16`，写反会被
  GenApi 参考实现以“LSB must be greater or equal than MSB”拒绝加载。
- `SCPSx` 在通道开始发送 GVSP 帧后按 §36.58 拒绝写入并返回
  `GEV_STATUS_BUSY`（普通包长写和 test-packet 请求都一样）；首个 GVSP 帧
  之前的 §11.7 协商窗口仍允许写。`SCPD` 的包间延迟在设备提供时间戳 tick
  频率时按 §36.59 生效，tick 频率为 0 时该寄存器无定义单位。
- 虚拟 Null 相机的 `Mono16` PFNC 值已修正为 `0x01100007`（旧 `0x01100001`
  不是 PFNC 值）；Null XML、GVSP leader、GenTL BufferInfo 三处一致。
  MVS 只识别标准值，旧值会让 16-bit 流显示为花屏；修正后 MVS 原生 GigE
  已按 `pf=0x01100007` 取流，原始数据与生成图逐字一致。
- 不声明 PACKETRESEND 能力，因此不发送重传请求是合规的“支持子集”。
- 单通道、无多 tap、无 GVRSP；这些是可选/未实现能力。
- 仍存缺口（不在本轮三项修复内）：流式中非零写 `SCPx` 当前会重开通道，
  R-586cd 要求返回 `GEV_STATUS_BUSY`；流式中写 `SCDAx` 未按 R-588cd 拒绝。
- 不能宣称完整覆盖 GigE Vision 3.0 全部可选特性。

## 对外可以说 / 不能说

可以说：

- 支持 GigE Vision 3.0/1.2 的核心控制与图像采集路径。
- 支持 USB3 Vision 1.2 的 GenCP、SIRM、图像流和 DEI 核心路径。
- GenTL 1.6 核心图像采集接口可用；可选特性未实现时明确返回
  `NOT_IMPLEMENTED`。
- 不伪造未实现的能力位。

不能说：

- “完全符合 GigE Vision 3.0 全部条款。”
- “完全符合 USB3 Vision 1.2 全部条款。”
- “覆盖所有可选特性（Chunk/Parts/Composite/GVRSP/RoCEv2/多流）。”
- “已在所有品牌相机和所有第三方宿主上验证。”

## 要接近“完整合规”还需要

1. 修复 Linux U3V Device Info CS（内核 f_fs 白名单补丁或自定义 U3V function）。
2. 明确并测试支持特性矩阵；可选特性要么实现，要么不声明。
3. 真实 GigE/U3V 多品牌相机矩阵（不同 PixelFormat、短包、可变帧、多 tap）。
4. pylon / MVS / HALCON 等我方 CTI 和第三方 producer 的双向矩阵。
5. 断线、重连、ENDPOINT_HALT、网络抖动和长时间稳定性。
6. 如果客户要求完整标准认证，建立正式的 clause-by-clause 合规测试报告。

## 现有证据

- `compliance/spec-audit/gige-audit.md`
- `compliance/spec-audit/u3v-audit.md`
- `compliance/spec-audit/u3v-3.2-raw.txt`
- `compliance/preflight-gige-u3v.md`
- `notes/gvcp-wirefacts.md`
- `notes/u3v-wirefacts.md`
- `notes/OPEN-QUESTIONS.md`
- `notes/remaining-work-checklist.md`
- `docs/GigE Vision v1.2.pdf`
- `docs/GigE-Vision-Specification-3-0-00a.pdf`
- `docs/USB3_Vision_Specification_V1.2.pdf`
