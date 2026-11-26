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

## Producer / GenApi boundary

A GenTL producer does not implement GenApi and does not evaluate feature
nodes. The division of responsibility is:

- The producer exposes discovery/open, the device XML URL
  (`GCGetPortURL`), register/memory access (`GCReadPort`/`GCWritePort`),
  data streams, buffers and events.
- The consumer fetches the XML through the reported URL/port, builds the
  GenApi node tree, and resolves feature names such as `ExposureTime`,
  `Width`, `Height`, `OffsetX`, `OffsetY`, `TriggerMode`, `Gain`,
  `ExposureAuto` or `UserSetSave` into register accesses.

The producer inspects the XML only to resolve `PayloadSize` before
`DSStartAcquisition`; it does not replace or rewrite the device XML. GigE
devices serve their own First URL XML; U3V devices serve the manifest XML.
Third-party consumers therefore see the camera's normal node tree.

Both producers export all 64 GenTL 1.6 entry points. Optional features outside
the implemented image-acquisition surface answer `GC_ERR_NOT_IMPLEMENTED`
instead of being faked: `DSGetBufferPartInfo`, `DSAnnounceCompositeBuffer`,
`DSGetNumFlows` and `DSGetFlowInfo` are such stubs, and `DSGetNumBufferParts`
answers `GC_ERR_NO_DATA` per GenTL 1.6 §6.3.5.15 (single-part buffers). The
stacked buffer-info queries, `GCReadPortStacked`/`GCWritePortStacked` and
`GCGetPortURLInfo` are implemented; `GCGetPortURL` plus `GCReadPort` remains
the portable way to fetch the device description.

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
