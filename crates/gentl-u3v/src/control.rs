//! GenCP client over the Device Control Interface (U3V 1.2 §4.1) and the
//! bootstrap-register walk (ABRM §4.2.1 / SBRM §4.2.2) that turns an
//! opened USB device into a usable control channel.
//!
//! Transaction discipline (spec-traced, see notes/u3v-wirefacts.md):
//! - request_id starts at 0 after connect (R-35ch) and increments per
//!   command; the acknowledge must echo it (GenCP §4.3.1).
//! - The host never sends more per command than the Maximum Command
//!   Transfer Length (R-41ch) and never requests more than the Maximum
//!   Acknowledge Transfer Length minus envelope (R-43cd); both chunk the
//!   u16 SCD length fields of READMEM/WRITEMEM.
//! - IN transfers are submitted at the full acknowledge size (R-117ch);
//!   devices terminate with a short packet or ZLP (R-118cd).
//! - PENDING_ACK (R-36ch) re-arms the wait instead of completing it.
//! - A command with the CommandResend flag is answered
//!   U3V_STATUS_RESEND_NOT_SUPPORTED (R-134cd), so a timed-out command is
//!   a hard error, not a resend candidate — retries happen only on
//!   USB-level failures, with a fresh request_id.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use gentl_core::channel::ControlChannel;
use gentl_core::{GentlError, GentlResult};

use crate::cti::{DeviceAccess, U3vDevice};
use crate::packets::{
    self, abrm, sbrm, status_name, AckCcd, AckError, ManifestEntry, CMD_OVERHEAD,
    GENCP_ACCESS_DENIED, GENCP_BUSY, GENCP_INVALID_ADDRESS, GENCP_SUCCESS, GENCP_WRITE_PROTECT,
    PENDING_ACK, PREFIX_CONTROL, READMEM_ACK, WRITEMEM_ACK,
};
use crate::usbio::UsbPipe;

/// Default per-command timeout before the Maximum Device Response Time
/// register has been read. The spec caps a device's response time at
/// 300 ms (GenCP §5.4.10), so this default leaves transfer margin.
const DEFAULT_CMD_TIMEOUT: Duration = Duration::from_millis(1000);
/// Upper bound for one acknowledge wait; pending acks may chain.
const MAX_CMD_TIMEOUT: Duration = Duration::from_secs(5);
/// Bootstrap IN transfer size before SBRM's Maximum Acknowledge Transfer
/// Length is known. Every bootstrap register fits (largest is 64 B).
const BOOTSTRAP_IN_SIZE: usize = 1024;
/// Sane cap on device-advertised max transfer lengths (bounds memory).
const MAX_TRANSFER_CAP: u32 = 1 << 20;

/// GenCP request/response engine over one DCI bulk endpoint pair. The
/// USB handle lifetime lives in the InterfaceSet (see cti.rs drop order);
/// the client holds the two endpoint pipes it transacts on.
pub struct GenCpClient {
    out_pipe: Arc<dyn UsbPipe>,
    in_pipe: Arc<dyn UsbPipe>,
    request_id: Mutex<u16>,
    max_command_len: u32,
    max_ack_len: u32,
    mdrt: Duration,
}

impl GenCpClient {
    /// Create a client with bootstrap defaults; `read_bootstrap` refines
    /// the transfer limits from the SBRM.
    pub fn new(out_pipe: Arc<dyn UsbPipe>, in_pipe: Arc<dyn UsbPipe>) -> Self {
        Self {
            out_pipe,
            in_pipe,
            request_id: Mutex::new(0),
            max_command_len: 0x1000,
            max_ack_len: BOOTSTRAP_IN_SIZE as u32,
            mdrt: DEFAULT_CMD_TIMEOUT,
        }
    }

    fn next_request_id(&self) -> u16 {
        let mut g = self.request_id.lock().expect("request_id poisoned");
        let id = *g;
        *g = g.wrapping_add(1);
        id
    }

    /// IN transfer size for one acknowledge (R-117ch): the full maximum
    /// acknowledge size, capped for sanity.
    fn in_size(&self) -> usize {
        self.max_ack_len.clamp(64, MAX_TRANSFER_CAP) as usize
    }

