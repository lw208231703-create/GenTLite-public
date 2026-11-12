# GigE 接收路径 100GigE 架构设计

> 2026-10-16。目标：让 GenTLite 的 GigE 接收路径在 Windows 上可分级地支撑
> 25/50/100GigE。基线实测见文末（gvsp_flood + gvsp_perf_rx 环回数据）。
> 立场不变：纯规范实现，零非标兼容；本设计只动接收侧架构与后端选择，
> GVSP 线格式与 GenTL 语义不变。

## 0. 问题分解

100GigE = 12.5 GByte/s 图像负载，线速约 14.6 GByte/s（含 IP/UDP/GVSP 头）。
两个独立瓶颈必须先分开：

1. **内核→用户态的数据通道**（syscall 次数、拷贝次数、RSS 散列）
2. **用户态重组管线**（每包解析 + memcpy 进 consumer buffer + DsCore 锁）

实测（本机，9KB 包）：单线程接收 0.15 Mpps / 1.33 GByte/s；4 线程 0.53 Mpps
但总量不涨——瓶颈 1 和 2 各占一半。100G 要求两者都 ~10 倍化。

**单流陷阱**：RSS 按四元组散列，一个 GVSP 流通道 = 一个四元组 = 一个硬件
队列 = 一个核。任何后端都必须靠**多流通道**（GigE Vision 原生支持，每通道
独立 SCDAx/SCPx/目的端口）把负载摊到多个队列。这是协议层事实，不是实现
选择——100G 相机必须多通道出流（业界 100GigE 相机即如此），producer 侧
按 GenTL 多 DataStream 模型一对一线程。

## 1. 后端分级（核心设计）

抽象 `RxBackend` trait（对齐 UsbBackend 的先例），`GevDataStream` 持有
Box<dyn RxBackend>，三级后端按能力自动降级，env 可强制：

```
XdpWin ──缺驱动/不支持──▶ Rio ──获取失败──▶ Std（今天的实现，保底）
```

| 级别 | 机制 | 预期能力 | 依赖 | Rust 现成库 |
|---|---|---|---|---|
| L0 Std | 多 socket + 每流一线程 + 巨型帧 | 10–40G（机况） | 无 | std（已有） |
| L1 RIO | Winsock Registered I/O：注册缓冲、完成队列批量出队 | 50–80G | Win8+，任意网卡 | **无**，需手写 FFI（WSAIoctl 取 RIO_EXTENSION_FUNCTION_TABLE，~300 行） |
| L2 XDP-Win | Microsoft xdp-for-windows，用户态 XDP 直达 | 100G+ | mlx5 系驱动（ConnectX-5+） | **无**，C API FFI（xdpapi.h） |
| L3 Rivermax | NVIDIA 商业 SDK，GPUDirect | 100G+，0 CPU 拷贝 | ConnectX + 付费授权（Windows 上 NVIDIA 对其合作伙伴开放） | 无（C SDK） |

选型依据（2026-10 调研）：

- Rust DPDK 生态不可用：crates.io 上的 rust-dpdk 是 2015 年 1.8.0 时代的
  死库；DPDK 官方 2025 才开始讨论 Rust 绑定（邮件列表阶段），不能依赖。
- Linux 侧（未来可移植项）有成熟库：xsk-rs / afxdp / xdp-rs（AF_XDP），
  LPC'18 论文《The Path to DPDK Speeds for AF_XDP》证实零拷贝 AF_XDP 可达
  DPDK 同级。本设计把 RxBackend 的 Linux 实现位留好。
- 机器视觉行业 Windows 100GigE 事实标准 = Rivermax（EVT 文档明确：
  "NVidia product requires Rivermax for Windows，Linux is open"）。L3 作为
  商业可选项接入，不进默认构建。
- RIO 是 Windows 原生"半内核旁路"：不挑网卡、不需驱动绑定、可在任意
  consumer 进程内使用（.cti 的关键约束），但无 Rust 封装——手写 FFI 是
  主要工作量。Microsoft 官方《Registered I/O API Extensions》+ grijjy
  的 UDP 服务端博文是参考实现来源。

