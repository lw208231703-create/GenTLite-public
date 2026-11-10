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
    /// Payload bytes. A recyclable payload returns its allocation to the
    /// source when the frame is dropped.
    pub payload: FramePayload,
}

/// Payload storage with optional recycling.
///
/// A `FramePayload` owns its bytes. When it carries a recycler, `Drop`
/// returns the emptied allocation to the source's pool instead of freeing
/// it; the next frame reuses the same capacity. This keeps the hot path free
/// of per-frame heap allocation without any raw-pointer ownership.
pub struct FramePayload {
    bytes: Vec<u8>,
    recycle: Option<std::sync::mpsc::SyncSender<Vec<u8>>>,
}

impl FramePayload {
    /// A plain owned payload (no recycling).
    pub fn new(bytes: Vec<u8>) -> Self {
        Self {
            bytes,
            recycle: None,
        }
    }

    /// An owned payload that returns its allocation to `recycle` on drop.
    pub fn with_recycle(bytes: Vec<u8>, recycle: std::sync::mpsc::SyncSender<Vec<u8>>) -> Self {
        Self {
            bytes,
            recycle: Some(recycle),
        }
    }

    /// Shrink the visible payload, as `Vec::truncate` did before the wrapper
    /// existed. The allocation and capacity stay intact for recycling.
    pub fn truncate(&mut self, len: usize) {
        self.bytes.truncate(len);
    }
}

impl std::ops::Deref for FramePayload {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        &self.bytes
    }
}

impl std::ops::DerefMut for FramePayload {
    fn deref_mut(&mut self) -> &mut [u8] {
        &mut self.bytes
    }
}

impl AsRef<[u8]> for FramePayload {
    fn as_ref(&self) -> &[u8] {
        &self.bytes
    }
}

impl Drop for FramePayload {
    fn drop(&mut self) {
        if let Some(recycle) = self.recycle.take() {
            let mut bytes = std::mem::take(&mut self.bytes);
            bytes.clear();
            let _ = recycle.try_send(bytes);
        }
    }
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
    /// Send one frame. Ownership passes to the session: a queueing front
    /// (U3V's emitter) can retain it without cloning; a synchronous front
    /// (GigE's socket writer) drops it when this returns.
    fn send_frame(&mut self, frame: CanonicalFrame);
    fn close(&mut self);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::sync_channel;

    #[test]
    fn recycled_payload_returns_the_same_allocation() {
        let (tx, rx) = sync_channel(1);
        let payload = FramePayload::with_recycle(vec![1, 2, 3], tx);
        let ptr = payload.as_ptr();

        drop(payload);

        let returned = rx.try_recv().expect("payload returned to the pool");
        assert_eq!(returned.as_ptr(), ptr);
        assert!(returned.is_empty(), "pooled buffers are cleared");
    }

    #[test]
    fn plain_payload_derefs_to_its_bytes() {
        let payload = FramePayload::new(vec![4, 5, 6]);
        assert_eq!(&payload[..], &[4, 5, 6]);
        assert_eq!(payload.as_ref(), &[4, 5, 6]);
    }
}
