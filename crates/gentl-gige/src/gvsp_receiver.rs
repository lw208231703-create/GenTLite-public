//! GVSP receiver: per-datagram reassembly of image blocks (leader +
//! payload packets + trailer) into announced buffers, plus the receive
//! thread reading the stream socket. Wire format: the GVSP section of
//! notes/gvcp-wirefacts.md (spec §24–25), codec in `gentl_gvcp::gvsp`.
//!
//! Reassembly is zero-copy: payload bytes are written directly into the
//! buffer reserved at the leader; core finalizes at the trailer. Blocks
//! with in-block packet gaps are dropped (M2 has no resend; that is M3).

use std::collections::BTreeMap;
use std::io;
use std::net::UdpSocket;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gentl_core::ds::{DsCore, FrameError, FrameMeta, FrameSink, FrameTarget};
use gentl_gvcp::gvsp::*;
use gentl_gvcp::wire::{
    GEV_STATUS_NOT_YET_AVAILABLE, GEV_STATUS_PACKET_AND_PREV_REMOVED_FROM_MEMORY,
    GEV_STATUS_PACKET_REMOVED_FROM_MEMORY, GEV_STATUS_PACKET_UNAVAILABLE,
    GEV_STATUS_TEMPORARILY_UNAVAILABLE,
};
use gentl_gvcp::GvcpClient;

/// Receive buffer size; covers the largest possible UDP datagram.
const RECV_BUF_LEN: usize = 65536;
/// Socket read timeout: the poll cadence for stop/kill flags.
const SOCKET_TIMEOUT: Duration = Duration::from_millis(100);
/// §16.7.3 resend-window poll cadence: while a block has missing packets
/// the loop polls at 1 ms so requests go out promptly (QoI).
const RESEND_POLL: Duration = Duration::from_millis(1);
/// Grace period after a graceful stop request while waiting for the
/// in-flight block's trailer before giving up on it (GVSP has no explicit
/// block-cancel; OPEN-QUESTION Q18).
const STOP_GRACE: Duration = Duration::from_secs(2);

// §16.7.3 resend timing — the spec leaves the mechanism and timing to the
// implementation ("quality of implementation"); the values follow the
// Aravis ecosystem (src/arvgvstreamprivate.h), which interops with the
// broadest range of GigE Vision devices.
//
/// Initial window before the first PACKETRESEND request for a newly
/// detected gap (INITIAL_PACKET_TIMEOUT, 1000 µs in Aravis).
pub const INITIAL_PACKET_TIMEOUT: Duration = Duration::from_millis(1);
/// Re-request interval while a gap stays unfilled (PACKET_TIMEOUT, 20000 µs
/// in Aravis).
pub const PACKET_RESEND_INTERVAL: Duration = Duration::from_millis(20);
/// How long an incomplete block is kept for resends before it is dropped
/// (FRAME_RETENTION, 100000 µs in Aravis). Overridable via
/// `GENTLITE_GEV_FRAME_RETENTION_MS`.
pub const FRAME_RETENTION: Duration = Duration::from_millis(100);

/// §16.7 resend configuration of a reassembler/receive thread.
#[derive(Clone, Copy, Debug)]
pub struct ResendConfig {
    /// R-197ca: only when the device declared PACKETRESEND support and the
    /// operator did not disable it (`GENTLITE_GEV_RESEND=0`).
    pub enabled: bool,
    /// §16.7.3 QoI: how long to keep an incomplete block.
    pub frame_retention: Duration,
}

impl ResendConfig {
    /// The historical M2 behavior: gaps drop the block at the trailer.
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            frame_retention: FRAME_RETENTION,
        }
    }
}

/// §16.7 resend machinery handed to a receive thread: the fire-and-forget
/// request path — the shared control channel plus this channel's index —
/// and the timing configuration. `None` in `receiver_loop` means resend is
/// disabled (device capability or operator env) and the historical
/// drop-on-gap behavior applies.
pub struct ResendHandle {
    pub client: Arc<Mutex<GvcpClient>>,
    pub channel: u16,
    pub config: ResendConfig,
}

/// Diagnostics counters (not the GenTL STREAM_INFO counters; those live in
/// `DsCore`). Published when the receive thread exits.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ReceiverStats {
    pub datagrams: u64,
    /// Undecodable headers, unknown packet formats, trailer/block
    /// mismatches.
    pub malformed: u64,
    /// Packets arriving with the PACKET_RESEND flag set.
    pub resend_packets: u64,
    /// PACKETRESEND_CMD requests sent (§16.7).
    pub resend_requests: u64,
    /// Blocks that arrived with gaps, were kept for resends, and completed.
    pub resend_recovered: u64,
    /// Header-only §16.7.2 error packets (CR-205st) answering a resend.
    pub resend_unavailable: u64,
    /// Retransmitted packets for data already received (deduplicated
    /// before accounting — S-6, compliance/preflight-gige-u3v.md).
    pub duplicate_packets: u64,
    /// Packets marking that the transmitter dropped the previous block.
    pub prev_block_dropped_marks: u64,
    /// Payload/trailer packets outside any known block.
    pub orphan_packets: u64,
    /// Trailers that completed a block (delivered or dropped).
    pub frames_completed: u64,
    /// Completed blocks handed to the acquisition engine.
    pub frames_delivered: u64,
    /// Blocks dropped: in-block packet gaps or trailer mismatch.
    pub frames_dropped_incomplete: u64,
    /// Blocks dropped: input pool empty.
    pub frames_dropped_no_buffer: u64,
    /// Blocks dropped: announced buffer smaller than the payload.
    pub frames_dropped_too_small: u64,
    /// Blocks dropped: engine not grabbing.
    pub frames_dropped_not_acquiring: u64,
    /// Leaders rejected: unparseable.
    pub frames_dropped_unsupported: u64,
    /// Payload bytes written into buffers.
    pub payload_bytes: u64,
}

impl ReceiverStats {
    /// Add another receiver's counters into `self` (M3-4a multi-channel
    /// fan-out reporting: one stats set per channel, one sum per stream).
    pub fn merge(&mut self, other: &ReceiverStats) {
        self.datagrams += other.datagrams;
        self.malformed += other.malformed;
        self.resend_packets += other.resend_packets;
        self.resend_requests += other.resend_requests;
        self.resend_recovered += other.resend_recovered;
        self.resend_unavailable += other.resend_unavailable;
        self.duplicate_packets += other.duplicate_packets;
        self.prev_block_dropped_marks += other.prev_block_dropped_marks;
        self.orphan_packets += other.orphan_packets;
        self.frames_completed += other.frames_completed;
        self.frames_delivered += other.frames_delivered;
        self.frames_dropped_incomplete += other.frames_dropped_incomplete;
        self.frames_dropped_no_buffer += other.frames_dropped_no_buffer;
        self.frames_dropped_too_small += other.frames_dropped_too_small;
        self.frames_dropped_not_acquiring += other.frames_dropped_not_acquiring;
        self.frames_dropped_unsupported += other.frames_dropped_unsupported;
        self.payload_bytes += other.payload_bytes;
    }
}

