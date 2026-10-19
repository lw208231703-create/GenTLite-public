//! Simulated camera register/memory model behind `gentl_gvcp::DeviceHandler`.
//!
//! Layout (all multi-byte values big-endian on the wire, spec §15.3):
//! - bootstrap registers of interface #0 per Table 36-1 (0x0000..0x0FFF;
//!   only the implemented slots are mapped, gaps answer
//!   GEV_STATUS_INVALID_ADDRESS)
//! - user register area at `USER_BASE` (Width/Height/PixelFormat/ExposureTime)
//! - GenICam XML blob at `XML_BASE` (read-only, 32-bit aligned per R-119cd)
//!
//! Simplifications vs a real device (M1 scope, documented):
//! - No peer identity: the GVCP `DeviceHandler` trait carries no (ip, port)
//!   context, so the CCP state machine cannot distinguish the primary
//!   application from a stranger (notes/OPEN-QUESTIONS.md Q7). Writing 0
//!   always releases, and a repeated identical request while occupied is
//!   accepted (R-069cd applied to any writer).
//! - Reads are never blocked while a peer holds exclusive access (the
//!   monitor restrictions of spec §10.1 are not enforced).
//!
//! M2: writing a non-zero host port to SCP0 opens the fake-image GVSP
//! stream channel (a [`crate::emitter::StreamScheduler`] stripe targets
//! SCDA0); writing 0 closes it. The scheduler owns no register-map locks,
//! so stopping it can never deadlock against reads or writes here.
//!
//! M3-4a: the same open/close hook serves every stream channel: channel x's
//! registers live at 0x0D00 + 0x40*x (SCPx/SCPSx/SCPDx/SCDAx, spec
//! §36.57–§36.60) and all open channels are served by the camera's single
//! frame-striped scheduler thread (whole frames round-robined, so
//! cross-channel blocks never overlap — the multi-stream area-scan model).
//! The register map models `StreamConfig::channels` channels; accesses
//! past that count answer GEV_STATUS_INVALID_ADDRESS (§36.57: channels
//! 1..511 are optional, so that answer is how an application probes the
//! count).

use std::collections::BTreeSet;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use gentl_gvcp::registers as reg;
use gentl_gvcp::wire::*;
use gentl_gvcp::DeviceHandler;

use crate::emitter::{ChannelSink, StreamParams, StreamScheduler, DEFAULT_PAYLOAD};
use crate::{SimConfig, StreamConfig, StreamingProtocol};

// Generic error (spec Appendix B.21); gentl-gvcp's wire module does not
// export it. Returned when the stream channel's UDP socket cannot be
// created.
const GEV_STATUS_ERROR: u16 = 0x8FFF;

/// Base of the custom user register area.
pub const USER_BASE: u32 = 0x0001_0000;
/// Width register (pixels, RW).
pub const REG_WIDTH: u32 = USER_BASE;
/// Height register (pixels, RW).
pub const REG_HEIGHT: u32 = USER_BASE + 4;
/// PixelFormat register (PFNC value, RW).
pub const REG_PIXEL_FORMAT: u32 = USER_BASE + 8;
/// ExposureTime register (IEEE754 binary32 BE, microseconds, RW).
pub const REG_EXPOSURE_TIME: u32 = USER_BASE + 12;
const USER_LEN: u32 = 16;

/// Base address of the embedded GenICam XML blob.
pub const XML_BASE: u32 = 0x0010_0000;

// OPEN-QUESTION Q1: the sim claims GigE Vision 2.2 compatibility, matching
// common 3.0-era cameras. Spec §36.1 assigns 0x00030000 to spec-3.0 devices.
const VERSION_VALUE: u32 = 0x0002_0002;

// Device Mode (1.2 §27.2 / 3.0 §36.2, MSB-first print): print bit 0 is the
// endianness bit — deprecated since 2.0 and MUST be set to 1 for backward
// compatibility (3.0 §36.2: "must be set to 1"); device_class = 0
// (Transmitter), single link, character set index = 1 (UTF-8) in the low
// byte. Aravis sets the same top bit (DEVICE_MODE_BIG_ENDIAN = 1<<31).
const DEVICE_MODE_VALUE: u32 = 0x8000_0001;

// Net If Configuration writable bits (spec §36.6): all three configuration
// flags (persistent / DHCP / LLA) are writable; the sim stores them, but
// the DHCP bit has no behavioral effect (the sim runs no DHCP client).
const NET_IF_CFG_WRITABLE: u32 =
    reg::IP_CFG_PERSISTENT_IP | reg::IP_CFG_DHCP | reg::IP_CFG_LLA;

// GVCP Capability (1.2 §27.28, MSB-first print): user-defined-name and
// serial-number registers are implemented, and PACKETRESEND (§16.7) is
// served from the per-channel last-block cache. Everything else the
// table offers (heartbeat disable, manifest table, test data, write
// memory, concatenation, events, ...) is genuinely unsupported in the
// sim and stays clear.
const GVCP_CAPABILITY_VALUE: u32 =
    0x8000_0000 | 0x4000_0000 | reg::GVCP_CAPABILITY_PACKET_RESEND;

// Timestamp tick frequency (spec §36.31): device-specific rate chosen as
// 1 MHz (sim is not an IEEE 1588 device); the value registers count
// microseconds since the camera was created.
const TIMESTAMP_TICK_FREQ: u64 = 1_000_000;

const HEARTBEAT_DEFAULT_MS: u32 = 3000;
const HEARTBEAT_MIN_MS: u32 = 500;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Access {
    Ro,
    Rw,
}

use Access::{Ro, Rw};

const fn br(addr: u32, len: u32, access: Access) -> BootReg {
    BootReg { addr, len, access }
}

/// One mapped bootstrap slot of interface #0 (Table 36-1).
struct BootReg {
    addr: u32,
    len: u32,
    access: Access,
}

