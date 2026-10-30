//! Gateway end-to-end on loopback, no hardware: `gentl_simulator::SimCamera`
//! plays the real camera, the assembled vgige stack (back-gige + core +
//! front-gige) proxies it on another port, and a plain `GvcpClient` plays
//! the remote client. Covers the four v1 contracts: register pass-through,
//! GEV status pass-through, the custom window + XML injection, and the
//! GVSP stream relay (including 1.2-camera → 3.0-wire conversion, since the
//! sim's `legacy_headers` mode emits the 1.2 form).

use std::net::{Ipv4Addr, UdpSocket};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gentl_core::GentlError;
use gentl_gvcp::gvsp::*;
use gentl_gvcp::registers as reg;
use gentl_gvcp::wire::*;
use gentl_gvcp::{GvcpClient, GvcpServer};
use gentl_simulator::{
    SimCamera, SimConfig, StreamConfig, StreamingProtocol, REG_EXPOSURE_TIME, REG_WIDTH,
};
use vgige_back_gige::GigeBack;
use vgige_core::{CameraBack, Relay, RegisterSpace, SharedBack, XmlMerger, CUSTOM_BASE, VGIGE_VERSION, XML_BASE_V, slot};
use vgige_front_gige::{assemble_identity, GigEFront, GvspSession, UpIdentity};

/// The virtual device's claimed MAC (an arbitrary locally-administered one).
const UP_MAC: [u8; 6] = [0x02, 0xAB, 0xCD, 0xEF, 0x01, 0x02];
const W: u32 = 64;
const H: u32 = 48;
const MONO8: u32 = 0x0108_0001;

/// Everything one test needs: sim camera + its server, the assembled vgige
/// stack (downstream back handle + register space), and the remote client.
struct Stack {
    _cam: SimCamera,
    _cam_server: GvcpServer,
    _up_server: GvcpServer,
    space: RegisterSpace,
    down: SharedBack,
    remote: GvcpClient,
}

fn start(legacy_stream: bool) -> Stack {
    let cam = SimCamera::with_stream(
        SimConfig {
            mac: [0x00, 0x11, 0x22, 0x33, 0x44, 0x55],
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
            pixel_format: MONO8,
            channels: 1,
            legacy_headers: legacy_stream,
        },
    );
    let cam_server = cam.start(0).expect("start sim camera");
    let down_port = cam_server.local_port().unwrap();

    // Back (GigE adapter) -> shared handle; core relay + register space;
    // front (discovery identity + GVSP 3.0 session) behind the GVCP server.
    let mut back = GigeBack::connect(Ipv4Addr::LOCALHOST, down_port).expect("connect down");
    CameraBack::write_reg(&mut back, reg::CCP, reg::CCP_EXCLUSIVE_ACCESS).expect("CCP(EA) downstream");
    let back: SharedBack = Arc::new(Mutex::new(back));
    let raw_xml = back.lock().unwrap().fetch_xml().expect("fetch camera XML");
    let xml = Arc::new(XmlMerger::new(&raw_xml, None).expect("merge camera XML"));
    let relay = Relay::new(
        Arc::clone(&back),
        true,
        Box::new(|regs| Box::new(GvspSession::new(regs))),
    );
    let space = RegisterSpace::new(
        Arc::clone(&back),
        Arc::clone(&xml),
        Ipv4Addr::LOCALHOST,
        UP_MAC,
        relay,
    );
    let identity = assemble_identity(
        &back,
        &UpIdentity {
            ip: Ipv4Addr::LOCALHOST,
            mac: UP_MAC,
            mask: Ipv4Addr::new(255, 0, 0, 0),
        },
    )
    .expect("assemble identity");
    // Bind the up-facing address explicitly, like main does.
    let up_server =
        GvcpServer::start_bound(Ipv4Addr::LOCALHOST, 0, GigEFront::new(identity, space.clone()))
            .expect("start proxy");
    let up_port = up_server.local_port().unwrap();
    let remote = GvcpClient::connect(Ipv4Addr::LOCALHOST, up_port).expect("connect remote");
    Stack {
        _cam: cam,
        _cam_server: cam_server,
        _up_server: up_server,
        space,
        down: back,
        remote,
    }
}

