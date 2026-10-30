#!/usr/bin/env python3
"""Exercise EVENT_REMOTE_DEVICE against the vgige virtual U3V camera."""

import ctypes as C
from pathlib import Path

lib = C.CDLL(str(Path("target/debug/gentl_u3v.dll").resolve()))
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
register_event = fn("GCRegisterEvent", [P, I32, C.POINTER(P)])
unregister_event = fn("GCUnregisterEvent", [P, I32])
get_data = fn("EventGetData", [P, P, C.POINTER(SZ), U64])
get_info = fn("EventGetInfo", [P, I32, C.POINTER(I32), P, C.POINTER(SZ)])
get_data_info = fn("EventGetDataInfo", [P, P, SZ, I32, C.POINTER(I32), P, C.POINTER(SZ)])
flush = fn("EventFlush", [P])
kill = fn("EventKill", [P])


def check(result, what):
    if result != 0:
        raise RuntimeError(f"{what}: GC_ERROR {result}")


check(init(), "GCInitLib")
tl = P()
iface = P()
dev = P()
event = P()
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
    check(register_event(dev, 4, C.byref(event)), "GCRegisterEvent remote")

    def fire_and_read():
        value = (1).to_bytes(4, "little")
        size = SZ(4)
        check(write_port(port, 0x0003_1008, value, C.byref(size)), "Event Test Control")
        raw = C.create_string_buffer(64)
        size = SZ(len(raw))
        check(get_data(event, raw, C.byref(size), 2000), "EventGetData")
        data = raw.raw[: size.value]
        if not data.startswith(b"U3VE"):
            raise RuntimeError("bad event prefix")
        event_id = int.from_bytes(data[14:16], "little")
        timestamp = int.from_bytes(data[16:24], "little")
        if event_id != 0x4FFF or timestamp == 0:
            raise RuntimeError("bad event content")
        numeric = U64(0)
        dtype = I32(0)
        out_size = SZ(C.sizeof(numeric))
        check(get_data_info(event, data, len(data), 0, C.byref(dtype), C.byref(numeric), C.byref(out_size)), "EVENT_DATA_ID")
        if numeric.value != 0x4FFF:
            raise RuntimeError("event id mismatch")

    fire_and_read()
    fired = U64(0)
    dtype = I32(0)
    size = SZ(C.sizeof(fired))
    check(get_info(event, 2, C.byref(dtype), C.byref(fired), C.byref(size)), "EVENT_NUM_FIRED")
    if fired.value < 1:
        raise RuntimeError("event was not counted")

    value = (1).to_bytes(4, "little")
    size = SZ(4)
    check(write_port(port, 0x0003_1008, value, C.byref(size)), "Event Test Control for flush")
    check(flush(event), "EventFlush")
    raw = C.create_string_buffer(64)
    size = SZ(len(raw))
    result = get_data(event, raw, C.byref(size), 50)
    if result != -1011:
        raise RuntimeError(f"flush left an event behind: {result}")
    check(kill(event), "EventKill")
    result = get_data(event, raw, C.byref(size), 50)
    if result != -1012:
        raise RuntimeError(f"EventKill did not abort: {result}")
    check(unregister_event(dev, 4), "GCUnregisterEvent")
    print("REMOTE EVENT OK: register, DEI delivery, data info, flush, kill, unregister")
finally:
    if dev.value:
        dev_close(dev)
    if iface.value:
        if_close(iface)
    if tl.value:
        tl_close(tl)
