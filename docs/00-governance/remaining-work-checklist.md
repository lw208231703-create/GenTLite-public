# 剩余工作总清单（2026-10-30 更新）

目标锚点：**高兼容性 + 协议规范标准性**。本清单区分“代码已完成”“无真机已验证”和“仍需真实相机/第三方环境”，避免把模拟器结果当成真机结果。

## A. 网关（vgige，板子中转）

| # | 事项 | 状态 | 验证/边界 |
|---|---|---|---|
| A1 | U3V Device Info CS 描述符 | 关闭 | 按项目宗旨不做；CTI 驱动设备契约已满足，第三方 producer 原生发现属范围外 |
| A2 | front-u3v EIRM 与 GenTL 设备事件 | ✅ 完成 | 注册、DEI 投递、事件数据、flush、kill、注销全链路脚本通过 |
| A3 | vgige-cli systemd 自启单元 | ✅ 完成 | 含陈旧 gadget 状态清理，板端异常重启恢复已验证 |
| A4 | 双 producer 同链与 back×front 组合 | ✅ 无真机完成 | `cti-exports` feature 隔离符号；`back-gige + front-u3v` 板端运行并经 Windows 收到 50/50 帧 |
| A5 | LDVision 打开虚拟相机并采图 | ✅ 完成 | 显式加载我方 CTI；ROI/曝光/帧率写读及 100/100 帧通过 |
| A6 | back-u3v × front-u3v | 待真机 | 需要真实 U3V 相机接板子 USB 口 |
| A7 | back-gige × front-u3v | 🟡 模拟器通过 | 真实 GigE 相机、第二网口和产线网络仍待验证 |
| A8 | 异常注入与恢复 | 🟡 虚拟链路通过 | 连续启停、宿主强杀、板端服务重启、Type-C 恢复通过；真实相机断连和网络抖动待测 |

## B. U3V gadget 前端与协议规范

| # | 事项 | 状态 | 验证/边界 |
|---|---|---|---|
| B1 | Type-C SuperSpeed 训练 | ✅ 完成 | normal/reverse 均通过；方向变化由 DWC3 重绑脚本和 udev 自动恢复 |
| B2 | SuperSpeed 分段与 15 分钟长稳重验 | ✅ 完成 | 120 fps 设置：108000/108000 帧，911.173 秒，平均 118.529 fps；结束后仍为 SuperSpeed |
| B3 | GenApi 严格校验 | ✅ 完成 | 官方 GenApi consumer 通过，52 节点和 ExposureTimeAbs 写读正常 |
| B4 | pylon/MVS 原生发现 | 范围外/已记录失败 | 两者均未发现设备；不能用我方 CTI 通过代替第三方 producer 通过 |
| B5 | Aravis 自动化互通回归 | ✅ 完成 | 脚本严格检查完成帧；GigE 默认/ROI 两组均 150/150 帧，自环需 `ARV_NO_PACKET_SOCKET=1` |
| B6 | FS/HS/SS 枚举与收流 | 🟡 HS/SS 通过 | FS 和更多 USB 主控/线材组合待测 |
| B7 | Windows 选择性挂起与更长长跑 | 进行中 | Aravis GigE 60 fps 与 Windows U3V 120 fps 两小时均在运行；8 小时和睡眠唤醒专项待后续执行 |

规范审计：`compliance/spec-audit/u3v-audit.md` 已覆盖 §3.2、§4.1–4.3、§5；`gige-audit.md` 已记录 RoCEv2、多 tap、断线恢复及可选特性边界。

## C. 通用 GenTL producer（Windows/Linux CTI）

| # | 事项 | 状态 | 验证/边界 |
|---|---|---|---|
| C1 | gentl-gige GVSP resend/ARQ | ✅ 已有 | `GENTLITE_GEV_RESEND` 开关及门控 |
| C2 | gentl-gige 多通道接收 | ✅ 已有 | `GENTLITE_GEV_STREAM_CHANNELS`，默认 8 通道 |
| C3 | U3V 事件接收 | ✅ 完成 | DEI 事件泵和 `EVENT_REMOTE_DEVICE` 已接线并实测 |
| C4 | Windows U-A 异常路径收尾 | 待明确 | 保留 docs/00-governance/PENDING.md 旧项，需先定义具体异常场景 |
| C5 | pylon/MVS/Halcon + 我方 CTI + 真实相机 | 待真机/授权 | 分宿主记录发现、打开、GenApi 和采图结果 |
| C6 | 真实相机矩阵回归 | 待真机 | 多品牌 GigE/U3V、不同帧格式和传输参数 |
| C7 | CTI 被第三方 GenTL 宿主加载 | ✅ 无真机完成 | LDVision 显式加载、打开、参数和采图通过；真机宿主矩阵随 C5 |

## D. 兼容性/规范工程

- ✅ D1 互通矩阵已填入 GenApi、LDVision、Aravis、pylon、MVS 实测结果与边界（`docs/30-validation/interop-matrix.md`）。
- D2 f_fs CS 白名单上游 issue/补丁查询：随 A1 关闭，不再作为主线工作。
- ✅ D3 板端部署、重启、帧率设置、虚拟 GigE 压测和远程事件脚本已就位。
- 未提交修改保留在工作区；不得把“已验证”自动等同于“已 commit/push”。

## E. AI 晶圆检测业务线

- E1 采芯片好样本（用户侧）。
- E2 PatchCore/EfficientAD 训练与评估。
- E3 接入 AI_Result/AI_Score 钩子；网关注册器和 XML 已就位，模型推理进程未接入。

