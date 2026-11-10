# GenTL on Linux

The Linux producer builds the same sources as Windows.

```bash
cargo build --release
cargo cti
```

Output in `target/release/`:

```text
libgentl_gige.so   libgentl_u3v.so
gentl_gige.cti     gentl_u3v.cti
```

Add `target/release/` to `GENICAM_GENTL64_PATH`, or load a `.cti` explicitly.
The USB3 Vision backend uses libusb (`rusb`); make sure the target camera
node is accessible to the user running the consumer.

On the Orange Pi gateway, the virtual camera runs as a Linux gadget; see
[`../../../board/`](../../../board/) for UDC, FunctionFS and systemd assets.
