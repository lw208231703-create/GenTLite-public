//! Library-shaped U3V client facade (added for the vgige smart-camera
//! gateway, roadmap Step 2 — the "GigE front ← U3V camera" matrix cell).
//!
//! The crate's GenTL producer surface (lib.rs/cti.rs) owns the wire
//! behavior but is handle-shaped and Windows/Linux-USB-bound at the edges.
//! This module exposes the same protocol machinery — GenCP transactions
//! (control.rs), the ABRM/SBRM bootstrap walk, the manifest XML download
//! (zip-aware, GenCP §5.5), the SIRM snapshot/programming and the UQ16
//! piece-accumulation receiver (stream.rs/usbio) — as plain owned objects
//! with no GenTL handles and no buffer-pool requirements:
//!
//! - [`U3vCamera`]: enumerate → open → register/memory access (little-
//!   endian, R-30cd) → device XML → [`U3vStream`].
//! - [`U3vStream`]: program the SIRM (§5.4.1.2) and pull one block at a
//!   time via [`U3vStream::next_block`], bounded by the caller's timeout.
//!
//! Zero protocol duplication: every codec and every accumulation helper is
//! the existing pub(crate) one. The one sibling piece is
//! `read_block_alloc` — the memory-sink counterpart of stream.rs's
//! zero-copy `read_block` (same §5.2.1.3 walk and R-127sr short-piece
//! semantics, `Vec` payload instead of a DsCore reservation; notes/
//! OPEN-QUESTIONS.md Q38).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gentl_core::ds::FrameMeta;
use gentl_core::{GentlError, GentlResult};

use crate::control::{read_bootstrap, read_device_xml, Bootstrap, GenCpClient};
use crate::packets::{self, ImageLeader, ImageTrailer};
use crate::stream::{
    self, drain_payload_and_trailer, program_sirm, read_sirm, SirmInfo, TransferGeometry,
};
use crate::types::FoundDevice;

pub use crate::usbio::UsbPipe;

/// Errors of the facade API.
#[derive(Debug)]
pub enum U3vError {
    /// A device-answered GenCP/U3V status (GenCP 1.3.1 §4.3.2.1 Table 6 and
    /// the U3V technology namespace), verbatim — the gateway forwards it
    /// to its GigE clients unchanged (error transparency).
    Status(u16),
    /// Local/transport failure (USB I/O, malformed wire data, protocol
    /// violation). Maps to the generic GEV 0x8FFF at the gateway seam.
    Transport(String),
    /// No answer within a command timeout.
    Timeout,
}

impl From<GentlError> for U3vError {
    fn from(e: GentlError) -> Self {
        match e {
            GentlError::GevStatus(s) => U3vError::Status(s),
            GentlError::Timeout => U3vError::Timeout,
            other => U3vError::Transport(other.message()),
        }
    }
}

/// One opened U3V camera: a GenCP control channel plus the bootstrap
/// snapshot and the stream endpoint pipes. One instance per physical
/// device; the gateway holds it behind its `CameraBack` seam.
pub struct U3vCamera {
    client: Arc<Mutex<GenCpClient>>,
    bootstrap: Bootstrap,
    stream_in: Vec<Arc<dyn UsbPipe>>,
    event_in: Option<Arc<dyn UsbPipe>>,
    /// Live DEI pump, if any (§5.4.2). Drop order: receiver first, then
    /// the pump thread observes `stop` and exits before the pipes.
    events: Mutex<Option<EventPump>>,
}

impl U3vCamera {
    /// Enumerate U3V devices (per-OS backend; see `discovery`).
    pub fn enumerate() -> Vec<FoundDevice> {
        crate::discovery::enumerate_devices().devices
    }

    /// Open by the backend instance ID (Windows: CM device instance path;
    /// Linux: `<bus>:<address>`), claim the U3V interfaces, and walk the
    /// ABRM/SBRM bootstrap (§4.2).
    pub fn open(instance_id: &str) -> GentlResult<Self> {
        let opened = crate::discovery::open_device_all(instance_id);
        let ifaces = crate::usbio::InterfaceSet::collect(opened)?;
        let control_out = ifaces.control_out.clone().ok_or_else(|| {
            GentlError::Io("device lacks a bulk OUT control endpoint (R-11cd)".into())
        })?;
        let control_in = ifaces.control_in.clone().ok_or_else(|| {
            GentlError::Io("device lacks a bulk IN control endpoint (R-11cd)".into())
        })?;
        // Clone the pipe handles (cheap Arc clones; InterfaceSet is not
        // consumed because it implements Drop on the Windows backend).
        Self::from_parts_full(
            control_out,
            control_in,
            ifaces.stream_in.clone(),
            ifaces.event_in.clone(),
        )
    }

