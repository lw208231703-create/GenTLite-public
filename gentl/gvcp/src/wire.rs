//! GVCP message headers and payloads. All multi-byte fields are big-endian
//! (spec §15.3). Layouts: docs/10-protocol/gvcp-wirefacts.md.

pub const GVCP_KEY: u8 = 0x42;
pub const GVCP_PORT: u16 = 3956;

/// GVCP flag byte: ACKNOWLEDGE. The spec's header example (3.0 Figure 15-8)
/// sends commands with flag 0x01; Aravis and pyGigEVision use the same value.
/// A non-compliant clone camera was also observed answering flag 0x80 (likely
/// ignoring the bit entirely) — see docs/00-governance/OPEN-QUESTIONS.md Q27.
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
// Message channel commands (§17 message channel dictionary).
pub const EVENT_CMD: u16 = 0x00C0;
pub const EVENT_ACK: u16 = 0x00C1;
pub const EVENTDATA_CMD: u16 = 0x00C2;
pub const EVENTDATA_ACK: u16 = 0x00C3;

/// Fixed part of one event record in EVENT_CMD / EVENTDATA_CMD
/// (§17.1.1 Figure 17-1): event_size u16, event_identifier u16,
/// stream_channel_index u16, reserved u16, block_id u64, timestamp u64.
pub const EVENT_RECORD_LEN: usize = 24;

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
/// Resent GVSP packet marker status (Table 19-1): SUCCESS and
/// GEV_STATUS_PACKET_RESEND must both be accepted as a normal packet.
pub const GEV_STATUS_PACKET_RESEND: u16 = 0x0100;
/// "Packet not yet available" — a resend could not be served YET; the
/// receiver may retry later (CR-205st, Table 16-2 status codes).
pub const GEV_STATUS_NOT_YET_AVAILABLE: u16 = 0x8010;
/// "Packet and previous packets removed from memory" (CR-205st).
pub const GEV_STATUS_PACKET_AND_PREV_REMOVED_FROM_MEMORY: u16 = 0x8011;
/// "Packet removed from memory" (CR-205st).
pub const GEV_STATUS_PACKET_REMOVED_FROM_MEMORY: u16 = 0x8012;
/// "Temporarily unavailable; a hold-off time extension may follow"
/// (CR-205st; the hold-off extension is not parsed — QoI, v1).
pub const GEV_STATUS_TEMPORARILY_UNAVAILABLE: u16 = 0x8014;

/// GVCP flag byte: extended IDs. PACKETRESEND_CMD must set this (CR-200c,
/// Fig 16-13): the request payload then carries the 64-bit block_id.
/// Print bit 3 per the MSB-first convention (Q31) -> 0x10; Aravis
/// ARV_GVCP_COMMAND_EXTENDED_ID_FLAG matches.
pub const FLAG_EXTENDED_ID: u8 = 0x10;

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
/// One asynchronous event decoded from the message channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GvcpEvent {
    /// Event number as defined in the XML device description (§17.1.1).
    pub event_id: u16,
    /// Index of the associated stream channel, 0xFFFF if none.
    pub stream_channel: u16,
    pub block_id: u64,
    /// Device timestamp in Tick-Frequency ticks; 0 when unsupported.
    pub timestamp: u64,
    /// EVENTDATA payload (empty for EVENT_CMD).
    pub data: Vec<u8>,
}

impl GvcpEvent {
    /// Encode in the wire record layout (also the payload layout handed to
    /// the GenTL consumer, so EventGetDataInfo parses it back unchanged).
    pub fn encode(&self) -> Vec<u8> {
        let size = EVENT_RECORD_LEN + self.data.len();
        let mut out = Vec::with_capacity(size);
        out.extend_from_slice(&(size as u16).to_be_bytes());
        out.extend_from_slice(&self.event_id.to_be_bytes());
        out.extend_from_slice(&self.stream_channel.to_be_bytes());
        out.extend_from_slice(&0u16.to_be_bytes());
        out.extend_from_slice(&self.block_id.to_be_bytes());
        out.extend_from_slice(&self.timestamp.to_be_bytes());
        out.extend_from_slice(&self.data);
        out
    }

    /// Decode one event record (EVENT_CMD or EVENTDATA_CMD layout).
    pub fn decode(record: &[u8]) -> Option<Self> {
        if record.len() < EVENT_RECORD_LEN {
            return None;
        }
        let size = usize::from(u16::from_be_bytes([record[0], record[1]]));
        if size < EVENT_RECORD_LEN || size > record.len() {
            return None;
        }
        Some(Self {
            event_id: u16::from_be_bytes([record[2], record[3]]),
            stream_channel: u16::from_be_bytes([record[4], record[5]]),
            block_id: u64::from_be_bytes(record[8..16].try_into().ok()?),
            timestamp: u64::from_be_bytes(record[16..24].try_into().ok()?),
            data: record[EVENT_RECORD_LEN..size].to_vec(),
        })
    }
}

