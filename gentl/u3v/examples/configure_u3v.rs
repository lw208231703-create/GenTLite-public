//! U3V parameter-adjustment robustness test: ROI windowing (Width/Height/
//! OffsetX/OffsetY), ExposureTime, then streaming after each change to
//! verify the payload pipeline follows the new geometry.
//!   cargo run --example configure_u3v -- [device-id]

#![allow(non_snake_case)]

use std::ffi::c_char;
use std::ptr;

use gentl_core::types::*;
use gentl_u3v::*;

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

fn node_block<'a>(xml: &'a str, _tag_close: &str, name: &str) -> Option<&'a str> {
    const CLOSES: [&str; 5] = [
        "</IntReg>",
        "</MaskedIntReg>",
        "</Integer>",
        "</IntSwissKnife>",
        "</FloatReg>",
    ];
    let key = format!("Name=\"{name}\"");
    let start = xml.find(&key)?;
    let mut end = xml.len();
    for c in CLOSES {
        if let Some(e) = xml[start..].find(c) {
            end = end.min(e + start + c.len());
        }
    }
    Some(&xml[start..end])
}

fn resolve_addr(xml: &str, name: &str, depth: u32) -> Option<u64> {
    if depth > 4 {
        return None;
    }
    for close in ["</IntReg>", "</MaskedIntReg>", "</Integer>", "</IntSwissKnife>", "</FloatReg>"] {
        if let Some(block) = node_block(xml, close, name) {
            if let Some(v) = tag_hex(block, "Address") {
                return Some(v);
            }
            if let Some(v) = tag_hex(block, "Value") {
                return Some(v);
            }
            if let Some(reference) = tag_text(block, "pAddress") {
                return resolve_addr(xml, &reference, depth + 1);
            }
            return None;
        }
    }
    None
}

fn tag_text(block: &str, tag: &str) -> Option<String> {
    let open = block.find(&format!("<{tag}>"))? + tag.len() + 2;
    let close = block[open..].find(&format!("</{tag}>"))? + open;
    Some(block[open..close].trim().to_owned())
}

fn tag_hex(block: &str, tag: &str) -> Option<u64> {
    let t = tag_text(block, tag)?;
    u64::from_str_radix(t.trim_start_matches("0x"), 16).ok()
}

struct Reg {
    addr: u64,
}

fn open_device() -> (TLHandle, IFHandle, DevHandle, PortHandle, String) {
    assert_eq!(GCInitLib(), GC_ERR_SUCCESS);
    let mut tl: TLHandle = ptr::null_mut();
    assert_eq!(TLOpen(&mut tl), GC_ERR_SUCCESS);
    let mut n_ifaces = 0u32;
    assert_eq!(TLGetNumInterfaces(tl, &mut n_ifaces), GC_ERR_SUCCESS);
    let mut iface: IFHandle = ptr::null_mut();
    let mut nd = 0u32;
    for i in 0..n_ifaces {
        let id = get_string(|s, sz| TLGetInterfaceID(tl, i, s, sz));
        let c = std::ffi::CString::new(id).unwrap();
        if TLOpenInterface(tl, c.as_ptr(), &mut iface) != GC_ERR_SUCCESS {
            continue;
        }
        let mut changed = 0u8;
        assert_eq!(IFUpdateDeviceList(iface, &mut changed, 1000), GC_ERR_SUCCESS);
        assert_eq!(IFGetNumDevices(iface, &mut nd), GC_ERR_SUCCESS);
        if nd >= 1 {
            break;
        }
        assert_eq!(IFClose(iface), GC_ERR_SUCCESS);
        iface = ptr::null_mut();
    }
    assert!(nd >= 1, "no U3V camera");
    let dev_id = get_string(|s, sz| IFGetDeviceID(iface, 0, s, sz));
    let c_dev = std::ffi::CString::new(dev_id.clone()).unwrap();
    let mut dev: DevHandle = ptr::null_mut();
    assert_eq!(IFOpenDevice(iface, c_dev.as_ptr(), 4, &mut dev), GC_ERR_SUCCESS, "open: {}", last_error());
    let mut port: PortHandle = ptr::null_mut();
    assert_eq!(DevGetPort(dev, &mut port), GC_ERR_SUCCESS);
    (tl, iface, dev, port, dev_id)
}

