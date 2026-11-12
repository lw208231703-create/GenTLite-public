# 遗留事项清单（Pending Work）

> 状态截至 2026-11-04。协议栈纯规范零非标兼容；workspace 测试全绿。
> GigE 模拟链路 + U3V 海康真机已毕业；以下事项按优先级排列。
> 每条注明：前置条件、验收标准、关联 OPEN-QUESTIONS。

## 〇、2026-11-04 vgige 虚拟相机里程碑

已完成：

- Orange Pi 5 Plus / vendor `6.1.115-rk35xx` / Type-C SuperSpeed；
- 虚拟 U3V + GigE，`1280×1024 Mono16`，30 fps，payload `2,621,440`；
- GigE 端序、NI XML 缓存隔离、legacy 1.2 GVSP、1464 payload 切片；
- NI-IMAQdx U3V/GigE 各 100/100 帧；
- GenTLite GigE/U3V 5 帧与 NI、模拟端逐字节 0 差异；
- `u3vffs` vendor 模块、DTB overlay、systemd/udev 恢复资产归档到
  `board/orangepi5plus/`；
- 完整踩坑记录：`docs/vgige-board-kernel-interop-2026-11-04.md`。

待收尾：

- NI test packet 已由 GigE front 的设备侧 `SCPS fire_test_packet` 实现，
  NI 5/5 帧验证通过；`.icd` 的 `PacketSize=1500` 仍建议保留。
- MVS/LD 的 16-bit 显示限制；Null 已增加 Mono8/Mono16 可选模式。
- GenTL U3V 与 NI-IMAQdx 驱动互斥；切换流程见记录文档。
- LD 重复设备需用当前 CTI 复测。
- 板端 vendor 内核/DTB/u3vffs 资产已归档；`install.sh` 自动安装脚本已加入。

### 2026-11-04 无真机收尾已完成

- `cargo cti` / `gentl-packager` 跨平台：Windows `.dll`、Linux `.so → .cti`。
- `TL_INFO_PATHNAME` 在 Windows `GetModuleFileNameW`、Linux `dladdr` 实现。
- Null 支持 Mono8 / Mono16 可写切换，PayloadSize 随格式变化。
- `FrameMeta` 携带实际 payload 字节数，未知 PixelFormat 的
  `BUFFER_INFO_SIZE_FILLED` 不再为 0。
- GVCP `write_reg_from` + CCP peer 绑定/过期测试。
- `board/orangepi5plus/install.sh` 自动安装板端资产。

## 一、U3V 收尾（相机在手边，半小时级）

### U-A. 异常路径验证（手册 U7）
- **内容**：拔线→恢复（UQ6 ENDPOINT_HALT 重同步是否失速）；接收线程死亡→引擎停止（C16 修复的真机验证：DSGetInfo(IS_GRABBING)=0、EventGetData 返回 ABORT）；启停循环（R-77st block_id 复位）
- **验收**：每项行为与修复设计一致，结果回填 UQ6/C16

### U-B. 第三方 consumer 验证（手册 U10）
- **内容**：pylon 加载 `gentl_u3v.dll` 建 nodemap 抓流（GenTL 语义终极第三方验证；`Local:` URL 消费见 UQ4）
- **验收**：pylon Viewer 出图

## 二、GigE 真机验收（等合规相机；自研相机等固件）

### P1. 真机收流验收 ~~（README M2 收尾）~~ **✅ 2026-10-19 完成：1000/1000 帧零丢失**（自研改良相机 192.168.1.92，1280x1024 Mono8，1.2 式 8 字节头；固定包长模式。遗留：自动协商被 Q35 固件 fire 缺陷阻塞，待厂家修复）
- **前置**：① 合规 GigE 相机（正规海康/Basler/大恒），或 ② 自研相机（**2026-10-16 Q31 更正：SCP/SCPS 位布局整改要求已撤销**——厂家原实现合规，我方 producer 已修正）
- **内容**：`grab_camera` 跑"枚举→打开→AcquisitionStart→收 1000 帧零丢帧"
- **工具就绪**：`grab_camera` / `dump_cti` / 预飞报告 §5.2 手册（G1-G9）
- **注意**：防火墙放行相机 IP 的 UDP 入站

### P1.5. 自研相机固件整改跟踪（非本仓库）
- 未整改：~~SCPx 位布局（#1）、SCPSx 位布局（#2）~~（Q31 更正：非违规，已撤销）；广播发现应答（#4，仍成立）
- 已整改：GVSP 帧结构（1.2 式，合规）、PayloadSize 端序（#5）
- 整改路径选型：保持 1.2（我方补 Q28-3a）或升 3.0（receiver 零改动）

