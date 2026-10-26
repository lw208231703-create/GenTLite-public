//! USB I/O backend abstraction (Phase B Linux port, notes/PENDING.md
//! "Linux 可移植"): [`UsbPipe`] is the minimal endpoint surface that
//! control.rs (GenCP transactions) and stream.rs (the SIRM receiver)
//! actually need — bulk read/write with timeouts, error-recovery flush,
//! pipe policy, and the endpoint MPS. Per-OS implementations live in
//! winusb.rs (the pre-port WinUSB code, behavior unchanged) and
//! libusb.rs (rusb). The OS-independent piece-accumulation read layer
//! (notes/OPEN-QUESTIONS.md UQ16 — exact-size accumulation for planned
//! payload pieces, size-capped accumulation for leader/trailer, ZLP
//! skip, short-transaction termination) lives here verbatim from
//! stream.rs, now unit-testable against a scripted mock on both
//! platforms.

use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use gentl_core::{GentlError, GentlResult};

#[cfg(windows)]
mod winusb;
#[cfg(not(windows))]
pub(crate) mod libusb;

#[cfg(windows)]
pub use winusb::last_error_text;
#[cfg(windows)]
pub use winusb::InterfaceSet;
#[cfg(not(windows))]
pub use libusb::InterfaceSet;

/// Pipe poll cadence for the receiver stop flags.
pub(crate) const POLL: Duration = Duration::from_millis(100);
/// Grace period waiting for the in-flight block's trailer after a
/// graceful stop request (U3V has no block-cancel; §5.4.1.2 abort is the
/// kill path).
pub(crate) const STOP_GRACE: Duration = Duration::from_secs(2);

/// One opened bulk endpoint of a U3V interface, backend-owned.
///
/// Lifetime invariant (same discipline as the pre-trait raw handles): a
/// pipe borrows its interface set's lifetime; the device module owns the
/// set and joins receiver threads before it can drop. The Windows
/// implementation is a raw-handle wrapper, so `Send`/`Sync` is upheld by
/// the caller's Mutex + join-before-drop discipline exactly as before.
pub trait UsbPipe: Send + Sync {
    /// Bulk IN transfer into `buf`; returns the number of bytes
    /// transferred. Completion contract both backends honor: the request
    /// ends on buffer-full or short packet (on Windows that is the RAW_IO
    /// documented behavior, UQ16; on Linux it is the usbfs/libusb sync
    /// transfer behavior). A zero transfer (ZLP) is a normal return, not
    /// an error. `GentlError::Timeout` means no data within `timeout` and
    /// is the caller's idle/poll signal.
    fn read_bulk(&self, buf: &mut [u8], timeout: Duration) -> GentlResult<u32>;

    /// Bulk OUT of the whole buffer (commands never exceed the Maximum
    /// Command Transfer Length, so a short write is an error).
    fn write_bulk(&self, buf: &[u8], timeout: Duration) -> GentlResult<()>;

    /// Best-effort inbound resync after a failed transaction. Windows:
    /// WinUsb_FlushPipe discards a stale acknowledge. Linux: libusb's
    /// synchronous transfer has no stale-URB concept (a timed-out read is
    /// already cancelled), so this maps to `clear_halt` — the endpoint
    /// recovery for a halt left behind by an error, i.e. the UQ6
    /// ENDPOINT_HALT re-synchronization expressed for usbfs.
    fn flush(&self);

    /// Backend pipe policy on the streaming (bulk IN) endpoint. Windows:
    /// enables RAW_IO (bypasses WinUSB's request-queuing layer, UQ16) and
    /// requires the MPS-aligned-request contract honored by
    /// [`align_request`]/[`aligned_window`]. Linux: no-op — usbfs has no
    /// request-queuing bypass policy; the sync transfer already is the
    /// unqueued path, and the UQ16 accumulation layer (necessary and
    /// sufficient for correct framing) is OS-independent.
    fn set_raw_io(&self) -> GentlResult<()>;

    /// Endpoint wMaxPacketSize — feeds the aligned-request contract.
    fn max_packet(&self) -> u16;
}

/// Round a piece size up to the endpoint's maximum packet size, as RAW_IO
/// requires every request length to be a multiple of it.
pub fn align_request(size: u32, mps: u16) -> u32 {
    let mps = u32::from(mps.max(1));
    size.div_ceil(mps) * mps
}

/// Largest MPS-aligned window of `buf` starting at `done` — every
/// request must be a multiple of the endpoint MPS, and the slice length
/// is the request length.
pub(crate) fn aligned_window(buf: &[u8], done: usize, mps: usize) -> Option<usize> {
    let rem = buf.len().checked_sub(done)?;
    let n = (rem / mps) * mps;
    (n > 0).then_some(n)
}

