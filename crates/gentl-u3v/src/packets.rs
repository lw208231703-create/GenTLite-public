//! Wire codec for the USB3 Vision control/event/stream formats and the
//! GenCP command layer inside them. Pure functions only — every fact here
//! is traced to the spec in notes/u3v-wirefacts.md (U3V 1.2 / GenCP 1.3.1
//! chapter numbers).
//!
//! Byte order: all bootstrap registers and protocol fields are
//! little-endian (U3V 1.2 R-30cd, §4.1.2); data blocks too (R-75st, §5.5.1).

// ---------------------------------------------------------------------------
// GenCP command layer (GenCP 1.3.1 §4)
// ---------------------------------------------------------------------------

/// U3V control-channel prefix: ASCII "U3VC" with `U` in the LSB (§4.1.3.1).
pub const PREFIX_CONTROL: u32 = 0x4356_3355;
/// U3V event-channel prefix: ASCII "U3VE" with `U` in the LSB (§4.1.3.1).
// Event-channel codec: implemented and unit-tested, but no GenTL event
// delivery consumes it yet (notes/OPEN-QUESTIONS.md UQ7).
#[allow(dead_code)]
pub const PREFIX_EVENT: u32 = 0x4556_3355;
/// U3V uses no postfix (R-31cd, §4.1.3.1).

// Command IDs (GenCP 1.3.1 §4.3.3 Table 7).
pub const READMEM_CMD: u16 = 0x0800;
pub const READMEM_ACK: u16 = 0x0801;
pub const WRITEMEM_CMD: u16 = 0x0802;
pub const WRITEMEM_ACK: u16 = 0x0803;
pub const PENDING_ACK: u16 = 0x0805;
#[allow(dead_code)] // stacked commands exist in GenCP; not used by this producer (GenTL.h stacked entry points stay stubs)
pub const READMEM_STACKED_CMD: u16 = 0x0806;
#[allow(dead_code)]
pub const READMEM_STACKED_ACK: u16 = 0x0807;
#[allow(dead_code)]
pub const WRITEMEM_STACKED_CMD: u16 = 0x0808;
#[allow(dead_code)]
pub const WRITEMEM_STACKED_ACK: u16 = 0x0809;
#[allow(dead_code)] // DEI event channel follow-up (UQ7)
pub const EVENT_CMD: u16 = 0x0C00;

// CCD request flags (GenCP 1.3.1 §4.3.1). U3V supports only RequestAck;
// CommandResend must be answered by the device with
// U3V_STATUS_RESEND_NOT_SUPPORTED (R-134cd).
pub const FLAG_REQUEST_ACK: u16 = 1 << 14;
#[allow(dead_code)] // resend is device-side only for U3V (R-134cd); never sent
pub const FLAG_COMMAND_RESEND: u16 = 1 << 15;

/// Fixed sizes of the transfer envelope, prefix + CCD.
pub const PREFIX_LEN: usize = 4;
pub const CCD_LEN: usize = 8;
pub const CMD_OVERHEAD: usize = PREFIX_LEN + CCD_LEN;
/// READMEM_CMD SCD: u64 address + u16 reserved + u16 length (GenCP §4.4.1).
pub const READMEM_SCD_LEN: usize = 12;
/// WRITEMEM_CMD SCD header: u64 address, then data (GenCP §4.4.3).
pub const WRITEMEM_SCD_HEADER_LEN: usize = 8;
/// WRITEMEM_ACK SCD when the Written-Length-Field capability is set:
/// u16 reserved + u16 length_written (GenCP §4.4.4).
pub const WRITEMEM_ACK_SCD_LEN: usize = 4;

// GenCP status codes (GenCP 1.3.1 §4.3.2.1 Table 6).
pub const GENCP_SUCCESS: u16 = 0x0000;
pub const GENCP_NOT_IMPLEMENTED: u16 = 0x8001;
pub const GENCP_INVALID_PARAMETER: u16 = 0x8002;
pub const GENCP_INVALID_ADDRESS: u16 = 0x8003;
pub const GENCP_WRITE_PROTECT: u16 = 0x8004;
pub const GENCP_BAD_ALIGNMENT: u16 = 0x8005;
pub const GENCP_ACCESS_DENIED: u16 = 0x8006;
pub const GENCP_BUSY: u16 = 0x8007;
pub const GENCP_MSG_TIMEOUT: u16 = 0x800B;
pub const GENCP_INVALID_HEADER: u16 = 0x800E;
pub const GENCP_WRONG_CONFIG: u16 = 0x800F;
pub const GENCP_ERROR: u16 = 0x8FFF;

