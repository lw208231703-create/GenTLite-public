//! Transport-neutral device events.
//!
//! The gateway forwards the downstream camera's asynchronous events to
//! whichever front the deployment selected. A GigE front re-emits them as
//! §17 EVENT/EVENTDATA datagrams (and prefers the raw pass-through relay
//! because the host was given the camera's own XML), while a U3V front has
//! to re-emit them as GenCP `EVENT_CMD` packets on its EIRM event endpoint
//! (§4.1.3.3). This module is the structured middle ground: one record
//! shape both sides understand, produced by the camera backends.
//!
//! ACK/retransmission semantics stay with the transports (the GigE message
//! channel and the U3V DEI channel do not share one); see
//! docs/00-governance/OPEN-QUESTIONS.md Q41 for the relay's v1 choice.

/// One asynchronous camera event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceEvent {
    /// Event number as defined by the camera's GenICam XML (§17.1.1 CO-230cd
    /// for GigE, GenCP event records for U3V).
    pub event_id: u16,
    /// Nanoseconds when the downstream transport defines that unit (U3V
    /// event timestamps are ns). 0 when it does not: GigE cameras count
    /// device ticks whose frequency the gateway may not know.
    pub timestamp_ns: u64,
    /// Camera event payload (EVENTDATA); empty for payload-less events.
    pub data: Vec<u8>,
}

/// Sink for [`DeviceEvent`]s, implemented by a front. Called from the
/// backend's event thread: implementations must not block for long (queue
/// with a bound and drop on overflow is the expected behavior).
pub trait DeviceEventSink: Send + Sync {
    fn forward(&self, event: DeviceEvent);
}