## F. 真实相机到位后的优先验收

1. 真实 U3V：back-u3v 直读、back-u3v × front-u3v、ROI/曝光/帧率、事件和异常恢复。
2. 真实 GigE：back-gige 直读、back-gige × front-u3v、丢包重传、断连/网络抖动和多通道。
3. 多品牌矩阵：不同 PixelFormat、可变帧、短包、非默认包大小及多 tap。
4. 第三方宿主：我方 CTI 与第三方 producer 分开验收；pylon/MVS 原生发现失败仍是已知边界。
5. 将模拟器结果与真机结果分开记录，不用“模拟链路通过”替代真机验收。

## G. 2026-11 规范补齐批次（已完成）

以 `notes/` 规范书 + NI MAX 实际行为为基准的一轮系统性修补（详见
`compliance/spec-audit/2026-11-gap-closure.md`，含逐项条款/文件/验收）：

- **GenTL producer**：GCGetInfo 真实实现、全入口点 panic 守卫、chunk 全链路（解析器 + GigE §24.4/§25.1 与 U3V §5.5.5.2 双编解码 + `DSGetBufferChunkData`）、GigE `WRITEMEM`（§16.6，R-185cd）与对齐状态码修正。
- **模拟相机**：控制通道心跳看门狗（§10.5 R-075cd/R-076cd）、FORCEIP 完整设备语义（§16.2）、扩展 chunk 发射（§25.1）、虚拟 null 相机 TestPattern/UserSetControl 真实语义（SFNC）。
- **网关**：下行相机心跳 worker（含 EA 丢失自动恢复）、back-gige 写路径（4B→WRITEREG、多字→WRITEMEM）、前端能力/身份本地虚拟化（持久 IP 不再被转发/破坏）、FORCEIP 安全边界（OPEN-QUESTIONS Q40）、chunk 中继（GigE 前端重发扩展 chunk、U3V 前端剔除）、XML 内容哈希 URL 防宿主陈旧缓存。
- **验收**：`cargo test --workspace` 260/260；本地 NI MAX：Attributes **167**（+5 新 SFNC 节点）、Compatibility Report **96/96 Pass、0 Fail**（~30 fps）。
- **新增工具**：`scripts/validate_genicam_xml.py`（用 NI 自带 GenApi XSD 校验 XML，支持 `--fetch` 从设备拉取；在 NI 验收前必跑）。

仍待外部条件：板端重部署 + 板端 NI MAX 复测；真实相机矩阵（back-u3v、back-gige×front-u3v）；`gentl/simulator` 的 TestPattern/UserSet 对齐（可选）；U3V 虚拟前端 chunk 发射（可选）。

## H. 2026-11-23 板端部署与验收（已完成）

- 源码已同步到板子（`/root/GenTLite-public`，旧版本备份 `/root/GenTLite-public.prev-20261123`），aarch64 原生构建通过；`vgige-gige.service` / `vgige.service` 均 active 且加载新 XML（27992B）。
- 板端 NI MAX（Windows 宿主）验收：ENUM 发现板端虚拟 GigE 相机；**Attributes 169 项**；**Compatibility Report 96/96 Pass、0 FAIL**。
- 板端自动化测试（aarch64）：**247 passed / 0 failed**（gentl-core/gvcp/simulator/gige/u3v + vgige-core/back-null/front-gige/front-u3v）。
- 2026-11-23 追加：网关虚拟 GigE 前端实现 **GVSP PACKETRESEND**（§16.7 缓存重传 + 能力位广播），重新部署后板端 `/TEST_NETWORK` 由 91 Pass/5 FAIL 变为 **96 Pass / 0 FAIL**。
- 仍待外部条件：真实 GigE/U3V 相机矩阵、U3V 虚拟相机的 NI 驱动切换（可选）、网关 GVSP resend 缓存（可选增强）。

## I. 2026-11-23 收尾补充（第 20–22 轮）

- **设备事件中继**：网关新增 `MessageRelay` seam；GigE 后端（§12 消息通道/§17 EVENT，原始数据报转发）与 U3V 后端（§4.1.3.3/§5.4.2 DEI 事件 → §17.1.1/§17.2.1 数据报）均已接通；CLI 对任何支持事件的 backend 自动启用虚拟消息通道，无事件后端保持 0 通道（§36.22 诚实声明）。验证：`relay_e2e::device_events_are_relayed_to_the_opened_message_channel` + back-u3v 事件转换单测。
- **板端 30 分钟长跑**：54000/54000 帧、29.999 fps、0 丢帧、RSS 稳定、服务无重启（第 22 轮）。
- **本地 NI 复测（最新构建）**：Attributes 169、Compatibility Report 96/96 Pass、0 FAIL。
- **U3V 板端矩阵**：追加第三方宿主证据 —— Basler pylon 枚举 0 个 U3V 设备，与 NI 不列出、我方 WinUSB `WinUsb_WritePipe: 6` 一致；结论仍是"需要驱动重绑（需授权）/ 干净 Windows 主机 / 真实 U3V 相机"三选一。
- 验收材料索引：`compliance/README.md`（含复跑命令）；主报告 `compliance/spec-audit/2026-11-gap-closure.md` 第 13 节为最终验收汇总。
- 事件通道逐事件重传（MCTT/MCRC）v1 取舍见 `OPEN-QUESTIONS.md` Q41。