/// Decode one message-channel datagram (EVENT_CMD / EVENTDATA_CMD). Multiple
/// events may be concatenated (§17.1: the event count is length / 24).
pub fn decode_message_events(datagram: &[u8]) -> Option<Vec<GvcpEvent>> {
    let header = CmdHeader::decode(datagram)?;
    if header.command != EVENT_CMD && header.command != EVENTDATA_CMD {
        return None;
    }
    let end = (HEADER_LEN + usize::from(header.length)).min(datagram.len());
    let payload = &datagram[HEADER_LEN..end];
    let mut events = Vec::new();
    let mut offset = 0usize;
    while offset + EVENT_RECORD_LEN <= payload.len() {
        let event = GvcpEvent::decode(&payload[offset..])?;
        offset += EVENT_RECORD_LEN + event.data.len();
        events.push(event);
    }
    Some(events)
}

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
/// WRITEMEM_CMD payload (§16.6.1 Figure 16-11): the 32-bit address followed
/// by the bytes to write (a multiple of 4, R-187c).
pub fn encode_writemem_cmd_payload(addr: u32, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + data.len());
    out.extend_from_slice(&addr.to_be_bytes());
    out.extend_from_slice(data);
    out
}

/// WRITEMEM_ACK payload (§16.6.2): the written start address echoed back
/// (R-190cd lets the application match the ack).
pub fn decode_writemem_ack_payload(payload: &[u8]) -> Option<u32> {
    if payload.len() < 4 {
        return None;
    }
    Some(u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]))
}

pub fn encode_readmem_cmd_payload(addr: u32, count: u16) -> [u8; 8] {
    let mut p = [0u8; 8];
    p[0..4].copy_from_slice(&addr.to_be_bytes());
    // p[4..6] reserved = 0
    p[6..8].copy_from_slice(&count.to_be_bytes());
    p
}

/// PACKETRESEND_CMD payload with extended IDs (CR-200c, Fig 16-13):
/// stream_channel_id u16 + reserved u16 + first_packet_id u32 +
/// last_packet_id u32 + block_id u64 (the 64-bit block_id is present
/// because the command header sets FLAG_EXTENDED_ID). last_packet_id
/// 0xFFFFFFFF means "up to the data trailer".
pub fn encode_packet_resend_payload(channel: u16, block_id: u64, first: u32, last: u32) -> [u8; 20] {
    let mut p = [0u8; 20];
    p[0..2].copy_from_slice(&channel.to_be_bytes());
    // p[2..4] reserved = 0
    p[4..8].copy_from_slice(&first.to_be_bytes());
    p[8..12].copy_from_slice(&last.to_be_bytes());
    p[12..20].copy_from_slice(&block_id.to_be_bytes());
    p
}

/// PACKETRESEND_CMD payload -> (stream_channel_id, block_id,
/// first_packet_id, last_packet_id). Accepts both the extended form
/// (FLAG_EXTENDED_ID set, 20-byte payload, 64-bit block_id) and the
/// legacy form (12-byte payload + 16-bit block_id).
pub fn decode_packet_resend_payload(flag: u8, p: &[u8]) -> Option<(u16, u64, u32, u32)> {
    let (min_len, block_id) = if flag & FLAG_EXTENDED_ID != 0 {
        if p.len() < 20 {
            return None;
        }
        (20, u64::from_be_bytes(p[12..20].try_into().ok()?))
    } else {
        if p.len() < 14 {
            return None;
        }
        (14, u64::from(u16::from_be_bytes([p[12], p[13]])))
    };
    if p.len() < min_len {
        return None;
    }
    Some((
        u16::from_be_bytes([p[0], p[1]]),
        block_id,
        u32::from_be_bytes(p[4..8].try_into().ok()?),
        u32::from_be_bytes(p[8..12].try_into().ok()?),
    ))
}

/// READMEM_ACK payload -> (address, data).
pub fn decode_readmem_payload(p: &[u8]) -> Option<(u32, &[u8])> {
    if p.len() < 4 {
        return None;
    }
    let addr = u32::from_be_bytes([p[0], p[1], p[2], p[3]]);
    Some((addr, &p[4..]))
}

/// FORCEIP_CMD payload size (§16.2.1 Figure 16-3): the MAC words plus three
/// reserved words around each of static_IP, static_subnet_mask and
/// static_default_gateway.
pub const FORCEIP_PAYLOAD_LEN: usize = 56;

/// GVCP flag byte: broadcast the FORCEIP_ACK (§16.2.1 bit 3). The forced
/// address may move the device to another subnet, so the requester may not
/// hear a unicast reply.
pub const FLAG_FORCEIP_ACK_BROADCAST: u8 = 0x08;

