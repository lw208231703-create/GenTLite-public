//! Data stream module: stream channel setup over the SIRM (§5.4), the
//! bulk-IN receiver thread reassembling leader/payload/trailer blocks
//! (§5.2/§5.5), and the EVENT_HANDLE bookkeeping behind
//! EVENT_NEW_BUFFER. Reassembly feeds `gentl_core::ds::FrameSink`
//! zero-copy: payload transfers are read straight into the reserved
//! announced buffer; the core finalizes at the trailer.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::thread::JoinHandle;

use gentl_core::ds::{DsCore, FrameError, FrameMeta, FrameSink, FrameTarget};
use gentl_core::error::{GentlError, GentlResult};
use gentl_core::event_queue::EventQueue;
use gentl_core::types::EventType;

use crate::control::GenCpClient;
use crate::cti::{DeviceAccess, U3vDevice};
use crate::packets::{
    self, decode_image_leader, decode_image_trailer, sirm, LeaderError, TrailerError,
    IMAGE_LEADER_LEN, IMAGE_TRAILER_LEN,
};
use crate::usbio::{self, poll_read, ReadAbort, UsbPipe};

/// GenTL stream ID for stream channel `i` (format not mandated by the
/// spec; mirrors the GigE producer's "Stream0" style).
pub fn stream_id(index: u32) -> String {
    format!("Stream{index}")
}

/// Preferred size of one payload transfer (§5.2.1.3 "equal sized
/// blocks"). A 1 KiB multiple and well inside every USB limit — EXCEPT
/// the one found the hard way (vgige bring-up 2026-10-27): a 1 MiB
/// piece makes WinUSB/xHCI halt the bulk-IN endpoint on the very first
/// piece (the device sees EPIPE after the leader); 512 KiB pieces
/// stream indefinitely (500/500 frames verified). The precise
/// driver-level mechanism is an open question (Q39); 512 KiB is the
/// empirical safe value.
const PAYLOAD_TRANSFER: u64 = 1 << 19;

/// SIRM snapshot of one stream channel, taken at `DevOpenDataStream`.
#[derive(Debug, Clone)]
pub struct SirmInfo {
    pub address: u64,
    pub si_info: u32,
    pub required_payload: u64,
    pub required_leader: u32,
    pub required_trailer: u32,
    /// SI Supported Payload Modes (§5.4.1.12), read when the device
    /// reports additional modes; kept for diagnostics.
    #[allow(dead_code)]
    pub supported_modes: u32,
    pub payload_mode: u32,
}

impl SirmInfo {
    /// 2^(bits 24-31 of SI Info) — the alignment all "SI Size" register
    /// values must be a multiple of (§5.4.1.1).
    pub fn alignment(&self) -> u64 {
        1u64 << (self.si_info >> 24)
    }

    #[allow(dead_code)] // used by the GenDC follow-up (UQ8)
    pub fn additional_modes(&self) -> bool {
        self.si_info & 1 != 0
    }
}

/// Receiver diagnostics (not the GenTL STREAM_INFO counters; those live
/// in `DsCore`). Published when the receiver thread exits.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ReceiverStats {
    pub transfers: u64,
    pub io_errors: u64,
    pub malformed_leaders: u64,
    pub malformed_trailers: u64,
    pub trailer_mismatch: u64,
    pub status_data_discarded: u64,
    pub status_data_overrun: u64,
    pub status_other: u64,
    pub unsupported_payload: u64,
    pub blocks_completed: u64,
    pub blocks_delivered: u64,
    pub blocks_dropped_no_buffer: u64,
    pub blocks_dropped_too_small: u64,
    pub blocks_dropped_error: u64,
    pub blocks_lost: u64,
    pub payload_bytes: u64,
}

/// Data stream module behind DS_HANDLE.
pub struct U3vDataStream {
    pub parent: Weak<U3vDevice>,
    pub core: Arc<DsCore>,
    pub channel_index: u32,
    pub sirm: SirmInfo,
    /// This channel's bulk IN pipe (backend-owned endpoint object).
    pipe: Arc<dyn UsbPipe>,
    receiver: Mutex<Option<JoinHandle<()>>>,
    stop_receiver: Arc<AtomicBool>,
    kill_receiver: Arc<AtomicBool>,
    /// Programmed transfer geometry of a running acquisition.
    geometry: Mutex<Option<TransferGeometry>>,
    pub events: Mutex<HashMap<EventType, Arc<EventModule>>>,
    /// Core buffer id -> boxed BUFFER_HANDLE.
    buffer_handles: Mutex<HashMap<u64, usize>>,
    pub receiver_stats: Arc<Mutex<ReceiverStats>>,
}

