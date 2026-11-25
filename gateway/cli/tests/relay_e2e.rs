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
    ChunkEmit, SimCamera, SimConfig, StreamConfig, StreamingProtocol, REG_EXPOSURE_TIME,
    REG_TEST_EVENT_DATA_LEN, REG_TEST_EVENT_ID, REG_TEST_EVENT_TRIGGER, REG_WIDTH,
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
    start_with(legacy_stream, None)
}

/// Like [`start`] but lets the simulated camera emit extended-chunk GVSP
/// blocks (GigE Vision 3.0 §25.1) for the chunk-relay tests.
fn start_with(legacy_stream: bool, chunk: Option<ChunkEmit>) -> Stack {
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
            chunk,
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
    let xml = Arc::new(XmlMerger::new_with_endian(&raw_xml, None, true).expect("merge camera XML"));
    // GigE device events: the sim really has a message channel, and the
    // producer backend relays its events to whoever opens the virtual
    // device's message channel.
    let message = Arc::new(vgige_core::MessageRelay::new());
    {
        let mut backend = back.lock().expect("down lock");
        backend
            .start_event_relay(Arc::clone(&message))
            .expect("camera message channel relay");
    }
    let relay = Relay::new(
        Arc::clone(&back),
        true,
        Box::new(|regs| Box::new(GvspSession::new(regs))),
    );
    let mut space = RegisterSpace::new(
        Arc::clone(&back),
        Arc::clone(&xml),
        Ipv4Addr::LOCALHOST,
        UP_MAC,
        relay,
    );
    space.set_bootstrap(0x0003_0000, Ipv4Addr::new(255, 0, 0, 0));
    space.set_message_channel(Arc::clone(&message));
    let identity = assemble_identity(
        &back,
        &UpIdentity {
            ip: Ipv4Addr::LOCALHOST,
            mac: UP_MAC,
            mask: Ipv4Addr::new(255, 0, 0, 0),
        },
    )
    .expect("assemble identity");
    space.set_identity(&identity);
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
        0x0003_0000,
        "virtual device advertises the 3.0 extended GVSP identity"
    );
    assert_eq!(
        s.remote.read_reg(reg::DEVICE_MODE).unwrap(),
        0x8000_0001,
        "big-endian + UTF-8 + transmitter (1.2 bit 0 / 2.x deprecated MSB)"
    );
    assert_eq!(
        s.remote.read_reg(reg::NET_IF_CAPABILITY).unwrap(),
        reg::IP_CFG_DHCP | reg::IP_CFG_LLA,
        "§4.3/§4.4: DHCP and LLA support bits are always 1; the virtual device does not implement Persistent IP (§4.2)"
    );
    assert_eq!(
        s.remote.read_reg(reg::NET_IF_CONFIGURATION).unwrap(),
        reg::IP_CFG_LLA,
        "§4.4: LLA is the always-active default scheme"
    );
    // Capability registers describe THIS device, not the downstream camera
    // (audit P1-1): no message channel (events are not relayed), GVSP-only
    // streaming, and a GVCP capability set limited to the bootstrap strings
    // the virtual server serves.
    assert_eq!(
        s.remote.read_reg(reg::NUM_MESSAGE_CHANNELS).unwrap(),
        1,
        "the camera message channel is relayed"
    );
    assert_eq!(s.remote.read_reg(reg::MESSAGE_CHANNEL_CAPABILITY).unwrap(), 0);
    assert_eq!(
        s.remote.read_reg(reg::STREAMING_CAPABILITY).unwrap(),
        0,
        "§36.27: GVSP supported, GVRSP not"
    );
    assert_eq!(
        s.remote.read_reg(reg::STREAMING_CONFIGURATION).unwrap(),
        reg::SCONF_DEPRECATED_BIT,
        "§36.38: GVSP mode plus the deprecated always-1 bit"
    );
    assert_eq!(
        s.remote.read_reg(reg::GVCP_CAPABILITY).unwrap(),
        reg::GVCP_CAPABILITY_USER_DEFINED_NAME
            | reg::GVCP_CAPABILITY_SERIAL_NUMBER
            | reg::GVCP_CAPABILITY_PACKET_RESEND,
        "§36.29: UN/SN plus PACKETRESEND (answered from the front block cache); EVENT/ACTION stay unsupported"
    );
    // Optional bootstrap registers the virtual device does not implement
    // answer INVALID_ADDRESS instead of leaking the camera's configuration
    // (writing 0x064C would reconfigure the real camera's Persistent IP).
    for addr in [
        reg::PERSISTENT_IP_ADDRESS,
        reg::PERSISTENT_SUBNET_MASK,
        reg::PERSISTENT_DEFAULT_GATEWAY,
    ] {
        assert_eq!(
            gev(s.remote.read_reg(addr).unwrap_err()),
            GEV_STATUS_INVALID_ADDRESS,
            "register 0x{addr:04X} must not be forwarded"
        );
    }
    // Bootstrap identity strings are served locally: a forwarded READMEM of
    // a camera string would be word-swapped by a little-endian backend.
    let mut mfg = [0u8; 32];
    s.remote.read_mem(reg::MANUFACTURER_NAME, &mut mfg).unwrap();
    let mfg_len = mfg.iter().position(|&b| b == 0).unwrap_or(mfg.len());
    assert_eq!(
        std::str::from_utf8(&mfg[..mfg_len]).unwrap(),
        "GenTLite",
        "discovery identity and bootstrap registers must agree"
    );
    let mut model = [0u8; 32];
    s.remote.read_mem(reg::MODEL_NAME, &mut model).unwrap();
    let model_len = model.iter().position(|&b| b == 0).unwrap_or(model.len());
    assert_eq!(
        std::str::from_utf8(&model[..model_len]).unwrap(),
        "SimCam",
        "the virtual model mirrors the discovery payload (the CLI appends the GigE suffix)"
    );
    // USER_DEFINED_NAME (§36.16) is writable and stored locally.
    s.remote
        .write_reg(reg::USER_DEFINED_NAME, u32::from_be_bytes(*b"vgig"))
        .unwrap();
    let mut user = [0u8; 16];
    s.remote.read_mem(reg::USER_DEFINED_NAME, &mut user).unwrap();
    assert_eq!(&user[..4], b"vgig");
    // The other identity strings are read-only.
    assert_eq!(
        gev(s.remote.write_reg(reg::MANUFACTURER_NAME, 0).unwrap_err()),
        GEV_STATUS_WRITE_PROTECT,
        "read-only bootstrap string"
    );
    assert_eq!(
        s.remote.read_reg(reg::CURRENT_SUBNET_MASK).unwrap(),
        u32::from(Ipv4Addr::new(255, 0, 0, 0))
    );
    assert_eq!(s.remote.read_reg(reg::NUM_NETWORK_INTERFACES).unwrap(), 1);

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

    // GVCP capability is virtualized: UN/SN plus PACKETRESEND, which the
    // front really answers from its block cache (16.7, R-197ca).
    let cap = s.remote.read_reg(reg::GVCP_CAPABILITY).unwrap();
    assert_ne!(cap & reg::GVCP_CAPABILITY_PACKET_RESEND, 0);
    assert_ne!(cap & 0xC000_0000, 0, "UN/SN capability bits survive");
    assert_eq!(cap & reg::GVCP_CAPABILITY_TEST_DATA, 0, "no test-data bit");

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

    // R-069cd: repeats of the same request are accepted without a new
    // connection; R-070cd: the primary switches privilege in place.
    s.remote
        .write_reg(reg::CCP, reg::CCP_EXCLUSIVE_ACCESS)
        .unwrap();
    s.remote
        .write_reg(reg::CCP, reg::CCP_EXCLUSIVE_ACCESS)
        .unwrap();
    s.remote
        .write_reg(reg::CCP, reg::CCP_CONTROL_ACCESS)
        .unwrap();
    assert_eq!(
        s.remote.read_reg(reg::CCP).unwrap(),
        reg::CCP_CONTROL_ACCESS,
        "the primary switched EA -> CA without closing the channel"
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
    assert!(url.starts_with("local:vgige-"), "virtual URL: {url}");
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
    // The GigE personality carries the standard transport nodes even though
    // the down camera is a (GigE) sim whose XML does not: the virtual
    // channel is what serves these registers.
    for (node, addr) in [
        ("GevSCPSPacketSize", 0x0D04u32),
        ("GevSCPHostPort", 0x0D00),
        ("GevSCDA", 0x0D18),
        ("GevSCPD", 0x0D08),
    ] {
        assert!(text.contains(&format!("Name=\"{node}\"")), "missing {node}");
        let reg_name = format!("{node}_Reg");
        let at = text.find(&format!("Name=\"{reg_name}\"")).unwrap();
        assert!(
            text[at..at + 240].contains(&format!("Address>0x{addr:08X}</Address>")),
            "{reg_name} is not mapped to 0x{addr:08X}"
        );
    }

    // First URL readback matches the served URL.
    let mut slot512 = [0u8; 512];
    s.remote.read_mem(reg::FIRST_URL, &mut slot512).unwrap();
    let end = slot512.iter().position(|&b| b == 0).unwrap();
    assert_eq!(&slot512[..end], url.as_bytes());

    // A 4-aligned read crossing the blob end is zero-padded (GenTL
    // §6.3.6.5 client-side alignment adaptation).
    let tail = (XML_BASE_V + len - 2) / 4 * 4;
    let mut padded = [0xFFu8; 4];
    s.remote.read_mem(tail, &mut padded).unwrap();
    let start = (tail - XML_BASE_V) as usize;
    for (i, byte) in padded.iter().enumerate() {
        if start + i >= len as usize {
            assert_eq!(*byte, 0, "byte {i} past the blob end is zero-padded");
        }
    }
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
    assert_eq!(
        gev(s.remote.write_reg(reg::SCP0, reg::scp_make(1)).unwrap_err()),
        GEV_STATUS_BUSY,
        "R-586cd: non-zero SCPx write while streaming is BUSY"
    );

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

#[test]
fn scps_write_while_streaming_is_busy() {
    let s = start(false);
    s.remote
        .write_reg(reg::SCDA0, u32::from(Ipv4Addr::LOCALHOST))
        .unwrap();
    // Pre-open configuration is allowed (§11.7 window); the recv helper
    // expects the same 300-byte payload packets as the other stream test.
    s.remote
        .write_reg(
            reg::SCPS0,
            reg::scps_make(300 + reg::GVSP_HEADERS_OVERHEAD as u32),
        )
        .unwrap();

    let rx = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    rx.set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    s.remote
        .write_reg(reg::SCP0, reg::scp_make(rx.local_addr().unwrap().port()))
        .unwrap();

    // One complete block proves transmission is active on the channel.
    let _ = recv_block(&rx);
    let before = s.remote.read_reg(reg::SCPS0).unwrap();

    // §36.58: write access is not possible while streaming. Both a plain
    // size write and a test-packet request answer GEV_STATUS_BUSY and leave
    // the register unchanged (no mid-stream packet-size switch).
    assert_eq!(
        gev(s.remote
            .write_reg(reg::SCPS0, reg::scps_make(2000))
            .unwrap_err()),
        GEV_STATUS_BUSY
    );
    assert_eq!(
        gev(s.remote
            .write_reg(
                reg::SCPS0,
                reg::scps_make(2000)
                    | reg::SCPS_FIRE_TEST_PACKET
                    | reg::SCPS_DO_NOT_FRAGMENT
            )
            .unwrap_err()),
        GEV_STATUS_BUSY
    );
    assert_eq!(s.remote.read_reg(reg::SCPS0).unwrap(), before);

    // Closing the channel ends transmission; the same write is accepted
    // again for the next session.
    s.remote.write_reg(reg::SCP0, 0).unwrap();
    s.remote
        .write_reg(reg::SCPS0, reg::scps_make(2000))
        .unwrap();
    assert_eq!(
        reg::scps_packet_size(s.remote.read_reg(reg::SCPS0).unwrap()),
        2000
    );
}

/// §36.62/§36.63 (CR-494cd/CR-495cd): the virtual GigE device must expose
/// SCC0/SCCFG0 for each stream channel. It implements no optional
/// per-channel feature, so SCC0 reads 0, SCCFG0 accepts only a zero write,
/// and R-590cd disables the write while streaming.
#[test]
fn stream_channel_capability_and_config_are_virtualized() {
    let s = start(false);
    assert_eq!(s.remote.read_reg(reg::SCC0).unwrap(), 0);
    assert_eq!(s.remote.read_reg(reg::SCCFG0).unwrap(), 0);

    // Capability register: read-only (§36.62).
    assert_eq!(
        gev(s.remote.write_reg(reg::SCC0, 0).unwrap_err()),
        GEV_STATUS_WRITE_PROTECT
    );
    // Configuration register: every bit reserved, only zero accepted.
    s.remote.write_reg(reg::SCCFG0, 0).unwrap();
    assert_eq!(
        gev(s.remote.write_reg(reg::SCCFG0, 1).unwrap_err()),
        GEV_STATUS_INVALID_PARAMETER
    );

    // R-590cd: write access is disabled while the channel streams.
    s.remote
        .write_reg(
            reg::SCPS0,
            reg::scps_make(300 + reg::GVSP_HEADERS_OVERHEAD as u32),
        )
        .unwrap();
    s.remote
        .write_reg(reg::SCDA0, u32::from(Ipv4Addr::LOCALHOST))
        .unwrap();
    let rx = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    rx.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    s.remote
        .write_reg(reg::SCP0, reg::scp_make(rx.local_addr().unwrap().port()))
        .unwrap();
    let _ = recv_block(&rx);
    assert_eq!(
        gev(s.remote.write_reg(reg::SCCFG0, 0).unwrap_err()),
        GEV_STATUS_BUSY
    );
    s.remote.write_reg(reg::SCP0, 0).unwrap();
    s.remote.write_reg(reg::SCCFG0, 0).unwrap();
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
        let id = hdr.block_id;
        match hdr.packet_format {
            PACKET_FORMAT_DATA_LEADER => {
                let body = &buf[hlen..n];
                let leader = if hdr.extended_id {
                    assert_eq!(body.len(), IMAGE_LEADER_BODY_LEN, "3.0 leader body");
                    ImageLeader::decode(body).expect("leader body decodes")
                } else {
                    assert_eq!(
                        body.len(),
                        IMAGE_LEADER_LEGACY_BODY_LEN,
                        "1.2 legacy leader body"
                    );
                    ImageLeader::decode_legacy(body).expect("legacy leader body decodes")
                };
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
    // The virtual SCPS0 asks for 348 total (3.0 48-byte overhead)
    // -> 300-byte payload packets.
    assert!(
        max_payload_packet <= 300,
        "payload packets respect the virtual SCPS0 (max {max_payload_packet})"
    );
    (block_id, max_payload_packet, payload)
}

/// §16.2 FORCEIP on the virtual device: the relay cannot rebind the host's
/// up-facing NIC, so a request for its own MAC is deliberately discarded
/// (no ACK, no empty datagram) instead of pretending the address changed —
/// design decision in docs/00-governance/OPEN-QUESTIONS.md Q40. Requests for
/// other MACs are discarded by the same rule (R-149cd).
/// R-152cd: the gateway must not ACK an address it cannot make effective.
#[test]
fn force_ip_does_not_ack_an_unreachable_address() {
    let s = start(false);
    let up_port = s._up_server.local_port().unwrap();
    let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_millis(400)))
        .unwrap();
    let request = ForceIpRequest {
        mac: UP_MAC,
        static_ip: u32::from(Ipv4Addr::new(192, 0, 2, 99)),
        subnet_mask: u32::from(Ipv4Addr::new(255, 255, 255, 0)),
        default_gateway: 0,
    };
    let payload = encode_force_ip_payload(&request);
    let mut datagram = vec![0u8; HEADER_LEN];
    let mut header = [0u8; HEADER_LEN];
    CmdHeader {
        flag: FLAG_ACK,
        command: FORCEIP_CMD,
        length: payload.len() as u16,
        req_id: 3,
    }
    .encode(&mut header);
    datagram.copy_from_slice(&header);
    datagram.extend_from_slice(&payload);
    socket
        .send_to(&datagram, (Ipv4Addr::LOCALHOST, up_port))
        .unwrap();
    assert!(
        socket.recv(&mut [0u8; 256]).is_err(),
        "no FORCEIP_ACK before the address is effective"
    );
    let client = GvcpClient::connect(Ipv4Addr::LOCALHOST, up_port).unwrap();
    assert_eq!(
        client.read_reg(reg::CURRENT_IP).unwrap(),
        u32::from(Ipv4Addr::LOCALHOST)
    );
}

/// R-152cd: an already-effective local address can be ACKed.
#[test]
fn force_ip_acks_an_already_effective_address() {
    let s = start(false);
    let up_port = s._up_server.local_port().unwrap();
    let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let request = ForceIpRequest {
        mac: UP_MAC,
        static_ip: u32::from(Ipv4Addr::LOCALHOST),
        subnet_mask: 0,
        default_gateway: 0,
    };
    let payload = encode_force_ip_payload(&request);
    let mut datagram = vec![0u8; HEADER_LEN + payload.len()];
    let mut header = [0u8; HEADER_LEN];
    CmdHeader {
        flag: FLAG_ACK,
        command: FORCEIP_CMD,
        length: payload.len() as u16,
        req_id: 4,
    }
    .encode(&mut header);
    datagram[..HEADER_LEN].copy_from_slice(&header);
    datagram[HEADER_LEN..].copy_from_slice(&payload);
    socket
        .send_to(&datagram, (Ipv4Addr::LOCALHOST, up_port))
        .unwrap();
    let mut reply = [0u8; 256];
    let n = socket.recv(&mut reply).expect("acknowledged local address");
    assert_eq!(u16::from_be_bytes([reply[0], reply[1]]), GEV_STATUS_SUCCESS);
    assert!(n >= HEADER_LEN);
}

/// Receive one complete GVSP block from the virtual wire, keeping the leader
/// and trailer bodies for protocol assertions: `(leader, payload, trailer)`.
fn recv_chunk_block(rx: &UdpSocket) -> (ImageLeader, Vec<u8>, Vec<u8>) {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut payload = Vec::new();
    let mut buf = [0u8; 65536];
    let mut leader = None;
    loop {
        assert!(Instant::now() < deadline, "timed out waiting for a chunk block");
        let (n, _) = rx.recv_from(&mut buf).expect("gvsp datagram");
        let (hdr, hlen) = GvspHeader::decode_auto(&buf[..n]).expect("decodable gvsp header");
        match hdr.packet_format {
            PACKET_FORMAT_DATA_LEADER => {
                leader = Some(ImageLeader::decode(&buf[hlen..n]).expect("leader decodes"));
            }
            PACKET_FORMAT_DATA_PAYLOAD => payload.extend_from_slice(&buf[hlen..n]),
            PACKET_FORMAT_DATA_TRAILER => {
                return (
                    leader.expect("leader before trailer"),
                    payload,
                    buf[hlen..n].to_vec(),
                );
            }
            other => panic!("unexpected packet format {other}"),
        }
    }
}

/// §25.1 chunk relay: a camera delivering an extended-chunk GVSP block must
/// reach the remote host unchanged — extended payload type, extended trailer
/// (chunk block length + layout id) and the verbatim chunk-formatted payload.

#[test]
fn stream_relay_reemits_extended_chunk_blocks() {
    let s = start_with(
        false,
        Some(ChunkEmit {
            id: 0x2222,
            len: 8,
            layout_id: 0x77,
        }),
    );
    s.remote
        .write_reg(reg::SCDA0, u32::from(Ipv4Addr::LOCALHOST))
        .unwrap();
    let total = 300 + reg::GVSP_HEADERS_OVERHEAD as u32;
    s.remote.write_reg(reg::SCPS0, reg::scps_make(total)).unwrap();
    let rx = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    rx.set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    s.remote
        .write_reg(reg::SCP0, reg::scp_make(rx.local_addr().unwrap().port()))
        .unwrap();

    let (leader, payload, trailer) = recv_chunk_block(&rx);
    assert!(leader.is_extended_chunk(), "§25.1 extended-chunk payload type");
    assert_eq!(leader.payload_type, PAYLOAD_TYPE_IMAGE | PAYLOAD_TYPE_EXTENDED_CHUNK_BIT);
    assert_eq!(leader.size_x, W);
    assert_eq!(leader.size_y, H);
    let (size_y, chunk_len, layout_id) =
        decode_image_extended_trailer_body(&trailer).expect("§25.1 Figure 25-1 trailer");
    assert_eq!(size_y, H, "trailer rows match the leader");
    assert_eq!(layout_id, 0x77, "chunk layout id relayed");
    assert_eq!(chunk_len, 16, "chunk block = 8 data bytes + 8-byte tag");

    // Payload: image (Mono8 W*H, already 4-byte aligned) + image chunk tag +
    // metadata chunk block (§24.4: data then CID/length, network byte order).
    let image_len = (W * H) as usize;
    assert_eq!(payload.len(), image_len + 8 + 16);
    assert_eq!(
        u32::from_be_bytes(payload[image_len..image_len + 4].try_into().unwrap()),
        1,
        "image chunk CID"
    );
    assert_eq!(
        u32::from_be_bytes(payload[image_len + 4..image_len + 8].try_into().unwrap()),
        image_len as u32,
        "image chunk length"
    );
    let data_at = image_len + 8;
    let tag_at = data_at + 8;
    assert_eq!(
        u32::from_be_bytes(payload[tag_at..tag_at + 4].try_into().unwrap()),
        0x2222,
        "metadata chunk CID"
    );
    assert_eq!(
        u32::from_be_bytes(payload[tag_at + 4..tag_at + 8].try_into().unwrap()),
        8,
        "metadata chunk length"
    );
    // Image bytes are the sim's pattern for a recent camera block id — the
    // relay does not reinterpret or truncate them.
    let matches = (1..=8u64).any(|b| {
        payload[..image_len]
            .iter()
            .enumerate()
            .all(|(k, &byte)| byte == (k as u64 + b) as u8)
    });
    assert!(matches, "relayed image bytes are verbatim");

    s.remote.write_reg(reg::SCP0, 0).unwrap();
}

/// 16.7 PACKETRESEND on the virtual device: the front caches recent blocks
/// and answers a retransmission request with packets marked
/// GEV_STATUS_PACKET_RESEND + GEV_FLAG_PACKET_RESEND (CR-204st); a block that
/// is no longer cached answers a header-only GEV_STATUS_PACKET_UNAVAILABLE
/// packet (CR-205st) instead of silence.
#[test]
fn packet_resend_is_answered_from_the_block_cache() {
    let s = start(false);
    s.remote
        .write_reg(reg::SCDA0, u32::from(Ipv4Addr::LOCALHOST))
        .unwrap();
    let total = 300 + reg::GVSP_HEADERS_OVERHEAD as u32;
    s.remote.write_reg(reg::SCPS0, reg::scps_make(total)).unwrap();
    let rx = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    rx.set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    s.remote
        .write_reg(reg::SCP0, reg::scp_make(rx.local_addr().unwrap().port()))
        .unwrap();

    // Receive one complete block and remember its id + a payload packet id.
    let mut block_id = 0u64;
    let mut payload_id = 0u32;
    let mut buf = [0u8; 65536];
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        assert!(Instant::now() < deadline, "no complete block received");
        let (n, _) = rx.recv_from(&mut buf).expect("gvsp datagram");
        let (hdr, _) = GvspHeader::decode_auto(&buf[..n]).expect("gvsp header");
        match hdr.packet_format {
            PACKET_FORMAT_DATA_PAYLOAD => {
                block_id = hdr.block_id;
                payload_id = hdr.packet_id;
            }
            PACKET_FORMAT_DATA_TRAILER if block_id != 0 => break,
            _ => {}
        }
    }

    // Ask for the captured payload packet again.
    s.remote.packet_resend(0, block_id, payload_id, payload_id);
    let mut resent_flag = None;
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        rx.set_read_timeout(Some(Duration::from_millis(200))).ok();
        let Ok((n, _)) = rx.recv_from(&mut buf) else {
            continue;
        };
        let Some((hdr, _)) = GvspHeader::decode_auto(&buf[..n]) else {
            continue;
        };
        if hdr.block_id == block_id
            && hdr.packet_id == payload_id
            && hdr.status == GEV_STATUS_PACKET_RESEND
        {
            resent_flag = Some(hdr.flag);
            break;
        }
    }
    let flag = resent_flag.expect("resent datagram within 5 s");
    assert_ne!(
        flag & GEV_FLAG_PACKET_RESEND,
        0,
        "CR-204st: resent packets carry the RESEND flag"
    );

    // An unknown block is answered with a header-only PACKET_UNAVAILABLE.
    let unknown = block_id.wrapping_add(1_000_000);
    s.remote.packet_resend(0, unknown, 0, 0);
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut unavailable = false;
    while Instant::now() < deadline {
        rx.set_read_timeout(Some(Duration::from_millis(200))).ok();
        let Ok((n, _)) = rx.recv_from(&mut buf) else {
            continue;
        };
        let Some((hdr, hlen)) = GvspHeader::decode_auto(&buf[..n]) else {
            continue;
        };
        if hdr.status == GEV_STATUS_PACKET_UNAVAILABLE {
            assert_eq!(hlen, n, "header-only error packet (CR-205st)");
            assert_eq!(hdr.block_id, unknown);
            unavailable = true;
            break;
        }
    }
    assert!(unavailable, "PACKET_UNAVAILABLE answer missing");

    s.remote.write_reg(reg::SCP0, 0).unwrap();
}

