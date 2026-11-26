# linux-vgige 设计台账：智能相机网关

> 状态：v1 ✅ + Step 1 传输无关拆分 ✅（2026-10-23，Windows 全
> workspace 143 测试全绿 0 警告；aarch64 板侧 SSH 配方已通，四 crate
> 测试复核见 §6.3）。
> 纪律同主仓库：纯规范实现、0 警告、双平台编译、注释引规范、不猜。
> 代码布局：`vgige/`（4 crate）——本文"linux-vgige"指网关整体。
>
> **2026-11-26 对账说明**：本文件主体是 2026-10-23 的设计台账。2026-11-10
> 仓库重构后 crate 位于 `gateway/`（core/backends/cli）+ `virtual/`
> （gige/u3v/null），`vgige/` 路径只存在于历史段落；§5 限制清单与 §8 矩阵中
> 标注「2026-11-26 更新」的条目已不再成立。当前权威状态见
> docs/00-governance/remaining-work-checklist.md。

## 1. 架构（Step 1 起：一个传输无关核心 + 前后端适配器）

Orange Pi 双网口盒子：对外网口扮演虚拟 GigE 设备（控制面可定制），
对内驱动真实相机，图像流经盒子重组后以虚拟身份重发。

```
远程客户端 ──GigE──▶ [up]  vgige-front-gige (GvcpServer + GigEFront
                        │     + GvspSession: 3.0 式编码 EI=1/20B 头)
                        ▼
                      vgige-core（传输无关）
                        │  RegisterSpace: 地址分发(透传/定制窗口/XML/流寄存器)
                        │  Relay: 虚拟通道0 + Source→环(4)→Session 驱动
                        │  XmlMerger / CanonicalFrame / CameraBack 等接缝
                        ▲
        back-gige (GigeBack: GVCP 控制透传 +      back-u3v (U3vBack:
        │  GigeFrameSource: GVSP 1.2/3.0 重组)     │  gentl-u3v client
        ▼                                        ▼  facade 之上:
     [down] ──GigE──▶ 真实相机                 U3vCamera/U3vStream:
                                               SIRM 编程 + 块拉取 → CanonicalFrame)
                                              ──USB3──▶ 真实 U3V 相机
装配：vgige-cli（参数/netid/signal/装配；--back gige|u3v 按编译特性二选一——
两个 GenTL producer 的 #[no_mangle] 导出同图链接会符号冲突，见 §7）
```

接缝纪律：无插件注册、无动态发现；trait 只在存在实际第二个实现前景处
存在（`CameraBack`/`FrameSource` ↔ back-u3v Step 2；`FrameSession` ↔
front-u3v Step 3）。`RegisterSpace` 是 GVCP 寄存器模型的具体类型（USB
gadget 前端不经过它），故不为 trait。

启动序列（`vgige-cli/src/main.rs`）：`GigeBack::connect`（GVCP 连接 +
getsockname 定对内源 IP）→ CCP(EA) + DownGuard 孤儿守卫 → 1s 稳定窗
（LeadingOE 固件 EA 后立即事务超时，Q35 族）→ netid 解析对外 MAC →
`back.fetch_xml()`（local: URL + 512B 分片 READMEM + zip 透明解压）→
`XmlMerger::new` 注入 → `Relay::new` + `RegisterSpace::new` +
`assemble_identity`（bootstrap 组装，IP/MAC 覆盖）→ `GigEFront` →
`GvcpServer::start_bound(up_ip, …)`（**绑定对外 IP 而非 0.0.0.0**）→
`VGIGE_READY` 就绪行。SIGINT/SIGTERM（unix, libc signal → AtomicBool
轮询）优雅退出：停服务 → `Relay::drop` 级联关流（虚拟 SCP0=0 → 停驱动
线程 → 真机关流，R-091ca）→ CCP=0（guard 兜底）。

## 2. crate 与接缝 API