/// One missing packet-id interval (inclusive ends) of the open block.
struct Gap {
    end: u32,
    /// §16.7.3 QoI: when the next PACKETRESEND request for this gap fires.
    next_request: Instant,
}

/// One open block being reassembled.
struct Block {
    block_id: u64,
    leader: ImageLeader,
    /// Payload bytes per full packet, inferred from the block's first
    /// payload packet (packet_id == 1, R-264st: ids run from 1): R-263st
    /// guarantees all full packets of a block are the same size, so the
    /// first payload is always a full packet. None until it arrives.
    stride: Option<usize>,
    /// Highest payload packet id seen (0 = none yet).
    highest_packet: u32,
    /// Payload bytes written so far (detects truncated blocks: the
    /// trailer cannot be told apart from a short last packet by ids
    /// alone, so the byte count must match the leader's expectation).
    bytes_written: usize,
    /// Missing packet-id intervals (§16.7), keyed by start id. A packet
    /// arriving with an id above highest+1 registers the skipped range;
    /// resends (or §16.7.2 error packets) drain it.
    gaps: BTreeMap<u32, Gap>,
    /// The trailer arrived while gaps were still open: the block stays
    /// alive for resends (§16.7) until the gaps drain or the frame
    /// retention expires. `trailer_body` remembers the trailer for the
    /// final validation once the block completes.
    trailer: bool,
    trailer_body: Vec<u8>,
    /// Set when a trailer-wait (resend) path engaged: the block counts as
    /// resend-recovered if it completes.
    resend_touched: bool,
    /// Leader arrival time: the frame-retention deadline (§16.7.3 QoI).
    opened_at: Instant,
    /// Unrecoverable by resending (a payload would overflow the buffer):
    /// dropped at the next decision point.
    overflow: bool,
    /// Reserved write target; `None` when the block is being dropped.
    target: Option<FrameTarget>,
}

fn frame_meta(l: &ImageLeader) -> FrameMeta {
    FrameMeta {
        timestamp: l.timestamp,
        pixel_format: l.pixel_format,
        width: l.size_x,
        height: l.size_y,
        offset_x: l.offset_x,
        offset_y: l.offset_y,
        padding_x: l.padding_x,
        padding_y: l.padding_y,
    }
}

/// Pure reassembly state over a [`FrameSink`]; unit-testable without
/// sockets (feed it encoded datagrams).
///
/// Both GVSP header forms are accepted, discriminated per datagram by the
/// Extended ID bit (3.0 §24.2; see `gentl_gvcp::gvsp::GvspHeader::
/// decode_auto`): the 3.0 20-byte form and the 1.2 legacy 8-byte form
/// (1.2 §24 Fig 24-1, Q34/M3-9). The leader body layout follows the
/// header form (40-byte §25.2.1 vs 36-byte 1.2 Fig 24-3); the trailer body
/// is identical in both.
///
/// The payload stride is always inferred from each block's first payload
/// packet (R-263st: full packets of a block are equal; only the last may
/// be short). An SCPS0-derived stride is deliberately not used: the SCPSx
/// packet_size semantics differ between header forms (48-byte header
/// budget at §36.58 vs 36-byte at 1.2 §27.46), and the register value may
/// lag what the wire actually carries (R-562st rounding is the device's
/// own), while the first packet measures the wire directly.
///
/// Packet-loss recovery is §16.7 PACKETRESEND: gaps are tracked per open
/// block, requests go out on `on_tick` (initial window
/// [`INITIAL_PACKET_TIMEOUT`], re-request every [`PACKET_RESEND_INTERVAL`],
/// give up after [`ResendConfig::frame_retention`]), and a block whose
/// trailer arrives with open gaps stays alive until the gaps drain or the
/// retention expires. With resend disabled the historical behavior
/// remains: a gap drops the block at the trailer.
pub struct GvspReassembler {
    block: Option<Block>,
    stats: ReceiverStats,
    resend: ResendConfig,
}

impl GvspReassembler {
    pub fn new() -> Self {
        Self::with_resend(ResendConfig::disabled())
    }

    pub fn with_resend(resend: ResendConfig) -> Self {
        Self {
            block: None,
            stats: ReceiverStats::default(),
            resend,
        }
    }

    pub fn stats(&self) -> &ReceiverStats {
        &self.stats
    }

    /// A buffer is reserved for the current block.
    pub fn has_in_flight(&self) -> bool {
        self.block.as_ref().is_some_and(|b| b.target.is_some())
    }

    /// §16.7.3: the receive thread should poll fast (1 ms) while a resend
    /// window is open, so requests fire promptly and recovered blocks
    /// complete without waiting for the normal socket timeout.
    pub fn wants_fast_poll(&self) -> bool {
        self.resend.enabled
            && self
                .block
                .as_ref()
                .is_some_and(|b| !b.gaps.is_empty() && !b.overflow)
    }

    /// §16.7.3 resend scheduler, driven by the receive thread (and by the
    /// unit tests directly — no sockets involved). Fires one PACKETRESEND
    /// request per due gap via `emit(block_id, first, last)`; drops the
    /// block as incomplete once the frame retention expires.
    pub fn on_tick(&mut self, sink: &DsCore, now: Instant, emit: &mut dyn FnMut(u64, u32, u32)) {
        if !self.resend.enabled {
            return;
        }
        let mut expired = false;
        let mut due: Vec<(u64, u32, u32)> = Vec::new();
        {
            let Some(block) = &mut self.block else { return };
            if block.gaps.is_empty() || block.overflow {
                return;
            }
            if now.saturating_duration_since(block.opened_at) > self.resend.frame_retention {
                expired = true;
            } else {
                for (&start, gap) in block.gaps.iter() {
                    if now >= gap.next_request {
                        due.push((block.block_id, start, gap.end));
                    }
                }
                for (_, start, _) in &due {
                    if let Some(gap) = block.gaps.get_mut(start) {
                        gap.next_request = now + PACKET_RESEND_INTERVAL;
                    }
                }            }
        }
        if expired {
            // §16.7.3 QoI: kept the block long enough; give up.
            self.stats.frames_dropped_incomplete += 1;
            self.abandon_block(sink, false);
            return;
        }
        for (block_id, start, end) in due {
            emit(block_id, start, end);
            self.stats.resend_requests += 1;
        }
    }

    /// End the current block without a trailer.
    pub fn abandon_block(&mut self, sink: &DsCore, incomplete_to_output: bool) {
        if let Some(block) = self.block.take() {
            if block.target.is_some() {
                sink.abort_frame(incomplete_to_output);
            }
        }
    }

