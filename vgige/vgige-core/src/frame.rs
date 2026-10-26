//! Relay seams: the frame common denominator ([`CanonicalFrame`]) and the
//! intake/re-emit traits ([`FrameSource`], [`FrameSession`]) the relay
//! driver is generic over.

use std::time::Duration;

/// One image frame in transport-neutral form: the metadata a GVSP image
/// leader carries plus the payload bytes exactly as the wire delivered
/// them (pixel data, padded rows included).
///
/// `padding_x`/`padding_y` ride along although the leader-encoder seam is
/// their only consumer: dropping them would change what a padded-frame
/// camera sees on the virtual wire (zero-behavior-change discipline).
pub struct CanonicalFrame {
    pub timestamp: u64,
    /// PFNC pixel format value.
    pub pixel_format: u32,
    pub width: u32,
    pub height: u32,
    pub offset_x: u32,
    pub offset_y: u32,
    pub padding_x: u32,
    pub padding_y: u32,
    pub payload: Vec<u8>,
}

/// Downstream image intake (the real camera's stream). One live instance
/// exists per open channel; the relay driver polls it from its receiver
/// thread.
///
/// Implementations: `vgige_back_gige::GigeFrameSource` (GVSP reassembly)
/// today; a U3V payload receiver (vgige-back-u3v) is the planned second
/// implementation — which is what justifies this trait's existence.
pub trait FrameSource: Send {
    /// Next reassembled frame, blocking up to `timeout` (the implementation
    /// MUST wait out the timeout even when internally dead, so the driver's
    /// stop-flag polling keeps its cadence). `None` = no frame within the
    /// window; a source that has suffered a transport-level failure keeps
    /// answering `None` after logging the error.
    fn next_frame(&mut self, timeout: Duration) -> Option<CanonicalFrame>;
}

/// Front-end encoding session (the virtual device's transmitter). Created
/// per channel open by the relay's [`crate::relay::SessionFactory`];
/// `send_frame` reads its live wire parameters (destination, slice size)
/// from the shared [`crate::relay::StreamRegs`] the front-specific
/// constructor captured.
///
/// Implementations: `vgige_front_gige::GvspSession` (GVSP 3.0 encoding)
/// today; a USB gadget transmitter (vgige-front-u3v) is the planned second
/// implementation.
pub trait FrameSession: Send {
    /// Bind/attach the session's transport resources. `payload_hint` is the
    /// initial payload-bytes-per-packet value derived from the virtual
    /// channel registers at open time; sessions whose wire format fixes the
    /// slice at open (e.g. a gadget MTU) consume it directly, sessions with
    /// per-frame negotiation (GigE SCPS) re-read their registers every
    /// `send_frame` and treat it as the first frame's value.
    fn open(&mut self, payload_hint: usize) -> Result<(), String>;
    fn send_frame(&mut self, frame: &CanonicalFrame);
    fn close(&mut self);
}
