//! Protocol-independent data stream core: the announced buffer pool, the
//! input pool / output queue bookkeeping, the acquisition counting state
//! machine, and the `EVENT_NEW_BUFFER` event queue.
//!
//! Transports feed frames in through [`FrameSink`]; the GenTL boundary
//! (gentl-gige) maps core buffer ids to module handles. Semantics follow
//! GenTL 1.6 §5 (acquisition engine) and §6.3.5 (data stream functions).
//!
//! Buffer lifecycle (all states mutually exclusive, one `Mutex`):
//! `Announced` -> `Queued` (input pool) -> `Filling` (in use by the engine)
//! -> `Filled` (output queue) -> delivered by `EventGetData` back to
//! `Announced`.

use std::collections::VecDeque;
use std::ffi::c_void;
use std::sync::{Arc, Mutex, MutexGuard};

use crate::event_queue::{Entry, EventQueue};
use crate::pixfmt;
use crate::{GentlError, GentlResult};

/// `GENTL_INFINITE` (GenTL.h): acquire until `DSStopAcquisition`.
pub const GENTL_INFINITE: u64 = u64::MAX;

/// Metadata of one completed frame, taken from the transport's frame
/// description (the GVSP image leader for GigE Vision).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FrameMeta {
    pub timestamp: u64,
    pub pixel_format: u32,
    pub width: u32,
    pub height: u32,
    pub offset_x: u32,
    pub offset_y: u32,
    pub padding_x: u32,
    pub padding_y: u32,
}

impl FrameMeta {
    /// Expected payload bytes, padding included (the GigE Vision image
    /// payload spans the padded rows).
    pub fn payload_bytes(&self) -> Option<usize> {
        let stride = pixfmt::payload_size(self.pixel_format, self.width, 1)?;
        let rows = stride
            .checked_add(self.padding_x as usize)?
            .checked_mul(self.height as usize)?;
        let total = rows.checked_add(self.padding_y as usize)?;
        Some(total)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BufferState {
    /// Announced only, or already delivered to the consumer. In no pool.
    Announced,
    /// In the input buffer pool, waiting to be filled.
    Queued,
    /// Removed from the input pool, currently being filled.
    Filling,
    /// In the output buffer queue, waiting for delivery.
    Filled,
}

impl BufferState {
    /// `BUFFER_INFO_IS_QUEUED`: input pool or output queue.
    pub fn is_queued_info(self) -> bool {
        matches!(self, BufferState::Queued | BufferState::Filled)
    }
}

/// Backing storage of an announced buffer.
enum Storage {
    /// Memory owned by the GenTL consumer (`DSAnnounceBuffer`).
    External(usize),
    /// Memory allocated by the producer (`DSAllocAndAnnounceBuffer`);
    /// freed when the buffer is revoked or the stream closes.
    Owned(Vec<u8>),
}

impl Storage {
    fn base(&self) -> usize {
        match self {
            Storage::External(p) => *p,
            Storage::Owned(v) => v.as_ptr() as usize,
        }
    }

    fn is_owned(&self) -> bool {
        matches!(self, Storage::Owned(_))
    }
}

/// One announced buffer and its engine bookkeeping.
pub struct DsBuffer {
    pub id: u64,
    /// `pPrivate` from the announcement call; handed back in
    /// `EVENT_NEW_BUFFER_DATA::pUserPointer`.
    user_ptr: usize,
    /// Total size in bytes.
    size: usize,
    pub state: BufferState,
    /// `BUFFER_INFO_NEW_DATA`: filled since the last `DSQueueBuffer`.
    pub new_data: bool,
    /// `BUFFER_INFO_SIZE_FILLED`.
    pub filled_size: usize,
    /// `BUFFER_INFO_IS_INCOMPLETE`.
    pub incomplete: bool,
    /// Frame metadata of the last fill, if any.
    pub meta: Option<FrameMeta>,
    /// `BUFFER_INFO_FRAMEID`: transport block id of the last fill.
    pub block_id: u64,
    storage: Storage,
}

impl DsBuffer {
    /// `BUFFER_INFO_BASE`: current base address of the buffer memory.
    pub fn base_ptr(&self) -> *mut c_void {
        self.storage.base() as *mut c_void
    }

    /// `BUFFER_INFO_USER_PTR`.
    pub fn user_ptr(&self) -> *mut c_void {
        self.user_ptr as *mut c_void
    }

    /// `BUFFER_INFO_SIZE`.
    pub fn size(&self) -> usize {
        self.size
    }

    /// Whether the producer allocated the memory (`DSAllocAndAnnounceBuffer`).
    pub fn is_owned(&self) -> bool {
        self.storage.is_owned()
    }

    /// Back to the input pool: `BUFFER_INFO_SIZE_FILLED` resets on
    /// `DSQueueBuffer` (GenTL.h), and NEW_DATA marks the last fill.
    fn reset_for_queue(&mut self) {
        self.state = BufferState::Queued;
        self.filled_size = 0;
        self.new_data = false;
    }
}

/// `DSFlushQueue` operation (ACQ_QUEUE_TYPE, GenTL.h).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcqQueueOp {
    /// All input pool buffers move to the output queue; event entries are
    /// added so every buffer is still delivered through the event.
    InputToOutput,
    /// Output queue buffers are discarded; their event entries removed.
    OutputDiscard,
    /// Every buffer returns to the input pool (in-flight untouched); event
    /// entries discarded.
    AllToInput,
    /// Buffers in no pool return to the input pool.
    UnqueuedToInput,
    /// Everything leaves the pools; no buffer bound to the engine; event
    /// entries discarded.
    AllDiscard,
}

/// Why [`FrameSink::begin_frame`] could not reserve a buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameError {
    /// The acquisition engine is not grabbing.
    NotAcquiring,
    /// Grabbing but the input pool is empty (queue underrun).
    QueueEmpty,
    /// Head-of-queue buffer is smaller than the expected payload.
    TooSmall,
    /// A frame is already in flight (transport protocol violation).
    Busy,
}