### P2. 三品牌兼容性矩阵（README M3 条目）
- **前置**：P1 完成 + 海康/Basler/大恒各至少一台；报告存 `compliance/`

## 三、M3 生产化（不依赖采购，可随时开工）

| # | 项 | 说明 | 关联 |
|---|---|---|---|
| M3-1 | **GVSP 丢包重传** | ✅ 2026-10-19：§16.7 全套——能力门控（R-197ca，GVCP Capability 0x4）+ fire-and-forget PACKETRESEND（CR-198ca/CR-201cd，GvcpClient::packet_resend）+ 乱序窗口重传（§16.7.3 QoI：1ms 初始 / 20ms 重发 / 100ms 保留，env 可调）+ 尾部缺口登记（trailer packet_id = 末包+1，R-264st）+ CR-204st/Table 19-1 标记 + CR-205st 错误包按不可恢复 + RESEND_RANGE_ERROR（CO-206st）+ S-6 重复包查重销账 + S-7 trailer block_id 不符即弃销账；sim 端块缓存重发 + 0x800C 错误包 + `set_packet_drop` 丢包注入；集成测试 resend.rs（注入 (1,3)/(1,7) 两缺口，块 1 完整恢复） | Q16/Q20 |
| M3-2 | **包大小协商（§11.7）** | ✅ 2026-10-16：test packet 二分（挂接点 `GevDataStream::start`）；`GENTLITE_GEV_PACKET_SIZE` 固定值模式；SCPS 写被拒 → FIRE-only 回退（Q33）；测试包判定 = block_id 0 + 总包长匹配 + test_data 能力的 LFSR 校验（§24.5/§24.5.1，QoI）；sim 发测试包 + 取整 clamp | Q16/Q33 |
| M3-3 | **断线检测与恢复** | 心跳超时、控制通道重建、流复位（S-1/S-2 一并处理） | — |
| M3-4 | **多网卡/多相机并发** | ~~多流通道扇出~~ ✅ 2026-10-16（单 DS 内扇出 K 通道，每通道 socket+线程；sim 整帧轮转；`GENTLITE_GEV_STREAM_CHANNELS` 默认 8）。~~剩余：SO_RCVBUF/绑核~~ ✅ 2026-10-16（M3-2 A 档：`GENTLITE_GEV_RCVBUF_KB` 默认 4096 读回存 ChannelCtx；`GENTLITE_GEV_RX_CORES` 接收线程亲和；connect/SIO_UDP_CONNRESET 动机不成立，不做——stream.rs 模块注释）。剩余：多 tap 相机 per-channel 槽。架构：docs/gige-100g-rx-design.md | — |
| M3-5 | ~~性能对比报告~~ ✅ 2026-10-19 初版：合规/m3-5-perf-report.md。60fps 同帧率下 GenTLite ≈10% 单核（1.37ms/帧）零丢失 vs MVS 进程 ≈0.5%（内核驱动接收，口径差异已注明）；系统级同口径复测留作可选后续 | 依赖 P1 |
| M3-6 | **FrameSink 签名贯通** | 未知格式 `SIZE_FILLED=0` 贯通真实字节数 | Q22 |
| M3-7 | **DeviceHandler peer 上下文** | sim CCP 完整语义 | Q7 |
| M3-8 | **TL_INFO_PATHNAME** | GetModuleFileNameW 取 DLL 路径 | Q10 |
| M3-9 | **1.2 式 GVSP 头接收** | ✅ 2026-10-19：8 字节 EI=0 头 + 36 字节 leader body 收发（1.2 §24 Fig 24-1/24-3，规范渲染图 + Aravis 0.8.30 双核验，简报原引布局已按图更正——Q34）；`decode_auto` 按 EI 位逐包判别；组帧 stride 统一改首 payload 推断（SCPS-stride 传入删除，legacy/3.0 头开销语义 36/48 不再影响组帧）；sim `StreamConfig.legacy_headers` 发射模式（SCPS 语义按 36）；集成测试 `legacy_12_header_stream_end_to_end`（含协商共存）。真机 1.2 相机（Q28，#1 SCP 阻塞解除后）验收 | Q28-3a/Q34 |
| — | **GVRSP（RoCEv2）传输** | 协商/切换骨架 ✅ 2026-10-16：Streaming Capability(0x092C)/Configuration(0x0960) 读写、Table 36-2 校验、R-667cd BUSY、GVRSP-only 设备在 DevOpenDevice 干净报 NOT_IMPLEMENTED（含 sim 三模式 + 端到端测试）。**数据路径未做**：需 WinND SPI FFI + RDMA 网卡 + 无损网络（PFC）+ 对端支持，硬件到位后按 Part 4（§27-35）逐章实现； RxBackend trait 挂接点 = GevDevice.transport / GevDataStream::start 的 match | — |

