# GenTLite

[![CI](https://github.com/lw208231703-create/GenTLite-public/actions/workflows/ci.yml/badge.svg)](https://github.com/lw208231703-create/GenTLite-public/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-yellow.svg)](LICENSE)

[English](README.md) | **简体中文**

通用 GenTL producer（`.cti`），面向 GigE Vision 与 USB3 Vision 工业相机。
目标是以统一 CTI 驱动合规相机完成参数调节与图像采集，主机端无需厂商 SDK。

Rust 实现，C ABI，MIT 许可。

## 能力

- GigE Vision（GVCP/GVSP，规范 3.0 / 1.2）与 USB3 Vision（GenCP
  1.3.1，规范 1.2）传输层实现
- GenTL 1.6 C ABI——64 个导出函数；GVSP 零拷贝采集，U3V 分段重组采集
- 全部 273 个 PFNC 像素格式；相机参数经相机自带 GenICam XML 调节；
  Chunk、Buffer Parts 等可选能力按需实现

## 快速开始

```bash
cargo cti    # 编译整个 workspace，并在 target/release/ 的 DLL 旁
             # 生成 gentl_gige.cti / gentl_u3v.cti
             # （cargo ctid 出 debug 版；直接 cargo build --release
             #  也可以，只是不会生成 .cti 副本）
```

把构建目录指给 GenTL consumer（加入 `GENICAM_GENTL64_PATH` 或显式加载
`.cti`）。`consumer-test/` 提供 C++ 参考实现；
`cargo run --example grab_u3v -- <id> 100` 可直接收图，
`cargo test --workspace` 无硬件回归。

## 状态

- USB3 Vision：海康 U3V 相机已验证；Windows 使用前需用 Zadig 切换为 WinUSB。
- GigE Vision：自研相机已完成 1000/1000 帧验证，Linux/aarch64 已完成
  380,000 帧、2 小时 10 分长稳测试。
- `vgige` 网关：GigE 中继与 USB 2.0 High-Speed U3V gadget 已有长稳报告。
- pylon / MVS / HALCON 等第三方宿主及多品牌相机矩阵仍待验证，详见
  `docs/interop-matrix.md`。

## 许可

MIT —— 见 [LICENSE](LICENSE)。`include/GenTL.h` / `include/PFNC.h`
为 EMVA 版权头文件（BSD 风格，版权头保留）。

*GenTL、GenICam、GigE Vision、USB3 Vision 为各自所有方的商标。本项目
基于公开规范的独立实现，与 EMVA、AIA/A3 无隶属或背书关系。*