    /// Run one command/acknowledge exchange. Returns the acknowledge
    /// transfer (prefix + CCD + SCD). A non-success device status is mapped
    /// through `status_error` (GenTL producer semantics).
    pub fn transaction(&self, command: &[u8], expect_ack: u16) -> GentlResult<Vec<u8>> {
        self.transaction_raw(command, expect_ack)
            .map_err(|e| match e {
                GentlError::GevStatus(s) => status_error(s),
                other => other,
            })
    }

    /// Like [`transaction`](Self::transaction), but a non-success device
    /// status surfaces as `GentlError::GevStatus(raw_code)`: library
    /// consumers (the `client` facade, and through it the vgige gateway)
    /// keep the exact GenCP/U3V status for error transparency, where
    /// mapping it to a GenTL error code would lose information.
    pub fn transaction_raw(&self, command: &[u8], expect_ack: u16) -> GentlResult<Vec<u8>> {
        let request_id = self.next_request_id();
        self.transaction_with_id(command, expect_ack, request_id)
            .map_err(|e| {
                // A failed exchange may leave a stale acknowledge in the
                // pipe (e.g. we timed out but the device answered later).
                // Flush the IN pipe so the next exchange starts clean;
                // full endpoint-halt re-synchronization (§4.1.4.1.2) is a
                // follow-up (notes/OPEN-QUESTIONS.md UQ6) — on Linux the
                // flush maps to clear_halt (see UsbPipe::flush).
                self.in_pipe.flush();
                e
            })
    }

    fn transaction_with_id(
        &self,
        command: &[u8],
        expect_ack: u16,
        request_id: u16,
    ) -> GentlResult<Vec<u8>> {
        self.out_pipe
            .write_bulk(command, self.mdrt + Duration::from_millis(500))?;

        // One round = submit IN, wait, handle PENDING_ACK by waiting again.
        let mut pending_rounds = 0u32;
        loop {
            let mut buf = vec![0u8; self.in_size()];
            let n = self
                .in_pipe
                .read_bulk(&mut buf, self.mdrt + Duration::from_millis(500))?;
            let transfer = &buf[..n as usize];

            let ccd: AckCcd =
                match packets::decode_ack(PREFIX_CONTROL, transfer, expect_ack, request_id) {
                    Ok(c) => c,
                    Err(AckError::UnexpectedCommand { got, .. }) if got == PENDING_ACK => {
                        pending_rounds += 1;
                        if pending_rounds > 8
                            || self.mdrt.saturating_mul(pending_rounds + 2) > MAX_CMD_TIMEOUT
                        {
                            return Err(GentlError::Timeout);
                        }
                        continue;
                    }
                    Err(AckError::RequestIdMismatch { .. }) => {
                        return Err(GentlError::Io(
                            "acknowledge request_id mismatch (device out of sync)".into(),
                        ));
                    }
                    Err(AckError::UnexpectedCommand { expected, got }) => {
                        return Err(GentlError::Io(format!(
                        "unexpected acknowledge command 0x{got:04X} (expected 0x{expected:04X})"
                    )));
                    }
                    Err(AckError::TooShort) => {
                        return Err(GentlError::Io("short acknowledge transfer".into()));
                    }
                    Err(AckError::BadPrefix) => {
                        return Err(GentlError::Io("bad acknowledge prefix".into()));
                    }
                };

            if ccd.status != GENCP_SUCCESS {
                return Err(GentlError::GevStatus(ccd.status));
            }
            // The SCD length the device claims must fit what arrived.
            let scd_len = ccd.length as usize;
            if transfer.len() < CMD_OVERHEAD + scd_len {
                return Err(GentlError::Io(format!(
                    "acknowledge SCD truncated: {} < {scd_len}",
                    transfer.len().saturating_sub(CMD_OVERHEAD)
                )));
            }
            return Ok(transfer[..CMD_OVERHEAD + scd_len].to_vec());
        }
    }

    /// Bytes readable per READMEM: u16 length field and the maximum
    /// acknowledge transfer envelope (R-43cd).
    fn read_chunk(&self) -> usize {
        let by_ack = self.max_ack_len.saturating_sub(CMD_OVERHEAD as u32);
        (u16::MAX as u32).min(by_ack).clamp(1, u16::MAX as u32) as usize
    }

