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
//!   application from a stranger (docs/00-governance/OPEN-QUESTIONS.md Q7). Writing 0
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
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gentl_gvcp::registers as reg;
use gentl_gvcp::wire::*;
use gentl_gvcp::DeviceHandler;

use crate::emitter::{ChannelSink, PatternConfig, StreamParams, StreamScheduler, DEFAULT_PAYLOAD};
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
/// SFNC ReverseX/ReverseY (image generator mirrors the fake frame).
pub const REG_REVERSE_X: u32 = USER_BASE + 16;
pub const REG_REVERSE_Y: u32 = USER_BASE + 20;
/// SFNC TestPattern (0 = off, 1 = horizontal ramp, 2 = vertical ramp).
pub const REG_TEST_PATTERN: u32 = USER_BASE + 24;
/// SFNC UserSetControl command/selector registers.
pub const REG_USER_SET_SELECTOR: u32 = USER_BASE + 28;
pub const REG_USER_SET_SAVE: u32 = USER_BASE + 32;
pub const REG_USER_SET_LOAD: u32 = USER_BASE + 36;
/// Test event source (message channel): event id, EVENTDATA payload length
/// (0 = plain EVENT_CMD) and the self-clearing trigger.
pub const REG_TEST_EVENT_ID: u32 = USER_BASE + 40;
pub const REG_TEST_EVENT_DATA_LEN: u32 = USER_BASE + 44;
pub const REG_TEST_EVENT_TRIGGER: u32 = USER_BASE + 48;
const USER_LEN: u32 = 52;

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
const NET_IF_CFG_WRITABLE: u32 = reg::IP_CFG_PERSISTENT_IP | reg::IP_CFG_DHCP;

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
    br(reg::PENDING_TIMEOUT, 4, Rw),
    br(reg::TIMESTAMP_TICK_FREQ_HIGH, 4, Ro),
    br(reg::TIMESTAMP_TICK_FREQ_LOW, 4, Ro),
    br(reg::TIMESTAMP_VALUE_HIGH, 4, Ro),
    br(reg::TIMESTAMP_VALUE_LOW, 4, Ro),
    br(reg::CCP, 4, Rw),
    br(reg::STREAMING_CONFIGURATION, 4, Rw),
    br(reg::MESSAGE_CHANNEL_PORT, 4, Rw),
    br(reg::MESSAGE_CHANNEL_DEST_ADDRESS, 4, Rw),
    br(reg::MESSAGE_CHANNEL_TRANSMISSION_TIMEOUT, 4, Rw),
    br(reg::MESSAGE_CHANNEL_RETRY_COUNT, 4, Rw),
    br(reg::MESSAGE_CHANNEL_SOURCE_PORT, 4, Ro),
    br(reg::MESSAGE_CHANNEL_CONFIGURATION, 4, Rw),
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
/// Word index of the SCCx capability value inside a channel's register block
/// (§36.62/CR-494cd: mandatory for GVSP devices; the sim implements none of
/// the optional per-channel features, so it reads 0).
const W_SCC: usize = 4;
/// Word index of the SCCFGx configuration value inside a channel's register
/// block (§36.63/CR-495cd: mandatory for GVSP devices; reserved bits, only a
/// zero write is accepted, and R-590cd forbids writes while streaming).
const W_SCCFG: usize = 5;
/// Words stored per stream channel register block.
const CHANNEL_WORDS: usize = 6;

/// Map a stream channel register offset (§36.57–§36.63) to its word index
/// inside the channel's register block. Only called with offsets validated
/// by [`reg::stream_channel_addr`].
fn channel_word_index(off: u32) -> usize {
    match off {
        reg::SCP_OFFSET => W_SCP,
        reg::SCPS_OFFSET => W_SCPS,
        reg::SCPD_OFFSET => W_SCPD,
        reg::SCDA_OFFSET => W_SCDA,
        reg::SCC_OFFSET => W_SCC,
        _ => W_SCCFG,
    }
}

/// CCP (0x0A00) state per spec §10.1/§36.46. The switchover key field reads
/// as 0 and CSE is not granted (GVCP Capability bit 10 = 0).
#[derive(Default)]
struct CcpState {
    /// Granted privilege: reg::CCP_EXCLUSIVE_ACCESS, reg::CCP_CONTROL_ACCESS, or 0.
    flags: u32,
    /// Primary application (control-channel peer): secondary applications
    /// are restricted by R-063cd (exclusive) and R-170cd (writes).
    peer: Option<SocketAddr>,
    /// CCP bits 16-19: incremented on every channel open, never 0 again
    /// after the first connection (saturating at 15).
    connection_count: u32,
    /// Last command from the primary application (§10.5): used for the
    /// heartbeat watchdog. `None` while no control channel is open.
    last_activity: Option<Instant>,
}

impl Inner {
    /// R-076cd: more than one HEARTBEAT_TIMEOUT period without a control
    /// message from the primary closes the control channel (state reset; the
    /// caller performs the R-072cd stream/message cleanup).
    fn expire_heartbeat(&mut self) -> bool {
        if self.ccp.flags == 0 {
            return false;
        }
        let window = Duration::from_millis(u64::from(self.heartbeat_ms));
        if self
            .ccp
            .last_activity
            .is_some_and(|last| last.elapsed() > window)
        {
            self.ccp.flags = 0;
            self.ccp.peer = None;
            self.ccp.last_activity = None;
            true
        } else {
            false
        }
    }
}

