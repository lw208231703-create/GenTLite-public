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
//! CCP is the simulator's simplified model (docs/00-governance/OPEN-QUESTIONS.md Q7):
//! no peer identity at this layer, so a repeated identical request while
//! occupied is accepted (R-069cd applied to any writer), 0 releases, EA
//! grants take priority over CA.
//!
//! GVCP-shaped by nature (32-bit big-endian register space): GigE-style
//! fronts use it; a USB3 Vision gadget front owns a different device model
//! and does not (roadmap Step 3 — which is why this is a concrete type and
//! not a trait).

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use gentl_gvcp::registers as reg;
use gentl_gvcp::wire::*;

use crate::back::{swap_register_endian, RegisterEndian, SharedBack};
use crate::relay::Relay;
use crate::xml::XmlMerger;
use crate::{slot, CUSTOM_BASE, CUSTOM_SLOTS, VGIGE_VERSION, XML_BASE_V};

/// Simplified single-host CCP state (spec §36.46): `flags` is the granted
/// privilege, with connection_count always zero to the requesting host.
struct CcpState {
    flags: u32,
    peer: Option<SocketAddr>,
    last_contact: Option<Instant>,
    heartbeat_ms: u32,
}

impl Default for CcpState {
    fn default() -> Self {
        Self {
            flags: 0,
            peer: None,
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
            self.peer = None;
            self.last_contact = None;
        }
        stale
    }

    fn update(&mut self, value: u32, peer: Option<SocketAddr>) -> (Result<(), u16>, bool) {
        if value == 0 {
            if peer.is_some() && self.peer.is_some() && peer != self.peer {
                return (Err(GEV_STATUS_ACCESS_DENIED), false);
            }
            self.flags = 0;
            self.peer = None;
            self.last_contact = None;
            return (Ok(()), true);
        }
        let requested = value & (reg::CCP_EXCLUSIVE_ACCESS | reg::CCP_CONTROL_ACCESS);
        if requested == 0 {
            return (Ok(()), false);
        }
        let granted = if requested & reg::CCP_EXCLUSIVE_ACCESS != 0 {
            reg::CCP_EXCLUSIVE_ACCESS
        } else {
            reg::CCP_CONTROL_ACCESS
        };
        if self.flags == 0 {
            self.flags = granted;
            self.peer = peer;
            self.last_contact = Some(Instant::now());
            return (Ok(()), false);
        }
        // R-070cd: the primary may switch between EA and CA without closing
        // the channel; another application is denied (no CSE support).
        let same_peer = match (self.peer, peer) {
            (Some(owner), Some(requester)) => owner == requester,
            (None, _) => true,
            (Some(_), None) => false,
        };
        if !same_peer {
            return (Err(GEV_STATUS_ACCESS_DENIED), false);
        }
        self.flags = granted;
        self.last_contact = Some(Instant::now());
        (Ok(()), false)
    }
}

/// Virtual-device identity strings behind the bootstrap string registers
/// (§36.3–§36.9 / Table 36-1). Served locally: forwarding a string READMEM
/// through a little-endian backend would word-swap it into garbage, and the
/// client must see one coherent identity (the same one the discovery payload
/// reports).
#[derive(Clone)]
struct IdentityStrings {
    manufacturer: [u8; 32],
    model: [u8; 32],
    device_version: [u8; 32],
    manufacturer_info: [u8; 48],
    serial: [u8; 16],
    /// §36.16 USER_DEFINED_NAME is read/write; the virtual device stores it
    /// locally instead of touching the camera's name.
    user_defined_name: Arc<Mutex<[u8; 16]>>,
}

impl Default for IdentityStrings {
    fn default() -> Self {
        Self {
            manufacturer: [0; 32],
            model: [0; 32],
            device_version: [0; 32],
            manufacturer_info: [0; 48],
            serial: [0; 16],
            user_defined_name: Arc::new(Mutex::new([0; 16])),
        }
    }
}

/// Copy `s` into a fixed bootstrap string slot, NUL/zero padded (§36.3: the
/// registers are fixed-size strings, zero-terminated when shorter).
fn put_str<const N: usize>(slot: &mut [u8; N], s: &str) {
    let bytes = s.as_bytes();
    let n = bytes.len().min(N);
    slot[..n].copy_from_slice(&bytes[..n]);
}

/// Virtual message channel state (spec §12): enabled when the downstream
/// backend relays events, plus the host destination written through MCDA +
/// MCP (§36.51/§36.52) and the mandatory MCTT/MCRC registers (§36.53/§36.54).
#[derive(Default)]
struct MessageChannel {
    relay: Option<Arc<crate::MessageRelay>>,
    /// MCDA (§36.52): destination IPv4 address.
    dest_address: u32,
    /// MCP (§36.51): host UDP port in bits 16-31; 0 closes the channel.
    port: u32,
}