/// GigE Vision 3.0 §12/§17 device events: the virtual device advertises one
/// message channel when the downstream camera has one; MCDA + MCP open it
/// (§12.2), the camera's EVENT_CMD is forwarded verbatim, and MCP = 0 closes
/// it (§12.4).
#[test]
fn device_events_are_relayed_to_the_opened_message_channel() {
    let s = start(false);
    assert_eq!(
        s.remote.read_reg(reg::NUM_MESSAGE_CHANNELS).unwrap(),
        1,
        "§36.22: one relayed message channel"
    );

    let rx = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    rx.set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    s.remote
        .write_reg(
            reg::MESSAGE_CHANNEL_DEST_ADDRESS,
            u32::from(Ipv4Addr::LOCALHOST),
        )
        .unwrap();
    s.remote
        .write_reg(
            reg::MESSAGE_CHANNEL_PORT,
            reg::mcp_make(rx.local_addr().unwrap().port()),
        )
        .unwrap();

    // Trigger one EVENT_CMD on the simulated camera.
    {
        let mut down = s.down.lock().expect("down lock");
        down.write_reg(REG_TEST_EVENT_ID, 7).unwrap();
        down.write_reg(REG_TEST_EVENT_DATA_LEN, 0).unwrap();
        down.write_reg(REG_TEST_EVENT_TRIGGER, 1).unwrap();
    }
    let mut buf = [0u8; 65536];
    let (n, _) = rx.recv_from(&mut buf).expect("forwarded event datagram");
    let events = decode_message_events(&buf[..n]).expect("EVENT_CMD decodes");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event_id, 7, "event id passes through unchanged");
    assert_eq!(events[0].stream_channel, 0xFFFF);

    // §12.4: closing the channel (MCP = 0) stops the forwarding.
    s.remote.write_reg(reg::MESSAGE_CHANNEL_PORT, 0).unwrap();
    std::thread::sleep(Duration::from_millis(150));
    {
        let mut down = s.down.lock().expect("down lock");
        down.write_reg(REG_TEST_EVENT_TRIGGER, 1).unwrap();
    }
    rx.set_read_timeout(Some(Duration::from_millis(400))).ok();
    assert!(
        rx.recv_from(&mut buf).is_err(),
        "no events after the host closed the message channel"
    );
}

