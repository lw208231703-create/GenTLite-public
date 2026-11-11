//! vgige-front-gige: the GigE Vision front-end — the virtual device a
//! remote client sees. [`GigEFront`] adapts [`RegisterSpace`] to
//! `gentl_gvcp`'s [`DeviceHandler`] (discovery identity, register/memory
//! dispatch) for [`GvcpServer`]; [`GvspSession`] is the [`FrameSession`]
//! that re-emits relayed frames as GigE Vision 3.0 GVSP blocks (20-byte
//! extended header, 40-byte leader body, 8-byte trailer), block_id from 1
//! per channel open (R-259st), sliced to the virtual SCPS0 (§36.58
//! total-size semantics). The virtual device claims GigE Vision 3.0 to
//! match that wire form.
//!
//! Roadmap: a USB3 Vision gadget front (Step 3) implements `FrameSession`
//! against a gadget device and owns a different device model — it will not
//! use [`RegisterSpace`] (no GVCP register space on USB), which is why the
//! front is a crate, not a trait hierarchy.

use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gentl_core::GentlResult;
use gentl_gvcp::gvsp::*;
use gentl_gvcp::registers as reg;
use gentl_gvcp::wire::*;
use gentl_gvcp::DeviceHandler;
use vgige_core::{
    packet_delay, virtual_payload_chunk, CanonicalFrame, FrameSession, RegisterSpace, SharedBack,
    StreamRegs,
};

/// Identity overrides for the up-facing NIC: the virtual device claims
/// this IP and MAC (the down camera's stay hidden from the client).
/// `mask` is the up subnet — the virtual device lives on the up network,
/// so its discovery payload carries the up subnet (the camera's own
/// network stays invisible to the client).
pub struct UpIdentity {
    pub ip: Ipv4Addr,
    pub mac: [u8; 6],
    pub mask: Ipv4Addr,
}

/// Assemble the virtual discovery payload: the camera's identity strings
/// via the back's native bootstrap ([`CameraBack::fetch_identity`]), the
/// up-facing IP/MAC/mask applied, and the network-shaped fields the
/// virtual device owns synthesized:
/// - `spec_version`: the virtual device emits the GigE Vision 3.0 extended
///   GVSP form (20-byte header, 40-byte leader body), so it claims 3.0.
/// - `device_mode`: big-endian + UTF-8 + transmitter. In 1.2 printed bit 0
///   is the endianess bit (1 = big-endian) and printed bits 24-31 are the
///   character-set index (1 = UTF-8); the same MSB stays the 2.x/3.0
///   deprecated always-1 bit.
/// - `ip_config_options/current`: the up NIC is configured by the
///   gateway host, so the claim is persistent-IP + LLA / persistent-IP
///   (§36.5 values; there is no camera NIC to mirror — a U3V back has no
///   network at all);
/// - `default_gateway`: not modeled, 0 (QoI — informational in
///   DISCOVERY_ACK only).
pub fn assemble_identity(back: &SharedBack, up: &UpIdentity) -> GentlResult<DiscoveryInfo> {
    let id = back
        .lock()
        .map_err(|_| gentl_core::GentlError::Io("back lock poisoned".into()))?
        .fetch_identity()
        .map_err(gentl_core::GentlError::GevStatus)?;
    Ok(DiscoveryInfo {
        // The GVSP front is 3.0-form; the downstream camera's native
        // version is not what the virtual wire speaks.
        spec_version_major: 3,
        spec_version_minor: 0,
        device_mode: 0x8000_0001,
        mac: up.mac,
        ip_config_options: reg::IP_CFG_PERSISTENT_IP | reg::IP_CFG_LLA,
        ip_config_current: reg::IP_CFG_PERSISTENT_IP,
        current_ip: u32::from(up.ip),
        current_subnet_mask: u32::from(up.mask),
        default_gateway: 0,
        manufacturer_name: id.manufacturer,
        model_name: id.model,
        device_version: id.device_version,
        manufacturer_info: String::new(),
        serial_number: id.serial_number,
        user_defined_name: id.user_defined_name,
    })
}

