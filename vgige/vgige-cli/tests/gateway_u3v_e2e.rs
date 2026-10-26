//! Gateway end-to-end for the "GigE front ← U3V camera" matrix cell, no
//! hardware: a fake GenCP/U3V device (minimal ABRM + manifest + SIRM + a
//! scripted image stream behind three `UsbPipe` endpoints) plays the
//! camera; `U3vBack` (via the gentl-u3v client facade) drives it; the
//! vgige core relays through the GigE front; a plain `GvcpClient` plays
//! the remote client. Mirrors relay_e2e.rs's four contracts: pass-through,
//! error transparency, custom window + XML injection, stream relay.
//!
//! Wire constants the fake speaks are re-declared here (values per GenCP
//! 1.3.1 §4.3.3 Table 7 / U3V 1.2 §5.5); gentl-u3v keeps its own pub(crate)
//! table and the facade's exactness is what the e2e exercises end to end.

use std::collections::{BTreeMap, VecDeque};
use std::net::{Ipv4Addr, UdpSocket};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gentl_core::GentlError;
use gentl_gvcp::gvsp::*;
use gentl_gvcp::registers as reg;
use gentl_gvcp::wire::*;
use gentl_gvcp::{GvcpClient, GvcpServer};
use gentl_u3v::client::UsbPipe;
use vgige_back_u3v::U3vBack;
use vgige_core::{Relay, RegisterSpace, SharedBack, XmlMerger, CUSTOM_BASE, VGIGE_VERSION, XML_BASE_V, slot};
use vgige_front_gige::{assemble_identity, GigEFront, GvspSession, UpIdentity};

// ---------------------------------------------------------------------------
// Fake GenCP/U3V device
// ---------------------------------------------------------------------------

// GenCP control channel (GenCP 1.3.1 §4; values LE on the wire, R-30cd).
const PREFIX_CONTROL: u32 = 0x4356_3355; // "U3VC"
const READMEM_CMD: u16 = 0x0800;
const READMEM_ACK: u16 = 0x0801;
const WRITEMEM_CMD: u16 = 0x0802;
const WRITEMEM_ACK: u16 = 0x0803;
const GENCP_SUCCESS: u16 = 0x0000;
const GENCP_NOT_IMPLEMENTED: u16 = 0x8001;
const GENCP_INVALID_ADDRESS: u16 = 0x8003;

// ABRM (GenCP §5.4 / U3V 1.2 Table 4-6) offsets and the capability bits
// the fake declares: SBRM + endianness register + written-length field.
const ABRM_DEVICE_CAPABILITY: u64 = 0x01C4;
const ABRM_MANIFEST_TABLE: u64 = 0x01D0;
const ABRM_SBRM_ADDRESS: u64 = 0x01D8;
const CAP_SBRM: u64 = 1 << 9;
const CAP_ENDIANNESS_REGISTER: u64 = 1 << 10;
const CAP_WRITTEN_LENGTH_FIELD: u64 = 1 << 11;

// SBRM (U3V 1.2 Table 4-7) + SIRM (Table 5-1/5-2).
const SBRM_BASE: u64 = 0x2000;
const SBRM_U3V_VERSION: u64 = SBRM_BASE; // u32: major<<16 | minor
const SBRM_U3VCP_CAPABILITY: u64 = SBRM_BASE + 0x04;
const SBRM_MAX_COMMAND_TRANSFER: u64 = SBRM_BASE + 0x14;
const SBRM_MAX_ACK_TRANSFER: u64 = SBRM_BASE + 0x18;
const SBRM_NUM_STREAM_CHANNELS: u64 = SBRM_BASE + 0x1C;
const SBRM_SIRM_ADDRESS: u64 = SBRM_BASE + 0x20;
const SBRM_SIRM_LENGTH: u64 = SBRM_BASE + 0x28;
const CAP_SIRM: u64 = 1 << 0;

const SIRM_BASE: u64 = 0x4000;
const SI_CONTROL: u64 = SIRM_BASE + 0x04;
const SI_STREAM_ENABLE: u32 = 1 << 0;