fn fetch_xml(port: PortHandle) -> String {
    let url = get_string(|s, sz| GCGetPortURL(port, s, sz));
    let body = url.split(char::from(0)).next().unwrap_or("");
    let body = body.trim_start_matches("Local:").trim_start_matches("local:");
    let mut parts = body.split(';');
    let _file = parts.next().unwrap_or("");
    let addr = u64::from_str_radix(parts.next().unwrap_or("0").trim(), 16).expect("url addr");
    let len = u64::from_str_radix(parts.next().unwrap_or("0").trim(), 16).expect("url len") as usize;
    let mut blob = vec![0u8; len];
    assert_eq!(read_port(port, addr, &mut blob), GC_ERR_SUCCESS);
    if blob.starts_with(b"PK") {
        let reader = std::io::Cursor::new(blob);
        let mut archive = zip::ZipArchive::new(reader).expect("zip");
        let mut chosen = None;
        for i in 0..archive.len() {
            let mut e = archive.by_index(i).expect("entry");
            if e.name().to_ascii_lowercase().ends_with(".xml") {
                let mut v = Vec::new();
                std::io::Read::read_to_end(&mut e, &mut v).unwrap();
                chosen = Some(v);
                break;
            }
        }
        blob = chosen.expect("xml in zip");
    }
    String::from_utf8_lossy(&blob).into_owned()
}

fn r32(port: PortHandle, addr: u64) -> u32 {
    let mut b = [0u8; 4];
    assert_eq!(read_port(port, addr, &mut b), GC_ERR_SUCCESS, "read reg 0x{addr:x}: {}", last_error());
    u32::from_le_bytes(b)
}

fn w32(port: PortHandle, addr: u64, v: u32) {
    assert_eq!(write_port(port, addr, &v.to_le_bytes()), GC_ERR_SUCCESS, "write reg 0x{addr:x}: {}", last_error());
}

static mut ACQ_START: u64 = 0;
static mut ACQ_STOP: u64 = 0;

unsafe fn acq_regs() -> (u64, u64) {
    (ACQ_START, ACQ_STOP)
}

/// Stream `frames` and verify leader metadata matches (w, h).
fn grab_and_verify(dev: DevHandle, port: PortHandle, expect_w: usize, expect_h: usize, frames: usize) {
    let mut ds: DSHandle = ptr::null_mut();
    let c_sid = std::ffi::CString::new("Stream0").unwrap();
    assert_eq!(DevOpenDataStream(dev, c_sid.as_ptr(), &mut ds), GC_ERR_SUCCESS);
    let mut ev: EventHandle = ptr::null_mut();
    assert_eq!(GCRegisterEvent(ds, 1, &mut ev), GC_ERR_SUCCESS);
    let _ps = [0u8; 8];
    // payload size from the SIRM-driven stream info is implicit; size
    // buffers generously (2x expectation covers padding).
    let payload_size = expect_w * expect_h * 2 + 4096;
    let mut buffers: Vec<Vec<u8>> = (0..4).map(|_| vec![0u8; payload_size]).collect();
    let mut handles = Vec::new();
    for b in buffers.iter_mut() {
        let mut bh: BufferHandle = ptr::null_mut();
        assert_eq!(DSAnnounceBuffer(ds, b.as_mut_ptr().cast(), b.len(), ptr::null_mut(), &mut bh), GC_ERR_SUCCESS);
        handles.push(bh);
    }
    for h in &handles {
        assert_eq!(DSQueueBuffer(ds, *h), GC_ERR_SUCCESS);
    }
    assert_eq!(DSStartAcquisition(ds, 0, u64::MAX), GC_ERR_SUCCESS, "start: {}", last_error());
    let (acq_on, acq_off) = unsafe { acq_regs() };
    w32(port, acq_on, 1);

    let mut dtype: InfoDataType = 0;
    let mut ok = 0usize;
    for i in 0..frames {
        let mut raw = [0u8; 16];
        let mut n = raw.len();
        let r = EventGetData(ev, raw.as_mut_ptr().cast(), &mut n, 10_000);
        assert_eq!(r, GC_ERR_SUCCESS, "frame {i}: {}", last_error());
        let nd = unsafe { (raw.as_ptr() as *const EventNewBufferData).read_unaligned() };
        let mut sz8 = 8usize;
        let mut w = 0usize;
        assert_eq!(DSGetBufferInfo(ds, nd.buffer_handle, 10, &mut dtype, (&mut w as *mut usize).cast(), &mut sz8), GC_ERR_SUCCESS);
        let mut h = 0usize;
        assert_eq!(DSGetBufferInfo(ds, nd.buffer_handle, 11, &mut dtype, (&mut h as *mut usize).cast(), &mut sz8), GC_ERR_SUCCESS);
        let mut incomplete = 0u8;
        let mut sz1 = 1usize;
        assert_eq!(DSGetBufferInfo(ds, nd.buffer_handle, 7, &mut dtype, (&mut incomplete as *mut u8).cast(), &mut sz1), GC_ERR_SUCCESS);
        let pos = handles.iter().position(|x| *x == nd.buffer_handle).unwrap();
        let nonzero = buffers[pos].iter().any(|&b| b != 0);
        if w == expect_w && h == expect_h && incomplete == 0 && nonzero {
            ok += 1;
        }
        if i < 10 || w != expect_w || h != expect_h || incomplete != 0 {
            println!("    frame {i} meta: {w}x{h} incomplete={incomplete} nonzero={nonzero} (expect {expect_w}x{expect_h})");
        }
        assert_eq!(DSQueueBuffer(ds, nd.buffer_handle), GC_ERR_SUCCESS);
    }
    w32(port, acq_off, 0);
    assert_eq!(DSStopAcquisition(ds, 0), GC_ERR_SUCCESS);
    assert_eq!(EventKill(ev), GC_ERR_SUCCESS);
    assert_eq!(GCUnregisterEvent(ds, 1), GC_ERR_SUCCESS);
    assert_eq!(DSFlushQueue(ds, 4), GC_ERR_SUCCESS);
    for h in &handles {
        assert_eq!(DSRevokeBuffer(ds, *h, ptr::null_mut(), ptr::null_mut()), GC_ERR_SUCCESS);
    }
    assert_eq!(DSClose(ds), GC_ERR_SUCCESS);
    assert_eq!(ok, frames, "frames matching {expect_w}x{expect_h}: {ok}/{frames}");
    println!("    ✓ {ok}/{frames} frames at {expect_w}x{expect_h}");
}

