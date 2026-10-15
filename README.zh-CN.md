# GenTLite

[![CI](https://github.com/lw208231703-create/GenTLite-public/actions/workflows/ci.yml/badge.svg)](https://github.com/lw208231703-create/GenTLite-public/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-yellow.svg)](LICENSE)

[English](README.md) | **简体中文**

通用 GenTL producer（`.cti`），面向 GigE Vision 与 USB3 Vision 工业相机。
一个库驱动所有合规相机——海康机器人、Basler等完成参数调节与图像采集，主机端无需任何厂商 SDK。

Rust 实现，C ABI，MIT 许可。

## 能力

- GigE Vision（GVCP/GVSP，规范 3.0 / 1.2）与 USB3 Vision（GenCP
  1.3.1，规范 1.2）传输层实现
- 完整 GenTL 1.6 接口——64 个导出函数，零拷贝采集
- 全部 273 个 PFNC 像素格式；相机全部参数经相机自带 GenICam XML 调节

## 快速开始

```bash
cargo build --release    # 产出 gentl_gige.dll / gentl_u3v.dll，即两个 GenTL producer
```

把 DLL 指给任意 GenTL consumer（加入 `GENICAM_GENTL64_PATH` 或显式
加载），pylon / HALCON / 自研 GenApi 代码皆可。`consumer-test/` 提供
C++ 参考实现；`cargo run --example grab_u3v -- <id> 100` 可直接收图，
`cargo test --workspace` 无硬件回归。

## 状态

USB3 Vision：海康威视usb3vision已通过验证（注意使用前需要用zadig将usb驱动切换未winusb）
GigE Vision：手里没有相机未测试

## 许可

MIT —— 见 [LICENSE](LICENSE)。`include/GenTL.h` / `include/PFNC.h`
为 EMVA 版权头文件（BSD 风格，版权头保留）。

*GenTL、GenICam、GigE Vision、USB3 Vision 为各自所有方的商标。本项目
基于公开规范的独立实现，与 EMVA、AIA/A3 无隶属或背书关系。*