// U3V technology-specific status codes, namespace = 1 (R-38cd, Table 4-2/4-3).
pub const U3V_STATUS_RESEND_NOT_SUPPORTED: u16 = 0xA001;
pub const U3V_STATUS_DSI_ENDPOINT_HALTED: u16 = 0xA002;
pub const U3V_STATUS_SI_PAYLOAD_SIZE_NOT_ALIGNED: u16 = 0xA003;
pub const U3V_STATUS_SI_REGISTERS_INCONSISTENT: u16 = 0xA004;
pub const U3V_STATUS_DEI_ENDPOINT_HALTED: u16 = 0xA005;
pub const U3V_STATUS_DATA_DISCARDED: u16 = 0xA100;
pub const U3V_STATUS_DATA_OVERRUN: u16 = 0xA101;

/// Human-readable name for a GenCP/U3V status code; unknown codes render
/// as hex (namespace/severity bits included verbatim).
pub fn status_name(code: u16) -> String {
    let name = match code {
        GENCP_SUCCESS => "GENCP_SUCCESS",
        GENCP_NOT_IMPLEMENTED => "GENCP_NOT_IMPLEMENTED",
        GENCP_INVALID_PARAMETER => "GENCP_INVALID_PARAMETER",
        GENCP_INVALID_ADDRESS => "GENCP_INVALID_ADDRESS",
        GENCP_WRITE_PROTECT => "GENCP_WRITE_PROTECT",
        GENCP_BAD_ALIGNMENT => "GENCP_BAD_ALIGNMENT",
        GENCP_ACCESS_DENIED => "GENCP_ACCESS_DENIED",
        GENCP_BUSY => "GENCP_BUSY",
        GENCP_MSG_TIMEOUT => "GENCP_MSG_TIMEOUT",
        GENCP_INVALID_HEADER => "GENCP_INVALID_HEADER",
        GENCP_WRONG_CONFIG => "GENCP_WRONG_CONFIG",
        GENCP_ERROR => "GENCP_ERROR",
        U3V_STATUS_RESEND_NOT_SUPPORTED => "U3V_STATUS_RESEND_NOT_SUPPORTED",
        U3V_STATUS_DSI_ENDPOINT_HALTED => "U3V_STATUS_DSI_ENDPOINT_HALTED",
        U3V_STATUS_SI_PAYLOAD_SIZE_NOT_ALIGNED => "U3V_STATUS_SI_PAYLOAD_SIZE_NOT_ALIGNED",
        U3V_STATUS_SI_REGISTERS_INCONSISTENT => "U3V_STATUS_SI_REGISTERS_INCONSISTENT",
        U3V_STATUS_DEI_ENDPOINT_HALTED => "U3V_STATUS_DEI_ENDPOINT_HALTED",
        U3V_STATUS_DATA_DISCARDED => "U3V_STATUS_DATA_DISCARDED",
        U3V_STATUS_DATA_OVERRUN => "U3V_STATUS_DATA_OVERRUN",
        other => return format!("status 0x{other:04X}"),
    };
    name.to_owned()
}

/// The 8-byte Common Command Data of a command packet (GenCP §4.3.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandCcd {
    pub flags: u16,
    pub command_id: u16,
    /// Length of the SCD, not including prefix, CCD or postfix.
    pub length: u16,
    pub request_id: u16,
}

impl CommandCcd {
    pub fn encode(&self, out: &mut [u8]) {
        out[0..2].copy_from_slice(&self.flags.to_le_bytes());
        out[2..4].copy_from_slice(&self.command_id.to_le_bytes());
        out[4..6].copy_from_slice(&self.length.to_le_bytes());
        out[6..8].copy_from_slice(&self.request_id.to_le_bytes());
    }
}

/// The 8-byte CCD of an acknowledge packet (GenCP §4.3.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AckCcd {
    pub status: u16,
    pub command_id: u16,
    pub length: u16,
    pub request_id: u16,
}

impl AckCcd {
    /// Decode from the CCD area of a transfer (already past the prefix).
    pub fn decode(buf: &[u8]) -> Option<Self> {
        if buf.len() < CCD_LEN {
            return None;
        }
        Some(Self {
            status: u16::from_le_bytes([buf[0], buf[1]]),
            command_id: u16::from_le_bytes([buf[2], buf[3]]),
            length: u16::from_le_bytes([buf[4], buf[5]]),
            request_id: u16::from_le_bytes([buf[6], buf[7]]),
        })
    }
}

/// Why an incoming acknowledge transfer is being rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AckError {
    TooShort,
    BadPrefix,
    UnexpectedCommand { expected: u16, got: u16 },
    RequestIdMismatch { expected: u16, got: u16 },
}

