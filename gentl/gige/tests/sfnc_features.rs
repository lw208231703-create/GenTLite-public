//! SFNC feature semantics over the real producer path: the simulator's
//! image generator honours TestPattern and ReverseX/ReverseY (the generator
//! configuration is snapshotted when the stream channel opens, like SCPSx —
//! GigE Vision 3.0 §36.58), and UserSetSave/UserSetLoad really snapshot and
//! restore the feature register area (SFNC UserSetControl).
//!
//! These tests write child features through a GenTL port (the producer's
//! GenApi path: GCWritePort → GVCP WRITEREG) and verify the emitted pixels,
//! so a regression in the producer's write path or in the simulator's
//! feature semantics fails here.

#![allow(non_snake_case)]

use std::ffi::{c_char, c_void};
use std::net::Ipv4Addr;
use std::ptr;

use gentl_core::types::*;
use gentl_gige::*;
use gentl_simulator::{
    SimCamera, SimConfig, StreamConfig, StreamingProtocol, REG_HEIGHT, REG_REVERSE_X,
    REG_TEST_PATTERN, REG_USER_SET_LOAD, REG_USER_SET_SAVE, REG_USER_SET_SELECTOR, REG_WIDTH,
};

const MAC: [u8; 6] = [0x00, 0x11, 0x22, 0x33, 0x44, 0x55];
const DEVICE_ID: &str = "00:11:22:33:44:55";
const W: u32 = 4;
const H: u32 = 2;
const PFNC_MONO8: u32 = 0x0108_0001;

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

fn read_reg(h: PortHandle, addr: u32) -> u32 {
    let mut buf = [0u8; 4];
    let mut size = buf.len();
    assert_eq!(
        GCReadPort(h, u64::from(addr), buf.as_mut_ptr().cast(), &mut size),
        GC_ERR_SUCCESS,
        "GCReadPort 0x{addr:04X}"
    );
    u32::from_be_bytes(buf)
}

fn write_reg(h: PortHandle, addr: u32, value: u32) {
    let buf = value.to_be_bytes();
    let mut size = buf.len();
    assert_eq!(
        GCWritePort(h, u64::from(addr), buf.as_ptr().cast(), &mut size),
        GC_ERR_SUCCESS,
        "GCWritePort 0x{addr:04X}"
    );
}

