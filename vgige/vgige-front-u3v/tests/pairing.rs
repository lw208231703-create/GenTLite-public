//! The crown test of Step 3: this crate's device state machine over
//! in-memory byte streams, driven by gentl-u3v's REAL host-side client
//! (`U3vCamera::from_pipes`) — a full U3V session with no USB and no
//! gateway on top: bootstrap walk, manifest XML (injected nodes),
//! register pass-through to the stub camera, custom window, and a Mono8
//! stream whose payload survives the round trip byte-for-byte.

mod support;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use gentl_u3v::client::{Block, U3vCamera};
use support::{
    mono8_frame, ByteQueue, ClientIn, ClientOut, DevCommands, DevTransfers, StubBack, MPS,
    PAYLOAD, VENDOR_WIDTH, W, H,
};
use vgige_core::{CameraBack, FrameSession, SharedBack, XmlMerger, CUSTOM_BASE, VGIGE_VERSION};
use vgige_front_u3v::{synthesize_guid, DeviceConfig, U3vDevice};

struct Pair {
    device: U3vDevice,
    client: U3vCamera,
    back: Arc<Mutex<StubBack>>,
}

fn start() -> Pair {
    let cmd_q = Arc::new(ByteQueue::default());
    let ack_q = Arc::new(ByteQueue::default());
    let data_q = Arc::new(ByteQueue::default());
    let event_q = Arc::new(ByteQueue::default());

    let back = Arc::new(Mutex::new(StubBack::new()));
    let raw = back.lock().unwrap().xml();
    let xml = XmlMerger::new(&raw, None).expect("merge");
    let config = DeviceConfig::from_parts(
        back.lock().unwrap().identity(),
        xml.blob(),
        PAYLOAD as u64,
        synthesize_guid("STUB-0001"),
    );
    let back_dyn: SharedBack = back.clone();
    let device = U3vDevice::new(
        config,
        Arc::clone(&back_dyn),
        Arc::new(DevCommands(Arc::clone(&cmd_q))),
        Arc::new(DevTransfers(Arc::clone(&ack_q), MPS as u16)),
        Arc::new(DevTransfers(Arc::clone(&data_q), MPS as u16)),
        Arc::new(DevTransfers(Arc::clone(&event_q), MPS as u16)),
    );
    let client = U3vCamera::from_pipes_with_events(
        Arc::new(ClientOut(cmd_q)),
        Arc::new(ClientIn(ack_q)),
        vec![Arc::new(ClientIn(data_q))],
        Some(Arc::new(ClientIn(event_q))),
    )
    .expect("host client bootstrap over the device");
    Pair { device, client, back }
}

#[test]
fn bootstrap_identity_and_manifest_xml() {
    let p = start();
    let b = p.client.bootstrap();
    assert_eq!(b.manufacturer, "StubVendor");
    assert_eq!(b.model, "StubCam");
    assert_eq!(b.serial_number, "STUB-0001");
    assert!(b.sirm_address.is_some(), "SIRM advertised");
    assert_eq!(b.number_of_stream_channels, 1);
    // Capability gating shape: SBRM + endianness + written-length.
    assert_eq!(b.device_capability & (1 << 9), 1 << 9);

    let xml = p.client.fetch_xml().expect("manifest XML");
    let text = String::from_utf8(xml).unwrap();
    assert!(text.contains("</RegisterDescription>"));
    assert!(text.contains("<Category Name=\"SmartCamera\""), "injected nodes");
    assert!(text.contains("WidthReg"), "camera's own node preserved");
}

#[test]
fn register_pass_through_and_custom_window() {
    let p = start();
    // Vendor register: device forwards to the downstream camera.
    assert_eq!(p.client.read_reg(u64::from(VENDOR_WIDTH)).unwrap(), W);
    p.client
        .write_reg(u64::from(VENDOR_WIDTH), 128)
        .expect("vendor write");
    assert_eq!(p.client.read_reg(u64::from(VENDOR_WIDTH)).unwrap(), 128);
    assert_eq!(
        CameraBack::read_reg(&mut *p.back.lock().unwrap(), VENDOR_WIDTH).unwrap(),
        128,
        "write landed on the downstream camera"
    );

    // Custom window is served locally on the device.
    assert_eq!(
        p.client.read_reg(u64::from(CUSTOM_BASE)).unwrap(),
        VGIGE_VERSION
    );
    p.device.set_ai_result(2);
    p.device.set_ai_score(987);
    assert_eq!(p.client.read_reg(u64::from(CUSTOM_BASE) + 4).unwrap(), 2);
    assert_eq!(p.client.read_reg(u64::from(CUSTOM_BASE) + 8).unwrap(), 987);
    // RO slot rejects writes (GenCP WRITE_PROTECT, GEV-mapped name).
    assert!(p.client.write_reg(u64::from(CUSTOM_BASE), 0).is_err());

    // Error transparency: an unmapped address reaches the camera and
    // its status crosses unchanged.
    let err = p.client.read_reg(0x0FF0).unwrap_err();
    match err {
        gentl_u3v::client::U3vError::Status(s) => assert_eq!(s, 0x8003),
        other => panic!("expected Status(0x8003), got {other:?}"),
    }
}

