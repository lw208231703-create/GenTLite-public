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
cargo cti    # build the producer crates, then place gentl_gige.cti /
             # gentl_u3v.cti next to the shared libraries in
             # target/release/ (Windows DLL or Unix .so)
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
- `vgige` virtual camera: Orange Pi 5 Plus / vendor kernel `6.1.115-rk35xx`,
  U3V and GigE fronts running `1280×1024 Mono16` at 30 fps.
  - NI-IMAQdx: 100/100 frames on both U3V and GigE.
  - GenTLite producer: 5-frame captures byte-identical to NI and to the
    simulated image for both transports.
  - GenTL U3V on Windows requires WinUSB for the U3V interface; NI-IMAQdx
    cannot share that interface at the same time.
  - The Null camera exposes both `Mono8` and `Mono16`; MVS/LD can use
    Mono8 while NI and GenTL can use Mono16.
- GenTL CTI packaging: `cargo cti` produces `.cti` twins on both Windows
  (`.dll`) and Linux (`.so`), and `TL_INFO_PATHNAME` reports the loaded CTI
  path on both platforms.
- Third-party host status: NI MAX/NI-IMAQdx works; MVS and LD can display the
  virtual camera's 8-bit stream but have incomplete 16-bit display support.
  The producer's XML, GVSP leader and GenTL BufferInfo all declare
  `Mono16 = 0x01100001`. See `docs/interop-matrix.md` and
  `docs/vgige-board-kernel-interop-2026-11-04.md`.

## License

MIT — see [LICENSE](LICENSE). `include/GenTL.h` / `include/PFNC.h` are
EMVA-copyrighted headers (BSD-style, headers retained).

*GenTL, GenICam, GigE Vision and USB3 Vision are trademarks of their
respective owners. Independent implementation from public specifications;
not affiliated with or endorsed by EMVA or AIA/A3.*
