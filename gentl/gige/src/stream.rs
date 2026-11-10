//! Data stream module object: GVSP stream channel setup/teardown over the
//! GVCP control channel (SCDAx/SCPx), one local receiver thread per
//! channel, and the registered-event bookkeeping behind EVENT_HANDLE.
//!
//! Channel rules (notes/gvcp-wirefacts.md, spec §11.5/§36.57): the primary
//! application opens a channel by writing SCDAx (destination IP) and then
//! SCPx with host_port in the low 16 bits (spec §36.57, MSB-first print
//! numbering — identical in GigE 1.2 §27.45; Q31). host_port != 0 activates,
//! 0 closes, and SCPx must be the last stream channel register written when
//! opening. Pure spec-only behavior: a device that ignores the spec encoding
//! fails the readback check with a clear error (notes/OPEN-QUESTIONS Q31).
//!
//! M3-4a multi-stream fan-out: the single GenTL DataStream ("Stream0",
//! GenTL surface unchanged) drives K GVSP channels, each with its own UDP
//! socket, receiver thread, and reassembler, all feeding the same DsCore.
//! K = min(device channels, `GENTLITE_GEV_STREAM_CHANNELS` default 8) —
//! see [`crate::discovery::desired_stream_channels`]. Channels 1..511 are
//! optional (spec §36.57): the count is probed by reading SCPx
//! sequentially until the device answers GEV_STATUS_INVALID_ADDRESS. A
//! secondary channel that fails to open degrades the stream to the
//! channels already opened; only total failure (channel 0) fails
//! DSStartAcquisition, matching the historical single-channel behavior.
//! Cross-channel frame delivery order is completion order — the spec is
//! silent on multi-channel ordering; every delivered frame keeps the
//! timestamp of its own leader.
//!
//! Shared-engine limitation (kept deliberately, M3-4a): the DsCore admits
//! exactly one in-flight frame. A frame-striped camera (whole frames
//! round-robined across channels, so cross-channel blocks never overlap —
//! the model the sim implements and the target of this fan-out) cannot
//! collide with itself. Only multi-tap parallel cameras (concurrent
//! cross-channel block streams) hit `FrameError::Busy`, dropping the
//! overlapped whole block (counted as `malformed` by the reassembler);
//! per-channel in-flight slots are the M3 DsCore work (gentl-core is
//! frozen this milestone).
//!
//! M3-2 packet-size negotiation (spec §11.7, R-567s): each channel's SCPSx
//! is negotiated with test packets after the channel open (SCPx readback)
//! and before the receiver thread spawns — the spec's "after open, before
//! regular streaming" window. `GENTLITE_GEV_PACKET_SIZE` overrides:
//! unset/0 = negotiate, >0 = fixed size (written and read back, no test
//! packet — QoI: the operator override is trusted). Details:
//! [`open_channel`].
//!
//! A-tier host-side tuning (spec-silent, QoI — notes/gige-100g-rx-design.md
//! §8): a deep SO_RCVBUF on every stream socket (`GENTLITE_GEV_RCVBUF_KB`,
//! default 4096) and optional receiver-thread CPU affinity
//! (`GENTLITE_GEV_RX_CORES`). Deliberately not done: `connect()`ing the
//! stream socket plus SIO_UDP_CONNRESET — that error only exists for
//! connected UDP sockets, and this socket stays unconnected, so the
//! original motivation does not apply.

