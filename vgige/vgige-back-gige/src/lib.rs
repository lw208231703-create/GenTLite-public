//! vgige-back-gige: the [`CameraBack`] / [`FrameSource`] adapter for GigE
//! Vision cameras. Control rides the shared GVCP client (register
//! pass-through, error transparency: the camera's GEV status codes cross
//! this boundary unchanged); the stream is the camera's channel 0 aimed at
//! a local receive socket, reassembled by [`GvspReassembler`] into
//! [`CanonicalFrame`]s — accepting both GVSP header forms (per-datagram EI
//! discrimination, 3.0 §24.2 / 1.2 §24 Fig 24-1), so a 1.2-era camera
//! forwards as 3.0 on a GigE front's up side.

use std::collections::VecDeque;
use std::io;
use std::net::{Ipv4Addr, UdpSocket};
use std::time::{Duration, Instant};

use gentl_core::ds::{BufferState, DsCore, GENTL_INFINITE};
use gentl_core::{GentlError, GentlResult};
use gentl_gige::GvspReassembler;
use gentl_gvcp::registers as reg;
use gentl_gvcp::wire::*;
use gentl_gvcp::GvcpClient;
use vgige_core::{CameraBack, CanonicalFrame, FrameSource};

/// READMEM chunk. 512 bytes keeps every request/acknowledge datagram under
/// the 1500-byte MTU — no IP fragmentation. A larger chunk (the 13 KB XML in
/// one READMEM) fragments across the wire and the LeadingOE camera silently
/// drops it (timeout, 2026-10-22, camera @192.168.1.89; loopback never
/// fragments, which is why the sim e2e missed it). Same value as the
/// producer's client (gentl-gvcp/src/client.rs READMEM_CHUNK).
const READMEM_CHUNK: usize = 512;

/// Socket read timeout inside one `next_frame` window.
const RECV_TIMEOUT_MIN: Duration = Duration::from_millis(1);

/// Reassembly buffer per slot: frames larger than this are dropped
/// (`frames_dropped_too_small` in the close-time stats log). 16 MiB covers
/// roughly a 16 MP Mono8 frame; v1 has no grow path (design note).
const RELAY_FRAME_MAX: usize = 16 * 1024 * 1024;
/// Reassembly pool depth (the relay ring adds its own bound on top).
const POOL_DEPTH: usize = 4;

/// The downstream GigE camera: one GVCP control channel plus the stream
/// configuration knowledge for channel 0.
pub struct GigeBack {
    client: GvcpClient,
    /// Source IPv4 of the down-facing NIC, written to the camera's SCDA0 at
    /// stream open.
    down_local_ip: Ipv4Addr,
}

impl GigeBack {
    /// Connect the control channel and resolve the down-facing source
    /// address (a connected UDP socket exposes it via getsockname).
    pub fn connect(camera: Ipv4Addr, port: u16) -> GentlResult<Self> {
        let client = GvcpClient::connect(camera, port)?;
        let down_local_ip = down_local_ip(camera, port)?;
        Ok(Self {
            client,
            down_local_ip,
        })
    }

    /// Message-carrying XML fetch for startup diagnostics; the
    /// [`CameraBack::fetch_xml`] trait method delegates, logging the detail
    /// and mapping to a GEV status code.
    pub fn fetch_xml_result(&mut self) -> GentlResult<Vec<u8>> {
        let url = self.client.xml_url()?;
        let local = parse_local_url(&url)?;
        let camera_xml = self.read_blob(local.addr, local.len)?;
        // Spec §14.1.1: the URL file extension may be `.zip` (manifest file
        // format 1, GenCP §5.5.1.2). Transparently unzip the first entry so
        // both bare and zipped camera XML are served (real cameras ship
        // both; LeadingOE units serve `*.zip` from First URL).
        let raw = if local.name.ends_with(".zip") || camera_xml.starts_with(b"PK\x03\x04") {
            Self::unzip_single_xml(&camera_xml)?
        } else {
            camera_xml
        };
        Ok(raw)
    }

