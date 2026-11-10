# gentl — universal GenTL producer

This folder contains the reusable, transport-oriented GenTL implementation.
It is the `.cti` producer used by third-party consumers such as NI-IMAQdx,
MVTec HALCON, pylon and the project's own consumers.

## Layout

| Path | Purpose |
|---|---|
| `core/` | GenTL ABI types, data-stream engine, pixel formats, errors |
| `gvcp/` | GVCP wire format, client/server, GigE bootstrap registers |
| `gige/` | GigE Vision producer (GVCP control + GVSP streaming) |
| `u3v/` | USB3 Vision producer (GenCP + SIRM/U3V streaming) |
| `simulator/` | hardware-free GigE camera simulator and flood tools |
| `packager/` | `cargo cti` / `cargo ctid` artifact packaging |
| `consumer-test/` | Windows C++ GenApi reference consumer |
| `include/` | GenTL.h and PFNC.h reference headers |
| `platforms/` | platform-specific build and host notes |

The Rust source is shared by Linux and Windows. Only the final artifact
extension differs:

```text
Linux:   libgentl_gige.so / libgentl_u3v.so  -> gentl_gige.cti / gentl_u3v.cti
Windows: gentl_gige.dll / gentl_u3v.dll      -> gentl_gige.cti / gentl_u3v.cti
```

Build from the repository root:

```bash
cargo cti
```

See [`platforms/linux/README.md`](platforms/linux/README.md) and
[`platforms/windows/README.md`](platforms/windows/README.md).