/// The virtual device handed to `gentl_gvcp::GvcpServer`: discovery
/// identity plus register-space dispatch. All behavioral semantics live in
/// [`RegisterSpace`] (vgige-core). Cloneable: the server owns one instance,
/// the assembler keeps a clone for ordered shutdown.
#[derive(Clone)]
pub struct GigEFront {
    identity: DiscoveryInfo,
    space: RegisterSpace,
    pending_test_packet: Arc<Mutex<Option<u32>>>,
}

impl GigEFront {
    pub fn new(identity: DiscoveryInfo, space: RegisterSpace) -> Self {
        Self {
            identity,
            space,
            pending_test_packet: Arc::new(Mutex::new(None)),
        }
    }

    fn maybe_fire_test_packet(&self) {
        let mut pending = match self.pending_test_packet.lock() {
            Ok(p) => p,
            Err(_) => return,
        };
        let Some(size) = *pending else { return };
        let scp = self.space.read_reg(reg::SCP0).unwrap_or(0);
        let scda = self.space.read_reg(reg::SCDA0).unwrap_or(0);
        let port = reg::scp_host_port(scp);
        if port == 0 || scda == 0 {
            return;
        }
        if let Some(packet) = build_test_packet(size) {
            if let Ok(socket) = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)) {
                let _ = socket.send_to(
                    &packet,
                    SocketAddr::from((Ipv4Addr::from(scda), port)),
                );
            }
        }
        *pending = None;
    }
}

impl DeviceHandler for GigEFront {
    fn discovery_info(&self) -> DiscoveryInfo {
        self.identity.clone()
    }

    fn read_reg(&self, addr: u32) -> Result<u32, u16> {
        self.space.read_reg(addr)
    }

    fn write_reg(&self, addr: u32, value: u32) -> Result<(), u16> {
        self.write_reg_from(
            SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)),
            addr,
            value,
        )
    }

    fn write_reg_from(&self, peer: SocketAddr, addr: u32, value: u32) -> Result<(), u16> {
        let result = self.space.write_reg_from(peer, addr, value);
        if result.is_ok() {
            if addr == reg::SCPS0 && value & reg::SCPS_FIRE_TEST_PACKET != 0 {
                if let Ok(mut pending) = self.pending_test_packet.lock() {
                    *pending = Some(reg::scps_packet_size(value));
                }
            }
            if addr == reg::SCPS0 || addr == reg::SCP0 || addr == reg::SCDA0 {
                self.maybe_fire_test_packet();
            }
        }
        result
    }

    fn read_mem(&self, addr: u32, count: u16) -> Result<Vec<u8>, u16> {
        self.space.read_block(addr, count)
    }

    fn write_mem(&self, addr: u32, data: &[u8]) -> Result<(), u16> {
        self.space.write_block(addr, data)
    }
}

fn build_test_packet(total: u32) -> Option<Vec<u8>> {
    let total = total as usize;
    if total < 36 {
        return None;
    }
    let mut packet = vec![0u8; total - 28];
    let mut header = [0u8; GVSP_LEGACY_HEADER_LEN];
    GvspHeader {
        status: 0,
        flag: 0,
        extended_id: false,
        packet_format: 0,
        block_id: 0,
        packet_id: 0,
    }
    .encode_legacy(&mut header);
    packet[..GVSP_LEGACY_HEADER_LEN].copy_from_slice(&header);
    Some(packet)
}

/// GVSP 3.0 encoding session: one UDP sender socket plus the virtual
/// channel's register file (destination and slice size are re-read per
/// frame; SCPS0 cannot change once the first frame was sent, §36.58).
pub struct GvspSession {
    socket: Option<UdpSocket>,
    regs: Arc<Mutex<StreamRegs>>,
    /// Per-channel-open block sequence (R-259st: restarts at 1).
    next_block_id: u64,
    /// Emit the 1.2 legacy 8-byte-header form instead of the 3.0 extended
    /// form. The default is 3.0; the virtual device advertises 3.0, so the
    /// two must not be mixed on one channel.
    legacy: bool,
    scratch: Vec<u8>,
}