## J. 2026-11-23 U3V 驱动重绑授权后的排查（第 25 轮）

- 结论：**不需要驱动重绑**——板端 U3V gadget 的 MI_00 已是 libwdi WinUSB 绑定，问题在我们自己的代码。
- 修复 3 个缺陷：①`gentl-u3v` facade 打开后丢弃 `InterfaceSet` 导致 WinUSB 句柄失效（WritePipe 6 / AV）；②Windows stream 管道误用 cancel-on-timeout（keep_pending 应为 true，解释板端 EPIPE/AIO timeout）；③网关 XmlMerger 对已含 SmartCamera 的下游 XML 重复注入（NI XML 加载失败）。均有单测/回归（270 passed / 0 failed）。
- 修复后：网关 U3V→GigE 正常启动、XML 通过 GenApi XSD、NI ENUM 出现设备且读到 169 属性。
- 仍待解决：Windows 从 stream 端点读到 0 长度 transfer（ZLP；AIO/同步同表现），需板端 `send()` 日志 + USB2/SS 描述符对照继续定位。

## K. 2026-11-23 链路/USB 速率排查（第 26 轮）

- 以太网 1Gbps 根因：Windows 网卡高级属性 `*SpeedDuplex` 被固定为 1.0Gbps（板端 r8169/RTL8226B PHY 通告 1000/2500）。需管理员执行 `Set-NetAdapterAdvancedProperty -Name '以太网' -RegistryKeyword '*SpeedDuplex' -DisplayValue '自动侦测'`，预期协商 2.5Gbps（5G 超出板端 PHY）。
- USB 已确认 SuperSpeed：板端 `/sys/class/udc/fc000000.usb/current_speed = super-speed`，非 USB2 回退。
- U3V 遗留：新增板端 `VGIGE_U3V_SEND_LOG` / `VGIGE_U3V_MAXBURST` 诊断开关；直连测试期间 stream 端点无发送、主机读 0 长度完成，问题仍在定位（与 USB 速率无关）。

## L. 2026-11-23 2.5G 复核 + U3V 端点定位（第 27 轮）

- 2.5G 已协商成功（Windows LinkSpeed=2.5Gbps，板端 2500Mb/s），实测 TCP **279 MB/s ≈ 2.24 Gbps**。
- U3V 端点级证据：pyusb 确认 USB 3.2/3 接口/端点齐全；板端 emitter 已提交 leader(52)+payload(512KiB)；主机 WinUSB 在 **event(0x82)/stream(0x83) 关联接口** 的 bulk IN 读取均**立即返回 0 字节**，control(0x81) 正常；RAW/AIO 因素已排除。
- 结论：问题收敛到 **WinUSB 关联接口 bulk IN 读取**（或 gadget 关联接口 I/O 前置条件）；下一步对照 pylon/NI 驱动行为并核查 `WinUsb_SetCurrentAlternateSetting`/每接口独立 WinUsb_Initialize。

## M. 2026-11-23 U3V 全链路打通（最终）

- 根因与修复：OVERLAPPED 结构体被移动导致完成长度读 0（改 Box）；网关块截止时间误用轮询窗口（改 max(1s)）；`U3vStream::stop()` 未取消遗留读（补 cancel_pending_read）；残留数据导致 leader 失步（drain 后再取消）；失步后整会话 dead（改可恢复 reopen，自动重读 SIRM 适配新几何）；resend 缓存扩到 8 块/256 MiB。
- 结果：**NI CameraValidator 属性矩阵 96/96 Pass、0 FAIL**（169 属性）；直接 GenTL U3V producer 50/50 帧；网关 U3V→GigE 20/20 帧；270 tests / 0 failed / 0 warning。
- 板端已重新部署干净构建并重启服务（备份 prev5）。
- 以太网 2.5 Gbps + USB SuperSpeed 均已确认（实测 TCP 279 MB/s）。

## N. 2026-11-23 外部 review 复核（GenTL 事件/chunk、PENDING_ACK、网关块访问、LLA）

- 复核结论：review 的 6 组问题中 5 组为真缺陷（另补充发现 GigE EVENT_DATA_VALUE 缺 24 字节标准头），1 组（GenCP 临时超时）为兼容性缺口而非硬性 MUST；EventGetInfo 子结论不成立（已撤回）。
- 修复：genlt-core 增加 ParsingChunkData + event_id_hex；genlt-gige/u3v 修正 EVENT_DATA_ID/VALUE、EventFlush/Kill、chunk 错误码；gvcp 与 u3v/control 采用 PENDING_ACK time_to_completion；gateway 支持多 word 虚拟寄存器块访问；simulator 强制 LLA 常开。
- 验证：275 tests / 0 failed / 0 warning；提交 ed1292d、9d5a6d5、00e7984、8bdf600、ddf4452。

## O. 2026-11-24 第二轮外部 review 复核

- 结论：4 个新问题 + 3 个历史问题中 6 组为真缺陷/改进点；第 5 组的"模拟器同样违反"判断不准确（模拟器已实现 R-586cd/R-588cd/R-562st），网关侧确实缺失。
- 修复：不完整缓冲区 DATA_SIZE；FORCEIP 关闭消息通道（模拟器+网关）；GVCP 服务端空 READREG/未对齐 READMEM 校验；XML 单引号/属性顺序；网关流寄存器严格状态与 SCPS 取整；U3V SIRM RO 权限与开流快照；U3V Current Speed 实测与 FunctionFS 字符串 ID 偏移。
- 验证：285 tests / 0 failed / 0 warnings；板端 USB 描述符读回 index4/5/6 = GUID/Function/DeviceVersion，Current Speed 实测 super-speed；本机 U3V producer 枚举并打开成功。
- 待办：宿主机 NI-IMAQ 驱动异常（niimaqk.sys 缺失、NIPALK 加载失败），需重启/修复 NI 安装后重跑 NI 矩阵（本机环境问题，已在 compliance §23 记录）。
- 提交：5519b08、8232d88、ddb7993、422985d、434a1cf。

