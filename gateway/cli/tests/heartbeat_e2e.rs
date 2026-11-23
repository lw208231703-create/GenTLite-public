//! Downlink heartbeat end-to-end (GigE Vision 3.0 §10.5, R-055ca/R-075cd/
//! R-076cd): the simulator now closes the control channel after 3x its
//! heartbeat timeout without primary traffic; the gateway's heartbeat
//! worker must keep that channel open across idle periods, and re-grant
//! exclusive access when the camera dropped it.

use std::net::Ipv4Addr;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gentl_gvcp::{registers as reg, GvcpServer};
use gentl_simulator::{SimCamera, SimConfig, StreamConfig, StreamingProtocol};
use vgige_back_gige::GigeBack;
use vgige_core::{heartbeat, CameraBack, SharedBack};

const MAC: [u8; 6] = [0x00, 0x11, 0x22, 0x33, 0x44, 0x55];

fn start() -> (SimCamera, GvcpServer, u16) {
    let cam = SimCamera::with_stream(
        SimConfig {
            mac: MAC,
            ip: Ipv4Addr::LOCALHOST,
            subnet_mask: Ipv4Addr::new(255, 0, 0, 0),
            gateway: Ipv4Addr::UNSPECIFIED,
            manufacturer_name: "GenTLite".into(),
            model_name: "SimCam".into(),
            device_version: "1.0.0".into(),
            serial_number: "SIM-HEARTBEAT".into(),
            streaming: StreamingProtocol::GvspOnly,
        },
        StreamConfig {
            width: 8,
            height: 8,
            pixel_format: 0x0108_0001,
            channels: 1,
            legacy_headers: false,
            chunk: None,
        },
    );
    let server = cam.start(0).expect("start sim camera");
    let port = server.local_port().expect("server port");
    (cam, server, port)
}

fn ea_set(back: &SharedBack) -> bool {
    let flags = back
        .lock()
        .expect("back lock")
        .read_reg(reg::CCP)
        .expect("CCP read");
    flags & reg::CCP_EXCLUSIVE_ACCESS != 0
}

#[test]
fn heartbeat_keeps_the_downlink_control_channel_open() {
    let (_cam, _server, port) = start();
    let mut back = GigeBack::connect(Ipv4Addr::LOCALHOST, port).expect("connect gateway backend");
    // Short timeout so the test does not idle for the 3 s default: the sim
    // clamps to >= 500 ms, so the device closes the channel after 1.5 s.
    back.write_reg(reg::HEARTBEAT_TIMEOUT, 500).expect("timeout write");
    back.write_reg(reg::CCP, reg::CCP_EXCLUSIVE_ACCESS)
        .expect("take exclusive access");
    let back: SharedBack = Arc::new(std::sync::Mutex::new(back));

    let stop = Arc::new(AtomicBool::new(false));
    let handle = heartbeat::spawn(Arc::clone(&back), Arc::clone(&stop));

    // Idle well past the 3x window: without heartbeats the sim would have
    // released the channel and this read would return CCP == 0.
    std::thread::sleep(Duration::from_millis(2200));
    assert!(
        ea_set(&back),
        "the heartbeat worker must keep exclusive access alive while idle"
    );

    // A camera that dropped the channel (e.g. a stall before the worker
    // started) is reclaimed by the worker.
    back.lock()
        .expect("back lock")
        .write_reg(reg::CCP, 0)
        .expect("release");
    let deadline = Instant::now() + Duration::from_secs(2);
    while !ea_set(&back) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(ea_set(&back), "the heartbeat worker re-grants exclusive access");

    // Stop the worker: the next idle window must expire again.
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    handle.join().expect("heartbeat joins");
    std::thread::sleep(Duration::from_millis(2200));
    assert!(
        !ea_set(&back),
        "without the worker the camera closes the control channel (R-076cd)"
    );
}