/// Buffer module behind BUFFER_HANDLE.
pub struct U3vBuffer {
    pub stream: Weak<U3vDataStream>,
    pub id: u64,
}

/// Event module behind EVENT_HANDLE.
pub struct EventModule {
    pub source: Weak<U3vDataStream>,
    pub event_type: EventType,
    pub queue: Arc<EventQueue>,
}

/// The payload transfer program (§5.2.1.3): "equal sized blocks" +
/// final1 + final2. All values are multiples of the SIRM alignment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TransferGeometry {
    pub(crate) max_leader: u32,
    pub(crate) transfer_size: u32,
    pub(crate) transfer_count: u32,
    pub(crate) final1: u32,
    pub(crate) final2: u32,
    pub(crate) max_trailer: u32,
}

/// Read one stream channel's SIRM snapshot (the register-read half of
/// `U3vDataStream::open`, shared with the `client` facade — U3V §5.4.1.1).
pub(crate) fn read_sirm(
    client: &GenCpClient,
    sirm_address: u64,
    channel_index: u32,
    sirm_length: u32,
) -> GentlResult<SirmInfo> {
    let address = sirm_address + channel_index as u64 * u64::from(sirm_length);
    let si_info = client.read_reg(address + sirm::SI_INFO)?;
    let mut payload_b = [0u8; 8];
    client.read_mem(address + sirm::SI_REQUIRED_PAYLOAD_SIZE, &mut payload_b)?;
    let required_payload = u64::from_le_bytes(payload_b);
    let required_leader = client.read_reg(address + sirm::SI_REQUIRED_LEADER_SIZE)?;
    let required_trailer = client.read_reg(address + sirm::SI_REQUIRED_TRAILER_SIZE)?;
    // The two mode registers exist only when the device supports
    // additional payload modes (§5.4.1.1 bit 0).
    let (supported_modes, payload_mode) = if si_info & 1 != 0 {
        (
            client.read_reg(address + sirm::SI_SUPPORTED_PAYLOAD_MODES)?,
            client.read_reg(address + sirm::SI_PAYLOAD_MODE)?,
        )
    } else {
        (sirm::MODE_STANDARD, sirm::MODE_STANDARD)
    };
    Ok(SirmInfo {
        address,
        si_info,
        required_payload,
        required_leader,
        required_trailer,
        supported_modes,
        payload_mode,
    })
}

/// Program the SIRM transfer sizes and raise Stream Enable (§5.4.1.2);
/// verifies the enable latched (SI registers inconsistent is rejected by
/// the device with U3V_STATUS_SI_REGISTERS_INCONSISTENT). Shared by
/// `U3vDataStream::start` and the `client` facade.
pub(crate) fn program_sirm(client: &GenCpClient, sirm: &SirmInfo) -> GentlResult<TransferGeometry> {
    if sirm.payload_mode != sirm::MODE_STANDARD {
        // GenDC container/flow modes are not implemented by this
        // producer; streaming a GenDC-configured channel would
        // mis-deliver payloads.
        return Err(GentlError::NotImplemented(
            "non-standard U3V payload mode (GenDC)",
        ));
    }
    let alignment = sirm.alignment();
    let geometry = plan_geometry(
        sirm.required_payload,
        sirm.required_leader.max(IMAGE_LEADER_LEN as u32),
        sirm.required_trailer.max(IMAGE_TRAILER_LEN as u32),
        alignment,
    );
    let a = sirm.address;
    // Host programs all sizes first; Stream Enable is the last write and
    // is rejected with U3V_STATUS_SI_REGISTERS_INCONSISTENT when the set
    // is illegal (§5.4.1.2).
    client.write_reg(a + sirm::SI_MAX_LEADER_SIZE, geometry.max_leader)?;
    client.write_reg(a + sirm::SI_PAYLOAD_TRANSFER_SIZE, geometry.transfer_size)?;
    client.write_reg(a + sirm::SI_PAYLOAD_TRANSFER_COUNT, geometry.transfer_count)?;
    client.write_reg(a + sirm::SI_PAYLOAD_FINAL1_SIZE, geometry.final1)?;
    client.write_reg(a + sirm::SI_PAYLOAD_FINAL2_SIZE, geometry.final2)?;
    client.write_reg(a + sirm::SI_MAX_TRAILER_SIZE, geometry.max_trailer)?;
    client.write_reg(a + sirm::SI_CONTROL, sirm::CONTROL_STREAM_ENABLE)?;
    let enabled = client.read_reg(a + sirm::SI_CONTROL)?;
    if enabled & sirm::CONTROL_STREAM_ENABLE == 0 {
        disable_sirm(client, sirm);
        return Err(GentlError::Io(
            "Stream Enable did not latch (SI registers inconsistent?)".into(),
        ));
    }
    Ok(geometry)
}

