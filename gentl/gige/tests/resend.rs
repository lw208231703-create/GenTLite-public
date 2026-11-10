//! M3-1 end-to-end: GVSP packet-loss recovery (§16.7 PACKETRESEND)
//! through the full GenTL stack. The sim drops two payload packets of
//! block 1 (packet ids 3 and 7, in a frame of 8); without resends that
//! frame would be undeliverable, so every pattern-checked frame — block
//! 1 first of all — proves the resend loop: gap detection, request,
//! retransmission, deduplication, and completion.
//!
//! Default environment on purpose: resend is on unless
//! `GENTLITE_GEV_RESEND=0`, and the sim declares the PACKETRESEND
//! capability (R-197ca). One test per binary keeps the process-global
//! env race-free.

#![allow(non_snake_case)]

use std::collections::BTreeSet;
use std::ffi::{c_char, c_void};
use std::net::Ipv4Addr;
use std::ptr;
use std::sync::{Arc, Mutex};

use gentl_core::types::*;
use gentl_gige::*;
use gentl_simulator::{SimCamera, SimConfig, StreamConfig, StreamingProtocol};

const MAC: [u8; 6] = [0x00, 0x11, 0x22, 0x33, 0x44, 0x88];
const DEVICE_ID: &str = "00:11:22:33:44:88";
const FRAMES_TO_CHECK: usize = 10;
const NUM_BUFFERS: usize = 4;
const W: u32 = 800;
const H: u32 = 600;
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

/// The sim's deterministic Mono8 pattern: pixel `p` of block `b` holds
/// `(p + b) mod 256`. Byte-exact across the dropped/recovered packets.
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
fn packet_resend_recovers_dropped_payloads_end_to_end() {
    let cam = SimCamera::with_stream(
        SimConfig {
            mac: MAC,
            ip: Ipv4Addr::LOCALHOST,
            subnet_mask: Ipv4Addr::new(255, 0, 0, 0),
            gateway: Ipv4Addr::UNSPECIFIED,
            manufacturer_name: "GenTLite".into(),
            model_name: "SimCam".into(),
            device_version: "1.0.0".into(),
            serial_number: "SIM-0004".into(),
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
    // Loss model (§16.7 regression hook): two packets of block 1. At the
    // negotiated jumbo size the frame spans 8 payload packets, so ids 3
    // and 7 are real packet ids; both leader and trailer still get
    // through, so block 1 only completes via resends.
    let drops = Arc::new(Mutex::new(BTreeSet::new()));
    drops.lock().unwrap().insert((1u64, 3u32));
    drops.lock().unwrap().insert((1u64, 7u32));
    cam.set_packet_drop(drops);

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

    // -- Acquisition ---------------------------------------------------------
    assert_eq!(
        DSStartAcquisition(ds, 0, u64::MAX /* GENTL_INFINITE */),
        GC_ERR_SUCCESS
    );

    // -- Frames: block 1 (two injected losses) must arrive complete --------
    let mut first_block_id = None;
    for checked in 0..FRAMES_TO_CHECK {
        let mut raw = [0u8; 16];
        let mut n = raw.len();
        assert_eq!(
            EventGetData(ev, raw.as_mut_ptr().cast(), &mut n, 20_000),
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
        if checked == 0 {
            first_block_id = Some(block_id);
        }
        let mut filled = 0usize;
        assert_eq!(
            DSGetBufferInfo(ds, nd.buffer_handle, 9, &mut dtype, (&mut filled as *mut usize).cast(), &mut size),
            GC_ERR_SUCCESS
        );
        assert_eq!(filled, payload_size, "BUFFER_INFO_SIZE_FILLED, frame {checked}");
        let mut incomplete = 0u8;
        size = 1;
        assert_eq!(
            DSGetBufferInfo(ds, nd.buffer_handle, 7, &mut dtype, (&mut incomplete as *mut u8).cast(), &mut size),
            GC_ERR_SUCCESS
        );
        assert_eq!(incomplete, 0, "frame {checked} complete — no resend leftovers");

        // Byte-exact payload, including the recovered packets of block 1.
        check_payload(&buffers[pos], block_id);
        assert_eq!(DSQueueBuffer(ds, nd.buffer_handle), GC_ERR_SUCCESS);
    }
    // Block 1 is among the delivered frames: its two dropped packets were
    // recovered through §16.7 requests. (Without resends the first event
    // would already have timed out.)
    let first = first_block_id.expect("first frame");
    assert!(
        (1..=(FRAMES_TO_CHECK as u64)).contains(&first),
        "first delivered block {first} sits inside the first {FRAMES_TO_CHECK} blocks"
    );

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
