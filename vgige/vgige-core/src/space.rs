//! The virtual device's address dispatch, without the server: every
//! register/memory access from a front-end lands here and is routed to one
//! of four destinations (design note §3):
//!
//! - pass-through: bootstrap + vendor space `< 0x2000_0000`, forwarded to
//!   the real camera via [`CameraBack`] with GEV status codes preserved
//!   verbatim — error transparency is the client-compatibility contract;
//! - local: the custom window `0x2000_0000..0x2000_1000` (256 u32 slots),
//!   the virtualized bootstrap registers (DEVICE_MAC/CURRENT_IP =
//!   up-facing NIC values, FIRST_URL = injected XML, GVCP_CAPABILITY with
//!   packet_resend masked per R-197ca), and the simplified local CCP;
//! - the injected XML blob at `0x2000_1000..` (READMEM only);
//! - the stream channel registers (§36.57–§36.60), routed to the [`Relay`]
//!   (channel 0; other channels answer GEV_STATUS_INVALID_ADDRESS like a
//!   device without them, §36.57).
//!
//! CCP is the simulator's simplified model (notes/OPEN-QUESTIONS.md Q7):
//! no peer identity at this layer, so a repeated identical request while
//! occupied is accepted (R-069cd applied to any writer), 0 releases, EA
//! grants take priority over CA.
//!
//! GVCP-shaped by nature (32-bit big-endian register space): GigE-style
//! fronts use it; a USB3 Vision gadget front owns a different device model
//! and does not (roadmap Step 3 — which is why this is a concrete type and
//! not a trait).

use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use gentl_gvcp::registers as reg;
use gentl_gvcp::wire::*;

use crate::back::SharedBack;
use crate::relay::Relay;
use crate::xml::XmlMerger;
use crate::{slot, CUSTOM_BASE, CUSTOM_SLOTS, VGIGE_VERSION, XML_BASE_V};

/// Simplified single-host CCP state (spec §36.46): `flags` is the granted
/// privilege, with connection_count always zero to the requesting host.
struct CcpState {
    flags: u32,
    last_contact: Option<Instant>,
    heartbeat_ms: u32,
}

impl Default for CcpState {
    fn default() -> Self {
        Self {
            flags: 0,
            last_contact: None,
            heartbeat_ms: 3000,
        }
    }
}

impl CcpState {
    fn expire_stale_grant(&mut self) -> bool {
        let stale = self.last_contact.is_some_and(|last| {
            last.elapsed().as_millis() > u128::from(self.heartbeat_ms)
        });
        if stale {
            self.flags = 0;
            self.last_contact = None;
        }
        stale
    }
}

/// The virtual device's register space. Cloneable (Arcs inside) so a
/// front-end can keep a handle for business hooks while the server owns
/// its own clone.
pub struct RegisterSpace {
    back: SharedBack,
    xml: Arc<XmlMerger>,
    /// Virtual identity: the up-facing NIC's IP/MAC (the camera's stay
    /// hidden from the client).
    up_ip: u32,
    up_mac: [u8; 6],
    /// Virtual First URL slot content (512 bytes, NUL padded).
    url_slot: Arc<[u8; 512]>,
    ccp: Arc<Mutex<CcpState>>,
    /// Custom-window general storage (slots 5..256); slots 0..4 are
    /// special-cased on access.
    slots: Arc<Mutex<[u32; CUSTOM_SLOTS]>>,
    channel: Relay,
    ai_result: Arc<AtomicU32>,
    ai_score: Arc<AtomicU32>,
}

impl Clone for RegisterSpace {
    fn clone(&self) -> Self {
        Self {
            back: Arc::clone(&self.back),
            xml: Arc::clone(&self.xml),
            up_ip: self.up_ip,
            up_mac: self.up_mac,
            url_slot: Arc::clone(&self.url_slot),
            ccp: Arc::clone(&self.ccp),
            slots: Arc::clone(&self.slots),
            channel: self.channel.clone(),
            ai_result: Arc::clone(&self.ai_result),
            ai_score: Arc::clone(&self.ai_score),
        }
    }
}