## P. 2026-11-24 第三轮 review 复核（虚拟 U3V 严格性）

- 两项均为真缺陷：全零传输几何被接受（应为 U3V_STATUS_SI_REGISTERS_INCONSISTENT）、SIRM 快照未覆盖全部 RW* 字段。
- 修复：Stream Enable 前置校验（required leader/trailer、payload capacity、Standard 模式、对齐）且失败不锁存；快照扩展为 7 个 RW* 字段；emitter fail-closed。
- 验证：286 tests / 0 failed / 0 warnings；板端 GenTL U3V producer 20/20 帧（2,621,440 B/帧）。

## Q. 2026-11-24 第四轮 review 复核（SIRM 部分字节写）

- 真回归：SI_CONTROL 的 1~3 字节写会 panic（GenCP 允许字节粒度 WRITEMEM）。
- 修复：SIRM 逐字节权限；控制字用当前寄存器 + 本次字节合并后校验，不再越界切片；失败返回 GenCP 状态而非 panic。
- 验证：288 tests / 0 failed / 0 warnings；板端 USB 部分字节写 ACK 正确、服务存活、streaming 20/20 帧。

## R. 2026-11-24 第五轮 review 复核（UserSet/事件注册/ROI/FORCEIP）

- 真缺陷：UserSetLoad Default 无效果、DeviceReset 不读 UserSetDefault、UserSetSave 非非易失、ROI 可超出 4096×4096；均已修复并测试（UserSet 持久化文件；ROI 双向钳制）。
- 事件注册：review 所述"并发重复成功"不成立（检查+插入同一锁），但补了 register/unregister/close lifecycle 互斥；8 线程并发测试 = 1 成功/7 RESOURCE_IN_USE。
- FORCEIP 非零地址：确认是已知强制条款缺口（Q40），在 compliance §26.1 明确列为"不支持"，不再宣称 relay front 全条款通过。
- 验证：291 tests / 0 failed / 0 warnings；板端 GVCP UserSet/ROI 实测 + 服务重启持久化实测。

## S. 2026-11-24 第六轮 review 复核（GVCP/U3V/GenTL）

- 真缺陷 10 组已修：WRITEMEM_ACK 语义（双向）、GVCP 主/次权限与 EA↔CA 切换、R-072cd 控制通道释放清理、U3V 高位地址溢出、模拟器 1x 心跳、READREG 部分成功、U3V CommandResend、GCCloseLib/初始化门禁、stacked 查询语义、Current Speed 错误兜底；README 能力表述同步修正。
- FORCEIP 非零地址仍是已记录的强制缺口（§26.1/Q40），不实现宿主 NIC 重绑定。
- 验证：299 tests / 0 failed / 0 warnings；板端 GVCP ACK/权限/清理与 U3V 边界实测通过。

## T. 2026-11-24 第七轮 review 复核（三个未闭合问题）

- ACCESS_DENIED 的 WRITEREG/WRITEMEM ACK 改为 header-only（R-138cd）；WRITEMEM 部分失败通过 progress 接口返回真实字节索引（R-194cd，模拟器/网关）；GCCloseLib 使全部旧 GenTL 句柄失效（handle generation），重新初始化后旧 TL 不可用。
- 验证：303 tests / 0 failed / 0 warnings；板端 GVCP 与 CTI 实测通过。FORCEIP 仍按 §26.1 记录为不支持。

## U. 2026-11-24 第八轮 review 复核（GCCloseLib 资源释放）

- 真 bug：GCCloseLib 只使句柄失效，未释放 HandleBox/模块/线程。已改为 token 句柄 + registry，GCCloseLib drain 全部句柄并调用各模块 shutdown（停止接收线程/心跳/事件泵，关闭相机 SCP/CCP），再释放 Arc/条目。
- 验证：305 tests / 0 failed / 0 warnings；GigE 集成测试验证线程归零与相机侧 SCP0/CCP0=0；CTI 验证旧句柄 -1006、新句柄正常。

## V. 2026-11-25 Markdown 规范全工程复核（GVSP/GVCP/GenCP/U3V/GenTL/SFNC）

- 依据新转换的 `notes/markdown/*.md` 重新交叉核对；violations 与证据见 `compliance/spec-audit/2026-11-md-spec-review.md`（本机审计记录，compliance/ 未纳入版本库）。
- 已修 wire 互通级：GVSP 3.0 leader body 36/u16 padding、GVSP flag 位序、MCP host_port 低 16 位、事件消息 EI/req_id、READREG ACCESS_DENIED header-only、FORCEIP broadcast 位；GenTL IS_COMPOSITE/IS_QUEUED；U3V block_id 0、request_id=0 写拒绝、DeviceInfo 长度扩展、SIZE_FILLED valid_payload_size。
- 待修高优先级：GenTL Close 级联/EventHandle 失效、可变帧高、U3V 短包/ZLP、模拟器几何与 XML 一致性、U3V front AcquisitionMode 窗口遮蔽；中低优先级清单见审计文件 §3。
- 验证：307 tests / 0 failed / 0 warnings；板端重新部署后 U3V 端到端 20/20 帧（2,621,440 B/帧），GigE 原始取流首包 56 字节（20+36 leader、payload_type=1、1280×1024、padding 0）。

