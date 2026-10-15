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
//! stream channel (a [`crate::emitter::StreamEmitter`] thread targeting
//! SCDA0); writing 0 closes it. The transmitter owns no register-map locks,
//! so stopping it can never deadlock against reads or writes here.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use gentl_gvcp::registers as reg;
use gentl_gvcp::wire::*;
use gentl_gvcp::DeviceHandler;

use crate::emitter::{StreamEmitter, StreamParams, DEFAULT_PAYLOAD};
use crate::{SimConfig, StreamConfig};

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

// Device Mode (spec §36.2): bit0 deprecated-always-1, device_class =
// Transmitter(0), single link configuration, character_set_index = 2 (ASCII).
const DEVICE_MODE_VALUE: u32 = 0x0200_0001;

// Net If Configuration writable bits: only the supported P/L bits plus
// PAUSE (spec §36.6); the sim does not offer DHCP.
const NET_IF_CFG_WRITABLE: u32 = reg::IP_CFG_PERSISTENT_IP | reg::IP_CFG_LLA | 0x3;

// GVCP Capability (spec §36.29): user-defined-name(0), serial-number(1),
// the three deprecated always-1 bits (9, 16, 26), concatenation(31).
const GVCP_CAPABILITY_VALUE: u32 = 0x8401_0203;

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
/// GEV_STATUS_INVALID_ADDRESS.
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
    br(reg::GVCP_CAPABILITY, 4, Ro),
    br(reg::HEARTBEAT_TIMEOUT, 4, Rw),
    br(reg::TIMESTAMP_TICK_FREQ_HIGH, 4, Ro),
    br(reg::TIMESTAMP_TICK_FREQ_LOW, 4, Ro),
    br(reg::TIMESTAMP_VALUE_HIGH, 4, Ro),
    br(reg::TIMESTAMP_VALUE_LOW, 4, Ro),
    br(reg::CCP, 4, Rw),
    br(reg::SCP0, 4, Rw),
    br(reg::SCPS0, 4, Rw),
    br(reg::SCPD0, 4, Rw),
    br(reg::SCDA0, 4, Rw),
];

const BOOT_LEN: u32 = 0x1000;

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
}

/// Register/memory model of one simulated camera.
pub struct SimRegMap {
    config: SimConfig,
    stream: StreamConfig,
    xml: Vec<u8>,
    inner: Mutex<Inner>,
    /// Transmitter of the open stream channel; `None` = channel closed.
    emitter: Mutex<Option<StreamEmitter>>,
}