/// Implemented bootstrap registers of interface #0. Only these slots are
/// mapped; every other address in 0x0000..0x0FFF is reserved and answers
/// GEV_STATUS_INVALID_ADDRESS. The stream channel registers SCP0/SCPS0/
/// SCPD0/SCDA0 are intentionally absent: they (and their channel 1..
/// counterparts) are served by the per-channel map, see [`Inner::channels`].
const BOOT_REGS: &[BootReg] = &[
    br(reg::VERSION, 4, Ro),
    br(reg::DEVICE_MODE, 4, Ro),
    br(reg::DEVICE_MAC_HIGH, 4, Ro),
    br(reg::DEVICE_MAC_LOW, 4, Ro),
    br(reg::NET_IF_CAPABILITY, 4, Ro),
    br(reg::NET_IF_CONFIGURATION, 4, Rw),
    br(reg::CURRENT_IP, 4, Ro),
    br(reg::CURRENT_SUBNET_MASK, 4, Ro),
    br(reg::CURRENT_DEFAULT_GATEWAY, 4, Ro),
    br(reg::MANUFACTURER_NAME, 32, Ro),
    br(reg::MODEL_NAME, 32, Ro),
    br(reg::DEVICE_VERSION, 32, Ro),
    br(reg::MANUFACTURER_INFO, 48, Ro),
    br(reg::SERIAL_NUMBER, 16, Ro),
    br(reg::USER_DEFINED_NAME, 16, Rw),
    br(reg::FIRST_URL, 512, Ro),
    br(reg::SECOND_URL, 512, Ro),
    br(reg::NUM_NETWORK_INTERFACES, 4, Ro),
    br(reg::NUM_MESSAGE_CHANNELS, 4, Ro),
    br(reg::NUM_STREAM_CHANNELS, 4, Ro),
    br(reg::STREAMING_CAPABILITY, 4, Ro),
    br(reg::MESSAGE_CHANNEL_CAPABILITY, 4, Ro),
    br(reg::GVCP_CAPABILITY, 4, Ro),
    br(reg::HEARTBEAT_TIMEOUT, 4, Rw),
    br(reg::TIMESTAMP_TICK_FREQ_HIGH, 4, Ro),
    br(reg::TIMESTAMP_TICK_FREQ_LOW, 4, Ro),
    br(reg::TIMESTAMP_VALUE_HIGH, 4, Ro),
    br(reg::TIMESTAMP_VALUE_LOW, 4, Ro),
    br(reg::CCP, 4, Rw),
    br(reg::STREAMING_CONFIGURATION, 4, Rw),
];

const BOOT_LEN: u32 = 0x1000;

/// Word index of the SCPx value inside a channel's register block.
const W_SCP: usize = 0;
/// Word index of the SCPSx value inside a channel's register block.
const W_SCPS: usize = 1;
/// Word index of the SCPDx value inside a channel's register block.
const W_SCPD: usize = 2;
/// Word index of the SCDAx value inside a channel's register block.
const W_SCDA: usize = 3;

/// Map a stream channel register offset (spec §36.57–§36.60) to its word
/// index inside the channel's register block. Only called with offsets
/// validated by [`reg::stream_channel_addr`].
fn channel_word_index(off: u32) -> usize {
    match off {
        reg::SCP_OFFSET => W_SCP,
        reg::SCPS_OFFSET => W_SCPS,
        reg::SCPD_OFFSET => W_SCPD,
        _ => W_SCDA,
    }
}

/// CCP (0x0A00) state per spec §10.1/§36.46. The switchover key field reads
/// as 0 and CSE is not granted (GVCP Capability bit 10 = 0).
#[derive(Default)]
struct CcpState {
    /// Granted privilege: reg::CCP_EXCLUSIVE_ACCESS, reg::CCP_CONTROL_ACCESS, or 0.
    flags: u32,
    /// CCP bits 16-19: incremented on every channel open, never 0 again
    /// after the first connection (saturating at 15).
    connection_count: u32,
}

/// Everything mutable; the map is reachable only through `&self`.
struct Inner {
    /// Backing bytes for 0x0000..0x0FFF; only slots listed in `BOOT_REGS`
    /// are ever touched.
    boot: Vec<u8>,
    /// Backing bytes for the user register area.
    user: [u8; USER_LEN as usize],
    ccp: CcpState,
    heartbeat_ms: u32,
    ts_epoch: Instant,
    /// Stream channel registers per channel (spec §36.57–§36.60), indexed
    /// by channel number: [SCPx, SCPSx, SCPDx, SCDAx]. The vec holds
    /// `StreamConfig::channels` entries; channels past that count are
    /// optional in the spec and answer GEV_STATUS_INVALID_ADDRESS.
    channels: Vec<[u32; 4]>,
}

/// Register/memory model of one simulated camera.
pub struct SimRegMap {
    config: SimConfig,
    stream: StreamConfig,
    xml: Vec<u8>,
    inner: Mutex<Inner>,
    /// Whole-frame striped transmitter over the open stream channels; the
    /// channel set mirrors the open (non-zero host_port) SCPx registers.
    scheduler: StreamScheduler,
}

impl SimRegMap {
    pub fn new(config: SimConfig, xml: &[u8], stream: StreamConfig) -> Self {
        let n_channels = usize::from(stream.channels.max(1));
        let mut m = SimRegMap {
            config,
            stream,
            xml: xml.to_vec(),
            inner: Mutex::new(Inner {
                boot: vec![0u8; BOOT_LEN as usize],
                user: [0u8; USER_LEN as usize],
                ccp: CcpState::default(),
                heartbeat_ms: HEARTBEAT_DEFAULT_MS,
                ts_epoch: Instant::now(),
                channels: vec![[0; 4]; n_channels],
            }),
            scheduler: StreamScheduler::new(),
        };
        m.init_bootstrap();
        m
    }

    fn w32(boot: &mut [u8], addr: u32, v: u32) {
        boot[addr as usize..addr as usize + 4].copy_from_slice(&v.to_be_bytes());
    }

    fn wstr(boot: &mut [u8], addr: u32, s: &str) {
        let bytes = s.as_bytes();
        let n = bytes.len().min(BOOT_LEN as usize - addr as usize);
        boot[addr as usize..addr as usize + n].copy_from_slice(&bytes[..n]);
    }