    /// Bytes writable per WRITEMEM: u16 CCD length field (SCD = 8-byte
    /// address + data) and the maximum command transfer envelope
    /// (R-41ch).
    fn write_chunk(&self) -> usize {
        let by_cmd = self
            .max_command_len
            .saturating_sub((CMD_OVERHEAD + packets::WRITEMEM_SCD_HEADER_LEN) as u32);
        (u16::MAX as u32 - packets::WRITEMEM_SCD_HEADER_LEN as u32)
            .min(by_cmd)
            .clamp(1, u16::MAX as u32) as usize
    }

    /// READMEM chunked to the device's limits. Byte-granular addresses
    /// and lengths are legal for U3V (GenCP §4.4.1: byte aligned unless
    /// the technology states otherwise; U3V states no alignment rule).
    pub fn read_mem(&self, addr: u64, buf: &mut [u8]) -> GentlResult<()> {
        self.read_mem_impl(addr, buf, false)
    }

    /// [`read_mem`](Self::read_mem) with the raw device status preserved
    /// (`GentlError::GevStatus`) — the library facade's error transparency.
    pub fn read_mem_raw(&self, addr: u64, buf: &mut [u8]) -> GentlResult<()> {
        self.read_mem_impl(addr, buf, true)
    }

    fn read_mem_impl(&self, addr: u64, buf: &mut [u8], raw: bool) -> GentlResult<()> {
        let chunk = self.read_chunk();
        let mut done = 0usize;
        while done < buf.len() {
            let n = chunk.min(buf.len() - done).min(u16::MAX as usize);
            let req = packets::encode_readmem(self.peek_request_id(), addr + done as u64, n as u16);
            let ack = if raw {
                self.transaction_raw(&req, READMEM_ACK)?
            } else {
                self.transaction(&req, READMEM_ACK)?
            };
            let data = &ack[CMD_OVERHEAD..];
            if data.len() < n {
                return Err(GentlError::Io(format!(
                    "short READMEM: {} of {n} bytes",
                    data.len()
                )));
            }
            buf[done..done + n].copy_from_slice(&data[..n]);
            done += n;
        }
        Ok(())
    }

    /// WRITEMEM chunked to the device's limits.
    pub fn write_mem(&self, addr: u64, data: &[u8]) -> GentlResult<()> {
        self.write_mem_impl(addr, data, false)
    }

    /// [`write_mem`](Self::write_mem) with the raw device status preserved.
    pub fn write_mem_raw(&self, addr: u64, data: &[u8]) -> GentlResult<()> {
        self.write_mem_impl(addr, data, true)
    }

    fn write_mem_impl(&self, addr: u64, data: &[u8], raw: bool) -> GentlResult<()> {
        let chunk = self.write_chunk();
        let mut done = 0usize;
        while done < data.len() {
            let n = chunk.min(data.len() - done);
            let req = packets::encode_writemem(
                self.peek_request_id(),
                addr + done as u64,
                &data[done..done + n],
            );
            let ack = if raw {
                self.transaction_raw(&req, WRITEMEM_ACK)?
            } else {
                self.transaction(&req, WRITEMEM_ACK)?
            };
            // U3V mandates Written Length Field support (R-111cd area):
            // verify the device accepted the full chunk when the field is
            // present. ack SCD length 0 means the field is absent.
            let scd = &ack[CMD_OVERHEAD..];
            if scd.len() >= packets::WRITEMEM_ACK_SCD_LEN {
                let written = u16::from_le_bytes([scd[2], scd[3]]);
                if written as usize != n {
                    return Err(GentlError::Io(format!(
                        "WRITEMEM accepted {written} of {n} bytes"
                    )));
                }
            }
            done += n;
        }
        Ok(())
    }

    /// The id the next command will use (encoding helpers take it; the
    /// transaction increments).
    fn peek_request_id(&self) -> u16 {
        *self.request_id.lock().expect("request_id poisoned")
    }