impl RegisterSpace {
    pub fn new(
        back: SharedBack,
        xml: Arc<XmlMerger>,
        up_ip: Ipv4Addr,
        up_mac: [u8; 6],
        channel: Relay,
    ) -> Self {
        let mut url_slot = [0u8; 512];
        let url = xml.url().as_bytes();
        url_slot[..url.len()].copy_from_slice(url);
        Self {
            back,
            xml,
            up_ip: u32::from(up_ip),
            up_mac,
            url_slot: Arc::new(url_slot),
            ccp: Arc::new(Mutex::new(CcpState::default())),
            slots: Arc::new(Mutex::new([0; CUSTOM_SLOTS])),
            channel,
            ai_result: Arc::new(AtomicU32::new(0)),
            ai_score: Arc::new(AtomicU32::new(0)),
        }
    }

    /// Business-layer hook: set the AI_Result slot (0 = not inspected,
    /// 1 = OK, 2 = NG).
    pub fn set_ai_result(&self, v: u32) {
        self.ai_result.store(v, Ordering::Relaxed);
    }

    /// Business-layer hook: set the AI_Score slot (anomaly score x1000).
    pub fn set_ai_score(&self, v: u32) {
        self.ai_score.store(v, Ordering::Relaxed);
    }

    fn slot_read(&self, idx: usize) -> u32 {
        match idx {
            slot::VERSION => VGIGE_VERSION,
            slot::AI_RESULT => self.ai_result.load(Ordering::Relaxed),
            slot::AI_SCORE => self.ai_score.load(Ordering::Relaxed),
            slot::FRAMES_RELAYED => self.channel.count(),
            _ => self.slots.lock().expect("vgige slots poisoned")[idx],
        }
    }

    fn slot_write(&self, idx: usize, value: u32) -> Result<(), u16> {
        match idx {
            slot::VERSION | slot::AI_RESULT | slot::AI_SCORE | slot::FRAMES_RELAYED => {
                Err(GEV_STATUS_WRITE_PROTECT)
            }
            slot::USER_COMMAND => {
                self.slots.lock().expect("vgige slots poisoned")[idx] = value;
                // Business hook placeholder: log and accept.
                eprintln!("vgige: UserCommand <= 0x{value:08X}");
                Ok(())
            }
            _ => {
                self.slots.lock().expect("vgige slots poisoned")[idx] = value;
                Ok(())
            }
        }
    }

    fn ccp_read(&self) -> u32 {
        let mut c = self.ccp.lock().expect("vgige ccp poisoned");
        if c.expire_stale_grant() {
            self.channel.close();
        }
        if c.flags != 0 {
            c.last_contact = Some(Instant::now());
        }
        c.flags
    }

    fn ccp_write(&self, value: u32) -> Result<(), u16> {
        let mut c = self.ccp.lock().expect("vgige ccp poisoned");
        if c.expire_stale_grant() {
            self.channel.close();
        }
        if value == 0 {
            c.flags = 0;
            c.last_contact = None;
            self.channel.close();
            return Ok(());
        }
        let requested = value & (reg::CCP_EXCLUSIVE_ACCESS | reg::CCP_CONTROL_ACCESS);
        if requested == 0 {
            // No privilege bits: nothing to grant, accept as a no-op.
            return Ok(());
        }
        if c.flags == 0 {
            // EA takes priority over CA when both are requested.
            c.flags = if requested & reg::CCP_EXCLUSIVE_ACCESS != 0 {
                reg::CCP_EXCLUSIVE_ACCESS
            } else {
                reg::CCP_CONTROL_ACCESS
            };
            c.last_contact = Some(Instant::now());
            return Ok(());
        }
        // R-069cd: a repeated identical request refreshes the grant.
        if requested == c.flags {
            c.last_contact = Some(Instant::now());
            Ok(())
        } else {
            Err(GEV_STATUS_ACCESS_DENIED)
        }
    }

    fn back_read_reg(&self, addr: u32) -> Result<u32, u16> {
        self.back
            .lock()
            .map_err(|_| crate::GEV_STATUS_ERROR)?
            .read_reg(addr)
    }

    fn back_write_reg(&self, addr: u32, value: u32) -> Result<(), u16> {
        self.back
            .lock()
            .map_err(|_| crate::GEV_STATUS_ERROR)?
            .write_reg(addr, value)
    }

    fn back_read_block(&self, addr: u32, count: u16) -> Result<Vec<u8>, u16> {
        self.back
            .lock()
            .map_err(|_| crate::GEV_STATUS_ERROR)?
            .read_block(addr, u32::from(count))
    }

