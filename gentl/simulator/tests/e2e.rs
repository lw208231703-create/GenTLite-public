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
    // apart (DeviceHandler carries no (ip, port); see OPEN-QUESTIONS Q7), so
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
