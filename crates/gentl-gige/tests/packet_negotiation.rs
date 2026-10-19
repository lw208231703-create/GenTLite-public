//! M3-2 end-to-end: §11.7 packet-size negotiation against the sim, default
//! mode (no `GENTLITE_GEV_PACKET_SIZE` — automatic test-packet search).
//!
//! The sim's GVSP emitter accepts fire-carrying SCPS0 writes while its
//! auto-stream runs (notes/OPEN-QUESTIONS.md Q33), so the producer's
//! negotiation runs there in full: the sim delivers every test packet on
//! loopback and the search converges to the sim's upper bound. The
//! assertions below are deliberately robust to where exactly it
//! converges (loopback jumbo delivery is QoI): the negotiated SCPS0 must
//! be a plausible post-negotiation value, the frames received afterwards
//! must be complete, and their payload pattern proves the receiver frames
//! with a stride of SCPS0-48 (any mismatch garbles the multi-packet
//! placement and fails the content check / drops the frame).

#![allow(non_snake_case)]

use std::ffi::{c_char, c_void};
use std::net::Ipv4Addr;
use std::ptr;

use gentl_core::types::*;
use gentl_gige::*;
use gentl_gvcp::registers;
use gentl_simulator::{SimCamera, SimConfig, StreamConfig, StreamingProtocol};

const MAC: [u8; 6] = [0x00, 0x11, 0x22, 0x33, 0x44, 0x55];
const DEVICE_ID: &str = "00:11:22:33:44:55";
const FRAMES_TO_CHECK: usize = 10;
const NUM_BUFFERS: usize = 4;
const W: u32 = 320;
const H: u32 = 240;
const PF: u32 = 0x0108_0001; // PFNC_Mono8

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

/// The sim's deterministic Mono8 pattern: pixel `p` of block `b` holds
/// `(p + b) mod 256`. With a frame of W*H bytes spanning multiple GVSP
/// payload packets, this only matches when the receiver placed every
/// packet at `packet_id * (SCPS0 - 48)` — i.e. the negotiated stride.
fn check_payload(buf: &[u8], block_id: u64) {
    let n_pixels = (W * H) as usize;
    assert_eq!(buf.len(), n_pixels);
    for (p, &b) in buf.iter().enumerate() {
        assert_eq!(
            b,
            (p + block_id as usize) as u8,
            "pixel {p} of block {block_id}"
        );
    }
}

