//! The relay: virtual stream channel 0 and its generic driver.
//!
//! Opening the virtual channel (SCP0 != 0, spec §36.57) asks the
//! [`CameraBack`] for a downstream [`FrameSource`], builds a front
//! [`FrameSession`] via the factory, and spawns the two driver threads:
//! source → bounded frame ring (depth 4, backpressure boundary) →
//! session. Closing (SCP0 = 0) stops the threads and cascades the
//! downstream close (R-091ca). All wire-specific knowledge lives behind
//! the three traits; this module only owns the channel register file and
//! the thread structure (notes/linux-vgige-design.md §4).
//!
//! v1 (design note §5): channel 0 only; no PACKETRESEND scheduling towards
//! the remote client (the virtual device does not advertise the capability
//! — the register space masks the bit); the receiving side's buffer sizing
//! is the source implementation's business (back-gige: fixed 16 MiB
//! slots).

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use gentl_gvcp::registers as reg;
use gentl_gvcp::wire::*;

use crate::back::SharedBack;
use crate::frame::{CanonicalFrame, FrameSession, FrameSource};
use crate::GEV_STATUS_ERROR;

/// Ring depth between receiver and sender threads (frames).
const RING_DEPTH: usize = 4;
/// Poll cadence for stop flags / empty rings / idle sources.
const POLL: Duration = Duration::from_millis(100);

/// Virtual stream-channel register file (spec §36.57–§36.60, channel 0).
/// Shared with the front's session: the sender re-reads it per frame for
/// the destination and slice size.
#[derive(Clone, Copy)]
pub struct StreamRegs {
    pub scp: u32,
    pub scps: u32,
    pub scpd: u32,
    pub scda: u32,
}

impl Default for StreamRegs {
    fn default() -> Self {
        Self {
            scp: 0,
            // §36.58 packet_size counts IP+UDP+GVSP headers; 1500 is the
            // classic Ethernet MTU the remote client expects by default.
            scps: reg::scps_make(1500),
            scpd: 0,
            scda: 0,
        }
    }
}

/// Payload bytes per packet derived from a virtual SCPS0 value: packet_size
/// counts IP+UDP+GVSP (§36.58; the 3.0 header budget is
/// GVSP_HEADERS_OVERHEAD = 48). An absurd request (< headers + 1 payload
/// byte) falls back to the 1500-byte MTU default rather than producing a
/// zero-length slice (Q21-class fallback).
pub fn virtual_payload_chunk(scps: u32) -> usize {
    reg::scps_packet_size(scps)
        .checked_sub(reg::GVSP_HEADERS_OVERHEAD)
        .filter(|c| *c >= 1)
        .map(|c| c as usize)
        .unwrap_or(1500 - reg::GVSP_HEADERS_OVERHEAD as usize)
}

/// Front plug point: builds the encoding session for one channel open, with
/// the shared register file attached. `FnMut` so the relay stays reusable
/// across open/close cycles (the factory is stored, not consumed).
pub type SessionFactory =
    Box<dyn FnMut(Arc<Mutex<StreamRegs>>) -> Box<dyn FrameSession> + Send>;

struct RelayInner {
    regs: Arc<Mutex<StreamRegs>>,
    /// Live driver threads; `None` while the virtual channel is closed.
    run: Mutex<Option<RelayRun>>,
    back: SharedBack,
    frames_relayed: Arc<AtomicU32>,
    factory: Mutex<SessionFactory>,
}

/// Live driver threads of one open channel.
struct RelayRun {
    stop: Arc<AtomicBool>,
    rx: Option<JoinHandle<()>>,
    tx: Option<JoinHandle<()>>,
}

impl RelayRun {
    fn stop_and_join(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.rx.take() {
            let _ = t.join();
        }
        if let Some(t) = self.tx.take() {
            let _ = t.join();
        }
    }
}

impl Drop for RelayRun {
    fn drop(&mut self) {
        self.stop_and_join();
    }
}

/// Virtual stream channel 0. Cloneable; the register space keeps one
/// instance and routes the stream-channel addresses here.
#[derive(Clone)]
pub struct Relay {
    inner: Arc<RelayInner>,
    stream_enabled: bool,
}

impl Relay {
    pub fn new(back: SharedBack, stream_enabled: bool, factory: SessionFactory) -> Self {
        Self {
            inner: Arc::new(RelayInner {
                regs: Arc::new(Mutex::new(StreamRegs::default())),
                run: Mutex::new(None),
                back,
                frames_relayed: Arc::new(AtomicU32::new(0)),
                factory: Mutex::new(factory),
            }),
            stream_enabled,
        }
    }

