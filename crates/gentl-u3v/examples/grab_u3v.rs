//! U3V real-camera grab: full streaming flow (verification handbook U6).
//!   cargo run --example grab_u3v -- [device-id] [n_frames]
//!
//! Register addresses (AcquisitionStart, PayloadSize) are resolved from
//! THIS camera's GenICam XML (Command/Integer nodes -> pValue -> IntReg
//! address), exactly like a GenApi consumer would — nothing is hardcoded.

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

/// Find a node's element block (from `Name="X"` to its closing tag). The
/// node's type is identified by whichever valid closing tag appears FIRST
/// after the name (GenApi XML does not nest register nodes).
fn node_block<'a>(xml: &'a str, _tag_close: &str, name: &str) -> Option<&'a str> {
    const CLOSES: [&str; 4] = ["</IntReg>", "</MaskedIntReg>", "</Integer>", "</IntSwissKnife>"];
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

/// Resolve a register-holding node to its absolute address:
/// direct `<Address>0x..</Address>`, or `<pAddress>REF</pAddress>` ->
/// REF is an Integer constant (`<Value>0x..</Value>`) or another register.
fn resolve_addr(xml: &str, name: &str, depth: u32) -> Option<u64> {
    if depth > 4 {
        return None;
    }
    for close in ["</IntReg>", "</MaskedIntReg>", "</Integer>", "</IntSwissKnife>"] {
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
            if let Some(reference) = tag_text(block, "pValue") {
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

fn main() {
    let mut args = std::env::args().skip(1);
    let wanted = args.next();
    let n_frames: usize = args.next().map(|s| s.parse().unwrap()).unwrap_or(50);

    assert_eq!(GCInitLib(), GC_ERR_SUCCESS);
    let mut tl: TLHandle = ptr::null_mut();
    assert_eq!(TLOpen(&mut tl), GC_ERR_SUCCESS);
    // The camera sits on one specific USB root: probe every interface.
    let mut n_ifaces = 0u32;
    assert_eq!(TLGetNumInterfaces(tl, &mut n_ifaces), GC_ERR_SUCCESS);
    let mut iface: IFHandle = ptr::null_mut();
    let mut nd = 0u32;
    for i in 0..n_ifaces {
        let id = get_string(|s, sz| TLGetInterfaceID(tl, i, s, sz));
        let c = std::ffi::CString::new(id.clone()).unwrap();
        if TLOpenInterface(tl, c.as_ptr(), &mut iface) != GC_ERR_SUCCESS {
            continue;
        }
        let mut changed = 0u8;
        assert_eq!(IFUpdateDeviceList(iface, &mut changed, 1000), GC_ERR_SUCCESS);
        assert_eq!(IFGetNumDevices(iface, &mut nd), GC_ERR_SUCCESS);
        if nd >= 1 {
            println!("camera on interface[{i}] {id}");
            break;
        }
        assert_eq!(IFClose(iface), GC_ERR_SUCCESS);
        iface = ptr::null_mut();
    }
    assert!(nd >= 1 && !iface.is_null(), "no U3V camera");
    let dev_id = wanted.unwrap_or_else(|| get_string(|s, sz| IFGetDeviceID(iface, 0, s, sz)));
    println!("opening {dev_id}");
    let c_dev = std::ffi::CString::new(dev_id).unwrap();
    let mut dev: DevHandle = ptr::null_mut();
    let r = IFOpenDevice(iface, c_dev.as_ptr(), 4, &mut dev);
    assert_eq!(r, GC_ERR_SUCCESS, "IFOpenDevice: {}", last_error());

    let mut port: PortHandle = ptr::null_mut();
    assert_eq!(DevGetPort(dev, &mut port), GC_ERR_SUCCESS);

    // XML via the port (Local: URL -> GenCP reads).
    let url = get_string(|s, sz| GCGetPortURL(port, s, sz));
    println!("xml url: {url}");
    let body = url.split(char::from(0)).next().unwrap_or("");
    let body = body.trim_start_matches("Local:").trim_start_matches("local:");
    let mut parts = body.split(';');
    let _file = parts.next().unwrap_or("");
    let addr = u64::from_str_radix(parts.next().unwrap_or("0").trim(), 16).expect("url addr");
    let len = u64::from_str_radix(parts.next().unwrap_or("0").trim(), 16).expect("url len") as usize;
    let mut blob = vec![0u8; len];
    assert_eq!(read_port(port, addr, &mut blob), GC_ERR_SUCCESS, "xml read: {}", last_error());
    // Spec §5.5.1.2: the manifest entry can be a ZIP; take the first .xml.
    let xml_bytes: Vec<u8> = if _file.to_ascii_lowercase().ends_with(".zip") || blob.starts_with(b"PK") {
        let reader = std::io::Cursor::new(blob);
        let mut archive = zip::ZipArchive::new(reader).expect("zip archive");
        let mut chosen: Option<Vec<u8>> = None;
        for i in 0..archive.len() {
            let mut e = archive.by_index(i).expect("zip entry");
            if e.name().to_ascii_lowercase().ends_with(".xml") {
                let mut v = Vec::with_capacity(e.size() as usize);
                std::io::Read::read_to_end(&mut e, &mut v).expect("zip inflate");
                chosen = Some(v);
                break;
            }
        }
        chosen.expect("zip without .xml")
    } else {
        blob
    };
    let xml = String::from_utf8_lossy(&xml_bytes).into_owned();
    println!("xml: {} bytes, head: {:?}", xml.len(), &xml[..xml.len().min(50)]);
    std::fs::write("u3v_camera_dump.xml", &xml).ok();

    // Resolve registers from the XML (U3V control channel is little-endian).
    // Accept direct feature nodes and the common `*_Reg` / `*Reg` helper
    // naming conventions, following `<pValue>` references when present.
    let acq_addr = resolve_addr(&xml, "AcquisitionStart_Reg", 0)
        .or_else(|| resolve_addr(&xml, "AcquisitionStartReg", 0))
        .or_else(|| resolve_addr(&xml, "AcquisitionStart", 0))
        .expect("AcquisitionStart reg in XML");
    let acq_stop_addr = resolve_addr(&xml, "AcquisitionStop_Reg", 0)
        .or_else(|| resolve_addr(&xml, "AcquisitionStopReg", 0))
        .or_else(|| resolve_addr(&xml, "AcquisitionStop", 0))
        .expect("AcquisitionStop reg in XML");
    let payload_addr = resolve_addr(&xml, "PayloadSize_Reg", 0)
        .or_else(|| resolve_addr(&xml, "PayloadSizeReg", 0))
        .or_else(|| resolve_addr(&xml, "PayloadSize", 0))
        .expect("PayloadSize reg in XML");
    let mut ps = [0u8; 8];
    assert_eq!(read_port(port, payload_addr, &mut ps), GC_ERR_SUCCESS);
    let payload_size = u64::from_le_bytes(ps) as usize;
    println!("payload size: {payload_size} (acq=0x{acq_addr:x} stop=0x{acq_stop_addr:x})");
    assert!(payload_size > 0 && payload_size < 1 << 30);

    let mut ds: DSHandle = ptr::null_mut();
    let c_sid = std::ffi::CString::new("Stream0").unwrap();
    assert_eq!(DevOpenDataStream(dev, c_sid.as_ptr(), &mut ds), GC_ERR_SUCCESS, "open stream: {}", last_error());
    let mut ev: EventHandle = ptr::null_mut();
    assert_eq!(GCRegisterEvent(ds, 1, &mut ev), GC_ERR_SUCCESS);

    const NUM_BUFFERS: usize = 4;
    let mut buffers: Vec<Vec<u8>> = (0..NUM_BUFFERS).map(|_| vec![0u8; payload_size]).collect();
    let mut handles: Vec<BufferHandle> = Vec::new();
    for buf in buffers.iter_mut() {
        let mut bh: BufferHandle = ptr::null_mut();
        assert_eq!(DSAnnounceBuffer(ds, buf.as_mut_ptr().cast(), buf.len(), ptr::null_mut(), &mut bh), GC_ERR_SUCCESS);
        handles.push(bh);
    }
    for bh in &handles {
        assert_eq!(DSQueueBuffer(ds, *bh), GC_ERR_SUCCESS);
    }

    assert_eq!(DSStartAcquisition(ds, 0, u64::MAX), GC_ERR_SUCCESS, "start: {}", last_error());
    // Arm the camera (SFNC AcquisitionStart) — little-endian on U3V.
    assert_eq!(write_port(port, acq_addr, &1u32.to_le_bytes()), GC_ERR_SUCCESS, "AcquisitionStart: {}", last_error());
    println!("acquisition started, waiting for {n_frames} frames...");

    let mut dtype: InfoDataType = 0;
    let mut ok = 0usize;
    for i in 0..n_frames {
        let mut raw = [0u8; 16];
        let mut n = raw.len();
        let r = EventGetData(ev, raw.as_mut_ptr().cast(), &mut n, 10_000);
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

        let pos = handles.iter().position(|x| *x == nd2.buffer_handle).unwrap();
        let nonzero = buffers[pos].iter().any(|&b| b != 0);
        if incomplete == 0 && nonzero {
            ok += 1;
        }
        if i < 5 || i == n_frames - 1 {
            println!("frame {i}: {w}x{h} pf=0x{pf:08x} incomplete={incomplete} nonzero={nonzero}");
        }
        assert_eq!(DSQueueBuffer(ds, nd2.buffer_handle), GC_ERR_SUCCESS);
    }
    println!("frames complete & non-zero: {ok}/{n_frames}");

    let _ = write_port(port, acq_stop_addr, &0u32.to_le_bytes());
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