    /// Process one GVSP datagram, in either header form (§24.2 EI bit,
    /// 1.2 §24 Fig 24-1 — see [`GvspHeader::decode_auto`]).
    pub fn on_datagram(&mut self, sink: &DsCore, dgram: &[u8]) {
        self.stats.datagrams += 1;
        let Some((hdr, header_len)) = GvspHeader::decode_auto(dgram) else {
            self.stats.malformed += 1;
            return;
        };
        let body = &dgram[header_len..];
        if hdr.flag & GEV_FLAG_PACKET_RESEND != 0 {
            // §16.7.2 (CR-204st): a retransmitted packet is counted, then
            // processed like any other packet.
            self.stats.resend_packets += 1;
        }
        if hdr.flag & GEV_FLAG_PREVIOUS_BLOCK_DROPPED != 0 {
            self.stats.prev_block_dropped_marks += 1;
        }
        match hdr.packet_format {
            PACKET_FORMAT_DATA_LEADER => self.on_leader(sink, &hdr, body),
            PACKET_FORMAT_DATA_PAYLOAD => self.on_payload(sink, &hdr, body),
            PACKET_FORMAT_DATA_TRAILER => self.on_trailer(sink, &hdr, body),
            _ => self.stats.malformed += 1, // H.264/multi-part/GenDC: not in M2
        }
    }

    fn on_leader(&mut self, sink: &DsCore, hdr: &GvspHeader, body: &[u8]) {
        // The previous block ended without a trailer: incomplete.
        if self.block.is_some() {
            self.stats.frames_dropped_incomplete += 1;
            self.abandon_block(sink, false);
        }
        // The leader body layout follows the header form: 3.0 §25.2.1
        // (40 bytes, opens with payload_type_specific) vs 1.2 §24 Fig 24-3
        // (36 bytes, opens with a reserved u16 and u16 paddings).
        let leader = if hdr.extended_id {
            ImageLeader::decode(body)
        } else {
            ImageLeader::decode_legacy(body)
        };
        let Some(leader) = leader else {
            self.stats.frames_dropped_unsupported += 1;
            return;
        };
        let expected = frame_meta(&leader).payload_bytes();
        sink.set_expected_payload_size(expected.unwrap_or(0));
        // Pixel formats without a known bytes-per-pixel value are still
        // reassembled: payload placement is stride-based (format
        // independent) and completion is decided by the trailer. The
        // expected byte count is unknown, so the size check at the trailer
        // is skipped for such frames — a truncated tail is undetectable
        // (capability loss, notes/OPEN-QUESTIONS.md Q21). begin_frame(0)
        // disables the up-front size gate; capacity is still enforced per
        // packet below (a buffer that cannot hold the frame is dropped at
        // the trailer).
        let target = match sink.begin_frame(expected.unwrap_or(0)) {
            Ok(t) => Some(t),
            Err(FrameError::QueueEmpty) => {
                self.stats.frames_dropped_no_buffer += 1;
                None
            }
            Err(FrameError::TooSmall) => {
                self.stats.frames_dropped_too_small += 1;
                None
            }
            Err(FrameError::NotAcquiring) => {
                self.stats.frames_dropped_not_acquiring += 1;
                None
            }
            Err(FrameError::Busy) => {
                self.stats.malformed += 1; // leader while a block is open
                None
            }
        };
        self.block = Some(Block {
            block_id: hdr.block_id,
            leader,
            stride: None,
            highest_packet: 0,
            bytes_written: 0,
            gaps: BTreeMap::new(),
            trailer: false,
            trailer_body: Vec::new(),
            resend_touched: false,
            opened_at: Instant::now(),
            overflow: false,
            target,
        });
    }

    /// §16.7.2 (CR-205st): a header-only payload packet carrying a
    /// Table 16-2 error status marks the requested packet(s)
    /// unrecoverable; the gap it belongs to is closed and the block either
    /// completes or — with further gaps — waits for the rest.
    fn on_unrecoverable(&mut self, sink: &DsCore, hdr: &GvspHeader, and_previous: bool) {
        self.stats.resend_unavailable += 1;
        let completed = if and_previous {
            // AND_PREV_REMOVED_FROM_MEMORY: everything up to this id is gone.
            let ids: Vec<u32> = match &self.block {
                Some(b) => b
                    .gaps
                    .iter()
                    .flat_map(|(&s, g)| s..=g.end.min(hdr.packet_id))
                    .collect(),
                None => Vec::new(),
            };
            let mut done = false;
            if let Some(block) = &mut self.block {
                for id in ids {
                    done = fill_packet(block, id);
                }
            }
            done
        } else {
            match &mut self.block {
                Some(block) => fill_packet(block, hdr.packet_id),
                None => false,
            }
        };
        if completed {
            self.complete(sink);
        }
    }

    /// A resend-filled block became complete: run the trailer-time
    /// validation and deliver.
    fn complete(&mut self, sink: &DsCore) {
        let Some(block) = self.block.take() else { return };
        let trailer_body = block.trailer_body.clone();
        self.deliver(sink, &block, &trailer_body);
    }

    fn on_payload(&mut self, sink: &DsCore, hdr: &GvspHeader, body: &[u8]) {
        if hdr.packet_id == 0 {
            self.stats.malformed += 1; // packet_id 0 is the leader
            return;
        }
        if body.is_empty() {
            // §16.7.2 (CR-205st): a header-only error answer. SUCCESS or
            // GEV_STATUS_PACKET_RESEND would mean a dataless payload —
            // malformed; the Table 16-2 error statuses mark the packet
            // unrecoverable.
            match hdr.status {
                GEV_STATUS_PACKET_UNAVAILABLE
                | GEV_STATUS_TEMPORARILY_UNAVAILABLE
                | GEV_STATUS_NOT_YET_AVAILABLE
                | GEV_STATUS_PACKET_AND_PREV_REMOVED_FROM_MEMORY
                | GEV_STATUS_PACKET_REMOVED_FROM_MEMORY => {
                    let and_previous =
                        hdr.status == GEV_STATUS_PACKET_AND_PREV_REMOVED_FROM_MEMORY;
                    self.on_unrecoverable(sink, hdr, and_previous);
                }
                _ => self.stats.malformed += 1,
            }
            return;
        }
        // CO-206st / CR-207sr: RESEND_RANGE_ERROR — the whole requested
        // range is unrecoverable; the body carries last_packet_id32 and no
        // data.
        if hdr.flag & GEV_FLAG_RESEND_RANGE_ERROR != 0 {
            let last = u32::from_be_bytes(body[0..4].try_into().unwrap_or([0, 0, 0, 0]));
            self.stats.resend_unavailable += 1;
            let completed = match &mut self.block {
                Some(block) => {
                    let mut done = false;
                    for id in hdr.packet_id..=last.max(hdr.packet_id) {
                        done = fill_packet(block, id);
                    }
                    done
                }
                None => false,
            };
            if completed {
                self.complete(sink);
            }
            return;
        }
        let now = Instant::now();
        let completed = {
            let Some(block) = &mut self.block else {
                self.stats.orphan_packets += 1;
                return;
            };
            let in_gap = in_gap(block, hdr.packet_id);
            if !in_gap && hdr.packet_id <= block.highest_packet {
                // S-6 (compliance/preflight-gige-u3v.md): a retransmitted
                // copy of a packet already accounted for is discarded
                // BEFORE any byte counting — duplicates must not inflate
                // bytes_written.
                self.stats.duplicate_packets += 1;
                return;
            }
            let stride = match block.stride {
                Some(s) => s,
                None => {
                    if hdr.packet_id == 1 {
                        block.stride = Some(body.len());
                        body.len()
                    } else {
                        // Cannot locate the data without a stride.
                        self.stats.malformed += 1;
                        return;
                    }
                }
            };
            // In-block gap registration by packet id sequence (§24: ids
            // run from 1 without holes).
            let prev_highest = block.highest_packet;
            if !in_gap {
                if prev_highest == 0 {
                    register_gap(block, 1, hdr.packet_id - 1, now);
                } else if hdr.packet_id > prev_highest + 1 {
                    register_gap(block, prev_highest + 1, hdr.packet_id - 1, now);
                }
            }
            block.highest_packet = block.highest_packet.max(hdr.packet_id);
            let Some((base, capacity)) = block.target.as_ref().map(|t| (t.base, t.capacity))
            else {
                // Dropping block: payloads counted, not written — but a
                // resend still closes its gap so completion can be told
                // apart from a vanished block.
                if in_gap {
                    fill_packet(block, hdr.packet_id);
                }
                return;
            };
            let Some(offset) = (hdr.packet_id as usize)
                .checked_sub(1)
                .and_then(|k| k.checked_mul(stride))
            else {
                block.overflow = true;
                return;
            };
            if offset >= capacity {
                block.overflow = true;
                return;
            }
            let n = body.len().min(capacity - offset);
            if n < body.len() {
                block.overflow = true; // would overflow the buffer
            }
            // SAFETY: `base` is exclusively reserved for this block until
            // the paired on_frame/abort_frame; offset+n <= capacity.
            unsafe {
                std::ptr::copy_nonoverlapping(body.as_ptr(), base.add(offset), n);
            }
            block.bytes_written += n;
            self.stats.payload_bytes += n as u64;
            if in_gap {
                fill_packet(block, hdr.packet_id)
            } else {
                false
            }
        };
        if completed {
            self.complete(sink);
        }
    }

