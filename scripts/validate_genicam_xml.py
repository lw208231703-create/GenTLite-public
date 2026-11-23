#!/usr/bin/env python3
"""Validate a GenICam XML device description against the GenApi 1.1 XSD.

Why: NI-IMAQdx / GenTL hosts refuse a camera whose XML violates the GenApi
schema ("Unable to load camera's XML file", 0xBFF6901D). A plain
well-formedness check is not enough — the 2026-11 virtual-camera work shipped
an XML whose ``<pInvalidator>`` sat inside an ``<Enumeration>`` (schema:
"element not expected"), well-formed but unloadable. Validating against the
schema NI itself ships pinpoints the offending line in milliseconds, before
running CameraValidator.

The XSD is looked up under the NI-IMAQdx install (``GenICam/xml/GenApi/
GenApiSchema_Version_1_1.xsd``); use ``--schema`` to point elsewhere.
``lxml`` is required for validation; the fetch path only needs the stdlib.

Usage:
  # validate a file already fetched from the device
  python scripts/validate_genicam_xml.py --xml dump.xml

  # fetch FirstURL + READMEM over GVCP, then validate (a vgige gateway or the
  # simulator must be running and reachable)
  python scripts/validate_genicam_xml.py --fetch 192.168.137.1:3956

Exit codes: 0 valid, 1 schema violations, 2 setup error (no lxml/XSD/XML).
"""

from __future__ import annotations

import argparse
import socket
import struct
import sys
from pathlib import Path

GVCP_KEY = 0x42
FLAG_ACK = 0x01
READMEM_CMD = 0x0084
FIRST_URL = 0x0200
FIRST_URL_LEN = 512
HEADER_LEN = 8

# First URL slot ends before SECOND_URL (0x0400) per Table 36-1.
KNOWN_SCHEMAS = [
    Path(
        r"D:\Program Files (x86)\National Instruments\NI-IMAQdx\GenICam\xml"
        r"\GenApi\GenApiSchema_Version_1_1.xsd"
    ),
    Path(
        r"C:\Program Files (x86)\National Instruments\NI-IMAQdx\GenICam\xml"
        r"\GenApi\GenApiSchema_Version_1_1.xsd"
    ),
    Path(
        r"C:\Program Files\National Instruments\NI-IMAQdx\GenICam\xml"
        r"\GenApi\GenApiSchema_Version_1_1.xsd"
    ),
]


def _gvcp(sock: socket.socket, target: tuple[str, int], command: int,
          payload: bytes, req_id: int) -> bytes:
    packet = (
        bytes([GVCP_KEY, FLAG_ACK])
        + struct.pack(">HHH", command, len(payload), req_id)
        + payload
    )
    sock.sendto(packet, target)
    data, _ = sock.recvfrom(65535)
    status, ack, length, ack_id = struct.unpack(">HHHH", data[0:HEADER_LEN])
    if ack != command + 1:
        raise RuntimeError(f"unexpected ack {ack:#06x} for command {command:#06x}")
    if ack_id != req_id:
        raise RuntimeError(f"ack_id {ack_id} != req_id {req_id}")
    if status != 0:
        raise RuntimeError(f"GVCP status {status:#06x} for command {command:#06x}")
    return data[HEADER_LEN:HEADER_LEN + length]


def fetch_xml(host: str, port: int, timeout: float) -> bytes:
    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    sock.settimeout(timeout)
    url_block = _gvcp(
        sock, (host, port), READMEM_CMD,
        struct.pack(">IHH", FIRST_URL, 0, FIRST_URL_LEN), 1,
    )
    # READMEM_ACK payload: 4-byte address echo, then the bytes.
    url = url_block[4:].split(b"\0", 1)[0].decode()
    if not url.startswith("local:"):
        raise RuntimeError(f"device First URL is not a local: URL: {url!r}")
    parts = url.split(";")
    if len(parts) < 3:
        raise RuntimeError(f"malformed local: URL: {url!r}")
    addr = int(parts[1], 16)
    length = int(parts[2], 16)
    xml = bytearray()
    req_id = 2
    offset = 0
    while offset < length:
        n = min(512, length - offset)
        aligned = (n + 3) & ~3  # READMEM needs a 4-byte multiple
        block = _gvcp(
            sock, (host, port), READMEM_CMD,
            struct.pack(">IHH", addr + offset, 0, aligned), req_id,
        )
        xml.extend(block[4:4 + n])
        offset += n
        req_id += 1
    return bytes(xml)


def find_schema(explicit: str | None) -> Path:
    if explicit:
        path = Path(explicit)
        if path.is_file():
            return path
        raise FileNotFoundError(f"--schema not found: {path}")
    for candidate in KNOWN_SCHEMAS:
        if candidate.is_file():
            return candidate
    raise FileNotFoundError(
        "NI GenApi XSD not found; pass --schema <GenApiSchema_Version_1_1.xsd>"
    )


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    source = parser.add_mutually_exclusive_group(required=True)
    source.add_argument("--xml", help="XML file to validate")
    source.add_argument("--fetch", help="host:port to fetch the XML from (GVCP)")
    parser.add_argument("--schema", help="GenApi XSD path (default: NI install)")
    parser.add_argument("--out", help="write the fetched XML to this path")
    parser.add_argument("--timeout", type=float, default=2.0)
    args = parser.parse_args()

    if args.fetch:
        host, _, port_text = args.fetch.partition(":")
        xml = fetch_xml(host, int(port_text or 3956), args.timeout)
        if args.out:
            Path(args.out).write_bytes(xml)
            print(f"fetched {len(xml)} bytes -> {args.out}")
    else:
        xml = Path(args.xml).read_bytes()
        print(f"read {len(xml)} bytes from {args.xml}")

    schema = find_schema(args.schema)
    print(f"schema: {schema}")

    try:
        from lxml import etree
    except ImportError:
        print(
            "error: lxml is required for schema validation "
            "(the DSH bundled python has it: "
            "dependencies/python/python.exe)",
            file=sys.stderr,
        )
        return 2

    schema_tree = etree.parse(str(schema))
    validator = etree.XMLSchema(schema_tree)
    try:
        doc = etree.fromstring(xml)
    except etree.XMLSyntaxError as exc:
        print(f"XML is not well-formed: {exc}")
        return 1

    if validator.validate(etree.ElementTree(doc)):
        print("OK: XML validates against the GenApi schema")
        return 0

    print("INVALID: GenApi schema violations:")
    for error in validator.error_log:
        print(f"  line {error.line}: {error.message}")
    return 1


if __name__ == "__main__":
    sys.exit(main())