/// Drop Stream Enable (§5.4.1.2: switching to 0 while active aborts the
/// stream). Best effort — used on every teardown path.
pub(crate) fn disable_sirm(client: &GenCpClient, sirm: &SirmInfo) {
    let _ = client.write_reg(sirm.address + sirm::SI_CONTROL, 0);
}

fn align_up(v: u64, alignment: u64) -> u64 {
    if alignment <= 1 {
        v
    } else {
        v.div_ceil(alignment) * alignment
    }
}

/// Program the payload transfer sizes for a required payload (§5.2.1.3
/// Figure 5-2): total = required aligned up; TS = min(512 KiB, total);
/// count = total / TS; final1 = remainder; final2 = 0. All values are
/// multiples of the SIRM alignment. (The spec example additionally
/// assumes a 1 KB/64 KB WinUSB granularity; see notes/OPEN-QUESTIONS.md
/// UQ9 — only the alignment rule is normative.)
fn plan_geometry(
    required_payload: u64,
    required_leader: u32,
    required_trailer: u32,
    alignment: u64,
) -> TransferGeometry {
    let total = align_up(required_payload, alignment);
    let ts_cap = align_up(PAYLOAD_TRANSFER, alignment).max(alignment);
    let transfer_size = ts_cap.min(total.max(alignment)).min(u32::MAX as u64) as u32;
    let (transfer_count, final1) = if transfer_size == 0 {
        (0, 0)
    } else {
        (
            (total / transfer_size as u64) as u32,
            (total % transfer_size as u64) as u32,
        )
    };
    TransferGeometry {
        max_leader: align_up(required_leader as u64, alignment).min(u32::MAX as u64) as u32,
        transfer_size,
        transfer_count,
        final1,
        final2: 0,
        max_trailer: align_up(required_trailer as u64, alignment).min(u32::MAX as u64) as u32,
    }
}

impl U3vDataStream {
    /// `DevOpenDataStream`: read the channel's SIRM and snapshot it. Does
    /// not program anything yet (programming happens per acquisition,
    /// R-72cd: the RW* values are captured at Stream Enable).
    pub fn open(device: &Arc<U3vDevice>, channel_index: u32) -> GentlResult<Arc<Self>> {
        let sirm_address = device
            .bootstrap
            .sirm_address
            .ok_or(GentlError::NotAvailable)?; // no streaming interface
        let pipe = device
            .ifaces
            .stream_in
            .get(channel_index as usize)
            .cloned()
            .ok_or(GentlError::InvalidId)?; // channel without an endpoint

        let sirm_info = {
            let c = device
                .client
                .lock()
                .map_err(|_| GentlError::Io("control client lock poisoned".into()))?;
            read_sirm(
                &c,
                sirm_address,
                channel_index,
                device.bootstrap.sirm_length,
            )?
        };
        let core = Arc::new(DsCore::new());
        core.set_expected_payload_size(sirm_info.required_payload as usize);
        Ok(Arc::new(Self {
            parent: Arc::downgrade(device),
            core,
            channel_index,
            sirm: sirm_info,
            pipe,
            receiver: Mutex::new(None),
            stop_receiver: Arc::new(AtomicBool::new(false)),
            kill_receiver: Arc::new(AtomicBool::new(false)),
            geometry: Mutex::new(None),
            events: Mutex::new(HashMap::new()),
            buffer_handles: Mutex::new(HashMap::new()),
            receiver_stats: Arc::new(Mutex::new(ReceiverStats::default())),
        }))
    }