const MANIFEST_BASE: u64 = 0x1000;
const XML_BASE: u64 = 0x3000;
/// Vendor register the fake serves for the pass-through contract (the XML
/// advertises it as WidthReg).
const VENDOR_WIDTH: u64 = 0x0010_0000;

const FAKE_MPS: usize = 1024;
const MONO8: u32 = 0x0108_0001;
const W: u32 = 64;
const H: u32 = 48;
const PAYLOAD: usize = (W * H) as usize;

/// Minimal but spec-shaped fake: byte-granular register memory over the
/// ABRM/SBRM/SIRM maps, the manifest table pointing at an embedded XML
/// blob, a Written-Length-field WRITEMEM acknowledge, and a Mono8 image
/// stream served block by block (host-paced, one block ahead) while
/// SI_CONTROL Stream Enable is set.
struct FakeU3v {
    mem: Mutex<BTreeMap<u64, u8>>,
    acks: Mutex<VecDeque<Vec<u8>>>,
    stream: Mutex<VecDeque<u8>>,
    /// Piece structure of the queued block (leader/payload/trailer sizes):
    /// reads are capped at the current piece's remainder so each logical
    /// piece short-packet-terminates like a real device (§5.2.1), instead
    /// of running into the next piece mid-request.
    pieces: Mutex<VecDeque<usize>>,
    piece_left: Mutex<usize>,
    next_block: Mutex<u64>,
}

impl FakeU3v {
    fn new() -> Self {
        let fake = Self {
            mem: Mutex::new(BTreeMap::new()),
            acks: Mutex::new(VecDeque::new()),
            stream: Mutex::new(VecDeque::new()),
            pieces: Mutex::new(VecDeque::new()),
            piece_left: Mutex::new(0),
            next_block: Mutex::new(1),
        };
        fake.init_memory();
        fake
    }

    fn init_memory(&self) {
        let mut m = self.mem.lock().unwrap();
        let put = |m: &mut BTreeMap<u64, u8>, addr: u64, bytes: &[u8]| {
            for (i, &b) in bytes.iter().enumerate() {
                m.insert(addr + i as u64, b);
            }
        };
        // ABRM identity block (GenCP §5.4; strings are 64-byte slots —
        // every byte of the slot must be mapped, zero padding included).
        let put_str = |m: &mut BTreeMap<u64, u8>, addr: u64, s: &str| {
            let mut slot = [0u8; 64];
            let bytes = s.as_bytes();
            slot[..bytes.len().min(64)].copy_from_slice(&bytes[..bytes.len().min(64)]);
            put(m, addr, &slot);
        };
        put(&mut m, 0x0000, &0x0001_0003u32.to_le_bytes()); // GenCP 1.3
        put_str(&mut m, 0x0004, "FakeU3V");
        put_str(&mut m, 0x0044, "FakeU3VCam");
        put_str(&mut m, 0x00C4, "1.0.0");
        put_str(&mut m, 0x0144, "FU3V-0001");
        put(
            &mut m,
            ABRM_DEVICE_CAPABILITY,
            &(CAP_SBRM | CAP_ENDIANNESS_REGISTER | CAP_WRITTEN_LENGTH_FIELD).to_le_bytes(),
        );
        put(&mut m, 0x01CC, &500u32.to_le_bytes()); // max response time, ms
        put(&mut m, ABRM_MANIFEST_TABLE, &MANIFEST_BASE.to_le_bytes());
        put(&mut m, ABRM_SBRM_ADDRESS, &SBRM_BASE.to_le_bytes());

        // Manifest: one entry, raw (uncompressed) Device XML (GenCP §5.5).
        let xml = FAKE_XML.as_bytes();
        put(&mut m, MANIFEST_BASE, &1u64.to_le_bytes());
        let mut entry = vec![0u8; 64];
        entry[4..8].copy_from_slice(&0u32.to_le_bytes()); // schema: raw XML, device XML
        entry[8..16].copy_from_slice(&XML_BASE.to_le_bytes());
        entry[16..24].copy_from_slice(&(xml.len() as u64).to_le_bytes());
        put(&mut m, MANIFEST_BASE + 8, &entry);

        // SBRM: one streaming channel, 1 KiB transfers.
        put(&mut m, SBRM_U3V_VERSION, &0x0001_0002u32.to_le_bytes()); // U3V 1.2
        put(&mut m, SBRM_U3VCP_CAPABILITY, &CAP_SIRM.to_le_bytes());
        put(&mut m, SBRM_MAX_COMMAND_TRANSFER, &1024u32.to_le_bytes());
        put(&mut m, SBRM_MAX_ACK_TRANSFER, &1024u32.to_le_bytes());
        put(&mut m, SBRM_NUM_STREAM_CHANNELS, &1u32.to_le_bytes());
        put(&mut m, SBRM_SIRM_ADDRESS, &SIRM_BASE.to_le_bytes());
        put(&mut m, SBRM_SIRM_LENGTH, &0x48u32.to_le_bytes());

        // SIRM: standard payload mode, alignment 1 (SI_INFO = 0). The RW
        // transfer-size registers (0x18..0x30) are pre-mapped: the host
        // programs them at Stream Enable, and the fake's writes — like a
        // real device's — must land.
        put(&mut m, SIRM_BASE, &0u32.to_le_bytes()); // SI_INFO
        put(&mut m, SI_CONTROL, &0u32.to_le_bytes());
        put(&mut m, SIRM_BASE + 0x08, &(PAYLOAD as u64).to_le_bytes());
        put(&mut m, SIRM_BASE + 0x10, &52u32.to_le_bytes()); // leader
        put(&mut m, SIRM_BASE + 0x14, &32u32.to_le_bytes()); // trailer
        put(&mut m, SIRM_BASE + 0x18, &[0u8; 0x30 - 0x18]);

        put(&mut m, XML_BASE, xml);
        put(&mut m, VENDOR_WIDTH, &W.to_le_bytes());
    }

