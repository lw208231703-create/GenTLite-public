//! End-to-end: SimCamera served over UDP, driven by the real GVCP client.

use std::net::{Ipv4Addr, UdpSocket};
use std::time::Duration;

use gentl_core::GentlError;
use gentl_gvcp::gvsp;
use gentl_gvcp::registers as reg;
use gentl_gvcp::wire::{GEV_STATUS_ACCESS_DENIED, GEV_STATUS_INVALID_ADDRESS};
use gentl_gvcp::GvcpClient;
use gentl_simulator::{SimCamera, SimConfig, StreamConfig, StreamingProtocol, REG_EXPOSURE_TIME, XML_BASE};

fn test_config() -> SimConfig {
    SimConfig {
        mac: [0x02, 0x00, 0x11, 0x22, 0x33, 0x44],
        ip: Ipv4Addr::new(192, 168, 1, 10),
        subnet_mask: Ipv4Addr::new(255, 255, 255, 0),
        gateway: Ipv4Addr::new(192, 168, 1, 1),
        manufacturer_name: "GenTLite".into(),
        model_name: "SimCam".into(),
        device_version: "1.0.0".into(),
        serial_number: "SIM-0001".into(),
        streaming: StreamingProtocol::GvspOnly,
    }
}

fn test_camera() -> SimCamera {
    SimCamera::new(test_config())
}

fn gev_status(err: GentlError) -> u16 {
    match err {
        GentlError::GevStatus(s) => s,
        other => panic!("expected GevStatus, got {other}"),
    }
}

#[test]
fn version_register_over_the_wire() {
    let cam = test_camera();
    let server = cam.start(0).unwrap();
    let port = server.local_port().unwrap();
    let client = GvcpClient::connect(Ipv4Addr::LOCALHOST, port).unwrap();
    assert_eq!(client.read_reg(reg::VERSION).unwrap(), 0x0002_0002);
    server.stop();
}

#[test]
fn exposure_register_write_read_roundtrip() {
    let cam = test_camera();
    let server = cam.start(0).unwrap();
    let port = server.local_port().unwrap();
    let client = GvcpClient::connect(Ipv4Addr::LOCALHOST, port).unwrap();

    let bits = 25_500.0f32.to_bits();
    client.write_reg(REG_EXPOSURE_TIME, bits).unwrap();
    assert_eq!(client.read_reg(REG_EXPOSURE_TIME).unwrap(), bits);

    // Width default and RW behaviour via a second feature register.
    assert_eq!(client.read_reg(gentl_simulator::REG_WIDTH).unwrap(), 1024);
    client.write_reg(gentl_simulator::REG_WIDTH, 640).unwrap();
    assert_eq!(client.read_reg(gentl_simulator::REG_WIDTH).unwrap(), 640);
    server.stop();
}

#[test]
fn xml_blob_and_first_url_over_the_wire() {
    let cam = test_camera();
    let server = cam.start(0).unwrap();
    let port = server.local_port().unwrap();
    let client = GvcpClient::connect(Ipv4Addr::LOCALHOST, port).unwrap();

    // First 5 bytes of the XML blob read back through READMEM.
    let mut head = [0u8; 8]; // 5 rounded up to the 4-byte READMEM granularity
    client.read_mem(XML_BASE, &mut head).unwrap();
    assert_eq!(&head[..5], b"<?xml");

    // xml_url() carries the local: address; the blob reads back identically.
    let url = client.xml_url().unwrap();
    let expected = format!("local:simcam.xml;{XML_BASE:x};{:x}", cam.xml().len());
    assert_eq!(url, expected);

    let mut buf = vec![0u8; cam.xml().len().next_multiple_of(4)];
    client.read_mem(XML_BASE, &mut buf).unwrap();
    assert_eq!(&buf[..cam.xml().len()], cam.xml().as_bytes());
    server.stop();
}