    pub fn is_running(&self) -> bool {
        self.receiver.lock().expect("receiver lock").is_some()
    }

    /// `DSStartAcquisition` transport half: program the SIRM transfer
    /// sizes, raise Stream Enable (§5.4.1.2), start the receiver thread.
    pub fn start(self: &Arc<Self>) -> GentlResult<()> {
        let device = self.parent.upgrade().ok_or(GentlError::InvalidHandle)?;
        if device.access == DeviceAccess::Monitor {
            // SIRM writes are control-channel writes: a monitor control
            // channel must not stream.
            return Err(GentlError::AccessDenied);
        }
        if self.is_running() {
            return Err(GentlError::ResourceInUse);
        }
        // Apply the backend policy before enabling the device, so a policy
        // failure cannot leave SIRM streaming without a receiver.
        self.pipe.set_raw_io()?;
        let geometry = {
            let c = device
                .client
                .lock()
                .map_err(|_| GentlError::Io("control client lock poisoned".into()))?;
            program_sirm(&c, &self.sirm)?
        };
        *self.geometry.lock().expect("geometry lock") = Some(geometry);
        self.stop_receiver.store(false, Ordering::SeqCst);
        self.kill_receiver.store(false, Ordering::SeqCst);

        let pipe = self.pipe.clone();
        let sink = self.core.clone();
        let stop = self.stop_receiver.clone();
        let kill = self.kill_receiver.clone();
        let stats = self.receiver_stats.clone();
        let name = format!("u3v-recv-{}-{}", device.id, self.channel_index);
        // The receiver thread holds a strong device reference: USB handles
        // must outlive the thread even if the consumer closes the device
        // handle first (audit D19: closing the parent must not free
        // handles a live receiver thread is blocked on). It also holds the
        // stream for fatal-exit engine cleanup (audit C16).
        let device = device.clone();
        let me = self.clone();
        let handle = match std::thread::Builder::new()
            .name(name)
            .spawn(move || receiver_loop(pipe, sink, stop, kill, stats, geometry, me, device))
        {
            Ok(handle) => handle,
            Err(e) => {
                *self.geometry.lock().expect("geometry lock") = None;
                self.pipe.cancel_pending_read();
                if let Some(device) = self.parent.upgrade() {
                    if let Ok(c) = device.client.lock() {
                        disable_sirm(&c, &self.sirm);
                    }
                }
                return Err(GentlError::Io(format!("spawn receiver: {e}")));
            }
        };
        *self.receiver.lock().expect("receiver lock") = Some(handle);
        Ok(())
    }

    /// Receiver-fatal recovery (desync / IO error): force the engine to
    /// the stopped state and wake pending event waiters with an abort, so
    /// `DSGetInfo(IS_GRABBING)` reports reality and consumers do not block
    /// forever on a dead stream (audit C16).
    pub fn on_receiver_fatal(self: &Arc<Self>) {
        self.core.abort_engine();
        self.pipe.cancel_pending_read();
        if let Ok(events) = self.events.lock() {
            for ev in events.values() {
                ev.queue.kill_one();
            }
        }
    }

    /// `DSStopAcquisition` transport half: drop Stream Enable (§5.4.1.2:
    /// switching to 0 while active aborts the stream) and join the
    /// receiver, gracefully or with kill semantics.
    pub fn stop(&self, kill: bool) {
        self.kill_receiver.store(kill, Ordering::SeqCst);
        self.stop_receiver.store(true, Ordering::SeqCst);
        if let Some(h) = self.receiver.lock().expect("receiver lock").take() {
            let _ = h.join();
        }
        // The receiver may have left a timed-out read outstanding; drop it
        // so a later DSStartAcquisition starts from a clean pipe.
        self.pipe.cancel_pending_read();
        *self.geometry.lock().expect("geometry lock") = None;
        if let Some(device) = self.parent.upgrade() {
            if device.access != DeviceAccess::Monitor {
                if let Ok(c) = device.client.lock() {
                    disable_sirm(&c, &self.sirm);
                }
            }
        }
    }