#[test]
fn test_pattern_reverse_and_user_sets_are_real_features() {
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
            width: W,
            height: H,
            pixel_format: PFNC_MONO8,
            channels: 1,
            legacy_headers: false,
            chunk: None,
        },
    );
    let server = cam.start(0).expect("start sim camera");
    let port_num = server.local_port().expect("server port");
    std::env::set_var("GENTLITE_GVCP_PORT", port_num.to_string());
    std::env::set_var(
        "GENTLITE_DISCOVERY_TARGETS",
        format!("127.0.0.1:{port_num}"),
    );

    assert_eq!(GCInitLib(), GC_ERR_SUCCESS);
    let mut tl: TLHandle = ptr::null_mut();
    assert_eq!(TLOpen(&mut tl), GC_ERR_SUCCESS);
    let (r, iface_id) = get_string(|s, n| TLGetInterfaceID(tl, 0, s, n));
    assert_eq!(r, GC_ERR_SUCCESS);
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
        IFOpenDevice(iface, c_dev.as_ptr(), 4 /* exclusive */, &mut dev),
        GC_ERR_SUCCESS
    );
    let mut port: PortHandle = ptr::null_mut();
    assert_eq!(DevGetPort(dev, &mut port), GC_ERR_SUCCESS);
    let mut ds: DSHandle = ptr::null_mut();
    let c_sid = std::ffi::CString::new("Stream0").unwrap();
    assert_eq!(
        DevOpenDataStream(dev, c_sid.as_ptr(), &mut ds),
        GC_ERR_SUCCESS
    );
    let mut ev: EventHandle = ptr::null_mut();
    assert_eq!(
        GCRegisterEvent(ds, 1 /* EVENT_NEW_BUFFER */, &mut ev),
        GC_ERR_SUCCESS
    );

    // Two small buffers (4x2 Mono8 = 8 bytes) plus one frame per phase.
    let mut buffers: Vec<Vec<u8>> = (0..2).map(|_| vec![0u8; (W * H) as usize]).collect();
    let mut handles: Vec<BufferHandle> = Vec::new();
    for (i, buf) in buffers.iter_mut().enumerate() {
        let mut bh: BufferHandle = ptr::null_mut();
        assert_eq!(
            DSAnnounceBuffer(
                ds,
                buf.as_mut_ptr().cast(),
                buf.len(),
                (0x2000 + i) as *mut c_void,
                &mut bh
            ),
            GC_ERR_SUCCESS
        );
        assert_eq!(DSQueueBuffer(ds, bh), GC_ERR_SUCCESS);
        handles.push(bh);
    }

    // Fetch one delivered frame and return (payload bytes, handle).
    let recv_frame = |expected: &[u8]| {
        let mut raw = [0u8; 16];
        let mut n = raw.len();
        assert_eq!(
            EventGetData(ev, raw.as_mut_ptr().cast(), &mut n, 5_000),
            GC_ERR_SUCCESS,
            "frame event within the timeout"
        );
        let nd = unsafe { (raw.as_ptr() as *const EventNewBufferData).read_unaligned() };
        let pos = handles
            .iter()
            .position(|h| *h == nd.buffer_handle)
            .expect("event handle is one of ours");
        assert_eq!(
            &buffers[pos][..expected.len()],
            expected,
            "generated pixels"
        );
        assert_eq!(DSQueueBuffer(ds, nd.buffer_handle), GC_ERR_SUCCESS);
    };

    // -- Phase 1: TestPattern = GreyHorizontalRamp -------------------------
    // The generator snapshot happens at channel open, so the feature is
    // written while the channel is still closed.
    write_reg(port, REG_TEST_PATTERN, 1);
    assert_eq!(read_reg(port, REG_TEST_PATTERN), 1);
    assert_eq!(DSStartAcquisition(ds, 0, u64::MAX), GC_ERR_SUCCESS);
    recv_frame(&[0, 85, 170, 255, 0, 85, 170, 255]);
    assert_eq!(DSStopAcquisition(ds, 0), GC_ERR_SUCCESS);

    // -- Phase 2: ReverseX mirrors the same ramp ---------------------------
    write_reg(port, REG_REVERSE_X, 1);
    assert_eq!(read_reg(port, REG_REVERSE_X), 1);
    assert_eq!(DSStartAcquisition(ds, 0, u64::MAX), GC_ERR_SUCCESS);
    recv_frame(&[255, 170, 85, 0, 255, 170, 85, 0]);
    assert_eq!(DSStopAcquisition(ds, 0), GC_ERR_SUCCESS);

    // -- UserSetControl: save/load really restore the feature area ----------
    write_reg(port, REG_WIDTH, 320);
    write_reg(port, REG_HEIGHT, 240);
    write_reg(port, REG_USER_SET_SELECTOR, 1);
    write_reg(port, REG_USER_SET_SAVE, 1);
    assert_eq!(read_reg(port, REG_USER_SET_SAVE), 0, "save self-clears");
    write_reg(port, REG_WIDTH, 640);
    write_reg(port, REG_HEIGHT, 480);
    write_reg(port, REG_USER_SET_LOAD, 1);
    assert_eq!(read_reg(port, REG_WIDTH), 320, "UserSet1 restored");
    assert_eq!(read_reg(port, REG_HEIGHT), 240);

    // -- Teardown ----------------------------------------------------------
    assert_eq!(DSStopAcquisition(ds, 0), GC_ERR_RESOURCE_IN_USE);
    assert_eq!(DSClose(ds), GC_ERR_SUCCESS);
    assert_eq!(DevClose(dev), GC_ERR_SUCCESS);
    assert_eq!(IFClose(iface), GC_ERR_SUCCESS);
    assert_eq!(TLClose(tl), GC_ERR_SUCCESS);
    assert_eq!(GCCloseLib(), GC_ERR_SUCCESS);
}
