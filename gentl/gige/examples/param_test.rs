//! Real-camera parameter test (2026-10-19, camera S/N 262041):
//! read/write every documented parameter register, verify readback, check
//! PayloadSize linkage after an ROI change. Register addresses/endianness
//! from THIS camera's dumped XML (camera_dump.xml, all BigEndian).
//!
//! Usage:
//!   GENTLITE_DISCOVERY_TARGETS=<ip:3956> cargo run --example param_test -- [device-id]

#![allow(non_snake_case)]

use std::ffi::c_char;
use std::ptr;

use gentl_core::types::*;
use gentl_gige::*;

// Parameter registers (camera_dump.xml; all IntReg BigEndian).
const REG_WIDTH: u64 = 0x1000_0008;
const REG_HEIGHT: u64 = 0x1000_000C;
const REG_OFFSET_X: u64 = 0x1000_0010;
const REG_OFFSET_Y: u64 = 0x1000_0014;
const REG_PIXEL_FORMAT: u64 = 0x1000_0018;
const REG_EXPOSURE_US: u64 = 0x1000_0030; // ExposureTime, µs (Converter identity)
const REG_GAIN: u64 = 0x1000_0038;
const REG_PAYLOAD_SIZE: u64 = 0x1000_0050; // RO, BIG-endian (this camera!)
const REG_FRAME_RATE: u64 = 0x1000_0098;