## 四、U3V 生产化观察项（真机长尾）

| # | 项 | 说明 | 关联 |
|---|---|---|---|
| UQ6 | ENDPOINT_HALT 重同步 | 控制通道失速恢复（U-A 验证时见分晓） | U-A |
| UQ7 | DEI 事件通道投递 | codec 已备，GenTL 事件投递未接（R-28ch 已知偏离） | — |
| UQ9 | 传输粒度特性 | 1MiB TS 已验证可用；高带宽下的粒度调优属 RAW_IO 完整档 | R1 |
| — | GenDC payload 模式 | 干净拒绝（UQ8）；真机有需求再实现 | UQ8 |

## 五、RAW_IO 优化（PENDING R1）

- **最小档：已完成并真机验证**（2026-10-14）：RAW_IO 策略 + MPS 对齐请求 + 非对齐片暂存拷贝；100/200 帧复验通过（见 UQ16）
- **完整档（挂起）**：overlapped 读池 2-4 在途，进一步消除块间空隙；触发条件：高带宽 U3V 相机实测丢帧/吞吐不足

## 六、可选大项（讨论后定）

| 项 | 说明 | 预估 |
|---|---|---|
| **Linux 可移植** | **Phase A ✅ 2026-10-21（GigE 链路 + 公共 crate）**：sys 后端抽象落地（`gentl-gige/src/sys.rs`、`gentl-simulator/src/sys.rs`，cfg 双实现，Windows 代码原样搬入零行为变化）；gentl-u3v 加 Linux 编译桩（枚举空/打开 NOT_IMPLEMENTED，运行时安全）；windows-sys 移入 `[target.'cfg(windows)'.dependencies]`，unix 侧加 libc；consumer_dll 测试双平台（Linux 加载 `libgentl_gige.so` 全量导出回归）。双端验证：Windows 132 测试全绿 0 警告；板子（Orange Pi 5 Plus aarch64）122 测试全绿 0 警告（差 10 = u3v WinUSB 专属 9 + mask_from_prefix 1，均按预期跳过）；aarch64 环回基线入库 docs/gige-100g-rx-design.md §8（22.1–22.2 Gbit/s / 0.17 Mpps）。**Phase B ✅ 2026-10-23 代码完成（U3V rusb/libusb 后端）**：`usbio` 重组为 `UsbPipe` trait（最小面：bulk 读写/flush/策略/MPS）+ `winusb.rs`（WinUSB 原样迁入）+ `libusb.rs`（rusb，链系统 libusb）；control/stream 完全平台无关（GenCP 事务/SIRM 重组经 trait 收发）；Linux 发现按 §3 描述符层一次匹配，Device Info 重序列化喂共享解析器（`descriptors.rs` 双端单测）；UQ16 累积读层下沉 usbio/mod.rs 并新增 8 个 MockPipe 单测双端运行；Linux 宿主侧 QoI 裁定记 OPEN-QUESTIONS Q37，事实回填 u3v-wirefacts.md "Linux 后端事实"节。Windows 150 测试全绿 0 警告（143 基线 + 7 新增）。**遗留：~~板子 aarch64 构建+测试验证~~ ✅ 2026-10-23 晚补齐：板端 0 警告 0 错误（rsproxy 镜像配置于板子 /root/.cargo/config.toml，rusb 链系统 libusb-1.0.28）；`cargo test -p gentl-u3v` 29/29 全绿（packets 10 + descriptors 3 + usbio 累积层 8 + control 2 + stream 2 + lib 2 + linux-discovery 2）；vgige 四 crate 11/11（core 4/back 1/cli 2/relay_e2e 4）；zopfli 0.8.1 钉完好**。**剩余唯一遗留：真机验收待海康相机插板子 USB3 口**（`lsusb` 应见 `2bdf:` 前缀 VID；然后 `cargo run --release --example grab_u3v` 自枚举自抓图（默认 50 帧，自动选第一台），或 `grab_u3v <GUID> 100` 抓 100 帧；或 `dump_cti target/release/libgentl_u3v.so` 看枚举/GUID） | 3-4 天 |
| GenTL 可选特性 | chunk(v1.3)/parts(v1.5)/composite(v1.6)——桩合法返回 NOT_IMPLEMENTED，真机有需求再填 | 按需 |
| M5 社区化 | 英文文档、语义化版本发布、camera_prj 切换吃狗粮 | 按 README M5 |