#[test]
fn error_statuses_over_the_wire() {
    let cam = test_camera();
    let server = cam.start(0).unwrap();
    let port = server.local_port().unwrap();
    let client = GvcpClient::connect(Ipv4Addr::LOCALHOST, port).unwrap();

    // Reserved bootstrap address.
    assert_eq!(
        gev_status(client.read_reg(0x0018).unwrap_err()),
        GEV_STATUS_INVALID_ADDRESS
    );
    // Read-only register.
    client.read_reg(reg::VERSION).unwrap();
    // CCP dance between two clients. The simplified model cannot tell peers
    // apart (DeviceHandler carries no (ip, port); see docs/00-governance/OPEN-QUESTIONS.md Q7), so
    // an identical re-request counts as an R-069cd refresh and denial is
    // exercised with a different privilege.
    let other = GvcpClient::connect(Ipv4Addr::LOCALHOST, port).unwrap();

    client.write_reg(reg::CCP, reg::CCP_EXCLUSIVE_ACCESS).unwrap(); // EA
    assert_eq!(
        client.read_reg(reg::CCP).unwrap() >> reg::CCP_CONNECTION_COUNT_SHIFT & 0xF,
        1
    );
    assert_eq!(
        gev_status(other.write_reg(reg::CCP, reg::CCP_CONTROL_ACCESS).unwrap_err()), // CA while EA held
        GEV_STATUS_ACCESS_DENIED
    );
    client.write_reg(reg::CCP, 0).unwrap();
    // Released: privilege bits clear, connection count stays non-zero.
    assert_eq!(
        client.read_reg(reg::CCP).unwrap(),
        1 << reg::CCP_CONNECTION_COUNT_SHIFT
    );

    // Discovery ack carries the configured identity.
    let found = gentl_gvcp::client::discover(
        &[(Ipv4Addr::LOCALHOST, port)],
        std::time::Duration::from_millis(200),
    )
    .unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].manufacturer_name, "GenTLite");
    assert_eq!(found[0].model_name, "SimCam");
    assert_eq!(found[0].serial_number, "SIM-0001");

    server.stop();
}

#[test]
fn gvsp_stream_over_the_wire() {
    const MONO8: u32 = 0x0108_0001;
    let cam = SimCamera::with_stream(
        test_config(),
        StreamConfig {
            width: 32,
            height: 16,
            pixel_format: MONO8,
            channels: 1,
            legacy_headers: false,
            chunk: None,
        },
    );
    let server = cam.start(0).unwrap();
    let port = server.local_port().unwrap();
    let client = GvcpClient::connect(Ipv4Addr::LOCALHOST, port).unwrap();

    let rx = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    rx.set_read_timeout(Some(Duration::from_millis(1000)))
        .unwrap();
    let host_port = rx.local_addr().unwrap().port();

    // Open the stream channel: destination IP, then the host port in the
    // SCPx layout (bits 16-31, spec §36.57); SCP0 is the last register
    // written when opening (R-561ca).
    client
        .write_reg(reg::SCDA0, u32::from(Ipv4Addr::LOCALHOST))
        .unwrap();
    client
        .write_reg(reg::SCP0, reg::scp_make(host_port))
        .unwrap();
    assert_eq!(
        reg::scp_host_port(client.read_reg(reg::SCP0).unwrap()),
        host_port,
        "SCP0 reads back the host_port field"
    );

    let mut buf = [0u8; 2048];
    // 32 * 16 = 512 bytes -> leader, one payload packet, trailer.
    let (n, _) = rx.recv_from(&mut buf).unwrap();
    let leader = gvsp::GvspHeader::decode(&buf[..n]).unwrap();
    assert_eq!(leader.packet_format, gvsp::PACKET_FORMAT_DATA_LEADER);
    assert_eq!(leader.packet_id, 0);
    assert_eq!(leader.block_id, 1);
    let l = gvsp::ImageLeader::decode(&buf[gvsp::GVSP_HEADER_LEN..n]).unwrap();
    assert_eq!(l.pixel_format, MONO8);
    assert_eq!(l.size_x, 32);
    assert_eq!(l.size_y, 16);

    let (n, _) = rx.recv_from(&mut buf).unwrap();
    let payload = gvsp::GvspHeader::decode(&buf[..n]).unwrap();
    assert_eq!(payload.packet_format, gvsp::PACKET_FORMAT_DATA_PAYLOAD);
    assert_eq!(payload.packet_id, 1);
    assert_eq!(payload.block_id, 1);
    assert_eq!(n, gvsp::GVSP_HEADER_LEN + 512);
    for (i, &b) in buf[gvsp::GVSP_HEADER_LEN..n].iter().enumerate() {
        assert_eq!(b, (i + 1) as u8, "pixel {i}");
    }

    let (n, _) = rx.recv_from(&mut buf).unwrap();
    let trailer = gvsp::GvspHeader::decode(&buf[..n]).unwrap();
    assert_eq!(trailer.packet_format, gvsp::PACKET_FORMAT_DATA_TRAILER);
    assert_eq!(trailer.packet_id, 2);
    assert_eq!(
        gvsp::decode_image_trailer_body(&buf[gvsp::GVSP_HEADER_LEN..n]).unwrap(),
        16
    );

    // Close the channel again (host_port = 0).
    client.write_reg(reg::SCP0, reg::scp_make(0)).unwrap();
    assert_eq!(client.read_reg(reg::SCP0).unwrap(), 0);
    server.stop();
}