/// The virtual device's register space. Cloneable (Arcs inside) so a
/// front-end can keep a handle for business hooks while the server owns
/// its own clone.
pub struct RegisterSpace {
    back: SharedBack,
    /// Native byte order of the backend's block register data. The virtual
    /// GigE register space is always big-endian; a little-endian backend
    /// (the null demo / U3V) is converted at this boundary.
    back_endian: RegisterEndian,
    xml: Arc<XmlMerger>,
    /// Virtual identity: the up-facing NIC's IP/MAC (the camera's stay
    /// hidden from the client).
    up_ip: Arc<AtomicU32>,
    up_mac: [u8; 6],
    /// Virtual device bootstrap identity: spec version (major<<16|minor) and
    /// the up-facing subnet mask. Kept coherent with the discovery payload.
    spec_version: u32,
    subnet_mask: Arc<AtomicU32>,
    /// Virtual First URL slot content (512 bytes, NUL padded).
    url_slot: Arc<[u8; 512]>,
    /// Locally served bootstrap identity strings.
    identity: IdentityStrings,
    /// Virtual message channel (GigE device event relay).
    message: Arc<Mutex<MessageChannel>>,
    ccp: Arc<Mutex<CcpState>>,
    /// Custom-window general storage (slots 5..256); slots 0..4 are
    /// special-cased on access.
    slots: Arc<Mutex<[u32; CUSTOM_SLOTS]>>,
    channel: Relay,
    ai_result: Arc<AtomicU32>,
    ai_score: Arc<AtomicU32>,
    pending_timeout_ms: Arc<AtomicU32>,
}