## W. 2026-11-25 终审建议复核（级联关闭/零通道/FORCEIP 过期/range 循环/文档）

- 已修：TLClose/IFClose/DevClose/DSClose 级联关闭子树（parent-aware handle registry + close_handle_tree）；零流通道诚实上报（GigE/U3V）；FORCEIP 先过期心跳再判权限（gateway + simulator）；RESEND_RANGE_ERROR 按实际缺口裁剪并限幅；`gvcp-wirefacts.md` GVSP flag 数值同步为 0x0004/0x0002/0x0001。
- 未变：relay 非零地址 FORCEIP 仍为 Q40/§26.1 记录的强制缺口。
- 验证：313 tests / 0 failed / 0 warnings（parent_close 集成测试覆盖两代 Close 级联与零通道）。
- 板端复验：重新部署后 U3V GenTL 取流 20/20 帧（2,621,440 B/帧），GigE 原始取流首包 56 字节（20+36 leader、payload_type=1、1280×1024、padding 0），`vgige`/`vgige-gige` 均 active。

## X. 2026-11-25 终审 12 项 + FORCEIP 修复

- 已修：U3V 短包/ZLP 终止（read_piece_exact 返回 terminated，后续 transfer 丢弃）；GigE 可变帧高（trailer size_y <= leader 可交付，FrameMeta.delivered_height，BUFFER_INFO_DELIVERED_IMAGEHEIGHT 独立于 HEIGHT）；U3V front 本地 ACQ 窗口只保留 Start/Stop/PayloadSize，AcquisitionMode/TriggerMode 转发后端；simulator 开流时从用户寄存器取 Width/Height/PixelFormat；GCUnregisterEvent 后 Event* 返回 INVALID_HANDLE；独立心跳 watchdog（GvcpServer tick，simulator/gateway）；Pending Timeout 0x0958 寄存器（simulator/gateway）；U3V READMEM ack 按 Maximum Acknowledge Transfer Length 截断；网关测试包使用 SCPSx 取整值与 DF；U3V EIRM+0x04 只读、null WidthMax 等 RO 写入 WRITE_PROTECT；GenTL EVENT_NUM_IN_QUEUE / NUM_CHUNKS_MAX 语义；U3V Event IN 按 EIRM Maximum Event Transfer Length 取缓冲、控制 IN ack MPS 整数倍补 ZLP；FORCEIP 非零地址在虚拟设备上生效并 ACK（Q40 更新为协议层生效、宿主 NIC 重绑定仍属平台操作）。
- 验证：322 tests / 0 failed / 0 warnings。板端重新部署后：U3V GenTL 取流 20/20 帧（2,621,440 B/帧）；GigE 首包 56 字节（20+36 leader）；Pending Timeout(0x0958)=1000；心跳 500 ms 空闲 800 ms 后无命令，流在积压数据排空后停止（0.8 s 静默），随后 SCP0/CCP 均为 0。

## Y. 2026-11-25 后续 6 项 review 修复

- FORCEIP：只对“本机已生效地址”ACK；不可达地址保持静默，避免假 ACK（Q40 v3）。
- 可变帧：trailer 行数对应的载荷字节数严格校验（1 字节不能冒充 2 行）；扩展 chunk 路径也接受 size_y < leader 并单独上报 delivered_height。
- EVENT_NEW_BUFFER：EventGetData/GetInfo/Flush/Kill 在注销后返回 INVALID_HANDLE（pending waiter 仍 ABORT）。
- Pending Timeout：改为只读（write → WRITE_PROTECT）；新增设备侧 PENDING_ACK 生成路径（gateway 在下游流打开前发送）。
- U3V：任何非 MPS 短包（即使恰好补足 planned size）都标记 terminated，后续 transfer 丢弃。
- U3V front：ACQ+0x08 PayloadSize 写入返回 WRITE_PROTECT，不再静默写本地缓存。
- 验证：325 tests / 0 failed / 0 warnings。板端重新部署后：U3V 20/20 帧、GigE 56-byte leader；PendingTimeout 读 1000、写 0x8004；SCP0 打开时首包为 PENDING_ACK(0x0089, len4) 后接 WRITEREG_ACK(0x0083)；FORCEIP 不可达地址保持静默。

## Z. 2026-11-25 PENDING_ACK 心跳/多寄存器/EventGetInfo 修复

- R-212cd：GvcpServer 在 PENDING_ACK 与最终 ACK 之间调用 `pending_begin/pending_end`；gateway CcpState 增加 suspended，挂起期间不做过期清理，最终 ACK 后重置 last_contact；新增 Ccp 单测。
- 多寄存器 WRITEREG：gateway 的 pending 判断扫描全部 8 字节地址/值对，SCP0 在第二对也会先发 PENDING_ACK；新增 raw e2e。
- EVENT_NEW_BUFFER 的 EventGetInfo 补上 queue.is_dead 检查，注销后返回 INVALID_HANDLE；stream_sim 增加断言。
- 验证：327 tests / 0 failed / 0 warnings。板端重新部署后：U3V 20/20 帧、GigE 56-byte leader、PENDING_ACK(0x0089, len4) 后接 WRITEREG_ACK(0x0083)；慢开流心跳暂停由 Ccp 单测覆盖（板端下游 open 为本地空后端，无法自然制造 700 ms 延迟）。

