//! End-to-end data-stream test: `gentl_simulator::SimCamera` behind the
//! real producer entry points. The sim's GVSP emitter streams fake images
//! as soon as the producer opens the channel (SCDA0/SCP0 written by
//! `DSStartAcquisition`), so the flow below exercises the complete M2
//! receive path over real UDP on loopback — no test double on the wire.
//!
//! The camera uses a non-default Mono16 stream to prove the metadata
//! wiring (Width/Height/PixelFormat registers -> GVSP leader ->
//! BUFFER_INFO_*) and the little-endian 2-byte pixel pattern.

#![allow(non_snake_case)]

use std::ffi::{c_char, c_void};
use std::net::Ipv4Addr;
use std::ptr;

use gentl_core::types::*;
use gentl_gige::*;
use gentl_gvcp::registers;
use gentl_simulator::{SimCamera, SimConfig, StreamConfig, REG_HEIGHT, REG_PIXEL_FORMAT, REG_WIDTH};

const MAC: [u8; 6] = [0x00, 0x11, 0x22, 0x33, 0x44, 0x55];
const DEVICE_ID: &str = "00:11:22:33:44:55";
const FRAMES_TO_CHECK: usize = 10;
const NUM_BUFFERS: usize = 4;
const W: u32 = 320;
const H: u32 = 240;
const PF: u32 = 0x0110_0007; // PFNC_Mono16

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

/// The sim's fake-image pattern for one delivered buffer: pixel `p` of
/// block `b` holds `(p + b) mod 2^16`, little-endian on the wire.
fn check_payload(buf: &[u8], block_id: u64) {
    let n_pixels = (W * H) as usize;
    assert_eq!(buf.len(), n_pixels * 2);
    for (p, chunk) in buf.chunks_exact(2).enumerate() {
        let v = u16::from_le_bytes([chunk[0], chunk[1]]);
        assert_eq!(
            v,
            (p + block_id as usize) as u16,
            "pixel {p} of block {block_id}"
        );
    }
}