#[test]
fn packet_size_negotiation_end_to_end() {
    // Deliberately NOT touching GENTLITE_GEV_PACKET_SIZE / _RCVBUF_KB /
    // _RX_CORES: the default path (auto negotiation, default socket
    // tuning) is what this test exercises, and it stays race-free against
    // the other test binaries because env is per-process.
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
            pixel_format: PF,
            channels: 1,
            legacy_headers: false,
        },
    );
    let server = cam.start(0).expect("start sim camera");
    let port = server.local_port().expect("server port");
    std::env::set_var("GENTLITE_GVCP_PORT", port.to_string());
    std::env::set_var("GENTLITE_DISCOVERY_TARGETS", format!("127.0.0.1:{port}"));

    assert_eq!(GCInitLib(), GC_ERR_SUCCESS);
    let mut tl: TLHandle = ptr::null_mut();
    assert_eq!(TLOpen(&mut tl), GC_ERR_SUCCESS);
    let mut num_ifaces = 0u32;
    assert_eq!(TLGetNumInterfaces(tl, &mut num_ifaces), GC_ERR_SUCCESS);
    assert!(num_ifaces >= 1, "test needs at least one network adapter");
    let (r, iface_id) = get_string(|s, n| TLGetInterfaceID(tl, 0, s, n));
    assert_eq!(r, GC_ERR_SUCCESS);
    let mut iface: IFHandle = ptr::null_mut();
    let c_iface = std::ffi::CString::new(iface_id).unwrap();
    assert_eq!(TLOpenInterface(tl, c_iface.as_ptr(), &mut iface), GC_ERR_SUCCESS);
    let mut changed = 0u8;
    assert_eq!(IFUpdateDeviceList(iface, &mut changed, 200), GC_ERR_SUCCESS);
    let mut dev: DevHandle = ptr::null_mut();
    let c_dev = std::ffi::CString::new(DEVICE_ID).unwrap();
    assert_eq!(
        IFOpenDevice(iface, c_dev.as_ptr(), 4 /* exclusive */, &mut dev),
        GC_ERR_SUCCESS
    );
    let mut num_streams = 0u32;
    assert_eq!(DevGetNumDataStreams(dev, &mut num_streams), GC_ERR_SUCCESS);
    assert_eq!(num_streams, 1);
    let (r, stream_id) = get_string(|s, n| DevGetDataStreamID(dev, 0, s, n));
    assert_eq!(r, GC_ERR_SUCCESS);
    let mut ds: DSHandle = ptr::null_mut();
    let c_sid = std::ffi::CString::new(stream_id.clone()).unwrap();
    assert_eq!(DevOpenDataStream(dev, c_sid.as_ptr(), &mut ds), GC_ERR_SUCCESS);
    let mut port_h: PortHandle = ptr::null_mut();
    assert_eq!(DevGetPort(dev, &mut port_h), GC_ERR_SUCCESS);

    // -- Buffers + event ----------------------------------------------------
    let payload_size = (W * H) as usize;
    let mut buffers: Vec<Vec<u8>> = (0..NUM_BUFFERS).map(|_| vec![0u8; payload_size]).collect();
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
    let mut ev: EventHandle = ptr::null_mut();
    assert_eq!(GCRegisterEvent(ds, 1 /* EVENT_NEW_BUFFER */, &mut ev), GC_ERR_SUCCESS);

    // -- Acquisition: channel open + §11.7 negotiation happen inside ------
    assert_eq!(
        DSStartAcquisition(ds, 0, u64::MAX /* GENTL_INFINITE */),
        GC_ERR_SUCCESS
    );

    // The negotiated SCPS0: the §11.7 search rewrote the register. On
    // loopback every test packet arrives, so this converges to the sim's
    // upper bound; the assertions stay valid for any convergence point.
    let mut scps0 = [0u8; 4];
    assert_eq!(
        read_port(port_h, u64::from(registers::SCPS0), &mut scps0),
        GC_ERR_SUCCESS
    );
    let scps_total = registers::scps_packet_size(u32::from_be_bytes(scps0));
    assert!(
        (1500..=65_535).contains(&scps_total),
        "negotiated SCPS0 total {scps_total} in [1500, 65535] (factory default 1448 would mean no negotiation happened)"
    );
    assert!(
        payload_size > scps_total as usize - registers::GVSP_HEADERS_OVERHEAD as usize,
        "frame ({payload_size} bytes) must exceed one negotiated packet ({} payload bytes) so the frame spans multiple packets and the stride check bites",
        scps_total - registers::GVSP_HEADERS_OVERHEAD
    );

    // -- Frames: complete, and framed with the negotiated stride ----------
    for checked in 0..FRAMES_TO_CHECK {
        let mut raw = [0u8; 16];
        let mut n = raw.len();
        assert_eq!(
            EventGetData(ev, raw.as_mut_ptr().cast(), &mut n, 10_000),
            GC_ERR_SUCCESS,
            "frame event {checked}"
        );
        assert_eq!(n, 16);
        let nd = unsafe { (raw.as_ptr() as *const EventNewBufferData).read_unaligned() };
        let pos = handles
            .iter()
            .position(|h| *h == nd.buffer_handle)
            .expect("event handle is one of ours");

        let mut dtype: InfoDataType = 0;
        let mut size = std::mem::size_of::<u64>();
        let mut block_id = 0u64;
        assert_eq!(
            DSGetBufferInfo(ds, nd.buffer_handle, 16, &mut dtype, (&mut block_id as *mut u64).cast(), &mut size),
            GC_ERR_SUCCESS
        );
        assert!(block_id >= 1, "BUFFER_INFO_FRAMEID: a regular block");
        let mut filled = 0usize;
        assert_eq!(
            DSGetBufferInfo(ds, nd.buffer_handle, 9, &mut dtype, (&mut filled as *mut usize).cast(), &mut size),
            GC_ERR_SUCCESS
        );
        assert_eq!(filled, payload_size, "BUFFER_INFO_SIZE_FILLED");
        let mut incomplete = 0u8;
        assert_eq!(
            DSGetBufferInfo(ds, nd.buffer_handle, 7, &mut dtype, (&mut incomplete as *mut u8).cast(), &mut size),
            GC_ERR_SUCCESS
        );
        assert_eq!(incomplete, 0, "complete frames only");

        // Payload placement is stride-exact: a receiver using any other
        // stride garbles every packet after the first.
        check_payload(&buffers[pos], block_id);
        assert_eq!(DSQueueBuffer(ds, nd.buffer_handle), GC_ERR_SUCCESS);
    }

    // -- Teardown -------------------------------------------------------------
    assert_eq!(DSStopAcquisition(ds, 0), GC_ERR_SUCCESS);
    let mut dtype: InfoDataType = 0;
    let mut size = std::mem::size_of::<u64>();
    let mut delivered = 0u64;
    assert_eq!(
        DSGetInfo(ds, 1 /* STREAM_INFO_NUM_DELIVERED */, &mut dtype, (&mut delivered as *mut u64).cast(), &mut size),
        GC_ERR_SUCCESS
    );
    assert!(
        delivered >= FRAMES_TO_CHECK as u64,
        "delivered {delivered} >= {FRAMES_TO_CHECK}"
    );

    assert_eq!(EventKill(ev), GC_ERR_SUCCESS);
    assert_eq!(GCUnregisterEvent(ds, 1), GC_ERR_SUCCESS);
    assert_eq!(DSFlushQueue(ds, 4 /* ACQ_QUEUE_ALL_DISCARD */), GC_ERR_SUCCESS);
    for (i, bh) in handles.iter().enumerate() {
        let mut base: *mut c_void = ptr::null_mut();
        let mut user: *mut c_void = ptr::null_mut();
        assert_eq!(DSRevokeBuffer(ds, *bh, &mut base, &mut user), GC_ERR_SUCCESS, "revoke buffer {i}");
        assert_eq!(base, buffers[i].as_mut_ptr().cast());
    }
    assert_eq!(DSClose(ds), GC_ERR_SUCCESS);
    assert_eq!(DevClose(dev), GC_ERR_SUCCESS);
    assert_eq!(IFClose(iface), GC_ERR_SUCCESS);
    assert_eq!(TLClose(tl), GC_ERR_SUCCESS);
    server.stop();
}