## AA. 2026-11-25 R-211cd：PENDING_ACK 期间响应 DISCOVERY

- `GvcpServer` 将触发 PENDING_ACK 的 WRITEREG 放入 worker 线程执行；主接收循环在 pending 期间继续服务 DISCOVERY（其他命令暂不并发处理），最终 ACK 由 worker 发送并调用 pending_end。
- 服务器共享 `Arc<H>`（DeviceHandler 增加 Sync），WRITEREG 处理逻辑抽成 `process_writereg` 供同步/异步路径复用；socket clone/spawn 失败时回退到同步执行并照常发送最终 WRITEREG_ACK（PendingGuard 在 ACK 后恢复心跳），不再静默丢弃命令。
- 新增单元测试：写寄存器 handler 睡 300 ms，PENDING_ACK 后 250 ms 内必须收到 DISCOVERY_ACK。
- 验证：328 tests / 0 failed / 0 warnings；板端重新部署后 U3V 20/20 帧、GigE 56-byte leader、SCP0 打开仍为 PENDING_ACK(0x0089)→WRITEREG_ACK(0x0083)。板端 null 后端开流过快，R-211 的 250 ms DISCOVERY 响应由 300 ms 慢 handler 单测覆盖。

## AB. 2026-11-26 文档对账批次（无代码改动）

以本地规范原文 `notes/markdown/gige-vision-3.0.md` 与当前代码为准，一次性修订 C 类
文档漂移（wirefacts / README / 状态与设计记录 / 本地 compliance 材料）：

- **线格式契约** `docs/10-protocol/gvcp-wirefacts.md`：flag ACK 值 0x80→**0x01**
  （§15.1 msb-first：印刷 bit7 即最低位）；DISCOVERY_ACK 载荷 232→**248**
  （Figure 16-2 字段合计）；WRITEREG_ACK 补 **4B SCD**（reserved+index，非
  ACCESS_DENIED 时，R-174c）；WRITEMEM_CMD 删去多余 u32 reserved（Figure 16-11）；
  GVSP 3.0 Image Leader body 36B、padding 各 **u16**（Figure 25-2）；补
  Pending Timeout(0x0958) 寄存器行；文首加对账修订记录。
- **producer README** `gentl/README.md`：不再是"stacked 端口未提供、URLInfo 仅 U3V"；
  改为"64 入口全导出，parts/composite/flows 为 NOT_IMPLEMENTED 桩，
  stacked 端口与 GCGetPortURLInfo 已实现"。
- **根 README**（中英）：zero-copy 表述改为"payload 数据报直接拷贝进 consumer
  预注册缓冲、无中间帧暂存（用户态仅此一次逐包拷贝）"；273 PFNC 说明改为"全部
  格式可算整块 payload，planar 的多 part 呈现需 Buffer Parts（未实现）"；Buffer Parts/Composite/flows 措辞与
  实现对齐（stacked buffer-info、buffer segment 已实现；DSGetNumBufferParts 返回
  NO_DATA）；GigE 1000/1000 与 380,000 帧补"固定 1500B 包长 + 1.2 式 8 字节头"的
  验收条件与 Q35 固件背景。
- **状态/设计记录加"2026-11-26 对账说明"**：`PENDING.md`（文末新增"过时条目索引"，
  覆盖 UQ7/UQ9/chunk/vgige 路径/GW-2 符号冲突等）；`linux-vgige-design.md`
  （§5 的 4/5/11/13/15/17 条与 §8 矩阵行加更新标注，§6.1 路径与 40B leader 更正）；
  `u3v-gadget-bringup-pitfalls.md`（"最终可用配置快照"标注为 2026-10-27 历史基线，
  非当前 vendor 6.1.115 + 512 KiB 部署）；`repository-layout.md`（ABI 归属：64 导出
  在 gige/u3v 的 lib.rs，core 提供支撑件）。
- **裁定账本** `OPEN-QUESTIONS.md`：Q10/Q32-1 标 `[RESOLVED]`；UQ7/UQ9/UQ10 追加
  "后续状态"（DEI 已接线 / 512 KiB 替代 1 MiB / stacked 端口已实现）。
- **其余**：`u3v-wirefacts.md` 的 UQ7 注记更新；`vgige-board-kernel-interop-2026-11-04.md`
  §9.6 与 §3.3 的自相矛盾按 2026-11-11 结论作废；本地 `compliance/`（不提交）加
  时点说明（README）、关闭 gige-audit 的 TL_INFO_PATHNAME 缺口、补 u3v-audit 的
  39/52 节点数口径。
- **附带**：代码注释中残留的 "40-byte/40B leader" 表述一并更正为 36B
  （`gentl/gige/src/gvsp_receiver.rs`、`virtual/gige/src/lib.rs`、
  `gentl/simulator/src/emitter.rs`，纯注释无行为变化）；`compliance/` 的
  `gige-audit.md`/`preflight-gige-u3v.md` 同步注明。
- **本批次未覆盖**：`cargo check --workspace --all-targets` 当前有 **1 个 warning**
  （`gentl/gige/tests/parent_close.rs:6` unused import c_void）——与本文旧段落的
  "0 warning" 记录不符，留待下个代码批次清理。
- 验证：文档与注释改动，`cargo test` 不受影响（328 passed 状态不变）。