    fn on_trailer(&mut self, sink: &DsCore, hdr: &GvspHeader, body: &[u8]) {
        let Some(block) = self.block.take() else {
            self.stats.orphan_packets += 1;
            return;
        };
        if block.block_id != hdr.block_id {
            // S-7 (compliance/preflight-gige-u3v.md): a trailer carrying
            // another block's id is a desync — the block must not be
            // delivered against the wrong id.
            self.stats.malformed += 1;
            self.stats.frames_dropped_incomplete += 1;
            if block.target.is_some() {
                sink.abort_frame(false);
            }
            return;
        }
        if block.target.is_none() {
            return; // dropping block: already counted at the leader
        }
        let mut block = block;
        // The trailer's packet_id is the next id after the last payload
        // (R-264st sequence): any id between the highest payload seen and
        // it never arrived — register the tail gap. Without this, a lost
        // tail would look "complete but short" instead of recoverable.
        let tail_from = block.highest_packet + 1;
        if hdr.packet_id > tail_from {
            register_gap(&mut block, tail_from, hdr.packet_id - 1, Instant::now());
        }
        if block.overflow {
            self.stats.frames_dropped_incomplete += 1;
            sink.abort_frame(false);
            return;
        }
        if !block.gaps.is_empty() && self.resend.enabled {
            // §16.7: the trailer only ends the block's *new* packets —
            // keep it open for resends until the gaps drain or the frame
            // retention expires (on_tick).
            block.trailer = true;
            block.trailer_body = body.to_vec();
            block.resend_touched = true;
            self.block = Some(block);
            return;
        }
        self.deliver(sink, &block, body);
    }

    /// Trailer-time delivery of a taken block: validate and hand the
    /// completed block to the engine.
    fn deliver(&mut self, sink: &DsCore, block: &Block, trailer_body: &[u8]) {
        // Trailer sanity: the delivered row count must match the leader,
        // and — when the pixel format has a known bytes-per-pixel value —
        // the accumulated payload must reach the expected size (catches
        // blocks whose tail packets never arrived). Unknown formats cannot
        // be checked this way (Q21).
        let rows_ok = decode_image_trailer_body(trailer_body)
            .map(|size_y| size_y == block.leader.size_y)
            .unwrap_or(false);
        if !rows_ok {
            self.stats.malformed += 1;
        }
        let size_ok = frame_meta(&block.leader)
            .payload_bytes()
            .map(|expected| expected == block.bytes_written)
            .unwrap_or(true);
        if !rows_ok || !size_ok {
            // Gapped/broken blocks are dropped instead of delivering
            // incomplete buffers; the reserved buffer returns to the
            // input pool.
            self.stats.frames_dropped_incomplete += 1;
            sink.abort_frame(false);
            return;
        }
        if sink.on_frame(block.block_id, frame_meta(&block.leader), &[]).is_some() {
            self.stats.frames_delivered += 1;
            if block.resend_touched {
                self.stats.resend_recovered += 1;
            }
        }
        self.stats.frames_completed += 1;
    }
}

/// Is `id` inside one of the block's missing intervals?
fn in_gap(block: &Block, id: u32) -> bool {
    block
        .gaps
        .range(..=id)
        .next_back()
        .is_some_and(|(_, g)| id <= g.end)
}

/// Register a missing interval [start, end] (inclusive). The first
/// §16.7.3 resend request fires after the initial window.
fn register_gap(block: &mut Block, start: u32, end: u32, now: Instant) {
    if start == 0 || start > end {
        return;
    }
    block.gaps.insert(
        start,
        Gap {
            end,
            next_request: now + INITIAL_PACKET_TIMEOUT,
        },
    );
}

/// Remove one packet id from the missing set (it arrived, or the device
/// declared it unrecoverable, §16.7.2). Returns true when the block just
/// became complete (trailer seen, nothing missing, deliverable).
fn fill_packet(block: &mut Block, id: u32) -> bool {
    let containing = block
        .gaps
        .range(..=id)
        .next_back()
        .filter(|(_, g)| id <= g.end)
        .map(|(&s, _)| s);
    if let Some(start) = containing {
        let (end, next_request) = {
            let g = &block.gaps[&start];
            (g.end, g.next_request)
        };
        if start == id && end == id {
            block.gaps.remove(&start);
        } else if start == id {
            block.gaps.remove(&start);
            block.gaps.insert(id + 1, Gap { end, next_request });
        } else if end == id {
            block.gaps.get_mut(&start).expect("gap start").end = id - 1;
        } else {
            block.gaps.get_mut(&start).expect("gap start").end = id - 1;
            block.gaps.insert(id + 1, Gap { end, next_request });
        }
    }
    block.trailer && block.gaps.is_empty() && block.target.is_some() && !block.overflow
}