    /// Read one 32-bit register; Err is a GEV status code.
    pub fn read_reg(&self, addr: u32) -> Result<u32, u16> {
        if addr % 4 != 0 {
            return Err(GEV_STATUS_BAD_ALIGNMENT);
        }
        // Stream channel registers: the virtual channel (§36.57–§36.60),
        // never forwarded.
        if let Some((x, off)) = reg::stream_channel_addr(addr) {
            if x != 0 {
                return Err(GEV_STATUS_INVALID_ADDRESS);
            }
            return self.channel.read_reg(off);
        }
        // Custom window: local slots.
        if addr >= CUSTOM_BASE && addr < CUSTOM_BASE + (CUSTOM_SLOTS as u32) * 4 {
            return Ok(self.slot_read(((addr - CUSTOM_BASE) / 4) as usize));
        }
        // XML blob: memory area, register access is not mapped (same split
        // the simulator applies: registers vs blob).
        if addr >= XML_BASE_V && addr < self.xml.base() + self.xml.blob().len() as u32 {
            return Err(GEV_STATUS_INVALID_ADDRESS);
        }
        // Virtualized bootstrap registers.
        match addr {
            reg::CCP => return Ok(self.ccp_read()),
            reg::HEARTBEAT_TIMEOUT => {
                return Ok(self.ccp.lock().expect("vgige ccp poisoned").heartbeat_ms)
            }
            reg::NUM_STREAM_CHANNELS => return Ok(1),
            reg::DEVICE_MAC_HIGH => {
                return Ok(u16::from_be_bytes([self.up_mac[0], self.up_mac[1]]) as u32)
            }
            reg::DEVICE_MAC_LOW => {
                return Ok(u32::from_be_bytes([
                    self.up_mac[2],
                    self.up_mac[3],
                    self.up_mac[4],
                    self.up_mac[5],
                ]))
            }
            reg::CURRENT_IP => return Ok(self.up_ip),
            reg::GVCP_CAPABILITY => {
                // The virtual server never answers PACKETRESEND_CMD (the
                // DeviceHandler default ignores it), so the capability must
                // not advertise it: R-197ca forbids requesters to send
                // resends to a device without the bit.
                return self
                    .back_read_reg(addr)
                    .map(|v| v & !reg::GVCP_CAPABILITY_PACKET_RESEND);
            }
            _ => {}
        }
        // Virtual First URL slot (0x0200..0x0400): the injected XML's
        // local: address, served locally.
        if addr >= reg::FIRST_URL && addr < reg::SECOND_URL {
            let off = (addr - reg::FIRST_URL) as usize;
            let slot = &self.url_slot[off..off + 4];
            return Ok(u32::from_be_bytes([slot[0], slot[1], slot[2], slot[3]]));
        }
        // Everything else passes through; the camera's GEV status answers
        // verbatim (error transparency).
        self.back_read_reg(addr)
    }

    /// Write one 32-bit register; Err is a GEV status code.
    pub fn write_reg(&self, addr: u32, value: u32) -> Result<(), u16> {
        if addr % 4 != 0 {
            return Err(GEV_STATUS_BAD_ALIGNMENT);
        }
        if let Some((x, off)) = reg::stream_channel_addr(addr) {
            if x != 0 {
                return Err(GEV_STATUS_INVALID_ADDRESS);
            }
            return self.channel.write_reg(off, value);
        }
        if addr >= CUSTOM_BASE && addr < CUSTOM_BASE + (CUSTOM_SLOTS as u32) * 4 {
            return self.slot_write(((addr - CUSTOM_BASE) / 4) as usize, value);
        }
        if addr >= XML_BASE_V && addr < self.xml.base() + self.xml.blob().len() as u32 {
            return Err(GEV_STATUS_INVALID_ADDRESS);
        }
        if addr == reg::CCP {
            return self.ccp_write(value);
        }
        if addr == reg::HEARTBEAT_TIMEOUT {
            self.ccp.lock().expect("vgige ccp poisoned").heartbeat_ms = value.max(500);
            return Ok(());
        }
        self.back_write_reg(addr, value)
    }

