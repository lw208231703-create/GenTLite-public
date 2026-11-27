//! Opened-device lifecycle: GVCP control channel acquisition (CCP),
//! heartbeat worker, and the `ControlChannel` implementation behind
//! GCReadPort/GCWritePort.

use std::collections::HashSet;
use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gentl_core::channel::ControlChannel;
use gentl_core::{GentlError, GentlResult};
use gentl_gvcp::registers;
use gentl_gvcp::wire::GEV_STATUS_ACCESS_DENIED;
use gentl_gvcp::GvcpClient;

use crate::cti::{DeviceAccess, DeviceEntry, GevDevice, GevInterface, StreamTransport};

/// Max payload per READMEM: the count field is a u16 and must be a
/// multiple of 4; 512 bytes per transaction keeps datagrams small.
const READMEM_CHUNK: usize = 512;

/// Max data bytes per WRITEMEM. Deliberately small (same 512-byte cap as
/// READMEM): camera control planes are known to wedge on IP-fragmented GVCP
/// datagrams (their RX path does not reassemble, Q35), and relying on the
/// u16 length field would allow a ~64 KB datagram. 512 bytes still turns
/// N register writes into one command.
const WRITEMEM_CHUNK: usize = 512;


/// GVCP device-side port; overridable for tests/software cameras.
fn gvcp_device_port() -> u16 {
    std::env::var("GENTLITE_GVCP_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(gentl_gvcp::wire::GVCP_PORT)
}

impl GevDevice {
    /// Connect the control channel and take the requested access level.
    pub fn open(
        iface: &Arc<GevInterface>,
        entry: &DeviceEntry,
        access: DeviceAccess,
    ) -> GentlResult<Arc<GevDevice>> {
        let client = GvcpClient::connect(Ipv4Addr::from(entry.discovery.current_ip), gvcp_device_port())?;

        if let Err(e) = Self::acquire(&client, access) {
            return Err(e);
        }

        // Transport selection (spec §36.27/§36.38): use the fastest
        // transport both sides support. The GVRSP (RoCEv2, Part 4) host
        // backend is not implemented yet, so a GVRSP-only device is
        // rejected here with a precise error instead of failing
        // mysteriously at stream time; a dual-mode device keeps running
        // on GVSP (its factory-default mode). A read failure falls back
        // to assuming GVSP (R-607s guarantees at least one transport;
        // pre-3.0 devices report neither capability bit).
        let capability = client.read_reg(registers::STREAMING_CAPABILITY).unwrap_or(0);
        let gvsp_supported = capability & registers::SCAP_GVSP_MODE_NOT_SUPPORTED == 0;
        let gvrsp_supported = capability & registers::SCAP_GVRSP_MODE_SUPPORTED != 0;
        let transport = match (gvsp_supported, gvrsp_supported) {
            (false, true) => {
                return Err(GentlError::NotImplemented(
                    "device requires GVRSP (RoCEv2) streaming, which this producer does not implement",
                ));
            }
            (false, false) => {
                // Neither transport advertised: violates R-607s; treat as
                // a non-conformant device rather than guessing.
                return Err(GentlError::Io(
                    "device reports no supported streaming protocol (R-607s violation)".into(),
                ));
            }
            (true, _) => StreamTransport::Gvsp,
        };

        // Heartbeat period = timeout/3 (spec §10.5). Read failures fall
        // back to the spec default of 3000 ms.
        let timeout_ms = client.read_reg(registers::HEARTBEAT_TIMEOUT).unwrap_or(3000);
        let period = Duration::from_millis(u64::from(timeout_ms / 3).clamp(50, 60_000));

        let client = Arc::new(Mutex::new(client));
        let stop = Arc::new(AtomicBool::new(false));
        let hb_client = client.clone();
        let hb_stop = stop.clone();
        let thread = std::thread::Builder::new()
            .name(format!("gvcp-heartbeat-{}", entry.id))
            .spawn(move || {
                while !hb_stop.load(Ordering::Relaxed) {
                    if let Ok(c) = hb_client.lock() {
                        c.heartbeat();
                    }
                    std::thread::sleep(period);
                }
            })
            .map_err(|e| GentlError::Io(format!("spawn heartbeat: {e}")))?;

        // GVCP Capability (0x0934): the test_data bit (print bit 6, mask
        // 0x02000000 — 1.2 §27.28 / 3.0 §36.29, MSB-first print numbering,
        // Q31) gates whether test packet payloads carry the §24.5.1 LFSR
        // stream (CR-280st), and the packet_resend bit (print bit 29,
        // mask 0x00000004) gates PACKETRESEND_CMD (§16.7, R-197ca: no
        // requests to a device without it). Best effort: unreadable
        // defaults to "no test data, no resend", both of which the spec
        // tolerates (payload don't care / requests simply never sent).
        let gvcp_capability = client
            .lock()
            .map_err(|_| GentlError::Io("client lock poisoned".into()))?
            .read_reg(registers::GVCP_CAPABILITY)
            .unwrap_or(0);
        let test_data_supported = gvcp_capability & registers::GVCP_CAPABILITY_TEST_DATA != 0;
        let packet_resend_supported =
            gvcp_capability & registers::GVCP_CAPABILITY_PACKET_RESEND != 0;

        Ok(Arc::new(GevDevice {
            id: entry.id.clone(),
            discovery: entry.discovery.clone(),
            access,
            transport,
            test_data_supported,
            packet_resend_supported,
            client,
            payload_reg: Mutex::new(None),
            stop_heartbeat: stop,
            heartbeat_thread: Mutex::new(Some(thread)),
            opened_streams: Mutex::new(HashSet::new()),
            events: crate::events::DeviceEvents::default(),
            parent: Arc::downgrade(iface),
        }))
    }

    fn acquire(client: &GvcpClient, access: DeviceAccess) -> GentlResult<()> {
        let ccp_value = match access {
            DeviceAccess::Monitor => return Ok(()), // no CCP write for read-only
            DeviceAccess::Control => registers::CCP_CONTROL_ACCESS,
            DeviceAccess::Exclusive => registers::CCP_EXCLUSIVE_ACCESS,
        };
        client.write_reg(registers::CCP, ccp_value).map_err(|e| match e {
            GentlError::GevStatus(s) if s == GEV_STATUS_ACCESS_DENIED => GentlError::AccessDenied,
            other => other,
        })
    }

    /// Release the control channel: stop the heartbeat, then write CCP=0
    /// (best effort — a dead device must not block handle cleanup).
    pub fn close(&self) {
        self.stop_heartbeat.store(true, Ordering::Relaxed);
        if let Some(t) = self.heartbeat_thread.lock().ok().and_then(|mut g| g.take()) {
            let _ = t.join();
        }
        // Close the message channel before releasing CCP (devices may refuse
        // message-channel register writes to non-primary applications).
        self.events.close(&self.client);
        if self.access != DeviceAccess::Monitor {
            if let Ok(c) = self.client.lock() {
                let _ = c.write_reg(registers::CCP, 0);
            }
        }
    }

    /// Current `PayloadSize` feature value, resolved through the device XML
    /// and read over GVCP. `None` when the XML has no followable feature or
    /// the register read fails; callers keep their previous value / the
    /// first-leader fallback.
    ///
    /// The feature location (address/length/endianness) is cached after the
    /// first successful XML parse, but the register itself is re-read on
    /// every call: `PayloadSize` is a function of Width/Height/PixelFormat
    /// (GigE Vision 3.0 §37.5.3), so a ROI change before streaming must be
    /// visible to the next `STREAM_INFO_PAYLOAD_SIZE` query.
    pub fn payload_size(&self) -> Option<usize> {
        let reg = self.payload_reg()?;
        let mut bytes = vec![0u8; reg.length];
        self.read_mem(u64::from(reg.address), &mut bytes).ok()?;
        crate::payload::decode_payload_value(&reg, &bytes)
    }

    /// Packet size advertised by the device XML before §11.7 negotiation.
    ///
    /// This is the fallback for devices whose test-packet search fails (a
    /// 1.2 camera may accept the SCPSx writes but never return a usable
    /// test packet). Reading the XML's `GevSCPSPacketSize` register before
    /// the search preserves the factory/default value so the caller can
    /// restore it instead of leaving SCPSx at a probe value.
    pub fn stream_packet_size_hint(&self) -> Option<u32> {
        let url = {
            let client = self.client.lock().ok()?;
            client.xml_url().ok()?
        };
        if !url.starts_with("local:") && !url.starts_with("Local:") {
            return None;
        }
        let xml = crate::payload::fetch_xml(&url, |addr, buf| self.read_mem(u64::from(addr), buf))?;
        let text = String::from_utf8_lossy(&xml);
        let defaults = crate::payload::parse_stream_defaults(&text);
        let reg = defaults.packet_size?;
        let mut bytes = vec![0u8; reg.length];
        self.read_mem(u64::from(reg.address), &mut bytes).ok()?;
        let value = crate::payload::decode_payload_value(&reg, &bytes)?;
        let size = (value as u32) & gentl_gvcp::registers::SCPS_SIZE_MASK;
        (size > gentl_gvcp::gvsp::GVSP_LEGACY_HEADERS_OVERHEAD).then_some(size)
    }

    fn payload_reg(&self) -> Option<crate::payload::PayloadReg> {
        let cached = self.payload_reg.lock().ok()?.clone();
        if let Some(reg) = cached {
            return reg;
        }
        let url = {
            let client = self.client.lock().ok()?;
            client.xml_url().ok()?
        };
        // Unsupported URL schemes are permanent for this device: cache the
        // absent feature instead of re-reading the First URL on every query.
        if !url.starts_with("local:") && !url.starts_with("Local:") {
            if let Ok(mut cache) = self.payload_reg.lock() {
                *cache = Some(None);
            }
            return None;
        }
        // A read failure is not cached: the next query may succeed. An XML
        // that parses but has no usable feature caches `None` (absent).
        let xml = crate::payload::fetch_xml(&url, |addr, buf| self.read_mem(u64::from(addr), buf))?;
        let text = String::from_utf8_lossy(&xml);
        let found = crate::payload::parse_payload_reg(&text);
        if let Ok(mut cache) = self.payload_reg.lock() {
            *cache = Some(found.clone());
        }
        found
    }
}

impl ControlChannel for GevDevice {
    /// Arbitrary address/length read. GVCP READMEM needs 4-byte-aligned
    /// addresses and multiple-of-4 counts, so the aligned window covering
    /// the request is read in chunks and the requested slice returned
    /// (GenTL 1.6 §6.3.6.5 puts this alignment adaptation on the producer).
    ///
    /// Command choice follows the spec only: READMEM (§16.5) is the
    /// general device-memory read and is tried first; a device that does
    /// not serve the address range that way answers
    /// GEV_STATUS_NOT_IMPLEMENTED (§19), which triggers the per-register
    /// READREG (§16.3) fallback. No address-range heuristics: the spec
    /// (1.2 §14.5 / 3.0 §16.5) defines no register-file/memory split, and
    /// observed devices partition differently (docs/00-governance/OPEN-QUESTIONS.md Q24).
    fn read_mem(&self, addr: u64, buf: &mut [u8]) -> GentlResult<()> {
        if buf.is_empty() {
            return Ok(());
        }
        let len = buf.len() as u64;
        if addr.checked_add(len).is_none() || addr + len > u64::from(u32::MAX) + 1 {
            return Err(GentlError::InvalidAddress);
        }
        // Address arithmetic stays in u64: a range ending exactly at 2^32 is
        // legal (the last 32-bit register is 0xFFFF_FFFC) and must not be
        // computed in u32 (review 2026-11-26).
        let start = addr & !3;
        let end = (addr + len + 3) & !3;
        let window_len = usize::try_from(end - start).map_err(|_| GentlError::InvalidAddress)?;
        let mut window = vec![0u8; window_len];

        let client = self.client.lock().map_err(|_| GentlError::Io("client lock poisoned".into()))?;
        let mut off = 0usize;
        while off < window_len {
            let chunk_addr = start + off as u64;
            let chunk_addr =
                u32::try_from(chunk_addr).map_err(|_| GentlError::InvalidAddress)?;
            let n = (window_len - off).min(READMEM_CHUNK);
            let result = client.read_mem(chunk_addr, &mut window[off..off + n]);
            let not_implemented = matches!(result, Err(GentlError::GevStatus(s)) if s == gentl_gvcp::wire::GEV_STATUS_NOT_IMPLEMENTED);
            if not_implemented {
                // Spec-driven fallback (§19 status code): the device
                // serves this range register-by-register instead.
                let mut p = off;
                while p < off + n {
                    let reg = start + p as u64;
                    let reg = u32::try_from(reg).map_err(|_| GentlError::InvalidAddress)?;
                    let v = client.read_reg(reg)?;
                    window[p..p + 4].copy_from_slice(&v.to_be_bytes());
                    p += 4;
                }
            } else {
                result?;
            }
            off += n;
        }
        drop(client);

        let base = usize::try_from(addr - start).map_err(|_| GentlError::InvalidAddress)?;
        buf.copy_from_slice(&window[base..base + buf.len()]);
        Ok(())
    }

    /// Arbitrary address/length write. A lone 4-byte register goes out as
    /// WRITEREG; runs of several whole registers go out as WRITEMEM batches
    /// (§16.6, one command instead of N) and fall back to per-register
    /// WRITEREG on *any* WRITEMEM failure. Partial edges are read-modify-write
    /// (GenTL 1.6 §6.3.6.6 sanctions RMW for alignment adaptation).
    fn write_mem(&self, addr: u64, buf: &[u8]) -> GentlResult<()> {
        if buf.is_empty() {
            return Ok(());
        }
        let len = buf.len() as u64;
        if addr.checked_add(len).is_none() || addr + len > u64::from(u32::MAX) + 1 {
            return Err(GentlError::InvalidAddress);
        }

        let client = self.client.lock().map_err(|_| GentlError::Io("client lock poisoned".into()))?;
        write_aligned_registers(&client, addr, buf)
    }

    fn xml_url(&self) -> GentlResult<String> {
        let client = self.client.lock().map_err(|_| GentlError::Io("client lock poisoned".into()))?;
        client.xml_url()
    }
}

fn write_aligned_registers(client: &GvcpClient, start: u64, buf: &[u8]) -> GentlResult<()> {
    let end = start + buf.len() as u64;
    let base = |reg: u64| (reg - start) as usize;
    let mut pos = start;

    // Head: a partial leading register outside the run.
    if pos % 4 != 0 {
        let reg = pos & !3;
        let n = usize::try_from((4 - (pos % 4)).min(end - pos))
            .map_err(|_| GentlError::InvalidAddress)?;
        rmw_register(client, reg, (pos - reg) as usize, &buf[..n])?;
        pos += n as u64;
    }

    // Whole registers. A single register uses WRITEREG: it is the register
    // command, while WRITEMEM is optional and a device may acknowledge it
    // with a malformed/zero index and silently drop the data (observed on a
    // real GVCP 1.2 LeadingOE camera: status 0, SCD 0x8012_0000, readback
    // unchanged). Multi-register runs keep the WRITEMEM batch and fall back
    // to WRITEREG on any failure, including that malformed-ACK case.
    let whole_end = end & !3;
    while pos < whole_end {
        let run_end = whole_end.min(pos + WRITEMEM_CHUNK as u64).max(pos + 4);
        let from = base(pos);
        let run = &buf[from..base(run_end)];
        let addr = u32::try_from(pos).map_err(|_| GentlError::InvalidAddress)?;
        if run.len() == 4 {
            let value = u32::from_be_bytes(run.try_into().expect("4-byte word"));
            client.write_reg(addr, value)?;
        } else {
            match client.write_mem(addr, run) {
                Ok(()) => {}
                Err(_) => write_registers(client, addr, run)?,
            }
        }
        pos = run_end;
    }

    // Tail: a partial trailing register.
    if pos < end {
        let reg = pos & !3;
        let n = usize::try_from(end - pos).map_err(|_| GentlError::InvalidAddress)?;
        rmw_register(client, reg, (pos - reg) as usize, &buf[base(pos)..])?;
        debug_assert_eq!(n, buf.len() - base(pos));
    }
    Ok(())
}

/// Per-register WRITEREG fallback for a run of whole 32-bit registers.
fn write_registers(client: &GvcpClient, addr: u32, run: &[u8]) -> GentlResult<()> {
    for (i, word) in run.chunks_exact(4).enumerate() {
        let value = u32::from_be_bytes(word.try_into().expect("4-byte word"));
        client.write_reg(addr + (i as u32) * 4, value)?;
    }
    Ok(())
}

/// Read-modify-write `offset..offset+data.len()` of one 32-bit register.
fn rmw_register(client: &GvcpClient, reg: u64, offset: usize, data: &[u8]) -> GentlResult<()> {
    let addr = u32::try_from(reg).map_err(|_| GentlError::InvalidAddress)?;
    let mut value = client.read_reg(addr)?.to_be_bytes();
    let end = offset + data.len();
    if end > 4 {
        return Err(GentlError::InvalidAddress);
    }
    value[offset..end].copy_from_slice(data);
    client.write_reg(addr, u32::from_be_bytes(value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use gentl_gvcp::wire::{GEV_STATUS_ACCESS_DENIED, GEV_STATUS_NOT_IMPLEMENTED};
    use gentl_gvcp::{DeviceHandler, DiscoveryInfo, GvcpServer};

    /// Records register writes, WRITEMEM payloads, and serves one fixed
    /// register value for RMW tests.
    #[derive(Clone, Default)]
    struct Recording {
        writes: Arc<Mutex<Vec<(u32, u32)>>>,
        mem_writes: Arc<Mutex<Vec<(u32, Vec<u8>)>>>,
        read_value: Option<u32>,
        reject_mem: bool,
    }

    impl DeviceHandler for Recording {
        fn discovery_info(&self) -> DiscoveryInfo {
            DiscoveryInfo {
                spec_version_major: 2,
                spec_version_minor: 2,
                device_mode: 0,
                mac: [0; 6],
                ip_config_options: 0,
                ip_config_current: 0,
                current_ip: 0,
                current_subnet_mask: 0,
                default_gateway: 0,
                manufacturer_name: String::new(),
                model_name: String::new(),
                device_version: String::new(),
                manufacturer_info: String::new(),
                serial_number: String::new(),
                user_defined_name: String::new(),
            }
        }

        fn read_reg(&self, _addr: u32) -> Result<u32, u16> {
            self.read_value.ok_or(GEV_STATUS_ACCESS_DENIED)
        }

        fn write_reg(&self, addr: u32, value: u32) -> Result<(), u16> {
            self.writes.lock().unwrap().push((addr, value));
            Ok(())
        }

        fn read_mem(&self, _addr: u32, _count: u16) -> Result<Vec<u8>, u16> {
            Err(GEV_STATUS_ACCESS_DENIED)
        }

        fn write_mem(&self, addr: u32, data: &[u8]) -> Result<(), u16> {
            if self.reject_mem {
                return Err(GEV_STATUS_NOT_IMPLEMENTED);
            }
            self.mem_writes.lock().unwrap().push((addr, data.to_vec()));
            Ok(())
        }
    }

    fn with_client(handler: Recording) -> (GvcpServer, GvcpClient) {
        let server = GvcpServer::start(0, handler).unwrap();
        let client = GvcpClient::connect(Ipv4Addr::LOCALHOST, server.local_port().unwrap()).unwrap();
        (server, client)
    }

    /// Full registers are batched into one WRITEMEM instead of N WRITEREGs.
    #[test]
    fn full_registers_use_one_writemem() {
        let handler = Recording::default();
        let (server, client) = with_client(handler.clone());

        write_aligned_registers(&client, 0x2000, &[1, 2, 3, 4, 5, 6, 7, 8]).unwrap();

        assert_eq!(
            handler.mem_writes.lock().unwrap().as_slice(),
            &[(0x2000, vec![1, 2, 3, 4, 5, 6, 7, 8])]
        );
        assert!(
            handler.writes.lock().unwrap().is_empty(),
            "no per-register fallback expected"
        );
        server.stop();
    }

    /// A lone 4-byte register is WRITEREG, never WRITEMEM: the latter is an
    /// optional memory command and some GVCP 1.2 devices mishandle it.
    #[test]
    fn single_register_uses_writereg_not_writemem() {
        let handler = Recording::default();
        let (server, client) = with_client(handler.clone());

        write_aligned_registers(&client, 0x2000, &[0xDE, 0xAD, 0xBE, 0xEF]).unwrap();

        assert_eq!(
            handler.writes.lock().unwrap().as_slice(),
            &[(0x2000, 0xDEAD_BEEF)]
        );
        assert!(
            handler.mem_writes.lock().unwrap().is_empty(),
            "single register must not use WRITEMEM"
        );
        server.stop();
    }

    /// A device that answers NOT_IMPLEMENTED to WRITEMEM still gets every
    /// register written through WRITEREG (§19 status-driven fallback).
    #[test]
    fn writemem_not_implemented_falls_back_to_writereg() {
        let handler = Recording {
            reject_mem: true,
            ..Recording::default()
        };
        let (server, client) = with_client(handler.clone());

        write_aligned_registers(&client, 0x2000, &[1, 2, 3, 4, 5, 6, 7, 8]).unwrap();

        assert_eq!(
            handler.writes.lock().unwrap().as_slice(),
            &[(0x2000, 0x0102_0304), (0x2004, 0x0506_0708)]
        );
        assert!(handler.mem_writes.lock().unwrap().is_empty());
        server.stop();
    }

    /// Real-machine regression (2026-11-27, LeadingOE LD-V4 GVCP 1.2):
    /// WRITEMEM is acknowledged with status 0 but index 0 and the data is
    /// dropped (ACK SCD 0x8012_0000), so `client.write_mem` errors with an Io
    /// (not a GEV status). Any multi-register write must still fall back to
    /// per-register WRITEREG instead of propagating that error.
    #[test]
    fn writemem_malformed_ack_falls_back_to_writereg() {
        let mut responder = MalformedWritememResponder::start();
        let client = GvcpClient::connect(Ipv4Addr::LOCALHOST, responder.port).unwrap();

        write_aligned_registers(&client, 0x2000, &[1, 2, 3, 4, 5, 6, 7, 8]).unwrap();
        responder.stop();

        assert_eq!(
            responder.writes.lock().unwrap().as_slice(),
            &[(0x2000, 0x0102_0304), (0x2004, 0x0506_0708)],
            "both registers must be written through WRITEREG"
        );
    }

    /// Minimal GVCP responder emulating the real camera's broken WRITEMEM:
    /// WRITEREG works; WRITEMEM answers success with SCD 0x8012_0000 and
    /// performs no write.
    struct MalformedWritememResponder {
        port: u16,
        writes: Arc<Mutex<Vec<(u32, u32)>>>,
        stop: Arc<AtomicBool>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl MalformedWritememResponder {
        fn start() -> Self {
            use std::sync::atomic::Ordering;
            use std::time::Duration;

            let sock = std::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
            sock.set_read_timeout(Some(Duration::from_millis(100))).unwrap();
            let port = sock.local_addr().unwrap().port();
            let writes = Arc::new(Mutex::new(Vec::new()));
            let stop = Arc::new(AtomicBool::new(false));
            let thread_writes = Arc::clone(&writes);
            let thread_stop = Arc::clone(&stop);
            let thread = std::thread::spawn(move || {
                let mut buf = [0u8; 2048];
                while !thread_stop.load(Ordering::Relaxed) {
                    let Ok((n, peer)) = sock.recv_from(&mut buf) else {
                        continue;
                    };
                    if n < 8 {
                        continue;
                    }
                    let cmd = u16::from_be_bytes([buf[2], buf[3]]);
                    let req_id = u16::from_be_bytes([buf[6], buf[7]]);
                    let (status, ack, payload): (u16, u16, Vec<u8>) = match cmd {
                        // WRITEREG: the real device answers correctly.
                        0x0082 if n >= 16 => {
                            let addr = u32::from_be_bytes([buf[8], buf[9], buf[10], buf[11]]);
                            let value = u32::from_be_bytes([buf[12], buf[13], buf[14], buf[15]]);
                            thread_writes.lock().unwrap().push((addr, value));
                            (0, 0x0083, vec![0, 0, 0, 1])
                        }
                        // WRITEMEM: success ACK with the real device's SCD.
                        0x0086 => (0, 0x0087, vec![0x80, 0x12, 0x00, 0x00]),
                        _ => (0x8001, cmd | 0x0001, Vec::new()),
                    };
                    let mut reply = Vec::with_capacity(8 + payload.len());
                    reply.extend_from_slice(&status.to_be_bytes());
                    reply.extend_from_slice(&ack.to_be_bytes());
                    reply.extend_from_slice(&(payload.len() as u16).to_be_bytes());
                    reply.extend_from_slice(&req_id.to_be_bytes());
                    reply.extend_from_slice(&payload);
                    let _ = sock.send_to(&reply, peer);
                }
            });
            Self {
                port,
                writes,
                stop,
                thread: Some(thread),
            }
        }

        fn stop(&mut self) {
            self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    impl Drop for MalformedWritememResponder {
        fn drop(&mut self) {
            self.stop();
        }
    }

    /// Partial edge registers are read-modify-write (GenTL 1.6 §6.3.6.6).
    #[test]
    fn unaligned_edge_is_read_modify_write() {
        let handler = Recording {
            read_value: Some(0xAABB_CCDD),
            ..Recording::default()
        };
        let (server, client) = with_client(handler.clone());

        write_aligned_registers(&client, 0x2002, &[0x11, 0x22]).unwrap();

        assert_eq!(
            handler.writes.lock().unwrap().as_slice(),
            &[(0x2000, 0xAABB_1122)],
            "only the addressed bytes change"
        );
        assert!(handler.mem_writes.lock().unwrap().is_empty());
        server.stop();
    }

    /// A write ending exactly at 2^32 (the last 32-bit register) must not
    /// overflow the address arithmetic: before the fix the release build
    /// wrapped `end` to 0 and silently skipped the write. A lone register is
    /// now WRITEREG rather than WRITEMEM, same as for every single-word write.
    #[test]
    fn write_at_the_top_of_the_address_space_is_not_lost() {
        let handler = Recording::default();
        let (server, client) = with_client(handler.clone());

        write_aligned_registers(&client, u64::from(u32::MAX) - 3, &[9, 8, 7, 6]).unwrap();

        assert_eq!(
            handler.writes.lock().unwrap().as_slice(),
            &[(u32::MAX - 3, 0x0908_0706)]
        );
        assert!(handler.mem_writes.lock().unwrap().is_empty());
        server.stop();
    }
}
