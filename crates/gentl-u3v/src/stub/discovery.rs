//! Phase A Linux stub for USB discovery (notes/PENDING.md "Linux 可移植"):
//! real enumeration needs the rusb/libusb backend (Phase B). Every entry
//! point reports an empty bus — a producer with zero interfaces and zero
//! devices is spec-correct and must not crash the consumer. The type
//! shapes mirror the Windows module so the ungated GenTL surface in
//! lib.rs type-checks unchanged; on this backend the device-side types
//! are never constructed, hence the module-wide allow.
#![allow(dead_code)]

/// One USB root hub = one GenTL Interface (UQ3) — none on the stub bus.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RootHubInfo {
    /// Root hub device instance ID, e.g. `USB\ROOT_HUB30\4&...`.
    pub instance_id: String,
    /// Bus-reported description, e.g. "USB 根集线器(USB 3.0)".
    pub display: String,
}

/// Identity from the Class-Specific Device Info Descriptor (§3.2.5
/// Table 3-5). Same field layout as the Windows module.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeviceIdentity {
    pub guid: Option<String>,
    pub gen_cp_version: u32,
    pub u3v_version: u32,
    pub manufacturer: String,
    pub model: String,
    pub family: String,
    pub device_version: String,
    pub manufacturer_info: String,
    pub serial_number: String,
    pub user_defined_name: String,
}

/// A U3V device found on the bus. Never constructed on this backend.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FoundDevice {
    /// GenTL device ID: the U3V GUID when readable, else
    /// "VID:PID:Serial" from the instance path (notes/OPEN-QUESTIONS UQ2).
    pub id: String,
    /// Backend instance ID (used to open the device).
    pub instance_id: String,
    /// Instance ID of the root hub this device hangs off.
    pub root_id: String,
    pub vid: u16,
    pub pid: u16,
    pub openable: bool,
    pub identity: DeviceIdentity,
}

/// Result of one enumeration sweep.
#[derive(Debug, Default)]
pub struct Enumeration {
    pub devices: Vec<FoundDevice>,
    /// Candidates/entries dropped along the way. Counted, never fatal.
    pub skipped: usize,
}

/// Phase B (rusb backend): no USB host enumeration on this platform yet.
pub fn list_root_hubs() -> Vec<RootHubInfo> {
    Vec::new()
}

/// Phase B (rusb backend): the bus always reads empty.
pub fn enumerate_devices() -> Enumeration {
    Enumeration::default()
}