| crate | 内容 | 关键 API |
|---|---|---|
| `vgige-core` | 传输无关核心 | `trait CameraBack`（read_reg/write_reg/read_block/write_block/fetch_xml/**fetch_identity**/open_stream/close_stream，`Result<_, u16>` GEV 状态码）；`trait FrameSource: Send`（`next_frame(timeout) -> Option<CanonicalFrame>`）；`trait FrameSession: Send`（`open(payload_hint)`/`send_frame(&CanonicalFrame)`/`close()`）；`CanonicalFrame`；`RegisterSpace`；`XmlMerger`；`Relay`（`StreamRegs`/`virtual_payload_chunk`/`SessionFactory`）；常量 `CUSTOM_BASE/XML_BASE_V/VGIGE_VERSION/slot::*` |
| `vgige-back-gige` | GigE 后端适配器 | `GigeBack::connect`、`fetch_xml_result`、`fetch_identity`（Table 36-1 字符串 + Version）、`GigeFrameSource`（GVSP 重组，Drop 打印统计） |
| `vgige-back-u3v` | U3V 后端适配器（Step 2） | `U3vBack::open(instance_id)` / `from_pipes`（fake/嵌入式传输）；`fetch_identity` 直接读 ABRM/SBRM 快照（零线上读取）；GenCP→GEV 状态码映射（模块注释映射表）；`U3vFrameSource`（块拉取 → CanonicalFrame；transport/desync 后 dead） |
| `vgige-front-gige` | GigE 前端适配器 | `GigEFront`、`GvspSession`、`assemble_identity`、`UpIdentity{ip,mac,mask}`；身份组装 = `back.fetch_identity()` + 本地合成 device_mode/ip-config（虚拟设备的自有声明，不再镜像相机网络值——GigE 格的子网/网关来源因此从"相机"变为"对外网口"，见 §7 Step 2） |
| `vgige-cli` | 二进制 + e2e | `main.rs`（`--back gige|u3v` 编译特性门控、GigE 侧 CCP EA + DownGuard + 1s 稳定窗仅 gige 生效）；`netid`（`resolve_up_link` → MAC+掩码）；`signal`；`tests/relay_e2e.rs`、`tests/gateway_u3v_e2e.rs`（fake GenCP 设备） |

**gentl-u3v client facade**（`crates/gentl-u3v/src/client.rs`，pub 模块）：
库形态 U3V 客户端， motiviation：GenTL producer 面是句柄形态且被 cti 层
绑定；网关需要的是"枚举→打开→GenCP 读写→XML→SIRM 开流→拉块"。内部
零协议复制——全部复用现有 control/packets/stream/usbio 机制：
`U3vCamera::{enumerate,open,from_pipes}` + `read/write_reg/mem`（LE，R-30cd，
原始状态码经 `transaction_raw` 保留为 `U3vError::Status`）+ `fetch_xml`
（manifest→XML，zip 透明）+ `stream(channel)`；`U3vStream::{start,stop,
next_block(timeout)}`（SIRM 编程 §5.4.1.2 + 内存汇块读取 `read_block_alloc`
——stream.rs 零拷贝 `read_block` 的库形态姊妹实现，同 §5.2.1.3/R-127sr
算法；超时用看门狗线程表达为停止旗标，Q38）。

## 3. 寄存器窗口表（对外可见地址空间）