    fn init_bootstrap(&mut self) {
        let mut i = self.inner.lock().expect("sim regmap poisoned");
        let boot = &mut i.boot;
        let cfg = &self.config;
        Self::w32(boot, reg::VERSION, VERSION_VALUE);
        Self::w32(boot, reg::DEVICE_MODE, DEVICE_MODE_VALUE);
        Self::w32(
            boot,
            reg::DEVICE_MAC_HIGH,
            u16::from_be_bytes([cfg.mac[0], cfg.mac[1]]) as u32,
        );
        Self::w32(
            boot,
            reg::DEVICE_MAC_LOW,
            u32::from_be_bytes([cfg.mac[2], cfg.mac[3], cfg.mac[4], cfg.mac[5]]),
        );
        Self::w32(boot, reg::NET_IF_CAPABILITY, reg::IP_CFG_PERSISTENT_IP | reg::IP_CFG_LLA);
        Self::w32(boot, reg::NET_IF_CONFIGURATION, reg::IP_CFG_PERSISTENT_IP);
        Self::w32(boot, reg::CURRENT_IP, u32::from(cfg.ip));
        Self::w32(boot, reg::CURRENT_SUBNET_MASK, u32::from(cfg.subnet_mask));
        Self::w32(boot, reg::CURRENT_DEFAULT_GATEWAY, u32::from(cfg.gateway));
        Self::wstr(boot, reg::MANUFACTURER_NAME, &cfg.manufacturer_name);
        Self::wstr(boot, reg::MODEL_NAME, &cfg.model_name);
        Self::wstr(boot, reg::DEVICE_VERSION, &cfg.device_version);
        Self::wstr(boot, reg::MANUFACTURER_INFO, "GenTLite simulated camera");
        Self::wstr(boot, reg::SERIAL_NUMBER, &cfg.serial_number);
        // USER_DEFINED_NAME stays all zero (factory default).
        let url = format!("local:simcam.xml;{XML_BASE:x};{:x}", self.xml.len());
        Self::wstr(boot, reg::FIRST_URL, &url);
        // SECOND_URL stays empty (R-445cd: register present, may be unused).
        Self::w32(boot, reg::NUM_NETWORK_INTERFACES, 1);
        Self::w32(boot, reg::NUM_MESSAGE_CHANNELS, 0);
        // Streaming Configuration factory default follows the supported
        // modes (§36.38: GVSP if supported, GVRSP-only otherwise). Bit 1 is
        // the deprecated always-1 bit. A GVRSP-only device has no GVSP
        // stream channels (its SCPx registers answer ACCESS_DENIED per
        // CR-671cd).
        let (capability, mode_default, n_sc) = match cfg.streaming {
            StreamingProtocol::GvspOnly => (
                0,
                reg::SCONF_PROTOCOL_GVSP << reg::SCONF_STREAMING_PROTOCOL_SHIFT,
                self.stream.channels.max(1),
            ),
            StreamingProtocol::Both => (
                reg::SCAP_GVRSP_MODE_SUPPORTED | reg::SCAP_SCEBA_SUPPORTED,
                reg::SCONF_PROTOCOL_GVSP << reg::SCONF_STREAMING_PROTOCOL_SHIFT,
                self.stream.channels.max(1),
            ),
            StreamingProtocol::GvrspOnly => (
                reg::SCAP_GVSP_MODE_NOT_SUPPORTED
                    | reg::SCAP_GVRSP_MODE_SUPPORTED
                    | reg::SCAP_SCEBA_SUPPORTED,
                reg::SCONF_PROTOCOL_GVRSP << reg::SCONF_STREAMING_PROTOCOL_SHIFT,
                0,
            ),
        };
        Self::w32(boot, reg::NUM_STREAM_CHANNELS, u32::from(n_sc));
        Self::w32(boot, reg::STREAMING_CAPABILITY, capability);
        Self::w32(
            boot,
            reg::STREAMING_CONFIGURATION,
            reg::SCONF_DEPRECATED_BIT | mode_default,
        );
        Self::w32(boot, reg::GVCP_CAPABILITY, GVCP_CAPABILITY_VALUE);
        Self::w32(boot, reg::TIMESTAMP_TICK_FREQ_HIGH, (TIMESTAMP_TICK_FREQ >> 32) as u32);
        Self::w32(boot, reg::TIMESTAMP_TICK_FREQ_LOW, TIMESTAMP_TICK_FREQ as u32);

        // SCPSx factory defaults (spec §36.58: device-specific). Sim
        // convention: the register holds the *total* packet size (bits
        // 16-31) including the IP/UDP/GVSP headers of the configured
        // header form (48-byte budget at 3.0, 36-byte at 1.2 legacy —
        // 1.2 §27.46; see notes/gvcp-wirefacts.md and Q28); 1400 payload +
        // headers keeps the IPv4 total under the 1500-byte MTU. Every
        // stream channel starts with the same default.
        let default_scps =
            reg::scps_make(DEFAULT_PAYLOAD as u32 + crate::emitter::headers_overhead(&self.stream));
        for ch in i.channels.iter_mut() {
            ch[W_SCPS] = default_scps;
        }

        i.user[0..4].copy_from_slice(&self.stream.width.to_be_bytes());
        i.user[4..8].copy_from_slice(&self.stream.height.to_be_bytes());
        i.user[8..12].copy_from_slice(&self.stream.pixel_format.to_be_bytes());
        i.user[12..16].copy_from_slice(&10_000.0f32.to_be_bytes());
    }

    /// Bootstrap slot containing the 4-byte range, if any.
    fn boot_reg(addr: u32) -> Option<&'static BootReg> {
        BOOT_REGS
            .iter()
            .find(|r| addr >= r.addr && addr + 4 <= r.addr + r.len)
    }

    /// Copy `len` bytes starting at `addr` if the whole range is covered by
    /// mapped memory (bootstrap slots, stream channel registers, user
    /// area, or the XML blob).
    fn read_mapped(&self, addr: u32, len: u32) -> Result<Vec<u8>, u16> {
        let end = addr.checked_add(len).ok_or(GEV_STATUS_INVALID_ADDRESS)?;
        let mut out = Vec::with_capacity(len as usize);
        let mut pos = addr;
        while pos < end {
            let take_end;
            if let Some((x, off)) = reg::stream_channel_addr(pos) {
                take_end = pos + 4;
                let i = self.inner.lock().expect("sim regmap poisoned");
                if Self::streaming_mode(&i) == reg::SCONF_PROTOCOL_GVRSP {
                    // CR-671cd: GVSP channel registers are inaccessible in
                    // GVRSP mode (READMEM answers like READREG).
                    return Err(GEV_STATUS_ACCESS_DENIED);
                }
                let Some(words) = i.channels.get(x as usize) else {
                    // §36.57: channels past the device count are optional
                    // and answer GEV_STATUS_INVALID_ADDRESS.
                    return Err(GEV_STATUS_INVALID_ADDRESS);
                };
                out.extend_from_slice(&words[channel_word_index(off)].to_be_bytes());
            } else if pos < BOOT_LEN {
                let Some(r) = Self::boot_reg_at(pos) else {
                    return Err(GEV_STATUS_INVALID_ADDRESS);
                };
                take_end = end.min(r.addr + r.len);
                out.extend_from_slice(&self.inner.lock().expect("sim regmap poisoned").boot
                    [pos as usize..take_end as usize]);
            } else if pos >= USER_BASE && pos < USER_BASE + USER_LEN {
                take_end = end.min(USER_BASE + USER_LEN);
                let i = self.inner.lock().expect("sim regmap poisoned");
                out.extend_from_slice(&i.user[(pos - USER_BASE) as usize..(take_end - USER_BASE) as usize]);
            } else if pos >= XML_BASE && (pos as usize) < XML_BASE as usize + self.xml.len() {
                take_end = end.min(XML_BASE + self.xml.len() as u32);
                out.extend_from_slice(
                    &self.xml[(pos - XML_BASE) as usize..(take_end - XML_BASE) as usize],
                );
            } else {
                return Err(GEV_STATUS_INVALID_ADDRESS);
            }
            if take_end <= pos {
                return Err(GEV_STATUS_INVALID_ADDRESS);
            }
            pos = take_end;
        }
        Ok(out)
    }

    fn boot_reg_at(addr: u32) -> Option<&'static BootReg> {
        BOOT_REGS
            .iter()
            .find(|r| addr >= r.addr && addr < r.addr + r.len)
    }

    /// Current `streaming_protocol` field (§36.38, print bits 4-5) decoded
    /// from the live Streaming Configuration register value.
    fn streaming_mode(i: &Inner) -> u32 {
        let b = &i.boot[reg::STREAMING_CONFIGURATION as usize..reg::STREAMING_CONFIGURATION as usize + 4];
        (u32::from_be_bytes(b.try_into().unwrap()) & reg::SCONF_STREAMING_PROTOCOL_MASK)
            >> 26
    }
}

