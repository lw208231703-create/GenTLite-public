# GenTL on Windows

The Windows producer builds the same sources as Linux.

```powershell
cargo build --release
cargo cti
```

Output in `target\release\`:

```text
gentl_gige.dll   gentl_u3v.dll
gentl_gige.cti   gentl_u3v.cti
```

Add `target\release\` to `GENICAM_GENTL64_PATH`, or load a `.cti` explicitly.

## U3V driver note

NI-IMAQdx and the GenTL U3V producer cannot own the same USB3 Vision
interface at once. To use `gentl_u3v.cti`, bind the interface to WinUSB
(for example with Zadig); to use NI-IMAQdx, restore the NI driver first.

The reference GenApi consumer is under
[`../../consumer-test/`](../../consumer-test/).
