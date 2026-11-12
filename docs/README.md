# 文档索引

本目录按用途分为五组。`notes/` 只放外部规范 PDF 和 GenICam 官方参考包，
不纳入 Git；这里放项目自己的工程记录。

## 00-governance — 项目治理与决策

- [Project Charter](00-governance/project-charter.md) — 项目宗旨、范围边界
- [OPEN-QUESTIONS](00-governance/OPEN-QUESTIONS.md) — 规范疑问、工程裁定、QoI 记录
- [PENDING](00-governance/PENDING.md) — 当前状态、未完成项、已知边界
- [剩余工作清单](00-governance/remaining-work-checklist.md) — 按验收项组织的剩余工作

## 10-protocol — 规范符合性与线格式

- [合规状态](10-protocol/compliance-status-2026-11-11.md) — GigE/U3V/GenTL 当前符合性结论
- [GVCP/GVSP wire facts](10-protocol/gvcp-wirefacts.md) — 寄存器、包格式、实测线格式
- [U3V/GenCP wire facts](10-protocol/u3v-wirefacts.md) — U3V 寄存器、事件、流格式

## 20-design — 架构与实现设计

- [仓库结构](20-design/repository-layout.md) — 目录、crate 职责和依赖方向
- [Linux vgige 设计](20-design/linux-vgige-design.md) — 网关与虚拟 GigE/U3V 前后端
- [GigE 100G RX 设计](20-design/gige-100g-rx-design.md) — 高速接收路径设计
- [U3V gadget 踩坑记录](20-design/u3v-gadget-bringup-pitfalls.md) — FunctionFS/gadget 实现细节

## 30-validation — 互操作与验收

- [宿主互通矩阵](30-validation/interop-matrix.md) — NI/MVS/LD/HALCON 等
- [压力测试手册](30-validation/stress-testing.md) — 长跑、重复启停、真实相机验收
- [板端联调记录](30-validation/vgige-board-kernel-interop-2026-11-04.md) — 内核、U3V/GigE、宿主问题

## 40-performance — 性能与带宽

- [vgige 性能基准](40-performance/vgige-performance-2026-11-04.md) — 帧率、CPU、吞吐
- [U3V 带宽定位](40-performance/u3v-bandwidth-2026-11-04.md) — 250 MB/s 上限与 transfer 调优

## 外部参考

`notes/`（本地忽略）包含：

- GigE Vision 3.0 / 1.2、USB3 Vision 1.2
- Camera Link 2.1、Camera Link HS 1.2、CoaXPress
- EMVA 1288 General / Linear 4.0
- `GenICam_Package_2025.10/`（GenApi、GenCP、GenDC、GenTL、SFNC）
