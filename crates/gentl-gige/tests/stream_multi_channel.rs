//! M3-4a: multi-stream channel fan-out end-to-end. A two-channel sim
//! camera behind the real producer entry points: DSStartAcquisition must
//! open BOTH GVSP channels (spec §36.57 probe: channels 1..511 are
//! optional and answer GEV_STATUS_INVALID_ADDRESS past the device count;
//! `GENTLITE_GEV_STREAM_CHANNELS` unset defaults to 8, so K = min(2, 8) =
//! 2), both emitters stream into the same GenTL DataStream, and
//! DSStopAcquisition closes both channels. The GenTL surface is still a
//! single "Stream0"; the two channels' frame sequences interleave in
//! completion order, so no cross-channel block-id sequence is asserted —
//! per-frame integrity (payload pattern vs the frame's own block id) and
//! the delivered count carry the proof.

#![allow(non_snake_case)]

use std::ffi::c_char;
use std::net::Ipv4Addr;
use std::ptr;

use gentl_core::types::*;
use gentl_gige::*;
use gentl_gvcp::registers;
use gentl_simulator::{SimCamera, SimConfig, StreamConfig, StreamingProtocol};

const MAC: [u8; 6] = [0x00, 0x11, 0x22, 0x33, 0x44, 0x77];
const DEVICE_ID: &str = "00:11:22:33:44:77";
const FRAMES_TO_CHECK: usize = 24;
const NUM_BUFFERS: usize = 4;
const W: u32 = 64;
const H: u32 = 64;
const PF: u32 = 0x0108_0001; // PFNC Mono8

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

fn scp_host_port(h: PortHandle, x: u32) -> u16 {
    let mut raw = [0u8; 4];
    assert_eq!(
        read_port(h, u64::from(registers::scp(x)), &mut raw),
        GC_ERR_SUCCESS,
        "read SCP{x}"
    );
    registers::scp_host_port(u32::from_be_bytes(raw))
}

