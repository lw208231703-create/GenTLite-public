//! vgige-front-gige: the GigE Vision front-end — the virtual device a
//! remote client sees. [`GigEFront`] adapts [`RegisterSpace`] to
//! `gentl_gvcp`'s [`DeviceHandler`] (discovery identity, register/memory
//! dispatch) for [`GvcpServer`]; [`GvspSession`] is the [`FrameSession`]
//! that re-emits relayed frames as GigE Vision 3.0-form GVSP blocks
//! (§24.2: 20-byte EI=1 headers, 40-byte §25.2.1 leader body, 8-byte
//! §25.2.3 trailer), block_id from 1 per channel open (R-259st), sliced to
//! the virtual SCPS0 (§36.58 total-size semantics).
//!
//! Roadmap: a USB3 Vision gadget front (Step 3) implements `FrameSession`
//! against a gadget device and owns a different device model — it will not
//! use [`RegisterSpace`] (no GVCP register space on USB), which is why the
//! front is a crate, not a trait hierarchy.

use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::{Arc, Mutex};

use gentl_core::GentlResult;
use gentl_gvcp::gvsp::*;
use gentl_gvcp::registers as reg;
use gentl_gvcp::wire::*;
use gentl_gvcp::DeviceHandler;
use vgige_core::{virtual_payload_chunk, CanonicalFrame, FrameSession, RegisterSpace, SharedBack, StreamRegs};

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
/// - `device_mode`: the virtual device is a GigE Vision 1.2/3.0-class
///   transmitter (same value the simulator publishes: deprecated
///   big-endian bit set per §36.2, device_class = Transmitter, UTF-8);
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
        spec_version_major: id.spec_version_major,
        spec_version_minor: id.spec_version_minor,
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
}

impl GigEFront {
    pub fn new(identity: DiscoveryInfo, space: RegisterSpace) -> Self {
        Self { identity, space }
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
        self.space.write_reg(addr, value)
    }

    fn read_mem(&self, addr: u32, count: u16) -> Result<Vec<u8>, u16> {
        self.space.read_block(addr, count)
    }
}

/// GVSP 3.0 encoding session: one UDP sender socket plus the virtual
/// channel's register file (destination and slice size are re-read per
/// frame, matching the v1 relay semantics where mid-stream SCPS0 writes
/// take effect on the next frame).
pub struct GvspSession {
    socket: Option<UdpSocket>,
    regs: Arc<Mutex<StreamRegs>>,
    /// Per-channel-open block sequence (R-259st: restarts at 1).
    next_block_id: u64,
}

impl GvspSession {
    pub fn new(regs: Arc<Mutex<StreamRegs>>) -> Self {
        Self {
            socket: None,
            regs,
            next_block_id: 1,
        }
    }
}

impl FrameSession for GvspSession {
    fn open(&mut self, _payload_hint: usize) -> Result<(), String> {
        // The GVSP front re-reads the virtual SCPS0 every send_frame (the
        // v1 semantics adopt mid-stream writes per frame), so the open-time
        // hint is informational here; fronts with a fixed slice (a U3V
        // gadget MTU) consume it directly — that second use is why the
        // parameter exists on the trait.
        let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).map_err(|e| e.to_string())?;
        self.socket = Some(socket);
        Ok(())
    }

    fn send_frame(&mut self, frame: &CanonicalFrame) {
        let Some(socket) = &self.socket else { return };
        let (target, chunk) = {
            let regs = self.regs.lock().expect("vgige relay regs poisoned");
            let target = SocketAddr::from((
                Ipv4Addr::from(regs.scda),
                reg::scp_host_port(regs.scp),
            ));
            (target, virtual_payload_chunk(regs.scps))
        };
        if target.port() == 0 {
            return; // channel closing; nothing to address.
        }
        send_block(socket, target, self.next_block_id, frame, chunk);
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
    let mut pkt = Vec::with_capacity(GVSP_HEADER_LEN + IMAGE_LEADER_BODY_LEN);
    push_header(&mut pkt, PACKET_FORMAT_DATA_LEADER, block_id, 0);
    pkt.extend_from_slice(&encode_image_leader_body(&leader));
    if socket.send_to(&pkt, target).is_err() {
        return;
    }
    let mut packet_id = 1u32; // R-264st: payload ids from 1.
    for data in frame.payload.chunks(chunk) {
        let mut pkt = Vec::with_capacity(GVSP_HEADER_LEN + data.len());
        push_header(&mut pkt, PACKET_FORMAT_DATA_PAYLOAD, block_id, packet_id);
        pkt.extend_from_slice(data);
        if socket.send_to(&pkt, target).is_err() {
            return;
        }
        packet_id += 1;
    }
    let mut pkt = Vec::with_capacity(GVSP_HEADER_LEN + IMAGE_TRAILER_LEN);
    push_header(&mut pkt, PACKET_FORMAT_DATA_TRAILER, block_id, packet_id);
    pkt.extend_from_slice(&encode_image_trailer_body(frame.height));
    let _ = socket.send_to(&pkt, target);
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
