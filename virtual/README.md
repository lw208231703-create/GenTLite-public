# virtual — virtual camera fronts and source

This folder contains the device-side implementations that make the gateway
look like a camera to a host.

## Layout

| Path | Purpose |
|---|---|
| `gige/` | virtual GigE Vision device: GVCP device handler + legacy GVSP 1.2 transmitter |
| `u3v/` | virtual USB3 Vision device: GenCP/SIRM state machine + Linux FunctionFS gadget |
| `null/` | camera-less synthetic source: built-in XML, Mono8/Mono16 test pattern, frame pacing |

`gige/` and `u3v/` are `FrameSession` implementations. They consume frames
from the relay in [`../gateway/`](../gateway/README.md); `null/` is a
`CameraBack`/`FrameSource` used when no real camera is attached.
