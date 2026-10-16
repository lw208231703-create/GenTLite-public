//! Streaming protocol negotiation end-to-end (spec §36.27/§36.38): a
//! GVRSP-only device is rejected at DevOpenDevice with a precise error, a
//! dual-mode device keeps streaming on GVSP with the device left in its
//! factory-default mode.
//!
//! One test function on purpose: the producer's loopback hooks are
//! process-wide environment variables, so the two scenarios must not run
//! concurrently.

use std::ffi::{c_char, CString};
use std::net::Ipv4Addr;
use std::ptr;

use gentl_core::types::*;
use gentl_gige::*;
use gentl_gvcp::registers as reg;
use gentl_gvcp::GvcpServer;
use gentl_simulator::{SimCamera, SimConfig, StreamingProtocol};

const MAC: [u8; 6] = [0x00, 0x11, 0x22, 0x33, 0x44, 0x55];

fn get_string(mut f: impl FnMut(*mut c_char, *mut usize) -> GCError) -> (GCError, String) {
    let mut buf = [0u8; 1024];
    let mut size = buf.len();
    let r = f(buf.as_mut_ptr() as *mut c_char, &mut size);
    let s = if r == GC_ERR_SUCCESS {
        String::from_utf8_lossy(&buf[..size.saturating_sub(1)]).into_owned()
    } else {
        String::new()
    };
    (r, s)
}

fn read_port(h: PortHandle, addr: u64, buf: &mut [u8]) -> GCError {
    let mut size = buf.len();
    GCReadPort(h, addr, buf.as_mut_ptr().cast(), &mut size)
}

fn sim(streaming: StreamingProtocol) -> SimCamera {
    SimCamera::new(SimConfig {
        mac: MAC,
        ip: Ipv4Addr::LOCALHOST,
        subnet_mask: Ipv4Addr::new(255, 0, 0, 0),
        gateway: Ipv4Addr::UNSPECIFIED,
        manufacturer_name: "GenTLite".into(),
        model_name: "SimCam".into(),
        device_version: "1.0.0".into(),
        serial_number: "SIM-0001".into(),
        streaming,
    })
}

/// Serve `cam`, aim the producer's loopback hooks at it, open the first
/// interface and refresh the device list. Returns None (after cleanup)
/// when the machine enumerates no interfaces.
fn serve_and_open(
    cam: SimCamera,
) -> Option<(GvcpServer, TLHandle, IFHandle, String)> {
    let server = cam.start(0).expect("start sim camera");
    let port = server.local_port().expect("server port");
    std::env::set_var("GENTLITE_GVCP_PORT", port.to_string());
    std::env::set_var("GENTLITE_DISCOVERY_TARGETS", format!("127.0.0.1:{port}"));

    assert_eq!(GCInitLib(), GC_ERR_SUCCESS);
    let mut tl: TLHandle = ptr::null_mut();
    assert_eq!(TLOpen(&mut tl), GC_ERR_SUCCESS);
    let mut n = 0u32;
    assert_eq!(TLGetNumInterfaces(tl, &mut n), GC_ERR_SUCCESS);
    if n == 0 {
        eprintln!("no network adapters enumerated; skipping");
        assert_eq!(TLClose(tl), GC_ERR_SUCCESS);
        server.stop();
        return None;
    }
    let (r, iface_id) = get_string(|s, sz| TLGetInterfaceID(tl, 0, s, sz));
    assert_eq!(r, GC_ERR_SUCCESS);
    let mut iface: IFHandle = ptr::null_mut();
    let c_id = CString::new(iface_id.clone()).unwrap();
    assert_eq!(TLOpenInterface(tl, c_id.as_ptr(), &mut iface), GC_ERR_SUCCESS);
    let mut changed = 0u8;
    assert_eq!(IFUpdateDeviceList(iface, &mut changed, 200), GC_ERR_SUCCESS);
    let mut nd = 0u32;
    assert_eq!(IFGetNumDevices(iface, &mut nd), GC_ERR_SUCCESS);
    assert!(nd >= 1, "discovery found no device");
    let (r, dev_id) = get_string(|s, sz| IFGetDeviceID(iface, 0, s, sz));
    assert_eq!(r, GC_ERR_SUCCESS);
    Some((server, tl, iface, dev_id))
}

#[test]
fn streaming_protocol_negotiation() {
    // Scenario 1: GVRSP-only device -> precise rejection at open time.
    let Some((server, tl, iface, dev_id)) = serve_and_open(sim(StreamingProtocol::GvrspOnly))
    else {
        return;
    };
    let c = CString::new(dev_id).unwrap();
    let mut dev: DevHandle = ptr::null_mut();
    assert_eq!(
        IFOpenDevice(iface, c.as_ptr(), 3 /* DEVICE_ACCESS_CONTROL */, &mut dev),
        GC_ERR_NOT_IMPLEMENTED,
        "GVRSP-only device: precise rejection at open time"
    );
    assert_eq!(IFClose(iface), GC_ERR_SUCCESS);
    assert_eq!(TLClose(tl), GC_ERR_SUCCESS);
    assert_eq!(GCCloseLib(), GC_ERR_SUCCESS);
    server.stop();

    // Scenario 2: dual-mode device opens on GVSP, device stays in its
    // factory-default mode, and the GVSP stream path works unchanged.
    let Some((server, tl, iface, dev_id)) = serve_and_open(sim(StreamingProtocol::Both))
    else {
        return;
    };
    let c = CString::new(dev_id).unwrap();
    let mut dev: DevHandle = ptr::null_mut();
    assert_eq!(
        IFOpenDevice(iface, c.as_ptr(), 3 /* DEVICE_ACCESS_CONTROL */, &mut dev),
        GC_ERR_SUCCESS,
        "dual-mode device opens on GVSP"
    );
    let mut port: PortHandle = ptr::null_mut();
    assert_eq!(DevGetPort(dev, &mut port), GC_ERR_SUCCESS);
    let mut buf = [0u8; 4];
    assert_eq!(
        read_port(port, u64::from(reg::STREAMING_CONFIGURATION), &mut buf),
        GC_ERR_SUCCESS
    );
    assert_eq!(
        (u32::from_be_bytes(buf) & reg::SCONF_STREAMING_PROTOCOL_MASK)
            >> reg::SCONF_STREAMING_PROTOCOL_SHIFT,
        reg::SCONF_PROTOCOL_GVSP,
        "device left in GVSP mode"
    );
    let mut ds: DSHandle = ptr::null_mut();
    let stream_id = CString::new("Stream0").unwrap();
    assert_eq!(DevOpenDataStream(dev, stream_id.as_ptr(), &mut ds), GC_ERR_SUCCESS);
    assert_eq!(DSStartAcquisition(ds, 0 /* ACQ_START_FLAGS_DEFAULT */, 1), GC_ERR_SUCCESS);
    assert_eq!(DSStopAcquisition(ds, 0 /* ACQ_STOP_FLAGS_DEFAULT */), GC_ERR_SUCCESS);
    assert_eq!(IFClose(iface), GC_ERR_SUCCESS);
    assert_eq!(TLClose(tl), GC_ERR_SUCCESS);
    assert_eq!(GCCloseLib(), GC_ERR_SUCCESS);
    server.stop();
}