## AC. 2026-11-26 代码风险 review 复核与修复

外部 review 复核上一轮"C 类代码风险"清单，逐项裁定为 **7 条确认实现问题 / 2 条已知能力或
架构限制 / 1 条部分成立**。本批次按 P1→P3 修掉确认项与部分成立项的可行动部分：

- **DSStop 跳过传输清理（确认，两个 producer）**：`DSStopAcquisition` 在
  `core.stop_acquisition()` 返回 `RESOURCE_IN_USE`（自动停/接收线程致命 abort 后）
  时曾直接返回，跳过 `ds.stop()` 的 SCPx=0 / SIRM Stream Enable=0。新增
  `gentl_core::ds::stop_sequence`：引擎半场结果不再短路传输半场，错误码照原样
  返回；两个 producer 的入口统一改用它。单测覆盖成功/错误两条路径都执行传输清理。
  （端到端"人为制造接收线程致命"未注入，属测试边界，已记录。）
- **SI_INFO alignment 未防御（确认）**：`SirmInfo::alignment()` 改为
  `Option<u64>`：指数 ≥32（SI Size 寄存器是 u32）或 ≥64（移位溢出）均返回
  `None`，`program_sirm` 以明确错误拒绝（不再 debug panic / release 掩码移位）。
  新增坏值单测（31 通过、32/63/64/255 拒绝）与 `program_sirm` 拒绝单测。
- **Flush 入池未复位缓冲元数据（确认）**：`ALL_TO_INPUT` / `UNQUEUED_TO_INPUT`
  改为经 `Pool::requeue_into_input` 复位——`SIZE_FILLED`/`DATA_SIZE` 按
  GenTL 1.6 §6.4.4.4 强制清零，`NEW_DATA`/INCOMPLETE/metadata 与
  `DSQueueBuffer` 保持一致；新增残留回归单测（此前 review 已实际复现）。
- **GCCloseLib 关闭顺序（确认顺序缺陷）**：`invalidate_handles` 不再按 HashMap
  drain 顺序 shutdown，改为按 parent 链计算深度、子先父后（与
  `close_handle_tree` 同契约），同级以 key 排序保证确定性；新增三级父子
  shutdown 顺序单测。影响上界仍为相机心跳超时（设备侧自清），非线程泄漏。
- **并发打开 TOCTOU（确认，两个 producer）**：`IFOpenDevice` / `DevOpenDataStream`
  在锁外完成打开后，回到锁内**复查**；竞态败者回滚已打开的 device（
  `device.close()` 释放 CCP/心跳）或 stream（drop）并返回 `RESOURCE_IN_USE`。
  `TLOpenInterface` 原本就是锁内 check+insert，未改。
- **GigE 写路径（部分成立）**：整寄存器段改用 **WRITEMEM 批处理**
  （≤512 B/命令，明确避免 IP 分片触发相机控制面失能，Q35 同源；设备回
  `NOT_IMPLEMENTED` 时按 §19 回退逐寄存器 WRITEREG），部分边缘寄存器仍按
  §6.3.6.6 RMW；同时修复 **u32 终点算术**：范围结束于 2^32（最后一个
  32 位寄存器 0xFFFF_FFFC）时，release 曾回绕为 0 而静默丢写，现全程 u64
  计算。新增 4 个 device 单测（WRITEMEM 批、回退、RMW、地址空间顶端）。
- **8 字节块端序（确认，条件触发；第一版未修数据路径）**：拆出
  `swap_register_words`（每 32 位寄存器字反转）与 `swap_register_u64`（单个
  64 位值），但真实块读写路径仍统一调用 `swap_register_endian`（8 字节整体
  反转）——后续 review 指出并复现了该缺口，见 AE 节；本批次仅完成 helper
  与接口语义。
- **back-u3v reopen 无终态（确认）**：连续 reopen 失败达到 5 次后置
  `dead` 终态并只打一条日志（此前无退避、无限重试、每 100 ms 刷日志）；
  成功 reopen 复位计数；新增重试上限单测。
- **未修（已知能力/架构，按 review 定性登记，不进缺陷队列）**：
  虚拟 U3V 前端不转发下游相机事件（需要设备侧 U3V 事件 ID 映射，非协议
  违规）；DsCore 单 in-flight 导致多 tap 并行块互相排斥（需 per-channel
  槽位，属路线图）。**2026-11-26 同日已落地，见 AD 节。**
- **验证**：`cargo test --workspace` = **337 passed / 0 failed**（较上批
  328 净增 9 个回归/单测）；`cargo check --workspace --all-targets` =
  **0 warning**（AB 节记录的 `parent_close.rs` 未使用导入已一并清除）。
  未做真机复验（均为 host 侧状态/边界修复）。

## AD. 2026-11-26 能力/架构两项落地（U3V 事件转发 + per-channel in-flight）

review 中定性为"能力缺口/架构限制"的两项，同日实现并测试：