impl Clone for RegisterSpace {
    fn clone(&self) -> Self {
        Self {
            back: Arc::clone(&self.back),
            back_endian: self.back_endian,
            xml: Arc::clone(&self.xml),
            up_ip: Arc::clone(&self.up_ip),
            up_mac: self.up_mac,
            spec_version: self.spec_version,
            subnet_mask: Arc::clone(&self.subnet_mask),
            url_slot: Arc::clone(&self.url_slot),
            identity: self.identity.clone(),
            message: Arc::clone(&self.message),
            ccp: Arc::clone(&self.ccp),
            slots: Arc::clone(&self.slots),
            channel: self.channel.clone(),
            ai_result: Arc::clone(&self.ai_result),
            ai_score: Arc::clone(&self.ai_score),
            pending_timeout_ms: Arc::clone(&self.pending_timeout_ms),
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
        let back_endian = back
            .lock()
            .map(|b| b.register_endian())
            .unwrap_or(RegisterEndian::Big);
        Self {
            back,
            back_endian,
            xml,
            up_ip: Arc::new(AtomicU32::new(u32::from(up_ip))),
            up_mac,
            spec_version: 0x0003_0000,
            subnet_mask: Arc::new(AtomicU32::new(0)),
            url_slot: Arc::new(url_slot),
            identity: IdentityStrings::default(),
            message: Arc::new(Mutex::new(MessageChannel::default())),
            ccp: Arc::new(Mutex::new(CcpState::default())),
            slots: Arc::new(Mutex::new([0; CUSTOM_SLOTS])),
            channel,
            ai_result: Arc::new(AtomicU32::new(0)),
            ai_score: Arc::new(AtomicU32::new(0)),
            pending_timeout_ms: Arc::new(AtomicU32::new(1000)),
        }
    }

    /// Business-layer hook: set the AI_Result slot (0 = not inspected,
    /// 1 = OK, 2 = NG).
    pub fn set_ai_result(&self, v: u32) {
        self.ai_result.store(v, Ordering::Relaxed);
    }

    /// Keep the bootstrap registers (Version / subnet) coherent with the
    /// discovery identity. Called before the register space is shared with
    /// the front; the defaults are GigE Vision 3.0 and mask 0.
    /// Current virtual device IPv4 address (FORCEIP-visible).
    pub fn current_ip(&self) -> u32 {
        self.up_ip.load(Ordering::SeqCst)
    }

    /// Current virtual device subnet mask.
    pub fn current_subnet_mask(&self) -> u32 {
        self.subnet_mask.load(Ordering::SeqCst)
    }

    pub fn set_bootstrap(&mut self, spec_version: u32, subnet_mask: Ipv4Addr) {
        self.spec_version = spec_version;
        self.subnet_mask.store(u32::from(subnet_mask), Ordering::SeqCst);
    }

    /// Enable the virtual message channel and route the camera's events
    /// (§17) to whichever host opens it. Called only when the downstream
    /// backend really started an event relay; otherwise the device keeps
    /// advertising zero message channels (§36.22).
    pub fn set_message_channel(&mut self, relay: Arc<crate::MessageRelay>) {
        if let Ok(mut message) = self.message.lock() {
            message.relay = Some(relay);
        }
    }

    /// Capture the camera identity for the locally served bootstrap string
    /// registers. The virtual device presents the camera's vendor/model/
    /// version/serial strings (transparency) through its own register model,
    /// so a little-endian backend cannot corrupt them via READMEM byte
    /// swapping.
    pub fn set_identity(&mut self, id: &DiscoveryInfo) {
        let mut identity = IdentityStrings::default();
        put_str(&mut identity.manufacturer, &id.manufacturer_name);
        put_str(&mut identity.model, &id.model_name);
        put_str(&mut identity.device_version, &id.device_version);
        put_str(&mut identity.manufacturer_info, &id.manufacturer_info);
        put_str(&mut identity.serial, &id.serial_number);
        {
            let mut user = identity
                .user_defined_name
                .lock()
                .expect("vgige identity poisoned");
            put_str(&mut user, &id.user_defined_name);
        }
        self.identity = identity;
    }

    /// FORCEIP_CMD (§16.2) policy of the virtual GigE device.
    ///
    /// The advertised MAC is the up-facing NIC's, so a request for another
    /// device is discarded (R-149cd), and so is one arriving while a remote
    /// host holds the control channel (R-150cd). A non-zero forced address
    /// cannot be honoured by a relay: rebinding the host interface is
    /// destructive and out of scope, so the request is discarded with a
    /// warning instead of pretending the address changed (design decision:
    /// docs/00-governance/OPEN-QUESTIONS.md Q40). static_IP == 0 still
    /// performs the required stream close and IP-cycle restart (R-151cd).
    pub fn force_ip(&self, request: &ForceIpRequest) -> ForceIpOutcome {
        if request.mac != self.up_mac {
            return ForceIpOutcome::Ignore;
        }
        // R-076cd/R-150cd: a heartbeat-expired grant must be cleared before
        // deciding whether another application still holds the channel.
        let expired = {
            let mut ccp = self.ccp.lock().expect("vgige ccp poisoned");
            ccp.expire_stale_grant()
        };
        if expired {
            self.cleanup_control_channel();
        }
        let granted = {
            let ccp = self.ccp.lock().expect("vgige ccp poisoned");
            ccp.flags != 0
        };
        if granted {
            return ForceIpOutcome::Ignore;
        }
        if request.static_ip == 0 {
            self.channel.close();
            self.close_message_channel();
            return ForceIpOutcome::Ignore;
        }
        // R-152cd: install the temporary static address before the ACK.
        // The gateway's virtual identity (CURRENT_IP/CURRENT_SUBNET_MASK and
        // discovery) moves with it; the OS NIC address itself is a platform
        // concern outside this process. Apply while no control grant exists.
        self.up_ip.store(request.static_ip, Ordering::SeqCst);
        self.subnet_mask
            .store(request.subnet_mask, Ordering::SeqCst);
        ForceIpOutcome::Ack
    }

    /// Independent heartbeat watchdog (R-076cd): expire an idle grant and
    /// run the R-072cd cleanup even when no further command arrives.
    pub fn tick(&self) {
        let expired = {
            let mut ccp = self.ccp.lock().expect("vgige ccp poisoned");
            ccp.expire_stale_grant()
        };
        if expired {
            self.cleanup_control_channel();
        }
    }

    /// R-151cd: clear MCDA/MCP and stop forwarding events to the old host.
    fn close_message_channel(&self) {
        if let Ok(mut message) = self.message.lock() {
            message.dest_address = 0;
            message.port = 0;
            if let Some(relay) = &message.relay {
                relay.set_destination(None);
            }
        }
    }

    /// Bootstrap string slot bytes for `[addr, addr+count)`, zero padded.
    fn identity_bytes(&self, addr: u32, count: u16) -> Option<Vec<u8>> {
        let len = u32::from(count);
        let end = addr.checked_add(len)?;
        // Only a range fully inside one slot is served locally (hosts read
        // one bootstrap string at a time).
        let slot: Option<(u32, Vec<u8>)> = if addr >= reg::MANUFACTURER_NAME
            && end <= reg::MANUFACTURER_NAME + 32
        {
            Some((reg::MANUFACTURER_NAME, self.identity.manufacturer.to_vec()))
        } else if addr >= reg::MODEL_NAME && end <= reg::MODEL_NAME + 32 {
            Some((reg::MODEL_NAME, self.identity.model.to_vec()))
        } else if addr >= reg::DEVICE_VERSION && end <= reg::DEVICE_VERSION + 32 {
            Some((reg::DEVICE_VERSION, self.identity.device_version.to_vec()))
        } else if addr >= reg::MANUFACTURER_INFO && end <= reg::MANUFACTURER_INFO + 48 {
            Some((
                reg::MANUFACTURER_INFO,
                self.identity.manufacturer_info.to_vec(),
            ))
        } else if addr >= reg::SERIAL_NUMBER && end <= reg::SERIAL_NUMBER + 16 {
            Some((reg::SERIAL_NUMBER, self.identity.serial.to_vec()))
        } else if addr >= reg::USER_DEFINED_NAME && end <= reg::USER_DEFINED_NAME + 16 {
            Some((
                reg::USER_DEFINED_NAME,
                self.identity
                    .user_defined_name
                    .lock()
                    .expect("vgige identity poisoned")
                    .to_vec(),
            ))
        } else {
            None
        };
        let (base, bytes) = slot?;
        let off = (addr - base) as usize;
        let mut out = vec![0u8; count as usize];
        let take = bytes.len().saturating_sub(off).min(count as usize);
        out[..take].copy_from_slice(&bytes[off..off + take]);
        Some(out)
    }

    /// Write into the writable bootstrap string slot (USER_DEFINED_NAME) or
    /// answer WRITE_PROTECT for the read-only identity strings. `None` when
    /// the range is not a virtualised string slot.
    fn identity_write(&self, addr: u32, data: &[u8]) -> Option<Result<(), u16>> {
        let end = addr.checked_add(data.len() as u32)?;
        if addr >= reg::USER_DEFINED_NAME && end <= reg::USER_DEFINED_NAME + 16 {
            let mut user = self
                .identity
                .user_defined_name
                .lock()
                .expect("vgige identity poisoned");
            let off = (addr - reg::USER_DEFINED_NAME) as usize;
            user[off..off + data.len()].copy_from_slice(data);
            return Some(Ok(()));
        }
        if self.identity_bytes(addr, data.len() as u16).is_some() {
            return Some(Err(GEV_STATUS_WRITE_PROTECT));
        }
        None
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
        let expired = c.expire_stale_grant();
        let flags = c.flags;
        drop(c);
        if expired {
            // R-072cd: closing the primary control channel aborts streaming
            // and clears the message channel state.
            self.cleanup_control_channel();
        }
        flags
    }

    fn ccp_write(&self, value: u32, peer: Option<SocketAddr>) -> Result<(), u16> {
        let mut c = self.ccp.lock().expect("vgige ccp poisoned");
        let expired = c.expire_stale_grant();
        let (result, close) = c.update(value, peer);
        drop(c);
        if expired || close {
            self.cleanup_control_channel();
        }
        result
    }

    /// R-072cd: closing the primary control channel stops streaming and
    /// clears the message channel (MCP/MCDA) and GVSP stream (SCPx) state.
    fn cleanup_control_channel(&self) {
        self.channel.close();
        self.close_message_channel();
    }

    /// GigE Vision 10.1 access rules: exclusive access denies every other
    /// application (R-063cd), control access allows secondary reads only
    /// (R-170cd writes). Primary traffic resets the heartbeat; an expired
    /// channel is closed and cleaned up per R-076cd/R-072cd.
    fn authorize(&self, peer: SocketAddr, write: bool) -> Result<(), u16> {
        let expired = {
            let mut c = self.ccp.lock().expect("vgige ccp poisoned");
            if c.flags == 0 {
                return Ok(());
            }
            if c.expire_stale_grant() {
                true
            } else {
                let exclusive = c.flags & reg::CCP_EXCLUSIVE_ACCESS != 0;
                if c.peer == Some(peer) {
                    c.last_contact = Some(Instant::now());
                } else if write || exclusive {
                    return Err(GEV_STATUS_ACCESS_DENIED);
                }
                false
            }
        };
        if expired {
            self.cleanup_control_channel();
        }
        Ok(())
    }

    pub fn read_reg_from(&self, peer: SocketAddr, addr: u32) -> Result<u32, u16> {
        self.authorize(peer, false)?;
        self.read_reg(addr)
    }

    pub fn read_mem_from(
        &self,
        peer: SocketAddr,
        addr: u32,
        count: u16,
    ) -> Result<Vec<u8>, u16> {
        self.authorize(peer, false)?;
        self.read_block(addr, count)
    }

    pub fn write_mem_from(
        &self,
        peer: SocketAddr,
        addr: u32,
        data: &[u8],
    ) -> Result<(), u16> {
        self.write_mem_from_progress(peer, addr, data)
            .map_err(|(status, _)| status)
    }

    /// R-194cd: WRITEMEM with the byte index of a mid-block failure.
    pub fn write_mem_from_progress(
        &self,
        peer: SocketAddr,
        addr: u32,
        data: &[u8],
    ) -> Result<(), (u16, u32)> {
        self.authorize(peer, true).map_err(|status| (status, 0))?;
        self.write_block_progress(addr, data)
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
        let mut data = self
            .back
            .lock()
            .map_err(|_| crate::GEV_STATUS_ERROR)?
            .read_block(addr, u32::from(count))?;
        if self.back_endian == RegisterEndian::Little {
            swap_register_endian(&mut data);
        }
        Ok(data)
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
            reg::VERSION => return Ok(self.spec_version),
            reg::DEVICE_MODE => return Ok(0x8000_0001),
            reg::NET_IF_CAPABILITY => {
                // §4.3: DHCP support bit ("always 1"); §4.4: LLA support bit
                // ("always 1"). The virtual device lives on the up NIC's
                // fixed address and does not implement Persistent IP (§4.2:
                // optional), so bit 31 stays clear and the persistent-IP
                // registers are not mapped.
                return Ok(reg::IP_CFG_DHCP | reg::IP_CFG_LLA)
            }
            reg::NET_IF_CONFIGURATION => {
                // §4.4: bit 29 is always set (LLA is the default, always
                // active scheme).
                return Ok(reg::IP_CFG_LLA)
            }
            reg::CURRENT_SUBNET_MASK => return Ok(self.subnet_mask.load(Ordering::SeqCst)),
            reg::NUM_NETWORK_INTERFACES => return Ok(1),
            reg::CCP => return Ok(self.ccp_read()),
            reg::HEARTBEAT_TIMEOUT => {
                return Ok(self.ccp.lock().expect("vgige ccp poisoned").heartbeat_ms)
            }
            reg::PENDING_TIMEOUT => {
                return Ok(self.pending_timeout_ms.load(Ordering::SeqCst))
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
            reg::CURRENT_IP => return Ok(self.up_ip.load(Ordering::SeqCst)),
            reg::GVCP_CAPABILITY => {
                // The capability bits must describe THIS device (§36.29):
                // the virtual server answers READREG/WRITEREG/READMEM, serves
                // the UN/SN bootstrap strings, and answers PACKETRESEND from
                // the GigE front's block cache (§16.7/R-197ca). EVENT (§16.8),
                // ACTION (§16.9), concatenation and a manifest table are not
                // implemented. Advertising the camera's bits would make hosts
                // enable features the gateway does not implement (audit P1-1).
                return Ok(reg::GVCP_CAPABILITY_USER_DEFINED_NAME
                    | reg::GVCP_CAPABILITY_SERIAL_NUMBER
                    | reg::GVCP_CAPABILITY_PACKET_RESEND);
            }
            reg::NUM_MESSAGE_CHANNELS => {
                let enabled = self
                    .message
                    .lock()
                    .map(|m| m.relay.is_some())
                    .unwrap_or(false);
                return Ok(u32::from(enabled));
            }
            reg::MESSAGE_CHANNEL_CAPABILITY => return Ok(0),
            // Mandatory message-channel registers (§36.51§36.54). MCSP is
            // optional (capability bit SP clear) and reads 0; MCCFG is not
            // mapped (capability bit CC clear).
            reg::MESSAGE_CHANNEL_PORT => {
                return Ok(self.message.lock().map(|m| m.port).unwrap_or(0))
            }
            reg::MESSAGE_CHANNEL_DEST_ADDRESS => {
                return Ok(self.message.lock().map(|m| m.dest_address).unwrap_or(0))
            }
            reg::MESSAGE_CHANNEL_TRANSMISSION_TIMEOUT => return Ok(0),
            reg::MESSAGE_CHANNEL_RETRY_COUNT => return Ok(0),
            reg::MESSAGE_CHANNEL_SOURCE_PORT => return Ok(0),
            // §36.27: GVSP is supported (bit 4 clear), GVRSP is not.
            reg::STREAMING_CAPABILITY => return Ok(0),
            // §36.38: protocol field = GVSP (0) plus the deprecated always-1
            // bit (bit 1).
            reg::STREAMING_CONFIGURATION => return Ok(reg::SCONF_DEPRECATED_BIT),
            _ => {}
        }
        // Optional bootstrap registers the virtual device does not implement
        // must answer INVALID_ADDRESS instead of exposing the camera's live
        // configuration: a write to 0x064C would reconfigure the real
        // camera's Persistent IP (destructive) and a read would leak it.
        if matches!(
            addr,
            reg::PERSISTENT_IP_ADDRESS
                | reg::PERSISTENT_SUBNET_MASK
                | reg::PERSISTENT_DEFAULT_GATEWAY
        ) {
            return Err(GEV_STATUS_INVALID_ADDRESS);
        }
        // Locally served bootstrap identity strings.
        if let Some(bytes) = self.identity_bytes(addr, 4) {
            return Ok(u32::from_be_bytes(
                bytes.try_into().expect("4-byte identity window"),
            ));
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
            return self.ccp_write(value, None);
        }
        if addr == reg::MESSAGE_CHANNEL_DEST_ADDRESS || addr == reg::MESSAGE_CHANNEL_PORT {
            // §12.2: the channel opens when MCP carries a non-zero host port
            // (MCDA first); §12.4: MCP = 0 closes it.
            let dest = {
                let mut message = self.message.lock().map_err(|_| crate::GEV_STATUS_ERROR)?;
                if addr == reg::MESSAGE_CHANNEL_DEST_ADDRESS {
                    message.dest_address = value;
                } else {
                    message.port = value;
                }
                let port = reg::mcp_host_port(message.port);
                (port != 0 && message.dest_address != 0).then(|| {
                    SocketAddr::from((Ipv4Addr::from(message.dest_address), port))
                })
            };
            if let Ok(message) = self.message.lock() {
                if let Some(relay) = &message.relay {
                    relay.set_destination(dest);
                }
            }
            return Ok(());
        }
        if addr == reg::MESSAGE_CHANNEL_TRANSMISSION_TIMEOUT
            || addr == reg::MESSAGE_CHANNEL_RETRY_COUNT
        {
            // The relay keeps no event retransmission buffer: ACK generation
            // stays disabled (the mandatory registers read 0, i.e. the same
            // behaviour as a device with MCTT = 0, CR-103cd). See
            // docs/00-governance/OPEN-QUESTIONS.md Q41.
            return Ok(());
        }
        // Optional registers the virtual device does not implement: the
        // camera's Persistent IP must never be written through the relay.
        if matches!(
            addr,
            reg::PERSISTENT_IP_ADDRESS
                | reg::PERSISTENT_SUBNET_MASK
                | reg::PERSISTENT_DEFAULT_GATEWAY
        ) {
            return Err(GEV_STATUS_INVALID_ADDRESS);
        }
        if addr == reg::STREAMING_CONFIGURATION {
            // §36.38 / R-667cd: write access is disabled while any stream
            // channel is open. The virtual device supports GVSP only, so a
            // GVRSP or reserved mode is an invalid parameter.
            if self.channel.is_open() {
                return Err(GEV_STATUS_BUSY);
            }
            let mode =
                (value & reg::SCONF_STREAMING_PROTOCOL_MASK) >> reg::SCONF_STREAMING_PROTOCOL_SHIFT;
            return if mode == reg::SCONF_PROTOCOL_GVSP {
                Ok(())
            } else {
                Err(GEV_STATUS_INVALID_PARAMETER)
            };
        }
        if let Some(result) = self.identity_write(addr, &value.to_be_bytes()) {
            return result;
        }
        if matches!(
            addr,
            reg::VERSION
                | reg::DEVICE_MODE
                | reg::NET_IF_CAPABILITY
                | reg::NET_IF_CONFIGURATION
                | reg::CURRENT_SUBNET_MASK
                | reg::NUM_NETWORK_INTERFACES
                | reg::NUM_MESSAGE_CHANNELS
                | reg::MESSAGE_CHANNEL_CAPABILITY
                | reg::MESSAGE_CHANNEL_PORT
                | reg::MESSAGE_CHANNEL_DEST_ADDRESS
                | reg::MESSAGE_CHANNEL_TRANSMISSION_TIMEOUT
                | reg::MESSAGE_CHANNEL_RETRY_COUNT
                | reg::MESSAGE_CHANNEL_SOURCE_PORT
                | reg::STREAMING_CAPABILITY
                | reg::GVCP_CAPABILITY
        ) {
            return Err(GEV_STATUS_WRITE_PROTECT);
        }
        if addr == reg::HEARTBEAT_TIMEOUT {
            self.ccp.lock().expect("vgige ccp poisoned").heartbeat_ms = value.max(500);
            return Ok(());
        }
        if addr == reg::PENDING_TIMEOUT {
            self.pending_timeout_ms.store(value.max(1), Ordering::SeqCst);
            return Ok(());
        }
        self.back_write_reg(addr, value)
    }

    /// Peer-aware write used by the GVCP server. CCP ownership is tied to
    /// the requesting host; other privileged state remains peer-neutral.
    pub fn write_reg_from(
        &self,
        peer: SocketAddr,
        addr: u32,
        value: u32,
    ) -> Result<(), u16> {
        if addr == reg::CCP {
            return self.ccp_write(value, Some(peer));
        }
        self.authorize(peer, true)?;
        self.write_reg(addr, value)
    }

    /// Whether one 32-bit word is served by the virtual device itself
    /// rather than forwarded to the downstream camera. Mirrors the local
    /// dispatch in [`RegisterSpace::read_reg`]/[`RegisterSpace::write_reg`]
    /// and lets a multi-word READMEM/WRITEMEM compose virtual register
    /// words locally instead of routing the whole block to the camera.
    fn is_virtual_reg(&self, addr: u32) -> bool {
        if reg::stream_channel_addr(addr).is_some() {
            return true;
        }
        if addr >= CUSTOM_BASE && addr < CUSTOM_BASE + (CUSTOM_SLOTS as u32) * 4 {
            return true;
        }
        if addr >= XML_BASE_V && addr < self.xml.base() + self.xml.blob().len() as u32 {
            return true;
        }
        if matches!(
            addr,
            reg::PERSISTENT_IP_ADDRESS
                | reg::PERSISTENT_SUBNET_MASK
                | reg::PERSISTENT_DEFAULT_GATEWAY
        ) {
            return true;
        }
        if matches!(
            addr,
            reg::VERSION
                | reg::DEVICE_MODE
                | reg::NET_IF_CAPABILITY
                | reg::NET_IF_CONFIGURATION
                | reg::CURRENT_SUBNET_MASK
                | reg::NUM_NETWORK_INTERFACES
                | reg::CCP
                | reg::HEARTBEAT_TIMEOUT
                | reg::NUM_STREAM_CHANNELS
                | reg::NUM_MESSAGE_CHANNELS
                | reg::MESSAGE_CHANNEL_CAPABILITY
                | reg::MESSAGE_CHANNEL_PORT
                | reg::MESSAGE_CHANNEL_DEST_ADDRESS
                | reg::MESSAGE_CHANNEL_TRANSMISSION_TIMEOUT
                | reg::MESSAGE_CHANNEL_RETRY_COUNT
                | reg::MESSAGE_CHANNEL_SOURCE_PORT
                | reg::STREAMING_CAPABILITY
                | reg::STREAMING_CONFIGURATION
                | reg::DEVICE_MAC_HIGH
                | reg::DEVICE_MAC_LOW
                | reg::CURRENT_IP
                | reg::GVCP_CAPABILITY
        ) {
            return true;
        }
        if addr >= reg::FIRST_URL && addr < reg::SECOND_URL {
            return true;
        }
        self.identity_bytes(addr, 4).is_some()
    }

    /// PACKETRESEND (§16.7.2): route the request to the virtual stream's
    /// front session (the relay owns the only channel, index 0).
    pub fn packet_resend(&self, channel: u16, block_id: u64, first: u32, last: u32) {
        self.channel.packet_resend(channel, block_id, first, last);
    }

    /// Block read (`count` bytes, a multiple of 4); Err is a GEV status code.
    pub fn read_block(&self, addr: u32, count: u16) -> Result<Vec<u8>, u16> {
        let len = u32::from(count);
        let end = addr.checked_add(len).ok_or(GEV_STATUS_INVALID_ADDRESS)?;
        // Persistent IP is not implemented on the virtual device (§4.2:
        // optional): never leak or forward the camera's configuration.
        if matches!(
            addr,
            reg::PERSISTENT_IP_ADDRESS
                | reg::PERSISTENT_SUBNET_MASK
                | reg::PERSISTENT_DEFAULT_GATEWAY
        ) {
            return Err(GEV_STATUS_INVALID_ADDRESS);
        }
        // Locally served bootstrap identity strings: the bytes are the
        // string's own, so no backend byte-order conversion applies.
        if let Some(bytes) = self.identity_bytes(addr, count) {
            return Ok(bytes);
        }
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
                reg::VERSION
                    | reg::DEVICE_MODE
                    | reg::NET_IF_CAPABILITY
                    | reg::NET_IF_CONFIGURATION
                    | reg::CURRENT_SUBNET_MASK
                    | reg::NUM_NETWORK_INTERFACES
                    | reg::CCP
                    | reg::HEARTBEAT_TIMEOUT
                    | reg::NUM_STREAM_CHANNELS
                    | reg::NUM_MESSAGE_CHANNELS
                    | reg::MESSAGE_CHANNEL_CAPABILITY
                    | reg::MESSAGE_CHANNEL_PORT
                    | reg::MESSAGE_CHANNEL_DEST_ADDRESS
                    | reg::MESSAGE_CHANNEL_TRANSMISSION_TIMEOUT
                    | reg::MESSAGE_CHANNEL_RETRY_COUNT
                    | reg::MESSAGE_CHANNEL_SOURCE_PORT
                    | reg::STREAMING_CAPABILITY
                    | reg::STREAMING_CONFIGURATION
                    | reg::DEVICE_MAC_HIGH
                    | reg::DEVICE_MAC_LOW
                    | reg::CURRENT_IP
                    | reg::GVCP_CAPABILITY
            );
        if count == 4 && virtual_reg {
            return self.read_reg(addr).map(|value| value.to_be_bytes().to_vec());
        }
        // A larger READMEM may legally span contiguous virtual registers
        // (GigE Vision §16.5 only requires 32-bit alignment). If any word of
        // the range is served locally, compose the block word by word:
        // virtual words through read_reg, the rest through the camera's
        // register read, encoded big-endian like any READMEM word.
        let mut pos = addr;
        let mut any_virtual = false;
        while pos < end {
            if self.is_virtual_reg(pos) {
                any_virtual = true;
                break;
            }
            pos += 4;
        }
        if any_virtual {
            let mut out = Vec::with_capacity(count as usize);
            let mut pos = addr;
            while pos < end {
                let value = if self.is_virtual_reg(pos) {
                    self.read_reg(pos)?
                } else {
                    self.back_read_reg(pos)?
                };
                out.extend_from_slice(&value.to_be_bytes());
                pos += 4;
            }
            return Ok(out);
        }
        // Pass through. Ranges overlapping the virtualized slots above
        // (First URL / custom window / blob) but not contained in them are
        // unusual; the camera's answer for the range is forwarded.
        self.back_read_block(addr, count)
    }