    /// One ZIP manifest entry contains a single GenICam XML file
    /// (GenCP §5.5.1.2 Table 34): the first `.xml` member wins.
    fn unzip_single_xml(zip_bytes: &[u8]) -> GentlResult<Vec<u8>> {
        use std::io::Read as _;
        let mut archive = zip::ZipArchive::new(std::io::Cursor::new(zip_bytes))
            .map_err(|e| GentlError::Io(format!("camera XML zip unreadable: {e}")))?;
        for i in 0..archive.len() {
            let mut file = archive
                .by_index(i)
                .map_err(|e| GentlError::Io(format!("camera XML zip entry: {e}")))?;
            if file.name().to_ascii_lowercase().ends_with(".xml") {
                let mut raw = Vec::with_capacity(file.size() as usize);
                file.read_to_end(&mut raw)
                    .map_err(|e| GentlError::Io(format!("camera XML unzip: {e}")))?;
                return Ok(raw);
            }
        }
        Err(GentlError::Io("camera XML zip has no .xml entry".into()))
    }

    /// READMEM the blob at `addr..addr+len` in chunks. READMEM carries
    /// 4-byte-aligned ranges only; a 1-3 byte tail that the device cannot
    /// serve (its mapping ends at `len`) is zero-padded — XML clients read
    /// through the aligned window GenTL §6.3.6.5 sanctions, which may cross
    /// the blob end.
    fn read_blob(&mut self, addr: u32, len: u32) -> GentlResult<Vec<u8>> {
        let mut out = Vec::with_capacity(len as usize);
        while (out.len() as u32) < len {
            let remaining = len - out.len() as u32;
            // READMEM ranges are 4-byte multiples; a 1-3 byte tail reads one
            // full word (possibly crossing the device mapping's end).
            let mut n = remaining.min(READMEM_CHUNK as u32) & !3;
            if n == 0 {
                n = 4;
            }
            let mut buf = vec![0u8; n as usize];
            match self.client.read_mem(addr + out.len() as u32, &mut buf) {
                Ok(()) => out.extend_from_slice(&buf),
                // A tail read crossing the device mapping's end: pad and stop.
                Err(GentlError::GevStatus(_)) if remaining < 4 => {
                    out.resize(len as usize, 0);
                    break;
                }
                Err(e) => return Err(e),
            }
        }
        out.truncate(len as usize);
        Ok(out)
    }
}

/// One fixed-string bootstrap slot as a String; the camera's GEV status
/// crosses unchanged (error transparency).
fn read_str(client: &GvcpClient, addr: u32, len: usize) -> Result<String, u16> {
    let mut buf = vec![0u8; len];
    client.read_mem(addr, &mut buf).map_err(gev_status)?;
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    Ok(String::from_utf8_lossy(&buf[..end]).into_owned())
}

/// Map a downstream transaction error to the GEV status the remote client
/// should see (error transparency is the compatibility contract): a GEV
/// status crosses unchanged, anything else (timeout/I/O, the camera being
/// unreachable) becomes the generic Appendix B.21 status — the simulator
/// regmap's precedent.
fn gev_status(e: GentlError) -> u16 {
    match e {
        GentlError::GevStatus(s) => s,
        _ => 0x8FFF,
    }
}

impl CameraBack for GigeBack {
    fn read_reg(&mut self, addr: u32) -> Result<u32, u16> {
        self.client.read_reg(addr).map_err(gev_status)
    }

    fn write_reg(&mut self, addr: u32, value: u32) -> Result<(), u16> {
        self.client.write_reg(addr, value).map_err(gev_status)
    }

    fn read_block(&mut self, addr: u32, len: u32) -> Result<Vec<u8>, u16> {
        // `len` is a multiple of 4 (GVCP READMEM granularity, enforced by
        // the register-space callers); chunk to keep datagrams MTU-safe.
        let mut out = Vec::with_capacity(len as usize);
        while (out.len() as u32) < len {
            let n = (len - out.len() as u32).min(READMEM_CHUNK as u32) as usize;
            let mut buf = vec![0u8; n];
            self.client
                .read_mem(addr + out.len() as u32, &mut buf)
                .map_err(gev_status)?;
            out.extend_from_slice(&buf);
        }
        Ok(out)
    }