    /// Build a camera from pre-opened endpoint pipes (test fakes, embedded
    /// transports). No USB discovery or interface claiming happens; the
    /// pipes carry their own backend lifetime.
    pub fn from_pipes(
        control_out: Arc<dyn UsbPipe>,
        control_in: Arc<dyn UsbPipe>,
        stream_in: Vec<Arc<dyn UsbPipe>>,
    ) -> GentlResult<Self> {
        Self::from_parts_full(control_out, control_in, stream_in, None)
    }

    /// Build a camera with an optional Device Event Interface endpoint.
    pub fn from_pipes_with_events(
        control_out: Arc<dyn UsbPipe>,
        control_in: Arc<dyn UsbPipe>,
        stream_in: Vec<Arc<dyn UsbPipe>>,
        event_in: Option<Arc<dyn UsbPipe>>,
    ) -> GentlResult<Self> {
        Self::from_parts_full(control_out, control_in, stream_in, event_in)
    }

    fn from_parts_full(
        control_out: Arc<dyn UsbPipe>,
        control_in: Arc<dyn UsbPipe>,
        stream_in: Vec<Arc<dyn UsbPipe>>,
        event_in: Option<Arc<dyn UsbPipe>>,
    ) -> GentlResult<Self> {
        let mut client = GenCpClient::new(control_out, control_in);
        let bootstrap = read_bootstrap(&mut client)?;
        Ok(Self {
            client: Arc::new(Mutex::new(client)),
            bootstrap,
            stream_in,
            event_in,
            events: Mutex::new(None),
        })
    }

    /// The ABRM/SBRM snapshot (identity strings, capability bits, SIRM
    /// location, transfer limits).
    pub fn bootstrap(&self) -> &Bootstrap {
        &self.bootstrap
    }

    fn locked_client(&self) -> Result<std::sync::MutexGuard<'_, GenCpClient>, U3vError> {
        self.client
            .lock()
            .map_err(|_| U3vError::Transport("control client lock poisoned".into()))
    }

    /// READMEM (GenCP §4.4.1), chunked to the device's limits; a non-success
    /// device status is returned verbatim in [`U3vError::Status`].
    pub fn read_mem(&self, addr: u64, buf: &mut [u8]) -> Result<(), U3vError> {
        self.locked_client()?.read_mem_raw(addr, buf)?;
        Ok(())
    }

    /// WRITEMEM (GenCP §4.4.3), chunked; the Written-Length field is
    /// verified when the device provides it (R-111cd area).
    pub fn write_mem(&self, addr: u64, data: &[u8]) -> Result<(), U3vError> {
        self.locked_client()?.write_mem_raw(addr, data)?;
        Ok(())
    }

    /// One 32-bit little-endian register (R-30cd).
    pub fn read_reg(&self, addr: u64) -> Result<u32, U3vError> {
        let mut b = [0u8; 4];
        self.read_mem(addr, &mut b)?;
        Ok(u32::from_le_bytes(b))
    }

    /// One 32-bit little-endian register (R-30cd).
    pub fn write_reg(&self, addr: u64, v: u32) -> Result<(), U3vError> {
        self.write_mem(addr, &v.to_le_bytes())
    }

    /// The device description XML: the manifest's first Device XML entry,
    /// unzipped when stored as ZIP (§4.1.5, GenCP §5.5 — R-59cd requires
    /// one).
    pub fn fetch_xml(&self) -> GentlResult<Vec<u8>> {
        let c = self.locked_client().map_err(u3v_to_gentl)?;
        read_device_xml(&c, self.bootstrap.manifest_table)
    }

    /// Snapshot stream channel `index`'s SIRM (§5.4.1.1) bound to its bulk
    /// IN endpoint. Does not program anything — [`U3vStream::start`] does.
    pub fn stream(&self, channel: u32) -> Result<U3vStream, U3vError> {
        let sirm_address = self
            .bootstrap
            .sirm_address
            .ok_or_else(|| U3vError::Transport("device reports no SIRM".into()))?;
        let pipe = self
            .stream_in
            .get(channel as usize)
            .cloned()
            .ok_or_else(|| {
                U3vError::Transport(format!("stream channel {channel} has no endpoint"))
            })?;
        let sirm = {
            let c = self.locked_client()?;
            read_sirm(&c, sirm_address, channel, self.bootstrap.sirm_length)
                .map_err(U3vError::from)?
        };
        Ok(U3vStream {
            client: Arc::clone(&self.client),
            pipe,
            sirm,
            geometry: Mutex::new(None),
        })
    }
}