/// Decoded FORCEIP_CMD payload (§16.2.1 Figure 16-3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ForceIpRequest {
    /// MAC address of the device to access (48 bits).
    pub mac: [u8; 6],
    /// Static IP address to force into the device (0 = restart the IP
    /// configuration cycle, §16.2.1 R-151cd).
    pub static_ip: u32,
    pub subnet_mask: u32,
    pub default_gateway: u32,
}

/// Device decision for a FORCEIP_CMD (see `DeviceHandler::force_ip`):
/// acknowledge the applied address, or discard the request silently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForceIpOutcome {
    Ack,
    Ignore,
}

/// Decode a FORCEIP_CMD payload: the MAC words are padded (the high word
/// carries MAC[0..2] in its low half) and each address is separated by three
/// reserved words.
pub fn decode_force_ip_payload(payload: &[u8]) -> Option<ForceIpRequest> {
    if payload.len() < FORCEIP_PAYLOAD_LEN {
        return None;
    }
    Some(ForceIpRequest {
        mac: [
            payload[2], payload[3], payload[4], payload[5], payload[6], payload[7],
        ],
        static_ip: u32::from_be_bytes(payload[20..24].try_into().ok()?),
        subnet_mask: u32::from_be_bytes(payload[36..40].try_into().ok()?),
        default_gateway: u32::from_be_bytes(payload[52..56].try_into().ok()?),
    })
}

/// Encode a FORCEIP_CMD payload (device tests / applications).
pub fn encode_force_ip_payload(request: &ForceIpRequest) -> [u8; FORCEIP_PAYLOAD_LEN] {
    let mut out = [0u8; FORCEIP_PAYLOAD_LEN];
    out[2..4].copy_from_slice(&request.mac[0..2]);
    out[4..8].copy_from_slice(&request.mac[2..6]);
    out[20..24].copy_from_slice(&request.static_ip.to_be_bytes());
    out[36..40].copy_from_slice(&request.subnet_mask.to_be_bytes());
    out[52..56].copy_from_slice(&request.default_gateway.to_be_bytes());
    out
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
    fn message_channel_event_roundtrip() {
        let event = GvcpEvent {
            event_id: 0x0007,
            stream_channel: 0xFFFF,
            block_id: 42,
            timestamp: 0x1122_3344_5566_7788,
            data: vec![0xE0, 0xE1, 0xE2, 0xE3],
        };
        let record = event.encode();
        assert_eq!(record.len(), EVENT_RECORD_LEN + 4);
        assert_eq!(GvcpEvent::decode(&record), Some(event.clone()));

        // One EVENTDATA_CMD datagram carrying the record after the GVCP header.
        let mut datagram = vec![0u8; HEADER_LEN];
        let mut header = [0u8; HEADER_LEN];
        CmdHeader {
            flag: FLAG_ACK | 0x08, // bit 3: extended id (§17.1.1)
            command: EVENTDATA_CMD,
            length: record.len() as u16,
            req_id: 3,
        }
        .encode(&mut header);
        datagram[..HEADER_LEN].copy_from_slice(&header);
        let _ = &mut datagram;
        datagram.extend_from_slice(&record);
        let decoded = decode_message_events(&datagram).expect("event decodes");
        assert_eq!(decoded.len(), 1);
        assert_eq!(decoded[0], event);
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

    #[test]
    fn force_ip_payload_roundtrip() {
        let req = ForceIpRequest {
            mac: [0x02, 0x00, 0x11, 0x22, 0x33, 0x44],
            static_ip: 0x0A00_0064,
            subnet_mask: 0xFFFF_FF00,
            default_gateway: 0x0A00_0001,
        };
        let p = encode_force_ip_payload(&req);
        assert_eq!(p.len(), FORCEIP_PAYLOAD_LEN);
        assert_eq!(&p[2..4], &req.mac[0..2]);
        assert_eq!(&p[4..8], &req.mac[2..6]);
        assert_eq!(decode_force_ip_payload(&p), Some(req));
        // Truncated payloads are rejected.
        assert_eq!(decode_force_ip_payload(&p[..55]), None);
    }

    #[test]
    fn writemem_payload_roundtrip() {
        let data = [1u8, 2, 3, 4, 5, 6, 7, 8];
        let p = encode_writemem_cmd_payload(0x2000_0100, &data);
        assert_eq!(p.len(), 12);
        assert_eq!(&p[0..4], &0x2000_0100u32.to_be_bytes());
        assert_eq!(&p[4..], &data);
        assert_eq!(
            decode_writemem_ack_payload(&0x2000_0100u32.to_be_bytes()),
            Some(0x2000_0100)
        );
        assert_eq!(decode_writemem_ack_payload(&[0, 0]), None);
    }
}