    fn write_block(&mut self, _addr: u32, _data: &[u8]) -> Result<(), u16> {
        // gentl-gvcp's client has no WRITEMEM path in v1, and no register-
        // space caller needs one; the seam exists for block-shaped backends
        // (U3V, Step 2). NOT_IMPLEMENTED is the honest §19 answer.
        Err(GEV_STATUS_NOT_IMPLEMENTED)
    }

    fn fetch_xml(&mut self) -> Result<Vec<u8>, u16> {
        self.fetch_xml_result().map_err(|e| {
            eprintln!("vgige: camera XML: {e}");
            gev_status(e)
        })
    }

    fn fetch_identity(&mut self) -> Result<vgige_core::BackIdentity, u16> {
        let version = self.client.read_reg(reg::VERSION).map_err(gev_status)?;
        Ok(vgige_core::BackIdentity {
            manufacturer: read_str(&self.client, reg::MANUFACTURER_NAME, 32)?,
            model: read_str(&self.client, reg::MODEL_NAME, 32)?,
            device_version: read_str(&self.client, reg::DEVICE_VERSION, 32)?,
            serial_number: read_str(&self.client, reg::SERIAL_NUMBER, 16)?,
            user_defined_name: read_str(&self.client, reg::USER_DEFINED_NAME, 16)?,
            spec_version_major: (version >> 16) as u16,
            spec_version_minor: (version & 0xFFFF) as u16,
        })
    }

    fn open_stream(&mut self) -> Result<Box<dyn FrameSource>, u16> {
        let rx = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).map_err(|e| {
            eprintln!("vgige: relay receive socket: {e}");
            0x8FFFu16
        })?;
        rx.set_read_timeout(Some(RECV_TIMEOUT_MIN)).map_err(|_| 0x8FFFu16)?;
        let rx_port = rx.local_addr().map_err(|_| 0x8FFFu16)?.port();

        // Downstream channel config, in §36.57 order: SCDA0 (destination =
        // this box on the camera-facing NIC), then SCP0 last.
        self.client
            .write_reg(reg::scda(0), u32::from(self.down_local_ip))
            .map_err(gev_status)?;
        // SCPS0 is read back, not negotiated: the reassembler infers the
        // payload stride from the wire (R-263st: the block's first payload
        // packet), so no packet-size handshake runs against the real
        // camera. Cameras with a broken §11.7 test-packet firmware
        // (Q35 class) are thus safe; the read also logs the size the
        // camera will actually send.
        match self.client.read_reg(reg::scps(0)) {
            Ok(scps) => eprintln!(
                "vgige: downstream SCPS0 = {} ({} payload bytes; stride measured on the wire)",
                reg::scps_packet_size(scps),
                reg::scps_packet_size(scps).saturating_sub(reg::GVSP_HEADERS_OVERHEAD),
            ),
            Err(e) => eprintln!("vgige: downstream SCPS0 read failed: {e}"),
        }
        self.client
            .write_reg(reg::scp(0), reg::scp_make(rx_port))
            .map_err(gev_status)?;
        // NOTE(AcquisitionStart): the remote client's AcquisitionStart is a
        // vendor-register write (0x1000_xxxx) that passes straight through
        // to the camera — that is the normal arming path. Cameras whose
        // stream auto-runs on channel open (the simulator, Q15) need
        // nothing more. v1 deliberately does not write vendor registers
        // itself: the address is camera-specific and guessing violates the
        // no-guess discipline.

        Ok(Box::new(GigeFrameSource::new(rx)))
    }

    fn close_stream(&mut self) -> Result<(), u16> {
        self.client.write_reg(reg::scp(0), 0).map_err(gev_status)
    }
}