#[test]
fn stream_blocks_roundtrip_byte_exact() {
    let p = start();
    let stream = p.client.stream(0).expect("stream channel 0");
    stream.start().expect("program SIRM + Stream Enable");

    let mut session = p.device.frame_session();
    session.open(0).expect("session open");
    session.send_frame(&mono8_frame(1, 111));
    session.send_frame(&mono8_frame(2, 222));

    // Block 1: leader metadata + payload bytes + trailer status.
    let block = stream
        .next_block(Duration::from_secs(5))
        .expect("block within the window")
        .expect("block ok");
    let (leader, payload, trailer) = match block {
        Block::Image {
            leader,
            payload,
            trailer,
        } => (leader, payload, trailer),
        other => panic!("expected an image block, got {other:?}"),
    };
    assert_eq!(leader.block_id, 1);
    assert_eq!(leader.timestamp, 111);
    assert_eq!(leader.pixel_format, support::MONO8);
    assert_eq!(leader.size_x, W);
    assert_eq!(leader.size_y, H);
    assert_eq!(payload.len(), PAYLOAD);
    assert!(
        payload
            .iter()
            .enumerate()
            .all(|(k, &b)| b == (k as u64 + 1) as u8),
        "payload is the Mono8 pattern for block 1, byte-exact"
    );
    assert_eq!(trailer.status, 0x0000);
    assert_eq!(trailer.size_y, H);

    // Block 2 follows with its own id and pattern.
    let block = stream
        .next_block(Duration::from_secs(5))
        .expect("block 2")
        .expect("block 2 ok");
    match block {
        Block::Image { payload, .. } => assert!(
            payload
                .iter()
                .enumerate()
                .all(|(k, &b)| b == (k as u64 + 2) as u8),
            "payload is the Mono8 pattern for block 2, byte-exact"
        ),
        other => panic!("expected an image block, got {other:?}"),
    }

    // FrameCountRelayed (custom-window slot 3) tracks the session.
    assert_eq!(
        p.client.read_reg(u64::from(CUSTOM_BASE) + 12).unwrap(),
        2
    );
}

#[test]
fn variable_frame_sizes_do_not_desync_following_blocks() {
    let p = start();
    let stream = p.client.stream(0).expect("stream channel 0");
    stream.start().expect("program SIRM + Stream Enable");
    let mut session = p.device.frame_session();
    session.open(0).expect("session open");

    let mut small = mono8_frame(3, 333);
    small.width = 2;
    small.height = 1;
    small.payload.truncate(2);
    let mut medium = mono8_frame(4, 444);
    medium.width = 2;
    medium.height = 2;
    medium.payload.truncate(4);
    session.send_frame(&small);
    session.send_frame(&medium);
    session.send_frame(&mono8_frame(5, 555));

    for (block_id, width, height, payload_len) in [(3, 2, 1, 2), (4, 2, 2, 4), (5, W, H, PAYLOAD)] {
        let block = stream
            .next_block(Duration::from_secs(5))
            .expect("variable-size block")
            .expect("variable-size block ok");
        match block {
            Block::Image {
                leader,
                payload,
                trailer,
            } => {
                assert_eq!(leader.block_id, block_id - 2);
                assert_eq!((leader.size_x, leader.size_y), (width, height));
                assert_eq!(payload.len(), payload_len);
                assert_eq!(trailer.status, 0);
                assert_eq!(trailer.size_y, height);
            }
            other => panic!("expected an image block, got {other:?}"),
        }
    }
}

#[test]
fn eirm_test_event_reaches_host() {
    let p = start();
    let events = p.client.start_events().expect("start DEI pump");
    p.client
        .write_reg(0x0003_1008, 1)
        .expect("fire Event Test Control");
    let event = events.next(Duration::from_secs(1)).expect("test event");
    assert_eq!(event.event_id, 0x4FFF);
    assert!(event.timestamp > 0);
    assert!(event.data.is_empty());
    p.client.stop_events().expect("stop DEI pump");
}