| 地址 | 归属 | 语义 |
|---|---|---|
| `< 0x2000_0000`（除下述虚寄存器） | 透传 | read/write/mem 原样转发真机，GEV 状态码原样映射（真机 0x8006 → 虚拟侧同码）；非 GEV 失败（超时/IO）→ 0x8FFF（附录 B.21，regmap 同款） |
| `0x0008/0x000C` DEVICE_MAC | 本地 | 对外网口真实 MAC（虚拟身份；不泄露真机 MAC） |
| `0x0024` CURRENT_IP | 本地 | 对外网口 IP |
| `0x0200..0x0400` FIRST_URL | 本地 | 指向注入后 XML：`local:vgige.xml;20001000;<len>` |
| `0x0934` GVCP_CAPABILITY | 混合 | 透传真机值并清 `packet_resend` 位（虚拟侧不应答 PACKETRESEND_CMD，R-197ca） |
| `0x0A00` CCP | 本地 | sim 同款简化模型：EA 优先/CA 授予、同请求位刷新接受（R-069cd）、0 释放、连接计数只增不减（§36.46 低 nibble）；无 peer 身份（Q7 同款注明） |
| `0x0D00/0x0D04/0x0D08/0x0D18` SCP0/SCPS0/SCPD0/SCDA0 | 本地（虚拟通道） | 不转发；SCP≠0 开流、SCPS 本地写后读回即所写值；首个 GVSP 帧发送前允许 SCPS 写（§11.7 协商窗口），开始发送后返回 `BUSY`（§36.58）；SCPD 包间延迟在设备提供 timestamp tick 时生效；fire 位自清并由前端发 test packet；通道 x>0 → `INVALID_ADDRESS`（§36.57 探测语义，v1 单通道） |
| `0x2000_0000` | 本地 RO | VgigeVersion = 0x0001_0000（1.0.0, major<<16\|minor<<8\|patch） |
| `0x2000_0004` | 本地 RO | AI_Result（0 未检/1 OK/2 NG），业务钩子占位 |
| `0x2000_0008` | 本地 RO | AI_Score（异常分×1000），业务钩子占位 |
| `0x2000_000C` | 本地 RO | FrameCountRelayed（每转发一块 +1） |
| `0x2000_0010` | 本地 WO | UserCommand：写接受并记日志（业务占位），读回存储值 |
| `0x2000_0014..0x2000_0FFF` | 本地 RW | 通用槽位，上电清零，写 RO 槽 → `WRITE_PROTECT` |
| `0x2000_1000..` | 本地 | 注入后 XML blob；READMEM 服务，跨尾读零填充（GenTL §6.3.6.5 对齐窗口适应）；寄存器访问 → `INVALID_ADDRESS` |
| 其余 ≥0x2000_0000 | 本地 | 未映射（blob 尾之后）→ `INVALID_ADDRESS` |

透传边界语义：客户端 `AcquisitionStart` 等厂商命令寄存器（0x1000_xxxx）
自然透传，远程采控对真机原样生效。真机流已自动运行（通道开即出流，
如 sim Q15）时无需额外动作；v1 不代写厂商寄存器（地址相机相关，不猜）。

## 4. 流中继线格式

- 对内自适应：逐包 EI 判别（3.0 §24.2 / 1.2 §24 Fig 24-1），重组
  stride 首包推断，gap 即弃块（v1 无 §16.7 重传调度）。
- 对外恒 3.0：20B EI=1 头 + 36B §25.2.1 leader body（真机 leader 元数据
  重编码）+ payload 原字节 + 8B §25.2.3 trailer；block_id 自 1 递增
  （R-259st），包 id leader=0/payload 从 1/trailer=末+1（R-264st）。
- 切片按虚拟 SCPS0（远程所写值，默认 1500 总包长 → 1452 payload；
  <48+1 的荒谬值回退 1452，Q21 类回退）。
- 背压边界 = 4 帧有界环（sync_channel(4)），阻塞等待发送线程+可停取消。

## 5. 已知限制（v1 清单，Step 1 后仍然成立）

1. **单客户端**：GVCP server 单线程逐请求处理；下游相机侧 CCP(EA) 由
   网关常驻持有，虚拟 CCP 只做展示语义（多客户端抢占不在 v1）。
2. **单通道**：仅虚拟通道 0；x>0 寄存器访问 → `INVALID_ADDRESS`。
3. ~~zip XML 不支持~~ **已解决（2026-10-22 真机整改）**：zip 透明解压
   （§14.1.1 manifest 格式 1，GenCP §5.5.1.2），首 entry 即相机 XML。
