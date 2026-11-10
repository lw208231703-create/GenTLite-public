//! GVSP (GigE Vision Streaming Protocol) packet codec — image payload type.
//! Shared by the producer's receiver (gentl-gige) and the simulator's
//! transmitter so both sides cannot drift apart. Facts: the GVSP section of
//! notes/gvcp-wirefacts.md (spec §24–25).
//!
//! Two header forms are coded here:
//! - the 3.0 form (§24.2): 20 bytes, Extended ID bit set, 64-bit block_id;
//! - the 1.2 form (1.2 §24 Fig 24-1, layout verified against the rendered
//!   figure and Aravis 0.8.30 `arvgvspprivate.h` — Q34): 8 bytes, no flag
//!   field, block_id u16 @2-3, packet_format u8 @4 (the whole byte; its top
//!   bit became the EI bit in 2.0), packet_id u24 @5-7.

#![allow(dead_code)]

pub const GVSP_HEADER_LEN: usize = 20;

/// 1.2 §24 Fig 24-1: the legacy (EI=0) GVSP header is 8 bytes.
pub const GVSP_LEGACY_HEADER_LEN: usize = 8;

/// IP + UDP + legacy GVSP header bytes counted in a 1.2 device's SCPSx
/// packet_size field (1.2 §27.46 "total packet size including IP, UDP and
/// GVSP header"): 1.2 cameras carry an 8-byte GVSP header, so the generic
/// payload data length is packet_size - 36 (verified against the Q28
/// capture: SCPS=1500 -> 1472-byte UDP datagrams -> 1464 payload bytes).
pub const GVSP_LEGACY_HEADERS_OVERHEAD: u32 = 20 + 8 + GVSP_LEGACY_HEADER_LEN as u32;

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
    /// Wire length of this header's form: 20 bytes with extended ids
    /// (EI=1), 8 bytes in the 1.2 legacy form (EI=0).
    pub fn header_len(&self) -> usize {
        if self.extended_id {
            GVSP_HEADER_LEN
        } else {
            GVSP_LEGACY_HEADER_LEN
        }
    }

    /// Encode the 3.0 (extended, 20-byte) form; `extended_id` MUST be true
    /// ([`encode_legacy`] covers the 1.2 form).
    pub fn encode(&self, out: &mut [u8; GVSP_HEADER_LEN]) {
        out[0..2].copy_from_slice(&self.status.to_be_bytes());
        out[2..4].copy_from_slice(&self.flag.to_be_bytes());
        out[4] = ((self.extended_id as u8) << 7) | (self.packet_format & 0x0F);
        out[5..8].copy_from_slice(&[0, 0, 0]); // reserved
        out[8..16].copy_from_slice(&self.block_id.to_be_bytes());
        out[16..20].copy_from_slice(&self.packet_id.to_be_bytes());
    }

    /// Encode the 1.2 legacy form (1.2 §24 Fig 24-1): status u16, block_id
    /// (low 16 bits) u16, packet_format u8 (the whole byte; 1.2 has no EI
    /// bit — the top bit stayed reserved until 2.0 claimed it, so mask to
    /// 7 bits like Aravis), packet_id (low 24 bits) u24. No flag field.
    pub fn encode_legacy(&self, out: &mut [u8; GVSP_LEGACY_HEADER_LEN]) {
        out[0..2].copy_from_slice(&self.status.to_be_bytes());
        out[2..4].copy_from_slice(&(self.block_id as u16).to_be_bytes());
        out[4] = self.packet_format & 0x7F;
        out[5..8].copy_from_slice(&self.packet_id.to_be_bytes()[1..4]);
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

    /// Decode the 1.2 legacy form (1.2 §24 Fig 24-1).
    pub fn decode_legacy(buf: &[u8]) -> Option<Self> {
        if buf.len() < GVSP_LEGACY_HEADER_LEN {
            return None;
        }
        Some(Self {
            status: u16::from_be_bytes([buf[0], buf[1]]),
            // 1.2 has no flag field.
            flag: 0,
            extended_id: false,
            packet_format: buf[4] & 0x7F,
            block_id: u64::from(u16::from_be_bytes([buf[2], buf[3]])),
            packet_id: u32::from_be_bytes([0, buf[5], buf[6], buf[7]]),
        })
    }

    /// Decode either header form, returning the header and its wire length.
    /// The discriminator is the Extended ID bit (3.0 §24.2: "EI flag must
    /// be set to 1 to inform the GVSP receiver that the block IDs are 64
    /// bits"), which lives at byte 4 top bit in the 20-byte form. A legacy
    /// (1.2) packet never sets it: byte 4 is the packet_format byte and all
    /// assigned formats are < 0x80 (QoI scope: same as the rest of the
    /// receiver — 2.0-only layouts are Q34). A datagram of 8-19 bytes with
    /// the EI bit set cannot be a complete 3.0 header and decodes as
    /// legacy; it was malformed for the strict decoder before, and an
    /// unknown legacy format still counts malformed downstream.
    pub fn decode_auto(buf: &[u8]) -> Option<(Self, usize)> {
        if buf.len() >= GVSP_HEADER_LEN && buf[4] & 0x80 != 0 {
            Self::decode(buf).map(|h| (h, GVSP_HEADER_LEN))
        } else if buf.len() >= GVSP_LEGACY_HEADER_LEN {
            Self::decode_legacy(buf).map(|h| (h, GVSP_LEGACY_HEADER_LEN))
        } else {
            None
        }
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

    /// Decode the 1.2 legacy body (1.2 §24 Fig 24-3, 36 bytes — see
    /// [`IMAGE_LEADER_LEGACY_BODY_LEN`]).
    pub fn decode_legacy(body: &[u8]) -> Option<Self> {
        if body.len() < IMAGE_LEADER_LEGACY_BODY_LEN {
            return None;
        }
        let u32be = |o: usize| -> Option<u32> { Some(u32::from_be_bytes(body[o..o + 4].try_into().ok()?)) };
        let payload_type = u16::from_be_bytes([body[2], body[3]]);
        if payload_type != PAYLOAD_TYPE_IMAGE {
            return None;
        }
        Some(Self {
            field_id: 0,      // 1.2 has no payload_type_specific
            field_count: 0,
            timestamp: u64::from_be_bytes(body[4..12].try_into().ok()?),
            pixel_format: u32be(12)?,
            size_x: u32be(16)?,
            size_y: u32be(20)?,
            offset_x: u32be(24)?,
            offset_y: u32be(28)?,
            padding_x: u32::from(u16::from_be_bytes([body[32], body[33]])),
            padding_y: u32::from(u16::from_be_bytes([body[34], body[35]])),
        })
    }
}

/// Exact image leader body: 2 (pts) + 2 (pt) + 8 (ts) + 4*8 (pf,x,y,ox,oy,px,py) = 40 bytes.
pub const IMAGE_LEADER_BODY_LEN: usize = 40;

/// 1.2 §24 Fig 24-3 Image Data Leader body: 2 (reserved) + 2 (payload
/// type) + 8 (timestamp) + pixel_format/size_x/size_y/offset_x/offset_y
/// (5 x u32) + padding_x/padding_y (2 x u16) = 36 bytes. The 3.0 body
/// (§25.2.1) instead opens with the 2-byte payload_type_specific
/// (field_id/field_count) and uses u32 paddings; the trailing padding
/// width difference is why the two bodies differ in length (36 vs 40)
/// despite carrying the same fields.
pub const IMAGE_LEADER_LEGACY_BODY_LEN: usize = 36;

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

/// 1.2 §24 Fig 24-3 legacy image leader body (see
/// [`IMAGE_LEADER_LEGACY_BODY_LEN`]). field_id/field_count do not exist in
/// 1.2 (decoded as 0).
pub fn encode_image_leader_body_legacy(l: &ImageLeader) -> [u8; IMAGE_LEADER_LEGACY_BODY_LEN] {
    let mut out = [0u8; IMAGE_LEADER_LEGACY_BODY_LEN];
    out[2..4].copy_from_slice(&PAYLOAD_TYPE_IMAGE.to_be_bytes());
    out[4..12].copy_from_slice(&l.timestamp.to_be_bytes());
    out[12..16].copy_from_slice(&l.pixel_format.to_be_bytes());
    out[16..20].copy_from_slice(&l.size_x.to_be_bytes());
    out[20..24].copy_from_slice(&l.size_y.to_be_bytes());
    out[24..28].copy_from_slice(&l.offset_x.to_be_bytes());
    out[28..32].copy_from_slice(&l.offset_y.to_be_bytes());
    // 1.2 carries the paddings as u16 each (Fig 24-3 last row).
    out[32..34].copy_from_slice(&(l.padding_x as u16).to_be_bytes());
    out[34..36].copy_from_slice(&(l.padding_y as u16).to_be_bytes());
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

    /// 1.2 §24 Fig 24-1: 8-byte header, byte layout status u16 / block_id
    /// u16 / packet_format u8 / packet_id u24, no flag field.
    #[test]
    fn legacy_header_roundtrip_and_byte_layout() {
        let h = GvspHeader {
            status: 0,
            flag: 0,
            extended_id: false,
            packet_format: PACKET_FORMAT_DATA_PAYLOAD,
            block_id: 0x0102,
            packet_id: 0x030405,
        };
        let mut buf = [0u8; GVSP_LEGACY_HEADER_LEN];
        h.encode_legacy(&mut buf);
        assert_eq!(
            buf,
            [0x00, 0x00, 0x01, 0x02, 0x03, 0x03, 0x04, 0x05],
            "status | block_id | format byte | packet_id u24"
        );
        assert_eq!(GvspHeader::decode_legacy(&buf).unwrap(), h);
        assert_eq!(h.header_len(), GVSP_LEGACY_HEADER_LEN);
        // The format byte's top bit is reserved (2.0 claimed it as EI):
        // formats above 0x7f cannot be encoded.
        let big = GvspHeader { packet_format: 0x83, ..h };
        big.encode_legacy(&mut buf);
        assert_eq!(buf[4] & 0x80, 0);
    }

    /// decode_auto discriminates on the EI bit (byte 4 top bit, 3.0 §24.2):
    /// 20-byte EI=1 packets decode extended; everything else decodes as the
    /// 1.2 8-byte form (legacy format bytes never set the top bit).
    #[test]
    fn decode_auto_picks_the_header_form() {
        let ext = GvspHeader {
            status: 1,
            flag: 2,
            extended_id: true,
            packet_format: PACKET_FORMAT_DATA_PAYLOAD,
            block_id: 0x0102030405060708,
            packet_id: 42,
        };
        let mut buf = [0u8; GVSP_HEADER_LEN];
        ext.encode(&mut buf);
        let (d, n) = GvspHeader::decode_auto(&buf).unwrap();
        assert_eq!(n, GVSP_HEADER_LEN);
        assert_eq!(d, ext);

        let leg = GvspHeader {
            status: 0,
            flag: 0,
            extended_id: false,
            packet_format: PACKET_FORMAT_DATA_LEADER,
            block_id: 7,
            packet_id: 3,
        };
        let mut lbuf = [0u8; GVSP_LEGACY_HEADER_LEN + 4];
        leg.encode_legacy((&mut lbuf[..GVSP_LEGACY_HEADER_LEN]).try_into().unwrap());
        let (d, n) = GvspHeader::decode_auto(&lbuf).unwrap();
        assert_eq!(n, GVSP_LEGACY_HEADER_LEN);
        assert_eq!(d, leg);

        assert!(GvspHeader::decode_auto(&[0u8; 7]).is_none());
    }

    /// 1.2 §24 Fig 24-3 legacy leader body: reserved u16 + payload_type +
    /// timestamp + 5 x u32 + 2 x u16 padding = 36 bytes; the u16 paddings
    /// are the length difference vs the 40-byte 3.0 body.
    #[test]
    fn legacy_leader_body_roundtrip() {
        let l = ImageLeader {
            field_id: 0,
            field_count: 0,
            timestamp: 0x1122334455667788,
            pixel_format: 0x01080001,
            size_x: 640,
            size_y: 480,
            offset_x: 1,
            offset_y: 2,
            padding_x: 3,
            padding_y: 4,
        };
        let body = encode_image_leader_body_legacy(&l);
        assert_eq!(body.len(), IMAGE_LEADER_LEGACY_BODY_LEN);
        assert_eq!(&body[0..2], &[0, 0], "1.2 opens with a reserved u16");
        assert_eq!(u16::from_be_bytes([body[2], body[3]]), 1);
        let d = ImageLeader::decode_legacy(&body).unwrap();
        assert_eq!(d.pixel_format, l.pixel_format);
        assert_eq!(d.size_x, 640);
        assert_eq!(d.size_y, 480);
        assert_eq!(d.offset_x, 1);
        assert_eq!(d.offset_y, 2);
        assert_eq!(d.padding_x, 3);
        assert_eq!(d.padding_y, 4);
        assert_eq!(d.timestamp, l.timestamp);
        assert!(ImageLeader::decode_legacy(&body[..35]).is_none());
    }
}