#[test]
fn data_stream_two_channel_fanout() {
    let cam = SimCamera::with_stream(
        SimConfig {
            mac: MAC,
            ip: Ipv4Addr::LOCALHOST,
            subnet_mask: Ipv4Addr::new(255, 0, 0, 0),
            gateway: Ipv4Addr::UNSPECIFIED,
            manufacturer_name: "GenTLite".into(),
            model_name: "SimCam".into(),
            device_version: "1.0.0".into(),
            serial_number: "SIM-0003".into(),
            streaming: StreamingProtocol::GvspOnly,
        },
        StreamConfig {
            width: W,
            height: H,
            pixel_format: PF,
            channels: 2,
        },
    );
    let server = cam.start(0).expect("start sim camera");
    let port = server.local_port().expect("server port");
    std::env::set_var("GENTLITE_GVCP_PORT", port.to_string());
    std::env::set_var("GENTLITE_DISCOVERY_TARGETS", format!("127.0.0.1:{port}"));
    // GENTLITE_GEV_STREAM_CHANNELS deliberately unset: default 8 > the
    // device's 2 channels, so both must open (K = min(2, 8) = 2).

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

    // The GenTL surface still exposes exactly one data stream.
    let mut num_streams = 0u32;
    assert_eq!(DevGetNumDataStreams(dev, &mut num_streams), GC_ERR_SUCCESS);
    assert_eq!(num_streams, 1, "one GenTL data stream regardless of channels");
    let (r, stream_id) = get_string(|s, n| DevGetDataStreamID(dev, 0, s, n));
    assert_eq!(r, GC_ERR_SUCCESS);
    assert_eq!(stream_id, "Stream0");

    let mut ds: DSHandle = ptr::null_mut();
    let c_sid = std::ffi::CString::new(stream_id).unwrap();
    assert_eq!(DevOpenDataStream(dev, c_sid.as_ptr(), &mut ds), GC_ERR_SUCCESS);

    let mut port_h: PortHandle = ptr::null_mut();
    assert_eq!(DevGetPort(dev, &mut port_h), GC_ERR_SUCCESS);
    let payload_size = (W * H) as usize;

    let mut ev: EventHandle = ptr::null_mut();
    assert_eq!(GCRegisterEvent(ds, 1 /* EVENT_NEW_BUFFER */, &mut ev), GC_ERR_SUCCESS);

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

    assert_eq!(
        DSStartAcquisition(ds, 0, u64::MAX /* GENTL_INFINITE */),
        GC_ERR_SUCCESS
    );

    // Both GVSP channels opened: each SCPx holds its own socket's port
    // (spec §36.57), and the two ports must differ (two local sockets).
    let scp0_port = scp_host_port(port_h, 0);
    let scp1_port = scp_host_port(port_h, 1);
    assert_ne!(scp0_port, 0, "channel 0 open");
    assert_ne!(scp1_port, 0, "channel 1 open (fan-out)");
    assert_ne!(scp0_port, scp1_port, "one socket per channel");

    // Both channels stream at the producer now: receive frames and verify
    // per-frame integrity. Cross-channel order is completion order, so the
    // block ids of consecutive deliveries are NOT asserted to be
    // consecutive; each frame's payload must match its own block id.
    let mut dtype: InfoDataType = 0;
    for checked in 0..FRAMES_TO_CHECK {
        let mut raw = [0u8; 16];
        let mut n = raw.len();
        assert_eq!(
            EventGetData(ev, raw.as_mut_ptr().cast(), &mut n, 10_000),
            GC_ERR_SUCCESS,
            "frame event {checked}"
        );
        let nd = unsafe { (raw.as_ptr() as *const EventNewBufferData).read_unaligned() };
        let pos = handles
            .iter()
            .position(|h| *h == nd.buffer_handle)
            .expect("announced handle");

        let mut block_id = 0u64;
        let mut size = 8usize;
        assert_eq!(
            DSGetBufferInfo(ds, nd.buffer_handle, 16, &mut dtype, (&mut block_id as *mut u64).cast(), &mut size),
            GC_ERR_SUCCESS
        );
        assert!(block_id >= 1, "frame {checked} block id");
        let mut incomplete = 0u8;
        let mut size = 1usize;
        assert_eq!(
            DSGetBufferInfo(ds, nd.buffer_handle, 7, &mut dtype, (&mut incomplete as *mut u8).cast(), &mut size),
            GC_ERR_SUCCESS
        );
        assert_eq!(incomplete, 0, "frame {checked} complete");
        let mut filled = 0usize;
        let mut size = std::mem::size_of::<usize>();
        assert_eq!(
            DSGetBufferInfo(ds, nd.buffer_handle, 9, &mut dtype, (&mut filled as *mut usize).cast(), &mut size),
            GC_ERR_SUCCESS
        );
        assert_eq!(filled, payload_size, "frame {checked} size");

        // Sim pattern for this frame's own block: pixel p = (p + block_id)
        // mod 256 — valid whichever channel delivered it.
        let b = block_id as usize;
        for (px, py) in [(0usize, 0usize), (W as usize - 1, 0), (17, 31), (W as usize - 1, H as usize - 1)] {
            let p = py * W as usize + px;
            assert_eq!(
                buffers[pos][p],
                ((p + b) & 0xFF) as u8,
                "frame {checked} pixel ({px},{py}) of block {block_id}"
            );
        }

        assert_eq!(DSQueueBuffer(ds, nd.buffer_handle), GC_ERR_SUCCESS);
    }

    assert_eq!(DSStopAcquisition(ds, 0), GC_ERR_SUCCESS);
    // DSStopAcquisition closes every opened channel (spec §36.57: a zero
    // host_port closes the channel).
    assert_eq!(scp_host_port(port_h, 0), 0, "channel 0 closed");
    assert_eq!(scp_host_port(port_h, 1), 0, "channel 1 closed");

    let mut delivered = 0u64;
    let mut size = 8usize;
    assert_eq!(
        DSGetInfo(ds, 1, &mut dtype, (&mut delivered as *mut u64).cast(), &mut size),
        GC_ERR_SUCCESS
    );
    assert!(
        delivered >= FRAMES_TO_CHECK as u64,
        "delivered {delivered} >= {FRAMES_TO_CHECK}"
    );

    assert_eq!(EventKill(ev), GC_ERR_SUCCESS);
    assert_eq!(GCUnregisterEvent(ds, 1), GC_ERR_SUCCESS);
    assert_eq!(DSFlushQueue(ds, 4 /* ACQ_QUEUE_ALL_DISCARD */), GC_ERR_SUCCESS);
    for bh in &handles {
        assert_eq!(
            DSRevokeBuffer(ds, *bh, ptr::null_mut(), ptr::null_mut()),
            GC_ERR_SUCCESS
        );
    }
    assert_eq!(DSClose(ds), GC_ERR_SUCCESS);
    assert_eq!(DevClose(dev), GC_ERR_SUCCESS);
    assert_eq!(IFClose(iface), GC_ERR_SUCCESS);
    assert_eq!(TLClose(tl), GC_ERR_SUCCESS);
    server.stop();
}
