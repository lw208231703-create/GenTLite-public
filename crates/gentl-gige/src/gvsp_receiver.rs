//! GVSP receiver: per-datagram reassembly of image blocks (leader +
//! payload packets + trailer) into announced buffers, plus the receive
//! thread reading the stream socket. Wire format: the GVSP section of
//! notes/gvcp-wirefacts.md (spec §24–25), codec in `gentl_gvcp::gvsp`.
//!
//! Reassembly is zero-copy: payload bytes are written directly into the
//! buffer reserved at the leader; core finalizes at the trailer. Blocks
//! with in-block packet gaps are dropped (M2 has no resend; that is M3).

use std::io;
use std::net::UdpSocket;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gentl_core::ds::{DsCore, FrameError, FrameMeta, FrameSink, FrameTarget};
use gentl_gvcp::gvsp::*;

/// Receive buffer size; covers the largest possible UDP datagram.
const RECV_BUF_LEN: usize = 65536;
/// Socket read timeout: the poll cadence for stop/kill flags.
const SOCKET_TIMEOUT: Duration = Duration::from_millis(100);
/// Grace period after a graceful stop request while waiting for the
/// in-flight block's trailer before giving up on it (GVSP has no explicit
/// block-cancel; OPEN-QUESTION Q18).
const STOP_GRACE: Duration = Duration::from_secs(2);

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

/// One open block being reassembled.
struct Block {
    block_id: u64,
    leader: ImageLeader,
    /// Payload bytes per full packet: derived from SCPS0 (packet_size minus
    /// IP/UDP/GVSP headers) if sane, else inferred from the block's first
    /// payload packet (packet_id == 1).
    stride: Option<usize>,
    /// Highest payload packet id seen (0 = none yet).
    highest_packet: u32,
    /// Payload bytes written so far (detects truncated blocks: the
    /// trailer cannot be told apart from a short last packet by ids
    /// alone, so the byte count must match the leader's expectation).
    bytes_written: usize,
    /// A packet gap (or overrun) was detected; the block is dropped at
    /// the trailer (M2: no resend).
    gap: bool,
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
/// `payload_size` is the GVSP generic payload packet data length: the SCPS0
/// packet_size field (spec §36.58, total size including IP/UDP/GVSP
/// headers) minus `GVSP_HEADERS_OVERHEAD`; 0 means "unknown" and makes the
/// reassembler infer the stride from each block's first payload packet.
pub struct GvspReassembler {
    payload_size: usize,
    block: Option<Block>,
    stats: ReceiverStats,
}

impl GvspReassembler {
    pub fn new(payload_size: usize) -> Self {
        Self {
            payload_size,
            block: None,
            stats: ReceiverStats::default(),
        }
    }

    pub fn stats(&self) -> &ReceiverStats {
        &self.stats
    }

    /// A buffer is reserved for the current block.
    pub fn has_in_flight(&self) -> bool {
        self.block.as_ref().is_some_and(|b| b.target.is_some())
    }

    /// End the current block without a trailer.
    pub fn abandon_block(&mut self, sink: &DsCore, incomplete_to_output: bool) {
        if let Some(block) = self.block.take() {
            if block.target.is_some() {
                sink.abort_frame(incomplete_to_output);
            }
        }
    }

    /// Process one GVSP datagram.
    pub fn on_datagram(&mut self, sink: &DsCore, dgram: &[u8]) {
        self.stats.datagrams += 1;
        let Some(hdr) = GvspHeader::decode(dgram) else {
            self.stats.malformed += 1;
            return;
        };
        let body = &dgram[GVSP_HEADER_LEN..];
        if hdr.flag & GEV_FLAG_PACKET_RESEND != 0 {
            self.stats.resend_packets += 1;
        }
        if hdr.flag & GEV_FLAG_PREVIOUS_BLOCK_DROPPED != 0 {
            self.stats.prev_block_dropped_marks += 1;
        }
        match hdr.packet_format {
            PACKET_FORMAT_DATA_LEADER => self.on_leader(sink, &hdr, body),
            PACKET_FORMAT_DATA_PAYLOAD => self.on_payload(&hdr, body),
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
        let Some(leader) = ImageLeader::decode(body) else {
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
            stride: (self.payload_size > 0).then_some(self.payload_size),
            highest_packet: 0,
            bytes_written: 0,
            gap: false,
            target,
        });
    }