impl DeviceHandler for SimRegMap {
    fn discovery_info(&self) -> DiscoveryInfo {
        let cfg = &self.config;
        DiscoveryInfo {
            // OPEN-QUESTION Q2: same 2.2 claim as the Version register.
            spec_version_major: 2,
            spec_version_minor: 2,
            device_mode: DEVICE_MODE_VALUE,
            mac: cfg.mac,
            ip_config_options: reg::IP_CFG_PERSISTENT_IP | reg::IP_CFG_LLA,
            ip_config_current: reg::IP_CFG_PERSISTENT_IP,
            current_ip: u32::from(cfg.ip),
            current_subnet_mask: u32::from(cfg.subnet_mask),
            default_gateway: u32::from(cfg.gateway),
            manufacturer_name: cfg.manufacturer_name.clone(),
            model_name: cfg.model_name.clone(),
            device_version: cfg.device_version.clone(),
            manufacturer_info: "GenTLite simulated camera".into(),
            serial_number: cfg.serial_number.clone(),
            user_defined_name: String::new(),
        }
    }

    fn read_reg(&self, addr: u32) -> Result<u32, u16> {
        if addr % 4 != 0 {
            return Err(GEV_STATUS_BAD_ALIGNMENT);
        }
        if let Some((x, off)) = reg::stream_channel_addr(addr) {
            let i = self.inner.lock().expect("sim regmap poisoned");
            if Self::streaming_mode(&i) == reg::SCONF_PROTOCOL_GVRSP {
                // CR-671cd: a device operating in GVRSP mode must prevent
                // access to the SCPx register (ACCESS_DENIED).
                return Err(GEV_STATUS_ACCESS_DENIED);
            }
            let Some(words) = i.channels.get(x as usize) else {
                // §36.57: stream channels 1..511 are optional; a device
                // without channel x answers GEV_STATUS_INVALID_ADDRESS —
                // the application probe for the channel count.
                return Err(GEV_STATUS_INVALID_ADDRESS);
            };
            return Ok(words[channel_word_index(off)]);
        }
        if addr == reg::CCP {
            let i = self.inner.lock().expect("sim regmap poisoned");
            return Ok(i.ccp.flags | ((i.ccp.connection_count & 0xF) << reg::CCP_CONNECTION_COUNT_SHIFT));
        }
        if addr == reg::HEARTBEAT_TIMEOUT {
            return Ok(self.inner.lock().expect("sim regmap poisoned").heartbeat_ms);
        }
        if addr == reg::TIMESTAMP_VALUE_HIGH || addr == reg::TIMESTAMP_VALUE_LOW {
            let i = self.inner.lock().expect("sim regmap poisoned");
            let ts = i.ts_epoch.elapsed().as_micros() as u64;
            let v = if addr == reg::TIMESTAMP_VALUE_HIGH {
                (ts >> 32) as u32
            } else {
                ts as u32
            };
            return Ok(v);
        }
        let bytes: [u8; 4] = self.read_mapped(addr, 4)?.try_into().unwrap();
        Ok(u32::from_be_bytes(bytes))
    }

    fn write_reg(&self, addr: u32, value: u32) -> Result<(), u16> {
        if addr % 4 != 0 {
            return Err(GEV_STATUS_BAD_ALIGNMENT);
        }
        if let Some((x, off)) = reg::stream_channel_addr(addr) {
            return self.write_stream_channel(x, off, value);
        }
        let mut i = self.inner.lock().expect("sim regmap poisoned");
        if addr == reg::CCP {
            // Simplified CCP state machine (no peer identity, see module
            // docs): 0 releases; when open, EA (priority over CA) or CA is
            // granted and the connection counter increments; when occupied,
            // a repeated identical request succeeds (R-069cd) and any other
            // non-zero write is denied.
            if value == 0 {
                i.ccp.flags = 0;
                return Ok(());
            }
            let requested = value & (reg::CCP_EXCLUSIVE_ACCESS | reg::CCP_CONTROL_ACCESS);
            if requested == 0 {
                // No privilege bits: nothing to grant, accept as a no-op.
                return Ok(());
            }
            if i.ccp.flags == 0 {
                i.ccp.flags = if requested & reg::CCP_EXCLUSIVE_ACCESS != 0 {
                    reg::CCP_EXCLUSIVE_ACCESS
                } else {
                    reg::CCP_CONTROL_ACCESS
                };
                i.ccp.connection_count = (i.ccp.connection_count + 1).min(0xF);
                return Ok(());
            }
            return if requested == i.ccp.flags {
                Ok(())
            } else {
                Err(GEV_STATUS_ACCESS_DENIED)
            };
        }
        if addr == reg::HEARTBEAT_TIMEOUT {
            // [O-460cd]: values below 500 ms are coerced to 500 ms and the
            // register reflects the value actually used.
            i.heartbeat_ms = value.max(HEARTBEAT_MIN_MS);
            return Ok(());
        }
        if addr == reg::STREAMING_CONFIGURATION {
            // §36.38 / R-667cd / Table 36-2. Write access is disabled while
            // any GVSP stream channel is open; selecting a protocol the
            // device does not support (or a reserved value 2-3, see
            // notes/OPEN-QUESTIONS.md Q29) returns GEV_STATUS_INVALID_PARAMETER.
            if i.channels.iter().any(|c| reg::scp_host_port(c[W_SCP]) != 0) {
                return Err(GEV_STATUS_BUSY);
            }
            let mode = (value & reg::SCONF_STREAMING_PROTOCOL_MASK) >> reg::SCONF_STREAMING_PROTOCOL_SHIFT;
            let (gvsp_ok, gvrsp_ok) = match self.config.streaming {
                StreamingProtocol::GvspOnly => (true, false),
                StreamingProtocol::Both => (true, true),
                StreamingProtocol::GvrspOnly => (false, true),
            };
            let supported = match mode {
                reg::SCONF_PROTOCOL_GVSP => gvsp_ok,
                reg::SCONF_PROTOCOL_GVRSP => gvrsp_ok,
                // Reserved values 2-3: rejected (notes/OPEN-QUESTIONS.md Q29).
                _ => false,
            };
            if !supported {
                return Err(GEV_STATUS_INVALID_PARAMETER);
            }
            // Store only the protocol field; bit 1 is the deprecated
            // always-1 bit and stays set (§36.38).
            i.boot[addr as usize..addr as usize + 4].copy_from_slice(
                &((value & reg::SCONF_STREAMING_PROTOCOL_MASK) | reg::SCONF_DEPRECATED_BIT)
                    .to_be_bytes(),
            );
            return Ok(());
        }
        if let Some(r) = Self::boot_reg(addr) {
            if r.access != Rw {
                return Err(GEV_STATUS_WRITE_PROTECT);
            }
            if addr == reg::NET_IF_CONFIGURATION {
                i.boot[addr as usize..addr as usize + 4]
                    .copy_from_slice(&(value & NET_IF_CFG_WRITABLE).to_be_bytes());
            } else {
                i.boot[addr as usize..addr as usize + 4].copy_from_slice(&value.to_be_bytes());
            }
            return Ok(());
        }
        if addr >= USER_BASE && addr + 4 <= USER_BASE + USER_LEN {
            i.user[(addr - USER_BASE) as usize..(addr - USER_BASE + 4) as usize]
                .copy_from_slice(&value.to_be_bytes());
            return Ok(());
        }
        Err(GEV_STATUS_INVALID_ADDRESS)
    }