    pub fn set_limits(&mut self, max_command: u32, max_ack: u32, mdrt_ms: u32) {
        self.max_command_len = max_command.clamp(64, MAX_TRANSFER_CAP);
        self.max_ack_len = max_ack.clamp(64, MAX_TRANSFER_CAP);
        if mdrt_ms > 0 {
            self.mdrt = Duration::from_millis(u64::from(mdrt_ms).min(3000));
        }
    }

    pub fn read_reg(&self, addr: u64) -> GentlResult<u32> {
        let mut b = [0u8; 4];
        self.read_mem(addr, &mut b)?;
        Ok(u32::from_le_bytes(b))
    }

    pub fn write_reg(&self, addr: u64, v: u32) -> GentlResult<()> {
        self.write_mem(addr, &v.to_le_bytes())
    }
}

/// Map a GenCP/U3V status code to the internal error type.
fn status_error(code: u16) -> GentlError {
    match code {
        GENCP_INVALID_ADDRESS => GentlError::InvalidAddress,
        GENCP_ACCESS_DENIED | GENCP_WRITE_PROTECT => GentlError::AccessDenied,
        GENCP_BUSY => GentlError::Busy,
        other => GentlError::Io(format!(
            "device status {} (0x{other:04X})",
            status_name(other)
        )),
    }
}

/// Everything the bootstrap registers told us about a device.
#[derive(Debug, Clone)]
/// Bootstrap register snapshot (ABRM + SBRM walk). The identity fields are
/// retained in full even where no current consumer reads them: they are the
/// device-facing info set for DevGetInfo and future features.
#[allow(dead_code)]
pub struct Bootstrap {
    pub gen_cp_version: u32,
    pub u3v_version: u32,
    pub manufacturer: String,
    pub model: String,
    pub family: String,
    pub device_version: String,
    pub serial_number: String,
    pub user_defined_name: String,
    pub device_capability: u64,
    pub max_device_response_time_ms: u32,
    pub manifest_table: u64,
    pub number_of_stream_channels: u32,
    pub sirm_address: Option<u64>,
    pub sirm_length: u32,
    pub eirm_address: Option<u64>,
    pub max_command_transfer: u32,
    pub max_ack_transfer: u32,
}

fn read_fixed_string(client: &GenCpClient, addr: u64, len: usize) -> GentlResult<String> {
    let mut buf = vec![0u8; len];
    client.read_mem(addr, &mut buf)?;
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    Ok(String::from_utf8_lossy(&buf[..end]).into_owned())
}

