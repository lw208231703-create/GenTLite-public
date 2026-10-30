//! The stream emitter: canonical frames → U3V data blocks (§5.5).
//! Leader and trailer are built with the shared packets encoders;
//! payload pieces follow the host-programmed transfer geometry
//! (§5.2.1.3) exactly — the host reads planned sizes, so the device
//! must transmit planned sizes (R-127sr short-final semantics are the
//! host's concern, not the device's, for a gateway with a fixed
//! configured frame size; QoI in Q39).

use std::sync::mpsc::Receiver;
use std::sync::Arc;
use std::time::Duration;

use gentl_u3v::packets;
use vgige_core::CanonicalFrame;

use super::{DeviceInner, StreamError, POLL};

/// Emitter thread body: pop frames from the session queue and emit
/// blocks while the host holds Stream Enable (§5.4.1.2). Frames
/// arriving while the stream is off are dropped (the device advertises
/// no buffer model in v1 — same simplification class as the simulator,
/// Q15).
pub(crate) fn emitter_loop(rx: Receiver<super::SessionFrame>, inner: &Arc<DeviceInner>) {
    let mut block_id = 1u64; // R-77st-class per-channel sequence from 1
    let mut generation = 0u64;
    let mut pending: Option<super::SessionFrame> = None;
    while !inner.stop.load(std::sync::atomic::Ordering::Relaxed) {
        if !inner.regs.stream_enabled() {
            // Not streaming: drain silently and wait.
            for _ in rx.try_iter().take(64) {}
            pending = None;
            std::thread::sleep(Duration::from_millis(2));
            continue;
        }
        let frame = match pending.take() {
            Some(f) => f,
            None => match rx.recv_timeout(POLL) {
                Ok(f) => f,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                // All senders dropped (relay closed = stream disabled):
                // the channel outlives them — the next enable clones a
                // fresh sender into it — so wait instead of dying.
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    std::thread::sleep(POLL);
                    continue;
                }
            },
        };
        if frame.generation < generation {
            continue;
        }
        if frame.generation > generation {
            generation = frame.generation;
            pending = None;
            block_id = 1;
        }
        if let Err(e) = emit_block(&*inner.stream_in, &inner.regs, block_id, &frame.frame) {
            eprintln!("vgige: u3v stream emit failed: {e}");
            // Mid-block writes cannot be retried safely (a partial piece
            // plus a re-sent leader desyncs the host) — abort the stream
            // instead: clear Stream Enable so the next host enable is a
            // clean 0→1 transition. The emitter itself never exits: the
            // channel outlives the relay's sessions, so after the abort
            // it waits for the next enable instead of dying silently
            // (a dead emitter needs a daemon restart, observed 2026-10-27).
            inner.regs.abort_stream();
        }
        block_id = block_id.wrapping_add(1);
    }
}

/// Send one block: leader piece, payload pieces per the programmed
/// geometry, trailer piece. Each `send` is one logical piece; the
/// transport may split it into MPS transactions (the host's UQ16
/// accumulation reassembles).
fn emit_block(
    stream: &dyn super::DeviceTransfers,
    regs: &super::RegisterFile,
    block_id: u64,
    frame: &CanonicalFrame,
) -> Result<(), StreamError> {
    let leader = packets::ImageLeader {
        leader_size: packets::IMAGE_LEADER_LEN as u16,
        block_id,
        payload_type: packets::PAYLOAD_TYPE_IMAGE,
        timestamp: frame.timestamp,
        pixel_format: frame.pixel_format,
        size_x: frame.width,
        size_y: frame.height,
        offset_x: frame.offset_x,
        offset_y: frame.offset_y,
        // Canonical padding_y has no U3V field (§5.5.5.1.1 carries one
        // padding); the relay's padded-row bytes ride in the payload.
        padding_x: frame.padding_x as u16,
    };
    if !regs.stream_enabled() {
        return Err(StreamError::Closed);
    }
    stream.send(&packets::encode_image_leader(&leader))?;

    let payload = &frame.payload;
    let (ts, count, f1, f2) = regs.programmed_geometry();
    emit_payload(stream, payload, ts, count, f1, f2, || regs.stream_enabled())?;

    let trailer = packets::encode_image_trailer(
        block_id,
        packets::GENCP_SUCCESS,
        payload.len() as u64,
        frame.height,
    );
    stream.send(&trailer)
}

fn emit_payload(
    stream: &dyn super::DeviceTransfers,
    payload: &[u8],
    ts: u32,
    count: u32,
    f1: u32,
    f2: u32,
    enabled: impl Fn() -> bool,
) -> Result<(), StreamError> {
    if ts == 0 && count == 0 && f1 == 0 && f2 == 0 {
        // Host enabled without programming: emit the payload as one
        // piece (QoI — a standards-strict device would reject at Stream
        // Enable with SI_REGISTERS_INCONSISTENT; Q39).
        if !enabled() {
            return Err(StreamError::Closed);
        }
        return stream.send(payload);
    }
    let planned: Vec<u32> = (0..count)
        .map(|_| ts)
        .chain([f1, f2])
        .filter(|&s| s > 0)
        .collect();
    let mut offset = 0usize;
    for size in planned {
        if !enabled() {
            return Err(StreamError::Closed);
        }
        let n = size as usize;
        let end = (offset + n).min(payload.len());
        let mut piece = Vec::with_capacity(n);
        piece.extend_from_slice(&payload[offset..end]);
        piece.resize(n, 0);
        stream.send(&piece)?;
        offset = end;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct RecordingTransfers {
        pieces: Mutex<Vec<Vec<u8>>>,
    }

    impl super::super::DeviceTransfers for RecordingTransfers {
        fn send(&self, bytes: &[u8]) -> Result<(), StreamError> {
            self.pieces.lock().unwrap().push(bytes.to_vec());
            Ok(())
        }

        fn max_packet(&self) -> u16 {
            512
        }
    }

    #[test]
    fn short_frame_still_emits_every_programmed_piece() {
        let stream = RecordingTransfers::default();
        emit_payload(&stream, &[1, 2], 4, 2, 3, 0, || true).unwrap();
        let pieces = stream.pieces.lock().unwrap();
        assert_eq!(pieces.as_slice(), &[
            vec![1, 2, 0, 0],
            vec![0, 0, 0, 0],
            vec![0, 0, 0],
        ]);
    }
}
