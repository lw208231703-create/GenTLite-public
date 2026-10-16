//! GVCP message headers and payloads. All multi-byte fields are big-endian
//! (spec §15.3). Layouts: notes/gvcp-wirefacts.md.

pub const GVCP_KEY: u8 = 0x42;
pub const GVCP_PORT: u16 = 3956;

/// GVCP flag byte: ACKNOWLEDGE. The spec's header example (3.0 Figure 15-8)
/// sends commands with flag 0x01; Aravis and pyGigEVision use the same value.
/// A non-compliant clone camera was also observed answering flag 0x80 (likely
/// ignoring the bit entirely) — see notes/OPEN-QUESTIONS.md Q27.
pub const FLAG_ACK: u8 = 0x01;

// Command / acknowledge codes (spec §16 dictionary)
pub const DISCOVERY_CMD: u16 = 0x0002;
pub const DISCOVERY_ACK: u16 = 0x0003;
pub const FORCEIP_CMD: u16 = 0x0004;
pub const FORCEIP_ACK: u16 = 0x0005;
pub const PACKETRESEND_CMD: u16 = 0x0040;
pub const READREG_CMD: u16 = 0x0080;
pub const READREG_ACK: u16 = 0x0081;
pub const WRITEREG_CMD: u16 = 0x0082;
pub const WRITEREG_ACK: u16 = 0x0083;
pub const READMEM_CMD: u16 = 0x0084;
pub const READMEM_ACK: u16 = 0x0085;
pub const WRITEMEM_CMD: u16 = 0x0086;
pub const WRITEMEM_ACK: u16 = 0x0087;
pub const PENDING_ACK: u16 = 0x0089;
pub const ACTION_CMD: u16 = 0x0100;
pub const ACTION_ACK: u16 = 0x0101;

// Status codes (spec §19)
pub const GEV_STATUS_SUCCESS: u16 = 0x0000;
pub const GEV_STATUS_NOT_IMPLEMENTED: u16 = 0x8001;
pub const GEV_STATUS_INVALID_PARAMETER: u16 = 0x8002;
pub const GEV_STATUS_INVALID_ADDRESS: u16 = 0x8003;
pub const GEV_STATUS_WRITE_PROTECT: u16 = 0x8004;
pub const GEV_STATUS_BAD_ALIGNMENT: u16 = 0x8005;
pub const GEV_STATUS_ACCESS_DENIED: u16 = 0x8006;
pub const GEV_STATUS_BUSY: u16 = 0x8007;
pub const GEV_STATUS_PACKET_UNAVAILABLE: u16 = 0x800C;
pub const GEV_STATUS_DATA_OVERRUN: u16 = 0x800D;

pub const HEADER_LEN: usize = 8;

/// Command header (spec §15.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CmdHeader {
    pub flag: u8,
    pub command: u16,
    /// Payload bytes following the header.
    pub length: u16,
    pub req_id: u16,
}

impl CmdHeader {
    pub fn new(command: u16, length: u16, req_id: u16) -> Self {
        Self {
            flag: FLAG_ACK,
            command,
            length,
            req_id,
        }
    }

    pub fn encode(&self, out: &mut [u8; HEADER_LEN]) {
        out[0] = GVCP_KEY;
        out[1] = self.flag;
        out[2..4].copy_from_slice(&self.command.to_be_bytes());
        out[4..6].copy_from_slice(&self.length.to_be_bytes());
        out[6..8].copy_from_slice(&self.req_id.to_be_bytes());
    }

    /// Strict parse: wrong key, truncated, or non-ack discovery-style
    /// checks are the caller's job. Returns None on bad key.
    pub fn decode(buf: &[u8]) -> Option<Self> {
        if buf.len() < HEADER_LEN || buf[0] != GVCP_KEY {
            return None;
        }
        Some(Self {
            flag: buf[1],
            command: u16::from_be_bytes([buf[2], buf[3]]),
            length: u16::from_be_bytes([buf[4], buf[5]]),
            req_id: u16::from_be_bytes([buf[6], buf[7]]),
        })
    }
}

/// Acknowledge header (spec §15.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AckHeader {
    pub status: u16,
    pub acknowledge: u16,
    pub length: u16,
    pub ack_id: u16,
}

impl AckHeader {
    pub fn new(status: u16, acknowledge: u16, length: u16, ack_id: u16) -> Self {
        Self {
            status,
            acknowledge,
            length,
            ack_id,
        }
    }

    pub fn encode(&self, out: &mut [u8; HEADER_LEN]) {
        out[0..2].copy_from_slice(&self.status.to_be_bytes());
        out[2..4].copy_from_slice(&self.acknowledge.to_be_bytes());
        out[4..6].copy_from_slice(&self.length.to_be_bytes());
        out[6..8].copy_from_slice(&self.ack_id.to_be_bytes());
    }

