//! Real-camera grab: full streaming flow against a physical camera.
//!   GENTLITE_DISCOVERY_TARGETS=<ip:3956> cargo run --example grab_camera -- [device-id] [n_frames]
//!
//! Register addresses (AcquisitionStart/Stop, PayloadSize) are read from
//! THIS camera's XML (LeadingOE clone): Command nodes write
//! <CommandValue> to their pValue IntReg. A production consumer resolves
//! these via GenApi; this tool hardcodes what camera_dump.xml documents.

#![allow(non_snake_case)]

use std::ffi::c_char;
use std::ptr;

use gentl_core::types::*;
use gentl_gige::*;

const REG_ACQ: u64 = 0x1000_0024; // AcquisitionStart(1)/Stop(0), BE
const REG_ACQ_MODE: u64 = 0x1000_0020; // AcquisitionMode: 2 = Continuous
const REG_PAYLOAD_SIZE: u64 = 0x1000_0050; // RO, little-endian

/// Releases the control channel (CCP=0) on panic/early return — a panicked
/// grab must not leave the camera occupied (real-machine lesson 2026-10-19:
/// an unreleased CCP + a camera-side hang made the device vanish until
/// power-cycle).
struct CcpGuard {
    port: PortHandle,
    active: bool,
}

impl CcpGuard {
    fn disarm(mut self) {
        self.active = false;
    }
}

impl Drop for CcpGuard {
    fn drop(&mut self) {
        if self.active {
            let zero = 0u32.to_be_bytes();
            let mut size = zero.len();
            let _ = GCWritePort(self.port, 0x0A00, zero.as_ptr() as *mut core::ffi::c_void, &mut size);
        }
    }
}

fn get_string(mut f: impl FnMut(*mut c_char, *mut usize) -> GCError) -> String {
    let mut buf = vec![0u8; 65536];
    let mut size = buf.len();
    let r = f(buf.as_mut_ptr() as *mut c_char, &mut size);
    assert_eq!(r, GC_ERR_SUCCESS, "get_string");
    String::from_utf8_lossy(&buf[..size.saturating_sub(1)]).into_owned()
}

fn read_port(h: PortHandle, addr: u64, buf: &mut [u8]) -> GCError {
    let mut size = buf.len();
    GCReadPort(h, addr, buf.as_mut_ptr().cast(), &mut size)
}

fn write_port(h: PortHandle, addr: u64, buf: &[u8]) -> GCError {
    let mut size = buf.len();
    GCWritePort(h, addr, buf.as_ptr().cast(), &mut size)
}

fn last_error() -> String {
    let mut code = 0i32;
    let mut txt = vec![0u8; 1024];
    let mut n = txt.len();
    GCGetLastError(&mut code, txt.as_mut_ptr() as *mut c_char, &mut n);
    format!("{code}: {}", String::from_utf8_lossy(&txt[..n.saturating_sub(1)]))
}

