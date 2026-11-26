//! The device-side register file: ABRM/SBRM/SIRM answer the host's
//! GenCP reads/writes; the manifest table points at the injected XML
//! blob; the custom window (vgige-core slot constants) is served
//! locally; every other address forwards to the downstream camera via
//! [`vgige_core::CameraBack`] 鈥?the gateway resells the real camera's
//! register space, and the served GenICam XML's vendor nodes carry
//! exactly these addresses, so the host's GenApi traffic lands on the
//! camera 1:1.
//!
//! Layout (device QoI): ABRM @0x0000 (0x400), manifest @0x0001_0000,
//! SBRM @0x0001_1000, XML @0x0002_0000, SIRM @0x0003_0000 (0x48),
//! demo acquisition @0x0004_0000, custom window @0x2000_0000 (0x1000).
//! All multi-byte registers are little-endian (R-30cd).

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use gentl_u3v::packets;
use vgige_core::{
    slot, swap_block, DeviceEvent, RegisterEndian, RegisterWidths, SharedBack, CUSTOM_BASE,
    CUSTOM_SLOTS, VGIGE_VERSION,
};

use crate::device::sha1;
use crate::{DeviceConfig, MANIFEST_BASE, SBRM_BASE, SIRM_BASE, XML_BASE};

/// The XML blob size cap (mirrors the host-side sanity bound,
/// control.rs read_device_xml).
const XML_MAX: usize = 32 << 20;

/// Demo acquisition block (SFNC level, see build_regions): Start/Stop
/// pokes at +0x00/+0x04, PayloadSize (u64) at +0x08.
const ACQ_BASE: u64 = 0x0004_0000;

struct Region {
    base: u64,
    mem: Mutex<Vec<u8>>,
    writable: bool,
}

impl Region {
    fn contains(&self, addr: u64, len: u64) -> bool {
        let Some(end) = addr.checked_add(len) else {
            return false;
        };
        addr >= self.base
            && end <= self.base + self.mem.lock().expect("region lock").len() as u64
    }
}

/// R-73st: the RW* registers of one streaming channel, captured when
/// Stream Enable is raised. `SI_CONTROL` is plain RW, not part of it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SirmSnapshot {
    pub max_leader: u32,
    pub transfer_size: u32,
    pub transfer_count: u32,
    pub final1: u32,
    pub final2: u32,
    pub max_trailer: u32,
    pub payload_mode: u32,
}

impl SirmSnapshot {
    /// Transfer geometry consumed by the frame emitter.
    fn geometry(&self) -> (u32, u32, u32, u32) {
        (self.transfer_size, self.transfer_count, self.final1, self.final2)
    }
}

/// Device-side ABRM/SBRM/SIRM register file with the gateway dispatch
/// (local ranges + custom window + camera pass-through).
pub struct RegisterFile {
    regions: Vec<Region>,
    config: DeviceConfig,
    back: SharedBack,
    /// Native byte order of the backend's block data; the U3V device-side
    /// register file is little-endian (R-30cd) and converts a big-endian
    /// GigE backend at this boundary.
    back_endian: RegisterEndian,
    /// Register widths declared by the served XML (block conversion needs
    /// them to tell a 64-bit register from two 32-bit registers; see
    /// [`vgige_core::regwidth`]).
    widths: RegisterWidths,
    sirm_control: Mutex<u32>,
    /// R-73st: every RW* SIRM register captured at Stream Enable and held
    /// until the stream is disabled, so mid-stream writes do not change the
    /// geometry of a running transmission.
    sirm_snapshot: Mutex<Option<SirmSnapshot>>,
    eirm_control: Mutex<u32>,
    event_in: Mutex<Option<Arc<dyn super::DeviceTransfers>>>,
    watcher: Mutex<Option<Box<dyn Fn(bool) + Send>>>,
    general_slots: Mutex<[u32; CUSTOM_SLOTS]>,
    ai_result: AtomicU32,
    ai_score: AtomicU32,
    /// FrameCountRelayed (custom-window slot 3): shared with the
    /// stream session, which increments it per forwarded frame.
    pub frames: Arc<AtomicU32>,
}

impl RegisterFile {
    pub fn new(config: DeviceConfig, back: SharedBack) -> Self {
        let regions = build_regions(&config);
        let widths = RegisterWidths::from_xml(&config.xml);
        let back_endian = back
            .lock()
            .map(|b| b.register_endian())
            .unwrap_or(RegisterEndian::Big);
        Self {
            regions,
            config,
            back,
            back_endian,
            widths,
            sirm_control: Mutex::new(0),
            sirm_snapshot: Mutex::new(None),
            eirm_control: Mutex::new(0),
            event_in: Mutex::new(None),
            watcher: Mutex::new(None),
            general_slots: Mutex::new([0; CUSTOM_SLOTS]),
            ai_result: AtomicU32::new(0),
            ai_score: AtomicU32::new(0),
            frames: Arc::new(AtomicU32::new(0)),
        }
    }

    pub(crate) fn config_ref(&self) -> &DeviceConfig {
        &self.config
    }

    pub fn set_event_in(&self, stream: Arc<dyn super::DeviceTransfers>) {
        *self.event_in.lock().expect("event-in lock") = Some(stream);
    }

    /// Forward one downstream camera event to the host's EIRM event
    /// endpoint (搂4.1.3.3): only while Event Enable (EI Control bit 0) is
    /// set, and bounded by the advertised Maximum Event Transfer Length
    /// (24-byte prefix+CCD+SCD header included). Oversized events are
    /// dropped, never truncated silently. `Ok(false)` = not sent.
    pub fn send_device_event(&self, event: &DeviceEvent) -> Result<bool, ()> {
        if *self.eirm_control.lock().expect("eirm control lock") & 1 == 0 {
            return Ok(false);
        }
        let packet = packets::encode_event(&packets::EventPacket {
            event_id: event.event_id,
            timestamp: event.timestamp_ns,
            data: event.data.clone(),
        });
        if packet.len() > crate::MAX_EVENT_TRANSFER_LEN as usize {
            return Ok(false);
        }
        let stream = self.event_in.lock().expect("event-in lock").clone();
        let Some(stream) = stream else {
            return Ok(false);
        };
        stream.send(&packet).map_err(|_| ())?;
        Ok(true)
    }

    /// Watch Stream Enable transitions (搂5.4.1.2): fired from the
    /// SI_CONTROL write path.
    pub fn set_stream_watcher(&self, watcher: impl Fn(bool) + Send + 'static) {
        *self.watcher.lock().expect("watcher lock") = Some(Box::new(watcher));
    }

    /// Emitter-fatal recovery: the stream transport died mid-block (e.g.
    /// a host-side pipe reset surfaces as EPIPE) 鈥?clear Stream Enable in
    /// the SIRM register and internally, and fire the watcher's close, so
    /// the next host enable is a real 0鈫? transition with the relay and
    /// emitter restarted. Without this a crashed consumer leaves the
    /// virtual camera streaming into the void and every later acquisition
    /// joins mid-frame (observed 2026-10-27).
    pub fn abort_stream(&self) {
        let si_lo = SIRM_BASE + packets::sirm::SI_CONTROL;
        {
            let mut guard = self.sirm_control.lock().expect("sirm control lock");
            *guard &= !packets::sirm::CONTROL_STREAM_ENABLE;
        }
        if let Some(r) = self.region(SIRM_BASE) {
            let mut m = r.mem.lock().expect("region lock");
            let off = (si_lo - r.base) as usize;
            let v = u32::from_le_bytes(m[off..off + 4].try_into().expect("u32"))
                & !packets::sirm::CONTROL_STREAM_ENABLE;
            m[off..off + 4].copy_from_slice(&v.to_le_bytes());
        }
        *self.sirm_snapshot.lock().expect("sirm snapshot lock") = None;
        if let Some(w) = &*self.watcher.lock().expect("watcher lock") {
            w(false);
        }
    }

    pub fn set_ai_result(&self, v: u32) {
        self.ai_result.store(v, Ordering::Relaxed);
    }

    pub fn set_ai_score(&self, v: u32) {
        self.ai_score.store(v, Ordering::Relaxed);
    }