/// The GEV status code behind a client error (panics on non-GEV errors).
fn gev(e: GentlError) -> u16 {
    match e {
        GentlError::GevStatus(s) => s,
        other => panic!("expected a GEV status, got {other}"),
    }
}

#[test]
fn register_passthrough_and_virtualized_identity() {
    let s = start(false);

    // Camera registers read through the proxy (the sim seeds Width from the
    // stream config).
    assert_eq!(s.remote.read_reg(REG_WIDTH).unwrap(), W);
    assert_eq!(
        s.remote.read_reg(reg::VERSION).unwrap(),
        0x0002_0002,
        "bootstrap Version passes through verbatim"
    );

    // Virtualized identity: the up-facing IP/MAC replace the camera's.
    assert_eq!(
        s.remote.read_reg(reg::CURRENT_IP).unwrap(),
        u32::from(Ipv4Addr::LOCALHOST)
    );
    assert_eq!(
        s.remote.read_reg(reg::DEVICE_MAC_HIGH).unwrap(),
        u16::from_be_bytes([UP_MAC[0], UP_MAC[1]]) as u32
    );
    assert_eq!(
        s.remote.read_reg(reg::DEVICE_MAC_LOW).unwrap(),
        u32::from_be_bytes([UP_MAC[2], UP_MAC[3], UP_MAC[4], UP_MAC[5]])
    );
    assert_eq!(s.remote.read_reg(reg::NUM_STREAM_CHANNELS).unwrap(), 1);
    let mut channel_count = [0u8; 4];
    s.remote.read_mem(reg::NUM_STREAM_CHANNELS, &mut channel_count).unwrap();
    assert_eq!(channel_count, 1u32.to_be_bytes());

    // GVCP capability passes through with the packet_resend bit masked
    // (the virtual server never serves PACKETRESEND_CMD, R-197ca).
    let cap = s.remote.read_reg(reg::GVCP_CAPABILITY).unwrap();
    assert_eq!(cap & reg::GVCP_CAPABILITY_PACKET_RESEND, 0);
    assert_ne!(cap & 0xC000_0000, 0, "UN/SN capability bits survive");

    // A write passes through and lands on the real camera.
    s.remote
        .write_reg(REG_EXPOSURE_TIME, 12_345.0f32.to_bits())
        .unwrap();
    let on_camera = s.down.lock().unwrap().read_reg(REG_EXPOSURE_TIME).unwrap();
    assert_eq!(on_camera, 12_345.0f32.to_bits());

    // The virtual CCP is local: grant and release never touch the camera.
    s.remote
        .write_reg(reg::CCP, reg::CCP_CONTROL_ACCESS)
        .unwrap();
    assert_eq!(s.remote.read_reg(reg::CCP).unwrap(), reg::CCP_CONTROL_ACCESS);
    s.remote.write_reg(reg::CCP, 0).unwrap();
    assert_eq!(s.remote.read_reg(reg::CCP).unwrap(), 0);

    s.remote.write_reg(reg::HEARTBEAT_TIMEOUT, 500).unwrap();
    assert_eq!(s.remote.read_reg(reg::HEARTBEAT_TIMEOUT).unwrap(), 500);
    s.remote.write_reg(reg::CCP, reg::CCP_CONTROL_ACCESS).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(600));
    s.remote.write_reg(reg::CCP, reg::CCP_EXCLUSIVE_ACCESS).unwrap();
}