fn main() {
    let (tl, iface, dev, port, dev_id) = open_device();
    println!("device: {dev_id}");
    let xml = fetch_xml(port);
    println!("xml: {} bytes", xml.len());

    // Resolve register addresses from the camera XML.
    let regs: Vec<Reg> = ["Width_Reg", "Height_Reg", "OffsetX_Reg", "OffsetY_Reg"]
        .iter()
        .map(|name| {
            let addr = resolve_addr(&xml, name, 0).unwrap_or_else(|| panic!("{name} in XML"));
            Reg { addr }
        })
        .collect();
    let exposure = resolve_addr(&xml, "ExposureTime_Reg", 0).unwrap_or_else(|| {
        // Float nodes may chain through a FloatReg with pAddress.
        resolve_addr(&xml, "ExposureTimeAbs_Reg", 0).expect("exposure register")
    });
    let payload_size_addr = resolve_addr(&xml, "PayloadSize_Reg", 0).expect("PayloadSize reg");
    let acq_on = resolve_addr(&xml, "AcquisitionStart_Reg", 0).expect("acq start");
    let acq_off = resolve_addr(&xml, "AcquisitionStop_Reg", 0).expect("acq stop");
    unsafe {
        ACQ_START = acq_on;
        ACQ_STOP = acq_off;
    }

    // -- Crash recovery: previous interrupted runs may have left a small
    // ROI; reset to the device maximum first.
    fn node_bounds(xml: &str, name: &str) -> (u32, u32, u32) {
        let block = node_block(xml, "</Integer>", name).unwrap_or("");
        let num = |v: &str| {
            u32::from_str_radix(v.trim_start_matches("0x"), 16)
                .or_else(|_| v.parse::<f64>().map(|x| x as u32))
                .ok()
        };
        // Limits may be literals or indirect (<pMin>X</pMin> -> node X).
        let t = |tag: &str, ptag: &str| {
            tag_text(block, tag)
                .and_then(|v| num(&v))
                .or_else(|| tag_text(block, ptag).and_then(|ref_name| {
                    node_block(xml, "</IntSwissKnife>", &ref_name)
                        .or_else(|| node_block(xml, "</Integer>", &ref_name))
                        .and_then(|b| tag_text(b, "Value").and_then(|v| num(&v)))
                }))
        };
        (t("Min", "pMin").unwrap_or(0), t("Max", "pMax").unwrap_or(0), t("Inc", "pInc").unwrap_or(1))
    }
    // Sensor maximum lives in WidthMax_Reg/HeightMax_Reg (plain registers).
    let wmax = resolve_addr(&xml, "WidthMax_Reg", 0).map(|a| r32(port, a)).unwrap_or(0);
    let hmax = resolve_addr(&xml, "HeightMax_Reg", 0).map(|a| r32(port, a)).unwrap_or(0);
    let cur_w = r32(port, regs[0].addr);
    let cur_h = r32(port, regs[1].addr);
    if wmax > 0 && hmax > 0 && (cur_w < wmax || cur_h < hmax) {
        println!("crash recovery: restoring {wmax}x{hmax}");
        w32(port, regs[2].addr, 0);
        w32(port, regs[3].addr, 0);
        w32(port, regs[0].addr, wmax);
        w32(port, regs[1].addr, hmax);
    }

    // -- Read current configuration -------------------------------------
    let orig: Vec<u32> = regs.iter().map(|r| r32(port, r.addr)).collect();
    let orig_exposure = r32(port, exposure);
    let orig_payload = r32(port, payload_size_addr);
    println!("orig: W={} H={} X={} Y={} exposure={} payload={orig_payload}",
        orig[0], orig[1], orig[2], orig[3], orig_exposure);

    // -- Baseline stream at original geometry ----------------------------
    println!("[1] baseline stream at {}x{}:", orig[0], orig[1]);
    grab_and_verify(dev, port, orig[0] as usize, orig[1] as usize, 30);

    // -- ROI windowing -----------------------------------------------------
    let (_, _, winc) = node_bounds(&xml, "Width");
    let (_, _, hinc) = node_bounds(&xml, "Height");
    let winc = winc.max(1);
    let hinc = hinc.max(1);
    let (rw, rh) = match std::env::var("CFG_ROI") {
        Ok(v) => {
            let mut it = v.split('x');
            (it.next().unwrap().parse().unwrap(), it.next().unwrap().parse().unwrap())
        }
        Err(_) => (orig[0] / 2, orig[1] / 2),
    };
    let (nw, nh) = (rw / winc * winc, rh / hinc * hinc);
    let (nx, ny) = (40u32, 24u32);
    println!("[2] set ROI {nw}x{nh} @ ({nx},{ny}) (inc {winc}x{hinc}):");
    // Order: size first, then offsets (writing size after offsets resets
    // them on this device — observed 2026-10-14).
    w32(port, regs[0].addr, nw);
    w32(port, regs[1].addr, nh);
    w32(port, regs[2].addr, nx);
    w32(port, regs[3].addr, ny);
    // The device may clamp to its own granularity: readback is the truth.
    let rb: Vec<u32> = regs.iter().map(|r| r32(port, r.addr)).collect();
    println!("    readback: W={} H={} X={} Y={}", rb[0], rb[1], rb[2], rb[3]);
    assert_eq!(rb[2], nx, "offset X readback");
    assert_eq!(rb[3], ny, "offset Y readback");
    assert!(rb[0] > 0 && rb[0] <= orig[0] && rb[1] > 0 && rb[1] <= orig[1], "size clamp sane");
    let (nw, nh) = (rb[0], rb[1]);
    let payload_after_roi = r32(port, payload_size_addr);
    println!("    payload {orig_payload} -> {payload_after_roi}");
    assert_eq!(payload_after_roi, nw * nh, "PayloadSize should follow ROI");

    println!("[3] stream at windowed ROI:");
    grab_and_verify(dev, port, nw as usize, nh as usize, 50);

    // -- Exposure ----------------------------------------------------------
    let new_exposure = if orig_exposure > 20_000 { orig_exposure / 2 } else { orig_exposure * 2 };
    println!("[4] exposure {orig_exposure} -> {new_exposure}:");
    w32(port, exposure, new_exposure);
    assert_eq!(r32(port, exposure), new_exposure, "exposure readback");

    // -- Restore -----------------------------------------------------------
    println!("[5] restore original config:");
    // Offsets to zero first: a size write with a nonzero offset is
    // clamped by the device (max shrinks by the offset — observed
    // 2026-10-14), and a size write resets the offsets anyway.
    w32(port, regs[2].addr, 0);
    w32(port, regs[3].addr, 0);
    w32(port, regs[0].addr, orig[0]);
    w32(port, regs[1].addr, orig[1]);
    w32(port, exposure, orig_exposure);
    let rb: Vec<u32> = regs.iter().map(|r| r32(port, r.addr)).collect();
    assert_eq!(rb, orig, "restore readback mismatch: {rb:?}");
    assert_eq!(r32(port, payload_size_addr), orig_payload, "payload restore");
    grab_and_verify(dev, port, orig[0] as usize, orig[1] as usize, 30);

    assert_eq!(DevClose(dev), GC_ERR_SUCCESS);
    assert_eq!(IFClose(iface), GC_ERR_SUCCESS);
    assert_eq!(TLClose(tl), GC_ERR_SUCCESS);
    println!("CONFIGURE OK: ROI windowing + exposure + streaming all verified");
}