/// Validate the envelope of an acknowledge transfer and return its CCD.
/// `prefix` selects the control or event channel magic.
pub fn decode_ack(prefix: u32, buf: &[u8], expect_command: u16, expect_request_id: u16) -> Result<AckCcd, AckError> {
    if buf.len() < CMD_OVERHEAD {
        return Err(AckError::TooShort);
    }
    let got_prefix = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
    if got_prefix != prefix {
        return Err(AckError::BadPrefix);
    }
    let ccd = AckCcd::decode(&buf[PREFIX_LEN..]).ok_or(AckError::TooShort)?;
    if ccd.command_id != expect_command {
        return Err(AckError::UnexpectedCommand {
            expected: expect_command,
            got: ccd.command_id,
        });
    }
    if ccd.request_id != expect_request_id {
        return Err(AckError::RequestIdMismatch {
            expected: expect_request_id,
            got: ccd.request_id,
        });
    }
    Ok(ccd)
}

/// Encode a READMEM command transfer. `len` is the byte count (u16 field,
/// GenCP §4.4.1). Returns the full 24-byte transfer.
pub fn encode_readmem(request_id: u16, addr: u64, len: u16) -> [u8; CMD_OVERHEAD + READMEM_SCD_LEN] {
    let mut out = [0u8; CMD_OVERHEAD + READMEM_SCD_LEN];
    out[0..4].copy_from_slice(&PREFIX_CONTROL.to_le_bytes());
    CommandCcd {
        flags: FLAG_REQUEST_ACK,
        command_id: READMEM_CMD,
        length: READMEM_SCD_LEN as u16,
        request_id,
    }
    .encode(&mut out[PREFIX_LEN..PREFIX_LEN + CCD_LEN]);
    out[CMD_OVERHEAD..CMD_OVERHEAD + 8].copy_from_slice(&addr.to_le_bytes());
    // reserved field stays 0
    out[CMD_OVERHEAD + 10..CMD_OVERHEAD + 12].copy_from_slice(&len.to_le_bytes());
    out
}

/// Encode a WRITEMEM command transfer (GenCP §4.4.3): prefix + CCD +
/// u64 address + payload.
pub fn encode_writemem(request_id: u16, addr: u64, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(CMD_OVERHEAD + WRITEMEM_SCD_HEADER_LEN + data.len());
    out.extend_from_slice(&PREFIX_CONTROL.to_le_bytes());
    let mut ccd = [0u8; CCD_LEN];
    CommandCcd {
        flags: FLAG_REQUEST_ACK,
        command_id: WRITEMEM_CMD,
        length: (WRITEMEM_SCD_HEADER_LEN + data.len()) as u16,
        request_id,
    }
    .encode(&mut ccd);
    out.extend_from_slice(&ccd);
    out.extend_from_slice(&addr.to_le_bytes());
    out.extend_from_slice(data);
    out
}

// ---------------------------------------------------------------------------
// Bootstrap register maps (ABRM / SBRM / SIRM / EIRM)
// ---------------------------------------------------------------------------

/// Technology Agnostic Bootstrap Register Map (GenCP 1.3.1 §5.4 as
/// incorporated by U3V 1.2 Table 4-6). All offsets from 0x0.
/// Full map transcribed from the spec; the producer consumes a subset.
#[allow(dead_code)]
pub mod abrm {
    pub const GENCPC_VERSION: u64 = 0x0000; // 4, R: 2x u16 (minor@0, major@16)
    pub const MANUFACTURER_NAME: u64 = 0x0004; // 64, R, ASCII
    pub const MODEL_NAME: u64 = 0x0044; // 64, R
    pub const FAMILY_NAME: u64 = 0x0084; // 64, R (CM)
    pub const DEVICE_VERSION: u64 = 0x00C4; // 64, R
    #[allow(dead_code)] // register map reference; not consumed yet
    pub const MANUFACTURER_INFO: u64 = 0x0104; // 64, R
    pub const SERIAL_NUMBER: u64 = 0x0144; // 64, R
    pub const USER_DEFINED_NAME: u64 = 0x0184; // 64, RW (CM)
    pub const DEVICE_CAPABILITY: u64 = 0x01C4; // 8, R
    pub const MAX_DEVICE_RESPONSE_TIME: u64 = 0x01CC; // 4, R, ms
    pub const MANIFEST_TABLE: u64 = 0x01D0; // 8, R: address of the manifest
    pub const SBRM_ADDRESS: u64 = 0x01D8; // 8, R (CM on capability bit 9)
    pub const DEVICE_CONFIGURATION: u64 = 0x01E0; // 8, RW (MultiEvent enable bit 0)
    pub const HEARTBEAT_TIMEOUT: u64 = 0x01E8; // 4 — not used by U3V
    pub const MESSAGE_CHANNEL_ID: u64 = 0x01EC; // 4 — not used by U3V
    pub const TIMESTAMP: u64 = 0x01F0; // 8, R, ns
    pub const TIMESTAMP_LATCH: u64 = 0x01F8; // 4, W
    pub const TIMESTAMP_INCREMENT: u64 = 0x01FC; // 8, R
    pub const ACCESS_PRIVILEGE: u64 = 0x0204; // 4 — not used by U3V
    pub const PROTOCOL_ENDIANESS: u64 = 0x0208; // 4 — deprecated in GenCP 1.3.1
    pub const IMPLEMENTATION_ENDIANESS: u64 = 0x020C; // 4, R: only LE supported
    pub const DEVICE_SW_VERSION: u64 = 0x0210; // 64, R (CM)
    pub const STRING_LEN: usize = 64;