    /// Block write (`data.len()` bytes, a multiple of 4); Err is a GEV status code.
    fn write_block_progress(&self, addr: u32, data: &[u8]) -> Result<(), (u16, u32)> {
        if data.is_empty() || data.len() % 4 != 0 {
            return Err((GEV_STATUS_BAD_ALIGNMENT, 0));
        }
        let len = data.len() as u32;
        let end = addr.checked_add(len).ok_or((GEV_STATUS_INVALID_ADDRESS, 0))?;
        // Persistent IP registers are not implemented (see read_reg).
        if matches!(
            addr,
            reg::PERSISTENT_IP_ADDRESS
                | reg::PERSISTENT_SUBNET_MASK
                | reg::PERSISTENT_DEFAULT_GATEWAY
        ) {
            return Err((GEV_STATUS_INVALID_ADDRESS, 0));
        }
        // Locally served bootstrap identity strings: USER_DEFINED_NAME is
        // writable, the other identity strings answer WRITE_PROTECT.
        if let Some(result) = self.identity_write(addr, data) {
            return result.map_err(|status| (status, 0));
        }
        if addr >= reg::FIRST_URL && end <= reg::SECOND_URL {
            return Err((GEV_STATUS_WRITE_PROTECT, 0));
        }
        if addr >= CUSTOM_BASE && end <= CUSTOM_BASE + (CUSTOM_SLOTS as u32) * 4 {
            for (index, chunk) in data.chunks_exact(4).enumerate() {
                let slot = ((addr - CUSTOM_BASE) / 4) as usize + index;
                self.slot_write(slot, u32::from_be_bytes(chunk.try_into().expect("u32")))
                    .map_err(|status| (status, (index * 4) as u32))?;
            }
            return Ok(());
        }
        if addr >= XML_BASE_V {
            return Err((GEV_STATUS_WRITE_PROTECT, 0));
        }
        let virtual_reg = reg::stream_channel_addr(addr).is_some()
            || matches!(
                addr,
                reg::VERSION
                    | reg::DEVICE_MODE
                    | reg::NET_IF_CAPABILITY
                    | reg::NET_IF_CONFIGURATION
                    | reg::CURRENT_SUBNET_MASK
                    | reg::NUM_NETWORK_INTERFACES
                    | reg::CCP
                    | reg::HEARTBEAT_TIMEOUT
                    | reg::NUM_STREAM_CHANNELS
                    | reg::NUM_MESSAGE_CHANNELS
                    | reg::MESSAGE_CHANNEL_CAPABILITY
                    | reg::MESSAGE_CHANNEL_PORT
                    | reg::MESSAGE_CHANNEL_DEST_ADDRESS
                    | reg::MESSAGE_CHANNEL_TRANSMISSION_TIMEOUT
                    | reg::MESSAGE_CHANNEL_RETRY_COUNT
                    | reg::MESSAGE_CHANNEL_SOURCE_PORT
                    | reg::STREAMING_CAPABILITY
                    | reg::STREAMING_CONFIGURATION
                    | reg::DEVICE_MAC_HIGH
                    | reg::DEVICE_MAC_LOW
                    | reg::CURRENT_IP
                    | reg::GVCP_CAPABILITY
            );
        if data.len() == 4 && virtual_reg {
            return self
                .write_reg(addr, u32::from_be_bytes(data[..4].try_into().expect("u32")))
                .map_err(|status| (status, 0));
        }
        // Multi-word WRITEMEM spanning virtual registers: handle each word
        // through the local/forwarded register dispatch (see read_block).
        let mut pos = addr;
        let mut any_virtual = false;
        while pos < end {
            if self.is_virtual_reg(pos) {
                any_virtual = true;
                break;
            }
            pos += 4;
        }
        if any_virtual {
            let mut pos = addr;
            let mut offset = 0usize;
            while pos < end {
                let word = u32::from_be_bytes(
                    data[offset..offset + 4].try_into().expect("4-byte word"),
                );
                if self.is_virtual_reg(pos) {
                    self.write_reg(pos, word)
                        .map_err(|status| (status, offset as u32))?;
                } else {
                    self.back_write_reg(pos, word)
                        .map_err(|status| (status, offset as u32))?;
                }
                pos += 4;
                offset += 4;
            }
            return Ok(());
        }
        let mut owned;
        let payload = if self.back_endian == RegisterEndian::Little {
            owned = data.to_vec();
            swap_register_endian(&mut owned);
            owned.as_slice()
        } else {
            data
        };
        self.back
            .lock()
            .map_err(|_| (crate::GEV_STATUS_ERROR, 0))?
            .write_block(addr, payload)
            .map_err(|status| (status, 0))
    }

