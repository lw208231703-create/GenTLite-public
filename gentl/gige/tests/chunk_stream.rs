//! End-to-end GVSP extended-chunk test (GigE Vision 3.0 §25.1): the
//! simulator streams image + one metadata chunk, and the producer must
//! deliver the chunk through the GenTL surface — buffer info flags and the
//! `DSGetBufferChunkData` layout (GenTL 1.6 §6.3.5.13).
//!
//! Wire form produced by the simulator's emitter: image data padded to 4
//! bytes, the image chunk tag (CID + length, network byte order, §24.4),
//! then the metadata chunk data and tag; the extended trailer reports the
//! metadata chunk block length and the chunk layout id.

#![allow(non_snake_case)]

use std::ffi::{c_char, c_void};
use std::net::Ipv4Addr;
use std::ptr;

use gentl_core::types::*;
use gentl_gige::*;
use gentl_simulator::{ChunkEmit, SimCamera, SimConfig, StreamConfig, StreamingProtocol};

const MAC: [u8; 6] = [0x00, 0x11, 0x22, 0x33, 0x44, 0x55];
const DEVICE_ID: &str = "00:11:22:33:44:55";
const W: u32 = 4;
const H: u32 = 4;
const PF: u32 = 0x0108_0001; // PFNC_Mono8
const IMAGE_LEN: usize = (W * H) as usize;
const CHUNK_ID: u32 = 0x2222;
const CHUNK_DATA_LEN: usize = 8;
const CHUNK_LAYOUT_ID: u32 = 0x77;
/// Metadata chunk block: padded data + 8-byte tag.
const CHUNK_BLOCK_LEN: usize = CHUNK_DATA_LEN + 8;
/// Chunk region starts after the padded image plus the image chunk tag.
const CHUNK_OFFSET: usize = IMAGE_LEN + 8;
const PAYLOAD_LEN: usize = CHUNK_OFFSET + CHUNK_BLOCK_LEN;

// BUFFER_INFO_CMD_LIST values used below (GenTL.h is authoritative).
const BUFFER_INFO_SIZE_FILLED: BufferInfoCmd = 9;
const BUFFER_INFO_FRAMEID: BufferInfoCmd = 16;
const BUFFER_INFO_DELIVERED_CHUNKPAYLOADSIZE: BufferInfoCmd = 23;
const BUFFER_INFO_CHUNKLAYOUTID: BufferInfoCmd = 24;
const BUFFER_INFO_CONTAINS_CHUNKDATA: BufferInfoCmd = 30;

/// `SINGLE_CHUNK_DATA` comes from `gentl_core::types` (`use` above).

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

fn info_value<T: Copy>(
    ds: DSHandle,
    cmd: BufferInfoCmd,
    h: BufferHandle,
    v: &mut T,
) -> GCError {
    let mut dtype: InfoDataType = 0;
    let mut size = std::mem::size_of::<T>();
    DSGetBufferInfo(ds, h, cmd, &mut dtype, (v as *mut T).cast(), &mut size)
}