use std::collections::HashMap;
use std::io;
use std::net::{Ipv4Addr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use gentl_core::ds::DsCore;
use gentl_core::error::{GentlError, GentlResult};
use gentl_core::event_queue::EventQueue;
use gentl_core::types::EventType;
use gentl_gvcp::gvsp;
use gentl_gvcp::registers;
use gentl_gvcp::wire::GEV_STATUS_INVALID_ADDRESS;
use gentl_gvcp::GvcpClient;

use crate::cti::{DeviceAccess, GevDevice};
use crate::discovery::desired_stream_channels;
use crate::gvsp_receiver::{receiver_loop, ReceiverStats};

/// The single stream channel's ID (fixed across sessions, GenTL 1.6 §3;
/// format not mandated by the spec — OPEN-QUESTION Q19).
pub const STREAM_ID: &str = "Stream0";

// §11.7 probe candidates: "To find the search limits one can write a very
// small and a very big value (e.g. 0 and 65535) to the register and read
// back the rounded values."
const PROBE_MIN: u32 = 0;
const PROBE_MAX: u32 = 65_535;
/// Single test-packet wait. QoI: spec §11.7 leaves arrival detection to
/// the management entity ("It is beyond the scope of this specification to
/// define how a management entity determines that a test packet is
/// received ... It is left as a quality of implementation"); 100 ms
/// covers a slow link with margin while keeping the search (≈17 probes)
/// around two seconds.
const TEST_PACKET_TIMEOUT: Duration = Duration::from_millis(100);

/// Requested SCPSx total packet size (IP+UDP+GVSP headers included) from
/// `GENTLITE_GEV_PACKET_SIZE`. Unset, 0, or unparseable: 0 = §11.7
/// automatic test-packet negotiation (the default). Any other value is a
/// fixed size written as-is; the device may round it (R-562st — the
/// readback wins). QoI: §11.7 leaves the search to the management entity,
/// and a fixed override is how an operator pins a known-good MTU.
fn desired_packet_size() -> u32 {
    std::env::var("GENTLITE_GEV_PACKET_SIZE")
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .unwrap_or(0)
}

/// Stream socket SO_RCVBUF request in KiB from `GENTLITE_GEV_RCVBUF_KB`
/// (default 4096). QoI host-side tuning: the GigE RX design notes
/// (notes/gige-100g-rx-design.md §8) list deep receive buffers as a
/// countermeasure against burst drops; the spec is silent on host tuning.
fn desired_rcvbuf_kb() -> u32 {
    const DEFAULT_KB: u32 = 4096;
    std::env::var("GENTLITE_GEV_RCVBUF_KB")
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .unwrap_or(DEFAULT_KB)
}

/// Receiver-thread CPU affinity mask from `GENTLITE_GEV_RX_CORES` (hex bit
/// mask, e.g. 0x00F0). Unset or unparseable: no binding. QoI: the spec is
/// silent on host thread placement and core selection is a deployment-side
/// decision — nothing here picks cores automatically (that would be
/// guessing).
fn desired_rx_affinity() -> Option<usize> {
    let v = std::env::var("GENTLITE_GEV_RX_CORES").ok()?;
    let hex = v.trim().trim_start_matches("0x").trim_start_matches("0X");
    usize::from_str_radix(hex, 16).ok()
}

/// §16.7 resend switch from `GENTLITE_GEV_RESEND`: unset or any value
/// other than "0" = enabled (the default; a device without the
/// PACKETRESEND capability bit still never sees a request, R-197ca);
/// "0" = disabled even when the device supports it.
fn desired_resend() -> bool {
    std::env::var("GENTLITE_GEV_RESEND")
        .map(|v| v.trim() != "0")
        .unwrap_or(true)
}

/// §16.7.3 frame retention (QoI — Aravis FRAME_RETENTION 100 ms) from
/// `GENTLITE_GEV_FRAME_RETENTION_MS`, clamped to 10..=60000 ms.
fn desired_frame_retention() -> Duration {
    const DEFAULT_MS: u64 = 100;
    let ms = std::env::var("GENTLITE_GEV_FRAME_RETENTION_MS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_MS);
    Duration::from_millis(ms.clamp(10, 60_000))
}

/// Set SO_RCVBUF on a stream socket and store the granted buffer size in
/// bytes (0 when the call failed); OS details live in [`crate::sys`].
fn apply_so_rcvbuf(socket: &UdpSocket, kb: u32) -> usize {
    crate::sys::set_recv_buffer(socket, kb as usize)
}

/// Bind the calling thread to the `GENTLITE_GEV_RX_CORES` mask. Best
/// effort: an out-of-range mask fails silently and the thread keeps its
/// default placement (QoI — see [`desired_rx_affinity`]).
fn set_rx_affinity(mask: usize) {
    crate::sys::set_thread_affinity(mask as u64);
}

/// Transport context of one GVSP stream channel (spec §36.57 SCPx): one
/// local UDP socket, one receiver thread with its own reassembler, one
/// stop/kill flag pair, and the channel's own SCPSx-derived payload
/// stride. The socket stays bound across stop/start; only DSClose drops
/// the stream and its channels.
struct ChannelCtx {
    /// Channel number x of the SCPx/SCPSx/SCPDx/SCDAx address map.
    index: u16,
    /// Local GVSP socket, bound at open to 0.0.0.0:0; its port is written
    /// into SCPx at start.
    socket: UdpSocket,
    receiver: Mutex<Option<JoinHandle<()>>>,
    stop: Arc<AtomicBool>,
    kill: Arc<AtomicBool>,
    /// This channel's receiver counters, published when its thread exits.
    stats: Arc<Mutex<ReceiverStats>>,
    /// Set once the channel open sequence verified (SCPx readback); stop
    /// closes only opened channels.
    opened: Mutex<bool>,
    /// Granted SO_RCVBUF in bytes (getsockopt readback; 0 = the
    /// setsockopt failed and the OS default is in effect). QoI diagnostic
    /// (see [`desired_rcvbuf_kb`]): the OS clamps the request to a kernel
    /// maximum, so only the readback is truthful. Kept for inspection —
    /// nothing reads it outside the unit test yet.
    #[allow(dead_code)]
    so_rcvbuf: usize,
}

impl ChannelCtx {
    fn new(index: u16, socket: UdpSocket) -> Self {
        // A-tier receive tuning (QoI — the spec is silent on host socket
        // options; notes/gige-100g-rx-design.md §8 motivates deep receive
        // buffers). Applied before the channel opens so the negotiation
        // window already benefits.
        let so_rcvbuf = apply_so_rcvbuf(&socket, desired_rcvbuf_kb());
        ChannelCtx {
            index,
            socket,
            receiver: Mutex::new(None),
            stop: Arc::new(AtomicBool::new(false)),
            kill: Arc::new(AtomicBool::new(false)),
            stats: Arc::new(Mutex::new(ReceiverStats::default())),
            opened: Mutex::new(false),
            so_rcvbuf,
        }
    }
}

/// Probe mode: §11.7's verbatim three-step (write the size plain, read
/// back the rounded value, fire it) versus the fire-only variant used when
/// a device rejects a plain SCPSx write on an open channel
/// ([`negotiate_packet_size`]).
#[derive(Clone, Copy, PartialEq, Eq)]
enum ProbeMode {
    ThreeStep,
    FireOnly,
}

/// One §11.7 probe of candidate total packet size `m` (IP+UDP+GVSP
/// headers included, spec §36.58). Returns the device-rounded size r
/// (R-562st: the register holds the value the device supports, so writes
/// must be read back) and whether r's test packet arrived. Any SCPSx
/// transaction error aborts the whole negotiation with None — the caller
/// silently keeps the factory SCPSx-derived stride (old/read-only
/// devices).
fn probe(
    c: &GvcpClient,
    x: u32,
    socket: &UdpSocket,
    m: u32,
    mode: ProbeMode,
    test_data_supported: bool,
) -> Option<(u32, bool)> {
    let r = match mode {
        ProbeMode::ThreeStep => {
            // §11.7 verbatim: "Write the packet size without bit 0
            // (fire_test_packet) set, read back the rounded packet size,
            // write the rounded packet size with bit 0 set".
            c.write_reg(registers::scps(x), registers::scps_make(m))
                .ok()?;
            let r = registers::scps_packet_size(c.read_reg(registers::scps(x)).ok()?);
            c.write_reg(
                registers::scps(x),
                r | registers::SCPS_FIRE_TEST_PACKET | registers::SCPS_DO_NOT_FRAGMENT,
            )
            .ok()?;
            r
        }
        ProbeMode::FireOnly => {
            // One combined write: fire + DF + candidate size. The device
            // rounds, stores, and fires the stored size; the readback then
            // yields the rounded r.
            c.write_reg(
                registers::scps(x),
                registers::scps_make(m)
                    | registers::SCPS_FIRE_TEST_PACKET
                    | registers::SCPS_DO_NOT_FRAGMENT,
            )
            .ok()?;
            registers::scps_packet_size(c.read_reg(registers::scps(x)).ok()?)
        }
    };
    // A rounded value with no room for even one payload byte is a device
    // error, not a size. The floor is the smallest header budget of either
    // form: 36 = IP 20 + UDP 8 + the 1.2 legacy GVSP header (3.0 devices
    // round higher anyway — §36.58's domain starts at 48+1).
    if r <= gvsp::GVSP_LEGACY_HEADERS_OVERHEAD {
        return None;
    }
    Some((r, test_packet_arrived(socket, r, test_data_supported)))
}

/// The §11.7 search from a confirmed lower bound: the 65535 bound probe
/// reads back the device's rounded upper limit; each further probe grows
/// `lo` when the test packet arrived and shrinks `hi` when it did not,
/// until the two meet. Device rounding (R-562st) can map a candidate
/// outside (lo, hi]; the `last_mid` stop rule guarantees termination in
/// that case instead of spinning on a repeated probe.
fn search_from(
    c: &GvcpClient,
    x: u32,
    socket: &UdpSocket,
    lo0: u32,
    mode: ProbeMode,
    test_data_supported: bool,
) -> Option<u32> {
    let mut lo = lo0;
    let (hi_r, hi_arrived) = probe(c, x, socket, PROBE_MAX, mode, test_data_supported)?;
    let mut hi = if hi_arrived {
        hi_r
    } else {
        hi_r.saturating_sub(1)
    };
    let mut last_mid = u32::MAX;
    while hi > lo {
        let mid = lo + (hi - lo + 1) / 2;
        if mid == last_mid {
            break;
        }
        last_mid = mid;
        let (r, arrived) = probe(c, x, socket, mid, mode, test_data_supported)?;
        if arrived {
            lo = r.max(lo);
        } else {
            hi = r.saturating_sub(1).min(hi);
        }
    }
    Some(lo)
}

/// §11.7 packet-size negotiation (R-567s: after the channel is open,
/// before regular streaming — the receiver thread has not spawned yet, so
/// this thread is the only reader of the stream socket: point-to-point
/// arrival detection). Returns the negotiated SCPSx total packet size.
///
/// The first probe uses the spec's three-step. When its plain SCPSx write
/// is rejected — a device that blocks SCPSx writes once the channel
/// streams (§36.58's block, QoI-extended in the sim, Q33) cannot take
/// step one even though §11.7's window is right after the open — the
/// search retries in fire-only mode: every probe write carries
/// fire_test_packet, the one SCPSx write a test-packet-tolerating device
/// must still accept. Devices that reject those too, and devices whose
/// minimum-size test packet does not arrive, return None and the caller
/// silently keeps the factory stride.
fn negotiate_packet_size(
    c: &GvcpClient,
    x: u32,
    socket: &UdpSocket,
    test_data_supported: bool,
) -> Option<u32> {
    match probe(
        c,
        x,
        socket,
        PROBE_MIN,
        ProbeMode::ThreeStep,
        test_data_supported,
    ) {
        Some((r, true)) => search_from(c, x, socket, r, ProbeMode::ThreeStep, test_data_supported),
        // Minimum size undeliverable: nothing below it can work either.
        Some((_, false)) => None,
        None => bisect_fire_only(c, x, socket, test_data_supported),
    }
}

/// Fire-only retry of the whole §11.7 search ([`negotiate_packet_size`]).
fn bisect_fire_only(
    c: &GvcpClient,
    x: u32,
    socket: &UdpSocket,
    test_data_supported: bool,
) -> Option<u32> {
    let (r, arrived) = probe(
        c,
        x,
        socket,
        PROBE_MIN,
        ProbeMode::FireOnly,
        test_data_supported,
    )?;
    if !arrived {
        return None;
    }
    search_from(c, x, socket, r, ProbeMode::FireOnly, test_data_supported)
}

/// Fixed-size mode (`GENTLITE_GEV_PACKET_SIZE` > 0): write the requested
/// total size without fire, read back the rounded value (R-562st), use it.
/// No test packet is fired — the operator override is trusted (QoI).
fn fixed_packet_size(c: &GvcpClient, x: u32, size: u32) -> Option<u32> {
    c.write_reg(registers::scps(x), registers::scps_make(size))
        .ok()?;
    let r = registers::scps_packet_size(c.read_reg(registers::scps(x)).ok()?);
    (r > registers::GVSP_HEADERS_OVERHEAD).then_some(r)
}

/// QoI arrival criterion (§11.7 NOTE: detection is left as a quality of
/// implementation; on this point-to-point socket the producer is the
/// GVSP receiver): a test packet is a datagram whose IPv4 total length
/// equals the SCPSx-declared size — §24.5: "IP Header (20 bytes) + UDP
/// Header (8 bytes) + GVSP Header + payload data is equal to the packet
/// size" — and whose GVSP block_id field is 0 (§16.7: "0 is reserved for
/// the test packet"; regular blocks start at 1, R-259st). The test packet
/// does not follow the usual GVSP layout and the EI bit is don't care
/// (§24.5), so an undecodable datagram simply does not count as a test
/// packet. Regular stream frames slipping into the window (block_id ≠ 0,
/// or a length mismatch) are discarded, not counted. When the device
/// declares the test_data capability, CR-280st makes the §24.5.1 LFSR
/// fill mandatory and the payload is compared against it (CO-281sr
/// SHOULD, gated on the declaration).
fn test_packet_arrived(socket: &UdpSocket, total: u32, test_data_supported: bool) -> bool {
    let deadline = Instant::now() + TEST_PACKET_TIMEOUT;
    // The socket has no read timeout yet (receiver_loop sets its own at
    // start); a failed set here just makes recv_from block — bounded by
    // the deadline check only on the next datagram, so ignore the result
    // and rely on the OS default never having been observed to fail.
    let _ = socket.set_read_timeout(Some(TEST_PACKET_TIMEOUT));
    let mut buf = [0u8; 65536];
    while Instant::now() < deadline {
        match socket.recv_from(&mut buf) {
            Ok((n, _)) => {
                if n as u32 + 20 + 8 != total {
                    continue; // regular frame or foreign datagram
                }
                // §24.5 leaves the test packet's Extended ID bit don't care
                // and the layout "closer to the GigE Vision 1.x packet
                // style", so a 1.2 transmitter fires it with an 8-byte
                // header: discriminate per datagram like the receiver does
                // (decode_auto) instead of assuming the 20-byte form.
                let Some((hdr, header_len)) = gvsp::GvspHeader::decode_auto(&buf[..n]) else {
                    continue; // not decodable as either anticipated form
                };
                if hdr.block_id != 0 {
                    continue; // regular stream block (R-259st: ids from 1)
                }
                if test_data_supported && !lfsr_matches(&buf[header_len..n]) {
                    continue; // CR-280st fill violated: not a valid test packet
                }
                return true;
            }
            Err(e)
                if e.kind() == io::ErrorKind::TimedOut || e.kind() == io::ErrorKind::WouldBlock =>
            {
                break
            }
            Err(_) => break, // socket error: count the probe as missed (QoI)
        }
    }
    false
}

/// §24.5.1 test-data generator, straight from the spec's C
/// pseudo-implementation: a 16-bit right-shifted LFSR with polynomial
/// 0x8016 and initial value 0xFFFF, clocked once per byte, each byte the
/// LSB of the register (the first payload byte is therefore 0xFF).
fn lfsr_matches(payload: &[u8]) -> bool {
    const POLYNOMIAL: u16 = 0x8016;
    let mut lfsr: u16 = 0xFFFF;
    for &byte in payload {
        if byte != (lfsr & 0xFF) as u8 {
            return false;
        }
        lfsr = (lfsr >> 1) ^ (0u16.wrapping_sub(lfsr & 1) & POLYNOMIAL);
    }
    true
}

/// Drain negotiation leftovers — test packets and stream frames of the
/// search window — so the receiver thread starts from a quiet socket
/// (R-567s ordering). Datagrams arriving between here and the receiver's
/// first read are handled by the reassembler's orphan/gap accounting.
fn drain_socket(socket: &UdpSocket) {
    let _ = socket.set_nonblocking(true);
    let mut buf = [0u8; 65536];
    loop {
        match socket.recv_from(&mut buf) {
            Ok(_) => continue,
            Err(_) => break,
        }
    }
    let _ = socket.set_nonblocking(false);
}

/// Open GVSP stream channel #x towards `local_ip:port`.
///
/// R-561ca (spec §36.57): host_port must be 0 before writing SCDAx. A
/// fresh channel satisfies this; a stale non-zero value means a previous
/// session died without closing — close it first (writing 0 is always
/// allowed, R-586cd). SCPx is the last stream channel register written
/// when opening; the only writes allowed after it are the SCPSx
/// packet-size negotiation of spec §11.7 (R-567s), implemented here (M3-2)
/// before the receiver thread spawns. The open is verified by reading
/// SCPx back: a device may refuse or silently adjust the register, and a
/// host_port of 0 means the channel did not open. Spec-only: no
/// non-compliant-device fallback (project decision 2026-10-12,
/// notes/OPEN-QUESTIONS Q25).
///
/// The receiver's payload stride is inferred from each block's first
/// payload packet (see [`crate::gvsp_receiver::GvspReassembler`]), so the
/// SCPSx readback is not needed here; negotiation still writes SCPSx on
/// the device, which is what the §11.7 contract is about.
fn open_channel(
    c: &GvcpClient,
    x: u32,
    local_ip: Ipv4Addr,
    port: u16,
    socket: &UdpSocket,
    test_data_supported: bool,
) -> GentlResult<()> {
    if registers::scp_host_port(c.read_reg(registers::scp(x))?) != 0 {
        c.write_reg(registers::scp(x), registers::scp_make(0))?;
    }
    c.write_reg(registers::scda(x), u32::from(local_ip))?;
    c.write_reg(registers::scp(x), registers::scp_make(port))?;
    if registers::scp_host_port(c.read_reg(registers::scp(x))?) == 0 {
        return Err(GentlError::Io(format!(
            "stream channel {x} did not open (SCPx host_port reads back 0)"
        )));
    }
    // M3-2 (§11.7, R-567s): the channel is open and regular streaming has
    // not started, so this is the negotiation window. Unset/0 env =
    // test-packet search; >0 = fixed size, written and read back without
    // a test packet. Any SCPSx transaction error abandons negotiation
    // silently — an old or read-only device must not fail the stream
    // start. After the search, drain whatever the window left on the
    // socket before the receiver thread starts.
    match desired_packet_size() {
        0 => {
            negotiate_packet_size(c, x, socket, test_data_supported);
        }
        fixed => {
            fixed_packet_size(c, x, fixed);
        }
    }
    drain_socket(socket);
    Ok(())
}

/// Data Stream module behind DS_HANDLE.
pub struct GevDataStream {
    pub parent: Weak<GevDevice>,
    /// Protocol-independent engine state (buffer pools, acquisition
    /// counting, event data push); shared by every GVSP channel.
    pub core: Arc<DsCore>,
    /// Control channel shared with the device (externally synchronized).
    pub control: Arc<Mutex<GvcpClient>>,
    /// One transport context per GVSP stream channel. Channel 0 is always
    /// present; further channels were probed at open (spec §36.57 — they
    /// are optional and answer GEV_STATUS_INVALID_ADDRESS past the device
    /// count).
    channels: Mutex<Vec<ChannelCtx>>,
    /// Device timestamp tick frequency, for BUFFER_INFO_TIMESTAMP_NS.
    pub tick_freq: Option<u64>,
    /// Registered events by type.
    events: Mutex<HashMap<EventType, Arc<EventModule>>>,
    /// Core buffer id -> boxed BUFFER_HANDLE; the value handed out at
    /// announcement is returned by DSGetBufferID and EVENT_NEW_BUFFER_DATA.
    buffer_handles: Mutex<HashMap<u64, usize>>,
}
/// Buffer module behind BUFFER_HANDLE.
pub struct GevBuffer {
    pub stream: Weak<GevDataStream>,
    pub id: u64,
}

/// Event module behind EVENT_HANDLE (GenTL 1.6 §4.2). One registered event
/// object; its queue backs EventGetData/EventGetInfo/EventFlush/EventKill.
pub struct EventModule {
    pub source: Weak<GevDataStream>,
    pub event_type: EventType,
    pub queue: Arc<EventQueue>,
}

/// Local source IP that would reach `device_ip` (for SCDAx). A connected
/// UDP socket performs no handshake; the kernel only resolves the route.
fn local_ip_for(device_ip: Ipv4Addr) -> GentlResult<Ipv4Addr> {
    let s = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).map_err(GentlError::from)?;
    s.connect((device_ip, 9)).map_err(GentlError::from)?;
    let addr = s.local_addr().map_err(GentlError::from)?;
    match addr.ip() {
        std::net::IpAddr::V4(ip) => Ok(ip),
        std::net::IpAddr::V6(_) => Err(GentlError::Io("no IPv4 route to device".into())),
    }
}

impl GevDataStream {
    /// `DevOpenDataStream`: probe the stream channel count (spec §36.57)
    /// and read the tick frequency, then bind one local socket per
    /// channel. Does not touch the device's channel state.
    pub fn open(device: &Arc<GevDevice>) -> GentlResult<Arc<Self>> {
        let control = device.client.clone();
        let (n_channels, tick_freq) = {
            let c = control
                .lock()
                .map_err(|_| GentlError::Io("client lock poisoned".into()))?;
            // §36.57: channels 1..511 are optional; a device without
            // channel x answers GEV_STATUS_INVALID_ADDRESS — the probe
            // terminator. The probe stops at `desired` channels: a device
            // implementing more cannot be used anyway. Any other failure
            // stops the probe too: the count is best-effort and degrades,
            // never errors (a device that drops probe reads still gets
            // its channel 0).
            let desired = desired_stream_channels();
            let mut have = 1u32; // channel 0 always exists
            for x in 1..desired {
                match c.read_reg(registers::scp(x)) {
                    Ok(_) => have += 1,
                    Err(GentlError::GevStatus(GEV_STATUS_INVALID_ADDRESS)) => break,
                    Err(_) => break,
                }
            }
            // Tick frequency read is best-effort (defaults below).
            let hi = c.read_reg(registers::TIMESTAMP_TICK_FREQ_HIGH);
            let lo = c.read_reg(registers::TIMESTAMP_TICK_FREQ_LOW);
            let freq = match (hi, lo) {
                (Ok(h), Ok(l)) => Some((u64::from(h) << 32) | u64::from(l)),
                _ => None,
            };
            (have, freq)
        };
        let mut channels = Vec::new();
        for x in 0..n_channels {
            // A bind failure of a secondary channel degrades to the
            // channels already bound; channel 0 must bind or the stream
            // is unusable (historical single-channel behavior).
            match UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)) {
                Ok(socket) => channels.push(ChannelCtx::new(x as u16, socket)),
                Err(e) if x == 0 => return Err(GentlError::from(e)),
                Err(_) => break,
            }
        }
        Ok(Arc::new(Self {
            parent: Arc::downgrade(device),
            core: Arc::new(DsCore::new()),
            control,
            channels: Mutex::new(channels),
            tick_freq,
            events: Mutex::new(HashMap::new()),
            buffer_handles: Mutex::new(HashMap::new()),
        }))
    }

    /// Receiver thread running on any channel?
    pub fn is_running(&self) -> bool {
        self.channels
            .lock()
            .unwrap()
            .iter()
            .any(|ch| ch.receiver.lock().unwrap().is_some())
    }

    /// `DSStartAcquisition` transport half: open every stream channel
    /// (SCDAx + SCPx sequence per channel) and start one receiver thread
    /// per channel, all feeding the shared engine.
    ///
    /// Degradation rule (spec §36.57 optional channels): a secondary
    /// channel whose open fails leaves the stream running on the channels
    /// already opened; only failure of channel 0 (or no channel at all)
    /// fails the start, matching the historical single-channel behavior.
    pub fn start(self: &Arc<Self>) -> GentlResult<()> {
        let device = self.parent.upgrade().ok_or(GentlError::InvalidHandle)?;
        // GVRSP-only devices were rejected at device open; the match stays
        // as the dispatch point for a future GVRSP (RoCEv2) backend.
        match device.transport {
            crate::cti::StreamTransport::Gvsp => {}
        }
        if device.access == DeviceAccess::Monitor {
            // SCDAx/SCPx are writes; a monitor control channel must not
            // stream (spec §10).
            return Err(GentlError::AccessDenied);
        }
        if self.is_running() {
            return Err(GentlError::ResourceInUse);
        }
        let result = (|| {
            let local_ip = local_ip_for(Ipv4Addr::from(device.discovery.current_ip))?;
            let rx_affinity = desired_rx_affinity();
            let test_data_supported = device.test_data_supported;
            // §16.7 (R-197ca): resend only when the device declares the
            // PACKETRESEND capability and the operator did not switch it off.
            let resend_cfg = crate::gvsp_receiver::ResendConfig {
                enabled: device.packet_resend_supported && desired_resend(),
                frame_retention: desired_frame_retention(),
            };
            let channels = self.channels.lock().unwrap();
            let mut opened = 0usize;
            let mut first_err: Option<GentlError> = None;
            for idx in 0..channels.len() {
                let x = u32::from(channels[idx].index);
                let port = channels[idx]
                    .socket
                    .local_addr()
                    .map_err(GentlError::from)?
                    .port();
                let res = {
                    let c = self
                        .control
                        .lock()
                        .map_err(|_| GentlError::Io("client lock poisoned".into()))?;
                    open_channel(&c, x, local_ip, port, &channels[idx].socket, test_data_supported)
                };
                match res {
                    Ok(()) => {
                        *channels[idx].opened.lock().unwrap() = true;
                        channels[idx].stop.store(false, Ordering::SeqCst);
                        channels[idx].kill.store(false, Ordering::SeqCst);
                        let sock = channels[idx].socket.try_clone().map_err(GentlError::from)?;
                        let sink = self.core.clone();
                        let stop = channels[idx].stop.clone();
                        let kill = channels[idx].kill.clone();
                        let stats = channels[idx].stats.clone();
                        let resend = resend_cfg.enabled.then(|| crate::gvsp_receiver::ResendHandle {
                            client: self.control.clone(),
                            channel: x as u16,
                            config: resend_cfg,
                        });
                        let name = format!("gvsp-recv-{}-ch{x}", device.id);
                        let me = self.clone();
                        let handle = std::thread::Builder::new()
                            .name(name)
                            .spawn(move || {
                                if let Some(mask) = rx_affinity {
                                    set_rx_affinity(mask);
                                }
                                receiver_loop(sock, sink, stop, kill, stats, me, resend)
                            })
                            .map_err(|e| GentlError::Io(format!("spawn receiver: {e}")))?;
                        *channels[idx].receiver.lock().unwrap() = Some(handle);
                        opened += 1;
                    }
                    Err(e) => {
                        first_err = Some(e);
                        break;
                    }
                }
            }
            if opened == 0 {
                return Err(first_err.unwrap_or_else(|| {
                    GentlError::Io("no stream channel available".into())
                }));
            }
            Ok(())
        })();
        if result.is_err() {
            self.stop(true);
        }
        result
    }

    /// Receiver-fatal recovery (IO error / unrecoverable desync): the
    /// transport is dead, so the engine must reflect that — otherwise
    /// `DSGetInfo(IS_GRABBING)` keeps reporting true and `EventGetData`
    /// blocks forever. Forces the core engine to the stopped state and
    /// wakes pending event waiters with an abort.
    pub fn on_receiver_fatal(self: &Arc<Self>) {
        self.core.abort_engine();
        if let Ok(events) = self.events.lock() {
            for ev in events.values() {
                ev.queue.kill_one();
            }
        }
    }

    /// `DSStopAcquisition` transport half: stop every receiver (graceful
    /// or kill) and close every opened channel (SCPx host_port = 0).
    pub fn stop(&self, kill: bool) {
        let channels = self.channels.lock().unwrap();
        for ch in channels.iter() {
            ch.kill.store(kill, Ordering::SeqCst);
            ch.stop.store(true, Ordering::SeqCst);
        }
        for ch in channels.iter() {
            if let Some(h) = ch.receiver.lock().unwrap().take() {
                let _ = h.join();
            }
        }
        if let Ok(c) = self.control.lock() {
            for ch in channels.iter() {
                let mut opened = ch.opened.lock().unwrap();
                if *opened {
                    // 0 closes the channel (spec §36.57).
                    let _ =
                        c.write_reg(registers::scp(u32::from(ch.index)), registers::scp_make(0));
                    *opened = false;
                }
            }
        }
    }

    /// `DSClose` cleanup: stop the engine, kill event registrations, free
    /// the buffer handle boxes (the buffers themselves die with the core).
    pub fn close(&self) {
        if self.is_running() {
            self.stop(false);
        }
        for ev in self.events.lock().unwrap().values() {
            ev.queue.mark_dead();
        }
        self.core.clear_event_queue();
        let handles: Vec<usize> = self
            .buffer_handles
            .lock()
            .unwrap()
            .values()
            .copied()
            .collect();
        for h in handles {
            unsafe { gentl_core::free_handle(h as *mut core::ffi::c_void) };
        }
        self.buffer_handles.lock().unwrap().clear();
    }

    /// `GCRegisterEvent`.
    pub fn register_event(
        self: &Arc<Self>,
        event_type: EventType,
    ) -> GentlResult<Arc<EventModule>> {
        let mut map = self.events.lock().unwrap();
        if map.contains_key(&event_type) {
            return Err(GentlError::ResourceInUse);
        }
        let module = Arc::new(EventModule {
            source: Arc::downgrade(self),
            event_type,
            queue: Arc::new(EventQueue::new()),
        });
        map.insert(event_type, module.clone());
        self.core.set_event_queue(module.queue.clone());
        Ok(module)
    }

    /// `GCUnregisterEvent`: pending events are silently discarded, pending
    /// output-queue buffers become non-queued, waiters get ABORT.
    pub fn unregister_event(&self, event_type: EventType) -> GentlResult<()> {
        let Some(module) = self.events.lock().unwrap().remove(&event_type) else {
            // GenTL 1.6 §6.3.7.7 lists this case as GC_ERR_NOT_INITIALIZED.
            return Err(GentlError::NotInitialized);
        };
        module.queue.mark_dead();
        self.core.clear_event_queue();
        self.core.discard_output();
        Ok(())
    }

    /// Record the handle handed out for a core buffer id. Stored as an
    /// address-sized integer so the stream stays `Send`/`Sync`.
    pub fn set_buffer_handle(&self, id: u64, handle: *mut core::ffi::c_void) {
        self.buffer_handles
            .lock()
            .unwrap()
            .insert(id, handle as usize);
    }

    /// The stable BUFFER_HANDLE for a core buffer id.
    pub fn buffer_handle(&self, id: u64) -> Option<*mut core::ffi::c_void> {
        self.buffer_handles
            .lock()
            .unwrap()
            .get(&id)
            .map(|h| *h as *mut core::ffi::c_void)
    }

    /// Drop a buffer's handle registration (does not free the box; the
    /// caller does that).
    pub fn remove_buffer_handle(&self, id: u64) {
        self.buffer_handles.lock().unwrap().remove(&id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_ip_for_loopback_is_loopback() {
        let ip = local_ip_for(Ipv4Addr::LOCALHOST).unwrap();
        assert_eq!(ip, Ipv4Addr::LOCALHOST);
    }

    /// §24.5.1: 16-bit right-shifted LFSR, polynomial 0x8016, init
    /// 0xFFFF, byte = LSB of the register. First byte 0xFF, and a
    /// corruption anywhere fails the comparison.
    #[test]
    fn lfsr_test_data_pattern() {
        const POLYNOMIAL: u16 = 0x8016;
        let mut lfsr: u16 = 0xFFFF;
        let mut expected = Vec::new();
        for _ in 0..512 {
            expected.push((lfsr & 0xFF) as u8);
            lfsr = (lfsr >> 1) ^ (0u16.wrapping_sub(lfsr & 1) & POLYNOMIAL);
        }
        assert_eq!(expected[0], 0xFF, "first byte is the LFSR init LSB");
        assert!(lfsr_matches(&expected));
        let mut corrupted = expected.clone();
        corrupted[100] ^= 0x01;
        assert!(!lfsr_matches(&corrupted));
        assert!(lfsr_matches(&[]), "empty payload vacuously matches");
    }

    /// SO_RCVBUF request + readback wiring: the granted size lands in the
    /// channel context. (The OS clamps the request; the readback is
    /// always > 0 for a valid socket.)
    #[test]
    fn so_rcvbuf_granted_value_is_stored() {
        let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).unwrap();
        let ch = ChannelCtx::new(0, socket);
        assert!(ch.so_rcvbuf > 0, "getsockopt readback of SO_RCVBUF");
    }
}