    /// Read one virtual channel-0 register (`off` is the offset inside the
    /// 0x40 block: SCP/SCPS/SCPD/SCDA, decoded by
    /// [`reg::stream_channel_addr`]). All four are served locally — nothing
    /// reaches the camera (the §36.57–§36.60 semantics are re-implemented
    /// by the virtual channel).
    pub fn read_reg(&self, off: u32) -> Result<u32, u16> {
        let regs = self.inner.regs.lock().expect("vgige relay regs poisoned");
        Ok(match off {
            reg::SCP_OFFSET => regs.scp,
            reg::SCPS_OFFSET => regs.scps,
            reg::SCPD_OFFSET => regs.scpd,
            _ => regs.scda,
        })
    }

    /// Write one virtual channel-0 register. `SCPx` non-zero opens the
    /// relay (downstream stream open + driver threads); `SCPx` = 0 closes
    /// it (threads stopped, then the camera's stream close, R-091ca).
    pub fn write_reg(&self, off: u32, value: u32) -> Result<(), u16> {
        match off {
            reg::SCP_OFFSET => self.write_scp(value),
            reg::SCPS_OFFSET => self.write_scps(value),
            reg::SCPD_OFFSET => {
                self.with_regs_mut(|r| r.scpd = value);
                Ok(())
            }
            _ => {
                self.with_regs_mut(|r| r.scda = value);
                Ok(())
            }
        }
    }

    /// Frames relayed since startup (custom-window slot 3, FrameCountRelayed).
    pub fn count(&self) -> u32 {
        self.inner.frames_relayed.load(Ordering::Relaxed)
    }

    /// Idempotent, quiet close: stop the driver threads and cascade the
    /// downstream stream close. Used by the SCP0=0 write path and by
    /// `Drop`.
    pub fn close(&self) {
        let run = self.inner.run.lock().expect("vgige relay run poisoned").take();
        let was_open = run.is_some();
        drop(run); // RelayRun::drop stops and joins the threads.
        if was_open {
            self.with_regs_mut(|r| r.scp = 0);
            let result = self.inner.back.lock().map(|mut b| b.close_stream());
            match result {
                Ok(Ok(())) => {}
                Ok(Err(e)) => eprintln!("vgige: downstream stream close failed: 0x{e:04X}"),
                Err(_) => eprintln!("vgige: back lock poisoned during cascade"),
            }
        }
    }

    fn with_regs_mut(&self, f: impl FnOnce(&mut StreamRegs)) {
        f(&mut self.inner.regs.lock().expect("vgige relay regs poisoned"));
    }

    /// Programmatic open without a GVCP host_port (front-u3v: the U3V
    /// host's Stream Enable is the open trigger, not a SCPx write; the
    /// virtual register file is a GigE-front concept).
    pub fn open(&self) -> Result<(), u16> {
        let open = self
            .inner
            .run
            .lock()
            .expect("vgige relay run poisoned")
            .is_some();
        if open {
            return Err(GEV_STATUS_BUSY);
        }
        if !self.stream_enabled {
            return Err(GEV_STATUS_ACCESS_DENIED);
        }
        self.open_channel(0)
    }

    /// SCPx write (spec §36.57): the host_port field (low 16 bits, §36.57
    /// MSB-first print — Q31 convention) opens/closes the channel.
    fn write_scp(&self, value: u32) -> Result<(), u16> {
        let host_port = reg::scp_host_port(value);
        if value != 0 && host_port == 0 {
            // A non-zero write with the host_port field at 0 carries no
            // port (the pre-Q31 "<<16" layout mistake). Reject loudly so a
            // wrong layout fails at the device instead of silently closing
            // the channel (notes/OPEN-QUESTIONS.md Q14/Q31).
            return Err(GEV_STATUS_INVALID_PARAMETER);
        }
        let open = self
            .inner
            .run
            .lock()
            .expect("vgige relay run poisoned")
            .is_some();
        if host_port != 0 {
            if open {
                // R-586cd: only a zero write (close) is accepted while the
                // channel is streaming.
                return Err(GEV_STATUS_BUSY);
            }
            if !self.stream_enabled {
                // --no-stream control relay: the virtual device runs with
                // streaming withdrawn; channel registers are inaccessible
                // (CR-671cd precedent).
                return Err(GEV_STATUS_ACCESS_DENIED);
            }
            return self.open_channel(host_port);
        }
        if open {
            self.close();
        } else {
            self.with_regs_mut(|r| r.scp = 0);
        }
        Ok(())
    }

    /// SCPSx write (spec §36.58). The value is stored locally and decides
    /// the packet size of the blocks the virtual channel emits; the read
    /// back value is exactly what was written (R-562st local version — the
    /// virtual transmitter always honors the requested size). Unlike a real
    /// device, plain writes are accepted while streaming and adopted per
    /// frame (the virtual channel is the gateway's own transmitter; noted
    /// in the design doc). The fire_test_packet bit is self-cleared on
    /// store; firing an actual test packet is not implemented (v1).
    fn write_scps(&self, value: u32) -> Result<(), u16> {
        self.with_regs_mut(|r| r.scps = value & !reg::SCPS_FIRE_TEST_PACKET);
        Ok(())
    }