/// GigE Vision 16.5 allows a READMEM/WRITEMEM to cover several contiguous
/// registers (32-bit alignment only). Words served by the virtual device
/// must be answered locally instead of forwarding the whole block to the
/// camera: SCPS0+SCPD0 and SCC0+SCCFG0 are adjacent virtual channel words.
#[test]
fn multi_word_block_access_to_virtual_registers() {
    let s = start(false);
    let scps_value = reg::scps_make(1500);
    s.remote.write_reg(reg::SCPS0, scps_value).unwrap();
    let mut pair = [0u8; 8];
    s.remote.read_mem(reg::SCPS0, &mut pair).unwrap();
    assert_eq!(
        &pair[0..4],
        &scps_value.to_be_bytes(),
        "SCPS0 word is served locally"
    );
    assert_eq!(&pair[4..8], &0u32.to_be_bytes(), "SCPD0 word is served locally");

    let mut scc = [0u8; 8];
    s.remote.read_mem(reg::SCC0, &mut scc).unwrap();
    assert_eq!(scc, [0u8; 8], "SCC0 + SCCFG0 are local zeros");

    // The same block size on the read-only SCC0 must be rejected by the
    // local dispatch (WRITE_PROTECT), not forwarded to the camera.
    let err = s.remote.write_mem(reg::SCC0, &[0u8; 8]).unwrap_err();
    assert_eq!(
        err,
        GentlError::GevStatus(GEV_STATUS_WRITE_PROTECT),
        "virtual read-only register answers WRITE_PROTECT locally"
    );
}

