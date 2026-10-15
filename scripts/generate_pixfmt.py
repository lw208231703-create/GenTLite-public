#!/usr/bin/env python3
"""Generate crates/gentl-core/src/pixfmt_table.rs from include/PFNC.h.

The table is explicit (every PFNC format enumerated, reviewable) and
faithful (mechanically extracted from the authoritative EMVA header, so no
manual transcription errors). Re-run this script when the PFNC version in
include/ changes; the generated file carries a header saying so.

Entry layout: (name, value, bits_per_pixel, planar)
  - bits_per_pixel comes from the PFNC value's own bits 16-23 (the EMVA
    size field), verified against the value itself.
  - planar marks multi-plane layouts (Coord3D_*_Planar, YCbCr*_Semiplanar)
    whose total payload size is still computable but whose GenTL
    presentation needs parts (v1.5, currently not implemented).
"""
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
PFNC_H = ROOT / "include" / "PFNC.h"
OUT = ROOT / "crates" / "gentl-core" / "src" / "pixfmt_table.rs"

# Values below 0x01000000 are helpers/masks, not pixel formats.
FMT = re.compile(r"^#define\s+(PFNC_[A-Za-z0-9_]+)\s+(0x[0-9A-Fa-f]{8})\s*(?:/\*.*\*/)?\s*$")

PLANAR_HINTS = ("_Planar", "_Semiplanar")


def main() -> int:
    entries = []
    for line in PFNC_H.read_text(encoding="latin-1").splitlines():
        m = FMT.match(line.strip())
        if not m:
            continue
        name, value_s = m.group(1), m.group(2)
        value = int(value_s, 16)
        if value < 0x01000000 or value >= 0x80000000:
            continue  # masks/helpers and the CUSTOM sentinel
        short = name[len("PFNC_"):]
        bits = (value >> 16) & 0xFF
        if bits == 0 or bits > 255:
            print(f"SKIP {short} 0x{value:08x}: implausible size field (bits/px total across components can reach 192) {bits}", file=sys.stderr)
            continue
        planar = any(h in short for h in PLANAR_HINTS)
        entries.append((short, value, bits, planar))

    entries.sort(key=lambda e: e[1])
    lines = [
        "//! GENERATED FILE — do not edit by hand.",
        "//! Source: `include/PFNC.h`, extracted by `scripts/generate_pixfmt.py`.",
        "//! Re-run the generator when the PFNC header version changes.",
        "//!",
        "//! Explicit enumeration of every PFNC pixel format (explicit rather",
        "//! than derived, per project decision 2026-10-14). `bits` is the",
        "//! EMVA size field of the value itself (bits 16-23), asserted in",
        "//! unit tests. `planar` entries have one total payload size here;",
        "//! their GenTL *presentation* needs parts (v1.5), not implemented.",
        "",
        "#![allow(dead_code)]",
        "",
        "/// One PFNC pixel format: (name, PFNC value, bits per pixel, planar).",
        "pub const FORMATS: &[(&str, u32, u8, bool)] = &[",
    ]
    for short, value, bits, planar in entries:
        lines.append(f'    ("{short}", 0x{value:08X}, {bits}, {str(planar).lower()}),')
    lines.extend(["];", ""])
    OUT.write_text("\n".join(lines), encoding="utf-8", newline="\n")
    print(f"wrote {OUT}: {len(entries)} formats")
    return 0


if __name__ == "__main__":
    sys.exit(main())