#[test]
fn control_release_closes_stream_but_new_grant_does_not() {
    let s = start(false);
    s.remote
        .write_reg(reg::SCDA0, u32::from(Ipv4Addr::LOCALHOST))
        .unwrap();
    s.remote.write_reg(reg::SCP0, reg::scp_make(12345)).unwrap();
    assert_ne!(s.remote.read_reg(reg::SCP0).unwrap(), 0);

    s.remote.write_reg(reg::CCP, reg::CCP_CONTROL_ACCESS).unwrap();
    assert_ne!(s.remote.read_reg(reg::SCP0).unwrap(), 0);
    s.remote.write_reg(reg::CCP, 0).unwrap();
    assert_eq!(s.remote.read_reg(reg::SCP0).unwrap(), 0);
}

#[test]
fn error_passthrough_matches_the_camera() {
    let s = start(false);

    // Reserved bootstrap gap: both sides must answer the same status
    // (the camera side already speaks GEV status codes through the back).
    let via_proxy = gev(s.remote.read_reg(0x0FF0).unwrap_err());
    let via_camera = s.down.lock().unwrap().read_reg(0x0FF0).unwrap_err();
    assert_eq!(via_proxy, via_camera);
    assert_eq!(via_proxy, GEV_STATUS_INVALID_ADDRESS);

    // Read-only register write: WRITE_PROTECT passes through (0x8004).
    assert_eq!(
        gev(s.remote.write_reg(reg::VERSION, 0).unwrap_err()),
        GEV_STATUS_WRITE_PROTECT
    );

    // Unaligned access: BAD_ALIGNMENT, answered locally by the proxy.
    assert_eq!(
        gev(s.remote.read_reg(0x0002).unwrap_err()),
        GEV_STATUS_BAD_ALIGNMENT
    );

    // The virtual CCP denies a different privilege while occupied (0x8006),
    // and repeats of the same request are accepted (R-069cd).
    s.remote
        .write_reg(reg::CCP, reg::CCP_EXCLUSIVE_ACCESS)
        .unwrap();
    s.remote
        .write_reg(reg::CCP, reg::CCP_EXCLUSIVE_ACCESS)
        .unwrap();
    assert_eq!(
        gev(s.remote.write_reg(reg::CCP, reg::CCP_CONTROL_ACCESS).unwrap_err()),
        GEV_STATUS_ACCESS_DENIED
    );
}