    fn reg_u32(&self, addr: u64) -> u32 {
        let m = self.mem.lock().unwrap();
        let b = |o: u64| m.get(&(addr + o)).copied().unwrap_or(0);
        u32::from_le_bytes([b(0), b(1), b(2), b(3)])
    }

    /// READMEM over the sparse map: every byte must be mapped.
    fn read_mem(&self, addr: u64, buf: &mut [u8]) -> u16 {
        let m = self.mem.lock().unwrap();
        for (i, b) in buf.iter_mut().enumerate() {
            match m.get(&(addr + i as u64)) {
                Some(&v) => *b = v,
                None => return GENCP_INVALID_ADDRESS,
            }
        }
        GENCP_SUCCESS
    }

    fn write_mem(&self, addr: u64, data: &[u8]) -> u16 {
        let mut m = self.mem.lock().unwrap();
        for (i, &b) in data.iter().enumerate() {
            if m.insert(addr + i as u64, b).is_none() {
                return GENCP_INVALID_ADDRESS;
            }
        }
        GENCP_SUCCESS
    }

    /// One GenCP command transfer arrives on the bulk OUT endpoint; the
    /// acknowledge is queued for the bulk IN endpoint.
    fn on_command(&self, cmd: &[u8]) {
        let (status, ack_command, scd): (u16, u16, Vec<u8>) = {
            if cmd.len() < 20 || u32::from_le_bytes([cmd[0], cmd[1], cmd[2], cmd[3]]) != PREFIX_CONTROL {
                (GENCP_NOT_IMPLEMENTED, READMEM_ACK, Vec::new())
            } else {
                let command = u16::from_le_bytes([cmd[6], cmd[7]]);
                let length = u16::from_le_bytes([cmd[8], cmd[9]]) as usize;
                let scd = &cmd[12..cmd.len().min(12 + length)];
                match command {
                    READMEM_CMD if scd.len() >= 12 => {
                        let addr = u64::from_le_bytes(scd[0..8].try_into().unwrap());
                        let len = u16::from_le_bytes([scd[10], scd[11]]) as usize;
                        let mut data = vec![0u8; len];
                        let status = self.read_mem(addr, &mut data);
                        let n = if status == GENCP_SUCCESS { len } else { 0 };
                        (status, READMEM_ACK, data[..n].to_vec())
                    }
                    WRITEMEM_CMD if scd.len() >= 8 => {
                        let addr = u64::from_le_bytes(scd[0..8].try_into().unwrap());
                        let data = &scd[8..];
                        let status = self.write_mem(addr, data);
                        let written = if status == GENCP_SUCCESS { data.len() } else { 0 };
                        // Written-Length field (CAP_WRITTEN_LENGTH_FIELD):
                        // reserved u16 + written u16 (GenCP §4.4.4).
                        let mut field = vec![0u8; 4];
                        field[2..4].copy_from_slice(&(written as u16).to_le_bytes());
                        (status, WRITEMEM_ACK, field)
                    }
                    _ => (GENCP_NOT_IMPLEMENTED, command.wrapping_add(1), Vec::new()),
                }
            }
        };
        let req = if cmd.len() >= 12 {
            u16::from_le_bytes([cmd[10], cmd[11]])
        } else {
            0
        };
        let mut ack = Vec::with_capacity(12 + scd.len());
        ack.extend_from_slice(&PREFIX_CONTROL.to_le_bytes());
        ack.extend_from_slice(&status.to_le_bytes());
        ack.extend_from_slice(&ack_command.to_le_bytes());
        ack.extend_from_slice(&(scd.len() as u16).to_le_bytes());
        ack.extend_from_slice(&req.to_le_bytes());
        ack.extend_from_slice(&scd);
        self.acks.lock().unwrap().push_back(ack);
    }