/// Receive thread: `recv_from` loop into the reassembler until stopped.
/// A graceful stop lets the in-flight block finish (its trailer still
/// arrives); a kill aborts it incomplete; both fall back to abandoning the
/// block after [`STOP_GRACE`]. Header form and payload stride are handled
/// by the reassembler (per-datagram EI discrimination and first-packet
/// stride inference — see [`GvspReassembler`]).
///
/// `resend` carries the §16.7 machinery when enabled: the reassembler
/// asks for missing ranges via `emit`, and the request goes out on the
/// shared control channel. §16.7 allows the request "asynchronously, even
/// while the acknowledgment of a previous command is awaited" — here the
/// control channel is mutex-serialized, so the receiver can briefly
/// interleave with control operations (QoI).
pub fn receiver_loop(
    socket: UdpSocket,
    sink: Arc<DsCore>,
    stop: Arc<AtomicBool>,
    kill: Arc<AtomicBool>,
    stats_out: Arc<Mutex<ReceiverStats>>,
    stream: Arc<crate::stream::GevDataStream>,
    resend: Option<ResendHandle>,
) {
    let _ = socket.set_read_timeout(Some(SOCKET_TIMEOUT));
    let mut reasm = match &resend {
        Some(h) => GvspReassembler::with_resend(h.config),
        None => GvspReassembler::new(),
    };
    let mut buf = vec![0u8; RECV_BUF_LEN];
    let mut stop_since: Option<Instant> = None;
    loop {
        if stop.load(Ordering::Relaxed) {
            let since = *stop_since.get_or_insert_with(Instant::now);
            let finishing = reasm.has_in_flight();
            if !finishing || kill.load(Ordering::Relaxed) || since.elapsed() > STOP_GRACE {
                reasm.abandon_block(&sink, true);
                break;
            }
        }
        // §16.7.3: poll at 1 ms while a resend window is open so requests
        // and completions are not delayed by the normal socket timeout.
        let _ = socket.set_read_timeout(Some(if reasm.wants_fast_poll() {
            RESEND_POLL
        } else {
            SOCKET_TIMEOUT
        }));
        match socket.recv_from(&mut buf) {
            Ok((n, _src)) => reasm.on_datagram(&sink, &buf[..n]),
            Err(e)
                if e.kind() == io::ErrorKind::TimedOut
                    || e.kind() == io::ErrorKind::WouldBlock => {}
            Err(_) => {
                // Transport-level failure: the socket is unusable. Abandon
                // the in-flight frame and make the engine state reflect the
                // death so consumers do not block forever.
                reasm.abandon_block(&sink, true);
                stream.on_receiver_fatal();
                break;
            }
        }
        if let Some(h) = &resend {
            let client = h.client.clone();
            let channel = h.channel;
            reasm.on_tick(&sink, Instant::now(), &mut |block_id, first, last| {
                if let Ok(c) = client.lock() {
                    c.packet_resend(channel, block_id, first, last);
                }
            });
        }
    }
    *stats_out.lock().unwrap() = reasm.stats().clone();
}

#[cfg(test)]
mod tests {
    use super::*;
    use gentl_core::ds::{AcqQueueOp, BufferState, GENTL_INFINITE};
    use gentl_core::pixfmt;

    const W: u32 = 6;
    const H: u32 = 4;
    const PF: u32 = pixfmt::PFNC_MONO8;
    const PAYLOAD: usize = (W * H) as usize; // 24 bytes
    const CHUNK: usize = 10; // 3 packets: 10 + 10 + 4

    fn leader_dgram(block_id: u64, ts: u64) -> Vec<u8> {
        let hdr = GvspHeader {
            status: 0,
            flag: 0,
            extended_id: true,
            packet_format: PACKET_FORMAT_DATA_LEADER,
            block_id,
            packet_id: 0,
        };
        let mut h = [0u8; GVSP_HEADER_LEN];
        hdr.encode(&mut h);
        let leader = ImageLeader {
            field_id: 0,
            field_count: 0,
            timestamp: ts,
            pixel_format: PF,
            size_x: W,
            size_y: H,
            offset_x: 0,
            offset_y: 0,
            padding_x: 0,
            padding_y: 0,
        };
        let mut v = h.to_vec();
        v.extend_from_slice(&encode_image_leader_body(&leader));
        v
    }

    fn payload_dgram(block_id: u64, packet_id: u32, data: &[u8]) -> Vec<u8> {
        let hdr = GvspHeader {
            status: 0,
            flag: 0,
            extended_id: true,
            packet_format: PACKET_FORMAT_DATA_PAYLOAD,
            block_id,
            packet_id,
        };
        let mut h = [0u8; GVSP_HEADER_LEN];
        hdr.encode(&mut h);
        let mut v = h.to_vec();
        v.extend_from_slice(data);
        v
    }

    fn trailer_dgram(block_id: u64) -> Vec<u8> {
        // packet_id = last payload id + 1, per the R-264st sequence — the
        // receiver derives the expected packet count from it.
        let hdr = GvspHeader {
            status: 0,
            flag: 0,
            extended_id: true,
            packet_format: PACKET_FORMAT_DATA_TRAILER,
            block_id,
            packet_id: 4, // the test frame has 3 payload packets
        };
        let mut h = [0u8; GVSP_HEADER_LEN];
        hdr.encode(&mut h);
        let mut v = h.to_vec();
        v.extend_from_slice(&encode_image_trailer_body(H));
        v
    }

    /// A full frame as the wire carries it.
    fn frame(block_id: u64, pattern_seed: u8) -> Vec<Vec<u8>> {
        let payload: Vec<u8> = (0..PAYLOAD).map(|i| i as u8 ^ pattern_seed).collect();
        let mut d = vec![leader_dgram(block_id, u64::from(pattern_seed))];
        for (i, chunk) in payload.chunks(CHUNK).enumerate() {
            d.push(payload_dgram(block_id, (i + 1) as u32, chunk));
        }
        d.push(trailer_dgram(block_id));
        d
    }

    fn started_sink() -> (Arc<DsCore>, u64) {
        let core = Arc::new(DsCore::new());
        let backing: Vec<u8> = vec![0; 4096];
        let id = core
            .announce_external(
                backing.leak() as *mut [u8] as *mut core::ffi::c_void,
                4096,
                std::ptr::null_mut(),
            )
            .unwrap();
        core.queue_buffer(id).unwrap();
        core.start_acquisition(GENTL_INFINITE).unwrap();
        (core, id)
    }

    #[test]
    fn reassembles_frame_zero_copy() {
        let (core, buf_id) = started_sink();
        let mut r = GvspReassembler::new();
        for d in frame(1, 0xF0) {
            r.on_datagram(&core, &d);
        }
        let s = r.stats();
        assert_eq!(s.frames_delivered, 1);
        assert_eq!(s.frames_completed, 1);
        assert_eq!(s.malformed, 0);
        assert_eq!(s.payload_bytes, PAYLOAD as u64);
        assert_eq!(core.await_delivery_count(), 1);
        core.with_buffer(buf_id, |b| {
            assert_eq!(b.state, BufferState::Filled);
            let mem =
                unsafe { std::slice::from_raw_parts(b.base_ptr() as *const u8, PAYLOAD) };
            let expect: Vec<u8> = (0..PAYLOAD).map(|i| i as u8 ^ 0xF0).collect();
            assert_eq!(mem, &expect);
            assert_eq!(b.meta.unwrap().timestamp, 0xF0);
            assert_eq!(b.filled_size, PAYLOAD);
        })
        .unwrap();
    }

