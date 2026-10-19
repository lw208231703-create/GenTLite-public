//! GVSP fake-image transmitter of the simulated camera (spec Part 3 §22–26;
//! notes/gvcp-wirefacts.md "GVSP" section). M3-4a: one
//! [`StreamScheduler`] per camera — a single thread that stripe-schedules
//! whole frames across every open stream channel:
//!
//! - Frame k goes to `open_channels[k % open_count]` (the open set is
//!   re-snapshots per frame, so open/close takes effect on the next
//!   frame). A frame is fully sent (leader, payloads, trailer) before
//!   the next one starts — cross-channel blocks never overlap, matching
//!   multi-stream area-scan cameras that stripe frames across channels.
//!   (Multi-tap parallel cameras with concurrent per-channel block
//!   streams are not simulated.)
//! - Each channel keeps its own local block_id sequence starting at 1
//!   (R-259st per channel); with striping, channel x's m-th frame carries
//!   local id m. Re-opening a channel restarts its sequence.
//!
//! Simplifications vs a real camera (documented):
//! - Blocks are emitted continuously while any channel is open; there is
//!   no AcquisitionStart gating (notes/OPEN-QUESTIONS.md Q15).
//! - The inter-packet delay (SCPDx, spec §36.59) is a plain thread sleep
//!   in timestamp-counter units (the sim clock ticks at 1 MHz, so µs).
//! - Packet resend (PACKETRESEND_CMD) is not implemented (M3 scope).

use std::collections::{BTreeSet, HashMap};
use std::io;
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::os::windows::io::AsRawSocket;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use gentl_gvcp::gvsp::*;
use gentl_gvcp::registers as reg;
use gentl_gvcp::wire::{GEV_STATUS_PACKET_RESEND, GEV_STATUS_PACKET_UNAVAILABLE};
use windows_sys::Win32::Networking::WinSock::{setsockopt, IPPROTO_IP, IP_DONTFRAGMENT};

use crate::StreamConfig;

/// Test hook: payload packets (block_id, packet_id) the scheduler drops
/// on send, simulating network loss (leader/trailer are never dropped).
pub type PacketDrop = BTreeSet<(u64, u32)>;

/// Payload bytes per full GVSP generic payload packet when SCPSx reads as
/// 0 (unwritten): 1400 data bytes, so the IPv4 total (1400 + IP/UDP/GVSP
/// headers) stays at 1448, under the 1500-byte MTU.
pub(crate) const DEFAULT_PAYLOAD: usize = 1400;

/// Largest payload data chunk put into one datagram (maximum IPv4 UDP
/// payload minus the GVSP header).
const MAX_PAYLOAD: usize = 65_507 - GVSP_HEADER_LEN;

/// Pause between consecutive frames on the wire. Gives the stripe clean
/// frame boundaries (and stands in for the frame period).
const FRAME_INTERVAL: Duration = Duration::from_millis(1);

/// Pause when no channel is open (the scheduler idles without busy-spin).
const IDLE_PAUSE: Duration = Duration::from_millis(1);

// PFNC pixel format value (include/PFNC.h).
const PFNC_MONO16: u32 = 0x0110_0007;

/// The SCPSx header budget of a stream config: 48 bytes (IP+UDP+20-byte
/// 3.0 GVSP header, §36.58) or 36 bytes (IP+UDP+8-byte 1.2 legacy header,
/// 1.2 §27.46 — Q28 capture verified: SCPS=1500 -> 1464-byte payloads).
pub(crate) fn headers_overhead(stream: &StreamConfig) -> u32 {
    if stream.legacy_headers {
        GVSP_LEGACY_HEADERS_OVERHEAD
    } else {
        reg::GVSP_HEADERS_OVERHEAD
    }
}

/// Round a requested SCPSx total packet size to the value the sim stores
/// and transmits (R-562st: the register must read back the size the
/// transmitter actually uses). Two steps: clamp to the sim's continuous
/// size domain [overhead+1, 65535] (≥ 1 payload byte, up to the u16
/// field), then floor the payload to a pixel multiple so the register
/// never promises a packet the emitter would realign on the wire. The
/// result is what [`StreamParams::repacket`] derives the same payload
/// from.
pub(crate) fn round_packet_size(stream: &StreamConfig, requested: u32) -> u32 {
    let align: u32 = if stream.pixel_format == PFNC_MONO16 { 2 } else { 1 };
    let overhead = headers_overhead(stream);
    let total = reg::scps_packet_size(requested).clamp(overhead + 1, 65_535);
    let payload = (total - overhead).clamp(align, MAX_PAYLOAD as u32) / align * align;
    payload + overhead
}

/// Emitter settings frozen at channel open. A real device forbids SCPSx
/// writes while streaming (spec §36.58), so the sim snapshots the registers
/// instead of re-reading them per packet.
#[derive(Clone, Copy, Debug)]
pub(crate) struct StreamParams {
    pub width: u32,
    pub height: u32,
    pub pixel_format: u32,
    /// 1.2 legacy header form (1.2 §24 Fig 24-1) vs the 3.0 form (§24.2).
    pub legacy: bool,
    pub payload_size: usize,
    pub packet_delay: Duration,
}

impl StreamParams {
    /// Snapshot from the SCPS0/SCPD0 register values (spec layout,
    /// §36.58/§36.59 — the SCPSx packet_size counts the header budget of
    /// the configured header form, 48 or 36 bytes) and the camera's
    /// stream configuration.
    ///
    /// An unwritten (0) or absurdly small (< headers) packet_size falls
    /// back to [`DEFAULT_PAYLOAD`] (a real device rounds to the nearest
    /// supported size instead, R-562st — see notes/OPEN-QUESTIONS.md Q21).
    pub fn new(stream: &StreamConfig, scps0: u32, scpd0: u32) -> Self {
        let mut params = StreamParams {
            width: stream.width,
            height: stream.height,
            pixel_format: stream.pixel_format,
            legacy: stream.legacy_headers,
            payload_size: DEFAULT_PAYLOAD,
            packet_delay: Duration::from_micros(u64::from(scpd0)),
        };
        params.repacket(scps0);
        params
    }

    /// (Re)derive the payload size from an SCPSx value: payload data per
    /// packet = packet_size - the mode's header budget (48 bytes at 3.0
    /// §36.58, 36 bytes at 1.2 §27.46 for the 8-byte legacy header). See
    /// [`StreamParams::new`] for the fallback rule.
    fn repacket(&mut self, scps0: u32) {
        let overhead = if self.legacy {
            GVSP_LEGACY_HEADERS_OVERHEAD
        } else {
            reg::GVSP_HEADERS_OVERHEAD
        };
        let requested = match reg::scps_packet_size(scps0).checked_sub(overhead) {
            Some(p) if p > 0 => p as usize,
            _ => DEFAULT_PAYLOAD,
        };
        // Multi-byte pixels must not be split across packets, so the
        // payload size is rounded down to a pixel multiple. The clamp is
        // to at least one pixel BEFORE aligning down: a payload of 0 would
        // spin the emit loop forever (offset never advances).
        let align = if self.pixel_format == PFNC_MONO16 { 2 } else { 1 };
        self.payload_size = requested.clamp(align, MAX_PAYLOAD) / align * align;
    }

    fn bytes_per_pixel(&self) -> usize {
        if self.pixel_format == PFNC_MONO16 { 2 } else { 1 }
    }