    /// One U3V image block (§5.5): leader, Mono8 payload `(k + block) mod
    /// 256`, trailer. The sim's GigE pattern, so the payload assertion
    /// shape matches relay_e2e.
    fn block_bytes(&self, block: u64) -> Vec<u8> {
        let mut v = Vec::with_capacity(52 + PAYLOAD + 32);
        let mut leader = vec![0u8; 52];
        leader[0..4].copy_from_slice(b"U3VL");
        leader[6..8].copy_from_slice(&52u16.to_le_bytes());
        leader[8..16].copy_from_slice(&block.to_le_bytes());
        leader[18..20].copy_from_slice(&1u16.to_le_bytes()); // payload type image
        leader[20..28].copy_from_slice(&(block * 1000).to_le_bytes());
        leader[28..32].copy_from_slice(&MONO8.to_le_bytes());
        leader[32..36].copy_from_slice(&W.to_le_bytes());
        leader[36..40].copy_from_slice(&H.to_le_bytes());
        v.extend_from_slice(&leader);
        for k in 0..PAYLOAD {
            v.push((k as u64 + block) as u8);
        }
        let mut trailer = vec![0u8; 32];
        trailer[0..4].copy_from_slice(b"U3VT");
        trailer[6..8].copy_from_slice(&32u16.to_le_bytes());
        trailer[8..16].copy_from_slice(&block.to_le_bytes());
        trailer[16..18].copy_from_slice(&GENCP_SUCCESS.to_le_bytes());
        trailer[20..28].copy_from_slice(&(PAYLOAD as u64).to_le_bytes());
        trailer[28..32].copy_from_slice(&H.to_le_bytes());
        v.extend_from_slice(&trailer);
        v
    }

    /// Bulk-IN endpoint of the streaming interface: host-paced, at most
    /// one block generated ahead, only while Stream Enable is set. Reads
    /// are capped at the endpoint MPS (so the host's UQ16 accumulation is
    /// exercised — one logical piece split across several transactions)
    /// AND at the current piece's remainder (so each piece short-packet-
    /// terminates, §5.2.1).
    fn stream_read(&self, buf: &mut [u8]) -> Result<u32, GentlError> {
        let mut q = self.stream.lock().unwrap();
        if q.is_empty() {
            if self.reg_u32(SI_CONTROL) & SI_STREAM_ENABLE == 0 {
                return Err(GentlError::Timeout);
            }
            let block = {
                let mut b = self.next_block.lock().unwrap();
                let cur = *b;
                *b += 1;
                cur
            };
            let bytes = self.block_bytes(block);
            // The leader piece (52) is current immediately; queue the
            // remaining pieces for when `piece_left` runs out.
            let mut pieces = self.pieces.lock().unwrap();
            pieces.push_back(PAYLOAD);
            pieces.push_back(32);
            *self.piece_left.lock().unwrap() = 52;
            q.extend(bytes.iter());
        }
        if *self.piece_left.lock().unwrap() == 0 {
            *self.piece_left.lock().unwrap() = self.pieces.lock().unwrap().pop_front().unwrap_or(0);
        }
        let n = buf
            .len()
            .min(FAKE_MPS)
            .min(q.len())
            .min(*self.piece_left.lock().unwrap());
        for b in buf[..n].iter_mut() {
            *b = q.pop_front().unwrap_or(0);
        }
        *self.piece_left.lock().unwrap() -= n;
        Ok(n as u32)
    }
}