/// Everything mutable; the map is reachable only through `&self`.
struct Inner {
    /// Backing bytes for 0x0000..0x0FFF; only slots listed in `BOOT_REGS`
    /// are ever touched.
    boot: Vec<u8>,
    /// Backing bytes for the user register area.
    user: [u8; USER_LEN as usize],
    /// SFNC UserSet1 snapshot (UserSetSave/UserSetLoad).
    user_saved: [u8; USER_LEN as usize],
    ccp: CcpState,
    heartbeat_ms: u32,
    /// Open message channel destination (MCDA + MCP) when both are set.
    message_dest: Option<SocketAddr>,
    /// Message channel sequence (test event source).
    event_seq: u32,
    ts_epoch: Instant,
    /// Stream channel registers per channel (spec §36.57–§36.60), indexed
    /// by channel number: [SCPx, SCPSx, SCPDx, SCDAx]. The vec holds
    /// `StreamConfig::channels` entries; channels past that count are
    /// optional in the spec and answer GEV_STATUS_INVALID_ADDRESS.
    channels: Vec<[u32; CHANNEL_WORDS]>,
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
    /// Test hook: when false, plain (no-fire) SCPSx writes are accepted
    /// while the channel streams, modelling a 1.2 camera whose §11.7
    /// window stays open for the packet-size fallback.
    block_scps_writes: AtomicBool,
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
                user_saved: [0u8; USER_LEN as usize],
                ccp: CcpState::default(),
                heartbeat_ms: HEARTBEAT_DEFAULT_MS,
                message_dest: None,
                event_seq: 0,
                ts_epoch: Instant::now(),
                channels: vec![[0; CHANNEL_WORDS]; n_channels],
            }),
            scheduler: StreamScheduler::new(),
            block_scps_writes: AtomicBool::new(true),
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

    /// Authorize one command from `peer` (GigE Vision 10.1): the primary
    /// application is the control-channel owner; during exclusive access
    /// every other application is denied (R-063cd), during control access
    /// writes are denied (R-170cd). Primary traffic resets the heartbeat; an
    /// expired channel is closed and cleaned up (R-072cd).
    fn authorize(&self, peer: SocketAddr, write: bool) -> Result<(), u16> {
        let expired = {
            let mut i = self.inner.lock().expect("sim regmap poisoned");
            if i.ccp.flags == 0 {
                return Ok(());
            }
            if i.expire_heartbeat() {
                true
            } else {
                let exclusive = i.ccp.flags & reg::CCP_EXCLUSIVE_ACCESS != 0;
                if i.ccp.peer == Some(peer) {
                    i.ccp.last_activity = Some(Instant::now());
                } else if write || exclusive {
                    return Err(GEV_STATUS_ACCESS_DENIED);
                }
                false
            }
        };
        if expired {
            self.close_control_channel();
        }
        Ok(())
    }

    /// R-072cd: stop streaming and clear the message/GVSP connection state
    /// when the primary control channel closes.
    fn close_control_channel(&self) {
        for x in 0..self.stream.channels.max(1) {
            let _ = self.write_scp(x as u32, 0);
        }
        self.close_message_channel();
    }

    /// Expire an idle primary grant without refreshing it (FORCEIP must not
    /// count as heartbeat traffic, R-075cd) and run the R-072cd cleanup.
    fn expire_stale_grant(&self) -> bool {
        let expired = {
            let mut i = self.inner.lock().expect("sim regmap poisoned");
            i.ccp.flags != 0 && i.expire_heartbeat()
        };
        if expired {
            self.close_control_channel();
        }
        expired
    }

    /// WRITEMEM core with byte progress (R-194cd): words before a failing
    /// word stay committed; the returned index is the failing byte offset.
    fn write_mem_inner(&self, addr: u32, data: &[u8]) -> Result<(), (u16, u32)> {
        if data.is_empty() || data.len() % 4 != 0 || addr % 4 != 0 {
            return Err((GEV_STATUS_BAD_ALIGNMENT, 0));
        }
        if addr >= XML_BASE && addr + data.len() as u32 <= XML_BASE + self.xml.len() as u32 {
            return Err((GEV_STATUS_WRITE_PROTECT, 0));
        }
        for (i, word) in data.chunks_exact(4).enumerate() {
            let word_addr = addr + 4 * i as u32;
            let value = u32::from_be_bytes(word.try_into().expect("4-byte chunk"));
            self.write_reg(word_addr, value)
                .map_err(|status| (status, (i * 4) as u32))?;
        }
        Ok(())
    }

    /// Heartbeat bookkeeping for the peer-less API (unit tests, embedded
    /// users): such grants have `ccp.peer == None`. Channels granted through
    /// the GVCP server carry a peer and are refreshed by `authorize`.
    fn direct_contact(&self) {
        let expired = {
            let mut i = self.inner.lock().expect("sim regmap poisoned");
            if i.ccp.flags == 0 || i.ccp.peer.is_some() {
                return;
            }
            if i.expire_heartbeat() {
                true
            } else {
                i.ccp.last_activity = Some(Instant::now());
                false
            }
        };
        if expired {
            self.close_control_channel();
        }
    }

    /// Test hook: send one EVENT_CMD (payload length 0) or EVENTDATA_CMD with
    /// a deterministic payload on the open message channel.
    fn emit_test_event(&self) {
        let (dest, event_id, data_len, timestamp, seq) = {
            let mut i = self.inner.lock().expect("sim regmap poisoned");
            i.event_seq = i.event_seq.wrapping_add(1);
            let seq = i.event_seq;
            let word = |reg_addr: u32| {
                u32::from_be_bytes(
                    i.user[(reg_addr - USER_BASE) as usize..][..4]
                        .try_into()
                        .expect("u32"),
                )
            };
            let timestamp = i.ts_epoch.elapsed().as_micros() as u64;
            (
                i.message_dest,
                word(REG_TEST_EVENT_ID) as u16,
                (word(REG_TEST_EVENT_DATA_LEN) as usize).min(8),
                timestamp,
                seq,
            )
        };
        let Some(dest) = dest else { return };
        let data: Vec<u8> = (0..data_len).map(|k| 0xE0u8.wrapping_add(k as u8)).collect();
        let command = if data.is_empty() {
            EVENT_CMD
        } else {
            EVENTDATA_CMD
        };
        let record_size = EVENT_RECORD_LEN + data.len();
        let mut datagram = Vec::with_capacity(HEADER_LEN + record_size);
        datagram.extend_from_slice(&[0x42, 0x10]); // GVCP key; bit 3 (msb-first) = EI
        datagram.extend_from_slice(&command.to_be_bytes());
        datagram.extend_from_slice(&(record_size as u16).to_be_bytes());
        // CR-097cd: req_id increments per message and 0 is invalid.
        let req_id = (seq % 0xFFFF) as u16;
        datagram.extend_from_slice(&req_id.max(1).to_be_bytes());
        datagram.extend_from_slice(&(record_size as u16).to_be_bytes());
        datagram.extend_from_slice(&event_id.to_be_bytes());
        datagram.extend_from_slice(&0xFFFFu16.to_be_bytes()); // no stream channel
        datagram.extend_from_slice(&0u16.to_be_bytes());
        datagram.extend_from_slice(&0u64.to_be_bytes()); // no block id
        datagram.extend_from_slice(&timestamp.to_be_bytes());
        datagram.extend_from_slice(&data);
        if let Ok(socket) = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)) {
            let _ = socket.send_to(&datagram, dest);
        }
    }

    fn init_bootstrap(&mut self) {        let mut i = self.inner.lock().expect("sim regmap poisoned");
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
        // GigE Vision 3.0 §4.3: DHCP support is mandatory ("this bit is
        // always 1"); §4.4: LLA support is mandatory and always active
        // (configuration bit 29 is always set). Persistent IP is optional
        // and advertised because the sim runs a static address.
        Self::w32(
            boot,
            reg::NET_IF_CAPABILITY,
            reg::IP_CFG_PERSISTENT_IP | reg::IP_CFG_DHCP | reg::IP_CFG_LLA,
        );
        Self::w32(
            boot,
            reg::NET_IF_CONFIGURATION,
            reg::IP_CFG_PERSISTENT_IP | reg::IP_CFG_LLA,
        );
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
        Self::w32(boot, reg::NUM_MESSAGE_CHANNELS, 1);
        // Message Channel Capability (§36.28): no optional features (MCSP /
        // MCCFG / EVENTDATA concatenation) are implemented.
        Self::w32(boot, reg::MESSAGE_CHANNEL_CAPABILITY, 0);
        // Streaming Configuration factory default follows the supported
        // modes (§36.38: GVSP if supported, GVRSP-only otherwise). Bit 1 is
        // the deprecated always-1 bit. A GVRSP-only device has no GVSP
        // stream channels (its SCPx registers answer ACCESS_DENIED per
        // CR-671cd).
        let (capability, mode_default, n_sc) = match cfg.streaming {
            StreamingProtocol::GvspOnly => (
                0,
                reg::SCONF_PROTOCOL_GVSP << reg::SCONF_STREAMING_PROTOCOL_SHIFT,
                self.stream.channels,
            ),
            StreamingProtocol::Both => (
                reg::SCAP_GVRSP_MODE_SUPPORTED | reg::SCAP_SCEBA_SUPPORTED,
                reg::SCONF_PROTOCOL_GVSP << reg::SCONF_STREAMING_PROTOCOL_SHIFT,
                self.stream.channels,
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
        Self::w32(boot, reg::PENDING_TIMEOUT, 1000);
        Self::w32(boot, reg::TIMESTAMP_TICK_FREQ_HIGH, (TIMESTAMP_TICK_FREQ >> 32) as u32);
        Self::w32(boot, reg::TIMESTAMP_TICK_FREQ_LOW, TIMESTAMP_TICK_FREQ as u32);

        // SCPSx factory defaults (spec §36.58: device-specific). Sim
        // convention: the register holds the *total* packet size (bits
        // 16-31) including the IP/UDP/GVSP headers of the configured
        // header form (48-byte budget at 3.0, 36-byte at 1.2 legacy —
        // 1.2 §27.46; see docs/10-protocol/gvcp-wirefacts.md and Q28); 1400 payload +
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
            } else if pos >= XML_BASE + self.xml.len() as u32
                && pos < XML_BASE + self.xml.len() as u32 + 4
                && (out.len() as u32) < len
            {
                // The GenTL XML fetch aligns each READMEM chunk up to a
                // 4-byte multiple, so the last chunk can start past the blob
                // end when the XML length is not aligned. The XML is the
                // sim's own memory (not a device mapping boundary): pad the
                // requested tail with zeros instead of INVALID_ADDRESS.
                out.resize(len as usize, 0);
                take_end = end;
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
            ip_config_options: reg::IP_CFG_PERSISTENT_IP | reg::IP_CFG_DHCP | reg::IP_CFG_LLA,
            ip_config_current: reg::IP_CFG_PERSISTENT_IP | reg::IP_CFG_LLA,
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
        self.direct_contact();
        if addr % 4 != 0 {
            return Err(GEV_STATUS_BAD_ALIGNMENT);
        }
        // R-075cd: any valid command from the primary application resets the
        // heartbeat counter (and an idle channel times out, R-076cd).
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
        self.direct_contact();
        if addr % 4 != 0 {
            return Err(GEV_STATUS_BAD_ALIGNMENT);
        }
        if let Some((x, off)) = reg::stream_channel_addr(addr) {
            return self.write_stream_channel(x, off, value);
        }
        let mut i = self.inner.lock().expect("sim regmap poisoned");
        if addr == reg::CCP {
            // CCP: 0 closes the primary control channel; the peer identity
            // and access checks are enforced in `write_reg_from`/`authorize`.
            // R-070cd: the primary may switch EA <-> CA without closing.
            if value == 0 {
                i.ccp.flags = 0;
                i.ccp.peer = None;
                i.ccp.last_activity = None;
                drop(i);
                // R-072cd: closing the primary channel stops streaming and
                // clears the message/GVSP connection state.
                self.close_control_channel();
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
                i.ccp.last_activity = Some(Instant::now());
                return Ok(());
            }
            // R-069cd: a repeated request succeeds; R-070cd: the primary
            // may switch between EA and CA without closing the channel.
            i.ccp.flags = if requested & reg::CCP_EXCLUSIVE_ACCESS != 0 {
                reg::CCP_EXCLUSIVE_ACCESS
            } else {
                reg::CCP_CONTROL_ACCESS
            };
            i.ccp.last_activity = Some(Instant::now());
            return Ok(());
        }
        if addr == reg::MESSAGE_CHANNEL_DEST_ADDRESS || addr == reg::MESSAGE_CHANNEL_PORT {
            // §12.2: the channel opens when MCP carries a non-zero host port
            // (MCDA first); §12.4: MCP = 0 closes it.
            i.boot[addr as usize..addr as usize + 4].copy_from_slice(&value.to_be_bytes());
            let read = |a: u32| {
                u32::from_be_bytes(
                    i.boot[a as usize..a as usize + 4]
                        .try_into()
                        .expect("u32"),
                )
            };
            let port = reg::mcp_host_port(read(reg::MESSAGE_CHANNEL_PORT));
            let dest_ip = read(reg::MESSAGE_CHANNEL_DEST_ADDRESS);
            i.message_dest = (port != 0 && dest_ip != 0)
                .then(|| SocketAddr::from((Ipv4Addr::from(dest_ip), port)));
            return Ok(());
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
            // docs/00-governance/OPEN-QUESTIONS.md Q29) returns GEV_STATUS_INVALID_PARAMETER.
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
                // Reserved values 2-3: rejected (docs/00-governance/OPEN-QUESTIONS.md Q29).
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
                // [R-029cd]: LLA is always activated and cannot be disabled.
                i.boot[addr as usize..addr as usize + 4].copy_from_slice(
                    &((value & NET_IF_CFG_WRITABLE) | reg::IP_CFG_LLA).to_be_bytes(),
                );
            } else {
                i.boot[addr as usize..addr as usize + 4].copy_from_slice(&value.to_be_bytes());
            }
            return Ok(());
        }
        if addr >= USER_BASE && addr + 4 <= USER_BASE + USER_LEN {
            // SFNC features with real semantics on the user register area:
            // UserSetSave/Load snapshot and restore the whole feature set,
            // and the image generator reads TestPattern/ReverseX/ReverseY at
            // channel open (the sim freezes them with the packet geometry).
            match addr {
                REG_USER_SET_SAVE => {
                    let selector = u32::from_be_bytes(
                        i.user[(REG_USER_SET_SELECTOR - USER_BASE) as usize..][..4]
                            .try_into()
                            .expect("u32"),
                    );
                    if value != 0 && selector == 1 {
                        i.user_saved = i.user;
                    }
                    i.user[(addr - USER_BASE) as usize..(addr - USER_BASE + 4) as usize]
                        .copy_from_slice(&0u32.to_be_bytes());
                    return Ok(());
                }
                REG_USER_SET_LOAD => {
                    let selector = u32::from_be_bytes(
                        i.user[(REG_USER_SET_SELECTOR - USER_BASE) as usize..][..4]
                            .try_into()
                            .expect("u32"),
                    );
                    if value != 0 && selector == 1 {
                        let saved = i.user_saved;
                        i.user = saved;
                    }
                    i.user[(addr - USER_BASE) as usize..(addr - USER_BASE + 4) as usize]
                        .copy_from_slice(&0u32.to_be_bytes());
                    return Ok(());
                }
                REG_TEST_PATTERN => {
                    if value <= 2 {
                        i.user[(addr - USER_BASE) as usize..(addr - USER_BASE + 4) as usize]
                            .copy_from_slice(&value.to_be_bytes());
                    }
                    return Ok(());
                }
                REG_REVERSE_X | REG_REVERSE_Y | REG_USER_SET_SELECTOR => {
                    if value <= 1 {
                        i.user[(addr - USER_BASE) as usize..(addr - USER_BASE + 4) as usize]
                            .copy_from_slice(&value.to_be_bytes());
                    }
                    return Ok(());
                }
                _ => {}
            }
            i.user[(addr - USER_BASE) as usize..(addr - USER_BASE + 4) as usize]
                .copy_from_slice(&value.to_be_bytes());
            if addr == REG_TEST_EVENT_TRIGGER {
                // Self-clearing trigger: fire one event on the open message
                // channel (device-side test source, §17.1/§17.2).
                i.user[(addr - USER_BASE) as usize..(addr - USER_BASE + 4) as usize]
                    .copy_from_slice(&0u32.to_be_bytes());
                drop(i);
                self.emit_test_event();
            }
            return Ok(());
        }
        Err(GEV_STATUS_INVALID_ADDRESS)
    }

    fn read_mem(&self, addr: u32, count: u16) -> Result<Vec<u8>, u16> {
        self.direct_contact();
        // READMEM is a primary-application command like READREG: it resets
        // the heartbeat watchdog (R-075cd).
        self.read_mapped(addr, count as u32)
    }

    fn write_mem(&self, addr: u32, data: &[u8]) -> Result<(), u16> {
        self.direct_contact();
        self.write_mem_inner(addr, data).map_err(|(status, _)| status)
    }

    fn write_reg_from(&self, peer: SocketAddr, addr: u32, value: u32) -> Result<(), u16> {
        self.authorize(peer, true)?;
        let result = self.write_reg(addr, value);
        if result.is_ok() && addr == reg::CCP && value != 0 {
            // The grant/switch above succeeded for this peer: record the
            // primary application for later access checks.
            self.inner.lock().expect("sim regmap poisoned").ccp.peer = Some(peer);
        }
        result
    }

    fn read_reg_from(&self, peer: SocketAddr, addr: u32) -> Result<u32, u16> {
        self.authorize(peer, false)?;
        self.read_reg(addr)
    }

    fn read_mem_from(
        &self,
        peer: SocketAddr,
        addr: u32,
        count: u16,
    ) -> Result<Vec<u8>, u16> {
        self.authorize(peer, false)?;
        self.read_mem(addr, count)
    }

    fn write_mem_from(&self, peer: SocketAddr, addr: u32, data: &[u8]) -> Result<(), u16> {
        self.write_mem_from_progress(peer, addr, data)
            .map_err(|(status, _)| status)
    }

    fn write_mem_from_progress(
        &self,
        peer: SocketAddr,
        addr: u32,
        data: &[u8],
    ) -> Result<(), (u16, u32)> {
        self.authorize(peer, true).map_err(|status| (status, 0))?;
        self.write_mem_inner(addr, data)
    }

    fn tick(&self) {
        // R-076cd: close an idle control channel even if no further command
        // ever arrives.
        let _ = self.expire_stale_grant();
    }

    fn force_ip(
        &self,
        _peer: SocketAddr,
        request: ForceIpRequest,
    ) -> gentl_gvcp::wire::ForceIpOutcome {
        use gentl_gvcp::wire::ForceIpOutcome;
        // R-149cd: a FORCEIP_CMD aimed at another MAC is silently discarded
        // (the application broadcasts it, so every device sees it).
        if request.mac != self.config.mac {
            return ForceIpOutcome::Ignore;
        }
        // R-076cd/R-150cd: expire an idle grant before checking the
        // privilege; FORCEIP itself must not refresh the heartbeat.
        self.expire_stale_grant();
        {
            let i = self.inner.lock().expect("sim regmap poisoned");
            // R-150cd: only process the command when no application holds
            // exclusive or control access.
            if i.ccp.flags != 0 {
                return ForceIpOutcome::Ignore;
            }
        }
        if request.static_ip == 0 {
            // R-151cd: close every stream channel and restart the IP
            // configuration cycle; no ACK is sent. The sim re-applies its
            // configured address — the cycle has nothing else to negotiate.
            for x in 0..self.stream.channels.max(1) as u32 {
                let _ = self.write_scp(x, 0);
            }
            self.close_message_channel();
            return ForceIpOutcome::Ignore;
        }
        // R-152cd: install the temporary static IP configuration, then
        // acknowledge it (the GVCP server sends the ACK once this returns).
        {
            let mut i = self.inner.lock().expect("sim regmap poisoned");
            Self::w32(&mut i.boot, reg::CURRENT_IP, request.static_ip);
            Self::w32(
                &mut i.boot,
                reg::CURRENT_SUBNET_MASK,
                request.subnet_mask,
            );
            Self::w32(
                &mut i.boot,
                reg::CURRENT_DEFAULT_GATEWAY,
                request.default_gateway,
            );
        }
        ForceIpOutcome::Ack
    }
}