    fn image_len(&self) -> usize {
        let pixels = u64::from(self.width) * u64::from(self.height);
        pixels.saturating_mul(self.bytes_per_pixel() as u64).min(1 << 31) as usize
    }
}

/// One cached block's datagrams for PACKETRESEND (§16.7.2). A real device
/// has limited resend memory — §16.7.3 tells requesters to ask "as early
/// as possible" — so the sim keeps only the most recent block per
/// channel. `packets` holds (packet_id, GVSP datagram) pairs: the leader
/// is id 0, payloads run from 1 (R-264st), the trailer is last.
#[derive(Clone)]
struct BlockCache {
    block_id: u64,
    packets: Vec<(u32, Vec<u8>)>,
}

/// Emitter state of one open stream channel. The scheduler owns one per
/// open channel; the socket handle is shared with the scheduler thread so
/// frames can be sent without holding the channel map lock for the whole
/// (blocking) send.
pub(crate) struct ChannelSink {
    socket: Arc<UdpSocket>,
    target: SocketAddr,
    params: StreamParams,
    /// Next local block id for this channel; restarts at 1 every time the
    /// channel opens (R-259st).
    next_block_id: u64,
    /// The most recently completed block, served to PACKETRESEND requests
    /// (§16.7.2). Replaced by the next frame's snapshot.
    last_block: Option<BlockCache>,
}

impl ChannelSink {
    pub fn open(target: SocketAddr, params: StreamParams) -> io::Result<Self> {
        // Dynamic source port per [O-086st].
        let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
        socket.set_nonblocking(true)?;
        Ok(ChannelSink {
            socket: Arc::new(socket),
            target,
            params,
            next_block_id: 1,
            last_block: None,
        })
    }

    /// Fire one §24.5 test packet: total IPv4 length `total_size`, the
    /// GVSP block_id field 0, the IP don't-fragment bit set. The payload
    /// is zero fill — the sim does not declare the GVCP Capability
    /// test_data bit, so CR-280st leaves the content "don't care".
    fn send_test_packet(&self, total_size: u32) {
        // §24.5: "IP Header (20 bytes) + UDP Header (8 bytes) + GVSP
        // Header + payload data is equal to the packet size" — the UDP
        // payload is the total minus the IP/UDP headers. The caller
        // passes the rounded stored size, so this always covers the
        // header plus at least one payload byte.
        let udp_len = total_size as usize - (20 + 8);
        let mut packet = [0u8; GVSP_HEADER_LEN + MAX_PAYLOAD];
        // The test packet does not follow the usual GVSP layout (§24.5):
        // it imitates the regular header of the device's form (EI don't
        // care, §24.5 NOTE) with the block_id field 0 (§16.7: "0 is
        // reserved for the test packet").
        write_header(&mut packet, self.params.legacy, PACKET_FORMAT_DATA_PAYLOAD, 0, 0);
        set_ip_do_not_fragment(&self.socket, true);
        send_test_datagram(&self.socket, self.target, &packet[..udp_len]);
        set_ip_do_not_fragment(&self.socket, false);
    }
}