    /// Open the relay: downstream stream open via the [`CameraBack`] (the
    /// adapter owns the wire order and the receive socket), then the two
    /// driver threads. Downstream errors pass through as GEV status so the
    /// remote client sees the real reason.
    fn open_channel(&self, host_port: u16) -> Result<(), u16> {
        let source = {
            let mut back = self.inner.back.lock().map_err(|_| GEV_STATUS_ERROR)?;
            back.open_stream().map_err(|status| {
                // The adapter has already logged the wire-level detail.
                status
            })?
        };

        let mut factory = self.inner.factory.lock().map_err(|_| GEV_STATUS_ERROR)?;
        let session = factory(Arc::clone(&self.inner.regs));
        drop(factory);

        let stop = Arc::new(AtomicBool::new(false));
        let (frame_tx, frame_rx) = sync_channel(RING_DEPTH);
        let rx_stop = stop.clone();
        let rx_handle = std::thread::Builder::new()
            .name("vgige-relay-rx".into())
            .spawn(move || driver_rx(rx_stop, source, frame_tx))
            .map_err(|_| GEV_STATUS_ERROR)?;

        let frames_relayed = Arc::clone(&self.inner.frames_relayed);
        let tx_stop = stop.clone();
        let hint = virtual_payload_chunk(self.inner.regs.lock().expect("vgige relay regs poisoned").scps);
        let tx_handle = std::thread::Builder::new()
            .name("vgige-relay-tx".into())
            .spawn(move || driver_tx(tx_stop, session, frame_rx, frames_relayed, hint))
            .map_err(|_| GEV_STATUS_ERROR)?;

        self.with_regs_mut(|r| r.scp = reg::scp_make(host_port));
        *self.inner.run.lock().expect("vgige relay run poisoned") = Some(RelayRun {
            stop,
            rx: Some(rx_handle),
            tx: Some(tx_handle),
        });
        Ok(())
    }
}

impl Drop for Relay {
    fn drop(&mut self) {
        // Cascade the channel close downstream (R-091ca) before the back
        // handle goes away; errors are already logged by close().
        self.close();
    }
}

/// Receiver side of the driver: poll the source, push frames into the
/// bounded ring (a full ring blocks here; datagrams the kernel cannot
/// buffer drop upstream and surface as incomplete blocks in the source's
/// own accounting).
fn driver_rx(
    stop: Arc<AtomicBool>,
    mut source: Box<dyn FrameSource>,
    frame_tx: SyncSender<CanonicalFrame>,
) {
    while !stop.load(Ordering::Relaxed) {
        if let Some(frame) = source.next_frame(POLL) {
            // Bounded ring with cancellation: wait for the sender while
            // checking the stop flag.
            let mut frame = frame;
            loop {
                match frame_tx.try_send(frame) {
                    Ok(()) => break,
                    Err(std::sync::mpsc::TrySendError::Full(f)) => {
                        frame = f;
                        if stop.load(Ordering::Relaxed) {
                            return;
                        }
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    Err(std::sync::mpsc::TrySendError::Disconnected(_)) => return,
                }
            }
        }
    }
}

/// Sender side of the driver: pop frames and hand them to the session for
/// wire encoding. `payload_hint` seeds the session's open() with the slice
/// size derived from the virtual SCPS0 at channel-open time.
fn driver_tx(
    stop: Arc<AtomicBool>,
    mut session: Box<dyn FrameSession>,
    frame_rx: Receiver<CanonicalFrame>,
    frames_relayed: Arc<AtomicU32>,
    payload_hint: usize,
) {
    if let Err(e) = session.open(payload_hint) {
        eprintln!("vgige: relay session open: {e}");
        return;
    }
    while !stop.load(Ordering::Relaxed) {
        match frame_rx.recv_timeout(POLL) {
            Ok(frame) => {
                session.send_frame(&frame);
                frames_relayed.fetch_add(1, Ordering::Relaxed);
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    session.close();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scps_chunk_math() {
        // §36.58: payload per packet = packet_size - (IP 20 + UDP 8 + GVSP 20).
        assert_eq!(virtual_payload_chunk(reg::scps_make(348)), 300);
        // Absurd sizes fall back to the MTU default instead of 0.
        assert_eq!(virtual_payload_chunk(reg::scps_make(10)), 1452);
        // The default virtual SCPS0 is a plain 1500 (no flag bits).
        assert_eq!(StreamRegs::default().scps, reg::scps_make(1500));
        assert_eq!(virtual_payload_chunk(StreamRegs::default().scps), 1452);
    }
}