/// [R-151cd]: the FORCEIP restart path (static_IP == 0) must close the
/// message channel too: MCDA/MCP read back zero and the previously opened
/// host stops receiving relayed events.
#[test]
fn force_ip_restart_closes_the_virtual_message_channel() {
    let s = start(false);
    let rx = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    rx.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    s.remote
        .write_reg(
            reg::MESSAGE_CHANNEL_DEST_ADDRESS,
            u32::from(Ipv4Addr::LOCALHOST),
        )
        .unwrap();
    s.remote
        .write_reg(
            reg::MESSAGE_CHANNEL_PORT,
            reg::mcp_make(rx.local_addr().unwrap().port()),
        )
        .unwrap();
    {
        let mut down = s.down.lock().expect("down lock");
        down.write_reg(REG_TEST_EVENT_ID, 7).unwrap();
        down.write_reg(REG_TEST_EVENT_DATA_LEN, 0).unwrap();
        down.write_reg(REG_TEST_EVENT_TRIGGER, 1).unwrap();
    }
    let mut buf = [0u8; 65536];
    assert!(rx.recv_from(&mut buf).is_ok(), "event relayed before FORCEIP");

    let up_port = s._up_server.local_port().unwrap();
    let request = ForceIpRequest {
        mac: UP_MAC,
        static_ip: 0,
        subnet_mask: 0,
        default_gateway: 0,
    };
    let payload = encode_force_ip_payload(&request);
    let mut datagram = vec![0u8; HEADER_LEN];
    let mut header = [0u8; HEADER_LEN];
    CmdHeader {
        flag: FLAG_ACK,
        command: FORCEIP_CMD,
        length: payload.len() as u16,
        req_id: 9,
    }
    .encode(&mut header);
    datagram.copy_from_slice(&header);
    datagram.extend_from_slice(&payload);
    let tx = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    tx.send_to(&datagram, (Ipv4Addr::LOCALHOST, up_port)).unwrap();

    assert_eq!(
        s.remote.read_reg(reg::MESSAGE_CHANNEL_PORT).unwrap(),
        0,
        "MCP cleared by the restart"
    );
    assert_eq!(
        s.remote
            .read_reg(reg::MESSAGE_CHANNEL_DEST_ADDRESS)
            .unwrap(),
        0,
        "MCDA cleared by the restart"
    );
    {
        let mut down = s.down.lock().expect("down lock");
        down.write_reg(REG_TEST_EVENT_TRIGGER, 1).unwrap();
    }
    rx.set_read_timeout(Some(Duration::from_millis(400))).ok();
    assert!(
        rx.recv_from(&mut buf).is_err(),
        "no events after the FORCEIP restart closed the message channel"
    );
}