/// Exclusive write reservation over one buffer, handed to the transport
/// between `begin_frame` and `on_frame`/`abort_frame` (the zero-copy path
/// writes payload data straight into it).
#[derive(Debug, PartialEq, Eq)]
pub struct FrameTarget {
    pub buffer_id: u64,
    pub base: *mut u8,
    pub capacity: usize,
}

// The pointer addresses memory owned by the buffer pool; the pool keeps the
// buffer in `Filling` state (exclusively reserved) until the paired
// `on_frame`/`abort_frame` call, and buffer memory outlives the reservation.
unsafe impl Send for FrameTarget {}

/// Transport-facing intake of the acquisition engine. Implemented by
/// [`DsCore`]; the GVSP receiver calls it once per frame block.
///
/// `data` in `on_frame` is empty when the transport already wrote the frame
/// into the reserved buffer via [`FrameTarget`] (zero-copy); otherwise core
/// copies `data` into the reserved buffer.
pub trait FrameSink: Send + Sync {
    /// Reserve the head-of-queue buffer for a frame of `payload_size`
    /// bytes. Moves it from the input pool to `Filling`.
    fn begin_frame(&self, payload_size: usize) -> Result<FrameTarget, FrameError>;

    /// Complete the in-flight frame (trailer received): copy `data` if
    /// non-empty, mark the buffer filled, move it to the output queue and
    /// push an `EVENT_NEW_BUFFER` entry. Returns the delivered buffer id,
    /// or `None` if the frame could not be kept.
    fn on_frame(&self, block_id: u64, meta: FrameMeta, data: &[u8]) -> Option<u64>;

    /// Discard the in-flight frame (gaps, unsupported, stop). With
    /// `incomplete_to_output` the partially filled buffer still goes to the
    /// output queue flagged `BUFFER_INFO_IS_INCOMPLETE` (ACQ_STOP_FLAGS_KILL
    /// semantics); otherwise it returns to the input pool.
    fn abort_frame(&self, incomplete_to_output: bool);
}

/// What [`DsCore::revoke`] hands back to the GenTL boundary.
#[derive(Debug, PartialEq, Eq)]
pub struct RevokedBuffer {
    /// Consumer memory base for `DSAnnounceBuffer` buffers; `None` for
    /// producer-allocated (`DSRevokeBuffer` writes NULL).
    pub base: Option<usize>,
    pub user_ptr: usize,
}

#[derive(Default)]
struct Pool {
    /// Announcement order; the index space of `DSGetBufferID`. Revoking a
    /// buffer shifts the following ones down (GenTL 1.6 §6.3.5.6).
    buffers: Vec<DsBuffer>,
    /// Input pool, FIFO fill order.
    input: VecDeque<u64>,
    /// Output queue, FIFO delivery order.
    output: VecDeque<u64>,
    /// Buffer reserved by `begin_frame`, if any.
    in_flight: Option<u64>,
    next_id: u64,
}

impl Pool {
    fn find(&self, id: u64) -> Option<&DsBuffer> {
        self.buffers.iter().find(|b| b.id == id)
    }

    fn find_mut(&mut self, id: u64) -> Option<&mut DsBuffer> {
        self.buffers.iter_mut().find(|b| b.id == id)
    }

    fn find_external_base(&self, base: usize) -> Option<u64> {
        self.buffers
            .iter()
            .find(|b| !b.is_owned() && b.storage.base() == base)
            .map(|b| b.id)
    }