## 七、已完成（备查）

M1 ✅ / M2 模拟链路 ✅（100 帧零丢帧）/ M4 ✅（海康真机 200/200 + 规范复核）/
预飞审计双路径 ✅（不合规项全修复）/ RAW_IO 最小档 ✅ / Basler 风格命名对齐 ✅

## 八、智能相机网关（vgige，台账 docs/linux-vgige-design.md）

### GW-1. v1 ✅ 2026-10-22（单体 linux-vgige，真机毕业）
- **内容**：Orange Pi 双网口盒子智能相机网关——对外网口 `GvcpServer::start_bound(up_ip, …)` 扮演虚拟 GigE 设备，对内 GVCP 常驻 CCP(EA) 驱动真机（DownGuard 孤儿守卫 + EA 后 1s 稳定窗，Q35 族）
  - 控制面：bootstrap/厂商区透传 + GEV 状态原样映射（错误透传）；虚寄存器（DEVICE_MAC/CURRENT_IP=对外网口真值、FIRST_URL→注入 XML、GVCP_CAPABILITY 掩 packet_resend/R-197ca、CCP 简化模型 §36.46）；定制窗口 0x2000_0000（Version/AI_Result/AI_Score/FrameCountRelayed/UserCommand + 通用 RW 槽）；XML 注入内建 SmartCamera Category + `--ext-nodes` 用户片段
  - 图像面：虚拟通道 0——SCP≠0 开流（R-586cd/BUSY、R-091ca 级联关流），`GvspReassembler` 重组（1.2/3.0 自适应）后**恒以 3.0 式重发**（EI=1 20B 头 §24.2、40B leader §25.2.1、block_id 自 1 R-259st、按虚拟 SCPS0 切片）
  - 真机整改：zip XML 透明解压（§14.1.1）、READMEM 512B 分片（13KB 分片是 LeadingOE 控制面失能触发器）
- **验收**：真机 1000/1000 帧（192.168.1.89）+ 烤机 44000/44000 帧 @≈0.94Gbit/s（compliance/relay-burn-15min-report.md）

### GW-1.5. Step 1 拆分 ✅ 2026-10-23（linux-vgige → vgige/ 四 crate）
- **结构**：`vgige-core`（传输无关：`CameraBack`/`FrameSource`/`FrameSession`/`CanonicalFrame`/`RegisterSpace`/`XmlMerger`/`Relay` 驱动环）+ `vgige-back-gige`（GVCP/GVSP 后端）+ `vgige-front-gige`（GvcpServer 前端 + GVSP 3.0 会话）+ `vgige-cli`（bin/netid/signal/e2e）；根 members 替换，旧目录删除
- **零行为变化**：143 测试语义原样（唯一测试修正：关流静默断言改"先冲刷在途包再监听"，消除既有竞态）；接缝克制（trait 只到能插 U3V 为止）
- **验证**：Windows `cargo test --workspace` 143 全绿 0 警告；aarch64 板四 crate 测试复核（SSH 配方，结果回填台账 §6.4）

### GW-1.6. Step 2 back-u3v ✅ 代码完成 2026-10-26（真机待相机）
- **内容**：gentl-u3v 加 **pub client facade**（`U3vCamera`/`U3vStream`，库形态复用 control/packets/stream/usbio 全部协议件，零复制；`from_pipes` 测试缝）；`vgige-back-u3v` 实现 `CameraBack`（GenCP→GEV 状态码映射表，公共码 0x8001-0x8007 同命名空间透传）+ `FrameSource`（SIRM 开流 + 块拉取 → CanonicalFrame，CR-86st/R-127sr 语义与 GenTL producer 一致）；`CameraBack::fetch_identity` 接缝（U3V=ABRM 快照零线上读取）；fake GenCP 设备 e2e 4 项（透传/错误码/XML 注入/U3V→3.0 流中继）；CLI `--back u3v`（编译特性二选一，双 producer 同图链接符号冲突）
- **验证**：Windows 全 workspace 155 全绿 0 警告（gentl-u3v 28 零回归）；aarch64 板 45 项全绿（含 u3v e2e）；**待 U3V 相机真机验收**（Q38）