#[test]
fn custom_window_and_injected_xml() {
    let s = start(false);

    // Slot semantics: version RO, general slots RW, RO slots write-protect.
    assert_eq!(
        s.remote.read_reg(CUSTOM_BASE + (slot::VERSION as u32) * 4).unwrap(),
        VGIGE_VERSION
    );
    s.remote.write_reg(CUSTOM_BASE + 0x40, 0xDEAD_BEEF).unwrap();
    assert_eq!(s.remote.read_reg(CUSTOM_BASE + 0x40).unwrap(), 0xDEAD_BEEF);
    assert_eq!(
        gev(s.remote.write_reg(CUSTOM_BASE, 1).unwrap_err()),
        GEV_STATUS_WRITE_PROTECT
    );

    // Business hooks mirror into the RO slots.
    assert_eq!(s.remote.read_reg(CUSTOM_BASE + 4).unwrap(), 0, "AI_Result defaults to 0");
    assert_eq!(s.remote.read_reg(CUSTOM_BASE + 8).unwrap(), 0, "AI_Score defaults to 0");
    s.space.set_ai_result(2);
    s.space.set_ai_score(987);
    assert_eq!(s.remote.read_reg(CUSTOM_BASE + 4).unwrap(), 2);
    assert_eq!(s.remote.read_reg(CUSTOM_BASE + 8).unwrap(), 987);
    // ... and the RO slots reject client writes even for the business
    // values.
    assert_eq!(
        gev(s.remote.write_reg(CUSTOM_BASE + 4, 0).unwrap_err()),
        GEV_STATUS_WRITE_PROTECT
    );

    // Channels past 0 are unimplemented on the virtual device (§36.57
    // probe semantics).
    assert_eq!(
        gev(s.remote.read_reg(reg::scp(1)).unwrap_err()),
        GEV_STATUS_INVALID_ADDRESS
    );

    // The virtual First URL points at the injected blob, and the blob
    // carries both the camera's own nodes and the injected SmartCamera
    // category.
    let url = s.remote.xml_url().unwrap();
    assert!(url.starts_with("local:vgige.xml;"), "virtual URL: {url}");
    let mut parts = url.split(';');
    parts.next();
    let addr = u32::from_str_radix(parts.next().unwrap(), 16).unwrap();
    let len = u32::from_str_radix(parts.next().unwrap(), 16).unwrap();
    assert_eq!(addr, XML_BASE_V);

    let mut buf = vec![0u8; ((len + 3) & !3) as usize];
    s.remote.read_mem(addr, &mut buf).unwrap();
    let text = String::from_utf8(buf[..len as usize].to_vec()).unwrap();
    assert!(text.contains("</RegisterDescription>"), "well-formed document tail");
    assert!(text.contains("<Category Name=\"SmartCamera\""));
    assert!(text.contains("VgigeVersionReg"));
    assert!(text.contains("<IntReg Name=\"WidthReg\""), "camera node preserved");
    assert!(text.contains("DeviceModelName"), "camera identity nodes preserved");

    // First URL readback matches the served URL.
    let mut slot512 = [0u8; 512];
    s.remote.read_mem(reg::FIRST_URL, &mut slot512).unwrap();
    let end = slot512.iter().position(|&b| b == 0).unwrap();
    assert_eq!(&slot512[..end], url.as_bytes());

    // A 4-aligned read crossing the blob end is zero-padded (GenTL
    // §6.3.6.5 client-side alignment adaptation).
    let tail = XML_BASE_V + len - 2;
    let mut padded = [0xFFu8; 4];
    s.remote.read_mem(tail, &mut padded).unwrap();
    assert_eq!(padded[2], 0);
    assert_eq!(padded[3], 0);
}

#[test]
fn stream_relay_reemits_as_30_from_a_12_camera() {
    let s = start(true); // the sim emits the 1.2 legacy wire form

    // Virtual channel configuration is local (R-562st readback: exactly
    // what was written).
    s.remote
        .write_reg(reg::SCDA0, u32::from(Ipv4Addr::LOCALHOST))
        .unwrap();
    let total = 300 + reg::GVSP_HEADERS_OVERHEAD as u32;
    s.remote.write_reg(reg::SCPS0, reg::scps_make(total)).unwrap();
    assert_eq!(
        reg::scps_packet_size(s.remote.read_reg(reg::SCPS0).unwrap()),
        total
    );

    let rx = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    rx.set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    s.remote
        .write_reg(reg::SCP0, reg::scp_make(rx.local_addr().unwrap().port()))
        .unwrap();

    // Two complete blocks off the virtual wire.
    let first = recv_block(&rx);
    let second = recv_block(&rx);
    assert_eq!(first.0, 1, "R-259st: the virtual sequence restarts at 1");
    assert_eq!(second.0, 2, "block ids increment by one");
    assert!(first.1 > 0 && second.1 > 0);

    // Payload bytes are the sim's Mono8 pattern `(k + block) mod 256` for
    // some recent real block id — the relay forwards image bytes verbatim
    // under its own block numbering.
    for (vbid, payload) in [(first.0, &first.2), (second.0, &second.2)] {
        assert_eq!(payload.len(), (W * H) as usize, "block {vbid} payload size");
        let matches = (1..=8u64).any(|b| {
            payload
                .iter()
                .enumerate()
                .all(|(k, &byte)| byte == (k as u64 + b) as u8)
        });
        assert!(matches, "block {vbid} payload is not any recent sim pattern");
    }

    // The relay counter in the custom window tracks the forwarded frames.
    let relayed = s
        .remote
        .read_reg(CUSTOM_BASE + (slot::FRAMES_RELAYED as u32) * 4)
        .unwrap();
    assert!(relayed >= 2, "FrameCountRelayed = {relayed}");

    // A first Start may reopen the channel while it is already open.
    s.remote.write_reg(reg::SCP0, reg::scp_make(1)).unwrap();
    assert_eq!(s.remote.read_reg(reg::SCP0).unwrap(), reg::scp_make(1));

    // Close cascades: the real camera's channel is closed too (R-091ca).
    s.remote.write_reg(reg::SCP0, 0).unwrap();
    assert_eq!(s.remote.read_reg(reg::SCP0).unwrap(), 0);
    let scp0_on_camera = s.down.lock().unwrap().read_reg(reg::SCP0).unwrap();
    assert_eq!(scp0_on_camera, 0, "downstream channel closed by the cascade");

    // Nothing NEW keeps arriving on the receiver socket: datagrams already
    // in flight when the close was requested (the sim emits a block every
    // 1 ms, so a block or two sits in kernel buffers) are drained first —
    // the assertion targets traffic sent after the cascade stopped the
    // camera. Same flush-then-listen shape as the sim's emitter
    // `assert_silent` (the pre-refactor timing happened to leave the
    // buffer empty; the race was always there).
    rx.set_read_timeout(Some(Duration::from_millis(50))).unwrap();
    let mut buf = [0u8; 2048];
    while rx.recv_from(&mut buf).is_ok() {}
    rx.set_read_timeout(Some(Duration::from_millis(300))).unwrap();
    assert!(
        rx.recv_from(&mut buf).is_err(),
        "datagrams arriving after the channel closed"
    );
}

