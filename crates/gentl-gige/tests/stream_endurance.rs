//! M2-C endurance: 100 frames through the full producer stack against the
//! simulator's GVSP emitter. Complements `stream_sim.rs` (which checks every
//! metadata path on 10 frames) by checking frame *sequence integrity* at
//! speed: consecutive block_ids with no gaps, complete frames only.

#![allow(non_snake_case)]

use std::ffi::c_char;
use std::net::Ipv4Addr;
use std::ptr;

use gentl_core::types::*;
use gentl_gige::*;
use gentl_simulator::{SimCamera, SimConfig, StreamConfig, StreamingProtocol};

const MAC: [u8; 6] = [0x00, 0x11, 0x22, 0x33, 0x44, 0x66];
const DEVICE_ID: &str = "00:11:22:33:44:66";
const FRAMES: usize = 100;
const NUM_BUFFERS: usize = 4;
const W: u32 = 64;
const H: u32 = 64;
const PF: u32 = 0x0108_0001; // PFNC Mono8

fn get_string(mut f: impl FnMut(*mut c_char, *mut usize) -> GCError) -> String {
    let mut buf = [0u8; 1024];
    let mut size = buf.len();
    let r = f(buf.as_mut_ptr() as *mut c_char, &mut size);
    assert_eq!(r, GC_ERR_SUCCESS);
    String::from_utf8_lossy(&buf[..size.saturating_sub(1)]).into_owned()
}

#[test]
fn endurance_100_frames_no_gaps() {
    let cam = SimCamera::with_stream(
        SimConfig {
            mac: MAC,
            ip: Ipv4Addr::LOCALHOST,
            subnet_mask: Ipv4Addr::new(255, 0, 0, 0),
            gateway: Ipv4Addr::UNSPECIFIED,
            manufacturer_name: "GenTLite".into(),
            model_name: "SimCam".into(),
            device_version: "1.0.0".into(),
            serial_number: "SIM-0002".into(),
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
    let mut iface: IFHandle = ptr::null_mut();
    let c_iface = std::ffi::CString::new(get_string(|s, n| TLGetInterfaceID(tl, 0, s, n))).unwrap();
    assert_eq!(TLOpenInterface(tl, c_iface.as_ptr(), &mut iface), GC_ERR_SUCCESS);
    let mut changed = 0u8;
    assert_eq!(IFUpdateDeviceList(iface, &mut changed, 200), GC_ERR_SUCCESS);
    let mut dev: DevHandle = ptr::null_mut();
    let c_dev = std::ffi::CString::new(DEVICE_ID).unwrap();
    assert_eq!(
        IFOpenDevice(iface, c_dev.as_ptr(), 4 /* exclusive */, &mut dev),
        GC_ERR_SUCCESS
    );

    let mut ds: DSHandle = ptr::null_mut();
    let c_sid = std::ffi::CString::new("Stream0").unwrap();
    assert_eq!(DevOpenDataStream(dev, c_sid.as_ptr(), &mut ds), GC_ERR_SUCCESS);

    let mut ev: EventHandle = ptr::null_mut();
    assert_eq!(GCRegisterEvent(ds, 1, &mut ev), GC_ERR_SUCCESS);

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
    assert_eq!(
        DSStartAcquisition(ds, 0, u64::MAX /* GENTL_INFINITE */),
        GC_ERR_SUCCESS
    );

    let mut last_block_id = 0u64;
    let mut dtype: InfoDataType = 0;
    for i in 0..FRAMES {
        let mut raw = [0u8; 16];
        let mut n = raw.len();
        assert_eq!(
            EventGetData(ev, raw.as_mut_ptr().cast(), &mut n, 10_000),
            GC_ERR_SUCCESS,
            "frame {i} event"
        );
        let nd = unsafe { (raw.as_ptr() as *const EventNewBufferData).read_unaligned() };
        let pos = handles
            .iter()
            .position(|h| *h == nd.buffer_handle)
            .expect("announced handle");

        // Sequence integrity: block ids strictly consecutive, no drops.
        let mut block_id = 0u64;
        let mut size = 8usize;
        assert_eq!(
            DSGetBufferInfo(ds, nd.buffer_handle, 16, &mut dtype, (&mut block_id as *mut u64).cast(), &mut size),
            GC_ERR_SUCCESS
        );
        if i == 0 {
            // Not necessarily 1: the §11.7 packet-size negotiation window
            // (M3-2) streams before the receiver attaches, so the first
            // reassembled block can be past the restart-at-1 point (the
            // R-259st restart itself is covered by the sim-side unit
            // test). What must hold: a regular block, and from here on
            // strict consecutiveness.
            assert!(block_id >= 1, "regular block id (R-259st range)");
        } else {
            assert_eq!(block_id, last_block_id + 1, "no gap before frame {i}");
        }
        last_block_id = block_id;

        // Complete frame only.
        let mut incomplete = 0u8;
        assert_eq!(
            DSGetBufferInfo(ds, nd.buffer_handle, 7, &mut dtype, (&mut incomplete as *mut u8).cast(), &mut size),
            GC_ERR_SUCCESS
        );
        assert_eq!(incomplete, 0, "frame {i} complete");

        // Spot-check the payload pattern at 4 deterministic pixels.
        let b = block_id as usize;
        for (px, py) in [(0usize, 0usize), (W as usize - 1, 0), (0, H as usize - 1), (17, 31)] {
            let p = py * W as usize + px;
            assert_eq!(
                buffers[pos][p],
                ((p + b) & 0xFF) as u8,
                "frame {i} pixel ({px},{py})"
            );
        }

        assert_eq!(DSQueueBuffer(ds, nd.buffer_handle), GC_ERR_SUCCESS);
    }

    let mut delivered = 0u64;
    let mut size = 8usize;
    assert_eq!(
        DSGetInfo(ds, 1, &mut dtype, (&mut delivered as *mut u64).cast(), &mut size),
        GC_ERR_SUCCESS
    );
    assert!(delivered >= FRAMES as u64, "delivered {delivered}");

    assert_eq!(DSStopAcquisition(ds, 0), GC_ERR_SUCCESS);
    assert_eq!(DSFlushQueue(ds, 4), GC_ERR_SUCCESS);
    for bh in &handles {
        assert_eq!(
            DSRevokeBuffer(ds, *bh, ptr::null_mut(), ptr::null_mut()),
            GC_ERR_SUCCESS
        );
    }
    assert_eq!(EventKill(ev), GC_ERR_SUCCESS);
    assert_eq!(GCUnregisterEvent(ds, 1), GC_ERR_SUCCESS);
    assert_eq!(DSClose(ds), GC_ERR_SUCCESS);
    assert_eq!(DevClose(dev), GC_ERR_SUCCESS);
    assert_eq!(IFClose(iface), GC_ERR_SUCCESS);
    assert_eq!(TLClose(tl), GC_ERR_SUCCESS);
    server.stop();
}
