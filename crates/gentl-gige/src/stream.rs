//! Data stream module object: GVSP stream channel setup/teardown over the
//! GVCP control channel (SCDAx/SCPx), one local receiver thread per
//! channel, and the registered-event bookkeeping behind EVENT_HANDLE.
//!
//! Channel rules (notes/gvcp-wirefacts.md, spec §11.5/§36.57): the primary
//! application opens a channel by writing SCDAx (destination IP) and then
//! SCPx = host_port << 16 (spec §36.57 — identical in GigE 1.2 §27.45;
//! the layout never changed). host_port != 0 activates, 0 closes, and SCPx
//! must be the last stream channel register written when opening. Pure
//! spec-only behavior: a device that ignores the spec encoding fails the
//! readback check with a clear error (project decision 2026-10-12,
//! notes/OPEN-QUESTIONS Q25).
//!
//! M3-4a multi-stream fan-out: the single GenTL DataStream ("Stream0",
//! GenTL surface unchanged) drives K GVSP channels, each with its own UDP
//! socket, receiver thread, and reassembler, all feeding the same DsCore.
//! K = min(device channels, `GENTLITE_GEV_STREAM_CHANNELS` default 8) —
//! see [`crate::discovery::desired_stream_channels`]. Channels 1..511 are
//! optional (spec §36.57): the count is probed by reading SCPx
//! sequentially until the device answers GEV_STATUS_INVALID_ADDRESS. A
//! secondary channel that fails to open degrades the stream to the
//! channels already opened; only total failure (channel 0) fails
//! DSStartAcquisition, matching the historical single-channel behavior.
//! Cross-channel frame delivery order is completion order — the spec is
//! silent on multi-channel ordering; every delivered frame keeps the
//! timestamp of its own leader.
//!
//! Shared-engine limitation (kept deliberately, M3-4a): the DsCore admits
//! exactly one in-flight frame. A frame-striped camera (whole frames
//! round-robined across channels, so cross-channel blocks never overlap —
//! the model the sim implements and the target of this fan-out) cannot
//! collide with itself. Only multi-tap parallel cameras (concurrent
//! cross-channel block streams) hit `FrameError::Busy`, dropping the
//! overlapped whole block (counted as `malformed` by the reassembler);
//! per-channel in-flight slots are the M3 DsCore work (gentl-core is
//! frozen this milestone).