/// §16.2 FORCEIP over the wire: the command is accepted (MAC match, no CCP
/// holder), the simulated device applies the forced IP configuration and
/// acknowledges it; a request for another MAC and a request while the
/// control channel is occupied are silently discarded (R-149cd/R-150cd).
#[test]
fn force_ip_command_end_to_end() {
    use gentl_gvcp::wire::{
        encode_force_ip_payload, CmdHeader, ForceIpRequest, FORCEIP_ACK, FORCEIP_CMD,
        FORCEIP_PAYLOAD_LEN, FLAG_ACK, HEADER_LEN,
    };

    let cam = test_camera();
    let server = cam.start(0).unwrap();
    let port = server.local_port().unwrap();
    let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_millis(400)))
        .unwrap();

    let send = |request: ForceIpRequest, req_id: u16| -> Option<Vec<u8>> {
        let payload = encode_force_ip_payload(&request);
        assert_eq!(payload.len(), FORCEIP_PAYLOAD_LEN);
        let header = CmdHeader {
            flag: FLAG_ACK,
            command: FORCEIP_CMD,
            length: payload.len() as u16,
            req_id,
        };
        let mut datagram = vec![0u8; HEADER_LEN];
        let mut buf = [0u8; HEADER_LEN];
        header.encode(&mut buf);
        datagram.copy_from_slice(&buf);
        datagram.extend_from_slice(&payload);
        socket.send_to(&datagram, (Ipv4Addr::LOCALHOST, port)).unwrap();
        let mut reply = [0u8; 256];
        match socket.recv(&mut reply) {
            Ok(n) => Some(reply[..n].to_vec()),
            Err(_) => None,
        }
    };

    let request = ForceIpRequest {
        mac: test_config().mac,
        static_ip: u32::from(Ipv4Addr::new(10, 0, 0, 100)),
        subnet_mask: u32::from(Ipv4Addr::new(255, 255, 255, 0)),
        default_gateway: u32::from(Ipv4Addr::new(10, 0, 0, 1)),
    };
    let ack = send(request, 7).expect("a valid FORCEIP is acknowledged");
    assert_eq!(u16::from_be_bytes([ack[0], ack[1]]), 0, "status SUCCESS");
    assert_eq!(
        u16::from_be_bytes([ack[2], ack[3]]),
        FORCEIP_ACK,
        "FORCEIP_ACK (Figure 16-4)"
    );
    assert_eq!(u16::from_be_bytes([ack[4], ack[5]]), 0, "no payload");
    assert_eq!(u16::from_be_bytes([ack[6], ack[7]]), 7, "ack_id echoes req_id");

    // R-152cd: the forced address is effective before the ACK went out.
    let client = GvcpClient::connect(Ipv4Addr::LOCALHOST, port).unwrap();
    assert_eq!(
        client.read_reg(reg::CURRENT_IP).unwrap(),
        u32::from(Ipv4Addr::new(10, 0, 0, 100))
    );
    assert_eq!(
        client.read_reg(reg::CURRENT_SUBNET_MASK).unwrap(),
        u32::from(Ipv4Addr::new(255, 255, 255, 0))
    );
    assert_eq!(
        client.read_reg(reg::CURRENT_DEFAULT_GATEWAY).unwrap(),
        u32::from(Ipv4Addr::new(10, 0, 0, 1))
    );

    // R-149cd: a request carrying another device's MAC is silently
    // discarded.
    let mut other = request;
    other.mac = [0x02, 0x00, 0x00, 0x00, 0x00, 0x01];
    other.static_ip = u32::from(Ipv4Addr::new(10, 0, 0, 200));
    assert!(send(other, 8).is_none(), "foreign MAC must be discarded");
    assert_eq!(
        client.read_reg(reg::CURRENT_IP).unwrap(),
        u32::from(Ipv4Addr::new(10, 0, 0, 100)),
        "a discarded request must not touch the configuration"
    );

    // R-150cd: while an application holds the control channel the command is
    // discarded (the device ignores it, no ACK).
    client
        .write_reg(reg::CCP, reg::CCP_EXCLUSIVE_ACCESS)
        .unwrap();
    let mut occupied = request;
    occupied.static_ip = u32::from(Ipv4Addr::new(10, 0, 0, 150));
    assert!(send(occupied, 9).is_none(), "occupied channel must discard");
    client.write_reg(reg::CCP, 0).unwrap();

    // R-151cd: static_IP == 0 restarts the IP configuration cycle without
    // an ACK.
    let restart = ForceIpRequest {
        static_ip: 0,
        ..request
    };
    assert!(send(restart, 10).is_none(), "restart cycle has no ACK");
    server.stop();
}

