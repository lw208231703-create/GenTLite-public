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