use std::collections::HashMap;
use std::net::{Ipv4Addr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::thread::JoinHandle;

use gentl_core::ds::DsCore;
use gentl_core::error::{GentlError, GentlResult};
use gentl_core::event_queue::EventQueue;
use gentl_core::types::EventType;
use gentl_gvcp::registers;
use gentl_gvcp::wire::GEV_STATUS_INVALID_ADDRESS;
use gentl_gvcp::GvcpClient;

use crate::cti::{DeviceAccess, GevDevice};
use crate::discovery::desired_stream_channels;
use crate::gvsp_receiver::{receiver_loop, ReceiverStats};

/// The single stream channel's ID (fixed across sessions, GenTL 1.6 §3;
/// format not mandated by the spec — OPEN-QUESTION Q16).
pub const STREAM_ID: &str = "Stream0";

/// Transport context of one GVSP stream channel (spec §36.57 SCPx): one
/// local UDP socket, one receiver thread with its own reassembler, one
/// stop/kill flag pair, and the channel's own SCPSx-derived payload
/// stride. The socket stays bound across stop/start; only DSClose drops
/// the stream and its channels.
struct ChannelCtx {
    /// Channel number x of the SCPx/SCPSx/SCPDx/SCDAx address map.
    index: u16,
    /// Local GVSP socket, bound at open to 0.0.0.0:0; its port is written
    /// into SCPx at start.
    socket: UdpSocket,
    receiver: Mutex<Option<JoinHandle<()>>>,
    stop: Arc<AtomicBool>,
    kill: Arc<AtomicBool>,
    /// This channel's receiver counters, published when its thread exits.
    stats: Arc<Mutex<ReceiverStats>>,
    /// Set once the channel open sequence verified (SCPx readback); stop
    /// closes only opened channels.
    opened: Mutex<bool>,
    /// Payload bytes per full GVSP generic payload packet for this
    /// channel: the SCPSx packet_size field (spec §36.58, total size
    /// including IP/UDP/GVSP headers) minus
    /// [`registers::GVSP_HEADERS_OVERHEAD`]. The camera default is kept
    /// (no SCPSx renegotiation — OPEN-QUESTION Q16). 0 until the channel
    /// opened.
    payload_size: usize,
}

impl ChannelCtx {
    fn new(index: u16, socket: UdpSocket) -> Self {
        ChannelCtx {
            index,
            socket,
            receiver: Mutex::new(None),
            stop: Arc::new(AtomicBool::new(false)),
            kill: Arc::new(AtomicBool::new(false)),
            stats: Arc::new(Mutex::new(ReceiverStats::default())),
            opened: Mutex::new(false),
            payload_size: 0,
        }
    }
}

/// Open GVSP stream channel #x towards `local_ip:port` and return the
/// channel's payload stride derived from SCPSx.
///
/// R-561ca (spec §36.57): host_port must be 0 before writing SCDAx. A
/// fresh channel satisfies this; a stale non-zero value means a previous
/// session died without closing — close it first (writing 0 is always
/// allowed, R-586cd). SCPx is the last stream channel register written
/// when opening; the only writes allowed after it are SCPSx packet-size
/// negotiation, which is the M3 hook (spec §11.7, R-567s — OPEN-QUESTION
/// Q16). The open is verified by reading SCPx back: a device may refuse
/// or silently adjust the register, and a host_port of 0 means the
/// channel did not open. Spec-only: no non-compliant-device fallback
/// (project decision 2026-10-12, notes/OPEN-QUESTIONS Q25).
fn open_channel(c: &GvcpClient, x: u32, local_ip: Ipv4Addr, port: u16) -> GentlResult<usize> {
    if registers::scp_host_port(c.read_reg(registers::scp(x))?) != 0 {
        c.write_reg(registers::scp(x), registers::scp_make(0))?;
    }
    c.write_reg(registers::scda(x), u32::from(local_ip))?;
    c.write_reg(registers::scp(x), registers::scp_make(port))?;
    if registers::scp_host_port(c.read_reg(registers::scp(x))?) == 0 {
        return Err(GentlError::Io(format!(
            "stream channel {x} did not open (SCPx host_port reads back 0)"
        )));
    }
    // §36.58 (same in 1.2 §27.46): bits 16-31 hold the total packet size
    // including the IP/UDP/GVSP headers; the payload data length is what
    // the receiver needs for packet placement. Spec-only decoding: no
    // non-compliant-device reinterpretation (project decision 2026-10-12,
    // notes/OPEN-QUESTIONS Q25). An unwritten (0) or absurdly small
    // packet_size falls back to a 1500-byte MTU assumption; the value is
    // per channel, read here at channel open. See notes/OPEN-QUESTIONS.md
    // Q21.
    let scps = c.read_reg(registers::scps(x))?;
    Ok(
        match registers::scps_packet_size(scps).checked_sub(registers::GVSP_HEADERS_OVERHEAD) {
            Some(p) if p > 0 => p as usize,
            _ => (1500 - registers::GVSP_HEADERS_OVERHEAD) as usize,
        },
    )
}

/// Data Stream module behind DS_HANDLE.
pub struct GevDataStream {
    pub parent: Weak<GevDevice>,
    /// Protocol-independent engine state (buffer pools, acquisition
    /// counting, event data push); shared by every GVSP channel.
    pub core: Arc<DsCore>,
    /// Control channel shared with the device (externally synchronized).
    pub control: Arc<Mutex<GvcpClient>>,
    /// One transport context per GVSP stream channel. Channel 0 is always
    /// present; further channels were probed at open (spec §36.57 — they
    /// are optional and answer GEV_STATUS_INVALID_ADDRESS past the device
    /// count).
    channels: Mutex<Vec<ChannelCtx>>,
    /// Device timestamp tick frequency, for BUFFER_INFO_TIMESTAMP_NS.
    pub tick_freq: Option<u64>,
    /// Registered events by type.
    events: Mutex<HashMap<EventType, Arc<EventModule>>>,
    /// Core buffer id -> boxed BUFFER_HANDLE; the value handed out at
    /// announcement is returned by DSGetBufferID and EVENT_NEW_BUFFER_DATA.
    buffer_handles: Mutex<HashMap<u64, usize>>,
}
/// Buffer module behind BUFFER_HANDLE.
pub struct GevBuffer {
    pub stream: Weak<GevDataStream>,
    pub id: u64,
}

/// Event module behind EVENT_HANDLE (GenTL 1.6 §4.2). One registered event
/// object; its queue backs EventGetData/EventGetInfo/EventFlush/EventKill.
pub struct EventModule {
    pub source: Weak<GevDataStream>,
    pub event_type: EventType,
    pub queue: Arc<EventQueue>,
}

/// Local source IP that would reach `device_ip` (for SCDAx). A connected
/// UDP socket performs no handshake; the kernel only resolves the route.
fn local_ip_for(device_ip: Ipv4Addr) -> GentlResult<Ipv4Addr> {
    let s = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).map_err(GentlError::from)?;
    s.connect((device_ip, 9)).map_err(GentlError::from)?;
    let addr = s.local_addr().map_err(GentlError::from)?;
    match addr.ip() {
        std::net::IpAddr::V4(ip) => Ok(ip),
        std::net::IpAddr::V6(_) => Err(GentlError::Io("no IPv4 route to device".into())),
    }
}