    fn push(&mut self, mut b: DsBuffer) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        b.id = id;
        self.buffers.push(b);
        id
    }

    /// Move all buffers in state `from` to state `to`; returns the ids.
    /// For `Queued` the input queue drives the walk; for `Filled` the
    /// output queue is emptied as well.
    fn drain_matching(&mut self, from: BufferState, to: BufferState) -> Vec<u64> {
        let mut moved = Vec::new();
        if from == BufferState::Queued {
            let drained: Vec<u64> = self.input.iter().copied().collect();
            self.input.clear();
            for id in drained {
                if let Some(b) = self.find_mut(id) {
                    if b.state == from {
                        b.state = to;
                        moved.push(id);
                    } else {
                        // Filling (in-flight): not in the input pool anymore.
                        self.input.push_back(id);
                    }
                }
            }
            return moved;
        }
        for b in &mut self.buffers {
            if b.state == from {
                b.state = to;
                moved.push(b.id);
            }
        }
        if from == BufferState::Filled {
            self.output.clear();
        }
        moved
    }
}

#[derive(Default)]
struct AcqState {
    /// `DSStartAcquisition` seen, `DSStopAcquisition` not yet (the stop
    /// call is required even after an auto-stop, GenTL 1.6 §6.3.5.12).
    started: bool,
    /// Buffers may currently be taken from the input pool.
    grabbing: bool,
    /// Remaining frames before auto-stop; `GENTL_INFINITE` = forever.
    num_to_acquire: u64,
    /// Filled/delivered since the last start (STREAM_INFO_NUM_DELIVERED).
    delivered: u64,
    /// Buffers taken into the engine (STREAM_INFO_NUM_STARTED).
    started_buffers: u64,
    /// Frames lost because the input pool was empty.
    underruns: u64,
}

/// The protocol-independent heart of a data stream. Shared between the
/// GenTL boundary thread(s) and the transport receiver thread.
pub struct DsCore {
    pool: Mutex<Pool>,
    acq: Mutex<AcqState>,
    /// Event queue of the currently registered `EVENT_NEW_BUFFER` object.
    /// `None` while no event is registered: signals are discarded.
    event_queue: Mutex<Option<Arc<EventQueue>>>,
    /// Expected payload size in bytes; the receiver records the last
    /// frame's expected size here (STREAM_INFO_PAYLOAD_SIZE).
    expected_payload: Mutex<usize>,
}

impl Default for DsCore {
    fn default() -> Self {
        Self::new()
    }
}

impl DsCore {
    pub fn new() -> Self {
        Self {
            pool: Mutex::new(Pool::default()),
            acq: Mutex::new(AcqState::default()),
            event_queue: Mutex::new(None),
            expected_payload: Mutex::new(0),
        }
    }