    /// `DSClose` cleanup.
    pub fn close(&self) {
        if self.is_running() {
            self.stop(false);
        }
        for ev in self.events.lock().expect("events lock").values() {
            ev.queue.mark_dead();
        }
        self.core.clear_event_queue();
        let handles: Vec<usize> = self
            .buffer_handles
            .lock()
            .expect("buffer handles lock")
            .values()
            .copied()
            .collect();
        for h in handles {
            unsafe { gentl_core::free_handle(h as *mut core::ffi::c_void) };
        }
        self.buffer_handles
            .lock()
            .expect("buffer handles lock")
            .clear();
    }

    pub fn register_event(
        self: &Arc<Self>,
        event_type: EventType,
    ) -> GentlResult<Arc<EventModule>> {
        let mut map = self.events.lock().expect("events lock");
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

    pub fn unregister_event(&self, event_type: EventType) -> GentlResult<()> {
        let Some(module) = self.events.lock().expect("events lock").remove(&event_type) else {
            return Err(GentlError::NotInitialized);
        };
        module.queue.mark_dead();
        self.core.clear_event_queue();
        self.core.discard_output();
        Ok(())
    }

    pub fn set_buffer_handle(&self, id: u64, handle: *mut core::ffi::c_void) {
        self.buffer_handles
            .lock()
            .expect("buffer handles lock")
            .insert(id, handle as usize);
    }

    pub fn buffer_handle(&self, id: u64) -> Option<*mut core::ffi::c_void> {
        self.buffer_handles
            .lock()
            .expect("buffer handles lock")
            .get(&id)
            .map(|h| *h as *mut core::ffi::c_void)
    }

    pub fn remove_buffer_handle(&self, id: u64) {
        self.buffer_handles
            .lock()
            .expect("buffer handles lock")
            .remove(&id);
    }
}

/// The receiver thread body: read one block after another off the bulk
/// IN pipe and hand complete frames to the acquisition engine.
fn receiver_loop(
    pipe: Arc<dyn UsbPipe>,
    sink: Arc<DsCore>,
    stop: Arc<AtomicBool>,
    kill: Arc<AtomicBool>,
    stats: Arc<Mutex<ReceiverStats>>,
    geometry: TransferGeometry,
    stream: Arc<U3vDataStream>,
    _device: Arc<crate::cti::U3vDevice>,
) {
    // SI Required Payload Size: buffer-size fallback for unknown/custom
    // pixel formats (§5.4.1.3). `sirm` outlives the receiver thread (the
    // thread holds `stream`), so a reference is safe here.
    let required_payload = stream.sirm.required_payload;
    let mut state = ReceiverState::default();
    loop {
        if stop.load(Ordering::SeqCst) {
            // Idle stop: no block in flight to finish.
            state.abandon(&sink, kill.load(Ordering::SeqCst));
            break;
        }
        match read_block(
            &*pipe,
            &sink,
            &mut state,
            &stop,
            &geometry,
            required_payload,
        ) {
            BlockOutcome::Completed => {}
            BlockOutcome::Stopped => {
                state.abandon(&sink, kill.load(Ordering::SeqCst));
                break;
            }
            BlockOutcome::Desync(_why) => {
                // Without a parseable stream the block boundaries are
                // unknowable; the receiver cannot re-synchronize. The
                // specific counter was bumped at the failing site. The
                // engine must reflect the death (audit C16).
                state.abandon(&sink, true);
                stream.on_receiver_fatal();
                break;
            }
        }
    }
    *stats.lock().expect("stats lock") = state.stats;
}

enum BlockOutcome {
    Completed,
    Stopped,
    Desync(&'static str),
}

#[derive(Default)]
struct ReceiverState {
    in_flight: bool,
    /// Highest block id seen; continuity gaps count lost blocks.
    last_block: Option<u64>,
    stats: ReceiverStats,
}

impl ReceiverState {
    fn abandon(&mut self, sink: &Arc<DsCore>, incomplete_to_output: bool) {
        if self.in_flight {
            sink.abort_frame(incomplete_to_output);
            self.in_flight = false;
        }
    }
}

/// Read one data block: leader, payload sequence, trailer (§5.2/§5.5).
fn read_block(
    pipe: &dyn UsbPipe,
    sink: &Arc<DsCore>,
    state: &mut ReceiverState,
    stop: &AtomicBool,
    geometry: &TransferGeometry,
    required_payload: u64,
) -> BlockOutcome {
    // -- Leader (single transfer, §5.2.1.2) ------------------------------
    // Request length must be a multiple of the endpoint MPS (RAW_IO
    // contract on Windows); the piece itself ends on a short packet, so
    // `n` stays the true size.
    let mps = u32::from(pipe.max_packet().max(1));
    let mut leader_buf =
        vec![0u8; usbio::align_request(geometry.max_leader, pipe.max_packet()) as usize];
    let n = match usbio::read_piece_capped(pipe, &mut leader_buf, mps as usize, stop, false) {
        Ok(n) => n as usize,
        Err(ReadAbort::Stop) => return BlockOutcome::Stopped,
        Err(ReadAbort::Io(_)) => {
            state.stats.io_errors += 1;
            return BlockOutcome::Desync("leader read failed");
        }
    };
    if n == 0 {
        // A dropped leader transfer is a device-side error; block
        // boundaries are unknowable from here.
        state.stats.malformed_leaders += 1;
        return BlockOutcome::Desync("zero-length leader transfer");
    }
    let leader = match decode_image_leader(&leader_buf[..n]) {
        Ok(l) => l,
        Err(e) => return leader_failure(pipe, &geometry, state, stop, e),
    };

    // Block continuity (§5.5.4.3: skipped ids mean discarded blocks).
    if let Some(prev) = state.last_block {
        if leader.block_id > prev + 1 {
            state.stats.blocks_lost += leader.block_id - prev - 1;
        }
    }
    state.last_block = Some(leader.block_id);

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
    // Expected payload: the leader-derived size when the pixel format is
    // known, else the SIRM-declared SI Required Payload Size (§5.4.1.3) —
    // the register exists exactly for buffer sizing and lets unknown or
    // custom pixel formats stream instead of being dropped.
    let expected = meta.payload_bytes().unwrap_or(required_payload as usize);

    // -- Reserve the output buffer (zero-copy target) ---------------------
    let target: Option<FrameTarget> = match sink.begin_frame(expected) {
        Ok(t) => Some(t),
        Err(FrameError::QueueEmpty) => {
            state.stats.blocks_dropped_no_buffer += 1;
            None
        }
        Err(FrameError::TooSmall) => {
            state.stats.blocks_dropped_too_small += 1;
            None
        }
        Err(FrameError::NotAcquiring) | Err(FrameError::Busy) => {
            // The engine stopped mid-block (auto-stop race): keep the
            // pipe in sync, deliver nothing.
            state.stats.blocks_dropped_error += 1;
            None
        }
    };
    state.in_flight = target.is_some();

    // -- Payload sequence (§5.2.1.3) --------------------------------------
    let mut written: u64 = 0;
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
        let size = size as u64;
        // Bytes after the first short/ZLP transfer are irrelevant
        // (R-127sr) and land in the scratch buffer.
        // RAW_IO: every request goes through the aligned staging scratch
        // and pieces are accumulated to their exact planned size (the
        // device may split one logical transfer into several short
        // transactions mid-piece — observed in the field, UQ16). Data is
        // then copied into the consumer buffer while the piece still
        // belongs to the frame; after the first short piece everything
        // lands in scratch and is discarded (R-127sr).
        if short_seen {
            // Drain the remaining planned transfers of this block.
            let request = usbio::align_request(size as u32, pipe.max_packet()) as usize;
            if request > scratch.len() {
                scratch.resize(request, 0);
            }
            let n = match usbio::read_piece_exact(
                pipe,
                &mut scratch[..request],
                size as usize,
                mps as usize,
                stop,
            ) {
                Ok(n) => n as u64,
                Err(ReadAbort::Stop) => return BlockOutcome::Stopped,
                Err(ReadAbort::Io(_)) => {
                    state.stats.io_errors += 1;
                    state.abandon(sink, false);
                    return BlockOutcome::Desync("payload drain failed");
                }
            };
            state.stats.transfers += 1;
            let _ = n;
            continue;
        }
        let request = usbio::align_request(size as u32, pipe.max_packet()) as usize;
        if request > scratch.len() {
            scratch.resize(request, 0);
        }
        let n = match usbio::read_piece_exact(
            pipe,
            &mut scratch[..request],
            size as usize,
            mps as usize,
            stop,
        ) {
            Ok(n) => n as u64,
            Err(ReadAbort::Stop) => return BlockOutcome::Stopped,
            Err(ReadAbort::Io(_)) => {
                state.stats.io_errors += 1;
                state.abandon(sink, false);
                return BlockOutcome::Desync("payload read failed");
            }
        };
        state.stats.transfers += 1;
        if let Some(t) = target.as_ref() {
            if written + n <= t.capacity as u64 {
                // SAFETY: exclusive reservation; written+n <= capacity.
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        scratch.as_ptr(),
                        t.base.add(written as usize),
                        n as usize,
                    );
                }
                state.stats.payload_bytes += n;
            } else {
                short_seen = true;
            }
        }
        written += n;
        if n < u64::from(size as u32) {
            short_seen = true;
        }
    }

    // -- Trailer (single transfer, §5.2.1.4) -------------------------------
    let mut trailer_buf =
        vec![0u8; usbio::align_request(geometry.max_trailer, pipe.max_packet()) as usize];
    let n = match usbio::read_piece_capped(pipe, &mut trailer_buf, mps as usize, stop, true) {
        Ok(n) => n as usize,
        Err(ReadAbort::Stop) => return BlockOutcome::Stopped,
        Err(ReadAbort::Io(_)) => {
            state.stats.io_errors += 1;
            state.abandon(sink, false);
            return BlockOutcome::Desync("trailer read failed");
        }
    };
    state.stats.transfers += 1;
    let trailer = match decode_image_trailer(&trailer_buf[..n]) {
        Ok(t) => t,
        Err(TrailerError::BadMagic) => {
            state.abandon(sink, false);
            state.stats.malformed_trailers += 1;
            return BlockOutcome::Desync("trailer magic");
        }
        Err(TrailerError::TooShort) | Err(TrailerError::BadSize) => {
            state.abandon(sink, false);
            state.stats.malformed_trailers += 1;
            return BlockOutcome::Desync("trailer size");
        }
    };
    if trailer.block_id != leader.block_id {
        state.stats.trailer_mismatch += 1;
        state.abandon(sink, false);
        return BlockOutcome::Desync("trailer block id");
    }
    state.stats.blocks_completed += 1;

    match trailer.status {
        packets::GENCP_SUCCESS => {}
        packets::U3V_STATUS_DATA_DISCARDED => {
            state.stats.status_data_discarded += 1;
            state.abandon(sink, false);
            state.stats.blocks_dropped_error += 1;
            return BlockOutcome::Completed;
        }
        packets::U3V_STATUS_DATA_OVERRUN => {
            state.stats.status_data_overrun += 1;
            state.abandon(sink, false);
            state.stats.blocks_dropped_error += 1;
            return BlockOutcome::Completed;
        }
        _ => {
            state.stats.status_other += 1;
            state.abandon(sink, false);
            state.stats.blocks_dropped_error += 1;
            return BlockOutcome::Completed;
        }
    }

    // Variable frame size: the trailer's size_y is the actual height
    // (CR-86st), making payload_bytes() match the valid rows.
    let meta = FrameMeta {
        height: trailer.size_y.min(meta.height),
        ..meta
    };
    let _ = trailer.valid_payload_size; // valid size is implied by meta rows
    if sink.on_frame(leader.block_id, meta, &[]).is_some() {
        state.stats.blocks_delivered += 1;
    }
    state.in_flight = false;
    BlockOutcome::Completed
}