/// Read outcomes that map to stop semantics.
#[derive(Debug)]
pub(crate) enum ReadAbort {
    Stop,
    Io(#[allow(dead_code)] String), // detail for the stats/log follow-up
}

/// One read with stop-flag polling. `in_block` controls the grace: a
/// graceful stop lets the in-flight block finish within [`STOP_GRACE`]
/// while an idle pipe (between blocks) exits immediately. Data starvation
/// past 30 s is treated as an I/O error.
pub(crate) fn poll_read(
    pipe: &dyn UsbPipe,
    buf: &mut [u8],
    stop: &AtomicBool,
    in_block: bool,
) -> Result<u32, ReadAbort> {
    use std::sync::atomic::Ordering;
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut grace: Option<Instant> = None;
    loop {
        if stop.load(Ordering::SeqCst) {
            if !in_block {
                return Err(ReadAbort::Stop);
            }
            match grace {
                None => grace = Some(Instant::now() + STOP_GRACE),
                Some(expiry) => {
                    if Instant::now() >= expiry {
                        return Err(ReadAbort::Stop);
                    }
                }
            }
        }
        match pipe.read_bulk(buf, POLL) {
            Ok(n) => return Ok(n),
            Err(GentlError::Timeout) => {
                if Instant::now() > deadline {
                    return Err(ReadAbort::Io("stream starved".into()));
                }
                continue;
            }
            Err(GentlError::Io(e)) => return Err(ReadAbort::Io(e)),
            Err(e) => return Err(ReadAbort::Io(e.to_string())),
        }
    }
}

/// Plan-exact accumulation for payload pieces (sizes known from the SIRM
/// geometry): read until exactly `expected` bytes are in, skipping ZLPs.
/// `buf` capacity must be `align_up(expected, mps)`; a piece longer than
/// planned is a protocol violation (Desync).
pub(crate) fn read_piece_exact(
    pipe: &dyn UsbPipe,
    buf: &mut [u8],
    expected: usize,
    mps: usize,
    stop: &AtomicBool,
) -> Result<u32, ReadAbort> {
    let mut done = 0usize;
    while done < expected {
        let Some(win) = aligned_window(buf, done, mps) else {
            return Err(ReadAbort::Io("piece ended before planned size".into()));
        };
        let n = poll_read(pipe, &mut buf[done..done + win], stop, true)? as usize;
        if n == 0 {
            continue; // ZLP between transactions
        }
        if done + n > expected {
            return Err(ReadAbort::Io("piece exceeded planned size".into()));
        }
        done += n;
    }
    Ok(done as u32)
}

/// Size-capped accumulation for leader/trailer (only their maximum is
/// known): a sub-MPS return or a ZLP ends the piece; otherwise fill the
/// aligned capacity. `buf` capacity must be `align_up(max, mps)`.
pub(crate) fn read_piece_capped(
    pipe: &dyn UsbPipe,
    buf: &mut [u8],
    mps: usize,
    stop: &AtomicBool,
    in_block: bool,
) -> Result<u32, ReadAbort> {
    let mut done = 0usize;
    loop {
        let Some(win) = aligned_window(buf, done, mps) else {
            return Ok(done as u32);
        };
        let n = poll_read(pipe, &mut buf[done..done + win], stop, in_block)? as usize;
        done += n;
        if n < mps || n == 0 {
            // Short transaction or ZLP: the piece has ended.
            return Ok(done as u32);
        }
    }
}

#[cfg(test)]
pub(crate) mod mock {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Arc;

    /// Scripted bulk-IN pipe: `read_bulk` pops the next scripted chunk
    /// size (capped to the request length), fills those bytes with a
    /// deterministic pattern, and returns it. An empty script answers
    /// `Timeout` (idle pipe) — so `poll_read` retries like on real
    /// hardware, and tests must script enough chunks to reach their
    /// expectation within the poll loop.
    pub(crate) struct MockPipe {
        pub(crate) script: std::sync::Mutex<VecDeque<usize>>,
        pub(crate) mps: u16,
    }

    impl MockPipe {
        pub(crate) fn new(chunks: &[usize], mps: u16) -> Self {
            Self {
                script: std::sync::Mutex::new(chunks.iter().copied().collect()),
                mps,
            }
        }
    }

    impl UsbPipe for MockPipe {
        fn read_bulk(&self, buf: &mut [u8], _timeout: Duration) -> GentlResult<u32> {
            let mut s = self.script.lock().expect("mock script lock");
            let Some(n) = s.pop_front() else {
                return Err(GentlError::Timeout);
            };
            let n = n.min(buf.len());
            for (i, b) in buf[..n].iter_mut().enumerate() {
                *b = (i as u8) ^ 0xA5;
            }
            Ok(n as u32)
        }
        fn write_bulk(&self, _buf: &[u8], _timeout: Duration) -> GentlResult<()> {
            Ok(())
        }
        fn flush(&self) {}
        fn set_raw_io(&self) -> GentlResult<()> {
            Ok(())
        }
        fn max_packet(&self) -> u16 {
            self.mps
        }
    }