    fn lock_pool(&self) -> MutexGuard<'_, Pool> {
        self.pool.lock().unwrap()
    }

    fn push_event(&self, buffer_id: u64) {
        if let Some(q) = &*self.event_queue.lock().unwrap() {
            q.push(Entry {
                id: buffer_id,
                payload: Vec::new(),
            });
        }
    }

    fn blank(user_ptr: usize, size: usize, storage: Storage) -> DsBuffer {
        DsBuffer {
            id: 0, // assigned by Pool::push
            user_ptr,
            size,
            state: BufferState::Announced,
            new_data: false,
            filled_size: 0,
            incomplete: false,
            meta: None,
            block_id: 0,
            storage,
        }
    }

    // -- expected payload -------------------------------------------------

    pub fn set_expected_payload_size(&self, size: usize) {
        *self.expected_payload.lock().unwrap() = size;
    }

    pub fn expected_payload_size(&self) -> usize {
        *self.expected_payload.lock().unwrap()
    }

    // -- event registration (called by the GenTL boundary) ----------------

    pub fn set_event_queue(&self, queue: Arc<EventQueue>) {
        *self.event_queue.lock().unwrap() = Some(queue);
    }

    pub fn clear_event_queue(&self) {
        *self.event_queue.lock().unwrap() = None;
    }

    // -- buffer announcement ----------------------------------------------

    /// `DSAnnounceBuffer`: register consumer-allocated memory.
    ///
    /// # Safety
    /// `base` must point to `size` writable bytes staying valid until the
    /// buffer is revoked (the GenTL consumer contract).
    pub fn announce_external(
        &self,
        base: *mut c_void,
        size: usize,
        user_ptr: *mut c_void,
    ) -> GentlResult<u64> {
        if base.is_null() || size == 0 {
            return Err(GentlError::InvalidParameter("pBuffer/iSize"));
        }
        let mut p = self.lock_pool();
        if p.find_external_base(base as usize).is_some() {
            // A buffer can only be announced once to a given stream.
            return Err(GentlError::ResourceInUse);
        }
        Ok(p.push(Self::blank(
            user_ptr as usize,
            size,
            Storage::External(base as usize),
        )))
    }

    /// `DSAllocAndAnnounceBuffer`: allocate and register producer memory.
    pub fn announce_owned(&self, size: usize, user_ptr: *mut c_void) -> GentlResult<u64> {
        if size == 0 {
            return Err(GentlError::InvalidParameter("iSize"));
        }
        let mut p = self.lock_pool();
        Ok(p.push(Self::blank(
            user_ptr as usize,
            size,
            Storage::Owned(vec![0u8; size]),
        )))
    }

    // -- buffer revocation / lookup ----------------------------------------

    /// `DSRevokeBuffer`; only allowed while the buffer is in no pool.
    pub fn revoke(&self, id: u64) -> GentlResult<RevokedBuffer> {
        let mut p = self.lock_pool();
        let Some(pos) = p.buffers.iter().position(|b| b.id == id) else {
            return Err(GentlError::InvalidHandle);
        };
        if p.buffers[pos].state != BufferState::Announced {
            return Err(GentlError::Busy); // queued, filling, or in the output queue
        }
        let b = p.buffers.remove(pos);
        Ok(RevokedBuffer {
            base: (!b.is_owned()).then_some(b.storage.base()),
            user_ptr: b.user_ptr,
        })
    }

    /// `DSGetBufferID`: buffer id at the announcement-order index.
    pub fn buffer_id_at(&self, index: usize) -> Option<u64> {
        self.lock_pool().buffers.get(index).map(|b| b.id)
    }

    /// Read access to one buffer's bookkeeping.
    pub fn with_buffer<R>(&self, id: u64, f: impl FnOnce(&DsBuffer) -> R) -> Option<R> {
        self.lock_pool().find(id).map(f)
    }

    pub fn announced_count(&self) -> usize {
        self.lock_pool().buffers.len()
    }

    pub fn queued_count(&self) -> usize {
        self.lock_pool().input.len()
    }

    pub fn await_delivery_count(&self) -> usize {
        self.lock_pool().output.len()
    }

    /// `DSQueueBuffer`: place an announced/delivered buffer into the input
    /// pool.
    pub fn queue_buffer(&self, id: u64) -> GentlResult<()> {
        let mut p = self.lock_pool();
        let Some(b) = p.find_mut(id) else {
            return Err(GentlError::InvalidHandle);
        };
        if b.state != BufferState::Announced {
            // Already queued/filling/filled: "the buffer is already queued".
            return Err(GentlError::ResourceInUse);
        }
        b.reset_for_queue();
        p.input.push_back(id);
        Ok(())
    }

    // -- DSFlushQueue --------------------------------------------------------

    pub fn flush(&self, op: AcqQueueOp) -> GentlResult<()> {
        let mut p = self.lock_pool();
        match op {
            AcqQueueOp::InputToOutput => {
                let moved = p.drain_matching(BufferState::Queued, BufferState::Filled);
                for id in &moved {
                    p.output.push_back(*id);
                }
                drop(p);
                // "if necessary adds entries in the New Buffer event data
                // queue": every moved buffer is still delivered via the event.
                for id in moved {
                    self.push_event(id);
                }
            }
            AcqQueueOp::OutputDiscard => {
                let moved = p.drain_matching(BufferState::Filled, BufferState::Announced);
                drop(p);
                if let Some(q) = &*self.event_queue.lock().unwrap() {
                    q.retain(|e| !moved.contains(&e.id));
                }
            }
            AcqQueueOp::AllToInput => {
                let from_output = p.drain_matching(BufferState::Filled, BufferState::Queued);
                let from_announced = p.drain_matching(BufferState::Announced, BufferState::Queued);
                p.input.extend(from_output);
                p.input.extend(from_announced);
                drop(p);
                if let Some(q) = &*self.event_queue.lock().unwrap() {
                    q.flush();
                }
            }
            AcqQueueOp::UnqueuedToInput => {
                let moved = p.drain_matching(BufferState::Announced, BufferState::Queued);
                p.input.extend(moved);
            }
            AcqQueueOp::AllDiscard => {
                p.drain_matching(BufferState::Queued, BufferState::Announced);
                p.drain_matching(BufferState::Filled, BufferState::Announced);
                p.input.clear();
                p.output.clear();
                drop(p);
                if let Some(q) = &*self.event_queue.lock().unwrap() {
                    q.flush();
                }
            }
        }
        Ok(())
    }

    // -- acquisition state machine -------------------------------------------

    /// `DSStartAcquisition`. `num_to_acquire == 0` is rejected; the caller
    /// (GenTL boundary) opens the transport channel after this succeeds.
    pub fn start_acquisition(&self, num_to_acquire: u64) -> GentlResult<()> {
        if num_to_acquire == 0 {
            return Err(GentlError::InvalidParameter("iNumToAcquire"));
        }
        let mut a = self.acq.lock().unwrap();
        if a.started {
            return Err(GentlError::ResourceInUse);
        }
        a.num_to_acquire = num_to_acquire;
        a.delivered = 0;
        a.started = true;
        a.grabbing = true;
        Ok(())
    }

    /// `DSStopAcquisition`: logically stops the engine. The caller stops
    /// the transport afterwards; the in-flight frame either completes
    /// (default flags) or is aborted incomplete (kill flags).
    pub fn stop_acquisition(&self) -> GentlResult<()> {
        let mut a = self.acq.lock().unwrap();
        if !a.started {
            // "The Acquisition has already been terminated or it has not
            // been started" (GenTL 1.6 §6.3.5.12).
            return Err(GentlError::ResourceInUse);
        }
        a.started = false;
        a.grabbing = false;
        Ok(())
    }

    pub fn is_grabbing(&self) -> bool {
        self.acq.lock().unwrap().grabbing
    }

    /// Receiver-fatal path (transport died: IO error / desync): force the
    /// engine into the stopped state so `DSGetInfo(IS_GRABBING)` reports
    /// reality and a later `DSStopAcquisition` observes "already
    /// terminated" (`GenTL 1.6 §6.3.5.12`). The transport layer is
    /// responsible for additionally aborting event waiters.
    pub fn abort_engine(&self) {
        let mut a = self.acq.lock().unwrap();
        if a.started {
            a.started = false;
            a.grabbing = false;
        }
    }

    pub fn num_delivered(&self) -> u64 {
        self.acq.lock().unwrap().delivered
    }

    pub fn num_started_buffers(&self) -> u64 {
        self.acq.lock().unwrap().started_buffers
    }

    pub fn num_underrun(&self) -> u64 {
        self.acq.lock().unwrap().underruns
    }

    /// Record one delivered frame and apply `iNumToAcquire` auto-stop.
    fn record_delivery(&self) {
        let mut a = self.acq.lock().unwrap();
        a.delivered += 1;
        if a.num_to_acquire != GENTL_INFINITE && a.delivered >= a.num_to_acquire {
            a.grabbing = false; // engine stops automatically; the stop call is still required
        }
    }

    /// Successful `EventGetData` retrieval: the buffer leaves the output
    /// queue without being requeued (GenTL 1.6 §5.1.3).
    pub fn mark_delivered(&self, id: u64) -> GentlResult<()> {
        let mut p = self.lock_pool();
        let Some(b) = p.find_mut(id) else {
            return Err(GentlError::InvalidHandle);
        };
        if b.state != BufferState::Filled {
            return Err(GentlError::InvalidHandle);
        }
        b.state = BufferState::Announced;
        p.output.retain(|&x| x != id);
        Ok(())
    }

    /// `EventFlush` / `GCUnregisterEvent`: discard the output queue and the
    /// pending event entries; input pool and in-flight buffers untouched.
    pub fn discard_output(&self) {
        let mut p = self.lock_pool();
        p.drain_matching(BufferState::Filled, BufferState::Announced);
        p.output.clear();
        drop(p);
        if let Some(q) = &*self.event_queue.lock().unwrap() {
            q.flush();
        }
    }
}