/// Leader decode failed: count it, drain what is drainable (non-image
/// payloads keep the pipe in sync), and decide whether the receiver can
/// go on.
fn leader_failure(
    pipe: &dyn UsbPipe,
    geometry: &TransferGeometry,
    state: &mut ReceiverState,
    stop: &AtomicBool,
    err: LeaderError,
) -> BlockOutcome {
    match err {
        LeaderError::UnsupportedPayloadType(_) => {
            // Not an image payload: drain the block without touching the
            // buffer pools (chunk/GenDC payloads are not implemented).
            state.stats.unsupported_payload += 1;
            match drain_payload_and_trailer(pipe, geometry, stop) {
                DrainOutcome::Done => BlockOutcome::Completed,
                DrainOutcome::Stop => BlockOutcome::Stopped,
                DrainOutcome::Desync => {
                    state.stats.io_errors += 1;
                    BlockOutcome::Desync("drain")
                }
            }
        }
        LeaderError::BadMagic => {
            state.stats.malformed_leaders += 1;
            BlockOutcome::Desync("leader magic")
        }
        LeaderError::TooShort | LeaderError::BadSize => {
            state.stats.malformed_leaders += 1;
            BlockOutcome::Desync("leader size")
        }
    }
}

pub(crate) enum DrainOutcome {
    Done,
    Stop,
    Desync,
}

