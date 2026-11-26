# Repository layout

The workspace is grouped by product role, not by language or build tool.

```text
GenTLite-public/
├── gentl/                  # universal GenTL producer (.cti)
│   ├── core/               # ABI support (handles/errors/types), DsCore, PFNC, chunk
│   ├── gvcp/               # GVCP wire/client/server + GigE registers
│   ├── gige/               # GigE Vision producer
│   ├── u3v/                # USB3 Vision producer
│   ├── simulator/          # hardware-free GigE camera simulator
│   ├── packager/           # cargo cti/ctid
│   ├── consumer-test/      # Windows C++ GenApi reference consumer
│   ├── include/            # GenTL.h, PFNC.h
│   └── platforms/
│       ├── linux/          # Linux build/host notes
│       └── windows/        # Windows build/WinUSB notes
├── gateway/                # Linux relay gateway
│   ├── core/               # relay, seams, register space, XML
│   ├── backends/
│   │   ├── gige/           # real GigE camera backend
│   │   └── u3v/            # real U3V camera backend
│   └── cli/                # vgige-cli binary
├── virtual/                # camera-facing virtual implementations
│   ├── gige/               # virtual GigE device
│   ├── u3v/                # virtual U3V gadget
│   └── null/               # synthetic camera source
├── board/                  # Orange Pi deployment assets
├── docs/                   # engineering records
│   ├── 00-governance/      # charter, decisions, pending work
│   ├── 10-protocol/        # compliance and wire facts
│   ├── 20-design/          # architecture and implementation design
│   ├── 30-validation/      # interop, stress and board bring-up
│   └── 40-performance/     # performance and bandwidth snapshots
├── notes/                  # specifications (local only; AIA/A3/EMVA copyrights)
├── scripts/                # shared build/deploy helpers
├── Cargo.toml              # workspace members
└── target/                 # build output (ignored)
```

## Dependency direction

```text
gentl/core ──┬── gentl/gvcp ── gentl/gige
             └── gentl/u3v

gateway/core ──┬── gateway/backends/gige
               ├── gateway/backends/u3v
               ├── virtual/null
               └── gateway/cli
virtual/gige ──┘  (uses gateway/core + gentl/core + gentl/gvcp)
virtual/u3v ───── (uses gateway/core + gentl/u3v + gentl/core)

gateway/cli ───── combines one backend + one virtual front
```

The key rule is that `gentl/` never depends on `gateway/` or `virtual/`.
The gateway and virtual camera can share the same transport implementations
without creating a second protocol stack.

## Where to look

| I want to... | Go to |
|---|---|
| change the buffer engine / handle-and-error ABI support | `gentl/core/` |
| change the 64 exported GenTL C entry points | `gentl/gige/src/lib.rs`, `gentl/u3v/src/lib.rs` |
| change GVCP or GigE producer | `gentl/gvcp/`, `gentl/gige/` |
| change U3V producer | `gentl/u3v/` |
| change the relay or camera backends | `gateway/core/`, `gateway/backends/` |
| change virtual GigE/U3V output | `virtual/gige/`, `virtual/u3v/` |
| change the synthetic test pattern | `virtual/null/` |
| deploy on Orange Pi | `board/orangepi5plus/` |
| browse the engineering docs | `docs/README.md` |
| read the protocol specifications | `notes/` (local only; AIA/A3/EMVA copyrights) |
| run the hardware-free tests | `cargo test --workspace` |
