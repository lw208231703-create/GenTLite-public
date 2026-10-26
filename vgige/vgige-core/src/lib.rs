//! vgige-core: transport-agnostic heart of the smart-camera gateway.
//!
//! The gateway relays a real camera to remote clients as a virtual device.
//! This crate holds everything that does not care how the camera or the
//! client is reached:
//!
//! - [`CameraBack`] — the downstream control/stream seam (implemented by
//!   vgige-back-gige over GVCP/GVSP today; vgige-back-u3v over USB3 Vision
//!   is the designed second implementation — hence the trait).
//! - [`FrameSource`] / [`FrameSession`] — image intake and re-emit seams of
//!   the relay driver ([`Relay`]); second implementations: GVSP reassembly
//!   (back-gige) / GVSP 3.0 encoding (front-gige) today, U3V payload receive
//!   / USB gadget transmit later.
//! - [`CanonicalFrame`] — the frame common denominator between seams.
//! - [`RegisterSpace`] — the virtual device's 32-bit address dispatch
//!   (pass-through / custom window / XML blob / stream registers / local
//!   CCP). GVCP-shaped by nature; used by GigE-style fronts.
//! - [`XmlMerger`] — camera XML + injected SmartCamera nodes.
//!
//! Seam discipline: no plugin registries, no dynamic discovery — a trait
//! exists only where a concrete second implementation is planned
//! (notes/linux-vgige-design.md, roadmap).

pub mod back;
pub mod frame;
pub mod relay;
pub mod space;
pub mod xml;

pub use back::{BackIdentity, CameraBack, SharedBack};
pub use frame::{CanonicalFrame, FrameSession, FrameSource};
pub use relay::{Relay, SessionFactory, StreamRegs, virtual_payload_chunk};
pub use space::RegisterSpace;
pub use xml::XmlMerger;

/// Semantic version of the gateway feature window, u32 packed
/// major<<16 | minor<<8 | patch (readable at slot 0x2000_0000).
pub const VGIGE_VERSION: u32 = 0x0001_0000; // 1.0.0

/// Base of the locally-served custom register window: 256 u32 slots
/// (0x2000_0000..0x2000_1000). Below this address everything that is not
/// explicitly virtualized is passed through to the real camera.
pub const CUSTOM_BASE: u32 = 0x2000_0000;
/// Number of u32 slots in the custom window.
pub const CUSTOM_SLOTS: usize = 256;

/// Base address of the injected GenICam XML blob on the virtual device
/// (local to vgige; reads are served from the injected copy).
pub const XML_BASE_V: u32 = CUSTOM_BASE + 0x1000;

/// Custom-window slot indices (Address = CUSTOM_BASE + 4*index).
pub mod slot {
    /// RO: [`crate::VGIGE_VERSION`].
    pub const VERSION: usize = 0;
    /// RO: AI inspection result (0 = not inspected, 1 = OK, 2 = NG);
    /// placeholder for the business layer ([`crate::space::RegisterSpace::set_ai_result`]).
    pub const AI_RESULT: usize = 1;
    /// RO: anomaly score x1000; placeholder ([`crate::space::RegisterSpace::set_ai_score`]).
    pub const AI_SCORE: usize = 2;
    /// RO: number of frames relayed to the remote client.
    pub const FRAMES_RELAYED: usize = 3;
    /// WO: user command; a write is accepted and logged (business hook).
    pub const USER_COMMAND: usize = 4;
}

// Generic error (GigE Vision spec Appendix B.21); gentl-gvcp's wire module
// does not export it. Used when the downstream transaction fails without a
// GEV status (timeout/I/O): the real camera is unreachable.
pub(crate) const GEV_STATUS_ERROR: u16 = 0x8FFF;
