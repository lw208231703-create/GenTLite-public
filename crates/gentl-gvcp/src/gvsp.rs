//! GVSP (GigE Vision Streaming Protocol) packet codec — image payload type.
//! Shared by the producer's receiver (gentl-gige) and the simulator's
//! transmitter so both sides cannot drift apart. Facts: the GVSP section of
//! notes/gvcp-wirefacts.md (spec §24–25).

#![allow(dead_code)]

pub const GVSP_HEADER_LEN: usize = 20;

// packet_format values (spec §24.2)
pub const PACKET_FORMAT_DATA_LEADER: u8 = 1;
pub const PACKET_FORMAT_DATA_TRAILER: u8 = 2;
pub const PACKET_FORMAT_DATA_PAYLOAD: u8 = 3;

// flag bits (Table 24-1)
pub const GEV_FLAG_PACKET_RESEND: u16 = 1 << 15;
pub const GEV_FLAG_PREVIOUS_BLOCK_DROPPED: u16 = 1 << 14;
pub const GEV_FLAG_RESEND_RANGE_ERROR: u16 = 1 << 13;

// payload_type values (Table 25-1)
pub const PAYLOAD_TYPE_IMAGE: u16 = 0x0001;

/// Parsed GVSP common header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GvspHeader {
    pub status: u16,
    pub flag: u16,
    pub extended_id: bool,
    pub packet_format: u8,
    pub block_id: u64,
    pub packet_id: u32,
}

impl GvspHeader {
    pub fn encode(&self, out: &mut [u8; GVSP_HEADER_LEN]) {
        out[0..2].copy_from_slice(&self.status.to_be_bytes());
        out[2..4].copy_from_slice(&self.flag.to_be_bytes());
        out[4] = ((self.extended_id as u8) << 7) | (self.packet_format & 0x0F);
        out[5..8].copy_from_slice(&[0, 0, 0]); // reserved
        out[8..16].copy_from_slice(&self.block_id.to_be_bytes());
        out[16..20].copy_from_slice(&self.packet_id.to_be_bytes());
    }

    pub fn decode(buf: &[u8]) -> Option<Self> {
        if buf.len() < GVSP_HEADER_LEN {
            return None;
        }
        Some(Self {
            status: u16::from_be_bytes([buf[0], buf[1]]),
            flag: u16::from_be_bytes([buf[2], buf[3]]),
            extended_id: buf[4] & 0x80 != 0,
            packet_format: buf[4] & 0x0F,
            block_id: u64::from_be_bytes(buf[8..16].try_into().ok()?),
            packet_id: u32::from_be_bytes(buf[16..20].try_into().ok()?),
        })
    }
}

/// Image data leader fields after the common header (36 bytes, §25.2.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageLeader {
    pub field_id: u8,      // progressive: 0
    pub field_count: u8,   // progressive: 0
    pub timestamp: u64,    // 0 when unsupported
    pub pixel_format: u32, // PFNC value
    pub size_x: u32,
    pub size_y: u32,
    pub offset_x: u32,
    pub offset_y: u32,
    pub padding_x: u32,
    pub padding_y: u32,
}

pub const IMAGE_TRAILER_LEN: usize = 8;

impl ImageLeader {
    pub fn decode(body: &[u8]) -> Option<Self> {
        if body.len() < IMAGE_LEADER_BODY_LEN {
            return None;
        }
        let u32be = |o: usize| -> Option<u32> { Some(u32::from_be_bytes(body[o..o + 4].try_into().ok()?)) };
        let payload_type = u16::from_be_bytes([body[2], body[3]]);
        if payload_type != PAYLOAD_TYPE_IMAGE {
            return None;
        }
        Some(Self {
            field_id: body[0] >> 4,
            field_count: body[0] & 0x0F,
            timestamp: u64::from_be_bytes(body[4..12].try_into().ok()?),
            pixel_format: u32be(12)?,
            size_x: u32be(16)?,
            size_y: u32be(20)?,
            offset_x: u32be(24)?,
            offset_y: u32be(28)?,
            padding_x: u32be(32)?,
            padding_y: u32be(36)?,
        })
    }
}

/// Exact image leader body: 2 (pts) + 2 (pt) + 8 (ts) + 4*8 (pf,x,y,ox,oy,px,py) = 40 bytes.
pub const IMAGE_LEADER_BODY_LEN: usize = 40;

pub fn encode_image_leader_body(l: &ImageLeader) -> [u8; IMAGE_LEADER_BODY_LEN] {
    let mut out = [0u8; IMAGE_LEADER_BODY_LEN];
    out[0] = (l.field_id & 0x0F) << 4 | (l.field_count & 0x0F);
    out[2..4].copy_from_slice(&PAYLOAD_TYPE_IMAGE.to_be_bytes());
    out[4..12].copy_from_slice(&l.timestamp.to_be_bytes());
    out[12..16].copy_from_slice(&l.pixel_format.to_be_bytes());
    out[16..20].copy_from_slice(&l.size_x.to_be_bytes());
    out[20..24].copy_from_slice(&l.size_y.to_be_bytes());
    out[24..28].copy_from_slice(&l.offset_x.to_be_bytes());
    out[28..32].copy_from_slice(&l.offset_y.to_be_bytes());
    out[32..36].copy_from_slice(&l.padding_x.to_be_bytes());
    out[36..40].copy_from_slice(&l.padding_y.to_be_bytes());
    out
}

/// Image trailer body (§25.2.3): reserved u16 + payload_type u16 + size_y u32.
pub fn encode_image_trailer_body(size_y: u32) -> [u8; IMAGE_TRAILER_LEN] {
    let mut out = [0u8; IMAGE_TRAILER_LEN];
    out[2..4].copy_from_slice(&PAYLOAD_TYPE_IMAGE.to_be_bytes());
    out[4..8].copy_from_slice(&size_y.to_be_bytes());
    out
}

pub fn decode_image_trailer_body(body: &[u8]) -> Option<u32> {
    if body.len() < IMAGE_TRAILER_LEN {
        return None;
    }
    if u16::from_be_bytes([body[2], body[3]]) != PAYLOAD_TYPE_IMAGE {
        return None;
    }
    Some(u32::from_be_bytes(body[4..8].try_into().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_roundtrip() {
        let h = GvspHeader {
            status: 0,
            flag: 0,
            extended_id: true,
            packet_format: PACKET_FORMAT_DATA_PAYLOAD,
            block_id: 0x0102030405060708,
            packet_id: 42,
        };
        let mut buf = [0u8; 20];
        h.encode(&mut buf);
        assert_eq!(GvspHeader::decode(&buf).unwrap(), h);
    }

    #[test]
    fn leader_body_roundtrip() {
        let l = ImageLeader {
            field_id: 0,
            field_count: 0,
            timestamp: 0x1122334455667788,
            pixel_format: 0x01080001, // Mono8 (verified against PFNC.h)
            size_x: 640,
            size_y: 480,
            offset_x: 0,
            offset_y: 0,
            padding_x: 0,
            padding_y: 0,
        };
        let body = encode_image_leader_body(&l);
        let d = ImageLeader::decode(&body).unwrap();
        assert_eq!(d.pixel_format, l.pixel_format);
        assert_eq!(d.size_x, 640);
        assert_eq!(d.timestamp, l.timestamp);
        assert_eq!(d.padding_y, 0);
    }

    #[test]
    fn trailer_body_roundtrip() {
        let body = encode_image_trailer_body(480);
        assert_eq!(decode_image_trailer_body(&body).unwrap(), 480);
        assert!(decode_image_trailer_body(&body[..4]).is_none());
    }
}