    /// Current Stream Enable bit (the emitter's gate).
    pub fn stream_enabled(&self) -> bool {
        *self.sirm_control.lock().expect("sirm control lock") & packets::sirm::CONTROL_STREAM_ENABLE
            != 0
    }

    /// The host-programmed transfer geometry (搂5.2.1.3): transfer size,
    /// count, final1, final2. All zeros until the host programs.
    pub fn programmed_geometry(&self) -> (u32, u32, u32, u32) {
        let r = self.region(SIRM_BASE).expect("sirm region");
        let m = r.mem.lock().expect("sirm lock");
        let w = |off: u64| {
            let b = (off - SIRM_BASE) as usize;
            u32::from_le_bytes(m[b..b + 4].try_into().expect("u32"))
        };
        (
            w(SIRM_BASE + packets::sirm::SI_PAYLOAD_TRANSFER_SIZE),
            w(SIRM_BASE + packets::sirm::SI_PAYLOAD_TRANSFER_COUNT),
            w(SIRM_BASE + packets::sirm::SI_PAYLOAD_FINAL1_SIZE),
            w(SIRM_BASE + packets::sirm::SI_PAYLOAD_FINAL2_SIZE),
        )
    }

    /// Geometry of the transmission currently running: the RW* values
    /// captured when Stream Enable was set (R-73st). Falls back to the live
    /// registers when no stream is running.
    pub fn stream_geometry(&self) -> (u32, u32, u32, u32) {
        if let Some(snapshot) = *self.sirm_snapshot.lock().expect("sirm snapshot lock") {
            return snapshot.geometry();
        }
        self.programmed_geometry()
    }

    /// The complete RW* snapshot of the running stream, if any.
    pub fn stream_snapshot(&self) -> Option<SirmSnapshot> {
        *self.sirm_snapshot.lock().expect("sirm snapshot lock")
    }

    /// Read one SIRM register word; `off` is relative to the SIRM base.
    fn sirm_word(&self, off: u64) -> u32 {
        let r = self.region(SIRM_BASE).expect("sirm region");
        let m = r.mem.lock().expect("region lock");
        let b = off as usize;
        u32::from_le_bytes(m[b..b + 4].try_into().expect("u32"))
    }

    /// Capture every RW* register (R-73st/Table 5-1).
    fn read_sirm_rw_star(&self) -> SirmSnapshot {
        use packets::sirm::*;
        SirmSnapshot {
            max_leader: self.sirm_word(SI_MAX_LEADER_SIZE),
            transfer_size: self.sirm_word(SI_PAYLOAD_TRANSFER_SIZE),
            transfer_count: self.sirm_word(SI_PAYLOAD_TRANSFER_COUNT),
            final1: self.sirm_word(SI_PAYLOAD_FINAL1_SIZE),
            final2: self.sirm_word(SI_PAYLOAD_FINAL2_SIZE),
            max_trailer: self.sirm_word(SI_MAX_TRAILER_SIZE),
            payload_mode: self.sirm_word(SI_PAYLOAD_MODE),
        }
    }

    /// 5.4.1.2: a Stream Enable the device cannot honour is rejected with
    /// U3V_STATUS_SI_REGISTERS_INCONSISTENT (or the alignment status from
    /// R-121cd), and the enable bit must not latch.
    fn validate_sirm_enable(&self, snap: &SirmSnapshot) -> Result<(), u16> {
        use packets::sirm::*;
        let alignment = 1u64 << (self.sirm_word(SI_INFO) >> 24);
        for value in [
            snap.max_leader,
            snap.max_trailer,
            snap.transfer_size,
            snap.final1,
            snap.final2,
        ] {
            if u64::from(value) % alignment != 0 {
                return Err(packets::U3V_STATUS_SI_PAYLOAD_SIZE_NOT_ALIGNED);
            }
        }
        // Additional payload modes are not implemented: only Standard
        // Transfer (0 or the MODE_STANDARD bit) is acceptable.
        if snap.payload_mode & !MODE_STANDARD != 0 {
            return Err(packets::U3V_STATUS_SI_REGISTERS_INCONSISTENT);
        }
        if snap.max_leader < self.sirm_word(SI_REQUIRED_LEADER_SIZE)
            || snap.max_trailer < self.sirm_word(SI_REQUIRED_TRAILER_SIZE)
        {
            return Err(packets::U3V_STATUS_SI_REGISTERS_INCONSISTENT);
        }
        let required_payload = self
            .back
            .lock()
            .map_err(|_| packets::GENCP_ERROR)?
            .payload_size()
            .unwrap_or(self.config.payload_size);
        let capacity = u64::from(snap.transfer_size) * u64::from(snap.transfer_count)
            + u64::from(snap.final1)
            + u64::from(snap.final2);
        if capacity < required_payload {
            return Err(packets::U3V_STATUS_SI_REGISTERS_INCONSISTENT);
        }
        Ok(())
    }

    /// Maximum acknowledge transfer length advertised in the SBRM.
    pub fn max_ack_transfer_len(&self) -> usize {
        crate::MAX_TRANSFER as usize
    }

    fn region(&self, addr: u64) -> Option<&Region> {
        // Regions do not overlap and are stored in ascending base
        // order: the containing region is the last one with base 鈮?addr.
        self.regions.iter().rev().find(|r| addr >= r.base)
    }

    /// One register/block read; Err is a GenCP status code (搂4.3.2.1).
    pub fn read(&self, addr: u64, buf: &mut [u8]) -> Result<(), u16> {
        let len = buf.len() as u64;
        let end = addr
            .checked_add(len)
            .ok_or(packets::GENCP_INVALID_ADDRESS)?;
        if (addr == ACQ_BASE + 0x08 || addr == SIRM_BASE + packets::sirm::SI_REQUIRED_PAYLOAD_SIZE)
            && (len == 4 || len == 8)
        {
            if let Ok(size) = self.back.lock().map_err(|_| packets::GENCP_ERROR)?.payload_size() {
                let bytes = size.to_le_bytes();
                buf.copy_from_slice(&bytes[..len as usize]);
                return Ok(());
            }
        }
        // Custom window first (it overlaps no device region, but the
        // dispatch is clearest ordered: device 鈫?window 鈫?camera).
        // The local demo acquisition block only owns Start/Stop (0x00/0x04)
        // and PayloadSize (0x08). Other SFNC nodes in the same window
        // (AcquisitionMode, TriggerMode, ...) belong to the real backend.
        if addr >= ACQ_BASE
            && end <= ACQ_BASE + 0x14
            && addr != ACQ_BASE
            && addr != ACQ_BASE + 4
            && !(addr == ACQ_BASE + 0x08 && (len == 4 || len == 8))
        {
            let data = self.back_read(addr, len)?;
            buf.copy_from_slice(&data);
            return Ok(());
        }
        if addr >= u64::from(CUSTOM_BASE)
            && end <= u64::from(CUSTOM_BASE) + (CUSTOM_SLOTS as u64) * 4
        {
            if len % 4 != 0 || addr % 4 != 0 {
                return Err(packets::GENCP_BAD_ALIGNMENT);
            }
            let mut pos = addr;
            for chunk in buf.chunks_exact_mut(4) {
                chunk.copy_from_slice(
                    &self
                        .slot_read(((pos - u64::from(CUSTOM_BASE)) / 4) as usize)
                        .to_le_bytes(),
                );
                pos += 4;
            }
            return Ok(());
        }
        if let Some(r) = self.region(addr).filter(|r| r.contains(addr, len)) {
            {
                let m = r.mem.lock().expect("region lock");
                let off = (addr - r.base) as usize;
                buf.copy_from_slice(&m[off..off + buf.len()]);
            }
            // R-58cd: Current Speed reflects the live link, not a constant.
            let speed_addr = SBRM_BASE + 0x40;
            if addr <= speed_addr && end > speed_addr {
                let off = (speed_addr - addr) as usize;
                buf[off..off + 4].copy_from_slice(&current_speed().to_le_bytes());
            }
            return Ok(());
        }
        self.back_read(addr, len)
            .map(|data| buf.copy_from_slice(&data))
    }