/// §16.6 WRITEMEM over the wire: a multi-word write lands in the register
/// model (R-185cd) and the ack carries reserved + written-byte index
/// (R-193c/R-194cd); the XML description answers WRITE_PROTECT.
#[test]
fn write_mem_command_end_to_end() {
    let cam = test_camera();
    let server = cam.start(0).unwrap();
    let port = server.local_port().unwrap();
    let client = GvcpClient::connect(Ipv4Addr::LOCALHOST, port).unwrap();

    let mut block = [0u8; 8];
    block[..4].copy_from_slice(&640u32.to_be_bytes());
    block[4..].copy_from_slice(&480u32.to_be_bytes());
    client.write_mem(gentl_simulator::REG_WIDTH, &block).unwrap();
    assert_eq!(client.read_reg(gentl_simulator::REG_WIDTH).unwrap(), 640);
    assert_eq!(client.read_reg(gentl_simulator::REG_HEIGHT).unwrap(), 480);

    // R-188cd/R-189c are enforced device-side.
    let mut reply = [0u8; 8];
    let datagram = {
        use gentl_gvcp::wire::{
            encode_writemem_cmd_payload, CmdHeader, FLAG_ACK, HEADER_LEN, WRITEMEM_CMD,
        };
        let payload = encode_writemem_cmd_payload(gentl_simulator::REG_WIDTH, &[0u8; 6]);
        let mut out = vec![0u8; HEADER_LEN];
        let mut hdr = [0u8; HEADER_LEN];
        CmdHeader {
            flag: FLAG_ACK,
            command: WRITEMEM_CMD,
            length: payload.len() as u16,
            req_id: 11,
        }
        .encode(&mut hdr);
        out.copy_from_slice(&hdr);
        out.extend_from_slice(&payload);
        let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        socket.set_read_timeout(Some(Duration::from_millis(500))).unwrap();
        socket.send_to(&out, (Ipv4Addr::LOCALHOST, port)).unwrap();
        let mut buf = [0u8; 64];
        let (n, _) = socket.recv_from(&mut buf).unwrap();
        reply[..2].copy_from_slice(&buf[..2]);
        n
    };
    assert!(datagram >= 8);
    assert_eq!(
        u16::from_be_bytes([reply[0], reply[1]]),
        gentl_gvcp::wire::GEV_STATUS_BAD_ALIGNMENT
    );

    // The XML blob is write-protected.
    assert_eq!(
        gev_status(client.write_mem(gentl_simulator::XML_BASE, &[0u8; 4]).unwrap_err()),
        gentl_gvcp::wire::GEV_STATUS_WRITE_PROTECT
    );
    server.stop();
}
