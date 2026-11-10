# gateway — Linux relay gateway

The gateway relays a real camera (or the Null source) to the virtual camera
fronts. On the Orange Pi it runs as `vgige-cli` under systemd.

## Layout

| Path | Purpose |
|---|---|
| `core/` | relay engine, `CameraBack`/`FrameSource`/`FrameSession` seams, register space, XML injection |
| `backends/gige/` | GigE Vision camera backend (GVCP + GVSP reassembly) |
| `backends/u3v/` | USB3 Vision camera backend (GenCP/U3V client facade) |
| `cli/` | `vgige-cli` binary and gateway feature matrix |

Camera fronts live in [`../virtual/`](../virtual/README.md); the reusable
GenTL producer lives in [`../gentl/`](../gentl/README.md).

## Common builds

```bash
# no camera -> virtual U3V
cargo build --release -p vgige-cli \
  --no-default-features --features back-null,front-u3v

# no camera -> virtual GigE
cargo build --release -p vgige-cli \
  --no-default-features --features back-null,front-gige

# real GigE camera -> virtual U3V
cargo build --release -p vgige-cli \
  --no-default-features --features back-gige,front-u3v
```

Board units and deployment assets are under [`../board/`](../board/).