    fn read_mem(&self, addr: u32, count: u16) -> Result<Vec<u8>, u16> {
        self.read_mapped(addr, count as u32)
    }
}

impl SimRegMap {
    /// PACKETRESEND (§16.7.2): serve the request from the channel's
    /// last-block cache (or answer GEV_STATUS_PACKET_UNAVAILABLE when the
    /// block is gone — CR-205st).
    pub fn packet_resend(&self, channel: u16, block_id: u64, first: u32, last: u32) {
        self.scheduler.packet_resend(channel, block_id, first, last);
    }

    /// Test hook (§16.7 regression): start dropping the given payload
    /// packets on send (leader/trailer are never dropped). The set is
    /// shared with the test, which typically inserts (block_id,
    /// packet_id) pairs before starting acquisition.
    pub fn set_packet_drop(&self, drops: Arc<Mutex<BTreeSet<(u64, u32)>>>) {
        self.scheduler.set_packet_drop(Some(drops));
    }

    /// Write one stream channel register (spec §36.57–§36.60). `x` is the
    /// channel number and `off` the register's offset inside the channel's
    /// 0x40 block, both decoded from the address by
    /// [`reg::stream_channel_addr`].
    fn write_stream_channel(&self, x: u32, off: u32, value: u32) -> Result<(), u16> {
        if Self::streaming_mode(&self.inner.lock().expect("sim regmap poisoned"))
            == reg::SCONF_PROTOCOL_GVRSP
        {
            // CR-671cd: no GVSP stream channel register is accessible in
            // GVRSP mode (the SCPx case is named by the requirement; the
            // whole channel block follows the device's mode).
            return Err(GEV_STATUS_ACCESS_DENIED);
        }
        if x >= u32::from(self.stream.channels.max(1)) {
            // §36.57: stream channels 1..511 are optional; accesses past
            // the device count answer GEV_STATUS_INVALID_ADDRESS.
            return Err(GEV_STATUS_INVALID_ADDRESS);
        }
        if off == reg::SCP_OFFSET {
            return self.write_scp(x, value);
        }
        if off == reg::SCPS_OFFSET {
            return self.write_scps(x, value);
        }
        // §36.59: SCPDx is not writable while the channel is streaming —
        // the value is frozen at open (R-586cd rejects in-stream SCPx
        // writes; the delay register follows the same rule). The
        // scheduler lock is taken alone here so the lock order never
        // inverts `write_scp`'s inner->scheduler order.
        if self.scheduler.is_streaming(x as u16) {
            return Err(GEV_STATUS_BUSY);
        }
        let mut i = self.inner.lock().expect("sim regmap poisoned");
        i.channels[x as usize][channel_word_index(off)] = value;
        Ok(())
    }

    /// SCPSx (spec §36.58) write; `value` is the raw register value (fire
    /// = 0x80000000, do_not_fragment = 0x40000000, packet_size = low 16
    /// bits, MSB-first print numbering — Q31).
    ///
    /// Rounding (R-562st): the stored value is what the transmitter will
    /// actually send — clamp to the sim's continuous size domain and floor
    /// the payload to a pixel multiple ([`crate::emitter::round_packet_size`]).
    ///
    /// fire_test_packet (§11.7/§24.5, notes/OPEN-QUESTIONS.md Q33): a
    /// write carrying the fire bit is accepted at any time and fires
    /// exactly one test packet of the rounded size towards (SCDAx, SCPx
    /// host_port); the stored value has the fire bit self-cleared
    /// (§36.58). R-567s places negotiation right after the channel open,
    /// before regular streaming; the sim's auto-stream has no acquisition
    /// gating (Q15) and cannot model "not streaming", so instead of
    /// rejecting test packets during the stream it tolerates them
    /// interleaved. A plain (no-fire) write keeps the §36.58 block while
    /// the channel streams.
    fn write_scps(&self, x: u32, value: u32) -> Result<(), u16> {
        let fire = value & reg::SCPS_FIRE_TEST_PACKET != 0;
        let rounded = crate::emitter::round_packet_size(&self.stream, value);
        if !fire && self.scheduler.is_streaming(x as u16) {
            return Err(GEV_STATUS_BUSY);
        }
        // Stored = the written value without the self-clearing fire bit
        // and with the size field replaced by the rounded size.
        let stored =
            (value & !reg::SCPS_FIRE_TEST_PACKET & !reg::SCPS_SIZE_MASK) | reg::scps_make(rounded);
        {
            let mut i = self.inner.lock().expect("sim regmap poisoned");
            i.channels[x as usize][W_SCPS] = stored;
        }
        self.scheduler.update_packet_size(x as u16, stored);
        if fire {
            // R-093st: an unsupported requested size must not produce a
            // test packet. Post-clamp the size is always transmittable
            // (the sim accepts the whole [49, 65535] domain — its clamp IS
            // the support test), so the fire always emits one packet. A
            // fire on a closed channel is ignored by the scheduler (§11.7:
            // the device can ignore or reject it).
            self.scheduler.send_test_packet(x as u16, rounded);
        }
        Ok(())
    }

    /// SCPx (0x0D00 + 0x40*x, spec §36.57): a non-zero host_port field
    /// (bits 16-31) opens stream channel x (spec §11.5, R-087ca), zero
    /// closes it. The register value follows the spec layout; SCPx must be
    /// the last register written when opening and the first when closing.
    ///
    /// The channel's sink is created before the register value is
    /// published and removed after it is withdrawn, and the scheduler
    /// thread join never runs while a register-map lock is held (the
    /// scheduler itself takes no register-map locks), so opening and
    /// closing cannot deadlock against reads or writes.
    fn write_scp(&self, x: u32, value: u32) -> Result<(), u16> {
        let host_port = reg::scp_host_port(value);
        if value != 0 && host_port == 0 {
            // A non-zero write with the host_port field left at 0 carries no
            // port — e.g. the pre-Q31 "<<16" layout mistake (a bare port
            // shifted out of the low 16 bits). Reject it loudly so a wrong
            // layout fails at the device instead of silently closing the
            // channel (notes/OPEN-QUESTIONS.md Q14/Q31).
            return Err(GEV_STATUS_INVALID_PARAMETER);
        }
        let key = x as u16;
        if host_port != 0 {
            let port = host_port;
            // Snapshot the channel configuration: SCPSx/SCPDx writes are
            // rejected while streaming (spec §36.58/§36.59), so freezing
            // the values at open matches the observable contract.
            let (target, params) = {
                let i = self.inner.lock().expect("sim regmap poisoned");
                if reg::scp_host_port(i.channels[x as usize][W_SCP]) != 0 {
                    // R-586cd: only a zero write (close) is accepted while
                    // the channel is streaming.
                    return Err(GEV_STATUS_BUSY);
                }
                let words = i.channels[x as usize];
                (
                    SocketAddr::from((Ipv4Addr::from(words[W_SCDA]), port)),
                    StreamParams::new(&self.stream, words[W_SCPS], words[W_SCPD]),
                )
            };
            let sink = ChannelSink::open(target, params).map_err(|_| GEV_STATUS_ERROR)?;
            let old = {
                let mut i = self.inner.lock().expect("sim regmap poisoned");
                i.channels[x as usize][W_SCP] = value;
                self.scheduler.insert(key, sink)
            };
            // Drop outside the locks (defensive; the slot was known empty).
            drop(old);
            return Ok(());
        }

        let old = {
            let mut i = self.inner.lock().expect("sim regmap poisoned");
            i.channels[x as usize][W_SCP] = 0;
            self.scheduler.remove(key)
        };
        drop(old);
        Ok(())
    }
}