/// R-562st rounding and the R-586cd/R-588cd in-stream write lock of the
/// virtual stream channel: SCPSx readback reports the value actually used,
/// and opening the stream freezes SCPx (non-zero writes are BUSY) and SCDAx.
#[test]
fn stream_registers_round_and_freeze_while_streaming() {
    let s = start(false);
    let min = reg::GVSP_HEADERS_OVERHEAD + 4;
    s.remote.write_reg(reg::SCPS0, reg::scps_make(3)).unwrap();
    assert_eq!(
        reg::scps_packet_size(s.remote.read_reg(reg::SCPS0).unwrap()),
        min,
        "smaller than the minimum rounds up"
    );
    s.remote.write_reg(reg::SCPS0, reg::scps_make(65_535)).unwrap();
    assert_eq!(
        reg::scps_packet_size(s.remote.read_reg(reg::SCPS0).unwrap()),
        65_532,
        "larger than the maximum rounds down to a supported aligned size"
    );
    s.remote.write_reg(reg::SCPS0, reg::scps_make(2001)).unwrap();
    assert_eq!(
        reg::scps_packet_size(s.remote.read_reg(reg::SCPS0).unwrap()),
        2000,
        "unaligned size rounds down to the nearest supported value"
    );

    // Open the channel with the packet size the recv helper expects.
    s.remote
        .write_reg(
            reg::SCPS0,
            reg::scps_make(300 + reg::GVSP_HEADERS_OVERHEAD as u32),
        )
        .unwrap();
    s.remote
        .write_reg(reg::SCDA0, u32::from(Ipv4Addr::LOCALHOST))
        .unwrap();
    let rx = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    rx.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    s.remote
        .write_reg(reg::SCP0, reg::scp_make(rx.local_addr().unwrap().port()))
        .unwrap();
    let _ = recv_block(&rx);

    // R-586cd: a non-zero SCPx write while streaming is BUSY (only zero,
    // i.e. closing, is accepted).
    assert_eq!(
        gev(s.remote
            .write_reg(reg::SCP0, reg::scp_make(9999))
            .unwrap_err()),
        GEV_STATUS_BUSY
    );
    // R-588cd: SCDAx is frozen while streaming.
    assert_eq!(
        gev(s.remote
            .write_reg(reg::SCDA0, u32::from(Ipv4Addr::LOCALHOST))
            .unwrap_err()),
        GEV_STATUS_BUSY
    );
    // Closing is still allowed.
    s.remote.write_reg(reg::SCP0, 0).unwrap();
}