/// The fake camera's GenICam XML: one vendor register in the scratch area
/// (little-endian, like every U3V register).
const FAKE_XML: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
<RegisterDescription xmlns=\"http://www.genicam.org/GenApi/Version_1_1\" ModelName=\"FakeU3VCam\" VendorName=\"FakeU3V\">\n\
\x20 <Group Comment=\"ImageFormat\">\n\
\x20   <Integer Name=\"Width\" NameSpace=\"Standard\">\n\
\x20     <ToolTip>Image width in pixels.</ToolTip>\n\
\x20     <pValue>WidthReg</pValue>\n\
\x20     <Min>1</Min>\n\
\x20     <Max>512</Max>\n\
\x20   </Integer>\n\
\x20   <IntReg Name=\"WidthReg\" NameSpace=\"Custom\">\n\
\x20     <Address>0x00100000</Address>\n\
\x20     <Length>4</Length>\n\
\x20     <AccessMode>RW</AccessMode>\n\
\x20     <pPort>Device</pPort>\n\
\x20     <Sign>Unsigned</Sign>\n\
\x20     <Endianess>LittleEndian</Endianess>\n\
\x20   </IntReg>\n\
\x20 </Group>\n\
</RegisterDescription>\n";

struct FakeControlOut(Arc<FakeU3v>);
struct FakeControlIn(Arc<FakeU3v>);
struct FakeStreamIn(Arc<FakeU3v>);

impl UsbPipe for FakeControlOut {
    fn read_bulk(&self, _buf: &mut [u8], _timeout: Duration) -> Result<u32, GentlError> {
        Err(GentlError::Timeout)
    }
    fn write_bulk(&self, buf: &[u8], _timeout: Duration) -> Result<(), GentlError> {
        self.0.on_command(buf);
        Ok(())
    }
    fn flush(&self) {}
    fn set_raw_io(&self) -> Result<(), GentlError> {
        Ok(())
    }
    fn max_packet(&self) -> u16 {
        FAKE_MPS as u16
    }
}

impl UsbPipe for FakeControlIn {
    fn read_bulk(&self, buf: &mut [u8], _timeout: Duration) -> Result<u32, GentlError> {
        let mut acks = self.0.acks.lock().unwrap();
        let Some(ack) = acks.pop_front() else {
            return Err(GentlError::Timeout);
        };
        let n = ack.len().min(buf.len());
        buf[..n].copy_from_slice(&ack[..n]);
        Ok(n as u32)
    }
    fn write_bulk(&self, _buf: &[u8], _timeout: Duration) -> Result<(), GentlError> {
        Ok(())
    }
    fn flush(&self) {}
    fn set_raw_io(&self) -> Result<(), GentlError> {
        Ok(())
    }
    fn max_packet(&self) -> u16 {
        FAKE_MPS as u16
    }
}

impl UsbPipe for FakeStreamIn {
    fn read_bulk(&self, buf: &mut [u8], _timeout: Duration) -> Result<u32, GentlError> {
        self.0.stream_read(buf)
    }
    fn write_bulk(&self, _buf: &[u8], _timeout: Duration) -> Result<(), GentlError> {
        Ok(())
    }
    fn flush(&self) {}
    fn set_raw_io(&self) -> Result<(), GentlError> {
        Ok(())
    }
    fn max_packet(&self) -> u16 {
        FAKE_MPS as u16
    }
}

// ---------------------------------------------------------------------------
// Gateway stack on loopback (GVCP front plays the remote side)
// ---------------------------------------------------------------------------