    /// A full frame as a 1.2 legacy (EI=0, 8-byte header) wire carries it:
    /// 8-byte headers, 36-byte leader body, u24 packet ids. Same payload
    /// layout as [`frame`] otherwise.
    fn legacy_frame(block_id: u64, pattern_seed: u8) -> Vec<Vec<u8>> {
        let hdr = |packet_format: u8, packet_id: u32| -> [u8; GVSP_LEGACY_HEADER_LEN] {
            let mut buf = [0u8; GVSP_LEGACY_HEADER_LEN];
            GvspHeader {
                status: 0,
                flag: 0,
                extended_id: false,
                packet_format,
                block_id,
                packet_id,
            }
            .encode_legacy(&mut buf);
            buf
        };
        let leader_body = encode_image_leader_body_legacy(&ImageLeader {
            field_id: 0,
            field_count: 0,
            timestamp: u64::from(pattern_seed),
            pixel_format: PF,
            size_x: W,
            size_y: H,
            offset_x: 0,
            offset_y: 0,
            padding_x: 0,
            padding_y: 0,
        });
        let mut v = vec![[hdr(PACKET_FORMAT_DATA_LEADER, 0).as_slice(), &leader_body].concat()];
        let payload: Vec<u8> = (0..PAYLOAD).map(|i| i as u8 ^ pattern_seed).collect();
        for (i, chunk) in payload.chunks(CHUNK).enumerate() {
            v.push([hdr(PACKET_FORMAT_DATA_PAYLOAD, (i + 1) as u32).as_slice(), chunk].concat());
        }
        v.push([hdr(PACKET_FORMAT_DATA_TRAILER, 0).as_slice(), &encode_image_trailer_body(H)].concat());
        v
    }

    /// M3-9: the 1.2 legacy form (8-byte header, EI=0, 1.2 §24 Fig 24-1)
    /// reassembles end to end: header form discrimination, 36-byte leader
    /// body, u16 paddings, u24 packet ids, trailer identical to 3.0.
    #[test]
    fn reassembles_legacy_12_frames() {
        let (core, buf_id) = started_sink();
        let mut r = GvspReassembler::new();
        for d in legacy_frame(1, 0xF0) {
            r.on_datagram(&core, &d);
        }
        let s = r.stats();
        assert_eq!(s.frames_delivered, 1);
        assert_eq!(s.malformed, 0);
        assert_eq!(s.payload_bytes, PAYLOAD as u64);
        core.with_buffer(buf_id, |b| {
            assert_eq!(b.state, BufferState::Filled);
            let mem = unsafe { std::slice::from_raw_parts(b.base_ptr() as *const u8, PAYLOAD) };
            let expect: Vec<u8> = (0..PAYLOAD).map(|i| i as u8 ^ 0xF0).collect();
            assert_eq!(mem, &expect);
            // Metadata comes from the 36-byte 1.2 leader body.
            assert_eq!(b.meta.unwrap().timestamp, 0xF0);
            assert_eq!(b.meta.unwrap().width, W);
            assert_eq!(b.meta.unwrap().height, H);
        })
        .unwrap();
        // A 3.0 frame right after a legacy one: per-datagram EI
        // discrimination keeps both forms working on one reassembler.
        // Return the delivered buffer to the input pool first (the engine
        // admits one in-flight frame).
        core.flush(AcqQueueOp::AllDiscard).unwrap();
        core.queue_buffer(buf_id).unwrap();
        for d in frame(2, 0x0F) {
            r.on_datagram(&core, &d);
        }
        assert_eq!(r.stats().frames_delivered, 2);
    }

    #[test]
    fn packet_gap_drops_block_and_counts() {
        let (core, buf_id) = started_sink();
        let mut r = GvspReassembler::new();
        r.on_datagram(&core, &leader_dgram(1, 1));
        let payload: Vec<u8> = (0..PAYLOAD).map(|i| i as u8).collect();
        // Packets 1 and 3 only: packet 2 is missing.
        r.on_datagram(&core, &payload_dgram(1, 1, &payload[..CHUNK]));
        r.on_datagram(&core, &payload_dgram(1, 3, &payload[2 * CHUNK..]));
        r.on_datagram(&core, &trailer_dgram(1));
        let s = r.stats();
        assert_eq!(s.frames_dropped_incomplete, 1);
        assert_eq!(s.frames_delivered, 0);
        // Buffer went back to the input pool, frame data not delivered.
        assert_eq!(core.queued_count(), 1);
        assert_eq!(core.await_delivery_count(), 0);
        assert!(core.with_buffer(buf_id, |b| !b.new_data).unwrap());
    }

    #[test]
    fn missing_trailer_drops_block_on_next_leader() {
        let (core, _) = started_sink();
        let mut r = GvspReassembler::new();
        r.on_datagram(&core, &leader_dgram(1, 1));
        r.on_datagram(&core, &leader_dgram(2, 2));
        assert_eq!(r.stats().frames_dropped_incomplete, 1);
        assert_eq!(r.stats().frames_delivered, 0);
        // Second block completes normally.
        let payload: Vec<u8> = (0..PAYLOAD).map(|i| i as u8).collect();
        for (i, chunk) in payload.chunks(CHUNK).enumerate() {
            r.on_datagram(&core, &payload_dgram(2, (i + 1) as u32, chunk));
        }
        r.on_datagram(&core, &trailer_dgram(2));
        assert_eq!(r.stats().frames_delivered, 1);
        assert_eq!(r.stats().frames_completed, 1);
    }

    #[test]
    fn no_buffer_counts_underrun_and_keeps_streaming() {
        let core = Arc::new(DsCore::new());
        core.start_acquisition(GENTL_INFINITE).unwrap();
        let mut r = GvspReassembler::new();
        for d in frame(1, 0) {
            r.on_datagram(&core, &d);
        }
        assert_eq!(r.stats().frames_dropped_no_buffer, 1);
        assert_eq!(r.stats().frames_delivered, 0);
        assert_eq!(core.num_underrun(), 1);
        assert_eq!(core.expected_payload_size(), PAYLOAD);
    }

    #[test]
    fn unknown_pixel_format_reassembles_and_completes_via_trailer() {
        let (core, buf_id) = started_sink();
        let mut r = GvspReassembler::new();
        // A leader whose pixel format the producer cannot size (a value
        // outside the PFNC table — here a never-assigned range) is not
        // rejected: placement is stride-based and the trailer alone
        // decides completion.
        let mut lead = leader_dgram(1, 1);
        lead[GVSP_HEADER_LEN + 12..GVSP_HEADER_LEN + 16]
            .copy_from_slice(&0xDEAD_BEEFu32.to_be_bytes()); // not in pixfmt_table
        r.on_datagram(&core, &lead);
        assert_eq!(r.stats().frames_dropped_unsupported, 0);
        assert_eq!(core.expected_payload_size(), 0, "unknown expected size");
        let payload: Vec<u8> = (0..PAYLOAD).map(|i| i as u8).collect();
        for (i, chunk) in payload.chunks(CHUNK).enumerate() {
            r.on_datagram(&core, &payload_dgram(1, (i + 1) as u32, chunk));
        }
        r.on_datagram(&core, &trailer_dgram(1));
        let s = r.stats();
        assert_eq!(s.frames_delivered, 1);
        assert_eq!(s.frames_completed, 1);
        // The payload bytes land in the buffer, but zero-copy delivery
        // cannot size the frame (payload_bytes() == None), so
        // BUFFER_INFO_SIZE_FILLED reports 0 (notes/OPEN-QUESTIONS.md Q21).
        core.with_buffer(buf_id, |b| {
            assert_eq!(b.state, BufferState::Filled);
            assert_eq!(b.filled_size, 0);
            let mem =
                unsafe { std::slice::from_raw_parts(b.base_ptr() as *const u8, PAYLOAD) };
            assert_eq!(mem, &payload);
        })
        .unwrap();
    }