impl SimRegMap {
    pub fn new(config: SimConfig, xml: &[u8], stream: StreamConfig) -> Self {
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
            }),
            emitter: Mutex::new(None),
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
        Self::w32(boot, reg::NUM_STREAM_CHANNELS, 1);
        Self::w32(boot, reg::GVCP_CAPABILITY, GVCP_CAPABILITY_VALUE);
        Self::w32(boot, reg::TIMESTAMP_TICK_FREQ_HIGH, (TIMESTAMP_TICK_FREQ >> 32) as u32);
        Self::w32(boot, reg::TIMESTAMP_TICK_FREQ_LOW, TIMESTAMP_TICK_FREQ as u32);
        // SCPS0 factory default is device-specific (spec §36.58). Sim
        // convention: the register holds the *total* packet size (bits
        // 16-31) including the IP/UDP/GVSP headers (spec layout, see
        // notes/gvcp-wirefacts.md); 1400 payload + 48 headers keeps the
        // IPv4 total at 1448 bytes, under the 1500-byte MTU.
        Self::w32(
            boot,
            reg::SCPS0,
            reg::scps_make(DEFAULT_PAYLOAD as u32 + reg::GVSP_HEADERS_OVERHEAD),
        );

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
    /// mapped memory (bootstrap slots, user area, or the XML blob).
    fn read_mapped(&self, addr: u32, len: u32) -> Result<Vec<u8>, u16> {
        let end = addr.checked_add(len).ok_or(GEV_STATUS_INVALID_ADDRESS)?;
        let mut out = Vec::with_capacity(len as usize);
        let mut pos = addr;
        while pos < end {
            let take_end;
            if pos < BOOT_LEN {
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
        if addr == reg::CCP {
            let i = self.inner.lock().expect("sim regmap poisoned");
            return Ok(i.ccp.flags | ((i.ccp.connection_count & 0xF) << 16));
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
        if addr == reg::SCP0 {
            return self.write_scp0(value);
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
    /// SCP0 (0x0D00, spec §36.57): a non-zero host_port field (bits 16-31)
    /// opens the stream channel (spec §11.5, R-087ca), zero closes it. The
    /// register value follows the spec layout; SCPx must be the last
    /// register written when opening and the first when closing.
    ///
    /// The transmitter is created before the register value is published
    /// and stopped after it is withdrawn, and the thread join in
    /// [`StreamEmitter`]'s drop never runs while a register-map lock is
    /// held (the emitter itself takes no register-map locks), so opening
    /// and closing cannot deadlock against reads or writes.
    fn write_scp0(&self, value: u32) -> Result<(), u16> {
        let host_port = reg::scp_host_port(value);
        if value != 0 && host_port == 0 {
            // A non-zero write with the host_port field left at 0 is not a
            // valid spec-layout SCPx write (it is what a raw "write the
            // bare port" produces, e.g. 50000); reject it loudly so the
            // wrong layout fails at the device instead of silently
            // closing the channel (notes/OPEN-QUESTIONS.md Q14 resolved).
            return Err(GEV_STATUS_INVALID_PARAMETER);
        }
        if host_port != 0 {
            let port = host_port;
            // Snapshot the channel configuration: a real device forbids
            // SCPSx/SCPDx writes while streaming (spec §36.58/§36.59), so
            // freezing the values at open matches the observable contract.
            let (target, params) = {
                let i = self.inner.lock().expect("sim regmap poisoned");
                if reg::scp_host_port(u32::from_be_bytes(
                    i.boot[reg::SCP0 as usize..reg::SCP0 as usize + 4]
                        .try_into()
                        .unwrap(),
                )) != 0
                {
                    // R-586cd: only a zero write (close) is accepted while
                    // the channel is streaming.
                    return Err(GEV_STATUS_BUSY);
                }
                let word = |addr: u32| {
                    u32::from_be_bytes(
                        i.boot[addr as usize..addr as usize + 4]
                            .try_into()
                            .unwrap(),
                    )
                };
                (
                    SocketAddr::from((Ipv4Addr::from(word(reg::SCDA0)), port)),
                    StreamParams::new(&self.stream, word(reg::SCPS0), word(reg::SCPD0)),
                )
            };
            let emitter =
                StreamEmitter::open(target, params).map_err(|_| GEV_STATUS_ERROR)?;
            let old = {
                let mut i = self.inner.lock().expect("sim regmap poisoned");
                i.boot[reg::SCP0 as usize..reg::SCP0 as usize + 4]
                    .copy_from_slice(&value.to_be_bytes());
                self.emitter
                    .lock()
                    .expect("sim emitter poisoned")
                    .replace(emitter)
            };
            // Join outside the locks (defensive; the slot was known empty).
            drop(old);
            return Ok(());
        }

        let old = {
            let mut i = self.inner.lock().expect("sim regmap poisoned");
            i.boot[reg::SCP0 as usize..reg::SCP0 as usize + 4]
                .copy_from_slice(&0u32.to_be_bytes());
            self.emitter.lock().expect("sim emitter poisoned").take()
        };
        drop(old);
        Ok(())
    }
}

impl Drop for SimRegMap {
    fn drop(&mut self) {
        // The transmitter's lifetime follows the register map (and thereby
        // the GVCP server holding it); its Drop joins the emitter thread.
        drop(self.emitter.lock().expect("sim emitter poisoned").take());
    }
}

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
            reg::CCP_EXCLUSIVE_ACCESS | (1 << 16)
        );
        // R-069cd: repeated identical request is accepted, no new connection.
        m.write_reg(reg::CCP, reg::CCP_EXCLUSIVE_ACCESS).unwrap();
        assert_eq!(
            m.read_reg(reg::CCP).unwrap(),
            reg::CCP_EXCLUSIVE_ACCESS | (1 << 16)
        );
        // Different privilege while occupied: denied.
        assert_eq!(
            m.write_reg(reg::CCP, reg::CCP_CONTROL_ACCESS),
            Err(GEV_STATUS_ACCESS_DENIED)
        );
        // Release, then control access grants and bumps the counter. The
        // connection count never returns to zero (spec §36.46).
        m.write_reg(reg::CCP, 0).unwrap();
        assert_eq!(m.read_reg(reg::CCP).unwrap(), 1 << 16);
        m.write_reg(reg::CCP, reg::CCP_CONTROL_ACCESS).unwrap();
        assert_eq!(
            m.read_reg(reg::CCP).unwrap(),
            reg::CCP_CONTROL_ACCESS | (2 << 16)
        );
        m.write_reg(reg::CCP, 0).unwrap();
        m.write_reg(reg::CCP, reg::CCP_EXCLUSIVE_ACCESS).unwrap();
        assert_eq!(
            m.read_reg(reg::CCP).unwrap(),
            reg::CCP_EXCLUSIVE_ACCESS | (3 << 16)
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
            },
            b"<fake/>",
            crate::StreamConfig {
                width: 640,
                height: 480,
                pixel_format: 0x0110_0007, // Mono16
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

        // Writes must use the spec layout (host_port in bits 16-31): a bare
        // port value (host_port field 0, value non-zero) is rejected, even
        // though the same number in the right layout opens the channel.
        assert_eq!(
            m.write_reg(reg::SCP0, 3956).unwrap_err(),
            GEV_STATUS_INVALID_PARAMETER
        );
        assert_eq!(
            m.write_reg(reg::SCP0, 50000).unwrap_err(),
            GEV_STATUS_INVALID_PARAMETER
        );
        // Open towards loopback (no receiver bound: the emitter's sends
        // fail silently, like on a real camera whose receiver vanished).
        m.write_reg(reg::SCDA0, u32::from(Ipv4Addr::LOCALHOST))
            .unwrap();
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
        // Net If Configuration masks off unsupported (DHCP) bits.
        m.write_reg(reg::NET_IF_CONFIGURATION, 0xFFFFFFFF).unwrap();
        assert_eq!(
            m.read_reg(reg::NET_IF_CONFIGURATION).unwrap(),
            reg::IP_CFG_PERSISTENT_IP | reg::IP_CFG_LLA | 0x3
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
}