impl U3vCamera {
    /// Start receiving device events (§4.1.3.3): raises the EIRM Event
    /// Enable bit when the SBRM reports an event interface (§5.4.2) and
    /// pumps the event bulk IN endpoint into a queue until
    /// [`Self::stop_events`]. One live pump per camera; a second call
    /// returns the existing receiver's queue endpoint is NOT shared —
    /// returns ResourceInUse so consumers stay independent.
    pub fn start_events(&self) -> Result<EventReceiver, U3vError> {
        let mut guard = self
            .events
            .lock()
            .map_err(|_| U3vError::Transport("events lock poisoned".into()))?;
        if guard.is_some() {
            return Err(U3vError::Transport(
                "device events already being received".into(),
            ));
        }
        let pipe = self
            .event_in
            .clone()
            .ok_or_else(|| U3vError::Transport("device has no event endpoint".into()))?;
        // §5.4.2 EI_CONTROL bit 0: enable event generation in the device.
        if let Some(eirm) = self.bootstrap.eirm_address {
            self.locked_client()?
                .write_reg(eirm + packets::eirm::EI_CONTROL, 1)
                .map_err(U3vError::from)?;
        }
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = std::sync::mpsc::channel::<packets::EventPacket>();
        let pump_stop = Arc::clone(&stop);
        let pump = match std::thread::Builder::new()
            .name("u3v-events".into())
            .spawn(move || {
                let mut buf = vec![0u8; 4096];
                while !pump_stop.load(Ordering::Relaxed) {
                    match pipe.read_bulk(&mut buf, EVENTS_POLL) {
                        Ok(0) | Err(GentlError::Timeout) => continue,
                        Err(_) => {
                            std::thread::sleep(EVENTS_POLL);
                            continue;
                        }
                        Ok(n) => {
                            // §4.1.3.3.2: one transfer carries one event;
                            // the SCD may be padded to the endpoint MPS —
                            // decode_event bounds-checks the true length.
                            if let Some(ev) = packets::decode_event(&buf[..n as usize]) {
                                if tx.send(ev).is_err() {
                                    break; // receiver gone
                                }
                            }
                        }
                    }
                }
            }) {
            Ok(pump) => pump,
            Err(e) => {
                if let Some(eirm) = self.bootstrap.eirm_address {
                    let _ = self.locked_client().map(|c| {
                        let _ = c.write_reg(eirm + packets::eirm::EI_CONTROL, 0);
                    });
                }
                return Err(U3vError::Transport(format!("spawn event pump: {e}")));
            }
        };
        *guard = Some(EventPump {
            stop,
            thread: Some(pump),
        });
        Ok(EventReceiver { rx })
    }

    /// Stop the event pump and lower the EIRM Event Enable bit.
    pub fn stop_events(&self) -> Result<(), U3vError> {
        let mut guard = self
            .events
            .lock()
            .map_err(|_| U3vError::Transport("events lock poisoned".into()))?;
        if let Some(mut pump) = guard.take() {
            pump.stop.store(true, Ordering::Relaxed);
            if let Some(t) = pump.thread.take() {
                let _ = t.join();
            }
            if let Some(eirm) = self.bootstrap.eirm_address {
                self.locked_client()?
                    .write_reg(eirm + packets::eirm::EI_CONTROL, 0)
                    .map_err(U3vError::from)?;
            }
        }
        Ok(())
    }
}

impl Drop for U3vCamera {
    fn drop(&mut self) {
        let _ = self.stop_events();
    }
}

const EVENTS_POLL: std::time::Duration = std::time::Duration::from_millis(100);