/// GigE Vision 10.1/R-072cd: exclusive access denies secondary applications,
/// the primary switches EA<->CA in place, control access allows secondary
/// reads only, and releasing the channel clears MCP/SCPx.
#[test]
fn peer_permissions_and_ccp_release_cleanup() {
    let s = start(false);
    let up_port = s._up_server.local_port().unwrap();
    let secondary = GvcpClient::connect(Ipv4Addr::LOCALHOST, up_port).unwrap();

    s.remote
        .write_reg(reg::CCP, reg::CCP_EXCLUSIVE_ACCESS)
        .unwrap();
    assert_eq!(
        gev(secondary.read_reg(reg::VERSION).unwrap_err()),
        GEV_STATUS_ACCESS_DENIED,
        "R-063cd: exclusive access denies secondary reads"
    );
    assert_eq!(
        gev(secondary
            .write_reg(reg::USER_DEFINED_NAME, 1)
            .unwrap_err()),
        GEV_STATUS_ACCESS_DENIED
    );

    // R-070cd: the primary switches to control access in place.
    s.remote
        .write_reg(reg::CCP, reg::CCP_CONTROL_ACCESS)
        .unwrap();
    assert!(
        secondary.read_reg(reg::VERSION).is_ok(),
        "control access allows secondary reads"
    );
    assert_eq!(
        gev(secondary
            .write_reg(reg::USER_DEFINED_NAME, 1)
            .unwrap_err()),
        GEV_STATUS_ACCESS_DENIED,
        "R-170cd: only the primary may write"
    );

    // Open message/stream channels as the primary, then release the channel.
    let rx = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    s.remote
        .write_reg(
            reg::MESSAGE_CHANNEL_DEST_ADDRESS,
            u32::from(Ipv4Addr::LOCALHOST),
        )
        .unwrap();
    s.remote
        .write_reg(
            reg::MESSAGE_CHANNEL_PORT,
            reg::mcp_make(rx.local_addr().unwrap().port()),
        )
        .unwrap();
    s.remote
        .write_reg(reg::SCDA0, u32::from(Ipv4Addr::LOCALHOST))
        .unwrap();
    s.remote
        .write_reg(reg::SCP0, reg::scp_make(rx.local_addr().unwrap().port()))
        .unwrap();
    s.remote.write_reg(reg::CCP, 0).unwrap();

    assert_eq!(
        secondary.read_reg(reg::MESSAGE_CHANNEL_PORT).unwrap(),
        0,
        "R-072cd: MCP cleared on control-channel close"
    );
    assert_eq!(
        secondary.read_reg(reg::SCP0).unwrap(),
        0,
        "R-072cd: SCPx cleared on control-channel close"
    );
}

