#!/usr/bin/env python3
"""Set the null backend's FrameRate register through the GenTL producer."""

import argparse
import ctypes as C
from pathlib import Path

parser = argparse.ArgumentParser()
parser.add_argument("fps", type=int)
parser.add_argument("--dll", default="target/release/gentl_u3v.cti")
args = parser.parse_args()
if not 1 <= args.fps <= 240:
    parser.error("fps must be in 1..240")

lib = C.CDLL(str(Path(args.dll).resolve()))
P = C.c_void_p
SZ = C.c_size_t
U64 = C.c_uint64
I32 = C.c_int32


def fn(name, params):
    f = getattr(lib, name)
    f.restype = I32
    f.argtypes = params
    return f


init = fn("GCInitLib", [])
tl_open = fn("TLOpen", [C.POINTER(P)])
tl_close = fn("TLClose", [P])
tl_iface_id = fn("TLGetInterfaceID", [P, C.c_uint32, P, C.POINTER(SZ)])
tl_open_iface = fn("TLOpenInterface", [P, C.c_char_p, C.POINTER(P)])
if_update = fn("IFUpdateDeviceList", [P, C.POINTER(C.c_uint8), U64])
if_num = fn("IFGetNumDevices", [P, C.POINTER(C.c_uint32)])
if_dev_id = fn("IFGetDeviceID", [P, C.c_uint32, P, C.POINTER(SZ)])
if_open = fn("IFOpenDevice", [P, C.c_char_p, I32, C.POINTER(P)])
if_close = fn("IFClose", [P])
dev_close = fn("DevClose", [P])
dev_port = fn("DevGetPort", [P, C.POINTER(P)])
write_port = fn("GCWritePort", [P, U64, P, C.POINTER(SZ)])
read_port = fn("GCReadPort", [P, U64, P, C.POINTER(SZ)])


def check(result, what):
    if result != 0:
        raise RuntimeError(f"{what}: GC_ERROR {result}")


check(init(), "GCInitLib")
tl = P()
iface = P()
dev = P()
check(tl_open(C.byref(tl)), "TLOpen")
try:
    text = C.create_string_buffer(1024)
    size = SZ(len(text))
    check(tl_iface_id(tl, 0, text, C.byref(size)), "TLGetInterfaceID")
    check(tl_open_iface(tl, text.value, C.byref(iface)), "TLOpenInterface")
    changed = C.c_uint8(0)
    check(if_update(iface, C.byref(changed), 1000), "IFUpdateDeviceList")
    count = C.c_uint32(0)
    check(if_num(iface, C.byref(count)), "IFGetNumDevices")
    if count.value < 1:
        raise RuntimeError("no U3V device")
    size = SZ(len(text))
    check(if_dev_id(iface, 0, text, C.byref(size)), "IFGetDeviceID")
    check(if_open(iface, text.value, 4, C.byref(dev)), "IFOpenDevice")
    port = P()
    check(dev_port(dev, C.byref(port)), "DevGetPort")
    value = args.fps.to_bytes(4, "little")
    size = SZ(4)
    check(write_port(port, 0x0004_101C, value, C.byref(size)), "write FrameRate")
    actual = C.create_string_buffer(4)
    size = SZ(4)
    check(read_port(port, 0x0004_101C, actual, C.byref(size)), "read FrameRate")
    readback = int.from_bytes(actual.raw, "little")
    if readback != args.fps:
        raise RuntimeError(f"FrameRate readback {readback} != {args.fps}")
    print(f"FrameRate={readback}")
finally:
    if dev.value:
        dev_close(dev)
    if iface.value:
        if_close(iface)
    if tl.value:
        tl_close(tl)