    pub fn decode(buf: &[u8]) -> Option<Self> {
        if buf.len() < HEADER_LEN {
            return None;
        }
        Some(Self {
            status: u16::from_be_bytes([buf[0], buf[1]]),
            acknowledge: u16::from_be_bytes([buf[2], buf[3]]),
            length: u16::from_be_bytes([buf[4], buf[5]]),
            ack_id: u16::from_be_bytes([buf[6], buf[7]]),
        })
    }
}

/// Fixed-size string slot in DISCOVERY_ACK / string bootstrap registers.
/// Strings may fill the whole slot without NUL termination (spec §16.1.2).
fn put_str(slot: &mut [u8], s: &str) {
    let bytes = s.as_bytes();
    let n = bytes.len().min(slot.len());
    slot[..n].copy_from_slice(&bytes[..n]);
}

fn get_str(slot: &[u8]) -> String {
    let end = slot.iter().position(|&b| b == 0).unwrap_or(slot.len());
    String::from_utf8_lossy(&slot[..end]).into_owned()
}

/// Payload of DISCOVERY_ACK (232 bytes), mirroring bootstrap registers
/// (spec §16.1.2, Figure 16-2).
#[derive(Debug, Clone)]
pub struct DiscoveryInfo {
    pub spec_version_major: u16,
    pub spec_version_minor: u16,
    pub device_mode: u32,
    pub mac: [u8; 6],
    pub ip_config_options: u32,
    pub ip_config_current: u32,
    pub current_ip: u32,
    pub current_subnet_mask: u32,
    pub default_gateway: u32,
    pub manufacturer_name: String,
    pub model_name: String,
    pub device_version: String,
    pub manufacturer_info: String,
    pub serial_number: String,
    pub user_defined_name: String,
}

pub const DISCOVERY_ACK_PAYLOAD_LEN: usize = 248;

impl DiscoveryInfo {
    pub fn encode_payload(&self) -> [u8; DISCOVERY_ACK_PAYLOAD_LEN] {
        let mut p = [0u8; DISCOVERY_ACK_PAYLOAD_LEN];
        fn w(p: &mut [u8], off: usize, v: u32) {
            p[off..off + 4].copy_from_slice(&v.to_be_bytes());
        }
        p[0..2].copy_from_slice(&self.spec_version_major.to_be_bytes());
        p[2..4].copy_from_slice(&self.spec_version_minor.to_be_bytes());
        w(&mut p, 4, self.device_mode);
        // MAC high: reserved u16 + upper 2 bytes of MAC
        w(&mut p, 8, u16::from_be_bytes([self.mac[0], self.mac[1]]) as u32);
        w(
            &mut p,
            12,
            u32::from_be_bytes([self.mac[2], self.mac[3], self.mac[4], self.mac[5]]),
        );
        w(&mut p, 16, self.ip_config_options);
        w(&mut p, 20, self.ip_config_current);
        w(&mut p, 36, self.current_ip);
        w(&mut p, 52, self.current_subnet_mask);
        w(&mut p, 68, self.default_gateway);
        put_str(&mut p[72..104], &self.manufacturer_name);
        put_str(&mut p[104..136], &self.model_name);
        put_str(&mut p[136..168], &self.device_version);
        put_str(&mut p[168..216], &self.manufacturer_info);
        put_str(&mut p[216..232], &self.serial_number);
        put_str(&mut p[232..248], &self.user_defined_name);
        p
    }

    pub fn decode_payload(p: &[u8]) -> Option<Self> {
        if p.len() < DISCOVERY_ACK_PAYLOAD_LEN {
            return None;
        }
        let u32be = |off: usize| u32::from_be_bytes([p[off], p[off + 1], p[off + 2], p[off + 3]]);
        let mac_hi = u32be(8) as u16;
        Some(Self {
            spec_version_major: u16::from_be_bytes([p[0], p[1]]),
            spec_version_minor: u16::from_be_bytes([p[2], p[3]]),
            device_mode: u32be(4),
            mac: [
                (mac_hi >> 8) as u8,
                mac_hi as u8,
                p[12],
                p[13],
                p[14],
                p[15],
            ],
            ip_config_options: u32be(16),
            ip_config_current: u32be(20),
            current_ip: u32be(36),
            current_subnet_mask: u32be(52),
            default_gateway: u32be(68),
            manufacturer_name: get_str(&p[72..104]),
            model_name: get_str(&p[104..136]),
            device_version: get_str(&p[136..168]),
            manufacturer_info: get_str(&p[168..216]),
            serial_number: get_str(&p[216..232]),
            user_defined_name: get_str(&p[232..248]),
        })
    }
}

/// READREG_CMD payload: N 32-bit-aligned register addresses.
pub fn encode_readreg(addrs: &[u32]) -> Vec<u8> {
    let mut v = Vec::with_capacity(addrs.len() * 4);
    for a in addrs {
        v.extend_from_slice(&a.to_be_bytes());
    }
    v
}