const UP_MAC: [u8; 6] = [0x02, 0xAB, 0xCD, 0xEF, 0x01, 0x02];

struct Stack {
    fake: Arc<FakeU3v>,
    _up_server: GvcpServer,
    space: RegisterSpace,
    down: SharedBack,
    remote: GvcpClient,
}

fn start() -> Stack {
    let fake = Arc::new(FakeU3v::new());
    let back = U3vBack::from_pipes(
        Arc::new(FakeControlOut(Arc::clone(&fake))),
        Arc::new(FakeControlIn(Arc::clone(&fake))),
        vec![Arc::new(FakeStreamIn(Arc::clone(&fake)))],
    )
    .expect("open fake U3V camera");
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
    let up_server =
        GvcpServer::start_bound(Ipv4Addr::LOCALHOST, 0, GigEFront::new(identity, space.clone()))
            .expect("start proxy");
    let up_port = up_server.local_port().unwrap();
    let remote = GvcpClient::connect(Ipv4Addr::LOCALHOST, up_port).expect("connect remote");
    Stack {
        fake,
        _up_server: up_server,
        space,
        down: back,
        remote,
    }
}

fn gev(e: GentlError) -> u16 {
    match e {
        GentlError::GevStatus(s) => s,
        other => panic!("expected a GEV status, got {other}"),
    }
}

#[test]
fn register_passthrough_and_virtualized_identity() {
    let s = start();

    // Vendor register pass-through: the GigE client's numeric address maps
    // 1:1 onto the GenCP address space; the U3V side is little-endian and
    // the value crosses both hops numerically unchanged.
    assert_eq!(s.remote.read_reg(VENDOR_WIDTH as u32).unwrap(), W);
    s.remote.write_reg(VENDOR_WIDTH as u32, 128).unwrap();
    assert_eq!(s.remote.read_reg(VENDOR_WIDTH as u32).unwrap(), 128);
    let on_camera = s.down.lock().unwrap().read_reg(VENDOR_WIDTH as u32).unwrap();
    assert_eq!(on_camera, 128, "write landed on the fake camera");

    // ABRM GenCP version passes through verbatim (LE-decoded by the U3V
    // side, re-encoded BE for the GigE hop — same numeric value).
    assert_eq!(s.remote.read_reg(0x0000).unwrap(), 0x0001_0003);

    // Identity virtualization is local to the register space: the up NIC
    // replaces the camera.
    assert_eq!(
        s.remote.read_reg(reg::CURRENT_IP).unwrap(),
        u32::from(Ipv4Addr::LOCALHOST)
    );
    assert_eq!(
        s.remote.read_reg(reg::DEVICE_MAC_LOW).unwrap(),
        u32::from_be_bytes([UP_MAC[2], UP_MAC[3], UP_MAC[4], UP_MAC[5]])
    );

    // Custom window is transport-agnostic.
    assert_eq!(
        s.remote.read_reg(CUSTOM_BASE + (slot::VERSION as u32) * 4).unwrap(),
        VGIGE_VERSION
    );
    s.space.set_ai_result(1);
    assert_eq!(s.remote.read_reg(CUSTOM_BASE + 4).unwrap(), 1);
}

#[test]
fn error_passthrough_matches_the_camera() {
    let s = start();

    // Unmapped ABRM gap: the fake answers GenCP INVALID_ADDRESS; the code
    // crosses to the GigE client unchanged (the common GenCP/GigE
    // namespace, GenCP 1.3.1 §4.3.2.1 Table 6).
    let via_proxy = gev(s.remote.read_reg(0x0FF0).unwrap_err());
    let via_camera = s.down.lock().unwrap().read_reg(0x0FF0).unwrap_err();
    assert_eq!(via_proxy, via_camera);
    assert_eq!(via_proxy, GENCP_INVALID_ADDRESS);

    // Unaligned access is answered locally by the register space.
    assert_eq!(
        gev(s.remote.read_reg(0x0002).unwrap_err()),
        GEV_STATUS_BAD_ALIGNMENT
    );
}