    fn on_payload(&mut self, hdr: &GvspHeader, body: &[u8]) {
        let Some(block) = &mut self.block else {
            self.stats.orphan_packets += 1;
            return;
        };
        if hdr.packet_id == 0 {
            self.stats.malformed += 1; // packet_id 0 is the leader
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
        // In-block gap detection by packet id sequence (§24: ids run from
        // 1 without holes; the resend flag marks retransmissions).
        if block.highest_packet == 0 {
            if hdr.packet_id > 1 {
                block.gap = true;
            }
        } else if hdr.packet_id > block.highest_packet + 1 {
            block.gap = true;
        }
        block.highest_packet = block.highest_packet.max(hdr.packet_id);
        let Some((base, capacity)) = block.target.as_ref().map(|t| (t.base, t.capacity)) else {
            return; // dropping block: payloads counted, not written
        };
        let Some(offset) = (hdr.packet_id as usize)
            .checked_sub(1)
            .and_then(|k| k.checked_mul(stride))
        else {
            block.gap = true;
            return;
        };
        if offset >= capacity {
            block.gap = true;
            return;
        }
        let n = body.len().min(capacity - offset);
        if n < body.len() {
            block.gap = true; // would overflow the buffer
        }
        // SAFETY: `base` is exclusively reserved for this block until the
        // paired on_frame/abort_frame; offset+n <= capacity.
        unsafe {
            std::ptr::copy_nonoverlapping(body.as_ptr(), base.add(offset), n);
        }
        block.bytes_written += n;
        self.stats.payload_bytes += n as u64;
    }

    fn on_trailer(&mut self, sink: &DsCore, hdr: &GvspHeader, body: &[u8]) {
        let Some(block) = self.block.take() else {
            self.stats.orphan_packets += 1;
            return;
        };
        if block.block_id != hdr.block_id {
            self.stats.malformed += 1;
        }
        if block.target.is_none() {
            return; // dropping block: already counted at the leader
        }
        // Trailer sanity: the delivered row count must match the leader,
        // and — when the pixel format has a known bytes-per-pixel value —
        // the accumulated payload must reach the expected size (catches
        // blocks whose tail packets never arrived). Unknown formats cannot
        // be checked this way (Q21).
        let rows_ok = decode_image_trailer_body(body)
            .map(|size_y| size_y == block.leader.size_y)
            .unwrap_or(false);
        if !rows_ok {
            self.stats.malformed += 1;
        }
        let size_ok = frame_meta(&block.leader)
            .payload_bytes()
            .map(|expected| expected == block.bytes_written)
            .unwrap_or(true);
        if block.gap || !rows_ok || !size_ok {
            // M2 drops gapped blocks instead of delivering incomplete
            // buffers; the reserved buffer returns to the input pool.
            self.stats.frames_dropped_incomplete += 1;
            sink.abort_frame(false);
            return;
        }
        if sink.on_frame(block.block_id, frame_meta(&block.leader), &[]).is_some() {
            self.stats.frames_delivered += 1;
        }
        self.stats.frames_completed += 1;
    }
}

/// Receive thread: `recv_from` loop into the reassembler until stopped.
/// A graceful stop lets the in-flight block finish (its trailer still
/// arrives); a kill aborts it incomplete; both fall back to abandoning the
/// block after [`STOP_GRACE`]. `payload_size` is the GVSP generic payload
/// packet data length (SCPS0 packet_size minus the IP/UDP/GVSP headers);
/// 0 lets the reassembler infer it from each block's first payload packet.
pub fn receiver_loop(
    socket: UdpSocket,
    sink: Arc<DsCore>,
    stop: Arc<AtomicBool>,
    kill: Arc<AtomicBool>,
    payload_size: usize,
    stats_out: Arc<Mutex<ReceiverStats>>,
    stream: Arc<crate::stream::GevDataStream>,
) {
    let _ = socket.set_read_timeout(Some(SOCKET_TIMEOUT));
    let mut reasm = GvspReassembler::new(payload_size);
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
        let hdr = GvspHeader {
            status: 0,
            flag: 0,
            extended_id: true,
            packet_format: PACKET_FORMAT_DATA_TRAILER,
            block_id,
            packet_id: 0,
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
        let mut r = GvspReassembler::new(0);
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

    #[test]
    fn packet_gap_drops_block_and_counts() {
        let (core, buf_id) = started_sink();
        let mut r = GvspReassembler::new(CHUNK);
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
        let mut r = GvspReassembler::new(CHUNK);
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
        let mut r = GvspReassembler::new(CHUNK);
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
        let mut r = GvspReassembler::new(CHUNK);
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
        let mut r = GvspReassembler::new(CHUNK);
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
        let mut r = GvspReassembler::new(CHUNK);
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
        let mut r = GvspReassembler::new(CHUNK);
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
        let mut r = GvspReassembler::new(CHUNK);
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
        let mut r = GvspReassembler::new(CHUNK);
        r.on_datagram(&core, &leader_dgram(1, 1));
        r.abandon_block(&core, false);
        assert_eq!(core.queued_count(), 1, "graceful abort returns to input");
        core.flush(AcqQueueOp::AllDiscard).unwrap();
    }
}