    /// Block write (`data.len()` bytes, a multiple of 4); Err is a GEV status code.
    pub fn write_block(&self, addr: u32, data: &[u8]) -> Result<(), u16> {
        self.write_block_progress(addr, data).map_err(|(status, _)| status)
    }
}

#[cfg(test)]
mod ccp_tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn ccp_peer_owns_the_grant_until_release() {
        let mut c = CcpState::default();
        let p1 = SocketAddr::from(([127, 0, 0, 1], 40001));
        let p2 = SocketAddr::from(([127, 0, 0, 1], 40002));

        let (r, close) = c.update(reg::CCP_EXCLUSIVE_ACCESS, Some(p1));
        assert_eq!(r, Ok(()));
        assert!(!close);
        assert_eq!(c.peer, Some(p1));

        let (r, _) = c.update(reg::CCP_EXCLUSIVE_ACCESS, Some(p2));
        assert_eq!(r, Err(GEV_STATUS_ACCESS_DENIED));

        let (r, _) = c.update(reg::CCP_EXCLUSIVE_ACCESS, Some(p1));
        assert_eq!(r, Ok(()));

        // R-070cd: the primary may switch to control access in place.
        let (r, _) = c.update(reg::CCP_CONTROL_ACCESS, Some(p1));
        assert_eq!(r, Ok(()));
        assert_eq!(c.flags, reg::CCP_CONTROL_ACCESS);

        let (r, _) = c.update(0, Some(p2));
        assert_eq!(r, Err(GEV_STATUS_ACCESS_DENIED));

        let (r, close) = c.update(0, Some(p1));
        assert_eq!(r, Ok(()));
        assert!(close);
        assert_eq!(c.flags, 0);
        assert_eq!(c.peer, None);
    }

    #[test]
    fn stale_heartbeat_releases_the_peer() {
        let mut c = CcpState::default();
        let p = SocketAddr::from(([127, 0, 0, 1], 40003));
        let _ = c.update(reg::CCP_CONTROL_ACCESS, Some(p));
        c.last_contact = Some(Instant::now() - Duration::from_secs(10));
        assert!(c.expire_stale_grant());
        assert_eq!(c.flags, 0);
        assert_eq!(c.peer, None);
    }
}
