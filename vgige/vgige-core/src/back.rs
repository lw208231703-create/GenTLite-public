//! Downstream camera seam: [`CameraBack`] — control transactions, XML
//! fetch, and stream open/close, all in GEV status codes (the gateway's
//! error-transparency contract: a camera's 0x8006 must reach the remote
//! client unchanged, whatever the transport below).
//!
//! Implementations include GigE Vision (`vgige-back-gige`), USB3 Vision
//! (`vgige-back-u3v`) and the synthetic null backend. Adapters translate
//! their transport's failures to
//! GEV status codes at this boundary (timeout/I/O -> 0x8FFF, Appendix B.21,
//! the simulator regmap's precedent).

use std::sync::{Arc, Mutex};

use crate::frame::FrameSource;

/// The downstream camera, behind one exclusive control channel.
pub trait CameraBack: Send {
    /// READREG of one 32-bit register (spec §16.3). Err = GEV status.
    fn read_reg(&mut self, addr: u32) -> Result<u32, u16>;
    /// WRITEREG of one 32-bit register (spec §16.4). Err = GEV status.
    fn write_reg(&mut self, addr: u32, value: u32) -> Result<(), u16>;
    /// READMEM-style block read (spec §16.5); `len` is a multiple of 4.
    /// Err = GEV status.
    fn read_block(&mut self, addr: u32, len: u32) -> Result<Vec<u8>, u16>;
    /// WRITEMEM-style block write. The GigE adapter answers
    /// GEV_STATUS_NOT_IMPLEMENTED (gentl-gvcp's client has no WRITEMEM path
    /// in v1); block writes exist in the seam because U3V register access
    /// is block-shaped (Step 2).
    fn write_block(&mut self, addr: u32, data: &[u8]) -> Result<(), u16>;
    /// Fetch the camera's GenICam XML (spec §14.1.1): First URL bootstrap
    /// register, `local:` blob read, zip transparently decompressed. Err =
    /// GEV status.
    fn fetch_xml(&mut self) -> Result<Vec<u8>, u16>;
    /// The camera's identity strings from its own bootstrap (GigE: Table
    /// 36-1 string registers + Version; U3V: ABRM §4.2.1 strings +
    /// GenCP/U3V versions). The front maps this into the virtual device's
    /// discovery payload — each back knows its native bootstrap layout,
    /// which is exactly why this rides the trait.
    fn fetch_identity(&mut self) -> Result<BackIdentity, u16>;
    /// Open the camera's stream towards this box: configure the downstream
    /// destination per the wire protocol and return the intake bound to the
    /// local receive endpoint. Err = GEV status (the remote client's
    /// channel-open write sees the real reason, e.g. ACCESS_DENIED).
    fn open_stream(&mut self) -> Result<Box<dyn FrameSource>, u16>;
    /// Close the downstream stream (camera channel register back to 0,
    /// R-091ca cascade source). Err = GEV status.
    fn close_stream(&mut self) -> Result<(), u16>;
}

/// Transport-native identity of the downstream camera (see
/// [`CameraBack::fetch_identity`]).
pub struct BackIdentity {
    pub manufacturer: String,
    pub model: String,
    pub device_version: String,
    pub serial_number: String,
    pub user_defined_name: String,
    /// The camera's native spec version (GigE: Version register;
    /// U3V: max(GenCP, U3V) version pair — the facade reports the U3V
    /// version, whose namespace the gateway's virtual GigE device claims).
    pub spec_version_major: u16,
    pub spec_version_minor: u16,
}

/// The shared downstream handle: register-space dispatch (front serve
/// thread), relay open/close (same thread in practice), and startup all
/// hold clones; every access is one short exclusive transaction.
pub type SharedBack = Arc<Mutex<dyn CameraBack>>;