    // Device Capability bits (GenCP 1.3.1 §5.4.9 Table 21). Verified
    // twice: spec page 54 read with `pdftotext -raw` (the -layout mode
    // scrambles this table's columns), and corroborated by a real device
    // (Hikrobot MV-CS016-10UC reports 0x0e09 = UserDefinedName+Timestamp+
    // SBRM+EndiannessReg+WrittenLength, 2026-10-13). An attempted
    // "correction" of this mapping from a garbled extraction was reverted;
    // this table is the reference.
    pub const CAP_USER_DEFINED_NAME: u64 = 1 << 0;
    pub const CAP_ACCESS_PRIVILEGE: u64 = 1 << 1;
    pub const CAP_MESSAGE_CHANNEL: u64 = 1 << 2;
    pub const CAP_TIMESTAMP: u64 = 1 << 3;
    pub const CAP_STRING_ENCODING_MASK: u64 = 0xF << 4; // 0 = ASCII
    pub const CAP_FAMILY_NAME: u64 = 1 << 8;
    pub const CAP_SBRM: u64 = 1 << 9;
    pub const CAP_ENDIANNESS_REGISTER: u64 = 1 << 10;
    pub const CAP_WRITTEN_LENGTH_FIELD: u64 = 1 << 11;
    pub const CAP_MULTI_EVENT: u64 = 1 << 12;
    pub const CAP_STACKED_COMMANDS: u64 = 1 << 13;
    pub const CAP_DEVICE_SW_VERSION: u64 = 1 << 14;
}

/// Technology Specific Bootstrap Register Map (U3V 1.2 Table 4-7).
/// Offsets relative to the SBRM base address.
#[allow(dead_code)]
pub mod sbrm {
    pub const U3V_VERSION: u64 = 0x00; // 4, R: 2x u16 (minor@0, major@16)
    pub const U3VCP_CAPABILITY: u64 = 0x04; // 8, R
    pub const U3VCP_CONFIGURATION: u64 = 0x0C; // 8, RW
    pub const MAX_COMMAND_TRANSFER_LENGTH: u64 = 0x14; // 4, R
    pub const MAX_ACK_TRANSFER_LENGTH: u64 = 0x18; // 4, R
    pub const NUMBER_OF_STREAM_CHANNELS: u64 = 0x1C; // 4, R
    pub const SIRM_ADDRESS: u64 = 0x20; // 8, R (CM: SIRM available)
    pub const SIRM_LENGTH: u64 = 0x28; // 4, R
    pub const EIRM_ADDRESS: u64 = 0x2C; // 8, R (CM: EIRM available)
    pub const EIRM_LENGTH: u64 = 0x34; // 4, R
    pub const IIDC2_ADDRESS: u64 = 0x38; // 8, R (CM: IIDC2 available)
    pub const CURRENT_SPEED: u64 = 0x40; // 4, R

    pub const CAP_SIRM: u64 = 1 << 0;
    pub const CAP_EIRM: u64 = 1 << 1;
    pub const CAP_IIDC2: u64 = 1 << 2;
}

/// Streaming Interface Register Map (U3V 1.2 Table 5-1/5-2). Offsets
/// relative to the SIRM base of the stream channel.
#[allow(dead_code)]
pub mod sirm {
    pub const SI_INFO: u64 = 0x00; // 4, R
    pub const SI_CONTROL: u64 = 0x04; // 4, RW: bit 0 = Stream Enable
    pub const SI_REQUIRED_PAYLOAD_SIZE: u64 = 0x08; // 8, R, UINT64
    pub const SI_REQUIRED_LEADER_SIZE: u64 = 0x10; // 4, R
    pub const SI_REQUIRED_TRAILER_SIZE: u64 = 0x14; // 4, R
    pub const SI_MAX_LEADER_SIZE: u64 = 0x18; // 4, RW*
    pub const SI_PAYLOAD_TRANSFER_SIZE: u64 = 0x1C; // 4, RW*
    pub const SI_PAYLOAD_TRANSFER_COUNT: u64 = 0x20; // 4, RW*
    pub const SI_PAYLOAD_FINAL1_SIZE: u64 = 0x24; // 4, RW*
    pub const SI_PAYLOAD_FINAL2_SIZE: u64 = 0x28; // 4, RW*
    pub const SI_MAX_TRAILER_SIZE: u64 = 0x2C; // 4, RW*
    pub const SI_SUPPORTED_PAYLOAD_MODES: u64 = 0x30; // 4, R (CM)
    pub const SI_PAYLOAD_MODE: u64 = 0x34; // 4, RW* (CM): bit0 = Standard
    pub const SI_GSIRM_ADDRESS: u64 = 0x38; // 8, R (CM)
    pub const SI_GSIRM_SIZE: u64 = 0x40; // 4, R (CM)