impl GevDataStream {
    /// `DevOpenDataStream`: probe the stream channel count (spec §36.57)
    /// and read the tick frequency, then bind one local socket per
    /// channel. Does not touch the device's channel state.
    pub fn open(device: &Arc<GevDevice>) -> GentlResult<Arc<Self>> {
        let control = device.client.clone();
        let (n_channels, tick_freq) = {
            let c = control
                .lock()
                .map_err(|_| GentlError::Io("client lock poisoned".into()))?;
            // §36.57: channels 1..511 are optional; a device without
            // channel x answers GEV_STATUS_INVALID_ADDRESS — the probe
            // terminator. The probe stops at `desired` channels: a device
            // implementing more cannot be used anyway. Any other failure
            // stops the probe too: the count is best-effort and degrades,
            // never errors (a device that drops probe reads still gets
            // its channel 0).
            let desired = desired_stream_channels();
            let mut have = 1u32; // channel 0 always exists
            for x in 1..desired {
                match c.read_reg(registers::scp(x)) {
                    Ok(_) => have += 1,
                    Err(GentlError::GevStatus(GEV_STATUS_INVALID_ADDRESS)) => break,
                    Err(_) => break,
                }
            }
            // Tick frequency read is best-effort (defaults below).
            let hi = c.read_reg(registers::TIMESTAMP_TICK_FREQ_HIGH);
            let lo = c.read_reg(registers::TIMESTAMP_TICK_FREQ_LOW);
            let freq = match (hi, lo) {
                (Ok(h), Ok(l)) => Some((u64::from(h) << 32) | u64::from(l)),
                _ => None,
            };
            (have, freq)
        };
        let mut channels = Vec::new();
        for x in 0..n_channels {
            // A bind failure of a secondary channel degrades to the
            // channels already bound; channel 0 must bind or the stream
            // is unusable (historical single-channel behavior).
            match UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)) {
                Ok(socket) => channels.push(ChannelCtx::new(x as u16, socket)),
                Err(e) if x == 0 => return Err(GentlError::from(e)),
                Err(_) => break,
            }
        }
        Ok(Arc::new(Self {
            parent: Arc::downgrade(device),
            core: Arc::new(DsCore::new()),
            control,
            channels: Mutex::new(channels),
            tick_freq,
            events: Mutex::new(HashMap::new()),
            buffer_handles: Mutex::new(HashMap::new()),
        }))
    }

    /// Receiver thread running on any channel?
    pub fn is_running(&self) -> bool {
        self.channels
            .lock()
            .unwrap()
            .iter()
            .any(|ch| ch.receiver.lock().unwrap().is_some())
    }

    /// `DSStartAcquisition` transport half: open every stream channel
    /// (SCDAx + SCPx sequence per channel) and start one receiver thread
    /// per channel, all feeding the shared engine.
    ///
    /// Degradation rule (spec §36.57 optional channels): a secondary
    /// channel whose open fails leaves the stream running on the channels
    /// already opened; only failure of channel 0 (or no channel at all)
    /// fails the start, matching the historical single-channel behavior.
    pub fn start(self: &Arc<Self>) -> GentlResult<()> {
        let device = self.parent.upgrade().ok_or(GentlError::InvalidHandle)?;
        // GVRSP-only devices were rejected at device open; the match stays
        // as the dispatch point for a future GVRSP (RoCEv2) backend.
        match device.transport {
            crate::cti::StreamTransport::Gvsp => {}
        }
        if device.access == DeviceAccess::Monitor {
            // SCDAx/SCPx are writes; a monitor control channel must not
            // stream (spec §10).
            return Err(GentlError::AccessDenied);
        }
        if self.is_running() {
            return Err(GentlError::ResourceInUse);
        }
        let local_ip = local_ip_for(Ipv4Addr::from(device.discovery.current_ip))?;
        let mut channels = self.channels.lock().unwrap();
        let mut opened = 0usize;
        let mut first_err: Option<GentlError> = None;
        for idx in 0..channels.len() {
            let x = u32::from(channels[idx].index);
            let port = channels[idx]
                .socket
                .local_addr()
                .map_err(GentlError::from)?
                .port();
            let res = {
                let c = self
                    .control
                    .lock()
                    .map_err(|_| GentlError::Io("client lock poisoned".into()))?;
                open_channel(&c, x, local_ip, port)
            };
            match res {
                Ok(payload_size) => {
                    channels[idx].payload_size = payload_size;
                    *channels[idx].opened.lock().unwrap() = true;
                    channels[idx].stop.store(false, Ordering::SeqCst);
                    channels[idx].kill.store(false, Ordering::SeqCst);
                    let sock = channels[idx].socket.try_clone().map_err(GentlError::from)?;
                    let sink = self.core.clone();
                    let stop = channels[idx].stop.clone();
                    let kill = channels[idx].kill.clone();
                    let stats = channels[idx].stats.clone();
                    let payload_size = channels[idx].payload_size;
                    let name = format!("gvsp-recv-{}-ch{x}", device.id);
                    let me = self.clone();
                    let handle = std::thread::Builder::new()
                        .name(name)
                        .spawn(move || {
                            receiver_loop(sock, sink, stop, kill, payload_size, stats, me)
                        })
                        .map_err(|e| GentlError::Io(format!("spawn receiver: {e}")))?;
                    *channels[idx].receiver.lock().unwrap() = Some(handle);
                    opened += 1;
                }
                Err(e) => {
                    first_err = Some(e);
                    break;
                }
            }
        }
        if opened == 0 {
            // Total failure: the caller (DSStartAcquisition) rolls the
            // engine state back and the start fails, as before M3-4a.
            return Err(first_err.unwrap_or_else(|| {
                GentlError::Io("no stream channel available".into())
            }));
        }
        Ok(())
    }

    /// Receiver-fatal recovery (IO error / unrecoverable desync): the
    /// transport is dead, so the engine must reflect that — otherwise
    /// `DSGetInfo(IS_GRABBING)` keeps reporting true and `EventGetData`
    /// blocks forever. Forces the core engine to the stopped state and
    /// wakes pending event waiters with an abort.
    pub fn on_receiver_fatal(self: &Arc<Self>) {
        self.core.abort_engine();
        if let Ok(events) = self.events.lock() {
            for ev in events.values() {
                ev.queue.kill_one();
            }
        }
    }

    /// `DSStopAcquisition` transport half: stop every receiver (graceful
    /// or kill) and close every opened channel (SCPx host_port = 0).
    pub fn stop(&self, kill: bool) {
        let channels = self.channels.lock().unwrap();
        for ch in channels.iter() {
            ch.kill.store(kill, Ordering::SeqCst);
            ch.stop.store(true, Ordering::SeqCst);
        }
        for ch in channels.iter() {
            if let Some(h) = ch.receiver.lock().unwrap().take() {
                let _ = h.join();
            }
        }
        if let Ok(c) = self.control.lock() {
            for ch in channels.iter() {
                let mut opened = ch.opened.lock().unwrap();
                if *opened {
                    // 0 closes the channel (spec §36.57).
                    let _ = c.write_reg(registers::scp(u32::from(ch.index)), registers::scp_make(0));
                    *opened = false;
                }
            }
        }
    }

    /// `DSClose` cleanup: stop the engine, kill event registrations, free
    /// the buffer handle boxes (the buffers themselves die with the core).
    pub fn close(&self) {
        if self.is_running() {
            self.stop(false);
        }
        for ev in self.events.lock().unwrap().values() {
            ev.queue.mark_dead();
        }
        self.core.clear_event_queue();
        let handles: Vec<usize> = self
            .buffer_handles
            .lock()
            .unwrap()
            .values()
            .copied()
            .collect();
        for h in handles {
            unsafe { gentl_core::free_handle(h as *mut core::ffi::c_void) };
        }
        self.buffer_handles.lock().unwrap().clear();
    }

    /// `GCRegisterEvent`.
    pub fn register_event(
        self: &Arc<Self>,
        event_type: EventType,
    ) -> GentlResult<Arc<EventModule>> {
        let mut map = self.events.lock().unwrap();
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

    /// `GCUnregisterEvent`: pending events are silently discarded, pending
    /// output-queue buffers become non-queued, waiters get ABORT.
    pub fn unregister_event(&self, event_type: EventType) -> GentlResult<()> {
        let Some(module) = self.events.lock().unwrap().remove(&event_type) else {
            // GenTL 1.6 §6.3.7.7 lists this case as GC_ERR_NOT_INITIALIZED.
            return Err(GentlError::NotInitialized);
        };
        module.queue.mark_dead();
        self.core.clear_event_queue();
        self.core.discard_output();
        Ok(())
    }

    /// Record the handle handed out for a core buffer id. Stored as an
    /// address-sized integer so the stream stays `Send`/`Sync`.
    pub fn set_buffer_handle(&self, id: u64, handle: *mut core::ffi::c_void) {
        self.buffer_handles.lock().unwrap().insert(id, handle as usize);
    }

    /// The stable BUFFER_HANDLE for a core buffer id.
    pub fn buffer_handle(&self, id: u64) -> Option<*mut core::ffi::c_void> {
        self.buffer_handles
            .lock()
            .unwrap()
            .get(&id)
            .map(|h| *h as *mut core::ffi::c_void)
    }

    /// Drop a buffer's handle registration (does not free the box; the
    /// caller does that).
    pub fn remove_buffer_handle(&self, id: u64) {
        self.buffer_handles.lock().unwrap().remove(&id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_ip_for_loopback_is_loopback() {
        let ip = local_ip_for(Ipv4Addr::LOCALHOST).unwrap();
        assert_eq!(ip, Ipv4Addr::LOCALHOST);
    }
}
