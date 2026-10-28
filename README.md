# GenTLite

[![CI](https://github.com/lw208231703-create/GenTLite-public/actions/workflows/ci.yml/badge.svg)](https://github.com/lw208231703-create/GenTLite-public/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-yellow.svg)](LICENSE)

**English** | [简体中文](README.zh-CN.md)

A universal GenTL producer (`.cti`) for GigE Vision and USB3 Vision
machine-vision cameras. The goal is one CTI for compliant cameras, with
parameter control and image acquisition and no vendor SDK on the host.

Written in Rust, C ABI, MIT licensed.

## Capabilities

- GigE Vision (GVCP/GVSP, spec 3.0 / 1.2) and USB3 Vision (GenCP 1.3.1,
  spec 1.2) transport implementations
- GenTL 1.6 C ABI — 64 entry points; zero-copy GVSP acquisition and
  segmented U3V reassembly
- All 273 PFNC pixel formats; camera parameters via the camera's own
  GenICam XML; optional Chunk and Buffer Parts features are implemented on demand

## Quick start

```bash
cargo cti    # build the workspace, then place gentl_gige.cti /
             # gentl_u3v.cti next to the DLLs in target/release/
             # (cargo ctid for a debug build; plain cargo build --release
             # still works, it just skips the .cti copies)
```

Point a GenTL consumer at the build directory (add it to
`GENICAM_GENTL64_PATH`, or load a `.cti` explicitly). `consumer-test/` is a
C++ reference; run `cargo run --example grab_u3v -- <id> 100` to grab frames,
or `cargo test --workspace` for a hardware-free regression.

## Status

- USB3 Vision: verified with a Hikrobot U3V camera; on Windows, switch the
  device to WinUSB with Zadig first.
- GigE Vision: 1000/1000 frames verified with a project camera; Linux/aarch64
  completed a 380,000-frame, 2-hour-10-minute soak test.
- `vgige`: GigE relay and USB 2.0 High-Speed U3V gadget soak reports are available.
- Third-party hosts (pylon, MVS, HALCON) and the multi-brand camera matrix
  remain to be verified; see `docs/interop-matrix.md`.

## License

MIT — see [LICENSE](LICENSE). `include/GenTL.h` / `include/PFNC.h` are
EMVA-copyrighted headers (BSD-style, headers retained).

*GenTL, GenICam, GigE Vision and USB3 Vision are trademarks of their
respective owners. Independent implementation from public specifications;
not affiliated with or endorsed by EMVA or AIA/A3.*