/// Whole-frame striped transmitter over every open stream channel. One
/// scheduler thread per camera; it starts on the first channel open and
/// stops when the last channel closes (or on drop). The thread owns no
/// register-map locks, so stopping it can never deadlock against reads or
/// writes.
pub(crate) struct StreamScheduler {
    stop: Arc<AtomicBool>,
    /// Open channels' sinks, keyed by channel number. The scheduler
    /// thread re-snapshots the key set per frame.
    sinks: Arc<Mutex<HashMap<u16, ChannelSink>>>,
    /// Test hook (§16.7 regression): payload packets to drop on send.
    /// `None` means injection is off.
    drops: Arc<Mutex<Option<Arc<Mutex<PacketDrop>>>>>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl StreamScheduler {
    pub fn new() -> Self {
        StreamScheduler {
            stop: Arc::new(AtomicBool::new(false)),
            sinks: Arc::new(Mutex::new(HashMap::new())),
            drops: Arc::new(Mutex::new(None)),
            thread: Mutex::new(None),
        }
    }

    /// Test hook: start dropping the given payload packets (leader and
    /// trailer are never dropped — the loss model is payload-only). Pass
    /// a shared set the test mutates; `None` disables injection again.
    pub fn set_packet_drop(&self, drops: Option<Arc<Mutex<PacketDrop>>>) {
        *self.drops.lock().expect("sim scheduler poisoned") = drops;
    }

    /// PACKETRESEND (§16.7.2): retransmit the cached packets of `block_id`
    /// with packet_id in [first, last] (last = 0xFFFFFFFF reaches through
    /// the trailer). Resent packets carry GEV_FLAG_PACKET_RESEND
    /// (CR-204st) and status GEV_STATUS_PACKET_RESEND (Table 19-1); the
    /// 1.2 legacy header has no flag field, so there the status alone
    /// marks the packet. Unknown block or out-of-range ids produce a
    /// header-only error packet with GEV_STATUS_PACKET_UNAVAILABLE
    /// (CR-205st). A closed channel has no socket to answer from — the
    /// request is ignored (QoI, the requester gives up via its own
    /// timeout).
    pub fn packet_resend(&self, channel: u16, block_id: u64, first: u32, last: u32) {
        let plan = {
            let g = self.sinks.lock().expect("sim scheduler poisoned");
            let Some(sink) = g.get(&channel) else { return; };
            let resent = match &sink.last_block {
                Some(cache) if cache.block_id == block_id => cache
                    .packets
                    .iter()
                    .filter(|(id, _)| *id >= first && (last == u32::MAX || *id <= last))
                    .map(|(_, d)| {
                        let mut d = d.clone();
                        // CR-204st / Table 19-1: mark the retransmission.
                        d[0..2].copy_from_slice(&GEV_STATUS_PACKET_RESEND.to_be_bytes());
                        if !sink.params.legacy {
                            // The 3.0 header has the flag field; the 1.2
                            // legacy header does not.
                            let flag = u16::from_be_bytes([d[2], d[3]]) | GEV_FLAG_PACKET_RESEND;
                            d[2..4].copy_from_slice(&flag.to_be_bytes());
                        }
                        d
                    })
                    .collect::<Vec<_>>(),
                _ => Vec::new(),
            };
            (sink.socket.clone(), sink.target, resent, sink.params.legacy, block_id, first)
        };
        let (socket, target, resent, legacy, block_id, first) = plan;
        if resent.is_empty() {
            // CR-205st: nothing to resend — answer with a header-only
            // error packet carrying the first requested id.
            let hdr = GvspHeader {
                status: GEV_STATUS_PACKET_UNAVAILABLE,
                flag: 0,
                extended_id: !legacy,
                packet_format: PACKET_FORMAT_DATA_PAYLOAD,
                block_id,
                packet_id: first,
            };
            let datagram = if legacy {
                let mut b = [0u8; GVSP_LEGACY_HEADER_LEN];
                hdr.encode_legacy(&mut b);
                b.to_vec()
            } else {
                let mut b = [0u8; GVSP_HEADER_LEN];
                hdr.encode(&mut b);
                b.to_vec()
            };
            send_test_datagram(&socket, target, &datagram);
            return;
        }
        for d in resent {
            send_test_datagram(&socket, target, &d);
        }
    }

    /// Is the channel currently streaming?
    pub fn is_streaming(&self, channel: u16) -> bool {
        self.sinks.lock().expect("sim scheduler poisoned").contains_key(&channel)
    }

    /// Send one §24.5 test packet on `channel`. A fire on a channel that
    /// is not open is ignored: §11.7 allows a device to ignore a test
    /// packet requested while the stream channel is closed ("the device
    /// can either ignore the request or return GEV_STATUS_INVALID_PARAMETER").
    pub fn send_test_packet(&self, channel: u16, total_size: u32) {
        let g = self.sinks.lock().expect("sim scheduler poisoned");
        if let Some(sink) = g.get(&channel) {
            sink.send_test_packet(total_size);
        }
    }

    /// Apply a new SCPSx value to a running channel. The frozen-at-open
    /// snapshot assumes SCPSx never changes while the channel streams
    /// (the §36.58 block the sim enforces for plain writes); an accepted
    /// fire-carrying negotiation write (§11.7, Q33) changes the register
    /// mid-stream, and on a real device the SCPSx value is the size the
    /// transmitter uses for the blocks that follow — the running emitter
    /// adopts it.
    pub fn update_packet_size(&self, channel: u16, scps: u32) {
        let mut g = self.sinks.lock().expect("sim scheduler poisoned");
        if let Some(sink) = g.get_mut(&channel) {
            sink.params.repacket(scps);
        }
    }

    /// Register a freshly opened channel; returns the previous sink if the
    /// channel was somehow still registered (defensive; the caller drops
    /// it outside the locks).
    pub fn insert(&self, channel: u16, sink: ChannelSink) -> Option<ChannelSink> {
        let old = self
            .sinks
            .lock()
            .expect("sim scheduler poisoned")
            .insert(channel, sink);
        self.ensure_thread();
        old
    }

    /// Remove a closed channel; stops the scheduler thread when the last
    /// channel closes. The caller drops the returned sink outside the
    /// locks.
    pub fn remove(&self, channel: u16) -> Option<ChannelSink> {
        let removed = self
            .sinks
            .lock()
            .expect("sim scheduler poisoned")
            .remove(&channel);
        if removed.is_some() {
            // The thread mutex is held across the emptiness check and the
            // join so a racing insert restarts the thread only after the
            // old one is gone (single-threaded GVCP server in practice;
            // this is defense in depth).
            let mut thread = self.thread.lock().expect("sim scheduler thread poisoned");
            if self.sinks.lock().expect("sim scheduler poisoned").is_empty() {
                self.stop.store(true, Ordering::Relaxed);
                if let Some(t) = thread.take() {
                    let _ = t.join();
                }
            }
        }
        removed
    }

    fn ensure_thread(&self) {
        let mut thread = self.thread.lock().expect("sim scheduler thread poisoned");
        if thread.is_none() {
            self.stop.store(false, Ordering::Relaxed);
            let stop = self.stop.clone();
            let sinks = self.sinks.clone();
            let drops = self.drops.clone();
            *thread = Some(
                std::thread::Builder::new()
                    .name("gvsp-sim-scheduler".into())
                    .spawn(move || scheduler_loop(stop, sinks, drops))
                    .expect("spawn GVSP scheduler"),
            );
        }
    }
}

impl Drop for StreamScheduler {
    fn drop(&mut self) {
        // The thread's lifetime follows the register map; its Drop joins
        // the scheduler thread.
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.lock().expect("sim scheduler thread poisoned").take() {
            let _ = t.join();
        }
    }
}

/// Stripe loop: frame k goes to `open_channels[k % open_count]` with the
/// open set re-snapshotted per frame; the frame is fully sent before the
/// next one starts. Per-channel local block ids advance by one on each
/// visit (channel x's m-th frame carries local id m, R-259st).
fn scheduler_loop(
    stop: Arc<AtomicBool>,
    sinks: Arc<Mutex<HashMap<u16, ChannelSink>>>,
    drops: Arc<Mutex<Option<Arc<Mutex<PacketDrop>>>>>,
) {
    let mut frame_index = 0u64;
    let epoch = Instant::now();
    // One reusable datagram buffer; the GVSP header carries no length,
    // receivers derive sizes from the UDP datagram length.
    let mut packet = [0u8; GVSP_HEADER_LEN + MAX_PAYLOAD];
    while !stop.load(Ordering::Relaxed) {
        let keys: Vec<u16> = {
            let mut ks: Vec<u16> = sinks
                .lock()
                .expect("sim scheduler poisoned")
                .keys()
                .copied()
                .collect();
            ks.sort_unstable();
            ks
        };
        if keys.is_empty() {
            sleep_interruptible(IDLE_PAUSE, &stop);
            continue;
        }
        let channel = keys[(frame_index % keys.len() as u64) as usize];
        let (socket, target, params, block_id, drops) = {
            let mut g = sinks.lock().expect("sim scheduler poisoned");
            // Channel closed between the snapshot and this lock: skip the
            // frame (the next loop iteration re-snapshots).
            let Some(sink) = g.get_mut(&channel) else {
                frame_index += 1;
                continue;
            };
            let block_id = sink.next_block_id;
            sink.next_block_id += 1;
            let drops = drops.lock().expect("sim scheduler poisoned").clone();
            (sink.socket.clone(), sink.target, sink.params, block_id, drops)
        };
        // 1 MHz sim clock, same tick as the Timestamp Value registers.
        let timestamp = epoch.elapsed().as_micros() as u64;
        // The resend cache accumulates during the send and is published
        // as the channel's snapshot only when the block completes (the
        // lock is not held across the sends).
        let mut cache: Vec<(u32, Vec<u8>)> = Vec::new();
        send_block(
            &socket,
            target,
            &params,
            block_id,
            timestamp,
            &mut packet,
            &stop,
            &drops,
            &mut cache,
        );
        {
            let mut g = sinks.lock().expect("sim scheduler poisoned");
            if let Some(sink) = g.get_mut(&channel) {
                sink.last_block = Some(BlockCache { block_id, packets: cache });
            }
        }
        frame_index += 1;
        sleep_interruptible(FRAME_INTERVAL, &stop);
    }
}

/// Send one block: data leader (packet_id 0), data payloads (packet_id from
/// 1, R-264st), data trailer. All packets but the last payload carry exactly
/// `payload_size` bytes (R-263st); none ever exceeds it (R-573st). Header
/// form and leader body follow `params.legacy` (1.2 §24 Fig 24-1/Fig 24-3
/// vs 3.0 §24.2/§25.2.1); the trailer body is identical in both.
///
/// Every datagram is also appended to `cache` (the PACKETRESEND snapshot,
/// §16.7.2). `drops` is the test loss model: a listed payload packet is
/// cached but not sent — the device "sent" it, the network dropped it
/// (leader/trailer are never dropped, so a block always closes).
fn send_block(
    socket: &UdpSocket,
    target: SocketAddr,
    params: &StreamParams,
    block_id: u64,
    timestamp: u64,
    packet: &mut [u8],
    stop: &AtomicBool,
    drops: &Option<Arc<Mutex<PacketDrop>>>,
    cache: &mut Vec<(u32, Vec<u8>)>,
) {
    let hlen = gvsp_header_len(params.legacy);
    let image_len = params.image_len();

    write_header(packet, params.legacy, PACKET_FORMAT_DATA_LEADER, block_id, 0);
    let leader = ImageLeader {
        field_id: 0,
        field_count: 0,
        timestamp,
        pixel_format: params.pixel_format,
        size_x: params.width,
        size_y: params.height,
        offset_x: 0,
        offset_y: 0,
        padding_x: 0,
        padding_y: 0,
    };
    let leader_end = write_leader_body(packet, params.legacy, &leader);
    cache.push((0, packet[..leader_end].to_vec()));
    if !send_until_stopped(socket, target, &packet[..leader_end], stop) {
        return;
    }

    let dropped = |packet_id: u32| {
        drops.as_ref().is_some_and(|d| {
            d.lock().expect("sim packet drop poisoned").contains(&(block_id, packet_id))
        })
    };
    let mut packet_id = 1u32;
    let mut offset = 0usize;
    while offset < image_len {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        let n = params.payload_size.min(image_len - offset);
        fill_pattern(
            &mut packet[hlen..hlen + n],
            offset,
            block_id,
            params.bytes_per_pixel(),
        );
        write_header(packet, params.legacy, PACKET_FORMAT_DATA_PAYLOAD, block_id, packet_id);
        cache.push((packet_id, packet[..hlen + n].to_vec()));
        if !dropped(packet_id)
            && !send_until_stopped(socket, target, &packet[..hlen + n], stop)
        {
            return;
        }
        packet_id += 1;
        offset += n;
        if params.packet_delay > Duration::ZERO {
            sleep_interruptible(params.packet_delay, stop);
        }
    }

    write_header(packet, params.legacy, PACKET_FORMAT_DATA_TRAILER, block_id, packet_id);
    let body = encode_image_trailer_body(params.height);
    packet[hlen..hlen + body.len()].copy_from_slice(&body);
    cache.push((packet_id, packet[..hlen + body.len()].to_vec()));
    let _ = send_until_stopped(socket, target, &packet[..hlen + body.len()], stop);
}

/// GVSP header wire length of the configured form.
fn gvsp_header_len(legacy: bool) -> usize {
    if legacy {
        GVSP_LEGACY_HEADER_LEN
    } else {
        GVSP_HEADER_LEN
    }
}

/// Write the GVSP header in the configured form (1.2 §24 Fig 24-1 legacy
/// 8-byte or 3.0 §24.2 20-byte).
fn write_header(packet: &mut [u8], legacy: bool, packet_format: u8, block_id: u64, packet_id: u32) {
    let hdr = GvspHeader {
        status: 0,
        flag: 0,
        extended_id: !legacy,
        packet_format,
        block_id,
        packet_id,
    };
    if legacy {
        let buf: &mut [u8; GVSP_LEGACY_HEADER_LEN] = (&mut packet[..GVSP_LEGACY_HEADER_LEN])
            .try_into()
            .expect("gvsp legacy header buffer");
        hdr.encode_legacy(buf);
    } else {
        let buf: &mut [u8; GVSP_HEADER_LEN] = (&mut packet[..GVSP_HEADER_LEN])
            .try_into()
            .expect("gvsp header buffer");
        hdr.encode(buf);
    }
}

/// Encode the image leader body in the configured form (1.2 §24 Fig 24-3
/// 36-byte or 3.0 §25.2.1 40-byte), copy it after the header, and return
/// the total datagram length.
fn write_leader_body(packet: &mut [u8], legacy: bool, leader: &ImageLeader) -> usize {
    let hlen = gvsp_header_len(legacy);
    if legacy {
        let body = encode_image_leader_body_legacy(leader);
        packet[hlen..hlen + body.len()].copy_from_slice(&body);
        hlen + body.len()
    } else {
        let body = encode_image_leader_body(leader);
        packet[hlen..hlen + body.len()].copy_from_slice(&body);
        hlen + body.len()
    }
}

/// Deterministic gradient: pixel `i` derives from `(i + block_id)`, letting
/// a receiver verify both the payload split and the block numbering
/// (mod 256 for Mono8, mod 65536 for Mono16).
fn fill_pattern(buf: &mut [u8], byte_offset: usize, block_id: u64, bpp: usize) {
    if bpp == 1 {
        for (k, b) in buf.iter_mut().enumerate() {
            *b = (byte_offset + k + block_id as usize) as u8;
        }
    } else {
        let first_pixel = byte_offset / 2;
        for (p, chunk) in buf.chunks_exact_mut(2).enumerate() {
            // Multi-byte pixel data is little-endian on the wire (R-344st).
            let v = (first_pixel + p + block_id as usize) as u16;
            chunk.copy_from_slice(&v.to_le_bytes());
        }
    }
}

/// Send one test datagram, waiting out send-buffer backpressure for up to
/// 100 ms (a large test packet can exceed the default send buffer on a
/// busy socket). Hard send errors and the deadline expiring are ignored
/// like on the streaming path: a vanished receiver must not wedge the
/// register write that fired the packet.
fn send_test_datagram(socket: &UdpSocket, target: SocketAddr, buf: &[u8]) {
    let deadline = Instant::now() + Duration::from_millis(100);
    loop {
        match socket.send_to(buf, target) {
            Ok(_) => return,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(_) => return,
        }
    }
}

/// Set/clear IP_DONTFRAGMENT on the sink socket for a §24.5 test packet
/// ("The don't fragment bit must also be set in the IP Header"). Best
/// effort: a failed setsockopt must not block the test packet.
fn set_ip_do_not_fragment(socket: &UdpSocket, on: bool) {
    let flag: i32 = i32::from(on);
    unsafe {
        setsockopt(
            socket.as_raw_socket() as usize,
            IPPROTO_IP,
            IP_DONTFRAGMENT,
            (&flag as *const i32).cast(),
            std::mem::size_of::<i32>() as i32,
        );
    }
}

/// Send one datagram, waiting out send-buffer backpressure. Returns false
/// when asked to stop while waiting. Hard send errors (e.g. the receiver
/// went away) are ignored like on a real camera: streaming continues.
fn send_until_stopped(socket: &UdpSocket, target: SocketAddr, buf: &[u8], stop: &AtomicBool) -> bool {
    loop {
        match socket.send_to(buf, target) {
            Ok(_) => return true,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                if stop.load(Ordering::Relaxed) {
                    return false;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(_) => return true,
        }
    }
}

/// Sleep up to `d`, returning early when `stop` is set.
fn sleep_interruptible(d: Duration, stop: &AtomicBool) {
    let start = Instant::now();
    while start.elapsed() < d {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::regmap::SimRegMap;
    use crate::{SimConfig, StreamConfig, StreamingProtocol};
    use gentl_gvcp::registers as reg;
    use gentl_gvcp::wire::{GEV_STATUS_BUSY, GEV_STATUS_INVALID_ADDRESS};
    use gentl_gvcp::DeviceHandler;
    use std::net::UdpSocket;

    const MONO8: u32 = 0x0108_0001;

    fn map_with_stream(stream: StreamConfig) -> SimRegMap {
        SimRegMap::new(
            SimConfig {
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
            stream,
        )
    }

    /// Local receiver socket with channel `x` opened towards it.
    fn open_channel_x(m: &SimRegMap, x: u32, scps: Option<u32>) -> UdpSocket {
        let rx = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        rx.set_read_timeout(Some(Duration::from_millis(500)))
            .unwrap();
        let port = rx.local_addr().unwrap().port();
        m.write_reg(reg::scda(x), u32::from(Ipv4Addr::LOCALHOST))
            .unwrap();
        if let Some(v) = scps {
            m.write_reg(reg::scps(x), v).unwrap();
        }
        m.write_reg(reg::scp(x), reg::scp_make(port)).unwrap();
        rx
    }

    /// Channel 0 variant of [`open_channel_x`].
    fn open_channel(m: &SimRegMap, scps0: Option<u32>) -> UdpSocket {
        open_channel_x(m, 0, scps0)
    }

    struct Packet {
        header: GvspHeader,
        data: Vec<u8>,
    }

    fn recv_packet(rx: &UdpSocket) -> Packet {
        let mut buf = [0u8; 65536];
        let (n, _) = rx.recv_from(&mut buf).expect("gvsp datagram");
        let header = GvspHeader::decode(&buf[..n]).expect("gvsp header");
        Packet {
            header,
            data: buf[GVSP_HEADER_LEN..n].to_vec(),
        }
    }

    /// Receive one full block: leader, `n_payloads` payloads, trailer.
    fn recv_block(rx: &UdpSocket, n_payloads: usize) -> Vec<Packet> {
        (0..n_payloads + 2).map(|_| recv_packet(rx)).collect()
    }

    /// Assert the socket sees no further datagrams (~`window` of silence).
    fn assert_silent(rx: &UdpSocket, window: Duration) {
        let mut buf = [0u8; 2048];
        rx.set_read_timeout(Some(Duration::from_millis(50))).unwrap();
        // Flush anything still queued from before the channel closed.
        while rx.recv_from(&mut buf).is_ok() {}
        rx.set_read_timeout(Some(window)).unwrap();
        assert!(
            rx.recv_from(&mut buf).is_err(),
            "datagrams arriving on a closed channel"
        );
    }

    /// Receive datagrams until `pred` matches or the deadline passes.
    /// Returns the matching packet's UDP datagram length and decoded
    /// header.
    fn recv_until(rx: &UdpSocket, mut pred: impl FnMut(usize, &GvspHeader) -> bool) -> Option<(usize, GvspHeader)> {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut buf = [0u8; 65536];
        while Instant::now() < deadline {
            rx.set_read_timeout(Some(Duration::from_millis(200))).unwrap();
            let Ok((n, _)) = rx.recv_from(&mut buf) else {
                continue;
            };
            let Some(header) = GvspHeader::decode(&buf[..n]) else {
                continue;
            };
            if pred(n, &header) {
                return Some((n, header));
            }
        }
        None
    }

    /// §11.7/§24.5 (notes/OPEN-QUESTIONS.md Q33): a fire-carrying SCPS0
    /// write is accepted even while the channel streams (a plain write
    /// stays BUSY) and fires exactly one test datagram — UDP length
    /// size-28 (IP 20 + UDP 8), GVSP block_id field 0. The register
    /// stores the rounded size with the self-cleared fire bit, and a
    /// second fire produces a second test datagram.
    #[test]
    fn fire_test_packet_sends_one_sized_test_datagram() {
        let m = map_with_stream(StreamConfig {
            width: 16,
            height: 16,
            pixel_format: MONO8,
            channels: 1,
            legacy_headers: false,
        });
        let rx = open_channel(&m, None);
        // Plain write while streaming: still blocked (§36.58).
        assert_eq!(
            m.write_reg(reg::SCPS0, reg::scps_make(1000)).unwrap_err(),
            GEV_STATUS_BUSY
        );
        let size = 1000 + reg::GVSP_HEADERS_OVERHEAD as u32; // total 1048
        m.write_reg(
            reg::SCPS0,
            reg::scps_make(size) | reg::SCPS_FIRE_TEST_PACKET,
        )
        .unwrap();
        // The auto-stream runs concurrently; pull until the test datagram
        // (block_id 0, exactly size-28 UDP bytes) shows up.
        let test = recv_until(&rx, |n, h| n == size as usize - 28 && h.block_id == 0);
        assert!(test.is_some(), "one test datagram of the declared size");
        // The register stores the rounded size with the fire bit cleared.
        let stored = m.read_reg(reg::SCPS0).unwrap();
        assert_eq!(
            stored & reg::SCPS_FIRE_TEST_PACKET,
            0,
            "fire_test_packet self-cleared (§36.58)"
        );
        assert_eq!(reg::scps_packet_size(stored), size);
        // Self-clearing is per write: firing again emits a second packet.
        m.write_reg(
            reg::SCPS0,
            reg::scps_make(size) | reg::SCPS_FIRE_TEST_PACKET,
        )
        .unwrap();
        let second = recv_until(&rx, |n, h| n == size as usize - 28 && h.block_id == 0);
        assert!(second.is_some(), "second fire, second test datagram");
        m.write_reg(reg::SCP0, 0).unwrap();
    }

    /// R-562st/Q33 rounding: sizes clamp to the sim's [49, 65535] domain
    /// and the register stores the rounded value — a clamped write still
    /// fires its (rounded) test packet.
    #[test]
    fn scps_write_rounding_clamps_to_supported_domain() {
        let m = map_with_stream(StreamConfig {
            width: 16,
            height: 16,
            pixel_format: MONO8,
            channels: 1,
            legacy_headers: false,
        });
        // Above the u16 maximum is impossible via the mask; probe the low
        // end instead: 0 rounds up to 49 (48 header bytes + 1 payload byte).
        m.write_reg(reg::SCPS0, reg::scps_make(0)).unwrap();
        assert_eq!(reg::scps_packet_size(m.read_reg(reg::SCPS0).unwrap()), 49);
        // A fire on the closed channel is ignored (§11.7: the device can
        // ignore or reject it) and no datagram can arrive anywhere.
        m.write_reg(
            reg::SCPS0,
            reg::scps_make(1000) | reg::SCPS_FIRE_TEST_PACKET,
        )
        .unwrap();
        assert_eq!(
            reg::scps_packet_size(m.read_reg(reg::SCPS0).unwrap()),
            1000,
            "the rounded size is stored even when nothing fires"
        );
        assert_eq!(
            m.read_reg(reg::SCPS0).unwrap() & reg::SCPS_FIRE_TEST_PACKET,
            0
        );
    }

    /// §16.7.2 (M3-1): a PACKETRESEND request retransmits the cached
    /// block's packets with the resend markers — GEV_FLAG_PACKET_RESEND
    /// (CR-204st) and status GEV_STATUS_PACKET_RESEND (Table 19-1) — and
    /// an unknown block answers with a header-only
    /// GEV_STATUS_PACKET_UNAVAILABLE packet (CR-205st).
    #[test]
    fn packet_resend_retransmits_marked_and_errors_on_unknown_block() {
        let m = map_with_stream(StreamConfig {
            width: 16,
            height: 16,
            pixel_format: MONO8,
            channels: 1,
            legacy_headers: false,
        });
        // 100-byte payloads: 256 bytes -> 3 payload packets per block.
        let rx = open_channel(&m, Some(reg::scps_make(100 + reg::GVSP_HEADERS_OVERHEAD)));
        recv_block(&rx, 3);
        // Resend packet 2 of block 1: marked, payload intact. The
        // scheduler keeps streaming new blocks concurrently, so pull
        // until the marked retransmission shows up.
        m.packet_resend(0, 1, 2, 2);
        let got = recv_until(&rx, |n, h| {
            n == GVSP_HEADER_LEN + 100
                && h.block_id == 1
                && h.packet_id == 2
                && h.flag & GEV_FLAG_PACKET_RESEND == GEV_FLAG_PACKET_RESEND
                && h.status == gentl_gvcp::wire::GEV_STATUS_PACKET_RESEND
        });
        assert!(got.is_some(), "marked retransmission of packet 2");
        // Unknown block: header-only error packet naming the first id.
        m.packet_resend(0, 999, 1, 1);
        let got = recv_until(&rx, |n, h| {
            n == GVSP_HEADER_LEN && h.status == gentl_gvcp::wire::GEV_STATUS_PACKET_UNAVAILABLE
        });
        let (_, h) = got.expect("CR-205st error packet");
        assert_eq!(h.packet_id, 1);
        m.write_reg(reg::SCP0, 0).unwrap();
    }

    /// Test hook (M3-1): the drop set removes exactly the listed payload
    /// packets; the leader always gets through and the block still closes
    /// with its trailer (payload-only loss model).
    #[test]
    fn packet_drop_injection_skips_listed_payloads_only() {
        let m = map_with_stream(StreamConfig {
            width: 16,
            height: 16,
            pixel_format: MONO8,
            channels: 1,
            legacy_headers: false,
        });
        let drops = Arc::new(Mutex::new(PacketDrop::new()));
        drops.lock().unwrap().insert((1, 1));
        m.set_packet_drop(drops);
        let rx = open_channel(&m, Some(reg::scps_make(100 + reg::GVSP_HEADERS_OVERHEAD)));
        // Block 1 = leader + payloads 2,3 (1 dropped) + trailer.
        let pkts = recv_block(&rx, 2);
        assert_eq!(pkts[0].header.packet_format, PACKET_FORMAT_DATA_LEADER);
        assert_eq!(pkts[1].header.packet_id, 2, "payload 1 dropped");
        assert_eq!(pkts[2].header.packet_id, 3);
        assert_eq!(pkts[3].header.packet_format, PACKET_FORMAT_DATA_TRAILER);
        m.write_reg(reg::SCP0, 0).unwrap();
    }

    #[test]
    fn emits_mono8_blocks_matching_scps_and_pattern() {
        let m = map_with_stream(StreamConfig {
            width: 64,
            height: 48,
            pixel_format: MONO8,
            channels: 1,
            legacy_headers: false,
        });
        // SCPS0 in the spec layout: total packet size 1000+48 -> a 1000-byte
        // payload per full packet.
        let rx = open_channel(&m, Some(reg::scps_make(1000 + reg::GVSP_HEADERS_OVERHEAD)));
        // 64 * 48 = 3072 bytes -> 1000 + 1000 + 1000 + 72.
        for block_idx in 0..10 {
            let block_id = (block_idx + 1) as u64;
            let pkts = recv_block(&rx, 4);

            let leader = &pkts[0];
            assert_eq!(leader.header.packet_format, PACKET_FORMAT_DATA_LEADER);
            assert_eq!(leader.header.packet_id, 0);
            assert_eq!(leader.header.block_id, block_id);
            assert!(leader.header.extended_id);
            let l = ImageLeader::decode(&leader.data).expect("leader body");
            assert_eq!(l.pixel_format, MONO8);
            assert_eq!(l.size_x, 64);
            assert_eq!(l.size_y, 48);

            for (i, p) in pkts[1..5].iter().enumerate() {
                assert_eq!(p.header.packet_format, PACKET_FORMAT_DATA_PAYLOAD);
                assert_eq!(p.header.packet_id, (i + 1) as u32);
                assert_eq!(p.header.block_id, block_id);
                let want = if i < 3 { 1000 } else { 72 };
                assert_eq!(p.data.len(), want, "payload {i} size");
                let base = i * 1000;
                for (k, &b) in p.data.iter().enumerate() {
                    assert_eq!(
                        b,
                        (base + k + block_id as usize) as u8,
                        "payload {i} byte {k}"
                    );
                }
            }

            let trailer = &pkts[5];
            assert_eq!(trailer.header.packet_format, PACKET_FORMAT_DATA_TRAILER);
            assert_eq!(trailer.header.packet_id, 5);
            assert_eq!(decode_image_trailer_body(&trailer.data).unwrap(), 48);
        }

        m.write_reg(reg::SCP0, 0).unwrap();
        assert_eq!(m.read_reg(reg::SCP0).unwrap(), 0);
        assert_silent(&rx, Duration::from_millis(300));
    }

    /// The factory (unwritten) SCPS0 keeps the 1400-byte default payload;
    /// an explicit 0 write is no longer an "unwritten" sentinel — R-562st
    /// rounding (Q33: clamp to the sim's [49, 65535] domain) stores 49,
    /// and the stream adopts the 1-byte minimum payload.
    #[test]
    fn scps_unwritten_default_and_explicit_zero_rounding() {
        let m = map_with_stream(StreamConfig {
            width: 128,
            height: 20,
            pixel_format: MONO8,
            channels: 1,
            legacy_headers: false,
        });
        // Factory default: total packet size 1400+48 (spec layout), no
        // write at all.
        assert_eq!(
            m.read_reg(reg::SCPS0).unwrap(),
            reg::scps_make(DEFAULT_PAYLOAD as u32 + reg::GVSP_HEADERS_OVERHEAD)
        );
        let rx = open_channel(&m, None);
        let pkts = recv_block(&rx, 2);
        assert_eq!(pkts[1].data.len(), 1400);
        assert_eq!(pkts[2].data.len(), 2560 - 1400);
        m.write_reg(reg::SCP0, 0).unwrap();
        drop(rx);

        // An explicit 0 rounds up to 49 (48 header bytes + 1 payload byte)
        // and the register stores the rounded value.
        let rx = open_channel(&m, Some(0));
        assert_eq!(
            reg::scps_packet_size(m.read_reg(reg::SCPS0).unwrap()),
            49,
            "R-562st: the explicit 0 is rounded and stored"
        );
        let leader = recv_packet(&rx);
        assert_eq!(leader.header.packet_format, PACKET_FORMAT_DATA_LEADER);
        let p1 = recv_packet(&rx);
        assert_eq!(p1.header.packet_format, PACKET_FORMAT_DATA_PAYLOAD);
        assert_eq!(p1.data.len(), 1, "payload = 49 - 48 headers");
        m.write_reg(reg::SCP0, 0).unwrap();
    }

    #[test]
    fn stream_params_interpret_scps0_in_spec_layout() {
        let stream = StreamConfig {
            width: 64,
            height: 48,
            pixel_format: MONO8,
            channels: 1,
            legacy_headers: false,
        };
        // Total packet size 1448 -> 1400 payload bytes (the sim default).
        let p = StreamParams::new(&stream, reg::scps_make(1448), 0);
        assert_eq!(p.payload_size, 1400);
        // Flag bits (fire_test_packet / do_not_fragment) are not size bits.
        let p = StreamParams::new(
            &stream,
            reg::scps_make(1000) | reg::SCPS_FIRE_TEST_PACKET | reg::SCPS_DO_NOT_FRAGMENT,
            0,
        );
        assert_eq!(p.payload_size, 1000 - reg::GVSP_HEADERS_OVERHEAD as usize);
        // Unwritten (0) or smaller-than-headers values fall back to 1400.
        assert_eq!(StreamParams::new(&stream, 0, 0).payload_size, DEFAULT_PAYLOAD);
        assert_eq!(
            StreamParams::new(&stream, reg::scps_make(10), 0).payload_size,
            DEFAULT_PAYLOAD
        );
        // Multi-byte pixels round the payload down to a pixel multiple.
        let s16 = StreamConfig {
            width: 64,
            height: 48,
            pixel_format: PFNC_MONO16,
            channels: 1,
            legacy_headers: false,
        };
        assert_eq!(
            StreamParams::new(&s16, reg::scps_make(999), 0).payload_size,
            (999 - reg::GVSP_HEADERS_OVERHEAD as usize) / 2 * 2
        );
    }

    #[test]
    fn mono16_pattern_is_little_endian() {
        let m = map_with_stream(StreamConfig {
            width: 4,
            height: 2,
            pixel_format: PFNC_MONO16,
            channels: 1,
            legacy_headers: false,
        });
        // 4 * 2 pixels * 2 bytes = 16 bytes -> a single payload packet.
        let rx = open_channel(&m, None);
        let pkts = recv_block(&rx, 1);
        let l = ImageLeader::decode(&pkts[0].data).expect("leader body");
        assert_eq!(l.pixel_format, PFNC_MONO16);
        assert_eq!(l.size_x, 4);
        assert_eq!(l.size_y, 2);
        let data = &pkts[1].data;
        assert_eq!(data.len(), 16);
        for p in 0..8 {
            let v = u16::from_le_bytes([data[2 * p], data[2 * p + 1]]);
            assert_eq!(v, (p + 1) as u16, "pixel {p}");
        }
        assert_eq!(decode_image_trailer_body(&pkts[2].data).unwrap(), 2);
        m.write_reg(reg::SCP0, 0).unwrap();
    }

    /// Receive one datagram with per-datagram header-form discrimination
    /// (decode_auto) and return the datagram length, the decoded header,
    /// and the body after the header.
    fn recv_packet_auto(rx: &UdpSocket) -> (usize, GvspHeader, Vec<u8>) {
        let mut buf = [0u8; 65536];
        let (n, _) = rx.recv_from(&mut buf).expect("gvsp datagram");
        let (header, hlen) = GvspHeader::decode_auto(&buf[..n]).expect("decodable header");
        (n, header, buf[hlen..n].to_vec())
    }

    /// M3-9: `legacy_headers: true` emits the 1.2 wire form — 8-byte
    /// headers (1.2 §24 Fig 24-1: status u16 / block_id u16 / format u8 /
    /// packet_id u24, no flag field), the 36-byte leader body (Fig 24-3:
    /// reserved u16 + payload_type + timestamp + 5 x u32 + u16 paddings),
    /// and the same 8-byte trailer body as 3.0. SCPSx counts the 36-byte
    /// header budget (1.2 §27.46).
    #[test]
    fn legacy_mode_emits_12_style_frames() {
        let m = map_with_stream(StreamConfig {
            width: 16,
            height: 16,
            pixel_format: MONO8,
            channels: 1,
            legacy_headers: true,
        });
        // Factory default SCPS0: 1400 payload + the 36-byte legacy budget.
        assert_eq!(
            reg::scps_packet_size(m.read_reg(reg::SCPS0).unwrap()),
            DEFAULT_PAYLOAD as u32 + GVSP_LEGACY_HEADERS_OVERHEAD,
            "1.2 SCPSx counts IP+UDP+8-byte GVSP header"
        );
        let rx = open_channel(&m, None);
        // 16*16 Mono8 = 256 bytes < 1400 payload -> one payload packet.
        let (n, header, body) = recv_packet_auto(&rx);
        assert_eq!(n, GVSP_LEGACY_HEADER_LEN + IMAGE_LEADER_LEGACY_BODY_LEN);
        assert!(!header.extended_id, "legacy form: EI clear");
        assert_eq!(header.packet_format, PACKET_FORMAT_DATA_LEADER);
        assert_eq!(header.packet_id, 0);
        assert_eq!(header.block_id, 1, "R-259st: ids from 1 per channel");
        // Leader body (1.2 Fig 24-3): reserved u16, payload_type u16 = 1,
        // timestamp u64, pixel_format, size_x, size_y, offset_x, offset_y,
        // padding_x u16, padding_y u16.
        assert_eq!(body.len(), IMAGE_LEADER_LEGACY_BODY_LEN);
        assert_eq!(&body[0..2], &[0, 0], "reserved u16");
        assert_eq!(u16::from_be_bytes([body[2], body[3]]), 1, "payload_type");
        assert_eq!(
            u32::from_be_bytes(body[12..16].try_into().unwrap()),
            MONO8,
            "pixel_format"
        );
        assert_eq!(u32::from_be_bytes(body[16..20].try_into().unwrap()), 16);
        assert_eq!(u32::from_be_bytes(body[20..24].try_into().unwrap()), 16);
        assert_eq!(u32::from_be_bytes(body[24..28].try_into().unwrap()), 0);
        assert_eq!(u32::from_be_bytes(body[28..32].try_into().unwrap()), 0);
        assert_eq!(u16::from_be_bytes([body[32], body[33]]), 0, "padding_x u16");
        assert_eq!(u16::from_be_bytes([body[34], body[35]]), 0, "padding_y u16");

        // Payload: 8-byte header, u24 packet_id, then the pattern bytes.
        let (n, header, body) = recv_packet_auto(&rx);
        assert_eq!(n, GVSP_LEGACY_HEADER_LEN + 256);
        assert_eq!(header.packet_format, PACKET_FORMAT_DATA_PAYLOAD);
        assert_eq!(header.packet_id, 1, "packet_id u24, R-264st from 1");
        assert_eq!(header.block_id, 1);
        assert_eq!(body.len(), 256);
        for (k, &b) in body.iter().enumerate() {
            assert_eq!(b, (k + 1) as u8, "legacy payload byte {k}");
        }

        // Trailer: 8-byte header + the same 8-byte body as 3.0
        // (reserved u16 + payload_type u16 + size_y u32).
        let (n, header, body) = recv_packet_auto(&rx);
        assert_eq!(n, GVSP_LEGACY_HEADER_LEN + IMAGE_TRAILER_LEN);
        assert_eq!(header.packet_format, PACKET_FORMAT_DATA_TRAILER);
        assert_eq!(u16::from_be_bytes([body[2], body[3]]), 1, "payload_type");
        assert_eq!(u32::from_be_bytes(body[4..8].try_into().unwrap()), 16, "size_y");
        m.write_reg(reg::SCP0, 0).unwrap();
    }

    /// 1.2 §27.46: in legacy mode the SCPSx write/readback semantics use
    /// the 36-byte header budget (contrast with the 3.0 interpretation in
    /// `stream_params_interpret_scps0_in_spec_layout`).
    #[test]
    fn legacy_scps_semantics_use_36_byte_budget() {
        let stream = StreamConfig {
            width: 64,
            height: 48,
            pixel_format: MONO8,
            channels: 1,
            legacy_headers: true,
        };
        // Total 1444 -> 1408 payload bytes (1444 - 36).
        let p = StreamParams::new(&stream, reg::scps_make(1444), 0);
        assert_eq!(p.payload_size, 1408);
        assert!(p.legacy);
        // Rounding floors the payload to a pixel multiple within the
        // 36-byte budget.
        let p = StreamParams::new(&stream, reg::scps_make(999), 0);
        assert_eq!(p.payload_size, 999 - GVSP_LEGACY_HEADERS_OVERHEAD as usize);
    }

    #[test]
    fn block_id_resets_on_reopen_and_scp_is_busy_while_open() {
        let m = map_with_stream(StreamConfig::default());
        let rx = open_channel(&m, None);
        assert_eq!(recv_packet(&rx).header.block_id, 1);
        // R-586cd: a non-zero SCPx write while streaming is rejected.
        assert_eq!(
            m.write_reg(reg::SCP0, reg::scp_make(1234)).unwrap_err(),
            GEV_STATUS_BUSY
        );
        m.write_reg(reg::SCP0, 0).unwrap();
        drop(rx);

        // R-259st: the block id restarts at 1 on the new channel.
        let rx = open_channel(&m, None);
        assert_eq!(recv_packet(&rx).header.block_id, 1);
        m.write_reg(reg::SCP0, 0).unwrap();
    }

    /// M3-4a: a two-channel camera stripe-schedules whole frames across
    /// its open channels — frame k goes to channel k % open_count, fully
    /// sent (leader/payloads/trailer) before the next frame starts, so
    /// cross-channel blocks never interleave. Each channel keeps its own
    /// local block_id sequence starting at 1 (R-259st); with striping,
    /// channel x's m-th frame carries local id m. Also: channels past the
    /// device count answer GEV_STATUS_INVALID_ADDRESS (§36.57 probe
    /// semantics); SCPSx/SCPDx are not writable while streaming
    /// (§36.58/§36.59); closing one channel leaves the others running.
    #[test]
    fn two_channels_independent_sequences_probe_busy_and_close() {
        let m = map_with_stream(StreamConfig {
            width: 16,
            height: 16,
            pixel_format: MONO8,
            channels: 2,
            legacy_headers: false,
        });
        assert_eq!(m.read_reg(reg::NUM_STREAM_CHANNELS).unwrap(), 2);
        // §36.57: channels 1..511 are optional; channel 5 does not exist
        // on this device, so reads AND writes answer INVALID_ADDRESS.
        assert_eq!(
            m.read_reg(reg::scp(5)).unwrap_err(),
            GEV_STATUS_INVALID_ADDRESS
        );
        assert_eq!(
            m.read_reg(reg::scps(5)).unwrap_err(),
            GEV_STATUS_INVALID_ADDRESS
        );
        assert_eq!(
            m.write_reg(reg::scp(5), reg::scp_make(9)).unwrap_err(),
            GEV_STATUS_INVALID_ADDRESS
        );
        assert_eq!(
            m.write_reg(reg::scda(5), 0).unwrap_err(),
            GEV_STATUS_INVALID_ADDRESS
        );
        // Both channels carry the §36.58 factory default.
        assert_eq!(m.read_reg(reg::scps(0)).unwrap(), m.read_reg(reg::scps(1)).unwrap());
        assert_ne!(m.read_reg(reg::scps(1)).unwrap(), 0);

        // Open both channels. ch0 may emit a brief lead-in while it is
        // the only open channel (between the two opens); the stripe
        // invariant below holds regardless.
        let rx0 = open_channel_x(&m, 0, None);
        let rx1 = open_channel_x(&m, 1, None);
        assert_ne!(reg::scp_host_port(m.read_reg(reg::scp(0)).unwrap()), 0);
        assert_ne!(reg::scp_host_port(m.read_reg(reg::scp(1)).unwrap()), 0);

        // 16*16 Mono8 = 256 bytes -> leader + one payload + trailer per
        // frame. Assert one complete, non-interleaved block: all packets
        // on one socket, same block_id, leader/payload/trailer in order.
        let assert_block = |pkts: &[Packet], block_id: u64, name: &str| {
            assert_eq!(pkts.len(), 3, "{name} packet count");
            assert_eq!(pkts[0].header.packet_format, PACKET_FORMAT_DATA_LEADER, "{name} leader");
            assert_eq!(pkts[0].header.packet_id, 0, "{name} leader packet id");
            assert_eq!(pkts[1].header.packet_format, PACKET_FORMAT_DATA_PAYLOAD, "{name} payload");
            assert_eq!(pkts[1].header.packet_id, 1, "{name} payload packet id");
            assert_eq!(pkts[2].header.packet_format, PACKET_FORMAT_DATA_TRAILER, "{name} trailer");
            for (i, p) in pkts.iter().enumerate() {
                assert_eq!(p.header.block_id, block_id, "{name} packet {i} block id");
            }
        };

        // ch1's very first frame must carry local id 1 (R-259st).
        let first = recv_block(&rx1, 1);
        assert_block(&first, 1, "ch1 first");
        // With two open channels the wire strictly alternates: after any
        // ch1 frame comes a ch0 frame, then ch1's next (local id +1), ...
        // Per-channel local ids are consecutive from each channel's queue
        // head (ch1: 2, 3, ...; ch0: lead-in head, head+1, ...).
        let mut last0: Option<u64> = None;
        let mut last1: u64 = 1;
        for _ in 2..=5u64 {
            let b0 = recv_block(&rx0, 1);
            let id0 = b0[0].header.block_id;
            if let Some(prev) = last0 {
                assert_eq!(id0, prev + 1, "ch0 striped sequence");
            } else {
                assert!(id0 >= 1, "ch0 head id");
            }
            assert_block(&b0, id0, "ch0");
            last0 = Some(id0);

            let b1 = recv_block(&rx1, 1);
            let id1 = b1[0].header.block_id;
            assert_eq!(id1, last1 + 1, "ch1 striped sequence");
            assert_block(&b1, id1, "ch1");
            last1 = id1;
        }

        // §36.58/§36.59: SCPSx/SCPDx writes are rejected while the
        // channel streams (the values were frozen at open); R-586cd
        // rejects a non-zero SCPx write.
        assert_eq!(
            m.write_reg(reg::scps(1), reg::scps_make(1000)).unwrap_err(),
            GEV_STATUS_BUSY
        );
        assert_eq!(m.write_reg(reg::scpd(1), 7).unwrap_err(), GEV_STATUS_BUSY);
        assert_eq!(
            m.write_reg(reg::scps(0), reg::scps_make(1000)).unwrap_err(),
            GEV_STATUS_BUSY
        );
        assert_eq!(
            m.write_reg(reg::scp(1), reg::scp_make(1234)).unwrap_err(),
            GEV_STATUS_BUSY
        );

        // Closing ch0 removes it from the stripe; ch1 keeps its local
        // sequence (ids already in flight may be queued, so ≥ last+1).
        m.write_reg(reg::scp(0), 0).unwrap();
        assert_eq!(m.read_reg(reg::scp(0)).unwrap(), 0);
        let next = recv_packet(&rx1).header.block_id;
        assert!(next >= last1 + 1, "ch1 continued across ch0 close (got {next})");
        assert_silent(&rx0, Duration::from_millis(200));

        m.write_reg(reg::scp(1), 0).unwrap();
        assert_silent(&rx1, Duration::from_millis(200));
    }
}
