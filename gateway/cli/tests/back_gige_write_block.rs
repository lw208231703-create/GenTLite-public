//! `back-gige` x `front-u3v` scalar write path (GigE Vision 3.0 §16.4):
//! the U3V front receives every GenApi write as a GenCP WRITEMEM block, so
//! the GigE backend must route 4-byte blocks through GVCP WRITEREG or a
//! U3V host could never configure a GigE camera relayed through the
//! gateway. Longer blocks stay NOT_IMPLEMENTED (no GVCP WRITEMEM in the
//! client, §16.5).

use std::net::Ipv4Addr;

use gentl_simulator::{SimCamera, SimConfig, StreamConfig, StreamingProtocol, REG_WIDTH};
use vgige_back_gige::GigeBack;
use vgige_core::CameraBack;

const MAC: [u8; 6] = [0x00, 0x11, 0x22, 0x33, 0x44, 0x55];

#[test]
fn four_byte_block_write_uses_writereg() {
    let cam = SimCamera::with_stream(
        SimConfig {
            mac: MAC,
            ip: Ipv4Addr::LOCALHOST,
            subnet_mask: Ipv4Addr::new(255, 0, 0, 0),
            gateway: Ipv4Addr::UNSPECIFIED,
            manufacturer_name: "GenTLite".into(),
            model_name: "SimCam".into(),
            device_version: "1.0.0".into(),
            serial_number: "SIM-WRITE".into(),
            streaming: StreamingProtocol::GvspOnly,
        },
        StreamConfig {
            width: 16,
            height: 16,
            pixel_format: 0x0110_0007,
            channels: 1,
            legacy_headers: false,
            chunk: None,
        },
    );
    let server = cam.start(0).expect("start sim camera");
    let port = server.local_port().expect("server port");
    let mut back = GigeBack::connect(Ipv4Addr::LOCALHOST, port).expect("connect backend");
    assert_eq!(
        back.register_endian(),
        vgige_core::RegisterEndian::Big,
        "GVCP register blocks are big-endian"
    );

    let addr = REG_WIDTH;
    back.write_block(addr, &640u32.to_be_bytes())
        .expect("4-byte WRITEMEM-equivalent lands as WRITEREG");
    assert_eq!(back.read_reg(addr).expect("read back"), 640);

    // Multi-word blocks ride GVCP WRITEMEM (§16.6, mandatory per R-185cd):
    // write Width + Height in one block and read both back.
    let mut block = [0u8; 8];
    block[..4].copy_from_slice(&1280u32.to_be_bytes());
    block[4..].copy_from_slice(&960u32.to_be_bytes());
    back.write_block(REG_WIDTH, &block)
        .expect("multi-word block write lands as WRITEMEM");
    assert_eq!(back.read_reg(REG_WIDTH).expect("width"), 1280);
    assert_eq!(
        back.read_reg(gentl_simulator::REG_HEIGHT).expect("height"),
        960
    );
    // A GenCP block that is not 32-bit aligned cannot be expressed on the
    // GigE wire (R-188cd/R-189c).
    assert_eq!(back.write_block(REG_WIDTH, &[0u8; 6]), Err(0x8005));
}