/// Receive one block from the virtual wire and return (block_id,
/// max_payload_packet_bytes, payload). Asserts the 3.0 wire form on the way:
/// the relay must always emit EI=1 20-byte headers and the 40-byte §25.2.1
/// leader body, whatever header form the real camera used.
fn recv_block(rx: &UdpSocket) -> (u64, usize, Vec<u8>) {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut payload = Vec::new();
    let mut max_payload_packet = 0usize;
    let mut buf = [0u8; 65536];
    let (block_id, _) = loop {
        assert!(Instant::now() < deadline, "timed out waiting for a block");
        let (n, _) = rx.recv_from(&mut buf).expect("gvsp datagram");
        let (hdr, hlen) = GvspHeader::decode_auto(&buf[..n]).expect("decodable gvsp header");
        assert!(
            hdr.extended_id,
            "the relay must emit the 3.0 extended form (EI=1, §24.2)"
        );
        let id = hdr.block_id;
        match hdr.packet_format {
            PACKET_FORMAT_DATA_LEADER => {
                let body = &buf[hlen..n];
                assert_eq!(
                    body.len(),
                    IMAGE_LEADER_BODY_LEN,
                    "relay must re-encode the leader in the 3.0 40-byte layout"
                );
                let leader = ImageLeader::decode(body).expect("leader body decodes");
                assert_eq!(leader.pixel_format, MONO8);
                assert_eq!(leader.size_x, W);
                assert_eq!(leader.size_y, H);
            }
            PACKET_FORMAT_DATA_PAYLOAD => {
                max_payload_packet = max_payload_packet.max(n - hlen);
                payload.extend_from_slice(&buf[hlen..n]);
            }
            PACKET_FORMAT_DATA_TRAILER => {
                assert_eq!(
                    decode_image_trailer_body(&buf[hlen..n]).unwrap(),
                    H,
                    "trailer rows match the leader"
                );
                break (id, ());
            }
            other => panic!("unexpected packet format {other}"),
        }
    };
    // The virtual SCPS0 asked for 348 total -> 300-byte payload packets.
    assert!(
        max_payload_packet <= 300,
        "payload packets respect the virtual SCPS0 (max {max_payload_packet})"
    );
    (block_id, max_payload_packet, payload)
}