4. ~~**无对外重传调度**：虚拟设备不 advertise packet_resend（已掩
   GVCP_CAPABILITY 位，R-197ca）；PACKETRESEND_CMD 到虚拟侧被忽略
   （请求方自行超时）。对内同样无重传（重组 gap 弃块）。~~
   **2026-11-26 更新：已实现。** 虚拟 GigE 前端实现 §16.7 PACKETRESEND
   块缓存（8 块/256 MiB 上限）并广播能力位，板端 `/TEST_NETWORK` 由
   91 Pass/5 FAIL 变为 96/0；对内重组仍无重传（gap 弃块）。
5. ~~**多根 Category QoI**：注入的 SmartCamera Category 未挂入真机 XML
   的 Root pFeature 链（多数客户端按节点名/类别遍历可见；挂 Root
   需改真机 XML 字符串结构，留 v2）。~~
   **2026-11-26 更新：已实现。** `XmlMerger::attach_to_root` 把注入的
   SmartCamera Category 挂进 Root 的 pFeature 链（`gateway/core/src/xml.rs`）；
   不挂链的节点在 NI/pylon 中不可见。
6. **重组缓冲固定 16 MiB/帧 ×4**：超限帧丢弃并计数（`frames_dropped_too_small`，
   收流侧 Drop 时日志可见）；无增长路径。
7. **未知 PFNC 格式转发为空 payload**（payload_bytes 不可算，
   Q21 同类）；Mono8/Mono16 等已知格式完整转发。
8. **fire_test_packet 在通道打开且尚未开始发送时可发测试包**；SCPS 在首个
   GVSP 帧之后写返回 `BUSY`（§36.58）；虚拟 SCPS 的 DF 位不应用。
9. **透传非原子**：多寄存器 WRITEREG 混合命中透传+本地窗口时逐条执行、
   首错即停（与 GVCP 服务器语义一致；跨窗口混合读写极少见）。
10. **Windows 无优雅停机**：Ctrl+C 默认硬终止（关流/CCP=0 只走 unix
    SIGINT/SIGTERM 路径）；netid 的 Windows 路径跳过环回适配器
    （无物理地址），Linux lo 可解析（MAC 全零）。
11. **GigE 侧写块未通**：`CameraBack::write_block` 在 GigE 适配器回
    `NOT_IMPLEMENTED`（gentl-gvcp 客户端无 WRITEMEM 路径）；接缝为
    U3V（块式寄存器访问）预留——back-u3v 已接通（GenCP WRITEMEM）。
12. **back-u3v 真机未验收**（Step 2）：矩阵 "GigE 前端 ← U3V 相机" 格
    只有 fake GenCP 设备 e2e（vgige-cli/tests/gateway_u3v_e2e.rs），
    真机全流程待 U3V 相机插板（Q38 回填项）。
13. ~~**CLI 后端按编译特性二选一**（`--no-default-features --features
    back-u3v`）：双 GenTL producer 同图链接符号冲突所致（Q38-7）；
    集成测试不受影响（独立链接单元）。~~
    **2026-11-26 更新：已解除。** 两个 producer 的 GenTL 导出由
    `cti-exports` feature 隔离（vgige 依赖时 `default-features = false`），
    6 种 back×front 组合均可链接；Q38-7 的"同图符号冲突"只适用于同时
    开启 cti-exports 的情形。
14. **U3V 侧仅标准 payload 模式**：GenDC container/flow 由
    `program_sirm` 干净拒绝（与 GenTL producer 同一语义）；未知 PFNC
    格式经 SI Required Payload Size 转发（§5.4.1.3）。
15. ~~**front-u3v 整机待验**（Step 3）：协议核心全链路可测（配对
    测试=真客户端 ↔ 设备状态机），gadget 物理层（configfs +
    FunctionFS）结构化但未在 Orange Pi dwc3 上 bring-up；运行前提：
    libcomposite 加载、configfs 挂载、Type-C 口 dwc3 UDC（Q39）。~~
    **2026-11-26 更新：已在 Orange Pi 5 Plus 上整机验收**（dwc3/configfs/
    FunctionFS 打通、Type-C SuperSpeed、`grab_u3v` 500/500 帧 ×2；见
    docs/20-design/u3v-gadget-bringup-pitfalls.md 与 board/orangepi5plus/README.md）。