impl FrameSink for DsCore {
    fn begin_frame(&self, payload_size: usize) -> Result<FrameTarget, FrameError> {
        if !self.is_grabbing() {
            return Err(FrameError::NotAcquiring);
        }
        let mut p = self.lock_pool();
        if p.in_flight.is_some() {
            return Err(FrameError::Busy);
        }
        let Some(&id) = p.input.front() else {
            self.acq.lock().unwrap().underruns += 1;
            return Err(FrameError::QueueEmpty);
        };
        let size = p.find(id).expect("input pool id").size();
        if size < payload_size {
            // Leave the too-small buffer at the head; every frame of this
            // configuration would overflow it.
            return Err(FrameError::TooSmall);
        }
        p.input.pop_front();
        let (base, size) = {
            let b = p.find_mut(id).expect("input pool id");
            b.state = BufferState::Filling;
            b.incomplete = false;
            (b.base_ptr() as *mut u8, b.size())
        };
        p.in_flight = Some(id);
        self.acq.lock().unwrap().started_buffers += 1;
        Ok(FrameTarget {
            buffer_id: id,
            base,
            capacity: size,
        })
    }

    fn on_frame(&self, block_id: u64, meta: FrameMeta, data: &[u8]) -> Option<u64> {
        let mut p = self.lock_pool();
        let id = p.in_flight.take()?;
        let b = p.find_mut(id)?;
        if !data.is_empty() {
            if data.len() > b.size() {
                // Oversized frame: keep the buffer, requeue, drop the frame.
                b.state = BufferState::Queued;
                p.input.push_back(id);
                return None;
            }
            // SAFETY: the buffer is exclusively reserved (Filling); the
            // copy stays within `size`.
            unsafe {
                std::ptr::copy_nonoverlapping(data.as_ptr(), b.base_ptr() as *mut u8, data.len());
            }
            b.filled_size = data.len();
        } else {
            // Zero-copy path: the transport wrote the payload; the byte
            // count is defined by the frame metadata.
            b.filled_size = meta.payload_bytes().unwrap_or(0).min(b.size());
        }
        b.state = BufferState::Filled;
        b.new_data = true;
        b.meta = Some(meta);
        b.block_id = block_id;
        p.output.push_back(id);
        drop(p);
        self.record_delivery();
        self.push_event(id);
        Some(id)
    }

