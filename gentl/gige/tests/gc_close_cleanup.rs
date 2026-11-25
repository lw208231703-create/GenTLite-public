//! GCCloseLib resource cleanup (GenTL 1.6 §6.3.1.1): a library close while
//! a stream is running must stop the receiver threads and release the
//! handles; repeated init/open/close cycles must not accumulate resources.

#![allow(non_snake_case)]

use std::ffi::c_char;
use std::net::Ipv4Addr;
use std::ptr;
use std::time::{Duration, Instant};

use gentl_core::types::*;
use gentl_gige::*;
use gentl_simulator::{SimCamera, SimConfig, StreamConfig, StreamingProtocol};

const MAC: [u8; 6] = [0x00, 0x11, 0x22, 0x33, 0x44, 0x55];
const DEVICE_ID: &str = "00:11:22:33:44:55";

fn get_string(mut f: impl FnMut(*mut c_char, *mut usize) -> GCError) -> String {
    let mut buf = [0u8; 512];
    let mut size = buf.len();
    assert_eq!(
        f(buf.as_mut_ptr() as *mut c_char, &mut size),
        GC_ERR_SUCCESS
    );
    String::from_utf8_lossy(&buf[..size.saturating_sub(1)]).into_owned()
}

#[test]
fn gc_close_lib_stops_open_stream_and_releases_handles() {
    let cam = SimCamera::with_stream(
        SimConfig {
            mac: MAC,
            ip: Ipv4Addr::LOCALHOST,
            subnet_mask: Ipv4Addr::new(255, 0, 0, 0),
            gateway: Ipv4Addr::UNSPECIFIED,
            manufacturer_name: "GenTLite".into(),
            model_name: "SimCam".into(),
            device_version: "1.0.0".into(),
            serial_number: "SIM-0001".into(),
            streaming: StreamingProtocol::GvspOnly,
        },
        StreamConfig {
            width: 64,
            height: 48,
            pixel_format: 0x0108_0001,
            channels: 1,
            legacy_headers: false,
            chunk: None,
        },
    );
    let server = cam.start(0).expect("start sim camera");
    let port = server.local_port().expect("server port");
    std::env::set_var("GENTLITE_GVCP_PORT", port.to_string());
    std::env::set_var("GENTLITE_DISCOVERY_TARGETS", format!("127.0.0.1:{port}"));

    assert_eq!(GCInitLib(), GC_ERR_SUCCESS);
    let mut tl: TLHandle = ptr::null_mut();
    assert_eq!(TLOpen(&mut tl), GC_ERR_SUCCESS);
    let iface_id = get_string(|s, n| TLGetInterfaceID(tl, 0, s, n));
    let mut iface: IFHandle = ptr::null_mut();
    let c_iface = std::ffi::CString::new(iface_id).unwrap();
    assert_eq!(
        TLOpenInterface(tl, c_iface.as_ptr(), &mut iface),
        GC_ERR_SUCCESS
    );
    let mut changed = 0u8;
    assert_eq!(IFUpdateDeviceList(iface, &mut changed, 200), GC_ERR_SUCCESS);
    let mut dev: DevHandle = ptr::null_mut();
    let c_dev = std::ffi::CString::new(DEVICE_ID).unwrap();
    assert_eq!(
        IFOpenDevice(iface, c_dev.as_ptr(), 4, &mut dev),
        GC_ERR_SUCCESS
    );
    let mut ds: DSHandle = ptr::null_mut();
    let c_sid = std::ffi::CString::new("Stream0").unwrap();
    assert_eq!(
        DevOpenDataStream(dev, c_sid.as_ptr(), &mut ds),
        GC_ERR_SUCCESS
    );

    let mut buffers: Vec<Vec<u8>> = (0..2).map(|_| vec![0u8; 64 * 48 + 64]).collect();
    let mut handles: Vec<BufferHandle> = Vec::new();
    for buf in buffers.iter_mut() {
        let mut bh: BufferHandle = ptr::null_mut();
        assert_eq!(
            DSAnnounceBuffer(ds, buf.as_mut_ptr().cast(), buf.len(), ptr::null_mut(), &mut bh),
            GC_ERR_SUCCESS
        );
        handles.push(bh);
        assert_eq!(DSQueueBuffer(ds, bh), GC_ERR_SUCCESS);
    }
    assert_eq!(DSStartAcquisition(ds, 0, u64::MAX), GC_ERR_SUCCESS);
    let deadline = Instant::now() + Duration::from_secs(3);
    while active_receiver_threads() == 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        active_receiver_threads() >= 1,
        "the receiver thread must be running before GCCloseLib"
    );

    // Close the library WITHOUT calling any module Close function.
    assert_eq!(GCCloseLib(), GC_ERR_SUCCESS);
    let deadline = Instant::now() + Duration::from_secs(3);
    while active_receiver_threads() != 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        active_receiver_threads(),
        0,
        "GCCloseLib must stop the open stream's receiver threads"
    );

    // The downstream camera must see the stream channel and control channel
    // closed/cleared even though the consumer never called DSClose/DevClose.
    let client = gentl_gvcp::GvcpClient::connect(Ipv4Addr::LOCALHOST, port).unwrap();
    assert_eq!(
        client
            .read_reg(gentl_gvcp::registers::SCP0)
            .expect("camera reachable"),
        0,
        "GCCloseLib must close the GVSP stream channel (SCP0 = 0)"
    );
    assert_eq!(
        client
            .read_reg(gentl_gvcp::registers::CCP)
            .expect("camera reachable")
            & (gentl_gvcp::registers::CCP_EXCLUSIVE_ACCESS
                | gentl_gvcp::registers::CCP_CONTROL_ACCESS),
        0,
        "GCCloseLib must release the camera control channel (CCP = 0)"
    );

    // Repeated init/open/close cycles must not accumulate resources.
    for _ in 0..3 {
        assert_eq!(GCInitLib(), GC_ERR_SUCCESS);
        let mut tl: TLHandle = ptr::null_mut();
        assert_eq!(TLOpen(&mut tl), GC_ERR_SUCCESS);
        assert_eq!(GCCloseLib(), GC_ERR_SUCCESS);
    }
    assert_eq!(active_receiver_threads(), 0);

    server.stop();
}