/// ABRM + SBRM walk (§4.2.1 Table 4-6, §4.2.2 Table 4-7).
pub fn read_bootstrap(client: &mut GenCpClient) -> GentlResult<Bootstrap> {
    let gen_cp_version = client.read_reg(abrm::GENCPC_VERSION)?;
    // Device Capability comes FIRST: it tells which optional ABRM fields
    // exist. Reading an unsupported register fails with
    // GENCP_INVALID_ADDRESS — mandatory for producer-side robustness and
    // required by real devices (observed 2026-10-13 on a Hikrobot camera:
    // it implements no FAMILY_NAME and answers 0x8003).
    let device_capability = {
        let mut b = [0u8; 8];
        client.read_mem(abrm::DEVICE_CAPABILITY, &mut b)?;
        u64::from_le_bytes(b)
    };
    let manufacturer = read_fixed_string(client, abrm::MANUFACTURER_NAME, abrm::STRING_LEN)?;
    let model = read_fixed_string(client, abrm::MODEL_NAME, abrm::STRING_LEN)?;
    let family = if device_capability & abrm::CAP_FAMILY_NAME != 0 {
        read_fixed_string(client, abrm::FAMILY_NAME, abrm::STRING_LEN)?
    } else {
        String::new()
    };
    let device_version = read_fixed_string(client, abrm::DEVICE_VERSION, abrm::STRING_LEN)?;
    let serial_number = read_fixed_string(client, abrm::SERIAL_NUMBER, abrm::STRING_LEN)?;
    let user_defined_name = if device_capability & abrm::CAP_USER_DEFINED_NAME != 0 {
        read_fixed_string(client, abrm::USER_DEFINED_NAME, abrm::STRING_LEN)?
    } else {
        String::new()
    };
    let max_device_response_time_ms = client.read_reg(abrm::MAX_DEVICE_RESPONSE_TIME)?;
    let mut manifest_b = [0u8; 8];
    client.read_mem(abrm::MANIFEST_TABLE, &mut manifest_b)?;
    let manifest_table = u64::from_le_bytes(manifest_b);

    // SBRM is conditional on the Device Capability register, but U3V
    // requires it (R-111cd area: "SBRM Supported--Available (Required by
    // this standard)"), so its absence is a device error.
    if device_capability & abrm::CAP_SBRM == 0 {
        return Err(GentlError::Io(format!(
            "device reports no SBRM (required by U3V), capability=0x{device_capability:016x}"
        )));
    }
    let mut sbrm_b = [0u8; 8];
    client.read_mem(abrm::SBRM_ADDRESS, &mut sbrm_b)?;
    let sbrm = u64::from_le_bytes(sbrm_b);

    let u3v_version = client.read_reg(sbrm + sbrm::U3V_VERSION)?;
    let capability = {
        let mut b = [0u8; 8];
        client.read_mem(sbrm + sbrm::U3VCP_CAPABILITY, &mut b)?;
        u64::from_le_bytes(b)
    };
    let max_command_transfer = client.read_reg(sbrm + sbrm::MAX_COMMAND_TRANSFER_LENGTH)?;
    let max_ack_transfer = client.read_reg(sbrm + sbrm::MAX_ACK_TRANSFER_LENGTH)?;
    let number_of_stream_channels = client.read_reg(sbrm + sbrm::NUMBER_OF_STREAM_CHANNELS)?;
    let sirm_address =
        (capability & sbrm::CAP_SIRM != 0 && number_of_stream_channels > 0).then(|| {
            let mut b = [0u8; 8];
            client
                .read_mem(sbrm + sbrm::SIRM_ADDRESS, &mut b)
                .map(|_| u64::from_le_bytes(b))
        });
    let sirm_address = match sirm_address {
        Some(Ok(a)) => Some(a),
        Some(Err(e)) => return Err(e),
        None => None,
    };
    let sirm_length = client.read_reg(sbrm + sbrm::SIRM_LENGTH)?;
    let eirm_address = (capability & sbrm::CAP_EIRM != 0).then(|| {
        let mut b = [0u8; 8];
        client
            .read_mem(sbrm + sbrm::EIRM_ADDRESS, &mut b)
            .map(|_| u64::from_le_bytes(b))
    });
    let eirm_address = match eirm_address {
        Some(Ok(a)) => Some(a),
        Some(Err(e)) => return Err(e),
        None => None,
    };

    client.set_limits(
        max_command_transfer,
        max_ack_transfer,
        max_device_response_time_ms,
    );

    Ok(Bootstrap {
        gen_cp_version,
        u3v_version,
        manufacturer,
        model,
        family,
        device_version,
        serial_number,
        user_defined_name,
        device_capability,
        max_device_response_time_ms,
        manifest_table,
        number_of_stream_channels,
        sirm_address,
        sirm_length,
        eirm_address,
        max_command_transfer,
        max_ack_transfer,
    })
}

/// Manifest Table walk (GenCP 1.3.1 §5.5): entry count + 64-byte entries.
pub fn read_manifest(client: &GenCpClient, table_addr: u64) -> GentlResult<Vec<ManifestEntry>> {
    let mut count_b = [0u8; 8];
    client.read_mem(table_addr, &mut count_b)?;
    let count = u64::from_le_bytes(count_b);
    if count > 64 {
        return Err(GentlError::Io(format!(
            "unreasonable manifest entry count {count}"
        )));
    }
    let mut entries = Vec::new();
    for i in 0..count {
        let mut buf = vec![0u8; packets::MANIFEST_ENTRY_LEN];
        client.read_mem(
            table_addr + 8 + i * packets::MANIFEST_ENTRY_LEN as u64,
            &mut buf,
        )?;
        if let Some(e) = packets::decode_manifest_entry(&buf) {
            entries.push(e);
        }
    }
    Ok(entries)
}

