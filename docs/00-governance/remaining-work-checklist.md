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

