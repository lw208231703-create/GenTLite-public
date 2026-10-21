//! Phase A Linux stub for the data stream (notes/PENDING.md "Linux
//! 可移植"): streaming needs the USB backend (Phase B: rusb/libusb). The
//! types exist so the ungated GenTL surface in lib.rs type-checks;
//! `open` can never succeed (no device can exist — stub discovery plus
//! the compiled-out open path), so every stream-level operation is
//! unreachable. `stream_id` stays real: stream IDs are pure formatting.

#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};

use gentl_core::ds::DsCore;
use gentl_core::error::{GentlError, GentlResult};
use gentl_core::event_queue::EventQueue;
use gentl_core::types::EventType;

use crate::cti::U3vDevice;

fn not_supported<T>() -> GentlResult<T> {
    Err(GentlError::NotImplemented(
        "U3V USB backend on this platform (Phase B: rusb, notes/PENDING.md)",
    ))
}

/// GenTL stream ID for stream channel `i` (format not mandated by the
/// spec; mirrors the GigE producer's "Stream0" style).
pub fn stream_id(index: u32) -> String {
    format!("Stream{index}")
}

/// Buffer module behind BUFFER_HANDLE.
pub struct U3vBuffer {
    pub stream: Weak<U3vDataStream>,
    pub id: u64,
}

/// Event module behind EVENT_HANDLE (GenTL 1.6 §4.2).
pub struct EventModule {
    pub source: Weak<U3vDataStream>,
    pub event_type: EventType,
    pub queue: Arc<EventQueue>,
}

/// Data stream module behind DS_HANDLE. Never constructed on this
/// backend: `open` always fails, so a DS_HANDLE can never be handed out.
pub struct U3vDataStream {
    pub parent: Weak<U3vDevice>,
    pub core: Arc<DsCore>,
    pub channel_index: u32,
    events: Mutex<HashMap<EventType, Arc<EventModule>>>,
    buffer_handles: Mutex<HashMap<u64, usize>>,
}

impl U3vDataStream {
    /// `DevOpenDataStream` transport half. Phase B (rusb backend);
    /// unreachable in practice — no `U3vDevice` can be opened on this
    /// platform, so no device module (and no stream) can exist.
    pub fn open(_device: &Arc<U3vDevice>, _channel_index: u32) -> GentlResult<Arc<Self>> {
        not_supported()
    }

    pub fn start(self: &Arc<Self>) -> GentlResult<()> {
        not_supported()
    }

    pub fn stop(&self, _kill: bool) {}

    pub fn close(&self) {}

    pub fn register_event(
        self: &Arc<Self>,
        _event_type: EventType,
    ) -> GentlResult<Arc<EventModule>> {
        not_supported()
    }

    /// No event can be registered on this backend, so unregister always
    /// reports "not registered" (GenTL 1.6 §6.3.7.7: GC_ERR_NOT_INITIALIZED).
    pub fn unregister_event(&self, _event_type: EventType) -> GentlResult<()> {
        Err(GentlError::NotInitialized)
    }

    pub fn set_buffer_handle(&self, _id: u64, _handle: *mut core::ffi::c_void) {}

    pub fn buffer_handle(&self, _id: u64) -> Option<*mut core::ffi::c_void> {
        None
    }

    pub fn remove_buffer_handle(&self, _id: u64) {}
}