#[test]
fn data_stream_end_to_end() {
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
        },
        StreamConfig {
            width: W,
            height: H,
            pixel_format: PF,
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

    // -- Device -> data stream enumeration --------------------------------
    let mut num_streams = 0u32;
    assert_eq!(DevGetNumDataStreams(dev, &mut num_streams), GC_ERR_SUCCESS);
    assert_eq!(num_streams, 1, "the sim exposes one stream channel");
    let (r, stream_id) = get_string(|s, n| DevGetDataStreamID(dev, 0, s, n));
    assert_eq!(r, GC_ERR_SUCCESS);
    assert_eq!(stream_id, "Stream0");

    // A second open of the same stream is RESOURCE_IN_USE.
    let mut ds: DSHandle = ptr::null_mut();
    let c_sid = std::ffi::CString::new(stream_id.clone()).unwrap();
    assert_eq!(DevOpenDataStream(dev, c_sid.as_ptr(), &mut ds), GC_ERR_SUCCESS);
    assert!(!ds.is_null());
    let mut ds2: DSHandle = ptr::null_mut();
    assert_eq!(
        DevOpenDataStream(dev, c_sid.as_ptr(), &mut ds2),
        GC_ERR_RESOURCE_IN_USE
    );
    // Unknown stream ID is INVALID_ID.
    let c_bad = std::ffi::CString::new("Stream7").unwrap();
    assert_eq!(
        DevOpenDataStream(dev, c_bad.as_ptr(), &mut ds2),
        GC_ERR_INVALID_ID
    );

    // -- Stream infos -------------------------------------------------------
    let mut dtype: InfoDataType = 0;
    let (r, v) = get_string(|s, n| DSGetInfo(ds, 10, &mut dtype, s as *mut c_void, n));
    assert_eq!(r, GC_ERR_SUCCESS);
    assert_eq!(v, "GEV");
    let (r, v) = get_string(|s, n| DSGetInfo(ds, 0, &mut dtype, s as *mut c_void, n));
    assert_eq!(r, GC_ERR_SUCCESS);
    assert_eq!(v, "Stream0");
    let mut grabbing = 0u8;
    let mut size = 1;
    assert_eq!(
        DSGetInfo(ds, 8, &mut dtype, (&mut grabbing as *mut u8).cast(), &mut size),
        GC_ERR_SUCCESS
    );
    assert_eq!(dtype, 11);
    assert_eq!(grabbing, 0, "engine not started yet");

    // -- Remote image configuration (from the sim's user registers) --------
    let mut port_h: PortHandle = ptr::null_mut();
    assert_eq!(DevGetPort(dev, &mut port_h), GC_ERR_SUCCESS);
    let mut w = [0u8; 4];
    assert_eq!(read_port(port_h, u64::from(REG_WIDTH), &mut w), GC_ERR_SUCCESS);
    assert_eq!(u32::from_be_bytes(w), W);
    let mut h = [0u8; 4];
    assert_eq!(read_port(port_h, u64::from(REG_HEIGHT), &mut h), GC_ERR_SUCCESS);
    assert_eq!(u32::from_be_bytes(h), H);
    let mut pf = [0u8; 4];
    assert_eq!(read_port(port_h, u64::from(REG_PIXEL_FORMAT), &mut pf), GC_ERR_SUCCESS);
    assert_eq!(u32::from_be_bytes(pf), PF);
    let payload_size = (W * H * 2) as usize;

    // -- Event registration -------------------------------------------------
    let mut ev: EventHandle = ptr::null_mut();
    assert_eq!(GCRegisterEvent(ds, 1 /* EVENT_NEW_BUFFER */, &mut ev), GC_ERR_SUCCESS);
    assert!(!ev.is_null());
    let mut ev2: EventHandle = ptr::null_mut();
    assert_eq!(
        GCRegisterEvent(ds, 1, &mut ev2),
        GC_ERR_RESOURCE_IN_USE,
        "one registration per module and event type"
    );
    assert_eq!(
        GCRegisterEvent(ds, 0 /* EVENT_ERROR */, &mut ev2),
        GC_ERR_NOT_IMPLEMENTED
    );
    let mut event_type = -1i32;
    let mut size = 4;
    assert_eq!(
        EventGetInfo(
            ev,
            0, /* EVENT_EVENT_TYPE */
            &mut dtype,
            (&mut event_type as *mut i32).cast(),
            &mut size
        ),
        GC_ERR_SUCCESS
    );
    assert_eq!(event_type, 1);
    let mut data_size_max = 0usize;
    let mut size = std::mem::size_of::<usize>();
    assert_eq!(
        EventGetInfo(
            ev,
            3, /* EVENT_SIZE_MAX */
            &mut dtype,
            (&mut data_size_max as *mut usize).cast(),
            &mut size
        ),
        GC_ERR_SUCCESS
    );
    assert_eq!(data_size_max, 16, "EVENT_NEW_BUFFER_DATA on x64");

    // -- Buffers --------------------------------------------------------------
    let mut buffers: Vec<Vec<u8>> = (0..NUM_BUFFERS).map(|_| vec![0u8; payload_size]).collect();
    let mut handles: Vec<BufferHandle> = Vec::new();
    for (i, buf) in buffers.iter_mut().enumerate() {
        let mut bh: BufferHandle = ptr::null_mut();
        let user = (0x1000 + i) as *mut c_void; // token, never dereferenced
        assert_eq!(
            DSAnnounceBuffer(ds, buf.as_mut_ptr().cast(), buf.len(), user, &mut bh),
            GC_ERR_SUCCESS
        );
        handles.push(bh);
    }
    let mut dup: BufferHandle = ptr::null_mut();
    assert_eq!(
        DSAnnounceBuffer(
            ds,
            buffers[0].as_mut_ptr().cast(),
            buffers[0].len(),
            ptr::null_mut(),
            &mut dup,
        ),
        GC_ERR_RESOURCE_IN_USE,
        "double announce of the same memory"
    );
    // Announced count and index space.
    let mut announced = 0usize;
    let mut size = std::mem::size_of::<usize>();
    assert_eq!(
        DSGetInfo(ds, 3, &mut dtype, (&mut announced as *mut usize).cast(), &mut size),
        GC_ERR_SUCCESS
    );
    assert_eq!(announced, NUM_BUFFERS);
    for (i, want) in handles.iter().enumerate() {
        let mut bh: BufferHandle = ptr::null_mut();
        assert_eq!(DSGetBufferID(ds, i as u32, &mut bh), GC_ERR_SUCCESS);
        assert_eq!(bh, *want, "DSGetBufferID returns the announcement handle");
    }
    let mut bh: BufferHandle = ptr::null_mut();
    assert_eq!(DSGetBufferID(ds, NUM_BUFFERS as u32, &mut bh), GC_ERR_INVALID_INDEX);

    for bh in &handles {
        assert_eq!(DSQueueBuffer(ds, *bh), GC_ERR_SUCCESS);
    }
    assert_eq!(
        DSQueueBuffer(ds, handles[0]),
        GC_ERR_RESOURCE_IN_USE,
        "already queued"
    );
    assert_eq!(DSQueueBuffer(ds, ptr::null_mut()), GC_ERR_INVALID_HANDLE);

    // -- Acquisition ----------------------------------------------------------
    assert_eq!(
        DSStartAcquisition(ds, 0, 0),
        GC_ERR_INVALID_PARAMETER,
        "iNumToAcquire == 0"
    );
    assert_eq!(
        DSStartAcquisition(ds, 1 /* bogus flag */, 5),
        GC_ERR_NOT_IMPLEMENTED
    );
    assert_eq!(
        DSStartAcquisition(ds, 0, u64::MAX /* GENTL_INFINITE */),
        GC_ERR_SUCCESS
    );
    // Start while running: RESOURCE_IN_USE.
    assert_eq!(DSStartAcquisition(ds, 0, u64::MAX), GC_ERR_RESOURCE_IN_USE);
    assert_eq!(
        DSGetInfo(ds, 8, &mut dtype, (&mut grabbing as *mut u8).cast(), &mut size),
        GC_ERR_SUCCESS
    );
    assert_eq!(grabbing, 1);

    // The channel is open: SCP0 holds the stream socket's local port in
    // the host_port field (bits 16-31, spec §36.57) and SCDA0 our loopback
    // address — the same values the sim's GVSP emitter reads to find its
    // target.
    let mut scp0 = [0u8; 4];
    assert_eq!(
        read_port(port_h, u64::from(registers::SCP0), &mut scp0),
        GC_ERR_SUCCESS
    );
    let scp0_port = registers::scp_host_port(u32::from_be_bytes(scp0));
    assert_ne!(scp0_port, 0, "SCP0 activates the channel");
    let mut scda0 = [0u8; 4];
    assert_eq!(
        read_port(port_h, u64::from(registers::SCDA0), &mut scda0),
        GC_ERR_SUCCESS
    );
    assert_eq!(scda0, Ipv4Addr::LOCALHOST.octets(), "SCDA0 = loopback");

    // The sim now streams frames at us continuously (no AcquisitionStart
    // gating, sim OPEN-QUESTIONS Q15).

    // Timeout path is not deterministic here (frames arrive immediately);
    // probe a short wait and fold a popped event back into the pool.
    let mut probe = [0u8; 16];
    let mut probe_size = probe.len();
    let r = EventGetData(ev, probe.as_mut_ptr().cast(), &mut probe_size, 20);
    assert!(r == GC_ERR_SUCCESS || r == GC_ERR_TIMEOUT, "probe: {r}");
    if r == GC_ERR_SUCCESS {
        let nh: BufferHandle = unsafe {
            (probe.as_ptr() as *const EventNewBufferData)
                .read_unaligned()
                .buffer_handle
        };
        assert_eq!(DSQueueBuffer(ds, nh), GC_ERR_SUCCESS);
    }

    // First real event: exercise the BUFFER_TOO_SMALL path, then retrieve
    // the same entry with a properly sized buffer.
    let mut small = 0u8;
    let mut small_size = 1usize;
    let r = EventGetData(ev, (&mut small as *mut u8).cast(), &mut small_size, 5000);
    if r == GC_ERR_BUFFER_TOO_SMALL {
        assert_eq!(small_size, 16);
    } else {
        panic!("expected BUFFER_TOO_SMALL, got {r}");
    }

    for checked in 0..FRAMES_TO_CHECK {
        let mut raw = [0u8; 16];
        let mut n = raw.len();
        assert_eq!(
            EventGetData(ev, raw.as_mut_ptr().cast(), &mut n, 5000),
            GC_ERR_SUCCESS,
            "frame event {checked}"
        );
        assert_eq!(n, 16);
        let nd = unsafe { (raw.as_ptr() as *const EventNewBufferData).read_unaligned() };
        let pos = handles
            .iter()
            .position(|h| *h == nd.buffer_handle)
            .expect("event handle is one of ours");
        assert_eq!(
            nd.p_user_pointer,
            (0x1000 + pos) as *mut c_void,
            "user pointer round trip"
        );

        // Buffer infos of the filled buffer.
        let mut size = std::mem::size_of::<usize>();
        let mut got_w = 0usize;
        assert_eq!(
            DSGetBufferInfo(ds, nd.buffer_handle, 10, &mut dtype, (&mut got_w as *mut usize).cast(), &mut size),
            GC_ERR_SUCCESS
        );
        assert_eq!(got_w, W as usize, "BUFFER_INFO_WIDTH");
        let mut got_h = 0usize;
        assert_eq!(
            DSGetBufferInfo(ds, nd.buffer_handle, 11, &mut dtype, (&mut got_h as *mut usize).cast(), &mut size),
            GC_ERR_SUCCESS
        );
        assert_eq!(got_h, H as usize, "BUFFER_INFO_HEIGHT");
        let mut got_pf = 0u64;
        assert_eq!(
            DSGetBufferInfo(ds, nd.buffer_handle, 20, &mut dtype, (&mut got_pf as *mut u64).cast(), &mut size),
            GC_ERR_SUCCESS
        );
        assert_eq!(got_pf, u64::from(PF), "BUFFER_INFO_PIXELFORMAT");
        let mut ts = 0u64;
        assert_eq!(
            DSGetBufferInfo(ds, nd.buffer_handle, 3, &mut dtype, (&mut ts as *mut u64).cast(), &mut size),
            GC_ERR_SUCCESS
        );
        assert!(ts > 0, "BUFFER_INFO_TIMESTAMP");
        let mut ts_ns = 0u64;
        assert_eq!(
            DSGetBufferInfo(ds, nd.buffer_handle, 28, &mut dtype, (&mut ts_ns as *mut u64).cast(), &mut size),
            GC_ERR_SUCCESS,
            "BUFFER_INFO_TIMESTAMP_NS (sim ticks at 1 MHz)"
        );
        assert_eq!(ts_ns, ts * 1000, "1 MHz tick -> ns scale");
        let mut block_id = 0u64;
        assert_eq!(
            DSGetBufferInfo(ds, nd.buffer_handle, 16, &mut dtype, (&mut block_id as *mut u64).cast(), &mut size),
            GC_ERR_SUCCESS
        );
        assert!(block_id >= 1, "BUFFER_INFO_FRAMEID");
        let mut filled = 0usize;
        assert_eq!(
            DSGetBufferInfo(ds, nd.buffer_handle, 9, &mut dtype, (&mut filled as *mut usize).cast(), &mut size),
            GC_ERR_SUCCESS
        );
        assert_eq!(filled, payload_size, "BUFFER_INFO_SIZE_FILLED");
        let mut b = 0u8;
        assert_eq!(
            DSGetBufferInfo(ds, nd.buffer_handle, 7, &mut dtype, (&mut b as *mut u8).cast(), &mut size),
            GC_ERR_SUCCESS
        );
        assert_eq!(b, 0, "BUFFER_INFO_IS_INCOMPLETE: complete frames only");

        // The buffer left the output queue with the retrieval (§5.1.3),
        // NEW_DATA is still set until requeue.
        assert_eq!(
            DSGetBufferInfo(ds, nd.buffer_handle, 5, &mut dtype, (&mut b as *mut u8).cast(), &mut size),
            GC_ERR_SUCCESS
        );
        assert_eq!(b, 0, "IS_QUEUED false after delivery");
        assert_eq!(
            DSGetBufferInfo(ds, nd.buffer_handle, 4, &mut dtype, (&mut b as *mut u8).cast(), &mut size),
            GC_ERR_SUCCESS
        );
        assert_eq!(b, 1, "NEW_DATA still set");

        // Payload bytes are the sim's deterministic frame pattern.
        check_payload(&buffers[pos], block_id);

        // Back into the input pool.
        assert_eq!(DSQueueBuffer(ds, nd.buffer_handle), GC_ERR_SUCCESS);
    }

    // -- Teardown -----------------------------------------------------------
    assert_eq!(DSStopAcquisition(ds, 0), GC_ERR_SUCCESS);
    assert_eq!(
        DSStopAcquisition(ds, 0),
        GC_ERR_RESOURCE_IN_USE,
        "already stopped"
    );
    assert_eq!(
        DSGetInfo(ds, 8, &mut dtype, (&mut grabbing as *mut u8).cast(), &mut size),
        GC_ERR_SUCCESS
    );
    assert_eq!(grabbing, 0);
    let mut delivered = 0u64;
    let mut size = std::mem::size_of::<u64>();
    assert_eq!(
        DSGetInfo(ds, 1, &mut dtype, (&mut delivered as *mut u64).cast(), &mut size),
        GC_ERR_SUCCESS
    );
    assert!(
        delivered >= FRAMES_TO_CHECK as u64,
        "delivered {delivered} >= {}",
        FRAMES_TO_CHECK
    );
    let mut payload_info = 0usize;
    assert_eq!(
        DSGetInfo(ds, 7, &mut dtype, (&mut payload_info as *mut usize).cast(), &mut size),
        GC_ERR_SUCCESS
    );
    assert_eq!(payload_info, payload_size, "STREAM_INFO_PAYLOAD_SIZE");

    // EventFlush empties the queue; GCUnregisterEvent invalidates it.
    assert_eq!(EventFlush(ev), GC_ERR_SUCCESS);
    assert_eq!(EventKill(ev), GC_ERR_SUCCESS);
    assert_eq!(GCUnregisterEvent(ds, 1), GC_ERR_SUCCESS);
    assert_eq!(
        GCUnregisterEvent(ds, 1),
        GC_ERR_NOT_INITIALIZED,
        "not registered anymore (spec §6.3.7.7)"
    );

    // Revoke needs the buffers out of the pools first.
    assert_eq!(DSFlushQueue(ds, 4 /* ACQ_QUEUE_ALL_DISCARD */), GC_ERR_SUCCESS);
    for (i, bh) in handles.iter().enumerate() {
        let mut base: *mut c_void = 1usize as *mut c_void;
        let mut user: *mut c_void = ptr::null_mut();
        assert_eq!(
            DSRevokeBuffer(ds, *bh, &mut base, &mut user),
            GC_ERR_SUCCESS,
            "revoke buffer {i}"
        );
        assert_eq!(base, buffers[i].as_mut_ptr().cast(), "consumer memory back");
        assert_eq!(user, (0x1000 + i) as *mut c_void);
        // NOTE: passing the revoked handle anywhere again is use-after-free
        // by the GenTL contract (the handle is dead); not tested.
    }

    assert_eq!(DSClose(ds), GC_ERR_SUCCESS);
    // Reopen after close works.
    assert_eq!(DevOpenDataStream(dev, c_sid.as_ptr(), &mut ds), GC_ERR_SUCCESS);
    assert_eq!(DSClose(ds), GC_ERR_SUCCESS);

    assert_eq!(DevClose(dev), GC_ERR_SUCCESS);
    assert_eq!(IFClose(iface), GC_ERR_SUCCESS);
    assert_eq!(TLClose(tl), GC_ERR_SUCCESS);
    server.stop();
}