/// READREG_ACK payload -> register values. Error if length is not a
/// multiple of 4 (spec §16.3.2 R-165cd).
pub fn decode_readreg_payload(p: &[u8]) -> Option<Vec<u32>> {
    if p.len() % 4 != 0 {
        return None;
    }
    Some(p.chunks_exact(4).map(|c| u32::from_be_bytes([c[0], c[1], c[2], c[3]])).collect())
}

/// WRITEREG_CMD payload: N (address, value) pairs.
pub fn encode_writereg(pairs: &[(u32, u32)]) -> Vec<u8> {
    let mut v = Vec::with_capacity(pairs.len() * 8);
    for (a, val) in pairs {
        v.extend_from_slice(&a.to_be_bytes());
        v.extend_from_slice(&val.to_be_bytes());
    }
    v
}

/// READMEM_CMD payload: address u32 + reserved u16 + count u16 (spec §16.5.1).
pub fn encode_readmem_cmd_payload(addr: u32, count: u16) -> [u8; 8] {
    let mut p = [0u8; 8];
    p[0..4].copy_from_slice(&addr.to_be_bytes());
    // p[4..6] reserved = 0
    p[6..8].copy_from_slice(&count.to_be_bytes());
    p
}

/// READMEM_ACK payload -> (address, data).
pub fn decode_readmem_payload(p: &[u8]) -> Option<(u32, &[u8])> {
    if p.len() < 4 {
        return None;
    }
    let addr = u32::from_be_bytes([p[0], p[1], p[2], p[3]]);
    Some((addr, &p[4..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cmd_header_roundtrip() {
        let h = CmdHeader::new(READREG_CMD, 12, 42);
        let mut buf = [0u8; 8];
        h.encode(&mut buf);
        assert_eq!(buf[0], 0x42);
        let d = AckHeader::decode(&buf); // must not panic on foreign layout
        assert!(d.is_some());
        let d = CmdHeader::decode(&buf).unwrap();
        assert_eq!(d, h);
    }

    #[test]
    fn ack_header_roundtrip() {
        let h = AckHeader::new(GEV_STATUS_SUCCESS, READREG_ACK, 8, 7);
        let mut buf = [0u8; 8];
        h.encode(&mut buf);
        assert_eq!(AckHeader::decode(&buf).unwrap(), h);
    }

    #[test]
    fn reject_bad_key() {
        let buf = [0u8; 8];
        assert!(CmdHeader::decode(&buf).is_none());
    }

    #[test]
    fn discovery_payload_roundtrip() {
        let info = DiscoveryInfo {
            spec_version_major: 2,
            spec_version_minor: 2,
            device_mode: 1,
            mac: [0x00, 0x11, 0x22, 0x33, 0x44, 0x55],
            ip_config_options: 0x60000000,
            ip_config_current: 0x60000000,
            current_ip: 0xC0A8010A,
            current_subnet_mask: 0xFFFFFF00,
            default_gateway: 0xC0A80101,
            manufacturer_name: "GenTLite".into(),
            model_name: "SimCam".into(),
            device_version: "1.0.0".into(),
            manufacturer_info: "simulated".into(),
            serial_number: "SIM-0001".into(),
            user_defined_name: String::new(),
        };
        let p = info.encode_payload();
        assert_eq!(p.len(), DISCOVERY_ACK_PAYLOAD_LEN);
        let d = DiscoveryInfo::decode_payload(&p).unwrap();
        assert_eq!(d.mac, info.mac);
        assert_eq!(d.current_ip, info.current_ip);
        assert_eq!(d.manufacturer_name, "GenTLite");
        assert_eq!(d.model_name, "SimCam");
        assert_eq!(d.serial_number, "SIM-0001");
        assert_eq!(d.spec_version_major, 2);
    }

    #[test]
    fn writereg_pairs_roundtrip() {
        let p = encode_writereg(&[(0x0A00, 0x80000000), (0x0938, 3000)]);
        assert_eq!(p.len(), 16);
        assert_eq!(&p[0..4], &0x0A00u32.to_be_bytes());
        assert_eq!(&p[4..8], &0x80000000u32.to_be_bytes());
    }

    #[test]
    fn readreg_payload_multiple_of_4() {
        assert!(decode_readreg_payload(&[1, 2, 3]).is_none());
        let vals = decode_readreg_payload(&0xDEADBEEFu32.to_be_bytes()).unwrap();
        assert_eq!(vals, vec![0xDEADBEEF]);
    }

    #[test]
    fn readmem_cmd_payload_layout() {
        let p = encode_readmem_cmd_payload(0x0200, 512);
        assert_eq!(&p[0..4], &0x0200u32.to_be_bytes());
        assert_eq!(&p[4..6], &[0, 0]); // reserved
        assert_eq!(&p[6..8], &512u16.to_be_bytes());
    }
}