/// Consume the rest of a block whose payload type we do not deliver:
/// payload transfers per the programmed geometry, then the trailer.
/// Shared by the zero-copy receiver and the `client` facade's
/// library-shaped block reader.
pub(crate) fn drain_payload_and_trailer(
    pipe: &dyn UsbPipe,
    geometry: &TransferGeometry,
    stop: &AtomicBool,
) -> DrainOutcome {
    let mut scratch = vec![0u8; geometry.transfer_size.max(geometry.final1).max(64) as usize];
    let transfers: Vec<u32> = (0..geometry.transfer_count)
        .map(|_| geometry.transfer_size)
        .chain([geometry.final1, geometry.final2])
        .collect();
    for size in transfers {
        if size == 0 {
            continue;
        }
        if (size as usize) > scratch.len() {
            scratch.resize(size as usize, 0);
        }
        match poll_read(pipe, &mut scratch[..size as usize], stop, true) {
            Ok(_) => {}
            Err(ReadAbort::Stop) => return DrainOutcome::Stop,
            Err(ReadAbort::Io(_)) => return DrainOutcome::Desync,
        }
    }
    let mut trailer = vec![0u8; geometry.max_trailer as usize];
    match poll_read(pipe, &mut trailer, stop, true) {
        Ok(_) => DrainOutcome::Done,
        Err(ReadAbort::Stop) => DrainOutcome::Stop,
        Err(ReadAbort::Io(_)) => DrainOutcome::Desync,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn geometry_aligned_and_bounded() {
        let g = plan_geometry(326_680, 52, 32, 1);
        assert_eq!(g.max_leader, 52);
        assert_eq!(g.max_trailer, 32);
        // Smaller than the 1 MiB cap: one equal-sized transfer.
        assert_eq!(g.transfer_size, 326_680);
        assert_eq!(g.transfer_count, 1);
        assert_eq!(g.final1, 0);
        assert_eq!(g.final2, 0);

        let g = plan_geometry(3 << 20, 52, 32, 1);
        assert_eq!(g.transfer_size, 1 << 19); // the empirical cap (Q39)
        assert_eq!(g.transfer_count, 6);
        assert_eq!(g.final1, 0);

        // Alignment 4: every programmed value is a multiple of 4.
        let g = plan_geometry(1000, 52, 32, 4);
        assert_eq!(g.transfer_size % 4, 0);
        assert_eq!(g.final1 % 4, 0);
        assert_eq!(g.max_leader % 4, 0);
        assert_eq!(g.max_trailer % 4, 0);
        let total =
            g.transfer_size as u64 * g.transfer_count as u64 + g.final1 as u64 + g.final2 as u64;
        assert!(total >= 1000);

        // Alignment larger than the 1 MiB cap: TS falls back to the
        // alignment itself.
        let g = plan_geometry(1 << 22, 64, 32, 1 << 22);
        assert_eq!(g.transfer_size as u64, 1 << 22);
        assert_eq!(g.transfer_count, 1);
        assert_eq!(g.final1, 0);
    }

    #[test]
    fn alignment_from_si_info() {
        let s = SirmInfo {
            address: 0,
            si_info: 2 << 24,
            required_payload: 0,
            required_leader: 0,
            required_trailer: 0,
            supported_modes: 1,
            payload_mode: 1,
        };
        assert_eq!(s.alignment(), 4);
        assert!(!s.additional_modes());
    }
}