impl GvspSession {
    pub fn new(regs: Arc<Mutex<StreamRegs>>) -> Self {
        Self {
            socket: None,
            regs,
            next_block_id: 1,
            legacy: false,
            scratch: Vec::with_capacity(65_535),
        }
    }
}

impl FrameSession for GvspSession {
    fn open(&mut self, _payload_hint: usize) -> Result<(), String> {
        // The GVSP front re-reads the virtual SCPS0 every send_frame, but
        // the relay rejects SCPS writes once transmission is active
        // (§36.58), so the value is fixed for the lifetime of the send.
        // The open-time hint is informational here; fronts with a fixed
        // slice (a U3V gadget MTU) consume it directly — that second use is
        // why the parameter exists on the trait.
        let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).map_err(|e| e.to_string())?;
        self.socket = Some(socket);
        Ok(())
    }

    fn send_frame(&mut self, frame: CanonicalFrame) {
        let Some(socket) = &self.socket else { return };
        let (target, chunk, delay) = {
            let regs = self.regs.lock().expect("vgige relay regs poisoned");
            let target =
                SocketAddr::from((Ipv4Addr::from(regs.scda), reg::scp_host_port(regs.scp)));
            let chunk = if self.legacy {
                reg::scps_packet_size(regs.scps)
                    .checked_sub(GVSP_LEGACY_HEADERS_OVERHEAD)
                    .filter(|c| *c >= 1)
                    .unwrap_or(1500 - GVSP_LEGACY_HEADERS_OVERHEAD) as usize
            } else {
                virtual_payload_chunk(regs.scps)
            };
            let delay = packet_delay(regs.scpd, regs.tick_freq);
            (target, chunk, delay)
        };
        if target.port() == 0 {
            return; // channel closing; nothing to address.
        }
        if self.legacy {
            send_block_legacy(
                socket,
                target,
                self.next_block_id,
                &frame,
                chunk,
                delay,
                &mut self.scratch,
            );
        } else {
            send_block(
                socket,
                target,
                self.next_block_id,
                &frame,
                chunk,
                delay,
                &mut self.scratch,
            );
        }
        self.next_block_id += 1;
    }

    fn close(&mut self) {
        self.socket = None;
    }
}

/// Emit one block: 3.0 20-byte EI=1 headers (§24.2), leader body in the
/// 40-byte §25.2.1 layout from the canonical metadata, payload bytes
/// verbatim, 8-byte §25.2.3 trailer body.
fn send_block(
    socket: &UdpSocket,
    target: SocketAddr,
    block_id: u64,
    frame: &CanonicalFrame,
    chunk: usize,
    delay: Duration,
    scratch: &mut Vec<u8>,
) {
    let leader = ImageLeader {
        field_id: 0,
        field_count: 0,
        timestamp: frame.timestamp,
        pixel_format: frame.pixel_format,
        size_x: frame.width,
        size_y: frame.height,
        offset_x: frame.offset_x,
        offset_y: frame.offset_y,
        padding_x: frame.padding_x,
        padding_y: frame.padding_y,
    };
    scratch.clear();
    push_header(scratch, PACKET_FORMAT_DATA_LEADER, block_id, 0);
    scratch.extend_from_slice(&encode_image_leader_body(&leader));
    if socket.send_to(scratch, target).is_err() {
        return;
    }
    let packet_id = send_payloads(
        socket,
        target,
        block_id,
        &frame.payload,
        chunk,
        delay,
        false,
        scratch,
    );
    scratch.clear();
    push_header(scratch, PACKET_FORMAT_DATA_TRAILER, block_id, packet_id);
    scratch.extend_from_slice(&encode_image_trailer_body(frame.height));
    let _ = socket.send_to(scratch, target);
}