/// Live pump behind [`U3vCamera::start_events`].
struct EventPump {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

/// Decoded device events (§4.1.3.3.2), until the camera is dropped or
/// [`U3vCamera::stop_events`] is called.
pub struct EventReceiver {
    rx: std::sync::mpsc::Receiver<packets::EventPacket>,
}

impl EventReceiver {
    /// The next device event, or None when the pump has stopped.
    pub fn next(&self, timeout: std::time::Duration) -> Option<packets::EventPacket> {
        self.rx.recv_timeout(timeout).ok()
    }
}

fn u3v_to_gentl(e: U3vError) -> GentlError {
    match e {
        U3vError::Status(s) => GentlError::GevStatus(s),
        U3vError::Transport(m) => GentlError::Io(m),
        U3vError::Timeout => GentlError::Timeout,
    }
}

/// One stream channel of an opened camera: programmed SIRM geometry plus
/// the bulk IN endpoint, pulled block by block.
pub struct U3vStream {
    client: Arc<Mutex<GenCpClient>>,
    pipe: Arc<dyn UsbPipe>,
    sirm: SirmInfo,
    geometry: Mutex<Option<TransferGeometry>>,
}

/// One received data block (U3V 1.2 §5.2/§5.5).
#[derive(Debug)]
pub enum Block {
    /// A complete image block: decoded leader (height already corrected
    /// per CR-86st), the payload bytes, and the trailer.
    Image {
        leader: ImageLeader,
        payload: Vec<u8>,
        trailer: ImageTrailer,
    },
    /// No frame for the caller: a non-image payload (drained to keep the
    /// pipe in sync) or a trailer status other than SUCCESS (§5.5.4:
    /// DATA_DISCARDED / DATA_OVERRUN / reserved). The pipe stays in sync.
    Skipped,
}

impl U3vStream {
    /// Program the transfer geometry (§5.2.1.3) and raise Stream Enable
    /// (§5.4.1.2); the device rejects an illegal set with
    /// U3V_STATUS_SI_REGISTERS_INCONSISTENT. Also applies the backend pipe
    /// policy (RAW_IO on Windows, a no-op on usbfs — UQ16).
    pub fn start(&self) -> Result<(), U3vError> {
        self.pipe.set_raw_io().map_err(U3vError::from)?;
        crate::usbio::drain_stale(&*self.pipe);
        let c = self
            .client
            .lock()
            .map_err(|_| U3vError::Transport("control client lock poisoned".into()))?;
        let geometry = program_sirm(&c, &self.sirm).map_err(U3vError::from)?;
        *self.geometry.lock().expect("geometry lock") = Some(geometry);
        Ok(())
    }

    /// Drop Stream Enable (§5.4.1.2: 0 while active aborts the stream).
    /// Called by `Drop` as well — safe to call twice.
    pub fn stop(&self) {
        if let Ok(c) = self.client.lock() {
            stream::disable_sirm(&c, &self.sirm);
        }
    }

