//! GenTL parent-close cascade and zero-stream honesty (GenTL 1.6
//! §6.1.3/§6.3.2.1/§6.3.3.1/§6.3.4.1/§6.3.4.4).

#![allow(non_snake_case)]

use std::ffi::{c_char, CString};
use std::net::Ipv4Addr;
use std::ptr;
use std::time::{Duration, Instant};

use gentl_core::types::*;
use gentl_gige::*;
use gentl_simulator::{SimCamera, SimConfig, StreamConfig, StreamingProtocol};

const MAC: [u8; 6] = [0x00, 0x11, 0x22, 0x33, 0x44, 0x55];
const DEVICE_ID: &str = "00:11:22:33:44:55";

fn sim(channels: u16) -> SimCamera {
    SimCamera::with_stream(
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
            channels,
            legacy_headers: false,
            chunk: None,
        },
    )
}

fn get_string(mut f: impl FnMut(*mut c_char, *mut usize) -> GCError) -> String {
    let mut buf = [0u8; 512];
    let mut size = buf.len();
    assert_eq!(
        f(buf.as_mut_ptr() as *mut c_char, &mut size),
        GC_ERR_SUCCESS
    );
    String::from_utf8_lossy(&buf[..size.saturating_sub(1)]).into_owned()
}

fn open_device() -> (TLHandle, IFHandle, DevHandle) {
    let mut tl: TLHandle = ptr::null_mut();
    assert_eq!(TLOpen(&mut tl), GC_ERR_SUCCESS);
    let iface_id = get_string(|s, n| TLGetInterfaceID(tl, 0, s, n));
    let mut iface: IFHandle = ptr::null_mut();
    let c_iface = CString::new(iface_id).unwrap();
    assert_eq!(
        TLOpenInterface(tl, c_iface.as_ptr(), &mut iface),
        GC_ERR_SUCCESS
    );
    let mut changed = 0u8;
    assert_eq!(IFUpdateDeviceList(iface, &mut changed, 200), GC_ERR_SUCCESS);
    let mut dev: DevHandle = ptr::null_mut();
    let c_dev = CString::new(DEVICE_ID).unwrap();
    assert_eq!(
        IFOpenDevice(iface, c_dev.as_ptr(), 4, &mut dev),
        GC_ERR_SUCCESS
    );
    (tl, iface, dev)
}

struct OpenStream {
    ds: DSHandle,
    _buffers: Vec<Vec<u8>>,
}

fn open_stream(dev: DevHandle) -> OpenStream {
    let mut ds: DSHandle = ptr::null_mut();
    let c_sid = CString::new("Stream0").unwrap();
    assert_eq!(
        DevOpenDataStream(dev, c_sid.as_ptr(), &mut ds),
        GC_ERR_SUCCESS
    );
    let mut buffers: Vec<Vec<u8>> = (0..2).map(|_| vec![0u8; 64 * 48 + 64]).collect();
    for buf in buffers.iter_mut() {
        let mut bh: BufferHandle = ptr::null_mut();
        assert_eq!(
            DSAnnounceBuffer(ds, buf.as_mut_ptr().cast(), buf.len(), ptr::null_mut(), &mut bh),
            GC_ERR_SUCCESS
        );
        assert_eq!(DSQueueBuffer(ds, bh), GC_ERR_SUCCESS);
    }
    OpenStream {
        ds,
        _buffers: buffers,
    }
}

fn start_and_wait(ds: DSHandle) {
    assert_eq!(DSStartAcquisition(ds, 0, u64::MAX), GC_ERR_SUCCESS);
    let deadline = Instant::now() + Duration::from_secs(3);
    while active_receiver_threads() == 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(active_receiver_threads() >= 1, "receiver running");
}

fn wait_threads_zero() {
    let deadline = Instant::now() + Duration::from_secs(3);
    while active_receiver_threads() != 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(active_receiver_threads(), 0);
}

fn camera_scp(port: u16) -> u32 {
    gentl_gvcp::GvcpClient::connect(Ipv4Addr::LOCALHOST, port)
        .unwrap()
        .read_reg(gentl_gvcp::registers::SCP0)
        .expect("camera reachable")
}