/// Emit one block in the 1.2 legacy form: 8-byte common header, 36-byte
/// leader body, payload, 8-byte trailer body. This is a fallback path only:
/// the virtual device advertises 3.0 and uses [`send_block`] by default, so
/// a session must not mix the two forms.
fn send_block_legacy(
    socket: &UdpSocket,
    target: SocketAddr,
    block_id: u64,
    frame: &CanonicalFrame,
    chunk: usize,
    delay: Duration,
    scratch: &mut Vec<u8>,
) {
    let leader = ImageLeader {
        field_id: 0,
        field_count: 0,
        timestamp: frame.timestamp,
        pixel_format: frame.pixel_format,
        size_x: frame.width,
        size_y: frame.height,
        offset_x: frame.offset_x,
        offset_y: frame.offset_y,
        padding_x: frame.padding_x,
        padding_y: frame.padding_y,
    };
    scratch.clear();
    push_header_legacy(scratch, PACKET_FORMAT_DATA_LEADER, block_id, 0);
    scratch.extend_from_slice(&encode_image_leader_body_legacy(&leader));
    if socket.send_to(scratch, target).is_err() {
        return;
    }
    let packet_id = send_payloads(
        socket,
        target,
        block_id,
        &frame.payload,
        chunk,
        delay,
        true,
        scratch,
    );
    scratch.clear();
    push_header_legacy(scratch, PACKET_FORMAT_DATA_TRAILER, block_id, packet_id);
    scratch.extend_from_slice(&encode_image_trailer_body(frame.height));
    let _ = socket.send_to(scratch, target);
}

/// Send the payload packets of one block, honoring the SCPD inter-packet
/// delay (§36.59: delay expressed in timestamp ticks, converted by
/// [`packet_delay`]). Returns the next packet id (the trailer's).
#[allow(clippy::too_many_arguments)]
fn send_payloads(
    socket: &UdpSocket,
    target: SocketAddr,
    block_id: u64,
    payload: &[u8],
    chunk: usize,
    delay: Duration,
    legacy: bool,
    scratch: &mut Vec<u8>,
) -> u32 {
    let mut packet_id = 1u32;
    let mut chunks = payload.chunks(chunk).peekable();
    while let Some(data) = chunks.next() {
        scratch.clear();
        if legacy {
            push_header_legacy(scratch, PACKET_FORMAT_DATA_PAYLOAD, block_id, packet_id);
        } else {
            push_header(scratch, PACKET_FORMAT_DATA_PAYLOAD, block_id, packet_id);
        }
        scratch.extend_from_slice(data);
        if socket.send_to(scratch, target).is_err() {
            return packet_id;
        }
        packet_id += 1;
        // §36.59: delay *between* packets, not after the last one.
        if chunks.peek().is_some() && !delay.is_zero() {
            std::thread::sleep(delay);
        }
    }
    packet_id
}

/// Append a 1.2 legacy GVSP header (8 bytes).
fn push_header_legacy(pkt: &mut Vec<u8>, packet_format: u8, block_id: u64, packet_id: u32) {
    let mut hdr = [0u8; GVSP_LEGACY_HEADER_LEN];
    GvspHeader {
        status: GEV_STATUS_SUCCESS,
        flag: 0,
        extended_id: false,
        packet_format,
        block_id,
        packet_id,
    }
    .encode_legacy(&mut hdr);
    pkt.extend_from_slice(&hdr);
}

/// Append a 3.0 extended GVSP header (§24.2).
fn push_header(pkt: &mut Vec<u8>, packet_format: u8, block_id: u64, packet_id: u32) {
    let mut hdr = [0u8; GVSP_HEADER_LEN];
    GvspHeader {
        status: GEV_STATUS_SUCCESS,
        flag: 0,
        extended_id: true,
        packet_format,
        block_id,
        packet_id,
    }
    .encode(&mut hdr);
    pkt.extend_from_slice(&hdr);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_packet_has_the_requested_total_ip_length() {
        let packet = build_test_packet(1500).expect("test packet");
        assert_eq!(packet.len() + 28, 1500);
        let header = GvspHeader::decode_legacy(&packet).expect("legacy header");
        assert_eq!(header.block_id, 0);
        assert_eq!(header.packet_id, 0);
    }

    #[test]
    fn too_small_test_packet_is_rejected() {
        assert!(build_test_packet(35).is_none());
    }
}