16. **front-u3v 变高帧不转发**：设备按已编程几何出块、尾短补零
    （`--u3v-payload` 是帧大小契约；CR-86st 高度修正属主机侧语义）。
17. ~~**矩阵一格不可单体链接**："GigE 相机 → U3V 设备"需要
    back-gige + front-u3v 同图——双 GenTL producer 符号冲突
    （Q38-7），编译期报错拒绝；v1 该格由 relay_e2e（真 back-gige +
    front-gige）与 front-u3v 套件拼合覆盖。~~
    **2026-11-26 更新：已可链接。** `cti-exports` feature 隔离后
    back-gige×front-u3v 可构建（`gateway/cli/Cargo.toml` 的 features 注释；
    `main.rs` 的 compile_error 只要求至少一个 back/front）。该组合仍未做
    真机验收（remaining-work-checklist A7 仅模拟器通过）。

## 6. 验证记录

### 6.1 测试

- `gateway/cli/tests/relay_e2e.rs`（双平台、无硬件，sim 扮演真机）：
  ① 读写透传 + 虚拟身份（IP/MAC/Capability 掩码/CCP 本地语义）
  ② 错误透传（0x8003/0x8004/0x8005/0x8006 双边一致）
  ③ 定制窗口（RO/WO/RW 语义 + 业务钩子）+ XML 拉取（含注入节点、
  真机节点保留、尾读零填充）+ §36.57 通道探测
  ④ 流中继：**sim 以 1.2 legacy 头发流** → 断言对外恒 3.0 式
  （EI=1/20B 头/**36B** leader；旧稿写 40B，2026-11-26 按 Figure 25-2 更正）、block_id 自 1 递增、按虚拟 SCPS0=348
  切片（≤300B/packet）、payload 字节与 sim 图案一致、
  FrameCountRelayed 增长、R-586cd BUSY、关流级联（真机 SCP0=0 +
  接收 socket 静默：先冲刷 close 前在途数据包再监听——sim 每 1ms 出块，
  静默断言针对级联后的新流量）。
- 单元：core/relay（SCPS 切片数学、默认寄存器）、core/xml（注入平衡性/
  内容/ext 片段/缺尾报错）、back-gige（local: 解析）、cli/netid（MAC
  解析、未知 IP 干净报错）。
- 二进制冒烟：`sim_server` + `vgige-cli --up <本机IP> --down
  127.0.0.1 --ext-nodes …` → `VGIGE_READY` 就绪行（真实 MAC、注入后
  XML、5 节点）。

### 6.2 真机验证记录（2026-10-22）

- **环境**：Orange Pi 5 Plus 双网口；对外 192.168.137.55，对内 192.168.1.89
  （产线随机机 00:30:53:9b:db:6c）
- **结果**：`linux-vgige --up 192.168.137.55 --down 192.168.1.89` 启动 →
  `VGIGE_READY xml=117884B nodes=5` → 远程客户端（grab_camera 经
  GENTLITE_DISCOVERY_TARGETS 指向虚拟设备）**1000/1000 帧完整**：
  发现/CCP/XML（zip 解压+5 注入节点）/参数透传/开流级联/图像转发全链路
  （真机 1.2 头进 → 3.0 头出，1280x1024 Mono8）
- **过程中修复**：①zip XML 透明解压（产线机 First URL 为 .zip）②READMEM_CHUNK
  65524→512（13KB 分片 UDP 是该相机控制面失能的确切触发器，Q35 补充）③CCP 孤儿守卫
- **结论**：智能相机网关 v1 真机毕业。遗留按 §5 限制清单（多客户端/多通道/重传透传等二期）

### 6.3 烤机（compliance/relay-burn-15min-report.md）

44,000/44,000 帧全完整，≈92 fps × 1.31 MB ≈ 0.94 Gbit/s 线速持续
8 分钟，网关 ~0.5 核 A76，内存平坦无泄漏。拓扑：Windows 客户端 ←GigE→
Linux 网关 ←GigE→ 产线相机 192.168.1.96（千兆直连第二网口）。

### 6.4 Step 1/2/3 后双端验证（2026-10-23 / 2026-10-26）

- Windows：`cargo test --workspace` **170 通过 / 0 失败 / 0 警告**
  （Step 3 新增：vgige-front-u3v 13 = lib 6 + pairing 3 + matrix 4；
  gentl-u3v 30 = 28 + 2 设备侧 round-trip）；`--release` 与
  `--all-targets` 0 警告；CLI 特性矩阵 4 个允许组合全部 0 警告，
  禁用组合（back-gige×front-u3v）编译期报错干净。
- aarch64 板（Orange Pi 5 Plus，/root/GenTLite-public）：tar 同步 +
  `cargo test -p gentl-u3v -p vgige-core -p vgige-back-gige
  -p vgige-back-u3v -p vgige-front-gige -p vgige-front-u3v
  -p vgige-cli` → **60 项全绿**（gentl-u3v 31＝Linux 侧多
  root_hub/discovery 用例 + 设备侧 2；vgige 套件 29），0 失败
  0 警告。

## 7. 变更面

- **Step 0（2026-10-22）**：单体 crate `linux-vgige/`（workspace
  member）：lib（handler/xml/relay/netid/signal）+ bin（main.rs）+
  tests/relay_e2e.rs；`crates/gentl-gvcp/src/server.rs` 新增
  `GvcpServer::start_bound(bind_ip, port, handler)`（`start()` 语义
  不变）。真机整改：zip 透明解压（zip crate）、READMEM 512B 分片、
  CCP 孤儿守卫（DownGuard）、EA 后 1s 稳定窗。
- **Step 1（2026-10-23）**：单体拆为 `vgige/` 4 crate（见 §1/§2）；
  根 workspace members 替换；`linux-vgige/` 目录删除。零行为变化：
  全部测试语义原样（唯一测试侧修正：静默断言改冲刷后监听，消除
  close 在途数据包的既有竞态）。gentl-gvcp 及其余 crate 自 Step 0
  起无改动。
- **Step 2（2026-10-26）**：
  - `crates/gentl-u3v`：新增 **pub `client` facade 模块**
    （`U3vCamera`/`U3vStream`/`Block`/`U3vError`，re-export
    `UsbPipe`；`from_pipes` 测试/嵌入式构造）；`control.rs` 加
    `transaction_raw`/`read_mem_raw`/`write_mem_raw`（producer 路径
    零变化）；`stream.rs` 抽出 `read_sirm`/`program_sirm`/
    `disable_sirm`（`U3vDataStream` 与 facade 共用，`TransferGeometry`
    pub(crate)）。gentl-u3v 28 项测试零回归。
  - 新增 `vgige/vgige-back-u3v`（`CameraBack`/`FrameSource` 实现 +
    GenCP→GEV 映射表）；`vgige-core` 的 `CameraBack` 加
    `fetch_identity` + `BackIdentity`；front-gige 的
    `assemble_identity` 改经 fetch_identity 并本地合成 device_mode/
    ip-config（**GigE 格 discovery 子网/网关来源从相机改为对外网口**，
    Q38-5）；`vgige-cli` 加 `--back gige|u3v`（编译特性二选一：双
    producer 的 #[no_mangle] 导出同图链接符号冲突，Q38-7）+
    `netid::resolve_up_link`（MAC+掩码）；新增
    `tests/gateway_u3v_e2e.rs`（fake GenCP 设备，4 项断言）。
- **Step 3（2026-10-26）**：
  - `crates/gentl-u3v`：packets.rs 补设备侧编解码
    （CommandCcd::decode/AckCcd::encode/SCD 解析/ack 构造/leader/
    trailer 编码，2 项 round-trip 测试），`packets`/`descriptors`
    模块转 pub；gentl-u3v 30 项零回归。
  - 新增 `vgige/vgige-front-u3v`：两层分离——`device/`
    （`regs.rs` 寄存器文件[ABRM 按规范偏移布局]/`server.rs` GenCP
    命令服务/`emit.rs` §5.5 块发射器[FrameSession 实现]/
    `desc.rs` §3.2 描述符构造，4 项字节级测试）跑在字节流 trait 上；
    `gadget.rs` Linux FunctionFS 物理层（open/serve 两段，整机待验）。
    `tests/pairing.rs`（王冠：↔ gentl-u3v 真客户端全链路 3 项）+
    `tests/matrix_e2e.rs`（矩阵格 4 项，StubBack 扮演下游）。
  - `vgige-core`：`Relay::open()` 编程式开关（front-u3v 的
    Stream Enable 触发路径）。
  - `vgige-cli`：`--front gige|u3v` + `--u3v-payload`；特性矩阵
    default=back-gige+front-gige；禁用组合（back-gige×front-u3v）
    编译期报错；3 个允许的非默认组合均 0 警告。

## 8. 路线图

**特性矩阵终态（cargo 特性 × 运行时 `--back`/`--front`）**：

| 组合 | 构建 | 运行时 |
|---|---|---|
| back-gige × front-gige | `default` | `vgige --up … --down <相机IP>` |
| back-u3v × front-gige | `--no-default-features --features back-u3v,front-gige` | `--back u3v --down <bus:addr>` |
| **back-gige × front-u3v** | ✅ 可构建（2026-11-26 更新：`cti-exports` feature 隔离；v1 时期的编译期拒绝已解除） | `--down <相机IP> --front u3v`（真机验收待 A7） |
| back-u3v × front-u3v | `--no-default-features --features back-u3v,front-u3v` | U3V 真机闭环（Q38 回填） |
| **back-null × front-gige** | `--no-default-features --features back-null,front-gige` | `--back null`（无相机演示） |
| **back-null × front-u3v** | `--no-default-features --features back-null,front-u3v` | `--back null --front u3v --u3v-payload 1310720`（**gadget 整机演示形态**） |

**back-null 合成模式**（2026-10-26 加，`vgige/vgige-back-null`）：无相机
时网关照跑——内置最小 GenApi 合法 XML（GenTLite Virtual 身份 +
Width/Height/PixelFormat，**不含** SmartCamera：由 XmlMerger 照常注入）
+ 30fps 1280×1024 Mono8 图案源（`(byte_k + block_id) mod 256`，sim 同款
可校验）。寄存器：读 0（XML 广告的三个寄存器按流几何播种 1280/1024/
Mono8）/写 no-op。XML 声明 LittleEndian（演示目标是 U3V 设备 front，
R-30cd；经 GigE front 的数值读会字节交换——demo 限制，crate 注释已标）。
`--down` 在 `--back null` 下可省略（缺省 "null"）。

| Step | 内容 | 接缝落点 |
|---|---|---|
| **1 ✅** | 单体 → 传输无关核心 + GigE 前后端（2026-10-23） | `CameraBack`/`FrameSource`/`FrameSession` |
| **2 ✅ 代码完成（2026-10-26）** | **back-u3v**：U3V 相机后端。gentl-u3v 加 pub `client` facade；`vgige-back-u3v` 实现 `CameraBack` + `FrameSource`；fake GenCP 设备 e2e 4 项；CLI `--back u3v`。**真机验收待 U3V 相机**（Q38） | 同左 |
| **3 ✅ 代码完成（2026-10-26）** | **front-u3v**：U3V 设备模拟前端。两层分离：`device/`（ABRM/SBRM/SIRM 寄存器文件 + GenCP 命令服务 + §5.5 块发射器 + §3.2 描述符构造，纯内存可测）+ `gadget.rs`（Linux FunctionFS 物理层）。配对测试（王冠）：device/ ↔ gentl-u3v 自家 U3vCamera 全链路；矩阵格 e2e 4 项。**整机集成待验**（Q39：dwc3/configfs bring-up；板端 UDC `fc000000.usb` 已出现，back-null 演示命令见矩阵表） | `FrameSession` + `Relay::open()` 编程式开关 |
