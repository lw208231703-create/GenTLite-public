#!/usr/bin/env python3
"""Stress the camera-less vgige GigE front through the real GenTL producer."""

import argparse
import ctypes as C
import os
from pathlib import Path
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--ip", default="192.168.137.55", help="vgige front IP")
    parser.add_argument("--port", type=int, default=3956, help="GVCP UDP port")
    parser.add_argument("--frames", type=int, default=300, help="frames to acquire")
    parser.add_argument("--device", help="device ID; default: first discovered")
    parser.add_argument("--dll", default="target/debug/gentl_gige.dll", help="gentl-gige DLL path")
    parser.add_argument("--packet-size", type=int, default=1500, help="fixed GVSP total packet size")
    args = parser.parse_args()
    if args.frames <= 0:
        parser.error("--frames must be positive")

    os.environ["GENTLITE_DISCOVERY_TARGETS"] = f"{args.ip}:{args.port}"
    os.environ["GENTLITE_GVCP_PORT"] = str(args.port)
    os.environ["GENTLITE_GEV_PACKET_SIZE"] = str(args.packet_size)

    lib = C.CDLL(str(Path(args.dll).resolve()))
    P = C.c_void_p
    SZ = C.c_size_t
    U32 = C.c_uint32
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
    tl_iface_id = fn("TLGetInterfaceID", [P, U32, P, C.POINTER(SZ)])
    tl_open_iface = fn("TLOpenInterface", [P, C.c_char_p, C.POINTER(P)])
    if_update = fn("IFUpdateDeviceList", [P, C.POINTER(C.c_uint8), U64])
    if_num = fn("IFGetNumDevices", [P, C.POINTER(U32)])
    if_dev_id = fn("IFGetDeviceID", [P, U32, P, C.POINTER(SZ)])
    if_open_dev = fn("IFOpenDevice", [P, C.c_char_p, I32, C.POINTER(P)])
    if_close = fn("IFClose", [P])
    dev_open_ds = fn("DevOpenDataStream", [P, C.c_char_p, C.POINTER(P)])
    dev_close = fn("DevClose", [P])
    dev_port = fn("DevGetPort", [P, C.POINTER(P)])
    write_port = fn("GCWritePort", [P, U64, P, C.POINTER(SZ)])
    register_event = fn("GCRegisterEvent", [P, I32, C.POINTER(P)])
    unregister_event = fn("GCUnregisterEvent", [P, I32])
    announce = fn("DSAnnounceBuffer", [P, P, SZ, P, C.POINTER(P)])
    queue = fn("DSQueueBuffer", [P, P])
    start = fn("DSStartAcquisition", [P, I32, U64])
    stop = fn("DSStopAcquisition", [P, I32])
    get_data = fn("EventGetData", [P, P, C.POINTER(SZ), U64])
    event_kill = fn("EventKill", [P])
    buffer_info = fn("DSGetBufferInfo", [P, P, I32, C.POINTER(I32), P, C.POINTER(SZ)])
    flush = fn("DSFlushQueue", [P, I32])
    revoke = fn("DSRevokeBuffer", [P, P, P, P])
    ds_close = fn("DSClose", [P])

    def check(result, what):
        if result != 0:
            raise RuntimeError(f"{what}: GC_ERROR {result}")

    class EventData(C.Structure):
        _fields_ = [("buffer", P), ("user", P)]

    check(init(), "GCInitLib")
    tl = P()
    iface = P()
    dev = P()
    ds = P()
    ev = P()
    handles = []
    check(tl_open(C.byref(tl)), "TLOpen")
    try:
        text = C.create_string_buffer(1024)
        size = SZ(len(text))
        check(tl_iface_id(tl, 0, text, C.byref(size)), "TLGetInterfaceID")
        check(tl_open_iface(tl, text.value, C.byref(iface)), "TLOpenInterface")
        changed = C.c_uint8(0)
        check(if_update(iface, C.byref(changed), 1000), "IFUpdateDeviceList")
        nd = U32(0)
        check(if_num(iface, C.byref(nd)), "IFGetNumDevices")
        if nd.value < 1:
            raise RuntimeError("no virtual GigE device")
        if args.device:
            device_id = args.device.encode()
        else:
            size = SZ(len(text))
            check(if_dev_id(iface, 0, text, C.byref(size)), "IFGetDeviceID")
            device_id = text.value
        print(f"device={device_id.decode()}")
        check(if_open_dev(iface, device_id, 4, C.byref(dev)), "IFOpenDevice")
        check(dev_open_ds(dev, b"Stream0", C.byref(ds)), "DevOpenDataStream")
        check(register_event(ds, 1, C.byref(ev)), "GCRegisterEvent")

        payload = 1280 * 1024
        buffers = [C.create_string_buffer(payload) for _ in range(4)]
        for buf in buffers:
            handle = P()
            check(announce(ds, C.addressof(buf), payload, None, C.byref(handle)), "DSAnnounceBuffer")
            handles.append(handle)
        for handle in handles:
            check(queue(ds, handle), "DSQueueBuffer")

        t0 = time.perf_counter()
        check(start(ds, 0, U64(-1).value), "DSStartAcquisition")
        port = P()
        check(dev_port(dev, C.byref(port)), "DevGetPort")
        size = SZ(4)
        check(write_port(port, 0x0004_0000, (1).to_bytes(4, "little"), C.byref(size)), "AcquisitionStart")
        for index in range(args.frames):
            raw = C.create_string_buffer(C.sizeof(EventData))
            size = SZ(C.sizeof(EventData))
            check(get_data(ev, raw, C.byref(size), 10000), f"EventGetData frame {index}")
            event = EventData.from_buffer(raw)
            dtype = I32(0)
            width = SZ(0)
            height = SZ(0)
            incomplete = C.c_uint8(0)
            size = SZ(C.sizeof(SZ))
            check(buffer_info(ds, event.buffer, 10, C.byref(dtype), C.byref(width), C.byref(size)), "width")
            size = SZ(C.sizeof(SZ))
            check(buffer_info(ds, event.buffer, 11, C.byref(dtype), C.byref(height), C.byref(size)), "height")
            size = SZ(1)
            check(buffer_info(ds, event.buffer, 7, C.byref(dtype), C.byref(incomplete), C.byref(size)), "incomplete")
            if (width.value, height.value, incomplete.value) != (1280, 1024, 0):
                raise RuntimeError(
                    f"bad frame {index}: {width.value}x{height.value} incomplete={incomplete.value}"
                )
            pos = next(i for i, handle in enumerate(handles) if handle.value == event.buffer)
            if not any(buffers[pos].raw):
                raise RuntimeError(f"zero frame {index}")
            check(queue(ds, event.buffer), "DSQueueBuffer")
        elapsed = time.perf_counter() - t0
        print(f"GIGE GRAB OK: {args.frames}/{args.frames} frames, {elapsed:.3f}s, {args.frames/elapsed:.3f} fps")
        check(stop(ds, 0), "DSStopAcquisition")
        size = SZ(4)
        check(write_port(port, 0x0004_0000, (0).to_bytes(4, "little"), C.byref(size)), "AcquisitionStop")
    finally:
        if ev.value:
            event_kill(ev)
            unregister_event(ds, 1)
        if ds.value:
            flush(ds, 4)
            for handle in handles:
                revoke(ds, handle, None, None)
            ds_close(ds)
        if dev.value:
            dev_close(dev)
        if iface.value:
            if_close(iface)
        if tl.value:
            tl_close(tl)


if __name__ == "__main__":
    main()