#[test]
fn xml_injection_over_manifest() {
    let s = start();

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
    assert!(text.contains("</RegisterDescription>"));
    assert!(text.contains("<Category Name=\"SmartCamera\""), "injected nodes");
    assert!(text.contains("WidthReg"), "camera's own node preserved");
    assert!(text.contains("LittleEndian"), "U3V register semantics preserved");
}

#[test]
fn stream_relay_u3v_camera_to_gige_client() {
    let s = start();

    // Virtual channel configuration is local.
    s.remote
        .write_reg(reg::SCDA0, u32::from(Ipv4Addr::LOCALHOST))
        .unwrap();
    let total = 300 + reg::GVSP_HEADERS_OVERHEAD as u32;
    s.remote.write_reg(reg::SCPS0, reg::scps_make(total)).unwrap();

    let rx = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    rx.set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    // Channel open programs the fake's SIRM and raises Stream Enable
    // (§5.4.1.2); the fake starts serving blocks.
    s.remote
        .write_reg(reg::SCP0, reg::scp_make(rx.local_addr().unwrap().port()))
        .unwrap();
    assert_ne!(
        s.fake.reg_u32(SI_CONTROL) & SI_STREAM_ENABLE,
        0,
        "fake camera is streaming"
    );

    // Two complete blocks off the virtual (GigE 3.0) wire.
    let first = recv_block(&rx);
    let second = recv_block(&rx);
    assert_eq!(first.0, 1, "R-259st: the virtual sequence restarts at 1");
    assert_eq!(second.0, 2);
    for (vbid, payload) in [(first.0, &first.1), (second.0, &second.1)] {
        assert_eq!(payload.len(), PAYLOAD, "block {vbid} payload size");
        let matches = (1..=8u64).any(|b| {
            payload
                .iter()
                .enumerate()
                .all(|(k, &byte)| byte == (k as u64 + b) as u8)
        });
        assert!(matches, "block {vbid} payload is the fake's Mono8 pattern");
    }

    let relayed = s
        .remote
        .read_reg(CUSTOM_BASE + (slot::FRAMES_RELAYED as u32) * 4)
        .unwrap();
    assert!(relayed >= 2, "FrameCountRelayed = {relayed}");

    // Close cascades: the driver's FrameSource drops -> U3vStream::stop
    // clears Stream Enable (§5.4.1.2); the fake stops serving blocks.
    s.remote.write_reg(reg::SCP0, 0).unwrap();
    assert_eq!(
        s.fake.reg_u32(SI_CONTROL) & SI_STREAM_ENABLE,
        0,
        "Stream Enable cleared by the cascade"
    );
    rx.set_read_timeout(Some(Duration::from_millis(50))).unwrap();
    let mut buf = [0u8; 2048];
    while rx.recv_from(&mut buf).is_ok() {}
    rx.set_read_timeout(Some(Duration::from_millis(300))).unwrap();
    assert!(
        rx.recv_from(&mut buf).is_err(),
        "datagrams arriving after the channel closed"
    );
}

/// Receive one block from the virtual wire and return (block_id, payload);
/// asserts the 3.0 wire form (EI=1 headers, 40-byte leader body) and the
/// 300-byte virtual-SCPS slicing. Same shape as relay_e2e's helper.
fn recv_block(rx: &UdpSocket) -> (u64, Vec<u8>) {
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
                assert_eq!(n - hlen, IMAGE_LEADER_BODY_LEN, "3.0 40-byte leader body");
                let leader = ImageLeader::decode(&buf[hlen..n]).expect("leader body decodes");
                assert_eq!(leader.pixel_format, MONO8);
                assert_eq!(leader.size_x, W);
                assert_eq!(leader.size_y, H);
            }
            PACKET_FORMAT_DATA_PAYLOAD => {
                max_payload_packet = max_payload_packet.max(n - hlen);
                payload.extend_from_slice(&buf[hlen..n]);
            }
            PACKET_FORMAT_DATA_TRAILER => {
                assert_eq!(decode_image_trailer_body(&buf[hlen..n]).unwrap(), H);
                break (id, ());
            }
            other => panic!("unexpected packet format {other}"),
        }
    };
    assert!(
        max_payload_packet <= 300,
        "payload packets respect the virtual SCPS0 (max {max_payload_packet})"
    );
    (block_id, payload)
}