/// Raw GVCP WRITEMEM so the test can inspect the ACK SCD (status + index).
fn raw_writemem(up_port: u16, addr: u32, data: &[u8]) -> (u16, Vec<u8>) {
    let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut header = [0u8; HEADER_LEN];
    CmdHeader {
        flag: FLAG_ACK,
        command: WRITEMEM_CMD,
        length: (4 + data.len()) as u16,
        req_id: 42,
    }
    .encode(&mut header);
    let mut datagram = header.to_vec();
    datagram.extend_from_slice(&addr.to_be_bytes());
    datagram.extend_from_slice(data);
    socket
        .send_to(&datagram, (Ipv4Addr::LOCALHOST, up_port))
        .unwrap();
    let mut reply = [0u8; 256];
    let (n, _) = socket.recv_from(&mut reply).unwrap();
    let status = u16::from_be_bytes([reply[0], reply[1]]);
    (status, reply[HEADER_LEN..n].to_vec())
}

/// R-194cd: a multi-word WRITEMEM over the virtual device that commits the
/// last custom-window slot and then fails past the window reports index 4.
#[test]
fn writemem_failure_reports_the_committed_byte_index() {
    let s = start(false);
    let up_port = s._up_server.local_port().unwrap();
    let addr = CUSTOM_BASE + (vgige_core::CUSTOM_SLOTS as u32 - 1) * 4;
    let data = [0u8; 8];
    let (status, scd) = raw_writemem(up_port, addr, &data);
    assert_eq!(status, GEV_STATUS_INVALID_ADDRESS);
    assert_eq!(scd, [0, 0, 0, 4], "index points at the failing byte");
    // The first slot was committed before the failure.
    assert_eq!(
        s.remote.read_reg(addr).unwrap(),
        0
    );
}