## 2. 数据面设计（与后端无关的部分）

```
每流通道（GenTL DataStream）：
  RxThread(绑核) ──▶ 解析 GVSP 头（20B，无分配）
       │
       ▼
  Reassembler（现有 GvspReassembler，保持不变）
       │  payload 包：memcpy slot→frame offset（每包唯一拷贝）
       ▼
  DsCore（去全局锁，见 §3）
```

- **包长策略**：100G 下 16KB 巨型帧（需相机/NIC/交换机全链路支持）把 pps
  压到 ~0.86M；9KB 则 ~1.5M。包越大后端压力越小，M3-2 包大小协商升级为
  必选（test packet 二分）。
- **L0 路径**（纯 std，立即可做）：
  - 每流通道 `SO_REUSEADDR` 绑独立端口 + 独立接收线程（不用端口复用共享，
    多流通道天然多端口，语义干净）；
  - 接收线程绑核、避开网卡 DPC 亲和的核（部署清单）；
  - `SO_RCVBUF` 显式调到 16–32MB（std 无 API，需 `windows-sys` 的
    setsockopt，已依赖）；
  - 预分配 64KB slot 环形数组，recv_from 零分配。
- **L1 RIO 路径**：注册 slot 池，RIOReceive 批量提交、RIODequeueCompletion
  批量收割（一次出队多个完成项 = syscall 摊薄）；完成项携带 buffer id，
  直接定位 slot。消费端 buffer（DSAnnounceBuffer）注册为 RIO buffer 后，
  payload 可直收进 frame offset（跳过 slot→frame 拷贝）——留作 L1 二期，
  一期保持 slot→memcpy（跨 4 核 memcpy 聚合带宽 >20GByte/s，不是瓶颈）。
- **L2 XDP-Win**：xdp-for-windows 的 Rx 路径（XskCreate + UMEM ring），
  模型与 AF_XDP 相同，Linux 移植时直接映射到 xsk-rs。

## 3. DsCore 去锁（用户态瓶颈 2）

实测 4 线程总量不涨的主因。改法：

- **输入池分片**：每 RxThread 持本地 input  freelist（预取一批 buffer），
  只在批次边界与全局池交换（批量转移 32/64 个），把锁竞争降 1-2 个数量级；
- **完成投递 SPSC**：block 完成时把 buffer id 经 per-stream SPSC 队列推给
  引擎线程（EventGetData 侧消费），替代每帧抢 DsCore 全局锁；
- `ReceiverStats` 改 per-thread 原子计数，聚合时读取（统计读取不在热路径）。

`GvspReassembler` 本身无锁需求（每流一个实例，天然线程封闭），不动。

## 4. 控制面配套（M3 依赖重排）

| 项 | 与 100G 的关系 |
|---|---|
| M3-1 丢包重传 | 从"可选"升为"高速必配"：100G 下内核丢包率再低也每秒丢包，无重传=持续掉帧。含重排窗口（乱序包暂存） |
| M3-2 包大小协商 | 巨型帧协商必选；同时把 SCPS 协商挂接点从 `GevDataStream::start` 实装 |
| M3-4 多流/多线程 | 本设计的 L0 层（§2）；与 GenTL 多 DataStream 语义对齐 |
| 相机多流通道发现 | GVCP Device Capability/StreamChannelCount 枚举，XML 里建多个 DataStream 条目——新增工作项 |
| sim 端配套 | gentl-simulator 支持多流通道 + gvsp_flood 已支持多流发送，可用于回归 |

## 5. 部署侧清单（代码之外，验收前置）

- 网卡：RSS 开启且队列数 ≥ 流通道数；巨型帧 MTU 9K/16K；中断 moderation
  调高；DPC 亲和与接收线程错开；BIOS 开 NUMA 对齐。
