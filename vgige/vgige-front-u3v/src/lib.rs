//! vgige-front-u3v: the U3V **device-simulating** front-end (roadmap Step
//! 3 — the gateway matrix cell "U3V front ← any back"): the Linux box
//! presents itself as a standard USB3 Vision camera on its USB gadget
//! port, so a PC host can drive the gateway with any U3V consumer.
//!
//! Two layers, and the split is the discipline of this crate:
//!
//! - [`device`]: the U3V device-side protocol state machine — ABRM/SBRM/
//!   SIRM register file answering the host's GenCP transactions, a GenCP
//!   command service, and the stream emitter turning
//!   [`vgige_core::CanonicalFrame`]s into §5.5 blocks (leader/payload
//!   pieces/trailer). Pure memory: it runs on byte-stream traits, never
//!   touches USB, and is fully testable — `tests/pairing.rs` drives it
//!   with the real host-side client (`gentl_u3v::client::U3vCamera`).
//! - [`gadget`]: the Linux FunctionFS physical layer binding those byte
//!   streams to configfs + /dev/*-ep* endpoints. Integration path only;
//!   the machine-level bring-up is 待验 (design note §5, Q39).
//!
//! Gateway semantics (symmetric to the backs): the bootstrap/vendor
//! register space forwards to the downstream [`vgige_core::CameraBack`]
//! (the served GenICam XML is the camera's own injected document, so the
//! host's GenApi writes land on the real camera 1:1); the custom window
//! (`0x2000_0000..`, vgige-core slot constants) is served locally; the
//! XML blob is exposed via the SBRM manifest table (GenCP §5.5 — the U3V
//! equivalent of the GigE front's FIRST_URL). U3V has no discovery
//! broadcast: identity lives entirely in the USB descriptors + ABRM, with
//! the GUID synthesized from the camera identity (Q39).

pub mod device;
pub mod gadget;

pub use device::{
    DeviceConfig, DeviceTransfers, HostCommands, RegisterFile, StreamError, U3vDevice,
    U3vStreamSession,
};
pub use gadget::GadgetConfig;

use vgige_core::BackIdentity;

/// Device-side address map (QoI — the device owns its layout; the Step-2
/// test fake uses the same shape, so the two protocol peers agree).
pub(crate) const ABRM_BASE: u64 = 0x0000;
pub(crate) const MANIFEST_BASE: u64 = 0x1000;
pub(crate) const SBRM_BASE: u64 = 0x2000;
pub(crate) const XML_BASE: u64 = 0x3000;
pub(crate) const SIRM_BASE: u64 = 0x4000;
/// SIRM transfer limit advertised to the host (SBRM 0x14/0x18): bounds
/// the GenCP chunk envelopes; 1 KiB is the common U3V minimum.
pub(crate) const MAX_TRANSFER: u32 = 1024;

/// Deterministic GUID synthesis for the device descriptors (Q39): the
/// U3V identity is a GUID, not a MAC — the gateway derives a stable GUID
/// from the downstream camera's serial + model (FNV-1a rounds, RFC 4122
/// version/variant bits set). QoI: uniqueness rests on the camera
/// serial; collisions across different cameras are practically excluded.
pub fn synthesize_guid(seed: &str) -> [u8; 16] {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01B3;
    let seed = seed.as_bytes();
    let mut out = [0u8; 16];
    for (i, b) in out.iter_mut().enumerate() {
        let mut h = FNV_OFFSET ^ (i as u64 + 1);
        for &s in seed {
            h = (h ^ s as u64).wrapping_mul(FNV_PRIME);
            h ^= h >> 32;
        }
        *b = (h >> ((i % 8) * 8)) as u8;
    }
    out[6] = (out[6] & 0x0F) | 0x50; // version 5 (name-based shape)
    out[8] = (out[8] & 0x3F) | 0x80; // RFC 4122 variant
    out
}

impl DeviceConfig {
    /// Convenience: build from the downstream camera's identity and the
    /// injected XML blob.
    pub fn from_parts(identity: BackIdentity, xml: &[u8], payload_size: u64, guid: [u8; 16]) -> Self {
        DeviceConfig {
            manufacturer: identity.manufacturer,
            model: identity.model,
            device_version: identity.device_version,
            serial_number: identity.serial_number,
            xml: xml.to_vec(),
            payload_size,
            guid,
        }
    }
}
