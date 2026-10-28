# 项目宗旨与范围裁决（Project Charter）

## 宗旨（一句话）

**一个自研 GenTL CTI，驱动不同设备。** 网关（智能相机）是与 CTI 同构的
第二条产线：它把"任意后端相机"包装成"标准设备"，由同一个 CTI 驱动——
两条产线共享同一套协议实现，不是两个项目。

## 派生原则

1. **协议主动权在用户态**：功能实现不改内核源码；内核侧只接受硬件配置
   类改动（设备树 overlay）。树外模块（.ko）是"记录过的备选"，不主动做。
2. **一切以规范文本为准**：实现引用条款号（§/R/CR），规范沉默处记录
   裁定（OPEN-QUESTIONS），禁止猜。
3. **兼容性正解 = 宿主加载我们的 CTI**（GenTL 标准支持多 producer），
   而不是让设备去兼容所有第三方 producer 的发现逻辑。

## 范围裁决记录

### A1（Device Info CS 描述符）——关闭，won't fix by design（2026-10-28）

- 背景：内核 f_fs 白名单拒绝 U3V 的 CS 描述符 → 内核组合 config 无
  Device Info → 第三方 producer（Aravis 实测）无法按 GUID 打开虚拟相机。
- 裁决：项目契约是"我们的 CTI 驱动设备"，第三方 producer 原生发现
  虚拟相机属范围外。CTI 侧标准路径（读 CS）早已实现，虚拟相机走
  EF/05 兜底，闭环完整。
- 保留项：configfs 设备类 EF/02/01（U3V Table 3-1 规范性要求，已修复）。
- 归档备选：f_u3v.ko 树外模块（客户点名要求原生发现时再评估）。
- 证据：notes/u3v-gadget-bringup-pitfalls.md "Aravis 互通测试结论"（发现关
  过、打开关精确死于 GUID/CS）。
