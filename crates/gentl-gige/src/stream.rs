//! Data stream module object: stream channel 0 setup/teardown over the
//! GVCP control channel (SCDA0/SCP0), the local GVSP receiver thread, and
//! the registered-event bookkeeping behind EVENT_HANDLE.
//!
//! Channel rules (notes/gvcp-wirefacts.md, spec §11.5/§36.57): the primary
//! application opens the channel by writing SCDA0 (destination IP) and then
//! SCP0 = host_port << 16 (spec §36.57 — identical in GigE 1.2 §27.45;
//! the layout never changed). host_port != 0 activates, 0 closes, and SCP0
//! must be the last stream channel register written when opening. Pure
//! spec-only behavior: a device that ignores the spec encoding fails the
//! readback check with a clear error (project decision 2026-10-12,
//! notes/OPEN-QUESTIONS Q25).

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
use gentl_gvcp::GvcpClient;

use crate::cti::{DeviceAccess, GevDevice};
use crate::gvsp_receiver::{receiver_loop, ReceiverStats};

/// The single stream channel's ID (fixed across sessions, GenTL 1.6 §3;
/// format not mandated by the spec — OPEN-QUESTION Q16).
pub const STREAM_ID: &str = "Stream0";

/// Data Stream module behind DS_HANDLE.
pub struct GevDataStream {
    pub parent: Weak<GevDevice>,
    /// Protocol-independent engine state (buffer pools, acquisition
    /// counting, event data push).
    pub core: Arc<DsCore>,
    /// Control channel shared with the device (externally synchronized).
    pub control: Arc<Mutex<GvcpClient>>,
    /// Local GVSP socket, bound at open to 0.0.0.0:0.
    socket: Mutex<Option<UdpSocket>>,
    receiver: Mutex<Option<JoinHandle<()>>>,
    stop_receiver: Arc<AtomicBool>,
    kill_receiver: Arc<AtomicBool>,
    /// Payload bytes per full GVSP generic payload packet: the SCPS0
    /// packet_size field (spec §36.58, total size including IP/UDP/GVSP
    /// headers) minus [`registers::GVSP_HEADERS_OVERHEAD`]. The camera
    /// default is kept (M2 does not renegotiate — OPEN-QUESTION Q16).
    pub payload_size: usize,
    /// Device timestamp tick frequency, for BUFFER_INFO_TIMESTAMP_NS.
    pub tick_freq: Option<u64>,
    /// Registered events by type.
    events: Mutex<HashMap<EventType, Arc<EventModule>>>,
    /// Core buffer id -> boxed BUFFER_HANDLE; the value handed out at
    /// announcement is returned by DSGetBufferID and EVENT_NEW_BUFFER_DATA.
    buffer_handles: Mutex<HashMap<u64, usize>>,
    /// Latest receiver counters.
    pub receiver_stats: Arc<Mutex<ReceiverStats>>,
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

/// Local source IP that would reach `device_ip` (for SCDA0). A connected
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
    /// `DevOpenDataStream`: read the channel configuration (SCPS0, tick
    /// frequency) and bind the local socket. Does not touch the device.
    pub fn open(device: &Arc<GevDevice>) -> GentlResult<Arc<Self>> {
        let control = device.client.clone();
        let (payload_size, tick_freq) = {
            let c = control
                .lock()
                .map_err(|_| GentlError::Io("client lock poisoned".into()))?;
            let scps = c.read_reg(registers::SCPS0)?;
            // §36.58 (same in 1.2 §27.46): bits 16-31 hold the total
            // packet size including the IP/UDP/GVSP headers; the payload
            // data length is what the receiver needs for packet placement.
            // Spec-only decoding: no non-compliant-device reinterpretation
            // (project decision 2026-10-12, notes/OPEN-QUESTIONS Q25).
            let payload_size = match registers::scps_packet_size(scps)
                .checked_sub(registers::GVSP_HEADERS_OVERHEAD)
            {
                Some(p) if p > 0 => p as usize,
                // Unwritten (0) or absurdly small packet_size: fall back to
                // a 1500-byte MTU assumption. A real device ships a factory
                // default; the value is verified again after any future M3
                // negotiation. See notes/OPEN-QUESTIONS.md Q21.
                _ => (1500 - registers::GVSP_HEADERS_OVERHEAD) as usize,
            };
            // Tick frequency read is best-effort (defaults below).
            let hi = c.read_reg(registers::TIMESTAMP_TICK_FREQ_HIGH);
            let lo = c.read_reg(registers::TIMESTAMP_TICK_FREQ_LOW);
            let freq = match (hi, lo) {
                (Ok(h), Ok(l)) => Some((u64::from(h) << 32) | u64::from(l)),
                _ => None,
            };
            (payload_size, freq)
        };
        let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).map_err(GentlError::from)?;
        Ok(Arc::new(Self {
            parent: Arc::downgrade(device),
            core: Arc::new(DsCore::new()),
            control,
            socket: Mutex::new(Some(socket)),
            receiver: Mutex::new(None),
            stop_receiver: Arc::new(AtomicBool::new(false)),
            kill_receiver: Arc::new(AtomicBool::new(false)),
            payload_size,
            tick_freq,
            events: Mutex::new(HashMap::new()),
            buffer_handles: Mutex::new(HashMap::new()),
            receiver_stats: Arc::new(Mutex::new(ReceiverStats::default())),
        }))
    }

    /// Receiver thread running?
    pub fn is_running(&self) -> bool {
        self.receiver.lock().unwrap().is_some()
    }

    /// `DSStartAcquisition` transport half: write SCDA0 + SCP0 (channel
    /// activation) and start the receiver thread.
    pub fn start(self: &Arc<Self>) -> GentlResult<()> {
        let device = self.parent.upgrade().ok_or(GentlError::InvalidHandle)?;
        if device.access == DeviceAccess::Monitor {
            // SCDA0/SCP0 are writes; a monitor control channel must not
            // stream (spec §10).
            return Err(GentlError::AccessDenied);
        }
        if self.is_running() {
            return Err(GentlError::ResourceInUse);
        }
        let local_ip = local_ip_for(Ipv4Addr::from(device.discovery.current_ip))?;
        let port = {
            let mut guard = self.socket.lock().unwrap();
            let sock = guard.as_mut().ok_or(GentlError::InvalidHandle)?;
            sock.local_addr().map_err(GentlError::from)?.port()
        };
        {
            let c = self
                .control
                .lock()
                .map_err(|_| GentlError::Io("client lock poisoned".into()))?;
            // R-561ca (spec §36.57): host_port must be 0 before writing
            // SCDA0. A fresh channel satisfies this; a stale non-zero value
            // means a previous session died without closing — close it
            // first (writing 0 is always allowed, R-586cd).
            if registers::scp_host_port(c.read_reg(registers::SCP0)?) != 0 {
                c.write_reg(registers::SCP0, registers::scp_make(0))?;
            }
            c.write_reg(registers::SCDA0, u32::from(local_ip))?;
            // SCP0 is the last stream channel register written when opening
            // the channel (spec §36.57); the only writes allowed after it
            // are SCPS0 packet-size negotiation, which is the M3 hook
            // (spec §11.7, R-567s — OPEN-QUESTION Q16).
            c.write_reg(registers::SCP0, registers::scp_make(port))?;
            // Verify the open took: the device may refuse or silently
            // adjust the register, so a host_port of 0 here means no
            // channel and no receiver may be started. Spec-only: no
            // non-compliant-device fallback (project decision 2026-10-12,
            // notes/OPEN-QUESTIONS Q25) — a device that ignores the spec
            // encoding fails here with a clear error.
            if registers::scp_host_port(c.read_reg(registers::SCP0)?) == 0 {
                return Err(GentlError::Io(
                    "stream channel did not open (SCP0 host_port reads back 0)".into(),
                ));
            }
        }
        self.stop_receiver.store(false, Ordering::SeqCst);
        self.kill_receiver.store(false, Ordering::SeqCst);
        let sock = {
            let guard = self.socket.lock().unwrap();
            guard
                .as_ref()
                .ok_or(GentlError::InvalidHandle)?
                .try_clone()
                .map_err(GentlError::from)?
        };
        let sink = self.core.clone();
        let stop = self.stop_receiver.clone();
        let kill = self.kill_receiver.clone();
        let stats = self.receiver_stats.clone();
        let payload_size = self.payload_size;
        let name = format!("gvsp-recv-{}", device.id);
        let me = self.clone();
        let handle = std::thread::Builder::new()
            .name(name)
            .spawn(move || receiver_loop(sock, sink, stop, kill, payload_size, stats, me))
            .map_err(|e| GentlError::Io(format!("spawn receiver: {e}")))?;
        *self.receiver.lock().unwrap() = Some(handle);
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

    /// `DSStopAcquisition` transport half: stop the receiver (graceful or
    /// kill) and close the channel (SCP0 host_port = 0).
    pub fn stop(&self, kill: bool) {
        self.kill_receiver.store(kill, Ordering::SeqCst);
        self.stop_receiver.store(true, Ordering::SeqCst);
        if let Some(h) = self.receiver.lock().unwrap().take() {
            let _ = h.join();
        }
        if let Ok(c) = self.control.lock() {
            // 0 closes the channel (spec §36.57).
            let _ = c.write_reg(registers::SCP0, registers::scp_make(0));
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
