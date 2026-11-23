//! Downlink control-channel heartbeat (GigE Vision 3.0 §10.5, R-055ca).
//!
//! The gateway holds Exclusive Access on the real camera for its whole
//! lifetime, but the downlink control channel is idle whenever the remote
//! host has no transaction to forward (the image path is GVSP-only). A
//! compliant device closes the control channel after 3× its heartbeat
//! timeout without primary traffic (R-076cd), after which pass-through
//! writes fail and a camera that couples the channel to the stream may stop
//! transmitting (R-091cd). This worker keeps the channel alive:
//!
//! - it reads the device's heartbeat timeout once (§36.30) and polls every
//!   timeout/3 (§10.5: the heartbeat period must be at most a third of the
//!   timeout);
//! - each poll reads the CCP register — a read is itself primary traffic, so
//!   it refreshes the counter (R-075cd);
//! - if the camera already released the channel (CCP without the EA bit),
//!   the worker re-grants exclusive access, so a stall does not wedge the
//!   gateway until the next host write.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use gentl_gvcp::registers as reg;

use crate::SharedBack;

/// Fallback period when the camera does not answer the timeout register.
const DEFAULT_TIMEOUT_MS: u32 = 3000;
/// Lowest polling period. §36.30 clamps the timeout to >= 500 ms, so the
/// nominal period is >= 167 ms; the floor only guards absurd values.
const MIN_PERIOD: Duration = Duration::from_millis(150);

/// Spawn the heartbeat worker. It runs until `stop` is set and exits within
/// one period after that.
pub fn spawn(back: SharedBack, stop: Arc<AtomicBool>) -> JoinHandle<()> {
    let timeout_ms = back
        .lock()
        .ok()
        .and_then(|mut b| b.read_reg(reg::HEARTBEAT_TIMEOUT).ok())
        .unwrap_or(DEFAULT_TIMEOUT_MS)
        .max(500);
    let period = MIN_PERIOD.max(Duration::from_millis(u64::from(timeout_ms / 3)));
    thread::Builder::new()
        .name("vgige-heartbeat".into())
        .spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                if let Ok(mut b) = back.lock() {
                    match b.read_reg(reg::CCP) {
                        Ok(flags) if flags & reg::CCP_EXCLUSIVE_ACCESS != 0 => {}
                        _ => {
                            // The camera closed the channel while the
                            // gateway was idle: reclaim exclusive access so
                            // the next pass-through transaction succeeds.
                            let _ = b.write_reg(reg::CCP, reg::CCP_EXCLUSIVE_ACCESS);
                        }
                    }
                }
                thread::sleep(period);
            }
        })
        .expect("spawn vgige heartbeat")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::back::{CameraBack, SharedBack};
    use std::sync::Mutex;

    /// Backend that hands out a scripted CCP value and records writes.
    struct FakeBack {
        ccp: u32,
        writes: Arc<Mutex<Vec<(u32, u32)>>>,
    }

    impl CameraBack for FakeBack {
        fn read_reg(&mut self, _addr: u32) -> Result<u32, u16> {
            Ok(self.ccp)
        }
        fn write_reg(&mut self, addr: u32, value: u32) -> Result<(), u16> {
            self.writes.lock().unwrap().push((addr, value));
            Ok(())
        }
        fn read_block(&mut self, _addr: u32, _len: u32) -> Result<Vec<u8>, u16> {
            Err(0x8001)
        }
        fn write_block(&mut self, _addr: u32, _data: &[u8]) -> Result<(), u16> {
            Err(0x8001)
        }
        fn fetch_xml(&mut self) -> Result<Vec<u8>, u16> {
            Err(0x8001)
        }
        fn fetch_identity(&mut self) -> Result<crate::BackIdentity, u16> {
            Err(0x8001)
        }
        fn open_stream(&mut self) -> Result<Box<dyn crate::FrameSource>, u16> {
            Err(0x8001)
        }
        fn close_stream(&mut self) -> Result<(), u16> {
            Err(0x8001)
        }
    }

    #[test]
    fn heartbeat_regrants_exclusive_access_and_stops() {
        let writes = Arc::new(Mutex::new(Vec::new()));
        let back: SharedBack = Arc::new(Mutex::new(FakeBack {
            ccp: 0, // channel was dropped by the camera
            writes: Arc::clone(&writes),
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let handle = spawn(Arc::clone(&back), Arc::clone(&stop));
        // The fake reports a 3 s timeout, so the period is 1 s: wait for at
        // least one tick, then stop.
        std::thread::sleep(Duration::from_millis(1200));
        stop.store(true, Ordering::Relaxed);
        handle.join().expect("heartbeat joins");

        let writes = writes.lock().unwrap();
        assert!(
            writes
                .iter()
                .any(|(addr, value)| *addr == reg::CCP && *value == reg::CCP_EXCLUSIVE_ACCESS),
            "heartbeat re-grants exclusive access, writes: {writes:?}"
        );
    }
}