- **虚拟 U3V 前端转发下游相机事件**（原能力缺口）
  - 新接缝：`vgige_core::device_event::{DeviceEvent, DeviceEventSink}` +
    `CameraBack::start_device_event_relay`（默认 NOT_IMPLEMENTED；一台相机只有
    一条事件通道，另一种事件泵已运行时回 `GEV_STATUS_BUSY` 0x8007）。
  - 后端：`vgige-back-u3v` 把 DEI `EventPacket` 转为 `DeviceEvent`（时间戳已是
    ns，直传）；`vgige-back-gige` 解析 §17 原始数据报（`GvcpEvent::decode`）
    转 `DeviceEvent`，GVCP 的设备 tick 时间戳在读到
    `TIMESTAMP_TICK_FREQ` 时换算为 ns，未知频率报 0（不伪造单位）。
  - 前端：`RegisterFile::send_device_event` 仅在 EIRM Event Enable（EI Control
    bit 0）置位时向事件端点发送 §4.1.3.3 `EVENT_CMD` 包；包长受广告的
    `MAX_EVENT_TRANSFER_LEN=4096` 约束，超大事件丢弃而非截断。
    `U3vDevice` 增加有界事件队列（8）+ 专用写线程 + `push_event`/`event_sink`
    （后端事件线程永不被 USB 写阻塞）；`vgige-cli` 的 `run_u3v_front` 完成接线，
    启动日志区分"转发已启用/后端无事件通道"。
  - 测试：back-u3v 转换单测；back-gige 转换单测（1 MHz→ns、未知频率→0、空
    data）；`regs` 单测（未使能不发、使能后 id/timestamp/data 正确、超大丢弃）；
    `pairing` e2e（真实 `U3vCamera` 事件端点收到 `DeviceEvent`）。
  - 边界：host 侧仍按 GenTL `EVENT_REMOTE_DEVICE` 投递；EIRM 事件缓冲长度仍
    为 4096；MCTT/重传语义未变（Q41）。
- **per-channel in-flight（多 tap 并行块）**（原架构限制）
  - `gentl-core`：`FrameTarget` 增加不透明 `token`；
    `FrameSink::{begin_frame,on_frame,abort_frame}` 以 token 匹配完成/放弃（同一
    DataStream 可同时持有多帧）；`Pool::in_flight` 由单槽改为
    `HashMap<token, buffer>`，删除"单槽已占用→Busy"路径；仍按输入池可用性/
    容量返回 `QueueEmpty`/`TooSmall`/`NotAcquiring`。
  - 适配：`gentl-gige` GVSP 重组器与 `gentl-u3v` 接收器都随块保存 token；多通道
    （多 tap）并发块各自落到自己的 buffer，互不排斥；重复完成同一 token 为安全
    的 no-op。
  - 测试：`ds` 双并发预留 + 各自完成单测（原 Busy 断言改为并发预留断言）；
    gige/u3v 全量集成测试（多通道、resend、可变帧、chunk）通过。
  - 边界：单通道仍只有一个 in-flight 块（由每通道 reassembler 保证）；真机
    multi-tap 相机未复验。
- **验证**：`cargo test --workspace` = **341 passed / 0 failed**（AD +4）；
  `cargo check --workspace --all-targets` = **0 warning**；gateway 特性矩阵
  `back-null/back-gige/back-u3v × front-u3v/front-gige` 四种组合编译通过。


## AE. 2026-11-26 复核跟进：8 字节块端序按寄存器宽度转换

上一轮（AC）只拆了 `swap_register_words` / `swap_register_u64` 两个 helper，
**真实数据路径没有改**：`RegisterSpace::back_read_block`/写路径与
`virtual/u3v` 的 `back_read`/`back_write` 仍对任意 8 字节块整体反转，于是
"两个相邻 32 位寄存器"被当成"一个 64 位整数"。review 给出实测：
同一次 READMEM 读取 Width+Height 返回 `Height,Width`；块写有对称风险
（两个寄存器写到对方地址）。

根因与修复：8 字节块本身没有类型信息，正确字节序取决于**寄存器宽度**，
而唯一可用来源是网关对外提供的 GenICam XML。新增
`gateway/core/src/regwidth.rs`：

- `RegisterWidths::from_xml`：从 XML 提取 `IntReg` / `MaskedIntReg` /
  `Register` 的 `<Length>`，支持直接 `<Address>` 与一级
  `<pAddress>` → `Integer <Value>`（含 `pValue` 链）的间接地址解析；
  只记录非 4 字节宽度（2/8 等，4 字节是兜底假设）；无法解析的地址不猜。
- `swap_block(data, addr, widths)`：从 `addr` 起逐寄存器反转——XML 声明宽度的
  寄存器按整值反转（64 位寄存器仍整体反转），其余按 32 位寄存器；无法对齐的
  尾巴保持原样。
- 数据路径接入：`RegisterSpace` 与 `virtual/u3v` 的 `RegisterFile` 各持有
  服务端 XML 的宽度表，**读、写两条块路径**都改用 `swap_block`；
  `swap_register_endian` 降级为"整值 helper"并在文档中注明数据路径不再使用它。

测试：`regwidth` 6 项（直接/间接/2 字节/8 字节/未知/空 XML）；
`RegisterSpace` 3 项走真实公开 API（review 复现的 Width+Height 8 字节读顺序、
XML 声明的 64 位整值、8 字节写落址）；`virtual/u3v` 1 项同规则反向
（Big→Little 的 8 字节两寄存器顺序 + 64 位整值 + 写路径）。

验证：`cargo test --workspace` = **351 passed / 0 failed**；
`cargo check --workspace --all-targets` = **0 warning**。

边界（仍在，非静默）：深层 `pAddress` 链（指向非常量 SwissKnife/自定义节点）
或 XML 未声明的 64 位地址仍按 4 字节兜底，可能错值——真机出现此类寄存器时
以 XML 声明修正；不涉及转换的同端序组合（GigE→GigE、Little→Little）不受影响。