    pub const CONTROL_STREAM_ENABLE: u32 = 1 << 0;
    pub const INFO_ALIGNMENT_SHIFT: u32 = 24; // bits 24-31: 2^n size alignment
    pub const MODE_STANDARD: u32 = 1 << 0;
}

/// Event Interface Register Map (U3V 1.2 Table 4-5). Offsets relative to
/// the EIRM base.
#[allow(dead_code)]
pub mod eirm {
    pub const EI_CONTROL: u64 = 0x00; // 4, RW: bit 0 = Event Enable
    pub const MAX_EVENT_TRANSFER_LENGTH: u64 = 0x04; // 4, R
    pub const EVENT_TEST_CONTROL: u64 = 0x08; // 4, RW: bit 0 self-clearing
}

/// One entry of the GenCP Manifest Table (GenCP 1.3.1 §5.5, Table 33/34).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ManifestEntry {
    /// Register address the file is read from.
    pub address: u64,
    /// File size in bytes.
    pub size: u64,
    /// 0 = Device XML, 1 = Buffer XML (2-7 reserved).
    pub file_type: u32,
    /// 0 = uncompressed XML, 1 = ZIP containing a single XML.
    pub file_format: u32,
}

pub const MANIFEST_ENTRY_LEN: usize = 64;
pub const FILE_TYPE_DEVICE_XML: u32 = 0;
pub const FILE_FORMAT_RAW_XML: u32 = 0;
pub const FILE_FORMAT_ZIP_XML: u32 = 1;

/// Parse one 64-byte manifest entry. Returns None on a short buffer.
pub fn decode_manifest_entry(buf: &[u8]) -> Option<ManifestEntry> {
    if buf.len() < MANIFEST_ENTRY_LEN {
        return None;
    }
    let schema_format = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
    Some(ManifestEntry {
        address: u64::from_le_bytes(buf[8..16].try_into().ok()?),
        size: u64::from_le_bytes(buf[16..24].try_into().ok()?),
        file_type: schema_format & 0x7,
        file_format: (schema_format >> 10) & 0x3F,
    })
}

// ---------------------------------------------------------------------------
// Streaming data block (U3V 1.2 §5.5)
// ---------------------------------------------------------------------------

/// Leader magic: ASCII "U3VL" with `U` in the LSB (§5.5.2 Table 5-4).
pub const LEADER_MAGIC: u32 = 0x4C56_3355;
/// Trailer magic: ASCII "U3VT" with `U` in the LSB (§5.5.3 Table 5-5).
pub const TRAILER_MAGIC: u32 = 0x5456_3355;

/// Payload types (§5.5.5 Table 5-6).
pub const PAYLOAD_TYPE_IMAGE: u16 = 0x0001;
/// Bit 14 of payload_type: extended chunk mode (§5.5.5.2).
#[allow(dead_code)] // extended-chunk payloads are rejected, not decoded
pub const PAYLOAD_TYPE_EXTENDED_CHUNK_BIT: u16 = 0x4000;

pub const IMAGE_LEADER_LEN: usize = 52;
pub const IMAGE_TRAILER_LEN: usize = 32;

/// Decoded image leader (§5.5.5.1.1 Table 5-7, all little-endian).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageLeader {
    pub leader_size: u16,
    pub block_id: u64,
    pub payload_type: u16,
    pub timestamp: u64,
    pub pixel_format: u32,
    pub size_x: u32,
    pub size_y: u32,
    pub offset_x: u32,
    pub offset_y: u32,
    pub padding_x: u16,
}

/// Why a leader buffer is being rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaderError {
    TooShort,
    BadMagic,
    BadSize,
    UnsupportedPayloadType(u16),
}