/// Download and (if zipped) unpack the first Device XML of the manifest
/// (R-59cd: a U3V device must provide one).
pub fn read_device_xml(client: &GenCpClient, manifest_table: u64) -> GentlResult<Vec<u8>> {
    let entries = read_manifest(client, manifest_table)?;
    let entry = entries
        .iter()
        .find(|e| e.file_type == packets::FILE_TYPE_DEVICE_XML)
        .ok_or_else(|| GentlError::Io("manifest has no Device XML entry".into()))?;
    if entry.size == 0 || entry.size > (32 << 20) {
        return Err(GentlError::Io(format!(
            "manifest XML size {} invalid",
            entry.size
        )));
    }
    let mut raw = vec![0u8; entry.size as usize];
    client.read_mem(entry.address, &mut raw)?;
    if entry.file_format == packets::FILE_FORMAT_RAW_XML {
        return Ok(raw);
    }
    if entry.file_format == packets::FILE_FORMAT_ZIP_XML {
        return unzip_single_xml(&raw);
    }
    Err(GentlError::Io(format!(
        "manifest XML file format {} unsupported",
        entry.file_format
    )))
}

/// A ZIP manifest entry contains a single GenICam XML file
/// (GenCP §5.5.1.2 Table 34).
fn unzip_single_xml(zip_bytes: &[u8]) -> GentlResult<Vec<u8>> {
    use std::io::Read as _;
    let reader = std::io::Cursor::new(zip_bytes);
    let mut archive =
        zip::ZipArchive::new(reader).map_err(|e| GentlError::Io(format!("zip open: {e}")))?;
    for i in 0..archive.len() {
        let mut file = archive
            .by_index(i)
            .map_err(|e| GentlError::Io(format!("zip entry: {e}")))?;
        if file.name().to_ascii_lowercase().ends_with(".xml") {
            let mut out = Vec::with_capacity(file.size() as usize);
            file.read_to_end(&mut out)
                .map_err(|e| GentlError::Io(format!("zip read: {e}")))?;
            return Ok(out);
        }
    }
    Err(GentlError::Io("zip manifest entry has no XML".into()))
}

impl ControlChannel for U3vDevice {
    /// U3V registers are little-endian (R-30cd), overriding the trait's
    /// big-endian convenience defaults (which match GigE, not U3V).
    fn read_reg(&self, addr: u64) -> GentlResult<u32> {
        let mut b = [0u8; 4];
        self.read_mem(addr, &mut b)?;
        Ok(u32::from_le_bytes(b))
    }

    fn write_reg(&self, addr: u64, value: u32) -> GentlResult<()> {
        self.write_mem(addr, &value.to_le_bytes())
    }

    fn read_mem(&self, addr: u64, buf: &mut [u8]) -> GentlResult<()> {
        let c = self
            .client
            .lock()
            .map_err(|_| GentlError::Io("control client lock poisoned".into()))?;
        c.read_mem(addr, buf)
    }

    fn write_mem(&self, addr: u64, buf: &[u8]) -> GentlResult<()> {
        if self.access == DeviceAccess::Monitor {
            return Err(GentlError::AccessDenied);
        }
        let c = self
            .client
            .lock()
            .map_err(|_| GentlError::Io("control client lock poisoned".into()))?;
        c.write_mem(addr, buf)
    }

    /// `Local:<model>.xml;<address>;<size>` of the first manifest entry
    /// (GenApi local-URL convention, see notes/OPEN-QUESTIONS.md UQ4).
    fn xml_url(&self) -> GentlResult<String> {
        let c = self
            .client
            .lock()
            .map_err(|_| GentlError::Io("control client lock poisoned".into()))?;
        let entries = read_manifest(&c, self.bootstrap.manifest_table)?;
        let entry = entries
            .iter()
            .find(|e| e.file_type == packets::FILE_TYPE_DEVICE_XML)
            .ok_or(GentlError::NotAvailable)?;
        let model = sanitize_file_stem(&self.bootstrap.model);
        Ok(format!(
            "Local:{model}.xml;{:X};{:X}",
            entry.address, entry.size
        ))
    }
}

/// Keep the URL's file stem free of characters that would confuse the
/// GenApi URL parser (whitespace, ';').
fn sanitize_file_stem(s: &str) -> String {
    let stem: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if stem.is_empty() {
        "device".into()
    } else {
        stem
    }
}