fn get_string(mut f: impl FnMut(*mut c_char, *mut usize) -> GCError) -> String {
    let mut buf = [0u8; 1024];
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

fn rd32(port: PortHandle, addr: u64) -> u32 {
    let mut b = [0u8; 4];
    assert_eq!(read_port(port, addr, &mut b), GC_ERR_SUCCESS, "read {addr:#x}");
    u32::from_be_bytes(b)
}

fn wr32(port: PortHandle, addr: u64, v: u32) {
    assert_eq!(
        write_port(port, addr, &v.to_be_bytes()),
        GC_ERR_SUCCESS,
        "write {addr:#x}"
    );
}

fn last_error() -> String {
    let mut code = 0i32;
    let mut txt = vec![0u8; 1024];
    let mut n = txt.len();
    GCGetLastError(&mut code, txt.as_mut_ptr() as *mut c_char, &mut n);
    format!("{code}: {}", String::from_utf8_lossy(&txt[..n.saturating_sub(1)]))
}

/// Releases the control channel on panic/early return.
struct CcpGuard {
    port: PortHandle,
    active: bool,
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

fn rw_test(port: PortHandle, name: &str, addr: u64, new_value: u32) {
    let before = rd32(port, addr);
    wr32(port, addr, new_value);
    let after = rd32(port, addr);
    let ok = after == new_value;
    println!(
        "{name:12} @ {addr:#010x}: {before} -> wrote {new_value} -> readback {after} {}",
        if ok { "OK" } else { "MISMATCH!" }
    );
    assert!(ok, "{name} readback mismatch");
}

fn main() {
    let wanted: Option<String> = std::env::args().nth(1);

    assert_eq!(GCInitLib(), GC_ERR_SUCCESS);
    let mut tl: TLHandle = ptr::null_mut();
    assert_eq!(TLOpen(&mut tl), GC_ERR_SUCCESS);
    let mut n = 0u32;
    assert_eq!(TLGetNumInterfaces(tl, &mut n), GC_ERR_SUCCESS);
    assert!(n >= 1);
    let iface_id = get_string(|s, sz| TLGetInterfaceID(tl, 0, s, sz));
    let mut iface: IFHandle = ptr::null_mut();
    let c_id = std::ffi::CString::new(iface_id).unwrap();
    assert_eq!(TLOpenInterface(tl, c_id.as_ptr(), &mut iface), GC_ERR_SUCCESS);
    let mut changed = 0u8;
    assert_eq!(IFUpdateDeviceList(iface, &mut changed, 1000), GC_ERR_SUCCESS);
    let mut nd = 0u32;
    assert_eq!(IFGetNumDevices(iface, &mut nd), GC_ERR_SUCCESS);
    assert!(nd >= 1, "no camera discovered");

    let dev_id = wanted.unwrap_or_else(|| get_string(|s, sz| IFGetDeviceID(iface, 0, s, sz)));
    println!("opening {dev_id}");
    let c_dev = std::ffi::CString::new(dev_id).unwrap();
    let mut dev: DevHandle = ptr::null_mut();
    assert_eq!(
        IFOpenDevice(iface, c_dev.as_ptr(), 3, &mut dev),
        GC_ERR_SUCCESS,
        "IFOpenDevice: {}",
        last_error()
    );
    let mut port: PortHandle = ptr::null_mut();
    assert_eq!(DevGetPort(dev, &mut port), GC_ERR_SUCCESS);
    let _guard = CcpGuard { port, active: true };

    // Identity snapshot.
    println!("--- current configuration (all registers BigEndian) ---");
    let w = rd32(port, REG_WIDTH);
    let h = rd32(port, REG_HEIGHT);
    let ox = rd32(port, REG_OFFSET_X);
    let oy = rd32(port, REG_OFFSET_Y);
    let pf = rd32(port, REG_PIXEL_FORMAT);
    let ps = rd32(port, REG_PAYLOAD_SIZE);
    println!("Width={w} Height={h} OffsetX={ox} OffsetY={oy} PixelFormat={pf:#010x}");
    println!("PayloadSize={ps} ({} == W*H*bpp? {})",
        ps,
        if pf == 0x0108_0001 { ps == w * h } else { true });

    // Read/write round-trips: exposure, gain, frame rate.
    println!("--- read/write round-trips ---");
    let exp0 = rd32(port, REG_EXPOSURE_US);
    let exp1 = if exp0 == 10000 { 20000 } else { 10000 };
    rw_test(port, "ExposureTime", REG_EXPOSURE_US, exp1);
    rw_test(port, "ExposureTime", REG_EXPOSURE_US, exp0);

    let g0 = rd32(port, REG_GAIN);
    let g1 = g0 + 2;
    rw_test(port, "Gain", REG_GAIN, g1);
    rw_test(port, "Gain", REG_GAIN, g0);

    let f0 = rd32(port, REG_FRAME_RATE);
    let f1 = if f0 == 30 { 60 } else { 30 };
    rw_test(port, "FrameRate", REG_FRAME_RATE, f1);
    rw_test(port, "FrameRate", REG_FRAME_RATE, f0);

    // ROI change: halve height, verify PayloadSize linkage, restore.
    println!("--- ROI change + PayloadSize linkage ---");
    let h_new = h / 2;
    wr32(port, REG_HEIGHT, h_new);
    let h_rb = rd32(port, REG_HEIGHT);
    let ps_new = rd32(port, REG_PAYLOAD_SIZE);
    println!("Height {h} -> {h_new} (readback {h_rb}); PayloadSize {ps} -> {ps_new}");
    assert_eq!(h_rb, h_new, "Height readback");
    if pf == 0x0108_0001 {
        assert_eq!(ps_new, w * h_new, "PayloadSize did not track ROI");
        println!("PayloadSize linkage OK ({} = {w}x{h_new})", ps_new);
    }
    wr32(port, REG_HEIGHT, h);
    let ps_restored = rd32(port, REG_PAYLOAD_SIZE);
    assert_eq!(ps_restored, ps, "PayloadSize restore");
    println!("restored Height={h}, PayloadSize={ps_restored}");

    // Offset pair: zero it, move it, restore.
    println!("--- offset round-trip ---");
    rw_test(port, "OffsetX", REG_OFFSET_X, 0);
    rw_test(port, "OffsetX", REG_OFFSET_X, ox);
    rw_test(port, "OffsetY", REG_OFFSET_Y, 0);
    rw_test(port, "OffsetY", REG_OFFSET_Y, oy);

    println!("PARAM TEST OK");
}