fn main() {
    let mut args = std::env::args().skip(1);
    let wanted = args.next();
    let n_frames: usize = args.next().map(|s| s.parse().unwrap()).unwrap_or(50);

    assert_eq!(GCInitLib(), GC_ERR_SUCCESS);
    let mut tl: TLHandle = ptr::null_mut();
    assert_eq!(TLOpen(&mut tl), GC_ERR_SUCCESS);
    let mut iface: IFHandle = ptr::null_mut();
    let c = std::ffi::CString::new(get_string(|s, sz| TLGetInterfaceID(tl, 0, s, sz))).unwrap();
    assert_eq!(TLOpenInterface(tl, c.as_ptr(), &mut iface), GC_ERR_SUCCESS);
    let mut changed = 0u8;
    assert_eq!(IFUpdateDeviceList(iface, &mut changed, 1000), GC_ERR_SUCCESS);
    let mut nd = 0u32;
    assert_eq!(IFGetNumDevices(iface, &mut nd), GC_ERR_SUCCESS);
    assert!(nd >= 1, "no camera (set GENTLITE_DISCOVERY_TARGETS?)");
    let dev_id = wanted.unwrap_or_else(|| get_string(|s, sz| IFGetDeviceID(iface, 0, s, sz)));
    println!("opening {dev_id}");
    let c_dev = std::ffi::CString::new(dev_id).unwrap();
    let mut dev: DevHandle = ptr::null_mut();
    let r = IFOpenDevice(iface, c_dev.as_ptr(), 4, &mut dev);
    assert_eq!(r, GC_ERR_SUCCESS, "IFOpenDevice: {}", last_error());

    let mut port: PortHandle = ptr::null_mut();
    assert_eq!(DevGetPort(dev, &mut port), GC_ERR_SUCCESS);

    // PayloadSize (RO). BIG-endian on this camera's current firmware
    // (camera_dump.xml declares BigEndian; verified by the parameter test
    // 2026-10-19: register holds 1310720 for 1280x1024 Mono8 — reading it
    // little-endian yields 5120, the exact value that misled the first
    // acceptance run). The old firmware's XML declared LE while the wire
    // was BE (Q28 #5); the improved firmware is consistent BE.
    let mut ps = [0u8; 4];
    assert_eq!(read_port(port, REG_PAYLOAD_SIZE, &mut ps), GC_ERR_SUCCESS);
    let payload_size = u32::from_be_bytes(ps) as usize;
    println!("payload size: {payload_size}");
    assert!(payload_size > 0);
    let buf_size = payload_size;

    let mut ds: DSHandle = ptr::null_mut();
    let c_sid = std::ffi::CString::new("Stream0").unwrap();
    assert_eq!(DevOpenDataStream(dev, c_sid.as_ptr(), &mut ds), GC_ERR_SUCCESS);
    let mut ev: EventHandle = ptr::null_mut();
    assert_eq!(GCRegisterEvent(ds, 1, &mut ev), GC_ERR_SUCCESS);

    const NUM_BUFFERS: usize = 4;
    let mut buffers: Vec<Vec<u8>> = (0..NUM_BUFFERS).map(|_| vec![0u8; buf_size]).collect();
    let mut handles: Vec<BufferHandle> = Vec::new();
    for buf in buffers.iter_mut() {
        let mut bh: BufferHandle = ptr::null_mut();
        assert_eq!(
            DSAnnounceBuffer(ds, buf.as_mut_ptr().cast(), buf.len(), ptr::null_mut(), &mut bh),
            GC_ERR_SUCCESS
        );
        handles.push(bh);
    }
    for bh in &handles {
        assert_eq!(DSQueueBuffer(ds, *bh), GC_ERR_SUCCESS);
    }

    // Open the stream channel, then arm the camera (SFNC AcquisitionStart).
    // AcquisitionMode=Continuous(2) first: this camera's default is not
    // free-running (real-machine finding 2026-10-19; mode reg per its XML).
    assert_eq!(write_port(port, REG_ACQ_MODE, &2u32.to_be_bytes()), GC_ERR_SUCCESS);
    let guard = CcpGuard { port, active: true };
    assert_eq!(
        DSStartAcquisition(ds, 0, u64::MAX),
        GC_ERR_SUCCESS,
        "DSStartAcquisition: {}",
        last_error()
    );
    assert_eq!(
        write_port(port, REG_ACQ, &1u32.to_be_bytes()),
        GC_ERR_SUCCESS,
        "AcquisitionStart: {}",
        last_error()
    );
    println!("acquisition started, waiting for {n_frames} frames...");

    let mut dtype: InfoDataType = 0;
    let mut ok = 0usize;
    for i in 0..n_frames {
        let mut raw = [0u8; 16];
        let mut n = raw.len();
        let r = EventGetData(ev, raw.as_mut_ptr().cast(), &mut n, if i == 0 { 8000 } else { 5000 });
        assert_eq!(r, GC_ERR_SUCCESS, "frame {i} event: {}", last_error());
        let nd2 = unsafe { (raw.as_ptr() as *const EventNewBufferData).read_unaligned() };

        let mut size8 = 8usize;
        let mut w = 0usize;
        assert_eq!(DSGetBufferInfo(ds, nd2.buffer_handle, 10, &mut dtype, (&mut w as *mut usize).cast(), &mut size8), GC_ERR_SUCCESS);
        let mut h = 0usize;
        assert_eq!(DSGetBufferInfo(ds, nd2.buffer_handle, 11, &mut dtype, (&mut h as *mut usize).cast(), &mut size8), GC_ERR_SUCCESS);
        let mut pf = 0u64;
        assert_eq!(DSGetBufferInfo(ds, nd2.buffer_handle, 20, &mut dtype, (&mut pf as *mut u64).cast(), &mut size8), GC_ERR_SUCCESS);
        let mut incomplete = 0u8;
        let mut size1 = 1usize;
        assert_eq!(DSGetBufferInfo(ds, nd2.buffer_handle, 7, &mut dtype, (&mut incomplete as *mut u8).cast(), &mut size1), GC_ERR_SUCCESS);

        let pos = handles.iter().position(|h2| *h2 == nd2.buffer_handle).unwrap();
        let nonzero = buffers[pos].iter().any(|&b| b != 0);
        if incomplete == 0 && nonzero {
            ok += 1;
        }
        if i < 5 || i == n_frames - 1 {
            println!(
                "frame {i}: {w}x{h} pf=0x{pf:08x} incomplete={incomplete} nonzero={nonzero}"
            );
        }
        assert_eq!(DSQueueBuffer(ds, nd2.buffer_handle), GC_ERR_SUCCESS);
    }
    println!("frames complete & non-zero: {ok}/{n_frames}");

    let _ = write_port(port, REG_ACQ, &0u32.to_be_bytes());
    assert_eq!(DSStopAcquisition(ds, 0), GC_ERR_SUCCESS);
    assert_eq!(EventKill(ev), GC_ERR_SUCCESS);
    assert_eq!(GCUnregisterEvent(ds, 1), GC_ERR_SUCCESS);
    assert_eq!(DSFlushQueue(ds, 4), GC_ERR_SUCCESS);
    for bh in &handles {
        assert_eq!(DSRevokeBuffer(ds, *bh, ptr::null_mut(), ptr::null_mut()), GC_ERR_SUCCESS);
    }
    assert_eq!(DSClose(ds), GC_ERR_SUCCESS);
    assert_eq!(DevClose(dev), GC_ERR_SUCCESS);
    assert_eq!(IFClose(iface), GC_ERR_SUCCESS);
    assert_eq!(TLClose(tl), GC_ERR_SUCCESS);

    if ok == n_frames {
        println!("GRAB OK: {ok}/{n_frames} frames");
    } else {
        println!("GRAB INCOMPLETE: {ok}/{n_frames}");
        std::process::exit(1);
    }
}