impl U3vDevice {
    /// Download the device description XML: the manifest's first Device
    /// XML entry, unpacked when stored as ZIP (§4.1.5, GenCP §5.5).
    pub fn device_xml(&self) -> GentlResult<Vec<u8>> {
        let c = self
            .client
            .lock()
            .map_err(|_| GentlError::Io("control client lock poisoned".into()))?;
        read_device_xml(&c, self.bootstrap.manifest_table)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_stem_sanitized() {
        assert_eq!(sanitize_file_stem("My Cam 5"), "My_Cam_5");
        assert_eq!(sanitize_file_stem("a;b"), "a_b");
        assert_eq!(sanitize_file_stem(""), "device");
        assert_eq!(sanitize_file_stem("X_Y-Z9"), "X_Y-Z9");
    }

    #[test]
    fn chunking_respects_u16_and_device_limits() {
        // The chunk math never touches the pipes; scripted mocks suffice.
        let client = GenCpClient::new(
            crate::usbio::mock::pipe(&[], 1024),
            crate::usbio::mock::pipe(&[], 1024),
        );
        // Bootstrap defaults: 4 KiB command / 1 KiB ack.
        assert!(client.read_chunk() <= 1024 - CMD_OVERHEAD);
        assert!(client.write_chunk() <= 4096 - CMD_OVERHEAD - 8);
        let mut client = client;
        client.set_limits(1 << 20, 1 << 20, 300);
        assert_eq!(
            client.read_chunk(),
            u16::MAX as usize,
            "u16 length field caps reads"
        );
        assert_eq!(client.write_chunk(), u16::MAX as usize - 8);
    }

    #[derive(Default)]
    struct WriteIo {
        commands: Mutex<Vec<Vec<u8>>>,
        replies: Mutex<std::collections::VecDeque<Vec<u8>>>,
    }

    impl UsbPipe for WriteIo {
        fn read_bulk(&self, buf: &mut [u8], _timeout: Duration) -> GentlResult<u32> {
            let reply = self.replies.lock().unwrap().pop_front().expect("reply");
            buf[..reply.len()].copy_from_slice(&reply);
            Ok(reply.len() as u32)
        }

        fn write_bulk(&self, buf: &[u8], _timeout: Duration) -> GentlResult<()> {
            let request_id = u16::from_le_bytes([buf[10], buf[11]]);
            let written = buf.len() - CMD_OVERHEAD - packets::WRITEMEM_SCD_HEADER_LEN;
            self.commands.lock().unwrap().push(buf.to_vec());
            self.replies
                .lock()
                .unwrap()
                .push_back(packets::encode_writemem_ack(
                    GENCP_SUCCESS,
                    request_id,
                    Some(written as u16),
                ));
            Ok(())
        }

        fn flush(&self) {}

        fn set_raw_io(&self) -> GentlResult<()> {
            Ok(())
        }

        fn max_packet(&self) -> u16 {
            64
        }
    }

    #[test]
    fn chunked_write_sends_each_data_slice_once() {
        let io = Arc::new(WriteIo::default());
        let mut client = GenCpClient::new(io.clone(), io.clone());
        client.set_limits(64, 64, 300);
        let data: Vec<u8> = (0..100).collect();

        client.write_mem(0x1000, &data).unwrap();

        let commands = io.commands.lock().unwrap();
        assert_eq!(commands.len(), 3);
        let mut done = 0usize;
        for (index, command) in commands.iter().enumerate() {
            let n = command.len() - CMD_OVERHEAD - packets::WRITEMEM_SCD_HEADER_LEN;
            assert_eq!(
                u16::from_le_bytes([command[6], command[7]]),
                packets::WRITEMEM_CMD
            );
            assert_eq!(u16::from_le_bytes([command[10], command[11]]), index as u16);
            assert_eq!(
                u64::from_le_bytes(command[CMD_OVERHEAD..CMD_OVERHEAD + 8].try_into().unwrap()),
                0x1000 + done as u64
            );
            assert_eq!(&command[CMD_OVERHEAD + 8..], &data[done..done + n]);
            done += n;
        }
        assert_eq!(done, data.len());
    }
}