/// Decode and validate an image leader. `payload_type` must be plain
/// Image (0x0001): this producer streams image payloads only; the caller
/// classifies leaders before reserving buffers.
pub fn decode_image_leader(buf: &[u8]) -> Result<ImageLeader, LeaderError> {
    if buf.len() < IMAGE_LEADER_LEN {
        return Err(LeaderError::TooShort);
    }
    let magic = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
    if magic != LEADER_MAGIC {
        return Err(LeaderError::BadMagic);
    }
    let leader_size = u16::from_le_bytes([buf[6], buf[7]]);
    // The transfer may legally be larger (host submits the programmed
    // maximum); the leader must at least contain the fixed image fields.
    if leader_size < IMAGE_LEADER_LEN as u16 {
        return Err(LeaderError::BadSize);
    }
    let payload_type = u16::from_le_bytes([buf[18], buf[19]]);
    if payload_type != PAYLOAD_TYPE_IMAGE {
        return Err(LeaderError::UnsupportedPayloadType(payload_type));
    }
    Ok(ImageLeader {
        leader_size,
        block_id: u64::from_le_bytes(buf[8..16].try_into().expect("len checked")),
        payload_type,
        timestamp: u64::from_le_bytes(buf[20..28].try_into().expect("len checked")),
        pixel_format: u32::from_le_bytes([buf[28], buf[29], buf[30], buf[31]]),
        size_x: u32::from_le_bytes([buf[32], buf[33], buf[34], buf[35]]),
        size_y: u32::from_le_bytes([buf[36], buf[37], buf[38], buf[39]]),
        offset_x: u32::from_le_bytes([buf[40], buf[41], buf[42], buf[43]]),
        offset_y: u32::from_le_bytes([buf[44], buf[45], buf[46], buf[47]]),
        padding_x: u16::from_le_bytes([buf[48], buf[49]]),
    })
}

/// Decoded image trailer (§5.5.5.1.3 Table 5-8, all little-endian).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageTrailer {
    pub trailer_size: u16,
    pub block_id: u64,
    pub status: u16,
    pub valid_payload_size: u64,
    pub size_y: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrailerError {
    TooShort,
    BadMagic,
    BadSize,
}

pub fn decode_image_trailer(buf: &[u8]) -> Result<ImageTrailer, TrailerError> {
    if buf.len() < IMAGE_TRAILER_LEN {
        return Err(TrailerError::TooShort);
    }
    let magic = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
    if magic != TRAILER_MAGIC {
        return Err(TrailerError::BadMagic);
    }
    let trailer_size = u16::from_le_bytes([buf[6], buf[7]]);
    if trailer_size < IMAGE_TRAILER_LEN as u16 {
        return Err(TrailerError::BadSize);
    }
    Ok(ImageTrailer {
        trailer_size,
        block_id: u64::from_le_bytes(buf[8..16].try_into().expect("len checked")),
        status: u16::from_le_bytes([buf[16], buf[17]]),
        valid_payload_size: u64::from_le_bytes(buf[20..28].try_into().expect("len checked")),
        size_y: u32::from_le_bytes([buf[28], buf[29], buf[30], buf[31]]),
    })
}

/// A decoded device event (§4.1.3.3.2): prefix U3VE + CCD(EVENT_CMD) +
/// SCD { event_size u16, event_id u16, timestamp u64, data }.
#[allow(dead_code)] // DEI event channel follow-up (UQ7)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventPacket {
    pub event_id: u16,
    pub timestamp: u64,
    pub data: Vec<u8>,
}