/// Source address the kernel would use towards the camera, discovered
/// without enumerating interfaces.
fn down_local_ip(down: Ipv4Addr, down_port: u16) -> std::io::Result<Ipv4Addr> {
    let s = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
    s.connect((down, down_port))?;
    match s.local_addr()?.ip() {
        std::net::IpAddr::V4(v4) => Ok(v4),
        std::net::IpAddr::V6(_) => Err(std::io::Error::new(
            std::io::ErrorKind::AddrNotAvailable,
            "down-facing route resolved to IPv6",
        )),
    }
}

/// One parsed `local:<name>;<addr hex>;<len hex>` First URL (spec §14.1.1,
/// R-119cd; the sim publishes exactly this form).
struct LocalUrl {
    name: String,
    addr: u32,
    len: u32,
}

/// Parse `local:<name>;<addr hex>;<len hex>` (spec §14.1.1). The scheme is
/// case-insensitive ("Local:" is the spec's spelling); other schemes
/// (http/file) are a clean rejection.
fn parse_local_url(url: &str) -> GentlResult<LocalUrl> {
    let rest = url
        .strip_prefix("local:")
        .or_else(|| url.strip_prefix("Local:"))
        .ok_or_else(|| {
            GentlError::Io(format!(
                "unsupported XML URL scheme `{url}`; linux-vgige only serves local: URLs"
            ))
        })?;
    let mut parts = rest.split(';');
    let name = parts.next().unwrap_or("").to_owned();
    let addr = parts
        .next()
        .and_then(|s| u32::from_str_radix(s, 16).ok())
        .ok_or_else(|| GentlError::Io(format!("malformed local XML URL `{url}` (address)")))?;
    let len = parts
        .next()
        .and_then(|s| u32::from_str_radix(s, 16).ok())
        .ok_or_else(|| GentlError::Io(format!("malformed local XML URL `{url}` (length)")))?;
    Ok(LocalUrl { name, addr, len })
}

/// GVSP intake of one open channel: the camera's datagrams reassembled into
/// [`CanonicalFrame`]s. Owns the reassembly pool ([`DsCore`] + 4 × 16 MiB
/// slots) and the receive socket; the relay driver polls `next_frame`.
struct GigeFrameSource {
    socket: UdpSocket,
    reasm: GvspReassembler,
    core: DsCore,
    /// Frames drained from the pool, not yet handed to the driver.
    pending: VecDeque<CanonicalFrame>,
    buf: [u8; 65536],
    /// Set on transport-level failure: keep answering `None` (the driver
    /// contract requires waiting out the timeout) until dropped.
    dead: bool,
}

impl GigeFrameSource {
    fn new(socket: UdpSocket) -> Self {
        let core = DsCore::new();
        let mut dead = false;
        for _ in 0..POOL_DEPTH {
            match core.announce_owned(RELAY_FRAME_MAX, std::ptr::null_mut()) {
                Ok(id) => {
                    if let Err(e) = core.queue_buffer(id) {
                        eprintln!("vgige: relay buffer requeue: {e}");
                        dead = true;
                    }
                }
                Err(e) => {
                    eprintln!("vgige: relay buffer announce: {e}");
                    dead = true;
                }
            }
        }
        if let Err(e) = core.start_acquisition(GENTL_INFINITE) {
            eprintln!("vgige: relay acquisition start: {e}");
            dead = true;
        }
        Self {
            socket,
            reasm: GvspReassembler::new(),
            core,
            pending: VecDeque::new(),
            buf: [0u8; 65536],
            dead,
        }
    }