- 交换机：全链路巨型帧 + 足够 buffer；相机直连可免。
- 相机：多流通道模式；PTP 如需。
- 核数预算：L0@100G 约需 8–12 个专用核（接收+重组+memcpy）；L1 约 4–6。

## 6. 里程碑拆分

1. **M3-4a（L0，先做）**：多流通道接收 + §3 去锁 + `SO_RCVBUF`/绑核——
   纯 std、模拟器可回归，目标本机 ≥30Gbit/s 聚合；
   **2026-10-16 进展**：多流通道扇出已完成——producer 单 DataStream 内部
   扇出 K 个 GVSP 通道（每通道 socket+线程+重组器，共享 DsCore），
   `GENTLITE_GEV_STREAM_CHANNELS` 控制期望通道数（默认 8，0=探测全部）；
   sim 改为整帧轮转调度器（真实多流相机语义，跨通道块不重叠）。
   110 测试全绿、0 警告；4 通道端到端 48 帧零丢失 lockstep 轮转验证。
   ~~遗留：`SO_RCVBUF`/绑核未做~~ **2026-10-16 已做**（M3-2 A 档：
   `GENTLITE_GEV_RCVBUF_KB` 默认 4096 读回存查、`GENTLITE_GEV_RX_CORES`
   接收线程亲和；connect/SIO_UDP_CONNRESET 动机不成立，不做）。
   遗留：多 tap 并行相机（跨通道块并发）撞 DsCore 单 in-flight 槽，
   per-channel 槽位属后续 DsCore 工作。
2. **M3-1+M3-2**：重传窗口 + 包协商（模拟器扩展丢包注入后回归）；
   **2026-10-19 进展**：两项均完成——M3-2 §11.7 协商（阶段 1）+ M3-1 §16.7
   重传全套（能力门控 R-197ca、fire-and-forget 请求、乱序窗口 1ms/20ms/100ms
   QoI、trailer-packet_id 尾部缺口登记、CR-204st/CR-205st 标记与错误包、
   sim 块缓存重发 + 丢包注入）；sim `set_packet_drop` 注入 (1,3)/(1,7) 的
   端到端回归（`tests/resend.rs`）证明块 1 完整恢复；132 测试全绿、0 警告；
3. **M3-4b（L1）**：RIO FFI 后端 + 批量完成收割；本机/双机 NIC 环境实测；
4. **M3-4c（L2/L3，按需）**：XDP-Win 后端（ConnectX 环境）/ Rivermax
   商业接入。真机 25G+ 相机验收（P1/P2）穿插进行。

## 7. 调研出处（2026-10-16）

- RIO API 官方文档：https://learn.microsoft.com/en-us/previous-versions/windows/it-pro/windows-server-2012-r2-and-2012/hh997032(v=ws.11)
- Windows/Linux 高性能 UDP 服务器（RIO/IOCP 实践）：https://blog.grijjy.com/2018/08/29/creating-high-performance-udp-servers-on-windows-and-linux/
- xdp-for-windows（Microsoft）：https://github.com/microsoft/xdp-for-windows
- AF_XDP 性能论文（LPC'18）：http://vger.kernel.org/lpc_net2018_talks/lpc18_paper_af_xdp_perf-v2.pdf
- Rust AF_XDP 库：xsk-rs https://github.com/DouglasGray/xsk-rs ；afxdp https://lib.rs/crates/afxdp ；xdp-rs https://github.com/sudachen/xdp-rs
- DPDK Rust 绑定现状（2025 邮件列表，未成熟）：https://mails.dpdk.org/archives/dev/2025-March/316367.html
- Rivermax SDK：https://developer.nvidia.com/networking/rivermax
- EVT 100GigE FAQ（Windows 需 Rivermax）：https://emergentvisiontec.com/tech-portal/10gige-25gige-100gige-vision-for-all-applications/
- XDP 通用性能参考（native 模式 ~24Mpps/核）：https://www.minzkn.com/linuxkernel/pages/xdp.html