#[test]
fn extended_chunk_frames_expose_the_genTL_chunk_layout() {
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
            chunk: Some(ChunkEmit {
                id: CHUNK_ID,
                len: CHUNK_DATA_LEN as u32,
                layout_id: CHUNK_LAYOUT_ID,
            }),
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
    assert_eq!(
        TLOpenInterface(tl, c_iface.as_ptr(), &mut iface),
        GC_ERR_SUCCESS
    );
    let mut changed = 0u8;
    assert_eq!(
        IFUpdateDeviceList(iface, &mut changed, 200),
        GC_ERR_SUCCESS
    );
    let mut dev: DevHandle = ptr::null_mut();
    let c_dev = std::ffi::CString::new(DEVICE_ID).unwrap();
    assert_eq!(
        IFOpenDevice(iface, c_dev.as_ptr(), 4 /* exclusive */, &mut dev),
        GC_ERR_SUCCESS
    );
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

    // Buffers are sized from the camera's PayloadSize (= image only here);
    // a chunk-carrying frame needs the whole valid payload, so announce a
    // little extra like a consumer that read the chunk-aware PayloadSize.
    let mut buffers: Vec<Vec<u8>> = (0..2).map(|_| vec![0u8; PAYLOAD_LEN + 64]).collect();
    let mut handles: Vec<BufferHandle> = Vec::new();
    for (i, buf) in buffers.iter_mut().enumerate() {
        let mut bh: BufferHandle = ptr::null_mut();
        let user = (0x1000 + i) as *mut c_void;
        assert_eq!(
            DSAnnounceBuffer(ds, buf.as_mut_ptr().cast(), buf.len(), user, &mut bh),
            GC_ERR_SUCCESS
        );
        handles.push(bh);
        assert_eq!(DSQueueBuffer(ds, bh), GC_ERR_SUCCESS);
    }
    assert_eq!(
        DSStartAcquisition(ds, 0, u64::MAX /* GENTL_INFINITE */),
        GC_ERR_SUCCESS
    );

    let mut data = [0u8; 16];
    let mut data_size = data.len();
    assert_eq!(
        EventGetData(ev, data.as_mut_ptr().cast(), &mut data_size, 5000),
        GC_ERR_SUCCESS,
        "a chunk frame must arrive (a rejected chunk payload would deliver none)"
    );
    let buffer_handle: BufferHandle = unsafe {
        (data.as_ptr() as *const EventNewBufferData)
            .read_unaligned()
            .buffer_handle
    };

    // -- Buffer info: chunk flags ------------------------------------------
    let mut contains: u8 = 0;
    assert_eq!(
        info_value(ds, BUFFER_INFO_CONTAINS_CHUNKDATA, buffer_handle, &mut contains),
        GC_ERR_SUCCESS
    );
    assert_eq!(contains, 1, "BUFFER_INFO_CONTAINS_CHUNKDATA");
    let mut chunk_size: usize = 0;
    assert_eq!(
        info_value(
            ds,
            BUFFER_INFO_DELIVERED_CHUNKPAYLOADSIZE,
            buffer_handle,
            &mut chunk_size
        ),
        GC_ERR_SUCCESS
    );
    assert_eq!(chunk_size, CHUNK_BLOCK_LEN);
    let mut layout: u64 = 0;
    assert_eq!(
        info_value(ds, BUFFER_INFO_CHUNKLAYOUTID, buffer_handle, &mut layout),
        GC_ERR_SUCCESS
    );
    assert_eq!(layout, u64::from(CHUNK_LAYOUT_ID));
    let mut filled: usize = 0;
    assert_eq!(
        info_value(ds, BUFFER_INFO_SIZE_FILLED, buffer_handle, &mut filled),
        GC_ERR_SUCCESS
    );
    assert_eq!(filled, PAYLOAD_LEN, "image + image tag + chunk block");
    let mut frame_id: u64 = 0;
    assert_eq!(
        info_value(ds, BUFFER_INFO_FRAMEID, buffer_handle, &mut frame_id),
        GC_ERR_SUCCESS
    );

    // -- GenTL chunk layout (§6.3.5.13) ------------------------------------
    let mut n_chunks = 0usize;
    assert_eq!(
        DSGetBufferChunkData(ds, buffer_handle, ptr::null_mut(), &mut n_chunks),
        GC_ERR_SUCCESS,
        "size query with a NULL array"
    );
    assert_eq!(n_chunks, 1);
    let mut chunks = [SingleChunkData {
        chunk_id: 0,
        chunk_offset: 0,
        chunk_length: 0,
    }];
    assert_eq!(
        DSGetBufferChunkData(ds, buffer_handle, chunks.as_mut_ptr(), &mut n_chunks),
        GC_ERR_SUCCESS
    );
    assert_eq!(n_chunks, 1, "one metadata chunk");
    assert_eq!(chunks[0].chunk_id, u64::from(CHUNK_ID));
    assert_eq!(chunks[0].chunk_offset as usize, CHUNK_OFFSET);
    assert_eq!(chunks[0].chunk_length, CHUNK_DATA_LEN);

    // -- Content: the sim's deterministic pattern --------------------------
    let idx = handles
        .iter()
        .position(|h| *h == buffer_handle)
        .expect("delivered handle is one of the announced buffers");
    let frame = &buffers[idx];
    let block = frame_id as usize;
    for (i, b) in frame[..IMAGE_LEN].iter().enumerate() {
        assert_eq!(*b, (i + block) as u8, "image byte {i}");
    }
    // Image chunk tag: CID 1 (image) and the padded image length, BE.
    assert_eq!(
        u32::from_be_bytes(frame[IMAGE_LEN..IMAGE_LEN + 4].try_into().unwrap()),
        1
    );
    assert_eq!(
        u32::from_be_bytes(frame[IMAGE_LEN + 4..IMAGE_LEN + 8].try_into().unwrap()),
        IMAGE_LEN as u32
    );
    // Chunk data pattern: fill_pattern with the byte offset past the image.
    for (i, b) in frame[CHUNK_OFFSET..CHUNK_OFFSET + CHUNK_DATA_LEN]
        .iter()
        .enumerate()
    {
        assert_eq!(*b, (IMAGE_LEN + i + block) as u8, "chunk byte {i}");
    }
    // Metadata chunk tag: the configured ID and data length, BE.
    let tag_at = CHUNK_OFFSET + CHUNK_DATA_LEN;
    assert_eq!(
        u32::from_be_bytes(frame[tag_at..tag_at + 4].try_into().unwrap()),
        CHUNK_ID
    );
    assert_eq!(
        u32::from_be_bytes(frame[tag_at + 4..tag_at + 8].try_into().unwrap()),
        CHUNK_DATA_LEN as u32
    );

    // -- Too-small array: GC_ERR_BUFFER_TOO_SMALL (§6.3.5.13 Returns) -----
    let mut capacity = 0usize;
    let mut one = [SingleChunkData {
        chunk_id: 0,
        chunk_offset: 0,
        chunk_length: 0,
    }];
    assert_eq!(
        DSGetBufferChunkData(ds, buffer_handle, one.as_mut_ptr(), &mut capacity),
        GC_ERR_BUFFER_TOO_SMALL
    );
    assert_eq!(capacity, 1, "required number of entries is reported");

    // -- Malformed chunk tags: GC_ERR_PARSING_CHUNK_DATA -------------------
    buffers[idx][tag_at + 4..tag_at + 8].copy_from_slice(&[0xFF; 4]);
    let mut capacity = 1usize;
    assert_eq!(
        DSGetBufferChunkData(ds, buffer_handle, one.as_mut_ptr(), &mut capacity),
        GC_ERR_PARSING_CHUNK_DATA
    );

    assert_eq!(DSQueueBuffer(ds, buffer_handle), GC_ERR_SUCCESS);
    assert_eq!(DSStopAcquisition(ds, 0), GC_ERR_SUCCESS);
    assert_eq!(DSClose(ds), GC_ERR_SUCCESS);
    assert_eq!(DevClose(dev), GC_ERR_SUCCESS);
    assert_eq!(IFClose(iface), GC_ERR_SUCCESS);
    assert_eq!(TLClose(tl), GC_ERR_SUCCESS);
    assert_eq!(GCCloseLib(), GC_ERR_SUCCESS);
}