    /// One register/block write; Err is a GenCP status code.
    pub fn write(&self, addr: u64, data: &[u8]) -> Result<(), u16> {
        let len = data.len() as u64;
        let end = addr
            .checked_add(len)
            .ok_or(packets::GENCP_INVALID_ADDRESS)?;
        if (addr == ACQ_BASE || addr == ACQ_BASE + 4) && len == 4 {
            self.back_write(addr, data)?;
        }
        // Forward the rest of the demo window (AcquisitionMode, TriggerMode,
        // ...) to the backend instead of swallowing the write locally.
        if addr >= ACQ_BASE && end <= ACQ_BASE + 0x14 {
            if addr == ACQ_BASE + 0x08 {
                // SFNC PayloadSize is read-only.
                return Err(packets::GENCP_WRITE_PROTECT);
            }
            if addr != ACQ_BASE && addr != ACQ_BASE + 4 {
                return self.back_write(addr, data);
            }
        }
        if addr >= u64::from(CUSTOM_BASE)
            && end <= u64::from(CUSTOM_BASE) + (CUSTOM_SLOTS as u64) * 4
        {
            if len != 4 || addr % 4 != 0 {
                return Err(packets::GENCP_BAD_ALIGNMENT);
            }
            return self.slot_write(
                ((addr - u64::from(CUSTOM_BASE)) / 4) as usize,
                u32::from_le_bytes(data[..4].try_into().expect("u32")),
            );
        }
        if let Some(r) = self.region(addr).filter(|r| r.contains(addr, len)) {
            if !r.writable {
                return Err(packets::GENCP_WRITE_PROTECT);
            }
            if data.is_empty() {
                return Ok(());
            }
            if r.base == SIRM_BASE {
                // R-72cd at byte granularity: GenCP WRITEMEM start/length are
                // byte aligned (GenCP 4.4.1), so a 1..3-byte register write
                // must merge with the existing bytes rather than index past
                // the payload. Any read-only byte rejects the whole write.
                let start = addr - SIRM_BASE;
                for byte in 0..len {
                    if !sirm_byte_writable(start + byte) {
                        return Err(packets::GENCP_WRITE_PROTECT);
                    }
                }
            }
            if r.base == crate::EIRM_BASE {
                // Table 4-5: EI Control (0x00) and Event Test Control (0x08)
                // are writable; Maximum Event Transfer Length (0x04) and
                // every reserved byte are read-only.
                let start = (addr - crate::EIRM_BASE) as usize;
                for byte in 0..len as usize {
                    if !matches!(start + byte, 0..=3 | 8..=11) {
                        return Err(packets::GENCP_WRITE_PROTECT);
                    }
                }
            }
            // 5.4.1.2: validate a 0->1 Stream Enable before the control
            // register is written, so a rejected enable leaves it at 0.
            let si_lo = SIRM_BASE + packets::sirm::SI_CONTROL;
            let si_hi = si_lo + 4;
            let control_word = if r.base == SIRM_BASE && addr < si_hi && end > si_lo {
                // Merge the (possibly partial) write with the current
                // register; a 1..3-byte write must not index past `data`.
                let mut word = [0u8; 4];
                {
                    let m = r.mem.lock().expect("region lock");
                    let off = (si_lo - r.base) as usize;
                    word.copy_from_slice(&m[off..off + 4]);
                }
                let from = addr.max(si_lo);
                let to = (addr + len).min(si_hi);
                for a in from..to {
                    word[(a - si_lo) as usize] = data[(a - addr) as usize];
                }
                let word = u32::from_le_bytes(word);
                if word & !packets::sirm::CONTROL_STREAM_ENABLE != 0 {
                    return Err(packets::GENCP_INVALID_PARAMETER);
                }
                let was = *self.sirm_control.lock().expect("sirm control lock")
                    & packets::sirm::CONTROL_STREAM_ENABLE
                    != 0;
                let wants = word & packets::sirm::CONTROL_STREAM_ENABLE != 0;
                if wants && !was {
                    self.validate_sirm_enable(&self.read_sirm_rw_star())?;
                }
                Some(word)
            } else {
                None
            };
            {
                let mut m = r.mem.lock().expect("region lock");
                let off = (addr - r.base) as usize;
                m[off..off + data.len()].copy_from_slice(data);
            }
            // SI_CONTROL side effect: fire the stream watcher.
            if control_word.is_some() {
                let v = {
                    let m = r.mem.lock().expect("region lock");
                    let off = (si_lo - r.base) as usize;
                    u32::from_le_bytes(m[off..off + 4].try_into().expect("u32"))
                };
                let enabled = v & packets::sirm::CONTROL_STREAM_ENABLE != 0;
                let mut guard = self.sirm_control.lock().expect("sirm control lock");
                let was = *guard & packets::sirm::CONTROL_STREAM_ENABLE != 0;
                *guard = v;
                drop(guard);
                if enabled != was {
                    // R-73st: capture every RW* register on enable and hold
                    // the copy until the stream is disabled.
                    if enabled {
                        *self.sirm_snapshot.lock().expect("sirm snapshot lock") =
                            Some(self.read_sirm_rw_star());
                    } else {
                        *self.sirm_snapshot.lock().expect("sirm snapshot lock") = None;
                    }
                    if let Some(w) = &*self.watcher.lock().expect("watcher lock") {
                        w(enabled);
                    }
                }
            }
            let ei_lo = crate::EIRM_BASE + packets::eirm::EI_CONTROL;
            if addr <= ei_lo && addr + len > ei_lo {
                let v = {
                    let m = r.mem.lock().expect("region lock");
                    let off = (ei_lo - r.base) as usize;
                    u32::from_le_bytes(m[off..off + 4].try_into().expect("u32"))
                };
                *self.eirm_control.lock().expect("eirm control lock") = v;
            }
            let test_lo = crate::EIRM_BASE + packets::eirm::EVENT_TEST_CONTROL;
            if addr <= test_lo && addr + len > test_lo {
                let v = {
                    let m = r.mem.lock().expect("region lock");
                    let off = (test_lo - r.base) as usize;
                    u32::from_le_bytes(m[off..off + 4].try_into().expect("u32"))
                };
                if v != 0 {
                    let enabled = *self.eirm_control.lock().expect("eirm control lock") & 1 != 0;
                    if enabled {
                        let stream = self.event_in.lock().expect("event-in lock").clone();
                        if let Some(stream) = stream {
                            let timestamp = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_nanos() as u64;
                            stream
                                .send(&packets::encode_event(&packets::EventPacket {
                                    event_id: 0x4FFF,
                                    timestamp,
                                    data: Vec::new(),
                                }))
                                .map_err(|_| packets::GENCP_ERROR)?;
                        }
                    }
                    let mut m = r.mem.lock().expect("region lock");
                    let off = (test_lo - r.base) as usize;
                    m[off..off + 4].copy_from_slice(&0u32.to_le_bytes());
                }
            }
            return Ok(());
        }
        self.back_write(addr, data)
    }

    fn slot_read(&self, idx: usize) -> u32 {
        match idx {
            slot::VERSION => VGIGE_VERSION,
            slot::AI_RESULT => self.ai_result.load(Ordering::Relaxed),
            slot::AI_SCORE => self.ai_score.load(Ordering::Relaxed),
            slot::FRAMES_RELAYED => self.frames.load(Ordering::Relaxed),
            _ => self.general_slots.lock().expect("slots lock")[idx],
        }
    }

    fn slot_write(&self, idx: usize, value: u32) -> Result<(), u16> {
        match idx {
            slot::VERSION | slot::AI_RESULT | slot::AI_SCORE | slot::FRAMES_RELAYED => {
                Err(packets::GENCP_WRITE_PROTECT)
            }
            slot::USER_COMMAND => {
                self.general_slots.lock().expect("slots lock")[idx] = value;
                eprintln!("vgige: UserCommand <= 0x{value:08X}");
                Ok(())
            }
            _ => {
                self.general_slots.lock().expect("slots lock")[idx] = value;
                Ok(())
            }
        }
    }