    /// Block read (`count` bytes, a multiple of 4); Err is a GEV status code.
    pub fn read_block(&self, addr: u32, count: u16) -> Result<Vec<u8>, u16> {
        let len = u32::from(count);
        let end = addr.checked_add(len).ok_or(GEV_STATUS_INVALID_ADDRESS)?;
        // Virtual First URL slot.
        if addr >= reg::FIRST_URL && end <= reg::SECOND_URL {
            let off = (addr - reg::FIRST_URL) as usize;
            return Ok(self.url_slot[off..off + count as usize].to_vec());
        }
        // Custom window: per-word slot semantics.
        if addr >= CUSTOM_BASE && end <= CUSTOM_BASE + (CUSTOM_SLOTS as u32) * 4 {
            let mut out = Vec::with_capacity(count as usize);
            let mut pos = addr;
            while pos < end {
                out.extend_from_slice(&self.slot_read(((pos - CUSTOM_BASE) / 4) as usize).to_be_bytes());
                pos += 4;
            }
            return Ok(out);
        }
        // Injected XML blob. Reads may extend past the blob end: the
        // GenTL-side alignment adaptation (§6.3.6.5) reads 4-aligned
        // windows, so the tail is zero-padded rather than rejected (the
        // blob is vgige's own memory, not a device mapping boundary).
        if addr >= self.xml.base() && addr < self.xml.base() + self.xml.blob().len() as u32 {
            let start = (addr - self.xml.base()) as usize;
            let take = (self.xml.blob().len() - start).min(count as usize);
            let mut out = Vec::with_capacity(count as usize);
            out.extend_from_slice(&self.xml.blob()[start..start + take]);
            out.resize(count as usize, 0);
            return Ok(out);
        }
        if addr >= XML_BASE_V {
            // Inside the local area but past the blob: unmapped.
            return Err(GEV_STATUS_INVALID_ADDRESS);
        }
        let virtual_reg = reg::stream_channel_addr(addr).is_some()
            || matches!(
                addr,
                reg::CCP
                    | reg::HEARTBEAT_TIMEOUT
                    | reg::NUM_STREAM_CHANNELS
                    | reg::DEVICE_MAC_HIGH
                    | reg::DEVICE_MAC_LOW
                    | reg::CURRENT_IP
                    | reg::GVCP_CAPABILITY
            );
        if count == 4 && virtual_reg {
            return self.read_reg(addr).map(|value| value.to_be_bytes().to_vec());
        }
        // Pass through. Ranges overlapping the virtualized slots above
        // (First URL / custom window / blob) but not contained in them are
        // unusual; the camera's answer for the range is forwarded.
        self.back_read_block(addr, count)
    }

    /// Block write (`data.len()` bytes, a multiple of 4); Err is a GEV status code.
    pub fn write_block(&self, addr: u32, data: &[u8]) -> Result<(), u16> {
        if data.is_empty() || data.len() % 4 != 0 {
            return Err(GEV_STATUS_BAD_ALIGNMENT);
        }
        let len = data.len() as u32;
        let end = addr.checked_add(len).ok_or(GEV_STATUS_INVALID_ADDRESS)?;
        if addr >= reg::FIRST_URL && end <= reg::SECOND_URL {
            return Err(GEV_STATUS_WRITE_PROTECT);
        }
        if addr >= CUSTOM_BASE && end <= CUSTOM_BASE + (CUSTOM_SLOTS as u32) * 4 {
            for (index, chunk) in data.chunks_exact(4).enumerate() {
                let slot = ((addr - CUSTOM_BASE) / 4) as usize + index;
                self.slot_write(slot, u32::from_be_bytes(chunk.try_into().expect("u32")))?;
            }
            return Ok(());
        }
        if addr >= XML_BASE_V {
            return Err(GEV_STATUS_WRITE_PROTECT);
        }
        let virtual_reg = reg::stream_channel_addr(addr).is_some()
            || matches!(
                addr,
                reg::CCP
                    | reg::HEARTBEAT_TIMEOUT
                    | reg::NUM_STREAM_CHANNELS
                    | reg::DEVICE_MAC_HIGH
                    | reg::DEVICE_MAC_LOW
                    | reg::CURRENT_IP
                    | reg::GVCP_CAPABILITY
            );
        if data.len() == 4 && virtual_reg {
            return self.write_reg(addr, u32::from_be_bytes(data[..4].try_into().expect("u32")));
        }
        self.back
            .lock()
            .map_err(|_| crate::GEV_STATUS_ERROR)?
            .write_block(addr, data)
    }
}