impl SimRegMap {
    /// R-151cd close path: MCP and MCDA go to zero and no further event is
    /// delivered to the previous host.
    fn close_message_channel(&self) {
        let mut i = self.inner.lock().expect("sim regmap poisoned");
        Self::w32(&mut i.boot, reg::MESSAGE_CHANNEL_PORT, 0);
        Self::w32(&mut i.boot, reg::MESSAGE_CHANNEL_DEST_ADDRESS, 0);
        i.message_dest = None;
    }

    /// PACKETRESEND (§16.7.2): serve the request from the channel's
    /// last-block cache (or answer GEV_STATUS_PACKET_UNAVAILABLE when the
    /// block is gone — CR-205st).
    pub fn packet_resend(&self, channel: u16, block_id: u64, first: u32, last: u32) {
        // A resend request is a valid GVCP command from the primary
        // application, so it also refreshes the heartbeat (R-075cd).
        self.scheduler.packet_resend(channel, block_id, first, last);
    }

    /// Test hook (§16.7 regression): start dropping the given payload
    /// packets on send (leader/trailer are never dropped). The set is
    /// shared with the test, which typically inserts (block_id,
    /// packet_id) pairs before starting acquisition.
    pub fn set_packet_drop(&self, drops: Arc<Mutex<BTreeSet<(u64, u32)>>>) {
        self.scheduler.set_packet_drop(Some(drops));
    }