    fn back_read(&self, addr: u64, len: u64) -> Result<Vec<u8>, u16> {
        // The gateway's camera seam is 32-bit (GVCP register space);
        // wider addresses are unmapped here.
        let addr32 = u32::try_from(addr).map_err(|_| packets::GENCP_INVALID_ADDRESS)?;
        let mut back = self.back.lock().map_err(|_| packets::GENCP_ERROR)?;
        let mut data = back.read_block(addr32, len as u32)?;
        if self.back_endian == RegisterEndian::Big {
            // Width-aware: an eight-byte block is two 32-bit registers unless
            // the served XML declares a 64-bit register at that address
            // (`vgige_core::regwidth`).
            swap_block(&mut data, addr32, &self.widths);
        }
        Ok(data)
    }

    fn back_write(&self, addr: u64, data: &[u8]) -> Result<(), u16> {
        let addr32 = u32::try_from(addr).map_err(|_| packets::GENCP_INVALID_ADDRESS)?;
        let mut back = self.back.lock().map_err(|_| packets::GENCP_ERROR)?;
        let mut owned;
        let payload = if self.back_endian == RegisterEndian::Big {
            owned = data.to_vec();
            swap_block(&mut owned, addr32, &self.widths);
            owned.as_slice()
        } else {
            data
        };
        back.write_block(addr32, payload)
    }
}

fn put_str(out: &mut Vec<u8>, s: &str, len: usize) {
    let bytes = s.as_bytes();
    out.extend_from_slice(&bytes[..bytes.len().min(len)]);
    out.resize(out.len() + (len - bytes.len().min(len)), 0);
}

/// Byte-level SIRM write permission: every byte of an RW/RW* register is
/// writable, every other byte is read-only and rejects the whole write
/// (R-72cd). Byte granularity matters because GenCP WRITEMEM lengths are
/// byte aligned (GenCP 4.4.1), so partial register writes are legal.
fn sirm_byte_writable(off: u64) -> bool {
    use packets::sirm::*;
    let within = |base: u64| off >= base && off < base + 4;
    within(SI_CONTROL)
        || within(SI_MAX_LEADER_SIZE)
        || within(SI_PAYLOAD_TRANSFER_SIZE)
        || within(SI_PAYLOAD_TRANSFER_COUNT)
        || within(SI_PAYLOAD_FINAL1_SIZE)
        || within(SI_PAYLOAD_FINAL2_SIZE)
        || within(SI_MAX_TRAILER_SIZE)
        || within(SI_PAYLOAD_MODE)
}

/// [R-58cd] current link speed as the SBRM expects it: the same bit layout
/// as bmSpeedSupport bits 0-6. Read live from the UDC that is bound to the
/// gadget; falls back to SuperSpeed (bit 3) when the information is not
/// available.
fn current_speed() -> u32 {
    #[cfg(target_os = "linux")]
    {
        if let Some(speed) = probe_udc_speed() {
            return speed;
        }
    }
    // R-58cd asks for the current link speed; when it cannot be probed,
    // report "unknown" (no speed bit set) instead of claiming SuperSpeed.
    0
}

#[cfg(target_os = "linux")]
fn probe_udc_speed() -> Option<u32> {
    use std::fs;
    // The gadget's UDC name: the single non-empty UDC file below configfs.
    let mut udc = None;
    for entry in fs::read_dir("/sys/kernel/config/usb_gadget").ok()? {
        let entry = entry.ok()?;
        let path = entry.path().join("UDC");
        if let Ok(name) = fs::read_to_string(&path) {
            let name = name.trim();
            if !name.is_empty() {
                udc = Some(name.to_owned());
                break;
            }
        }
    }
    let udc = udc?;
    let speed = fs::read_to_string(format!("/sys/class/udc/{udc}/current_speed")).ok()?;
    match speed.trim() {
        "low-speed" => Some(1 << 0),
        "full-speed" => Some(1 << 1),
        "high-speed" => Some(1 << 2),
        "super-speed" => Some(1 << 3),
        "super-speed-plus" => Some(1 << 4),
        _ => None,
    }
}