    fn abort_frame(&self, incomplete_to_output: bool) {
        let mut p = self.lock_pool();
        let Some(id) = p.in_flight.take() else {
            return;
        };
        let b = p.find_mut(id).expect("in-flight id");
        if incomplete_to_output {
            b.state = BufferState::Filled;
            b.new_data = true;
            b.incomplete = true;
            p.output.push_back(id);
            drop(p);
            self.push_event(id);
        } else {
            b.state = BufferState::Queued;
            p.input.push_back(id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event_queue::WaitResult;
    use std::time::Duration;

    const SHORT: Duration = Duration::from_millis(50);

    fn meta() -> FrameMeta {
        FrameMeta {
            timestamp: 123,
            pixel_format: crate::pixfmt::PFNC_MONO8,
            width: 4,
            height: 3,
            ..FrameMeta::default()
        }
    }

    /// Announce a real allocation (leaked on purpose; tests are short-lived).
    fn announce(core: &DsCore, size: usize) -> (u64, *mut c_void) {
        let v: Vec<u8> = vec![0; size];
        let p = v.leak() as *mut [u8] as *mut c_void;
        let id = core
            .announce_external(p, size, 0xABCDusize as *mut c_void)
            .unwrap();
        (id, p)
    }

    #[test]
    fn announce_queue_revoke_cycle() {
        let core = DsCore::new();
        let (b0, p0) = announce(&core, 64);
        let (b1, _) = announce(&core, 64);
        assert_eq!(core.announced_count(), 2);
        assert_eq!(core.buffer_id_at(0), Some(b0));
        assert_eq!(core.buffer_id_at(1), Some(b1));
        assert_eq!(core.buffer_id_at(2), None);

        // Queue both; double queue is RESOURCE_IN_USE.
        core.queue_buffer(b0).unwrap();
        assert_eq!(core.queue_buffer(b0), Err(GentlError::ResourceInUse));
        core.queue_buffer(b1).unwrap();
        assert_eq!(core.queued_count(), 2);

        // Queued buffer cannot be revoked.
        assert_eq!(core.revoke(b0), Err(GentlError::Busy));

        // Flush everything out of the pools, then revoke.
        core.flush(AcqQueueOp::AllDiscard).unwrap();
        let r = core.revoke(b0).unwrap();
        assert_eq!(r.user_ptr, 0xABCD);
        assert_eq!(r.base, Some(p0 as usize));
        // Announcement-order index space stays dense after a revoke.
        assert_eq!(core.buffer_id_at(0), Some(b1));
    }

    #[test]
    fn announce_validation() {
        let core = DsCore::new();
        assert!(matches!(
            core.announce_external(std::ptr::null_mut(), 16, std::ptr::null_mut()),
            Err(GentlError::InvalidParameter(_))
        ));
        assert!(matches!(
            core.announce_external(1usize as *mut c_void, 0, std::ptr::null_mut()),
            Err(GentlError::InvalidParameter(_))
        ));
        assert!(matches!(
            core.announce_owned(0, std::ptr::null_mut()),
            Err(GentlError::InvalidParameter(_))
        ));
        // Unknown id.
        assert!(matches!(core.revoke(999), Err(GentlError::InvalidHandle)));
        assert!(matches!(core.queue_buffer(999), Err(GentlError::InvalidHandle)));
    }

    #[test]
    fn announce_owned_revoke_returns_no_base() {
        let core = DsCore::new();
        let id = core.announce_owned(32, std::ptr::null_mut()).unwrap();
        let r = core.revoke(id).unwrap();
        assert!(r.base.is_none(), "producer-allocated: NULL base out");
        assert_eq!(r.user_ptr, 0);
    }

    #[test]
    fn double_announce_same_pointer_rejected() {
        let core = DsCore::new();
        let (id, p) = announce(&core, 16);
        assert_eq!(
            core.announce_external(p, 16, std::ptr::null_mut()),
            Err(GentlError::ResourceInUse)
        );
        // Same pointer with a different size is still the same buffer.
        assert_eq!(
            core.announce_external(p, 8, std::ptr::null_mut()),
            Err(GentlError::ResourceInUse)
        );
        assert!(core.with_buffer(id, |b| !b.is_owned()).unwrap());
    }

    #[test]
    fn frame_copy_path_fills_and_events() {
        let core = DsCore::new();
        let (b0, _) = announce(&core, 64);
        let q = Arc::new(EventQueue::new());
        core.set_event_queue(q.clone());
        core.queue_buffer(b0).unwrap();
        core.start_acquisition(GENTL_INFINITE).unwrap();

        let data = [0x5Au8; 12];
        let t = core.begin_frame(12).expect("begin");
        assert_eq!(t.buffer_id, b0);
        let id = core.on_frame(7, meta(), &data).expect("delivered");
        assert_eq!(id, b0);
        assert_eq!(core.num_delivered(), 1);
        assert_eq!(core.await_delivery_count(), 1);
        assert_eq!(core.queued_count(), 0);
        assert_eq!(core.num_started_buffers(), 1);

        core.with_buffer(b0, |b| {
            assert_eq!(b.state, BufferState::Filled);
            assert!(b.new_data);
            assert_eq!(b.filled_size, 12);
            assert_eq!(b.meta.unwrap().timestamp, 123);
            assert_eq!(b.block_id, 7);
            assert_eq!(b.user_ptr(), 0xABCDusize as *mut c_void);
            let mem = unsafe { std::slice::from_raw_parts(b.base_ptr() as *const u8, 12) };
            assert_eq!(mem, &data);
        }).unwrap();

        // Event entry pushed, correlated with the buffer id.
        match q.wait(SHORT) {
            WaitResult::Entry(e) => assert_eq!(e.id, b0),
            other => panic!("expected event entry, got {other:?}"),
        }
    }

    #[test]
    fn zero_copy_path_sets_filled_size_from_meta() {
        let core = DsCore::new();
        let (b0, _) = announce(&core, 64);
        core.queue_buffer(b0).unwrap();
        core.start_acquisition(GENTL_INFINITE).unwrap();
        let t = core.begin_frame(12).expect("begin");
        assert_eq!(t.buffer_id, b0);
        assert_eq!(t.capacity, 64);
        // Zero-copy writes go through the target pointer.
        unsafe {
            std::ptr::write_bytes(t.base, 0x11, 12);
        }
        core.on_frame(1, meta(), &[]);
        core.with_buffer(b0, |b| {
            assert_eq!(b.filled_size, 12, "4x3 Mono8 = 12 bytes");
            assert!(b.new_data);
            let mem = unsafe { std::slice::from_raw_parts(b.base_ptr() as *const u8, 12) };
            assert!(mem.iter().all(|&x| x == 0x11));
        }).unwrap();
    }

    #[test]
    fn begin_frame_errors_and_underrun() {
        let core = DsCore::new();
        // Not acquiring.
        assert_eq!(core.begin_frame(4), Err(FrameError::NotAcquiring));
        core.start_acquisition(1).unwrap();
        // Queue empty.
        assert_eq!(core.begin_frame(4), Err(FrameError::QueueEmpty));
        assert_eq!(core.num_underrun(), 1);

        let (small, _) = announce(&core, 4);
        core.queue_buffer(small).unwrap();
        let (big, _) = announce(&core, 64);
        core.queue_buffer(big).unwrap();
        assert_eq!(core.begin_frame(8), Err(FrameError::TooSmall));
        assert_eq!(core.num_underrun(), 1, "too-small is not an underrun");

        // Take the big buffer out of the pool first (FIFO behind the small
        // head means TooSmall again; drop the small one and requeue big).
        core.flush(AcqQueueOp::AllDiscard).unwrap();
        core.revoke(small).unwrap();
        core.queue_buffer(big).unwrap();
        assert!(core.begin_frame(8).is_ok());
        // In-flight block blocks a second begin.
        assert_eq!(core.begin_frame(8), Err(FrameError::Busy));
        core.on_frame(1, meta(), &[]);
        // num_to_acquire=1 reached -> auto-stop.
        assert!(!core.is_grabbing());
        assert_eq!(core.begin_frame(4), Err(FrameError::NotAcquiring));
        // ...but DSStopAcquisition is still valid and required.
        core.stop_acquisition().unwrap();
        assert_eq!(
            core.stop_acquisition(),
            Err(GentlError::ResourceInUse)
        );
    }

    #[test]
    fn abort_frame_paths() {
        let core = DsCore::new();
        let (b0, _) = announce(&core, 64);
        core.queue_buffer(b0).unwrap();
        core.start_acquisition(GENTL_INFINITE).unwrap();
        let _t = core.begin_frame(64).unwrap();

        // Plain abort: back to the input pool.
        core.abort_frame(false);
        assert_eq!(core.queued_count(), 1);
        assert_eq!(core.await_delivery_count(), 0);

        // Incomplete abort (kill): to the output queue, flagged.
        let _t = core.begin_frame(64).unwrap();
        core.abort_frame(true);
        assert_eq!(core.queued_count(), 0);
        assert_eq!(core.await_delivery_count(), 1);
        core.with_buffer(b0, |b| {
            assert_eq!(b.state, BufferState::Filled);
            assert!(b.incomplete);
        }).unwrap();
        // Abort with nothing in flight is a no-op.
        core.abort_frame(false);
        core.abort_frame(true);
        assert_eq!(core.await_delivery_count(), 1);
    }

    #[test]
    fn deliver_and_requeue_cycle() {
        let core = DsCore::new();
        let (b0, _) = announce(&core, 64);
        core.queue_buffer(b0).unwrap();
        core.start_acquisition(GENTL_INFINITE).unwrap();
        let _t = core.begin_frame(3).unwrap();
        core.on_frame(1, meta(), &[1, 2, 3]);
        assert!(core.with_buffer(b0, |b| b.new_data).unwrap());
        core.mark_delivered(b0).unwrap();
        core.with_buffer(b0, |b| {
            assert_eq!(b.state, BufferState::Announced);
            assert!(!b.state.is_queued_info());
        }).unwrap();
        // Delivering twice is an error.
        assert!(core.mark_delivered(b0).is_err());
        // Requeue clears NEW_DATA and SIZE_FILLED.
        core.queue_buffer(b0).unwrap();
        core.with_buffer(b0, |b| {
            assert!(!b.new_data);
            assert_eq!(b.filled_size, 0);
            assert!(b.state.is_queued_info());
        }).unwrap();
    }

    #[test]
    fn flush_operations() {
        let core = DsCore::new();
        let (b0, _) = announce(&core, 16);
        let (b1, _) = announce(&core, 16);
        let (b2, _) = announce(&core, 16);
        core.queue_buffer(b0).unwrap();
        core.queue_buffer(b1).unwrap();
        core.start_acquisition(GENTL_INFINITE).unwrap();
        let _t = core.begin_frame(12).unwrap();
        core.on_frame(1, meta(), &[]); // b0 -> output
        assert_eq!(core.queued_count(), 1); // b1
        assert_eq!(core.await_delivery_count(), 1); // b0

        // INPUT_TO_OUTPUT: queued b1 joins the output queue with an event.
        // (The fill of b0 happened before any event registration and was
        // discarded — signals without a registered event are dropped.)
        let q = Arc::new(EventQueue::new());
        core.set_event_queue(q.clone());
        core.flush(AcqQueueOp::InputToOutput).unwrap();
        assert_eq!(core.queued_count(), 0);
        assert_eq!(core.await_delivery_count(), 2);
        assert_eq!(q.len(), 1, "one event entry added for the flushed buffer");
        core.with_buffer(b1, |b| {
            assert!(!b.new_data, "flush data is not new data");
            assert!(b.state.is_queued_info());
        }).unwrap();

        // OUTPUT_DISCARD: output queue drained, its event entries removed.
        core.flush(AcqQueueOp::OutputDiscard).unwrap();
        assert_eq!(core.await_delivery_count(), 0);
        assert_eq!(q.len(), 0);
        assert!(core.with_buffer(b0, |b| b.state == BufferState::Announced).unwrap());

        // UNQUEUED_TO_INPUT: b0/b1/b2 are all in no pool by now.
        core.flush(AcqQueueOp::UnqueuedToInput).unwrap();
        assert_eq!(core.queued_count(), 3);
        assert!(core.with_buffer(b2, |b| b.state == BufferState::Queued).unwrap());

        // ALL_TO_INPUT: everything stays in the pool, entries flushed.
        core.flush(AcqQueueOp::AllToInput).unwrap();
        assert_eq!(core.queued_count(), 3);
        assert_eq!(core.await_delivery_count(), 0);

        // ALL_DISCARD: revocable again.
        core.flush(AcqQueueOp::AllDiscard).unwrap();
        assert_eq!(core.queued_count(), 0);
        core.revoke(b0).unwrap();
        core.revoke(b1).unwrap();
        core.revoke(b2).unwrap();
    }

    #[test]
    fn discard_output_clears_queue_and_output() {
        let core = DsCore::new();
        let (b0, _) = announce(&core, 16);
        let q = Arc::new(EventQueue::new());
        core.set_event_queue(q.clone());
        core.queue_buffer(b0).unwrap();
        core.start_acquisition(GENTL_INFINITE).unwrap();
        let _t = core.begin_frame(12).unwrap();
        core.on_frame(1, meta(), &[]);
        assert_eq!(q.len(), 1);
        core.discard_output();
        assert_eq!(q.len(), 0);
        assert_eq!(core.await_delivery_count(), 0);
        assert!(core.with_buffer(b0, |b| b.state == BufferState::Announced).unwrap());
    }

    #[test]
    fn start_parameter_validation() {
        let core = DsCore::new();
        assert!(matches!(
            core.start_acquisition(0),
            Err(GentlError::InvalidParameter(_))
        ));
        core.start_acquisition(5).unwrap();
        assert_eq!(core.start_acquisition(5), Err(GentlError::ResourceInUse));
        core.stop_acquisition().unwrap();
        core.start_acquisition(GENTL_INFINITE).unwrap();
        assert!(core.is_grabbing());
    }

    #[test]
    fn frame_meta_payload_bytes() {
        let m = meta();
        assert_eq!(m.payload_bytes(), Some(12));
        let padded = FrameMeta {
            padding_x: 4,
            padding_y: 2,
            ..meta()
        };
        assert_eq!(padded.payload_bytes(), Some((4 + 4) * 3 + 2));
        let bad = FrameMeta {
            pixel_format: 0xFFFF_FFFF,
            ..meta()
        };
        assert_eq!(bad.payload_bytes(), None);
    }
}