### GW-1.7. Step 3 front-u3v ✅ 代码完成 2026-10-26（整机集成待验）
- **结构**：`vgige-front-u3v` 两层分离——`device/`（设备侧协议状态机：ABRM/SBRM/SIRM 寄存器文件 + GenCP 命令服务 + §5.5 块发射器 + §3.2 描述符构造，纯内存可测）+ `gadget.rs`（Linux FunctionFS 物理层，configfs/ep0/端点桥接）。gentl-u3v 下沉设备侧编解码（packets 模块转 pub）
- **接缝**：`U3vStreamSession`（FrameSession）+ `Relay::open()`（Stream Enable 触发的编程式开关）；寄存器语义与 back 对称（bootstrap/厂商区转发 CameraBack、定制窗口本地、manifest 暴露注入 XML）
- **测试（无硬件验收）**：**配对测试=王冠**（device/ 内存字节流 ↔ gentl-u3v 自家 U3vCamera：bootstrap/能力门控/XML 注入/SIRM 配置/Mono8 逐字节）；矩阵格 e2e 4 项（Stream Enable 级联开闭下游）；desc 字节级 4 项（EF/02/01 身份三元组 + Device Info 经 gentl-u3v 解析器往返）；Windows 全绿 0 警告、板端全绿
- **待验**：Orange Pi dwc3 gadget bring-up（libcomposite/configfs/Type-C），Q39 记录 MPS/GUID/VID 等 QoI 裁定；矩阵 "GigE相机→U3V设备" 格因双 producer 符号冲突编译期拒绝（Q38-7），v1 由相邻格覆盖拼合

### GW-1.8. back-null 合成模式 ✅ 2026-10-26（gadget 演示解锁）
- **内容**：`vgige/vgige-back-null`——无相机时网关照跑：`NullBack`（读 0[XML 三寄存器按流几何播种]/写 no-op/内建最小 GenApi 合法 XML[LittleEndian，SmartCamera 由注入器补]）+ `NullFrameSource`（30fps 1280×1024 Mono8，`(k+block) mod 256` 可校验图案）；CLI `back-null` 特性 + `--back null`（`--down` 可省）
- **矩阵**：新增两格——back-null×front-gige、back-null×front-u3v（**后者=gadget 整机演示形态**：`--no-default-features --features back-null,front-u3v --back null --front u3v --u3v-payload 1310720`）；back-gige×front-u3v 编译期拒绝不变
- **测试**：NullBack 4 项（寄存器语义/身份/XML、帧尺寸+图案逐字节[blocks 1/2]、next_frame 契约[None⇒等满窗口，调度容忍写法]、close no-op）；Windows workspace 174 全绿 0 警告、板端 vgige 33 全绿；READY 冒烟 `down=null xml=6289B nodes=5 stream=on`
- **板端**：UDC `fc000000.usb` 已出现，gadget 演示前提就绪（整机联调由用户执行）

### GW-2. v2 候选（讨论后定）
| 项 | 说明 |
|---|---|
| 双 producer 符号去冲突 | 把 gentl-gige/gentl-u3v 的 GenTL 导出面移到独立 wrapper crate（或 unions），解锁 "GigE 相机 → U3V 设备" 单体链接格 |
| 整机 bring-up | front-u3v gadget 在 Orange Pi 上跑通 + PC 侧 pylon/MVS 抓流（Q39 清单）；back-u3v 真机验收（Q38） |
| Step 2/3 真机闭环 | U3V 相机 ↔ 网关 ↔ U3V 客户端全链路；GigE 相机 ↔ 网关 ↔ U3V 客户端 |
| 多客户端抢占 | DeviceHandler peer 上下文（M3-7 同款阻塞）+ 虚拟 CCP 真实语义 |
| 多通道扇出 | 虚拟通道 x>0 → 真机通道映射或单通道多播 |
| 对外 PACKETRESEND | 虚拟侧块缓存 + §16.7 应答（重组缓冲即缓存源） |
| SmartCamera 挂 Root | 解析真机 Root Category 补 pFeature（需轻量 XML 结构解析） |
| 业务层 | AI_Result/AI_Score 真实生产者（front-gige 的 RegisterSpace 与 front-u3v 的 RegisterFile 均已留 `set_ai_result/set_ai_score` 钩子） |
| GenApi 消费验证 | consumer-test C++ 经网关连 sim（复用 sim_server 手册模式） |