fn build_regions(config: &DeviceConfig) -> Vec<Region> {
    assert!(
        config.xml.len() <= XML_MAX,
        "XML blob exceeds the device cap"
    );
    // ABRM (GenCP 搂5.4 / U3V Table 4-6) at the spec offsets, including
    // the capability-gated slots as zeros (FAMILY_NAME / MANUFACTURER_INFO
    // / USER_DEFINED_NAME bits are unset in Device Capability). Strings
    // are 64-byte slots; the host reads fixed addresses, so the layout
    // must be exact.
    let mut abrm = Vec::new();
    abrm.extend_from_slice(&0x0001_0003u32.to_le_bytes()); // GenCP 1.3
    put_str(&mut abrm, &config.manufacturer, 64); // 0x0004
    put_str(&mut abrm, &config.model, 64); // 0x0044
    put_str(&mut abrm, "", 64); // 0x0084 FAMILY_NAME (unsupported)
    put_str(&mut abrm, &config.device_version, 64); // 0x00C4
    put_str(&mut abrm, "", 64); // 0x0104 MANUFACTURER_INFO (unsupported)
    put_str(&mut abrm, &config.serial_number, 64); // 0x0144
    put_str(&mut abrm, "", 64); // 0x0184 USER_DEFINED_NAME (unsupported)
    abrm.resize(packets::abrm::DEVICE_CAPABILITY as usize, 0);
    abrm.extend_from_slice(
        &(packets::abrm::CAP_SBRM
            | packets::abrm::CAP_ENDIANNESS_REGISTER
            | packets::abrm::CAP_WRITTEN_LENGTH_FIELD)
            .to_le_bytes(),
    );
    abrm.extend_from_slice(&500u32.to_le_bytes()); // max response time, ms
    abrm.extend_from_slice(&MANIFEST_BASE.to_le_bytes());
    abrm.extend_from_slice(&SBRM_BASE.to_le_bytes());
    abrm.resize(0x400, 0);

    // Manifest (GenCP 搂5.5): one entry, raw XML, pointing at the blob.
    // R-50cd: the entry MUST carry a valid SHA1 of the file 鈥?at
    // offset 24, 20 bytes (field layout matches the host-side parser in
    // gentl-u3v packets::decode_manifest_entry, bytes 24..44 were
    // previously zero = deviation, fixed 2026-10-28).
    let mut manifest = Vec::new();
    manifest.extend_from_slice(&1u64.to_le_bytes());
    let mut entry = vec![0u8; packets::MANIFEST_ENTRY_LEN];
    entry[4..8].copy_from_slice(&packets::FILE_FORMAT_RAW_XML.to_le_bytes()); // raw, device XML
    entry[8..16].copy_from_slice(&XML_BASE.to_le_bytes());
    entry[16..24].copy_from_slice(&(config.xml.len() as u64).to_le_bytes());
    entry[24..44].copy_from_slice(&sha1::sha1(&config.xml));
    manifest.extend_from_slice(&entry);

    // SBRM (U3V Table 4-7): one streaming channel, 1 KiB transfers.
    let mut sbrm = vec![0u8; 0x44];
    sbrm[0..4].copy_from_slice(&0x0001_0002u32.to_le_bytes()); // U3V 1.2
    sbrm[0x04..0x0C].copy_from_slice(&((packets::sbrm::CAP_SIRM | packets::sbrm::CAP_EIRM) as u64).to_le_bytes());
    sbrm[0x14..0x18].copy_from_slice(&crate::MAX_TRANSFER.to_le_bytes());
    sbrm[0x18..0x1C].copy_from_slice(&crate::MAX_TRANSFER.to_le_bytes());
    sbrm[0x1C..0x20].copy_from_slice(&1u32.to_le_bytes()); // channels
    sbrm[0x20..0x28].copy_from_slice(&SIRM_BASE.to_le_bytes());
    sbrm[0x28..0x2C].copy_from_slice(&0x48u32.to_le_bytes());
    sbrm[0x2C..0x34].copy_from_slice(&crate::EIRM_BASE.to_le_bytes());
    sbrm[0x34..0x38].copy_from_slice(&0x0Cu32.to_le_bytes());
    sbrm[0x40..0x44].copy_from_slice(&0x08u32.to_le_bytes()); // Current Speed: SuperSpeed

    // SIRM (Table 5-1/5-2): alignment 1 (SI_INFO=0), standard mode;
    // RW transfer sizes zeroed (host programs at Stream Enable).
    let mut sirm = vec![0u8; 0x48];
    sirm[0x08..0x10].copy_from_slice(&config.payload_size.to_le_bytes());
    sirm[0x10..0x14].copy_from_slice(&(packets::IMAGE_LEADER_LEN as u32).to_le_bytes());
    sirm[0x14..0x18].copy_from_slice(&(packets::IMAGE_TRAILER_LEN as u32).to_le_bytes());

    let mut eirm = vec![0u8; 0x0C];
    eirm[0x04..0x08].copy_from_slice(&crate::MAX_EVENT_TRANSFER_LEN.to_le_bytes());

    // Demo acquisition block (SFNC level): AcquisitionStart/Stop pokes
    // plus the frame PayloadSize the host sizes its buffers against.
    // The U3V stream itself is gated by SIRM Stream Enable (搂5.4.1.2);
    // these are served locally so a camera-less (null-back) device still
    // completes a standard GenApi acquisition sequence. Real backends
    // advertise their own acquisition nodes, forwarded 1:1.
    let mut acq = vec![0u8; 0x14];
    acq[0x08..0x10].copy_from_slice(&config.payload_size.to_le_bytes());

    // Regions MUST be listed in ascending base order: the lookup takes
    // the last region with base <= addr.
    vec![
        Region {
            base: crate::ABRM_BASE,
            mem: Mutex::new(abrm),
            writable: false,
        },
        Region {
            base: MANIFEST_BASE,
            mem: Mutex::new(manifest),
            writable: false,
        },
        Region {
            base: SBRM_BASE,
            mem: Mutex::new(sbrm),
            writable: false,
        },
        Region {
            base: XML_BASE,
            mem: Mutex::new(config.xml.clone()),
            writable: false,
        },
        Region {
            base: SIRM_BASE,
            mem: Mutex::new(sirm),
            writable: true,
        },
        Region {
            base: crate::EIRM_BASE,
            mem: Mutex::new(eirm),
            writable: true,
        },
        Region {
            base: ACQ_BASE,
            mem: Mutex::new(acq),
            writable: true,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::synthesize_guid;
    use vgige_core::{BackIdentity, CameraBack};

    struct NullBack;
    impl vgige_core::CameraBack for NullBack {
        fn register_endian(&self) -> vgige_core::RegisterEndian {
            vgige_core::RegisterEndian::Little
        }

        fn read_reg(&mut self, _a: u32) -> Result<u32, u16> {
            Ok(0)
        }
        fn write_reg(&mut self, _a: u32, _v: u32) -> Result<(), u16> {
            Ok(())
        }
        fn read_block(&mut self, _a: u32, len: u32) -> Result<Vec<u8>, u16> {
            Ok(vec![0; len as usize])
        }
        fn write_block(&mut self, _a: u32, _d: &[u8]) -> Result<(), u16> {
            Ok(())
        }
        fn fetch_xml(&mut self) -> Result<Vec<u8>, u16> {
            Ok(vec![])
        }
        fn fetch_identity(&mut self) -> Result<BackIdentity, u16> {
            Ok(BackIdentity {
                manufacturer: "m".into(),
                model: "m".into(),
                device_version: "1".into(),
                serial_number: "s".into(),
                user_defined_name: String::new(),
                spec_version_major: 1,
                spec_version_minor: 2,
            })
        }
        fn open_stream(&mut self) -> Result<Box<dyn vgige_core::FrameSource>, u16> {
            Err(0x8001)
        }
        fn close_stream(&mut self) -> Result<(), u16> {
            Ok(())
        }
    }

    #[test]
    fn abrm_walk_reads() {
        let cfg = DeviceConfig::from_parts(
            BackIdentity {
                manufacturer: "m".into(),
                model: "m".into(),
                device_version: "1".into(),
                serial_number: "s".into(),
                user_defined_name: String::new(),
                spec_version_major: 1,
                spec_version_minor: 2,
            },
            b"<x/>",
            3072,
            synthesize_guid("s"),
        );
        let rf = RegisterFile::new(cfg, std::sync::Arc::new(std::sync::Mutex::new(NullBack)));
        let mut b = [0u8; 8];
        for (addr, len) in [
            (0x0000u64, 4usize),
            (0x01C4, 8),
            (0x0004, 64),
            (0x0044, 64),
            (0x00C4, 64),
            (0x0144, 64),
            (0x01CC, 4),
            (0x01D0, 8),
            (0x01D8, 8),
        ] {
            let mut buf = vec![0u8; len];
            rf.read(addr, &mut buf)
                .unwrap_or_else(|s| panic!("ABRM read 0x{addr:04X}/{len} -> 0x{s:04X}"));
        }
        // SBRM region reads.
        for (off, len) in [
            (0u64, 4usize),
            (0x04, 8),
            (0x14, 4),
            (0x18, 4),
            (0x1C, 4),
            (0x20, 8),
            (0x28, 4),
        ] {
            let mut buf = vec![0u8; len];
            rf.read(crate::SBRM_BASE + off, &mut buf)
                .unwrap_or_else(|s| panic!("SBRM read +0x{off:02X}/{len} -> 0x{s:04X}"));
        }
        let mut speed = [0u8; 4];
        rf.read(crate::SBRM_BASE + 0x40, &mut speed).unwrap();
        let speed = u32::from_le_bytes(speed);
        assert_eq!(speed & 0xFFFF_FF80, 0, "reserved speed bits stay 0");
        assert!(
            speed == 0 || speed.count_ones() == 1,
            "known speed must be one bmSpeedSupport bit, got 0x{speed:02X}"
        );
        let _ = &mut b;
    }

    /// Spec offsets are exact: the serial string sits at ABRM 0x144
    /// (fixed-address reads 鈥?a padded-build mistake must fail HERE,
    /// not get masked by a permissive test back).
    #[test]
    fn abrm_strings_at_spec_offsets() {
        let cfg = DeviceConfig::from_parts(
            BackIdentity {
                manufacturer: "Manu".into(),
                model: "Modl".into(),
                device_version: "9.9.9".into(),
                serial_number: "SER-77".into(),
                user_defined_name: String::new(),
                spec_version_major: 1,
                spec_version_minor: 2,
            },
            b"<x/>",
            3072,
            synthesize_guid("SER-77"),
        );
        let rf = RegisterFile::new(cfg, std::sync::Arc::new(std::sync::Mutex::new(NullBack)));
        let read_str = |addr: u64| {
            let mut buf = vec![0u8; 64];
            rf.read(addr, &mut buf).unwrap();
            let end = buf.iter().position(|&b| b == 0).unwrap_or(64);
            String::from_utf8(buf[..end].to_vec()).unwrap()
        };
        assert_eq!(read_str(0x0004), "Manu");
        assert_eq!(read_str(0x0044), "Modl");
        assert_eq!(read_str(0x00C4), "9.9.9");
        assert_eq!(read_str(0x0144), "SER-77");
    }

    #[test]
    fn payload_size_tracks_backend_roi_for_32_and_64_bit_reads() {
        let back = std::sync::Arc::new(std::sync::Mutex::new(vgige_back_null::NullBack::new()));
        let cfg = DeviceConfig::from_parts(
            BackIdentity {
                manufacturer: "m".into(),
                model: "m".into(),
                device_version: "1".into(),
                serial_number: "s".into(),
                user_defined_name: String::new(),
                spec_version_major: 1,
                spec_version_minor: 2,
            },
            b"<x/>",
            1280 * 1024 * 2,
            synthesize_guid("s"),
        );
        let rf = RegisterFile::new(cfg, back.clone());
        back.lock().unwrap().write_reg(vgige_back_null::REG_WIDTH, 640).unwrap();
        back.lock().unwrap().write_reg(vgige_back_null::REG_HEIGHT, 512).unwrap();

        let mut b32 = [0u8; 4];
        rf.read(ACQ_BASE + 0x08, &mut b32).unwrap();
        assert_eq!(u32::from_le_bytes(b32), 640 * 512 * 2);
        let mut b64 = [0u8; 8];
        rf.read(ACQ_BASE + 0x08, &mut b64).unwrap();
        assert_eq!(u64::from_le_bytes(b64), 640 * 512 * 2);
        rf.read(SIRM_BASE + packets::sirm::SI_REQUIRED_PAYLOAD_SIZE, &mut b64)
            .unwrap();
        assert_eq!(u64::from_le_bytes(b64), 640 * 512 * 2);
        rf.write(ACQ_BASE, &1u32.to_le_bytes()).unwrap();
        assert_eq!(back.lock().unwrap().read_reg(ACQ_BASE as u32).unwrap(), 1);
        rf.write(ACQ_BASE + 4, &0u32.to_le_bytes()).unwrap();
        assert_eq!(back.lock().unwrap().read_reg(ACQ_BASE as u32).unwrap(), 0);
    }

    fn null_cfg() -> DeviceConfig {
        DeviceConfig::from_parts(
            BackIdentity {
                manufacturer: "m".into(),
                model: "m".into(),
                device_version: "1".into(),
                serial_number: "s".into(),
                user_defined_name: String::new(),
                spec_version_major: 1,
                spec_version_minor: 2,
            },
            b"<x/>",
            3072,
            synthesize_guid("s"),
        )
    }

    /// [R-72cd]: read-only SIRM registers reject writes with WRITE_PROTECT;
    /// RW and RW* registers remain writable.
    #[test]
    fn sirm_read_only_registers_reject_writes() {
        let rf = RegisterFile::new(
            null_cfg(),
            std::sync::Arc::new(std::sync::Mutex::new(NullBack)),
        );
        for off in [
            packets::sirm::SI_INFO,
            packets::sirm::SI_REQUIRED_PAYLOAD_SIZE,
            packets::sirm::SI_REQUIRED_LEADER_SIZE,
            packets::sirm::SI_REQUIRED_TRAILER_SIZE,
            packets::sirm::SI_SUPPORTED_PAYLOAD_MODES,
            packets::sirm::SI_GSIRM_ADDRESS,
            packets::sirm::SI_GSIRM_SIZE,
        ] {
            let buf = [0u8; 8];
            assert_eq!(
                rf.write(SIRM_BASE + off, &buf[..if off == packets::sirm::SI_REQUIRED_PAYLOAD_SIZE { 8 } else { 4 }]),
                Err(packets::GENCP_WRITE_PROTECT),
                "offset 0x{off:02X} is read-only"
            );
        }
        rf.write(
            SIRM_BASE + packets::sirm::SI_PAYLOAD_TRANSFER_SIZE,
            &100u32.to_le_bytes(),
        )
        .expect("RW* transfer size is writable");
        // 5.4.1.2: enabling with an unprogrammed/inconsistent set is rejected.
        assert_eq!(
            rf.write(
                SIRM_BASE + packets::sirm::SI_CONTROL,
                &packets::sirm::CONTROL_STREAM_ENABLE.to_le_bytes()
            ),
            Err(packets::U3V_STATUS_SI_REGISTERS_INCONSISTENT)
        );
        program_valid_geometry(&rf);
        rf.write(
            SIRM_BASE + packets::sirm::SI_CONTROL,
            &packets::sirm::CONTROL_STREAM_ENABLE.to_le_bytes(),
        )
        .expect("SI_CONTROL is RW and the programmed geometry is valid");
    }

    /// Program a complete, valid Standard Transfer geometry for `null_cfg`.
    fn program_valid_geometry(rf: &RegisterFile) {
        let put = |off: u64, v: u32| {
            rf.write(SIRM_BASE + off, &v.to_le_bytes()).unwrap();
        };
        put(
            packets::sirm::SI_MAX_LEADER_SIZE,
            packets::IMAGE_LEADER_LEN as u32,
        );
        put(
            packets::sirm::SI_MAX_TRAILER_SIZE,
            packets::IMAGE_TRAILER_LEN as u32,
        );
        put(packets::sirm::SI_PAYLOAD_TRANSFER_SIZE, 2048);
        put(packets::sirm::SI_PAYLOAD_TRANSFER_COUNT, 1);
        put(packets::sirm::SI_PAYLOAD_FINAL1_SIZE, 1024);
    }

    /// [R-73st]: the RW* geometry is captured at Stream Enable and held
    /// until the stream is disabled; mid-stream writes do not affect the
    /// running transmission.
    #[test]
    fn sirm_geometry_is_snapshotted_at_stream_enable() {
        let rf = RegisterFile::new(
            null_cfg(),
            std::sync::Arc::new(std::sync::Mutex::new(NullBack)),
        );
        let put = |off: u64, v: u32| {
            rf.write(SIRM_BASE + off, &v.to_le_bytes()).unwrap();
        };
        put(
            packets::sirm::SI_MAX_LEADER_SIZE,
            packets::IMAGE_LEADER_LEN as u32,
        );
        put(
            packets::sirm::SI_MAX_TRAILER_SIZE,
            packets::IMAGE_TRAILER_LEN as u32,
        );
        put(packets::sirm::SI_PAYLOAD_TRANSFER_SIZE, 2048);
        put(packets::sirm::SI_PAYLOAD_TRANSFER_COUNT, 1);
        put(packets::sirm::SI_PAYLOAD_FINAL1_SIZE, 1024);
        put(packets::sirm::SI_PAYLOAD_FINAL2_SIZE, 40);
        put(packets::sirm::SI_CONTROL, packets::sirm::CONTROL_STREAM_ENABLE);
        assert_eq!(rf.stream_geometry(), (2048, 1, 1024, 40));
        let snap = rf.stream_snapshot().expect("snapshot captured");
        assert_eq!(
            snap,
            SirmSnapshot {
                max_leader: packets::IMAGE_LEADER_LEN as u32,
                transfer_size: 2048,
                transfer_count: 1,
                final1: 1024,
                final2: 40,
                max_trailer: packets::IMAGE_TRAILER_LEN as u32,
                payload_mode: 0,
            },
            "R-73st captures every RW* register"
        );

        // Mid-stream writes update the registers but not the snapshot.
        put(packets::sirm::SI_PAYLOAD_TRANSFER_SIZE, 3000);
        put(packets::sirm::SI_MAX_LEADER_SIZE, 999);
        put(packets::sirm::SI_PAYLOAD_MODE, packets::sirm::MODE_STANDARD);
        assert_eq!(rf.stream_geometry(), (2048, 1, 1024, 40));
        assert_eq!(rf.stream_snapshot().unwrap(), snap);

        // Disabling releases the snapshot: the live values apply again.
        put(packets::sirm::SI_CONTROL, 0);
        assert_eq!(rf.stream_snapshot(), None);
        assert_eq!(rf.stream_geometry(), (3000, 1, 1024, 40));
    }

    /// 5.4.1.2: an all-zero transfer geometry cannot be accepted; the enable
    /// must not latch, the watcher must not fire, and the status is
    /// U3V_STATUS_SI_REGISTERS_INCONSISTENT.
    #[test]
    fn stream_enable_rejects_inconsistent_geometry() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let rf = RegisterFile::new(
            null_cfg(),
            std::sync::Arc::new(std::sync::Mutex::new(NullBack)),
        );
        let enabled = std::sync::Arc::new(AtomicBool::new(false));
        let flag = enabled.clone();
        rf.set_stream_watcher(move |on| flag.store(on, Ordering::SeqCst));

        let enable = || {
            rf.write(
                SIRM_BASE + packets::sirm::SI_CONTROL,
                &packets::sirm::CONTROL_STREAM_ENABLE.to_le_bytes(),
            )
        };
        assert_eq!(enable(), Err(packets::U3V_STATUS_SI_REGISTERS_INCONSISTENT));
        assert!(!rf.stream_enabled(), "the enable bit must not latch");
        assert!(!enabled.load(Ordering::SeqCst), "watcher must not fire");
        let mut control = [0u8; 4];
        rf.read(SIRM_BASE + packets::sirm::SI_CONTROL, &mut control)
            .unwrap();
        assert_eq!(u32::from_le_bytes(control), 0, "SI_CONTROL stays 0");

        // Leader/trailer maximums below the required sizes are rejected.
        let put = |off: u64, v: u32| {
            rf.write(SIRM_BASE + off, &v.to_le_bytes()).unwrap();
        };
        put(packets::sirm::SI_MAX_LEADER_SIZE, 1);
        put(
            packets::sirm::SI_MAX_TRAILER_SIZE,
            packets::IMAGE_TRAILER_LEN as u32,
        );
        put(packets::sirm::SI_PAYLOAD_TRANSFER_SIZE, 4096);
        put(packets::sirm::SI_PAYLOAD_TRANSFER_COUNT, 1);
        assert_eq!(enable(), Err(packets::U3V_STATUS_SI_REGISTERS_INCONSISTENT));

        // A payload capacity below SI Required Payload Size is rejected.
        program_valid_geometry(&rf);
        put(packets::sirm::SI_PAYLOAD_TRANSFER_SIZE, 1024);
        put(packets::sirm::SI_PAYLOAD_FINAL1_SIZE, 0);
        assert_eq!(enable(), Err(packets::U3V_STATUS_SI_REGISTERS_INCONSISTENT));

        // Unsupported payload modes are rejected too.
        program_valid_geometry(&rf);
        put(packets::sirm::SI_PAYLOAD_MODE, 1 << 1);
        assert_eq!(enable(), Err(packets::U3V_STATUS_SI_REGISTERS_INCONSISTENT));

        // The valid set latches and captures the snapshot.
        put(packets::sirm::SI_PAYLOAD_MODE, 0);
        assert_eq!(enable(), Ok(()));
        assert!(rf.stream_enabled());
        assert!(enabled.load(Ordering::SeqCst));
        assert_eq!(rf.stream_geometry(), (2048, 1, 1024, 0));
    }

    /// GenCP WRITEMEM is byte aligned (GenCP 4.4.1): partial SI_CONTROL
    /// writes must merge with the register and never index past the payload.
    #[test]
    fn partial_si_control_writes_merge_without_panicking() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let rf = RegisterFile::new(
            null_cfg(),
            std::sync::Arc::new(std::sync::Mutex::new(NullBack)),
        );
        let enabled = std::sync::Arc::new(AtomicBool::new(false));
        let flag = enabled.clone();
        rf.set_stream_watcher(move |on| flag.store(on, Ordering::SeqCst));
        let si = SIRM_BASE + packets::sirm::SI_CONTROL;
        let write = |bytes: &[u8]| rf.write(si, bytes);
        let read_control = || {
            let mut b = [0u8; 4];
            rf.read(si, &mut b).unwrap();
            u32::from_le_bytes(b)
        };

        // 1-byte enable with an unprogrammed geometry: rejected, no panic,
        // enable bit does not latch.
        assert_eq!(
            write(&[1]),
            Err(packets::U3V_STATUS_SI_REGISTERS_INCONSISTENT)
        );
        assert_eq!(read_control(), 0);
        assert!(!rf.stream_enabled());
        assert!(!enabled.load(Ordering::SeqCst));

        // Partial writes merge with the existing register.
        program_valid_geometry(&rf);
        assert_eq!(write(&[1]), Ok(()), "1-byte enable");
        assert!(rf.stream_enabled());
        assert_eq!(read_control(), 1);
        assert_eq!(write(&[0]), Ok(()), "1-byte disable");
        assert!(!rf.stream_enabled());
        assert_eq!(read_control(), 0);
        assert_eq!(write(&[1, 0]), Ok(()), "2-byte enable");
        assert!(rf.stream_enabled());
        assert_eq!(write(&[0, 0, 0]), Ok(()), "3-byte disable");
        assert!(!rf.stream_enabled());

        // Reserved bits are rejected even in a partial write.
        assert_eq!(
            write(&[0, 1]),
            Err(packets::GENCP_INVALID_PARAMETER),
            "byte 1 is reserved"
        );

        // A write that crosses into the read-only next register is rejected.
        assert_eq!(
            rf.write(si + 2, &[0, 0, 0, 0]),
            Err(packets::GENCP_WRITE_PROTECT)
        );
        // A partial write into a read-only register is rejected too.
        assert_eq!(
            rf.write(SIRM_BASE + packets::sirm::SI_INFO + 1, &[0]),
            Err(packets::GENCP_WRITE_PROTECT)
        );
    }

    /// Partial writes to an RW* geometry register merge byte-wise and become
    /// the value captured by the next Stream Enable.
    #[test]
    fn partial_rw_star_writes_merge_before_stream_enable() {
        let rf = RegisterFile::new(
            null_cfg(),
            std::sync::Arc::new(std::sync::Mutex::new(NullBack)),
        );
        let put = |off: u64, v: u32| {
            rf.write(SIRM_BASE + off, &v.to_le_bytes()).unwrap();
        };
        put(
            packets::sirm::SI_MAX_LEADER_SIZE,
            packets::IMAGE_LEADER_LEN as u32,
        );
        put(
            packets::sirm::SI_MAX_TRAILER_SIZE,
            packets::IMAGE_TRAILER_LEN as u32,
        );
        // 4096 written as two 2-byte pieces of the transfer size register.
        rf.write(
            SIRM_BASE + packets::sirm::SI_PAYLOAD_TRANSFER_SIZE,
            &[0x00, 0x10],
        )
        .unwrap();
        rf.write(
            SIRM_BASE + packets::sirm::SI_PAYLOAD_TRANSFER_SIZE + 2,
            &[0x00, 0x00],
        )
        .unwrap();
        put(packets::sirm::SI_PAYLOAD_TRANSFER_COUNT, 1);
        rf.write(
            SIRM_BASE + packets::sirm::SI_CONTROL,
            &[packets::sirm::CONTROL_STREAM_ENABLE as u8],
        )
        .unwrap();
        assert!(rf.stream_enabled());
        assert_eq!(rf.stream_geometry(), (4096, 1, 0, 0));
        let mut ts = [0u8; 4];
        rf.read(SIRM_BASE + packets::sirm::SI_PAYLOAD_TRANSFER_SIZE, &mut ts)
            .unwrap();
        assert_eq!(u32::from_le_bytes(ts), 4096);
    }

    /// A malformed GenCP address must answer INVALID_ADDRESS instead of
    /// wrapping `addr + len` and panicking on a huge custom-window slot.
    #[test]
    fn high_address_access_does_not_panic() {
        let rf = RegisterFile::new(
            null_cfg(),
            std::sync::Arc::new(std::sync::Mutex::new(NullBack)),
        );
        let bad = u64::MAX - 3;
        let mut buf = [0u8; 4];
        assert_eq!(
            rf.read(bad, &mut buf),
            Err(packets::GENCP_INVALID_ADDRESS),
            "read overflow"
        );
        assert_eq!(
            rf.write(bad, &[0u8; 4]),
            Err(packets::GENCP_INVALID_ADDRESS),
            "write overflow"
        );
        assert_eq!(
            rf.write(u64::from(CUSTOM_BASE) + 5 * 4, &[0u8; 4]),
            Ok(()),
            "the boundary itself stays writable"
        );
    }

    #[test]
    fn acquisition_mode_and_trigger_mode_are_forwarded() {
        let cfg = null_cfg();
        let back: vgige_core::SharedBack = std::sync::Arc::new(
            std::sync::Mutex::new(vgige_back_null::NullBack::new()),
        );
        let rf = RegisterFile::new(cfg, back.clone());
        rf.write(0x0004_000C, &0u32.to_le_bytes()).unwrap();
        assert_eq!(
            back.lock().unwrap().read_reg(0x0004_000C).unwrap(),
            2,
            "AcquisitionMode reaches the backend"
        );
        let mut b = [0u8; 4];
        rf.read(0x0004_000C, &mut b).unwrap();
        assert_eq!(u32::from_le_bytes(b), 2);
    }

    #[test]
    fn eirm_max_event_transfer_is_read_only() {
        let rf = RegisterFile::new(
            null_cfg(),
            std::sync::Arc::new(std::sync::Mutex::new(vgige_back_null::NullBack::new())),
        );
        assert_eq!(
            rf.write(crate::EIRM_BASE + 0x04, &123u32.to_le_bytes()),
            Err(packets::GENCP_WRITE_PROTECT)
        );
    }

    struct EventPipe {
        sent: std::sync::Arc<std::sync::Mutex<Vec<Vec<u8>>>>,
    }

    impl crate::device::DeviceTransfers for EventPipe {
        fn send(&self, bytes: &[u8]) -> Result<(), crate::device::StreamError> {
            self.sent.lock().unwrap().push(bytes.to_vec());
            Ok(())
        }
        fn max_packet(&self) -> u16 {
            512
        }
    }

    /// Downstream events only reach the host while Event Enable is set, are
    /// encoded as 搂4.1.3.3 EVENT_CMD packets with id/timestamp/data intact,
    /// and oversized events are dropped instead of truncated (review
    /// 2026-11-26, U3V event forwarding).
    #[test]
    fn device_events_follow_eirm_enable_and_length_bound() {
        let rf = RegisterFile::new(
            null_cfg(),
            std::sync::Arc::new(std::sync::Mutex::new(vgige_back_null::NullBack::new())),
        );
        let sent = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        rf.set_event_in(std::sync::Arc::new(EventPipe { sent: sent.clone() }));

        let event = DeviceEvent {
            event_id: 0x9001,
            timestamp_ns: 1_234,
            data: vec![1, 2, 3],
        };
        // Event Enable off (factory default): nothing goes out.
        assert_eq!(rf.send_device_event(&event), Ok(false));
        assert!(sent.lock().unwrap().is_empty());

        // Host enables the EIRM event interface (搂4.1.3.3.1: EI Control bit 0).
        rf.write(crate::EIRM_BASE, &1u32.to_le_bytes()).unwrap();
        assert_eq!(rf.send_device_event(&event), Ok(true));
        {
            let sent = sent.lock().unwrap();
            assert_eq!(sent.len(), 1);
            let decoded = packets::decode_event(&sent[0]).expect("EVENT_CMD packet");
            assert_eq!(decoded.event_id, 0x9001);
            assert_eq!(decoded.timestamp, 1_234);
            assert_eq!(decoded.data, vec![1, 2, 3]);
        }

        // Too large for Maximum Event Transfer Length: drop, do not truncate.
        let oversized = DeviceEvent {
            event_id: 7,
            timestamp_ns: 0,
            data: vec![0; crate::MAX_EVENT_TRANSFER_LEN as usize],
        };
        assert_eq!(rf.send_device_event(&oversized), Ok(false));
        assert_eq!(sent.lock().unwrap().len(), 1);
    }
    // -- Width-aware block endian conversion (review 2026-11-26 follow-up) --

    /// Big-endian backend (the GigE camera direction) with canned blocks.
    struct BigBack {
        reads: std::sync::Arc<std::sync::Mutex<Vec<(u32, Vec<u8>)>>>,
        writes: std::sync::Arc<std::sync::Mutex<Vec<(u32, Vec<u8>)>>>,
    }

    impl vgige_core::CameraBack for BigBack {
        fn register_endian(&self) -> vgige_core::RegisterEndian {
            vgige_core::RegisterEndian::Big
        }
        fn read_reg(&mut self, _addr: u32) -> Result<u32, u16> {
            Err(0x8001)
        }
        fn write_reg(&mut self, _addr: u32, _value: u32) -> Result<(), u16> {
            Ok(())
        }
        fn read_block(&mut self, addr: u32, len: u32) -> Result<Vec<u8>, u16> {
            if let Some((_, data)) = self
                .reads
                .lock()
                .unwrap()
                .iter()
                .find(|(a, d)| *a == addr && d.len() == len as usize)
            {
                return Ok(data.clone());
            }
            Ok(vec![0u8; len as usize])
        }
        fn write_block(&mut self, addr: u32, data: &[u8]) -> Result<(), u16> {
            self.writes.lock().unwrap().push((addr, data.to_vec()));
            Ok(())
        }
        fn fetch_xml(&mut self) -> Result<Vec<u8>, u16> {
            Ok(Vec::new())
        }
        fn fetch_identity(&mut self) -> Result<BackIdentity, u16> {
            Ok(BackIdentity {
                manufacturer: "m".into(),
                model: "m".into(),
                device_version: "1".into(),
                serial_number: "s".into(),
                user_defined_name: String::new(),
                spec_version_major: 2,
                spec_version_minor: 0,
            })
        }
        fn open_stream(&mut self) -> Result<Box<dyn vgige_core::FrameSource>, u16> {
            Err(0x8001)
        }
        fn close_stream(&mut self) -> Result<(), u16> {
            Ok(())
        }
    }

    /// Served XML (GigE personality, rewritten to LittleEndian for the U3V
    /// front by XmlMerger; widths are endianness-independent).
    const BIG_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<RegisterDescription Name="Test" ModelName="Test" VendorName="T"
  StandardNameSpace="None" SchemaMajorVersion="1" SchemaMinorVersion="1"
  SchemaSubMinorVersion="0" MajorVersion="1" MinorVersion="0" SubMinorVersion="0"
  ToolTip="test" ProductGuid="00000000-0000-0000-0000-000000000000"
  VersionGuid="00000000-0000-0000-0000-000000000000">
  <IntReg Name="WidthReg">
    <Address>0x00080000</Address><Length>4</Length><AccessMode>RW</AccessMode>
  </IntReg>
  <IntReg Name="HeightReg">
    <Address>0x00080004</Address><Length>4</Length><AccessMode>RW</AccessMode>
  </IntReg>
  <IntReg Name="PayloadSizeReg">
    <Address>0x00081000</Address><Length>8</Length><AccessMode>RO</AccessMode>
  </IntReg>
</RegisterDescription>
"#;

    fn big_register_file(
        reads: Vec<(u32, Vec<u8>)>,
    ) -> (
        RegisterFile,
        std::sync::Arc<std::sync::Mutex<Vec<(u32, Vec<u8>)>>>,
    ) {
        let writes = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let back: vgige_core::SharedBack = std::sync::Arc::new(std::sync::Mutex::new(BigBack {
            reads: std::sync::Arc::new(std::sync::Mutex::new(reads)),
            writes: std::sync::Arc::clone(&writes),
        }));
        let cfg = DeviceConfig::from_parts(
            BackIdentity {
                manufacturer: "m".into(),
                model: "m".into(),
                device_version: "1".into(),
                serial_number: "s".into(),
                user_defined_name: String::new(),
                spec_version_major: 2,
                spec_version_minor: 0,
            },
            BIG_XML.as_bytes(),
            3072,
            synthesize_guid("s"),
        );
        (RegisterFile::new(cfg, back), writes)
    }

    /// The U3V front's view of the same rule: a GenCP 8-byte READMEM of two
    /// 32-bit big-endian registers becomes two little-endian words in order;
    /// an XML-declared 64-bit register is reversed as one value.
    #[test]
    fn block_endian_uses_declared_register_widths() {
        let (rf, writes) = big_register_file(vec![
            (
                0x0008_0000,
                // Big-endian Width=1280, Height=1024.
                vec![0x00, 0x00, 0x05, 0x00, 0x00, 0x00, 0x04, 0x00],
            ),
            (
                0x0008_1000,
                0x0026_2140u64.to_be_bytes().to_vec(), // 2,621,440
            ),
        ]);

        let mut pair = [0u8; 8];
        rf.read(0x0008_0000, &mut pair).expect("READMEM pair");
        assert_eq!(pair, [0x00, 0x05, 0x00, 0x00, 0x00, 0x04, 0x00, 0x00]);
        assert_eq!(u32::from_le_bytes(pair[0..4].try_into().unwrap()), 1280);
        assert_eq!(u32::from_le_bytes(pair[4..8].try_into().unwrap()), 1024);

        let mut value = [0u8; 8];
        rf.read(0x0008_1000, &mut value).expect("READMEM u64");
        assert_eq!(u64::from_le_bytes(value), 0x0026_2140);

        rf.write(0x0008_0000, &pair).expect("WRITEMEM pair");
        let writes = writes.lock().unwrap();
        assert_eq!(
            writes.as_slice(),
            &[(0x0008_0000, vec![0x00, 0x00, 0x05, 0x00, 0x00, 0x00, 0x04, 0x00])],
            "Width/Height keep their own addresses in the big-endian backend"
        );
    }
}