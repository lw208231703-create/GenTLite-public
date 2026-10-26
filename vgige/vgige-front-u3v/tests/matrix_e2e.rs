//! The matrix cell "U3V front ← GigE-class back" (shape-aligned with
//! relay_e2e.rs / gateway_u3v_e2e.rs): a stub `CameraBack` plays the
//! downstream camera, the vgige relay moves its frames back→front, and
//! the front-u3v device serves a real U3V host client. The open/close
//! trigger is the host's Stream Enable (§5.4.1.2) — no GVCP SCPx in
//! sight. (The real GigeBack cannot be linked here: dual-GenTL-producer
//! symbol collision, Q38-7; the seam itself is covered in
//! vgige-cli/tests/relay_e2e.rs.)

mod support;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use gentl_u3v::client::{Block, U3vCamera};
use support::{
    mono8_frame, ByteQueue, ClientIn, ClientOut, DevCommands, DevTransfers, StubBack, MPS,
    PAYLOAD, VENDOR_WIDTH, W,
};
use vgige_core::{CameraBack, Relay, SharedBack, XmlMerger, CUSTOM_BASE, VGIGE_VERSION};
use vgige_front_u3v::{synthesize_guid, DeviceConfig, U3vDevice};

struct Cell {
    device: U3vDevice,
    client: U3vCamera,
    back: Arc<Mutex<StubBack>>,
}

fn start(n_frames: usize) -> Cell {
    let cmd_q = Arc::new(ByteQueue::default());
    let ack_q = Arc::new(ByteQueue::default());
    let data_q = Arc::new(ByteQueue::default());

    let mut stub = StubBack::new();
    stub.frames = (1..=n_frames as u64).map(|b| mono8_frame(b, b * 1000)).collect();
    let back = Arc::new(Mutex::new(stub));
    let raw = back.lock().unwrap().xml();
    let xml = XmlMerger::new(&raw, None).expect("merge");
    let config = DeviceConfig::from_parts(
        back.lock().unwrap().identity(),
        xml.blob(),
        PAYLOAD as u64,
        synthesize_guid("STUB-0001"),
    );
    let back_dyn: SharedBack = back.clone();
    let dev_stream = DevTransfers(Arc::clone(&data_q), MPS as u16);
    let device = U3vDevice::new(
        config,
        Arc::clone(&back_dyn),
        Arc::new(DevCommands(Arc::clone(&cmd_q))),
        Arc::new(DevTransfers(Arc::clone(&ack_q), MPS as u16)),
        Arc::new(dev_stream),
    );

    // Relay: back → device session. The host's Stream Enable opens the
    // downstream channel; disabling closes it (cascade, R-091ca shape).
    let relay = Relay::new(
        Arc::clone(&back_dyn),
        true,
        Box::new({
            let device = device.clone();
            move |_regs| Box::new(device.frame_session())
        }),
    );
    device.set_stream_watcher({
        let relay = relay.clone();
        move |on| {
            if on {
                let _ = relay.open();
            } else {
                relay.close();
            }
        }
    });

    let client = U3vCamera::from_pipes(
        Arc::new(ClientOut(cmd_q)),
        Arc::new(ClientIn(ack_q)),
        vec![Arc::new(ClientIn(data_q))],
    )
    .expect("host client over the gateway device");
    Cell {
        device,
        client,
        back,
    }
}

#[test]
fn pass_through_and_identity() {
    let c = start(0);
    // Bootstrap identity = the downstream camera's (ABRM walk over the
    // device, served from the stub).
    assert_eq!(c.client.bootstrap().manufacturer, "StubVendor");
    assert_eq!(c.client.bootstrap().model, "StubCam");
    // Vendor register forwards 1:1.
    assert_eq!(c.client.read_reg(u64::from(VENDOR_WIDTH)).unwrap(), W);
    c.client.write_reg(u64::from(VENDOR_WIDTH), 200).unwrap();
    assert_eq!(
        CameraBack::read_reg(&mut *c.back.lock().unwrap(), VENDOR_WIDTH).unwrap(),
        200
    );
    // Custom window: local slot semantics.
    assert_eq!(
        c.client.read_reg(u64::from(CUSTOM_BASE)).unwrap(),
        VGIGE_VERSION
    );
    c.device.set_ai_result(1);
    assert_eq!(c.client.read_reg(u64::from(CUSTOM_BASE) + 4).unwrap(), 1);
}

#[test]
fn error_transparency() {
    let c = start(0);
    let err = c.client.read_reg(0x0FF0).unwrap_err();
    match err {
        gentl_u3v::client::U3vError::Status(s) => assert_eq!(s, 0x8003),
        other => panic!("expected Status(0x8003), got {other:?}"),
    }
    // RO custom slot: WRITE_PROTECT shape (0x8004).
    let err = c.client.write_reg(u64::from(CUSTOM_BASE), 1).unwrap_err();
    match err {
        gentl_u3v::client::U3vError::Status(s) => assert_eq!(s, 0x8004),
        other => panic!("expected Status(0x8004), got {other:?}"),
    }
}

#[test]
fn xml_injection_over_manifest() {
    let c = start(0);
    let xml = c.client.fetch_xml().expect("manifest XML");
    let text = String::from_utf8(xml).unwrap();
    assert!(text.contains("<Category Name=\"SmartCamera\""), "injected");
    assert!(text.contains("WidthReg"), "camera nodes preserved");
}

#[test]
fn stream_relay_cascade_on_stream_enable() {
    let c = start(3);
    let stream = c.client.stream(0).expect("channel 0");
    stream.start().expect("SIRM program + enable");

    // The host's Stream Enable cascaded downstream: the stub camera's
    // frames relay to the U3V host, byte-exact, ids from 1.
    let mut ids = Vec::new();
    for want in 1..=3u64 {
        let block = stream
            .next_block(Duration::from_secs(5))
            .expect("block")
            .expect("block ok");
        match block {
            Block::Image {
                leader, payload, ..
            } => {
                assert_eq!(leader.block_id, want);
                assert_eq!(payload.len(), PAYLOAD);
                assert!(
                    payload
                        .iter()
                        .enumerate()
                        .all(|(k, &b)| b == (k as u64 + want) as u8),
                    "block {want} payload byte-exact"
                );
                ids.push(leader.block_id);
            }
            other => panic!("expected an image block, got {other:?}"),
        }
    }
    assert_eq!(ids, vec![1, 2, 3]);

    // Relayed counter visible in the custom window.
    assert!(c.client.read_reg(u64::from(CUSTOM_BASE) + 12).unwrap() >= 3);

    // Close cascade: host drops Stream Enable → relay closes the
    // downstream camera (the stub's close flag latches) and no further
    // block starts within the window.
    stream.stop();
    let closed = c
        .back
        .lock()
        .unwrap()
        .closed
        .load(std::sync::atomic::Ordering::SeqCst);
    assert!(closed, "downstream closed by the cascade");
    assert!(
        stream.next_block(Duration::from_millis(400)).is_none(),
        "no block after Stream Enable dropped"
    );
}
