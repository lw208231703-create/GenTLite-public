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

        Ok(Arc::new(GevDevice {
            id: entry.id.clone(),
            discovery: entry.discovery.clone(),
            access,
            transport,
            client,
            stop_heartbeat: stop,
            heartbeat_thread: Mutex::new(Some(thread)),
            opened_streams: Mutex::new(HashSet::new()),
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
        if self.access != DeviceAccess::Monitor {
            if let Ok(c) = self.client.lock() {
                let _ = c.write_reg(registers::CCP, 0);
            }
        }
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
    /// observed devices partition differently (notes/OPEN-QUESTIONS Q24).
    fn read_mem(&self, addr: u64, buf: &mut [u8]) -> GentlResult<()> {
        if buf.is_empty() {
            return Ok(());
        }
        let len = buf.len() as u64;
        if addr.checked_add(len).is_none() || addr + len > u64::from(u32::MAX) + 1 {
            return Err(GentlError::InvalidAddress);
        }
        let start = (addr as u32) & !3;
        let end = ((addr as u32) + buf.len() as u32 + 3) & !3;
        let window_len = (end - start) as usize;
        let mut window = vec![0u8; window_len];

        let client = self.client.lock().map_err(|_| GentlError::Io("client lock poisoned".into()))?;
        let mut off = 0usize;
        while off < window_len {
            let chunk_addr = start + off as u32;
            let n = (window_len - off).min(READMEM_CHUNK);
            let result = client.read_mem(chunk_addr, &mut window[off..off + n]);
            let not_implemented = matches!(result, Err(GentlError::GevStatus(s)) if s == gentl_gvcp::wire::GEV_STATUS_NOT_IMPLEMENTED);
            if not_implemented {
                // Spec-driven fallback (§19 status code): the device
                // serves this range register-by-register instead.
                let mut p = off;
                while p < off + n {
                    let v = client.read_reg(start + p as u32)?;
                    window[p..p + 4].copy_from_slice(&v.to_be_bytes());
                    p += 4;
                }
            } else {
                result?;
            }
            off += n;
        }
        drop(client);

        let base = (addr as u32 - start) as usize;
        buf.copy_from_slice(&window[base..base + buf.len()]);
        Ok(())
    }

    /// Arbitrary address/length write via WRITEREG. Fully covered
    /// registers are written directly; partial edges are read-modify-write
    /// (GenTL 1.6 §6.3.6.6 sanctions RMW for alignment adaptation).
    fn write_mem(&self, addr: u64, buf: &[u8]) -> GentlResult<()> {
        if buf.is_empty() {
            return Ok(());
        }
        let len = buf.len() as u64;
        if addr.checked_add(len).is_none() || addr + len > u64::from(u32::MAX) + 1 {
            return Err(GentlError::InvalidAddress);
        }
        let start = addr as u32;
        let end = start + buf.len() as u32;

        let client = self.client.lock().map_err(|_| GentlError::Io("client lock poisoned".into()))?;
        let mut reg = start & !3;
        while reg < end {
            let reg_end = reg + 4;
            let from = start.max(reg) - start;
            let to = end.min(reg_end) - start;
            if from == 0 && to == 4 {
                let v = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]);
                client.write_reg(reg, v)?;
            } else {
                let mut v = client.read_reg(reg)?.to_be_bytes();
                v[from as usize..to as usize].copy_from_slice(&buf[from as usize..to as usize]);
                client.write_reg(reg, u32::from_be_bytes(v))?;
            }
            reg = reg_end;
        }
        Ok(())
    }

    fn xml_url(&self) -> GentlResult<String> {
        let client = self.client.lock().map_err(|_| GentlError::Io("client lock poisoned".into()))?;
        client.xml_url()
    }
}