/// R-076cd/R-150cd: a FORCEIP arriving after the heartbeat expired must see
/// the cleared grant and close the relay's stream/message channel, without
/// relying on a later ordinary command to trigger expiry.
#[test]
fn force_ip_sees_an_expired_heartbeat() {
    let s = start(false);
    s.remote.write_reg(reg::HEARTBEAT_TIMEOUT, 500).unwrap();
    s.remote
        .write_reg(reg::CCP, reg::CCP_EXCLUSIVE_ACCESS)
        .unwrap();
    let rx = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    s.remote
        .write_reg(reg::SCDA0, u32::from(Ipv4Addr::LOCALHOST))
        .unwrap();
    s.remote
        .write_reg(reg::SCP0, reg::scp_make(rx.local_addr().unwrap().port()))
        .unwrap();
    {
        let mut down = s.down.lock().expect("down lock");
        assert_ne!(down.read_reg(reg::SCP0).unwrap(), 0, "relay opened");
    }

    std::thread::sleep(Duration::from_millis(700));
    let up_port = s._up_server.local_port().unwrap();
    let request = ForceIpRequest {
        mac: UP_MAC,
        static_ip: 0,
        subnet_mask: 0,
        default_gateway: 0,
    };
    let payload = encode_force_ip_payload(&request);
    let mut header = [0u8; HEADER_LEN];
    CmdHeader {
        flag: FLAG_ACK,
        command: FORCEIP_CMD,
        length: payload.len() as u16,
        req_id: 77,
    }
    .encode(&mut header);
    let tx = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let mut datagram = header.to_vec();
    datagram.extend_from_slice(&payload);
    tx.send_to(&datagram, (Ipv4Addr::LOCALHOST, up_port)).unwrap();

    // No up-facing command is issued here: the downstream state is the
    // observable proof that FORCEIP performed the expiry cleanup itself.
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        let scp = {
            let mut down = s.down.lock().expect("down lock");
            down.read_reg(reg::SCP0).unwrap()
        };
        if scp == 0 {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "expired grant + FORCEIP restart closed the downstream stream (SCP0={scp:#x})"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// R-076cd: the device closes the control channel after the heartbeat
/// timeout even if no further command ever arrives (independent watchdog).
#[test]
fn heartbeat_watchdog_closes_stream_without_another_command() {
    let s = start(false);
    s.remote.write_reg(reg::HEARTBEAT_TIMEOUT, 500).unwrap();
    s.remote
        .write_reg(reg::CCP, reg::CCP_EXCLUSIVE_ACCESS)
        .unwrap();
    let rx = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    s.remote
        .write_reg(reg::SCDA0, u32::from(Ipv4Addr::LOCALHOST))
        .unwrap();
    s.remote
        .write_reg(reg::SCP0, reg::scp_make(rx.local_addr().unwrap().port()))
        .unwrap();
    assert_ne!(
        s.down.lock().expect("down lock").read_reg(reg::SCP0).unwrap(),
        0,
        "relay opened"
    );

    // No GVCP command is sent during this window.
    std::thread::sleep(Duration::from_millis(800));
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        let scp = s
            .down
            .lock()
            .expect("down lock")
            .read_reg(reg::SCP0)
            .unwrap();
        if scp == 0 {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "heartbeat watchdog did not close the stream (SCP0={scp:#x})"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// R-210cd/R-472cd: opening the downstream stream is announced with a
/// PENDING_ACK before the (potentially blocking) WRITEREG completes.
#[test]
fn pending_ack_precedes_downstream_stream_open() {
    let s = start(false);
    let up_port = s._up_server.local_port().unwrap();
    let rx = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let mut payload = Vec::new();
    payload.extend_from_slice(&reg::SCP0.to_be_bytes());
    payload.extend_from_slice(&reg::scp_make(rx.local_addr().unwrap().port()).to_be_bytes());
    let mut header = [0u8; HEADER_LEN];
    CmdHeader {
        flag: FLAG_ACK,
        command: WRITEREG_CMD,
        length: payload.len() as u16,
        req_id: 15,
    }
    .encode(&mut header);
    let mut datagram = header.to_vec();
    datagram.extend_from_slice(&payload);
    socket
        .send_to(&datagram, (Ipv4Addr::LOCALHOST, up_port))
        .unwrap();

    let mut reply = [0u8; 256];
    let (n, _) = socket.recv_from(&mut reply).unwrap();
    assert!(n >= HEADER_LEN);
    assert_eq!(
        u16::from_be_bytes([reply[2], reply[3]]),
        PENDING_ACK,
        "PENDING_ACK must arrive before the stream-open acknowledge"
    );
    assert_eq!(u16::from_be_bytes([reply[4], reply[5]]), 4);
    assert_eq!(u16::from_be_bytes([reply[6], reply[7]]), 15);
    let (n, _) = socket.recv_from(&mut reply).unwrap();
    assert!(n >= HEADER_LEN);
    assert_eq!(
        u16::from_be_bytes([reply[2], reply[3]]),
        WRITEREG_ACK,
        "final WRITEREG_ACK follows"
    );
    assert_eq!(u16::from_be_bytes([reply[0], reply[1]]), GEV_STATUS_SUCCESS);
    assert_eq!(u16::from_be_bytes([reply[6], reply[7]]), 15);
    // Close the relay stream again through the normal control channel.
    s.remote.write_reg(reg::SCP0, 0).unwrap();
}
