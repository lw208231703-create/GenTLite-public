# GenTLite

[![CI](https://github.com/lw208231703-create/GenTLite-public/actions/workflows/ci.yml/badge.svg)](https://github.com/lw208231703-create/GenTLite-public/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-yellow.svg)](LICENSE)

**English** | [简体中文](README.zh-CN.md)

A universal GenTL producer (`.cti`) for GigE Vision and USB3 Vision
machine-vision cameras. One library drives every compliant camera —
Hikvision, Basler, etc. — for parameter control and image acquisition,
with no vendor SDK on the host.

Written in Rust, C ABI, MIT licensed.

## Capabilities

- GigE Vision (GVCP/GVSP, spec 3.0 / 1.2) and USB3 Vision (GenCP 1.3.1,
  spec 1.2) transport implementations
- Complete GenTL 1.6 interface — 64 entry points, zero-copy acquisition
- All 273 PFNC pixel formats; any camera parameter via the camera's own
  GenICam XML

## Quick start

```bash
cargo cti    # build the workspace, then place gentl_gige.cti /
             # gentl_u3v.cti next to the DLLs in target/release/
             # (cargo ctid for a debug build; plain cargo build --release
             # still works, it just skips the .cti copies)
```

Point any GenTL consumer at the build directory (add it to
`GENICAM_GENTL64_PATH`, or load a `.cti` explicitly) — pylon, HALCON, or your
own GenApi code. `consumer-test/` is a C++ reference; run
`cargo run --example grab_u3v -- <id> 100` to grab frames, or
`cargo test --workspace` for a hardware-free regression.

## Status

USB3 Vision: verified with a Hikvision USB3 Vision camera (note: use Zadig
to switch the USB driver to WinUSB before use).
GigE Vision: no camera on hand yet, untested.

## License

MIT — see [LICENSE](LICENSE). `include/GenTL.h` / `include/PFNC.h` are
EMVA-copyrighted headers (BSD-style, headers retained).

*GenTL, GenICam, GigE Vision and USB3 Vision are trademarks of their
respective owners. Independent implementation from public specifications;
not affiliated with or endorsed by EMVA or AIA/A3.*