## 8. 基线实测（2026-10-15，本机环回，9KB 包，12MiB 帧）

| 形态 | 发送 | 接收 | 说明 |
|---|---|---|---|
| 单流（现生产架构） | 27.2 Gbit/s | **10.7 Gbit/s / 0.15 Mpps / 1.33 GByte/s** | 1169 块 incomplete（socket 缓冲溢出） |
| 4 流 | 76 Gbit/s | 10.3 Gbit/s / 0.53 Mpps / 1.26 GByte/s | pps ×3.5，总量持平 → 锁/拷贝瓶颈证实 |
| 单流 + 16K 巨型帧 | 54.8 Gbit/s | **23.5 Gbit/s / 0.18 Mpps / 2.92 GByte/s** | 包长 ×1.78 → 吞吐 ×2.2：**单线程瓶颈是每包开销，不是 memcpy** |
| 4 流 + 16K 巨型帧 | 137 Gbit/s | **~15.8 Gbit/s / ~0.45 Mpps / ~1.96 GByte/s**（窗口折算） | **环回多流不升反降**：环回无 RSS，全部流挤同一条内核路径与 loopback 锁；扇出的 RSS 收益只能真机验证 |
| 单流 + 16K 巨型帧（aarch64 环回） | 31.9–32.2 Gbit/s | **22.1–22.2 Gbit/s / 0.17 Mpps / 2.75–2.77 GByte/s** | Orange Pi 5 Plus（Armbian 26.5.1，rustc 1.85.1）；两次复测（端口 50012/50022）稳定；~2900–2950 块交付，~0.1% incomplete（发送端 >30G 压满环回时内核缓冲溢出），0 underrun |

aarch64 环回实测备注（2026-10-21，Phase A Linux 移植验收）：
- 工具与方法同本节（`gvsp_perf_rx --port <P> --streams 1 --seconds 14
  --buf-count 32` + `gvsp_flood 127.0.0.1:<P> --seconds 10 --payload 16080`）；
  两次复测数字见上行。
- 部署侧调优（同 §5 清单性质）：`net.core.rmem_max`/`wmem_max` 提到 32MB，
  使 `GENTLITE_GEV_RCVBUF_KB=4096` 的读回值与 Windows 侧深缓冲等效（Linux
  默认 rmem_max 212992 会把请求钳到 ~416KB——SO_RCVBUF 语义差异见
  `crates/gentl-gige/src/sys.rs` QoI 注释）。
- 结论：aarch64 单线程接收与 Windows 本机 16K 基线（23.5 Gbit/s / 0.18 Mpps）
  同量级，"每包开销是单线程天花板"的推论跨平台成立；多流扇出的 RSS 收益
  仍需真机 NIC 验证（环回多流负收益的结论同样适用于 aarch64）。

16K 行推论（2026-10-16，回应"现网卡普遍支持 16128B 巨型帧"）：当前代码
+ 16K 巨型帧已覆盖 10G  with 大余量，环回下单线程摸到 25G 线速的 ~94%；
真机 NIC 路径会打折，但 25G 从"必须改造"降为"真机验收待定"。**包长是
第一杠杆**：M3-2 包大小协商的优先级相应上调，全链路（相机 SCPS 能力、
交换机 MTU——不少工业交换机封顶 9K/10K，需核实）必须端到端支持。

4 流行推论（2026-10-16）：环回上多流是**负收益**（内核路径共享，非 RSS
场景）；多通道扇出的吞吐价值只在真 NIC 上成立（不同四元组散到不同硬件
队列/核）。正确性已由 4 通道端到端 lockstep 测试证明。环回压测注意两点：
(a) 端口被上一实例快速重复绑定后环回可能停止投递（实测，换端口即恢复）；
(b) 测量窗口须排除 cargo 编译时间，否则数字被稀释。

工具：`cargo run --release --example gvsp_perf_rx -- --streams N` +
`cargo run --release --example gvsp_flood -- 127.0.0.1:PORT --streams N`