#[test]
fn parent_close_cascades_and_zero_channels_are_honest() {
    // -- 1. A device with zero stream channels must report zero streams. ---
    let zero = sim(0);
    let zero_server = zero.start(0).expect("zero-channel camera");
    let zero_port = zero_server.local_port().unwrap();
    std::env::set_var("GENTLITE_GVCP_PORT", zero_port.to_string());
    std::env::set_var(
        "GENTLITE_DISCOVERY_TARGETS",
        format!("127.0.0.1:{zero_port}"),
    );
    assert_eq!(GCInitLib(), GC_ERR_SUCCESS);
    let (tl, iface, dev) = open_device();
    let mut count = 1u32;
    assert_eq!(DevGetNumDataStreams(dev, &mut count), GC_ERR_SUCCESS);
    assert_eq!(count, 0, "zero physical stream channels -> zero streams");
    let mut buf = [0u8; 64];
    let mut size = buf.len();
    assert_eq!(
        DevGetDataStreamID(dev, 0, buf.as_mut_ptr().cast(), &mut size),
        GC_ERR_NOT_IMPLEMENTED
    );
    let c_sid = CString::new("Stream0").unwrap();
    let mut ds: DSHandle = ptr::null_mut();
    assert_eq!(
        DevOpenDataStream(dev, c_sid.as_ptr(), &mut ds),
        GC_ERR_NOT_IMPLEMENTED
    );
    assert_eq!(DevClose(dev), GC_ERR_SUCCESS);
    assert_eq!(IFClose(iface), GC_ERR_SUCCESS);
    assert_eq!(TLClose(tl), GC_ERR_SUCCESS);
    assert_eq!(GCCloseLib(), GC_ERR_SUCCESS);
    zero_server.stop();

    // -- 2. IFClose must cascade to Device + DataStream. -----------------
    let cam = sim(1);
    let server = cam.start(0).expect("start sim camera");
    let port = server.local_port().unwrap();
    std::env::set_var("GENTLITE_GVCP_PORT", port.to_string());
    std::env::set_var("GENTLITE_DISCOVERY_TARGETS", format!("127.0.0.1:{port}"));
    assert_eq!(GCInitLib(), GC_ERR_SUCCESS);
    let (tl, iface, dev) = open_device();
    let stream = open_stream(dev);
    start_and_wait(stream.ds);
    assert_eq!(IFClose(iface), GC_ERR_SUCCESS);
    wait_threads_zero();
    let mut grabbing = 0u8;
    let mut dtype: InfoDataType = 0;
    let mut size = std::mem::size_of::<u8>();
    assert_eq!(
        DSGetInfo(
            stream.ds,
            8,
            &mut dtype,
            (&mut grabbing as *mut u8).cast(),
            &mut size
        ),
        GC_ERR_INVALID_HANDLE,
        "the cascade closed the child DataStream"
    );
    assert_eq!(DSClose(stream.ds), GC_ERR_INVALID_HANDLE);
    assert_eq!(DevClose(dev), GC_ERR_INVALID_HANDLE);
    assert_eq!(camera_scp(port), 0, "the cascade closed SCP0");
    assert_eq!(TLClose(tl), GC_ERR_SUCCESS);
    assert_eq!(GCCloseLib(), GC_ERR_SUCCESS);

    // -- 3. TLClose must cascade to Interface + Device + DataStream. -----
    assert_eq!(GCInitLib(), GC_ERR_SUCCESS);
    let (tl, iface, dev) = open_device();
    let stream = open_stream(dev);
    start_and_wait(stream.ds);
    assert_eq!(TLClose(tl), GC_ERR_SUCCESS);
    wait_threads_zero();
    assert_eq!(DSClose(stream.ds), GC_ERR_INVALID_HANDLE);
    assert_eq!(DevClose(dev), GC_ERR_INVALID_HANDLE);
    assert_eq!(IFClose(iface), GC_ERR_INVALID_HANDLE);
    assert_eq!(camera_scp(port), 0, "TLClose cascade closed SCP0");
    assert_eq!(GCCloseLib(), GC_ERR_SUCCESS);
    server.stop();
}