    #[test]
    fn unparseable_leader_still_counts_unsupported() {
        let (core, _) = started_sink();
        let mut r = GvspReassembler::new();
        let mut d = leader_dgram(1, 1);
        d.truncate(GVSP_HEADER_LEN + 20); // image leader body needs 40 bytes
        r.on_datagram(&core, &d);
        r.on_datagram(&core, &trailer_dgram(1));
        assert_eq!(r.stats().frames_dropped_unsupported, 1);
        assert_eq!(r.stats().frames_delivered, 0);
        assert_eq!(core.num_underrun(), 0);
    }

    #[test]
    fn orphan_and_malformed_datagrams_counted() {
        let (core, _) = started_sink();
        let mut r = GvspReassembler::new();
        r.on_datagram(&core, &[0u8; 8]); // too short
        r.on_datagram(&core, &payload_dgram(9, 1, &[1, 2, 3])); // no block
        r.on_datagram(&core, &trailer_dgram(9)); // no block
        let d = frame(1, 1);
        for x in d {
            r.on_datagram(&core, &x);
        }
        assert_eq!(r.stats().malformed, 1);
        assert_eq!(r.stats().orphan_packets, 2);
        assert_eq!(r.stats().frames_delivered, 1);
    }

    #[test]
    fn resend_and_prev_dropped_flags_counted() {
        let (core, _) = started_sink();
        let mut r = GvspReassembler::new();
        let mut lead = leader_dgram(1, 1);
        lead[2] = 0x40; // PREV_BLOCK_DROPPED
        r.on_datagram(&core, &lead);
        let mut resend = payload_dgram(1, 1, &[0u8; CHUNK]);
        resend[2] = 0x80; // PACKET_RESEND
        r.on_datagram(&core, &resend);
        assert_eq!(r.stats().prev_block_dropped_marks, 1);
        assert_eq!(r.stats().resend_packets, 1);
        assert_eq!(r.stats().frames_delivered, 0, "block still open");
        // Block is gapped (only packet 1 of 3 seen); trailer drops it.
        r.on_datagram(&core, &trailer_dgram(1));
        assert_eq!(r.stats().frames_dropped_incomplete, 1);
    }

    #[test]
    fn stop_mid_block_abandons_incomplete() {
        let (core, _) = started_sink();
        let mut r = GvspReassembler::new();
        r.on_datagram(&core, &leader_dgram(1, 1));
        assert!(r.has_in_flight());
        r.abandon_block(&core, true);
        assert!(!r.has_in_flight());
        assert_eq!(core.await_delivery_count(), 1, "kill: incomplete to output");
        core.with_buffer(core.buffer_id_at(0).unwrap(), |b| {
            assert!(b.incomplete);
            assert_eq!(b.state, BufferState::Filled);
        })
        .unwrap();
        // Return the buffer to the input pool, then a fresh frame works.
        core.flush(AcqQueueOp::AllDiscard).unwrap();
        core.queue_buffer(core.buffer_id_at(0).unwrap()).unwrap();
        for d in frame(2, 7) {
            r.on_datagram(&core, &d);
        }
        assert_eq!(r.stats().frames_delivered, 1);
    }

    #[test]
    fn flush_back_to_input_via_sink() {
        let (core, _) = started_sink();
        let mut r = GvspReassembler::new();
        r.on_datagram(&core, &leader_dgram(1, 1));
        r.abandon_block(&core, false);
        assert_eq!(core.queued_count(), 1, "graceful abort returns to input");
        core.flush(AcqQueueOp::AllDiscard).unwrap();
    }

    /// A §16.7.2 retransmitted payload datagram: data plus the resend
    /// markers (GEV_FLAG_PACKET_RESEND, status GEV_STATUS_PACKET_RESEND).
    fn resend_payload_dgram(block_id: u64, packet_id: u32, data: &[u8]) -> Vec<u8> {
        let mut d = payload_dgram(block_id, packet_id, data);
        d[0..2].copy_from_slice(&gentl_gvcp::wire::GEV_STATUS_PACKET_RESEND.to_be_bytes());
        let flag = u16::from_be_bytes([d[2], d[3]]) | GEV_FLAG_PACKET_RESEND;
        d[2..4].copy_from_slice(&flag.to_be_bytes());
        d
    }

    /// §16.7: a block with a lost packet is kept past its trailer, a
    /// PACKETRESEND request fires after the initial window (on_tick is
    /// deterministic — no sockets), and the marked retransmission
    /// completes the block with the payload intact.
    #[test]
    fn resend_recovers_gapped_block() {
        let (core, buf_id) = started_sink();
        let mut r = GvspReassembler::with_resend(ResendConfig {
            enabled: true,
            frame_retention: FRAME_RETENTION,
        });
        let t0 = Instant::now();
        let payload: Vec<u8> = (0..PAYLOAD).map(|i| i as u8).collect();
        r.on_datagram(&core, &leader_dgram(1, 1));
        r.on_datagram(&core, &payload_dgram(1, 1, &payload[..CHUNK]));
        // Packet 2 is lost; packet 3 registers the gap (2..=2).
        r.on_datagram(&core, &payload_dgram(1, 3, &payload[2 * CHUNK..]));
        r.on_datagram(&core, &trailer_dgram(1));
        // Trailer with an open gap: kept for resends, not dropped.
        assert_eq!(r.stats().frames_dropped_incomplete, 0);
        assert!(r.wants_fast_poll());
        // Initial window (§16.7.3 QoI, 1 ms) passes: one request [2, 2].
        let mut emitted = Vec::new();
        r.on_tick(&core, t0 + Duration::from_millis(5), &mut |b, f, l| {
            emitted.push((b, f, l))
        });
        assert_eq!(emitted, vec![(1, 2, 2)]);
        assert_eq!(r.stats().resend_requests, 1);
        // Not due again immediately (20 ms re-request interval).
        let mut again = Vec::new();
        r.on_tick(&core, t0 + Duration::from_millis(6), &mut |b, f, l| {
            again.push((b, f, l))
        });
        assert!(again.is_empty());
        // The device answers with the marked retransmission.
        r.on_datagram(&core, &resend_payload_dgram(1, 2, &payload[CHUNK..2 * CHUNK]));
        let s = r.stats();
        assert_eq!(s.resend_packets, 1);
        assert_eq!(s.resend_recovered, 1);
        assert_eq!(s.frames_delivered, 1);
        assert_eq!(s.frames_completed, 1);
        assert!(!r.wants_fast_poll());
        core.with_buffer(buf_id, |b| {
            let mem = unsafe { std::slice::from_raw_parts(b.base_ptr() as *const u8, PAYLOAD) };
            assert_eq!(mem, &payload, "gap-filled payload intact");
        })
        .unwrap();
    }