/// Parse one event transfer. The SCD starts right after prefix + CCD.
#[allow(dead_code)] // DEI event channel follow-up (UQ7)
pub fn decode_event(buf: &[u8]) -> Option<EventPacket> {
    if buf.len() < CMD_OVERHEAD + 12 {
        return None;
    }
    let prefix = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
    if prefix != PREFIX_EVENT {
        return None;
    }
    let ccd = CommandCcd {
        flags: u16::from_le_bytes([buf[4], buf[5]]),
        command_id: u16::from_le_bytes([buf[6], buf[7]]),
        length: u16::from_le_bytes([buf[8], buf[9]]),
        request_id: u16::from_le_bytes([buf[10], buf[11]]),
    };
    if ccd.command_id != EVENT_CMD {
        return None;
    }
    let scd = &buf[CMD_OVERHEAD..];
    Some(EventPacket {
        event_id: u16::from_le_bytes([scd[2], scd[3]]),
        timestamp: u64::from_le_bytes(scd[4..12].try_into().ok()?),
        data: scd[12..].to_vec(),
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefixes_are_ascii_u3vc_u3ve() {
        // 'U' in the LSB: transmitted little-endian byte order spells the
        // ASCII string (U3V §4.1.3.1).
        assert_eq!(PREFIX_CONTROL.to_le_bytes(), *b"U3VC");
        assert_eq!(PREFIX_EVENT.to_le_bytes(), *b"U3VE");
        assert_eq!(LEADER_MAGIC.to_le_bytes(), *b"U3VL");
        assert_eq!(TRAILER_MAGIC.to_le_bytes(), *b"U3VT");
    }

    #[test]
    fn readmem_encoding_layout() {
        let t = encode_readmem(7, 0x1122_3344_5566_7788, 0xABCD);
        assert_eq!(&t[0..4], b"U3VC");
        assert_eq!(u16::from_le_bytes([t[4], t[5]]), FLAG_REQUEST_ACK);
        assert_eq!(u16::from_le_bytes([t[6], t[7]]), READMEM_CMD);
        assert_eq!(u16::from_le_bytes([t[8], t[9]]), READMEM_SCD_LEN as u16);
        assert_eq!(u16::from_le_bytes([t[10], t[11]]), 7);
        assert_eq!(&t[12..20], &0x1122_3344_5566_7788u64.to_le_bytes());
        assert_eq!(&t[20..22], &[0, 0], "reserved");
        assert_eq!(&t[22..24], &0xABCDu16.to_le_bytes());
        assert_eq!(t.len(), 24);
    }

    #[test]
    fn writemem_encoding_layout() {
        let t = encode_writemem(1, 0xDEAD_BEEF, &[0xAA, 0xBB]);
        assert_eq!(&t[0..4], b"U3VC");
        assert_eq!(u16::from_le_bytes([t[6], t[7]]), WRITEMEM_CMD);
        // CCD length = SCD = address + payload (GenCP §4.4.3).
        assert_eq!(u16::from_le_bytes([t[8], t[9]]), 10);
        assert_eq!(&t[12..20], &0xDEAD_BEEFu64.to_le_bytes());
        assert_eq!(&t[20..], &[0xAA, 0xBB]);
    }

    #[test]
    fn ack_decode_roundtrip() {
        // Build a successful READMEM_ACK for request 42 with 4 data bytes.
        let mut t = Vec::new();
        t.extend_from_slice(b"U3VC");
        t.extend_from_slice(&GENCP_SUCCESS.to_le_bytes());
        t.extend_from_slice(&READMEM_ACK.to_le_bytes());
        t.extend_from_slice(&4u16.to_le_bytes());
        t.extend_from_slice(&42u16.to_le_bytes());
        t.extend_from_slice(&[1, 2, 3, 4]);

        let ccd = decode_ack(PREFIX_CONTROL, &t, READMEM_ACK, 42).expect("valid ack");
        assert_eq!(ccd.status, GENCP_SUCCESS);
        assert_eq!(ccd.length, 4);
        assert_eq!(&t[CMD_OVERHEAD..], &[1, 2, 3, 4]);

        assert_eq!(
            decode_ack(PREFIX_CONTROL, &t, WRITEMEM_ACK, 42),
            Err(AckError::UnexpectedCommand {
                expected: WRITEMEM_ACK,
                got: READMEM_ACK
            })
        );
        assert_eq!(
            decode_ack(PREFIX_CONTROL, &t, READMEM_ACK, 43),
            Err(AckError::RequestIdMismatch {
                expected: 43,
                got: 42
            })
        );
        let mut bad = t.clone();
        bad[0] = b'X';
        assert_eq!(decode_ack(PREFIX_CONTROL, &bad, READMEM_ACK, 42), Err(AckError::BadPrefix));
        assert_eq!(decode_ack(PREFIX_CONTROL, &t[..7], READMEM_ACK, 42), Err(AckError::TooShort));
    }

    #[test]
    fn status_names_cover_table() {
        assert_eq!(status_name(GENCP_SUCCESS), "GENCP_SUCCESS");
        assert_eq!(status_name(GENCP_INVALID_ADDRESS), "GENCP_INVALID_ADDRESS");
        assert_eq!(status_name(GENCP_ERROR), "GENCP_ERROR");
        assert_eq!(status_name(U3V_STATUS_RESEND_NOT_SUPPORTED), "U3V_STATUS_RESEND_NOT_SUPPORTED");
        assert_eq!(status_name(U3V_STATUS_DATA_OVERRUN), "U3V_STATUS_DATA_OVERRUN");
        assert_eq!(status_name(0x1234), "status 0x1234");
    }

    fn sample_leader() -> Vec<u8> {
        let mut b = vec![0u8; IMAGE_LEADER_LEN];
        b[0..4].copy_from_slice(&LEADER_MAGIC.to_le_bytes());
        b[6..8].copy_from_slice(&(IMAGE_LEADER_LEN as u16).to_le_bytes());
        b[8..16].copy_from_slice(&9u64.to_le_bytes());
        b[18..20].copy_from_slice(&PAYLOAD_TYPE_IMAGE.to_le_bytes());
        b[20..28].copy_from_slice(&123_456_789u64.to_le_bytes());
        b[28..32].copy_from_slice(&0x0108_0001u32.to_le_bytes()); // PFNC Mono8
        b[32..36].copy_from_slice(&640u32.to_le_bytes());
        b[36..40].copy_from_slice(&480u32.to_le_bytes());
        b[40..44].copy_from_slice(&10u32.to_le_bytes());
        b[44..48].copy_from_slice(&20u32.to_le_bytes());
        b[48..50].copy_from_slice(&3u16.to_le_bytes());
        b
    }

    #[test]
    fn image_leader_decode_roundtrip() {
        let l = decode_image_leader(&sample_leader()).expect("leader");
        assert_eq!(l.block_id, 9);
        assert_eq!(l.payload_type, PAYLOAD_TYPE_IMAGE);
        assert_eq!(l.timestamp, 123_456_789);
        assert_eq!(l.pixel_format, 0x0108_0001);
        assert_eq!(l.size_x, 640);
        assert_eq!(l.size_y, 480);
        assert_eq!(l.offset_x, 10);
        assert_eq!(l.offset_y, 20);
        assert_eq!(l.padding_x, 3);
    }

    #[test]
    fn image_leader_rejects_garbage() {
        assert_eq!(decode_image_leader(&[]), Err(LeaderError::TooShort));
        let mut bad = sample_leader();
        bad[0] = 0;
        assert_eq!(decode_image_leader(&bad), Err(LeaderError::BadMagic));
        let mut small = sample_leader();
        small[6..8].copy_from_slice(&10u16.to_le_bytes());
        assert_eq!(decode_image_leader(&small), Err(LeaderError::BadSize));
        let mut chunk = sample_leader();
        chunk[18..20].copy_from_slice(&0x4000u16.to_le_bytes());
        assert_eq!(
            decode_image_leader(&chunk),
            Err(LeaderError::UnsupportedPayloadType(0x4000))
        );
        // Larger leader (host submitted more than the fixed size) is fine.
        let mut big = sample_leader();
        big.resize(64, 0);
        big[6..8].copy_from_slice(&64u16.to_le_bytes());
        assert!(decode_image_leader(&big).is_ok());
    }

    #[test]
    fn image_trailer_decode() {
        let mut b = vec![0u8; IMAGE_TRAILER_LEN];
        b[0..4].copy_from_slice(&TRAILER_MAGIC.to_le_bytes());
        b[6..8].copy_from_slice(&(IMAGE_TRAILER_LEN as u16).to_le_bytes());
        b[8..16].copy_from_slice(&9u64.to_le_bytes());
        b[16..18].copy_from_slice(&U3V_STATUS_DATA_OVERRUN.to_le_bytes());
        b[20..28].copy_from_slice(&307_200u64.to_le_bytes());
        b[28..32].copy_from_slice(&470u32.to_le_bytes());
        let t = decode_image_trailer(&b).expect("trailer");
        assert_eq!(t.block_id, 9);
        assert_eq!(t.status, U3V_STATUS_DATA_OVERRUN);
        assert_eq!(t.valid_payload_size, 307_200);
        assert_eq!(t.size_y, 470);
    }

    #[test]
    fn event_decode_layout() {
        let mut b = Vec::new();
        b.extend_from_slice(b"U3VE");
        b.extend_from_slice(&0u16.to_le_bytes()); // flags
        b.extend_from_slice(&EVENT_CMD.to_le_bytes());
        b.extend_from_slice(&16u16.to_le_bytes()); // SCD length
        b.extend_from_slice(&0u16.to_le_bytes()); // request_id (unused)
        b.extend_from_slice(&0u16.to_le_bytes()); // event_size (0: no multi-event)
        b.extend_from_slice(&0x4FFFu16.to_le_bytes()); // U3V_EVENT_TEST
        b.extend_from_slice(&777u64.to_le_bytes()); // timestamp
        b.extend_from_slice(&[0x5A; 4]);
        let ev = decode_event(&b).expect("event");
        assert_eq!(ev.event_id, 0x4FFF);
        assert_eq!(ev.timestamp, 777);
        assert_eq!(ev.data, vec![0x5A; 4]);
    }

    #[test]
    fn manifest_entry_decode() {
        let mut b = vec![0u8; MANIFEST_ENTRY_LEN];
        b[4..8].copy_from_slice(&((1 << 10) | 0u32).to_le_bytes()); // ZIP + Device XML
        b[8..16].copy_from_slice(&0x10_000u64.to_le_bytes());
        b[16..24].copy_from_slice(&4096u64.to_le_bytes());
        let e = decode_manifest_entry(&b).expect("entry");
        assert_eq!(e.file_type, FILE_TYPE_DEVICE_XML);
        assert_eq!(e.file_format, FILE_FORMAT_ZIP_XML);
        assert_eq!(e.address, 0x10_000);
        assert_eq!(e.size, 4096);
    }
}