    /// Test hook (§11.7 fallback regression): stop emitting test packets.
    /// The SCPSx writes still succeed, so the producer's search times out
    /// and must restore the factory/XML packet size.
    pub fn set_test_packets(&self, enabled: bool) {
        self.scheduler.set_test_packets(enabled);
    }

    /// Test hook (§11.7 fallback regression): allow plain SCPSx writes
    /// while the channel streams. A real 1.2 camera accepts them in the
    /// post-open negotiation window; the default sim block makes the
    /// successful fallback path unreachable.
    pub fn set_block_scps_writes(&self, block: bool) {
        self.block_scps_writes.store(block, Ordering::Relaxed);
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
        if off == reg::SCC_OFFSET {
            // §36.62/CR-494cd: mandatory capability register, read-only. No
            // optional per-channel feature is implemented, so it reads 0.
            return Err(GEV_STATUS_WRITE_PROTECT);
        }
        if off == reg::SCCFG_OFFSET {
            // §36.63/CR-495cd: mandatory configuration register. R-590cd
            // disables write access while streaming; with no capability bit
            // set every configuration bit is reserved, so a non-zero write
            // is an invalid parameter.
            if self.scheduler.is_streaming(x as u16) {
                return Err(GEV_STATUS_BUSY);
            }
            if value != 0 {
                return Err(GEV_STATUS_INVALID_PARAMETER);
            }
            self.inner.lock().expect("sim regmap poisoned").channels[x as usize][W_SCCFG] = 0;
            return Ok(());
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
    /// fire_test_packet (§11.7/§24.5, docs/00-governance/OPEN-QUESTIONS.md Q33): a
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
        if !fire
            && self.block_scps_writes.load(Ordering::Relaxed)
            && self.scheduler.is_streaming(x as u16)
        {
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
            // channel (docs/00-governance/OPEN-QUESTIONS.md Q14/Q31).
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
                let user_word = |reg_addr: u32| {
                    u32::from_be_bytes(
                        i.user[(reg_addr - USER_BASE) as usize..][..4]
                            .try_into()
                            .expect("u32"),
                    )
                };
                let pattern = PatternConfig {
                    test_pattern: user_word(REG_TEST_PATTERN),
                    reverse_x: user_word(REG_REVERSE_X) != 0,
                    reverse_y: user_word(REG_REVERSE_Y) != 0,
                };
                (
                    SocketAddr::from((Ipv4Addr::from(words[W_SCDA]), port)),
                    StreamParams::new(&self.stream, words[W_SCPS], words[W_SCPD])
                        .with_geometry(
                            user_word(REG_WIDTH),
                            user_word(REG_HEIGHT),
                            user_word(REG_PIXEL_FORMAT),
                        )
                        .with_pattern(pattern),
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

    /// Test hook (§11.7 fallback regression): emit test packets or not.
    pub fn set_test_packets(&self, enabled: bool) {
        self.map.set_test_packets(enabled);
    }

    /// Test hook (§11.7 fallback regression): block plain SCPSx writes
    /// while streaming, or allow them in the negotiation window.
    pub fn set_block_scps_writes(&self, block: bool) {
        self.map.set_block_scps_writes(block);
    }
}

impl DeviceHandler for SimDevice {
    fn discovery_info(&self) -> DiscoveryInfo {
        self.map.discovery_info()
    }
    fn read_reg(&self, addr: u32) -> Result<u32, u16> {
        self.map.read_reg(addr)
    }
    fn read_reg_from(&self, peer: SocketAddr, addr: u32) -> Result<u32, u16> {
        self.map.read_reg_from(peer, addr)
    }
    fn write_reg(&self, addr: u32, value: u32) -> Result<(), u16> {
        self.map.write_reg(addr, value)
    }
    fn write_reg_from(&self, peer: SocketAddr, addr: u32, value: u32) -> Result<(), u16> {
        self.map.write_reg_from(peer, addr, value)
    }
    fn read_mem(&self, addr: u32, count: u16) -> Result<Vec<u8>, u16> {
        self.map.read_mem(addr, count)
    }
    fn read_mem_from(&self, peer: SocketAddr, addr: u32, count: u16) -> Result<Vec<u8>, u16> {
        self.map.read_mem_from(peer, addr, count)
    }
    fn write_mem(&self, addr: u32, data: &[u8]) -> Result<(), u16> {
        self.map.write_mem(addr, data)
    }
    fn write_mem_from(&self, peer: SocketAddr, addr: u32, data: &[u8]) -> Result<(), u16> {
        self.map.write_mem_from(peer, addr, data)
    }
    fn write_mem_from_progress(
        &self,
        peer: SocketAddr,
        addr: u32,
        data: &[u8],
    ) -> Result<(), (u16, u32)> {
        self.map.write_mem_from_progress(peer, addr, data)
    }
    fn force_ip(
        &self,
        peer: SocketAddr,
        request: gentl_gvcp::wire::ForceIpRequest,
    ) -> gentl_gvcp::wire::ForceIpOutcome {
        self.map.force_ip(peer, request)
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
        // R-070cd: the primary switches privilege without closing the channel.
        m.write_reg(reg::CCP, reg::CCP_CONTROL_ACCESS).unwrap();
        assert_eq!(
            m.read_reg(reg::CCP).unwrap(),
            reg::CCP_CONTROL_ACCESS | (1 << reg::CCP_CONNECTION_COUNT_SHIFT)
        );
        m.write_reg(reg::CCP, reg::CCP_EXCLUSIVE_ACCESS).unwrap();
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
            chunk: None,
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

    /// §36.62/§36.63 (CR-494cd/CR-495cd): SCCx/SCCFGx are mandatory for
    /// GVSP devices. The sim supports no optional per-channel feature, so
    /// SCC0 reads 0 and SCCFG0 accepts only a zero write; R-590cd disables
    /// the write while streaming.
    #[test]
    fn stream_channel_capability_and_config_registers() {
        let m = test_map();
        assert_eq!(m.read_reg(reg::SCC0).unwrap(), 0);
        assert_eq!(m.read_reg(reg::SCCFG0).unwrap(), 0);
        // Write-protected capability register.
        assert_eq!(
            m.write_reg(reg::SCC0, 0).unwrap_err(),
            GEV_STATUS_WRITE_PROTECT
        );
        // Reserved configuration bits: only zero is accepted.
        m.write_reg(reg::SCCFG0, 0).unwrap();
        assert_eq!(
            m.write_reg(reg::SCCFG0, 1).unwrap_err(),
            GEV_STATUS_INVALID_PARAMETER
        );
        // R-590cd: write access is disabled during streaming.
        m.write_reg(reg::SCP0, reg::scp_make(3956)).unwrap();
        assert_eq!(m.write_reg(reg::SCCFG0, 0).unwrap_err(), GEV_STATUS_BUSY);
        m.write_reg(reg::SCP0, 0).unwrap();
        m.write_reg(reg::SCCFG0, 0).unwrap();
        // READMEM sees the same values.
        assert_eq!(m.read_mem(reg::SCC0, 4).unwrap(), vec![0, 0, 0, 0]);
        assert_eq!(m.read_mem(reg::SCCFG0, 4).unwrap(), vec![0, 0, 0, 0]);
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
            m.read_reg(USER_BASE + USER_LEN).unwrap_err(),
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
        // Reserved protocol values 2-3 are rejected (docs/00-governance/OPEN-QUESTIONS.md Q29).
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

    /// R-075cd/R-076cd (§10.5): the device closes the control channel after
    /// 3x the heartbeat timeout without primary traffic, and any valid
    /// command from the primary refreshes the window.
    #[test]
    fn heartbeat_timeout_closes_the_control_channel() {
        let m = test_map();
        m.write_reg(reg::HEARTBEAT_TIMEOUT, 500).unwrap();
        m.write_reg(reg::CCP, reg::CCP_EXCLUSIVE_ACCESS).unwrap();
        assert_eq!(
            m.read_reg(reg::CCP).unwrap() & reg::CCP_EXCLUSIVE_ACCESS,
            reg::CCP_EXCLUSIVE_ACCESS
        );

        // Keep-alive traffic inside the 500 ms window keeps the channel open;
        // any command counts, not just a CCP read (R-075cd).
        for _ in 0..3 {
            std::thread::sleep(Duration::from_millis(200));
            let read = m.read_reg(reg::USER_DEFINED_NAME).unwrap();
            let _ = read;
            assert_eq!(
                m.read_reg(reg::CCP).unwrap() & reg::CCP_EXCLUSIVE_ACCESS,
                reg::CCP_EXCLUSIVE_ACCESS
            );
        }

        // Open the message/stream channels so the R-072cd cleanup is
        // observable after the heartbeat closes the control channel.
        m.write_reg(reg::MESSAGE_CHANNEL_DEST_ADDRESS, u32::from(Ipv4Addr::LOCALHOST))
            .unwrap();
        m.write_reg(reg::MESSAGE_CHANNEL_PORT, reg::mcp_make(40000))
            .unwrap();
        m.write_reg(reg::SCDA0, u32::from(Ipv4Addr::LOCALHOST)).unwrap();
        m.write_reg(reg::SCP0, reg::scp_make(40001)).unwrap();

        // Idle past the window: the next command observes the closed channel
        // (CCP reads 0) and a new grant succeeds.
        std::thread::sleep(Duration::from_millis(1600));
        assert_eq!(
            m.read_reg(reg::CCP).unwrap()
                & (reg::CCP_EXCLUSIVE_ACCESS | reg::CCP_CONTROL_ACCESS),
            0
        );
        assert_eq!(m.read_reg(reg::MESSAGE_CHANNEL_PORT).unwrap(), 0);
        assert_eq!(m.read_reg(reg::SCP0).unwrap(), 0);
        m.write_reg(reg::CCP, reg::CCP_EXCLUSIVE_ACCESS).unwrap();
        assert_eq!(
            m.read_reg(reg::CCP).unwrap() & reg::CCP_EXCLUSIVE_ACCESS,
            reg::CCP_EXCLUSIVE_ACCESS
        );
    }

    /// §16.2 FORCEIP policy: another MAC is discarded (R-149cd), an occupied
    /// control channel is discarded (R-150cd), static_IP 0 restarts the IP
    /// cycle without an ACK (R-151cd), and a forced address is applied and
    /// acknowledged (R-152cd).
    #[test]
    fn force_ip_policy() {
        use gentl_gvcp::wire::{ForceIpOutcome, ForceIpRequest};
        let m = test_map();
        let peer = SocketAddr::from((Ipv4Addr::LOCALHOST, 3956));
        let mac = m.config.mac;
        let mut request = ForceIpRequest {
            mac,
            static_ip: u32::from(Ipv4Addr::new(10, 0, 0, 100)),
            subnet_mask: u32::from(Ipv4Addr::new(255, 255, 255, 0)),
            default_gateway: u32::from(Ipv4Addr::new(10, 0, 0, 1)),
        };
        // R-149cd: a request addressed to another device's MAC changes
        // nothing.
        let other_mac = ForceIpRequest {
            mac: [0, 0, 0, 0, 0, 1],
            ..request
        };
        assert_eq!(m.force_ip(peer, other_mac), ForceIpOutcome::Ignore);
        assert_eq!(
            m.read_reg(reg::CURRENT_IP).unwrap(),
            u32::from(m.config.ip)
        );
        // R-150cd: while an application holds the control channel the
        // command is silently discarded.
        m.write_reg(reg::CCP, reg::CCP_EXCLUSIVE_ACCESS).unwrap();
        assert_eq!(m.force_ip(peer, request), ForceIpOutcome::Ignore);
        m.write_reg(reg::CCP, 0).unwrap();
        // R-151cd: static_IP == 0 restarts the IP configuration without an
        // ACK (the device keeps its configured address here).
        let restart = ForceIpRequest { static_ip: 0, ..request };
        assert_eq!(m.force_ip(peer, restart), ForceIpOutcome::Ignore);
        // R-152cd: a non-zero address is applied and acknowledged.
        assert_eq!(m.force_ip(peer, request), ForceIpOutcome::Ack);
        assert_eq!(
            m.read_reg(reg::CURRENT_IP).unwrap(),
            u32::from(Ipv4Addr::new(10, 0, 0, 100))
        );
        assert_eq!(
            m.read_reg(reg::CURRENT_SUBNET_MASK).unwrap(),
            u32::from(Ipv4Addr::new(255, 255, 255, 0))
        );
        assert_eq!(
            m.read_reg(reg::CURRENT_DEFAULT_GATEWAY).unwrap(),
            u32::from(Ipv4Addr::new(10, 0, 0, 1))
        );
        request.static_ip = u32::from(Ipv4Addr::new(10, 0, 0, 150));
        assert_eq!(m.force_ip(peer, request), ForceIpOutcome::Ack);
    }

    /// The GenTL XML fetch rounds the last READMEM chunk up to 4 bytes; a
    /// non-aligned XML length must therefore be padded, not rejected.
    #[test]
    fn xml_read_rounded_past_the_blob_end_is_zero_padded() {
        let m = test_map();
        let len = m.xml.len() as u32;
        let padded = len.next_multiple_of(4);
        if padded == len {
            return; // fixture already aligned; nothing to prove
        }
        let block = m.read_mem(XML_BASE, padded as u16).unwrap();
        assert_eq!(&block[..len as usize], m.xml.as_slice());
        assert_eq!(&block[len as usize..], &vec![0u8; (padded - len) as usize][..]);
    }

    /// §16.6 WRITEMEM (R-185cd: mandatory): word writes go through the
    /// register model; misalignment is BAD_ALIGNMENT (R-188cd/R-189c) and the
    /// XML description is read-only.
    #[test]
    fn write_mem_applies_words_and_enforces_alignment() {
        let m = test_map();
        let mut block = [0u8; 8];
        block[..4].copy_from_slice(&1280u32.to_be_bytes());
        block[4..].copy_from_slice(&960u32.to_be_bytes());
        m.write_mem(crate::REG_WIDTH, &block).unwrap();
        assert_eq!(m.read_reg(crate::REG_WIDTH).unwrap(), 1280);
        assert_eq!(m.read_reg(crate::REG_HEIGHT).unwrap(), 960);
        assert_eq!(
            m.write_mem(crate::REG_WIDTH, &[0u8; 6]),
            Err(GEV_STATUS_BAD_ALIGNMENT),
            "R-188cd: length not a multiple of 4"
        );
        assert_eq!(
            m.write_mem(crate::REG_WIDTH + 2, &[0u8; 4]),
            Err(GEV_STATUS_BAD_ALIGNMENT),
            "R-189c: unaligned address"
        );
        assert_eq!(
            m.write_mem(XML_BASE, &[0u8; 4]),
            Err(GEV_STATUS_WRITE_PROTECT),
            "the XML blob is read-only"
        );
    }

    /// [R-029cd]: LLA MUST always stay activated; writing 0 must not clear
    /// the configuration bit.
    #[test]
    fn lla_cannot_be_disabled() {
        let m = test_map();
        m.write_reg(reg::NET_IF_CONFIGURATION, 0).unwrap();
        let cfg = m.read_reg(reg::NET_IF_CONFIGURATION).unwrap();
        assert_eq!(cfg & reg::IP_CFG_LLA, reg::IP_CFG_LLA, "LLA always 1");
        assert_eq!(
            cfg & (reg::IP_CFG_PERSISTENT_IP | reg::IP_CFG_DHCP),
            0,
            "persistent/DHCP are user configurable"
        );
        m.write_reg(
            reg::NET_IF_CONFIGURATION,
            reg::IP_CFG_PERSISTENT_IP | reg::IP_CFG_DHCP | reg::IP_CFG_LLA,
        )
        .unwrap();
        assert_eq!(
            m.read_reg(reg::NET_IF_CONFIGURATION).unwrap(),
            reg::IP_CFG_PERSISTENT_IP | reg::IP_CFG_DHCP | reg::IP_CFG_LLA
        );
    }

    /// [R-151cd]: a FORCEIP restart (static_IP == 0) must also close the
    /// message channel: MCP/MCDA read 0 and the old host receives no more
    /// events.
    #[test]
    fn force_ip_restart_closes_the_message_channel() {
        let m = test_map();
        let rx = std::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        rx.set_read_timeout(Some(std::time::Duration::from_millis(500)))
            .ok();
        m.write_reg(
            reg::MESSAGE_CHANNEL_DEST_ADDRESS,
            u32::from(Ipv4Addr::LOCALHOST),
        )
        .unwrap();
        m.write_reg(
            reg::MESSAGE_CHANNEL_PORT,
            reg::mcp_make(rx.local_addr().unwrap().port()),
        )
        .unwrap();
        m.write_reg(REG_TEST_EVENT_ID, 7).unwrap();
        m.write_reg(REG_TEST_EVENT_DATA_LEN, 0).unwrap();
        m.write_reg(REG_TEST_EVENT_TRIGGER, 1).unwrap();
        let mut buf = [0u8; 512];
        assert!(rx.recv_from(&mut buf).is_ok(), "event before FORCEIP");

        let peer = SocketAddr::from((Ipv4Addr::LOCALHOST, 1234));
        let request = ForceIpRequest {
            mac: m.config.mac,
            static_ip: 0,
            subnet_mask: 0,
            default_gateway: 0,
        };
        let outcome = gentl_gvcp::server::DeviceHandler::force_ip(&m, peer, request);
        assert_eq!(outcome, gentl_gvcp::wire::ForceIpOutcome::Ignore);
        assert_eq!(m.read_reg(reg::MESSAGE_CHANNEL_PORT).unwrap(), 0);
        assert_eq!(m.read_reg(reg::MESSAGE_CHANNEL_DEST_ADDRESS).unwrap(), 0);

        m.write_reg(REG_TEST_EVENT_TRIGGER, 1).unwrap();
        rx.set_read_timeout(Some(std::time::Duration::from_millis(300)))
            .ok();
        assert!(
            rx.recv_from(&mut buf).is_err(),
            "no events after the FORCEIP restart closed the message channel"
        );
    }

    /// GigE Vision 10.1 access rules and R-072cd release cleanup:
    /// - exclusive access denies every other application (R-063cd);
    /// - the primary switches EA<->CA in place (R-070cd);
    /// - control access lets secondaries read but not write (R-170cd);
    /// - releasing the channel clears MCP and SCPx (R-072cd).
    #[test]
    fn peer_permissions_and_ccp_release_cleanup() {
        use gentl_gvcp::server::DeviceHandler;
        let m = test_map();
        let p1 = SocketAddr::from((Ipv4Addr::LOCALHOST, 40001));
        let p2 = SocketAddr::from((Ipv4Addr::LOCALHOST, 40002));

        DeviceHandler::write_reg_from(&m, p1, reg::CCP, reg::CCP_EXCLUSIVE_ACCESS)
            .expect("first application gets EA");
        assert_eq!(
            DeviceHandler::read_reg_from(&m, p2, reg::VERSION),
            Err(GEV_STATUS_ACCESS_DENIED),
            "R-063cd: exclusive access denies secondary reads"
        );
        assert_eq!(
            DeviceHandler::write_reg_from(&m, p2, REG_EXPOSURE_TIME, 1),
            Err(GEV_STATUS_ACCESS_DENIED)
        );

        // R-070cd: the primary switches to CA without releasing.
        DeviceHandler::write_reg_from(&m, p1, reg::CCP, reg::CCP_CONTROL_ACCESS)
            .expect("EA -> CA switch");
        assert!(
            DeviceHandler::read_reg_from(&m, p2, reg::VERSION).is_ok(),
            "control access allows secondary reads"
        );
        assert_eq!(
            DeviceHandler::write_reg_from(&m, p2, REG_EXPOSURE_TIME, 1),
            Err(GEV_STATUS_ACCESS_DENIED),
            "R-170cd: only the primary may write"
        );

        // Open the message and stream channels as the primary, then release.
        DeviceHandler::write_reg_from(&m, p1, reg::MESSAGE_CHANNEL_DEST_ADDRESS, u32::from(Ipv4Addr::LOCALHOST)).unwrap();
        DeviceHandler::write_reg_from(&m, p1, reg::MESSAGE_CHANNEL_PORT, reg::mcp_make(40000)).unwrap();
        DeviceHandler::write_reg_from(&m, p1, reg::SCDA0, u32::from(Ipv4Addr::LOCALHOST)).unwrap();
        DeviceHandler::write_reg_from(&m, p1, reg::SCP0, reg::scp_make(40001)).unwrap();
        DeviceHandler::write_reg_from(&m, p1, reg::CCP, 0).expect("primary releases");

        assert_eq!(
            DeviceHandler::read_reg_from(&m, p2, reg::MESSAGE_CHANNEL_PORT).unwrap(),
            0,
            "R-072cd: MCP cleared on control-channel close"
        );
        assert_eq!(
            DeviceHandler::read_reg_from(&m, p2, reg::SCP0).unwrap(),
            0,
            "R-072cd: SCPx cleared on control-channel close"
        );
    }

    /// R-194cd: when a later word fails, earlier words stay committed and the
    /// progress API reports the failing byte offset.
    #[test]
    fn writemem_reports_the_failed_word_offset() {
        let m = test_map();
        // First word is the writable CCP register; 0x0A04 is reserved.
        let data = [0u8, 0, 0, 1, 0, 0, 0, 0];
        assert_eq!(
            m.write_mem_inner(reg::CCP, &data),
            Err((GEV_STATUS_INVALID_ADDRESS, 4)),
            "the second word fails at byte offset 4"
        );
        assert_eq!(
            m.read_reg(reg::CCP).unwrap() & reg::CCP_EXCLUSIVE_ACCESS,
            reg::CCP_EXCLUSIVE_ACCESS,
            "the first word stays committed"
        );
    }

    /// R-076cd/R-150cd: FORCEIP must observe an expired heartbeat, not a
    /// stale cached grant, and the expiry cleanup closes streams/channels.
    #[test]
    fn force_ip_sees_an_expired_heartbeat() {
        let m = test_map();
        m.write_reg(reg::HEARTBEAT_TIMEOUT, 500).unwrap();
        m.write_reg(reg::CCP, reg::CCP_EXCLUSIVE_ACCESS).unwrap();
        m.write_reg(reg::MESSAGE_CHANNEL_DEST_ADDRESS, u32::from(Ipv4Addr::LOCALHOST))
            .unwrap();
        m.write_reg(reg::MESSAGE_CHANNEL_PORT, reg::mcp_make(40000))
            .unwrap();
        m.write_reg(reg::SCDA0, u32::from(Ipv4Addr::LOCALHOST)).unwrap();
        m.write_reg(reg::SCP0, reg::scp_make(40001)).unwrap();
        std::thread::sleep(Duration::from_millis(600));
        let request = ForceIpRequest {
            mac: m.config.mac,
            static_ip: 0,
            subnet_mask: 0,
            default_gateway: 0,
        };
        let peer = SocketAddr::from((Ipv4Addr::LOCALHOST, 3956));
        assert_eq!(m.force_ip(peer, request), ForceIpOutcome::Ignore);
        assert_eq!(
            m.read_reg(reg::CCP).unwrap()
                & (reg::CCP_EXCLUSIVE_ACCESS | reg::CCP_CONTROL_ACCESS),
            0,
            "stale grant expired before the FORCEIP privilege check"
        );
        assert_eq!(m.read_reg(reg::MESSAGE_CHANNEL_PORT).unwrap(), 0);
        assert_eq!(m.read_reg(reg::SCP0).unwrap(), 0);
    }

    #[test]
    fn pending_timeout_register_is_writable() {
        let m = test_map();
        assert_eq!(m.read_reg(reg::PENDING_TIMEOUT).unwrap(), 1000);
        m.write_reg(reg::PENDING_TIMEOUT, 500).unwrap();
        assert_eq!(m.read_reg(reg::PENDING_TIMEOUT).unwrap(), 500);
    }
}