    /// §16.7.2 (CR-205st): a header-only error packet (here
    /// GEV_STATUS_PACKET_UNAVAILABLE) marks the packet unrecoverable; the
    /// block then completes but fails the byte-count check and drops.
    #[test]
    fn resend_error_packet_marks_packet_unrecoverable() {
        let (core, _) = started_sink();
        let mut r = GvspReassembler::with_resend(ResendConfig {
            enabled: true,
            frame_retention: FRAME_RETENTION,
        });
        let t0 = Instant::now();
        let payload: Vec<u8> = (0..PAYLOAD).map(|i| i as u8).collect();
        r.on_datagram(&core, &leader_dgram(1, 1));
        r.on_datagram(&core, &payload_dgram(1, 1, &payload[..CHUNK]));
        r.on_datagram(&core, &payload_dgram(1, 3, &payload[2 * CHUNK..]));
        r.on_datagram(&core, &trailer_dgram(1));
        let mut emitted = Vec::new();
        r.on_tick(&core, t0 + Duration::from_millis(5), &mut |b, f, l| {
            emitted.push((b, f, l))
        });
        assert_eq!(emitted, vec![(1, 2, 2)]);
        // Header-only error answer for packet 2.
        let mut err = payload_dgram(1, 2, &[]);
        err[0..2].copy_from_slice(&gentl_gvcp::wire::GEV_STATUS_PACKET_UNAVAILABLE.to_be_bytes());
        r.on_datagram(&core, &err);
        let s = r.stats();
        assert_eq!(s.resend_unavailable, 1);
        assert_eq!(s.frames_dropped_incomplete, 1, "24 bytes expected, 20 written");
        assert_eq!(s.frames_delivered, 0);
    }

    /// §16.7.3 QoI: past the frame retention an unanswered gap gives up —
    /// the block drops as incomplete without further requests.
    #[test]
    fn resend_retention_expiry_drops_block() {
        let (core, _) = started_sink();
        let mut r = GvspReassembler::with_resend(ResendConfig {
            enabled: true,
            frame_retention: FRAME_RETENTION,
        });
        let t0 = Instant::now();
        let payload: Vec<u8> = (0..PAYLOAD).map(|i| i as u8).collect();
        r.on_datagram(&core, &leader_dgram(1, 1));
        r.on_datagram(&core, &payload_dgram(1, 1, &payload[..CHUNK]));
        r.on_datagram(&core, &payload_dgram(1, 3, &payload[2 * CHUNK..]));
        r.on_datagram(&core, &trailer_dgram(1));
        let mut emitted = Vec::new();
        r.on_tick(
            &core,
            t0 + FRAME_RETENTION + Duration::from_millis(50),
            &mut |b, f, l| emitted.push((b, f, l)),
        );
        assert!(emitted.is_empty(), "expired before any request fired");
        let s = r.stats();
        assert_eq!(s.frames_dropped_incomplete, 1);
        assert_eq!(s.resend_requests, 0);
        assert!(!r.wants_fast_poll());
    }

    /// S-6 (compliance/preflight-gige-u3v.md): a duplicate packet — a
    /// marked retransmission of data already received — is discarded
    /// before byte accounting; bytes_written and the payload stay exact.
    #[test]
    fn resend_duplicate_does_not_double_count() {
        let (core, buf_id) = started_sink();
        let mut r = GvspReassembler::with_resend(ResendConfig {
            enabled: true,
            frame_retention: FRAME_RETENTION,
        });
        let t0 = Instant::now();
        let payload: Vec<u8> = (0..PAYLOAD).map(|i| i as u8).collect();
        r.on_datagram(&core, &leader_dgram(1, 1));
        r.on_datagram(&core, &payload_dgram(1, 1, &payload[..CHUNK]));
        // Packets 2 and 3 are lost (one gap 2..=3).
        r.on_datagram(&core, &trailer_dgram(1));
        let mut emitted = Vec::new();
        r.on_tick(&core, t0 + Duration::from_millis(5), &mut |b, f, l| {
            emitted.push((b, f, l))
        });
        assert_eq!(emitted, vec![(1, 2, 3)]);
        // The resend of packet 2 arrives twice (device memory kept the
        // whole requested range; re-requesting also re-sends). The second
        // copy is an in-block duplicate and must not double-count.
        r.on_datagram(&core, &resend_payload_dgram(1, 2, &payload[CHUNK..2 * CHUNK]));
        r.on_datagram(&core, &resend_payload_dgram(1, 2, &payload[CHUNK..2 * CHUNK]));
        let s = r.stats();
        assert_eq!(s.duplicate_packets, 1);
        assert_eq!(s.frames_delivered, 0, "packet 3 still missing");
        assert_eq!(s.payload_bytes, 2 * CHUNK as u64, "S-6: no double counting");
        // Packet 3 completes the block.
        r.on_datagram(&core, &resend_payload_dgram(1, 3, &payload[2 * CHUNK..]));
        let s = r.stats();
        assert_eq!(s.resend_recovered, 1);
        assert_eq!(s.frames_delivered, 1);
        assert_eq!(s.payload_bytes, PAYLOAD as u64);
        core.with_buffer(buf_id, |b| {
            let mem = unsafe { std::slice::from_raw_parts(b.base_ptr() as *const u8, PAYLOAD) };
            assert_eq!(mem, &payload);
        })
        .unwrap();
    }

    /// S-7 (compliance/preflight-gige-u3v.md): a trailer carrying another
    /// block's id must drop the block, not deliver it against the wrong
    /// id.
    #[test]
    fn trailer_block_id_mismatch_drops_block() {
        let (core, _) = started_sink();
        let mut r = GvspReassembler::new();
        for d in frame(1, 0) {
            r.on_datagram(&core, &d);
        }
        // A complete block was delivered; re-run a block and mismatch its
        // trailer.
        core.flush(AcqQueueOp::AllDiscard).unwrap();
        core.queue_buffer(core.buffer_id_at(0).unwrap()).unwrap();
        let mut r = GvspReassembler::new();
        let payload: Vec<u8> = (0..PAYLOAD).map(|i| i as u8).collect();
        r.on_datagram(&core, &leader_dgram(7, 1));
        r.on_datagram(&core, &payload_dgram(7, 1, &payload[..CHUNK]));
        r.on_datagram(&core, &payload_dgram(7, 2, &payload[CHUNK..2 * CHUNK]));
        r.on_datagram(&core, &payload_dgram(7, 3, &payload[2 * CHUNK..]));
        r.on_datagram(&core, &trailer_dgram(99));
        let s = r.stats();
        assert_eq!(s.frames_delivered, 0);
        assert_eq!(s.malformed, 1);
        assert_eq!(s.frames_dropped_incomplete, 1);
        assert_eq!(core.queued_count(), 1, "buffer back in the input pool");
    }
}