    /// Convenience: a pipe object with `chunks` scripted.
    pub(crate) fn pipe(chunks: &[usize], mps: u16) -> Arc<dyn UsbPipe> {
        Arc::new(MockPipe::new(chunks, mps))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    #[test]
    fn align_request_rounds_to_mps() {
        assert_eq!(align_request(52, 1024), 1024); // leader
        assert_eq!(align_request(32, 1024), 1024); // trailer
        assert_eq!(align_request(1024, 1024), 1024);
        assert_eq!(align_request(1025, 1024), 2048); // odd final piece
        assert_eq!(align_request(1_048_576, 1024), 1_048_576); // full TS piece
        assert_eq!(align_request(100, 512), 512);
        assert_eq!(align_request(100, 1), 100); // degenerate mps
    }

    #[test]
    fn aligned_window_floors_to_mps_multiple() {
        let buf = [0u8; 3072];
        assert_eq!(aligned_window(&buf, 0, 1024), Some(3072));
        assert_eq!(aligned_window(&buf, 1024, 1024), Some(2048));
        assert_eq!(aligned_window(&buf, 2048, 1024), Some(1024));
        assert_eq!(aligned_window(&buf, 3072, 1024), None);
        // Window smaller than MPS: no request possible.
        let small = [0u8; 512];
        assert_eq!(aligned_window(&small, 0, 1024), None);
    }

    /// UQ16: the device may split one logical transfer into several short
    /// transactions mid-piece — the exact accumulation keeps reading until
    /// the planned size is in.
    #[test]
    fn piece_exact_accumulates_device_split_transactions() {
        let stop = AtomicBool::new(false);
        let pipe = mock::pipe(&[1024, 1024, 500], 1024);
        let mut buf = vec![0u8; align_request(2548, 1024) as usize];
        let n = read_piece_exact(&*pipe, &mut buf, 2548, 1024, &stop).expect("accumulate");
        assert_eq!(n, 2548);
    }

    #[test]
    fn piece_exact_skips_zlps() {
        let stop = AtomicBool::new(false);
        let pipe = mock::pipe(&[0, 0, 300], 1024);
        let mut buf = vec![0u8; align_request(300, 1024) as usize];
        let n = read_piece_exact(&*pipe, &mut buf, 300, 1024, &stop).expect("zlp skip");
        assert_eq!(n, 300);
    }

    #[test]
    fn piece_exact_rejects_oversized_piece() {
        let stop = AtomicBool::new(false);
        let pipe = mock::pipe(&[1024], 1024);
        let mut buf = vec![0u8; 1024];
        let r = read_piece_exact(&*pipe, &mut buf, 1000, 1024, &stop);
        assert!(matches!(r, Err(ReadAbort::Io(_))), "piece exceeded planned size");
    }

    #[test]
    fn piece_exact_fails_when_capacity_runs_out() {
        let stop = AtomicBool::new(false);
        // Contract violation: capacity smaller than the planned piece.
        let pipe = mock::pipe(&[1024], 1024);
        let mut buf = vec![0u8; 1024];
        let r = read_piece_exact(&*pipe, &mut buf, 2048, 1024, &stop);
        assert!(matches!(r, Err(ReadAbort::Io(_))), "piece ended before planned size");
    }

    /// Leader/trailer: a short transaction (sub-MPS) or ZLP ends the
    /// piece; full MPS-multiple reads keep accumulating.
    #[test]
    fn piece_capped_ends_on_short_or_zlp() {
        let stop = AtomicBool::new(false);
        // Short transaction after one full read.
        let pipe = mock::pipe(&[1024, 512], 1024);
        let mut buf = vec![0u8; 2048];
        let n = read_piece_capped(&*pipe, &mut buf, 1024, &stop, false).expect("short ends");
        assert_eq!(n, 1536);

        // ZLP immediately: empty piece.
        let pipe = mock::pipe(&[0], 1024);
        let mut buf = vec![0u8; 1024];
        let n = read_piece_capped(&*pipe, &mut buf, 1024, &stop, false).expect("zlp ends");
        assert_eq!(n, 0);
    }

    #[test]
    fn piece_capped_fills_aligned_capacity() {
        let stop = AtomicBool::new(false);
        // Two full MPS-multiple reads exactly fill the 2 KiB capacity.
        let pipe = mock::pipe(&[1024, 1024], 1024);
        let mut buf = vec![0u8; 2048];
        let n = read_piece_capped(&*pipe, &mut buf, 1024, &stop, true).expect("fill capacity");
        assert_eq!(n, 2048);
    }
}