    /// Move every filled pool buffer into `pending`, returning each buffer
    /// to the input pool first so the reassembler never starves.
    fn drain_pool(&mut self) {
        for i in 0..self.core.announced_count() {
            let Some(id) = self.core.buffer_id_at(i) else { continue };
            let taken = self.core.with_buffer(id, |b| {
                if b.state != BufferState::Filled {
                    return None;
                }
                let meta = b.meta?;
                // The zero-copy path fills exactly the leader's expected
                // payload (padded image rows, DsCore::on_frame); unknown
                // pixel formats cannot be sized and forward as empty
                // payloads (v1, design note). begin_frame's size gate
                // guarantees the buffer holds `len` bytes.
                let len = meta.payload_bytes().unwrap_or(0);
                let mut payload = vec![0u8; len];
                if len > 0 {
                    // SAFETY: the buffer is Filled and exclusively ours
                    // until mark_delivered; len <= buffer capacity (the
                    // begin_frame size gate), and payload has exactly len
                    // bytes.
                    unsafe {
                        std::ptr::copy_nonoverlapping(
                            b.base_ptr() as *const u8,
                            payload.as_mut_ptr(),
                            len,
                        );
                    }
                }
                Some(CanonicalFrame {
                    timestamp: meta.timestamp,
                    pixel_format: meta.pixel_format,
                    width: meta.width,
                    height: meta.height,
                    offset_x: meta.offset_x,
                    offset_y: meta.offset_y,
                    padding_x: meta.padding_x,
                    padding_y: meta.padding_y,
                    payload,
                })
            });
            // with_buffer wraps the closure's own Option: None buffer ->
            // outer None; a buffer that is not filled/has no meta -> inner
            // None.
            let Some(Some(frame)) = taken else { continue };
            let _ = self.core.mark_delivered(id);
            if let Err(e) = self.core.queue_buffer(id) {
                eprintln!("vgige: relay buffer requeue: {e}");
            }
            self.pending.push_back(frame);
        }
    }
}

impl FrameSource for GigeFrameSource {
    fn next_frame(&mut self, timeout: Duration) -> Option<CanonicalFrame> {
        if let Some(frame) = self.pending.pop_front() {
            return Some(frame);
        }
        if self.dead {
            // The driver contract: wait out the timeout even when no frame
            // can ever arrive, so its stop-flag polling keeps its cadence.
            std::thread::sleep(timeout);
            return None;
        }
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return None;
            }
            let _ = self
                .socket
                .set_read_timeout(Some(remaining.max(RECV_TIMEOUT_MIN)));
            match self.socket.recv_from(&mut self.buf) {
                Ok((n, _)) => self.reasm.on_datagram(&self.core, &self.buf[..n]),
                Err(e)
                    if e.kind() == io::ErrorKind::TimedOut
                        || e.kind() == io::ErrorKind::WouldBlock => {}
                Err(e) => {
                    eprintln!("vgige: relay receive socket failed: {e}");
                    self.dead = true;
                    std::thread::sleep(remaining);
                    return None;
                }
            }
            self.drain_pool();
            if let Some(frame) = self.pending.pop_front() {
                return Some(frame);
            }
        }
    }
}

impl Drop for GigeFrameSource {
    fn drop(&mut self) {
        // Close-time diagnostics (design note §5 item 6: the drop counters
        // are visible in the log at channel close).
        self.reasm.abandon_block(&self.core, false);
        let s = self.reasm.stats();
        eprintln!(
            "vgige: relay rx stats: {} delivered, {} incomplete, {} no-buffer, {} too-small, {} payload bytes",
            s.frames_delivered,
            s.frames_dropped_incomplete,
            s.frames_dropped_no_buffer,
            s.frames_dropped_too_small,
            s.payload_bytes,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_local_url_form() {
        let u = parse_local_url("local:simcam.xml;100000;1c84").unwrap();
        assert_eq!(u.name, "simcam.xml");
        assert_eq!(u.addr, 0x0010_0000);
        assert_eq!(u.len, 0x1c84);
        let u = parse_local_url("Local:cam.xml;A0000;40").unwrap();
        assert_eq!(u.addr, 0xA0000);
        assert!(parse_local_url("http://x/cam.xml").is_err());
        assert!(parse_local_url("local:cam.xml;zz;10").is_err());
        assert!(parse_local_url("local:cam.zip;0;100").is_ok(), "zip detected at fetch");
    }
}