// The scheduler's lifetime follows the register map (and thereby the GVCP
// server holding it): `StreamScheduler::drop` stops and joins the stripe
// thread, which touches only its own channel map — no register-map locks.

/// Cloneable handle wrapping the shared register map; handed to
/// [`gentl_gvcp::GvcpServer`].
#[derive(Clone)]
pub struct SimDevice {
    map: Arc<SimRegMap>,
}

impl SimDevice {
    pub fn new(map: Arc<SimRegMap>) -> Self {
        Self { map }
    }

    /// Test hook (§16.7 regression): payload packets to drop on send.
    pub fn set_packet_drop(&self, drops: Arc<Mutex<BTreeSet<(u64, u32)>>>) {
        self.map.set_packet_drop(drops);
    }
}

impl DeviceHandler for SimDevice {
    fn discovery_info(&self) -> DiscoveryInfo {
        self.map.discovery_info()
    }
    fn read_reg(&self, addr: u32) -> Result<u32, u16> {
        self.map.read_reg(addr)
    }
    fn write_reg(&self, addr: u32, value: u32) -> Result<(), u16> {
        self.map.write_reg(addr, value)
    }
    fn read_mem(&self, addr: u32, count: u16) -> Result<Vec<u8>, u16> {
        self.map.read_mem(addr, count)
    }
    fn packet_resend(&self, channel: u16, block_id: u64, first: u32, last: u32) {
        self.map.packet_resend(channel, block_id, first, last);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gentl_gvcp::registers as reg;
    use std::net::Ipv4Addr;

    fn test_map() -> SimRegMap {
        SimRegMap::new(
            crate::SimConfig {
                mac: [0x02, 0x00, 0x11, 0x22, 0x33, 0x44],
                ip: Ipv4Addr::new(192, 168, 1, 10),
                subnet_mask: Ipv4Addr::new(255, 255, 255, 0),
                gateway: Ipv4Addr::new(192, 168, 1, 1),
                manufacturer_name: "GenTLite".into(),
                model_name: "SimCam".into(),
                device_version: "1.0.0".into(),
                serial_number: "SIM-0001".into(),
                streaming: StreamingProtocol::GvspOnly,
            },
            b"<?xml version=\"1.0\"?><fake/>",
            crate::StreamConfig::default(),
        )
    }

    #[test]
    fn version_register_reads_2_2() {
        let m = test_map();
        assert_eq!(m.read_reg(reg::VERSION).unwrap(), 0x0002_0002);
    }

    #[test]
    fn identity_registers() {
        let m = test_map();
        assert_eq!(
            m.read_reg(reg::DEVICE_MAC_HIGH).unwrap(),
            0x0000_0200 // reserved + first MAC bytes 02 00
        );
        assert_eq!(
            m.read_reg(reg::DEVICE_MAC_LOW).unwrap(),
            0x1122_3344
        );
        assert_eq!(m.read_reg(reg::CURRENT_IP).unwrap(), 0xC0A8_010A);
        let name = m.read_mem(reg::MANUFACTURER_NAME, 32).unwrap();
        assert_eq!(&name[..8], b"GenTLite");
        let serial = m.read_mem(reg::SERIAL_NUMBER, 16).unwrap();
        assert_eq!(&serial[..8], b"SIM-0001");
    }

    #[test]
    fn ccp_grant_refresh_deny_release() {
        let m = test_map();
        // Unoccupied reads as 0.
        assert_eq!(m.read_reg(reg::CCP).unwrap(), 0);
        // Grant exclusive.
        m.write_reg(reg::CCP, reg::CCP_EXCLUSIVE_ACCESS).unwrap();
        assert_eq!(
            m.read_reg(reg::CCP).unwrap(),
            reg::CCP_EXCLUSIVE_ACCESS | (1 << reg::CCP_CONNECTION_COUNT_SHIFT)
        );
        // R-069cd: repeated identical request is accepted, no new connection.
        m.write_reg(reg::CCP, reg::CCP_EXCLUSIVE_ACCESS).unwrap();
        assert_eq!(
            m.read_reg(reg::CCP).unwrap(),
            reg::CCP_EXCLUSIVE_ACCESS | (1 << reg::CCP_CONNECTION_COUNT_SHIFT)
        );
        // Different privilege while occupied: denied.
        assert_eq!(
            m.write_reg(reg::CCP, reg::CCP_CONTROL_ACCESS),
            Err(GEV_STATUS_ACCESS_DENIED)
        );
        // Release, then control access grants and bumps the counter. The
        // connection count never returns to zero (spec §36.46).
        m.write_reg(reg::CCP, 0).unwrap();
        assert_eq!(
            m.read_reg(reg::CCP).unwrap(),
            1 << reg::CCP_CONNECTION_COUNT_SHIFT
        );
        m.write_reg(reg::CCP, reg::CCP_CONTROL_ACCESS).unwrap();
        assert_eq!(
            m.read_reg(reg::CCP).unwrap(),
            reg::CCP_CONTROL_ACCESS | (2 << reg::CCP_CONNECTION_COUNT_SHIFT)
        );
        m.write_reg(reg::CCP, 0).unwrap();
        m.write_reg(reg::CCP, reg::CCP_EXCLUSIVE_ACCESS).unwrap();
        assert_eq!(
            m.read_reg(reg::CCP).unwrap(),
            reg::CCP_EXCLUSIVE_ACCESS | (3 << reg::CCP_CONNECTION_COUNT_SHIFT)
        );
    }

    #[test]
    fn exposure_register_roundtrip_and_default() {
        let m = test_map();
        assert_eq!(
            m.read_reg(REG_EXPOSURE_TIME).unwrap(),
            10_000.0f32.to_bits()
        );
        m.write_reg(REG_EXPOSURE_TIME, 33_000.0f32.to_bits()).unwrap();
        assert_eq!(
            m.read_reg(REG_EXPOSURE_TIME).unwrap(),
            33_000.0f32.to_bits()
        );
    }

    #[test]
    fn user_register_defaults() {
        let m = test_map();
        assert_eq!(m.read_reg(REG_WIDTH).unwrap(), 1024);
        assert_eq!(m.read_reg(REG_HEIGHT).unwrap(), 768);
        assert_eq!(m.read_reg(REG_PIXEL_FORMAT).unwrap(), 0x0108_0001);
    }

    #[test]
    fn stream_config_seeds_user_registers() {
        let m = SimRegMap::new(
            crate::SimConfig {
                mac: [0x02, 0x00, 0x11, 0x22, 0x33, 0x44],
                ip: Ipv4Addr::new(192, 168, 1, 10),
                subnet_mask: Ipv4Addr::new(255, 255, 255, 0),
                gateway: Ipv4Addr::new(192, 168, 1, 1),
                manufacturer_name: "GenTLite".into(),
                model_name: "SimCam".into(),
                device_version: "1.0.0".into(),
                serial_number: "SIM-0001".into(),
                streaming: StreamingProtocol::GvspOnly,
            },
            b"<fake/>",
            crate::StreamConfig {
                width: 640,
                height: 480,
                pixel_format: 0x0110_0007, // Mono16
                channels: 1,
                legacy_headers: false,
            },
        );
        assert_eq!(m.read_reg(REG_WIDTH).unwrap(), 640);
        assert_eq!(m.read_reg(REG_HEIGHT).unwrap(), 480);
        assert_eq!(m.read_reg(REG_PIXEL_FORMAT).unwrap(), 0x0110_0007);
        assert_eq!(
            m.read_reg(REG_EXPOSURE_TIME).unwrap(),
            10_000.0f32.to_bits()
        );
    }

    #[test]
    fn stream_channel_open_busy_close() {
        let m = test_map();
        // Factory state: channel closed, zero packet delay, default SCPS0
        // (total packet size 1400+48 per the spec layout).
        assert_eq!(m.read_reg(reg::SCP0).unwrap(), 0);
        assert_eq!(m.read_reg(reg::SCPD0).unwrap(), 0);
        assert_eq!(
            m.read_reg(reg::SCPS0).unwrap(),
            reg::scps_make(crate::emitter::DEFAULT_PAYLOAD as u32 + reg::GVSP_HEADERS_OVERHEAD)
        );
        m.write_reg(reg::SCPD0, 5).unwrap();
        assert_eq!(m.read_reg(reg::SCPD0).unwrap(), 5);

        // The spec layout keeps host_port in the LOW 16 bits (§36.57,
        // MSB-first print "bits 16-31"): a bare port value opens the
        // channel, while the pre-Q31 "<<16" style leaves the host_port
        // field at zero and is rejected as an invalid no-op write.
        m.write_reg(reg::SCDA0, u32::from(Ipv4Addr::LOCALHOST))
            .unwrap();
        m.write_reg(reg::SCP0, 3956).unwrap();
        assert_eq!(reg::scp_host_port(m.read_reg(reg::SCP0).unwrap()), 3956);
        assert_eq!(
            m.write_reg(reg::SCP0, 3957).unwrap_err(),
            GEV_STATUS_BUSY,
            "R-586cd: a non-zero write is rejected while streaming"
        );
        m.write_reg(reg::SCP0, 0).unwrap();
        assert_eq!(
            m.write_reg(reg::SCP0, 3956 << 16).unwrap_err(),
            GEV_STATUS_INVALID_PARAMETER
        );
        // Open towards loopback (no receiver bound: the emitter's sends
        // fail silently, like on a real camera whose receiver vanished).
        m.write_reg(reg::SCP0, reg::scp_make(3956)).unwrap();
        assert_eq!(reg::scp_host_port(m.read_reg(reg::SCP0).unwrap()), 3956);
        // R-586cd: only a zero write (close) is accepted while streaming.
        assert_eq!(
            m.write_reg(reg::SCP0, reg::scp_make(3957)).unwrap_err(),
            GEV_STATUS_BUSY
        );
        m.write_reg(reg::SCP0, 0).unwrap();
        assert_eq!(m.read_reg(reg::SCP0).unwrap(), 0);
        // Re-open and close; the map's Drop cleans up the last emitter.
        m.write_reg(reg::SCP0, reg::scp_make(3956)).unwrap();
        m.write_reg(reg::SCP0, 0).unwrap();
    }

    #[test]
    fn unmapped_and_alignment_error_codes() {
        let m = test_map();
        // Reserved bootstrap gap.
        assert_eq!(
            m.read_reg(0x0018).unwrap_err(),
            GEV_STATUS_INVALID_ADDRESS
        );
        // Unaligned register access.
        assert_eq!(m.read_reg(0x0002).unwrap_err(), GEV_STATUS_BAD_ALIGNMENT);
        assert_eq!(
            m.write_reg(0x0002, 0).unwrap_err(),
            GEV_STATUS_BAD_ALIGNMENT
        );
        // Read-only registers reject writes.
        assert_eq!(
            m.write_reg(reg::VERSION, 0).unwrap_err(),
            GEV_STATUS_WRITE_PROTECT
        );
        assert_eq!(
            m.write_reg(reg::MANUFACTURER_NAME, 0).unwrap_err(),
            GEV_STATUS_WRITE_PROTECT
        );
        // Just past the user register area.
        assert_eq!(
            m.read_reg(USER_BASE + 16).unwrap_err(),
            GEV_STATUS_INVALID_ADDRESS
        );
        // XML area rejects register writes.
        assert_eq!(
            m.write_reg(XML_BASE, 0).unwrap_err(),
            GEV_STATUS_INVALID_ADDRESS
        );
    }

    #[test]
    fn read_mem_requires_full_coverage() {
        let m = test_map();
        // XML blob is mapped and starts with the XML declaration.
        let head = m.read_mem(XML_BASE, 5).unwrap();
        assert_eq!(head, b"<?xml");
        // A range crossing the gap between bootstrap and user area fails.
        assert_eq!(
            m.read_mem(0x0FF0, 64).unwrap_err(),
            GEV_STATUS_INVALID_ADDRESS
        );
        // Unmapped high address fails.
        assert_eq!(
            m.read_mem(0x0020_0000, 4).unwrap_err(),
            GEV_STATUS_INVALID_ADDRESS
        );
        // String slots are readable via READMEM.
        let xml_len = 28; // b"<?xml version=\"1.0\"?><fake/>"
        let url = m.read_mem(reg::FIRST_URL, 512).unwrap();
        let end = url.iter().position(|&b| b == 0).unwrap();
        let url = std::str::from_utf8(&url[..end]).unwrap();
        assert_eq!(url, format!("local:simcam.xml;100000;{xml_len:x}"));
    }

    #[test]
    fn writable_bootstrap_cells() {
        let m = test_map();
        // Heartbeat default, then coercion to the 500 ms minimum [O-460cd].
        assert_eq!(m.read_reg(reg::HEARTBEAT_TIMEOUT).unwrap(), 3000);
        m.write_reg(reg::HEARTBEAT_TIMEOUT, 100).unwrap();
        assert_eq!(m.read_reg(reg::HEARTBEAT_TIMEOUT).unwrap(), 500);
        m.write_reg(reg::HEARTBEAT_TIMEOUT, 5000).unwrap();
        assert_eq!(m.read_reg(reg::HEARTBEAT_TIMEOUT).unwrap(), 5000);
        // User-defined name (16 bytes, RW) accepts a word write.
        m.write_reg(reg::USER_DEFINED_NAME, u32::from_be_bytes(*b"cam0"))
            .unwrap();
        assert_eq!(
            m.read_mem(reg::USER_DEFINED_NAME, 4).unwrap(),
            b"cam0"
        );
        // Net If Configuration stores every writable configuration flag
        // (persistent / DHCP / LLA); the DHCP bit has no behavior.
        m.write_reg(reg::NET_IF_CONFIGURATION, 0xFFFFFFFF).unwrap();
        assert_eq!(
            m.read_reg(reg::NET_IF_CONFIGURATION).unwrap(),
            reg::IP_CFG_PERSISTENT_IP | reg::IP_CFG_DHCP | reg::IP_CFG_LLA
        );
        // Timestamp registers tick at the advertised 1 MHz.
        let lo1 = m.read_reg(reg::TIMESTAMP_VALUE_LOW).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let lo2 = m.read_reg(reg::TIMESTAMP_VALUE_LOW).unwrap();
        assert!(lo2 > lo1);
        assert_eq!(
            m.read_reg(reg::TIMESTAMP_TICK_FREQ_LOW).unwrap(),
            1_000_000
        );
    }

    #[test]
    fn discovery_info_matches_config() {
        let m = test_map();
        let d = m.discovery_info();
        assert_eq!(d.spec_version_major, 2);
        assert_eq!(d.spec_version_minor, 2);
        assert_eq!(d.mac, [0x02, 0x00, 0x11, 0x22, 0x33, 0x44]);
        assert_eq!(d.current_ip, 0xC0A8_010A);
        assert_eq!(d.manufacturer_name, "GenTLite");
        assert_eq!(d.serial_number, "SIM-0001");
    }

    fn map_with_streaming(mode: StreamingProtocol) -> SimRegMap {
        SimRegMap::new(
            crate::SimConfig {
                mac: [0x02, 0x00, 0x11, 0x22, 0x33, 0x44],
                ip: Ipv4Addr::new(192, 168, 1, 10),
                subnet_mask: Ipv4Addr::new(255, 255, 255, 0),
                gateway: Ipv4Addr::new(192, 168, 1, 1),
                manufacturer_name: "GenTLite".into(),
                model_name: "SimCam".into(),
                device_version: "1.0.0".into(),
                serial_number: "SIM-0001".into(),
                streaming: mode,
            },
            b"<?xml version=\"1.0\"?><fake/>",
            crate::StreamConfig::default(),
        )
    }

    /// Streaming Configuration write semantics: Table 36-2 mode/capability
    /// validation, R-667cd BUSY while a channel is open, and CR-671cd
    /// ACCESS_DENIED on GVSP channel registers in GVRSP mode.
    #[test]
    fn streaming_configuration_semantics() {
        use gentl_gvcp::wire::{
            GEV_STATUS_ACCESS_DENIED, GEV_STATUS_BUSY, GEV_STATUS_INVALID_PARAMETER,
        };

        // GVSP-only device (factory default, as today).
        let m = map_with_streaming(StreamingProtocol::GvspOnly);
        assert_eq!(m.read_reg(reg::STREAMING_CAPABILITY).unwrap(), 0);
        assert_eq!(
            m.write_reg(
                reg::STREAMING_CONFIGURATION,
                reg::SCONF_DEPRECATED_BIT
                    | (reg::SCONF_PROTOCOL_GVRSP << reg::SCONF_STREAMING_PROTOCOL_SHIFT)
            ),
            Err(GEV_STATUS_INVALID_PARAMETER),
            "Table 36-2: GVRSP mode on a GVSP-only device"
        );
        // Open channel 0, then the configuration locks (R-667cd).
        m.write_reg(reg::SCP0, reg::scp_make(40001)).unwrap();
        assert_eq!(
            m.write_reg(reg::STREAMING_CONFIGURATION, reg::SCONF_DEPRECATED_BIT),
            Err(GEV_STATUS_BUSY)
        );
        m.write_reg(reg::SCP0, 0).unwrap();
        // Reserved protocol values 2-3 are rejected (notes/OPEN-QUESTIONS.md Q29).
        assert_eq!(
            m.write_reg(
                reg::STREAMING_CONFIGURATION,
                reg::SCONF_DEPRECATED_BIT | (2 << 26)
            ),
            Err(GEV_STATUS_INVALID_PARAMETER)
        );
        // Re-selecting the active mode is legal.
        m.write_reg(reg::STREAMING_CONFIGURATION, reg::SCONF_DEPRECATED_BIT).unwrap();
        assert_eq!(
            m.read_reg(reg::STREAMING_CONFIGURATION).unwrap(),
            reg::SCONF_DEPRECATED_BIT
        );

        // Dual-mode device: switching to GVRSP locks out the GVSP channel
        // registers (CR-671cd), switching back restores them.
        let m = map_with_streaming(StreamingProtocol::Both);
        assert_eq!(
            m.read_reg(reg::STREAMING_CAPABILITY).unwrap(),
            reg::SCAP_SCEBA_SUPPORTED | reg::SCAP_GVRSP_MODE_SUPPORTED,
            "CR-665cd: SCEBAx mandatory with GVRSP"
        );
        m.write_reg(
            reg::STREAMING_CONFIGURATION,
            reg::SCONF_DEPRECATED_BIT
                | (reg::SCONF_PROTOCOL_GVRSP << reg::SCONF_STREAMING_PROTOCOL_SHIFT),
        )
        .unwrap();
        assert_eq!(m.read_reg(reg::SCP0), Err(GEV_STATUS_ACCESS_DENIED));
        assert_eq!(m.write_reg(reg::SCP0, reg::scp_make(40001)), Err(GEV_STATUS_ACCESS_DENIED));
        m.write_reg(reg::STREAMING_CONFIGURATION, reg::SCONF_DEPRECATED_BIT).unwrap();
        assert_eq!(m.read_reg(reg::SCP0).unwrap(), 0, "GVSP mode restores channel access");

        // GVRSP-only device: mode 0 writes rejected, channels denied.
        let m = map_with_streaming(StreamingProtocol::GvrspOnly);
        assert_eq!(
            m.read_reg(reg::STREAMING_CAPABILITY).unwrap(),
            reg::SCAP_GVSP_MODE_NOT_SUPPORTED
                | reg::SCAP_GVRSP_MODE_SUPPORTED
                | reg::SCAP_SCEBA_SUPPORTED
        );
        assert_eq!(m.read_reg(reg::SCP0), Err(GEV_STATUS_ACCESS_DENIED));
        assert_eq!(
            m.write_reg(reg::STREAMING_CONFIGURATION, reg::SCONF_DEPRECATED_BIT),
            Err(GEV_STATUS_INVALID_PARAMETER),
            "Table 36-2: GVSP mode on a GVRSP-only device"
        );
    }
}