    /// Pull the next block, waiting up to `timeout` for its first piece.
    /// `None` = the window expired before a block started (the natural
    /// poll/idle answer, and the caller's cancellation path). After a
    /// `Some(Ok(..))` the block is complete; after an `Err` the pipe is
    /// desynchronized and the stream is dead (same policy as the GenTL
    /// receiver's desync).
    pub fn next_block(&self, timeout: Duration) -> Option<Result<Block, U3vError>> {
        if timeout.is_zero() {
            return Some(Err(U3vError::Transport(
                "next_block with a zero timeout".into(),
            )));
        }
        let Some(geometry) = *self.geometry.lock().expect("geometry lock") else {
            return Some(Err(U3vError::Transport("stream not started".into())));
        };
        // The piece-accumulation helpers are stop-flag driven; the pull
        // API's only cancellation source is the caller's timeout, armed as
        // a watchdog (QoI: one short-lived thread per call — at gateway
        // frame rates this is far below noise; Q38).
        let stop = Arc::new(AtomicBool::new(false));
        let watchdog_stop = stop.clone();
        std::thread::Builder::new()
            .name("u3v-next-block-watchdog".into())
            .spawn(move || {
                std::thread::sleep(timeout);
                watchdog_stop.store(true, Ordering::SeqCst);
            })
            .ok()?;
        let r = read_block_alloc(&*self.pipe, &geometry, self.sirm.required_payload, &stop);
        // Disarm: a fired watchdog's flag is harmless on a finished read.
        stop.store(true, Ordering::SeqCst);
        match r {
            Ok(block) => Some(Ok(block)),
            Err(U3vBlockError::U3vTimeout) => None,
            Err(U3vBlockError::U3v(e)) => Some(Err(e)),
        }
    }
}

impl Drop for U3vStream {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Internal read outcomes: window expiry vs real errors.
enum U3vBlockError {
    U3v(U3vError),
    U3vTimeout,
}
impl From<U3vError> for U3vBlockError {
    fn from(e: U3vError) -> Self {
        U3vBlockError::U3v(e)
    }
}

/// The memory-sink block walk: leader piece, payload pieces per the
/// programmed geometry, trailer piece (§5.2.1). Same wire algorithm as
/// stream.rs's zero-copy `read_block` — §5.2.1.3's equal-sized transfers,
/// R-127sr (bytes after the first short piece are drained and discarded),
/// §5.5.4 trailer status handling, CR-86st (trailer size_y corrects the
/// leader height) — with a `Vec` payload instead of a DsCore reservation.
/// All framing work delegates to the UQ16 accumulation helpers; all codec
/// work to packets. Every piece uses `in_block = false`: the watchdog is
/// the only stop source here, and it means "the caller's time is up"
/// (unlike the GenTL receiver's graceful-stop grace period).
fn read_block_alloc(
    pipe: &dyn UsbPipe,
    geometry: &TransferGeometry,
    required_payload: u64,
    stop: &AtomicBool,
) -> Result<Block, U3vBlockError> {
    use crate::usbio::{read_piece_capped, read_piece_exact, ReadAbort};
    let mps = usize::from(pipe.max_packet().max(1));

    // -- Leader (single transfer, §5.2.1.2) --------------------------------
    let mut leader_buf =
        vec![0u8; crate::usbio::align_request(geometry.max_leader, pipe.max_packet()) as usize];
    let n = match read_piece_capped(pipe, &mut leader_buf, mps, stop, false) {
        Ok(n) => n as usize,
        Err(ReadAbort::Stop) => return Err(U3vBlockError::U3vTimeout),
        Err(ReadAbort::Io(e)) => return Err(U3vError::Transport(e).into()),
    };
    if n == 0 {
        return Err(U3vError::Transport("zero-length leader transfer".into()).into());
    }
    let mut leader = match packets::decode_image_leader(&leader_buf[..n]) {
        Ok(l) => l,
        Err(e) => {
            // Unsupported (non-image) payloads are drained so the pipe
            // stays in sync; malformed leaders desync the stream.
            use packets::LeaderError;
            match e {
                LeaderError::UnsupportedPayloadType(_) => {
                    match drain_payload_and_trailer(pipe, geometry, stop) {
                        stream::DrainOutcome::Done => {
                            return Ok(Block::Skipped);
                        }
                        stream::DrainOutcome::Stop => {
                            return Err(U3vError::Transport(
                                "stream timed out while draining a block".into(),
                            )
                            .into())
                        }
                        stream::DrainOutcome::Desync => {
                            return Err(U3vError::Transport("drain failed".into()).into());
                        }
                    }
                }
                LeaderError::BadMagic => {
                    return Err(U3vError::Transport("leader magic".into()).into());
                }
                LeaderError::TooShort | LeaderError::BadSize => {
                    return Err(U3vError::Transport("leader size".into()).into());
                }
            }
        }
    };

    // Expected payload: leader-derived size when the pixel format is
    // known, else the SIRM-declared SI Required Payload Size (§5.4.1.3).
    let meta = FrameMeta {
        timestamp: leader.timestamp,
        pixel_format: leader.pixel_format,
        width: leader.size_x,
        height: leader.size_y,
        offset_x: leader.offset_x,
        offset_y: leader.offset_y,
        padding_x: u32::from(leader.padding_x),
        padding_y: 0,
    };
    let expected = meta.payload_bytes().unwrap_or(required_payload as usize);

    // -- Payload sequence (§5.2.1.3) ----------------------------------------
    let mut payload = Vec::with_capacity(expected);
    let mut short_seen = false;
    let mut scratch = vec![0u8; geometry.transfer_size.max(geometry.final1).max(64) as usize];
    let transfers: Vec<u32> = (0..geometry.transfer_count)
        .map(|_| geometry.transfer_size)
        .chain([geometry.final1, geometry.final2])
        .collect();
    for size in transfers {
        if size == 0 {
            continue;
        }
        let request = crate::usbio::align_request(size, pipe.max_packet()) as usize;
        if request > scratch.len() {
            scratch.resize(request, 0);
        }
        let n = match read_piece_exact(pipe, &mut scratch[..request], size as usize, mps, stop) {
            Ok(n) => n as usize,
            Err(ReadAbort::Stop) => {
                return Err(U3vError::Transport("stream timed out during payload".into()).into())
            }
            Err(ReadAbort::Io(e)) => return Err(U3vError::Transport(e).into()),
        };
        if !short_seen {
            let take = n.min(expected.saturating_sub(payload.len()));
            payload.extend_from_slice(&scratch[..take]);
            if take < n {
                short_seen = true;
            }
        }
        // Bytes after the first short piece are irrelevant (R-127sr): they
        // were read above (pipe discipline) and are not copied.
        if n < size as usize {
            short_seen = true;
        }
    }

    // -- Trailer (single transfer, §5.2.1.4) ---------------------------------
    let mut trailer_buf =
        vec![0u8; crate::usbio::align_request(geometry.max_trailer, pipe.max_packet()) as usize];
    let n = match read_piece_capped(pipe, &mut trailer_buf, mps, stop, false) {
        Ok(n) => n as usize,
        Err(ReadAbort::Stop) => {
            return Err(U3vError::Transport("stream timed out during trailer".into()).into())
        }
        Err(ReadAbort::Io(e)) => return Err(U3vError::Transport(e).into()),
    };
    let trailer = match packets::decode_image_trailer(&trailer_buf[..n]) {
        Ok(t) => t,
        Err(_) => return Err(U3vError::Transport("trailer malformed".into()).into()),
    };
    if trailer.block_id != leader.block_id {
        return Err(U3vError::Transport("trailer block id mismatch".into()).into());
    }
    match trailer.status {
        packets::GENCP_SUCCESS => {}
        // §5.5.4: DATA_DISCARDED / DATA_OVERRUN / every reserved code means
        // the block carries no deliverable image; the pipe is in sync.
        _ => return Ok(Block::Skipped),
    }

    // Variable frame size: the trailer's size_y is the actual height
    // (CR-86st); trim the payload to the corrected row count (the pieces
    // carried the programmed — possibly larger — size).
    leader.size_y = trailer.size_y.min(leader.size_y);
    let corrected = FrameMeta {
        height: leader.size_y,
        ..meta
    };
    payload.truncate(corrected.payload_bytes().unwrap_or(payload.len()));
    Ok(Block::Image {
        leader,
        payload,
        trailer,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    struct MidBlockTimeoutPipe {
        state: Mutex<u8>,
        stop: Arc<AtomicBool>,
    }

    impl UsbPipe for MidBlockTimeoutPipe {
        fn read_bulk(&self, buf: &mut [u8], _timeout: Duration) -> GentlResult<u32> {
            let mut state = self.state.lock().unwrap();
            match *state {
                0 => {
                    let leader = packets::ImageLeader {
                        leader_size: packets::IMAGE_LEADER_LEN as u16,
                        block_id: 1,
                        payload_type: packets::PAYLOAD_TYPE_IMAGE,
                        timestamp: 0,
                        pixel_format: gentl_core::pixfmt::PFNC_MONO8,
                        size_x: 1,
                        size_y: 1,
                        offset_x: 0,
                        offset_y: 0,
                        padding_x: 0,
                    };
                    let bytes = packets::encode_image_leader(&leader);
                    buf[..bytes.len()].copy_from_slice(&bytes);
                    *state = 1;
                    Ok(bytes.len() as u32)
                }
                1 => {
                    buf[0] = 0x5A;
                    *state = 2;
                    self.stop.store(true, Ordering::SeqCst);
                    Ok(1)
                }
                _ => Err(GentlError::Timeout),
            }
        }

        fn write_bulk(&self, _buf: &[u8], _timeout: Duration) -> GentlResult<()> {
            Ok(())
        }

        fn flush(&self) {}

        fn set_raw_io(&self) -> GentlResult<()> {
            Ok(())
        }

        fn max_packet(&self) -> u16 {
            1
        }
    }

    #[test]
    fn timeout_after_leader_is_a_transport_error() {
        let stop = Arc::new(AtomicBool::new(false));
        let pipe = MidBlockTimeoutPipe {
            state: Mutex::new(0),
            stop: stop.clone(),
        };
        let geometry = TransferGeometry {
            max_leader: packets::IMAGE_LEADER_LEN as u32,
            transfer_size: 1,
            transfer_count: 1,
            final1: 0,
            final2: 0,
            max_trailer: packets::IMAGE_TRAILER_LEN as u32,
        };

        let result = read_block_alloc(&pipe, &geometry, 1, &stop);
        match result {
            Err(U3vBlockError::U3v(U3vError::Transport(message))) => {
                assert!(message.contains("trailer"));
            }
            _ => panic!("mid-block timeout must not look like an idle timeout"),
        }
    }
}
